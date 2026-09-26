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
//! ## `theta_JA` defaults by package class
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
//! | Package          | `theta_JA` (C/W) | source / note                                   |
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
//! Chip resistors / capacitors use a size-derived `theta_JA` (a hot 0402 with no
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

/// `theta_JA` (C/W) used when the package is not recognised. A small SMD part in
/// still air with little copper; pessimistic on purpose.
pub const DEFAULT_THETA_JA: f64 = 200.0;

/// Maximum junction temperature (C) default for discretes / passives / small
/// signal parts (the common 125 C industrial limit).
pub const DEFAULT_TJ_MAX_C: f64 = 125.0;

/// Maximum junction temperature (C) default for power packages (TO-220, DPAK,
/// D2PAK, SOT-223, SMA/SMB/SMC), which are typically rated to 150 C.
pub const POWER_TJ_MAX_C: f64 = 150.0;

/// Power / discrete / IC package body tokens, matched anywhere in an uppercased
/// footprint string (`KiCad` footprints embed the package name, e.g.
/// "Package_TO_SOT_SMD:SOT-23", "Package_TO_SOT_THT:TO-220-3_Vertical"), each
/// with its still-air `theta_JA` (C/W).
///
/// ORDER MATTERS: the first row whose token is present wins, so the more
/// specific / larger bodies come before the substrings that would also match a
/// smaller one (TO-263 before TO-220, SOT-223 before SOT-23, the small diode
/// bodies before the generic SOD ones, SOIC-16 before SOIC-8).
const PACKAGE_THETA_JA: &[(&[&str], f64)] = &[
    (&["TO-263", "TO263", "D2PAK", "DDPAK"], 50.0),
    (&["TO-252", "TO252", "DPAK"], 70.0),
    (&["TO-220", "TO220"], 62.0),
    (&["TO-92", "TO92"], 200.0),
    (&["SOT-223", "SOT223"], 65.0),
    (&["SOT-89", "SOT89"], 140.0),
    (&["SOT-23-5", "SOT-23-6", "SOT23-5", "SOT23-6"], 220.0),
    (&["SOT-353", "SOT-363"], 220.0),
    (&["SOT-23", "SOT23"], 250.0),
    (&["SOD-523", "SOD523", "SOD-323", "SOD323"], 450.0),
    (&["SOD-123", "SOD123", "SOD-80"], 340.0),
    (&["DO-214AB", "SMC"], 60.0),
    (&["DO-214AA", "SMB"], 75.0),
    (&["DO-214AC", "SMA"], 90.0),
    // Through-hole axial rectifier / small-signal in free air.
    (&["DO-41", "DO41", "DO-201", "DO-35"], 100.0),
    (&["SOIC-16", "SOIC-14", "SO-16", "SO-14"], 90.0),
    (&["SOIC-8", "SOIC8", "SO-8", "SOIC_8"], 120.0),
    (&["TSSOP", "MSOP"], 150.0),
    // Bottom thermal pad to copper; moderate.
    (&["QFN", "DFN"], 50.0),
    (&["LQFP", "TQFP", "QFP"], 60.0),
];

/// Size-derived `theta_JA` for a chip resistor / capacitor footprint, by imperial
/// size token: still-air with minimal copper, bracketing the power deratings the
/// stress monitor already encodes.
///
/// 01005 MUST be tested before 0402: `KiCad` pairs the imperial code with its
/// metric twin, and 01005's metric code IS 0402 ("`R_01005_0402Metric`"), so a
/// substring match on "0402" would give the smallest chip the 0402 `theta_JA`
/// (600), LOWER than 0201's 900, i.e. under-estimating Tj on the worst-cooling
/// body. Smaller body = hotter, so 01005 sits ABOVE 0201, matching
/// `resistor_power_from_footprint`'s "match the smallest package first" rule.
const CHIP_THETA_JA: &[(&str, f64)] = &[
    ("01005", 1200.0),
    ("0201", 900.0),
    ("0402", 600.0),
    ("0603", 400.0),
    ("0805", 300.0),
    ("1206", 220.0),
    ("1210", 160.0),
    ("2010", 120.0),
    ("2512", 80.0),
];

/// Package bodies that exist to dissipate, so default to [`POWER_TJ_MAX_C`].
const POWER_BODIES: &[&str] = &[
    "TO-220", "TO220", "TO-263", "TO263", "D2PAK", "TO-252", "TO252", "DPAK", "SOT-223", "SOT223",
    "DO-214", "SMA", "SMB", "SMC",
];

/// The first row of `table` one of whose tokens `f` contains.
fn body_match<T: Copy>(table: &[(T, f64)], f: &str, has: impl Fn(T, &str) -> bool) -> Option<f64> {
    table.iter().find(|(k, _)| has(*k, f)).map(|(_, v)| *v)
}

