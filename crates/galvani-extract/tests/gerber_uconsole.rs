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

/// The gerber trace-current surface (Round 5): per-net copper geometry is
/// reconstructed from the gerber primitives so a trace-current check can run on
/// a board that ships no CAD. The decisive correctness property is the same as
/// the native-CAD trace_current module: the high-current planes are reported
/// `Poured` (their true cross-section is not a discrete-segment width, so they
/// are honestly out of reach), while real routed signal traces carry a finite,
/// sane width. A check that mistook a plane's pad-entry stub for the conductor
/// would be a confident false positive on a famous board; the `Poured`
/// exemption prevents it.
#[test]
fn uconsole_per_net_copper_is_reconstructed_and_planes_are_poured() {
    use galvani_extract::gerber::connect::GerberCopperKind;

    let Some(dir) = uconsole_dir() else {
        if std::env::var("GALVANI_REQUIRE_CORPUS").is_ok() {
            panic!("corpus required but uconsole_gerber missing");
        }
        eprintln!("skipping uConsole copper (corpus absent)");
        return;
    };

    let g = from_gerber_dir(&dir).expect("uConsole gerbers must reverse-extract");
    let nc = &g.stats.net_copper;
    assert_eq!(nc.len(), g.stats.n_nets, "one copper row per net");

    // The GND plane is the dominant pour: it must be classified Poured (out of
    // the discrete-width check's reach), not flagged as a thin trace.
    let gnd = nc
        .iter()
        .find(|c| c.name == "GND")
        .expect("GND net present in copper table");
    assert_eq!(
        gnd.kind,
        GerberCopperKind::Poured,
        "the ground plane must be Poured (out of reach), not a discrete trace"
    );
    assert!(gnd.region_count > 0, "GND should carry pour regions");

    // Real routed signal traces exist and carry a finite, sane width: the board
    // has many `Traces` nets, and the narrowest discrete track is a plausible
    // fine-pitch signal width (not a degenerate zero, not absurdly wide).
    let routed: Vec<_> = nc
        .iter()
        .filter(|c| c.kind == GerberCopperKind::Traces && c.min_track_width_mm.is_some())
        .collect();
    assert!(
        routed.len() > 50,
        "expected many routed signal nets, got {}",
        routed.len()
    );
    let narrowest = routed
        .iter()
        .filter_map(|c| c.min_track_width_mm)
        .fold(f64::INFINITY, f64::min);
    assert!(
        (0.05..0.6).contains(&narrowest),
        "narrowest routed track {narrowest:.3} mm is out of the plausible signal range"
    );

    // The ampacity physics is the same one the native-CAD trace_current uses, so
    // a 0.122 mm 1 oz trace at 10 C rise rates ~0.5 A (cross-checked by hand /
    // standard trace-width calculators). This is just the engine, not a finding:
    // no cited current is attributed to a reconstructed net (gerbers carry no
    // names or BOM-bound identity here), so the check correctly fires nothing.
    let amp = galvani_extract::ipc2221_ampacity(0.122, 1.0, 10.0, true);
    assert!((amp - 0.52).abs() < 0.1, "0.122 mm ampacity was {amp:.2} A");
}
