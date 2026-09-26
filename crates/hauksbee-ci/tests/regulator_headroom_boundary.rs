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
    let min_in_v = minimum_input_v();
    // The slack admits float noise, not a real shortfall: 100 uV under refuses.
    let seen = (
        exit_code_at(d.path(), min_in_v),
        exit_code_at(d.path(), min_in_v - 1e-4),
    );
    assert_eq!(seen, (0, 3), "equality must pass, a shortfall must refuse");
}
