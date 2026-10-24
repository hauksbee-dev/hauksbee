//! Schematic-stage CI: a `board = "thing.kicad_sch"` spec runs headless against
//! a schematic (no layout required), and agrees with the same spec run against
//! the project's PCB.
//!
//! The board under test is `KiCad`'s `pic_programmer` demo, a 2-sheet `KiCad` 10
//! hierarchy whose schematic extraction is cross-validated net-for-net against
//! its layout (see hauksbee-extract's `schematic.rs` tests). That makes it the
//! right fixture for the powerful guarantee here: **schematic-stage CI and
//! layout-stage CI return the same verdict where both exist.**

use std::path::PathBuf;

use hauksbee_ci::run;

use crate::support;

/// A reference designator that exists only on the sub-sheet
/// (`pic_sockets.kicad_sch`): U5 is one of the PIC sockets. Used to prove the
/// hierarchy is actually followed.
const SUBSHEET_REF: &str = "U5";

/// The checked-in example, with only its corpus location adapted to this checkout.
/// Resolve the EXAMPLE's board, not an unrelated fixture: a broken example path
/// must still fail, while an explicit corpus override must actually be used.
fn schematic_example() -> Option<tempfile::NamedTempFile> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/pic_programmer_schematic.toml");
    let text = std::fs::read_to_string(path).expect("read the example spec");
    let mut spec: toml::Value = toml::from_str(&text).expect("parse the example spec");
    let rel = spec["board"]
        .as_str()
        .expect("the example spec has a board key")
        .strip_prefix("../../../board-corpus/")
        .expect("the example points into the documented repo-root corpus");
    let board = hauksbee_testkit::corpus_board(
        env!("CARGO_MANIFEST_DIR"),
        &rel.replacen("kicad_demos/", "kicad-demos-src/", 1),
    )
    .or_else(|| {
        hauksbee_testkit::corpus_or_skip(
            env!("CARGO_MANIFEST_DIR"),
            rel,
            "PIC programmer schematic example",
        )
    })?;
    spec["board"] =
        toml::Value::String(board.canonicalize().unwrap().to_string_lossy().into_owned());
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), toml::to_string(&spec).unwrap()).unwrap();
    Some(file)
}

/// The corpus `pic_programmer` project directory, if the corpus is present in
/// this checkout.
///
/// Resolved through the testkit helper rather than joining one hardcoded name,
/// because the corpus has two accepted layouts: the hand-built tree calls this
/// board `kicad-demos-src`, `scripts/fetch-corpus.sh` calls it `kicad_demos`.
/// Naming only the first meant every test here skipped for anyone who followed
/// CONTRIBUTING and ran the fetch, and a skip nobody reads is not a gate.
fn pic_programmer_dir() -> PathBuf {
    hauksbee_testkit::corpus_board_any(
        env!("CARGO_MANIFEST_DIR"),
        &[
            "kicad-demos-src/demos/pic_programmer",
            "kicad_demos/demos/pic_programmer",
        ],
    )
    .unwrap_or_default()
}

/// The `pic_programmer` parts with no model, open on its VCC rail.
const PIC_PROGRAMMER_OPEN: &[&str] = &["U1", "U4", "U5", "U6"];

/// The shared assertion body, parameterised on the board path. Identical for
/// the schematic and the PCB so the only variable is which stage we run at.
fn spec_body(board: &std::path::Path) -> String {
    format!(
        r#"name = "pic_programmer agreement"
board = {board}
duration_ms = 1

[[supply]]
ref = "P1"
pin = "2"
kind = "ideal"
volts = 12.0

[[assert]]
kind = "voltage"
net = "VCC"
min = 4.8
max = 5.2

[[assert]]
kind = "no_faults"
"#,
        board = support::toml_path(board),
    )
}

/// The board's one real static finding: `C4`, a capacitor position (disc
/// capacitor footprint) whose value is `0`. It is the board's own flaw, so the
/// shipped example runs RED on exactly that row, and the examples' own
/// assertions stay green.
const PIC_PROGRAMMER_ZERO_VALUE_ROW: &str = "static_check: lint value_package_sanity on C4";

