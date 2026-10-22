//! How far a drafted card can be trusted, and how a finding resting on a doubted
//! field is worded.
//!
//! A card drafted from a datasheet is not a reviewed one. Measured on held-out
//! boards, the false alarms a clean board drew came from a handful of drafted-card
//! mistakes, each of which a deterministic check can see in the card itself:
//!
//! 1. a pad read from the wrong package column (a signal pad given the ground
//!    return's role, or a pad count that is not the footprint's);
//! 2. a split-supply op-amp limit (`±15 V`) written as absolute numbers, which a
//!    single-supply design (V- on ground) then "violates";
//! 3. a current-shunt monitor's sense input rated to the part's own supply, when
//!    the input's common-mode range reaches far above it.
//!
//! One set of detectors ([`entry_problems`], [`board_problems`], and the
//! pad-count check in [`crate::card_coverage`]) serves three readers. The
//! extraction validator ([`crate::card_coverage::findings`], so `ds check` and the
//! post-session check) tells the drafter what to fix while it can. A problem the
//! drafter leaves is stamped on the card as a `[[models.source.doubt]]` row, with
//! its own low confidence ([`stamp`]). The finding pass then reads those rows
//! ([`doubt_note`]): a finding whose only basis is a doubted field of a drafted
//! card is reported as "check this card", never as a board fault, and links the
//! card file. Reviewed cards (the built-in library, packs, hand-written user
//! models) carry no doubt and are judged exactly as before.

use std::sync::LazyLock;

use hauksbee_ir::evidence::ModelSourceTier;
use regex::Regex;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table};

use crate::pin_roles::{is_sense_input_role, power_pin, PowerKind};
use crate::schema::{ComponentKind, Doubt, DoubtArea, ModelEntry, OperatingEnvelope};
use crate::{netclass, SourceLayer};

/// One thing wrong or unconfirmed on a drafted card, worded as the fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub area: DoubtArea,
    pub message: String,
}

impl Problem {
    const fn new(area: DoubtArea, message: String) -> Self {
        Self { area, message }
    }

    /// The doubt row that records this problem on the card.
    #[must_use]
    pub fn doubt(&self) -> Doubt {
        Doubt {
            area: self.area,
            reason: self.message.clone(),
        }
    }
}

/// Whether a resolved card is a drafted one: extracted from a datasheet into a
/// user's own model directory, not reviewed into the library or a pack.
#[must_use]
pub fn is_drafted(tier: ModelSourceTier, layer: Option<SourceLayer>) -> bool {
    tier == ModelSourceTier::DatasheetDerived
        && matches!(
            layer,
            Some(SourceLayer::UserDir | SourceLayer::UserConfigDir | SourceLayer::ModelsDirFlag)
        )
}

// ── Class 2: a split-supply limit written as absolute numbers ─────────────────

/// A negative-supply role of an op-amp or comparator (`vee`, `v_minus`, `v-`).
#[must_use]
pub fn is_negative_supply_role(role: &str) -> bool {
    let r = role.trim().to_ascii_lowercase();
    r.starts_with("vee")
        || r.starts_with("vss")
        || r.starts_with("vneg")
        || matches!(
            r.as_str(),
            "v-" | "vn" | "v_minus" | "vminus" | "vs-" | "vs_minus"
        )
        || r.ends_with("_neg")
}

/// Rail-relative limits of an op-amp or comparator: never absolute `±` figures.
///
/// An op-amp's datasheet prints its limits for a split supply (`±15 V`) and, in
/// the same table, the single-supply equivalent. A supply window on the negative
/// supply pin, or a negative `min_v` anywhere, is written for the split supply:
/// the same part on a single supply has V- on ground, which that row then
/// "violates". The supply window is the whole V+ to V- span (`±2.25..±18 V` is
/// 4.5..36 V on the V+ pin), and an input range tracks the rails
/// (`input_common_mode`, a headroom below V+).
fn split_supply(entry: &ModelEntry) -> Vec<Problem> {
    if !matches!(entry.kind, ComponentKind::Opamp | ComponentKind::Comparator) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for e in &entry.envelope {
        let OperatingEnvelope::SupplyRange {
            pin, min_v, max_v, ..
        } = e
        else {
            continue;
        };
        let returns = power_pin(pin, entry.kind).is_some_and(|p| p.kind == PowerKind::Return);
        let negative_rail = is_negative_supply_role(pin);
        // A few tenths of a volt below ground is an input clamp rating, not a split rail.
        if negative_rail || (!returns && *min_v <= -1.0) {
            out.push(Problem::new(
                DoubtArea::Envelope,
                format!(
                    "supply_range on '{pin}' ({min_v}..{max_v} V) is an absolute figure written \
                     for a split supply: a single-supply design puts V- on ground, which it then \
                     violates. Limits relative to V- are rail-relative: give the V+ pin's \
                     supply_range the whole V+ to V- span as printed (±2.25..±18 V is 4.5..36 V), \
                     an input range as input_common_mode (headroom below V+), and no row on the \
                     V- pin"
                ),
            ));
        }
    }
    out
}

