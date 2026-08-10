//! The `--drc` report: geometric copper-short and clearance detection over the
//! extracted board, rendered in the requested output mode. It also carries the
//! `kicad-cli` oracle cross-check (DRC-only) and, under `--strict`, exits non-zero
//! on a true short. Pure CLI glue over the extractor's DRC and the binder.

use std::path::Path;

use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

use crate::binder::bind_board;
use crate::result::{BindSummary, DrcStructured, JsonInputEvidence, JsonReport};

use super::{kicad_pro_clearance_rules, OutputMode};

/// Run geometric short / clearance detection, print it in `mode`, cross-check the
/// oracle when asked, then (under `strict`) exit non-zero on a true short. Returns
/// `Ok(())` on the non-gating paths; a strict short calls `std::process::exit(2)`.
#[allow(clippy::too_many_arguments)]
pub fn emit(
    board_path: &Path,
    board: &ExtractedBoard,
    text: &str,
    raw: &[u8],
    input_kind: crate::board_input::InputKind,
    altium_present: bool,
    lib: &ModelLibrary,
    reader_notes: &[String],
    mode: OutputMode,
    oracle: bool,
    strict: bool,
    verbose: bool,
    inputs: &[JsonInputEvidence],
) -> anyhow::Result<()> {
    let mut report = if altium_present {
        ExtractedBoard::altium_drc(raw)?
    } else {
        // KiCad 10 keeps class clearances in the sibling .kicad_pro. Resolve
        // concrete net names here (the CLI has both the board path and the
        // extracted netlist), then hand the DRC a pairwise clearance resolver.
        ExtractedBoard::drc_with_clearance_rules(
            text,
            kicad_pro_clearance_rules(board_path, board),
        )?
    };
    // Waivers, same semantics as `--check`: a short the board's owner overruled
    // for a stated reason must come out of THIS gate too, or the same board is
    // green under `--check --strict` and red under `--drc --strict`. The key
    // mirrors check.rs exactly: only a true short is waivable; a clearance
    // violation does not gate on its own, so it has nothing to be excused from.
    let mut waivers = super::check::load_waivers(board_path);
    let (kept, waived) = waivers.partition("drc", std::mem::take(&mut report.findings), |f| {
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
    report.findings = kept;
    // Zero routed copper (D2): a pads-only board passes the spacing check
    // vacuously, so `UNROUTED_COPPER_NOTE` is printed by the two surfaces that
    // read this flag, `--drc` and `--check`.
    let unrouted = !altium_present && super::unrouted_kicad_layout(text);
    let bound = bind_board(board, lib);
    let evidence = crate::evidence::BoardEvidence::from_bound(
        board,
        &bound.report,
        reader_notes,
        hauksbee_ir::evidence::RunDate::from_system_clock(),
    )?
    .with_input_artifact(board_path, raw, input_kind)?;
    let structured = DrcStructured::from_report(&report);
    let mut maps = evidence.maps_for_drc(&structured)?;
    let coverage = evidence.check_coverage_map("drc", "DRC input coverage")?;
    let coverage_undermined =
        coverage.status() == hauksbee_ir::evidence::EvidenceStatus::Undermined;
    if coverage.status() != hauksbee_ir::evidence::EvidenceStatus::Clean {
        maps.push(coverage);
    }
    let evidence = evidence.with_maps(maps);
    match mode {
        OutputMode::Json => {
            // Grouped DRC (Fix #8): shorts kept verbatim, clearance findings
            // grouped by (net_a, net_b, layer), at-limit separated from below-rule.
            let mut jr = JsonReport::new(&bound.name, BindSummary::from_report(&bound.report))
                .with_inputs(inputs)
                .with_evidence(&evidence);
            jr.drc = Some(structured.clone());
            if unrouted {
                jr.notes.push(crate::result::JsonNote {
                    kind: crate::result::JsonNoteKind::Coverage,
                    message: super::UNROUTED_COPPER_NOTE.to_string(),
                });
            }
            // A green verdict that quietly dropped findings would be worse than
            // no waivers at all, so the machine surface carries them too.
            jr.waived = waived.iter().cloned().map(Into::into).collect();
            println!("{}", jr.to_json());
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
            print!("{}", crate::render_drc_condensed(&structured, verbose));
        }
        OutputMode::Text => {
            if unrouted {
                println!("{}", super::UNROUTED_COPPER_NOTE);
            }
            // Grouped, honest DRC: one line per (net pair + cause) with a count,
            // and gap==rule labelled "at minimum clearance (no margin)" rather
            // than the wrong "below the spacing the board asks for" (Fix #8).
            print!("{}", structured.render());
        }
    }
    if oracle && mode != OutputMode::Json {
        print!("{}", oracle_cross_check(board_path, &report));
    }
    if !matches!(mode, OutputMode::Json) {
        print!("{}", evidence.render_plain());
        print!(
            "{}",
            super::check::render_waivers_scoped(&waived, &waivers, &["drc"], true)
        );
    }
    // Strict: any true short fails the gate (clearance-only does not). An
    // unvalidated board format (KiCad 10+) yields possibly-phantom shorts, so it
    // does not gate (the printed caveat tells the user to cross-check). Asked of
    // the machine findings, so this exit code and the `--junit`/`--sarif` files
    // count the same shorts.
    let would_gate = super::check::drc_gate_fails(&report);
    super::note_ungated_findings(strict, would_gate);
    if strict && would_gate {
        super::strict_gate_exit(mode, &super::drc_gate_items(&report));
    }
    // Copper is model-free: only an undermined DRC coverage claim (the input
    // could not honestly be inspected) or an undermined shorts map exits 3;
    // the bind state never poisons the copper surface.
    if strict
        && (coverage_undermined || crate::result::run_level_undermined(evidence.maps(), |_| false))
    {
        // Not `exit_invalid_for_analysis`: that helper annotates bind blockers,
        // and this surface names none (the bind gate does not reach copper,
        // whatever the board's own bind state is), so it would be a no-op
        // wrapper here.
        std::process::exit(crate::result::EXIT_INVALID_FOR_ANALYSIS);
    }
    Ok(())
}

/// Parse a version string like "10.0.3" (or "KiCad 9.0.3") into a comparable
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
/// version (a KiCad-10 cli is needed to read v20260206 boards). KiCad is NOT
/// bundled with hauksbee; this finds an existing install. Returns (path, version).
pub(crate) fn find_kicad_cli() -> Option<(String, String)> {
    let mut candidates: Vec<String> = vec!["kicad-cli".to_string()];
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
    let mut best: Option<(String, String, (u32, u32, u32))> = None;
    for c in candidates {
        let Ok(out) = std::process::Command::new(&c).arg("version").output() else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let ver = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let parsed = parse_version(&ver);
        if best.as_ref().is_none_or(|b| parsed > b.2) {
            best = Some((c, ver, parsed));
        }
    }
    best.map(|(p, v, _)| (p, v))
}

/// Cross-check hauksbee's geometric DRC against KiCad's own `kicad-cli pcb drc`,
/// so a copper finding is self-confirming without running a second tool by hand.
/// Honest about the two tools' different scopes (KiCad's violation count includes
/// clearance / annular-ring / etc.), and flags the one case that matters: hauksbee
/// reporting a short the oracle does not (a likely hauksbee false positive).
fn oracle_cross_check(board: &Path, report: &hauksbee_extract::DrcReport) -> String {
    let Some((cli, ver)) = find_kicad_cli() else {
        return format!(
            "\noracle: no kicad-cli found (PATH or /Applications). Install KiCad to \
             cross-check geometric DRC; see {}.\n",
            hauksbee_ir::docs_url("docs/cosim/ORACLES.md")
        );
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
        return format!("\noracle (kicad-cli {ver}): failed to launch.\n");
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
        return format!(
            "\noracle (kicad-cli {ver}): could not load this board{}. A KiCad-10 (>= 10.0) \
             cli is required for v20260206 boards.\n",
            if why.trim().is_empty() {
                String::new()
            } else {
                format!(" ({})", why.trim())
            }
        );
    };
    let _ = std::fs::remove_file(&tmp);
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    let violations = v.get("violations").and_then(|x| x.as_array());
    let nviol = violations.map_or(0, |a| a.len());
    let nunconn = v
        .get("unconnected_items")
        .and_then(|x| x.as_array())
        .map_or(0, |a| a.len());
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
    let verdict = if shorts == 0 && confirmed == 0 {
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
    format!(
        "\noracle (kicad-cli {ver}): {confirmed} touching-copper violation(s), {nviol} total DRC \
         violation(s), {nunconn} unconnected.\n\
         hauksbee: {shorts} short(s), {clear} clearance. -> {verdict}.\n"
    )
}
