//! Extraction from a `.kicad_pcb` layout into the canonical [`ExtractedBoard`].
//!
//! Handles the s-expression board format across a wide version range, from
//! KiCad 5 (20171130, bare atoms) through KiCad 10 (20250907). The hard part is
//! that the net representation drifted: KiCad =<9 declares `(net N "name")` with
//! numeric ids at top level, while KiCad 10 dropped the ids and nets survive
//! only as `(net "name")` references on pads. This module normalises both into
//! one [`Net`] table (synthesising ids from names where absent) and walks the
//! footprints to attach each pad to its [`Pin`], producing the same board form
//! the schematic and netlist readers do. `extract_from_doc` lets a caller that
//! already holds the parsed CST skip a re-parse.

use crate::{Component, ExtractError, ExtractedBoard, Net, Pin};
use forge_sexpr::{Document, List};

pub fn extract(text: &str) -> Result<ExtractedBoard, ExtractError> {
    let doc = forge_sexpr::parse(text)?;
    extract_from_doc(&doc)
}

/// Extract from an already-parsed `.kicad_pcb` document, avoiding a re-parse
/// when the caller already holds the CST.
pub fn extract_from_doc(doc: &Document) -> Result<ExtractedBoard, ExtractError> {
    let root = doc.root().ok_or(ExtractError::WrongRoot {
        expected: "kicad_pcb",
        found: None,
    })?;
    if root.name() != Some("kicad_pcb") {
        return Err(ExtractError::WrongRoot {
            expected: "kicad_pcb",
            found: root.name().map(str::to_string),
        });
    }

    // KiCad ≤9 declares `(net N "name")` at top level. KiCad 10 (20260206+)
    // dropped numeric ids: nets exist only as `(net "name")` references on
    // pads, so we synthesize a table from the names we encounter.
    let mut table = NetTable::default();
    for n in root.find_all("net") {
        match (n.arg_i64(0), n.arg_value(0), n.arg_value(1)) {
            (Some(id), _, name) => table.declare(id, name.unwrap_or_default()),
            (None, Some(name), _) => {
                table.id_of(&name);
            }
            _ => {}
        }
    }

    let mut components = Vec::new();
    // KiCad 5 wrote `(module ...)`, 6+ writes `(footprint ...)`.
    for fp in root.find_all("footprint").chain(root.find_all("module")) {
        components.push(extract_footprint(fp, &mut table));
    }

    let mut nets = table.into_nets();
    nets.sort_by_key(|n| n.id);

    Ok(ExtractedBoard {
        name: String::new(),
        nets,
        components,
    })
}

/// Net identity across format generations: declared ids where the file has
/// them, synthesized ids (negative of insertion order is avoided; we use
/// max_id+1 onward) for name-only references.
#[derive(Default)]
struct NetTable {
    by_id: std::collections::BTreeMap<i64, String>,
    by_name: std::collections::HashMap<String, i64>,
    next_synthetic: i64,
}

impl NetTable {
    fn declare(&mut self, id: i64, name: String) {
        self.by_name.insert(name.clone(), id);
        self.by_id.insert(id, name);
        self.next_synthetic = self.next_synthetic.max(id + 1);
    }

    fn id_of(&mut self, name: &str) -> i64 {
        if let Some(&id) = self.by_name.get(name) {
            return id;
        }
        let id = self.next_synthetic.max(1);
        self.next_synthetic = id + 1;
        self.declare(id, name.to_string());
        id
    }

    fn into_nets(self) -> Vec<Net> {
        self.by_id
            .into_iter()
            .map(|(id, name)| Net { id, name })
            .collect()
    }
}

/// A pad/segment net reference: `(net 4 "GND")` in ≤v9, `(net "GND")` in v10.
fn net_ref(list: &List, table: &mut NetTable) -> Option<i64> {
    let net = list.find("net")?;
    if let Some(id) = net
        .arg(0)
        .filter(|t| !t.is_string())
        .and_then(|t| t.as_i64())
    {
        return Some(id);
    }
    let name = net.arg_value(0)?;
    Some(table.id_of(&name))
}

fn extract_footprint(fp: &List, table: &mut NetTable) -> Component {
    let lib_id = fp.arg_value(0).unwrap_or_default();
    let (fx, fy, frot) = at_of(fp);
    let layer = fp.find_value("layer").unwrap_or_default();

    let mut reference = String::new();
    let mut value = String::new();
    let mut properties = Vec::new();
    // KiCad 7+: (property "Reference" "R1" ...)
    for prop in fp.find_all("property") {
        let (Some(k), Some(v)) = (prop.arg_value(0), prop.arg_value(1)) else {
            continue;
        };
        match k.as_str() {
            "Reference" => reference = v,
            "Value" => value = v,
            _ => properties.push((k, v)),
        }
    }
    // KiCad 5/6: (fp_text reference "R1" ...)
    if reference.is_empty() || value.is_empty() {
        for t in fp.find_all("fp_text") {
            match (t.arg_value(0).as_deref(), t.arg_value(1)) {
                (Some("reference"), Some(v)) if reference.is_empty() => reference = v,
                (Some("value"), Some(v)) if value.is_empty() => value = v,
                _ => {}
            }
        }
    }

    let rot_rad = frot.to_radians();
    let (sin, cos) = rot_rad.sin_cos();
    let mut pins = Vec::new();
    for pad in fp.find_all("pad") {
        let number = pad.arg_value(0).unwrap_or_default();
        let net = net_ref(pad, table);
        let (px, py, _) = at_of(pad);
        // Pad offsets are in the footprint frame; KiCad's y axis points
        // down, and footprint rotation is counter-clockwise, so the world
        // offset is (x cos + y sin, -x sin + y cos).
        let abs = (fx + px * cos + py * sin, fy - px * sin + py * cos);
        pins.push(Pin {
            number,
            net,
            function: pad.find_value("pinfunction").unwrap_or_default(),
            kind: pad.find_value("pintype").unwrap_or_default(),
            position: Some(abs),
        });
    }

    // Do-Not-Populate: KiCad writes a `dnp` flag inside the footprint's
    // `(attr ...)` list (e.g. `(attr smd exclude_from_bom dnp)`). A DNP footprint
    // is on the layout but not assembled, so it is electrically absent.
    let dnp = fp.find("attr").map(|a| a.has_flag("dnp")).unwrap_or(false);

    Component {
        reference,
        value,
        lib_id: lib_id.clone(),
        footprint: lib_id,
        position: Some((fx, fy, frot)),
        layer,
        properties,
        dnp,
        pins,
    }
}

/// `(at x y [rot])` with all parts optional in degenerate files.
fn at_of(list: &List) -> (f64, f64, f64) {
    match list.find("at") {
        Some(at) => (
            at.arg_f64(0).unwrap_or(0.0),
            at.arg_f64(1).unwrap_or(0.0),
            at.arg_f64(2).unwrap_or(0.0),
        ),
        None => (0.0, 0.0, 0.0),
    }
}
