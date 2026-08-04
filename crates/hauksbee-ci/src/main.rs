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

use hauksbee_ci::{run, RunConfig};

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
    /// Run one or more hauksbee-ci specs and assert on the result.
    ///
    /// Examples:
    ///   hauksbee-ci run ci/power-up.toml --junit results.xml
    ///   hauksbee-ci run ci/*.toml --junit results.xml   (merged report, worst exit code)
    ///
    /// Exit codes (the pipeline contract): 0 every assertion held (GREEN);
    /// 1 at least one assertion failed (RED); 2 spec/board error (bad TOML,
    /// missing board, unknown net); 3 invalid for analysis (the analog solve
    /// aborted, so the result is not trustworthy and the run refuses to
    /// pretend).
    Run(RunArgs),

    /// Validate one or more specs WITHOUT running any simulation.
    ///
    /// Parses the spec, runs every structural validation, and (unless
    /// --no-board) resolves and loads the referenced board file to validate
    /// every net and component reference against it. No emulator boots, no
    /// circuit is solved; this is the fast load-only mode editor tooling
    /// polls on every keystroke.
    ///
    /// What it checks: TOML syntax and vocabulary (unknown/missing fields),
    /// every field's documented bounds, assertion/peripheral/supply/scenario
    /// structure, cross-references inside the spec (scenario ids, peripheral
    /// ids), and, with the board loaded, every referenced net and component.
    ///
    /// What it does NOT check (these need a run, a model bind, or an artifact
    /// that may legitimately not exist yet at edit time): firmware existence
    /// or loadability, sensor-TOML contents, scenario part/profile resolution
    /// against the model DB, tolerance patterns matching real components,
    /// MCU/emulator resolution, and of course every behavioral assertion.
    ///
    /// All independent errors are reported in one invocation, not one per
    /// run. Exit code: 0 when every spec is valid, 2 otherwise.
    Check(CheckArgs),

    /// Scaffold a starter spec from a board, so your first spec is an edit.
    ///
    /// Loads and binds the board, then writes `<board-stem>.toml` into the
    /// current directory (or --out) with the detected MCU, supplies and rails
    /// already filled in and every line commented; the spec's `board = "..."`
    /// path is written relative to where the spec lands. Refuses to overwrite
    /// an existing spec.
    ///
    /// Example:
    ///   cd ci && hauksbee-ci init ../hardware/board.kicad_pcb
    Init(InitArgs),
}

#[derive(Parser)]
struct RunArgs {
    /// The hauksbee-ci TOML spec(s) to run. More than one (spelled out or via
    /// a shell glob, `hauksbee-ci run ci/*.toml`) runs each in turn,
    /// aggregates one summary, merges everything into one --junit file, and
    /// exits with the worst code of the set (severity order 3 > 2 > 1 > 0).
    #[arg(value_name = "SPEC", num_args = 1.., required = true)]
    specs: Vec<PathBuf>,

    /// Write JUnit XML to this path (for the CI test-report step).
    #[arg(long, value_name = "OUT.XML")]
    junit: Option<PathBuf>,

    /// Suppress the per-assertion human report (exit code still reflects pass/fail).
    #[arg(long)]
    quiet: bool,

    /// Print the run result as one JSON object on stdout instead of the human
    /// report (the web checks panel and any tool consume this). Exit codes are
    /// unchanged; a spec/board error prints `{"ok":false,"error":...}`.
    #[arg(long)]
    json: bool,

    /// Re-run one ensemble member in isolation (a failing fuzz/tolerance seed,
    /// or a corner index). Sampling is keyed by the absolute seed number, so
    /// the isolated run reproduces the full run's values exactly.
    #[arg(long, value_name = "N")]
    seed: Option<u32>,

    /// Extra model directory, layered above the builtin db, installed packs,
    /// and the user model dirs (`~/.hauksbee/models`, `~/.config/hauksbee/models`);
    /// the same layer order as `hauksbee run --models-dir`. Lets a spec's
    /// board bind custom parts checked into the hardware repo, including
    /// `[[models]] kind = "mcu"` routing entries for user SoC descriptors.
    #[arg(long, value_name = "DIR")]
    models_dir: Option<PathBuf>,
}

