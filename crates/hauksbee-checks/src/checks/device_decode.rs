//! Device-decode check class: configurable controllers whose strap / divider
//! resistors *select a documented operating mode*, decoded against the part's
//! datasheet bands.
//!
//!
//! A whole class of parts (USB-PD sink controllers, programmable LDOs, address-
//! strapped peripherals, mode-pin codecs) read an analog resistor-divider voltage
//! on a configuration pin and decode it against a published table of voltage
//! bands. The board author picks resistor values to land the pin in the band for
//! the mode they want. If the chosen values miss the band, the part silently
//! enters the *wrong* mode. A value/short sweep cannot see this: every resistor
//! is in spec and every net is connected; the fault is that the divider decodes
//! to a band the author did not intend.
//!
//! ## Honest scope
//!
//! This is a **per-part** check that **grows incrementally**. There is no generic
//! "decode any config pin" engine, because each part has its own pins, its own
//! band table, and its own consistency rules (e.g. the CYPD3177's `VBUS_MIN` >
//! `VBUS_MAX` override). Each supported part is a hand-written decoder seeded from
//! its datasheet. Three parts are seeded today: the Cypress / Infineon
//! **CYPD3177** (EZ-PD BCR) USB-C PD sink controller, and the TI **`BQ2407x`**
//! charger family's TMR safety-timer resistor (an out-of-band value found as
//! a real 4.7k-for-47k typo on a published keyboard), and the TI **TPS25982**
//! eFuse family's ILIM resistor plus a connected, rated connector witness.
//! Adding a part means
//! adding a decoder; this is by design, not a stub. One arm is generic: any
//! card with a regulated-current programming law (`[models.current_program]`)
//! is judged on whether its fitted resistor asks for more than the card's
//! sourced programming ceiling.
//!
//! ## Zero-false-positive discipline (binding)
//!
//! The check fires ONLY when:
//!   - the part is positively identified (value/MPN contains the part token), AND
//!   - the configuration divider on the pin resolves to *known* resistor values
//!     (a parseable pull-up to the reference rail and a parseable pull-down to
//!     ground), so the pin voltage is computable.
//!
//! If the divider cannot be resolved to concrete resistor values, the check stays
//! SILENT. A check that cannot resolve the divider does not fire. (See
//! `docs/checks/DEVICE_DECODE.md` and `docs/about/LIMITATIONS.md`.)
//! TPS25982's revised evidence contract makes one narrow distinction: a floating
//! ILIM pin is still silent, while an assembled but unreadable ILIM resistor gets
//! a Low abstention that reads as not judged. Neither state can produce a
//! protective connector-budget warning.
//!
//! ## CYPD3177 seed (the fault this check reproduces)
//!
//! The board's rotary VBUS voltage selector mis-codes its top two detents
//! because it keeps a permanent 10k pull-down across the pin and switches an
//! *additional parallel* leg in, which cannot reproduce the datasheet's
//! intended single-pull-down-per-detent codes.
//! Detent 4 ("15 V") decodes to the 12 V band; detent 5 ("20 V") decodes to the
//! 19 V band. Compounded by a hard-wired `VBUS_MIN` = 19 V, which (datasheet Note 1)
//! overrides `VBUS_MAX` whenever `VBUS_MIN` > `VBUS_MAX`, defeating the selector.
//!
//! ### What this check resolves automatically vs what it cannot
//!
//! Resolves automatically from the netlist:
//!   - `VBUS_MAX` / `VBUS_MIN` pins (by pin function name, or by net name fallback).
//!   - The fixed divider on each pin: pull-up resistor to the 3.3 V reference and
//!     the permanent pull-down resistor to ground.
//!   - Switch-selectable extra pull-down legs *when* the selector is a multi-pad
//!     component (e.g. a rotary SW) whose common pad sits on the config net and
//!     whose other pads each reach ground directly or through one resistor: each
//!     such pad is decoded as one detent.
//!   - The Note-1 consistency check (`VBUS_MIN` band > `VBUS_MAX` band).
//!
//! Cannot resolve (honest limits):
//!   - It does not know the *intended* voltage label silk-screened next to each
//!     detent. It flags a detent only when the decoded band is internally
//!     inconsistent with the part's own reachable range, OR via the Note-1 check.
//!     The "detent N is labelled 15 V but codes 12 V" mismatch is proven in the
//!     unit tests against the hunt's hand-derived numbers; on a real board it is
//!     reported as the set of distinct bands the selector can reach, so a reviewer
//!     sees that "20 V" is unreachable.
//!   - Selectors wired through parts that do not bind as a multi-pad switch on the
//!     config net are not enumerated; the static (permanent) divider is still
//!     decoded and the Note-1 check still runs.

use super::{is_ground_name, is_unconnected_net, norm};
use hauksbee_bind::program_resistor::{ProgramReading, ProgramUnread};
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_plain_resistor as is_source_classified_resistor, Component, ExtractedBoard, LintCheck,
    LintFinding, NetLintReport, Severity, ABSTENTION_MARKER, INFORMATIONAL_MARKER, REMEDY_MARKER,
};
use hauksbee_models::rating_basis::{not_judged, Limit};
use hauksbee_models::value::parse_value;
use hauksbee_models::{ComponentKind, ModelLibrary};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Reference rail for the CYPD3177 config dividers (VDDD = 3.3 V).
const VDDD: f64 = 3.3;

/// A decoded PD request voltage from a `VBUS_MAX` / `VBUS_MIN` divider, in volts.
///
/// `Faulty` is the catch-all for a voltage above every named band (should not
/// happen for an in-range divider, kept for totality).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdVolts {
    V5,
    V9,
    V12,
    V15,
    V19,
    V20,
}

impl PdVolts {
    /// Nominal volts for ordering / display.
    #[must_use]
    pub const fn volts(self) -> u32 {
        match self {
            Self::V5 => 5,
            Self::V9 => 9,
            Self::V12 => 12,
            Self::V15 => 15,
            Self::V19 => 19,
            Self::V20 => 20,
        }
    }
}

/// Decode a config-pin voltage (in millivolts) against the EZ-PD BCR datasheet
/// **Table 2** `VBUS_MAX` bands (Infineon 002-25383), VDDD = 3.3 V reference.
///
/// Verbatim bands (mV):
///   5 V: 0..=248, 9 V: 249..=786, 12 V: 787..=1347, 15 V: 1348..=1920,
///   19 V: 1921..=2778, 20 V: >= 2779.
///
/// `VBUS_MIN` uses the same band structure, so this one decoder serves both pins.
#[must_use]
pub const fn decode_table2(mv: i64) -> PdVolts {
    match mv {
        i64::MIN..=248 => PdVolts::V5,
        249..=786 => PdVolts::V9,
        787..=1347 => PdVolts::V12,
        1348..=1920 => PdVolts::V15,
        1921..=2778 => PdVolts::V19,
        _ => PdVolts::V20,
    }
}

/// Config-pin voltage of a single-pull-up / single-pull-down divider, in mV.
///
/// `Vpin = VDDD * Rpd / (Rpu + Rpd)`. `r_pd` is the *effective* pull-down (the
/// permanent pull-down in parallel with any switched leg). An `r_pd` of 0 (a
/// detent that grounds the pin directly) gives 0 mV; an open pull-down is modelled
/// by the caller passing only the permanent resistor.
#[must_use]
#[allow(clippy::similar_names, clippy::cast_possible_truncation)] // datasheet names; mV of a 3.3 V divider
pub fn divider_mv(r_pu: f64, r_pd: f64) -> i64 {
    if r_pu <= 0.0 && r_pd <= 0.0 {
        return 0;
    }
    let v = VDDD * r_pd / (r_pu + r_pd);
    (v * 1000.0).round() as i64
}

/// Two resistors in parallel (both finite, > 0). Either being 0 ohm shorts to 0.
fn parallel(a: f64, b: f64) -> f64 {
    if a <= 0.0 || b <= 0.0 {
        return 0.0;
    }
    (a * b) / (a + b)
}

/// Does this component's value / MPN positively identify it as a CYPD3177
/// (EZ-PD BCR)? Robust to layout-only extraction: we key on the value string
/// directly rather than depending on a model-DB entry existing.
fn is_cypd3177(c: &Component) -> bool {
    let v = c.value.to_ascii_uppercase().replace(['-', ' ', '_'], "");
    let l = c.lib_id.to_ascii_uppercase().replace(['-', ' ', '_'], "");
    v.contains("CYPD3177") || l.contains("CYPD3177") || v.contains("EZPDBCR")
}

/// A plain two-terminal, assembled resistor (the shared strap-lint predicate)
/// with a parseable ohm value.
fn resistor_ohms(c: &Component) -> Option<f64> {
    if !is_plain_resistor(c) {
        return None;
    }
    parse_value(&c.value)
        .map(|p| p.si)
        .filter(|o| o.is_finite() && *o >= 0.0)
}

// ── Net-name helpers (local copies; extract versions are private) ──

/// Is this the 3.3 V reference rail (VDDD)? The CYPD3177 dividers reference VDDD,
/// which the board exposes as +3.3V / +3V3 / VDDD.
fn is_vddd_name(name: &str) -> bool {
    let n = norm(name);
    matches!(
        n.as_str(),
        "+3V3" | "3V3" | "+3.3V" | "3.3V" | "VDDD" | "VCC3V3" | "VDD3V3" | "VDD3P3" | "+3V3A"
    ) || n.contains("3V3")
        || n.contains("3.3V")
        || n == "VDDD"
}

/// The net id a U pin carries for a given pin-function role (e.g. "`VBUS_MAX`"),
/// or, failing a function match, by the net's own name. Returns (`net_id`, name).
fn pin_net_for_role<'a>(
    board: &'a ExtractedBoard,
    comp: &'a Component,
    role: &str,
) -> Option<(i64, &'a str)> {
    // Prefer the pin function (the schematic / newer .kicad_pcb populate it).
    if let Some(p) = comp
        .pins
        .iter()
        .find(|p| p.function.eq_ignore_ascii_case(role) && p.net.is_some())
    {
        if let Some(id) = p.net {
            if let Some(net) = board.net(id) {
                return Some((id, &net.name));
            }
        }
    }
    // Fallback: a net whose (leaf) name names the role, attached to this part.
    let want = role.to_ascii_uppercase();
    for p in &comp.pins {
        let Some(id) = p.net else { continue };
        let Some(net) = board.net(id) else { continue };
        let n = norm(&net.name);
        if n == want || n.contains(&want) {
            return Some((id, &net.name));
        }
    }
    None
}

/// The resolved fixed divider on a config net: the pull-up resistor to VDDD and
/// the permanent pull-down resistor to ground. Either may be absent. Switched
/// legs are handled separately.
struct FixedDivider {
    r_pu: Option<(String, f64)>, // (ref, ohms) to VDDD
    r_pd: Option<(String, f64)>, // (ref, ohms) to GND
}

/// Walk a config net's members and resolve the fixed pull-up (to VDDD) and
/// permanent pull-down (to GND) resistors. If multiple pull-downs are directly
/// present, they are combined in parallel; likewise pull-ups.
fn resolve_fixed_divider(board: &ExtractedBoard, net_id: i64) -> FixedDivider {
    let mut pu: Option<(String, f64)> = None;
    let mut pd: Option<(String, f64)> = None;
    for (c, _p) in board.net_members(net_id) {
        let Some(ohms) = resistor_ohms(c) else {
            continue;
        };
        for op in &c.pins {
            let Some(oid) = op.net else { continue };
            if oid == net_id {
                continue;
            }
            let Some(on) = board.net(oid) else { continue };
            if is_ground_name(&on.name) {
                pd = Some(match pd {
                    None => (c.reference.clone(), ohms),
                    Some((r, o)) => (format!("{r}||{}", c.reference), parallel(o, ohms)),
                });
            } else if is_vddd_name(&on.name) {
                pu = Some(match pu {
                    None => (c.reference.clone(), ohms),
                    Some((r, o)) => (format!("{r}||{}", c.reference), parallel(o, ohms)),
                });
            }
        }
    }
    FixedDivider { r_pu: pu, r_pd: pd }
}

/// One enumerated rotary detent: its switched extra pull-down (None = open leg),
/// and the resulting decode.
struct Detent {
    /// Effective extra pull-down (ohms) the selector adds in parallel with the
    /// permanent pull-down. `Some(0.0)` = grounds the pin directly; `None` = open.
    extra_pd: Option<f64>,
    label: String,
}

