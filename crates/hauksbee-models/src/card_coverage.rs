//! What a drafted card must hold for its kind, and whether it binds and pins
//! out as the datasheet and board say: the checks a strong drafter does by
//! hand, run deterministically on every draft.
//!
//! Measured against an expert card, a cheaper drafter's misses fell into
//! three groups: a `value_re` that missed the board's ordering-code spelling
//! (`STM32F407VETx`), pads read from the neighbouring package's column of a
//! multi-package pin table, and whole field groups left out (supply ranges,
//! straps, internal-regulator outputs, theta-JA). Each is a check here:
//!
//! 1. Binding: [`ordering_wildcards`] gives the spellings a board library uses
//!    for the part, and [`findings`] reports a `value_re` or `footprint_re`
//!    that misses the board's value, footprint or those spellings.
//! 2. Pins: the card's pad map against the datasheet's own pin table for the
//!    package the board (or the card) names ([`crate::pin_table`]).
//! 3. Coverage: [`needs`] lists the fields this kind needs and where they
//!    usually live; each must be filled or carried as an uncertainty record.
//!
//! One list, read three ways: the drafting harness feeds the findings back as
//! fix-it messages, the `ds checklist` / `ds check` helpers print them inside
//! the agent's sandbox ([`crate::ds_toolkit`]), and the replay scores saved
//! cards with them.

use std::borrow::Cow;
use std::cell::RefCell;
use std::fmt::Write as _;

use hauksbee_ir::evidence::ModelUncertainty;

use crate::card_trust::Problem;
use crate::pin_table::{self, Column};
use crate::schema::{ComponentKind, Doubt, DoubtArea, ModelEntry, OperatingEnvelope};

/// How a finding is fed back: an error blocks the draft, a warning asks for
/// one redraft and is then accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warn,
}

/// One problem with a draft, worded as the fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    /// Which check found it: `binding`, `pins` or `coverage`.
    pub check: &'static str,
    pub message: String,
}

/// The board's view of the part being drafted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Board {
    pub refdes: String,
    pub value: String,
    pub footprint: String,
    /// (pad, net) pairs, in the board's order.
    pub pads: Vec<(String, String)>,
}

impl Board {
    /// The footprint's distinct pad numbers, in the board's order. A footprint
    /// repeats a number for one electrical pad (an exposed pad's thermal vias
    /// and paste apertures, a TO-263 tab sharing the centre lead's number), and
    /// leaves an unnumbered (mechanical) pad's name empty: neither is another pin.
    #[must_use]
    pub fn distinct_pads(&self) -> Vec<&str> {
        let mut seen = std::collections::HashSet::new();
        (self.pads.iter().map(|(pad, _)| pad.trim()))
            .filter(|pad| !pad.is_empty() && seen.insert(*pad))
            .collect()
    }

    /// The board's pad count: its distinct pad numbers, else the count the
    /// footprint's name says.
    #[must_use]
    pub fn pad_count(&self) -> Option<usize> {
        let n = self.distinct_pads().len();
        (n > 0).then_some(n).or_else(|| {
            crate::package::pin_count(&self.footprint).and_then(|n| usize::try_from(n).ok())
        })
    }

    /// `board.md`: the facts a drafter matches against, one pad per line.
    #[must_use]
    pub fn markdown(&self) -> String {
        let mut out = format!(
            "# Board part\n\nrefdes: {}\nvalue: {}\nfootprint: {}\npads: {}\n",
            self.refdes,
            self.value,
            self.footprint,
            self.pad_count()
                .map_or_else(|| "unknown".into(), |n| n.to_string())
        );
        if !self.pads.is_empty() {
            out.push_str("\n| pad | net |\n|---|---|\n");
            // A pad number repeated on one net (thermal vias) is one row.
            let mut seen = std::collections::HashSet::new();
            for (pad, net) in &self.pads {
                if seen.insert((pad, net)) {
                    let _ = writeln!(out, "| {pad} | {net} |");
                }
            }
        }
        out
    }

    /// Read back [`Board::markdown`].
    #[must_use]
    pub fn from_markdown(text: &str) -> Self {
        let mut b = Self::default();
        for line in text.lines() {
            if let Some((k, v)) = line.split_once(": ") {
                match k {
                    "refdes" => b.refdes = v.trim().into(),
                    "value" => b.value = v.trim().into(),
                    "footprint" => b.footprint = v.trim().into(),
                    _ => {}
                }
            } else if let Some(row) = line.strip_prefix("| ") {
                let cells: Vec<&str> = row
                    .trim_end_matches('|')
                    .split('|')
                    .map(str::trim)
                    .collect();
                if let [pad, net] = cells[..] {
                    if pad != "pad" && !pad.starts_with("--") {
                        b.pads.push((pad.into(), net.into()));
                    }
                }
            }
        }
        b
    }
}

/// What the deterministic checks read: the datasheet's pages and pin table,
/// and the board when the run has one.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub part: String,
    /// Page texts, index 0 is PDF page 1.
    pub pages: Vec<String>,
    pub pin_columns: Vec<Column>,
    pub board: Option<Board>,
    /// Companion documents fetched for the run (an application note the
    /// datasheet defers to): each one's name and page texts.
    pub companions: Vec<(String, Vec<String>)>,
}

impl Facts {
    /// Every page a number may be read from: the datasheet's, and the
    /// companions' pages that are about what they were fetched for (a whole
    /// reference manual prints nearly every small number somewhere, so its
    /// other pages would vouch for anything).
    #[must_use]
    pub fn workspace_pages(&self) -> Vec<&str> {
        self.pages
            .iter()
            .map(String::as_str)
            .chain(
                self.companions
                    .iter()
                    .flat_map(|(_, pages)| pages.iter().map(String::as_str))
                    .filter(|p| crate::companion_docs::on_topic(p)),
            )
            .collect()
    }
}

thread_local! {
    static FACTS: RefCell<Option<Facts>> = const { RefCell::new(None) };
}

/// Hold `facts` for the checks run while `f` runs on this thread (the
/// drafting loop's validation hooks read them), and drop them after.
pub fn with_facts<T>(facts: Facts, f: impl FnOnce() -> T) -> T {
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            FACTS.with(|c| c.borrow_mut().take());
        }
    }
    FACTS.with(|c| *c.borrow_mut() = Some(facts));
    let _clear = Clear;
    f()
}

/// The findings for `entry` against the facts [`with_facts`] holds; none
/// outside such a run.
#[must_use]
pub fn current_findings(entry: &ModelEntry) -> Vec<Finding> {
    FACTS.with(|c| {
        c.borrow()
            .as_ref()
            .map(|f| findings(entry, f))
            .unwrap_or_default()
    })
}

/// The spellings a board library or BOM gives an orderable part besides its full code.
///
/// The temperature digit becomes `x` (`STM32F407VET6` -> `STM32F407VETx`,
/// `STM32F407VET`), the way `KiCad`'s MCU symbols name them.
#[must_use]
pub fn ordering_wildcards(part: &str) -> Vec<String> {
    let part = part.trim();
    let mut out = Vec::new();
    let b = part.as_bytes();
    // <letters+digits><letter><digit>: an ST/NXP-style temperature digit.
    if b.len() >= 8
        && b[b.len() - 1].is_ascii_digit()
        && b[b.len() - 2].is_ascii_alphabetic()
        && b[..b.len() - 2].iter().any(u8::is_ascii_digit)
        && part.starts_with(|c: char| c.is_ascii_alphabetic())
        && part.chars().all(|c| c.is_ascii_alphanumeric())
    {
        let stem = &part[..part.len() - 1];
        out.push(format!("{stem}x"));
        out.push(stem.to_string());
    }
    out
}

fn kind_name(kind: ComponentKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// One field group a kind needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    /// Short key a missing-field record names (`supply_range:vdd`).
    pub key: String,
    /// What to write, in card terms.
    pub ask: String,
    /// The datasheet section it usually comes from.
    pub look: &'static str,
}

