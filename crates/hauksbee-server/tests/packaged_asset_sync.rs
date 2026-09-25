//! The co-sim crate's embedded MCP4728 spec must match this crate's copy.
//!
//! This crate's `assets/sensor-specs/` holds the authoritative bundled specs
//! the web sensor catalogue serves. `hauksbee-cosim` embeds its own mirror of
//! `mcp4728.toml` so `cargo package` can ship it; byte drift would mean the
//! scheduler runs a different behavior than the browser offers. Skipped when
//! the repo is not around the crate (a published crate).

use std::path::Path;

#[test]
fn sensor_spec_mirrors_match_testdata() {
    let mirror = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-cosim/assets/sensor-specs/mcp4728.toml");
    let Ok(on_disk) = std::fs::read_to_string(&mirror) else {
        return;
    };
    assert_eq!(
        include_str!("../assets/sensor-specs/mcp4728.toml"),
        on_disk,
        "co-sim mcp4728 mirror has drifted from assets/sensor-specs/; \
         run scripts/sync-crate-assets.sh"
    );
}
