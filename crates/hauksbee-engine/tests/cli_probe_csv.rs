//! `hauksbee run --headless --probe NET[,NET...] --probe-csv <path>`: record the
//! named nets' node voltages each chunk and write them to a CSV, so waveforms are
//! scriptable with no UI.
//!
//! This drives the compiled binary on a small board and checks the CSV contract a
//! script depends on: it exists, its header is `time_s` then one column per
//! probed net, and it has about one row per simulated millisecond (the headless
//! chunk cadence). An unknown net must fail before the run, with a suggestion.

use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

/// A small board with a +5V rail and a BTN net (no firmware needed: the analog
/// co-sim still advances and samples nets each chunk).
fn board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/boards/button_pullup.kicad_pcb")
}

fn out_csv(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hauksbee_probe_{}_{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("probe.csv");
    let _ = std::fs::remove_file(&p);
    p
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("hauksbee binary runs")
}

#[test]
fn probe_writes_csv_with_header_and_plausible_rows() {
    let b = board();
    let csv = out_csv("ok");
    let seconds = 0.05; // 50 ms at the 1 kHz headless cadence => ~50 rows.
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--headless",
        "--seconds",
        "0.05",
        "--probe",
        "+5V,BTN",
        "--probe-csv",
        csv.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "headless probe run should succeed; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(csv.exists(), "the probe CSV should be written to disk");

    let body = std::fs::read_to_string(&csv).unwrap();
    let mut lines = body.lines();

    // Header: time_s then one column per probed net, in the order requested.
    assert_eq!(
        lines.next(),
        Some("time_s,+5V,BTN"),
        "header is time_s then a column per probed net"
    );

    // One row per chunk, each with three fields (time + two nets). Row count is
    // ~seconds/frame_dt; allow a small slack around the loop boundary.
    let rows: Vec<&str> = lines.filter(|l| !l.is_empty()).collect();
    let expected = (seconds * 1000.0) as usize; // 1 kHz cadence
    assert!(
        rows.len().abs_diff(expected) <= 2,
        "expected ~{expected} rows, got {}",
        rows.len()
    );
    for row in &rows {
        assert_eq!(
            row.split(',').count(),
            3,
            "each row is time_s + one value per probed net: {row}"
        );
    }
}

#[test]
fn unknown_probe_net_fails_before_run_with_suggestion() {
    let b = board();
    let csv = out_csv("unknown");
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--headless",
        "--seconds",
        "0.01",
        "--probe",
        "+5W", // a typo for +5V
        "--probe-csv",
        csv.to_str().unwrap(),
    ]);
    assert!(!out.status.success(), "an unknown probed net must fail the run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("did you mean") && stderr.contains("+5V"),
        "the error should suggest the near-match +5V; got:\n{stderr}"
    );
    assert!(!csv.exists(), "no CSV should be written when validation fails");
}
