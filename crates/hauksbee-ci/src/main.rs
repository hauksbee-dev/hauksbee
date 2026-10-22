//! `hauksbee-ci` CLI.
//!
//! ```text
//! hauksbee-ci run <spec.toml> [--junit <out.xml>] [--quiet]
//! ```
//!
//! Exit code is 0 if every assertion passed, 1 if any ordinarily failed, 2 on a
//! usage/spec error, and 3 when the run is invalid for analysis (an analog chunk
//! failed to converge under an assertion's window, or the strict abort tripped;
//! ). When `GITHUB_ACTIONS` is set in the environment, GitHub workflow
//! annotations are emitted to stderr so failures surface inline without
//! contaminating machine-readable stdout.
//!
//! The argument surface is defined with `clap` (derive API): `--help`/`-h`,
//! usage-on-error, and did-you-mean suggestions all come for free.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use hauksbee_ci::{run, RunConfig};
use hauksbee_ir::{eprint, eprintln, print, println};

/// CI for hardware: run a board+firmware spec headless and assert on the result.
///
/// Point it at a TOML spec and it boots the firmware on the emulated PCB, then
/// asserts on rails, UART, blink rate and stress faults. Exits 0 if every
/// assertion passed, 1 if any failed, 2 on a spec/usage error, 3 when the analog
/// co-sim did not converge under an assertion's window (invalid for analysis).
/// Writes `JUnit` XML with --junit and emits GitHub annotations under `GITHUB_ACTIONS`.
#[derive(Parser)]
#[command(
    name = "hauksbee-ci",
    version = hauksbee_ci::version_string(),
    about = "CI for hardware: boot firmware on the emulated PCB and assert rails, UART, and blink.",
    long_about = None,
    propagate_version = true,
    arg_required_else_help = true,
    after_help = "try it now: hauksbee-ci run --example blinky",
    after_long_help = "try it now: hauksbee-ci run --example blinky\n\n\
Exit codes (hauksbee-ci run / check):\n  \
0  every assertion held (GREEN); check: every spec valid\n  \
1  an assertion failed (RED)\n  \
2  spec/board error, usage error, or unwritable output; check: any diagnostic\n  \
3  invalid for analysis: the result cannot be trusted"
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
    /// missing board, unknown net); 3 invalid for analysis (the result
    /// cannot be trusted: an aborted analog solve, undermined evidence, or a
    /// host-paced-clock claim that cannot be judged, such as absent firmware
    /// output or a level that holds only inside its window).
    ///
    /// Sibling contract: `hauksbee run` numbers 1 and 2 differently there
    /// (1 = input error: the board could not be read; 2 = usage error, or
    /// findings under --strict); each binary's --help states its own contract.
    #[command(verbatim_doc_comment)]
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
    /// It also resolves BOM, placement, variant and firmware files; firmware
    /// existence and obvious extension/content mismatches are checked. What it
    /// does NOT check (these need a run or a circuit bind): firmware loadability
    /// on the target core, sensor-TOML contents, scenario part/profile
    /// resolution, tolerance patterns matching real components, MCU/emulator
    /// availability, and of course every behavioral assertion.
    ///
    /// Independent errors are reported together in one invocation rather than
    /// one per run, in file order. The exception is an error that stops the file
    /// becoming a spec at all: a TOML syntax error, an unknown key, a missing
    /// required key. Deserialization stops at the first of those, so it is
    /// reported alone and the validations below it do not run; fix it and
    /// re-run to see the rest.
    ///
    /// Exit code: 0 when every spec is valid, 2 otherwise.
    #[command(verbatim_doc_comment)]
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
    // clap prints this doc as --help, where backticks would be pasted into a shell.
    #[allow(clippy::doc_markdown)]
    #[command(verbatim_doc_comment)]
    Init(InitArgs),

    /// Add one assertion to a spec from one command line, checked before it is written.
    ///
    /// Say what you saw or what you expect and the assertion is appended to the
    /// spec, after the same checks `hauksbee-ci check` makes (a field the kind
    /// does not have, a value of the wrong type, a net the board lacks). With
    /// --observe the numbers come from one nominal run instead of from you.
    ///
    /// Examples:
    ///   hauksbee-ci add ci/blink.toml toggle net=D13 freq_hz=5 tolerance=0.2
    ///   hauksbee-ci add ci/blink.toml toggle net=D13 --observe
    ///   hauksbee-ci add ci/blink.toml uart contains="hauksbee-demo v1"
    // clap prints this doc as --help, where backticks would be pasted into a shell.
    #[allow(clippy::doc_markdown)]
    #[command(verbatim_doc_comment)]
    Add(AddArgs),

    /// List the assertion kinds, or show one with its fields and an example.
    ///
    /// With no KIND it prints every kind the spec loader accepts and what it
    /// checks. With a KIND it prints that kind's required and optional fields,
    /// what else the spec must declare, a TOML example that passes `check`, and
    /// the usual reason it fails.
    ///
    /// Example:
    ///   hauksbee-ci explain voltage
    #[command(verbatim_doc_comment)]
    Explain(ExplainArgs),

    /// Check a spec the way an editor does: findings with a character range and a one-click fix.
    ///
    /// Prints one JSON object, `{"ok":true,"diagnostics":[...]}`; each diagnostic
    /// has `severity`, `code`, `start`/`end` (UTF-16 offsets into the file),
    /// `message` and, when the cure is mechanical, `fix`. The file may omit
    /// `board` and `firmware`; the app supplies them.
    Lint(LintArgs),

    /// Read one assertion's value off a single nominal run, with no bound and no verdict.
    ///
    /// Prints one JSON object: the reading, its range and unit, and the
    /// uncertainty the run admits. A reading the run cannot honestly take is
    /// `{"ok":false,"error":...}`, never a number.
    Measure(MeasureArgs),

    /// Install the pre-commit gate into this repository.
    ///
    /// Detects which hook mechanism the repo uses: a repo with a
    /// `.pre-commit-config.yaml` gets the hauksbee-ci entry added to it (then
    /// run `pre-commit install`); any other git repo gets a plain, self-
    /// contained `.git/hooks/pre-commit`. Idempotent: re-running changes
    /// nothing.
    #[command(subcommand)]
    Hook(HookCommand),

    /// Print the GitHub Actions workflow that runs hauksbee-ci on every push
    /// and pull request. `--write` writes it to
    /// `.github/workflows/hauksbee.yml` (or a path you pass) instead;
    /// idempotent, and it refuses to overwrite a diverged file.
    GithubAction(GithubActionArgs),
}

