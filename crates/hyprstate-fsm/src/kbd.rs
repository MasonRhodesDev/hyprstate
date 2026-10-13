//! Keyboard backlight policy (POWER_SPEC.md "Keyboard backlight"): the pure
//! rule table and the level arithmetic. The daemon senses the inputs and
//! writes the LED; nothing here does I/O.
//!
//! The LED is on only while two things are true at once: the room is dark
//! and the user is typing. It is off whenever the session is locked, the
//! machine is going to sleep, or input has gone idle. A manual key press
//! arms an override that keeps the keys lit while the user keeps typing
//! even in a lit room; the override releases when typing stops.

/// `#@ kbd-idle` default: seconds of no input before the keys go dark.
pub const DEFAULT_KBD_IDLE_SECS: u16 = 10;
/// Longest `#@ kbd-idle` accepted; anything longer is a typo.
pub const MAX_KBD_IDLE_SECS: u16 = 600;
/// `#@ kbd-dark-lux` default. PLACEHOLDER: the sensor on mason-air reads 1
/// to 2 in a dim room and the lit-room reading has not been taken yet; this
/// value is a guess that the measured one replaces.
pub const DEFAULT_KBD_DARK_LUX: u32 = 10;
/// Level used before the first manual key press writes one. The spec leaves
/// it open; half brightness is visible without being the maximum.
pub const DEFAULT_KBD_LEVEL_PCT: u8 = 50;
/// `kbd up` / `kbd down` move the level by this many percent.
pub const KBD_LEVEL_STEP: u8 = 10;

/// What the daemon knows when it decides whether the keys are lit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KbdInputs {
    /// logind LockedHint.
    pub locked: bool,
    /// PrepareForSleep(true) seen and no resume yet.
    pub sleeping: bool,
    /// Input seen within the last `kbd-idle` seconds (ext-idle-notify).
    pub typing: bool,
    /// The last sensor reading was at or below `kbd-dark-lux`.
    pub dark: bool,
    /// A manual key press armed the override and typing has not stopped.
    pub override_armed: bool,
}

/// The four rules, in order. Locked or sleeping: off. Not typing: off.
/// Typing and dark: on. Typing and not dark: off unless overridden.
pub fn kbd_lit(w: &KbdInputs) -> bool {
    if w.locked || w.sleeping {
        return false;
    }
    if !w.typing {
        return false;
    }
    w.dark || w.override_armed
}

/// Whether the room counts as dark for a sensor reading.
pub fn is_dark(lux: f64, dark_lux: u32) -> bool {
    lux <= dark_lux as f64
}

/// The override survives exactly as long as typing does: an idled event
/// releases it, nothing else does (short of `kbd auto`).
pub fn override_after_idle(armed: bool, idled: bool) -> bool {
    armed && !idled
}

/// Clamp a requested level to a percent.
pub fn clamp_level(pct: i64) -> u8 {
    pct.clamp(0, 100) as u8
}

/// One `kbd up` or `kbd down` step from `level`.
pub fn step_level(level: u8, up: bool) -> u8 {
    let delta = KBD_LEVEL_STEP as i64;
    clamp_level(level as i64 + if up { delta } else { -delta })
}

/// Raw LED value for a percent of `max`, rounded to nearest; a non-zero
/// percent never rounds down to an LED that is off.
pub fn level_to_raw(pct: u8, max: u32) -> u32 {
    if pct == 0 || max == 0 {
        return 0;
    }
    let raw = (max as u64 * pct as u64 + 50) / 100;
    (raw as u32).clamp(1, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(
        locked: bool,
        sleeping: bool,
        typing: bool,
        dark: bool,
        override_armed: bool,
    ) -> KbdInputs {
        KbdInputs {
            locked,
            sleeping,
            typing,
            dark,
            override_armed,
        }
    }

    /// The whole table: only typing-and-dark and typing-under-override light
    /// the keys, and lock or sleep beats both.
    #[test]
    fn rule_table() {
        // (locked, sleeping, typing, dark, override) -> lit
        let cases = [
            (false, false, true, true, false, true),
            (false, false, true, false, false, false),
            (false, false, true, false, true, true),
            (false, false, true, true, true, true),
            (false, false, false, true, false, false),
            (false, false, false, true, true, false),
            (true, false, true, true, false, false),
            (true, false, true, true, true, false),
            (false, true, true, true, false, false),
            (false, true, true, false, true, false),
        ];
        for (locked, sleeping, typing, dark, ov, lit) in cases {
            assert_eq!(
                kbd_lit(&w(locked, sleeping, typing, dark, ov)),
                lit,
                "locked={locked} sleeping={sleeping} typing={typing} dark={dark} override={ov}"
            );
        }
    }

    /// A manual press in a lit room keeps the keys on while typing continues
    /// and no longer once typing stops; the next typing in that room is off
    /// again until the next press.
    #[test]
    fn override_lives_exactly_as_long_as_typing() {
        let lit_room_typing = w(false, false, true, false, true);
        assert!(kbd_lit(&lit_room_typing));
        let armed = override_after_idle(true, true);
        assert!(!armed, "idled releases the override");
        assert!(!kbd_lit(&w(false, false, false, false, armed)));
        assert!(!kbd_lit(&w(false, false, true, false, armed)));
        assert!(override_after_idle(true, false), "typing keeps it");
        assert!(!override_after_idle(false, false));
    }

    #[test]
    fn dark_is_at_or_below_the_threshold() {
        assert!(is_dark(1.0, DEFAULT_KBD_DARK_LUX));
        assert!(is_dark(DEFAULT_KBD_DARK_LUX as f64, DEFAULT_KBD_DARK_LUX));
        assert!(!is_dark(
            DEFAULT_KBD_DARK_LUX as f64 + 0.5,
            DEFAULT_KBD_DARK_LUX
        ));
        assert!(is_dark(0.0, 0));
        assert!(!is_dark(0.5, 0));
    }

    #[test]
    fn level_steps_and_clamps() {
        assert_eq!(step_level(50, true), 60);
        assert_eq!(step_level(50, false), 40);
        assert_eq!(step_level(95, true), 100);
        assert_eq!(step_level(5, false), 0);
        assert_eq!(clamp_level(-20), 0);
        assert_eq!(clamp_level(250), 100);
    }

    /// Asahi's LED is 0..255: 50% is 128, 1% still lights the LED, 0 is off.
    #[test]
    fn level_to_raw_rounds_and_never_hides_a_nonzero_level() {
        assert_eq!(level_to_raw(50, 255), 128);
        assert_eq!(level_to_raw(100, 255), 255);
        assert_eq!(level_to_raw(1, 255), 3);
        assert_eq!(level_to_raw(1, 3), 1);
        assert_eq!(level_to_raw(0, 255), 0);
        assert_eq!(level_to_raw(50, 0), 0);
    }
}
