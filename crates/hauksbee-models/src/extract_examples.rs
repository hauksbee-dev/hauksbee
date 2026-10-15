//! Curated library cards for parts like the one being extracted, shown to the
//! extraction as patterns.
//!
//! A person writing a card by hand greps the library for a similar part and
//! copies its shape: a buck is a converter block on `sw` with board feedback, a
//! motor or gate driver is a supply-draw law. The extraction prompt's format
//! examples are deliberately of other kinds (a same-kind card offers numbers to
//! copy), so without this it had no way to learn how the library writes a part
//! class the generic kind guidance does not spell out.

use std::collections::HashSet;

/// Words that say nothing about what a part is.
const STOP: &[&str] = &[
    "and",
    "the",
    "with",
    "for",
    "not",
    "can",
    "modelled",
    "modeled",
    "model",
    "models",
    "only",
    "pin",
    "pins",
    "package",
    "packages",
    "pad",
    "from",
    "per",
    "its",
    "via",
    "one",
    "two",
    "low",
    "high",
    "input",
    "output",
    "voltage",
    "current",
    "supply",
    "datasheet",
    "text",
    "averaged",
    "draw",
    "card",
    "this",
    "that",
    "are",
    "has",
    "but",
    "all",
    "any",
    "each",
];

/// Part classes the generic kind guidance does not spell out, each as the
/// phrases a datasheet's front page uses for it and the marker of the card
/// shape the library writes it in.
const PATTERNS: &[(&[&str], &str)] = &[
    (
        &[
            "buck",
            "boost",
            "step-down",
            "step down",
            "step-up",
            "step up",
            "dc-dc",
            "dc/dc",
            "switching regulator",
            "switch-mode",
        ],
        "[models.behavioral.converter]",
    ),
    (
        &[
            "motor driver",
            "h-bridge",
            "gate driver",
            "fet driver",
            "led driver",
            "half-bridge",
            "pre-driver",
            "predriver",
            "solenoid",
        ],
        r#"name = "supply_draw""#,
    ),
];

/// Most library cards one prompt carries.
pub const LIMIT: usize = 2;
/// A longer card would crowd the prompt more than its pattern is worth.
const MAX_CARD_CHARS: usize = 6_000;
/// How much of the datasheet's text counts as its front page.
const FRONT_CHARS: usize = 3_000;

fn words(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() >= 3 && !w.bytes().any(|b| b.is_ascii_digit()))
        .map(str::to_ascii_lowercase)
        .filter(|w| !STOP.contains(&w.as_str()))
        .collect()
}

/// Every `[[models]]` block of the builtin files as written, less the comment
/// header of the card that follows it.
fn blocks() -> Vec<&'static str> {
    let mut out = Vec::new();
    for (_, text) in crate::BUILTIN_TOML_FILES {
        let starts: Vec<usize> = text
            .match_indices("[[models]]\n")
            .map(|(i, _)| i)
            .filter(|&i| i == 0 || text.as_bytes()[i - 1] == b'\n')
            .collect();
        for (n, &start) in starts.iter().enumerate() {
            let end = starts.get(n + 1).copied().unwrap_or(text.len());
            let mut block = &text[start..end];
            while let Some(cut) = block.trim_end().rfind('\n') {
                let last = block[cut + 1..].trim();
                if last.is_empty() || last.starts_with('#') {
                    block = &block[..cut];
                } else {
                    break;
                }
            }
            out.push(block);
        }
    }
    out
}

/// A parsed library card: its text, id, kind, description words and whether
/// its match patterns already cover the part.
struct Card {
    text: &'static str,
    id: String,
    kind: String,
    words: HashSet<String>,
    matches_part: bool,
}

fn cards(part: &str) -> Vec<Card> {
    let parse = |text: &'static str| {
        let doc: toml::Table = toml::from_str(text).ok()?;
        let entry = doc.get("models")?.as_array()?.first()?.as_table()?;
        let matcher = entry.get("match").and_then(toml::Value::as_table);
        let matches_part = matcher.into_iter().flatten().any(|(_, re)| {
            re.as_str()
                .and_then(|re| regex::Regex::new(re).ok())
                .is_some_and(|re| re.is_match(part))
        });
        Some(Card {
            text,
            id: entry.get("id")?.as_str()?.to_string(),
            kind: entry.get("kind")?.as_str()?.to_string(),
            words: words(entry.get("description")?.as_str()?),
            matches_part,
        })
    };
    blocks().into_iter().filter_map(parse).collect()
}

