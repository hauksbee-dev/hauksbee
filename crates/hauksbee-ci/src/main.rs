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
    version = hauksbee_ci::version_string(),
    about = "CI for hardware: boot firmware on the emulated PCB and assert rails, UART, and blink.",
    long_about = None,
    propagate_version = true,
    arg_required_else_help = true,
    after_help = "try it now: hauksbee-ci run --example blinky"
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
    Init(InitArgs),

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

    /// Write JUnit XML to this path (for the CI test-report step).
    #[arg(long, value_name = "OUT.XML")]
    junit: Option<PathBuf>,

    /// Suppress the per-assertion human report (exit code still reflects pass/fail).
    #[arg(long)]
    quiet: bool,

    /// Print the run result as one JSON object on stdout instead of the human
    /// report (the web checks panel and any tool consume this). Exit codes are
    /// unchanged; a spec/board error prints `{"ok":false,"error":...}`. The
    /// shape is published as a JSON Schema in
    /// crates/hauksbee-ci/schemas/hauksbee-ci-report.schema.json and documented
    /// in docs/ci/JSON_OUTPUT.md.
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
struct InitArgs {
    /// Board file to scaffold a spec from (.kicad_pcb, .kicad_sch, .net, .brd, .PcbDoc, .d356, .board).
    /// With no argument, looks for exactly one board file in the current
    /// directory and tells you the command to run on it.
    #[arg(value_name = "BOARD")]
    board: Option<PathBuf>,

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
    let mut args = match cli.command {
        Command::Run(args) => args,
        Command::Check(args) => return cmd_check(args),
        Command::Init(args) => return cmd_init(args),
        Command::Hook(HookCommand::Install) => return cmd_hook_install(),
        Command::Hook(HookCommand::Uninstall) => {
            return hauksbee_ci::integrate::run_hook_uninstall()
        }
        Command::GithubAction(args) => return cmd_github_action(args),
    };

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
            Err(e) => {
                eprintln!("hauksbee-ci: {e}");
                return ExitCode::from(2);
            }
        }
    }

    if let Some(path) = &args.emit_manifest {
        match capture_manifest(&args) {
            Ok(manifest) => {
                if let Err(e) = manifest.write_new(path) {
                    eprintln!("hauksbee-ci: {e}");
                    return ExitCode::from(2);
                }
                eprintln!(
                    "wrote immutable run manifest {} to {}",
                    manifest.manifest_id,
                    path.display()
                );
            }
            Err(e) => {
                eprintln!("hauksbee-ci: could not capture run manifest: {e}");
                return ExitCode::from(2);
            }
        }
    }

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
                    // The error variant of the published line shape (`ok:
                    // false` plus the sentence); see
                    // hauksbee_ci::report::CiJsonLine and docs/ci/JSON_OUTPUT.md.
                    println!(
                        "{}",
                        hauksbee_ci::report::CiJsonError::new(e.to_string()).render_json()
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

    // A GREEN run ends by pointing at whichever repo wiring (pre-commit hook,
    // GitHub workflow) is missing, and says nothing when both are in place.
    // RED runs end with the per-kind docs pointer inside the report instead.
    if worst == 0 && !args.quiet && !args.json {
        if let Ok(cwd) = std::env::current_dir() {
            if let Some(step) = hauksbee_ci::integrate::green_next_step(&cwd) {
                println!("{step}");
            }
        }
    }

    ExitCode::from(worst)
}

fn capture_manifest(args: &RunArgs) -> anyhow::Result<hauksbee_engine::run_manifest::RunManifest> {
    use std::collections::BTreeMap;

    use hauksbee_engine::run_manifest::{
        absolutize_argv_paths, board_sidecar_inputs, implicit_model_inputs, ManifestInput,
        ManifestRequest, RunManifest, ToolIdentity,
    };

    let mut inputs = Vec::new();
    for (index, spec_path) in args
        .specs
        .iter()
        .enumerate()
        .filter(|_| args.example.is_none())
    {
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
                let path = std::path::Path::new(path);
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    spec.base_dir.join(path)
                };
                inputs.push(ManifestInput::new(
                    format!("sensor_spec[{index}][{sensor_index}]"),
                    path,
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
                let path = std::path::Path::new(&channel.file);
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    trace.base_dir.join(path)
                };
                inputs.push(ManifestInput::new(
                    format!("hardware_trace_data[{index}][{assert_index}][{channel_index}]"),
                    path,
                ));
            }
        }
    }
    if let Some(path) = &args.models_dir {
        inputs.push(ManifestInput::new("models_dir", path));
    }
    inputs.extend(implicit_model_inputs());

    let options = BTreeMap::from([
        ("example".into(), serde_json::json!(args.example)),
        ("json".into(), serde_json::json!(args.json)),
        ("junit".into(), serde_json::json!(args.junit)),
        ("models_dir".into(), serde_json::json!(args.models_dir)),
        ("quiet".into(), serde_json::json!(args.quiet)),
        ("seed".into(), serde_json::json!(args.seed)),
        (
            "specs".into(),
            if args.example.is_some() {
                serde_json::json!([])
            } else {
                serde_json::json!(args.specs)
            },
        ),
    ]);
    let mut features = Vec::new();
    if cfg!(feature = "avr") {
        features.push("avr".to_string());
    }
    if cfg!(feature = "qemu") {
        features.push("qemu".to_string());
    }
    if cfg!(feature = "renode") {
        features.push("renode".to_string());
    }
    let replay_paths = args
        .specs
        .iter()
        .filter(|_| args.example.is_none())
        .cloned()
        .chain(args.models_dir.iter().cloned())
        .chain(args.junit.iter().cloned())
        .collect::<Vec<_>>();
    let base = std::env::current_dir()?;
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

