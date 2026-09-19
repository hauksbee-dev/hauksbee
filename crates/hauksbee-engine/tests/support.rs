//! Shared helpers for the engine crate's integration-test binary.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Output};

use hauksbee_engine::binder::BoundBoard;
use hauksbee_engine::reports::bind::BindReport;
use hauksbee_extract::{Component, Net, Pin};
use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};

/// The compiled `hauksbee` binary (Cargo sets this for the engine crate's tests).
#[allow(dead_code)]
pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

/// The compiled `hauksbee` binary as an owned path.
#[allow(dead_code)]
pub fn hauksbee_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hauksbee"))
}

/// Workspace-relative example boards, resolved from this crate's manifest dir.
#[allow(dead_code)]
pub fn board(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

/// Run the compiled binary and wait for it to exit.
#[allow(dead_code)]
pub fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("hauksbee binary runs")
}

/// [`run`] against a board: the `run` subcommand, that board's path, then
/// `args`. Every CLI test that analyses a board spells this same prefix.
#[allow(dead_code)]
pub fn run_board(board: &std::path::Path, args: &[&str]) -> Output {
    let mut argv = vec!["run", board.to_str().unwrap()];
    argv.extend_from_slice(args);
    run(&argv)
}

/// Decode a captured `Output`'s stdout as UTF-8.
#[allow(dead_code)]
pub fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// Decode a captured `Output`'s stderr as UTF-8.
#[allow(dead_code)]
pub fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Assert a captured run succeeded, surfacing stderr on failure.
#[allow(dead_code)]
pub fn assert_ok(o: &Output) {
    assert!(o.status.success(), "{}", stderr(o));
}

/// Decode and concatenate a captured run's stdout and stderr, in that order.
#[allow(dead_code)]
pub fn combined(o: &Output) -> String {
    format!("{}{}", stdout(o), stderr(o))
}

/// The repo root, two levels up from this crate's manifest dir.
#[allow(dead_code)]
pub fn repo_root() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

/// Return a device without changing the caller's insertion order or DeviceId.
pub fn resistor(name: String, a: NodeId, b: NodeId, ohms: f64) -> Device {
    Device::Resistor {
        name,
        a,
        b,
        ohms,
        tc1: None,
    }
}

pub fn capacitor(name: String, a: NodeId, b: NodeId, farads: f64) -> Device {
    Device::Capacitor {
        name,
        a,
        b,
        farads,
        ic: None,
    }
}

pub fn vsource(name: String, p: NodeId, n: NodeId, kind: SourceKind) -> Device {
    Device::Vsource { name, p, n, kind }
}

/// Minimal runtime layers; preserve the supplied net map and net-name order.
pub fn bound_board(
    name: String,
    circuit: Circuit,
    net_nodes: HashMap<String, NodeId>,
    net_names: Vec<String>,
) -> BoundBoard {
    BoundBoard {
        name,
        circuit,
        net_nodes,
        net_names,
        digital: Vec::new(),
        mcus: Vec::new(),
        dnp_mcus: Vec::new(),
        component_kinds: HashMap::new(),
        input_sources: HashMap::new(),
        supplies: Vec::new(),
        behavioral: Vec::new(),
        device_meta: Vec::new(),
        latching_switches: false,
        dacs: Vec::new(),
        peripherals: Vec::new(),
        report: BindReport::default(),
    }
}

/// Unpositioned fixture pin; PCB-only and passive kinds remain distinct.
pub fn pin(number: &str, net: Option<i64>, function: &str, kind: &str) -> Pin {
    Pin {
        number: number.to_string(),
        net,
        function: function.to_string(),
        kind: kind.to_string(),
        position: None,
    }
}

/// [`pin`] with no function and no kind: just a number on a net, the common
/// case for a fixture that only cares about connectivity.
#[allow(dead_code)]
pub fn netted_pin(number: &str, net: i64) -> Pin {
    pin(number, Some(net), "", "")
}

/// [`pin`] with no function, kind "passive": a resistor/capacitor-style pad.
#[allow(dead_code)]
pub fn passive_pin(number: &str, net: i64) -> Pin {
    pin(number, Some(net), "", "passive")
}

pub fn component(
    reference: String,
    value: String,
    lib_id: String,
    footprint: String,
    pins: Vec<Pin>,
) -> Component {
    Component {
        reference,
        value,
        lib_id,
        footprint,
        position: None,
        layer: String::new(),
        properties: Vec::new(),
        dnp: false,
        pins,
    }
}

pub fn net(id: i64, name: String) -> Net {
    Net { id, name }
}

/// A two-pin 0402 resistor-shaped [`component`], the shape both the
/// zero-ohm-jumper and zero-ohm-link fixtures build their board from: no
/// `lib_id`, footprint fixed at the real 0402 link package.
#[allow(dead_code)]
pub fn r0402(reference: &str, value: &str, pins: Vec<Pin>) -> Component {
    component(
        reference.to_string(),
        value.to_string(),
        String::new(),
        "Resistor_SMD:R_0402_1005Metric".to_string(),
        pins,
    )
}
