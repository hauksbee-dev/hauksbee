//! Boot strapping-pin lint (static, no firmware needed).
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! An MCU samples certain pins at the reset latch window to choose its boot
//! mode. If a strapping pin is not at the level the part needs during that
//! window, the chip can boot wrong (the textbook case: Olimex ESP32-EVB rev D,
//! where the Ethernet PHY's free-running 50 MHz `REF_CLK` sits on GPIO0 - a
//! strapping pin that must be a stable HIGH - so the ESP32 randomly enters
//! download mode; ESP-IDF Ethernet API documents this exact failure, and Olimex
//! fixed it in rev E by gating PHY power until the oscillator stabilises).
//!
//! This check reads the per-part strap table from the model db
//! (`[[models.straps]]`, see `crates/hauksbee-models/db/mcu.toml`) and, for each
//! bound MCU, examines the net each strap pin sits on. It fires only on the
//! structurally unambiguous case, in line with the calibration discipline the
//! rest of hauksbee follows (zero false positives on known-good boards, or the
//! check does not ship):
//!
//!   (a) a *free-running clock source* (a powered oscillator) reaches the strap
//!       net with no strong static pull to override it -> HIGH severity, because
//!       a clock present at the reset latch is the documented failure mode and a
//!       clock is, by definition, free-running at reset;
//!   (b) the strap net is *resistively biased to the wrong level* for normal
//!       boot (a pull-up where the part needs LOW, or a pull-down where it needs
//!       HIGH) -> MEDIUM. Pulls both ways form a divider, solved from the
//!       pulls' conductances at every rail and tolerance corner against the
//!       card's sourced VIL/VIH: consistently wrong -> MEDIUM, mid-band or
//!       corner-dependent -> HIGH, no thresholds or values -> a LOW abstention;
//!   (c) (deliberately NOT fired on "no external bias" while the silicon pull
//!       settles the pin at the level the part needs: every ESP32 strap has a
//!       documented internal pull, so a bare strap pin held by the internal pull
//!       is correct - firing there would be a confident false positive on Watchy
//!       and others. See the calibration note in the docs. The pull is the strap
//!       row's own or the card's boot table. It fires when that pull is the wrong
//!       level, when the card says there is none, and when a jumper or header
//!       the user can leave open is the only thing holding a boot-select pin: the
//!       open position reads the pull, or floats.)
//!
//! DNP-aware: a Do-Not-Populate resistor is not assembled, so it neither biases
//! nor overrides anything. On a PCB-only extraction (no schematic pin types) the
//! clock-driver detection still works - it keys on the *component* (an
//! oscillator), not a pin electrical type - but the reduced visibility is noted.

use super::{is_ground_name, is_unconnected_net};
use hauksbee_extract::{
    Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity,
};
use std::collections::{BTreeMap, BTreeSet};

use hauksbee_ir::boot::{BootDecision, StrapReading, StrapReadingLevel};
use hauksbee_models::boot_mode::{pull_level, select_mode, BootInput, PinLevel, Selection};
use hauksbee_models::refdes::{designator, RefClass};
use hauksbee_models::value::parse_value;
use hauksbee_models::{
    ComponentKind, ModelEntry, ModelLibrary, PeripheralSpec, StrapInternalPull, StrapLevel,
    StrapPin,
};

use super::envelope::{role_net_id, RailRange, RailResolver};
use super::floating_inputs::{control_lint_covers, Classifier, Hold, Weak, WeakKind};
use hauksbee_bind::binder::resolve;
use hauksbee_extract::assembly::AssemblyState;

/// Run the strap-pin lint over an extracted board, resolving each component
/// against `lib` to find MCUs that carry a strap table.
#[must_use]
pub fn strap_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    strap_lint_with_drives(board, lib, &BTreeMap::new())
}

/// [`strap_lint`] with solved (or spec-forced) net spans, which a pull
/// divider is judged at alongside each rail's declared corners.
#[must_use]
pub fn strap_lint_with_drives(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    solved: &BTreeMap<String, RailRange>,
) -> NetLintReport {
    let statics = super::BoardStatics::of(board, lib);
    let rails = RailResolver::new(board, &statics, solved);
    let models: BTreeMap<&str, ModelEntry> = super::resolved_components(board, lib)
        .map(|(c, m)| (c.reference.as_str(), m))
        .collect();
    let senses = BTreeSet::new();
    let classify = Classifier {
        board,
        models: &models,
        senses: &senses,
        strict: true,
        open_jumpers: true,
    };
    let mut report = NetLintReport::default();
    for comp in &board.components {
        // The three-state contract: only a present part's strap table runs.
        let Some(part) = AssemblyState::of(comp).fitted() else {
            continue;
        };
        let res = resolve(lib, part);
        let Some(model) = res.model.as_ref() else {
            continue;
        };
        if model.straps.is_empty() {
            continue;
        }
        for strap in &model.straps {
            let Ok((pad_num, net_id)) = strap_site(comp, model, strap) else {
                continue; // `strap_coverage` names the unplaced strap
            };
            let Some(net) = board.net(net_id) else {
                continue;
            };
            // A floating strap net. For a pin with a documented internal pull
            // (every ESP32 strap) an undriven net settles to a defined level, so
            // skip; the prior, conservative behaviour. But a pin with NO internal
            // pull (STM32 BOOT0) left floating is genuinely undefined at reset:
            // that is the fault, not a non-event. This is the boot-mode latch the
            // firmware cannot override. A net held only by parts that set a level
            // some of the time (a button to one rail, a header pin, an MCU pad
            // that is high impedance at reset) may float just the same.
            let hold = strap_hold(&classify, comp, &pad_num, &net.name);
            let pull = effective_pull(model, strap);
            let no_pull = pull == StrapInternalPull::None;
            // Where an internal pull settles a bare pin, only a net with nothing
            // else on it (the prior behaviour) is reported as left open.
            let floats = match &hold {
                Hold::Floats => no_pull || strap_net_is_bare(board, net_id, comp),
                Hold::MayFloat(_) => no_pull,
                Hold::Defined => false,
            };
            let site = StrapSite {
                comp,
                model,
                strap,
                pad: &pad_num,
                net: &net.name,
            };
            if floats {
                if no_pull {
                    // A net some MCU output drives is driven once firmware runs. At
                    // most it is a note about the reset window, never a fault.
                    if driven_by_mcu(&hold) {
                        report.findings.extend(driven_strap_note(
                            board,
                            (comp, model),
                            strap,
                            (&pad_num, &net.name),
                            &hold,
                        ));
                    } else if !strap_is_reported_elsewhere(
                        board, comp, model, strap, &hold, &pad_num,
                    ) {
                        report
                            .findings
                            .extend(floating_finding(board, &site, &hold));
                    }
                } else {
                    // A bare pin the silicon pull settles: silent when the pull
                    // is the level the part needs, a fault when it is not.
                    report
                        .findings
                        .extend(wrong_default_finding(board, &site, pull, None));
                }
                continue;
            }
            if let Hold::MayFloat(weak) = &hold {
                // Held only by a jumper or header the user can leave open, on a
                // pin the silicon pulls: the open position takes the pull's level.
                if weak
                    .iter()
                    .all(|w| matches!(w.kind, WeakKind::Jumper | WeakKind::Connector))
                    && strap.boot_select
                {
                    report
                        .findings
                        .extend(wrong_default_finding(board, &site, pull, Some(weak)));
                }
            }
            examine_strap(
                board,
                lib,
                &rails,
                (comp, model),
                strap,
                net_id,
                &mut report,
            );
        }
    }
    report
}

/// True when the only thing on the strap net is its own part: nothing could
/// define a level, with or without an internal pull to settle it.
fn strap_net_is_bare(board: &ExtractedBoard, net_id: i64, mcu: &Component) -> bool {
    board
        .net_members(net_id)
        .iter()
        .all(|(c, _)| c.reference == mcu.reference)
}

/// What holds the net of a strap's pad.
fn strap_hold(classify: &Classifier<'_>, comp: &Component, pad: &str, net_name: &str) -> Hold {
    if is_unconnected_net(net_name) {
        return Hold::Floats;
    }
    comp.pins
        .iter()
        .find(|p| p.number == pad)
        .map_or(Hold::Defined, |p| classify.hold(comp, p))
}

/// A non-boot strap row (reset, enable) the model-driven control checks
/// already judge: the extract lint's named control pin alone on its net, or a
/// card role the card says must not float whose net holds nothing but parts
/// that cannot hold a level. Only an MCU pad as the holder is new here.
fn strap_is_reported_elsewhere(
    board: &ExtractedBoard,
    comp: &Component,
    model: &ModelEntry,
    strap: &StrapPin,
    hold: &Hold,
    pad: &str,
) -> bool {
    if strap.boot_select {
        return false;
    }
    let Some(pin) = comp.pins.iter().find(|p| p.number == pad) else {
        return false;
    };
    if control_lint_covers(board, comp, pin) {
        return true;
    }
    let required = model
        .params
        .get_str("must_not_float_roles")
        .unwrap_or_default()
        .split(',')
        .any(|r| r.trim() == strap.role);
    let gpio = matches!(hold, Hold::MayFloat(w) if w.iter().any(|w| w.kind == WeakKind::Gpio));
    required && !gpio
}

/// Whether a weak holder of the net is an MCU pad that is an output once
/// firmware runs.
fn driven_by_mcu(hold: &Hold) -> bool {
    matches!(hold, Hold::MayFloat(weak) if weak.iter().any(|w| w.kind == WeakKind::McuOutput))
}

/// The note for a boot strap with no internal pull whose net an MCU output
/// drives. The level the pin latches is the driver's only once its firmware
/// runs; while that MCU is in reset its pad is high-impedance and nothing sets
/// the strap. That is a note (`Low`), never a fault: the driver holds the level
/// in normal operation. A strap that is not a boot-select latch, or whose mode
/// the board cannot select, has no reset window worth a note.
fn driven_strap_note(
    board: &ExtractedBoard,
    (comp, model): (&Component, &ModelEntry),
    strap: &StrapPin,
    (pad_num, net_name): (&str, &str),
    hold: &Hold,
) -> Option<LintFinding> {
    let Hold::MayFloat(weak) = hold else {
        return None;
    };
    if !strap.boot_select
        || strap
            .only_with
            .as_deref()
            .is_some_and(|selector| !selector_is_drivable(board, comp, model, selector))
    {
        return None;
    }
    let drivers = weak
        .iter()
        .filter(|w| w.kind == WeakKind::McuOutput)
        .map(|w| w.label.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let fix = pull_fix(board, comp, model, strap, net_name);
    Some(LintFinding {
        check: LintCheck::StrapPin,
        severity: Severity::Low,
        message: format!(
            "{} strap pin {pad_num} ({}) is driven by {drivers} on net '{net_name}', so it has a level once that firmware runs. The datasheet latches this pin at reset ({}) and the pin has no internal pull, so while the driving MCU is in reset or has not yet configured its pad the boot level is undefined. If the part must boot the application whatever the driver is doing, {}",
            comp.reference,
            strap.role,
            strap.note,
            fix.replacen("Add", "add", 1),
        ),
        refs: vec![comp.reference.clone()],
        nets: vec![net_name.to_string()],
    })
}

/// One strap pin as placed on the board: its part and card, the strap row, the
/// pad that carries it and the net on that pad.
struct StrapSite<'a> {
    comp: &'a Component,
    model: &'a ModelEntry,
    strap: &'a StrapPin,
    pad: &'a str,
    net: &'a str,
}

/// The pull the silicon applies to a strap's pad: the strap row's own, else the
/// card's boot table (`[[models.boot_inputs]]`, which the ESP cards fill).
fn effective_pull(model: &ModelEntry, strap: &StrapPin) -> StrapInternalPull {
    if strap.internal_pull != StrapInternalPull::Unknown {
        return strap.internal_pull;
    }
    model
        .boot_table()
        .input(&strap.role)
        .map_or(StrapInternalPull::Unknown, |i| i.internal_pull)
}

