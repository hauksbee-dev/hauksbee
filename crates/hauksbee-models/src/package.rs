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
    /// The name spells an exposed pad or a thermally enhanced leadframe
    /// (`-1EP`, `PowerPAD`, `PowerPAK SO-8`, `HTSSOP`, `SO-8FL`).
    pub exposed_pad: bool,
}

impl Body {
    /// Whether a card row printed for `self` rates a part on `footprint`: one
    /// specific body (a catch-all `SOT` or `SOIC` names none), and an
    /// exposed-pad row only on a footprint with the pad.
    #[must_use]
    pub fn rates(self, footprint: Self) -> bool {
        let specific = !matches!(self.name, "SOT" | "SOIC");
        specific && self.name == footprint.name && (!self.exposed_pad || footprint.exposed_pad)
    }
}

/// `(spellings, family, theta_JA, power)`, matched on the name uppercased with
/// everything but letters and digits dropped (`SOT-23` and `SOT23` alike).
///
/// ORDER MATTERS: the first row one of whose spellings the name contains wins,
/// so larger or more specific bodies come before the substrings a smaller one
/// shares (TO-263 and TO-220F before TO-220, SOT-223 before SOT-23, TSSOP before SSOP).
/// Bodies sharing a row read as one, so a card's rows for either both apply
/// to it: the least favourable decides. A catch-all family row (`SOT`,
/// `SOIC`) follows its specific bodies and carries the unrecognised-package
/// default of 200 C/W. `DDPAK` is no power body, so its Tj,max default is
/// 125 C, below the 150 C its old substring match on `DPAK` gave.
const BODIES: &[(&[&str], &str, f64, bool)] = &[
    (&["TO-263", "D2PAK"], "", 50.0, true),
    (&["DDPAK"], "", 50.0, false),
    (&["TO-252", "DPAK"], "", 70.0, true),
    (&["TO-220F", "ITO-220"], "", 62.0, true),
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

static EXPOSED_PAD: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    let pad = r"(?i)(?:^|[^A-Z])EP(?:[^A-Z]|$)|EXPOSED|POWER ?PA[DK]|POWERSO|THERMAL ?PAD";
    let enhanced = r"(?:^|[^A-Z])H(?:T?SS?OP|SOIC|VSSOP)|SO-?8 ?FL";
    regex::Regex::new(&format!("{pad}|{enhanced}")).expect("a valid pattern")
});

/// The package body a footprint or package name spells, if the table knows it.
/// A `KiCad` library prefix (`Package_DFN_QFN:`) is no part of the body.
#[must_use]
pub fn package_body(name: &str) -> Option<Body> {
    let squashed = squash(name.rsplit(':').next().unwrap_or(name));
    let row = (BODIES.iter()).find(|r| r.0.iter().any(|s| squashed.contains(&squash(s))));
    row.map(|&(names, family, theta_ja_c_per_w, power)| Body {
        name: names[0],
        family: Some(family).filter(|f| !f.is_empty()),
        theta_ja_c_per_w,
        power,
        exposed_pad: EXPOSED_PAD.is_match(name),
    })
}

/// The outline a package or footprint name spells: two names with different
/// outlines never share a pin map, whatever their pin counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outline {
    /// QFN, UFQFPN, VQFN, MLF, MLP, LFCSP.
    QuadLeadless,
    /// QFP, LQFP, TQFP.
    QuadLeaded,
    /// DFN, SON, WSON.
    DualLeadless,
    /// SOIC, SO, SOP, SSOP, TSSOP, MSOP.
    SmallOutline,
    /// SOT, TSOT, SC-70.
    Sot,
    Dip,
    /// BGA, WLCSP, LGA.
    Grid,
}

/// `(spellings, outline)`, first match per name token wins: a leadless quad
/// spelling before the `QFP` it contains (`UFQFPN`), `SOT` before `SO`.
const OUTLINES: &[(&[&str], Outline)] = &[
    (
        &["LFCSP", "QFN", "QFPN", "MLF", "MLP"],
        Outline::QuadLeadless,
    ),
    (&["BGA", "CSP", "LGA"], Outline::Grid),
    (&["QFP"], Outline::QuadLeaded),
    (&["DFN", "SON"], Outline::DualLeadless),
    (&["SOT"], Outline::Sot),
    (&["SOIC", "SOP", "SO"], Outline::SmallOutline),
    (&["DIP"], Outline::Dip),
];

