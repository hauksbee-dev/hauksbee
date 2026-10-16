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
//! pin, so a net whose only way to the on level is a connector is a Medium
//! note instead of a High fault.
//!
//! Polarity is never guessed. An enable is active-high when its card role is
//! `en`/`enable` (and the card does not say `en_on_level = "low"`) or its pin
//! is named like one with no active-low decoration; a `SHDN` pin is not
//! judged without a card. A reset is active-low when the pin is on an MCU
//! card's `reset` role (every MCU card's reset role is the active-low reset
//! input), carries a `_n` card role, or is spelled active-low (`~{RESET}`,
//! `!RESET`, `RESET#`, `nRST`, `NRST`, `RESET_N`). Anything else, including an
//! active-low enable or an active-high reset, is not judged.
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
//! Linear regulators with an `en` pin are `regulator_enable`'s; the extract
//! layer's generic floating-control note covers a control pin alone on its
//! net.

use std::collections::HashMap;

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    control_role, is_connector_like, is_ground, Component, ExtractedBoard, LintCheck, LintFinding,
    NetLintReport, Pin, Severity, REMEDY_MARKER,
};
use hauksbee_models::netclass;
use hauksbee_models::refdes::RefClass;
use hauksbee_models::{ComponentKind, ModelEntry, ModelLibrary};

use super::envelope::role_net_id;
use super::indicators::name_suggests_active_low;
use super::regulator_enable::{decapitalise, ohms, survey_net, Reg, Survey};

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
    if kind_text.contains("output") || kind_text.contains("power") {
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
            let low = model.kind == ComponentKind::Mcu
                || r.ends_with("_n")
                || r.contains("nrst")
                || reset_spelled_active_low(function);
            return low.then(|| Control {
                kind: Kind::Reset,
                label: label("RESET"),
                role: role.clone(),
            });
        }
        if enable_role(r) {
            let high = model.params.get_str("en_on_level") != Some("low")
                && !name_suggests_active_low(function);
            return high.then(|| Control {
                kind: Kind::Enable,
                label: label("EN"),
                role: role.clone(),
            });
        }
        // The card maps this pad to something else; a symbol's name for it
        // is not evidence of a control.
        return None;
    }
    let raw = function.to_ascii_uppercase();
    match control_role(bare(function))? {
        "reset" => reset_spelled_active_low(function).then(|| Control {
            kind: Kind::Reset,
            label: label("RESET"),
            role: None,
        }),
        "enable" => {
            let active_low = name_suggests_active_low(function)
                || raw.starts_with('N')
                || raw.contains("SHDN")
                || raw.contains("SHUTDOWN");
            (!active_low).then(|| Control {
                kind: Kind::Enable,
                label: label("EN"),
                role: None,
            })
        }
        _ => None,
    }
}

fn internal_pull(kind: Kind, role: Option<&str>, model: Option<&ModelEntry>) -> InternalPull {
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
    let key = match kind {
        Kind::Enable => "en_pull_up_ohms",
        Kind::Reset => "reset_pull_up_ohms",
    };
    match model.params.get_f64(key) {
        Some(r) if r.is_finite() && r > 0.0 => InternalPull::Known(r),
        _ => InternalPull::Unknown,
    }
}

/// The parallel resistance of the pulls, `None` when one does not read.
fn parallel(pulls: &[Option<f64>]) -> Option<f64> {
    let mut conductance = 0.0;
    for p in pulls {
        let r = (*p)?;
        if r <= 0.0 {
            return Some(0.0);
        }
        conductance += 1.0 / r;
    }
    (conductance > 0.0).then(|| 1.0 / conductance)
}

/// The finding's words on how far the pin can rise, or `None` when the
/// internal pull could lift it to a valid high.
fn level_claim(pull: &InternalPull, rpd: f64) -> Option<String> {
    let (rint, source) = match pull {
        InternalPull::Absent => return Some(
            "the part has no pull of its own on this pin, so the pull-down alone sets its level"
                .to_string(),
        ),
        InternalPull::Known(r) => (*r, "the card's internal pull-up minimum"),
        InternalPull::Unknown => (
            ASSUMED_INTERNAL_PULL_OHMS,
            "an assumed internal pull-up floor",
        ),
    };
    let fraction = rpd / (rpd + rint);
    (fraction <= NEVER_HIGH_FRACTION).then(|| {
        format!(
            "even against {source} of {} the pin sits at no more than {:.2} of the supply, below \
             any valid high",
            ohms(rint),
            fraction
        )
    })
}

/// The supply net the part's logic runs from: by its card's pin roles, else
/// the first positive supply rail on one of its pads.
fn supply_name(board: &ExtractedBoard, comp: &Component, model: Option<&ModelEntry>) -> String {
    let by_card = model.and_then(|m| {
        ["vdd", "vcc", "dvdd", "vddio", "vdd_io", "3v3", "vcc_io"]
            .iter()
            .find_map(|role| role_net_id(comp, m, role))
    });
    by_card
        .and_then(|id| board.net(id))
        .or_else(|| {
            comp.pins
                .iter()
                .filter_map(|p| p.net.and_then(|id| board.net(id)))
                .find(|n| netclass::is_positive_supply(&n.name))
        })
        .map_or_else(
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
        // A simple linear regulator's enable is `regulator_enable`'s subject.
        if model.is_some_and(|m| m.kind == ComponentKind::Vreg && m.behavioral.converter.is_none())
        {
            continue;
        }
        for pin in &comp.pins {
            if let Some(f) = judge(board, comp, pin, model, &models) {
                report.findings.push(f);
            }
        }
    }
    report
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
    let reg = Reg {
        comp,
        on_high: true,
        input: 0,
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
        &internal_pull(control.kind, control.role.as_deref(), model),
        rpd,
    )?;
    let unfitted = match unfitted.as_slice() {
        [] => String::new(),
        [one] => format!(" ({one} on the net is not fitted)"),
        many => format!(" ({} on the net are not fitted)", many.join(", ")),
    };
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
    let fix = format!(
        "Return the pull resistor to {supply} instead of ground (a pull-up), or drive {label} \
         from a GPIO or a supervisor; a pull to ground belongs on {label} only when something \
         else releases the pin."
    );
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
        (
            Severity::Medium,
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

    #[test]
    fn an_active_high_enable_held_low_by_its_only_pull_is_a_finding() {
        let found = lint(vec![ic("EN", RST), pull_down("R1", "1k")]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(
            &found[0],
            &["EN pin 1", "the part is never enabled", "enable level"],
        );
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
            lint(vec![ic("EN", RST), pull_down("R1", "1k"), driver]).is_empty(),
            "a supervisor or driver pin on the net"
        );
        let switch = part("SW1", "SPDT", &[("1", V3), ("2", RST), ("3", GND)]);
        assert!(
            lint(vec![ic("EN", RST), pull_down("R1", "1k"), switch]).is_empty(),
            "a switch that reaches a rail"
        );
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

    /// A connector pad may release the pin from outside: a Medium note, and
    /// only when the board pulls the pin off.
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
        assert!(lint(vec![ic("~{RESET}", RST), header]).is_empty());
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

    #[test]
    fn a_regulator_and_a_rail_are_not_this_checks() {
        // A rail-named net with a bleeder to ground is not a control net.
        let mut u = ic("EN", V3);
        u.pins[0].net = Some(V3);
        let bleeder = part("R1", "1k", &[("1", V3), ("2", GND)]);
        assert!(lint(vec![u, bleeder]).is_empty());
    }
}
