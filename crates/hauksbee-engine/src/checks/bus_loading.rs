//! I2C bus-loading lint: pull-ups PRESENT but mis-sized for the bus.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! The extract-layer `MissingI2cPullup` check answers "is there a pull-up at
//! all". This check answers the next question, the wishlist's "bus loading"
//! item: given the pull-ups that ARE there, can the bus actually work?
//!
//! Cross-layer, per the C6.1 audit: the fault spans the POWER layer (which
//! rail, at what voltage, the pull-up hangs from) and the SIGNAL layer (how
//! many devices load the line and what the open-drain drivers must sink).
//! Each resistor is individually a legal part; only the combination fails.
//!
//! ## Too strong (decision-grade, Medium)
//!
//! An open-drain driver must sink the full pull-up current while holding the
//! line at VOL. The I2C specification (UM10204) rates a standard driver at
//! **3 mA** sink at VOL = 0.4 V, which is exactly its Rp(min) formula:
//! `Rp(min) = (VDD - VOL) / 3 mA`. With the effective (parallel) pull-up
//! `R_eff` to a rail `V`, the sink current while the pin holds the line at
//! VOL is therefore `(V - VOL) / R_eff`. Every input is known exactly:
//! resistor values parse from the board, the rail resolves through the
//! binder's one rail table, VOL is the spec's 0.4 V. Above 3 mA some device
//! on the bus may never pull a valid low, and the failure is intermittent by
//! part lot, which is why it earns a finding rather than a note.
//!
//! ## Too weak (estimate-based, Low)
//!
//! The rise-time budget (1000 ns in standard mode, the loosest budget, so
//! this cannot false-fire on a fast-mode-capable bus) is `t_r = 0.847 *
//! R_eff * C_bus` (0.847 = ln(0.7/0.3), the 30%..70% definition in UM10204).
//! Real bus capacitance needs layout extraction; this check ESTIMATES it as
//! 10 pF per attached device plus a 20 pF trace allowance, states that basis
//! in the finding, and reports at Low severity: per the repo's
//! decision-grade-vs-context discipline, a capacitance estimate can support
//! an advisory, never a pass/fail.

use std::collections::HashSet;

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity,
};

use crate::binder::power_rail_voltage;

/// Maximum sink current (A) a standard I2C open-drain driver guarantees at
/// VOL: 3 mA per UM10204.
const I2C_MAX_SINK_A: f64 = 3.0e-3;

/// The output-low voltage (V) the 3 mA rating is specified at: the line sits
/// at VOL while the driver sinks, so the pull-up sees `V_rail - VOL` across
/// it. This is UM10204's own Rp(min) operating point.
const I2C_VOL_V: f64 = 0.4;

/// Standard-mode rise-time budget (s): 1000 ns per UM10204. The loosest of
/// the mode budgets, so the weak-side advisory cannot false-fire on a bus
/// that intends a faster mode.
const I2C_RISE_BUDGET_S: f64 = 1000.0e-9;

/// 30%-to-70% RC rise factor: ln(0.7/0.3).
const RISE_FACTOR: f64 = 0.8473;

/// Estimated pin capacitance per attached device (F): UM10204 budgets 10 pF
/// per connected pin.
const C_PER_DEVICE_F: f64 = 10.0e-12;

/// Estimated trace allowance (F) for a small board's routing.
const C_TRACE_F: f64 = 20.0e-12;

/// Is this net an I2C data/clock line by name? Token-exact SDA/SCL match with
/// the common index/alt prefixes ("SDA1", "ASDA"), mirroring the extract
/// layer's presence check so the two checks agree on what an I2C net is.
fn is_i2c_net(name: &str) -> bool {
    let n = name.trim();
    let leaf = n.rsplit('/').next().unwrap_or(n).to_ascii_uppercase();
    leaf.split(|c: char| !c.is_ascii_alphanumeric()).any(|t| {
        let t = t.strip_prefix('A').unwrap_or(t);
        let t = t.trim_end_matches(|c: char| c.is_ascii_digit());
        t == "SDA" || t == "SCL"
    })
}

/// A plain two-terminal, assembled resistor with a parseable value.
fn resistor_ohms(c: &Component) -> Option<f64> {
    // Three-state contract: a DNP or identity-refused pull-up must not count
    // toward the bus's effective pull resistance.
    if !AssemblyState::of(c).is_present() {
        return None;
    }
    let r = c.reference.to_ascii_uppercase();
    let lib = c.lib_id.to_ascii_lowercase();
    let is_r_ref = r.starts_with('R')
        && !r.starts_with("RV")
        && !r.starts_with("RT")
        && !r.starts_with("RN")
        && !r.starts_with("RP")
        && !r.starts_with("RM");
    if !is_r_ref
        || c.pins.iter().filter(|p| p.net.is_some()).count() != 2
        || lib.contains("ferrite")
        || lib.contains("inductor")
    {
        return None;
    }
    hauksbee_models::value::parse_value(&c.value)
        .map(|p| p.si)
        .filter(|o| o.is_finite() && *o > 0.0)
}

