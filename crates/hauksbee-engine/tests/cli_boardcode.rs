//! CLI-level test for Board-as-Code as a first-class analysis input (#63):
//! `hauksbee run <file>.board --report` works and reproduces the bind a
//! `.kicad_pcb` produces, exercising the real compiled binary.

use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

fn board(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

#[test]
fn run_board_as_code_report() {
    // A self-contained Board-as-Code source: one 1N4148 diode (SOD-323, pads 1/2,
    // no roles) and a resistor. The CLI must accept the `.board`, recompile it,
    // bind it, and print the report with a pin-role guess for the diode.
    let dir = std::env::temp_dir().join(format!("hauksbee_cli_board_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("InputSystem.board");
    std::fs::write(
        &path,
        r#"# Board-as-Code (hauksbee board DSL v1)
board version 20241229

fn main {
    net "ANODE_NET"
    net "CATHODE_NET"
    comp D1 lib "Diode_SMD:D_SOD-323" val "1N4148" layer "F.Cu" at 0 0 rot 0 {
        pad "1" smd rect at 0 0 size 1 1 layers [F.Cu] net "CATHODE_NET"
        pad "2" smd rect at 1 0 size 1 1 layers [F.Cu] net "ANODE_NET"
    }
    comp R1 lib "Resistor_SMD:R_0402_1005Metric" val "10k" layer "F.Cu" at 5 0 rot 0 {
        pad "1" smd rect at 5 0 size 1 1 layers [F.Cu] net "ANODE_NET"
        pad "2" smd rect at 6 0 size 1 1 layers [F.Cu] net "CATHODE_NET"
    }
}
"#,
    )
    .unwrap();

    let out = Command::new(bin())
        .args(["run", path.to_str().unwrap(), "--report"])
        .output()
        .expect("hauksbee runs");
    assert!(out.status.success(), "run .board --report must exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);

    // The diode bound (analog diode), and a pin-role guess fired for it.
    assert!(stdout.contains("D1"), "report lists D1:\n{stdout}");
    assert!(
        stdout.contains("analog diode"),
        "the diode binds as an analog diode:\n{stdout}"
    );
    assert!(
        stdout.contains("pin-role guess"),
        "report mentions pin-role guesses:\n{stdout}"
    );
    assert!(
        stdout.contains("diode_2pin_k1_a2"),
        "guess names the matched rule:\n{stdout}"
    );

    // The header-only detection also works (a `.board` saved under a different
    // extension still routes through the recompile path).
    let alt = dir.join("InputSystem.txt");
    std::fs::copy(&path, &alt).unwrap();
    let out2 = Command::new(bin())
        .args(["run", alt.to_str().unwrap(), "--report"])
        .output()
        .expect("hauksbee runs");
    assert!(out2.status.success(), "header-detected .board must run");
    assert!(String::from_utf8_lossy(&out2.stdout).contains("analog diode"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_zip_of_a_board_code_export_checks() {
    // A zipped Board-as-Code export ("zip it and we figure it out", the same
    // promise the web drop zone keeps) must run through the CLI too. The old
    // loader treated EVERY .zip as a gerber archive, so this exact input died
    // with a gerber extraction error while the identical upload analyzed fine
    // on the web.
    use std::io::Write;
    let dsl = br#"# Board-as-Code (hauksbee board DSL v1)
board version 20241229

fn main {
    net "A"
    net "B"
    comp R1 lib "Resistor_SMD:R_0402_1005Metric" val "10k" layer "F.Cu" at 0 0 rot 0 {
        pad "1" smd rect at 0 0 size 1 1 layers [F.Cu] net "A"
        pad "2" smd rect at 1 0 size 1 1 layers [F.Cu] net "B"
    }
}
"#;
    let dir = std::env::temp_dir().join(format!("hauksbee_cli_zip_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let zip_path = dir.join("export.zip");
    let mut w = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
    w.start_file("export/tarski.board", zip::write::SimpleFileOptions::default())
        .unwrap();
    w.write_all(dsl).unwrap();
    w.finish().unwrap();

    let out = Command::new(bin())
        .args(["run", zip_path.to_str().unwrap(), "--check"])
        .output()
        .expect("hauksbee runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "run <zip of a .board export> --check must exit 0:\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("R1") || stderr.contains("R1"),
        "the compiled board's R1 reaches the check report:\nstdout: {stdout}\nstderr: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn to_code_netlist_emits_board() {
    // `to-code` accepts a netlist (not just a .kicad_pcb) and emits Board-as-Code.
    let net_path = board("../../testdata/tarski_brownout_cell.net");
    if !net_path.exists() {
        return; // corpus not present
    }
    let out = Command::new(bin())
        .args(["to-code", net_path.to_str().unwrap()])
        .output()
        .expect("hauksbee runs");
    assert!(out.status.success(), "to-code on a .net must succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Board-as-Code"),
        "emits the .board header:\n{}",
        &stdout[..stdout.len().min(200)]
    );
    assert!(stdout.contains("fn main"), "emits the main body");
}
