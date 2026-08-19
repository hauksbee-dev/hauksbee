use hauksbee_extract::{
    clearance_rules_from_kicad_pro, parse_kicad_dru, ExtractedBoard, KicadDruConstraintKind,
};

const DOORBELL_DRU: &str =
    include_str!("../../../qc/blind_trials/work/doorbell/kicad/doorbell.kicad_dru");
const PRECEDENCE_BOARD: &str = include_str!("fixtures/kicad_dru_precedence.kicad_pcb");
const PRECEDENCE_PROJECT: &str = include_str!("fixtures/kicad_dru_precedence.kicad_pro");
const PRECEDENCE_DRU: &str = include_str!("fixtures/kicad_dru_precedence.kicad_dru");
const PRECEDENCE_RESTRICTIVE_LAST_DRU: &str =
    include_str!("fixtures/kicad_dru_precedence_restrictive_last.kicad_dru");
const BARE_SCOPE_DRU: &str = include_str!("fixtures/kicad_dru_bare_scope.kicad_dru");

#[test]
fn parses_doorbells_actual_custom_rules() {
    let parsed = parse_kicad_dru(DOORBELL_DRU).expect("doorbell DRU parses");
    assert_eq!(parsed.version, 1);
    assert_eq!(parsed.rules.len(), 4);
    assert_eq!(parsed.global_clearance_mm(), Some(0.127));
    assert!(parsed.unsupported_constraint_counts.is_empty());

    let conditional = parsed
        .rules
        .iter()
        .find(|rule| rule.name.starts_with("PTH hole-to-copper"))
        .expect("PTH rule present");
    assert_eq!(
        conditional.condition.as_deref(),
        Some("A.Pad_Type == 'Through Hole Pad'")
    );
    assert!(conditional
        .constraints
        .iter()
        .any(|constraint| constraint.kind == KicadDruConstraintKind::HoleClearance));
}

#[test]
fn converts_explicit_mil_inch_and_mm_units() {
    let parsed = parse_kicad_dru(
        r#"(version 1)
        (rule "mil" (constraint clearance (min 5mil)))
        (rule "inch" (constraint hole_clearance (min 0.01in)))
        (rule "mm" (constraint edge_clearance (min 0.3mm)))"#,
    )
    .expect("all explicit distance units parse");
    assert!((parsed.rules[0].constraints[0].min_mm.unwrap() - 0.127).abs() < 1e-12);
    assert!((parsed.rules[1].constraints[0].min_mm.unwrap() - 0.254).abs() < 1e-12);
    assert!((parsed.rules[2].constraints[0].min_mm.unwrap() - 0.3).abs() < 1e-12);
    assert!(parsed.rules.iter().all(|rule| !rule.has_bare_value()));
}

#[test]
fn bare_value_rule_is_retained_with_line_and_type_but_not_applied() {
    let parsed = parse_kicad_dru(BARE_SCOPE_DRU).expect("scope-probe DRU parses");
    let bare = &parsed.rules[0];
    assert_eq!(bare.name, "bare rule");
    assert_eq!(bare.line_number, 3);
    assert_eq!(
        bare.bare_value_constraint_types(),
        vec![KicadDruConstraintKind::Clearance]
    );
    assert!(bare.has_bare_value());
    assert!(!parsed.rules[1].has_bare_value());
    assert!(parsed.has_bare_values());
    assert_eq!(parsed.global_clearance_mm(), None);
    assert_eq!(
        parsed
            .unevaluated_rules()
            .map(|rule| rule.name.as_str())
            .collect::<Vec<_>>(),
        vec!["bare rule", "mm rule"]
    );
}

