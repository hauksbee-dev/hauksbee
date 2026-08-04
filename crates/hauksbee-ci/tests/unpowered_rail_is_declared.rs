//! A rail nobody could put a voltage to must be declared, not solved around.
//!
//! Found by uploading the flagship board as a newcomer would: `hauksbee-ci
//! init` then `hauksbee-ci run`, no edits. The board has six supply-ish nets.
//! One of them, `ANALOG_VDD`, names a supply but not a voltage, so the binder
//! declines to guess (right: inventing 5 V would overdrive every part on it)
//! and the net sits at 0 V. The run then solved the whole board around a dead
//! rail and reported, with no qualification of any kind:
//!
//!     [FAIL] no stress faults raised
//!           2 fault(s); first: R_Shunt15301 overpower 0.281 > 0.062 at 0.4ms
//!
//! A named accusation against one specific 0402 resistor, at a named
//! millisecond, derived from an operating point that does not exist. Powering
//! ANALOG_VDD makes it vanish. A first-time user has no way to tell that report
//! from a true one, and this is the asymmetry that matters: a missed fault
//! costs them a bug, a confident false one costs them the tool.
//!
//! So the rule these tests hold: if a supply-named net carrying a rail's worth
//! of parts is powered by nothing, every surface says so, and says so before
//! the results it undermines.

use hauksbee_ci::{report::CiResult, RunConfig};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

/// A board with two supply-ish nets: `VCC` (5 V by convention, so it resolves)
/// and `ANALOG_VDD` (a supply with no readable magnitude). Enough parts hang off
/// ANALOG_VDD for it to read as a rail rather than a signal.
fn board_with_a_nameless_rail(dir: &Path) -> PathBuf {
    let mut nodes_vcc = String::new();
    let mut nodes_avdd = String::new();
    let mut comps = String::new();
    for i in 1..=8 {
        comps.push_str(&format!(
            "    (comp (ref \"R{i}\") (value \"10k\") \
             (footprint \"Resistor_SMD:R_0603_1608Metric\") \
             (libsource (lib \"Device\") (part \"R\")))\n"
        ));
        nodes_avdd.push_str(&format!("      (node (ref \"R{i}\") (pin \"1\"))\n"));
        nodes_vcc.push_str(&format!("      (node (ref \"R{i}\") (pin \"2\"))\n"));
    }
    let net = format!(
        "(export (version \"E\")\n  (components\n{comps}  )\n  (nets\n\
         \x20   (net (code \"1\") (name \"VCC\")\n{nodes_vcc})\n\
         \x20   (net (code \"2\") (name \"ANALOG_VDD\")\n{nodes_avdd})\n  ))\n"
    );
    let path = dir.join("nameless_rail.net");
    std::fs::write(&path, net).expect("write board");
    path
}

fn spec_for(dir: &Path, board: &Path, extra: &str) -> PathBuf {
    let spec = dir.join("spec.toml");
    std::fs::write(
        &spec,
        format!(
            "name = \"rail check\"\n\
             board = \"{}\"\n\
             duration_ms = 2\n\
             [[supply]]\n\
             net = \"VCC\"\n\
             kind = \"ideal\"\n\
             volts = 5.0\n\
             [[assert]]\n\
             kind = \"no_faults\"\n{extra}",
            board.display()
        ),
    )
    .expect("write spec");
    spec
}

fn run(spec: &Path) -> CiResult {
    hauksbee_ci::run(&RunConfig {
        spec: spec.to_path_buf(),
        seed: None,
        models_dir: None,
    })
    .expect("run the spec")
}

