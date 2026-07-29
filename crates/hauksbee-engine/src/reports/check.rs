//! `--check` / `--all`: the whole static suite (bind + DRC + lint + SI + USB-C)
//! in ONE report, plus the bare-`--json` combined report (bind + DRC + lint + SI
//! + USB-C) that a `run <board> --json` with no specific selector emits.

use std::path::Path;

use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

use crate::binder::bind_board;
use crate::result::{
    lint_findings_json, si_findings_json, usbc_finding_json, BindSummary, DrcStructured, JsonReport,
};

use super::{kicad_pro_clearance_rules, lint_fails, si_fails, OutputMode};

/// Run the full static suite and print it in `mode`, then (under `strict`) exit
/// non-zero if any real finding gates.
pub fn emit(
    board_path: &Path,
    board: &ExtractedBoard,
    text: &str,
    raw: &[u8],
    altium_present: bool,
    lib: &ModelLibrary,
    mode: OutputMode,
    strict: bool,
) -> anyhow::Result<()> {
    let bound = bind_board(board, lib);
    let summary = BindSummary::from_report(&bound.report);
    let mut drc = if altium_present {
        ExtractedBoard::altium_drc(raw)?
    } else {
        ExtractedBoard::drc_with_clearance_rules(
            text,
            kicad_pro_clearance_rules(board_path, board),
        )?
    };
    let lint = crate::checks::engine_lint(board, lib);
    let geo_text = if altium_present { None } else { Some(text) };
    // Route through the single SI chokepoint so this aggregate surface carries
    // the same trace-ampacity + input-cap-ripple findings as the dedicated
    // `--si` report (the bare `si_checks` omitted them, giving `--check` a false
    // "looks healthy" over an under-width power trace).
    let mut si = crate::checks::engine_si(board, lib, geo_text);
    // USB-C CC compliance, only when the board has a USB-C receptacle.
    let usbc = crate::usb_c_report(board);

    // Waivers: overrule a finding this board's owner judged wrong, without
    // switching the check off for everyone. Findings an active waiver covers
    // come out of the gate and go into their own section, never out of sight.
    let mut waivers = load_waivers(board_path);
    let mut lint = lint;
    let (kept_lint, waived_lint) =
        waivers.partition("lint", std::mem::take(&mut lint.findings), |f| {
            (
                f.check.as_str().to_string(),
                f.nets.clone(),
                f.refs.clone(),
                f.message.clone(),
            )
        });
    lint.findings = kept_lint;
    let (kept_si, waived_si) = waivers.partition("si", std::mem::take(&mut si.findings), |f| {
        (
            f.check.as_str().to_string(),
            f.nets.clone(),
            f.refs.clone(),
            f.message.clone(),
        )
    });
    si.findings = kept_si;
    // DrcReport keeps shorts and clearance violations in one list, so partition
    // the whole list and let the key function leave the non-shorts unwaivable.
    // A clearance violation does not gate on its own, so it has nothing to be
    // excused from.
    let (kept_drc, waived_shorts) =
        waivers.partition("drc", std::mem::take(&mut drc.findings), |f| {
            let kind = match f.kind {
                hauksbee_extract::ViolationKind::Short => "short",
                _ => "clearance-not-waivable",
            };
            (
                kind.to_string(),
                vec![f.net_a_name.clone(), f.net_b_name.clone()],
                Vec::new(),
                format!("{} to {} on {}", f.net_a_name, f.net_b_name, f.layer),
            )
        });
    drc.findings = kept_drc;
    let drc_structured = DrcStructured::from_report(&drc);
    let waived: Vec<_> = waived_lint
        .into_iter()
        .chain(waived_si)
        .chain(waived_shorts)
        .collect();

    match mode {
        OutputMode::Json => {
            let mut jr = JsonReport::new(&bound.name, summary);
            jr.drc = Some(drc_structured);
            let mut findings = lint_findings_json(&lint);
            findings.extend(si_findings_json(&si));
            // Fold USB-C in as a finding so the aggregate stays one valid JSON doc.
            findings.extend(usbc.as_ref().and_then(usbc_finding_json));
            jr.findings = Some(findings);
            // A green verdict that quietly dropped findings would be worse than
            // no waivers at all, so the machine surface carries them too.
            jr.waived = waived.iter().cloned().map(Into::into).collect();
            println!("{}", jr.to_json());
        }
        OutputMode::Plain => {
            // Bind-role honesty (Marco): the plain persona surface must not hide
            // that active ICs are unmodelled, otherwise `--check --plain` reads
            // "healthy" while firmware/analog/AC/thermal on their nets are
            // uncovered. Text/JSON/web all carry this; plain must too. Use the
            // SAME union the web/json personas do, unresolved active ICs PLUS
            // resolved-but-open active ICs, so all four personas agree.
            let open = crate::result::coverage_open_active_refs(&summary).len();
            if open > 0 {
                let m = summary.critical_parts_total;
                println!(
                    "Heads-up: {open} active IC(s) are unresolved/open, so firmware/analog/AC/thermal \
                     results on their nets would be INCOMPLETE, but the copper checks below are \
                     unaffected. Add models with --models-dir to cover them (run --report for the {m}-part \
                     bind table).\n"
                );
            }
            println!("== Copper spacing (DRC) ==");
            print!("{}", crate::plain_drc_structured(&drc_structured).render());
            println!("\n== Connectivity / lint ==");
            print!("{}", crate::plain_netlint(&lint).render());
            println!("\n== Signal integrity ==");
            print!("{}", crate::plain_si(&si).render());
            if let Some(u) = &usbc {
                println!("\n== USB-C CC compliance ==");
                print!("{}", u.render_plain());
            }
        }
        OutputMode::Text => {
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
    }
    if !matches!(mode, OutputMode::Json) {
        print!("{}", render_waivers(&waived, &waivers));
    }
    let usbc_serious = usbc.as_ref().is_some_and(|u| u.is_serious());
    // Unvalidated board format (KiCad 10+) → its shorts may be phantom; do not
    // fail the gate on them (the caveat is still printed above).
    let drc_gates = drc.version_warning.is_none() && drc.short_count() > 0;
    let would_gate = drc_gates || lint_fails(&lint) || si_fails(&si) || usbc_serious;
    super::note_ungated_findings(strict, would_gate);
    if strict && would_gate {
        std::process::exit(2);
    }
    Ok(())
}

/// Load the waivers that apply to this board, or none.
///
/// A malformed waiver file is a warning, not a failed run. The findings it
/// would have covered simply gate, which fails closed: a typo cannot quietly
/// turn into a silenced check.
fn load_waivers(board_path: &Path) -> crate::waiver::WaiverSet {
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

/// The waived section, plus the two ways a waiver file goes wrong.
fn render_waivers(
    waived: &[crate::waiver::WaivedFinding],
    waivers: &crate::waiver::WaiverSet,
) -> String {
    use std::fmt::Write;
    let expired = waivers.expired();
    let stale = waivers.stale();
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
             describes what fires."
        );
    }
    s
}

