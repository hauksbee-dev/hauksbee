//! A linear regulator whose enable pin nothing can take to its on level.
//!
//! Many LDO datasheets state what an open enable pin does: "logic low or open
//! = shutdown" (the card's `en_open = "off"`), or an internal pull toward the
//! off level (`en_pull_down_ohms` on an on-high pin). On such a part an EN pin
//! left floating, or wired only to parts that cannot raise it (a capacitor, a
//! pull-down, a low-side transistor, another part's enable input, a pull-up
//! that is not fitted), holds the regulator off for good: its rail never
//! comes on and everything it feeds is unpowered. The binder already gates the
//! rail on that pin (`hauksbee-bind/src/vreg_enable.rs`); this check states it
//! as a gating finding on the board, naming the regulator, the pin, its net,
//! the rail that stays off and what the datasheet says, with the fix.
//!
//! It stays silent wherever the board may set the on level: the pin tied to
//! the regulator's input or any other net a part drives, a pull resistor to a
//! non-ground net, a GPIO or logic output, a switch or connector that can
//! reach a rail, a transistor that can pull high, or any part no rule here
//! knows. A pin tied straight to the off rail (EN to GND) is a deliberate
//! choice and is left to the bind report. A connector pad on the net is an
//! off-board source that may drive the pin, so a net whose only way to the on
//! level is a connector is a Medium finding, not a High one, and only when the
//! board itself pulls the pin to the off level (with no pull at all the pin is
//! simply an external enable input). A card that states no open-pin
//! state, or states the pin is on when open (`en_open = "on"`, an internal
//! pull toward on), is not judged: an unknown is never read as off.

use std::collections::HashMap;

use hauksbee_bind::binder::{is_ground, resolve};
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    control_role, is_capacitor, is_connector_like, is_test_point_or_hole, Component,
    ExtractedBoard, LintCheck, LintFinding, NetLintReport, Pin, Severity, REMEDY_MARKER,
};
use hauksbee_models::refdes::RefClass;
use hauksbee_models::{ComponentKind, Confidence, ModelEntry, ModelLibrary};

use super::envelope::role_net_id;
use super::model_controls::low_side_sink;

/// Why the card says an open pin holds the part off.
enum OffWhenOpen {
    /// The datasheet's stated open-pin state (`en_open = "off"`), with the
    /// card's quote of it when it carries one (`en_open_basis`).
    Stated(Option<String>),
    /// The part's internal pull toward the off level.
    Pull { up: bool, value: String },
}

impl OffWhenOpen {
    /// What the card says about the open pin, or `None` when an open pin is
    /// on or the datasheet does not say.
    fn of(model: &ModelEntry, on_high: bool) -> Option<Self> {
        let params = &model.params;
        let positive = |k| params.get_f64(k).filter(|v| v.is_finite() && *v > 0.0);
        let pull = [
            ("en_pull_down_ohms", false),
            ("en_pull_up_ohms", true),
            ("en_pull_down_a", false),
            ("en_pull_up_a", true),
        ]
        .into_iter()
        .find_map(|(k, up)| positive(k).map(|v| (k, up, v)));
        if let Some((key, up, v)) = pull {
            // A pull toward the on level holds an open pin on.
            if up == on_high {
                return None;
            }
            let value = if key.ends_with("_ohms") {
                ohms(v)
            } else {
                format!("{:.3} µA", v * 1e6)
            };
            return Some(Self::Pull { up, value });
        }
        (params.get_str("en_open") == Some("off")).then(|| {
            Self::Stated(
                params
                    .get_str("en_open_basis")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            )
        })
    }

    fn sentence(&self, model_id: &str, pin: &str) -> String {
        match self {
            Self::Stated(Some(quote)) => format!(
                "the datasheet states an open {pin} pin shuts the part down ({quote}; card \
                 '{model_id}')"
            ),
            Self::Stated(None) => format!(
                "card '{model_id}' records the datasheet's statement that an open {pin} pin \
                 shuts the part down (en_open = \"off\")"
            ),
            Self::Pull { up, value } => format!(
                "the part's internal {} ({value}, card '{model_id}') holds an open {pin} pin at \
                 its off level",
                if *up { "pull-up" } else { "pull-down" }
            ),
        }
    }
}

