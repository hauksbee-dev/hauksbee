//! `hauksbee-ci` CLI.
//!
//! ```text
//! hauksbee-ci run <spec.toml> [--junit <out.xml>] [--quiet]
//! ```
//!
//! Exit code is 0 if every assertion passed, 1 if any ordinarily failed, 2 on a
//! usage/spec error, and 3 when the run is invalid for analysis (an analog chunk
//! failed to converge under an assertion's window, or the strict abort tripped;
//! 05 §3b). When `GITHUB_ACTIONS` is set in the environment, GitHub workflow
//! annotations are emitted to stdout so failures surface inline.
//!
//! The argument surface is defined with `clap` (derive API): `--help`/`-h`,
//! usage-on-error, and did-you-mean suggestions all come for free.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use hauksbee_ci::{init, run, RunConfig};

/// CI for hardware: run a board+firmware spec headless and assert on the result.
///
/// Point it at a TOML spec and it boots the firmware on the emulated PCB, then
/// asserts on rails, UART, blink rate and stress faults. Exits 0 if every
/// assertion passed, 1 if any failed, 2 on a spec/usage error, 3 when the analog
/// co-sim did not converge under an assertion's window (invalid for analysis).
/// Writes JUnit XML with --junit and emits GitHub annotations under GITHUB_ACTIONS.
#[derive(Parser)]
#[command(
    name = "hauksbee-ci",
    version,
    about = "CI for hardware: boot firmware on the emulated PCB and assert rails, UART, and blink.",
    long_about = None,
    propagate_version = true,
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a hauksbee-ci spec and assert on the result.
    ///
    /// Example:
    ///   hauksbee-ci run ci/power-up.toml --junit results.xml
    Run(RunArgs),

    /// Scaffold a starter spec from a board, so your first spec is an edit.
    ///
    /// Loads and binds the board, then writes `<board-stem>.toml` beside it with
    /// the detected MCU, supplies and rails already filled in and every line
    /// commented. Refuses to overwrite an existing spec.
    ///
    /// Example:
    ///   hauksbee-ci init hardware/board.kicad_pcb
    Init(InitArgs),
}

#[derive(Parser)]
struct RunArgs {
    /// The hauksbee-ci TOML spec to run.
    #[arg(value_name = "SPEC")]
    spec: PathBuf,

    /// Write JUnit XML to this path (for the CI test-report step).
    #[arg(long, value_name = "OUT.XML")]
    junit: Option<PathBuf>,

    /// Suppress the per-assertion human report (exit code still reflects pass/fail).
    #[arg(long)]
    quiet: bool,

    /// Re-run one ensemble member in isolation (a failing fuzz/tolerance seed,
    /// or a corner index). Sampling is keyed by the absolute seed number, so
    /// the isolated run reproduces the full run's values exactly.
    #[arg(long, value_name = "N")]
    seed: Option<u32>,
}

#[derive(Parser)]
struct InitArgs {
    /// Board file to scaffold a spec from (.kicad_pcb, .kicad_sch, .net, .brd, .d356).
    #[arg(value_name = "BOARD")]
    board: PathBuf,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let args = match cli.command {
        Command::Run(args) => args,
        Command::Init(args) => return cmd_init(args),
    };

    let cfg = RunConfig {
        spec: args.spec.clone(),
        seed: args.seed,
    };
    let result = match run(&cfg) {
        Ok(r) => r,
        Err(e) => {
            // Spec / board errors: surface as a GitHub error too, then exit 2.
            eprintln!("hauksbee-ci: {e}");
            // Emit JUnit even on this error path when --junit was requested: a CI
            // that only reads the Checks/JUnit tab would otherwise see nothing at
            // all for an exit-2 (a desynced spec, a missing board). Write a
            // single errored testcase carrying the message.
            if let Some(path) = &args.junit {
                let xml = hauksbee_ci::report::render_junit_error(&e.to_string());
                if let Err(werr) = std::fs::write(path, xml) {
                    eprintln!(
                        "hauksbee-ci: could not write JUnit XML to {}: {werr}",
                        path.display()
                    );
                }
            }
            if std::env::var_os("GITHUB_ACTIONS").is_some() {
                // Percent first, then control chars (else the %0A/%0D we insert
                // get their own % re-encoded to %25, garbling the annotation).
                let msg = e
                    .to_string()
                    .replace('%', "%25")
                    .replace('\r', "%0D")
                    .replace('\n', "%0A");
                println!("::error title=hauksbee-ci spec error::{msg}");
            }
            return ExitCode::from(2);
        }
    };

    if !args.quiet {
        print!("{}", result.render_human());
    }

    if let Some(path) = &args.junit {
        if let Err(e) = std::fs::write(path, result.render_junit()) {
            eprintln!(
                "hauksbee-ci: could not write JUnit XML to {}: {e}",
                path.display()
            );
            return ExitCode::from(2);
        }
        if !args.quiet {
            println!("wrote JUnit XML to {}", path.display());
        }
    }

    if std::env::var_os("GITHUB_ACTIONS").is_some() {
        print!("{}", result.render_github_annotations());
    }

    ExitCode::from(result.exit_code() as u8)
}

/// `hauksbee-ci init <board>`: scaffold a starter spec and print where it landed.
/// Board / bind errors carry the crate's loud, did-you-mean style; exit 2 on any
/// failure to match the spec-error contract of `run`.
fn cmd_init(args: InitArgs) -> ExitCode {
    match init(&args.board) {
        Ok(path) => {
            println!("wrote starter spec to {}", path.display());
            println!("edit it, then run:  hauksbee-ci run {}", path.display());
            // Where the documented pre-commit hook looks. init deliberately
            // writes the spec beside its board (so the relative `board = "..."`
            // stays valid); the hook searches `ci/` and the repo root by default
            // (HAUKSBEE_CI_SPECS, colon-separated). Say so, rather than silently
            // relocating the file, so a scaffolded spec is actually discovered.
            println!(
                "\nto have the pre-commit hook run this automatically, put the spec where it\n\
                 searches — `ci/` or the repo root by default (override with HAUKSBEE_CI_SPECS,\n\
                 colon-separated). Either move it into `ci/` (and fix the `board = \"...\"` path\n\
                 to stay relative to it), or add its directory to HAUKSBEE_CI_SPECS."
            );
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("hauksbee-ci: {e}");
            ExitCode::from(2)
        }
    }
}
