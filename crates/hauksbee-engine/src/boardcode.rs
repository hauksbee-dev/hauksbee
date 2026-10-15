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
use crate::reports::truncate;
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
    /// What a clean result rests on without evidence: a defaulted ambient that
    /// decides a temperature-dependent rating, a Tj,max taken from the package.
    pub assumptions: Vec<hauksbee_ir::evidence::Assumption>,
}

impl CheckReport {
    /// A one-line health verdict for scripts: any fault (rating violation)
    /// means the circuit is not healthy, regardless of destructive mode.
    #[must_use]
    pub const fn healthy(&self) -> bool {
        self.faults.is_empty()
    }

    /// What the assumptions, and every part simulated open, leave the run
    /// entitled to claim, in the evidence maps' wording.
    #[must_use]
    pub fn status(&self) -> hauksbee_ir::evidence::EvidenceStatus {
        use hauksbee_ir::evidence::{Assumption, EvidenceMap, RunDate};
        let open = (self.unresolved.iter()).map(|(r, v)| Assumption::open_part(r, v, ""));
        let on_path: Vec<Assumption> = self.assumptions.iter().cloned().chain(open).collect();
        EvidenceMap::derive_status(&on_path, RunDate::from_system_clock())
    }
}

/// Options for [`check_code`].
pub struct CheckOptions {
    pub seconds: f64,
    /// Run the stress monitor in destructive mode (parts can be destroyed).
    pub destructive: bool,
    /// Ambient temperature (C) for the steady-state junction-temperature
    /// estimate and temperature-dependent ratings. `None` runs at
    /// [`crate::thermal::DEFAULT_AMBIENT_C`] (25 C), disclosed where it decides a rating.
    pub ambient_c: Option<f64>,
    /// `--models-dir`: the highest layer over the builtin and user dirs, as `run` loads it.
    pub models_dir: Option<std::path::PathBuf>,
}

impl Default for CheckOptions {
    fn default() -> Self {
        Self {
            seconds: 0.2,
            destructive: false,
            ambient_c: None,
            models_dir: None,
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
                layer: "F.Cu".to_string(),
                properties: vec![(
                    hauksbee_extract::altium::REFERENCE_AMBIGUOUS_KEY.to_string(),
                    "inferred reference without source UID".to_string(),
                )],
                ..Default::default()
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
                layer: "F.Cu".to_string(),
                dnp: true,
                ..Default::default()
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
    /// back still classified Present, while `DnpAbsent` and `IdentityUnknown` can
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
        let r4 = board.component("R4").expect("R4 survives the round trip");
        assert!(
            hauksbee_extract::assembly::AssemblyState::of(r4).is_present(),
            "a Present part must come back Present, with no DNP flag or refusal invented"
        );
    }
}

/// Recompile Board-as-Code, bind it, run a headless co-sim with the stress
/// monitor, and return a [`CheckReport`].
///
/// # Errors
///
/// Returns an error if the code does not recompile to board text, or for any
/// reason [`check_board_text`] fails.
pub fn check_code(code: &str, opts: &CheckOptions) -> anyhow::Result<CheckReport> {
    let board_text = code_to_board_text(code)?;
    check_board_text(&board_text, opts)
}

/// Same as [`check_code`] but starting from `.kicad_pcb` text directly (used by
/// the original-vs-edited comparison and tests).
///
/// # Errors
///
/// Returns an error if the text is not a parseable board, if the board has no
/// components, or if the co-simulation engine cannot be built from it.
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
    // The live session's, `run`'s and hauksbee-ci's library (user dirs too): the bare
    // builtin swapped a user card for a fallback and passed a part the app faulted.
    let extra: Vec<&std::path::Path> = opts.models_dir.as_deref().into_iter().collect();
    let lib = ModelLibrary::builtin_with_user_dirs(&extra);
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

