//! Can a hand-written spec make `hauksbee-ci run` exit 3?
//!
//! Exit 3 is invalid-for-analysis: the analog co-sim failed to solve a chunk, so
//! the samples in that window are held-stale and no honest verdict exists. The
//! consequences of that state are proven at the outcome boundary
//! (`analog_invalid.rs`) and the divergence itself at the scheduler boundary
//! (hauksbee-engine's `cosim_failed_chunk.rs`), but neither of those is a
//! process exiting 3 through a shell on a spec someone could write.
//!
//! **It cannot be reached, and this file is the evidence.** Every route from the
//! spec surface into the analog solve is closed on purpose, and each one is
//! closed at a specific place in the code:
//!
//! | route | what closes it |
//! |---|---|
//! | a rail as a hard short (two ideal sources on one node) | `SupplyLeg::stamp` puts the source on a hidden `__supply_<net>` node behind `STIFF_R_OHMS` (1 mΩ), so the rail is never a bare `Vsource` |
//! | a second source on a driven net | `runner::drive_net` refuses when the node already carries a `Vsource`, and only ever retargets its own `Vci_drive_<net>` |
//! | a second supply leg on one net | `runner::attach_supply` reconfigures the existing leg rather than stamping another |
//! | a stimulus peripheral | injects through a 50 Ω series resistor, never singular |
//! | a stiff nonlinear board (diodes, BJTs) | Newton's limiting, gmin and source stepping, plus step cutting to `dt_min` |
//! | a behavioural law that has no value | `behavioral::update`'s law loop CLAMPS a non-finite value to 0 instead of letting it poison the matrix |
//! | a `Device::Behavioral` whose expression faults inside Newton (the one path that does abort) | only `hauksbee_ir::spice` builds one, from a `B` card in a SPICE deck; no board format and no model entry can, so `hauksbee-ci run` never sees one |
//!
//! So the tests below are two-sided in an unusual way: each one drives a real
//! `hauksbee-ci run` down one of those routes and asserts it comes back with a
//! TRUSTWORTHY verdict (0 or 1), never 3. They are a live record of the search,
//! so the next person asking this question reads a gate rather than repeating it,
//! and if a change opens one of the routes, the test that names it fails and says
//! which.
//!
//! What would have to change for exit 3 to be provokable from a spec, in
//! increasing order of how much I would want it:
//!
//! 1. `hauksbee-ci run --deck <spice.cir>`, or a `board` that may be a SPICE
//!    deck. That reaches `Device::Behavioral` and therefore the in-Newton fault
//!    path, which is the only mechanism that genuinely aborts today. It is also
//!    the honest fixture: `B1 out 0 I={1/(V(a)-V(a))}` is a circuit with no
//!    solution, not a trick.
//! 2. A `[[fault]]`-style spec block that asks for a specific solver condition
//!    (an unsolvable node, a forced non-convergence) for exactly this purpose.
//!    A test hook in the spec surface, which is a cost.
//! 3. Nothing. Keep exit 3 proven at the two boundaries it is proven at, keep
//!    this file as the record of why the third boundary has no fixture, and read
//!    the GitLab/Jenkins/Azure recipes that map exit 3 as documentation of a
//!    contract rather than of an observed run.
//!
//! Note for whoever reads this next: this was measured against a branch point
//! where 0 Ω resistors and the solver's non-convergence reporting were being
//! changed elsewhere. If literal 0 Ω board resistors become milliohm links (they
//! were on their way to it), the first row of that table gets even harder to
//! break, not easier.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee-ci")
}

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture_dir() -> PathBuf {
    manifest().join("tests/fixtures/exit3")
}

/// Write `body` beside a copy of the blinky board and run it.
fn run_spec(tag: &str, body: &str) -> (Output, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        manifest().join("examples/boards/blinky.kicad_pcb"),
        dir.path().join("blinky.kicad_pcb"),
    )
    .unwrap();
    let spec = dir.path().join(format!("{tag}.toml"));
    std::fs::write(&spec, body).unwrap();
    let out = Command::new(bin())
        .arg("run")
        .arg(&spec)
        .current_dir(dir.path())
        .output()
        .expect("binary runs");
    (out, dir)
}

/// The assertion every route shares: a verdict, not a refusal.
fn assert_trustworthy(route: &str, out: &Output) {
    let code = out.status.code();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        matches!(code, Some(0) | Some(1)),
        "{route}: expected a trustworthy verdict (0 or 1), got {code:?}.\n\
         If this is now a 3, the route is OPEN and exit 3 finally has an \
         end-to-end fixture: promote this spec into one.\n{stdout}\n{stderr}"
    );
    assert!(
        !stdout.contains("INVALID"),
        "{route}: no assertion may come back INVALID:\n{stdout}"
    );
}

const HEAD: &str = "board = \"blinky.kicad_pcb\"\nduration_ms = 2\nframe_ms = 0.1\n";

