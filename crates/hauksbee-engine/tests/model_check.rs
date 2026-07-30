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
value = ["^10k$"]
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
