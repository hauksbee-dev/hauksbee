//! A drive that loses must say so, and a failed chunk must name a net (E30, E29a).
//!
//! Forcing a net to 20 V beside a co-located 3.3 V rail left the net reading
//! 3.300 V with nothing said. The product's whole claim is that it never
//! fabricates an answer, and 3.300 V presented as the result of a 20 V request
//! is a fabricated answer. Any time a drive or override loses to another stamped
//! source on the same node, both must be named along with the winner.
//!
//! Two-sided throughout: the contested board is loud, and a board where nothing
//! contests a net is silent.

use std::collections::HashMap;

use hauksbee_engine::binder::BoundBoard;
use hauksbee_engine::report::BindReport;
use hauksbee_engine::scheduler::Scheduler;
use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::SolverOptions;

fn board(circuit: Circuit, nets: &[(&str, NodeId)]) -> BoundBoard {
    let mut net_nodes = HashMap::new();
    for (name, id) in nets {
        net_nodes.insert((*name).to_string(), *id);
    }
    BoundBoard {
        name: "drive_override".to_string(),
        circuit,
        net_nodes,
        net_names: nets.iter().map(|(n, _)| (*n).to_string()).collect(),
        digital: Vec::new(),
        mcus: Vec::new(),
        dnp_mcus: Vec::new(),
        component_kinds: HashMap::new(),
        input_sources: HashMap::new(),
        supplies: Vec::new(),
        behavioral: Vec::new(),
        device_meta: Vec::new(),
        dacs: Vec::new(),
        report: BindReport::default(),
    }
}

/// The reported shape: a 3.3 V rail on RES, and a 20 V drive stamped on the same
/// node. Two ideal sources, one net.
fn contested_board() -> BoundBoard {
    let mut circuit = Circuit::new();
    let res = circuit.node("RES");
    circuit.add(Device::Vsource {
        name: "Vsupply_RES".to_string(),
        p: res,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(3.3),
    });
    circuit.add(Device::Vsource {
        name: "Vci_drive_RES".to_string(),
        p: res,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(20.0),
    });
    circuit.add(Device::Resistor {
        name: "R1".to_string(),
        a: res,
        b: NodeId::GROUND,
        ohms: 10_000.0,
        tc1: None,
    });
    board(circuit, &[("RES", res)])
}

/// The same rail with nothing contesting it.
fn quiet_board() -> BoundBoard {
    let mut circuit = Circuit::new();
    let res = circuit.node("RES");
    circuit.add(Device::Vsource {
        name: "Vsupply_RES".to_string(),
        p: res,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(3.3),
    });
    circuit.add(Device::Resistor {
        name: "R1".to_string(),
        a: res,
        b: NodeId::GROUND,
        ohms: 10_000.0,
        tc1: None,
    });
    board(circuit, &[("RES", res)])
}

#[test]
fn a_drive_that_loses_to_a_co_located_source_is_named_on_both_sides() {
    let mut sched = Scheduler::new(contested_board(), None, SolverOptions::default())
        .expect("build scheduler for the contested board");
    sched.chunk_s = 1e-4;
    sched.step(1e-4);

    let notes = sched.drive_conflicts();
    assert_eq!(
        notes.len(),
        1,
        "exactly one contested net, got: {notes:?}"
    );
    let n = &notes[0];
    assert!(n.contains("RES"), "must name the net: {n}");
    assert!(
        n.contains("Vsupply_RES") && n.contains("Vci_drive_RES"),
        "must name BOTH sources: {n}"
    );
    assert!(
        n.contains("3.300") && n.contains("20.000"),
        "must name both requested voltages: {n}"
    );
    // And it must resolve the outcome rather than leaving the reader to guess.
    assert!(
        n.contains("won") || n.contains("matches no single source"),
        "must say which won: {n}"
    );
}

#[test]
fn an_uncontested_rail_says_nothing() {
    let mut sched = Scheduler::new(quiet_board(), None, SolverOptions::default())
        .expect("build scheduler for the quiet board");
    sched.chunk_s = 1e-4;
    sched.step(1e-4);
    assert!(
        sched.drive_conflicts().is_empty(),
        "a rail nothing contests must produce no note: {:?}",
        sched.drive_conflicts()
    );
    // Sanity: the quiet board really does solve, so the silence is the silence
    // of a healthy run and not of a run that never got that far.
    assert!(sched.analog_valid(), "the quiet board must solve");
}

#[test]
fn a_post_solve_override_on_a_stamped_rail_is_named() {
    // The engine's own override path: `force_net_voltage` rewrites the node
    // voltage AFTER the solve, so it wins over the stamped rail invisibly. That
    // is the same honesty defect wearing different clothes.
    let mut sched = Scheduler::new(quiet_board(), None, SolverOptions::default())
        .expect("build scheduler");
    sched.chunk_s = 1e-4;
    assert!(sched.force_net_voltage("RES", 20.0), "RES must exist");
    sched.step(1e-4);

    let notes = sched.drive_conflicts();
    assert_eq!(notes.len(), 1, "the override must be named: {notes:?}");
    let n = &notes[0];
    assert!(n.contains("RES"), "{n}");
    assert!(n.contains("20.000"), "must name the override value: {n}");
    assert!(
        n.contains("Vsupply_RES"),
        "must name the stamped source it overrides: {n}"
    );
    assert!(
        n.contains("no effect"),
        "must say the stamped source has no effect on the reported voltage: {n}"
    );
}

#[test]
fn a_failed_chunk_names_the_net_and_the_interval() {
    // E29a on the scheduler surface: the contested board is also structurally
    // singular, so every chunk fails. The per-window diagnosis must carry the
    // time interval AND the net, not just a count.
    let mut sched = Scheduler::new(contested_board(), None, SolverOptions::default())
        .expect("build scheduler");
    sched.chunk_s = 1e-4;
    sched.step(1e-4);
    sched.step(1e-4);

    assert!(!sched.analog_valid(), "the singular board must not pass");
    let diags = sched.failed_window_diagnoses();
    assert_eq!(diags.len(), 1, "contiguous failures merge: {diags:?}");
    let d = &diags[0];
    assert!(
        d.contains("0.000-0.200 ms"),
        "must carry the time interval: {d}"
    );
    assert!(d.contains("RES"), "must name the offending net: {d}");
    assert!(
        d.contains("Vsupply_RES") && d.contains("Vci_drive_RES"),
        "must name the offending elements: {d}"
    );
}