#[derive(Subcommand)]
enum HookCommand {
    /// Wire the pre-commit gate into the repository containing the current
    /// directory.
    Install,
    /// Remove the pre-commit gate hauksbee-ci installed (the plain hook block
    /// or the .pre-commit-config.yaml entry). Refuses to touch a hook it did
    /// not write.
    Uninstall,
}

#[derive(Parser)]
struct GithubActionArgs {
    /// Write the workflow to this path instead of printing it. Passing the
    /// flag with no value writes `.github/workflows/hauksbee.yml`.
    #[arg(
        long,
        value_name = "PATH",
        num_args = 0..=1,
        default_missing_value = ".github/workflows/hauksbee.yml"
    )]
    write: Option<PathBuf>,
}

#[derive(Parser)]
struct RunArgs {
    /// The hauksbee-ci TOML spec(s) to run. More than one (spelled out or via
    /// a shell glob, `hauksbee-ci run ci/*.toml`) runs each in turn,
    /// aggregates one summary, merges everything into one --junit file, and
    /// exits with the worst code of the set (severity order 3 > 2 > 1 > 0).
    /// Optional only because `--example` takes its place.
    //
    // Deliberately NOT `required_unless_present = "example"`: clap's message for
    // that contradicted the help it sends the reader to (`<SPEC>...` required
    // against `[SPEC]...` optional) and mentioned neither the flag that makes it
    // optional nor how to get a spec. `missing_spec_error` says all three.
    #[arg(value_name = "SPEC", num_args = 1..)]
    specs: Vec<PathBuf>,

    /// Run an embedded example instead of a spec file (try `blinky`). The
    /// example's spec, board and firmware are compiled into the binary and
    /// materialized under the temp directory, so this works with no checkout
    /// on disk.
    #[arg(long, value_name = "NAME", conflicts_with = "specs")]
    example: Option<String>,

    /// Write `JUnit` XML to this path (for the CI test-report step).
    #[arg(long, value_name = "OUT.XML")]
    junit: Option<PathBuf>,

    /// Suppress the per-assertion human report (exit code still reflects pass/fail).
    #[arg(long)]
    quiet: bool,

    /// Also print each assertion's numeric error budget and the provenance of
    /// every model behind it (hidden by default; JSON and `JUnit` always carry them).
    #[arg(long, short = 'v')]
    verbose: bool,

    /// Print the run result as one JSON object on stdout instead of the human
    /// report (the web checks panel and any tool consume this). Exit codes are
    /// unchanged; a spec/board error prints `{"ok":false,"error":...}`. The
    /// shape is published as a JSON Schema in
    /// crates/hauksbee-ci/schemas/hauksbee-ci-report.schema.json and documented
    /// in `docs/ci/JSON_OUTPUT.md`.
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
    /// `[[models]] kind = "mcu"` routing entries for user `SoC` descriptors.
    /// Replaces the spec's `models_dir` key for this run.
    #[arg(long, value_name = "DIR")]
    models_dir: Option<PathBuf>,

