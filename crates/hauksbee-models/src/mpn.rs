//! Decode a passive's manufacturer part number into what it orders.
//!
//! A chip capacitor's or resistor's part number spells its value, rated
//! voltage, dielectric and case in fixed fields, so a board's MPN field can be
//! read back and set against its Value field and footprint. The series decoded
//! are the major ones (Murata GRM/GCM family, Samsung CL, Yageo CC and RC, TDK
//! C, KEMET C, Vishay CRCW, Panasonic ERJ); anything else is `None`, never a
//! guess. Each table cites the vendor's own part-numbering page; the
//! adopt/borrow decision is in `docs/dev-plans/mpn-decoder.md`.

use std::sync::LazyLock;

use regex::Regex;

/// What a passive's part number orders.
#[derive(Debug, Clone, PartialEq)]
pub struct PassiveMpn {
    /// The series that decoded it (`Murata GRM`).
    pub series: &'static str,
    /// `'C'` for a capacitor, `'R'` for a resistor.
    pub kind: char,
    /// The value in farads or ohms.
    pub value: f64,
    /// The rated voltage (V), when the part number carries one.
    pub rated_v: Option<f64>,
    /// The dielectric (`X7R`), when the part number carries one the table knows.
    pub dielectric: Option<&'static str>,
    /// The imperial case code (`0402`).
    pub case: &'static str,
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a valid pattern")
}

fn lookup<T: Copy>(table: &[(&str, T)], key: &str) -> Option<T> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// A capacitance code in picofarads: two digits and a power of ten (`104`),
/// or `R` as the decimal point (`1R5`, `R50`).
fn pf_code(code: &str) -> Option<f64> {
    let pf = if code.contains('R') {
        code.replacen('R', ".", 1).parse::<f64>().ok()?
    } else {
        let (digits, power) = code.split_at(code.len().checked_sub(1)?);
        digits.parse::<f64>().ok()? * 10f64.powi(power.parse::<i32>().ok()?)
    };
    Some(pf * 1e-12)
}

/// A resistance code: `R`, `K` or `M` as the decimal point and multiplier
/// (`4K7`, `562R`, `10K0`, `R00`), else significant digits and a power of ten
/// (`1002`, `103`).
fn ohm_code(code: &str) -> Option<f64> {
    let Some(at) = code.find(['R', 'K', 'M']) else {
        let (digits, power) = code.split_at(code.len().checked_sub(1)?);
        return Some(digits.parse::<f64>().ok()? * 10f64.powi(power.parse::<i32>().ok()?));
    };
    let scale = match code.as_bytes()[at] {
        b'K' => 1e3,
        b'M' => 1e6,
        _ => 1.0,
    };
    let text = format!("0{}.{}0", &code[..at], &code[at + 1..]);
    Some(text.parse::<f64>().ok()? * scale)
}

fn imperial(code: &str) -> Option<&'static str> {
    crate::chip_size::find_imperial(|c| c == code).map(|c| c.imperial)
}

/// Murata rated voltage, standard and voltage-derated codes (Murata "Part
/// Numbering", Chip Multilayer Ceramic Capacitors, p. 4).
const MURATA_VOLTS: &[(&str, f64)] = &[
    ("0D", 2.0),
    ("0E", 2.5),
    ("EA", 2.5),
    ("0G", 4.0),
    ("EB", 4.0),
    ("0J", 6.3),
    ("EC", 6.3),
    ("1A", 10.0),
    ("ED", 10.0),
    ("1C", 16.0),
    ("EE", 16.0),
    ("1E", 25.0),
    ("EF", 25.0),
    ("YA", 35.0),
    ("EG", 35.0),
    ("1H", 50.0),
    ("EH", 50.0),
    ("1J", 63.0),
    ("1K", 80.0),
    ("2A", 100.0),
    ("EL", 100.0),
    ("2D", 200.0),
    ("2E", 250.0),
    ("BB", 350.0),
    ("2W", 450.0),
    ("2H", 500.0),
    ("2J", 630.0),
    ("3A", 1000.0),
];

