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

use std::cell::RefCell;
use std::fmt::Write as _;

use hauksbee_ir::evidence::ModelUncertainty;

use crate::pin_table::{self, Column};
use crate::schema::{ComponentKind, ModelEntry, OperatingEnvelope};

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
    /// The board's pad count: its numeric pads, else every pad listed.
    #[must_use]
    pub fn pad_count(&self) -> Option<usize> {
        let n = self.pads.len();
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
            for (pad, net) in &self.pads {
                let _ = writeln!(out, "| {pad} | {net} |");
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
    match entry.kind {
        ComponentKind::Mcu => mcu_needs(entry, text, &mut out),
        ComponentKind::Vreg => vreg_needs(entry, &mut out),
        _ => {}
    }
    out
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
                  boot-mode table names"
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
        ("straps", _) => !entry.straps.is_empty(),
        ("rail_order", _) => entry
            .envelope
            .iter()
            .any(|e| matches!(e, OperatingEnvelope::RailOrder { .. })),
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
    let mut seen = std::collections::HashSet::new();
    out.retain(|f| seen.insert(f.message.clone()));
    out
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
            out.push(Finding {
                severity: Severity::Warn,
                check: "binding",
                message: format!(
                    "footprint_re '{}' does not match the board footprint '{}', so the card \
                     would not bind this part: widen it (or drop it when one pinout serves every \
                     package)",
                    rules.footprint_re.as_deref().unwrap_or(""),
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
    tokens.iter().any(|t| {
        *t == r
            || *t == bare
            || (family(&r).is_some() && family(&r) == family(t))
            || (family(&bare).is_some() && family(&bare) == family(&numbered(t)))
            || (matches!(r.as_str(), "nc" | "dnc") && t.starts_with("nc"))
            || (matches!(r.as_str(), "ep" | "pad" | "thermal") && t.contains("pad"))
    })
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
    let pads = facts.board.as_ref().and_then(Board::pad_count).or(card_max);
    let Ok(col) = pin_table::choose(&facts.pin_columns, package.as_deref(), pads) else {
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
}
