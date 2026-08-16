//! Reusable source-bound cards first authored for a historical campaign must
//! resolve from the built-in library. A user should not need to discover and
//! install that campaign pack merely to make an ordinary board executable.

use hauksbee_models::{ComponentQuery, ModelLibrary};

fn resolve(value: &str) -> hauksbee_models::Resolution {
    ModelLibrary::builtin().resolve(&ComponentQuery {
        value: Some(value.into()),
        ..Default::default()
    })
}

#[test]
fn pedalboard_core_devices_are_builtin_and_executable() {
    for (value, id, capability) in [
        (
            "NCP1117-3.3_SOT223",
            "ncp1117_3v3_sot223",
            "nominal_dc_regulation",
        ),
        ("AP64501SP-13", "ap64501sp_13", "board_feedback_divider"),
        ("74AHCT1G32SE-7", "74ahct1g32se_7", "combinational_or"),
        ("TLP2761", "tlp2761_dc_transfer", "threshold_transfer"),
    ] {
        let resolution = resolve(value);
        let model = resolution
            .model
            .as_ref()
            .unwrap_or_else(|| panic!("{value} did not resolve from builtins"));
        assert_eq!(model.id, id, "{value} resolved to the wrong card");
        assert!(
            model
                .coverage
                .implements
                .iter()
                .any(|item| item == capability),
            "{value} must expose executable capability {capability}"
        );
        assert!(
            !model.coverage.missing.is_empty(),
            "the executable slice must not hide remaining behavior gaps"
        );
        assert_eq!(
            resolution.source.as_deref(),
            Some("builtin"),
            "{value} unexpectedly requires external model setup"
        );
        assert!(
            !resolution.references.is_empty(),
            "{value} must retain a machine-readable primary-source citation"
        );
    }

    let buck = resolve("AP64501SP-13").model.unwrap();
    assert!(buck.behavioral.converter.is_some());
    let optocoupler = resolve("TLP2761").model.unwrap();
    assert!(optocoupler.behavioral.fsm.is_some());
    assert_eq!(optocoupler.behavioral.laws.len(), 4);
    let gate = resolve("74AHCT1G32SE-7").model.unwrap();
    assert_eq!(gate.logic.inputs, ["a", "b"]);
    assert_eq!(gate.logic.outputs, ["y1"]);
}

#[test]
fn similar_but_unverified_values_do_not_gain_exact_behavior() {
    for (value, forbidden) in [
        ("NCP1117-5.0_SOT223", "ncp1117_3v3_sot223"),
        ("AP64500SP-13", "ap64501sp_13"),
        ("74AHCT1G32SE-NOT-7", "74ahct1g32se_7"),
        ("TLP2762", "tlp2761_dc_transfer"),
    ] {
        let resolution = resolve(value);
        assert!(
            resolution
                .model
                .as_ref()
                .is_none_or(|model| model.id != forbidden),
            "hostile near-name {value} gained behavior from {forbidden}"
        );
    }
}
