//! Downstream source-compatibility for public function signatures that predate
//! the optional Eagle companion input.

use std::path::{Path, PathBuf};

use hauksbee_engine::board_input::InputKind;
use hauksbee_engine::reports::OutputMode;
use hauksbee_engine::result::JsonInputEvidence;
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

type TuiRun = fn(&Path, &str, Option<&Path>, Option<PathBuf>) -> anyhow::Result<()>;
type TuiBuild = fn(&Path, &str, Option<&Path>) -> anyhow::Result<hauksbee_engine::tui::AppState>;
type DrcGateItems = fn(&hauksbee_extract::DrcReport) -> Vec<String>;
type GatherFindings = fn(
    &Path,
    &ExtractedBoard,
    &str,
    &[u8],
    bool,
    &ModelLibrary,
) -> anyhow::Result<Vec<hauksbee_engine::result::JsonFinding>>;
type CombinedJson = fn(
    &Path,
    &ExtractedBoard,
    &str,
    &[u8],
    InputKind,
    bool,
    &ModelLibrary,
    &[String],
    bool,
    &[JsonInputEvidence],
) -> anyhow::Result<()>;
type CheckEmit = fn(
    &Path,
    &ExtractedBoard,
    &str,
    &[u8],
    InputKind,
    bool,
    &ModelLibrary,
    &[String],
    OutputMode,
    bool,
    bool,
    &[JsonInputEvidence],
) -> anyhow::Result<()>;
type DrcEmit = fn(
    &Path,
    &ExtractedBoard,
    &str,
    &[u8],
    InputKind,
    bool,
    &ModelLibrary,
    &[String],
    OutputMode,
    bool,
    bool,
    bool,
    &[JsonInputEvidence],
) -> anyhow::Result<()>;

#[test]
fn established_public_function_items_keep_their_original_types() {
    let _: TuiRun = hauksbee_engine::tui::run;
    let _: TuiBuild = hauksbee_engine::tui::build_state;
    let _: DrcGateItems = hauksbee_engine::reports::drc_gate_items;
    let _: GatherFindings = hauksbee_engine::reports::check::gather_findings;
    let _: CombinedJson = hauksbee_engine::reports::check::emit_combined_json;
    let _: CheckEmit = hauksbee_engine::reports::check::emit;
    let _: DrcEmit = hauksbee_engine::reports::drc::emit;
}
