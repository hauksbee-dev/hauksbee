//! EasyEDA Pro (2.x) PCB documents: JSON Lines, one array per record.
//!
//! The record layouts below were inferred from a real MIT-licensed `.epcb`
//! (the format specification is all-rights-reserved and was not used), so only
//! the records that file exercises are read: `COMPONENT`, `ATTR`, `PAD_NET`,
//! `NET`, `VIA`, `LINE`, `POUR`, `POLY`. Anything else is counted by name under
//! "not read". Coordinates are in mil (the file's own rule table is in whole
//! mil multiples of the JLCPCB capabilities, and a via of 12.0078 / 24.0158 is
//! 0.305 / 0.61 mm).

use std::collections::HashMap;

use serde_json::Value;

use super::{clip, finish, EasyedaExtraction, EasyedaStats, Edition, NetTable};
use crate::altium::{REFERENCE_UNRESOLVED_KEY, VALUE_UNRESOLVED_KEY};
use crate::{Component, ExtractError, Pin};

/// Millimetres per Pro coordinate unit.
const UNIT_MM: f64 = 0.0254;

const VALUE_UNRESOLVED_REASON: &str =
    "no value in the EasyEDA Pro layout; Pro keeps values in the project's device library, so \
     give the whole .epro project (or a BOM) to read them";

/// Most nodes searched in a project file for a footprint title.
const PROJECT_NODE_BUDGET: usize = 100_000;

const fn is_copper(layer: i64) -> bool {
    matches!(layer, 1 | 2 | 15..=46)
}

const fn layer_kind(layer: i64) -> &'static str {
    match layer {
        3 | 4 => "silkscreen",
        5 | 6 => "paste-mask",
        7 | 8 => "solder-mask",
        11 => "board-outline",
        12 => "multi-layer",
        13 | 14 => "document",
        _ => "other non-copper",
    }
}

fn text(row: &[Value], i: usize) -> &str {
    row.get(i).and_then(Value::as_str).unwrap_or("")
}

fn number(row: &[Value], i: usize) -> Option<f64> {
    row.get(i).and_then(Value::as_f64).filter(|v| v.is_finite())
}

fn integer(row: &[Value], i: usize) -> Option<i64> {
    row.get(i).and_then(Value::as_i64)
}

fn json_error(line: usize, e: &serde_json::Error) -> ExtractError {
    use serde_json::error::Category;
    let why = match e.classify() {
        Category::Eof => "ends in the middle of a record: the file looks truncated",
        _ => "is not a valid JSON record",
    };
    ExtractError::Easyeda(format!(
        "line {line} of this EasyEDA Pro file {why} ({e}), so nothing was read. \
         Save the board again from EasyEDA Pro or download it again"
    ))
}

/// The library object `uuid` in a project file: the nearest object keyed by
/// that uuid. Best effort, bounded.
fn find_object<'a>(project: &'a Value, uuid: &str) -> Option<&'a serde_json::Map<String, Value>> {
    let mut stack = vec![(project, 0_usize)];
    let mut seen = 0_usize;
    while let Some((node, depth)) = stack.pop() {
        seen += 1;
        if seen > PROJECT_NODE_BUDGET || depth > 10 {
            return None;
        }
        match node {
            Value::Object(map) => {
                if let Some(Value::Object(hit)) = map.get(uuid) {
                    return Some(hit);
                }
                stack.extend(map.values().map(|v| (v, depth + 1)));
            }
            Value::Array(items) => stack.extend(items.iter().map(|v| (v, depth + 1))),
            _ => {}
        }
    }
    None
}

/// The readable title of a library object, if it has one.
fn object_title(object: &serde_json::Map<String, Value>) -> Option<String> {
    ["display_title", "title", "name"].iter().find_map(|key| {
        object
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(clip)
    })
}

/// Title of the library object `uuid` in a project file.
fn lookup_title(project: &Value, uuid: &str) -> Option<String> {
    find_object(project, uuid).and_then(object_title)
}

/// What a project's device library says about one part: the device record a
/// layout's `Device` attribute points at.
///
/// INFERRED, not specified: the layout of `project.json` below was read from
/// public EasyEDA Pro project exports (a top-level `devices` object keyed by the
/// device uuid, each with a `title` and an `attributes` object holding `Value`,
/// `Manufacturer Part`, `Manufacturer` and `Supplier Part`). It is covered by the
/// in-test project archives, not by a real `.epro` fixture.
#[derive(Default)]
struct DeviceIdentity {
    /// The part's value when the device record states one, else its
    /// manufacturer part number (which names the part outright).
    value: String,
    /// Properties to attach (`Manufacturer Part`, `Manufacturer`, `Supplier Part`).
    properties: Vec<(String, String)>,
}

