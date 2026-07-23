//! Extraction from Altium Designer `.PcbDoc` board files.
//!
//! Altium is the dominant professional / enterprise / regulated-industry EDA
//! tool, so reading its native designs unlocks a large, serious tier of boards
//! that never touch KiCad or Eagle.
//!
//! ## Container
//!
//! A `.PcbDoc` is a Microsoft OLE2 / Compound File Binary (CFB) file: a
//! filesystem-in-a-file of *storages* (directories) and *streams* (files).
//! We open it with the battle-tested [`cfb`] crate rather than hand-rolling the
//! FAT/DIFAT. Each logical section is a sub-storage (`Pads6`, `Nets6`,
//! `Components6`, ...) holding a `Data` stream (the records) and a small
//! `Header` stream (a record count, which we ignore). Older Altium / Protel
//! files drop the `6` suffix (`Pads`, `Nets`, `Components`), so we try both.
//!
//! ## Record encoding
//!
//! Two encodings live inside the `Data` streams:
//!
//! - **Properties strings** (`Board6`, `Nets6`, `Components6`, `Polygons6`,
//!   `Classes6`): a u32 little-endian length (top byte is a flag, masked off),
//!   then a NUL-terminated ASCII string `|KEY=VALUE|KEY=VALUE|...`. Keys are
//!   uppercased; coordinate values carry a `mil` suffix.
//! - **Fixed binary records** (`Pads6`, `Vias6`, `Tracks6`, `Arcs6`): a 1-byte
//!   record-type marker, then one or more sub-records each prefixed with a u32
//!   length. Coordinates are signed `i32` in Altium internal units
//!   (1 unit = 2.54 nm = 1/10000 mil). Net and component references are u16
//!   indices into `Nets6` / `Components6` (`0xFFFF` = none).
//!
//! ## Provenance
//!
//! The record layouts here are ported field-by-field from KiCad's open-source
//! Altium importer (GPL/CC, KiCad master tree), principally:
//! `pcbnew/pcb_io/altium/altium_parser_pcb.cpp` (the `APAD6` / `AVIA6` /
//! `ATRACK6` / `AARC6` / `ACOMPONENT6` / `ANET6` parsers and the
//! `ALTIUM_LAYER` enum), `common/io/altium/altium_binary_parser.cpp`
//! (`ReadProperties`, the unit conversion), and `altium_props_utils.cpp`
//! (`ConvertToKicadUnit`). The `cfb` crate replaces KiCad's vendored
//! `CompoundFileReader`. Cross-checked against the `altium2kicad` project and a
//! Python `olefile` prototype before porting. See `docs/ingest/ALTIUM.md`.
//!
//! [`cfb`]: https://docs.rs/cfb

use crate::{Component, ExtractError, ExtractedBoard, Net, Pin};
use std::collections::HashMap;
use std::io::{Cursor, Read};

/// Millimetres per Altium internal coordinate unit. Altium stores coordinates
/// as `i32` in units of 1/10000 mil = 0.1 microinch = 2.54 nm, so
/// `mm = unit * 2.54e-6`. (KiCad's `ConvertToKicadUnit` is the same factor
/// expressed in nanometres: `nm = unit * 2.54`.)
pub(crate) const MM_PER_UNIT: f64 = 2.54e-6;

/// Net / component reference sentinel meaning "not attached".
pub(crate) const NONE_U16: u16 = 0xFFFF;

/// An Altium net index that means "the board outline polygon" (used by
/// `Polygons6`); not a real net.
const POLYGON_BOARD_NET: u16 = 0xFFFE;

// ── Altium layer ids (the 1-byte `LAYER` field) ──────────────────────────────
// Ported from KiCad `ALTIUM_LAYER` (altium_parser_pcb.h). We only need to tell
// copper layers apart and name the two outer ones the way the rest of hauksbee
// does (`F.Cu` / `B.Cu` / `In<n>.Cu`).

pub(crate) const ALTIUM_TOP_LAYER: u8 = 1;
pub(crate) const ALTIUM_BOTTOM_LAYER: u8 = 32;
pub(crate) const ALTIUM_MULTI_LAYER: u8 = 74;

