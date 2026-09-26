//! Behavioural-converter vregs must own their output rail.
//!
//! The pin-role dilemma this gates against: a vreg model carrying a
//! `[models.behavioral.converter]` block used to be unauthorable. Mapping the
//! converter's output pin as role `out` stamped `bind_vreg`'s ideal source on
//! the same net the converter drives, and the supply pass then trusted that
//! `Vreg_*` source; the ideal source wins, so input current read zero and the
//! converter model did nothing. Avoiding `out` (mapping the pin as `fb`, the
//! honest choice on a fixed-output buck whose FB pin ties to the rail) made
//! the converter work physically, but the part was reported "vreg output not
//! connected, left open", did not count as bound, and the canonical-named
//! output net STILL got the ideal auto-rail stamped over the converter.
//!
//! Now: a converter-carrying vreg stamps no ideal source, counts as bound with
//! no warning, and the supply pass suppresses the auto-rail on the converter's
//! output net, so the converter is the net's only supply and its reflected
//! input current is real.

use hauksbee_engine::binder::{bind_board_with, BoundBoard};
use hauksbee_engine::reports::bind::BindOutcome;
use hauksbee_engine::scheduler::Scheduler;
use hauksbee_engine::{BoardEvidence, CustomRegistry, DynamicLoad};
use hauksbee_extract::{Component, ExtractedBoard, Pin};
use hauksbee_ir::evidence::{Assumption, AssumptionKind, EvidenceStatus, RunDate};
use hauksbee_ir::Device;
use hauksbee_models::ModelLibrary;
use hauksbee_solve::SolverOptions;

use crate::support::passive_pin as pin;

fn comp(reference: &str, value: &str, footprint: &str, pins: Vec<Pin>) -> Component {
    crate::support::component(
        reference.to_string(),
        value.to_string(),
        String::new(),
        footprint.to_string(),
        pins,
    )
}

/// +5V (net 1) feeds a behavioural buck U1 whose regulated 3.3 V output is the
/// canonical supply-named net +3.3V (net 2), loaded by R1 = 33R to GND (net 3).
/// The +3.3V name is the point: it is exactly the shape the ideal auto-rail
/// used to claim.
fn buck_board() -> ExtractedBoard {
    ExtractedBoard {
        name: "buck_rail_test".to_string(),
        nets: vec![
            crate::support::net(1, "+5V".to_string()),
            crate::support::net(2, "+3.3V".to_string()),
            crate::support::net(3, "GND".to_string()),
        ],
        components: vec![
            comp(
                "U1",
                "TESTBUCK33",
                "Package_TO_SOT_SMD:SOT-23-6",
                vec![pin("1", 2), pin("3", 1), pin("4", 3)],
            ),
            comp(
                "R1",
                "33R",
                "Resistor_SMD:R_0402_1005Metric",
                vec![pin("1", 2), pin("2", 3)],
            ),
        ],
    }
}

/// A model library with one converter-carrying vreg, the output pin mapped as
/// `fb` (the fixed-output-buck shape: the sense pin IS the rail node).
fn lib_with_buck(out_role: &str) -> ModelLibrary {
    let dir = hauksbee_testkit::fresh_scratch_dir("behavioral_vreg_rail", out_role);
    std::fs::write(
        dir.join("testbuck.toml"),
        format!(
            r#"
[[models]]
id = "test_buck_3v3"
kind = "vreg"
description = "test fixed-3.3V buck with a behavioural converter"

[models.match]
value_re = "(?i)^TESTBUCK33$"

[models.params]
vout = 3.3

[models.pins]
"1" = "{out_role}"
"3" = "in"
"4" = "gnd"

[models.behavioral.converter]
topology = "buck"
out_pin = "{out_role}"
in_pin = "in"
vout_setpoint = 3.3
iout_limit_a = 2.0
efficiency = 0.9
out_r_ohms = 0.01
crossover_hz = 30e3
"#
        ),
    )
    .unwrap();
    ModelLibrary::builtin_with_user_dirs(&[dir.as_path()])
}

fn bind(out_role: &str) -> BoundBoard {
    bind_board_with(
        &buck_board(),
        &lib_with_buck(out_role),
        &CustomRegistry::new(),
    )
}