/// A resistor's value in ohms, or `None` when its value text does not read as
/// a resistance.
pub(super) fn resistor_ohms(c: &Component) -> Option<f64> {
    hauksbee_models::value::parse_value(&c.value)
        .filter(|p| matches!(p.unit.as_deref(), None | Some("Ω")))
        .map(|p| p.si)
        .filter(|v| v.is_finite() && *v >= 0.0)
}

pub(super) fn ohms(v: f64) -> String {
    let (scaled, unit) = if v >= 1e6 {
        (v / 1e6, "MΩ")
    } else if v >= 1e3 {
        (v / 1e3, "kΩ")
    } else {
        (v, "Ω")
    };
    let s = format!("{scaled:.2}");
    format!("{} {unit}", s.trim_end_matches('0').trim_end_matches('.'))
}

/// What one other pad on a control net does to its level.
pub(super) enum Member {
    /// It may take the pin to its on level (a pull to a rail, a driver, a
    /// part no rule knows): the board is not judged.
    MaySet,
    /// It cannot, and reads this way in the finding.
    Cannot(String),
    /// A resistor (or a wire-like ferrite or inductor) from the net to the
    /// off rail: it cannot raise the pin and it actively holds it off. `ohms`
    /// is its resistance, `None` when the value does not read as one.
    OffPull { label: String, ohms: Option<f64> },
    /// A connector pad: whatever is plugged in may drive the net, but nothing
    /// on this board does.
    External(String),
}

/// The control pin under judgement, as the classifier needs it. For a
/// regulator the off rail is its GND (on-high pin) or its input (on-low pin);
/// for any other part's active-high enable or active-low reset it is ground.
pub(super) struct Reg<'a> {
    pub(super) comp: &'a Component,
    /// Whether the pin is on at its HIGH level.
    pub(super) on_high: bool,
    /// The net the off level sits on when the pin is on-low.
    pub(super) input: i64,
    pub(super) gnd: Option<i64>,
}

impl Reg<'_> {
    /// The net the pin's off level sits on: ground for an on-high pin, the
    /// regulator's input for an on-low one.
    fn is_off_rail(&self, board: &ExtractedBoard, net: Option<i64>) -> bool {
        let Some(id) = net.filter(|id| *id != 0) else {
            return false;
        };
        if self.on_high {
            Some(id) == self.gnd || board.net(id).is_some_and(|n| is_ground(&n.name))
        } else {
            id == self.input
        }
    }
}

