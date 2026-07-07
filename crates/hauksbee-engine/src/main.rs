//! `hauksbee` CLI: bind a board and bring it to life.
//!
//! ```text
//! hauksbee run        <board-file> [--firmware <hex>] [--seconds N] [--headless]
//!                                 [--port 3001] [--report] [--drc] [--ampacity] [--lint] [--si]
//!                                 [--resources] [--plain] [--strict]
//!                                 [--apply-shorts] [--models-dir <dir>]
//! hauksbee serve      [--port 3001]
//! hauksbee to-code    <board-file> [--out <file.board>]
//! hauksbee from-code  <code-file>  [--out <file.kicad_pcb>] [--relayout|--incremental]
//!                                 [--route|--route-grid]
//! hauksbee check-code <code-dir|file> [--seconds N] [--destructive]
//! ```
//!
//! - `run --report`       : print the bind report table and exit.
//! - `run --drc`          : print the geometric short / clearance report and exit.
//! - `run --apply-shorts` : apply every detected copper short (bridge the nets)
//!                          before simulating, so the run shows the consequences.
//! - `run --headless`     : run the co-sim for `--seconds` and print summary stats.
//! - `run --plain`        : translate any report into a non-engineer-readable
//!                          verdict (what / why / fix). Alias `--explain`.
//! - `run --strict`       : exit 2 when a report finds a real defect (else 0).
//!                          Alias `--fail-on-findings`.
//! - `run --strict-boot`  : exit 2 when the co-sim raises a boot-safety advisory
//!                          (a switch-driving control net held HIGH at power-up
//!                          with no bias). Advisory-only without this flag.
//! - `run` (default)      : serve the live websocket (frontend/dist if present).
//! - `serve`              : local web front door: open a page, drop a board,
//!                          get the plain-language report in the browser.
//! - `to-code`            : decompile a board (`.kicad_pcb`, `.net`, IPC, Eagle)
//!                          into editable Board-as-Code text.
//! - `from-code`          : recompile Board-as-Code back into a `.kicad_pcb`.
//! - `check-code`         : recompile, bind, co-sim with the stress monitor, and
//!                          print a fault report (the edit -> simulate loop).
//!
//! The argument surface is defined with `clap` (derive API), so `--help`/`-h`
//! at the top level and per command, usage-on-error, and did-you-mean
//! suggestions all come for free.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use hauksbee_engine::binder::bind_board;
use hauksbee_engine::boardcode::{
    check_code, code_to_board_text, decompile_any_to_code, load_code, render_check_report,
    CheckOptions,
};
use hauksbee_engine::result::{
    BindSummary, strict_analog_exit_code, BootGateJson, CosimFailedWindow, CosimJson, JsonNote,
    JsonNoteKind, JsonReport, NetActivity, EXIT_INVALID_FOR_ANALYSIS,
};
use hauksbee_engine::HauksbeeEngine;
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;
use hauksbee_server::Server;

/// CI for hardware: hand it a PCB and it tells you what blows up before you fab.
///
/// Point `hauksbee` at any board file (KiCad, Eagle, IPC-D-356, or gerbers) and
/// it extracts the circuit, binds device models, runs the static checks, and can
/// co-simulate the firmware on an emulated MCU. Start with `run --report`.
#[derive(Parser)]
#[command(
    name = "hauksbee",
    version,
    about = "CI for hardware: hand it a PCB; it tells you what blows up before you order boards.",
    long_about = None,
    propagate_version = true,
    infer_subcommands = true,
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Suppress informational notes (the chatty `note:` lines on stderr, e.g.
    /// "other board file(s) found nearby"). Errors, warnings, and report output
    /// are unaffected. These notes are also silenced automatically under `--json`
    /// and when stdout is piped/redirected, so they never pollute machine or
    /// report output; `--quiet` also hides them for an interactive terminal.
    #[arg(long, global = true)]
    quiet: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Extract + bind a board, then check it or bring it to life.
    ///
    /// Accepts a KiCad `.kicad_pcb`/`.kicad_sch`, Eagle `.brd`, IPC-D-356,
    /// gerbers, Altium `.PcbDoc`, a KiCad `.net`, or Board-as-Code (`.board`,
    /// detected by extension or its DSL header) — all analysed identically.
    ///
    /// With no flag, serves the live 2D/3D frontend on a local websocket. The
    /// `--report`/`--drc`/`--lint`/`--si`/`--resources` flags each print one
    /// static report and exit; `--headless` runs the co-sim for `--seconds`.
    ///
    /// These reports are informational and exit 0 by default, even when they
    /// list findings. Add `--strict` to FAIL (exit 2) on a real defect, or
    /// `--plain` for a non-engineer-readable verdict. For the full assertion /
    /// fault flow gate on `hauksbee-ci` or `hauksbee check-code`.
    ///
    /// Example:
    ///   hauksbee run my_board.kicad_pcb --report
    ///   hauksbee run my_board.kicad_pcb --drc --plain --strict
    Run(RunArgs),

    /// Decompile a board into editable Board-as-Code text.
    ///
    /// Example:
    ///   hauksbee to-code my_board.kicad_pcb --out my_board.board
    ToCode(ToCodeArgs),

    /// Recompile Board-as-Code back into a `.kicad_pcb`.
    ///
    /// Optionally re-place the parts (`--relayout`/`--incremental`) and route
    /// them (`--route` via freerouting, or `--route-grid` for the in-tree A*).
    ///
    /// Example:
    ///   hauksbee from-code my_board.board --out my_board.kicad_pcb --route
    FromCode(FromCodeArgs),

    /// Recompile, bind, co-sim with the stress monitor, print a fault report.
    ///
    /// This is the edit -> simulate loop: exits non-zero if a fault is raised,
    /// so it drops straight into a script or pre-commit hook.
    ///
    /// Example:
    ///   hauksbee check-code my_board.board --seconds 0.2
    CheckCode(CheckCodeArgs),

    /// Simulate a SPICE deck (`.cir`) and write the results as CSV.
    ///
    /// Loads the netlist through the same loader the rest of the tool uses,
    /// runs the analysis the deck asks for (or the one a flag forces), and
    /// writes one column per probe. With no analysis flag: a `.tran` card runs
    /// a transient, otherwise the DC operating point (`.op`).
    ///
    /// Honesty: an analysis the front-end cannot yet feed (`--ac`, `--dc`) and
    /// an output format not yet built (`--format raw|both`) REFUSE with a clear
    /// message and a non-zero exit — never a silent no-op or a wrong number. A
    /// malformed deck prints the loader's line-numbered error and exits 2.
    ///
    /// Example:
    ///   hauksbee sim rc.cir --out rc.csv
    ///   hauksbee sim amp.cir --tran --print V(out) I(V1)
    Sim(SimArgs),

    /// Start the local web front door: a "drop your board, get a report" page.
    ///
    /// Opens a local web server with no board pre-loaded. Point a browser at the
    /// printed URL, drop a .kicad_pcb / .kicad_sch / .brd / gerber zip on the
    /// page, and get the plain-language verdict, the full report, and a 2D map
    /// of the parts — no terminal needed beyond this command. Nothing is
    /// uploaded off the machine; the analysis runs in this process.
    ///
    /// Example:
    ///   hauksbee serve --port 3001
    Serve(ServeArgs),

    /// Report which co-simulation backends this build can actually locate.
    ///
    /// Runs the ENGINE's own backend discovery — the same `find_qemu` /
    /// `find_renode` a co-sim would use — and prints, for each backend, the
    /// resolved binary path or that it is absent. This is the authoritative
    /// probe `scripts/doctor.sh` calls, so the shell tool can never disagree
    /// with the engine: a Homebrew mainline `qemu-system-xtensa` that has no
    /// `esp32` machine is reported absent here exactly as the co-sim rejects it,
    /// and a `~/renode-portable` install the co-sim finds is reported present.
    ///
    /// stdout is one machine-parseable line per backend
    /// (`NAME<TAB>STATUS<TAB>PATH-OR-HINT`, STATUS a single lowercase token); the
    /// human header goes to stderr. `--json` emits a JSON object instead.
    ///
    /// Example:
    ///   hauksbee doctor --backends
    Doctor(DoctorArgs),

    /// Model-library tooling: validate model TOML before a board ever loads it.
    ///
    /// `models lint <file>` checks a `[[models]]` db file (params per kind,
    /// plus each entry's `[models.logic]` block: schema validation, expression
    /// compilation, and the exhaustive combinational-cycle convergence check)
    /// or a `[sensor]` register-map spec. Every failure is a NAMED error tied
    /// to its entry — the same validation binding performs, runnable
    /// standalone so a spec author (or an LLM extraction pipeline) fails fast.
    ///
    /// Example:
    ///   hauksbee models lint my_part.toml
    Models(ModelsArgs),
}

#[derive(Parser)]
struct ModelsArgs {
    #[command(subcommand)]
    command: ModelsCommand,
}

#[derive(Subcommand)]
enum ModelsCommand {
    /// Validate a model / sensor spec TOML file; exit 2 on any finding.
    Lint(ModelsLintArgs),
}

#[derive(Parser)]
struct ModelsLintArgs {
    /// A `[[models]]` db TOML file or a `[sensor]` spec TOML file.
    file: PathBuf,
}

#[derive(Parser)]
// The five "print one report and exit" flags pick the report to show, so they
// are mutually exclusive: pass at most one (clap errors clearly if you pass two,
// rather than the old parser's silent first-wins behaviour).
#[command(group(
    clap::ArgGroup::new("report_mode")
        .args(["report", "drc", "ampacity", "lint", "si", "resources", "thermal", "usb_c"])
        .multiple(false)
))]
struct RunArgs {
    /// Board file to load (.kicad_pcb, .kicad_sch, .brd, .d356, or gerbers).
    #[arg(value_name = "BOARD")]
    board: PathBuf,

    /// Firmware image to co-simulate on the board's MCU (e.g. an AVR .hex/.elf).
    #[arg(long, value_name = "HEX")]
    firmware: Option<PathBuf>,

    /// Seconds of simulated time to run under --headless.
    #[arg(long, default_value_t = 1.0, value_name = "N")]
    seconds: f64,

    /// Run the co-sim headless for --seconds and print summary stats (no server).
    #[arg(long)]
    headless: bool,

    /// Print the bind report table (every component -> device model) and exit.
    #[arg(long, group = "report_mode")]
    report: bool,

    /// Print the geometric copper short / clearance (DRC) report and exit.
    #[arg(long, group = "report_mode")]
    drc: bool,

