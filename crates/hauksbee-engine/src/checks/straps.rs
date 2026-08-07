//! Boot strapping-pin lint (static, no firmware needed).
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
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

use hauksbee_extract::{
    Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity,
};
use hauksbee_models::value::parse_value;
use hauksbee_models::{ModelLibrary, StrapInternalPull, StrapLevel};

use crate::binder::resolve;
use hauksbee_extract::assembly::AssemblyState;

/// Run the strap-pin lint over an extracted board, resolving each component
/// against `lib` to find MCUs that carry a strap table.
pub fn strap_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
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
            // A floating strap net. For a pin with a documented internal pull
            // (every ESP32 strap) an undriven net settles to a defined level, so
            // skip; the prior, conservative behaviour. But a pin with NO internal
            // pull (STM32 BOOT0) left floating is genuinely undefined at reset:
            // that is the fault, not a non-event. This is the boot-mode latch the
            // firmware cannot override.
            if is_unconnected_net(&net.name) || net_is_isolated_to(board, net_id, comp) {
                if strap.internal_pull == StrapInternalPull::None {
                    // High (vs the wrong-pull arm's Medium): a wrong static pull at
                    // least boots *deterministically* wrong, so it is caught on the
                    // first power-up at the bench. A float boots correctly often
                    // enough to pass the bench yet fail intermittently in the field
                    // (humidity, EMI, a probe), a latent, unrecoverable failure
                    // that survives bring-up, which is the more dangerous outcome.
                    report.findings.push(LintFinding {
                        check: LintCheck::StrapPin,
                        severity: Severity::High,
                        message: format!(
                            "{} strap pin {} ({}) is left floating (net '{}' has no pull resistor and \
                             the pin has no internal pull): the boot level is undefined at reset, so the \
                             part may enter the bootloader instead of the application ({})",
                            comp.reference, pad_num, strap.role, net.name, strap.note
                        ),
                        refs: vec![comp.reference.clone()],
                        nets: vec![net.name.clone()],
                    });
                }
                continue;
            }
            examine_strap(
                board,
                comp,
                strap.role.as_str(),
                strap.level,
                strap.boot_select,
                &strap.note,
                net_id,
                &net.name,
                &mut report,
            );
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
    boot_select: bool,
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
    //     "defined" case), AND only for a *boot-select* strap whose wrong level
    //     is unrecoverable (BOOT0, GPIO0, GPIO9, QSPI_SS). A cosmetic or
    //     flash-voltage strap (ESP32 GPIO15 boot-log, GPIO2, GPIO12) may be
    //     legitimately repurposed as an ordinary GPIO with a pull on a shipped
    //     board, so a wrong-bias finding there would be a confident false
    //     positive on a correct design - exactly the cardinal sin. Gating on
    //     boot_select keeps this arm to the pins where "wrong pull" == "won't
    //     boot".
    let want_high = matches!(level, StrapLevel::High);
    let want_low = matches!(level, StrapLevel::Low);
    if boot_select && (want_high || want_low) {
        if let Some((rref, to_ground)) = wrong_pull(board, net_id, &members, want_high) {
            let dir = if to_ground {
                "pull-down to ground"
            } else {
                "pull-up to a rail"
            };
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
/// `is_resistor` plus a DNP guard. Shared with the boot check's bias-resistor
/// predicate, a varistor (RV) or thermistor (RT) to ground sets no DC level
/// and must never be credited as a bias.
pub(crate) fn is_assembled_resistor(c: &Component) -> bool {
    if !AssemblyState::of(c).is_present() {
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
        // Olimex's value string is "Q50MHz/..."; ref CR* / X* / Y* / OSC* are
        // the usual oscillator designators.
        || r.starts_with("OSC")
        || r.starts_with("CR")
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

/// If the strap net carries an assembled pull resistor whose far pad biases it
/// to the WRONG level for normal boot, return (resistor ref, to_ground). A
/// pull-up when LOW is wanted, or a pull-down when HIGH is wanted, is wrong.
fn wrong_pull(
    board: &ExtractedBoard,
    net_id: i64,
    members: &[(&Component, &hauksbee_extract::Pin)],
    want_high: bool,
) -> Option<(String, bool)> {
    // Collect the strong (<= 20 kOhm) pulls on the net, in each direction. A net
    // can carry both (a divider); we only fire on a wrong-direction pull when
    // there is NO opposing correct-direction pull to net it out (a divider that
    // resolves the strap to its correct level, plus the part's internal pull, is
    // not a fault). This is the split-divider guard.
    let mut wrong: Option<(String, bool)> = None; // (ref, to_ground)
    let mut has_correct = false;
    for (c, _p) in members {
        if !is_assembled_resistor(c) {
            continue;
        }
        // A pull only counts if it is a *strong* bias relative to the part's
        // internal pull (~45 kOhm on ESP32). A weak/large series resistor into a
        // load is not a bias. Treat <= 20 kOhm as a real pull. Unknown value:
        // be conservative and do not count it.
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
            let to_rail = is_rail_name(&on.name);
            // A pull toward the level the strap needs is "correct".
            let correct = (want_high && to_rail) || (!want_high && to_ground);
            let is_wrong = (want_high && to_ground) || (!want_high && to_rail);
            if correct {
                has_correct = true;
            }
            if is_wrong && wrong.is_none() {
                wrong = Some((c.reference.clone(), to_ground));
            }
        }
    }
    if has_correct {
        return None; // an opposing correct pull resolves the strap; not a fault
    }
    wrong
}

// ── Small net-name helpers (local copies; the extract versions are private) ──

fn norm(name: &str) -> String {
    let n = name.trim();
    let leaf = n.rsplit('/').next().unwrap_or(n);
    leaf.trim().to_ascii_uppercase()
}

fn is_ground_name(name: &str) -> bool {
    let n = norm(name);
    matches!(
        n.as_str(),
        "GND" | "GNDA" | "GNDD" | "AGND" | "DGND" | "PGND" | "VSS" | "GNDIO" | "0"
    ) || n.starts_with("GND")
}

/// Is this net name a power rail? Every use in this file is boolean ("is the
/// far side of this pull / this hop a rail"), so this is a predicate, not a
/// voltage table. Breadth matches boot.rs's `is_power_or_ground_net`, and for
/// the same calibration reason: an exact-name table (3V3/5V/1V8 plus
/// VCC/VBUS-gated forms) misses VBAT/VSYS/VMOT/VIN and bare voltages (9V/12V),
/// so a correct pull-up to "VBAT" reads as a signal net, which both lets the
/// clock-reach hop wander onto a rail (a spurious HIGH clock finding through an
/// oscillator's VDD) and hides a genuine wrong-direction pull from `wrong_pull`.
fn is_rail_name(name: &str) -> bool {
    let n = norm(name);
    // Ground is its own family, never a rail.
    if is_ground_name(name) {
        return false;
    }
    // Explicit '+' rail (e.g. "+3V3", "+5V", "+12V").
    if n.starts_with('+') {
        return true;
    }
    // V-prefixed rails (VCC/VDD/VBAT/VMOT/VSYS/VIN/VIO/VREF…) and bare voltage
    // names starting with a digit and carrying a 'V' ("12V", "3V3", "9V").
    let v_named = n.starts_with('V') && n.len() >= 2;
    let voltage_named = n.contains('V') && n.chars().next().is_some_and(|c| c.is_ascii_digit());
    v_named || voltage_named
}

fn is_unconnected_net(name: &str) -> bool {
    name.trim_start_matches('/').starts_with("unconnected-")
}

/// True when the only thing on `net_id` is `mcu` itself, no resistor, no other
/// component, nothing that could define a level. Such a strap net is floating
/// just as surely as a KiCad `unconnected-*` net, even if it carries a real name.
/// A net with ANY other member (a pull resistor, a header, a jumper, another
/// driver) is NOT isolated, so a jumper-selectable BOOT0 (a resistor to a 3-pin
/// header, as on dev boards) never trips this, only a genuinely dangling pin.
fn net_is_isolated_to(board: &ExtractedBoard, net_id: i64, mcu: &Component) -> bool {
    board
        .net_members(net_id)
        .iter()
        .all(|(c, _)| c.reference == mcu.reference)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::LintCheck;

    /// A constructed ESP32 board whose GPIO0 strap (ESP-WROOM-32 pad 25) carries
    /// a 50 MHz oscillator through a 10R series resistor - the Olimex fault shape
    /// in miniature. `clean` swaps the oscillator for a plain 10k pull-up to
    /// +3V3 (the correct strap bias).
    fn esp32_strap_board(clean: bool) -> String {
        // GPIO0 net = 3. Oscillator output net = 4 (faulty) bridged by R36 (10R).
        let driver = if clean {
            // 10k pull-up R36 from GPIO0 (net 3) to +3V3 (net 5): correct bias.
            r#"
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 120 100)
    (fp_text reference R36 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "/GPIO0"))
    (pad 2 smd rect (at 2 0) (net 5 "+3V3"))
  )"#
        } else {
            // CR1 50 MHz oscillator: out(net 4)/gnd/oe/vdd(+3V3), out -> R36 10R
            // -> GPIO0 (net 3). Free-running clock on the strap pin.
            r#"
  (module Oscillator:Oscillator_SMD_4Pin (layer F.Cu)
    (at 130 100)
    (fp_text reference CR1 (at 0 0) (layer F.SilkS))
    (fp_text value Q50MHz/25ppm/3V/4P (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 6 "/OSC_EN"))
    (pad 2 smd rect (at 1 0) (net 1 "GND"))
    (pad 3 smd rect (at 2 0) (net 4 "Net-(CR1-OUT)"))
    (pad 4 smd rect (at 3 0) (net 5 "+3V3"))
  )
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 120 100)
    (fp_text reference R36 (at 0 0) (layer F.SilkS))
    (fp_text value 10R (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 4 "Net-(CR1-OUT)"))
    (pad 2 smd rect (at 2 0) (net 3 "/GPIO0"))
  )"#
        };
        format!(
            r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "/GPIO0")
  (net 4 "Net-(CR1-OUT)")
  (net 5 "+3V3")
  (net 6 "/OSC_EN")
  (module RF_Module:ESP32-WROOM-32 (layer F.Cu)
    (at 100 100)
    (fp_text reference U3 (at 0 0) (layer F.SilkS))
    (fp_text value ESP-WROOM-32 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 5 "+3V3"))
    (pad 25 smd rect (at 0 5) (net 3 "/GPIO0"))
  ){driver}
)"#
        )
    }

    fn strap_findings(text: &str) -> NetLintReport {
        let board = ExtractedBoard::from_kicad_pcb(text).expect("parse synthetic board");
        let lib = ModelLibrary::builtin();
        strap_lint(&board, &lib)
    }

    #[test]
    fn clock_on_gpio0_strap_fires_high() {
        let r = strap_findings(&esp32_strap_board(false));
        let strap: Vec<_> = r.of_check(LintCheck::StrapPin).collect();
        assert_eq!(strap.len(), 1, "exactly the GPIO0 clock finding");
        assert!(matches!(strap[0].severity, Severity::High));
        assert!(strap[0].message.contains("gpio0"));
        assert!(strap[0].message.contains("CR1"));
        assert!(strap[0].nets.iter().any(|n| n.contains("GPIO0")));
    }

    #[test]
    fn pulled_gpio0_strap_is_clean() {
        // The fixed shape: a 10k pull-up to +3V3, no oscillator. Must be silent.
        let r = strap_findings(&esp32_strap_board(true));
        assert_eq!(
            r.of_check(LintCheck::StrapPin).count(),
            0,
            "a correctly-pulled strap raises no strap finding"
        );
    }

    #[test]
    fn strap_pullup_to_rail_does_not_reach_oscillator_vdd() {
        // Regression for the GPIO15 false fire: a strap with a pull-up to +3V3
        // must NOT "reach" an oscillator that merely shares the +3V3 rail via its
        // VDD pin. The clean board has the pull-up to +3V3 and (here) we add an
        // oscillator on an unrelated net sharing only +3V3. Still clean.
        let mut text = esp32_strap_board(true);
        // Insert an oscillator powered from +3V3 (net 5) but whose OUTPUT goes to
        // an unrelated net 7, not the strap. Sharing only the rail must not fire.
        let osc = r#"
  (module Oscillator:Oscillator_SMD_4Pin (layer F.Cu)
    (at 140 100)
    (fp_text reference CR2 (at 0 0) (layer F.SilkS))
    (fp_text value Q25MHz/25ppm/3V/4P (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 6 "/OSC_EN"))
    (pad 2 smd rect (at 1 0) (net 1 "GND"))
    (pad 3 smd rect (at 2 0) (net 7 "Net-(CR2-OUT)"))
    (pad 4 smd rect (at 3 0) (net 5 "+3V3"))
  )
)"#;
        // Replace the trailing ")" with the oscillator block + ")".
        text = text.trim_end().trim_end_matches(')').to_string();
        text.push_str(osc);
        let r = strap_findings(&text);
        assert_eq!(
            r.of_check(LintCheck::StrapPin).count(),
            0,
            "sharing only the +3V3 rail with an oscillator must not fire"
        );
    }

    /// A boot-select strap (STM32 BOOT0, which needs LOW) pulled to the WRONG
    /// level (a pull-up to +3V3) must fire medium; the correct pull-down is
    /// silent. STM32F103 pad 44 = BOOT0.
    fn stm32_boot0_board(pull_to_rail: bool) -> String {
        let (pull_net, pull_name) = if pull_to_rail {
            (2, "+3V3") // pull-up: WRONG for BOOT0 (needs low)
        } else {
            (1, "GND") // pull-down: correct
        };
        format!(
            r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (net 3 "/BOOT0")
  (module Package_QFP:LQFP-48 (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value STM32F103C8T6 (at 0 2) (layer F.Fab))
    (pad 9  smd rect (at 0 0) (net 2 "+3V3"))
    (pad 8  smd rect (at 0 1) (net 1 "GND"))
    (pad 44 smd rect (at 0 2) (net 3 "/BOOT0"))
  )
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "/BOOT0"))
    (pad 2 smd rect (at 2 0) (net {pull_net} "{pull_name}"))
  )
)"#
        )
    }

    #[test]
    fn strap_pullup_to_vbat_rail_does_not_reach_oscillator_vdd() {
        // Bug #19 regression: same shape as the +3V3 test above, but the rail
        // is named "VBAT", a name an exact-name rail table misses, and then the
        // series-R hop walks onto the rail, finds the oscillator's VDD, and
        // fires a spurious HIGH "free-running clock" finding on a correct
        // pull-up. VBAT must count as a rail; the board is clean.
        let text = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 3 "/GPIO0")
  (net 5 "VBAT")
  (net 6 "/OSC_EN")
  (net 7 "Net-(CR2-OUT)")
  (module RF_Module:ESP32-WROOM-32 (layer F.Cu)
    (at 100 100)
    (fp_text reference U3 (at 0 0) (layer F.SilkS))
    (fp_text value ESP-WROOM-32 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 5 "VBAT"))
    (pad 25 smd rect (at 0 5) (net 3 "/GPIO0"))
  )
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 120 100)
    (fp_text reference R36 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "/GPIO0"))
    (pad 2 smd rect (at 2 0) (net 5 "VBAT"))
  )
  (module Oscillator:Oscillator_SMD_4Pin (layer F.Cu)
    (at 140 100)
    (fp_text reference CR2 (at 0 0) (layer F.SilkS))
    (fp_text value Q25MHz/25ppm/3V/4P (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 6 "/OSC_EN"))
    (pad 2 smd rect (at 1 0) (net 1 "GND"))
    (pad 3 smd rect (at 2 0) (net 7 "Net-(CR2-OUT)"))
    (pad 4 smd rect (at 3 0) (net 5 "VBAT"))
  )
)"#;
        let r = strap_findings(text);
        assert_eq!(
            r.of_check(LintCheck::StrapPin).count(),
            0,
            "a pull-up to a VBAT-named rail shared with an oscillator's VDD must not fire"
        );
    }

    #[test]
    fn boot0_pulled_to_vbat_rail_fires_medium() {
        // Bug #20 regression: BOOT0 needs LOW, and here it is pulled up to a
        // rail named "VBAT". A rail table that returns None for VBAT makes the
        // wrong-direction pull read as "not a rail", silently missing a genuine
        // mis-strap. It must fire the Medium wrong-pull finding.
        let text = stm32_boot0_board(true).replace("+3V3", "VBAT");
        let r = strap_findings(&text);
        let strap: Vec<_> = r.of_check(LintCheck::StrapPin).collect();
        assert_eq!(
            strap.len(),
            1,
            "BOOT0 pulled high to VBAT is wrong (needs low)"
        );
        assert!(matches!(strap[0].severity, Severity::Medium));
        assert!(strap[0].message.contains("boot0"));
        assert!(strap[0].message.contains("wrong level"));
    }

    #[test]
    fn boot0_pulled_to_wrong_level_fires_medium() {
        let r = strap_findings(&stm32_boot0_board(true));
        let strap: Vec<_> = r.of_check(LintCheck::StrapPin).collect();
        assert_eq!(strap.len(), 1, "BOOT0 pulled high is wrong (needs low)");
        assert!(matches!(strap[0].severity, Severity::Medium));
        assert!(strap[0].message.contains("boot0"));
        assert!(strap[0].message.contains("wrong level"));
    }

    #[test]
    fn boot0_pulled_to_correct_level_is_clean() {
        let r = strap_findings(&stm32_boot0_board(false));
        assert_eq!(
            r.of_check(LintCheck::StrapPin).count(),
            0,
            "BOOT0 pulled low is correct, no finding"
        );
    }

    #[test]
    fn floating_boot0_no_internal_pull_fires_high() {
        // The flight-controller bug shape (explosion33/RCS_Hardware): an STM32
        // BOOT0 pad left UNCONNECTED, with no pull resistor anywhere. STM32 BOOT0
        // has no internal pull, so the boot source is undefined at reset; the
        // part can come up in the bootloader instead of the application. Must fire
        // a SERIOUS strap finding.
        let text = r#"(kicad_pcb (version 20211014) (host pcbnew 6.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (net 38 "unconnected-(U1-Pad44)")
  (module Package_QFP:LQFP-48 (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value STM32F401CCU6 (at 0 2) (layer F.Fab))
    (pad 9  smd rect (at 0 0) (net 2 "+3V3"))
    (pad 8  smd rect (at 0 1) (net 1 "GND"))
    (pad 44 smd rect (at 0 2) (net 38 "unconnected-(U1-Pad44)"))
  )
)"#;
        let r = strap_findings(text);
        let strap: Vec<_> = r.of_check(LintCheck::StrapPin).collect();
        assert_eq!(
            strap.len(),
            1,
            "a floating BOOT0 with no internal pull must fire"
        );
        assert!(matches!(strap[0].severity, Severity::High));
        assert!(strap[0].message.contains("floating"));
        assert!(strap[0].nets.iter().any(|n| n.contains("Pad44")));
    }

    #[test]
    fn isolated_named_boot0_net_fires_via_net_is_isolated_to() {
        // The `net_is_isolated_to` branch (not the unconnected-* name branch): a
        // BOOT0 net that carries a real name but has nothing on it except the MCU
        // pin (someone labelled it, then forgot the pulldown) is just as floating.
        let text = r#"(kicad_pcb (version 20211014) (host pcbnew 6.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (net 3 "/BOOT0")
  (module Package_QFP:LQFP-48 (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value STM32F401CCU6 (at 0 2) (layer F.Fab))
    (pad 9  smd rect (at 0 0) (net 2 "+3V3"))
    (pad 8  smd rect (at 0 1) (net 1 "GND"))
    (pad 44 smd rect (at 0 2) (net 3 "/BOOT0"))
  )
)"#;
        let r = strap_findings(text);
        let strap: Vec<_> = r.of_check(LintCheck::StrapPin).collect();
        assert_eq!(strap.len(), 1, "a named-but-isolated BOOT0 must fire");
        assert!(matches!(strap[0].severity, Severity::High));
        assert!(strap[0].message.contains("floating"));
    }

    #[test]
    fn jumper_selectable_boot0_does_not_fire() {
        // The zero-false-positive guarantee: a BOOT0 routed through a resistor to a
        // 3-pin selector header (the standard dev-board boot-mode jumper) has a
        // non-MCU member on its net, so it is NOT isolated and must NOT fire, even
        // though no static pull resolves it (the jumper does, at assembly).
        let text = r#"(kicad_pcb (version 20211014) (host pcbnew 6.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (net 3 "/BOOT0")
  (net 4 "/BOOT0_SEL")
  (module Package_QFP:LQFP-48 (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value STM32F401CCU6 (at 0 2) (layer F.Fab))
    (pad 44 smd rect (at 0 2) (net 3 "/BOOT0"))
  )
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 1k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "/BOOT0"))
    (pad 2 smd rect (at 2 0) (net 4 "/BOOT0_SEL"))
  )
  (module Connector:Conn_01x03 (layer F.Cu)
    (at 120 100)
    (fp_text reference J1 (at 0 0) (layer F.SilkS))
    (fp_text value BOOT_SEL (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 2 "+3V3"))
    (pad 2 smd rect (at 1 0) (net 4 "/BOOT0_SEL"))
    (pad 3 smd rect (at 2 0) (net 1 "GND"))
  )
)"#;
        let r = strap_findings(text);
        assert_eq!(
            r.of_check(LintCheck::StrapPin).count(),
            0,
            "a jumper-selectable BOOT0 (resistor to a 3-pin header) must NOT fire"
        );
    }

    #[test]
    fn floating_esp32_strap_does_not_fire() {
        // The symmetric negative: an ESP32 GPIO0 left undriven must NOT fire; the
        // ESP32 strap has a documented internal pull, so a floating net settles to
        // a defined level. This is the calibration the floating arm must respect.
        let text = r#"(kicad_pcb (version 20211014) (host pcbnew 6.0)
  (net 0 "")
  (net 1 "GND")
  (net 5 "+3V3")
  (net 9 "unconnected-(U3-Pad25)")
  (module RF_Module:ESP32-WROOM-32 (layer F.Cu)
    (at 100 100)
    (fp_text reference U3 (at 0 0) (layer F.SilkS))
    (fp_text value ESP32-WROOM-32 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 5 "+3V3"))
    (pad 25 smd rect (at 0 5) (net 9 "unconnected-(U3-Pad25)"))
  )
)"#;
        let r = strap_findings(text);
        assert_eq!(
            r.of_check(LintCheck::StrapPin).count(),
            0,
            "a floating ESP32 strap (internal pull) must not fire"
        );
    }

    #[test]
    fn boot0_with_an_opposing_correct_pull_does_not_fire() {
        // Split-divider guard: BOOT0 (needs low) with BOTH a pull-up and a
        // pull-down. The wrong-direction pull (the pull-up) must NOT fire,
        // because the pull-down is present to resolve the strap correctly.
        let text = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (net 3 "/BOOT0")
  (module Package_QFP:LQFP-48 (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value STM32F103C8T6 (at 0 2) (layer F.Fab))
    (pad 9  smd rect (at 0 0) (net 2 "+3V3"))
    (pad 8  smd rect (at 0 1) (net 1 "GND"))
    (pad 44 smd rect (at 0 2) (net 3 "/BOOT0"))
  )
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "/BOOT0"))
    (pad 2 smd rect (at 2 0) (net 2 "+3V3"))
  )
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 110 105)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 4k7 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "/BOOT0"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )
)"#;
        let r = strap_findings(text);
        assert_eq!(
            r.of_check(LintCheck::StrapPin).count(),
            0,
            "an opposing correct pull resolves the strap; not a fault"
        );
    }

    #[test]
    fn non_boot_select_strap_with_a_pull_does_not_fire_wrong_level() {
        // The cardinal-sin guard: an ESP32 GPIO15 (boot-log strap, level=high,
        // NOT boot_select) reused as an ordinary GPIO with a pull-DOWN must NOT
        // be flagged as "wrong level". A board may legitimately repurpose it.
        let text = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (net 3 "/GPIO15_LED")
  (module RF_Module:ESP32-WROOM-32 (layer F.Cu)
    (at 100 100)
    (fp_text reference U3 (at 0 0) (layer F.SilkS))
    (fp_text value ESP-WROOM-32 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 2 "+3V3"))
    (pad 23 smd rect (at 0 5) (net 3 "/GPIO15_LED"))
  )
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "/GPIO15_LED"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )
)"#;
        let r = strap_findings(text);
        assert_eq!(
            r.of_check(LintCheck::StrapPin).count(),
            0,
            "GPIO15 is not boot_select; a pull there is the board's choice, not a fault"
        );
    }

    #[test]
    fn avr_has_no_straps_examined() {
        // An ATmega328P board: the AVR entry carries no strap table, so the
        // strap lint must examine nothing and stay silent.
        let text = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "PC6_RESET")
  (module Package_QFP:TQFP-32_7x7mm_P0.8mm (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value ATmega328P (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "PC6_RESET"))
    (pad 7 smd rect (at 0 5) (net 2 "+5V"))
    (pad 8 smd rect (at 0 6) (net 1 "GND"))
  )
)"#;
        let r = strap_findings(text);
        assert_eq!(r.of_check(LintCheck::StrapPin).count(), 0);
    }
}
