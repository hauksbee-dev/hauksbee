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
            for prop in comp.find_all("property") {
                if let (Some(k), Some(v)) = (prop.find_value("name"), prop.find_value("value")) {
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
                // KiCad netlists encode DNP as a property field; not threaded
                // here yet (the corpus's DNP-sensitive boards parse from the PCB
                // / schematic paths, which do set this).
                dnp: false,
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
    if let Some(net_list) = root.find("nets") {
        for net in net_list.find_all("net") {
            let id = net.find_i64("code").unwrap_or(-1);
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
