//! Bills of material: the artifact that says what a part actually IS.
//!
//! A layout gives a footprint and a value string. Neither names a part. A
//! footprint is a pad pattern shared by thousands of devices, and a value
//! string is a human label whose meaning is convention: `10k` is a resistance,
//! `AO3400A` is a part number, `DNP` is an instruction, and `~` is KiCad's way
//! of writing nothing. So a board whose layout says `10k` binds, and a board
//! whose layout says nothing useful binds unresolved even when the BOM sitting
//! beside it names the exact manufacturer part number. This module reads that
//! BOM.
//!
//! Reading it is a column-mapping problem rather than a parsing problem. There
//! is no BOM format. There are the shapes each CAD tool exports, the shapes the
//! assembly houses accept, the shapes the distributors' cart pages produce, and
//! the spreadsheet somebody maintains by hand. [`BomDialect`] enumerates the
//! nine that a survey of 664 real BOM and placement files from public projects
//! found; `docs/ingest/BOM.md` records the survey and the frequency of each.
//!
//! The design centre is that a wrong answer is worse than no answer. A
//! mis-bound part makes the report confidently wrong about a device that is not
//! on the board, and every number downstream inherits that. So the mapping is
//! detected with an explicit confidence tier per column ([`MappingConfidence`]),
//! a run that cannot reach a confident mapping REFUSES rather than guessing, and
//! the mapping it did use is recorded in [`BomProvenance`] so a reader can check
//! it. Erroring is fine here. Silently mis-binding is not.
//!
//! Long-form: `docs/ingest/BOM.md`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// How many lines from the top a delimited BOM is allowed to spend on banner
/// text before its header row. KiCad's grouped export spends six (source, date,
/// tool, generator, component count, a blank); Altium's spends up to thirteen.
/// Forty is well past every real file the survey saw and still bounds the work
/// on a file that is not a BOM at all.
const MAX_BANNER_LINES: usize = 40;

/// The exit code a refused BOM produces: "invalid for analysis", the same code
/// a diverged co-simulation produces. A BOM hauksbee cannot map is not a failed
/// assertion (exit 1) and not a usage error (exit 2): it is an input that cannot
/// be analysed truthfully, which is exactly what 3 means. The definition lives
/// in `hauksbee_engine::result::EXIT_INVALID_FOR_ANALYSIS`; `hauksbee-engine`'s
/// `bom_identity` test asserts the two agree, because this crate is below the
/// engine and cannot import it.
pub const EXIT_INVALID_FOR_ANALYSIS: i32 = 3;

// ── Errors ──────────────────────────────────────────────────────────────────

/// Why a BOM could not be used. Every variant is already a whole human
/// sentence naming the file, what is wrong, and what to do about it, because a
/// refusal whose message does not say what to do next is a dead end.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum BomError {
    /// The file is empty or is only whitespace.
    #[error(
        "{name} is empty, so there is no BOM to read. If the export produced a \
         zero-byte file, re-run it; if this is a Git LFS pointer, fetch the real \
         file with `git lfs pull`"
    )]
    Empty { name: String },

    /// Nothing in the first [`MAX_BANNER_LINES`] lines looks like a BOM header.
    #[error(
        "{name} does not read as a bill of materials: no row in its first {looked} \
         lines has a reference-designator column beside a value, part-number, \
         footprint or quantity column. hauksbee reads KiCad, Altium, Eagle, \
         LCSC/JLCPCB, Digi-Key and hand-maintained spreadsheet BOMs, comma-, \
         semicolon- or tab-separated. If this really is a BOM, name its reference \
         column explicitly with `--bom-column reference={example}`"
    )]
    NotABom {
        name: String,
        looked: usize,
        example: String,
    },

    /// A reference column exists but only as a guess, so using it would risk
    /// attaching every identity in the file to the wrong parts.
    #[error(
        "{name} has no column hauksbee is confident is the reference designator. \
         The closest is {candidate:?}, which is a guess, and a guess here attaches \
         every part number in the file to the wrong part. Confirm it with \
         `--bom-column reference={candidate}`, or name the right column the same way. \
         The columns in the file are: {headers}"
    )]
    UnconfidentReferenceColumn {
        name: String,
        candidate: String,
        headers: String,
    },

    /// Two columns are equally good candidates for one role.
    #[error(
        "{name} has two columns that could be the {role}: {headers}. hauksbee will \
         not pick one for you, because picking wrong makes the report confidently \
         wrong. Say which with `--bom-column {role}=<column>`"
    )]
    AmbiguousColumn {
        name: String,
        role: String,
        headers: String,
    },

    /// The reference column is mapped but empty on every row, so nothing in the
    /// file can be attached to a part. Distributor cart exports do this.
    #[error(
        "{name} has a {column:?} column but it is empty on all {rows} rows, so \
         nothing in this file can be attached to a part on the board. A distributor \
         cart export (Digi-Key, Mouser) usually looks like this: it is a purchase \
         list, not a BOM. Re-export it with the reference-designator field filled \
         in, or point hauksbee at the BOM your CAD tool wrote"
    )]
    EmptyReferenceColumn {
        name: String,
        column: String,
        rows: usize,
    },

    /// The file parsed as text but not as a table.
    #[error("{name} is not a readable table: {detail}. Re-export it and retry")]
    Unreadable { name: String, detail: String },

    /// A caller override names a column the file does not have.
    #[error(
        "{name} has no column called {wanted:?}, so `--bom-column {role}={wanted}` \
         cannot be honoured. Its columns are: {headers}"
    )]
    UnknownColumnOverride {
        name: String,
        role: String,
        wanted: String,
        headers: String,
    },

    /// Reading the file from disk failed.
    #[error("cannot read {name}: {detail}")]
    Io { name: String, detail: String },
}

impl BomError {
    /// The process exit code this refusal maps to. Always
    /// [`EXIT_INVALID_FOR_ANALYSIS`]: every variant here means the input cannot
    /// be analysed, not that an assertion failed.
    pub fn exit_code(&self) -> i32 {
        EXIT_INVALID_FOR_ANALYSIS
    }
}

// ── Dialects ────────────────────────────────────────────────────────────────

/// The BOM shapes that actually exist in the wild.
///
/// The frequencies in each doc comment are counts from the survey recorded in
/// `docs/ingest/BOM.md`: 664 real BOM and placement files gathered from public
/// hardware projects. They are here so that a future reader can tell which
/// branches carry the traffic and which are long-tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BomDialect {
    /// KiCad's `bom_csv_grouped_by_value` family: an `Item, Qty,
    /// Reference(s), Value, LibPart, Footprint, Datasheet` header under a
    /// five-line preamble naming the source schematic and the generator script.
    /// One row per value group, reference designators comma-separated in one
    /// cell. 35 files.
    KicadGrouped,
    /// KiCad's ungrouped / `Component, Description, Part, References, Value,
    /// Footprint, Quantity Per PCB` shape, including the KiBom and the
    /// `bom2grouped_csv` variants. Reference designators here are often
    /// SPACE-separated rather than comma-separated. 27 files.
    KicadUngrouped,
    /// Altium's BOM export: `Comment, Description, Designator, Footprint,
    /// LibRef, Quantity`, where `Comment` is the value column and `LibRef` is
    /// the schematic symbol. 35 files.
    Altium,
    /// Eagle's `partlist` ULP output: fixed-width columns under a `Partlist` /
    /// `Exported from ... EAGLE Version ...` banner, one row per part, columns
    /// `Part Value Device Package Library Sheet`. The banner is optional; real
    /// exports start straight at the header, or carry an arbitrary title line.
    /// 61 files.
    EaglePartlist,
    /// Eagle's grouped partlist: `Qty Value Device Package Parts Description`,
    /// also fixed-width, with the reference designators in the `Parts` column.
    EagleGroupedPartlist,
    /// The LCSC / EasyEDA assembly BOM: `Comment, Designator, Footprint, LCSC
    /// Part #`. The LCSC code is a DISTRIBUTOR code, not a manufacturer part
    /// number, and is never used for identity; see [`ColumnRole`]. 71 files,
    /// the single most common BOM shape in the survey.
    Lcsc,
    /// JLCPCB's SMT assembly BOM: `Comment, Designator, Footprint, JLCPCB Part
    /// #`, sometimes with the header spelled `JLCPCB Part #（optional）` in
    /// full-width brackets. 39 files.
    Jlcpcb,
    /// A Digi-Key BOM-manager or cart export: `Manufacturer Part Number,
    /// Manufacturer, Digi-Key Part Number, Customer Reference, ...`. Real
    /// identity, frequently no usable reference designators, which is a
    /// refusal rather than a guess. 32 files.
    DigiKey,
    /// A hand-maintained spreadsheet export mapped entirely from its column
    /// names. Anything from `Designator, Value, Qty, Package, ..., MPN, DNP` to
    /// a four-column sheet somebody typed. 41 files.
    Spreadsheet,
}