#[test]
fn a_rail_with_no_readable_voltage_is_named_in_every_surface() {
    let dir = tempfile::tempdir().expect("tempdir");
    let board = board_with_a_nameless_rail(dir.path());
    let result = run(&spec_for(dir.path(), &board, ""));

    assert_eq!(
        result.dead_rails,
        vec!["ANALOG_VDD".to_string()],
        "ANALOG_VDD is a supply with no magnitude and nothing feeds it"
    );

    let human = result.render_human();
    assert!(
        human.contains("UNPOWERED RAIL: ANALOG_VDD"),
        "the terminal report names it:\n{human}"
    );
    // Position is the point. A reader who meets a named component fault first
    // and the caveat afterwards has already believed the fault.
    let banner = human.find("UNPOWERED RAIL").expect("banner present");
    let first_verdict = human
        .find("[PASS]")
        .into_iter()
        .chain(human.find("[FAIL]"))
        .min()
        .expect("some assertion rendered");
    assert!(
        banner < first_verdict,
        "the warning must come BEFORE the results it undermines:\n{human}"
    );

    let json: serde_json::Value = serde_json::from_str(&result.render_json()).expect("json parses");
    assert_eq!(
        json["dead_rails"],
        serde_json::json!(["ANALOG_VDD"]),
        "a tool reading --json gets the same qualification a person does"
    );

    let junit = result.render_junit();
    assert!(
        junit.contains("UNPOWERED RAIL: ANALOG_VDD"),
        "a dashboard reader who only sees the JUnit tab gets it too:\n{junit}"
    );

    let gh = result.render_github_annotations();
    assert!(
        gh.contains("::warning title=hauksbee-ci UNPOWERED RAIL::"),
        "and so does a pull request:\n{gh}"
    );
}

#[test]
fn powering_the_rail_clears_the_warning() {
    // The other half: this must be a statement about the board, not a banner
    // that is always on. A spec that answers the question gets a clean report.
    let dir = tempfile::tempdir().expect("tempdir");
    let board = board_with_a_nameless_rail(dir.path());
    let spec = spec_for(
        dir.path(),
        &board,
        "\n[[supply]]\nnet = \"ANALOG_VDD\"\nkind = \"ideal\"\nvolts = 3.3\n",
    );
    let result = run(&spec);
    assert!(
        result.dead_rails.is_empty(),
        "the spec powers ANALOG_VDD, so nothing is dead: {:?}",
        result.dead_rails
    );
    assert!(
        !result.render_human().contains("UNPOWERED RAIL"),
        "and the banner is gone"
    );
}

#[test]
fn a_resolvable_rail_is_never_called_dead() {
    // VCC has a conventional voltage and the binder resolves it. Warning about
    // it would be the false positive this whole mechanism exists to prevent.
    let dir = tempfile::tempdir().expect("tempdir");
    let board = board_with_a_nameless_rail(dir.path());
    let result = run(&spec_for(dir.path(), &board, ""));
    assert!(
        !result.dead_rails.iter().any(|n| n == "VCC"),
        "VCC resolves to 5 V by name: {:?}",
        result.dead_rails
    );
}

#[test]
fn the_scaffold_asks_about_the_rail_it_cannot_resolve() {
    // The report warning is the safety net. The scaffold is the fix: put the
    // question where the user is already editing, before they wait out a run.
    let dir = tempfile::tempdir().expect("tempdir");
    let board = board_with_a_nameless_rail(dir.path());
    let toml = hauksbee_ci::init::render_spec(&board).expect("scaffold");
    assert!(
        toml.contains("# net = \"ANALOG_VDD\""),
        "the starter spec offers a commented supply for it:\n{toml}"
    );
    assert!(
        toml.contains("what does this rail run at?"),
        "and says what it needs from the user:\n{toml}"
    );
    // It must stay commented: the scaffold does not know the voltage, and a
    // guessed number that runs is worse than a blank that does not.
    assert!(
        !toml.contains("\nnet = \"ANALOG_VDD\""),
        "it must not invent a voltage and enable it:\n{toml}"
    );
}

