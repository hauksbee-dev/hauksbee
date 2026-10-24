//! `--check` / `--all`: the whole static suite (bind + DRC + lint + SI + USB-C)
//! in ONE report, plus the bare-`--json` combined report (bind + DRC + lint + SI
//! + USB-C) that a `run <board> --json` with no specific selector emits.

use hauksbee_ir::{eprintln, print, println};
use std::path::Path;

use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

use crate::binder::bind_board;
use crate::result::{
    lint_findings_json, si_findings_json, usbc_finding_json, BindSummary, DrcStructured,
    JsonReport, RunVerdict,
};

use super::{lint_fails, si_fails, KicadClearanceInput, OutputMode, StaticInputs};

/// Run the full static suite and print it in `mode`, then (under `strict`) exit
/// non-zero if any real finding gates. `quiet` suppresses the text surface and
/// `oracle` cross-checks against kicad-cli.
#[allow(clippy::fn_params_excessive_bools, reason = "independent CLI switches")]
pub(crate) fn emit_with_schematic_quiet(
    input: &StaticInputs,
    mode: OutputMode,
    strict: bool,
    verbose: bool,
    oracle: bool,
    quiet: bool,
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
) -> anyhow::Result<()> {
    CheckReport {
        input,
        schematic_ties,
    }
    .emit(
        &CheckDisplay::Full {
            mode,
            verbose,
            oracle,
            quiet,
            defer: false,
        },
        strict,
    )
    .map(|_| ())
}

/// [`emit_with_schematic_quiet`] for a caller that closes the run itself: the
/// whole report prints, but its closing `VERDICT:` line and its `--strict`
/// exits are left to the caller (see [`CheckOutcome`]), so a run that goes on
/// to judge the board some other way can end on ONE verdict covering both.
#[allow(clippy::fn_params_excessive_bools, reason = "independent CLI switches")]
pub(crate) fn emit_deferred_with_schematic_quiet(
    input: &StaticInputs,
    mode: OutputMode,
    strict: bool,
    verbose: bool,
    oracle: bool,
    quiet: bool,
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
) -> anyhow::Result<CheckOutcome> {
    CheckReport {
        input,
        schematic_ties,
    }
    .emit(
        &CheckDisplay::Full {
            mode,
            verbose,
            oracle,
            quiet,
            defer: true,
        },
        strict,
    )?
    .ok_or_else(|| anyhow::anyhow!("a deferred check report always leaves its outcome"))
}

/// What a deferred `--check` pass leaves for the caller that closes the run:
/// its machine verdict, the counts its closing line reads, and the `--strict`
/// exits it has not yet taken.
pub(crate) struct CheckOutcome {
    /// The verdict the `--json` document of this pass states.
    pub(crate) verdict: RunVerdict,
    /// `(serious, worth a look)`, as the closing line counts them.
    pub(crate) counts: (usize, usize),
    /// The copper caveat for a board format whose copper is unvalidated: why,
    /// and how many potential copper issues the copper section lists.
    copper_unvalidated: Option<(String, usize)>,
    strict: StrictTail,
}

impl CheckOutcome {
    /// The copper caveat, shaped as the closing line takes it.
    pub(crate) fn copper_unvalidated(&self) -> Option<CopperUnvalidated<'_>> {
        self.copper_unvalidated
            .as_ref()
            .map(|(reason, issues)| CopperUnvalidated {
                reason,
                issues: *issues,
            })
    }

    /// Take the `--strict` exits this pass owes, if any. `unvouched` is whether
    /// the run as a whole (not just this pass) is partly judged or inconclusive.
    pub(crate) fn finish(self, unvouched: bool) {
        self.strict.run(&self.verdict, unvouched);
    }
}

/// The exits a `--check` pass takes under `--strict`, held apart from the
/// report so a caller can take them after the rest of its output.
struct StrictTail {
    strict: bool,
    would_gate: bool,
    mode: OutputMode,
    gate_items: Vec<String>,
    blockers: Vec<String>,
}

impl StrictTail {
    /// Say why an exit 0 is not a pass, then exit 2 on a gating finding or 3
    /// on a verdict the run could not give, exactly as `--check` alone does.
    fn run(&self, verdict: &RunVerdict, unvouched: bool) {
        super::note_unvouched_exit(self.strict, unvouched);
        if self.strict && self.would_gate {
            super::strict_gate_exit(self.mode, &self.gate_items);
        }
        // Exit 3 on the SAME rule the JSON verdict uses (run-level undermined
        // evidence or unbound verdict-critical parts), not on any undermined map:
        // a finding-backed badge must not exit 3 under --strict while the verdict
        // field says pass.
        // A partly judged run is not a pass either: exit 3, annotated as such.
        // Every strict non-pass verdict is annotated with the verdict's own
        // reason, so the GitHub surface is red wherever JUnit, SARIF and the
        // exit code are, blockers or not.
        if self.strict && verdict.verdict == "invalid" {
            // Blockers are named in the shared INCONCLUSIVE sentence by the exit
            // below; a partly judged run, or one with no blocker to name, says
            // the verdict's own reason instead.
            if verdict.partial || self.blockers.is_empty() {
                super::ci_artifacts::github_verdict_annotation(
                    verdict.reason.as_deref().unwrap_or_default(),
                );
            }
            super::exit_invalid_for_analysis(&self.blockers);
        }
    }
}

/// The words after a failing verdict that still exits 0, in the glossary's
/// terms, so a CI log reader who sees "failing" beside a green job learns why
/// and what to add.
pub(crate) fn report_only_suffix() -> String {
    format!(
        " (exit 0: report only. {})",
        hauksbee_ir::glossary::Term::ReportOnlyExit
            .entry()
            .what_to_do
    )
}

// Both entrypoints run the same static suite over one input bundle, so the
// report pipeline is written once.
struct CheckReport<'a> {
    input: &'a StaticInputs<'a>,
    schematic_ties: Option<&'a crate::schematic_ties::SchematicTies>,
}

enum CheckDisplay {
    Full {
        mode: OutputMode,
        verbose: bool,
        oracle: bool,
        quiet: bool,
        /// Leave the closing line and the strict exits to the caller.
        defer: bool,
    },
    CombinedJson,
}

