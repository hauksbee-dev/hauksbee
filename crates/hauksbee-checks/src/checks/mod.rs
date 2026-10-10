//! Design-rule / physics checks that run against a parsed or solved board.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! Each check is self-contained: it takes an
//! [`ExtractedBoard`] (and, where it needs
//! physics, builds and solves its own [`Circuit`](hauksbee_ir::Circuit)), and
//! returns a verdict plus the numbers behind it. They are kept separate from
//! the bind-time [`stress`](hauksbee_bind::stress) monitor: stress watches a running
//! co-simulation against datasheet ratings, whereas a check answers a specific
//! standards question about the design.
//!
//! - [`usb_c`]: the USB Type-C CC attach classifier. It attaches a generic
//!   source + cable model to a board's CC termination and classifies the result
//!   against the USB Type-C spec windows (Sink / `AudioAccessory` / ...). This is
//!   what re-derives the `RPi` 4 shared-CC-pulldown fault cold.
//! - [`straps`]: the boot strapping-pin lint. It reads each MCU's strap table
//!   from the model db and flags a strap net that cannot hold the level the part
//!   needs at reset (a free-running clock on it, or a pull to the wrong rail).
//! - [`mcu_coverage`]: flags a recognised MCU that has no authored device model,
//!   so the strap and resource-conflict checks above could not run on it,
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
//! - [`back_power`]: the cross-domain pull lint. A modelled part's signal pin
//!   tied or pulled up to a rail above the part's own supply conducts through
//!   the pin's protection clamp into the lower rail: back-powering at
//!   power-down, abs-max violation powered up.
//! - [`bus_loading`]: I2C pull-up SIZING (the presence check is the extract
//!   layer's `MissingI2cPullup`): an effective pull-up demanding more sink
//!   current than the spec's 3 mA driver rating, or advisorily too weak for
//!   the estimated bus capacitance to meet the rise-time budget.
//! - [`logic_levels`]: a CMOS logic input left between its VCC-scaled VIL
//!   and VIH, where the datasheet guarantees no logic state.
//! - [`rail_conflicts`]: a rail more than one source sets the voltage of
//!   (two regulators tied, differently named rails joined), from the binder's
//!   rail-ownership pass.
//! - [`rail_decoupling`]: a supply rail with far fewer capacitors to ground
//!   than ICs drawing from it, or no bulk capacitor on a busy rail (a count,
//!   reported as a non-gating note).
//! - [`floating_inputs`]: a CMOS logic input left with no driver, pull or
//!   other connection, no-connect flag or not.
//! - [`siblings`]: one instance of a repeated cell (a part group of eight or
//!   more with one dominant wiring signature) wired differently from its
//!   twins on two or more pads, or a minority of transistor units whose
//!   base, collector and emitter are a terminal exchange of the majority's
//!   wiring: the swapped-pin outlier. Run from [`node_drive`] on its index.
//! - [`node_drive`]: three node-level DC faults read from pin roles: a BJT
//!   base driven straight from a rail, switch or output with its emitter on a
//!   rail; a node held only by capacitor plates and high-impedance inputs;
//!   and a MOSFET gate or analog-switch select held only by MCU or connector
//!   pins with no pull.
//! - [`dc_stress`]: every card rating (junction, channel and driver-pin
//!   current, resistor wattage, junction temperature) judged at the board's
//!   solved DC operating point by the co-simulation's own stress monitor.

pub mod ampacity;
pub mod analog_inputs;
pub mod back_power;
pub mod boot;
pub mod bus_loading;
pub mod contention;
pub mod converter;
pub mod dc_stress;
pub mod device_decode;
pub mod envelope;
pub mod floating_inputs;
pub mod indicators;
pub mod logic_levels;
pub mod mcu_coverage;
pub mod model_controls;
pub mod node_drive;
pub mod rail_conflicts;
pub mod rail_decoupling;
pub mod ripple;
pub mod siblings;
pub mod straps;
#[cfg(test)]
pub(crate) mod test_support;
pub mod usb_c;

use hauksbee_bind::variant_sweep::CornerPlan;
use hauksbee_extract::{ExtractedBoard, NetLintReport, SiReport};
use hauksbee_models::ModelLibrary;