// ── Class 3: a sense input rated to the part's own supply ─────────────────────

/// The differential inputs of a shunt monitor, which read like an amplifier's.
fn is_differential_sense_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "in_plus"
            | "in_minus"
            | "vin_plus"
            | "vin_minus"
            | "in+"
            | "in-"
            | "vin+"
            | "vin-"
            | "inp"
            | "inn"
            | "is_plus"
            | "is_minus"
    )
}

static CURRENT_SENSE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)current[ -]?(sens|shunt|monitor)|shunt|power monitor|current/power|\bina\d{3}")
        .expect("valid regex")
});

/// Whether the card is a current-shunt monitor or current-sense amplifier: its
/// text says so, or it has a pin that only a sense amplifier has.
fn is_current_sense(entry: &ModelEntry) -> bool {
    CURRENT_SENSE.is_match(&entry.description)
        || CURRENT_SENSE.is_match(&entry.id)
        || entry.pins.values().any(|r| is_sense_input_role(r))
}

/// A basis that rates the input relative to the supply (`VS + 0.3 V`): the
/// datasheet itself says the input is capped at the supply.
static SUPPLY_RELATIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bV(S|CC|DD|\+|IN)\b\s*[+\-−]\s*\d|\bsupply[- ]relative|\brelative to (V|the )?supply")
        .expect("valid regex")
});

/// The sense inputs of a shunt monitor: its sense-role pins and, for a card that
/// reads as current sensing, its differential inputs.
fn sense_inputs(entry: &ModelEntry) -> Vec<&str> {
    entry
        .pins
        .values()
        .map(String::as_str)
        .filter(|r| is_sense_input_role(r) || is_differential_sense_role(r))
        .collect()
}

/// A shunt monitor's sense input is not capped at its own supply, and is rated.
///
/// The common-mode range of a current-shunt monitor reaches far above VS (an
/// INA219 on 3.3 V senses a 26 V bus). A card whose sense input is limited to the
/// part's own supply makes every real board "overvoltage" it, and a card that
/// rates the input not at all leaves the back-power rule to read the bus as a
/// signal driven above the supply.
fn sense_input(entry: &ModelEntry) -> Vec<Problem> {
    if !is_current_sense(entry) {
        return Vec::new();
    }
    let inputs = sense_inputs(entry);
    if inputs.is_empty() {
        return Vec::new();
    }
    // `vs` is a supply only on an amplifier to the generic role table; a shunt monitor's is one too.
    let is_supply = |pin: &str| {
        power_pin(pin, entry.kind).is_some_and(|p| p.kind == PowerKind::Supply)
            || matches!(pin.to_ascii_lowercase().as_str(), "vs" | "v_s" | "vsupply")
    };
    let own_supply_max = entry
        .envelope
        .iter()
        .filter_map(|e| match e {
            OperatingEnvelope::SupplyRange { pin, max_v, .. } if is_supply(pin) => Some(*max_v),
            _ => None,
        })
        .reduce(f64::max);
    let mut out = Vec::new();
    let mut unrated = Vec::new();
    for role in &inputs {
        let mut rated = false;
        for e in &entry.envelope {
            let (hit, ceiling, texts) = match e {
                OperatingEnvelope::SupplyRange {
                    pin,
                    max_v,
                    abs_max_v,
                    basis,
                    abs_max_basis,
                    ..
                } => (
                    pin.eq_ignore_ascii_case(role),
                    abs_max_v.unwrap_or(*max_v),
                    [basis, abs_max_basis],
                ),
                OperatingEnvelope::InputTolerance {
                    pins,
                    max_v,
                    abs_max_v,
                    basis,
                    abs_max_basis,
                    ..
                } => (
                    pins.iter().any(|p| p.eq_ignore_ascii_case(role)),
                    abs_max_v.unwrap_or(*max_v),
                    [basis, abs_max_basis],
                ),
                _ => continue,
            };
            if !hit {
                continue;
            }
            rated = true;
            let stated = texts.iter().any(|t| SUPPLY_RELATIVE.is_match(t));
            if !stated && own_supply_max.is_some_and(|vs| ceiling <= vs + 0.6) {
                out.push(Problem::new(
                    DoubtArea::Envelope,
                    format!(
                        "sense input '{role}' is rated to {ceiling} V, no more than the part's \
                         own supply ({} V): a current-shunt monitor's input common-mode range \
                         normally reaches far above VS, so every real board would be flagged. \
                         Rate it from the datasheet's input common-mode or input-voltage row as an \
                         input_tolerance, or leave it unrated unless the datasheet caps it at the supply",
                        own_supply_max.unwrap_or_default()
                    ),
                ));
            }
        }
        if !rated {
            unrated.push(*role);
        }
    }
    unrated.sort_unstable();
    unrated.dedup();
    if !unrated.is_empty() {
        out.push(Problem::new(
            DoubtArea::Envelope,
            format!(
                "sense input(s) {} carry no voltage rating: add an input_tolerance row from the \
                 datasheet's common-mode or input-voltage row (supply-independent, so a bus above \
                 VS is not read as back-powering the part)",
                unrated.join(", ")
            ),
        ));
    }
    out
}