    /// Print IPC-2221 trace-current capacity for power-like routed nets and exit.
    /// This is capacity-only unless a future spec supplies per-net current.
    #[arg(long, group = "report_mode")]
    ampacity: bool,

    /// Print the connectivity lint + strap-pin + resource-conflict report and exit.
    #[arg(long, group = "report_mode")]
    lint: bool,

    /// Print the signal-integrity / physics static-check report and exit.
    #[arg(long, group = "report_mode")]
    si: bool,

    /// Print only the MCU internal resource-conflict report and exit.
    #[arg(long, group = "report_mode")]
    resources: bool,

    /// Print the USB-C CC compliance report (the attach a compliant source sees
    /// from the receptacle's CC termination, and whether it applies VBUS) and
    /// exit. Flags the Raspberry-Pi-4-class shared-CC-pulldown fault.
    #[arg(long = "usb-c", group = "report_mode")]
    usb_c: bool,

    /// Run a short headless co-sim and print the steady-state junction-temperature
    /// estimate per dissipating device (Tj = Tambient + P * theta_JA), then exit.
    #[arg(long, group = "report_mode")]
    thermal: bool,

    /// Ambient temperature (C) for the --thermal estimate. Default 25 C.
    #[arg(long, default_value_t = 25.0, value_name = "C", help_heading = "Advanced / analyses")]
    ambient: f64,

    /// Translate the report into plain language for a non-engineer: a one-line
    /// verdict, then each finding as what it is, why it matters, and what to do.
    /// Applies to --drc/--lint/--si/--resources/--usb-c and to --headless faults.
    #[arg(long, visible_alias = "explain")]
    plain: bool,

    /// Emit machine-readable JSON instead of the box-drawing tables, for any of
    /// --report/--drc/--lint/--si/--resources/--usb-c/--thermal/--ac. Implies
    /// non-interactive, stable output (see docs schema §4.1). `valid:false` +
    /// `reason` is set on AC/thermal results that are meaningless; the bind
    /// section reports critical_parts_bound + active_path_unresolved by role.
    #[arg(long)]
    json: bool,

    /// Exit non-zero if a report (--drc/--lint/--si/--resources/--usb-c) finds problems,
    /// so it can gate a CI pipeline directly. Default stays exit 0 (scripts that
    /// only read the text are unaffected). Counts shorts + serious/medium lint &
    /// SI findings; clearance-only and low-severity notes do not fail the gate.
    #[arg(long, visible_alias = "fail-on-findings")]
    strict: bool,

    /// Opt-in: escalate a PARTIAL-coverage thermal result to exit 3 (invalid for
    /// analysis). By default a thermal table that is real but incomplete (rows
    /// exist while an active power IC on the live circuit is open/unresolved)
    /// emits a non-gating coverage caveat and still exits 0, so existing CI exit
    /// codes are unchanged. Pass this only when partial coverage must FAIL.
    #[arg(long, help_heading = "Advanced / analyses")]
    strict_thermal: bool,

    /// Opt-in: escalate the co-sim boot-safety advisories to exit 2. By default,
    /// heads-up notes about MCU control nets driven HIGH at boot — or left
    /// floating the whole run — with no bias resistor are advisory only and do
    /// not affect the exit code. Pass --strict-boot to fail CI on any such note.
    #[arg(long, help_heading = "Advanced / analyses")]
    strict_boot: bool,

    /// List the board's net names (sorted) and exit. Use it to find the exact net
    /// to pass to `--ac-node` / `--ac-loop` without grepping the layout file.
    #[arg(long)]
    list_nets: bool,

    /// Run ALL the static checks at once (bind + DRC + lint + signal integrity) in
    /// one report, instead of one flag at a time. Honours --plain / --json / --strict.
    #[arg(long, visible_alias = "all")]
    check: bool,

    /// Cross-check the geometric DRC against KiCad's own `kicad-cli pcb drc` (the
    /// oracle) and print whether they agree, so a copper finding is self-confirming
    /// without running a second tool by hand. Uses a `kicad-cli` found on PATH or
    /// in a standard install location (newest version preferred); KiCad is NOT
    /// bundled (see `docs/ORACLES.md`). No-op unless paired with `--drc`.
    #[arg(long, help_heading = "Advanced / analyses")]
    oracle: bool,

    /// Bridge every detected copper short before simulating (show the consequences).
    #[arg(long, help_heading = "Advanced / analyses")]
    apply_shorts: bool,

    /// Serve the live 2D/3D websocket frontend (the historical bare-`run`
    /// behaviour). With no report/serve flag on a TTY, `run` now launches the
    /// interactive terminal UI instead; pass `--serve` to keep the web frontend,
    /// or use the `hauksbee serve` subcommand.
    #[arg(long, help_heading = "Advanced / analyses")]
    serve: bool,

    /// Force the interactive terminal UI even when stdout is not a TTY (mainly
    /// for testing under a PTY). Normally the TUI is the auto-default for bare
    /// `run` on a TTY; this never triggers when a report flag is given.
    #[arg(long, help_heading = "Advanced / analyses")]
    tui: bool,

    /// Port for the live frontend websocket server (`--serve`).
    #[arg(long, default_value_t = 3001, value_name = "PORT", help_heading = "Advanced / analyses")]
    port: u16,

    /// Extra model directory (highest priority), layered over the built-in DB.
    #[arg(long, value_name = "DIR", help_heading = "Advanced / analyses")]
    models_dir: Option<PathBuf>,

    /// Small-signal AC sweep: `<fstart>:<fstop>:<points>[:lin]` (Hz; points per
    /// decade unless `:lin`). Linearises about the DC operating point and prints
    /// a Bode (magnitude dB + phase) table for `--ac-node`, then exits. Drive is
    /// a unit AC source on every independent source in the circuit.
    ///
    /// Example: hauksbee run board.kicad_pcb --ac 10:1e6:20 --ac-node OUT
    #[arg(long, value_name = "FSTART:FSTOP:POINTS", help_heading = "Advanced / analyses")]
    ac: Option<String>,

    /// Output net(s) to report for `--ac` (repeatable). Defaults to every net.
    #[arg(long = "ac-node", value_name = "NET", help_heading = "Advanced / analyses")]
    ac_node: Vec<String>,

    /// Write the full AC sweep (all reported nets) to this CSV file.
    #[arg(long, value_name = "FILE", help_heading = "Advanced / analyses")]
    ac_csv: Option<PathBuf>,

    /// Measure loop stability at this break/output net: report gain crossover
    /// and phase margin. Use with `--ac`. The net is the far side of a loop
    /// broken by an injection `Vsource` (see docs/AC_ANALYSIS.md).
    #[arg(long = "ac-loop", value_name = "NET", help_heading = "Advanced / analyses")]
    ac_loop: Option<String>,

    /// Record these nets' node voltages each chunk of a `--headless` run and write
    /// them to `--probe-csv`, so waveforms are scriptable with no UI. Comma-
    /// separated and/or repeatable: `--probe +5V,GATE --probe D13`. An unknown net
    /// is a loud error (with near-matches) before the run starts.
    #[arg(long, value_name = "NET[,NET...]", value_delimiter = ',', help_heading = "Advanced / analyses")]
    probe: Vec<String>,

    /// CSV path for `--probe`: header is `time_s` then one column per probed net,
    /// one row per co-sim chunk.
    #[arg(long, value_name = "FILE", help_heading = "Advanced / analyses")]
    probe_csv: Option<PathBuf>,
}

#[derive(Parser)]
struct ToCodeArgs {
    /// Board file to decompile.
    #[arg(value_name = "BOARD")]
    board: PathBuf,

    /// Write the Board-as-Code to this file (default: print to stdout).
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,
}

#[derive(Parser)]
struct FromCodeArgs {
    /// Board-as-Code file (or directory) to recompile.
    #[arg(value_name = "CODE")]
    code: PathBuf,

    /// Write the `.kicad_pcb` here (default: print to stdout).
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,

    /// Re-place every part from scratch before emitting.
    #[arg(long, conflicts_with = "incremental")]
    relayout: bool,

    /// Re-place only parts that moved, keeping the rest pinned.
    #[arg(long)]
    incremental: bool,

    /// Autoroute via freerouting (falls back to the grid A* if absent).
    #[arg(long, conflicts_with = "route_grid")]
    route: bool,

    /// Force the in-tree grid A* router instead of freerouting.
    #[arg(long)]
    route_grid: bool,
}

#[derive(Parser)]
struct ServeArgs {
    /// Port for the local web front door.
    #[arg(long, default_value_t = 3001, value_name = "PORT")]
    port: u16,
}

/// Output file format for `hauksbee sim`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum SimFormat {
    /// One column per probe, one row per timepoint (or one row for `.op`).
    Csv,
    /// ngspice ASCII rawfile — not yet implemented (plan step 14).
    Raw,
    /// CSV and rawfile side by side — not yet implemented (plan step 14).
    Both,
}

#[derive(Parser)]
// At most one analysis may be forced; with none, the deck's directives decide.
#[command(group(
    clap::ArgGroup::new("analysis")
        .args(["op", "tran", "ac", "dc"])
        .multiple(false)
))]
struct SimArgs {
    /// SPICE deck to simulate (`.cir`).
    #[arg(value_name = "DECK")]
    file: PathBuf,

    /// Write the CSV here (default: print to stdout).
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,

    /// Output format. `csv` (default) is solid; `raw`/`both` refuse loudly until
    /// the rawfile writer lands (plan step 14).
    #[arg(long, value_enum, default_value_t = SimFormat::Csv)]
    format: SimFormat,

    /// Force the DC operating point analysis.
    #[arg(long, group = "analysis")]
    op: bool,

    /// Force a transient run (needs a `.tran` card for the window).
    #[arg(long, group = "analysis")]
    tran: bool,

    /// AC sweep — recognized but NOT yet wired in `hauksbee sim` (the front-end
    /// does not parse `.ac`/AC source magnitudes yet, plan step 9). Refuses.
    #[arg(long, group = "analysis")]
    ac: bool,

    /// DC sweep — recognized but NOT yet wired (plan step 9). Refuses.
    #[arg(long, group = "analysis")]
    dc: bool,

    /// Probe expressions to output: `V(a)`, `V(a,b)`, `I(V1)`. Overrides the
    /// deck's `.print`. Space-separated. Default: every node voltage.
    #[arg(long, value_name = "PROBE", num_args = 1..)]
    print: Vec<String>,
}

#[derive(Parser)]
struct DoctorArgs {
    /// Report co-sim backend availability (the default and, for now, only
    /// check). Accepted explicitly so the documented `hauksbee doctor
    /// --backends` invocation is stable if future checks are added.
    #[arg(long)]
    backends: bool,