/// Murata L x W code (same sheet, p. 2).
const MURATA_SIZE: &[(&str, &str)] = &[
    ("02", "01005"),
    ("03", "0201"),
    ("15", "0402"),
    ("18", "0603"),
    ("21", "0805"),
    ("31", "1206"),
    ("32", "1210"),
    ("42", "1808"),
    ("43", "1812"),
    ("55", "2220"),
];

/// Murata temperature characteristic code (same sheet, p. 3).
const MURATA_TC: &[(&str, &str)] = &[
    ("5C", "C0G"),
    ("R6", "X5R"),
    ("R7", "X7R"),
    ("Z7", "X7R"),
    ("R9", "X8R"),
    ("C7", "X7S"),
    ("C8", "X6S"),
    ("D7", "X7T"),
    ("W0", "X7T"),
];

/// `GRM 15 5 R7 1H 332 K A01 D`: series, L x W, thickness, characteristic,
/// voltage, capacitance, ...
fn murata(pn: &str) -> Option<PassiveMpn> {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| re(r"^G(?:RM|CM|RT|CJ|JM|QM)(\d\d)\w(\w\w)(\w\w)(\d\d\d|\dR\d|R\d\d)"));
    let c = RE.captures(pn)?;
    Some(PassiveMpn {
        series: "Murata",
        kind: 'C',
        value: pf_code(&c[4])?,
        rated_v: Some(lookup(MURATA_VOLTS, &c[3])?),
        dielectric: lookup(MURATA_TC, &c[2]),
        case: lookup(MURATA_SIZE, &c[1])?,
    })
}

/// Samsung CL size, dielectric and voltage codes (Samsung Electro-Mechanics
/// MLCC catalog Part I, December 2025, "Part Numbering", pp. 6-7).
const SAMSUNG_SIZE: &[(&str, &str)] = &[
    ("02", "01005"),
    ("03", "0201"),
    ("05", "0402"),
    ("10", "0603"),
    ("21", "0805"),
    ("31", "1206"),
    ("32", "1210"),
    ("42", "1808"),
    ("43", "1812"),
    ("55", "2220"),
];

const SAMSUNG_VOLTS: &[(&str, f64)] = &[
    ("S", 2.5),
    ("R", 4.0),
    ("Q", 6.3),
    ("P", 10.0),
    ("O", 16.0),
    ("A", 25.0),
    ("L", 35.0),
    ("B", 50.0),
    ("C", 100.0),
    ("D", 200.0),
    ("E", 250.0),
    ("F", 350.0),
    ("G", 500.0),
    ("H", 630.0),
    ("I", 1000.0),
    ("J", 2000.0),
    ("K", 3000.0),
];

const SAMSUNG_TC: &[(&str, &str)] = &[
    ("C", "C0G"),
    ("A", "X5R"),
    ("X", "X6S"),
    ("W", "X6T"),
    ("B", "X7R"),
    ("K", "X7R"),
    ("Y", "X7S"),
    ("Z", "X7T"),
    ("F", "Y5V"),
];

/// `CL 10 B 104 K B 8NNNC`: size, dielectric, capacitance, tolerance, voltage.
fn samsung(pn: &str) -> Option<PassiveMpn> {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| re(r"^CL(\d\d)([A-Z])(\d\d\d|\dR\d|R\d\d)[A-Z]([A-Z])"));
    let c = RE.captures(pn)?;
    Some(PassiveMpn {
        series: "Samsung CL",
        kind: 'C',
        value: pf_code(&c[3])?,
        rated_v: Some(lookup(SAMSUNG_VOLTS, &c[4])?),
        dielectric: lookup(SAMSUNG_TC, &c[2]),
        case: lookup(SAMSUNG_SIZE, &c[1])?,
    })
}