/// Enumerate the selectable detents of a rotary / slide selector whose *common*
/// pad sits on `net_id`. We look for a multi-pad switch-like component (>= 3
/// connected pads, ref SW*/S*, or footprint naming a switch) that touches
/// `net_id`, then for each of its *other* pads decode the leg: a pad on ground is
/// a direct-ground detent (0 ohm); a pad reaching ground through exactly one
/// resistor contributes that resistor as the extra pull-down; an unconnected /
/// dangling pad is an open detent.
fn enumerate_detents(board: &ExtractedBoard, net_id: i64) -> Vec<Detent> {
    let mut detents = Vec::new();
    // Find a switch component with a pad on this net.
    for (sw, _p) in board.net_members(net_id) {
        if !is_selector_switch(sw) {
            continue;
        }
        for pad in &sw.pins {
            let Some(pid) = pad.net else {
                // a dangling NC pad on the selector is an open detent
                detents.push(Detent {
                    extra_pd: None,
                    label: format!("{}.{}", sw.reference, pad.number),
                });
                continue;
            };
            if pid == net_id {
                continue; // the common pad
            }
            let Some(pn) = board.net(pid) else { continue };
            if is_unconnected_net(&pn.name) {
                detents.push(Detent {
                    extra_pd: None,
                    label: format!("{}.{} (open)", sw.reference, pad.number),
                });
                continue;
            }
            if is_ground_name(&pn.name) {
                detents.push(Detent {
                    extra_pd: Some(0.0),
                    label: format!("{}.{} (GND)", sw.reference, pad.number),
                });
                continue;
            }
            // A leg node: look for a single resistor from this node to ground.
            if let Some((rref, ohms)) = leg_resistor_to_ground(board, pid) {
                detents.push(Detent {
                    extra_pd: Some(ohms),
                    label: format!("{}.{} ({rref})", sw.reference, pad.number),
                });
            }
            // else: leg node we cannot resolve to a clean pull-down -> skip it
            // (zero-false-positive: we do not invent a value).
        }
        // Only the first selector on the net is enumerated.
        break;
    }
    detents
}

/// A slide / rotary / DIP selector: a switch-class component (ref SW*/S*, or a
/// footprint/lib naming a switch) with >= 3 connected pads (a 2-pad part is a
/// plain on/off button, not a multi-position selector).
fn is_selector_switch(c: &Component) -> bool {
    let r = c.reference.to_ascii_uppercase();
    let lib = c.lib_id.to_ascii_lowercase();
    let fp = c.footprint.to_ascii_lowercase();
    let connected = c.pins.iter().filter(|p| p.net.is_some()).count();
    if connected < 3 {
        return false;
    }
    r.starts_with("SW")
        || (r.starts_with('S') && r.len() <= 3 && r.chars().skip(1).all(|c| c.is_ascii_digit()))
        || lib.contains("switch")
        || lib.contains("rotary")
        || fp.contains("switch")
        || fp.contains("rotary")
}

/// A single assembled resistor with one pad on `node` and the other on ground.
fn leg_resistor_to_ground(board: &ExtractedBoard, node: i64) -> Option<(String, f64)> {
    for (c, _p) in board.net_members(node) {
        let Some(ohms) = resistor_ohms(c) else {
            continue;
        };
        for op in &c.pins {
            let Some(oid) = op.net else { continue };
            if oid == node {
                continue;
            }
            if let Some(on) = board.net(oid) {
                if is_ground_name(&on.name) {
                    return Some((c.reference.clone(), ohms));
                }
            }
        }
    }
    None
}

/// Run the device-decode check class over a board.
///
/// Seeded parts: the
/// CYPD3177 USB-C PD sink, the TI `BQ2407x` charger family's TMR safety timer,
/// and the TI TPS25982 eFuse family's ILIM current limit; each identified part
/// is decoded independently.
#[must_use]
pub fn device_decode_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    for (index, comp) in board.components.iter().enumerate() {
        check_program_domain(board, index, comp, lib, &mut report);
        if is_cypd3177(comp) {
            check_cypd3177(board, comp, &mut report);
        }
        if is_bq2407x(comp) {
            check_bq2407x_tmr(board, comp, &mut report);
            check_bq2407x_ts(board, comp, &mut report);
        }
        if is_tps25982(comp) {
            check_tps25982_ilim(board, comp, lib, &mut report);
        }
        check_program_range(board, comp, lib, &mut report);
    }
    report
}

/// Card-driven, for any part whose model states the datasheet's allowed
/// programming-resistor range (`current_program.min_program_ohms` /
/// `max_program_ohms`): a fitted resistor from the programming pin to ground
/// outside that range is a Medium finding, because the part is then outside
/// its specified operation whatever the programming equation would return.
/// Silent when the part is absent or unmodelled, the card states no range, or
/// the pin is floating. A leg whose value cannot be read is the one parameter
/// that decides the check missing, so it abstains by name (not judged, never a
/// pass), unless the programming-domain check already abstained on the same pin.
fn check_program_range(
    board: &ExtractedBoard,
    comp: &Component,
    lib: &ModelLibrary,
    report: &mut NetLintReport,
) {
    let AssemblyState::Present(part) = AssemblyState::of(comp) else {
        return;
    };
    let resolution = hauksbee_bind::binder::resolve(lib, part);
    let Some(model) = resolution
        .model
        .as_ref()
        .filter(|m| !m.id.starts_with("generic"))
    else {
        return;
    };
    let Some(program) = model
        .current_program
        .as_ref()
        .filter(|p| p.min_program_ohms.is_some() || p.max_program_ohms.is_some())
    else {
        return;
    };
    let Ok(net) = hauksbee_bind::component_evidence::role_net(part, model, &program.pin) else {
        return;
    };
    let (reference, ohms) = match program_resistor_to_ground(board, net) {
        ProgramResistor::Floating => return,
        ProgramResistor::Unparseable { reference, value } => {
            let net_name = board.net(net).map(|n| n.name.clone()).unwrap_or_default();
            let already_abstained = report.findings.iter().any(|f| {
                f.check == LintCheck::DeviceDecode
                    && f.is_abstention()
                    && f.refs.first() == Some(&comp.reference)
                    && f.nets == [net_name.clone()]
            });
            if !already_abstained {
                report.findings.push(program_pin_unreadable(
                    comp,
                    &program.pin.to_ascii_uppercase(),
                    &net_name,
                    &ProgramUnread::unreadable_value(&reference, &value),
                    "its resistance is unchecked against the range the datasheet allows",
                ));
            }
            return;
        }
        ProgramResistor::Parsed { reference, ohms } => (reference, ohms),
    };
    let Some((bound, below)) = program.out_of_program_range(ohms) else {
        return;
    };
    let pin = program.pin.to_ascii_uppercase();
    let (side, word) = if below {
        ("below", "minimum")
    } else {
        ("above", "maximum")
    };
    report.findings.push(LintFinding {
        check: LintCheck::DeviceDecode,
        severity: Severity::Medium,
        message: format!(
            "{} {pin} resistor {reference} = {} ohm is {side} the {} ohm {word} the {} \
             datasheet allows on {pin}, so the part is outside its specified operation; \
             fit a value inside the datasheet's programming range",
            comp.reference,
            if (0.01..10.0).contains(&ohms) {
                format!("{ohms:.2}")
            } else {
                format_ohms(ohms)
            },
            format_ohms(bound),
            model.id,
        ),
        refs: vec![comp.reference.clone(), reference],
        nets: board
            .net(net)
            .map(|n| vec![n.name.clone()])
            .unwrap_or_default(),
    });
}

// ─────────────────────────────────────────────────────────────────────────────
// Any card with a regulated-current programming law: the fitted resistor must
// ask for a current inside the card's sourced programming domain.
//
// A `[models.current_program]` block with `semantics = "regulated_current"`
// carries the datasheet's programming equation and `max_operating_current_a`,
// the highest current the datasheet specifies for programmed operation (the
// schema requires it for this semantics and forbids inferring it from a device
// rating). A resistor whose equation current is above that ceiling programs
// the part outside its specified range: the binder then clamps to the ceiling
// and reports only a reduced-fidelity assumption, so without this finding a
// charger PROG resistor a decade too low passes. The read is the same populated
// resistance-to-ground walk the ampacity attribution and the binder use. A pin
// shorted to ground (a 0 ohm resistor or closed link) is the extreme of the
// same fault and is reported as one: the equation diverges as R goes to 0. A
// network the walk cannot read (a duplicate designator, a reference spelling
// that does not say "resistor", no resistor path to ground) is the one
// parameter that decides this check missing, so the check abstains by name
// (`not judged, which is not a pass`) and the verdict is never a pass. A pin
// with nothing fitted on it (no resistor, or only a DNP one) is floating and
// stays silent. A generic stand-in card, a protection-limit law, or a
// sense-scaled law (which needs the sense shunt, and for which a shorted
// programming resistor is zero current, not unbounded) stays silent.
// ─────────────────────────────────────────────────────────────────────────────

/// Margin over a card's programming ceiling before the programmed current is
/// called out of range. It covers a 5 % resistor and E96 rounding of a
/// resistor chosen to sit at the ceiling (on a 1000 V / R law, 2.0k asks
/// exactly 0.50 A and the E96 neighbour 1.91k asks 0.52 A); the E24 step
/// below, 1.8k at 0.56 A, is past it and is reported.
const PROGRAM_DOMAIN_GRACE: f64 = 1.10;

