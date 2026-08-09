//! Regression gate for the lily58 live-sim divergence (corpus-exhaustive
//! browser run): the USBLC6-2 ESD-array clamp laws sat on the board's FLOATING
//! USB data nets (connector skipped, no MCU bound there), and the old
//! chunk-updated constant-`Isource` law form was an explicit relaxation with
//! no feedback inside the solve. One chunk's constant current drove the
//! gmin-anchored net to `I/gmin` volts, the next chunk evaluated the clamp AT
//! that voltage, and the loop multiplied the state by ~1e12 per chunk with
//! alternating sign (measured: 1.97 A -> 1.97e12 -> 1.98e24 -> ... -> inf)
//! until every later Newton solve failed at whatever net happened to condition
//! worst (the reported `Net-(STLED2-A)`).
//!
//! Two fixes, each tested two-sided here:
//!
//!   * eligible current laws now stamp as solver-implicit
//!     `Device::Behavioral`, so Newton owns the clamp's conductance and the
//!     floating net simply sits at its clamped equilibrium;
//!   * a "converged" chunk state that is not board reality (a net beyond the
//!     1e9 V sanity bound) is REFUSED and recorded as a failed chunk with the
//!     net named, never adopted as a quiet answer, so even a law the implicit
//!     form cannot express fails honestly instead of poisoning the run.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use hauksbee_engine::behavioral::BehavioralDevice;
use hauksbee_engine::binder::BoundBoard;
use hauksbee_engine::report::BindReport;
use hauksbee_engine::scheduler::{Scheduler, STRICT_CONSECUTIVE_FAILED_ABORT};
use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_models::behavioral::Behavioral;
use hauksbee_models::Params;
use hauksbee_solve::SolverOptions;

/// Wrap a hand-built circuit + behavioural devices as a minimal BoundBoard.
fn board_with(
    circuit: Circuit,
    net_nodes: HashMap<String, NodeId>,
    behavioral: Vec<BehavioralDevice>,
) -> BoundBoard {
    let net_names = net_nodes.keys().cloned().collect();
    BoundBoard {
        name: "clamp-fixture".to_string(),
        circuit,
        net_nodes,
        net_names,
        digital: Vec::new(),
        mcus: Vec::new(),
        dnp_mcus: Vec::new(),
        component_kinds: HashMap::new(),
        input_sources: HashMap::new(),
        supplies: Vec::new(),
        behavioral,
        device_meta: Vec::new(),
        dacs: Vec::new(),
        report: BindReport::default(),
    }
}

/// The lily58 shape in miniature: a floating IO net (its only anchor is gmin),
/// a driven 5 V VBUS, and the USBLC6-2 steering-diode pair as clamp laws.
fn usblc6_fixture() -> (BoundBoard, NodeId) {
    let mut c = Circuit::new();
    let io = c.node("IO");
    let vbus = c.node("VBUS");
    c.add(Device::Vsource {
        name: "Vbus".into(),
        p: vbus,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });
    // ST's own piecewise numbers, exactly as the shipped usblc6_2 model
    // carries them.
    let toml = r#"
[[laws]]
name = "io_to_vbus"
kind = "current"
a = "io"
b = "vbus"
expr = "max(0.0, (v_io - v_vbus - vt_clamp) / rd_clamp)"

[[laws]]
name = "gnd_to_io"
kind = "current"
a = "gnd"
b = "io"
expr = "max(0.0, (v_gnd - v_io - vt_clamp) / rd_clamp)"
"#;
    let model: Behavioral = toml::from_str(toml).unwrap();
    let mut params = Params::default();
    params.set_f64("vt_clamp", 1.1);
    params.set_f64("rd_clamp", 0.5);
    let mut roles = BTreeMap::new();
    roles.insert("io".to_string(), io);
    roles.insert("vbus".to_string(), vbus);
    roles.insert("gnd".to_string(), NodeId::GROUND);
    let dev = BehavioralDevice::stamp(&mut c, "U6", &model, &params, &roles, &|_| None)
        .expect("clamp laws stamp a device");
    let mut net_nodes = HashMap::new();
    net_nodes.insert("IO".to_string(), io);
    net_nodes.insert("VBUS".to_string(), vbus);
    (board_with(c, net_nodes, vec![dev]), io)
}