/// Yageo CC voltage code ("Ordering information - global part number", p. 2
/// of the UPY-GPHC X5R/X7R, UPY-NP0X7R mid- and high-voltage datasheets).
const YAGEO_CC_VOLTS: &[(&str, f64)] = &[
    ("4", 4.0),
    ("5", 6.3),
    ("6", 10.0),
    ("7", 16.0),
    ("8", 25.0),
    ("9", 50.0),
    ("0", 100.0),
    ("A", 200.0),
    ("Y", 250.0),
    ("B", 500.0),
    ("Z", 630.0),
    ("C", 1000.0),
    ("D", 2000.0),
    ("S", 2500.0),
    ("E", 3000.0),
];

/// `CC 0402 K R X7R 9 B B 104`: size, tolerance, packing, characteristic
/// (literal, NP0 spelt `NPO`), voltage, `B`, process, capacitance.
fn yageo_cc(pn: &str) -> Option<PassiveMpn> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        re(r"^CC(\d{4})[A-Z][A-Z](NPO|X5R|X7R|X7S|X6S|Y5V)(\w)B[BN](\d\d\d|\dR\d|R\d\d)$")
    });
    let c = RE.captures(pn)?;
    Some(PassiveMpn {
        series: "Yageo CC",
        kind: 'C',
        value: pf_code(&c[4])?,
        rated_v: Some(lookup(YAGEO_CC_VOLTS, &c[3])?),
        dielectric: dielectric_name(&c[2]),
        case: imperial(&c[1])?,
    })
}

/// A dielectric spelt out in the part number, by its EIA name.
fn dielectric_name(text: &str) -> Option<&'static str> {
    ["C0G", "X5R", "X7R", "X7S", "X6S", "X7T", "X8R", "Y5V"]
        .into_iter()
        .find(|d| *d == text)
        .or_else(|| matches!(text, "NPO" | "NP0").then_some("C0G"))
}

/// TDK rated voltage (TDK MLCC catalog B11 "C Series General Application",
/// p. 5; C Series High Temperature, June 2015, p. 2; B11 High Voltage, p. 106).
/// TDK's `3F` is 3 kV where Murata's is 3.15 kV, so the tables stay apart.
const TDK_VOLTS: &[(&str, f64)] = &[
    ("0G", 4.0),
    ("0J", 6.3),
    ("1A", 10.0),
    ("1C", 16.0),
    ("1E", 25.0),
    ("1V", 35.0),
    ("1H", 50.0),
    ("2A", 100.0),
    ("2E", 250.0),
    ("2W", 450.0),
    ("2J", 630.0),
    ("3A", 1000.0),
    ("3D", 2000.0),
    ("3F", 3000.0),
];

/// `C 1608 X7R 1H 104 K 080AA`: metric size, characteristic, voltage,
/// capacitance.
fn tdk(pn: &str) -> Option<PassiveMpn> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        re(r"^C(\d{4})(C0G|NP0|X5R|X7R|X7S|X6S|X7T|X8R|Y5V)(\d[A-Z])(\d\d\d|\dR\d|R\d\d)")
    });
    let c = RE.captures(pn)?;
    Some(PassiveMpn {
        series: "TDK C",
        kind: 'C',
        value: pf_code(&c[4])?,
        rated_v: Some(lookup(TDK_VOLTS, &c[3])?),
        dielectric: dielectric_name(&c[2]),
        case: crate::chip_size::by_metric(&c[1])?.imperial,
    })
}

/// KEMET C-series voltage digit (KEMET C1002 X7R SMD datasheet, p. 1,
/// "Ordering Information").
const KEMET_VOLTS: &[(&str, f64)] = &[
    ("9", 6.3),
    ("8", 10.0),
    ("4", 16.0),
    ("3", 25.0),
    ("6", 35.0),
    ("5", 50.0),
    ("1", 100.0),
    ("2", 200.0),
    ("A", 250.0),
];