    /// Vendor SPICE model file (`.lib`, `.cir`, `.mod`, `.sp`, `.ckt`).
    /// Repeatable, and the same flag `hauksbee run` has. Every `.model` /
    /// `.subckt` card in it claims the parts whose Value or MPN is the card's
    /// own name, above every model directory.
    #[arg(long, value_name = "FILE")]
    spice_models: Vec<PathBuf>,

    /// Write the sampled time series of every net an assertion names, per
    /// seed, as `time_s,<net>` CSV under DIR/<spec-stem>/ (whole run plus each
    /// assertion's window, with an index.json). Opt-in; the report is unchanged.
    #[arg(long, value_name = "DIR")]
    waveforms: Option<PathBuf>,

    /// Write a canonical, immutable JSON reproduction manifest. It hashes the
    /// specs and every resolved board/firmware/overlay/model/trace input,
    /// records exact seeds/options/tool versions and safe environment selectors,
    /// and refuses to overwrite an existing file. Replay it with
    /// `hauksbee reproduce <FILE>`.
    #[arg(long, value_name = "FILE")]
    emit_manifest: Option<PathBuf>,
}

#[derive(Parser)]
struct CheckArgs {
    /// The hauksbee-ci TOML spec(s) to validate.
    #[arg(value_name = "SPEC", num_args = 1.., required = true)]
    specs: Vec<PathBuf>,

    /// Skip resolving the on-disk artifacts (the board file AND the firmware
    /// image): parse + structural validation only. Net and component references
    /// are then NOT validated (they need the board) and the firmware path is not
    /// checked to exist, so a clean exit means "structurally valid", not "will
    /// run". Use it in an editor loop where neither artifact is built yet.
    #[arg(long)]
    no_board: bool,

    /// Extra model directory, layered above the builtin db, installed packs,
    /// and user model dirs. Uses the same authority as `run --models-dir`, so
    /// BOM/placement identity reconciliation cannot disagree between check and
    /// run merely because a repository carries custom parts.
    /// Replaces the spec's `models_dir` key for this run.
    #[arg(long, value_name = "DIR")]
    models_dir: Option<PathBuf>,

    /// Print diagnostics as JSON on stdout: one array per spec, one per line
    /// (a single spec prints a single array). Each element is
    /// {"line", "col", "code", "message", "fix"}; line/col/fix are omitted
    /// when not derivable. A valid spec prints an empty array.
    #[arg(long)]
    json: bool,
}

#[derive(Parser)]
struct ExplainArgs {
    /// The assertion kind to describe (for example `voltage`); omit to list them all.
    #[arg(value_name = "KIND")]
    kind: Option<String>,

    /// Print the whole catalogue as one JSON object (the app reads this).
    #[arg(long, conflicts_with_all = ["kind", "schema"])]
    json: bool,

    /// Print the spec's JSON Schema with the catalogue's words attached to every
    /// assertion property (`x-hauksbee-doc`) and the glossary (`x-hauksbee-glossary`).
    #[arg(long, conflicts_with = "kind")]
    schema: bool,
}

#[derive(Parser)]
struct LintArgs {
    /// The spec text to lint (a TOML file).
    #[arg(value_name = "SPEC")]
    spec: PathBuf,
}

#[derive(Parser)]
struct MeasureArgs {
    /// The spec to run.
    #[arg(value_name = "SPEC")]
    spec: PathBuf,

    /// Which `[[assert]]` block to read, counting from 0.
    #[arg(long, value_name = "N")]
    assertion: usize,
}

#[derive(Parser)]
struct InitArgs {
    /// Board file to scaffold a spec from (any format `hauksbee-ci run` reads:
    /// KiCad, Eagle, Altium, EasyEDA, IPC-2581, IPC-D-356, ODB++ or a fab archive).
    /// With no argument, looks for exactly one board file in the current
    /// directory and tells you the command to run on it.
    #[arg(value_name = "BOARD")]
    board: Option<PathBuf>,

    /// Where to write the spec: a path ending in .toml is the spec file, and
    /// anything else is a directory (created if missing) that gets
    /// <board-stem>.toml inside it. Default: the current directory.
    #[arg(long, value_name = "PATH")]
    out: Option<PathBuf>,

    /// Print the supplies the starter spec would declare as one JSON object
    /// and write no file (the web app seeds its Power supplies from this).
    #[arg(long, conflicts_with = "out")]
    json: bool,

    /// The firmware image (.elf or .hex) to boot once and propose assertions
    /// from. Needs --suggest-asserts.
    #[arg(
        long,
        value_name = "IMAGE",
        requires = "suggest_asserts",
        conflicts_with = "json"
    )]
    firmware: Option<PathBuf>,

    /// Boot --firmware once and append what it did to the starter spec as
    /// commented assertions: the UART text it printed, the entry point it
    /// reached, and the rate of every net that toggled. Uncomment the ones you
    /// want, or edit a number to say what you expect instead.
    #[arg(long, requires = "firmware", conflicts_with = "json")]
    suggest_asserts: bool,

    /// How long to run the firmware for --suggest-asserts (default 1000 ms,
    /// enough to time a blink).
    #[arg(long, value_name = "MS", requires = "suggest_asserts")]
    duration_ms: Option<f64>,
}

