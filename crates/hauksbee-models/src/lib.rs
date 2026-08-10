//! `hauksbee-models`, PCB component model library.
//!
//! Given a component identified by lib_id, value, footprint, or part-number,
//! this crate resolves a simulation model definition. Physics arrives by
//! several authoring routes (built-in DB, installed model packs, LLM
//! datasheet extraction, hand-written behavioural models, user SPICE) that
//! map onto six explicit priority layers ([`SourceLayer`],
//! 06-extensibility-sdk §3):
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

pub mod behavioral;
pub mod datasheet;
pub mod logic_spec;
pub mod matcher;
pub mod pack;
pub mod param_names;
pub mod pin_rules;
pub mod profile;
pub mod schema;
pub mod sensor_spec;
pub mod spice_input;
pub mod unmodelled;
pub mod validation;
pub mod value;

use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
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
pub use schema::{ComponentKind, ModelEntry, Params, StrapInternalPull, StrapLevel, StrapPin};
pub use sensor_spec::{
    Bus, Encoding, ProtocolStyle, RegisterSpec, Sensor, SensorSpec, SensorSpecError,
};
pub use spice_input::SpiceCard;
pub use unmodelled::{UnmodelledNote, UnmodelledPart, UnmodelledTable};

/// Built-in pin-role inference rules, embedded at compile time.
static BUILTIN_PIN_RULES_TOML: &str = include_str!("../db/pin_rules.toml");
static BUILTIN_UNMODELLED_TOML: &str = include_str!("../db/unmodelled.toml");

// ── Embedded database files ───────────────────────────────────────────────────

/// All built-in TOML database files embedded at compile time.
static BUILTIN_TOML_FILES: &[(&str, &str)] = &[
    ("passives", include_str!("../db/passives.toml")),
    ("diodes", include_str!("../db/diodes.toml")),
    ("bjt", include_str!("../db/bjt.toml")),
    ("mosfet", include_str!("../db/mosfet.toml")),
    (
        "opamp_comparator",
        include_str!("../db/opamp_comparator.toml"),
    ),
    ("analog_switch", include_str!("../db/analog_switch.toml")),
    ("digital", include_str!("../db/digital.toml")),
    ("dac_adc", include_str!("../db/dac_adc.toml")),
    ("vreg", include_str!("../db/vreg.toml")),
    ("power_ics", include_str!("../db/power_ics.toml")),
    ("mcu", include_str!("../db/mcu.toml")),
    ("ignore", include_str!("../db/ignore.toml")),
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

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),
}

// ── Source layers ─────────────────────────────────────────────────────────────

/// The explicit resolution priority layer an entry was loaded from
/// (06-extensibility-sdk §3). A higher-priority layer beats a lower one
/// *regardless of specificity*; specificity only breaks ties within a layer.
/// Before this existed, user-over-builtin worked only because user entries
/// happened to score higher on specificity; now the layer is the comparison's
/// first key, by construction.
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
    /// The plan-mandated priority integer: builtin=0, pack=10, user dir=20,
    /// user config dir=25, `--models-dir`=30, user SPICE=40.
    pub fn priority(self) -> u32 {
        match self {
            SourceLayer::Builtin => 0,
            SourceLayer::Pack => 10,
            SourceLayer::UserDir => 20,
            SourceLayer::UserConfigDir => 25,
            SourceLayer::ModelsDirFlag => 30,
            SourceLayer::Spice => 40,
        }
    }

    /// Short name for reports.
    pub fn name(self) -> &'static str {
        match self {
            SourceLayer::Builtin => "builtin",
            SourceLayer::Pack => "pack",
            SourceLayer::UserDir => "user-dir",
            SourceLayer::UserConfigDir => "user-config-dir",
            SourceLayer::ModelsDirFlag => "models-dir",
            SourceLayer::Spice => "spice",
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
    /// An exact, unambiguous match (e.g. exact lib_id + value + MPN).
    Exact,
    /// Matched by family rules (e.g. value regex catches the whole BC84x family).
    Family,
    /// Heuristic / partial match (e.g. only footprint matched).
    Guessed,
    /// No matching entry found.
    Unresolved,
}

impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Confidence::Exact => write!(f, "exact"),
            Confidence::Family => write!(f, "family"),
            Confidence::Guessed => write!(f, "guessed"),
            Confidence::Unresolved => write!(f, "unresolved"),
        }
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
}

