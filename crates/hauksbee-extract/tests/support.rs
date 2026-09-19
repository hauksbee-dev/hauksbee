//! Shared helpers for the extract crate's integration-test binary.

use std::path::PathBuf;

/// A file under this crate's `tests/fixtures/`.
#[allow(dead_code)]
pub fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel)
}

/// A short between nets `want_a` and `want_b` exists in `report`.
///
/// Shared by the KiCad and Eagle DRC suites: same `DrcReport` shape, same
/// "does a short between these two named nets exist" check.
#[allow(dead_code)]
pub fn assert_short(report: &hauksbee_extract::DrcReport, want_a: &str, want_b: &str) {
    let found = report.shorts().any(|f| {
        let names = [f.net_a_name.as_str(), f.net_b_name.as_str()];
        names.contains(&want_a) && names.contains(&want_b)
    });
    assert!(
        found,
        "expected a SHORT between {want_a} and {want_b}; got {:?}",
        report
            .findings
            .iter()
            .map(|f| (f.kind, f.net_a_name.clone(), f.net_b_name.clone(), f.gap_mm))
            .collect::<Vec<_>>()
    );
}

/// Assert `report`'s first short is a non-positive-gap overlap, optionally on
/// `layer` and optionally naming `owner` as one of the two items' owners.
///
/// Shared by the KiCad and Eagle "every copper kind overlapping foreign
/// copper is a short" sweeps: same `DrcReport` shape, same three checks.
#[allow(dead_code)]
pub fn assert_overlap_short(
    report: &hauksbee_extract::DrcReport,
    name: &str,
    layer: Option<&str>,
    owner: Option<&str>,
) {
    let f = report.shorts().next().unwrap();
    assert!(
        f.gap_mm <= 0.0,
        "{name}: overlap gap {} is non-positive",
        f.gap_mm
    );
    if let Some(layer) = layer {
        assert_eq!(f.layer, layer, "{name}: layer");
    }
    if let Some(owner) = owner {
        let owners = [f.item_a.owner.as_str(), f.item_b.owner.as_str()];
        assert!(
            owners.contains(&owner),
            "{name}: owner recorded: {owners:?}"
        );
    }
}