#[test]
fn retains_condition_layer_severity_and_constraint_bounds() {
    let parsed = parse_kicad_dru(
        r#"(version 1)
        (rule "scoped"
          (condition "A.NetClass == 'Power'")
          (layer "F.Cu")
          (severity warning)
          (constraint clearance (min 0.2mm) (opt 0.25mm) (max 0.3mm)))"#,
    )
    .expect("scoped rule parses");
    let rule = &parsed.rules[0];
    assert_eq!(rule.condition.as_deref(), Some("A.NetClass == 'Power'"));
    assert_eq!(rule.layer.as_deref(), Some("F.Cu"));
    assert_eq!(rule.severity.as_deref(), Some("warning"));
    assert_eq!(rule.constraints[0].min_mm, Some(0.2));
    assert_eq!(rule.constraints[0].opt_mm, Some(0.25));
    assert_eq!(rule.constraints[0].max_mm, Some(0.3));
}

#[test]
fn counts_recognised_but_unimplemented_constraints() {
    let parsed = parse_kicad_dru(
        r#"(version 1)
        (rule "widths" (constraint track_width (min 0.1mm)))
        (rule "lengths" (constraint length (max 20mm)))
        (rule "widths again" (constraint track_width (min 0.2mm)))"#,
    )
    .expect("known unimplemented constraints remain visible");
    assert_eq!(parsed.unsupported_constraint_counts["track_width"], 2);
    assert_eq!(parsed.unsupported_constraint_counts["length"], 1);
}

#[test]
fn unknown_constraint_type_fails_closed() {
    let error = parse_kicad_dru(
        r#"(version 1)
        (rule "future grammar" (constraint copper_magic (min 0.1mm)))"#,
    )
    .expect_err("an unknown constraint cannot be silently dropped");
    assert!(error
        .to_string()
        .contains("unknown constraint type copper_magic"));
}

#[test]
fn malformed_file_and_unknown_version_fail_closed() {
    let malformed =
        parse_kicad_dru("(version 1) (rule \"broken\"").expect_err("an unclosed rule must fail");
    assert!(malformed.to_string().contains("parse"));

    let version =
        parse_kicad_dru("(version 99)").expect_err("an unrecognised grammar version must fail");
    assert!(version
        .to_string()
        .contains("unsupported .kicad_dru version 99"));
}

#[test]
fn last_matching_global_rule_wins_when_the_looser_rule_is_last() {
    let parsed = parse_kicad_dru(PRECEDENCE_DRU).expect("precedence DRU parses");
    assert_eq!(parsed.global_clearance_mm(), Some(0.15));

    let project_rules = clearance_rules_from_kicad_pro(PRECEDENCE_PROJECT, ["A", "B"])
        .expect("project netclass parses");
    let project_only =
        ExtractedBoard::drc_with_clearance_rules(PRECEDENCE_BOARD, Some(project_rules.clone()))
            .expect("project-only DRC runs");

    let mut overridden = project_rules;
    overridden.apply_global_clearance_override(parsed.global_clearance_mm().unwrap());
    let with_dru = ExtractedBoard::drc_with_clearance_rules(PRECEDENCE_BOARD, Some(overridden))
        .expect("DRU-backed DRC runs");

    assert_eq!(project_only.clearance_violations().count(), 0);
    assert_eq!(with_dru.clearance_violations().count(), 0);
}

#[test]
fn a_restrictive_last_rule_tightens_clearance_and_produces_more_findings() {
    let parsed = parse_kicad_dru(PRECEDENCE_RESTRICTIVE_LAST_DRU)
        .expect("restrictive-last precedence DRU parses");
    assert_eq!(parsed.global_clearance_mm(), Some(0.25));

    let project_rules = clearance_rules_from_kicad_pro(PRECEDENCE_PROJECT, ["A", "B"])
        .expect("project netclass parses");
    let project_only =
        ExtractedBoard::drc_with_clearance_rules(PRECEDENCE_BOARD, Some(project_rules.clone()))
            .expect("project-only DRC runs");

    let mut tightened = project_rules;
    tightened.apply_global_clearance_override(parsed.global_clearance_mm().unwrap());
    let with_dru = ExtractedBoard::drc_with_clearance_rules(PRECEDENCE_BOARD, Some(tightened))
        .expect("DRU-backed DRC runs");

    let project_count = project_only.clearance_violations().count();
    let custom_count = with_dru.clearance_violations().count();
    assert_eq!(project_count, 0);
    assert_eq!(custom_count, 1);
    assert!(custom_count > project_count);
}