#[derive(Parser)]
struct AddArgs {
    /// The spec to add the assertion to.
    #[arg(value_name = "SPEC")]
    spec: PathBuf,

    /// The assertion kind (`hauksbee-ci explain` lists them).
    #[arg(value_name = "KIND")]
    kind: String,

    /// The assertion's fields as key=value (`hauksbee-ci explain KIND` lists
    /// them), for example net=D13 freq_hz=5.
    // Same as the `Add` variant: this is --help text, not rustdoc.
    #[allow(clippy::doc_markdown)]
    #[arg(value_name = "KEY=VALUE")]
    fields: Vec<String>,

    /// Take the numbers you leave out from one nominal run of the spec, by the
    /// same margins the app uses (toggle and voltage).
    #[arg(long)]
    observe: bool,

    /// Check the assertion and print the TOML it would add; write nothing.
    #[arg(long)]
    dry_run: bool,
}

fn main() -> ExitCode {
    use hauksbee_ci::integrate;
    // A killed CI run must not orphan its co-sim emulators: reap every live
    // Renode/QEMU child on SIGTERM/SIGINT. See hauksbee_mcu::children.
    hauksbee_mcu::children::install_signal_reaper();
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => cmd_run(args),
        Command::Check(args) => cmd_check(args),
        Command::Init(args) => cmd_init(args),
        Command::Add(args) => cmd_add(&args),
        Command::Explain(args) => cmd_explain(&args),
        Command::Lint(args) => cmd_lint(&args),
        Command::Measure(args) => cmd_measure(&args),
        Command::Hook(HookCommand::Install) => {
            report(cwd().and_then(|cwd| integrate::hook_install(&cwd)))
        }
        Command::Hook(HookCommand::Uninstall) => {
            report(cwd().and_then(|cwd| integrate::hook_uninstall(&cwd)))
        }
        Command::GithubAction(GithubActionArgs { write: None }) => {
            report(integrate::try_github_workflow_yaml().map(|yaml| {
                print!("{yaml}");
                String::new()
            }))
        }
        Command::GithubAction(GithubActionArgs { write: Some(path) }) => {
            report(cwd().and_then(|cwd| integrate::github_action_write(&cwd, &path)))
        }
    }
}