impl BomDialect {
    /// The short machine name recorded in provenance, and the name a report
    /// prints. Stable: it goes in JSON.
    pub fn kind(self) -> &'static str {
        match self {
            BomDialect::KicadGrouped => "kicad_grouped_bom",
            BomDialect::KicadUngrouped => "kicad_ungrouped_bom",
            BomDialect::Altium => "altium_bom",
            BomDialect::EaglePartlist => "eagle_partlist",
            BomDialect::EagleGroupedPartlist => "eagle_grouped_partlist",
            BomDialect::Lcsc => "lcsc_bom",
            BomDialect::Jlcpcb => "jlcpcb_bom",
            BomDialect::DigiKey => "digikey_bom",
            BomDialect::Spreadsheet => "spreadsheet_bom",
        }
    }

    /// The human name, for a report line a person reads.
    pub fn describe(self) -> &'static str {
        match self {
            BomDialect::KicadGrouped => "KiCad grouped BOM",
            BomDialect::KicadUngrouped => "KiCad ungrouped BOM",
            BomDialect::Altium => "Altium BOM",
            BomDialect::EaglePartlist => "Eagle partlist",
            BomDialect::EagleGroupedPartlist => "Eagle grouped partlist",
            BomDialect::Lcsc => "LCSC assembly BOM",
            BomDialect::Jlcpcb => "JLCPCB assembly BOM",
            BomDialect::DigiKey => "Digi-Key BOM export",
            BomDialect::Spreadsheet => "spreadsheet BOM",
        }
    }

    /// True when this dialect's `Comment` column is definitionally the value
    /// column. In Altium, LCSC and JLCPCB exports it is; in a spreadsheet
    /// somebody wrote, `Comment` is a comment.
    fn comment_is_value(self) -> bool {
        matches!(
            self,
            BomDialect::Altium | BomDialect::Lcsc | BomDialect::Jlcpcb
        )
    }

    /// True for the two fixed-width Eagle shapes, which are sliced by header
    /// column offsets rather than split on a delimiter.
    fn is_fixed_width(self) -> bool {
        matches!(
            self,
            BomDialect::EaglePartlist | BomDialect::EagleGroupedPartlist
        )
    }
}

// ── Column roles and confidence ─────────────────────────────────────────────

/// What a BOM column is for. Only these roles are read; every other column is
/// recorded as ignored so the input inventory can say what was left on the
/// floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnRole {
    /// The reference designators this row covers. The join key: without it a
    /// BOM cannot be attached to a board at all.
    Reference,
    /// The value string, the same kind of claim the layout's value field makes.
    Value,
    /// The manufacturer part number. The only column that carries identity the
    /// layout cannot: a globally unique key naming exactly one device.
    Mpn,
    /// The manufacturer name. Recorded, not used for matching.
    Manufacturer,
    /// How many parts this row covers, for the internal-consistency check
    /// against the row's own reference list.
    Quantity,
    /// The footprint or package.
    Footprint,
    /// The do-not-populate / populate flag.
    Populate,
    /// A distributor's own order code (LCSC `C1525`, Digi-Key `311-15LRCT-ND`,
    /// Mouser `621-BCM857BS-7-F`). Recorded so a reader can trace the row back
    /// to what was ordered, and NEVER used for identity: a distributor code
    /// carries a distributor prefix, is not a manufacturer part number, and
    /// matching a model regex against one is how a part binds to the wrong
    /// device.
    DistributorPart,
}

impl ColumnRole {
    /// The name used in `--bom-column <role>=<column>` and in JSON.
    pub fn flag_name(self) -> &'static str {
        match self {
            ColumnRole::Reference => "reference",
            ColumnRole::Value => "value",
            ColumnRole::Mpn => "mpn",
            ColumnRole::Manufacturer => "manufacturer",
            ColumnRole::Quantity => "quantity",
            ColumnRole::Footprint => "footprint",
            ColumnRole::Populate => "populate",
            ColumnRole::DistributorPart => "distributor_part",
        }
    }

    /// Parse the role name a caller typed. `None` for anything unrecognised,
    /// so the caller can list the accepted names.
    pub fn from_flag_name(s: &str) -> Option<Self> {
        Some(
            match s
                .trim()
                .to_ascii_lowercase()
                .replace(['-', ' '], "_")
                .as_str()
            {
                "reference" | "refdes" | "designator" => ColumnRole::Reference,
                "value" => ColumnRole::Value,
                "mpn" => ColumnRole::Mpn,
                "manufacturer" => ColumnRole::Manufacturer,
                "quantity" | "qty" => ColumnRole::Quantity,
                "footprint" => ColumnRole::Footprint,
                "populate" | "dnp" => ColumnRole::Populate,
                "distributor_part" => ColumnRole::DistributorPart,
                _ => return None,
            },
        )
    }

    /// Every role name, for an error message that has to list them.
    pub fn all() -> &'static [ColumnRole] {
        &[
            ColumnRole::Reference,
            ColumnRole::Value,
            ColumnRole::Mpn,
            ColumnRole::Manufacturer,
            ColumnRole::Quantity,
            ColumnRole::Footprint,
            ColumnRole::Populate,
            ColumnRole::DistributorPart,
        ]
    }
}

/// How sure the column detection is about one assignment.
///
/// The tier decides what a non-interactive run may do, and it is the whole
/// mechanism by which "autodetect, then ask rather than guess" becomes
/// something a CI job can execute without a human present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MappingConfidence {
    /// The header is an unambiguous name for this role, or the dialect defines
    /// it. `MPN`, `Designator`, `Reference(s)`, `Quantity`, `DNP`. A header
    /// literally called `MPN` is not a guess.
    Certain,
    /// The header means this role in a recognised dialect, or is a widely used
    /// abbreviation, but is not self-explanatory in isolation: `Val`, `Ref`,
    /// `Package`, `Quantity Per PCB`, and `Comment` inside an Altium, LCSC or
    /// JLCPCB export.
    Likely,
    /// The header only plausibly means this role: `Part`, `Component`,
    /// `Description`, `Customer Reference`. Never used without confirmation.
    /// A guessed column is left unmapped and named in the report, because the
    /// status quo (the layout's own value) is a better answer than a wrong one.
    Guess,
}

impl MappingConfidence {
    /// True when a non-interactive run may act on this assignment unasked.
    pub fn usable_unattended(self) -> bool {
        matches!(self, MappingConfidence::Certain | MappingConfidence::Likely)
    }

    pub fn describe(self) -> &'static str {
        match self {
            MappingConfidence::Certain => "certain",
            MappingConfidence::Likely => "likely",
            MappingConfidence::Guess => "guess",
        }
    }
}

/// One column, mapped to one role, with the evidence for the mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnAssignment {
    pub role: ColumnRole,
    /// Zero-based column index in the header row.
    pub index: usize,
    /// The header exactly as spelled in the file, so a report can echo it.
    pub header: String,
    pub confidence: MappingConfidence,
    /// True when the header alone was not enough and the column's own CONTENT
    /// settled it, which is how an ambiguously named reference column becomes
    /// usable without asking. Recorded rather than hidden: a reader checking the
    /// mapping needs to know which assignments rest on the header and which rest
    /// on the data.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirmed_by_content: bool,
}

/// The mapping a BOM was read with: which column filled which role, at what
/// confidence, plus the columns that were deliberately not used.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnMap {
    pub used: Vec<ColumnAssignment>,
    /// Guess-tier candidates that were left unmapped. Each carries the flag
    /// that would map it, so the report can say how to resolve it.
    pub left_unmapped: Vec<ColumnAssignment>,
    /// Headers no role claimed at all.
    pub ignored_headers: Vec<String>,
}

impl ColumnMap {
    fn index_of(&self, role: ColumnRole) -> Option<usize> {
        self.used.iter().find(|a| a.role == role).map(|a| a.index)
    }

    fn header_of(&self, role: ColumnRole) -> Option<&str> {
        self.used
            .iter()
            .find(|a| a.role == role)
            .map(|a| a.header.as_str())
    }

