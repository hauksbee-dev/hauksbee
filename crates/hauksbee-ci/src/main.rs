//! `hauksbee-ci` CLI.
//!
//! ```text
//! hauksbee-ci run <spec.toml> [--junit <out.xml>] [--quiet]
//! ```
//!
//! Exit code is 0 if every assertion passed, 1 otherwise (2 on a usage/spec
//! error). When `GITHUB_ACTIONS` is set in the environment, GitHub workflow
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
/// assertion passed, 1 if any failed, 2 on a spec/usage error. Writes JUnit XML
/// with --junit and emits GitHub annotations under GITHUB_ACTIONS.
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
    };
    let result = match run(&cfg) {
        Ok(r) => r,
        Err(e) => {
            // Spec / board errors: surface as a GitHub error too, then exit 2.
            eprintln!("hauksbee-ci: {e}");
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
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("hauksbee-ci: {e}");
            ExitCode::from(2)
        }
    }
}