/// `C 1206 C 106 M 4 R ACTU`: imperial size, `C`, capacitance, tolerance,
/// voltage, dielectric (`R` is X7R on the cited sheet).
fn kemet(pn: &str) -> Option<PassiveMpn> {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| re(r"^C(\d{4})C(\d\d\d|\dR\d)[A-Z]([0-9A])([A-Z])"));
    let c = RE.captures(pn)?;
    Some(PassiveMpn {
        series: "KEMET C",
        kind: 'C',
        value: pf_code(&c[2])?,
        rated_v: Some(lookup(KEMET_VOLTS, &c[3])?),
        dielectric: (&c[4] == "R").then_some("X7R"),
        case: imperial(&c[1])?,
    })
}

const fn resistor(series: &'static str, value: f64, case: &'static str) -> PassiveMpn {
    PassiveMpn {
        series,
        kind: 'R',
        value,
        rated_v: None,
        dielectric: None,
        case,
    }
}

/// Yageo `RC_L` (datasheet `PYu-RC_Group_51_RoHS_L_12`, p. 2): `RC 0603 F R - 07
/// 10K L`, size, tolerance, packaging, TCR, reel and power, resistance.
fn yageo_rc(pn: &str) -> Option<PassiveMpn> {
    static RE: LazyLock<Regex> = LazyLock::new(|| re(r"^RC(\d{4})[BDFJ][RKS]-\w\w(\d*[RKM]\d*)L$"));
    let c = RE.captures(pn)?;
    Some(resistor("Yageo RC", ohm_code(&c[2])?, imperial(&c[1])?))
}

/// Vishay CRCW (document 20035, p. 3, "Part Number and Product Description"):
/// size, a four-character resistance (`0000` a jumper), tolerance.
fn vishay_crcw(pn: &str) -> Option<PassiveMpn> {
    static RE: LazyLock<Regex> = LazyLock::new(|| re(r"^CRCW(\d{4})([0-9RKM]{4})[FJZ]"));
    let c = RE.captures(pn)?;
    let value = if &c[2] == "0000" {
        0.0
    } else {
        ohm_code(&c[2])?
    };
    Some(resistor("Vishay CRCW", value, imperial(&c[1])?))
}

/// Panasonic ERJ size codes (AOA0000C301 p. 1 for +-5 %, AOA0000C304 pp. 1-2
/// for +-1 % and +-0.5 %), longest first so `12Z` is read before `12`.
const PANASONIC_SIZE: &[(&str, &str)] = &[
    ("XGN", "01005"),
    ("1GN", "0201"),
    ("1GJ", "0201"),
    ("12Z", "2010"),
    ("12S", "2010"),
    ("12N", "1812"),
    ("14N", "1210"),
    ("1TN", "2512"),
    ("2GE", "0402"),
    ("2RC", "0402"),
    ("2RK", "0402"),
    ("3GE", "0603"),
    ("3EK", "0603"),
    ("6GE", "0805"),
    ("6EN", "0805"),
    ("8GE", "1206"),
    ("8EN", "1206"),
    ("1R", "0201"),
    ("2R", "0402"),
    ("3R", "0603"),
    ("6R", "0805"),
    ("14", "1210"),
    ("12", "1812"),
    ("1T", "2512"),
];

/// `ERJ-3EKF1002V`, `ERJ-3GEYJ102V`, `ERJ-3RBD1002V`: size, the +-0.5 % TCR
/// letter or the +-5 % marking `Y`, tolerance, resistance, packaging.
fn panasonic_erj(pn: &str) -> Option<PassiveMpn> {
    static RE: LazyLock<Regex> = LazyLock::new(|| re(r"^[HBKEY]?[DFJ0](\d{3,4}|\d*R\d+)[A-Z]$"));
    let body = pn.strip_prefix("ERJ")?;
    let body = body.strip_prefix('-').unwrap_or(body);
    let (code, case) = PANASONIC_SIZE
        .iter()
        .find(|(code, _)| body.starts_with(code))?;
    let c = RE.captures(&body[code.len()..])?;
    Some(resistor("Panasonic ERJ", ohm_code(&c[1])?, case))
}

