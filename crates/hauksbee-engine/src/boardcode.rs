//! The Board-as-Code edit -> simulate loop.
//!
//! This module closes the loop between the executable board DSL in
//! `forge-codegen` and hauksbee's co-simulation. The flow:
//!
//! ```text
//! .kicad_pcb ──forge_codegen::to_code──▶ .board (editable text)
//!      │                                       │  (a human or AI edits it)
//!      ▼                                       ▼
//!   (original)                       forge_codegen::Program::parse
//!                                              │
//!                                     Program::build ──▶ Pcb ──▶ .kicad_pcb text
//!                                              │
//!                              hauksbee_extract::ExtractedBoard::from_kicad_pcb
//!                                              │
//!                                          bind_board
//!                                              │
//!                                  headless co-sim + StressMonitor
//!                                              │
//!                                          CheckReport
//! ```
//!
//! [`decompile_board_to_code`] is the CODE side: a board becomes editable text.
//! [`code_to_board_text`] is the recompile: text becomes a valid `.kicad_pcb`.
//! [`check_code`] runs the whole simulate loop on a code directory/file and
//! returns a [`CheckReport`] (bind health + the faults the stress monitor
//! raised), which the CLI renders.

use std::path::Path;

use forge_codegen::dsl::{Comp, Pad, Stmt};
use forge_codegen::{to_code, Program};
use forge_model::Pcb;
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;
use hauksbee_server::engine::Engine;

use crate::binder::bind_board;
use crate::engine::HauksbeeEngine;
use crate::stress::FaultEvent;

/// Decompile a parsed-or-raw `.kicad_pcb` text into editable Board-as-Code.
pub fn decompile_board_to_code(kicad_pcb_text: &str) -> anyhow::Result<String> {
    let pcb = Pcb::parse(kicad_pcb_text).map_err(|e| anyhow::anyhow!("parsing board: {e:?}"))?;
    Ok(to_code(&pcb))
}

/// Decompile *any* extracted board text into editable Board-as-Code.
///
/// `decompile_board_to_code` only accepts a `.kicad_pcb` (it goes straight
/// through `Pcb::parse`, which rejects a `.net`/IPC/Eagle file). This bridge
/// handles every text format the extractor understands: a `.kicad_pcb` keeps
/// the geometry-rich layout path (so blocks/clusters and pad geometry survive),
/// and everything else (KiCad `.net`, IPC-D-356, Eagle `.brd`, KiCad `.sch`)
/// goes through [`ExtractedBoard::from_auto`] + [`program_from_extracted`].
///
/// A `.net` carries pin *functions* (`pinfunction "A"`/`"K"`) that a layout
/// lacks. The Board-as-Code `pad` line has no role slot (it is a geometry +
/// net record), so the function is not re-emitted verbatim; what survives is
/// the pad *number*, which the binder's pin-role rule table maps back to a role
/// (Feature 2). For a netlist with a generic diode the standard `1->K, 2->A`
/// numbering binds the part directly, with a guess-warning naming the rule.
pub fn decompile_any_to_code(board_text: &str) -> anyhow::Result<String> {
    let head: String = board_text.chars().take(512).collect();
    if head.contains("(kicad_pcb") {
        // Layout: keep the cluster-aware geometry decompiler.
        return decompile_board_to_code(board_text);
    }
    // Netlist / IPC / Eagle / schematic: extract, then emit flat code.
    let board = ExtractedBoard::from_auto(board_text)?;
    Ok(program_from_extracted(&board).emit())
}