    /// Emit machine-readable JSON (`{"backends":[{name,status,available,...}]}`)
    /// instead of the tab-separated text table.
    #[arg(long)]
    json: bool,
}

#[derive(Parser)]
struct CheckCodeArgs {
    /// Board-as-Code file (or directory) to check.
    #[arg(value_name = "CODE")]
    code: PathBuf,

    /// Seconds of simulated time to run the stress monitor for.
    #[arg(long, default_value_t = 0.2, value_name = "N")]
    seconds: f64,

    /// Run the stress monitor in destructive mode (parts can be destroyed).
    #[arg(long)]
    destructive: bool,

    /// Ambient temperature (C) for the steady-state junction-temperature
    /// estimate (Tj = Tambient + P * theta_JA). Default 25 C.
    #[arg(long, default_value_t = 25.0, value_name = "C", help_heading = "Advanced / analyses")]
    ambient: f64,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // Fix #3 (LOW): under `--json`, an AI/CI consumer expects parseable output on
    // EVERY path, including a hard error. Emit `{"ok": false, "error": "..."}`
    // instead of the plaintext `error:` line so the failure is still valid JSON.
    // Only `run --json` produces JSON at all, so this is the only place it applies.
    let json = matches!(&cli.command, Command::Run(args) if args.json);
    let quiet = cli.quiet;
    let result = match cli.command {
        Command::Run(args) => cmd_run(args, quiet),
        Command::ToCode(args) => cmd_to_code(args),
        Command::FromCode(args) => cmd_from_code(args),
        Command::CheckCode(args) => cmd_check_code(args),
        Command::Serve(args) => cmd_serve(args),
        Command::Doctor(args) => cmd_doctor(args),
        Command::Sim(args) => cmd_sim(args),
        Command::Models(args) => match args.command {
            ModelsCommand::Lint(args) => cmd_models_lint(args),
        },
    };
    if let Err(e) = &result {
        if json {
            // serde_json escapes the message so the object is always well-formed.
            let obj = serde_json::json!({ "ok": false, "error": e.to_string() });
            println!("{obj}");
        } else {
            eprintln!("error: {e}");
        }
        std::process::exit(1);
    }
    result
}

/// `hauksbee models lint <file>`: standalone validation of model TOML.
///
/// Dispatches on the file's root shape: a `[sensor]` table lints as a
/// register-map sensor spec (`SensorSpec::from_toml`, the validation the
/// engine interpreter applies); anything with `[[models]]` entries lints each
/// entry's kind-specific params (`hauksbee_models::validate`) and, when a
/// `[models.logic]` block is present, COMPILES it through the same
/// `LogicComponent::compile` path binding uses — schema validation, expression
/// lowering, and the exhaustive comb-cycle convergence check — so "lint said
/// ok" and "the board binds it" can never disagree.
fn cmd_models_lint(args: ModelsLintArgs) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(&args.file)
        .map_err(|e| anyhow::anyhow!("reading '{}': {e}", args.file.display()))?;
    let root: toml::Value = toml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("'{}' is not TOML: {e}", args.file.display()))?;

    let mut findings = 0usize;
    let mut checked = 0usize;

    if root.get("sensor").is_some() {
        checked += 1;
        match hauksbee_models::SensorSpec::from_toml(&text) {
            Ok(spec) => println!("sensor '{}': ok", spec.sensor().name),
            Err(e) => {
                findings += 1;
                println!("sensor spec: ERROR: {e}");
            }
        }
    } else if root.get("models").is_some() {
        let db: hauksbee_models::schema::DbFile = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("'{}': [[models]] parse error: {e}", args.file.display()))?;
        for entry in &db.models {
            checked += 1;
            let mut entry_findings = 0usize;
            if let Err(errors) = hauksbee_models::validation::validate(entry) {
                for err in errors {
                    entry_findings += 1;
                    println!("model '{}': ERROR: {err}", entry.id);
                }
            }
            if !entry.logic.is_empty() {
                match hauksbee_engine::logic::LogicComponent::compile(&entry.id, &entry.logic) {
                    Ok(compiled) => {
                        for w in &compiled.warnings {
                            println!("model '{}' [models.logic]: warning: {w}", entry.id);
                        }
                    }
                    Err(e) => {
                        entry_findings += 1;
                        println!("model '{}' [models.logic]: ERROR: {e}", entry.id);
                    }
                }
            }
            if entry_findings == 0 {
                println!("model '{}': ok", entry.id);
            }
            findings += entry_findings;
        }
    } else {
        anyhow::bail!(
            "'{}' has neither a [sensor] table nor [[models]] entries — nothing to lint",
            args.file.display()
        );
    }

    println!(
        "{checked} item(s) checked, {findings} finding(s){}",
        if findings == 0 { " — clean" } else { "" }
    );
    if findings > 0 {
        std::process::exit(2);
    }
    Ok(())
}

/// Read a board file, turning a missing file into an actionable message.
fn read_board_text(path: &Path) -> anyhow::Result<String> {
    std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "no board file at '{}'. Check the path, or try a bundled example:\n  \
                 hauksbee run crates/hauksbee-ci/examples/boards/blinky.kicad_pcb --report",
                path.display()
            )
        } else {
            anyhow::anyhow!("reading '{}': {e}", path.display())
        }
    })
}

/// True when the text is a Board-as-Code (`.board`) DSL source, recognised by
/// the header `program_from_extracted`/`to_code` emit. Lets a `.board` saved
/// without that extension still route through the recompile path.
fn is_board_code_header(text: &str) -> bool {
    let head: String = text.chars().take(256).collect();
    head.contains("Board-as-Code") || head.contains("board version ")
}

