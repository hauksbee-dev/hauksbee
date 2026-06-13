//! `hauksbee` CLI: bind a board and bring it to life.
//!
//! ```text
//! hauksbee run        <board-file> [--firmware <hex>] [--seconds N] [--headless]
//!                                 [--port 3001] [--report] [--drc] [--lint] [--si]
//!                                 [--resources] [--apply-shorts]
//! hauksbee to-code    <board-file> [--out <file.board>]
//! hauksbee from-code  <code-file>  [--out <file.kicad_pcb>]
//! hauksbee check-code <code-dir|file> [--seconds N] [--destructive]
//! ```
//!
//! - `run --report`       : print the bind report table and exit.
//! - `run --drc`          : print the geometric short / clearance report and exit.
//! - `run --apply-shorts` : apply every detected copper short (bridge the nets)
//!                          before simulating, so the run shows the consequences.
//! - `run --headless`     : run the co-sim for `--seconds` and print summary stats.
//! - `run` (default)      : serve the live websocket (frontend/dist if present).
//! - `to-code`            : decompile a board into editable Board-as-Code text.
//! - `from-code`          : recompile Board-as-Code back into a `.kicad_pcb`.
//! - `check-code`         : recompile, bind, co-sim with the stress monitor, and
//!                          print a fault report (the edit -> simulate loop).

use std::path::{Path, PathBuf};

use hauksbee_engine::binder::bind_board;
use hauksbee_engine::boardcode::{
    check_code, decompile_board_to_code, load_code, render_check_report,
    CheckOptions,
};
use hauksbee_engine::HauksbeeEngine;
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;
use hauksbee_server::Server;

struct Args {
    board: PathBuf,
    firmware: Option<PathBuf>,
    seconds: f64,
    headless: bool,
    report_only: bool,
    drc_only: bool,
    lint_only: bool,
    si_only: bool,
    resources_only: bool,
    apply_shorts: bool,
    port: u16,
    /// Extra user model directory (highest priority), layered over the built-in
    /// DB and the default `~/.hauksbee/models` / `~/.config/hauksbee/models` dirs.
    models_dir: Option<PathBuf>,
}

fn parse_run_args(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let board = it.next().map(PathBuf::from).ok_or_else(usage)?;
    let mut args = Args {
        board,
        firmware: None,
        seconds: 1.0,
        headless: false,
        report_only: false,
        drc_only: false,
        lint_only: false,
        si_only: false,
        resources_only: false,
        apply_shorts: false,
        port: 3001,
        models_dir: None,
    };
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--models-dir" => {
                args.models_dir = Some(PathBuf::from(it.next().ok_or("--models-dir needs a path")?))
            }
            "--firmware" => {
                args.firmware = Some(PathBuf::from(it.next().ok_or("--firmware needs a path")?))
            }
            "--seconds" => {
                args.seconds = it
                    .next()
                    .ok_or("--seconds needs a number")?
                    .parse()
                    .map_err(|_| "bad --seconds")?
            }
            "--headless" => args.headless = true,
            "--report" => args.report_only = true,
            "--drc" => args.drc_only = true,
            "--lint" => args.lint_only = true,
            "--si" => args.si_only = true,
            "--resources" => args.resources_only = true,
            "--apply-shorts" => args.apply_shorts = true,
            "--port" => {
                args.port = it
                    .next()
                    .ok_or("--port needs a number")?
                    .parse()
                    .map_err(|_| "bad --port")?
            }
            other => return Err(format!("unknown flag '{other}'\n{}", usage())),
        }
    }
    Ok(args)
}

fn usage() -> String {
    "usage:\n  \
     hauksbee run <board-file> [--firmware <hex>] [--seconds N] [--headless] [--port 3001] [--report] [--drc] [--lint] [--si] [--resources] [--apply-shorts] [--models-dir <dir>]\n  \
     hauksbee to-code <board-file> [--out <file.board>]\n  \
     hauksbee from-code <code-file> [--out <file.kicad_pcb>] [--relayout|--incremental] [--route|--route-grid]\n  \
     hauksbee check-code <code-dir|file> [--seconds N] [--destructive]"
        .to_string()
}

