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
//! level is a connector is a non-gating note, not a High finding, and only
//! when the board itself pulls the pin to the off level (with no pull at all
//! the pin is simply an external enable input): a default-off enable brought
//! out to a header is a common deliberate design. A jumper or header of at
//! most three pads with another pad on a live net (a solder jumper from EN to
//! VIN) may close onto that net, so it may set the level. A part referenced
//! like a resistor whose value is not a resistance (a reset button `RESET`),
//! a switch named by its symbol or footprint, and any other card-less part
//! whose other pads reach only the off rail cannot raise the pin. A card
//! that states no open-pin state, or states the pin is on when open
//! (`en_open = "on"`, an internal pull toward on), is not judged here: an
//! unknown is never read as off. A pull-down that holds such a pin off is
//! `control_held_off`'s, which judges the divider against the internal pull.
//!
//! A second fault needs no card statement about the open pin: an enable whose
//! only source is the device's own output (see `self_fed_enable`). The
//! output is 0 V until the part is on, so an on-high enable wired to it, or to
//! any net downstream of it through resistors, ferrites, diodes or another
//! regulator, can never rise and the part can never start. It is judged for
//! every regulator and load switch card with an enable and an output pad, and
//! only when nothing else on that downstream island can set the level.

use std::collections::HashMap;

use hauksbee_bind::binder::{is_ground, resolve};
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    control_role, is_capacitor, is_connector_like, is_test_point_or_hole, Component,
    ExtractedBoard, LintCheck, LintFinding, NetLintReport, Pin, Severity, REMEDY_MARKER,
};
use hauksbee_models::refdes::RefClass;
use hauksbee_models::{CapSide, ComponentKind, Confidence, ModelEntry, ModelLibrary};

use super::envelope::role_net_id;
use super::model_controls::low_side_sink;
use hauksbee_ir::units::{format_amps, format_volts};

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
                format_amps(v)
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

/// What the card says an enable pin does when nothing drives it, in the part's
/// own terms (on or off), whichever level is the on level.
pub(super) enum OpenState {
    /// An open pin holds the part off: `en_open = "off"`, or an internal pull
    /// toward the off level.
    Off,
    /// An open pin turns the part on, with the card's reason: `en_open = "on"`,
    /// or an internal pull toward the on level.
    On(String),
    /// The datasheet says the pin must not be left open
    /// (`en_open = "undefined"`), with the card's quote when it carries one.
    Undefined(Option<String>),
    /// The card states nothing about an open pin.
    Unknown,
}