/// How bad a strap left to float is, from the role the card gives it. A pin
/// that selects the boot mode, or one whose card row states no pull at all
/// (an enable, a reset), is High. A pin whose pull comes only from the boot
/// table follows its datasheet role: a level that must be defined (the flash
/// supply select) is Medium, and a preference the part tolerates (a glitch
/// guard, the boot-log silencer) is Low.
fn float_severity(strap: &StrapPin) -> Severity {
    if strap.boot_select || strap.internal_pull != StrapInternalPull::Unknown {
        Severity::High
    } else if strap.level == StrapLevel::Defined {
        Severity::Medium
    } else {
        Severity::Low
    }
}

/// What a boot-mode strap reads and where the part goes with the user able to
/// leave `net` open (or an undriven net settling to `pull`), in the vendor's
/// words. `None` when the card carries no boot table or the other straps leave
/// the table's answer open.
fn open_position_outcome(
    site: &StrapSite<'_>,
    pull: StrapInternalPull,
    with_jumper_open: bool,
) -> Option<String> {
    let table = site.model.boot_table();
    if table.is_empty() {
        return None;
    }
    let settled = pull_level(pull);
    let levels = BTreeMap::from([(site.strap.role.clone(), settled)]);
    let pin = table
        .input(&site.strap.role)
        .map_or_else(|| site.strap.role.to_uppercase(), BootInput::display_name);
    let basis = match (settled, with_jumper_open) {
        (Some(PinLevel::High), true) => "with the jumper open, internal pull-up",
        (Some(PinLevel::Low), true) => "with the jumper open, internal pull-down",
        (None, true) => "with the jumper open, floating",
        (Some(PinLevel::High), false) => "undriven, internal pull-up",
        (Some(PinLevel::Low), false) => "undriven, internal pull-down",
        (None, false) => "undriven, floating",
    };
    let level = match settled {
        Some(PinLevel::High) => StrapReadingLevel::High,
        Some(PinLevel::Low) => StrapReadingLevel::Low,
        None => StrapReadingLevel::Indeterminate,
    };
    let reading = StrapReading {
        role: site.strap.role.clone(),
        pin,
        net: Some(site.net.to_string()),
        volts: None,
        level,
        basis: basis.to_string(),
    };
    match select_mode(table.modes, &levels) {
        Selection::Row(row) => Some(BootDecision::sentence(row.outcome, &row.name, &[reading])),
        // Another strap still decides, so only a floating pin is worth saying.
        Selection::Indeterminate { .. } | Selection::NoRow if settled.is_none() => Some(format!(
            "{} at reset release -> the boot mode is undefined",
            reading.describe()
        )),
        Selection::Indeterminate { .. } | Selection::NoRow => None,
    }
}

/// The finding for a strap a silicon pull settles to the wrong level, or
/// `None` when the settled level is the one the part needs, the card gives no
/// pull or no level to judge, or the net is not left to that pull. With `weak`
/// the pin is held by a jumper or header the user can leave open and the
/// finding reads the open position; without, nothing is on the net at all.
fn wrong_default_finding(
    board: &ExtractedBoard,
    site: &StrapSite<'_>,
    pull: StrapInternalPull,
    weak: Option<&[Weak]>,
) -> Option<LintFinding> {
    let settled = pull_level(pull)?;
    let want = match site.strap.level {
        StrapLevel::High => PinLevel::High,
        StrapLevel::Low => PinLevel::Low,
        StrapLevel::Defined => return None,
    };
    if settled == want {
        return None;
    }
    let (comp, strap) = (site.comp, site.strap);
    let who = weak.map_or_else(
        || format!("net '{}' has nothing on it", site.net),
        |weak| {
            let holders = weak
                .iter()
                .map(|w| w.label.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "net '{}' is held only by {holders}, which set no level while {}",
                site.net,
                strap_when_open(weak)
            )
        },
    );
    let outcome = open_position_outcome(site, pull, weak.is_some())
        .map(|o| format!(" {o}."))
        .unwrap_or_default();
    Some(LintFinding {
        check: LintCheck::StrapPin,
        severity: Severity::Medium,
        message: format!(
            "{} strap pin {} ({}) {who}, so the pin's own pull-{} settles it {} at reset, but it must be {} ({}).{outcome} {}",
            comp.reference,
            site.pad,
            strap.role,
            if settled == PinLevel::High { "up" } else { "down" },
            settled.as_str(),
            want.as_str(),
            strap.note,
            pull_fix(board, comp, site.model, strap, site.net),
        ),
        refs: vec![comp.reference.clone()],
        nets: vec![site.net.to_string()],
    })
}

/// The finding for a strap with no internal pull that nothing holds, or `None`
/// when a mode-qualifying strap's mode is unreachable on this board.
fn floating_finding(
    board: &ExtractedBoard,
    site: &StrapSite<'_>,
    hold: &Hold,
) -> Option<LintFinding> {
    let StrapSite {
        comp,
        model,
        strap,
        pad: pad_num,
        net: net_name,
    } = *site;
    let (severity, selector) = match strap.only_with.as_deref() {
        // A mode-qualifying strap: its float matters only when the board can
        // select the mode it qualifies.
        Some(selector) => {
            if !selector_is_drivable(board, comp, model, selector) {
                return None;
            }
            (Severity::Medium, Some(selector))
        }
        // High (vs the wrong-pull arm's Medium): a wrong static pull at least
        // boots *deterministically* wrong, so it is caught on the first
        // power-up at the bench. A float boots correctly often enough to pass
        // the bench yet fail intermittently in the field (humidity, EMI, a
        // probe), a latent, unrecoverable failure that survives bring-up, which
        // is the more dangerous outcome. A strap the card gives only a tolerated
        // preference follows its datasheet role (see `float_severity`).
        None => (float_severity(strap), None),
    };
    // A jumper the user can leave open: what the part does in that position.
    let open = matches!(hold, Hold::MayFloat(w) if w.iter().any(|w| w.kind == WeakKind::Jumper))
        .then(|| open_position_outcome(site, StrapInternalPull::None, true))
        .flatten()
        .map(|o| format!(" With the jumper open: {o}."))
        .unwrap_or_default();
    let message = match (hold, selector) {
        (Hold::MayFloat(weak), _) => {
            let holders = weak.iter().map(|w| w.label.as_str()).collect::<Vec<_>>().join(", ");
            let when = strap_when_open(weak);
            let fix = pull_fix(board, comp, model, strap, net_name);
            let why = selector.map_or_else(
                || "so the level it needs is undefined at reset and the part may boot wrong (for a boot strap, enter the bootloader instead of the application)".to_string(),
                |sel| format!("and the board can drive the {sel} boot strap, so the level this pin must hold in that mode is undefined at reset"),
            );
            format!(
                "{} strap pin {pad_num} ({}) may float: net '{net_name}' is held only by {holders}, none of which sets a level while {when}, and the pin has no internal pull, {why} ({}).{open} {fix}",
                comp.reference, strap.role, strap.note
            )
        }
        (_, Some(sel)) => format!(
            "{} strap pin {pad_num} ({}) is left floating (net '{net_name}' has no pull resistor and the pin has no internal pull), and the board can drive the {sel} boot strap: the level this pin must hold in that mode is undefined at reset ({}). {}",
            comp.reference,
            strap.role,
            strap.note,
            pull_fix(board, comp, model, strap, net_name)
        ),
        (_, None) => {
            let why = if strap.boot_select {
                "the part may enter the bootloader instead of the application"
            } else {
                "the part may latch the wrong setting"
            };
            format!(
                "{} strap pin {pad_num} ({}) is left floating (net '{net_name}' has no pull resistor and the pin has no internal pull): the level it needs is undefined at reset, so {why} ({}). {}",
                comp.reference,
                strap.role,
                strap.note,
                pull_fix(board, comp, model, strap, net_name)
            )
        }
    };
    Some(LintFinding {
        check: LintCheck::StrapPin,
        severity,
        message,
        refs: vec![comp.reference.clone()],
        nets: vec![net_name.to_string()],
    })
}

/// When a strap net's weak holders let go, in words.
fn strap_when_open(weak: &[Weak]) -> String {
    let kinds: BTreeSet<&str> = weak
        .iter()
        .map(|w| match w.kind {
            WeakKind::Switch => "no button or switch is closed",
            WeakKind::Connector => "nothing is plugged in",
            WeakKind::Gpio | WeakKind::McuOutput => {
                "the driving MCU is in reset and its pad is high-impedance"
            }
            WeakKind::Sink => "the transistor is off",
            WeakKind::Jumper => "the jumper is open",
        })
        .collect();
    kinds.into_iter().collect::<Vec<_>>().join(" or while ")
}

/// The pull that fixes a floating strap: a 10 kOhm resistor to the level the
/// strap needs at reset.
fn pull_fix(
    board: &ExtractedBoard,
    comp: &Component,
    model: &ModelEntry,
    strap: &StrapPin,
    net_name: &str,
) -> String {
    let vdd = role_net_id(comp, model, strap.supply_role(model))
        .and_then(|id| board.net(id))
        .map_or_else(|| "its supply".to_string(), |n| format!("'{}'", n.name));
    match strap.level {
        StrapLevel::High => format!("Add a 10 kOhm pull-up to {vdd} on '{net_name}'"),
        StrapLevel::Low => format!("Add a 10 kOhm pull-down to GND on '{net_name}'"),
        StrapLevel::Defined => format!(
            "Add a 10 kOhm pull-up to {vdd} or pull-down to GND on '{net_name}', whichever level the application needs at reset"
        ),
    }
}

/// Whether the board can put the boot-select strap `selector` of `comp` into its
/// alternate mode. Its net must carry something that can move it off the
/// normal level: a button, header, transistor, another IC, a series path, or a
/// pull to the opposite level. A pull to the normal level and a capacitor only
/// hold the normal mode, so a net with nothing else on it is not drivable. An
/// unlocatable selector counts as not drivable, so a card error never
/// manufactures a finding.
fn selector_is_drivable(
    board: &ExtractedBoard,
    comp: &Component,
    model: &ModelEntry,
    selector: &str,
) -> bool {
    let Some(strap) = model.straps.iter().find(|s| s.role == selector) else {
        return false;
    };
    let Ok((_, net_id)) = strap_site(comp, model, strap) else {
        return false;
    };
    if board
        .net(net_id)
        .is_none_or(|net| is_unconnected_net(&net.name))
    {
        return false;
    }
    // The far side of a resistor that only holds the normal level.
    let holds_normal = |far: i64| {
        board.net(far).is_some_and(|n| match strap.level {
            StrapLevel::High => is_rail_name(&n.name),
            StrapLevel::Low => is_ground_name(&n.name),
            StrapLevel::Defined => is_rail_name(&n.name) || is_ground_name(&n.name),
        })
    };
    board.net_members(net_id).into_iter().any(|(c, _)| {
        if c.reference == comp.reference || !AssemblyState::of(c).is_present() {
            return false;
        }
        if is_assembled_resistor(c) {
            let mut far = c
                .pins
                .iter()
                .filter_map(|p| p.net)
                .filter(|id| *id != net_id);
            return !far.all(&holds_normal);
        }
        !hauksbee_extract::is_capacitor(c)
    })
}

/// Map a strap role to the pad that carries it, then to that pad's net, or say
/// why the strap has no net to examine.
fn strap_site(
    comp: &Component,
    model: &ModelEntry,
    strap: &StrapPin,
) -> Result<(String, i64), &'static str> {
    let pad = model
        .pins
        .iter()
        .find(|(_, role)| role.as_str() == strap.role)
        .map(|(pad, _)| pad.clone())
        .ok_or("the model maps no pad to it")?;
    let pin = comp
        .pins
        .iter()
        .find(|p| p.number == pad)
        .ok_or("its pad is not on this footprint")?;
    let net = pin.net.ok_or("its pad is unrouted")?;
    Ok((pad, net))
}

