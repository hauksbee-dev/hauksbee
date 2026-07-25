//! Design-rule / physics checks that run against a parsed or solved board.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
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
//! - [`converter`]: discrete switching-converter topology recovery (switch node
//!   / input rail / output rail / bulk caps) from the netlist + part kinds,
//!   shared by the ampacity and ripple checks.
//! - [`ampacity`]: IPC-2221 trace-ampacity, wired into `--si`. Attributes cited
//!   currents from the DB models and runs the
//!   [`audit_trace_currents`](hauksbee_extract::audit_trace_currents) engine.
//! - [`ripple`]: input bulk-capacitor ripple-current overstress on a buck.
//! - [`contention`]: the model-aware driver-contention lint. Two parts that BIND
//!   to push-pull digital outputs on one net are fighting, whatever the
//!   schematic's pin electrical types said. The extract-layer contention check
//!   reads pin types and treats a `bidirectional` MCU pad as a resolver, so a
//!   mis-mapped logic model driving an MCU GPIO net slipped past it.

pub mod ampacity;
pub mod boot;
pub mod contention;
pub mod converter;
pub mod device_decode;
pub mod mcu_coverage;
pub mod ripple;
pub mod straps;
pub mod usb_c;

use hauksbee_extract::{ExtractedBoard, NetLintReport, SiReport};
use hauksbee_models::ModelLibrary;

/// The full engine-level lint: the connectivity net-lint plus the model-aware
/// checks — strap pins, MCU resource conflicts, the unmodelled-MCU coverage
/// note, configured-device decode faults (e.g. a CYPD3177 PD-sink divider), and
/// model-aware driver contention.
/// Kept as one function so every surface (`--lint`, `--check`, the JSON
/// aggregate, TUI, the web front door) runs the identical set and no caller can
/// reopen the "Looks healthy" hole by forgetting one — device_decode used to be
/// spliced in only on the `--lint` path, so the other surfaces returned a false
/// PASS on those faults.
pub fn engine_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = board.net_lint();
    report
        .findings
        .extend(straps::strap_lint(board, lib).findings);
    report
        .findings
        .extend(resources_lint(board, lib).findings);
    report
        .findings
        .extend(device_decode::device_decode_lint(board, lib).findings);
    report
        .findings
        .extend(contention::contention_lint(board, lib).findings);
    report
}

/// The full engine-level signal-integrity report: the extract-layer SI checks
/// (`board.si_checks`) PLUS the model-aware engine-layer checks whose attribution
/// needs the bound DB models — trace ampacity (IPC-2221) and input-cap ripple.
/// Kept as one chokepoint so every SI surface (`--si`, `--check`, the JSON
/// aggregate, TUI, the web front door) runs the identical set. The
/// ampacity/ripple checks were previously appended only on the dedicated `--si`
/// path, so `--check`, the combined `--json`, and the web report returned a
/// false "looks healthy" over an under-width power trace or an over-ripple input
/// cap that `--si` flagged — the SI twin of the `engine_lint` hole above.
/// `geo_text` is the raw layout text (None for Altium, whose geometry is not yet
/// threaded into the text-based SI checks).
pub fn engine_si(board: &ExtractedBoard, lib: &ModelLibrary, geo_text: Option<&str>) -> SiReport {
    let mut report = board.si_checks(geo_text);
    ampacity::append_ampacity(board, lib, geo_text, &mut report);
    ripple::append_ripple(board, lib, &mut report);
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
