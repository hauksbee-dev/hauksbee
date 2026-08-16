//! A voltage assertion on a net nothing modeled defines must not pass on the
//! solver's number, and a net a modeled pull defines must not be blocked by
//! the open parts beside it.
//!
//! Found by testing a real defect pair as a user would: the GLR all2can
//! board's upstream fix adds a missing SWCLK pull-down (100k to GND). A user
//! asserts "SWCLK rests low". Pre-fix, SWCLK's only members are the
//! unmodelled MCU and the debug connector, both open: GMIN pins the isolated
//! node to 0.000 V, which sits neatly inside the asserted band, and the old
//! behavior reported INVALID on BOTH revisions (undermined by the same open
//! parts), so the one assertion that should discriminate the defect could
//! not. The refinement (hauksbee_engine::dcpath) reads the built circuit:
//!
//! - nothing with DC conductance on the net -> the number is a convention,
//!   not a level: ordinary RED, traced to the missing pull ("add a pull
//!   resistor or a model"), exit 1;
//! - a modeled passive path to a reference -> the level stands even if every
//!   open part is high-impedance: the open-part assumptions downgrade to a
//!   stated caveat and the assertion may pass, qualified, exit 0.
//!
//! Same stated assumption set on both sides, opposite verdicts: a user
//! assertion plus the fix commit's own resistor becomes a RED -> GREEN pair.

use hauksbee_ci::{report::CiResult, RunConfig};
use std::path::{Path, PathBuf};

/// The pre-fix shape: SWCLK carries only an unresolvable IC pin and a debug
/// connector pin. With `with_pulldown`, the fix commit's R33 (100k to GND)
/// joins them.
fn board(dir: &Path, with_pulldown: bool) -> PathBuf {
    let mut comps = String::from(
        "    (comp (ref \"IC4\") (value \"TOTALLYUNKNOWN999\") \
         (footprint \"Package_SO:SOIC-8_3.9x4.9mm_P1.27mm\") \
         (libsource (lib \"MCU\") (part \"X\")))\n\
         \x20   (comp (ref \"J6\") (value \"DebugConn\") \
         (footprint \"Connector_PinHeader_1.27mm:PinHeader_2x05_P1.27mm\") \
         (libsource (lib \"Connector\") (part \"Conn\")))\n",
    );
    let mut swclk_nodes = String::from(
        "      (node (ref \"IC4\") (pin \"1\"))\n      (node (ref \"J6\") (pin \"2\"))\n",
    );
    let mut gnd_nodes = String::from(
        "      (node (ref \"IC4\") (pin \"4\"))\n      (node (ref \"J6\") (pin \"3\"))\n",
    );
    if with_pulldown {
        comps.push_str(
            "    (comp (ref \"R33\") (value \"100k\") \
             (footprint \"Resistor_SMD:R_0402_1005Metric\") \
             (libsource (lib \"Device\") (part \"R\")))\n",
        );
        swclk_nodes.push_str("      (node (ref \"R33\") (pin \"1\"))\n");
        gnd_nodes.push_str("      (node (ref \"R33\") (pin \"2\"))\n");
    }
    let net = format!(
        "(export (version \"E\")\n  (components\n{comps}  )\n  (nets\n\
         \x20   (net (code \"1\") (name \"SWCLK\")\n{swclk_nodes})\n\
         \x20   (net (code \"2\") (name \"GND\")\n{gnd_nodes})\n  ))\n"
    );
    let name = if with_pulldown { "fixed.net" } else { "faulty.net" };
    let path = dir.join(name);
    std::fs::write(&path, net).expect("write board");
    path
}

fn run(dir: &Path, board: &Path) -> CiResult {
    let spec = dir.join(format!(
        "{}.toml",
        board.file_stem().unwrap().to_string_lossy()
    ));
    std::fs::write(
        &spec,
        format!(
            "name = \"swclk rests low\"\n\
             board = \"{}\"\n\
             duration_ms = 2\n\
             [[assert]]\n\
             kind = \"voltage\"\n\
             net = \"SWCLK\"\n\
             min = -0.1\n\
             max = 0.3\n",
            board.display()
        ),
    )
    .expect("write spec");
    hauksbee_ci::run(&RunConfig {
        spec,
        seed: None,
        models_dir: None,
    })
    .expect("run the spec")
}

#[test]
fn a_floating_net_fails_red_with_the_missing_pull_named() {
    let dir = tempfile::tempdir().expect("tempdir");
    let board = board(dir.path(), false);
    let result = run(dir.path(), &board);
    let voltage = result
        .results
        .iter()
        .find(|r| r.kind == "voltage")
        .expect("voltage assertion evaluated");
    assert!(!voltage.passed, "a floating net must not pass numerically");
    assert!(
        !voltage.invalid,
        "floating is a decidable RED under the run's own stated assumption, \
         not invalid-for-analysis: {}",
        voltage.detail
    );
    assert!(
        voltage.detail.contains("floating") && voltage.detail.contains("pull resistor"),
        "the diagnosis traces to the missing pull: {}",
        voltage.detail
    );
    assert!(
        voltage.detail.contains("IC4") && voltage.detail.contains("J6"),
        "the parts whose models would also unlock the answer are named: {}",
        voltage.detail
    );
    assert_eq!(result.exit_code(), 1, "an ordinary red, exit 1");
}

#[test]
fn a_modeled_pulldown_defines_the_net_and_the_opens_become_a_caveat() {
    let dir = tempfile::tempdir().expect("tempdir");
    let board = board(dir.path(), true);
    let result = run(dir.path(), &board);
    let voltage = result
        .results
        .iter()
        .find(|r| r.kind == "voltage")
        .expect("voltage assertion evaluated");
    assert!(
        voltage.passed,
        "100k to GND defines the level; the open parts beside it are a \
         caveat, not a veto: {}",
        voltage.detail
    );
    assert!(!voltage.invalid);
    assert!(
        voltage.detail.contains("R33")
            && voltage.detail.contains("unmodelled part on the net drives or loads it"),
        "the caveat names the defining element and the residual assumption: {}",
        voltage.detail
    );
    assert_eq!(result.exit_code(), 0, "a qualified green, exit 0");
}
