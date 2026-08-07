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

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("gerber_advanced")
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
