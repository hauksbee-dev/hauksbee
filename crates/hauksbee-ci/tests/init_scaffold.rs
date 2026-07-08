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

/// An STM32F103 blue pill board: its MCU binds to the external `renode:stm32f103`
/// backend, which cannot satisfy a boot-coverage assertion the way the in-process
/// AVR backend can.
fn stm32_bluepill() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/boards/stm32_bluepill_demo.kicad_pcb")
}

/// Copy `blinky.kicad_pcb` into a fresh per-test temp dir and return the copy's
/// path, so init writes its `.toml` there rather than polluting the source tree.
/// The `tag` keeps parallel tests off each other's directory.
fn board_in_tempdir(tag: &str) -> PathBuf {
    copy_board_to_tempdir(&blinky(), tag)
}

/// Copy `src` (a `.kicad_pcb`) into a fresh per-test temp dir, returning the
/// copy's path, so init writes its `.toml` there rather than into the source
/// tree. The `tag` keeps parallel tests off each other's directory.
fn copy_board_to_tempdir(src: &std::path::Path, tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hauksbee_ci_init_{}_{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = src.file_name().unwrap();
    let dst = dir.join(file);
    std::fs::copy(src, &dst).unwrap();
    // A stale spec from a previous run would trip the overwrite guard.
    let stem = src.file_stem().unwrap().to_str().unwrap();
    let _ = std::fs::remove_file(dir.join(format!("{stem}.toml")));
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

    // The scaffold reflects what the board actually is: the detected MCU and the
    // detected +5V supply leg.
    assert_eq!(spec.mcu.as_deref(), Some("atmega328p"), "detected MCU is filled in");
    assert!(
        spec.supplies.iter().any(|s| s.net == "+5V"),
        "the +5V supply leg the binder detected is scaffolded"
    );
    // The starter must run GREEN out of the box: only `no_faults` is live.
    // boot-coverage is scaffolded COMMENTED-OUT on every backend (even the AVR
    // in-process one), because it asserts on firmware behaviour and `firmware =`
    // is itself commented in the starter — left live it goes RED on the first run
    // (the exact false-red the persona panel hit). The rail voltage asserts also
    // stay commented. So exactly one assertion loads.
    let kinds: Vec<&str> = spec.asserts.iter().map(|a| a.kind.as_str()).collect();
    assert!(kinds.contains(&"no_faults"), "a no_faults assertion is enabled");
    assert!(
        !kinds.contains(&"boot-coverage"),
        "boot-coverage is scaffolded commented-out so the starter is GREEN out of the box"
    );
    assert_eq!(spec.asserts.len(), 1, "only the no_faults assertion is live");

    // The rendered text still carries a (commented) boot-coverage block so the
    // user can opt in after wiring firmware.
    let text = hauksbee_ci::init::render_spec(&board).expect("render scaffolds a spec");
    assert!(
        text.contains("# kind = \"boot-coverage\""),
        "a commented boot-coverage block is present to opt into, got:\n{text}"
    );
    assert!(
        !text.contains("\nkind = \"boot-coverage\""),
        "boot-coverage must not be a live assertion in the starter, got:\n{text}"
    );
}

#[test]
fn init_comments_out_boot_coverage_when_the_backend_cannot_satisfy_it() {
    // The STM32 blue pill binds to the external `renode:stm32f103` backend, which
    // co-sims GPIO/UART but models ADC and I2C/SPI peripheral-slave coupling as
    // no-ops and cannot report pin drive direction (docs/MCU.md). A live
    // boot-coverage assertion there can go RED with a misleading diagnosis on a
    // net the firmware actually drives, so init must scaffold it commented-out
    // with an honest note rather than as a live assertion.
    let board = copy_board_to_tempdir(&stm32_bluepill(), "backend_gap");

    // The rendered text carries the honest backend-gap note and a commented-out
    // (`# `) boot-coverage assertion, not a live one.
    let text = hauksbee_ci::init::render_spec(&board).expect("render scaffolds a spec");
    assert!(
        text.contains("renode:stm32f103"),
        "the note names the backend that cannot satisfy the assertion, got:\n{text}"
    );
    assert!(
        text.contains("# kind = \"boot-coverage\""),
        "boot-coverage is scaffolded commented-out, got:\n{text}"
    );
    assert!(
        !text.contains("\nkind = \"boot-coverage\""),
        "boot-coverage must not be a live assertion on this backend, got:\n{text}"
    );

    // It still writes and round-trips through the loader, with the live-assertion
    // set reduced to `no_faults` only (boot-coverage did not load).
    let spec_path = init(&board).expect("init scaffolds a spec");
    let spec = Spec::load(&spec_path).expect("generated spec parses through Spec::load");
    let kinds: Vec<&str> = spec.asserts.iter().map(|a| a.kind.as_str()).collect();
    assert!(kinds.contains(&"no_faults"), "no_faults stays live");
    assert!(
        !kinds.contains(&"boot-coverage"),
        "boot-coverage is commented out, so it must not load"
    );
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
