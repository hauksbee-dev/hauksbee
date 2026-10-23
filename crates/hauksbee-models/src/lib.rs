//! `hauksbee-models`, PCB component model library.
//!
//! Given a component identified by `lib_id`, value, footprint, or part-number,
//! this crate resolves a simulation model definition. Physics arrives by
//! several authoring routes (built-in DB, installed model packs, LLM
//! datasheet extraction, hand-written behavioural models, user SPICE) that
//! map onto six explicit priority layers ([`SourceLayer`]):
//!
//! | layer                                    | priority |
//! |------------------------------------------|----------|
//! | built-in db                              | 0        |
//! | installed packs                          | 10       |
//! | user model dir (`~/.hauksbee/models`)    | 20       |
//! | user config dir (`~/.config/hauksbee/models`) | 25  |
//! | `--models-dir`                           | 30       |
//! | user SPICE cards                         | 40       |
//!
//! Semantic [`ModelSourceTier`] wins first. Storage layer then breaks a
//! same-tier tie; *within* a layer the specificity score breaks the tie (see
//! [`matcher`]). Same-layer conflicts between two different
//! packs are reported loudly at load, naming both packs.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-models/README.md (the
//! crate tour) and docs/how-and-why/hauksbee-models/pack.md (resolution
//! layering).
//!
//! # Quick start
//!
//! ```rust
//! use hauksbee_models::{ModelLibrary, ComponentQuery};
//!
//! let lib = ModelLibrary::builtin();
//! let q = ComponentQuery::new(
//!     Some("Device:R".to_string()),
//!     Some("10k".to_string()),
//!     None,
//! );
//! let res = lib.resolve(&q);
//! println!("{}", res);
//! ```

/// The one wording of how a rating's scalar is chosen among its printed rows: the
/// extraction prompt (`KEY_GUIDE`, `RATINGS_RULES`) and the validator's message
/// all embed it, so they cannot disagree.
macro_rules! standard_condition {
    () => {
        "the row at the standard condition (25 °C, the standard test board), else the least favourable row the part number allows"
    };
}

pub mod audio_roles;
pub mod behavioral;
pub mod boot_mode;
pub mod card_coverage;
pub mod card_provenance;
pub mod card_scope;
pub mod card_templates;
pub mod card_trust;
mod check;
pub mod chip_size;
pub mod companion_docs;
mod conventions;
pub mod datasheet;
pub mod datasheet_discovery;
pub mod datasheet_fetch;
pub mod derating;
pub mod ds_toolkit;
pub mod extract_admission;
pub mod extract_attempts;
pub mod extract_cache;
pub mod extract_capacity;
pub mod extract_config;
pub mod extract_examples;
pub mod extract_harness;
pub mod extract_messages;
pub mod extract_output;
pub mod extract_plan;
pub mod extract_progress;
pub mod extract_stage_messages;
pub mod extract_trace;
pub mod extract_usage;
pub mod logic_spec;
pub mod matcher;
pub mod mcu_modules;
pub mod netclass;
pub mod output_load;
pub mod pack;
pub mod package;
pub mod param_names;
pub mod peripheral_limits;
pub mod pin_roles;
pub mod pin_rules;
pub mod pin_table;
mod plausibility;
pub mod profile;
pub mod ranges;
pub mod rating_basis;
pub mod refdes;
pub mod schema;
pub mod sensor_spec;
pub mod spice_input;
mod units;
pub mod unmodelled;
pub mod validation;
pub mod value;
pub mod variants;

use std::path::{Path, PathBuf};

use regex::Error as RegexError;
use thiserror::Error;

use hauksbee_ir::evidence::{
    ModelLayer, ModelSource, ModelSourceTier, ModelUncertainty, ModelValidation,
};

pub use logic_spec::{Logic, LogicExpr, LogicSpecError, ValidatedLogic};
pub use matcher::ComponentQuery;
pub use pack::{Pack, PackError, PackManifest, PackRecord, PackStore, Provenance};
pub use pin_rules::{InferredRole, PinRule, PinRuleTable};
pub use profile::{LoadProfile, Segment};
pub use schema::{
    abs_max_citation, basis_names_abs_max, declared_pin_ceiling_v, ComponentKind, Doubt, DoubtArea,
    DriveSelect, EnvelopeSeverity, ModelCoverage, ModelEntry, ModelSourceReference,
    OperatingEnvelope, OutputDrive, OutputLevel, OutputPoint, Params, PeripheralPower,
    PeripheralSpec, ResetSpec, StrapInternalPull, StrapLevel, StrapPin,
};
pub use sensor_spec::{
    Bus, Encoding, ProtocolStyle, RegisterSpec, Sensor, SensorSpec, SensorSpecError,
};
pub use spice_input::SpiceCard;
pub use unmodelled::{UnmodelledNote, UnmodelledPart, UnmodelledTable};

// ── Embedded database files ───────────────────────────────────────────────────

macro_rules! builtin_db {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_str!(concat!("../db/", $name, ".toml")))),*]
    };
}

/// All built-in `[[models]]` database files embedded at compile time.
static BUILTIN_TOML_FILES: &[(&str, &str)] = builtin_db![
    "passives",
    "diodes",
    "bjt",
    "mosfet",
    "opamp_comparator",
    "analog_switch",
    "digital",
    "dac_adc",
    "vreg",
    "timer",
    "power_ics",
    "mcu",
    "memory",
    "memory_spi",
    "sensors_motion",
    "sensors_env",
    "sensors_env2",
    "sensors_power",
    "converters",
    "interface_serial",
    "interface_io",
    "analog",
    "discrete_active",
    "discrete_diodes",
    "power_regs",
    "drivers",
    "mcu_extra",
    "mcu_extra2",
    "logic",
    "misc",
    "ignore",
];

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("TOML parse error in '{source}': {error}")]
    TomlParse {
        source: String,
        #[source]
        error: toml::de::Error,
    },

    #[error("invalid regex in entry '{id}': {error}")]
    InvalidRegex { id: String, error: RegexError },

    #[error("validation failed for '{id}': {messages}")]
    ValidationFailed { id: String, messages: String },

    #[error("pin-rule error in '{file}': {message}")]
    PinRules { file: String, message: String },

    #[error("directory does not exist: {dir}")]
    MissingDir { dir: String },

    #[error(
        "no model cards in {dir}: it holds no '*.toml' card and no pack \
         (a directory with pack.toml beside a models/ folder). Point \
         --models-dir at the directory the cards are actually in"
    )]
    NoCards { dir: String },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),
}

/// Pack directories sitting directly inside `dir`, sorted by path so two runs
/// load the same cards in the same order.
fn child_packs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut packs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("pack.toml").is_file() && p.join("models").is_dir())
        .collect();
    packs.sort();
    packs
}

/// Whether `text` is one of the shipped builtin db files, verbatim.
///
/// Its cards were reviewed against their footprints when they were curated, so the
/// machine-draft rules (`validation::draft_pin_map_problems`) do not apply.
#[must_use]
pub fn is_builtin_db(text: &str) -> bool {
    BUILTIN_TOML_FILES.iter().any(|(_, src)| *src == text)
}

/// Whether [`ModelLibrary::load_models_dir`] would find anything to load in `dir`.
///
/// That is a pack, child packs, or a card file. An empty output directory (a
/// first `models extract --out-dir`) holds none and is no models dir yet.
#[must_use]
pub fn holds_cards(dir: &Path) -> bool {
    (dir.join("pack.toml").is_file() && dir.join("models").is_dir())
        || holds_toml(dir)
        || !child_packs(dir).is_empty()
}

/// Whether `dir` holds at least one `*.toml` file directly.
fn holds_toml(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            e.path().extension().and_then(|x| x.to_str()) == Some("toml") && e.path().is_file()
        })
    })
}

// ── Source layers ─────────────────────────────────────────────────────────────

/// The explicit resolution priority layer an entry was loaded from.
///
/// A higher-priority layer beats a lower one *regardless of specificity*;
/// specificity only breaks ties within a layer, so user-over-builtin holds by
/// construction rather than by an entry happening to score more specifically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceLayer {
    /// The embedded `db/*.toml` database.
    Builtin,
    /// An installed model pack (`~/.hauksbee/packs/<name>@<version>/`).
    Pack,
    /// The legacy standing user model directory (`~/.hauksbee/models`), where
    /// datasheet extraction writes.
    UserDir,
    /// The user's own config model directory (`~/.config/hauksbee/models`),
    /// ABOVE `UserDir` so a hand-corrected model there deterministically wins
    /// over an auto-extracted one of the same id in `~/.hauksbee/models`.
    UserConfigDir,
    /// An explicit `--models-dir` flag.
    ModelsDirFlag,
    /// A user SPICE `.model`/`.subckt` card (always an exact name match, so
    /// it is checked before the entry scan).
    Spice,
}

impl SourceLayer {
    /// Layer priority integer: builtin=0, pack=10, user dir=20,
    /// user config dir=25, `--models-dir`=30, user SPICE=40.
    #[must_use]
    pub const fn priority(self) -> u32 {
        match self {
            Self::Builtin => 0,
            Self::Pack => 10,
            Self::UserDir => 20,
            Self::UserConfigDir => 25,
            Self::ModelsDirFlag => 30,
            Self::Spice => 40,
        }
    }