fn main() -> anyhow::Result<()> {
    let mut it = std::env::args().skip(1);
    let cmd = match it.next() {
        Some(c) => c,
        None => {
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    };
    let result = match cmd.as_str() {
        "run" => cmd_run(it),
        "to-code" => cmd_to_code(it),
        "from-code" => cmd_from_code(it),
        "check-code" => cmd_check_code(it),
        other => {
            eprintln!("unknown command '{other}'\n{}", usage());
            std::process::exit(2);
        }
    };
    if let Err(e) = &result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
    result
}

fn cmd_run(it: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let args = match parse_run_args(it) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    // Read raw bytes first: an Altium `.PcbDoc` is a binary OLE2 container and
    // would fail a UTF-8 `read_to_string`. Text formats (KiCad / Eagle / IPC)
    // are recovered losslessly from these bytes.
    let raw = std::fs::read(&args.board)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", args.board.display()))?;
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

    if args.report_only {
        let bound = bind_board(&board, &lib);
        print!("{}", bound.report.render_table());
        return Ok(());
    }

    // --drc: run geometric short / clearance detection, print, exit.
    if args.drc_only {
        let report = if altium.is_some() {
            ExtractedBoard::altium_drc(&raw)?
        } else {
            ExtractedBoard::drc(&text)?
        };
        print!("{}", render_drc(&report));
        return Ok(());
    }

    // --lint: run the connectivity lint-class checks, the boot strap-pin lint
    // (which needs the model db's per-part strap tables), and the MCU internal
    // resource-conflict check (a lint-class structural check too), print, exit.
    if args.lint_only {
        let mut report = board.net_lint();
        let straps = hauksbee_engine::checks::straps::strap_lint(&board, &lib);
        report.findings.extend(straps.findings);
        report.findings.extend(board.resource_conflicts().findings);
        print!("{}", hauksbee_extract::render_netlint(&report));
        return Ok(());
    }

    // --resources: run only the MCU internal resource-conflict check, print, exit.
    if args.resources_only {
        let report = board.resource_conflicts();
        print!("{}", hauksbee_extract::render_netlint(&report));
        return Ok(());
    }

    // --si: run the signal-integrity / physics static checks, print, exit. The
    // geometry-bearing checks (antenna keepout, USB length skew) need the raw
    // KiCad layout text, so it is passed through.
    if args.si_only {
        // Altium geometry is not yet threaded into the SI text checks, so pass
        // None there; the connectivity-based SI checks still run on `board`.
        let geo_text = if altium.is_some() { None } else { Some(text.as_str()) };
        let report = board.si_checks(geo_text);
        print!("{}", hauksbee_extract::render_si(&report));
        return Ok(());
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

    if args.headless {
        run_headless(&mut engine, args.seconds);
        return Ok(());
    }

    serve(engine, args.port)
}

/// `hauksbee to-code <board-file> [--out <file.board>]`
fn cmd_to_code(mut it: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let board = it.next().ok_or_else(|| anyhow::anyhow!("{}", usage()))?;
    let mut out: Option<PathBuf> = None;
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--out" => out = Some(PathBuf::from(it.next().ok_or_else(|| anyhow::anyhow!("--out needs a path"))?)),
            other => anyhow::bail!("unknown flag '{other}'\n{}", usage()),
        }
    }
    let text = std::fs::read_to_string(&board)?;
    let code = decompile_board_to_code(&text)?;
    match out {
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
fn cmd_from_code(mut it: impl Iterator<Item = String>) -> anyhow::Result<()> {
    use forge_codegen::{relayout, LayoutConfig, Program};
    let code_path = it.next().ok_or_else(|| anyhow::anyhow!("{}", usage()))?;
    let mut out: Option<PathBuf> = None;
    let mut layout: Option<LayoutConfig> = None;
    let mut route = RouteMode::None;
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--out" => out = Some(PathBuf::from(it.next().ok_or_else(|| anyhow::anyhow!("--out needs a path"))?)),
            "--relayout" => layout = Some(LayoutConfig::full()),
            "--incremental" => layout = Some(LayoutConfig::incremental()),
            "--route" => route = RouteMode::Freerouting,
            "--route-grid" => route = RouteMode::Grid,
            other => anyhow::bail!("unknown flag '{other}'\n{}", usage()),
        }
    }
    let code = load_code(Path::new(&code_path))?;

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
        route_board(&mut pcb, &prog, route, out.as_deref())?;
    }

    let board_text = pcb.emit();
    match out {
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
fn cmd_check_code(mut it: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let code_path = it.next().ok_or_else(|| anyhow::anyhow!("{}", usage()))?;
    let mut opts = CheckOptions::default();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--seconds" => {
                opts.seconds = it
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--seconds needs a number"))?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --seconds"))?
            }
            "--destructive" => opts.destructive = true,
            other => anyhow::bail!("unknown flag '{other}'\n{}", usage()),
        }
    }
    let code = load_code(Path::new(&code_path))?;
    let report = check_code(&code, &opts)?;
    print!("{}", render_check_report(&report));
    if !report.healthy() {
        std::process::exit(1);
    }
    Ok(())
}

fn run_headless(engine: &mut HauksbeeEngine, seconds: f64) {
    use hauksbee_server::engine::Engine;
    eprintln!("co-sim: {seconds:.2}s headless...");
    let frame_dt = 1.0 / 1000.0; // 1 kHz frame cadence
    let mut t = 0.0;
    let mut last_uart: Vec<u8> = Vec::new();
    while t < seconds {
        let frame = engine.step(frame_dt);
        for bytes in frame.uart.values() {
            last_uart.extend_from_slice(bytes);
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
}

fn serve(engine: HauksbeeEngine, port: u16) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let server = Server::new(Box::new(engine));
        let static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../frontend/dist");
        let dir = static_dir.exists().then_some(static_dir);
        let addr = format!("127.0.0.1:{port}");
        server.serve(&addr, dir.as_deref()).await
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