/// Build an editable Board-as-Code [`Program`] directly from an
/// [`ExtractedBoard`].
///
/// This is the bridge that lets an extracted board (KiCad netlist, IPC-356,
/// Eagle) become editable code without a `.kicad_pcb` round-trip. Net ids are
/// resolved to names; each component becomes a singleton [`Stmt::Single`] so the
/// program is flat and every pad-net is explicit and editable. Pad geometry the
/// extractor does not carry is filled with sane SMD defaults (connectivity, not
/// geometry, is what matters for the simulate loop).
pub fn program_from_extracted(board: &ExtractedBoard) -> Program {
    let mut id_to_name = std::collections::HashMap::new();
    for n in &board.nets {
        id_to_name.insert(n.id, n.name.clone());
    }

    let mut body: Vec<Stmt> = Vec::new();
    // Declare nets in board order for a stable table.
    for n in &board.nets {
        if !n.name.is_empty() {
            body.push(Stmt::Net(n.name.clone()));
        }
    }

    for c in &board.components {
        let (x, y, rot) = c.position.unwrap_or((0.0, 0.0, 0.0));
        let pads = c
            .pins
            .iter()
            .map(|p| {
                let net = p
                    .net
                    .and_then(|id| id_to_name.get(&id).cloned())
                    .filter(|s| !s.is_empty());
                let at = p.position.unwrap_or((0.0, 0.0));
                Pad {
                    number: p.number.clone(),
                    kind: "smd".to_string(),
                    shape: "rect".to_string(),
                    at,
                    size: (1.0, 1.0),
                    drill: None,
                    layers: vec!["F.Cu".to_string()],
                    net,
                }
            })
            .collect();
        body.push(Stmt::Single(Comp {
            reference: c.reference.clone(),
            // Board-as-Code recompiles to a `.kicad_pcb`, where a component's
            // identity is its FOOTPRINT (the extractor sets `footprint = lib_id`
            // of the footprint block). A netlist carries both a symbol lib_id
            // ("Device:D") and a footprint ("Diode_SMD:D_SOD-323"); the footprint
            // is what survives the round-trip and what the binder + pin-rule
            // table match on, so prefer it. Fall back to the symbol lib_id only
            // when no footprint is known.
            lib_id: if !c.footprint.is_empty() {
                c.footprint.clone()
            } else {
                c.lib_id.clone()
            },
            value: c.value.clone(),
            layer: if c.layer.is_empty() {
                "F.Cu".to_string()
            } else {
                c.layer.clone()
            },
            at: (x, y),
            rot,
            space: None,
            pads,
        }));
    }

    Program {
        version: 20241229,
        blocks: Vec::new(),
        body,
        outline: None,
    }
}

/// Recompile Board-as-Code text into a `.kicad_pcb` string.
///
/// Parses the DSL, interprets it into a [`Pcb`], and emits KiCad s-expression
/// text. Errors carry the offending line number from the DSL parser.
pub fn code_to_board_text(code: &str) -> anyhow::Result<String> {
    let prog = Program::parse(code).map_err(|e| anyhow::anyhow!("board code: {e}"))?;
    let pcb = prog.build();
    Ok(pcb.emit())
}

/// Load Board-as-Code from a path that is either a `.board` file or a directory
/// containing exactly one `.board` file.
pub fn load_code(path: &Path) -> anyhow::Result<String> {
    if path.is_dir() {
        let mut found = None;
        for entry in std::fs::read_dir(path)? {
            let p = entry?.path();
            if p.extension().map(|e| e == "board").unwrap_or(false) {
                if found.is_some() {
                    anyhow::bail!(
                        "{} contains more than one .board file; pass the file directly",
                        path.display()
                    );
                }
                found = Some(p);
            }
        }
        let f =
            found.ok_or_else(|| anyhow::anyhow!("no .board file found in {}", path.display()))?;
        Ok(std::fs::read_to_string(f)?)
    } else {
        std::fs::read_to_string(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!(
                    "no board-as-code file at '{}'. Check the path, or try a bundled example:\n  \
                     hauksbee check-code examples/board-as-code/blinky.board",
                    path.display()
                )
            } else {
                anyhow::anyhow!("reading '{}': {e}", path.display())
            }
        })
    }
}

/// The outcome of a [`check_code`] run.
#[derive(Debug, Clone)]
pub struct CheckReport {
    /// Board name (from the rebuilt board).
    pub board_name: String,
    pub component_count: usize,
    pub net_count: usize,
    /// Fraction of components the binder resolved to a model (0..1).
    pub resolved_fraction: f64,
    /// How many seconds of co-sim were run.
    pub simulated_seconds: f64,
    /// All faults the stress monitor raised during the run, de-duplicated by
    /// `(component, kind)` keeping the worst value.
    pub faults: Vec<FaultEvent>,
    /// Number of nets that toggled during the run (activity sanity).
    pub active_nets: usize,
    /// The non-ignored components the binder could NOT resolve to a model, as
    /// `(reference, value)`. These are simulated as OPEN, so any firmware/analog
    /// result on their nets is incomplete, naming them makes "N% resolved"
    /// actionable instead of a bare number.
    pub unresolved: Vec<(String, String)>,
}