/// The smallest resistance whose equation current is at most `ceiling_a`, by
/// bisection in log space (every supported one-resistor law falls as the
/// resistance rises). `None` when even a 1 Gohm resistor asks for more.
fn min_program_ohms(
    program: &hauksbee_models::schema::CurrentProgram,
    from_ohms: f64,
    ceiling_a: f64,
) -> Option<f64> {
    let asks = |ohms: f64| program.equation_current_a(ohms).unwrap_or(f64::INFINITY);
    let (mut lo, mut hi) = (from_ohms.ln(), 1e9_f64.ln());
    if asks(hi.exp()) > ceiling_a {
        return None;
    }
    for _ in 0..80 {
        let mid = 0.5 * (lo + hi);
        if asks(mid.exp()) > ceiling_a {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(hi.exp())
}

fn check_program_domain(
    board: &ExtractedBoard,
    index: usize,
    comp: &Component,
    lib: &ModelLibrary,
    report: &mut NetLintReport,
) {
    use hauksbee_models::schema::{CurrentProgramEquation, CurrentProgramSemantics};
    // A programmed part has supply, ground and the programming pin at least;
    // two-terminal parts never carry a programming law, so skip resolving them.
    if comp.pins.len() < 3 {
        return;
    }
    let AssemblyState::Present(part) = AssemblyState::of(comp) else {
        return;
    };
    let Some(model) = hauksbee_bind::binder::resolve(lib, part).model else {
        return;
    };
    if model.id.starts_with("generic") {
        return;
    }
    let Some(program) = model
        .current_program
        .as_ref()
        .filter(|p| p.semantics == CurrentProgramSemantics::RegulatedCurrent)
    else {
        return;
    };
    let Some(ceiling_a) = program
        .max_operating_current_a
        .filter(|a| a.is_finite() && *a > 0.0)
    else {
        return;
    };
    let Ok(net) = hauksbee_bind::component_evidence::role_net(part, &model, &program.pin) else {
        return;
    };
    // The sense-scaled law needs the sense shunt, and a shorted programming
    // resistor there is zero current (the control voltage collapses), never the
    // unbounded current of the one-resistor laws: no verdict here either way.
    if matches!(
        program.equation,
        CurrentProgramEquation::SenseScaledResistance { .. }
    ) {
        return;
    }
    let net_name = board.net(net).map(|n| n.name.clone()).unwrap_or_default();
    let (ohms, path) = match super::ampacity::program_reading(board, net, 4, Some(index)) {
        ProgramReading::Ohms(ohms, path) => (ohms, path),
        ProgramReading::Shorted(links) => {
            report.findings.push(shorted_program_finding(
                comp, program, ceiling_a, &net_name, links,
            ));
            return;
        }
        ProgramReading::Unread(ProgramUnread::UnknownNet) => return,
        // A floating pin (nothing fitted on it) is the explicit user rule: absent
        // or floating stays silent, as for every other programming pin here.
        ProgramReading::Unread(ProgramUnread::NoPath)
            if !has_fitted_member_besides(board, net, comp) =>
        {
            return;
        }
        ProgramReading::Unread(why) => {
            report.findings.push(unread_program_finding(
                comp, program, ceiling_a, &net_name, &why,
            ));
            return;
        }
    };
    let Some(asked_a) = program.equation_current_a(ohms) else {
        return;
    };
    if asked_a <= ceiling_a * PROGRAM_DOMAIN_GRACE {
        return;
    }
    let fix = program_fix(program, ohms, ceiling_a);
    let mut refs = vec![comp.reference.clone()];
    refs.extend(path.iter().cloned());
    report.findings.push(LintFinding {
        check: LintCheck::DeviceDecode,
        severity: Severity::Medium,
        message: format!(
            "{} ({}) is programmed outside its specified range: its {} resistor {} = {} ohm \
             on net '{net_name}' asks for {} by the part's programming equation, \
             above the {} the datasheet specifies for programmed operation, so \
             the part either overheats delivering it or limits at an unspecified point; {fix}",
            comp.reference,
            comp.value,
            program.pin,
            path.join("+"),
            format_ohms(ohms),
            format_amps(asked_a),
            format_amps(ceiling_a)
        ),
        refs,
        nets: vec![net_name],
    });
}

/// The fix for a programming resistor that is too small: the least resistance
/// whose equation current is within the card's ceiling.
fn program_fix(
    program: &hauksbee_models::schema::CurrentProgram,
    from_ohms: f64,
    ceiling_a: f64,
) -> String {
    min_program_ohms(program, from_ohms, ceiling_a).map_or_else(
        || {
            "fit the resistor the datasheet's programming table gives for the current wanted"
                .to_string()
        },
        |min| {
            format!(
                "fit at least {} ohm for the {} ceiling, or the value the \
                 datasheet's programming table gives for the current wanted",
                format_ohms(min),
                format_amps(ceiling_a)
            )
        },
    )
}

/// A programming pin tied to ground (zero ohms, by a ground net or by `0R` /
/// closed links alone): the equation `I = k / R` diverges, the extreme of the
/// too-small resistor [`check_program_domain`] reports for any other value.
fn shorted_program_finding(
    comp: &Component,
    program: &hauksbee_models::schema::CurrentProgram,
    ceiling_a: f64,
    net_name: &str,
    links: Vec<String>,
) -> LintFinding {
    let via = if links.is_empty() {
        "directly".to_string()
    } else {
        format!("through {} (a 0 ohm link)", links.join("+"))
    };
    let mut refs = vec![comp.reference.clone()];
    refs.extend(links);
    LintFinding {
        check: LintCheck::DeviceDecode,
        severity: Severity::Medium,
        message: format!(
            "{} ({}) is programmed outside its specified range: its {} pin on net '{net_name}' \
             is tied to ground {via}, 0 ohm, so the part's programming equation asks for an \
             unbounded current, far above the {} the datasheet specifies for \
             programmed operation, so the part either overheats delivering it or limits at an \
             unspecified point; {}",
            comp.reference,
            comp.value,
            program.pin,
            format_amps(ceiling_a),
            program_fix(program, 1e-3, ceiling_a)
        ),
        refs,
        nets: vec![net_name.to_string()],
    }
}

/// Whether any fitted component other than `ic` is joined to `net` by more
/// than one pad, or a resistor with a dangling leg: anything a reader could
/// have to read. A DNP part, a test point or another single-pad marker is not.
fn has_fitted_member_besides(board: &ExtractedBoard, net: i64, ic: &Component) -> bool {
    board.net_members(net).into_iter().any(|(member, _)| {
        !std::ptr::eq(member, ic)
            && !matches!(AssemblyState::of(member), AssemblyState::DnpAbsent(_))
            && !hauksbee_bind::program_resistor::is_inert_marker(member)
    })
}

/// The abstention for a programming network the walk could not read: the one
/// parameter that decides [`check_program_domain`] is missing, so the check
/// says so by name, and the verdict reads it as not judged, never a pass.
fn unread_program_finding(
    comp: &Component,
    program: &hauksbee_models::schema::CurrentProgram,
    ceiling_a: f64,
    net_name: &str,
    why: &ProgramUnread,
) -> LintFinding {
    program_pin_unreadable(
        comp,
        &program.pin,
        net_name,
        why,
        &format!(
            "its programmed current is unchecked against the {} the datasheet \
             specifies",
            format_amps(ceiling_a)
        ),
    )
}

/// The one abstention for a programming pin whose resistance cannot be read
/// (an unparseable value, a duplicate designator, an unclassifiable reference,
/// no resistor path, a network too large): every check that rests on a program
/// pin's resistance raises it, so none can read an unreadable setting as a pass.
/// `unchecked` says what therefore stays unchecked. Low, marked as an abstention
/// and as not judged, with the remedy that fits the reason.
fn program_pin_unreadable(
    comp: &Component,
    pin: &str,
    net_name: &str,
    why: &ProgramUnread,
    unchecked: &str,
) -> LintFinding {
    let remedy = match why {
        ProgramUnread::Part { .. } => {
            "make that part readable (rename a duplicate designator, or give the resistor a \
             conventional R designator and a numeric value), then re-check"
        }
        ProgramUnread::NoPath => {
            "fit the programming resistor from the pin to a ground net so its current can be \
             read, then re-check"
        }
        ProgramUnread::TooComplex | ProgramUnread::UnknownNet => {
            "reduce the pin's network to one resistor, or a short parallel or series set, to \
             ground, then re-check"
        }
    };
    let what = format!(
        "{} ({}) programming-resistor check{ABSTENTION_MARKER}: its {pin} pin's resistance to \
         ground on net '{net_name}' could not be read off the board ({}), so {unchecked}",
        comp.reference,
        comp.value,
        why.describe(),
    );
    let mut refs = vec![comp.reference.clone()];
    if let ProgramUnread::Part { reference, .. } = why {
        refs.push(reference.clone());
    }
    LintFinding {
        check: LintCheck::DeviceDecode,
        severity: Severity::Low,
        message: format!("{} {REMEDY_MARKER}{remedy}", not_judged(&what)),
        refs,
        nets: vec![net_name.to_string()],
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TPS25982 (TI TPS259822/3/4/7, L current-limiter and O circuit-breaker
// variants), ILIM pin.
//
// Source: Texas Instruments TPS25982 datasheet, SLVSEI3D (Rev. D, May 2026):
//   - Device Comparison Table: the eight TPS259822/3/4/7 L/O variants covered
//     by this one table. TPS25980/81/85 and other adjacent families are NOT
//     included: their separate datasheets are not evidence for this decoder.
//   - Section 7.3.3.3, Equation 8 (active-current-limiter variants), with the
//     same law in Section 7.3.3.2, Equation 4 (circuit-breaker variants):
//         RILIM(ohm) = 1460 / (ILIM(A) - 0.11)
//   - Section 6.5 Electrical Characteristics, RILIM=100 ohm and TJ=-40..125 C:
//         ILIM = 12.85 / 14.71 / 15.99 A (min / typ / max).
//     For another legal RILIM we evaluate Equation 8 for typical, then retain
//     that full-temperature row's min/typ and max/typ tolerance ratios. This is
//     an explicit table-bound estimate, not measured board current.
//   - Section 6.3 Recommended Operating Conditions: RILIM=82..1650 ohm.
//
// Every legal, parseable RILIM gets a Low (non-gating) decode note so a
// detector-pair can retain the setting even when no rated series witness is
// connected. A protective Medium finding needs a bound max_current_a on an
// actually connected connector reachable from IN or OUT (directly or through
// a low-ohmic series shunt), and fires only when the decoded MINIMUM exceeds
// the rating by more than the declared 10% tolerance grace.
// ─────────────────────────────────────────────────────────────────────────────

const TPS25982_RILIM_MIN_OHMS: f64 = 82.0;
const TPS25982_RILIM_MAX_OHMS: f64 = 1650.0;
const TPS25982_EQ8_NUMERATOR_A_OHM: f64 = 1460.0;
const TPS25982_EQ8_OFFSET_A: f64 = 0.11;
const TPS25982_TABLE_100R_MIN_A: f64 = 12.85;
const TPS25982_TABLE_100R_TYP_A: f64 = 14.71;
const TPS25982_TABLE_100R_MAX_A: f64 = 15.99;
const TPS25982_BUDGET_GRACE: f64 = 1.10;
/// Cross only deliberate low-ohmic series links while looking for a connector
/// boundary. This admits the real board's 2 mOhm current-sense shunt, but not a
/// pull-down/load resistor that merely happens to share the rail.
const TPS25982_SERIES_LINK_MAX_OHMS: f64 = 1.0;
const TPS25982_SERIES_WALK_MAX_DEPTH: usize = 4;

#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_field_names)] // `_a` is the unit (amps), the repo's field convention
struct IlimBand {
    min_a: f64,
    typ_a: f64,
    max_a: f64,
}

fn tps25982_ilim_band(ohms: f64) -> IlimBand {
    let typ_a = TPS25982_EQ8_NUMERATOR_A_OHM / ohms + TPS25982_EQ8_OFFSET_A;
    IlimBand {
        min_a: typ_a * (TPS25982_TABLE_100R_MIN_A / TPS25982_TABLE_100R_TYP_A),
        typ_a,
        max_a: typ_a * (TPS25982_TABLE_100R_MAX_A / TPS25982_TABLE_100R_TYP_A),
    }
}

/// Minimum RILIM that makes the table-scaled minimum no greater than `limit_a`.
fn tps25982_required_rilim_ohms(limit_a: f64) -> Option<f64> {
    let min_ratio = TPS25982_TABLE_100R_MIN_A / TPS25982_TABLE_100R_TYP_A;
    let denominator = limit_a / min_ratio - TPS25982_EQ8_OFFSET_A;
    (denominator > 0.0).then_some(TPS25982_EQ8_NUMERATOR_A_OHM / denominator)
}

/// Match the exact TPS25982 family table: root token, or its 2/3/4/7 L/O
/// orderable variants. In particular TPS25980/81/85 and a made-up TPS259820 do
/// not borrow this family decoder.
fn is_tps25982_token(value: &str) -> bool {
    let uppercase = value.to_ascii_uppercase();
    uppercase.match_indices("TPS25982").any(|(start, _)| {
        // Require a real token boundary on the left. This still admits a
        // library-qualified value such as `Power_Management:TPS259824LNRGET`,
        // but rejects an unrelated identifier merely containing the substring.
        if start > 0
            && uppercase[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric())
        {
            return false;
        }
        let token: String = uppercase[start..]
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        let rest = &token["TPS25982".len()..];
        if rest.is_empty() {
            return true;
        }
        let mut chars = rest.chars();
        let variant = chars.next().expect("non-empty variant suffix");
        if !matches!(variant, '2' | '3' | '4' | '7') {
            return false;
        }
        chars
            .next()
            .is_none_or(|response| matches!(response, 'L' | 'O'))
    })
}

fn is_tps25982(c: &Component) -> bool {
    if is_tps25982_token(&c.value) || is_tps25982_token(&c.lib_id) {
        return true;
    }
    c.properties.iter().any(|(key, value)| {
        hauksbee_extract::bom::is_mpn_property_key(key) && is_tps25982_token(value)
    })
}

use super::straps::is_assembled_resistor as is_plain_resistor;
use hauksbee_ir::units::{format_amps, format_series};

enum ProgramResistor {
    Floating,
    Parsed { reference: String, ohms: f64 },
    Unparseable { reference: String, value: String },
}

/// Resolve all assembled resistor legs directly from `node` to ground. Multiple
/// parseable legs are a real parallel network and are combined; any unreadable
/// leg makes the whole setting explicitly unjudgeable instead of selecting the
/// first convenient resistor.
fn program_resistor_to_ground(board: &ExtractedBoard, node: i64) -> ProgramResistor {
    let mut parsed: Vec<(String, f64)> = Vec::new();
    let mut unreadable: Vec<(String, String)> = Vec::new();
    for (component, _) in board.net_members(node) {
        if !is_plain_resistor(component) {
            continue;
        }
        let reaches_ground = component.pins.iter().any(|pin| {
            pin.net
                .filter(|net| *net != node)
                .and_then(|net| board.net(net))
                .is_some_and(|net| is_ground_name(&net.name))
        });
        if !reaches_ground {
            continue;
        }
        match parse_value(&component.value)
            .map(|parsed| parsed.si)
            .filter(|ohms| ohms.is_finite() && *ohms >= 0.0)
        {
            Some(ohms) => parsed.push((component.reference.clone(), ohms)),
            None => unreadable.push((component.reference.clone(), component.value.clone())),
        }
    }
    if !unreadable.is_empty() {
        unreadable.sort();
        return ProgramResistor::Unparseable {
            reference: unreadable
                .iter()
                .map(|(reference, _)| reference.as_str())
                .collect::<Vec<_>>()
                .join("||"),
            value: unreadable
                .iter()
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>()
                .join("||"),
        };
    }
    if parsed.is_empty() {
        return ProgramResistor::Floating;
    }
    parsed.sort_by(|a, b| a.0.cmp(&b.0));
    let conductance: f64 = parsed
        .iter()
        .map(|(_, ohms)| {
            if *ohms == 0.0 {
                f64::INFINITY
            } else {
                1.0 / ohms
            }
        })
        .sum();
    let ohms = if conductance.is_infinite() {
        0.0
    } else {
        1.0 / conductance
    };
    ProgramResistor::Parsed {
        reference: parsed
            .iter()
            .map(|(reference, _)| reference.as_str())
            .collect::<Vec<_>>()
            .join("||"),
        ohms,
    }
}

/// Unique net IDs carried by pin functions `IN`, `IN_1`, ... or `OUT`,
/// `OUT_17`, ... . A role split across multiple pads is normal on this eFuse;
/// a pin without a real net contributes nothing.
fn tps25982_power_nets(component: &Component, role: &str) -> BTreeSet<i64> {
    component
        .pins
        .iter()
        .filter(|pin| {
            let function = pin.function.to_ascii_uppercase();
            function == role || function.starts_with(&format!("{role}_"))
        })
        .filter_map(|pin| pin.net)
        .collect()
}

fn low_ohmic_far_net(component: &Component, from: i64) -> Option<i64> {
    if !is_plain_resistor(component) {
        return None;
    }
    let ohms = parse_value(&component.value)?.si;
    if !ohms.is_finite() || !(0.0..=TPS25982_SERIES_LINK_MAX_OHMS).contains(&ohms) {
        return None;
    }
    let nets: BTreeSet<_> = component.pins.iter().filter_map(|pin| pin.net).collect();
    if nets.len() != 2 || !nets.contains(&from) {
        return None;
    }
    nets.into_iter().find(|net| *net != from)
}

#[derive(Debug, Clone)]
struct RatedConnector {
    reference: String,
    model_id: String,
    rating_a: f64,
    path_net: String,
}

/// Find rated connector boundaries on the actual IN/OUT conduction path. The
/// walk is deliberately narrow: same-net copper plus a bounded number of
/// <=1-ohm assembled resistor links. It cannot cross the eFuse itself, a DNP /
/// identity-refused component, a pull-down, or an unconnected connector pad.
fn tps25982_rated_connectors(
    board: &ExtractedBoard,
    efuse: &Component,
    lib: &ModelLibrary,
) -> Vec<RatedConnector> {
    let starts: BTreeSet<_> = tps25982_power_nets(efuse, "IN")
        .into_iter()
        .chain(tps25982_power_nets(efuse, "OUT"))
        .collect();
    let mut queue: VecDeque<_> = starts.into_iter().map(|net| (net, 0usize)).collect();
    let mut seen_nets = BTreeSet::new();
    let mut witnesses = BTreeMap::<String, RatedConnector>::new();

    while let Some((net_id, depth)) = queue.pop_front() {
        if !seen_nets.insert(net_id) {
            continue;
        }
        let Some(net) = board.net(net_id) else {
            continue;
        };
        if is_ground_name(&net.name) || is_unconnected_net(&net.name) {
            continue;
        }
        for (component, _) in board.net_members(net_id) {
            if component.reference == efuse.reference {
                continue; // never jump internally from IN to OUT
            }
            let AssemblyState::Present(part) = AssemblyState::of(component) else {
                continue;
            };
            if let Some(model) = hauksbee_bind::binder::resolve(lib, part).model {
                if model.kind == ComponentKind::Connector {
                    let r = &model.ratings;
                    let rating = r.basis.judged(Limit::Current, r.max_current_a);
                    if let Some(rating_a) = rating.filter(|a| *a > 0.0) {
                        witnesses
                            .entry(component.reference.clone())
                            .or_insert_with(|| RatedConnector {
                                reference: component.reference.clone(),
                                model_id: model.id.clone(),
                                rating_a,
                                path_net: net.name.clone(),
                            });
                    }
                }
            }
            if depth < TPS25982_SERIES_WALK_MAX_DEPTH {
                if let Some(far_net) = low_ohmic_far_net(component, net_id) {
                    queue.push_back((far_net, depth + 1));
                }
            }
        }
    }
    witnesses.into_values().collect()
}

fn check_tps25982_ilim(
    board: &ExtractedBoard,
    efuse: &Component,
    lib: &ModelLibrary,
    report: &mut NetLintReport,
) {
    let Some((ilim_net_id, ilim_net_name)) = pin_net_for_role(board, efuse, "ILIM") else {
        return; // no resolvable ILIM pin: silent
    };
    let (r_ref, ohms) = match program_resistor_to_ground(board, ilim_net_id) {
        ProgramResistor::Floating => return, // explicit user rule: absent/floating stays silent
        ProgramResistor::Unparseable { reference, value } => {
            report.findings.push(program_pin_unreadable(
                efuse,
                "ILIM",
                ilim_net_name,
                &ProgramUnread::unreadable_value(&reference, &value),
                "no current-limit band or connector-budget verdict is claimed",
            ));
            return;
        }
        ProgramResistor::Parsed { reference, ohms } => (reference, ohms),
    };

    if !(TPS25982_RILIM_MIN_OHMS..=TPS25982_RILIM_MAX_OHMS).contains(&ohms) {
        report.findings.push(LintFinding {
            check: LintCheck::DeviceDecode,
            severity: Severity::Medium,
            message: format!(
                "{} eFuse current limit: ILIM resistor {r_ref} = {} ohm is outside the \
                 TPS25982 datasheet's 82-1650 ohm programming range, so Equation 8 \
                 does not support a current-limit or connector-budget verdict; fit an \
                 in-range value selected from RILIM = 1460 / (ILIM - 0.11)",
                efuse.reference,
                format_ohms(ohms)
            ),
            refs: vec![efuse.reference.clone(), r_ref],
            nets: vec![ilim_net_name.to_string()],
        });
        return;
    }

    let band = tps25982_ilim_band(ohms);
    report.findings.push(LintFinding {
        check: LintCheck::DeviceDecode,
        severity: Severity::Low,
        message: format!(
            "{} eFuse current-limit decode{INFORMATIONAL_MARKER}: ILIM resistor {r_ref} = {} ohm \
             decodes to {} min/typ/max across the TPS25982 \
             datasheet tolerance band (Section 7.3.3.3 Equation 8 scaled by the \
             Section 6.5 full-temperature 100-ohm row); this note alone makes no \
             connector-budget verdict",
            efuse.reference,
            format_ohms(ohms),
            format_series(&[band.min_a, band.typ_a, band.max_a], "/", "A")
        ),
        refs: vec![efuse.reference.clone(), r_ref.clone()],
        nets: vec![ilim_net_name.to_string()],
    });

    for witness in tps25982_rated_connectors(board, efuse, lib) {
        let grace_a = witness.rating_a * TPS25982_BUDGET_GRACE;
        if band.min_a <= grace_a {
            continue;
        }
        let grace_r = tps25982_required_rilim_ohms(grace_a).map(f64::ceil);
        let strict_r = tps25982_required_rilim_ohms(witness.rating_a).map(f64::ceil);
        let fix = match (grace_r, strict_r) {
            (Some(grace_r), Some(strict_r)) => format!(
                "Increase {r_ref} to at least {grace_r:.0} ohm to meet the 10% grace \
                 threshold (or {strict_r:.0} ohm to bring the decoded minimum to the \
                 rating itself)"
            ),
            _ => format!(
                "Select a larger legal {r_ref} from Equation 8, or use a connector with \
                 a sufficient continuous-current rating"
            ),
        };
        report.findings.push(LintFinding {
            check: LintCheck::DeviceDecode,
            severity: Severity::Medium,
            message: format!(
                "{} eFuse connector budget: {r_ref} = {} ohm decodes to \
                 {} min/typ/max, while connected {} ({}) is rated \
                 {} on the {} conduction path; the minimum {} exceeds the \
                 rating plus the explicit 10% grace ({}). {fix}",
                efuse.reference,
                format_ohms(ohms),
                format_series(&[band.min_a, band.typ_a, band.max_a], "/", "A"),
                witness.reference,
                witness.model_id,
                format_amps(witness.rating_a),
                witness.path_net,
                format_amps(band.min_a),
                format_amps(grace_a)
            ),
            refs: vec![efuse.reference.clone(), r_ref.clone(), witness.reference],
            nets: vec![ilim_net_name.to_string(), witness.path_net],
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// BQ2407x (TI bq24072/73/74/75/79) charge safety timer, TMR pin.
//
// Datasheet (TI "bq24072, bq24073, bq24074, bq24075, bq24079", timer section):
//   tPRECHG  = KTMR × RTMR
//   tMAXCHG  = 10 × KTMR × RTMR         KTMR = 48 s/kΩ
// with the programming range RTMR = 18 kΩ .. 72 kΩ. TMR left floating selects
// the internal default timers; TMR tied to VSS disables the timers. Both of
// those are documented modes and stay silent here; the finding is a resistor
// that lands OUTSIDE the programming band, where the timer setting is not
// defined by the datasheet at all. That is the classic decode fault: every
// net connects, the resistor is a perfectly good resistor, and the charge
// safety timer is 10× off the design intent (a battery that terminates
// charge after ~38 minutes instead of ~6 hours on a real board's 4.7 kΩ-for-
// 47 kΩ typo), which no value/short sweep and no SPICE deck can see.
// ─────────────────────────────────────────────────────────────────────────────

/// Seconds per kΩ of TMR resistance for the precharge timer; fast charge is
/// 10×. From the bq2407x datasheet electrical characteristics.
const BQ2407X_KTMR_S_PER_KOHM: f64 = 48.0;
/// The documented TMR programming band (Ω).
const BQ2407X_TMR_MIN_OHMS: f64 = 18_000.0;
const BQ2407X_TMR_MAX_OHMS: f64 = 72_000.0;
/// Below this the strap reads as the documented "timers disabled" tie to VSS
/// rather than a mis-programmed value.
const BQ2407X_TMR_DISABLE_OHMS: f64 = 100.0;
/// Table 7-1 and Section 9.3.6 document this fixed TS-to-VSS value for an
/// application that intentionally does not use battery temperature monitoring.
const BQ2407X_TS_DISABLE_OHMS: f64 = 10_000.0;

/// Does this component's value / MPN positively identify it as a `BQ2407x`
/// charger? Covers bq24072/73/74/75/79 and the -Q1 automotive variants.
fn is_bq2407x(c: &Component) -> bool {
    let v = c.value.to_ascii_uppercase().replace([' ', '-'], "");
    let l = c.lib_id.to_ascii_uppercase().replace([' ', '-'], "");
    v.contains("BQ2407") || l.contains("BQ2407")
}

/// Decode the TMR programming resistor of one identified `BQ2407x`.
fn check_bq2407x_tmr(board: &ExtractedBoard, u: &Component, report: &mut NetLintReport) {
    let Some((tmr_id, tmr_name)) = pin_net_for_role(board, u, "TMR") else {
        return; // pin not resolvable: silent, per the zero-false-positive bar
    };
    let fd = resolve_fixed_divider(board, tmr_id);
    let Some((r_ref, ohms)) = fd.r_pd else {
        return; // floating (internal defaults) or unparseable: silent
    };
    if ohms <= BQ2407X_TMR_DISABLE_OHMS {
        return; // documented "timers disabled" strap
    }
    if (BQ2407X_TMR_MIN_OHMS..=BQ2407X_TMR_MAX_OHMS).contains(&ohms) {
        return; // in-band: a legal programmed value
    }
    let naive_maxchg_min = 10.0 * BQ2407X_KTMR_S_PER_KOHM * (ohms / 1000.0) / 60.0;
    let band_lo_h = 10.0 * BQ2407X_KTMR_S_PER_KOHM * (BQ2407X_TMR_MIN_OHMS / 1000.0) / 3600.0;
    let band_hi_h = 10.0 * BQ2407X_KTMR_S_PER_KOHM * (BQ2407X_TMR_MAX_OHMS / 1000.0) / 3600.0;
    report.findings.push(LintFinding {
        check: LintCheck::DeviceDecode,
        severity: Severity::Medium,
        message: format!(
            "{} charge safety timer: TMR programming resistor {r_ref} = {} lands outside \
             the datasheet's 18k-72k programming band (that band spans tMAXCHG \
             {band_lo_h:.1}-{band_hi_h:.1} h at 48 s/kOhm). Taken at face value it would \
             be ~{naive_maxchg_min:.0} min of fast-charge before the charger gives up, and \
             below/above the band the behavior is not defined by the datasheet at all; a \
             battery that never finishes charging looks exactly like this. Fit an in-band \
             value, tie TMR to VSS to disable the timers deliberately, or leave it open \
             for the internal defaults",
            u.reference,
            format_ohms(ohms),
        ),
        refs: vec![u.reference.clone(), r_ref],
        nets: vec![tmr_name.to_string()],
    });
}

/// A source-classified fixed resistor with a concrete ohmic value. Unlike the
/// older TMR helper, this rung asks the shared part classifier first so a
/// capacitor or ferrite in an R-numbered slot cannot be promoted to evidence.
fn fixed_resistor_ohms(c: &Component) -> Option<f64> {
    if !AssemblyState::of(c).is_present() || !is_source_classified_resistor(c) {
        return None;
    }
    let connected = c.pins.iter().filter(|pin| pin.net.is_some()).count();
    if connected != 2 {
        return None;
    }
    parse_value(&c.value)
        .map(|parsed| parsed.si)
        .filter(|ohms| ohms.is_finite() && *ohms >= 0.0)
}

/// Thermistors have no first-class `PassiveClass` yet, so retain only explicit
/// identity evidence: the conventional RT reference, a thermistor/NTC token in
/// the library/value/footprint, or the datasheet's named 103AT family token.
/// A bare numeric resistor value is intentionally insufficient.
fn is_thermistor_class(c: &Component) -> bool {
    if !AssemblyState::of(c).is_present() {
        return false;
    }
    let reference = c.reference.trim().to_ascii_uppercase();
    let identity = format!("{} {} {}", c.value, c.lib_id, c.footprint).to_ascii_lowercase();
    reference.starts_with("RT")
        || identity.contains("thermistor")
        || identity.contains("ntc")
        || identity.contains("103at")
}

fn ts_abstention(
    u: &Component,
    ts_name: &str,
    reason: impl AsRef<str>,
    report: &mut NetLintReport,
) {
    report.findings.push(LintFinding {
        check: LintCheck::DeviceDecode,
        severity: Severity::Low,
        message: format!(
            "{} TS thermistor network abstained on '{}': {}; unlock: identify the battery-pack thermistor part or reduce TS to one source-classified two-terminal element",
            u.reference,
            ts_name,
            reason.as_ref(),
        ),
        refs: vec![u.reference.clone()],
        nets: vec![ts_name.to_string()],
    });
}

/// Decode the `BQ2407x` TS network using the same positive-identity ladder as the
/// TMR decoder. TI SLUS810N Table 7-1 maps TS to pin 1 and Section 9.3.6 says an
/// NTC in the battery pack provides over-temperature protection. A fixed 10k
/// TS-to-VSS resistor is the explicitly documented "TS function not used"
/// strap and therefore stays silent; another single fixed resistor has enough
/// evidence to say the temperature-dependent protection is absent. Everything
/// more complex abstains and names the evidence needed to decide it.
fn check_bq2407x_ts(board: &ExtractedBoard, u: &Component, report: &mut NetLintReport) {
    let Some((ts_id, ts_name)) = pin_net_for_role(board, u, "TS") else {
        return;
    };

    let mut peers: BTreeMap<&str, &Component> = BTreeMap::new();
    for (component, _) in board.net_members(ts_id) {
        if component.reference != u.reference && AssemblyState::of(component).is_present() {
            peers
                .entry(component.reference.as_str())
                .or_insert(component);
        }
    }

    if peers
        .values()
        .any(|component| is_thermistor_class(component))
    {
        return;
    }

    if peers.len() != 1 {
        ts_abstention(
            u,
            ts_name,
            format!(
                "the net has {} fitted non-thermistor components, so its temperature response is not judgeable",
                peers.len()
            ),
            report,
        );
        return;
    }

    let component = *peers.values().next().expect("length checked");
    let Some(ohms) = fixed_resistor_ohms(component) else {
        ts_abstention(
            u,
            ts_name,
            format!(
                "{} is not a parseable source-classified fixed resistor or thermistor",
                component.reference
            ),
            report,
        );
        return;
    };
    let mut far_nets = BTreeSet::new();
    for pin in &component.pins {
        if let Some(net_id) = pin.net.filter(|net_id| *net_id != ts_id) {
            far_nets.insert(net_id);
        }
    }
    if far_nets.len() != 1 {
        ts_abstention(
            u,
            ts_name,
            format!(
                "{} does not resolve to one far-side net",
                component.reference
            ),
            report,
        );
        return;
    }
    let far_id = *far_nets.iter().next().expect("length checked");
    let Some(far_net) = board.net(far_id) else {
        ts_abstention(u, ts_name, "the resistor's far-side net is missing", report);
        return;
    };
    if !is_ground_name(&far_net.name) {
        ts_abstention(
            u,
            ts_name,
            format!(
                "{} returns to '{}' rather than VSS",
                component.reference, far_net.name
            ),
            report,
        );
        return;
    }

    if (ohms - BQ2407X_TS_DISABLE_OHMS).abs() <= f64::EPSILON * BQ2407X_TS_DISABLE_OHMS {
        return;
    }

    report.findings.push(LintFinding {
        check: LintCheck::DeviceDecode,
        severity: Severity::Medium,
        message: format!(
            "{} TS thermistor protection: fixed resistor {} = {} from TS to VSS is not the datasheet-documented 10k disable strap and contains no thermistor-class part, so it defeats battery-pack over-temperature protection; TI BQ2407x datasheet SLUS810N, Section 9.3.6 Battery Pack Temperature Monitoring. Fit the battery-pack NTC network, or use the documented 10k TS-to-VSS strap only when temperature monitoring is intentionally disabled",
            u.reference,
            component.reference,
            format_ohms(ohms),
        ),
        refs: vec![u.reference.clone(), component.reference.clone()],
        nets: vec![ts_name.to_string()],
    });
}

/// Human ohms: 4700 -> "4.7k", 470 -> "470", 0.001 -> "0.001", 1e-6 -> "1.00e-6".
/// A value under one ohm keeps its significant figures: it is never rounded to
/// "0", which would read as a short.
pub(crate) fn format_ohms(ohms: f64) -> String {
    if ohms >= 1_000_000.0 {
        format!("{:.1}M", ohms / 1_000_000.0)
    } else if ohms >= 1_000.0 {
        format!("{:.1}k", ohms / 1_000.0)
    } else if ohms >= 0.9995 || ohms <= 0.0 || !ohms.is_finite() {
        format!("{ohms:.0}")
    } else if ohms >= 1e-3 {
        format!("{ohms:.3}")
    } else {
        format!("{ohms:.2e}")
    }
}

/// Decode a CYPD3177's `VBUS_MAX` (with its rotary selector, if resolvable) and
/// `VBUS_MIN` dividers, and flag the documented failure modes.
fn check_cypd3177(board: &ExtractedBoard, u: &Component, report: &mut NetLintReport) {
    // ── VBUS_MAX ────────────────────────────────────────────────────────────
    let max = pin_net_for_role(board, u, "VBUS_MAX");
    let min = pin_net_for_role(board, u, "VBUS_MIN");

    // Resolve VBUS_MAX's reachable bands (permanent divider + each detent).
    let mut max_band: Option<PdVolts> = None; // the TOP reachable band (open detent)
    let mut reachable: Vec<(PdVolts, String)> = Vec::new();
    if let Some((max_id, max_name)) = max {
        let fd = resolve_fixed_divider(board, max_id);
        // Need at least a pull-up and the permanent pull-down to compute anything.
        if let (Some((_pur, rpu)), Some((_pdr, rpd))) = (&fd.r_pu, &fd.r_pd) {
            let detents = enumerate_detents(board, max_id);
            if detents.is_empty() {
                // No selector resolvable: decode the static divider alone.
                let mv = divider_mv(*rpu, *rpd);
                let b = decode_table2(mv);
                max_band = Some(b);
                reachable.push((b, format!("static {mv} mV")));
            } else {
                for d in &detents {
                    let eff_pd = d.extra_pd.map_or(*rpd, |extra| parallel(*rpd, extra));
                    let mv = divider_mv(*rpu, eff_pd);
                    let b = decode_table2(mv);
                    reachable.push((b, format!("{} -> {mv} mV", d.label)));
                }
                // The "open" detent (only the permanent divider) is the top of
                // the dial; use it as the nominal max for the Note-1 comparison.
                let open_mv = divider_mv(*rpu, *rpd);
                max_band = Some(decode_table2(open_mv));

                // Flag a SELECTOR that cannot reach the part's top bands: the
                // headline capability is unreachable. This only applies to a real
                // multi-detent selector, a plain fixed divider decodes to exactly
                // ONE band by construction, so it can NEVER reach both 15 V and
                // 20 V, and firing the "selector cannot reach" finding on it was
                // a false positive on every correct fixed-voltage sink.
                let reaches_20 = reachable.iter().any(|(b, _)| *b == PdVolts::V20);
                let reaches_15 = reachable.iter().any(|(b, _)| *b == PdVolts::V15);
                if !reachable.is_empty() && (!reaches_20 || !reaches_15) {
                    let bands: Vec<String> = reachable
                        .iter()
                        .map(|(b, why)| format!("{}V [{why}]", b.volts()))
                        .collect();
                    let mut missing = Vec::new();
                    if !reaches_15 {
                        missing.push("15V");
                    }
                    if !reaches_20 {
                        missing.push("20V");
                    }
                    report.findings.push(LintFinding {
                        check: LintCheck::DeviceDecode,
                        severity: Severity::Medium,
                        message: format!(
                            "{} VBUS_MAX selector on net '{max_name}' decodes (Table 2) to {{{}}}, but cannot reach {{{}}}: the divider's permanent pull-down plus switched-parallel legs cannot reproduce the datasheet's per-detent codes, so the top setting(s) are unreachable",
                            u.reference,
                            bands.join(", "),
                            missing.join(", "),
                        ),
                        refs: vec![u.reference.clone()],
                        nets: vec![max_name.to_string()],
                    });
                }
            }
        }
    }

    // ── VBUS_MIN and Note-1 consistency ─────────────────────────────────────
    if let Some((min_id, min_name)) = min {
        let fd = resolve_fixed_divider(board, min_id);
        if let (Some((_pur, rpu)), Some((_pdr, rpd))) = (&fd.r_pu, &fd.r_pd) {
            let mv = divider_mv(*rpu, *rpd);
            let min_band = decode_table2(mv);
            // Note 1: if VBUS_MIN > VBUS_MAX, VBUS_MAX is used as both min and
            // max, so for every reachable detent whose VBUS_MAX band is BELOW the
            // hard-wired VBUS_MIN, the selector is silently defeated (the part
            // requests VBUS_MAX, not the labelled higher voltage). We fire when at
            // least one reachable detent is clamped, comparing against the lowest
            // reachable VBUS_MAX band so we catch the defeat even when the top
            // (open) detent happens to equal VBUS_MIN.
            let clamped = reachable
                .iter()
                .filter(|(b, _)| min_band.volts() > b.volts())
                .count();
            if max_band.is_some() && clamped > 0 {
                let top = max_band.map_or(0, PdVolts::volts);
                report.findings.push(LintFinding {
                    check: LintCheck::DeviceDecode,
                    severity: Severity::High,
                    message: format!(
                        "{} VBUS_MIN net '{min_name}' decodes (Table 2) to {}V ({mv} mV), GREATER than {clamped} of the VBUS_MAX selector's reachable detents (top band {top}V): per EZ-PD BCR datasheet Note 1 (VBUS_MIN > VBUS_MAX), VBUS_MAX is used as both minimum and maximum, so the hard-wired VBUS_MIN silently overrides and defeats those selector positions",
                        u.reference,
                        min_band.volts(),
                    ),
                    refs: vec![u.reference.clone()],
                    nets: vec![min_name.to_string()],
                });
            }
        }
    }
}

/// Parse a synthetic board and run it through `device_decode_lint` with the
/// built-in model library. Shared by the `tests`, `bq2407x_tests` and
/// `tps25982_tests` modules below, which each fed an identical board through
/// an identically-named local copy of this helper.
#[cfg(test)]
fn lint(board_text: &str) -> NetLintReport {
    let board = ExtractedBoard::from_kicad_pcb(board_text).expect("board parses");
    device_decode_lint(&board, &ModelLibrary::builtin())
}

#[cfg(test)]
mod tests {
    use super::*;

    const V33: &str = r#"(net 5 "+3.3V")"#;
    const GND: &str = r#"(net 1 "GND")"#;
    const VMIN: &str = r#"(net 20 "Net-(U1-VBUS_MIN)")"#;
    const VMAX: &str = r#"(net 21 "VBUS_max")"#;
    const R15P: &str = r#"(net 24 "Net-(R15-Pad1)")"#;
    const R16P: &str = r#"(net 25 "Net-(R16-Pad1)")"#;

    /// A two-pad `R` module at (110, `y`) from net `a` to net `b`; `attr` follows
    /// the value text (the DNP attribute, or nothing).
    fn res(reference: &str, value: &str, y: u32, a: &str, b: &str, attr: &str) -> String {
        format!(
            r"  (module R (layer F.Cu) (at 110 {y})
    (fp_text reference {reference} (at 0 0) (layer F.SilkS))
    (fp_text value {value} (at 0 2) (layer F.Fab)){attr}
    (pad 1 smd rect (at 0 0) {a})
    (pad 2 smd rect (at 2 0) {b})
  )
"
        )
    }

    // ── 1. Pure Table-2 decode against the band edges ───────────────────────
    #[test]
    fn table2_band_edges_decode() {
        let cases: [(i64, PdVolts); 11] = [
            (0, PdVolts::V5),
            (248, PdVolts::V5),
            (249, PdVolts::V9),
            (786, PdVolts::V9),
            (787, PdVolts::V12),
            (1347, PdVolts::V12),
            (1348, PdVolts::V15),
            (1920, PdVolts::V15),
            (1921, PdVolts::V19),
            (2778, PdVolts::V19),
            (2779, PdVolts::V20),
        ];
        for (mv, want) in cases {
            assert_eq!(decode_table2(mv), want, "decode {mv} mV");
        }
    }

    // ── 2. The five hand-derived detent voltages decode as the hunt states ──
    //    proving detent 4 ("15 V") mis-decodes to 12 V and detent 5 ("20 V")
    //    mis-decodes to 19 V.
    #[test]
    fn hunt_detent_voltages_decode() {
        let cases: [(i64, PdVolts, &str); 5] = [
            (0, PdVolts::V5, "detent1 5V"),
            (499, PdVolts::V9, "detent2 9V"),
            (908, PdVolts::V12, "detent3 12V"),
            (1315, PdVolts::V12, "detent4 labelled 15V -> 12V WRONG"),
            (2185, PdVolts::V19, "detent5 labelled 20V -> 19V WRONG"),
        ];
        for (mv, want, why) in cases {
            assert_eq!(decode_table2(mv), want, "{why}");
        }
    }

    // ── 3. Divider-voltage computation matches the hunt's hand math (~2 mV) ──
    #[test]
    fn divider_voltage_matches_hand_math() {
        let rpu = 5100.0;
        let r12 = 10_000.0;
        // detent5 open: permanent R12 alone -> 2185 mV
        assert!((divider_mv(rpu, r12) - 2185).abs() <= 2, "open leg");
        // detent4: R12 || 5.1k = 3400 -> 1315 mV; detent3: R12 || 2.4k -> 908 mV;
        // detent2: R12 || 1k -> 499 mV.
        for (leg, want_mv) in [(5100.0, 1315), (2400.0, 908), (1000.0, 499)] {
            let got = divider_mv(rpu, parallel(r12, leg));
            assert!((got - want_mv).abs() <= 2, "{leg} ohm leg: {got} mV");
        }
        // detent1: direct GND (0 ohm) -> 0 mV
        assert_eq!(divider_mv(rpu, parallel(r12, 0.0)), 0, "GND-direct leg");
    }

    // ── 4a. Synthetic board: VBUS_MIN hard-wired to 19V band while VBUS_MAX
    //        tops out below it -> the Note-1 override finding must fire. ──────
    fn cypd_board(min_pd: &str, max_extra_leg_pd: &str, dnp_r9_r12: bool) -> String {
        // U1 CYPD3177: pad 1 = VBUS_MIN, pad 2 = VBUS_MAX.
        // VBUS_MAX net 21: R11 5.1k pull-up to +3.3V (net 5), R12 10k pull-down to
        //   GND (net 1), plus a slide switch SW1 whose common (pad 6) is on net 21
        //   and whose pad 4 reaches GND via R15 (`max_extra_leg_pd`), pad 5 open.
        // VBUS_MIN net 20: R9 5.1k pull-up to +3.3V, R10 `min_pd` pull-down to GND.
        // `dnp_r9_r12` gives R9 and R12 the KiCad DNP attribute.
        let dnp = if dnp_r9_r12 {
            "\n    (attr smd exclude_from_bom dnp)"
        } else {
            ""
        };
        format!(
            r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 5 "+3.3V")
  (net 20 "Net-(U1-VBUS_MIN)")
  (net 21 "VBUS_max")
  (net 24 "Net-(R15-Pad1)")
  (net 31 "unconnected-(SW1-Pad5)")
  (module Package_DFN_QFN:QFN24 (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value CYPD3177-24LQ (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 20 "Net-(U1-VBUS_MIN)") (pinfunction "VBUS_MIN"))
    (pad 2 smd rect (at 0 1) (net 21 "VBUS_max") (pinfunction "VBUS_MAX"))
  )
{r9}{r10}{r11}{r12}{r15}
  (module SW (layer F.Cu) (at 120 100)
    (fp_text reference SW1 (at 0 0) (layer F.SilkS))
    (fp_text value SS-10-15SPE (at 0 2) (layer F.Fab))
    (pad 4 smd rect (at 0 0) (net 24 "Net-(R15-Pad1)"))
    (pad 5 smd rect (at 1 0) (net 31 "unconnected-(SW1-Pad5)"))
    (pad 6 smd rect (at 2 0) (net 21 "VBUS_max"))
  )
)"#,
            r9 = res("R9", "5k1", 100, V33, VMIN, dnp),
            r10 = res("R10", min_pd, 102, VMIN, GND, ""),
            r11 = res("R11", "5k1", 104, V33, VMAX, ""),
            r12 = res("R12", "10k", 106, VMAX, GND, dnp),
            r15 = res("R15", max_extra_leg_pd, 108, R15P, GND, ""),
        )
    }

    #[test]
    fn device_decode_findings_flow_through_engine_lint() {
        // Chokepoint fix: device_decode must run INSIDE engine_lint so
        // --check/--json/TUI/frontdoor surface these faults, not only --lint.
        let text = cypd_board("10k", "5k1", false);
        let board = ExtractedBoard::from_kicad_pcb(&text).expect("parse synthetic board");
        let lib = ModelLibrary::builtin();
        assert!(
            device_decode_lint(&board, &lib)
                .of_check(LintCheck::DeviceDecode)
                .next()
                .is_some(),
            "fixture should produce a device_decode finding"
        );
        let full = crate::checks::engine_lint(&board, &lib);
        assert!(
            full.of_check(LintCheck::DeviceDecode).next().is_some(),
            "engine_lint must include device_decode findings via the chokepoint"
        );
    }

    #[test]
    fn note1_override_fires_when_min_exceeds_max() {
        // VBUS_MIN = 10k pull-down -> 2185 mV -> 19V band.
        // VBUS_MAX: permanent 10k, switched leg R15 = 5.1k. Open detent -> 2185 mV
        //   (19V) too -- but with R12 permanent the dial tops at the 19V band, and
        //   its lower detents (5.1k leg -> 1315 mV = 12V) sit below VBUS_MIN. The
        //   top reachable band equals 19V, and the 15V/20V finding plus the Note-1
        //   finding both apply. We assert the Note-1 (High) finding fires.
        let r = lint(&cypd_board("10k", "5k1", false));
        let note1: Vec<_> = r
            .of_check(LintCheck::DeviceDecode)
            .filter(|f| matches!(f.severity, Severity::High))
            .collect();
        assert_eq!(note1.len(), 1, "exactly one Note-1 override finding");
        assert!(note1[0].message.contains("Note 1"));
        assert!(note1[0].message.contains("VBUS_MIN"));
        assert!(note1[0].nets.iter().any(|n| n.contains("VBUS_MIN")));
    }

    #[test]
    fn unreachable_top_band_fires_medium() {
        // The selector cannot reach 20V (no detent grounds the pull-up / opens the
        // pull-down to give >= 2779 mV), so the Medium "unreachable top band"
        // finding fires regardless of the Note-1 case.
        let r = lint(&cypd_board("10k", "5k1", false));
        let med: Vec<_> = r
            .of_check(LintCheck::DeviceDecode)
            .filter(|f| matches!(f.severity, Severity::Medium))
            .collect();
        assert_eq!(med.len(), 1, "exactly one unreachable-top-band finding");
        assert!(med[0].message.contains("VBUS_MAX"));
        assert!(med[0].message.contains("20V"));
    }

    #[test]
    fn fixed_voltage_sink_without_a_selector_is_silent() {
        // R13: a CYPD3177 whose VBUS_MAX is a plain fixed pull-up + pull-down
        // with NO selector switch decodes to exactly ONE band and can never
        // reach both 15 V and 20 V by construction. The "selector cannot reach"
        // reachability gate must NOT fire on it; it applies only to a real
        // multi-detent selector. (This is the primary/correct EZ-PD BCR use.)
        let text = &format!(
            r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 5 "+3.3V")
  (net 21 "VBUS_max")
  (module Package_DFN_QFN:QFN24 (layer F.Cu) (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value CYPD3177-24LQ (at 0 2) (layer F.Fab))
    (pad 2 smd rect (at 0 1) (net 21 "VBUS_max") (pinfunction "VBUS_MAX"))
  )
{r11}{r12}
)"#,
            r11 = res("R11", "5k1", 104, V33, VMAX, ""),
            r12 = res("R12", "10k", 106, VMAX, GND, ""),
        );
        assert_eq!(
            lint(text).of_check(LintCheck::DeviceDecode).count(),
            0,
            "a fixed-voltage (no-selector) CYPD3177 must not trip the selector-reachability finding"
        );
    }

    // ── 4b. Clean config: VBUS_MIN low (5V band) and VBUS_MAX able to reach
    //        both 15V and 20V -> the check is SILENT (zero false positive). ───
    #[test]
    fn clean_config_is_silent() {
        // VBUS_MIN pull-down 100k -> 3.3*100/105.1 = 3140 mV -> 20V band? No: we
        // want VBUS_MIN LOW. Use a large pull-UP-dominant divider instead: make
        // VBUS_MIN decode to the 5V band by a tiny pull-down (0R -> 0 mV).
        // And make VBUS_MAX reach 15V and 20V: a switch leg that opens the
        // pull-down (giving a high node) plus a direct-ground detent. We model a
        // single-pull-down-per-detent correct design: NO permanent R12 (so the
        // open detent gives the full 3.3V = 20V), and a leg that yields the 15V
        // band.
        let text = &format!(
            r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 5 "+3.3V")
  (net 20 "Net-(U1-VBUS_MIN)")
  (net 21 "VBUS_max")
  (net 24 "Net-(R15-Pad1)")
  (net 25 "Net-(R16-Pad1)")
  (module Package_DFN_QFN:QFN24 (layer F.Cu) (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value CYPD3177-24LQ (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 20 "Net-(U1-VBUS_MIN)") (pinfunction "VBUS_MIN"))
    (pad 2 smd rect (at 0 1) (net 21 "VBUS_max") (pinfunction "VBUS_MAX"))
  )
{r9}{r10}{r11}{r15}{r16}
  (module SW (layer F.Cu) (at 120 100)
    (fp_text reference SW1 (at 0 0) (layer F.SilkS))
    (fp_text value ROTARY (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 1 "GND"))
    (pad 3 smd rect (at 1 0) (net 25 "Net-(R16-Pad1)"))
    (pad 4 smd rect (at 2 0) (net 24 "Net-(R15-Pad1)"))
    (pad 6 smd rect (at 3 0) (net 21 "VBUS_max"))
  )
)"#,
            r9 = res("R9", "100k", 100, V33, VMIN, ""),
            r10 = res("R10", "1k", 102, VMIN, GND, ""),
            r11 = res("R11", "5k1", 104, V33, VMAX, ""),
            r15 = res("R15", "5k1", 106, R15P, GND, ""),
            r16 = res("R16", "1k", 110, R16P, GND, ""),
        );
        // Here VBUS_MAX has NO permanent pull-down (no R12). resolve_fixed_divider
        // finds a pull-up (R11) but NO pull-down -> the check cannot compute the
        // static divider, so per zero-false-positive discipline it stays silent on
        // VBUS_MAX. VBUS_MIN resolves (100k/1k -> ~33 mV = 5V band) but with no
        // VBUS_MAX band to compare, Note-1 cannot fire. Net result: silent.
        let r = lint(text);
        assert_eq!(
            r.of_check(LintCheck::DeviceDecode).count(),
            0,
            "a config the check cannot fault (or cannot fully resolve) stays silent"
        );
    }

    // ── Zero-false-positive: a non-CYPD board is never touched. ──────────────
    #[test]
    fn non_cypd_part_is_ignored() {
        let text = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 5 "+3.3V")
  (net 21 "VBUS_max")
  (module QFN (layer F.Cu) (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value STM32F103 (at 0 2) (layer F.Fab))
    (pad 2 smd rect (at 0 1) (net 21 "VBUS_max") (pinfunction "VBUS_MAX"))
  )
)"#;
        assert_eq!(lint(text).of_check(LintCheck::DeviceDecode).count(), 0);
    }

    // ── DNP-awareness (the real pd-sink board's actual state) ────────────────
    // On the INGBZGMBH board the permanent VBUS_MAX pull-down (R12) and the
    // VBUS_MIN pull-up (R9) are marked Do-Not-Populate in BOTH the schematic and
    // the .kicad_pcb (`(attr smd exclude_from_bom dnp)`). With R12 unpopulated the
    // VBUS_MAX divider has no static pull-down, so it cannot be computed as a
    // fixed divider and the check stays SILENT. This is correct: a check that
    // ignored DNP and fired here would be a false positive. (See the report: this
    // also means the hunt doc's "permanent R12 always present" premise does not
    // match the as-built files.)
    #[test]
    fn dnp_permanent_pulldown_makes_check_silent() {
        // Same as the faulting board, but R12 (VBUS_MAX permanent pull-down) and
        // R9 (VBUS_MIN pull-up) carry the KiCad DNP attribute.
        let text = cypd_board("10k", "5k1", true);
        assert_eq!(
            lint(&text).of_check(LintCheck::DeviceDecode).count(),
            0,
            "with the permanent pull-down DNP the static divider is uncomputable; the check must stay silent"
        );
    }
}

