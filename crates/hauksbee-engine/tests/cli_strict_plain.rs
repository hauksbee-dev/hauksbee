//! CLI-level tests for the accessibility surfaces added to `hauksbee run`:
//!
//! - `--strict` (alias `--fail-on-findings`): exits non-zero when a report finds
//!   a real problem, while the default (no flag) stays exit 0.
//! - `--plain` (alias `--explain`): prints the plain-language verdict instead of
//!   the expert table.
//!
//! These exercise the actual compiled binary so the exit-code contract that a CI
//! pipeline depends on is tested end to end, not just the library predicates.

use std::path::PathBuf;
use std::process::Command;

/// The compiled `hauksbee` binary (Cargo sets this for the engine crate's tests).
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

/// Workspace-relative example boards, resolved from this crate's manifest dir.
fn board(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

/// A board that contains two real copper shorts (GND <-> +5V on both layers).
fn shorted_board() -> PathBuf {
    board("../hauksbee-ci/examples/boards/boot_gate.kicad_pcb")
}

/// A board that is clean for the connectivity lint.
fn clean_board() -> PathBuf {
    board("../../testdata/boards/button_pullup.kicad_pcb")
}

fn ac_loop_board() -> PathBuf {
    board("tests/fixtures/ac_loop_one_pole.kicad_pcb")
}

fn ac_loop_models() -> PathBuf {
    board("tests/fixtures/ac_loop_models")
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("hauksbee binary runs")
}

#[test]
fn ampacity_prints_capacity_only_report() {
    let path = std::env::temp_dir().join(format!(
        "hauksbee-ampacity-{}-{}.kicad_pcb",
        std::process::id(),
        "power"
    ));
    std::fs::write(
        &path,
        r#"(kicad_pcb (version 20260206) (generator pcbnew)
  (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
  (net 0 "")
  (net 1 "+BATT")
  (segment (start 0 0) (end 10 0) (width 0.5) (layer "F.Cu") (net 1))
)"#,
    )
    .expect("write temp board");
    let out = run(&["run", path.to_str().unwrap(), "--ampacity"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("capacity only"));
    assert!(stdout.contains("supply a current"));
    assert!(stdout.contains("+BATT"));
}

#[test]
fn drc_without_strict_exits_zero_even_with_shorts() {
    let b = shorted_board();
    let out = run(&["run", b.to_str().unwrap(), "--drc"]);
    assert!(
        out.status.success(),
        "default --drc must stay exit 0 (existing-script contract); got {:?}",
        out.status.code()
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("short"),
        "expert table should list the shorts"
    );
}

#[test]
fn drc_strict_exits_nonzero_on_shorts() {
    let b = shorted_board();
    let out = run(&["run", b.to_str().unwrap(), "--drc", "--strict"]);
    assert!(
        !out.status.success(),
        "--strict must fail the gate when shorts exist"
    );
}

#[test]
fn fail_on_findings_alias_works() {
    let b = shorted_board();
    let out = run(&["run", b.to_str().unwrap(), "--drc", "--fail-on-findings"]);
    assert!(
        !out.status.success(),
        "--fail-on-findings is the documented alias of --strict"
    );
}

#[test]
fn strict_on_clean_board_exits_zero() {
    let b = clean_board();
    // Lint is clean on this board, so --strict must NOT fail.
    let out = run(&["run", b.to_str().unwrap(), "--lint", "--strict"]);
    assert!(
        out.status.success(),
        "--strict must exit 0 when there are no findings; got {:?}",
        out.status.code()
    );
}

#[test]
fn plain_drc_prints_verdict_and_what_why_fix() {
    let b = shorted_board();
    let out = run(&["run", b.to_str().unwrap(), "--drc", "--plain"]);
    assert!(
        out.status.success(),
        "--plain alone does not change exit code"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Verdict line + the three plain sections, in everyday language.
    assert!(
        stdout.contains("serious"),
        "should lead with a verdict counting serious issues"
    );
    assert!(
        stdout.contains("Why it matters:"),
        "each finding explains why"
    );
    assert!(
        stdout.contains("What to do:"),
        "each finding suggests a fix"
    );
    // No raw enum-style token leaks into the plain text.
    assert!(!stdout.contains("ViolationKind"));
}

#[test]
fn explain_alias_matches_plain() {
    let b = shorted_board();
    let a = run(&["run", b.to_str().unwrap(), "--drc", "--plain"]);
    let c = run(&["run", b.to_str().unwrap(), "--drc", "--explain"]);
    assert_eq!(
        String::from_utf8_lossy(&a.stdout),
        String::from_utf8_lossy(&c.stdout),
        "--explain is an alias of --plain"
    );
}

#[test]
fn plain_clean_board_reads_healthy() {
    let b = clean_board();
    let out = run(&["run", b.to_str().unwrap(), "--lint", "--plain"]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_lowercase();
    assert!(
        stdout.contains("healthy"),
        "a clean board should read as healthy"
    );
}

#[test]
fn ac_all_requested_nodes_missing_is_invalid_not_valid() {
    // Honesty hole fix (Finding 1): when EVERY requested --ac-node is absent from
    // the circuit, the AC sweep produced no data for any of them. The tool must
    // refuse that as INVALID for the requested analysis (valid:false + exit 3),
    // not silently report `ac: { valid: true, nets: [] }` with exit 0.
    let b = clean_board();
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--ac",
        "1:1e6:20",
        "--ac-node",
        "/NONEXISTENT",
        "--json",
    ]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "all-missing AC nodes must exit 3 (EXIT_INVALID_FOR_ANALYSIS), got {:?}; stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value =
        serde_json::from_str(&stdout).expect("--json must emit parseable JSON");
    assert_eq!(
        v["ac"]["valid"],
        serde_json::Value::Bool(false),
        "ac.valid must be false when no requested node exists; got: {stdout}"
    );
    let reason = v["ac"]["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("/NONEXISTENT"),
        "the reason must name the missing requested node(s); got: {reason}"
    );
}

#[test]
fn ac_all_requested_nodes_missing_text_warns_and_exits_three() {
    // Same honesty hole on the TEXT surface: a WARNING line + exit 3, never a
    // table presented as a valid result.
    let b = clean_board();
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--ac",
        "1:1e6:20",
        "--ac-node",
        "/NONEXISTENT",
    ]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "text path must also exit 3; got {:?}",
        out.status.code()
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("WARNING") && stderr.contains("not valid"),
        "expected a WARNING that the AC result is not valid; got: {stderr}"
    );
}

#[test]
fn ac_loop_cli_reports_real_one_pole_phase_margin() {
    let b = ac_loop_board();
    let models = ac_loop_models();
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--models-dir",
        models.to_str().unwrap(),
        "--ac",
        "1:1e8:50",
        "--ac-node",
        "OUT",
        "--ac-loop",
        "OUT",
    ]);

    assert!(
        out.status.success(),
        "AC loop fixture should succeed; status={:?}; stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Loop stability at net 'OUT'"),
        "missing loop report: {stdout}"
    );
    assert!(
        stdout.contains("DC/low-f loop gain : 100.00 dB"),
        "missing 100 dB low-frequency gain: {stdout}"
    );
    assert!(
        stdout.contains("gain crossover     :") && stdout.contains("|T| = 0 dB"),
        "missing gain crossover: {stdout}"
    );
    assert!(
        stdout.contains("phase margin       :") && stdout.contains("90."),
        "single-pole loop should report about 90 deg phase margin: {stdout}"
    );
}

