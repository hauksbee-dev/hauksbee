//! Does a part's `Datasheet` field name a different device?
//!
//! A schematic's `Datasheet` property is copied from a symbol, a sibling part
//! or a search result, and is often left pointing at the part it was copied
//! from: an OPA379 whose link reads `opa2336.pdf`, a 74HC157 whose link is the
//! LS-family sheet. A model drafted from that file describes the wrong device,
//! and nothing downstream notices.
//!
//! The comparison is deliberately conservative. Only the file name of a link
//! that ends in `.pdf` is read, and only when it carries a part-number-like
//! token. Vendor document numbers (`ds31783`, `slos189a`, `20001826E`),
//! version, package and hash tokens say nothing about the device and are set
//! aside. The part number is compared after the same normalisation the layout
//! lint uses (`normalise_mpn`), with its packaging and reel codes left on it:
//! a file name that is a prefix of the part, contains it, or shares five
//! leading characters with it agrees, so `tps7a47.pdf` serves a TPS7A4701RGWR
//! and `tl07x.pdf` a TL072. Digit runs of three or more figures are then
//! compared allowing a single changed or inserted digit (the channel count of
//! an op-amp family), so a family sheet that lists the part among others is
//! silent. 74-series logic is compared by family and function, where the
//! family letters matter: `74HC_HCT4051` lists HC and HCT, `sn74LS157` does
//! not list HC. Whatever cannot be parsed is silent.

use crate::layout_lint::{is_part_number, normalise_mpn};

/// The device the datasheet link names when it clearly is not `identities`.
///
/// `identities` are the board's candidate part numbers for one component (its
/// manufacturer-part-number fields and its value); the link agrees when any of
/// them agrees. `locator` is the `Datasheet` field. Returns the link's file
/// name without its `.pdf` extension, as written, for the message.
#[must_use]
pub fn datasheet_names_other_part(identities: &[&str], locator: &str) -> Option<String> {
    let stem = file_stem(locator)?;
    let tokens: Vec<String> = stem
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_ascii_uppercase)
        .collect();
    let named: Vec<&str> = tokens
        .iter()
        .map(String::as_str)
        .filter(|t| part_like(t))
        .collect();
    if named.is_empty() {
        return None;
    }
    let file = FileTokens::new(&tokens, &named);
    let mut judged = false;
    for identity in identities.iter().filter(|i| is_part_number(i)) {
        let part: String = normalise_mpn(identity)
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect();
        match file.verdict(&part) {
            Verdict::Agrees | Verdict::Unknown => return None,
            Verdict::Differs => judged = true,
        }
    }
    judged.then_some(stem)
}

/// The file name of a link or path that ends in `.pdf`, without the extension.
fn file_stem(locator: &str) -> Option<String> {
    let path = locator.trim().split(['?', '#']).next()?;
    let name = path.rsplit(['/', '\\']).next()?;
    let name = percent_decoded(name);
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".pdf")?;
    (!stem.is_empty()).then(|| name[..stem.len()].to_string())
}

/// `%20`-style escapes undone; anything malformed is left as it was.
fn percent_decoded(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| char::from(b).to_digit(16);
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(u8::try_from(h * 16 + l).unwrap_or(b' '));
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A token that could be a device's part number: four or more characters with
/// a letter and a digit, and not a document number, version, package or hash.
fn part_like(token: &str) -> bool {
    token.len() >= 4
        && token.chars().any(|c| c.is_ascii_alphabetic())
        && token.chars().any(|c| c.is_ascii_digit())
        && !generic(token)
}

/// Tokens that name a document, revision, package or file rather than a
/// device. Setting one aside only ever makes the comparison quieter.
fn generic(token: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    // Digits then at most one letter, the shape of a document number.
    let digits_then_letter = |s: &str| {
        let body = s
            .strip_suffix(|c: char| c.is_ascii_alphabetic())
            .unwrap_or(s);
        digits(body)
    };
    let hash = token.len() >= 8 && token.bytes().all(|b| b.is_ascii_hexdigit());
    let numbered = |prefix: &str, min: usize| {
        token
            .strip_prefix(prefix)
            .is_some_and(|rest| digits_then_letter(rest) && rest.len() >= min)
    };
    // TI literature numbers: SLOS189A, SBOS123, SCLS123.
    let ti = token.len() >= 7
        && token.starts_with('S')
        && token[1..4].bytes().all(|b| b.is_ascii_uppercase())
        && token.len() <= 8
        && digits_then_letter(&token[4..]);
    hash || ti
        || (digits_then_letter(token) && token.len() >= 7)
        || numbered("DS", 3)
        || numbered("DM", 6)
        || ["AN", "TN", "UM", "TB", "V", "VER", "REV", "R"]
            .iter()
            .any(|p| numbered(p, 1))
        || [
            "SOT", "SOIC", "TSSOP", "SSOP", "MSOP", "VSSOP", "QFN", "VQFN", "UQFN", "HVQFN", "DFN",
            "SOP", "LQFP", "TQFP", "QFP", "BGA", "LGA", "WLCSP", "TO", "DIP", "PDIP", "SC", "SOD",
        ]
        .iter()
        .any(|p| numbered(p, 1))
}

/// What comparing one part number with the link concluded.
enum Verdict {
    Agrees,
    /// Nothing in the part number can be compared (no figures to read).
    Unknown,
    Differs,
}

/// A link's file name, read once for every part number compared with it.
struct FileTokens<'a> {
    named: Vec<&'a str>,
    /// Digit runs of three or more figures, from every token that is not a
    /// hash: a family sheet often lists the later members by figures alone.
    runs: Vec<String>,
    /// The 74-series (family, function) pairs the name lists.
    logic: Vec<(String, String)>,
}

