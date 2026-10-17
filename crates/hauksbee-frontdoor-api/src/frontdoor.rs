//! Callback and data contracts supplied by an embedding application to a
//! front door.
//!
//! Each alias and struct here is one capability the web UI can offer: analyze
//! an uploaded board, launch a live session, run a check, simulate a SPICE
//! deck, extract a model from a datasheet. The front door holds them as trait objects and
//! never learns which crate provided them; the engine implements them and
//! never learns it is being served over a websocket. Keeping only the
//! signatures here is what lets both sides compile without each other.

use std::sync::Arc;

use crate::engine::Engine;

/// Analysis of a board with its optional firmware and companion schematic.
pub type SchematicAnalyzer =
    Arc<dyn Fn(&str, &[u8], Option<(&str, &[u8])>, Option<(&str, &[u8])>) -> String + Send + Sync>;

/// One named file supplied through the local web app.
#[derive(Debug, Clone)]
pub struct NamedUpload {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// The complete design-input bundle shared by browser analysis, Checks, and Live Sim.
///
/// Keeping this contract in one type prevents a surface from quietly
/// dropping manufacturing identity or model context accepted by another.
#[derive(Debug, Clone)]
pub struct DesignUpload {
    pub board: NamedUpload,
    pub firmware: Option<NamedUpload>,
    pub schematic: Option<NamedUpload>,
    pub bom: Option<NamedUpload>,
    pub placement: Option<NamedUpload>,
    pub variant: Option<NamedUpload>,
    pub asbuilt: Option<NamedUpload>,
    pub models: Vec<NamedUpload>,
}

/// Analyze the complete browser design-input bundle.
pub type DesignAnalyzer = Arc<dyn Fn(DesignUpload) -> String + Send + Sync>;

/// Simulate a raw SPICE deck: `(file_name, deck_text) -> JSON result`.
///
/// On refusal it returns the reason instead. The result carries the same CSV bytes
/// `hauksbee sim` writes, so the browser needs no CSV writer of its own.
pub type DeckSimulator = Arc<dyn Fn(&str, &str) -> Result<String, String> + Send + Sync>;

/// Run browser-composed checks against the complete design-input bundle.
pub type DesignCheckRunner = Arc<dyn Fn(DesignUpload, &str) -> String + Send + Sync>;

/// The spec body that asks a [`DesignCheckRunner`] for the board's starter supplies.
///
/// The answer is what `hauksbee-ci init --json` prints, not a run. The marker is
/// a TOML comment, so it is also a harmless spec to any runner that does not know it.
pub const SCAFFOLD_SPEC: &str = "# hauksbee:scaffold";

/// The spec body that asks a [`DesignCheckRunner`] for the assertion catalogue
/// (what `hauksbee-ci explain --json` prints), with no board involved.
pub const CATALOG_SPEC: &str = "# hauksbee:catalog";

/// The spec body that asks a [`DesignCheckRunner`] for the editor's JSON Schema of a
/// spec, with the catalogue's words attached (`hauksbee-ci explain --schema`).
pub const SCHEMA_SPEC: &str = "# hauksbee:schema";

/// The first line of a spec body that asks a [`DesignCheckRunner`] to lint the TOML
/// that follows it (`hauksbee-ci lint`); no board is involved.
pub const LINT_SPEC: &str = "# hauksbee:lint";

/// The first line of a spec body that asks a [`DesignCheckRunner`] to read one
/// assertion's value off a single run (`hauksbee-ci measure`).
///
/// The line is the marker, a space and the assertion's index; the spec fragment
/// follows as for a check. It is a TOML comment, so the staged spec stays valid.
pub const MEASURE_SPEC: &str = "# hauksbee:measure";

/// Everything a successful live-launch callback hands to the serving runtime.
pub struct LiveLaunch {
    pub engine: Box<dyn Engine>,
    pub board_name: String,
    /// `(file name, board text)` for the board-file route.
    pub board_file: Option<(String, String)>,
    /// The drawable `KiCad` layout text, set only when `board_file` is some
    /// other text (an Eagle file) the viewer cannot
    /// draw. Served at `/boards/<file name>/layout`.
    pub layout_file: Option<String>,
    /// Resources that must remain alive for the session's lifetime.
    pub keepalive: Option<Box<dyn std::any::Any + Send>>,
}

/// Build a live engine for an uploaded board, its optional firmware and an
/// optional companion schematic.
pub type SchematicLiveLauncher = Arc<
    dyn Fn(&str, &[u8], Option<(&str, &[u8])>, Option<(&str, &[u8])>) -> Result<LiveLaunch, String>
        + Send
        + Sync,
>;

/// Launch Live Sim from the same complete bundle used by report and Checks.
pub type DesignLiveLauncher = Arc<dyn Fn(DesignUpload) -> Result<LiveLaunch, String> + Send + Sync>;

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
    /// The backend for this extraction only; empty keeps the configured one.
    pub backend: String,
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

/// A settings request taking one string argument; Ok(settings payload) or an error.
pub type SettingsAction = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

/// A backend check that streams progress lines; Ok(reply) or an error.
pub type SettingsProbe = Arc<dyn Fn(&mut dyn FnMut(&str)) -> Result<String, String> + Send + Sync>;

/// The extraction settings page's backend: read, save, apply a preset, and a
/// live connectivity check.
pub struct ExtractSettingsHooks {
    /// GET: the whole settings payload as JSON.
    pub get: Arc<dyn Fn() -> String + Send + Sync>,
    /// PUT: the request body (a config JSON object) -> the settings payload,
    /// or an error message.
    pub save: SettingsAction,
    /// POST preset/{id} -> the settings payload, or an error message.
    pub preset: SettingsAction,
    /// Run the configured backend on a one-line prompt while streaming
    /// progress lines; Ok(reply).
    pub test: SettingsProbe,
}

/// The datasheet-extraction backend supplied by the analysis application.
pub struct DatasheetHooks {
    pub ready: DatasheetReady,
    pub extract: DatasheetExtractor,
    pub save: DatasheetSaver,
    pub check: DatasheetChecker,
    pub spice_check: DatasheetChecker,
    pub draft: ModelDrafter,
    pub settings: ExtractSettingsHooks,
}

/// Engine-backed hooks used by browser tool panels.
pub struct ToolHooks {
    pub deps_status: DepsStatus,
    pub install: DepInstaller,
    pub datasheet: DatasheetHooks,
}
