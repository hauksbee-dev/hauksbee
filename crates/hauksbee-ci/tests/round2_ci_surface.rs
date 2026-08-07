//! Round-2 CI-surface contracts, the cheap (no co-sim) half:
//!
//! * `boot_coverage` is the canonical assertion kind and `boot-coverage` a
//!   silent alias, forever (a rename must never break a spec that was correct
//!   when written).
//! * Closed vocabularies reject a typo with a did-you-mean hint, not only a
//!   full-list dump.
//! * The GitHub annotation budget: no per-assertion `::notice` on passes,
//!   capped `::error`/`::warning` counts (GitHub truncates at 10/type/step),
//!   the rollup always present.
//! * Waived failures are visible-but-not-gating on every surface (JUnit
//!   `<skipped>`, exit code 0).
//! * The merged multi-spec JUnit document aggregates honest counts.

use std::path::PathBuf;
use std::time::Duration;

use hauksbee_ci::assertions::AssertResult;
use hauksbee_ci::report::{junit_error_suite, render_junit_document, CiResult};
use hauksbee_ci::{apply_waivers, waiver_notes, Spec};

fn write_spec(dir: &std::path::Path, name: &str, kind_line: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(
        &p,
        format!(
            // boot_coverage refuses to load without firmware (it would be a
            // hollow gate), so the helper stages one.
            "name = \"t\"\nboard = \"b.kicad_pcb\"\nfirmware = \"app.elf\"\nduration_ms = 10\n\n\
             [[assert]]\n{kind_line}\nnet = \"EN\"\nmin = 3.0\ndeadline_ms = 10.0\n"
        ),
    )
    .unwrap();
    p
}

#[test]
fn both_boot_coverage_spellings_load_and_normalize_to_the_canonical_kind() {
    let dir = tempfile::tempdir().unwrap();
    for spelling in ["kind = \"boot_coverage\"", "kind = \"boot-coverage\""] {
        let p = write_spec(dir.path(), "s.toml", spelling);
        let spec = Spec::load(&p).unwrap_or_else(|e| panic!("{spelling} must load: {e}"));
        assert_eq!(
            spec.asserts[0].kind, "boot_coverage",
            "the loader folds every accepted spelling onto the canonical one"
        );
        std::fs::remove_file(&p).unwrap();
    }
}

#[test]
fn an_unknown_assertion_kind_gets_a_did_you_mean_hint() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("s.toml");
    std::fs::write(
        &p,
        "name = \"t\"\nboard = \"b.kicad_pcb\"\nduration_ms = 10\n\n\
         [[assert]]\nkind = \"voltag\"\nnet = \"VCC\"\nmin = 3.0\n",
    )
    .unwrap();
    let err = Spec::load(&p).unwrap_err().to_string();
    assert!(
        err.contains("did you mean 'voltage'?"),
        "a one-edit typo must be pointed at the real kind, got: {err}"
    );
    assert!(
        err.contains("voltage|uart|toggle"),
        "the full list still follows the hint: {err}"
    );
    assert!(
        err.contains("boot_coverage") && !err.contains("boot-coverage"),
        "the kinds list teaches the canonical spelling: {err}"
    );
}

#[test]
fn an_unknown_supply_kind_gets_a_did_you_mean_hint() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("s.toml");
    std::fs::write(
        &p,
        "name = \"t\"\nboard = \"b.kicad_pcb\"\nduration_ms = 10\n\n\
         [[supply]]\nnet = \"VCC\"\nkind = \"benchh\"\nvolts = 5.0\n\n\
         [[assert]]\nkind = \"voltage\"\nnet = \"VCC\"\nmin = 3.0\n",
    )
    .unwrap();
    let err = Spec::load(&p).unwrap_err().to_string();
    assert!(
        err.contains("did you mean 'bench'?"),
        "supply-kind typos get the same treatment: {err}"
    );
}

#[test]
fn an_unknown_peripheral_type_gets_a_did_you_mean_hint() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("s.toml");
    std::fs::write(
        &p,
        "name = \"t\"\nboard = \"b.kicad_pcb\"\nduration_ms = 10\n\n\
         [[peripheral]]\nid = \"B1\"\ntype = \"pushbuton\"\nnet = \"BTN\"\n\n\
         [[assert]]\nkind = \"voltage\"\nnet = \"VCC\"\nmin = 3.0\n",
    )
    .unwrap();
    let err = Spec::load(&p).unwrap_err().to_string();
    assert!(
        err.contains("did you mean 'pushbutton'?"),
        "peripheral-type typos get a hint: {err}"
    );
}

