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
    crate::reject_merge_conflict(text)?;
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
    reject_non_finite_geometry(root)?;

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

    // Names invented for undeclared nets are a guess about the user's board, so
    // say how many rather than letting `Net-(7)` appear in a report unexplained.
    if table.synthesized_from_pads > 0 {
        eprintln!(
            "hauksbee: {} net(s) are referenced by pads but never declared in this \
             board; they are reported as Net-(<id>) because the file carries no name \
             for them. Re-save the board from KiCad to write the net table.",
            table.synthesized_from_pads
        );
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
    /// How many nets were named by [`NetTable::ensure_id`] because a pad
    /// referenced an id the file never declared. Reported to the user, since a
    /// synthesized name is a guess about their board.
    synthesized_from_pads: usize,
}

impl NetTable {
    /// Make sure `id` exists in the table, naming it if it does not.
    ///
    /// A `.kicad_pcb` normally declares every net at top level before any pad
    /// references it, but a hand-written or partially-generated board can carry
    /// `(net 7)` on a pad with no `(net 7 "…")` anywhere. Before this, those
    /// pads kept their net ids while the net TABLE stayed empty, so the board
    /// reported zero nets while its pads were wired to 34 of them: every net-keyed
    /// check then had nothing to look at and passed vacuously. Recording the id
    /// under a synthetic name keeps the two views consistent; the count of
    /// synthesized nets is surfaced by the caller so the guess is never silent.
    fn ensure_id(&mut self, id: i64) -> i64 {
        if !self.by_id.contains_key(&id) {
            // KiCad's own name for a net it cannot label; the parenthesised id
            // makes it obvious in a report that the name came from us, not the
            // file.
            self.declare(id, format!("Net-({id})"));
            self.synthesized_from_pads += 1;
        }
        id
    }

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

/// Geometry lists whose numeric arguments are coordinates or dimensions. Every
/// distance the clearance check computes comes from one of these.
const GEOMETRY_LISTS: &[&str] = &[
    "at",
    "start",
    "end",
    "mid",
    "center",
    "xy",
    "size",
    "width",
    "thickness",
    "drill",
    "offset",
    "rect_delta",
    "radius",
];

/// Refuse a board carrying a coordinate that is not a finite number.
///
/// This is the one shape of corruption that produces a CONFIDENT WRONG ANSWER
/// rather than a parse failure. Every distance comparison against NaN is false,
/// so a pad at `(at nan nan)` is closer to nothing and further from nothing:
/// the clearance check walks the whole board and reports "no shorts or
/// clearance violations", a green verdict on geometry that does not exist. The
/// same holds for `inf` and for a decimal exponent that overflows to infinity
/// (`1e400`), which is how a mangled unit conversion usually arrives.
///
/// KiCad cannot write such a file; every instance is a hand edit, a broken
/// generator or a corrupted transfer, so refusing costs no real board. That is
/// measured, not assumed: zero files in a 1139-board corpus of real KiCad
/// layouts (KiCad 4 through 10) carry a non-finite coordinate.
fn reject_non_finite_geometry(root: &List) -> Result<(), ExtractError> {
    let mut stack: Vec<&List> = vec![root];
    while let Some(list) = stack.pop() {
        if let Some(name) = list.name() {
            if GEOMETRY_LISTS.contains(&name) {
                for i in 0.. {
                    let Some(token) = list.arg(i) else { break };
                    // Quoted text is never a coordinate: a net legitimately
                    // named "NaN" must not take the board down.
                    if token.is_string() {
                        continue;
                    }
                    // Not `as_f64`: that already filters non-finite values to
                    // `None`, which is exactly the silent-default behaviour
                    // being caught here. Parse the raw token.
                    match token.value().parse::<f64>() {
                        Ok(v) if !v.is_finite() => {
                            return Err(ExtractError::Corrupt(format!(
                                "board geometry is corrupt: '({name} …)' carries the \
                                 non-numeric coordinate '{}'. Distances cannot be \
                                 compared against it, so a clearance check would \
                                 report a meaningless pass. Re-save the board from \
                                 KiCad, or fix the '{name}' entry by hand",
                                token.value()
                            )));
                        }
                        _ => {}
                    }
                }
            }
        }
        stack.extend(list.lists());
    }
    Ok(())
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
            return Some(table.ensure_id(id));
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