const LOOK_ROC: &str = "Recommended / general operating conditions";
const LOOK_ABS: &str = "Absolute maximum ratings";
const LOOK_THERMAL: &str = "Thermal information / package characteristics";

fn is_output_role(role: &str) -> bool {
    let r = role.to_ascii_lowercase();
    matches!(
        r.as_str(),
        "out" | "vout" | "sw" | "lx" | "fb" | "bst" | "boot"
    ) || r.starts_with("vcap")
        || r.starts_with("vout")
}

/// The supply roles a card's pins name: the primary rails, a battery-backup
/// rail, and a regulator's input.
fn supply_roles(entry: &ModelEntry) -> Vec<String> {
    let mut roles: Vec<String> = entry
        .pins
        .values()
        .map(|r| r.trim().to_ascii_lowercase())
        .filter(|r| {
            let power = crate::pin_roles::power_pin(r, entry.kind)
                .is_some_and(|p| p.kind == crate::pin_roles::PowerKind::Supply);
            let vreg_in = entry.kind == ComponentKind::Vreg
                && matches!(r.as_str(), "in" | "vin" | "pvin" | "avin" | "vin1");
            power || vreg_in || r == "vbat"
        })
        .collect();
    roles.sort();
    roles.dedup();
    roles
}

/// The field groups `entry`'s kind (and the roles it names) needs, with where each usually lives.
///
/// `text` is the datasheet's text, for needs that only apply when the part
/// has the feature (boot straps); `columns` is its pin table, which says
/// whether the pinout differs by package.
#[must_use]
pub fn needs(entry: &ModelEntry, text: &str, columns: &[Column]) -> Vec<Need> {
    let mut out = Vec::new();
    let pinouts_differ = columns.iter().any(|a| {
        columns
            .iter()
            .any(|b| b.max_pad() == a.max_pad() && !pin_table::same_map(a, b))
    }) || {
        let mut counts: Vec<usize> = columns.iter().map(Column::max_pad).collect();
        counts.sort_unstable();
        counts.dedup();
        counts.len() > 1
    };
    if pinouts_differ {
        out.push(Need {
            key: "footprint_re".into(),
            ask: "[models.match] footprint_re naming this package, since the pinout differs by \
                  package"
                .into(),
            look: "Pin table / package information",
        });
    }
    let roles = supply_roles(entry);
    for role in &roles {
        out.push(Need {
            key: format!("supply_range:{role}"),
            ask: format!(
                "[[models.envelope]] kind = \"supply_range\", pin = \"{role}\", min_v/max_v from \
                 the recommended operating row"
            ),
            look: LOOK_ROC,
        });
    }
    if !roles.is_empty() {
        out.push(Need {
            key: "abs_max_v".into(),
            ask: "abs_max_v on each supply_range (or ratings.max_voltage_v) from the absolute \
                  maximum supply row"
                .into(),
            look: LOOK_ABS,
        });
    }
    if !matches!(
        entry.kind,
        ComponentKind::Passive | ComponentKind::Connector | ComponentKind::Ignore
    ) {
        out.push(Need {
            key: "theta_ja".into(),
            ask: "[models.ratings] theta_ja_c_per_w for this package".into(),
            look: LOOK_THERMAL,
        });
    }
    if says_supply_pin_becomes_output(text) {
        out.push(Need {
            key: "supply_outputs:dual_role".into(),
            ask: "[[models.supply_outputs]] for each supply pin the datasheet says is an output \
                  in one supply mode (the on-chip regulator's output), with `when = { pin = \
                  \"<the pin that selects the mode>\", min_v = <lowest level for that mode> }` \
                  so the rail counts as sourced only in that mode"
                .into(),
            look: "Reference circuits / power supply scheme / pin descriptions",
        });
    }
    match entry.kind {
        ComponentKind::Mcu => mcu_needs(entry, text, &mut out),
        ComponentKind::Vreg => vreg_needs(entry, &mut out),
        _ => {}
    }
    open_input_needs(entry, text, &mut out);
    analog_input_needs(entry, text, &mut out);
    out
}

/// The datasheet states what an input does when left open: an internal pull or
/// a default ("when `INx` is open, the output goes to its default state", "H or
/// open", "internal pull-up").
static OPEN_INPUT_STATE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)\b(?:[HL]\s+or\s+open|open\s+or\s+[HL])\b|internal(?:ly)?\s+(?:weak\s+)?pull[- ]?(?:up|down)|\bis\s+open\b[^.]{0,120}\bdefault\b|\bdefault\b[^.]{0,120}\b(?:when|if)\b[^.]{0,40}\bopen\b",
    )
    .expect("open-input regex")
});

/// An analog input pin's datasheet table prints an input impedance row.
static INPUT_IMPEDANCE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)\binput\s+(?:impedance|resistance)\b").expect("input-impedance regex")
});

/// An amplifier card with analog inputs must say whether the part biases them
/// internally (an audio or class-D amplifier input behind a coupling
/// capacitor), or the no-DC-path check cannot tell it from a bare op-amp input.
fn analog_input_needs(entry: &ModelEntry, text: &str, out: &mut Vec<Need>) {
    let analog = entry.pins.values().any(|r| {
        let r = r.to_ascii_lowercase();
        r.starts_with("in_plus") || r.starts_with("in_minus")
    });
    if !analog || !INPUT_IMPEDANCE.is_match(text) {
        return;
    }
    out.push(Need {
        key: "input_dc_path".into(),
        ask: "[[models.input_dc_path]] role/to/ohms for each analog input the part biases \
              internally (the datasheet's input impedance is a resistance to a bias, ground or \
              supply, as for an audio or class-D amplifier); a plain high-impedance op-amp input \
              needs none, so say that in an `unknown` record whose reason names the row"
            .into(),
        look: "Electrical characteristics (input impedance) / application circuit",
    });
}

/// A logic card must record which inputs the datasheet gives an internal pull
/// or a default when open, or the floating-input check calls an isolator's
/// open `INx` (documented, supported) a fault.
fn open_input_needs(entry: &ModelEntry, text: &str, out: &mut Vec<Need>) {
    if entry.logic.inputs.is_empty() || !OPEN_INPUT_STATE.is_match(text) {
        return;
    }
    out.push(Need {
        key: "pulled_inputs".into(),
        ask: "[models.logic] pulled_inputs listing every input that has an internal pull \
              resistor or a stated default when open (an isolator's INx, an enable that is \
              \"H or open\"), by the roles named in `inputs`; an empty list only when the \
              datasheet says every input must be driven"
            .into(),
        look: "Pin functions / function table (open-input default)",
    });
}

/// Whether the datasheet says a supply pin is an output in one supply mode
/// ("VCCD becomes an output", "is an output when ... is fed 5 V").
fn says_supply_pin_becomes_output(text: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\bbecomes?\s+(?:an?\s+)?output\b|\b(?:is|acts\s+as)\s+an?\s+(?:supply\s+)?output\s+(?:only\s+)?(?:when|if|while)\b",
        )
        .expect("static regex")
    })
    .is_match(text)
}

/// What a card of `kind` needs before any draft exists (the checklist's groups).
#[must_use]
pub fn kind_needs(kind: ComponentKind, text: &str, columns: &[Column]) -> Vec<Need> {
    let entry = ModelEntry {
        kind,
        ..ModelEntry::default()
    };
    needs(&entry, text, columns)
}

fn has_role(entry: &ModelEntry, pred: &dyn Fn(&str) -> bool) -> bool {
    entry.pins.values().any(|r| pred(&r.to_ascii_lowercase()))
}

