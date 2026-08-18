//! Callback and data contracts supplied by an embedding application to a
//! front door.
//!
//! Each alias and struct here is one capability the web UI can offer: analyze
//! an uploaded board, launch a live session, run a schematic check, extract a
//! model from a datasheet. The front door holds them as trait objects and
//! never learns which crate provided them; the engine implements them and
//! never learns it is being served over a websocket. Keeping only the
//! signatures here is what lets both sides compile without each other.

use std::sync::Arc;

use crate::engine::Engine;

/// Analyze an uploaded board: `(file_name, board_bytes) -> JSON report string`.
pub type Analyzer = Arc<dyn Fn(&str, &[u8]) -> String + Send + Sync>;

/// Analyze a board and optional firmware.
pub type FirmwareAnalyzer = Arc<dyn Fn(&str, &[u8], Option<(&str, &[u8])>) -> String + Send + Sync>;

/// Firmware-aware analysis with an optional companion schematic.
pub type SchematicAnalyzer =
    Arc<dyn Fn(&str, &[u8], Option<(&str, &[u8])>, Option<(&str, &[u8])>) -> String + Send + Sync>;

/// Run checks composed by a web builder.
pub type CheckRunner =
    Arc<dyn Fn(&str, &[u8], Option<(&str, &[u8])>, &str) -> String + Send + Sync>;

/// Schematic-aware checks runner.
pub type SchematicCheckRunner = Arc<
    dyn Fn(&str, &[u8], Option<(&str, &[u8])>, Option<(&str, &[u8])>, &str) -> String + Send + Sync,
>;

/// Everything a successful live-launch callback hands to the serving runtime.
pub struct LiveLaunch {
    pub engine: Box<dyn Engine>,
    pub board_name: String,
    /// `(file name, board text)` for the board-file route.
    pub board_file: Option<(String, String)>,
    /// Resources that must remain alive for the session's lifetime.
    pub keepalive: Option<Box<dyn std::any::Any + Send>>,
}

/// Build a live engine for an uploaded board and optional firmware.
pub type LiveLauncher =
    Arc<dyn Fn(&str, &[u8], Option<(&str, &[u8])>) -> Result<LiveLaunch, String> + Send + Sync>;

/// Live launcher which additionally receives an optional schematic.
pub type SchematicLiveLauncher = Arc<
    dyn Fn(&str, &[u8], Option<(&str, &[u8])>, Option<(&str, &[u8])>) -> Result<LiveLaunch, String>
        + Send
        + Sync,
>;

/// Report optional-dependency status as JSON.
pub type DepsStatus = Arc<dyn Fn() -> String + Send + Sync>;

/// Run one dependency install while streaming progress lines.
pub type DepInstaller = Arc<dyn Fn(&str, &mut dyn FnMut(&str)) -> Result<(), String> + Send + Sync>;

/// Everything one datasheet extraction needs.
pub struct DatasheetJob {
    pub pdf_name: String,
    pub pdf: Vec<u8>,
    pub reference: String,
    pub part: String,
    pub kind: String,
    /// Empty selects the embedding application's default model.
    pub model: String,
}

/// Whether datasheet extraction is ready, encoded as JSON.
pub type DatasheetReady = Arc<dyn Fn() -> String + Send + Sync>;

/// Run one extraction while streaming progress; returns a reviewable model card.
pub type DatasheetExtractor =
    Arc<dyn Fn(DatasheetJob, &mut dyn FnMut(&str)) -> Result<String, String> + Send + Sync>;

/// Save a reviewed model card into the user's model library.
pub type DatasheetSaver = Arc<dyn Fn(&str, &str, &str) -> Result<String, String> + Send + Sync>;

/// Validate a model without writing anything.
pub type DatasheetChecker = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

/// Draft a board-local extension from an already resolved model.
pub type ModelDrafter = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

/// The datasheet-extraction backend supplied by the analysis application.
pub struct DatasheetHooks {
    pub ready: DatasheetReady,
    pub extract: DatasheetExtractor,
    pub save: DatasheetSaver,
    pub check: DatasheetChecker,
    pub spice_check: DatasheetChecker,
    pub draft: ModelDrafter,
}

/// Engine-backed hooks used by browser tool panels.
pub struct ToolHooks {
    pub deps_status: DepsStatus,
    pub install: DepInstaller,
    pub datasheet: DatasheetHooks,
}