    // The binding caveats (fallback models, guessed pins, defaults) CI and the app
    // carry, from their builder, so check-code cannot call clean what they qualify.
    let gaps = bound.power_coverage_gaps();
    let today = hauksbee_ir::evidence::RunDate::from_system_clock();
    let evidence = crate::evidence::BoardEvidence::from_bound(&board, &bound.report, &gaps, today)?;
    // A supply-named rail nothing powers sits at 0 V, so "no faults" around it
    // is not evidence: the engine carries the census as the app's note does.
    let mut engine = HauksbeeEngine::from_board_and_bound(&board, bound, None, "/boards/check")?;
    if opts.destructive {
        let mut controls = engine.controls();
        controls.destructive_faults = true;
        engine.set_controls(controls);
    }
    match opts.ambient_c {
        Some(ambient) => engine.scheduler_mut().set_ambient_c(ambient),
        None => engine.scheduler_mut().stress.ambient_c = crate::thermal::DEFAULT_AMBIENT_C,
    }

    // Headless co-sim, collecting faults each frame.
    let frame_dt = 1.0 / 1000.0;
    let mut t = 0.0;
    let mut faults: Vec<FaultEvent> = Vec::new();
    #[allow(clippy::while_float, reason = "t steps in the engine's f64 frames")]
    while t < opts.seconds {
        let frame = engine.step(frame_dt);
        faults.extend(
            frame
                .faults
                .into_iter()
                .map(crate::reports::cosim::fault_event),
        );
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
        // Open parts stay out: `unresolved` names them and `status` counts them.
        assumptions: (evidence.with_assumptions(sched.stress.disclosures())?)
            .assumptions()
            .iter()
            .filter(|a| a.kind() != hauksbee_ir::evidence::AssumptionKind::OpenPart)
            .cloned()
            .collect(),
    })
}

/// Render a [`CheckReport`] as a terminal table.
#[must_use]
pub fn render_check_report(r: &CheckReport) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "Board-as-Code check: {}", r.board_name);
    // FLOOR the percentage: `{:.0}` rounds a fraction in [0.995, 1.0) up to "100",
    // so a board with an unresolved (→ silently OPEN in sim) component read as
    // "100% resolved". Only a true 1.0 shows 100; anything short floors, so a
    // partial board can never masquerade as fully covered.
    #[allow(clippy::cast_possible_truncation, reason = "floored into 0..100")]
    #[allow(clippy::cast_sign_loss, reason = "non-negative; NaN reads as 0")]
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
        // Only a clean run may claim "within ratings"; an undermined one found nothing.
        let headline = match r.status() {
            hauksbee_ir::evidence::EvidenceStatus::Clean => "circuit is within ratings.",
            hauksbee_ir::evidence::EvidenceStatus::Qualified => "within ratings, qualified:",
            hauksbee_ir::evidence::EvidenceStatus::Undermined => "none found, but NOT a pass:",
        };
        let _ = writeln!(s, "  no faults: {headline}");
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
                truncate(&f.component, 26),
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
    // The conditions a verdict rests on, faulted or not, as the live notes show them.
    for a in &r.assumptions {
        let _ = writeln!(s, "    qualified: {} {}", a.statement(), a.consequence());
    }
    s
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
            assumptions: Vec::new(),
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
    fn healthy_requires_no_faults_regardless_of_destroyed_flag() {
        // No faults => healthy.
        assert!(report(1.0).healthy(), "clean board is healthy");

        // A non-destructive fault (the default mode) must still fail health.
        let mut r = report(1.0);
        r.faults.push(FaultEvent {
            component: "Q7".into(),
            kind: crate::stress::FaultKind::Overcurrent,
            value: 2.0,
            limit: 1.0,
            t: 0.005,
            destroyed: false,
        });
        assert!(!r.healthy(), "a non-destroyed fault must fail health");

        // A destructive fault also fails (backward compat).
        r.faults[0].destroyed = true;
        assert!(!r.healthy(), "a destroyed fault must fail health");
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
        assert_eq!(r.status().as_str(), "undermined", "open parts: {s}");
    }
}