impl CheckReport<'_> {
    #[allow(clippy::too_many_lines, reason = "one static-suite pass, then render")]
    fn emit(self, display: &CheckDisplay, strict: bool) -> anyhow::Result<Option<CheckOutcome>> {
        let Self {
            input,
            schematic_ties,
        } = self;
        let StaticInputs {
            board_path,
            board,
            text,
            altium_present,
            lib,
            ..
        } = *input;
        let combined = matches!(display, CheckDisplay::CombinedJson);
        let (mode, verbose, oracle, quiet, defer) = match *display {
            CheckDisplay::Full {
                mode,
                verbose,
                oracle,
                quiet,
                defer,
            } => (mode, verbose, oracle, quiet, defer),
            CheckDisplay::CombinedJson => (OutputMode::Json, false, false, false, false),
        };
        hauksbee_ir::progress::begin_bind(board.components.len(), 1);
        let mut bound = bind_board(board, lib);
        let summary = BindSummary::from_report(&bound.report);
        // The lint judges the board at every assumed corner of a class-only
        // part; the evidence then says the result holds across them.
        hauksbee_ir::progress::begin_lint();
        let (mut lint, corner_plan) = crate::checks::engine_lint_swept(board, lib);
        if let Some(plan) = &corner_plan {
            plan.mark(&mut bound.report);
        }
        // The layout checks run here, still part of the nets stage, as they do
        // on the web path: the bar counts them with the lint, not with the
        // copper sweep that follows. They read the layout text alone, so the
        // findings are the same wherever they are gathered.
        lint.findings
            .extend(crate::checks::layout_lint(board, input.geo_text(), Some(board_path)).findings);
        lint.findings
            .extend(super::firmware_intent::findings(board, lib, input.firmware));
        let evidence = input.evidence(&bound.report)?;
        hauksbee_ir::progress::begin_copper();
        let (clearance_input, mut drc) = input.drc()?;
        let evidence = evidence.with_custom_rules_coverage(
            clearance_input
                .as_ref()
                .and_then(KicadClearanceInput::custom_coverage),
        )?;

        // The companion schematic's declarations, applied before anything reads the
        // findings. Reclassifies, never deletes: a covered contact keeps its layer,
        // location and measured gap and gains the declaration that qualifies it.
        let qualification = schematic_ties.map(|ties| ties.qualify(&drc));
        let evidence =
            super::with_schematic_evidence(evidence, schematic_ties, qualification.as_ref())?;
        // Route through the single SI chokepoint so this aggregate surface carries
        // the same trace-ampacity + input-cap-ripple findings as the dedicated
        // `--si` report (the bare `si_checks` omitted them, giving `--check` a false
        // "looks healthy" over an under-width power trace).
        hauksbee_ir::progress::begin_untracked(hauksbee_ir::progress::Stage::Signals);
        let mut si = crate::checks::engine_si(board, lib, input.geo_text());
        // USB-C CC compliance, only when the board has a USB-C receptacle.
        let usbc = crate::usb_c_report(board);
        hauksbee_ir::progress::begin(hauksbee_ir::progress::Stage::Report, 0);

        // Waivers: overrule a finding this board's owner judged wrong, without
        // switching the check off for everyone. Findings an active waiver covers
        // come out of the gate and go into their own section, never out of sight.
        let mut waivers = load_waivers(board_path);
        let waived = apply_static_waivers(&mut waivers, &mut lint, &mut si, &mut drc);
        let provenance = super::clearance_provenance(
            clearance_input.as_ref(),
            board_path,
            text,
            drc.clearance_mm,
        );
        let provenance = super::drc::without_unread_rule(provenance, input.input_kind);
        let mut drc_structured = DrcStructured::from_report_with_ties(
            &drc,
            qualification.as_ref(),
            text.contains("<eagle") && schematic_ties.is_none(),
        )
        .with_clearance_rule_provenance(&provenance);
        let oracle_summary = (oracle && mode != OutputMode::Json).then(|| {
            super::drc::oracle_cross_check_and_annotate(
                board_path,
                &drc,
                &mut drc_structured,
                qualification.as_ref(),
            )
        });
        let mut actual_findings = lint_findings_json(&lint);
        actual_findings.extend(si_findings_json(&si));
        actual_findings.extend(usbc.as_ref().and_then(usbc_finding_json));
        let mut evidence_maps =
            evidence.maps_for_drc_with_ties(&drc_structured, qualification.as_ref())?;
        evidence_maps.extend(evidence.maps_for_findings(&actual_findings)?);
        for (check, assertion) in [
            ("drc", "DRC input coverage"),
            ("si", "Signal-integrity input coverage"),
        ] {
            let coverage = evidence.check_coverage_map(check, assertion)?;
            if coverage.status() != hauksbee_ir::evidence::EvidenceStatus::Clean {
                evidence_maps.push(coverage);
            }
        }
        let evidence = evidence.with_maps(evidence_maps);

        // Zero routed copper: a pads-only board passes the spacing check
        // vacuously (there is nothing to space). Say so prominently instead of
        // letting "no copper spacing problems" read as "the routing is clean" on
        // a board that has no routing yet. Preserve the bare-JSON schema, which
        // has no unrouted note; only the explicit full check adds it.
        let unrouted = !combined && !altium_present && super::unrouted_kicad_layout(text);
        // The verdict blockers: unmodelled current-carrying / active parts. The
        // lint/SI sections and the closing verdict must read INCONCLUSIVE over
        // them, never a clean bill; the copper (DRC) section reads the layout and
        // is deliberately exempt. Without --strict the exit code is unchanged;
        // under it these blockers exit 3, matching the verdict field.
        let blockers = crate::result::unmodelled_critical_refs(&summary);
        let usbc_reliable = super::usb_c::scoped_blockers(board, &blockers).is_empty();
        let mut triage = crate::plain::order_triage_with_rule_source(
            &drc_structured,
            &provenance,
            &lint,
            &si,
            usbc.as_ref(),
            usbc_reliable,
            summary.unresolved,
            true,
        );
        // No outcome is stated on this surface, so what an indicator MEANS was
        // not judged; say so instead of letting a clean triage imply it was.
        let indicators = crate::checks::indicators::indicator_traces(board, lib);
        triage.add_not_covered(crate::checks::indicators::not_judged_note(&indicators));
        triage.add_not_covered(crate::plain::values_not_carried_note(board));
        // This surface's own exit gate, computed before anything renders so the
        // machine document can state the same outcome the exit code will. It is
        // wider than the `serious` severity on purpose (medium lint findings, any
        // SI finding), which is why the verdict has to be told about it. An
        // An unvalidated board format (KiCad 10+) yields possibly-phantom shorts
        // that do not gate unless KiCad's own DRC confirms the exact net pair; the
        // unmatched caveat is printed instead.
        let would_gate = check_gate_fails(&drc_structured, &lint, &si, usbc.as_ref());
        // The machine document, assembled for EVERY output mode: its verdict is
        // the single derivation the plain triage headline and the closing
        // VERDICT line read, so no human surface can say something the `--json`
        // `verdict` field does not. Everything the verdict reads is attached
        // here, before it is computed.
        let [rails_entered, rails_held] =
            crate::checks::rail_basis::rail_conditions(board, lib, &bound);
        let mut jr = machine_report(
            &bound.name,
            &summary,
            &drc_structured,
            Some(&provenance),
            &actual_findings,
            would_gate,
        )
        .with_inputs(input.inputs)
        .with_evidence(&evidence)
        // A pass over hardware-fixed indicator meaning nobody stated rests on it.
        .with_condition(crate::checks::indicators::intent_condition(&indicators))
        // So does a pass on copper judged against default spacing rules.
        .with_condition(super::drc::default_rules_condition(
            &drc_structured,
            &provenance,
        ))
        // And one on spacing nobody measured, from an input with no copper geometry.
        .with_condition(super::drc::unmeasured_spacing_condition(input.input_kind))
        // A schematic passes its own checks and is still nothing to order.
        .with_unorderable_input(super::drc::unorderable_input(input.input_kind, None))
        // And on every rail held at its named voltage with nothing on the
        // board modelled to source it: from a connector, or from nothing seen.
        .with_condition(rails_entered)
        .with_condition(rails_held)
        .with_judgement_scope(crate::scope::JudgementScope::of(board, &bound, &blockers));
        // The verdict carries the triage's inspect count, so the closing
        // line's tag and the web headline read the same number.
        jr.inspect_items = triage.inspect.len();
        // The board's real clearance-rule provenance, the one the sections and
        // `--json` read, attached before anything counts against it.
        jr.clearance_rule_source = Some(provenance.clone());
        // `run_verdict` also tells the CI artifacts the verdict (see
        // `JsonReport::run_verdict`), so JUnit and SARIF can never be green
        // beside a verdict that is not a pass.
        let run_verdict = jr.run_verdict();
        // The headline counts are the machine document's own: `--json`'s
        // `serious_count` and `actionable_count`, not a second derivation. A
        // heads-up counts exactly when `--json` carries it as an actionable
        // finding (the SI impedance advisories do); the copper heads-ups
        // (suppressed class, unvalidated format, no copper) are notes the JSON
        // does not count, so the headline does not either.
        let (_, _, json_serious, json_actionable) = jr.verdict();
        let headline_counts = (json_serious, json_actionable.saturating_sub(json_serious));
        match mode {
            OutputMode::Json => {
                jr.triage = Some(triage);
                if unrouted {
                    jr.push_coverage_note(super::UNROUTED_COPPER_NOTE.to_string());
                }
                if !blockers.is_empty() {
                    jr.push_coverage_note(crate::result::inconclusive_verdict(&blockers));
                }
                // A green verdict that quietly dropped findings would be worse than
                // no waivers at all, so the machine surface carries them too.
                jr.waived = waived.iter().cloned().map(Into::into).collect();
                println!("{}", jr.to_json());
            }
            OutputMode::Plain => {
                print!("{}", triage.render(&run_verdict));
                // The unmodelled key parts are stated once. The triage block
                // already carries them when the run is not a pass: its headline
                // (named, with the next step) when its lists have nothing else
                // to lead with, or, for a failing run, the NOT JUDGED line after
                // the failures. Only then is the standalone banner and the
                // open-IC heads-up left out.
                let named = crate::result::refs_summary(&blockers);
                let named_in_triage = match run_verdict.verdict {
                    "invalid" => {
                        (run_verdict.partial
                            || (triage.do_not_order.is_empty() && triage.inspect.is_empty()))
                            && run_verdict
                                .reason
                                .as_deref()
                                .is_some_and(|reason| reason.contains(&named))
                    }
                    "fail" => run_verdict
                        .not_judged
                        .as_deref()
                        .is_some_and(|said| said.contains(&named)),
                    _ => false,
                };
                // The scope banner once, above every section; each model-dependent
                // section then points back at the verdict instead of repeating it.
                if !blockers.is_empty() && !named_in_triage {
                    println!("{}\n", crate::result::inconclusive_verdict(&blockers));
                }
                let drc_plain = crate::plain::plain_drc_structured_with_rule_source(
                    &drc_structured,
                    &provenance,
                );
                let mut lint_plain = crate::plain_netlint(&lint);
                lint_plain.unmodelled_critical.clone_from(&blockers);
                lint_plain.inconclusive_said_above = true;
                let mut si_plain = crate::plain_si(&si);
                si_plain.unmodelled_critical.clone_from(&blockers);
                si_plain.inconclusive_said_above = true;
                print!(
                    "{}",
                    crate::plain::fix_first(&drc_plain, &lint_plain, usbc.as_ref(), &si_plain)
                        .render()
                );
                // Bind-role honesty (Marco): the plain persona surface must not hide
                // that active ICs are unmodelled, otherwise `--check --plain` reads
                // "healthy" while firmware/analog/AC/thermal on their nets are
                // uncovered. Text/JSON/web all carry this; plain must too. Use the
                // SAME union the web/json personas do, unresolved active ICs PLUS
                // resolved-but-open active ICs, so all four personas agree.
                let open_refs = crate::result::coverage_open_active_refs(&summary);
                let open = open_refs.len();
                let unsaid = !named_in_triage || open_refs.iter().any(|r| !blockers.contains(r));
                if open > 0 && unsaid {
                    // No part count in the pointer: `critical_parts_total` counts
                    // critical actives, not bind-table rows, and a wrong count
                    // sends a first-time reader hunting for a table that does not
                    // exist (qc scenario 01 pins this).
                    println!(
                        "Heads-up: {open} active IC(s) are unresolved/open, so firmware/analog/AC/thermal \
                         results on their nets would be INCOMPLETE, but the copper checks below are \
                         unaffected. Add models with --models-dir (hauksbee models --help) to cover \
                         them (run --report for the bind table).\n"
                    );
                }
                println!("== {} ==", drc_structured.section_title(&provenance));
                if unrouted {
                    println!("{}", super::UNROUTED_COPPER_NOTE);
                }
                print!(
                    "{}",
                    crate::plain::render_drc_condensed_with_rule_source_and_unlock(
                        &drc_structured,
                        &provenance,
                        verbose,
                        false,
                    )
                );
                if let Some(summary) = &oracle_summary {
                    print!("{summary}");
                }
                println!(
                    "\n== Connectivity / lint (incl. MCU resource conflicts, boot strap pins) =="
                );
                print!("{}", lint_plain.render());
                println!("\n== Signal integrity ==");
                print!("{}", si_plain.render());
                if let Some(u) = &usbc {
                    println!("\n== USB-C CC compliance ==");
                    print!("{}", u.render_plain());
                }
                print!(
                    "{}",
                    what_this_pass_did_not_check(
                        summary.mcu_bound,
                        bound
                            .mcus
                            .iter()
                            .any(|mcu| backend_is_compiled(&mcu.backend)),
                        usbc.is_some(),
                        if defer {
                            Followed::BySimulation
                        } else {
                            Followed::ByNothing
                        },
                    )
                );
            }
            OutputMode::Text => {
                let mut display_report = bound.report;
                super::bind::mark_decode_only_rows(&mut display_report, &lint);
                print!(
                    "{}",
                    if verbose {
                        display_report.render_table()
                    } else {
                        display_report.render_table_compact()
                    }
                );
                print!("{}", summary.render_banner());
                // What this run did not cover, and the scope banner, once and
                // above every section: the default report states the same scope
                // `--plain` does, in its own words.
                if !triage.not_covered.is_empty() {
                    println!("NOT COVERED: {}", triage.not_covered);
                }
                // A failing run says what it did not judge in the verdict's own
                // words, on one line; the INCONCLUSIVE sentence is for a run
                // whose verdict is that it cannot vouch.
                if let Some(line) = run_verdict.not_judged_line() {
                    println!("{line}");
                } else if !blockers.is_empty() {
                    println!("{}", crate::result::inconclusive_verdict(&blockers));
                }
                println!("\n== {} ==", drc_structured.section_title(&provenance));
                if unrouted {
                    println!("{}", super::UNROUTED_COPPER_NOTE);
                }
                print!(
                    "{}",
                    drc_structured.render_with_clearance_rule_provenance(&provenance)
                );
                if let Some(summary) = &oracle_summary {
                    print!("{summary}");
                }
                println!(
                    "\n== Connectivity / lint (incl. MCU resource conflicts, boot strap pins) =="
                );
                // Verdict first in a model-dependent section that found nothing:
                // the extract body's "no findings." must sit under the refusal,
                // not above it. A section that lists findings claims no clean
                // bill, so it needs no pointer.
                if !blockers.is_empty() && lint.findings.is_empty() {
                    println!("{}", crate::plain::INCONCLUSIVE_SEE_TOP);
                }
                print!("{}", hauksbee_extract::render_netlint(&lint));
                println!("\n== Signal integrity ==");
                if !blockers.is_empty() && si.finding_count() == 0 {
                    println!("{}", crate::plain::INCONCLUSIVE_SEE_TOP);
                }
                print!("{}", hauksbee_extract::render_si(&si));
                if let Some(u) = &usbc {
                    println!("\n== USB-C CC compliance ==");
                    print!("{}", u.render());
                }
            }
        }
        let copper_unvalidated = drc_structured.version_warning.as_deref().map(|reason| {
            (
                reason.to_string(),
                crate::plain::plain_drc_structured_with_rule_source(&drc_structured, &provenance)
                    .potential_copper_issue_count(),
            )
        });
        let outcome = CheckOutcome {
            verdict: run_verdict,
            counts: headline_counts,
            copper_unvalidated,
            strict: StrictTail {
                strict,
                would_gate,
                mode,
                gate_items: gate_items(&drc_structured, &lint, &si, usbc.as_ref()),
                blockers,
            },
        };
        if !matches!(mode, OutputMode::Json) {
            print!(
                "{}",
                super::render_evidence_appendix(&evidence, quiet, verbose)
            );
            print!("{}", render_waivers(&waived, &waivers));
            // One verdict line to end on (U4), matching the web/TUI verdict shape:
            // the last thing `--check` prints answers "is my board ok" without
            // scrolling back through four sections. --json is unchanged (its
            // consumers read the structured findings). A caller that closes the
            // run itself prints its own, covering more than this pass.
            if !defer {
                // A failing verdict that still exits 0 says so on its own line.
                let report_only = if !strict && would_gate {
                    report_only_suffix()
                } else {
                    String::new()
                };
                println!(
                    "\n{}{report_only}",
                    verdict_line(
                        &outcome.verdict,
                        outcome.counts,
                        outcome.copper_unvalidated(),
                    )
                );
            }
        }
        if defer {
            return Ok(Some(outcome));
        }
        let unvouched = outcome.verdict.partial || outcome.verdict.verdict == "invalid";
        outcome.finish(unvouched);
        Ok(None)
    }
}

