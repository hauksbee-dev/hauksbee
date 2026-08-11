//! Source-compatibility contract for downstream struct literals.

use hauksbee_extract::{DrcFinding, DrcReport, Item, ItemKind, ViolationKind};

#[test]
fn pre_net_tie_public_struct_literals_still_compile() {
    let finding = DrcFinding {
        kind: ViolationKind::Short,
        net_a: 1,
        net_b: 2,
        net_a_name: "GND".into(),
        net_b_name: "AGND".into(),
        layer: "Top".into(),
        x: 0.0,
        y: 0.0,
        gap_mm: -0.1,
        required_clearance_mm: 0.2,
        item_a: Item {
            kind: ItemKind::Track,
            net: 1,
            owner: String::new(),
        },
        item_b: Item {
            kind: ItemKind::Track,
            net: 2,
            owner: String::new(),
        },
    };
    let _report = DrcReport {
        clearance_mm: 0.2,
        findings: vec![finding],
        primitive_count: 2,
        version_warning: None,
        zone_pad_overlaps_suppressed: Some(0),
    };
}