/// The effective pull-up on one net: parallel combination of every resistor
/// from the net to a resolvable rail, the highest such rail voltage, and the
/// contributing resistor refs. `None` when no resolvable pull-up exists (the
/// presence gap is `MissingI2cPullup`'s finding, not ours).
fn effective_pullup(board: &ExtractedBoard, net_id: i64) -> Option<(f64, f64, Vec<String>)> {
    let mut inv_r = 0.0f64;
    let mut rail_v: f64 = 0.0;
    let mut refs = Vec::new();
    for (c, _p) in board.net_members(net_id) {
        let Some(ohms) = resistor_ohms(c) else {
            continue;
        };
        for op in &c.pins {
            let Some(oid) = op.net.filter(|&id| id != 0 && id != net_id) else {
                continue;
            };
            let Some(on) = board.net(oid) else { continue };
            let Some(v) = power_rail_voltage(&on.name) else {
                continue;
            };
            if v <= 0.0 {
                continue; // a negative rail is not an I2C pull-up
            }
            inv_r += 1.0 / ohms;
            rail_v = rail_v.max(v);
            refs.push(c.reference.clone());
        }
    }
    if inv_r > 0.0 {
        Some((1.0 / inv_r, rail_v, refs))
    } else {
        None
    }
}

/// Count the distinct non-resistor devices attached to the net (the bus
/// load), deduped by reference (an IPC-356 both-sided record lists a pad
/// twice).
fn attached_devices(board: &ExtractedBoard, net_id: i64) -> usize {
    let mut refs: HashSet<&str> = HashSet::new();
    for (c, _p) in board.net_members(net_id) {
        if !AssemblyState::of(c).is_present() || resistor_ohms(c).is_some() {
            continue;
        }
        refs.insert(c.reference.as_str());
    }
    refs.len()
}

