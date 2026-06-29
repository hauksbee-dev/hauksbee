//! Design-rule / physics checks that run against a parsed or solved board.
//!
//! Each check is self-contained: it takes an
//! [`ExtractedBoard`](hauksbee_extract::ExtractedBoard) (and, where it needs
//! physics, builds and solves its own [`Circuit`](hauksbee_ir::Circuit)), and
//! returns a verdict plus the numbers behind it. They are kept separate from
//! the bind-time [`stress`](crate::stress) monitor: stress watches a running
//! co-simulation against datasheet ratings, whereas a check answers a specific
//! standards question about the design.
//!
//! - [`usb_c`]: the USB Type-C CC attach classifier. It attaches a generic
//!   source + cable model to a board's CC termination and classifies the result
//!   against the USB Type-C spec windows (Sink / AudioAccessory / ...). This is
//!   what re-derives the RPi 4 shared-CC-pulldown fault cold.
//! - [`straps`]: the boot strapping-pin lint. It reads each MCU's strap table
//!   from the model db and flags a strap net that cannot hold the level the part
//!   needs at reset (a free-running clock on it, or a pull to the wrong rail).
//! - [`mcu_coverage`]: flags a recognised MCU that has no authored device model,
//!   so the strap and resource-conflict checks above could not run on it —
//!   keeping a "Looks healthy" verdict from being printed over a recognised MCU
//!   the tool never modelled.

pub mod mcu_coverage;
pub mod straps;
pub mod usb_c;

use hauksbee_extract::{ExtractedBoard, NetLintReport};
use hauksbee_models::ModelLibrary;

/// The full engine-level lint: the connectivity net-lint plus the three
/// model-aware checks — strap pins, MCU resource conflicts, and the
/// unmodelled-MCU coverage note. Kept as one function so every surface (`--lint`,
/// `--check`, the JSON aggregate) runs the identical set and no caller can
/// reopen the "Looks healthy over an unexamined MCU" hole by forgetting one.
pub fn engine_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = board.net_lint();
    report
        .findings
        .extend(straps::strap_lint(board, lib).findings);
    report
        .findings
        .extend(resources_lint(board, lib).findings);
    report
}

/// The `--resources` view: the MCU internal resource-conflict check PLUS the
/// unchecked-strap-bearing-MCU coverage note. A named chokepoint so neither the
/// `--resources` path nor `engine_lint` can drop the coverage note by forgetting
/// to append it.
pub fn resources_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = board.resource_conflicts();
    report
        .findings
        .extend(mcu_coverage::mcu_coverage_lint(board, lib).findings);
    report
}
