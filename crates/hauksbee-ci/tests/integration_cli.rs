//! Command-line integration tests bundled to avoid repeated linking.

#[path = "check_cli.rs"]
mod check_cli;
#[path = "cli_diagnostics.rs"]
mod cli_diagnostics;
#[path = "exit3_reachability.rs"]
mod exit3_reachability;
#[path = "hook_gate_e2e.rs"]
mod hook_gate_e2e;
#[path = "init_scaffold.rs"]
mod init_scaffold;
#[path = "progress_stays_out_of_the_way.rs"]
mod progress_stays_out_of_the_way;
#[path = "round2_cli.rs"]
mod round2_cli;
#[path = "round3_cli.rs"]
mod round3_cli;
#[path = "shipped_examples_run.rs"]
mod shipped_examples_run;