/// Run the I2C bus-loading lint over every I2C-named net with a pull-up.
pub fn bus_loading_lint(board: &ExtractedBoard) -> NetLintReport {
    let mut report = NetLintReport::default();
    for net in &board.nets {
        if net.id == 0 || !is_i2c_net(&net.name) {
            continue;
        }
        // A stub "SDA" with fewer than two members is a NC pad, same
        // calibration as the presence check.
        if board.net_members(net.id).len() < 2 {
            continue;
        }
        let Some((r_eff, rail_v, refs)) = effective_pullup(board, net.id) else {
            continue; // missing pull-up is MissingI2cPullup's finding
        };

        // Too strong: sink current above the spec's 3 mA driver rating. The
        // line sits at VOL while the driver sinks, so the pull-up carries
        // (V_rail - VOL) / R_eff: UM10204's Rp(min) operating point.
        let sink_a = (rail_v - I2C_VOL_V).max(0.0) / r_eff;
        if sink_a > I2C_MAX_SINK_A {
            report.findings.push(LintFinding {
                check: LintCheck::I2cBusLoading,
                severity: Severity::Medium,
                message: format!(
                    "I2C line \"{}\" is pulled up by {:.0} ohm effective ({}) to a \
                     {rail_v:.1} V rail: holding the line at the 0.4 V VOL takes \
                     {:.1} mA, above the 3 mA an I2C open-drain driver is specified \
                     to sink, so a device on the bus may never produce a valid low.",
                    net.name,
                    r_eff,
                    refs.join("+"),
                    sink_a * 1e3,
                ),
                refs,
                nets: vec![net.name.clone()],
            });
            continue; // one finding per net; too-strong and too-weak exclude each other
        }

        // Too weak for the ESTIMATED load: advisory only, estimate disclosed.
        let devices = attached_devices(board, net.id);
        let c_est = devices as f64 * C_PER_DEVICE_F + C_TRACE_F;
        let t_r = RISE_FACTOR * r_eff * c_est;
        if t_r > I2C_RISE_BUDGET_S {
            report.findings.push(LintFinding {
                check: LintCheck::I2cBusLoading,
                severity: Severity::Low,
                message: format!(
                    "I2C line \"{}\" rises through {:.0} ohm effective ({}) into an \
                     estimated {:.0} pF ({devices} devices at 10 pF + 20 pF trace \
                     allowance): ~{:.0} ns to cross 30%..70%, past even the \
                     standard-mode 1000 ns budget. The capacitance is an estimate, \
                     so this is advisory: measure or strengthen the pull-up.",
                    net.name,
                    r_eff,
                    refs.join("+"),
                    c_est * 1e12,
                    t_r * 1e9,
                ),
                refs,
                nets: vec![net.name.clone()],
            });
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare_pullup() -> Component {
        Component {
            reference: "R1".into(),
            value: "4.7k".into(),
            lib_id: "Device:R".into(),
            footprint: "R_0603".into(),
            position: None,
            layer: "F.Cu".into(),
            properties: Vec::new(),
            dnp: false,
            pins: vec![
                hauksbee_extract::Pin {
                    number: "1".into(),
                    net: Some(1),
                    function: String::new(),
                    kind: String::new(),
                    position: None,
                },
                hauksbee_extract::Pin {
                    number: "2".into(),
                    net: Some(2),
                    function: String::new(),
                    kind: String::new(),
                    position: None,
                },
            ],
        }
    }

    /// Two-sided three-state contract: a fitted pull-up contributes its ohms
    /// to R_eff; a DNP or identity-refused record of the same part must not.
    #[test]
    fn dnp_or_refused_pullup_contributes_no_ohms() {
        assert_eq!(resistor_ohms(&bare_pullup()), Some(4_700.0));

        let mut dnp = bare_pullup();
        dnp.dnp = true;
        assert_eq!(resistor_ohms(&dnp), None);

        let mut refused = bare_pullup();
        refused.properties.push((
            hauksbee_extract::DUPLICATE_REFERENCE_CONFLICT_KEY.into(),
            "two contradictory R1 records".into(),
        ));
        assert_eq!(resistor_ohms(&refused), None);
    }

    /// A one-master-one-peripheral bus whose SDA pull-up value is the knob.
    fn i2c_board(pullup: &str) -> String {
        format!(
            r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (net 3 "/SDA")
  (net 4 "/SCL")
  (module Package_QFP:LQFP-32 (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value MCU (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "/SDA"))
    (pad 2 smd rect (at 0 1) (net 4 "/SCL"))
    (pad 3 smd rect (at 0 2) (net 2 "+3V3"))
  )
  (module Package_SO:SOIC-8 (layer F.Cu)
    (at 110 100)
    (fp_text reference U2 (at 0 0) (layer F.SilkS))
    (fp_text value SENSOR (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "/SDA"))
    (pad 2 smd rect (at 0 1) (net 4 "/SCL"))
  )
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 120 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value {pullup} (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "/SDA"))
    (pad 2 smd rect (at 2 0) (net 2 "+3V3"))
  )
)"#
        )
    }

    fn run(text: &str) -> NetLintReport {
        let board = ExtractedBoard::from_kicad_pcb(text).expect("parse synthetic board");
        bus_loading_lint(&board)
    }

    /// Faulty side: a 330 ohm pull-up to 3.3 V demands (3.3 - 0.4)/330 =
    /// 8.8 mA of sink current at VOL, far past the 3 mA spec: one Medium
    /// finding on the SDA net.
    #[test]
    fn too_strong_pullup_fires_on_sink_current() {
        let r = run(&i2c_board("330"));
        let f: Vec<_> = r.of_check(LintCheck::I2cBusLoading).collect();
        assert_eq!(
            f.len(),
            1,
            "one finding for the 330R SDA pull-up, got {f:?}"
        );
        assert_eq!(f[0].severity, Severity::Medium);
        assert!(f[0].nets.contains(&"/SDA".to_string()));
        assert!(
            f[0].message.contains("8.8 mA"),
            "(3.3 - 0.4) V / 330 ohm = 8.8 mA must appear: {}",
            f[0].message
        );
    }

    /// The threshold is pinned from BOTH sides at the spec's own operating
    /// point: 1.0 k to 3.3 V sinks (3.3 - 0.4)/1000 = 2.90 mA (legal, silent);
    /// 910 ohm sinks 3.19 mA (fires). A check that forgot the VOL subtraction
    /// reads 3.3 mA at 1.0 k and false-fails the legal pull-up; a threshold
    /// looser than 3 mA misses the 910 ohm case.
    #[test]
    fn sink_current_threshold_is_pinned_at_the_spec_operating_point() {
        let legal = run(&i2c_board("1k"));
        assert_eq!(
            legal.of_check(LintCheck::I2cBusLoading).count(),
            0,
            "2.90 mA at VOL is inside the 3 mA rating, got {:?}",
            legal.findings
        );
        let over = run(&i2c_board("910"));
        let f: Vec<_> = over.of_check(LintCheck::I2cBusLoading).collect();
        assert_eq!(f.len(), 1, "3.19 mA at VOL must fire, got {f:?}");
        assert_eq!(f[0].severity, Severity::Medium);
    }

    /// Clean side: the textbook 4.7k pull-up sinks 0.7 mA and rises in ~160 ns
    /// at this bus's estimated load: silent on both arms.
    #[test]
    fn textbook_pullup_is_silent() {
        let r = run(&i2c_board("4.7k"));
        assert_eq!(
            r.of_check(LintCheck::I2cBusLoading).count(),
            0,
            "4.7k to 3.3 V is the datasheet idiom, got {:?}",
            r.findings
        );
    }

    /// Weak side: a 47k pull-up into a 2-device bus (estimated 40 pF) takes
    /// ~1.6 us to rise, past the 1000 ns standard-mode budget: one Low
    /// advisory that discloses the estimate basis.
    #[test]
    fn too_weak_pullup_advises_on_rise_time() {
        let r = run(&i2c_board("47k"));
        let f: Vec<_> = r.of_check(LintCheck::I2cBusLoading).collect();
        assert_eq!(f.len(), 1, "one advisory for the 47k pull-up, got {f:?}");
        assert_eq!(f[0].severity, Severity::Low);
        assert!(
            f[0].message.contains("estimate"),
            "the advisory must disclose the estimated capacitance: {}",
            f[0].message
        );
    }
}