fn mcu_needs(entry: &ModelEntry, text: &str, out: &mut Vec<Need>) {
    let has = |pred: &dyn Fn(&str) -> bool| has_role(entry, pred);
    out.push(Need {
        key: "backend".into(),
        ask: format!(
            "params.backend naming the simulator core{}",
            suggest_backend(&entry.id)
                .or_else(|| suggest_backend(entry.r#match.value_re.as_deref().unwrap_or("")))
                .map_or_else(String::new, |b| format!(" (this family: \"{b}\")"))
        ),
        look: "Ordering information / device overview",
    });
    let lower = text.to_ascii_lowercase();
    if lower.contains("boot0") || lower.contains("boot mode") || lower.contains("strap") {
        out.push(Need {
            key: "straps".into(),
            ask: "[[models.straps]] for each boot/strap pin (role, level, only_with) the \
                  boot-mode table names, each with vil/vih and `supply` set to the supply \
                  role the datasheet's power-domain table puts that pad on (the IO supply, \
                  not the core supply, for a GPIO or strap pad)"
                .into(),
            look: "Boot configuration / boot modes",
        });
    }
    if has(&|r| r.starts_with("vref")) && has(&|r| r == "vdda") {
        out.push(Need {
            key: "rail_order:vref".into(),
            ask: "[[models.envelope]] kind = \"rail_order\" with the VREF+ role at or \
                  below vdda"
                .into(),
            look: LOOK_ROC,
        });
    }
    out.push(Need {
        key: "internal_clock".into(),
        ask: "params.internal_clock: the internal oscillator the part runs on after reset \
              (e.g. a 16 MHz RC), from the clock characteristics"
            .into(),
        look: "Internal clock source characteristics",
    });
    if has(&|r| r == "boot0") {
        out.push(Need {
            key: "input_tolerance:boot0".into(),
            ask: "[[models.envelope]] kind = \"input_tolerance\" for boot0 (its own \
                  maximum input voltage)"
                .into(),
            look: "Absolute maximum ratings / I/O static characteristics",
        });
    }
    if text.trim().is_empty() || lower.contains("adc") || lower.contains("analog-to-digital") {
        out.push(Need {
            key: "adc_pins".into(),
            ask: "[models.pin_capability.adc] listing every pad of [models.pins] that is an \
                  ADC input with its channel (a part with no ADC: an empty table), basis \
                  citing the pin-definition table and page"
                .into(),
            look: "Pin definitions / analog function table",
        });
    }
    for (peripheral, name) in entry.named_peripherals() {
        if let crate::schema::NamedPeripheral::Function(f) = peripheral {
            out.push(Need {
                key: format!("pin_function:{}", f.key()),
                ask: format!(
                    "[models.pin_capability.{key}] listing every pad of [models.pins] that can \
                     carry {label} (an empty table when the package has none), basis citing the \
                     alternate-function table and page; or, for a part that routes it to any \
                     pad, {key} under [models.pin_capability.routed]. The card names it at {name}",
                    key = f.key(),
                    label = f.label(),
                ),
                look: "Alternate function mapping / pin definitions",
            });
        }
    }
    if has(&|r| r.starts_with("vcap")) {
        out.push(Need {
            key: "supply_outputs:vcap".into(),
            ask: "[[models.supply_outputs]] for each VCAP pin (the internal regulator's \
                  output, never a supply_range)"
                .into(),
            look: "Power supply scheme / embedded regulator",
        });
    }
}

fn vreg_needs(entry: &ModelEntry, out: &mut Vec<Need>) {
    let has = |pred: &dyn Fn(&str) -> bool| has_role(entry, pred);
    if has(&|r| matches!(r, "out" | "vout")) {
        out.push(Need {
            key: "supply_outputs:out".into(),
            ask: "[[models.supply_outputs]] role = \"out\" with nominal/min/max from the \
                  output-accuracy row"
                .into(),
            look: "Electrical characteristics",
        });
    }
    if has(&|r| matches!(r, "en" | "enable" | "shdn" | "en_n" | "shdn_n")) {
        out.push(Need {
            key: "enable".into(),
            ask: "the enable pin's limits: a rail_order (en at or below in) when the \
                  datasheet says EN must not exceed VIN, and whether it may float \
                  (params.must_not_float_roles, or an `unknown` record for en_open)"
                .into(),
            look: "Pin functions / enable description",
        });
        out.push(Need {
            key: "i_shdn_a".into(),
            ask: "params.i_shdn_a: the shutdown (EN low) supply current".into(),
            look: "Electrical characteristics",
        });
    }
}

/// The built-in MCU backend whose core name begins `part`, longest first.
#[must_use]
pub fn suggest_backend(part: &str) -> Option<String> {
    let squashed: String = part
        .to_ascii_lowercase()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    crate::ModelLibrary::builtin_shared()
        .mcu_backends()
        .into_iter()
        .filter(|b| {
            let core = b.split_once(':').map_or(b.as_str(), |(_, c)| c);
            core.len() >= 4 && squashed.contains(core)
        })
        .max_by_key(String::len)
}

/// Whether the card has `need`: the field itself, or an uncertainty record
/// naming it (the drafter looked and the datasheet has no row).
#[must_use]
pub fn satisfied(entry: &ModelEntry, need: &Need) -> bool {
    has_field(entry, need) || recorded(entry, need)
}

/// An uncertainty record names `need` (the drafter says the datasheet has no row).
fn recorded(entry: &ModelEntry, need: &Need) -> bool {
    entry.source.as_ref().is_some_and(|s| {
        let tail = need.key.split(':').next_back().unwrap_or(&need.key);
        let head = need.key.split(':').next().unwrap_or(&need.key);
        s.uncertainty.iter().any(|u| {
            let (ModelUncertainty::Interval { parameter, .. }
            | ModelUncertainty::Unknown { parameter, .. }) = u;
            let p = parameter.to_ascii_lowercase();
            p.contains(&need.key) || p.contains(head) || (tail.len() > 2 && p.contains(tail))
        })
    })
}

/// The card carries the field `need` asks for.
fn has_field(entry: &ModelEntry, need: &Need) -> bool {
    let ranges = || {
        entry.envelope.iter().filter_map(|e| match e {
            OperatingEnvelope::SupplyRange { pin, abs_max_v, .. } => {
                Some((pin.to_ascii_lowercase(), *abs_max_v))
            }
            _ => None,
        })
    };
    match need.key.split_once(':').unwrap_or((need.key.as_str(), "")) {
        ("supply_range", role) => ranges().any(|(p, _)| p == role),
        ("abs_max_v", _) => entry.ratings.max_voltage_v.is_some() || ranges().any(|(_, a)| a.is_some()),
        ("theta_ja", _) => entry.ratings.theta_ja_c_per_w.is_some(),
        ("footprint_re", _) => entry.r#match.footprint_re.is_some() || entry.variant_of.is_some(),
        ("internal_clock" | "i_shdn_a", _) => entry.params.0.contains_key(need.key.as_str()),
        ("input_tolerance", role) => entry.envelope.iter().any(|e| {
            matches!(e, OperatingEnvelope::InputTolerance { pins, .. }
                if pins.iter().any(|p| p.eq_ignore_ascii_case(role)))
        }),
        ("backend", _) => entry.params.get_str("backend").is_some(),
        ("pulled_inputs", _) => !entry.logic.pulled_inputs.is_empty(),
        ("input_dc_path", _) => !entry.input_dc_path.is_empty(),
        ("straps", _) => !entry.straps.is_empty(),
        ("adc_pins", _) => entry.adc_declared(),
        ("pin_function", key) => hauksbee_ir::pin_functions::PeripheralFunction::ALL
            .iter()
            .find(|f| f.key() == key)
            .is_some_and(|f| entry.pin_capability.declares(*f)),
        ("rail_order", _) => entry
            .envelope
            .iter()
            .any(|e| matches!(e, OperatingEnvelope::RailOrder { .. })),
        ("supply_outputs", "dual_role") => !entry.supply_outputs.is_empty(),
        ("supply_outputs", role) => entry
            .supply_outputs
            .iter()
            .any(|o| o.role.to_ascii_lowercase().starts_with(role)),
        ("enable", _) => {
            entry.envelope.iter().any(|e| {
                matches!(e, OperatingEnvelope::RailOrder { lower, .. } if lower.eq_ignore_ascii_case("en"))
            }) || entry.params.get_str("must_not_float_roles").is_some()
        }
        _ => false,
    }
}

/// Wording that shows the datasheet does print a need's row, for the needs
/// a drafter most often records as absent without looking.
fn evidence(key: &str) -> Option<&'static str> {
    Some(match key {
        "theta_ja" => {
            r"(?i)(θ|Θ|theta|R\s*θ|R\s*th)\s*_?\s*JA\b|junction[- ]to[- ]ambient|junction-ambient"
        }
        "abs_max_v" => r"(?i)absolute\s+maximum\s+ratings",
        "internal_clock" => r"(?i)internal\s+(rc\s+)?oscillator|internal\s+clock|\bRC\s+oscillator",
        "i_shdn_a" => r"(?i)shutdown\s+(supply\s+)?current|\bI\s*_?\s*(SHDN|SD)\b",
        "pulled_inputs" => {
            r"(?i)\b(?:[HL]\s+or\s+open|open\s+or\s+[HL])\b|internal(?:ly)?\s+(?:weak\s+)?pull[- ]?(?:up|down)"
        }
        _ => return None,
    })
}

/// The first PDF page (1-based) whose text matches `need`'s evidence.
fn printed_on(need: &Need, pages: &[String]) -> Option<usize> {
    let re = regex::Regex::new(evidence(&need.key)?).ok()?;
    // Skip contents pages (dotted leaders): they name a section, not its row.
    pages
        .iter()
        .position(|p| re.is_match(p) && !p.contains("....."))
        .map(|i| i + 1)
}

/// Every finding for `entry` against `facts`.
#[must_use]
pub fn findings(entry: &ModelEntry, facts: &Facts) -> Vec<Finding> {
    let mut out = binding(entry, facts);
    out.extend(pins(entry, facts));
    out.extend(trust_findings(entry, facts));
    let text = facts.pages.join("\n");
    // Without the datasheet's text there is nothing to say a field is there
    // to read, so a missing group is no finding.
    let needs = if text.trim().is_empty() {
        Vec::new()
    } else {
        needs(entry, &text, &facts.pin_columns)
    };
    for need in needs {
        if has_field(entry, &need) {
            continue;
        }
        if recorded(entry, &need) {
            if let Some(page) = printed_on(&need, &facts.pages) {
                out.push(Finding {
                    severity: Severity::Warn,
                    check: "coverage",
                    message: format!(
                        "{} is recorded as not in the datasheet, but PDF page {page} prints it \
                         (`ds page {page}`): add {} from there, or say in the record's reason why \
                         that row does not apply",
                        need.key, need.ask
                    ),
                });
            }
        } else {
            out.push(Finding {
                severity: Severity::Warn,
                check: "coverage",
                message: format!(
                    "{} is missing: add {} (usually under \"{}\"; `ds find` locates it), or a \
                     [[models.source.uncertainty]] `unknown` record with parameter = \"{}\" whose \
                     reason says the datasheet has no such row",
                    need.key, need.ask, need.look, need.key
                ),
            });
        }
    }
    for e in &entry.envelope {
        if let OperatingEnvelope::SupplyRange { pin, .. } = e {
            if is_output_role(pin) {
                out.push(Finding {
                    severity: Severity::Error,
                    check: "coverage",
                    message: format!(
                        "supply_range on '{pin}', an output the part drives: a supply_range is \
                         for a pin the board supplies; give '{pin}' a [[models.supply_outputs]] \
                         row instead"
                    ),
                });
            }
        }
    }
    out.extend(boot_straps(entry, facts));
    out.extend(strap_domain(entry));
    out.extend(crate::card_scope::entry_findings(entry, facts));
    out.extend(crate::card_provenance::findings(
        entry,
        &facts.workspace_pages(),
    ));
    let mut seen = std::collections::HashSet::new();
    out.retain(|f| seen.insert(f.message.clone()));
    out
}

/// The findings for every entry of a reply: the first entry's full set, the
/// scope and provenance checks on the others (named by id), and whether the
/// reply covers every package the pin table lists.
#[must_use]
pub fn reply_findings(entries: &[ModelEntry], facts: &Facts) -> Vec<Finding> {
    let Some((first, rest)) = entries.split_first() else {
        return Vec::new();
    };
    let mut out = findings(first, facts);
    let pages = facts.workspace_pages();
    for e in rest {
        let mut more = crate::card_scope::entry_findings(e, facts);
        more.extend(crate::card_provenance::findings(e, &pages));
        more.extend(trust_findings(e, facts));
        for f in &mut more {
            f.message = format!("[[models]] '{}': {}", e.id, f.message);
        }
        out.extend(more);
    }
    out.extend(crate::card_scope::package_coverage(entries, facts));
    out.extend(crate::card_scope::theta_coverage(entries, facts));
    out
}

/// [`reply_findings`] against the facts [`with_facts`] holds.
#[must_use]
pub fn current_reply_findings(entries: &[ModelEntry]) -> Vec<Finding> {
    FACTS.with(|c| {
        c.borrow()
            .as_ref()
            .map(|f| reply_findings(entries, f))
            .unwrap_or_default()
    })
}

/// The `check` name a [`crate::card_trust::Problem`] is reported under.
const fn trust_check(area: DoubtArea) -> &'static str {
    match area {
        DoubtArea::Card => "trust-card",
        DoubtArea::Pins => "trust-pins",
        DoubtArea::Envelope => "trust-envelope",
    }
}