impl<'a> FileTokens<'a> {
    fn new(all: &'a [String], named: &[&'a str]) -> Self {
        let runs = all
            .iter()
            .filter(|t| !(t.len() >= 8 && t.bytes().all(|b| b.is_ascii_hexdigit())))
            .flat_map(|t| digit_runs(t))
            .filter(|r| r.len() >= 3)
            .collect();
        Self {
            named: named.to_vec(),
            runs,
            logic: logic_pairs(all),
        }
    }

    fn verdict(&self, part: &str) -> Verdict {
        if let Some((family, function)) = logic_part(part) {
            if !self.logic.is_empty() {
                let listed = self
                    .logic
                    .iter()
                    .any(|(f, n)| *f == family && *n == function);
                return if listed {
                    Verdict::Agrees
                } else {
                    Verdict::Differs
                };
            }
        }
        let mine: Vec<String> = digit_runs(part)
            .into_iter()
            .filter(|r| r.len() >= 3)
            .collect();
        if mine.is_empty() {
            return Verdict::Unknown;
        }
        let same_name = self.named.iter().any(|t| {
            part.contains(t)
                || t.contains(part)
                || common_prefix(part, t) >= 5
                // `tl07x.pdf`: a trailing wildcard stands for any variant.
                || wildcard_covers(t, part)
        });
        let same_figures = mine.iter().any(|m| {
            self.runs
                .iter()
                .any(|r| close_figures(m, r) || shared_sheet(m, r))
        });
        if same_name || same_figures {
            Verdict::Agrees
        } else {
            Verdict::Differs
        }
    }
}

/// Numbers whose one datasheet is published under the first number: the
/// industrial-range LM2904 is in `lm358.pdf`. A short, closed list; anything
/// not on it is compared by its figures alone.
const SHARED_SHEETS: &[&[&str]] = &[
    &["358", "258", "2904"],
    &["324", "224", "2902"],
    &["339", "239", "2901"],
    &["393", "293", "2903"],
];

fn shared_sheet(a: &str, b: &str) -> bool {
    SHARED_SHEETS
        .iter()
        .any(|group| group.contains(&a) && group.contains(&b))
}

/// `TL07X` stands for every `TL07...` variant.
fn wildcard_covers(token: &str, part: &str) -> bool {
    let stem = token.trim_end_matches('X');
    stem.len() < token.len() && stem.len() >= 3 && part.starts_with(stem)
}

/// The maximal runs of ASCII digits in `s`.
fn digit_runs(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_ascii_digit())
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .collect()
}