/// Every problem the card shows on its own, with no datasheet or board in hand.
#[must_use]
pub fn entry_problems(entry: &ModelEntry) -> Vec<Problem> {
    let mut out = split_supply(entry);
    out.extend(sense_input(entry));
    out.extend(vih_over_supply(entry));
    out
}

/// Class 4: an input-high threshold above the supply it is read at. The figure
/// is the wrong column, another supply's row, or a ratio (0.7 VDD) written as
/// volts at a larger rail, so every divider and pull-up verdict built on it is
/// wrong. The drafting validator refuses it ([`crate::plausibility`]); a card
/// that got past it is stamped here.
fn vih_over_supply(entry: &ModelEntry) -> Vec<Problem> {
    crate::plausibility::vih_above_supply(entry)
        .into_iter()
        .map(|why| {
            Problem::new(
                DoubtArea::Card,
                format!(
                    "{why}: an input-high threshold cannot exceed its own supply. Re-read the \
                     VIH row at the supply it is stated for (a ratio such as 0.7 VDD goes in \
                     `levels.vih` as [vcc, volts] points, not as volts at the top rail)"
                ),
            )
        })
        .collect()
}

// ── Class 1: a power or return role on a pad the board wires as a signal ──────

/// Tokens of a net name that only a signal carries.
const SIGNAL_TOKENS: &[&str] = &[
    "SDA", "SCL", "SCK", "SCLK", "CLK", "MOSI", "MISO", "SDI", "SDO", "CS", "NSS", "RX", "TX",
    "RXD", "TXD", "RTS", "CTS", "RST", "RESET", "INT", "IRQ", "GPIO", "ADC", "DAC", "PWM", "DATA",
    "SIG", "SIGNAL", "SW", "LX", "FB", "COMP", "DP", "DM",
];

static SIGNAL_PIN_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(P[A-K]\d+|GPIO\d+|IO\d+|ADC\d+|AIN\d+|D\d+|A\d+|DQ\d+|SDA\d*|SCL\d*)$")
        .expect("valid regex")
});

/// A net name that is clearly a signal: not ground, not a rail, and carrying a
/// token only a signal has. An unnamed net (`Net-(U1-Pad3)`) is not clearly anything.
#[must_use]
pub fn clearly_signal(net: &str) -> bool {
    if netclass::is_ground(net) || netclass::may_be_supply(net) {
        return false;
    }
    let leaf = netclass::leaf(net);
    if leaf.is_empty() || leaf.starts_with("NET-(") || leaf.contains("UNCONNECTED") {
        return false;
    }
    leaf.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|t| SIGNAL_TOKENS.contains(&t) || SIGNAL_PIN_NAME.is_match(t))
}

