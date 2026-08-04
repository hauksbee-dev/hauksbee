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
    verbose: bool,
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
                     unaffected. Add models with --models-dir (hauksbee models --help) to cover them (run --report for the {m}-part \
                     bind table).\n"
                );
            }
            println!("== Copper spacing (DRC) ==");
            print!("{}", crate::render_drc_condensed(&drc_structured, verbose));
            println!("\n== Connectivity / lint ==");
            print!("{}", crate::plain_netlint(&lint).render());
            println!("\n== Signal integrity ==");
            print!("{}", crate::plain_si(&si).render());
            if let Some(u) = &usbc {
                println!("\n== USB-C CC compliance ==");
                print!("{}", u.render_plain());
            }
            print!("{}", what_this_pass_did_not_check(summary.mcu_bound));
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
        super::strict_gate_exit(mode, &gate_items(drc_gates, &drc, &lint, &si, &usbc));
    }
    Ok(())
}

/// Every gating subject across the aggregate suite, in report order
/// (DRC shorts, lint, SI, USB-C), for the `--strict` failure line.
fn gate_items(
    drc_gates: bool,
    drc: &hauksbee_extract::DrcReport,
    lint: &hauksbee_extract::NetLintReport,
    si: &hauksbee_extract::SiReport,
    usbc: &Option<crate::checks::usb_c::UsbcReport>,
) -> Vec<String> {
    let mut items = if drc_gates {
        super::drc_gate_items(drc)
    } else {
        Vec::new()
    };
    items.extend(super::lint_gate_items(lint));
    items.extend(super::si_gate_items(si));
    if let Some(u) = usbc {
        if u.is_serious() {
            items.push(format!("usb_c_cc {}", u.headline));
        }
    }
    items
}

/// The full static suite (DRC shorts + lint + SI + USB-C) as DATA, for the
/// `--junit`/`--sarif` artifact writers: the same checks and the same waiver
/// discipline as [`emit`], without rendering a report. Waived findings are
/// excluded here exactly as they are excluded from the gate, so a SARIF error
/// can never fail a pipeline the report command would pass.
pub fn gather_findings(
    board_path: &Path,
    board: &ExtractedBoard,
    text: &str,
    raw: &[u8],
    altium_present: bool,
    lib: &ModelLibrary,
) -> anyhow::Result<Vec<crate::result::JsonFinding>> {
    let mut drc = if altium_present {
        ExtractedBoard::altium_drc(raw)?
    } else {
        ExtractedBoard::drc_with_clearance_rules(text, kicad_pro_clearance_rules(board_path, board))?
    };
    let mut lint = crate::checks::engine_lint(board, lib);
    let geo_text = if altium_present { None } else { Some(text) };
    let mut si = crate::checks::engine_si(board, lib, geo_text);
    let usbc = crate::usb_c_report(board);

    let mut waivers = load_waivers(board_path);
    let (kept_lint, _) = waivers.partition("lint", std::mem::take(&mut lint.findings), |f| {
        (
            f.check.as_str().to_string(),
            f.nets.clone(),
            f.refs.clone(),
            f.message.clone(),
        )
    });
    lint.findings = kept_lint;
    let (kept_si, _) = waivers.partition("si", std::mem::take(&mut si.findings), |f| {
        (
            f.check.as_str().to_string(),
            f.nets.clone(),
            f.refs.clone(),
            f.message.clone(),
        )
    });
    si.findings = kept_si;
    let (kept_drc, _) = waivers.partition("drc", std::mem::take(&mut drc.findings), |f| {
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

    let mut findings: Vec<crate::result::JsonFinding> = Vec::new();
    // DRC shorts as findings (serious; an unvalidated board format's shorts
    // may be phantom, mirroring the gate they demote to warnings).
    let phantom = drc.version_warning.is_some();
    for f in &drc.findings {
        if !matches!(f.kind, hauksbee_extract::ViolationKind::Short) {
            continue;
        }
        findings.push(crate::result::JsonFinding {
            check: "drc".to_string(),
            kind: "short".to_string(),
            severity: if phantom { "warning" } else { "serious" }.to_string(),
            nets: vec![f.net_a_name.clone(), f.net_b_name.clone()],
            location_mm: None,
            layer: Some(f.layer.clone()),
            refs: Vec::new(),
            actionable: true,
            message: format!(
                "copper short: {} touches {} on {}",
                f.net_a_name, f.net_b_name, f.layer
            ),
            plain: format!(
                "two different nets ({} and {}) are touching on layer {}",
                f.net_a_name, f.net_b_name, f.layer
            ),
            fix: None,
        });
    }
    findings.extend(lint_findings_json(&lint));
    findings.extend(si_findings_json(&si));
    findings.extend(usbc.as_ref().and_then(usbc_finding_json));
    Ok(findings)
}

/// What a static pass cannot see, said at the end of one.
///
/// Every section of this report can come back healthy on a board with a real
/// fault in it, because these checks read the copper and the netlist and never
/// run the board. The flagship regression this project was built around is
/// exactly that shape: a rail that collapses on a fuzzed power-up, invisible to
/// any static check and found only by booting the firmware against the solved
/// circuit.
///
/// Someone who reads "Looks healthy" and stops has been misled by omission,
/// even though nothing here is false. Tested against the real board: a bare
/// upload gives a full bind and a clean report, and mentions the dynamic checks
/// nowhere at all.
fn what_this_pass_did_not_check(mcu_bound: bool) -> String {
    let mut s = String::from("\n== What this pass did not check ==\n");
    s.push_str(
        "These checks read the board. They do not run it, so nothing above can \
         see a fault that only appears while the board is powered: a rail that \
         sags on inrush, a brownout at power-up, a part that overheats under \
         load.\n",
    );
    if mcu_bound {
        s.push_str(
            "\nThis board has a processor hauksbee can emulate, so it can boot your \
             firmware against the solved circuit and assert on what happens.\n\n  \
             hauksbee-ci init <board>     scaffold a spec into the current dir\n  \
             hauksbee-ci run <spec>       run it, here or in a pipeline\n",
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
    render_waivers_scoped(waived, waivers, None, true)
}

/// [`render_waivers`], restricted to the checks a narrower command actually ran.
///
/// `--drc` never runs the SI checks, so an SI waiver necessarily matches
/// nothing there; reporting it "stale" would tell the user to delete a waiver
/// that is doing its job. `scope` keeps the expired/stale sections to the named
/// check. `report_stale` is off for subset commands (`--resources` runs only
/// part of the lint family, so a no-hit proves nothing about the waiver).
pub(crate) fn render_waivers_scoped(
    waived: &[crate::waiver::WaivedFinding],
    waivers: &crate::waiver::WaiverSet,
    scope: Option<&str>,
    report_stale: bool,
) -> String {
    use std::fmt::Write;
    let in_scope = |w: &&crate::waiver::Waiver| {
        scope.is_none_or(|s| w.check.eq_ignore_ascii_case(s))
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
        super::strict_gate_exit(
            OutputMode::Json,
            &gate_items(drc_gates, &drc, &lint, &si, &usbc),
        );
    }
    Ok(())
}