fn common_prefix(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

/// The same figures, or one changed digit, or one inserted digit: `OPA379` and
/// `OPA2379` (dual) are one family sheet, `OPA379` and `OPA2336` are not.
fn close_figures(a: &str, b: &str) -> bool {
    if a.len() == b.len() {
        return a.bytes().zip(b.bytes()).filter(|(x, y)| x != y).count() <= 1;
    }
    let (short, long) = if a.len() < b.len() { (a, b) } else { (b, a) };
    long.len() == short.len() + 1
        && (0..long.len()).any(|i| {
            let mut t = long.to_string();
            t.remove(i);
            t == short
        })
}

/// A token with a leading `SN`/`CD`/`MC`/`M` vendor prefix removed when a 74
/// or 54 follows it.
fn without_vendor(s: &str) -> &str {
    for vendor in ["SN", "CD", "MC", "M"] {
        if let Some(rest) = s.strip_prefix(vendor) {
            if rest.starts_with("74") || rest.starts_with("54") {
                return rest;
            }
        }
    }
    s
}

/// (family, function) of a 74/54-series part: `74HC157D` is (`HC`, `157`).
/// `None` for anything else, and for the single-gate `1G` numbering.
fn logic_part(part: &str) -> Option<(String, String)> {
    let rest = without_vendor(part)
        .strip_prefix("74")
        .or_else(|| without_vendor(part).strip_prefix("54"))?;
    let family: String = rest.chars().take_while(char::is_ascii_alphabetic).collect();
    let digits: String = rest[family.len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    ((1..=6).contains(&family.len()) && (2..=6).contains(&digits.len())).then_some((family, digits))
}

/// Every (family, function) pair a file name lists. `74HC_HCT4051` lists HC
/// and HCT at 4051; `74HC4051_4052` lists HC at 4051 and 4052.
fn logic_pairs(tokens: &[String]) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let mut last: Vec<String> = Vec::new();
    for token in tokens {
        let head = without_vendor(token);
        if let Some(rest) = head.strip_prefix("74").or_else(|| head.strip_prefix("54")) {
            let family: String = rest.chars().take_while(char::is_ascii_alphabetic).collect();
            let after = &rest[family.len()..];
            // `74HC157D`: the function digits end the name; `74HC` alone
            // waits for the family or function in the tokens after it.
            let function: String = after.chars().take_while(char::is_ascii_digit).collect();
            if !family.is_empty() && after.is_empty() {
                pending = vec![family];
                continue;
            }
            if !family.is_empty() && function.len() >= 2 {
                pairs.push((family.clone(), function));
                last = vec![family];
                pending.clear();
                continue;
            }
        }
        let letters: String = token
            .chars()
            .take_while(char::is_ascii_alphabetic)
            .collect();
        let digits = &token[letters.len()..];
        if !pending.is_empty() {
            if digits.is_empty() {
                pending.push(letters);
                continue;
            }
            if digits.len() >= 2 && digits.bytes().all(|b| b.is_ascii_digit()) {
                if !letters.is_empty() {
                    pending.push(letters);
                }
                for f in &pending {
                    pairs.push((f.clone(), digits.to_string()));
                }
                last = std::mem::take(&mut pending);
                continue;
            }
        }
        if letters.is_empty() && digits.len() >= 2 && digits.bytes().all(|b| b.is_ascii_digit()) {
            for f in &last {
                pairs.push((f.clone(), digits.to_string()));
            }
        }
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::datasheet_names_other_part as names_other;

    /// The four datasheet links a real board carried for the wrong device.
    #[test]
    fn the_four_real_wrong_links_are_named() {
        for (part, link, other) in [
            (
                "OPA379AIDCKR",
                "https://www.ti.com/lit/ds/symlink/opa2336.pdf",
                "opa2336",
            ),
            (
                "74HC157D",
                "https://www.ti.com/lit/ds/symlink/sn74LS157.pdf",
                "sn74LS157",
            ),
            (
                "74LVC244APW",
                "https://assets.nexperia.com/documents/data-sheet/74HC_HCT244.pdf",
                "74HC_HCT244",
            ),
            (
                "74HC574D",
                "https://www.ti.com/lit/ds/symlink/sn74HCT574.pdf",
                "sn74HCT574",
            ),
        ] {
            assert_eq!(names_other(&[part], link).as_deref(), Some(other), "{part}");
        }
    }

    /// Links that are right, or that say nothing about the device, stay silent.
    #[test]
    fn legitimate_links_stay_silent() {
        for (part, link) in [
            // A family sheet that lists the part.
            (
                "74HC4051D",
                "https://assets.nexperia.com/documents/74HC_HCT4051.pdf",
            ),
            (
                "74HCT4051D",
                "https://assets.nexperia.com/documents/74HC_HCT4051.pdf",
            ),
            ("74HC4052D", "https://example.com/74HC4051_4052_4053.pdf"),
            // Vendor prefixes and reel codes on either side.
            (
                "SN74HC157DR",
                "https://www.ti.com/lit/ds/symlink/sn74hc157.pdf",
            ),
            (
                "CD74HC4051M",
                "https://www.ti.com/lit/ds/symlink/cd74hc4051.pdf",
            ),
            (
                "74HC157D,118",
                "https://assets.nexperia.com/documents/74HC157.pdf",
            ),
            ("LM358DR", "https://www.ti.com/lit/ds/symlink/lm358.pdf"),
            ("LM2904DR", "https://www.ti.com/lit/ds/symlink/lm358.pdf"),
            (
                "AD8606ARMZ-REEL7",
                "https://www.analog.com/media/en/AD8606.pdf",
            ),
            (
                "OPA2379AIDCKR",
                "https://www.ti.com/lit/ds/symlink/opa379.pdf",
            ),
            (
                "TPS7A4701RGWR",
                "https://www.ti.com/lit/ds/symlink/tps7a47.pdf",
            ),
            (
                "TL072CDR",
                "https://www.st.com/resource/en/datasheet/tl07x.pdf",
            ),
            (
                "STM32F103C8T6",
                "https://www.st.com/resource/en/datasheet/stm32f103c8.pdf",
            ),
            (
                "BSS138",
                "https://www.infineon.com/dgdl/Infineon-BSS138-DS-v02_00-EN.pdf",
            ),
            (
                "NCP1117ST33T3G",
                "https://www.onsemi.com/pdf/datasheet/ncp1117-d.pdf",
            ),
            (
                "ATMEGA328P-AU",
                "https://ww1.microchip.com/ATmega48A_PA-88A_PA-168A_PA-328_P.pdf",
            ),
            (
                "MCP1700T-3302E/TT",
                "https://ww1.microchip.com/MCP1700-Low-Quiescent-Current-LDO-20001826E.pdf",
            ),
            (
                "LTC3105EMS#PBF",
                "https://www.analog.com/media/en/3105fa.pdf",
            ),
            // A vendor document number says nothing about the device.
            (
                "DMG1013T-7",
                "https://www.diodes.com/assets/Datasheets/ds31783.pdf",
            ),
            (
                "OPA379AIDCKR",
                "https://www.ti.com/lit/ds/symlink/slos189a.pdf",
            ),
            (
                "STM32F103C8T6",
                "https://www.st.com/resource/DM00161566.pdf",
            ),
            // Not a PDF file name, empty, or nothing to compare.
            ("OPA379AIDCKR", "https://www.ti.com/product/OPA2336"),
            ("OPA379AIDCKR", "https://www.ti.com/lit/gpn/opa2336?dl=1"),
            ("OPA379AIDCKR", "~"),
            ("OPA379AIDCKR", "datasheet.pdf"),
            ("OPA379AIDCKR", "https://example.com/5c7a3b9e0f1d4a2b.pdf"),
            (
                "ESP32-WROOM-32E",
                "https://www.espressif.com/esp32-s3_datasheet_en.pdf",
            ),
            // The value is a description or a passive, not a part number.
            (
                "10k",
                "https://www.yageo.com/upload/PYu-RC_Group_51_RoHS_L_12.pdf",
            ),
            ("100nF X7R", "https://example.com/GRM188R71H104KA93.pdf"),
        ] {
            assert_eq!(names_other(&[part], link), None, "{part} vs {link}");
        }
    }

    /// Either identity agreeing is enough: the value may be a nickname.
    #[test]
    fn any_identity_that_agrees_silences_the_link() {
        let link = "https://www.ti.com/lit/ds/symlink/opa379.pdf";
        assert_eq!(names_other(&["OPA2336", "OPA379AIDCKR"], link), None);
        assert!(names_other(&["OPA2336"], link).is_some());
    }

    /// Distinct devices of one number family are named, and a spec with a
    /// percent-encoded name is read as the name.
    #[test]
    fn different_numbers_are_named_and_escapes_are_read() {
        assert!(names_other(&["LM358DR"], "https://x.example/lm324.pdf").is_some());
        assert!(names_other(&["RP2350"], "https://x.example/rp2040-datasheet.pdf").is_some());
        assert!(names_other(&["74HC4051D"], "https://x.example/74HC4052.pdf").is_some());
        assert!(names_other(&["LM358DR"], "https://x.example/LM358%20datasheet.pdf").is_none());
    }
}
