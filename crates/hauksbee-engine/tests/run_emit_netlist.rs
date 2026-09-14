//! `hauksbee run <board> --emit-netlist`: the connectivity surface.
//!
//! The point of the flag is that another tool can ask hauksbee what is
//! connected to what. That answer has to arrive on a schematic with no models
//! and no copper at all, and it has to follow the sheet hierarchy, or a
//! consumer gets a plausible-looking subset of the real circuit.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-extract/tests/fixtures")
        .join(rel)
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("hauksbee binary runs")
}

fn stdout(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// net name -> sorted "REF.PIN" strings, for compact assertions.
fn members(doc: &serde_json::Value) -> BTreeMap<String, Vec<String>> {
    doc["nets"]
        .as_array()
        .expect("nets is an array")
        .iter()
        .map(|net| {
            let pins = net["pins"]
                .as_array()
                .expect("pins is an array")
                .iter()
                .map(|p| {
                    format!(
                        "{}.{}",
                        p["ref"].as_str().expect("ref"),
                        p["pin"].as_str().expect("pin")
                    )
                })
                .collect();
            (net["name"].as_str().expect("name").to_string(), pins)
        })
        .collect()
}

#[test]
fn a_schematic_with_no_models_still_answers_what_is_connected_to_what() {
    let board = fixture("two_resistors.kicad_sch");
    let out = run(&[
        "run",
        board.to_str().expect("fixture path is utf-8"),
        "--emit-netlist",
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let so = stdout(&out);
    let doc: serde_json::Value = serde_json::from_str(&so)
        .unwrap_or_else(|e| panic!("--emit-netlist --json must be JSON: {e}\n{so}"));
    assert_eq!(doc["board"], "two_resistors");

    // R1 pin 2 and R2 pin 1 share the one wire on the sheet; the other two
    // pins each sit on a net of their own.
    let by_net = members(&doc);
    assert_eq!(
        by_net,
        BTreeMap::from([
            ("Net-(R1-Pad1)".to_string(), vec!["R1.1".to_string()]),
            (
                "Net-(R1-Pad2)".to_string(),
                vec!["R1.2".to_string(), "R2.1".to_string()]
            ),
            ("Net-(R2-Pad2)".to_string(), vec!["R2.2".to_string()]),
        ])
    );

    // Pin names come off the schematic symbol, so the field is populated.
    let first = &doc["nets"][0]["pins"][0];
    assert_eq!(first["pin_name"], "~");
}

#[test]
fn the_netlist_follows_the_sheet_hierarchy() {
    // subsheet_parent.kicad_sch instantiates subsheet_child.kicad_sch, whose
    // U1 is reachable only by resolving the child file. A netlist that stops
    // at the top sheet loses it silently, which is the failure this surface
    // exists to make visible.
    let board = fixture("subsheet_parent.kicad_sch");
    let out = run(&[
        "run",
        board.to_str().expect("fixture path is utf-8"),
        "--emit-netlist",
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let doc: serde_json::Value =
        serde_json::from_str(&stdout(&out)).expect("--emit-netlist --json must be JSON");
    let refs: Vec<String> = members(&doc)
        .into_values()
        .flatten()
        .map(|m| m.split('.').next().unwrap_or_default().to_string())
        .collect();
    assert!(refs.iter().any(|r| r == "R1"), "{refs:?}");
    assert!(
        refs.iter().any(|r| r == "U1"),
        "the child sheet's part is missing: {refs:?}"
    );
}

#[test]
fn the_text_form_is_one_tab_separated_line_per_connected_pin() {
    let board = fixture("two_resistors.kicad_sch");
    let out = run(&[
        "run",
        board.to_str().expect("fixture path is utf-8"),
        "--emit-netlist",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let so = stdout(&out);
    let lines: Vec<&str> = so.lines().collect();
    assert_eq!(
        lines,
        vec![
            "Net-(R1-Pad1)\tR1\t1\t~",
            "Net-(R1-Pad2)\tR1\t2\t~",
            "Net-(R1-Pad2)\tR2\t1\t~",
            "Net-(R2-Pad2)\tR2\t2\t~",
        ]
    );
}

#[test]
fn two_runs_on_the_same_board_emit_byte_identical_documents() {
    let board = fixture("power_labels.kicad_sch");
    let path = board.to_str().expect("fixture path is utf-8");
    let first = stdout(&run(&["run", path, "--emit-netlist", "--json"]));
    let second = stdout(&run(&["run", path, "--emit-netlist", "--json"]));
    assert!(!first.trim().is_empty());
    assert_eq!(first, second);
}
