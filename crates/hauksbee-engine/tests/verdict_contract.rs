//! The verdict contract: vacuous passes must die.
//!
//! Two coupled promises, both two-sided here:
//!
//! 1. **Strict thermal is the default.** A PARTIAL-coverage `--thermal` result
//!    (real rows while an active power IC on the live circuit is
//!    open/unresolved) escalates to exit 3 by default; `--no-strict-thermal`
//!    restores exit 0 while KEEPING the INCONCLUSIVE coverage caveat, and
//!    `--strict-thermal` stays accepted as a quiet no-op so existing CI
//!    invocations do not break. A fully-covered board is unchanged either way.
//!
//! 2. **The INCONCLUSIVE verdict vocabulary.** `--lint`/`--si`/`--check` must
//!    never print "Looks healthy" (or an equivalent clean bill) when
//!    current-carrying / active parts are unbound: the verdict says
//!    INCONCLUSIVE with the count, the named parts, and the unlocking input.
//!    The same board with the part bound gets the normal verdict. The prose
//!    never changes the exit code on its own (docs/ci/CI.md states the
//!    boundary).

use std::path::PathBuf;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// The example board with dissipating parts and NO active ICs at all:
/// thermal coverage is vacuously complete, so its exit codes must be
/// unchanged by the strict-thermal default flip.
fn fully_covered_board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-ci/examples/boards/power_resistor.kicad_pcb")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("hauksbee binary runs")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

// ── 1. Strict thermal is the default ────────────────────────────────────────

#[test]
fn thermal_partial_coverage_escalates_by_default() {
    let b = fixture("thermal_partial_coverage.kicad_pcb");
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--thermal",
        "--seconds",
        "0.05",
    ]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "partial thermal coverage must exit 3 by DEFAULT; stderr: {}",
        stderr(&out)
    );
    let err = stderr(&out);
    assert!(
        err.contains("INCONCLUSIVE") && err.contains("U3"),
        "the caveat names the open active IC in the shared vocabulary:\n{err}"
    );
    assert!(
        err.contains("thermal coverage is PARTIAL"),
        "the caveat states the honest coverage fact:\n{err}"
    );
}

#[test]
fn no_strict_thermal_opts_out_of_the_exit_but_never_of_the_caveat() {
    let b = fixture("thermal_partial_coverage.kicad_pcb");
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--thermal",
        "--seconds",
        "0.05",
        "--no-strict-thermal",
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "--no-strict-thermal restores exit 0; stderr: {}",
        stderr(&out)
    );
    let err = stderr(&out);
    assert!(
        err.contains("INCONCLUSIVE") && err.contains("U3"),
        "the opt-out changes ONLY the exit code; the caveat still prints:\n{err}"
    );
    // The real rows are still shown (the table is real, just incomplete).
    assert!(
        stdout(&out).contains("R1"),
        "the solved dissipating row survives the opt-out:\n{}",
        stdout(&out)
    );
}

#[test]
fn strict_thermal_flag_is_still_accepted_quietly() {
    // Existing CI invocations pass --strict-thermal; it now names the default,
    // so it must keep working (same exit 3) without a usage error.
    let b = fixture("thermal_partial_coverage.kicad_pcb");
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--thermal",
        "--seconds",
        "0.05",
        "--strict-thermal",
    ]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "--strict-thermal is a quiet no-op documenting the default; stderr: {}",
        stderr(&out)
    );
    assert!(
        !stderr(&out).contains("unexpected argument"),
        "no clap error for the compatibility flag:\n{}",
        stderr(&out)
    );
}

#[test]
fn strict_and_no_strict_thermal_together_is_a_usage_error() {
    let b = fixture("thermal_partial_coverage.kicad_pcb");
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--thermal",
        "--strict-thermal",
        "--no-strict-thermal",
    ]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "contradictory strictness flags are a usage error, not a silent pick; stderr: {}",
        stderr(&out)
    );
}

#[test]
fn thermal_partial_json_is_refused_by_default_and_valid_under_opt_out() {
    let b = fixture("thermal_partial_coverage.kicad_pcb");
    // Default: the JSON document carries the structured refusal and exits 3.
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--thermal",
        "--seconds",
        "0.05",
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(3));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    // Validity is #[serde(flatten)]ed into the thermal object.
    assert_eq!(v["thermal"]["valid"], false);
    assert_eq!(v["thermal"]["coverage"]["partial"], true);
    // Opt-out: valid data, coverage still says partial, note still present.
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--thermal",
        "--seconds",
        "0.05",
        "--json",
        "--no-strict-thermal",
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    assert_eq!(v["thermal"]["valid"], true);
    assert_eq!(v["thermal"]["coverage"]["partial"], true);
    let notes = v["notes"].as_array().expect("coverage note rides notes");
    assert!(
        notes
            .iter()
            .any(|n| n["kind"] == "coverage"
                && n["message"].as_str().unwrap_or("").contains("PARTIAL")),
        "the JSON consumer sees the coverage caveat even under the opt-out:\n{notes:?}"
    );
}