/// A ground or supply role on a pad whose net on the board is clearly a signal.
///
/// The card then reads the pad from another package's column (or another pad
/// numbering), and every power-wiring finding on the part is about the card.
#[must_use]
pub fn board_problems(entry: &ModelEntry, pads: &[(String, String)]) -> Vec<Problem> {
    let mut out = Vec::new();
    for (pad, role) in &entry.pins {
        let Some(power) = power_pin(role, entry.kind) else {
            continue;
        };
        let Some((_, net)) = pads.iter().find(|(p, _)| p == pad) else {
            continue;
        };
        if !clearly_signal(net) {
            continue;
        }
        let what = match power.kind {
            PowerKind::Return => "ground return",
            PowerKind::Supply => "supply",
        };
        out.push(Problem::new(
            DoubtArea::Pins,
            format!(
                "pad {pad} is the {what} ('{role}') on the card, but the board wires it to \
                 '{net}', a signal net: the pad map was probably read from another package's \
                 column or numbered differently. Re-read pad {pad} from the pin-table column \
                 for this footprint"
            ),
        ));
    }
    out
}

/// Whether `pins` count the pads a footprint of `board_pads` pads (`board_total`
/// counting an exposed pad) has: the package column's count may leave the
/// exposed pad out.
#[must_use]
pub const fn pad_counts_agree(
    column_pins: usize,
    board_numeric: usize,
    board_total: usize,
) -> bool {
    column_pins == board_numeric || column_pins == board_total || column_pins + 1 == board_total
}

// ── Stamping and reading the doubt rows ───────────────────────────────────────

/// `raw` with a `[[models.source.doubt]]` row for each of `doubts`.
///
/// Rows go on the entry whose id they are keyed by. An entry already carrying rows has them replaced, so
/// a redraft never stacks the same doubt twice. `raw` is returned as it was when
/// there is nothing to stamp or it does not parse.
#[must_use]
pub fn stamp(raw: &str, doubts: &[(String, Vec<Doubt>)]) -> String {
    if doubts.iter().all(|(_, d)| d.is_empty()) {
        return raw.to_string();
    }
    let Ok(mut doc) = raw.parse::<DocumentMut>() else {
        return raw.to_string();
    };
    let Some(tables) = doc.get_mut("models").and_then(Item::as_array_of_tables_mut) else {
        return raw.to_string();
    };
    for table in tables.iter_mut() {
        let id = table.get("id").and_then(Item::as_str).unwrap_or_default();
        let Some((_, rows)) = doubts.iter().find(|(i, _)| i == id) else {
            continue;
        };
        if rows.is_empty() {
            continue;
        }
        let source = table.entry("source").or_insert(Item::Table(Table::new()));
        let Some(source) = source.as_table_mut() else {
            continue;
        };
        let mut array = ArrayOfTables::new();
        for d in rows {
            let mut row = Table::new();
            row.insert("area", toml_edit::value(area_name(d.area)));
            row.insert("reason", toml_edit::value(&d.reason));
            array.push(row);
        }
        source.insert("doubt", Item::ArrayOfTables(array));
    }
    doc.to_string()
}

const fn area_name(area: DoubtArea) -> &'static str {
    match area {
        DoubtArea::Card => "card",
        DoubtArea::Pins => "pins",
        DoubtArea::Envelope => "envelope",
    }
}

/// The finding families a doubt can cap. A family reads a field of the card and
/// nothing else about the part, so a doubt on that field is its only basis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// A level compared with a window or a pin's rating (operating envelope,
    /// back-power, analog and logic inputs).
    Envelope,
    /// What a pad is for (power wiring: open, swapped, grounded supply).
    Pins,
    /// Anything else the card says about the part (strap thresholds, pin
    /// roles, bus and control behaviour, pull-up needs). Only the drafter's own
    /// low-confidence doubt and a pad-map doubt cover it: a wrong pad map taints
    /// every role-based verdict, a wrong ratings row does not.
    Card,
}

/// Whether `doubt` covers findings of the `basis` family.
#[must_use]
pub const fn covers(doubt: DoubtArea, basis: Basis) -> bool {
    matches!(
        (doubt, basis),
        (DoubtArea::Card, _)
            | (DoubtArea::Envelope, Basis::Envelope)
            | (DoubtArea::Pins, Basis::Pins | Basis::Envelope | Basis::Card)
    )
}

/// The reason of the first doubt covering `basis`, if the card has one.
#[must_use]
pub fn doubted(doubts: &[Doubt], basis: Basis) -> Option<&Doubt> {
    doubts.iter().find(|d| covers(d.area, basis))
}

/// The marker every surface reads a downgraded finding by; only [`doubt_note`]
/// writes it.
pub const CHECK_CARD_MARKER: &str = "Check this card";