#[test]
fn plain_and_strict_compose() {
    // Plain output AND a non-zero exit on the same run.
    let b = shorted_board();
    let out = run(&["run", b.to_str().unwrap(), "--drc", "--plain", "--strict"]);
    assert!(!out.status.success(), "strict still gates with --plain on");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Why it matters:"),
        "plain text still printed"
    );
}

/// A board with two crossing F.Cu tracks on different nets (a real geometric
/// short), written at the given `.kicad_pcb` format version.
fn crossing_short_board(version: u32, tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "hauksbee-vergate-{}-{}-{}.kicad_pcb",
        std::process::id(),
        version,
        tag
    ));
    std::fs::write(
        &path,
        format!(
            "(kicad_pcb (version {version}) (generator pcbnew)\n\
             \x20 (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal))\n\
             \x20 (net 0 \"\")\n\
             \x20 (net 1 \"GND\")\n\
             \x20 (net 2 \"VCC\")\n\
             \x20 (segment (start 0 5) (end 10 5) (width 1.0) (layer \"F.Cu\") (net 1))\n\
             \x20 (segment (start 5 0) (end 5 10) (width 1.0) (layer \"F.Cu\") (net 2))\n)"
        ),
    )
    .expect("write temp board");
    path
}

/// The safety-critical CI-gating contract for unvalidated board formats: a real
/// geometric short on a KiCad-10 (20260206) board must NOT fail `--strict` (its
/// shorts may be phantom and can't be cross-checked), while the identical short
/// on a validated KiCad-7 (20221018) board MUST fail `--strict`. Same geometry,
/// only the format version differs — so this pins the version gate, not the DRC.
#[test]
fn strict_gate_ignores_shorts_on_unvalidated_kicad10_but_not_validated() {
    // Validated (KiCad 7): the short gates the build.
    let validated = crossing_short_board(20221018, "validated");
    let out = run(&["run", validated.to_str().unwrap(), "--drc", "--strict"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a real short on a validated board must fail --strict"
    );
    assert!(String::from_utf8_lossy(&out.stdout).to_lowercase().contains("short"));

    // Unvalidated (KiCad 10): the same short does NOT gate (caveat printed).
    let unvalidated = crossing_short_board(20260206, "unvalidated");
    let out = run(&["run", unvalidated.to_str().unwrap(), "--drc", "--strict"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a possibly-phantom short on an unvalidated KiCad-10 board must NOT fail --strict"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("UNRELIABLE"),
        "the unreliable-version caveat must be printed: {stdout}"
    );

    // And in --check the same divergence holds (the aggregate gate).
    let out = run(&["run", unvalidated.to_str().unwrap(), "--check", "--strict"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "--check --strict must not fail on unvalidated-version shorts"
    );
}

/// The boot-safety advisory, end to end through the compiled binary: a board
/// whose firmware drives a transistor-gate net HIGH and holds it from reset
/// (boot_gate + variant A) is named in `--headless --json` as a
/// `boot_control_net` note, advisory-only by default (exit 0), and escalated to
/// exit 2 under `--strict-boot`. Uses committed fixtures.
#[test]
fn boot_advisory_emits_note_and_strict_boot_gates() {
    let b = board("../hauksbee-ci/examples/boards/boot_gate.kicad_pcb");
    let fw = board("../../testdata/firmware/boot_gate_a/boot_gate.hex");
    if !fw.exists() {
        eprintln!("skipping: boot_gate_a firmware not built");
        return;
    }
    let (b, fw) = (b.to_str().unwrap(), fw.to_str().unwrap());

    // Advisory present in JSON, and NOT a gate by default.
    let out = run(&["run", b, "--firmware", fw, "--headless", "--seconds", "0.05", "--json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"boot_control_net\""),
        "expected a boot_control_net note for the held-high GATE_CTRL; got:\n{stdout}"
    );
    assert!(
        out.status.success(),
        "a boot advisory must NOT fail the run without --strict-boot"
    );

    // --strict-boot escalates it to exit 2.
    let strict = run(&[
        "run", b, "--firmware", fw, "--headless", "--seconds", "0.05", "--strict-boot",
    ]);
    assert_eq!(
        strict.status.code(),
        Some(2),
        "--strict-boot must exit 2 when a held-high switch net has no bias"
    );
}

/// A clean board whose firmware only toggles a signal (no switch-driving net
/// held high) raises NO boot advisory and is not gated by --strict-boot.
#[test]
fn clean_firmware_raises_no_boot_advisory() {
    let b = board("../hauksbee-ci/examples/boards/blinky.kicad_pcb");
    let fw = board("../../testdata/firmware/demo/demo.hex");
    if !fw.exists() {
        eprintln!("skipping: demo firmware not present");
        return;
    }
    let (b, fw) = (b.to_str().unwrap(), fw.to_str().unwrap());
    let out = run(&[
        "run", b, "--firmware", fw, "--headless", "--seconds", "0.1", "--json", "--strict-boot",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("\"boot_control_net\""),
        "a toggling signal must not raise a boot advisory; got:\n{stdout}"
    );
    assert!(
        out.status.success(),
        "--strict-boot must exit 0 when there is no boot advisory"
    );
}