    /// Short name for reports.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Pack => "pack",
            Self::UserDir => "user-dir",
            Self::UserConfigDir => "user-config-dir",
            Self::ModelsDirFlag => "models-dir",
            Self::Spice => "spice",
        }
    }

    /// The coarse source string [`Resolution::source`] keeps for existing
    /// consumers: every user layer collapses to the historical `"user"`.
    #[must_use]
    pub const fn coarse_name(self) -> &'static str {
        match self {
            Self::UserDir | Self::UserConfigDir | Self::ModelsDirFlag => "user",
            other => other.name(),
        }
    }
}

impl std::fmt::Display for SourceLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}({})", self.name(), self.priority())
    }
}

// ── Resolution result ─────────────────────────────────────────────────────────

/// How confident the library is in a resolved model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    /// An exact, unambiguous match (e.g. exact `lib_id` + value + MPN).
    Exact,
    /// Matched by family rules (e.g. value regex catches the whole `BC84x` family).
    Family,
    /// Heuristic / partial match (e.g. only footprint matched).
    Guessed,
    /// No matching entry found.
    Unresolved,
}

impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Exact => "exact",
            Self::Family => "family",
            Self::Guessed => "guessed",
            Self::Unresolved => "unresolved",
        })
    }
}

/// The result of resolving a component query.
#[derive(Debug, Clone)]
pub struct Resolution {
    /// The resolved model entry, or `None` when unresolved.
    pub model: Option<ModelEntry>,
    /// How confident we are in this resolution.
    pub confidence: Confidence,
    /// The query that produced this result (for diagnostics).
    pub query: ComponentQuery,
    /// Coarse source string kept for existing consumers: `"builtin"`,
    /// `"pack"`, `"user"` (user dir or --models-dir), or `"spice"`.
    pub source: Option<String>,
    /// The explicit priority layer the winning entry came from.
    pub layer: Option<SourceLayer>,
    /// Where within the layer: the db/pack/file name that shipped the entry
    /// (e.g. `"digital"`, `"acme-sensors@1.2.0"`, a user file stem).
    pub origin: Option<String>,
    /// Canonical source/validation/uncertainty record consumed by the evidence
    /// spine. `None` only for an unresolved/open component.
    pub provenance: Option<ModelSource>,
    /// Human-auditable datasheet/vendor/measurement references retained from
    /// `[models.source]`. These are separate from numeric uncertainty because a
    /// citation identifies bytes/sections while uncertainty states what the
    /// model is allowed to claim.
    pub references: Vec<schema::ModelSourceReference>,
    /// Other entry ids that matched exactly as well as the winner, which only
    /// its smaller id chose: deterministic, but a guess about which part this is.
    pub tied: Vec<String>,
    /// The orderable-variant selection, when the winner is one of a `variant_of` set.
    pub selection: Option<variants::Selection>,
}

impl Resolution {
    #[must_use]
    pub const fn unresolved(query: ComponentQuery) -> Self {
        Self {
            model: None,
            confidence: Confidence::Unresolved,
            query,
            source: None,
            layer: None,
            origin: None,
            provenance: None,
            references: Vec::new(),
            tied: Vec::new(),
            selection: None,
        }
    }
}

impl std::fmt::Display for Resolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ref_str = self.query.reference.as_deref().unwrap_or("?");
        let val_str = self.query.value.as_deref().unwrap_or("");
        match &self.model {
            Some(m) => write!(
                f,
                "{} ({}) => {} '{}' [{}]",
                ref_str, val_str, m.id, m.description, self.confidence
            ),
            None => write!(f, "{ref_str} ({val_str}) => UNRESOLVED"),
        }
    }
}

// ── ModelLibrary ──────────────────────────────────────────────────────────────

/// One compiled entry tagged with the layer and origin it was loaded from.
struct LayeredEntry {
    layer: SourceLayer,
    /// The db file stem, pack `name@version`, or user file stem.
    origin: String,
    provenance: ModelSource,
    references: Vec<schema::ModelSourceReference>,
    compiled: CompiledEntry,
}

/// The component model library.
///
/// Holds compiled match rules from every loaded source, each tagged with its
/// explicit [`SourceLayer`], plus user SPICE cards (layer 40, matched by exact
/// card name before the entry scan).
pub struct ModelLibrary {
    /// All compiled TOML entries, in load order, each carrying its layer.
    entries: Vec<LayeredEntry>,
    /// User-provided SPICE cards (`.model` / `.subckt`).
    spice: Vec<SpiceCard>,
    /// Pin-role inference rules (built-in seed + user `pin_rules.toml` files).
    /// User rules are prepended so they override the built-ins.
    pin_rules: PinRuleTable,
    /// Named abstentions: parts the library knows it cannot model, each with the
    /// input that would unlock it. Consulted ONLY on the no-model path, and it
    /// can change nothing but the disclosure text.
    unmodelled: UnmodelledTable,
    /// Every resolution answered so far, by query. A static check resolves
    /// each part once per check and binding once per pass, so a large board
    /// asked the same few thousand questions over a dozen times each, a full
    /// entry scan apiece. [`Self::resolve`] is a pure function of the query
    /// and the loaded entries, and every method that loads entries clears this.
    /// A read-write lock, not a mutex: an ensemble resolves the same board on
    /// every worker thread at once, and almost every call is a hit, so readers
    /// must not queue behind each other (measured: a mutex here left a quarter
    /// of each worker's time waiting on the lock).
    resolved: std::sync::RwLock<Option<std::collections::HashMap<ComponentQuery, Resolution>>>,
    /// Whole-part answers a caller layered over [`Self::resolve`] (the binder's
    /// fallbacks), by a key the caller builds from everything the answer reads.
    /// Cleared with [`Self::resolved`].
    parts: std::sync::RwLock<Option<std::collections::HashMap<String, Resolution>>>,
}

/// Resolutions [`ModelLibrary`] keeps before it starts over, so a long-lived
/// session resolving many boards stays bounded.
const RESOLVED_CAP: usize = 1 << 15;

use matcher::CompiledEntry;

