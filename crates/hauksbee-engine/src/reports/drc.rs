//! The `--drc` report: geometric copper-short and clearance detection.
//!
//! Runs over the extracted board and renders in the requested output mode. It
//! also carries the shared `kicad-cli` oracle cross-check and, under `--strict`,
//! exits non-zero on a true short. Pure CLI glue over the extractor's DRC and
//! the binder.

use hauksbee_ir::{print, println};
use std::path::Path;

use crate::binder::bind_board;
use crate::result::{BindSummary, DrcStructured, JsonReport};

use super::{KicadClearanceInput, OutputMode, StaticInputs};

/// Run geometric short / clearance detection, print it in `mode`, cross-check the
/// oracle when asked, then (under `strict`) exit non-zero on a true short. Returns
/// `Ok(())` on the non-gating paths; a strict short calls `std::process::exit(2)`.
/// `quiet` suppresses the text surface.
#[allow(clippy::fn_params_excessive_bools, reason = "independent CLI switches")]
#[allow(clippy::too_many_lines, reason = "linear pass: detect, render, gate")]
pub(crate) fn emit_with_schematic_quiet(
    input: &StaticInputs,
    mode: OutputMode,
    oracle: bool,
    strict: bool,
    verbose: bool,
    quiet: bool,
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
) -> anyhow::Result<()> {
    let StaticInputs {
        board_path, text, ..
    } = *input;
    let (clearance_input, mut report) = input.drc()?;
    // The companion schematic's declarations, applied before waivers and before
    // anything reads the findings. Reclassifies, never deletes.
    let qualification = schematic_ties.map(|ties| ties.qualify(&report));
    // Waivers, same semantics as `--check`: a short the board's owner overruled
    // for a stated reason must come out of THIS gate too, or the same board is
    // green under `--check --strict` and red under `--drc --strict`. The key
    // mirrors check.rs exactly: only a true short is waivable; a clearance
    // violation does not gate on its own, so it has nothing to be excused from.
    let mut waivers = super::check::load_waivers(board_path);
    let waived = super::check::waive_drc(&mut waivers, &mut report);
    // Zero routed copper: a pads-only board passes the spacing check
    // vacuously, so `UNROUTED_COPPER_NOTE` is printed by the two surfaces that
    // read this flag, `--drc` and `--check`.
    let unrouted = !input.altium_present && super::unrouted_kicad_layout(text);
    let bound = bind_board(input.board, input.lib);
    let evidence = input.evidence(&bound.report)?.with_custom_rules_coverage(
        clearance_input
            .as_ref()
            .and_then(KicadClearanceInput::custom_coverage),
    )?;
    let evidence =
        super::with_schematic_evidence(evidence, schematic_ties, qualification.as_ref())?;
    let provenance = super::clearance_provenance(
        clearance_input.as_ref(),
        board_path,
        text,
        report.clearance_mm,
    );
    let provenance = without_unread_rule(provenance, input.input_kind);
    let mut structured = DrcStructured::from_report_with_ties(
        &report,
        qualification.as_ref(),
        text.contains("<eagle") && schematic_ties.is_none(),
    )
    .with_clearance_rule_provenance(&provenance);
    // Run the independent oracle before rendering so agreement on an exact net
    // pair can sit on the finding line itself, where a reader is deciding
    // whether a demoted KiCad-format finding is credible.
    let oracle_summary = (oracle && mode != OutputMode::Json).then(|| {
        oracle_cross_check_and_annotate(
            board_path,
            &report,
            &mut structured,
            qualification.as_ref(),
        )
    });
    let mut maps = evidence.maps_for_drc_with_ties(&structured, qualification.as_ref())?;
    let coverage = evidence.check_coverage_map("drc", "DRC input coverage")?;
    let coverage_undermined =
        coverage.status() == hauksbee_ir::evidence::EvidenceStatus::Undermined;
    if coverage.status() != hauksbee_ir::evidence::EvidenceStatus::Clean {
        maps.push(coverage);
    }
    let evidence = evidence.with_maps(maps);
    // The machine document is built in EVERY output mode: building it is what
    // tells the CI artifacts the run's verdict (see `JsonReport::to_json`), so a
    // `--junit`/`--sarif` beside a text report cannot be green over a verdict
    // that is not a pass. Only `--json` prints it.
    //
    // Grouped DRC: shorts kept verbatim, clearance findings
    // grouped by (net_a, net_b, layer), at-limit separated from below-rule.
    let mut jr = JsonReport::new(&bound.name, BindSummary::from_report(&bound.report))
        .with_inputs(input.inputs)
        .with_evidence(&evidence);
    jr.findings = Some(drc_findings(&structured, Some(&provenance)));
    jr.drc = Some(structured.clone());
    jr.clearance_rule_source = Some(provenance.clone());
    // A copper-only verdict over default rules is a conditional pass,
    // and says which checks it covers.
    jr.verdict_conditions
        .extend(default_rules_condition(&structured, &provenance));
    jr.verdict_conditions
        .extend(unmeasured_spacing_condition(input.input_kind));
    jr = jr.with_unorderable_input(unorderable_input(input.input_kind, None));
    jr.push_coverage_note(crate::scope::single_check_note("DRC"));
    if provenance.source == crate::result::ClearanceRuleSource::Defaulted {
        jr.push_coverage_note(provenance.cli_notice());
    }
    if unrouted {
        jr.push_coverage_note(super::UNROUTED_COPPER_NOTE.to_string());
    }
    // A green verdict that quietly dropped findings would be worse than
    // no waivers at all, so the machine surface carries them too.
    jr.waived = waived.iter().cloned().map(Into::into).collect();
    let document = jr.to_json();
    match mode {
        OutputMode::Json => {
            println!("{document}");
        }
        OutputMode::Plain => {
            if unrouted {
                println!("{}", super::UNROUTED_COPPER_NOTE);
            }
            // Plain mode renders from the SAME grouped structure as text/json so
            // all surfaces agree: duplicates collapsed, and gap==rule labelled
            // "at minimum clearance (no margin)" rather than the wrong "below".
            // Repeated near-identical clearance findings condense to aggregate
            // lines past the first few; --verbose restores every instance.
            print!(
                "{}",
                crate::plain::render_drc_condensed_with_rule_source(
                    &structured,
                    &provenance,
                    verbose,
                )
            );
        }
        OutputMode::Text => {
            if unrouted {
                println!("{}", super::UNROUTED_COPPER_NOTE);
            }
            // Grouped, honest DRC: one line per (net pair + cause) with a count,
            // and gap==rule labelled "at minimum clearance (no margin)" rather
            // than the wrong "below the spacing the board asks for".
            print!(
                "{}",
                structured.render_with_clearance_rule_provenance(&provenance)
            );
        }
    }
    if let Some(summary) = &oracle_summary {
        print!("{summary}");
    }
    if !matches!(mode, OutputMode::Json) {
        // The same scope the JSON document states: copper only, not the board.
        println!("\n{}", crate::scope::single_check_note("DRC"));
        print!(
            "{}",
            super::render_evidence_appendix(&evidence, quiet, verbose)
        );
        print!(
            "{}",
            super::check::render_waivers_scoped(&waived, &waivers, &["drc"], true)
        );
    }
    // Strict: any confirmed short fails the gate (clearance-only does not). An
    // unmatched unvalidated-format short remains a non-gating note; an exact
    // KiCad-oracle net-pair match restores serious severity and gating.
    let would_gate = structured
        .shorts
        .iter()
        .any(|short| short.severity == "serious");
    super::note_ungated_findings(strict, would_gate);
    if strict && would_gate {
        super::strict_gate_exit(mode, &super::drc_structured_gate_items(&structured));
    }
    // Copper is model-free: only an undermined DRC coverage claim (the input
    // could not honestly be inspected) or an undermined shorts map exits 3;
    // the bind state never poisons the copper surface.
    let unvouched =
        coverage_undermined || crate::result::run_level_undermined(evidence.maps(), |_| false);
    super::note_unvouched_exit(strict, unvouched);
    if strict && unvouched {
        let refusal = crate::result::Refusal::new(
            "a trustworthy copper-spacing result",
            "DRC input coverage or run-level evidence is undermined",
            vec!["the parsed board inventory remains available"],
            "resolve the cited input/evidence assumption, then rerun --drc",
        );
        crate::reports::ci_artifacts::exit_with_refusal(
            crate::result::EXIT_INVALID_FOR_ANALYSIS,
            &refusal,
        );
    }
    Ok(())
}

