//! Source-compatibility contract for the published structured DRC types.

use hauksbee_engine::result::{DrcShort, DrcStructured};

#[test]
fn pre_net_tie_public_struct_literals_still_compile() {
    let short = DrcShort {
        net_a: "GND".into(),
        net_b: "AGND".into(),
        layer: "Top".into(),
        gap_mm: -0.1,
        loc_mm: [0.0, 0.0],
        severity: "serious".into(),
        plain: "GND shorts AGND".into(),
        fix: "separate the copper".into(),
    };
    let _report = DrcStructured {
        clearance_rule_mm: 0.2,
        primitive_count: 2,
        shorts: vec![short],
        violations: Vec::new(),
        at_limit: Vec::new(),
        version_warning: None,
        suppression_note: None,
    };
}
