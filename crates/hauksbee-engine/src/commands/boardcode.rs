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

/// `hauksbee from-code <code-file> [--out <file.kicad_pcb>] [--relayout|--incremental] [--route|--route-grid] [--route-strict]`
pub fn from_code(
    code_path: &Path,
    out: Option<&Path>,
    relayout_flag: bool,
    incremental: bool,
    route_flag: bool,
    route_grid_flag: bool,
    route_strict: bool,
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
        route_board(&mut pcb, &prog, route, out, route_strict)?;
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
/// After whichever router merges copper back, an honest post-merge audit runs
/// (connections routed, an automatic internal DRC, and an endpoint-net
/// assertion); with `route_strict` an open connection or a serious DRC finding
/// makes the command exit non-zero.
fn route_board(
    pcb: &mut forge_model::Pcb,
    prog: &forge_codegen::Program,
    mode: RouteMode,
    out: Option<&Path>,
    route_strict: bool,
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
                eprintln!(
                    "merged {} segments, {} vias in {:.1}s (freerouting)",
                    o.segments, o.vias, o.elapsed_secs
                );
                return post_merge_audit(pcb, "freerouting", false, route_strict);
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
    eprintln!("merged {} segments (grid A* fallback)", seg);
    post_merge_audit(pcb, "grid A* fallback", true, route_strict)
}

/// Post-merge honesty audit shared by both routers. Reports the real routed
/// connections (rat-lines closed under the merged copper, not "nets with a
/// wire"), runs the internal DRC on the produced board, and counts endpoints
/// that terminate in a wrong-net pad. Under `route_strict`, any open connection,
/// serious (short) DRC finding, or endpoint-net violation returns an error so
/// the CLI exits non-zero.
fn post_merge_audit(
    pcb: &forge_model::Pcb,
    router: &str,
    is_grid: bool,
    route_strict: bool,
) -> anyhow::Result<()> {
    let conn = forge_codegen::connectivity(pcb);
    let endpoint_viol = forge_codegen::endpoint_net_violations(pcb);

    // Run the same internal DRC as `hauksbee run --drc` on the produced board.
    let board_text = pcb.emit();
    let drc = hauksbee_extract::ExtractedBoard::drc(&board_text)?;
    let serious = drc.short_count();
    let total = drc.findings.len();
    // On an unvalidated (KiCad 10+) format the shorts are unreliable; do not
    // gate strict on them (matches the DRC surface's own caveat).
    let reliable = drc.version_warning.is_none();

    eprintln!(
        "routed: {}/{} connections, {} unrouted ({router}); endpoint-net violations: {endpoint_viol}",
        conn.routed, conn.total, conn.unrouted
    );
    eprintln!("DRC: {serious} serious, {total} total");
    if let Some(w) = &drc.version_warning {
        eprintln!("DRC note: {w}");
    }
    // BUG 4: the grid A* fallback has no clearance model, so a "completed" run
    // can still be riddled with shorts. Say so plainly.
    if is_grid && reliable && serious > 0 {
        eprintln!(
            "grid A* completed with {serious} clearance violations; this board needs freerouting"
        );
    }

    if route_strict {
        let mut reasons = Vec::new();
        if conn.unrouted > 0 {
            reasons.push(format!("{} unrouted connections", conn.unrouted));
        }
        if reliable && serious > 0 {
            reasons.push(format!("{serious} serious DRC violations"));
        }
        if endpoint_viol > 0 {
            reasons.push(format!("{endpoint_viol} endpoint-net violations"));
        }
        if !reasons.is_empty() {
            anyhow::bail!("route-strict: {}", reasons.join(", "));
        }
    }
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
