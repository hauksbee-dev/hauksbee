//! Indicators whose MEANING no check can judge.
//!
//! An LED driven from a logic signal is electrically fine whichever way round
//! the signal's polarity runs: an active-low `READY_N` through a non-inverting
//! buffer lights the LED when the board is NOT ready, and every voltage,
//! current and rating on that path is in range. Only the designer's intent
//! ("lit means ready") can say it is wrong, so without a stated outcome the
//! verdict must say this was not judged rather than let a clean bill imply it
//! was.
//!
//! This module only traces; it never makes a finding. For each indicator on a
//! net driven by a logic output (directly or through a series resistor) it
//! walks back through single-input buffer and inverter stages, read from the
//! part's own `[models.logic]` block, to the net that originates the signal,
//! and reads that net's name for an active-low hint. The report surfaces list
//! the traces as not checked; `hauksbee-ci init` scaffolds one outcome each.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use hauksbee_bind::binder::{is_ground, power_rail_voltage};
use hauksbee_bind::digital::output_roles;
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{is_led, is_plain_resistor, Component, ExtractedBoard};
use hauksbee_models::{ComponentKind, ModelEntry, ModelLibrary};

/// One logic stage between the source net and the indicator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    /// The gate's reference ("U1").
    pub reference: String,
    /// True for an inverter, false for a non-inverting buffer.
    pub inverting: bool,
}

/// An indicator, the logic signal that drives it, and the path between.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndicatorTrace {
    /// The indicator's reference ("D3").
    pub indicator: String,
    /// The net on the indicator's own terminal: its level is what lights it.
    pub indicator_net: String,
    /// The indicator's driven terminal is its cathode (it lights when the
    /// drive is LOW); `None` when the pin names do not say which end is which.
    pub sinks: Option<bool>,
    /// The part whose output finally drives the indicator path.
    pub driver: String,
    /// Stages from the indicator back toward the source, nearest first.
    pub stages: Vec<Stage>,
    /// Where the signal originates: the net feeding the first stage, or the
    /// driving output's own net when there are no stages.
    pub source_net: String,
    /// The source net's name reads as active-low (`_N`, `n` prefix/suffix,
    /// an overbar spelling).
    pub source_active_low: bool,
    /// A microcontroller pin drives the indicator directly, with no logic
    /// stage between: firmware writes the level that lights it, so its
    /// meaning is a firmware choice rather than one ordering the board fixes.
    pub firmware_sets_level: bool,
}

impl IndicatorTrace {
    /// "D3 (from `READY_N`, name suggests active-low, via U1 buffer)".
    #[must_use]
    pub fn describe(&self) -> String {
        let hint = if self.source_active_low {
            ", name suggests active-low"
        } else {
            ""
        };
        let via = if self.stages.is_empty() {
            format!(", driven by {}", self.driver)
        } else {
            let path: Vec<String> = self
                .stages
                .iter()
                .rev()
                .map(|s| {
                    let kind = if s.inverting { "inverter" } else { "buffer" };
                    format!("{} {kind}", s.reference)
                })
                .collect();
            format!(", via {}", path.join(" -> "))
        };
        format!("{} (from {}{hint}{via})", self.indicator, self.source_net)
    }
}

/// The plain-language "not judged" line for `traces`, or `None` when there are
/// none. Concise by design: it names what was not checked and why, never a
/// failure.
#[must_use]
pub fn not_judged_note(traces: &[IndicatorTrace]) -> Option<String> {
    const SHOWN: usize = 6;
    if traces.is_empty() {
        return None;
    }
    // Indicators straight on one part's outputs with nothing to say about
    // their source (an LED driver's channels) read as one group, not a list
    // of auto-named nets.
    let plain = |t: &IndicatorTrace| t.stages.is_empty() && !t.source_active_low;
    let mut groups: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for t in traces.iter().filter(|t| plain(t)) {
        groups.entry(&t.driver).or_default().push(&t.indicator);
    }
    let mut items: Vec<String> = traces
        .iter()
        .filter(|t| !plain(t) || groups[t.driver.as_str()].len() == 1)
        .map(IndicatorTrace::describe)
        .collect();
    for (driver, leds) in groups.iter().filter(|(_, l)| l.len() > 1) {
        items.push(format!(
            "{} on {driver} outputs ({})",
            leds.len(),
            leds.join(", ")
        ));
    }
    let mut listed: Vec<String> = items.iter().take(SHOWN).cloned().collect();
    if items.len() > SHOWN {
        listed.push(format!("and {} more", items.len() - SHOWN));
    }
    Some(format!(
        "Indicators: indicator meaning and signal polarity versus intent: not checked for {}. \
         These are electrically fine either way round, and no stated outcome says which level \
         should light them; `hauksbee-ci init` scaffolds one to confirm.",
        listed.join("; ")
    ))
}

