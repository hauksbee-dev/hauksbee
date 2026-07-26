//! Decompile a KiCad board into code.
//!
//! The vision: a board is a *program* that rebuilds it. Hardware blocks that
//! repeat (a synapse circuit instanced hundreds of times, a neuron circuit
//! instanced a dozen) become **functions** called with different placements;
//! instances that deviate from their cluster's template stand out as explicit
//! anomalies. This serves both programmatic board generation and anomaly
//! visibility.
//!
//! ## Pipeline
//!
//! ```text
//! Pcb ──[netlist]──> Netlist ──[partition]──> blocks
//!     ──[fingerprint]──> WL colours ──[cluster]──> Analysis
//!     ──[emit]──> Rust-like source
//!     ──[rebuild]──> Pcb' (semantically equal to Pcb up to net renaming)
//! ```
//!
//! ## Algorithm summary
//!
//! 1. **Netlist extraction** flattens the CST into owned components with pads
//!    and interned nets.
//! 2. **Partitioning** builds the component-net graph, drops *global* nets
//!    (power/ground rails: by name, or by fanout exceeding a configurable
//!    fraction of the board), and takes connected components of footprints as
//!    candidate blocks. Without dropping globals, GND would merge everything.
//! 3. **Fingerprinting** runs Weisfeiler-Lehman colour refinement on the
//!    bipartite component+net graph, seeded by `(lib_id, value)`. Equal
//!    fingerprints => same cluster. This is approximate isomorphism; see
//!    [`fingerprint`] for the documented false-merge risk.
//! 4. **Clustering** groups blocks by fingerprint, derives a majority-vote
//!    template per cluster, aligns each instance by role, computes a rigid
//!    placement (Kabsch 2D), and diffs each instance for anomalies.
//! 5. **Emission** writes a `fn block_*` per cluster + a `main` calling them,
//!    inlining singletons and anomalous instances with `// ANOMALY:` comments.
//! 6. **Rebuild** interprets the structure back into a `Pcb` and a comparison
//!    helper checks semantic equivalence up to net renaming.

mod cluster;
pub mod dsl;
mod emit;
mod fingerprint;
mod fplib;
mod layout;
mod netlist;
mod partition;
mod rebuild;
mod report;
pub mod route_freerouting;

pub use cluster::{analyze, Analysis, Anomaly, Cluster, Instance, Placement, TemplateRole};
pub use fingerprint::{BlockGraph, Fingerprint};
pub use netlist::{Comp, CompPad, NetId, Netlist};
pub use partition::{partition, Block, Partition, PartitionConfig};
pub use rebuild::{
    compare, compare_connectivity, rebuild, semantics, semantics_of_netlist, BoardSemantics,
    FpSummary,
};
pub use report::{render_anomaly, render_report};

pub use dsl::{from_board, to_code, Program};
pub use fplib::FootprintLib;
pub use layout::{
    relayout, route_grid, FullConfig, IncrementalConfig, LayoutConfig, LayoutReport, RouteResult,
    RoutedTrack,
};
pub use route_freerouting::{
    connectivity, endpoint_net_violations, find_freerouting_jar, freerouting_available,
    merge_ses_into_pcb, merge_ses_text, parse_ses, route_with_freerouting, run_freerouting,
    validate_jar, write_dsn, Connectivity, FreeroutingConfig, RouteError, RouteOutcome, RouteRules,
    SesRoutes, DSN_FILE_NAME, SES_FILE_NAME,
};

use forge_model::Pcb;

/// One-call decompilation: returns the analysis for a parsed board using the
/// default partition configuration.
pub fn decompile_analysis(pcb: &Pcb) -> (Netlist, Analysis) {
    decompile_analysis_with(pcb, &PartitionConfig::default())
}

/// Decompilation with an explicit partition configuration.
pub fn decompile_analysis_with(pcb: &Pcb, cfg: &PartitionConfig) -> (Netlist, Analysis) {
    let nl = Netlist::from_pcb(pcb);
    let part = partition(&nl, cfg);
    let analysis = analyze(&nl, &part);
    (nl, analysis)
}

/// Decompile a board into readable Rust-like source.
///
/// Deterministic: calling twice on the same board yields identical text.
pub fn decompile(pcb: &Pcb) -> String {
    let (nl, analysis) = decompile_analysis(pcb);
    emit::emit_program(&nl, &analysis)
}

/// Decompile and also return the textual analysis report.
pub fn decompile_with_report(pcb: &Pcb) -> (String, String) {
    let (nl, analysis) = decompile_analysis(pcb);
    let code = emit::emit_program(&nl, &analysis);
    let rep = report::render_report(&nl, &analysis);
    (code, rep)
}
