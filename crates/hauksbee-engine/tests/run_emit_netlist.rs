//! `hauksbee run <board> --emit-netlist`: the connectivity surface.
//!
//! Another tool can ask hauksbee what is connected to what. The answer has to
//! arrive on a schematic with no models and no copper, follow the sheet
//! hierarchy, and come out byte-identical run to run.

use crate::support::{run, stdout};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn emit(rel: &str, json: bool) -> String {
    let board = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-extract/tests/fixtures")
        .join(rel);
    let mut args = vec![
        "run",
        board.to_str().expect("fixture path is utf-8"),
        "--emit-netlist",
    ];
    if json {
        args.push("--json");
    }
    let out = run(&args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout(&out)
}

/// net name -> "REF.PIN" members, for compact assertions.
fn members(json: &str) -> BTreeMap<String, Vec<String>> {
    let doc: serde_json::Value =
        serde_json::from_str(json).unwrap_or_else(|e| panic!("must be JSON: {e}\n{json}"));
    assert_eq!(
        doc["nets"][0]["pins"][0]["pin_name"], "~",
        "pin names come off the symbol"
    );
    doc["nets"]
        .as_array()
        .expect("nets")
        .iter()
        .map(|net| {
            let pins = net["pins"]
                .as_array()
                .expect("pins")
                .iter()
                .map(|p| {
                    format!(
                        "{}.{}",
                        p["ref"].as_str().unwrap(),
                        p["pin"].as_str().unwrap()
                    )
                })
                .collect();
            (net["name"].as_str().unwrap().to_string(), pins)
        })
        .collect()
}

#[test]
fn a_schematic_with_no_models_answers_what_is_connected_to_what_across_sheets() {
    // R1 pin 2 and R2 pin 1 share the one wire; the other two pins each sit on
    // a net of their own.
    let by_net = members(&emit("two_resistors.kicad_sch", true));
    let pins = |s: &[&str]| s.iter().map(|p| p.to_string()).collect::<Vec<_>>();
    assert_eq!(
        by_net,
        BTreeMap::from([
            ("Net-(R1-Pad1)".to_string(), pins(&["R1.1"])),
            ("Net-(R1-Pad2)".to_string(), pins(&["R1.2", "R2.1"])),
            ("Net-(R2-Pad2)".to_string(), pins(&["R2.2"])),
        ])
    );

    // subsheet_parent instantiates subsheet_child, whose U1 is reachable only
    // by resolving the child file; a netlist that stops at the top sheet loses
    // it silently.
    let refs: Vec<String> = members(&emit("subsheet_parent.kicad_sch", true))
        .into_values()
        .flatten()
        .map(|m| m.split('.').next().unwrap().to_string())
        .collect();
    assert!(
        refs.contains(&"R1".to_string()) && refs.contains(&"U1".to_string()),
        "{refs:?}"
    );
}

#[test]
fn the_text_form_is_tab_separated_and_two_runs_are_byte_identical() {
    let text = emit("two_resistors.kicad_sch", false);
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        [
            "Net-(R1-Pad1)\tR1\t1\t~",
            "Net-(R1-Pad2)\tR1\t2\t~",
            "Net-(R1-Pad2)\tR2\t1\t~",
            "Net-(R2-Pad2)\tR2\t2\t~",
        ]
    );
    let first = emit("power_labels.kicad_sch", true);
    assert!(!first.trim().is_empty());
    assert_eq!(first, emit("power_labels.kicad_sch", true));
}
