//! The static check must scale with the board, not with its square.
//!
//! `hauksbee check` on the ~800-IC Tarski board went from a few seconds to
//! about 42 s when the DC-operating-point rating check landed. The DC solve was
//! not the cost: the check had become quadratic in the part count (every bind
//! re-resolved every part against the whole library, three binds per check,
//! each meta's rating paths scanned the whole circuit, evidence rendering and
//! the net lint asked every map or component per net). Each piece is invisible
//! on a ten-part fixture, so nothing went red.
//!
//! This guard runs the full engine lint on a synthetic board of `SMALL` and of
//! `4 * SMALL` identical, independent driver stages and asserts the time ratio.
//! Linear work predicts 4x, quadratic 16x; the bound sits between with room for
//! fixed costs and noise. It compares a RATIO of best-of-`REPEATS` runs taken
//! interleaved, so machine load scales both sides and cancels (the
//! `speed_gate` method), and each run gets a fresh model library so a
//! resolution memo warmed by an earlier run cannot hide a quadratic bind.
//!
//! Run: `cargo nextest run -p hauksbee-engine --test it static_check_scaling`.

use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

/// Stages on the small board; the large one has four times as many.
const SMALL: usize = 12;
/// Interleaved runs per size, keeping the fastest.
const REPEATS: usize = 3;
/// Largest accepted `large / small` time ratio. Measured 3.55x (load ~60,
/// 2026-10-05); linear is 4x, quadratic 16x.
const MAX_RATIO: f64 = 9.0;

/// `cells` copies of a 74HC04 output driving a BC847 base through 10k with a
/// 1k collector pull-up, on shared `GND` and `+5V` rails.
fn board(cells: usize) -> String {
    let mut pcb = String::from(
        "(kicad_pcb (version 20171130) (host pcbnew 5.1.0)\n  (net 0 \"\")\n  (net 1 \"GND\")\n  (net 2 \"+5V\")\n",
    );
    for k in 0..cells {
        let n = 3 + 3 * k;
        let _ = writeln!(
            pcb,
            "  (net {n} \"DRIVE{k}\")\n  (net {} \"BASE{k}\")\n  (net {} \"COLL{k}\")",
            n + 1,
            n + 2
        );
    }
    let part = |pcb: &mut String,
                lib: &str,
                at: usize,
                reference: &str,
                value: &str,
                pads: &[(u8, usize, &str)]| {
        let _ = writeln!(
            pcb,
            "  (module {lib} (layer F.Cu)\n    (at {at} 100)\n    (fp_text reference {reference} (at 0 0) (layer F.SilkS))\n    (fp_text value {value} (at 0 2) (layer F.Fab))"
        );
        for (i, &(pad, net, name)) in pads.iter().enumerate() {
            let _ = writeln!(
                pcb,
                "    (pad {pad} smd rect (at {i} 0) (net {net} \"{name}\"))"
            );
        }
        pcb.push_str("  )\n");
    };
    for k in 0..cells {
        let n = 3 + 3 * k;
        let (drive, base, coll) = (format!("DRIVE{k}"), format!("BASE{k}"), format!("COLL{k}"));
        let at = 100 + 40 * k;
        let soic = "Package_SO:SOIC-14_3.9x8.7mm_P1.27mm";
        let r0603 = "Resistor_SMD:R_0603_1608Metric";
        part(
            &mut pcb,
            soic,
            at,
            &format!("U{}", k + 1),
            "74HC04",
            &[(1, 1, "GND"), (2, n, &drive), (7, 1, "GND"), (14, 2, "+5V")],
        );
        part(
            &mut pcb,
            r0603,
            at + 10,
            &format!("R{}", 2 * k + 1),
            "10k",
            &[(1, n, &drive), (2, n + 1, &base)],
        );
        part(
            &mut pcb,
            "Package_TO_SOT_SMD:SOT-23",
            at + 20,
            &format!("Q{}", k + 1),
            "BC847",
            &[(1, n + 1, &base), (2, 1, "GND"), (3, n + 2, &coll)],
        );
        part(
            &mut pcb,
            r0603,
            at + 30,
            &format!("R{}", 2 * k + 2),
            "1k",
            &[(1, 2, "+5V"), (2, n + 2, &coll)],
        );
    }
    pcb.push_str(")\n");
    pcb
}

/// One full engine lint of `board`, timed, with a library no earlier run warmed.
fn lint_time(board: &ExtractedBoard) -> Duration {
    let lib = ModelLibrary::builtin();
    let start = Instant::now();
    let report = hauksbee_engine::checks::engine_lint(board, &lib);
    let took = start.elapsed();
    std::hint::black_box(report);
    took
}

#[test]
fn the_static_check_scales_linearly_with_the_board() {
    let small = ExtractedBoard::from_auto(&board(SMALL)).expect("parse small board");
    let large = ExtractedBoard::from_auto(&board(4 * SMALL)).expect("parse large board");
    let (mut best_small, mut best_large) = (Duration::MAX, Duration::MAX);
    for _ in 0..REPEATS {
        best_small = best_small.min(lint_time(&small));
        best_large = best_large.min(lint_time(&large));
    }
    let ratio = best_large.as_secs_f64() / best_small.as_secs_f64().max(1e-9);
    eprintln!(
        "static check: {SMALL} stages {best_small:?}, {} stages {best_large:?}, ratio {ratio:.2}x (max {MAX_RATIO}x)",
        4 * SMALL
    );
    assert!(
        ratio <= MAX_RATIO,
        "the static check took {ratio:.2}x as long on a board 4x the size ({best_small:?} -> \
         {best_large:?}); linear is 4x, quadratic 16x. Some pass has gone superlinear in the part \
         count: profile `hauksbee check` on a large board"
    );
}