// ── report-surface helpers ───────────────────────────────────────────────────

fn result_named(label: &str, passed: bool) -> AssertResult {
    AssertResult {
        label: label.to_string(),
        kind: "voltage".to_string(),
        passed,
        invalid: false,
        detail: format!("{label} detail"),
        failing_seed: None,
        failing_seeds: Vec::new(),
        seeds_total: 1,
        why: None,
        waived: None,
        subject_nets: vec!["+5V".to_string()],
        subject_refs: Vec::new(),
    }
}

fn ci_result(results: Vec<AssertResult>) -> CiResult {
    CiResult {
        spec_name: "t".into(),
        board: "b.kicad_pcb".into(),
        results,
        seeds: 1,
        elapsed: Duration::from_secs(0),
        analog_abort: false,
        coverage: None,
        substitutions: Vec::new(),
        coverage_warnings: Vec::new(),
        timing_coverage: Vec::new(),
        timing_refusals: Vec::new(),
        dead_rails: Vec::new(),
        waiver_notes: Vec::new(),
        inventory: Vec::new(),
        assumptions: Vec::new(),
        evidence: Vec::new(),
    }
}

#[test]
fn passing_assertions_emit_no_per_assertion_notice_annotations() {
    // GitHub truncates at 10 notices per step; a 12-assertion green spec would
    // burn the whole budget on PASS lines nobody acts on. Only the rollup
    // notice remains.
    let r = ci_result(
        (0..12)
            .map(|i| result_named(&format!("a{i}"), true))
            .collect(),
    );
    let gh = r.render_github_annotations();
    let notices = gh.matches("::notice").count();
    assert_eq!(notices, 1, "exactly the rollup notice: {gh}");
    assert!(
        gh.contains("12/12 assertions passed"),
        "the rollup still tells the story: {gh}"
    );
    assert!(!gh.contains("::error"), "a green run emits no errors: {gh}");
}

#[test]
fn error_annotations_are_capped_with_an_overflow_line_and_the_rollup() {
    let r = ci_result(
        (0..15)
            .map(|i| result_named(&format!("a{i}"), false))
            .collect(),
    );
    let gh = r.render_github_annotations();
    let errors = gh.matches("::error").count();
    // MAX_ERROR_ANNOTATIONS per-assertion + 1 overflow + 1 rollup = 10, which
    // is exactly GitHub's per-type truncation threshold.
    assert_eq!(
        errors,
        CiResult::MAX_ERROR_ANNOTATIONS + 2,
        "8 verdicts + overflow + rollup: {gh}"
    );
    assert!(
        gh.contains("...and 7 more failing assertion(s)"),
        "the overflow names how many were suppressed: {gh}"
    );
    assert!(
        gh.contains("hardware check RED"),
        "the rollup error survives the cap: {gh}"
    );
}

#[test]
fn warning_annotations_are_capped_with_an_overflow_line() {
    let mut r = ci_result(vec![result_named("a", true)]);
    r.coverage_warnings = (0..12).map(|i| format!("hole {i}")).collect();
    let gh = r.render_github_annotations();
    let warnings = gh.matches("::warning").count();
    assert_eq!(
        warnings,
        CiResult::MAX_WARNING_ANNOTATIONS + 1,
        "9 warnings + 1 overflow: {gh}"
    );
    assert!(
        gh.contains("...and 3 more warning(s)"),
        "the overflow names the suppressed count: {gh}"
    );
}

#[test]
fn a_waived_failure_is_visible_but_not_gating_on_every_surface() {
    let mut fail = result_named("rail holds", false);
    fail.waived = Some("fab-confirmed artifact (until 2030-01-01)".to_string());
    let r = ci_result(vec![result_named("banner prints", true), fail]);

    // Exit code: green, because the only failure is waived.
    assert_eq!(r.exit_code(), 0, "a waived failure must not gate");

    // Human report: still shows the failure, marked WAIVED, with the reason.
    let human = r.render_human();
    assert!(human.contains("[WAIVED] rail holds"), "{human}");
    assert!(human.contains("fab-confirmed artifact"), "{human}");
    assert!(human.contains("1 failure(s) waived"), "{human}");

    // JUnit: a <skipped> testcase, zero failures counted.
    let junit = r.render_junit();
    assert!(junit.contains("failures=\"0\""), "{junit}");
    assert!(junit.contains("skipped=\"1\""), "{junit}");
    assert!(junit.contains("<skipped message=\"waived FAIL:"), "{junit}");

    // GitHub: a warning, not an error; the rollup is the green notice.
    let gh = r.render_github_annotations();
    assert!(
        gh.contains("::warning title=hauksbee-ci WAIVED FAIL::"),
        "{gh}"
    );
    assert!(!gh.contains("::error"), "{gh}");

    // JSON: the per-result waived field rides along.
    assert!(r.render_json().contains("fab-confirmed artifact"));
}

