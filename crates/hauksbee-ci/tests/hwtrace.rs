//! The hardware-trace corpus harness (T6).
//!
//! For every `testdata/hwtraces/<board>/<scenario>/spec.toml`, run the spec's
//! co-sim (same board + firmware + supply the capture session used) and let
//! its `hwtrace` assertion compare the simulated waveforms against the
//! captured trace, feature by feature, within the trace's stated tolerances.
//! Prints the per-feature table with both values; the same shape as the
//! ngspice corpus harness, with a different oracle.
//!
//! Unlike ngspice, the oracle here is a checked-in file, so nothing is
//! skipped-if-absent: an unreadable trace is a loud failure. What CAN be
//! absent is *real* hardware data; the seed traces are synthetic (labeled so
//! in their trace.toml and bannered in every report line) and prove the
//! pipeline, not the simulator.

#[cfg(feature = "avr")]
use std::{fmt::Write as _, path::PathBuf};

use crate::support;

#[cfg(feature = "avr")]
use hauksbee_ci::run;

#[cfg(feature = "avr")]
fn hwtraces_dir() -> PathBuf {
    support::repo("testdata/hwtraces")
}

/// Every scenario spec under `testdata/hwtraces/<board>/<scenario>/spec.toml`.
#[cfg(feature = "avr")]
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

// Replays AVR .hex firmware (the demo/blinky corpus is ATmega-based), so it
// needs the GPL-gated `avr` feature; the GPL-free renode/qemu build refuses
// AVR firmware by design.
#[cfg(feature = "avr")]
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
        let result = run(&support::run_config(spec.clone()))
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
    assert!(
        !any_failed,
        "hwtrace corpus has features over tolerance (see lines above)"
    );
}

/// Gate (b): a deliberate mismatch must FAIL and the failure must name the
/// feature and both values. We reuse the real seed scenario (board, firmware,
/// supply) but swap in a capture whose period is 300 ms where the firmware
/// toggles at 200 ms; the failure the harness exists to catch.
// Replays AVR .hex firmware (the demo/blinky corpus is ATmega-based), so it
// needs the GPL-gated `avr` feature; the GPL-free renode/qemu build refuses
// AVR firmware by design.
#[cfg(feature = "avr")]
#[test]
fn hwtrace_deliberate_mismatch_names_feature_and_values() {
    // A clean 300 ms-period square-wave "capture", 1 s at 1 kSa/s.
    let mut csv = String::from("time_s,volts\n");
    let mut t = 0.0_f64;
    for _ in 0..1000 {
        let phase = (t / 0.3).fract();
        let v = if phase >= 0.5 { 4.9 } else { 0.04 };
        let _ = writeln!(csv, "{t:.4},{v:.3}");
        t += 0.001;
    }
    let dir = support::write_tmp("d13.csv", &csv)
        .parent()
        .unwrap()
        .to_path_buf();

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

    std::fs::write(
        dir.join("spec.toml"),
        format!(
            r#"
name = "deliberate hwtrace mismatch"
board = {}
firmware = {}
duration_ms = 1000

[[supply]]
net = "+5V"
kind = "usb"
usb = "5v0.5a"

[[assert]]
kind = "hwtrace"
trace = "trace.toml"
"#,
            support::toml_path(&support::blinky_board()),
            support::toml_path(&support::repo("testdata/firmware/demo/demo.hex")),
        ),
    )
    .expect("write spec");

    let result = run(&support::run_config(dir.join("spec.toml"))).expect("run");

    let period = result
        .results
        .iter()
        .find(|r| r.label.contains("period"))
        .expect("a period feature result");
    eprintln!("mismatch detail: {}", period.detail);
    assert!(
        !period.passed,
        "a 200-vs-300 ms period must fail: {}",
        period.detail
    );
    assert!(period.detail.contains("period"), "{}", period.detail);
    assert!(
        period.detail.contains("300"),
        "captured value missing: {}",
        period.detail
    );
    assert!(
        period.detail.contains("20"),
        "sim value missing: {}",
        period.detail
    );
    assert!(period.detail.contains("EXCEEDS"), "{}", period.detail);
    assert!(
        period.detail.contains("SYNTHETIC"),
        "a synthetic trace must be bannered: {}",
        period.detail
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ── honesty rules, no sim required ───────────────────────────────────────────

/// Load a one-channel trace on D13, expecting a refusal: `provenance` is the
/// `[trace]` table's provenance line (or nothing), `file` the channel's data
/// file and `feature` the body of its one `[[channel.feature]]`.
fn refusal(provenance: &str, file: &str, feature: &str) -> String {
    let body = format!(
        "[trace]\nboard = \"b\"\nscenario = \"s\"\n{provenance}instrument = \"i\"\n\n\
         [[channel]]\nnet = \"D13\"\nfile = \"{file}\"\n\n[[channel.feature]]\n{feature}\n"
    );
    let path = support::write_tmp("trace.toml", &body);
    hauksbee_ci::hwtrace::Trace::load(&path)
        .unwrap_err()
        .to_string()
}

/// The provenance rule is mandatory: a trace that does not say whether it is
/// real or synthetic must not load.
#[test]
fn trace_without_provenance_is_refused() {
    let err = refusal("", "d13.csv", "kind = \"period\"\nreltol = 0.1");
    assert!(err.contains("provenance"), "got: {err}");
}

/// A voltage feature on a VCD channel is refused by name: a logic analyzer
/// records bits, not volts.
#[test]
fn voltage_feature_on_vcd_is_refused() {
    let msg = refusal(
        "provenance = \"synthetic\"\n",
        "d13.vcd",
        "kind = \"max\"\nabstol = 0.3",
    );
    assert!(msg.contains("bits, not volts"), "got: {msg}");
    assert!(msg.contains("max"), "got: {msg}");
}

/// A feature without any tolerance is refused: hardware traces carry their
/// own error bars and must state them.
#[test]
fn feature_without_tolerance_is_refused() {
    let err = refusal("provenance = \"real\"\n", "d13.csv", "kind = \"period\"");
    assert!(err.contains("error bars"), "got: {err}");
}