/// True when an Altium layer id is a copper layer (Top, 30 inner, Bottom) or
/// the multi-layer slot that through-hole pads/vias live on.
pub(crate) fn is_copper_layer(layer: u8) -> bool {
    (ALTIUM_TOP_LAYER..=ALTIUM_BOTTOM_LAYER).contains(&layer) || layer == ALTIUM_MULTI_LAYER
}

/// Canonical copper-layer name, matching the `F.Cu` / `B.Cu` / `In<n>.Cu`
/// convention used by every other extractor / the DRC engine.
pub(crate) fn layer_name(layer: u8) -> String {
    match layer {
        ALTIUM_TOP_LAYER => "F.Cu".to_string(),
        ALTIUM_BOTTOM_LAYER => "B.Cu".to_string(),
        // Mid layers 2..=31 -> In1.Cu..=In30.Cu.
        n if (2..=31).contains(&n) => format!("In{}.Cu", n - 1),
        // Through-hole copper has no single side; report it as front for the
        // human-facing description (the DRC fans it across all copper layers).
        _ => "F.Cu".to_string(),
    }
}

/// Map an Altium layer *name* (as found in `Components6`/`Board6` properties,
/// e.g. "TOP", "BOTTOM") to its side for component placement.
pub(crate) fn side_from_layer_name(name: &str) -> &'static str {
    match name.trim().to_ascii_uppercase().as_str() {
        "BOTTOM" | "BOTTOMLAYER" => "B.Cu",
        _ => "F.Cu",
    }
}

// ── CFB container access ──────────────────────────────────────────────────────

/// An opened `.PcbDoc`: the CFB container plus the resolved storage-name suffix
/// ("6" for Altium Designer, "" for older Protel-era files).
pub(crate) struct PcbDoc {
    cf: cfb::CompoundFile<Cursor<Vec<u8>>>,
    /// "6" or "" depending on the file generation.
    suffix: &'static str,
}

impl PcbDoc {
    /// Open the CFB container from raw bytes and detect the storage naming.
    pub(crate) fn open(bytes: &[u8]) -> Result<Self, ExtractError> {
        let cf = cfb::CompoundFile::open(Cursor::new(bytes.to_vec()))
            .map_err(|e| ExtractError::Altium(format!("not a valid OLE2/CFB file: {e}")))?;
        // Altium Designer uses the `6` suffix; older Protel files do not.
        let suffix = if cf.is_stream("/Nets6/Data") || cf.exists("/Nets6") {
            "6"
        } else if cf.is_stream("/Nets/Data") || cf.exists("/Nets") {
            ""
        } else {
            return Err(ExtractError::Altium(
                "OLE2 file has no Altium Nets stream (not a .PcbDoc?)".into(),
            ));
        };
        Ok(PcbDoc { cf, suffix })
    }

    /// Read a whole `Data` stream for a section (e.g. "Pads" -> "Pads6/Data"),
    /// returning `None` when the section is absent (a board may have no vias).
    pub(crate) fn data(&mut self, section: &str) -> Option<Vec<u8>> {
        let path = format!("/{section}{}/Data", self.suffix);
        if !self.cf.is_stream(&path) {
            return None;
        }
        let mut s = self.cf.open_stream(&path).ok()?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).ok()?;
        Some(buf)
    }
}

// ── Stream cursor for the two record encodings ────────────────────────────────

/// A forward cursor over a `Data` stream.
pub(crate) struct StreamReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> StreamReader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        StreamReader { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    fn u8_at(&self, off: usize) -> u8 {
        self.buf.get(off).copied().unwrap_or(0)
    }

    fn u16_at(&self, off: usize) -> u16 {
        if off + 2 <= self.buf.len() {
            u16::from_le_bytes([self.buf[off], self.buf[off + 1]])
        } else {
            0
        }
    }

    fn u32_at(&self, off: usize) -> u32 {
        if off + 4 <= self.buf.len() {
            u32::from_le_bytes([
                self.buf[off],
                self.buf[off + 1],
                self.buf[off + 2],
                self.buf[off + 3],
            ])
        } else {
            0
        }
    }