/// The faithful `fb` mapping: the part binds as a behavioural converter with
/// no "left open" warning, no ideal `Vreg_U1` source, and NO auto-rail supply
/// leg on the converter's output net.
#[test]
fn converter_vreg_binds_covered_and_suppresses_the_auto_rail() {
    let bound = bind("fb");

    let row = bound.report.row("U1").expect("U1 has a bind row");
    assert!(
        matches!(&row.outcome, BindOutcome::Behavioral { device } if device.contains("converter")),
        "U1 must bind as a behavioural converter, got {:?}",
        row.outcome
    );
    assert!(
        row.warning.is_none(),
        "a converter-carrying vreg must not warn 'left open': {:?}",
        row.warning
    );

    // No ideal source may drive the output net: neither bind_vreg's Vreg_U1
    // nor the supply pass's Vsupply_+3.3V.
    for dev in &bound.circuit.devices {
        if let Device::Vsource { name, .. } = dev {
            assert!(
                !name.starts_with("Vreg_U1"),
                "no ideal vreg source may be stamped: {name}"
            );
            assert_ne!(
                name, "Vsupply_+3.3V",
                "the auto-rail must be suppressed on the converter-driven net"
            );
        }
    }
    assert!(
        bound.supplies.iter().all(|l| l.net_name != "+3.3V"),
        "no supply leg on the converter-driven net, got {:?}",
        bound
            .supplies
            .iter()
            .map(|l| l.net_name.clone())
            .collect::<Vec<_>>()
    );
    // The INPUT rail keeps its supply leg: only the converter's output is his.
    assert!(
        bound.supplies.iter().any(|l| l.net_name == "+5V"),
        "the input rail still gets its ideal supply leg"
    );
    // The converter itself was stamped (its output source exists).
    assert!(
        bound
            .circuit
            .devices
            .iter()
            .any(|d| matches!(d, Device::Vsource { name, .. } if name == "Vbeh_U1_conv")),
        "the behavioural converter leg is stamped"
    );
    // And the converter source carries the regulator's stress meta, so a
    // max_temp/max_current watch on U1 stays acceptable (the fb-mapping used
    // to lose it along with the bind).
    assert!(
        bound.device_meta.iter().any(|m| m.reference == "U1"),
        "U1 keeps a stress-monitor meta via its converter source"
    );
}

/// The other horn of the old dilemma: mapping the pin as `out` used to stamp
/// the ideal `Vreg_U1` source over the converter. Both mappings must now bind
/// identically: converter only, no ideal source, no auto-rail.
#[test]
fn converter_vreg_with_out_role_stamps_no_ideal_source() {
    let bound = bind("out");
    assert!(
        !bound
            .circuit
            .devices
            .iter()
            .any(|d| matches!(d, Device::Vsource { name, .. } if name.starts_with("Vreg_"))),
        "role 'out' must not resurrect the ideal vreg source"
    );
    assert!(
        bound.supplies.iter().all(|l| l.net_name != "+3.3V"),
        "the auto-rail stays suppressed under the 'out' mapping too"
    );
    let row = bound.report.row("U1").expect("U1 has a bind row");
    assert!(
        matches!(&row.outcome, BindOutcome::Behavioral { .. }) && row.warning.is_none(),
        "bound clean under 'out' as well: {:?} / {:?}",
        row.outcome,
        row.warning
    );
}

/// End to end: the converter actually regulates the rail and reflects a real
/// input current. 33R on 3.3 V is 100 mA out, so the reflected 5 V input draw
/// must be ~73 mA (`P_out` / (0.9 * 5V)), and emphatically not zero, zero input
/// current was the ideal-rail-wins symptom.
#[test]
fn converter_vreg_regulates_and_draws_real_input_current() {
    let bound = bind("fb");
    let mut sched = Scheduler::new(bound, None, SolverOptions::default())
        .expect("scheduler builds for the buck board");
    let chunk = 1e-4_f64;
    for _ in 0..50 {
        sched.step(chunk);
    }
    assert!(
        sched.analog_valid(),
        "the buck board must solve cleanly, failed windows: {:?}",
        sched.failed_windows()
    );
    let vout = sched.net_voltage("+3.3V").expect("+3.3V is a live net");
    assert!(
        (vout - 3.3).abs() < 0.05,
        "the converter regulates the rail to 3.3V, got {vout}"
    );
    let (_, _, iin, _) = sched
        .behavioral_states()
        .into_iter()
        .find(|(reference, _, _, _)| reference == "U1")
        .expect("U1 is a live behavioural device");
    let iin = iin.expect("U1 carries a converter, so it reports input current");
    assert!(
        iin > 0.05,
        "the converter must reflect a real input current (~73 mA), got {iin} A"
    );
}

