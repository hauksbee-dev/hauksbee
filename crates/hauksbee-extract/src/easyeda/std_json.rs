//! EasyEDA Standard (6.x) PCB documents: one JSON object whose `shape` list holds
//! `~`-separated records.
//!
//! Field orders are those of EasyEDA's published "PCB File Format" page, checked
//! against real exports; the places the page is silent (`nameAlias`, the `@n`
//! pad-number suffix, the TEXT `display` slot) were settled from the fixtures.
//! Coordinates are in 10 mil units with the canvas origin at (4000, 3000).

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use super::{
    clip, coord, finish, EasyedaExtraction, EasyedaStats, Edition, NetTable, STD_ORIGIN,
    STD_UNIT_MM,
};
use crate::altium::{REFERENCE_UNRESOLVED_KEY, VALUE_UNRESOLVED_KEY};
use crate::{Component, ExtractError, Pin};

/// Why a Standard part has no value, for the engine's bind report.
const VALUE_UNRESOLVED_REASON: &str =
    "no value in the EasyEDA PCB; EasyEDA keeps values with the schematic symbol";

/// Attribute keys that describe the editor's simulation or 3D setup, not the part.
const SKIPPED_ATTRS: &[&str] = &[
    "package",
    "nameAlias",
    "spicePre",
    "spiceSymbolName",
    "3DModel",
    "Contributor",
    "link",
    "Value",
];

/// Most attributes kept per part, and the longest value kept. A hostile file can
/// attach thousands of enormous ones.
const MAX_ATTRS: usize = 32;
const MAX_ATTR_VALUE: usize = 200;

fn json_error(e: &serde_json::Error) -> ExtractError {
    use serde_json::error::Category;
    let reason = match e.classify() {
        Category::Eof => {
            "ends in the middle of the JSON: the file looks truncated (an interrupted \
             download or save)"
        }
        Category::Syntax => "is not valid JSON",
        Category::Data | Category::Io => "does not have the structure of an EasyEDA document",
    };
    ExtractError::Easyeda(format!(
        "this EasyEDA file {reason} ({e}), so nothing was read. Re-export the board \
         from EasyEDA (File > Export > EasyEDA source) or download it again"
    ))
}

const fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// What a `docType` value means, for a refusal that says what the file is.
fn doc_type_name(doc_type: &str) -> &'static str {
    match doc_type {
        "1" => "schematic sheet",
        "2" => "schematic symbol",
        "4" => "PCB footprint (a library part)",
        "5" => "multi-sheet schematic",
        "14" => "PCB module",
        _ => "document of another kind",
    }
}

/// Copper layers: top, bottom and the 32 inner layers.
fn is_copper(layer: &str) -> bool {
    matches!(layer.trim().parse::<u32>(), Ok(1 | 2 | 21..=52))
}

fn layer_kind(layer: &str) -> &'static str {
    match layer.trim() {
        "3" | "4" => "silkscreen",
        "5" | "6" => "paste-mask",
        "7" | "8" => "solder-mask",
        "10" => "board-outline",
        "11" => "multi-layer",
        "12" => "documentation",
        "13" | "14" | "15" => "fabrication",
        "99" | "100" | "101" => "component-shape",
        _ => "other non-copper",
    }
}

/// Field `i` of a record's arguments, or `""` when the record is short.
fn arg<'a>(args: &[&'a str], i: usize) -> &'a str {
    args.get(i).copied().unwrap_or("")
}

struct Ctx<'a> {
    nets: NetTable,
    stats: EasyedaStats,
    layer_names: &'a HashMap<String, String>,
}

