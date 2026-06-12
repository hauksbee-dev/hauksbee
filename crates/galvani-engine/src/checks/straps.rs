//! Boot strapping-pin lint (static, no firmware needed).
//!
//! An MCU samples certain pins at the reset latch window to choose its boot
//! mode. If a strapping pin is not at the level the part needs during that
//! window, the chip can boot wrong (the textbook case: Olimex ESP32-EVB rev D,
//! where the Ethernet PHY's free-running 50 MHz REF_CLK sits on GPIO0 - a
//! strapping pin that must be a stable HIGH - so the ESP32 randomly enters
//! download mode; ESP-IDF Ethernet API documents this exact failure, and Olimex
//! fixed it in rev E by gating PHY power until the oscillator stabilises).
//!
//! This check reads the per-part strap table from the model db
//! (`[[models.straps]]`, see `crates/galvani-models/db/mcu.toml`) and, for each
//! bound MCU, examines the net each strap pin sits on. It fires only on the
//! structurally unambiguous case, in line with the famous-sweep calibration
//! discipline (zero false positives on known-good boards or the check does not
//! ship, see `docs/KNOWN_FAULTS_VALIDATION.md`):
//!
//!   (a) a *free-running clock source* (a powered oscillator) reaches the strap
//!       net with no strong static pull to override it -> HIGH severity, because
//!       a clock present at the reset latch is the documented failure mode and a
//!       clock is, by definition, free-running at reset;
//!   (b) the strap net is *resistively biased to the wrong level* for normal
//!       boot (a pull-up where the part needs LOW, or a pull-down where it needs
//!       HIGH) -> MEDIUM;
//!   (c) (deliberately NOT fired on "no external bias": every ESP32 strap has a
//!       documented internal pull, so a bare strap pin held by the internal pull
//!       is correct - firing there would be a confident false positive on Watchy
//!       and others. See the calibration note in the docs.)
//!
//! DNP-aware: a Do-Not-Populate resistor is not assembled, so it neither biases
//! nor overrides anything. On a PCB-only extraction (no schematic pin types) the
//! clock-driver detection still works - it keys on the *component* (an
//! oscillator), not a pin electrical type - but the reduced visibility is noted.

use galvani_extract::{
    Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity,
};
use galvani_models::value::parse_value;
use galvani_models::{ModelLibrary, StrapLevel};

use crate::binder::resolve;

/// Run the strap-pin lint over an extracted board, resolving each component
/// against `lib` to find MCUs that carry a strap table.
pub fn strap_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    for comp in &board.components {
        let res = resolve(lib, comp);
        let Some(model) = res.model.as_ref() else {
            continue;
        };
        if model.straps.is_empty() {
            continue;
        }
        // Map each strap role to the pad that carries it, then to the net.
        for strap in &model.straps {
            // Find the pad whose role matches this strap.
            let Some(pad_num) = model
                .pins
                .iter()
                .find(|(_, role)| role.as_str() == strap.role)
                .map(|(pad, _)| pad.clone())
            else {
                continue;
            };
            let Some(pin) = comp.pins.iter().find(|p| p.number == pad_num) else {
                continue; // strap pad not present on this footprint instance
            };
            let Some(net_id) = pin.net else {
                continue; // strap pad unrouted on a PCB-only input
            };
            let Some(net) = board.net(net_id) else {
                continue;
            };
            if is_unconnected_net(&net.name) {
                continue;
            }
            examine_strap(board, comp, strap.role.as_str(), strap.level, &strap.note, net_id, &net.name, &mut report);
        }
    }
    report
}

