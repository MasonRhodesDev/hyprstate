//! GPU-primary selection: pure classification and selection over a topology
//! snapshot (see GPU_SPEC.md). The io layer builds `GpuSnapshot` from sysfs;
//! everything here is a pure function of it.

/// One GPU candidate from /dev/dri/by-path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuCard {
    /// Stable /dev/dri/by-path entry (selection key).
    pub path: String,
    /// Resolved cardN (NOT stable across boots; emitted, never persisted).
    pub card: String,
    pub boot_vga: u32,
    pub vram: u64,
    /// Connected non-eDP connectors.
    pub external: u32,
    /// Connected eDP connectors.
    pub edp: u32,
}

#[derive(Debug, Clone, Default)]
pub struct GpuSnapshot {
    pub cards: Vec<GpuCard>,
    /// A non-candidate DRM device (DisplayLink/platform) has a connected
    /// output — selection must bail to open-all-GPUs behavior.
    pub non_pci_display: bool,
    pub lid_closed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuMode {
    Auto,
    Igpu,
    Dgpu,
    Off,
}

impl GpuMode {
    pub fn as_str(self) -> &'static str {
        match self {
            GpuMode::Auto => "auto",
            GpuMode::Igpu => "igpu",
            GpuMode::Dgpu => "dgpu",
            GpuMode::Off => "off",
        }
    }
}

/// Where the mode came from — feeds the `reason` strings (`override-igpu`,
/// `profile-dgpu`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuModeSource {
    Override,
    Profile,
    Platform,
    Default,
}

impl GpuModeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            GpuModeSource::Override => "override",
            GpuModeSource::Profile => "profile",
            GpuModeSource::Platform => "platform",
            GpuModeSource::Default => "default",
        }
    }
}

/// Mode precedence: override file > profile preference > platform_profile >
/// auto. Inputs are the first words of the respective sources (None =
/// missing). `overlay` is the daemon's freshly-selected profile `#@ gpu`
/// value, or the breadcrumb file word at CLI/select time — same fall-through
/// semantics either way ("auto"/unknown falls through).
pub fn resolve_gpu_mode(
    override_word: Option<&str>,
    overlay: Option<&str>,
    platform_word: Option<&str>,
) -> (GpuMode, GpuModeSource, Vec<String>) {
    let mut warnings = Vec::new();
    if let Some(word) = override_word {
        match word {
            "igpu" => return (GpuMode::Igpu, GpuModeSource::Override, warnings),
            "dgpu" => return (GpuMode::Dgpu, GpuModeSource::Override, warnings),
            "off" => return (GpuMode::Off, GpuModeSource::Override, warnings),
            "auto" => {}
            other => warnings.push(format!("ignoring unknown mode {other:?}")),
        }
    }
    match overlay {
        Some("igpu") => return (GpuMode::Igpu, GpuModeSource::Profile, warnings),
        Some("dgpu") => return (GpuMode::Dgpu, GpuModeSource::Profile, warnings),
        _ => {}
    }
    match platform_word {
        Some("low-power") | Some("quiet") => (GpuMode::Igpu, GpuModeSource::Platform, warnings),
        Some("performance") => (GpuMode::Dgpu, GpuModeSource::Platform, warnings),
        // balanced / balanced-performance / cool / custom / missing /
        // unknown: deliberately auto (exhaustive against the kernel ABI).
        _ => (GpuMode::Auto, GpuModeSource::Default, warnings),
    }
}

