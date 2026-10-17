//! Extraction from EasyEDA / JLCEDA boards.
//!
//! EasyEDA is the browser CAD tool behind JLCPCB's assembly service, and `OSHWLab`
//! hosts a large public corpus of boards made in it. It has two editions with two
//! unrelated file formats, and this module reads both:
//!
//! | Edition | File | Shape |
//! |---|---|---|
//! | Standard (6.x) | `.json` "EasyEDA Source", docType 3 | one JSON object whose `shape` array holds `~`-separated records (`LIB~`, `PAD~`, `TRACK~`, `VIA~`, ...) |
//! | Pro (2.x) | `.epcb` layout, or a `.epro` / `.zip` project archive | JSON Lines, one array per record (`["COMPONENT", ...]`, `["PAD_NET", ...]`, `["LINE", ...]`) |
//!
//! Detection is by content, never by extension: the Standard form is a JSON
//! object with `head.docType` and a `shape` list, the Pro form is a file whose
//! first line is `["DOCTYPE","PCB",...]`, and an archive is a zip holding a Pro
//! `project.json` plus `.epcb`, or Standard `.json` PCB documents.
//!
//! ## What each edition gives
//!
//! * **Standard** states everything the electrical checks need in the board
//!   file: every part is a `LIB` record carrying its package, designator text and
//!   pads, and every pad names its net. Parts, pins with positions, nets and the
//!   package are read. A part's *value* is only present when the author let the
//!   footprint display it (`nameAlias` of `Value`); otherwise EasyEDA keeps it in
//!   the schematic and the part is read with its value reported unresolved,
//!   exactly as an Altium `.PcbDoc` is.
//! * **Pro** `.epcb` files keep the netlist as `PAD_NET` rows (component, pad,
//!   net) and leave pad geometry, footprint names and values in the project's
//!   footprint and device libraries, so a bare `.epcb` yields designators,
//!   placement and connectivity with footprints named only by library id. The
//!   `.epro` archive adds `project.json`, from which footprint and device
//!   titles are read when it states them.
//!
//! ## Copper
//!
//! Tracks, arcs, vias and pours are read and **counted**, with their layers and
//! total track length, but hauksbee's IR holds no copper (the same position as
//! [`crate::ipc2581`]), so an EasyEDA board reports clearance DRC and
//! trace-geometry checks as *not checked* rather than green. That is said in the
//! read's notes ([`EasyedaStats::notes`]) and in the verdict's conditions.
//!
//! ## Named, never silent
//!
//! Every record is classified. Records that are read go to the board; copper is
//! counted; records that carry nothing electrical (silkscreen text, 3D-model
//! nodes, design rules, fonts) are counted under [`EasyedaStats::read_past`];
//! anything else is counted under [`EasyedaStats::unsupported`](EasyedaStats#structfield.unsupported) by its record
//! name. Both lists reach the user as notes.
//!
//! ## Reuse decision
//!
//! Recorded here because the repository rule is to measure existing work before
//! writing a reader. The bar was: read a real Standard and a real Pro board into
//! parts, pins and nets, with named diagnostics, in Rust, with no GPL code.
//!
//! | Candidate | Licence | Decision | Why |
//! |---|---|---|---|
//! | EasyEDA's published Standard format pages (docs.easyeda.com, "EasyEDA PCB File Format") | documentation | **adopt** as the field reference | Documents `LIB`, `PAD`, `TRACK`, `VIA`, `COPPERAREA`, `SOLIDREGION`, `HOLE`, `TEXT`, the layer ids and the 10 mil unit. Cross-checked against two real exports; the docs leave `nameAlias`, the `@n` pad-number suffix and `SVGNODE` undocumented, which the fixtures settle. |
//! | wokwi/easyeda2kicad (TypeScript EasyEDA to KiCad board converter) | MIT | **borrow knowledge, not code** | Confirms the origin offset (4000, 3000), the 10 mil to mm factor, the PAD field order and that `TEXT` type `P` is the designator. It targets KiCad output (a different IR), is TypeScript, and drops what hauksbee must report, so porting it would not meet the "named diagnostic" bar. |
//! | KiCad's EasyEDA importer | GPL-3.0-or-later | **reject** | Licence-incompatible with a permissive crate, and C++ tied to KiCad's board model. Its public developer documentation was read only for the Pro file-container description (a zip of JSON Lines plus `project.json`). |
//! | `easyeda2kicad.py`, `JLC2KiCad_lib` and the other part-library converters | GPL-3.0 / MIT | **reject** | They convert library parts through EasyEDA's web API, not boards. |
//! | `LC2KiCad` (C++) | LGPL-3.0 | **reject** | Standard edition only, converts to KiCad 5, and LGPL linkage into a Rust crate buys nothing over a 600 line reader. |
//! | EasyEDA Pro format specification (v2.2 and v3 zips) | proprietary, "all rights reserved" | **not used** | Cannot be vendored or quoted. The Pro field layout here was inferred from a real MIT-licensed project and is covered only for the records that project exercises; v3 (the `\|\|` log form) is detected and refused by name. |
//!
//! Written from scratch because no permissively licensed library reads an
//! EasyEDA board into parts, pins and nets; the existing tools emit KiCad files
//! and discard or hide exactly the diagnostics this reader exists to produce.

