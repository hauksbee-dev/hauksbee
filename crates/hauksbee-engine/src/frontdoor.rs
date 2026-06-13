//! The web front-door analysis: take an uploaded board file's bytes, run every
//! static check, and return a single JSON payload a browser can render with no
//! CLI involved.
//!
//! This is the "drop your board, get a report" backend. It reuses the exact same
//! analysis as the CLI (`ExtractedBoard` extraction + the DRC / lint / SI /
//! resource checks + the plain-language [`crate::plain`] templates), so the web
//! report and the terminal report can never disagree. The HTTP plumbing lives in
//! `hauksbee-server`; this module is pure (bytes in, JSON string out) so it has
//! no web dependency and is unit-testable.

use serde::Serialize;

use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

use crate::binder::bind_board;
use crate::plain::{plain_drc, plain_netlint, plain_si, PlainFinding, PlainLevel, PlainReport};

/// A finding, in the shape the browser renders.
#[derive(Debug, Clone, Serialize)]
pub struct WebFinding {
    /// "serious" | "warning" | "note".
    pub level: String,
    pub what: String,
    pub why: String,
    pub fix: String,
}

impl From<&PlainFinding> for WebFinding {
    fn from(f: &PlainFinding) -> Self {
        WebFinding {
            level: match f.level {
                PlainLevel::Serious => "serious",
                PlainLevel::Warning => "warning",
                PlainLevel::Note => "note",
            }
            .to_string(),
            what: f.what.clone(),
            why: f.why.clone(),
            fix: f.fix.clone(),
        }
    }
}

/// One analysis section (one check family) as the browser sees it.
#[derive(Debug, Clone, Serialize)]
pub struct WebSection {
    /// "Copper spacing (DRC)", "Connectivity", "Signal integrity".
    pub title: String,
    pub verdict: String,
    pub findings: Vec<WebFinding>,
}

impl WebSection {
    fn from_plain(title: &str, p: &PlainReport) -> Self {
        WebSection {
            title: title.to_string(),
            verdict: p.verdict(),
            findings: p.findings.iter().map(WebFinding::from).collect(),
        }
    }
}

/// A component for the simple 2D footprint map (positions in board mm).
#[derive(Debug, Clone, Serialize)]
pub struct WebComponent {
    pub reference: String,
    pub value: String,
    pub x: f64,
    pub y: f64,
    pub rot: f64,
}

/// The whole payload sent back to the browser after an upload.
#[derive(Debug, Clone, Serialize)]
pub struct WebReport {
    pub ok: bool,
    /// Set when extraction failed; `ok` is false and the rest is empty.
    pub error: Option<String>,
    pub board_name: String,
    pub file_name: String,
    pub num_components: usize,
    pub num_nets: usize,
    /// The single overall headline across all sections.
    pub headline: String,
    /// Total serious findings across all sections.
    pub serious: usize,
    /// Total findings across all sections.
    pub total: usize,
    pub sections: Vec<WebSection>,
    /// Components with a known position, for the 2D map (empty for netlist-only
    /// inputs that carry no layout).
    pub components: Vec<WebComponent>,
}

