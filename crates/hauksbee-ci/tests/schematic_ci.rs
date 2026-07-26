//! Schematic-stage CI: a `board = "thing.kicad_sch"` spec runs headless against
//! a schematic (no layout required), and agrees with the same spec run against
//! the project's PCB.
//!
//! The board under test is KiCad's `pic_programmer` demo, a 2-sheet KiCad 10
//! hierarchy whose schematic extraction is cross-validated net-for-net against
//! its layout (see hauksbee-extract's `schematic.rs` tests). That makes it the
//! right fixture for the powerful guarantee here: **schematic-stage CI and
//! layout-stage CI return the same verdict where both exist.**

use std::path::PathBuf;

use hauksbee_ci::{run, RunConfig};

/// A reference designator that exists only on the sub-sheet
/// (`pic_sockets.kicad_sch`): U5 is one of the PIC sockets. Used to prove the
/// hierarchy is actually followed.
const SUBSHEET_REF: &str = "U5";

/// The checked-in schematic-stage example spec.
fn schematic_example() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/pic_programmer_schematic.toml")
}

/// The corpus pic_programmer project directory, if the corpus symlink is
/// present in this checkout.
fn pic_programmer_dir() -> PathBuf {
    hauksbee_testkit::corpus_dir(env!("CARGO_MANIFEST_DIR"))
        .unwrap_or_default()
        .join("kicad-demos-src/demos/pic_programmer")
}

fn write_tmp(name: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("hauksbee_ci_sch_tests");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

/// The shared assertion body, parameterised on the board path. Identical for
/// the schematic and the PCB so the only variable is which stage we run at.
fn spec_body(board: &str) -> String {
    format!(
        r#"name = "pic_programmer agreement"
board = "{board}"
duration_ms = 1

[[supply]]
net = "VCC"
kind = "ideal"
volts = 5.0

[[assert]]
kind = "voltage"
net = "VCC"
min = 4.99
max = 5.01

[[assert]]
kind = "no_faults"
"#
    )
}

#[test]
fn schematic_example_spec_passes() {
    let dir = pic_programmer_dir();
    if !dir.join("pic_programmer.kicad_sch").exists() {
        eprintln!("corpus pic_programmer missing; skipping");
        return;
    }
    let result = run(&RunConfig {
        spec: schematic_example(),
        ..Default::default()
    })
    .expect("schematic spec runs");
    assert!(
        result.passed(),
        "schematic-stage CI must be GREEN:\n{}",
        result.render_human()
    );
    assert_eq!(result.results.len(), 2);
}

/// The flagship guarantee: the same spec body, once against the `.kicad_sch`
/// and once against the `.kicad_pcb`, returns the same per-assertion verdict.
#[test]
fn schematic_and_pcb_agree() {
    let dir = pic_programmer_dir();
    let sch = dir.join("pic_programmer.kicad_sch");
    let pcb = dir.join("pic_programmer.kicad_pcb");
    if !sch.exists() || !pcb.exists() {
        eprintln!("corpus pic_programmer pair missing; skipping");
        return;
    }

    let sch_spec = write_tmp("agree_sch.toml", &spec_body(&sch.display().to_string()));
    let pcb_spec = write_tmp("agree_pcb.toml", &spec_body(&pcb.display().to_string()));

    let sch_res = run(&RunConfig {
        spec: sch_spec,
        ..Default::default()
    })
    .expect("schematic spec runs");
    let pcb_res = run(&RunConfig {
        spec: pcb_spec,
        ..Default::default()
    })
    .expect("pcb spec runs");

    // Same number of assertions, same labels, same pass/fail on each.
    assert_eq!(
        sch_res.results.len(),
        pcb_res.results.len(),
        "schematic and PCB produced a different number of results"
    );
    for (s, p) in sch_res.results.iter().zip(pcb_res.results.iter()) {
        assert_eq!(s.label, p.label, "assertion labels diverged");
        assert_eq!(
            s.passed, p.passed,
            "verdict for {:?} disagrees between schematic ({}) and PCB ({})\n  sch: {}\n  pcb: {}",
            s.label, s.passed, p.passed, s.detail, p.detail
        );
    }

    // And both must actually be green (a vacuous agreement on two RED runs
    // would also satisfy the loop above).
    assert!(sch_res.passed(), "schematic run must be GREEN");
    assert!(pcb_res.passed(), "pcb run must be GREEN");
}

/// Pointing the spec at a sub-sheet (`pic_sockets.kicad_sch`, which is
/// referenced by the root) is a clear error that names the root, not a silent
/// partial extraction.
#[test]
fn pointing_at_subsheet_is_a_clear_error() {
    let dir = pic_programmer_dir();
    let sub = dir.join("pic_sockets.kicad_sch");
    if !sub.exists() {
        eprintln!("corpus pic_programmer missing; skipping");
        return;
    }
    let spec = write_tmp(
        "subsheet.toml",
        &format!(
            "name=\"sub\"\nboard=\"{}\"\nduration_ms=1\n[[assert]]\nkind=\"no_faults\"\n",
            sub.display()
        ),
    );
    let err = run(&RunConfig {
        spec,
        ..Default::default()
    })
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("sub-sheet"), "should flag a sub-sheet: {msg}");
    assert!(
        msg.contains("pic_programmer.kicad_sch"),
        "should name the hierarchy root: {msg}"
    );
}

/// The loader dispatches on file type: a `.kicad_sch` root is loaded *by path*
/// so its hierarchy resolves. A reference that only exists on the sub-sheet
/// (the PIC sockets live in `pic_sockets.kicad_sch`) must therefore be present
/// on the extracted board; a single-sheet text load would silently drop it. We
/// probe this through the public API: a `max_current` assert on a sub-sheet-only
/// reference is an "unknown component" error unless the sub-sheet was loaded.
#[test]
fn hierarchy_subsheet_components_are_loaded() {
    let dir = pic_programmer_dir();
    let sch = dir.join("pic_programmer.kicad_sch");
    if !sch.exists() {
        eprintln!("corpus pic_programmer missing; skipping");
        return;
    }
    // SUBSHEET_REF (a PIC socket) lives only on the sub-sheet pic_sockets.kicad_sch.
    let spec = write_tmp(
        "subsheet_load.toml",
        &format!(
            "name=\"load\"\nboard=\"{}\"\nduration_ms=1\n[[assert]]\nkind=\"max_current\"\nref=\"{}\"\namps=100.0\n",
            sch.display(),
            SUBSHEET_REF,
        ),
    );
    // If the sub-sheet were dropped, this errors with "unknown component". If it
    // is loaded, the ref resolves, and then either the assert is evaluable (a
    // 100 A ceiling trivially holds) or, for a socket kind whose current is not
    // tracked, the runner rejects the assert as untrackable ("resistors and
    // diodes"). Both loaded outcomes prove the hierarchy was followed; only
    // "unknown component" means it was not. (This test previously relied on the
    // untracked-kind case silently passing green, that silent pass was bug #18
    // and is now a loud rejection.)
    match run(&RunConfig {
        spec,
        ..Default::default()
    }) {
        Ok(res) => assert!(
            res.passed(),
            "sub-sheet component must be present and within its ceiling"
        ),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                !msg.contains("unknown component"),
                "sub-sheet ref {SUBSHEET_REF} not found, hierarchy not loaded: {msg}"
            );
            assert!(
                msg.contains("resistors and diodes"),
                "expected the untracked-kind rejection, got: {msg}"
            );
        }
    }
}
