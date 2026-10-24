//! An enable or reset input the board holds at its off level for good.
//!
//! A regulator's open enable pin is `regulator_enable`'s subject. This check
//! is the other half of "held off permanently": a part whose active-high
//! enable (an ESP32 `EN`, a converter's `CE`) or active-low reset (`NRST`,
//! `~{RESET}`) has a resistor to ground and nothing on the net that can take
//! it to the on level. The pull exists to hold the pin off until something
//! releases it, and on this net nothing does: the part is never enabled, or is
//! held in reset and never runs. That is what a pull-up whose far end was
//! wired to ground, or one that was never fitted behind a fitted pull-down,
//! looks like.
//!
//! What can release the pin keeps the check silent: a pull to any net that is
//! not the off rail, a GPIO or logic output, a supervisor's output, a switch
//! or transistor that can reach a rail, or any part no rule here knows (the
//! classifier is shared with `regulator_enable`). A button to ground, a
//! capacitor, a low-side transistor and another part's own enable or reset
//! input cannot. A connector pad is an off-board source that may drive the
//! pin, so a net whose only way to the on level is a connector is not a High
//! fault: on an enable it is a non-gating note (a default-off enable brought
//! out to a header is a common deliberate design), on a reset a Medium
//! finding (a debug header's reset line only pulls low). A jumper or header
//! of at most three pads with another pad on a live net may close onto it and
//! keeps the board silent.
//!
//! Polarity and direction are never guessed. An enable is active-high when
//! its card role is `en`/`enable`/`chip_en`/`chip_pu` (and the card does not
//! say `en_on_level = "low"`); without a card only Espressif's `CHIP_PU` /
//! `CHIP_EN` (one sense only) is judged, never a bare `EN`, `CE`, `CEN` or
//! `ENABLE` (a memory's chip enable is active-low and normally pulled down).
//! A reset is an active-low input when the pin is on an MCU card's `reset`
//! role (every MCU card's reset role is the active-low reset input), or on a
//! `_n` card role that the card shows is an input (listed in
//! `must_not_float_roles`, a printed `reset_pull_up_ohms`, or a symbol typing
//! the pin `input`): cards give `rst_n` to open-drain reset outputs as well.
//! Without a card a reset is judged only when spelled active-low
//! (`~{RESET}`, `!RESET`, `RESET#`, `nRST`, `NRST`, `RESET_N`). A pin the
//! symbol types as an output, tri-state or open collector / drain is never
//! judged. A trailing `_<digits>` pad suffix (`CHIP_PU_3`) is ignored.
//!
//! The opposite polarities are judged the same way, mirrored, only when the
//! card states them: an enable the card says runs the part on a low level
//! (`en_on_level = "low"`, or a role that names itself active-low, `en_n`) and
//! a reset the card says holds the part in reset while high
//! (`reset_input.active_high`). A pull to the part's own supply with nothing on
//! the net that can take the pin low is the wrong rail for them: the part is
//! never enabled, or never leaves reset. A name or a symbol never sets these
//! polarities, and a card that is silent leaves the pin unjudged.
//! Anything else is not judged.
//!
//! An internal pull-up inside the part can still win against a weak external
//! pull-down, so the check computes the level: the pin sits at most
//! `Rpd / (Rpd + Rint)` of the supply. It fires only when that is at or below
//! 0.3, under every logic family's input-high floor. `Rint` is the card's
//! printed pull-up (`reset_pull_up_ohms`, `en_pull_up_ohms`, a minimum), none
//! at all when the card lists the role in `must_not_float_roles` (the
//! datasheet gives the pin no pull), and otherwise an assumed 10 kOhm floor,
//! so a part whose datasheet is silent is judged only against a stiff
//! pull-down.
//!
//! A second rule judges the other side of "nothing releases it": an enable
//! that only an MCU pad drives and nothing holds (`enable_floats_in_reset`).
//! The pad is high impedance while its MCU is in reset (unless the MCU card
//! lists the pad's reset mode, see `reset_state`), so the enable floats through
//! reset and power-up, and what the part does then is the card's open-pin
//! state (`regulator_enable::OpenState`): off is safe, on or undefined is a
//! Medium finding, and a card silent on it is a Medium finding only for a
//! part that is not a plain regulator. A pull to either rail, a fitted
//! resistor of any value, a switch, header or jumper, or any non-MCU part on
//! the net leaves the net to the other checks.
//!
//! The enable pad of a linear regulator whose card says an open enable is
//! off is `regulator_enable`'s; every other regulator enable is judged here,
//! and the two never report one pad. The extract
//! layer's generic floating-control note covers a control pin alone on its
//! net.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    control_role, is_connector_like, is_ground, Component, ExtractedBoard, LintCheck, LintFinding,
    NetLintReport, Pin, Severity, REMEDY_MARKER,
};
use hauksbee_models::netclass;
use hauksbee_models::refdes::RefClass;
use hauksbee_models::{ComponentKind, ModelEntry, ModelLibrary};

use super::envelope::role_net_id;
use super::floating_inputs::Classifier;
use super::indicators::name_suggests_active_low;
use super::regulator_enable::{
    decapitalise, is_load_switch, judged_enable_pad, ohms, pin_drives, survey_net, unfitted_note,
    OpenState, Reg, Survey,
};
use super::reset_state::{reset_rest, ResetRest};

/// The weakest internal pull-up assumed when a card prints none and does not
/// say the pin has none.
const ASSUMED_INTERNAL_PULL_OHMS: f64 = 10_000.0;

/// The highest fraction of the supply a held-off pin may sit at for the
/// check to call it never valid-high.
const NEVER_HIGH_FRACTION: f64 = 0.3;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Enable,
    Reset,
}

impl Kind {
    /// What the part does while the pin is held off.
    const fn consequence(self) -> &'static str {
        match self {
            Self::Enable => "the part is never enabled and everything it drives stays dead",
            Self::Reset => "the part is held in reset and never runs",
        }
    }

    /// The level the pin needs to be released.
    const fn on_level(self) -> &'static str {
        match self {
            Self::Enable => "enable level",
            Self::Reset => "reset-released level",
        }
    }
}

/// The part's internal pull toward the on level, from its card.
enum InternalPull {
    /// The datasheet gives the pin no pull (`must_not_float_roles`).
    Absent,
    /// A printed minimum resistance, ohms.
    Known(f64),
    /// The card says nothing.
    Unknown,
}

struct Control {
    kind: Kind,
    /// The pin's name for the finding.
    label: String,
    /// The card's role for the pad, lower case.
    role: Option<String>,
    /// Whether the part runs with the pin at its HIGH level. An active-high
    /// enable and an active-low reset do; an active-low enable and an
    /// active-high reset, whose polarity only the card can give, run low and
    /// are held off by a pull to the supply.
    run_high: bool,
}

/// An Eagle-style `!RESET` spelling reads as the active-low reset it is.
fn bare(function: &str) -> &str {
    function.trim().trim_start_matches('!')
}

fn reset_role(role: &str) -> bool {
    role.split('_')
        .any(|t| matches!(t, "reset" | "rst" | "nrst" | "resetn"))
}

