//! CLI-level checks that ride the real compiled binary on small KiCad boards:
//! the bind report with pin-role guesses, `models resolve --json`, the
//! `--version` source identity, and the shipped unrouted stormduino fixture.

use crate::support::{board, run};
use std::path::PathBuf;

fn scratch_dir(tag: &str) -> PathBuf {
    hauksbee_testkit::fresh_scratch_dir("cli", tag)
}

#[test]
fn run_report_guesses_pin_roles_for_a_bare_diode() {
    // A self-contained KiCad board: one 1N4148 diode (SOD-323, pads 1/2, no
    // roles) and a resistor. The CLI must bind it and print the report with a
    // pin-role guess for the diode.
    let dir = scratch_dir("diode_report");
    let path = dir.join("InputSystem.kicad_pcb");
    std::fs::write(
        &path,
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "ANODE_NET")
  (net 2 "CATHODE_NET")
  (module Diode_SMD:D_SOD-323 (layer F.Cu)
    (at 0 0)
    (fp_text reference D1 (at 0 0) (layer F.SilkS))
    (fp_text value 1N4148 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (size 1 1) (layers F.Cu) (net 2 "CATHODE_NET"))
    (pad 2 smd rect (at 1 0) (size 1 1) (layers F.Cu) (net 1 "ANODE_NET"))
  )
  (module Resistor_SMD:R_0402_1005Metric (layer F.Cu)
    (at 5 0)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (size 1 1) (layers F.Cu) (net 1 "ANODE_NET"))
    (pad 2 smd rect (at 1 0) (size 1 1) (layers F.Cu) (net 2 "CATHODE_NET"))
  )
)
"#,
    )
    .unwrap();

    let out = run(&["run", path.to_str().unwrap(), "--report"]);
    assert!(out.status.success(), "run --report must exit 0");
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

    let _ = std::fs::remove_dir_all(&dir);
}

/// The retired Board-as-Code text format is not special-cased: a `.board`
/// file is refused like any other unsupported format, with the ordinary list
/// of formats the tool does read.
#[test]
fn a_dot_board_file_is_refused_with_the_ordinary_format_guidance() {
    let dir = scratch_dir("dot_board");
    let path = dir.join("legacy.board");
    std::fs::write(&path, "board version 20241229\n\nfn main {\n}\n").unwrap();

    let out = run(&["run", path.to_str().unwrap(), "--report"]);
    assert_eq!(out.status.code(), Some(1), "an unsupported file exits 1");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let (_, listed) = stderr
        .split_once("unrecognized board format")
        .unwrap_or_else(|| panic!("the refusal names the format problem:\n{stderr}"));
    assert!(
        listed.contains("KiCad") && listed.contains("Eagle"),
        "{stderr}"
    );
    for retired in [".board", "Board-as-Code", "DSL"] {
        assert!(
            !listed.contains(retired),
            "the format list must not offer {retired}:\n{stderr}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// A tiny two-pad one-net board. Pad `at` offsets are footprint-RELATIVE
/// (`KiCad` semantics), so R1 at (15,10) puts pad 1 at board (10,10) and pad 2
/// at (20,10), both on net "A".
const SEAM_BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "A")
  (module Resistor_SMD:R_0805_2012Metric (layer F.Cu)
    (at 15 10)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at -5 0) (size 1 1) (layers F.Cu) (net 1 "A"))
    (pad 2 smd rect (at 5 0) (size 1 1) (layers F.Cu) (net 1 "A"))
  )
)
"#;

/// `models resolve --json` emits the per-component table as JSON.
#[test]
fn models_resolve_json_object() {
    let dir = scratch_dir("resolvejson");
    let board = dir.join("seam.kicad_pcb");
    std::fs::write(&board, SEAM_BOARD).unwrap();

    let out = run(&["models", "resolve", board.to_str().unwrap(), "--json"]);
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
    let out = run(&["--version"]);
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

/// The unrouted stormduino fixture is a conversion of a real board. Its USB-C
/// receptacle sits at 270 degrees with 0.3 mm pads on a 0.5 mm pitch; written
/// unrotated, neighbouring pads of the one footprint overlapped, giving sixteen
/// "oracle-confirmed copper shorts" and a Live Sim that bridged them. A board
/// with no routed copper has no copper shorts; the only honest DO NOT ORDER
/// reason left is that it is not routed.
#[test]
fn unrouted_stormduino_fixture_reports_no_copper_shorts() {
    let path = board("../hauksbee-ci/tests/fixtures/stormduino.kicad_pcb");
    let out = run(&["run", path.to_str().unwrap(), "--check", "--plain"]);
    let text = crate::support::stdout(&out);
    let head: String = text.chars().take(1500).collect();
    assert!(
        !text.contains("copper short") && !text.contains("are touching"),
        "an unrouted example must not report copper shorts:\n{head}"
    );
    assert!(
        text.contains("the board is not routed yet"),
        "the honest reason it cannot be ordered stays:\n{head}"
    );
}