    /// The lines a report prints so the mapping actually used is never
    /// invisible. Recording the mapping is half the contract: a run that
    /// proceeds on a detected mapping must say which one it used.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for a in &self.used {
            let how = if a.confirmed_by_content {
                format!(
                    "{}, confirmed by the column's content",
                    a.confidence.describe()
                )
            } else {
                a.confidence.describe().to_string()
            };
            out.push(format!(
                "  {} <- {:?} ({how})",
                a.role.flag_name(),
                a.header
            ));
        }
        for a in &self.left_unmapped {
            out.push(format!(
                "  {} not mapped: {:?} is only a guess. Confirm it with `--bom-column {}={}`",
                a.role.flag_name(),
                a.header,
                a.role.flag_name(),
                a.header
            ));
        }
        out
    }
}

/// Caller-supplied column mappings, from `--bom-column <role>=<column>` or a
/// spec table. An override is [`MappingConfidence::Certain`] by construction:
/// the human said so.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColumnOverrides {
    pairs: Vec<(ColumnRole, String)>,
}

impl ColumnOverrides {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one override. Later overrides for the same role replace earlier ones.
    pub fn set(&mut self, role: ColumnRole, header: impl Into<String>) -> &mut Self {
        let header = header.into();
        self.pairs.retain(|(r, _)| *r != role);
        self.pairs.push((role, header));
        self
    }

    /// Parse a `role=column` pair as typed on a command line. The error is the
    /// message to show, already naming the accepted role names.
    pub fn parse_pair(s: &str) -> Result<(ColumnRole, String), String> {
        let (role, header) = s.split_once('=').ok_or_else(|| {
            format!(
                "`{s}` is not a column mapping. Write it as `<role>=<column header>`, \
                 for example `reference=Designator`"
            )
        })?;
        let parsed = ColumnRole::from_flag_name(role).ok_or_else(|| {
            let names = ColumnRole::all()
                .iter()
                .map(|r| r.flag_name())
                .collect::<Vec<_>>()
                .join(", ");
            format!("`{role}` is not a BOM column role. The roles are: {names}")
        })?;
        if header.trim().is_empty() {
            return Err(format!(
                "`{s}` names the {} role but no column. Write it as \
                 `{}=<column header>`",
                parsed.flag_name(),
                parsed.flag_name()
            ));
        }
        Ok((parsed, header.trim().to_string()))
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    fn get(&self, role: ColumnRole) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(r, _)| *r == role)
            .map(|(_, h)| h.as_str())
    }
}

// ── Provenance ──────────────────────────────────────────────────────────────

/// One thing an artifact contributed to the analysis.
///
/// Deliberately the smallest shape that answers "which file identified this
/// part". The names and fields mirror `docs/dev-plans/evidence-spine.md` §2.2
/// exactly (`Contribution { what, detail }`,
/// `IgnoredInput { what, why }`, `ArtifactProvenance { path, kind, sha256,
/// contributed, ignored, .. }`) so that when `hauksbee-ir`'s `evidence` module
/// lands, this is absorbed rather than becoming a second vocabulary for the
/// same idea.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contribution {
    pub what: String,
    pub detail: String,
}

/// One thing in an artifact that was read and deliberately not used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IgnoredInput {
    pub what: String,
    pub why: String,
}

/// What a BOM read contributed, and how it was read.
///
/// `role` is `ArtifactRole::Bom` in the evidence spine's vocabulary; it is not
/// carried here because the enum lives in `hauksbee-ir` and this crate must not
/// pre-empt its definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BomProvenance {
    /// The path as the caller gave it, or the display name for text input.
    pub path: String,
    /// [`BomDialect::kind`], the stable machine name.
    pub kind: String,
    /// Hex SHA-256 of the exact bytes read. A BOM is the artifact most likely
    /// to be edited between runs, so pinning it is the difference between a
    /// reproducible identity claim and a guess about which revision was used.
    pub sha256: String,
    /// The mapping the read actually used.
    pub column_map: ColumnMap,
    /// Rows that produced at least one reference designator.
    pub rows: usize,
    pub contributed: Vec<Contribution>,
    pub ignored: Vec<IgnoredInput>,
}

impl BomProvenance {
    /// The block a report prints so that what was read, how it was mapped, and
    /// what was dropped are all visible without opening the file.
    ///
    /// Half the contract of proceeding on a detected mapping is saying which
    /// mapping was used, so this is not optional output.
    pub fn lines(&self) -> Vec<String> {
        let sha = if self.sha256.is_empty() {
            String::new()
        } else {
            format!(", sha256 {}", &self.sha256[..8])
        };
        let mut out = vec![format!("{} ({}{sha})", self.path, self.kind)];
        out.extend(self.column_map.lines());
        for c in &self.contributed {
            out.push(format!("  contributed: {}: {}", c.what, c.detail));
        }
        for i in &self.ignored {
            out.push(format!("  ignored:     {}: {}", i.what, i.why));
        }
        out
    }
}

// ── The BOM ─────────────────────────────────────────────────────────────────

/// One BOM row, after mapping. A grouped row covering twelve capacitors is one
/// `BomRow` with twelve references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BomRow {
    /// Reference designators, in file order, duplicates removed.
    pub references: Vec<String>,
    /// The value column, trimmed. Empty when absent.
    pub value: String,
    /// The manufacturer part number, when the file carries one.
    pub mpn: Option<String>,
    pub manufacturer: Option<String>,
    pub footprint: Option<String>,
    /// The stated quantity, when the column exists and parses.
    pub quantity: Option<usize>,
    /// `Some(true)` populate, `Some(false)` do not populate, `None` when the
    /// file says nothing either way.
    pub populate: Option<bool>,
    pub distributor_part: Option<String>,
    /// One-based line number in the source file, for a message that has to
    /// point at a row.
    pub line: usize,
}

/// A read bill of materials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bom {
    pub dialect: BomDialect,
    pub rows: Vec<BomRow>,
    pub provenance: BomProvenance,
}

impl Bom {
    /// Read a BOM from a path. The dialect is detected from the content, never
    /// the extension: the tab-separated exports that claim to be `.csv` are
    /// common enough that trusting the extension is a bug.
    pub fn read(path: &Path) -> Result<Self, BomError> {
        let name = path.display().to_string();
        let bytes = std::fs::read(path).map_err(|e| BomError::Io {
            name: name.clone(),
            detail: e.to_string(),
        })?;
        Self::from_bytes(&bytes, &name, &ColumnOverrides::new())
    }

    /// Read a BOM from a path with caller-confirmed column mappings.
    pub fn read_with(path: &Path, overrides: &ColumnOverrides) -> Result<Self, BomError> {
        let name = path.display().to_string();
        let bytes = std::fs::read(path).map_err(|e| BomError::Io {
            name: name.clone(),
            detail: e.to_string(),
        })?;
        Self::from_bytes(&bytes, &name, overrides)
    }

    /// Read a BOM from bytes, hashing them for provenance. `name` is what error
    /// messages call the file.
    pub fn from_bytes(
        bytes: &[u8],
        name: &str,
        overrides: &ColumnOverrides,
    ) -> Result<Self, BomError> {
        let text = decode(bytes);
        let sha = sha256_hex(bytes);
        Self::parse(&text, name, &sha, overrides)
    }

    /// Read a BOM from text already in memory, with no hash. Used by tests and
    /// by callers that synthesized the table.
    pub fn from_text(
        text: &str,
        name: &str,
        overrides: &ColumnOverrides,
    ) -> Result<Self, BomError> {
        Self::parse(text, name, "", overrides)
    }

    /// Does this file look like a BOM at all? Cheap enough to run over every
    /// file beside a board, so a caller can find the BOM without being told.
    pub fn detects(bytes: &[u8]) -> bool {
        let text = decode(bytes);
        // The FULL read, not just the table search. A cheaper check that says yes
        // where `read` says exit 3 would send a caller that autodetects the BOM
        // beside a board straight into a refusal it did not ask for.
        Self::parse(&text, "", "", &ColumnOverrides::new()).is_ok()
    }

