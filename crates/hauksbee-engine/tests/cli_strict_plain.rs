//! `hauksbee run` through the compiled binary: exit codes (0 clean, 1 input
//! error, 2 usage error / gated findings, 3 invalid for analysis), the
//! `--plain` / `--json` / `--report` / `--drc` / `--lint` / `--check`
//! surfaces, the CI artifacts, the input-refusal surfaces, `doctor`, and the
//! boot-safety advisory over real AVR firmware.
//!
//! Merged from cli_strict_plain, cli_round2_fixes, cli_round3_fixes,
//! cli_firmware_guard, cli_probe_csv, cli_doctor, cli_eagle_tie_contract,
//! bom_placement_cli and run_report_schema_drift.

use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

/// A path relative to this crate's manifest dir.
fn board(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

/// A repo-root relative path.
fn repo(rel: &str) -> PathBuf {
    board("../..").join(rel)
}

/// A board that is clean for every static check: a resistor and a switch.
fn clean_board() -> PathBuf {
    repo("testdata/boards/button_pullup.kicad_pcb")
}

/// The landing-page blinky sample (ATmega328P + LED, routed).
fn blinky_board() -> PathBuf {
    repo("frontend/public/samples/blinky.kicad_pcb")
}

/// The landing-page boot-gate sample: a transistor gate net (GATE_CTRL) the
/// boot_gate firmware drives HIGH from reset.
fn boot_gate_board() -> PathBuf {
    repo("frontend/public/samples/boot_gate.kicad_pcb")
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .env_remove("GITHUB_ACTIONS")
        .output()
        .expect("hauksbee binary runs")
}

fn stdout(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hauksbee-cli-{}-{name}", std::process::id()))
}

/// A board with two crossing F.Cu tracks on different nets (a real geometric
/// short between GND and VCC), written at the given `.kicad_pcb` format
/// version, plus a benign two-resistor divider so the board is not refused as
/// component-free on the part-level paths.
fn crossing_short_board(version: u32, tag: &str) -> PathBuf {
    let path = tmp(&format!("short-{version}-{tag}.kicad_pcb"));
    std::fs::write(
        &path,
        format!(
            "(kicad_pcb (version {version}) (generator pcbnew)\n\
             \x20 (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal))\n\
             \x20 (net 0 \"\")\n\
             \x20 (net 1 \"GND\")\n\
             \x20 (net 2 \"VCC\")\n\
             \x20 (net 3 \"MID\")\n\
             \x20 (footprint \"Resistor_SMD:R_0603\" (layer \"F.Cu\")\n\
             \x20   (at 20 20)\n\
             \x20   (property \"Reference\" \"R1\")\n\
             \x20   (property \"Value\" \"10k\")\n\
             \x20   (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net 2 \"VCC\"))\n\
             \x20   (pad \"2\" smd rect (at 2 0) (size 1 1) (layers \"F.Cu\") (net 3 \"MID\"))\n\
             \x20 )\n\
             \x20 (footprint \"Resistor_SMD:R_0603\" (layer \"F.Cu\")\n\
             \x20   (at 20 25)\n\
             \x20   (property \"Reference\" \"R2\")\n\
             \x20   (property \"Value\" \"10k\")\n\
             \x20   (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net 3 \"MID\"))\n\
             \x20   (pad \"2\" smd rect (at 2 0) (size 1 1) (layers \"F.Cu\") (net 1 \"GND\"))\n\
             \x20 )\n\
             \x20 (segment (start 0 5) (end 10 5) (width 1.0) (layer \"F.Cu\") (net 1))\n\
             \x20 (segment (start 5 0) (end 5 10) (width 1.0) (layer \"F.Cu\") (net 2))\n)"
        ),
    )
    .expect("write temp board");
    path
}

/// A validated-format (KiCad 7) board carrying one real copper short.
fn shorted_board(tag: &str) -> PathBuf {
    crossing_short_board(20221018, tag)
}

// ── --strict and the exit-code contract ─────────────────────────────────────

#[test]
fn drc_strict_gates_shorts_and_the_default_does_not() {
    let b = shorted_board("strict");
    let b = b.to_str().unwrap();

    let lax = run(&["run", b, "--drc"]);
    assert_eq!(lax.status.code(), Some(0), "default --drc stays exit 0");
    assert!(stdout(&lax).contains("short"), "the table lists the short");

    let strict = run(&["run", b, "--drc", "--strict"]);
    assert_eq!(strict.status.code(), Some(2), "--strict fails the gate");
    assert!(
        stdout(&strict).contains("FAILED under --strict:"),
        "text mode says why it exits 2:\n{}",
        stdout(&strict)
    );
    let alias = run(&["run", b, "--drc", "--fail-on-findings"]);
    assert_eq!(alias.status.code(), Some(2), "--fail-on-findings is the alias");

    // --json keeps stdout one JSON document; the gate line goes to stderr.
    let json = run(&["run", b, "--drc", "--strict", "--json"]);
    assert_eq!(json.status.code(), Some(2));
    serde_json::from_str::<serde_json::Value>(&stdout(&json))
        .expect("stdout stays one valid JSON document under --strict");
    assert!(stderr(&json).contains("FAILED under --strict:"));

    // Plain output AND a non-zero exit on the same run.
    let plain = run(&["run", b, "--drc", "--plain", "--strict"]);
    assert_eq!(plain.status.code(), Some(2));
    assert!(stdout(&plain).contains("Why it matters:"));
}

#[test]
fn bare_json_strict_gates_a_shorted_board() {
    let b = shorted_board("bare-json");
    let strict = run(&["run", b.to_str().unwrap(), "--json", "--strict"]);
    assert_eq!(
        strict.status.code(),
        Some(2),
        "bare --json --strict must gate; stderr={}",
        stderr(&strict)
    );
    let v: serde_json::Value =
        serde_json::from_str(stdout(&strict).trim()).expect("--json --strict emits valid JSON");
    assert!(v.get("drc").is_some(), "the combined JSON carries the DRC block");
    assert_eq!(v["verdict"], "fail");
    assert_eq!(v["ok"], false);

    let lax = run(&["run", b.to_str().unwrap(), "--json"]);
    assert_eq!(lax.status.code(), Some(0), "bare --json stays exit 0");
}

#[test]
fn strict_on_a_clean_board_exits_zero() {
    let b = clean_board();
    for flags in [["--lint", "--strict"], ["--check", "--strict"]] {
        let out = run(&["run", b.to_str().unwrap(), flags[0], flags[1]]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{flags:?} must exit 0 with no findings; stderr={}",
            stderr(&out)
        );
    }
    let json = run(&["run", b.to_str().unwrap(), "--check", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&json)).expect("one JSON document");
    assert_eq!(v["verdict"], "pass");
    assert_eq!(v["ok"], true);
    assert_eq!(v["serious_count"], 0);
}

/// Same geometry, only the format version differs: a real short on a validated
/// KiCad-7 board gates `--strict`; the identical short on an unvalidated
/// KiCad-10 board does not (it may be phantom), and the caveat is printed.
#[test]
fn strict_gate_ignores_shorts_on_unvalidated_kicad10_but_not_validated() {
    let validated = crossing_short_board(20221018, "validated");
    let out = run(&["run", validated.to_str().unwrap(), "--drc", "--strict"]);
    assert_eq!(out.status.code(), Some(2));

    let unvalidated = crossing_short_board(20260206, "unvalidated");
    let out = run(&["run", unvalidated.to_str().unwrap(), "--drc", "--strict"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).contains("UNRELIABLE"), "{}", stdout(&out));

    let out = run(&["run", unvalidated.to_str().unwrap(), "--check", "--json", "--strict"]);
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    assert_eq!(v["verdict"], "pass");
    assert_eq!(
        v["drc"]["shorts"].as_array().map(Vec::len),
        Some(1),
        "the possibly-phantom short is still reported, not dropped:\n{v}"
    );
}

// ── --plain ─────────────────────────────────────────────────────────────────

#[test]
fn plain_drc_prints_verdict_and_what_why_fix() {
    let b = shorted_board("plain");
    let out = run(&["run", b.to_str().unwrap(), "--drc", "--plain"]);
    assert_eq!(out.status.code(), Some(0), "--plain alone does not change exit code");
    let text = stdout(&out);
    assert!(text.contains("serious"), "leads with a verdict counting serious issues");
    assert!(text.contains("Why it matters:") && text.contains("What to do:"));
    assert!(!text.contains("ViolationKind"), "no raw enum token leaks");

    let explain = run(&["run", b.to_str().unwrap(), "--drc", "--explain"]);
    assert_eq!(text, stdout(&explain), "--explain is an alias of --plain");
}

#[test]
fn plain_clean_board_reads_healthy_and_check_ends_on_one_verdict_line() {
    let b = clean_board();
    let out = run(&["run", b.to_str().unwrap(), "--lint", "--plain"]);
    assert!(stdout(&out).to_lowercase().contains("healthy"));

    // Bare --plain is the prose full report, not a hint.
    let out = run(&["run", blinky_board().to_str().unwrap(), "--plain"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("VERDICT:"));

    let out = run(&["run", b.to_str().unwrap(), "--check"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    let last = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
    assert!(last.starts_with("VERDICT: "), "the last line is the verdict: {last:?}");
    assert!(last.contains("serious") && last.contains("worth a look"));
}

/// `--check --plain` must carry the bind-role honesty: a board with an
/// unmodellable active IC (U1 XLOGIC9999) must not read as fully covered.
#[test]
fn plain_check_surfaces_open_active_ic_bind_honesty() {
    let b = board("tests/fixtures/plain_check_open_active_ic.kicad_pcb");
    let out = run(&["run", b.to_str().unwrap(), "--check", "--plain"]);
    assert_eq!(out.status.code(), Some(0), "no gate without --strict");
    let text = stdout(&out);
    assert!(
        text.contains("active IC(s) are unresolved")
            && text.contains("INCOMPLETE")
            && text.contains("--models-dir"),
        "plain --check must surface the open-active-IC heads-up; got:\n{text}"
    );
    assert!(
        text.contains("--report") && !text.contains("--bind"),
        "the heads-up must point at the real --report flag"
    );
}

#[test]
fn every_analysis_surface_accepts_plain() {
    let b = blinky_board();
    let ampacity = run(&["run", b.to_str().unwrap(), "--ampacity", "--plain"]);
    assert_eq!(ampacity.status.code(), Some(0), "{}", stderr(&ampacity));
    let thermal = run(&[
        "run",
        b.to_str().unwrap(),
        "--thermal",
        "--plain",
        "--seconds",
        "0.05",
    ]);
    assert_ne!(
        thermal.status.code(),
        Some(1),
        "--plain must not be rejected as a flag error: {}",
        stderr(&thermal)
    );
    let res = run(&["run", clean_board().to_str().unwrap(), "--resources"]);
    assert_eq!(res.status.code(), Some(0));
    assert!(stdout(&res).contains("resource-conflicts:"));
}

// ── --ac ────────────────────────────────────────────────────────────────────

#[test]
fn ac_with_every_requested_node_missing_is_invalid_not_valid() {
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
        "all-missing AC nodes must exit 3; stderr={}",
        stderr(&out)
    );
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("--json parses");
    assert_eq!(v["ac"]["valid"], false);
    assert!(v["ac"]["not_found_nets"]
        .as_array()
        .expect("not_found_nets is an array")
        .iter()
        .any(|n| n.as_str() == Some("/NONEXISTENT")));

    let text = run(&["run", b.to_str().unwrap(), "--ac", "1:1e6:20", "--ac-node", "/NONEXISTENT"]);
    assert_eq!(text.status.code(), Some(3), "text path also exits 3");
    assert!(!stderr(&text).contains("WARNING"), "a typo'd node is an error");
}

#[test]
fn ac_loop_cli_reports_real_one_pole_phase_margin() {
    let b = board("tests/fixtures/ac_loop_one_pole.kicad_pcb");
    let models = board("tests/fixtures/ac_loop_models");
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
    assert!(out.status.success(), "stderr={}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("Loop stability at net 'OUT'"), "{text}");
    assert!(text.contains("DC/low-f loop gain : 100.00 dB"), "{text}");
    assert!(text.contains("gain crossover     :") && text.contains("|T| = 0 dB"), "{text}");
    assert!(
        text.contains("phase margin       :") && text.contains("90."),
        "single-pole loop reports ~90 deg phase margin: {text}"
    );
}

// ── artifacts and inputs ────────────────────────────────────────────────────

#[test]
fn junit_and_sarif_artifacts_are_written_and_valid() {
    let b = shorted_board("artifacts");
    let junit = tmp("report.junit.xml");
    let sarif = tmp("report.sarif");
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--drc",
        "--junit",
        junit.to_str().unwrap(),
        "--sarif",
        sarif.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let jx = std::fs::read_to_string(&junit).expect("junit written");
    assert!(jx.contains("<testsuites") && jx.contains("<failure"), "{jx}");
    let sj: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sarif).expect("sarif written"))
            .expect("sarif parses");
    assert_eq!(sj["version"], "2.1.0");
    assert!(sj["runs"][0]["results"].as_array().is_some_and(|r| !r.is_empty()));
}

#[test]
fn asbuilt_overlay_is_applied_and_a_bad_one_is_an_error() {
    let b = shorted_board("asbuilt");
    for flag in ["--report", "--drc", "--check"] {
        let out = run(&[
            "run",
            b.to_str().unwrap(),
            flag,
            "--asbuilt",
            "/nonexistent/overlay.asbuilt.toml",
        ]);
        assert!(!out.status.success(), "{flag} must hard-error on a missing overlay");
    }
    let overlay = tmp("jumper.asbuilt.toml");
    std::fs::write(&overlay, "[[jumper]]\nfrom = \"GND\"\nto = \"MID\"\n").unwrap();
    let plain = run(&["run", b.to_str().unwrap(), "--report"]);
    let with = run(&[
        "run",
        b.to_str().unwrap(),
        "--report",
        "--asbuilt",
        overlay.to_str().unwrap(),
    ]);
    assert!(with.status.success(), "{}", stderr(&with));
    assert!(stdout(&with).contains("as-built overlay"));
    assert_ne!(stdout(&plain), stdout(&with), "an overlay changes the report");
    let bogus = tmp("bogus.asbuilt.toml");
    std::fs::write(
        &bogus,
        "[[replace]]\nref = \"NOSUCHPART99\"\n[replace.set]\nohms = 10.0\n",
    )
    .unwrap();
    let bad = run(&[
        "run",
        b.to_str().unwrap(),
        "--report",
        "--asbuilt",
        bogus.to_str().unwrap(),
    ]);
    assert!(!bad.status.success());
    assert!(stderr(&bad).contains("NOSUCHPART99"));
}

#[test]
fn artifact_flags_without_their_analysis_are_errors() {
    let b = clean_board();
    let cases: &[&[&str]] = &[
        &["--probe-csv", "/tmp/x.csv"],
        &["--ac-csv", "/tmp/x.csv"],
        &["--ac-node", "OUT"],
        &["--ampacity", "--json"],
    ];
    for extra in cases {
        let mut args = vec!["run", b.to_str().unwrap()];
        args.extend_from_slice(extra);
        let out = run(&args);
        assert!(!out.status.success(), "{extra:?} must be refused, not silently ignored");
    }
    // Contradictory selected surfaces are a usage error (exit 2).
    let out = run(&["run", b.to_str().unwrap(), "--drc", "--headless", "--json"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("cannot be used with"));
}

#[test]
fn escaped_net_names_are_displayed_as_real_names() {
    let p = tmp("slashnet.kicad_pcb");
    std::fs::write(
        &p,
        r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
  (net 0 "")
  (net 1 "/GPIO0{slash}XTAL1")
  (net 2 "SCL_{2}")
  (module Resistor_SMD:R_0402_1005Metric (layer F.Cu)
    (at 100 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 1 "/GPIO0{slash}XTAL1"))
    (pad 2 smd rect (at 1 0) (net 2 "SCL_{2}"))
  )
  (segment (start 0 0) (end 5 0) (width 0.5) (layer "F.Cu") (net 1))
  (segment (start 0 2) (end 5 2) (width 0.5) (layer "F.Cu") (net 2))
)
"#,
    )
    .unwrap();
    let out = run(&["run", p.to_str().unwrap(), "--list-nets"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("/GPIO0/XTAL1") && text.contains("SCL_2"), "{text}");
    assert!(!text.contains("{slash}") && !text.contains("{2}"), "{text}");
    let json = run(&["run", p.to_str().unwrap(), "--list-nets", "--json"]);
    assert!(stdout(&json).contains("/GPIO0/XTAL1") && !stdout(&json).contains("{slash}"));
}

// ── refusal surfaces ────────────────────────────────────────────────────────

#[test]
fn missing_firmware_is_a_clean_exit_1_not_a_segfault() {
    let out = run(&[
        "run",
        blinky_board().to_str().unwrap(),
        "--firmware",
        "does_not_exist.hex",
        "--headless",
        "--seconds",
        "0.05",
    ]);
    assert_eq!(out.status.code(), Some(1), "got {:?}: {}", out.status, stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("does_not_exist.hex") && err.contains("no firmware file"), "{err}");
    assert!(!err.contains("panicked"));
}

#[test]
fn unknown_example_is_an_input_error_with_a_json_envelope() {
    let out = run(&["run", "--example", "bogus", "--check"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!stderr(&out).to_lowercase().contains("backtrace"));

    let out = run(&["run", "--example", "bogus", "--json"]);
    let v: serde_json::Value = serde_json::from_str(stdout(&out).trim())
        .unwrap_or_else(|e| panic!("--json stays JSON on this error path: {e}"));
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap_or_default().contains("bogus"));
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn numeric_flag_bounds_are_usage_errors_and_negative_numbers_parse() {
    let b = blinky_board();
    for bad in ["2000", "-300", "1e308", "nan"] {
        let out = run(&["run", b.to_str().unwrap(), "--thermal", "--ambient", bad]);
        assert_eq!(out.status.code(), Some(2), "usage error for --ambient {bad}");
        assert!(stderr(&out).lines().all(|l| l.len() < 300), "no exploded rendering");
    }
    let out = run(&["run", b.to_str().unwrap(), "--drc", "--ambient", "-40"]);
    assert_eq!(out.status.code(), Some(0), "-40 C parses: {}", stderr(&out));
    for bad in ["0", "-1", "1e307"] {
        let out = run(&["run", b.to_str().unwrap(), "--headless", "--seconds", bad]);
        assert_eq!(out.status.code(), Some(2), "--seconds {bad} refused before any run");
    }
}

#[test]
fn unsupported_and_malformed_inputs_are_named_at_exit_1() {
    // A KiCad project file points at the sibling board.
    let prl = repo("testdata/boards/button_pullup.kicad_prl");
    let out = run(&["run", prl.to_str().unwrap(), "--check"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("button_pullup.kicad_pcb"), "{}", stderr(&out));

    // A corrupt file with a recognized extension: content, not format.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("broken.kicad_pcb");
    std::fs::write(&p, "this is not an s-expression at all").unwrap();
    let out = run(&["run", p.to_str().unwrap(), "--check"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("did not parse"), "{}", stderr(&out));

    // A Git LFS pointer is a pointer, not a board.
    let lfs = dir.path().join("pointer.kicad_pcb");
    std::fs::write(
        &lfs,
        "version https://git-lfs.github.com/spec/v1\n\
         oid sha256:0000000000000000000000000000000000000000000000000000000000000000\n\
         size 12345\n",
    )
    .unwrap();
    let out = run(&["run", lfs.to_str().unwrap(), "--check"]);
    assert_ne!(out.status.code(), Some(0), "an LFS pointer must not analyse as a board");
    assert!(
        stderr(&out).to_lowercase().contains("lfs"),
        "the refusal names the LFS pointer: {}",
        stderr(&out)
    );

    // An unsupported format.
    let txt = dir.path().join("notes.txt");
    std::fs::write(&txt, "hello").unwrap();
    let out = run(&["run", txt.to_str().unwrap(), "--check"]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));

    // A missing file.
    let out = run(&["run", "/definitely/not/here.kicad_pcb", "--check"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn a_directory_holding_boards_is_resolved_or_disambiguated() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(clean_board(), dir.path().join("only.kicad_pcb")).unwrap();
    let out = run(&["run", dir.path().to_str().unwrap(), "--drc"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("using the board file inside it"));

    std::fs::copy(clean_board(), dir.path().join("second.kicad_pcb")).unwrap();
    let out = run(&["run", dir.path().to_str().unwrap(), "--drc"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("holding 2 board files"), "{}", stderr(&out));
}

#[test]
fn the_nearby_boards_note_never_pollutes_stdout_and_quiet_silences_it() {
    let b = blinky_board();
    let out = run(&["run", b.to_str().unwrap(), "--json"]);
    assert!(!stdout(&out).contains("found nearby") && !stdout(&out).contains("note:"));
    let out = run(&["run", b.to_str().unwrap(), "--report", "--quiet"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!stdout(&out).contains("found nearby") && !stderr(&out).contains("found nearby"));
}

// ── --probe ─────────────────────────────────────────────────────────────────

#[test]
fn probe_writes_csv_with_header_and_plausible_rows() {
    let b = clean_board();
    let csv = tmp("probe.csv");
    let _ = std::fs::remove_file(&csv);
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
    assert!(out.status.success(), "stderr:\n{}", stderr(&out));
    let body = std::fs::read_to_string(&csv).expect("the probe CSV is written");
    let mut lines = body.lines();
    assert_eq!(lines.next(), Some("time_s,+5V,BTN"));
    let rows: Vec<&str> = lines.filter(|l| !l.is_empty()).collect();
    assert!(rows.len().abs_diff(50) <= 2, "~50 rows at 1 kHz, got {}", rows.len());
    for row in &rows {
        assert_eq!(row.split(',').count(), 3, "{row}");
    }

    let csv2 = tmp("probe-unknown.csv");
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--headless",
        "--seconds",
        "0.01",
        "--probe",
        "+5W",
        "--probe-csv",
        csv2.to_str().unwrap(),
    ]);
    assert!(!out.status.success(), "an unknown probed net fails the run");
    assert!(stderr(&out).contains("did you mean") && stderr(&out).contains("+5V"));
    assert!(!csv2.exists(), "no CSV when validation fails");
}

// ── doctor ──────────────────────────────────────────────────────────────────

#[test]
fn doctor_backends_is_a_machine_table_and_json_is_well_formed() {
    const KNOWN_STATUSES: &[&str] = &["ok", "absent", "builtin", "disabled"];
    let out = run(&["doctor", "--backends"]);
    assert!(out.status.success(), "a missing backend is information, not failure");
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    assert!(!lines.is_empty());
    for line in &lines {
        let fields: Vec<&str> = line.splitn(3, '\t').collect();
        assert_eq!(fields.len(), 3, "NAME<TAB>STATUS<TAB>DETAIL: {line:?}");
        assert!(KNOWN_STATUSES.contains(&fields[1]), "status token: {line:?}");
    }
    assert_eq!(
        lines.iter().filter(|l| l.split('\t').next() == Some("avr")).count(),
        1,
        "the avr backend is reported exactly once:\n{text}"
    );
    let bare = run(&["doctor"]);
    assert!(bare.status.success() && stdout(&bare).contains("avr"));

    let out = run(&["doctor", "--backends", "--json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let arr = v["backends"].as_array().expect("`backends` is an array");
    assert!(arr.iter().any(|b| b["name"] == "avr"));
    for b in arr {
        let status = b["status"].as_str().expect("status is a string");
        let available = b["available"].as_bool().expect("available is a bool");
        assert_eq!(available, status == "ok" || status == "builtin", "{b}");
    }
}

// ── companion inputs: schematic, BOM, placement ─────────────────────────────

#[test]
fn eagle_schematic_context_never_downgrades_the_json_exit_or_junit() {
    let dir = tempfile::tempdir().unwrap();
    let brd = dir.path().join("declared.brd");
    let sch = dir.path().join("declared.sch");
    let junit = dir.path().join("drc.xml");
    std::fs::write(
        &brd,
        include_bytes!("../../hauksbee-extract/tests/fixtures/eagle_ties/declared.brd"),
    )
    .unwrap();
    std::fs::write(
        &sch,
        include_bytes!("../../hauksbee-extract/tests/fixtures/eagle_ties/declared.sch"),
    )
    .unwrap();
    let out = Command::new(bin())
        .arg("run")
        .arg(&brd)
        .args(["--drc", "--json", "--strict", "--schematic"])
        .arg(&sch)
        .arg("--junit")
        .arg(&junit)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["verdict"], "fail");
    assert!(json["drc"]["shorts"]
        .as_array()
        .unwrap()
        .iter()
        .all(|short| short["severity"] == "serious"));
    let input = json["inputs"]
        .as_array()
        .expect("top-level input inventory")
        .iter()
        .find(|input| input["kind"] == "schematic")
        .expect("the schematic is a top-level input");
    assert_eq!(input["format"], "eagle_schematic");
    assert_eq!(input["sha256"].as_str().map(str::len), Some(64));
    let junit = std::fs::read_to_string(junit).expect("JUnit finalized");
    assert!(junit.contains("<failure"), "{junit}");
}

#[test]
fn bom_and_placement_feed_the_json_inventory_and_a_conflicting_bom_refuses() {
    let dir = tempfile::tempdir().expect("temp directory");
    let board = dir.path().join("board.kicad_pcb");
    let bom = dir.path().join("bom.csv");
    let placement = dir.path().join("positions.csv");
    std::fs::write(
        &board,
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (module Package_DFN_QFN:QFN-10 (layer F.Cu)
    (at 10 20)
    (fp_text reference U9 (at 0 0) (layer F.SilkS))
    (fp_text value "" (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 1 "GND"))
    (pad 2 smd rect (at 1 0) (net 1 "GND"))
    (pad 3 smd rect (at 2 0) (net 1 "GND"))
    (pad 4 smd rect (at 3 0) (net 1 "GND"))
    (pad 5 smd rect (at 4 0) (net 1 "GND"))
    (pad 6 smd rect (at 5 0) (net 1 "GND"))
    (pad 7 smd rect (at 6 0) (net 1 "GND"))
    (pad 8 smd rect (at 7 0) (net 1 "GND"))
    (pad 9 smd rect (at 8 0) (net 1 "GND"))
    (pad 10 smd rect (at 9 0) (net 1 "GND"))))"#,
    )
    .unwrap();
    std::fs::write(
        &bom,
        "Assembly Ref,Value,MPN,Manufacturer,Footprint\nU9,,MCP4728,Microchip,QFN-10\n",
    )
    .unwrap();
    std::fs::write(
        &placement,
        "Designator,Val,Package,Mid X,Mid Y,Rotation,Layer\nU9,MCP4728,QFN-10,10,20,0,top\n",
    )
    .unwrap();
    let out = run(&[
        "run",
        board.to_str().unwrap(),
        "--bom",
        bom.to_str().unwrap(),
        "--bom-column",
        "reference=Assembly Ref",
        "--placement",
        placement.to_str().unwrap(),
        "--report",
        "--json",
    ]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON document");
    let inputs = json["inputs"].as_array().expect("input inventory");
    assert_eq!(
        inputs.iter().map(|i| &i["kind"]).collect::<Vec<_>>(),
        vec!["board", "bom", "placement"],
        "{json}"
    );
    assert!(inputs.iter().any(|input| {
        input["identity"].as_array().is_some_and(|lines| lines
            .iter()
            .any(|l| l.as_str().is_some_and(|s| s.contains("U9 identified"))))
    }));

    std::fs::write(&bom, "Assembly Ref,Value,MPN\nU9,,MCP4728\nU9,,STM32F103C8\n").unwrap();
    let refused = run(&[
        "run",
        board.to_str().unwrap(),
        "--bom",
        bom.to_str().unwrap(),
        "--bom-column",
        "reference=Assembly Ref",
        "--report",
        "--json",
    ]);
    assert_eq!(refused.status.code(), Some(3), "{}", stdout(&refused));
    let error: serde_json::Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(error["ok"], false);
    assert_eq!(error["verdict"], "invalid");
}

#[test]
fn watchy_position_exports_reconcile_and_ambiguous_artifacts_refuse() {
    let board = repo("frontend/public/samples/watchy.kicad_pcb");
    for relative in [
        "../hauksbee-extract/tests/fixtures/placement/watchy.pos",
        "../hauksbee-extract/tests/fixtures/placement/watchy-pos.csv",
    ] {
        let placement = crate::board(relative);
        let out = run(&[
            "run",
            board.to_str().unwrap(),
            "--placement",
            placement.to_str().unwrap(),
            "--report",
            "--json",
        ]);
        assert!(out.status.success(), "{relative}: {}", stderr(&out));
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let reconciliation = json["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|input| input["kind"] == "placement")
            .and_then(|input| input["identity"].as_array())
            .and_then(|lines| {
                lines.iter().find_map(|line| {
                    line.as_str()
                        .filter(|line| line.contains("placement reconciliation:"))
                })
            })
            .unwrap_or_else(|| panic!("reconciliation evidence missing: {json}"));
        for fact in ["75 of 75 placements match", "Y axis mirrored"] {
            assert!(reconciliation.contains(fact), "{fact}: {reconciliation}");
        }
    }

    let dir = tempfile::tempdir().unwrap();
    for (flag, name, body) in [
        (
            "placement",
            "ambiguous-placement.csv",
            "Designator,Mid X,X,Mid Y,Rotation,Layer\nU4,86.12,999,-85.91,-90,Top\n",
        ),
        (
            "bom",
            "duplicate-nonnumeric.csv",
            "Designator,Value,MPN\nRX,receiver,SN65HVD230\nRX,receiver,MCP2562\n",
        ),
    ] {
        let artifact = dir.path().join(name);
        std::fs::write(&artifact, body).unwrap();
        let out = run(&[
            "run",
            board.to_str().unwrap(),
            &format!("--{flag}"),
            artifact.to_str().unwrap(),
            "--report",
            "--json",
        ]);
        assert_eq!(out.status.code(), Some(3), "{name} is invalid for analysis");
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(json["error"].is_string(), "structured refusal: {json}");
    }
}

// ── the --json schema ───────────────────────────────────────────────────────

/// The checked-in `schemas/hauksbee-run-report.schema.json` is GENERATED from
/// `JsonReport`; regenerate with
/// `UPDATE_RUN_SCHEMA=1 cargo test -p hauksbee-engine --test cli_strict_plain`.
#[test]
fn run_report_schema_file_matches_the_report_type() {
    use hauksbee_engine::result::{JsonReport, RUN_REPORT_SCHEMA_VERSION};
    use schemars::generate::SchemaSettings;
    use serde_json::json;

    let mut schema = SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<JsonReport>();
    let obj = schema.ensure_object();
    obj.insert("$schema".into(), json!("http://json-schema.org/draft-07/schema#"));
    obj.insert("title".into(), json!("hauksbee run --json report"));
    obj.insert(
        "description".into(),
        json!(format!(
            "The document `hauksbee run <board> --json` (and the per-report --json \
             surfaces) emits. schema_version {RUN_REPORT_SCHEMA_VERSION}. GENERATED from \
             `JsonReport` in crates/hauksbee-engine/src/result.rs plus the \
             ok/verdict/serious_count/actionable_count rollup `to_json` prepends. Do not \
             hand-edit; regenerate with: UPDATE_RUN_SCHEMA=1 cargo test -p hauksbee-engine \
             --test cli_strict_plain"
        )),
    );
    let props = obj
        .get_mut("properties")
        .and_then(|p| p.as_object_mut())
        .expect("root schema has properties");
    props.insert("ok".into(), json!({ "type": "boolean" }));
    props.insert(
        "verdict".into(),
        json!({ "type": "string", "enum": ["pass", "fail", "invalid"] }),
    );
    props.insert("serious_count".into(), json!({ "type": "integer", "minimum": 0 }));
    props.insert("actionable_count".into(), json!({ "type": "integer", "minimum": 0 }));
    for key in ["ok", "verdict", "serious_count", "actionable_count"] {
        let required = obj
            .get_mut("required")
            .and_then(|r| r.as_array_mut())
            .expect("root schema has required");
        if !required.iter().any(|v| v == key) {
            required.push(json!(key));
        }
    }
    let mut expected = serde_json::to_string_pretty(obj).expect("schema serializes");
    expected.push('\n');

    let path = board("schemas/hauksbee-run-report.schema.json");
    if std::env::var_os("UPDATE_RUN_SCHEMA").is_some() {
        std::fs::write(&path, &expected).unwrap();
        return;
    }
    let on_disk = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing {}: {e}", path.display()));
    assert_eq!(on_disk, expected, "the schema file drifted from JsonReport");

    // A minimal report carries the version constant and the rollup keys.
    let report = JsonReport::new(
        "board",
        hauksbee_engine::result::BindSummary::from_report(&Default::default()),
    );
    let v: serde_json::Value = serde_json::from_str(&report.to_json()).expect("one JSON document");
    assert_eq!(v["schema_version"], RUN_REPORT_SCHEMA_VERSION);
    for key in ["ok", "verdict", "serious_count", "actionable_count", "board", "bind"] {
        assert!(v.get(key).is_some(), "missing rollup/header key {key}");
    }
}

// ── the boot-safety advisory over real AVR firmware ─────────────────────────

/// A board whose firmware drives a transistor-gate net HIGH from reset is named
/// in `--headless --json` as a `boot_control_net` note: advisory-only by default
/// (exit 0, verdict pass), escalated to exit 2 / verdict fail under
/// `--strict-boot`. The boot-state panel names the gate and its drive state;
/// the variant that never drives it reports it as floating.
#[cfg(feature = "avr")]
#[test]
fn boot_advisory_is_reported_and_strict_boot_gates_it() {
    let b = boot_gate_board();
    let fw_a = repo("testdata/firmware/boot_gate_a/boot_gate.hex");
    let fw_b = repo("testdata/firmware/boot_gate_b/boot_gate.hex");
    assert!(fw_a.exists() && fw_b.exists(), "tracked firmware fixtures");
    let (b, fw_a, fw_b) = (b.to_str().unwrap(), fw_a.to_str().unwrap(), fw_b.to_str().unwrap());
    let base = |fw: &str, extra: &[&str]| {
        let mut args = vec!["run", b, "--firmware", fw, "--headless", "--seconds", "0.05"];
        args.extend_from_slice(extra);
        run(&args)
    };

    let out = base(fw_a, &["--json"]);
    assert_eq!(out.status.code(), Some(0), "advisory-only: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("\"boot_control_net\""), "expected a boot_control_net note:\n{text}");
    assert!(text.contains("\"boot_gates\"") && text.contains("\"driven_high\""), "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).expect("one JSON document");
    assert_eq!(v["verdict"], "pass");

    let strict = base(fw_a, &["--strict-boot", "--json"]);
    assert_eq!(strict.status.code(), Some(2), "--strict-boot escalates to exit 2");
    let v: serde_json::Value = serde_json::from_str(&stdout(&strict)).expect("one JSON document");
    assert_eq!(v["verdict"], "fail", "the document agrees with the exit");

    let plain = base(fw_a, &["--plain"]);
    let p = stdout(&plain);
    assert!(p.contains("GATE_CTRL") && p.contains("driven HIGH"), "panel:\n{p}");
    let text = stdout(&base(fw_a, &[]));
    assert!(text.contains("BOOT HAZARD") && text.contains("GATE_CTRL"), "{text}");

    let floating = base(fw_b, &["--plain"]);
    let f = stdout(&floating);
    assert!(f.contains("GATE_CTRL") && f.contains("floating"), "undriven gate:\n{f}");
}

/// A clean board whose firmware only toggles a signal raises NO boot advisory.
#[cfg(feature = "avr")]
#[test]
fn clean_firmware_raises_no_boot_advisory() {
    let b = blinky_board();
    let fw = repo("testdata/firmware/demo/demo.hex");
    assert!(fw.exists(), "tracked firmware fixture: {}", fw.display());
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--firmware",
        fw.to_str().unwrap(),
        "--headless",
        "--seconds",
        "0.1",
        "--json",
        "--strict-boot",
    ]);
    assert!(!stdout(&out).contains("\"boot_control_net\""), "{}", stdout(&out));
    assert!(out.status.success(), "--strict-boot exits 0 with no advisory");
}

/// A gate driven HIGH that ALSO has a pulldown must still report "driven HIGH"
/// (the panel once reused the safety-filtered set and inverted the label).
#[cfg(feature = "avr")]
#[test]
fn boot_panel_reports_high_even_with_a_gate_pulldown() {
    let b = board("tests/fixtures/boot_gate_pulldown.kicad_pcb");
    let fw = repo("testdata/firmware/boot_gate_a/boot_gate.hex");
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--firmware",
        fw.to_str().unwrap(),
        "--headless",
        "--seconds",
        "0.05",
        "--plain",
    ]);
    let p = stdout(&out);
    let gate_line = p
        .lines()
        .find(|l| {
            l.contains("GATE_CTRL")
                && (l.contains("driven") || l.contains("pulled") || l.contains("floating"))
        })
        .unwrap_or("");
    assert!(gate_line.contains("driven HIGH"), "got line: {gate_line:?}\nfull:\n{p}");
    assert!(!gate_line.contains("LOW") && !gate_line.contains("floating"));
}
