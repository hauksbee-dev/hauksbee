//! The verdict contract: vacuous passes must die.
//!
//! Three coupled promises, all two-sided here:
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
//!
//! 3. **Cross-surface parity.** Each surface's `--strict` exit code says the
//!    same thing as that surface's own JSON verdict, on the same board:
//!    `invalid` exits 3, `fail` exits 2, `pass` exits 0. Three exceptions are
//!    deliberate: the `--no-strict-thermal` opt-out, pinned below because its
//!    document must keep refusing even though its exit does not, and the two
//!    co-sim paths that keep exit 3 over a `fail` document because the run was
//!    not analysable even though it observed faults (an aborted analog solve, a
//!    runtime timing refusal), documented in docs/ci/CI.md, not pinned here.
//!
//!    The gate is per surface, so the bind gate that invalidates
//!    `--lint`/`--si`/`--check`/`--usb-c` leaves the copper (`--drc`) and
//!    descriptive (`--report`) surfaces alone, on both the exit code and the
//!    verdict field. JUnit/SARIF grade that same selected surface and a failing
//!    gate/refusal reaches GitHub annotations too. Requested artifact paths are
//!    invalidated before parsing and finalized once, so early errors and
//!    refusals cannot leave a previous run's green file archiveable.
//!
//!    They agree on the widened `fail` route too: the artifacts grade a
//!    testcase failure on the finding's own `gating` flag, not on the severity
//!    word, so a run that gates on a medium lint finding or on co-sim faults
//!    archives a failing testcase instead of `failures="0"`. Pinned below from
//!    both sides, including the direction that must stay green: a
//!    possibly-phantom copper short is `warning` and does NOT gate, so it stays
//!    a passing testcase carrying its text.
//!
//!    Non-finding terminal outcomes use a typed refusal testcase/result;
//!    `--strict-boot` is promoted to a typed co-sim finding when armed.

use std::path::{Path, PathBuf};
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

/// The example board that carries two real copper shorts (so the copper surface
/// has a `fail` verdict of its own to agree with) AND a control net its
/// firmware drives high from reset (so `--strict-boot` has an advisory to
/// escalate).
fn boot_gate_board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-ci/examples/boards/boot_gate.kicad_pcb")
}

/// A real board whose SI report carries a medium (`warning`) crystal-load-cap
/// finding and nothing serious, so `--si`'s own gate is the only thing that can
/// make it fail.
fn medium_si_board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../frontend/public/boards/stickhub.kicad_pcb")
}

fn run(args: &[&str]) -> Output {
    // Scrubbed, not inherited: under GitHub Actions the CLI adds workflow
    // annotations, so a suite that inherited the variable would exercise a
    // different annotation path in CI than on a laptop. The tests that WANT
    // annotations use `run_in_actions`.
    Command::new(bin())
        .args(args)
        .env_remove("GITHUB_ACTIONS")
        .output()
        .expect("hauksbee binary runs")
}

/// Run with `GITHUB_ACTIONS` set, the only condition under which the CLI emits
/// workflow-command annotations.
fn run_in_actions(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .env("GITHUB_ACTIONS", "true")
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
    let out = run(&["run", b.to_str().unwrap(), "--thermal", "--seconds", "0.05"]);
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
    // The expert text surface carries the same sentence, and it LEADS: the
    // extract body's "net-lint: no findings." must sit under the refusal, or
    // the first thing a reader sees is a clean bill.
    let out = run(&["run", b.to_str().unwrap(), "--lint"]);
    assert_eq!(out.status.code(), Some(0));
    let text = stdout(&out);
    let verdict_at = text.find("INCONCLUSIVE").unwrap_or_else(|| {
        panic!("the default text summary is not a vacuous pass either:\n{text}")
    });
    assert!(text.contains("Q1"), "{text}");
    let clean_at = text
        .find("net-lint: no findings.")
        .expect("the factual body still prints");
    assert!(
        verdict_at < clean_at,
        "the INCONCLUSIVE verdict must lead the 'no findings.' body:\n{text}"
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
        text.contains("Looks healthy: no connectivity problems found."),
        "the healthy dialect returns exactly (this fixture is lint-clean):\n{text}"
    );
}