/// The one condition a pass rests on while hardware-fixed indicator meaning is
/// unjudged, or `None` when there is none to state.
///
/// One compact sentence for the whole area, naming the indicators (the
/// not-covered note carries each traced path). An indicator a microcontroller
/// pin drives directly is left out: firmware writes its level, so ordering
/// the board does not fix its meaning, and it stays listed as not covered.
#[must_use]
pub fn intent_condition(traces: &[IndicatorTrace]) -> Option<String> {
    const SHOWN: usize = 4;
    let fixed: Vec<&str> = traces
        .iter()
        .filter(|t| !t.firmware_sets_level)
        .map(|t| t.indicator.as_str())
        .collect();
    let (listed, them) = match fixed.as_slice() {
        [] => return None,
        [one] => ((*one).to_string(), "it"),
        many if many.len() <= SHOWN => (many.join(", "), "them"),
        many => (
            format!(
                "{} and {} more",
                many[..SHOWN].join(", "),
                many.len() - SHOWN
            ),
            "them",
        ),
    };
    Some(format!(
        "Indicator polarity versus intent was not checked for {listed}: the hardware sets \
         which level lights {them}, and no stated outcome says which should; state it as an \
         outcome (hauksbee-ci init scaffolds one) to remove this condition."
    ))
}

/// Does a net name read as active-low? Looks at the last path segment only, so
/// a `KiCad` sheet path (`/power/EN`) is not read as an overbar.
#[must_use]
pub fn name_suggests_active_low(name: &str) -> bool {
    let full = name.trim();
    let leaf = full.rsplit('/').next().unwrap_or(full);
    let upper = leaf.to_ascii_uppercase();
    if leaf.starts_with('~') || leaf.starts_with('!') || leaf.ends_with('#') || leaf.ends_with('\\')
    {
        return true;
    }
    if upper.ends_with("_N") || upper.ends_with("-N") {
        return true;
    }
    let mut chars = leaf.chars();
    let first_two = (chars.next(), chars.next());
    // nRESET / nCS: a lower-case n prefix before an upper-case name.
    if let (Some('n'), Some(c)) = first_two {
        if c.is_ascii_uppercase() {
            return true;
        }
    }
    // RESETn / CSn: a lower-case n suffix after an upper-case name.
    let mut rev = leaf.chars().rev();
    matches!((rev.next(), rev.next()), (Some('n'), Some(c)) if c.is_ascii_uppercase())
}

/// A supply-looking name the rail table cannot price (`VDD`, `VDD_IO`,
/// `+3V3_AUX`): a power LED on it is not a logic indicator.
fn looks_like_supply(name: &str) -> bool {
    let leaf = name
        .trim()
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    leaf.starts_with('+')
        || ["VDD", "VCC", "VBUS", "VIN", "VBAT", "VREF", "VSYS", "VAA"]
            .iter()
            .any(|p| leaf.starts_with(p) || leaf.contains(&format!("_{p}")))
}

/// What drives a net: a modelled logic output, maybe a single-input stage.
#[derive(Clone)]
struct Driver {
    reference: String,
    /// A microcontroller pin: its level is firmware state.
    mcu: bool,
    /// `(inverting, input net)` when the output is one buffer/inverter stage.
    stage: Option<(bool, i64)>,
}

/// Kinds whose outputs are logic outputs.
const fn is_logic_kind(kind: ComponentKind) -> bool {
    matches!(
        kind,
        ComponentKind::Digital | ComponentKind::ShiftRegister | ComponentKind::Mcu
    )
}

/// `expr` as a single-input stage over `inputs`: `Some((inverting, input))`.
fn single_input_stage(expr: &str, inputs: &[String]) -> Option<(bool, String)> {
    let mut e: String = expr.chars().filter(|c| !c.is_whitespace()).collect();
    while e.starts_with('(') && e.ends_with(')') {
        e = e[1..e.len() - 1].to_string();
    }
    let (inverting, name) = e
        .strip_prefix('!')
        .or_else(|| e.strip_prefix('~'))
        .map_or_else(
            || (false, e.clone()),
            |rest| {
                (
                    true,
                    rest.trim_matches(|c| c == '(' || c == ')').to_string(),
                )
            },
        );
    inputs.contains(&name).then_some((inverting, name))
}

fn pad_net(comp: &Component, pad: &str) -> Option<i64> {
    comp.pins
        .iter()
        .find(|p| p.number == pad)
        .and_then(|p| p.net)
        .filter(|&id| id != 0)
}

