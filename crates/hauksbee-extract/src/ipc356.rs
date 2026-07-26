//! Extraction from IPC-D-356/356A netlists; the universal fallback. Every
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
    let mut truncated = 0usize;

    for line in text.lines() {
        if line.len() < 3 {
            continue;
        }
        if line.starts_with('P') && line.contains("UNITS") {
            // IPC-356A units matrix: CUST 0 / inch = 1/10000 inch per LSB
            // (the imperial default), CUST 1 / MM = micrometre per LSB, CUST 2 =
            // 0.1 micrometre per LSB. A CUST 2 export read at the imperial scale
            // would place every pad ~25.4x off.
            if line.contains("CUST 2") {
                scale = 0.0001;
            } else if line.contains("CUST 1") || line.contains("MM") {
                scale = 0.001;
            }
            continue;
        }
        if !(line.starts_with("317") || line.starts_with("327") || line.starts_with("367")) {
            continue;
        }
        saw_record = true;
        // A 317/327/367 marks a real test record whose fixed columns run past
        // column 32 (net, ref-des, pin, then coordinates). A line too short for
        // them is truncated, NOT a legitimate blank-ref via record, which keeps
        // full width with spaces. Count it and skip, so a truncated export is
        // surfaced rather than silently discarded down the via path below (which
        // would otherwise be indistinguishable from an intentional blank ref).
        if line.len() < 32 {
            truncated += 1;
            continue;
        }
        // Columns (1-based, per IPC-D-356): 4-17 net name, 21-26 ref des,
        // 27 '-', 28-31 pin number. Coordinates follow after column 32.
        let get =
            |a: usize, b: usize| -> &str { line.get(a..b.min(line.len())).unwrap_or("").trim() };
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

    if truncated > 0 {
        eprintln!(
            "hauksbee: skipped {truncated} truncated IPC-D-356 test record(s) \
             (lines too short to contain their columns); the netlist may be incomplete"
        );
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

/// `X+0123450Y-0067890`, located in the coordinate region past the fixed
/// net/ref/pin/access columns. The search MUST start past column 32: an 'X' in
/// an X-bearing net name (RXD/TXD) or reference designator (X1, a crystal)
/// sits earlier in the record, and scanning the whole line would latch onto it
/// and drop (or corrupt) the real coordinate.
fn parse_xy(line: &str) -> Option<(f64, f64)> {
    let coord = line.get(31..)?;
    let xi = coord.find('X')?;
    let rest = &coord[xi..];
    let yi = rest.find('Y')?;
    let x: f64 = rest[1..yi].trim().parse().ok()?;
    // Trim the leading whitespace of the Y span too: IPC-356A allows a blank
    // column for a positive sign (`Y 029450`), and without this trim the span
    // scan below would stop on that leading space and read an empty number.
    let after_y = rest[yi + 1..].trim_start();
    let end = after_y
        .find(|c: char| !(c.is_ascii_digit() || c == '+' || c == '-'))
        .unwrap_or(after_y.len());
    let y: f64 = after_y[..end].trim().parse().ok()?;
    Some((x, y))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a full-width IPC-D-356 `317` test record with fields at their fixed
    /// columns: net (4-17), ref-des (21-26), pin (28-31). Always >= 32 chars.
    fn record(net: &str, refdes: &str, pin: &str) -> String {
        let mut s = vec![b' '; 40];
        s[0..3].copy_from_slice(b"317");
        for (i, b) in net.bytes().take(14).enumerate() {
            s[3 + i] = b;
        }
        for (i, b) in refdes.bytes().take(6).enumerate() {
            s[20 + i] = b;
        }
        s[26] = b'-';
        for (i, b) in pin.bytes().take(4).enumerate() {
            s[27 + i] = b;
        }
        String::from_utf8(s).unwrap()
    }

    /// As [`record`], but with a coordinate field placed in the coordinate
    /// region (column 33 onward), so `parse_xy` has something to read.
    fn record_xy(net: &str, refdes: &str, pin: &str, coord: &str) -> String {
        let base = record(net, refdes, pin);
        let mut s = base.as_bytes()[..32].to_vec();
        s.extend_from_slice(coord.as_bytes());
        String::from_utf8(s).unwrap()
    }

    #[test]
    fn x_bearing_net_or_ref_does_not_latch_the_coordinate_scan() {
        // R12: an 'X' in the net name (RXD) or reference designator (X1) sits in
        // the fixed columns BEFORE the coordinate field. The scan must start past
        // column 32, else find('X') latches onto it and the coordinate is lost.
        for (net, refd) in [("RXD", "U1"), ("GND", "X1")] {
            let rec = record_xy(net, refd, "1", "X+0019000Y+0029450");
            let board = extract(&format!("{rec}\n")).unwrap();
            let pos = board.components[0].pins[0].position;
            let (x, y) = pos.expect("coordinate must survive an X-bearing field");
            assert!((x - 48.26).abs() < 1e-3, "x={x}");
            assert!((y - 74.803).abs() < 1e-3, "y={y}");
        }
    }

    #[test]
    fn blank_positive_sign_coordinate_parses() {
        // IPC-356A allows a blank column for a positive sign (`X 0019000`). The
        // Y span must be trimmed like the X span, else the leading space stops
        // the digit scan at an empty number and both coordinates are dropped.
        let rec = record_xy("GND", "R1", "1", "X 0019000Y 0029450");
        let board = extract(&format!("{rec}\n")).unwrap();
        let (x, y) = board.components[0].pins[0]
            .position
            .expect("blank-sign coordinate must parse");
        assert!((x - 48.26).abs() < 1e-3 && (y - 74.803).abs() < 1e-3, "{x},{y}");
    }

    #[test]
    fn cust2_units_scale_metric_tenth_micron() {
        // R12: a CUST 2 metric export (0.1 µm/LSB) must not be read at the
        // imperial 1/10000-inch scale (~25.4x too large).
        let rec = record_xy("GND", "R1", "1", "X+0019000Y+0029450");
        let board = extract(&format!("P  UNITS CUST 2\n{rec}\n")).unwrap();
        let (x, _) = board.components[0].pins[0].position.unwrap();
        assert!((x - 1.9).abs() < 1e-6, "19000 * 0.0001 mm = 1.9, got {x}");
    }

    #[test]
    fn truncated_record_is_skipped_full_record_parses() {
        // Bug-hunt #7: a truncated 317 line (too short to hold its ref-des
        // columns) must be dropped as truncated, NOT silently treated as a
        // blank-ref via record, while a full record still parses.
        let full = record("GND", "R1", "1");
        let truncated = "317GND"; // 6 chars: a truncated data record
        let text = format!("{full}\n{truncated}\n");
        let board = extract(&text).unwrap();
        assert_eq!(board.components.len(), 1, "only the full record is a part");
        assert_eq!(board.components[0].reference, "R1");
    }
}