/// What `mpn` orders, when it is a passive of a series this module decodes.
///
/// ```
/// use hauksbee_models::mpn::decode;
/// let c = decode("GRM155R71H332KA01D").unwrap();
/// assert_eq!((c.kind, c.case, c.rated_v, c.dielectric), ('C', "0402", Some(50.0), Some("X7R")));
/// assert!((c.value - 3.3e-9).abs() < 1e-15);
/// assert_eq!(decode("CRCW060310K0FKEA").unwrap().value, 10_000.0);
/// assert_eq!(decode("LM358"), None);
/// ```
#[must_use]
pub fn decode(mpn: &str) -> Option<PassiveMpn> {
    let pn = mpn.trim().to_ascii_uppercase();
    [
        murata,
        samsung,
        yageo_cc,
        tdk,
        kemet,
        yageo_rc,
        vishay_crcw,
        panasonic_erj,
    ]
    .iter()
    .find_map(|f| f(&pn))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Part numbers with the values their vendors publish for them.
    #[test]
    fn major_series_decode_to_their_published_values() {
        for (pn, kind, value, volts, case) in [
            ("GRM188R71H331KA01D", 'C', 330e-12, Some(50.0), "0603"),
            ("GRM21BR61A106KE19L", 'C', 10e-6, Some(10.0), "0805"),
            ("GCM1555C1H101JA16D", 'C', 100e-12, Some(50.0), "0402"),
            ("GRM188R6YA106MA73D", 'C', 10e-6, Some(35.0), "0603"),
            ("CL10B104KB8NNNC", 'C', 100e-9, Some(50.0), "0603"),
            ("CL05A105KQ5NNNC", 'C', 1e-6, Some(6.3), "0402"),
            ("CC0402KRX7R9BB104", 'C', 100e-9, Some(50.0), "0402"),
            ("CC0402JRNPO9BN100", 'C', 10e-12, Some(50.0), "0402"),
            ("C1608X7R1H104K080AA", 'C', 100e-9, Some(50.0), "0603"),
            ("C1005C0G1H180J", 'C', 18e-12, Some(50.0), "0402"),
            ("C0603C104K5RACTU", 'C', 100e-9, Some(50.0), "0603"),
            ("RC0603FR-0710KL", 'R', 10e3, None, "0603"),
            ("RC0402JR-07100KL", 'R', 100e3, None, "0402"),
            ("RC0402JR-074K7L", 'R', 4.7e3, None, "0402"),
            ("RC0603JR-070RL", 'R', 0.0, None, "0603"),
            ("CRCW0603562RFKEA", 'R', 562.0, None, "0603"),
            ("ERJ-3EKF1002V", 'R', 10e3, None, "0603"),
            ("ERJ-3GEYJ102V", 'R', 1e3, None, "0603"),
            ("ERJ-2GEJ103X", 'R', 10e3, None, "0402"),
            ("ERJ-3RBD1002V", 'R', 10e3, None, "0603"),
            ("ERJ-12ZYJ102U", 'R', 1e3, None, "2010"),
        ] {
            let d = decode(pn).unwrap_or_else(|| panic!("{pn} decodes"));
            assert_eq!((d.kind, d.rated_v, d.case), (kind, volts, case), "{pn}");
            assert!((d.value - value).abs() <= value * 1e-9, "{pn}: {}", d.value);
        }
    }

    #[test]
    fn unknown_series_and_codes_abstain() {
        for pn in [
            "LM358DR",
            "BSS138",
            "GRM99BR71H332KA01D",
            "GRM155R71V332KA01D",
            "CL10B104KZ8NNNC",
            "C1525",
            "10k",
            "",
        ] {
            assert_eq!(decode(pn), None, "{pn}");
        }
    }
}
