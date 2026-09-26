//! A refused solve must name the smallest thing it can identify (E29).
//!
//! Two-sided. The negative side: a genuinely singular topology still refuses,
//! but the refusal now names the contested net and both sources, so nobody has
//! to bisect a board by model class to find it. The positive side: a board with
//! near-zero-ohm links that DOES solve reports nothing, because a diagnostic
//! that fires on healthy boards trains the user to ignore it.

use crate::shared::add_vdc;
use hauksbee_ir::{Circuit, Device, DiodeModel, NodeId};
use hauksbee_solve::{blame, dc_operating_point, run_tran, Probe, SolverOptions, Workspace};
use hauksbee_testkit::circuits::{diode, vpulse};

fn r(name: &str, a: NodeId, b: NodeId, ohms: f64) -> Device {
    Device::resistor(name.to_string(), a, b, ohms)
}

/// A board where one net is pinned by two ideal sources at different voltages:
/// two identical MNA constraint rows, no solution, and until now a refusal that
/// named nothing.
#[test]
fn a_singular_topology_refuses_and_names_the_node() {
    let mut c = Circuit::new();
    let res = c.node("RES");
    let mid = c.node("MID");
    add_vdc(&mut c, "Vsupply_RES", res, NodeId::GROUND, 3.3);
    add_vdc(&mut c, "Vdrive_RES", res, NodeId::GROUND, 20.0);
    c.add(r("R1", res, mid, 10_000.0));
    c.add(r("R2", mid, NodeId::GROUND, 10_000.0));

    let opts = SolverOptions::default();
    let mut ws = Workspace::new(&c);
    let err = dc_operating_point(&mut ws, &c, &opts)
        .expect_err("two ideal sources on one net is unsolvable and must be refused");
    let message = err.to_string();

    assert!(
        message.contains("RES"),
        "the refusal must name the contested net, got: {err}"
    );
    assert!(
        message.contains("Vsupply_RES") && message.contains("Vdrive_RES"),
        "the refusal must name both sources, got: {err}"
    );
    assert!(
        message.contains("3.300") && message.contains("20.000"),
        "the refusal must name both requested voltages, got: {err}"
    );
}

/// The other side of the same coin: a board with real milliohm links solves, and
/// the diagnostic stays quiet. A near-zero-ohm link is only a suspect when a
/// solve has actually failed.
#[test]
fn a_healthy_board_with_milliohm_links_solves_and_accuses_nobody() {
    let mut c = Circuit::new();
    let vcc = c.node("VCC");
    let a = c.node("A");
    let b = c.node("B");
    add_vdc(&mut c, "V1", vcc, NodeId::GROUND, 5.0);
    // Three 1 mohm jumpers in a chain, exactly what the bind-time 0R treatment
    // produces, plus an ordinary load.
    c.add(r("R1", vcc, a, 1e-3));
    c.add(r("R2", a, b, 1e-3));
    c.add(r("R3", b, NodeId::GROUND, 1e-3));
    c.add(r("R4", vcc, NodeId::GROUND, 1_000.0));

    let opts = SolverOptions::default();
    let mut ws = Workspace::new(&c);
    dc_operating_point(&mut ws, &c, &opts).expect("milliohm jumpers must not stop the solve");

    assert!(
        blame::stiff_links(&c).is_empty(),
        "1 mohm jumpers are physical, not pathological: {:?}",
        blame::stiff_links(&c)
    );
    assert!(
        blame::source_conflicts(&c).is_empty(),
        "nothing contests a net on this board"
    );
}

/// A microohm link IS named, because that is the conductance that actually
/// poisons the matrix and the thing the anyshake board needed said out loud.
#[test]
fn a_microohm_link_is_named_as_the_element_to_look_at() {
    let mut c = Circuit::new();
    let vcc = c.node("VCC");
    let a = c.node("A");
    add_vdc(&mut c, "V1", vcc, NodeId::GROUND, 5.0);
    c.add(r("R1", vcc, a, 1e-6));
    c.add(r("R2", a, NodeId::GROUND, 4_700.0));
    c.add(r("R3", vcc, NodeId::GROUND, 10_000.0));

    let stiff = blame::stiff_links(&c);
    assert_eq!(stiff.len(), 1, "exactly R1 stands out, got {stiff:?}");
    assert_eq!(stiff[0].name, "R1");

    let layout = hauksbee_solve::Layout::new(&c);
    let clause = blame::blame_clause(&c, &layout, Some((1.0, 0)))
        .expect("a stiff link is always worth naming on a failed solve");
    assert!(clause.contains("R1"), "{clause}");
    assert!(clause.contains("net 'VCC'"), "{clause}");
}

/// A march that keeps failing Newton stops at `max_failed_steps` with a stated
/// refusal naming a net, instead of cutting dt toward dt_min for minutes (the
/// MNT Reform chunk that ground through 600+ cuts). Unbudgeted, the same
/// diode turn-on is solved by cutting the step.
#[test]
fn a_march_over_its_failed_step_budget_refuses_and_names_a_net() {
    let mut c = Circuit::new();
    let (vin, a) = (c.node("VIN"), c.node("A"));
    vpulse(&mut c, "V1", vin, (0.0, 5.0), 1e-6, (1e-9, 1e-9), 1e-6);
    c.add(r("R1", vin, a, 100.0));
    diode(&mut c, "D1", a, NodeId::GROUND, DiodeModel::default());
    let opts = SolverOptions {
        max_newton: 3,
        ..SolverOptions::default()
    };
    let probes = [Probe::NodeVoltage("A".into())];
    run_tran(&c, &opts, 3e-6, &probes).expect("step cuts alone solve the turn-on");
    let budgeted = SolverOptions {
        max_failed_steps: Some(1),
        ..opts
    };
    let msg = run_tran(&c, &budgeted, 3e-6, &probes)
        .expect_err("two failed solves exceed a budget of one")
        .to_string();
    assert!(msg.contains("march budget"), "{msg}");
    assert!(msg.contains("net '"), "the refusal must name a net: {msg}");
}