fn cmd_run(args: RunArgs, quiet: bool) -> anyhow::Result<()> {
    // Validate `--firmware` up front, before any heavy work or the TUI takes over
    // the terminal. The native emulator loaders segfault (exit 139) on a missing
    // file instead of erroring; this turns a one-character typo into a clean,
    // actionable message naming the absolute path that was tried.
    if let Some(fw) = &args.firmware {
        hauksbee_mcu::validate_firmware_path(fw)?;
    }
    // Read raw bytes first: an Altium `.PcbDoc` is a binary OLE2 container and
    // would fail a UTF-8 read. Text formats (KiCad / Eagle / IPC) are recovered
    // losslessly from these bytes. Keep the actionable not-found error.
    let raw = std::fs::read(&args.board).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "no board file at '{}'. Check the path, or try a bundled example:\n  \
                 hauksbee run crates/hauksbee-ci/examples/boards/blinky.kicad_pcb --report",
                args.board.display()
            )
        } else {
            anyhow::anyhow!("reading '{}': {e}", args.board.display())
        }
    })?;
    // Advisory: if this board sits among sibling .kicad_pcb files (a multi-board
    // product), say so — a clean verdict on one file is misleading if the user
    // meant the whole thing. Routed through `Notes` so it stays on stderr and is
    // silenced under --quiet / --json / a piped stdout (it is helpful exactly once
    // for an interactive user, and pure noise in report and pipeline output).
    let notes = Notes::new(quiet, args.json);
    warn_sibling_boards(&args.board, notes);
    // Binary board (Altium): auto-detected from the OLE2 magic + Altium streams,
    // exactly as the Eagle path is auto-detected from XML content. No new CLI
    // surface.
    let altium = ExtractedBoard::from_auto_bytes(&raw).transpose()?;
    // Text view for the text formats and the geometry-bearing text checks.
    let mut text = if altium.is_some() {
        String::new()
    } else {
        String::from_utf8_lossy(&raw).into_owned()
    };
    // Board-as-Code (`.board`): detected by extension or the DSL header. Parse
    // the DSL, recompile it to `.kicad_pcb` text, then feed the same analysis
    // path the layout formats use. The recompiled KiCad text replaces `text` so
    // the geometry-bearing checks (DRC, SI) and the live viewer see the rebuilt
    // board, not the DSL source they cannot parse.
    let is_board_code = altium.is_none()
        && (args.board.extension().and_then(|e| e.to_str()) == Some("board")
            || is_board_code_header(&text));
    if is_board_code {
        text = code_to_board_text(&text)?;
    }
    // A `.kicad_sch` may reference sub-sheets that live in sibling files, so
    // it must be loaded by path to recurse the hierarchy; everything else is
    // self-contained and sniffed from its content.
    let board = if let Some(b) = altium.clone() {
        b
    } else if !is_board_code
        && args.board.extension().and_then(|e| e.to_str()) == Some("kicad_sch")
    {
        ExtractedBoard::from_kicad_schematic_path(&args.board)?
    } else {
        ExtractedBoard::from_auto(&text)?
    };
    // Layered model library: builtin < ~/.hauksbee/models (datasheet-extracted)
    // < ~/.config/hauksbee/models (user) < --models-dir (highest). A custom
    // behavioural part dropped in any of these loads with no recompile.
    let extra: Vec<&std::path::Path> = args.models_dir.as_deref().into_iter().collect();
    let lib = ModelLibrary::builtin_with_user_dirs(&extra);

    // --probe records live waveforms, which only exist during a co-sim; it is
    // meaningless for the static reports and the interactive server. Fail loudly
    // rather than silently ignore the flag.
    if !args.probe.is_empty() && !args.headless {
        anyhow::bail!("--probe records co-sim waveforms and needs --headless");
    }

    // --list-nets: print the board's net names so the user can pick one for
    // --ac-node / --ac-loop without grepping the layout. One net per line on
    // stdout (pipeable); a JSON array under --json.
    if args.list_nets {
        let bound = bind_board(&board, &lib);
        let mut nets: Vec<String> = bound.net_names.clone();
        nets.sort();
        if args.json {
            let body = nets
                .iter()
                .map(|n| format!("{n:?}"))
                .collect::<Vec<_>>()
                .join(",");
            println!("[{body}]");
        } else {
            eprintln!("{} net(s):", nets.len());
            for n in &nets {
                println!("{n}");
            }
        }
        return Ok(());
    }

    // --check / --all: the whole static suite (bind + DRC + lint + SI) in ONE
    // report, so a person (or an AI) gets everything in a single command instead
    // of running one flag at a time. Honours --plain / --json / --strict.
    if args.check {
        return hauksbee_engine::reports::check::emit(&args.board, &board, &text, &raw, altium.is_some(), &lib, hauksbee_engine::reports::OutputMode::from_flags(args.json, args.plain), args.strict);
    }

    if args.report {
        return hauksbee_engine::reports::bind::emit(&board, &lib, hauksbee_engine::reports::OutputMode::from_flags(args.json, args.plain));
    }

    // --drc: run geometric short / clearance detection, print, exit.
    if args.drc {
        return hauksbee_engine::reports::drc::emit(&args.board, &board, &text, &raw, altium.is_some(), &lib, hauksbee_engine::reports::OutputMode::from_flags(args.json, args.plain), args.oracle, args.strict);
    }

    // --ampacity: IPC-2221 capacity-only report. No current is fabricated here:
    // without a per-net current spec this tells the user the bottleneck capacity
    // and explicitly asks for a current before pass/fail.
    if args.ampacity {
        return hauksbee_engine::reports::ampacity::emit(&text, altium.is_some());
    }

    // --lint: run the connectivity lint-class checks, the boot strap-pin lint
    // (which needs the model db's per-part strap tables), and the MCU internal
    // resource-conflict check (a lint-class structural check too), print, exit.
    if args.lint {
        return hauksbee_engine::reports::lint::emit(&board, &lib, hauksbee_engine::reports::OutputMode::from_flags(args.json, args.plain), args.strict);
    }

    // --resources: run only the MCU internal resource-conflict check, print, exit.
    if args.resources {
        return hauksbee_engine::reports::lint::emit_resources(&board, &lib, hauksbee_engine::reports::OutputMode::from_flags(args.json, args.plain), args.strict);
    }

    // --usb-c: run the USB-C CC attach classifier (the RPi 4 re-derivation) and
    // print the compliance report. The capability existed but was unreachable from
    // any user-facing surface; this is its CLI front door.
    if args.usb_c {
        return hauksbee_engine::reports::usb_c::emit(&board, hauksbee_engine::reports::OutputMode::from_flags(args.json, args.plain), args.strict);
    }

    // --si: run the signal-integrity / physics static checks, print, exit. The
    // geometry-bearing checks (antenna keepout, USB length skew) need the raw
    // KiCad layout text, so it is passed through.
    if args.si {
        return hauksbee_engine::reports::si::emit(&board, &text, altium.is_some(), &lib, hauksbee_engine::reports::OutputMode::from_flags(args.json, args.plain), args.strict);
    }

    // --ac: small-signal AC sweep on the bound circuit, print Bode + (optional)
    // loop-stability margins, then exit. Informational like the other reports.
    if let Some(ac_arg) = &args.ac {
        let bound = bind_board(&board, &lib);
        return hauksbee_engine::reports::ac::emit(
            &bound,
            ac_arg,
            &args.ac_node,
            args.ac_csv.as_deref(),
            args.ac_loop.as_deref(),
            args.json,
        );
    }

    // Bare `--json` with no specific report selector: emit a COMBINED machine
    // report (bind + DRC + lint/straps/resources + SI) and exit. Without this,
    // `--json` alone falls through to the TUI/websocket default below and hangs a
    // piped / CI / AI caller (the regression a bare `run <board> --json` hit).
    // `--json` is an explicit machine-intent flag, so it must never launch the TUI.
    // `--thermal`/`--headless` are selectors handled further down with their OWN
    // JSON emitters (thermal coverage, co-sim notes); they must fall THROUGH this
    // combined branch or those JSON paths become unreachable dead code.
    if args.json && !args.thermal && !args.headless {
        return hauksbee_engine::reports::check::emit_combined_json(&args.board, &board, &text, &raw, altium.is_some(), &lib);
    }

    // Default flow (no report/headless/ac flag). The interactive terminal UI is
    // the new human-facing default: bare `run <board>` on a TTY launches it. Any
    // explicit report flag was handled above, so reaching here means none was
    // given. `--serve` keeps the historical websocket frontend; a non-TTY stdout
    // (piped / CI) also keeps the websocket behaviour untouched, so existing
    // scripts and tests are unaffected.
    //
    // `--firmware`/`--apply-shorts` only matter for the simulating paths; the TUI
    // honours `--firmware` for its co-sim pane. We branch to the TUI before
    // building the websocket engine so we never spin up tokio for the TUI path.
    let stdout_is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    // Altium boards reach here with an empty `text` (binary parsed from bytes);
    // the TUI's text-based build path can't analyse those, so they keep the
    // websocket flow.
    let launch_tui =
        !args.serve && !args.headless && altium.is_none() && (args.tui || stdout_is_tty);
    if launch_tui {
        return hauksbee_engine::tui::run(
            &args.board,
            &text,
            args.models_dir.as_deref(),
            args.firmware.clone(),
        );
    }

    // Bind with the layered library (so a --models-dir / user-dir custom part is
    // in scope), then build the engine from the bound board.
    let bound = bind_board(&board, &lib);
    // Net names captured before `bound` is consumed, for --probe validation.
    let probe_known_nets: Vec<String> = if args.probe.is_empty() {
        Vec::new()
    } else {
        bound.net_names.clone()
    };
    let mut engine = HauksbeeEngine::from_bound(
        bound,
        args.firmware.as_deref(),
        &format!("/boards/{}", file_name(&args.board)),
    )?;

    // --apply-shorts: bridge every detected copper short before simulating.
    if args.apply_shorts {
        let report = if altium.is_some() {
            ExtractedBoard::altium_drc(&raw)?
        } else {
            ExtractedBoard::drc_with_clearance_rules(
                &text,
                hauksbee_engine::reports::kicad_pro_clearance_rules(&args.board, &board),
            )?
        };
        let applied = engine.apply_drc_shorts(&report);
        eprintln!(
            "applied {applied} copper short(s) of {} detected ({} clearance violations)",
            report.short_count(),
            report.clearance_violations().count(),
        );
    }

    // --thermal: run a short co-sim, then print the steady-state junction
    // temperature per dissipating device and exit. Fix #1: a thermal table that
    // covers ~no dissipating devices because the power ICs are UNRESOLVED is a
    // meaningless result, not a "runs cool" pass — flag it invalid and exit 3.
    if args.thermal {
        return hauksbee_engine::reports::thermal::emit(&mut engine, args.ambient, args.seconds, args.json, args.strict_thermal);
    }

    if args.headless {
        // --probe preconditions, checked before the run so a typo fails fast with
        // the same near-match style the rest of the net-facing CLI uses.
        let probes = dedup_probes(&args.probe);
        validate_probes(&probes, args.probe_csv.as_deref(), &probe_known_nets)?;

        let board_name = engine.report().board_name.clone();
        let summary = BindSummary::from_report(engine.report());
        let mut uart_seen = false;
        let faults = run_headless(
            &mut engine,
            args.seconds,
            &mut uart_seen,
            args.json,
            args.strict,
            &probes,
            args.probe_csv.as_deref(),
        )?;

        // Co-sim honesty summary (Track B): total net toggles, UART activity, and
        // any chip substitution detected at build time. Built from the SAME run
        // stats the text table reads, so every surface agrees.
        let cosim = build_cosim_json(&engine, uart_seen);
        let total_toggles = cosim.as_ref().map(|c| c.total_toggles).unwrap_or(0);
        // Analog-fidelity honesty (05 §3b): once any chunk's analog solve failed,
        // the run held stale voltages and cannot vouch for analog-derived findings
        // over the failed windows. `analog_abort` is the stricter condition: the
        // solve was stuck for a whole streak of chunks, so a strict run must
        // refuse (exit 3) rather than complete a fake-quiet run.
        let analog_valid = engine.scheduler().analog_valid();
        let failed_chunk_count = engine.scheduler().failed_chunk_count();
        let analog_abort = engine.scheduler().analog_abort_tripped();
        // A co-sim that drove no GPIO, produced no net toggles, AND emitted no
        // UART did not exercise the firmware. `any_gpio_driven()` is essential:
        // a firmware that drives a control line high and HOLDS it (boot-gate style)
        // has zero net toggles yet clearly ran, so a toggles-only test would cry
        // wolf on it. Determined BEFORE emitting so the refusal reaches every
        // surface — including --json when no MCU was instantiated (cosim is None).
        let zero_activity =
            total_toggles == 0 && !uart_seen && !engine.scheduler().any_gpio_driven();

        // Boot-safety advisory — derived once (so --json and --plain agree) from
        // the library so the TUI and web get the same advisory from the same call.
        // `held_high_control_nets` are the heads-up hazards (a control net driven/
        // pulled HIGH and held from reset that switches a transistor/relay and has
        // no bias resistor — a MOSFET gate / relay / igniter energised at power-up;
        // the switch requirement is the zero-FP guard). `gate_states` is the
        // informational per-gate panel, populated only when firmware actually ran
        // (with no --firmware or a stalled one, every gate would read "floating").
        let boot_advisory = hauksbee_engine::checks::boot::analyze(
            &board,
            &engine.scheduler().firmware_held_high_nets(),
            &engine.scheduler().firmware_output_configured_nets(),
            &engine.scheduler().firmware_driven_nets(),
            args.firmware.is_some() && !zero_activity,
        );
        let held_high_boot_nets = &boot_advisory.held_high_control_nets;
        let has_boot_advisory = !held_high_boot_nets.is_empty();
        let gate_rows = &boot_advisory.gate_states;

        if args.json {
            let mut jr = JsonReport::new(&board_name, summary);
            // A substitution is an info-level note that must never be silently
            // absent (it changes how much the co-sim result can be trusted).
            for sub in engine.scheduler().substitutions() {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::CosimSubstitution,
                    message: sub.message(),
                });
            }
            if zero_activity {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::Coverage,
                    message: "co-sim saw zero net toggles and no UART output; the \
                              firmware was not exercised — this result cannot vouch \
                              for firmware behaviour"
                        .to_string(),
                });
            }
            // A non-convergent chunk held stale voltages: a loud coverage note so
            // a CI consumer that filters notes (not just the CosimJson body) sees
            // the analog side is not trustworthy over the failed windows (05 §3b).
            if !analog_valid {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::Coverage,
                    message: format!(
                        "co-sim analog solve failed to converge on {failed_chunk_count} \
                         chunk(s); those windows held stale node voltages and are \
                         reported as analog_valid:false; analog-derived findings over \
                         them are not trustworthy"
                    ),
                });
            }
            for net in held_high_boot_nets {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::BootControlNet,
                    message: format!(
                        "control net '{net}' drives a transistor/relay, is driven HIGH and held \
                         from power-up, and has no resistor setting a safe default. If a HIGH on \
                         it turns the switched load ON when it must stay OFF until firmware \
                         enables it, it is energised at power-up — confirm the polarity and that \
                         this is intended."
                    ),
                });
            }
            if !gate_rows.is_empty() {
                jr.boot_gates = Some(
                    gate_rows
                        .iter()
                        .map(|(reference, net, state)| BootGateJson {
                            reference: reference.clone(),
                            net: net.clone(),
                            state: state.json().to_string(),
                        })
                        .collect(),
                );
            }
            jr.cosim = cosim;
            println!("{}", jr.to_json());
        } else if args.plain {
            // A co-sim with no stress faults is NOT plainly "healthy" if it ran on
            // a substitute chip or never exercised the firmware. Surface those as
            // heads-up notes so the verdict reads "no failures, but N worth a look"
            // (via PlainReport::verdict) instead of a bare "Looks healthy".
            let mut report = hauksbee_engine::plain_faults(&faults);
            for sub in engine.scheduler().substitutions() {
                report
                    .heads_up
                    .push(format!("co-sim ran on a SUBSTITUTE chip — {}", sub.message()));
            }
            if zero_activity {
                report.heads_up.push(
                    "co-sim saw zero net toggles and no UART output — the firmware was not \
                     exercised, so this result cannot vouch for firmware behaviour"
                        .to_string(),
                );
            }
            if !analog_valid {
                report.heads_up.push(format!(
                    "co-sim analog solve failed to converge on {failed_chunk_count} chunk(s); \
                     those windows held stale voltages and cannot be trusted (analog_valid is \
                     false); rerun with --json to see the exact failed windows"
                ));
            }
            // Boot-safety heads-up: control nets the firmware switches ON and
            // holds from power-up, with no resistor setting a safe default. The
            // netlist alone cannot tell whether a power-up HIGH is intended;
            // running the firmware can. This is what surfaces, e.g., a MOSFET /
            // relay / igniter that energises at reset because firmware drove its
            // gate high (or enabled a pull-up on it) before anything else ran.
            for net in held_high_boot_nets {
                report.heads_up.push(format!(
                    "control net '{net}' switches a transistor/relay and is driven HIGH and held \
                     from the moment the board powers up, with no resistor setting a safe default \
                     level. If a HIGH on this net turns the load ON when it must stay OFF until \
                     the firmware deliberately enables it (a MOSFET, relay, motor driver, or \
                     igniter), it is energised at power-up — confirm the polarity and that this \
                     is intended."
                ));
            }
            println!();
            print!("{}", report.render());
            if !gate_rows.is_empty() {
                print!("{}", hauksbee_engine::checks::boot::render_boot_gate_panel(gate_rows));
            }
        }

        // 0-activity refusal (Track B): warn always; under --strict this is a hard
        // refusal (exit 3), not a clean pass. The UART-AND-toggles guard avoids
        // false positives on firmware that is busy on the bus but quiet on GPIO.
        if zero_activity {
            eprintln!(
                "WARNING: co-sim saw zero net toggles; cannot vouch for firmware \
                 behaviour (the MCU may have stalled at boot, run no I/O, or the \
                 firmware may not match this board)."
            );
            if args.strict {
                std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
            }
        }

        // Refuse-rather-than-fake (05 §3b): once the analog solve was stuck for a
        // whole streak of chunks, a strict run must abort with the invalid code
        // rather than complete a fake-quiet run. Warn always so the reason is never
        // silent; only --strict turns it into a failing exit.
        if analog_abort {
            eprintln!(
                "WARNING: co-sim analog solve failed to converge for {} chunks in a row \
                 ({} failed chunks total); the run held stale voltages and cannot vouch \
                 for the analog side.",
                hauksbee_engine::scheduler::STRICT_CONSECUTIVE_FAILED_ABORT,
                failed_chunk_count,
            );
            if let Some(code) = strict_analog_exit_code(args.strict && analog_abort) {
                std::process::exit(code);
            }
        }

        // Strict: any fault raised during the run fails the gate.
        if args.strict && !faults.is_empty() {
            std::process::exit(2);
        }
        // --strict-boot: opt-in escalation of the boot-safety advisory to a
        // failing gate (exit 2). The run was valid and these are real findings
        // about specific nets; default behaviour leaves them advisory-only. Print
        // the reason to stderr so the failure is never silent — including in the
        // default headless mode (neither --json nor --plain), where the advisory
        // text is not otherwise emitted.
        if args.strict_boot && has_boot_advisory {
            for net in held_high_boot_nets {
                eprintln!(
                    "BOOT HAZARD (--strict-boot): control net '{net}' switches a transistor/relay \
                     and is driven HIGH and held from power-up with no bias resistor — the load is \
                     energised at reset."
                );
            }
            std::process::exit(2);
        }
        return Ok(());
    }

    // Non-TTY invocation with no report flag and no explicit --serve: rather than
    // silently starting a websocket server a pipe / CI can't use (the "something
    // different" §7 warns about), print a two-line hint pointing at the report
    // surfaces and exit cleanly. A TTY would have launched the TUI above; an
    // explicit --serve keeps the historical websocket behaviour untouched.
    if !stdout_is_tty && !args.serve {
        eprintln!(
            "hauksbee run: stdout is not a terminal, so there is no interactive dashboard to show."
        );
        eprintln!(
            "  For a report add a flag: --check (all static checks) · --plain (prose) · --json (machine); or --serve for the browser UI."
        );
        return Ok(());
    }

    // Serve the loaded board's own file at the URL the frontend fetches it from
    // (`/boards/<name>`), so the 2D/3D viewer renders the real geometry for any
    // board, not just the demo boards baked into dist/.
    let board_url = format!("/boards/{}", file_name(&args.board));
    serve(engine, args.port, Some((board_url, text)))
}