fn device_identity(project: &Value, uuid: &str) -> DeviceIdentity {
    let record = project
        .get("devices")
        .and_then(|d| d.get(uuid))
        .and_then(Value::as_object)
        .or_else(|| find_object(project, uuid));
    let Some(attributes) = record
        .and_then(|r| r.get("attributes"))
        .and_then(Value::as_object)
    else {
        return DeviceIdentity::default();
    };
    let attr = |key: &str| {
        attributes
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(clip)
    };
    let mut identity = DeviceIdentity::default();
    for key in ["Manufacturer Part", "Manufacturer", "Supplier Part"] {
        if let Some(v) = attr(key) {
            identity.properties.push((key.to_string(), v));
        }
    }
    identity.value = attr("Value")
        .or_else(|| attr("Manufacturer Part"))
        .unwrap_or_default();
    identity
}

/// The board's name from a project file, else from the member path.
pub(super) fn board_title(project: Option<&Value>, member: &str) -> String {
    let from_project = project
        .and_then(|p| p.get("title").or_else(|| p.get("name")))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(clip);
    from_project.unwrap_or_else(|| {
        let file = member.rsplit(['/', '\\']).next().unwrap_or(member);
        let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
        if stem.is_empty() {
            String::new()
        } else {
            clip(stem)
        }
    })
}

struct Part {
    layer: i64,
    position: Option<(f64, f64, f64)>,
    reference: String,
    footprint_uuid: String,
    device_uuid: String,
    value: String,
    extra: Vec<(String, String)>,
    pins: Vec<Pin>,
}

/// The state of one Pro document while its records are read.
struct Doc {
    stats: EasyedaStats,
    nets: NetTable,
    parts: Vec<Part>,
    by_id: HashMap<String, usize>,
    layer_names: HashMap<i64, String>,
    /// `(component id, pad number, net)` from `PAD_NET` records.
    pad_rows: Vec<(String, String, String)>,
    /// `(parent id, key, value)` from `ATTR` records.
    attr_rows: Vec<(String, String, String)>,
}

/// A record that opens the file must be `["DOCTYPE", "PCB", ...]`.
fn check_header(tag: &str, row: &[Value]) -> Result<(), ExtractError> {
    if tag != "DOCTYPE" {
        return Err(ExtractError::Easyeda(
            "this file does not open with the EasyEDA Pro DOCTYPE record".to_string(),
        ));
    }
    let kind = text(row, 1);
    if kind != "PCB" {
        return Err(ExtractError::Easyeda(format!(
            "this EasyEDA Pro file is a {} document, not a PCB board. Open the \
             board in the PCB editor and save that (.epcb), or drop the whole \
             project (.epro)",
            clip(kind)
        )));
    }
    Ok(())
}

impl Doc {
    fn new(source: &str) -> Self {
        Self {
            stats: EasyedaStats::new(Edition::Pro, source),
            nets: NetTable::default(),
            parts: Vec::new(),
            by_id: HashMap::new(),
            layer_names: HashMap::new(),
            pad_rows: Vec::new(),
            attr_rows: Vec::new(),
        }
    }

    fn layer_name(&self, layer: i64) -> String {
        self.layer_names
            .get(&layer)
            .cloned()
            .unwrap_or_else(|| format!("layer {layer}"))
    }

