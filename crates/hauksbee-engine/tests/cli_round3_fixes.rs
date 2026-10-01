//! Round-3 audit fixes, engine CLI half: contracts that only the compiled
//! binary can prove (error-handler routing, exit codes, output envelopes).

use crate::support::{board, expect_err, run, run_board, stderr, stdout};
use std::path::PathBuf;

fn blinky_board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-ci/examples/boards/blinky.kicad_pcb")
}

fn clean_board() -> PathBuf {
    board("../../testdata/boards/button_pullup.kicad_pcb")
}

// ── B1: --example resolution goes through the normal error handler ──────────

#[test]
fn unknown_example_uses_the_lowercase_error_handler() {
    let rows: [(&[&str], &str); 2] = [
        (&["run", "--example", "bogus", "--check"], "board"),
        (&["sim", "--example", "bogus"], "deck"),
    ];
    for (args, what) in rows {
        let out = run(args);
        let err = stderr(&out);
        assert!(
            err.contains(&format!("error: no embedded example {what} named 'bogus'")),
            "the normal lowercase handler formats it: {err}"
        );
        assert!(
            !err.contains("Error:") && !err.to_lowercase().contains("backtrace"),
            "anyhow's default rendering must not leak: {err}"
        );
        expect_err(&out, 1, &[]);
    }
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
    expect_err(&out, 1, &[]);
}

