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
//! Long-form how-and-why: docs/how-and-why/hauksbee-ci/README.md.
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
pub mod hwtrace;
pub mod init;
pub mod report;
pub mod runner;
pub mod scenarios;
pub mod spec;
pub mod tolerance;

pub use error::SpecError;
pub use init::init;
pub use report::CiResult;
pub use spec::Spec;

/// How to run a spec.
#[derive(Debug, Clone, Default)]
pub struct RunConfig {
    /// Path to the TOML spec file.
    pub spec: PathBuf,
    /// Run only this one ensemble seed (fuzz + tolerance), for re-running a
    /// reported failing seed in isolation. Sampling is keyed by the absolute
    /// seed number, so the isolated run reproduces the full run's values
    /// exactly. `None` = run the whole ensemble.
    pub seed: Option<u32>,
    /// Extra model directory, layered above the builtin db, installed packs,
    /// and the user model dirs — the same `--models-dir` layer `hauksbee run`
    /// has, so a custom `[[models]]` routing entry binds in CI too.
    pub models_dir: Option<PathBuf>,
}

/// Load the spec, run the co-sim across its seeds, and evaluate its assertions.
pub fn run(cfg: &RunConfig) -> Result<CiResult, SpecError> {
    let started = Instant::now();
    let spec = Spec::load(&cfg.spec)?;
    let extra: Vec<&std::path::Path> = cfg.models_dir.as_deref().into_iter().collect();
    let lib = hauksbee_models::ModelLibrary::builtin_with_user_dirs(&extra);
    let outcomes = runner::run_spec_with_lib(&spec, cfg.seed, &lib)?;
    let results = assertions::evaluate(&spec, &outcomes);
    // A strict analog abort on ANY seed forces the invalid-for-analysis exit even
    // if no assertion's window happened to overlap the failed span (05 §3b).
    let analog_abort = outcomes.iter().any(|o| o.analog_abort);
    // Coverage banner data for a tolerance ensemble: how many members ran and
    // how many components were sampled (from any outcome; the set is fixed).
    let coverage = if spec.has_tolerances() {
        let components = outcomes
            .first()
            .map(|o| o.sampled_values.len())
            .unwrap_or(0);
        let members = outcomes.len() as u32;
        Some(match spec.ensemble_mode()? {
            // Member 0 is the nominal baseline (it draws no random sample), so
            // the number of genuinely SAMPLED seeds is members - 1. Reporting the
            // full member count claimed `seeds=1` sampled something when it only
            // ran the nominal.
            tolerance::Mode::MonteCarlo => report::EnsembleCoverage::MonteCarlo {
                seeds: members.saturating_sub(1),
                components,
            },
            tolerance::Mode::Corners => report::EnsembleCoverage::Corners {
                corners: members,
                components,
            },
        })
    } else {
        None
    };
    Ok(CiResult {
        spec_name: spec.name.clone(),
        board: spec.board.display().to_string(),
        results,
        seeds: outcomes.len() as u32,
        elapsed: started.elapsed(),
        analog_abort,
        coverage,
    })
}
