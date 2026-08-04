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
        let Some(first) = n.arg(0) else { continue };
        if first.is_string() {
            // v10 name-only declaration: `(net "name")`. An empty name means
            // "no net" and must not be interned (see `net_ref`).
            let name = first.value();
            if !name.is_empty() {
                table.id_of(&name);
            }
        } else if let Some(id) = first.as_i64() {
            // Net 0 is the "no net" sentinel (empty name); never intern it.
            if id != 0 {
                table.declare(id, n.arg_value(1).unwrap_or_default());
            }
        } else if let Some(name) = n.arg_value(1).filter(|s| !s.is_empty()) {
            // The id slot is present but not a valid i64 (overflow, garbage).
            // The declared name is still authoritative: never adopt the raw
            // digit string as the net's name, that would break exact-name
            // matches downstream (ground/power detection on "GND"/"VSS").
            table.id_of(&name);
        }
    }

    let mut components: Vec<Component> = Vec::new();
    // KiCad 5 wrote `(module ...)`, 6+ writes `(footprint ...)`.
    for fp in root.find_all("footprint").chain(root.find_all("module")) {
        // A footprint with no pads at all is board artwork, not a component:
        // silkscreen logos, drawn graphics, mechanical outlines. Treating them
        // as parts let an Olimex "Logo-..." silkscreen bind as an inductor (the
        // L prefix) and raise placeholder-value warnings on decoration, and it
        // padded bind-rate denominators with things no model could ever bind.
        if fp.find_all("pad").next().is_none() {
            continue;
        }
        let c = extract_footprint(fp, &mut table);
        // Two footprints sharing one reference designator are one electrical
        // part with several physical instances (a testpoint placed on both
        // board sides is the common case: Watchy's TP4/TP5). Merge the later
        // instance's pads into the first so every downstream count (bind rows,
        // num_components, resolve-rate denominators) sees one part per refdes.
        // A part is DNP only when every one of its instances is.
        let prev = (!c.reference.is_empty())
            .then(|| components.iter_mut().find(|p| p.reference == c.reference))
            .flatten();
        if let Some(prev) = prev {
            prev.pins.extend(c.pins);
            prev.dnp = prev.dnp && c.dnp;
            continue;
        }
        components.push(c);
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
        // File-syntax escapes ({slash}, subscript braces) end here: the table
        // keys and the emitted Net names are the real KiCad display names.
        let name = crate::netname::unescape_net_name(&name);
        self.by_name.insert(name.clone(), id);
        self.by_id.insert(id, name);
        self.next_synthetic = self.next_synthetic.max(id + 1);
    }

    fn id_of(&mut self, name: &str) -> i64 {
        let name = crate::netname::unescape_net_name(name);
        if let Some(&id) = self.by_name.get(&name) {
            return id;
        }
        let id = self.next_synthetic.max(1);
        self.next_synthetic = id + 1;
        self.declare(id, name);
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
    if let Some(first) = net.arg(0).filter(|t| !t.is_string()) {
        // ≤v9 numeric id slot. An unparseable id (overflow, garbage) must not
        // leak its digit string in as a name, resolve through the declared
        // name in the next slot instead.
        if let Some(id) = first.as_i64() {
            // KiCad reserves net 0 (always name "") as the "no net": every
            // unconnected / mounting / free pad carries `(net 0 "")`. Interning
            // it would fuse all of them onto one shared node; the same hazard
            // the v10 empty-name guard below prevents.
            if id == 0 {
                return None;
            }
            return Some(id);
        }
        let name = net.arg_value(1).filter(|n| !n.is_empty())?;
        return Some(table.id_of(&name));
    }
    // v10 name-only reference. An empty name means "no net": interning ""
    // would hand every unconnected pad in the file the same synthetic id,
    // fusing unrelated pads onto one node. No net clause and `(net "")`
    // must land in the same place, `None`.
    let name = net.arg_value(0).filter(|n| !n.is_empty())?;
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
