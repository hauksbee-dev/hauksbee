//! Steady-state junction-temperature estimation (thermal monitor).
//!
//! The stress monitor already computes each device's live power dissipation and
//! compares it against its rated power. This module takes the next step: it
//! turns dissipation into a *junction temperature* and flags parts that run too
//! hot, using the textbook first-order steady-state model
//!
//! ```text
//!   Tj = Tambient + P_dissipated * theta_JA
//! ```
//!
//! where `theta_JA` (C/W) is the junction-to-ambient thermal resistance in
//! still air with no heatsink. It is the simplest model that gives a defensible
//! number: it assumes the part has reached thermal equilibrium (steady state),
//! that all dissipated power leaves through one lumped path to a fixed ambient,
//! and that neighbours do not heat each other. See the "limits" note at the
//! bottom and `docs/checks/THERMAL.md` for what this does and does not capture.
//!
//! ## theta_JA defaults by package class
//!
//! When a device's model entry does not carry an explicit `theta_ja_c_per_w`,
//! we derive one from its footprint package class. The defaults below are
//! representative still-air, single-layer/typical-board datasheet figures.
//! They are deliberately on the *pessimistic* (higher-resistance, hotter) side
//! within each package's published range, because over-estimating temperature
//! is the safe direction for a screening check and real boards rarely beat the
//! best-case JEDEC 2s2p numbers. Sources are the package thermal sections of
//! the parts that use each body, cross-checked against JEDEC JESD51 and the
//! Vishay / onsemi / TI / Nexperia package application notes:
//!
//! | Package          | theta_JA (C/W) | source / note                                   |
//! |------------------|----------------|-------------------------------------------------|
//! | SOT-23 / SOT-23-3| 250            | Nexperia/onsemi SOT-23 small-signal, still air  |
//! | SOT-23-5/6       | 220            | slightly more pin/copper than 3-pin             |
//! | SOT-223          | 65             | onsemi NCP1117 SOT-223, tab to 1 in^2 copper    |
//! | SOIC-8           | 120            | TI SOIC (D) 8-pin, JEDEC low-K                   |
//! | SOIC-14/16       | 90             | larger SOIC body                                |
//! | TSSOP-8/up       | 150            | TI PW package, JEDEC low-K                       |
//! | DPAK (TO-252)    | 70             | onsemi DPAK to 1 in^2 copper                     |
//! | D2PAK (TO-263)   | 50             | onsemi D2PAK to copper                           |
//! | TO-220           | 62             | onsemi TO-220 free-air (no heatsink)            |
//! | TO-92            | 200            | onsemi TO-92 still air                           |
//! | SOD-123 (diode)  | 340            | Vishay SOD-123 small-signal diode               |
//! | SOD-323/SOD-523  | 450            | smaller diode bodies run hotter                 |
//! | SMA (DO-214AC)   | 90             | Vishay SMA rectifier to recommended pad         |
//! | SMB (DO-214AA)   | 75             | Vishay SMB rectifier to recommended pad         |
//! | SMC (DO-214AB)   | 60             | Vishay SMC rectifier to recommended pad         |
//!
//! Chip resistors / capacitors use a size-derived theta_JA (a hot 0402 with no
//! copper flood is genuinely ~600 C/W; a 2512 nearer ~80 C/W). These are
//! consistent with the chip-resistor power deratings the stress monitor already
//! encodes (a 0402 at 1/16 W reaching its rated rise is the same physics).
//!
//! A part whose package is unrecognised falls back to a conservative
//! [`DEFAULT_THETA_JA`].

use hauksbee_models::schema::ComponentKind;

/// Default ambient temperature (C) when none is configured. 25 C is the
/// datasheet reference ambient ("TA = 25 C") most ratings are quoted at.
pub const DEFAULT_AMBIENT_C: f64 = 25.0;

/// theta_JA (C/W) used when the package is not recognised. A small SMD part in
/// still air with little copper; pessimistic on purpose.
pub const DEFAULT_THETA_JA: f64 = 200.0;

/// Maximum junction temperature (C) default for discretes / passives / small
/// signal parts (the common 125 C industrial limit).
pub const DEFAULT_TJ_MAX_C: f64 = 125.0;

/// Maximum junction temperature (C) default for power packages (TO-220, DPAK,
/// D2PAK, SOT-223, SMA/SMB/SMC), which are typically rated to 150 C.
pub const POWER_TJ_MAX_C: f64 = 150.0;