#[test]
fn the_inconclusive_sentence_is_identical_across_lint_surfaces() {
    // One dialect: the sentence in the --plain verdict, the default text
    // summary, and the JSON coverage note must be byte-identical, or the
    // vocabulary has forked per surface.
    let b = fixture("verdict_fet_unbound.kicad_pcb");
    let extract = |text: &str| -> String {
        text.lines()
            .find(|l| l.starts_with("INCONCLUSIVE:"))
            .unwrap_or_else(|| panic!("an INCONCLUSIVE line exists in:\n{text}"))
            .to_string()
    };
    let plain = extract(&stdout(&run(&[
        "run",
        b.to_str().unwrap(),
        "--lint",
        "--plain",
    ])));
    let text = extract(&stdout(&run(&["run", b.to_str().unwrap(), "--lint"])));
    let json_out = stdout(&run(&["run", b.to_str().unwrap(), "--lint", "--json"]));
    let v: serde_json::Value = serde_json::from_str(&json_out).expect("one JSON document");
    let note = v["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .filter_map(|n| n["message"].as_str())
        .find(|m| m.starts_with("INCONCLUSIVE:"))
        .expect("an INCONCLUSIVE coverage note")
        .to_string();
    assert_eq!(plain, text, "plain and text sentences must not fork");
    assert_eq!(text, note, "text and JSON sentences must not fork");
}

#[test]
fn check_plain_sections_read_inconclusive_but_drc_stays_exempt() {
    let b = fixture("verdict_fet_unbound.kicad_pcb");
    let out = run(&["run", b.to_str().unwrap(), "--check", "--plain"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    // Both model-dependent sections carry the sentence.
    assert_eq!(
        text.matches("INCONCLUSIVE: 1 current-carrying / active part(s) have no model (Q1)")
            .count(),
        2,
        "the lint AND SI sections both refuse the clean bill:\n{text}"
    );
    // The copper check reads the layout and owes nothing to device models:
    // its verdict may still claim health on its own.
    assert!(
        text.contains("Looks healthy: no copper spacing (drc) problems found."),
        "the DRC section stays exempt from the model-coverage refusal:\n{text}"
    );
}

#[test]
fn thermal_json_note_leads_with_the_shared_inconclusive_tag() {
    let b = fixture("thermal_partial_coverage.kicad_pcb");
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
    assert!(
        v["notes"]
            .as_array()
            .expect("notes")
            .iter()
            .filter_map(|n| n["message"].as_str())
            .any(|m| m.starts_with("INCONCLUSIVE:") && m.contains("PARTIAL")),
        "the thermal JSON note speaks the same INCONCLUSIVE dialect as the text caveat:\n{v}"
    );
}

#[test]
fn si_json_carries_the_inconclusive_note_and_exit_zero() {
    let b = fixture("verdict_fet_unbound.kicad_pcb");
    let out = run(&["run", b.to_str().unwrap(), "--si", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "without --strict, INCONCLUSIVE is prose + notes and changes no exit code \
         for --si; stderr: {}",
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
        !notes.iter().any(|n| n["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("INCONCLUSIVE"))),
        "a bound FET leaves no INCONCLUSIVE note:\n{notes:?}"
    );
}

#[test]
fn web_front_door_refuses_a_clean_bill_over_an_unbound_fet() {
    // The browser surface must agree with the CLI: an unbound power FET (a
    // Q-prefix part, NOT an active IC) demotes the headline and the
    // model-dependent sections. Before this contract the web filtered on
    // `active_ic` alone, so a Q-only board still read "Looks healthy".
    let b = fixture("verdict_fet_unbound.kicad_pcb");
    let bytes = std::fs::read(&b).expect("fixture readable");
    let report = hauksbee_engine::frontdoor::analyze("verdict_fet_unbound.kicad_pcb", &bytes);
    assert!(
        report
            .bind
            .as_ref()
            .is_some_and(|b| b.active_path_unresolved.contains(&"Q1".to_string())),
        "the web bind summary names the unbound FET: {:?}",
        report.bind
    );
    assert!(
        !report.headline.contains("Looks healthy"),
        "the web headline must not bless an unbound protection FET: {}",
        report.headline
    );
    for section in &report.sections {
        if section.title.starts_with("Connectivity") || section.title.starts_with("Signal") {
            assert!(
                section.verdict.starts_with("INCONCLUSIVE") && section.verdict.contains("Q1"),
                "web section '{}' must read INCONCLUSIVE naming Q1: {}",
                section.title,
                section.verdict
            );
        }
    }
    // The bound twin restores the normal web verdicts.
    let b = fixture("verdict_fet_bound.kicad_pcb");
    let bytes = std::fs::read(&b).expect("fixture readable");
    let report = hauksbee_engine::frontdoor::analyze("verdict_fet_bound.kicad_pcb", &bytes);
    assert!(
        report
            .bind
            .as_ref()
            .is_none_or(|b| b.active_path_unresolved.is_empty()),
        "a bound FET leaves no open critical parts: {:?}",
        report.bind
    );
    assert!(
        !report
            .sections
            .iter()
            .any(|s| s.verdict.starts_with("INCONCLUSIVE")),
        "no INCONCLUSIVE section on the bound twin"
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

// ---------------------------------------------------------------------------
// The machine verdict's bind gate, both sides and both scopes. On the
// model-dependent-claim surfaces (--si here; --check/--lint share the flag) an
// unbound verdict-critical part flips `verdict` to "invalid"/`ok:false`, the
// machine mirror of the INCONCLUSIVE prose. On the copper-only (--drc) and
// descriptive (--report) surfaces the same board stays un-gated: DRC reads
// the layout and needs no device model, and the bind table is not a pass/fail
// claim, so poisoning their verdicts would refuse answers those surfaces can
// honestly give.
// ---------------------------------------------------------------------------

#[test]
fn bind_gate_flips_the_model_claim_surfaces_and_spares_the_copper_ones() {
    let unbound = fixture("verdict_fet_unbound.kicad_pcb");
    let out = run(&["run", unbound.to_str().unwrap(), "--si", "--json"]);
    assert_eq!(out.status.code(), Some(0), "exit stays 0 without --strict");
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    assert_eq!(
        v["verdict"], "invalid",
        "an unbound verdict-critical part invalidates the --si machine verdict:\n{v}"
    );
    assert_eq!(v["ok"], false);

    let out = run(&["run", unbound.to_str().unwrap(), "--drc", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    assert_ne!(
        v["verdict"], "invalid",
        "--drc is copper-only and must not be poisoned by the bind gate:\n{v}"
    );

    // Bound side: the same surface earns its pass.
    let bound = fixture("verdict_fet_bound.kicad_pcb");
    let out = run(&["run", bound.to_str().unwrap(), "--si", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    assert_ne!(v["verdict"], "invalid", "a bound board is not gated:\n{v}");
}

#[test]
fn junit_agrees_with_the_json_verdict_about_bind_blockers() {
    // Scenario 08's inverse: an invalid JSON verdict must show red in the
    // JUnit file too, not a green test-report tab beside a red dashboard.
    let unbound = fixture("verdict_fet_unbound.kicad_pcb");
    let dir = tempfile::tempdir().expect("tempdir");
    let junit = dir.path().join("out.xml");
    let out = run(&[
        "run",
        unbound.to_str().unwrap(),
        "--check",
        "--junit",
        junit.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let xml = std::fs::read_to_string(&junit).expect("junit written");
    assert!(
        xml.contains("INVALID evidence:"),
        "the bind blocker reaches the test report as a gate-grade entry:\n{xml}"
    );
    assert!(
        !junit_all_green(&xml),
        "the JUnit root must not read all-green beside an invalid JSON verdict: {}",
        junit_root(&xml)
    );
}

// ---------------------------------------------------------------------------
// Strict-exit parity, one surface at a time. `--strict` is the CI gate and
// `verdict` is the machine claim; a surface whose gate and verdict disagree
// hands a pipeline a green build beside a red document (or the reverse). Each
// test below reads BOTH from the same surface on the same board, on both sides
// of whatever makes that surface refuse.
// ---------------------------------------------------------------------------

/// The `(verdict, ok)` pair a machine surface printed.
fn json_verdict(out: &Output) -> (String, bool) {
    let doc = stdout(out);
    let v: serde_json::Value =
        serde_json::from_str(&doc).unwrap_or_else(|e| panic!("one JSON document ({e}):\n{doc}"));
    (
        v["verdict"]
            .as_str()
            .unwrap_or_else(|| panic!("a verdict field:\n{v}"))
            .to_string(),
        v["ok"]
            .as_bool()
            .unwrap_or_else(|| panic!("an ok field:\n{v}")),
    )
}

/// Assert a surface's strict exit code and its own JSON verdict tell the same
/// story about one board: `invalid` exits 3, `fail` exits 2, `pass` exits 0.
///
/// `surface` is the selector flags (empty for the default machine report). The
/// strict half re-reads the verdict out of the very invocation that exited, so
/// the document and the exit code being compared are the same run's, not two
/// runs that merely agree.
fn assert_gate_matches_verdict(surface: &[&str], board: &Path, want_verdict: &str) {
    let b = board.to_str().unwrap();
    let label = if surface.is_empty() {
        "the default machine report".to_string()
    } else {
        surface.join(" ")
    };
    let mut args = vec!["run", b];
    args.extend_from_slice(surface);
    args.push("--json");
    let json = run(&args);
    let (verdict, ok) = json_verdict(&json);
    assert_eq!(
        verdict,
        want_verdict,
        "{label} on {b} must read {want_verdict}; stderr: {}",
        stderr(&json)
    );
    assert_eq!(ok, verdict == "pass", "ok must mirror the verdict word");
    // Without --strict every surface this helper covers stays exit 0: the
    // verdict is a document field, and turning it into an exit code is what
    // --strict is for. `--thermal` and `--ac` are not covered here because they
    // gate by DEFAULT (their own tests pin that), which is why the helper takes
    // the selector rather than looping over all of them.
    assert_eq!(
        json.status.code(),
        Some(0),
        "{label} must not gate without --strict; stderr: {}",
        stderr(&json)
    );
    let want_code = match want_verdict {
        "invalid" => 3,
        "fail" => 2,
        _ => 0,
    };
    args.push("--strict");
    let strict = run(&args);
    assert_eq!(
        strict.status.code(),
        Some(want_code),
        "{label} --strict must exit {want_code} to match its own '{want_verdict}' verdict; \
         stderr: {}",
        stderr(&strict)
    );
    assert_eq!(
        json_verdict(&strict).0,
        want_verdict,
        "the document the gating invocation printed must say the same thing its exit code did"
    );
}

/// [`assert_gate_matches_verdict`] for the default machine report (a bare
/// `--json` with no selector), which assembles its own verdict and gate.
fn assert_bare_json_gate_matches_verdict(board: &Path, want_verdict: &str) {
    assert_gate_matches_verdict(&[], board, want_verdict);
}

#[test]
fn lint_strict_exit_agrees_with_the_lint_verdict_on_both_sides_of_the_bind_gate() {
    assert_gate_matches_verdict(
        &["--lint"],
        &fixture("verdict_fet_unbound.kicad_pcb"),
        "invalid",
    );
    assert_gate_matches_verdict(&["--lint"], &fixture("verdict_fet_bound.kicad_pcb"), "pass");
}

#[test]
fn si_strict_exit_agrees_with_the_si_verdict_on_both_sides_of_the_bind_gate() {
    assert_gate_matches_verdict(
        &["--si"],
        &fixture("verdict_fet_unbound.kicad_pcb"),
        "invalid",
    );
    assert_gate_matches_verdict(&["--si"], &fixture("verdict_fet_bound.kicad_pcb"), "pass");
}

#[test]
fn check_strict_exit_agrees_with_the_check_verdict_on_both_sides_of_the_bind_gate() {
    assert_gate_matches_verdict(
        &["--check"],
        &fixture("verdict_fet_unbound.kicad_pcb"),
        "invalid",
    );
    assert_gate_matches_verdict(
        &["--check"],
        &fixture("verdict_fet_bound.kicad_pcb"),
        "pass",
    );
}

#[test]
fn drc_strict_exit_agrees_with_the_copper_verdict_the_bind_gate_never_touches() {
    // The exemption, both halves: the unbound FET that invalidates --lint/--si
    // above leaves --drc at `pass` AND at exit 0 under --strict, because copper
    // spacing owes nothing to device models...
    assert_gate_matches_verdict(
        &["--drc"],
        &fixture("verdict_fet_unbound.kicad_pcb"),
        "pass",
    );
    // ...and that exit 0 is not a dead gate: a board with real shorts reads
    // `fail` and exits 2 on the same surface.
    assert_gate_matches_verdict(&["--drc"], &boot_gate_board(), "fail");
}

#[test]
fn usb_c_strict_exit_agrees_with_its_cc_scoped_verdict() {
    // The CC verdict rests on the identity of the parts on the receptacle's CC
    // nets, so the bind gate is scoped to them. An unbound FET sitting on CC1
    // invalidates the surface and exits 3 under --strict...
    assert_gate_matches_verdict(
        &["--usb-c"],
        &fixture("verdict_usb_c_cc_fet_unbound.kicad_pcb"),
        "invalid",
    );
    // ...while the same unbound FET class on a board with no receptacle at all
    // leaves nothing for the CC claim to be inconclusive about: `pass`, exit 0.
    assert_gate_matches_verdict(
        &["--usb-c"],
        &fixture("verdict_fet_unbound.kicad_pcb"),
        "pass",
    );
}

#[test]
fn the_report_surface_describes_the_binding_and_never_gates_on_it() {
    // `--report` is a description of what was modelled, not a pass/fail check,
    // so `--strict` never reaches its renderer (reports/bind.rs takes no strict
    // flag; the pre-surface refusal for a board with no placement still applies
    // and exits 3 over an `invalid` document, like every other surface). Its
    // verdict therefore may not read `invalid` either: incomplete binding is
    // this report's SUBJECT, printed in full, and binding completeness reaches a
    // verdict only through the verdict-critical bind gate this surface is exempt
    // from. A document saying `ok:false` beside a command that exits 0 hands a
    // pipeline two answers.
    for name in [
        "verdict_fet_unbound.kicad_pcb",
        "verdict_fet_bound.kicad_pcb",
    ] {
        let b = fixture(name);
        assert_gate_matches_verdict(&["--report"], &b, "pass");
        // Exempt from the gate is not the same as silent: the per-net binding
        // evidence the exemption declines to gate on is still in the document
        // for the unbound board.
        let out = run(&["run", b.to_str().unwrap(), "--report", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
        let undermined_binding = v["evidence"].as_array().expect("evidence").iter().any(|m| {
            m["status"] == "undermined"
                && m["assertion"]
                    .as_str()
                    .is_some_and(|a| a.starts_with("Binding completeness"))
        });
        assert_eq!(
            undermined_binding,
            name.contains("unbound"),
            "the descriptive report still names the net it could not bind ({name}):\n{v}"
        );
    }
}

// ---------------------------------------------------------------------------
// A surface's own strict gate can be WIDER than the shared `serious` severity:
// `--lint` gates on medium-severity findings and `--si` on any finding at all,
// both of which serialize as `warning`/`note`. The verdict field has to know,
// or the same invocation prints `"verdict":"pass"` and exits 2.
// ---------------------------------------------------------------------------

#[test]
fn a_medium_lint_finding_fails_the_verdict_because_it_fails_the_gate() {
    let b = fixture("verdict_medium_lint.kicad_pcb");
    // No finding here is `serious`: the gate grade is the surface's own.
    let out = run(&["run", b.to_str().unwrap(), "--lint", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    assert_eq!(v["serious_count"], 0, "{v}");
    assert!(
        v["findings"]
            .as_array()
            .expect("findings")
            .iter()
            .any(|f| f["severity"] == "warning" && f["kind"] == "placeholder_value"),
        "the fixture's gating finding is a medium/warning one:\n{v}"
    );
    // ...and yet it gates, so `fail` is the only verdict that can sit beside
    // the exit code, on every surface whose gate includes it.
    for surface in [["--lint"], ["--check"]] {
        assert_gate_matches_verdict(&surface, &b, "fail");
    }
    // The bare machine report shares `--check`'s gate.
    assert_bare_json_gate_matches_verdict(&b, "fail");
    // Under GitHub Actions a gating run's stdout is STILL exactly one JSON
    // document: the workflow annotations are stderr, not report content. This
    // is the exit-2 route, the one that used to append `::error` lines after
    // the document and break a consumer parsing it.
    let out = run_in_actions(&["run", b.to_str().unwrap(), "--lint", "--json", "--strict"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert_eq!(
        json_verdict(&out).0,
        "fail",
        "stdout must parse as one document under GitHub Actions:\n{}",
        stdout(&out)
    );
    let err = stderr(&out);
    assert!(
        err.lines()
            .any(|l| l.starts_with("::error ") && l.contains("--strict gate")),
        "the gate annotated the checks tab, on stderr:\n{err}"
    );
}

/// The `--si` half of the same widening, on a real board: `si_fails` counts its
/// medium finding, which serializes as `warning`, so `serious_count` stays 0
/// while both the verdict and the exit code have to say the run failed.
#[test]
fn a_medium_si_finding_fails_the_verdict_because_it_fails_the_gate() {
    let b = medium_si_board();
    let out = run(&["run", b.to_str().unwrap(), "--si", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    assert_eq!(v["serious_count"], 0, "{v}");
    assert!(
        v["findings"]
            .as_array()
            .expect("findings")
            .iter()
            .any(|f| f["severity"] == "warning"),
        "the gating SI finding is a medium/warning one:\n{v}"
    );
    assert_gate_matches_verdict(&["--si"], &b, "fail");
}

#[test]
fn the_resources_subset_and_the_bare_machine_report_gate_like_their_verdicts() {
    // Two more routes with verdict/gate code of their own: `--resources` (the
    // MCU-conflict subset of the lint family) and the default machine report a
    // bare `--json` produces.
    assert_gate_matches_verdict(
        &["--resources"],
        &fixture("verdict_fet_unbound.kicad_pcb"),
        "invalid",
    );
    assert_gate_matches_verdict(
        &["--resources"],
        &fixture("verdict_fet_bound.kicad_pcb"),
        "pass",
    );
    assert_bare_json_gate_matches_verdict(&fixture("verdict_fet_unbound.kicad_pcb"), "invalid");
    assert_bare_json_gate_matches_verdict(&fixture("verdict_fet_bound.kicad_pcb"), "pass");
}

// ---------------------------------------------------------------------------
// The CI artifact surfaces (JUnit, SARIF, GitHub annotations) carry the same
// verdict. The JUnit agreement test above covers `--check`; these extend it to
// a specialist surface and to the co-sim path, whose artifacts are rewritten
// after the run.
// ---------------------------------------------------------------------------

/// The `<testsuites …>` root line of a JUnit document.
fn junit_root(xml: &str) -> String {
    xml.lines()
        .find(|l| l.trim_start().starts_with("<testsuites"))
        .unwrap_or_else(|| panic!("a JUnit root element in:\n{xml}"))
        .to_string()
}

fn junit_all_green(xml: &str) -> bool {
    let root = junit_root(xml);
    root.contains("failures=\"0\"") && root.contains("errors=\"0\"")
}

/// Every `<testcase name="…">` in a JUnit document.
fn junit_testcases(xml: &str) -> Vec<String> {
    xml.split("<testcase name=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next().map(str::to_string))
        .collect()
}

#[test]
fn junit_and_sarif_agree_with_a_specialist_surfaces_verdict() {
    // --junit/--sarif follow the selected specialist surface, including its
    // model-dependent bind refusal, so it cannot hand CI a green artifact
    // beside its own invalid JSON verdict.
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, want_invalid) in [
        ("verdict_fet_unbound.kicad_pcb", true),
        ("verdict_fet_bound.kicad_pcb", false),
    ] {
        let b = fixture(name);
        let junit = dir.path().join(format!("{name}.xml"));
        let sarif = dir.path().join(format!("{name}.sarif"));
        let out = run(&[
            "run",
            b.to_str().unwrap(),
            "--lint",
            "--json",
            "--junit",
            junit.to_str().unwrap(),
            "--sarif",
            sarif.to_str().unwrap(),
        ]);
        assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
        let (verdict, _) = json_verdict(&out);
        assert_eq!(
            verdict == "invalid",
            want_invalid,
            "--lint --json verdict on {name}"
        );

        let xml = std::fs::read_to_string(&junit).expect("junit written");
        assert_eq!(
            xml.contains("INVALID evidence:"),
            want_invalid,
            "the JUnit file must carry the bind blocker exactly when the verdict is \
             invalid ({name}):\n{xml}"
        );
        assert_eq!(
            !junit_all_green(&xml),
            want_invalid,
            "the JUnit root must be red exactly when the JSON verdict is invalid ({name}): {}",
            junit_root(&xml)
        );

        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sarif).expect("sarif written"))
                .expect("valid SARIF JSON");
        let results = doc["runs"][0]["results"].as_array().expect("results array");
        assert_eq!(
            results
                .iter()
                .any(|r| r["ruleId"] == "evidence/undermined" && r["level"] == "error"),
            want_invalid,
            "SARIF must raise an error-level evidence result exactly when the verdict is \
             invalid ({name}):\n{results:?}"
        );
    }
}

#[test]
fn github_annotations_agree_with_a_specialist_surfaces_verdict() {
    // The third artifact surface: an error annotation appears with the invalid
    // verdict and nowhere else, so a pull request's checks tab cannot read
    // clean beside a refused document. No `--junit` here on purpose: with an
    // artifact flag the run annotates from the artifact writer too, which would
    // pass this test without the gating exit ever annotating anything, and a
    // pipeline that gates on the exit code alone would still get a silent
    // checks tab.
    for surface in ["--lint", "--si", "--check", "--resources"] {
        let unbound = fixture("verdict_fet_unbound.kicad_pcb");
        let out = run_in_actions(&[
            "run",
            unbound.to_str().unwrap(),
            surface,
            "--json",
            "--strict",
        ]);
        assert_eq!(
            out.status.code(),
            Some(3),
            "{surface} gates on the bind blocker; stderr: {}",
            stderr(&out)
        );
        let err = stderr(&out);
        assert!(
            err.lines().any(|l| l.starts_with("::error ")
                && l.contains("Q1")
                && l.contains("INCONCLUSIVE:")),
            "{surface} annotates the blocker in the shared sentence:\n{err}"
        );
        assert_eq!(json_verdict(&out).0, "invalid");

        let bound = fixture("verdict_fet_bound.kicad_pcb");
        let out = run_in_actions(&[
            "run",
            bound.to_str().unwrap(),
            surface,
            "--json",
            "--strict",
        ]);
        assert!(
            !stderr(&out).lines().any(|l| l.starts_with("::error ")),
            "no error annotation on the bound twin ({surface}):\n{}",
            stderr(&out)
        );
    }
    // Exactly once, not once per call site. With an artifact flag the same
    // blockers are reachable from both the artifact writer and the gating exit,
    // and a duplicate spends one of GitHub's ten annotations per type per step
    // on a line the reader has already seen.
    let dir = tempfile::tempdir().expect("tempdir");
    let unbound = fixture("verdict_fet_unbound.kicad_pcb");
    let out = run_in_actions(&[
        "run",
        unbound.to_str().unwrap(),
        "--check",
        "--json",
        "--strict",
        "--junit",
        dir.path().join("out.xml").to_str().unwrap(),
    ]);
    let err = stderr(&out);
    assert_eq!(
        err.lines()
            .filter(|l| l.starts_with("::error ") && l.contains("evidence undermined"))
            .count(),
        1,
        "the blocker annotation fires once, not once per call site:\n{err}"
    );
}

// ---------------------------------------------------------------------------
// The widened `fail` route on the artifact surfaces. A gate that is wider than
// the shared `serious` grade used to leave the archived JUnit reading
// `failures="0"` beside a red verdict and a non-zero exit, which tells a CI
// dashboard the build was fine. The artifacts grade on the finding's own
// `gating` flag, so all three sides of that run say the same thing.
// ---------------------------------------------------------------------------

/// Every `<testcase>` in a JUnit document that carries a `<failure>`.
fn junit_failing_testcases(xml: &str) -> Vec<String> {
    xml.split("<testcase name=\"")
        .skip(1)
        .filter(|rest| {
            rest.split("</testcase>")
                .next()
                .unwrap_or("")
                .contains("<failure")
        })
        .filter_map(|rest| rest.split('"').next().map(str::to_string))
        .collect()
}

/// SARIF `(ruleId, level)` pairs.
fn sarif_levels(doc: &serde_json::Value) -> Vec<(String, String)> {
    doc["runs"][0]["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|r| {
            (
                r["ruleId"].as_str().unwrap_or_default().to_string(),
                r["level"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

#[test]
fn a_gating_medium_finding_is_red_in_the_archived_artifact_too() {
    // The fixture's only finding is a medium/`warning` one, so nothing here is
    // `serious`: the severity word alone made this artifact all-green while the
    // verdict said `fail` and `--strict` exited 2.
    let b = fixture("verdict_medium_lint.kicad_pcb");
    let dir = tempfile::tempdir().expect("tempdir");
    let junit = dir.path().join("out.xml");
    let sarif = dir.path().join("out.sarif");
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--lint",
        "--json",
        "--strict",
        "--junit",
        junit.to_str().unwrap(),
        "--sarif",
        sarif.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    assert_eq!(v["verdict"], "fail");
    assert_eq!(
        v["serious_count"], 0,
        "the gating finding is not serious:\n{v}"
    );

    let xml = std::fs::read_to_string(&junit).expect("junit written");
    assert!(
        !junit_all_green(&xml),
        "a run that exited 2 must not archive an all-green report: {}",
        junit_root(&xml)
    );
    assert_eq!(
        junit_failing_testcases(&xml),
        vec!["placeholder_value R3".to_string()],
        "the failing testcase is the gating finding itself:\n{xml}"
    );

    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sarif).expect("sarif written"))
            .expect("valid SARIF JSON");
    assert!(
        sarif_levels(&doc).contains(&("lint/placeholder_value".into(), "error".into())),
        "SARIF must raise the gating finding at error level:\n{:?}",
        sarif_levels(&doc)
    );
}

#[test]
fn the_same_board_archives_all_green_once_a_waiver_takes_the_gate_away() {
    // The other side of the same board: an active waiver overrules the only
    // gating finding, so the gate is not engaged, the verdict is `pass`, the
    // exit is 0 and the archived report has to be green. This is the control on
    // the test above, not a second reading of the flag: waivers are applied
    // before the artifact is written, so the finding is absent from the file
    // either way. What it pins is that the three surfaces go green together.
    let dir = tempfile::tempdir().expect("tempdir");
    let b = dir.path().join("verdict_medium_lint.kicad_pcb");
    std::fs::copy(fixture("verdict_medium_lint.kicad_pcb"), &b).expect("stage the board");
    // Beside the board, where `WaiverSet::discover` looks.
    std::fs::write(
        dir.path().join("hauksbee-waivers.toml"),
        "[[waive]]\ncheck = \"lint\"\nkind = \"placeholder_value\"\nrefs = [\"R3\"]\n\
         reason = \"R3 is a documented DNP placeholder on this build\"\n\
         until = \"2999-01-01\"\n",
    )
    .expect("stage the waiver");
    let junit = dir.path().join("out.xml");
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--lint",
        "--json",
        "--strict",
        "--junit",
        junit.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    assert_eq!(v["verdict"], "pass");
    assert_eq!(
        v["waived"].as_array().map(Vec::len),
        Some(1),
        "the overruled finding is reported, not hidden:\n{v}"
    );
    let xml = std::fs::read_to_string(&junit).expect("junit written");
    assert!(
        junit_all_green(&xml),
        "an exit-0 pass must archive a green report: {}",
        junit_root(&xml)
    );

    // Every static surface has to excuse it, on the same board, or the exit code
    // and the archived file are answering with different suites. The bare
    // `--json` gate skipped waivers entirely: it exited 2 on the excused finding
    // while the artifact, written from the waived-down suite, said
    // `failures="0"`. That is the same split verdict this contract exists to
    // forbid, on the likeliest CI invocation of all.
    for surface in [
        vec!["--json", "--strict"],
        vec!["--check", "--json", "--strict"],
        vec!["--lint", "--json", "--strict"],
    ] {
        let junit = dir.path().join(format!("{}.xml", surface.join("")));
        let mut args = vec!["run", b.to_str().unwrap()];
        args.extend(surface.iter().copied());
        args.extend(["--junit", junit.to_str().unwrap()]);
        let out = run(&args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{surface:?} must excuse the waived finding; stderr: {}",
            stderr(&out)
        );
        let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
        assert_eq!(v["verdict"], "pass", "{surface:?}: {v}");
        assert_eq!(
            v["waived"].as_array().map(Vec::len),
            Some(1),
            "{surface:?} reports the overruled finding rather than hiding it:\n{v}"
        );
        assert!(
            v["findings"]
                .as_array()
                .is_none_or(|f| f.iter().all(|f| f["kind"] != "placeholder_value")),
            "{surface:?} must not carry the excused finding as live:\n{v}"
        );
        let xml = std::fs::read_to_string(&junit).expect("junit written");
        assert!(
            junit_all_green(&xml),
            "{surface:?} archived a red report beside exit 0: {}",
            junit_root(&xml)
        );
    }
}

#[test]
fn specialist_artifacts_and_annotations_grade_only_the_selected_surface() {
    // A requested artifact describes THIS invocation. `--drc` did not run the
    // lint gate, so a lint-only board must be green in JSON, exit status, JUnit,
    // SARIF and GitHub annotations together. A hidden full-suite result would
    // give CI mutually exclusive answers about one command.
    let b = fixture("verdict_medium_lint.kicad_pcb");
    let dir = tempfile::tempdir().expect("tempdir");
    let junit = dir.path().join("drc-selector.xml");
    let sarif = dir.path().join("drc-selector.sarif");
    let out = run_in_actions(&[
        "run",
        b.to_str().unwrap(),
        "--drc",
        "--json",
        "--strict",
        "--junit",
        junit.to_str().unwrap(),
        "--sarif",
        sarif.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "the copper surface has nothing to gate on here; stderr: {}",
        stderr(&out)
    );
    assert_eq!(json_verdict(&out).0, "pass");
    let xml = std::fs::read_to_string(&junit).expect("junit written");
    assert!(junit_all_green(&xml), "selected DRC artifact: {xml}");
    assert!(
        xml.contains("testsuite name=\"drc\"")
            && !xml.contains("testsuite name=\"lint\"")
            && !xml.contains("testsuite name=\"si\""),
        "an unselected check must not appear as if it ran:\n{xml}"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sarif).expect("sarif written"))
            .expect("valid SARIF JSON");
    assert!(
        sarif_levels(&doc).iter().all(|(_, level)| level != "error"),
        "selected DRC SARIF must not contain an unselected lint error: {doc}"
    );
    assert!(
        !stderr(&out)
            .lines()
            .any(|line| line.starts_with("::error ")),
        "a passing selected surface must not emit an error annotation:\n{}",
        stderr(&out)
    );
}

#[test]
fn usb_c_artifact_ignores_unrelated_bind_blockers_like_the_selected_report() {
    let board = fixture("verdict_fet_unbound.kicad_pcb");
    let dir = tempfile::tempdir().expect("tempdir");
    let junit = dir.path().join("usb-c.xml");
    let out = run(&[
        "run",
        board.to_str().unwrap(),
        "--usb-c",
        "--json",
        "--strict",
        "--junit",
        junit.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(json_verdict(&out).0, "pass");
    let xml = std::fs::read_to_string(&junit).expect("junit");
    assert!(
        junit_all_green(&xml) && !xml.contains("Q1"),
        "an unrelated open FET is outside the selected CC claim:\n{xml}"
    );
}

#[test]
fn a_cosim_fault_run_archives_a_failing_testcase() {
    // Every raised fault fails the co-sim gate, and the plain-language
    // classifier grades most of them `warning`, so this whole family was
    // archived green beside a `fail` verdict and an exit 2 under `--strict`.
    let board = fixture("cosim_fault_led.kicad_pcb");
    let dir = tempfile::tempdir().expect("tempdir");
    let junit = dir.path().join("out.xml");
    let sarif = dir.path().join("out.sarif");
    let out = run(&[
        "run",
        board.to_str().unwrap(),
        "--headless",
        "--seconds",
        "0.05",
        "--json",
        "--junit",
        junit.to_str().unwrap(),
        "--sarif",
        sarif.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let (verdict, _) = json_verdict(&out);
    assert_eq!(verdict, "fail", "the faults fail this run's own gate");

    let xml = std::fs::read_to_string(&junit).expect("junit written");
    let failing = junit_failing_testcases(&xml);
    assert!(
        failing.iter().any(|c| c == "overpower R1"),
        "the fault that failed the gate is the failing testcase: {failing:?}\n{xml}"
    );
    assert!(
        !junit_all_green(&xml),
        "a `fail` verdict must not archive an all-green report: {}",
        junit_root(&xml)
    );
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sarif).expect("sarif written"))
            .expect("valid SARIF JSON");
    assert!(
        sarif_levels(&doc).contains(&("cosim/overpower".into(), "error".into())),
        "SARIF raises the fault at error level:\n{:?}",
        sarif_levels(&doc)
    );

    // The armed side: the co-sim exit gate is asked of these same flags, so
    // under `--strict` the exit code is 2 beside the same failing testcase. This
    // board also refuses (no firmware to exercise), and `fail` outranks that, so
    // 2 is the code the flags have to produce.
    let strict_junit = dir.path().join("strict.xml");
    let out = run(&[
        "run",
        board.to_str().unwrap(),
        "--headless",
        "--seconds",
        "0.05",
        "--json",
        "--strict",
        "--junit",
        strict_junit.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert_eq!(json_verdict(&out).0, "fail");
    let strict_xml = std::fs::read_to_string(&strict_junit).expect("junit written");
    assert!(
        junit_failing_testcases(&strict_xml)
            .iter()
            .any(|c| c == "overpower R1"),
        "the archived file names the fault the exit 2 was about:\n{strict_xml}"
    );
}

#[test]
fn an_invalid_verdict_keeps_the_invalid_shape_in_the_artifact() {
    // Precedence: widening what counts as a failure must not relabel the
    // `invalid` route. This board's red comes from unbound verdict-critical
    // parts, so the artifact carries the INVALID evidence blocker as its only
    // gate-grade entry and the document still reads `invalid`, not `fail`.
    let b = fixture("verdict_fet_unbound.kicad_pcb");
    let dir = tempfile::tempdir().expect("tempdir");
    let junit = dir.path().join("out.xml");
    let sarif = dir.path().join("out.sarif");
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--check",
        "--json",
        "--junit",
        junit.to_str().unwrap(),
        "--sarif",
        sarif.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(json_verdict(&out).0, "invalid");

    let xml = std::fs::read_to_string(&junit).expect("junit written");
    assert_eq!(
        junit_failing_testcases(&xml),
        vec!["undermined Q1".to_string()],
        "the blocker is the only gate-grade entry:\n{xml}"
    );
    assert!(
        xml.contains("INVALID evidence:"),
        "and it keeps the invalid wording rather than reading as a plain \
         finding failure:\n{xml}"
    );
    // `<error>` belongs to a whole-run refusal (exit 3), which this is not: the
    // two red shapes stay distinct.
    assert!(junit_root(&xml).contains("errors=\"0\""), "{xml}");

    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sarif).expect("sarif written"))
            .expect("valid SARIF JSON");
    let levels = sarif_levels(&doc);
    assert!(
        levels.contains(&("evidence/undermined".into(), "error".into())),
        "{levels:?}"
    );
    assert!(
        !levels
            .iter()
            .any(|(id, _)| id == "hauksbee/invalid-for-analysis"),
        "no refusal result on a run that reached a verdict:\n{levels:?}"
    );
}

#[test]
fn a_medium_finding_that_does_not_gate_stays_a_passing_testcase() {
    // The direction that must NOT turn red. A copper short on a board format
    // the copper extraction was never validated against may be phantom, so it
    // grades `warning` and the copper gate excuses it: the run passes, exits 0,
    // and the finding is archived as a passing case carrying its text. It wears
    // the same `warning` severity a gating lint finding wears, which is how the
    // artifact proves it grades the gate and not the vocabulary.
    let dir = tempfile::tempdir().expect("tempdir");
    let b = dir.path().join("phantom_short.kicad_pcb");
    let text = std::fs::read_to_string(boot_gate_board()).expect("the shorted example board");
    // Same two shorts, re-declared as a KiCad version the copper extraction has
    // not been validated against.
    let (first, rest) = text.split_once('\n').expect("a header line");
    assert!(
        first.contains("(version 20171130)"),
        "the fixture's version line moved: {first}"
    );
    std::fs::write(
        &b,
        format!("{}\n{rest}", first.replace("20171130", "20260206")),
    )
    .expect("stage the board");

    let junit = dir.path().join("out.xml");
    let out = run(&[
        "run",
        b.to_str().unwrap(),
        "--check",
        "--json",
        "--strict",
        "--junit",
        junit.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    assert_eq!(v["verdict"], "pass");
    assert_eq!(
        v["drc"]["shorts"].as_array().map(Vec::len),
        Some(2),
        "the shorts are still reported, not dropped:\n{v}"
    );
    let xml = std::fs::read_to_string(&junit).expect("junit written");
    assert!(
        junit_all_green(&xml),
        "an excused finding must not fail the archived report: {}",
        junit_root(&xml)
    );
    assert!(
        junit_testcases(&xml)
            .iter()
            .any(|c| c.starts_with("short ")),
        "and it is still archived, as a passing case:\n{xml}"
    );
    assert!(
        xml.contains("<system-out>copper short:"),
        "carrying its text:\n{xml}"
    );
}

#[test]
fn cosim_junit_agrees_with_the_cosim_json_verdict_about_bind_blockers() {
    // The co-sim machine report is bind-gated like the static ones, and its CI
    // artifacts are REWRITTEN after the run; the rewrite must not lose the
    // blocker and leave a green test report beside an invalid verdict.
    let unbound = fixture("verdict_fet_unbound.kicad_pcb");
    let dir = tempfile::tempdir().expect("tempdir");
    let junit = dir.path().join("cosim.xml");
    let out = run(&[
        "run",
        unbound.to_str().unwrap(),
        "--headless",
        "--seconds",
        "0.05",
        "--json",
        "--junit",
        junit.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(
        json_verdict(&out).0,
        "invalid",
        "an unbound verdict-critical part gates the co-sim JSON verdict too"
    );
    let xml = std::fs::read_to_string(&junit).expect("junit written");
    assert!(
        xml.contains("INVALID evidence:") && xml.contains("Q1"),
        "the rewritten co-sim artifact keeps the bind blocker:\n{xml}"
    );
    assert!(
        !junit_all_green(&xml),
        "the co-sim JUnit root must be red beside an invalid verdict: {}",
        junit_root(&xml)
    );
}

#[test]
fn the_cosim_refusal_rewrite_keeps_every_finding_the_complete_artifact_carried() {
    // A strict co-sim that refuses (zero activity here) rewrites the artifacts
    // with the refusal attached. That rewrite is the dangerous one: writing the
    // refusal alone would erase real electrical faults from the test report a
    // pipeline archives. The refusing run's artifact must be a SUPERSET of the
    // complete run's.
    let board = fixture("cosim_fault_led.kicad_pcb");
    let dir = tempfile::tempdir().expect("tempdir");
    let complete = dir.path().join("complete.xml");
    let out = run(&[
        "run",
        board.to_str().unwrap(),
        "--headless",
        "--seconds",
        "0.05",
        "--junit",
        complete.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let complete_cases = junit_testcases(&std::fs::read_to_string(&complete).expect("junit"));
    assert!(
        complete_cases.iter().any(|c| c == "overpower R1"),
        "the complete artifact carries the stress faults: {complete_cases:?}"
    );

    let refused = dir.path().join("refused.xml");
    let refused_sarif = dir.path().join("refused.sarif");
    let out = run_in_actions(&[
        "run",
        board.to_str().unwrap(),
        "--headless",
        "--seconds",
        "0.05",
        "--strict",
        "--json",
        "--junit",
        refused.to_str().unwrap(),
        "--sarif",
        refused_sarif.to_str().unwrap(),
    ]);
    // This run both refuses (no firmware activity to vouch for) and found real
    // faults, and `fail` outranks `invalid`: the exit code is 2, matching the
    // verdict its own document printed, while the refusal stays on stderr and
    // in the artifacts.
    assert_eq!(
        json_verdict(&out).0,
        "fail",
        "raised faults are a judgement the run CAN make"
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "the findings code, not invalid-for-analysis; stderr: {}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("zero net toggles"),
        "the refusal is not silenced by the fault gate taking the exit:\n{}",
        stderr(&out)
    );

    let xml = std::fs::read_to_string(&refused).expect("junit");
    let refused_cases = junit_testcases(&xml);
    for case in &complete_cases {
        assert!(
            refused_cases.contains(case),
            "the refusal rewrite dropped '{case}' from the archived report:\n{xml}"
        );
    }
    assert!(
        xml.contains("requested claim is answerable"),
        "the refusal itself is in the artifact too:\n{xml}"
    );
    assert!(
        !junit_all_green(&xml),
        "a refused run is not an all-green test report: {}",
        junit_root(&xml)
    );

    // SARIF is rewritten from the same findings list, so it owes the same
    // superset plus the refusal rule.
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&refused_sarif).expect("sarif written"))
            .expect("valid SARIF JSON");
    let results = doc["runs"][0]["results"].as_array().expect("results array");
    assert!(
        results.iter().any(|r| r["ruleId"] == "cosim/overpower"),
        "the refusal rewrite kept the stress faults in SARIF too:\n{results:?}"
    );
    assert!(
        results
            .iter()
            .any(|r| r["ruleId"] == "hauksbee/invalid-for-analysis" && r["level"] == "error"),
        "and raised the refusal as an error-level result:\n{results:?}"
    );
    // The third artifact surface must not be the quiet one: a refusal that
    // shows red in JUnit and SARIF has to annotate the checks tab as well.
    let err = stderr(&out);
    assert!(
        err.lines()
            .any(|l| l.starts_with("::error ") && l.contains("invalid for analysis")),
        "the refusal reaches the GitHub annotation surface:\n{err}"
    );
}

/// `--strict-boot` is a gate like any other, so under it the boot advisory is
/// gate-grade for the co-sim document too. Needs real firmware (the advisory
/// only speaks when the MCU actually ran), so it rides the AVR fixture the
/// boot-advisory CLI test uses. That .hex is tracked, so the skip below is belt
/// and braces for a partial checkout, not an expected outcome: a silent no-op
/// here means the fixture went missing, not that the test had nothing to do.
#[cfg(feature = "avr")]
#[test]
fn strict_boot_verdict_agrees_with_the_strict_boot_exit() {
    let b = boot_gate_board();
    let fw = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/boot_gate_a/boot_gate.hex");
    assert!(
        fw.exists(),
        "required tracked firmware fixture: {}",
        fw.display()
    );
    let base = [
        "run",
        b.to_str().unwrap(),
        "--firmware",
        fw.to_str().unwrap(),
        "--headless",
        "--seconds",
        "0.05",
        "--json",
    ];
    // Advisory only: exit 0 and a verdict that says so.
    let out = run(&base);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(
        json_verdict(&out).0,
        "pass",
        "the boot advisory does not gate without --strict-boot"
    );
    // Escalated: exit 2 AND `fail` in the document that exit was printed beside.
    let dir = tempfile::tempdir().expect("tempdir");
    let junit = dir.path().join("strict-boot.xml");
    let sarif = dir.path().join("strict-boot.sarif");
    let mut args = base.to_vec();
    args.extend([
        "--strict-boot",
        "--junit",
        junit.to_str().unwrap(),
        "--sarif",
        sarif.to_str().unwrap(),
    ]);
    let out = run_in_actions(&args);
    assert_eq!(
        out.status.code(),
        Some(2),
        "--strict-boot escalates the advisory; stderr: {}",
        stderr(&out)
    );
    assert_eq!(
        json_verdict(&out).0,
        "fail",
        "and the document must not read `pass` beside that exit 2"
    );
    let xml = std::fs::read_to_string(&junit).expect("junit written");
    assert!(
        junit_failing_testcases(&xml)
            .iter()
            .any(|case| case.contains("boot_control_net") && case.contains("GATE_CTRL")),
        "the strict-boot gate itself must be a failing testcase:\n{xml}"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sarif).expect("sarif written"))
            .expect("valid SARIF JSON");
    assert!(
        sarif_levels(&doc).contains(&("cosim/boot_control_net".into(), "error".into())),
        "strict-boot must be an error-level SARIF result: {doc}"
    );
    assert!(
        stderr(&out).lines().any(|line| {
            line.starts_with("::error ")
                && line.contains("strict-boot")
                && line.contains("GATE_CTRL")
        }),
        "strict-boot must annotate the GitHub surface:\n{}",
        stderr(&out)
    );
}

#[test]
fn a_pre_analysis_refusal_replaces_stale_requested_artifacts() {
    const EMPTY_BOARD_DSL: &str = "# Board-as-Code (hauksbee board DSL v1)\n\
board version 20241229\n\nfn main {\n}\n";
    let dir = tempfile::tempdir().expect("tempdir");
    let board = dir.path().join("empty.board");
    let junit = dir.path().join("out.xml");
    let sarif = dir.path().join("out.sarif");
    std::fs::write(&board, EMPTY_BOARD_DSL).expect("empty board fixture");
    std::fs::write(&junit, "stale green JUnit from a previous run").expect("stale junit");
    std::fs::write(&sarif, "stale green SARIF from a previous run").expect("stale sarif");

    let out = run_in_actions(&[
        "run",
        board.to_str().unwrap(),
        "--check",
        "--json",
        "--junit",
        junit.to_str().unwrap(),
        "--sarif",
        sarif.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(3), "stderr: {}", stderr(&out));
    let xml = std::fs::read_to_string(&junit).expect("junit replaced");
    assert!(
        junit_root(&xml).contains("errors=\"1\"") && xml.contains("no components"),
        "the refusal must replace the stale artifact:\n{xml}"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sarif).expect("sarif replaced"))
            .expect("valid replacement SARIF");
    assert!(
        sarif_levels(&doc).contains(&("hauksbee/invalid-for-analysis".into(), "error".into())),
        "the refusal must replace stale SARIF: {doc}"
    );
    assert!(
        stderr(&out)
            .lines()
            .any(|line| line.starts_with("::error ") && line.contains("no components")),
        "the refusal must annotate GitHub:\n{}",
        stderr(&out)
    );
}

#[test]
fn missing_processor_refusal_finalizes_every_requested_surface() {
    let board = fully_covered_board();
    let firmware =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/firmware/demo/demo.hex");
    assert!(
        firmware.exists(),
        "required tracked firmware fixture: {}",
        firmware.display()
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let junit = dir.path().join("missing-processor.xml");
    let sarif = dir.path().join("missing-processor.sarif");
    let out = run_in_actions(&[
        "run",
        board.to_str().unwrap(),
        "--firmware",
        firmware.to_str().unwrap(),
        "--headless",
        "--seconds",
        "0.01",
        "--json",
        "--junit",
        junit.to_str().unwrap(),
        "--sarif",
        sarif.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(3), "stderr: {}", stderr(&out));
    let xml = std::fs::read_to_string(&junit).expect("junit finalized");
    assert!(
        junit_root(&xml).contains("errors=\"1\"")
            && xml.contains("supported MCU")
            && !xml.contains("did not reach its final outcome"),
        "the concrete missing-processor refusal must replace the pending marker:\n{xml}"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sarif).expect("sarif finalized"))
            .expect("valid SARIF");
    assert!(
        doc["runs"][0]["results"]
            .as_array()
            .expect("results")
            .iter()
            .any(|result| {
                result["ruleId"] == "hauksbee/invalid-for-analysis"
                    && result["message"]["text"]
                        .as_str()
                        .is_some_and(|message| message.contains("supported MCU"))
            }),
        "SARIF carries the concrete refusal: {doc}"
    );
    assert!(
        stderr(&out)
            .lines()
            .any(|line| line.starts_with("::error ") && line.contains("supported MCU")),
        "GitHub carries the concrete refusal:\n{}",
        stderr(&out)
    );
}

#[test]
fn artifact_transaction_refuses_to_overwrite_its_board_input() {
    let dir = tempfile::tempdir().expect("tempdir");
    let board = dir.path().join("board.kicad_pcb");
    let original = std::fs::read(fixture("verdict_fet_bound.kicad_pcb")).expect("fixture");
    std::fs::write(&board, &original).expect("staged board");
    let out = run(&[
        "run",
        board.to_str().unwrap(),
        "--check",
        "--junit",
        board.to_str().unwrap(),
    ]);
    assert!(
        !out.status.success(),
        "an output/input alias must be refused"
    );
    assert_eq!(
        std::fs::read(&board).expect("board retained"),
        original,
        "artifact initialization must never clobber a run input"
    );
}

#[test]
fn artifact_transaction_refuses_a_hard_link_to_its_board_input() {
    let dir = tempfile::tempdir().expect("tempdir");
    let board = dir.path().join("board.kicad_pcb");
    let junit = dir.path().join("board-hard-link.xml");
    let original = std::fs::read(fixture("verdict_fet_bound.kicad_pcb")).expect("fixture");
    std::fs::write(&board, &original).expect("staged board");
    std::fs::hard_link(&board, &junit).expect("hard link supported for staged files");

    let out = run(&[
        "run",
        board.to_str().unwrap(),
        "--check",
        "--junit",
        junit.to_str().unwrap(),
    ]);

    assert!(
        !out.status.success(),
        "a hard-linked output/input alias must be refused"
    );
    assert_eq!(
        std::fs::read(&board).expect("board retained"),
        original,
        "artifact initialization must never write through a hard link to an input"
    );
}

#[test]
fn artifact_transaction_refuses_hard_linked_junit_and_sarif_outputs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let junit = dir.path().join("out.xml");
    let sarif = dir.path().join("out.sarif");
    let original = b"existing artifact bytes that must survive validation";
    std::fs::write(&junit, original).expect("seed junit");
    std::fs::hard_link(&junit, &sarif).expect("hard link supported for staged files");

    let out = run(&[
        "run",
        fixture("verdict_fet_bound.kicad_pcb").to_str().unwrap(),
        "--check",
        "--junit",
        junit.to_str().unwrap(),
        "--sarif",
        sarif.to_str().unwrap(),
    ]);

    assert!(
        !out.status.success(),
        "hard-linked artifact outputs need different files"
    );
    assert_eq!(
        std::fs::read(&junit).expect("seed retained"),
        original,
        "alias validation must happen before either output is written"
    );
}

#[test]
fn parse_error_never_writes_through_hard_linked_artifact_outputs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let junit = dir.path().join("usage.xml");
    let sarif = dir.path().join("usage.sarif");
    let original = b"existing linked artifact bytes";
    std::fs::write(&junit, original).expect("seed junit");
    std::fs::hard_link(&junit, &sarif).expect("hard link supported for staged files");

    let out = run(&[
        "run",
        fixture("verdict_fet_bound.kicad_pcb").to_str().unwrap(),
        "--junit",
        junit.to_str().unwrap(),
        "--sarif",
        sarif.to_str().unwrap(),
        "--not-a-real-run-flag",
    ]);

    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert_eq!(
        std::fs::read(&junit).expect("linked seed retained"),
        original,
        "parse-error invalidation must reject linked outputs before either write"
    );
}

#[test]
fn unsafe_artifact_alias_still_invalidates_every_other_safe_requested_surface() {
    let dir = tempfile::tempdir().expect("tempdir");
    let board = dir.path().join("board.kicad_pcb");
    let sarif = dir.path().join("out.sarif");
    let original = std::fs::read(fixture("verdict_fet_bound.kicad_pcb")).expect("fixture");
    std::fs::write(&board, &original).expect("staged board");
    std::fs::write(&sarif, "stale green sarif").expect("stale sarif");
    let out = run(&[
        "run",
        board.to_str().unwrap(),
        "--check",
        "--junit",
        board.to_str().unwrap(),
        "--sarif",
        sarif.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    assert_eq!(
        std::fs::read(&board).expect("board retained"),
        original,
        "unsafe artifact path must never clobber the board"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sarif).expect("sarif replaced"))
            .expect("valid replacement SARIF");
    assert!(
        doc["runs"][0]["results"]
            .as_array()
            .expect("results")
            .iter()
            .any(|result| result["ruleId"] == "hauksbee/run-error"),
        "the safe requested surface must not retain stale evidence: {doc}"
    );
}

#[test]
fn clap_usage_error_invalidates_requested_artifacts_before_exiting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let junit = dir.path().join("usage.xml");
    let sarif = dir.path().join("usage.sarif");
    std::fs::write(&junit, "stale green junit").expect("stale junit");
    std::fs::write(&sarif, "stale green sarif").expect("stale sarif");
    let out = run(&[
        "run",
        fixture("verdict_fet_bound.kicad_pcb").to_str().unwrap(),
        "--junit",
        junit.to_str().unwrap(),
        "--sarif",
        sarif.to_str().unwrap(),
        "--not-a-real-run-flag",
    ]);
    assert!(!out.status.success());
    let xml = std::fs::read_to_string(&junit).expect("junit replaced");
    assert!(
        junit_root(&xml).contains("errors=\"1\"") && xml.contains("not-a-real-run-flag"),
        "clap failure must replace stale JUnit:\n{xml}"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sarif).expect("sarif replaced"))
            .expect("valid SARIF");
    assert!(
        doc["runs"][0]["results"]
            .as_array()
            .expect("results")
            .iter()
            .any(|result| {
                result["ruleId"] == "hauksbee/run-error"
                    && result["properties"]["exit_code"] == out.status.code().unwrap()
            }),
        "clap failure must replace stale SARIF: {doc}"
    );
}

#[test]
fn clap_usage_error_never_invalidates_over_a_declared_run_input() {
    let dir = tempfile::tempdir().expect("tempdir");
    let firmware = dir.path().join("firmware.hex");
    let original = b":020000040000FA\n:00000001FF\n";
    std::fs::write(&firmware, original).expect("firmware fixture");
    let out = run(&[
        "run",
        fixture("verdict_fet_bound.kicad_pcb").to_str().unwrap(),
        "--firmware",
        firmware.to_str().unwrap(),
        "--junit",
        firmware.to_str().unwrap(),
        "--not-a-real-run-flag",
    ]);
    assert!(!out.status.success());
    assert_eq!(
        std::fs::read(&firmware).expect("firmware retained"),
        original,
        "parse-error cleanup must not overwrite a declared run input"
    );
}

#[test]
fn clap_error_for_a_non_run_command_never_mutates_run_artifact_flags() {
    let dir = tempfile::tempdir().expect("tempdir");
    let alleged_junit = dir.path().join("not-a-run-artifact.xml");
    let original = b"this file belongs to the caller, not to a run";
    std::fs::write(&alleged_junit, original).expect("seed caller file");

    // Here `run` is the positional BOARD argument to `to-code`, not the
    // selected top-level subcommand. `--junit` is invalid for `to-code` and
    // must not grant the parse-error path permission to mutate its value.
    let out = run(&["to-code", "run", "--junit", alleged_junit.to_str().unwrap()]);

    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert_eq!(
        std::fs::read(&alleged_junit).expect("caller file retained"),
        original,
        "only a proven top-level `run` command may invalidate run artifacts"
    );
}

#[test]
fn unknown_embedded_example_replaces_seeded_artifacts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let junit = dir.path().join("unknown-example.xml");
    let sarif = dir.path().join("unknown-example.sarif");
    std::fs::write(&junit, "stale green junit").expect("seed junit");
    std::fs::write(&sarif, "stale green sarif").expect("seed sarif");

    let out = run(&[
        "run",
        "--example",
        "definitely-not-an-example",
        "--junit",
        junit.to_str().unwrap(),
        "--sarif",
        sarif.to_str().unwrap(),
    ]);

    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    let xml = std::fs::read_to_string(&junit).expect("junit replaced");
    assert!(
        junit_root(&xml).contains("errors=\"1\"") && xml.contains("definitely-not-an-example"),
        "unknown-example resolution must finalize JUnit:\n{xml}"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sarif).expect("sarif replaced"))
            .expect("valid replacement SARIF");
    assert!(
        doc["runs"][0]["results"]
            .as_array()
            .expect("results")
            .iter()
            .any(|result| {
                result["ruleId"] == "hauksbee/run-error"
                    && result["properties"]["exit_code"] == 1
                    && result["message"]["text"]
                        .as_str()
                        .is_some_and(|message| message.contains("definitely-not-an-example"))
            }),
        "unknown-example resolution must finalize SARIF: {doc}"
    );
}

#[test]
fn static_report_and_headless_are_a_usage_error_without_surface_drift() {
    let dir = tempfile::tempdir().expect("tempdir");
    let junit = dir.path().join("conflicting-surface.xml");
    std::fs::write(&junit, "stale green junit").expect("seed junit");

    let out = run_in_actions(&[
        "run",
        fixture("verdict_fet_unbound.kicad_pcb").to_str().unwrap(),
        "--drc",
        "--headless",
        "--json",
        "--strict",
        "--junit",
        junit.to_str().unwrap(),
    ]);

    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("cannot be used with"),
        "Clap must reject contradictory selected surfaces: {}",
        stderr(&out)
    );
    let xml = std::fs::read_to_string(&junit).expect("junit invalidated");
    assert!(
        junit_root(&xml).contains("errors=\"1\"") && xml.contains("cannot be used with"),
        "usage failure must replace the stale artifact:\n{xml}"
    );
}

/// `--thermal` gates by default rather than under `--strict`, and its exit code
/// agrees with its verdict on both sides of that gate. Under the documented
/// `--no-strict-thermal` opt-out a refusing document deliberately exits 0, the
/// same situation as omitting `--strict` elsewhere. Pinned so nobody "fixes"
/// that split by silencing the document instead: the opt-out is about the exit
/// code, and the coverage is still partial either way.
#[test]
fn thermal_gates_by_default_and_the_opt_out_keeps_the_refusing_document() {
    let b = fixture("thermal_partial_coverage.kicad_pcb");
    let base = [
        "run",
        b.to_str().unwrap(),
        "--thermal",
        "--seconds",
        "0.05",
        "--json",
    ];
    let out = run(&base);
    assert_eq!(
        out.status.code(),
        Some(3),
        "the default thermal gate; stderr: {}",
        stderr(&out)
    );
    assert_eq!(
        json_verdict(&out).0,
        "invalid",
        "and the document says the same thing the exit code does"
    );

    let mut args = base.to_vec();
    args.push("--no-strict-thermal");
    let out = run(&args);
    assert_eq!(
        out.status.code(),
        Some(0),
        "the opt-out restores exit 0; stderr: {}",
        stderr(&out)
    );
    // The document is NOT rewritten to match: the coverage is still partial and
    // the evidence still undermined, so the machine verdict still refuses. The
    // user opted out of the exit code, not out of the truth.
    assert_eq!(
        json_verdict(&out).0,
        "invalid",
        "the opt-out must not silence the verdict, only the exit code"
    );
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    assert_eq!(v["thermal"]["coverage"]["partial"], true);
    assert!(
        v["notes"]
            .as_array()
            .expect("notes")
            .iter()
            .filter_map(|n| n["message"].as_str())
            .any(|m| m.starts_with("INCONCLUSIVE:")),
        "the caveat survives the opt-out:\n{v}"
    );
}

/// `ok` is true iff `verdict == "pass"`, on the refusal envelopes too. A board
/// with no component placement refuses before any surface renders, and that
/// hand-built envelope read `ok:true` beside `verdict:"invalid"` and exit 3, so
/// a pipeline gating on `ok` took a refusal for a clean run.
#[test]
fn the_placement_free_refusal_envelope_keeps_ok_iff_pass() {
    let dir = tempfile::tempdir().expect("tempdir");
    let board = dir.path().join("no_parts.kicad_pcb");
    std::fs::write(
        &board,
        "(kicad_pcb (version 20171130) (host pcbnew 5.1.0)\n  (net 0 \"\")\n  (net 1 \"GND\")\n  \
         (segment (start 0 0) (end 10 0) (width 0.25) (layer F.Cu) (net 1))\n)\n",
    )
    .expect("fixture written");
    let out = run(&["run", board.to_str().unwrap(), "--check", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "no placement means no part-level verdict; stderr: {}",
        stderr(&out)
    );
    let (verdict, ok) = json_verdict(&out);
    assert_eq!(verdict, "invalid");
    assert!(!ok, "ok must mirror the verdict word on the envelope too");
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    assert!(
        v["refusal"]["next_action"].is_string(),
        "the envelope still carries the structured refusal:\n{v}"
    );
}