/// The bare-`--json` combined machine report (bind + DRC + lint/straps/resources
/// + SI + USB-C) for a `run <board> --json` with no specific report selector.
/// Without it a bare `--json` would fall through to the TUI/websocket default and
/// hang a piped / CI / AI caller. Carries the same findings as [`emit`]'s JSON,
/// and (under `strict`) gates with the same exit-2 contract.
pub fn emit_combined_json(
    board_path: &Path,
    board: &ExtractedBoard,
    text: &str,
    raw: &[u8],
    altium_present: bool,
    lib: &ModelLibrary,
    strict: bool,
) -> anyhow::Result<()> {
    let bound = bind_board(board, lib);
    let mut jr = JsonReport::new(&bound.name, BindSummary::from_report(&bound.report));
    let drc = if altium_present {
        ExtractedBoard::altium_drc(raw)?
    } else {
        ExtractedBoard::drc_with_clearance_rules(
            text,
            kicad_pro_clearance_rules(board_path, board),
        )?
    };
    jr.drc = Some(DrcStructured::from_report(&drc));
    let lint = crate::checks::engine_lint(board, lib);
    let geo_text = if altium_present { None } else { Some(text) };
    // Same SI chokepoint as the text path: the combined `run --json` must carry
    // the ampacity/ripple findings too, or a machine consumer of the JSON reads
    // a false-clean SI section.
    let si = crate::checks::engine_si(board, lib, geo_text);
    let usbc = crate::usb_c_report(board);
    let mut findings = lint_findings_json(&lint);
    findings.extend(si_findings_json(&si));
    // Fold the USB-C CC verdict in too, matching `--check --json`: the default
    // machine command must not be blind to a Serious CC fault that the explicit
    // `--check --json` surfaces.
    findings.extend(usbc.as_ref().and_then(usbc_finding_json));
    jr.findings = Some(findings);
    println!("{}", jr.to_json());
    // Honour `--strict` on the default machine command: a bare
    // `run <board> --json --strict` must gate a shorted/failing board like the
    // text path does (it silently exited 0 before). Mirror emit()'s gate.
    let usbc_serious = usbc.as_ref().is_some_and(|u| u.is_serious());
    let drc_gates = drc.version_warning.is_none() && drc.short_count() > 0;
    let would_gate = drc_gates || lint_fails(&lint) || si_fails(&si) || usbc_serious;
    super::note_ungated_findings(strict, would_gate);
    if strict && would_gate {
        std::process::exit(2);
    }
    Ok(())
}
