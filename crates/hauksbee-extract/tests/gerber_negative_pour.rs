//! A copper pour drawn *negatively*: one board-sized dark region followed by
//! `%LPC*%` voids.
//!
//! Altium 24 plots every plane and pour this way, and it is the shape that made
//! a real four-layer STM32 CAN devboard (ARDEP mainboard rev 1.1, 1924 aperture
//! flashes, four copper films) reconstruct to exactly ONE net: reading the dark
//! region and discarding the ~390 clear regions left a solid sheet of copper on
//! every signal layer, so the union-find had nothing to separate. With the
//! voids cut, that job reconstructs to 284 nets. The gate here is the distilled
//! mechanism, not the board.

use std::path::{Path, PathBuf};

use hauksbee_extract::gerber::from_gerber_dir;

fn job(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn voids_in_a_negative_pour_separate_the_copper_they_ring() {
    // A 20x20 mm dark region, then two 4x4 mm voids, then a 1 mm pad flashed in
    // the middle of each void. Three conductors: the pour, and one pad apiece.
    // A reader that drops the voids sees one sheet of copper and reports 1 net.
    let e = from_gerber_dir(&job("gerber_negative_pour")).expect("negative pour fixture");
    assert_eq!(e.stats.n_layers, 1);
    assert_eq!(
        e.stats.n_nets, 3,
        "the pour plus two voided pads are three conductors, not one sheet"
    );
}

#[test]
fn an_antipad_island_is_its_own_net_not_the_planes() {
    // The negative plane's other half. Where the previous test's voids ring
    // nothing, an antipad drawn as an ANNULAR clear flash rings a 2 mm island of
    // plane copper free: that island is the through-hole's annular pad, wired to
    // the bottom-side track through the plated barrel, and it is emphatically not
    // on the plane.
    //
    // The plane, and the via net (island + barrel + two bottom pads + track), are
    // two nets. Reading it as one is the merge in miniature, and it happens by
    // either of two mistakes: dropping the clear (the plane is then solid and
    // swallows everything the barrel reaches) or cutting the ring but leaving the
    // island inside the plane's own primitive, since connectivity unions per
    // primitive.
    let e = from_gerber_dir(&job("gerber_negative_antipad")).expect("negative antipad fixture");
    assert_eq!(e.stats.n_layers, 2);
    assert_eq!(
        e.stats.n_holes, 1,
        "one plated barrel stitches the two layers"
    );
    assert_eq!(
        e.stats.n_nets, 2,
        "the plane and the through-hole net are two conductors"
    );
}

#[test]
fn a_pad_the_voids_leave_bridged_stays_on_the_pour() {
    // The lookalike, and the side that stops the fix from simply splitting
    // everything. Identical pour, identical pads, identical void around the
    // left pad; the right pad's void is drawn as two bars with a 1 mm strip of
    // copper left standing between them, exactly how a thermal relief reaches a
    // pad. That pad IS on the pour, so there are two conductors, not three.
    let e = from_gerber_dir(&job("gerber_negative_pour_thermal")).expect("thermal fixture");
    assert_eq!(e.stats.n_layers, 1);
    assert_eq!(
        e.stats.n_nets, 2,
        "copper the voids leave standing still joins the pad to the pour"
    );
}