pub(super) fn classify(
    board: &ExtractedBoard,
    reg: &Reg<'_>,
    (o, q): (&Component, &Pin),
    (en, models): (i64, &HashMap<&str, ModelEntry>),
) -> Member {
    let model = models.get(o.reference.as_str());
    if std::ptr::eq(o, reg.comp) {
        // The regulator's own other pads: its input is a tie to VIN (handled
        // before here); its output or bypass pad holds no level of its own
        // while the part is off.
        return Member::Cannot(format!("{}'s own pad {}", o.reference, q.number));
    }
    if is_capacitor(o) {
        return Member::Cannot(o.reference.clone());
    }
    let far: Vec<Option<i64>> = o
        .pins
        .iter()
        .filter(|p| p.net != Some(en))
        .map(|p| p.net.filter(|id| *id != 0))
        .collect();
    let class = RefClass::of(&o.reference);
    let series = matches!(
        class,
        RefClass::Resistor | RefClass::FerriteBead | RefClass::Inductor
    );
    if series && o.pins.len() == 2 {
        return match far.first().copied().flatten() {
            None => Member::Cannot(format!("{} (its other end is on no net)", o.reference)),
            Some(id) if reg.is_off_rail(board, Some(id)) => {
                let to = board.net(id).map_or("ground", |n| n.name.as_str());
                let ohms = match class {
                    RefClass::Resistor => resistor_ohms(o),
                    // A ferrite bead or inductor is a wire at DC.
                    _ => Some(0.0),
                };
                Member::OffPull {
                    label: format!("{} to '{to}'", o.reference),
                    ohms,
                }
            }
            Some(_) => Member::MaySet,
        };
    }
    if class == RefClass::Switch {
        // A switch that only ever closes onto the off rail cannot turn the
        // part on; one that can reach anything else may be the power switch.
        return if far
            .iter()
            .all(|n| n.is_none() || reg.is_off_rail(board, *n))
        {
            Member::Cannot(format!(
                "{} (it switches only to the off level)",
                o.reference
            ))
        } else {
            Member::MaySet
        };
    }
    if is_connector_like(o) {
        // An off-board signal may drive it: an external enable, not a fault.
        return Member::External(format!("{} pin {}", o.reference, q.number));
    }
    if class == RefClass::Transistor {
        return match low_side_sink(o, q, model) {
            Some(label) if reg.on_high => Member::Cannot(label),
            _ => Member::MaySet,
        };
    }
    // Another part's own enable or shutdown input sets nothing.
    let role = model
        .and_then(|m| m.pins.get(&q.number))
        .map(|r| r.to_ascii_lowercase());
    let input = q.kind.trim().eq_ignore_ascii_case("input");
    if matches!(role.as_deref(), Some("en" | "shdn"))
        || (input && matches!(control_role(&q.function), Some("enable" | "reset")))
    {
        return Member::Cannot(format!(
            "{} pin {} (an enable input)",
            o.reference, q.number
        ));
    }
    // Another part's reset input, whatever alternate function the symbol
    // names it with (`PC6(/RESET)`), takes a level and sets none.
    if names_reset_input(q) {
        return Member::Cannot(format!("{} pin {} (a reset input)", o.reference, q.number));
    }
    Member::MaySet
}

/// Whether a pad is a micro's reset input: named for the reset after its port
/// name (`PC6(/RESET)`, `PA0/NRST`), where the symbol shares the pad with the
/// pin's port function, and not an output. A bare `RESET` of unstated type
/// may be a supervisor's output and is left to the caller.
fn names_reset_input(q: &Pin) -> bool {
    if q.kind.trim().to_ascii_lowercase().contains("output") {
        return false;
    }
    let name = q.function.trim().to_ascii_uppercase();
    let is_reset = |t: &str| t.contains("RESET") || matches!(t, "RST" | "NRST" | "MCLR");
    let mut tokens = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty());
    let first_is_reset = tokens.next().is_some_and(is_reset);
    !first_is_reset && tokens.any(is_reset)
}

/// What every other pad on a control net does to the pin's level.
#[derive(Default)]
pub(super) struct Survey {
    /// Pads that cannot raise the pin, as the finding reads them.
    pub(super) held: Vec<String>,
    /// The resistance of each resistor (or wire-like part) holding the pin
    /// at its off level; `None` for a value that does not read as one.
    pub(super) pulls: Vec<Option<f64>>,
    /// Connector pads: an off-board source may drive the net.
    pub(super) external: Vec<String>,
    /// Parts on the net that are not fitted.
    pub(super) unfitted: Vec<String>,
}

/// Classify every other fitted pad on net `en`. `None` when anything on it may
/// raise the pin to its on level, or when a record on it is refused: nothing
/// about an unknown is evidence of a fault.
pub(super) fn survey_net(
    board: &ExtractedBoard,
    reg: &Reg<'_>,
    (comp, pad): (&Component, &str),
    (en, models): (i64, &HashMap<&str, ModelEntry>),
) -> Option<Survey> {
    let mut out = Survey::default();
    for (o, q) in board.net_members(en) {
        if std::ptr::eq(o, comp) && q.number == pad {
            continue;
        }
        match AssemblyState::of(o) {
            AssemblyState::DnpAbsent(_) => {
                out.unfitted.push(o.reference.clone());
                continue;
            }
            // Nothing about a refused record is evidence either way.
            AssemblyState::IdentityUnknown(_) => return None,
            AssemblyState::Present(_) => {}
        }
        if is_test_point_or_hole(o) {
            continue;
        }
        match classify(board, reg, (o, q), (en, models)) {
            Member::MaySet => return None,
            Member::Cannot(how) => out.held.push(how),
            Member::OffPull { label, ohms } => {
                out.held.push(label);
                out.pulls.push(ohms);
            }
            Member::External(label) => out.external.push(label),
        }
    }
    Some(out)
}

