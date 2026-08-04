//! CLI-level regression tests for the round-2 stress-test fixes: --asbuilt on
//! the static report paths, the --strict failure line, the resources/lint
//! output split, sim's empty-deck refusal, the subcommand-inference guards,
//! flag-consistency errors, --ac-node pre-validation, real net names, the
//! --junit/--sarif artifacts, and the to-code copper disclosure. All exercise
//! the compiled binary, because every one of these is an exit-code or
//! output-surface contract.

use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

fn board(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

/// A board with two real copper shorts (GND <-> +5V), so --strict gates.
fn shorted_board() -> PathBuf {
    board("../hauksbee-ci/examples/boards/boot_gate.kicad_pcb")
}

fn clean_board() -> PathBuf {
    board("../../testdata/boards/button_pullup.kicad_pcb")
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
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
    std::env::temp_dir().join(format!("hauksbee-r2-{}-{name}", std::process::id()))
}

// ── --asbuilt on every path (blocker 1) ─────────────────────────────────────

#[test]
fn asbuilt_bad_path_errors_on_every_report_branch() {
    let b = shorted_board();
    for flag in ["--report", "--drc", "--check"] {
        let out = run(&[
            "run",
            b.to_str().unwrap(),
            flag,
            "--asbuilt",
            "/nonexistent/overlay.asbuilt.toml",
        ]);
        assert!(
            !out.status.success(),
            "{flag} must hard-error on a missing overlay"
        );
        let err = stderr(&out);
        assert!(
            err.contains("as-built overlay") || err.contains("asbuilt"),
            "{flag}: the error must name the overlay: {err}"
        );
    }
}

#[test]
fn asbuilt_overlay_is_applied_and_narrated_on_report_paths() {
    // Discover two real net names so the jumper overlay describes this board.
    let b = shorted_board();
    let list = run(&["run", b.to_str().unwrap(), "--list-nets"]);
    assert!(list.status.success());
    let nets: Vec<String> = stdout(&list)
        .lines()
        .map(str::to_string)
        .filter(|n| !n.is_empty())
        .collect();
    assert!(nets.len() >= 2, "need two nets, got {nets:?}");
    let overlay = tmp("jumper.asbuilt.toml");
    std::fs::write(
        &overlay,
        format!("[[jumper]]\nfrom = \"{}\"\nto = \"{}\"\n", nets[0], nets[1]),
    )
    .unwrap();

    let plain = run(&["run", b.to_str().unwrap(), "--report"]);
    let with = run(&[
        "run",
        b.to_str().unwrap(),
        "--report",
        "--asbuilt",
        overlay.to_str().unwrap(),
    ]);
    assert!(with.status.success(), "overlaid report runs: {}", stderr(&with));
    let with_out = stdout(&with);
    assert!(
        with_out.contains("as-built overlay") && with_out.contains("applied"),
        "the report must narrate the applied overlay:\n{with_out}"
    );
    assert_ne!(
        stdout(&plain),
        with_out,
        "a report with an overlay must differ from one without"
    );
    // An overlay that does NOT describe the board (unknown ref) is a hard
    // error on the report path too, not just the simulating one.
    let bogus = tmp("bogus.asbuilt.toml");
    std::fs::write(&bogus, "[[replace]]\nref = \"NOSUCHPART99\"\n[replace.set]\nohms = 10.0\n")
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
    let _ = std::fs::remove_file(&overlay);
    let _ = std::fs::remove_file(&bogus);
}

// ── --strict failure line (blocker 2) ───────────────────────────────────────

#[test]
fn strict_gate_prints_the_failure_line() {
    let b = shorted_board();
    let out = run(&["run", b.to_str().unwrap(), "--drc", "--strict"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stdout(&out).contains("FAILED under --strict:"),
        "text mode must say why it exits 2:\n{}",
        stdout(&out)
    );
    assert!(stdout(&out).contains("gate-grade finding(s)"));

    // --plain must also acknowledge the gate on stdout.
    let plain = run(&["run", b.to_str().unwrap(), "--drc", "--strict", "--plain"]);
    assert_eq!(plain.status.code(), Some(2));
    assert!(stdout(&plain).contains("FAILED under --strict:"));

    // --json keeps stdout a single JSON document; the line goes to stderr.
    let json = run(&["run", b.to_str().unwrap(), "--drc", "--strict", "--json"]);
    assert_eq!(json.status.code(), Some(2));
    let doc = stdout(&json);
    serde_json::from_str::<serde_json::Value>(&doc)
        .expect("stdout must stay one valid JSON document under --strict");
    assert!(stderr(&json).contains("FAILED under --strict:"));
}

// ── --resources is not --lint (major 5) ─────────────────────────────────────

#[test]
fn resources_output_is_distinguishable_from_lint() {
    let b = clean_board();
    let lint = run(&["run", b.to_str().unwrap(), "--lint"]);
    let res = run(&["run", b.to_str().unwrap(), "--resources"]);
    assert!(lint.status.success() && res.status.success());
    assert_ne!(stdout(&lint), stdout(&res), "the two reports must differ");
    assert!(stdout(&res).contains("resource-conflicts:"));
    // A clean resources run says what WAS checked.
    assert!(
        stdout(&res).to_lowercase().contains("checked"),
        "clean --resources must name what it checked:\n{}",
        stdout(&res)
    );
}

/// `--plain` must not borrow `--lint`'s subject. Both reports share
/// `plain_netlint`, whose subject is the whole connectivity family, so a clean
/// `--resources --plain` printed "no connectivity problems found": a clean bill
/// of health for the I2C-pullup, floating-pin, LED-sanity and contention checks
/// that this command never ran.
#[test]
fn plain_resources_names_resource_conflicts_not_connectivity() {
    let b = clean_board();
    let res = run(&["run", b.to_str().unwrap(), "--resources", "--plain"]);
    assert!(res.status.success());
    let out = stdout(&res);
    assert!(
        out.contains("no MCU resource conflicts found"),
        "clean --resources --plain must name MCU resource conflicts:\n{out}"
    );
    assert!(
        !out.contains("connectivity"),
        "and must not claim anything about connectivity:\n{out}"
    );
    // --lint, which really does run the connectivity family, keeps its subject.
    let lint = run(&["run", b.to_str().unwrap(), "--lint", "--plain"]);
    assert!(lint.status.success());
    assert!(
        stdout(&lint).contains("no connectivity problems found"),
        "--lint --plain keeps the connectivity subject:\n{}",
        stdout(&lint)
    );
}

/// `--check --plain` must not let the USB-C section's absence read as a pass.
/// The section renders only when a USB-C receptacle was found, so on every other
/// board the suite has to say which of the two it was.
#[test]
fn plain_check_states_whether_usb_c_compliance_ran() {
    let b = clean_board();
    let out = run(&["run", b.to_str().unwrap(), "--check", "--plain"]);
    assert!(out.status.success());
    let s = stdout(&out);
    assert!(
        s.contains("USB-C CC compliance did not run"),
        "a board with no USB-C must say the check did not run:\n{s}"
    );
    // And the lint section names the checks that ride with it, so a reader knows
    // resource conflicts and strap pins were covered by that verdict.
    assert!(
        s.contains("MCU resource conflicts") && s.contains("boot strap pins"),
        "the lint section must name what rides with it:\n{s}"
    );
}

// ── sim empty-deck refusal (major 4) and probe validation (minor 17) ────────

#[test]
fn sim_refuses_element_free_decks() {
    let deck = tmp("empty.cir");
    std::fs::write(&deck, "* an empty deck\n.op\n.end\n").unwrap();
    let out = run(&["sim", deck.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("no circuit elements"),
        "{}",
        stderr(&out)
    );
    let _ = std::fs::remove_file(&deck);
}

#[test]
fn sim_invalid_probe_is_misuse_not_nonconvergence() {
    let deck = board("../../examples/learn/02-mna-by-hand/divider.cir");
    let out = run(&["sim", deck.to_str().unwrap(), "--op", "--print", "V(nope)"]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(err.contains("invalid probe"), "{err}");
    assert!(
        !err.contains("did not converge"),
        "a typo must not be blamed on the circuit: {err}"
    );
}

// ── subcommand-inference hazards (major 3) ──────────────────────────────────

#[test]
fn check_code_on_a_board_file_names_the_actual_fix() {
    let b = shorted_board();
    let out = run(&["check-code", b.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("hauksbee run") && stderr(&out).contains("--check"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn bare_board_path_suggests_run_check() {
    let b = shorted_board();
    let out = run(&[b.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("hauksbee run"));
}

#[test]
fn abbreviated_subcommands_are_not_inferred() {
    let out = run(&["se"]);
    assert!(!out.status.success(), "'se' must not start a server");
}

// ── flag consistency (minors 10 / 14) ───────────────────────────────────────

#[test]
fn artifact_flags_without_their_analysis_are_errors() {
    let b = clean_board();
    let cases: &[&[&str]] = &[
        &["--probe-csv", "/tmp/x.csv"],
        &["--ac-csv", "/tmp/x.csv"],
        &["--ac-node", "OUT"],
        &["--ampacity", "--json"],
        &["--ampacity", "--plain"],
    ];
    for extra in cases {
        let mut args = vec!["run", b.to_str().unwrap()];
        args.extend_from_slice(extra);
        let out = run(&args);
        assert!(
            !out.status.success(),
            "{extra:?} must be refused, not silently ignored"
        );
    }
}

// ── --ac-node validation before the sweep (minor 15) ────────────────────────

#[test]
fn ac_node_typo_fails_before_the_sweep_as_an_error() {
    let b = board("tests/fixtures/ac_loop_one_pole.kicad_pcb");
    let models = board("tests/fixtures/ac_loop_models");
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--models-dir",
        models.to_str().unwrap(),
        "--ac",
        "10:1e3:5",
        "--ac-node",
        "NO_SUCH_NET_XYZ",
    ]);
    assert_eq!(out.status.code(), Some(3), "invalid-for-analysis exit");
    let err = stderr(&out);
    assert!(
        err.contains("error") && err.contains("NO_SUCH_NET_XYZ"),
        "{err}"
    );
    assert!(
        !err.contains("WARNING"),
        "a typo'd --ac-node is an error, not a WARNING: {err}"
    );
}

// ── real net names everywhere (major 6) ─────────────────────────────────────

#[test]
fn escaped_net_names_are_displayed_as_real_names() {
    let p = tmp("slashnet.kicad_pcb");
    std::fs::write(
        &p,
        // One real component: a components-free board is now refused as
        // invalid for analysis (the M6 empty-board guard), and this test is
        // about net-NAME display, not about empty boards.
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
    assert!(text.contains("/GPIO0/XTAL1"), "{text}");
    assert!(text.contains("SCL_2"), "{text}");
    assert!(
        !text.contains("{slash}") && !text.contains("{2}"),
        "internal escapes must not leak: {text}"
    );
    // The JSON surface carries the real names too.
    let json = run(&["run", p.to_str().unwrap(), "--list-nets", "--json"]);
    let doc = stdout(&json);
    assert!(doc.contains("/GPIO0/XTAL1") && !doc.contains("{slash}"), "{doc}");
    let _ = std::fs::remove_file(&p);
}

// ── --junit / --sarif artifacts (item 25) ───────────────────────────────────

#[test]
fn junit_and_sarif_artifacts_are_written_and_valid() {
    let b = shorted_board();
    let junit = tmp("report.junit.xml");
    let sarif = tmp("report.sarif");
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--report",
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
    assert!(sj["runs"][0]["results"]
        .as_array()
        .is_some_and(|r| !r.is_empty()));
    let _ = std::fs::remove_file(&junit);
    let _ = std::fs::remove_file(&sarif);
}

// ── --tui without a terminal (minor 8) ──────────────────────────────────────

#[test]
fn tui_without_a_terminal_names_the_problem() {
    let b = clean_board();
    let out = run(&["run", b.to_str().unwrap(), "--tui"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("terminal"),
        "must explain the missing TTY, not an os error: {err}"
    );
    assert!(!err.contains("os error"), "{err}");
}

// ── to-code (minor 18 + copper disclosure) ──────────────────────────────────

#[test]
fn to_code_on_a_directory_is_a_sentence() {
    let dir = tmp("a-directory");
    std::fs::create_dir_all(&dir).unwrap();
    let out = run(&["to-code", dir.to_str().unwrap()]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("is a directory"), "{err}");
    assert!(!err.contains("os error"), "{err}");
    let _ = std::fs::remove_dir(&dir);
}

#[test]
fn to_code_discloses_that_copper_is_not_carried() {
    let b = shorted_board();
    let out = run(&["to-code", b.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("copper"),
        "the emitted code must disclose the copper drop in-file"
    );
    assert!(stderr(&out).contains("copper"), "and on stderr");
}

// ── run --version does not invent a binary name (help nit 20) ───────────────

#[test]
fn run_version_does_not_claim_a_hauksbee_run_binary() {
    let out = run(&["run", "--version"]);
    assert!(
        !stdout(&out).contains("hauksbee-run") && !stderr(&out).contains("hauksbee-run"),
        "no surface may claim a 'hauksbee-run' binary exists"
    );
}
