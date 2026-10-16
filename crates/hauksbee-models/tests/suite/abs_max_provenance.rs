//! A supply range's absolute maximum comes from a different table of the sheet
//! than its recommended window, so a finding that states it cites both.
//!
//! `basis` is the Recommended Operating Conditions row; `abs_max_basis` is the
//! Absolute Maximum Ratings row. These tests pin the helper that builds the
//! cited clause and the library-wide rule that no shipped card hands the
//! absolute-maximum figure a window-only citation.

use hauksbee_models::schema::DbFile;
use hauksbee_models::{abs_max_citation, basis_names_abs_max, OperatingEnvelope};

#[test]
fn a_window_only_basis_does_not_name_the_absolute_maximum_table() {
    // Faulty: the pre-fix TXB0101 basis cites section 5.3 alone.
    assert!(!basis_names_abs_max(
        "TI TXB0101 SCES639F, Section 5.3 Recommended Operating Conditions, VCCA supply-voltage row"
    ));
    assert!(!basis_names_abs_max(
        "Recommended Operating Conditions, VCC Supply Voltage row"
    ));
    // Controls: a basis that already carries the second table.
    for basis in [
        "TI TXB0102 SCES641E, 5.3 VCCA 1.2 V to 3.6 V; 5.1 VCCA -0.5 V to 4.6 V",
        "Microchip MIC5205 DS20005785A, Operating Ratings, input voltage VIN +2.5 V to +16 V; Absolute Maximum Ratings, VIN -20 V to +20 V",
        "NXP Limiting Values, VDD",
    ] {
        assert!(basis_names_abs_max(basis), "{basis}");
    }
}

#[test]
fn the_citation_carries_both_bases_or_says_it_has_one() {
    let roc = "TI TXB0101 SCES639F, Section 5.3 Recommended Operating Conditions, VCCA supply-voltage row";
    let amr = "TI TXB0101 SCES639F, Section 5.1 Absolute Maximum Ratings p.5, VCCA -0.5 V to 4.6 V";
    // Both rows recorded: both quoted, in that order.
    let both = abs_max_citation(roc, amr, 4.6);
    assert!(both.contains("Section 5.3"), "{both}");
    assert!(
        both.contains("absolute maximum basis: \"TI TXB0101 SCES639F, Section 5.1"),
        "{both}"
    );
    // A basis that already names its second table is quoted as it stands.
    let two = "TI TXB0102 SCES641E, 5.3 VCCA 1.2 V to 3.6 V; 5.1 VCCA -0.5 V to 4.6 V";
    assert_eq!(abs_max_citation(two, "", 4.6), format!("basis: \"{two}\""));
    // Neither: the figure is not presented as read from the window's table.
    let none = abs_max_citation(roc, "", 4.6);
    assert!(
        none.contains("records no source for its 4.60 V absolute maximum"),
        "{none}"
    );
}

/// Every shipped `supply_range` that states an `abs_max_v` carries a source for
/// it: an explicit `abs_max_basis`, or a `basis` that names the second table.
#[test]
fn every_shipped_absolute_maximum_has_its_own_source() {
    let db = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("db");
    let (mut offenders, mut rows) = (Vec::new(), 0);
    for path in std::fs::read_dir(db)
        .expect("db dir")
        .flatten()
        .map(|f| f.path())
    {
        let text = std::fs::read_to_string(&path).expect("db file");
        let Ok(file) = toml::from_str::<DbFile>(&text) else {
            continue;
        };
        for entry in &file.models {
            for envelope in &entry.envelope {
                if let OperatingEnvelope::SupplyRange {
                    pin,
                    abs_max_v: Some(limit),
                    basis,
                    abs_max_basis,
                    ..
                } = envelope
                {
                    rows += 1;
                    if abs_max_basis.trim().is_empty() && !basis_names_abs_max(basis) {
                        offenders.push(format!("{} {pin} {limit} V: {basis}", entry.id));
                    }
                }
            }
        }
    }
    assert!(rows > 100, "only {rows} rows with an abs_max_v were seen");
    assert!(
        offenders.is_empty(),
        "absolute maxima that cite only the recommended window:\n  {}",
        offenders.join("\n  ")
    );
}