/// Each logic-driven net and what drives it.
fn drivers(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    is_signal: &dyn Fn(i64) -> bool,
) -> HashMap<i64, Driver> {
    let mut out: HashMap<i64, Driver> = HashMap::new();
    for (comp, model) in super::resolved_components(board, lib) {
        if !is_logic_kind(model.kind) {
            continue;
        }
        if model.kind == ComponentKind::Mcu {
            // A GPIO's direction is firmware state: any signal pin may drive.
            for id in comp.pins.iter().filter_map(|p| p.net) {
                if is_signal(id) {
                    out.entry(id).or_insert_with(|| Driver {
                        reference: comp.reference.clone(),
                        mcu: true,
                        stage: None,
                    });
                }
            }
            continue;
        }
        for (pad, role) in model_output_pads(&model) {
            let Some(id) = pad_net(comp, &pad).filter(|&id| is_signal(id)) else {
                continue;
            };
            let stage = model
                .logic
                .comb
                .get(&role)
                .and_then(|expr| single_input_stage(expr, &model.logic.inputs))
                .and_then(|(inverting, input)| {
                    let in_pad = model.pins.iter().find(|(_, r)| **r == input)?.0;
                    Some((inverting, pad_net(comp, in_pad)?))
                });
            out.insert(
                id,
                Driver {
                    reference: comp.reference.clone(),
                    mcu: false,
                    stage,
                },
            );
        }
    }
    // A schematic pin typed `output` is a driver even on an unmodelled part.
    for comp in fitted(board) {
        for pin in &comp.pins {
            if pin.kind.eq_ignore_ascii_case("output") {
                if let Some(id) = pin.net.filter(|&id| is_signal(id)) {
                    out.entry(id).or_insert_with(|| Driver {
                        reference: comp.reference.clone(),
                        mcu: false,
                        stage: None,
                    });
                }
            }
        }
    }
    out
}

/// `(pad, role)` for each output role the model maps to a pad.
fn model_output_pads(model: &ModelEntry) -> Vec<(String, String)> {
    let outputs: BTreeSet<String> = output_roles(model).into_iter().collect();
    model
        .pins
        .iter()
        .filter(|(_, role)| outputs.contains(*role))
        .map(|(pad, role)| (pad.clone(), role.clone()))
        .collect()
}

fn fitted(board: &ExtractedBoard) -> impl Iterator<Item = &Component> {
    board
        .components
        .iter()
        .filter(|c| AssemblyState::of(c).is_present())
}

fn nets_of(c: &Component) -> BTreeSet<i64> {
    c.pins
        .iter()
        .filter_map(|p| p.net)
        .filter(|&id| id != 0)
        .collect()
}