    /// Every reference designator the BOM mentions, sorted and deduplicated.
    pub fn references(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .rows
            .iter()
            .flat_map(|r| r.references.iter().cloned())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// The row covering one reference designator, if any.
    pub fn row_for(&self, reference: &str) -> Option<&BomRow> {
        self.rows
            .iter()
            .find(|r| r.references.iter().any(|x| x == reference))
    }

    /// Rows whose stated quantity disagrees with the number of references the
    /// same row enumerates, as `(row, stated, enumerated)`.
    ///
    /// This is the BOM disagreeing with ITSELF, which is the only quantity
    /// check worth making: the reference list is an enumerated fact and the
    /// quantity is a number somebody derived from it, so where they differ the
    /// list wins and the number is reported. A quantity that disagrees with the
    /// number of placements on the board is not a separate case: it is a
    /// reference designator missing from one side or the other, which
    /// [`crate::bom`]'s reconciliation already reports by name.
    pub fn quantity_disagreements(&self) -> Vec<(&BomRow, usize, usize)> {
        self.rows
            .iter()
            .filter_map(|r| {
                let stated = r.quantity?;
                let enumerated = r.references.len();
                (stated != enumerated && enumerated > 0).then_some((r, stated, enumerated))
            })
            .collect()
    }

    /// Identity hints for the binder, one per reference designator the BOM
    /// names. See [`IdentityHint`].
    pub fn identity_hints(&self) -> Vec<IdentityHint> {
        let mut out = Vec::new();
        for row in &self.rows {
            for r in &row.references {
                out.push(IdentityHint {
                    reference: r.clone(),
                    value: (!row.value.is_empty()).then(|| row.value.clone()),
                    mpn: row.mpn.clone(),
                    populate: row.populate,
                    source: self.provenance.path.clone(),
                    source_kind: self.provenance.kind.to_string(),
                });
            }
        }
        out
    }

    fn parse(
        text: &str,
        name: &str,
        sha: &str,
        overrides: &ColumnOverrides,
    ) -> Result<Self, BomError> {
        if text.trim().is_empty() {
            return Err(BomError::Empty {
                name: name.to_string(),
            });
        }
        let table = read_table(text, name)?;
        let dialect = table.dialect;
        let map = map_columns(&table, dialect, overrides, name)?;
        let ref_index = map.index_of(ColumnRole::Reference).expect("mapped above");
        let ref_header = map
            .header_of(ColumnRole::Reference)
            .unwrap_or_default()
            .to_string();
        let populate_key = map
            .header_of(ColumnRole::Populate)
            .map(normalise_header)
            .unwrap_or_default();

        let mut rows = Vec::new();
        let mut skipped_placeholders: Vec<String> = Vec::new();
        let mut rows_seen = 0usize;
        for row in &table.rows {
            let cell = |role: ColumnRole| -> Option<String> {
                let i = map.index_of(role)?;
                let v = row.cells.get(i)?.trim();
                (!v.is_empty() && v != "~" && v != "-").then(|| v.to_string())
            };
            let raw_refs = row.cells.get(ref_index).map(String::as_str).unwrap_or("");
            if raw_refs.trim().is_empty() {
                continue;
            }
            rows_seen += 1;
            let (references, placeholders) = split_references(raw_refs);
            skipped_placeholders.extend(placeholders);
            if references.is_empty() {
                continue;
            }
            let value = cell(ColumnRole::Value).unwrap_or_default();
            rows.push(BomRow {
                references,
                mpn: cell(ColumnRole::Mpn).filter(|m| looks_like_mpn(m)),
                manufacturer: cell(ColumnRole::Manufacturer),
                footprint: cell(ColumnRole::Footprint),
                quantity: cell(ColumnRole::Quantity).and_then(|q| parse_quantity(&q)),
                populate: cell(ColumnRole::Populate)
                    .and_then(|c| parse_populate(&c, &populate_key))
                    .or_else(|| populate_from_value(&value)),
                distributor_part: cell(ColumnRole::DistributorPart),
                value,
                line: row.line,
            });
        }

        if rows.is_empty() {
            return Err(BomError::EmptyReferenceColumn {
                name: name.to_string(),
                column: ref_header,
                rows: table.rows.len().max(rows_seen),
            });
        }

        let with_mpn = rows.iter().filter(|r| r.mpn.is_some()).count();
        let parts: usize = rows.iter().map(|r| r.references.len()).sum();
        let mut contributed = vec![Contribution {
            what: "part identity".to_string(),
            detail: format!(
                "{parts} reference designators over {} {}, {with_mpn} of {} carrying a \
                 manufacturer part number",
                rows.len(),
                plural(rows.len(), "row", "rows"),
                plural(rows.len(), "it", "them"),
            ),
        }];
        let stated = rows.iter().filter(|r| r.populate.is_some()).count();
        if stated > 0 {
            contributed.push(Contribution {
                what: "populate flags".to_string(),
                detail: format!(
                    "{stated} {} whether the part is fitted",
                    plural(stated, "row states", "rows state")
                ),
            });
        }

        let mut ignored: Vec<IgnoredInput> = map
            .ignored_headers
            .iter()
            .map(|h| IgnoredInput {
                what: format!("column {h:?}"),
                why: "no analysis reads it".to_string(),
            })
            .collect();
        for a in &map.left_unmapped {
            ignored.push(IgnoredInput {
                what: format!("column {:?}", a.header),
                why: format!(
                    "it is only a guess at the {} column; confirm it with \
                     `--bom-column {}={}`",
                    a.role.flag_name(),
                    a.role.flag_name(),
                    a.header
                ),
            });
        }
        if rows.iter().any(|r| r.distributor_part.is_some()) {
            ignored.push(IgnoredInput {
                what: "distributor order codes".to_string(),
                why: "a distributor code is not a manufacturer part number, so matching a \
                      model against one would bind the wrong device"
                    .to_string(),
            });
        }
        skipped_placeholders.sort();
        skipped_placeholders.dedup();
        if !skipped_placeholders.is_empty() {
            ignored.push(IgnoredInput {
                what: format!("{} unannotated designators", skipped_placeholders.len()),
                why: format!(
                    "{} name no specific part, so they cannot be matched to the board",
                    skipped_placeholders.join(", ")
                ),
            });
        }

        Ok(Bom {
            dialect,
            rows,
            provenance: BomProvenance {
                path: name.to_string(),
                kind: dialect.kind().to_string(),
                sha256: sha.to_string(),
                column_map: map,
                rows: rows_seen,
                contributed,
                ignored,
            },
        })
    }
}

/// One artifact's claim about what one reference designator is.
///
/// The binder consumes these; see `hauksbee_engine::binder::apply_identity` for
/// the precedence between a hint and the layout's own value, and for what
/// happens when the two contradict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityHint {
    pub reference: String,
    /// The artifact's value string, when it has one.
    pub value: Option<String>,
    /// The manufacturer part number, the only field that carries identity the
    /// layout cannot supply.
    pub mpn: Option<String>,
    /// What the artifact says about fitting this part.
    pub populate: Option<bool>,
    /// The artifact this came from, for attribution.
    pub source: String,
    /// The artifact's kind, e.g. `lcsc_bom` or `kicad_pos`.
    pub source_kind: String,
}

// ── Reserved property keys ──────────────────────────────────────────────────

/// Property key under which a BOM-supplied manufacturer part number is
/// attached to a [`crate::Component`].
///
/// The property bag is already the channel a reader uses to hand the binder
/// out-of-band facts about a part (`value_unresolved` does the same job for
/// Altium), and the binder's `resolve` already looks for a manufacturer part
/// number there. Using a RESERVED key rather than relying on that heuristic
/// scan matters: the scan also matches a `Mouser Part Number` column, and a
/// distributor code with its distributor prefix does not match a model's
/// anchored part-number regex, so it silently costs the bind it was meant to
/// win.
pub const MPN_PROPERTY: &str = "hauksbee_bom_mpn";

/// Property key naming the artifact a [`MPN_PROPERTY`] came from, so a bind
/// that used BOM identity is attributable to the file that supplied it.
pub const IDENTITY_SOURCE_PROPERTY: &str = "hauksbee_identity_source";

/// Property key under which an artifact-supplied value string fills a
/// [`crate::Component`] whose layout value was empty.
pub const VALUE_PROPERTY: &str = "hauksbee_artifact_value";

// ── Tabular reading ─────────────────────────────────────────────────────────

/// A delimiter-separated or fixed-width row.
#[derive(Debug, Clone)]
pub(crate) struct TableRow {
    pub(crate) cells: Vec<String>,
    /// One-based source line.
    pub(crate) line: usize,
}

/// A table read out of a text file: the header, the rows, and how it was split.
#[derive(Debug, Clone)]
pub(crate) struct Table {
    pub(crate) headers: Vec<String>,
    pub(crate) rows: Vec<TableRow>,
    pub(crate) dialect: BomDialect,
}

