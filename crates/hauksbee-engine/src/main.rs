//! `hauksbee` CLI: bind a board and bring it to life.
//!
//! ```text
//! hauksbee run        <board-file> [--firmware <hex>] [--seconds N] [--headless]
//!                                 [--port 3001] [--report] [--drc] [--lint] [--si]
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
        .args(["report", "drc", "lint", "si", "resources", "thermal"])
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

    /// Print the connectivity lint + strap-pin + resource-conflict report and exit.
    #[arg(long, group = "report_mode")]
    lint: bool,

    /// Print the signal-integrity / physics static-check report and exit.
    #[arg(long, group = "report_mode")]
    si: bool,

    /// Print only the MCU internal resource-conflict report and exit.
    #[arg(long, group = "report_mode")]
    resources: bool,

    /// Run a short headless co-sim and print the steady-state junction-temperature
    /// estimate per dissipating device (Tj = Tambient + P * theta_JA), then exit.
    #[arg(long, group = "report_mode")]
    thermal: bool,

    /// Ambient temperature (C) for the --thermal estimate. Default 25 C.
    #[arg(long, default_value_t = 25.0, value_name = "C")]
    ambient: f64,

    /// Translate the report into plain language for a non-engineer: a one-line
    /// verdict, then each finding as what it is, why it matters, and what to do.
    /// Applies to --drc/--lint/--si/--resources and to --headless faults.
    #[arg(long, visible_alias = "explain")]
    plain: bool,

    /// Exit non-zero if a report (--drc/--lint/--si/--resources) finds problems,
    /// so it can gate a CI pipeline directly. Default stays exit 0 (scripts that
    /// only read the text are unaffected). Counts shorts + serious/medium lint &
    /// SI findings; clearance-only and low-severity notes do not fail the gate.
    #[arg(long, visible_alias = "fail-on-findings")]
    strict: bool,

    /// Bridge every detected copper short before simulating (show the consequences).
    #[arg(long)]
    apply_shorts: bool,

    /// Port for the live frontend websocket server (default flow).
    #[arg(long, default_value_t = 3001, value_name = "PORT")]
    port: u16,

    /// Extra model directory (highest priority), layered over the built-in DB.
    #[arg(long, value_name = "DIR")]
    models_dir: Option<PathBuf>,

    /// Small-signal AC sweep: `<fstart>:<fstop>:<points>[:lin]` (Hz; points per
    /// decade unless `:lin`). Linearises about the DC operating point and prints
    /// a Bode (magnitude dB + phase) table for `--ac-node`, then exits. Drive is
    /// a unit AC source on every independent source in the circuit.
    ///
    /// Example: hauksbee run board.kicad_pcb --ac 10:1e6:20 --ac-node OUT
    #[arg(long, value_name = "FSTART:FSTOP:POINTS")]
    ac: Option<String>,

    /// Output net(s) to report for `--ac` (repeatable). Defaults to every net.
    #[arg(long = "ac-node", value_name = "NET")]
    ac_node: Vec<String>,

    /// Write the full AC sweep (all reported nets) to this CSV file.
    #[arg(long, value_name = "FILE")]
    ac_csv: Option<PathBuf>,

    /// Measure loop stability at this break/output net: report gain crossover
    /// and phase margin. Use with `--ac`. The net is the far side of a loop
    /// broken by an injection `Vsource` (see docs/AC_ANALYSIS.md).
    #[arg(long = "ac-loop", value_name = "NET")]
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
    #[arg(long, default_value_t = 25.0, value_name = "C")]
    ambient: f64,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Run(args) => cmd_run(args),
        Command::ToCode(args) => cmd_to_code(args),
        Command::FromCode(args) => cmd_from_code(args),
        Command::CheckCode(args) => cmd_check_code(args),
        Command::Serve(args) => cmd_serve(args),
    };
    if let Err(e) = &result {
        eprintln!("error: {e}");
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

    if args.report {
        // The bind table is a description of the board, not a pass/fail check, so
        // --plain / --strict do not apply to it; print the table as before.
        let bound = bind_board(&board, &lib);
        print!("{}", bound.report.render_table());
        return Ok(());
    }

    // --drc: run geometric short / clearance detection, print, exit.
    if args.drc {
        let report = if altium.is_some() {
            ExtractedBoard::altium_drc(&raw)?
        } else {
            ExtractedBoard::drc(&text)?
        };
        if args.plain {
            print!("{}", hauksbee_engine::plain_drc(&report).render());
        } else {
            print!("{}", render_drc(&report));
        }
        // Strict: any true short fails the gate (clearance-only does not).
        if args.strict && report.short_count() > 0 {
            std::process::exit(2);
        }
        return Ok(());
    }

    // --lint: run the connectivity lint-class checks, the boot strap-pin lint
    // (which needs the model db's per-part strap tables), and the MCU internal
    // resource-conflict check (a lint-class structural check too), print, exit.
    if args.lint {
        let mut report = board.net_lint();
        let straps = hauksbee_engine::checks::straps::strap_lint(&board, &lib);
        report.findings.extend(straps.findings);
        report.findings.extend(board.resource_conflicts().findings);
        if args.plain {
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
        let report = board.resource_conflicts();
        if args.plain {
            print!("{}", hauksbee_engine::plain_netlint(&report).render());
        } else {
            print!("{}", hauksbee_extract::render_netlint(&report));
        }
        if args.strict && lint_fails(&report) {
            std::process::exit(2);
        }
        return Ok(());
    }

    // --si: run the signal-integrity / physics static checks, print, exit. The
    // geometry-bearing checks (antenna keepout, USB length skew) need the raw
    // KiCad layout text, so it is passed through.
    if args.si {
        // Altium geometry is not yet threaded into the SI text checks, so pass
        // None there; the connectivity-based SI checks still run on `board`.
        let geo_text = if altium.is_some() { None } else { Some(text.as_str()) };
        let report = board.si_checks(geo_text);
        if args.plain {
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
        return cmd_ac(&bound, ac_arg, &args.ac_node, args.ac_csv.as_deref(), args.ac_loop.as_deref());
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
            ExtractedBoard::drc(&text)?
        };
        let applied = engine.apply_drc_shorts(&report);
        eprintln!(
            "applied {applied} copper short(s) of {} detected ({} clearance violations)",
            report.short_count(),
            report.clearance_violations().count(),
        );
    }

    // --thermal: run a short co-sim, then print the steady-state junction
    // temperature per dissipating device and exit.
    if args.thermal {
        engine.scheduler_mut().set_ambient_c(args.ambient);
        run_thermal(&mut engine, args.seconds.max(0.05), args.ambient);
        return Ok(());
    }

    if args.headless {
        let faults = run_headless(&mut engine, args.seconds);
        if args.plain {
            println!();
            print!("{}", hauksbee_engine::plain_faults(&faults).render());
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

    // Print a Bode table per requested node.
    for net in &nodes {
        let bode = resp.bode(circuit, net);
        if bode.is_empty() {
            eprintln!("warning: net '{net}' not found in circuit; skipping");
            continue;
        }
        println!("\nAC sweep: net '{net}' ({} points)", bode.len());
        println!(
            "┌────────────────┬───────────────┬───────────────┐\n\
             │ Freq (Hz)      │ Mag (dB)      │ Phase (deg)   │\n\
             ├────────────────┼───────────────┼───────────────┤"
        );
        for (f, db, ph) in &bode {
            println!("│ {f:>14.4} │ {db:>13.4} │ {ph:>13.3} │");
        }
        println!("└────────────────┴───────────────┴───────────────┘");
    }

    // Optional loop-stability report.
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
        // server crate needs no dependency on the engine/extract crates.
        let analyze: hauksbee_server::frontdoor::Analyzer =
            Arc::new(|name: &str, contents: &str| hauksbee_engine::analyze_json(name, contents));
        hauksbee_server::frontdoor::serve(&addr, analyze).await
    })
}

/// Run a short headless co-sim and print the steady-state junction-temperature
/// estimate per dissipating device. Surfaces any over-temperature fault raised
/// (the same fault channel the live monitor uses) and exits 0 (informational,
/// like the other `run` reports).
fn run_thermal(engine: &mut HauksbeeEngine, seconds: f64, ambient_c: f64) {
    use hauksbee_server::engine::Engine;
    use std::collections::HashMap;

    eprintln!("thermal: {seconds:.2}s co-sim at {ambient_c:.0} C ambient...");
    let frame_dt = 1.0 / 1000.0;
    let mut t = 0.0;
    // Peak temperature seen per device over the run (steady state is reached
    // quickly; the peak is the worst-case junction temperature).
    let mut peak_temp: HashMap<String, f64> = HashMap::new();
    let mut overtemp: HashMap<String, (f64, f64)> = HashMap::new(); // ref -> (Tj, limit)
    while t < seconds {
        let frame = engine.step(frame_dt);
        for (reference, &tj) in &engine.scheduler().temp_states() {
            let e = peak_temp.entry(reference.clone()).or_insert(f64::NEG_INFINITY);
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

    let mut rows: Vec<(String, f64)> =
        peak_temp.into_iter().filter(|(_, v)| v.is_finite()).collect();
    rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    println!(
        "\nsteady-state junction temperature (Tj = {ambient_c:.0} C + P * theta_JA):"
    );
    if rows.is_empty() {
        println!("  no dissipating device reached a measurable temperature.");
        return;
    }
    println!(
        "┌────────────────────┬───────────┬──────────┐\n\
         │ Component          │  Tj (C)   │  status  │\n\
         ├────────────────────┼───────────┼──────────┤"
    );
    for (reference, tj) in &rows {
        let status = if let Some((_, limit)) = overtemp.get(reference) {
            format!("OVER {limit:.0}")
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
    if !overtemp.is_empty() {
        println!(
            "\n{} device(s) over their junction-temperature limit.",
            overtemp.len()
        );
    } else {
        println!("\nall dissipating devices within their junction-temperature limit.");
    }
}

/// Run the co-sim headless for `seconds`, print the activity summary, and return
/// the faults raised (de-duplicated by component+kind, worst value kept) so the
/// caller can render them in plain language and/or gate on them under --strict.
fn run_headless(
    engine: &mut HauksbeeEngine,
    seconds: f64,
) -> Vec<hauksbee_engine::FaultEvent> {
    use hauksbee_engine::{FaultEvent, FaultKind};
    use hauksbee_server::engine::Engine;
    eprintln!("co-sim: {seconds:.2}s headless...");
    let frame_dt = 1.0 / 1000.0; // 1 kHz frame cadence
    let mut t = 0.0;
    let mut last_uart: Vec<u8> = Vec::new();
    let mut faults: Vec<FaultEvent> = Vec::new();
    while t < seconds {
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

    let sched = engine.scheduler();
    println!("\nsimulated {:.3}s over {} nets", sched.sim_time, sched.stats.len());
    // Sort nets by activity (toggle count then range).
    let mut rows: Vec<_> = sched.stats.iter().collect();
    rows.sort_by(|a, b| {
        b.1.toggles
            .cmp(&a.1.toggles)
            .then((b.1.max_v - b.1.min_v).partial_cmp(&(a.1.max_v - a.1.min_v)).unwrap())
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
        println!("\nUART output ({} bytes):\n{}", last_uart.len(), s.trim_end());
    }

    // De-duplicate faults by (component, kind), keeping the worst value, so a
    // fault that trips every chunk is reported once. Mirrors check_board_text.
    faults.sort_by(|a, b| {
        a.component
            .cmp(&b.component)
            .then(a.kind.as_str().cmp(b.kind.as_str()))
            .then(b.value.partial_cmp(&a.value).unwrap_or(std::cmp::Ordering::Equal))
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

/// Render a geometric DRC report as a Unicode table plus a summary line.
fn render_drc(report: &hauksbee_extract::DrcReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "DRC: {} primitive(s), clearance rule {:.3} mm\n",
        report.primitive_count, report.clearance_mm
    ));
    if report.findings.is_empty() {
        out.push_str("no shorts or clearance violations.\n");
        return out;
    }
    out.push_str(
        "┌──────────┬──────────────────────────┬──────────────────────────┬────────┬──────────┬───────────────┐\n\
         │ Kind     │ Net A                    │ Net B                    │ Layer  │ Gap (mm) │ Location      │\n\
         ├──────────┼──────────────────────────┼──────────────────────────┼────────┼──────────┼───────────────┤\n",
    );
    for f in &report.findings {
        out.push_str(&format!(
            "│ {:<8} │ {:<24} │ {:<24} │ {:<6} │ {:>8.4} │ {:>5.1},{:<7.1} │\n",
            f.kind.as_str(),
            truncate(&f.net_a_name, 24),
            truncate(&f.net_b_name, 24),
            truncate(&f.layer, 6),
            f.gap_mm,
            f.x,
            f.y,
        ));
    }
    out.push_str(
        "└──────────┴──────────────────────────┴──────────────────────────┴────────┴──────────┴───────────────┘\n",
    );
    out.push_str(&format!(
        "\n{} short(s), {} clearance violation(s).\n",
        report.short_count(),
        report.clearance_violations().count(),
    ));
    out
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