/// Integrated = boot_vga AND smallest-VRAM agreeing on the same card.
/// Disagreement (e.g. a muxed laptop reporting boot_vga on the discrete) or
/// a VRAM tie without boot_vga -> None -> unmanaged. Cards lacking
/// mem_info_vram_total (Intel, nouveau) read as 0, which agrees trivially.
/// Returns an index into `cards`; caller guarantees len >= 2.
pub fn integrated_card(cards: &[GpuCard]) -> Option<usize> {
    let min_vram = cards.iter().map(|c| c.vram).min()?;
    let by_vram: Vec<usize> = (0..cards.len())
        .filter(|&i| cards[i].vram == min_vram)
        .collect();
    let by_vga: Vec<usize> = (0..cards.len())
        .filter(|&i| cards[i].boot_vga == 1)
        .collect();
    if by_vga.len() == 1 {
        return by_vram.contains(&by_vga[0]).then(|| by_vga[0]);
    }
    if by_vga.is_empty() && by_vram.len() == 1 {
        return Some(by_vram[0]);
    }
    None
}

/// Colon-free device node for AQ_DRM_DEVICES. We SELECT by the stable PCI
/// by-path but EMIT the resolved cardN node: AQ_DRM_DEVICES is
/// colon-separated and PCI by-path names contain colons, so aquamarine
/// would shatter a by-path value on every ':'.
pub fn devnode(card: &GpuCard) -> String {
    format!("/dev/dri/{}", card.card)
}