/// `hauksbee run <board> --ac <fstart>:<fstop>:<points> [--ac-node NET ...]
/// [--ac-csv FILE] [--ac-loop NET]`
///
/// Runs the small-signal AC analysis on the bound circuit and prints a Bode
/// table (magnitude in dB, phase in degrees) for the requested net(s). With
/// `--ac-loop`, also reports gain crossover and phase margin for that net.
/// Exit code for a malformed deck (the loader rejected it). Distinct from the
/// exit-3 "cannot honestly answer" (a well-formed deck we refuse to fake).
const EXIT_MALFORMED_DECK: i32 = 2;

/// `hauksbee sim`: load a `.cir`, run `.op` or `.tran`, write CSV.
fn cmd_sim(args: SimArgs) -> anyhow::Result<()> {
    use hauksbee_ir::SpiceLoader;
    use hauksbee_solve::{
        default_probes, run_op, run_tran, Integration, Probe, SimOutput, SolverOptions, StepControl,
        DcInit,
    };

    // Read the deck. A missing file is an ordinary CLI error (exit 1) with an
    // actionable message, not a deck-malformed refusal.
    let text = std::fs::read_to_string(&args.file).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!("no deck at '{}'. Check the path.", args.file.display())
        } else {
            anyhow::anyhow!("reading '{}': {e}", args.file.display())
        }
    })?;

    // Parse. A SpiceError already carries its line number; print it verbatim and
    // exit 2 (malformed deck) — never fall through to a wrong parse.
    let (circuit, directives) = match SpiceLoader::load_with_directives(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(EXIT_MALFORMED_DECK);
        }
    };

    // Output format: only CSV is implemented. Rawfile refuses loudly (plan
    // step 14) rather than emitting nothing or the wrong thing.
    if matches!(args.format, SimFormat::Raw | SimFormat::Both) {
        eprintln!(
            "error: --format {} is not yet implemented (ngspice rawfile output is \
             SPICE-compat plan step 14). Use --format csv (the default).",
            match args.format {
                SimFormat::Raw => "raw",
                SimFormat::Both => "both",
                SimFormat::Csv => unreachable!(),
            }
        );
        std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
    }

    // Choose the analysis: an explicit flag wins; otherwise a `.tran` card means
    // transient and anything else means the operating point.
    enum Analysis {
        Op,
        Tran,
        Ac,
        Dc,
    }
    let analysis = if args.op {
        Analysis::Op
    } else if args.tran {
        Analysis::Tran
    } else if args.ac {
        Analysis::Ac
    } else if args.dc {
        Analysis::Dc
    } else if directives.tran.is_some() {
        Analysis::Tran
    } else {
        Analysis::Op
    };

    // Refuse the unwired analyses loudly (exit 3): the netlist front-end does
    // not parse `.ac`/`.dc` directives or AC source magnitudes yet, so there is
    // nothing honest to compute. A loud refusal, never a silent no-op.
    match analysis {
        Analysis::Ac => {
            eprintln!(
                "error: --ac is recognized but not yet wired in `hauksbee sim`. The netlist \
                 front-end does not yet parse `.ac` directives or AC source magnitudes \
                 (SPICE-compat plan step 9). Refusing rather than emitting an empty or wrong \
                 result. Use --op or --tran."
            );
            std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
        }
        Analysis::Dc => {
            eprintln!(
                "error: --dc (DC sweep) is recognized but not yet wired in `hauksbee sim`. The \
                 sweep driver is SPICE-compat plan step 9. Refusing rather than faking a result. \
                 Use --op for a single operating point or --tran."
            );
            std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
        }
        _ => {}
    }

    // Probes: an explicit --print wins; otherwise every node voltage (and we say
    // so, on stderr, so the choice is never a silent surprise).
    let probes: Vec<Probe> = if args.print.is_empty() {
        eprintln!(
            "note: no --print given and the loader does not yet parse `.print`; \
             writing every node voltage."
        );
        default_probes(&circuit)
    } else {
        let mut ps = Vec::with_capacity(args.print.len());
        for tok in &args.print {
            match Probe::parse(tok) {
                Ok(p) => ps.push(p),
                Err(msg) => {
                    eprintln!("error: --print: {msg}");
                    std::process::exit(EXIT_MALFORMED_DECK);
                }
            }
        }
        ps
    };

    // Build solver options from the deck's tolerances.
    let mut opts = SolverOptions::default();
    if let Some(r) = directives.reltol {
        opts.reltol = r;
    }
    if let Some(a) = directives.abstol {
        opts.abstol = a;
    }
    if let Some(v) = directives.vntol {
        opts.vntol = v;
    }

    let output: SimOutput = match analysis {
        Analysis::Op => match run_op(&circuit, &opts, &probes) {
            Ok(o) => o,
            Err(msg) => {
                eprintln!(
                    "error: DC operating point did not converge (or a probe was invalid): {msg}"
                );
                std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
            }
        },
        Analysis::Tran => {
            let Some(td) = directives.tran else {
                eprintln!(
                    "error: --tran requested but the deck has no `.tran` card, so there is no \
                     stop time or step to run. Add `.tran <tstep> <tstop>` or use --op."
                );
                std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
            };
            // Adaptive step bounded by the deck's requested step (its tmax if
            // given, else tstep), the same shape the existing cross-check uses.
            let dt_max = td.tmax.unwrap_or(td.tstep).max(1e-15);
            opts.integration = Integration::Trapezoidal;
            opts.step = StepControl::Adaptive {
                dt_initial: (td.tstep / 100.0).max(1e-15),
                dt_min: 1e-15,
                dt_max,
            };
            // `uic` means power-on start: skip the DC solve, march from rest.
            if directives.use_initial_conditions {
                opts.dc_init = DcInit::FromZero;
            }
            match run_tran(&circuit, &opts, td.tstop, &probes) {
                Ok(o) => o,
                Err(msg) => {
                    eprintln!("error: transient solve failed: {msg}");
                    std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
                }
            }
        }
        Analysis::Ac | Analysis::Dc => unreachable!("refused above"),
    };

    // Serialize to CSV and write to --out or stdout.
    let csv = sim_output_to_csv(&output);
    match &args.out {
        Some(path) => {
            std::fs::write(path, csv)
                .map_err(|e| anyhow::anyhow!("writing '{}': {e}", path.display()))?;
            eprintln!(
                "wrote {} row(s) x {} column(s) to {}",
                output.rows.len(),
                output.columns.len(),
                path.display()
            );
        }
        None => print!("{csv}"),
    }
    Ok(())
}

