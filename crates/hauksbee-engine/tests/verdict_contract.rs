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
//!    `invalid` exits 3, `fail` exits 2, `pass` exits 0. The gate is per
//!    surface, so the bind gate that invalidates `--lint`/`--si`/`--check`/
//!    `--usb-c` leaves the copper (`--drc`) and descriptive (`--report`)
//!    surfaces alone, on both the exit code and the verdict field. The CI
//!    artifacts (JUnit, SARIF, GitHub annotations) carry the same verdict,
//!    including through the co-sim rewrite and its refusal rewrite.

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

/// A board carrying two real copper shorts, so the copper surface has a
/// verdict of its own to agree with.
fn shorted_board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-ci/examples/boards/boot_gate.kicad_pcb")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
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
    let root = xml.lines().nth(1).unwrap_or_default();
    assert!(
        !root.contains("failures=\"0\"") || !root.contains("errors=\"0\""),
        "the JUnit root must not read all-green beside an invalid JSON verdict: {root}"
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

/// Assert one surface's strict exit code and its own JSON verdict tell the
/// same story on one board: `invalid` gates with [`EXIT_INVALID`], `fail` with
/// the findings code 2, `pass` with 0.
fn assert_gate_matches_verdict(surface: &str, board: &Path, want_verdict: &str) {
    let b = board.to_str().unwrap();
    let json = run(&["run", b, surface, "--json"]);
    let (verdict, ok) = json_verdict(&json);
    assert_eq!(
        verdict,
        want_verdict,
        "{surface} on {b} must read {want_verdict}; stderr: {}",
        stderr(&json)
    );
    assert_eq!(ok, verdict == "pass", "ok must mirror the verdict word");
    // Without --strict every surface stays exit 0: the verdict is a document
    // field, and turning it into an exit code is what --strict is for.
    assert_eq!(
        json.status.code(),
        Some(0),
        "{surface} must not gate without --strict; stderr: {}",
        stderr(&json)
    );
    let want_code = match want_verdict {
        "invalid" => 3,
        "fail" => 2,
        _ => 0,
    };
    let strict = run(&["run", b, surface, "--strict"]);
    assert_eq!(
        strict.status.code(),
        Some(want_code),
        "{surface} --strict must exit {want_code} to match its own '{want_verdict}' verdict; \
         stderr: {}",
        stderr(&strict)
    );
}

#[test]
fn lint_strict_exit_agrees_with_the_lint_verdict_on_both_sides_of_the_bind_gate() {
    assert_gate_matches_verdict(
        "--lint",
        &fixture("verdict_fet_unbound.kicad_pcb"),
        "invalid",
    );
    assert_gate_matches_verdict("--lint", &fixture("verdict_fet_bound.kicad_pcb"), "pass");
}

#[test]
fn si_strict_exit_agrees_with_the_si_verdict_on_both_sides_of_the_bind_gate() {
    assert_gate_matches_verdict("--si", &fixture("verdict_fet_unbound.kicad_pcb"), "invalid");
    assert_gate_matches_verdict("--si", &fixture("verdict_fet_bound.kicad_pcb"), "pass");
}

#[test]
fn check_strict_exit_agrees_with_the_check_verdict_on_both_sides_of_the_bind_gate() {
    assert_gate_matches_verdict(
        "--check",
        &fixture("verdict_fet_unbound.kicad_pcb"),
        "invalid",
    );
    assert_gate_matches_verdict("--check", &fixture("verdict_fet_bound.kicad_pcb"), "pass");
}

#[test]
fn drc_strict_exit_agrees_with_the_copper_verdict_the_bind_gate_never_touches() {
    // The exemption, both halves: the unbound FET that invalidates --lint/--si
    // above leaves --drc at `pass` AND at exit 0 under --strict, because copper
    // spacing owes nothing to device models...
    assert_gate_matches_verdict("--drc", &fixture("verdict_fet_unbound.kicad_pcb"), "pass");
    // ...and that exit 0 is not a dead gate: a board with real shorts reads
    // `fail` and exits 2 on the same surface.
    assert_gate_matches_verdict("--drc", &shorted_board(), "fail");
}

#[test]
fn usb_c_strict_exit_agrees_with_its_cc_scoped_verdict() {
    // The CC verdict rests on the identity of the parts on the receptacle's CC
    // nets, so the bind gate is scoped to them. An unbound FET sitting on CC1
    // invalidates the surface and exits 3 under --strict...
    assert_gate_matches_verdict(
        "--usb-c",
        &fixture("verdict_usb_c_cc_fet_unbound.kicad_pcb"),
        "invalid",
    );
    // ...while the same unbound FET class on a board with no receptacle at all
    // leaves nothing for the CC claim to be inconclusive about: `pass`, exit 0.
    assert_gate_matches_verdict("--usb-c", &fixture("verdict_fet_unbound.kicad_pcb"), "pass");
}

#[test]
fn the_report_surface_describes_the_binding_and_never_gates_on_it() {
    // `--report` is a description of what was modelled, not a pass/fail check,
    // so --strict does not apply to it (reports/bind.rs). Its document still
    // carries the honest `invalid` when binding is incomplete, because
    // incomplete binding is precisely this report's subject; what it must never
    // do is turn that into a failing build.
    let unbound = fixture("verdict_fet_unbound.kicad_pcb");
    let out = run(&["run", unbound.to_str().unwrap(), "--report", "--json"]);
    let (verdict, ok) = json_verdict(&out);
    assert_eq!(verdict, "invalid", "stderr: {}", stderr(&out));
    assert!(!ok);
    // The route matters: `--report` keeps the per-net binding-completeness maps
    // the specialist surfaces trim, and those are what make it inconclusive.
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    assert!(
        v["evidence"].as_array().expect("evidence").iter().any(|m| {
            m["status"] == "undermined"
                && m["assertion"]
                    .as_str()
                    .is_some_and(|a| a.starts_with("Binding completeness"))
        }),
        "the descriptive report says which net it could not bind:\n{v}"
    );
    for extra in [None, Some("--strict")] {
        let mut args = vec!["run", unbound.to_str().unwrap(), "--report"];
        if let Some(f) = extra {
            args.push(f);
        }
        let out = run(&args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "--report never gates ({extra:?}); stderr: {}",
            stderr(&out)
        );
    }
    // Bound twin: the description has nothing left to qualify.
    let bound = fixture("verdict_fet_bound.kicad_pcb");
    let out = run(&[
        "run",
        bound.to_str().unwrap(),
        "--report",
        "--json",
        "--strict",
    ]);
    assert_eq!(json_verdict(&out).0, "pass", "stderr: {}", stderr(&out));
    assert_eq!(out.status.code(), Some(0));
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
    // --junit/--sarif are written from the full static suite whatever selector
    // asked for them, so a specialist surface must not be able to hand CI a
    // green artifact beside its own invalid JSON verdict.
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
    // The third artifact surface, same rule: an error annotation appears with
    // the invalid verdict and nowhere else, so a pull request's checks tab
    // cannot read clean beside a refused document.
    let unbound = fixture("verdict_fet_unbound.kicad_pcb");
    let dir = tempfile::tempdir().expect("tempdir");
    let junit = dir.path().join("out.xml");
    let out = run_in_actions(&[
        "run",
        unbound.to_str().unwrap(),
        "--lint",
        "--json",
        "--junit",
        junit.to_str().unwrap(),
    ]);
    let err = stderr(&out);
    assert!(
        err.lines()
            .any(|l| l.starts_with("::error ") && l.contains("Q1") && l.contains("INCONCLUSIVE:")),
        "an error annotation names the bind blocker in the shared sentence:\n{err}"
    );

    let bound = fixture("verdict_fet_bound.kicad_pcb");
    let junit = dir.path().join("bound.xml");
    let out = run_in_actions(&[
        "run",
        bound.to_str().unwrap(),
        "--lint",
        "--json",
        "--junit",
        junit.to_str().unwrap(),
    ]);
    let err = stderr(&out);
    assert!(
        !err.lines().any(|l| l.starts_with("::error ")),
        "no error annotation on the bound twin:\n{err}"
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
    let board =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cosim_fault_led.kicad_pcb");
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
    let out = run(&[
        "run",
        board.to_str().unwrap(),
        "--headless",
        "--seconds",
        "0.05",
        "--strict",
        "--junit",
        refused.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "a strict co-sim that saw zero activity refuses with the invalid code; stderr: {}",
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
}