/// `hauksbee-ci hook install`: exit 0 with the one-line outcome, 2 on error
/// (no repo, unreadable config), matching the spec-error contract.
fn cmd_hook_install() -> ExitCode {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("hauksbee-ci: cannot determine the current directory: {e}");
            return ExitCode::from(2);
        }
    };
    match hauksbee_ci::integrate::hook_install(&cwd) {
        Ok(msg) => {
            println!("{msg}");
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("hauksbee-ci: {e}");
            ExitCode::from(2)
        }
    }
}

/// `hauksbee-ci github-action [--write [PATH]]`.
fn cmd_github_action(args: GithubActionArgs) -> ExitCode {
    match args.write {
        None => match hauksbee_ci::integrate::try_github_workflow_yaml() {
            Ok(yaml) => {
                print!("{yaml}");
                ExitCode::from(0)
            }
            Err(e) => {
                eprintln!("hauksbee-ci: {e}");
                ExitCode::from(2)
            }
        },
        Some(path) => {
            let cwd = match std::env::current_dir() {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("hauksbee-ci: cannot determine the current directory: {e}");
                    return ExitCode::from(2);
                }
            };
            match hauksbee_ci::integrate::github_action_write(&cwd, &path) {
                Ok(msg) => {
                    println!("{msg}");
                    ExitCode::from(0)
                }
                Err(e) => {
                    eprintln!("hauksbee-ci: {e}");
                    ExitCode::from(2)
                }
            }
        }
    }
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

/// `hauksbee-ci init <board>`: scaffold a starter spec and print where it landed.
/// Board / bind errors carry the crate's loud, did-you-mean style; exit 2 on any
/// failure to match the spec-error contract of `run`.
fn cmd_init(args: InitArgs) -> ExitCode {
    let Some(board) = args.board else {
        return suggest_board_in_cwd();
    };
    match hauksbee_ci::init::init_to(&board, args.out.as_deref()) {
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
            // Both integrations install from the tool, so the spec is one
            // command away from actually gating anything.
            println!(
                "\nwire it in:\n  \
                 hauksbee-ci hook install          # block commits that break it\n  \
                 hauksbee-ci github-action --write # run it on every push / PR"
            );
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("hauksbee-ci: {e}");
            ExitCode::from(2)
        }
    }
}

/// `hauksbee-ci init` with no board: when the current directory holds exactly
/// one board file, name the exact command; otherwise list what was found.
/// Always exit 2 (nothing was scaffolded).
fn suggest_board_in_cwd() -> ExitCode {
    const BOARD_EXTS: &[&str] = &["kicad_pcb", "kicad_sch", "net", "brd", "d356", "board"];
    let mut boards: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(".") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let ext_matches = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| BOARD_EXTS.contains(&e) || e.eq_ignore_ascii_case("pcbdoc"));
            if path.is_file() && ext_matches {
                boards.push(name);
            }
        }
    }
    boards.sort();
    match boards.as_slice() {
        [] => eprintln!(
            "hauksbee-ci: init needs a board file and none was found here. \
             Run `hauksbee-ci init <board>` with a .kicad_pcb, .kicad_sch, \
             .net, .brd, .PcbDoc, .d356 or .board file."
        ),
        [one] => eprintln!("hauksbee-ci: found {one}; run:  hauksbee-ci init {one}"),
        many => eprintln!(
            "hauksbee-ci: found {} board files here ({}); pick one: \
             hauksbee-ci init <board>",
            many.len(),
            many.join(", ")
        ),
    }
    ExitCode::from(2)
}
