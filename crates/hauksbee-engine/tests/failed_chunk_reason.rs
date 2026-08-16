//! A failed analog chunk must carry its cause, not just its extent.
//!
//! `solve_chunk` used to discard the solver's error string (`Err(_)`), so a
//! non-convergent run reported failed windows with no reason attached, and
//! finding the offending element meant manual bisection. The scheduler now
//! keeps the most recent solver message (device-named when a behavioural
//! fault caused the failure) and exposes it alongside the failed windows.

use std::collections::HashMap;

use hauksbee_engine::binder::BoundBoard;
use hauksbee_engine::report::BindReport;
use hauksbee_engine::scheduler::Scheduler;
use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::SolverOptions;

/// Same forcing as `cosim_failed_chunk.rs`: two conflicting ideal sources on
/// one node is a structurally singular MNA system, so every chunk fails.
fn impossible_board() -> BoundBoard {
    let mut circuit = Circuit::new();
    let n1 = circuit.node("n1");
    circuit.add(Device::Vsource {
        name: "V1".to_string(),
        p: n1,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });
    circuit.add(Device::Vsource {
        name: "V2".to_string(),
        p: n1,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(3.0),
    });
    let mut net_nodes = HashMap::new();
    net_nodes.insert("n1".to_string(), n1);
    BoundBoard {
        name: "impossible".to_string(),
        circuit,
        net_nodes,
        net_names: vec!["n1".to_string()],
        digital: Vec::new(),
        mcus: Vec::new(),
        dnp_mcus: Vec::new(),
        component_kinds: HashMap::new(),
        input_sources: HashMap::new(),
        supplies: Vec::new(),
        behavioral: Vec::new(),
        device_meta: Vec::new(),
        dacs: Vec::new(),
        peripherals: Vec::new(),
        report: BindReport::default(),
    }
}

#[test]
fn failed_chunk_retains_the_solver_error() {
    let mut sched = Scheduler::new(impossible_board(), None, SolverOptions::default())
        .expect("build scheduler for the impossible board");
    assert!(
        sched.last_solve_error().is_none(),
        "a fresh run carries no failure reason"
    );
    sched.step(1e-4);
    assert!(sched.failed_chunk_count() >= 1, "the chunk must fail");
    let reason = sched
        .last_solve_error()
        .expect("a failed chunk keeps the solver's message");
    assert!(
        !reason.trim().is_empty(),
        "the retained reason is a real message, got {reason:?}"
    );

    // A run reset clears the stale reason with the rest of the accounting.
    sched.reset_run_state();
    assert!(
        sched.last_solve_error().is_none(),
        "reset_run_state clears the failure reason"
    );
}
