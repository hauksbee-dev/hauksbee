//! Product gate for the browser's no-LLM behavior picker.
//!
//! The server crate intentionally cannot depend on the engine/model crate (the
//! engine already embeds the server), so its endpoint test can prove exact
//! checked-in bytes but not execute the shared sensor validator. This engine
//! test closes that boundary: every catalog file must parse through the same
//! `SensorSpec` path used by live attachment and CI.

use std::collections::BTreeSet;
use std::path::Path;

const CATALOG: [(&str, &str); 8] = [
    (
        "ads1115.toml",
        include_str!("../../../testdata/sensor-specs/ads1115.toml"),
    ),
    (
        "bma423_chip_id.toml",
        include_str!("../../../testdata/sensor-specs/bma423_chip_id.toml"),
    ),
    (
        "bme280.toml",
        include_str!("../../../testdata/sensor-specs/bme280.toml"),
    ),
    (
        "icm42605.toml",
        include_str!("../../../testdata/sensor-specs/icm42605.toml"),
    ),
    (
        "ina219.toml",
        include_str!("../../../testdata/sensor-specs/ina219.toml"),
    ),
    (
        "lm75.toml",
        include_str!("../../../testdata/sensor-specs/lm75.toml"),
    ),
    (
        "mcp4728.toml",
        include_str!("../../../testdata/sensor-specs/mcp4728.toml"),
    ),
    (
        "mpu6050.toml",
        include_str!("../../../testdata/sensor-specs/mpu6050.toml"),
    ),
];

#[test]
fn every_checked_in_sensor_behavior_is_catalogued_and_executable() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/sensor-specs");
    let on_disk = std::fs::read_dir(&directory)
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().and_then(|value| value.to_str()) == Some("toml"))
                .then(|| path.file_name().unwrap().to_string_lossy().into_owned())
        })
        .collect::<BTreeSet<_>>();
    let catalogued = CATALOG
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        on_disk, catalogued,
        "a checked-in spec and browser catalog drifted"
    );

    for (name, bytes) in CATALOG {
        let spec = hauksbee_models::SensorSpec::from_toml(bytes)
            .unwrap_or_else(|error| panic!("{name} is not executable: {error}"));
        assert!(
            !spec.sensor().name.trim().is_empty(),
            "{name} has no user-facing name"
        );
    }
}