impl CheckReport {
    /// A one-line health verdict for scripts.
    pub fn healthy(&self) -> bool {
        self.faults.iter().all(|f| !f.destroyed)
    }
}

/// Options for [`check_code`].
pub struct CheckOptions {
    pub seconds: f64,
    /// Run the stress monitor in destructive mode (parts can be destroyed).
    pub destructive: bool,
    /// Ambient temperature (C) for the steady-state junction-temperature
    /// estimate. Defaults to [`crate::thermal::DEFAULT_AMBIENT_C`] (25 C).
    pub ambient_c: f64,
}

impl Default for CheckOptions {
    fn default() -> Self {
        CheckOptions {
            seconds: 0.2,
            destructive: false,
            ambient_c: crate::thermal::DEFAULT_AMBIENT_C,
        }
    }
}

/// Recompile Board-as-Code, bind it, run a headless co-sim with the stress
/// monitor, and return a [`CheckReport`].
pub fn check_code(code: &str, opts: &CheckOptions) -> anyhow::Result<CheckReport> {
    let board_text = code_to_board_text(code)?;
    check_board_text(&board_text, opts)
}

/// Same as [`check_code`] but starting from `.kicad_pcb` text directly (used by
/// the original-vs-edited comparison and tests).
pub fn check_board_text(board_text: &str, opts: &CheckOptions) -> anyhow::Result<CheckReport> {
    let board = ExtractedBoard::from_auto(board_text)?;
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);

    let board_name = bound.name.clone();
    let component_count = board.components.len();
    let net_count = bound.net_names.len();
    let resolved_fraction = bound.report.resolved_fraction();
    // Capture WHICH parts are unresolved before `bound` is consumed by the engine,
    // so the report can name them (they simulate as OPEN).
    let unresolved: Vec<(String, String)> = bound
        .report
        .non_ignored()
        .filter(|r| !r.outcome.is_resolved())
        .map(|r| (r.reference.clone(), r.value.clone()))
        .collect();

    let mut engine = HauksbeeEngine::from_bound(bound, None, "/boards/check")?;
    if opts.destructive {
        let mut controls = engine.controls();
        controls.destructive_faults = true;
        engine.set_controls(controls);
    }
    engine.scheduler_mut().set_ambient_c(opts.ambient_c);

    // Headless co-sim, collecting faults each frame.
    let frame_dt = 1.0 / 1000.0;
    let mut t = 0.0;
    let mut faults: Vec<FaultEvent> = Vec::new();
    while t < opts.seconds {
        let frame = engine.step(frame_dt);
        for f in frame.faults {
            faults.push(FaultEvent {
                component: f.component,
                kind: crate::stress::FaultKind::from_str(&f.kind),
                value: f.value,
                limit: f.limit,
                t: f.t,
                destroyed: f.destroyed,
            });
        }
        t += frame_dt;
    }

    let sched = engine.scheduler();
    let active_nets = sched.stats.values().filter(|s| s.toggles > 0).count();
    let simulated_seconds = sched.sim_time;

    // De-duplicate faults by (component, kind), keeping the worst value.
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

    Ok(CheckReport {
        board_name,
        component_count,
        net_count,
        resolved_fraction,
        simulated_seconds,
        faults,
        active_nets,
        unresolved,
    })
}