fn enable_role(role: &str) -> bool {
    matches!(role, "en" | "enable" | "chip_en" | "chip_pu")
}

/// A card role that names itself an active-low enable.
fn active_low_enable_role(role: &str) -> bool {
    matches!(role, "en_n" | "enable_n")
}

/// Whether the card gives the reset role the opposite of the usual polarity:
/// the part is held in reset while the pin reads high.
fn card_reset_active_high(model: &ModelEntry, role: &str) -> bool {
    model
        .reset_input
        .as_ref()
        .is_some_and(|r| r.active_high && r.role.eq_ignore_ascii_case(role))
}

/// A pin function without a trailing `_<digits>` pad suffix (`CHIP_PU_3`,
/// `~{RESET}_2`): some module symbols number repeated names that way.
fn without_pad_suffix(function: &str) -> &str {
    let f = function.trim();
    match f.rsplit_once('_') {
        Some((head, tail))
            if !head.is_empty() && !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) =>
        {
            head
        }
        _ => f,
    }
}

/// Whether the card says a reset-role pad is an input: an MCU's reset role,
/// a role the datasheet gives no pull and requires driven
/// (`must_not_float_roles`), or a printed internal pull-up
/// (`reset_pull_up_ohms`), which only an input has. Cards also give the
/// `rst_n` role to open-drain reset outputs (a USB bridge's, a timer's, a
/// supervisor's), so a role alone says nothing about direction.
fn card_reset_input(model: &ModelEntry, role: &str) -> bool {
    model.kind == ComponentKind::Mcu
        || model
            .params
            .get_str("must_not_float_roles")
            .unwrap_or_default()
            .split(',')
            .any(|r| r.trim().eq_ignore_ascii_case(role))
        || model
            .params
            .get_f64("reset_pull_up_ohms")
            .is_some_and(|r| r.is_finite() && r > 0.0)
}

/// Whether a pin function spells an active-low reset.
fn reset_spelled_active_low(function: &str) -> bool {
    let f = function.trim();
    if f.starts_with('!') || name_suggests_active_low(f) {
        return true;
    }
    let u = f.to_ascii_uppercase();
    matches!(
        u.as_str(),
        "NRST" | "NRESET" | "RSTN" | "RESETN" | "RESET_N" | "RST_N"
    )
}

/// Which control the pad is and whether it is one this check judges: an
/// active-high enable or an active-low reset.
fn control_of(pin: &Pin, model: Option<&ModelEntry>) -> Option<Control> {
    let kind_text = pin.kind.trim().to_ascii_lowercase();
    if pin_drives(pin) || kind_text.contains("power") {
        return None;
    }
    let function = pin.function.trim();
    let role = model
        .and_then(|m| m.pins.get(&pin.number))
        .map(|r| r.to_ascii_lowercase());
    let label = |fallback: &str| {
        if function.is_empty() {
            fallback.to_string()
        } else {
            function.to_string()
        }
    };
    if let Some(r) = role.as_deref() {
        let model = model?;
        if reset_role(r) {
            // The card says the part resets on a high level: held in reset by
            // a pull-up, released by a low.
            if card_reset_active_high(model, r) {
                return Some(Control {
                    kind: Kind::Reset,
                    label: label("RESET"),
                    role: role.clone(),
                    run_high: false,
                });
            }
            let low = model.kind == ComponentKind::Mcu
                || r.ends_with("_n")
                || r.contains("nrst")
                || reset_spelled_active_low(function);
            // Direction from the card; a symbol that types the pin as an
            // input says so too. Anything else may be a reset output.
            let input = card_reset_input(model, r) || matches!(kind_text.as_str(), "input" | "in");
            return (low && input).then(|| Control {
                kind: Kind::Reset,
                label: label("RESET"),
                role: role.clone(),
                run_high: true,
            });
        }
        if enable_role(r) || active_low_enable_role(r) || (r == "ctrl" && is_load_switch(model)) {
            // The card's polarity: `en_on_level = "low"` or a role that names
            // itself active-low runs the part on a low level. A symbol that
            // only hints at the opposite polarity is not enough to judge.
            let card_low =
                model.params.get_str("en_on_level") == Some("low") || active_low_enable_role(r);
            let run_high = !card_low;
            return (card_low || !name_suggests_active_low(function)).then(|| Control {
                kind: Kind::Enable,
                label: label("EN"),
                role: role.clone(),
                run_high,
            });
        }
        // The card maps this pad to something else; a symbol's name for it
        // is not evidence of a control.
        return None;
    }
    // No card: a name alone never sets a polarity, except a reset spelled
    // active-low and Espressif's chip enable, which has only one sense.
    // `EN`, `CE`, `CEN` and `ENABLE` are not judged without a card: a memory's
    // chip enable is active-low and is normally held selected by a pull-down.
    let name = without_pad_suffix(function);
    if matches!(name.to_ascii_uppercase().as_str(), "CHIP_PU" | "CHIP_EN") {
        return Some(Control {
            kind: Kind::Enable,
            label: label("EN"),
            role: None,
            run_high: true,
        });
    }
    (control_role(bare(name)) == Some("reset") && reset_spelled_active_low(name)).then(|| Control {
        kind: Kind::Reset,
        label: label("RESET"),
        role: None,
        run_high: true,
    })
}

fn internal_pull(
    kind: Kind,
    role: Option<&str>,
    model: Option<&ModelEntry>,
    run_high: bool,
) -> InternalPull {
    let Some(model) = model else {
        return InternalPull::Unknown;
    };
    if let Some(role) = role {
        let declared = model
            .params
            .get_str("must_not_float_roles")
            .unwrap_or_default()
            .split(',')
            .any(|r| r.trim().eq_ignore_ascii_case(role));
        if declared {
            return InternalPull::Absent;
        }
    }
    // The printed values are pull-ups toward the high level; a pin that runs on
    // a low level has no printed pull toward it, so the floor is assumed.
    if !run_high {
        return InternalPull::Unknown;
    }
    let key = match kind {
        Kind::Enable => "en_pull_up_ohms",
        Kind::Reset => "reset_pull_up_ohms",
    };
    match model.params.get_f64(key) {
        Some(r) if r.is_finite() && r > 0.0 => InternalPull::Known(r),
        _ => InternalPull::Unknown,
    }
}

/// The parallel resistance of the pulls whose values read, `None` when none
/// does. A pull whose value does not read only lowers the true resistance,
/// so leaving it out can only under-state the hold: a level judged held
/// without it is held with it.
fn parallel(pulls: &[Option<f64>]) -> Option<f64> {
    let mut conductance = 0.0;
    for r in pulls.iter().flatten() {
        if *r <= 0.0 {
            return Some(0.0);
        }
        conductance += 1.0 / r;
    }
    (conductance > 0.0).then(|| 1.0 / conductance)
}