/// Render a [`hauksbee_solve::SimOutput`] as CSV. A transient prepends a
/// `time_s` column; an operating point is a bare header + one row.
fn sim_output_to_csv(o: &hauksbee_solve::SimOutput) -> String {
    let mut s = String::new();
    let mut header = Vec::new();
    if o.time.is_some() {
        header.push("time_s".to_string());
    }
    header.extend(o.columns.iter().cloned());
    s.push_str(&header.join(","));
    s.push('\n');
    for (i, row) in o.rows.iter().enumerate() {
        let mut cells = Vec::with_capacity(row.len() + 1);
        if let Some(t) = &o.time {
            cells.push(format!("{:.10e}", t[i]));
        }
        for v in row {
            cells.push(format!("{v:.10e}"));
        }
        s.push_str(&cells.join(","));
        s.push('\n');
    }
    s
}

/// `hauksbee to-code <board-file> [--out <file.board>]`
///
/// Accepts any text board the extractor understands. A `.kicad_pcb` keeps the
/// cluster-aware geometry decompiler; a KiCad `.net` / IPC-D-356 / Eagle `.brd`
/// / `.kicad_sch` is extracted and emitted flat (so a layout-free netlist also
/// becomes editable Board-as-Code).
fn cmd_to_code(args: ToCodeArgs) -> anyhow::Result<()> {
    let text = read_board_text(&args.board)?;
    let code = decompile_any_to_code(&text)?;
    match args.out {
        Some(p) => {
            std::fs::write(&p, &code)?;
            eprintln!("wrote {}", p.display());
        }
        None => print!("{code}"),
    }
    Ok(())
}

/// How to route the recompiled board.
#[derive(Clone, Copy, PartialEq)]
enum RouteMode {
    /// No routing (placement only) - the historical default.
    None,
    /// Hand off to freerouting; fall back to the grid A* if it is absent.
    Freerouting,
    /// Force the in-tree grid A* fallback.
    Grid,
}

/// `hauksbee from-code <code-file> [--out <file.kicad_pcb>] [--relayout|--incremental] [--route|--route-grid]`
fn cmd_from_code(args: FromCodeArgs) -> anyhow::Result<()> {
    use forge_codegen::{relayout, LayoutConfig, Program};

    let layout: Option<LayoutConfig> = if args.relayout {
        Some(LayoutConfig::full())
    } else if args.incremental {
        Some(LayoutConfig::incremental())
    } else {
        None
    };
    let route = if args.route {
        RouteMode::Freerouting
    } else if args.route_grid {
        RouteMode::Grid
    } else {
        RouteMode::None
    };

    let code = load_code(&args.code)?;

    // Parse + (optionally) re-place, then build a Pcb we can route on.
    let base = Program::parse(&code).map_err(|e| anyhow::anyhow!("board code: {e}"))?;
    let mut prog = base.clone();
    if let Some(cfg) = layout {
        let report = relayout(&mut prog, &base, &cfg);
        eprintln!(
            "re-layout: {} groups, {} moved, {} kept",
            report.groups,
            report.moved.len(),
            report.kept
        );
    }
    let mut pcb = prog.build();

    if route != RouteMode::None {
        route_board(&mut pcb, &prog, route, args.out.as_deref())?;
    }

    let board_text = pcb.emit();
    match args.out {
        Some(p) => {
            std::fs::write(&p, &board_text)?;
            eprintln!("wrote {}", p.display());
        }
        None => print!("{board_text}"),
    }
    Ok(())
}

/// Route a built board in place. Prefers freerouting; documents and falls back
/// to the grid A* when freerouting is unavailable (or when explicitly forced).
fn route_board(
    pcb: &mut forge_model::Pcb,
    prog: &forge_codegen::Program,
    mode: RouteMode,
    out: Option<&Path>,
) -> anyhow::Result<()> {
    use forge_codegen::{route_grid, FreeroutingConfig, RouteRules};

    let rules = RouteRules::default();
    let fr_cfg = FreeroutingConfig::default();

    let use_grid = match mode {
        RouteMode::Grid => true,
        RouteMode::Freerouting => !forge_codegen::freerouting_available(&fr_cfg),
        RouteMode::None => return Ok(()),
    };

    if !use_grid {
        // Freerouting handoff (the production path).
        let workdir = out
            .and_then(|p| p.parent())
            .map(|d| d.join("freerouting-work"))
            .unwrap_or_else(|| std::env::temp_dir().join("hauksbee-freerouting"));
        eprintln!("routing: freerouting handoff (DSN -> freerouting -> SES)...");
        match forge_codegen::route_with_freerouting(pcb, prog.outline, &rules, &fr_cfg, &workdir) {
            Ok(o) => {
                let pct = if o.nets_to_route > 0 {
                    o.nets_routed as f64 / o.nets_to_route as f64 * 100.0
                } else {
                    100.0
                };
                eprintln!(
                    "routed: {}/{} nets ({:.0}%), {} segments, {} vias, {:.1}s (freerouting)",
                    o.nets_routed, o.nets_to_route, pct, o.segments, o.vias, o.elapsed_secs
                );
                return Ok(());
            }
            Err(e) => {
                eprintln!("freerouting failed ({e}); falling back to grid A*");
            }
        }
    } else {
        eprintln!("routing: freerouting absent, using in-tree grid A* fallback");
    }

    // Grid A* fallback. Route on the program (it reads pad geometry from there)
    // and merge tracks onto the board.
    let res = route_grid(prog, 0.5);
    let mut net_id = std::collections::HashMap::new();
    for n in pcb.nets() {
        net_id.insert(n.name.clone(), n.id);
    }
    let mut seg = 0usize;
    for t in &res.tracks {
        let id = net_id.get(&t.net).copied();
        for pair in t.points.windows(2) {
            pcb.add_segment(pair[0], pair[1], 0.25, "F.Cu", id);
            seg += 1;
        }
    }
    eprintln!(
        "routed: {} tracks ({} segments), {} unrouted nets (grid A* fallback)",
        res.tracks.len(),
        seg,
        res.unrouted.len()
    );
    Ok(())
}

/// `hauksbee check-code <code-dir|file> [--seconds N] [--destructive]`
fn cmd_check_code(args: CheckCodeArgs) -> anyhow::Result<()> {
    let opts = CheckOptions {
        seconds: args.seconds,
        destructive: args.destructive,
        ambient_c: args.ambient,
    };
    let code = load_code(&args.code)?;
    let report = check_code(&code, &opts)?;
    print!("{}", render_check_report(&report));
    if !report.healthy() {
        std::process::exit(1);
    }
    Ok(())
}

/// `hauksbee serve [--port N]`: the local web front door (upload-and-report).
fn cmd_serve(args: ServeArgs) -> anyhow::Result<()> {
    use std::sync::Arc;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let addr = format!("127.0.0.1:{}", args.port);
        // Inject the engine's analysis as the server's analyzer callback, so the
        // server crate needs no dependency on the engine/extract crates. The
        // firmware-aware callback handles both the board-only path (firmware ==
        // None -> analyze_json) and the firmware co-sim path.
        let analyze: hauksbee_server::frontdoor::FirmwareAnalyzer = Arc::new(
            |name: &str, contents: &str, fw: Option<(&str, &[u8])>| match fw {
                Some((fw_name, fw_bytes)) => {
                    hauksbee_engine::analyze_with_firmware_json(name, contents, fw_name, fw_bytes)
                }
                None => hauksbee_engine::analyze_json(name, contents),
            },
        );
        hauksbee_server::frontdoor::serve_with_firmware(&addr, analyze).await
    })
}