/// The AeroFC shape: a rail the binder can price by name (`+3V3`) next to a
/// battery rail it cannot (`VBAT`, in the engine's supply-token list but with
/// no readable magnitude). Enough parts on VBAT for it to read as a rail.
fn board_with_a_battery_rail(dir: &Path) -> PathBuf {
    let mut nodes_3v3 = String::new();
    let mut nodes_vbat = String::new();
    let mut comps = String::new();
    for i in 1..=8 {
        comps.push_str(&format!(
            "    (comp (ref \"R{i}\") (value \"10k\") \
             (footprint \"Resistor_SMD:R_0603_1608Metric\") \
             (libsource (lib \"Device\") (part \"R\")))\n"
        ));
        nodes_vbat.push_str(&format!("      (node (ref \"R{i}\") (pin \"1\"))\n"));
        nodes_3v3.push_str(&format!("      (node (ref \"R{i}\") (pin \"2\"))\n"));
    }
    let net = format!(
        "(export (version \"E\")\n  (components\n{comps}  )\n  (nets\n\
         \x20   (net (code \"1\") (name \"+3V3\")\n{nodes_3v3})\n\
         \x20   (net (code \"2\") (name \"VBAT\")\n{nodes_vbat})\n  ))\n"
    );
    let path = dir.join("battery_rail.net");
    std::fs::write(&path, net).expect("write board");
    path
}

#[test]
fn the_scaffolds_questions_cover_every_rail_the_first_run_warns_about() {
    // The AeroFC regression: init once recognised supply nets with its own
    // narrower pattern, scaffolded only the +3V3 it could price, and the very
    // first run of that untouched scaffold warned "UNPOWERED RAIL: VBAT" about
    // a net init never mentioned. init and the runner now share ONE detector
    // (runner::unpowered_supply_nets), so this pins the invariant end to end:
    // every rail the first run warns about was already a [[supply]] question
    // in the scaffold the user is holding.
    let dir = tempfile::tempdir().expect("tempdir");
    let board = board_with_a_battery_rail(dir.path());
    let spec_path =
        hauksbee_ci::init::init_to(&board, Some(dir.path())).expect("scaffold the board");
    let toml = std::fs::read_to_string(&spec_path).expect("read the scaffold");

    assert!(
        toml.contains("# net = \"VBAT\""),
        "the scaffold offers a supply for the battery rail it cannot price:\n{toml}"
    );
    assert!(
        toml.contains("[[supply]]\nnet = \"+3V3\""),
        "while powering the rail it CAN price, live and unprompted:\n{toml}"
    );

    // Run the scaffold exactly as written, no edits. Whatever the report
    // warns about must be a net the scaffold already asked about.
    let result = run(&spec_path);
    assert!(
        result.dead_rails.contains(&"VBAT".to_string()),
        "the first run does warn about VBAT (the warning half of the loop): {:?}",
        result.dead_rails
    );
    for net in &result.dead_rails {
        assert!(
            toml.contains(&format!("# net = \"{net}\"")),
            "the run warns about {net} but the scaffold never asked about it; \
             init and the runner have drifted apart:\n{toml}"
        );
    }
}

#[test]
fn the_flagship_board_is_the_case_this_came_from() {
    // Regression anchor. If ANALOG_VDD ever stops being reported on the real
    // board, the newcomer path is back to reporting a named component fault
    // from an operating point that does not exist.
    let board = repo_root().join("testdata/tarski_inputsystem.net");
    if !board.exists() {
        eprintln!("skipping: the flagship board is not in this tree");
        return;
    }
    let toml = hauksbee_ci::init::render_spec(&board).expect("scaffold the flagship board");
    assert!(
        toml.contains("# net = \"ANALOG_VDD\""),
        "the scaffold asks about ANALOG_VDD on the real board:\n{toml}"
    );
    assert!(
        toml.contains("[[supply]]\nnet = \"+5V\""),
        "while still powering the rail it CAN resolve, unprompted:\n{toml}"
    );
}