/// Inspect the members of one strap net and emit a finding if it cannot hold the
/// required level at reset.
#[allow(clippy::too_many_arguments)]
fn examine_strap(
    board: &ExtractedBoard,
    mcu: &Component,
    role: &str,
    level: StrapLevel,
    note: &str,
    net_id: i64,
    net_name: &str,
    report: &mut NetLintReport,
) {
    let members = board.net_members(net_id);

    // (a) A free-running clock source on the strap net. This is the Olimex GPIO0
    //     fault: a powered oscillator reaches the pin (here through a small
    //     series resistor), so a clock is present at the reset latch window. We
    //     look across the net AND one series-resistor hop, because the real
    //     board puts a 10R between the oscillator and GPIO0.
    if let Some(osc) = clock_source_reaching(board, net_id) {
        if osc.reference != mcu.reference {
            report.findings.push(LintFinding {
                check: LintCheck::StrapPin,
                severity: Severity::High,
                message: format!(
                    "{} strap pin {} ({role}) net '{net_name}' carries a free-running clock source {} ({}): a clock present at the reset latch can mis-strap the part ({note})",
                    mcu.reference, role, osc.reference, osc.value
                ),
                refs: vec![mcu.reference.clone(), osc.reference.clone()],
                nets: vec![net_name.to_string()],
            });
            return; // one finding per strap; the clock is the dominant fault
        }
    }

    // (b) A pull resistor biasing the strap to the WRONG level for normal boot.
    //     Only fire when the required level is a hard HIGH or LOW (not the
    //     "defined" case, where either polarity is acceptable for this lint).
    let want_high = matches!(level, StrapLevel::High);
    let want_low = matches!(level, StrapLevel::Low);
    if want_high || want_low {
        if let Some((rref, to_ground)) = wrong_pull(board, net_id, &members, want_high) {
            let dir = if to_ground { "pull-down to ground" } else { "pull-up to a rail" };
            report.findings.push(LintFinding {
                check: LintCheck::StrapPin,
                severity: Severity::Medium,
                message: format!(
                    "{} strap pin {} ({role}) net '{net_name}' has a {dir} ({rref}) but normal boot needs {}: the strap is biased to the wrong level ({note})",
                    mcu.reference, role, level.as_str()
                ),
                refs: vec![mcu.reference.clone(), rref],
                nets: vec![net_name.to_string()],
            });
        }
    }
}

/// A two-terminal resistor (ref R*, not RV/RT/RN/RP/RM, two connected pads, not
/// a ferrite/inductor), and assembled (not DNP). Mirrors the extract lint's
/// `is_resistor` plus a DNP guard.
fn is_assembled_resistor(c: &Component) -> bool {
    if c.dnp {
        return false;
    }
    let r = c.reference.to_ascii_uppercase();
    let lib = c.lib_id.to_ascii_lowercase();
    let is_r_ref = r.starts_with('R')
        && !r.starts_with("RV")
        && !r.starts_with("RT")
        && !r.starts_with("RN")
        && !r.starts_with("RP")
        && !r.starts_with("RM");
    let connected = c.pins.iter().filter(|p| p.net.is_some()).count();
    is_r_ref && connected == 2 && !lib.contains("ferrite") && !lib.contains("inductor")
}