/// Every DRC result as machine findings, in report order: each short, then
/// each below-rule clearance group, with its location, layer, nets and the
/// parts whose pads it touches. The `findings[]` array, the CI artifacts and
/// the `--strict` gate read this one list, so they cannot disagree.
///
/// A short keeps its structured severity: `serious` gates; `note` (an
/// unvalidated-format claim or a board-authorized tie) does not. A clearance
/// group is a `warning` and never gates on its own; one measured only against
/// a default rule without crossing its floor is not actionable.
pub(crate) fn drc_findings(
    drc: &DrcStructured,
    provenance: Option<&crate::result::ClearanceRuleProvenance>,
) -> Vec<crate::result::JsonFinding> {
    let phantom = drc.version_warning.is_some();
    let shorts = drc.shorts.iter().map(|s| crate::result::JsonFinding {
        check: "drc".into(),
        kind: "short".into(),
        severity: s.severity.clone(),
        nets: vec![s.net_a.clone(), s.net_b.clone()],
        location_mm: Some(s.loc_mm),
        layer: Some(s.layer.clone()),
        refs: s.refs.clone(),
        // A tool-only claim still needs a look; an authorized tie does not.
        actionable: s.severity == "serious" || phantom,
        message: format!(
            "copper short: {} touches {} on {}",
            s.net_a, s.net_b, s.layer
        ),
        plain: s.plain.clone(),
        fix: Some(s.fix.clone()),
    });
    let groups = drc.violations.iter().map(|g| crate::result::JsonFinding {
        check: "drc".into(),
        kind: "clearance".into(),
        severity: "warning".into(),
        nets: vec![g.net_a.clone(), g.net_b.clone()],
        location_mm: Some(g.min_gap_loc_mm),
        layer: Some(g.layer.clone()),
        refs: g.refs.clone(),
        actionable: !provenance.is_some_and(|p| p.is_advisory(g)),
        message: format!(
            "clearance: {} to {} on {}, {} below the {:.3} mm rule (tightest {})",
            g.net_a,
            g.net_b,
            g.layer,
            g.below_count,
            g.rule_mm,
            hauksbee_ir::units::format_gap_mm(g.min_gap_mm)
        ),
        plain: provenance.map_or_else(|| g.plain.clone(), |p| g.label_with_rule_provenance(p)),
        fix: Some(g.fix.clone()),
    });
    // A stale fill is a warning: the action is a refill, and it never gates.
    let stale = drc.stale_fills.iter().map(|sf| crate::result::JsonFinding {
        check: "drc".into(),
        kind: "stale_fill".into(),
        severity: "warning".into(),
        nets: vec![sf.zone_net.clone(), sf.copper_net.clone()],
        location_mm: Some(sf.loc_mm),
        layer: Some(sf.layer.clone()),
        refs: Vec::new(),
        actionable: true,
        message: sf.plain.clone(),
        plain: sf.plain.clone(),
        fix: Some(sf.fix.clone()),
    });
    shorts.chain(stale).chain(groups).collect()
}