#[cfg(test)]
mod program_domain_tests {
    use super::*;

    /// An MCP73831 linear charger (SOT-23-5: 1 STAT, 2 VSS, 3 VBAT, 4 VDD,
    /// 5 PROG) with `prog` as its PROG resistor to ground. The card's law is
    /// `I_REG = 1000 V / R_PROG` with a 0.5 A programmed-operation ceiling.
    fn charger_board(value: &str, prog: &str) -> String {
        format!(
            r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "VBUS")
  (net 3 "VBAT")
  (net 4 "/charger/PROG")
  (net 5 "/charger/STAT")
  (module Package_TO_SOT_SMD:SOT-23-5 (layer F.Cu) (at 100 100)
    (fp_text reference U3 (at 0 0) (layer F.SilkS))
    (fp_text value "{value}" (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 5 "/charger/STAT"))
    (pad 2 smd rect (at 0 1) (net 1 "GND"))
    (pad 3 smd rect (at 0 2) (net 3 "VBAT"))
    (pad 4 smd rect (at 1 0) (net 2 "VBUS"))
    (pad 5 smd rect (at 1 1) (net 4 "/charger/PROG"))
  )
  (module Resistor_SMD:R_0603 (layer F.Cu) (at 110 100)
    (fp_text reference R8 (at 0 0) (layer F.SilkS))
    (fp_text value "{prog}" (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 4 "/charger/PROG"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )
)
"#
        )
    }