    /// One record after the header.
    fn record(&mut self, tag: &str, row: &[Value]) {
        match tag {
            "HEAD" => {
                if let Some(v) = row
                    .get(1)
                    .and_then(|h| h.get("editorVersion"))
                    .and_then(Value::as_str)
                {
                    self.stats.editor_version = clip(v);
                }
            }
            "LAYER" => {
                if let Some(id) = integer(row, 1) {
                    self.layer_names.insert(id, clip(text(row, 3)));
                }
            }
            "NET" => {
                self.nets.id(text(row, 1));
            }
            "COMPONENT" => self.component(row),
            "ATTR" => self.attr_rows.push((
                text(row, 3).to_string(),
                text(row, 7).to_string(),
                text(row, 8).to_string(),
            )),
            "PAD_NET" => self.pad_rows.push((
                text(row, 1).to_string(),
                text(row, 2).to_string(),
                text(row, 3).to_string(),
            )),
            "VIA" => {
                self.nets.id(text(row, 3));
                self.stats.copper.vias += 1;
            }
            "LINE" | "ARC" => self.line(tag, row),
            "POUR" => self.pour(row),
            "POLY" => {
                let layer = integer(row, 4).unwrap_or(0);
                if is_copper(layer) {
                    self.stats.copper.solid_regions += 1;
                } else {
                    self.stats.past(&format!("{} graphics", layer_kind(layer)));
                }
            }
            "POURED" => self.stats.past("computed copper-pour fill polygons"),
            "STRING" => self.stats.past("text labels"),
            "IMAGE" => self.stats.past("embedded images"),
            "FONT" | "SILK_OPTS" | "ACTIVE_LAYER" | "LAYER_PHYS" | "CANVAS" | "PREFERENCE"
            | "PRIMITIVE" | "RULE" | "RULE_SELECTOR" | "RULE_TEMPLATE" | "PANELIZE"
            | "PANELIZE_STAMP" | "PANELIZE_SIDE" => {
                self.stats.past("editor settings and design rules");
            }
            other => self.stats.unsupported(other),
        }
    }

    fn component(&mut self, row: &[Value]) {
        let position = match (number(row, 4), number(row, 5)) {
            (Some(x), Some(y)) => Some((x * UNIT_MM, y * UNIT_MM, number(row, 6).unwrap_or(0.0))),
            _ => None,
        };
        self.by_id
            .insert(text(row, 1).to_string(), self.parts.len());
        self.parts.push(Part {
            layer: integer(row, 3).unwrap_or(1),
            position,
            reference: String::new(),
            footprint_uuid: String::new(),
            device_uuid: String::new(),
            value: String::new(),
            extra: Vec::new(),
            pins: Vec::new(),
        });
    }

    /// A `LINE` or `ARC`: copper when on a copper layer, otherwise a graphic.
    fn line(&mut self, tag: &str, row: &[Value]) {
        let layer = integer(row, 4).unwrap_or(0);
        if !is_copper(layer) {
            self.stats.past(&format!("{} graphics", layer_kind(layer)));
            return;
        }
        self.nets.id(text(row, 3));
        let name = self.layer_name(layer);
        self.stats.copper.layers.insert(name);
        if tag == "ARC" {
            self.stats.copper.arcs += 1;
        } else if let (Some(x1), Some(y1), Some(x2), Some(y2)) = (
            number(row, 5),
            number(row, 6),
            number(row, 7),
            number(row, 8),
        ) {
            self.stats.copper.tracks += 1;
            self.stats.copper.track_length_mm += (x2 - x1).hypot(y2 - y1) * UNIT_MM;
        } else {
            self.stats.unsupported("LINE with unreadable coordinates");
        }
    }

    fn pour(&mut self, row: &[Value]) {
        self.nets.id(text(row, 3));
        self.stats.copper.pours += 1;
        if let Some(layer) = integer(row, 4) {
            let name = self.layer_name(layer);
            self.stats.copper.layers.insert(name);
        }
    }

    /// Attributes and pad rows refer to parts by id and may come before or
    /// after the part, so resolve them once every part is known.
    fn resolve(&mut self) {
        for (parent, key, value) in std::mem::take(&mut self.attr_rows) {
            let Some(&i) = self.by_id.get(&parent) else {
                self.stats.past("attributes of non-component objects");
                continue;
            };
            let part = &mut self.parts[i];
            match key.as_str() {
                "Designator" => part.reference = value.trim().to_string(),
                "Footprint" => part.footprint_uuid = value,
                "Device" => part.device_uuid = value,
                "Value" => part.value = value,
                "" => {}
                _ => {
                    if part.extra.len() < 32 {
                        part.extra
                            .push((clip(&key), value.chars().take(200).collect()));
                    }
                }
            }
        }
        for (component, number_text, net) in std::mem::take(&mut self.pad_rows) {
            let Some(&i) = self.by_id.get(&component) else {
                self.stats.disagreements.push(format!(
                    "A PAD_NET record names component '{}' which the file does not place, so \
                     that connection was not read.",
                    clip(&component)
                ));
                continue;
            };
            let net = self.nets.id(&net);
            self.parts[i].pins.push(Pin {
                number: number_text,
                net,
                ..Default::default()
            });
        }
    }