/// Derive a junction-to-ambient thermal resistance (C/W) from a footprint
/// string, by matching the package body token. Still-air, no heatsink. See the
/// module docs for the per-package sources.
#[must_use]
pub fn theta_ja_from_footprint(footprint: &str, kind: ComponentKind) -> f64 {
    let f = footprint.to_ascii_uppercase();
    // Chip passives: size-derived (smaller body = worse cooling).
    if matches!(kind, ComponentKind::Passive) {
        if let Some(t) = chip_passive_theta_ja(&f) {
            return t;
        }
    }
    let any = |tokens: &[&str], f: &str| tokens.iter().any(|t| f.contains(t));
    body_match(PACKAGE_THETA_JA, &f, any).unwrap_or(DEFAULT_THETA_JA)
}

/// Size-derived `theta_JA` for a chip resistor / capacitor footprint, from
/// [`CHIP_THETA_JA`]. `None` if the footprint carries no recognised chip size
/// (so the caller can fall through to a default).
fn chip_passive_theta_ja(f_upper: &str) -> Option<f64> {
    body_match(CHIP_THETA_JA, f_upper, |t, f| f.contains(t))
}

/// Default maximum junction temperature (C) for a package class. Power packages
/// (tab / leadframe parts that exist to dissipate) default to 150 C; everything
/// else to the common 125 C industrial limit.
#[must_use]
pub fn default_tj_max(footprint: &str) -> f64 {
    let f = footprint.to_ascii_uppercase();
    if POWER_BODIES.iter().any(|b| f.contains(b)) {
        POWER_TJ_MAX_C
    } else {
        DEFAULT_TJ_MAX_C
    }
}

/// Steady-state junction temperature (C): `Tj = Tambient + P * theta_JA`.
#[must_use]
pub fn junction_temp_c(ambient_c: f64, power_w: f64, theta_ja: f64) -> f64 {
    power_w.max(0.0).mul_add(theta_ja, ambient_c)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Table lookups return the tabulated constant exactly, so compare bits.
    fn theta_bits(footprint: &str, kind: ComponentKind) -> u64 {
        theta_ja_from_footprint(footprint, kind).to_bits()
    }

    #[test]
    fn hand_checked_sot23_half_watt_reaches_150c() {
        // 0.5 W in a SOT-23 (theta_JA = 250 C/W) at 25 C ambient:
        // Tj = 25 + 0.5 * 250 = 150 C. The canonical hand-check.
        let theta = theta_ja_from_footprint("Package_TO_SOT_SMD:SOT-23", ComponentKind::BjtNpn);
        assert!((theta - 250.0).abs() < 1e-12);
        let tj = junction_temp_c(25.0, 0.5, theta);
        assert!((tj - 150.0).abs() < 1e-9, "Tj = {tj}, expected 150");
    }

    #[test]
    fn to220_free_air_default() {
        let theta =
            theta_ja_from_footprint("Package_TO_SOT_THT:TO-220-3_Vertical", ComponentKind::Vreg);
        assert!((theta - 62.0).abs() < 1e-12);
        // A 1 W TO-220 in 25 C air: 25 + 62 = 87 C, comfortably under its 150 C limit.
        let tj = junction_temp_c(25.0, 1.0, theta);
        assert!((tj - 87.0).abs() < 1e-9);
        assert!((default_tj_max("...TO-220...") - 150.0).abs() < 1e-12);
    }

    #[test]
    fn chip_01005_not_mismatched_as_its_0402_metric_twin() {
        // "R_01005_0402Metric" contains "0402"; the 01005 branch must win
        // (checked first), giving the smallest body the HIGHER theta_JA, never
        // the 0402's 600 (which would under-estimate Tj, the unsafe direction).
        let theta =
            theta_ja_from_footprint("Resistor_SMD:R_01005_0402Metric", ComponentKind::Passive);
        assert!(theta > 900.0, "01005 sits above 0201's 900, got {theta}");
        // The 0201 whose metric twin is 0603 still resolves to 0201.
        assert_eq!(
            theta_bits("Resistor_SMD:R_0201_0603Metric", ComponentKind::Passive),
            900f64.to_bits()
        );
    }

    #[test]
    fn soic8_default() {
        assert_eq!(
            theta_bits("Package_SO:SOIC-8_3.9x4.9mm_P1.27mm", ComponentKind::Opamp),
            120f64.to_bits()
        );
    }

    #[test]
    fn sot223_before_sot23() {
        // SOT-223 must not be mis-matched as SOT-23.
        assert_eq!(
            theta_bits("Package_TO_SOT_SMD:SOT-223-3_TabPin2", ComponentKind::Vreg),
            65f64.to_bits()
        );
    }

    #[test]
    fn chip_resistor_sizes() {
        assert_eq!(
            theta_bits("Resistor_SMD:R_0402_1005Metric", ComponentKind::Passive),
            600f64.to_bits()
        );
        assert_eq!(
            theta_bits("Resistor_SMD:R_2512_6332Metric", ComponentKind::Passive),
            80f64.to_bits()
        );
    }

    #[test]
    fn unknown_package_falls_back() {
        assert_eq!(
            theta_bits("Some:Weird_Package", ComponentKind::BjtNpn),
            DEFAULT_THETA_JA.to_bits()
        );
        let tj_max = default_tj_max("Some:Weird_Package");
        assert_eq!(tj_max.to_bits(), DEFAULT_TJ_MAX_C.to_bits());
    }
}