/// Run the full front-door analysis on an uploaded board file.
///
/// `file_name` is used only for display and to disambiguate a `.kicad_sch` (which
/// the extractor handles by content here too). `contents` is the file's text.
pub fn analyze(file_name: &str, contents: &str) -> WebReport {
    let board = match ExtractedBoard::from_auto(contents) {
        Ok(b) => b,
        Err(e) => {
            return WebReport {
                ok: false,
                error: Some(format!(
                    "Could not read this board file: {e}. Supported: KiCad .kicad_pcb / .kicad_sch, Eagle .brd, IPC-D-356 .d356, or a gerber zip."
                )),
                board_name: String::new(),
                file_name: file_name.to_string(),
                num_components: 0,
                num_nets: 0,
                headline: "Could not read the file.".to_string(),
                serious: 0,
                total: 0,
                sections: Vec::new(),
                components: Vec::new(),
            };
        }
    };

    let lib = ModelLibrary::builtin_with_user_dirs(&[]);

    // DRC reads copper geometry from the raw text (gerbers/KiCad layout).
    let drc = ExtractedBoard::drc(contents).unwrap_or_default();
    let drc_plain = plain_drc(&drc);

    // Lint = connectivity checks + strap lint + MCU resource conflicts, the same
    // bundle the `--lint` CLI surface assembles.
    let mut lint = board.net_lint();
    let straps = crate::checks::straps::strap_lint(&board, &lib);
    lint.findings.extend(straps.findings);
    lint.findings.extend(board.resource_conflicts().findings);
    let lint_plain = plain_netlint(&lint);

    // SI needs the layout text for the geometry-bearing checks.
    let si = board.si_checks(Some(contents));
    let si_plain = plain_si(&si);

    let sections = vec![
        WebSection::from_plain("Copper spacing (DRC)", &drc_plain),
        WebSection::from_plain("Connectivity & wiring", &lint_plain),
        WebSection::from_plain("Signal integrity", &si_plain),
    ];

    let serious: usize = sections.iter().map(|s| {
        s.findings.iter().filter(|f| f.level == "serious").count()
    }).sum();
    let total: usize = sections.iter().map(|s| s.findings.len()).sum();

    let headline = overall_headline(total, serious);

    // Bind to count nets/components consistently with the report panel.
    let bound = bind_board(&board, &lib);

    let components = board
        .components
        .iter()
        .filter_map(|c| {
            c.position.map(|(x, y, rot)| WebComponent {
                reference: c.reference.clone(),
                value: c.value.clone(),
                x,
                y,
                rot,
            })
        })
        .collect();

    WebReport {
        ok: true,
        error: None,
        board_name: board.name.clone(),
        file_name: file_name.to_string(),
        num_components: board.components.len(),
        num_nets: bound.net_names.len(),
        headline,
        serious,
        total,
        sections,
        components,
    }
}

/// The single line at the top of the web report.
fn overall_headline(total: usize, serious: usize) -> String {
    if total == 0 {
        return "Looks healthy: the static checks found no problems.".to_string();
    }
    let issues = if total == 1 { "issue" } else { "issues" };
    if serious == 0 {
        format!("{total} {issues} found, none serious. Worth a look before you order boards.")
    } else {
        format!("{total} {issues} found, {serious} serious. Fix the serious ones before ordering boards.")
    }
}

/// Serialize an [`analyze`] result to a JSON string for the HTTP layer.
pub fn analyze_json(file_name: &str, contents: &str) -> String {
    let report = analyze(file_name, contents);
    serde_json::to_string(&report).unwrap_or_else(|e| {
        format!("{{\"ok\":false,\"error\":\"failed to serialize report: {e}\"}}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORTED: &str = include_str!(
        "../../hauksbee-ci/examples/boards/boot_gate.kicad_pcb"
    );

    #[test]
    fn analyze_shorted_board_reports_serious() {
        let r = analyze("boot_gate.kicad_pcb", SHORTED);
        assert!(r.ok, "extraction should succeed: {:?}", r.error);
        assert!(r.serious > 0, "boot_gate has copper shorts -> serious findings");
        assert!(r.headline.to_lowercase().contains("serious"));
        // The DRC section specifically should carry the shorts.
        let drc = r.sections.iter().find(|s| s.title.contains("DRC")).unwrap();
        assert!(drc.findings.iter().any(|f| f.level == "serious"));
        // Every finding has all three plain fields.
        for s in &r.sections {
            for f in &s.findings {
                assert!(!f.what.is_empty() && !f.why.is_empty() && !f.fix.is_empty());
            }
        }
    }

    #[test]
    fn analyze_emits_component_positions_for_a_layout() {
        let r = analyze("boot_gate.kicad_pcb", SHORTED);
        assert!(!r.components.is_empty(), "a KiCad layout has placed parts");
    }

    #[test]
    fn analyze_garbage_returns_a_friendly_error() {
        let r = analyze("nope.txt", "this is not a board file at all");
        assert!(!r.ok);
        assert!(r.error.is_some());
        // The JSON wrapper still produces valid JSON.
        let json = analyze_json("nope.txt", "garbage");
        assert!(json.contains("\"ok\":false"));
    }

    #[test]
    fn analyze_json_is_valid_json() {
        let json = analyze_json("boot_gate.kicad_pcb", SHORTED);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ok"], true);
        assert!(v["sections"].as_array().unwrap().len() >= 3);
    }
}