/// The finding's words on how far the pin can move toward its run level, or
/// `None` when the internal pull could take it to a valid one. `run_high` is
/// the level the part runs at: the held-off pull is a pull-down when it is
/// set, a pull-up when not.
fn level_claim(pull: &InternalPull, rext: f64, run_high: bool) -> Option<String> {
    let (held, own) = if run_high {
        ("pull-down", "pull-up")
    } else {
        ("pull-up", "pull-down")
    };
    let (rint, source) = match pull {
        InternalPull::Absent => {
            return Some(format!(
                "the part has no pull of its own on this pin, so the {held} alone sets its level"
            ));
        }
        InternalPull::Known(r) => (*r, format!("the card's internal {own} minimum")),
        InternalPull::Unknown => (
            ASSUMED_INTERNAL_PULL_OHMS,
            format!("an assumed internal {own} floor"),
        ),
    };
    // The pin's distance from the off rail, as a fraction of the supply.
    let fraction = rext / (rext + rint);
    (fraction <= NEVER_HIGH_FRACTION).then(|| {
        if run_high {
            format!(
                "even against {source} of {} the pin sits at no more than {fraction:.2} of the \
                 supply, below any valid high",
                ohms(rint),
            )
        } else {
            format!(
                "even against {source} of {} the pin sits at no less than {:.2} of the supply, \
                 above any valid low",
                ohms(rint),
                1.0 - fraction,
            )
        }
    })
}

/// The supply net the part's logic runs from: by its card's pin roles, else
/// the first positive supply rail on one of its pads.
fn supply_net<'a>(
    board: &'a ExtractedBoard,
    comp: &Component,
    model: Option<&ModelEntry>,
) -> Option<&'a hauksbee_extract::Net> {
    let by_card = model.and_then(|m| {
        ["vdd", "vcc", "dvdd", "vddio", "vdd_io", "3v3", "vcc_io"]
            .iter()
            .find_map(|role| role_net_id(comp, m, role))
    });
    by_card.and_then(|id| board.net(id)).or_else(|| {
        comp.pins
            .iter()
            .filter_map(|p| p.net.and_then(|id| board.net(id)))
            .find(|n| netclass::is_positive_supply(&n.name))
    })
}

fn supply_name(board: &ExtractedBoard, comp: &Component, model: Option<&ModelEntry>) -> String {
    supply_net(board, comp, model).map_or_else(
        || "the part's logic supply".to_string(),
        |n| format!("'{}'", n.name),
    )
}

/// One finding per enable or reset pin the board holds off permanently.
#[must_use]
pub fn control_held_off_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    let mut models: HashMap<&str, ModelEntry> = HashMap::new();
    for (comp, model) in super::resolved_components(board, lib) {
        models.insert(comp.reference.as_str(), model);
    }
    let by_ref: BTreeMap<&str, ModelEntry> = models.iter().map(|(k, v)| (*k, v.clone())).collect();
    for comp in &board.components {
        if !AssemblyState::of(comp).is_present() || is_connector_like(comp) {
            continue;
        }
        if !matches!(
            RefClass::of(&comp.reference),
            RefClass::ActiveIc | RefClass::Other
        ) {
            continue;
        }
        let model = models.get(comp.reference.as_str());
        // The enable pad `regulator_enable` judges (an off-when-open linear
        // regulator's) is its subject; every other regulator enable, one
        // whose card is silent on the open state or pulls it on inside the
        // part, is judged here.
        let theirs = model
            .filter(|m| m.kind == ComponentKind::Vreg || is_load_switch(m))
            .and_then(|_| judged_enable_pad(board, lib, comp));
        for pin in &comp.pins {
            if theirs.as_deref() == Some(pin.number.as_str()) {
                continue;
            }
            if let Some(f) = judge(board, comp, pin, model, &models) {
                report.findings.push(f);
            } else if let Some(f) = enable_floats_in_reset(board, comp, pin, model, &by_ref) {
                report.findings.push(f);
            }
        }
    }
    report
}