/// The condition a pass rests on when copper spacing was judged against a
/// default clearance rather than the board's own rules, with how to lift it.
/// `None` for the board's own rules, or when there was no copper to space (a
/// netlist): a pass on them is unconditional.
pub(crate) fn default_rules_condition(
    drc: &DrcStructured,
    provenance: &crate::result::ClearanceRuleProvenance,
) -> Option<String> {
    use crate::result::ClearanceRuleSource;
    if drc.primitive_count == 0 {
        return None;
    }
    let lift = match provenance.source {
        ClearanceRuleSource::Defaulted if provenance.project_file_found => {
            "repair the board's .kicad_pro so its netclass rules can be read, then rerun"
        }
        ClearanceRuleSource::Defaulted => {
            "put the board's .kicad_pro beside it (or upload it with the board) and rerun"
        }
        ClearanceRuleSource::ToolDefault => {
            "this format carries no design rules Hauksbee reads, so confirm spacing with your CAD tool's own DRC"
        }
        _ => return None,
    };
    Some(format!(
        "Copper spacing was judged against a DEFAULT {:.3} mm clearance, not your rules; \
         {lift} to remove this assumption.",
        provenance.value_mm
    ))
}

/// The provenance, marked as having read no clearance rule when the input
/// kind carries none: fabrication files, netlists and schematics. Their report
/// then says "none", not a default or a board-file value nobody measured
/// against.
pub(crate) fn without_unread_rule(
    provenance: crate::result::ClearanceRuleProvenance,
    kind: crate::board_input::InputKind,
) -> crate::result::ClearanceRuleProvenance {
    if unmeasured_spacing_condition(kind).is_some()
        || kind == crate::board_input::InputKind::Schematic
    {
        provenance.with_no_rule_read()
    } else {
        provenance
    }
}

