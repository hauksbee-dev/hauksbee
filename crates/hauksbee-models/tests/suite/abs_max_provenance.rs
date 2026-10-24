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
        none.contains("records no source for its 4.6 V absolute maximum"),
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

/// A card that prints a per-pin absolute maximum for a supply pin (a
/// characteristic row named `<ROLE>_MAX`, the datasheet's own symbol) must
/// enforce it on that pin. Without a `supply_range` row the pin is judged at
/// the card-wide `ratings.max_voltage_v`, which is the highest supply pin's
/// limit: a 1.89 V internal-regulator pin was judged at 6 V, and a design that
/// tied it to a 3.3 V rail passed.
#[test]
fn a_supply_pin_with_its_own_absolute_maximum_row_enforces_it() {
    use hauksbee_models::pin_roles::{power_pin, PowerKind};
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
            let Some(source) = &entry.source else {
                continue;
            };
            let roles: std::collections::BTreeSet<String> = entry
                .pins
                .values()
                .map(|r| r.to_ascii_lowercase())
                .collect();
            for row in &source.characteristic {
                let Some(max) = row.max else { continue };
                // The datasheet symbol is the first token: `VCC_MAX, ...`.
                let symbol = row
                    .parameter
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let Some(role) = symbol.strip_suffix("_max") else {
                    continue;
                };
                if !roles.contains(role)
                    || !power_pin(role, entry.kind).is_some_and(|p| p.kind == PowerKind::Supply)
                {
                    continue;
                }
                rows += 1;
                let enforced = entry.envelope.iter().any(|e| {
                    matches!(e, OperatingEnvelope::SupplyRange { pin, abs_max_v, max_v, .. }
                        if pin.eq_ignore_ascii_case(role)
                            && abs_max_v.unwrap_or(*max_v) <= max + 1e-9)
                });
                if !enforced {
                    offenders.push(format!(
                        "{} {role}: {} <= {max} V ({})",
                        entry.id, row.parameter, row.basis
                    ));
                }
            }
        }
    }
    assert!(rows > 0, "no <ROLE>_MAX supply rows were seen");
    assert!(
        offenders.is_empty(),
        "supply pins whose own absolute maximum no supply_range row enforces:\n  {}",
        offenders.join("\n  ")
    );
}