/// The finding for an enable only MCU pads drive, which floats while they are in
/// reset. `None` for anything else: a pin that is not a judged enable, a card
/// that says an open pin is off, or a net something other than an MCU pad holds.
fn enable_floats_in_reset(
    board: &ExtractedBoard,
    comp: &Component,
    pin: &Pin,
    model: Option<&ModelEntry>,
    models: &BTreeMap<&str, ModelEntry>,
) -> Option<LintFinding> {
    // Only a card'd part that is not itself an MCU: an MCU's enable-like pad is
    // its reset, and a part with no card has no open-pin state to read.
    let model = model.filter(|m| m.kind != ComponentKind::Mcu)?;
    let control = control_of(pin, Some(model))?;
    if control.kind != Kind::Enable {
        return None;
    }
    let net = board.net(pin.net.filter(|id| *id != 0)?)?;
    if is_ground(&net.name)
        || netclass::is_supply(&net.name)
        || hauksbee_extract::is_unconnected_net(&net.name)
    {
        return None;
    }
    let senses = BTreeSet::new();
    let classify = Classifier {
        board,
        models,
        senses: &senses,
        strict: true,
        open_jumpers: false,
    };
    let ResetRest::McuHiZ(pads) = reset_rest(&classify, comp, pin) else {
        return None;
    };
    let declared_no_float = control.role.as_deref().is_some_and(|role| {
        model
            .params
            .get_str("must_not_float_roles")
            .unwrap_or_default()
            .split(',')
            .any(|r| r.trim().eq_ignore_ascii_case(role))
    });
    let state = match OpenState::of(model, control.run_high) {
        // The card requires the pin driven but states nothing of an open pin.
        OpenState::Unknown if declared_no_float => OpenState::Undefined(Some(
            "the datasheet gives the pin no pull and requires it driven".to_string(),
        )),
        state => state,
    };
    let consequence = match state {
        OpenState::Off => return None,
        OpenState::On(why) => format!("{why}, so the part is switched on while the MCU is in reset"),
        OpenState::Undefined(quote) => format!(
            "the datasheet leaves an open pin undefined{}, so the part's state while the MCU is in reset is unknown",
            quote.map_or_else(String::new, |q| format!(" ({q})"))
        ),
        // A regulator or driver whose active-high enable the card is silent on
        // is left alone: a rail that is up or down for the length of a reset is
        // a design choice there. A load switch (its whole job is to be off until
        // asked) and any enable that runs on a low level have no such default.
        OpenState::Unknown
            if control.run_high && model.kind != ComponentKind::AnalogSwitch =>
        {
            return None;
        }
        OpenState::Unknown => format!(
            "card '{}' states nothing about an open pin and gives it no internal pull, so the part may be on while the MCU is in reset",
            model.id
        ),
    };
    let drivers = pads
        .iter()
        .map(|w| w.label.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let supply = supply_name(board, comp, Some(model));
    let label = &control.label;
    let fix = if control.run_high {
        format!(
            "Add a 100k pull-down from '{}' to ground so {label} rests off until firmware drives it.",
            net.name
        )
    } else {
        format!(
            "Add a 100k pull-up from '{}' to {supply} so {label} rests off until firmware drives it.",
            net.name
        )
    };
    Some(LintFinding {
        check: LintCheck::ControlHeldOff,
        severity: Severity::Medium,
        message: format!(
            "{} ({}) {label} pin {} is driven only by {drivers} on net '{}'. While that MCU is in reset its pad is high-impedance and nothing on the net pulls the pin, so {label} floats through power-up and reset: {consequence}. {REMEDY_MARKER}{fix}",
            comp.reference,
            comp.value.trim(),
            pin.number,
            net.name,
        ),
        refs: vec![comp.reference.clone()],
        nets: vec![net.name.clone()],
    })
}

/// What to change: return the pull to the level the part runs at, or drive the
/// pin from something that can release it.
fn fix_sentence(run_high: bool, supply: &str, label: &str) -> String {
    if run_high {
        format!(
            "Return the pull resistor to {supply} instead of ground (a pull-up), or drive {label} \
             from a GPIO or a supervisor; a pull to ground belongs on {label} only when something \
             else releases the pin."
        )
    } else {
        format!(
            "Return the pull resistor to ground instead of {supply} (a pull-down), or drive \
             {label} from a GPIO or a supervisor; a pull to {supply} belongs on {label} only \
             when something else releases the pin."
        )
    }
}

fn judge(
    board: &ExtractedBoard,
    comp: &Component,
    pin: &Pin,
    model: Option<&ModelEntry>,
    models: &HashMap<&str, ModelEntry>,
) -> Option<LintFinding> {
    let control = control_of(pin, model)?;
    let net_id = pin.net.filter(|id| *id != 0)?;
    let net = board.net(net_id)?;
    // A rail, ground or a placeholder is not a control net.
    if is_ground(&net.name)
        || netclass::is_supply(&net.name)
        || hauksbee_extract::is_unconnected_net(&net.name)
    {
        return None;
    }
    // The off level sits on ground for a pin that runs high and on the part's
    // own supply for one that runs low. A low-running pin with no supply net
    // on the board has no off rail to read, so it is not judged.
    let off_rail = if control.run_high {
        0
    } else {
        supply_net(board, comp, model)?.id
    };
    let reg = Reg {
        comp,
        on_high: control.run_high,
        input: off_rail,
        gnd: None,
    };
    let Survey {
        held,
        pulls,
        external,
        unfitted,
    } = survey_net(board, &reg, (comp, &pin.number), (net_id, models))?;
    // Without a pull to the off level this is a floating or externally
    // driven pin, which other checks own.
    if pulls.is_empty() {
        return None;
    }
    let rpd = parallel(&pulls)?;
    let claim = level_claim(
        &internal_pull(
            control.kind,
            control.role.as_deref(),
            model,
            control.run_high,
        ),
        rpd,
        control.run_high,
    )?;
    let unfitted = unfitted_note(&unfitted);
    let supply = supply_name(board, comp, model);
    let label = &control.label;
    let subject = format!(
        "{} ({}) {label} pin {} is on net '{}', which holds only {}{unfitted}",
        comp.reference,
        comp.value.trim(),
        pin.number,
        net.name,
        held.join(", "),
    );
    let fix = fix_sentence(control.run_high, &supply, label);
    let (severity, message) = if external.is_empty() {
        (
            Severity::High,
            format!(
                "{subject}, and nothing on it can take the pin to its {}: {claim}, so {}. \
                 {REMEDY_MARKER}{fix}",
                control.kind.on_level(),
                control.kind.consequence(),
            ),
        )
    } else {
        // A default-off enable brought out to a connector with a local
        // pull-down is a common deliberate design (the host enables the
        // part), so it is a non-gating note. A reset pulled into reset with
        // only a connector to release it gates: a debug or programming
        // header's reset line only ever pulls low, and a reset is normally
        // pulled to its released level.
        let severity = match control.kind {
            Kind::Enable => Severity::Low,
            Kind::Reset => Severity::Medium,
        };
        (
            severity,
            format!(
                "{subject}; the only thing on it that can take the pin to its {} is {}, an \
                 off-board source: {claim}, so unless that source always releases the pin, {}. \
                 {REMEDY_MARKER}If {} is meant to be released from outside, confirm it always \
                 is; otherwise {}",
                control.kind.on_level(),
                external.join(", "),
                control.kind.consequence(),
                label,
                decapitalise(&fix),
            ),
        )
    };
    Some(LintFinding {
        check: LintCheck::ControlHeldOff,
        severity,
        message,
        refs: vec![comp.reference.clone()],
        nets: vec![net.name.clone()],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::test_support::{board, part};
    use crate::test_support::pin_fn;

    const GND: i64 = 1;
    const V3: i64 = 2;
    const RST: i64 = 3;
    const GPIO: i64 = 4;

    const NETS: [(i64, &str); 4] = [(GND, "GND"), (V3, "+3V3"), (RST, "/RST"), (GPIO, "/IO")];

    fn lint(parts: Vec<Component>) -> Vec<LintFinding> {
        control_held_off_lint(&board(&NETS, parts), &ModelLibrary::builtin())
            .of_check(LintCheck::ControlHeldOff)
            .cloned()
            .collect()
    }

    /// A card-less IC whose pad 1 is a named control pin on `net`.
    fn ic(function: &str, net: i64) -> Component {
        let mut u = part("U1", "SomeIC", &[("1", net), ("2", V3), ("3", GND)]);
        u.pins[0] = pin_fn("1", net, function);
        u.pins[0].kind = "input".into();
        u
    }

    fn pull_down(reference: &str, value: &str) -> Component {
        part(reference, value, &[("1", RST), ("2", GND)])
    }

    fn says(f: &LintFinding, all: &[&str]) {
        for s in all {
            assert!(f.message.contains(s), "{s:?} in {}", f.message);
        }
    }

    /// The fault: a stiff pull-down is all an active-low reset has.
    #[test]
    fn a_reset_held_low_by_its_only_pull_is_a_high_finding() {
        let found = lint(vec![ic("~{RESET}", RST), pull_down("R1", "1k")]);
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.refs, ["U1"]);
        assert_eq!(f.nets, ["/RST"]);
        says(
            f,
            &[
                "U1 (SomeIC) ~{RESET} pin 1 is on net '/RST', which holds only R1 to 'GND'",
                "the part is held in reset and never runs",
                "Return the pull resistor to '+3V3'",
            ],
        );
        assert!(f.own_remedy().is_some(), "{}", f.message);
    }

    /// Espressif's chip enable has one sense, so it is judged by name; a
    /// trailing pad suffix (`CHIP_PU_3`, a model-less module symbol) does not
    /// hide it.
    #[test]
    fn an_active_high_enable_held_low_by_its_only_pull_is_a_finding() {
        let found = lint(vec![ic("CHIP_PU", RST), pull_down("R1", "1k")]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
        says(
            &found[0],
            &["CHIP_PU pin 1", "the part is never enabled", "enable level"],
        );
        let found = lint(vec![ic("CHIP_PU_3", RST), pull_down("R1", "1k")]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["CHIP_PU_3 pin 1", "the part is never enabled"]);
    }

    /// A carded active-high load switch (AP2281: EN is `ctrl`, the card states
    /// no open-pin behaviour) is an enable like a regulator's: a pull-down is
    /// all it has, so it never turns on; a pull-up as well releases it.
    #[test]
    fn a_load_switch_enable_held_low_by_its_only_pull_is_a_finding() {
        let switch = || {
            part(
                "U1",
                "AP2281-1WG-7",
                &[
                    ("1", GPIO),
                    ("2", GND),
                    ("3", RST),
                    ("4", V3),
                    ("5", GND),
                    ("6", V3),
                ],
            )
        };
        let found = lint(vec![switch(), pull_down("R1", "1k")]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
        assert_eq!(found[0].nets, ["/RST"]);
        says(&found[0], &["U1", "pin 3", "never enabled"]);
        let mut pull_up = pull_down("R2", "10k");
        pull_up.pins[1].net = Some(V3);
        assert!(lint(vec![switch(), pull_down("R1", "1k"), pull_up]).is_empty());
        assert!(lint(vec![switch()]).is_empty());
    }

    /// Without a card a bare `EN`, `CE`, `CEN` or `ENABLE` has no known
    /// polarity: a memory's or ADC's chip enable is active-low and a 10k
    /// pull-down that keeps it selected is normal wiring, not a fault.
    #[test]
    fn a_card_less_chip_enable_is_not_judged_by_its_name() {
        for name in ["CE", "CEN", "EN", "ENABLE", "EN_2"] {
            for value in ["10k", "1k"] {
                assert!(
                    lint(vec![ic(name, RST), pull_down("R1", value)]).is_empty(),
                    "{name} pulled down by {value}"
                );
            }
        }
    }

    /// The legal controls: every way the board can release the pin.
    #[test]
    fn a_pin_the_board_can_release_is_silent() {
        let mut pull_up = pull_down("R2", "10k");
        pull_up.pins[1].net = Some(V3);
        assert!(
            lint(vec![ic("~{RESET}", RST), pull_down("R1", "1k"), pull_up]).is_empty(),
            "a pull-up as well"
        );
        let series = part("R3", "1k", &[("1", GPIO), ("2", RST)]);
        let mcu = part("U2", "SomeMCU", &[("7", GPIO)]);
        assert!(
            lint(vec![
                ic("~{RESET}", RST),
                pull_down("R1", "1k"),
                series,
                mcu
            ])
            .is_empty(),
            "a GPIO through a resistor"
        );
        let driver = part("U3", "Supervisor", &[("4", RST)]);
        assert!(
            lint(vec![ic("CHIP_PU", RST), pull_down("R1", "1k"), driver]).is_empty(),
            "a supervisor or driver pin on the net"
        );
        let switch = part("SW1", "SPDT", &[("1", V3), ("2", RST), ("3", GND)]);
        assert!(
            lint(vec![ic("CHIP_PU", RST), pull_down("R1", "1k"), switch]).is_empty(),
            "a switch that reaches a rail"
        );
        // A 2-pad jumper from the pin to the supply closes onto the rail when
        // fitted ("default off, fit the jumper to enable").
        let jumper = part("JP1", "SolderJumper_2_Open", &[("1", RST), ("2", V3)]);
        assert!(
            lint(vec![
                ic("CHIP_PU", RST),
                pull_down("R1", "1k"),
                jumper.clone()
            ])
            .is_empty(),
            "a jumper to the supply"
        );
        assert!(
            lint(vec![ic("~{RESET}", RST), pull_down("R1", "1k"), jumper]).is_empty(),
            "a jumper to the supply on a reset"
        );
    }

    /// The part's own GPIO or output on the same net may release the pin; a
    /// pad of its own that drives nothing does not.
    #[test]
    fn the_parts_own_output_on_the_net_releases_it() {
        let own = |kind: &str| {
            let mut u = ic("~{RESET}", RST);
            u.pins.push(pin_fn("4", RST, "PA3"));
            u.pins[3].kind = kind.into();
            u
        };
        assert!(
            lint(vec![own("output"), pull_down("R1", "1k")]).is_empty(),
            "its own output"
        );
        assert!(
            lint(vec![own("bidirectional"), pull_down("R1", "1k")]).is_empty(),
            "its own GPIO"
        );
        let found = lint(vec![own("passive"), pull_down("R1", "1k")]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["U1's own pad 4"]);
    }

    /// A second footprint of the same micro shares the reset net: its reset
    /// pin (a port name with the reset as an alternate function) takes the
    /// level and sets none. A bare `RESET` of unstated type may be a
    /// supervisor's output and keeps the board silent.
    #[test]
    fn another_parts_reset_input_does_not_release_it() {
        let mut alt = part("U2", "SomeMCU", &[("29", RST), ("2", V3), ("3", GND)]);
        alt.pins[0] = pin_fn("29", RST, "PC6(/RESET)");
        alt.pins[0].kind = "bidirectional".into();
        let found = lint(vec![
            ic("~{RESET}", RST),
            pull_down("R1", "1k"),
            alt.clone(),
        ]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["U2 pin 29 (a reset input)"]);
        let mut supervisor = alt;
        supervisor.pins[0] = pin_fn("29", RST, "RESET");
        supervisor.pins[0].kind = "bidirectional".into();
        assert!(
            lint(vec![ic("~{RESET}", RST), pull_down("R1", "1k"), supervisor]).is_empty(),
            "a bare RESET pin of unstated direction may drive the net"
        );
    }

    /// A reset button whose reference reads as a resistor (`RESET`) and whose
    /// value is not a resistance only takes the pin to ground; the real
    /// pull-down still sets the level. Judged the same with or without a
    /// switch footprint.
    #[test]
    fn a_button_referenced_like_a_resistor_does_not_release_it() {
        let mcu = || {
            let mut u = part(
                "U1",
                "ATmega328P",
                &[("1", RST), ("7", V3), ("8", GND), ("20", V3), ("22", GND)],
            );
            u.pins[0].kind = "bidirectional".into();
            u
        };
        let button = part("RESET", "RESET", &[("1", RST), ("2", GND)]);
        let found = lint(vec![mcu(), pull_down("R1", "10k"), button.clone()]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
        says(
            &found[0],
            &[
                "R1 to 'GND'",
                "RESET (its other pads reach only the off level)",
            ],
        );
        let mut tact = button;
        tact.footprint = "Button_Switch_SMD:SW_SPST_EVQP7".into();
        let found = lint(vec![mcu(), pull_down("R1", "10k"), tact]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["RESET (it switches only to the off level)"]);
        // The legal control: the same board with its pull-up returned.
        let mut up = pull_down("R1", "10k");
        up.pins[1].net = Some(V3);
        let button = part("RESET", "RESET", &[("1", RST), ("2", GND)]);
        assert!(lint(vec![mcu(), up, button]).is_empty());
    }

    #[test]
    fn a_button_to_ground_and_a_capacitor_do_not_release_it() {
        let button = part("SW1", "SW_Push", &[("1", RST), ("2", GND)]);
        let cap = part("C1", "100n", &[("1", RST), ("2", GND)]);
        let found = lint(vec![
            ic("~{RESET}", RST),
            pull_down("R1", "1k"),
            button,
            cap,
        ]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["R1 to 'GND'", "SW1", "C1"]);
    }

    /// No pull at all is the floating-control check's, a pin tied to ground
    /// is deliberate, and an unfitted pull-down holds nothing.
    #[test]
    fn no_pull_a_tie_to_ground_and_an_unfitted_pull_are_not_this_check() {
        assert!(lint(vec![ic("~{RESET}", RST)]).is_empty(), "alone");
        let cap = part("C1", "100n", &[("1", RST), ("2", GND)]);
        assert!(lint(vec![ic("~{RESET}", RST), cap]).is_empty(), "cap only");
        assert!(lint(vec![ic("~{RESET}", GND)]).is_empty(), "tied to ground");
        let mut dnp = pull_down("R1", "1k");
        dnp.dnp = true;
        assert!(lint(vec![ic("~{RESET}", RST), dnp]).is_empty(), "DNP");
    }

    /// A connector pad may release the pin from outside, and only when the
    /// board pulls the pin off is it reported: a Medium finding on a reset,
    /// a non-gating note on an enable (a default-off external enable is a
    /// common design). A wide header stays an off-board source even with a
    /// supply pin on it.
    #[test]
    fn a_connector_pad_makes_it_a_medium_note_and_only_with_a_pull() {
        let header = part("J1", "Conn_01x02", &[("1", RST), ("2", GND)]);
        let found = lint(vec![
            ic("~{RESET}", RST),
            pull_down("R1", "1k"),
            header.clone(),
        ]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::Medium);
        says(&found[0], &["J1 pin 1", "off-board source"]);
        assert!(lint(vec![ic("~{RESET}", RST), header.clone()]).is_empty());
        let found = lint(vec![ic("CHIP_PU", RST), pull_down("R1", "1k"), header]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::Low);
        let wide = part(
            "J2",
            "Conn_01x06",
            &[
                ("1", V3),
                ("2", RST),
                ("3", GPIO),
                ("4", GND),
                ("5", GND),
                ("6", GND),
            ],
        );
        let found = lint(vec![ic("~{RESET}", RST), pull_down("R1", "1k"), wide]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::Medium);
        says(&found[0], &["J2 pin 2"]);
    }

    /// Cards give `rst_n` to open-drain reset outputs too (TPL5010 pad 6,
    /// "open-drain active-low reset output"): a pull-down on one is not a
    /// held reset input. Nor is any pin the symbol types as an output.
    #[test]
    fn a_reset_output_is_not_a_held_reset_input() {
        let timer = |kind: &str| {
            let mut u = part(
                "U1",
                "TPL5010DDC",
                &[
                    ("1", V3),
                    ("2", GND),
                    ("3", GPIO),
                    ("4", GPIO),
                    ("5", GPIO),
                    ("6", RST),
                ],
            );
            u.pins[5].kind = kind.into();
            u
        };
        for kind in ["", "passive", "open_collector", "output"] {
            assert!(
                lint(vec![timer(kind), pull_down("R1", "1k")]).is_empty(),
                "rst_n typed {kind:?}"
            );
        }
        for kind in ["open_collector", "output", "tri_state"] {
            let mut u = ic("~{RESET}", RST);
            u.pins[0].kind = kind.into();
            assert!(
                lint(vec![u, pull_down("R1", "1k")]).is_empty(),
                "card-less ~{{RESET}} typed {kind}"
            );
        }
    }

    /// Polarity is never guessed.
    #[test]
    fn an_active_low_enable_and_an_undecorated_reset_are_not_judged() {
        assert!(lint(vec![ic("~{EN}", RST), pull_down("R1", "1k")]).is_empty());
        assert!(lint(vec![ic("SHDN", RST), pull_down("R1", "1k")]).is_empty());
        assert!(lint(vec![ic("RESET", RST), pull_down("R1", "1k")]).is_empty());
        assert!(lint(vec![ic("D3", RST), pull_down("R1", "1k")]).is_empty());
    }

    /// An internal pull-up can win against a weak pull-down: the divider
    /// decides, from the card's printed minimum.
    #[test]
    fn the_level_against_the_internal_pull_up_decides() {
        // ATmega328P (DIP-28): pad 1 is PC6/RESET, RRST 30 kOhm minimum.
        let mcu = || {
            let mut u = part(
                "U1",
                "ATmega328P",
                &[("1", RST), ("7", V3), ("8", GND), ("20", V3), ("22", GND)],
            );
            u.pins[0].kind = "bidirectional".into();
            u
        };
        // 10k against 30k leaves at most 0.25 of the supply: never a valid high.
        let found = lint(vec![mcu(), pull_down("R1", "10k")]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(
            &found[0],
            &[
                "no more than 0.25 of the supply",
                "internal pull-up minimum of 30 kΩ",
            ],
        );
        // 100k against 30k leaves 0.77 of the supply: the part releases itself.
        assert!(lint(vec![mcu(), pull_down("R1", "100k")]).is_empty());
        // A card-less part is judged only against a stiff pull-down.
        assert!(lint(vec![ic("~{RESET}", RST), pull_down("R1", "10k")]).is_empty());
    }

    /// A reset input the datasheet gives no pull (the ADS122C04's) is held
    /// by any pull-down.
    #[test]
    fn a_pin_with_no_internal_pull_is_held_by_any_pull_down() {
        let adc = part("U1", "ADS122C04IPW", &[("3", RST), ("13", V3), ("4", GND)]);
        let found = lint(vec![adc, pull_down("R1", "100k")]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["no pull of its own", "R1 to 'GND'"]);
    }

    /// A test card for a linear regulator (1 IN, 2 GND, 3 EN, 5 OUT) carrying
    /// `enable` params, on +5V to +3V3 with EN on `/RST` and `extra` parts.
    /// Returns (this check's findings, `regulator_enable`'s).
    fn ldo(tag: &str, enable: &str, extra: Vec<Component>) -> (Vec<LintFinding>, Vec<LintFinding>) {
        const VIN: i64 = 5;
        let card = format!(
            "[[models]]\nid = \"held_off_ldo_{tag}\"\nkind = \"vreg\"\n\
             description = \"enable fixture\"\n[models.match]\n\
             value_re = \"^HOLDTEST-{tag}$\"\n[models.params]\nvout = 3.3\n\
             dropout_v = 0.3\niq_a = 5.0e-5\n{enable}\n[models.pins]\n\
             \"1\" = \"in\"\n\"2\" = \"gnd\"\n\"3\" = \"en\"\n\"5\" = \"out\"\n"
        );
        let dir = std::env::temp_dir().join(format!(
            "hauksbee_control_held_off_{tag}_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("models.toml"), card).unwrap();
        let mut lib = ModelLibrary::builtin();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "{tag}: {errors:?}");
        let reg = part(
            "U1",
            &format!("HOLDTEST-{tag}"),
            &[("1", VIN), ("2", GND), ("3", RST), ("5", V3)],
        );
        let mut parts = vec![reg];
        parts.extend(extra);
        let nets = [
            (GND, "GND"),
            (V3, "+3V3"),
            (RST, "/LDO_EN"),
            (GPIO, "/IO"),
            (VIN, "+5V"),
        ];
        let b = board(&nets, parts);
        let mine = control_held_off_lint(&b, &lib)
            .of_check(LintCheck::ControlHeldOff)
            .cloned()
            .collect();
        let theirs = crate::checks::regulator_enable::regulator_enable_lint(&b, &lib).findings;
        (mine, theirs)
    }

    /// A regulator whose card does not say an open enable is off is not
    /// `regulator_enable`'s, so a pull-down that holds its enable low is
    /// judged here: one pulled on inside the part (an internal pull-up a 1k
    /// pull-down overrides), and one whose card is silent on the open state.
    /// An off-when-open card's enable is `regulator_enable`'s alone.
    #[test]
    fn a_regulator_enable_regulator_enable_does_not_judge_is_judged_here() {
        let (mine, theirs) = ldo(
            "PULLUP",
            "en_pull_up_ohms = 1.0e6",
            vec![pull_down("R1", "1k")],
        );
        assert!(theirs.is_empty(), "{theirs:#?}");
        assert_eq!(mine.len(), 1, "{mine:#?}");
        assert_eq!(mine[0].severity, Severity::High);
        says(
            &mine[0],
            &[
                "U1 (HOLDTEST-PULLUP) EN pin 3",
                "R1 to 'GND'",
                "internal pull-up minimum of 1 MΩ",
            ],
        );
        let (mine, theirs) = ldo("SILENT", "", vec![pull_down("R1", "1k")]);
        assert!(theirs.is_empty(), "{theirs:#?}");
        assert_eq!(mine.len(), 1, "{mine:#?}");
        says(&mine[0], &["an assumed internal pull-up floor of 10 kΩ"]);
        // Legal control: the same regulator pulled up to its input.
        let mut up = pull_down("R1", "100k");
        up.pins[1].net = Some(5);
        let (mine, theirs) = ldo("SILENTUP", "", vec![up]);
        assert!(
            mine.is_empty() && theirs.is_empty(),
            "{mine:#?} {theirs:#?}"
        );
        // An off-when-open card: one finding, from regulator_enable.
        let (mine, theirs) = ldo("OFFOPEN", "en_open = \"off\"", vec![pull_down("R1", "1k")]);
        assert!(mine.is_empty(), "{mine:#?}");
        assert_eq!(theirs.len(), 1, "{theirs:#?}");
    }

    /// A digital part (1 vcc, 2 gnd, 3 the control pin) whose card states the
    /// control pin's polarity in a param line (`param`) or a table (`table`),
    /// on `/RST` with `parts`.
    fn polar(
        tag: &str,
        role: &str,
        (param, table): (&str, &str),
        parts: Vec<Component>,
    ) -> Vec<LintFinding> {
        let card = format!(
            "[[models]]\nid = \"held_off_polar_{tag}\"\nkind = \"digital\"\n\
             description = \"polarity fixture\"\n[models.match]\n\
             value_re = \"^POLAR-{tag}$\"\n[models.params]\nidentity_only = true\n\
             warning = \"identity only\"\nunlocked_by = \"validated behavior\"\n{param}\n\
             [models.pins]\n\"1\" = \"vcc\"\n\"2\" = \"gnd\"\n\"3\" = \"{role}\"\n{table}\n"
        );
        let dir = hauksbee_testkit::fresh_scratch_dir("control_held_off", tag);
        std::fs::write(dir.join("models.toml"), card).unwrap();
        let mut lib = ModelLibrary::builtin();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "{tag}: {errors:?}");
        let mut all = vec![part(
            "U1",
            &format!("POLAR-{tag}"),
            &[("1", V3), ("2", GND), ("3", RST)],
        )];
        all.extend(parts);
        control_held_off_lint(&board(&NETS, all), &lib)
            .of_check(LintCheck::ControlHeldOff)
            .cloned()
            .collect()
    }

    fn pull_up(reference: &str, value: &str) -> Component {
        part(reference, value, &[("1", RST), ("2", V3)])
    }

    /// An enable the card runs on a low level, pulled to the supply with
    /// nothing able to take it low, is held off for good: the wrong rail.
    #[test]
    fn an_active_low_enable_pulled_to_the_supply_is_held_off_and_pulled_to_ground_is_not() {
        for (tag, role, card) in [
            ("ENN", "en_n", ("", "")),
            ("ENLOW", "en", ("en_on_level = \"low\"", "")),
        ] {
            let found = polar(tag, role, card, vec![pull_up("R1", "1k")]);
            assert_eq!(found.len(), 1, "{tag}: {found:#?}");
            assert_eq!(found[0].severity, Severity::High, "{tag}");
            says(
                &found[0],
                &[
                    "pin 3 is on net '/RST'",
                    "R1 to '+3V3'",
                    "the part is never enabled",
                    "above any valid low",
                    "Return the pull resistor to ground instead of '+3V3'",
                ],
            );
            // Fixed: the same pin pulled to ground enables the part.
            assert!(
                polar(tag, role, card, vec![pull_down("R1", "10k")]).is_empty(),
                "{tag}"
            );
            // Something that can take the pin low releases it: a GPIO, a button
            // to ground.
            let gpio = part("U2", "ATmega328P", &[("5", RST), ("8", GND)]);
            assert!(polar(tag, role, card, vec![pull_up("R1", "1k"), gpio]).is_empty());
            let button = part("SW1", "SW_Push", &[("1", RST), ("2", GND)]);
            assert!(polar(tag, role, card, vec![pull_up("R1", "1k"), button]).is_empty());
        }
    }

    /// A reset the card holds while the pin reads high is held in reset by a
    /// pull-up nothing releases.
    #[test]
    fn an_active_high_reset_pulled_to_the_supply_is_held_in_reset_and_pulled_to_ground_is_not() {
        let card = (
            "",
            "[models.reset_input]\nrole = \"rst\"\nactive_high = true\n",
        );
        let found = polar("RSTHI", "rst", card, vec![pull_up("R1", "1k")]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
        says(
            &found[0],
            &[
                "R1 to '+3V3'",
                "the part is held in reset and never runs",
                "Return the pull resistor to ground instead of '+3V3'",
            ],
        );
        assert!(polar("RSTHI", "rst", card, vec![pull_down("R1", "10k")]).is_empty());
        // Another supply is not the part's off rail: a pull there is not judged.
        let mut other = pull_up("R1", "1k");
        other.pins[1].net = Some(GPIO);
        assert!(polar("RSTHI", "rst", card, vec![other]).is_empty());
        // A card that says nothing about the polarity leaves the pin unjudged.
        assert!(polar("RSTNONE", "rst", ("", ""), vec![pull_up("R1", "1k")]).is_empty());
        assert!(polar("RSTNONE", "rst", ("", ""), vec![pull_down("R1", "10k")]).is_empty());
    }

    #[test]
    fn a_regulator_and_a_rail_are_not_this_checks() {
        // A rail-named net with a bleeder to ground is not a control net.
        let mut u = ic("CHIP_PU", V3);
        u.pins[0].net = Some(V3);
        let bleeder = part("R1", "1k", &[("1", V3), ("2", GND)]);
        assert!(lint(vec![u, bleeder]).is_empty());
    }

    /// A part (kind `kind`, pad 3 the enable `role`, params `params`) whose
    /// enable net `/RST` has only the pads of an MCU (U2: pad 2 unlisted, pad 3
    /// pulled up in reset) and `extra` parts on it. Returns this check's
    /// findings.
    fn gpio_enable(
        tag: &str,
        (kind, role, params): (&str, &str, &str),
        mcu_pad: &str,
        extra: Vec<Component>,
    ) -> Vec<LintFinding> {
        // A load switch names its input and output; the rest only a supply and ground.
        let pins = if kind == "analog_switch" {
            "\"1\" = \"in_out_a\"\n\"2\" = \"vss\"\n\"4\" = \"in_out_b\"\n"
        } else {
            "\"1\" = \"vcc\"\n\"2\" = \"gnd\"\n"
        };
        let card = format!(
            "[[models]]\nid = \"gpio_en_{tag}\"\nkind = \"{kind}\"\n\
             description = \"enable fixture\"\n[models.match]\n\
             value_re = \"^GENTEST-{tag}$\"\n[models.params]\n{params}\n\
             [models.pins]\n{pins}\"3\" = \"{role}\"\n\
             [[models]]\nid = \"gpio_mcu_{tag}\"\nkind = \"mcu\"\n\
             description = \"reset mode fixture\"\n[models.match]\n\
             value_re = \"^GMCU-{tag}$\"\n[models.pins]\n\"1\" = \"vdd\"\n\
             \"2\" = \"gpio_a\"\n\"3\" = \"gpio_b\"\n\
             [models.pin_capability.reset_mode]\nbasis = \"fixture\"\n\
             [models.pin_capability.reset_mode.pads]\n\"3\" = \"pull_up\"\n"
        );
        let dir = hauksbee_testkit::fresh_scratch_dir("control_held_off", tag);
        std::fs::write(dir.join("models.toml"), card).unwrap();
        let mut lib = ModelLibrary::builtin();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "{tag}: {errors:?}");
        let mut all = vec![
            part(
                "U1",
                &format!("GENTEST-{tag}"),
                &[("1", V3), ("2", GND), ("3", RST)],
            ),
            part("U2", &format!("GMCU-{tag}"), &[("1", V3), (mcu_pad, RST)]),
        ];
        all.extend(extra);
        control_held_off_lint(&board(&NETS, all), &lib)
            .of_check(LintCheck::ControlHeldOff)
            .cloned()
            .collect()
    }

    /// The card shape of a real load switch (active-high `ctrl`, input and
    /// output pins) with `open` as what the datasheet says an open pin does.
    fn load_switch(open: &str) -> String {
        format!("ron = 0.04\nroff = 5.0e7\nvth = 1.2\nen_on_level = \"high\"\n{open}")
    }

    const OPEN_IS_ON: &str = "en_open = \"on\"\nen_open_basis = \"fixture p.1: open = on\"";

    /// A load-switch enable only an MCU pad drives, with the card saying an open
    /// pin turns the part on: while the MCU is in reset the pad is
    /// high-impedance, the pin floats and the switch can be on.
    #[test]
    fn an_enable_only_a_gpio_drives_floats_in_reset_and_the_card_says_what_that_does() {
        let params = load_switch(OPEN_IS_ON);
        let found = gpio_enable("SWON", ("analog_switch", "ctrl", &params), "2", vec![]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::Medium);
        says(
            &found[0],
            &[
                "U1 (GENTEST-SWON) EN pin 3 is driven only by U2 pin 2",
                "high-impedance",
                "floats through power-up and reset",
                "fixture p.1: open = on",
                "switched on while the MCU is in reset",
                "100k pull-down from '/RST' to ground",
            ],
        );
        assert!(found[0].own_remedy().is_some(), "{}", found[0].message);
    }

    /// The silent controls: a pull to the off level, an open state the card says
    /// is off, an internal pull toward off, and an MCU pad the card pulls in
    /// reset.
    #[test]
    fn a_gpio_enable_with_a_pull_or_an_off_when_open_card_is_silent() {
        let on = load_switch(OPEN_IS_ON);
        let sw = |tag: &str, params: &str, pad: &str, extra: Vec<Component>| {
            gpio_enable(tag, ("analog_switch", "ctrl", params), pad, extra)
        };
        let pull = part("R1", "100k", &[("1", RST), ("2", GND)]);
        assert!(sw("SWPULL", &on, "2", vec![pull]).is_empty());
        let off = load_switch("en_open = \"off\"\nen_open_basis = \"fixture p.1: open = off\"");
        assert!(sw("SWOFF", &off, "2", vec![]).is_empty());
        let internal = load_switch(
            "en_pull_down_ohms = 1.0e6\nen_open_basis = \"fixture p.1: internal pull-down\"",
        );
        assert!(sw("SWINT", &internal, "2", vec![]).is_empty());
        // A pull-up on an active-high pin is the wrong way round, but the pin is
        // then defined (on) rather than floating: not this rule's.
        let up = part("R1", "100k", &[("1", RST), ("2", V3)]);
        assert!(sw("SWUP", &on, "2", vec![up]).is_empty());
        // Pad 3 of the MCU is pulled in reset per its card.
        assert!(sw("SWPAD", &on, "3", vec![]).is_empty());
    }

    /// A load switch whose datasheet says nothing at all about an open pin is
    /// judged too: it exists to be off until asked.
    #[test]
    fn a_load_switch_enable_with_no_stated_open_state_floats_in_reset() {
        let params = load_switch("");
        let found = gpio_enable("SWUNK", ("analog_switch", "ctrl", &params), "2", vec![]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["states nothing about an open pin"]);
    }

    /// A motor driver (a digital card) whose active-low enable states no pull:
    /// the same rule. The card's `enable_n` role gives the polarity.
    #[test]
    fn a_driver_enable_with_no_stated_pull_floats_in_reset() {
        let identity = "identity_only = true\nwarning = \"identity only\"\nunlocked_by = \"validated behavior\"";
        let driver = ("digital", "enable_n", identity);
        let found = gpio_enable("DRV", driver, "2", vec![]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(
            &found[0],
            &[
                "states nothing about an open pin",
                "may be on while the MCU is in reset",
            ],
        );
        let pull = part("R1", "100k", &[("1", RST), ("2", V3)]);
        assert!(gpio_enable("DRVPULL", driver, "2", vec![pull]).is_empty());
        // An active-high enable on a card that is silent is left alone (a rail
        // that follows its enable through a reset is a design choice).
        let high_silent = ("digital", "enable", identity);
        assert!(gpio_enable("DRVHI", high_silent, "2", vec![]).is_empty());
    }

    /// A load switch whose datasheet says an open enable is undefined (the
    /// AP22815 shape: `en_open = "undefined"` and `must_not_float_roles`).
    #[test]
    fn a_load_switch_whose_datasheet_forbids_a_floating_enable_floats_in_reset() {
        let params = load_switch(
            "en_open = \"undefined\"\nen_open_basis = \"fixture p.2: do not leave floating\"\nmust_not_float_roles = \"ctrl\"",
        );
        let found = gpio_enable("SWUNDEF", ("analog_switch", "ctrl", &params), "2", vec![]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(
            &found[0],
            &[
                "leaves an open pin undefined",
                "do not leave floating",
                "100k pull-down",
            ],
        );
    }
}
