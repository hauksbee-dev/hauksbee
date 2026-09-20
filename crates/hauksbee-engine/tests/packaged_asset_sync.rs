//! The crate-embedded asset mirrors must match their authoritative copies.
//!
//! `cargo package` ships only files under the crate directory, so everything
//! this crate embeds lives in `assets/`. The large Unix installer and Python
//! provenance helper are authoritative there, with repo-root forwarders kept
//! for stable user-facing paths. Smaller assets remain mirrors of their
//! maintained homes (repo-root `scripts/`, the blinky board in
//! `crates/hauksbee-ci/examples/boards/`, the demo deck in repo-root
//! `examples/decks/`). This test checks both arrangements whenever the repo is
//! present; in a published crate (no repo around the crate), checks against
//! repo-root files are skipped.

use crate::support::repo_root;

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
fn installer_forwarders_target_the_crate_owned_scripts() {
    for (forwarder, target, marker) in [
        (
            "scripts/install-sims.sh",
            "crates/hauksbee-engine/assets/scripts/install-sims.sh",
            "exec \"$SCRIPT_DIR/../crates/hauksbee-engine/assets/scripts/install-sims.sh\" \"$@\"",
        ),
        (
            "scripts/simulator-provenance.py",
            "crates/hauksbee-engine/assets/scripts/simulator-provenance.py",
            "crates/hauksbee-engine/assets/scripts/simulator-provenance.py",
        ),
    ] {
        let forwarder = repo_root().join(forwarder);
        let target = repo_root().join(target);
        if !forwarder.exists() {
            // Published crate: repo-root forwarders are unavailable.
            continue;
        }
        assert!(
            target.is_file(),
            "forwarder target is missing: {}",
            target.display()
        );
        let contents = std::fs::read_to_string(&forwarder).expect("read installer forwarder");
        assert!(
            contents.contains(marker),
            "{} does not invoke its crate-owned target",
            forwarder.display()
        );
    }
}

#[test]
fn installer_sidecar_mirrors_match_the_repo_scripts() {
    for (embedded, rel) in [
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
        include_bytes!("../../hauksbee-cosim/assets/sensor-specs/mcp4728.toml"),
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