    /// Faulty input: 200 ohm asks 5 A of a part specified to 0.5 A.
    #[test]
    fn prog_resistor_a_decade_low_fires_with_the_ceiling_and_the_fix() {
        let report = lint(&charger_board("MCP73831T-2ACI/OT", "200"));
        let f: Vec<_> = report.of_check(LintCheck::DeviceDecode).collect();
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].severity, Severity::Medium);
        assert!(f[0].refs.contains(&"U3".to_string()) && f[0].refs.contains(&"R8".to_string()));
        assert_eq!(f[0].nets, vec!["/charger/PROG".to_string()]);
        assert!(
            f[0].message.contains("5 A") && f[0].message.contains("500 mA"),
            "{}",
            f[0].message
        );
        assert!(
            f[0].message.contains("at least 2.0k"),
            "the remedy names the minimum: {}",
            f[0].message
        );
    }

    /// Legal controls: the datasheet's 2k (exactly the ceiling), the E96 1.91k
    /// inside the grace, and an ordinary 10k all stay silent.
    #[test]
    fn in_range_prog_resistors_stay_silent() {
        for prog in ["2k", "1.91k", "10k", "100k"] {
            let report = lint(&charger_board("MCP73831T-2ACI/OT", prog));
            assert_eq!(
                report.of_check(LintCheck::DeviceDecode).count(),
                0,
                "PROG = {prog} must stay silent: {:?}",
                report.findings
            );
        }
    }

    /// The one parameter that decides this check cannot be read: the check
    /// abstains by name (low, not judged, a remedy) and never reads as a clean
    /// pass. An unparseable value is such a case.
    #[test]
    fn an_unreadable_prog_resistor_abstains_by_name() {
        for prog in ["RC0603", "1 000", "1_000"] {
            let report = lint(&charger_board("MCP73831T-2ACI/OT", prog));
            let f: Vec<_> = report.of_check(LintCheck::DeviceDecode).collect();
            assert_eq!(f.len(), 1, "PROG = {prog} must abstain once: {f:?}");
            assert_eq!(f[0].severity, Severity::Low, "{}", f[0].message);
            assert!(f[0].is_abstention(), "{}", f[0].message);
            assert!(
                hauksbee_models::rating_basis::is_not_judged(&f[0].message),
                "the verdict reads it as not judged: {}",
                f[0].message
            );
            assert!(
                f[0].message.contains("U3") && f[0].message.contains("prog pin"),
                "names the part and the pin: {}",
                f[0].message
            );
            assert!(f[0].own_remedy().is_some(), "{}", f[0].message);
        }
    }

    /// A single-pad part (a test point) joined to the PROG net is not a
    /// programming element: the 2k resistor still reads, in range and silent,
    /// and a too-small resistor still fires with the test point present.
    fn with_test_point(board: &str) -> String {
        let tp = r#"  (module TestPoint:TestPoint_Pad_D1.0mm (layer F.Cu) (at 140 100)
    (fp_text reference TP1 (at 0 0) (layer F.SilkS))
    (fp_text value TP (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 4 "/charger/PROG"))
  )