impl ModelLibrary {
    /// Create an empty library with no entries.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            entries: Vec::new(),
            spice: Vec::new(),
            pin_rules: PinRuleTable::empty(),
            unmodelled: UnmodelledTable::empty(),
            resolved: std::sync::RwLock::new(None),
            parts: std::sync::RwLock::new(None),
        }
    }

    /// Create a library loaded with all built-in database entries.
    ///
    /// # Panics
    ///
    /// Panics when a built-in model database, `pin_rules.toml` or `unmodelled.toml`
    /// fails to load, which the test suite rules out before release.
    #[must_use]
    pub fn builtin() -> Self {
        let mut lib = Self::empty();
        for (name, toml_src) in BUILTIN_TOML_FILES {
            lib.load_toml_str(toml_src, name, SourceLayer::Builtin)
                .unwrap_or_else(|e| panic!("built-in database '{name}' failed to load: {e}"));
        }
        lib.pin_rules
            .load_toml_str(include_str!("../db/pin_rules.toml"), false)
            .unwrap_or_else(|e| panic!("built-in pin_rules.toml failed to load: {e}"));
        lib.unmodelled
            .load_toml_str(include_str!("../db/unmodelled.toml"), false)
            .unwrap_or_else(|e| panic!("built-in unmodelled.toml failed to load: {e}"));
        lib
    }

    /// The pin-role inference table (built-in rules plus any user
    /// `pin_rules.toml`). Consulted by the binder when a pad lacks an explicit
    /// pin-function role.
    #[must_use]
    pub const fn pin_rules(&self) -> &PinRuleTable {
        &self.pin_rules
    }

    /// The named-abstention table (built-in plus any user `unmodelled.toml`).
    ///
    /// Consulted by the binder ONLY when nothing resolved, and only to replace the
    /// disclosure's two generic sentences with specific ones. It cannot make a part
    /// bind; see `unmodelled.rs` for why that containment is the point.
    #[must_use]
    pub const fn unmodelled(&self) -> &UnmodelledTable {
        &self.unmodelled
    }

    /// Lazily-initialised shared built-in library.
    #[must_use]
    pub fn builtin_shared() -> &'static Self {
        static LIB: std::sync::LazyLock<ModelLibrary> =
            std::sync::LazyLock::new(ModelLibrary::builtin);
        &LIB
    }

    /// Built-in library plus installed packs plus the user model directories,
    /// in explicit layer order (lowest to highest priority):
    ///   0. the embedded builtin db,
    ///   1. installed packs (`~/.hauksbee/packs`, recorded in `packs.toml`),
    ///   2. `~/.hauksbee/models`, where datasheet extraction writes,
    ///   3. `~/.config/hauksbee/models`; the user's own custom models,
    ///   4. each `extra_dirs` entry, e.g. a `--models-dir` flag, highest.
    ///
    /// A custom behavioural part dropped into one of these loads without
    /// recompiling. Directory and pack load errors are warned to stderr, not
    /// fatal, so a single malformed user file never breaks the whole library.
    pub fn builtin_with_user_dirs(extra_dirs: &[&Path]) -> Self {
        let mut lib = Self::builtin();
        if let Some(store) = PackStore::default_location() {
            for w in lib.load_packs(&store) {
                eprintln!("[models] packs: {w}");
            }
        }
        let home = std::env::var("HOME").ok().map(PathBuf::from);
        if let Some(h) = &home {
            // Keep the hand-corrected config layer above extracted user models.
            for (dir, layer) in [
                (h.join(".hauksbee").join("models"), SourceLayer::UserDir),
                (
                    h.join(".config").join("hauksbee").join("models"),
                    SourceLayer::UserConfigDir,
                ),
            ] {
                for e in lib.load_dir_layer(&dir, layer) {
                    eprintln!("[models] user dir {}: {e}", dir.display());
                }
            }
        }
        for dir in extra_dirs {
            for error in lib.load_models_dir(dir) {
                eprintln!("[models] --models-dir {}: {error}", dir.display());
            }
        }
        lib
    }

    /// Load one `--models-dir` argument at [`SourceLayer::ModelsDirFlag`].
    ///
    /// The flag is typed by a person, so it accepts every shape a person
    /// actually has on disk:
    ///   * a directory of loose `*.toml` cards,
    ///   * one pack root (`pack.toml` beside `models/`),
    ///   * a directory that HOLDS packs (`*/pack.toml`).
    ///
    /// The third shape used to be a silent no-op. The scan is one level deep,
    /// so `--models-dir models` over a `models/generic/` pack loaded zero cards
    /// and every part stayed unresolved with nothing on stderr to say why. A
    /// directory with nothing loadable in it now says so, for the same reason a
    /// missing one does.
    pub fn load_models_dir(&mut self, dir: &Path) -> Vec<ModelError> {
        if dir.join("pack.toml").is_file() && dir.join("models").is_dir() {
            return self.load_dir_layer(&dir.join("models"), SourceLayer::ModelsDirFlag);
        }

        // Loose cards first, so a directory that holds both keeps loading the
        // cards sitting directly in it.
        let mut errors = self.load_dir_layer(dir, SourceLayer::ModelsDirFlag);
        let packs = child_packs(dir);
        for pack in &packs {
            errors.extend(self.load_dir_layer(&pack.join("models"), SourceLayer::ModelsDirFlag));
        }
        if errors.is_empty() && packs.is_empty() && dir.is_dir() && !holds_toml(dir) {
            errors.push(ModelError::NoCards {
                dir: dir.display().to_string(),
            });
        }
        errors
    }

    /// Load every installed pack from `store` at [`SourceLayer::Pack`].
    ///
    /// Returns human-readable warnings; nothing here is fatal (a library must
    /// survive one bad installed pack), but nothing is silent either:
    ///   - a recorded pack whose dir fails validation is skipped with a warning;
    ///   - two packs shipping the same model id is a same-layer conflict,
    ///     reported naming both packs (resolving it quietly is forbidden,
    ///     within a layer only specificity orders entries, so identical ids
    ///     would tie on match rules and win by load order, i.e. by accident).
    pub fn load_packs(&mut self, store: &PackStore) -> Vec<String> {
        let mut warnings = Vec::new();
        let records = match store.list() {
            Ok(r) => r,
            Err(e) => return vec![format!("cannot read pack record: {e}")],
        };
        // model id -> pack dir_name that first shipped it, for conflict reports.
        let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for record in &records {
            let pack = match pack::Pack::load(&store.pack_dir(record)) {
                Ok(p) => p,
                Err(e) => {
                    warnings.push(format!(
                        "installed pack '{}@{}' failed to load and was skipped: {e}",
                        record.name, record.version
                    ));
                    continue;
                }
            };
            let origin = pack.manifest.dir_name();
            let pack_tier = match pack.manifest.provenance {
                Provenance::Vendor => ModelSourceTier::VendorSpice,
                Provenance::HandWritten => ModelSourceTier::CuratedPack,
                Provenance::DatasheetExtracted => ModelSourceTier::DatasheetDerived,
            };
            for file in &pack.model_files {
                let before = self.entries.len();
                let loaded = std::fs::read_to_string(file)
                    .map_err(|e| format!("reading '{}': {e}", file.display()))
                    .and_then(|src| {
                        self.load_toml_str_with_tier(
                            &src,
                            &origin,
                            SourceLayer::Pack,
                            Some(pack_tier),
                        )
                        .map_err(|e| e.to_string())
                    });
                if let Err(e) = loaded {
                    warnings.push(format!("pack '{origin}': {e}"));
                    continue;
                }
                for le in &self.entries[before..] {
                    let id = le.compiled.entry.id.clone();
                    match seen.get(&id) {
                        Some(other) if *other != origin => warnings.push(format!(
                            "same-layer conflict: model id '{id}' is shipped by both \
                             pack '{other}' and pack '{origin}'; within the pack layer \
                             nothing orders them; remove one pack or rename the entry"
                        )),
                        Some(_) => {}
                        None => {
                            seen.insert(id, origin.clone());
                        }
                    }
                }
            }
        }
        warnings
    }

    /// Append entries from a user TOML directory at runtime (consuming form).
    ///
    /// Every `.toml` file in `dir` is parsed and validated. Files that fail
    /// are reported but do not prevent other files from loading.
    ///
    /// # Errors
    ///
    /// Returns every per-file [`ModelError`] from [`Self::load_user_dir`] when any
    /// file failed; the entries that did load are dropped with `self`.
    pub fn with_user_dir(mut self, dir: &Path) -> Result<Self, Vec<ModelError>> {
        match self.load_user_dir(dir) {
            errs if errs.is_empty() => Ok(self),
            errs => Err(errs),
        }
    }

    /// Append entries from a user TOML directory in place at
    /// [`SourceLayer::UserDir`], returning any per-file errors (an empty vec
    /// on success).
    pub fn load_user_dir(&mut self, dir: &Path) -> Vec<ModelError> {
        self.load_dir_layer(dir, SourceLayer::UserDir)
    }

    /// Append entries from a TOML directory at an explicit layer.
    pub fn load_dir_layer(&mut self, dir: &Path, layer: SourceLayer) -> Vec<ModelError> {
        let mut errors = Vec::new();
        if !dir.exists() {
            // The auto-discovered `~/.hauksbee` / `~/.config` dirs legitimately
            // may not exist, skip them silently. But `--models-dir` (and the CI
            // equivalent) is an EXPLICIT user-typed path: a missing one is a
            // typo that would otherwise load zero models with no signal, so the
            // user's whole custom set silently never applies. Report it.
            if layer == SourceLayer::ModelsDirFlag {
                errors.push(ModelError::MissingDir {
                    dir: dir.display().to_string(),
                });
            }
            return errors;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => return vec![ModelError::Io(e)],
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let src = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => {
                    errors.push(ModelError::Io(e));
                    continue;
                }
            };
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_string();
            // A `pin_rules.toml` (or any file carrying a `[[pin_rules]]` array)
            // feeds the pin-role table and an `unmodelled.toml` the abstention
            // table, both prepended so user rules override the built-ins.
            // The unmodelled dispatch is deliberately narrower: it also requires
            // NO `[[models]]` array, because these files are heavily commented
            // and a models file that merely MENTIONS the token in prose would
            // otherwise be routed here and silently lose every model in it.
            let table = if name == "pin_rules" || src.contains("[[pin_rules]]") {
                self.pin_rules.load_toml_str(&src, true)
            } else if name == "unmodelled"
                || (src.contains("[[unmodelled]]") && !src.contains("[[models]]"))
            {
                self.unmodelled.load_toml_str(&src, true)
            } else {
                let source_name = path.display().to_string();
                errors.extend(self.load_toml_str(&src, &source_name, layer).err());
                continue;
            };
            let rules_error = |message| ModelError::PinRules {
                file: name,
                message,
            };
            errors.extend(table.err().map(rules_error));
        }
        errors
    }

    /// Add all `.model` and `.subckt` cards from a SPICE file.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read.
    pub fn add_spice_file(&mut self, path: &Path) -> anyhow::Result<usize> {
        let cards = spice_input::parse_spice_file(path)?;
        let n = cards.len();
        self.spice.extend(cards);
        self.forget_resolutions();
        Ok(n)
    }

    /// Add a SPICE card directly (for testing).
    pub fn add_spice_card(&mut self, card: SpiceCard) {
        self.spice.push(card);
        self.forget_resolutions();
    }

    /// Load a user SPICE model file at [`SourceLayer::Spice`], the top of the
    /// layer ladder, and say how many cards it carried.
    ///
    /// This is [`Self::add_spice_file`] with the errors a person typing a path
    /// needs. A file with no `.model` or `.subckt` card in it is one of them:
    /// the user named the file expecting the parts in it to bind, and loading
    /// zero cards quietly leaves those parts unresolved with nothing said.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read, or when it holds no `.model` or `.subckt`
    /// card; both messages say what to point `--spice-models` at.
    pub fn load_spice_models(&mut self, path: &Path) -> anyhow::Result<usize> {
        let added = self.add_spice_file(path).map_err(|e| {
            anyhow::anyhow!(
                "cannot read SPICE model cards from '{}': {e}. Point --spice-models at \
                 the vendor's model file (`.lib`, `.cir`, `.mod`, `.sp` or `.ckt`)",
                path.display()
            )
        })?;
        if added == 0 {
            anyhow::bail!(
                "'{}' carries no `.model` or `.subckt` card, so nothing was loaded from it. \
                 Point --spice-models at the file holding the vendor's `.model` lines",
                path.display()
            );
        }
        Ok(added)
    }

    // ── Resolution ────────────────────────────────────────────────────────────

    /// Resolve a component query to a model entry.
    ///
    /// The winner is the matching entry with the highest semantic source tier,
    /// then ([`SourceLayer::priority`], specificity score). This keeps a new
    /// datasheet-extracted draft below curated sources while preserving an
    /// explicit user-model override.
    /// Within a layer the specificity score orders entries; a score tie is
    /// broken by regex constrainedness ([`CompiledEntry::regex_specificity`]),
    /// so an exact-literal override (`^1N4004$`) beats the family pattern
    /// (`^1N400[1-7]$`) it carves out of. A remaining tie is broken by the
    /// lexicographically-smallest entry id, never by load order, so
    /// resolution is deterministic regardless of file order. User SPICE cards
    /// are layer 40, the highest, and match by exact card name, so they are
    /// checked before the entry scan.
    pub fn resolve(&self, q: &ComponentQuery) -> Resolution {
        // The designator reaches a resolution in only two ways: whether it is an
        // active IC (a footprint-only discrete card never speaks for one), and the
        // text echoed back in `query` and the variant selection. So the memo is keyed
        // by the designator's class, and the caller's own text is put back on a hit:
        // a board of N parts that differ only by designator costs one library scan.
        let class_ref = q.reference.as_ref().map(|_| {
            if q.is_active_ic() {
                "U".to_string()
            } else {
                "R".to_string()
            }
        });
        let key_holder;
        let key = if q.reference == class_ref {
            q
        } else {
            let mut k = q.clone();
            k.reference.clone_from(&class_ref);
            key_holder = k;
            &key_holder
        };
        let with_designator = |mut r: Resolution| {
            if q.reference != class_ref {
                r.query.reference.clone_from(&q.reference);
                if let Some(s) = r.selection.as_mut() {
                    s.reference = q.reference.clone().unwrap_or_default();
                }
            }
            r
        };
        if let Some(r) = self
            .resolved
            .read()
            .ok()
            .and_then(|m| m.as_ref().and_then(|m| m.get(key).cloned()))
        {
            return with_designator(r);
        }
        // A field of a megabyte is matched against every library entry in full:
        // match on the cut one, so one hostile value cannot stall the run.
        let r = key.bounded_for_matching().map_or_else(
            || self.resolve_uncached(key),
            |bounded| self.resolve_uncached(&bounded),
        );
        if let Ok(mut memo) = self.resolved.write() {
            let memo = memo.get_or_insert_with(std::collections::HashMap::new);
            if memo.len() >= RESOLVED_CAP {
                memo.clear();
            }
            memo.insert(key.clone(), r.clone());
        }
        with_designator(r)
    }

    /// The answer a caller layers over [`Self::resolve`] for the part `key`
    /// names, worked out by `compute` the first time and remembered after.
    ///
    /// `key` must hold everything `compute` reads: two parts with one key get
    /// one answer. A board of thousands of parts that differ only by designator
    /// and wiring asks the same question once.
    pub fn memoised_part(&self, key: &str, compute: impl FnOnce() -> Resolution) -> Resolution {
        let held = (self.parts.read().ok())
            .and_then(|memo| memo.as_ref().and_then(|memo| memo.get(key).cloned()));
        if let Some(held) = held {
            return held;
        }
        let answer = compute();
        if let Ok(mut memo) = self.parts.write() {
            let memo = memo.get_or_insert_with(std::collections::HashMap::new);
            if memo.len() >= RESOLVED_CAP {
                memo.clear();
            }
            memo.insert(key.to_string(), answer.clone());
        }
        answer
    }

    /// The MCU cards, in load order.
    fn mcu_cards(&self) -> impl Iterator<Item = &ModelEntry> {
        (self.entries.iter())
            .map(|le| &le.compiled.entry)
            .filter(|e| e.kind == ComponentKind::Mcu)
    }

    /// The core clock (Hz) an MCU card gives as `default_clock_hz`: what the
    /// part runs at when neither its firmware's fuses nor a crystal on the board
    /// state a clock. It belongs to the card, so every consumer (the co-sim's AVR
    /// core above all) reads it here and no run keeps a clock of its own.
    ///
    /// `requested_part` is the component's value. The card it resolves to wins;
    /// a value no card matches (an ordering code such as `ATmega328P-PU`) takes
    /// the clock the cards of the same `backend` and `module` shape agree on, and
    /// `None` when they disagree or none states one.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // a positive, finite Hz
    pub fn mcu_default_clock_hz(
        &self,
        backend: &str,
        requested_part: &str,
        module: bool,
    ) -> Option<u64> {
        let hz = |e: &ModelEntry| {
            (e.params.get_f64("default_clock_hz"))
                .filter(|h| h.is_finite() && *h >= 1.0)
                .map(|h| h.round() as u64)
        };
        let on_backend = |e: &ModelEntry| e.params.get_str("backend") == Some(backend);
        let mut query = ComponentQuery::new(
            None::<String>,
            Some(requested_part.to_string()),
            None::<String>,
        );
        query.mpn = Some(requested_part.to_string());
        if let Some(h) = (self.resolve(&query).model.as_ref())
            .filter(|m| on_backend(m))
            .and_then(hz)
        {
            return Some(h);
        }
        let mut agreed = None;
        for card in self
            .mcu_cards()
            .filter(|e| on_backend(e) && e.params.get_bool("module").unwrap_or(false) == module)
        {
            match (agreed, hz(card)) {
                (_, None) => {}
                (None, found) => agreed = found,
                (Some(a), Some(h)) if a != h => return None,
                (Some(_), Some(_)) => {}
            }
        }
        agreed
    }

    /// The simavr cores the cards name (`backend = "simavr:<part>"`), sorted and
    /// unique: the AVR parts this build has a card for.
    #[must_use]
    pub fn simavr_parts(&self) -> Vec<String> {
        let parts: std::collections::BTreeSet<String> = self
            .mcu_cards()
            .filter_map(|e| e.params.get_str("backend")?.strip_prefix("simavr:"))
            .map(str::to_string)
            .collect();
        parts.into_iter().collect()
    }

    /// Every simulator backend an MCU card names (`renode:stm32f4`,
    /// `simavr:atmega328p`), sorted and unique: the cores a drafted MCU card
    /// can name.
    #[must_use]
    pub fn mcu_backends(&self) -> Vec<String> {
        let set: std::collections::BTreeSet<String> = self
            .mcu_cards()
            .filter_map(|e| e.params.get_str("backend"))
            .map(str::to_string)
            .collect();
        set.into_iter().collect()
    }

    /// Drop every remembered resolution: the entries they were answered from
    /// are about to change.
    fn forget_resolutions(&mut self) {
        if let Ok(memo) = self.resolved.get_mut() {
            *memo = None;
        }
        if let Ok(memo) = self.parts.get_mut() {
            *memo = None;
        }
    }

    /// [`Self::resolve`] without the memo: one scan of every entry.
    fn resolve_uncached(&self, q: &ComponentQuery) -> Resolution {
        use std::cmp::Reverse;
        // Layer 40: SPICE cards, match by card name against value and MPN.
        if let Some(card) = self.find_spice_match(q) {
            return Self::resolution_from_spice(card, q.clone());
        }

        // Layers 0..30: one scan, best (layer priority, specificity,
        // regex constrainedness, smallest id) wins. The last two keys make
        // ties deterministic and independent of load order; the entries only
        // the id separates are reported as `tied`.
        let rank = |le: &LayeredEntry| {
            let score = le.compiled.specificity_score(q);
            let (tier, layer) = (le.provenance.tier().priority(), le.layer.priority());
            (tier, layer, score, le.compiled.regex_specificity())
        };
        let prepared = matcher::PreparedValue::of(q);
        let matched =
            || (self.entries.iter()).filter(|le| le.compiled.matches_prepared(q, &prepared));
        if let Some(le) = matched().max_by_key(|le| (rank(le), Reverse(&le.compiled.entry.id))) {
            let (id, top) = (&le.compiled.entry.id, rank(le));
            // Siblings of an orderable-variant set are not a tie: `variants::select`
            // scores them against the component and binds one, a stand-in, or none.
            let family = le.compiled.entry.variant_of.as_deref();
            let tier =
                |o: &LayeredEntry| (rank(o).0, rank(o).1, o.compiled.entry.variant_of.clone());
            let sibling = |o: &LayeredEntry| family.is_some() && tier(o) == tier(le);
            let mut siblings: Vec<&LayeredEntry> = matched().filter(|o| sibling(o)).collect();
            siblings.sort_by(|a, b| a.compiled.entry.id.cmp(&b.compiled.entry.id));
            let entries: Vec<&ModelEntry> = siblings.iter().map(|o| &o.compiled.entry).collect();
            let mut selection = family.map(|family| variants::select(family, &entries, q));
            let bound = selection.as_ref().map(variants::Selection::bound);
            let named = |b| {
                siblings
                    .into_iter()
                    .find(|o| Some(&*o.compiled.entry.id) == b)
            };
            let Some(le) = bound.map_or(Some(le), named) else {
                let open = Resolution::unresolved(q.clone());
                return Resolution { selection, ..open };
            };
            let (mut model, mut provenance) = (le.compiled.entry.clone(), le.provenance.clone());
            // A stand-in for an undetermined set runs on the envelope and pin standard of the members left.
            if let Some(s) = selection.as_mut().filter(|s| s.determined.is_none()) {
                let left = |e: &&ModelEntry| s.survivors().any(|m| m.id == e.id);
                let left: Vec<&ModelEntry> = entries.iter().copied().filter(left).collect();
                s.widened = variants::envelope(&mut model, &mut provenance, &left);
                model.pin_standard = left.iter().find_map(|e| e.pin_standard.clone());
                // Members printed for different packages leave the stand-in's undetermined.
                let packages: std::collections::BTreeSet<_> =
                    left.iter().map(|e| &e.pin_package).collect();
                model.pin_package = match Vec::from_iter(packages)[..] {
                    [one] => one.clone(),
                    _ => None,
                };
            }
            let tie = |other: &&LayeredEntry| {
                rank(other) == top && other.compiled.entry.id != *id && !sibling(other)
            };
            let ids = matched().filter(tie).map(|o| o.compiled.entry.id.clone());
            let ids: std::collections::BTreeSet<String> = ids.collect();
            return Resolution {
                selection,
                tied: ids.into_iter().collect(),
                model: Some(model),
                confidence: score_to_confidence(rank(le).2 + le.compiled.outright_bonus()),
                query: q.clone(),
                source: Some(le.layer.coarse_name().to_string()),
                layer: Some(le.layer),
                origin: Some(le.origin.clone()),
                provenance: Some(provenance),
                references: le.references.clone(),
            };
        }

        Resolution::unresolved(q.clone())
    }

    /// Resolve a batch of queries and return a resolution report table.
    #[must_use]
    pub fn report(&self, queries: &[ComponentQuery]) -> ResolutionReport {
        let resolutions: Vec<Resolution> = queries.iter().map(|q| self.resolve(q)).collect();
        ResolutionReport { resolutions }
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    fn load_toml_str(
        &mut self,
        src: &str,
        source_name: &str,
        layer: SourceLayer,
    ) -> Result<(), ModelError> {
        self.load_toml_str_with_tier(src, source_name, layer, None)
    }

    fn load_toml_str_with_tier(
        &mut self,
        src: &str,
        source_name: &str,
        layer: SourceLayer,
        default_tier: Option<ModelSourceTier>,
    ) -> Result<(), ModelError> {
        let parse = |e| ModelError::TomlParse {
            source: source_name.to_string(),
            error: e,
        };
        let db_file: schema::DbFile = toml::from_str(src).map_err(parse)?;
        if let Some(messages) = variants::lint(&db_file.models).into_iter().next() {
            let id = source_name.to_string();
            return Err(ModelError::ValidationFailed { id, messages });
        }

        for mut entry in db_file.models {
            let id = entry.id.clone();
            let invalid = |messages: String| ModelError::ValidationFailed {
                id: id.clone(),
                messages: format!("{source_name}: {messages}"),
            };
            // An all-None `[match]` block (an omitted section, or a typo'd key
            // serde silently drops) matches EVERY component at specificity 0;
            // one such stray user entry would silently rebind the whole board.
            if entry.r#match.is_empty() {
                return Err(invalid(
                    "entry has no match rules (lib_id / value_re / footprint_re / mpn_re all \
                     absent); at least one is required, else it would match every component"
                        .to_string(),
                ));
            }
            if layer != SourceLayer::Builtin {
                let mut errors = validation::validate(&entry)
                    .err()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|error| error.message)
                    .collect::<Vec<_>>();
                errors.extend(
                    behavioral::validate_behavioral(&entry.behavioral)
                        .into_iter()
                        .map(|error| format!("[models.behavioral]: {error}")),
                );
                if !entry.logic.is_empty() {
                    if let Err(error) = entry.logic.validate() {
                        errors.push(format!("[models.logic]: {error}"));
                    }
                }
                if !errors.is_empty() {
                    return Err(invalid(errors.join("; ")));
                }
            }
            // Taken before compiling so provenance is carried once, by
            // `ModelSource`, not also inside the compiled entry. Builtin cards
            // skip `validate` above, so their source is checked here too.
            entry.derating = derating::Derating::of(&entry);
            entry.pin_standard = validation::pin_standard(&entry).map(str::to_owned);
            entry.pin_package = validation::pin_package(&entry).map(str::to_owned);
            entry.doubts = entry
                .source
                .as_ref()
                .map(|s| s.doubt.clone())
                .unwrap_or_default();
            // A level printed at a heavy test current is the light-load level
            // behind an output resistance (`output_load`), for every digital card.
            let (load_rows, load_records) = output_load::light_load_levels(&mut entry);
            let mut declaration = entry.source.take();
            if let Some(Err(e)) = declaration.as_ref().map(schema::ModelSourceSpec::validate) {
                return Err(invalid(format!("[models.source]: {e}")));
            }
            let compiled = CompiledEntry::compile(entry).map_err(|e| ModelError::InvalidRegex {
                id: id.clone(),
                error: e,
            })?;
            let references = declaration
                .as_ref()
                .map(|source| source.references.clone())
                .unwrap_or_default();
            // A row param is its own characteristic, so its interval is derived
            // (a card with no source declares no uncertainty, derived or not).
            if let Some(source) = declaration.as_mut() {
                let rows = compiled.entry.params.rows();
                let rows = rows.filter_map(|(key, row)| param_names::row_characteristic(key, row));
                source.characteristic.extend(rows.chain(load_rows));
                // What a MOSFET law rests on (vto's declared threshold range, the
                // law's error at each RDS(on) row), and a loaded level's light-load
                // working, is stated, never left in a comment; it joins, not
                // replaces, the model-error record.
                let mut law = param_names::mosfet_law_evidence(&compiled.entry);
                law.extend(load_records);
                if source.uncertainty.is_empty() && !law.is_empty() {
                    source.uncertainty.push(default_model_uncertainty(&id));
                }
                source.uncertainty.extend(law);
            }
            let provenance = model_source(
                declaration.as_ref(),
                default_tier,
                default_source_tier(layer),
                default_validation(layer),
                model_layer(layer),
                source_name,
                &id,
            )
            .map_err(|error| invalid(format!("invalid [models.source]: {error}")))?;
            self.forget_resolutions();
            self.entries.push(LayeredEntry {
                layer,
                origin: source_name.to_string(),
                provenance,
                references,
                compiled,
            });
        }
        Ok(())
    }

    fn find_spice_match<'a>(&'a self, q: &ComponentQuery) -> Option<&'a SpiceCard> {
        let val = q.value.as_deref().unwrap_or("").to_uppercase();
        let mpn = q.mpn.as_deref().unwrap_or("").to_uppercase();
        self.spice.iter().find(|card| {
            let name = card.name.to_uppercase();
            name == val || name == mpn
        })
    }

    fn resolution_from_spice(card: &SpiceCard, query: ComponentQuery) -> Resolution {
        // A `.subckt` is a multi-terminal macro, not a single-device kind, and
        // an unrecognized `.model` type must not be silently claimed as a
        // passive with Exact confidence (that shadows the real part). Both
        // resolve to Unresolved instead.
        let kind = match card.kind {
            spice_input::SpiceCardKind::Subckt => None,
            spice_input::SpiceCardKind::Model => {
                spice_kind_from_model_type(card.model_type.as_deref())
            }
        };
        let Some(kind) = kind else {
            return Resolution::unresolved(query);
        };
        let mut params = Params::default();
        for (k, v) in &card.params {
            params.set_f64(k.to_lowercase(), *v);
        }
        let entry = ModelEntry {
            id: card.name.to_lowercase(),
            kind,
            description: format!("User SPICE card: {}", card.name),
            params,
            pins: (1..)
                .map(|i| i.to_string())
                .zip(card.ports.iter().cloned())
                .collect(),
            ..Default::default()
        };

        Resolution {
            model: Some(entry),
            confidence: Confidence::Exact,
            source: Some("spice".to_string()),
            layer: Some(SourceLayer::Spice),
            origin: Some(card.name.clone()),
            provenance: Some(
                ModelSource::new(
                    ModelSourceTier::UserModel,
                    ModelLayer::Spice,
                    card.name.clone(),
                    ModelValidation::Unvalidated,
                    vec![ModelUncertainty::unknown(
                        format!("{}.model", card.name),
                        "the SPICE card declares no validated numeric error interval",
                    )
                    .expect("static unknown uncertainty is valid")],
                )
                .expect("SPICE card names are non-empty after parsing"),
            ),
            ..Resolution::unresolved(query)
        }
    }
}

