//! Real-world regression: the ClockworkPi uConsole mainboard reverse-extracted
//! from its published Allegro `.art` gerbers + Allegro `smt_loc.txt`
//! pick-and-place. No native CAD exists for this board, so this is the proof
//! that the gerber path unlocks a whole class of hardware galvani otherwise
//! could not ingest.
//!
//! The board exercises the awkward parts of the format: an older RS-274X
//! dialect (bare `FSA` with no zero char, `FS`+`MO` combined in one extended
//! block) that the upstream parser rejects until normalised, a *gerber-format*
//! drill film (holes drawn as flashes, not Excellon), Allegro role-named
//! layers (`top`/`gnd02`/`pwr04`/`gnd05`/`bottom`), and a `!`-delimited
//! pick-and-place in mils. See docs/GERBER.md.
//!
//! Skipped when the corpus is absent; required under `GALVANI_REQUIRE_CORPUS=1`.

use std::path::PathBuf;

use galvani_extract::gerber::from_gerber_dir;

fn uconsole_dir() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../board-corpus/famous/uconsole_gerber");
    p.exists().then_some(p)
}

#[test]
fn uconsole_mainboard_reconstructs() {
    let Some(dir) = uconsole_dir() else {
        if std::env::var("GALVANI_REQUIRE_CORPUS").is_ok() {
            panic!("corpus required but uconsole_gerber missing");
        }
        eprintln!("skipping uConsole (corpus absent)");
        return;
    };

    let g = from_gerber_dir(&dir).expect("uConsole gerbers must reverse-extract");
    let s = &g.stats;

    eprintln!(
        "uConsole: {} layers, {} holes, {} nets, {} components, {} flashes ({} assigned)",
        s.n_layers, s.n_holes, s.n_nets, s.n_components, s.total_flashes, s.assigned_flashes
    );

    // Five copper films shipped (top, gnd02, pwr04, gnd05, bottom).
    assert_eq!(s.n_layers, 5, "expected the 5 published copper films");
    // A handheld mainboard: hundreds of placed parts, hundreds of nets, a big
    // ground net. These are loose floors so cosmetic geometry changes don't
    // break the test, but a real regression (e.g. the FS normaliser breaking)
    // would collapse them to ~0.
    assert!(s.n_components > 180, "components placed: {}", s.n_components);
    assert!(s.n_nets > 250, "nets reconstructed: {}", s.n_nets);
    assert!(s.n_holes > 800, "plated holes: {}", s.n_holes);
    assert!(s.gnd_detected, "a GND-class net should be labelled");

    // Bind rate: most placed components must land on at least one net.
    let bound = g
        .board
        .components
        .iter()
        .filter(|c| c.pins.iter().any(|p| p.net.is_some()))
        .count();
    let rate = bound as f64 / g.board.components.len().max(1) as f64;
    assert!(rate > 0.9, "component bind rate only {:.0}%", rate * 100.0);

    // The dominant net should be ground, and it should be large.
    let gnd = g.board.net_by_name("GND").expect("GND net present");
    let gnd_pads = g
        .board
        .components
        .iter()
        .flat_map(|c| &c.pins)
        .filter(|p| p.net == Some(gnd.id))
        .count();
    assert!(gnd_pads > 500, "GND only has {gnd_pads} pads");
}