impl Ctx<'_> {
    fn layer_name(&self, id: &str) -> String {
        let id = id.trim();
        self.layer_names.get(id).map_or_else(
            || match id {
                "1" => "TopLayer".to_string(),
                "2" => "BottomLayer".to_string(),
                other => format!("layer {}", clip(other)),
            },
            |n| clip(n),
        )
    }

    /// A non-copper primitive: counted by what it is, never dropped silently.
    fn past_graphic(&mut self, layer: &str) {
        self.stats.past(&format!("{} graphics", layer_kind(layer)));
    }

    /// One copper-capable primitive (`TRACK`, `ARC`, `VIA`, ...) outside or
    /// inside a footprint. `layer` is `None` for a via, which spans the stack.
    fn primitive(&mut self, tag: &str, args: &[&str]) -> Result<(), ExtractError> {
        match tag {
            "TRACK" => {
                let layer = arg(args, 1);
                if !is_copper(layer) {
                    self.past_graphic(layer);
                    return Ok(());
                }
                self.nets.id(arg(args, 2));
                let mut points = Vec::new();
                for t in arg(args, 3).split_whitespace() {
                    let Some(v) = coord(t)? else {
                        self.stats.unsupported("TRACK with unreadable coordinates");
                        return Ok(());
                    };
                    points.push(v);
                }
                let pts: Vec<(f64, f64)> = points.chunks_exact(2).map(|c| (c[0], c[1])).collect();
                let length: f64 = pts
                    .windows(2)
                    .map(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1) * STD_UNIT_MM)
                    .sum();
                let segments = pts.len().saturating_sub(1);
                self.stats.copper.tracks += segments;
                self.stats.copper.track_length_mm += length;
                let name = self.layer_name(layer);
                self.stats.copper.layers.insert(name);
            }
            "ARC" => {
                let layer = arg(args, 1);
                if is_copper(layer) {
                    self.nets.id(arg(args, 2));
                    self.stats.copper.arcs += 1;
                    let name = self.layer_name(layer);
                    self.stats.copper.layers.insert(name);
                } else {
                    self.past_graphic(layer);
                }
            }
            "VIA" => {
                coord(arg(args, 0))?;
                coord(arg(args, 1))?;
                self.nets.id(arg(args, 3));
                self.stats.copper.vias += 1;
                for id in ["1", "2"] {
                    let name = self.layer_name(id);
                    self.stats.copper.layers.insert(name);
                }
            }
            "COPPERAREA" => {
                let layer = arg(args, 1);
                self.nets.id(arg(args, 2));
                if is_copper(layer) {
                    self.stats.copper.pours += 1;
                    let name = self.layer_name(layer);
                    self.stats.copper.layers.insert(name);
                } else {
                    self.past_graphic(layer);
                }
            }
            "SOLIDREGION" => {
                let layer = arg(args, 0);
                let kind = arg(args, 3);
                if is_copper(layer) && kind == "solid" {
                    self.nets.id(arg(args, 1));
                    self.stats.copper.solid_regions += 1;
                    let name = self.layer_name(layer);
                    self.stats.copper.layers.insert(name);
                } else if is_copper(layer) {
                    self.stats.past(&format!(
                        "copper {} region (a keep-out or cut-out)",
                        clip(kind)
                    ));
                } else {
                    self.past_graphic(layer);
                }
            }
            "CIRCLE" => {
                let layer = arg(args, 4);
                if is_copper(layer) {
                    self.stats.unsupported("CIRCLE on a copper layer");
                } else {
                    self.past_graphic(layer);
                }
            }
            "TEXT" => {
                let layer = arg(args, 6);
                if is_copper(layer) {
                    self.stats.unsupported("TEXT on a copper layer");
                } else {
                    self.stats.past("text labels");
                }
            }
            "HOLE" => self.stats.past("non-plated mounting holes"),
            "SVGNODE" => self.stats.past("SVG graphic and 3D-model nodes"),
            other => self.stats.unsupported(other),
        }
        Ok(())
    }
}

/// `key`/`value` pairs of a part's backtick-delimited attribute string.
fn parse_attrs(text: &str) -> BTreeMap<String, String> {
    let parts: Vec<&str> = text.split('`').collect();
    parts
        .chunks_exact(2)
        .filter(|kv| !kv[0].is_empty())
        .map(|kv| (kv[0].to_string(), kv[1].to_string()))
        .collect()
}

/// A pad number with EasyEDA's `@n` de-duplication suffix removed: pads that
/// share a number are stored `GND@0`, `GND@1`, and all mean pad `GND`.
fn pad_number(raw: &str) -> String {
    let raw = raw.trim();
    if let Some((base, suffix)) = raw.rsplit_once('@') {
        if !base.is_empty() && !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()) {
            return base.to_string();
        }
    }
    raw.to_string()
}

