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

/// The Inkplate films, in whichever corpus layout this machine has.
///
/// `<corpus>/famous/inkplate6_gerber` was joined directly, so a fetched corpus
/// (no `famous/` level) failed the guard below on the path rather than on the
/// reconstruction. `scripts/fetch-corpus.sh` also has to unpack the films: the
/// upstream repository carries them only inside a zip, and the manifest's
/// `unpack` field is what lands the .GTL/.GBL/.TXT set in this directory.
fn inkplate_dir() -> Option<PathBuf> {
    hauksbee_testkit::corpus_board(env!("CARGO_MANIFEST_DIR"), "famous/inkplate6_gerber")
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
    // guard, pinned exactly, and derived from the three drill files:
    //
    //   EPD_board-RoundHoles.TXT declares T1-T5, T7, T8, T10, T11 under
    //   `;TYPE=PLATED` and T12-T15 under `;TYPE=NON_PLATED`. Counting its body
    //   coordinate lines (modal, so a bare `Y....` is a hit too) per tool gives
    //   T1 16, T2 4, T3 459, T4 64, T5 11, T7 31, T8 2, T10 41, T11 2 = 630
    //   plated, and T12 4, T13 5, T14 4, T15 4 = 17 non-plated that are
    //   correctly excluded.
    //
    //   EPD_board-RectHoles.TXT (plated T9, C0.02756") and
    //   EPD_board-SlotHoles.TXT (plated T6, C0.01968") each hold two
    //   G00/M15/G01/M16 routs: a rapid to the start with the cutter up, a
    //   plunge, ONE cut segment, a retract. That is 4 routed plated slots, and
    //   each is one plated hit whose barrel is the stadium swept along the cut.
    //
    // So 630 + 4 = 634. This test previously pinned 638 because the reader
    // predated rout handling: it read both the `G00` rapid and the `G01` cut
    // endpoint of every rout as separate round hits, which turned one slot into
    // two phantom barrels and counted a cutter-up positioning move as a drilled
    // hole. The corpus files are fixed, so the exact pin is stable and catches
    // any dialect regression (a partial parse lands on a different number).
    assert_eq!(
        s.n_holes, 634,
        "Inkplate plated-hole count must be 630 round + 4 routed slots"
    );
    // The other side of the same pin: the four routs must arrive as slots. If
    // they ever revert to round hits, n_holes drifts and this drops to zero.
    assert_eq!(
        s.n_slots, 4,
        "the -RectHoles/-SlotHoles routs are 4 plated slots, not round hits"
    );
    // Both drill files declare a plain plated set with no layer pair, and the
    // job carries no partial-span drill, so nothing is refused: every plated
    // hit on this 2-layer board stitches top to bottom.
    assert_eq!(
        s.refused_span_holes, 0,
        "a 2-layer job with no multi-span drill must refuse no barrel"
    );

    // Connectivity reconstructs from copper alone; a dominant ground net exists.
    assert!(s.gnd_detected, "a GND-class net should be labelled");

    // Per-net copper is available, and the trace-current surface has real
    // widths to work with here: this Altium export draws its planes as G36/G37
    // filled regions and routes its signals with draw apertures, so `Poured`
    // and discrete-`Traces` nets both exist. (Both counts read differently
    // while the negative-pour bug stood: the board-sized dark region was on
    // every net, so every net came back `Poured` and none had a discrete width
    // to measure. That reading was a symptom, not a property of the export.)
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
    let poured = s
        .net_copper
        .iter()
        .filter(|c| c.kind == GerberCopperKind::Poured)
        .count();
    assert!(
        traces > 50,
        "the routed nets carry a measurable discrete width, got {traces}"
    );
    assert!(
        (2..traces).contains(&poured),
        "a handful of planes are poured and the rest are routed, got {poured} poured \
         against {traces} routed"
    );

    // No P&P in the published set, so no components are bound. This is the
    // documented honest limit, asserted so a future P&P addition is noticed.
    assert_eq!(
        s.n_components, 0,
        "the published Inkplate gerber set has no pick-and-place"
    );
}

/// The net count this exact film set currently reconstructs to.
///
/// This was a standing known gap: the Inkplate collapsed to 18 connected
/// components because its Altium films draw negative planes and the reader
/// discarded their clear-polarity geometry. Applying those painter operations
/// restores the intended plane-segmentation class. The exact hand-auditable
/// mechanism tests live in `tests/gerber_negative_pour.rs`; this row deliberately
/// makes no claim about the board's true final net count because the published
/// manufacturing set provides no native netlist oracle.
#[test]
fn inkplate6_reconstruction_count_is_stable_not_an_oracle() {
    let Some(dir) = inkplate_dir() else {
        return;
    };
    let g = from_gerber_dir(&dir).expect("Inkplate gerbers must reverse-extract");
    // This is a deterministic regression pin, not a native-net correctness
    // oracle: the published manufacturing set has no netlist or placement file.
    // True Boolean painter replay plus exact finite-width contact currently yields
    // 179 (the earlier signed/enclosure implementation yielded 181). Neither is a
    // correctness measurement without the missing native netlist; this row only
    // catches an unreviewed whole-film drift.
    assert_eq!(
        g.stats.n_nets, 179,
        "the exact film-set regression pin moved"
    );
}

/// The SparkFun RP2040 Thing Plus panel, an Eagle export whose films draw copper as
/// filled regions and ring every off-net pad with clearance.
///
/// It is the second board the negative-pour cut moved. Eagle's clearance rings
/// are exactly the clear painter operations that were being dropped, but this
/// published set has no drill and no placement map, so its total cannot serve as
/// a native connectivity oracle. The exact output is retained only as a
/// deterministic whole-board regression pin.
///
/// This board also ships `RP2040_Thing_Plus-Panel.brd` beside its gerbers, and this
/// crate reads Eagle binaries, so a pin-to-net partition comparison against the
/// native layout is the gate that would settle the count outright. It needs the
/// per-net copper GEOMETRY to be reachable from `ReconStats` (the published set has
/// no pick-and-place, so no pads bind and there is nothing to compare pad-wise), and
/// that API does not exist yet. Left as the highest-value test to add in this area.
#[test]
fn sparkfun_rp2040_panel_count_is_stable_not_an_oracle() {
    let Some(dir) = hauksbee_testkit::corpus_board(
        env!("CARGO_MANIFEST_DIR"),
        "sparkfun_thingplus_rp2040/Hardware/Production",
    ) else {
        return;
    };
    let g = from_gerber_dir(&dir).expect("SparkFun panel gerbers must reverse-extract");
    assert_eq!(g.stats.n_layers, 2, "the published set is GTL plus GBL");
    assert_eq!(
        g.stats.n_holes, 0,
        "no drill ships with it, so the two layers cannot stitch"
    );
    // The earlier signed/enclosure implementation yielded 3,438. The exact
    // painter/contact implementation yields 3,378; without drill, placement, or
    // native partition data neither number is an accuracy claim.
    assert_eq!(
        g.stats.n_nets, 3378,
        "the exact published-film regression pin moved"
    );
}