/// Whether the lint, SI or USB-C findings fail the `--check` gate.
fn findings_gate(
    lint: &hauksbee_extract::NetLintReport,
    si: &hauksbee_extract::SiReport,
    usbc: Option<&crate::checks::usb_c::UsbcReport>,
) -> bool {
    lint_fails(lint)
        || si_fails(si)
        || usbc.is_some_and(crate::checks::usb_c::UsbcReport::is_serious)
}

/// The `--check` exit gate over the structured copper result: a serious short,
/// or a lint, SI or USB-C finding that gates. Wider than the `serious` grade on
/// purpose (medium lint findings, any SI finding), which is why the machine
/// verdict has to be told about it. The web report gates on the same rule.
pub(crate) fn check_gate_fails(
    drc: &DrcStructured,
    lint: &hauksbee_extract::NetLintReport,
    si: &hauksbee_extract::SiReport,
    usbc: Option<&crate::checks::usb_c::UsbcReport>,
) -> bool {
    drc.shorts.iter().any(|short| short.severity == "serious") || findings_gate(lint, si, usbc)
}

/// The machine document for a `--check` run, minus its inputs and evidence:
/// everything `JsonReport::verdict` reads about the findings, the bind summary
/// and this surface's own strict gate. One constructor for the JSON output, for
/// the verdict the plain triage and the closing line carry, and for the web
/// report's verdict.
///
/// `findings` is the complete list the gate reads: every copper short and
/// clearance group comes first, with its location, layer and pad owners, so
/// SARIF, `JUnit` and annotations carry what `--strict` fails on.
pub(crate) fn machine_report(
    board_name: &str,
    summary: &BindSummary,
    drc: &DrcStructured,
    provenance: Option<&crate::result::ClearanceRuleProvenance>,
    findings: &[crate::result::JsonFinding],
    would_gate: bool,
) -> JsonReport {
    let mut jr = JsonReport::new(board_name, summary.clone())
        .with_bind_verdict_gate()
        .with_surface_gate(would_gate);
    jr.drc = Some(drc.clone());
    let mut all = super::drc::drc_findings(drc, provenance);
    all.extend_from_slice(findings);
    jr.findings = Some(all);
    jr
}