impl OpenState {
    /// The open-pin state of the enable pin of `model`, which is on at its high
    /// level when `on_high`. An internal pull decides first (it is what an open
    /// pin settles to), then the datasheet's stated open state.
    pub(super) fn of(model: &ModelEntry, on_high: bool) -> Self {
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
            if up != on_high {
                return Self::Off;
            }
            let value = if key.ends_with("_ohms") {
                ohms(v)
            } else {
                format_amps(v)
            };
            return Self::On(format!(
                "the part's internal {} ({value}) turns an open pin on",
                if up { "pull-up" } else { "pull-down" }
            ));
        }
        let quote = params
            .get_str("en_open_basis")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        match params.get_str("en_open") {
            Some("off") => Self::Off,
            Some("on") => Self::On(quote.map_or_else(
                || "the datasheet states an open pin turns the part on (en_open = \"on\")".into(),
                |q| format!("the datasheet states an open pin turns the part on ({q})"),
            )),
            Some("undefined") => Self::Undefined(quote),
            _ => Self::Unknown,
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

/// Whether a schematic pin's electrical type says the pin drives its net: an
/// output, a tri-state pin, or an open-collector / open-drain / open-emitter
/// pin (`KiCad` `output`, `tri_state`, `open_collector`, `open_emitter`;
/// Eagle `out`, `oc`, `hiz`). A reset or enable of that type is an output
/// (a supervisor's or a bridge's reset line), never an input to judge.
pub(super) fn pin_drives(q: &Pin) -> bool {
    let k = q.kind.trim().to_ascii_lowercase().replace(['-', ' '], "_");
    k.contains("output")
        || k.contains("open_collector")
        || k.contains("open_drain")
        || k.contains("open_emitter")
        || k.contains("tri_state")
        || k.contains("tristate")
        || matches!(k.as_str(), "out" | "oc" | "hiz")
}

/// Whether a card role names a port or general-purpose I/O pad (`gpio4`,
/// `io12`, `pa3`, `pc6_reset`): firmware may drive it.
fn role_is_port(role: &str) -> bool {
    let b = role.as_bytes();
    role.starts_with("gpio")
        || (role.starts_with("io") && b.get(2).is_some_and(u8::is_ascii_digit))
        || (b.len() >= 3 && b[0] == b'p' && b[1].is_ascii_lowercase() && b[2].is_ascii_digit())
}

/// A push button or tactile switch, by its reference class or by a word of
/// the symbol, footprint or value naming one (`SW_Push`, `EVQ-P7`,
/// `TACTILE`, `PUSHBUTTON`, `BTN`): boards label a reset button with refs
/// like `RESET` that read as a resistor by prefix alone. Whole words only, so
/// a battery `Contact` is not a tactile switch.
pub(super) fn is_switch(o: &Component) -> bool {
    if RefClass::of(&o.reference) == RefClass::Switch {
        return true;
    }
    let blob = format!("{} {} {}", o.lib_id, o.footprint, o.value).to_ascii_uppercase();
    blob.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| {
        matches!(w, "SW" | "BTN")
            || ["TACT", "EVQ", "PUSH", "BUTTON"]
                .iter()
                .any(|p| w.starts_with(p))
    })
}

pub(super) fn classify(
    board: &ExtractedBoard,
    reg: &Reg<'_>,
    (o, q): (&Component, &Pin),
    (en, models): (i64, &HashMap<&str, ModelEntry>),
) -> Member {
    let model = models.get(o.reference.as_str());
    let role = model
        .and_then(|m| m.pins.get(&q.number))
        .map(|r| r.to_ascii_lowercase());
    if std::ptr::eq(o, reg.comp) {
        // The part's own other pads: a GPIO or output of its own may drive
        // the net. A regulator's input is a tie to VIN (handled before
        // here); its output or bypass pad holds no level of its own while
        // the part is off.
        if pin_drives(q)
            || q.kind.trim().eq_ignore_ascii_case("bidirectional")
            || role.as_deref().is_some_and(role_is_port)
        {
            return Member::MaySet;
        }
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
    let only_off = far
        .iter()
        .all(|n| n.is_none() || reg.is_off_rail(board, *n));
    let class = RefClass::of(&o.reference);
    // A part whose reference reads as a resistor but whose value is not a
    // resistance (a reset button referenced `RESET`) is not priced as one.
    let series = match class {
        RefClass::Resistor => resistor_ohms(o).is_some(),
        RefClass::FerriteBead | RefClass::Inductor => true,
        _ => false,
    };
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
    if is_switch(o) {
        // A switch that only ever closes onto the off rail cannot turn the
        // part on; one that can reach anything else may be the power switch.
        return if only_off {
            Member::Cannot(format!(
                "{} (it switches only to the off level)",
                o.reference
            ))
        } else {
            Member::MaySet
        };
    }
    if is_connector_like(o) {
        // A jumper or small header with another pad on a live net (a solder
        // jumper from EN to VIN, a 3-pin selector) closes onto that net when
        // fitted: it may set the level on the board itself.
        if o.pins.len() <= 3 && !only_off {
            return Member::MaySet;
        }
        // An off-board signal may drive it: an external enable or reset.
        return Member::External(format!("{} pin {}", o.reference, q.number));
    }
    if class == RefClass::Transistor {
        return match low_side_sink(o, q, model) {
            Some(label) if reg.on_high => Member::Cannot(label),
            _ => Member::MaySet,
        };
    }
    // A part with no card role on this pad and no IC body whose other pads
    // reach only the off rail (a diode or TVS to ground, a button whose value
    // is not a resistance) can only take the pin to its off level.
    if class != RefClass::ActiveIc
        && role.is_none()
        && o.pins.len() >= 2
        && only_off
        && far.iter().any(Option::is_some)
    {
        return Member::Cannot(format!(
            "{} (its other pads reach only the off level)",
            o.reference
        ));
    }
    // Another part's own enable or shutdown input sets nothing.
    let input = q.kind.trim().eq_ignore_ascii_case("input");
    let is_other_enable = match (role.as_deref(), model.and_then(roles_of)) {
        (Some(r), Some(roles)) => roles.enable.contains(&r),
        (Some("en" | "shdn"), None) => true,
        _ => false,
    };
    if is_other_enable || (input && matches!(control_role(&q.function), Some("enable" | "reset"))) {
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
    if pin_drives(q) {
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

/// A fitted part's card, when it resolves by more than a guess.
fn confident_model(lib: &ModelLibrary, comp: &Component) -> Option<ModelEntry> {
    let part = AssemblyState::of(comp).fitted()?;
    let res = resolve(lib, part);
    if matches!(res.confidence, Confidence::Guessed | Confidence::Unresolved) {
        return None;
    }
    res.model
}

/// The card roles of a part this check judges: where its enable, input,
/// output and ground sit.
#[derive(Clone, Copy)]
pub(super) struct Roles {
    enable: &'static [&'static str],
    input: &'static str,
    output: &'static str,
    gnd: &'static str,
}

/// A linear regulator: enable `en` (else `shdn`), input `in`, output `out`.
const REGULATOR: Roles = Roles {
    enable: &["en", "shdn"],
    input: "in",
    output: "out",
    gnd: "gnd",
};

/// A load switch carded as an analog switch: enable `ctrl`, input `in_out_b`,
/// switched output `in_out_a`.
const LOAD_SWITCH: Roles = Roles {
    enable: &["ctrl"],
    input: "in_out_b",
    output: "in_out_a",
    gnd: "vss",
};

fn has_role(model: &ModelEntry, role: &str) -> bool {
    model.pins.values().any(|r| r.eq_ignore_ascii_case(role))
}

/// Whether a card is a load switch: an analog-switch entry that states its
/// enable is active high (`en_on_level = "high"`, which only a load switch
/// carries) and maps an enable, an input and a switched output. A signal
/// switch or multiplexer states no enable level and is never judged here.
pub(super) fn is_load_switch(model: &ModelEntry) -> bool {
    model.kind == ComponentKind::AnalogSwitch
        && model.params.get_str("en_on_level") == Some("high")
        && [LOAD_SWITCH.enable[0], LOAD_SWITCH.input, LOAD_SWITCH.output]
            .into_iter()
            .all(|r| has_role(model, r))
}

/// The roles this check reads on a card, or `None` for a part it does not judge.
fn roles_of(model: &ModelEntry) -> Option<Roles> {
    match model.kind {
        ComponentKind::Vreg => Some(REGULATOR),
        ComponentKind::AnalogSwitch if is_load_switch(model) => Some(LOAD_SWITCH),
        _ => None,
    }
}

/// A simple linear regulator or a load switch whose card fits the footprint:
/// this check's subject.
fn is_judged_part(board: &ExtractedBoard, comp: &Component, model: &ModelEntry) -> bool {
    let simple = match model.kind {
        ComponentKind::Vreg => model.behavioral.converter.is_none(),
        ComponentKind::AnalogSwitch => is_load_switch(model),
        _ => false,
    };
    simple && super::card_fits(board, comp, model)
}

/// The card's enable pad: its role (`en`, else `shdn`; a load switch's
/// `ctrl`), pad number and whether the pin is on at its high level.
fn enable_pad(model: &ModelEntry) -> Option<(&'static str, String, bool)> {
    let roles = roles_of(model)?;
    let (role, pad) = roles.enable.iter().find_map(|role| {
        model
            .pins
            .iter()
            .find(|(_, r)| r.eq_ignore_ascii_case(role))
            .map(|(pad, _)| (*role, pad.clone()))
    })?;
    let on_high = model.params.get_str("en_on_level") != Some("low");
    Some((role, pad, on_high))
}

/// The enable pin's name for a finding when the symbol names none.
fn enable_label(role: &str) -> String {
    if role == "ctrl" {
        "EN".to_string()
    } else {
        role.to_ascii_uppercase()
    }
}

/// The net of the card role `pick` selects from the part's roles.
fn net_of(comp: &Component, model: &ModelEntry, pick: fn(&Roles) -> &'static str) -> Option<i64> {
    role_net_id(comp, model, pick(&roles_of(model)?))
}

/// The enable pad this check judges on `comp`, when it judges one: a fitted
/// simple linear regulator whose card says an open enable is off, with the
/// pad, its input and its output on the board. `control_held_off` leaves
/// exactly that pad to this check and judges every other regulator enable
/// (a card silent on the open state, or one pulled on inside the part), so a
/// pull-down nothing releases is never missed by both and never reported by
/// both.
pub(super) fn judged_enable_pad(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    comp: &Component,
) -> Option<String> {
    let model = confident_model(lib, comp)?;
    if !is_judged_part(board, comp, &model) {
        return None;
    }
    let (_, pad, on_high) = enable_pad(&model)?;
    OffWhenOpen::of(&model, on_high)?;
    if !comp.pins.iter().any(|p| p.number == pad) {
        return None;
    }
    board.net(net_of(comp, &model, |r| r.input)?)?;
    board.net(net_of(comp, &model, |r| r.output)?)?;
    Some(pad)
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
        let Some(model) = confident_model(lib, comp) else {
            continue;
        };
        if is_judged_part(board, comp, &model) {
            regulators.push((comp, model.clone()));
        }
        models.insert(comp.reference.as_str(), model);
    }
    for (comp, model) in regulators {
        if let Some(finding) = judge(board, comp, &model, &models) {
            report.findings.push(finding);
        }
    }
    // The self-fed enable is judged on every regulator and load switch card,
    // simple or not; a pin the open-pin judgement reports is reported once.
    for comp in &board.components {
        if report
            .findings
            .iter()
            .any(|f| f.refs == [comp.reference.clone()])
        {
            continue;
        }
        if let Some(finding) = self_fed_enable(board, comp, &models) {
            report.findings.push(finding);
        }
    }
    report
}

/// How one pin on a net of the output's downstream island reads.
enum Island {
    /// A load, a capacitor or a return: it sets no level of its own.
    Load,
    /// A series part carrying the island on to this net.
    Carries(i64),
    /// Anything that might source a level: the island is not judged.
    Source,
}

/// The far end of a two-pin series part `o` entered at `q`: a return or a
/// dangling end is a load, any other net is carried on.
fn series_far(board: &ExtractedBoard, (o, q): (&Component, &Pin), net: i64) -> Island {
    let others: Vec<Option<i64>> = o
        .pins
        .iter()
        .filter(|p| p.number != q.number)
        .map(|p| p.net.filter(|id| *id != 0))
        .collect();
    let [far] = others.as_slice() else {
        return Island::Source;
    };
    match *far {
        None => Island::Load,
        Some(id) if id == net => Island::Load,
        Some(id) if board.net(id).is_some_and(|n| is_ground(&n.name)) => Island::Load,
        Some(id) => Island::Carries(id),
    }
}

fn island_member(board: &ExtractedBoard, (o, q): (&Component, &Pin), net: i64) -> Island {
    if is_test_point_or_hole(o) || is_capacitor(o) {
        return Island::Load;
    }
    // A connector pad, whatever its drawn type, may be fed from off the board.
    if is_connector_like(o) {
        return Island::Source;
    }
    if o.pins.len() == 2 {
        match RefClass::of(&o.reference) {
            RefClass::Resistor if resistor_ohms(o).is_some() => {
                return series_far(board, (o, q), net);
            }
            RefClass::FerriteBead | RefClass::Inductor => return series_far(board, (o, q), net),
            RefClass::Diode => {
                // Conducts from its anode only: a diode into the island is a source.
                let f = q.function.trim().to_ascii_uppercase();
                return if matches!(f.as_str(), "A" | "ANODE") {
                    series_far(board, (o, q), net)
                } else {
                    Island::Source
                };
            }
            _ => {}
        }
    }
    match q.kind.trim().to_ascii_lowercase().as_str() {
        "power_in" | "input" => Island::Load,
        _ => Island::Source,
    }
}

/// The nets downstream of `out` through series parts and other regulators fed
/// from them, when every pin on them reads as a load (see [`Island`]). `None`
/// when anything on them might source a level, or a record is refused.
fn output_island(
    board: &ExtractedBoard,
    (reg, out): (&Component, i64),
    models: &HashMap<&str, ModelEntry>,
) -> Option<std::collections::BTreeSet<i64>> {
    let mut island = std::collections::BTreeSet::from([out]);
    let mut queue = vec![out];
    let mut pending: Vec<&Component> = Vec::new();
    while let Some(net) = queue.pop() {
        for (o, q) in board.net_members(net) {
            match AssemblyState::of(o) {
                AssemblyState::DnpAbsent(_) => continue,
                AssemblyState::IdentityUnknown(_) => return None,
                AssemblyState::Present(_) => {}
            }
            let role = models
                .get(o.reference.as_str())
                .and_then(|m| m.pins.get(&q.number))
                .map(|r| r.to_ascii_lowercase());
            if std::ptr::eq(o, reg) {
                // The part's own output sources the island, on every pad of it
                // (a tab or a second output pad the card does not name too);
                // no other pad may drive or share it.
                let own_out = net == out;
                if !own_out
                    && (pin_drives(q) || q.kind.trim().eq_ignore_ascii_case("bidirectional"))
                {
                    return None;
                }
                continue;
            }
            // Another regulator or load switch fed from the island carries it on
            // to its output.
            if let Some((m, r)) = models
                .get(o.reference.as_str())
                .and_then(|m| roles_of(m).map(|r| (m, r)))
            {
                match role.as_deref() {
                    Some(x) if x == r.input => {
                        let next = role_net_id(o, m, r.output)?;
                        if island.insert(next) {
                            queue.push(next);
                        }
                        continue;
                    }
                    Some(x) if x == r.gnd || r.enable.contains(&x) => continue,
                    Some("fb" | "bypass") if m.kind == ComponentKind::Vreg => continue,
                    _ => return None,
                }
            }
            match island_member(board, (o, q), net) {
                Island::Load => {}
                Island::Carries(next) => {
                    if island.insert(next) {
                        queue.push(next);
                    }
                }
                Island::Source => {
                    if !pending.iter().any(|p: &&Component| std::ptr::eq(*p, o)) {
                        pending.push(o);
                    }
                }
            }
        }
    }
    // A part that might source a level is no source when everything that
    // powers it is on the island: it is dead until the regulator runs (an MCU
    // whose GPIO pulls an I2C line up from the rail, a sensor on the rail).
    pending
        .iter()
        .all(|o| powered_only_by(board, o, &island))
        .then_some(island)
}

/// Whether every supply pad of `o` is on the island: it has at least one
/// supply pad on a non-ground net, none of them off the island, and it is not
/// a connector (an off-board source). A part with no typed supply pad is not
/// read as island-powered.
fn powered_only_by(
    board: &ExtractedBoard,
    o: &Component,
    island: &std::collections::BTreeSet<i64>,
) -> bool {
    if is_connector_like(o) {
        return false;
    }
    let mut supplied = false;
    for p in &o.pins {
        if !p.kind.trim().eq_ignore_ascii_case("power_in") {
            continue;
        }
        let Some(id) = p.net.filter(|id| *id != 0) else {
            continue;
        };
        if board.net(id).is_some_and(|n| is_ground(&n.name)) {
            continue;
        }
        if !island.contains(&id) {
            return false;
        }
        supplied = true;
    }
    supplied
}

/// The on-high enable of a device that has an input and an output, with the
/// roles of both.
struct SelfFedGate {
    role: &'static str,
    pad: String,
    in_role: String,
    out_role: String,
}

/// The enable `self_fed_enable` judges on a card: a regulator's on-high `en`
/// or `shdn`, or a single-channel load switch's `ctrl` (every load switch card
/// is active-high: an active-low option is excluded from its match rule by
/// name). A load switch's two terminals are named `in_out_a` and `in_out_b`
/// with no direction, so the input is the one the card gives an input-side
/// `[[models.supply_cap]]` row; with none the output is unknown and the
/// switch is not judged.
fn self_fed_gate(model: &ModelEntry) -> Option<SelfFedGate> {
    match model.kind {
        ComponentKind::Vreg => {
            let (role, pad, on_high) = enable_pad(model)?;
            on_high.then(|| SelfFedGate {
                role,
                pad,
                in_role: "in".into(),
                out_role: "out".into(),
            })
        }
        ComponentKind::AnalogSwitch => {
            let has = |role: &str| model.pins.values().any(|r| r.eq_ignore_ascii_case(role));
            let pad = model
                .pins
                .iter()
                .find(|(_, r)| r.eq_ignore_ascii_case("ctrl"))
                .map(|(pad, _)| pad.clone())?;
            if !(has("in_out_a") && has("in_out_b")) {
                return None;
            }
            let inputs: Vec<&str> = model
                .supply_cap
                .iter()
                .filter(|c| c.side == CapSide::Input)
                .map(|c| c.role.as_str())
                .filter(|r| matches!(*r, "in_out_a" | "in_out_b"))
                .collect();
            let [input] = inputs.as_slice() else {
                return None;
            };
            let out_role = if *input == "in_out_a" {
                "in_out_b"
            } else {
                "in_out_a"
            };
            Some(SelfFedGate {
                role: "ctrl",
                pad,
                in_role: (*input).to_string(),
                out_role: out_role.to_string(),
            })
        }
        _ => None,
    }
}

/// A regulator or load switch whose on-high enable is fed only by the part's
/// own output: wired to it directly, or to a net downstream of it that nothing
/// else drives. The output is 0 V until the part runs, so the enable never
/// rises and the part never starts. It is the one finding for a pin whose
/// card is silent on an open enable; a pin the open-pin judgement already
/// reports is reported once there.
fn self_fed_enable(
    board: &ExtractedBoard,
    comp: &Component,
    models: &HashMap<&str, ModelEntry>,
) -> Option<LintFinding> {
    let model = models.get(comp.reference.as_str())?;
    if !super::card_fits(board, comp, model) {
        return None;
    }
    let gate = self_fed_gate(model)?;
    let (role, pad) = (gate.role, gate.pad);
    let pin = comp.pins.iter().find(|p| p.number == pad)?;
    let en = pin.net.filter(|id| *id != 0)?;
    let out = role_net_id(comp, model, &gate.out_role)?;
    let input = role_net_id(comp, model, &gate.in_role);
    let island = output_island(board, (comp, out), models)?;
    if !island.contains(&en) || input.is_some_and(|i| island.contains(&i)) {
        return None;
    }
    let name = |id: i64| board.net(id).map_or_else(String::new, |n| n.name.clone());
    let (en_name, out_name) = (name(en), name(out));
    let label = if pin.function.trim().is_empty() {
        enable_label(role)
    } else {
        pin.function.trim().to_string()
    };
    let how = if en == out {
        format!("is wired straight to its own output '{out_name}'")
    } else {
        format!(
            "is on net '{en_name}', which is fed only from its own output '{out_name}' through \
             series parts, and nothing else on it can drive it"
        )
    };
    let fix = input.map(name).map_or_else(
        || {
            format!(
                "Tie {label} to the part's input rail, or pull it up to that rail through \
                 10k-100k, or drive it from a GPIO or supervisor."
            )
        },
        |inp| {
            format!(
                "Tie {label} to the input '{inp}', or pull it up to '{inp}' through 10k-100k, \
                 or drive it from a GPIO or supervisor."
            )
        },
    );
    let mut nets = vec![en_name];
    if !nets.contains(&out_name) {
        nets.push(out_name.clone());
    }
    Some(LintFinding {
        check: LintCheck::RegulatorHeldOff,
        severity: Severity::High,
        message: format!(
            "{} ({}) {label} pad {} {how}: the output is 0 V until the part is enabled and the \
             enable is high only when the output is, so the part can never start and \
             '{out_name}' never comes on; everything powered from it is unpowered. \
             {REMEDY_MARKER}{fix}",
            comp.reference,
            comp.value.trim(),
            pin.number,
        ),
        refs: vec![comp.reference.clone()],
        nets,
    })
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
        many => format!(
            " ({} on the net are not fitted)",
            hauksbee_ir::vocabulary::names_summary(many)
        ),
    }
}

fn judge(
    board: &ExtractedBoard,
    comp: &Component,
    model: &ModelEntry,
    models: &HashMap<&str, ModelEntry>,
) -> Option<LintFinding> {
    let (role, pad, on_high) = enable_pad(model)?;
    let off = OffWhenOpen::of(model, on_high)?;
    let pin = comp.pins.iter().find(|p| p.number == pad)?;
    // Without the rail and its input on the board the finding could not
    // name what stays off; a record that lacks them is not judged.
    let roles = roles_of(model)?;
    let input = role_net_id(comp, model, roles.input)?;
    let out = role_net_id(comp, model, roles.output)?;
    let out_name = board.net(out)?.name.clone();
    let in_name = board.net(input)?.name.clone();
    let reg = Reg {
        comp,
        on_high,
        input,
        gnd: role_net_id(comp, model, roles.gnd),
    };
    let label = if pin.function.trim().is_empty() {
        enable_label(role)
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
        .map_or_else(String::new, |v| format!(" ({})", format_volts(v)));
    let fix = remedy(&label, on_high, &in_name);
    let noun = if is_load_switch(model) {
        "load switch"
    } else {
        "regulator"
    };
    let (severity, message) = if external.is_empty() {
        (
            Severity::High,
            format!(
                "{} ({}) {label} pad {} {topology}{unfitted}: {}, so the {noun} stays off and its \
                 output '{out_name}'{vout} never comes on; everything powered from '{out_name}' is \
                 unpowered. {REMEDY_MARKER}{fix}",
                comp.reference,
                comp.value.trim(),
                pin.number,
                off.sentence(&model.id, &label),
            ),
        )
    } else {
        // A default-off enable brought out to a connector with a local
        // pull-down is a common, deliberate design (the host or module
        // switches the rail): the board alone cannot tell it from a fault,
        // so it is a non-gating note naming the connector.
        (
            Severity::Low,
            format!(
                "{} ({}) {label} pad {} {topology}{unfitted}; the only thing on it that can raise \
                 it is {}, an off-board source, so unless that source always drives the pin the \
                 board holds the {noun} off and its output '{out_name}'{vout} never comes on. \
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
                "output '+3V3' (3.3 V) never comes on",
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
        // "Default off, fit the jumper to enable": a 2-pad jumper from EN to
        // VIN beside a 100k pull-down closes onto the input when fitted.
        let jumper = part("JP1", "SolderJumper_2_Open", &[("1", EN), ("2", VIN)]);
        let pull = part("R2", "100k", &[("1", EN), ("2", GND)]);
        assert!(
            mic5205(MIC, Some(EN), vec![jumper, pull]).is_empty(),
            "jumper to VIN"
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
    /// raise the pin is a non-gating note naming the header (a default-off
    /// enable brought out to a header is a common deliberate design, so it is
    /// not the High "nothing can raise it"). The same header with no
    /// pull-down is an external enable input, silent (the legal control
    /// above).
    #[test]
    fn a_pull_down_and_a_header_is_a_note_that_names_the_header() {
        let pull = part("R1", "100k", &[("1", EN), ("2", GND)]);
        let header = part("JP3", "Conn_01x02", &[("1", EN), ("2", GND)]);
        let found = mic5205(MIC, Some(EN), vec![pull, header]);
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(f.severity, Severity::Low);
        says(
            f,
            &[
                "which holds only R1 to 'GND'",
                "the only thing on it that can raise it is JP3 pin 1, an off-board source",
                "output '+3V3' (3.3 V) never comes on",
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

    /// A regulator whose card is silent on an open enable (so the open-pin
    /// judgement abstains), `U1` from `+5V` to `+3V3` with EN on `en`, plus
    /// `extra` parts, judged by [`regulator_enable_lint`].
    fn self_fed(tag: &str, en: i64, extra: Vec<Component>) -> Vec<LintFinding> {
        let reg = part(
            "U1",
            &format!("SFTEST-{tag}"),
            &[("1", VIN), ("2", GND), ("3", en), ("5", V3)],
        );
        self_fed_with(tag, reg, extra)
    }

    /// As [`self_fed`], with the regulator part `reg` given.
    fn self_fed_with(tag: &str, reg: Component, extra: Vec<Component>) -> Vec<LintFinding> {
        let card = format!(
            "[[models]]\nid = \"sf_test_{tag}\"\nkind = \"vreg\"\n\
             description = \"self-fed enable fixture\"\n[models.match]\n\
             value_re = \"^SFTEST-{tag}$\"\n[models.params]\nvout = 3.3\n\
             dropout_v = 0.3\niq_a = 5.0e-5\n[models.pins]\n\
             \"1\" = \"in\"\n\"2\" = \"gnd\"\n\"3\" = \"en\"\n\"5\" = \"out\"\n"
        );
        let dir = std::env::temp_dir().join(format!(
            "hauksbee_regulator_selffed_{tag}_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("models.toml"), card).unwrap();
        let mut lib = ModelLibrary::builtin();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "{tag}: {errors:?}");
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
            (EN, "/EN"),
            (GPIO, "/MCU_PA3"),
        ];
        regulator_enable_lint(&board(&nets, parts), &lib).findings
    }

    /// An MCU with a supply pad on `supply` and a GPIO on `/MCU_PA3`, pulled
    /// up to `+3V3` through `R7`: the usual I2C pull-up on a regulator's rail.
    fn mcu_with_pull_up(supply: i64) -> Vec<Component> {
        let mut mcu = part("U3", "SomeMCU", &[("1", supply), ("2", GND), ("7", GPIO)]);
        mcu.pins[0].kind = "power_in".into();
        mcu.pins[1].kind = "power_in".into();
        mcu.pins[2].kind = "bidirectional".into();
        vec![mcu, part("R7", "4.7k", &[("1", V3), ("2", GPIO)])]
    }

    #[test]
    fn a_self_fed_enable_is_flagged_through_parts_only_the_rail_powers() {
        // An MCU hanging a GPIO on the rail through a pull-up is powered by
        // that rail alone, so it cannot raise the enable before the part runs.
        let mut parts = vec![load_on(V3)];
        parts.extend(mcu_with_pull_up(V3));
        let found = self_fed("RAILMCU", V3, parts);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
        says(&found[0], &["its own output '+3V3'", "can never start"]);
        // A tab or second output pad the card does not name is the part's own output.
        let mut tab = part(
            "U1",
            "SFTEST-TABBED",
            &[("1", VIN), ("2", GND), ("3", V3), ("5", V3), ("6", V3)],
        );
        tab.pins[4].kind = "power_out".into();
        let found = self_fed_with("TABBED", tab, vec![load_on(V3)]);
        assert_eq!(found.len(), 1, "{found:#?}");
    }

    #[test]
    fn a_self_fed_enable_stays_silent_when_a_part_powered_elsewhere_hangs_on_the_rail() {
        // The same pull-up, but the MCU on the line is powered from the input
        // rail, so it can drive the line (and through the resistor the enable).
        let mut parts = vec![load_on(V3)];
        parts.extend(mcu_with_pull_up(VIN));
        assert!(
            self_fed("OTHERMCU", V3, parts).is_empty(),
            "powered elsewhere"
        );
        // A part with a supply on each of two rails is also powered elsewhere.
        let mut parts = vec![load_on(V3)];
        parts.extend(mcu_with_pull_up(V3));
        let mut both = part("U5", "TwoRail", &[("1", V3), ("2", VIN), ("3", GPIO)]);
        both.pins[0].kind = "power_in".into();
        both.pins[1].kind = "power_in".into();
        both.pins[2].kind = "bidirectional".into();
        parts.push(both);
        assert!(self_fed("TWORAIL", V3, parts).is_empty(), "second supply");
        // A connector on the rail is an off-board source.
        let mut header = part("J1", "Conn_01x02", &[("1", V3), ("2", GND)]);
        header.pins[0].kind = "power_in".into();
        assert!(
            self_fed("HDR", V3, vec![load_on(V3), header]).is_empty(),
            "a connector"
        );
    }

    /// An IC load on `net`: a supply pad, which draws and sets nothing.
    fn load_on(net: i64) -> Component {
        let mut load = part("U2", "SomeIC", &[("1", net), ("2", GND)]);
        load.pins[0].kind = "power_in".into();
        load.pins[1].kind = "power_in".into();
        load
    }

    #[test]
    fn an_enable_wired_to_the_regulators_own_output_can_never_start() {
        let found = self_fed("DIRECT", V3, vec![load_on(V3)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(f.check, LintCheck::RegulatorHeldOff);
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.refs, ["U1"]);
        assert_eq!(f.nets, ["+3V3"]);
        says(
            f,
            &[
                "U1 (SFTEST-DIRECT) EN pad 3 is wired straight to its own output '+3V3'",
                "can never start",
            ],
        );
        assert!(
            f.own_remedy()
                .is_some_and(|r| r.starts_with("Tie EN to the input '+5V'")),
            "{}",
            f.message
        );
    }

    #[test]
    fn an_enable_fed_only_through_series_parts_from_the_output_can_never_start() {
        // OUT -> 10k -> EN, with a pull-down on EN: still only the output feeds it.
        let r = part("R1", "10k", &[("1", V3), ("2", EN)]);
        let pull = part("R2", "100k", &[("1", EN), ("2", GND)]);
        let found = self_fed("SERIES", EN, vec![r, pull, load_on(V3)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(
            &found[0],
            &["is on net '/EN', which is fed only from its own output '+3V3'"],
        );
        assert_eq!(found[0].nets, ["/EN", "+3V3"]);
    }

    /// AP2553 load switch (SOT-26: 1 IN, 2 GND, 3 EN, 4 FAULT, 5 ILIM, 6 OUT)
    /// from `+5V` to `+3V3` with EN on `en`, plus `extra` parts. The card names
    /// the terminals `in_out_b` (IN, the input-side supply capacitor row) and
    /// `in_out_a` (OUT); EN is `ctrl`, active high (DS35404 Rev. 12-3).
    fn load_switch(en: i64, extra: Vec<Component>) -> Vec<LintFinding> {
        let mut parts = vec![
            part(
                "U1",
                "AP2553W6-7",
                &[("1", VIN), ("2", GND), ("3", en), ("6", V3)],
            ),
            part("C1", "1u", &[("1", VIN), ("2", GND)]),
            part("C2", "1u", &[("1", V3), ("2", GND)]),
            part("R9", "10k", &[("1", VIN), ("2", 7)]),
        ];
        parts.extend(extra);
        let nets = [
            (VIN, "+5V"),
            (V3, "+3V3_SW"),
            (GND, "GND"),
            (EN, "/EN"),
            (GPIO, "/MCU_PA3"),
            (7, "/FAULT"),
        ];
        regulator_enable_lint(&board(&nets, parts), &ModelLibrary::builtin())
            .of_check(LintCheck::RegulatorHeldOff)
            .cloned()
            .collect()
    }

    #[test]
    fn a_load_switch_enable_wired_to_its_own_output_can_never_start() {
        // Straight to OUT, and through a series resistor from OUT.
        let found = load_switch(V3, vec![load_on(V3)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.refs, ["U1"]);
        says(
            f,
            &[
                "U1 (AP2553W6-7)",
                "is wired straight to its own output '+3V3_SW'",
                "can never start",
            ],
        );
        assert!(
            f.own_remedy()
                .is_some_and(|r| r.starts_with("Tie") && r.contains("the input '+5V'")),
            "{}",
            f.message
        );
        let r = part("R1", "10k", &[("1", V3), ("2", EN)]);
        let found = load_switch(EN, vec![r, load_on(V3)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(
            &found[0],
            &["is on net '/EN', which is fed only from its own output '+3V3_SW'"],
        );
    }

    /// A real LDO card that states an open enable is off (MIC5205) with EN on
    /// its own output, directly or through a resistor, is reported once.
    #[test]
    fn a_carded_ldo_enable_on_its_own_output_is_flagged_once() {
        let found = mic5205(MIC, Some(V3), vec![load_on(V3)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
        says(&found[0], &["its own output '+3V3'", "can never start"]);
        let r = part("R1", "100k", &[("1", V3), ("2", EN)]);
        let found = mic5205(MIC, Some(EN), vec![r, load_on(V3)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["fed only from its own output '+3V3'"]);
    }

    #[test]
    fn a_load_switch_enable_something_else_can_drive_is_silent() {
        let silent = |en: i64, extra: Vec<Component>| {
            let mut parts = vec![load_on(V3)];
            parts.extend(extra);
            load_switch(en, parts)
        };
        assert!(silent(VIN, vec![]).is_empty(), "tied to the input");
        // A GPIO enables the switch, with a pull-down for the idle state.
        let mut mcu = part("U3", "SomeMCU", &[("7", EN)]);
        mcu.pins[0].kind = "bidirectional".into();
        let pull = part("R2", "100k", &[("1", EN), ("2", GND)]);
        assert!(silent(EN, vec![mcu, pull]).is_empty(), "a GPIO drives it");
        // Pulled up to the input through a resistor.
        let up = part("R3", "100k", &[("1", VIN), ("2", EN)]);
        assert!(silent(EN, vec![up]).is_empty(), "pulled up to the input");
    }

    #[test]
    fn an_enable_something_else_can_drive_is_silent() {
        let on = |tag: &str, en: i64, extra: Vec<Component>| {
            let mut parts = vec![load_on(V3)];
            parts.extend(extra);
            self_fed(tag, en, parts)
        };
        assert!(on("VIN", VIN, vec![]).is_empty(), "tied to the input");
        // A divider from the output that is also pulled up to the input.
        let r = part("R1", "10k", &[("1", V3), ("2", EN)]);
        let up = part("R2", "100k", &[("1", VIN), ("2", EN)]);
        assert!(on("PULLED", EN, vec![r, up]).is_empty(), "pulled up to VIN");
        // A GPIO on the line.
        let r = part("R1", "10k", &[("1", V3), ("2", EN)]);
        let mut mcu = part("U3", "SomeMCU", &[("7", EN)]);
        mcu.pins[0].kind = "bidirectional".into();
        assert!(on("GPIO", EN, vec![r, mcu]).is_empty(), "a GPIO drives it");
        // A header: an external enable.
        let r = part("R1", "10k", &[("1", V3), ("2", EN)]);
        let header = part("J1", "Conn_01x02", &[("1", EN), ("2", GND)]);
        assert!(on("HEADER", EN, vec![r, header]).is_empty(), "a header");
        // Another source on the output rail itself.
        let mut other = part("U4", "OtherReg", &[("1", V3)]);
        other.pins[0].kind = "power_out".into();
        assert!(on("SECOND", V3, vec![other]).is_empty(), "a second source");
        // A load with no stated pin type is not read as a load.
        assert!(
            self_fed("UNTYPED", V3, vec![part("U2", "SomeIC", &[("1", V3)])]).is_empty(),
            "an unknown pin"
        );
    }

    /// A load switch carded as an analog switch: `value` from `+5V` (IN, role
    /// `in_out_b`) to `+3V3` (OUT, `in_out_a`) with its enable pad on `en` and
    /// `extra` parts. `pads` is (OUT, IN, GND, ON).
    fn card_load_switch(
        value: &str,
        pads: [&str; 4],
        en: Option<i64>,
        extra: Vec<Component>,
    ) -> Vec<LintFinding> {
        let [out, input, gnd, on] = pads;
        let mut sw = part(
            "U1",
            value,
            &[(out, V3), (input, VIN), (gnd, GND), (on, EN)],
        );
        sw.pins[3].net = en;
        let mut parts = vec![sw, part("C1", "1u", &[("1", VIN), ("2", GND)])];
        parts.extend(extra);
        let nets = [
            (VIN, "+5V"),
            (V3, "+3V3"),
            (GND, "GND"),
            (EN, "/SW_EN"),
            (GPIO, "/MCU_PA3"),
        ];
        regulator_enable_lint(&board(&nets, parts), &ModelLibrary::builtin())
            .of_check(LintCheck::RegulatorHeldOff)
            .cloned()
            .collect()
    }

    const TPS22916: &str = "TPS22916YFP";
    const TPS22916_PADS: [&str; 4] = ["A1", "A2", "B1", "B2"];

    /// TPS22916 (SLVSDO5F PDF p.1: smart ON-pin pull-down, 750 kOhm): an ON pin
    /// nothing can raise holds the switch off for good, as a regulator's does.
    #[test]
    fn a_load_switch_enable_nothing_can_raise_holds_it_off() {
        let found = card_load_switch(TPS22916, TPS22916_PADS, Some(EN), vec![]);
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.refs, ["U1"]);
        says(
            f,
            &[
                "U1 (TPS22916YFP) EN pad B2 is alone on net '/SW_EN'",
                "internal pull-down (750 kΩ, card 'tps22916')",
                "so the load switch stays off and its output '+3V3' never comes on",
            ],
        );
        assert!(
            f.own_remedy().is_some_and(|r| r
                .starts_with("Pull EN up to the input '+5V' through 10k–100k, or tie it to '+5V'")),
            "{}",
            f.message
        );
        // A pull-down alone is no better.
        let down = part("R1", "100k", &[("1", EN), ("2", GND)]);
        assert_eq!(
            card_load_switch(TPS22916, TPS22916_PADS, Some(EN), vec![down]).len(),
            1
        );
    }

    #[test]
    fn a_load_switch_enable_the_board_can_raise_is_silent() {
        let silent = |en: Option<i64>, extra: Vec<Component>| {
            card_load_switch(TPS22916, TPS22916_PADS, en, extra).is_empty()
        };
        // Tied to the input rail, pulled up to it, or driven by a GPIO.
        assert!(silent(Some(VIN), vec![]));
        assert!(silent(
            Some(EN),
            vec![part("R1", "100k", &[("1", EN), ("2", VIN)])]
        ));
        let mut mcu = part("U3", "SomeMCU", &[("7", EN)]);
        mcu.pins[0].kind = "bidirectional".into();
        assert!(silent(Some(EN), vec![mcu]));
        // A signal analog switch (a 74LVC1G66) has a `ctrl` pad and no enable
        // level: it is not a load switch and is never judged here.
        let sig = card_load_switch("74LVC1G66", ["1", "2", "3", "4"], Some(EN), vec![]);
        assert!(sig.is_empty(), "{sig:#?}");
    }

    /// The part's own output is 0 V until it runs: an enable wired to it
    /// can never rise (AP2553: active-high EN, no open-pin statement).
    #[test]
    fn a_load_switch_enable_fed_from_its_own_output_can_never_start() {
        let ap2553 = |en| card_load_switch("AP2553W6-7", ["6", "1", "2", "3"], Some(en), vec![]);
        let found = ap2553(V3);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(
            &found[0],
            &[
                "U1 (AP2553W6-7) EN pad 3 is wired straight to its own output '+3V3'",
                "can never start",
            ],
        );
        assert!(ap2553(VIN).is_empty(), "tied to the input rail");
    }
}
