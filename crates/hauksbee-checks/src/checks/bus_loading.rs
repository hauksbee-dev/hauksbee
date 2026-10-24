//! I2C bus-loading lint: pull-ups PRESENT but mis-sized for the bus.
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
//! ## Speed class
//!
//! The limits depend on the bus's speed class ([`I2cMode`], UM10204 Table 10).
//! A static lint cannot see the firmware's clock, so the class is the one the
//! bus's net name states (`I2C_400K_SDA`, `SCL_FM+`); a bus that names none is
//! judged as standard mode and every finding says so, because standard mode
//! has the loosest rise-time budget and cannot fault a bus that runs faster
//! than the board claims.
//!
//! ## Too strong (decision-grade, Medium)
//!
//! An open-drain driver must sink the full pull-up current while holding the
//! line at VOL. The I2C specification rates a driver at **3 mA** sink at VOL =
//! 0.4 V in standard and fast mode (20 mA in fast-mode plus), which is exactly
//! its Rp(min) formula: `Rp(min) = (VDD - VOL) / IOL`. With the effective
//! (parallel) pull-up `R_eff` to a rail `V`, the sink current while the pin
//! holds the line at VOL is `(V - VOL) / R_eff`. Every input is known exactly:
//! resistor values parse from the board, the rail resolves through the
//! binder's one rail table, VOL is the spec's 0.4 V. A device whose card states
//! a lower sink rating (`iol_a`) lowers the limit to its own. Above the limit
//! some device on the bus may never pull a valid low, and the failure is
//! intermittent by part lot, which is why it earns a finding rather than a
//! note.
//!
//! ## Too weak (Medium)
//!
//! The rise time is `t_r = 0.847 * R_eff * C_bus` (0.847 = ln(0.7/0.3), the
//! 30%..70% definition in UM10204) against the class budget: 1000 ns standard,
//! 300 ns fast, 120 ns fast-mode plus. Real bus capacitance needs layout
//! extraction; this check ESTIMATES it as 10 pF per attached device plus a
//! 20 pF trace allowance and states that basis in the finding. The estimate is
//! a FLOOR: it counts only each pin and a short trace, and connectors, cables,
//! protection parts and longer routes only add capacitance. A rise past the
//! budget at the floor is therefore a rise past it on the real bus, so the
//! finding is Medium, not an advisory. Parallel pull-ups combine into the one
//! effective resistance first.
//!
//! The floor argument holds for a bus that is wholly on this board. A net that
//! reaches a connector continues off-board, where the far end may add its own
//! pull-up in parallel (a bus broken out to an external module that
//! carries its own terminations, for example), so the on-board resistor alone is not
//! the effective pull-up and the rise is not known to fail. Those nets keep
//! the Low advisory and say why.

use std::collections::HashSet;

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_connector_like, Component, ExtractedBoard, I2cMode, LintCheck, LintFinding, NetLintReport,
    Severity,
};
use hauksbee_models::ModelLibrary;

use hauksbee_bind::binder::power_rail_voltage;
use hauksbee_ir::units::{format_amps, format_volts};

/// The output-low voltage (V) the sink rating is specified at: the line sits
/// at VOL while the driver sinks, so the pull-up sees `V_rail - VOL` across
/// it. This is UM10204's own Rp(min) operating point.
const I2C_VOL_V: f64 = 0.4;

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
pub(crate) fn is_i2c_net(name: &str) -> bool {
    let n = name.trim();
    let leaf = n.rsplit('/').next().unwrap_or(n).to_ascii_uppercase();
    leaf.split(|c: char| !c.is_ascii_alphanumeric()).any(|t| {
        let t = t.strip_prefix('A').unwrap_or(t);
        let t = t.trim_end_matches(|c: char| c.is_ascii_digit());
        t == "SDA" || t == "SCL"
    })
}

