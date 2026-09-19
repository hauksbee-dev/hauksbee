//! A datasheet-guaranteed headroom row is met when the input EQUALS it. The
//! L7805 at VI = VO + 2 V dropout was refused as "below the 7.000 V this model
//! needs": the solved input carries Newton residual and the comparison carried
//! no slack. Equality passes now; a shortfall a user could see still refuses.

use crate::support;
use hauksbee_ci::RunConfig;
use std::path::Path;

/// AMS1117-3.3 spec-limit endpoint 3.399 V + its 1.1 V typical dropout.
const MIN_IN_V: f64 = 4.499;

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
    // The slack admits float noise, not a real shortfall: 100 uV under refuses.
    let seen = (
        exit_code_at(d.path(), MIN_IN_V),
        exit_code_at(d.path(), MIN_IN_V - 1e-4),
    );
    assert_eq!(seen, (0, 3), "equality must pass, a shortfall must refuse");
}