#[test]
fn fully_covered_board_is_unchanged_on_both_sides_of_the_flag() {
    let b = fully_covered_board();
    for extra in [None, Some("--no-strict-thermal")] {
        let mut args = vec!["run", b.to_str().unwrap(), "--thermal", "--seconds", "0.05"];
        if let Some(f) = extra {
            args.push(f);
        }
        let out = run(&args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "a fully-covered board stays exit 0 ({extra:?}); stderr: {}",
            stderr(&out)
        );
        assert!(
            !stderr(&out).contains("INCONCLUSIVE"),
            "no coverage caveat on a fully-covered board ({extra:?}):\n{}",
            stderr(&out)
        );
    }
}

// ── 2. The INCONCLUSIVE verdict vocabulary ──────────────────────────────────

#[test]
fn lint_over_an_unbound_power_fet_says_inconclusive_naming_it() {
    let b = fixture("verdict_fet_unbound.kicad_pcb");
    let out = run(&["run", b.to_str().unwrap(), "--lint", "--plain"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "the INCONCLUSIVE prose does not change the exit code; stderr: {}",
        stderr(&out)
    );
    let text = stdout(&out);
    assert!(
        text.contains("INCONCLUSIVE") && text.contains("Q1"),
        "the verdict names the unbound current-carrying part:\n{text}"
    );
    assert!(
        text.contains("--models-dir") || text.contains("hauksbee models new"),
        "the verdict states the unlocking input:\n{text}"
    );
    assert!(
        !text.contains("Looks healthy"),
        "an unbound power FET forbids the clean bill:\n{text}"
    );
    // The expert text surface carries the same sentence.
    let out = run(&["run", b.to_str().unwrap(), "--lint"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        stdout(&out).contains("INCONCLUSIVE") && stdout(&out).contains("Q1"),
        "the default text summary is not a vacuous pass either:\n{}",
        stdout(&out)
    );
}

#[test]
fn lint_with_the_fet_bound_gives_the_normal_verdict() {
    let b = fixture("verdict_fet_bound.kicad_pcb");
    let out = run(&["run", b.to_str().unwrap(), "--lint", "--plain"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(
        !text.contains("INCONCLUSIVE"),
        "a bound FET unlocks the conclusive verdict:\n{text}"
    );
    assert!(
        text.contains("Looks healthy") || text.contains("issue"),
        "the normal verdict vocabulary returns:\n{text}"
    );
}

#[test]
fn si_json_carries_the_inconclusive_note_and_exit_zero() {
    let b = fixture("verdict_fet_unbound.kicad_pcb");
    let out = run(&["run", b.to_str().unwrap(), "--si", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "INCONCLUSIVE is prose + notes, never an exit-code change for --si; stderr: {}",
        stderr(&out)
    );
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    let notes = v["notes"].as_array().expect("notes array present");
    assert!(
        notes.iter().any(|n| {
            n["kind"] == "coverage"
                && n["message"]
                    .as_str()
                    .is_some_and(|m| m.starts_with("INCONCLUSIVE") && m.contains("Q1"))
        }),
        "the machine surface carries the same INCONCLUSIVE sentence:\n{notes:?}"
    );
    // Bound side: no such note.
    let b = fixture("verdict_fet_bound.kicad_pcb");
    let out = run(&["run", b.to_str().unwrap(), "--si", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    let empty = Vec::new();
    let notes = v["notes"].as_array().unwrap_or(&empty);
    assert!(
        !notes
            .iter()
            .any(|n| n["message"].as_str().is_some_and(|m| m.starts_with("INCONCLUSIVE"))),
        "a bound FET leaves no INCONCLUSIVE note:\n{notes:?}"
    );
}

#[test]
fn check_closing_verdict_is_inconclusive_not_clean() {
    let b = fixture("verdict_fet_unbound.kicad_pcb");
    let out = run(&["run", b.to_str().unwrap(), "--check"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("VERDICT: inconclusive") && text.contains("Q1"),
        "the closing line refuses the clean claim and names the part:\n{text}"
    );
    assert!(
        !text.contains("VERDICT: clean"),
        "no clean bill over an unmodelled current-carrying part:\n{text}"
    );
    // Bound side: the clean verdict returns.
    let b = fixture("verdict_fet_bound.kicad_pcb");
    let out = run(&["run", b.to_str().unwrap(), "--check"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("VERDICT: clean"),
        "binding the FET restores the clean verdict:\n{}",
        stdout(&out)
    );
}
