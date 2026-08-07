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
    let e = from_gerber_dir(&job("gerber_slot_unplated")).expect("unplated slot fixture");
    assert_eq!(e.stats.n_holes, 0, "a mechanical slot has no plated wall");
    assert_eq!(e.stats.n_slots, 0);
    assert_eq!(
        e.stats.n_nets, 3,
        "an unplated slot must not be allowed to invent a net"
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