/// One line per fitted part with a strap table.
///
/// It says how far the strap lint examined each strap. The lint only emits rows
/// for faults, so without this a strap that was checked and found correct, a
/// strap whose level is not judged, and a strap that could not be located all
/// read as the same silence.
#[must_use]
pub fn strap_coverage(board: &ExtractedBoard, lib: &ModelLibrary) -> Vec<String> {
    let models: BTreeMap<&str, ModelEntry> = super::resolved_components(board, lib)
        .map(|(c, m)| (c.reference.as_str(), m))
        .collect();
    let senses = BTreeSet::new();
    let classify = Classifier {
        board,
        models: &models,
        senses: &senses,
        strict: true,
        open_jumpers: true,
    };
    let mut lines = Vec::new();
    for comp in &board.components {
        let Some(part) = AssemblyState::of(comp).fitted() else {
            continue;
        };
        let res = resolve(lib, part);
        let Some(model) = res.model.as_ref().filter(|m| !m.straps.is_empty()) else {
            continue;
        };
        let each: Vec<String> = model
            .straps
            .iter()
            .map(|strap| {
                let (pad, net_id) = match strap_site(comp, model, strap) {
                    Ok(site) => site,
                    Err(why) => return format!("{} NOT checked ({why})", strap.role),
                };
                let name = board.net(net_id).map_or("?", |n| n.name.as_str());
                let hold = strap_hold(&classify, comp, &pad, name);
                let bare = strap_net_is_bare(board, net_id, comp);
                let no_pull = effective_pull(model, strap) == StrapInternalPull::None;
                let depth = if hold == Hold::Floats && (bare || no_pull) {
                    "unconnected, float check only"
                } else if driven_by_mcu(&hold) && no_pull {
                    "driven by an MCU output, reset-window note only"
                } else if matches!(hold, Hold::MayFloat(_)) && no_pull {
                    "held only by parts that may let go (button, header, MCU pad), may-float check"
                } else if strap.boot_select && strap.level != StrapLevel::Defined {
                    "clock source and pull bias checked"
                } else {
                    "clock source checked, bias level not judged (not a boot-select strap)"
                };
                format!("{} (pad {pad}, net '{name}'): {depth}", strap.role)
            })
            .collect();
        lines.push(format!(
            "strap pins on {} ({}): {}",
            comp.reference,
            model.id,
            each.join("; ")
        ));
    }
    lines
}

/// Inspect the members of one strap net and emit a finding if it cannot hold the
/// required level at reset.
fn examine_strap(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    rails: &RailResolver<'_>,
    (mcu, model): (&Component, &ModelEntry),
    strap: &StrapPin,
    net_id: i64,
    report: &mut NetLintReport,
) {
    let (role, note, net_name) = (
        &strap.role,
        &strap.note,
        &board.net(net_id).map_or("", |n| &n.name),
    );
    let mut push = |severity, message: String, mut refs: Vec<String>| {
        refs.insert(0, mcu.reference.clone());
        let nets = vec![net_name.to_string()];
        let message = format!(
            "{} strap pin {role} net '{net_name}' {message} ({note})",
            mcu.reference
        );
        report.findings.push(LintFinding {
            check: LintCheck::StrapPin,
            severity,
            message,
            refs,
            nets,
        });
    };

    // (a) A free-running clock source on the strap net. This is the Olimex GPIO0
    //     fault: a powered oscillator reaches the pin (here through a small
    //     series resistor), so a clock is present at the reset latch window. We
    //     look across the net AND one series-resistor hop, because the real
    //     board puts a 10R between the oscillator and GPIO0.
    if let Some(osc) = clock_source_reaching(board, net_id).filter(|o| o.reference != mcu.reference)
    {
        // A clock whose enable a modelled detector holds off from power-up until
        // the part's own enable/reset input has risen is the designed fix for
        // this fault: whether it lasts past the latch is timing, which a netlist
        // check does not judge. That is an abstention (Low), never a pass.
        if let Some(gate) = clock_held_off_past_reset(board, lib, osc, net_id, (mcu, model)) {
            let what = format!(
                "cannot judge the clock from {} ({}): {}, so the clock is absent at the reset latch only if that release lags the strap hold time; the timing is not judged, so its boot level is unchecked",
                osc.reference, osc.value, gate.said
            );
            return push(
                Severity::Low,
                what,
                vec![osc.reference.clone(), gate.detector],
            );
        }
        let what = format!("carries a free-running clock source {} ({}): a clock present at the reset latch can mis-strap the part", osc.reference, osc.value);
        return push(Severity::High, what, vec![osc.reference.clone()]); // the clock is the dominant fault
    }

    // (b) Pull resistors biasing the strap to the WRONG level for normal boot.
    //     Only for a hard HIGH/LOW *boot-select* strap whose wrong level is
    //     unrecoverable (BOOT0, GPIO0, GPIO9, QSPI_SS). A cosmetic or
    //     flash-voltage strap (ESP32 GPIO15 boot-log, GPIO2, GPIO12) may be
    //     legitimately repurposed as an ordinary GPIO with a pull on a shipped
    //     board, so a wrong-bias finding there would be a confident false
    //     positive on a correct design - exactly the cardinal sin.
    let want_high = strap.level == StrapLevel::High;
    if !strap.boot_select || !(want_high || strap.level == StrapLevel::Low) {
        return;
    }
    let pulls = pulls_on(board, net_id);
    if pulls.iter().any(|p| p.up == want_high) && pulls.iter().any(|p| p.up != want_high) {
        // Pulls both ways: a divider sets the level, not "a correct pull exists".
        return match judge_divider(board, rails, (mcu, model), strap, &pulls) {
            Ok(None) => {}
            Ok(Some((severity, what))) => push(
                severity,
                what,
                pulls.into_iter().map(|p| p.reference).collect(),
            ),
            // Low is reserved for this abstention: the plain and JSON renderers word it as unchecked.
            Err(why) => push(
                Severity::Low,
                format!(
                    "cannot judge divider ({}): {why}, so its boot level is unchecked",
                    describe(&pulls)
                ),
                Vec::new(),
            ),
        };
    }
    // One-sided: against an internal pull (~45 kOhm on ESP32) a wrong pull
    // only counts when it is *strong*; <= 20 kOhm is a real pull. A pin with
    // no internal pull (STM32 BOOT0) has nothing to fight, so any wrong-way
    // resistor, of any or unreadable value, alone sets the boot level.
    let no_internal = strap.internal_pull == StrapInternalPull::None;
    let strong = |p: &Pull| no_internal || p.ohms.is_some_and(|r| r <= 20_000.0);
    // A fitted switch, transistor or IC on the net (directly or behind one series
    // resistor) can pull the strap to the other level on the board itself: the
    // wrong-way resistor is then the idle bias of a designed boot circuit (a
    // bootloader button or a gated pull-down), not the only thing setting the
    // level. Which level wins at reset is dynamic, so the static lint abstains.
    if level_switched_on_board(board, lib, net_id, mcu, want_high) {
        return;
    }
    if let Some(p) = pulls.iter().find(|p| p.up != want_high && strong(p)) {
        let dir = if p.up {
            "pull-up to a rail"
        } else {
            "pull-down to ground"
        };
        let far = board.net(p.far).map_or("?", |n| n.name.as_str());
        let to = if want_high {
            "the I/O supply"
        } else {
            "ground"
        };
        let against = if no_internal {
            "the pin has no internal pull, so this resistor alone sets the level".to_string()
        } else {
            format!("at {} it overrides the pin's internal pull", p.value.trim())
        };
        let what = format!(
            "has a {dir} ({} to '{far}') but normal boot needs {}: the strap is biased to the wrong level ({against}). Move {} to {to}, or replace it with a pull to {to}",
            p.reference,
            strap.level.as_str(),
            p.reference
        );
        push(Severity::Medium, what, vec![p.reference.clone()]);
    }
}

/// Is a fitted part that can move the strap net to its *normal* boot level on
/// the board itself on the net directly or behind one series resistor? Then a
/// wrong-way resistor is the idle bias of a designed boot circuit, not the only
/// thing setting the level. A pull resistor to a rail or ground is a bias, not
/// a hop. A header or test point does not count: what an off-board plug drives
/// is not a design on this board. The strap's own MCU never counts.
///
/// Only a part that can actually drive the net counts:
/// - a transistor (it may switch the strap either way, e.g. an NMOS pulling
///   BOOT0 low for normal boot against a 1M idle pull-up);
/// - a switch with a terminal off the wrong-level supply. A BOOTSEL button
///   whose far side is ground when normal boot needs high only pushes the
///   strap further the wrong way, so it never excuses a wrong-way pull;
/// - an IC whose pin on the net is not input-only. A peripheral's chip-select
///   input (the QSPI flash `/CS` sharing an RP2040's `QSPI_SS`) cannot set a
///   level at all.
fn level_switched_on_board(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    net_id: i64,
    mcu: &Component,
    want_high: bool,
) -> bool {
    let wrong_supply = |id: i64| {
        board.net(id).is_some_and(|n| {
            if want_high {
                is_ground_name(&n.name)
            } else {
                is_rail_name(&n.name)
            }
        })
    };
    let drives = |c: &Component, at: i64| {
        if c.reference == mcu.reference || !AssemblyState::of(c).is_present() {
            return false;
        }
        match RefClass::of(&c.reference) {
            RefClass::Transistor => true,
            RefClass::ActiveIc => c
                .pins
                .iter()
                .filter(|p| p.net == Some(at))
                .any(|p| !input_only_pin(lib, c, p)),
            class if class == RefClass::Switch || designator(&c.reference).starts_with("SW") => c
                .pins
                .iter()
                .filter_map(|p| p.net)
                .any(|id| id != at && !wrong_supply(id)),
            _ => false,
        }
    };
    let on_net = |id: i64| {
        board
            .net_members(id)
            .into_iter()
            .any(|(c, _)| drives(c, id))
    };
    if on_net(net_id) {
        return true;
    }
    board
        .net_members(net_id)
        .into_iter()
        .filter(|(c, _)| is_assembled_resistor(c))
        .flat_map(|(c, _)| c.pins.iter().filter_map(|p| p.net))
        .filter(|id| *id != net_id)
        .filter(|id| {
            board
                .net(*id)
                .is_some_and(|n| !is_ground_name(&n.name) && !is_rail_name(&n.name))
        })
        .any(on_net)
}

/// Whether an IC pin can only sense its net, never drive it: the schematic
/// types it `input`; its bound card names it a logic input or a peripheral's
/// chip-select role; or, on a part no MCU card binds, its pin name is a
/// chip-select (`/CS`, `~{CS}`, `!CS!`, `CS#`, `SSEL`, `nCE`), which on a SPI
/// peripheral is an input by definition. Anything else may drive.
fn input_only_pin(lib: &ModelLibrary, c: &Component, p: &hauksbee_extract::Pin) -> bool {
    if p.kind.trim().eq_ignore_ascii_case("input") {
        return true;
    }
    let model = AssemblyState::of(c)
        .fitted()
        .and_then(|part| resolve(lib, part).model);
    if let Some(m) = &model {
        if let Some(role) = m.pins.get(&p.number) {
            let cs = match &m.peripheral {
                Some(
                    PeripheralSpec::SpiNorFlash { cs_role, .. }
                    | PeripheralSpec::RegisterMap { cs_role, .. },
                ) => Some(cs_role.as_str()),
                _ => None,
            };
            if cs.is_some_and(|cs| cs.eq_ignore_ascii_case(role))
                || m.logic.inputs.iter().any(|i| i.eq_ignore_ascii_case(role))
            {
                return true;
            }
        }
        if m.kind == ComponentKind::Mcu {
            return false; // a controller's chip-select pin is an output
        }
    }
    is_chip_select_name(&p.function)
}

