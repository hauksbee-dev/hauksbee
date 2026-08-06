//! Round-3 audit fixes, engine CLI half: contracts that only the compiled
//! binary can prove (error-handler routing, exit codes, output envelopes).

use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
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

fn blinky_board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-ci/examples/boards/blinky.kicad_pcb")
}

fn board(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn clean_board() -> PathBuf {
    board("../../testdata/boards/button_pullup.kicad_pcb")
}

// ── B1: --example resolution goes through the normal error handler ──────────

#[test]
fn run_unknown_example_uses_the_lowercase_error_handler() {
    let out = run(&["run", "--example", "bogus", "--check"]);
    let err = stderr(&out);
    assert!(
        err.contains("error: no embedded example board named 'bogus'"),
        "the normal lowercase handler formats it: {err}"
    );
    assert!(
        !err.contains("Error:") && !err.to_lowercase().contains("backtrace"),
        "anyhow's default Error/backtrace rendering must not leak: {err}"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "input errors exit 1, like every other run input error"
    );
}

#[test]
fn run_unknown_example_under_json_emits_the_json_envelope() {
    let out = run(&["run", "--example", "bogus", "--json"]);
    let so = stdout(&out);
    let v: serde_json::Value = serde_json::from_str(so.trim())
        .unwrap_or_else(|e| panic!("--json must stay JSON on this error path: {e}\n{so}"));
    assert_eq!(v["ok"], serde_json::Value::Bool(false));
    assert!(
        v["error"]
            .as_str()
            .unwrap_or_default()
            .contains("no embedded example board named 'bogus'"),
        "the error field carries the message: {so}"
    );
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn sim_unknown_example_uses_the_lowercase_error_handler() {
    let out = run(&["sim", "--example", "bogus"]);
    let err = stderr(&out);
    assert!(
        err.contains("error: no embedded example deck named 'bogus'"),
        "the normal handler formats it: {err}"
    );
    assert!(
        !err.contains("Error:") && !err.to_lowercase().contains("backtrace"),
        "anyhow's default rendering must not leak: {err}"
    );
    assert_eq!(out.status.code(), Some(1));
}

// ── B2 (engine half): a board file handed to `models lint` ──────────────────

#[test]
fn models_lint_on_a_board_names_the_actual_fix_without_dumping_the_board() {
    let b = blinky_board();
    let out = run(&["models", "lint", b.to_str().unwrap()]);
    let err = stderr(&out);
    assert!(
        err.contains("is a board, not a model spec"),
        "names what happened: {err}"
    );
    assert!(
        err.contains("hauksbee models resolve"),
        "names the command they meant: {err}"
    );
    assert!(
        !err.contains("(kicad_pcb"),
        "the board content must not be dumped as error context: {err}"
    );
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn models_lint_toml_error_context_is_width_capped() {
    // A non-board TOML file whose failing line is enormous: the parser's
    // caret snippet must be width-capped, not dumped whole.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("giant.toml");
    let giant = format!("[models]]\n# {}\n", "x".repeat(5000));
    std::fs::write(&p, giant).unwrap();
    let out = run(&["models", "lint", p.to_str().unwrap()]);
    assert_ne!(out.status.code(), Some(0));
    let err = stderr(&out);
    let longest = err.lines().map(|l| l.chars().count()).max().unwrap_or(0);
    assert!(
        longest <= 400,
        "error context lines must be width-capped, longest was {longest}:\n{err}"
    );
}

// ── H4/M3/M7: numeric flag bounds and negative numbers ───────────────────────

#[test]
fn ambient_out_of_range_is_a_usage_error_naming_the_bound() {
    let b = blinky_board();
    for bad in ["2000", "-300", "1e308", "nan"] {
        let out = run(&["run", b.to_str().unwrap(), "--thermal", "--ambient", bad]);
        assert_eq!(out.status.code(), Some(2), "usage error for {bad}");
        let err = stderr(&out);
        assert!(
            err.contains("[-273.15, 1000]") || err.contains("not a number"),
            "{bad}: the bound is named: {err}"
        );
        // The 309-digit float rendering must never appear.
        assert!(
            err.lines().all(|l| l.len() < 300),
            "{bad}: no exploded numeric rendering: {err}"
        );
    }
}

#[test]
fn negative_ambient_parses_instead_of_reading_as_a_flag() {
    let b = blinky_board();
    // -40 C is a perfectly good ambient; clap used to reject it as an
    // unexpected argument. --drc keeps the run cheap.
    let out = run(&["run", b.to_str().unwrap(), "--drc", "--ambient", "-40"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
}

#[test]
fn zero_or_huge_seconds_is_a_usage_error_naming_the_bound() {
    let b = blinky_board();
    for bad in ["0", "-1", "1e307"] {
        let out = run(&["run", b.to_str().unwrap(), "--headless", "--seconds", bad]);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{bad} must be refused before any run"
        );
        assert!(
            stderr(&out).contains("(0, 1e6]"),
            "{bad}: the bound is named: {}",
            stderr(&out)
        );
    }
}

// ── H2: KiCad project files point at the sibling board ──────────────────────

#[test]
fn kicad_prl_input_points_at_the_sibling_board() {
    let prl = board("../../testdata/boards/button_pullup.kicad_prl");
    let out = run(&["run", prl.to_str().unwrap(), "--check"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(
        err.contains("KiCad project file, not the board"),
        "names what the file is: {err}"
    );
    assert!(
        err.contains("button_pullup.kicad_pcb"),
        "suggests the sibling layout by stem: {err}"
    );
}

// ── H3: a directory holding a board file ─────────────────────────────────────

#[test]
fn directory_with_one_board_file_is_used_with_a_note() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(clean_board(), dir.path().join("only.kicad_pcb")).unwrap();
    let out = run(&["run", dir.path().to_str().unwrap(), "--drc"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("using the board file inside it"),
        "the substitution is disclosed: {}",
        stderr(&out)
    );
}

#[test]
fn directory_with_several_board_files_asks_which() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(clean_board(), dir.path().join("a.kicad_pcb")).unwrap();
    std::fs::copy(clean_board(), dir.path().join("b.kicad_pcb")).unwrap();
    let out = run(&["run", dir.path().to_str().unwrap(), "--drc"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(
        err.contains("holding 2 board files") && err.contains("a.kicad_pcb"),
        "lists the candidates: {err}"
    );
}

// ── U13: corrupt file with a recognized extension ────────────────────────────

#[test]
fn corrupt_kicad_pcb_says_content_not_format() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("broken.kicad_pcb");
    std::fs::write(&p, "this is not an s-expression at all").unwrap();
    let out = run(&["run", p.to_str().unwrap(), "--check"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(
        err.contains("looks like a KiCad board by extension") && err.contains("did not parse"),
        "names the content problem instead of the generic format list: {err}"
    );
}

// ── M1: conflict policy warnings ─────────────────────────────────────────────

#[test]
fn list_nets_with_a_report_flag_warns_which_flag_is_ignored() {
    let b = blinky_board();
    let out = run(&["run", b.to_str().unwrap(), "--list-nets", "--drc"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        stderr(&out).contains("the report flag is ignored"),
        "no silent drop: {}",
        stderr(&out)
    );
}

#[test]
fn tui_with_a_report_flag_warns() {
    let b = blinky_board();
    let out = run(&["run", b.to_str().unwrap(), "--tui", "--drc"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        stderr(&out).contains("--tui is ignored"),
        "no silent drop: {}",
        stderr(&out)
    );
}

// ── M2: --thermal --plain refuses instead of silently ignoring ───────────────

#[test]
fn thermal_plain_refuses_loudly() {
    let b = blinky_board();
    let out = run(&["run", b.to_str().unwrap(), "--thermal", "--plain"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("--thermal has no --plain form"),
        "{}",
        stderr(&out)
    );
}

// ── M6: zero-component boards refuse to pass ────────────────────────────────

const EMPTY_BOARD_DSL: &str = "# Board-as-Code (hauksbee board DSL v1)\n\
                               board version 20241229\n\nfn main {\n}\n";

#[test]
fn run_on_an_empty_board_is_invalid_for_analysis() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("empty.board");
    std::fs::write(&p, EMPTY_BOARD_DSL).unwrap();
    let out = run(&["run", p.to_str().unwrap(), "--check"]);
    assert_eq!(out.status.code(), Some(3), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("this board has no components"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn check_code_on_an_empty_board_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("empty.board");
    std::fs::write(&p, EMPTY_BOARD_DSL).unwrap();
    let out = run(&["check-code", p.to_str().unwrap(), "--seconds", "0.01"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(
        stderr(&out).contains("this board has no components"),
        "{}",
        stderr(&out)
    );
}

// ── M8: --probe validation ───────────────────────────────────────────────────

#[cfg(feature = "avr")]
#[test]
fn empty_probe_is_rejected() {
    let b = blinky_board();
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--headless",
        "--seconds",
        "0.01",
        "--probe",
        "",
        "--probe-csv",
        "/dev/null",
    ]);
    assert_ne!(out.status.code(), Some(0));
    assert!(
        stderr(&out).contains("only empty net name"),
        "{}",
        stderr(&out)
    );
}

#[cfg(feature = "avr")]
#[test]
fn unknown_probe_net_points_at_list_nets() {
    let b = blinky_board();
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--headless",
        "--seconds",
        "0.01",
        "--probe",
        "TOTALLY_BOGUS_NET",
        "--probe-csv",
        "/dev/null",
    ]);
    assert_ne!(out.status.code(), Some(0));
    let err = stderr(&out);
    assert!(
        err.contains("not found on the board") && err.contains("--list-nets"),
        "near-match error plus the discovery pointer: {err}"
    );
}

// ── U3: bare --plain means the prose full report ─────────────────────────────

#[test]
fn bare_plain_implies_check_instead_of_the_dead_end_hint() {
    let b = blinky_board();
    let out = run(&["run", b.to_str().unwrap(), "--plain"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let so = stdout(&out);
    assert!(
        so.contains("VERDICT:"),
        "prose full report, not a hint: {so}"
    );
    assert!(
        !stderr(&out).contains("no interactive dashboard"),
        "the dead-loop hint must not print: {}",
        stderr(&out)
    );
}

// ── U4: --check ends on one verdict line ─────────────────────────────────────

#[test]
fn check_ends_with_a_single_verdict_line() {
    let b = clean_board();
    let out = run(&["run", b.to_str().unwrap(), "--check"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let so = stdout(&out);
    let last = so
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    assert!(
        last.starts_with("VERDICT: "),
        "the last line is the verdict: {last:?}"
    );
    assert!(
        last.contains("serious") && last.contains("worth a look"),
        "verdict carries both counts: {last}"
    );
}

// ── U5: the unresolved-parts heads-up names the scaffolding command ──────────

#[test]
fn coverage_heads_up_names_models_new_with_the_first_unresolved_ref() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("mystery.board");
    std::fs::write(
        &p,
        "# Board-as-Code (hauksbee board DSL v1)\n\
         board version 20241229\n\nfn main {\n\
         \tnet \"A\"\n\tnet \"B\"\n\
         \tcomp U1 lib \"Package_SO:SOIC-8\" val \"TOTALLYUNKNOWN999\" layer \"F.Cu\" at 0 0 rot 0 {\n\
         \t\tpad \"1\" smd rect at 0 0 size 1 1 layers [F.Cu] net \"A\"\n\
         \t\tpad \"2\" smd rect at 1 0 size 1 1 layers [F.Cu] net \"B\"\n\
         \t}\n}\n",
    )
    .unwrap();
    let out = run(&[
        "run",
        p.to_str().unwrap(),
        "--headless",
        "--seconds",
        "0.01",
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let so = stdout(&out);
    assert!(
        so.contains("hauksbee models new --board") && so.contains(" U1"),
        "the heads-up names the exact scaffolding command: {so}"
    );
}

// ── U10: doctor piped output stays two clean blocks ──────────────────────────

#[test]
fn doctor_piped_output_does_not_interleave_formats() {
    let out = run(&["doctor", "--backends"]);
    assert_eq!(out.status.code(), Some(0));
    // stdout: pure TSV, three fields per line, no prose.
    for line in stdout(&out).lines() {
        assert!(
            line.split('\t').count() >= 3,
            "stdout must stay machine TSV: {line:?}"
        );
    }
}

// ── D2: zero routed copper must not read as a clean spacing check ────────────

#[test]
fn pads_only_board_gets_the_unrouted_copper_note() {
    // A .board with components but no routes compiles to a segment-free
    // layout: the canbus-stepper shape (pads only, nothing routed).
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("unrouted.board");
    std::fs::write(
        &p,
        "# Board-as-Code (hauksbee board DSL v1)\n\
         board version 20241229\n\nfn main {\n\
         \tnet \"A\"\n\tnet \"B\"\n\
         \tcomp R1 lib \"Resistor_SMD:R_0402_1005Metric\" val \"10k\" layer \"F.Cu\" at 0 0 rot 0 {\n\
         \t\tpad \"1\" smd rect at 0 0 size 1 1 layers [F.Cu] net \"A\"\n\
         \t\tpad \"2\" smd rect at 1 0 size 1 1 layers [F.Cu] net \"B\"\n\
         \t}\n}\n",
    )
    .unwrap();
    for flag in ["--drc", "--check"] {
        let out = run(&["run", p.to_str().unwrap(), flag]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{flag} stderr: {}",
            stderr(&out)
        );
        assert!(
            stdout(&out).contains("no routed copper"),
            "{flag} must carry the pads-only caveat: {}",
            stdout(&out)
        );
    }
    // And the machine surface carries it as a note.
    let out = run(&["run", p.to_str().unwrap(), "--drc", "--json"]);
    assert!(
        stdout(&out).contains("no routed copper"),
        "--json note: {}",
        stdout(&out)
    );
}
