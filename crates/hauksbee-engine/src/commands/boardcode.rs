//! The Board-as-Code subcommands: `to-code` decompiles any text board into the
//! editable `.board` source, `from-code` recompiles that source back to a KiCad
//! PCB (with optional placement / routing), and `check-code` recompiles then runs
//! the stress co-sim. This is the CLI glue over the `boardcode` module; the
//! round-trip logic itself lives there.

use std::path::{Path, PathBuf};

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

/// Everything `from-code` can be asked to do beyond the bare recompile. Passed
/// as a struct because the flag surface (routing engine knobs, the DSN escape
/// hatch, machine output) outgrew a positional-argument list.
pub struct FromCodeOpts {
    /// Write the `.kicad_pcb` here (default: print to stdout).
    pub out: Option<PathBuf>,
    pub relayout: bool,
    pub incremental: bool,
    pub route: bool,
    pub route_grid: bool,
    pub route_strict: bool,
    /// Wall-clock budget for the freerouting child (`--route-timeout`).
    pub route_timeout_secs: u64,
    /// Freerouting optimisation passes (`--route-passes`, its `-mp`).
    pub route_passes: u32,
    /// Explicit jar (`--freerouting-jar`), overriding env + search.
    pub freerouting_jar: Option<PathBuf>,
    /// Write the DSN here and stop before routing (`--route-dsn`).
    pub route_dsn: Option<PathBuf>,
    /// Emit the routing result as one JSON object on stdout (`--json`).
    pub json: bool,
}

/// `hauksbee from-code <code-file> [--out <file.kicad_pcb>] [--relayout|--incremental] [--route|--route-grid|--route-dsn <f>] [--route-strict] [--json]`
pub fn from_code(code_path: &Path, opts: &FromCodeOpts) -> anyhow::Result<()> {
    use forge_codegen::{relayout, LayoutConfig, Program};

    let layout: Option<LayoutConfig> = if opts.relayout {
        Some(LayoutConfig::full())
    } else if opts.incremental {
        Some(LayoutConfig::incremental())
    } else {
        None
    };
    let route = if opts.route {
        RouteMode::Freerouting
    } else if opts.route_grid {
        RouteMode::Grid
    } else {
        RouteMode::None
    };
    if opts.json && route == RouteMode::None && opts.route_dsn.is_none() {
        anyhow::bail!(
            "from-code --json describes a routing run; add --route, --route-grid, or --route-dsn \
             (for the analysis reports use `hauksbee run <board> --json`)"
        );
    }

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

    // The DSN escape hatch: write the router's input and stop before routing.
    // The operator routes it with whatever router on whatever clock, then
    // `hauksbee merge-ses` brings the SES back through the same audit.
    if let Some(dsn_path) = &opts.route_dsn {
        let dsn_text =
            forge_codegen::write_dsn(&pcb, prog.outline, &forge_codegen::RouteRules::default());
        std::fs::write(dsn_path, &dsn_text)?;
        eprintln!("wrote routing DSN to {}", dsn_path.display());
        eprintln!(
            "route it with any Specctra router (e.g. java -jar freerouting.jar -de {} -do board.ses),\n\
             then merge the result: hauksbee merge-ses {} board.ses --out <board.kicad_pcb>",
            dsn_path.display(),
            code_path.display()
        );
        if opts.json {
            println!(
                "{}",
                serde_json::json!({ "ok": true, "dsn": dsn_path.display().to_string() })
            );
        }
    }

    let route_stats = if route != RouteMode::None {
        let stats = route_board(&mut pcb, &prog, route, opts)?;
        // On a strict failure this prints the JSON (ok:false) and exits, so a
        // board that failed the gate is never written.
        finish_routed(&stats, opts.route_strict, opts.json)?;
        Some(stats)
    } else {
        None
    };

    let board_text = pcb.emit();
    match &opts.out {
        Some(p) => {
            std::fs::write(p, &board_text)?;
            eprintln!("wrote {}", p.display());
        }
        None => print!("{board_text}"),
    }
    // The success JSON is emitted after the board is safely on disk, so
    // `ok:true` always means "the routed board file exists".
    if opts.json {
        if let Some(stats) = &route_stats {
            println!("{}", route_json(stats, true, None));
        }
    }
    Ok(())
}