    fn i32_at(&self, off: usize) -> i32 {
        self.u32_at(off) as i32
    }

    /// Coordinate at `off` converted to millimetres.
    fn coord_mm(&self, off: usize) -> f64 {
        self.i32_at(off) as f64 * MM_PER_UNIT
    }

    // ── Properties-string records ─────────────────────────────────────────────

    /// Read the next properties block as a key->value map (keys uppercased), or
    /// `None` at end of stream. The block is a u32 length (top byte masked) then
    /// a NUL-terminated `|KEY=VALUE|...` string.
    pub(crate) fn next_properties(&mut self) -> Option<HashMap<String, String>> {
        if self.remaining() < 4 {
            return None;
        }
        let len = (self.u32_at(self.pos) & 0x00FF_FFFF) as usize;
        self.pos += 4;
        if self.remaining() < len {
            self.pos = self.buf.len();
            return None;
        }
        let raw = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        Some(parse_properties_bytes(raw))
    }

    // ── Fixed binary records ──────────────────────────────────────────────────

    /// At the start of a fixed binary record, read the 1-byte record-type
    /// marker, returning `None` at end of stream.
    pub(crate) fn next_record_type(&mut self) -> Option<u8> {
        if self.remaining() < 1 {
            return None;
        }
        let t = self.u8_at(self.pos);
        self.pos += 1;
        Some(t)
    }

    /// Enter the next sub-record: read its u32 length and return the absolute
    /// payload start and end offsets, leaving `pos` parked just before the
    /// payload. Returns `None` if the stream is exhausted.
    fn enter_subrecord(&mut self) -> Option<(usize, usize)> {
        if self.remaining() < 4 {
            return None;
        }
        let len = self.u32_at(self.pos) as usize;
        self.pos += 4;
        let start = self.pos;
        let end = (start + len).min(self.buf.len());
        Some((start, end))
    }

    /// Skip a sub-record entirely (length then payload).
    fn skip_subrecord(&mut self) {
        if let Some((_, end)) = self.enter_subrecord() {
            self.pos = end;
        }
    }
}

/// Parse a `|KEY=VALUE|...` Altium properties block from its RAW bytes into an
/// uppercased-key map, decoding each value by its key. Altium writes a name
/// field twice in one block: a CP1252 twin (`NAME=Mü` as `M\xFC`) and a UTF-8
/// twin (`%UTF8%NAME=Mü` as `M\xC3\xBC`). Decoding the whole block as one unit
/// failed UTF-8 on the CP1252 twin's high byte and fell back to CP1252 for
/// everything — mojibake'ing the genuine UTF-8 twin (`M\xC3\xBC` → `MÃ¼`),
/// which is exactly the twin `prop_str` prefers. So decode per value: a
/// `%UTF8%` key is genuine UTF-8 (strict, lossy only as a last resort); the
/// ANSI twin keeps the UTF-8-or-CP1252 heuristic.
fn parse_properties_bytes(raw: &[u8]) -> HashMap<String, String> {
    // Trim the block's trailing NUL terminator(s).
    let mut end = raw.len();
    while end > 0 && raw[end - 1] == 0 {
        end -= 1;
    }
    let raw = &raw[..end];

    let mut map = HashMap::new();
    for tok in raw.split(|&b| b == b'|') {
        if tok.is_empty() {
            continue;
        }
        let Some(eq) = tok.iter().position(|&b| b == b'=') else {
            continue;
        };
        // Keys are ASCII; decode with the same heuristic, then uppercase.
        let key = decode_altium_str(&tok[..eq]).trim().to_ascii_uppercase();
        let val_bytes = &tok[eq + 1..];
        let val = if key.starts_with("%UTF8%") {
            match std::str::from_utf8(val_bytes) {
                Ok(s) => s.to_string(),
                Err(_) => String::from_utf8_lossy(val_bytes).into_owned(),
            }
        } else {
            decode_altium_str(val_bytes)
        };
        map.insert(key, val);
    }
    map
}

