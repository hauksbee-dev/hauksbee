//! The boards the landing page offers must look like boards, and work.
//!
//! A sample is the first thing a stranger clicks. On a tool whose whole claim
//! is that it starts from the copper, a sample with no copper argues against
//! the product, and two of the three shipped with none: blinky and boot_gate
//! were pad-and-netlist fixtures with zero segments, zero vias and no board
//! outline. They are routed now, and this stops that regressing.
//!
//! It also gates what the samples bind. A sample exists to show the tool
//! working, so one that opens with "these parts have no model" demonstrates
//! the opposite. That floor is deliberately a floor and not "everything must
//! bind": watchy is a real shipped product with parts nobody has modelled yet,
//! and pretending otherwise would mean either fabricating models or dropping
//! the most convincing board we have. What the floor prevents is the number
//! going BACKWARDS unnoticed.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

/// Count occurrences, not lines: these files are written compactly, so a
/// line-based grep reports 1 for a board carrying twenty-eight segments.
fn count(hay: &str, needle: &str) -> usize {
    hay.matches(needle).count()
}

/// Every board offered as a one-click sample on the landing page.
const SAMPLES: &[&str] = &[
    "frontend/public/samples/blinky.kicad_pcb",
    "frontend/public/samples/boot_gate.kicad_pcb",
    "frontend/public/samples/watchy.kicad_pcb",
];

#[test]
fn every_sample_board_has_real_copper_and_an_outline() {
    for rel in SAMPLES {
        let path = repo_root().join(rel);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{rel} is offered as a sample but cannot be read: {e}"));
        let segments = count(&text, "(segment");
        let edge = count(&text, "Edge.Cuts");
        assert!(
            segments > 0,
            "{rel} has no copper traces. It is offered as a sample on a tool that claims \
             to start from the copper, so shipping a pad cloud argues against the product. \
             Route it: `hauksbee to-code <board> --out b.board` then `hauksbee from-code \
             b.board --out <board> --relayout --route`."
        );
        assert!(
            edge > 0,
            "{rel} has no board outline, so it does not render as a board at all"
        );
    }
}

/// The samples must stay in step with the fixtures they were copied from.
///
/// One deliberate exception, recorded in frontend/public/samples/README.md:
/// boot_gate's fixture keeps two GND/+5V shorts because waiver_gate.rs asserts
/// it gates red, and that premise is what stops the waiver tests passing
/// vacuously. The sample is the same board routed clean. Any OTHER divergence
/// is a stale copy, which is how a sample quietly stops matching the thing it
/// is meant to demonstrate.
#[test]
fn samples_match_their_fixtures_except_where_documented() {
    let root = repo_root();
    for (sample, fixture) in [
        (
            "frontend/public/samples/blinky.kicad_pcb",
            "crates/hauksbee-ci/examples/boards/blinky.kicad_pcb",
        ),
        (
            "frontend/public/samples/watchy.kicad_pcb",
            "crates/hauksbee-ci/examples/boards/watchy.kicad_pcb",
        ),
    ] {
        let a = std::fs::read(root.join(sample)).expect("sample readable");
        let b = std::fs::read(root.join(fixture)).expect("fixture readable");
        assert_eq!(
            a, b,
            "{sample} has drifted from {fixture}. Re-copy it, or if the difference is \
             deliberate, record it in frontend/public/samples/README.md and exempt it here \
             the way boot_gate is."
        );
    }
}