/// Remedy 4: the drafted-card mistakes behind false alarms.
///
/// See [`crate::card_trust`]: a pad count that is not the footprint's, a power or
/// return role on a pad the board wires as a signal, a split-supply op-amp limit
/// written as absolute numbers, a shunt monitor's sense input capped at its supply.
#[must_use]
pub fn trust_problems(entry: &ModelEntry, facts: &Facts) -> Vec<crate::card_trust::Problem> {
    let mut out = crate::card_trust::entry_problems(entry);
    if let Some(board) = &facts.board {
        out.extend(crate::card_trust::board_problems(entry, &board.pads));
        out.extend(pad_count_problem(entry, board, Some(facts)));
    }
    out
}

fn trust_findings(entry: &ModelEntry, facts: &Facts) -> Vec<Finding> {
    trust_problems(entry, facts)
        .into_iter()
        .map(|p| Finding {
            severity: Severity::Warn,
            check: trust_check(p.area),
            message: p.message,
        })
        .collect()
}

/// The pin count of the package column the card chose against the pad count of
/// the footprint it binds: a card read from the six-pin column, bound to an
/// eight-pad part, has every pad shifted.
fn pad_count_problem(
    entry: &ModelEntry,
    board: &Board,
    facts: Option<&Facts>,
) -> Option<crate::card_trust::Problem> {
    if entry.pins.is_empty()
        || re_matches(entry.r#match.footprint_re.as_deref(), &board.footprint) == Some(false)
    {
        return None;
    }
    let total = board.pad_count()?;
    let numeric = (board.distinct_pads().into_iter())
        .filter(|p| p.chars().all(|c| c.is_ascii_digit()))
        .count();
    let numeric = if numeric == 0 { total } else { numeric };
    let package = crate::validation::pin_package(entry)
        .map(str::to_owned)
        .or_else(|| {
            let source = entry.source.as_ref()?;
            source.pinout.first().map(|p| p.package.clone())
        });
    let column = package
        .as_deref()
        .zip(facts)
        .and_then(|(p, f)| pin_table::choose(&f.pin_columns, Some(p), None).ok());
    let card_max = entry
        .pins
        .keys()
        .filter_map(|p| p.parse::<usize>().ok())
        .max();
    let (chosen, from) = column.map_or_else(
        || card_max.map(|n| (n, "its highest pad".to_string())),
        |c| Some((c.max_pad(), format!("the {} column", c.label))),
    )?;
    (!crate::card_trust::pad_counts_agree(chosen, numeric, total)).then(|| {
        crate::card_trust::Problem {
            area: DoubtArea::Card,
            message: format!(
                "the card's pad map is read from {from} ({chosen} pins) but the footprint '{}' \
                 has {total} pads: the pins belong to another package. Read the pin-table column \
                 for this footprint's package (`ds pins --package` prints each), or gate the card \
                 with footprint_re",
                board.footprint
            ),
        }
    })
}

/// The bind-time form of the pad-count check: with no datasheet to hand, the
/// card's highest pad against the pads of the footprint it is bound to.
#[must_use]
pub fn bound_pad_problem(entry: &ModelEntry, board: &Board) -> Option<Problem> {
    pad_count_problem(entry, board, None)
}

/// What a drafted card left unconfirmed, for its `[[models.source.doubt]]` rows:
/// every trust problem still on it, and a pad map that disagrees with the
/// datasheet's column.
#[must_use]
pub fn doubts(entry: &ModelEntry, facts: &Facts) -> Vec<Doubt> {
    let mut out: Vec<Doubt> = trust_problems(entry, facts)
        .iter()
        .map(Problem::doubt)
        .collect();
    out.extend(pins(entry, facts).into_iter().map(|f| Doubt {
        area: DoubtArea::Pins,
        reason: f.message,
    }));
    out
}

/// [`doubts`] of each entry against the facts [`with_facts`] holds, keyed by id.
#[must_use]
pub fn current_doubts(entries: &[ModelEntry]) -> Vec<(String, Vec<Doubt>)> {
    FACTS.with(|c| {
        let facts = c.borrow();
        entries
            .iter()
            .map(|e| {
                let found = facts.as_ref().map(|f| doubts(e, f)).unwrap_or_default();
                (e.id.clone(), found)
            })
            .collect()
    })
}

fn re_matches(pattern: Option<&str>, s: &str) -> Option<bool> {
    let p = pattern?;
    Some(regex::Regex::new(&format!("(?i){p}")).is_ok_and(|re| re.is_match(s)))
}

/// Remedy 1: the card binds the board's value and footprint and the part's
/// ordering-code spellings.
fn binding(entry: &ModelEntry, facts: &Facts) -> Vec<Finding> {
    let rules = &entry.r#match;
    let mut out = Vec::new();
    let mut want: Vec<String> = ordering_wildcards(&facts.part);
    if let Some(b) = &facts.board {
        if !b.value.is_empty() {
            want.insert(0, b.value.clone());
        }
        if re_matches(rules.footprint_re.as_deref(), &b.footprint) == Some(false) {
            let pattern = rules.footprint_re.as_deref().unwrap_or("");
            // Measured: a doubled backslash (`P0\\\\.5mm`) matched a literal backslash, and
            // the fix that followed drew the next warning, a round later.
            let doubled = if pattern.contains("\\\\") {
                "; a doubled backslash matches a literal backslash (in a TOML basic string \
                 write `\\\\.` for a dot, or use a '...' literal string and a single one)"
            } else {
                ""
            };
            let family = crate::card_scope::family_pattern(entry, facts)
                .map(|p| format!(", e.g. '{p}'"))
                .unwrap_or_default();
            out.push(Finding {
                severity: Severity::Warn,
                check: "binding",
                message: format!(
                    "footprint_re '{pattern}' does not match the board footprint '{}', so the \
                     card would not bind this part: widen it to the package family{family} (or \
                     drop it when one pinout serves every package){doubled}",
                    b.footprint
                ),
            });
        }
    }
    let mut seen = std::collections::HashSet::new();
    want.retain(|v| seen.insert(v.to_ascii_lowercase()));
    let misses: Vec<String> = want
        .into_iter()
        .filter(|v| re_matches(rules.value_re.as_deref(), v) == Some(false))
        .collect();
    if !misses.is_empty() {
        out.push(Finding {
            severity: Severity::Warn,
            check: "binding",
            message: format!(
                "value_re '{}' does not match {}, the way a board names this part: widen it, \
                 e.g. a trailing optional group such as (T[67]?|Tx)?",
                rules.value_re.as_deref().unwrap_or(""),
                misses
                    .iter()
                    .map(|m| format!("'{m}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    out
}

/// Whether a card role agrees with a printed pin name.
#[must_use]
pub fn role_agrees(role: &str, printed: &str, kind: ComponentKind) -> bool {
    let tokens = pin_table::name_tokens(printed);
    let r = pin_table::norm(role);
    let numbered = |s: &str| s.trim_end_matches(|c: char| c.is_ascii_digit()).to_string();
    let bare = role
        .rsplit_once('_')
        .filter(|(_, n)| n.chars().all(|c| c.is_ascii_digit()))
        .map_or_else(|| r.clone(), |(b, _)| pin_table::norm(b));
    let family = |s: &str| crate::pin_roles::power_pin(s, kind).map(|p| (p.kind, p.family));
    // A printed name cut short by a narrow cell or a page break (`VDD_SEN` for
    // `VDD_SENSE`) is no evidence of disagreement. A cut counts only inside an
    // alphabetic word, never at a digit: `GPIO1` for `gpio12` is a different pad.
    let cut = |t: &str, full: &str| {
        t.len() >= 3
            && full.starts_with(t)
            && t.ends_with(|c: char| c.is_ascii_alphabetic())
            && full[t.len()..].starts_with(|c: char| c.is_ascii_alphabetic())
    };
    tokens.iter().any(|t| {
        *t == r
            || *t == bare
            || cut(t, &r)
            || cut(t, &bare)
            || (family(&r).is_some() && family(&r) == family(t))
            || (family(&bare).is_some() && family(&bare) == family(&numbered(t)))
            || (matches!(r.as_str(), "nc" | "dnc") && t.starts_with("nc"))
            || (matches!(r.as_str(), "ep" | "pad" | "thermal") && t.contains("pad"))
    })
}

/// Every pad the pin table names for a boot-select pin (`PB2/BOOT1`, `BOOT0`)
/// has a `[[models.straps]]` row: one strap row for `BOOT0` leaves the other
/// pin of the boot-mode table unmodelled.
fn boot_straps(entry: &ModelEntry, facts: &Facts) -> Vec<Finding> {
    static BOOT_PIN: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)\bn?BOOT\d\b|/n?BOOT\d").expect("re"));
    if entry.straps.is_empty() {
        return Vec::new();
    }
    let Some(col) = board_column(entry, facts) else {
        return Vec::new();
    };
    let strapped = |role: &str| {
        entry
            .straps
            .iter()
            .any(|s| s.role.eq_ignore_ascii_case(role))
    };
    let missing: Vec<String> = entry
        .pins
        .iter()
        .filter_map(|(pad, role)| {
            let (printed, _) = col.pads.get(pad)?;
            (BOOT_PIN.is_match(printed) && !strapped(role))
                .then(|| format!("pad {pad} '{printed}' (role '{role}')"))
        })
        .collect();
    if missing.is_empty() {
        return Vec::new();
    }
    let hint = if facts.companions.is_empty() {
        "the boot-mode table"
    } else {
        "the boot-mode table (a companion document holds it: `ds find 'boot configuration'`, then `ds page N --doc DOC`)"
    };
    vec![Finding {
        severity: Severity::Warn,
        check: "coverage",
        message: format!(
            "the pin table names {} for boot selection but the card has no [[models.straps]] row \
             for it: add one (role, level, only_with) from {hint}, or an `unknown` record naming \
             the pin saying why",
            missing.join(", ")
        ),
    }]
}

/// Whether a supply role is an IO-domain supply (`iovdd`, `vddio`, `vccio`, `vio`).
fn is_io_supply(role: &str) -> bool {
    crate::pin_roles::power_pin(role, ComponentKind::Mcu).is_some_and(|p| {
        p.kind == crate::pin_roles::PowerKind::Supply
            && (p.family.starts_with("io") || p.family.ends_with("io") || p.family == "vio")
    })
}

/// A strap's thresholds are read at the supply its pad is powered from. A card
/// that has both a core supply and an IO supply, and scales a strap's `vil`/`vih`
/// on the core one, reads every divider on that pad at the wrong rail (a 1.2 V
/// core against a 3.3 V strap): the datasheet's power-domain table says which.
fn strap_domain(entry: &ModelEntry) -> Vec<Finding> {
    let io: Vec<&str> = entry
        .pins
        .values()
        .map(String::as_str)
        .filter(|r| is_io_supply(r))
        .collect();
    let Some(io_supply) = io.first().copied() else {
        return Vec::new();
    };
    let wrong: Vec<String> = entry
        .straps
        .iter()
        .filter(|s| (s.vil.is_some() || s.vih.is_some()) && s.supply.is_none())
        .filter(|s| !is_io_supply(s.supply_role(entry)))
        .map(|s| format!("'{}' (read at '{}')", s.role, s.supply_role(entry)))
        .collect();
    if wrong.is_empty() {
        return Vec::new();
    }
    vec![Finding {
        severity: Severity::Warn,
        check: "coverage",
        message: format!(
            "strap {} scales its vil/vih on the core supply, but the card also has the IO supply \
             '{io_supply}': read the datasheet's power-domain table for the supply each strap \
             pad is powered from. If it is the IO supply (usual for GPIO and strap pads), add \
             `supply = \"{io_supply}\"` to the strap row; if the table puts the pad on the core \
             supply, keep it and set `supply` to that role so the choice is explicit",
            wrong.join(", ")
        ),
    }]
}

/// The pin-table column for the package the board (or, failing that, the card) is for.
fn board_column<'a>(entry: &ModelEntry, facts: &'a Facts) -> Option<Cow<'a, Column>> {
    if facts.pin_columns.is_empty() || entry.pins.is_empty() {
        return None;
    }
    let card_max = entry
        .pins
        .keys()
        .filter_map(|p| p.parse::<usize>().ok())
        .max();
    let package = facts
        .board
        .as_ref()
        .map(|b| b.footprint.clone())
        .filter(|f| !f.is_empty())
        .or_else(|| entry.pin_package.clone())
        .or_else(|| {
            entry
                .source
                .as_ref()
                .and_then(|s| s.pinout.first().map(|p| p.package.clone()))
        });
    if let Some(board) = facts.board.as_ref().filter(|b| !b.pads.is_empty()) {
        // The board's own pads: a table without the exposed pad still fits.
        let numbers: Vec<&str> = board.pads.iter().map(|(p, _)| p.as_str()).collect();
        return pin_table::choose_for_board(&facts.pin_columns, package.as_deref(), &numbers).ok();
    }
    let pads = facts.board.as_ref().and_then(Board::pad_count).or(card_max);
    pin_table::choose(&facts.pin_columns, package.as_deref(), pads)
        .ok()
        .map(Cow::Borrowed)
}

/// Remedy 2: the pad map against the datasheet's column for this package.
fn pins(entry: &ModelEntry, facts: &Facts) -> Vec<Finding> {
    if facts.pin_columns.is_empty() || entry.pins.is_empty() {
        return Vec::new();
    }
    let card_max = entry
        .pins
        .keys()
        .filter_map(|p| p.parse::<usize>().ok())
        .max();
    let Some(col) = board_column(entry, facts) else {
        return Vec::new();
    };
    let mut wrong = Vec::new();
    for (pad, role) in &entry.pins {
        if let Some((printed, page)) = col.pads.get(pad) {
            if !role_agrees(role, printed, entry.kind) {
                wrong.push(format!(
                    "pad {pad}: card '{role}', table '{printed}' (p.{page})"
                ));
            }
        }
    }
    if wrong.is_empty() {
        return Vec::new();
    }
    let sure = col.is_whole() && card_max == Some(col.max_pad());
    let n = wrong.len();
    wrong.truncate(12);
    vec![Finding {
        severity: if sure && n >= 2 {
            Severity::Error
        } else {
            Severity::Warn
        },
        check: "pins",
        message: format!(
            "{n} pad(s) disagree with the datasheet's {} column (`ds pins --package {}` prints \
             it): {}{}. Read each pad from that column, not a neighbouring package's",
            col.label,
            col.label,
            wrong.join("; "),
            if n > 12 { "; ..." } else { "" }
        ),
    }]
}

/// The checklist `ds checklist` prints for a kind before any draft exists:
/// the needs of a card with the usual roles of that kind.
#[must_use]
pub fn checklist_text(
    kind: ComponentKind,
    roles: &[&str],
    text: &str,
    columns: &[Column],
) -> String {
    let mut entry = ModelEntry {
        kind,
        ..ModelEntry::default()
    };
    for (i, r) in roles.iter().enumerate() {
        entry.pins.insert((i + 1).to_string(), (*r).to_string());
    }
    let mut out = format!(
        "# What a {} card needs\n\nEach line: fill it, or add a [[models.source.uncertainty]] \
         `unknown` record with that parameter name and why.\n\n",
        kind_name(kind)
    );
    for need in needs(&entry, text, columns) {
        let _ = writeln!(
            out,
            "- {}: {}  (look under: {})",
            need.key, need.ask, need.look
        );
    }
    for line in crate::card_fields::checklist_lines(kind, roles) {
        let _ = writeln!(out, "- {line}");
    }
    out.push_str(
        "- pins: every pad from the pin-table column of THIS package (`ds pins`)\n\
         - match: value_re accepting the part and its board spellings (`ds board`)\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(toml_text: &str) -> ModelEntry {
        let db: crate::schema::DbFile = toml::from_str(toml_text).expect("toml");
        db.models.into_iter().next().expect("one")
    }

    #[test]
    fn ordering_codes_get_their_board_spellings() {
        assert_eq!(
            ordering_wildcards("STM32F407VET6"),
            ["STM32F407VETx", "STM32F407VET"]
        );
        assert!(ordering_wildcards("TLV70033").is_empty());
        assert!(ordering_wildcards("LM358").is_empty());
    }

    #[test]
    fn roles_agree_with_printed_names() {
        let k = ComponentKind::Mcu;
        assert!(role_agrees("vdd", "VDD", k));
        assert!(role_agrees("vdd_3", "VDD", k));
        assert!(role_agrees("vss", "VSS", k));
        assert!(role_agrees("pb2", "PB2/BOOT1", k));
        assert!(role_agrees("boot1", "PB2/BOOT1", k));
        assert!(role_agrees("vref_plus", "VREF+", k));
        assert!(role_agrees("osc_in", "PH0-OSC_IN", k));
        assert!(!role_agrees("pe2", "PE3", k));
        assert!(!role_agrees("vdd", "PE2", k));
    }

    /// A pin-table cell cut short (a narrow column, a wrapped heading) is not a
    /// disagreement with the full role, but a different pad number or a
    /// different name still is.
    #[test]
    fn a_truncated_printed_name_still_agrees_with_its_role() {
        let k = ComponentKind::Vreg;
        assert!(role_agrees("vdd_sense", "VDD_SEN", k));
        assert!(role_agrees("pgood", "PGOO", k));
        assert!(role_agrees("enable", "ENAB/", k));
        assert!(role_agrees("feedback", "FEEDB", k));
        // Not a cut: a digit decides the pad, and a short stub names nothing.
        assert!(!role_agrees("gpio12", "GPIO1", k));
        assert!(!role_agrees("pe2", "PE3", k));
        assert!(!role_agrees("sense", "SE", k));
        assert!(!role_agrees("pgood", "FB", k));
        assert!(!role_agrees("fb", "PGOOD", k));
    }

    const LDO: &str = r#"
[[models]]
id = "ldo"
kind = "vreg"
[models.match]
value_re = "(?i)^TLV70033$"
[models.pins]
1 = "in"
2 = "gnd"
3 = "out"
"#;

    /// The pad cross-check abstains on a printed name the table cut short, and
    /// still refuses the card when a pad really carries another name.
    #[test]
    fn a_cut_short_pin_table_name_is_not_a_wrong_pad() {
        let card = entry(
            r#"
[[models]]
id = "pg_ldo"
kind = "vreg"
[models.match]
value_re = "(?i)^PGLDO$"
[models.pins]
1 = "in"
2 = "gnd"
3 = "pgood"
4 = "enable"
"#,
        );
        let facts = |printed: [&str; 4]| {
            let mut col = Column {
                label: "SOT".into(),
                ..Column::default()
            };
            for (i, name) in printed.iter().enumerate() {
                col.pads.insert((i + 1).to_string(), ((*name).into(), 4));
            }
            Facts {
                part: "PGLDO".into(),
                pin_columns: vec![col],
                ..Facts::default()
            }
        };
        let pad_findings = |printed| {
            findings(&card, &facts(printed))
                .into_iter()
                .filter(|f| f.check == "pins")
                .count()
        };
        assert_eq!(pad_findings(["IN", "GND", "PGOO", "ENAB/"]), 0);
        assert_eq!(pad_findings(["IN", "GND", "FB", "NC"]), 1);
    }

    #[test]
    fn missing_groups_and_wrong_pads_are_reported() {
        let card = entry(LDO);
        let mut col = Column {
            label: "SOT".into(),
            ..Column::default()
        };
        for (p, n) in [
            ("1", "IN"),
            ("2", "GND"),
            ("3", "EN"),
            ("4", "NC"),
            ("5", "OUT"),
        ] {
            col.pads.insert(p.into(), (n.into(), 4));
        }
        let facts = Facts {
            part: "TLV70033".into(),
            pages: vec!["Recommended operating conditions: VIN 2.0 V to 5.5 V".into()],
            pin_columns: vec![col],
            board: Some(Board {
                value: "TLV70033DDCR".into(),
                footprint: "Package_TO_SOT_SMD:SOT-23-5".into(),
                ..Board::default()
            }),
            ..Facts::default()
        };
        let found = findings(&card, &facts);
        let text = |c: &str| {
            found
                .iter()
                .filter(|f| f.check == c)
                .map(|f| f.message.clone())
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(text("binding").contains("TLV70033DDCR"), "{found:?}");
        assert!(
            text("pins").contains("pad 3: card 'out', table 'EN'"),
            "{found:?}"
        );
        let cov = text("coverage");
        assert!(cov.contains("supply_range:in"), "{cov}");
        assert!(cov.contains("supply_outputs:out"), "{cov}");
        assert!(cov.contains("theta_ja"), "{cov}");

        // No datasheet text: nothing shows a group is there to read.
        let blind = Facts {
            pages: Vec::new(),
            ..facts
        };
        assert!(findings(&card, &blind)
            .iter()
            .all(|f| f.check != "coverage"));
    }

    #[test]
    fn a_boot_select_pad_without_a_strap_row_is_reported() {
        let card = entry(
            "[[models]]\nid = \"mcu\"\nkind = \"mcu\"\n[models.pins]\n37 = \"pb2\"\n94 = \"boot0\"\n100 = \"vdd\"\n\
             [[models.straps]]\nrole = \"boot0\"\nlevel = \"low\"\nboot_select = true\n",
        );
        let mut col = Column {
            label: "LQFP100".into(),
            ..Column::default()
        };
        col.pads.insert("37".into(), ("PB2/BOOT1".into(), 52));
        col.pads.insert("94".into(), ("BOOT0".into(), 60));
        col.pads.insert("100".into(), ("VDD".into(), 61));
        let facts = Facts {
            part: "STM32F407VET6".into(),
            pin_columns: vec![col],
            ..Facts::default()
        };
        let found = boot_straps(&card, &facts);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].message.contains("pad 37 'PB2/BOOT1'"), "{found:?}");
        assert!(!found[0].message.contains("pad 94"), "{found:?}");
        // With both straps present there is nothing to say.
        let both = entry(
            "[[models]]\nid = \"mcu\"\nkind = \"mcu\"\n[models.pins]\n37 = \"pb2\"\n94 = \"boot0\"\n100 = \"vdd\"\n\
             [[models.straps]]\nrole = \"boot0\"\nlevel = \"low\"\nboot_select = true\n\
             [[models.straps]]\nrole = \"pb2\"\nlevel = \"low\"\nboot_select = false\n",
        );
        assert!(boot_straps(&both, &facts).is_empty());
    }

    /// A strap pad scaled on the core supply of a card that also has an IO
    /// supply is the mapping error a drafted card makes; naming the supply (or
    /// a `level_rails` entry) answers the finding.
    #[test]
    fn a_strap_read_at_the_core_supply_beside_an_io_supply_is_asked_about() {
        let base = "[[models]]\nid = \"mcu\"\nkind = \"mcu\"\n[models.pins]\n1 = \"vdd\"\n2 = \"iovdd\"\n3 = \"boot0\"\n";
        let strap = "[[models.straps]]\nrole = \"boot0\"\nlevel = \"low\"\nboot_select = true\n\
                     vil = [0.3, 0.0]\nvih = [0.7, 0.0]\n";
        let found = strap_domain(&entry(&format!("{base}{strap}")));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].message.contains("supply = \"iovdd\"") && found[0].message.contains("'boot0'"),
            "{found:?}"
        );
        // Named on the strap, or by the pad's level rail: nothing to ask.
        assert!(strap_domain(&entry(&format!("{base}{strap}supply = \"iovdd\"\n"))).is_empty());
        let railed = format!("{base}[models.level_rails]\niovdd = [\"boot0\"]\n{strap}");
        assert!(strap_domain(&entry(&railed)).is_empty());
        // A card with one supply has no domain to confuse.
        let single =
            "[[models]]\nid = \"mcu\"\nkind = \"mcu\"\n[models.pins]\n1 = \"vdd\"\n3 = \"boot0\"\n";
        assert!(strap_domain(&entry(&format!("{single}{strap}"))).is_empty());
    }

    #[test]
    fn a_not_stated_record_is_challenged_when_the_datasheet_prints_the_row() {
        let card = entry(&format!(
            "{LDO}\n[models.source]\ntier = \"datasheet-derived\"\n\
             validation = \"physical-bounds-only\"\n[[models.source.uncertainty]]\n\
             status = \"unknown\"\nparameter = \"theta_ja_c_per_w\"\nreason = \"no θJA stated\"\n"
        ));
        let facts = Facts {
            part: "TLV70033".into(),
            pages: vec![
                "Contents\n7.8 Thermal characteristics ........ 185".into(),
                "Thermal resistance junction-ambient LQFP100 43 °C/W".into(),
            ],
            ..Facts::default()
        };
        let found = findings(&card, &facts);
        assert!(
            found
                .iter()
                .any(|f| f.message.starts_with("theta_ja is recorded")
                    && f.message.contains("PDF page 2")),
            "{found:?}"
        );
    }

    #[test]
    fn an_uncertainty_record_satisfies_a_need_and_output_ranges_block() {
        let card = entry(&format!(
            "{LDO}\n[models.ratings]\ntheta_ja_c_per_w = 200.0\n\
             [[models.envelope]]\nkind = \"supply_range\"\npin = \"out\"\nmin_v = 0.0\nmax_v = 3.3\n"
        ));
        let found = findings(&card, &Facts::default());
        assert!(
            found
                .iter()
                .any(|f| f.severity == Severity::Error
                    && f.message.contains("output the part drives")),
            "{found:?}"
        );
        assert!(!found.iter().any(|f| f.message.starts_with("theta_ja")));
    }

    #[test]
    fn board_markdown_round_trips() {
        let b = Board {
            refdes: "U3".into(),
            value: "TLV70033".into(),
            footprint: "SOT-23-5".into(),
            pads: vec![("1".into(), "VIN".into()), ("5".into(), "+3V3".into())],
        };
        assert_eq!(Board::from_markdown(&b.markdown()), b);
    }
    /// TUSB8041 (2026-10-21): a QFN-64-1EP footprint with 43 thermal-via and
    /// paste sub-pads numbered like the exposed pad counted 107 pads against a
    /// 64-lead card with its exposed pad, and the agent asked for the footprint
    /// to be "corrected".
    fn qfn64_board(ep_copies: usize) -> Board {
        let mut pads: Vec<(String, String)> = (1..=64)
            .map(|n| (n.to_string(), format!("SIG{n}")))
            .collect();
        pads.extend((0..ep_copies).map(|_| ("65".to_string(), "GND".to_string())));
        // A mechanical pad has no number.
        pads.push((String::new(), "GND".into()));
        Board {
            refdes: "U2".into(),
            value: "TUSB8041RGCR".into(),
            footprint: "Package_DFN_QFN:QFN-64-1EP_9x9mm_P0.5mm_EP6x6mm_ThermalVias".into(),
            pads,
        }
    }

    fn card_with_pads(count: usize) -> ModelEntry {
        let mut e = ModelEntry {
            id: "tusb8041rgcr".into(),
            ..ModelEntry::default()
        };
        e.pins = (1..=count)
            .map(|n| (n.to_string(), format!("p{n}")))
            .collect();
        e
    }

    #[test]
    fn a_repeated_exposed_pad_number_is_one_pad() {
        let board = qfn64_board(43);
        assert_eq!(board.pads.len(), 64 + 43 + 1);
        assert_eq!(board.pad_count(), Some(65));
        assert_eq!(board.distinct_pads().len(), 65);
        assert!(
            board.markdown().contains("pads: 65\n"),
            "{}",
            board.markdown()
        );
        // The card with its exposed pad, and the one without it, both fit.
        for count in [65, 64] {
            let found = bound_pad_problem(&card_with_pads(count), &board);
            assert!(found.is_none(), "{count} pins: {found:?}");
        }
        // A card read from another package's column still does not.
        let found = bound_pad_problem(&card_with_pads(48), &board)
            .expect("a 48-pin card on a 65-pad footprint");
        assert!(found.message.contains("has 65 pads"), "{}", found.message);
    }

    #[test]
    fn board_markdown_lists_a_repeated_pad_once_per_net() {
        let board = qfn64_board(43);
        let text = board.markdown();
        assert_eq!(text.matches("| 65 | GND |").count(), 1, "{text}");
        let back = Board::from_markdown(&text);
        assert_eq!(back.pad_count(), Some(65));
        // The same number on two nets keeps both rows: the board disagrees with itself.
        let mut split = qfn64_board(2);
        split.pads.push(("65".into(), "VCC".into()));
        let text = split.markdown();
        assert!(
            text.contains("| 65 | GND |") && text.contains("| 65 | VCC |"),
            "{text}"
        );
    }
    /// CY7C65215 (2026-10-21): a doubled backslash in `footprint_re` was flagged
    /// as not matching, the fix drew the anchored-scope warning a round later.
    /// The first warning names the family form and the doubled backslash.
    #[test]
    fn a_footprint_re_that_misses_names_the_family_form_and_a_doubled_backslash() {
        let mut entry = ModelEntry {
            id: "cy7c65215".into(),
            ..ModelEntry::default()
        };
        entry.r#match.footprint_re =
            Some(r"^Package_DFN_QFN:QFN-32-1EP_5x5mm_P0\\.5mm_EP3\\.45x3\\.45mm$".into());
        let facts = Facts {
            board: Some(Board {
                footprint: "Package_DFN_QFN:QFN-32-1EP_5x5mm_P0.5mm_EP3.45x3.45mm".into(),
                ..Board::default()
            }),
            ..Facts::default()
        };
        let message = binding(&entry, &facts)
            .into_iter()
            .find(|f| f.message.starts_with("footprint_re"))
            .expect("a pattern that misses the footprint is flagged")
            .message;
        assert!(message.contains("doubled backslash"), "{message}");
        assert!(message.contains("QFN[-_ ]?32"), "{message}");
        // A pattern that matches draws neither.
        entry.r#match.footprint_re = Some("(?i)QFN[-_ ]?32(?:[-_ ]|$)".into());
        assert!(binding(&entry, &facts)
            .iter()
            .all(|f| !f.message.starts_with("footprint_re")));
    }

    /// A logic card whose datasheet states an open-input default must list it.
    #[test]
    fn an_open_input_default_in_the_datasheet_asks_for_pulled_inputs() {
        let mut card = ModelEntry {
            id: "iso".into(),
            kind: ComponentKind::Digital,
            ..ModelEntry::default()
        };
        card.logic.inputs = vec!["ina".into(), "inb".into()];
        let want = |text: &str| {
            needs(&card, text, &[])
                .into_iter()
                .any(|n| n.key == "pulled_inputs")
        };
        for text in [
            "Default mode: When INx is open, the corresponding channel output goes to the default logic state.",
            "EN pins are H or open to enable the outputs",
            "The inputs have an internal pull-down resistor.",
        ] {
            assert!(want(text), "{text}");
        }
        // Silent control: a datasheet that states nothing about an open input.
        assert!(!want(
            "Recommended operating conditions: VCC 3.0 V to 5.5 V"
        ));
        // Once the card lists the pulled inputs, or says it has none to record,
        // the need is met.
        let need = needs(&card, "H or open", &[])
            .into_iter()
            .find(|n| n.key == "pulled_inputs")
            .expect("need");
        assert!(!satisfied(&card, &need));
        card.logic.pulled_inputs = vec!["ina".into()];
        assert!(satisfied(&card, &need));
    }

    /// An amplifier card with an input-impedance row must say whether it biases the input.
    #[test]
    fn an_input_impedance_row_asks_for_the_inputs_dc_path() {
        let mut card = ModelEntry {
            id: "amp".into(),
            kind: ComponentKind::Opamp,
            ..ModelEntry::default()
        };
        card.pins.insert("3".into(), "in_plus".into());
        card.pins.insert("4".into(), "in_minus".into());
        let want = |card: &ModelEntry, text: &str| {
            needs(card, text, &[])
                .into_iter()
                .any(|n| n.key == "input_dc_path")
        };
        assert!(want(&card, "Input impedance, all gains: 100 kOhm"));
        // Silent control: no input-impedance row, or no analog input pin.
        assert!(!want(&card, "Supply current 2 mA"));
        let mut gate = ModelEntry::default();
        gate.pins.insert("1".into(), "a".into());
        assert!(!want(&gate, "Input impedance 100 kOhm"));
        card.input_dc_path.push(crate::schema::InputDcPath {
            role: "in_plus".into(),
            to: crate::schema::DcPathTo::Bias,
            ohms: Some(1.0e5),
            basis: "p.5".into(),
        });
        let need = needs(&card, "Input impedance 100 kOhm", &[])
            .into_iter()
            .find(|n| n.key == "input_dc_path")
            .expect("need still listed");
        assert!(satisfied(&card, &need));
    }

    /// A datasheet that says a supply pin becomes an output in one mode asks for
    /// a `supply_outputs` row with its mode; a card that has one, or an
    /// ordinary datasheet, is not asked.
    #[test]
    fn a_supply_pin_that_becomes_an_output_needs_a_supply_output_row() {
        let bare = entry(
            "[[models]]\nid = \"u\"\nkind = \"digital\"\n[models.pins]\n1 = \"vregin\"\n2 = \"vccd\"\n",
        );
        let text = "when the part runs on +5 V (VREGIN), the VCCD becomes an output.";
        let asks = |e: &ModelEntry, t: &str| {
            needs(e, t, &[])
                .iter()
                .any(|n| n.key == "supply_outputs:dual_role")
        };
        assert!(asks(&bare, text));
        let with_row = entry(
            "[[models]]\nid = \"u\"\nkind = \"digital\"\n[models.pins]\n1 = \"vregin\"\n2 = \"vccd\"\n\
             [[models.supply_outputs]]\nrole = \"vccd\"\nnominal = 3.3\nmin = 3.0\nmax = 3.6\nbasis = \"b\"\n",
        );
        let need = needs(&with_row, text, &[])
            .into_iter()
            .find(|n| n.key == "supply_outputs:dual_role")
            .expect("asked");
        assert!(has_field(&with_row, &need));
        assert!(!has_field(&bare, &need));
        // An ordinary datasheet (an output that is simply high when enabled) is silent.
        assert!(!asks(
            &bare,
            "the power-good output is high when the rail is in regulation."
        ));
    }
}
