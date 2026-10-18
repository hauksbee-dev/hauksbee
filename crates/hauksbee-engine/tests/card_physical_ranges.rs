//! A card value outside its physical range is a unit slip, never a requirement.
//!
//! Reproduces the measured case: an LLM-extracted LDO card carried
//! `cout_min_f = 1.0` (the datasheet's 1 uF, written in microfarads into a farad
//! field). Validation accepted it, and the output-capacitor check then demanded
//! "at least 999,985.8 uF" of the board. Now `models lint` and the extraction
//! validator refuse the card, and a card that reaches the engine anyway (a built-in
//! layer skips validation) is read without the value and marked suspect.

use hauksbee_engine::binder::bind_board;
use hauksbee_engine::checks::regulator_cout::regulator_cout_lint;
use hauksbee_engine::reports::bind::BindOutcome;
use hauksbee_extract::{Component, ExtractedBoard};
use hauksbee_models::{ModelLibrary, SourceLayer};

fn card(cout: &str) -> String {
    format!(
        "[[models]]\nid = \"zz_ldo_slip\"\nkind = \"vreg\"\n\
         description = \"hand-built LDO for the range regression\"\n\
         [models.match]\nvalue_re = \"^ZZLDOSLIP$\"\n\
         [models.params]\nvout = 3.3\ndropout_v = 0.3\niq_a = 5e-5\ncout_min_f = {cout}\n\
         [models.pins]\n\"1\" = \"gnd\"\n\"2\" = \"out\"\n\"3\" = \"in\"\n"
    )
}

/// The card in a library, loaded at the layer that skips validation.
fn library_with(cout: &str) -> (ModelLibrary, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("zz_ldo_slip.toml"), card(cout)).expect("write card");
    let mut lib = ModelLibrary::builtin();
    let errors = lib.load_dir_layer(dir.path(), SourceLayer::Builtin);
    assert!(errors.is_empty(), "{errors:?}");
    (lib, dir)
}

/// U1 from +5V to +3V3 with one 10 uF capacitor across the output.
fn board() -> ExtractedBoard {
    let pin = |n: &str, net: i64| crate::support::pin(n, Some(net), "", "passive");
    let part = |r: &str, v: &str, pins| {
        crate::support::component(r.into(), v.into(), String::new(), String::new(), pins)
    };
    let comps: Vec<Component> = vec![
        part(
            "U1",
            "ZZLDOSLIP",
            vec![pin("1", 3), pin("2", 2), pin("3", 1)],
        ),
        part("C1", "10u", vec![pin("1", 2), pin("2", 3)]),
    ];
    ExtractedBoard {
        name: "card_physical_ranges".into(),
        nets: [(1, "+5V"), (2, "+3V3"), (3, "GND")]
            .into_iter()
            .map(|(i, n)| crate::support::net(i, n.to_string()))
            .collect(),
        components: comps,
    }
}

#[test]
fn validation_and_lint_refuse_the_one_farad_card() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = dir.path().join("slip.toml");
    std::fs::write(&file, card("1.0")).unwrap();
    let report = hauksbee_engine::commands::models::lint_report(&file).expect("lint runs");
    assert!(report.findings >= 1, "{:?}", report.lines);
    let text = report.lines.join("\n");
    for needle in [
        "ERROR",
        "param 'cout_min_f'",
        "100 pF to 10 mF",
        "1e-6x gives 1 uF",
    ] {
        assert!(text.contains(needle), "{needle:?} in {text}");
    }
    // The same card written in farads is clean.
    std::fs::write(&file, card("1.0e-6")).unwrap();
    let ok = hauksbee_engine::commands::models::lint_report(&file).expect("lint runs");
    assert_eq!(ok.findings, 0, "{:?}", ok.lines);
    // The extraction validator is the same function, and an installed layer applies it.
    let mut lib = ModelLibrary::builtin();
    std::fs::write(&file, card("1.0")).unwrap();
    let errors = lib.load_dir_layer(dir.path(), SourceLayer::UserDir);
    assert_eq!(errors.len(), 1, "{errors:?}");
}

#[test]
fn an_absurd_card_value_is_never_demanded_of_the_board() {
    let (lib, _dir) = library_with("1.0");
    let bound = bind_board(&board(), &lib);
    let row = bound.report.row("U1").expect("U1 bind row");
    assert_eq!(row.model_id.as_deref(), Some("zz_ldo_slip"));
    assert!(!matches!(row.outcome, BindOutcome::Unresolved { .. }));
    let warning = row.warning.as_deref().unwrap_or_default();
    for needle in [
        "[partial model]",
        "cout_min_f",
        "outside the plausible range",
        "not used",
    ] {
        assert!(warning.contains(needle), "{needle:?} in {warning}");
    }
    for absurd in ["requires at least", "operating limit violated", "999"] {
        assert!(!warning.contains(absurd), "{absurd:?} in {warning}");
    }

    let findings = regulator_cout_lint(&board(), &lib).findings;
    assert_eq!(findings.len(), 1, "{findings:?}");
    let f = &findings[0];
    assert_eq!(f.severity, hauksbee_extract::Severity::Low, "{}", f.message);
    assert!(f.is_abstention(), "{}", f.message);
    assert!(f.message.contains("cout_min_f"), "{}", f.message);
    assert!(!f.message.contains("below the"), "{}", f.message);
    assert!(!f.message.contains("999"), "{}", f.message);
}

#[test]
fn the_same_card_in_farads_binds_and_checks_normally() {
    let (lib, _dir) = library_with("1.0e-6");
    let bound = bind_board(&board(), &lib);
    let row = bound.report.row("U1").expect("U1 bind row");
    let warning = row.warning.as_deref().unwrap_or_default();
    assert!(
        !warning.contains("outside the plausible range"),
        "{warning}"
    );
    // 10 uF across the output meets a 1 uF minimum: no finding.
    assert!(regulator_cout_lint(&board(), &lib).findings.is_empty());
}