/// Trace every logic-driven indicator on `board` back to its source net.
#[must_use]
pub fn indicator_traces(board: &ExtractedBoard, lib: &ModelLibrary) -> Vec<IndicatorTrace> {
    let names: HashMap<i64, &str> = board.nets.iter().map(|n| (n.id, n.name.as_str())).collect();
    let name = |id: i64| names.get(&id).copied().unwrap_or("").to_string();
    let is_signal = |id: i64| {
        let n = names.get(&id).copied().unwrap_or("");
        id != 0 && !is_ground(n) && power_rail_voltage(n).is_none() && !looks_like_supply(n)
    };
    let drivers = drivers(board, lib, &is_signal);
    if drivers.is_empty() {
        return Vec::new();
    }
    // Two-pin resistors by net, for the series resistor between output and LED.
    let mut resistors: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for r in fitted(board).filter(|c| is_plain_resistor(c)) {
        if let [a, b] = nets_of(r).into_iter().collect::<Vec<_>>()[..] {
            resistors.entry(a).or_default().push(b);
            resistors.entry(b).or_default().push(a);
        }
    }
    let mut out = Vec::new();
    for led in fitted(board).filter(|c| is_led(c)) {
        let nets = nets_of(led);
        if nets.len() != 2 {
            continue;
        }
        // The driven terminal: on a logic-driven net, or one resistor away.
        let found = nets.iter().find_map(|&own| {
            if !is_signal(own) {
                return None;
            }
            if drivers.contains_key(&own) {
                return Some((own, own));
            }
            resistors
                .get(&own)?
                .iter()
                .find(|far| drivers.contains_key(far))
                .map(|&far| (own, far))
        });
        let Some((own, mut net)) = found else {
            continue;
        };
        let sinks = led.pins.iter().find(|p| p.net == Some(own)).and_then(|p| {
            // The pin name says which end it is; failing that, a pad
            // numbered A/K does.
            [p.function.as_str(), p.number.as_str()]
                .into_iter()
                .find_map(|f| match f.trim().to_ascii_uppercase().as_str() {
                    "K" | "CATHODE" | "-" => Some(true),
                    "A" | "ANODE" | "+" => Some(false),
                    _ => None,
                })
        });
        let mut stages = Vec::new();
        let mut seen = BTreeSet::from([net]);
        let mut driver = drivers[&net].reference.clone();
        while let Some(d) = drivers.get(&net) {
            driver.clone_from(&d.reference);
            let Some((inverting, input)) = d.stage else {
                break;
            };
            if !seen.insert(input) {
                break;
            }
            stages.push(Stage {
                reference: d.reference.clone(),
                inverting,
            });
            net = input;
        }
        let source_net = name(net);
        let firmware_sets_level = stages.is_empty() && drivers.get(&net).is_some_and(|d| d.mcu);
        out.push(IndicatorTrace {
            indicator: led.reference.clone(),
            indicator_net: name(own),
            sinks,
            driver,
            stages,
            source_active_low: name_suggests_active_low(&source_net),
            source_net,
            firmware_sets_level,
        });
    }
    out.sort_by(|a, b| a.indicator.cmp(&b.indicator));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_low_spellings_are_recognised_and_plain_names_are_not() {
        for n in [
            "READY_N",
            "/sheet/RESET_N",
            "nRESET",
            "CSn",
            "~{RST}",
            "~RST",
            "!WR",
            "RD#",
        ] {
            assert!(name_suggests_active_low(n), "{n}");
        }
        for n in ["READY", "/power/EN", "LED_DRV", "MAIN", "SCAN", "N"] {
            assert!(!name_suggests_active_low(n), "{n}");
        }
    }

    #[test]
    fn a_drivers_plain_channels_are_one_group_and_traced_ones_stay_itemised() {
        let trace = |led: &str, stages: Vec<Stage>, src: &str| IndicatorTrace {
            indicator: led.into(),
            indicator_net: format!("Net-({led}-K)"),
            sinks: Some(true),
            driver: "U10".into(),
            stages,
            source_net: src.into(),
            source_active_low: name_suggests_active_low(src),
            firmware_sets_level: false,
        };
        let buffer = vec![Stage {
            reference: "U1".into(),
            inverting: false,
        }];
        let note = not_judged_note(&[
            trace("D1", buffer, "READY_N"),
            trace("D10", Vec::new(), "Net-(D10-K)"),
            trace("D11", Vec::new(), "Net-(D11-K)"),
        ])
        .unwrap();
        assert!(
            note.contains("D1 (from READY_N, name suggests active-low, via U1 buffer)"),
            "{note}"
        );
        assert!(note.contains("2 on U10 outputs (D10, D11)"), "{note}");
        assert!(!note.contains("Net-(D10-K)"), "{note}");
        assert!(not_judged_note(&[]).is_none());
    }

    #[test]
    fn hardware_fixed_indicators_make_one_condition_and_firmware_set_ones_none() {
        let trace = |led: &str, firmware: bool| IndicatorTrace {
            indicator: led.into(),
            indicator_net: format!("Net-({led}-K)"),
            sinks: Some(true),
            driver: "U1".into(),
            stages: Vec::new(),
            source_net: "STATUS".into(),
            source_active_low: false,
            firmware_sets_level: firmware,
        };
        // A GPIO-driven LED's level is firmware's to choose: no condition.
        assert!(intent_condition(&[trace("D1", true)]).is_none());
        assert!(intent_condition(&[]).is_none());
        let one = intent_condition(&[trace("D1", true), trace("D2", false)]).unwrap();
        assert!(
            one.contains("not checked for D2: the hardware sets which level lights it,"),
            "{one}"
        );
        assert!(!one.contains("D1"), "{one}");
        // Many indicators stay one compact sentence, not a list per LED.
        let many: Vec<_> = (1..=9).map(|i| trace(&format!("D{i}"), false)).collect();
        let many = intent_condition(&many).unwrap();
        assert!(many.contains("D1, D2, D3, D4 and 5 more: "), "{many}");
        assert!(!many.contains("D5"), "{many}");
    }

    #[test]
    fn single_input_stages_are_read_from_the_logic_expression() {
        let ins = vec!["a".to_string()];
        assert_eq!(single_input_stage("!a", &ins), Some((true, "a".into())));
        assert_eq!(
            single_input_stage(" ( a ) ", &ins),
            Some((false, "a".into()))
        );
        assert_eq!(single_input_stage("!(a)", &ins), Some((true, "a".into())));
        assert_eq!(single_input_stage("a & b", &ins), None);
    }
}