/// Read a property value, preferring a `%UTF8%`-prefixed twin key when present
/// (Altium stores the Unicode form of footprint/library names that way).
fn prop_str(map: &HashMap<String, String>, key: &str) -> String {
    let utf8_key = format!("%UTF8%{key}");
    map.get(&utf8_key.to_ascii_uppercase())
        .or_else(|| map.get(&key.to_ascii_uppercase()))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Parse an Altium length value that carries a unit suffix (`"100mil"`,
/// `"+12.5mil"`, `"1.5mm"`) into millimetres. Bare numbers are assumed mil
/// (Altium's property default).
pub(crate) fn parse_len_mm(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some(v) = s.strip_suffix("mil") {
        return v.trim().parse::<f64>().ok().map(|m| m * 0.0254);
    }
    if let Some(v) = s.strip_suffix("mm") {
        return v.trim().parse::<f64>().ok();
    }
    s.parse::<f64>().ok().map(|m| m * 0.0254)
}

// ── Connectivity extraction ───────────────────────────────────────────────────

/// One pad's connectivity-relevant fields, pulled from a PADS6 record.
pub(crate) struct PadRecord {
    pub name: String,
    pub layer: u8,
    pub net: u16,
    pub component: u16,
    pub x_mm: f64,
    pub y_mm: f64,
}

/// Decode an Altium Properties/text byte string. Altium stores these in the
/// native Windows codepage (Windows-1252), though modern exports may be UTF-8.
/// Prefer a valid UTF-8 reading (exact for ASCII and modern files); otherwise
/// fall back to Windows-1252 so a byte like `0xE4` ('ä') decodes to the right
/// character instead of the U+FFFD `from_utf8_lossy` would emit — which silently
/// corrupts internationally-authored footprint/net/component names.
fn decode_altium_str(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_string(),
        Err(_) => raw.iter().map(|&b| cp1252_char(b)).collect(),
    }
}

/// Map one Windows-1252 byte to its Unicode scalar. Bytes `0x00–0x7F` and
/// `0xA0–0xFF` coincide with Latin-1 (byte value == code point); `0x80–0x9F`
/// carry the CP1252-specific punctuation (smart quotes, dashes, €, …). The five
/// undefined slots map to U+FFFD.
fn cp1252_char(b: u8) -> char {
    match b {
        0x80 => '\u{20AC}',
        0x82 => '\u{201A}',
        0x83 => '\u{0192}',
        0x84 => '\u{201E}',
        0x85 => '\u{2026}',
        0x86 => '\u{2020}',
        0x87 => '\u{2021}',
        0x88 => '\u{02C6}',
        0x89 => '\u{2030}',
        0x8A => '\u{0160}',
        0x8B => '\u{2039}',
        0x8C => '\u{0152}',
        0x8E => '\u{017D}',
        0x91 => '\u{2018}',
        0x92 => '\u{2019}',
        0x93 => '\u{201C}',
        0x94 => '\u{201D}',
        0x95 => '\u{2022}',
        0x96 => '\u{2013}',
        0x97 => '\u{2014}',
        0x98 => '\u{02DC}',
        0x99 => '\u{2122}',
        0x9A => '\u{0161}',
        0x9B => '\u{203A}',
        0x9C => '\u{0153}',
        0x9E => '\u{017E}',
        0x9F => '\u{0178}',
        0x81 | 0x8D | 0x8F | 0x90 | 0x9D => '\u{FFFD}',
        // 0x00–0x7F and 0xA0–0xFF: Latin-1, where the byte IS the code point.
        _ => b as char,
    }
}

/// Parse every record in a `Pads6/Data` stream into [`PadRecord`]s. The PADS6
/// layout is the hardest in the format: six sub-records, with the geometry in
/// sub-record 5 (variable length, branched on its declared size). Ported from
/// KiCad `APAD6` (`altium_parser_pcb.cpp`).
pub(crate) fn parse_pads(buf: &[u8]) -> Vec<PadRecord> {
    let mut r = StreamReader::new(buf);
    let mut out = Vec::new();
    while let Some(rt) = r.next_record_type() {
        if rt != 2 {
            // Not a PAD marker: the stream is corrupt or a version we do not
            // model. Stop rather than misread.
            break;
        }
        // Sub-record 1: the pad designator (a Pascal string inside the block).
        let Some((s1, e1)) = r.enter_subrecord() else {
            break;
        };
        let name = if e1 > s1 {
            let nlen = r.u8_at(s1) as usize;
            let from = s1 + 1;
            let to = (from + nlen).min(e1);
            decode_altium_str(&buf[from..to])
        } else {
            String::new()
        };
        r.pos = e1;
        // Sub-records 2,3,4: skipped.
        r.skip_subrecord();
        r.skip_subrecord();
        r.skip_subrecord();
        // Sub-record 5: geometry.
        let Some((s5, e5)) = r.enter_subrecord() else {
            break;
        };
        // The geometry fields run through offset s5+20 (the Y coordinate's last
        // byte). enter_subrecord clamps a declared length that overruns the
        // buffer, so a truncated stream yields e5 < s5+21; the field readers
        // below would then silently return 0 and place the pad at the origin.
        // Stop rather than misread — the same discipline as the record-marker
        // check above (a phantom pad at (0,0) is worse than a short pad list).
        if e5 - s5 < 21 {
            break;
        }
        let layer = r.u8_at(s5);
        let net = r.u16_at(s5 + 3);
        let component = r.u16_at(s5 + 7);
        let x_mm = r.coord_mm(s5 + 13);
        // Altium Y is up; the DRC is self-consistent in relative positions, so
        // we keep Altium's frame (no negation) the way the Eagle path does.
        let y_mm = r.coord_mm(s5 + 17);
        r.pos = e5;
        // Sub-record 6: per-layer stack, skipped (we use the top/mid/bot sizes
        // for DRC, not the 32-layer table).
        r.skip_subrecord();
        out.push(PadRecord {
            name,
            layer,
            net,
            component,
            x_mm,
            y_mm,
        });
    }
    out
}

/// One component, from a COMPONENT(S)6 properties record.
struct CompRecord {
    refdes: String,
    pattern: String,
    library: String,
    layer_name: String,
    /// Last segment of `SOURCEHIERARCHICALPATH` (the channel name on a
    /// channel-replicated design, e.g. "FLASH2"); used to disambiguate repeated
    /// designators the way KiCad's importer does.
    channel: String,
    x_mm: f64,
    y_mm: f64,
    rotation: f64,
}

fn parse_components(buf: &[u8]) -> Vec<CompRecord> {
    let mut r = StreamReader::new(buf);
    let mut out = Vec::new();
    while let Some(m) = r.next_properties() {
        let refdes = prop_str(&m, "SOURCEDESIGNATOR");
        // KiCad prepends "UNK" to an all-digit designator; mirror that so the
        // refdes is a valid-looking reference for the binder.
        let refdes = if !refdes.is_empty() && refdes.chars().all(|c| c.is_ascii_digit()) {
            format!("UNK{refdes}")
        } else {
            refdes
        };
        let x_mm = parse_len_mm(&prop_str(&m, "X")).unwrap_or(0.0);
        let y_mm = parse_len_mm(&prop_str(&m, "Y")).unwrap_or(0.0);
        let rotation = prop_str(&m, "ROTATION").parse::<f64>().unwrap_or(0.0);
        // Altium hierarchical paths use a backslash separator; the last segment
        // is the channel name on a replicated design.
        let channel = prop_str(&m, "SOURCEHIERARCHICALPATH")
            .rsplit('\\')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        out.push(CompRecord {
            refdes,
            pattern: prop_str(&m, "PATTERN"),
            library: prop_str(&m, "SOURCEFOOTPRINTLIBRARY"),
            layer_name: prop_str(&m, "LAYER"),
            channel,
            x_mm,
            y_mm,
            rotation,
        });
    }
    out
}

/// Net names in stream order; a primitive's net field is a 0-based index here.
pub(crate) fn parse_net_names(buf: &[u8]) -> Vec<String> {
    let mut r = StreamReader::new(buf);
    let mut out = Vec::new();
    while let Some(m) = r.next_properties() {
        out.push(prop_str(&m, "NAME"));
    }
    out
}

/// Component reference designators in stream order (index = component field on a
/// primitive). Used by the DRC geometry path to attach an owner to each pad.
pub(crate) fn parse_component_refs(buf: &[u8]) -> Vec<String> {
    parse_components(buf)
        .into_iter()
        .map(|c| c.refdes)
        .collect()
}

/// Read every properties record in a `Data` stream into uppercased-key maps.
/// Exposed so the DRC polygon parser can share the one decoder.
pub(crate) fn properties_records(buf: &[u8]) -> Vec<HashMap<String, String>> {
    let mut r = StreamReader::new(buf);
    let mut out = Vec::new();
    while let Some(m) = r.next_properties() {
        out.push(m);
    }
    out
}

/// Map an Altium layer *name* (as in a `Polygons6`/`Board6` `LAYER=` property)
/// to its 1-byte layer id. Only the values we act on are mapped; anything else
/// returns a non-copper sentinel so the DRC skips it.
pub(crate) fn layer_id_from_name(name: &str) -> u8 {
    match name.trim().to_ascii_uppercase().as_str() {
        "TOP" | "TOPLAYER" => ALTIUM_TOP_LAYER,
        "BOTTOM" | "BOTTOMLAYER" => ALTIUM_BOTTOM_LAYER,
        "MULTILAYER" => ALTIUM_MULTI_LAYER,
        other => {
            // "MID1".."MID30" -> 2..=31
            if let Some(n) = other.strip_prefix("MID").and_then(|s| s.parse::<u8>().ok()) {
                if (1..=30).contains(&n) {
                    return n + 1;
                }
            }
            0 // not a copper layer we handle
        }
    }
}

/// Component comment/value text comes from a TEXTS6 record flagged
/// `isComment`, linked back to its component by index. Returns
/// component-index -> comment string. Best-effort: a board without comment
/// labels simply yields none.
fn parse_comment_texts(buf: &[u8]) -> HashMap<u16, String> {
    let mut r = StreamReader::new(buf);
    let mut out: HashMap<u16, String> = HashMap::new();
    while let Some(rt) = r.next_record_type() {
        if rt != 5 {
            break;
        }
        // Sub-record 1: the fixed text properties.
        let Some((s1, e1)) = r.enter_subrecord() else {
            break;
        };
        let component = r.u16_at(s1 + 7);
        // is_comment / is_designator only exist when the block is long enough.
        let is_comment = if e1 - s1 >= 123 {
            r.u8_at(s1 + 41) != 0
        } else {
            false
        };
        r.pos = e1;
        // Sub-record 2: the text string (a Pascal string). We ignore WideStrings
        // indirection here; the inline form covers the common case.
        let Some((s2, e2)) = r.enter_subrecord() else {
            break;
        };
        if is_comment && component != NONE_U16 && e2 > s2 {
            let tlen = r.u8_at(s2) as usize;
            let from = s2 + 1;
            let to = (from + tlen).min(e2);
            let txt = decode_altium_str(&buf[from..to]);
            // Altium stores special tokens (".Designator", ".Comment") as the
            // inline text when the displayed string is bound to a field rather
            // than a literal; the real value then lives in WideStrings6, which we
            // do not resolve. Skip the placeholders so the value stays empty
            // (the binder works off footprint + connectivity regardless) rather
            // than mislabelling every part ".Comment".
            if !txt.starts_with('.') && !txt.is_empty() {
                out.entry(component).or_insert(txt);
            }
        }
        r.pos = e2;
    }
    out
}

/// Extract the connectivity model from a `.PcbDoc`'s raw bytes.
pub fn extract(bytes: &[u8]) -> Result<ExtractedBoard, ExtractError> {
    let mut doc = PcbDoc::open(bytes)?;

    let net_names = doc
        .data("Nets")
        .map(|b| parse_net_names(&b))
        .unwrap_or_default();
    let comps = doc
        .data("Components")
        .map(|b| parse_components(&b))
        .unwrap_or_default();
    let pads = doc.data("Pads").map(|b| parse_pads(&b)).unwrap_or_default();
    let comments = doc
        .data("Texts")
        .map(|b| parse_comment_texts(&b))
        .unwrap_or_default();

    // Nets: id is the 0-based index + 1 so that id 0 stays the hauksbee "no net"
    // bucket (matching the KiCad / Eagle convention that net 0 is unconnected).
    let nets: Vec<Net> = net_names
        .iter()
        .enumerate()
        .map(|(i, name)| Net {
            id: i as i64 + 1,
            name: name.clone(),
        })
        .collect();

    // Group pads by their owning component index.
    let mut pads_by_comp: HashMap<u16, Vec<&PadRecord>> = HashMap::new();
    let mut free_pads: Vec<&PadRecord> = Vec::new();
    for p in &pads {
        if p.component == NONE_U16 {
            free_pads.push(p);
        } else {
            pads_by_comp.entry(p.component).or_default().push(p);
        }
    }

    let net_id = |field: u16| -> Option<i64> {
        if field == NONE_U16 || field == POLYGON_BOARD_NET {
            None
        } else if (field as usize) < net_names.len() {
            Some(field as i64 + 1)
        } else {
            None
        }
    };

    // A channel-replicated design (e.g. three identical FLASH banks) reuses the
    // same `SOURCEDESIGNATOR` ("C1") across channels. KiCad disambiguates those
    // by appending the channel name ("C1_FLASH2"); mirror that so every
    // component has a unique reference for the binder. Designators that are
    // already unique are left untouched.
    let mut refdes_count: HashMap<&str, usize> = HashMap::new();
    for c in &comps {
        *refdes_count.entry(c.refdes.as_str()).or_default() += 1;
    }
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut components: Vec<Component> = Vec::with_capacity(comps.len());
    for (idx, c) in comps.iter().enumerate() {
        let idx16 = idx as u16;
        // Build a unique reference.
        let mut reference = c.refdes.clone();
        if refdes_count.get(c.refdes.as_str()).copied().unwrap_or(0) > 1 {
            if !c.channel.is_empty() {
                reference = format!("{}_{}", c.refdes, c.channel);
            }
            // Guarantee uniqueness even if the channel is missing/duplicated.
            let mut n = 2;
            while used.contains(&reference) {
                reference = format!("{}_{}", c.refdes, n);
                n += 1;
            }
        }
        used.insert(reference.clone());
        let pins: Vec<Pin> = pads_by_comp
            .get(&idx16)
            .map(|ps| {
                ps.iter()
                    .map(|p| Pin {
                        number: p.name.clone(),
                        net: net_id(p.net),
                        function: String::new(),
                        kind: String::new(),
                        position: Some((p.x_mm, p.y_mm)),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let value = comments.get(&idx16).cloned().unwrap_or_default();
        let lib_id = if c.library.is_empty() {
            c.pattern.clone()
        } else {
            format!("{}:{}", c.library, c.pattern)
        };
        components.push(Component {
            reference,
            value,
            lib_id,
            footprint: c.pattern.clone(),
            position: Some((c.x_mm, c.y_mm, c.rotation)),
            layer: side_from_layer_name(&c.layer_name).to_string(),
            properties: Vec::new(),
            dnp: false,
            pins,
        });
    }

    Ok(ExtractedBoard {
        name: String::new(),
        nets,
        components,
    })
}

/// Quick content sniff: is this byte slice an Altium `.PcbDoc`? It must be an
/// OLE2 file (`D0 CF 11 E0` magic) that actually contains an Altium Nets
/// storage. The container check alone is not enough: many unrelated formats
/// (old `.doc`, `.msi`, `.msg`) are OLE2 too.
pub fn looks_like_pcbdoc(bytes: &[u8]) -> bool {
    if bytes.len() < 8 || bytes[0..4] != [0xD0, 0xCF, 0x11, 0xE0] {
        return false;
    }
    PcbDoc::open(bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assemble a minimal single-record `Pads6/Data` stream: a PAD marker, a
    /// name sub-record ("P1"), three skipped sub-records, then a geometry
    /// sub-record that *declares* `geom_len` bytes but only supplies `geom_have`
    /// of them. `enter_subrecord` clamps the declared length to the buffer, so
    /// `geom_have < geom_len` models a truncated stream.
    fn pads_stream(geom_len: u32, geom_have: usize) -> Vec<u8> {
        let mut b = Vec::new();
        b.push(2u8); // record type: PAD
        // sub-record 1 (name): len=3, payload = [nlen=2, 'P','1'].
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&[2, b'P', b'1']);
        // sub-records 2,3,4: empty (len=0), skipped.
        for _ in 0..3 {
            b.extend_from_slice(&0u32.to_le_bytes());
        }
        // sub-record 5 (geometry): declared length then the supplied bytes.
        b.extend_from_slice(&geom_len.to_le_bytes());
        b.extend(std::iter::repeat(0u8).take(geom_have));
        b
    }

    #[test]
    fn truncated_pad_geometry_is_dropped_not_zeroed() {
        // Bug-hunt #4: a geometry sub-record shorter than the 21 bytes the fields
        // span used to be read as zeros, placing a phantom pad at the origin.
        // With the guard, the truncated record is dropped entirely.
        let buf = pads_stream(50, 10);
        assert!(
            parse_pads(&buf).is_empty(),
            "a truncated pad record must be dropped, not emitted at (0,0)"
        );
    }

    #[test]
    fn full_pad_geometry_is_parsed() {
        // Control: the SAME shape with enough geometry bytes yields one pad, so
        // the truncation guard is what drops the record above — not a malformed
        // fixture.
        let buf = pads_stream(21, 21);
        let pads = parse_pads(&buf);
        assert_eq!(pads.len(), 1, "a full-length pad record must parse");
        assert_eq!(pads[0].name, "P1");
    }

    #[test]
    fn cp1252_decode_recovers_non_ascii() {
        // Bug-hunt #5: a Windows-1252 'ä' (0xE4) must decode to 'ä', not the
        // U+FFFD that from_utf8_lossy produced.
        assert_eq!(decode_altium_str(&[b'R', 0xE4]), "Rä");
        // The CP1252-specific range (0x80-0x9F): 0x92 is a right single quote.
        assert_eq!(decode_altium_str(&[0x92]), "\u{2019}");
        // Valid UTF-8 is preserved exactly (modern exports).
        assert_eq!(decode_altium_str("Nét".as_bytes()), "Nét");
        // Plain ASCII is unchanged.
        assert_eq!(decode_altium_str(b"GND"), "GND");
    }

    #[test]
    fn utf8_twin_survives_a_cp1252_twin_in_the_same_block() {
        // R12: a block carrying BOTH a CP1252 twin (NAME=M\xFC) and a UTF-8 twin
        // (%UTF8%NAME=M\xC3\xBC). The CP1252 high byte makes the block invalid
        // UTF-8; decoding it whole mojibake'd the UTF-8 twin. Per-value decoding
        // keeps each twin correct, so prop_str's preferred %UTF8% value is "Mü".
        let mut block: Vec<u8> = Vec::new();
        block.extend_from_slice(b"|NAME=M");
        block.push(0xFC); // CP1252 'ü'
        block.extend_from_slice(b"|%UTF8%NAME=M");
        block.extend_from_slice(&[0xC3, 0xBC]); // UTF-8 'ü'
        block.push(0x00); // block terminator
        let map = parse_properties_bytes(&block);
        assert_eq!(map.get("NAME").map(String::as_str), Some("Mü"), "CP1252 twin");
        assert_eq!(
            map.get("%UTF8%NAME").map(String::as_str),
            Some("Mü"),
            "UTF-8 twin must not be mojibake'd"
        );
        assert_eq!(prop_str(&map, "NAME"), "Mü", "prop_str prefers the clean %UTF8% twin");
    }
}
