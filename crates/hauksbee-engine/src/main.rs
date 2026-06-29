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
//! - `run` (default)      : serve the live websocket (frontend/dist if present).
//! - `serve`              : local web front door: open a page, drop a board,
//!                          get the plain-language report in the browser.
//! - `to-code`            : decompile a board into editable Board-as-Code text.
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
    check_code, decompile_board_to_code, load_code, render_check_report, CheckOptions,
};
use hauksbee_engine::result::{
    self, ac_is_all_sentinel, coverage_open_active_refs, lint_findings_json, no_signal_path_reason,
    si_findings_json, thermal_coverage, thermal_validity, usbc_finding_json, AcJson, AcNetJson, BindSummary,
    CheckCoverage, CosimJson, DrcStructured, JsonNote, JsonNoteKind, JsonReport, NetActivity,
    ThermalDeviceJson, ThermalJson, Validity, EXIT_INVALID_FOR_ANALYSIS,
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
    let text = if altium.is_some() {
        String::new()
    } else {
        String::from_utf8_lossy(&raw).into_owned()
    };
    // A `.kicad_sch` may reference sub-sheets that live in sibling files, so
    // it must be loaded by path to recurse the hierarchy; everything else is
    // self-contained and sniffed from its content.
    let board = if let Some(b) = altium.clone() {
        b
    } else if args.board.extension().and_then(|e| e.to_str()) == Some("kicad_sch") {
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
        let report = hauksbee_engine::checks::engine_lint(&board, &lib);
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
        let report = board.si_checks(geo_text);
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
        let faults = run_headless(&mut engine, args.seconds, &mut uart_seen, args.json);

        // Co-sim honesty summary (Track B): total net toggles, UART activity, and
        // any chip substitution detected at build time. Built from the SAME run
        // stats the text table reads, so every surface agrees.
        let cosim = build_cosim_json(&engine, uart_seen);
        let total_toggles = cosim.as_ref().map(|c| c.total_toggles).unwrap_or(0);
        // A co-sim that drove no GPIO, produced no net toggles, AND emitted no
        // UART did not exercise the firmware. `any_gpio_driven()` is essential:
        // a firmware that drives a control line high and HOLDS it (boot-gate style)
        // has zero net toggles yet clearly ran, so a toggles-only test would cry
        // wolf on it. Determined BEFORE emitting so the refusal reaches every
        // surface — including --json when no MCU was instantiated (cosim is None).
        let zero_activity =
            total_toggles == 0 && !uart_seen && !engine.scheduler().any_gpio_driven();

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
            println!();
            print!("{}", report.render());
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

        // Strict: any fault raised during the run fails the gate.
        if args.strict && !faults.is_empty() {
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
fn cmd_to_code(args: ToCodeArgs) -> anyhow::Result<()> {
    let text = read_board_text(&args.board)?;
    let code = decompile_board_to_code(&text)?;
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

/// Run the co-sim headless for `seconds`, print the activity summary, and return
/// the faults raised (de-duplicated by component+kind, worst value kept) so the
/// caller can render them in plain language and/or gate on them under --strict.
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

    Some(CosimJson {
        mcu_ref,
        backend,
        requested_part,
        substituted,
        total_toggles,
        uart_seen,
        activity_summary,
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