/// `hauksbee merge-ses <code> <ses> [--out <file.kicad_pcb>] [--route-strict] [--json]`
///
/// Recompile the board from its Board-as-Code source (deterministic placement,
/// so pads land exactly where the exported DSN put them), merge a user-supplied
/// SES onto it, and run the SAME post-merge audit the built-in route path runs.
pub fn merge_ses(
    code_path: &Path,
    ses_path: &Path,
    out: Option<&Path>,
    route_strict: bool,
    json: bool,
) -> anyhow::Result<()> {
    use forge_codegen::Program;

    let started = std::time::Instant::now();
    let code = load_code(code_path)?;
    let prog = Program::parse(&code).map_err(|e| anyhow::anyhow!("board code: {e}"))?;
    let mut pcb = prog.build();

    let ses_text = std::fs::read_to_string(ses_path)
        .map_err(|e| anyhow::anyhow!("reading SES '{}': {e}", ses_path.display()))?;
    let rules = forge_codegen::RouteRules::default();
    let (segments, vias) = forge_codegen::merge_ses_text(&mut pcb, &ses_text, &rules);
    if segments == 0 && vias == 0 {
        // A wrong file (or a SES whose net names match nothing) merging to zero
        // copper is a mistake, not a routed board; refuse rather than emit an
        // unrouted board that claims to be the routed one.
        anyhow::bail!(
            "no copper merged from {}: the SES carries no wires/vias matching this board's nets \
             (was it produced from this board's DSN?)",
            ses_path.display()
        );
    }
    eprintln!(
        "merged {} segments, {} vias from {} (merged-ses)",
        segments,
        vias,
        ses_path.display()
    );

    let audit = post_merge_audit(&pcb, "merged-ses", false)?;
    let stats = RouteStats {
        engine: "merged-ses".to_string(),
        seconds: started.elapsed().as_secs_f64(),
        segments,
        vias,
        nets_total: nets_needing_route(&pcb),
        audit,
    };
    finish_routed(&stats, route_strict, json)?;

    let board_text = pcb.emit();
    match out {
        Some(p) => {
            std::fs::write(p, &board_text)?;
            eprintln!("wrote {}", p.display());
        }
        None => print!("{board_text}"),
    }
    if json {
        println!("{}", route_json(&stats, true, None));
    }
    Ok(())
}

/// The audit numbers a routed (or merged) board is judged by. One source for
/// the human lines, the strict gate, and the `--json` object, so the three
/// surfaces can never disagree.
struct RouteAudit {
    conn: forge_codegen::Connectivity,
    drc_serious: usize,
    /// On an unvalidated (KiCad 10+) format the short counts are unreliable
    /// and the strict gate must not fire on them.
    drc_reliable: bool,
    endpoint_violations: usize,
}

/// Everything one routing run produced, for reporting.
struct RouteStats {
    /// What produced the copper: "freerouting-1.9.0" (the jar stem), "grid"
    /// for the in-tree A*, "merged-ses" for a user-supplied SES.
    engine: String,
    seconds: f64,
    segments: usize,
    vias: usize,
    /// Nets with >=2 pads (the ones that needed routing at all).
    nets_total: usize,
    audit: RouteAudit,
}

/// Nets with at least two connected pads: the denominators a routing report
/// speaks in.
fn nets_needing_route(pcb: &forge_model::Pcb) -> usize {
    let mut net_pads: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for fp in pcb.footprints() {
        for pad in fp.pads() {
            if let Some((_, n)) = pad.net() {
                if !n.is_empty() {
                    *net_pads.entry(n).or_default() += 1;
                }
            }
        }
    }
    net_pads.values().filter(|&&c| c >= 2).count()
}

