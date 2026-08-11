//! Downstream source-compatibility contract for the public finding shape.
//!
//! This intentionally constructs `JsonFinding` with the fields available in
//! schema-v2 releases before the CI-artifact gating work. Adding a required
//! public field makes this integration test fail to compile, exactly as it
//! would for downstream Rust users with existing struct literals.

use hauksbee_engine::result::JsonFinding;

#[test]
fn existing_struct_literal_still_compiles_and_serializes_the_additive_gate_grade() {
    let finding = JsonFinding {
        check: "lint".into(),
        kind: "placeholder_value".into(),
        severity: "warning".into(),
        nets: vec!["+5V".into()],
        location_mm: None,
        layer: None,
        refs: vec!["R1".into()],
        actionable: true,
        message: "placeholder value".into(),
        plain: "placeholder value".into(),
        fix: Some("set the fitted value".into()),
    };

    let wire = serde_json::to_value(&finding).expect("finding serializes");
    assert_eq!(wire["gating"], true, "medium lint findings gate");
}