#[test]
fn a_conflicting_source_on_a_supplied_rail_still_solves() {
    // The closest a spec gets to the engine test's impossible board: an ideal
    // supply commanding 5 V and a net_drive commanding 60 V on the same net.
    // STIFF_R_OHMS sits between them, so this is a large current, not a
    // singular matrix.
    let (out, _d) = run_spec(
        "conflict",
        &format!(
            "{HEAD}\n[[supply]]\nnet = \"+5V\"\nkind = \"ideal\"\nvolts = 5.0\n\n\
             [[net_drive]]\nnet = \"+5V\"\nvolts = 60.0\n\n\
             [[assert]]\nkind = \"voltage\"\nnet = \"ADC0\"\nmin = 0.0\n"
        ),
    );
    assert_trustworthy("conflicting source on a supplied rail", &out);
}

#[test]
fn a_diode_slammed_far_past_its_forward_drop_still_solves() {
    // D1's anode is LED_A and its cathode is GND, so this puts 60 V straight
    // across a diode with no series resistance. Newton's limiting handles it.
    let (out, _d) = run_spec(
        "diode",
        &format!(
            "{HEAD}\n[[supply]]\nnet = \"+5V\"\nkind = \"bench\"\nvolts = 5.0\n\n\
             [[net_drive]]\nnet = \"LED_A\"\nvolts = 60.0\n\n\
             [[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = 4.5\n"
        ),
    );
    assert_trustworthy("diode slammed past its forward drop", &out);
}

#[test]
fn a_zero_ohm_link_between_two_opposed_drives_still_solves() {
    // R1 joins D13 and LED_A. Overridden to 0 and driven from both ends at
    // opposite polarity, this is the classic singular pair, and it is exactly
    // the case the concurrent 0-ohm-as-milliohm-link work makes better behaved.
    let (out, _d) = run_spec(
        "zero-ohm",
        &format!(
            "{HEAD}\n[[supply]]\nnet = \"+5V\"\nkind = \"ideal\"\nvolts = 5.0\n\n\
             [[override]]\nref = \"R1\"\nvalue = \"0\"\n\n\
             [[net_drive]]\nnet = \"D13\"\nvolts = 5.0\n\n\
             [[net_drive]]\nnet = \"LED_A\"\nvolts = -5.0\n\n\
             [[assert]]\nkind = \"voltage\"\nnet = \"ADC0\"\nmin = 0.0\n"
        ),
    );
    assert_trustworthy("zero-ohm link between opposed drives", &out);
}

#[test]
fn a_destruction_scale_step_load_still_solves() {
    // A gigaamp step edge into an inductive decoupling network: the stiffest
    // thing a scenario can ask for.
    let (out, _d) = run_spec(
        "step",
        &format!(
            "{HEAD}\n[[supply]]\nnet = \"+5V\"\nkind = \"bench\"\nvolts = 5.0\n\
             current_limit_a = 1.0\nr_out_ohms = 0.05\n\n\
             [decoupling]\nparasitics = true\n\n\
             [[profile]]\nid = \"absurd\"\n[[profile.segment]]\nlevel_a = 1.0e9\n\
             rise_s = 0.0\nduration_s = 0.001\n\n\
             [[scenario]]\nid = \"step\"\npart = \"U1\"\nprofile = \"absurd\"\n\
             supply_net = \"+5V\"\nstart_ms = 0.5\n\n\
             [[assert]]\nkind = \"rail_window\"\nnet = \"+5V\"\nscenario = \"step\"\n\
             min = 4.5\n"
        ),
    );
    assert_trustworthy("destruction-scale step load", &out);
}

// The most promising route, and the one worth a permanent gate: a user model dir
// (`--models-dir`, a documented flag) carrying a behavioural law that cannot be
// evaluated. `v_in / sense_ohms` with `sense_ohms = 0` is infinite at every
// iterate, and an infinite value entering the matrix is precisely what makes a
// solve abort. It does not, because the law loop clamps a non-finite value to
// zero before it is stamped. That clamp is load-bearing in BOTH directions:
// remove it and a typo'd model silently becomes an aborted run.
#[test]
fn an_unevaluable_behavioural_law_is_clamped_rather_than_poisoning_the_solve() {
    let fixture = fixture_dir();
    let out = Command::new(bin())
        .arg("run")
        .arg(fixture.join("spec.toml"))
        .arg("--models-dir")
        .arg(fixture.join("models"))
        .output()
        .expect("binary runs");
    assert_trustworthy("unevaluable behavioural law", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The divider is undisturbed: the law contributed exactly nothing, which is
    // what "clamped to 0" means electrically.
    assert!(
        stdout.contains("SENSE"),
        "the fixture's assertion must have been evaluated:\n{stdout}"
    );
    assert!(
        stdout.contains("2.500V"),
        "a clamped law leaves the 1k/1k divider at half the rail:\n{stdout}"
    );
}

#[test]
fn the_exit_3_fixture_is_documented_where_someone_will_find_it() {
    // The fixture is the one artifact of this search that a future change can
    // turn into a real exit-3 case, so its own files have to say what they are
    // for. A fixture nobody can interpret is worse than no fixture.
    for (file, needle) in [
        ("spec.toml", "exit 3"),
        ("models/hbtest.toml", "exit 3"),
        ("board.net", "behavioural law"),
    ] {
        let path = fixture_dir().join(file);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        assert!(
            text.contains(needle),
            "{} must explain itself (looking for {needle:?})",
            path.display()
        );
    }
    assert!(Path::new(&fixture_dir()).is_dir());
}
