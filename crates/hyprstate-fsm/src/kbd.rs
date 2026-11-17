pub const DEFAULT_KBD_IDLE_SECS: u16 = 10;
pub const MAX_KBD_IDLE_SECS: u16 = 600;
pub const DEFAULT_KBD_DARK_LUX: u32 = 10;
pub const DEFAULT_KBD_LEVEL_PCT: u8 = 50;
pub const KBD_LEVEL_STEP: u8 = 10;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KbdInputs {
    pub locked: bool,
    pub sleeping: bool,
    pub typing: bool,
    pub dark: bool,
    pub override_armed: bool,
}

pub fn kbd_lit(w: &KbdInputs) -> bool {
    if w.locked || w.sleeping {
        return false;
    }
    if !w.typing {
        return false;
    }
    w.dark || w.override_armed
}

pub fn is_dark(lux: f64, dark_lux: u32) -> bool {
    lux <= dark_lux as f64
}

pub fn override_after_idle(armed: bool, idled: bool) -> bool {
    armed && !idled
}

pub fn clamp_level(pct: i64) -> u8 {
    pct.clamp(0, 100) as u8
}

pub fn step_level(level: u8, up: bool) -> u8 {
    let delta = KBD_LEVEL_STEP as i64;
    clamp_level(level as i64 + if up { delta } else { -delta })
}

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

    #[test]
    fn rule_table() {
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
