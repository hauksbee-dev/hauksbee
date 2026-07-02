//! `hauksbee-ci`: CI for hardware.
//!
//! Software got transformed by running tests on every commit. Hardware has had
//! nothing like it. `hauksbee-ci` runs the hauksbee PCB emulator headless in a
//! pipeline: on every layout change it boots the firmware on the real board,
//! and asserts the things you would otherwise only learn on the bench — the
//! rail comes up at 4.96 V, the UART says hello, the LED blinks at 5 Hz, no
//! part exceeds its rating.
//!
//! Input is a hand-written [`spec::Spec`] (TOML) checked into a hardware repo.
//! Output is a human report, a process exit code (0 green / 1 red), JUnit XML
//! for any CI system, and GitHub Actions annotations.
//!
//! ```no_run
//! use hauksbee_ci::{run, RunConfig};
//! let cfg = RunConfig { spec: "ci/power-up.toml".into(), ..Default::default() };
//! let result = run(&cfg).unwrap();
//! std::process::exit(result.exit_code());
//! ```

use std::path::PathBuf;
use std::time::Instant;

pub mod assertions;
pub mod error;
pub mod init;
pub mod report;
pub mod runner;
pub mod scenarios;
pub mod spec;

pub use error::SpecError;
pub use init::init;
pub use report::CiResult;
pub use spec::Spec;

/// How to run a spec.
#[derive(Debug, Clone, Default)]
pub struct RunConfig {
    /// Path to the TOML spec file.
    pub spec: PathBuf,
}

/// Load the spec, run the co-sim across its seeds, and evaluate its assertions.
pub fn run(cfg: &RunConfig) -> Result<CiResult, SpecError> {
    let started = Instant::now();
    let spec = Spec::load(&cfg.spec)?;
    let outcomes = runner::run_spec(&spec)?;
    let results = assertions::evaluate(&spec, &outcomes);
    // A strict analog abort on ANY seed forces the invalid-for-analysis exit even
    // if no assertion's window happened to overlap the failed span (05 §3b).
    let analog_abort = outcomes.iter().any(|o| o.analog_abort);
    Ok(CiResult {
        spec_name: spec.name.clone(),
        board: spec.board.display().to_string(),
        results,
        seeds: outcomes.len() as u32,
        elapsed: started.elapsed(),
        analog_abort,
    })
}