/// A dedicated chip-select pin name, active-low decoration stripped.
fn is_chip_select_name(function: &str) -> bool {
    let core: String = function
        .trim()
        .to_ascii_uppercase()
        .chars()
        .filter(|ch| !matches!(ch, '~' | '{' | '}' | '!' | '/' | '#' | ' '))
        .collect();
    let core = core
        .strip_suffix("_N")
        .or_else(|| core.strip_suffix("_B"))
        .unwrap_or(&core);
    matches!(
        core,
        "CS" | "CSN" | "NCS" | "CSB" | "SS" | "SSN" | "SSEL" | "CE" | "NCE"
    )
}

/// A plain two-terminal resistor on the shared part-class ladder
/// (`hauksbee_extract::is_plain_resistor`), and assembled (not DNP). Shared with
/// the boot check's bias-resistor predicate: a varistor or thermistor to ground
/// sets no DC level and is never credited as a bias.
pub(crate) fn is_assembled_resistor(c: &Component) -> bool {
    AssemblyState::of(c).is_present() && hauksbee_extract::is_plain_resistor(c)
}

/// Is this component a *powered oscillator* (a free-running clock source), as
/// opposed to a bare crystal (a passive resonator that does not drive on its
/// own)? An oscillator is a packaged part with its own VDD; we key on the
/// reference / value / `lib_id`, and require it to NOT be a plain 2-terminal
/// crystal. DNP oscillators are not assembled, so they do not drive.
fn is_clock_oscillator(c: &Component) -> bool {
    if !AssemblyState::of(c).is_present() {
        return false;
    }
    let r = c.reference.to_ascii_uppercase();
    let v = c.value.to_ascii_uppercase();
    let lib = c.lib_id.to_ascii_lowercase();
    // An oscillator has >= 3 connected pads (OUT/GND/VDD, often +OE); a bare
    // crystal is 2-terminal. Refuse 2-terminal parts outright.
    let connected = c.pins.iter().filter(|p| p.net.is_some()).count();
    if connected < 3 {
        return false;
    }
    // The >= 3-pad gate (a powered oscillator, not a 2-terminal crystal) plus
    // the rail/ground hop-exclusion in clock_source_reaching is what keeps this
    // sound. The value-string match below is the soft edge: a 3+-pad part whose
    // value merely contains "MHZ"/"OSC" could be mis-classed, but it only
    // matters if it ALSO sits on a strap signal net (or one series-R away), and
    // a free-running clock that close to a strap pin is the fault we want. The
    // lib_id "oscillator" match is the strong signal; the rest are value/ref
    // heuristics for parts whose symbol library does not say "oscillator".
    lib.contains("oscillator")
        || v.contains("OSCILLATOR")
        || v.contains("MHZ")
        || v.contains("OSC")
        || v.contains("XO")
        // Olimex's value string is "Q50MHz/..." (caught by MHZ above). CR is
        // the IEEE diode designator, so it is not an oscillator by itself.
        || r.starts_with("OSC")
        || (r.starts_with('X') && v.contains("MHZ"))
        || (r.starts_with('Y') && v.contains("MHZ"))
}

/// Find a free-running clock source whose *output* reaches `net_id` either
/// directly or through exactly one assembled series resistor (the real board
/// puts a 10R between the oscillator output and GPIO0).
///
/// The hop deliberately never travels through a power rail or ground: a clock
/// signal does not propagate via VDD/GND, and an oscillator's VDD pin sits on a
/// rail. Without that guard, a strap with an ordinary pull-up to +3V3 would
/// "reach" the oscillator's VDD on the same rail and false-fire (the exact bug
/// the GPIO15 pull-up exposed during calibration). So the oscillator must be
/// reached on a *signal* net, via a pad that is not on a rail/ground.
fn clock_source_reaching(board: &ExtractedBoard, net_id: i64) -> Option<&Component> {
    // The strap net itself must not be a rail/ground (it never is for a strap,
    // but guard anyway so the membership scan is meaningful).
    if let Some(n) = board.net(net_id) {
        if is_ground_name(&n.name) || is_rail_name(&n.name) {
            return None;
        }
    }
    // Direct: an oscillator drives this signal net via a non-power pad.
    if let Some(c) = oscillator_driving_net(board, net_id) {
        return Some(c);
    }
    // One series-resistor hop: a resistor with one pad on the strap net and the
    // other pad on a *signal* net (not a rail/ground) that an oscillator drives.
    for (c, _p) in board.net_members(net_id) {
        if !is_assembled_resistor(c) {
            continue;
        }
        for op in &c.pins {
            let Some(oid) = op.net else { continue };
            if oid == net_id {
                continue;
            }
            // Never hop through a power rail or ground.
            if let Some(on) = board.net(oid) {
                if is_ground_name(&on.name) || is_rail_name(&on.name) {
                    continue;
                }
            }
            if let Some(c) = oscillator_driving_net(board, oid) {
                return Some(c);
            }
        }
    }
    None
}

/// An assembled oscillator that touches `net_id` via a pad that is NOT one of
/// its power pads (a clock OUTPUT, not its VDD/GND). We identify power pads by
/// the net they sit on (rail/ground), since PCB-only inputs carry no pin
/// functions. So: the oscillator drives this net iff one of its pads is on
/// `net_id` and that net is not a rail/ground.
fn oscillator_driving_net(board: &ExtractedBoard, net_id: i64) -> Option<&Component> {
    let net = board.net(net_id)?;
    if is_ground_name(&net.name) || is_rail_name(&net.name) {
        return None;
    }
    for (c, _p) in board.net_members(net_id) {
        if is_clock_oscillator(c) {
            return Some(c);
        }
    }
    None
}

/// A detector found holding a strap clock's enable off through reset.
struct ClockGate {
    /// The detector's reference.
    detector: String,
    /// How it gates the clock, for the finding.
    said: String,
}

/// Is `role` a strapped part's enable or reset input (the pin whose rising
/// edge latches the straps)?
fn enable_or_reset_role(role: &str) -> bool {
    matches!(role, "en" | "enable" | "chip_en" | "chip_pu")
        || role
            .split('_')
            .any(|t| matches!(t, "reset" | "rst" | "nrst" | "resetn"))
}

/// Whether net `id` follows one of `reset_nets`, so a detector `det` sensing it
/// releases only after the reset does.
///
/// It does when it is that net, or a node that only the reset net charges (an
/// RC delay into the detector): every other part on
/// it is a resistor from the reset net, a capacitor to ground, the detector
/// itself, or a discharge path that can only carry current away from it (a
/// diode anode, directly or through one resistor, per the diode's card). Read
/// from the topology and the cards, never the names: the reset net's own
/// pull-up reaches the rail through one resistor too, but the rail has its
/// own sources, and a detector on it releases with the supply, not after the
/// reset.
fn follows_reset(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    reset_nets: &[i64],
    id: i64,
    det: &str,
) -> bool {
    let from_reset = |c: &Component, id: i64| {
        is_assembled_resistor(c)
            && c.pins
                .iter()
                .any(|p| p.net.is_some_and(|n| n != id && reset_nets.contains(&n)))
    };
    let to_ground = |c: &Component, id: i64| {
        hauksbee_extract::is_capacitor(c)
            && c.pins.iter().any(|p| {
                p.net.is_some_and(|n| {
                    n != id && board.net(n).is_some_and(|n| is_ground_name(&n.name))
                })
            })
    };
    // Current leaves `id` through this pin only: it is a fitted diode's anode.
    let anode_on = |c: &Component, id: i64| {
        AssemblyState::of(c).fitted().is_some_and(|f| {
            resolve(lib, f).model.is_some_and(|card| {
                card.kind == ComponentKind::Diode
                    && c.pins.iter().filter(|p| p.net == Some(id)).all(|p| {
                        card.pins
                            .get(&p.number)
                            .is_some_and(|r| r.to_ascii_lowercase().starts_with("anode"))
                    })
            })
        })
    };
    let discharges = |c: &Component, id: i64| {
        anode_on(c, id)
            || is_assembled_resistor(c)
                && c.pins
                    .iter()
                    .filter_map(|p| p.net)
                    .filter(|&n| n != id)
                    .all(|n| {
                        board
                            .net_members(n)
                            .iter()
                            .all(|(o, _)| o.reference == c.reference || anode_on(o, n))
                    })
    };
    if reset_nets.contains(&id) {
        return true;
    }
    let members = board.net_members(id);
    members.iter().any(|(c, _)| from_reset(c, id))
        && members.iter().all(|(c, _)| {
            c.reference == det || from_reset(c, id) || to_ground(c, id) || discharges(c, id)
        })
}

/// The oscillator `osc` reaching strap net `strap_net` has an enable held low
/// from power-up by a fitted part whose card says so, and that part's sensing
/// input follows `mcu`'s own enable/reset net.
///
/// Every link is read from the cards, never from names: the detector's card
/// holds its output pin at 0 V in its FSM's initial state (a voltage law
/// `only_in_state` that state), it leaves that state on a guard over the
/// voltage of a pin that sits on the MCU card's enable/reset net or one
/// assembled resistor from it, and the oscillator's pad on the gated net is
/// not on the clock's path to the strap. A detector sensing a rail, or one
/// with no card, gates nothing the check can see.
fn clock_held_off_past_reset(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    osc: &Component,
    strap_net: i64,
    (mcu, model): (&Component, &ModelEntry),
) -> Option<ClockGate> {
    let power = |id: i64| {
        board
            .net(id)
            .is_none_or(|n| is_ground_name(&n.name) || is_rail_name(&n.name))
    };
    // The MCU's enable/reset nets, by its card's roles.
    let reset_nets: Vec<i64> = mcu
        .pins
        .iter()
        .filter(|p| {
            model
                .pins
                .get(&p.number)
                .is_some_and(|role| enable_or_reset_role(&role.to_ascii_lowercase()))
        })
        .filter_map(|p| p.net)
        .filter(|&id| !power(id))
        .collect();
    if reset_nets.is_empty() {
        return None;
    }
    // The clock's own path: the strap net and every net one resistor from it.
    let mut clock_path = vec![strap_net];
    for (c, _) in board.net_members(strap_net) {
        if is_assembled_resistor(c) {
            clock_path.extend(c.pins.iter().filter_map(|p| p.net));
        }
    }
    for gate_net in osc.pins.iter().filter_map(|p| p.net) {
        if power(gate_net) || clock_path.contains(&gate_net) {
            continue;
        }
        for (det, det_pin) in board.net_members(gate_net) {
            if det.reference == osc.reference || det.reference == mcu.reference {
                continue;
            }
            let Some(fitted) = AssemblyState::of(det).fitted() else {
                continue;
            };
            let Some(card) = resolve(lib, fitted).model else {
                continue;
            };
            let Some(fsm) = card.behavioral.fsm.as_ref() else {
                continue;
            };
            let Some(initial) = fsm.initial.as_ref().or_else(|| fsm.states.first()) else {
                continue;
            };
            let Some(out_role) = card.pins.get(&det_pin.number) else {
                continue;
            };
            let holds_low = card.behavioral.laws.iter().any(|law| {
                law.kind == hauksbee_models::behavioral::LawKind::Voltage
                    && law.a == *out_role
                    && law.only_in_state.as_deref() == Some(initial.as_str())
                    && law.expr.trim().parse::<f64>().is_ok_and(|v| v == 0.0)
            });
            if !holds_low {
                continue;
            }
            // The pins whose voltage releases the detector from its initial state.
            let sensed = det.pins.iter().find_map(|p| {
                let role = card.pins.get(&p.number)?;
                let var = format!("v_{role}");
                let releases = fsm
                    .transitions
                    .iter()
                    .any(|t| t.from == *initial && t.guard.contains(&var));
                let net = p.net?;
                (releases && follows_reset(board, lib, &reset_nets, net, &det.reference))
                    .then(|| (role.clone(), net))
            });
            let Some((sense_role, sense_net)) = sensed else {
                continue;
            };
            let name = |id: i64| board.net(id).map_or("", |n| n.name.as_str());
            return Some(ClockGate {
                detector: det.reference.clone(),
                said: format!(
                    "{} ({}) holds its enable net '{}' low from power-up until its {sense_role} input on '{}', which follows {}'s enable/reset, passes the detector's threshold",
                    det.reference,
                    det.value,
                    name(gate_net),
                    name(sense_net),
                    mcu.reference
                ),
            });
        }
    }
    None
}