mod pro;
mod std_json;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;

use crate::{sample_list, Component, ExtractError, ExtractedBoard, Net};

/// Millimetres per EasyEDA Standard unit. The editor's base unit is 10 mil.
pub(crate) const STD_UNIT_MM: f64 = 0.254;

/// The canvas origin the editor places a new document's (0, 0) at, in Standard
/// units. Subtracted so board coordinates start near zero like the other
/// readers', not around 4000 units out.
pub(crate) const STD_ORIGIN: (f64, f64) = (4000.0, 3000.0);

/// How much of the start of a file the content sniff reads.
const SNIFF_WINDOW: usize = 64 * 1024;

/// Longest record name or key echoed into a note. A hostile file can make these
/// arbitrarily long and a note is read by a person.
const NAME_LIMIT: usize = 40;

/// Which EasyEDA edition a file came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edition {
    /// EasyEDA Standard: a JSON document of `~`-separated shape strings.
    Standard,
    /// EasyEDA Pro: JSON Lines records, alone (`.epcb`) or in a project archive.
    Pro,
}

impl Edition {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "Standard",
            Self::Pro => "Pro",
        }
    }
}

/// Copper the reader saw and counted but the IR cannot hold.
#[derive(Debug, Clone, Default)]
pub struct CopperAccount {
    /// Straight copper track segments.
    pub tracks: usize,
    /// Their summed length in mm.
    pub track_length_mm: f64,
    /// Copper arcs.
    pub arcs: usize,
    /// Vias.
    pub vias: usize,
    /// Copper areas and pours (the outline; the fill is computed by the editor).
    pub pours: usize,
    /// Solid copper regions.
    pub solid_regions: usize,
    /// Pads that belong to no part (test points, fiducials drawn as pads).
    pub free_pads: usize,
    /// Names of the copper layers any of the above sit on.
    pub layers: BTreeSet<String>,
}

impl CopperAccount {
    const fn is_empty(&self) -> bool {
        self.tracks + self.arcs + self.vias + self.pours + self.solid_regions + self.free_pads == 0
    }

    fn sentence(&self) -> String {
        if self.is_empty() {
            return "no copper".to_string();
        }
        let mut parts = Vec::new();
        if self.tracks > 0 {
            parts.push(format!(
                "{} track segment(s) totalling {:.1} mm",
                self.tracks, self.track_length_mm
            ));
        }
        for (n, what) in [
            (self.arcs, "arc(s)"),
            (self.vias, "via(s)"),
            (self.pours, "pour(s)"),
            (self.solid_regions, "solid region(s)"),
            (self.free_pads, "free pad(s)"),
        ] {
            if n > 0 {
                parts.push(format!("{n} {what}"));
            }
        }
        let layers = if self.layers.is_empty() {
            String::new()
        } else {
            format!(
                " on {}",
                self.layers.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        };
        format!("{}{layers}", parts.join(", "))
    }
}

/// The honest accounting of an EasyEDA read.
#[derive(Debug, Clone)]
pub struct EasyedaStats {
    pub edition: Edition,
    /// What was read, for a note: `"a .json PCB document"`, `"PCB/x.epcb in a .epro project"`.
    pub source: String,
    /// The editor version the file declares, verbatim, or empty.
    pub editor_version: String,
    /// Pins that ended up on a net.
    pub connected_pins: usize,
    /// Placements dropped as board artwork because they carry no pad.
    pub artwork: Vec<String>,
    /// Declared nets no part pad sits on (a track, via or pour names them).
    pub nets_without_pads: Vec<String>,
    /// Counted, unmeasured copper.
    pub copper: CopperAccount,
    /// Records that carry nothing electrical, by readable description.
    pub read_past: BTreeMap<String, usize>,
    /// Records found and not readable, by record name. Never empty silently.
    pub unsupported: BTreeMap<String, usize>,
    /// Parts whose value the file does not carry.
    pub value_unresolved: Vec<String>,
    /// Edition-specific limits the reader owes the verdict, each a sentence.
    pub caveats: Vec<String>,
    /// Cross-checks inside the file that disagreed with themselves.
    pub disagreements: Vec<String>,
    /// Other boards in the same container that were not read.
    pub other_boards: Vec<String>,
}

impl EasyedaStats {
    pub(crate) fn new(edition: Edition, source: impl Into<String>) -> Self {
        Self {
            edition,
            source: source.into(),
            editor_version: String::new(),
            connected_pins: 0,
            artwork: Vec::new(),
            nets_without_pads: Vec::new(),
            copper: CopperAccount::default(),
            read_past: BTreeMap::new(),
            unsupported: BTreeMap::new(),
            value_unresolved: Vec::new(),
            caveats: Vec::new(),
            disagreements: Vec::new(),
            other_boards: Vec::new(),
        }
    }