/// Decode bytes to text, normalising the three things a spreadsheet round trip
/// does to a file.
///
/// A leading UTF-8 byte-order mark, which 68 of the 664 surveyed files carry and
/// which turns `Designator` into a header no rule recognises. A Windows code
/// page, which a degree sign or an ohm glyph arrives in. And a bare carriage
/// return as the line ending, which Excel for Mac still writes and which
/// `str::lines` reads as one enormous single line, so a real 47-row BOM reads as
/// a header and nothing else.
///
/// Shared with the placement reader, which needs all three for the same reasons.
pub(crate) fn decode(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        // Not UTF-8: treat it as Latin-1, which is what a Windows-1252 export
        // is for every character a BOM actually uses.
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    };
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Hex SHA-256, for provenance. Hand-rolled because this crate has no hash
/// dependency and one 40-line function is a smaller commitment than a new
/// entry in the workspace's dependency audit.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = bytes.to_vec();
    let bit_len = (bytes.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [
                t1.wrapping_add(t2),
                v[0],
                v[1],
                v[2],
                v[3].wrapping_add(t1),
                v[4],
                v[5],
                v[6],
            ];
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

/// Split one delimited line, honouring RFC 4180 double quoting (a doubled quote
/// inside a quoted field is one literal quote).
///
/// Hand-rolled rather than pulled in: the whole job is thirty lines, and the
/// alternative is a new workspace dependency for every consumer of this crate.
pub(crate) fn split_delimited(line: &str, delim: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            c if c == delim && !in_quotes => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out.iter().map(|s| s.trim().to_string()).collect()
}

/// The delimiter a line is most likely split on. Tab is checked before comma so
/// a tab-separated export that claims to be `.csv` reads correctly, which the
/// survey found in three of the files it gathered.
pub(crate) fn sniff_delimiter(line: &str) -> char {
    let counts = [
        ('\t', line.matches('\t').count()),
        (',', line.matches(',').count()),
        (';', line.matches(';').count()),
        ('|', line.matches('|').count()),
    ];
    // `max_by_key` returns the LAST maximum, which would silently make the last
    // candidate win a tie. Folding with a strict `>` keeps the documented order.
    counts
        .iter()
        .filter(|(_, n)| *n > 0)
        .fold(None::<(char, usize)>, |best, &(d, n)| match best {
            Some((_, bn)) if bn >= n => best,
            _ => Some((d, n)),
        })
        .map(|(d, _)| d)
        .unwrap_or(',')
}

/// Normalise a header for matching: drop the byte-order mark, the quotes, the
/// `(optional)` an assembly house's template adds (including the full-width
/// bracket form JLCPCB's own template uses), then fold everything that is not
/// alphanumeric into a single underscore.
///
/// `"Reference(s)"` becomes `reference_s`, `"Mid X"` becomes `mid_x`,
/// `"JLCPCB Part #（optional）"` becomes `jlcpcb_part`.
pub(crate) fn normalise_header(raw: &str) -> String {
    let mut s = raw.trim().trim_matches('"').to_ascii_lowercase();
    for token in ["(optional)", "（optional）", "?optional?", "(mpn)"] {
        s = s.replace(token, "");
    }
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

/// Read a text file as a table, finding the header row past any banner and
/// deciding the dialect.
fn read_table(text: &str, name: &str) -> Result<Table, BomError> {
    let lines: Vec<&str> = text.lines().collect();

    // Fixed-width Eagle first: its header is recognised by shape, and its
    // banner is optional, absent or an arbitrary title line, so a rule keyed on
    // the banner misses real files.
    if let Some((i, dialect)) = find_eagle_header(&lines) {
        return Ok(read_fixed_width(&lines, i, dialect));
    }

    let looked = lines.len().min(MAX_BANNER_LINES);
    for (i, line) in lines.iter().take(MAX_BANNER_LINES).enumerate() {
        if line.trim().is_empty() || line.trim_start().starts_with("//") {
            continue;
        }
        let delim = sniff_delimiter(line);
        let cells = split_delimited(line, delim);
        if cells.len() < 2 {
            continue;
        }
        let keys: Vec<String> = cells.iter().map(|c| normalise_header(c)).collect();
        let Some(dialect) = classify_headers(&keys) else {
            continue;
        };
        let headers: Vec<String> = cells
            .iter()
            .map(|c| c.trim().trim_matches('"').trim().to_string())
            .collect();
        let width = headers.len();
        let rows = lines
            .iter()
            .enumerate()
            .skip(i + 1)
            .filter(|(_, l)| !l.trim().is_empty() && !is_comment_line(l))
            .map(|(n, l)| {
                let mut cells = split_delimited(l, delim);
                cells.resize(width.max(cells.len()), String::new());
                TableRow { cells, line: n + 1 }
            })
            .collect();
        return Ok(Table {
            headers,
            rows,
            dialect,
        });
    }

    Err(BomError::NotABom {
        name: name.to_string(),
        looked,
        example: "Designator".to_string(),
    })
}

/// Locate an Eagle fixed-width partlist header, returning its line index and
/// which of the two Eagle shapes it is.
fn find_eagle_header(lines: &[&str]) -> Option<(usize, BomDialect)> {
    for (i, line) in lines.iter().take(MAX_BANNER_LINES).enumerate() {
        let keys: Vec<String> = line.split_whitespace().map(normalise_header).collect();
        let head: Vec<&str> = keys.iter().map(String::as_str).collect();
        match head.as_slice() {
            ["part", "value", "device", ..] => return Some((i, BomDialect::EaglePartlist)),
            ["qty", "value", "device", ..] if head.contains(&"parts") => {
                return Some((i, BomDialect::EagleGroupedPartlist))
            }
            _ => {}
        }
    }
    None
}

/// The column names in a fixed-width header row, and where each column starts.
///
/// Two things make this harder than splitting on whitespace. A value containing
/// a space (`"1uF 10V"`, `"Hole 2mm"`, `"Capacitor Tantalum SMD"`) is ordinary,
/// so the DATA cannot be tokenised. And the alignment is not uniform: Eagle and
/// Altium left-align every column, while KiCad's position file RIGHT-aligns its
/// three numeric columns, so a number begins several characters to the left of
/// its own header and slicing from the header's offset reads `2550` out of
/// `74.2550`.
///
/// So the boundaries come from the data rather than from the header: a column
/// boundary is a character position that is blank in the header AND in every
/// data row. That is exactly what makes the file readable by eye, it is
/// alignment-agnostic, and it needs no per-dialect special case. If the number
/// of regions it finds does not match the number of header names, two columns
/// touch somewhere and this cannot be trusted, so it falls back to the header's
/// own offsets with the first column forced to zero (anything left of the first
/// name is padding, and KiCad puts a `#` comment marker there).
///
/// `data` must be the real data rows only: a banner line is dense text that
/// would blank out every boundary.
pub(crate) fn fixed_width_columns(header: &str, data: &[&str]) -> (Vec<String>, Vec<usize>) {
    let chars: Vec<char> = header.chars().collect();
    let mut names: Vec<String> = Vec::new();
    let mut name_starts: Vec<usize> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        let name: String = chars[start..i].iter().collect();
        // A lone `#` is KiCad's comment marker, not a column.
        if name == "#" && names.is_empty() {
            continue;
        }
        names.push(name);
        name_starts.push(start);
    }
    if names.is_empty() {
        return (names, name_starts);
    }

    let occupied = |line: &str| -> Vec<bool> { line.chars().map(|c| !c.is_whitespace()).collect() };
    let mut ink = occupied(header);
    for row in data {
        for (p, filled) in occupied(row).into_iter().enumerate() {
            if p >= ink.len() {
                ink.push(filled);
            } else {
                ink[p] |= filled;
            }
        }
    }
    let mut spans: Vec<usize> = Vec::new();
    let mut prev = false;
    for (p, &filled) in ink.iter().enumerate() {
        if filled && !prev {
            spans.push(p);
        }
        prev = filled;
    }

    // Each header name takes the start of the ink region its own name sits in,
    // which is the leftmost character any row uses for that column and therefore
    // the correct boundary whichever way the column is aligned. Two names inside
    // one region means the region's columns touch on some row, so the second name
    // falls back to its own offset, which is the best available answer and is
    // right whenever that column is left-aligned.
    let mut starts = Vec::with_capacity(names.len());
    for &name_start in &name_starts {
        let region = spans
            .iter()
            .rev()
            .find(|&&s| s <= name_start)
            .copied()
            .unwrap_or(name_start);
        let taken = starts.last().is_some_and(|&prev| prev >= region);
        starts.push(if taken { name_start } else { region });
    }
    if let Some(first) = starts.first_mut() {
        *first = 0;
    }
    (names, starts)
}

/// One fixed-width data line sliced by the header's column offsets. A line
/// shorter than the header yields empty trailing cells, since both writers
/// right-trim a row whose last columns are blank.
pub(crate) fn slice_fixed_width(line: &str, starts: &[usize]) -> Vec<String> {
    let cs: Vec<char> = line.chars().collect();
    // A column boundary must never fall INSIDE a token. Where it does, it is
    // snapped left to the token's own start.
    //
    // This is what keeps a mis-estimated boundary from silently corrupting a
    // number instead of failing. A right-aligned `174.2550` under a `PosX` header
    // begins three characters left of the header, and a boundary landing between
    // the `1` and the `7` reads `4.2550` out of it: a coordinate off by 170 mm,
    // finite, plausible, and wrong. A boundary landing after a leading `-` loses
    // the sign the same way. Snapping recovers the whole token, and the column to
    // its left loses characters that were never its own.
    let mut bounds: Vec<usize> = Vec::with_capacity(starts.len());
    for (k, &from) in starts.iter().enumerate() {
        let mut at = from.min(cs.len());
        if k > 0
            && at > 0
            && at < cs.len()
            && !cs[at].is_whitespace()
            && !cs[at - 1].is_whitespace()
        {
            let floor = bounds[k - 1];
            while at > floor && !cs[at - 1].is_whitespace() {
                at -= 1;
            }
        }
        bounds.push(at.max(bounds.last().copied().unwrap_or(0)));
    }
    bounds
        .iter()
        .enumerate()
        .map(|(k, &from)| {
            let to = bounds.get(k + 1).copied().unwrap_or(cs.len()).min(cs.len());
            if from >= cs.len() || to <= from {
                return String::new();
            }
            cs[from..to]
                .iter()
                .collect::<String>()
                .trim()
                .trim_matches('"')
                .trim()
                .to_string()
        })
        .collect()
}

/// True for a line that is a comment rather than data. Skipped both when
/// detecting fixed-width column boundaries, where a run of prose would blank out
/// every boundary, and when reading rows, where it would become a part.
pub(crate) fn is_comment_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') || t.starts_with("//") || t.starts_with(';')
}

