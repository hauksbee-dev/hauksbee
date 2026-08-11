//! A netlist has no copper, so DRC must not report a clean bill on it.
//!
//! Found by running the flagship board in the way a newcomer would: upload it,
//! read the report. Every section came back healthy, including "no copper
//! spacing problems found" on a `.net` file that contains no copper at all.
//! The clearance sweep had examined zero primitives. A reader takes "healthy"
//! as "your copper is fine"; it was never looked at.
//!
//! This is the exact failure the project exists to prevent, on the default
//! path, so it gets a test rather than a comment.

use hauksbee_engine::result::DrcStructured;

/// A DRC result over input that carried no copper.
fn empty_drc() -> DrcStructured {
    DrcStructured {
        clearance_rule_mm: 0.0,
        primitive_count: 0,
        shorts: Vec::new(),
        violations: Vec::new(),
        at_limit: Vec::new(),
        version_warning: None,
        suppression_note: None,
    }
}

#[test]
fn a_drc_that_examined_nothing_does_not_read_as_healthy() {
    let report = hauksbee_engine::plain_drc_structured(&empty_drc());
    let text = report.render().to_lowercase();
    assert!(
        !text.contains("looks healthy"),
        "zero primitives examined must not render as a clean bill:\n{text}"
    );
    assert!(
        text.contains("no copper was checked"),
        "it has to say what was not checked:\n{text}"
    );
}

/// The premise: a DRC that DID examine copper and found nothing still reports
/// healthy. Without this the test above would pass on a change that broke every
/// clean verdict.
#[test]
fn a_real_clean_drc_still_reads_as_healthy() {
    let mut drc = empty_drc();
    drc.primitive_count = 1_252;
    drc.clearance_rule_mm = 0.2;
    let report = hauksbee_engine::plain_drc_structured(&drc);
    let text = report.render().to_lowercase();
    assert!(
        text.contains("healthy"),
        "a board with copper and no findings is genuinely clean:\n{text}"
    );
}
