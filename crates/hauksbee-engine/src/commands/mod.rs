//! The CLI command handlers, one module per subcommand. The binary (`main.rs`)
//! is left with argument parsing + dispatch + process-level concerns; each
//! handler takes plain parameters (never the clap arg structs, which stay in the
//! binary) so the logic lives in the library where the TUI, tests and future
//! surfaces can reach it.

pub mod boardcode;
pub mod common;
pub mod doctor;
pub mod models;
pub mod run;
pub mod serve;
pub mod sim;