#[test]
fn an_invalid_result_still_forces_exit_3_even_when_marked_up_by_nothing() {
    let mut invalid = result_named("window", false);
    invalid.invalid = true;
    let r = ci_result(vec![invalid]);
    assert_eq!(r.exit_code(), 3, "INVALID outranks everything");
}

#[test]
fn the_merged_junit_document_aggregates_counts_across_suites() {
    let green = ci_result(vec![result_named("a", true)]);
    let red = ci_result(vec![result_named("b", false), result_named("c", true)]);
    let doc = render_junit_document(&[
        green.junit_suite(),
        red.junit_suite(),
        junit_error_suite("ci/broken.toml", "no spec file at 'ci/broken.toml'"),
    ]);
    // One envelope, three suites, honest totals: 4 tests, 1 failure, 1 error.
    assert_eq!(doc.matches("<testsuite ").count(), 3, "{doc}");
    assert!(
        doc.contains("<testsuites name=\"hauksbee-ci\" tests=\"4\" failures=\"1\" errors=\"1\""),
        "{doc}"
    );
    assert!(doc.contains("ci/broken.toml"), "{doc}");
    // Exactly one XML prolog and one envelope: it is one document, not three
    // concatenated ones.
    assert_eq!(doc.matches("<?xml").count(), 1, "{doc}");
    assert_eq!(doc.matches("</testsuites>").count(), 1, "{doc}");
}

// ── waiver matching against a real waiver file ──────────────────────────────

fn waiver_file(dir: &std::path::Path, body: &str) -> PathBuf {
    let p = dir.join("hauksbee-waivers.toml");
    std::fs::write(&p, body).unwrap();
    p
}

const CI_WAIVER: &str = r#"
[[waive]]
check = "ci"
kind = "voltage"
nets = ["+5V"]
reason = "bench-verified; the model's ESR is pessimistic here"
until = "2030-01-01"
"#;

#[test]
fn an_active_ci_waiver_covers_the_matching_failure_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let p = waiver_file(dir.path(), CI_WAIVER);
    let mut waivers = hauksbee_engine::waiver::WaiverSet::load(&p).unwrap();

    let mut results = vec![
        result_named("+5V holds", false), // matches: kind voltage, net +5V
        {
            let mut r = result_named("3V3 holds", false);
            r.subject_nets = vec!["3V3".to_string()];
            r // different net: not covered
        },
        result_named("+5V settles", true), // a pass is never touched
    ];
    apply_waivers(&mut results, &mut waivers);

    assert!(results[0].waived.is_some(), "the named finding is covered");
    assert!(
        results[0]
            .waived
            .as_deref()
            .unwrap()
            .contains("until 2030-01-01"),
        "the mark carries the expiry: {:?}",
        results[0].waived
    );
    assert!(results[1].waived.is_none(), "a different net still gates");
    assert!(results[2].waived.is_none(), "passes are not marked");
    assert!(
        waiver_notes(&waivers).is_empty(),
        "a waiver that matched is neither stale nor lapsed"
    );
}

#[test]
fn an_expired_ci_waiver_gates_again_and_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let p = waiver_file(
        dir.path(),
        r#"
[[waive]]
check = "ci"
kind = "voltage"
nets = ["+5V"]
reason = "expired on purpose"
until = "2020-01-01"
"#,
    );
    let mut waivers = hauksbee_engine::waiver::WaiverSet::load(&p).unwrap();
    let mut results = vec![result_named("+5V holds", false)];
    apply_waivers(&mut results, &mut waivers);
    assert!(
        results[0].waived.is_none(),
        "the whole point of an expiry is that the finding comes back"
    );
    let notes = waiver_notes(&waivers);
    assert!(
        notes.iter().any(|n| n.contains("lapsed")),
        "the red is explainable: {notes:?}"
    );
}

#[test]
fn an_invalid_result_can_never_be_waived() {
    // A waiver overrules a finding; an INVALID is the absence of one. Letting
    // a waiver green an untrustworthy run would fail open.
    let dir = tempfile::tempdir().unwrap();
    let p = waiver_file(dir.path(), CI_WAIVER);
    let mut waivers = hauksbee_engine::waiver::WaiverSet::load(&p).unwrap();
    let mut invalid = result_named("+5V holds", false);
    invalid.invalid = true;
    let mut results = vec![invalid];
    apply_waivers(&mut results, &mut waivers);
    assert!(results[0].waived.is_none());
    let r = ci_result(results);
    assert_eq!(r.exit_code(), 3, "INVALID still refuses");
}

