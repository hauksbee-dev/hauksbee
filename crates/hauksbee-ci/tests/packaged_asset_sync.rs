//! The crate-embedded demo firmware mirror must match `testdata/firmware/`.
//!
//! The blinky example embeds its firmware from this crate's `assets/` so
//! `cargo package` can ship it; the authoritative copy stays at repo-root
//! `testdata/firmware/demo/demo.hex`, which a dozen tests across the
//! workspace load by path. Skipped when the repo is not around the crate.

use std::path::Path;

#[test]
fn demo_firmware_mirror_matches_testdata() {
    let authoritative = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate lives two levels under the repo root")
        .join("testdata/firmware/demo/demo.hex");
    let Ok(on_disk) = std::fs::read(&authoritative) else {
        return;
    };
    assert_eq!(
        include_bytes!("../assets/firmware/demo.hex").as_slice(),
        on_disk.as_slice(),
        "demo firmware mirror has drifted from testdata/firmware/demo/demo.hex; \
         run scripts/sync-crate-assets.sh"
    );
}