/// Provenance for a model with an optional `[models.source]` or
/// `[sensor.source]`.
///
/// A pack-manifest `pack_tier` is authoritative: an entry in a
/// datasheet-derived pack cannot promote itself to a vendor tier. Omitted
/// fields take the defaults, and an omitted uncertainty is an explicit UNKNOWN.
///
/// # Errors
///
/// Fails when the resulting [`ModelSource`] is malformed, for example an
/// uncertainty record the source rules reject; the error is its message.
///
/// # Panics
///
/// Never in practice: the fallback UNKNOWN uncertainty is built from a fixed,
/// valid reason.
pub fn model_source(
    declared: Option<&schema::ModelSourceSpec>,
    pack_tier: Option<ModelSourceTier>,
    default_tier: ModelSourceTier,
    default_validation: ModelValidation,
    layer: ModelLayer,
    origin: &str,
    id: &str,
) -> Result<ModelSource, String> {
    let mut uncertainty = declared
        .map(|s| s.uncertainty.clone())
        .filter(|values| !values.is_empty())
        .unwrap_or_else(|| vec![default_model_uncertainty(id)]);
    // A characteristic row printing min and max states its interval, unless a
    // declared one already names that parameter or those bounds.
    for row in declared.iter().flat_map(|s| &s.characteristic) {
        let stated = |u: &ModelUncertainty| {
            matches!(u, ModelUncertainty::Interval { parameter: p, low, high, .. }
            if *p == row.parameter || [Some(*low), Some(*high)] == [row.min, row.max])
        };
        uncertainty.extend(row.interval().filter(|_| !uncertainty.iter().any(stated)));
    }
    ModelSource::new(
        pack_tier
            .or_else(|| declared.map(|s| s.tier))
            .unwrap_or(default_tier),
        layer,
        origin,
        declared.map_or(default_validation, |s| s.validation),
        uncertainty,
    )
    .map_err(|error| error.to_string())
}

