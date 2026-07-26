//! CI parity for the firmware input tiers (see `hauksbee-engine`'s
//! `firmware_input`): the spec's `firmware` key accepts a zip or a PlatformIO
//! project exactly like `run --firmware` and the web drop zone, so the same
//! repo layout works on every surface, including the GitHub Action, which
//! shells `hauksbee-ci run`.

use std::io::Write;
use std::path::PathBuf;

use hauksbee_ci::{run, RunConfig};

fn repo(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hauksbee_ci_fwinput_{}_{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A spec whose `firmware` is a ZIP containing a built image (at the
/// PlatformIO artifact path) must resolve and run to the same green verdict as
/// pointing at the .hex directly. Uses the committed boot_gate hex + board, so
/// it needs the GPL-gated `avr` feature like the other boot-gate tests.
#[cfg(feature = "avr")]
#[test]
fn spec_firmware_zip_resolves_and_passes() {
    let dir = scratch("zip");
    let hex = std::fs::read(repo("../../testdata/firmware/boot_gate_a/boot_gate.hex"))
        .expect("committed boot_gate hex present");
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    w.start_file(
        "project/.pio/build/uno/firmware.hex",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    w.write_all(&hex).unwrap();
    let bytes = w.finish().unwrap().into_inner();
    std::fs::write(dir.join("fw.zip"), bytes).unwrap();

    let board = repo("examples/boards/boot_gate.kicad_pcb");
    std::fs::write(
        dir.join("spec.toml"),
        format!(
            r#"name = "firmware-from-zip parity"
board = "{}"
firmware = "fw.zip"
mcu = "atmega328p"
duration_ms = 50

[[supply]]
net = "+5V"
kind = "ideal"
volts = 5.0

[[assert]]
kind = "boot-coverage"
net = "GATE_CTRL"
min = 3.0
deadline_ms = 20.0
"#,
            board.display()
        ),
    )
    .unwrap();

    let result = run(&RunConfig {
        spec: dir.join("spec.toml"),
        ..Default::default()
    })
    .expect("zip firmware spec runs");
    assert!(
        result.passed(),
        "the zipped hex boots and drives the gate:\n{}",
        result.render_human()
    );
}

/// A `firmware` directory with neither a platformio.ini nor a built artifact
/// must fail with the resolver's actionable message, not a loader segfault or
/// a bare not-a-file error.
#[test]
fn spec_firmware_useless_dir_is_an_actionable_error() {
    let dir = scratch("dir");
    std::fs::create_dir_all(dir.join("not_a_project")).unwrap();
    let board = repo("examples/boards/boot_gate.kicad_pcb");
    std::fs::write(
        dir.join("spec.toml"),
        format!(
            r#"name = "firmware-dir error"
board = "{}"
firmware = "not_a_project"
duration_ms = 10

[[supply]]
net = "+5V"
kind = "ideal"
volts = 5.0

[[assert]]
kind = "no_faults"
"#,
            board.display()
        ),
    )
    .unwrap();

    let err = run(&RunConfig {
        spec: dir.join("spec.toml"),
        ..Default::default()
    })
    .expect_err("a useless firmware dir must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("platformio.ini"),
        "the error says what a resolvable directory would need: {msg}"
    );
    assert!(
        msg.contains("firmware"),
        "the error names the spec field: {msg}"
    );
}
