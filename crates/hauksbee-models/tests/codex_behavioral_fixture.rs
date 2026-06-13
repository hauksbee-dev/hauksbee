//! Offline fixture test for behavioural extraction.
//!
//! `testdata/datasheets/LTC4020_codex_extracted.toml` is the verbatim output of
//! a REAL `model-extract --kind charger` run of codex against the LTC4020
//! datasheet content (see docs/MODELS.md "Live extraction"). This test loads it
//! offline and asserts the agreement points against the hand-written model, so
//! the live result is captured and regression-locked without needing codex in
//! CI. The live run itself is the `#[ignore]`d test in the model-extract binary.

use hauksbee_models::schema::DbFile;

#[test]
fn codex_extracted_ltc4020_matches_hand_model_structurally() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/datasheets/LTC4020_codex_extracted.toml"
    );
    let src = std::fs::read_to_string(path).expect("read codex fixture");
    let db: DbFile = toml::from_str(&src).expect("codex fixture parses as a model");
    let m = db.models.into_iter().next().expect("one model");

    // Agreement with the hand model:
    assert_eq!(m.kind, hauksbee_models::ComponentKind::Vreg, "charger base kind");
    assert!(!m.behavioral.is_empty(), "must carry a behavioural block");

    // Pin map: codex got the load-bearing pads right.
    for (pad, role) in [("36", "pvin"), ("20", "bat"), ("25", "ilimit"), ("23", "csp"), ("22", "csn")] {
        assert_eq!(m.pins.get(pad).map(String::as_str), Some(role), "pad {pad}");
    }

    // Converter: same topology, pins, output voltage, efficiency as the hand model.
    let c = m.behavioral.converter.as_ref().expect("converter");
    assert_eq!(
        c.topology,
        hauksbee_models::behavioral::Topology::BuckBoost,
        "codex agreed on buck-boost topology"
    );
    assert_eq!(c.out_pin, "bat");
    assert_eq!(c.in_pin, "pvin");
    assert!((c.vout_setpoint - 28.8).abs() < 0.1, "8S LiFePO4 28.8 V CV target");
    assert!((c.efficiency.unwrap() - 0.92).abs() < 0.01, "92% efficiency");

    // The input-current-limit program structure is present (rsense + prog refs),
    // which is the load-bearing agreement: codex understood the ILIMIT/RSENSE
    // programming even though it (honestly) left the transfer-function constants
    // at 0 because the excerpt did not state the equation.
    let sp = c.iin_program.as_ref().expect("iin_program present");
    assert!(sp.rsense_ref.is_some(), "codex bound an input sense resistor ref");
    assert!(sp.prog_ref.is_some(), "codex bound an ILIMIT program resistor ref");
}