// The run command end to end: the per-spec loop and the aggregate output share its state.
#[allow(clippy::too_many_lines)]
fn cmd_run(mut args: RunArgs) -> ExitCode {
    if args.specs.is_empty() && args.example.is_none() {
        eprintln!("{}", missing_spec_error());
        return ExitCode::from(2);
    }

    // --example: materialize the embedded example and run its spec like any
    // other. The suggestion paths promise this works from a bare binary.
    if let Some(name) = &args.example {
        match hauksbee_ci::examples::materialize(name) {
            Ok(spec) => {
                if !args.quiet && !args.json {
                    println!("example '{name}' materialized at {}", spec.display());
                }
                args.specs = vec![spec];
            }
            Err(e) => return fail(e),
        }
    }

    if let Some(path) = &args.emit_manifest {
        match capture_manifest(&args) {
            Ok(manifest) => {
                if let Err(e) = manifest.write_new(path) {
                    return fail(e);
                }
                eprintln!(
                    "wrote immutable run manifest {} to {}",
                    manifest.manifest_id,
                    path.display()
                );
            }
            Err(e) => return fail(format_args!("could not capture run manifest: {e}")),
        }
    }

    // A co-sim is minutes of silence otherwise, which reads as a hang. At a
    // terminal it narrates phases and prints a line every 10 s; with stderr
    // redirected (a CI log) only a once-a-minute heartbeat survives, so a log
    // never fills with progress but a long run is never silent. `--quiet` asked
    // for silence and `--json` output is parsed, so both stay quiet.
    if !args.quiet && !args.json {
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

    for (index, spec) in args.specs.iter().enumerate() {
        let cfg = RunConfig {
            spec: spec.clone(),
            seed: args.seed,
            models_dir: args.models_dir.clone(),
            spice_models: args.spice_models.clone(),
            waveforms: args.waveforms.clone(),
        };
        let code = match run(&cfg) {
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
                    print!(
                        "{}",
                        result.render_human_for(args.verbose, Some(&spec.display().to_string()))
                    );
                }
                if args.junit.is_some() {
                    suites.push(result.junit_suite());
                }
                // What gated this spec, by assertion label, for the manifest's
                // recorded outcome (waived reds do not gate).
                hauksbee_engine::run_manifest::note_gating_findings(
                    &format!("spec[{index}]"),
                    result
                        .results
                        .iter()
                        .filter(|r| (!r.passed || r.invalid) && r.waived.is_none())
                        .map(|r| r.label.clone()),
                );
                // Annotations are a human surface, so they go to stderr, which
                // GitHub reads workflow commands from just as it does stdout.
                // On stdout they were corrupting the one contract this binary
                // makes to a machine: under --json, stdout is NDJSON and one
                // line per spec, and every ::notice landed in the middle of it.
                if github {
                    // Keep the published `--json` stdout contract as pure
                    // NDJSON. GitHub processes workflow commands from stderr
                    // too, so annotations remain visible without becoming a
                    // second, non-JSON output line.
                    eprint!("{}", result.render_github_annotations());
                }
                result.exit_code()
            }
            Err(e) => {
                // Spec / board errors: surface as a GitHub error too, count
                // toward the merged JUnit, and keep running the REST of the
                // set: in a multi-spec invocation one desynced spec must not
                // hide the others' verdicts.
                if args.json {
                    // The error variant of the published line shape (`ok:
                    // false` plus the sentence); see
                    // hauksbee_ci::report::CiJsonLine and docs/ci/JSON_OUTPUT.md.
                    println!(
                        "{}",
                        hauksbee_ci::report::CiJsonError::new(e.to_string()).render_json()
                    );
                }
                eprintln!("{}", e.cli_line(&spec.display().to_string()));
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
                    eprintln!(
                        "::error title=hauksbee-ci spec error::{}",
                        hauksbee_ci::report::gh_escape(&e.to_string())
                    );
                }
                e.exit_code()
            }
        };
        let code = u8::try_from(code).unwrap_or(u8::MAX);
        verdicts.push((spec.display().to_string(), code));
        worst = worst.max(code);
    }

    if let Some(path) = &args.junit {
        let xml = hauksbee_ci::report::render_junit_document(&suites);
        if let Err(e) = std::fs::write(path, xml) {
            eprintln!(
                "hauksbee-ci: could not write JUnit XML to {}: {e}",
                path.display()
            );
            hauksbee_engine::run_manifest::finish_outcome("hauksbee-ci", i32::from(2u8.max(worst)));
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

    // A GREEN run ends by pointing at whichever repo wiring (pre-commit hook,
    // GitHub workflow) is missing, and says nothing when both are in place.
    // RED and INVALID runs end with the fix inside the report instead.
    if worst == 0 && !args.quiet && !args.json {
        if let Some(step) = cwd()
            .ok()
            .and_then(|cwd| hauksbee_ci::integrate::green_next_step(&cwd))
        {
            println!("{step}");
        }
    }

    hauksbee_engine::run_manifest::finish_outcome("hauksbee-ci", i32::from(worst));
    ExitCode::from(worst)
}

/// What `hauksbee-ci run` with nothing to run says. Names both ways to give it
/// something (a spec file, or the bundled example that needs no checkout) and
/// the command that writes a first spec, because "the following required
/// arguments were not provided: <SPEC>..." tells someone with no spec yet
/// nothing they can act on.
fn missing_spec_error() -> String {
    "hauksbee-ci: run needs a spec to run, and none was given.\n\
     \x20 a spec file:        hauksbee-ci run ci/power-up.toml\n\
     \x20 several at once:    hauksbee-ci run ci/*.toml\n\
     \x20 no spec yet:        hauksbee-ci run --example blinky   (bundled, needs no checkout)\n\
     \x20 scaffold your own:  hauksbee-ci init <board>"
        .to_string()
}

/// The repository-integration commands (`hook install` / `hook uninstall` /
/// `github-action`) share one contract: exit 0 with the one-line outcome on
/// stdout, 2 with the reason on stderr (no repo, unreadable config), matching
/// the spec-error exit code. An empty outcome prints nothing (the workflow
/// YAML was already written to stdout).
fn report(outcome: anyhow::Result<String>) -> ExitCode {
    match outcome {
        Ok(msg) => {
            if !msg.is_empty() {
                println!("{msg}");
            }
            ExitCode::from(0)
        }
        Err(e) => fail(e),
    }
}

/// Print a spec/usage-class error and return its exit code (2).
fn fail(e: impl std::fmt::Display) -> ExitCode {
    eprintln!("hauksbee-ci: {e}");
    ExitCode::from(2)
}

fn cwd() -> anyhow::Result<PathBuf> {
    std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("cannot determine the current directory: {e}"))
}