#[derive(Parser)]
struct CheckArgs {
    /// The hauksbee-ci TOML spec(s) to validate.
    #[arg(value_name = "SPEC", num_args = 1.., required = true)]
    specs: Vec<PathBuf>,

    /// Skip resolving/loading the board file: parse + structural validation
    /// only. Net and component references are then NOT validated (they need
    /// the board), so a clean exit means "structurally valid", not "will run".
    #[arg(long)]
    no_board: bool,

    /// Print diagnostics as JSON on stdout: one array per spec, one per line
    /// (a single spec prints a single array). Each element is
    /// {"line", "col", "code", "message", "fix"}; line/col/fix are omitted
    /// when not derivable. A valid spec prints an empty array.
    #[arg(long)]
    json: bool,
}

#[derive(Parser)]
struct InitArgs {
    /// Board file to scaffold a spec from (.kicad_pcb, .kicad_sch, .net, .brd, .d356).
    #[arg(value_name = "BOARD")]
    board: PathBuf,

    /// Where to write the spec: a path ending in .toml is the spec file, and
    /// anything else is a directory (created if missing) that gets
    /// <board-stem>.toml inside it. Default: the current directory.
    #[arg(long, value_name = "PATH")]
    out: Option<PathBuf>,
}

fn main() -> ExitCode {
    // A killed CI run must not orphan its co-sim emulators: reap every live
    // Renode/QEMU child on SIGTERM/SIGINT. See hauksbee_mcu::children.
    hauksbee_mcu::children::install_signal_reaper();
    let cli = Cli::parse();
    let args = match cli.command {
        Command::Run(args) => args,
        Command::Check(args) => return cmd_check(args),
        Command::Init(args) => return cmd_init(args),
    };

    // A co-sim is minutes of silence otherwise, which reads as a hang. Only for
    // a human watching: `--quiet` asked for silence, `--json` output is parsed,
    // and a CI log wants the result rather than a hundred progress lines. The
    // sink itself also declines a non-terminal stderr.
    if !args.quiet && !args.json && std::env::var_os("GITHUB_ACTIONS").is_none() {
        hauksbee_ci::progress::to_stderr();
    }

    // Multi-spec aggregation (one summary, one merged JUnit document, worst
    // exit code of the set). A single spec is the one-element case of the same
    // loop, so the two paths cannot drift.
    let github = std::env::var_os("GITHUB_ACTIONS").is_some();
    let multi = args.specs.len() > 1;
    let mut suites: Vec<hauksbee_ci::report::JunitSuite> = Vec::new();
    // Exit severity is 3 > 2 > 1 > 0 (an untrustworthy run outranks a plain
    // spec error, which outranks a red assertion), which is numeric max.
    let mut worst: u8 = 0;
    // Per-spec verdict lines for the aggregate summary.
    let mut verdicts: Vec<(String, u8)> = Vec::new();

    for spec in &args.specs {
        let cfg = RunConfig {
            spec: spec.clone(),
            seed: args.seed,
            models_dir: args.models_dir.clone(),
        };
        match run(&cfg) {
            Ok(result) => {
                if args.json {
                    // One JSON object per spec, one per line (NDJSON): the
                    // single-spec shape is byte-identical to before, and a
                    // multi-spec consumer splits on newlines.
                    println!("{}", result.render_json());
                } else if !args.quiet {
                    if multi {
                        println!("=== {} ===", spec.display());
                    }
                    print!("{}", result.render_human());
                }
                if args.junit.is_some() {
                    suites.push(result.junit_suite());
                }
                if github {
                    print!("{}", result.render_github_annotations());
                }
                let code = result.exit_code() as u8;
                verdicts.push((spec.display().to_string(), code));
                worst = worst.max(code);
            }
            Err(e) => {
                // Spec / board errors: surface as a GitHub error too, count
                // toward the merged JUnit, and keep running the REST of the
                // set: in a multi-spec invocation one desynced spec must not
                // hide the others' verdicts.
                if args.json {
                    println!(
                        "{}",
                        serde_json::json!({ "ok": false, "error": e.to_string() })
                    );
                }
                eprintln!("hauksbee-ci: {}: {e}", spec.display());
                // Emit JUnit even on this error path when --junit was
                // requested: a CI that only reads the Checks/JUnit tab would
                // otherwise see nothing at all for an exit-2 (a desynced spec,
                // a missing board), a single errored testcase carries the message.
                if args.junit.is_some() {
                    suites.push(hauksbee_ci::report::junit_error_suite(
                        &spec.display().to_string(),
                        &e.to_string(),
                    ));
                }
                if github {
                    // Percent first, then control chars (else the %0A/%0D we
                    // insert get their own % re-encoded to %25, garbling the
                    // annotation).
                    let msg = e
                        .to_string()
                        .replace('%', "%25")
                        .replace('\r', "%0D")
                        .replace('\n', "%0A");
                    println!("::error title=hauksbee-ci spec error::{msg}");
                }
                verdicts.push((spec.display().to_string(), 2));
                worst = worst.max(2);
            }
        }
    }

    if let Some(path) = &args.junit {
        let xml = hauksbee_ci::report::render_junit_document(&suites);
        if let Err(e) = std::fs::write(path, xml) {
            eprintln!(
                "hauksbee-ci: could not write JUnit XML to {}: {e}",
                path.display()
            );
            return ExitCode::from(2u8.max(worst));
        }
        if !args.quiet && !args.json {
            println!("wrote JUnit XML to {}", path.display());
        }
    }

    // The aggregate summary, only when there was a set to aggregate.
    if multi && !args.quiet && !args.json {
        println!("\n=== {} specs ===", verdicts.len());
        for (spec, code) in &verdicts {
            let word = match code {
                0 => "GREEN",
                1 => "RED",
                2 => "SPEC ERROR",
                _ => "INVALID",
            };
            println!("  [{word}] {spec}");
        }
        println!("worst exit code of the set: {worst} (severity order 3 > 2 > 1 > 0)");
    }

    ExitCode::from(worst)
}