/// A finding `message` about `part`, resting only on a doubted field of its
/// drafted card `card`: reworded as a note to check the card, not a board fault.
#[must_use]
pub fn doubt_note(part: &str, card: &str, doubt: &Doubt, message: &str) -> String {
    if doubt.area == DoubtArea::Card {
        return format!(
            "{CHECK_CARD_MARKER}: card '{card}' for {part} is drafted, low confidence, verify. \
             It was drafted from the datasheet, not reviewed, and the drafter was not sure of it \
             ({}). Judged by that card the board shows: {message} This is a note about the \
             card, not a board fault, so it does not fail the run; check the card against the \
             datasheet, then delete its [[models.source.doubt]] rows.",
            doubt.reason.trim_end_matches('.')
        );
    }
    format!(
        "{CHECK_CARD_MARKER}: card '{card}' for {part} is drafted from the datasheet, not \
         reviewed, and the drafting check could not confirm it ({}). Judged by that card the \
         board shows: {message} This is a note about the card, not a board fault; check the \
         card against the datasheet, then delete its [[models.source.doubt]] rows.",
        doubt.reason.trim_end_matches('.')
    )
}

/// Whether `message` is a [`doubt_note`].
#[must_use]
pub fn is_doubt_note(message: &str) -> bool {
    message.starts_with(CHECK_CARD_MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(text: &str) -> ModelEntry {
        let mut e: ModelEntry = toml::from_str(text).expect("card parses");
        e.doubts = e
            .source
            .as_ref()
            .map(|s| s.doubt.clone())
            .unwrap_or_default();
        e
    }

    const OPAMP: &str = r#"
id = "oa"
kind = "opamp"
description = "dual op amp"
[pins]
"1" = "out"
"2" = "in_minus"
"3" = "in_plus"
"4" = "vee"
"8" = "vcc"
"#;

    fn row(pin: &str, min: f64, max: f64) -> String {
        format!(
            "[[envelope]]\nkind = \"supply_range\"\npin = \"{pin}\"\nmin_v = {min}\nmax_v = {max}\n\
             basis = \"Recommended Operating Conditions\"\n"
        )
    }

    #[test]
    fn an_opamps_absolute_split_supply_limits_are_not_rail_relative() {
        let negative_rail = entry(&format!("{OPAMP}{}", row("vee", -18.0, -1.5)));
        let found = entry_problems(&negative_rail);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].message.contains("single-supply"), "{found:?}");
        let signed = entry(&format!("{OPAMP}{}", row("in_plus", -15.0, 15.0)));
        assert_eq!(entry_problems(&signed).len(), 1);
        // The V+ pin's whole span, and a clamp a few tenths below ground, are fine.
        let span = entry(&format!(
            "{OPAMP}{}{}",
            row("vcc", 4.5, 36.0),
            row("in_plus", -0.3, 36.0)
        ));
        assert!(entry_problems(&span).is_empty());
        // A regulator's rows are not an op-amp's.
        let vreg = entry(
            "id = \"v\"\nkind = \"vreg\"\n[pins]\n\"1\" = \"in\"\n\"2\" = \"vee\"\n\
             [[envelope]]\nkind = \"supply_range\"\npin = \"in\"\nmin_v = -5.0\nmax_v = 5.0\nbasis = \"x\"\n",
        );
        assert!(entry_problems(&vreg).is_empty());
    }

    const SHUNT: &str = r#"
