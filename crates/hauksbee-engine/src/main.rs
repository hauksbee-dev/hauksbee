//! `hauksbee` CLI: argument parsing (clap derive) + dispatch + process-level
//! concerns (exit codes, the `--json` error envelope). Every command's logic
//! lives in the library: the report families in [`hauksbee_engine::reports`], the
//! subcommand handlers in [`hauksbee_engine::commands`]. The `--help` text (per
//! command, usage-on-error, did-you-mean) is generated from the clap structs
//! below, so this file is intentionally thin.
//!
//! Subcommands: `run` (bind a board, then report / TUI / co-sim / serve), `serve`
//! (web front door), `to-code` / `from-code` / `check-code` (Board-as-Code), `sim`
//! (SPICE deck), `doctor` (backend availability), `models lint`. See each
//! `Command` variant (or `--help`) for the full flag surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

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
    /// All four analyses run: operating point (`--op`), transient (`--tran`),
    /// AC small-signal sweep (`--ac`), and DC sweep (`--dc`) — force one with
    /// its flag, or let the deck's own directives choose. Output is CSV by
    /// default; `--format raw` writes an ngspice ASCII rawfile (the format
    /// `ngnutmeg`/`gaw`/`spicelib` read) and `--format both` writes CSV and
    /// rawfile side by side.
    ///
    /// Honesty: the tool refuses loudly rather than fake a number. `--format
    /// both` needs `--out` to name its two files and refuses (exit 2) without
    /// it. A well-formed deck we cannot honestly answer — e.g. `.ac` with no AC
    /// source — exits 3 explaining why. A malformed deck prints the loader's
    /// line-numbered error and exits 2. Only the ngspice *ASCII* rawfile is
    /// emitted (the binary rawfile variant is not written). The exact
    /// supported/refused card list is the drift-tested compatibility statement,
    /// `docs/spice-compat/compatibility.md`.
    ///
    /// Example:
    ///   hauksbee sim rc.cir --out rc.csv
    ///   hauksbee sim amp.cir --tran --print V(out) I(V1)
    ///   hauksbee sim rc.cir --ac --print V(out)   # Bode table (needs an AC source + .ac card)
    ///   hauksbee sim rc.cir --format raw --out rc.csv   # also writes rc.raw
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

    /// Model-library tooling: validate model TOML, manage installed model
    /// packs, and debug which entry wins resolution.
    ///
    /// `models lint <file>` checks a `[[models]]` db file (params per kind,
    /// plus each entry's `[models.logic]` block: schema validation, expression
    /// compilation, and the exhaustive combinational-cycle convergence check)
    /// or a `[sensor]` register-map spec. Every failure is a NAMED error tied
    /// to its entry — the same validation binding performs, runnable
    /// standalone so a spec author (or an LLM extraction pipeline) fails fast.
    ///
    /// `models add <path|url>` installs a model pack (a directory with a
    /// pack.toml manifest and models/*.toml) into ~/.hauksbee/packs;
    /// `models remove <name>` uninstalls it; `models list` shows what is
    /// installed. `models resolve <board>` prints, per component, which model
    /// entry won and from which priority layer (builtin=0 < pack=10 <
    /// user-dir=20 < --models-dir=30 < spice=40).
    ///
    /// Examples:
    ///   hauksbee models lint my_part.toml
    ///   hauksbee models add ./acme-sensors
    ///   hauksbee models resolve my_board.kicad_pcb
    Models(ModelsArgs),

    /// Watch a target and re-run the right check on every file change: a board
    /// runs `run --check`, a `.board` runs `check-code`, a `.toml` runs the spec
    /// through `hauksbee-ci`. Ctrl-C exits with the last run's code.
    ///
    /// Example:
    ///   hauksbee watch my_board.kicad_pcb --plain
    Watch(WatchArgs),
}

#[derive(Parser)]
struct WatchArgs {
    /// Board, `.board`, or hauksbee-ci spec (`.toml`) to watch.
    #[arg(value_name = "TARGET")]
    target: PathBuf,
    /// Stream plain-language reports (default: the expert report).
    #[arg(long, visible_alias = "explain")]
    plain: bool,
    /// Run the check once and exit (test the plumbing without watching).
    #[arg(long)]
    once: bool,
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
    /// Install a model pack from a directory, tarball, or git URL.
    Add(ModelsAddArgs),
    /// Uninstall a model pack by name.
    Remove(ModelsRemoveArgs),
    /// List installed model packs.
    List,
    /// Show, per board component, which model entry won and from which layer.
    Resolve(ModelsResolveArgs),
}

#[derive(Parser)]
struct ModelsLintArgs {
    /// A `[[models]]` db TOML file or a `[sensor]` spec TOML file.
    file: PathBuf,
}

#[derive(Parser)]
struct ModelsAddArgs {
    /// A pack directory, a .tar.gz/.tgz/.tar archive, or a git URL.
    source: String,
}

