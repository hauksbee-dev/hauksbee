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
    w.start_file(
        "export/tarski.board",
        zip::write::SimpleFileOptions::default(),
    )
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

/// A tiny two-pad one-net board for the routing-seam tests. Pad `at` offsets
/// are footprint-RELATIVE (KiCad semantics), so R1 at (15,10) puts pad 1 at
/// board (10,10) and pad 2 at (20,10), both on net "A".
const SEAM_BOARD: &str = r#"# Board-as-Code (hauksbee board DSL v1)
board version 20241229

fn main {
    net "A"
    comp R1 lib "Resistor_SMD:R_0805_2012Metric" val "10k" layer "F.Cu" at 15 10 rot 0 {
        pad "1" smd rect at -5 0 size 1 1 layers [F.Cu] net "A"
        pad "2" smd rect at 5 0 size 1 1 layers [F.Cu] net "A"
    }
}
"#;

/// A SES that routes SEAM_BOARD's net "A": one F.Cu wire from pad 1 to pad 2.
/// No `(place ...)` anchor, so the scale is the declared-resolution fallback
/// (10000 units/mm).
const SEAM_SES: &str = r#"(session board.ses
  (routes
    (resolution um 10)
    (network_out
      (net "A"
        (wire (path F.Cu 2500 100000 100000 200000 100000))
      )
    )
  )
)
"#;

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hauksbee_cli_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The DSN/SES seam end to end, no router installed: `from-code --route-dsn`
/// exports the DSN and stops (announcing it), then `merge-ses` merges an
/// externally produced SES and its `--json` object carries the audit.
#[test]
fn route_dsn_export_and_merge_ses_roundtrip() {
    let dir = scratch_dir("seam");
    let board = dir.join("seam.board");
    std::fs::write(&board, SEAM_BOARD).unwrap();

    // Export the DSN and stop.
    let dsn = dir.join("seam.dsn");
    let out_pcb = dir.join("seam.kicad_pcb");
    let out = Command::new(bin())
        .args([
            "from-code",
            board.to_str().unwrap(),
            "--out",
            out_pcb.to_str().unwrap(),
            "--route-dsn",
            dsn.to_str().unwrap(),
        ])
        .output()
        .expect("hauksbee runs");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "route-dsn export must exit 0:\n{stderr}"
    );
    let dsn_text = std::fs::read_to_string(&dsn).expect("DSN written");
    assert!(dsn_text.starts_with("(pcb "), "a Specctra DSN:\n{dsn_text}");
    assert!(
        stderr.contains("merge-ses"),
        "announces the way back (merge-ses):\n{stderr}"
    );

    // Merge an externally routed SES back, machine-readable.
    let ses = dir.join("seam.ses");
    std::fs::write(&ses, SEAM_SES).unwrap();
    let routed = dir.join("routed.kicad_pcb");
    let out = Command::new(bin())
        .args([
            "merge-ses",
            board.to_str().unwrap(),
            ses.to_str().unwrap(),
            "--out",
            routed.to_str().unwrap(),
            "--route-strict",
            "--json",
        ])
        .output()
        .expect("hauksbee runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a fully-routed merge passes --route-strict:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("one parseable JSON object on stdout");
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["nets_total"], 1, "{v}");
    assert_eq!(v["connections_routed"], 1, "{v}");
    assert_eq!(v["unrouted"], 0, "{v}");
    assert_eq!(v["segments"], 1, "{v}");
    assert_eq!(v["engine"], "merged-ses", "{v}");
    assert_eq!(v["endpoint_net_violations"], 0, "{v}");
    assert!(v["drc_serious"].is_number(), "{v}");
    assert!(v["seconds"].is_number(), "{v}");
    let routed_text = std::fs::read_to_string(&routed).expect("routed board written");
    assert!(
        routed_text.contains("(segment"),
        "merged copper reaches the board file"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A SES that closes nothing real must not pass: an off-board wire merges but
/// leaves the connection open, `--route-strict --json` reports ok:false with
/// the metrics, and the board is NOT written.
#[test]
fn merge_ses_strict_fails_on_open_connection_with_one_json_line() {
    let dir = scratch_dir("seam_strict");
    let board = dir.join("seam.board");
    std::fs::write(&board, SEAM_BOARD).unwrap();
    let ses = dir.join("bad.ses");
    // A wire nowhere near pad 2: net "A" stays open.
    std::fs::write(
        &ses,
        r#"(session board.ses
  (routes
    (resolution um 10)
    (network_out
      (net "A"
        (wire (path F.Cu 2500 100000 100000 120000 100000))
      )
    )
  )
)
"#,
    )
    .unwrap();
    let routed = dir.join("routed.kicad_pcb");
    let out = Command::new(bin())
        .args([
            "merge-ses",
            board.to_str().unwrap(),
            ses.to_str().unwrap(),
            "--out",
            routed.to_str().unwrap(),
            "--route-strict",
            "--json",
        ])
        .output()
        .expect("hauksbee runs");
    assert!(!out.status.success(), "an open connection must fail strict");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 1, "exactly one JSON line on stdout:\n{stdout}");
    let v: serde_json::Value = serde_json::from_str(lines[0]).expect("parseable JSON");
    assert_eq!(v["ok"], false, "{v}");
    assert_eq!(v["unrouted"], 1, "{v}");
    assert!(v["error"].as_str().unwrap().contains("route-strict"), "{v}");
    assert!(!routed.exists(), "a strict-failed board is not written");

    let _ = std::fs::remove_dir_all(&dir);
}