/// Is this component a *powered oscillator* (a free-running clock source), as
/// opposed to a bare crystal (a passive resonator that does not drive on its
/// own)? An oscillator is a packaged part with its own VDD; we key on the
/// reference / value / lib_id, and require it to NOT be a plain 2-terminal
/// crystal. DNP oscillators are not assembled, so they do not drive.
fn is_clock_oscillator(c: &Component) -> bool {
    if c.dnp {
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
    let looks_osc = lib.contains("oscillator")
        || v.contains("OSCILLATOR")
        || v.contains("MHZ")
        || v.contains("OSC")
        || v.contains("XO")
        // Olimex's value string is "Q50MHz/..."; ref CR* / X* / Y* / OSC* are
        // the usual oscillator designators.
        || r.starts_with("OSC")
        || r.starts_with("CR")
        || (r.starts_with('X') && v.contains("MHZ"))
        || (r.starts_with('Y') && v.contains("MHZ"));
    looks_osc
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
fn clock_source_reaching<'a>(board: &'a ExtractedBoard, net_id: i64) -> Option<&'a Component> {
    // The strap net itself must not be a rail/ground (it never is for a strap,
    // but guard anyway so the membership scan is meaningful).
    if let Some(n) = board.net(net_id) {
        if is_ground_name(&n.name) || rail_voltage_name(&n.name).is_some() {
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
                if is_ground_name(&on.name) || rail_voltage_name(&on.name).is_some() {
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
fn oscillator_driving_net<'a>(board: &'a ExtractedBoard, net_id: i64) -> Option<&'a Component> {
    let net = board.net(net_id)?;
    if is_ground_name(&net.name) || rail_voltage_name(&net.name).is_some() {
        return None;
    }
    for (c, _p) in board.net_members(net_id) {
        if is_clock_oscillator(c) {
            return Some(c);
        }
    }
    None
}

/// If the strap net carries an assembled pull resistor whose far pad biases it
/// to the WRONG level for normal boot, return (resistor ref, to_ground). A
/// pull-up when LOW is wanted, or a pull-down when HIGH is wanted, is wrong.
fn wrong_pull(
    board: &ExtractedBoard,
    net_id: i64,
    members: &[(&Component, &galvani_extract::Pin)],
    want_high: bool,
) -> Option<(String, bool)> {
    for (c, _p) in members {
        if !is_assembled_resistor(c) {
            continue;
        }
        // A pull only counts if it is a *strong* bias relative to the part's
        // internal pull (~45 kOhm on ESP32). A weak/large series resistor into a
        // load is not a bias. Treat <= 20 kOhm as a real pull. Unknown value:
        // be conservative and do not fire.
        let ohms = parse_value(&c.value).map(|p| p.si).unwrap_or(f64::INFINITY);
        if !(ohms.is_finite() && ohms <= 20_000.0) {
            continue;
        }
        for op in &c.pins {
            if op.net == Some(net_id) {
                continue;
            }
            let Some(oid) = op.net else { continue };
            let Some(on) = board.net(oid) else { continue };
            let to_ground = is_ground_name(&on.name);
            let to_rail = rail_voltage_name(&on.name).is_some();
            if to_ground && want_high {
                return Some((c.reference.clone(), true)); // pull-down, want high
            }
            if to_rail && !want_high {
                return Some((c.reference.clone(), false)); // pull-up, want low
            }
        }
    }
    None
}

// ── Small net-name helpers (local copies; the extract versions are private) ──

fn norm(name: &str) -> String {
    let n = name.trim();
    let leaf = n.rsplit('/').next().unwrap_or(n);
    leaf.trim().to_ascii_uppercase()
}

fn is_ground_name(name: &str) -> bool {
    let n = norm(name);
    matches!(n.as_str(), "GND" | "GNDA" | "GNDD" | "AGND" | "DGND" | "PGND" | "VSS" | "GNDIO" | "0")
        || n.starts_with("GND")
}

fn rail_voltage_name(name: &str) -> Option<f64> {
    let n = norm(name);
    match n.as_str() {
        "+5V" | "5V" | "VCC" | "VDD" | "+VCC" | "VBUS" => Some(5.0),
        "+3V3" | "3V3" | "+3.3V" | "3.3V" | "VCC3V3" | "VDD3V3" | "VDD3P3" | "+3V3A" => Some(3.3),
        "+3V" | "3V" => Some(3.0),
        "+1V8" | "1V8" | "1.8V" => Some(1.8),
        _ => {
            if n.contains("3V3") || n.contains("3.3V") {
                Some(3.3)
            } else if n.contains("5V") && (n.starts_with('+') || n.contains("VCC") || n.contains("VBUS")) {
                Some(5.0)
            } else {
                None
            }
        }
    }
}

fn is_unconnected_net(name: &str) -> bool {
    name.trim_start_matches('/').starts_with("unconnected-")
}