    pub(crate) fn past(&mut self, what: &str) {
        *self.read_past.entry(what.to_string()).or_default() += 1;
    }

    pub(crate) fn unsupported(&mut self, what: &str) {
        *self.unsupported.entry(clip(what)).or_default() += 1;
    }

    /// The whole-sentence notes a report must carry for this read. The
    /// board-input normalizer copies them onto its own notes, so what the reader
    /// could not read reaches the user instead of stopping at this crate.
    #[must_use]
    pub fn notes(&self) -> Vec<String> {
        let version = if self.editor_version.is_empty() {
            String::new()
        } else {
            format!(", editor {}", clip(&self.editor_version))
        };
        let mut out = vec![format!(
            "EasyEDA {} input ({}{version}): the parts, pads and nets were read from the \
             document's own part and net records, not reverse-engineered from copper. \
             Copper was counted ({}) but not measured: clearance DRC and trace-geometry \
             SI need a layout format that carries checkable copper (KiCad, Eagle, \
             Altium) and were not run.",
            self.edition.as_str(),
            self.source,
            self.copper.sentence()
        )];
        out.extend(self.caveats.iter().cloned());
        if !self.value_unresolved.is_empty() {
            out.push(format!(
                "{} part(s) carry no value in this EasyEDA file ({}); EasyEDA keeps \
                 values with the schematic symbol, so the value-dependent checks treat \
                 them as unresolved rather than guessing one.",
                self.value_unresolved.len(),
                sample_list(&self.value_unresolved)
            ));
        }
        out.extend(crate::exchange_disclosures(
            &self.artwork,
            &self.nets_without_pads,
        ));
        if !self.unsupported.is_empty() {
            out.push(format!(
                "Found in the file and NOT read, so nothing in them was checked: {}.",
                counts(&self.unsupported)
            ));
        }
        if !self.read_past.is_empty() {
            out.push(format!(
                "Read past as carrying nothing electrical: {}.",
                counts(&self.read_past)
            ));
        }
        out.extend(self.disagreements.iter().cloned());
        if !self.other_boards.is_empty() {
            out.push(format!(
                "This container also holds {} other board(s) that were not read: {}. \
                 Pass one of them on its own to check it.",
                self.other_boards.len(),
                sample_list(&self.other_boards)
            ));
        }
        out
    }
}

fn counts(map: &BTreeMap<String, usize>) -> String {
    map.iter()
        .map(|(k, n)| format!("{n} x {k}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// `text` cut to a note-sized prefix, on a character boundary.
pub(crate) fn clip(text: &str) -> String {
    let mut out: String = text.chars().take(NAME_LIMIT).collect();
    if out.len() < text.len() {
        out.push('\u{2026}');
    }
    out
}

/// An EasyEDA read: the board plus its accounting.
#[derive(Debug)]
pub struct EasyedaExtraction {
    pub board: ExtractedBoard,
    pub stats: EasyedaStats,
}

/// The nets of a board in first-seen order. An empty name is "not connected",
/// which is the absence of a net rather than a net called `""`.
#[derive(Default)]
pub(crate) struct NetTable {
    pub(crate) nets: Vec<Net>,
    ids: HashMap<String, i64>,
}

impl NetTable {
    /// The id of net `name`, adding it on first sight. `None` for an empty name.
    pub(crate) fn id(&mut self, name: &str) -> Option<i64> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        if let Some(&id) = self.ids.get(name) {
            return Some(id);
        }
        let id = i64::try_from(self.nets.len()).ok()? + 1;
        self.ids.insert(name.to_string(), id);
        self.nets.push(Net::new(id, name));
        Some(id)
    }
}

/// A coordinate or size from a record field. `Ok(None)` for blank or garbage
/// text (the caller reports the record as unreadable), and a refusal for every
/// spelling that parses to a non-finite number (`NaN`, `inf`, `1e999`): a
/// position that is not a real number must stop the read, not flow into a check
/// as a confident wrong answer.
pub(crate) fn coord(text: &str) -> Result<Option<f64>, ExtractError> {
    match text.trim().parse::<f64>() {
        Ok(v) if v.is_finite() => Ok(Some(v)),
        Ok(_) => Err(ExtractError::Corrupt(format!(
            "this EasyEDA file has a coordinate or size that is not a finite number \
             ('{}'). A position like that cannot be checked truthfully, so nothing was \
             read. Re-export the board from EasyEDA, or look for a hand-edited value",
            clip(text.trim())
        ))),
        Err(_) => Ok(None),
    }
}

/// Assemble the finished board: artwork split out, duplicate designators merged,
/// unused nets named.
pub(crate) fn finish(
    name: String,
    nets: NetTable,
    components: Vec<Component>,
    stats: &mut EasyedaStats,
    keep_pinless: bool,
) -> ExtractedBoard {
    let components = if keep_pinless {
        crate::merge_duplicate_references(components)
    } else {
        let (kept, artwork) = crate::split_board_artwork(components);
        stats.artwork = artwork.iter().map(|a| clip(a)).collect();
        kept
    };
    stats.value_unresolved = components
        .iter()
        .filter(|c| {
            c.properties
                .iter()
                .any(|(k, _)| k == crate::altium::VALUE_UNRESOLVED_KEY)
        })
        .map(|c| clip(&c.reference))
        .collect();
    stats.connected_pins = components
        .iter()
        .flat_map(|c| c.pins.iter())
        .filter(|p| p.net.is_some())
        .count();
    stats.nets_without_pads = crate::nets_without_pads(&nets.nets, &components)
        .iter()
        .map(|n| clip(n))
        .collect();
    ExtractedBoard {
        name,
        nets: nets.nets,
        components,
    }
}

// ── detection ────────────────────────────────────────────────────────────────

/// The leading text of `bytes`, lossy-decoded, with a byte-order mark and leading
/// whitespace removed.
fn head(bytes: &[u8]) -> String {
    let window = &bytes[..bytes.len().min(SNIFF_WINDOW)];
    String::from_utf8_lossy(window)
        .trim_start_matches('\u{feff}')
        .trim_start()
        .to_string()
}

/// Content sniff for an EasyEDA Standard document.
///
/// A JSON object that declares a `docType` and carries a `canvas`, `shape` or
/// `dataStr`. Keyed on structure, so a renamed file still reads and an arbitrary
/// `.json` does not.
#[must_use]
pub fn looks_like_easyeda_std(bytes: &[u8]) -> bool {
    let head = head(bytes);
    head.starts_with('{')
        && head.contains("\"docType\"")
        && ["\"canvas\"", "\"shape\"", "\"dataStr\""]
            .iter()
            .any(|k| head.contains(k))
}

/// Content sniff for a Pro JSON Lines document: the first line is the
/// `["DOCTYPE", ...]` header every Pro design file opens with.
#[must_use]
pub fn looks_like_easyeda_pro(bytes: &[u8]) -> bool {
    let head = head(bytes);
    head.starts_with("[\"DOCTYPE\"") || head.starts_with("[ \"DOCTYPE\"")
}

/// Content sniff for the Pro v3 log form (`{...}||{...}` lines opened by a
/// `DOCHEAD` record). Claimed so it is refused by name, not as "unrecognized".
#[must_use]
pub fn looks_like_easyeda_pro_v3(bytes: &[u8]) -> bool {
    let head = head(bytes);
    head.starts_with('{')
        && head
            .lines()
            .next()
            .is_some_and(|l| l.contains("\"DOCHEAD\"") && l.contains("||"))
}

/// Does `bytes` look like any EasyEDA board document, of either edition?
#[must_use]
pub fn looks_like_easyeda(bytes: &[u8]) -> bool {
    looks_like_easyeda_std(bytes)
        || looks_like_easyeda_pro(bytes)
        || looks_like_easyeda_pro_v3(bytes)
}

// ── reading ──────────────────────────────────────────────────────────────────

/// Read an EasyEDA board document (Standard `.json` or Pro `.epcb`).
///
/// # Errors
/// [`ExtractError::Easyeda`] when the bytes are not a readable EasyEDA PCB. The
/// message names what the file is and what to do: a schematic, a footprint, a Pro
/// v3 log, invalid or truncated JSON, and a non-finite coordinate each have their
/// own sentence.
pub fn extract(bytes: &[u8]) -> Result<EasyedaExtraction, ExtractError> {
    if looks_like_easyeda_pro_v3(bytes) {
        return Err(ExtractError::Easyeda(PRO_V3_MESSAGE.to_string()));
    }
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_start_matches('\u{feff}');
    if looks_like_easyeda_pro(bytes) {
        return pro::extract(text, "a .epcb layout", "", None);
    }
    std_json::extract(text)
}

/// The refusal for a Pro v3 log file, exposed so the board-input normalizer can
/// refuse it ahead of reader selection like the other named refusals.
pub const PRO_V3_MESSAGE: &str =
    "this is an EasyEDA Pro file in the newer v3 log format (records joined by '||' \
     under a DOCHEAD line), which hauksbee does not read yet. Export the board from \
     EasyEDA Pro as Gerber, ODB++ or IPC-2581, or save the project in the v2 format \
     (.epro) and drop that instead";

// ── archives ─────────────────────────────────────────────────────────────────

/// Names of the members of a zip, or `None` when `bytes` is not a readable zip.
fn zip_names(bytes: &[u8]) -> Option<Vec<String>> {
    if !bytes.starts_with(b"PK\x03\x04") {
        return None;
    }
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).ok()?;
    let mut names = Vec::with_capacity(zip.len().min(4096));
    for i in 0..zip.len().min(100_000) {
        let name = zip.by_index_raw(i).ok()?.name().to_string();
        names.push(name);
    }
    Some(names)
}

fn is_epcb(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".epcb")
}

fn is_json(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".json")
}