/// An UNVALIDATED copper result on the closing line: why it is unvalidated and
/// how many potential copper issues the copper section itself lists. The count
/// comes from the same plain report that section prints, never from the
/// all-checks "worth a look" total, which also counts lint and signal-integrity
/// findings.
#[derive(Clone, Copy)]
pub(crate) struct CopperUnvalidated<'a> {
    pub reason: &'a str,
    pub issues: usize,
}

/// The single closing verdict line for `--check` (U4).
///
/// Its tag comes from the shared [`RunVerdict`] (the one `--json` prints), never
/// from the counts on the line: "clean" is only claimable when the machine
/// verdict is a pass, `failing` when it is `fail` (a serious finding OR a
/// finding this surface's strict gate fails on) and `inconclusive` when it could
/// not be judged. Either way the line ends on the shared
/// [`crate::result::VerdictCause::reason`] sentence, the same one `--json`, the CI renderers and
/// the web report carry, including its note that key parts were not modelled.
pub(crate) fn verdict_line(
    verdict: &RunVerdict,
    (serious, worth_a_look): (usize, usize),
    copper_unvalidated: Option<CopperUnvalidated<'_>>,
) -> String {
    let Some(reason) = verdict.reason.as_deref() else {
        // A schematic that passes is not "clean" to order: no board exists
        // yet. The tag says so, and the sentence that says why is the lead
        // of the conditions.
        let unorderable = verdict.unorderable.as_deref();
        let conditions = verdict.conditions.join(" ");
        if let (Some(why), None) = (unorderable, &copper_unvalidated) {
            let rest = verdict.conditions_without(why).join(" ");
            let also = if rest.is_empty() {
                String::new()
            } else {
                format!(" Also: {rest}")
            };
            return format!(
                "VERDICT: {}: 0 serious, {worth_a_look} worth a look; {why}{also}",
                hauksbee_ir::vocabulary::NOT_ORDERABLE_TAG
            );
        }
        if let Some(CopperUnvalidated { reason, issues }) = copper_unvalidated {
            let on = if conditions.is_empty() {
                String::new()
            } else {
                format!(" Conditions: {conditions}")
            };
            return format!(
                "VERDICT: copper UNVALIDATED: {}; unmatched copper findings remain tool-only \
                 and none counted as serious. Reason: {reason}{on}",
                crate::plain::potential_copper_issues(issues)
            );
        }
        // A pass with items the triage lists to inspect is not clean: the tag
        // says what the triage bucket says (the verdict carries its count, as
        // the web headline reads it), so the two never disagree.
        let tag = verdict.inspect_tag().unwrap_or_else(|| "clean".to_string());
        // A pass that rests on an assumption says so on the line itself.
        return if conditions.is_empty() {
            format!("VERDICT: {tag}: 0 serious, {worth_a_look} worth a look")
        } else {
            format!(
                "VERDICT: {tag}, conditionally: 0 serious, {worth_a_look} worth a look; {conditions}"
            )
        };
    };
    // A partly judged reason opens with the same lead as the tag, so the line
    // says it once: the tag carries it and the reason continues from there.
    let lead = hauksbee_ir::vocabulary::PARTLY_JUDGED_LEAD;
    let reason = verdict
        .partial
        .then(|| reason.strip_prefix(lead).and_then(|r| r.strip_prefix(": ")))
        .flatten()
        .unwrap_or(reason);
    let tag = if verdict.partial {
        lead
    } else if verdict.verdict == "invalid" {
        "inconclusive"
    } else {
        "failing"
    };
    // The copper caveat is added to the verdict, never in place of it, so this
    // line never hides the reason `--json` gives.
    let copper =
        copper_unvalidated.map_or_else(String::new, |CopperUnvalidated { reason, .. }| {
            format!(
                " Copper from this board format is UNVALIDATED: unmatched copper findings remain \
             tool-only and none counted as serious unless KiCad's own DRC confirms the exact net \
             pair. Reason: {reason}"
            )
        });
    format!("VERDICT: {tag}: {serious} serious, {worth_a_look} worth a look; {reason}{copper}")
}

/// Every gating subject across the aggregate suite, in report order
/// (DRC shorts, lint, SI, USB-C), for the `--strict` failure line.
fn gate_items(
    drc: &DrcStructured,
    lint: &hauksbee_extract::NetLintReport,
    si: &hauksbee_extract::SiReport,
    usbc: Option<&crate::checks::usb_c::UsbcReport>,
) -> Vec<String> {
    let mut items = super::drc_structured_gate_items(drc);
    items.extend(super::lint_gate_items(lint));
    items.extend(super::si_gate_items(si));
    if let Some(u) = usbc.filter(|u| u.is_serious()) {
        items.push(format!("usb_c_cc {}", u.headline));
    }
    items
}

/// The full static suite (DRC shorts + lint + SI + USB-C) as DATA.
///
/// For the `--junit`/`--sarif` artifact writers: the same checks and the same
/// waiver discipline as `emit_with_schematic_quiet`, without rendering a report.
/// Waived findings are excluded here exactly as they are excluded from the gate,
/// so a SARIF error can never fail a pipeline the report command would pass.
///
/// # Errors
///
/// Returns an error if the sibling `KiCad` project/custom clearance rules cannot
/// be loaded, or if the DRC cannot run over the board (`KiCad` or Altium).
pub fn gather_findings_with_schematic(
    board_path: &Path,
    board: &ExtractedBoard,
    text: &str,
    raw: &[u8],
    altium_present: bool,
    lib: &ModelLibrary,
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
) -> anyhow::Result<Vec<crate::result::JsonFinding>> {
    gather_static(
        board_path,
        board,
        text,
        raw,
        altium_present,
        lib,
        schematic_ties,
    )
    .map(|g| g.kept)
}

/// The static suite's findings after the board's waivers, and the findings
/// those waivers overruled.
struct GatheredStatic {
    kept: Vec<crate::result::JsonFinding>,
    /// Each finding a waiver excused, with that waiver's "reason (until date)".
    waived: Vec<(crate::result::JsonFinding, String)>,
}

/// One finding's identity for telling the waived ones apart: every field a
/// finding is reported by.
fn finding_key(f: &crate::result::JsonFinding) -> (&str, &str, &str, &str, &[String], &[String]) {
    (&f.check, &f.kind, &f.severity, &f.message, &f.nets, &f.refs)
}

/// [`gather_findings_with_schematic`], keeping what the waivers overruled so a
/// surface can list it as waived instead of dropping it.
fn gather_static(
    board_path: &Path,
    board: &ExtractedBoard,
    text: &str,
    raw: &[u8],
    altium_present: bool,
    lib: &ModelLibrary,
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
) -> anyhow::Result<GatheredStatic> {
    // The stages a progress bar names, in the web analysis's own words and order.
    hauksbee_ir::progress::begin_copper();
    let (clearance_input, mut drc) = super::run_drc(board_path, board, text, raw, altium_present)?;

    // The companion schematic's declarations, applied before anything reads the
    // findings. Reclassifies, never deletes: a covered contact keeps its layer,
    // location and measured gap and gains the declaration that qualifies it.
    // This surface produces CI findings only and builds no evidence registry, so
    // there is no inventory row to add here; the report surfaces above own that.
    let qualification = schematic_ties.map(|ties| ties.qualify(&drc));
    hauksbee_ir::progress::begin_lint();
    let mut lint = crate::checks::engine_lint(board, lib);
    let geo_text = if altium_present { None } else { Some(text) };
    lint.findings
        .extend(crate::checks::layout_lint(board, geo_text, Some(board_path)).findings);
    hauksbee_ir::progress::begin_untracked(hauksbee_ir::progress::Stage::Signals);
    let mut si = crate::checks::engine_si(board, lib, geo_text);
    let usbc = crate::usb_c_report(board);

    let provenance =
        super::clearance_provenance(clearance_input.as_ref(), board_path, text, drc.clearance_mm);
    let eagle_untied = text.contains("<eagle") && schematic_ties.is_none();
    let findings = |drc: &hauksbee_extract::DrcReport,
                    lint: &hauksbee_extract::NetLintReport,
                    si: &hauksbee_extract::SiReport| {
        let structured =
            DrcStructured::from_report_with_ties(drc, qualification.as_ref(), eagle_untied)
                .with_clearance_rule_provenance(&provenance);
        let mut findings = super::drc::drc_findings(&structured, Some(&provenance));
        findings.extend(lint_findings_json(lint));
        findings.extend(si_findings_json(si));
        findings.extend(usbc.as_ref().and_then(usbc_finding_json));
        findings
    };
    let before = findings(&drc, &lint, &si);

    // The same waivers, applied the same way, as every surface that gates: the
    // artifact must not carry a finding the exit code excused, or the other way
    // round.
    let mut waivers = load_waivers(board_path);
    let _ = apply_static_waivers(&mut waivers, &mut lint, &mut si, &mut drc);
    let kept = findings(&drc, &lint, &si);

    // What the waivers took out: every finding before them, less (one for one)
    // every finding still standing after them.
    let mut standing: Vec<_> = kept.iter().map(finding_key).collect();
    let mut reasons = load_waivers(board_path);
    let mut waived = Vec::new();
    for f in &before {
        if let Some(i) = standing.iter().position(|k| *k == finding_key(f)) {
            standing.swap_remove(i);
            continue;
        }
        let why = reasons
            .take_waiver(&f.check, &f.kind, &f.nets, &f.refs, &f.message)
            .map_or_else(
                || "excluded by the board's own DRC exclusions".to_string(),
                |w| format!("{} (until {})", w.reason, w.until),
            );
        waived.push((f.clone(), why));
    }
    Ok(GatheredStatic { kept, waived })
}