fn board_xy(x: &str, y: &str) -> Result<Option<(f64, f64)>, ExtractError> {
    let (Some(x), Some(y)) = (coord(x)?, coord(y)?) else {
        return Ok(None);
    };
    Ok(Some((
        (x - STD_ORIGIN.0) * STD_UNIT_MM,
        (y - STD_ORIGIN.1) * STD_UNIT_MM,
    )))
}

/// One `LIB` record: a placed part with its package, designator and pads.
fn read_lib(ctx: &mut Ctx<'_>, record: &str, index: usize) -> Result<Component, ExtractError> {
    let mut sections = record.split("#@$");
    let head_text = sections.next().unwrap_or("");
    let head: Vec<&str> = head_text.split('~').collect();
    let attrs = parse_attrs(arg(&head, 3));
    let rotation = coord(arg(&head, 4))?.unwrap_or(0.0);
    let position = board_xy(arg(&head, 1), arg(&head, 2))?.map(|(x, y)| (x, y, rotation));

    let mut designator = String::new();
    let mut name_text = String::new();
    let mut pins = Vec::new();
    let mut layer = "";
    for section in sections {
        let fields: Vec<&str> = section.split('~').collect();
        let tag = arg(&fields, 0);
        let args = fields.get(1..).unwrap_or(&[]);
        match tag {
            "PAD" => {
                let pad_layer = arg(args, 5);
                if layer.is_empty() && matches!(pad_layer.trim(), "1" | "2") {
                    layer = pad_layer.trim();
                }
                let net = ctx.nets.id(arg(args, 6));
                pins.push(Pin {
                    number: pad_number(arg(args, 7)),
                    net,
                    position: board_xy(arg(args, 1), arg(args, 2))?,
                    ..Default::default()
                });
            }
            "TEXT" => {
                let text = arg(args, 9).trim();
                match arg(args, 0) {
                    "P" => designator = text.to_string(),
                    "N" => name_text = text.to_string(),
                    _ => ctx.stats.past("text labels"),
                }
            }
            other => ctx.primitive(other, args)?,
        }
    }

    let mut properties: Vec<(String, String)> = attrs
        .iter()
        .filter(|(k, v)| !SKIPPED_ATTRS.contains(&k.as_str()) && !v.is_empty())
        .take(MAX_ATTRS)
        .map(|(k, v)| (clip(k), v.chars().take(MAX_ATTR_VALUE).collect::<String>()))
        .collect();

    // The footprint's N text is the part's value only when the author set the
    // name alias to show it (`Value(F)`); otherwise it is just a label.
    let alias = attrs.get("nameAlias").map_or("", String::as_str);
    let value = attrs
        .get("Value")
        .filter(|v| !v.is_empty())
        .cloned()
        .or_else(|| {
            (alias.starts_with("Value") && !name_text.is_empty()).then(|| name_text.clone())
        })
        .unwrap_or_default();
    if value.is_empty() {
        properties.push((
            VALUE_UNRESOLVED_KEY.to_string(),
            VALUE_UNRESOLVED_REASON.to_string(),
        ));
        if !name_text.is_empty() {
            properties.push((
                "name".to_string(),
                name_text.chars().take(MAX_ATTR_VALUE).collect(),
            ));
        }
    }

    let reference = if designator.is_empty() {
        let generated = format!("UNK{index}");
        properties.push((
            REFERENCE_UNRESOLVED_KEY.to_string(),
            format!("no designator text in the footprint; assigned stable synthetic identity '{generated}'"),
        ));
        generated
    } else {
        designator
    };

    let package = attrs.get("package").cloned().unwrap_or_default();
    Ok(Component {
        reference,
        value,
        lib_id: if package.is_empty() {
            String::new()
        } else {
            format!("easyeda:{package}")
        },
        footprint: package,
        position,
        layer: if layer == "2" { "B.Cu" } else { "F.Cu" }.to_string(),
        properties,
        dnp: false,
        pins,
    })
}

/// The `shape` list of a PCB document, or the refusal naming what is there instead.
fn shape_list(doc: &serde_json::Map<String, Value>) -> Result<&Vec<Value>, ExtractError> {
    match doc.get("shape") {
        Some(Value::Array(a)) => Ok(a),
        Some(other) => Err(ExtractError::Easyeda(format!(
            "this EasyEDA PCB's 'shape' is {} rather than a list of records, so the \
             board cannot be read. Re-export it from EasyEDA",
            kind_of(other)
        ))),
        None => Err(ExtractError::Easyeda(
            "this EasyEDA PCB has no 'shape' list, so it holds no board. If it is an \
             empty document, add the parts in EasyEDA and export again"
                .to_string(),
        )),
    }
}