"#;
        let end = board.rfind(')').expect("closing paren");
        format!("{}{tp})\n", &board[..end])
    }

    #[test]
    fn a_single_pad_test_point_on_the_prog_net_is_not_a_false_alarm() {
        let clean = lint(&with_test_point(&charger_board("MCP73831T-2ACI/OT", "2k")));
        assert_eq!(
            clean.of_check(LintCheck::DeviceDecode).count(),
            0,
            "{:?}",
            clean.findings
        );
        let fault = lint(&with_test_point(&charger_board("MCP73831T-2ACI/OT", "200")));
        let f: Vec<_> = fault.of_check(LintCheck::DeviceDecode).collect();
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].severity, Severity::Medium, "{}", f[0].message);
    }

    /// Floating PROG disables the charger (datasheet), and the file's rule is
    /// that an absent or floating programming pin stays silent: an unfitted
    /// (DNP) resistor, alone or beside a test point, is not an abstention. A
    /// fitted but unreadable member (the duplicate and reference tests below)
    /// still abstains.
    #[test]
    fn a_dnp_prog_resistor_leaves_the_pin_floating_and_silent() {
        let dnp = charger_board("MCP73831T-2ACI/OT", "2k").replace(
            "(fp_text reference R8 (at 0 0) (layer F.SilkS))",
            "(fp_text reference R8 (at 0 0) (layer F.SilkS))\n    (attr smd exclude_from_bom dnp)",
        );
        for board in [dnp.clone(), with_test_point(&dnp)] {
            let report = lint(&board);
            assert_eq!(
                report.of_check(LintCheck::DeviceDecode).count(),
                0,
                "{:?}",
                report.findings
            );
        }
    }

    /// A PROG resistor with one dangling leg (its second pad on no net, or on
    /// KiCad's `unconnected-(..)` placeholder net) is a programming resistor
    /// drawn but not connected: the pin is not floating by design, so the check
    /// abstains (naming the part and pin, and saying no resistor path to ground
    /// is fitted) and the verdict is never a pass. A test point beside it
    /// changes nothing.
    #[test]
    fn a_prog_resistor_with_one_dangling_leg_abstains_by_name() {
        let ground_pad = r#"(pad 2 smd rect (at 2 0) (net 1 "GND"))"#;
        let netless = charger_board("MCP73831T-2ACI/OT", "2k")
            .replace(ground_pad, "(pad 2 smd rect (at 2 0))");
        let placeholder = charger_board("MCP73831T-2ACI/OT", "2k")
            .replacen(
                "(net 0 \"\")",
                "(net 0 \"\")\n  (net 6 \"unconnected-(R8-Pad2)\")",
                1,
            )
            .replace(
                ground_pad,
                r#"(pad 2 smd rect (at 2 0) (net 6 "unconnected-(R8-Pad2)"))"#,
            );
        for board in [
            netless.clone(),
            placeholder.clone(),
            with_test_point(&netless),
            with_test_point(&placeholder),
        ] {
            let report = lint(&board);
            let f: Vec<_> = report.of_check(LintCheck::DeviceDecode).collect();
            assert_eq!(f.len(), 1, "one abstention, not silence: {f:?}");
            assert!(f[0].is_abstention(), "{}", f[0].message);
            assert!(
                hauksbee_models::rating_basis::is_not_judged(&f[0].message),
                "{}",
                f[0].message
            );
            assert!(
                f[0].message.contains("U3") && f[0].message.contains("prog pin"),
                "{}",
                f[0].message
            );
        }
    }

    /// H1: a second part with the same designator makes the PROG resistor
    /// ambiguous, so the check abstains naming R8 and the duplicate, instead
    /// of dropping the finding the single-R8 board gets. The one-resistor
    /// board with the same fault still fires, and the legal control is silent.
    #[test]
    fn a_duplicate_designator_on_the_prog_resistor_abstains() {
        let duplicate = r#"  (module Resistor_SMD:R_0603 (layer F.Cu) (at 120 100)
    (fp_text reference R8 (at 0 0) (layer F.SilkS))
    (fp_text value 1k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 1 "GND"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )
"#;
        let with_duplicate = |prog: &str| {
            let board = charger_board("MCP73831T-2ACI/OT", prog);
            let end = board.rfind(')').expect("closing paren");
            format!("{}{duplicate})\n", &board[..end])
        };
        for prog in ["1k", "10k"] {
            let report = lint(&with_duplicate(prog));
            let f: Vec<_> = report.of_check(LintCheck::DeviceDecode).collect();
            assert_eq!(f.len(), 1, "PROG = {prog}: {f:?}");
            assert!(f[0].is_abstention(), "{}", f[0].message);
            assert!(
                f[0].message.contains("duplicate designator") && f[0].message.contains("R8"),
                "{}",
                f[0].message
            );
        }
        // Control: without the duplicate the 1k fault fires and the 10k is silent.
        let fault = lint(&charger_board("MCP73831T-2ACI/OT", "1k"));
        assert!(fault
            .of_check(LintCheck::DeviceDecode)
            .any(|f| f.severity == Severity::Medium));
        let clean = lint(&charger_board("MCP73831T-2ACI/OT", "10k"));
        assert_eq!(clean.of_check(LintCheck::DeviceDecode).count(), 0);
    }

    /// H3: a reference that does not classify the part as a resistor leaves
    /// the PROG resistance unread, so the check abstains naming it; the same
    /// board with a conventional designator fires or stays silent as it should.
    #[test]
    fn an_unclassifiable_reference_spelling_abstains() {
        for reference in ["R-8", "RES8", "X8", "RN8"] {
            let board = charger_board("MCP73831T-2ACI/OT", "1k").replace("R8", reference);
            let report = lint(&board);
            let f: Vec<_> = report.of_check(LintCheck::DeviceDecode).collect();
            assert_eq!(f.len(), 1, "{reference}: {f:?}");
            assert!(f[0].is_abstention(), "{reference}: {}", f[0].message);
            assert!(
                f[0].message.contains(reference) && f[0].message.contains("not identified"),
                "{}",
                f[0].message
            );
        }
        let ok = lint(&charger_board("MCP73831T-2ACI/OT", "1k"));
        assert!(ok
            .of_check(LintCheck::DeviceDecode)
            .any(|f| f.severity == Severity::Medium));
    }

    /// H2: PROG shorted to ground (a 0 ohm resistor, or the pin on the ground
    /// net) is the extreme of the over-current fault and is reported as one,
    /// like 10 ohm or 1 milliohm. MCP73831: IREG = 1000 V / RPROG, RPROG 2-67
    /// kOhm. A 0 ohm link in series with a real resistor is not a short.
    #[test]
    fn a_prog_pin_shorted_to_ground_is_flagged() {
        for prog in ["0", "0R", "0 ohm"] {
            let report = lint(&charger_board("MCP73831T-2ACI/OT", prog));
            let f: Vec<_> = report.of_check(LintCheck::DeviceDecode).collect();
            assert_eq!(f.len(), 1, "PROG = {prog}: {f:?}");
            assert_eq!(f[0].severity, Severity::Medium, "{}", f[0].message);
            assert!(
                f[0].message.contains("tied to ground")
                    && f[0].message.contains("R8")
                    && f[0].message.contains("at least 2.0k"),
                "{}",
                f[0].message
            );
            assert!(f[0].refs.contains(&"U3".to_string()) && f[0].refs.contains(&"R8".to_string()));
        }
        // The pin on the ground net itself.
        let direct = charger_board("MCP73831T-2ACI/OT", "10k").replace(
            r#"(pad 5 smd rect (at 1 1) (net 4 "/charger/PROG"))"#,
            r#"(pad 5 smd rect (at 1 1) (net 1 "GND"))"#,
        );
        let report = lint(&direct);
        let f: Vec<_> = report.of_check(LintCheck::DeviceDecode).collect();
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].severity, Severity::Medium);
        assert!(f[0].message.contains("directly"), "{}", f[0].message);
        // Control: a 0 ohm link ahead of a real 10k resistor reads 10k, silent.
        let series = charger_board("MCP73831T-2ACI/OT", "0R")
            .replace(
                r#"(pad 2 smd rect (at 2 0) (net 1 "GND"))"#,
                r#"(pad 2 smd rect (at 2 0) (net 6 "/charger/PROGX"))"#,
            )
            .replacen(
                r#"(net 5 "/charger/STAT")"#,
                r#"(net 5 "/charger/STAT")(net 6 "/charger/PROGX")"#,
                1,
            );
        let series = format!(
            "{}  (module Resistor_SMD:R_0603 (layer F.Cu) (at 130 100)\n    (fp_text reference R9 (at 0 0) (layer F.SilkS))\n    (fp_text value 10k (at 0 2) (layer F.Fab))\n    (pad 1 smd rect (at 0 0) (net 6 \"/charger/PROGX\"))\n    (pad 2 smd rect (at 2 0) (net 1 \"GND\"))\n  )\n)\n",
            &series[..series.rfind(')').expect("closing paren")]
        );
        let report = lint(&series);
        assert_eq!(
            report.of_check(LintCheck::DeviceDecode).count(),
            0,
            "{:?}",
            report.findings
        );
    }

    /// N2: a sub-ohm programming resistor is flagged with its value in
    /// significant figures, never printed as "0 ohm"; the absurd equation
    /// current stays readable.
    #[test]
    fn a_sub_ohm_prog_resistor_prints_its_value() {
        let report = lint(&charger_board("MCP73831T-2ACI/OT", "1m"));
        let f: Vec<_> = report.of_check(LintCheck::DeviceDecode).collect();
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(
            f[0].message.contains("= 0.001 ohm") && !f[0].message.contains("= 0 ohm"),
            "{}",
            f[0].message
        );
        assert!(f[0].message.contains("1 MA"), "{}", f[0].message);
        assert_eq!(format_ohms(0.001), "0.001");
        assert_eq!(format_ohms(0.5), "0.500");
        assert_eq!(format_ohms(1e-6), "1.00e-6");
        assert_eq!(format_ohms(0.0), "0");
        assert_eq!(format_ohms(470.0), "470");
        assert_eq!(format_ohms(4700.0), "4.7k");
    }

    /// A part with no programming law is never judged, whatever sits on pin 5.
    #[test]
    fn a_part_without_a_programming_law_stays_silent() {
        let report = lint(&charger_board("74HC1G04", "200"));
        assert_eq!(report.of_check(LintCheck::DeviceDecode).count(), 0);
    }
}