/// Up to [`LIMIT`] library cards of `kind` in the shape a [`PATTERNS`] class
/// named on the datasheet's front page calls for, the closest descriptions
/// first (shared words weighted by rarity), never a card already matching
/// `part`. Empty when the front page names no such class: the generic
/// guidance covers the rest, and a loose likeness only adds noise.
#[must_use]
pub fn similar(part: &str, kind: &str, front_text: &str) -> Vec<(String, &'static str)> {
    let front_text: String = front_text.chars().take(FRONT_CHARS).collect();
    let low = front_text.to_ascii_lowercase();
    let low = low.split_whitespace().collect::<Vec<_>>().join(" ");
    let markers: Vec<&str> = PATTERNS
        .iter()
        .filter(|(phrases, _)| phrases.iter().any(|p| low.contains(p)))
        .map(|(_, marker)| *marker)
        .collect();
    if markers.is_empty() {
        return Vec::new();
    }
    let all = cards(part);
    let total = all.len().max(1);
    let rarity = |w: &String| {
        let seen = all.iter().filter(|c| c.words.contains(w)).count();
        rarity_weight(total, seen)
    };
    let front = words(&front_text);
    let mut scored: Vec<(u64, &Card)> = all
        .iter()
        .filter(|c| c.kind == kind && !c.matches_part && c.text.len() <= MAX_CARD_CHARS)
        .filter(|c| markers.iter().any(|m| c.text.contains(m)))
        .map(|c| (c.words.intersection(&front).map(rarity).sum(), c))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.text.len().cmp(&b.1.text.len())));
    scored
        .into_iter()
        .take(LIMIT)
        .map(|(_, c)| (c.id.clone(), c.text))
        .collect()
}

/// A word's weight from how many of `total` cards use it: the inverse
/// document frequency in quarter-bits (`4 * log2(total / seen)`), zero for a
/// word most cards share.
fn rarity_weight(total: usize, seen: usize) -> u64 {
    let ratio = total.saturating_mul(16) / seen.max(1);
    // log2(ratio^4) = 4 * log2(ratio); ratio^4 stays far inside u64 for any
    // library size, since ratio <= 16 * total.
    let ratio = u64::try_from(ratio).unwrap_or(u64::MAX).min(1 << 15);
    u64::from(ratio.pow(4).max(1).ilog2()).saturating_sub(16)
}

/// The prompt section showing [`similar`]'s cards, or nothing when none fits.
#[must_use]
pub fn prompt_section(part: &str, kind: &str, front_text: &str) -> String {
    let cards = similar(part, kind, front_text);
    if cards.is_empty() {
        return String::new();
    }
    let ids: Vec<&str> = cards.iter().map(|(id, _)| id.as_str()).collect();
    let body: Vec<&str> = cards.iter().map(|(_, card)| *card).collect();
    format!(
        "LIBRARY PATTERNS -- CURATED CARDS OF THIS KIND FOR SIMILAR PARTS ({}):\n\
         Hauksbee's own library models parts like this one this way. Follow their SHAPE: which \
         blocks they use, the pin-role names, how the behaviour is expressed and what they \
         declare as not modelled. Their numbers, pin numbers, match patterns, references and \
         sha256 belong to OTHER parts: every value in your answer comes from this datasheet, \
         and copying one of theirs is a WRONG ANSWER.\n\n{}\n\n",
        ids.join(", "),
        body.join("\n\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_buck_is_shown_converter_cards_and_never_its_own() {
        let front = "3-A, 28-V input, step-down synchronous buck converter, adjustable \
                     output, eco-mode, current mode control";
        let cards = similar("TPS62933DRLR", "vreg", front);
        assert_eq!(cards.len(), LIMIT, "{cards:?}");
        assert!(cards
            .iter()
            .all(|(_, c)| c.contains("[models.behavioral.converter]")));
        assert!(
            cards.iter().all(|(id, _)| !id.contains("tps62933")),
            "{cards:?}"
        );
        assert!(cards.iter().all(|(_, c)| c.starts_with("[[models]]")));
        assert!(cards.iter().all(|(_, c)| !c
            .trim_end()
            .lines()
            .last()
            .unwrap_or("")
            .starts_with('#')));
    }

    #[test]
    fn a_motor_driver_is_shown_supply_draw_cards() {
        let front = "Brushed DC motor driver with an integrated H-bridge, PWM control, \
                     current regulation and sleep mode";
        let cards = similar("XYZ123", "digital", front);
        assert!(!cards.is_empty());
        assert!(
            cards.iter().all(|(_, c)| c.contains("supply_draw")),
            "{cards:?}"
        );
        assert!(prompt_section("XYZ123", "digital", front).contains("WRONG ANSWER"));
    }

    #[test]
    fn a_front_page_naming_no_such_class_gets_none() {
        assert!(similar("X", "digital", "").is_empty());
        let sensor = "Humidity and temperature sensor, I2C interface, low power, calibrated";
        assert!(prompt_section("X", "digital", sensor).is_empty());
        // A pattern of another kind's shape adds nothing to this kind.
        assert!(similar("X", "opamp", "synchronous buck converter").is_empty());
    }

    #[test]
    fn rarity_is_a_log_ratio() {
        assert_eq!(rarity_weight(10, 10), 0);
        assert_eq!(rarity_weight(2, 1), 4);
        assert_eq!(rarity_weight(8, 1), 12);
        assert!(rarity_weight(600, 2) > rarity_weight(600, 60));
    }
}
