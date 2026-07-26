//! One module per report family. Each builds the **structured** finding type (the
//! honesty layer in [`crate::result`]) and renders it in exactly one of the three
//! output surfaces. The per-flag `if json / else if plain / else` triplication
//! that used to live inline in `cmd_run` collapses into a single `match` on
//! [`OutputMode`] per report, and the ~700-line `cmd_run` becomes a thin
//! dispatcher: pick the mode once, call the right report's `emit`.
//!
//! Rendering itself is unchanged, each `emit` delegates to the existing
//! renderers (`DrcStructured::render`, `plain_*`, `JsonReport`, the extract-crate
//! text renderers) so the output stays byte-for-byte what it was.

pub mod ac;
pub mod ampacity;
pub mod bind;
pub mod check;
pub mod cosim;
pub mod drc;
pub mod lint;
pub mod si;
pub mod thermal;
pub mod usb_c;

use std::path::Path;

use hauksbee_extract::ExtractedBoard;

/// The output surface a report renders into, resolved once from the CLI flags so
/// each report matches it a single time instead of re-checking `--json`/`--plain`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputMode {
    /// The default box-drawing / expert text tables.
    Text,
    /// Plain-language prose (`--plain` / `--explain`).
    Plain,
    /// Machine-readable JSON (`--json`).
    Json,
}

impl OutputMode {
    /// Resolve the surface from the two CLI flags. `--json` wins over `--plain`
    /// (a machine consumer never wants prose), matching the historical precedence.
    pub fn from_flags(json: bool, plain: bool) -> Self {
        if json {
            OutputMode::Json
        } else if plain {
            OutputMode::Plain
        } else {
            OutputMode::Text
        }
    }
}

/// Read project-file netclass clearances and resolve them to this board's
/// concrete net names. KiCad 10 stores this in the sibling `.kicad_pro` rather
/// than the `.kicad_pcb`; missing/malformed project files simply leave DRC on
/// the board/default rules. Shared by the `--drc`, `--check` and combined-`--json`
/// reports.
pub fn kicad_pro_clearance_rules(
    board_path: &Path,
    board: &ExtractedBoard,
) -> Option<hauksbee_extract::ClearanceRules> {
    let text = std::fs::read_to_string(board_path.with_extension("kicad_pro")).ok()?;
    hauksbee_extract::clearance_rules_from_kicad_pro(
        &text,
        board.nets.iter().map(|n| n.name.as_str()),
    )
}

/// Strict-mode predicate for the connectivity/resource lint: any high/medium
/// finding fails the gate.
pub fn lint_fails(report: &hauksbee_extract::NetLintReport) -> bool {
    use hauksbee_extract::Severity;
    report
        .findings
        .iter()
        .any(|f| matches!(f.severity, Severity::High | Severity::Medium))
}

/// Strict-mode predicate for the SI report: any real finding (high/medium/low,
/// but not the informational computed-value notes) fails the gate.
pub fn si_fails(report: &hauksbee_extract::SiReport) -> bool {
    report.finding_count() > 0
}