impl Resolution {
    fn unresolved(query: ComponentQuery) -> Self {
        Resolution {
            model: None,
            confidence: Confidence::Unresolved,
            query,
            source: None,
            layer: None,
            origin: None,
            provenance: None,
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
            None => write!(f, "{} ({}) => UNRESOLVED", ref_str, val_str),
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
}

use matcher::CompiledEntry;

impl ModelLibrary {
    /// Create an empty library with no entries.
    pub fn empty() -> Self {
        ModelLibrary {
            entries: Vec::new(),
            spice: Vec::new(),
            pin_rules: PinRuleTable::empty(),
            unmodelled: UnmodelledTable::empty(),
        }
    }

    /// Create a library loaded with all built-in database entries.
    ///
    /// Panics if the embedded TOML is malformed (compile-time guarantee).
    pub fn builtin() -> Self {
        let mut lib = ModelLibrary::empty();
        for (name, toml_src) in BUILTIN_TOML_FILES {
            lib.load_toml_str(toml_src, name, SourceLayer::Builtin)
                .unwrap_or_else(|e| panic!("built-in database '{}' failed to load: {}", name, e));
        }
        lib.pin_rules
            .load_toml_str(BUILTIN_PIN_RULES_TOML, false)
            .unwrap_or_else(|e| panic!("built-in pin_rules.toml failed to load: {e}"));
        lib.unmodelled
            .load_toml_str(BUILTIN_UNMODELLED_TOML, false)
            .unwrap_or_else(|e| panic!("built-in unmodelled.toml failed to load: {e}"));
        lib
    }

    /// The pin-role inference table (built-in rules plus any user
    /// `pin_rules.toml`). Consulted by the binder when a pad lacks an explicit
    /// pin-function role.
    pub fn pin_rules(&self) -> &PinRuleTable {
        &self.pin_rules
    }

    /// The named-abstention table (built-in plus any user `unmodelled.toml`).
    ///
    /// Consulted by the binder ONLY when nothing resolved, and only to replace the
    /// disclosure's two generic sentences with specific ones. It cannot make a part
    /// bind; see `unmodelled.rs` for why that containment is the point.
    pub fn unmodelled(&self) -> &UnmodelledTable {
        &self.unmodelled
    }

    /// Lazily-initialised shared built-in library.
    pub fn builtin_shared() -> &'static ModelLibrary {
        static LIB: Lazy<ModelLibrary> = Lazy::new(ModelLibrary::builtin);
        &LIB
    }

    /// Built-in library plus installed packs plus the user model directories,
    /// in explicit layer order (lowest to highest priority):
    ///   0. the embedded builtin db,
    ///   1. installed packs (`~/.hauksbee/packs`, recorded in `packs.toml`),
    ///   2. `~/.hauksbee/models`, where datasheet extraction writes,
    ///   3. `~/.config/hauksbee/models`; the user's own custom models,
    ///   4. each `extra_dirs` entry, e.g. a `--models-dir` flag, highest.
    /// A custom behavioural part dropped into one of these loads without
    /// recompiling. Directory and pack load errors are warned to stderr, not
    /// fatal, so a single malformed user file never breaks the whole library.
    pub fn builtin_with_user_dirs(extra_dirs: &[&Path]) -> ModelLibrary {
        let mut lib = ModelLibrary::builtin();
        if let Some(store) = PackStore::default_location() {
            for w in lib.load_packs(&store) {
                eprintln!("[models] packs: {w}");
            }
        }
        let home = std::env::var("HOME").ok().map(PathBuf::from);
        if let Some(h) = &home {
            // The two user dirs load at DISTINCT layers so `~/.config/hauksbee/
            // models` (a hand-corrected model) deterministically overrides a
            // same-id auto-extracted one in `~/.hauksbee/models`. Loading both
            // at one layer left a same-id collision to be resolved silently.
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
            for e in lib.load_dir_layer(dir, SourceLayer::ModelsDirFlag) {
                eprintln!("[models] --models-dir {}: {e}", dir.display());
            }
        }
        lib
    }

    /// Load every installed pack from `store` at [`SourceLayer::Pack`].
    ///
    /// Returns human-readable warnings; nothing here is fatal (a library must
    /// survive one bad installed pack), but nothing is silent either:
    ///   - a recorded pack whose dir fails validation is skipped with a warning;
    ///   - two packs shipping the same model id is a same-layer conflict,
    ///     reported naming both packs (the plan forbids resolving it quietly,
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
            let dir = store.pack_dir(record);
            let pack = match pack::Pack::load(&dir) {
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
            for file in &pack.model_files {
                let src = match std::fs::read_to_string(file) {
                    Ok(s) => s,
                    Err(e) => {
                        warnings.push(format!(
                            "pack '{origin}': reading '{}': {e}",
                            file.display()
                        ));
                        continue;
                    }
                };
                let before = self.entries.len();
                let pack_tier = match pack.manifest.provenance {
                    Provenance::Vendor => ModelSourceTier::VendorSpice,
                    Provenance::HandWritten => ModelSourceTier::CuratedPack,
                    Provenance::DatasheetExtracted => ModelSourceTier::DatasheetDerived,
                };
                if let Err(e) =
                    self.load_toml_str_with_tier(&src, &origin, SourceLayer::Pack, Some(pack_tier))
                {
                    warnings.push(format!("pack '{origin}': {e}"));
                    continue;
                }
                for le in &self.entries[before..] {
                    let id = le.compiled.entry.id.clone();
                    if let Some(other) = seen.get(&id) {
                        if *other != origin {
                            warnings.push(format!(
                                "same-layer conflict: model id '{id}' is shipped by both \
                                 pack '{other}' and pack '{origin}'; within the pack layer \
                                 nothing orders them; remove one pack or rename the entry"
                            ));
                        }
                    } else {
                        seen.insert(id, origin.clone());
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
            // is loaded into the pin-role inference table, prepended so user
            // rules override the built-ins. Everything else is model entries.
            if name == "pin_rules" || src.contains("[[pin_rules]]") {
                if let Err(e) = self.pin_rules.load_toml_str(&src, true) {
                    errors.push(ModelError::PinRules {
                        file: name,
                        message: e,
                    });
                }
                continue;
            }
            // An `unmodelled.toml` (or any file carrying an `[[unmodelled]]`
            // array) is loaded into the abstention table, prepended so a user can
            // override a built-in abstention's text. Same shape as pin_rules.
            if name == "unmodelled" || src.contains("[[unmodelled]]") {
                if let Err(e) = self.unmodelled.load_toml_str(&src, true) {
                    errors.push(ModelError::PinRules {
                        file: name,
                        message: e,
                    });
                }
                continue;
            }
            if let Err(e) = self.load_toml_str(&src, &name, layer) {
                errors.push(e);
            }
        }
        errors
    }

    /// Add all `.model` and `.subckt` cards from a SPICE file.
    pub fn add_spice_file(&mut self, path: &Path) -> anyhow::Result<usize> {
        let cards = spice_input::parse_spice_file(path)?;
        let n = cards.len();
        self.spice.extend(cards);
        Ok(n)
    }

    /// Add a SPICE card directly (for testing).
    pub fn add_spice_card(&mut self, card: SpiceCard) {
        self.spice.push(card);
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
        // Layer 40: SPICE cards, match by card name against value and MPN.
        if let Some(card) = self.find_spice_match(q) {
            return self.resolution_from_spice(card, q.clone());
        }

        // Layers 0..30: one scan, best (layer priority, specificity,
        // regex constrainedness, smallest id) wins. The last two keys make
        // ties deterministic and independent of load order.
        use std::cmp::Reverse;
        let sort_key = |le: &'_ LayeredEntry, score: u32| {
            (
                le.provenance.tier().priority(),
                le.layer.priority(),
                score,
                le.compiled.regex_specificity(),
                Reverse(le.compiled.entry.id.clone()),
            )
        };
        let mut best: Option<(&LayeredEntry, u32)> = None;
        for le in &self.entries {
            if !le.compiled.matches(q) {
                continue;
            }
            let score = le.compiled.specificity_score(q);
            if best
                .as_ref()
                .map(|(b, s)| sort_key(le, score) > sort_key(b, *s))
                .unwrap_or(true)
            {
                best = Some((le, score));
            }
        }
        if let Some((le, score)) = best {
            let source = match le.layer {
                SourceLayer::Builtin => "builtin",
                SourceLayer::Pack => "pack",
                // Both user layers keep the historical "user" string.
                SourceLayer::UserDir | SourceLayer::UserConfigDir | SourceLayer::ModelsDirFlag => {
                    "user"
                }
                SourceLayer::Spice => "spice",
            };
            return Resolution {
                model: Some(le.compiled.entry.clone()),
                confidence: score_to_confidence(score),
                query: q.clone(),
                source: Some(source.to_string()),
                layer: Some(le.layer),
                origin: Some(le.origin.clone()),
                provenance: Some(le.provenance.clone()),
            };
        }

        Resolution::unresolved(q.clone())
    }

    /// Resolve a batch of queries and return a resolution report table.
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
        let db_file: schema::DbFile = toml::from_str(src).map_err(|e| ModelError::TomlParse {
            source: source_name.to_string(),
            error: e,
        })?;

        #[derive(serde::Deserialize)]
        struct SourceFile {
            #[serde(default)]
            models: Vec<SourceRow>,
        }
        #[derive(serde::Deserialize)]
        struct SourceRow {
            id: String,
            #[serde(default)]
            source: Option<schema::ModelSourceSpec>,
        }
        let declarations: std::collections::HashMap<String, schema::ModelSourceSpec> =
            toml::from_str::<SourceFile>(src)
                .map(|file| {
                    file.models
                        .into_iter()
                        .filter_map(|row| row.source.map(|source| (row.id, source)))
                        .collect()
                })
                .unwrap_or_default();

        for entry in db_file.models {
            let id = entry.id.clone();
            // Fail loud on an entry with no match rules. An all-None `[match]`
            // block (an omitted section, or a typo'd key like `mch_re` that serde
            // silently drops) matches EVERY component at specificity 0. Since the
            // user layer outranks pack/builtin in `resolve`'s sort key, one such
            // stray entry would silently rebind the whole board to it, a
            // wholesale-wrong simulation with no diagnostic. The schema documents
            // "at least one rule must be populated"; enforce it here.
            if entry.r#match.is_empty() {
                return Err(ModelError::ValidationFailed {
                    id,
                    messages: "entry has no match rules (lib_id / value_re / \
                               footprint_re / mpn_re all absent); at least one is \
                               required, else it would match every component"
                        .to_string(),
                });
            }
            let compiled = CompiledEntry::compile(entry).map_err(|e| ModelError::InvalidRegex {
                id: id.clone(),
                error: e,
            })?;
            let declaration = declarations.get(&compiled.entry.id);
            // Pack-manifest provenance is authoritative. An entry in a
            // datasheet-derived pack cannot promote itself to a vendor tier.
            let tier = default_tier
                .or_else(|| declaration.map(|source| source.tier))
                .unwrap_or_else(|| default_source_tier(layer));
            let validation = declaration
                .map(|source| source.validation)
                .unwrap_or_else(|| default_validation(layer));
            let uncertainty = declaration
                .map(|source| source.uncertainty.clone())
                .filter(|values| !values.is_empty())
                .unwrap_or_else(|| {
                    vec![ModelUncertainty::unknown(
                        format!("{}.model", compiled.entry.id),
                        "the source publishes no validated numeric error interval",
                    )
                    .expect("static unknown uncertainty is valid")]
                });
            let provenance = ModelSource::new(
                tier,
                model_layer(layer),
                source_name,
                validation,
                uncertainty,
            )
            .map_err(|error| ModelError::ValidationFailed {
                id: compiled.entry.id.clone(),
                messages: format!("invalid [models.source]: {error}"),
            })?;
            self.entries.push(LayeredEntry {
                layer,
                origin: source_name.to_string(),
                provenance,
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

    fn resolution_from_spice(&self, card: &SpiceCard, query: ComponentQuery) -> Resolution {
        // Build a synthetic ModelEntry from the SPICE card
        use schema::{MatchRules, ModelEntry, Params};
        use std::collections::BTreeMap;

        // A `.subckt` is a multi-terminal macro, not a single-device kind, and
        // an unrecognized `.model` type must not be silently claimed as a
        // passive with Exact confidence (that shadowed the real part). Both
        // resolve to Unresolved instead. (R8 #10)
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
        // For subckt: store port names as pin map
        let mut pins = BTreeMap::new();
        for (i, port) in card.ports.iter().enumerate() {
            pins.insert((i + 1).to_string(), port.clone());
        }

        let entry = ModelEntry {
            id: card.name.to_lowercase(),
            kind,
            description: format!("User SPICE card: {}", card.name),
            r#match: MatchRules::default(),
            params,
            pins,
            ratings: Default::default(),
            straps: Vec::new(),
            behavioral: Default::default(),
            logic: Default::default(),
            current_program: None,
            passive_class: None,
        };

        Resolution {
            model: Some(entry),
            confidence: Confidence::Exact,
            query,
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
        }
    }
}

fn model_layer(layer: SourceLayer) -> ModelLayer {
    match layer {
        SourceLayer::Builtin => ModelLayer::Builtin,
        SourceLayer::Pack => ModelLayer::Pack,
        SourceLayer::UserDir => ModelLayer::UserDir,
        SourceLayer::UserConfigDir => ModelLayer::UserConfigDir,
        SourceLayer::ModelsDirFlag => ModelLayer::ModelsDir,
        SourceLayer::Spice => ModelLayer::Spice,
    }
}

fn default_source_tier(layer: SourceLayer) -> ModelSourceTier {
    match layer {
        SourceLayer::Builtin => ModelSourceTier::CuratedLibrary,
        SourceLayer::Pack => ModelSourceTier::CuratedPack,
        SourceLayer::UserDir => ModelSourceTier::DatasheetDerived,
        SourceLayer::UserConfigDir | SourceLayer::ModelsDirFlag => ModelSourceTier::UserModel,
        // A loose card is user supplied. Vendor provenance is established
        // only by a licensed vendor pack manifest.
        SourceLayer::Spice => ModelSourceTier::UserModel,
    }
}

fn default_validation(layer: SourceLayer) -> ModelValidation {
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
fn score_to_confidence(score: u32) -> Confidence {
    if score >= 50 {
        Confidence::Exact
    } else if score >= 20 {
        Confidence::Family
    } else {
        Confidence::Guessed
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
    /// Count resolutions by confidence level.
    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let exact = self
            .resolutions
            .iter()
            .filter(|r| r.confidence == Confidence::Exact)
            .count();
        let family = self
            .resolutions
            .iter()
            .filter(|r| r.confidence == Confidence::Family)
            .count();
        let guessed = self
            .resolutions
            .iter()
            .filter(|r| r.confidence == Confidence::Guessed)
            .count();
        let unresolved = self
            .resolutions
            .iter()
            .filter(|r| r.confidence == Confidence::Unresolved)
            .count();
        (exact, family, guessed, unresolved)
    }

    /// All unresolved queries.
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
        writeln!(
            f,
            "┌──────────────────────────────────────────────────────────────┐"
        )?;
        writeln!(
            f,
            "│ Resolution report: {} components",
            self.resolutions.len()
        )?;
        writeln!(
            f,
            "│  exact={exact}  family={family}  guessed={guessed}  unresolved={unresolved}"
        )?;
        writeln!(
            f,
            "├──────────┬──────────────────────────┬────────────┬────────────┤"
        )?;
        writeln!(
            f,
            "│ Ref      │ Value                    │ Model ID   │ Confidence │"
        )?;
        writeln!(
            f,
            "├──────────┼──────────────────────────┼────────────┼────────────┤"
        )?;
        for res in &self.resolutions {
            let ref_s = res.query.reference.as_deref().unwrap_or("?");
            let val_s = res.query.value.as_deref().unwrap_or("");
            let (model_id, conf) = match &res.model {
                Some(m) => (m.id.as_str(), res.confidence.to_string()),
                None => ("UNRESOLVED", "unresolved".to_string()),
            };
            // Truncate on CHAR boundaries, a byte slice like `&val_s[..24]`
            // panics when a multibyte char (e.g. 'µ' in a "µF" value) straddles
            // the cut point.
            let clip = |s: &str, n: usize| -> String { s.chars().take(n).collect() };
            writeln!(
                f,
                "│ {:<8} │ {:<24} │ {:<10} │ {:<10} │",
                clip(ref_s, 8),
                clip(val_s, 24),
                clip(model_id, 10),
                clip(&conf, 10),
            )?;
        }
        writeln!(
            f,
            "└──────────┴──────────────────────────┴────────────┴────────────┘"
        )?;
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn lib() -> ModelLibrary {
        ModelLibrary::builtin()
    }

    #[test]
    fn builtin_loads_without_panic() {
        let _l = lib();
    }

    #[test]
    fn entry_without_match_rules_is_rejected_loud() {
        // R31: an all-None `[match]` block (an omitted section, or a typo'd key
        // like `mch_re` that serde silently drops) matches EVERY component at
        // specificity 0. Since the user layer outranks pack/builtin, one such
        // stray entry would silently rebind the whole board to it. Loading must
        // fail loud instead of silently accepting a universal catch-all.
        let mut lib = ModelLibrary::builtin();
        let stray = r#"
            [[models]]
            id = "stray"
            kind = "passive"
        "#;
        let err = lib
            .load_toml_str(stray, "stray.toml", SourceLayer::UserDir)
            .expect_err("an entry with no match rules must be rejected");
        assert!(
            matches!(&err, ModelError::ValidationFailed { id, .. } if id == "stray"),
            "must be a ValidationFailed naming the stray entry, got: {err:?}"
        );

        // A well-formed entry (one match rule) still loads.
        let ok = r#"
            [[models]]
            id = "real"
            kind = "passive"
            [models.match]
            lib_id = "Device:R"
        "#;
        assert!(
            lib.load_toml_str(ok, "real.toml", SourceLayer::UserDir)
                .is_ok(),
            "an entry with a populated match rule must load"
        );
    }

    /// Round-7 #14: the report's Display truncates long fields to fit the table;
    /// slicing a value string at a fixed BYTE index panics when a multibyte char
    /// straddles it. A "µF" value longer than the 24-col cell must render, not
    /// crash.
    #[test]
    fn resolution_report_display_survives_multibyte_value() {
        // 23 ASCII + 'µ' (2 bytes) puts a char boundary across byte index 24.
        let value = format!("{}{}", "a".repeat(23), "µF");
        let q = ComponentQuery {
            value: Some(value),
            ..Default::default()
        };
        let report = ResolutionReport {
            resolutions: vec![Resolution::unresolved(q)],
        };
        let rendered = report.to_string();
        assert!(
            rendered.contains("Resolution report"),
            "report renders: {rendered}"
        );
    }

    #[test]
    fn resolve_resistor() {
        let l = lib();
        let q = ComponentQuery::new(Some("Device:R".to_string()), Some("10k".to_string()), None);
        let res = l.resolve(&q);
        // Exact lib_id match with no value_re in the rule resolves at Family
        // confidence (the entry covers the whole Device:R class, not a specific part).
        assert!(
            res.confidence <= Confidence::Family,
            "expected Family or better, got {:?}",
            res.confidence
        );
        assert_eq!(res.model.unwrap().kind, ComponentKind::Passive);
    }

    #[test]
    fn resolve_bc847() {
        let l = lib();
        let q = ComponentQuery {
            value: Some("BC847".to_string()),
            ..Default::default()
        };
        let res = l.resolve(&q);
        assert!(res.model.is_some(), "expected BC847 to resolve");
        assert_eq!(res.model.unwrap().kind, ComponentKind::BjtNpn);
    }

    #[test]
    fn resolve_1n4148() {
        let l = lib();
        let q = ComponentQuery {
            value: Some("1N4148".to_string()),
            ..Default::default()
        };
        let res = l.resolve(&q);
        assert!(res.model.is_some());
        assert_eq!(res.model.unwrap().kind, ComponentKind::Diode);
    }

    #[test]
    fn resolve_74hc595() {
        let l = lib();
        let q = ComponentQuery {
            value: Some("74HC595".to_string()),
            ..Default::default()
        };
        let res = l.resolve(&q);
        assert!(res.model.is_some());
        assert_eq!(res.model.unwrap().kind, ComponentKind::ShiftRegister);
    }

    #[test]
    fn at28c256_fast_write_time_requires_explicit_f_identity() {
        let l = lib();
        let standard = l
            .resolve(&ComponentQuery {
                value: Some("28C256".to_string()),
                ..Default::default()
            })
            .model
            .expect("generic EEPROM resolves");
        let fast = l
            .resolve(&ComponentQuery {
                value: Some("AT28C256F-15JU".to_string()),
                ..Default::default()
            })
            .model
            .expect("explicit F EEPROM resolves");
        assert_eq!(standard.id, "eeprom_28c256");
        assert_eq!(standard.logic.memories[0].program_time_s, Some(0.010));
        assert_eq!(fast.id, "eeprom_at28c256f");
        assert_eq!(fast.logic.memories[0].program_time_s, Some(0.003));
    }

    #[test]
    fn resolve_mounting_hole_is_ignore() {
        let l = lib();
        let q = ComponentQuery {
            footprint: Some("MountingHole:MountingHole_4.3mm_M4".to_string()),
            ..Default::default()
        };
        let res = l.resolve(&q);
        assert!(res.model.is_some());
        assert_eq!(res.model.unwrap().kind, ComponentKind::Ignore);
    }

    #[test]
    fn unresolved_is_loud() {
        let l = lib();
        let q = ComponentQuery {
            value: Some("UNKNOWNPART_XYZ999".to_string()),
            reference: Some("U99".to_string()),
            ..Default::default()
        };
        let res = l.resolve(&q);
        assert_eq!(res.confidence, Confidence::Unresolved);
        assert!(res.model.is_none());
    }

    #[test]
    fn spice_card_overrides_builtin() {
        let mut l = lib();
        let card = SpiceCard {
            name: "BC847".to_string(),
            kind: spice_input::SpiceCardKind::Model,
            raw: ".MODEL BC847 NPN(IS=2E-14)".to_string(),
            ports: Vec::new(),
            params: [("IS".to_string(), 2e-14_f64)].into_iter().collect(),
            model_type: Some("NPN".to_string()),
        };
        l.add_spice_card(card);
        let q = ComponentQuery {
            value: Some("BC847".to_string()),
            ..Default::default()
        };
        let res = l.resolve(&q);
        assert_eq!(res.source.as_deref(), Some("spice"));
        assert_eq!(
            res.provenance.as_ref().unwrap().tier(),
            ModelSourceTier::UserModel,
            "a loose SPICE card is a user override, not an unverified vendor claim"
        );
    }

    /// Round-8 #9: `~/.config/hauksbee/models` must sit ABOVE `~/.hauksbee/
    /// models` so a hand-corrected model there deterministically overrides an
    /// auto-extracted same-id one. The two user dirs must be distinct layers.
    #[test]
    fn user_config_dir_outranks_user_dir() {
        assert!(
            SourceLayer::UserConfigDir.priority() > SourceLayer::UserDir.priority(),
            "~/.config/hauksbee/models must outrank ~/.hauksbee/models"
        );
        assert!(
            SourceLayer::UserConfigDir.priority() < SourceLayer::ModelsDirFlag.priority(),
            "--models-dir still wins over the config dir"
        );
    }

    /// U3: an explicit `--models-dir` pointing at a nonexistent path is a
    /// user typo; it must produce a loud error (so the CLI's eprintln fires),
    /// not silently load zero models. The auto-discovered user dirs, by
    /// contrast, may legitimately be absent and must stay silent.
    #[test]
    fn missing_models_dir_flag_reports_but_auto_dirs_stay_silent() {
        let mut l = lib();
        let missing = Path::new("/nonexistent/hauksbee/models/typo");
        let flag_errs = l.load_dir_layer(missing, SourceLayer::ModelsDirFlag);
        assert!(
            flag_errs
                .iter()
                .any(|e| matches!(e, ModelError::MissingDir { .. })),
            "an explicit --models-dir typo must report MissingDir, got: {flag_errs:?}"
        );
        for auto in [SourceLayer::UserDir, SourceLayer::UserConfigDir] {
            assert!(
                l.load_dir_layer(missing, auto).is_empty(),
                "an absent auto-discovered dir ({auto:?}) must stay silent"
            );
        }
    }

    /// Round-8 #10: a `.subckt`, and an unrecognized `.model` type, must NOT be
    /// silently imported as a Passive with Exact confidence (that shadowed the
    /// real part). Both resolve Unresolved; a genuine passive `.model R` still
    /// resolves.
    #[test]
    fn spice_subckt_and_unknown_model_do_not_masquerade_as_passive() {
        let subckt = SpiceCard {
            name: "MYOPAMP".to_string(),
            kind: spice_input::SpiceCardKind::Subckt,
            raw: ".SUBCKT MYOPAMP INP INN VCC VEE OUT".to_string(),
            ports: vec![
                "INP".into(),
                "INN".into(),
                "VCC".into(),
                "VEE".into(),
                "OUT".into(),
            ],
            params: Default::default(),
            model_type: None,
        };
        let mut l = lib();
        l.add_spice_card(subckt);
        let res = l.resolve(&ComponentQuery {
            value: Some("MYOPAMP".to_string()),
            ..Default::default()
        });
        assert!(
            res.model.is_none(),
            "a subckt must not resolve to a Passive model"
        );
        assert_eq!(res.confidence, Confidence::Unresolved);

        let vdmos = SpiceCard {
            name: "M1".to_string(),
            kind: spice_input::SpiceCardKind::Model,
            raw: ".MODEL M1 VDMOS(...)".to_string(),
            ports: Vec::new(),
            params: Default::default(),
            model_type: Some("VDMOS".to_string()),
        };
        let mut l2 = lib();
        l2.add_spice_card(vdmos);
        let res2 = l2.resolve(&ComponentQuery {
            value: Some("M1".to_string()),
            ..Default::default()
        });
        assert!(
            res2.model.is_none(),
            "an unknown .model type must not resolve to Passive"
        );

        // A genuine passive .model R still resolves.
        let rmod = SpiceCard {
            name: "RMOD".to_string(),
            kind: spice_input::SpiceCardKind::Model,
            raw: ".MODEL RMOD R (...)".to_string(),
            ports: Vec::new(),
            params: Default::default(),
            model_type: Some("R".to_string()),
        };
        let mut l3 = lib();
        l3.add_spice_card(rmod);
        let res3 = l3.resolve(&ComponentQuery {
            value: Some("RMOD".to_string()),
            ..Default::default()
        });
        assert!(
            res3.model.is_some(),
            "a .model R is a genuine passive and must resolve"
        );
    }

    #[test]
    fn report_display() {
        let l = lib();
        let queries = vec![
            ComponentQuery {
                reference: Some("R1".to_string()),
                value: Some("10k".to_string()),
                footprint: Some(
                    "Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm_P10.16mm_Horizontal".to_string(),
                ),
                ..Default::default()
            },
            ComponentQuery {
                reference: Some("D1".to_string()),
                value: Some("1N4148".to_string()),
                ..Default::default()
            },
            ComponentQuery {
                reference: Some("U99".to_string()),
                value: Some("UNKNOWN".to_string()),
                ..Default::default()
            },
        ];
        let report = l.report(&queries);
        let s = report.to_string();
        assert!(s.contains("R1"));
        assert!(s.contains("1N4148"));
        assert!(s.contains("UNRESOLVED"));
    }

    #[test]
    fn source_policy_places_curated_models_above_extracted_and_estimated_models() {
        let mut l = ModelLibrary::empty();
        let entry = |id: &str, tier: &str| {
            format!(
                r#"
[[models]]
id = "{id}"
kind = "diode"
[models.source]
tier = "{tier}"
validation = "physical-bounds-only"
[models.match]
value_re = "^LADDER$"
[models.params]
is = 1e-9
n = 1.5
rs = 0.5
"#
            )
        };
        l.load_toml_str(
            &entry("estimated", "estimated-fallback"),
            "fallback",
            SourceLayer::Builtin,
        )
        .unwrap();
        l.load_toml_str(
            &entry("extracted", "datasheet-derived"),
            "extracted",
            SourceLayer::UserDir,
        )
        .unwrap();
        l.load_toml_str(
            &entry("curated", "curated-library"),
            "curated",
            SourceLayer::Builtin,
        )
        .unwrap();

        let resolved = l.resolve(&ComponentQuery::new(None, Some("LADDER".into()), None));
        assert_eq!(resolved.model.unwrap().id, "curated");
        assert_eq!(
            resolved.provenance.unwrap().tier(),
            hauksbee_ir::evidence::ModelSourceTier::CuratedLibrary
        );
    }

    #[test]
    fn explicit_user_model_can_override_the_accuracy_ladder() {
        let mut l = ModelLibrary::builtin();
        l.load_toml_str(
            r#"
[[models]]
id = "user_bat43"
kind = "diode"
[models.match]
value_re = "^BAT43$"
[models.params]
is = 1e-9
n = 1.3
rs = 0.2
"#,
            "user-bat43",
            SourceLayer::ModelsDirFlag,
        )
        .unwrap();
        let resolved = l.resolve(&ComponentQuery::new(None, Some("BAT43".into()), None));
        assert_eq!(resolved.model.unwrap().id, "user_bat43");
        assert_eq!(
            resolved.provenance.unwrap().tier(),
            hauksbee_ir::evidence::ModelSourceTier::UserModel
        );
    }

    /// Test resolution against components actually found in the pic_programmer board.
    #[test]
    fn pic_programmer_bom() {
        let l = lib();
        // Components extracted from the pic_programmer.kicad_pcb
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
            (
                "R10",
                "5,1K",
                "Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm_P10.16mm_Horizontal",
            ),
            ("D2", "BAT43", "Diode_THT:D_DO-35_SOD27_P7.62mm_Horizontal"),
            (
                "D1",
                "1N4004",
                "Diode_THT:D_DO-35_SOD27_P12.70mm_Horizontal",
            ),
            ("D8", "RED-LED", "LED_THT:LED_D5.0mm"),
            ("D9", "GREEN-LED", "LED_THT:LED_D5.0mm"),
            ("Q1", "BC237", "footprints:TO-92"),
            ("Q3", "BC307", "footprints:TO-92"),
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

        // Print the report for inspection during test runs
        println!("{}", report);

        let (exact, family, guessed, unresolved) = report.counts();
        println!(
            "pic_programmer: exact={exact} family={family} guessed={guessed} unresolved={unresolved}"
        );

        // The components that should definitely resolve
        let must_resolve = [
            "C1", "C2", "R1", "D2", "D1", "D8", "D9", "Q1", "L1", "U3", "P101", "C5", "U2",
        ];
        for res in &report.resolutions {
            let r = res.query.reference.as_deref().unwrap_or("");
            if must_resolve.contains(&r) {
                assert!(
                    res.model.is_some(),
                    "component {} (value={:?}) should have resolved but didn't",
                    r,
                    res.query.value
                );
            }
        }

        // Unresolved count should be 0 for the list above
        let unresolved_list = report.unresolved();
        let unresolved_refs: Vec<_> = unresolved_list
            .iter()
            .filter(|r| must_resolve.contains(&r.query.reference.as_deref().unwrap_or("")))
            .collect();
        assert!(
            unresolved_refs.is_empty(),
            "expected no unresolved in must_resolve set, got: {:?}",
            unresolved_refs
                .iter()
                .map(|r| r.query.reference.as_deref())
                .collect::<Vec<_>>()
        );
    }
}