/// The model-error record a source declaring none carries.
fn default_model_uncertainty(id: &str) -> ModelUncertainty {
    ModelUncertainty::unknown(
        format!("{id}.model"),
        "the source publishes no validated numeric error interval",
    )
    .expect("static unknown uncertainty is valid")
}

const fn model_layer(layer: SourceLayer) -> ModelLayer {
    match layer {
        SourceLayer::Builtin => ModelLayer::Builtin,
        SourceLayer::Pack => ModelLayer::Pack,
        SourceLayer::UserDir => ModelLayer::UserDir,
        SourceLayer::UserConfigDir => ModelLayer::UserConfigDir,
        SourceLayer::ModelsDirFlag => ModelLayer::ModelsDir,
        SourceLayer::Spice => ModelLayer::Spice,
    }
}

const fn default_source_tier(layer: SourceLayer) -> ModelSourceTier {
    match layer {
        SourceLayer::Builtin => ModelSourceTier::CuratedLibrary,
        SourceLayer::Pack => ModelSourceTier::CuratedPack,
        SourceLayer::UserDir => ModelSourceTier::DatasheetDerived,
        // A loose SPICE card is user supplied. Vendor provenance is established
        // only by a licensed vendor pack manifest.
        SourceLayer::UserConfigDir | SourceLayer::ModelsDirFlag | SourceLayer::Spice => {
            ModelSourceTier::UserModel
        }
    }
}