/// Route a built board in place. Prefers freerouting; documents and falls back
/// to the grid A* when freerouting is unavailable (or when explicitly forced).
/// After whichever router merges copper back, the honest post-merge audit runs;
/// strict gating and JSON emission happen in [`finish_routed`] on the returned
/// stats.
fn route_board(
    pcb: &mut forge_model::Pcb,
    prog: &forge_codegen::Program,
    mode: RouteMode,
    opts: &FromCodeOpts,
) -> anyhow::Result<RouteStats> {
    use forge_codegen::{route_grid, FreeroutingConfig, RouteRules};

    let rules = RouteRules::default();
    let fr_cfg = FreeroutingConfig {
        jar: opts.freerouting_jar.clone(),
        max_passes: opts.route_passes,
        timeout: std::time::Duration::from_secs(opts.route_timeout_secs),
        ..FreeroutingConfig::default()
    };

    let use_grid = match mode {
        RouteMode::Grid => true,
        RouteMode::Freerouting => !forge_codegen::freerouting_available(&fr_cfg),
        RouteMode::None => anyhow::bail!("route_board called with RouteMode::None"),
    };

    if !use_grid {
        // Freerouting handoff (the production path).
        let workdir = opts
            .out
            .as_deref()
            .and_then(|p| p.parent())
            .map(|d| d.join("freerouting-work"))
            .unwrap_or_else(|| std::env::temp_dir().join("hauksbee-freerouting"));
        // Announce the DSN up front: it is the hand-off artifact an operator
        // wants when this run fails or times out (route it themselves, then
        // `hauksbee merge-ses`), so it is never written silently.
        eprintln!(
            "routing: freerouting handoff (DSN -> freerouting -> SES); DSN at {}",
            workdir.join(forge_codegen::DSN_FILE_NAME).display()
        );
        match forge_codegen::route_with_freerouting(pcb, prog.outline, &rules, &fr_cfg, &workdir) {
            Ok(o) => {
                eprintln!(
                    "merged {} segments, {} vias in {:.1}s ({})",
                    o.segments, o.vias, o.elapsed_secs, o.engine
                );
                let audit = post_merge_audit(pcb, &o.engine, false)?;
                return Ok(RouteStats {
                    engine: o.engine,
                    seconds: o.elapsed_secs,
                    segments: o.segments,
                    vias: o.vias,
                    nets_total: o.nets_to_route,
                    audit,
                });
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
    let started = std::time::Instant::now();
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
    let audit = post_merge_audit(pcb, "grid A* fallback", true)?;
    Ok(RouteStats {
        engine: "grid".to_string(),
        seconds: started.elapsed().as_secs_f64(),
        segments: seg,
        vias: 0,
        nets_total: nets_needing_route(pcb),
        audit,
    })
}

/// Post-merge honesty audit shared by every merge path (freerouting, the grid
/// fallback, and `merge-ses`). Reports the real routed connections (rat-lines
/// closed under the merged copper, not "nets with a wire"), runs the internal
/// DRC on the produced board, and counts endpoints that terminate in a
/// wrong-net pad. Pure measurement: the strict gate lives in [`finish_routed`].
fn post_merge_audit(
    pcb: &forge_model::Pcb,
    router: &str,
    is_grid: bool,
) -> anyhow::Result<RouteAudit> {
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

    Ok(RouteAudit {
        conn,
        drc_serious: serious,
        drc_reliable: reliable,
        endpoint_violations: endpoint_viol,
    })
}

/// The strict gate over a finished route. On a violation with `--json`, the
/// one JSON object (ok:false + error + the full metrics) is printed and the
/// process exits 1 directly, so stdout carries exactly one parseable line
/// instead of a metrics object followed by a second error envelope.
fn finish_routed(stats: &RouteStats, route_strict: bool, json: bool) -> anyhow::Result<()> {
    if !route_strict {
        return Ok(());
    }
    let audit = &stats.audit;
    let mut reasons = Vec::new();
    if audit.conn.unrouted > 0 {
        reasons.push(format!("{} unrouted connections", audit.conn.unrouted));
    }
    if audit.drc_reliable && audit.drc_serious > 0 {
        reasons.push(format!("{} serious DRC violations", audit.drc_serious));
    }
    if audit.endpoint_violations > 0 {
        reasons.push(format!(
            "{} endpoint-net violations",
            audit.endpoint_violations
        ));
    }
    if reasons.is_empty() {
        return Ok(());
    }
    let msg = format!("route-strict: {}", reasons.join(", "));
    if json {
        println!("{}", route_json(stats, false, Some(&msg)));
        std::process::exit(1);
    }
    anyhow::bail!(msg);
}

/// The one machine-readable routing object (docs schema: from-code/merge-ses
/// `--json`). Keys are stable; `ok:false` carries an `error` string.
fn route_json(stats: &RouteStats, ok: bool, error: Option<&str>) -> String {
    let mut obj = serde_json::json!({
        "ok": ok,
        "nets_total": stats.nets_total,
        "connections_routed": stats.audit.conn.routed,
        "unrouted": stats.audit.conn.unrouted,
        "segments": stats.segments,
        "vias": stats.vias,
        "seconds": stats.seconds,
        "engine": stats.engine,
        "drc_serious": stats.audit.drc_serious,
        "endpoint_net_violations": stats.audit.endpoint_violations,
    });
    if let Some(e) = error {
        obj["error"] = serde_json::Value::String(e.to_string());
    }
    obj.to_string()
}

/// `hauksbee check-code <code-dir|file> [--seconds N] [--destructive] [--json]`
pub fn check(
    code_path: &Path,
    seconds: f64,
    destructive: bool,
    ambient: f64,
    json: bool,
) -> anyhow::Result<()> {
    let opts = CheckOptions {
        seconds,
        destructive,
        ambient_c: ambient,
    };
    let code = load_code(code_path)?;
    let report = check_code(&code, &opts)?;
    if json {
        println!("{}", check_json(&report));
    } else {
        print!("{}", render_check_report(&report));
    }
    if !report.healthy() {
        std::process::exit(1);
    }
    Ok(())
}

/// The `check-code --json` object: the whole [`crate::boardcode::CheckReport`]
/// as one machine-readable line. `ok` mirrors the exit code (false when a part
/// was destroyed); `resolved_fraction` stays the raw fraction so a consumer
/// never re-derives it from a rounded percentage.
fn check_json(r: &crate::boardcode::CheckReport) -> String {
    serde_json::json!({
        "ok": r.healthy(),
        "board": r.board_name,
        "components": r.component_count,
        "nets": r.net_count,
        "resolved_fraction": r.resolved_fraction,
        "unresolved": r
            .unresolved
            .iter()
            .map(|(reference, value)| serde_json::json!({ "ref": reference, "value": value }))
            .collect::<Vec<_>>(),
        "simulated_seconds": r.simulated_seconds,
        "active_nets": r.active_nets,
        "faults": r
            .faults
            .iter()
            .map(|f| {
                serde_json::json!({
                    "component": f.component,
                    "kind": f.kind.as_str(),
                    "value": f.value,
                    "limit": f.limit,
                    "t": f.t,
                    "destroyed": f.destroyed,
                })
            })
            .collect::<Vec<_>>(),
    })
    .to_string()
}