/// The name less a `KiCad`/Eagle library prefix, as alphanumeric tokens with
/// `SC-70` kept whole.
fn tokens(name: &str) -> Vec<String> {
    let bare = name.rsplit(':').next().unwrap_or(name).to_ascii_uppercase();
    let bare = bare.replace("SC-", "SC");
    bare.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The one outline a package or footprint name spells; `None` when it spells
/// none or two (`LQFP48/UFQFPN48`, a column printed for both).
#[must_use]
pub fn outline(name: &str) -> Option<Outline> {
    let of = |token: &String| {
        let letters: String = token
            .chars()
            .take_while(char::is_ascii_alphabetic)
            .collect();
        let hit = |s: &&str| letters.contains(s) && (*s != "SO" || letters.ends_with("SO"));
        let sc = sc_body(token)
            .is_some_and(|body| matches!(body, 70 | 88))
            .then_some(Outline::Sot);
        sc.or_else(|| {
            OUTLINES
                .iter()
                .find(|(names, _)| names.iter().any(hit))
                .map(|r| r.1)
        })
    };
    let mut found: Vec<Outline> = tokens(name).iter().filter_map(of).collect();
    found.dedup();
    match found[..] {
        [one] => Some(one),
        _ => None,
    }
}

static IPC: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"^[A-Z]+\d+P\d+X\d+.*-(\d+)[A-Z]?$").expect("a valid pattern")
});
static COUNTED: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    let family = "QFN|QFPN|DFN|QFP|SOIC|SOP|DIP|BGA|LGA|CSP|SON|MLF|MLP|LCC|SO";
    regex::Regex::new(&format!(r"(?:{family})(\d{{1,3}})$")).expect("a valid pattern")
});

/// `SC70` spelled as one token: its body number.
fn sc_body(token: &str) -> Option<u32> {
    token.strip_prefix("SC")?.parse().ok()
}

/// How many pins a package or footprint name says it has.
///
/// Read only where the spelling is unambiguous: an IPC-7351 name's `-49N`, `N pins`, a SOT/SC/TO
/// body's count suffix (`SOT-23-5`; a bare `SOT-23` is 3), or a counted family
/// (`QFN-48`, `UFQFPN48`, `SOIC-8`). `None` when two readings disagree.
#[must_use]
pub fn pin_count(name: &str) -> Option<u32> {
    let tokens = tokens(name);
    if let Some(c) = IPC.captures(&tokens.join("-")) {
        return c[1].parse().ok();
    }
    let mut counts = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        let next = tokens.get(i + 1).and_then(|t| t.parse::<u32>().ok());
        let after = tokens.get(i + 2).and_then(|t| t.parse::<u32>().ok());
        if token == "PINS" || token == "PIN" {
            counts.push(i.checked_sub(1).and_then(|j| tokens[j].parse().ok()));
        } else if ["SOT", "TSOT", "TO", "SC"].contains(&token.as_str()) || sc_body(token).is_some()
        {
            let body = sc_body(token).or(next);
            let suffix = if body == next { after } else { next };
            counts.push(suffix.or_else(|| (token == "SOT" && body == Some(23)).then_some(3)));
        } else if let Some(c) = COUNTED
            .captures(token)
            .filter(|_| !token.starts_with("SOT"))
        {
            counts.push(c[1].parse().ok());
        } else if COUNTED.is_match(&format!("{token}0"))
            && token.chars().all(|c| c.is_ascii_alphabetic())
        {
            counts.push(next);
        }
    }
    let mut counts: Vec<u32> = counts.into_iter().flatten().collect();
    counts.sort_unstable();
    counts.dedup();
    match counts[..] {
        [one] => Some(one),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{outline, pin_count, Outline};

    #[test]
    fn outlines_and_pin_counts_read_off_package_and_footprint_names() {
        let cases: &[(&str, Option<Outline>, Option<u32>)] = &[
            ("UFQFPN48", Some(Outline::QuadLeadless), Some(48)),
            (
                "moteus:UFQFPN-48_7X7X0P55MM",
                Some(Outline::QuadLeadless),
                Some(48),
            ),
            (
                "QFN50P700X700X60-49N",
                Some(Outline::QuadLeadless),
                Some(49),
            ),
            (
                "Package_DFN_QFN:QFN-48-1EP_7x7mm_P0.5mm",
                Some(Outline::QuadLeadless),
                Some(48),
            ),
            (
                "LQFP-64_10x10mm_P0.5mm",
                Some(Outline::QuadLeaded),
                Some(64),
            ),
            (
                "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm",
                Some(Outline::SmallOutline),
                Some(8),
            ),
            ("TSSOP-20", Some(Outline::SmallOutline), Some(20)),
            ("SO-8", Some(Outline::SmallOutline), Some(8)),
            ("Package_TO_SOT_SMD:SOT-23-5", Some(Outline::Sot), Some(5)),
            ("SOT-23", Some(Outline::Sot), Some(3)),
            ("SC-70-6", Some(Outline::Sot), Some(6)),
            ("SC70-5", Some(Outline::Sot), Some(5)),
            ("DCK (SC70), 5 pins", Some(Outline::Sot), Some(5)),
            ("WSON-8", Some(Outline::DualLeadless), Some(8)),
            ("LQFP48/UFQFPN48", None, Some(48)),
            ("LQFP48/LQFP64", Some(Outline::QuadLeaded), None),
            ("TO-220-3", None, Some(3)),
            ("Resistor_SMD:R_0603_1608Metric", None, None),
        ];
        for &(name, want_outline, want_count) in cases {
            assert_eq!(
                (outline(name), pin_count(name)),
                (want_outline, want_count),
                "{name}"
            );
        }
    }
}