/// The findings that fail `hauksbee check` on the board at `board_path`.
///
/// The board is loaded through the shared normalizer with its Eagle companion
/// schematic, the full static suite runs with this board's waivers, and only
/// the findings that gate ([`crate::result::JsonFinding::gates`]) are kept. A simulation
/// run of the same board (`hauksbee-ci`) reads them so it can never report a
/// pass that the board's own static verdict fails.
///
/// # Errors
///
/// Returns an error when the board cannot be loaded or the DRC cannot run over
/// it, as [`gather_findings_with_schematic`].
pub fn static_gating_findings(
    board_path: &Path,
    lib: &ModelLibrary,
) -> anyhow::Result<Vec<crate::result::JsonFinding>> {
    static_gate(board_path, lib).map(|g| g.gating)
}

/// What `hauksbee check` gates on for one board, split by the board's waivers.
#[derive(Debug, Clone, Default)]
pub struct StaticGate {
    /// The findings that fail the check.
    pub gating: Vec<crate::result::JsonFinding>,
    /// Findings that would fail it but a waiver overrules, each with the
    /// waiver's "reason (until date)". Listed, never hidden.
    pub waived: Vec<(crate::result::JsonFinding, String)>,
    /// The as-built overlay the board was judged under, when there was one:
    /// what it changed on the static board, and each entry it could not carry
    /// (a finding on the parts or nets such an entry touches is the design's).
    pub as_built: Option<crate::asbuilt::AsBuiltReport>,
}

/// [`static_gating_findings`] plus the gate-grade findings the board's waiver
/// file overrules, so a surface that reports the static verdict can list them
/// as waived.
///
/// # Errors
///
/// As [`static_gating_findings`].
pub fn static_gate(board_path: &Path, lib: &ModelLibrary) -> anyhow::Result<StaticGate> {
    static_gate_as_built(board_path, lib, None)
}

/// [`static_gate`] on the board as reworked by `overlay`, when there is one.
///
/// The overlay is applied to the extraction exactly as `hauksbee --asbuilt
/// --check` applies it ([`crate::asbuilt::AsBuiltOverlay::apply_to_board`]),
/// so a finding the rework removes is gone and one it creates is found. The
/// copper (DRC) checks still read the design files.
///
/// # Errors
///
/// As [`static_gating_findings`], and when the overlay does not describe the
/// board.
pub fn static_gate_as_built(
    board_path: &Path,
    lib: &ModelLibrary,
    overlay: Option<&crate::asbuilt::AsBuiltOverlay>,
) -> anyhow::Result<StaticGate> {
    let norm = crate::board_input::from_path(board_path)?;
    let altium_present = norm.is_binary();
    let text = norm.layout_text.unwrap_or_default();
    // The same `<eagle>` head sniff `hauksbee check` uses to look for ties.
    let board_is_eagle = text
        .chars()
        .take(512)
        .collect::<String>()
        .contains("<eagle");
    // Ties belong to the design files, so they are read off the design board.
    let ties = crate::schematic_ties::resolve(board_path, &norm.board, None, board_is_eagle)?;
    let mut board = norm.board;
    let as_built = overlay
        .map(|overlay| overlay.apply_to_board(&mut board, lib))
        .transpose()?;
    let gathered = gather_static(
        board_path,
        &board,
        &text,
        &norm.raw,
        altium_present,
        lib,
        ties.as_ref(),
    )?;
    let mut gating = gathered.kept;
    gating.retain(crate::result::JsonFinding::gates);
    let mut waived = gathered.waived;
    waived.retain(|(f, _)| f.gates());
    Ok(StaticGate {
        gating,
        waived,
        as_built,
    })
}

/// Apply this board's waivers to the whole static suite, in one place, and
/// return the findings they overruled.
///
/// Every static surface has to do this identically: a waiver that excuses a
/// finding on `--check` but not on the bare `--json` would make the two
/// disagree about the same board, and the artifact writer (which applies
/// waivers) would then disagree with a gate that did not. The bare `--json`
/// gate skipped this entirely: it exited 2 on an excused finding while the
/// `--junit`/`--sarif` file beside it, written from the waived-down suite, said
/// `failures="0"`.
///
/// A clearance violation does not gate on its own, so the DRC key function
/// leaves the non-shorts unwaivable rather than letting a `drc` waiver excuse
/// something that was never gate-grade.
pub(crate) fn apply_static_waivers(
    waivers: &mut crate::waiver::WaiverSet,
    lint: &mut hauksbee_extract::NetLintReport,
    si: &mut hauksbee_extract::SiReport,
    drc: &mut hauksbee_extract::DrcReport,
) -> Vec<crate::waiver::WaivedFinding> {
    let mut waived = waive_lint(waivers, lint);
    waived.extend(waive_si(waivers, si));
    waived.extend(waive_drc(waivers, drc));
    waived
}

/// Apply the board's `lint`-scoped waivers to `report`, returning what was
/// excused. Resource conflicts are lint-class findings and ride the same check
/// name inside `--check`, so one waiver file covers `--lint`, `--resources` and
/// the combined report; keying them differently would split it.
pub(crate) fn waive_lint(
    waivers: &mut crate::waiver::WaiverSet,
    report: &mut hauksbee_extract::NetLintReport,
) -> Vec<crate::waiver::WaivedFinding> {
    let (kept, waived) = waivers.partition("lint", std::mem::take(&mut report.findings), |f| {
        (
            f.check.as_str().to_string(),
            f.nets.clone(),
            f.refs.clone(),
            f.message.clone(),
        )
    });
    report.findings = kept;
    waived
}

/// Apply the board's `si`-scoped waivers to `report`, returning what was excused.
pub(crate) fn waive_si(
    waivers: &mut crate::waiver::WaiverSet,
    report: &mut hauksbee_extract::SiReport,
) -> Vec<crate::waiver::WaivedFinding> {
    let (kept, waived) = waivers.partition("si", std::mem::take(&mut report.findings), |f| {
        (
            f.check.as_str().to_string(),
            f.nets.clone(),
            f.refs.clone(),
            f.message.clone(),
        )
    });
    report.findings = kept;
    waived
}

/// Apply the board's `drc`-scoped waivers to `report`, returning what was
/// excused. Only a true short is waivable; a clearance violation does not gate
/// on its own, so it has nothing to be excused from.
pub(crate) fn waive_drc(
    waivers: &mut crate::waiver::WaiverSet,
    report: &mut hauksbee_extract::DrcReport,
) -> Vec<crate::waiver::WaivedFinding> {
    // The design's own `drc_exclusions` come first: already out of the gate, listed
    // here so they stay visible.
    let mut excluded: Vec<_> = report
        .excluded
        .iter()
        .map(|f| crate::waiver::WaivedFinding {
            check: "drc".to_string(),
            kind: match f.kind {
                hauksbee_extract::ViolationKind::Short => "short",
                hauksbee_extract::ViolationKind::Clearance
                | hauksbee_extract::ViolationKind::StaleFill => "clearance",
            }
            .to_string(),
            subject: format!(
                "{} to {} on {} at ({:.3}, {:.3}) mm",
                f.net_a_name, f.net_b_name, f.layer, f.x, f.y
            ),
            reason: "excluded by the design's own DRC exclusions (.kicad_pro)".to_string(),
            until: "no expiry".to_string(),
        })
        .collect();
    let (kept, waived) = waivers.partition("drc", std::mem::take(&mut report.findings), |f| {
        let kind = match f.kind {
            hauksbee_extract::ViolationKind::Short => "short",
            hauksbee_extract::ViolationKind::Clearance
            | hauksbee_extract::ViolationKind::StaleFill => "clearance-not-waivable",
        };
        (
            kind.to_string(),
            vec![f.net_a_name.clone(), f.net_b_name.clone()],
            Vec::new(),
            format!("{} to {} on {}", f.net_a_name, f.net_b_name, f.layer),
        )
    });
    report.findings = kept;
    excluded.extend(waived);
    excluded
}

