//! One integration-test binary for the crate; each module was a
//! separate test file (and separate link step) before.

#[path = "ac_validation.rs"]
mod ac_validation;
#[path = "analytic.rs"]
mod analytic;
#[path = "behavioral_sources.rs"]
mod behavioral_sources;
#[path = "bjt_physics_torn.rs"]
mod bjt_physics_torn;
#[path = "board_benchmark_inputs.rs"]
mod board_benchmark_inputs;
#[path = "breakpoints.rs"]
mod breakpoints;
#[path = "bughunt_regression.rs"]
mod bughunt_regression;
#[path = "controlled_sources.rs"]
mod controlled_sources;
#[path = "coupled_inductors.rs"]
mod coupled_inductors;
#[path = "decoupling_sag.rs"]
mod decoupling_sag;
#[path = "error_budget.rs"]
mod error_budget;
#[path = "features.rs"]
mod features;
#[path = "ic_nodeset.rs"]
mod ic_nodeset;
#[path = "kicad_vectors.rs"]
mod kicad_vectors;
#[path = "linesearch_fixture.rs"]
mod linesearch_fixture;
#[path = "mosfet_rds_on.rs"]
mod mosfet_rds_on;
#[path = "newton_bypass.rs"]
mod newton_bypass;
#[path = "ngspice.rs"]
mod ngspice;
#[path = "nonconvergence_blame.rs"]
mod nonconvergence_blame;
#[path = "opamp_dynamics.rs"]
mod opamp_dynamics;
#[path = "opamp_follower_rail.rs"]
mod opamp_follower_rail;
#[path = "parallel_determinism.rs"]
mod parallel_determinism;
#[path = "parallel_speedup.rs"]
mod parallel_speedup;
#[path = "perf.rs"]
mod perf;
#[path = "perf_gate.rs"]
mod perf_gate;
#[path = "planned_assembly.rs"]
mod planned_assembly;
#[path = "power_ramp.rs"]
mod power_ramp;
#[path = "rail_tear.rs"]
mod rail_tear;
#[path = "rawfile_roundtrip.rs"]
mod rawfile_roundtrip;
#[path = "refusal_shapes.rs"]
mod refusal_shapes;
#[path = "staged_dc.rs"]
mod staged_dc;
#[path = "staged_property.rs"]
mod staged_property;
#[path = "stretcher_transient.rs"]
mod stretcher_transient;