#[test]
fn a_stale_ci_waiver_is_reported_but_static_check_waivers_are_not_our_business() {
    let dir = tempfile::tempdir().unwrap();
    let p = waiver_file(
        dir.path(),
        r#"
[[waive]]
check = "ci"
kind = "voltage"
nets = ["+5V"]
reason = "matched nothing"
until = "2030-01-01"

[[waive]]
check = "si"
kind = "controlled_impedance"
nets = ["USB_DP"]
reason = "an SI waiver a CI run never consults"
until = "2030-01-01"
"#,
    );
    let waivers = hauksbee_engine::waiver::WaiverSet::load(&p).unwrap();
    let notes = waiver_notes(&waivers);
    assert_eq!(notes.len(), 1, "only the ci waiver is reported: {notes:?}");
    assert!(notes[0].contains("matched nothing"), "{notes:?}");
    assert!(
        !notes.iter().any(|n| n.contains("controlled_impedance")),
        "an si waiver is hauksbee run's business, not this run's: {notes:?}"
    );
}

// M6: every other way to get a waiver wrong refuses to load (no `reason`, an
// unparseable `until`, no `nets`/`refs`). A typo in `check` was the one that
// went through silently: it names no surface, so nothing ever matches it and
// no surface's stale accounting claimed it either. The user believes a finding
// is waived and it is not.
#[test]
fn a_waiver_naming_no_check_surface_is_reported_not_silently_inert() {
    let dir = tempfile::tempdir().unwrap();
    let p = waiver_file(
        dir.path(),
        r#"
[[waive]]
check = "cl"
kind = "voltage"
nets = ["+5V"]
reason = "meant ci, typed cl"
until = "2030-01-01"
"#,
    );
    let mut waivers = hauksbee_engine::waiver::WaiverSet::load(&p).unwrap();
    // It really is inert: the failure it was meant to cover still gates.
    let mut results = vec![result_named("+5V holds", false)];
    apply_waivers(&mut results, &mut waivers);
    assert!(results[0].waived.is_none());

    let notes = waiver_notes(&waivers);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("check = 'cl'"), "{notes:?}");
    assert!(notes[0].contains("can never match"), "{notes:?}");
    assert!(
        notes[0].contains("did you mean 'ci'?"),
        "the near miss is the whole diagnosis: {notes:?}"
    );
    assert!(notes[0].contains("ci, drc, lint, si"), "{notes:?}");
}

#[test]
fn a_real_static_check_surface_is_still_not_reported_here() {
    // The unknown-surface note must not swallow the deliberate filter: si /
    // drc / lint waivers belong to `hauksbee run`'s surfaces, and telling a CI
    // reader they matched nothing would be telling them to delete waivers this
    // run never consults.
    let dir = tempfile::tempdir().unwrap();
    for surface in ["si", "drc", "lint"] {
        let p = waiver_file(
            dir.path(),
            &format!(
                "\n[[waive]]\ncheck = \"{surface}\"\nkind = \"short\"\n\
                 nets = [\"USB_DP\"]\nreason = \"not this run's business\"\n\
                 until = \"2030-01-01\"\n"
            ),
        );
        let waivers = hauksbee_engine::waiver::WaiverSet::load(&p).unwrap();
        assert!(
            waiver_notes(&waivers).is_empty(),
            "a {surface} waiver must stay quiet here"
        );
    }
}

#[test]
fn the_boot_coverage_alias_matches_a_canonical_waiver() {
    // A spec still written with the old spelling normalizes at load, so its
    // failures carry kind "boot_coverage" and a waiver written canonically
    // covers them; nobody has to know the alias history to write a waiver.
    let dir = tempfile::tempdir().unwrap();
    let p = waiver_file(
        dir.path(),
        r#"
[[waive]]
check = "ci"
kind = "boot_coverage"
nets = ["EN"]
reason = "gate driven by an unmodelled supervisor"
until = "2030-01-01"
"#,
    );
    let mut waivers = hauksbee_engine::waiver::WaiverSet::load(&p).unwrap();
    let mut r = result_named("EN driven", false);
    r.kind = "boot_coverage".to_string();
    r.subject_nets = vec!["EN".to_string()];
    let mut results = vec![r];
    apply_waivers(&mut results, &mut waivers);
    assert!(results[0].waived.is_some());
}
