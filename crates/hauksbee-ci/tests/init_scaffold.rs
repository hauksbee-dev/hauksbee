//! `hauksbee-ci init <board>`: the generated starter spec must parse back through
//! the crate's own spec loader, and a second init must refuse to clobber it.
//!
//! The whole promise is "your first spec is an edit, not a blank page", which is
//! only true if what we emit is a spec the loader actually accepts. This binds a
//! real board (the committed AVR blinky, which has a detectable MCU and a +5V
//! rail), scaffolds a spec beside a temp copy, and loads it.

use std::path::PathBuf;

use hauksbee_ci::{init, Spec};

/// A board with a detectable MCU (atmega328p) and a named +5V rail.
fn blinky() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/boards/blinky.kicad_pcb")
}

/// Copy `blinky.kicad_pcb` into a fresh per-test temp dir and return the copy's
/// path, so init writes its `.toml` there rather than polluting the source tree.
/// The `tag` keeps parallel tests off each other's directory.
fn board_in_tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hauksbee_ci_init_{}_{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dst = dir.join("blinky.kicad_pcb");
    std::fs::copy(blinky(), &dst).unwrap();
    // A stale spec from a previous run would trip the overwrite guard.
    let _ = std::fs::remove_file(dir.join("blinky.toml"));
    dst
}

#[test]
fn init_generates_a_spec_the_loader_accepts() {
    let board = board_in_tempdir("load");
    let spec_path = init(&board).expect("init scaffolds a spec");

    // It landed beside the board as <stem>.toml.
    assert_eq!(spec_path.file_name().unwrap(), "blinky.toml");
    assert!(spec_path.exists(), "the spec file should be written to disk");

    // The generated spec round-trips through the crate's own loader (the point of
    // the feature). Structural validation runs here too, so a bad scaffold fails.
    let spec = Spec::load(&spec_path).expect("generated spec parses through Spec::load");

    // The scaffold reflects what the board actually is: the detected MCU, the
    // detected +5V supply leg, and the two enabled assertions.
    assert_eq!(spec.mcu.as_deref(), Some("atmega328p"), "detected MCU is filled in");
    assert!(
        spec.supplies.iter().any(|s| s.net == "+5V"),
        "the +5V supply leg the binder detected is scaffolded"
    );
    let kinds: Vec<&str> = spec.asserts.iter().map(|a| a.kind.as_str()).collect();
    assert!(kinds.contains(&"no_faults"), "a no_faults assertion is enabled");
    assert!(kinds.contains(&"boot-coverage"), "a boot-coverage assertion is enabled");
    // Exactly the two enabled assertions parse; the rail voltage asserts stay
    // commented out (they must not count toward the loaded spec).
    assert_eq!(spec.asserts.len(), 2, "only the two enabled assertions are live");
}

#[test]
fn init_refuses_to_overwrite_an_existing_spec() {
    let board = board_in_tempdir("overwrite");
    init(&board).expect("first init writes the spec");
    let err = init(&board).expect_err("second init must refuse to overwrite");
    assert!(
        err.to_string().contains("refusing to overwrite"),
        "the refusal names the reason, got: {err}"
    );
}