/// Net-leaf name, upper-cased: strips any sheet path prefix.
pub(crate) fn norm(name: &str) -> String {
    let n = name.trim();
    let leaf = n.rsplit('/').next().unwrap_or(n);
    leaf.trim().to_ascii_uppercase()
}

pub(crate) fn is_ground_name(name: &str) -> bool {
    let n = norm(name);
    matches!(
        n.as_str(),
        "GND" | "GNDA" | "GNDD" | "AGND" | "DGND" | "PGND" | "VSS" | "GNDIO" | "0"
    ) || n.starts_with("GND")
}

/// `KiCad`'s placeholder net for a pin that is on no net at all.
pub(crate) fn is_unconnected_net(name: &str) -> bool {
    name.trim_start_matches('/').starts_with("unconnected-")
}

/// Every assembled component paired with the model that resolved it.
///
/// The three-state assembly contract, asked once: only a Present record can be
/// a resolvable part, so DNP and identity-refused records abstain rather than
/// contribute a model-derived finding.
pub(crate) fn resolved_components<'a>(
    board: &'a ExtractedBoard,
    lib: &'a ModelLibrary,
) -> impl Iterator<Item = (&'a hauksbee_extract::Component, hauksbee_models::ModelEntry)> + 'a {
    board.components.iter().filter_map(move |comp| {
        let part = hauksbee_extract::assembly::AssemblyState::of(comp).fitted()?;
        Some((comp, hauksbee_bind::binder::resolve(lib, part).model?))
    })
}

/// The full engine-level lint.
///
/// It is the connectivity net-lint plus the model-aware
/// checks, strap pins, MCU resource conflicts, the unmodelled-MCU coverage
/// note, configured-device decode faults (e.g. a CYPD3177 PD-sink divider), and
/// model-aware driver contention.
/// Kept as one function so every surface (`--lint`, `--check`, the JSON
/// aggregate, TUI, the web front door) runs the identical set and no caller can
/// reopen the "Looks healthy" hole by forgetting one: splice `device_decode` in
/// on the `--lint` path alone and every other surface returns a false PASS on
/// those faults.
#[must_use]
pub fn engine_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    engine_lint_swept(board, lib).0
}

/// [`engine_lint`] judged at every assumed corner of the board's class-only
/// parts (a generic MOSFET symbol with no part number binds a corner set).
///
/// The unpinned run judges each set at its stand-in; each further run of the
/// [`CornerPlan`] pins the corners and adds the findings the stand-in run did
/// not raise, naming the corners that raised them. So the result passes only
/// if every corner does. Returns the plan, which the caller marks on its bind
/// report so the evidence says the result holds across the corners, not at one.
/// The sweep is bounded by the plan (exhaustive for a few sets, else joint),
/// so its cost is a small constant number of extra lint passes.
#[must_use]
pub fn engine_lint_swept(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
) -> (NetLintReport, Option<CornerPlan>) {
    let mut report = engine_lint_at(board, lib);
    let plan = CornerPlan::of(board, lib);
    for run in plan.iter().flat_map(|p| &p.runs) {
        let mut pinned = board.clone();
        hauksbee_bind::variant_sweep::pin(&mut pinned, run);
        let corners: Vec<String> = run.iter().map(|(r, id)| format!("{r} = {id}")).collect();
        for mut finding in engine_lint_at(&pinned, lib).findings {
            let seen = report.findings.iter().any(|f| {
                f.check == finding.check && f.refs == finding.refs && f.nets == finding.nets
            });
            if seen {
                continue;
            }
            let named = run
                .iter()
                .all(|(_, id)| finding.message.contains(id.as_str()));
            if !named {
                finding.message = format!(
                    "{} (at the assumed corners {})",
                    finding.message,
                    corners.join(", ")
                );
            }
            report.findings.push(finding);
        }
    }
    (report, plan)
}