/// `hauksbee doctor --backends`: report co-sim backend availability using the
/// engine's OWN discovery, so `scripts/doctor.sh` can never drift from what a
/// real co-sim would accept.
///
/// For each backend this calls the exact resolver the scheduler uses
/// (`hauksbee_mcu::qemu::find_qemu`, `hauksbee_mcu::renode::find_renode`) — no
/// re-implemented search logic. `find_qemu` runs the Espressif-fork check
/// (`is_esp_fork`), so a Homebrew mainline `qemu-system-xtensa` on PATH is
/// reported `absent` here just as the co-sim rejects it, and a fork under
/// `~/.galvani-qemu-esp` or a Renode under `~/renode-portable` is reported
/// present with its resolved path.
///
/// stdout: one line per backend, `NAME<TAB>STATUS<TAB>DETAIL`, STATUS a single
/// lowercase token (`ok` / `absent` / `builtin` / `disabled`); DETAIL is the
/// resolved path or a one-line install hint and may contain spaces (parsers
/// should read field 3 to end-of-line). The human header goes to stderr so the
/// data stream stays clean.
fn cmd_doctor(args: DoctorArgs) -> anyhow::Result<()> {
    // A probed backend. `status` is a single token by contract (see above).
    struct Backend {
        name: &'static str,
        status: &'static str,
        detail: String,
        summary: &'static str,
    }

    let mut backends: Vec<Backend> = Vec::new();

    // AVR: built into this binary via libsimavr (feature `avr`); there is no
    // external process to locate, so it is `builtin` when compiled in.
    #[cfg(feature = "avr")]
    backends.push(Backend {
        name: "avr",
        status: "builtin",
        detail: "simavr linked into this binary".to_string(),
        summary: "ATmega / ATtiny firmware co-sim",
    });
    #[cfg(not(feature = "avr"))]
    backends.push(Backend {
        name: "avr",
        status: "disabled",
        detail: "compiled out — rebuild with the default features + libsimavr \
                 (scripts/install-sims.sh --avr)"
            .to_string(),
        summary: "ATmega / ATtiny firmware co-sim",
    });

    // Espressif QEMU (Xtensa ESP32 / ESP32-S3, RISC-V ESP32-C3). `find_qemu`
    // verifies the binary is the Espressif fork before accepting it.
    #[cfg(feature = "qemu")]
    {
        use hauksbee_mcu::qemu::{find_qemu, QemuArch};
        let probes = [
            (
                "qemu-xtensa",
                QemuArch::Xtensa,
                "ESP32 / ESP32-S3 firmware co-sim (Espressif QEMU fork)",
            ),
            (
                "qemu-riscv32",
                QemuArch::Riscv32,
                "ESP32-C3 firmware co-sim (Espressif QEMU fork)",
            ),
        ];
        for (name, arch, summary) in probes {
            match find_qemu(arch) {
                Ok(p) => backends.push(Backend {
                    name,
                    status: "ok",
                    detail: p.display().to_string(),
                    summary,
                }),
                Err(e) => backends.push(Backend {
                    name,
                    status: "absent",
                    detail: one_line(&e.to_string()),
                    summary,
                }),
            }
        }
    }
    #[cfg(not(feature = "qemu"))]
    for (name, summary) in [
        ("qemu-xtensa", "ESP32 / ESP32-S3 firmware co-sim"),
        ("qemu-riscv32", "ESP32-C3 firmware co-sim"),
    ] {
        backends.push(Backend {
            name,
            status: "disabled",
            detail: "built without the `qemu` feature".to_string(),
            summary,
        });
    }

    // Renode (STM32 / nRF52 / SiFive RISC-V, i.e. ARM Cortex-M and RISC-V).
    #[cfg(feature = "renode")]
    match hauksbee_mcu::renode::find_renode() {
        Ok(p) => backends.push(Backend {
            name: "renode",
            status: "ok",
            detail: p.display().to_string(),
            summary: "STM32 / nRF52 / RISC-V firmware co-sim",
        }),
        Err(e) => backends.push(Backend {
            name: "renode",
            status: "absent",
            detail: one_line(&e.to_string()),
            summary: "STM32 / nRF52 / RISC-V firmware co-sim",
        }),
    }
    #[cfg(not(feature = "renode"))]
    backends.push(Backend {
        name: "renode",
        status: "disabled",
        detail: "built without the `renode` feature".to_string(),
        summary: "STM32 / nRF52 / RISC-V firmware co-sim",
    });

    if args.json {
        let arr: Vec<serde_json::Value> = backends
            .iter()
            .map(|b| {
                serde_json::json!({
                    "name": b.name,
                    "status": b.status,
                    "available": b.status == "ok" || b.status == "builtin",
                    "detail": b.detail,
                    "summary": b.summary,
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "backends": arr }));
        return Ok(());
    }

    // Human framing on stderr; the data table on stdout stays parseable.
    eprintln!("hauksbee co-sim backends (resolved by the engine's own discovery)");
    for b in &backends {
        eprintln!("    {:<13} {}", b.name, b.summary);
        println!("{}\t{}\t{}", b.name, b.status, b.detail);
    }
    Ok(())
}

/// Collapse a possibly multi-line message to its first line (discovery errors
/// are one line today, but this keeps the doctor table one-row-per-backend even
/// if a resolver's message grows).
fn one_line(msg: &str) -> String {
    msg.lines().next().unwrap_or("").to_string()
}






/// Build the machine-readable co-sim summary (Track B) from a finished run.
/// Returns `None` when no MCU core ran (no co-sim happened, so there is nothing
/// to summarise). Reads the scheduler's per-net stats for the total toggle count
/// and the top-N most-active nets, and the MCU binding identities for the
/// requested part / backend / substitution flag.
fn build_cosim_json(engine: &HauksbeeEngine, uart_seen: bool) -> Option<CosimJson> {
    let sched = engine.scheduler();
    let identities = sched.mcu_identities();
    // No live MCU => no co-sim ran (e.g. a renode/qemu board with no firmware).
    let (mcu_ref, backend, requested_part) = identities.into_iter().next()?;
    // A substitution is recorded against this reference iff its requested part
    // was collapsed onto a less-specific modelled core.
    let substituted = sched
        .substitutions()
        .iter()
        .any(|s| s.reference == mcu_ref);

    let total_toggles: u64 = sched.stats.values().map(|s| s.toggles).sum();

    // Top-N nets by activity (toggles, then voltage range), mirroring the text
    // table's ordering so JSON and text agree on "most active".
    let mut rows: Vec<_> = sched.stats.iter().collect();
    rows.sort_by(|a, b| {
        b.1.toggles.cmp(&a.1.toggles).then(
            (b.1.max_v - b.1.min_v)
                .partial_cmp(&(a.1.max_v - a.1.min_v))
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });
    let activity_summary: Vec<NetActivity> = rows
        .iter()
        .take(10)
        .filter(|(_, st)| st.toggles > 0)
        .map(|(name, st)| NetActivity {
            net: (*name).clone(),
            toggles: st.toggles,
            v_min: if st.min_v.is_finite() { st.min_v } else { 0.0 },
            v_max: if st.max_v.is_finite() { st.max_v } else { 0.0 },
        })
        .collect();

    // Analog-fidelity honesty (05 §3b): a run that held stale voltages over one
    // or more non-convergent chunks is not a faithful analog result. Surface the
    // exact windows so a consumer sees which span cannot be trusted rather than
    // reading the quiet-held voltages as real.
    let analog_valid = sched.analog_valid();
    let failed_windows: Vec<CosimFailedWindow> = sched
        .failed_windows()
        .iter()
        .map(|&(start_s, end_s)| CosimFailedWindow { start_s, end_s })
        .collect();

    Some(CosimJson {
        mcu_ref,
        backend,
        requested_part,
        substituted,
        total_toggles,
        uart_seen,
        activity_summary,
        analog_valid,
        failed_windows,
        spi_framing: sched
            .spi_framing_modes()
            .into_iter()
            .map(|(bus, mode)| hauksbee_engine::result::CosimSpiFraming {
                bus,
                mode: mode.as_str().to_string(),
            })
            .collect(),
    })
}

/// Warn (advisory, stderr) when the board sits among sibling `.kicad_pcb` files —
/// a multi-board product (e.g. a main board with a separate ESC/daughter board in
/// a sibling folder). A clean verdict on ONE file reads as "the product is fine"
/// when the user may have meant the whole thing. Best-effort; never fails the run.
/// Whether informational `note:` lines should be shown for this invocation.
/// They go to stderr and only for an interactive human: suppressed under
/// `--quiet`, under `--json` (machine output), and when stdout is piped or
/// redirected (not a TTY), so report and pipeline output stays clean while the
/// note stays discoverable for interactive users. Pure so it is unit-testable
/// without a real terminal.
fn notes_visible(quiet: bool, json: bool, stdout_is_tty: bool) -> bool {
    !quiet && !json && stdout_is_tty
}

/// The single gate every chatty informational note routes through, so `--quiet`
/// (and JSON / non-TTY suppression) is honoured uniformly and future notes
/// inherit the behaviour by going through here instead of a bare `eprintln!`.
#[derive(Clone, Copy)]
struct Notes {
    enabled: bool,
}

impl Notes {
    fn new(quiet: bool, json: bool) -> Self {
        Notes {
            enabled: notes_visible(
                quiet,
                json,
                std::io::IsTerminal::is_terminal(&std::io::stdout()),
            ),
        }
    }

    /// Emit a single-line informational note (prefixed `note:`) on stderr, unless
    /// notes are suppressed for this invocation.
    fn say(&self, msg: impl std::fmt::Display) {
        if self.enabled {
            eprintln!("note: {msg}");
        }
    }
}

fn warn_sibling_boards(board: &std::path::Path, notes: Notes) {
    if !notes.enabled {
        return;
    }
    let Ok(abs) = std::fs::canonicalize(board) else {
        return;
    };
    let Some(dir) = abs.parent() else {
        return;
    };
    let mut found: Vec<std::path::PathBuf> = Vec::new();
    let is_hidden = |p: &std::path::Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with('.'))
            .unwrap_or(false)
    };
    let mut scan = |d: &std::path::Path| {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("kicad_pcb") && p != abs {
                    found.push(p);
                }
            }
        }
    };
    scan(dir);
    // Immediate CHILD directories (e.g. a daughter board in `KiCad/ESC_Board/`)
    // and SIBLING directories (children of the grandparent). One level only, and
    // hidden dirs (`.history`, `.git`) are skipped so we don't surface backups.
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() && !is_hidden(&p) {
                scan(&p);
            }
        }
    }
    if let Some(gp) = dir.parent() {
        if let Ok(rd) = std::fs::read_dir(gp) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() && p != dir && !is_hidden(&p) {
                    scan(&p);
                }
            }
        }
    }
    found.sort();
    found.dedup();
    if found.is_empty() {
        return;
    }
    notes.say(format!(
        "{} other board file(s) found nearby; this run only checks '{}':",
        found.len(),
        board.display()
    ));
    for p in found.iter().take(5) {
        eprintln!("  - {}", p.display());
    }
    if found.len() > 5 {
        eprintln!("  ... and {} more", found.len() - 5);
    }
    eprintln!("  If they are part of the same product, check each one separately.");
}

/// Trim, drop empties, and de-duplicate probe net names while preserving the
/// order the user gave (which becomes the CSV column order).
fn dedup_probes(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in raw {
        let n = n.trim();
        if !n.is_empty() && !out.iter().any(|e| e == n) {
            out.push(n.to_string());
        }
    }
    out
}

