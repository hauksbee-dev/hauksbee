//! Package bodies read off a footprint or package name.
//!
//! The one table the thermal defaults, orderable-variant selection and a
//! card's per-package rating rows all read, so `SOT-223` is never a `SOT-23`
//! in one place and not in another.

/// A package body and what the thermal defaults take from it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Body {
    /// The body's first spelling in the table (`SOT-223`, `SOIC-8`).
    pub name: &'static str,
    /// The coarse family orderable-variant selection compares (`SOT` for
    /// `SOT-23-5` and `SOT-223`), where CAD libraries spell bodies differently.
    pub family: Option<&'static str>,
    /// Still-air junction-to-ambient thermal resistance (C/W), on the
    /// pessimistic side of the body's published range (sources in
    /// `hauksbee-bind`'s `thermal` module docs).
    pub theta_ja_c_per_w: f64,
    /// A tab or leadframe body that exists to dissipate, so its Tj,max defaults
    /// to 150 C rather than 125 C.
    pub power: bool,
}

/// `(spellings, family, theta_JA, power)`, matched on the name uppercased with
/// everything but letters and digits dropped (`SOT-23` and `SOT23` alike).
///
/// ORDER MATTERS: the first row one of whose spellings the name contains wins,
/// so larger or more specific bodies come before the substrings a smaller one
/// shares (TO-263 before TO-220, SOT-223 before SOT-23, TSSOP before SSOP).
/// Bodies sharing a row read as one, so a card's rows for either both apply
/// to it: the least favourable decides. A catch-all family row (`SOT`,
/// `SOIC`) follows its specific bodies and carries the unrecognised-package
/// default of 200 C/W. `DDPAK` is no power body, so its Tj,max default stays
/// the 125 C it always was.
const BODIES: &[(&[&str], &str, f64, bool)] = &[
    (&["TO-263", "D2PAK"], "", 50.0, true),
    (&["DDPAK"], "", 50.0, false),
    (&["TO-252", "DPAK"], "", 70.0, true),
    (&["TO-220"], "", 62.0, true),
    (&["TO-92"], "", 200.0, false),
    (&["SOT-223"], "SOT", 65.0, true),
    (&["SOT-89"], "SOT", 140.0, false),
    (&["SOT-23-5", "SOT-23-6"], "SOT", 220.0, false),
    (&["SOT-353", "SOT-363"], "SOT", 220.0, false),
    (&["SOT-23"], "SOT", 250.0, false),
    (&["SOT"], "SOT", 200.0, false),
    (&["SOD-523", "SOD-323"], "", 450.0, false),
    (&["SOD-123", "SOD-80"], "", 340.0, false),
    (&["DO-214AB", "SMC"], "", 60.0, true),
    (&["DO-214AA", "SMB"], "", 75.0, true),
    (&["DO-214AC", "SMA"], "", 90.0, true),
    (&["DO-41", "DO-201", "DO-35"], "", 100.0, false),
    (&["SOIC-16", "SO-16"], "SOIC", 90.0, false),
    (&["SOIC-14", "SO-14"], "SOIC", 90.0, false),
    (&["SOIC-8", "SO-8"], "SOIC", 120.0, false),
    (&["SOIC"], "SOIC", 200.0, false),
    (&["TSSOP"], "TSSOP", 150.0, false),
    (&["MSOP"], "", 150.0, false),
    (&["SSOP"], "SSOP", 200.0, false),
    (&["QFN"], "QFN", 50.0, false),
    (&["DFN"], "DFN", 50.0, false),
    (&["TQFP"], "TQFP", 60.0, false),
    (&["LQFP"], "LQFP", 60.0, false),
    (&["QFP"], "", 60.0, false),
    (&["BGA"], "BGA", 200.0, false),
    (&["DIP"], "DIP", 200.0, false),
];

fn squash(text: &str) -> String {
    let kept = text.chars().filter(char::is_ascii_alphanumeric);
    kept.map(|c| c.to_ascii_uppercase()).collect()
}

/// The package body a footprint or package name spells, if the table knows it.
#[must_use]
pub fn package_body(name: &str) -> Option<Body> {
    let name = squash(name);
    let row = (BODIES.iter()).find(|r| r.0.iter().any(|s| name.contains(&squash(s))));
    row.map(|&(names, family, theta_ja_c_per_w, power)| Body {
        name: names[0],
        family: Some(family).filter(|f| !f.is_empty()),
        theta_ja_c_per_w,
        power,
    })
}