#[test]
fn schematic_example_spec_is_red_only_on_the_boards_zero_value_capacitor() {
    let Some(spec) = schematic_example() else {
        return;
    };
    let result = run(&support::run_config(spec.path().to_path_buf())).expect("schematic spec runs");
    // Its two assertions are the ones the board can honestly support: the rail
    // ahead of the 7805 and the input diode. None touches an open part, so none
    // is undermined, and the four open parts must still be disclosed in the run.
    // The run as a whole is RED because the board's own static check fails it on
    // C4 (a capacitor position with value `0`), which `hauksbee check` reports
    // too; the example has no waiver because the board belongs to the corpus.
    support::assert_red_only_on_static(
        &result,
        &[PIC_PROGRAMMER_ZERO_VALUE_ROW],
        "the shipped schematic example",
    );
    let own: Vec<_> = result
        .results
        .iter()
        .filter(|r| r.kind != hauksbee_ir::vocabulary::STATIC_CHECK_KIND)
        .collect();
    assert_eq!(own.len(), 2);
    assert!(own.iter().all(|r| r.passed && !r.invalid));
    for part in PIC_PROGRAMMER_OPEN {
        let id = format!("open-part:{part}");
        assert!(
            result.assumptions.iter().any(|a| a.id().as_str() == id),
            "the open part {part} is no longer disclosed in the run's assumptions"
        );
    }
}

/// The headline guarantee: the same spec body, once against the `.kicad_sch`
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

    let sch_spec = support::write_tmp("agree_sch.toml", &spec_body(&sch));
    let pcb_spec = support::write_tmp("agree_pcb.toml", &spec_body(&pcb));

    let sch_res = run(&support::run_config(sch_spec)).expect("schematic spec runs");
    let pcb_res = run(&support::run_config(pcb_spec)).expect("pcb spec runs");

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
        assert_eq!(
            s.invalid, p.invalid,
            "INVALID for {:?} disagrees between schematic and PCB\n  sch: {}\n  pcb: {}",
            s.label, s.detail, p.detail
        );
    }

    // And both must hold numerically (a vacuous agreement on two RED runs would
    // also satisfy the loop above). VCC and `no_faults` are the assertions the
    // shipped example leaves out because the open parts undermine them, so this
    // body must stay INVALID for exactly that model gap, not turn silently green;
    // `assert_green` once U4 and the sockets are modelled and C4 is fixed.
    // The board's own static check also fails it on C4 at both stages, so the
    // run exits 1 (RED) rather than 3; the INVALID cause is still pinned.
    assert_eq!(sch_res.exit_code(), pcb_res.exit_code());
    for res in [&sch_res, &pcb_res] {
        let failing: Vec<&str> = res
            .results
            .iter()
            .filter(|r| {
                r.kind == hauksbee_ir::vocabulary::STATIC_CHECK_KIND && !r.passed && !r.invalid
            })
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(failing, [PIC_PROGRAMMER_ZERO_VALUE_ROW]);
        support::assert_undermined_only_by_open_parts(res, PIC_PROGRAMMER_OPEN);
    }
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
    let spec = support::write_tmp(
        "subsheet.toml",
        &format!(
            "name=\"sub\"\nboard={}\nduration_ms=1\n[[assert]]\nkind=\"no_faults\"\n",
            support::toml_path(&sub)
        ),
    );
    let err = run(&support::run_config(spec)).unwrap_err();
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
    let spec = support::write_tmp(
        "subsheet_load.toml",
        &format!(
            "name=\"load\"\nboard={}\nduration_ms=1\n[[assert]]\nkind=\"max_current\"\nref=\"{}\"\namps=100.0\n",
            support::toml_path(&sch),
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
    match run(&support::run_config(spec)) {
        Ok(res) => support::assert_green(
            &res,
            "sub-sheet component must be present and within its ceiling",
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