/// The clamp pair compiles to solver-implicit behavioral devices (no
/// chunk-updated `Ibeh_` Isource remains), and a run over the floating net
/// stays sane: every chunk solves, no net leaves single-digit volts. Before
/// the implicit form, this exact fixture diverged by ~1e12 per chunk.
#[test]
fn usblc6_clamp_on_floating_net_is_implicit_and_stays_sane() {
    let (board, _) = usblc6_fixture();
    let implicit = board
        .circuit
        .devices
        .iter()
        .filter(|d| matches!(d, Device::Behavioral { .. }))
        .count();
    let runtime_isources = board
        .circuit
        .devices
        .iter()
        .filter(|d| matches!(d, Device::Isource { name, .. } if name.starts_with("Ibeh_")))
        .count();
    assert_eq!(
        implicit, 2,
        "both clamp laws must stamp as solver-implicit behavioral devices"
    );
    assert_eq!(
        runtime_isources, 0,
        "no chunk-updated Isource may remain for an implicitly-stamped law"
    );

    let mut sched =
        Scheduler::new(board, None, SolverOptions::default()).expect("scheduler builds");
    for _ in 0..30 {
        let _ = sched.step(1e-4);
    }
    assert_eq!(
        sched.failed_chunk_count(),
        0,
        "the clamp pair on a floating net must solve every chunk (reasons: {:?})",
        sched.failed_window_reasons()
    );
    for (net, v) in sched.net_voltages() {
        assert!(
            v.abs() < 10.0,
            "net '{net}' at {v:.3e} V: the clamp relaxation is diverging again"
        );
    }
}

/// The pathological side: a law the implicit form cannot express (it reads
/// FSM context) blasting a constant 2 A into the same floating net. The old
/// behaviour was FAKE QUIET: the solve "converged" at I/gmin = 2e12 V (KCL
/// balances exactly there) and the run reported clean chunks. Now the
/// insane state is refused: failed chunks are recorded with the net named,
/// nothing insane is ever published, and the strict-abort streak trips, which
/// is what ends a live session honestly.
#[test]
fn constant_blast_into_floating_net_refuses_honestly_not_fake_quiet() {
    let mut c = Circuit::new();
    let io = c.node("IO");
    // `t_in_state` forces the runtime (chunk-updated) law form: the implicit
    // compiler must refuse FSM context, which this test also pins.
    let toml = r#"
[[laws]]
name = "blast"
kind = "current"
a = "gnd"
b = "io"
expr = "2.0 + 0.0 * t_in_state"
"#;
    let model: Behavioral = toml::from_str(toml).unwrap();
    let mut roles = BTreeMap::new();
    roles.insert("io".to_string(), io);
    roles.insert("gnd".to_string(), NodeId::GROUND);
    let dev = BehavioralDevice::stamp(&mut c, "U9", &model, &Params::default(), &roles, &|_| None)
        .expect("blast law stamps a device");
    assert!(
        c.devices
            .iter()
            .any(|d| matches!(d, Device::Isource { name, .. } if name == "Ibeh_U9_blast")),
        "an FSM-context law must keep the chunk-updated Isource form"
    );
    let mut net_nodes = HashMap::new();
    net_nodes.insert("IO".to_string(), io);

    let mut sched = Scheduler::new(board_with(c, net_nodes, vec![dev]), None, {
        SolverOptions::default()
    })
    .expect("scheduler builds");
    for _ in 0..10 {
        let _ = sched.step(1e-4);
    }
    assert!(
        sched.failed_chunk_count() > 0,
        "2 A into a floating net has no board-real answer; the run must refuse"
    );
    assert!(
        sched
            .failed_window_reasons()
            .iter()
            .any(|r| r.contains("not board reality") && r.contains("IO")),
        "the refusal must name the insane net, got: {:?}",
        sched.failed_window_reasons()
    );
    assert!(
        sched.analog_abort_tripped(),
        "a permanently insane solve must trip the strict-abort streak (this is \
         what ends the live session)"
    );
    for (net, v) in sched.net_voltages() {
        assert!(
            v.is_finite() && v.abs() < 1e9,
            "net '{net}' published at {v:.3e} V: an insane state was adopted"
        );
    }
}