/// What a static pass cannot see, said at the end of one.
///
/// Every section of this report can come back healthy on a board with a real
/// fault in it, because these checks read the copper and the netlist and never
/// run the board. The regression this project was built around is exactly
/// that shape: a rail that collapses on a fuzzed power-up, invisible to
/// any static check and found only by booting the firmware against the solved
/// circuit.
///
/// Someone who reads "Looks healthy" and stops has been misled by omission,
/// even though nothing here is false. Tested against the real board: a bare
/// upload gives a full bind and a clean report, and mentions the dynamic checks
/// nowhere at all.
fn backend_is_compiled(backend: &str) -> bool {
    let kind = backend.split_once(':').map(|(kind, _)| kind);
    (kind == Some("simavr") && cfg!(feature = "avr"))
        || (kind == Some("renode") && cfg!(feature = "renode"))
        || (kind == Some("qemu") && cfg!(feature = "qemu"))
}

/// What, if anything, runs the board right after this static report.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Followed {
    ByNothing,
    BySimulation,
}

fn what_this_pass_did_not_check(
    mcu_bound: bool,
    mcu_backend_compiled: bool,
    usbc_ran: bool,
    followed: Followed,
) -> String {
    let mut s = String::from("\n== What this pass did not check ==\n");
    if followed == Followed::BySimulation {
        // The caller co-simulates the board straight after this report, so
        // "they do not run it" would be false of the run as a whole.
        s.push_str(
            "These checks read the board; nothing above ran it. The simulation \
             that follows runs it for the time you asked for, so a fault that \
             only appears later, or under a different load, will not show.\n",
        );
    } else {
        s.push_str(
            "These checks read the board. They do not run it, so nothing above can \
             see a fault that only appears while the board is powered: a rail that \
             sags on inrush, a brownout at power-up, a part that overheats under \
             load.\n",
        );
    }
    // The USB-C section above appears only when there is a USB-C connector to
    // classify. Its silent absence otherwise would read as "checked and clean"
    // on a board whose USB-C hauksbee never found, so say which it was.
    if !usbc_ran {
        s.push_str(
            "\nUSB-C CC compliance did not run: no USB-C receptacle was found on this \
             board. It has its own section above when there is one.\n",
        );
    }
    if mcu_bound && mcu_backend_compiled {
        s.push_str(
            "\nThis board has a processor with co-simulation support in this build. \
             With its external backend dependency installed where required, it can boot your \
             firmware against the solved circuit and assert on what happens.\n\n  \
             hauksbee-ci init <board>     scaffold a spec into the current dir\n  \
             hauksbee-ci run <spec>       run it, here or in a pipeline\n",
        );
    } else if mcu_bound {
        s.push_str(
            "\nA processor bound on this board, but this binary does not include its \
             co-simulation backend. The static result above is still valid; do not infer \
             that this build can boot its firmware. Use a binary that includes the matching \
             backend before making firmware or timing claims.\n",
        );
    } else {
        s.push_str(
            "\nNo processor bound on this board, so there is no firmware to boot. \
             You can still assert on rails and stress under a power-up scenario:\n\n  \
             hauksbee-ci init <board>     scaffold a spec into the current dir\n",
        );
    }
    s
}

/// Load the waivers that apply to this board, or none.
///
/// A malformed waiver file is a warning, not a failed run. The findings it
/// would have covered simply gate, which fails closed: a typo cannot quietly
/// turn into a silenced check.
///
/// Shared with the single-check reports (`--drc`, `--lint`, `--si`): a board
/// that is green under `--check --strict` must not turn red under the narrower
/// command, or the waiver looks like it silently stopped applying.
pub(crate) fn load_waivers(board_path: &Path) -> crate::waiver::WaiverSet {
    match crate::waiver::WaiverSet::discover(board_path) {
        Ok(set) => set,
        Err(e) => {
            eprintln!(
                "WARNING: ignoring the waiver file ({e}). Every finding it would have \
                 covered gates this run."
            );
            crate::waiver::WaiverSet::default()
        }
    }
}

/// The waived section, plus the two ways a waiver file goes wrong. Shared with
/// the single-check reports so every text surface explains an overruled or
/// lapsed waiver in the same words.
pub(crate) fn render_waivers(
    waived: &[crate::waiver::WaivedFinding],
    waivers: &crate::waiver::WaiverSet,
) -> String {
    // The full `--check` suite evaluates exactly these waiver checks; naming
    // any other check's waiver stale here would tell the user to delete a
    // waiver this command never consulted (e.g. a `ci` waiver for
    // hauksbee-ci's assertions).
    render_waivers_scoped(waived, waivers, &["drc", "lint", "si"], true)
}

/// [`render_waivers`], restricted to the checks a narrower command actually ran.
///
/// `--drc` never runs the SI checks, so an SI waiver necessarily matches
/// nothing there; reporting it "stale" would tell the user to delete a waiver
/// that is doing its job. `scope` keeps the expired/stale sections to the
/// checks the running gate actually evaluated (empty = no restriction).
/// `report_stale` is off for subset commands (`--resources` runs only
/// part of the lint family, so a no-hit proves nothing about the waiver).
pub(crate) fn render_waivers_scoped(
    waived: &[crate::waiver::WaivedFinding],
    waivers: &crate::waiver::WaiverSet,
    scope: &[&str],
    report_stale: bool,
) -> String {
    use std::fmt::Write;
    let in_scope = |w: &&crate::waiver::Waiver| {
        scope.is_empty() || scope.iter().any(|s| w.check.eq_ignore_ascii_case(s))
    };
    let expired: Vec<_> = waivers.expired().into_iter().filter(in_scope).collect();
    let stale: Vec<_> = if report_stale {
        waivers.stale().into_iter().filter(in_scope).collect()
    } else {
        Vec::new()
    };
    if waived.is_empty() && expired.is_empty() && stale.is_empty() {
        return String::new();
    }
    let mut s = String::new();
    if !waived.is_empty() {
        let _ = writeln!(s, "\n== Waived ({}) ==", waived.len());
        let _ = writeln!(
            s,
            "These findings fired and were overruled. They are not in the gate."
        );
        for w in waived {
            let _ = writeln!(s, "  {}/{}: {}", w.check, w.kind, w.subject);
            let _ = writeln!(s, "      because: {} (until {})", w.reason, w.until);
        }
    }
    if !expired.is_empty() {
        let _ = writeln!(
            s,
            "\n{} waiver(s) have lapsed and no longer apply:",
            expired.len()
        );
        for w in &expired {
            let _ = writeln!(
                s,
                "  {}/{} expired {}: {}",
                w.check, w.kind, w.until, w.reason
            );
        }
        let _ = writeln!(
            s,
            "Anything they covered is back in the gate. Look again, then renew or delete."
        );
    }
    if !stale.is_empty() {
        let _ = writeln!(
            s,
            "\n{} waiver(s) matched nothing on this board:",
            stale.len()
        );
        for w in &stale {
            let _ = writeln!(s, "  {}/{}: {}", w.check, w.kind, w.reason);
        }
        let _ = writeln!(
            s,
            "Either the finding is fixed and the waiver can go, or it no longer \
             describes what fires. Note a waiver's `nets` must ALL appear in one \
             finding's net set (AND, not OR); to cover several findings, write one \
             [[waive]] block per finding."
        );
    }
    s
}