/// One [`engine_lint`] pass over `board` as it stands.
fn engine_lint_at(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = board.net_lint();
    report
        .findings
        .extend(straps::strap_lint(board, lib).findings);
    report.findings.extend(resources_lint(board, lib).findings);
    report
        .findings
        .extend(device_decode::device_decode_lint(board, lib).findings);
    report
        .findings
        .extend(envelope::envelope_lint(board, lib).findings);
    report
        .findings
        .extend(contention::contention_lint(board, lib).findings);
    report
        .findings
        .extend(back_power::back_power_lint(board, lib).findings);
    report
        .findings
        .extend(bus_loading::bus_loading_lint(board).findings);
    // One binding serves both checks that need the bound board: the rail
    // conflicts read its report, the DC stress pass then consumes it. Binding
    // a large board is not cheap, so it is done once here, not once per check.
    let bound = hauksbee_bind::binder::bind_board(board, lib);
    report
        .findings
        .extend(rail_conflicts::rail_conflict_findings(&bound.report).findings);
    report
        .findings
        .extend(rail_decoupling::decoupling_lint(board, lib).findings);
    report
        .findings
        .extend(floating_inputs::floating_input_lint(board, lib).findings);
    report
        .findings
        .extend(node_drive::node_drive_lint(board, lib).findings);
    report
        .findings
        .extend(dc_stress::dc_stress_findings(bound).findings);
    // Static surfaces have no solved levels, so the comparator half of this
    // check reads the rail table and abstains on a divider tap; the ADC
    // source-impedance half is pure topology and runs either way.
    let rails = |net: &str| hauksbee_bind::binder::power_rail_voltage(net);
    report
        .findings
        .extend(analog_inputs::analog_input_lint(board, lib, &rails).findings);
    // Likewise a logic input is judged statically only where it is tied to a
    // named rail (3.3 V into a 5 V CMOS input); solved levels judge the rest.
    let point = |net: &str| rails(net).map(|v| envelope::RailRange { min_v: v, max_v: v });
    report
        .findings
        .extend(logic_levels::logic_level_lint(board, lib, &point).findings);
    for finding in model_controls::model_control_lint(board, lib).findings {
        // Schematic/netlist extraction can already carry the pin function and
        // raise the same structural finding. The model-aware path exists for
        // PCB-only inputs whose pads have numbers/nets but no symbol function.
        if !report.findings.iter().any(|existing| {
            existing.check == finding.check
                && existing.refs == finding.refs
                && existing.nets == finding.nets
        }) {
            report.findings.push(finding);
        }
    }
    report
}

/// Layout wiring checks on a `KiCad` layout (empty for anything else).
///
/// Reports pads copper never joins to their net and, when `board_path` has a
/// same-stem `.kicad_sch` beside it, pins the layout wires differently from
/// that schematic.
#[must_use]
pub fn layout_lint(
    board: &ExtractedBoard,
    text: Option<&str>,
    board_path: Option<&std::path::Path>,
) -> NetLintReport {
    let Some(text) = text.filter(|t| t.contains("(kicad_pcb")) else {
        return NetLintReport::default();
    };
    let schematic = board_path.and_then(ExtractedBoard::companion_schematic);
    board.layout_lint(text, schematic.as_ref())
}

/// The full engine-level signal-integrity report.
///
/// It is the extract-layer SI checks
/// (`board.si_checks`) PLUS the model-aware engine-layer checks whose attribution
/// needs the bound DB models, trace ampacity (IPC-2221) and input-cap ripple.
/// Kept as one chokepoint so every SI surface (`--si`, `--check`, the JSON
/// aggregate, TUI, the web front door) runs the identical set. The
/// ampacity/ripple checks appended on the dedicated `--si` path alone would
/// leave `--check`, the combined `--json`, and the web report returning a
/// false "looks healthy" over an under-width power trace or an over-ripple input
/// cap that `--si` flags; the SI twin of the `engine_lint` hole above.
/// `geo_text` is the raw layout text (None for Altium, whose geometry is not yet
/// threaded into the text-based SI checks).
#[must_use]
pub fn engine_si(board: &ExtractedBoard, lib: &ModelLibrary, geo_text: Option<&str>) -> SiReport {
    let mut report = board.si_checks(geo_text);
    ampacity::append_ampacity(board, lib, geo_text, &mut report);
    ripple::append_ripple(board, lib, &mut report);
    report
}

/// The `--resources` view: the MCU internal resource-conflict check PLUS the
/// unchecked-strap-bearing-MCU coverage note.
///
/// A named chokepoint so neither the
/// `--resources` path nor `engine_lint` can drop the coverage note by forgetting
/// to append it.
#[must_use]
pub fn resources_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = board.resource_conflicts();
    report
        .findings
        .extend(mcu_coverage::mcu_coverage_lint(board, lib).findings);
    report
}