#[derive(Parser)]
struct ModelsRemoveArgs {
    /// The pack name (as shown by `models list`).
    name: String,
}

#[derive(Parser)]
struct ModelsResolveArgs {
    /// Board file to resolve (.kicad_pcb, .kicad_sch, .brd, .d356, gerbers…).
    board: PathBuf,
    /// Extra model directory, loaded at the --models-dir layer (priority 30).
    #[arg(long, value_name = "DIR")]
    models_dir: Option<PathBuf>,
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

    /// As-built overlay (.asbuilt.toml): the declarative physical delta between
    /// the design files and the real reworked board (cut traces, jumper wires,
    /// lifted pins, fitted component values), applied before simulating.
    #[arg(long, value_name = "FILE")]
    asbuilt: Option<PathBuf>,

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

    /// Output format. `csv` (default) writes one column per probe; `raw` writes
    /// an ngspice ASCII rawfile (to `--out` if given, else stdout); `both` writes
    /// a CSV and a rawfile side by side and requires `--out` to name them.
    #[arg(long, value_enum, default_value_t = hauksbee_engine::commands::sim::SimFormat::Csv)]
    format: hauksbee_engine::commands::sim::SimFormat,

    /// Force the DC operating point analysis.
    #[arg(long, group = "analysis")]
    op: bool,

    /// Force a transient run (needs a `.tran` card for the window).
    #[arg(long, group = "analysis")]
    tran: bool,

    /// Run the deck's `.ac` sweep (small-signal). Needs an `.ac` card and at
    /// least one source with an `AC <mag> [phase]` spec.
    #[arg(long, group = "analysis")]
    ac: bool,

    /// Run the deck's `.dc` sweep. Needs a `.dc <src> <start> <stop> <step>` card.
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
        Command::Run(args) => hauksbee_engine::commands::run::run(run_config(args), quiet),
        Command::ToCode(args) => {
            hauksbee_engine::commands::boardcode::to_code(&args.board, args.out.as_deref())
        }
        Command::FromCode(args) => hauksbee_engine::commands::boardcode::from_code(
            &args.code,
            args.out.as_deref(),
            args.relayout,
            args.incremental,
            args.route,
            args.route_grid,
        ),
        Command::CheckCode(args) => hauksbee_engine::commands::boardcode::check(
            &args.code,
            args.seconds,
            args.destructive,
            args.ambient,
        ),
        Command::Serve(args) => hauksbee_engine::commands::serve::run(args.port),
        Command::Doctor(args) => {
            hauksbee_engine::commands::doctor::run(args.backends, args.json)
        }
        Command::Sim(args) => hauksbee_engine::commands::sim::run(
            &args.file,
            args.out.as_deref(),
            args.format,
            args.op,
            args.tran,
            args.ac,
            args.dc,
            &args.print,
        ),
        Command::Models(args) => match args.command {
            ModelsCommand::Lint(args) => hauksbee_engine::commands::models::lint(&args.file),
            ModelsCommand::Add(args) => hauksbee_engine::commands::models::add(&args.source),
            ModelsCommand::Remove(args) => hauksbee_engine::commands::models::remove(&args.name),
            ModelsCommand::List => hauksbee_engine::commands::models::list(),
            ModelsCommand::Resolve(args) => hauksbee_engine::commands::models::resolve(
                &args.board,
                args.models_dir.as_deref(),
            ),
        },
        Command::Watch(args) => {
            hauksbee_engine::commands::watch::run(args.target, args.plain, args.once)
        }
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


/// Deconstruct the parsed `RunArgs` (clap) into the library's plain [`RunConfig`],
/// so the run orchestrator lives in `hauksbee_engine::commands::run` while
/// argument parsing stays here.
fn run_config(a: RunArgs) -> hauksbee_engine::commands::run::RunConfig {
    hauksbee_engine::commands::run::RunConfig {
        board: a.board,
        firmware: a.firmware,
        asbuilt: a.asbuilt,
        seconds: a.seconds,
        headless: a.headless,
        report: a.report,
        drc: a.drc,
        ampacity: a.ampacity,
        lint: a.lint,
        si: a.si,
        resources: a.resources,
        usb_c: a.usb_c,
        thermal: a.thermal,
        ambient: a.ambient,
        plain: a.plain,
        json: a.json,
        strict: a.strict,
        strict_thermal: a.strict_thermal,
        strict_boot: a.strict_boot,
        list_nets: a.list_nets,
        check: a.check,
        oracle: a.oracle,
        apply_shorts: a.apply_shorts,
        serve: a.serve,
        tui: a.tui,
        port: a.port,
        models_dir: a.models_dir,
        ac: a.ac,
        ac_node: a.ac_node,
        ac_csv: a.ac_csv,
        ac_loop: a.ac_loop,
        probe: a.probe,
        probe_csv: a.probe_csv,
    }
}