/// One assembled resistor from the strap net to a rail (`up`) or to ground.
pub(super) struct Pull {
    pub(super) reference: String,
    pub(super) value: String,
    pub(super) far: i64,
    pub(super) up: bool,
    /// `None` when the value string does not parse to ohms.
    pub(super) ohms: Option<f64>,
    /// Fractional tolerance when the part states one ("10k 1%", or a
    /// `Tolerance` property); zero when unknown.
    pub(super) tol: f64,
}

pub(super) fn pulls_on(board: &ExtractedBoard, net_id: i64) -> Vec<Pull> {
    let mut pulls = Vec::new();
    for (c, _) in board
        .net_members(net_id)
        .into_iter()
        .filter(|(c, _)| is_assembled_resistor(c))
    {
        for far in c
            .pins
            .iter()
            .filter_map(|p| p.net)
            .filter(|id| *id != net_id)
        {
            let Some(name) = board.net(far).map(|n| n.name.as_str()) else {
                continue;
            };
            if !(is_ground_name(name) || is_rail_name(name)) {
                continue;
            }
            let ohms =
                parse_value(&c.value).filter(|v| matches!(v.unit.as_deref(), None | Some("Ω")));
            let (reference, value, up) =
                (c.reference.clone(), c.value.clone(), !is_ground_name(name));
            pulls.push(Pull {
                reference,
                value,
                far,
                up,
                ohms: ohms.map(|v| v.si),
                tol: super::stated_tolerance(c).unwrap_or(0.0),
            });
        }
    }
    pulls
}