/// Derive a junction-to-ambient thermal resistance (C/W) from a footprint
/// string, by matching the package body token. Still-air, no heatsink. See the
/// module docs for the per-package sources.
pub fn theta_ja_from_footprint(footprint: &str, kind: ComponentKind) -> f64 {
    let f = footprint.to_ascii_uppercase();

    // Chip passives: size-derived (smaller body = worse cooling).
    if matches!(kind, ComponentKind::Passive) {
        if let Some(t) = chip_passive_theta_ja(&f) {
            return t;
        }
    }

    // Power / discrete / IC packages, matched by body token anywhere in the
    // footprint string (KiCad footprints embed the package name, e.g.
    // "Package_TO_SOT_SMD:SOT-23", "Package_TO_SOT_THT:TO-220-3_Vertical").
    // Order matters: check the more specific / larger bodies before substrings
    // that would also match a smaller one (TO-263 before TO-220, SOT-223 before
    // SOT-23, SOIC-16 before SOIC-8).
    if f.contains("TO-263") || f.contains("TO263") || f.contains("D2PAK") || f.contains("DDPAK") {
        return 50.0;
    }
    if f.contains("TO-252") || f.contains("TO252") || f.contains("DPAK") {
        return 70.0;
    }
    if f.contains("TO-220") || f.contains("TO220") {
        return 62.0;
    }
    if f.contains("TO-92") || f.contains("TO92") {
        return 200.0;
    }
    if f.contains("SOT-223") || f.contains("SOT223") {
        return 65.0;
    }
    if f.contains("SOT-89") || f.contains("SOT89") {
        return 140.0;
    }
    if f.contains("SOT-23-5")
        || f.contains("SOT-23-6")
        || f.contains("SOT23-5")
        || f.contains("SOT23-6")
        || f.contains("SOT-353")
        || f.contains("SOT-363")
    {
        return 220.0;
    }
    if f.contains("SOT-23") || f.contains("SOT23") {
        return 250.0;
    }
    // Diode-specific small bodies (run hot; check before generic SOD).
    if f.contains("SOD-523")
        || f.contains("SOD523")
        || f.contains("SOD-323")
        || f.contains("SOD323")
    {
        return 450.0;
    }
    if f.contains("SOD-123") || f.contains("SOD123") || f.contains("SOD-80") {
        return 340.0;
    }
    if f.contains("DO-214AB") || f.contains("SMC") {
        return 60.0;
    }
    if f.contains("DO-214AA") || f.contains("SMB") {
        return 75.0;
    }
    if f.contains("DO-214AC") || f.contains("SMA") {
        return 90.0;
    }
    if f.contains("DO-41") || f.contains("DO41") || f.contains("DO-201") || f.contains("DO-35") {
        // Through-hole axial rectifier / small-signal in free air.
        return 100.0;
    }
    // SOIC / SO bodies.
    if f.contains("SOIC-16") || f.contains("SOIC-14") || f.contains("SO-16") || f.contains("SO-14")
    {
        return 90.0;
    }
    if f.contains("SOIC-8") || f.contains("SOIC8") || f.contains("SO-8") || f.contains("SOIC_8") {
        return 120.0;
    }
    if f.contains("TSSOP") || f.contains("MSOP") {
        return 150.0;
    }
    if f.contains("QFN") || f.contains("DFN") {
        // Bottom thermal pad to copper; moderate.
        return 50.0;
    }
    if f.contains("LQFP") || f.contains("TQFP") || f.contains("QFP") {
        return 60.0;
    }

    DEFAULT_THETA_JA
}

/// Size-derived theta_JA for a chip resistor / capacitor footprint, by imperial
/// size token. `None` if the footprint carries no recognised chip size (so the
/// caller can fall through to a default). Values are still-air with minimal
/// copper; they bracket the power deratings the stress monitor already uses.
fn chip_passive_theta_ja(f_upper: &str) -> Option<f64> {
    // 01005 MUST be tested before 0402: KiCad pairs the imperial code with its
    // metric twin, and 01005's metric code IS 0402 ("R_01005_0402Metric"), so a
    // substring match on "0402" would give the smallest chip the 0402 theta_JA
    // (600), LOWER than 0201's 900, i.e. under-estimating Tj on the worst-
    // cooling body. Smaller body = hotter, so 01005 sits ABOVE 0201. Mirrors
    // resistor_power_from_footprint's "match the smallest package first" rule.
    let t = if f_upper.contains("01005") {
        1200.0
    } else if f_upper.contains("0201") {
        900.0
    } else if f_upper.contains("0402") {
        600.0
    } else if f_upper.contains("0603") {
        400.0
    } else if f_upper.contains("0805") {
        300.0
    } else if f_upper.contains("1206") {
        220.0
    } else if f_upper.contains("1210") {
        160.0
    } else if f_upper.contains("2010") {
        120.0
    } else if f_upper.contains("2512") {
        80.0
    } else {
        return None;
    };
    Some(t)
}

