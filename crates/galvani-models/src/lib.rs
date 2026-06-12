//! `galvani-models` — PCB component model library.
//!
//! Given a component identified by lib_id, value, footprint, or part-number,
//! this crate resolves a simulation model definition. Three sources, in
//! priority order:
//!
//! 1. User-supplied SPICE models / `.subckt` cards.
//! 2. Built-in defaults database (TOML files embedded at compile time).
//! 3. Datasheet extraction via the `model-extract` binary (external).
//!
//! # Quick start
//!
//! ```rust
//! use galvani_models::{ModelLibrary, ComponentQuery};
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

pub mod matcher;
pub mod profile;
pub mod schema;
pub mod spice_input;
pub mod validation;
pub mod value;

use std::path::Path;

use once_cell::sync::Lazy;
use regex::Error as RegexError;
use thiserror::Error;

pub use matcher::ComponentQuery;
pub use profile::{LoadProfile, Segment};
pub use schema::{ComponentKind, ModelEntry, Params};
pub use spice_input::SpiceCard;

// ── Embedded database files ───────────────────────────────────────────────────

/// All built-in TOML database files embedded at compile time.
static BUILTIN_TOML_FILES: &[(&str, &str)] = &[
    ("passives",          include_str!("../db/passives.toml")),
    ("diodes",            include_str!("../db/diodes.toml")),
    ("bjt",               include_str!("../db/bjt.toml")),
    ("mosfet",            include_str!("../db/mosfet.toml")),
    ("opamp_comparator",  include_str!("../db/opamp_comparator.toml")),
    ("analog_switch",     include_str!("../db/analog_switch.toml")),
    ("digital",           include_str!("../db/digital.toml")),
    ("dac_adc",           include_str!("../db/dac_adc.toml")),
    ("vreg",              include_str!("../db/vreg.toml")),
    ("mcu",               include_str!("../db/mcu.toml")),
    ("ignore",            include_str!("../db/ignore.toml")),
];

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("TOML parse error in '{source}': {error}")]
    TomlParse { source: String, #[source] error: toml::de::Error },

    #[error("invalid regex in entry '{id}': {error}")]
    InvalidRegex { id: String, error: RegexError },

    #[error("validation failed for '{id}': {messages}")]
    ValidationFailed { id: String, messages: String },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),
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
            Confidence::Exact      => write!(f, "exact"),
            Confidence::Family     => write!(f, "family"),
            Confidence::Guessed    => write!(f, "guessed"),
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
    /// Source that produced the match: `"builtin"`, `"user"`, or `"spice"`.
    pub source: Option<String>,
}