/// What to say, for a pass over an input that is not a board layout: a
/// schematic. It is the sentence a pass opens with in place of "OK TO ORDER"
/// (see [`crate::result::RunVerdict::unorderable`]) and one of the pass's
/// conditions. `layout_text` is the input's text where the loader read a lone
/// schematic sheet as plain text (the upload path cannot see sibling files, so
/// it does not call it a schematic); the command line passes `None`. `None` for
/// every input that is a board, or a fab export of one.
pub(crate) fn unorderable_input(
    kind: crate::board_input::InputKind,
    layout_text: Option<&str>,
) -> Option<String> {
    use crate::board_input::InputKind;
    let is_schematic = kind == InputKind::Schematic
        || (kind == InputKind::Text
            && layout_text.is_some_and(|text| {
                hauksbee_extract::reader::Registry::builtin()
                    .detect(text.as_bytes(), None)
                    .is_some_and(|reader| reader.name() == "kicad-schematic")
            }));
    is_schematic.then(|| {
        "this input is a schematic, not a board layout, so there is no board to order yet: \
         Hauksbee checked the circuit as drawn (parts, connections and ratings against \
         datasheets), but a schematic has no copper, spacing, placement or fabrication files, \
         so none of those were checked and ordering cannot be judged. Run hauksbee on the \
         board layout (.kicad_pcb) to judge ordering."
            .to_string()
    })
}

/// The condition a pass rests on when the input carried connectivity but no
/// copper geometry the DRC measures (a gerber, IPC-D-356, ODB++, IPC-2581
/// or EasyEDA input), so copper spacing was never examined at all.
///
/// Distinct from [`default_rules_condition`], which is about a rule that WAS
/// applied but is not the board's own. Here no rule could be applied: an empty
/// sweep finds nothing whatever the gaps are, and a headline of "OK TO ORDER"
/// over it would vouch for spacing nobody measured. `None` for every input whose
/// copper the DRC does read.
pub(crate) fn unmeasured_spacing_condition(kind: crate::board_input::InputKind) -> Option<String> {
    use crate::board_input::InputKind;
    matches!(
        kind,
        InputKind::Gerber
            | InputKind::Ipc356Archive
            | InputKind::Odb
            | InputKind::Ipc2581
            | InputKind::Easyeda
    )
    .then(|| {
        format!(
            "Copper spacing was not checked: {} gives Hauksbee connectivity but no copper \
             geometry to measure gaps on, so a gap below your fab's minimum would not be \
             flagged; run your CAD tool's design-rule check on the layout to remove this \
             assumption.",
            crate::board_input::input_kind_phrase(kind)
        )
    })
}

/// Parse a version string like "10.0.3" (or "`KiCad` 9.0.3") into a comparable
/// (major, minor, patch) tuple, ignoring any surrounding text.
fn parse_version(s: &str) -> (u32, u32, u32) {
    let n: Vec<u32> = s
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|x| x.parse().ok())
        .collect();
    (
        n.first().copied().unwrap_or(0),
        n.get(1).copied().unwrap_or(0),
        n.get(2).copied().unwrap_or(0),
    )
}

