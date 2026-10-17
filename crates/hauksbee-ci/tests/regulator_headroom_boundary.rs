//! A datasheet-guaranteed headroom row is met when the input EQUALS it. The
//! L7805 at VI = VO + 2 V dropout was refused as "below the 7.000 V this model
//! needs": the solved input carries Newton residual and the comparison carried
//! no slack. Equality passes now; a shortfall a user could see still refuses.

use crate::support;
use hauksbee_ci::RunConfig;
use hauksbee_ir::evidence::ModelUncertainty;
use hauksbee_models::{ComponentQuery, ModelLibrary};
use std::path::Path;

fn minimum_input_v() -> f64 {
    let resolved = ModelLibrary::builtin().resolve(&ComponentQuery::new(
        None,
        Some("AMS1117-3.3".into()),
        None,
    ));
    let model = resolved.model.expect("fixture regulator card");
    let upper_vout = resolved
        .provenance
        .as_ref()
        .into_iter()
        .flat_map(hauksbee_ir::evidence::ModelSource::uncertainty)
        .find_map(|uncertainty| match uncertainty {
            ModelUncertainty::Interval {
                parameter, high, ..
            } if parameter == "vout" => Some(*high),
            _ => None,
        })
        .expect("AMS1117 card must preserve its upper output limit");
    upper_vout + model.params.get_f64("dropout_v").expect("declared dropout")
}

fn exit_code_at(dir: &Path, volts: f64) -> i32 {
    let board = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/regulator.board");
    let spec = dir.join(format!("boundary-{volts}.toml"));
    let body = format!(
        "name = \"headroom boundary\"\nboard = {}\nduration_ms = 3\n\
         [[supply]]\nnet = \"VIN\"\nkind = \"ideal\"\nvolts = {volts}\n\
         [[assert]]\nkind = \"voltage\"\nnet = \"VOUT\"\nmin = 3.2\nmax = 3.4\nafter_ms = 1.0\n",
        support::toml_path(&board)
    );
    std::fs::write(&spec, body).expect("write spec");
    let cfg = RunConfig {
        spec,
        ..RunConfig::default()
    };
    hauksbee_ci::run(&cfg).expect("run the spec").exit_code()
}

#[test]
fn an_input_exactly_on_the_dropout_bound_is_supported() {
    let d = tempfile::tempdir().expect("tempdir");
    // The bound is at the pin: the ideal supply's 1 mOhm sense resistor drops
    // the 10 mA load plus the card's 5 mA quiescent current the LDO now draws.
    let min_in_v = (3.3_f64 / 330.0 + 5e-3).mul_add(1e-3, minimum_input_v());
    // The slack admits float noise, not a real shortfall: 100 uV under refuses.
    let seen = (
        exit_code_at(d.path(), min_in_v),
        exit_code_at(d.path(), min_in_v - 1e-4),
    );
    assert_eq!(seen, (0, 3), "equality must pass, a shortfall must refuse");
}

/// A supply declared on the regulator's own output is refused, and the
/// refusal names the input to declare instead, by net and by pin, with the
/// headroom the card needs there, so the spec is fixed in one edit.
#[test]
fn a_supply_on_the_regulated_rail_is_pointed_at_the_regulator_input() {
    let d = tempfile::tempdir().expect("tempdir");
    let board = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/regulator.board");
    let spec = d.path().join("on-output.toml");
    let body = format!(
        "name = \"supply on the output\"\nboard = {}\nduration_ms = 3\n\
         [[supply]]\nnet = \"VOUT\"\nkind = \"ideal\"\nvolts = 3.3\n\
         [[assert]]\nkind = \"voltage\"\nnet = \"VOUT\"\nmin = 3.2\nmax = 3.4\n",
        support::toml_path(&board)
    );
    std::fs::write(&spec, body).expect("write spec");
    let cfg = RunConfig {
        spec,
        ..RunConfig::default()
    };
    let err = hauksbee_ci::run(&cfg).expect_err("a fight with U1 must refuse");
    let msg = err.to_string();
    let fix = format!(
        "Declare the supply on U1's input instead: net = \"VIN\" (or ref = \"U1\", pin = \"3\"), \
         at least {}.",
        hauksbee_ir::units::format_volts(minimum_input_v())
    );
    assert!(msg.contains("'VOUT' is already driven by U1"), "{msg}");
    assert!(msg.contains(&fix), "{msg}");
}