/// One gating finding per fitted linear regulator whose card says an open
/// enable pin is off and whose board gives that pin nothing that can take it
/// to its on level.
#[must_use]
pub fn regulator_enable_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    let mut models: HashMap<&str, ModelEntry> = HashMap::new();
    let mut regulators = Vec::new();
    for comp in &board.components {
        let Some(part) = AssemblyState::of(comp).fitted() else {
            continue;
        };
        let res = resolve(lib, part);
        if matches!(res.confidence, Confidence::Guessed | Confidence::Unresolved) {
            continue;
        }
        let Some(model) = res.model else { continue };
        if model.kind == ComponentKind::Vreg
            && model.behavioral.converter.is_none()
            && super::card_fits(board, comp, &model)
        {
            regulators.push((comp, model.clone()));
        }
        models.insert(comp.reference.as_str(), model);
    }
    for (comp, model) in regulators {
        if let Some(finding) = judge(board, comp, &model, &models) {
            report.findings.push(finding);
        }
    }
    report
}

/// The concrete fix: pull or tie the pin to its on level.
fn remedy(label: &str, on_high: bool, in_name: &str) -> String {
    let to = if on_high {
        format!("up to the input '{in_name}' through 10k–100k, or tie it to '{in_name}'")
    } else {
        "down to ground through 10k–100k, or tie it to ground".to_string()
    };
    format!(
        "Pull {label} {to}; if a GPIO or logic output is meant to switch the rail, keep that \
         pull so the rail has a defined state while the driver is not yet driving."
    )
}

pub(super) fn decapitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map_or_else(String::new, |f| f.to_lowercase().chain(c).collect())
}

/// " (R1 on the net is not fitted)", or nothing when every part is fitted.
pub(super) fn unfitted_note(unfitted: &[String]) -> String {
    match unfitted {
        [] => String::new(),
        [one] => format!(" ({one} on the net is not fitted)"),
        many => format!(" ({} on the net are not fitted)", many.join(", ")),
    }
}

