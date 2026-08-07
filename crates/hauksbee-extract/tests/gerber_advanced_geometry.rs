//! Synthesized acceptance gates for C1.4 advanced Gerber geometry.
//!
//! The checked-in native KiCad board records the intended slot, castellation,
//! blind-via, and buried-via constructions. The fab files beside it are a
//! deliberately adversarial four-layer partition: top and bottom copper cross
//! both drill coordinates, while X2 says the drills stop at L1-L2 and L2-L3.
//! Treating either hit as a through-hole fabricates a short between three
//! conductors.

use std::path::{Path, PathBuf};

use hauksbee_extract::gerber::from_gerber_dir;

const EXPECTED_COPPER_LAYERS: usize = 4;
const EXPECTED_PLATED_HOLES: usize = 2;
const EXPECTED_NETS_WITH_DECLARED_SPANS: usize = 3;

fn job(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn fixture() -> PathBuf {
    job("gerber_advanced")
}

#[test]
fn x2_drill_spans_do_not_become_through_holes() {
    let extraction = from_gerber_dir(&fixture()).expect("advanced Gerber fixture must extract");

    assert_eq!(extraction.stats.n_layers, EXPECTED_COPPER_LAYERS);
    assert_eq!(extraction.stats.n_holes, EXPECTED_PLATED_HOLES);
    assert_eq!(
        extraction.stats.n_nets, EXPECTED_NETS_WITH_DECLARED_SPANS,
        "L1-L2 and L2-L3 plated hits must not stitch the full four-layer stack"
    );
}

#[test]
fn a_plated_slot_connects_what_its_wall_touches() {
    // Two pads 6 mm apart on the top layer with no copper between them, and a
    // G85 slot routed from one to the other. The plated wall is the conductor,
    // so the pads are one net. The bottom-layer pad 6 mm away from the slot is
    // untouched and stays its own net, which is what stops this passing on a
    // reader that simply merges everything.
    let e = from_gerber_dir(&job("gerber_slot_plated")).expect("plated slot fixture");
    assert_eq!(e.stats.n_holes, 1);
    assert_eq!(
        e.stats.n_slots, 1,
        "the G85 record is a slot, not a round hole"
    );
    assert_eq!(e.stats.n_nets, 2, "the slot wall joins both top pads");
    assert!(
        e.stats.notes.is_empty(),
        "nothing to refuse here: {:?}",
        e.stats.notes
    );
}

#[test]
fn an_unplated_slot_of_the_same_shape_connects_nothing() {
    // The lookalike. Identical copper, identical slot path, identical tool: the
    // ONLY difference is that the file describes the slot as an outline cut
    // rather than a plated one. It must connect nothing, so the two top pads
    // stay separate and the job reports three nets instead of two.
    //
    // The drill file is named `slot-mechanical.drl` on purpose. A name carrying
    // `NPTH` would be caught by the file-name filter before the body was even
    // read, so the test would pass on a reader that ignores the declaration
    // entirely. Here the `NonPlated` attribute in the body is the only thing
    // standing between this slot and a fabricated net.
    let e = from_gerber_dir(&job("gerber_slot_unplated")).expect("unplated slot fixture");
    assert_eq!(e.stats.n_holes, 0, "a mechanical slot has no plated wall");
    assert_eq!(e.stats.n_slots, 0);
    assert_eq!(
        e.stats.n_nets, 3,
        "an unplated slot must not be allowed to invent a net"
    );
}

#[test]
fn a_slot_flashed_on_a_drill_film_connects_along_its_whole_length() {
    // A drill film draws the finished cutout, so this slot arrives as a single
    // 4 mm by 1 mm obround flash centred at (5, 5). The tool is the narrow
    // side, 1 mm, and it swept the long axis, so the plated wall spans x from
    // 3.0 to 7.0.
    //
    // The two pads sit at (3.2, 5) and (6.8, 5), reaching in to x = 3.4 and
    // 6.6. Nothing else on the board connects them. They are on the wall, and
    // they are 1.1 mm clear of the inscribed circle that a reader taking only
    // the narrow dimension would place at the flash's centre. So this fixture
    // separates the two models: one net if the slot is recovered as the
    // stadium it is, three if it is reduced to a circle in the middle.
    let e = from_gerber_dir(&job("gerber_film_slot")).expect("film slot fixture");
    assert_eq!(e.stats.n_holes, 1);
    assert_eq!(e.stats.n_slots, 1, "an oblong drill flash is a slot");
    assert_eq!(
        e.stats.n_nets, 1,
        "the plated wall runs the length of the slot, so both pads are one conductor"
    );
}

#[test]
fn the_same_flash_declared_mechanical_connects_nothing() {
    // Identical copper, identical obround flash, identical geometry. The only
    // difference is that the film declares its drill aperture
    // `%TA.AperFunction,MechanicalDrill`: a cutout with no plating, so no wall
    // and no conductor. The two pads stay apart.
    let e =
        from_gerber_dir(&job("gerber_film_slot_mechanical")).expect("mechanical film slot fixture");
    assert_eq!(e.stats.n_holes, 0, "a mechanical cutout is not a conductor");
    assert_eq!(e.stats.n_slots, 0);
    assert_eq!(
        e.stats.n_nets, 2,
        "the pads are not joined by a mechanical slot"
    );
}

#[test]
fn a_drill_film_that_never_says_whether_it_is_plated_abstains_out_loud() {
    // The same flash again, on a film with no `TF.FileFunction`, no aperture
    // function, a name that says nothing, and no NPTH sibling to imply this is
    // the plated set. Plating is the difference between a conductor and a hole
    // in the board, and nothing here states it.
    //
    // Guessing plated invents a net; guessing mechanical deletes one. So the
    // hits are dropped, the pads stay apart, and the reader names the file and
    // says what would settle it.
    let e = from_gerber_dir(&job("gerber_plating_unknown")).expect("unknown plating fixture");
    assert_eq!(e.stats.n_holes, 0);
    assert_eq!(e.stats.refused_plating_files, 1);
    assert_eq!(e.stats.n_nets, 2, "nothing may be joined on a guess");
    assert!(
        e.stats
            .notes
            .iter()
            .any(|n| n.contains("unk-drill.gbr") && n.contains("plated")),
        "the abstention must name the file: {:?}",
        e.stats.notes
    );
}

#[test]
fn a_castellation_is_one_node_with_its_pad_and_is_counted() {
    // A plated half-hole on the board edge at (0, 5): the outline runs straight
    // through the barrel, so the copper ring is cut and no closed ring contains
    // the hole. The pad, the barrel and the bottom-side pad are one conductor.
    //
    // The same job also carries a MECHANICAL edge slot at (20, 5) cutting the
    // second pad pair. That one is an outline feature, so it joins nothing and
    // is not a castellation: the two pads it passes through stay on separate
    // nets, and the castellation count stays at one.
    let e = from_gerber_dir(&job("gerber_castellation")).expect("castellation fixture");
    assert_eq!(
        e.stats.n_holes, 1,
        "only the plated edge hit is a conductor"
    );
    assert_eq!(
        e.stats.n_castellations, 1,
        "the plated edge hit is cut by the outline; the mechanical one is not plated"
    );
    assert_eq!(
        e.stats.n_nets, 3,
        "castellated pair = 1 net, plus the two pads the mechanical slot leaves apart"
    );
}

#[test]
fn a_drill_file_with_no_readable_span_in_a_multi_span_job_refuses() {
    // Four layers, a pad at one point on every layer, and two plated drill
    // files that both hit it. One names its pair (L1-L2); the other says
    // nothing at all. On a job that demonstrably uses more than one span,
    // "says nothing" is not "through-hole": reading it as one would merge all
    // four layers into a single net that the stackup does not contain.
    //
    // So the L1-L2 hit stitches its two layers and the silent hit stitches
    // none, leaving three nets, one refusal, and a note that names the file.
    let e = from_gerber_dir(&job("gerber_span_refusal")).expect("span refusal fixture");
    assert_eq!(e.stats.n_layers, 4);
    assert_eq!(e.stats.n_holes, 2, "both hits are still reported");
    assert_eq!(e.stats.refused_span_holes, 1);
    assert_eq!(
        e.stats.n_nets, 3,
        "L1+L2 join; L3 and L4 stay apart because the second span is unknown"
    );
    assert!(
        e.stats
            .notes
            .iter()
            .any(|n| n.contains("refuse-PTH-extra.drl")),
        "the refusal must name the file it applies to: {:?}",
        e.stats.notes
    );
}

#[test]
fn a_lone_blind_via_declaration_does_not_widen_to_the_whole_stack() {
    // Four copper layers, a pad on each at one point, and a SINGLE plated drill
    // declaring `Plated,1,2,PTH`. That is a blind via reaching the top two
    // layers, so the answer is three nets: {L1, L2}, {L3}, {L4}.
    //
    // The trap this catches is a reader that works out the board's layer count
    // from the drill declarations alone. With only this file to go on, the
    // deepest layer any drill names is 2, so `1,2` looks like the full depth of
    // the board and the hit gets stitched through every film: one net, built
    // out of a declaration that said the opposite. The copper films are
    // evidence about the stack too, and here they are the evidence that says
    // this board has four layers.
    let e = from_gerber_dir(&job("gerber_lone_blind")).expect("lone blind via fixture");
    assert_eq!(e.stats.n_layers, 4);
    assert_eq!(e.stats.n_holes, 1);
    assert_eq!(
        e.stats.refused_span_holes, 0,
        "the span is perfectly readable"
    );
    assert_eq!(
        e.stats.n_nets, 3,
        "a lone L1-L2 declaration is a blind via, not a through-hole"
    );
    assert!(
        e.stats.notes.is_empty(),
        "nothing is missing or ambiguous here: {:?}",
        e.stats.notes
    );
}

#[test]
fn a_routed_arc_wall_connects_what_the_curve_touches_and_not_what_its_chord_does() {
    // A quarter-circle routed slot of radius 5 mm, cut counter-clockwise from
    // (5, 0) to (0, 5) with a 0.8 mm cutter, so the plated wall occupies the
    // band from radius 4.6 to 5.4. Five pads on the top copper:
    //
    //   A (5, 0) and B (0, 5)  at the two ends of the cut
    //   C  outside the wall on the 45-degree ray, touching it
    //   D  at (2.5, 2.5), the midpoint of the CHORD, 1.46 mm inside the arc
    //   E  on the 33.75-degree ray, reaching out to radius 4.55
    //
    // C, D and E are the test, and they fail three different wrong arcs.
    //
    // C must join. It is 1.35 mm clear of the straight chord, so a reader that
    // replaces the arc with its chord loses it.
    //
    // D must not join. It sits on the chord and is 0.38 mm clear of the true
    // wall, so the same straightening invents a connection to it.
    //
    // E must not join either, and it is the subtle one. It reaches to radius
    // 4.55, inside the real wall at 4.6, but a coarse tessellation puts its
    // chords at radius 4.90 whose 0.4 mm-wide stadiums reach in to 4.50 and
    // swallow it. Tessellating an arc into chords always over-reaches on the
    // concave side; E fails unless that over-reach is held under the tolerance
    // the union itself works to.
    let e = from_gerber_dir(&job("gerber_rout_arc")).expect("rout arc fixture");
    assert!(
        e.stats.n_slots >= 4,
        "the arc must tessellate: {} segment(s)",
        e.stats.n_slots
    );
    assert_eq!(
        e.stats.n_nets, 3,
        "A, B and C are one conductor through the arc wall; D and E are each on their own"
    );
}

#[test]
fn a_blind_via_on_a_gapped_stack_is_refused_rather_than_placed_by_position() {
    // Two copper films, but they say what they are: the top declares
    // `Copper,L1,Top` and the bottom declares `Copper,L4,Bot`. So this is a
    // four-layer board whose two inner films did not reach us. The drill
    // declares `Plated,1,2,PTH`: a blind via from the top to the first inner
    // layer, which is one of the films we do not have.
    //
    // The trap is that our films are numbered densely, 0 and 1, so the bottom
    // film IS "index 1" in the reader's own numbering. Indexing the pair `1,2`
    // straight into that shorts the top of the board to the bottom of it, off a
    // declaration that said the via stops at the second layer. The films'
    // own layer numbers are what rule that out: layer 2 is not among them, so
    // the via cannot be placed and is refused.
    let e = from_gerber_dir(&job("gerber_gapped_blind")).expect("gapped blind fixture");
    assert_eq!(e.stats.n_layers, 2, "only two films classified");
    assert_eq!(e.stats.n_holes, 1);
    assert_eq!(e.stats.refused_span_holes, 1);
    assert_eq!(
        e.stats.n_nets, 2,
        "a blind via to a missing layer must not join the outer films"
    );
    assert!(
        e.stats.notes.iter().any(|n| n.contains("gap-PTH.drl")),
        "the refusal must name the file: {:?}",
        e.stats.notes
    );
}

#[test]
fn a_declared_span_naming_a_layer_this_job_lacks_refuses_end_to_end() {
    // Two copper layers, a pad on each at the same point, and ONE plated drill
    // file whose X2 attribute says `Plated,3,6,PTH`: a buried via running from
    // layer 3 to layer 6 of a board whose inner films this job does not carry.
    //
    // Two tempting shortcuts both fabricate a net here. Treating the unusable
    // declaration as silence, then noticing there is only one drill file, reads
    // it as a through-hole. Clamping `3,6` onto the two layers we have does the
    // same thing by another route. Either way a buried via that touches neither
    // outer layer ends up joining both of them.
    //
    // Note the contrast with a `1,<deepest>` declaration, which IS a
    // through-hole and stays one however few films we classified: the
    // difference is that `3,6` names a position in a stack we cannot locate.
    let e = from_gerber_dir(&job("gerber_span_out_of_range")).expect("out-of-range span fixture");
    assert_eq!(e.stats.n_layers, 2);
    assert_eq!(e.stats.n_holes, 1);
    assert_eq!(e.stats.refused_span_holes, 1);
    assert_eq!(
        e.stats.n_nets, 2,
        "an unusable declaration must not be widened into a through-hole"
    );
    assert!(
        e.stats.notes.iter().any(|n| n.contains("oor-PTH.drl")),
        "the refusal must name the file: {:?}",
        e.stats.notes
    );
    // The job also has to say that it is looking at a 6-layer board through
    // two films, because that missing copper is the underlying problem and it
    // costs more than the one refused hit.
    assert!(
        e.stats
            .notes
            .iter()
            .any(|n| n.contains("6-layer board") && n.contains("2 copper layer")),
        "the missing copper layers must be reported: {:?}",
        e.stats.notes
    );
}

#[test]
fn a_full_depth_span_survives_a_job_whose_inner_films_did_not_classify() {
    // The counterpart, and the one a real board hits. KiCad names an inner
    // layer's film after the user's label ("GND_Cu", "Power_Cu"), so a six-layer
    // job can classify only its two outer films. Its drill still declares
    // `Plated,1,6,PTH`, and that declaration means the hit goes through the
    // whole board, which stays true whichever films we recognised.
    //
    // Refusing it because "layer 6 is not in our stack" costs every
    // through-hole on the board. The reform motherboard closed-loop row is
    // exactly this case and dropped from 99.7% to 97.2% net partition while
    // this reader got it wrong.
    let e = from_gerber_dir(&job("gerber_span_full_depth")).expect("full-depth span fixture");
    assert_eq!(e.stats.n_layers, 2, "only the two outer films classified");
    assert_eq!(
        e.stats.refused_span_holes, 0,
        "a full-depth hit is not refused"
    );
    assert_eq!(
        e.stats.n_nets, 1,
        "the through-hole joins the two films this job does carry"
    );
    assert!(
        e.stats
            .notes
            .iter()
            .any(|n| n.contains("6-layer board") && n.contains("2 copper layer")),
        "the missing copper layers must still be reported: {:?}",
        e.stats.notes
    );
}
