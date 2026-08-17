//! The crate-embedded sensor-spec mirrors must match `testdata/sensor-specs/`.
//!
//! The web sensor catalogue embeds its specs from this crate's
//! `assets/sensor-specs/` so `cargo package` can ship them; the authoritative
//! copies stay in repo-root `testdata/sensor-specs/`, which the engine's
//! co-sim tests load by path. Byte drift here would mean the browser serves a
//! different behavior than the tests validated. Skipped when the repo is not
//! around the crate (a published crate), where the mirror is the only copy.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate lives two levels under the repo root")
        .to_path_buf()
}

#[test]
fn sensor_spec_mirrors_match_testdata() {
    for (embedded, name) in [
        (include_str!("../assets/sensor-specs/lm75.toml"), "lm75"),
        (
            include_str!("../assets/sensor-specs/bma423_chip_id.toml"),
            "bma423_chip_id",
        ),
        (include_str!("../assets/sensor-specs/bme280.toml"), "bme280"),
        (
            include_str!("../assets/sensor-specs/mpu6050.toml"),
            "mpu6050",
        ),
        (
            include_str!("../assets/sensor-specs/ads1115.toml"),
            "ads1115",
        ),
        (include_str!("../assets/sensor-specs/ina219.toml"), "ina219"),
        (
            include_str!("../assets/sensor-specs/mcp4728.toml"),
            "mcp4728",
        ),
        (
            include_str!("../assets/sensor-specs/icm42605.toml"),
            "icm42605",
        ),
    ] {
        let path = repo_root().join(format!("testdata/sensor-specs/{name}.toml"));
        let Ok(on_disk) = std::fs::read_to_string(&path) else {
            return;
        };
        assert_eq!(
            embedded, on_disk,
            "sensor-spec mirror for {name} has drifted from testdata/sensor-specs/; \
             run scripts/sync-crate-assets.sh"
        );
    }
}