impl Resolution {
    fn unresolved(query: ComponentQuery) -> Self {
        Resolution { model: None, confidence: Confidence::Unresolved, query, source: None }
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

/// The component model library.
///
/// Holds compiled match rules from the built-in database plus any user-loaded
/// entries and SPICE cards.
pub struct ModelLibrary {
    /// Compiled entries from the built-in database (highest priority after SPICE).
    builtin: Vec<CompiledEntry>,
    /// User-provided entries loaded from runtime TOML files.
    user: Vec<CompiledEntry>,
    /// User-provided SPICE cards (`.model` / `.subckt`).
    spice: Vec<SpiceCard>,
}

use matcher::CompiledEntry;

impl ModelLibrary {
    /// Create an empty library with no entries.
    pub fn empty() -> Self {
        ModelLibrary { builtin: Vec::new(), user: Vec::new(), spice: Vec::new() }
    }

    /// Create a library loaded with all built-in database entries.
    ///
    /// Panics if the embedded TOML is malformed (compile-time guarantee).
    pub fn builtin() -> Self {
        let mut lib = ModelLibrary::empty();
        for (name, toml_src) in BUILTIN_TOML_FILES {
            lib.load_toml_str(toml_src, name, false)
                .unwrap_or_else(|e| panic!("built-in database '{}' failed to load: {}", name, e));
        }
        lib
    }

    /// Lazily-initialised shared built-in library.
    pub fn builtin_shared() -> &'static ModelLibrary {
        static LIB: Lazy<ModelLibrary> = Lazy::new(ModelLibrary::builtin);
        &LIB
    }

    /// Append entries from a user TOML directory at runtime.
    ///
    /// Every `.toml` file in `dir` is parsed and validated. Files that fail
    /// are reported but do not prevent other files from loading.
    pub fn with_user_dir(mut self, dir: &Path) -> Result<Self, Vec<ModelError>> {
        let mut errors = Vec::new();
        if !dir.exists() {
            return Ok(self);
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => return Err(vec![ModelError::Io(e)]),
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
            let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string();
            if let Err(e) = self.load_toml_str(&src, &name, true) {
                errors.push(e);
            }
        }
        if errors.is_empty() {
            Ok(self)
        } else {
            Err(errors)
        }
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
    /// Priority order:
    /// 1. User SPICE cards (by value / MPN matching the card name).
    /// 2. User runtime TOML entries.
    /// 3. Built-in TOML database entries.
    ///
    /// When multiple entries match, the one with the highest specificity score
    /// wins. Ties are broken by insertion order (first wins).
    pub fn resolve(&self, q: &ComponentQuery) -> Resolution {
        // 1. SPICE cards — match by card name against value and MPN
        if let Some(card) = self.find_spice_match(q) {
            return self.resolution_from_spice(card, q.clone());
        }

        // 2. User entries (priority over builtin)
        if let Some((entry, score)) = best_match(&self.user, q) {
            return Resolution {
                model: Some(entry.clone()),
                confidence: score_to_confidence(score),
                query: q.clone(),
                source: Some("user".to_string()),
            };
        }

        // 3. Built-in entries
        if let Some((entry, score)) = best_match(&self.builtin, q) {
            return Resolution {
                model: Some(entry.clone()),
                confidence: score_to_confidence(score),
                query: q.clone(),
                source: Some("builtin".to_string()),
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
        is_user: bool,
    ) -> Result<(), ModelError> {
        let db_file: schema::DbFile = toml::from_str(src).map_err(|e| ModelError::TomlParse {
            source: source_name.to_string(),
            error: e,
        })?;

        for entry in db_file.models {
            let id = entry.id.clone();
            let compiled = CompiledEntry::compile(entry).map_err(|e| ModelError::InvalidRegex {
                id: id.clone(),
                error: e,
            })?;
            if is_user {
                self.user.push(compiled);
            } else {
                self.builtin.push(compiled);
            }
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

        let kind = spice_kind_from_model_type(card.model_type.as_deref());
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
        };

        Resolution {
            model: Some(entry),
            confidence: Confidence::Exact,
            query,
            source: Some("spice".to_string()),
        }
    }
}

/// Pick the best-matching compiled entry for a query.
fn best_match<'a>(entries: &'a [CompiledEntry], q: &ComponentQuery) -> Option<(&'a ModelEntry, u32)> {
    let mut best: Option<(&ModelEntry, u32)> = None;
    for compiled in entries {
        if compiled.matches(q) {
            let score = compiled.specificity_score(q);
            if best.map(|(_, s)| score > s).unwrap_or(true) {
                best = Some((&compiled.entry, score));
            }
        }
    }
    best
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

/// Infer a ComponentKind from a SPICE `.model` type string.
fn spice_kind_from_model_type(t: Option<&str>) -> ComponentKind {
    match t.unwrap_or("").to_uppercase().as_str() {
        "D"                       => ComponentKind::Diode,
        "NPN"                     => ComponentKind::BjtNpn,
        "PNP"                     => ComponentKind::BjtPnp,
        "NMOS" | "NMOSFET"        => ComponentKind::Nmos,
        "PMOS" | "PMOSFET"        => ComponentKind::Pmos,
        _                         => ComponentKind::Passive, // conservative fallback
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
        let exact    = self.resolutions.iter().filter(|r| r.confidence == Confidence::Exact).count();
        let family   = self.resolutions.iter().filter(|r| r.confidence == Confidence::Family).count();
        let guessed  = self.resolutions.iter().filter(|r| r.confidence == Confidence::Guessed).count();
        let unresolved = self.resolutions.iter().filter(|r| r.confidence == Confidence::Unresolved).count();
        (exact, family, guessed, unresolved)
    }

    /// All unresolved queries.
    pub fn unresolved(&self) -> Vec<&Resolution> {
        self.resolutions.iter().filter(|r| r.confidence == Confidence::Unresolved).collect()
    }
}

impl std::fmt::Display for ResolutionReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (exact, family, guessed, unresolved) = self.counts();
        writeln!(f, "┌──────────────────────────────────────────────────────────────┐")?;
        writeln!(f, "│ Resolution report: {} components", self.resolutions.len())?;
        writeln!(f, "│  exact={exact}  family={family}  guessed={guessed}  unresolved={unresolved}")?;
        writeln!(f, "├──────────┬──────────────────────────┬────────────┬────────────┤")?;
        writeln!(f, "│ Ref      │ Value                    │ Model ID   │ Confidence │")?;
        writeln!(f, "├──────────┼──────────────────────────┼────────────┼────────────┤")?;
        for res in &self.resolutions {
            let ref_s = res.query.reference.as_deref().unwrap_or("?");
            let val_s = res.query.value.as_deref().unwrap_or("");
            let (model_id, conf) = match &res.model {
                Some(m) => (m.id.as_str(), res.confidence.to_string()),
                None    => ("UNRESOLVED", "unresolved".to_string()),
            };
            writeln!(
                f,
                "│ {:<8} │ {:<24} │ {:<10} │ {:<10} │",
                &ref_s[..ref_s.len().min(8)],
                &val_s[..val_s.len().min(24)],
                &model_id[..model_id.len().min(10)],
                &conf[..conf.len().min(10)],
            )?;
        }
        writeln!(f, "└──────────┴──────────────────────────┴────────────┴────────────┘")?;
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
    fn resolve_resistor() {
        let l = lib();
        let q = ComponentQuery::new(
            Some("Device:R".to_string()),
            Some("10k".to_string()),
            None,
        );
        let res = l.resolve(&q);
        // Exact lib_id match with no value_re in the rule resolves at Family
        // confidence (the entry covers the whole Device:R class, not a specific part).
        assert!(
            res.confidence <= Confidence::Family,
            "expected Family or better, got {:?}", res.confidence
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
        let q = ComponentQuery { value: Some("1N4148".to_string()), ..Default::default() };
        let res = l.resolve(&q);
        assert!(res.model.is_some());
        assert_eq!(res.model.unwrap().kind, ComponentKind::Diode);
    }

    #[test]
    fn resolve_74hc595() {
        let l = lib();
        let q = ComponentQuery { value: Some("74HC595".to_string()), ..Default::default() };
        let res = l.resolve(&q);
        assert!(res.model.is_some());
        assert_eq!(res.model.unwrap().kind, ComponentKind::ShiftRegister);
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
        let q = ComponentQuery { value: Some("BC847".to_string()), ..Default::default() };
        let res = l.resolve(&q);
        assert_eq!(res.source.as_deref(), Some("spice"));
    }

    #[test]
    fn report_display() {
        let l = lib();
        let queries = vec![
            ComponentQuery { reference: Some("R1".to_string()), value: Some("10k".to_string()), footprint: Some("Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm_P10.16mm_Horizontal".to_string()), ..Default::default() },
            ComponentQuery { reference: Some("D1".to_string()), value: Some("1N4148".to_string()), ..Default::default() },
            ComponentQuery { reference: Some("U99".to_string()), value: Some("UNKNOWN".to_string()), ..Default::default() },
        ];
        let report = l.report(&queries);
        let s = report.to_string();
        assert!(s.contains("R1"));
        assert!(s.contains("1N4148"));
        assert!(s.contains("UNRESOLVED"));
    }

    /// Test resolution against components actually found in the pic_programmer board.
    #[test]
    fn pic_programmer_bom() {
        let l = lib();
        // Components extracted from the pic_programmer.kicad_pcb
        let bom: &[(&str, &str, &str)] = &[
            ("C1",   "100µF",    "Capacitor_THT:CP_Axial_L18.0mm_D6.5mm_P25.00mm_Horizontal"),
            ("C2",   "220uF",    "Capacitor_THT:CP_Axial_L18.0mm_D6.5mm_P25.00mm_Horizontal"),
            ("R1",   "10K",      "Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm_P10.16mm_Horizontal"),
            ("R10",  "5,1K",     "Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm_P10.16mm_Horizontal"),
            ("D2",   "BAT43",    "Diode_THT:D_DO-35_SOD27_P7.62mm_Horizontal"),
            ("D1",   "1N4004",   "Diode_THT:D_DO-35_SOD27_P12.70mm_Horizontal"),
            ("D8",   "RED-LED",  "LED_THT:LED_D5.0mm"),
            ("D9",   "GREEN-LED","LED_THT:LED_D5.0mm"),
            ("Q1",   "BC237",    "footprints:TO-92"),
            ("Q3",   "BC307",    "footprints:TO-92"),
            ("L1",   "22uH",     "Inductor_THT:L_Radial_D7.8mm_P5.00mm_Fastron_07HCP"),
            ("U3",   "7805",     "Package_TO_SOT_THT:TO-220-3_Horizontal_TabDown"),
            ("P101", "CONN_1",   "MountingHole:MountingHole_4.3mm_M4"),
            ("C5",   "10nF",     "Capacitor_THT:C_Disc_D5.1mm_W3.2mm_P5.00mm"),
            ("U2",   "74HC125",  "Package_DIP:DIP-14_W7.62mm_LongPads"),
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
        let must_resolve = ["C1", "C2", "R1", "D2", "D1", "D8", "D9", "Q1", "L1", "U3", "P101", "C5", "U2"];
        for res in &report.resolutions {
            let r = res.query.reference.as_deref().unwrap_or("");
            if must_resolve.contains(&r) {
                assert!(
                    res.model.is_some(),
                    "component {} (value={:?}) should have resolved but didn't",
                    r, res.query.value
                );
            }
        }

        // Unresolved count should be 0 for the list above
        let unresolved_list = report.unresolved();
        let unresolved_refs: Vec<_> = unresolved_list
            .iter()
            .filter(|r| {
                must_resolve.contains(&r.query.reference.as_deref().unwrap_or(""))
            })
            .collect();
        assert!(
            unresolved_refs.is_empty(),
            "expected no unresolved in must_resolve set, got: {:?}",
            unresolved_refs.iter().map(|r| r.query.reference.as_deref()).collect::<Vec<_>>()
        );
    }
}
