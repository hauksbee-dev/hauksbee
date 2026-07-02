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
    self, ac_is_all_sentinel, coverage_open_active_refs, lint_findings_json, no_signal_path_reason,
    si_findings_json, thermal_coverage, thermal_validity, usbc_finding_json, AcJson, AcNetJson, BindSummary,
    strict_analog_exit_code, BootGateJson, CheckCoverage, CosimFailedWindow, CosimJson, DrcStructured,
    JsonNote, JsonNoteKind, JsonReport, NetActivity, ThermalDeviceJson, ThermalJson, Validity,
    EXIT_INVALID_FOR_ANALYSIS,
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
    let result = match cli.command {
        Command::Run(args) => cmd_run(args),
        Command::ToCode(args) => cmd_to_code(args),
        Command::FromCode(args) => cmd_from_code(args),
        Command::CheckCode(args) => cmd_check_code(args),
        Command::Serve(args) => cmd_serve(args),
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

fn cmd_run(args: RunArgs) -> anyhow::Result<()> {
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
    // meant the whole thing. Human-facing only (stderr); skipped under --json.
    if !args.json {
        warn_sibling_boards(&args.board);
    }
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
        let bound = bind_board(&board, &lib);
        let summary = BindSummary::from_report(&bound.report);
        let drc = if altium.is_some() {
            ExtractedBoard::altium_drc(&raw)?
        } else {
            ExtractedBoard::drc_with_clearance_rules(
                &text,
                kicad_pro_clearance_rules(&args.board, &board),
            )?
        };
        let drc_structured = DrcStructured::from_report(&drc);
        let lint = hauksbee_engine::checks::engine_lint(&board, &lib);
        let geo_text = if altium.is_some() { None } else { Some(text.as_str()) };
        let si = board.si_checks(geo_text);
        // USB-C CC compliance, only when the board has a USB-C receptacle.
        let usbc = hauksbee_engine::usb_c_report(&board);

        if args.json {
            let mut jr = JsonReport::new(&bound.name, summary);
            jr.drc = Some(drc_structured);
            let mut findings = lint_findings_json(&lint);
            findings.extend(si_findings_json(&si));
            // Fold USB-C in as a finding so the aggregate stays one valid JSON doc.
            findings.extend(usbc.as_ref().and_then(usbc_finding_json));
            jr.findings = Some(findings);
            println!("{}", jr.to_json());
        } else if args.plain {
            println!("== Copper spacing (DRC) ==");
            print!(
                "{}",
                hauksbee_engine::plain_drc_structured(&drc_structured).render()
            );
            println!("\n== Connectivity / lint ==");
            print!("{}", hauksbee_engine::plain_netlint(&lint).render());
            println!("\n== Signal integrity ==");
            print!("{}", hauksbee_engine::plain_si(&si).render());
            if let Some(u) = &usbc {
                println!("\n== USB-C CC compliance ==");
                print!("{}", u.render_plain());
            }
        } else {
            print!("{}", bound.report.render_table());
            print!("{}", summary.render_banner());
            println!("\n== Copper spacing (DRC) ==");
            print!("{}", drc_structured.render());
            println!("\n== Connectivity / lint ==");
            print!("{}", hauksbee_extract::render_netlint(&lint));
            println!("\n== Signal integrity ==");
            print!("{}", hauksbee_extract::render_si(&si));
            if let Some(u) = &usbc {
                println!("\n== USB-C CC compliance ==");
                print!("{}", u.render());
            }
        }
        let usbc_serious = usbc.as_ref().is_some_and(|u| u.is_serious());
        // Unvalidated board format (KiCad 10+) → its shorts may be phantom; do
        // not fail the gate on them (the caveat is still printed above).
        let drc_gates = drc.version_warning.is_none() && drc.short_count() > 0;
        if args.strict && (drc_gates || lint_fails(&lint) || si_fails(&si) || usbc_serious) {
            std::process::exit(2);
        }
        return Ok(());
    }

    if args.report {
        // The bind table is a description of the board, not a pass/fail check, so
        // --plain / --strict do not apply to it. We now augment it with an honest
        // role-aware summary (Fix #5): critical_parts_bound + active_path_unresolved,
        // and a loud WARNING when the active circuit is open, instead of a bare %.
        let bound = bind_board(&board, &lib);
        let summary = BindSummary::from_report(&bound.report);
        if args.json {
            let report = JsonReport::new(&bound.name, summary);
            println!("{}", report.to_json());
        } else {
            print!("{}", bound.report.render_table());
            print!("{}", summary.render_banner());
            // Plain bottom line (Marco): a 74-row bind table that ends in scary
            // "NOT trustworthy" warnings reads like the tool broke. Give --plain a
            // one-line verdict that says what it means and what's still usable.
            if args.plain {
                let n = summary.critical_parts_bound_n;
                let m = summary.critical_parts_total;
                let open = summary
                    .active_path_unresolved
                    .iter()
                    .filter(|u| u.active_ic)
                    .count();
                println!();
                if open > 0 {
                    println!(
                        "Bottom line: {n} of {m} critical parts modelled. {open} active IC(s) above are \
                         unresolved/open, so firmware/analog/AC/thermal results on their nets would be \
                         INCOMPLETE — but the copper checks are unaffected (run --drc). Add models with \
                         --models-dir to cover them."
                    );
                } else if m > 0 {
                    println!("Bottom line: all {m} critical parts modelled; the board binds cleanly.");
                } else {
                    println!("Bottom line: no active ICs to model; this is a passive board.");
                }
            }
        }
        return Ok(());
    }

    // --drc: run geometric short / clearance detection, print, exit.
    if args.drc {
        let report = if altium.is_some() {
            ExtractedBoard::altium_drc(&raw)?
        } else {
            // KiCad 10 keeps class clearances in the sibling .kicad_pro. Resolve
            // concrete net names here (the CLI has both the board path and the
            // extracted netlist), then hand the DRC a pairwise clearance resolver.
            ExtractedBoard::drc_with_clearance_rules(
                &text,
                kicad_pro_clearance_rules(&args.board, &board),
            )?
        };
        if args.json {
            // Grouped DRC (Fix #8): shorts kept verbatim, clearance findings
            // grouped by (net_a, net_b, layer), at-limit separated from below-rule.
            let bound = bind_board(&board, &lib);
            let mut jr = JsonReport::new(&bound.name, BindSummary::from_report(&bound.report));
            jr.drc = Some(DrcStructured::from_report(&report));
            println!("{}", jr.to_json());
        } else if args.plain {
            // Plain mode renders from the SAME grouped structure as text/json so
            // all surfaces agree: duplicates collapsed, and gap==rule labelled
            // "at minimum clearance (no margin)" rather than the wrong "below".
            print!(
                "{}",
                hauksbee_engine::plain_drc_structured(&DrcStructured::from_report(&report)).render()
            );
        } else {
            // Grouped, honest DRC: one line per (net pair + cause) with a count,
            // and gap==rule labelled "at minimum clearance (no margin)" rather
            // than the wrong "below the spacing the board asks for" (Fix #8).
            print!("{}", DrcStructured::from_report(&report).render());
        }
        if args.oracle && !args.json {
            print!("{}", oracle_cross_check(&args.board, &report));
        }
        // Strict: any true short fails the gate (clearance-only does not). An
        // unvalidated board format (KiCad 10+) yields possibly-phantom shorts, so
        // it does not gate (the printed caveat tells the user to cross-check).
        if args.strict && report.version_warning.is_none() && report.short_count() > 0 {
            std::process::exit(2);
        }
        return Ok(());
    }

    // --ampacity: IPC-2221 capacity-only report. No current is fabricated here:
    // without a per-net current spec this tells the user the bottleneck capacity
    // and explicitly asks for a current before pass/fail.
    if args.ampacity {
        let rows = if altium.is_some() {
            Vec::new()
        } else {
            let doc = forge_sexpr::parse(&text)?;
            let copper = doc
                .root()
                .map(hauksbee_extract::net_copper_from_root)
                .unwrap_or_default();
            hauksbee_extract::trace_capacity_report(
                &copper,
                &hauksbee_extract::TraceAudit::default(),
            )
        };
        print!("{}", hauksbee_extract::render_trace_capacity_report(&rows));
        return Ok(());
    }

    // --lint: run the connectivity lint-class checks, the boot strap-pin lint
    // (which needs the model db's per-part strap tables), and the MCU internal
    // resource-conflict check (a lint-class structural check too), print, exit.
    if args.lint {
        let mut report = hauksbee_engine::checks::engine_lint(&board, &lib);
        report
            .findings
            .extend(hauksbee_engine::checks::device_decode::device_decode_lint(&board, &lib).findings);
        if args.json {
            let bound = bind_board(&board, &lib);
            let mut jr = JsonReport::new(&bound.name, BindSummary::from_report(&bound.report));
            jr.findings = Some(lint_findings_json(&report));
            println!("{}", jr.to_json());
        } else if args.plain {
            print!("{}", hauksbee_engine::plain_netlint(&report).render());
        } else {
            print!("{}", hauksbee_extract::render_netlint(&report));
        }
        // Surface pin-role GUESS warnings: roles the binder inferred from the
        // configurable pin-rule table rather than an explicit pin-function.
        // Nothing is silently guessed, so the lint reports each one.
        let bound = bind_board(&board, &lib);
        let guesses: Vec<(String, String)> = bound
            .report
            .guess_warnings()
            .map(|(r, g)| (r.to_string(), g.to_string()))
            .collect();
        if !guesses.is_empty() {
            println!("\npin-role guesses ({}):", guesses.len());
            for (r, g) in &guesses {
                println!("  ? {r}: {g}");
            }
        }
        if args.strict && lint_fails(&report) {
            std::process::exit(2);
        }
        return Ok(());
    }

    // --resources: run only the MCU internal resource-conflict check, print, exit.
    if args.resources {
        // resources_lint = resource conflicts + the unchecked-MCU coverage note,
        // so a clean result is not mistaken for "checked and conflict-free".
        let report = hauksbee_engine::checks::resources_lint(&board, &lib);
        if args.json {
            let bound = bind_board(&board, &lib);
            let mut jr = JsonReport::new(&bound.name, BindSummary::from_report(&bound.report));
            jr.findings = Some(lint_findings_json(&report));
            println!("{}", jr.to_json());
        } else if args.plain {
            print!("{}", hauksbee_engine::plain_netlint(&report).render());
        } else {
            print!("{}", hauksbee_extract::render_netlint(&report));
        }
        if args.strict && lint_fails(&report) {
            std::process::exit(2);
        }
        return Ok(());
    }

    // --usb-c: run the USB-C CC attach classifier (the RPi 4 re-derivation) and
    // print the compliance report. The capability existed but was unreachable from
    // any user-facing surface; this is its CLI front door.
    if args.usb_c {
        match hauksbee_engine::usb_c_report(&board) {
            None => {
                if args.json {
                    println!("{{\"check\":\"usb_c_cc\",\"level\":\"info\",\"headline\":\"no USB-C receptacle detected\"}}");
                } else {
                    println!("USB-C CC compliance: no USB-C receptacle with CC nets found on this board.");
                }
            }
            Some(report) => {
                if args.json {
                    println!("{}", report.to_json());
                } else if args.plain {
                    print!("{}", report.render_plain());
                } else {
                    print!("{}", report.render());
                }
                if args.strict && report.is_serious() {
                    std::process::exit(2);
                }
            }
        }
        return Ok(());
    }

    // --si: run the signal-integrity / physics static checks, print, exit. The
    // geometry-bearing checks (antenna keepout, USB length skew) need the raw
    // KiCad layout text, so it is passed through.
    if args.si {
        // Altium geometry is not yet threaded into the SI text checks, so pass
        // None there; the connectivity-based SI checks still run on `board`.
        let geo_text = if altium.is_some() {
            None
        } else {
            Some(text.as_str())
        };
        let mut report = board.si_checks(geo_text);
        // Engine-layer SI checks whose attribution needs the bound DB models:
        // trace ampacity (current attribution + IPC-2221) and input-cap ripple
        // (converter topology + cap ripple rating). These augment the
        // extract-layer SI report exactly the way --lint augments its report
        // with the strap lint.
        hauksbee_engine::checks::ampacity::append_ampacity(&board, &lib, geo_text, &mut report);
        hauksbee_engine::checks::ripple::append_ripple(&board, &lib, &mut report);
        if args.json {
            let bound = bind_board(&board, &lib);
            let mut jr = JsonReport::new(&bound.name, BindSummary::from_report(&bound.report));
            jr.findings = Some(si_findings_json(&report));
            println!("{}", jr.to_json());
        } else if args.plain {
            print!("{}", hauksbee_engine::plain_si(&report).render());
        } else {
            print!("{}", hauksbee_extract::render_si(&report));
        }
        if args.strict && si_fails(&report) {
            std::process::exit(2);
        }
        return Ok(());
    }

    // --ac: small-signal AC sweep on the bound circuit, print Bode + (optional)
    // loop-stability margins, then exit. Informational like the other reports.
    if let Some(ac_arg) = &args.ac {
        let bound = bind_board(&board, &lib);
        return cmd_ac(
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
        let bound = bind_board(&board, &lib);
        let mut jr = JsonReport::new(&bound.name, BindSummary::from_report(&bound.report));
        let drc = if altium.is_some() {
            ExtractedBoard::altium_drc(&raw)?
        } else {
            ExtractedBoard::drc_with_clearance_rules(
                &text,
                kicad_pro_clearance_rules(&args.board, &board),
            )?
        };
        jr.drc = Some(DrcStructured::from_report(&drc));
        let lint = hauksbee_engine::checks::engine_lint(&board, &lib);
        let geo_text = if altium.is_some() { None } else { Some(text.as_str()) };
        let si = board.si_checks(geo_text);
        let mut findings = lint_findings_json(&lint);
        findings.extend(si_findings_json(&si));
        jr.findings = Some(findings);
        println!("{}", jr.to_json());
        return Ok(());
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
                kicad_pro_clearance_rules(&args.board, &board),
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
        engine.scheduler_mut().set_ambient_c(args.ambient);
        let summary = BindSummary::from_report(engine.report());
        let board_name = engine.report().board_name.clone();
        let rows = collect_thermal(&mut engine, args.seconds.max(0.05));
        let validity = thermal_validity(rows.len(), &summary);
        // Coverage is the honest "N of M" companion to validity. It is NON-gating
        // by default (the partial case stays exit 0); only --strict-thermal
        // escalates a partial-coverage table to exit 3. validity stays unchanged.
        let coverage = thermal_coverage(rows.len(), &summary);
        // The open active ICs to NAME in the caveat, computed before `summary` is
        // moved into the JSON report.
        let coverage_refs = coverage_open_active_refs(&summary);
        if args.json {
            let mut jr = JsonReport::new(&board_name, summary);
            // Surface the partial-coverage caveat as an info note too, so a JSON
            // consumer that ignores `coverage` still sees the honesty annotation.
            if coverage.partial {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::Coverage,
                    message: thermal_coverage_caveat(&coverage),
                });
            }
            jr.thermal = Some(ThermalJson {
                validity: validity.clone(),
                ambient_c: args.ambient,
                devices: rows
                    .iter()
                    .map(|(r, tj, over)| ThermalDeviceJson {
                        reference: r.clone(),
                        tj_c: *tj,
                        over_limit: *over,
                    })
                    .collect(),
                coverage: Some(coverage.clone()),
            });
            println!("{}", jr.to_json());
        } else {
            render_thermal_text(&rows, args.ambient, &validity);
            // Partial-coverage caveat (text path): the table is real but some
            // active power IC on the live circuit is open/unresolved, so the
            // result understates the true thermal load. Naming the parts keeps
            // this from being a silent false-comfort pass.
            if coverage.partial {
                emit_thermal_coverage_caveat(&coverage, &coverage_refs);
            }
        }
        if !validity.valid {
            std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
        }
        // Opt-in escalation: partial coverage fails only under --strict-thermal.
        if coverage.partial && args.strict_thermal {
            std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
        }
        return Ok(());
    }

    if args.headless {
        let board_name = engine.report().board_name.clone();
        let summary = BindSummary::from_report(engine.report());
        let mut uart_seen = false;
        let faults = run_headless(
            &mut engine,
            args.seconds,
            &mut uart_seen,
            args.json,
            args.strict,
        );

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

        // Boot-safety advisory — computed once so the --json and --plain paths
        // agree. A control net the firmware drives (or pulls) HIGH and holds from
        // reset, that **switches a transistor/relay** and has **no bias resistor**
        // setting a safe default: a MOSFET gate / relay / motor enable / igniter
        // energised at power-up. The switch requirement is the zero-FP guard — it
        // is what separates a genuine load-control net (e.g. the igniter gate fed
        // by a mis-mapped SoftwareSerial pull-up) from an ordinary `INPUT_PULLUP`
        // button input, which is also held high but switches nothing.
        let held_high_boot_nets: Vec<String> = engine
            .scheduler()
            .firmware_held_high_nets()
            .into_iter()
            .filter(|net| net_drives_a_switch(&board, net) && net_has_no_bias_resistor(&board, net))
            .collect();
        let has_boot_advisory = !held_high_boot_nets.is_empty();

        // Informational boot-state panel: what the firmware does to each
        // transistor gate at power-up (driven HIGH / driven LOW / floating).
        // Reported, not judged — so an ambiguous case is a line the user reads,
        // never a false alarm. Computed once for both --plain and --json.
        //
        // Only meaningful when firmware actually ran: with no `--firmware`, or a
        // firmware that never executed (`zero_activity`), every gate would read
        // "floating" — which says nothing about the design, only that nothing
        // drove the pins. Suppress the panel in those cases (the zero-activity
        // warning already covers a stalled firmware).
        let gate_rows = if args.firmware.is_some() && !zero_activity {
            let gates = transistor_gate_nets(&board);
            // The panel reports the actual level, so it uses the UNFILTERED
            // held-high set — NOT `held_high_boot_nets`, whose switch/no-bias
            // filter is for the *warning* only. (Filtering here inverted the
            // label: a gate driven HIGH that also has a bias resistor would drop
            // out of "held high" and be misread as LOW.) `configured` is the set
            // of nets the firmware actively drove as outputs, which separates a
            // strong output-high from a weak internal pull-up.
            let held_high: std::collections::HashSet<String> =
                engine.scheduler().firmware_held_high_nets().into_iter().collect();
            let configured: std::collections::HashSet<String> =
                engine.scheduler().firmware_output_configured_nets().into_iter().collect();
            let driven: std::collections::HashSet<String> =
                engine.scheduler().firmware_driven_nets().into_iter().collect();
            boot_gate_states(&gates, &held_high, &configured, &driven)
        } else {
            Vec::new()
        };

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
            for net in &held_high_boot_nets {
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
            for net in &held_high_boot_nets {
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
                print!("{}", render_boot_gate_panel(&gate_rows));
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
            for net in &held_high_boot_nets {
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
fn cmd_ac(
    bound: &hauksbee_engine::BoundBoard,
    ac_arg: &str,
    ac_nodes: &[String],
    csv: Option<&Path>,
    ac_loop: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    use hauksbee_solve::{AcAnalysis, AcSpec, LoopStability, SolverOptions};

    let spec = AcSpec::parse(ac_arg).map_err(|e| anyhow::anyhow!("--ac: {e}"))?;
    let circuit = &bound.circuit;

    // Default to every non-ground node if no --ac-node given.
    let nodes: Vec<String> = if ac_nodes.is_empty() {
        (1..circuit.node_count())
            .map(|i| circuit.node_name(hauksbee_ir::NodeId(i as u32)).to_string())
            .collect()
    } else {
        ac_nodes.to_vec()
    };

    let resp = AcAnalysis::new(SolverOptions::default())
        .run(circuit, &spec)
        .map_err(|e| anyhow::anyhow!("AC analysis: {e}"))?;

    // Collect the bode rows once so both the validity check and the renderers
    // read the SAME data (one structured result, no forked logic).
    let summary = BindSummary::from_report(&bound.report);
    let per_net: Vec<(String, Vec<(f64, f64, f64)>)> = nodes
        .iter()
        .map(|net| (net.clone(), resp.bode(circuit, net)))
        .collect();

    // Fix #1 (CRITICAL): an AC sweep where EVERY reported net is at the -6000 dB
    // sentinel has no signal path — it is a meaningless result, not data. Refuse
    // to present it as a Bode table; name the unresolved driving ICs and exit 3
    // ("board invalid for the requested analysis"), never 0.
    let nonempty: Vec<&(String, Vec<(f64, f64, f64)>)> =
        per_net.iter().filter(|(_, b)| !b.is_empty()).collect();

    // Fix #1b (HIGH honesty hole): if EVERY requested net produced no data at all
    // (none exist in the circuit), `nonempty` is empty. Previously this slipped
    // past the all-sentinel guard (which requires `!nonempty.is_empty()`) and the
    // JSON path emitted `ac: { valid: true, nets: [] }` with exit 0 — a meaningless
    // result reported as valid. Refuse it: name the missing requested nodes, emit
    // valid:false, and exit 3, exactly like the all-sentinel path. Only fires when
    // the user explicitly asked for nodes; the "every node" default never lands
    // here because at least one real node exists in any bound circuit.
    if nonempty.is_empty() {
        let missing = nodes.join(", ");
        let reason = format!("no requested AC nodes found in the circuit: {missing}");
        if json {
            let mut jr = JsonReport::new(&bound.name, summary);
            jr.ac = Some(AcJson {
                validity: Validity::invalid(reason),
                nets: Vec::new(),
                no_signal_path_nets: Vec::new(),
                coverage: None,
            });
            println!("{}", jr.to_json());
        } else {
            eprintln!("WARNING: AC result not valid — {reason}");
            eprintln!(
                "  (none of the requested --ac-node nets exist in this circuit; \
                 check the net names against the board, then re-run.)"
            );
        }
        std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
    }

    let all_sentinel =
        !nonempty.is_empty() && nonempty.iter().all(|(_, b)| ac_is_all_sentinel(b));

    if all_sentinel {
        // Name a representative net for the reason (the first reported one).
        let net = nonempty
            .first()
            .map(|(n, _)| n.as_str())
            .unwrap_or("the requested node");
        let reason = no_signal_path_reason(net, &summary);
        if json {
            let mut jr = JsonReport::new(&bound.name, summary);
            jr.ac = Some(AcJson {
                validity: Validity::invalid(reason),
                nets: Vec::new(),
                no_signal_path_nets: Vec::new(),
                coverage: None,
            });
            println!("{}", jr.to_json());
        } else {
            eprintln!("WARNING: AC result not valid — {reason}");
            eprintln!(
                "  (every reported net is at the {:.0} dB floor: no path to drive it. \
                 Bind the driving ICs with --models-dir, then re-run.)",
                result::AC_FLOOR_DB
            );
        }
        std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
    }

    // Loop-net validity guard (degeneracy) — applied to BOTH --json and text
    // BEFORE either emits. A loop/break net that is missing from the circuit OR
    // sits at the dB floor has no feedback path to measure: LoopStability would
    // yield a meaningless ~-6000 dB gain on the text path, while the --json path
    // (which returns just below) would emit valid:true and exit 0 — a structured
    // false-pass. Refuse it identically on both surfaces with exit 3.
    if let Some(loop_net) = ac_loop {
        let loop_bode = resp.bode(circuit, loop_net);
        if loop_bode.is_empty() || ac_is_all_sentinel(&loop_bode) {
            let reason = if loop_bode.is_empty() {
                format!("loop/break net '{loop_net}' not found in the circuit")
            } else {
                no_signal_path_reason(loop_net, &summary)
            };
            if json {
                // Structured refusal on the JSON surface too — a consumer reading
                // stdout (not the exit code) must see valid:false, not empty output.
                let mut jr = JsonReport::new(&bound.name, summary);
                jr.ac = Some(AcJson {
                    validity: Validity::invalid(reason),
                    nets: vec![],
                    no_signal_path_nets: vec![loop_net.to_string()],
                    coverage: None,
                });
                println!("{}", jr.to_json());
            } else {
                eprintln!("WARNING: --ac-loop result not valid — {reason}");
                eprintln!(
                    "  (no feedback path to measure at '{loop_net}'. Bind the driving \
                     ICs with --models-dir, then re-run.)"
                );
            }
            std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
        }
    }

    if json {
        // Valid sweep: emit the structured bode per net. Skip empty/not-found
        // nets AND any individual net that is all-sentinel (no path to THIS net),
        // so a JSON consumer never sees -6000 dB rows presented as real data
        // alongside valid:true. The skipped nets are listed so the omission is
        // explicit, never silent.
        let mut jr = JsonReport::new(&bound.name, summary);
        let nets: Vec<AcNetJson> = per_net
            .iter()
            .filter(|(_, b)| !b.is_empty() && !ac_is_all_sentinel(b))
            .map(|(net, b)| AcNetJson {
                net: net.clone(),
                points: b.iter().map(|(f, db, ph)| [*f, *db, *ph]).collect(),
            })
            .collect();
        let no_path: Vec<String> = per_net
            .iter()
            .filter(|(_, b)| !b.is_empty() && ac_is_all_sentinel(b))
            .map(|(net, _)| net.clone())
            .collect();
        // Honest coverage for a partially-valid sweep: some requested nets carry
        // signal, others sit at the floor. Non-gating; mirrors `no_signal_path_nets`.
        let requested = nets.len() + no_path.len();
        let coverage = if no_path.is_empty() {
            None
        } else {
            let frac = if requested == 0 {
                1.0
            } else {
                (nets.len() as f64 / requested as f64).clamp(0.0, 1.0)
            };
            Some(CheckCoverage {
                resolved_fraction: frac,
                dissipating_count: nets.len(),
                total_active_count: requested,
                open_active_on_live_circuit: no_path.len(),
                partial: true,
            })
        };
        jr.ac = Some(AcJson {
            validity: Validity::valid(),
            nets,
            no_signal_path_nets: no_path,
            coverage,
        });
        println!("{}", jr.to_json());
        return Ok(());
    }

    // Print a Bode table per requested node. Track how many of the requested
    // nets had no signal path so we can print an end-of-run summary that matches
    // the JSON surface's `no_signal_path_nets` list (text/JSON parity).
    let mut no_path_nets: Vec<String> = Vec::new();
    let mut requested_nets = 0usize;
    for (net, bode) in &per_net {
        if bode.is_empty() {
            eprintln!("warning: net '{net}' not found in circuit; skipping");
            continue;
        }
        requested_nets += 1;
        // A single net at the floor amid others that carry signal is still a
        // local "no path here" — caveat it rather than presenting -6000 as data.
        if ac_is_all_sentinel(bode) {
            no_path_nets.push(net.clone());
            println!(
                "\nAC sweep: net '{net}' — NO SIGNAL PATH (all points at the {:.0} dB floor); result not meaningful for this net.",
                result::AC_FLOOR_DB
            );
            continue;
        }
        println!("\nAC sweep: net '{net}' ({} points)", bode.len());
        println!(
            "┌────────────────┬───────────────┬───────────────┐\n\
             │ Freq (Hz)      │ Mag (dB)      │ Phase (deg)   │\n\
             ├────────────────┼───────────────┼───────────────┤"
        );
        for (f, db, ph) in bode {
            println!("│ {f:>14.4} │ {db:>13.4} │ {ph:>13.3} │");
        }
        println!("└────────────────┴───────────────┴───────────────┘");
    }

    // End-of-run partial-sentinel summary (text/JSON parity): name how many of
    // the requested nets had no signal path, matching JSON's no_signal_path_nets.
    // Non-gating: a partially-valid sweep still exits 0 on the text path.
    if !no_path_nets.is_empty() {
        println!(
            "\nAC coverage: {} of {} requested net(s) had no signal path (at the {:.0} dB floor): {}.",
            no_path_nets.len(),
            requested_nets,
            result::AC_FLOOR_DB,
            no_path_nets.join(", "),
        );
    }

    // Active-circuit coverage caveat (the false-comfort case): the per-net sentinel
    // check above only catches a net AT the dB floor. A board whose active devices
    // (op-amps, drivers, MOSFETs, MCU) are UNRESOLVED -> OPEN still solves as a
    // passive shell and prints a clean, authoritative-looking Bode that is NOT the
    // real loop. The --json path carries this via the bind summary (active_path_
    // unresolved); the TEXT path must say it too, or the result reads as trustworthy.
    let ac_open = coverage_open_active_refs(&summary);
    if !ac_open.is_empty() {
        eprintln!(
            "\nCAVEAT: this AC result is NOT trustworthy — {} active IC(s) on the live circuit \
             are unresolved/open ({}), so the response/loop shown is a passive shell, not the real \
             circuit. Bind them with --models-dir, then re-run.",
            ac_open.len(),
            ac_open.join(", "),
        );
    }

    // Optional loop-stability report (text only). The loop-net validity guard
    // above already refused a missing/floored loop net (exit 3) for both surfaces,
    // so by here the net carries signal and LoopStability has a real response.
    if let Some(loop_net) = ac_loop {
        let st = LoopStability::from_response(&resp, circuit, loop_net)
            .map_err(|e| anyhow::anyhow!("--ac-loop: {e}"))?;
        let m = st.margins();
        println!("\nLoop stability at net '{loop_net}':");
        println!("  DC/low-f loop gain : {:.2} dB", m.dc_gain_db);
        match (m.gain_crossover_hz, m.phase_margin_deg) {
            (Some(fc), Some(pm)) => {
                println!("  gain crossover     : {fc:.4} Hz (|T| = 0 dB)");
                println!("  phase margin       : {pm:.2} deg");
            }
            _ => println!("  gain crossover     : none in band (loop never reaches 0 dB)"),
        }
        match (m.phase_crossover_hz, m.gain_margin_db) {
            (Some(fp), Some(gm)) => {
                println!("  phase crossover    : {fp:.4} Hz (phase = -180 deg)");
                println!("  gain margin        : {gm:.2} dB");
            }
            _ => println!("  phase crossover    : none in band (phase never reaches -180 deg)"),
        }
    }

    // Optional CSV of the full sweep.
    if let Some(path) = csv {
        let mut out = String::from("net,freq_hz,mag_db,phase_deg\n");
        for net in &nodes {
            for (f, db, ph) in resp.bode(circuit, net) {
                out.push_str(&format!("{net},{f},{db},{ph}\n"));
            }
        }
        std::fs::write(path, out)?;
        eprintln!("wrote {}", path.display());
    }

    Ok(())
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

/// Run a short headless co-sim and collect the steady-state junction-temperature
/// estimate per dissipating device. Returns `(reference, peak_Tj_C, over_limit)`
/// rows, sorted hottest-first. The caller decides how to render (text / JSON) and
/// whether the empty-table case is a meaningless result (Fix #1).
fn collect_thermal(engine: &mut HauksbeeEngine, seconds: f64) -> Vec<(String, f64, bool)> {
    use hauksbee_server::engine::Engine;
    use std::collections::HashMap;

    eprintln!("thermal: {seconds:.2}s co-sim...");
    let frame_dt = 1.0 / 1000.0;
    let mut t = 0.0;
    // Peak temperature seen per device over the run (steady state is reached
    // quickly; the peak is the worst-case junction temperature).
    let mut peak_temp: HashMap<String, f64> = HashMap::new();
    let mut overtemp: HashMap<String, (f64, f64)> = HashMap::new(); // ref -> (Tj, limit)
    while t < seconds {
        let frame = engine.step(frame_dt);
        for (reference, &tj) in &engine.scheduler().temp_states() {
            let e = peak_temp
                .entry(reference.clone())
                .or_insert(f64::NEG_INFINITY);
            if tj > *e {
                *e = tj;
            }
        }
        for f in &frame.faults {
            if f.kind == "overtemperature" {
                overtemp.insert(f.component.clone(), (f.value, f.limit));
            }
        }
        t += frame_dt;
    }

    let mut rows: Vec<(String, f64, bool)> = peak_temp
        .into_iter()
        .filter(|(_, v)| v.is_finite())
        .map(|(r, tj)| {
            let over = overtemp.contains_key(&r);
            (r, tj, over)
        })
        .collect();
    rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    rows
}

/// Render the thermal result as text. When the result is invalid (empty table
/// because the dissipating devices are unresolved/open), print a loud WARNING
/// naming the reason rather than a near-empty table that reads as "runs cool".
/// The one-line partial-coverage caveat text (shared by JSON note + stderr).
fn thermal_coverage_caveat(coverage: &CheckCoverage) -> String {
    // `covered` is the active ICs that actually made it into the table; the bug to
    // avoid is mixing it with `dissipating_count` (which counts ALL dissipating
    // rows, mostly passives) — that produced nonsense like "40 of 7 active ICs".
    let covered = coverage
        .total_active_count
        .saturating_sub(coverage.open_active_on_live_circuit);
    format!(
        "thermal coverage is PARTIAL: only {covered} of {} active power IC(s) on the \
         live circuit are in the table ({} open/unresolved). The {} dissipating part(s) \
         shown are real, but the result UNDERSTATES the true load: open power ICs \
         dissipate nothing in simulation.",
        coverage.total_active_count,
        coverage.open_active_on_live_circuit,
        coverage.dissipating_count,
    )
}

/// Emit the partial-coverage caveat on the text path, naming the open active
/// ICs. Non-gating by default (does not change the exit code unless
/// `--strict-thermal` is set by the caller).
fn emit_thermal_coverage_caveat(coverage: &CheckCoverage, open_refs: &[String]) {
    eprintln!("CAVEAT: {}", thermal_coverage_caveat(coverage));
    if !open_refs.is_empty() {
        eprintln!(
            "  open/unresolved active IC(s): {}. Bind them with --models-dir, then re-run \
             (or pass --strict-thermal to FAIL on partial coverage).",
            open_refs.join(", ")
        );
    }
}

fn render_thermal_text(rows: &[(String, f64, bool)], ambient_c: f64, validity: &Validity) {
    if !validity.valid {
        let reason = validity
            .reason
            .as_deref()
            .unwrap_or("no resolved dissipating devices");
        eprintln!("WARNING: thermal result not valid — {reason}");
        eprintln!(
            "  (a thermal table covering no dissipating devices is NOT a 'runs cool' pass. \
             Bind the power ICs with --models-dir, then re-run.)"
        );
        return;
    }
    println!("\nsteady-state junction temperature (Tj = {ambient_c:.0} C + P * theta_JA):");
    if rows.is_empty() {
        println!("  no dissipating device reached a measurable temperature (board carries no static load).");
        return;
    }
    println!(
        "┌────────────────────┬───────────┬──────────┐\n\
         │ Component          │  Tj (C)   │  status  │\n\
         ├────────────────────┼───────────┼──────────┤"
    );
    let mut n_over = 0;
    for (reference, tj, over) in rows {
        let status = if *over {
            n_over += 1;
            "OVER".to_string()
        } else {
            "ok".to_string()
        };
        println!(
            "│ {:<18} │ {:>7.1}   │ {:<8} │",
            truncate(reference, 18),
            tj,
            status
        );
    }
    println!("└────────────────────┴───────────┴──────────┘");
    if n_over > 0 {
        println!("\n{n_over} device(s) over their junction-temperature limit.");
    } else {
        println!("\nall dissipating devices within their junction-temperature limit.");
    }
}

/// True when a net has no *bias* resistor — no resistor tying it toward a power
/// rail or ground, so nothing on the board fixes its power-up level (it is set
/// entirely by firmware). Used to sharpen the boot-control-net heads-up to nets
/// with no hardware fail-safe. A resistor whose other terminal is NOT a rail or
/// ground is a series element (e.g. GPIO -> R -> MOSFET gate), which sets no
/// default level, so it does NOT count as a bias and the net is still flagged.
/// An unknown/unresolvable net name returns false (assume biased; stay silent).
fn net_has_no_bias_resistor(board: &hauksbee_extract::ExtractedBoard, net_name: &str) -> bool {
    let Some(net) = board.nets.iter().find(|n| n.name == net_name) else {
        return false;
    };
    let is_resistor =
        |c: &hauksbee_extract::Component| c.reference.starts_with('R') || c.reference.starts_with('r');
    for (comp, _) in board.net_members(net.id) {
        // A DNP (not-assembled) resistor is electrically absent: it must NOT be
        // credited as a bias (that would suppress a real hazard).
        if comp.dnp || !is_resistor(comp) {
            continue;
        }
        for p in &comp.pins {
            if let Some(other) = p.net {
                if other != net.id && is_power_or_ground_net(board, other) {
                    return false; // a pull-up/down to a rail/ground: a hardware default exists
                }
            }
        }
    }
    true
}

/// True when a net connects to a transistor or relay — a switch whose control
/// input (a MOSFET/BJT gate-base, a relay coil) at the wrong level at power-up
/// switches a load. This is the load-bearing zero-FP guard for the boot
/// advisory: it separates a genuine load-control net (e.g. an igniter gate fed
/// by a mis-mapped pull-up) from an ordinary `INPUT_PULLUP` button input — both
/// read HIGH at boot, but only the former switches anything. Reference prefix
/// 'Q' = transistor, 'K' = relay (standard KiCad designators). DNP (not
/// assembled) switches don't count. (Pin-function data that would let us require
/// the *control* terminal specifically is absent in PCB-only extraction, so any
/// terminal of a populated Q/K qualifies — a deliberate, conservative breadth.)
fn net_drives_a_switch(board: &hauksbee_extract::ExtractedBoard, net_name: &str) -> bool {
    let Some(net) = board.nets.iter().find(|n| n.name == net_name) else {
        return false;
    };
    board.net_members(net.id).iter().any(|(c, _)| {
        !c.dnp
            && matches!(
                c.reference.chars().next().map(|ch| ch.to_ascii_uppercase()),
                Some('Q') | Some('K')
            )
    })
}

/// Whether a net id names a power rail or ground. Grounds: the GND/AGND/VSS
/// family. Rails: a leading '+', a `V…`/`…V` name (VCC/VDD/VBAT/VMOT/VSYS/VIN
/// and bare voltages like 12V/3V3/5V/1V8). The broad `V`-name rule is
/// deliberately inclusive — a missed rail would mis-read a real pull as "no
/// bias" and over-flag, so on the zero-FP surface we err toward recognising
/// rails (a false rail only *suppresses* an advisory, the safe direction here is
/// the opposite, hence breadth).
fn is_power_or_ground_net(board: &hauksbee_extract::ExtractedBoard, net_id: i64) -> bool {
    let Some(net) = board.nets.iter().find(|n| n.id == net_id) else {
        return false;
    };
    let n = net.name.to_ascii_uppercase();
    // Ground family.
    if n.starts_with("GND") || n.ends_with("GND") || n.starts_with("VSS") {
        return true;
    }
    // Explicit '+' rail (e.g. "+3V3", "+5V", "+12V").
    if n.starts_with('+') {
        return true;
    }
    // V-prefixed rails (VCC/VDD/VBAT/VMOT/VSYS/VIN/VIO/VREF…) and bare voltage
    // names with a digit and a 'V' (e.g. "12V", "3V3", "5V0", "1V8", "9V").
    let v_named = n.starts_with('V') && n.len() >= 2;
    let voltage_named = n.contains('V')
        && n.chars().next().is_some_and(|c| c.is_ascii_digit())
        && n.chars().any(|c| c.is_ascii_digit());
    v_named || voltage_named
}

/// The pad number of a transistor's *control* terminal (a MOSFET gate / BJT
/// base), inferred from the footprint by package convention. Conservative:
/// returns `None` for any package whose control-pad position isn't reliable
/// (e.g. TO-92, whose lead order varies by part), so the boot-state panel simply
/// omits that device rather than mislabelling a row.
fn switch_control_pad(footprint: &str) -> Option<&'static str> {
    let f = footprint.to_ascii_uppercase();
    // 8-lead SINGLE power MOSFET (Power-SO-8 family): gate on pad 4, source on
    // 1-3, drain on 5-8. Checked before the 3-lead group ("SOT-23-8" also
    // contains "SOT-23"). SOT-23-8 is unambiguously a single power FET; a bare
    // SO-8/SOIC-8 is more often a DUAL FET or a gate-driver IC (gates on other
    // pads), so only treat those as pad-4 when the footprint says "power".
    let eight_lead_single = f.contains("SOT-23-8")
        || f.contains("SOT23-8")
        || ((f.contains("SO-8") || f.contains("SOIC-8") || f.contains("SO8"))
            && (f.contains("POWER") || f.contains("PWR")));
    if eight_lead_single {
        return Some("4");
    }
    // 3-lead discrete packages where the control terminal is pad 1 (MOSFET gate
    // G-D-S, BJT base B-C-E/B-E-C — pad 1 is the control either way).
    const THREE_LEAD: [&str; 12] = [
        "SOT-23", "SOT23", "SOT-323", "SOT323", "SC-70", "SC70", "TO-252", "DPAK", "TO-263",
        "D2PAK", "TO-220", "TO-247",
    ];
    if THREE_LEAD.iter().any(|p| f.contains(p)) {
        return Some("1");
    }
    None
}

/// A pad named as a MOSFET *gate* (`G`/`GATE`).
fn is_gate_pad_name(s: &str) -> bool {
    matches!(s.trim().to_ascii_uppercase().as_str(), "G" | "GATE")
}

/// A pad named as a BJT *base* (`B`/`BASE`). Kept separate from the gate name so
/// a 4-terminal MOSFET with an explicit bulk/body pad labelled `B` never has its
/// bulk picked over the real gate — gate names are tried first.
fn is_base_pad_name(s: &str) -> bool {
    matches!(s.trim().to_ascii_uppercase().as_str(), "B" | "BASE")
}

/// True for any transistor control-terminal pad name (gate or base). KiCad
/// MOSFET symbols commonly name pads `G`/`D`/`S`, which is more reliable than
/// footprint inference, so we prefer a name when present.
fn is_control_pad_name(s: &str) -> bool {
    is_gate_pad_name(s) || is_base_pad_name(s)
}

/// Every transistor (`Q…`) whose control terminal can be identified, paired with
/// the net on that terminal — the rows of the boot-state panel. The control pad
/// is found first by an explicit `G`/`GATE` pad name (then `B`/`BASE`), else by
/// footprint convention. DNP transistors and unidentifiable parts are skipped
/// (the panel omits a device rather than mislabel it).
fn transistor_gate_nets(board: &hauksbee_extract::ExtractedBoard) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for c in &board.components {
        if c.dnp
            || c.reference.chars().next().map(|ch| ch.to_ascii_uppercase()) != Some('Q')
        {
            continue;
        }
        let named = |is: fn(&str) -> bool| {
            c.pins
                .iter()
                .find(move |p| is(&p.number) || is(&p.function))
        };
        // 1. An explicit GATE pad, 2. an explicit BASE pad (gate wins over a
        // bulk pad also labelled `B`), 3. else the footprint's control pad.
        let pin = named(is_gate_pad_name).or_else(|| named(is_base_pad_name)).or_else(|| {
            switch_control_pad(&c.footprint).and_then(|pad| c.pins.iter().find(|p| p.number == pad))
        });
        let Some(net_id) = pin.and_then(|p| p.net) else {
            continue;
        };
        if let Some(net) = board.nets.iter().find(|n| n.id == net_id) {
            if !net.name.is_empty() {
                out.push((c.reference.clone(), net.name.clone()));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// What the firmware does to a gate net at power-up. Reported factually (no
/// channel-type safety claim — a HIGH gate is "on" for a low-side N-MOSFET but
/// "off" for a high-side P-MOSFET, which the netlist can't disambiguate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BootGateState {
    /// Strong push-pull HIGH (the pin is configured as an output and held high).
    DrivenHigh,
    /// HIGH via a weak internal pull-up (the firmware left the pin an input but
    /// enabled its pull-up) — e.g. a serial RX pin mis-mapped onto a gate. The
    /// gate still goes high, but by accident rather than an intended drive.
    PulledHigh,
    DrivenLow,
    Floating,
}

impl BootGateState {
    fn label(self) -> &'static str {
        match self {
            BootGateState::DrivenHigh => "driven HIGH and held",
            BootGateState::PulledHigh => "pulled HIGH (weak internal pull-up)",
            BootGateState::DrivenLow => "driven LOW and held",
            BootGateState::Floating => "never driven (floating)",
        }
    }
    /// A short marker for the states worth a look (active or undefined at reset);
    /// LOW is reported without a marker (the common held-off case).
    fn marker(self) -> &'static str {
        match self {
            BootGateState::DrivenHigh | BootGateState::PulledHigh => "  <- switched at power-up",
            BootGateState::Floating => "  <- undefined until firmware drives it",
            BootGateState::DrivenLow => "",
        }
    }
    fn json(self) -> &'static str {
        match self {
            BootGateState::DrivenHigh => "driven_high",
            BootGateState::PulledHigh => "pulled_high",
            BootGateState::DrivenLow => "driven_low",
            BootGateState::Floating => "floating",
        }
    }
}

/// Classify each transistor gate's power-up state from the co-sim drive sets.
/// `held_high` is the UNFILTERED set of nets held high (a factual level, so it
/// must not be the safety-filtered advisory list); `configured` is the set the
/// firmware drove as outputs (used to split a strong HIGH from a pull-up);
/// `driven` is the union (output-configured ∪ written) — a net in neither is
/// floating. A `pinMode(OUTPUT)`-with-no-write pin appears in `driven` and
/// reports "driven LOW"; note the analog solve leaves it tri-stated (it only
/// enables a Thevenin leg on a PORT edge), so panel and solver intentionally
/// disagree there — the panel is the more faithful account of the real pin.
fn boot_gate_states(
    gates: &[(String, String)],
    held_high: &std::collections::HashSet<String>,
    configured: &std::collections::HashSet<String>,
    driven: &std::collections::HashSet<String>,
) -> Vec<(String, String, BootGateState)> {
    gates
        .iter()
        .map(|(reference, net)| {
            let state = if held_high.contains(net) {
                if configured.contains(net) {
                    BootGateState::DrivenHigh
                } else {
                    BootGateState::PulledHigh
                }
            } else if driven.contains(net) {
                BootGateState::DrivenLow
            } else {
                BootGateState::Floating
            };
            (reference.clone(), net.clone(), state)
        })
        .collect()
}

/// Render the informational boot-state panel for the `--plain` surface: aligned
/// plain-language lines, one per transistor gate, reporting (not judging) what
/// the firmware does to it at power-up. The arrows flag the active / undefined
/// cases for a non-engineer to verify; LOW is reported without a flag.
fn render_boot_gate_panel(rows: &[(String, String, BootGateState)]) -> String {
    let ref_w = rows.iter().map(|(r, _, _)| r.len()).max().unwrap_or(3).max(2);
    let net_w = rows.iter().map(|(_, n, _)| n.len()).max().unwrap_or(8).max(8);
    let mut s = String::from(
        "\nPower-up state of MOSFET / transistor gates — what the firmware does to each\n\
         switch the moment the board powers up. Verify each is the level you intend\n\
         (a HIGH or floating gate can switch a load on before the firmware means to):\n",
    );
    for (reference, net, state) in rows {
        s.push_str(&format!(
            "  {reference:<ref_w$}  {net:<net_w$}  {}{}\n",
            state.label(),
            state.marker(),
        ));
    }
    s
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
    })
}

/// Warn (advisory, stderr) when the board sits among sibling `.kicad_pcb` files —
/// a multi-board product (e.g. a main board with a separate ESC/daughter board in
/// a sibling folder). A clean verdict on ONE file reads as "the product is fine"
/// when the user may have meant the whole thing. Best-effort; never fails the run.
fn warn_sibling_boards(board: &std::path::Path) {
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
    eprintln!(
        "note: {} other board file(s) found nearby — this run only checks '{}':",
        found.len(),
        board.display()
    );
    for p in found.iter().take(5) {
        eprintln!("  - {}", p.display());
    }
    if found.len() > 5 {
        eprintln!("  ... and {} more", found.len() - 5);
    }
    eprintln!("  If they are part of the same product, check each one separately.");
}

fn run_headless(
    engine: &mut HauksbeeEngine,
    seconds: f64,
    uart_seen: &mut bool,
    quiet: bool,
    strict: bool,
) -> Vec<hauksbee_engine::FaultEvent> {
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
    faults
}

/// Strict-mode predicate for the lint report: a high- or medium-severity finding
/// fails the gate. Low-severity notes (cosmetic / unlikely-to-bite) do not, to
/// keep the gate from being noisy.
fn lint_fails(report: &hauksbee_extract::NetLintReport) -> bool {
    use hauksbee_extract::Severity;
    report
        .findings
        .iter()
        .any(|f| matches!(f.severity, Severity::High | Severity::Medium))
}

/// Strict-mode predicate for the SI report: any real finding (high/medium/low,
/// but not the informational computed-value notes) fails the gate.
fn si_fails(report: &hauksbee_extract::SiReport) -> bool {
    report.finding_count() > 0
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

/// Read project-file netclass clearances and resolve them to this board's
/// concrete net names. KiCad 10 stores this in the sibling `.kicad_pro` rather
/// than the `.kicad_pcb`; missing/malformed project files simply leave DRC on
/// the board/default rules.
fn kicad_pro_clearance_rules(
    board_path: &std::path::Path,
    board: &hauksbee_extract::ExtractedBoard,
) -> Option<hauksbee_extract::ClearanceRules> {
    let text = std::fs::read_to_string(board_path.with_extension("kicad_pro")).ok()?;
    hauksbee_extract::clearance_rules_from_kicad_pro(
        &text,
        board.nets.iter().map(|n| n.name.as_str()),
    )
}

/// Parse a version string like "10.0.3" (or "KiCad 9.0.3") into a comparable
/// (major, minor, patch) tuple, ignoring any surrounding text.
fn parse_version(s: &str) -> (u32, u32, u32) {
    let n: Vec<u32> = s
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|x| x.parse().ok())
        .collect();
    (
        n.first().copied().unwrap_or(0),
        n.get(1).copied().unwrap_or(0),
        n.get(2).copied().unwrap_or(0),
    )
}

/// Extract the "actual N mm" distance from a kicad-cli DRC violation description
/// like "Clearance violation (zone clearance 0.5000 mm; actual 0.0000 mm)".
fn actual_mm(desc: &str) -> Option<f64> {
    let rest = &desc[desc.find("actual ")? + "actual ".len()..];
    rest.split_whitespace().next()?.parse().ok()
}

/// Locate a usable `kicad-cli` (the geometric-DRC oracle): PATH first, then the
/// standard macOS / Linux / Homebrew install locations, preferring the highest
/// version (a KiCad-10 cli is needed to read v20260206 boards). KiCad is NOT
/// bundled with hauksbee; this finds an existing install. Returns (path, version).
fn find_kicad_cli() -> Option<(String, String)> {
    let mut candidates: Vec<String> = vec!["kicad-cli".to_string()];
    let home = std::env::var("HOME").unwrap_or_default();
    for base in ["/Applications".to_string(), format!("{home}/Applications")] {
        if let Ok(rd) = std::fs::read_dir(&base) {
            for e in rd.flatten() {
                let name = e.file_name();
                if name.to_str().is_some_and(|n| n.starts_with("KiCad")) {
                    // Handles both `<base>/KiCad*.app/...` (entry is the bundle) and
                    // `<base>/KiCad*/KiCad.app/...` (entry is a folder holding it,
                    // the macOS .dmg / cask layout).
                    for sub in [
                        "Contents/MacOS/kicad-cli",
                        "KiCad.app/Contents/MacOS/kicad-cli",
                    ] {
                        let cli = e.path().join(sub);
                        if cli.exists() {
                            candidates.push(cli.to_string_lossy().into_owned());
                        }
                    }
                }
            }
        }
    }
    for p in [
        "/usr/bin/kicad-cli",
        "/usr/local/bin/kicad-cli",
        "/opt/homebrew/bin/kicad-cli",
    ] {
        if std::path::Path::new(p).exists() {
            candidates.push(p.to_string());
        }
    }
    let mut best: Option<(String, String, (u32, u32, u32))> = None;
    for c in candidates {
        let Ok(out) = std::process::Command::new(&c).arg("version").output() else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let ver = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let parsed = parse_version(&ver);
        if best.as_ref().is_none_or(|b| parsed > b.2) {
            best = Some((c, ver, parsed));
        }
    }
    best.map(|(p, v, _)| (p, v))
}

/// Cross-check hauksbee's geometric DRC against KiCad's own `kicad-cli pcb drc`,
/// so a copper finding is self-confirming without running a second tool by hand.
/// Honest about the two tools' different scopes (KiCad's violation count includes
/// clearance / annular-ring / etc.), and flags the one case that matters: hauksbee
/// reporting a short the oracle does not (a likely hauksbee false positive).
fn oracle_cross_check(board: &std::path::Path, report: &hauksbee_extract::DrcReport) -> String {
    let Some((cli, ver)) = find_kicad_cli() else {
        return "\noracle: no kicad-cli found (PATH or /Applications). Install KiCad to \
                cross-check geometric DRC; see docs/ORACLES.md.\n"
            .to_string();
    };
    let tmp = std::env::temp_dir().join(format!(
        "hauksbee_oracle_drc_{}_{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_file(&tmp);
    let run = std::process::Command::new(&cli)
        .args(["pcb", "drc", "--severity-error", "--format", "json", "-o"])
        .arg(&tmp)
        .arg(board)
        .output();
    let Ok(out) = run else {
        return format!("\noracle (kicad-cli {ver}): failed to launch.\n");
    };
    let Ok(text) = std::fs::read_to_string(&tmp) else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut detail = Vec::new();
        if let Some(code) = out.status.code() {
            detail.push(format!("exit status {code}"));
        } else {
            detail.push("terminated by signal".to_string());
        }
        if !stderr.trim().is_empty() {
            detail.push(stderr.trim().to_string());
        }
        if !stdout.trim().is_empty() {
            detail.push(stdout.trim().to_string());
        }
        let why = detail.join("; ");
        return format!(
            "\noracle (kicad-cli {ver}): could not load this board{}. A KiCad-10 (>= 10.0) \
             cli is required for v20260206 boards.\n",
            if why.trim().is_empty() {
                String::new()
            } else {
                format!(" ({})", why.trim())
            }
        );
    };
    let _ = std::fs::remove_file(&tmp);
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    let violations = v.get("violations").and_then(|x| x.as_array());
    let nviol = violations.map_or(0, |a| a.len());
    let nunconn = v
        .get("unconnected_items")
        .and_then(|x| x.as_array())
        .map_or(0, |a| a.len());
    // What "a short" means in each tool: hauksbee = copper of two nets at gap <= 0
    // (touching). KiCad expresses the same fact two ways — a `shorting_items`
    // violation (its connectivity merged the nets) OR a `clearance`/`hole_clearance`
    // at actual ~0 mm (geometrically touching but not merged). Count both as the
    // oracle's confirmed touches; KiCad's other violations (annular, mask-bridge,
    // courtyard, sub-rule-but-positive clearance) are not net shorts. Counts do not
    // map 1:1 (the tools decompose a touch into different numbers of rows), so the
    // verdict is about presence/over-reporting, not exact equality.
    let confirmed = violations.map_or(0, |a| {
        a.iter()
            .filter(|x| {
                let ty = x.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if ty == "shorting_items" {
                    return true;
                }
                (ty == "clearance" || ty == "hole_clearance")
                    && x.get("description")
                        .and_then(|d| d.as_str())
                        .and_then(actual_mm)
                        .is_some_and(|a| a < 0.005)
            })
            .count()
    });
    let (shorts, clear) = (report.short_count(), report.clearance_violations().count());
    let verdict = if shorts == 0 && confirmed == 0 {
        "agree: neither finds touching copper".to_string()
    } else if shorts > 0 && confirmed == 0 {
        format!("hauksbee finds {shorts} short(s) the oracle does not — likely false positives, investigate")
    } else if shorts == 0 && confirmed > 0 {
        format!(
            "oracle finds {confirmed} touching-copper violation(s) hauksbee missed — investigate"
        )
    } else if shorts > confirmed * 2 {
        format!("both find touching copper, but hauksbee's {shorts} >> the oracle's {confirmed} — hauksbee likely over-reports; compare by location")
    } else {
        format!("agree: both find touching copper ({shorts} hauksbee / {confirmed} oracle; counts differ by decomposition)")
    };
    format!(
        "\noracle (kicad-cli {ver}): {confirmed} touching-copper violation(s), {nviol} total DRC \
         violation(s), {nunconn} unconnected.\n\
         hauksbee: {shorts} short(s), {clear} clearance. -> {verdict}.\n"
    )
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
mod boot_headsup_tests {
    use super::{
        boot_gate_states, is_control_pad_name, is_power_or_ground_net, net_drives_a_switch,
        net_has_no_bias_resistor, switch_control_pad, transistor_gate_nets, BootGateState,
    };
    use hauksbee_extract::{Component, ExtractedBoard, Net, Pin};

    fn pin(net: Option<i64>) -> Pin {
        Pin {
            number: "1".into(),
            net,
            function: String::new(),
            kind: String::new(),
            position: None,
        }
    }
    fn resistor(reference: &str, a: i64, b: i64) -> Component {
        Component {
            reference: reference.into(),
            value: "10k".into(),
            lib_id: String::new(),
            footprint: String::new(),
            position: None,
            layer: String::new(),
            properties: vec![],
            dnp: false,
            pins: vec![pin(Some(a)), pin(Some(b))],
        }
    }
    fn board(nets: &[(i64, &str)], comps: Vec<Component>) -> ExtractedBoard {
        ExtractedBoard {
            name: "t".into(),
            nets: nets
                .iter()
                .map(|(id, n)| Net {
                    id: *id,
                    name: (*n).into(),
                })
                .collect(),
            components: comps,
        }
    }

    #[test]
    fn rails_and_grounds_recognised_signals_not() {
        let b = board(
            &[
                (1, "GND"), (2, "+3V3"), (3, "VCC"), (4, "GNDA"), (5, "5V"),
                (6, "VMOT"), (7, "VSYS"), (8, "VIN"), (9, "12V"), (10, "1V8"),
                (11, "SIG"), (12, "DATA0"),
            ],
            vec![],
        );
        for id in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10] {
            assert!(is_power_or_ground_net(&b, id), "net {id} should be rail/ground");
        }
        assert!(!is_power_or_ground_net(&b, 11), "SIG must not read as rail/ground");
        assert!(!is_power_or_ground_net(&b, 12), "DATA0 must not read as rail/ground");
    }

    #[test]
    fn gate_net_with_no_resistor_has_no_bias() {
        // GATE has only the MCU pin + a MOSFET gate, no resistor at all.
        let b = board(&[(1, "GATE")], vec![]);
        assert!(net_has_no_bias_resistor(&b, "GATE"));
    }

    #[test]
    fn pulldown_to_ground_counts_as_bias() {
        // GATE -> R1 -> GND: a pull-down is a hardware fail-safe, so NOT flagged.
        let b = board(&[(1, "GATE"), (2, "GND")], vec![resistor("R1", 1, 2)]);
        assert!(!net_has_no_bias_resistor(&b, "GATE"));
    }

    #[test]
    fn series_resistor_to_a_signal_is_not_a_bias() {
        // GPIO -> R1 -> GATE: on GPIO, R1's far end is a signal, not a rail/
        // ground, so it sets no power-up default — the net is still flagged.
        let b = board(&[(1, "GPIO"), (2, "GATE")], vec![resistor("R1", 1, 2)]);
        assert!(net_has_no_bias_resistor(&b, "GPIO"));
    }

    #[test]
    fn unknown_net_name_is_treated_as_biased() {
        let b = board(&[(1, "GATE")], vec![]);
        assert!(!net_has_no_bias_resistor(&b, "does-not-exist"));
    }

    fn part(reference: &str, net: i64) -> Component {
        part_dnp(reference, net, false)
    }
    fn part_dnp(reference: &str, net: i64, dnp: bool) -> Component {
        Component {
            reference: reference.into(),
            value: String::new(),
            lib_id: String::new(),
            footprint: String::new(),
            position: None,
            layer: String::new(),
            properties: vec![],
            dnp,
            pins: vec![pin(Some(net))],
        }
    }

    #[test]
    fn net_to_transistor_or_relay_drives_a_switch() {
        // GATE -> Q1 (transistor) and COIL -> K1 (relay) both count; a net to
        // only a header / IC does not, so the boot advisory stays off pins that
        // switch nothing. A DNP (not-assembled) transistor does NOT count.
        let b = board(
            &[(1, "GATE"), (2, "COIL"), (3, "HDR"), (4, "DNPGATE")],
            vec![
                part("Q1", 1),
                part("K1", 2),
                part("J3", 3),
                part("U7", 3),
                part_dnp("Q9", 4, true),
            ],
        );
        assert!(net_drives_a_switch(&b, "GATE"));
        assert!(net_drives_a_switch(&b, "COIL"));
        assert!(!net_drives_a_switch(&b, "HDR"));
        assert!(!net_drives_a_switch(&b, "DNPGATE"), "a DNP transistor must not count");
        assert!(!net_drives_a_switch(&b, "missing"));
    }

    #[test]
    fn switch_control_pad_by_footprint() {
        assert_eq!(switch_control_pad("Package_TO_SOT_SMD:SOT-23"), Some("1"));
        assert_eq!(switch_control_pad("SOT-23-3"), Some("1"));
        assert_eq!(switch_control_pad("Package_TO_SOT_SMD:TO-252-3_DPAK"), Some("1"));
        // 8-lead single power MOSFET: gate on pad 4 (checked before SOT-23).
        assert_eq!(switch_control_pad("SOT-23-8_Handsoldering"), Some("4"));
        assert_eq!(switch_control_pad("Package_SO:SO-8_Power"), Some("4"));
        // Bare SO-8 / SOIC-8 is too often a dual FET or driver IC -> None.
        assert_eq!(switch_control_pad("Package_SO:SO-8"), None);
        assert_eq!(switch_control_pad("Package_SO:SOIC-8"), None);
        // Unknown / unreliable packages: None (the panel omits the device).
        assert_eq!(switch_control_pad("Package_TO_SOT_THT:TO-92"), None);
        assert_eq!(switch_control_pad("Resistor_SMD:R_0402"), None);
    }

    #[test]
    fn control_pad_names_recognised() {
        for s in ["G", "GATE", "g", "Base", "B"] {
            assert!(is_control_pad_name(s), "{s} should be a control pad name");
        }
        for s in ["D", "S", "1", "drain", ""] {
            assert!(!is_control_pad_name(s), "{s} must not be a control pad name");
        }
    }

    fn transistor(reference: &str, footprint: &str, pads: &[(&str, &str, i64)], dnp: bool) -> Component {
        Component {
            reference: reference.into(),
            value: String::new(),
            lib_id: String::new(),
            footprint: footprint.into(),
            position: None,
            layer: String::new(),
            properties: vec![],
            dnp,
            pins: pads
                .iter()
                .map(|(num, func, net)| Pin {
                    number: (*num).into(),
                    net: Some(*net),
                    function: (*func).into(),
                    kind: String::new(),
                    position: None,
                })
                .collect(),
        }
    }

    #[test]
    fn transistor_gate_nets_prefers_named_pad_then_footprint() {
        let b = board(
            &[(1, "GATE_A"), (2, "DRN"), (3, "SRC"), (4, "GATE_B"), (5, "X"), (6, "BULK")],
            vec![
                // Named G/D/S pads: the 'G' pad's net wins regardless of footprint.
                transistor("Q1", "whatever", &[("G", "", 1), ("D", "", 2), ("S", "", 3)], false),
                // Numbered SOT-23 pads, no names: footprint says control = pad 1.
                transistor("Q2", "SOT-23", &[("1", "", 4), ("2", "", 5), ("3", "", 2)], false),
                // DNP transistor: skipped.
                transistor("Q3", "SOT-23", &[("1", "", 1)], true),
                // Unknown footprint, no named pad: skipped (no mislabel).
                transistor("Q5", "TO-92", &[("1", "", 5)], false),
                // 4-terminal MOSFET with a bulk pad labelled 'B': the GATE pad
                // must win over the bulk, never picking BULK.
                transistor("Q4", "SOT-23", &[("G", "", 1), ("S", "", 3), ("D", "", 2), ("B", "", 6)], false),
            ],
        );
        let gates = transistor_gate_nets(&b);
        assert_eq!(
            gates,
            vec![
                ("Q1".to_string(), "GATE_A".to_string()),
                ("Q2".to_string(), "GATE_B".to_string()),
                ("Q4".to_string(), "GATE_A".to_string()),
            ]
        );
    }

    #[test]
    fn boot_gate_states_classifies_high_low_floating() {
        let gates = vec![
            ("Q1".to_string(), "DrivenHi".to_string()),
            ("Q2".to_string(), "PulledHi".to_string()),
            ("Q3".to_string(), "LoNet".to_string()),
            ("Q4".to_string(), "FloatNet".to_string()),
        ];
        let set = |xs: &[&str]| -> std::collections::HashSet<String> {
            xs.iter().map(|s| s.to_string()).collect()
        };
        let held_high = set(&["DrivenHi", "PulledHi"]);
        // DrivenHi is an output (strong high); PulledHi is NOT configured output
        // (a weak pull-up). LoNet is an output held low.
        let configured = set(&["DrivenHi", "LoNet"]);
        let driven = set(&["DrivenHi", "PulledHi", "LoNet"]);
        let rows = boot_gate_states(&gates, &held_high, &configured, &driven);
        assert_eq!(rows[0].2, BootGateState::DrivenHigh);
        assert_eq!(rows[1].2, BootGateState::PulledHigh);
        assert_eq!(rows[2].2, BootGateState::DrivenLow);
        assert_eq!(rows[3].2, BootGateState::Floating);
    }
}