/// Corpus gate: the whole lily58 Pro V2 board, bound by the ordinary pipeline,
/// must now march past the 1 ms mark the live sim stuck at, with zero failed
/// chunks and physical voltages. This is the board the defect shipped on.
#[test]
fn lily58_pro_v2_marches_past_one_millisecond_with_sane_voltages() {
    let Some(root) = hauksbee_testkit::corpus_boards_root_or_skip(
        env!("CARGO_MANIFEST_DIR"),
        "lily58 live-divergence regression",
    ) else {
        return;
    };
    let path: PathBuf = root.join("lily58/Pro_V2/Pro_V2.kicad_pcb");
    if !path.exists() {
        if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
            panic!(
                "HAUKSBEE_REQUIRE_CORPUS set but {} is missing",
                path.display()
            );
        }
        eprintln!("corpus board absent ({}); skipping", path.display());
        return;
    }
    let text = std::fs::read_to_string(&path).expect("read lily58");
    // Mirror the web live-launch pipeline exactly (`live_launcher`): DRC on
    // the same text, DNP defaults, bind, engine, validated shorts bridged.
    // Anything less simulates a different circuit than the one that diverged.
    let drc = hauksbee_extract::ExtractedBoard::drc(&text).unwrap_or_default();
    let mut board = hauksbee_extract::ExtractedBoard::from_auto(&text).expect("parse lily58");
    board
        .apply_dnp_policy(Default::default(), &[], &[])
        .expect("dnp policy");
    let bound = hauksbee_engine::bind_board(&board, &hauksbee_models::ModelLibrary::builtin());
    let mut engine = hauksbee_engine::HauksbeeEngine::from_bound(bound, None, "/boards/lily58")
        .expect("engine builds");
    engine.apply_and_disclose_drc_shorts(&drc);
    // 30 chunks x 100 us = 3 ms of sim time, three times the 1 ms wall the
    // live session died at.
    for _ in 0..30 {
        let _ = engine.scheduler_mut().step(1e-4);
    }
    let sched = engine.scheduler();
    assert_eq!(
        sched.failed_chunk_count(),
        0,
        "lily58 chunks failed again: {:?}",
        sched.failed_window_reasons()
    );
    for (net, v) in sched.net_voltages() {
        assert!(
            v.is_finite() && v.abs() < 100.0,
            "lily58 net '{net}' at {v:.3e} V is not board reality"
        );
    }
}

/// A live engine's step must return promptly once the solve is declared
/// dead: the blast fixture fails every chunk, so a step spanning many chunks
/// would grind a full rescue ladder per chunk for the whole span. With
/// `stop_when_dead` (set by the live engine), the step ends at the abort
/// streak instead: the session can only be ended honestly if the step that
/// discovered the death actually returns.
#[test]
fn a_dead_solve_ends_a_multi_chunk_step_at_the_abort_streak() {
    let mut c = Circuit::new();
    let io = c.node("IO");
    let toml = r#"
[[laws]]
name = "blast"
kind = "current"
a = "gnd"
b = "io"
expr = "2.0 + 0.0 * t_in_state"
"#;
    let model: Behavioral = toml::from_str(toml).unwrap();
    let mut roles = BTreeMap::new();
    roles.insert("io".to_string(), io);
    roles.insert("gnd".to_string(), NodeId::GROUND);
    let dev = BehavioralDevice::stamp(&mut c, "U9", &model, &Params::default(), &roles, &|_| None)
        .expect("blast law stamps a device");
    let mut net_nodes = HashMap::new();
    net_nodes.insert("IO".to_string(), io);
    let mut sched = Scheduler::new(board_with(c, net_nodes, vec![dev]), None, {
        SolverOptions::default()
    })
    .expect("scheduler builds");
    sched.stop_when_dead = true;

    // One step spanning 100 chunks of a solve that fails every chunk.
    let _ = sched.step(1e-2);
    assert!(
        sched.analog_abort_tripped(),
        "the blast fixture must trip the abort streak"
    );
    assert!(
        sched.failed_chunk_count() <= u64::from(STRICT_CONSECUTIVE_FAILED_ABORT) + 1,
        "a dead live step must stop at the abort streak, not grind all 100 chunks \
         (failed {})",
        sched.failed_chunk_count()
    );
}
