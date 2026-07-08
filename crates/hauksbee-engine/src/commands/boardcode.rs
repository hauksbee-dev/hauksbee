//! The Board-as-Code subcommands: `to-code` decompiles any text board into the
//! editable `.board` source, `from-code` recompiles that source back to a KiCad
//! PCB (with optional placement / routing), and `check-code` recompiles then runs
//! the stress co-sim. This is the CLI glue over the `boardcode` module; the
//! round-trip logic itself lives there.

use std::path::Path;

use crate::boardcode::{
    check_code, decompile_any_to_code, load_code, render_check_report, CheckOptions,
};
use crate::commands::common::read_board_text;

/// `hauksbee to-code <board-file> [--out <file.board>]`
///
/// Accepts any text board the extractor understands. A `.kicad_pcb` keeps the
/// cluster-aware geometry decompiler; a KiCad `.net` / IPC-D-356 / Eagle `.brd`
/// / `.kicad_sch` is extracted and emitted flat (so a layout-free netlist also
/// becomes editable Board-as-Code).
pub fn to_code(board: &Path, out: Option<&Path>) -> anyhow::Result<()> {
    let text = read_board_text(board)?;
    let code = decompile_any_to_code(&text)?;
    match out {
        Some(p) => {
            std::fs::write(&p, &code)?;
            eprintln!("wrote {}", p.display());
        }
        None => print!("{code}"),
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
pub enum RouteMode {
    /// No routing (placement only) - the historical default.
    None,
    /// Hand off to freerouting; fall back to the grid A* if it is absent.
    Freerouting,
    /// Force the in-tree grid A* fallback.
    Grid,
}

/// `hauksbee from-code <code-file> [--out <file.kicad_pcb>] [--relayout|--incremental] [--route|--route-grid]`
pub fn from_code(
    code_path: &Path,
    out: Option<&Path>,
    relayout_flag: bool,
    incremental: bool,
    route_flag: bool,
    route_grid_flag: bool,
) -> anyhow::Result<()> {
    use forge_codegen::{relayout, LayoutConfig, Program};

    let layout: Option<LayoutConfig> = if relayout_flag {
        Some(LayoutConfig::full())
    } else if incremental {
        Some(LayoutConfig::incremental())
    } else {
        None
    };
    let route = if route_flag {
        RouteMode::Freerouting
    } else if route_grid_flag {
        RouteMode::Grid
    } else {
        RouteMode::None
    };

    let code = load_code(code_path)?;

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
        route_board(&mut pcb, &prog, route, out)?;
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
pub fn check(code_path: &Path, seconds: f64, destructive: bool, ambient: f64) -> anyhow::Result<()> {
    let opts = CheckOptions {
        seconds: seconds,
        destructive: destructive,
        ambient_c: ambient,
    };
    let code = load_code(code_path)?;
    let report = check_code(&code, &opts)?;
    print!("{}", render_check_report(&report));
    if !report.healthy() {
        std::process::exit(1);
    }
    Ok(())
}