fn describe(pulls: &[Pull]) -> String {
    let side = |p: &Pull| if p.up { "up" } else { "down" };
    pulls
        .iter()
        .map(|p| format!("{} {} {}", p.reference, p.value, side(p)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Judge a pull divider on a boot-select strap: the pin voltage from the
/// pulls' conductances, at every combination of each rail's solved level and
/// declared corners and at both resistor-tolerance extremes, against the
/// card's sourced VIL/VIH at that corner's VDD. `Ok(None)` when every corner
/// reads the required level; `Err` names what is missing to judge it at all.
fn judge_divider(
    board: &ExtractedBoard,
    rails: &RailResolver<'_>,
    (mcu, model): (&Component, &ModelEntry),
    strap: &StrapPin,
    pulls: &[Pull],
) -> Result<Option<(Severity, String)>, String> {
    if let Some(p) = pulls.iter().find(|p| p.ohms.is_none()) {
        return Err(format!(
            "{} value '{}' is not a resistance",
            p.reference, p.value
        ));
    }
    let (Some([lo_k, lo_v]), Some([hi_k, hi_v])) = (strap.vil, strap.vih) else {
        return Err(format!(
            "the '{}' card carries no sourced VIL/VIH for {}",
            model.id, strap.role
        ));
    };
    let supply = strap.supply_role(model);
    let vdd = role_net_id(mcu, model, supply).ok_or_else(|| {
        format!(
            "the '{}' card names no {supply} pin to scale VIL/VIH",
            model.id
        )
    })?;
    // The rails in play (the vdd net shares a corner with a pull to it).
    let mut nets: Vec<(i64, Vec<f64>)> = Vec::new();
    for id in pulls.iter().filter(|p| p.up).map(|p| p.far).chain([vdd]) {
        let name = board.net(id).map_or("?", |n| n.name.as_str());
        let corners = rails.corners(id);
        if corners.is_empty() {
            return Err(format!("rail '{name}' has no known voltage"));
        }
        if !nets.iter().any(|(n, _)| *n == id) {
            nets.push((id, corners));
        }
    }
    let combos: usize = nets.iter().map(|(_, c)| c.len()).product();
    let widen = |(a, b): (f64, f64), v: f64| (a.min(v), b.max(v));
    let empty = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut pin, mut il, mut ih) = (empty, empty, empty);
    let mut reads = [false; 3]; // low, undefined, high
    for mut k in 0..combos {
        let mut level = BTreeMap::new();
        for (id, corners) in &nets {
            level.insert(*id, corners[k % corners.len()]);
            k /= corners.len();
        }
        let (vil, vih) = (
            lo_k.mul_add(level[&vdd], lo_v),
            hi_k.mul_add(level[&vdd], hi_v),
        );
        (il, ih) = (widen(il, vil), widen(ih, vih));
        for skew in [-1.0, 1.0] {
            // A 0 Ohm part is a short: a micro-ohm pins the net to its far side.
            let (mut g, mut i) = (0.0, 0.0);
            for p in pulls {
                let sign = if p.up { -1.0 } else { 1.0 };
                let r = (p.ohms.unwrap_or_default() * (skew * p.tol).mul_add(sign, 1.0)).max(1e-6);
                g += 1.0 / r;
                i += level.get(&p.far).copied().unwrap_or(0.0) / r;
            }
            let v = i / g;
            pin = widen(pin, v);
            reads[if v <= vil {
                0
            } else if v >= vih {
                2
            } else {
                1
            }] = true;
        }
    }
    let want_high = strap.level == StrapLevel::High;
    let (verdict, severity) = match reads {
        [true, false, false] if !want_high => return Ok(None),
        [false, false, true] if want_high => return Ok(None),
        [true, false, false] => ("LOW at every corner", Severity::Medium),
        [false, false, true] => ("HIGH at every corner", Severity::Medium),
        [false, true, false] => ("an undefined level (between VIL and VIH)", Severity::High),
        _ => ("an undefined, corner-dependent level", Severity::High),
    };
    let span = |pair: (f64, f64)| {
        if (pair.1 - pair.0).abs() < 5e-3 {
            format_volts(pair.0)
        } else {
            format_series(&<[f64; 2]>::from(pair), "-", "V")
        }
    };
    Ok(Some((severity, format!(
        "is a pull divider ({}) at {} across {combos} rail corner(s) and the resistor tolerances, against VIL <= {} and VIH >= {}: it reads {verdict}, but normal boot needs {}",
        describe(pulls), span(pin), span(il), span(ih), strap.level.as_str()
    ))))
}

// ── Small net-name helpers (local copies; the extract versions are private) ──

// Is the far side of this pull or hop a rail? The broad form shared with the
// boot check (`netclass::may_be_supply`): an exact-voltage table misses VBAT,
// VSYS and VMOT, which lets the clock-reach hop wander onto a rail and hides a
// genuine wrong-direction pull.
use hauksbee_ir::units::{format_series, format_volts};
use hauksbee_models::netclass::may_be_supply as is_rail_name;

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;
    use Severity::{High, Low, Medium};

    /// A test MCU whose active-low boot strap carries absolute thresholds
    /// (VIL 0.9 V, VIH 2.0 V), so divider corners are easy to place by hand.
    const STRAP_CARD: &str = r#"
[[models]]
id = "strap_test_mcu"
kind = "mcu"
description = "divider fixture"
[models.match]
value_re = "^STRAPTEST$"
[models.pins]
"1" = "vdd"
"2" = "boot0"
[[models.straps]]
role = "boot0"
level = "low"
boot_select = true
internal_pull = "none"
vil = [0.0, 0.9]
vih = [0.0, 2.0]
"#;

    fn library() -> ModelLibrary {
        let dir = hauksbee_testkit::fresh_scratch_dir("straps", "card");
        std::fs::write(dir.join("models.toml"), STRAP_CARD).unwrap();
        let mut lib = ModelLibrary::builtin();
        assert!(lib.load_user_dir(&dir).is_empty(), "test card must load");
        lib
    }

    /// Nets: 1 GND, 2 the rail, 3 the strap, 4/7 oscillator outputs, 6 enable.
    fn strap_board(rail: &str, strap: &str, parts: Vec<Component>) -> ExtractedBoard {
        let nets = [
            (1, "GND"),
            (2, rail),
            (3, strap),
            (4, "Net-(CR1-OUT)"),
            (6, "/OSC_EN"),
            (7, "Net-(CR2-OUT)"),
        ];
        board(&nets, parts)
    }
    /// A pull from the strap (net 3) to `far` (1 = GND, 2 = the rail).
    fn pull(reference: &str, value: &str, far: i64) -> Component {
        part(reference, value, &[("1", 3), ("2", far)])
    }
    fn osc(reference: &str, out: i64) -> Component {
        part(
            reference,
            "Q50MHz/25ppm/3V/4P",
            &[("1", 6), ("2", 1), ("3", out), ("4", 2)],
        )
    }
    fn esp32(strap_pad: &str) -> Component {
        part("U3", "ESP-WROOM-32", &[("2", 2), (strap_pad, 3)])
    }
    fn stm32() -> Component {
        part("U1", "STM32F103C8T6", &[("9", 2), ("8", 1), ("44", 3)])
    }
    fn test_mcu() -> Component {
        part("U9", "STRAPTEST", &[("1", 2), ("2", 3)])
    }

    fn check(
        name: &str,
        b: &ExtractedBoard,
        drives: &BTreeMap<String, RailRange>,
        want: &[(Severity, &[&str])],
    ) {
        let report = strap_lint_with_drives(b, &library(), drives);
        let got: Vec<_> = report.of_check(LintCheck::StrapPin).collect();
        assert_eq!(got.len(), want.len(), "{name}: {got:#?}");
        for (finding, (severity, needles)) in got.iter().zip(want) {
            assert_eq!(finding.severity, *severity, "{name}: {}", finding.message);
            for needle in *needles {
                assert!(
                    finding.message.contains(needle),
                    "{name}: '{needle}' not in {}",
                    finding.message
                );
            }
        }
    }

    // (name, board, expected (severity, message fragments) findings)
    type Case<'a> = (&'a str, ExtractedBoard, &'a [(Severity, &'a [&'a str])]);

    #[test]
    #[allow(clippy::too_many_lines)] // one flat table of strap cases reads best in one place
    fn strap_cases() {
        let none = BTreeMap::new();
        let with_unrelated_osc = |rail| {
            strap_board(
                rail,
                "/GPIO0",
                vec![esp32("25"), pull("R36", "10k", 2), osc("CR2", 7)],
            )
        };
        let jumper = board(
            &[(1, "GND"), (2, "+3V3"), (3, "/BOOT0"), (4, "/BOOT0_SEL")],
            vec![
                part("U1", "STM32F401CCU6", &[("44", 3)]),
                pull("R1", "1k", 4),
                part("J1", "BOOT_SEL", &[("1", 2), ("2", 4), ("3", 1)]),
            ],
        );
        let cases: Vec<Case> = vec![
            // The Olimex fault: a 50 MHz oscillator through a 10R onto GPIO0.
            (
                "clock on GPIO0",
                strap_board(
                    "+3V3",
                    "/GPIO0",
                    vec![
                        esp32("25"),
                        osc("CR1", 4),
                        part("R36", "10R", &[("1", 4), ("2", 3)]),
                    ],
                ),
                &[(High, &["gpio0", "CR1", "GPIO0"])],
            ),
            (
                "pulled GPIO0",
                strap_board("+3V3", "/GPIO0", vec![esp32("25"), pull("R36", "10k", 2)]),
                &[],
            ),
            // Sharing only the rail with an oscillator's VDD is not a clock path,
            // whether the rail is named +3V3 or VBAT (bug #19).
            ("osc shares +3V3", with_unrelated_osc("+3V3"), &[]),
            ("osc shares VBAT", with_unrelated_osc("VBAT"), &[]),
            (
                "BOOT0 pulled up",
                strap_board("+3V3", "/BOOT0", vec![stm32(), pull("R1", "10k", 2)]),
                &[(Medium, &["boot0", "wrong level"])],
            ),
            (
                "BOOT0 pulled to VBAT (bug #20)",
                strap_board("VBAT", "/BOOT0", vec![stm32(), pull("R1", "10k", 2)]),
                &[(Medium, &["boot0", "wrong level"])],
            ),
            (
                "BOOT0 pulled down",
                strap_board("+3V3", "/BOOT0", vec![stm32(), pull("R1", "10k", 1)]),
                &[],
            ),
            // No internal pull to fight: a weak wrong-way pull alone sets BOOT0.
            (
                "BOOT0 weakly pulled up",
                strap_board("+3V3", "/BOOT0", vec![stm32(), pull("R1", "100k", 2)]),
                &[(
                    Medium,
                    &[
                        "boot0",
                        "wrong level",
                        "no internal pull",
                        "Move R1 to ground",
                    ],
                )],
            ),
            // A bootloader-button circuit: the 1M pull-up is the idle bias and a
            // transistor behind a series resistor pulls BOOT0 low for normal boot.
            (
                "BOOT0 pull-up with an on-board pull-down switch",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![
                        stm32(),
                        pull("R35", "1M", 2),
                        part("R38", "2.2k", &[("1", 3), ("2", 4)]),
                        part("Q8", "BSS138", &[("1", 6), ("2", 4), ("3", 1)]),
                    ],
                ),
                &[],
            ),
            (
                "BOOT0 pull-up with the switch not fitted",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![
                        stm32(),
                        pull("R35", "1M", 2),
                        part("R38", "2.2k", &[("1", 3), ("2", 4)]),
                        Component {
                            dnp: true,
                            ..part("Q8", "BSS138", &[("1", 6), ("2", 4), ("3", 1)])
                        },
                    ],
                ),
                &[(Medium, &["boot0", "wrong level", "R35"])],
            ),
            (
                "BOOT0 weakly pulled down",
                strap_board("+3V3", "/BOOT0", vec![stm32(), pull("R1", "100k", 1)]),
                &[],
            ),
            // GPIO0's internal pull-up beats a weak pull-down: not judged wrong.
            (
                "weak pull-down on GPIO0",
                strap_board("+3V3", "/GPIO0", vec![esp32("25"), pull("R36", "100k", 1)]),
                &[],
            ),
            // STM32 BOOT0 has no internal pull: floating is undefined at reset,
            // whether KiCad names the net unconnected-* or it is merely isolated.
            (
                "floating BOOT0",
                strap_board(
                    "+3V3",
                    "unconnected-(U1-Pad44)",
                    vec![part("U1", "STM32F401CCU6", &[("44", 3)])],
                ),
                &[(High, &["floating", "Pad44"])],
            ),
            (
                "isolated BOOT0",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![part("U1", "STM32F401CCU6", &[("44", 3)])],
                ),
                &[(High, &["floating"])],
            ),
            ("jumper-selected BOOT0", jumper, &[]),
            // ESP32 straps have an internal pull, so an undriven GPIO0 is defined.
            (
                "floating GPIO0",
                strap_board("+3V3", "unconnected-(U3-Pad25)", vec![esp32("25")]),
                &[],
            ),
            // GPIO15 is not boot_select: a board may repurpose it with a pull.
            (
                "GPIO15 pulled down",
                strap_board(
                    "+3V3",
                    "/GPIO15_LED",
                    vec![esp32("23"), pull("R1", "10k", 1)],
                ),
                &[],
            ),
            (
                "AVR has no straps",
                strap_board(
                    "+5V",
                    "PC6_RESET",
                    vec![part("U1", "ATmega328P", &[("1", 3), ("7", 2), ("8", 1)])],
                ),
                &[],
            ),
            // Dividers. A strong correct pull against a weak opposing one: GPIO0
            // sits at 2.85-3.15 V over the +3V3 corners, above 0.75 x VDD.
            (
                "GPIO0 1k up / 10k down",
                strap_board(
                    "+3V3",
                    "/GPIO0",
                    vec![esp32("25"), pull("R1", "1k", 2), pull("R2", "10k", 1)],
                ),
                &[],
            ),
            (
                "active-low 1k down / 10k up",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![test_mcu(), pull("R1", "10k", 2), pull("R2", "1k", 1)],
                ),
                &[],
            ),
            // The audit case: 1k up and 10k down on an active-low strap reads HIGH.
            (
                "active-low 1k up / 10k down",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![test_mcu(), pull("R1", "1k", 2), pull("R2", "10k", 1)],
                ),
                &[(
                    Medium,
                    &["2.85-3.15 V", "reads HIGH at every corner", "needs low"],
                )],
            ),
            (
                "mid-band 10k / 10k",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![test_mcu(), pull("R1", "10k", 2), pull("R2", "10k", 1)],
                ),
                &[(
                    High,
                    &["1.567-1.732 V", "VIL <= 900 mV", "VIH >= 2 V", "undefined"],
                )],
            ),
            // 27k/10k crosses VIL inside the +3V3 envelope: 0.85 V at 3.135 V but
            // 0.94 V at 3.465 V, so the level depends on the corner.
            (
                "corner-dependent 27k / 10k",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![test_mcu(), pull("R1", "27k 1%", 2), pull("R2", "10k 1%", 1)],
                ),
                &[(High, &["corner-dependent"])],
            ),
            (
                "unreadable divider value",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![test_mcu(), pull("R1", "10k", 2), pull("R2", "XYZ", 1)],
                ),
                &[(Low, &["cannot judge divider", "R2 value 'XYZ'"])],
            ),
            // A net held only by parts that set a level some of the time may float.
            (
                "BOOT0 on a button to the rail only",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![test_mcu(), part("SW1", "SW_Push", &[("1", 3), ("2", 2)])],
                ),
                &[(
                    High,
                    &["may float", "SW1 pin 1", "pull-down to GND on '/BOOT0'"],
                )],
            ),
            (
                "BOOT0 on a header pin only",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![test_mcu(), part("J2", "Conn_01x01", &[("1", 3)])],
                ),
                &[(High, &["may float", "J2 pin 1", "nothing is plugged in"])],
            ),
            // The driver is an output once firmware runs (this pad is the
            // card's PWM output; an untyped layout pad reads the same), so the
            // net has a level: only a note about the reset window.
            (
                "BOOT0 driven by another MCU's output pad",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![test_mcu(), part("U5", "ATmega328P", &[("5", 3)])],
                ),
                &[(
                    Low,
                    &[
                        "driven by U5 pin 5",
                        "firmware runs",
                        "add a 10 kOhm pull-down",
                    ],
                )],
            ),
            // A pad the symbol types as an input only listens: it drives nothing.
            (
                "BOOT0 on another MCU's input-only pad",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![test_mcu(), {
                        let mut u = part("U5", "ATmega328P", &[("4", 3)]);
                        u.pins[0].kind = "input".into();
                        u
                    }],
                ),
                &[(High, &["may float", "U5 pin 4", "high-impedance"])],
            ),
            (
                "BOOT0 button with its pull-down",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![
                        test_mcu(),
                        part("SW1", "SW_Push", &[("1", 3), ("2", 2)]),
                        pull("R1", "10k", 1),
                    ],
                ),
                &[],
            ),
            (
                "BOOT0 on a capacitor and a test point only",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![
                        test_mcu(),
                        part("C1", "100n", &[("1", 3), ("2", 1)]),
                        part("TP1", "TestPoint", &[("1", 3)]),
                    ],
                ),
                &[(High, &["floating"])],
            ),
            // STM32F103's card has no sourced BOOT0 thresholds: abstain, visibly.
            (
                "no thresholds",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![stm32(), pull("R1", "1k", 2), pull("R2", "10k", 1)],
                ),
                &[(Low, &["cannot judge divider", "no sourced VIL/VIH"])],
            ),
        ];
        for (name, b, want) in &cases {
            check(name, b, &none, want);
        }
        // A solved span joins the declared corners: +3V3 sagging to 2.6 V in the
        // run pulls the 1k/10k active-low divider down to 2.36 V, still HIGH.
        let solved = BTreeMap::from([(
            "+3V3".to_string(),
            RailRange {
                min_v: 2.6,
                max_v: 3.3,
            },
        )]);
        let faulty = strap_board(
            "+3V3",
            "/BOOT0",
            vec![test_mcu(), pull("R1", "1k", 2), pull("R2", "10k", 1)],
        );
        check(
            "solved rail",
            &faulty,
            &solved,
            &[(Medium, &["2.364-3.15 V", "4 rail corner(s)"])],
        );
    }

    /// The Olimex rev-L fix: the clock's enable is held low by a detector whose
    /// card holds its output low from power-up, and the detector senses the
    /// ESP32's EN through an RC. Whether that lasts past the strap hold time is
    /// timing, so the lint abstains (Low, unchecked); it never calls it clean.
    /// A diode that can only discharge the detector's input (its anode there,
    /// through a resistor, as rev L's BAT54A does) keeps it behind EN. The
    /// faulty controls keep the High: a detector that senses the rail releases
    /// with the supply, not after EN; a diode that charges the input from the
    /// rail bypasses the delay; and a plain pull-up gates nothing.
    #[test]
    fn a_clock_held_off_through_reset_is_unchecked_not_clean() {
        let none = BTreeMap::new();
        // Nets: 1 GND, 2 rail, 3 strap, 4 clock out, 6 enable, 8 EN, 9 detector
        // input, 10 the far side of a resistor off the detector input.
        let gated_with = |sense: i64, detector: bool, extra: Vec<Component>| {
            let mut parts = vec![
                part("U3", "ESP-WROOM-32", &[("2", 2), ("25", 3), ("3", 8)]),
                osc("CR1", 4),
                part("R36", "10R", &[("1", 4), ("2", 3)]),
                part("R34", "10k", &[("1", 6), ("2", 2)]),
                part("R24", "10k", &[("1", 8), ("2", 2)]),
                part("R52", "100k", &[("1", 9), ("2", 8)]),
                part("C32", "1uF", &[("1", 9), ("2", 1)]),
            ];
            if detector {
                parts.push(part("U11", "HX7027", &[("1", 6), ("2", 1), ("3", sense)]));
            }
            parts.extend(extra);
            board(
                &[
                    (1, "GND"),
                    (2, "+3V3"),
                    (3, "/GPIO0"),
                    (4, "Net-(CR1-OUT)"),
                    (6, "/OSC_DIS"),
                    (8, "/ESP_EN"),
                    (9, "Net-(U11-VIn)"),
                    (10, "Net-(D10-CA)"),
                ],
                parts,
            )
        };
        let gated = |sense: i64, detector: bool| gated_with(sense, detector, vec![]);
        // BAT54A: pads 1 and 2 cathodes, pad 3 the common anode.
        let discharge = || {
            vec![
                part("D10", "BAT54A", &[("1", 2), ("2", 8), ("3", 10)]),
                part("R58", "49.9R", &[("1", 10), ("2", 9)]),
            ]
        };
        let charge_from_rail = || {
            vec![
                part("D10", "BAT54A", &[("1", 10), ("2", 10), ("3", 2)]),
                part("R58", "49.9R", &[("1", 10), ("2", 9)]),
            ]
        };
        check(
            "detector behind EN",
            &gated(9, true),
            &none,
            &[(Low, &["CR1", "U11", "/OSC_DIS", "unchecked"])],
        );
        check(
            "detector on EN itself",
            &gated(8, true),
            &none,
            &[(Low, &["CR1", "U11", "unchecked"])],
        );
        check(
            "detector behind EN with a diode discharge path",
            &gated_with(9, true, discharge()),
            &none,
            &[(Low, &["CR1", "U11", "unchecked"])],
        );
        check(
            "detector input charged from the rail through a diode",
            &gated_with(9, true, charge_from_rail()),
            &none,
            &[(High, &["CR1", "free-running clock"])],
        );
        check(
            "detector on the rail",
            &gated(2, true),
            &none,
            &[(High, &["CR1", "free-running clock"])],
        );
        check(
            "pull-up only",
            &gated(9, false),
            &none,
            &[(High, &["CR1", "free-running clock"])],
        );
    }

    /// The RP2040 BOOTSEL strap (`QSPI_SS`, needs high; RP2040 datasheet
    /// 3184e62-clean sec 2.8.1.2 p.130 "Check if SPI CS pin is tied low ...
    /// skip flash boot") shares its net with the flash's chip-select input and a
    /// BOOTSEL button that pulls it to ground through 1k. Neither can hold the
    /// strap high, so a pull-down there still fires; a pull-up stays silent; a
    /// part that can drive the net still makes the lint abstain.
    #[test]
    fn qspi_ss_wrong_way_pull_is_not_excused_by_flash_cs_or_bootsel_button() {
        // Nets: 1 GND, 2 +3V3, 3 QSPI_SS, 4 the button side of the series 1k.
        let run = |pull_to: i64, flash: Component, extra: Vec<Component>| {
            let mut parts = vec![
                part("U3", "RP2040", &[("56", 3)]),
                flash,
                pull("R2", "10k", pull_to),
                part("R1", "1k", &[("1", 3), ("2", 4)]),
                part("SW1", "BOOTSEL", &[("1", 4), ("2", 4), ("3", 1), ("4", 1)]),
            ];
            parts.extend(extra);
            let b = board(
                &[(1, "GND"), (2, "+3V3"), (3, "/QSPI_SS"), (4, "/USB_BOOT")],
                parts,
            );
            strap_lint(&b, &library())
                .of_check(LintCheck::StrapPin)
                .map(|f| (f.severity, f.message.clone()))
                .collect::<Vec<_>>()
        };
        // A carded flash: its pad 1 is the card's SPI NOR `cs` role.
        let carded = || part("U2", "W25Q128JVSIQ", &[("1", 3), ("4", 1), ("8", 2)]);
        // An uncarded flash whose schematic names the pin SSEL (no pin type).
        let named = || {
            let mut u = part("U1", "8MB QSPI", &[("1", 3), ("4", 1), ("8", 2)]);
            u.pins[0].function = "SSEL".into();
            u
        };
        for (what, flash) in [("carded", carded()), ("named", named())] {
            let got = run(1, flash, vec![]);
            assert_eq!(got.len(), 1, "{what}: {got:#?}");
            assert_eq!(got[0].0, Medium, "{what}");
            for needle in ["qspi_ss", "R2", "needs high", "wrong level"] {
                assert!(
                    got[0].1.contains(needle),
                    "{what}: '{needle}' in {}",
                    got[0].1
                );
            }
        }
        // Legal control: the 10k pull-up holds QSPI_SS high for flash boot.
        assert!(run(2, carded(), vec![]).is_empty());
        assert!(run(2, named(), vec![]).is_empty());
        // A part that can drive the net (an IC pin of unknown direction, here an
        // inverter output) may hold the strap high itself: abstain.
        let driver = part("U5", "74HC04", &[("2", 3), ("7", 1), ("14", 2)]);
        assert!(run(1, carded(), vec![driver]).is_empty());
        // A button to the rail can lift the strap to its normal level: abstain.
        let lift = part("SW2", "B3U-1000P", &[("1", 3), ("2", 2)]);
        assert!(run(1, carded(), vec![lift]).is_empty());
    }

    /// A clean strap check says what it examined, and a strap it could not
    /// locate says so, instead of both reading as silence (an ESP32-S3 module
    /// with GPIO0 pulled up, GPIO46 on a signal net, and no GPIO45 pad).
    #[test]
    fn strap_coverage_names_checked_unjudged_and_unplaced_straps() {
        let mut module = part(
            "U3",
            "ESP32-S3-WROOM-1-N8R8",
            &[("2", 2), ("27", 3), ("16", 6)],
        );
        module.footprint = "RF_Module:ESP32-S3-WROOM-1".into();
        let parts = vec![
            module,
            pull("R1", "10k", 2),
            part("U4", "74HC04", &[("1", 6)]),
        ];
        let b = strap_board("+3V3", "/BOOT", parts);
        let lib = library();
        assert_eq!(
            strap_lint(&b, &lib).of_check(LintCheck::StrapPin).count(),
            0
        );
        let lines = strap_coverage(&b, &lib);
        assert_eq!(lines.len(), 1, "{lines:#?}");
        for needle in [
            "strap pins on U3 (esp32s3_wroom)",
            "gpio0 (pad 27, net '/BOOT'): clock source and pull bias checked",
            "gpio46 (pad 16, net '/OSC_EN'): clock source checked, bias level not judged",
            "gpio45 NOT checked (its pad is not on this footprint)",
        ] {
            assert!(lines[0].contains(needle), "{needle}: {}", lines[0]);
        }
        // An unrecognised package keeps no strap table: no coverage line, and
        // the unchecked-MCU note says the straps were not checked.
        let bare = strap_board("+3V3", "/BOOT", vec![part("U3", "ESP32-S3", &[("2", 2)])]);
        assert!(strap_coverage(&bare, &lib).is_empty());
        let note = super::super::mcu_coverage::mcu_coverage_lint(&bare, &lib);
        assert!(note
            .findings
            .iter()
            .any(|f| f.message.contains("were NOT checked")));
    }

    /// The ESP32 chip enable has no internal pull (datasheet: do not leave it
    /// floating). An MCU pad that is high impedance at reset is not a pull;
    /// a button or header alone is the model-control lint's finding, not this one.
    #[test]
    fn an_enable_held_only_by_another_mcu_pad_may_float_but_a_pull_up_holds_it() {
        let none = BTreeMap::new();
        let module = || {
            let mut c = part("U3", "ESP32-S3-WROOM-1-N8R8", &[("2", 2), ("3", 3)]);
            c.footprint = "RF_Module:ESP32-S3-WROOM-1".into();
            c
        };
        let run = |extra: Vec<Component>| {
            let mut parts = vec![module()];
            parts.extend(extra);
            strap_lint_with_drives(&strap_board("+3V3", "/EN", parts), &library(), &none)
        };
        // An enable some MCU output drives has a level once firmware runs, and
        // an enable is not a latch sampled at power-up: silent, not a fault.
        let driven = run(vec![part("U5", "ATmega328P", &[("5", 3)])]);
        assert_eq!(
            driven.of_check(LintCheck::StrapPin).count(),
            0,
            "{driven:#?}"
        );
        // An MCU pad the symbol types as an input only listens.
        let mut listener = part("U5", "ATmega328P", &[("4", 3)]);
        listener.pins[0].kind = "input".into();
        let gpio = run(vec![listener]);
        let found: Vec<_> = gpio.of_check(LintCheck::StrapPin).collect();
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, High);
        for needle in ["may float", "U5 pin 4", "pull-up to '+3V3'"] {
            assert!(
                found[0].message.contains(needle),
                "{needle}: {}",
                found[0].message
            );
        }
        // A pull-up holds it; a button alone is left to the model-control lint.
        assert_eq!(
            run(vec![pull("R1", "10k", 2)])
                .of_check(LintCheck::StrapPin)
                .count(),
            0
        );
        let button = part("SW1", "SW_Push", &[("1", 3), ("2", 1)]);
        assert_eq!(run(vec![button]).of_check(LintCheck::StrapPin).count(), 0);
    }

    /// A strap net an MCU output drives has a level once that firmware runs:
    /// never a Medium or High fault, at most a Low note about the reset window,
    /// and only for a boot-select latch. The same net with nothing driving it is
    /// still the fault.
    #[test]
    fn a_strap_net_an_mcu_output_drives_is_a_note_never_a_fault() {
        let none = BTreeMap::new();
        let driver = |kind: &str, extra: Vec<Component>| {
            let mut u = part("U5", "ATmega328P", &[("4", 3)]);
            u.pins[0].kind = kind.into();
            let mut parts = vec![test_mcu(), u];
            parts.extend(extra);
            strap_board("+3V3", "/BOOT0", parts)
        };
        let worst = |b: &ExtractedBoard| {
            strap_lint_with_drives(b, &library(), &none)
                .of_check(LintCheck::StrapPin)
                .map(|f| f.severity)
                .min_by_key(|s| match s {
                    High => 0,
                    Medium => 1,
                    Low => 2,
                })
        };
        // Output-like pads: a note, whatever else shares the net.
        for kind in ["output", "bidirectional", "tri_state", ""] {
            assert_eq!(worst(&driver(kind, vec![])), Some(Low), "{kind}");
        }
        let header = part("J2", "Conn_01x01", &[("1", 3)]);
        assert_eq!(worst(&driver("bidirectional", vec![header])), Some(Low));
        // A pull resistor holds the level: silent.
        let pulled = driver("bidirectional", vec![pull("R1", "10k", 1)]);
        assert_eq!(worst(&pulled), None);
        // Nothing drives it: the fault.
        let bare = strap_board("+3V3", "/BOOT0", vec![test_mcu()]);
        assert_eq!(worst(&bare), Some(High));
    }

    /// A mode-qualifying strap (ESP32-C3 GPIO8: any level in SPI boot, high for
    /// Joint Download Boot, ESP32-C3 Series Datasheet v2.4 Tables 3-1 and 3-3)
    /// left floating is a fault only when the board can select that mode: a
    /// BOOT button or a pull-down on GPIO9 can, a bare pull-up and a debounce
    /// capacitor cannot.
    #[test]
    fn mode_qualified_strap_float_needs_a_drivable_selector() {
        // Nets: 1 GND, 2 +3V3, 3 GPIO8, 5 GPIO9.
        let chip = || {
            let mut c = part(
                "U1",
                "ESP32-C3FH4",
                &[("17", 2), ("33", 1), ("14", 3), ("15", 5)],
            );
            c.footprint = "Package_DFN_QFN:QFN-32-1EP_5x5mm_P0.5mm_EP3.7x3.7mm".into();
            c
        };
        let on = |reference: &str, value: &str, a: i64, b: i64| {
            part(reference, value, &[("1", a), ("2", b)])
        };
        let run = |gpio8: &str, extra: Vec<Component>| {
            let mut parts = vec![chip()];
            parts.extend(extra);
            let b = board(&[(1, "GND"), (2, "+3V3"), (3, gpio8), (5, "/BOOT")], parts);
            strap_lint(&b, &library())
                .of_check(LintCheck::StrapPin)
                .map(|f| (f.severity, f.message.clone()))
                .collect::<Vec<_>>()
        };
        let float = "unconnected-(U1-GPIO8-Pad14)";
        let button = || on("SW1", "B3U-1000P", 5, 1);
        let up9 = || on("R6", "10k", 5, 2);
        // Faulty: GPIO8 floats and a BOOT button can pull GPIO9 low.
        let got = run(float, vec![up9(), button()]);
        assert_eq!(got.len(), 1, "{got:#?}");
        assert_eq!(got[0].0, Medium);
        for needle in [
            "U1 strap pin 14 (gpio8)",
            "floating",
            "gpio9",
            "Joint Download Boot",
        ] {
            assert!(got[0].1.contains(needle), "'{needle}' not in {}", got[0].1);
        }
        // A pull-down on GPIO9 selects download mode outright: GPIO8's float is
        // still a fault (beside GPIO9's own wrong-level finding).
        let down = run(float, vec![on("R6", "10k", 5, 1)]);
        assert_eq!(
            down.iter().filter(|(_, m)| m.contains("(gpio8)")).count(),
            1,
            "{down:#?}"
        );
        // Fixed: a pull-up holds GPIO8 high.
        let fixed = run("/GPIO8", vec![up9(), button(), on("R23", "10k", 3, 2)]);
        assert!(fixed.is_empty(), "{fixed:#?}");
        // Legal controls: GPIO9 only pulled up (and debounced), or left alone,
        // so download mode is unreachable and GPIO8's level never matters.
        let held = run(float, vec![up9(), on("C9", "100n", 5, 1)]);
        assert!(held.is_empty(), "{held:#?}");
        assert!(run(float, vec![]).is_empty());
    }

    /// A part with a 1.2 V core (`vdd`) and a 3.3 V IO supply (`iovdd`): the
    /// strap pad is powered from the IO supply, so its thresholds scale there.
    fn io_domain_card(strap_lines: &str) -> String {
        format!(
            r#"
[[models]]
id = "strap_io_mcu"
kind = "mcu"
description = "IO-domain strap fixture"
[models.match]
value_re = "^STRAPIO$"
[models.pins]
"1" = "vdd"
"2" = "iovdd"
"3" = "boot0"
[[models.straps]]
role = "boot0"
level = "low"
boot_select = true
internal_pull = "none"
vil = [0.3, 0.0]
vih = [0.7, 0.0]
{strap_lines}
"#
        )
    }

    fn io_lint(card: &str, parts: Vec<Component>) -> Vec<(Severity, String)> {
        let dir = hauksbee_testkit::fresh_scratch_dir("straps", "io");
        std::fs::write(dir.join("models.toml"), card).unwrap();
        let mut lib = ModelLibrary::builtin();
        assert!(lib.load_user_dir(&dir).is_empty(), "test card must load");
        let nets = [(1, "GND"), (2, "+3V3"), (3, "/BOOT0"), (5, "+1V2")];
        let b = board(&nets, parts);
        let report = strap_lint_with_drives(&b, &lib, &BTreeMap::new());
        report
            .of_check(LintCheck::StrapPin)
            .map(|f| (f.severity, f.message.clone()))
            .collect()
    }

    fn io_mcu() -> Component {
        part("U9", "STRAPIO", &[("1", 5), ("2", 2), ("3", 3)])
    }

    /// The pad's thresholds are read at the IO rail the card names, not the
    /// core rail: a 10k/10k divider at 3.3 V is mid-band for 0.3/0.7 x 3.3 V
    /// (undefined), where the core 1.2 V rail would call it a clean high.
    #[test]
    fn a_strap_on_the_io_domain_is_judged_at_the_io_rail() {
        let divider = || vec![io_mcu(), pull("R1", "10k", 2), pull("R2", "10k", 1)];
        for card in [
            io_domain_card("supply = \"iovdd\""),
            io_domain_card("").replace(
                "[[models.straps]]",
                "[models.level_rails]\niovdd = [\"boot0\"]\n[[models.straps]]",
            ),
        ] {
            let found = io_lint(&card, divider());
            assert_eq!(found.len(), 1, "{found:#?}");
            assert_eq!(found[0].0, High, "{found:#?}");
            assert!(
                found[0].1.contains("undefined") && found[0].1.contains("VIH >= 2."),
                "{}",
                found[0].1
            );
        }
        // Left on the core rail (the card says nothing about the pad's domain),
        // the same divider reads a clean HIGH against 0.7 x 1.2 V: the very
        // error a drafted card makes by mapping the pad to the core domain.
        let core = io_lint(&io_domain_card(""), divider());
        assert_eq!(core.len(), 1, "{core:#?}");
        assert!(
            core[0].1.contains("reads HIGH at every corner"),
            "{core:#?}"
        );
    }

    /// Correct IO-domain boards stay silent, and the fix names the IO rail.
    #[test]
    fn a_correct_io_domain_strap_board_is_silent() {
        let card = io_domain_card("supply = \"iovdd\"");
        // Active-low strap: 1k down against 10k up reads 0.3 V (<= 0.99 V).
        let parts = vec![io_mcu(), pull("R1", "10k", 2), pull("R2", "1k", 1)];
        assert!(io_lint(&card, parts).is_empty());
        let plain = vec![io_mcu(), pull("R1", "10k", 1)];
        assert!(io_lint(&card, plain).is_empty());
        // A floating pad names the IO rail in its fix, not the core rail.
        let float = io_lint(
            &card.replace("level = \"low\"", "level = \"high\""),
            vec![io_mcu()],
        );
        assert!(float.iter().all(|(_, m)| !m.contains("+1V2")), "{float:#?}");
    }

    /// STM32F103 BOOT0 (no internal pull) with a two-pad jumper to the supply
    /// as its only holder floats whenever the jumper is open, and the finding
    /// says what the part does then. A pull-down beside the jumper holds the
    /// open position low, and a pre-bridged solder jumper cannot be opened.
    #[test]
    fn a_boot_pin_held_only_by_an_openable_jumper_floats_in_the_open_position() {
        let jumper = |reference: &str, footprint: &str| {
            let mut c = part(reference, "BOOT0_HI", &[("1", 3), ("2", 2)]);
            c.footprint = footprint.into();
            c
        };
        let header = "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical";
        let run = |extra: Vec<Component>| {
            let mut parts = vec![stm32()];
            parts.extend(extra);
            let b = strap_board("+3V3", "/BOOT0", parts);
            strap_lint(&b, &library())
                .of_check(LintCheck::StrapPin)
                .map(|f| (f.severity, f.message.clone()))
                .collect::<Vec<_>>()
        };
        let got = run(vec![jumper("JP1", header)]);
        assert_eq!(got.len(), 1, "{got:#?}");
        assert_eq!(got[0].0, High, "{got:#?}");
        for needle in [
            "U1 strap pin 44 (boot0) may float",
            "JP1 pin 1",
            "the jumper is open",
            "With the jumper open: BOOT0 with the jumper open, floating",
            "boot mode is undefined",
            "10 kOhm pull-down",
        ] {
            assert!(got[0].1.contains(needle), "'{needle}' not in {}", got[0].1);
        }
        // Fixed: a pull-down holds the open position low (normal boot).
        let fixed = run(vec![jumper("JP1", header), pull("R1", "10k", 1)]);
        assert!(fixed.is_empty(), "{fixed:#?}");
        // A solder jumper that ships bridged is a closed link, not a choice.
        let bridged = "Jumper:SolderJumper-2_P1.3mm_Bridged_RoundedPad1.0x1.5mm";
        assert!(run(vec![jumper("SJ1", bridged)]).is_empty());
        // The same jumper, left open as shipped, is open-able.
        let open = "Jumper:SolderJumper-2_P1.3mm_Open_RoundedPad1.0x1.5mm";
        assert_eq!(run(vec![jumper("SJ1", open)]).len(), 1);
    }

    /// A boot-select pin the silicon pulls to a level, in a card of its own.
    const PULL_CARD: &str = r#"