/// A generic buck card; `extra` lands in its converter block, `fb` adds a
/// feedback pin "5" regulating to 0.6 V.
fn lib_with_card(tag: &str, setpoint: f64, extra: &str, fb: bool) -> ModelLibrary {
    let dir = hauksbee_testkit::fresh_scratch_dir("behavioral_vreg_rail", tag);
    let (fb_pin, fb_block) = if fb {
        (
            "\"5\" = \"fb\"",
            "[models.behavioral.converter.feedback]\npin = \"fb\"\nvref_v = 0.6",
        )
    } else {
        ("", "")
    };
    let card = format!(
        "[[models]]\nid = \"test_buck_{tag}\"\nkind = \"vreg\"\ndescription = \"test buck\"\n\
         [models.match]\nvalue_re = \"(?i)^TESTBUCK$\"\n[models.params]\nvout = {setpoint}\n\
         [models.pins]\n\"1\" = \"sw\"\n\"3\" = \"in\"\n\"4\" = \"gnd\"\n{fb_pin}\n\
         [models.behavioral.converter]\ntopology = \"buck\"\nout_pin = \"sw\"\nin_pin = \"in\"\n\
         vout_setpoint = {setpoint}\niout_limit_a = 2.0\nefficiency = 0.9\nout_r_ohms = 0.01\n\
         {extra}\n{fb_block}\n"
    );
    std::fs::write(dir.join("testbuck.toml"), card).unwrap();
    ModelLibrary::builtin_with_user_dirs(&[dir.as_path()])
}

/// U1 from `input` through L1/C1 onto +OUT (net 2), loaded by `load`; with
/// `fb`, R2/R3 divide +OUT to 0.6 V at 3.3 V.
fn lc_buck_board(input: &str, load: &str, fb: bool) -> ExtractedBoard {
    let two =
        |r: &str, v: &str, fp: &str, a: i64, b: i64| comp(r, v, fp, vec![pin("1", a), pin("2", b)]);
    let res = "Resistor_SMD:R_0402_1005Metric";
    let mut u1 = vec![pin("1", 4), pin("3", 1), pin("4", 3)];
    let mut components = vec![
        two("L1", "10uH", "Inductor_SMD:L_1210_3225Metric", 4, 2),
        two("C1", "22uF", "Capacitor_SMD:C_1206_3216Metric", 2, 3),
        two("R1", load, res, 2, 3),
    ];
    if fb {
        u1.push(pin("5", 5));
        components.push(two("R2", "45k", res, 2, 5));
        components.push(two("R3", "10k", res, 5, 3));
    }
    components.push(comp("U1", "TESTBUCK", "Package_TO_SOT_SMD:SOT-23-6", u1));
    let names = [(1, input), (2, "+OUT"), (3, "GND"), (4, "SW"), (5, "FB")];
    ExtractedBoard {
        name: "lc_buck_test".to_string(),
        nets: names
            .iter()
            .map(|&(id, n)| crate::support::net(id, n.to_string()))
            .collect(),
        components,
    }
}

fn run_lc_buck(board: &ExtractedBoard, lib: &ModelLibrary, chunks: usize) -> (Scheduler, Vec<f64>) {
    let bound = bind_board_with(board, lib, &CustomRegistry::new());
    let mut sched =
        Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler builds");
    let trace = (0..chunks)
        .map(|_| {
            sched.step(1e-4);
            sched.net_voltage("+OUT").expect("+OUT is live")
        })
        .collect();
    assert!(
        sched.analog_valid(),
        "failed windows: {:?}",
        sched.failed_windows()
    );
    (sched, trace)
}

/// A feedback buck starts in regulation (settled like a DC operating point)
/// and holds there: the old per-chunk relaxation rang against the output LC
/// and could swing the rail through zero. Unloaded but for its divider it
/// does too: a no-sink clamp put that operating point on a kink the loop
/// gain's finite-difference probe straddled, so the DC solve never converged.
#[test]
fn feedback_buck_starts_in_regulation_and_holds_against_its_output_lc() {
    let lib = lib_with_card("fb", 3.3, "", true);
    for load in ["33R", "1Meg"] {
        let (_, trace) = run_lc_buck(&lc_buck_board("+5V", load, true), &lib, 30);
        let want = 0.6 * (1.0 + 45.0 / 10.0);
        assert!(
            trace.iter().all(|v| (v - want).abs() < 0.05 * want),
            "{want} V wanted, got {trace:?}"
        );
        let first = trace[0];
        assert!((first - want).abs() < 0.01 * want, "first chunk {first} V");
    }
}

/// A buck cannot exceed its maximum duty: a 12 V setpoint on a 9 V input
/// sits near 0.9 * 9 V, not a pass-through 9 V. The converter dissipates, so
/// its junction reads above ambient.
#[test]
fn buck_output_is_capped_by_max_duty_and_heats_the_part() {
    let lib = lib_with_card("dmax", 12.0, "max_duty = 0.9", false);
    let (sched, trace) = run_lc_buck(&lc_buck_board("+9V", "100R", false), &lib, 20);
    let vout = *trace.last().unwrap();
    assert!((vout - 8.1).abs() < 0.1, "D_max 0.9 of 9 V, got {vout} V");
    let tj = sched.temp_states().get("U1").copied();
    assert!(
        tj.is_some_and(|t| t > sched.ambient_c()),
        "converter Tj {tj:?}"
    );
}