/// `check-code --json` emits one machine-readable object; the human table is
/// unchanged when the flag is absent.
#[test]
fn check_code_json_object() {
    let dir = scratch_dir("checkjson");
    let board = dir.join("seam.board");
    std::fs::write(&board, SEAM_BOARD).unwrap();

    let out = Command::new(bin())
        .args([
            "check-code",
            board.to_str().unwrap(),
            "--seconds",
            "0.01",
            "--json",
        ])
        .output()
        .expect("hauksbee runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "healthy board exits 0:\n{stdout}");
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("one JSON object on stdout");
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["components"], 1, "{v}");
    assert!(
        v["nets"].is_number() && v["resolved_fraction"].is_number(),
        "{v}"
    );
    assert!(v["faults"].is_array() && v["unresolved"].is_array(), "{v}");
    assert!(v["simulated_seconds"].is_number(), "{v}");

    // Default output is the human table, not JSON.
    let human = Command::new(bin())
        .args(["check-code", board.to_str().unwrap(), "--seconds", "0.01"])
        .output()
        .expect("hauksbee runs");
    let hs = String::from_utf8_lossy(&human.stdout);
    assert!(
        hs.contains("Board-as-Code check:"),
        "human table unchanged:\n{hs}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `models resolve --json` emits the per-component table as JSON.
#[test]
fn models_resolve_json_object() {
    let dir = scratch_dir("resolvejson");
    let board = dir.join("seam.board");
    std::fs::write(&board, SEAM_BOARD).unwrap();

    let out = Command::new(bin())
        .args(["models", "resolve", board.to_str().unwrap(), "--json"])
        .output()
        .expect("hauksbee runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "resolve --json exits 0:\n{stdout}");
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("one JSON object on stdout");
    let comps = v["components"].as_array().expect("components array");
    assert_eq!(comps.len(), 1, "{v}");
    assert_eq!(comps[0]["ref"], "R1", "{v}");
    assert_eq!(comps[0]["value"], "10k", "{v}");
    assert!(
        comps[0]["model"].is_string() && comps[0]["layer"].is_string(),
        "{v}"
    );
    assert!(v["total"].is_number() && v["unresolved"].is_number(), "{v}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--version` carries a Git hash only when build.rs could prove the source
/// identity. A dirty checkout must not label its changed bytes as clean HEAD.
#[test]
fn version_reports_only_verified_source_identity() {
    let out = Command::new(bin())
        .arg("--version")
        .output()
        .expect("hauksbee runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "crate version present: {stdout}"
    );
    match option_env!("GIT_HASH") {
        Some(hash) => assert!(
            stdout.contains(&format!("git {hash}")),
            "verified git hash reaches --version: {stdout}"
        ),
        None => assert!(
            !stdout.contains("git "),
            "unverified source bytes must not claim a clean commit: {stdout}"
        ),
    }
}

/// `from-code --json` without a routing flag is refused loudly (it describes a
/// routing run), instead of silently printing nothing machine-readable.
#[test]
fn from_code_json_requires_a_routing_flag() {
    let dir = scratch_dir("jsonnoroute");
    let board = dir.join("seam.board");
    std::fs::write(&board, SEAM_BOARD).unwrap();
    let out_pcb = dir.join("seam.kicad_pcb");
    let out = Command::new(bin())
        .args([
            "from-code",
            board.to_str().unwrap(),
            "--out",
            out_pcb.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("hauksbee runs");
    assert!(!out.status.success(), "--json with no routing flag refuses");
    // The refusal itself is machine-readable (the JSON error envelope).
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("JSON error envelope on stdout");
    assert_eq!(v["ok"], false, "{v}");
    assert!(v["error"].as_str().unwrap().contains("--route"), "{v}");

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
