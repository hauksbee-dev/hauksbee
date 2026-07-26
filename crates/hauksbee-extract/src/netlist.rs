//! Extraction from a KiCad netlist export into the canonical [`ExtractedBoard`].
//!
//! The input is the s-expression `(export (components (comp ...)) (nets (net
//! (node ...))))` that KiCad writes from the schematic. This is the most direct
//! ingestion path: unlike the gerber reader it does not reconstruct
//! connectivity, it reads the nets the schematic already declares. Each `comp`
//! becomes a [`Component`] (refdes plus its `libsource` lib/part id) and each
//! `net` becomes a [`Net`] whose `node` entries name the (refdes, pin) [`Pin`]s
//! tied together. The result is the same neutral board form every other
//! front-end lowers to, so downstream binding and simulation never see which
//! reader produced it.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-extract/netlist.md.

use crate::{Component, ExtractError, ExtractedBoard, Net, Pin};
use forge_sexpr::Document;

pub fn extract(text: &str) -> Result<ExtractedBoard, ExtractError> {
    let doc = forge_sexpr::parse(text)?;
    extract_from_doc(&doc)
}

/// Extract from an already-parsed netlist `(export ...)` document.
pub fn extract_from_doc(doc: &Document) -> Result<ExtractedBoard, ExtractError> {
    let root = doc.root().ok_or(ExtractError::WrongRoot {
        expected: "export",
        found: None,
    })?;
    if root.name() != Some("export") {
        return Err(ExtractError::WrongRoot {
            expected: "export",
            found: root.name().map(str::to_string),
        });
    }

    let name = root
        .find("design")
        .and_then(|d| d.find_value("source"))
        .unwrap_or_default();

    let mut components = Vec::new();
    if let Some(comps) = root.find("components") {
        for comp in comps.find_all("comp") {
            let lib_id = comp
                .find("libsource")
                .map(|ls| {
                    let lib = ls.find_value("lib").unwrap_or_default();
                    let part = ls.find_value("part").unwrap_or_default();
                    if lib.is_empty() {
                        part
                    } else {
                        format!("{lib}:{part}")
                    }
                })
                .unwrap_or_default();
            let mut properties = Vec::new();
            let mut dnp = false;
            for prop in comp.find_all("property") {
                let key = prop.find_value("name");
                let val = prop.find_value("value");
                if key
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case("dnp") || n.eq_ignore_ascii_case("exclude_from_board"))
                {
                    // KiCad emits this property (usually value-less) only for DNP
                    // parts. Presence => DNP, unless an explicit falsey value overrides.
                    dnp = !matches!(val.as_deref(), Some("no") | Some("false") | Some("0"));
                }
                if let (Some(k), Some(v)) = (key, val) {
                    properties.push((k, v));
                }
            }
            components.push(Component {
                reference: comp.find_value("ref").unwrap_or_default(),
                value: comp.find_value("value").unwrap_or_default(),
                lib_id,
                footprint: comp.find_value("footprint").unwrap_or_default(),
                position: None,
                layer: String::new(),
                properties,
                // KiCad netlists encode DNP as a (usually value-less) property
                // child; thread it through so DNP-aware analysis behaves the same
                // regardless of ingestion path (PCB / schematic already do).
                dnp,
                pins: Vec::new(),
            });
        }
    }

    let index: std::collections::HashMap<String, usize> = components
        .iter()
        .enumerate()
        .map(|(i, c)| (c.reference.clone(), i))
        .collect();

    let mut nets = Vec::new();
    // Fresh unique ids for nets whose `(code ...)` is missing or unparseable.
    // Sharing one fixed sentinel (-1) fused every such net, and every pin on
    // them, into a single electrically-shorted net. KiCad codes are
    // non-negative, so a decreasing negative counter never collides with a real
    // code or with another synthetic id.
    let mut synthetic_net_id: i64 = -1;
    if let Some(net_list) = root.find("nets") {
        for net in net_list.find_all("net") {
            let id = match net.find_i64("code") {
                Some(code) => code,
                None => {
                    let s = synthetic_net_id;
                    synthetic_net_id -= 1;
                    s
                }
            };
            nets.push(Net {
                id,
                name: net.find_value("name").unwrap_or_default(),
            });
            for node in net.find_all("node") {
                let Some(reference) = node.find_value("ref") else {
                    continue;
                };
                let Some(&ci) = index.get(&reference) else {
                    continue;
                };
                components[ci].pins.push(Pin {
                    number: node.find_value("pin").unwrap_or_default(),
                    net: Some(id),
                    function: node.find_value("pinfunction").unwrap_or_default(),
                    kind: node.find_value("pintype").unwrap_or_default(),
                    position: None,
                });
            }
        }
    }
    nets.sort_by_key(|n| n.id);

    Ok(ExtractedBoard {
        name,
        nets,
        components,
    })
}