/// The bare-`--json` combined machine report (bind + DRC + lint/straps/resources +
/// SI + USB-C) for a `run <board> --json` with no specific report selector.
/// Without it a bare `--json` would fall through to the TUI/websocket default and
/// hang a piped / CI / AI caller. Carries the same findings as `emit_with_schematic_quiet`'s JSON,
/// and (under `strict`) gates with the same exit-2 contract.
pub(crate) fn emit_combined_json_with_schematic(
    input: &StaticInputs,
    strict: bool,
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
) -> anyhow::Result<()> {
    CheckReport {
        input,
        schematic_ties,
    }
    .emit(&CheckDisplay::CombinedJson, strict)
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::render_waivers;
    use crate::result::{lint_findings_json, si_findings_json, BindSummary, DrcStructured};
    use hauksbee_bind::bind_report::UnresolvedActive;
    use hauksbee_extract::{
        DrcReport, LintCheck, LintFinding, NetLintReport, Severity, SiCheck, SiFinding, SiReport,
        SiSeverity,
    };

    fn lint(check: LintCheck, severity: Severity, message: &str) -> LintFinding {
        LintFinding {
            check,
            severity,
            message: message.to_string(),
            refs: vec!["U4".to_string()],
            nets: Vec::new(),
        }
    }

    fn unmodelled(refs: &[&str]) -> Vec<UnresolvedActive> {
        refs.iter()
            .map(|r| UnresolvedActive {
                reference: (*r).to_string(),
                value: "IC".to_string(),
                reason: "no model; left open".to_string(),
                unlocked_by: None,
                consequence: "its nets are not driven".to_string(),
                active_ic: true,
            })
            .collect()
    }

    fn bind(active: Vec<UnresolvedActive>) -> BindSummary {
        BindSummary {
            resolved: 1,
            unresolved: active.len(),
            non_ignored: 1,
            critical_parts_bound: "1/1".to_string(),
            critical_parts_bound_n: 1,
            critical_parts_total: 1,
            mcu_bound: false,
            active_path_unresolved: active,
            resolved_but_open_active: Vec::new(),
        }
    }

    /// Everything one `--check` run says about its outcome, assembled with the
    /// production functions `emit` calls, so the three can be compared.
    struct Surfaces {
        json_verdict: String,
        json_reason: Option<String>,
        triage: String,
        line: String,
        /// The copper section's own one-line verdict.
        copper_section_verdict: String,
    }

    fn surfaces(
        lint: &NetLintReport,
        si: &SiReport,
        unmodelled_parts: &[&str],
        conditions: &[&str],
        copper_unvalidated: Option<&str>,
        scope_nets: Option<usize>,
    ) -> Surfaces {
        let drc = DrcStructured::from_report(&DrcReport {
            clearance_mm: 0.2,
            findings: Vec::new(),
            primitive_count: 2,
            version_warning: copper_unvalidated.map(str::to_string),
            zone_pad_overlaps_suppressed: Some(0),
            excluded: Vec::new(),
            clearance_unjudged: hauksbee_extract::UnjudgedClearance::default(),
        });
        let provenance = crate::result::ClearanceRuleProvenance::defaulted(0.2, false);
        let copper_plain = crate::plain::plain_drc_structured_with_rule_source(&drc, &provenance);
        let summary = bind(unmodelled(unmodelled_parts));
        let mut findings = lint_findings_json(lint);
        findings.extend(si_findings_json(si));
        let would_gate = super::super::lint_fails(lint) || super::super::si_fails(si);
        let mut jr = super::machine_report(
            "synthetic",
            &summary,
            &drc,
            Some(&provenance),
            &findings,
            would_gate,
        );
        jr.clearance_rule_source = Some(provenance.clone());
        jr.verdict_conditions = conditions.iter().map(ToString::to_string).collect();
        // A scope over `nets` named nets, one of which ("VB") the open parts touch.
        let mut jr = jr.with_judgement_scope(scope_nets.map(|nets| crate::scope::JudgementScope {
            open_parts: unmodelled_parts.iter().map(ToString::to_string).collect(),
            nets,
            wiring_dependent: vec!["VB".into()],
            electrical_dependent: vec!["VB".into()],
            held_rails: Vec::new(),
        }));
        let triage = crate::plain::order_triage_with_rule_source(
            &drc,
            &provenance,
            lint,
            si,
            None,
            true,
            summary.unresolved,
            true,
        );
        jr.inspect_items = triage.inspect.len();
        let run_verdict = jr.run_verdict();
        let (_, _, json_serious, json_actionable) = jr.verdict();
        let (serious, worth) = (json_serious, json_actionable.saturating_sub(json_serious));
        let json: serde_json::Value = serde_json::from_str(&jr.to_json()).unwrap();
        Surfaces {
            json_verdict: json["verdict"].as_str().unwrap().to_string(),
            json_reason: json["verdict_reason"].as_str().map(str::to_string),
            triage: triage.render(&run_verdict),
            line: super::verdict_line(
                &run_verdict,
                (serious, worth),
                copper_unvalidated.map(|reason| super::CopperUnvalidated {
                    reason,
                    issues: copper_plain.potential_copper_issue_count(),
                }),
            ),
            copper_section_verdict: copper_plain.verdict(),
        }
    }

    /// One row of the agreement table.
    struct Row {
        name: &'static str,
        lint: Vec<LintFinding>,
        si: Vec<SiFinding>,
        unmodelled: &'static [&'static str],
        conditions: &'static [&'static str],
        copper_unvalidated: Option<&'static str>,
        scope_nets: Option<usize>,
        verdict: &'static str,
        headline: &'static str,
        line: &'static str,
    }

    #[allow(clippy::too_many_lines, reason = "one table of rows")]
    fn agreement_rows() -> Vec<Row> {
        const FAILED: &str = "NOT OK TO ORDER YET: this run failed. ";
        const COPPER: Option<&str> = Some("board newer than the validated KiCad format");
        let warning = || {
            lint(
                LintCheck::OutputContention,
                Severity::Medium,
                "U4 and U5 both drive the net",
            )
        };
        let serious = || lint(LintCheck::OutputContention, Severity::High, "two drivers");
        let row = |name, verdict, headline, line| Row {
            name,
            lint: Vec::new(),
            si: Vec::new(),
            unmodelled: &[],
            conditions: &[],
            copper_unvalidated: None,
            scope_nets: None,
            verdict,
            headline,
            line,
        };
        vec![
            row("clean", "pass", "OK TO ORDER: ", "VERDICT: clean: "),
            Row {
                conditions: &["Q1 is judged at its assumed corners."],
                ..row(
                    "conditional pass",
                    "pass",
                    "OK TO ORDER, conditionally: ",
                    "VERDICT: clean, conditionally: ",
                )
            },
            Row {
                // A rating no check could judge: the rest was judged, so a
                // pass, but one that names the unchecked rating.
                lint: vec![lint(
                    LintCheck::DcOverstress,
                    Severity::Low,
                    "U4's current and power ratings check abstained: its path runs into J1 \
                     (no model, left open), so the rating is unverified, not met",
                )],
                ..row(
                    "abstained rating",
                    "pass",
                    "OK TO ORDER, conditionally: ",
                    "VERDICT: clean, conditionally: ",
                )
            },
            Row {
                // Strap pins nobody checked are an item to inspect: never "clean".
                lint: vec![lint(
                    LintCheck::UncheckedMcu,
                    Severity::Low,
                    "U4 (STM32F405) looks like a strap-bearing MCU but has no boot-strap table, \
                     so its boot strap-pins were NOT checked",
                )],
                ..row(
                    "unchecked MCU straps",
                    "pass",
                    "INSPECT BEFORE ORDERING:\n  - U4 (STM32F405)",
                    "VERDICT: inspect before ordering (1 item), conditionally: ",
                )
            },
            Row {
                lint: vec![warning()],
                ..row("gating warning", "fail", FAILED, "VERDICT: failing: ")
            },
            Row {
                si: vec![SiFinding {
                    check: SiCheck::I2cRiseTime,
                    severity: SiSeverity::Medium,
                    message: "SDA rise time 1.4 us".to_string(),
                    refs: vec!["R5".to_string()],
                    nets: vec!["SDA".to_string()],
                }],
                ..row("gating SI finding", "fail", FAILED, "VERDICT: failing: ")
            },
            Row {
                lint: vec![serious()],
                ..row("serious", "fail", "DO NOT ORDER:", "VERDICT: failing: ")
            },
            Row {
                unmodelled: &["U1", "U2"],
                ..row(
                    "unmodelled key parts",
                    "invalid",
                    "INCONCLUSIVE (not a pass): ",
                    "VERDICT: inconclusive: ",
                )
            },
            Row {
                lint: vec![warning()],
                unmodelled: &["U1", "U2", "U3"],
                ..row(
                    "gating warning plus unmodelled key parts",
                    "fail",
                    FAILED,
                    "VERDICT: failing: ",
                )
            },
            Row {
                lint: vec![serious()],
                unmodelled: &["U1"],
                ..row(
                    "serious plus unmodelled key parts",
                    "fail",
                    "DO NOT ORDER:",
                    "VERDICT: failing: ",
                )
            },
            Row {
                unmodelled: &["U1"],
                scope_nets: Some(10),
                ..row(
                    "unmodelled key part, rest of the board judged",
                    "invalid",
                    "PARTLY JUDGED (not a pass): ",
                    "VERDICT: PARTLY JUDGED (not a pass): ",
                )
            },
            Row {
                lint: vec![serious()],
                unmodelled: &["U1"],
                scope_nets: Some(10),
                ..row(
                    "serious plus a scoped unmodelled key part",
                    "fail",
                    "DO NOT ORDER:",
                    "VERDICT: failing: ",
                )
            },
            Row {
                unmodelled: &["U1"],
                scope_nets: Some(1),
                ..row(
                    "unmodelled key part on every net",
                    "invalid",
                    "INCONCLUSIVE (not a pass): ",
                    "VERDICT: inconclusive: ",
                )
            },
            Row {
                lint: vec![warning()],
                copper_unvalidated: COPPER,
                ..row(
                    "gating warning on an unvalidated copper format",
                    "fail",
                    FAILED,
                    "VERDICT: failing: ",
                )
            },
            Row {
                copper_unvalidated: COPPER,
                ..row(
                    "unvalidated copper format, nothing else",
                    "pass",
                    "OK TO ORDER: ",
                    "VERDICT: copper UNVALIDATED: ",
                )
            },
        ]
    }

    /// The closing line's copper count is the copper section's own, not the
    /// all-checks "worth a look" total: two non-copper notes beside an
    /// UNVALIDATED copper result with nothing listed must not read as two
    /// potential copper issues.
    #[test]
    fn unvalidated_copper_count_is_the_copper_sections_not_the_worth_a_look_total() {
        let notes = vec![
            lint(
                LintCheck::UncheckedMcu,
                Severity::Low,
                "U4 has no boot-strap table, so its boot strap-pins were NOT checked",
            ),
            lint(
                LintCheck::UncheckedMcu,
                Severity::Low,
                "U5 has no boot-strap table, so its boot strap-pins were NOT checked",
            ),
        ];
        let got = surfaces(
            &NetLintReport { findings: notes },
            &SiReport {
                findings: Vec::new(),
            },
            &[],
            &[],
            Some("board newer than the validated KiCad format"),
            None,
        );
        assert!(
            got.copper_section_verdict
                .starts_with("No potential copper issues listed"),
            "{}",
            got.copper_section_verdict
        );
        assert!(
            got.line
                .starts_with("VERDICT: copper UNVALIDATED: no potential copper issues listed;"),
            "{}",
            got.line
        );
        assert!(!got.line.contains("2 potential"), "{}", got.line);
    }

    #[test]
    fn a_pass_with_items_to_inspect_is_never_called_clean() {
        let pass = crate::result::RunVerdict {
            inspect: 17,
            ..crate::result::RunVerdict::default()
        };
        let line = super::verdict_line(&pass, (0, 17), None);
        assert!(
            line.starts_with("VERDICT: inspect before ordering (17 items): 0 serious"),
            "{line}"
        );
        // Control: nothing to inspect is clean.
        let pass = crate::result::RunVerdict::default();
        let line = super::verdict_line(&pass, (0, 0), None);
        assert!(line.starts_with("VERDICT: clean: 0 serious"), "{line}");
    }

    #[test]
    fn an_abstention_listed_to_inspect_is_said_once_in_the_triage() {
        let mcu = lint(
            LintCheck::UncheckedMcu,
            Severity::Low,
            "U4 (STM32F405) looks like a strap-bearing MCU but has no boot-strap table, so its \
             boot strap-pins were NOT checked",
        );
        let got = surfaces(
            &NetLintReport {
                findings: vec![mcu],
            },
            &SiReport {
                findings: Vec::new(),
            },
            &[],
            &[],
            None,
            None,
        );
        assert_eq!(got.json_verdict, "pass");
        // The inspect item states it; a CONDITIONALLY line would repeat it.
        assert_eq!(
            got.triage.matches("NOT checked").count(),
            1,
            "{}",
            got.triage
        );
        assert!(!got.triage.contains("CONDITIONALLY"), "{}", got.triage);
        // The closing line, which has no item list, names it as its condition.
        assert!(got.line.contains("NOT checked"), "{}", got.line);
        // Control: a condition that is not an item still opens the triage.
        let got = surfaces(
            &NetLintReport {
                findings: vec![lint(
                    LintCheck::UncheckedMcu,
                    Severity::Low,
                    "U4 (STM32F405) looks like a strap-bearing MCU but has no boot-strap \
                     table, so its boot strap-pins were NOT checked",
                )],
            },
            &SiReport {
                findings: Vec::new(),
            },
            &[],
            &["Q1 is judged at its assumed corners."],
            None,
            None,
        );
        assert!(
            got.triage
                .contains("CONDITIONALLY: whatever the items below decide, this rests on: Q1"),
            "{}",
            got.triage
        );
        assert_eq!(
            got.triage.matches("NOT checked").count(),
            1,
            "{}",
            got.triage
        );
    }

    #[test]
    fn json_verdict_triage_headline_and_verdict_line_agree() {
        for row in agreement_rows() {
            let got = surfaces(
                &NetLintReport { findings: row.lint },
                &SiReport { findings: row.si },
                row.unmodelled,
                row.conditions,
                row.copper_unvalidated,
                row.scope_nets,
            );
            let name = row.name;
            assert_eq!(got.json_verdict, row.verdict, "{name}: JSON verdict");
            assert!(
                got.triage.contains(&format!("\n{}", row.headline)),
                "{name}: triage headline `{}` missing from\n{}",
                row.headline,
                got.triage
            );
            assert!(
                got.line.starts_with(row.line),
                "{name}: VERDICT line `{}` should start `{}`",
                got.line,
                row.line
            );
            if row.verdict == "pass" {
                assert!(got.json_reason.is_none(), "{name}");
                continue;
            }
            // A partly judged run names the open part and what was not judged.
            if row.headline.starts_with("PARTLY JUDGED") {
                for text in [&got.triage, &got.line] {
                    assert!(text.contains(" U1 has no usable model"), "{text}");
                }
                // Said once: the closing line's tag carries the lead, and the
                // reason after it does not open with it again.
                assert_eq!(got.line.matches("PARTLY JUDGED").count(), 1, "{}", got.line);
                assert_eq!(
                    got.triage.matches("PARTLY JUDGED").count(),
                    1,
                    "{}",
                    got.triage
                );
            }
            // Whatever the wording, a run that is not a pass never reads as one.
            assert!(
                !got.triage.contains("\nOK TO ORDER") && !got.line.contains("VERDICT: clean"),
                "{name}: a {} run read as clean:\n{}\n{}",
                row.verdict,
                got.triage,
                got.line
            );
            // The reason JSON gives is the reason the closing line gives.
            let reason = got.json_reason.as_deref().expect("a non-pass states why");
            let lead = format!("{}: ", hauksbee_ir::vocabulary::PARTLY_JUDGED_LEAD);
            let said = reason.strip_prefix(&lead).unwrap_or(reason);
            assert!(got.line.contains(said), "{name}: {}\n{reason}", got.line);
            // A failure over unmodelled key parts keeps the fail and says the
            // rest of the board is not vouched for, in JSON and in the line.
            if row.verdict == "fail" && !row.unmodelled.is_empty() {
                let list = row.unmodelled.join(", ");
                let caveat = if row.scope_nets.is_some() {
                    "are not judged"
                } else {
                    "the rest of the board is not vouched for"
                };
                for text in [reason, got.line.as_str()] {
                    assert!(
                        text.contains("no usable model")
                            && text.contains(&list)
                            && text.contains(caveat),
                        "{name}: unmodelled parts hidden in:\n{text}"
                    );
                }
                // The triage block says it once, on its own line after the
                // failures, and the failure sentence above does not carry it too.
                let said: Vec<&str> = got
                    .triage
                    .lines()
                    .filter(|l| l.starts_with("NOT JUDGED: "))
                    .collect();
                assert_eq!(
                    said.len(),
                    1,
                    "{name}: one NOT JUDGED line:\n{}",
                    got.triage
                );
                assert!(
                    said[0].contains(&list) && said[0].contains(caveat),
                    "{name}: the line names the parts: {}",
                    said[0]
                );
                assert_eq!(
                    got.triage.matches("no usable model").count(),
                    1,
                    "{name}: said once in the triage block:\n{}",
                    got.triage
                );
                assert!(!got.triage.contains(" Also: "), "{name}:\n{}", got.triage);
            } else {
                // Control: nothing was left unjudged on a failing run, or the
                // run is not a failing one, so there is no such line.
                assert!(
                    !got.triage.contains("NOT JUDGED"),
                    "{name}: unexpected NOT JUDGED line:\n{}",
                    got.triage
                );
            }
        }
    }

    /// E48: the full `--check` suite evaluates drc/lint/si waivers only, so
    /// its waiver section must not name a `ci` waiver stale: hauksbee-ci owns
    /// that check, this command never consulted it, and "matched nothing"
    /// here would tell the user to delete a waiver doing its job elsewhere.
    #[test]
    fn stale_reporting_names_only_the_checks_this_gate_evaluates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hauksbee-waivers.toml");
        std::fs::write(
            &path,
            r#"
[[waive]]
check = "ci"
kind = "voltage"
nets = ["+5V"]
reason = "known sag until the supply rework lands"
until = "2030-01-01"

[[waive]]
check = "si"
kind = "controlled_impedance"
nets = ["USB_DP"]
reason = "prototype stackup"
until = "2030-01-01"
"#,
        )
        .unwrap();
        let set = crate::waiver::WaiverSet::load(&path).unwrap();
        // Neither waiver matched anything: only the si one (an evaluated
        // check) may be reported stale.
        let out = render_waivers(&[], &set);
        assert!(
            out.contains("si/controlled_impedance"),
            "the evaluated check's stale waiver must be named:\n{out}"
        );
        assert!(
            !out.contains("ci/voltage"),
            "a ci waiver must not be reported by the static suite:\n{out}"
        );
    }
}