fn judge(
    board: &ExtractedBoard,
    comp: &Component,
    model: &ModelEntry,
    models: &HashMap<&str, ModelEntry>,
) -> Option<LintFinding> {
    let (role, pad) = ["en", "shdn"].into_iter().find_map(|role| {
        model
            .pins
            .iter()
            .find(|(_, r)| r.eq_ignore_ascii_case(role))
            .map(|(pad, _)| (role, pad.clone()))
    })?;
    let on_high = model.params.get_str("en_on_level") != Some("low");
    let off = OffWhenOpen::of(model, on_high)?;
    let pin = comp.pins.iter().find(|p| p.number == pad)?;
    // Without the rail and its input on the board the finding could not
    // name what stays off; a record that lacks them is not judged.
    let input = role_net_id(comp, model, "in")?;
    let out = role_net_id(comp, model, "out")?;
    let out_name = board.net(out)?.name.clone();
    let in_name = board.net(input)?.name.clone();
    let reg = Reg {
        comp,
        on_high,
        input,
        gnd: role_net_id(comp, model, "gnd"),
    };
    let label = if pin.function.trim().is_empty() {
        role.to_ascii_uppercase()
    } else {
        pin.function.trim().to_string()
    };
    let net = pin.net.filter(|id| *id != 0);
    let survey = match net {
        Some(en) => {
            // Tied to the input (on for an on-high pin) or to its off rail (a
            // deliberate choice the bind report already names): not judged.
            if en == input || reg.is_off_rail(board, Some(en)) {
                return None;
            }
            survey_net(board, &reg, (comp, &pin.number), (en, models))?
        }
        None => Survey::default(),
    };
    let Survey {
        held,
        pulls,
        external,
        unfitted,
    } = survey;
    // A connector on the net is an off-board source that may drive the pin:
    // with nothing on the board pulling it off the pin is an external enable
    // input, and the board is not judged.
    if !external.is_empty() && pulls.is_empty() {
        return None;
    }
    let net_name = net.and_then(|id| board.net(id)).map(|n| n.name.clone());
    let topology = match (&net_name, held.is_empty()) {
        (None, _) => "is on no net".to_string(),
        (Some(n), true) => format!("is alone on net '{n}'"),
        (Some(n), false) => format!(
            "is on net '{n}', which holds only {}, and none of them can take it to its on level",
            held.join(", ")
        ),
    };
    let unfitted = unfitted_note(&unfitted);
    let vout = model
        .params
        .get_f64("vout")
        .map_or_else(String::new, |v| format!(" ({v:.2} V)"));
    let fix = remedy(&label, on_high, &in_name);
    let (severity, message) = if external.is_empty() {
        (
            Severity::High,
            format!(
                "{} ({}) {label} pad {} {topology}{unfitted}: {}, so the regulator stays off and its \
                 output '{out_name}'{vout} never comes on; everything powered from '{out_name}' is \
                 unpowered. {REMEDY_MARKER}{fix}",
                comp.reference,
                comp.value.trim(),
                pin.number,
                off.sentence(&model.id, &label),
            ),
        )
    } else {
        (
            Severity::Medium,
            format!(
                "{} ({}) {label} pad {} {topology}{unfitted}; the only thing on it that can raise \
                 it is {}, an off-board source, so unless that source always drives the pin the \
                 board holds the regulator off and its output '{out_name}'{vout} never comes on. \
                 {REMEDY_MARKER}If an external enable is meant to switch this rail, confirm it \
                 drives the pin whenever the rail is needed; otherwise {}",
                comp.reference,
                comp.value.trim(),
                pin.number,
                external.join(", "),
                decapitalise(&fix),
            ),
        )
    };
    let mut nets: Vec<String> = net_name.into_iter().collect();
    nets.push(out_name);
    Some(LintFinding {
        check: LintCheck::RegulatorHeldOff,
        severity,
        message,
        refs: vec![comp.reference.clone()],
        nets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::test_support::{board, part};

    const VIN: i64 = 1;
    const V3: i64 = 2;
    const GND: i64 = 3;
    const EN: i64 = 4;
    const GPIO: i64 = 5;

    /// MIC5205-3.3 (SOT-23-5: 1 IN, 2 GND, 3 EN, 4 BYP, 5 OUT) from +5V to
    /// +3V3 with its EN pad on `en` and `extra` parts. Microchip DS20005785B
    /// Table 3-1 p.9: "Logic-high = enable, logic-low or open = shutdown".
    fn mic5205(value: &str, en: Option<i64>, extra: Vec<Component>) -> Vec<LintFinding> {
        let mut reg = part(
            "U1",
            value,
            &[("1", VIN), ("2", GND), ("3", EN), ("4", 6), ("5", V3)],
        );
        reg.pins[2].net = en;
        let mut parts = vec![
            reg,
            part("C1", "1u", &[("1", VIN), ("2", GND)]),
            part("C2", "2.2u", &[("1", V3), ("2", GND)]),
        ];
        parts.extend(extra);
        let nets = [
            (VIN, "+5V"),
            (V3, "+3V3"),
            (GND, "GND"),
            (EN, "/LDO_EN"),
            (GPIO, "/MCU_PA3"),
            (6, "Net-(U1-BYP)"),
        ];
        regulator_enable_lint(&board(&nets, parts), &ModelLibrary::builtin())
            .of_check(LintCheck::RegulatorHeldOff)
            .cloned()
            .collect()
    }

    const MIC: &str = "MIC5205-3.3YM5";

    fn dnp(mut c: Component) -> Component {
        c.dnp = true;
        c
    }

    fn says(f: &LintFinding, all: &[&str]) {
        for s in all {
            assert!(f.message.contains(s), "{s:?} in {}", f.message);
        }
    }

    #[test]
    fn an_enable_left_open_on_an_off_when_open_part_holds_the_rail_off() {
        let found = mic5205(MIC, Some(EN), vec![]);
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.refs, ["U1"]);
        assert_eq!(f.nets, ["/LDO_EN", "+3V3"]);
        says(
            f,
            &[
                "U1 (MIC5205-3.3YM5) EN pad 3 is alone on net '/LDO_EN'",
                "logic-low or open = shutdown",
                "card 'mic5205_3v3'",
                "output '+3V3' (3.30 V) never comes on",
            ],
        );
        assert_eq!(
            f.own_remedy().map(|r| r
                .starts_with("Pull EN up to the input '+5V' through 10k–100k, or tie it to '+5V'")),
            Some(true),
            "{}",
            f.message
        );
        // A pad on no net at all is just as open.
        let none = mic5205(MIC, None, vec![]);
        assert_eq!(none.len(), 1);
        says(&none[0], &["EN pad 3 is on no net"]);
        assert_eq!(none[0].nets, ["+3V3"]);
    }

    #[test]
    fn an_unfitted_pull_up_leaves_the_enable_open() {
        let pull = dnp(part("R1", "100k", &[("1", VIN), ("2", EN)]));
        let rc = part("C3", "100n", &[("1", EN), ("2", GND)]);
        let down = part("R2", "1M", &[("1", EN), ("2", GND)]);
        let found = mic5205(MIC, Some(EN), vec![pull, rc, down]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(
            &found[0],
            &[
                "which holds only C3, R2 to 'GND'",
                "(R1 on the net is not fitted)",
            ],
        );
    }

    /// The legal controls: EN tied to VIN, pulled up, driven from a GPIO, on
    /// a header or a switch to a rail, and tied to GND on purpose.
    #[test]
    fn an_enable_the_board_can_raise_is_silent() {
        assert!(mic5205(MIC, Some(VIN), vec![]).is_empty(), "tied to VIN");
        let pull = part("R1", "100k", &[("1", VIN), ("2", EN)]);
        assert!(mic5205(MIC, Some(EN), vec![pull]).is_empty(), "pulled up");
        let series = part("R1", "1k", &[("1", GPIO), ("2", EN)]);
        let mcu = part("U2", "SomeMCU", &[("7", GPIO)]);
        assert!(
            mic5205(MIC, Some(EN), vec![series, mcu]).is_empty(),
            "driven by a GPIO through a resistor"
        );
        let mcu = part("U2", "SomeMCU", &[("7", EN)]);
        assert!(
            mic5205(MIC, Some(EN), vec![mcu]).is_empty(),
            "driven by an IC pin"
        );
        let header = part("J1", "Conn_01x03", &[("1", EN), ("2", GND)]);
        assert!(
            mic5205(MIC, Some(EN), vec![header]).is_empty(),
            "external enable"
        );
        let switch = part("SW1", "SPDT", &[("1", VIN), ("2", EN), ("3", GND)]);
        assert!(
            mic5205(MIC, Some(EN), vec![switch]).is_empty(),
            "power switch"
        );
        assert!(
            mic5205(MIC, Some(GND), vec![]).is_empty(),
            "tied off on purpose"
        );
    }

    #[test]
    fn a_switch_or_low_side_transistor_to_ground_cannot_raise_it() {
        let switch = part("SW1", "SW_Push", &[("1", EN), ("2", GND)]);
        let found = mic5205(MIC, Some(EN), vec![switch]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["SW1 (it switches only to the off level)"]);
        let mut q = part("Q1", "Q_NPN_BCE", &[("1", GPIO), ("2", EN), ("3", GND)]);
        q.lib_id = "Device:Q_NPN_BCE".into();
        q.pins[1].function = "C".into();
        let found = mic5205(MIC, Some(EN), vec![q]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["Q1's collector"]);
    }

    /// A pull-down to the off level with a header as the only thing that can
    /// raise the pin is a Medium finding naming the header (it may carry an
    /// enable, so it is not the High "nothing can raise it"). The same header
    /// with no pull-down is an external enable input, silent (the legal
    /// control above).
    #[test]
    fn a_pull_down_and_a_header_is_a_medium_finding_that_names_the_header() {
        let pull = part("R1", "100k", &[("1", EN), ("2", GND)]);
        let header = part("JP3", "Conn_01x02", &[("1", EN), ("2", GND)]);
        let found = mic5205(MIC, Some(EN), vec![pull, header]);
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(f.severity, Severity::Medium);
        says(
            f,
            &[
                "which holds only R1 to 'GND'",
                "the only thing on it that can raise it is JP3 pin 1, an off-board source",
                "output '+3V3' (3.30 V) never comes on",
            ],
        );
        assert!(f.own_remedy().is_some(), "{}", f.message);
        // A ferrite bead or inductor to ground is a 0 ohm pull-down too.
        let bead = part("FB1", "BLM18", &[("1", EN), ("2", GND)]);
        let found = mic5205(MIC, Some(EN), vec![bead]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
    }

    /// A card that does not say what an open pin does is never read as off,
    /// and one whose pin is on when open is silent.
    #[test]
    fn a_card_without_an_off_open_state_is_not_judged() {
        // The same open EN pad against a test card stating each open state.
        let lint = |tag: &str, enable: &str| {
            let card = format!(
                "[[models]]\nid = \"en_test_{tag}\"\nkind = \"vreg\"\n\
                 description = \"enable fixture\"\n[models.match]\n\
                 value_re = \"^ENTEST-{tag}$\"\n[models.params]\nvout = 3.3\n\
                 dropout_v = 0.3\niq_a = 5.0e-5\n{enable}\n[models.pins]\n\
                 \"1\" = \"in\"\n\"2\" = \"gnd\"\n\"3\" = \"en\"\n\"5\" = \"out\"\n"
            );
            let dir = std::env::temp_dir().join(format!(
                "hauksbee_regulator_enable_{tag}_{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("models.toml"), card).unwrap();
            let mut lib = ModelLibrary::builtin();
            let errors = lib.load_user_dir(&dir);
            assert!(errors.is_empty(), "{tag}: {errors:?}");
            let reg = part(
                "U1",
                &format!("ENTEST-{tag}"),
                &[("1", VIN), ("2", GND), ("3", EN), ("5", V3)],
            );
            let nets = [(VIN, "+5V"), (V3, "+3V3"), (GND, "GND"), (EN, "/EN")];
            regulator_enable_lint(&board(&nets, vec![reg]), &lib).findings
        };
        assert!(lint("UNSTATED", "").is_empty(), "unknown open state");
        assert!(lint("ON", "en_open = \"on\"").is_empty(), "on when open");
        assert!(
            lint("PULLUP", "en_pull_up_ohms = 1.0e6").is_empty(),
            "pulled on inside the part"
        );
        // A pull toward the off level (an internal pull-down) decides it too.
        let found = lint("PULLDOWN", "en_pull_down_ohms = 3.0e6");
        assert_eq!(found.len(), 1, "{found:#?}");
        says(
            &found[0],
            &["internal pull-down (3 MΩ, card 'en_test_PULLDOWN')"],
        );
        // The card's datasheet quote is carried into the finding.
        let found = lint(
            "QUOTED",
            "en_open = \"off\"\nen_open_basis = \"p.3: EN low or open = shutdown\"",
        );
        assert_eq!(found.len(), 1, "{found:#?}");
        says(
            &found[0],
            &["(p.3: EN low or open = shutdown; card 'en_test_QUOTED')"],
        );
    }
}