/// Read a fixed-width Eagle partlist into a [`Table`].
fn read_fixed_width(lines: &[&str], header_line: usize, dialect: BomDialect) -> Table {
    let body: Vec<(usize, &str)> = lines
        .iter()
        .enumerate()
        .skip(header_line + 1)
        .filter(|(_, l)| !l.trim().is_empty() && !is_comment_line(l))
        .map(|(n, l)| (n, *l))
        .collect();
    let data: Vec<&str> = body.iter().map(|(_, l)| *l).collect();
    let (headers, starts) = fixed_width_columns(lines[header_line], &data);
    let rows = body
        .iter()
        .map(|(n, l)| TableRow {
            cells: slice_fixed_width(l, &starts),
            line: n + 1,
        })
        .collect();

    Table {
        headers,
        rows,
        dialect,
    }
}

/// Decide which dialect a header row belongs to, or `None` when the row is not
/// a BOM header at all.
///
/// Order is load-bearing. An Altium BOM that also carries an `LCSC Part #`
/// column is an Altium BOM, so `LibRef` is tested before the distributor
/// columns; a KiCad grouped export that carries an `MPN` column is still a
/// KiCad grouped export.
fn classify_headers(keys: &[String]) -> Option<BomDialect> {
    let has = |k: &str| keys.iter().any(|x| x == k);
    let any_starting = |p: &str| keys.iter().any(|x| x.starts_with(p));

    let reference_like = keys.iter().any(|k| {
        role_candidates(k, None)
            .iter()
            .any(|(r, _)| *r == ColumnRole::Reference)
    });
    let supporting = keys.iter().any(|k| {
        role_candidates(k, None).iter().any(|(r, _)| {
            matches!(
                r,
                ColumnRole::Value | ColumnRole::Mpn | ColumnRole::Footprint | ColumnRole::Quantity
            )
        })
    });
    if !reference_like || !supporting {
        return None;
    }

    if has("digi_key_part_number") && has("manufacturer_part_number") {
        return Some(BomDialect::DigiKey);
    }
    if has("libref") {
        return Some(BomDialect::Altium);
    }
    if has("reference_s") {
        return Some(BomDialect::KicadGrouped);
    }
    if has("references") && (has("quantity_per_pcb") || has("component") || has("part")) {
        return Some(BomDialect::KicadUngrouped);
    }
    if any_starting("jlcpcb_part") {
        return Some(BomDialect::Jlcpcb);
    }
    if any_starting("lcsc") {
        return Some(BomDialect::Lcsc);
    }
    Some(BomDialect::Spreadsheet)
}

// ── Column mapping ──────────────────────────────────────────────────────────

/// Every role a normalised header could fill, with the confidence of each.
///
/// `dialect` sharpens two cases that are genuinely dialect-dependent: `Comment`
/// is the value column in an Altium, LCSC or JLCPCB export and a free-text note
/// anywhere else, and `Part` / `Device` are the Eagle partlist's own reference
/// and footprint columns while `Part` is only a guess in a spreadsheet.
fn role_candidates(key: &str, dialect: Option<BomDialect>) -> Vec<(ColumnRole, MappingConfidence)> {
    use ColumnRole::*;
    use MappingConfidence::*;
    let eagle = dialect.is_some_and(BomDialect::is_fixed_width);
    let comment_is_value = dialect.is_some_and(BomDialect::comment_is_value);

    let mut out = Vec::new();
    let mut push = |r: ColumnRole, c: MappingConfidence| out.push((r, c));
    match key {
        // ── reference designators ──
        "designator"
        | "designators"
        | "refdes"
        | "ref_des"
        | "reference_designator"
        | "reference_designators"
        | "reference_s"
        | "references"
        | "reference" => push(Reference, Certain),
        "ref"
        | "refs"
        | "top_designator"
        | "bottom_designator"
        | "topdesignator"
        | "bottomdesignator"
        | "component_reference" => push(Reference, Likely),
        "parts" => push(Reference, Likely),
        "part" => push(Reference, if eagle { Certain } else { Guess }),
        "customer_reference" | "component_number_on_pcb" => push(Reference, Guess),
        "component" | "item" | "row" => push(Reference, Guess),

        // ── value ──
        "value" => push(Value, Certain),
        "val" | "kicad_value" => push(Value, Likely),
        "comment" => push(Value, if comment_is_value { Likely } else { Guess }),
        "description_value" | "value_description" => push(Value, Likely),
        "description" => push(Value, Guess),

        // ── manufacturer part number ──
        "mpn"
        | "mpn1"
        | "manufacturer_part_number"
        | "mfr_part_number"
        | "mfg_part_number"
        | "manufacturer_part_no"
        | "manufacturer_part" => push(Mpn, Certain),
        "partnumber" | "part_number" | "mfg_pn" | "mfr_pn" | "manufacturer_pn" | "part_no" => {
            push(Mpn, Likely)
        }

        // ── manufacturer ──
        "manufacturer" | "manufacturer_name" => push(Manufacturer, Certain),
        "mfn" | "mfg" | "mfr" | "vendor" => push(Manufacturer, Likely),

        // ── quantity ──
        "quantity" | "qty" => push(Quantity, Certain),
        "quantity_per_pcb" | "quantity_1" | "quantity_per_side" | "qty_per_pcb" => {
            push(Quantity, Likely)
        }

        // ── footprint ──
        "footprint" => push(Footprint, Certain),
        "package" | "libpart" | "libref" | "package_id" => push(Footprint, Likely),
        "device" => push(Footprint, if eagle { Certain } else { Guess }),

        // ── populate ──
        "dnp" | "do_not_populate" | "do_not_place" | "populate" | "dni" => push(Populate, Certain),
        "population" | "fitted" | "assembly" => push(Populate, Likely),
        "status" | "assembly_class" | "placement" | "mount_type" => push(Populate, Guess),

        // ── distributor codes ──
        "lcsc"
        | "lcsc_part"
        | "lcsc_part_number"
        | "lcsc_partnumber"
        | "digi_key_part_number"
        | "digikey_part_number"
        | "mouser_part_number"
        | "arrow_part_number"
        | "digikey" => push(DistributorPart, Certain),
        // `LCSC Part Type` sits beside `LCSC Part #` in real exports and is a
        // category, not an order code, so the `_type` suffix is excluded rather
        // than left to tie with the column that is one.
        _ if key.ends_with("_type") => {}
        _ => {
            if key.starts_with("jlcpcb_part") || key.starts_with("lcsc_part") {
                push(DistributorPart, Certain);
            } else if key.starts_with("mpn") {
                push(Mpn, Likely);
            }
        }
    }
    out
}