fn base_name(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

/// Read up to `limit` bytes of one zip member.
fn read_member(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    index: usize,
    limit: u64,
) -> Result<Vec<u8>, ExtractError> {
    let f = zip.by_index(index).map_err(|e| {
        ExtractError::Easyeda(format!(
            "could not read a member of the EasyEDA archive: {e}"
        ))
    })?;
    let mut buf = Vec::new();
    f.take(limit.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| {
            ExtractError::Easyeda(format!(
                "could not unpack a member of the EasyEDA archive: {e}"
            ))
        })?;
    if buf.len() as u64 > limit {
        return Err(ExtractError::Easyeda(
            crate::odbpp::tree::expansion_limit_message("EasyEDA archive"),
        ));
    }
    Ok(buf)
}

/// Content sniff for an EasyEDA project archive: a zip that holds a Pro
/// `project.json` with at least one `.epcb`, or Standard PCB documents as
/// `.json` members. A zip of gerbers has neither.
#[must_use]
pub fn looks_like_easyeda_archive(bytes: &[u8]) -> bool {
    let Some(names) = zip_names(bytes) else {
        return false;
    };
    let has_project = names.iter().any(|n| base_name(n) == "project.json");
    if has_project && names.iter().any(|n| is_epcb(n)) {
        return true;
    }
    let Ok(mut zip) = zip::ZipArchive::new(std::io::Cursor::new(bytes)) else {
        return false;
    };
    names
        .iter()
        .enumerate()
        .filter(|(_, n)| is_json(n) && base_name(n) != "project.json")
        .take(16)
        .any(|(i, _)| {
            zip.by_index(i).is_ok_and(|f| {
                let mut head = Vec::new();
                f.take(SNIFF_WINDOW as u64).read_to_end(&mut head).is_ok()
                    && looks_like_easyeda_std(&head)
            })
        })
}

/// Read an EasyEDA project archive: a Pro `.epro` (or `.zip`) holding
/// `project.json` and `.epcb` layouts, or a zip of Standard `.json` documents.
///
/// The first board in path order is read; the rest are named in
/// [`EasyedaStats::other_boards`] rather than merged or silently dropped.
///
/// # Errors
/// [`ExtractError::Easyeda`] when the archive cannot be unpacked, holds no board,
/// or the board it holds does not read.
pub fn extract_archive(bytes: &[u8]) -> Result<EasyedaExtraction, ExtractError> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| {
        ExtractError::Easyeda(format!("this EasyEDA archive is not a readable zip: {e}"))
    })?;
    let mut members: Vec<(String, usize)> = (0..zip.len().min(100_000))
        .filter_map(|i| {
            let name = zip.by_index_raw(i).ok()?.name().to_string();
            Some((name, i))
        })
        .collect();
    members.sort();
    let budget = crate::MAX_INFLATED_BYTES;

    // Pro: project.json plus .epcb members.
    let project = members
        .iter()
        .find(|(n, _)| base_name(n) == "project.json")
        .map(|(_, i)| *i);
    let epcbs: Vec<&(String, usize)> = members.iter().filter(|(n, _)| is_epcb(n)).collect();
    if !epcbs.is_empty() {
        let project_json = match project {
            Some(i) => {
                serde_json::from_slice::<serde_json::Value>(&read_member(&mut zip, i, budget)?).ok()
            }
            None => None,
        };
        let mut read: Option<EasyedaExtraction> = None;
        let mut others = Vec::new();
        let mut first_err: Option<ExtractError> = None;
        for (name, idx) in &epcbs {
            if read.is_some() {
                others.push(name.clone());
                continue;
            }
            let data = read_member(&mut zip, *idx, budget)?;
            let text = String::from_utf8_lossy(&data);
            let text = text.trim_start_matches('\u{feff}');
            let source = format!("{} in a project archive", clip(base_name(name)));
            match pro::extract(text, &source, name, project_json.as_ref()) {
                Ok(got) => read = Some(got),
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        let Some(mut got) = read else {
            return Err(first_err.unwrap_or_else(|| {
                ExtractError::Easyeda("the EasyEDA archive holds no readable PCB".to_string())
            }));
        };
        got.stats.other_boards = others;
        return Ok(got);
    }

    // Standard: .json members that are PCB documents.
    let mut read: Option<EasyedaExtraction> = None;
    let mut others = Vec::new();
    for (name, idx) in &members {
        if !is_json(name) {
            continue;
        }
        let data = read_member(&mut zip, *idx, budget)?;
        if !looks_like_easyeda_std(&data) {
            continue;
        }
        if read.is_some() {
            others.push(name.clone());
            continue;
        }
        let text = String::from_utf8_lossy(&data);
        let mut got = std_json::extract(text.trim_start_matches('\u{feff}'))?;
        got.stats.source = format!("{name} in a zip");
        read = Some(got);
    }
    let Some(mut got) = read else {
        return Err(ExtractError::Easyeda(
            "this zip holds no EasyEDA PCB: it has no Pro .epcb layout and no Standard \
             .json PCB document. Export the board from EasyEDA (Standard: File > Export \
             > EasyEDA source; Pro: File > Save As > project .epro) and drop that"
                .to_string(),
        ));
    };
    got.stats.other_boards = others;
    Ok(got)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coord_refuses_every_non_finite_spelling() {
        for bad in ["NaN", "nan", "inf", "-inf", "infinity", "1e999"] {
            assert!(coord(bad).is_err(), "{bad:?} must be refused");
        }
        for unreadable in ["", "  ", "abc"] {
            assert_eq!(coord(unreadable).ok(), Some(None));
        }
        assert_eq!(coord(" -12.5 ").ok(), Some(Some(-12.5)));
    }

    #[test]
    fn sniffs_do_not_cross_claim() {
        let std = br#"{"head":{"docType":"3"},"canvas":"CA~1","shape":[]}"#;
        let pro = b"[\"DOCTYPE\",\"PCB\",\"1.8\"]\n[\"HEAD\",{}]\n";
        let v3 = b"{\"type\":\"DOCHEAD\"}||{\"docType\":\"PCB\"}\n";
        assert!(looks_like_easyeda_std(std) && !looks_like_easyeda_pro(std));
        assert!(looks_like_easyeda_pro(pro) && !looks_like_easyeda_std(pro));
        assert!(looks_like_easyeda_pro_v3(v3) && !looks_like_easyeda_std(v3));
        for other in [&b"(kicad_pcb)"[..], b"<eagle>", b"{\"a\":1}", b"[1,2]", b""] {
            assert!(!looks_like_easyeda(other));
        }
    }

    #[test]
    fn clip_bounds_a_hostile_name() {
        let long = "x".repeat(10_000);
        assert!(clip(&long).chars().count() <= NAME_LIMIT + 1);
        assert_eq!(clip("LIB"), "LIB");
    }
}