/// Pure: (device list primary-first, reason) or (None, reason) = unmanaged
/// (caller prints nothing; Hyprland falls back to its own defaults).
pub fn gpu_desired(
    snap: &GpuSnapshot,
    mode: GpuMode,
    source: GpuModeSource,
) -> (Option<Vec<String>>, String) {
    if mode == GpuMode::Off {
        return (None, "override-off".into());
    }
    if snap.cards.len() < 2 {
        return (None, "no-multi-gpu".into());
    }
    if snap.non_pci_display {
        return (None, "non-pci-display-present".into());
    }
    let Some(integrated) = integrated_card(&snap.cards) else {
        return (None, "ambiguous-integrated".into());
    };
    if snap.lid_closed && !snap.cards.iter().any(|c| c.external > 0) {
        // Docked cold boot: DP links can still be down at early-login sysfs
        // read. Omitting the dock's GPU here would leave a lid-closed
        // session with no usable output (and the lid FSM would
        // suspend-loop). The caller does one settle retry; persistent ->
        // unmanaged.
        return (None, "bailed-transient".into());
    }

    let mut discretes: Vec<usize> = (0..snap.cards.len()).filter(|&i| i != integrated).collect();
    discretes.sort_by(|&a, &b| {
        let (ca, cb) = (&snap.cards[a], &snap.cards[b]);
        (
            std::cmp::Reverse(ca.external),
            std::cmp::Reverse(ca.vram),
            &ca.path,
        )
            .cmp(&(
                std::cmp::Reverse(cb.external),
                std::cmp::Reverse(cb.vram),
                &cb.path,
            ))
    });
    let best = discretes[0];

    let (primary, reason): (usize, String) = match mode {
        GpuMode::Auto => {
            if snap.cards[best].external > 0 || snap.cards[best].edp > 0 {
                (best, "dgpu-has-display".into())
            } else {
                (integrated, "dgpu-idle-omitted".into())
            }
        }
        GpuMode::Igpu => (integrated, format!("{}-igpu", source.as_str())),
        GpuMode::Dgpu => (best, format!("{}-dgpu", source.as_str())),
        GpuMode::Off => unreachable!(),
    };

    let mut devices = vec![primary];
    if integrated != primary {
        devices.push(integrated); // integrated always listed (eDP/hotplug)
    }
    for &i in &discretes {
        if i == primary {
            continue;
        }
        if snap.cards[i].external > 0 || snap.cards[i].edp > 0 {
            devices.push(i); // display-less discretes omitted -> runtime PM
        }
    }

    // Usable-output invariant: never emit a list under which nothing can
    // light up — a connected external on a listed card, or eDP + lid open.
    let usable = devices.iter().any(|&i| snap.cards[i].external > 0)
        || (!snap.lid_closed && devices.iter().any(|&i| snap.cards[i].edp > 0));
    if !usable {
        return (None, "bailed-transient".into());
    }

    (
        Some(devices.iter().map(|&i| devnode(&snap.cards[i])).collect()),
        reason,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(
        path: &str,
        card_n: &str,
        boot_vga: u32,
        vram: u64,
        external: u32,
        edp: u32,
    ) -> GpuCard {
        GpuCard {
            path: path.to_string(),
            card: card_n.to_string(),
            boot_vga,
            vram,
            external,
            edp,
        }
    }

    /// Framework-16-shaped snapshot: iGPU has boot_vga + small VRAM.
    fn fw16(igpu_edp: u32, dgpu_external: u32) -> Vec<GpuCard> {
        vec![
            card(
                "/dev/dri/by-path/pci-0000:03:00.0-card",
                "card1",
                0,
                8 << 30,
                dgpu_external,
                0,
            ),
            card(
                "/dev/dri/by-path/pci-0000:c4:00.0-card",
                "card2",
                1,
                512 << 20,
                0,
                igpu_edp,
            ),
        ]
    }

    fn snap(cards: Vec<GpuCard>, non_pci: bool, lid_closed: bool) -> GpuSnapshot {
        GpuSnapshot {
            cards,
            non_pci_display: non_pci,
            lid_closed,
        }
    }

    #[test]
    fn test_integrated_by_agreement() {
        assert_eq!(integrated_card(&fw16(1, 0)), Some(1));
    }

    #[test]
    fn test_integrated_disagreement_is_none() {
        // boot_vga on the big-VRAM card (muxed laptop): signals disagree.
        let cards = vec![
            card("a", "card0", 1, 8 << 30, 0, 0),
            card("b", "card1", 0, 512 << 20, 0, 0),
        ];
        assert_eq!(integrated_card(&cards), None);
    }

    #[test]
    fn test_integrated_vram_only_when_no_boot_vga() {
        let cards = vec![
            card("a", "card0", 0, 8 << 30, 0, 0),
            card("b", "card1", 0, 1 << 20, 0, 0),
        ];
        assert_eq!(integrated_card(&cards), Some(1));
    }

    #[test]
    fn test_integrated_vram_tie_without_boot_vga_is_none() {
        // Both vram=0 (Intel-style missing mem_info_vram_total).
        let cards = vec![
            card("a", "card0", 0, 0, 0, 0),
            card("b", "card1", 0, 0, 0, 0),
        ];
        assert_eq!(integrated_card(&cards), None);
    }

    #[test]
    fn test_gpu_unmanaged_cases() {
        let (d, r) = gpu_desired(
            &snap(fw16(1, 0), false, false),
            GpuMode::Off,
            GpuModeSource::Override,
        );
        assert_eq!((d, r.as_str()), (None, "override-off"));

        let one_card = fw16(1, 0)[..1].to_vec();
        let (d, r) = gpu_desired(
            &snap(one_card, false, false),
            GpuMode::Auto,
            GpuModeSource::Default,
        );
        assert_eq!((d, r.as_str()), (None, "no-multi-gpu"));

        let (d, r) = gpu_desired(
            &snap(fw16(1, 0), true, false),
            GpuMode::Auto,
            GpuModeSource::Default,
        );
        assert_eq!((d, r.as_str()), (None, "non-pci-display-present"));

        let ambiguous = vec![
            card("a", "card0", 1, 8 << 30, 0, 0),
            card("b", "card1", 0, 1 << 20, 0, 0),
        ];
        let (d, r) = gpu_desired(
            &snap(ambiguous, false, false),
            GpuMode::Auto,
            GpuModeSource::Default,
        );
        assert_eq!((d, r.as_str()), (None, "ambiguous-integrated"));
    }

    #[test]
    fn test_gpu_auto_dgpu_with_display_is_primary() {
        let (devices, reason) = gpu_desired(
            &snap(fw16(1, 1), false, false),
            GpuMode::Auto,
            GpuModeSource::Default,
        );
        assert_eq!(devices.unwrap(), vec!["/dev/dri/card1", "/dev/dri/card2"]);
        assert_eq!(reason, "dgpu-has-display");
    }

    #[test]
    fn test_gpu_auto_idle_dgpu_is_omitted() {
        let (devices, reason) = gpu_desired(
            &snap(fw16(1, 0), false, false),
            GpuMode::Auto,
            GpuModeSource::Default,
        );
        // iGPU only; dGPU omitted -> runtime PM suspends it.
        assert_eq!(devices.unwrap(), vec!["/dev/dri/card2"]);
        assert_eq!(reason, "dgpu-idle-omitted");
    }

    #[test]
    fn test_gpu_igpu_mode_lists_dgpu_only_with_display() {
        let (devices, reason) = gpu_desired(
            &snap(fw16(1, 1), false, false),
            GpuMode::Igpu,
            GpuModeSource::Override,
        );
        assert_eq!(devices.unwrap(), vec!["/dev/dri/card2", "/dev/dri/card1"]);
        assert_eq!(reason, "override-igpu");

        let (devices, _) = gpu_desired(
            &snap(fw16(1, 0), false, false),
            GpuMode::Igpu,
            GpuModeSource::Override,
        );
        assert_eq!(devices.unwrap(), vec!["/dev/dri/card2"]);
    }

    #[test]
    fn test_gpu_dgpu_mode_always_lists_dgpu() {
        let (devices, reason) = gpu_desired(
            &snap(fw16(1, 0), false, false),
            GpuMode::Dgpu,
            GpuModeSource::Profile,
        );
        assert_eq!(devices.unwrap(), vec!["/dev/dri/card1", "/dev/dri/card2"]);
        assert_eq!(reason, "profile-dgpu");
    }

    #[test]
    fn test_gpu_lid_closed_no_externals_bails_transient() {
        let (devices, reason) = gpu_desired(
            &snap(fw16(1, 0), false, true),
            GpuMode::Auto,
            GpuModeSource::Default,
        );
        assert_eq!(devices, None);
        assert_eq!(reason, "bailed-transient");
    }

    /// Lid open but eDP not connected and no externals anywhere: any list we
    /// could emit lights nothing up -> must bail, never print.
    #[test]
    fn test_gpu_usable_output_invariant() {
        let (devices, reason) = gpu_desired(
            &snap(fw16(0, 0), false, false),
            GpuMode::Auto,
            GpuModeSource::Default,
        );
        assert_eq!(devices, None);
        assert_eq!(reason, "bailed-transient");
    }

    #[test]
    fn test_resolve_gpu_mode_precedence() {
        use GpuMode::*;
        use GpuModeSource::*;
        // Override wins over everything.
        let (m, s, w) = resolve_gpu_mode(Some("dgpu"), Some("igpu"), Some("low-power"));
        assert_eq!((m, s), (Dgpu, Override));
        assert!(w.is_empty());
        // Override "auto" falls through silently; unknown warns + falls through.
        let (m, s, w) = resolve_gpu_mode(Some("auto"), Some("igpu"), None);
        assert_eq!((m, s), (Igpu, Profile));
        assert!(w.is_empty());
        let (m, s, w) = resolve_gpu_mode(Some("sideways"), None, Some("performance"));
        assert_eq!((m, s), (Dgpu, Platform));
        assert_eq!(w.len(), 1);
        // Overlay "auto"/missing falls through to platform_profile.
        let (m, s, _) = resolve_gpu_mode(None, Some("auto"), Some("quiet"));
        assert_eq!((m, s), (Igpu, Platform));
        // balanced and friends are deliberately auto.
        for word in ["balanced", "balanced-performance", "cool", "custom", "???"] {
            let (m, s, _) = resolve_gpu_mode(None, None, Some(word));
            assert_eq!((m, s), (Auto, Default));
        }
        let (m, s, _) = resolve_gpu_mode(None, None, None);
        assert_eq!((m, s), (Auto, Default));
    }
}
