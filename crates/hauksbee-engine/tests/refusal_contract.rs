//! C5.3: one structured refusal contract shared by every report surface.
//!
//! A reason string is not enough: it tells a user why the tool stopped, but it
//! does not preserve the work that remains valid or give them the cheapest way
//! to make the claim answerable.  These tests define the four mandatory fields
//! before the implementation exists.

use hauksbee_engine::result::{Refusal, Validity};

#[test]
fn refusal_serializes_the_four_mandatory_answers_without_losing_partial_work() {
    let refusal = Refusal::new(
        "AC transfer response at V(out)",
        "an AC stimulus on the driving source",
        vec!["board extraction and component binding completed"],
        "add `AC 1` to V1, then rerun the same sweep",
    );
    let validity = Validity::refused(refusal.clone());
    let value = serde_json::to_value(validity).expect("validity serializes");

    assert_eq!(value["valid"], false);
    assert_eq!(value["reason"], "an AC stimulus on the driving source");
    assert_eq!(value["refusal"]["claim"], refusal.claim);
    assert_eq!(
        value["refusal"]["missing_prerequisite"],
        refusal.missing_prerequisite
    );
    assert_eq!(
        value["refusal"]["valid_partial_conclusions"][0],
        refusal.valid_partial_conclusions[0]
    );
    assert_eq!(value["refusal"]["next_action"], refusal.next_action);
}

#[test]
fn refusal_text_is_a_lossless_render_of_the_structured_contract() {
    let refusal = Refusal::new(
        "thermal safety conclusion",
        "a resolved dissipating model for U3",
        vec!["the board parsed", "copper checks remain valid"],
        "bind U3 with --models-dir, then rerun --thermal",
    );
    let text = refusal.render_text();

    for expected in [
        "refused claim: thermal safety conclusion",
        "missing prerequisite: a resolved dissipating model for U3",
        "valid partial conclusions: the board parsed; copper checks remain valid",
        "next action: bind U3 with --models-dir, then rerun --thermal",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
    }
}