[[models]]
id = "strap_pull_mcu"
kind = "mcu"
description = "silicon pull fixture"
[models.match]
value_re = "^STRAPPULL$"
[models.pins]
"1" = "vdd"
"2" = "xboot"
[[models.straps]]
role = "xboot"
level = "high"
boot_select = true
[[models.boot_inputs]]
role = "xboot"
pin = "XBOOT"
internal_pull = "pull_down"
[[models.boot_modes]]
name = "Normal"
when = { xboot = "high" }
outcome = "user_flash"
[[models.boot_modes]]
name = "Download"
when = { xboot = "low" }
outcome = "rom_bootloader"
"#;

    fn pull_lint(card: &str, extra: Vec<Component>) -> Vec<(Severity, String)> {
        let mut parts = vec![part("U9", "STRAPPULL", &[("1", 2), ("2", 3)])];
        parts.extend(extra);
        io_lint(card, parts)
    }

    /// A strap pin left bare, or held only by an open-able jumper, settles to
    /// its silicon pull, and that level must be the one the part needs.
    #[test]
    fn a_strap_the_silicon_pull_settles_the_wrong_way_is_flagged_and_the_right_way_is_silent() {
        let wrong = pull_lint(PULL_CARD, vec![]);
        assert_eq!(wrong.len(), 1, "{wrong:#?}");
        assert_eq!(wrong[0].0, Medium);
        for needle in [
            "nothing on it",
            "pull-down settles it low",
            "must be high",
            "XBOOT undriven, internal pull-down (low) at reset release -> Download",
            "10 kOhm pull-up",
        ] {
            assert!(
                wrong[0].1.contains(needle),
                "'{needle}' not in {}",
                wrong[0].1
            );
        }
        let right = PULL_CARD.replace("pull_down", "pull_up");
        assert!(pull_lint(&right, vec![]).is_empty());
        // A jumper to ground as the only holder reads the pull when open: the
        // right way (pull-up) is the usual boot jumper, silent; the wrong way
        // boots the download mode with the jumper left off.
        let mut jp = part("JP1", "BOOT", &[("1", 3), ("2", 1)]);
        jp.footprint = "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical".into();
        assert!(pull_lint(&right, vec![jp.clone()]).is_empty());
        let open = pull_lint(PULL_CARD, vec![jp]);
        assert_eq!(open.len(), 1, "{open:#?}");
        for needle in [
            "JP1 pin 1",
            "while the jumper is open",
            "with the jumper open, internal pull-down (low)",
        ] {
            assert!(
                open[0].1.contains(needle),
                "'{needle}' not in {}",
                open[0].1
            );
        }
    }

    /// An ESP32-C3 GPIO2 has no internal pull at reset (datasheet Table 3-1)
    /// and the strap table only recommends it high (glitch guard), so a bare
    /// GPIO2 is a Low finding, not the High a boot-select pin gets. A pull-up
    /// fixes it, and the classic ESP32 GPIO0, which its internal pull-up holds
    /// high, stays silent bare.
    #[test]
    fn an_esp_strap_with_no_external_pull_is_judged_by_its_datasheet_role() {
        let c3 = || {
            let mut c = part("U1", "ESP32-C3FH4", &[("17", 2), ("33", 1), ("6", 3)]);
            c.footprint = "Package_DFN_QFN:QFN-32-1EP_5x5mm_P0.5mm_EP3.7x3.7mm".into();
            c
        };
        let run = |parts: Vec<Component>| {
            strap_lint(&strap_board("+3V3", "/GPIO2", parts), &library())
                .of_check(LintCheck::StrapPin)
                .map(|f| (f.severity, f.message.clone()))
                .collect::<Vec<_>>()
        };
        let bare = run(vec![c3()]);
        assert_eq!(bare.len(), 1, "{bare:#?}");
        assert_eq!(bare[0].0, Low);
        for needle in [
            "U1 strap pin 6 (gpio2)",
            "floating",
            "10 kOhm pull-up to '+3V3'",
        ] {
            assert!(
                bare[0].1.contains(needle),
                "'{needle}' not in {}",
                bare[0].1
            );
        }
        assert!(run(vec![c3(), pull("R1", "10k", 2)]).is_empty());
        // Classic ESP32 GPIO0 bare: the internal pull-up is the right level.
        let bare_gpio0 = strap_board("+3V3", "/GPIO0", vec![esp32("25")]);
        assert_eq!(
            strap_lint(&bare_gpio0, &library())
                .of_check(LintCheck::StrapPin)
                .count(),
            0
        );
    }
}
