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

use hauksbee_extract::ExtractedBoard;
use hauksbee_frontdoor_api::engine::Engine;
use hauksbee_models::ModelLibrary;

use crate::binder::bind_board;
use crate::engine::HauksbeeEngine;
use crate::stress::FaultEvent;

pub use hauksbee_bind::boardcode::{
    code_to_board_text, decompile_any_to_code, decompile_board_to_code, load_code,
    program_from_extracted,
};

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

#[cfg(test)]
mod identity_tests {
    use super::*;
    use hauksbee_extract::Component;

    #[test]
    fn boardcode_refuses_identity_evidence_it_cannot_serialize() {
        let board = ExtractedBoard {
            name: "ambiguous".to_string(),
            nets: Vec::new(),
            components: vec![Component {
                reference: "R1".to_string(),
                value: "1k".to_string(),
                lib_id: "Device:R".to_string(),
                footprint: "R_0603".to_string(),
                position: None,
                layer: "F.Cu".to_string(),
                properties: vec![(
                    hauksbee_extract::altium::REFERENCE_AMBIGUOUS_KEY.to_string(),
                    "inferred reference without source UID".to_string(),
                )],
                dnp: false,
                pins: Vec::new(),
            }],
        };

        let error = program_from_extracted(&board)
            .expect_err("the current DSL has no field that can preserve identity refusal")
            .to_string();
        assert!(
            error.contains("R1"),
            "the refusal must name the component: {error}"
        );
        assert!(
            error.contains("ambiguous"),
            "the refusal must explain the lost evidence: {error}"
        );
    }

    #[test]
    fn boardcode_refuses_dnp_state_it_cannot_serialize() {
        let board = ExtractedBoard {
            name: "variant".to_string(),
            nets: Vec::new(),
            components: vec![Component {
                reference: "R2".to_string(),
                value: "0R".to_string(),
                lib_id: "Device:R".to_string(),
                footprint: "R_0603".to_string(),
                position: None,
                layer: "F.Cu".to_string(),
                properties: Vec::new(),
                dnp: true,
                pins: Vec::new(),
            }],
        };

        let error = program_from_extracted(&board)
            .expect_err("DNP would be recompiled as a fitted zero-ohm link")
            .to_string();
        assert!(error.contains("R2") && error.contains("DNP"), "{error}");
    }

    #[test]
    fn native_kicad_decompile_cannot_bypass_the_dnp_refusal() {
        let source = r#"(kicad_pcb (version 20240108)
  (net 0 "")
  (net 1 "N1")
  (footprint "R_0603" (layer "F.Cu")
    (attr smd exclude_from_bom dnp)
    (property "Reference" "R3")
    (property "Value" "0R")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "N1"))
  )
)"#;
        let error = decompile_board_to_code(source)
            .expect_err("the geometry-rich path must not turn a DNP link into fitted code")
            .to_string();
        assert!(error.contains("R3") && error.contains("DNP"), "{error}");
    }

    /// The other side of the round-trip contract: with the two refusals above,
    /// "all three states survive" means a Present part passes through code and
    /// back still classified Present, while DnpAbsent and IdentityUnknown can
    /// only leave as loud errors, never as a silently fitted part.
    #[test]
    fn a_present_part_round_trips_and_stays_present() {
        let source = r#"(kicad_pcb (version 20240108)
  (net 0 "")
  (net 1 "N1")
  (footprint "Resistor_SMD:R_0603" (layer "F.Cu")
    (attr smd)
    (property "Reference" "R4")
    (property "Value" "10k")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "N1"))
  )
)"#;
        let code = decompile_board_to_code(source).expect("a fitted part decompiles");
        let rebuilt = code_to_board_text(&code).expect("the code recompiles");
        let board = ExtractedBoard::from_kicad_pcb(&rebuilt).expect("recompiled board extracts");
        let r4 = board
            .components
            .iter()
            .find(|c| c.reference == "R4")
            .expect("R4 survives the round trip");
        assert!(
            hauksbee_extract::assembly::AssemblyState::of(r4).is_present(),
            "a Present part must come back Present, with no DNP flag or refusal invented"
        );
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
    // A zero-component board can prove nothing: every stress check passes
    // vacuously, and "100% resolved, no faults" on an empty .board is false
    // comfort. Refuse loudly instead.
    if board.components.is_empty() {
        anyhow::bail!(
            "this board has no components; nothing to check, so a pass would be \
             meaningless"
        );
    }
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
            "  {} unresolved (simulated as OPEN; add models with --models-dir, see hauksbee models --help):",
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