#[test]
fn bare_run_and_sim_name_the_bundled_example_as_the_next_action() {
    for (cmd, next) in [
        ("run", "run --example blinky"),
        ("sim", "sim --example rlc_ringdown"),
    ] {
        let out = run(&[cmd]);
        let err = stderr(&out);
        assert!(err.contains(next) && !err.contains("required arg"), "{err}");
        expect_err(&out, 1, &[]);
    }
    // Refused after the artifact transaction opens, so a stale JUnit fails closed.
    let dir = tempfile::tempdir().unwrap();
    let junit = dir.path().join("j.xml");
    run(&["run", "--junit", junit.to_str().unwrap()]);
    assert!(std::fs::read_to_string(&junit)
        .unwrap()
        .contains("errors=\"1\""));
    let json = stdout(&run(&["run", "--json"]));
    assert!(
        json.contains(r#""ok":false"#) && json.contains("blinky"),
        "{json}"
    );
}

// ── B2 (engine half): a board file handed to `models lint` ──────────────────

#[test]
fn models_lint_on_a_board_names_the_actual_fix_without_dumping_the_board() {
    let b = blinky_board();
    let out = run(&["models", "lint", b.to_str().unwrap()]);
    let err = stderr(&out);
    expect_err(
        &out,
        1,
        &["is a board, not a model spec", "hauksbee models resolve"],
    );
    assert!(
        !err.contains("(kicad_pcb"),
        "the board content must not be dumped as error context: {err}"
    );
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
    for bad in ["2000", "-300", "1e308", "nan"] {
        let out = run_board(&blinky_board(), &["--thermal", "--ambient", bad]);
        expect_err(&out, 2, &[]);
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
    // -40 C is a perfectly good ambient; clap used to reject it as an
    // unexpected argument. --drc keeps the run cheap.
    let out = run_board(&blinky_board(), &["--drc", "--ambient", "-40"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
}

#[test]
fn zero_or_huge_seconds_is_a_usage_error_naming_the_bound() {
    for bad in ["0", "-1", "1e307"] {
        let out = run_board(&blinky_board(), &["--headless", "--seconds", bad]);
        expect_err(&out, 2, &[]);
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
    let out = run_board(
        &board("../../testdata/boards/button_pullup.kicad_prl"),
        &["--check"],
    );
    expect_err(
        &out,
        1,
        &[
            "KiCad project file, not the board",
            "button_pullup.kicad_pcb",
        ],
    );
}

// ── H3: a directory holding a board file ─────────────────────────────────────

#[test]
fn directory_with_one_board_file_is_used_with_a_note() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(clean_board(), dir.path().join("only.kicad_pcb")).unwrap();
    let out = run_board(dir.path(), &["--drc"]);
    expect_err(&out, 0, &["using the board file inside it"]);
}

#[test]
fn directory_with_several_board_files_asks_which() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(clean_board(), dir.path().join("a.kicad_pcb")).unwrap();
    std::fs::copy(clean_board(), dir.path().join("b.kicad_pcb")).unwrap();
    let out = run_board(dir.path(), &["--drc"]);
    expect_err(&out, 1, &["holding 2 board files", "a.kicad_pcb"]);
}

// ── U13: corrupt file with a recognized extension ────────────────────────────

#[test]
fn corrupt_kicad_pcb_says_content_not_format() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("broken.kicad_pcb");
    std::fs::write(&p, "this is not an s-expression at all").unwrap();
    let out = run_board(&p, &["--check"]);
    expect_err(
        &out,
        1,
        &["looks like a KiCad board by extension", "did not parse"],
    );
}

// ── M1: conflict policy warnings ─────────────────────────────────────────────

#[test]
fn list_nets_with_a_report_flag_warns_which_flag_is_ignored() {
    let out = run_board(&blinky_board(), &["--list-nets", "--drc"]);
    expect_err(&out, 0, &["the report flag is ignored"]);
}

// ── M2: every analysis surface accepts --plain ────────────────────

#[test]
fn ampacity_and_thermal_accept_plain() {
    let ampacity = run_board(&blinky_board(), &["--ampacity", "--plain"]);
    assert_eq!(ampacity.status.code(), Some(0), "{}", stderr(&ampacity));
    assert!(stdout(&ampacity).contains("Plain-language ampacity"));

    // Thermal is a report-surface contract test, not an AVR integration test.
    // Keep it on a passive board so the permissive (`--no-default-features`)
    // build exercises the plain renderer without requiring the GPL AVR backend.
    let thermal = run_board(
        &clean_board(),
        &["--thermal", "--plain", "--seconds", "0.05"],
    );
    assert_ne!(
        thermal.status.code(),
        Some(1),
        "--plain must not be rejected as a flag error: {}",
        stderr(&thermal)
    );
    assert!(
        stdout(&thermal).contains("Plain-language thermal"),
        "{}",
        stdout(&thermal)
    );
}

#[test]
fn quiet_keeps_summary_but_suppresses_per_item_evidence_appendix() {
    let b = board("tests/fixtures/plain_check_open_active_ic.kicad_pcb");
    let normal = run_board(&b, &["--check", "--plain"]);
    let quiet = run(&["--quiet", "run", b.to_str().unwrap(), "--check", "--plain"]);
    assert!(
        stdout(&normal).contains("== Evidence =="),
        "{}",
        stdout(&normal)
    );
    let text = stdout(&quiet);
    assert!(
        text.contains("VERDICT:"),
        "verdict remains visible:\n{text}"
    );
    assert!(
        text.contains("active IC(s) are unresolved/open"),
        "open-part summary remains visible:\n{text}"
    );
    assert!(
        !text.contains("== Evidence =="),
        "appendix must be hidden:\n{text}"
    );
    assert!(
        text.contains("rerun without --quiet to show the compact evidence appendix"),
        "the recovery instruction remains:\n{text}"
    );
}

// ── M6: zero-component boards refuse to pass ────────────────────────────────

/// Write a Board-as-Code file whose `fn main` holds `body`; the tempdir owns it.
fn write_board(name: &str, body: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join(name);
    let dsl = format!(
        "# Board-as-Code (hauksbee board DSL v1)\nboard version 20241229\n\nfn main {{\n{body}}}\n"
    );
    std::fs::write(&p, dsl).unwrap();
    (dir, p)
}

#[test]
fn run_on_an_empty_board_is_invalid_for_analysis() {
    let (_dir, p) = write_board("empty.board", "");
    let out = run_board(&p, &["--check"]);
    expect_err(&out, 3, &["this board has no components"]);
}

#[test]
fn check_code_on_an_empty_board_refuses() {
    let (_dir, p) = write_board("empty.board", "");
    let out = run(&["check-code", p.to_str().unwrap(), "--seconds", "0.01"]);
    expect_err(&out, 1, &["this board has no components"]);
}

// ── M8: --probe validation ───────────────────────────────────────────────────

#[cfg(feature = "avr")]
#[test]
fn empty_probe_is_rejected() {
    let out = run_board(
        &blinky_board(),
        &[
            "--headless",
            "--seconds",
            "0.01",
            "--probe",
            "",
            "--probe-csv",
            "/dev/null",
        ],
    );
    expect_err(&out, 1, &["only empty net name"]);
}

#[cfg(feature = "avr")]
#[test]
fn unknown_probe_net_points_at_list_nets() {
    let out = run_board(
        &blinky_board(),
        &[
            "--headless",
            "--seconds",
            "0.01",
            "--probe",
            "TOTALLY_BOGUS_NET",
            "--probe-csv",
            "/dev/null",
        ],
    );
    expect_err(&out, 1, &["not found on the board", "--list-nets"]);
}

// ── U3: bare --plain means the prose full report ─────────────────────────────

#[test]
fn bare_plain_implies_check_instead_of_the_dead_end_hint() {
    let out = run_board(&blinky_board(), &["--plain"]);
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
    let out = run_board(&clean_board(), &["--check"]);
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

/// An invalid verdict says why and what to do next, with the SAME sentence in
/// the `--check` closing line and the `--json` document.
#[test]
fn invalid_verdict_states_one_reason_on_text_and_json() {
    let (_dir, p) = write_board(
        "mystery.board",
        "\tnet \"A\"\n\tnet \"B\"\n\
         \tcomp U1 lib \"Package_SO:SOIC-8\" val \"TOTALLYUNKNOWN999\" layer \"F.Cu\" at 0 0 rot 0 {\n\
         \t\tpad \"1\" smd rect at 0 0 size 1 1 layers [F.Cu] net \"A\"\n\
         \t\tpad \"2\" smd rect at 1 0 size 1 1 layers [F.Cu] net \"B\"\n\
         \t}\n",
    );
    let json: serde_json::Value =
        serde_json::from_str(&stdout(&run_board(&p, &["--check", "--json"]))).expect("json");
    assert_eq!(json["verdict"], "invalid");
    let reason = json["verdict_reason"].as_str().expect("invalid carries a reason");
    assert!(reason.contains("U1") && reason.contains("; next: "), "{reason}");
    let text = stdout(&run_board(&p, &["--check"]));
    let last = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
    assert!(last.starts_with("VERDICT: inconclusive") && last.ends_with(reason), "{last}");
}

// ── U5: the unresolved-parts heads-up names the scaffolding command ──────────

#[test]
fn coverage_heads_up_names_the_draft_pack_command_with_the_board_path() {
    let (_dir, p) = write_board(
        "mystery.board",
        "\tnet \"A\"\n\tnet \"B\"\n\
         \tcomp U1 lib \"Package_SO:SOIC-8\" val \"TOTALLYUNKNOWN999\" layer \"F.Cu\" at 0 0 rot 0 {\n\
         \t\tpad \"1\" smd rect at 0 0 size 1 1 layers [F.Cu] net \"A\"\n\
         \t\tpad \"2\" smd rect at 1 0 size 1 1 layers [F.Cu] net \"B\"\n\
         \t}\n",
    );
    let out = run_board(&p, &["--headless", "--seconds", "0.01"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let so = stdout(&out);
    // The heads-up moved from per-ref `models new` scaffolding to the
    // review-then-approve draft-pack flow; it must still name the exact
    // runnable command including this board's real path.
    assert!(
        so.contains("hauksbee models prepare") && so.contains("mystery.board"),
        "the heads-up names the exact draft-pack command: {so}"
    );
}

// ── U10: doctor piped output stays two clean blocks ──────────────────────────

#[test]
fn doctor_piped_output_does_not_interleave_formats() {
    let out = run(&["doctor", "--backends"]);
    expect_err(&out, 0, &[]);
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
    let (_dir, p) = write_board(
        "unrouted.board",
        "\tnet \"A\"\n\tnet \"B\"\n\
         \tcomp R1 lib \"Resistor_SMD:R_0402_1005Metric\" val \"10k\" layer \"F.Cu\" at 0 0 rot 0 {\n\
         \t\tpad \"1\" smd rect at 0 0 size 1 1 layers [F.Cu] net \"A\"\n\
         \t\tpad \"2\" smd rect at 1 0 size 1 1 layers [F.Cu] net \"B\"\n\
         \t}\n",
    );
    for flag in ["--drc", "--check"] {
        let out = run_board(&p, &[flag]);
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
    let out = run_board(&p, &["--drc", "--json"]);
    assert!(
        stdout(&out).contains("no routed copper"),
        "--json note: {}",
        stdout(&out)
    );
}

// ── `check` is `run --check`; a closed stdout ends quietly ───────────────────

#[test]
fn check_subcommand_matches_run_check() {
    let (clean, blinky) = (clean_board(), blinky_board());
    let boards = [clean.to_str().unwrap(), blinky.to_str().unwrap()];
    for extra in [&[][..], &["--plain"], &["--json"], &["--strict", "--plain"]] {
        for board in boards {
            let via_run = run(&[&["run", "--check", board], extra].concat());
            let via_check = run(&[&["check", board], extra].concat());
            assert_eq!(
                via_check.status.code(),
                via_run.status.code(),
                "{board} {extra:?}"
            );
            assert_eq!(stdout(&via_check), stdout(&via_run), "{board} {extra:?}");
            assert_eq!(stderr(&via_check), stderr(&via_run), "{board} {extra:?}");
        }
    }
    let out = run(&["--help"]);
    assert!(stdout(&out).contains("shorthand for `run --check`"));
    assert!(stdout(&out).contains("3  invalid for analysis"));
}

#[cfg(unix)]
#[test]
fn closed_stdout_ends_quietly_with_exit_141() {
    use std::process::{Command, Stdio};
    let mut child = Command::new(crate::support::bin())
        .args(["run", "--example", "blinky", "--check", "--plain"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    drop(child.stdout.take());
    let out = child.wait_with_output().expect("wait");
    assert!(!stderr(&out).contains("panicked"), "{}", stderr(&out));
    assert_eq!(out.status.code(), Some(141));
}