/// `hauksbee-ci check <spec>...`: load-only validation, no simulation. Exit 0
/// when every spec is valid, 2 when any produced diagnostics (matching `run`'s
/// spec-error exit code).
fn cmd_check(args: CheckArgs) -> ExitCode {
    let opts = hauksbee_ci::check::CheckOptions {
        no_board: args.no_board,
    };
    let mut worst = 0u8;
    for spec in &args.specs {
        let diags = hauksbee_ci::check::check_spec(spec, &opts);
        if args.json {
            println!(
                "{}",
                serde_json::to_string(&diags).expect("diagnostics serialize")
            );
        } else if diags.is_empty() {
            println!("{}: OK", spec.display());
        } else {
            for d in &diags {
                eprintln!("{}", d.render_human(spec));
            }
        }
        if !diags.is_empty() {
            worst = 2;
        }
    }
    ExitCode::from(worst)
}

/// `hauksbee-ci init <board>`: scaffold a starter spec and print where it landed.
/// Board / bind errors carry the crate's loud, did-you-mean style; exit 2 on any
/// failure to match the spec-error contract of `run`.
fn cmd_init(args: InitArgs) -> ExitCode {
    match hauksbee_ci::init::init_to(&args.board, args.out.as_deref()) {
        Ok(path) => {
            println!("wrote starter spec to {}", path.display());
            println!("edit it, then run:  hauksbee-ci run {}", path.display());
            // Where the documented pre-commit hook looks: `ci/` and the repo
            // root by default (HAUKSBEE_CI_SPECS, colon-separated). The spec
            // just landed wherever the user asked (default: right here), so
            // the advice is one line of orientation, not a relocation chore.
            println!(
                "\nthe pre-commit hook and the GitHub action discover specs in `ci/` and the\n\
                 repo root (override with HAUKSBEE_CI_SPECS, colon-separated). This one is at\n\
                 {}; if that is somewhere else, either move it or add its directory to\n\
                 HAUKSBEE_CI_SPECS. The `board = \"...\"` path inside it is already relative\n\
                 to the spec's own directory.",
                path.display()
            );
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("hauksbee-ci: {e}");
            ExitCode::from(2)
        }
    }
}
