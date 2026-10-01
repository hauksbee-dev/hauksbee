//! A card's pin map is for one package; bound to another footprint it must be
//! refused, not counted as bound.
//!
//! A drafted card read off the wrong package column binds cleanly: supply and
//! ground roles land on GPIO pads, nothing complains, and the MCU counts as
//! bound. The fake MCU here prints two columns (an 8-pin QFN and a 10-pin SOP)
//! and maps the QFN's. On a QFN footprint it binds; on the SOP footprint the
//! binder and the coverage report both refuse it, say why, and leave the part
//! unmodelled.

use crate::support::{component, net, pin};
use hauksbee_engine::bind_report::BindSummary;
use hauksbee_engine::binder::bind_board;
use hauksbee_engine::commands::models::{coverage_snapshot, CoverageStage};
use hauksbee_engine::reports::bind::BindOutcome;
use hauksbee_extract::{Component, ExtractedBoard};
use hauksbee_models::ModelLibrary;

const CARD: &str = r#"
[[models]]
id = "fake_two_column_mcu"
kind = "mcu"
description = "fake 8-pin MCU, QFN-8 package"
[models.match]
value_re = "(?i)^FAKEMCU8$"
[models.params]
backend = "renode:stm32f103"
[models.pins]
"1" = "vdd"
"2" = "pa0"
"3" = "pa1"
"4" = "pa2"
"5" = "pa3"
"6" = "nrst"
"7" = "pa4"
"8" = "vss"
[models.source]
tier = "datasheet-derived"
[[models.source.pinout]]
package = "QFN-8"
pins = { "1" = "VDD", "2" = "PA0", "3" = "PA1", "4" = "PA2", "5" = "PA3", "6" = "NRST", "7" = "PA4", "8" = "VSS" }
[[models.source.pinout]]
package = "SOP-10"
pins = { "1" = "PA0", "2" = "VDD", "3" = "PA1", "4" = "PA2", "5" = "PA3", "6" = "NRST", "7" = "PA4", "8" = "PA5", "9" = "VSS", "10" = "PA6" }
"#;

/// The MCU on `footprint`, wired as that package's own column says: VDD and
/// ground where the board's package puts them, every other pad on a GPIO net.
fn board(footprint: &str, pads: u32, vdd: u32, grounds: &[u32]) -> ExtractedBoard {
    let net_of = |n: u32| match n {
        n if n == vdd => 1,
        n if grounds.contains(&n) => 2,
        n => 10 + i64::from(n),
    };
    let pins = (1..=pads)
        .map(|n| pin(&n.to_string(), Some(net_of(n)), "", ""))
        .collect();
    let mcu = Component {
        position: Some((10.0, 10.0, 0.0)),
        ..component(
            "U1".into(),
            "FAKEMCU8".into(),
            String::new(),
            footprint.into(),
            pins,
        )
    };
    let mut nets = vec![net(1, "+3V3".into()), net(2, "GND".into())];
    nets.extend((1..=pads).map(|n| net(10 + i64::from(n), format!("/IO{n}"))));
    ExtractedBoard {
        name: "package-fit".into(),
        nets,
        components: vec![mcu],
    }
}

#[test]
fn a_two_column_card_binds_on_its_own_package_and_is_refused_on_the_other() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("fake_mcu.toml"), CARD).unwrap();
    let mut lib = ModelLibrary::builtin();
    let errors = lib.load_models_dir(dir.path());
    assert!(errors.is_empty(), "the card loads: {errors:?}");

    // Its own package: the QFN-8 with an exposed pad, grounded.
    let own = board("Package_DFN_QFN:QFN-8-1EP_3x3mm_P0.65mm", 9, 1, &[8, 9]);
    let bound = bind_board(&own, &lib);
    let summary = BindSummary::from_report(&bound.report);
    assert!(summary.mcu_bound, "{:?}", bound.report.rows);
    assert_eq!(summary.critical_parts_bound_n, 1);

    // The other column's package: a SOP-10 wired VDD 2, ground 9.
    let other = board("Package_SO:SOP-10_3.9x4.9mm_P1.0mm", 10, 2, &[9]);
    let bound = bind_board(&other, &lib);
    let row = bound
        .report
        .rows
        .iter()
        .find(|r| r.reference == "U1")
        .unwrap();
    let BindOutcome::Unresolved { reason } = &row.outcome else {
        panic!("a QFN-8 map on a SOP-10 must not bind: {:?}", row.outcome);
    };
    assert!(
        reason.contains("package mismatch")
            && reason.contains("QFN-8")
            && reason.contains("SOP-10"),
        "{reason}"
    );
    assert!(row
        .warning
        .as_deref()
        .is_some_and(|w| w.contains("package mismatch")));
    let summary = BindSummary::from_report(&bound.report);
    assert!(!summary.mcu_bound && bound.mcus.is_empty());
    assert_eq!(summary.critical_parts_bound_n, 0, "never counted as bound");
    let open = summary
        .active_path_unresolved
        .iter()
        .find(|u| u.reference == "U1");
    assert!(
        open.is_some_and(|u| u.reason.contains("package mismatch")),
        "{summary:?}"
    );

    // Coverage agrees: unresolved (so the batch re-drafts it), with the reason.
    let coverage = coverage_snapshot(&other, &lib);
    let row = coverage
        .components
        .iter()
        .find(|r| r.reference == "U1")
        .unwrap();
    assert_eq!(row.stage, CoverageStage::Unresolved);
    assert!(
        row.refused.as_deref().is_some_and(|r| r.contains("SOP-10")),
        "{row:?}"
    );
    let own_row = coverage_snapshot(&own, &lib)
        .components
        .into_iter()
        .find(|r| r.reference == "U1");
    assert!(own_row.is_some_and(|r| r.refused.is_none() && r.stage != CoverageStage::Unresolved));
}

/// Same outline and pin count, wrong column: the card's ground pad sits on a
/// GPIO net while the board grounds a pad the card leaves unmapped.
#[test]
fn a_map_whose_ground_lands_on_a_signal_is_refused_on_a_same_size_footprint() {
    let dir = tempfile::tempdir().unwrap();
    // Read off a column whose ground is pad 3: the board's ground is 8 and the pad.
    let card = CARD
        .replace("\"3\" = \"pa1\"", "\"3\" = \"vss\"")
        .replace("\"8\" = \"vss\"", "\"8\" = \"unknown_pad_8\"");
    let card = card
        .replace("\"3\" = \"PA1\"", "\"3\" = \"VSS\"")
        .replace("\"8\" = \"VSS\" }", "\"8\" = \"unknown_pad_8\" }");
    std::fs::write(dir.path().join("fake_mcu.toml"), card).unwrap();
    let mut lib = ModelLibrary::builtin();
    let errors = lib.load_models_dir(dir.path());
    assert!(errors.is_empty(), "the card loads: {errors:?}");
    let bound = bind_board(
        &board("Package_DFN_QFN:QFN-8-1EP_3x3mm_P0.65mm", 9, 1, &[8, 9]),
        &lib,
    );
    let row = bound
        .report
        .rows
        .iter()
        .find(|r| r.reference == "U1")
        .unwrap();
    assert!(
        matches!(&row.outcome, BindOutcome::Unresolved { reason } if reason.contains("puts ground on pad(s) 3")),
        "{:?}",
        row.outcome
    );
    assert!(bound.mcus.is_empty());
}
