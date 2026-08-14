//! Validating a model in the editor must agree with saving it.
//!
//! The write-your-own panel calls `check` while someone types. If it ever
//! accepts something `save` refuses, the editor is worse than useless: it
//! teaches the author their model is fine and then loses their work at the
//! last step. So the two run the same checks, and this pins that.

use hauksbee_engine::webextract;

/// A minimal model that should pass everything.
const GOOD: &str = r#"
[[models]]
id = "test_r"
kind = "passive"
description = "a plain resistor"
[models.match]
value_re = "^10k$"
"#;

#[test]
fn a_valid_model_checks_clean_and_says_what_it_is() {
    let summary = webextract::check(GOOD).expect("the model is valid");
    assert!(
        summary.contains("test_r"),
        "the summary must name the part, so an author who typed the wrong id sees it: {summary}"
    );
}

#[test]
fn a_model_without_a_real_match_rule_is_refused_before_save() {
    let stale_editor_shape = r#"
[[models]]
id = "test_r"
kind = "passive"
[models.match]
value = ["^10k$"]
"#;
    let err =
        webextract::check(stale_editor_shape).expect_err("a model that cannot bind is invalid");
    assert!(err.contains("no match rules"), "{err}");
}

#[test]
fn broken_toml_is_reported_as_toml_not_as_a_model_problem() {
    let err = webextract::check("[[models]\nid = ").expect_err("must fail");
    assert!(
        err.contains("not valid model TOML"),
        "a syntax error is a syntax error, not a validation failure: {err}"
    );
}

#[test]
fn an_empty_editor_does_not_read_as_an_error() {
    // Someone who has typed nothing yet has not made a mistake.
    let err = webextract::check("   ").expect_err("nothing to check");
    assert!(err.contains("nothing to check"), "{err}");
}

#[test]
fn a_document_with_no_model_entry_says_so() {
    let err = webextract::check("# just a comment\n").expect_err("must fail");
    assert!(
        err.contains("no [[models]] entry"),
        "an author who has not written the entry yet needs to hear that: {err}"
    );
}

/// The contract that matters: check must not accept what save refuses.
#[test]
fn check_and_save_agree_about_what_is_invalid() {
    for bad in [
        "[[models]]\nid = \"x\"\n",         // no kind
        "[[models]]\nkind = \"passive\"\n", // no id
        "not toml at all",
    ] {
        let checked = webextract::check(bad);
        let saved = webextract::save("x", "passive", bad);
        assert!(
            checked.is_err(),
            "check accepted something save would refuse:\n{bad}"
        );
        assert!(saved.is_err(), "save unexpectedly accepted:\n{bad}");
    }
}

#[test]
fn every_entry_in_a_multi_model_draft_is_validated() {
    let second_entry_cannot_bind =
        format!("{GOOD}\n[[models]]\nid = \"hidden_bad_entry\"\nkind = \"passive\"\n");
    let checked = webextract::check(&second_entry_cannot_bind)
        .expect_err("an invalid later entry must not hide behind the valid first entry");
    let saved = webextract::save("two_models", "passive", &second_entry_cannot_bind)
        .expect_err("save must validate the same complete document");
    assert!(checked.contains("hidden_bad_entry"), "{checked}");
    assert!(saved.contains("hidden_bad_entry"), "{saved}");
}

#[test]
fn browser_check_and_save_reject_solver_invalid_behavioral_blocks() {
    let bad_converter = r#"
[[models]]
id = "bad_converter"
kind = "vreg"
[models.match]
value_re = "^BAD$"
[models.params]
vout = 3.3
dropout_v = 0.2
iq_a = 0.0001
[models.pins]
"1" = "in"
"2" = "out"
[models.behavioral.converter]
topology = "buck"
out_pin = "out"
in_pin = "in"
vout_setpoint = 0.0
efficiency = 0.9
"#;
    let checked = webextract::check(bad_converter)
        .expect_err("the browser must run the behavioral solver-safety gate");
    let saved = webextract::save("bad_converter", "vreg", bad_converter)
        .expect_err("save must run the same behavioral gate");
    assert!(
        checked.contains("behavioral") && checked.contains("vout_setpoint"),
        "{checked}"
    );
    assert!(
        saved.contains("behavioral") && saved.contains("vout_setpoint"),
        "{saved}"
    );
}

/// A pasted SPICE deck must be judged by the front end that would actually run
/// it. An earlier version asked a minimal card scanner instead and reported
/// that a `.subckt` would not simulate; the loader flattens subcircuits at
/// load, so that was false, and most vendor models ship as a subckt.
#[test]
fn a_subcircuit_loads_because_the_loader_flattens_it() {
    // The first line of a SPICE deck is its title and is always a comment.
    // Omitting it silently eats the first real card, which is the mistake
    // everyone makes once.
    let deck = "\
* divider
.subckt divider in out
R1 in out 1k
R2 out 0 1k
.ends
V1 vin 0 5
X1 vin mid divider
";
    let r = webextract::spice_report(deck).expect("a subckt deck loads");
    assert!(
        r.contains("loads this"),
        "a subcircuit is flattened and runs, so say so: {r}"
    );
}

#[test]
fn a_flat_deck_reports_what_the_solver_will_see() {
    let r = webextract::spice_report("* rc\nV1 in 0 5\nR1 in out 1k\nR2 out 0 1k\n")
        .expect("a flat deck loads");
    assert!(r.contains("device(s)"), "the count is the useful part: {r}");
}

#[test]
fn a_deck_the_loader_refuses_keeps_the_loader_own_words() {
    // The loader names the line and the directive. Rewording it would lose the
    // part that lets someone find the problem in their own file.
    let err =
        webextract::spice_report("* deck\nX1 a b nosuchsubckt\n").expect_err("undefined subckt");
    assert!(
        err.to_lowercase().contains("subckt") || err.contains("nosuchsubckt"),
        "the refusal must name what was wrong: {err}"
    );
}

#[test]
fn an_empty_spice_box_does_not_read_as_an_error() {
    let err = webextract::spice_report("  ").expect_err("nothing yet");
    assert!(err.contains("nothing to check"), "{err}");
}