/// Extract the "actual N mm" distance from a kicad-cli DRC violation description
/// like "Clearance violation (zone clearance 0.5000 mm; actual 0.0000 mm)".
fn actual_mm(desc: &str) -> Option<f64> {
    let rest = &desc[desc.find("actual ")? + "actual ".len()..];
    rest.split_whitespace().next()?.parse().ok()
}

/// Locate a usable `kicad-cli` (the geometric-DRC oracle): PATH first, then the
/// standard macOS / Linux / Homebrew install locations, preferring the highest
/// version (a KiCad-10 cli is needed to read v20260206 boards). `KiCad` is NOT
/// bundled with hauksbee; this finds an existing install. Returns (path, version).
///
/// `kicad-cli version` takes seconds to start on a cold macOS bundle, and the
/// same binary is usually reachable as PATH, a /Applications bundle and a
/// Homebrew link. So the candidates are de-duplicated by real path, asked in
/// parallel, and each answer is remembered until that binary changes.
pub(crate) fn find_kicad_cli() -> Option<(String, String)> {
    static VERSIONS: crate::deps::StampCache<String> = crate::deps::StampCache::new();
    let mut candidates: Vec<String> = crate::deps::which_on_path("kicad-cli")
        .map(|p| p.to_string_lossy().into_owned())
        .into_iter()
        .collect();
    let home = std::env::var("HOME").unwrap_or_default();
    for base in ["/Applications".to_string(), format!("{home}/Applications")] {
        if let Ok(rd) = std::fs::read_dir(&base) {
            for e in rd.flatten() {
                let name = e.file_name();
                if name.to_str().is_some_and(|n| n.starts_with("KiCad")) {
                    // Handles both `<base>/KiCad*.app/...` (entry is the bundle) and
                    // `<base>/KiCad*/KiCad.app/...` (entry is a folder holding it,
                    // the macOS .dmg / cask layout).
                    for sub in [
                        "Contents/MacOS/kicad-cli",
                        "KiCad.app/Contents/MacOS/kicad-cli",
                    ] {
                        let cli = e.path().join(sub);
                        if cli.exists() {
                            candidates.push(cli.to_string_lossy().into_owned());
                        }
                    }
                }
            }
        }
    }
    for p in [
        "/usr/bin/kicad-cli",
        "/usr/local/bin/kicad-cli",
        "/opt/homebrew/bin/kicad-cli",
    ] {
        if std::path::Path::new(p).exists() {
            candidates.push(p.to_string());
        }
    }
    let mut seen = std::collections::HashSet::new();
    candidates.retain(|c| seen.insert(std::fs::canonicalize(c).unwrap_or_else(|_| c.into())));
    let version_of = |c: &String| -> Option<String> {
        let real = std::fs::canonicalize(c).unwrap_or_else(|_| c.into());
        if let Some(ver) = VERSIONS.get(&real) {
            return Some(ver);
        }
        let out = std::process::Command::new(c).arg("version").output().ok()?;
        if !out.status.success() {
            return None;
        }
        let ver = String::from_utf8_lossy(&out.stdout).trim().to_string();
        VERSIONS.put(&real, ver.clone());
        Some(ver)
    };
    let answers: Vec<Option<String>> = std::thread::scope(|scope| {
        // Every thread must be started before the first join, so the handles
        // are collected on purpose.
        #[allow(clippy::needless_collect)]
        let handles: Vec<_> = candidates
            .iter()
            .map(|c| scope.spawn(|| version_of(c)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().ok().flatten())
            .collect()
    });
    candidates
        .into_iter()
        .zip(answers)
        // Reversed so a tie on version keeps the earliest candidate (PATH first).
        .rev()
        .filter_map(|(c, ver)| ver.map(|v| (c, v)))
        .max_by_key(|(_, v)| parse_version(v))
}

/// Cross-check hauksbee's geometric DRC against `KiCad`'s own `kicad-cli pcb drc`,
/// so a copper finding is self-confirming without running a second tool by hand.
/// Honest about the two tools' different scopes (`KiCad`'s violation count includes
/// clearance / annular-ring / etc.), and flags the one case that matters: hauksbee
/// reporting a short the oracle does not (a likely hauksbee false positive).
#[derive(Default)]
struct OracleCrossCheck {
    summary: String,
    version: Option<String>,
    confirmed_pairs: std::collections::BTreeSet<(String, String)>,
}

fn net_pair(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

fn item_net(description: &str) -> Option<&str> {
    let start = description.find('[')? + 1;
    let end = description[start..].find(']')? + start;
    Some(&description[start..end])
}

fn oracle_confirmed_pairs(v: &serde_json::Value) -> std::collections::BTreeSet<(String, String)> {
    let mut pairs = std::collections::BTreeSet::new();
    let Some(violations) = v.get("violations").and_then(|x| x.as_array()) else {
        return pairs;
    };
    for violation in violations {
        let ty = violation.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let touches = ty == "shorting_items"
            || ((ty == "clearance" || ty == "hole_clearance")
                && violation
                    .get("description")
                    .and_then(|d| d.as_str())
                    .and_then(actual_mm)
                    .is_some_and(|actual| actual < 0.005));
        if !touches {
            continue;
        }
        let nets: Vec<&str> = violation
            .get("items")
            .and_then(|items| items.as_array())
            .into_iter()
            .flatten()
            .filter_map(|item| item.get("description").and_then(|d| d.as_str()))
            .filter_map(item_net)
            .collect();
        if let [first, second, ..] = nets.as_slice() {
            if first != second {
                pairs.insert(net_pair(first, second));
            }
        }
    }
    pairs
}

fn annotate_oracle_agreement(
    structured: &mut DrcStructured,
    oracle: &OracleCrossCheck,
    authorized_ties: &[bool],
) {
    let version = oracle.version.as_deref().unwrap_or("unknown version");
    for (index, short) in structured.shorts.iter_mut().enumerate() {
        if oracle
            .confirmed_pairs
            .contains(&net_pair(&short.net_a, &short.net_b))
        {
            short.attach_oracle_agreement(version);
            // KiCad 10+ format uncertainty is the only demotion an independent
            // pair match settles. A board-local, coordinate-authorized net tie
            // remains a note: the oracle confirms contact, not that it is a
            // defect. Validated-format shorts are already serious.
            if structured.version_warning.is_some()
                && !authorized_ties.get(index).copied().unwrap_or(false)
            {
                short.severity = "serious".to_string();
            }
        }
    }
}

/// Run `KiCad`'s DRC oracle and attach pair-specific agreement to the existing
/// structured findings. This is shared by `--drc` and the combined `--check`
/// surface so `--oracle` cannot mean two different things.
pub(crate) fn oracle_cross_check_and_annotate(
    board: &Path,
    report: &hauksbee_extract::DrcReport,
    structured: &mut DrcStructured,
    qualification: Option<&hauksbee_extract::DrcTieQualification>,
) -> String {
    let result = oracle_cross_check(board, report);
    let authorized_ties: Vec<bool> = report
        .shorts()
        .map(|finding| qualification.is_some_and(|ties| ties.tie_for(finding).is_some()))
        .collect();
    annotate_oracle_agreement(structured, &result, &authorized_ties);
    result.summary
}

#[allow(clippy::too_many_lines, reason = "one kicad-cli run, read, classify")]
fn oracle_cross_check(board: &Path, report: &hauksbee_extract::DrcReport) -> OracleCrossCheck {
    let Some((cli, ver)) = find_kicad_cli() else {
        return OracleCrossCheck {
            summary: format!(
                "\noracle: no kicad-cli found (PATH or /Applications). Install KiCad to \
             cross-check geometric DRC; see {}.\n",
                hauksbee_ir::docs_url("docs/cosim/ORACLES.md")
            ),
            ..Default::default()
        };
    };
    let tmp = std::env::temp_dir().join(format!(
        "hauksbee_oracle_drc_{}_{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_file(&tmp);
    let run = std::process::Command::new(&cli)
        .args(["pcb", "drc", "--severity-error", "--format", "json", "-o"])
        .arg(&tmp)
        .arg(board)
        .output();
    let Ok(out) = run else {
        return OracleCrossCheck {
            summary: format!("\noracle (kicad-cli {ver}): failed to launch.\n"),
            version: Some(ver),
            ..Default::default()
        };
    };
    let Ok(text) = std::fs::read_to_string(&tmp) else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut detail = Vec::new();
        if let Some(code) = out.status.code() {
            detail.push(format!("exit status {code}"));
        } else {
            detail.push("terminated by signal".to_string());
        }
        if !stderr.trim().is_empty() {
            detail.push(stderr.trim().to_string());
        }
        if !stdout.trim().is_empty() {
            detail.push(stdout.trim().to_string());
        }
        let why = detail.join("; ");
        return OracleCrossCheck {
            summary: format!(
                "\noracle (kicad-cli {ver}): could not load this board{}. A KiCad-10 (>= 10.0) \
             cli is required for v20260206 boards.\n",
                if why.trim().is_empty() {
                    String::new()
                } else {
                    format!(" ({})", why.trim())
                }
            ),
            version: Some(ver),
            ..Default::default()
        };
    };
    let _ = std::fs::remove_file(&tmp);
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    let violations = v.get("violations").and_then(|x| x.as_array());
    let nviol = violations.map_or(0, std::vec::Vec::len);
    let nunconn = v
        .get("unconnected_items")
        .and_then(|x| x.as_array())
        .map_or(0, std::vec::Vec::len);
    // What "a short" means in each tool: hauksbee = copper of two nets at gap <= 0
    // (touching). KiCad expresses the same fact two ways, a `shorting_items`
    // violation (its connectivity merged the nets) OR a `clearance`/`hole_clearance`
    // at actual ~0 mm (geometrically touching but not merged). Count both as the
    // oracle's confirmed touches; KiCad's other violations (annular, mask-bridge,
    // courtyard, sub-rule-but-positive clearance) are not net shorts. Counts do not
    // map 1:1 (the tools decompose a touch into different numbers of rows), so the
    // verdict is about presence/over-reporting, not exact equality.
    let confirmed = violations.map_or(0, |a| {
        a.iter()
            .filter(|x| {
                let ty = x.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if ty == "shorting_items" {
                    return true;
                }
                (ty == "clearance" || ty == "hole_clearance")
                    && x.get("description")
                        .and_then(|d| d.as_str())
                        .and_then(actual_mm)
                        .is_some_and(|a| a < 0.005)
            })
            .count()
    });
    let (shorts, clear) = (report.short_count(), report.clearance_violations().count());
    let stale = report.stale_fills().count();
    let verdict = if shorts == 0 && stale > 0 {
        // KiCad words a track, via or pad inside a stale fill as a clearance
        // violation at 0, which the touching-copper count above includes.
        if confirmed > 0 {
            format!(
                "agree: KiCad also flags the {stale} item(s) inside a stale zone fill, as clearance violations at 0 mm; refill zones, not a short"
            )
        } else {
            format!(
                "hauksbee finds {stale} item(s) inside a stale zone fill that the oracle does not flag; investigate"
            )
        }
    } else if shorts == 0 && confirmed == 0 {
        "agree: neither finds touching copper".to_string()
    } else if shorts > 0 && confirmed == 0 {
        format!("hauksbee finds {shorts} short(s) the oracle does not; likely false positives, investigate")
    } else if shorts == 0 && confirmed > 0 {
        format!(
            "oracle finds {confirmed} touching-copper violation(s) hauksbee missed; investigate"
        )
    } else if shorts > confirmed * 2 {
        format!("both find touching copper, but hauksbee's {shorts} >> the oracle's {confirmed}: hauksbee likely over-reports; compare by location")
    } else {
        format!("agree: both find touching copper ({shorts} hauksbee / {confirmed} oracle; counts differ by decomposition)")
    };
    let stale_text = if stale > 0 {
        format!(", {stale} stale zone fill(s)")
    } else {
        String::new()
    };
    OracleCrossCheck {
        summary: format!(
            "\noracle (kicad-cli {ver}): {confirmed} touching-copper violation(s), {nviol} total DRC \
         violation(s), {nunconn} unconnected.\n\
         hauksbee: {shorts} short(s), {clear} clearance{stale_text}. -> {verdict}.\n"
        ),
        version: Some(ver),
        confirmed_pairs: oracle_confirmed_pairs(&v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fab_inputs_with_no_copper_geometry_pass_only_conditionally() {
        use crate::board_input::InputKind;
        for kind in [
            InputKind::Gerber,
            InputKind::Ipc356Archive,
            InputKind::Odb,
            InputKind::Ipc2581,
            InputKind::Easyeda,
        ] {
            let condition = unmeasured_spacing_condition(kind)
                .unwrap_or_else(|| panic!("{kind:?} must state that spacing was not measured"));
            assert!(
                condition.starts_with("Copper spacing was not checked"),
                "{condition}"
            );
        }
        // The controls: inputs whose copper the DRC reads carry no such condition.
        for kind in [InputKind::Text, InputKind::Altium] {
            assert!(unmeasured_spacing_condition(kind).is_none(), "{kind:?}");
        }
    }

    /// Conditions are listed one under another and joined into the headline,
    /// so each starts like a sentence ("Copper spacing ...", like the next
    /// one's "Rails ...").
    #[test]
    fn the_default_rules_condition_starts_with_a_capital_for_every_source() {
        use crate::result::{ClearanceRuleProvenance, DrcStructured};
        let drc = DrcStructured {
            clearance_rule_mm: 0.2,
            primitive_count: 20,
            stale_fills: Vec::new(),
            shorts: Vec::new(),
            violations: Vec::new(),
            at_limit: Vec::new(),
            version_warning: None,
            suppression_note: None,
        };
        for provenance in [
            ClearanceRuleProvenance::defaulted(0.2, false),
            ClearanceRuleProvenance::defaulted(0.2, true),
            ClearanceRuleProvenance::tool_default(0.2),
        ] {
            let condition = default_rules_condition(&drc, &provenance)
                .unwrap_or_else(|| panic!("{:?} is a default rule", provenance.source));
            assert!(
                condition.starts_with("Copper spacing was judged against a DEFAULT 0.200 mm"),
                "{condition}"
            );
        }
    }

    #[test]
    fn oracle_confirmed_unvalidated_short_is_promoted_but_tool_only_short_is_not() {
        let json = serde_json::json!({
            "violations": [{
                "type": "clearance",
                "description": "Clearance violation (actual 0.0010 mm)",
                "items": [
                    {"description": "Pad 1 [+3V3] of U1"},
                    {"description": "Zone [GND] on F.Cu"}
                ]
            }]
        });
        let oracle = OracleCrossCheck {
            summary: String::new(),
            version: Some("10.0.5".into()),
            confirmed_pairs: oracle_confirmed_pairs(&json),
        };
        let report = hauksbee_extract::DrcReport {
            clearance_mm: 0.2,
            primitive_count: 4,
            version_warning: Some("board format 20260206 is UNVALIDATED".into()),
            zone_pad_overlaps_suppressed: Some(0),
            excluded: Vec::new(),
            clearance_unjudged: hauksbee_extract::UnjudgedClearance::default(),
            findings: vec![
                short("GND", "+3V3", 1.0, 2.0),
                short("GND", "PYRO4_FIRE", 3.0, 4.0),
            ],
        };
        let mut structured = DrcStructured::from_report(&report);
        annotate_oracle_agreement(&mut structured, &oracle, &[false, false]);

        assert_eq!(structured.shorts[0].severity, "serious");
        assert_eq!(structured.shorts[1].severity, "note");
        let plain = crate::plain::plain_drc_structured(&structured);
        let confirmed = &plain.findings[0];
        let tool_only = &plain.findings[1];
        assert!(
            confirmed.what.contains("KiCad") && confirmed.what.contains("specific net pair"),
            "oracle agreement must be on the finding line: {}",
            confirmed.what
        );
        assert_eq!(confirmed.level, crate::plain::PlainLevel::Serious);
        assert_eq!(tool_only.level, crate::plain::PlainLevel::Note);
        assert!(
            tool_only.why.contains("TOOL-ONLY"),
            "the unmatched short must identify its evidence boundary: {}",
            tool_only.why
        );
    }

    fn short(net_a: &str, net_b: &str, x: f64, y: f64) -> hauksbee_extract::DrcFinding {
        let kind = hauksbee_extract::ViolationKind::Short;
        crate::result::test_finding(kind, (net_a, net_b), (x, y), (0.0, 0.2))
    }
}