/// A plain two-terminal, assembled resistor with a parseable value. The
/// three-state assembly contract is part of the shared predicate: a DNP or
/// identity-refused pull-up must not count toward the bus's effective pull
/// resistance.
pub(crate) fn resistor_ohms(c: &Component) -> Option<f64> {
    if !super::straps::is_assembled_resistor(c) {
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
    (inv_r > 0.0).then_some((1.0 / inv_r, rail_v, refs))
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

/// The speed class a bus net's name states, else standard mode and `false`.
fn mode_of(board: &ExtractedBoard, net_id: i64, name: &str) -> (I2cMode, bool) {
    // The sibling line of the pair is named for the same bus (`I2C_400K_SDA`,
    // `I2C_400K_SCL`), but a net on a shared device can name its own speed too.
    let stated = I2cMode::from_hint(name).or_else(|| {
        let mut modes = board
            .net_members(net_id)
            .into_iter()
            .flat_map(|(c, _)| c.pins.iter().filter_map(|p| p.net))
            .filter(|&id| id != 0 && id != net_id)
            .filter_map(|id| board.net(id))
            .filter(|n| is_i2c_net(&n.name))
            .filter_map(|n| I2cMode::from_hint(&n.name));
        modes.next()
    });
    stated.map_or((I2cMode::Standard, false), |m| (m, true))
}

/// The lowest sink rating (A) a device on the net states in its card
/// (`iol_a`), with its reference.
fn weakest_driver(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    net_id: i64,
) -> Option<(f64, String)> {
    let mut weakest: Option<(f64, String)> = None;
    for (comp, model) in super::resolved_components(board, lib) {
        if !comp.pins.iter().any(|p| p.net == Some(net_id)) {
            continue;
        }
        let Some(iol) = super::plausible_param(&model, "iol_a").filter(|i| *i > 0.0) else {
            continue;
        };
        if weakest.as_ref().is_none_or(|(w, _)| iol < *w) {
            weakest = Some((iol, comp.reference.clone()));
        }
    }
    weakest
}

/// How a finding says which speed class it judged by.
fn mode_clause(mode: I2cMode, stated: bool) -> String {
    if stated {
        format!(
            "Judged as {}, the class the net's name states.",
            mode.label()
        )
    } else {
        format!(
            "Judged as {}: the net names no speed (name it, for example SDA_400K or SDA_FM+, to \
             judge it against fast mode or fast-mode plus).",
            mode.label()
        )
    }
}

/// Run the I2C bus-loading lint over every I2C-named net with a pull-up.
#[must_use]
#[allow(clippy::cast_precision_loss)] // a device count never nears 2^52
pub fn bus_loading_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
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
        let (mode, stated) = mode_of(board, net.id, &net.name);

        // Too strong: sink current above the driver rating. The line sits at
        // VOL while the driver sinks, so the pull-up carries (V_rail - VOL) /
        // R_eff: UM10204's Rp(min) operating point. A device card that states
        // a lower rating lowers the limit to its own.
        let (limit_a, limit_from) =
            match weakest_driver(board, lib, net.id).filter(|(i, _)| *i < mode.sink_limit_a()) {
                Some((iol, who)) => (iol, format!("{who}'s card iol_a")),
                None => (
                    mode.sink_limit_a(),
                    format!("the {} driver rating", mode.label()),
                ),
            };
        let sink_a = (rail_v - I2C_VOL_V).max(0.0) / r_eff;
        if sink_a > limit_a {
            report.findings.push(LintFinding {
                check: LintCheck::I2cBusLoading,
                severity: Severity::Medium,
                message: format!(
                    "I2C line \"{}\" is pulled up by {:.0} ohm effective ({}) to a \
                     {} rail: holding the line at the 0.4 V VOL takes \
                     {}, above the {} sink limit ({limit_from}), so a device on the bus may never produce a valid low. {}",
                    net.name,
                    r_eff,
                    refs.join("+"),
                    format_volts(rail_v),
                    format_amps(sink_a),
                    format_amps(limit_a),
                    mode_clause(mode, stated),
                ),
                refs,
                nets: vec![net.name.clone()],
            });
            continue; // one finding per net; too-strong and too-weak exclude each other
        }

        // Too weak for the ESTIMATED load. The estimate is a floor on the real
        // capacitance, so a rise past the budget here is a rise past it on the
        // real bus.
        let devices = attached_devices(board, net.id);
        let connector = board.net_members(net.id).into_iter().find_map(|(c, _)| {
            (AssemblyState::of(c).is_present() && is_connector_like(c)).then(|| c.reference.clone())
        });
        let c_est = (devices as f64).mul_add(C_PER_DEVICE_F, C_TRACE_F);
        let t_r = RISE_FACTOR * r_eff * c_est;
        if t_r > mode.rise_limit_s() {
            let basis = connector.as_ref().map_or_else(
                || {
                    "The capacitance is an estimate and a floor (connectors, cables and \
                     protection parts only add to it), so the real rise is slower still: \
                     strengthen the pull-up."
                        .to_string()
                },
                |j| {
                    format!(
                        "The capacitance is an estimate, and the line continues off-board \
                         through {j}, where the far end may add its own pull-up in parallel, \
                         so this is advisory: check the other side's pull-up or strengthen this one."
                    )
                },
            );
            report.findings.push(LintFinding {
                check: LintCheck::I2cBusLoading,
                severity: if connector.is_some() {
                    Severity::Low
                } else {
                    Severity::Medium
                },
                message: format!(
                    "I2C line \"{}\" rises through {:.0} ohm effective ({}) into an \
                     estimated {:.0} pF ({devices} devices at 10 pF + 20 pF trace \
                     allowance): ~{:.0} ns to cross 30%..70%, past the {} {:.0} ns \
                     budget. {basis} {}",
                    net.name,
                    r_eff,
                    refs.join("+"),
                    c_est * 1e12,
                    t_r * 1e9,
                    mode.label(),
                    mode.rise_limit_s() * 1e9,
                    mode_clause(mode, stated),
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
    use crate::test_support::two_pin_resistor;

    fn bare_pullup() -> Component {
        two_pin_resistor("R1", "4.7k")
    }

    /// Two-sided three-state contract: a fitted pull-up contributes its ohms
    /// to `R_eff`; a DNP or identity-refused record of the same part must not.
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
        bus_loading_lint(&board, &ModelLibrary::builtin())
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
            f[0].message.contains("8.788 mA"),
            "(3.3 - 0.4) V / 330 ohm = 8.788 mA must appear: {}",
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
    /// ~1.6 us to rise, past the 1000 ns standard-mode budget: one Medium
    /// finding that discloses the estimate basis. Medium, not an advisory,
    /// because the 10 pF per pin + 20 pF trace estimate is a floor on the real
    /// capacitance (connectors and cables only add), so the real rise is
    /// slower than the one that already fails.
    #[test]
    fn too_weak_pullup_fires_on_rise_time() {
        let r = run(&i2c_board("47k"));
        let f: Vec<_> = r.of_check(LintCheck::I2cBusLoading).collect();
        assert_eq!(f.len(), 1, "one finding for the 47k pull-up, got {f:?}");
        assert_eq!(f[0].severity, Severity::Medium);
        assert!(
            f[0].message.contains("estimate") && f[0].message.contains("standard-mode"),
            "the finding must disclose the estimate and the assumed class: {}",
            f[0].message
        );
    }

    /// Calibration, clean-board corpus: the same 47k pull-up on a line that
    /// continues off-board through a connector is not known to fail, since the
    /// far end may add its own pull-up in parallel. It stays a Low advisory that
    /// says so, where the wholly on-board bus above is Medium.
    #[test]
    fn too_weak_pullup_on_a_line_leaving_the_board_stays_advisory() {
        let with_connector = i2c_board("47k").replace(
            "  (module Resistor_SMD:",
            r#"  (module Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical (layer F.Cu)
    (at 130 100)
    (fp_text reference J1 (at 0 0) (layer F.SilkS))
    (fp_text value Conn (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 3 "/SDA"))
    (pad 2 thru_hole circle (at 0 1) (net 4 "/SCL"))
  )
  (module Resistor_SMD:"#,
        );
        let r = run(&with_connector);
        let f: Vec<_> = r.of_check(LintCheck::I2cBusLoading).collect();
        assert_eq!(f.len(), 1, "the finding is kept, got {f:?}");
        assert_eq!(f[0].severity, Severity::Low);
        assert!(
            f[0].message.contains("J1") && f[0].message.contains("off-board"),
            "{}",
            f[0].message
        );
    }

    fn count(text: &str) -> usize {
        run(text).of_check(LintCheck::I2cBusLoading).count()
    }

    /// The rise budget follows the class the net's name states: a 10k pull-up
    /// (~339 ns at the estimated 40 pF) is inside standard mode's 1000 ns,
    /// past fast mode's 300 ns and far past fast-mode plus's 120 ns.
    #[test]
    fn rise_budget_follows_the_named_speed_class() {
        let board = |net: &str| i2c_board("10k").replace("/SDA", net);
        assert_eq!(count(&board("/SDA")), 0, "unnamed: standard mode, 339 ns");
        assert_eq!(count(&board("/SDA_100K")), 0, "named standard mode");
        assert_eq!(count(&board("/SDA_400K")), 1, "339 ns > 300 ns");
        assert_eq!(count(&board("/SDA_FM+")), 1, "339 ns > 120 ns");
        let f = run(&board("/SDA_400K"));
        let f = f.of_check(LintCheck::I2cBusLoading).next().expect("fires");
        assert!(f.message.contains("fast-mode (400 kHz)"), "{}", f.message);
        // An unnamed bus says which class it assumed.
        let unnamed = run(&i2c_board("47k"));
        let unnamed = unnamed
            .of_check(LintCheck::I2cBusLoading)
            .next()
            .expect("fires");
        assert!(
            unnamed.message.contains("names no speed"),
            "{}",
            unnamed.message
        );
        // A 4.7k pull-up (~160 ns) meets fast mode and fails only fast-mode plus.
        assert_eq!(count(&i2c_board("4.7k").replace("/SDA", "/SDA_400K")), 0);
        assert_eq!(count(&i2c_board("4.7k").replace("/SDA", "/SDA_1M")), 1);
    }

    /// Fast-mode plus drivers sink 20 mA: 330 ohm (8.8 mA) is legal on a bus
    /// that names that class and a fault on one that does not; 100 ohm
    /// (29 mA) is a fault on both.
    #[test]
    fn sink_limit_follows_the_named_speed_class() {
        let named = |value: &str| i2c_board(value).replace("/SDA", "/SDA_FM+");
        assert_eq!(count(&i2c_board("330")), 1, "8.8 mA > 3 mA, unnamed");
        let legal = run(&named("330"));
        assert!(
            legal
                .of_check(LintCheck::I2cBusLoading)
                .all(|f| !f.message.contains("sink limit")),
            "8.8 mA is inside 20 mA: {:?}",
            legal.findings
        );
        assert_eq!(count(&named("100")), 1, "29 mA > 20 mA");
    }

    /// A 400 kHz bus whose SDA pull-up is `first`, with `second` in parallel.
    fn two_pullups(first: &str, second: &str) -> String {
        let board = i2c_board(first).replace("/SDA", "/SDA_400K");
        let r2 = format!(
            r#"
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 130 100)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value {second} (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "/SDA_400K"))
    (pad 2 smd rect (at 2 0) (net 2 "+3V3"))
  )
)"#
        );
        let body = board.trim_end().strip_suffix(')').expect("board closes");
        format!("{body}{r2}")
    }

    /// Parallel pull-ups combine before the rise is judged: 22k alone is 745 ns
    /// on fast mode; 22k || 22k = 11k is 372 ns, still past 300 ns; 22k || 4.7k
    /// = 3.9k is 132 ns and meets it.
    #[test]
    fn parallel_pullups_combine_into_one_effective_resistance() {
        assert_eq!(count(&i2c_board("22k").replace("/SDA", "/SDA_400K")), 1);
        assert_eq!(count(&two_pullups("22k", "22k")), 1, "11k: 372 ns");
        assert_eq!(count(&two_pullups("22k", "4.7k")), 0, "3.9k: 132 ns");
    }

    /// A device card that states its own sink rating (`iol_a`) lowers the
    /// limit to it: 1 k to 3.3 V sinks 2.9 mA, inside the spec's 3 mA and
    /// past a 2 mA part. The same board with no card stating it stays silent.
    #[test]
    fn a_card_stated_sink_rating_lowers_the_limit() {
        let card = |iol: &str| {
            format!(
                r#"
[[models]]
id = "weak_sensor"
kind = "digital"
description = "iol fixture"
[models.match]
value_re = "^SENSOR$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
{iol}
[models.pins]
"1" = "sda"
"2" = "scl"
"#
            )
        };
        let fires = |source: &str| {
            let dir = std::env::temp_dir().join(format!(
                "hauksbee_bus_loading_{}_{}",
                source.len(),
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("models.toml"), source).unwrap();
            let mut lib = ModelLibrary::empty();
            assert!(lib.load_user_dir(&dir).is_empty(), "fixture card loads");
            let board = ExtractedBoard::from_kicad_pcb(&i2c_board("1k")).expect("parse");
            bus_loading_lint(&board, &lib)
                .of_check(LintCheck::I2cBusLoading)
                .map(|f| f.message.clone())
                .collect::<Vec<_>>()
        };
        let weak = fires(&card("iol_a = 0.002"));
        assert_eq!(weak.len(), 1, "2.9 mA > 2 mA: {weak:?}");
        assert!(weak[0].contains("U2's card iol_a"), "{}", weak[0]);
        assert!(fires(&card("iol_a = 0.003")).is_empty(), "3 mA part: legal");
        assert!(fires(&card("")).is_empty(), "no stated rating: spec 3 mA");
    }
}