#[cfg(test)]
mod bq2407x_tests {
    use super::*;

    fn bq_board(tmr_value: &str) -> String {
        format!(
            r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 7 "/left/power/TMR")
  (module Package_DFN_QFN:QFN16 (layer F.Cu)
    (at 100 100)
    (fp_text reference U11 (at 0 0) (layer F.SilkS))
    (fp_text value BQ24075RGT (at 0 2) (layer F.Fab))
    (pad 6 smd rect (at 0 0) (net 7 "/left/power/TMR") (pinfunction "TMR"))
    (pad 11 smd rect (at 0 1) (net 1 "GND"))
  )
  (module R (layer F.Cu) (at 110 100)
    (fp_text reference R31 (at 0 0) (layer F.SilkS))
    (fp_text value {tmr_value} (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 7 "/left/power/TMR"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )
)
"#
        )
    }

    /// The seed case: 4.7k where 47k was intended, below the 18-72k band.
    #[test]
    fn out_of_band_tmr_resistor_fires_with_the_timer_math() {
        let report = lint(&bq_board("4.7k"));
        let f: Vec<_> = report.of_check(LintCheck::DeviceDecode).collect();
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].severity, Severity::Medium);
        assert!(
            f[0].message.contains("18k-72k") && f[0].message.contains("R31"),
            "{}",
            f[0].message
        );
        assert!(
            f[0].message.contains("38 min") || f[0].message.contains("~38"),
            "the naive decode is stated: {}",
            f[0].message
        );
    }

    /// The upstream fix value is in-band and silent; so are the documented
    /// floating (internal defaults) and VSS-tie (disabled) modes; so is an
    /// unparseable value (zero-false-positive bar).
    #[test]
    fn documented_modes_and_unresolvable_values_stay_silent() {
        for value in ["47k", "18k", "72k", "0", "RC0402FR-0747KL"] {
            let report = lint(&bq_board(value));
            assert_eq!(
                report.of_check(LintCheck::DeviceDecode).count(),
                0,
                "TMR = {value} must stay silent"
            );
        }
        // Floating: no resistor at all on the pin.
        let floating = bq_board("47k").replace(
            "(net 7 \"/left/power/TMR\")\n    (pad 2",
            "(net 0 \"\")\n    (pad 2",
        );
        // (crude: detach R31 pad 1; the pin net keeps only the charger)
        let report = lint(&floating);
        assert_eq!(report.of_check(LintCheck::DeviceDecode).count(), 0);
    }

    fn bq_ts_board(parts: &str) -> String {
        format!(
            r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 8 "/left/power/TS")
  (net 9 "+3V3")
  (module Package_DFN_QFN:QFN16 (layer F.Cu)
    (at 100 100)
    (fp_text reference U11 (at 0 0) (layer F.SilkS))
    (fp_text value BQ24075RGT (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 8 "/left/power/TS") (pinfunction "TS"))
    (pad 8 smd rect (at 0 1) (net 1 "GND") (pinfunction "VSS"))
  )
{parts})
"#
        )
    }

    fn two_pin_part(
        reference: &str,
        value: &str,
        lib: &str,
        far_net: i64,
        far_name: &str,
    ) -> String {
        format!(
            r#"  (module {lib} (layer F.Cu) (at 110 100)
    (fp_text reference {reference} (at 0 0) (layer F.SilkS))
    (fp_text value {value} (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 8 "/left/power/TS"))
    (pad 2 smd rect (at 2 0) (net {far_net} "{far_name}"))
  )
"#
        )
    }

    #[test]
    fn fixed_non_disable_ts_resistor_fires_with_protection_and_basis() {
        let text = bq_ts_board(&two_pin_part("R33", "4.7k", "Device:R", 1, "GND"));
        let report = lint(&text);
        let findings: Vec<_> = report.of_check(LintCheck::DeviceDecode).collect();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Medium);
        assert!(findings[0]
            .message
            .contains("defeats battery-pack over-temperature protection"));
        assert!(findings[0].message.contains("Section 9.3.6"));
        assert!(findings[0].message.contains("R33 = 4.7k"));
    }

    #[test]
    fn documented_10k_ts_disable_strap_stays_silent() {
        let text = bq_ts_board(&two_pin_part("R33", "10k", "Device:R", 1, "GND"));
        let report = lint(&text);
        assert_eq!(report.of_check(LintCheck::DeviceDecode).count(), 0);
    }

    #[test]
    fn thermistor_class_on_ts_stays_silent() {
        let text = bq_ts_board(&two_pin_part(
            "RT1",
            "10k NTC",
            "Device:Thermistor_NTC",
            1,
            "GND",
        ));
        let report = lint(&text);
        assert_eq!(report.of_check(LintCheck::DeviceDecode).count(), 0);
    }

    #[test]
    fn complex_ts_network_abstains_with_the_exact_unlock() {
        let parts = format!(
            "{}{}",
            two_pin_part("R33", "100k", "Device:R", 1, "GND"),
            two_pin_part("R34", "1k", "Device:R", 9, "+3V3")
        );
        let report = lint(&bq_ts_board(&parts));
        let findings: Vec<_> = report.of_check(LintCheck::DeviceDecode).collect();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Low);
        assert!(findings[0]
            .message
            .contains("TS thermistor network abstained"));
        assert!(findings[0].message.contains(
            "identify the battery-pack thermistor part or reduce TS to one source-classified two-terminal element"
        ));
    }
}