/// Default maximum junction temperature (C) for a package class. Power packages
/// (tab / leadframe parts that exist to dissipate) default to 150 C; everything
/// else to the common 125 C industrial limit.
pub fn default_tj_max(footprint: &str) -> f64 {
    let f = footprint.to_ascii_uppercase();
    let power = f.contains("TO-220")
        || f.contains("TO220")
        || f.contains("TO-263")
        || f.contains("TO263")
        || f.contains("D2PAK")
        || f.contains("TO-252")
        || f.contains("TO252")
        || f.contains("DPAK")
        || f.contains("SOT-223")
        || f.contains("SOT223")
        || f.contains("DO-214")
        || f.contains("SMA")
        || f.contains("SMB")
        || f.contains("SMC");
    if power {
        POWER_TJ_MAX_C
    } else {
        DEFAULT_TJ_MAX_C
    }
}

/// Steady-state junction temperature (C): `Tj = Tambient + P * theta_JA`.
pub fn junction_temp_c(ambient_c: f64, power_w: f64, theta_ja: f64) -> f64 {
    ambient_c + power_w.max(0.0) * theta_ja
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hand_checked_sot23_half_watt_reaches_150c() {
        // 0.5 W in a SOT-23 (theta_JA = 250 C/W) at 25 C ambient:
        // Tj = 25 + 0.5 * 250 = 150 C. The canonical hand-check.
        let theta = theta_ja_from_footprint("Package_TO_SOT_SMD:SOT-23", ComponentKind::BjtNpn);
        assert_eq!(theta, 250.0);
        let tj = junction_temp_c(25.0, 0.5, theta);
        assert!((tj - 150.0).abs() < 1e-9, "Tj = {tj}, expected 150");
    }

    #[test]
    fn to220_free_air_default() {
        let theta =
            theta_ja_from_footprint("Package_TO_SOT_THT:TO-220-3_Vertical", ComponentKind::Vreg);
        assert_eq!(theta, 62.0);
        // A 1 W TO-220 in 25 C air: 25 + 62 = 87 C, comfortably under its 150 C limit.
        let tj = junction_temp_c(25.0, 1.0, theta);
        assert!((tj - 87.0).abs() < 1e-9);
        assert_eq!(default_tj_max("...TO-220..."), 150.0);
    }

    #[test]
    fn chip_01005_not_mismatched_as_its_0402_metric_twin() {
        // R12: "R_01005_0402Metric" contains "0402"; the 01005 branch must win
        // (checked first), giving the smallest body the HIGHER theta_JA, never
        // the 0402's 600 (which would under-estimate Tj, the unsafe direction).
        let theta = theta_ja_from_footprint("Resistor_SMD:R_01005_0402Metric", ComponentKind::Passive);
        assert_ne!(theta, 600.0, "must not collide with the 0402 metric twin");
        assert!(theta > 900.0, "01005 sits above 0201's 900, got {theta}");
        // The 0201 whose metric twin is 0603 still resolves to 0201.
        assert_eq!(
            theta_ja_from_footprint("Resistor_SMD:R_0201_0603Metric", ComponentKind::Passive),
            900.0
        );
    }

    #[test]
    fn soic8_default() {
        assert_eq!(
            theta_ja_from_footprint("Package_SO:SOIC-8_3.9x4.9mm_P1.27mm", ComponentKind::Opamp),
            120.0
        );
    }

    #[test]
    fn sot223_before_sot23() {
        // SOT-223 must not be mis-matched as SOT-23.
        assert_eq!(
            theta_ja_from_footprint("Package_TO_SOT_SMD:SOT-223-3_TabPin2", ComponentKind::Vreg),
            65.0
        );
    }

    #[test]
    fn chip_resistor_sizes() {
        assert_eq!(
            theta_ja_from_footprint("Resistor_SMD:R_0402_1005Metric", ComponentKind::Passive),
            600.0
        );
        assert_eq!(
            theta_ja_from_footprint("Resistor_SMD:R_2512_6332Metric", ComponentKind::Passive),
            80.0
        );
    }

    #[test]
    fn unknown_package_falls_back() {
        assert_eq!(
            theta_ja_from_footprint("Some:Weird_Package", ComponentKind::BjtNpn),
            DEFAULT_THETA_JA
        );
        assert_eq!(default_tj_max("Some:Weird_Package"), DEFAULT_TJ_MAX_C);
    }
}
