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
use hauksbee_engine::report::BindOutcome;
use hauksbee_engine::scheduler::Scheduler;
use hauksbee_engine::CustomRegistry;
use hauksbee_extract::{Component, ExtractedBoard, Net, Pin};
use hauksbee_ir::Device;
use hauksbee_models::ModelLibrary;
use hauksbee_solve::SolverOptions;

fn pin(number: &str, net: i64) -> Pin {
    Pin {
        number: number.to_string(),
        net: Some(net),
        function: String::new(),
        kind: "passive".to_string(),
        position: None,
    }
}

fn comp(reference: &str, value: &str, footprint: &str, pins: Vec<Pin>) -> Component {
    Component {
        reference: reference.to_string(),
        value: value.to_string(),
        lib_id: String::new(),
        footprint: footprint.to_string(),
        position: None,
        layer: String::new(),
        properties: Vec::new(),
        dnp: false,
        pins,
    }
}

/// +5V (net 1) feeds a behavioural buck U1 whose regulated 3.3 V output is the
/// canonical supply-named net +3.3V (net 2), loaded by R1 = 33R to GND (net 3).
/// The +3.3V name is the point: it is exactly the shape the ideal auto-rail
/// used to claim.
fn buck_board() -> ExtractedBoard {
    ExtractedBoard {
        name: "buck_rail_test".to_string(),
        nets: vec![
            Net {
                id: 1,
                name: "+5V".to_string(),
            },
            Net {
                id: 2,
                name: "+3.3V".to_string(),
            },
            Net {
                id: 3,
                name: "GND".to_string(),
            },
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
    let dir = std::env::temp_dir().join(format!(
        "hauksbee_behavioral_vreg_rail_{out_role}_{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
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

    let row = bound
        .report
        .rows
        .iter()
        .find(|r| r.reference == "U1")
        .expect("U1 has a bind row");
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
    let row = bound
        .report
        .rows
        .iter()
        .find(|r| r.reference == "U1")
        .expect("U1 has a bind row");
    assert!(
        matches!(&row.outcome, BindOutcome::Behavioral { .. }) && row.warning.is_none(),
        "bound clean under 'out' as well: {:?} / {:?}",
        row.outcome,
        row.warning
    );
}

/// End to end: the converter actually regulates the rail and reflects a real
/// input current. 33R on 3.3 V is 100 mA out, so the reflected 5 V input draw
/// must be ~73 mA (P_out / (0.9 * 5V)), and emphatically not zero, zero input
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
    let vout = sched
        .net_voltage("+3.3V")
        .expect("+3.3V is a live net");
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
