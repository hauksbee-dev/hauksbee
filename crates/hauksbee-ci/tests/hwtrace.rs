//! The hardware-trace corpus harness (T6, validation plan §T6).
//!
//! For every `testdata/hwtraces/<board>/<scenario>/spec.toml`, run the spec's
//! co-sim (same board + firmware + supply the capture session used) and let
//! its `hwtrace` assertion compare the simulated waveforms against the
//! captured trace, feature by feature, within the trace's stated tolerances.
//! Prints the per-feature table with both values — the same shape as the
//! ngspice corpus harness, with a different oracle.
//!
//! Unlike ngspice, the oracle here is a checked-in file, so nothing is
//! skipped-if-absent: an unreadable trace is a loud failure. What CAN be
//! absent is *real* hardware data — the seed traces are synthetic (labeled so
//! in their trace.toml and bannered in every report line) and prove the
//! pipeline, not the simulator.

use std::path::{Path, PathBuf};

use hauksbee_ci::{run, RunConfig};

fn hwtraces_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .join("testdata/hwtraces")
}

/// Every scenario spec under `testdata/hwtraces/<board>/<scenario>/spec.toml`.
fn corpus_specs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for board in std::fs::read_dir(hwtraces_dir()).expect("read hwtraces dir") {
        let board = board.expect("dir entry").path();
        if !board.is_dir() {
            continue;
        }
        for scenario in std::fs::read_dir(&board).expect("read board dir") {
            let spec = scenario.expect("dir entry").path().join("spec.toml");
            if spec.is_file() {
                out.push(spec);
            }
        }
    }
    out.sort();
    out
}

#[test]
fn hwtrace_corpus() {
    let specs = corpus_specs();
    assert!(
        !specs.is_empty(),
        "no scenario specs found under {}",
        hwtraces_dir().display()
    );

    let mut any_failed = false;
    for spec in &specs {
        let scenario = spec
            .parent()
            .unwrap()
            .strip_prefix(hwtraces_dir())
            .unwrap()
            .display()
            .to_string();
        let result = run(&RunConfig {
            spec: spec.clone(),
            ..Default::default()
        })
        .unwrap_or_else(|e| panic!("{scenario}: run failed: {e}"));

        for r in &result.results {
            eprintln!(
                "[{}] {scenario}: {}",
                if r.passed { "PASS" } else { "FAIL" },
                r.detail
            );
            any_failed |= !r.passed;
        }
    }
    assert!(!any_failed, "hwtrace corpus has features over tolerance (see lines above)");
}

/// Gate (b): a deliberate mismatch must FAIL and the failure must name the
/// feature and both values. We reuse the real seed scenario (board, firmware,
/// supply) but swap in a capture whose period is 300 ms where the firmware
/// toggles at 200 ms — the failure the harness exists to catch.
#[test]
fn hwtrace_deliberate_mismatch_names_feature_and_values() {
    let dir = std::env::temp_dir().join(format!("hauksbee_hwtrace_mismatch_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");

    // A clean 300 ms-period square-wave "capture", 1 s at 1 kSa/s.
    let mut csv = String::from("time_s,volts\n");
    let mut t = 0.0_f64;
    while t <= 1.0 {
        let phase = (t / 0.3).fract();
        let v = if phase >= 0.5 { 4.9 } else { 0.04 };
        csv.push_str(&format!("{t:.4},{v:.3}\n"));
        t += 0.001;
    }
    std::fs::write(dir.join("d13.csv"), csv).expect("write csv");

    std::fs::write(
        dir.join("trace.toml"),
        r#"
[trace]
board = "blinky"
scenario = "deliberate mismatch: 300 ms period against a 200 ms firmware"
provenance = "synthetic"
instrument = "constructed in-test"

[[channel]]
net = "D13"
file = "d13.csv"

[[channel.feature]]
kind = "period"
reltol = 0.10
"#,
    )
    .expect("write trace");

    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    std::fs::write(
        dir.join("spec.toml"),
        format!(
            r#"
name = "deliberate hwtrace mismatch"
board = "{}"
firmware = "{}"
duration_ms = 1000

[[supply]]
net = "+5V"
kind = "usb"
usb = "5v0.5a"

[[assert]]
kind = "hwtrace"
trace = "trace.toml"
"#,
            repo.join("examples/boards/blinky.kicad_pcb").display(),
            repo.join("../../testdata/firmware/demo/demo.hex").display(),
        ),
    )
    .expect("write spec");

    let result = run(&RunConfig {
        spec: dir.join("spec.toml"),
        ..Default::default()
    })
    .expect("run");

    let period = result
        .results
        .iter()
        .find(|r| r.label.contains("period"))
        .expect("a period feature result");
    eprintln!("mismatch detail: {}", period.detail);
    assert!(!period.passed, "a 200-vs-300 ms period must fail: {}", period.detail);
    assert!(period.detail.contains("period"), "{}", period.detail);
    assert!(period.detail.contains("300"), "captured value missing: {}", period.detail);
    assert!(period.detail.contains("20"), "sim value missing: {}", period.detail);
    assert!(period.detail.contains("EXCEEDS"), "{}", period.detail);
    assert!(
        period.detail.contains("SYNTHETIC"),
        "a synthetic trace must be bannered: {}",
        period.detail
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ── honesty rules, no sim required ───────────────────────────────────────────

fn load_trace_str(body: &str) -> Result<hauksbee_ci::hwtrace::Trace, hauksbee_ci::SpecError> {
    let dir = std::env::temp_dir().join(format!(
        "hauksbee_hwtrace_load_{}_{}",
        std::process::id(),
        body.len()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("trace.toml");
    std::fs::write(&path, body).unwrap();
    let r = hauksbee_ci::hwtrace::Trace::load(&path);
    let _ = std::fs::remove_dir_all(&dir);
    r
}

/// The provenance rule is mandatory: a trace that does not say whether it is
/// real or synthetic must not load.
#[test]
fn trace_without_provenance_is_refused() {
    let err = load_trace_str(
        r#"
[trace]
board = "b"
scenario = "s"
instrument = "i"

[[channel]]
net = "D13"
file = "d13.csv"

[[channel.feature]]
kind = "period"
reltol = 0.1
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("provenance"), "got: {err}");
}

/// A voltage feature on a VCD channel is refused by name: a logic analyzer
/// records bits, not volts.
#[test]
fn voltage_feature_on_vcd_is_refused() {
    let err = load_trace_str(
        r#"
[trace]
board = "b"
scenario = "s"
provenance = "synthetic"
instrument = "i"

[[channel]]
net = "D13"
file = "d13.vcd"

[[channel.feature]]
kind = "max"
abstol = 0.3
"#,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("bits, not volts"), "got: {msg}");
    assert!(msg.contains("max"), "got: {msg}");
}

/// A feature without any tolerance is refused: hardware traces carry their
/// own error bars and must state them.
#[test]
fn feature_without_tolerance_is_refused() {
    let err = load_trace_str(
        r#"
[trace]
board = "b"
scenario = "s"
provenance = "real"
instrument = "scope"

[[channel]]
net = "D13"
file = "d13.csv"

[[channel.feature]]
kind = "period"
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("error bars"), "got: {err}");
}