id = "mon"
kind = "digital"
description = "high-side current shunt monitor"
[pins]
"1" = "in_plus"
"2" = "in_minus"
"3" = "vs"
"4" = "gnd"
"#;

    #[test]
    fn a_shunt_monitors_sense_input_is_not_capped_at_its_supply_and_is_rated() {
        let vs = row("vs", 2.7, 5.5);
        let capped = entry(&format!(
            "{SHUNT}{vs}{}{}",
            row("in_plus", 0.0, 5.5),
            row("in_minus", 0.0, 5.5)
        ));
        let found = entry_problems(&capped);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found
            .iter()
            .all(|p| p.message.contains("no more than the part's own supply")));
        let rated = entry(&format!(
            "{SHUNT}{vs}[[envelope]]\nkind = \"input_tolerance\"\npins = [\"in_plus\", \"in_minus\"]\n\
             max_v = 26.0\nabs_max_v = 28.0\nbasis = \"Recommended Operating Conditions, VIN+\"\n"
        ));
        assert!(entry_problems(&rated).is_empty());
        let unrated = entry(&format!("{SHUNT}{vs}"));
        let found = entry_problems(&unrated);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].message.contains("in_minus, in_plus"));
        // The datasheet itself capping the input at the supply is stated, so kept.
        let stated = entry(&format!(
            "{SHUNT}{vs}[[envelope]]\nkind = \"supply_range\"\npin = \"in_plus\"\nmin_v = 0.0\nmax_v = 5.5\n\
             basis = \"Absolute Maximum Ratings, IN+ to GND: VS + 0.3 V\"\n\
             [[envelope]]\nkind = \"supply_range\"\npin = \"in_minus\"\nmin_v = 0.0\nmax_v = 5.5\n\
             basis = \"Absolute Maximum Ratings, IN- to GND: VS + 0.3 V\"\n"
        ));
        assert!(entry_problems(&stated).is_empty());
        // An amplifier that is not a current sensor keeps its inputs as they are.
        assert!(entry_problems(&entry(&format!("{OPAMP}{}", row("vcc", 4.5, 36.0)))).is_empty());
    }

    #[test]
    fn a_ground_role_on_a_signal_net_is_a_wrong_pad() {
        let card = entry(
            "id = \"u\"\nkind = \"digital\"\n[pins]\n\"1\" = \"vcc\"\n\"2\" = \"gnd\"\n\"3\" = \"sda\"\n",
        );
        let pads = |net: &str| {
            vec![
                ("1".to_string(), "+3V3".to_string()),
                ("2".to_string(), net.to_string()),
                ("3".to_string(), "I2C_SDA".to_string()),
            ]
        };
        let found = board_problems(&card, &pads("SPI_MOSI"));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].area, DoubtArea::Pins);
        assert!(found[0].message.contains("pad 2"));
        for quiet in ["GND", "AGND", "Net-(U1-Pad2)", "SHIELD", "VSS"] {
            assert!(board_problems(&card, &pads(quiet)).is_empty(), "{quiet}");
        }
    }

    #[test]
    fn pad_counts_allow_an_exposed_pad() {
        assert!(pad_counts_agree(8, 8, 9));
        assert!(pad_counts_agree(9, 8, 9));
        assert!(!pad_counts_agree(6, 8, 8));
    }

    #[test]
    fn stamping_writes_doubt_rows_and_a_reload_reads_them() {
        let raw = "# extracted\n[[models]]\nid = \"a\"\nkind = \"digital\"\n[models.source]\ntier = \"datasheet-derived\"\n\
                   [[models]]\nid = \"b\"\nkind = \"digital\"\n";
        let doubt = Doubt {
            area: DoubtArea::Envelope,
            reason: "sense input capped".into(),
        };
        let out = stamp(
            raw,
            &[("a".into(), vec![doubt.clone()]), ("b".into(), vec![])],
        );
        assert!(out.starts_with("# extracted"), "{out}");
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        let rows = &parsed["models"][0]["source"]["doubt"];
        assert_eq!(rows[0]["area"].as_str(), Some("envelope"));
        assert!(parsed["models"][1].get("source").is_none());
        assert_eq!(stamp(raw, &[]), raw);
        assert_eq!(
            stamp(&out, &[("a".into(), vec![doubt])])
                .matches("[[models.source.doubt]]")
                .count(),
            1
        );
    }

    #[test]
    fn a_doubt_covers_only_the_findings_it_is_the_basis_of() {
        use DoubtArea::{Card, Envelope, Pins};
        assert!(covers(Card, Basis::Pins) && covers(Card, Basis::Envelope));
        assert!(covers(Envelope, Basis::Envelope) && !covers(Envelope, Basis::Pins));
        assert!(covers(Pins, Basis::Pins) && covers(Pins, Basis::Envelope));
        let d = Doubt {
            area: Card,
            reason: "low confidence.".into(),
        };
        let note = doubt_note("U1", "c", &d, "U1 is over.");
        assert!(
            is_doubt_note(&note) && note.contains("not a board fault"),
            "{note}"
        );
    }

    #[test]
    fn only_a_user_extracted_card_is_drafted() {
        use ModelSourceTier::{CuratedLibrary, DatasheetDerived};
        assert!(is_drafted(DatasheetDerived, Some(SourceLayer::UserDir)));
        assert!(!is_drafted(DatasheetDerived, Some(SourceLayer::Builtin)));
        assert!(!is_drafted(CuratedLibrary, Some(SourceLayer::UserDir)));
        assert!(!is_drafted(
            ModelSourceTier::UserModel,
            Some(SourceLayer::UserDir)
        ));
    }
}