/// Detect the column mapping, or refuse.
///
/// The contract this function exists to satisfy: a non-interactive run, which
/// is the main consumer since this feeds CI, never blocks on a question. It
/// either proceeds on a mapping every used column reached
/// [`MappingConfidence::Likely`] or better on, and records that mapping, or it
/// refuses with [`EXIT_INVALID_FOR_ANALYSIS`] naming the ambiguous column and
/// how to resolve it. An interactive caller reads [`ColumnMap::left_unmapped`]
/// and the refusal's own text to build the question it asks.
fn map_columns(
    table: &Table,
    dialect: BomDialect,
    overrides: &ColumnOverrides,
    name: &str,
) -> Result<ColumnMap, BomError> {
    let headers_list = || {
        table
            .headers
            .iter()
            .map(|h| format!("{h:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };

    // Candidates per role, best confidence first, in column order.
    let mut per_role: BTreeMap<ColumnRole, Vec<ColumnAssignment>> = BTreeMap::new();
    for (index, header) in table.headers.iter().enumerate() {
        if header.trim().is_empty() {
            continue;
        }
        let key = normalise_header(header);
        for (role, confidence) in role_candidates(&key, Some(dialect)) {
            per_role.entry(role).or_default().push(ColumnAssignment {
                role,
                index,
                header: header.clone(),
                confidence,
                confirmed_by_content: false,
            });
        }
    }

    // Overrides win outright, and are Certain because a human said so.
    for role in ColumnRole::all() {
        let Some(wanted) = overrides.get(*role) else {
            continue;
        };
        let found = table.headers.iter().position(|h| {
            h.eq_ignore_ascii_case(wanted) || normalise_header(h) == normalise_header(wanted)
        });
        let Some(index) = found else {
            return Err(BomError::UnknownColumnOverride {
                name: name.to_string(),
                role: role.flag_name().to_string(),
                wanted: wanted.to_string(),
                headers: headers_list(),
            });
        };
        per_role.insert(
            *role,
            vec![ColumnAssignment {
                role: *role,
                index,
                header: table.headers[index].clone(),
                confidence: MappingConfidence::Certain,
                confirmed_by_content: false,
            }],
        );
    }

    // A guessed reference column can be CONFIRMED by its own content: if every
    // cell in it splits into tokens shaped like reference designators, it is the
    // reference column, and that is evidence rather than a guess. This is what
    // turns a Digi-Key BOM-manager export, whose designators live under the
    // ambiguous header `Customer Reference`, from a refusal into a read.
    //
    // Two guards, and the first is the important one. Only headers whose PURPOSE
    // is a reference field are eligible, not any guess. Shape alone cannot tell a
    // designator from a part number, because `BC547` and `LM358` have exactly the
    // shape of `R547` and `LM358`; promoting a `Part` column full of part numbers
    // on shape alone would attach every row to a designator the board does not
    // have. So the evidence is "this header is the tool's own reference field AND
    // its content agrees", which is a real claim, rather than "this column looks
    // designator-ish", which is not.
    if let Some(candidates) = per_role.get_mut(&ColumnRole::Reference) {
        for c in candidates.iter_mut() {
            let eligible = matches!(
                normalise_header(&c.header).as_str(),
                "customer_reference" | "component_number_on_pcb" | "reference_designator"
            );
            if c.confidence == MappingConfidence::Guess
                && eligible
                && column_looks_like_references(table, c.index)
            {
                c.confidence = MappingConfidence::Likely;
                c.confirmed_by_content = true;
            }
        }
    }

    let mut map = ColumnMap::default();
    for (role, mut candidates) in per_role {
        candidates.sort_by_key(|c| (c.confidence, c.index));
        // Two columns spelled the same way are one column duplicated, not two
        // competing claims: `Value` beside `VALUE`, or `Footprint` twice, are both
        // real in exports, and taking the first is the only sane reading.
        let mut seen: Vec<String> = Vec::new();
        candidates.retain(|c| {
            let key = normalise_header(&c.header);
            let fresh = !seen.contains(&key);
            seen.push(key);
            fresh
        });
        let best = candidates[0].confidence;
        let tied: Vec<&ColumnAssignment> =
            candidates.iter().filter(|c| c.confidence == best).collect();

        // Two columns equally entitled to one role. Refusing is only right for
        // a role the analysis actually reads: a tie between two distributor
        // order codes or two manufacturer-name columns cannot make a bind wrong,
        // because neither is used for identity, so the first is taken and the
        // rest are recorded as ignored. The Reference role has its own carve-out:
        // an assembly BOM split by board side carries `topDesignator` and
        // `bottomDesignator`, which are one reference column in two halves.
        let unused_for_identity =
            matches!(role, ColumnRole::DistributorPart | ColumnRole::Manufacturer);
        let one_column_in_halves = role == ColumnRole::Reference && side_split_designators(&tied);
        let tie_matters = !unused_for_identity && !one_column_in_halves;
        if tied.len() > 1 && tie_matters {
            return Err(BomError::AmbiguousColumn {
                name: name.to_string(),
                role: role.flag_name().to_string(),
                headers: tied
                    .iter()
                    .map(|c| format!("{:?}", c.header))
                    .collect::<Vec<_>>()
                    .join(" and "),
            });
        }

        let chosen = candidates.remove(0);
        if chosen.confidence.usable_unattended() {
            map.used.push(chosen);
        } else {
            map.left_unmapped.push(chosen);
        }
    }

    // Reference is the join key: without a confident one nothing in the file
    // can be attached to a part, so this is a refusal rather than a note.
    if map.index_of(ColumnRole::Reference).is_none() {
        return match map
            .left_unmapped
            .iter()
            .find(|a| a.role == ColumnRole::Reference)
        {
            Some(guess) => Err(BomError::UnconfidentReferenceColumn {
                name: name.to_string(),
                candidate: guess.header.clone(),
                headers: headers_list(),
            }),
            None => Err(BomError::NotABom {
                name: name.to_string(),
                looked: MAX_BANNER_LINES,
                example: "Designator".to_string(),
            }),
        };
    }

    let claimed: Vec<usize> = map
        .used
        .iter()
        .chain(map.left_unmapped.iter())
        .map(|a| a.index)
        .collect();
    map.ignored_headers = table
        .headers
        .iter()
        .enumerate()
        .filter(|(i, h)| !claimed.contains(i) && !h.trim().is_empty())
        .map(|(_, h)| h.clone())
        .collect();
    Ok(map)
}

/// True when the tied reference candidates are the top/bottom pair a
/// side-split assembly BOM uses, which is one reference column in two halves
/// rather than two competing claims.
fn side_split_designators(tied: &[&ColumnAssignment]) -> bool {
    tied.iter().all(|c| {
        let k = normalise_header(&c.header);
        matches!(
            k.as_str(),
            "top_designator" | "bottom_designator" | "topdesignator" | "bottomdesignator"
        )
    })
}

// ── Cell interpretation ─────────────────────────────────────────────────────

/// Does one column's content read as reference designators?
///
/// The shape a designator has: one to four letters, then digits, optionally with
/// a suffix, and short. `R1`, `C101`, `U4A`, `BTN0`, `SW1`. Deliberately not
/// matched: anything starting with a digit (a distributor code), anything with no
/// digit at all (a packaging or status word), and anything long (a description).
///
/// The threshold is nine cells in ten, because a real designator column has the
/// occasional `TP` or `FID1-4` in it, and three non-empty cells minimum, because
/// two rows are not evidence of anything.
fn column_looks_like_references(table: &Table, index: usize) -> bool {
    let mut looked = 0usize;
    let mut matched = 0usize;
    for row in &table.rows {
        let Some(cell) = row.cells.get(index) else {
            continue;
        };
        let cell = cell.trim();
        if cell.is_empty() {
            continue;
        }
        looked += 1;
        let (refs, _) = split_references(cell);
        if !refs.is_empty() && refs.iter().all(|r| has_designator_shape(r)) {
            matched += 1;
        }
    }
    looked >= 3 && matched * 10 >= looked * 9
}

/// One token's shape, as [`column_looks_like_references`] defines it.
fn has_designator_shape(token: &str) -> bool {
    if token.len() > 12 {
        return false;
    }
    let letters = token
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .count();
    if letters == 0 || letters > 4 {
        return false;
    }
    let rest = &token[letters..];
    !rest.is_empty()
        && rest.chars().next().is_some_and(|c| c.is_ascii_digit())
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// A reference designator that names no specific part: KiCad's artwork
/// placeholders (`REF**`, `G***`), Altium's logo row (`*`), and an
/// unannotated designator whose number is still a question mark (`U?4`).
/// ...and a token that cannot be a designator at all, which is how a footer line
/// (`Total number of parts: 3 (generated by ...)`) stops becoming three phantom
/// parts. A designator always contains a letter and never contains bracket or
/// colon punctuation; `GND`, `V+`, `RX` and `MOTOR_POWER` are all real
/// designators from the survey, so a digit is not required.
fn is_placeholder_reference(s: &str) -> bool {
    s.contains('*')
        || s.contains('?')
        || s.contains(':')
        || s.contains('(')
        || s.contains(')')
        || s.len() > 24
        || !s.chars().any(|c| c.is_ascii_alphabetic())
}

/// Split a reference-designator cell into designators, returning the real ones
/// and the placeholders that were skipped.
///
/// Both separators are real and both appear in the survey: KiCad's grouped
/// export writes `"C1, C2, C3"`, its ungrouped export and the Digi-Key and
/// LibreSolar shapes write `C1 C2 C3`, and a hand-maintained sheet writes
/// `"C3, C2, C1, "` with a trailing separator. Semicolon and slash appear in
/// spreadsheets. So the split accepts all of them rather than picking one.
pub(crate) fn split_references(cell: &str) -> (Vec<String>, Vec<String>) {
    let mut refs: Vec<String> = Vec::new();
    let mut placeholders: Vec<String> = Vec::new();
    for tok in cell.split([',', ';', '/', ' ', '\t', '\n']) {
        let tok = tok.trim().trim_matches('"').trim();
        if tok.is_empty() {
            continue;
        }
        if is_placeholder_reference(tok) {
            placeholders.push(tok.to_string());
            continue;
        }
        if !refs.iter().any(|r| r == tok) {
            refs.push(tok.to_string());
        }
    }
    (refs, placeholders)
}

/// Singular or plural form, so a report never says "1 rows".
fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 {
        one
    } else {
        many
    }
}

/// A quantity cell, which is sometimes `1`, sometimes `1 pc`, sometimes blank.
fn parse_quantity(s: &str) -> Option<usize> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Interpret a populate / DNP cell, given the header it sits under.
///
/// The header decides the polarity and the cell alone cannot: `YES` under a
/// `DNP` header means do not populate, and `YES` under a `Populate` header means
/// the opposite. So a self-describing token (`DNP`, `fitted`) is read directly,
/// and a bare `yes` / `y` / `x` / `1` / `true` is read as an assertion of
/// whatever the header names, inverted when the header is a do-not-populate
/// header. A cell nobody can read either way returns `None`, which means "this
/// file says nothing", not "populate".
fn parse_populate(cell: &str, header_key: &str) -> Option<bool> {
    let c = cell.trim().to_ascii_lowercase();
    if let Some(explicit) = match c.as_str() {
        "dnp" | "dni" | "do not populate" | "do not place" | "no fit" | "nofit" | "nf"
        | "not populated" | "unpopulated" | "exclude" | "excluded" => Some(false),
        "populate" | "fit" | "fitted" | "populated" | "place" | "include" | "included"
        | "assemble" => Some(true),
        _ => None,
    } {
        return Some(explicit);
    }
    let asserted = match c.as_str() {
        "yes" | "y" | "x" | "1" | "true" | "t" => true,
        "no" | "n" | "0" | "false" | "f" => false,
        _ => return None,
    };
    // `DNP` and `DNI` name the negative; `Populate`, `Fitted`, `Assembly` name
    // the positive.
    let negative_header = matches!(
        header_key,
        "dnp" | "dni" | "do_not_populate" | "do_not_place"
    );
    Some(if negative_header { !asserted } else { asserted })
}

/// A value column that says `DNP` instead of a value is stating a populate
/// decision, not naming a part. Real: the survey found rows whose entire
/// identity is the word `DNP`.
fn populate_from_value(value: &str) -> Option<bool> {
    let v = value.trim().to_ascii_lowercase();
    (v == "dnp" || v == "dni" || v == "do not populate").then_some(false)
}

/// True when a cell plausibly IS a manufacturer part number rather than a
/// placeholder or a value restated.
///
/// A part number is at least three characters and contains a digit; KiCad's
/// `~` for nothing, a bare `-`, and a lone `?` are not part numbers. This
/// filter exists because an `MPN` column is very often present and mostly
/// empty, and an empty cell that reaches the binder as `Some("")` costs a bind.
fn looks_like_mpn(s: &str) -> bool {
    let s = s.trim();
    s.len() >= 3
        && s.chars().any(|c| c.is_ascii_digit())
        && !s.starts_with('~')
        && !is_distributor_code(s)
}

/// Is this cell a distributor's own order code rather than a manufacturer part
/// number?
///
/// A column called `LCSC` or `Digi-Key Part Number` is caught by its header, but a
/// column called `Part Number` or `SKU` is not, and real sheets put an LCSC code
/// under exactly that heading. So the SHAPE is checked as well, for the three
/// families that have one: LCSC's `C` followed by digits only, Digi-Key's `-ND`
/// suffix, and Mouser's `NNN-` numeric prefix. Matching a model regex against one
/// of these is how a part binds to the wrong device.
fn is_distributor_code(s: &str) -> bool {
    let up = s.trim().to_ascii_uppercase();
    // LCSC: C followed by four or more digits and nothing else.
    if let Some(rest) = up.strip_prefix('C') {
        if rest.len() >= 4 && rest.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    // Digi-Key: the `-ND` / `-1-ND` / `-CT-ND` suffix family.
    if up.ends_with("-ND") || up.ends_with("-ND)") {
        return true;
    }
    // Mouser: a three-or-four digit house prefix before a hyphen.
    if let Some((head, tail)) = up.split_once('-') {
        if (3..=4).contains(&head.len())
            && head.chars().all(|c| c.is_ascii_digit())
            && !tail.is_empty()
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn header_normalisation_folds_the_real_spellings() {
        assert_eq!(normalise_header("Reference(s)"), "reference_s");
        assert_eq!(normalise_header("\"Mid X\""), "mid_x");
        assert_eq!(normalise_header("JLCPCB Part #（optional）"), "jlcpcb_part");
        assert_eq!(
            normalise_header("Digi-Key Part Number"),
            "digi_key_part_number"
        );
        assert_eq!(normalise_header("Center-X(mm)"), "center_x_mm");
    }

    #[test]
    fn quoted_cells_survive_the_split() {
        assert_eq!(
            split_delimited(r#"1,2,"a, b",4"#, ','),
            vec!["1", "2", "a, b", "4"]
        );
        assert_eq!(
            split_delimited(r#""say ""hi""",2"#, ','),
            vec![r#"say "hi""#, "2"]
        );
    }

    #[test]
    fn references_split_on_comma_or_space_and_drop_placeholders() {
        let (refs, ph) = split_references("C3, C2, C1, ");
        assert_eq!(refs, vec!["C3", "C2", "C1"]);
        assert!(ph.is_empty());
        let (refs, _) = split_references("BTN0 BTN1 BTN2");
        assert_eq!(refs, vec!["BTN0", "BTN1", "BTN2"]);
        let (refs, ph) = split_references("REF**, G***, U?4, R1");
        assert_eq!(refs, vec!["R1"]);
        assert_eq!(ph, vec!["REF**", "G***", "U?4"]);
    }

    #[test]
    fn a_tab_separated_export_that_claims_to_be_csv_still_reads() {
        let text = "Designator\tValue\tMPN\nR1\t10k\tRC0402FR-0710KL\n";
        let bom = Bom::from_text(text, "claims.csv", &ColumnOverrides::new()).unwrap();
        assert_eq!(bom.rows.len(), 1);
        assert_eq!(bom.rows[0].mpn.as_deref(), Some("RC0402FR-0710KL"));
    }
}