const fn default_validation(layer: SourceLayer) -> ModelValidation {
    match layer {
        SourceLayer::Builtin | SourceLayer::Pack | SourceLayer::UserDir => {
            ModelValidation::PhysicalBoundsOnly
        }
        SourceLayer::UserConfigDir | SourceLayer::ModelsDirFlag | SourceLayer::Spice => {
            ModelValidation::Unvalidated
        }
    }
}

/// Convert a specificity score to a confidence level.
const fn score_to_confidence(score: u32) -> Confidence {
    match score {
        50.. => Confidence::Exact,
        20.. => Confidence::Family,
        _ => Confidence::Guessed,
    }
}

/// Infer a [`ComponentKind`] from a SPICE `.model` type string. Returns `None`
/// for a type the resolver cannot model, an unknown/unsupported type is NOT
/// silently downgraded to `Passive` (that produced a wrong `Exact` match that
/// shadowed the real part). Genuine passive model types (R/C/L) still map to
/// `Passive`.
fn spice_kind_from_model_type(t: Option<&str>) -> Option<ComponentKind> {
    match t.unwrap_or("").to_uppercase().as_str() {
        "D" => Some(ComponentKind::Diode),
        "NPN" => Some(ComponentKind::BjtNpn),
        "PNP" => Some(ComponentKind::BjtPnp),
        "NMOS" | "NMOSFET" => Some(ComponentKind::Nmos),
        "PMOS" | "PMOSFET" => Some(ComponentKind::Pmos),
        "R" | "RES" | "C" | "CAP" | "L" | "IND" => Some(ComponentKind::Passive),
        _ => None, // unknown/unsupported: don't guess Passive
    }
}

// ── ResolutionReport ──────────────────────────────────────────────────────────

/// A table of resolution results for a batch of queries.
pub struct ResolutionReport {
    pub resolutions: Vec<Resolution>,
}

impl ResolutionReport {
    /// Count resolutions by confidence level: `(exact, family, guessed,
    /// unresolved)`.
    #[must_use]
    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let mut n = [0usize; 4];
        for r in &self.resolutions {
            n[r.confidence as usize] += 1;
        }
        n.into()
    }

    /// All unresolved queries.
    #[must_use]
    pub fn unresolved(&self) -> Vec<&Resolution> {
        self.resolutions
            .iter()
            .filter(|r| r.confidence == Confidence::Unresolved)
            .collect()
    }
}

