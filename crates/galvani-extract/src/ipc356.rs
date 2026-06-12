//! Extraction from IPC-D-356/356A netlists — the universal fallback. Every
//! serious EDA tool (Altium, Allegro, PADS, Eagle, KiCad) exports this with
//! fab outputs, so any board whose source format we don't read natively can
//! still be ingested via its fab files.
//!
//! Fixed-column record format. We read the `317`/`327` (through-hole / SMD)
//! test records: net name in columns 4-17, reference and pin in the `R...`
//! `-...` fields, X/Y location in 1/10000 inch (or mm when a `P  UNITS`
//! parameter says so).

use crate::{Component, ExtractError, ExtractedBoard, Net, Pin};
use std::collections::HashMap;

pub fn extract(text: &str) -> Result<ExtractedBoard, ExtractError> {
    let mut scale = 0.00254; // 1/10000 inch -> mm
    let mut nets: Vec<Net> = Vec::new();
    let mut net_ids: HashMap<String, i64> = HashMap::new();
    let mut comps: HashMap<String, Vec<Pin>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut saw_record = false;

    for line in text.lines() {
        if line.len() < 3 {
            continue;
        }
        if line.starts_with('P') && line.contains("UNITS") {
            if line.contains("CUST 1") || line.contains("MM") {
                // CUST 1: metric, micrometres per LSB.
                scale = 0.001;
            }
            continue;
        }
        if !(line.starts_with("317") || line.starts_with("327") || line.starts_with("367")) {
            continue;
        }
        saw_record = true;
        // Columns (1-based, per IPC-D-356): 4-17 net name, 21-26 ref des,
        // 27 '-', 28-31 pin number. Coordinates follow after column 32.
        let get = |a: usize, b: usize| -> &str {
            line.get(a..b.min(line.len())).unwrap_or("").trim()
        };
        let net_name = get(3, 17).to_string();
        let reference = get(20, 26).to_string();
        let pin_number = get(27, 31).to_string();
        if reference.is_empty() || reference == "VIA" {
            continue; // via / bare-copper access record
        }
        let net = if net_name.is_empty() || net_name == "N/C" {
            None
        } else {
            let next = net_ids.len() as i64 + 1;
            let id = *net_ids.entry(net_name.clone()).or_insert(next);
            if id == next {
                nets.push(Net { id, name: net_name });
            }
            Some(id)
        };
        let position = parse_xy(line).map(|(x, y)| (x * scale, y * scale));
        if !comps.contains_key(&reference) {
            order.push(reference.clone());
        }
        comps.entry(reference).or_default().push(Pin {
            number: pin_number,
            net,
            function: String::new(),
            kind: String::new(),
            position,
        });
    }

    if !saw_record {
        return Err(ExtractError::WrongRoot {
            expected: "IPC-D-356 test records",
            found: None,
        });
    }

    let components = order
        .into_iter()
        .map(|reference| Component {
            pins: comps.remove(&reference).unwrap_or_default(),
            reference,
            value: String::new(),
            lib_id: String::new(),
            footprint: String::new(),
            position: None,
            layer: String::new(),
            properties: Vec::new(),
            dnp: false,
        })
        .collect();

    Ok(ExtractedBoard {
        name: String::new(),
        nets,
        components,
    })
}

/// `X+0123450Y-0067890` anywhere after the access fields.
fn parse_xy(line: &str) -> Option<(f64, f64)> {
    let xi = line.find('X')?;
    let rest = &line[xi..];
    let yi = rest.find('Y')?;
    let x: f64 = rest[1..yi].trim().parse().ok()?;
    let after_y = &rest[yi + 1..];
    let end = after_y
        .find(|c: char| !(c.is_ascii_digit() || c == '+' || c == '-'))
        .unwrap_or(after_y.len());
    let y: f64 = after_y[..end].trim().parse().ok()?;
    Some((x, y))
}