/// Render a [`CheckReport`] as a terminal table.
pub fn render_check_report(r: &CheckReport) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "Board-as-Code check: {}", r.board_name);
    // FLOOR the percentage: `{:.0}` rounds a fraction in [0.995, 1.0) up to "100",
    // so a board with an unresolved (→ silently OPEN in sim) component read as
    // "100% resolved". Only a true 1.0 shows 100; anything short floors, so a
    // partial board can never masquerade as fully covered.
    let pct = if r.resolved_fraction >= 1.0 {
        100
    } else {
        (r.resolved_fraction * 100.0).floor() as u32
    };
    // Zero active nets is the CORRECT answer for this surface (it runs no
    // firmware, so nothing toggles), but a bare "0 active nets" sitting above
    // "no faults" reads as a failure to a first-time user. Say why.
    let active = if r.active_nets == 0 {
        "0 active nets (nothing toggles without firmware; `hauksbee run <board> \
         --firmware <f> --headless` exercises it)"
            .to_string()
    } else {
        format!("{} active nets", r.active_nets)
    };
    let _ = writeln!(
        s,
        "  {} components, {} nets, {pct}% resolved, {active}",
        r.component_count, r.net_count,
    );
    // Name the unresolved parts so "{pct}% resolved" is actionable: these bind to
    // no model and simulate as OPEN, so any firmware/analog result on their nets
    // is incomplete. Add models with --models-dir to cover them.
    if !r.unresolved.is_empty() {
        let _ = writeln!(
            s,
            "  {} unresolved (simulated as OPEN; add models with --models-dir):",
            r.unresolved.len()
        );
        for (reference, value) in &r.unresolved {
            let val = if value.trim().is_empty() {
                String::new()
            } else {
                format!(" ({value})")
            };
            let _ = writeln!(s, "    - {reference}{val}");
        }
    }
    let _ = writeln!(s, "  simulated {:.3}s", r.simulated_seconds);
    if r.faults.is_empty() {
        let _ = writeln!(s, "  no faults: circuit is within ratings.");
    } else {
        let _ = writeln!(s, "  {} fault(s):", r.faults.len());
        let _ = writeln!(
            s,
            "┌────────────────────────────┬──────────────┬────────────┬────────────┬───────┐"
        );
        let _ = writeln!(
            s,
            "│ Component                  │ Fault        │ Value      │ Limit      │ Dead  │"
        );
        let _ = writeln!(
            s,
            "├────────────────────────────┼──────────────┼────────────┼────────────┼───────┤"
        );
        for f in &r.faults {
            let _ = writeln!(
                s,
                "│ {:<26} │ {:<12} │ {:>10.4} │ {:>10.4} │ {:<5} │",
                trunc(&f.component, 26),
                f.kind.as_str(),
                f.value,
                f.limit,
                if f.destroyed { "yes" } else { "no" },
            );
        }
        let _ = writeln!(
            s,
            "└────────────────────────────┴──────────────┴────────────┴────────────┴───────┘"
        );
    }
    s
}

fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    fn report(fraction: f64) -> CheckReport {
        CheckReport {
            board_name: "b".into(),
            component_count: 250,
            net_count: 40,
            resolved_fraction: fraction,
            simulated_seconds: 0.1,
            faults: Vec::new(),
            active_nets: 5,
            unresolved: Vec::new(),
        }
    }

    #[test]
    fn partial_resolution_never_rounds_up_to_100_percent() {
        // R44: `{:.0}` rounds a fraction in [0.995, 1.0) up to "100", so a board
        // with one unresolved (→ silently OPEN) component read "100% resolved". A
        // sub-1.0 fraction must floor, so only a true 1.0 shows 100%.
        let s = render_check_report(&report(249.0 / 250.0)); // 0.996
        assert!(
            s.contains("99% resolved") && !s.contains("100% resolved"),
            "0.996 must show 99%, not a rounded 100%: {s}"
        );
        // A genuinely complete board still shows 100%.
        let full = render_check_report(&report(1.0));
        assert!(full.contains("100% resolved"), "1.0 must show 100%: {full}");
    }

    #[test]
    fn unresolved_parts_are_named_not_just_counted() {
        // U1: "84% resolved" was a bare number; the check must NAME the parts it
        // could not model (they simulate as OPEN), so the user knows what to add.
        let mut r = report(0.5);
        r.unresolved = vec![
            ("U3".into(), "ATmega328P".into()),
            ("Q7".into(), "2N3906".into()),
        ];
        let s = render_check_report(&r);
        assert!(
            s.contains("U3") && s.contains("ATmega328P"),
            "names the MCU: {s}"
        );
        assert!(
            s.contains("Q7") && s.contains("2N3906"),
            "names the transistor: {s}"
        );
        assert!(
            s.contains("simulated as OPEN"),
            "explains the consequence: {s}"
        );
    }
}