impl std::fmt::Display for ResolutionReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (exact, family, guessed, unresolved) = self.counts();
        let n = self.resolutions.len();
        writeln!(
            f,
            "┌──────────────────────────────────────────────────────────────┐\n\
             │ Resolution report: {n} components\n\
             │  exact={exact}  family={family}  guessed={guessed}  unresolved={unresolved}\n\
             ├──────────┬──────────────────────────┬────────────┬────────────┤\n\
             │ Ref      │ Value                    │ Model ID   │ Confidence │\n\
             ├──────────┼──────────────────────────┼────────────┼────────────┤"
        )?;
        for res in &self.resolutions {
            let ref_s = res.query.reference.as_deref().unwrap_or("?");
            let val_s = res.query.value.as_deref().unwrap_or("");
            let model_id = res.model.as_ref().map_or("UNRESOLVED", |m| m.id.as_str());
            // Truncate on CHAR boundaries: a byte slice panics when a multibyte
            // char (the 'µ' in a "µF" value) straddles the cut point.
            let clip = |s: &str, n: usize| -> String { s.chars().take(n).collect() };
            writeln!(
                f,
                "│ {:<8} │ {:<24} │ {:<10} │ {:<10} │",
                clip(ref_s, 8),
                clip(val_s, 24),
                clip(model_id, 10),
                clip(&res.confidence.to_string(), 10),
            )?;
        }
        writeln!(
            f,
            "└──────────┴──────────────────────────┴────────────┴────────────┘"
        )
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn lib() -> ModelLibrary {
        ModelLibrary::builtin()
    }

    /// A megabyte-long value used to cost a megabyte times every library entry
    /// (a 3 MB one did not finish in two minutes). It is matched on its first
    /// kilobyte now, and the answer for a short value is unchanged.
    #[test]
    fn a_huge_value_string_resolves_in_bounded_time() {
        let started = std::time::Instant::now();
        let huge = ComponentQuery {
            value: Some("1".repeat(3_000_000)),
            reference: Some("R3".to_string()),
            ..Default::default()
        };
        let _ = lib().resolve(&huge);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "took {:?}",
            started.elapsed()
        );
        let bounded = huge.bounded_for_matching().expect("cut");
        assert_eq!(bounded.value.as_deref().map(str::len), Some(1024));
        let short = ComponentQuery {
            value: Some("10k".to_string()),
            ..Default::default()
        };
        assert!(short.bounded_for_matching().is_none());
    }

    #[test]
    fn a_cut_value_stays_on_a_character_boundary() {
        let q = ComponentQuery {
            value: Some("é".repeat(2_000)),
            ..Default::default()
        };
        let cut = q.bounded_for_matching().expect("cut").value.expect("value");
        assert!(cut.len() <= 1024 && cut.chars().all(|c| c == 'é'));
    }

    fn by_value(l: &ModelLibrary, value: &str) -> Resolution {
        l.resolve(&ComponentQuery {
            value: Some(value.to_string()),
            ..Default::default()
        })
    }

    /// A loaded digital card whose `voh`/`vol` rows are printed at 24 mA (the
    /// extracted SN74LVC244A shape, SCAS414AG 5.5) resolves with the
    /// light-load level, a derived `ro`, and both workings on its evidence.
    #[test]
    fn a_loaded_level_row_resolves_as_its_light_load_level() {
        let mut l = ModelLibrary::empty();
        l.load_toml_str(
            "[[models]]\nid = \"lvc\"\nkind = \"digital\"\n[models.source]\n\
             tier = \"datasheet-derived\"\nvalidation = \"physical-bounds-only\"\n\
             [models.match]\nvalue_re = \"^LVCX$\"\n[models.params]\n\
             voh = { min = 2.0, unit = \"V\", conditions = \"VCC = 3 V, IOH = -24 mA\", basis = \"5.5 VOH\" }\n\
             vol = { max = 0.8, unit = \"V\", conditions = \"VCC = 3 V, IOL = 24 mA\", basis = \"5.5 VOL\" }\n",
            "lvc",
            // Builtin skips the executable-behaviour validation this bare card would fail.
            SourceLayer::Builtin,
        )
        .unwrap();
        let resolved = by_value(&l, "LVCX");
        let model = resolved.model.expect("resolves");
        assert!((model.params.get_f64("voh").unwrap() - 3.0).abs() < 1e-9);
        assert!((model.params.get_f64("ro").unwrap() - 41.7).abs() < 1e-9);
        let records = resolved
            .provenance
            .expect("a source")
            .uncertainty()
            .to_vec();
        let named = |p: &str| {
            records.iter().any(
                |u| matches!(u, ModelUncertainty::Interval { parameter, .. } if parameter == p),
            )
        };
        assert!(named("voh") && named("vol") && named("ro"), "{records:?}");
    }

    fn card(name: &str, kind: spice_input::SpiceCardKind, model_type: Option<&str>) -> SpiceCard {
        SpiceCard {
            name: name.to_string(),
            kind,
            raw: String::new(),
            ports: Vec::new(),
            params: std::collections::BTreeMap::new(),
            model_type: model_type.map(String::from),
        }
    }

    #[test]
    fn malformed_source_metadata_and_unsafe_references_reject_the_file() {
        let malformed = r#"
            [[models]]
            id = "bad-source"
            kind = "passive"
            [models.match]
            value_re = "^1k$"
            [models.source]
            tier = "user-model"
            validation = "unvalidated"
            [[models.source.uncertainty]]
            status = "interval"
            parameter = "ohms"
            low = 900.0
            high = 1100.0
            unit = "ohm"
            kind = "invented-confidence-class"
            basis = "hostile fixture"
        "#;
        let error = ModelLibrary::empty()
            .load_toml_str(malformed, "bad-source.toml", SourceLayer::UserDir)
            .unwrap_err();
        assert!(matches!(error, ModelError::TomlParse { .. }), "{error:?}");

        for (layer, record) in [
            (SourceLayer::UserDir, "references]]\nurl = \"http://vendor.example/device.pdf\"\ntitle = \"Device\"\nlocator = \"Table 1\""),
            (SourceLayer::Builtin, "references]]\nurl = \"https://vendor.example/device.pdf\"\ntitle = \"Device\"\nlocator = \"Table 1\"\nsha256 = \"abc\""),
            (SourceLayer::UserDir, "uncertainty]]\nstatus = \"interval\"\nparameter = \"ohms\"\nlow = 1100.0\nhigh = 900.0\nunit = \"ohm\"\nkind = \"specification-limits\"\nbasis = \"Table 1\""),
            (SourceLayer::Builtin, "uncertainty]]\nstatus = \"unknown\"\nparameter = \"model\"\nreason = \" \""),
        ] {
            let source = format!(
                "[[models]]\nid = \"bad\"\nkind = \"passive\"\n[models.match]\nvalue_re = \"^1k$\"\n\
                 [models.source]\ntier = \"user-model\"\nvalidation = \"unvalidated\"\n\
                 [[models.source.{record}\n"
            );
            let error = ModelLibrary::empty()
                .load_toml_str(&source, "bad-source.toml", layer)
                .unwrap_err();
            assert!(
                matches!(&error, ModelError::ValidationFailed { messages, .. } if messages.contains("[models.source]")),
                "{error:?}"
            );
        }
    }

    #[test]
    fn entry_without_match_rules_is_rejected_loud() {
        let mut lib = lib();
        let err = lib
            .load_toml_str(
                "[[models]]\nid = \"stray\"\nkind = \"passive\"\n",
                "stray.toml",
                SourceLayer::UserDir,
            )
            .unwrap_err();
        assert!(
            matches!(&err, ModelError::ValidationFailed { id, .. } if id == "stray"),
            "{err:?}"
        );
        lib.load_toml_str(
            "[[models]]\nid = \"real\"\nkind = \"passive\"\n[models.match]\nlib_id = \"Device:R\"\n",
            "real.toml",
            SourceLayer::UserDir,
        )
        .unwrap();
        let source = "[[models]]\nid='bad_converter'\nkind='vreg'\n[models.match]\nvalue_re='BAD'\n[models.pins]\n1='in'\n2='out'\n[models.behal.converter]\ntopology='boost'\nout_pin='out'\nin_pin='in'\nvout_setpoint=-5\n".replace("behal", "behavioral");
        let error = lib
            .load_toml_str(&source, "bad.toml", SourceLayer::UserDir)
            .unwrap_err()
            .to_string();
        for text in ["bad.toml", "bad_converter", "positive finite number"] {
            assert!(error.contains(text), "{error}");
        }
    }

    #[test]
    fn resolution_report_display_survives_multibyte_value() {
        let q = ComponentQuery {
            value: Some(format!("{}{}", "a".repeat(23), "µF")),
            ..Default::default()
        };
        let report = ResolutionReport {
            resolutions: vec![Resolution::unresolved(q)],
        };
        assert!(report.to_string().contains("Resolution report"));
    }

    #[test]
    fn builtin_resolves_representative_parts() {
        let l = lib();
        let r = l.resolve(&ComponentQuery::new(
            Some("Device:R".into()),
            Some("10k".into()),
            None,
        ));
        assert!(r.confidence <= Confidence::Family, "{:?}", r.confidence);
        assert_eq!(r.model.unwrap().kind, ComponentKind::Passive);
        for (value, kind) in [
            ("BC847", ComponentKind::BjtNpn),
            ("1N4148", ComponentKind::Diode),
            ("74HC595", ComponentKind::ShiftRegister),
        ] {
            assert_eq!(by_value(&l, value).model.unwrap().kind, kind, "{value}");
        }
        let hole = l.resolve(&ComponentQuery {
            footprint: Some("MountingHole:MountingHole_4.3mm_M4".to_string()),
            ..Default::default()
        });
        assert_eq!(hole.model.unwrap().kind, ComponentKind::Ignore);

        let res = l.resolve(&ComponentQuery {
            value: Some("UNKNOWNPART_XYZ999".to_string()),
            reference: Some("U99".to_string()),
            ..Default::default()
        });
        assert_eq!(res.confidence, Confidence::Unresolved);
        assert!(res.model.is_none());

        // A rule listing the value outright is exact; a family pattern is not.
        for (rule, want) in [("^XY1$", Confidence::Exact), ("^XY1", Confidence::Family)] {
            let card = format!("[[models]]\nid = 'x'\nkind = 'ignore'\nmatch.value_re = '{rule}'");
            let mut lib = ModelLibrary::empty();
            lib.load_toml_str(&card, "x", SourceLayer::Builtin).unwrap();
            assert_eq!(by_value(&lib, "XY1").confidence, want, "{rule}");
        }
    }

    #[test]
    fn at28c256_fast_write_time_requires_explicit_f_identity() {
        let l = lib();
        let standard = by_value(&l, "28C256").model.unwrap();
        let fast = by_value(&l, "AT28C256F-15JU").model.unwrap();
        assert_eq!(standard.id, "eeprom_28c256");
        assert_eq!(standard.logic.memories[0].program_time_s, Some(0.010));
        assert_eq!(fast.id, "eeprom_at28c256f");
        assert_eq!(fast.logic.memories[0].program_time_s, Some(0.003));
    }

    #[test]
    fn spice_cards_override_builtin_but_subckts_and_unknown_types_do_not_masquerade() {
        let mut l = lib();
        let mut bc847 = card("BC847", spice_input::SpiceCardKind::Model, Some("NPN"));
        bc847.params = std::iter::once(("IS".to_string(), 2e-14_f64)).collect();
        l.add_spice_card(bc847);
        let res = by_value(&l, "BC847");
        assert_eq!(res.source.as_deref(), Some("spice"));
        assert_eq!(res.provenance.unwrap().tier(), ModelSourceTier::UserModel);

        let mut subckt = card("MYOPAMP", spice_input::SpiceCardKind::Subckt, None);
        subckt.ports = vec!["INP".into(), "INN".into(), "OUT".into()];
        l.add_spice_card(subckt);
        let res = by_value(&l, "MYOPAMP");
        assert!(res.model.is_none());
        assert_eq!(res.confidence, Confidence::Unresolved);

        l.add_spice_card(card("M1", spice_input::SpiceCardKind::Model, Some("VDMOS")));
        assert!(by_value(&l, "M1").model.is_none());

        l.add_spice_card(card("RMOD", spice_input::SpiceCardKind::Model, Some("R")));
        assert!(by_value(&l, "RMOD").model.is_some());
    }

    #[test]
    fn a_vendor_spice_file_binds_its_cards_and_reports_a_file_that_cannot_serve() {
        let temp = tempfile::tempdir().unwrap();
        let vendor = temp.path().join("vendor.lib");
        std::fs::write(
            &vendor,
            "* vendor models\n.model 2SD1664R NPN(IS=1.2e-14 BF=250 NF=1.0 VAF=80)\n.end\n",
        )
        .unwrap();

        let mut l = lib();
        assert!(by_value(&l, "2SD1664R").model.is_none());
        assert_eq!(l.load_spice_models(&vendor).unwrap(), 1);
        let res = by_value(&l, "2SD1664R");
        assert_eq!(res.model.map(|m| m.id).as_deref(), Some("2sd1664r"));
        assert_eq!(res.source.as_deref(), Some("spice"));

        // A file the user named that carries nothing is an error, not a quiet
        // zero: the parts in it were meant to bind.
        let empty = temp.path().join("empty.lib");
        std::fs::write(&empty, "* only a comment\n.end\n").unwrap();
        let err = format!("{:#}", l.load_spice_models(&empty).unwrap_err());
        assert!(err.contains("empty.lib"), "{err}");
        assert!(err.contains(".model"), "{err}");

        let missing = temp.path().join("not-there.lib");
        let err = format!("{:#}", l.load_spice_models(&missing).unwrap_err());
        assert!(err.contains("not-there.lib"), "{err}");
        assert!(err.contains("--spice-models"), "{err}");
    }

    #[test]
    fn layer_priorities_and_missing_dirs() {
        assert!(SourceLayer::UserConfigDir.priority() > SourceLayer::UserDir.priority());
        assert!(SourceLayer::UserConfigDir.priority() < SourceLayer::ModelsDirFlag.priority());

        let mut l = lib();
        let missing = Path::new("/nonexistent/hauksbee/models/typo");
        let flag_errs = l.load_dir_layer(missing, SourceLayer::ModelsDirFlag);
        assert!(
            flag_errs
                .iter()
                .any(|e| matches!(e, ModelError::MissingDir { .. })),
            "{flag_errs:?}"
        );
        for auto in [SourceLayer::UserDir, SourceLayer::UserConfigDir] {
            assert!(l.load_dir_layer(missing, auto).is_empty(), "{auto:?}");
        }
    }

    /// Write a pack root (`pack.toml` + `models/<id>.toml`) at `root`.
    fn write_pack(root: &Path, id: &str) {
        std::fs::create_dir_all(root.join("models")).unwrap();
        std::fs::write(
            root.join("pack.toml"),
            format!(
                "[pack]\nname='{id}'\nversion='0.1.0'\nlicense='MIT'\nprovenance='hand-written'\n"
            ),
        )
        .unwrap();
        std::fs::write(root.join("models").join("device.toml"), card_toml(id)).unwrap();
    }

    fn card_toml(id: &str) -> String {
        format!(
            "[[models]]\nid = \"{id}\"\nkind = \"digital\"\n[models.match]\n\
             value_re = \"(?i)^{id}$\"\n[models.params]\nidentity_only = true\n\
             warning = \"test identity only\"\nunlocked_by = \"test behavior\"\n\
             [models.pins]\n\"1\" = \"in\"\n\"2\" = \"out\"\n\
             [models.coverage]\nmissing = [\"executable_behavior\"]\n"
        )
    }

    #[test]
    fn models_dir_flag_accepts_a_pack_root_a_directory_of_packs_and_loose_cards() {
        // `--models-dir` has three shapes: the pack root itself, a directory
        // holding packs (`models/<pack>/models/`, which used to load nothing:
        // the scan was one level deep), and loose cards, alone or beside packs.
        let root = tempfile::tempdir().unwrap();
        write_pack(root.path(), "pack_root_device");
        let resolved = by_value(
            &ModelLibrary::builtin_with_user_dirs(&[root.path()]),
            "PACK_ROOT_DEVICE",
        );
        assert_eq!(
            resolved.model.map(|m| m.id).as_deref(),
            Some("pack_root_device")
        );
        assert_eq!(resolved.source.as_deref(), Some("user"));

        let parent = tempfile::tempdir().unwrap();
        write_pack(&parent.path().join("generic"), "parent_dir_generic");
        write_pack(&parent.path().join("curated"), "parent_dir_curated");
        std::fs::write(
            parent.path().join("loose.toml"),
            card_toml("parent_dir_loose"),
        )
        .unwrap();
        let packs = ModelLibrary::builtin_with_user_dirs(&[parent.path()]);
        for id in [
            "parent_dir_generic",
            "parent_dir_curated",
            "parent_dir_loose",
        ] {
            assert_eq!(
                by_value(&packs, &id.to_uppercase())
                    .model
                    .map(|m| m.id)
                    .as_deref(),
                Some(id),
            );
        }

        // A directory with nothing to load says so instead of loading zero
        // cards silently, and stays quiet once a card is there.
        let empty = tempfile::tempdir().unwrap();
        std::fs::write(empty.path().join("README.md"), "no cards here\n").unwrap();
        let mut l = lib();
        let errs = l.load_models_dir(empty.path());
        assert!(
            errs.iter().any(|e| matches!(e, ModelError::NoCards { .. })),
            "{errs:?}"
        );
        std::fs::write(
            empty.path().join("card.toml"),
            card_toml("quiet_dir_device"),
        )
        .unwrap();
        assert!(l.load_models_dir(empty.path()).is_empty());
    }

    #[test]
    fn source_policy_ranks_curated_above_extracted_and_estimated_unless_user_overrides() {
        let mut l = ModelLibrary::empty();
        let entry = |id: &str, tier: &str| {
            format!(
                "[[models]]\nid = \"{id}\"\nkind = \"diode\"\n[models.source]\ntier = \"{tier}\"\n\
                 validation = \"physical-bounds-only\"\n[models.match]\nvalue_re = \"^LADDER$\"\n\
                 [models.params]\nis = 1e-9\nn = 1.5\nrs = 0.5\n"
            )
        };
        l.load_toml_str(
            &entry("estimated", "estimated-fallback"),
            "f",
            SourceLayer::Builtin,
        )
        .unwrap();
        l.load_toml_str(
            &entry("extracted", "datasheet-derived"),
            "e",
            SourceLayer::UserDir,
        )
        .unwrap();
        l.load_toml_str(
            &entry("curated", "curated-library"),
            "c",
            SourceLayer::Builtin,
        )
        .unwrap();
        let resolved = by_value(&l, "LADDER");
        assert_eq!(resolved.model.unwrap().id, "curated");
        assert_eq!(
            resolved.provenance.unwrap().tier(),
            ModelSourceTier::CuratedLibrary
        );

        let mut l = lib();
        l.load_toml_str(
            "[[models]]\nid = \"user_bat43\"\nkind = \"diode\"\n[models.match]\nvalue_re = \"^BAT43$\"\n\
             [models.params]\nis = 1e-9\nn = 1.3\nrs = 0.2\n",
            "user-bat43",
            SourceLayer::ModelsDirFlag,
        )
        .unwrap();
        let resolved = by_value(&l, "BAT43");
        assert_eq!(resolved.model.unwrap().id, "user_bat43");
        assert_eq!(
            resolved.provenance.unwrap().tier(),
            ModelSourceTier::UserModel
        );
    }

    /// Resolution against the `pic_programmer` board's BOM.
    #[test]
    fn pic_programmer_bom() {
        let l = lib();
        let bom: &[(&str, &str, &str)] = &[
            (
                "C1",
                "100µF",
                "Capacitor_THT:CP_Axial_L18.0mm_D6.5mm_P25.00mm_Horizontal",
            ),
            (
                "C2",
                "220uF",
                "Capacitor_THT:CP_Axial_L18.0mm_D6.5mm_P25.00mm_Horizontal",
            ),
            (
                "R1",
                "10K",
                "Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm_P10.16mm_Horizontal",
            ),
            ("D2", "BAT43", "Diode_THT:D_DO-35_SOD27_P7.62mm_Horizontal"),
            (
                "D1",
                "1N4004",
                "Diode_THT:D_DO-35_SOD27_P12.70mm_Horizontal",
            ),
            ("D8", "RED-LED", "LED_THT:LED_D5.0mm"),
            ("Q1", "BC237", "footprints:TO-92"),
            (
                "L1",
                "22uH",
                "Inductor_THT:L_Radial_D7.8mm_P5.00mm_Fastron_07HCP",
            ),
            (
                "U3",
                "7805",
                "Package_TO_SOT_THT:TO-220-3_Horizontal_TabDown",
            ),
            ("P101", "CONN_1", "MountingHole:MountingHole_4.3mm_M4"),
            ("C5", "10nF", "Capacitor_THT:C_Disc_D5.1mm_W3.2mm_P5.00mm"),
            ("U2", "74HC125", "Package_DIP:DIP-14_W7.62mm_LongPads"),
            ("U99", "UNKNOWN", ""),
        ];
        let queries: Vec<ComponentQuery> = bom
            .iter()
            .map(|(r, v, fp)| ComponentQuery {
                reference: Some(r.to_string()),
                value: Some(v.to_string()),
                footprint: Some(fp.to_string()),
                ..Default::default()
            })
            .collect();
        let report = l.report(&queries);
        for res in &report.resolutions {
            let r = res.query.reference.as_deref().unwrap_or("");
            assert_eq!(
                res.model.is_some(),
                r != "U99",
                "{r} (value={:?})",
                res.query.value
            );
        }
        let s = report.to_string();
        assert!(s.contains("R1") && s.contains("UNRESOLVED"));
    }

    /// Addressable LEDs bind by value AND footprint: the SK6812MINI and its
    /// -E sibling share a value spelling but not a pinout, and a WS2812B card
    /// must not take the 2020 package's different pin map.
    #[test]
    fn addressable_leds_resolve_by_package() {
        let l = lib();
        let id = |value: &str, fp: &str| {
            let q = ComponentQuery {
                value: Some(value.into()),
                footprint: Some(fp.into()),
                ..Default::default()
            };
            l.resolve(&q).model.map(|m| m.id)
        };
        let ws = "LED_SMD:LED_WS2812B_PLCC4_5.0x5.0mm_P3.2mm";
        assert_eq!(id("WS2812B", ws).as_deref(), Some("ws2812b"));
        assert_eq!(
            id("YS-SK6812MINI", "yuchi_kbd:SK6812MINI-E").as_deref(),
            Some("sk6812mini_e")
        );
        let mini = "LED_SMD:LED_SK6812MINI_PLCC4_3.5x3.5mm_P1.75mm";
        assert_eq!(id("SK6812MINI", mini).as_deref(), Some("sk6812mini"));
        for (value, fp) in [
            ("SK6812MINI-E", mini),
            ("WS2812B", "LED_SMD:LED_WS2812B-2020_PLCC4_2.0x2.0mm"),
        ] {
            let got = id(value, fp);
            assert!(
                !got.as_deref()
                    .is_some_and(|m| m.starts_with("ws2812") || m.starts_with("sk6812")),
                "{value} {fp}: {got:?}"
            );
        }
    }
}
