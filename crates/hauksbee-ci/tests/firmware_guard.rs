//! Defect: a spec whose `firmware` path is stale (missing on disk) used to reach
//! the native emulator loader and SIGSEGV (exit 139). The classic trigger is the
//! bundled blinky.toml, whose firmware path is spec-relative three levels up: it
//! resolves fine in-tree and breaks the moment the spec is copied elsewhere.
//!
//! The runner must instead fail with a clean, actionable error that names the
//! resolved absolute path, the spec field it came from, and what it was resolved
//! relative to. This is a library-level test (no binary, no native loader), so it
//! runs regardless of which MCU backend features are enabled.

use std::path::PathBuf;

use hauksbee_ci::{run, RunConfig, SpecError};

fn example_board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/boards/blinky.kicad_pcb")
}

#[test]
fn stale_firmware_path_is_clean_error_not_segfault() {
    // Build a self-contained spec dir with a real board and a firmware path that
    // does not exist, mirroring "copied the bundled spec somewhere else".
    let dir = std::env::temp_dir().join(format!("hauksbee-ci-fw-guard-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("make temp spec dir");
    std::fs::copy(example_board(), dir.join("blinky.kicad_pcb")).expect("copy board");
    let spec_path = dir.join("blinky.toml");
    std::fs::write(
        &spec_path,
        r#"name = "firmware guard"
board = "blinky.kicad_pcb"
firmware = "firmware/build/does_not_exist.hex"
mcu = "atmega328p"
duration_ms = 10

[[assert]]
kind = "voltage"
net = "+5V"
min = 0.0
"#,
    )
    .expect("write spec");

    let result = run(&RunConfig { spec: spec_path });
    let err = result.expect_err("a missing firmware file must fail, not run");
    let SpecError::Io(msg) = &err else {
        panic!("expected an Io error naming the firmware path, got {err:?}");
    };
    // Names the tried file and explains it is missing.
    assert!(msg.contains("does_not_exist.hex"), "message: {msg}");
    assert!(msg.contains("no firmware file"), "message: {msg}");
    // Points at the spec field and what the path resolved against.
    assert!(msg.contains("`firmware ="), "message names the spec field: {msg}");
    assert!(
        msg.contains("resolved relative to the spec file at"),
        "message explains the resolution base: {msg}"
    );
    // The absolute directory of the spec is named so a copied-spec user sees it.
    assert!(
        msg.contains(&dir.display().to_string()),
        "message names the spec dir {}: {msg}",
        dir.display()
    );

    let _ = std::fs::remove_dir_all(&dir);
}