/// Validate `--probe` preconditions before a headless run: a `--probe-csv` sink
/// is required, and every probed net must exist on the board. An unknown net
/// fails loudly with near-matches, the same did-you-mean style the spec loader
/// uses for a bad net name.
fn validate_probes(
    probes: &[String],
    csv: Option<&Path>,
    known: &[String],
) -> anyhow::Result<()> {
    if probes.is_empty() {
        return Ok(());
    }
    if csv.is_none() {
        anyhow::bail!("--probe needs --probe-csv <path> to write the waveforms to");
    }
    let known_set: std::collections::HashSet<&str> = known.iter().map(String::as_str).collect();
    for net in probes {
        if !known_set.contains(net.as_str()) {
            let near = nearest_nets(net, known, 5);
            let hint = if near.is_empty() {
                String::new()
            } else {
                format!(" - did you mean: {}?", near.join(", "))
            };
            anyhow::bail!("--probe: net '{net}' not found on the board{hint}");
        }
    }
    Ok(())
}

/// Up to `limit` known net names closest to `target` by edit distance, favouring
/// substring matches. A compact twin of the spec loader's net suggester, kept
/// local because the engine binary cannot depend on the CI crate.
fn nearest_nets(target: &str, known: &[String], limit: usize) -> Vec<String> {
    let t = target.to_ascii_lowercase();
    let mut scored: Vec<(usize, &String)> = known
        .iter()
        .map(|name| {
            let n = name.to_ascii_lowercase();
            let contains = n.contains(&t) || t.contains(&n);
            let dist = levenshtein(&t, &n);
            let score = if contains { dist.saturating_sub(3) } else { dist };
            (score, name)
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)));
    let cutoff = (target.len() / 2).max(3);
    scored
        .into_iter()
        .filter(|(score, _)| *score <= cutoff)
        .take(limit)
        .map(|(_, name)| name.clone())
        .collect()
}

/// Classic Levenshtein edit distance.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

fn run_headless(
    engine: &mut HauksbeeEngine,
    seconds: f64,
    uart_seen: &mut bool,
    quiet: bool,
    strict: bool,
    probes: &[String],
    probe_csv: Option<&Path>,
) -> anyhow::Result<Vec<hauksbee_engine::FaultEvent>> {
    use hauksbee_engine::{FaultEvent, FaultKind};
    use hauksbee_server::engine::Engine;
    // External emulator backends (Renode/QEMU) advance over a socket: a fine 1 ms
    // chunk means thousands of round-trips and a co-sim that looks frozen for
    // minutes. Use a coarse 10 ms chunk for them and print progress so a slow
    // STM32 run is legible. In-process AVR stays at 1 kHz (fast, more resolution).
    let external = engine
        .scheduler()
        .mcu_identities()
        .iter()
        .any(|(_, backend, _)| backend.starts_with("renode:") || backend.starts_with("qemu:"));
    let frame_dt = if external { 10.0 / 1000.0 } else { 1.0 / 1000.0 };
    if external {
        engine.scheduler_mut().chunk_s = frame_dt;
        eprintln!(
            "co-sim: {seconds:.2}s on an external emulator (slow — roughly wall-clock \
             per simulated second; this is normal for Renode/QEMU). Progress:"
        );
    } else {
        eprintln!("co-sim: {seconds:.2}s headless...");
    }
    let mut t = 0.0;
    let mut next_progress = seconds / 5.0; // ~5 progress lines over the run
    let mut last_uart: Vec<u8> = Vec::new();
    let mut faults: Vec<FaultEvent> = Vec::new();
    // Probe recording: one (time_s, [volts per probed net]) row per chunk. The
    // net order follows the order the user gave, which becomes the CSV columns.
    let mut probe_rows: Vec<(f64, Vec<f64>)> = Vec::new();
    while t < seconds {
        // Refuse rather than fake (05 §3b): under --strict, stop as soon as the
        // analog solve has been stuck for a whole streak of chunks. Continuing
        // would burn wall time producing more held-voltage frames the strict gate
        // is about to reject anyway, so break and let the caller exit 3. Non-strict
        // runs complete so the failed windows and analog_valid:false are reported.
        if strict && engine.scheduler().analog_abort_tripped() {
            break;
        }
        if external && t >= next_progress {
            eprintln!("  ... {t:.2} / {seconds:.2}s simulated");
            next_progress += (seconds / 5.0).max(frame_dt);
        }
        let frame = engine.step(frame_dt);
        if !probes.is_empty() {
            // A probed net absent from the frame reads 0 V (e.g. a net collapsed
            // onto ground); validation already rejected genuinely unknown names.
            let volts = probes
                .iter()
                .map(|net| frame.net_voltages.get(net).copied().unwrap_or(0.0))
                .collect();
            probe_rows.push((frame.t, volts));
        }
        for bytes in frame.uart.values() {
            last_uart.extend_from_slice(bytes);
        }
        for f in frame.faults {
            faults.push(FaultEvent {
                component: f.component,
                kind: FaultKind::from_str(&f.kind),
                value: f.value,
                limit: f.limit,
                t: f.t,
                destroyed: f.destroyed,
            });
        }
        t += frame_dt;
    }

    *uart_seen = !last_uart.is_empty();
    // The activity table + UART dump are human-facing. Under `--json` (quiet) the
    // SAME data is emitted structurally via CosimJson.activity_summary, so printing
    // it here would corrupt stdout for a machine consumer. Suppress when quiet.
    if !quiet {
        let sched = engine.scheduler();
        println!(
            "\nsimulated {:.3}s over {} nets",
            sched.sim_time,
            sched.stats.len()
        );
        // Sort nets by activity (toggle count then range).
        let mut rows: Vec<_> = sched.stats.iter().collect();
        rows.sort_by(|a, b| {
            b.1.toggles.cmp(&a.1.toggles).then(
                (b.1.max_v - b.1.min_v)
                    .partial_cmp(&(a.1.max_v - a.1.min_v))
                    .unwrap(),
            )
        });
        println!("\nmost active nets:");
        println!(
            "┌────────────────────────────┬──────────┬──────────┬──────────┐\n\
             │ Net                        │ min (V)  │ max (V)  │ toggles  │\n\
             ├────────────────────────────┼──────────┼──────────┼──────────┤"
        );
        for (name, st) in rows.iter().take(15) {
            let min_v = if st.min_v.is_finite() { st.min_v } else { 0.0 };
            let max_v = if st.max_v.is_finite() { st.max_v } else { 0.0 };
            println!(
                "│ {:<26} │ {:>8.3} │ {:>8.3} │ {:>8} │",
                truncate(name, 26),
                min_v,
                max_v,
                st.toggles
            );
        }
        println!("└────────────────────────────┴──────────┴──────────┴──────────┘");

        // Analog-fidelity line (05 §3b): if any chunk failed to converge, say so
        // and where, so the default text mode never presents held-stale voltages
        // as a quiet, healthy run.
        let failed = sched.failed_chunk_count();
        if failed > 0 {
            println!(
                "\nanalog_valid: false ({failed} chunk(s) failed to converge); \
                 those windows held stale voltages:"
            );
            for &(start_s, end_s) in sched.failed_windows() {
                println!("  [{:.6}s .. {:.6}s)", start_s, end_s);
            }
        }

        if !last_uart.is_empty() {
            let s = String::from_utf8_lossy(&last_uart);
            println!(
                "\nUART output ({} bytes):\n{}",
                last_uart.len(),
                s.trim_end()
            );
        }
    }

    // De-duplicate faults by (component, kind), keeping the worst value, so a
    // fault that trips every chunk is reported once. Mirrors check_board_text.
    faults.sort_by(|a, b| {
        a.component
            .cmp(&b.component)
            .then(a.kind.as_str().cmp(b.kind.as_str()))
            .then(
                b.value
                    .partial_cmp(&a.value)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    faults.dedup_by(|a, b| a.component == b.component && a.kind.as_str() == b.kind.as_str());

    // Write the probe CSV: header `time_s` then one column per probed net, one
    // row per chunk. Done after the run so a slow board still streams its summary.
    if let Some(path) = probe_csv {
        let mut csv = String::from("time_s");
        for net in probes {
            csv.push(',');
            csv.push_str(net);
        }
        csv.push('\n');
        for (t, volts) in &probe_rows {
            csv.push_str(&format!("{t:.6}"));
            for v in volts {
                csv.push_str(&format!(",{v:.6}"));
            }
            csv.push('\n');
        }
        std::fs::write(path, csv)
            .map_err(|e| anyhow::anyhow!("writing probe CSV to {}: {e}", path.display()))?;
        if !quiet {
            eprintln!("wrote {} probe row(s) to {}", probe_rows.len(), path.display());
        }
    }

    Ok(faults)
}



fn serve(
    engine: HauksbeeEngine,
    port: u16,
    board_file: Option<(String, String)>,
) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let server = Server::new(Box::new(engine));
        let static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../frontend/dist");
        let dir = static_dir.exists().then(|| static_dir.clone());
        let addr = format!("127.0.0.1:{port}");

        if dir.is_some() {
            println!("\n  hauksbee is live. Open this in your browser:\n");
            println!("      http://{addr}\n");
            println!("  (2D/3D board view; Ctrl-C to stop.)\n");
        } else {
            // The frontend is a build artifact and is not checked in, so a fresh
            // clone has no dist/ yet. Serve the websocket regardless (so the API
            // and any external viewer still work) but tell the user exactly how
            // to get the live view rather than leaving them on a blank 404 page.
            println!("\n  hauksbee websocket server is live at ws://{addr}/ws  (Ctrl-C to stop)\n");
            println!("  The live 2D/3D viewer at http://{addr} needs the frontend built once:\n");
            println!("      cd frontend && bun install && bun run build\n");
            println!("  then re-run this command. For a quick non-visual check, try:\n");
            println!("      hauksbee run <board> --report      # bind table");
            println!("      hauksbee run <board> --drc          # copper shorts");
            println!("      hauksbee run <board> --headless     # co-sim summary\n");
        }
        server
            .serve_with_board(&addr, dir.as_deref(), board_file)
            .await
    })
}






fn file_name(p: &Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("board")
        .to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

#[cfg(test)]
mod notes_gate_tests {
    use super::notes_visible;

    #[test]
    fn shown_only_for_interactive_non_json_non_quiet() {
        // Default interactive terminal: the note is discoverable.
        assert!(notes_visible(false, false, true));
    }

    #[test]
    fn quiet_suppresses_notes() {
        assert!(!notes_visible(true, false, true));
        assert!(!notes_visible(true, false, false));
        assert!(!notes_visible(true, true, true));
    }

    #[test]
    fn json_never_emits_notes() {
        // --json is machine output: no note regardless of TTY or quiet.
        assert!(!notes_visible(false, true, true));
        assert!(!notes_visible(false, true, false));
    }

    #[test]
    fn piped_stdout_suppresses_notes() {
        // Non-TTY stdout (piped / redirected / CI): keep report output clean.
        assert!(!notes_visible(false, false, false));
    }
}