    /// The finished components, resolving footprint and device names through
    /// the project file when there is one.
    fn components(&mut self, project: Option<&Value>) -> Vec<Component> {
        let mut footprints_unresolved = 0_usize;
        let components: Vec<Component> = std::mem::take(&mut self.parts)
            .into_iter()
            .enumerate()
            .map(|(index, p)| {
                let mut properties = p.extra;
                let footprint = project
                    .and_then(|pr| lookup_title(pr, &p.footprint_uuid))
                    .unwrap_or_default();
                if footprint.is_empty() {
                    footprints_unresolved += 1;
                    if !p.footprint_uuid.is_empty() {
                        properties.push(("footprint_uuid".to_string(), clip(&p.footprint_uuid)));
                    }
                }
                let mut value = p.value;
                if !p.device_uuid.is_empty() {
                    let device = project.and_then(|pr| lookup_title(pr, &p.device_uuid));
                    properties.push((
                        "device".to_string(),
                        device.unwrap_or_else(|| clip(&p.device_uuid)),
                    ));
                    // The layout's own Value attribute wins; the device library
                    // fills only what the layout leaves empty.
                    if let Some(pr) = project {
                        let identity = device_identity(pr, &p.device_uuid);
                        if value.is_empty() {
                            value = identity.value;
                        }
                        properties.extend(identity.properties);
                    }
                }
                if value.is_empty() {
                    properties.push((
                        VALUE_UNRESOLVED_KEY.to_string(),
                        VALUE_UNRESOLVED_REASON.to_string(),
                    ));
                }
                let reference = if p.reference.is_empty() {
                    let generated = format!("UNK{index}");
                    properties.push((
                        REFERENCE_UNRESOLVED_KEY.to_string(),
                        format!(
                            "no Designator attribute; assigned stable synthetic identity \
                             '{generated}'"
                        ),
                    ));
                    generated
                } else {
                    p.reference
                };
                Component {
                    reference,
                    value,
                    lib_id: if footprint.is_empty() {
                        String::new()
                    } else {
                        format!("easyeda:{footprint}")
                    },
                    footprint,
                    position: p.position,
                    layer: if p.layer == 2 { "B.Cu" } else { "F.Cu" }.to_string(),
                    properties,
                    dnp: false,
                    pins: p.pins,
                }
            })
            .collect();

        if footprints_unresolved > 0 {
            self.stats.caveats.push(format!(
                "EasyEDA Pro keeps pad geometry, footprint names and part values in the \
                 project's libraries, not in the .epcb layout. {footprints_unresolved} \
                 part(s) have an unresolved footprint here, so only connectivity and \
                 placement were read. Pad positions are not in this file."
            ));
        }
        self.stats.caveats.push(
            "A Pro layout states a part's pads only through its PAD_NET records, so a part with \
             none is read as board artwork."
                .to_string(),
        );
        components
    }
}

/// Read a Pro JSON Lines PCB document. `source` names where it came from and
/// `member` is its path inside an archive (empty for a loose file); `project`
/// is the archive's `project.json` when there was one.
pub(super) fn extract(
    text_in: &str,
    source: &str,
    member: &str,
    project: Option<&Value>,
) -> Result<EasyedaExtraction, ExtractError> {
    let mut doc = Doc::new(source);
    let mut first = true;

    for (n, line) in text_in.lines().enumerate() {
        let line_no = n + 1;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|e| json_error(line_no, &e))?;
        let Value::Array(row) = &value else {
            return Err(ExtractError::Easyeda(format!(
                "line {line_no} of this EasyEDA Pro file is not a record (an array whose \
                 first item names the record), so the file is not a Pro design document"
            )));
        };
        let tag = text(row, 0);
        if tag.is_empty() {
            return Err(ExtractError::Easyeda(format!(
                "line {line_no} of this EasyEDA Pro file is a record without a name, so \
                 the file is not a Pro design document"
            )));
        }
        if first {
            first = false;
            check_header(tag, row)?;
        } else {
            doc.record(tag, row);
        }
    }
    if first {
        return Err(ExtractError::Easyeda(
            "this EasyEDA Pro file is empty, so there is no board to read".to_string(),
        ));
    }

    doc.resolve();
    let components = doc.components(project);
    let name = board_title(project, member);
    let Doc {
        nets, mut stats, ..
    } = doc;
    let board = finish(name, nets, components, &mut stats, false);
    Ok(EasyedaExtraction { board, stats })
}