/// `hauksbee-ci check <spec>...`: load-only validation, no simulation. Exit 0
/// when every spec is valid, 2 when any produced diagnostics (matching `run`'s
/// spec-error exit code).
fn cmd_check(args: CheckArgs) -> ExitCode {
    let opts = hauksbee_ci::check::CheckOptions {
        no_board: args.no_board,
        models_dir: args.models_dir,
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

/// `hauksbee-ci explain [kind]`: the assertion catalogue, from the table the report
/// and the app read. An unknown kind is a usage error that names the nearest kind.
fn cmd_explain(args: &ExplainArgs) -> ExitCode {
    use hauksbee_ci::catalog;
    if args.schema {
        return report(Ok(catalog::editor_schema().to_string()));
    }
    report(match (&args.kind, args.json) {
        (_, true) => Ok(catalog::to_json().to_string()),
        (None, _) => Ok(catalog::render_list().trim_end().to_string()),
        (Some(kind), _) => catalog::entry(kind)
            .map(|e| catalog::render_entry(e).trim_end().to_string())
            .ok_or_else(|| {
                let kinds: Vec<&str> = catalog::CATALOG.iter().map(|e| e.kind).collect();
                anyhow::anyhow!(
                    "unknown assertion kind '{kind}'{}; see: hauksbee-ci explain",
                    hauksbee_ci::error::did_you_mean_hint(kind, &kinds)
                )
            }),
    })
}

/// `hauksbee-ci lint <spec>`: the editor's findings as JSON; exit 0 whatever they are.
fn cmd_lint(args: &LintArgs) -> ExitCode {
    report(
        std::fs::read_to_string(&args.spec)
            .map(|text| hauksbee_ci::lint::lint_json(&text).to_string())
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", args.spec.display())),
    )
}

/// `hauksbee-ci measure <spec> --assertion N`: one reading as JSON. A reading the run
/// could not take is `{"ok":false,"error":..}` and exit 2.
fn cmd_measure(args: &MeasureArgs) -> ExitCode {
    match hauksbee_ci::measure::measure(&args.spec, args.assertion) {
        Ok(reading) => {
            println!("{reading}");
            ExitCode::from(0)
        }
        Err(error) => {
            println!("{}", serde_json::json!({ "ok": false, "error": error }));
            ExitCode::from(2)
        }
    }
}

/// `hauksbee-ci init <board>`: scaffold a starter spec and print where it landed.
/// Board / bind errors carry the crate's loud, did-you-mean style; exit 2 on any
/// failure to match the spec-error contract of `run`.
fn cmd_init(args: InitArgs) -> ExitCode {
    let Some(board) = args.board else {
        return report(Err(suggest_board_in_cwd()));
    };
    if args.json {
        use hauksbee_ir::progress::{Plan, Stage};
        // The scaffold reads and binds the board, then (when a regulator might need a
        // supply) runs it for a millisecond: no copper, lint or findings of its own.
        let plan = || Plan {
            bytes: std::fs::metadata(&board).map_or(0, |m| m.len()),
            simulate_s: Some(0.001),
        };
        let unused = [
            Stage::Models,
            Stage::Copper,
            Stage::Nets,
            Stage::Signals,
            Stage::Report,
        ];
        return hauksbee_ci::progress::observed(plan, &unused, || {
            report(
                hauksbee_ci::init::scaffold(&board)
                    .map(|found| {
                        serde_json::json!({ "ok": true, "supplies": found.supplies }).to_string()
                    })
                    .map_err(anyhow::Error::from),
            )
        });
    }
    if let (Some(firmware), true) = (&args.firmware, args.suggest_asserts) {
        return report(
            hauksbee_ci::init::init_observing(
                &board,
                args.out.as_deref(),
                hauksbee_ci::init::Observe {
                    firmware,
                    duration_ms: args.duration_ms,
                },
            )
            .map(|found| {
                let path = found.path.display();
                let mut msg = format!("wrote starter spec to {path}\n");
                if let Some(why) = &found.warning {
                    let _ = writeln!(
                        msg,
                        "the firmware run did not finish, so nothing was proposed:\n  {why}"
                    );
                } else if found.proposed.is_empty() {
                    msg.push_str("the run showed nothing to assert on\n");
                } else {
                    msg.push_str("proposed (commented at the end of the file):\n");
                    for line in &found.proposed {
                        let _ = writeln!(msg, "  {line}");
                    }
                }
                let _ = write!(
                    msg,
                    "uncomment the ones you want, then run:  hauksbee-ci run {path}\n\
                     or add one directly:  hauksbee-ci add {path} <kind> key=value"
                );
                msg
            })
            .map_err(anyhow::Error::from),
        );
    }
    report(
        hauksbee_ci::init::init_to(&board, args.out.as_deref())
            .map(|path| {
                // Where the documented pre-commit hook looks: `ci/` and the
                // repo root by default (HAUKSBEE_CI_SPECS, colon-separated).
                // The spec just landed wherever the user asked (default: right
                // here), so the advice is one line of orientation, not a
                // relocation chore. Both integrations install from the tool,
                // so the spec is one command away from actually gating anything.
                let path = path.display();
                format!(
                    "wrote starter spec to {path}\n\
                     edit it, then run:  hauksbee-ci run {path}\n\n\
                     the pre-commit hook and the GitHub action discover specs in `ci/` and the\n\
                     repo root (override with HAUKSBEE_CI_SPECS, colon-separated). This one is at\n\
                     {path}; if that is somewhere else, either move it or add its directory to\n\
                     HAUKSBEE_CI_SPECS. The `board = \"...\"` path inside it is already relative\n\
                     to the spec's own directory.\n\n\
                     wire it in:\n  \
                     hauksbee-ci hook install          # block commits that break it\n  \
                     hauksbee-ci github-action --write # run it on every push / PR"
                )
            })
            .map_err(anyhow::Error::from),
    )
}

/// `hauksbee-ci add <spec> <kind> key=value...`: one assertion into a spec.
fn cmd_add(args: &AddArgs) -> ExitCode {
    report(
        hauksbee_ci::add::add_assertion(
            &args.spec,
            &args.kind,
            &args.fields,
            hauksbee_ci::add::AddOptions {
                observe: args.observe,
                dry_run: args.dry_run,
            },
        )
        .map(|added| {
            let verb = if added.wrote {
                "added to"
            } else {
                "would add to"
            };
            format!(
                "{verb} {}: {}\n\n{}",
                added.path.display(),
                added.label,
                added.block.trim_end()
            )
        })
        .map_err(anyhow::Error::msg),
    )
}

/// `hauksbee-ci init` with no board: when the current directory holds exactly
/// one board file, name the exact command; otherwise list what was found.
/// Always an error (nothing was scaffolded).
fn suggest_board_in_cwd() -> anyhow::Error {
    const BOARD_EXTS: &[&str] = &["kicad_pcb", "kicad_sch", "net", "brd", "d356"];
    let mut boards: Vec<String> = std::fs::read_dir(".")
        .into_iter()
        .flat_map(std::iter::Iterator::flatten)
        .filter(|entry| {
            let path = entry.path();
            path.is_file()
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| BOARD_EXTS.contains(&e) || e.eq_ignore_ascii_case("pcbdoc"))
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    boards.sort();
    match boards.as_slice() {
        [] => anyhow::anyhow!(
            "init needs a board file and none was found here. \
             Run `hauksbee-ci init <board>` with one of: {}.",
            hauksbee_engine::board_input::supported_formats_sentence()
        ),
        [one] => anyhow::anyhow!("found {one}; run:  hauksbee-ci init {one}"),
        many => anyhow::anyhow!(
            "found {} board files here ({}); pick one: hauksbee-ci init <board>",
            many.len(),
            many.join(", ")
        ),
    }
}

fn capture_manifest(args: &RunArgs) -> anyhow::Result<hauksbee_engine::run_manifest::RunManifest> {
    use std::collections::BTreeMap;

    use hauksbee_engine::run_manifest::{
        absolutize_argv_paths, implicit_model_inputs, ManifestInput, ManifestRequest, RunManifest,
        ToolIdentity,
    };

    let mut inputs = Vec::new();
    for (index, spec_path) in args
        .specs
        .iter()
        .enumerate()
        .filter(|_| args.example.is_none())
    {
        spec_inputs(&mut inputs, index, spec_path, args.models_dir.is_none())?;
    }
    if let Some(path) = &args.models_dir {
        inputs.push(ManifestInput::new("models_dir", path));
    }
    for (index, path) in args.spice_models.iter().enumerate() {
        inputs.push(ManifestInput::new(format!("spice_models[{index}]"), path));
    }
    inputs.extend(implicit_model_inputs());

    // Paths are recorded absolute here and made project-relative by the
    // manifest, so an option never names the capturing directory.
    let base = std::env::current_dir()?;
    let absolute = |path: &PathBuf| base.join(path);
    // The ensemble each spec runs, resolved even when it is the default: the
    // seed count and the Monte-Carlo or corner mode decide which members run.
    let mut ensembles = Vec::new();
    for (index, spec_path) in args.specs.iter().enumerate() {
        let spec = hauksbee_ci::Spec::load(spec_path)?;
        ensembles.push(serde_json::json!({
            "spec": index,
            "seeds": spec.ensemble_seed_count(),
            "mode": format!("{:?}", spec.ensemble_mode()?),
        }));
    }
    let options = BTreeMap::from([
        ("ensembles".into(), serde_json::json!(ensembles)),
        ("example".into(), serde_json::json!(args.example)),
        ("json".into(), serde_json::json!(args.json)),
        (
            "junit".into(),
            serde_json::json!(args.junit.as_ref().map(absolute)),
        ),
        (
            "models_dir".into(),
            serde_json::json!(args.models_dir.as_ref().map(absolute)),
        ),
        ("quiet".into(), serde_json::json!(args.quiet)),
        ("seed".into(), serde_json::json!(args.seed)),
        (
            "spice_models".into(),
            serde_json::json!(args.spice_models.iter().map(absolute).collect::<Vec<_>>()),
        ),
        (
            "waveforms".into(),
            serde_json::json!(args.waveforms.as_ref().map(absolute)),
        ),
        (
            "specs".into(),
            if args.example.is_some() {
                serde_json::json!([])
            } else {
                serde_json::json!(args.specs.iter().map(absolute).collect::<Vec<_>>())
            },
        ),
    ]);
    let features = [
        ("avr", cfg!(feature = "avr")),
        ("qemu", cfg!(feature = "qemu")),
        ("renode", cfg!(feature = "renode")),
    ]
    .into_iter()
    .filter(|&(_, on)| on)
    .map(|(name, _)| name.to_string())
    .collect();
    let replay_paths = args
        .specs
        .iter()
        .filter(|_| args.example.is_none())
        .cloned()
        .chain(args.models_dir.iter().cloned())
        .chain(args.spice_models.iter().cloned())
        .chain(args.junit.iter().cloned())
        .collect::<Vec<_>>();
    RunManifest::capture(ManifestRequest {
        tool: ToolIdentity::workspace("hauksbee-ci"),
        command: absolutize_argv_paths(
            hauksbee_engine::run_manifest::replay_argv("hauksbee-ci"),
            &base,
            &replay_paths,
        ),
        options,
        inputs,
        feature_flags: features,
    })
}

/// The manifest inputs one spec (the `index`th) reads: the spec, its board and
/// sidecars, every optional file it names, and its sensor and trace data.
/// `spec_models_dir` is false when `--models-dir` replaces the spec's own.
fn spec_inputs(
    inputs: &mut Vec<hauksbee_engine::run_manifest::ManifestInput>,
    index: usize,
    spec_path: &std::path::Path,
    spec_models_dir: bool,
) -> anyhow::Result<()> {
    use hauksbee_engine::run_manifest::{board_sidecar_inputs, ManifestInput};

    let spec = hauksbee_ci::Spec::load(spec_path)?;
    inputs.push(ManifestInput::new(format!("spec[{index}]"), spec_path));
    let board_path = spec.board_path();
    inputs.push(ManifestInput::new(format!("board[{index}]"), &board_path));
    inputs.extend(board_sidecar_inputs(
        &board_path,
        &format!("board[{index}]"),
    ));
    if let Some(path) = spec.bom_path() {
        inputs.push(ManifestInput::new(format!("bom[{index}]"), path));
    }
    if let Some(path) = spec.placement_path() {
        inputs.push(ManifestInput::new(format!("placement[{index}]"), path));
    }
    if let Some(path) = spec.variant_path() {
        inputs.push(ManifestInput::new(format!("variant[{index}]"), path));
    }
    if let Some(path) = spec.models_dir_path().filter(|_| spec_models_dir) {
        inputs.push(ManifestInput::new(format!("models_dir[{index}]"), path));
    }
    if let Some(path) = spec.firmware_path() {
        inputs.push(ManifestInput::new(
            format!("firmware_source[{index}]"),
            &path,
        ));
        if let Some(resolved) = hauksbee_engine::firmware_input::resolve_firmware_cli(&path)? {
            if resolved.path != path {
                inputs.push(ManifestInput::new(
                    format!("firmware_resolved[{index}]"),
                    resolved.path,
                ));
            }
        }
    }
    if let Some(path) = spec.asbuilt_path() {
        inputs.push(ManifestInput::new(format!("asbuilt[{index}]"), path));
    }
    if let Some(path) = spec.mcu_descriptor_dir() {
        inputs.push(ManifestInput::new(
            format!("mcu_descriptor_dir[{index}]"),
            path,
        ));
    }
    for (sensor_index, sensor) in spec.sensors.iter().enumerate() {
        if let Some(path) = &sensor.spec_file {
            // `join` keeps an absolute path as it is.
            inputs.push(ManifestInput::new(
                format!("sensor_spec[{index}][{sensor_index}]"),
                spec.base_dir.join(path),
            ));
        }
    }
    for (assert_index, assertion) in spec.asserts.iter().enumerate() {
        if assertion.kind != "hwtrace" {
            continue;
        }
        let trace_path = hauksbee_ci::hwtrace::trace_path(&spec, assertion)?;
        let trace = hauksbee_ci::hwtrace::Trace::load(&trace_path)?;
        inputs.push(ManifestInput::new(
            format!("hardware_trace[{index}][{assert_index}]"),
            &trace_path,
        ));
        for (channel_index, channel) in trace.channels.iter().enumerate() {
            inputs.push(ManifestInput::new(
                format!("hardware_trace_data[{index}][{assert_index}][{channel_index}]"),
                trace.base_dir.join(&channel.file),
            ));
        }
    }
    Ok(())
}