/// An input-current-limited converter settles where its limited input power
/// meets the load, `V = sqrt(lim * eff * Vin * R)`: 2.67 V here. Newton used
/// to limit-cycle between the regulating and the clamped branch.
#[test]
fn input_limited_converter_finds_its_operating_point() {
    let lib = lib_with_card("iin", 3.3, "iin_limit_a = 0.02", false);
    let (_, trace) = run_lc_buck(&lc_buck_board("+12V", "33R", false), &lib, 5);
    let want = (0.02 * 0.9 * 12.0 * 33.0_f64).sqrt();
    assert!(
        trace.iter().all(|v| (v - want).abs() < 0.01 * want),
        "{trace:?}"
    );
}

/// The 3.3 V feedback buck from +12V with `cout` on +OUT, stepped by 1.5 A at
/// 1 ms: the rail's lowest 1 us sample over the next 300 us, where it ends,
/// and the bound board's evidence.
fn load_step(extra: &str, cout: &str) -> (f64, f64, BoardEvidence) {
    let mut board = lc_buck_board("+12V", "33R", true);
    board.components[1].value = cout.into();
    let lib = lib_with_card(&format!("step{cout}{}", extra.len()), 3.3, extra, true);
    let mut bound = bind_board_with(&board, &lib, &CustomRegistry::new());
    let today = RunDate::from_epoch_days(20_666);
    let evidence = BoardEvidence::from_bound(&board, &bound.report, &[], today).unwrap();
    let node = bound.circuit.node("+OUT");
    let profile = toml::from_str("id = \"s\"\n[[segment]]\nlevel_a = 1.5\nrise_s = 25e-9").unwrap();
    let load = DynamicLoad::new(&mut bound.circuit, "step", node, profile, 1e-3, 0);
    let mut sched = Scheduler::new(bound, None, SolverOptions::default()).unwrap();
    sched.attach_peripheral(Box::new(load));
    sched.step(1e-3);
    let mut trace = (0..300).map(|_| {
        sched.step(1e-6);
        sched.net_voltage("+OUT").unwrap()
    });
    let low = trace.by_ref().take(299).fold(f64::INFINITY, f64::min);
    let end = trace.next().unwrap();
    assert!(sched.analog_valid(), "{:?}", sched.failed_windows());
    (low, end, evidence)
}

/// A regulated buck's load step dips by its loop bandwidth, about
/// `dI / (2 pi f_c C)` (0.36 V here), and recovers within 300 us; the old
/// stiff source rang the output LC undamped by `dI sqrt(L/C)` (1.0 V). A card
/// stating its crossover carries no default-parameter assumption.
#[test]
fn load_step_dips_by_the_loop_bandwidth_and_recovers() {
    let (low, end, evidence) = load_step("crossover_hz = 30e3", "22uF");
    let est = 1.5 / (std::f64::consts::TAU * 30e3 * 22e-6);
    let dip = 3.3 - low;
    assert!((0.5..1.1).contains(&(dip / est)), "{dip} V vs {est} V");
    assert!((end - 3.3).abs() < 0.033, "recovered to {end} V");
    let mut assumptions = evidence.assumptions().iter();
    assert!(assumptions.all(|a| a.kind() != AssumptionKind::DefaultParameter));
}

/// An unstated crossover still regulates (at the conventional `f_sw / 10`) but is
/// a disclosed default parameter, which qualifies every assertion it feeds.
#[test]
fn unstated_crossover_is_a_disclosed_default_parameter() {
    let (low, _, evidence) = load_step("", "22uF");
    assert!(3.3 - low < 0.4, "dip to {low} V");
    let named = |a: &Assumption| a.id().as_str() == "default-parameter:U1.crossover_hz";
    assert!(evidence.assumptions().iter().any(named));
    let rail = evidence.ci_assertion_map("dip", &["+OUT".into()], &[], None, None);
    assert_eq!(rail.unwrap().status(), EvidenceStatus::Qualified);
}

/// The loop cannot rescue a genuinely undersized output capacitor: a tenth
/// of the capacitance dips far outside a 10% band.
#[test]
fn undersized_output_cap_still_dips_out_of_band() {
    let (low, _, _) = load_step("crossover_hz = 30e3", "2.2uF");
    assert!(3.3 - low > 0.33, "2.2 uF dipped only to {low} V");
}