#[cfg(test)]
mod program_range_tests {
    use super::*;

    /// A DRV8871 (whose card states the 15 kOhm minimum ILIM resistor) with
    /// `rilim` from ILIM to ground, or nothing on the pin.
    fn drv_board(rilim: Option<&str>) -> String {
        let resistor = rilim
            .map(|value| {
                format!(
                    r#"  (module Resistor_SMD:R_2512_6332Metric (layer F.Cu)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value {value} (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 4 "Net-(U2-ILIM)"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )
"#
                )
            })
            .unwrap_or_default();
        format!(
            r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+12V")
  (net 4 "Net-(U2-ILIM)")
  (module Package_SO:SOIC-8-1EP_3.9x4.9mm_P1.27mm_EP2.29x3mm (layer F.Cu)
    (fp_text reference U2 (at 0 0) (layer F.SilkS))
    (fp_text value DRV8871DDAR (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 1 "GND"))
    (pad 4 smd rect (at 0 3) (net 4 "Net-(U2-ILIM)"))
    (pad 5 smd rect (at 0 4) (net 2 "+12V"))
    (pad 7 smd rect (at 0 6) (net 1 "GND"))
  )
{resistor})
"#
        )
    }

    fn range_findings(report: &NetLintReport) -> Vec<&LintFinding> {
        report
            .of_check(LintCheck::DeviceDecode)
            .filter(|f| f.severity == Severity::Medium && f.message.contains("programming range"))
            .collect()
    }

    #[test]
    fn a_programming_resistor_below_the_card_minimum_fires() {
        let report = lint(&drv_board(Some("0.1")));
        let hits = range_findings(&report);
        assert_eq!(hits.len(), 1, "{:?}", report.findings);
        assert_eq!(hits[0].refs, vec!["U2".to_string(), "R2".to_string()]);
        assert!(
            hits[0]
                .message
                .contains("= 0.10 ohm is below the 15.0k ohm minimum"),
            "{}",
            hits[0].message
        );
    }

    #[test]
    fn in_range_floating_and_unreadable_programming_resistors_stay_silent() {
        for rilim in [Some("33k"), Some("15k"), None, Some("TBD")] {
            let report = lint(&drv_board(rilim));
            assert!(
                range_findings(&report).is_empty(),
                "{rilim:?}: {:?}",
                report.findings
            );
        }
    }
}

#[cfg(test)]
mod tps25982_tests {
    use super::*;

    fn tps_board(
        part_value: &str,
        rilim_value: Option<&str>,
        connector_net: Option<&str>,
    ) -> String {
        let resistor = rilim_value
            .map(|value| {
                format!(
                    r#"  (module Resistor_SMD:R_0402_1005Metric (layer F.Cu)
    (fp_text reference R48 (at 0 0) (layer F.SilkS))
    (fp_text value {value} (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 4 "Net-(U19-ILIM)"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )
"#
                )
            })
            .unwrap_or_default();
        let connector = connector_net
            .map(|net| {
                let (net_id, name) = if net == "EFUSE_OUT" {
                    (3, "EFUSE_OUT")
                } else {
                    (5, "unconnected-(J8-Pin_1-Pad1)")
                };
                format!(
                    r#"  (module Connector_JST:JST_VH_B2P-VH_1x02_P3.96mm_Vertical (layer F.Cu)
    (fp_text reference J8 (at 0 0) (layer F.SilkS))
    (fp_text value JST_B2P-VH (at 0 2) (layer F.Fab))
    (pad 1 thru_hole rect (at 0 0) (net {net_id} "{name}"))
    (pad 2 thru_hole circle (at 3.96 0) (net 1 "GND"))
  )
"#
                )
            })
            .unwrap_or_default();
        format!(
            r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "EFUSE_IN")
  (net 3 "EFUSE_OUT")
  (net 4 "Net-(U19-ILIM)")
  (net 5 "unconnected-(J8-Pin_1-Pad1)")
  (module Package_DFN_QFN:QFN24 (layer F.Cu)
    (fp_text reference U19 (at 0 0) (layer F.SilkS))
    (fp_text value {part_value} (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 2 "EFUSE_IN") (pinfunction "IN_1"))
    (pad 8 smd rect (at 0 1) (net 4 "Net-(U19-ILIM)") (pinfunction "ILIM_8"))
    (pad 17 smd rect (at 0 2) (net 3 "EFUSE_OUT") (pinfunction "OUT_17"))
    (pad 26 smd rect (at 0 3) (net 1 "GND") (pinfunction "GND_26"))
  )
{resistor}{connector})
"#
        )
    }

    fn connector_budget_warnings(report: &NetLintReport) -> Vec<&LintFinding> {
        report
            .of_check(LintCheck::DeviceDecode)
            .filter(|finding| {
                finding.severity == Severity::Medium && finding.message.contains("connector budget")
            })
            .collect()
    }

    #[test]
    fn connected_10a_connector_100r_fires_with_band_rating_grace_and_fix_math() {
        let report = lint(&tps_board(
            "TPS259824LNRGET",
            Some("100"),
            Some("EFUSE_OUT"),
        ));
        let warnings = connector_budget_warnings(&report);
        assert_eq!(warnings.len(), 1, "{:#?}", report.findings);
        let message = &warnings[0].message;
        assert!(message.contains("12.85/14.71/15.99 A"), "{message}");
        assert!(
            message.contains("10 A") && message.contains("11 A"),
            "{message}"
        );
        assert!(message.contains("10% grace"), "{message}");
        assert!(
            message.contains("at least 117 ohm") && message.contains("129 ohm"),
            "{message}"
        );
        assert_eq!(warnings[0].refs, ["U19", "R48", "J8"]);
    }

    #[test]
    fn connected_10a_connector_127r_is_silent_under_the_ten_percent_grace() {
        let report = lint(&tps_board(
            "TPS259824LNRGET",
            Some("127"),
            Some("EFUSE_OUT"),
        ));
        assert!(
            connector_budget_warnings(&report).is_empty(),
            "{:#?}",
            report.findings
        );
        let notes: Vec<_> = report
            .of_check(LintCheck::DeviceDecode)
            .filter(|finding| finding.severity == Severity::Low)
            .collect();
        assert_eq!(notes.len(), 1, "{:#?}", report.findings);
        assert!(notes[0].message.contains("10.14/11.61/12.62 A"));
    }

    #[test]
    fn no_connected_rating_witness_keeps_only_the_informational_decode() {
        for connector_net in [None, Some("unconnected")] {
            let report = lint(&tps_board("TPS259824LNRGET", Some("100"), connector_net));
            assert!(
                connector_budget_warnings(&report).is_empty(),
                "{:#?}",
                report.findings
            );
            let rows: Vec<_> = report.of_check(LintCheck::DeviceDecode).collect();
            assert_eq!(rows.len(), 1, "{:#?}", report.findings);
            assert_eq!(rows[0].severity, Severity::Low);
            assert!(rows[0].message.contains("informational"));
        }
    }

    #[test]
    fn floating_ilim_is_silent_and_unparseable_ilim_abstains_by_name() {
        let floating = lint(&tps_board("TPS259824LNRGET", None, Some("EFUSE_OUT")));
        assert_eq!(floating.of_check(LintCheck::DeviceDecode).count(), 0);

        let unreadable = lint(&tps_board(
            "TPS259824LNRGET",
            Some("RC0402FR-07100RL"),
            Some("EFUSE_OUT"),
        ));
        let rows: Vec<_> = unreadable.of_check(LintCheck::DeviceDecode).collect();
        assert_eq!(rows.len(), 1, "{:#?}", unreadable.findings);
        assert_eq!(rows[0].severity, Severity::Low);
        assert!(rows[0].is_abstention(), "{}", rows[0].message);
        assert!(
            hauksbee_models::rating_basis::is_not_judged(&rows[0].message),
            "the verdict reads it as not judged: {}",
            rows[0].message
        );
        assert!(rows[0].message.contains("ILIM"));
        assert!(rows[0].message.contains("not a readable resistance"));
        assert!(rows[0].own_remedy().is_some(), "{}", rows[0].message);
    }

    #[test]
    fn illegal_rilim_value_fires_without_claiming_a_budget_decode() {
        let report = lint(&tps_board("TPS259824LNRGET", Some("50"), None));
        let rows: Vec<_> = report.of_check(LintCheck::DeviceDecode).collect();
        assert_eq!(rows.len(), 1, "{:#?}", report.findings);
        assert_eq!(rows[0].severity, Severity::Medium);
        assert!(rows[0].message.contains("82-1650 ohm"));
        assert!(rows[0].message.contains("does not support"));
    }

    #[test]
    fn family_match_includes_only_the_datasheet_comparison_table() {
        for value in [
            "TPS25982",
            "TPS259822LNRGE",
            "TPS259823ONRGET",
            "TPS259824LNRGET",
            "TPS259827ONRGE",
        ] {
            assert!(is_tps25982_token(value), "{value} must match");
        }
        for value in [
            "TPS25980",
            "TPS25981",
            "TPS25985",
            "TPS259820LNRGE",
            "TPS259825LNRGE",
            "TPS259824XNRGE",
            "NOTTPS259824LNRGET",
            "TPS25982FAMILY",
        ] {
            assert!(!is_tps25982_token(value), "{value} must not match");
        }
    }
}