/// Layer id -> name from the document's `layers` list (`"1~TopLayer~..."`).
fn layer_table(doc: &serde_json::Map<String, Value>) -> HashMap<String, String> {
    let mut layer_names = HashMap::new();
    if let Some(Value::Array(layers)) = doc.get("layers") {
        for l in layers.iter().filter_map(Value::as_str) {
            let mut it = l.split('~');
            if let (Some(id), Some(name)) = (it.next(), it.next()) {
                layer_names.insert(id.to_string(), name.to_string());
            }
        }
    }
    layer_names
}

/// Read an EasyEDA Standard document.
pub(super) fn extract(text: &str) -> Result<EasyedaExtraction, ExtractError> {
    let root: Value = serde_json::from_str(text).map_err(|e| json_error(&e))?;
    let Value::Object(root_map) = &root else {
        return Err(ExtractError::Easyeda(format!(
            "this is JSON, but {} rather than an EasyEDA document (an object with a \
             'head' and a 'shape' list)",
            kind_of(&root)
        )));
    };

    // The "EasyEDA source" export wraps the document in `dataStr`, as an object
    // or as a string holding JSON; a bare PCB document has `head` at the top.
    let inner;
    let doc_value: &Value = match root_map.get("dataStr") {
        Some(Value::String(s)) => {
            inner = serde_json::from_str::<Value>(s).map_err(|e| json_error(&e))?;
            &inner
        }
        Some(v @ Value::Object(_)) => v,
        _ => &root,
    };
    let Value::Object(doc) = doc_value else {
        return Err(ExtractError::Easyeda(
            "this EasyEDA file's 'dataStr' is not a document object".to_string(),
        ));
    };

    let head = doc.get("head").or_else(|| root_map.get("head"));
    let field = |name: &str| -> String {
        head.and_then(|h| h.get(name))
            .or_else(|| root_map.get(name))
            .map_or_else(String::new, |v| match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => String::new(),
            })
    };
    let doc_type = field("docType");
    if doc_type != "3" {
        return Err(ExtractError::Easyeda(if doc_type.is_empty() {
            "this JSON has no 'docType', so it is not an EasyEDA design file".to_string()
        } else {
            format!(
                "this EasyEDA file is a {}, not a PCB board (docType {}). Open the board \
                 in the PCB editor and export it from there (File > Export > EasyEDA \
                 source), then drop that file",
                doc_type_name(&doc_type),
                clip(&doc_type)
            )
        }));
    }

    let shapes = shape_list(doc)?;
    let layer_names = layer_table(doc);

    let mut ctx = Ctx {
        nets: NetTable::default(),
        stats: EasyedaStats::new(Edition::Standard, "a Standard JSON PCB document"),
        layer_names: &layer_names,
    };
    ctx.stats.editor_version = field("editorVersion");
    let name = root_map
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map_or_else(String::new, clip);

    let mut components = Vec::new();
    for (index, shape) in shapes.iter().enumerate() {
        let Some(record) = shape.as_str() else {
            ctx.stats.unsupported("shape entry that is not text");
            continue;
        };
        let tag = record.split('~').next().unwrap_or("");
        if tag == "LIB" {
            components.push(read_lib(&mut ctx, record, index)?);
        } else if tag == "PAD" {
            let fields: Vec<&str> = record.split('~').collect();
            ctx.nets.id(arg(&fields, 7));
            ctx.stats.copper.free_pads += 1;
        } else {
            let fields: Vec<&str> = record.split('~').collect();
            ctx.primitive(tag, fields.get(1..).unwrap_or(&[]))?;
        }
    }

    if components.is_empty() {
        ctx.stats.caveats.push(
            "This EasyEDA PCB places no parts (it has no LIB records), so there is nothing to \
             check beyond its copper."
                .to_string(),
        );
    }
    let Ctx {
        nets, mut stats, ..
    } = ctx;
    let board = finish(name, nets, components, &mut stats, false);
    Ok(EasyedaExtraction { board, stats })
}
