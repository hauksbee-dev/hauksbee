//! The crate-embedded asset mirrors must match their authoritative copies.
//!
//! `cargo package` ships only files under the crate directory, so everything
//! this crate embeds lives in `assets/` mirrors. The authoritative copies
//! stay where they are maintained and consumed (repo-root `scripts/`, the
//! blinky board in `crates/hauksbee-ci/examples/boards/`, the demo deck in
//! repo-root `examples/decks/`). This test pins mirror == authoritative,
//! byte for byte, whenever the authoritative file is present; in a published
//! crate (no repo around the crate) the comparison is skipped, which is the
//! point: there the mirror IS the only copy.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate lives two levels under the repo root")
        .to_path_buf()
}

fn assert_mirror(embedded: &[u8], authoritative_rel: &str) {
    let path = repo_root().join(authoritative_rel);
    let Ok(on_disk) = std::fs::read(&path) else {
        // Published crate or partial checkout: the mirror is the only copy.
        return;
    };
    assert_eq!(
        embedded,
        on_disk.as_slice(),
        "crate asset mirror has drifted from {authoritative_rel}; \
         run scripts/sync-crate-assets.sh"
    );
}

#[test]
fn installer_script_mirrors_match_the_repo_scripts() {
    for (embedded, rel) in [
        (
            include_bytes!("../assets/scripts/install-sims.sh").as_slice(),
            "scripts/install-sims.sh",
        ),
        (
            include_bytes!("../assets/scripts/common.sh").as_slice(),
            "scripts/common.sh",
        ),
        (
            include_bytes!("../assets/scripts/required-simulator-versions.env").as_slice(),
            "scripts/required-simulator-versions.env",
        ),
        (
            include_bytes!("../assets/scripts/renode-checksums.txt").as_slice(),
            "scripts/renode-checksums.txt",
        ),
        (
            include_bytes!("../assets/scripts/espressif-qemu-checksums.txt").as_slice(),
            "scripts/espressif-qemu-checksums.txt",
        ),
        (
            include_bytes!("../assets/scripts/simulator-provenance.py").as_slice(),
            "scripts/simulator-provenance.py",
        ),
        (
            include_bytes!("../assets/scripts/simavr-payload-provenance.sh").as_slice(),
            "scripts/simavr-payload-provenance.sh",
        ),
        (
            include_bytes!("../assets/scripts/install-sims-windows.ps1").as_slice(),
            "scripts/install-sims-windows.ps1",
        ),
    ] {
        assert_mirror(embedded, rel);
    }
}

#[test]
fn embedded_sensor_spec_mirror_matches_testdata() {
    assert_mirror(
        include_bytes!("../assets/sensor-specs/mcp4728.toml"),
        "testdata/sensor-specs/mcp4728.toml",
    );
}

#[test]
fn example_mirrors_match_their_homes() {
    assert_mirror(
        include_bytes!("../assets/examples/blinky.kicad_pcb"),
        "crates/hauksbee-ci/examples/boards/blinky.kicad_pcb",
    );
    assert_mirror(
        include_bytes!("../assets/examples/rlc_ringdown.cir"),
        "examples/decks/rlc_ringdown.cir",
    );
}
