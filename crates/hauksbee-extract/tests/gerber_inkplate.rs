//! Real-world regression: the Soldered/e-radionica Inkplate 6 main board (EPD
//! board) reverse-extracted from its published Altium gerbers + Excellon drill.
//!
//! The Inkplate ships manufacturing files in the Altium dialect, which exercises
//! parts of the readers no other corpus board does and which initially failed:
//!   - Protel-extension copper (`.GTL`/`.GBL`), 2-layer.
//!   - An Excellon drill named `EPD_board-RoundHoles.TXT` (the `holes` token, not
//!     `drill`/`drl`, so the classifier had to learn it) with the Altium body
//!     dialect: `;FILE_FORMAT=2:5`, `INCH,LZ`, `T1F00S00C...` tool defs (feed/
//!     speed before the diameter), MODAL single-axis coordinate lines (`X..` keeps
//!     the last Y), and `;TYPE=PLATED` / `;TYPE=NON_PLATED` sections. Before the
//!     reader fix this drill yielded ZERO holes, so the two layers never stitched.
//!
//! There is NO pick-and-place in the published gerber set, so components cannot
//! be bound (documented limit in docs/ingest/GERBER.md: "no P&P -> nets and geometry
//! reconstruct from copper alone, but components cannot be bound"). This test
//! therefore asserts the connectivity reconstruction (nets, drill stitching,
//! per-net copper), not component binding.
//!
//! Skipped when the corpus is absent; required under `HAUKSBEE_REQUIRE_CORPUS=1`.

use std::path::PathBuf;

use hauksbee_extract::gerber::connect::GerberCopperKind;
use hauksbee_extract::gerber::from_gerber_dir;

fn inkplate_dir() -> Option<PathBuf> {
    let p = hauksbee_testkit::corpus_dir(env!("CARGO_MANIFEST_DIR"))
        .unwrap_or_default()
        .join("famous/inkplate6_gerber");
    p.exists().then_some(p)
}

#[test]
fn inkplate6_reconstructs_with_altium_drill_stitching() {
    let Some(dir) = inkplate_dir() else {
        if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
            panic!("corpus required but inkplate6_gerber missing");
        }
        eprintln!("skipping Inkplate (corpus absent)");
        return;
    };

    let g = from_gerber_dir(&dir).expect("Inkplate gerbers must reverse-extract");
    let s = &g.stats;
    eprintln!(
        "Inkplate6: {} layers, {} holes, {} nets, GND={}",
        s.n_layers, s.n_holes, s.n_nets, s.gnd_detected
    );

    // 2-layer board (GTL + GBL).
    assert_eq!(s.n_layers, 2, "Inkplate is a 2-layer board");

    // The Altium drill must parse. Before the reader fix (modal coords,
    // FILE_FORMAT=2:5, T<idx>F..S..C.. tool defs, TYPE sections) this was ZERO
    // and the two layers did not stitch. This is the load-bearing regression
    // guard, pinned exactly: 630 round plated holes (EPD_board-RoundHoles.TXT)
    // plus the 8 endpoints of the 4 routed plated slots (-RectHoles/-SlotHoles,
    // each a G00/M15/G01/M16 rout whose two endpoints stitch as plated barrels).
    // The corpus file is fixed, so an exact pin is stable and catches any
    // dialect regression (a partial parse lands on a different number).
    assert_eq!(
        s.n_holes, 638,
        "Inkplate plated-hole count must be 630 round + 8 slot endpoints"
    );

    // Connectivity reconstructs from copper alone; a dominant ground net exists.
    assert!(s.gnd_detected, "a GND-class net should be labelled");
    assert!(s.n_nets > 100, "nets reconstructed: {}", s.n_nets);

    // Per-net copper is available (the gerber trace-current surface), and it
    // pins a genuine honest limit: Altium's gerber export for this board draws
    // ALL copper - traces included - as G36/G37 filled REGIONS, not as draw-
    // aperture tracks. So every net reconstructs as `Poured` (or `None` for a
    // net with only pad flashes), and the discrete-track-width check has no
    // discrete width to measure on this board. That is the SAFE failure
    // direction (a Poured net is never flagged, so there is no false positive),
    // and it is the documented reason the trace-current surface is inert here
    // while it runs on the Allegro uConsole (whose traces are draw-aperture
    // tracks). Asserted so the characteristic is on the record, not a surprise.
    let gnd = s
        .net_copper
        .iter()
        .find(|c| c.name == "GND")
        .expect("GND copper row present");
    assert_eq!(
        gnd.kind,
        GerberCopperKind::Poured,
        "GND plane must be Poured"
    );
    let traces = s
        .net_copper
        .iter()
        .filter(|c| c.kind == GerberCopperKind::Traces)
        .count();
    assert_eq!(
        traces, 0,
        "this Altium export draws traces as regions, so no net is discrete-Traces"
    );
    let poured = s
        .net_copper
        .iter()
        .filter(|c| c.kind == GerberCopperKind::Poured)
        .count();
    assert!(
        poured > 100,
        "copper should be region-dominated, got {poured} poured"
    );

    // No P&P in the published set, so no components are bound. This is the
    // documented honest limit, asserted so a future P&P addition is noticed.
    assert_eq!(
        s.n_components, 0,
        "the published Inkplate gerber set has no pick-and-place"
    );
}
