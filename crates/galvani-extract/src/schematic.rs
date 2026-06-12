//! Extraction from KiCad s-expression schematics (`.kicad_sch`, KiCad 6
//! through 10).
//!
//! A layout (`.kicad_pcb`) hands us connectivity for free: every pad already
//! carries its net. A schematic does not. The net list has to be *derived*
//! geometrically the same way eeschema derives it:
//!
//! 1. Every symbol pin has a connection point in absolute schematic
//!    coordinates (the pin's lib-symbol `(at)` transformed by the symbol
//!    instance's placement). Wires, junctions, labels, no-connects and sheet
//!    pins all live at absolute coordinates too.
//! 2. Things that share a coordinate are electrically joined: a wire endpoint
//!    touching a pin, two wires crossing at a junction, a label sitting on a
//!    wire. We union-find every such coincidence into connected components.
//! 3. Named connections (local labels, global labels, hierarchical labels,
//!    power symbols) then *merge* components that carry the same name, even
//!    when they never touch geometrically. Power symbols name their net after
//!    their Value ("GND", "+5V"); a local label names its net after its text.
//! 4. Across a sheet hierarchy, a child sheet's `(hierarchical_label "X")`
//!    is the same net as the parent `(sheet (pin "X"))` it sits behind, and
//!    global labels / power nets unify across every sheet.
//!
//! The result is an [`ExtractedBoard`] indistinguishable, to the binder and
//! solver, from one extracted from a layout, minus copper geometry. Net names
//! follow KiCad's conventions: a named net keeps its label name; an unnamed
//! net is `Net-(R1-Pad1)` after its lowest-sorted member pin.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use forge_sexpr::{Document, List};

use crate::{Component, ExtractError, ExtractedBoard, Net, Pin};

/// Coordinates are snapped to this grid (in mm) before comparison so that
/// floating-point noise in the file never splits a connection. KiCad places
/// everything on a 1.27 mm (50 mil) grid; 1e-3 mm tolerance is far tighter
/// than any real spacing yet absorbs textual rounding.
const SNAP: f64 = 1000.0; // points per mm → 0.001 mm resolution

/// A point snapped to the comparison grid: integer micrometres.
type Pt = (i64, i64);

fn snap(x: f64, y: f64) -> Pt {
    ((x * SNAP).round() as i64, (y * SNAP).round() as i64)
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Extract from a single schematic file's text. If the schematic references
/// sub-sheets, they are resolved relative to `base_dir` when provided;
/// without a directory, sub-sheets are skipped (single-sheet extraction).
pub fn extract(text: &str) -> Result<ExtractedBoard, ExtractError> {
    let doc = forge_sexpr::parse(text)?;
    let mut builder = NetlistBuilder::new();
    let root = builder.add_sheet_doc(&doc, "/", None, None)?;
    builder.finish(root)
}

/// Extract from an already-parsed top-level schematic document. Sub-sheets,
/// if any, are resolved relative to `base_dir`.
pub fn extract_from_doc(
    doc: &Document,
    base_dir: Option<&Path>,
) -> Result<ExtractedBoard, ExtractError> {
    let mut builder = NetlistBuilder::new();
    let root = builder.add_sheet_doc(doc, "/", base_dir, None)?;
    builder.finish(root)
}

/// Extract from a top-level `.kicad_sch` on disk, recursing into its
/// hierarchy. This is the path the cross-validation and the CLI use, because
/// the hierarchy lives in sibling files.
pub fn extract_from_path(path: &Path) -> Result<ExtractedBoard, ExtractError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ExtractError::Xml(format!("read {}: {e}", path.display())))?;
    let doc = forge_sexpr::parse(&text)?;
    let base = path.parent().map(Path::to_path_buf);
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let mut builder = NetlistBuilder::new();
    let root = builder.add_sheet_doc(&doc, "/", base.as_deref(), Some(name))?;
    builder.finish(root)
}

// ---------------------------------------------------------------------------
// Union-find over connection points
// ---------------------------------------------------------------------------

#[derive(Default)]
struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<usize>,
}

impl UnionFind {
    fn make(&mut self) -> usize {
        let id = self.parent.len();
        self.parent.push(id);
        self.rank.push(0);
        id
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        if self.rank[ra] < self.rank[rb] {
            self.parent[ra] = rb;
        } else if self.rank[ra] > self.rank[rb] {
            self.parent[rb] = ra;
        } else {
            self.parent[rb] = ra;
            self.rank[ra] += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Intermediate representation while building the netlist
// ---------------------------------------------------------------------------

/// A symbol pin pinned to an absolute schematic coordinate, tagged with which
/// component instance it belongs to.
struct PinSite {
    comp: usize, // index into NetlistBuilder::components
    pin_idx: usize,
    node: usize, // union-find node
}

/// A named anchor sitting at a coordinate: a label, power-net pin, sheet pin,
/// or hierarchical label. `name` is the net name it imposes. `scope` decides
/// how far the name reaches.
struct NamedAnchor {
    name: String,
    scope: NameScope,
    node: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum NameScope {
    /// Local label: unifies only within its own sheet.
    Local,
    /// Global label or power net: unifies across the whole design.
    Global,
    /// Hierarchical label (child side) or sheet pin (parent side): unifies
    /// within the sheet, and connects to the matching pin one level up.
    Hierarchical,
}

struct NetlistBuilder {
    uf: UnionFind,
    components: Vec<Component>,
    pin_sites: Vec<PinSite>,
    anchors: Vec<NamedAnchor>,
    /// Sheet each anchor was declared on, parallel to `anchors`. Used to scope
    /// local-label unification to a single sheet.
    anchor_sheet: Vec<SheetId>,
    /// Wire endpoints / junctions / no-connects, keyed by coordinate, so any
    /// two things at the same coordinate end up unioned.
    point_node: HashMap<(SheetId, Pt), usize>,
    /// Hierarchical wiring: (sheet instance path, pin name) → union-find node,
    /// recorded for both the parent sheet-pin and the child hierarchical-label
    /// so the two sides can be joined.
    hier_parent: Vec<(String, String, usize)>,
    hier_child: Vec<(String, String, usize)>,
    next_sheet: SheetId,
}

type SheetId = u32;

impl NetlistBuilder {
    fn new() -> Self {
        NetlistBuilder {
            uf: UnionFind::default(),
            components: Vec::new(),
            pin_sites: Vec::new(),
            anchors: Vec::new(),
            anchor_sheet: Vec::new(),
            point_node: HashMap::new(),
            hier_parent: Vec::new(),
            hier_child: Vec::new(),
            next_sheet: 0,
        }
    }

    /// A union-find node anchored to a coordinate within a sheet. The first
    /// thing to claim a coordinate creates the node; later things at the same
    /// coordinate union into it.
    fn node_at(&mut self, sheet: SheetId, at: Pt) -> usize {
        if let Some(&n) = self.point_node.get(&(sheet, at)) {
            return n;
        }
        let n = self.uf.make();
        self.point_node.insert((sheet, at), n);
        n
    }

    /// Record a named anchor, keeping `anchor_sheet` parallel to `anchors`.
    fn push_anchor(&mut self, sheet: SheetId, anchor: NamedAnchor) {
        self.anchors.push(anchor);
        self.anchor_sheet.push(sheet);
    }

    /// Parse and fold one sheet document into the global netlist, recursing
    /// into any `(sheet ...)` children. `inst_path` is the hierarchical
    /// instance path of *this* sheet ("/" for the root). `parent_pins` maps
    /// the names of the sheet-pins on the parent that point at this sheet to
    /// their parent-side union-find nodes.
    ///
    /// Returns the root board name (only meaningful for the top sheet).
    fn add_sheet_doc(
        &mut self,
        doc: &Document,
        inst_path: &str,
        base_dir: Option<&Path>,
        name_hint: Option<String>,
    ) -> Result<String, ExtractError> {
        let root = doc.root().ok_or(ExtractError::WrongRoot {
            expected: "kicad_sch",
            found: None,
        })?;
        if root.name() != Some("kicad_sch") {
            return Err(ExtractError::WrongRoot {
                expected: "kicad_sch",
                found: root.name().map(str::to_string),
            });
        }

        let sheet: SheetId = self.next_sheet;
        self.next_sheet += 1;

        // The hierarchical instance path KiCad records on each symbol begins
        // with the *root* schematic's uuid. At the top level we are handed
        // "/", so seed the real path from this file's uuid; sub-sheets already
        // arrive with a fully-qualified path from their parent.
        let inst_path: String = if inst_path == "/" {
            match root.find_value("uuid") {
                Some(u) => format!("/{u}"),
                None => "/".to_string(),
            }
        } else {
            inst_path.to_string()
        };
        let inst_path = inst_path.as_str();

        // Resolve the embedded lib_symbols once: lib_id → pin geometry.
        let lib = LibSymbols::parse(root);

        // -- Symbol instances -------------------------------------------------
        for sym in root.find_all("symbol") {
            // Real instances carry (lib_id ...); lib defs live under
            // lib_symbols and never appear at root level.
            let Some(lib_id) = sym.find_value("lib_id") else {
                continue;
            };
            self.add_symbol(sheet, sym, &lib_id, &lib, inst_path);
        }

        // -- Wires: union their two endpoints --------------------------------
        for wire in root.find_all("wire") {
            let mut pts = Vec::new();
            if let Some(p) = wire.find("pts") {
                for xy in p.find_all("xy") {
                    if let (Some(x), Some(y)) = (xy.arg_f64(0), xy.arg_f64(1)) {
                        pts.push(snap(x, y));
                    }
                }
            }
            for win in pts.windows(2) {
                let a = self.node_at(sheet, win[0]);
                let b = self.node_at(sheet, win[1]);
                self.uf.union(a, b);
            }
        }

        // -- Junctions: a junction is a connection point; touching wires/pins
        //    at that coordinate are already unioned via node_at, so we just
        //    materialise the node.
        for j in root.find_all("junction") {
            if let Some(at) = at_xy(j) {
                self.node_at(sheet, at);
            }
        }

        // -- No-connects ------------------------------------------------------
        // A no-connect marks a pin as deliberately unconnected. For netlist
        // structure it is a no-op (the pin simply stays on its own net); we
        // still materialise the node so the coordinate is accounted for.
        for nc in root.find_all("no_connect") {
            if let Some(at) = at_xy(nc) {
                self.node_at(sheet, at);
            }
        }

        // -- Labels -----------------------------------------------------------
        for lbl in root.find_all("label") {
            self.add_label(sheet, lbl, NameScope::Local);
        }
        for lbl in root.find_all("global_label") {
            self.add_label(sheet, lbl, NameScope::Global);
        }
        for lbl in root.find_all("hierarchical_label") {
            if let (Some(name), Some(at)) = (lbl.arg_value(0), at_xy(lbl)) {
                let node = self.node_at(sheet, at);
                let name = normalize_label(&name);
                self.push_anchor(
                    sheet,
                    NamedAnchor {
                        name: name.clone(),
                        scope: NameScope::Hierarchical,
                        node,
                    },
                );
                // Child side of a hierarchical connection: keyed by this
                // sheet's own instance path.
                self.hier_child.push((inst_path.to_string(), name, node));
            }
        }

        // -- Sub-sheets: record their pins, then recurse ----------------------
        for sub in root.find_all("sheet") {
            self.add_subsheet(sheet, sub, inst_path, base_dir)?;
        }

        // Board name: top sheet only.
        let name = name_hint
            .or_else(|| {
                root.find("title_block")
                    .and_then(|t| t.find_value("title"))
            })
            .unwrap_or_default();
        Ok(name)
    }

    fn add_symbol(
        &mut self,
        sheet: SheetId,
        sym: &List,
        lib_id: &str,
        lib: &LibSymbols,
        inst_path: &str,
    ) {
        let (sx, sy, srot) = at_of(sym);
        let mirror = sym.find_value("mirror");
        // A multi-unit part (e.g. a quad NAND) is drawn as one symbol instance
        // per gate, each with its own `(unit N)` and its own placement. Each
        // instance must contribute *only* its own unit's pins plus the unit-0
        // pins common to every gate (power pins usually live there). All
        // instances sharing a reference are merged into one component later.
        let inst_unit: u32 = sym.find_i64("unit").unwrap_or(1).max(0) as u32;
        // Per-instance reference. When a sheet is instantiated more than once
        // (reused hierarchy), each placement gets its own designator, recorded
        // in `(instances (project .. (path "/uuid/uuid" (reference "R201"))))`.
        // The static `(property "Reference")` is only a template, so we prefer
        // the instances entry whose path matches the sheet instance we are
        // currently expanding. Falls back to the template for flat designs.
        let reference = instance_reference(sym, inst_path)
            .or_else(|| property(sym, "Reference"))
            .unwrap_or_default();
        let value = property(sym, "Value").unwrap_or_default();
        let footprint = property(sym, "Footprint").unwrap_or_default();

        let def = lib.get(lib_id);
        // A symbol is a power-net source when KiCad would treat it as one:
        // either its library symbol carries the `(power)` flag, or its
        // reference is the canonical power prefix `#PWR`. Some older library
        // symbols (e.g. a hand-drawn VPP) omit the `(power)` flag yet are still
        // power symbols, so the reference test is what catches them. The
        // power_in/power_out distinction (handled per pin below) then decides
        // whether it actually *names* the net or merely flags it.
        let is_power = def.map(|d| d.is_power).unwrap_or(false)
            || reference.starts_with("#PWR");

        // Power symbols and other "#"-referenced symbols (PWR_FLAG, mounting
        // holes are real refs) are not simulation components in the usual
        // sense, but a power symbol still owns a pin that names a net. We keep
        // the component so its pin participates, except we skip emitting
        // power/flag pseudo-components from the final board (handled later by
        // reference prefix).
        let comp_idx = self.components.len();
        let mut properties = Vec::new();
        for prop in sym.find_all("property") {
            if let (Some(k), Some(v)) = (prop.arg_value(0), prop.arg_value(1)) {
                if !matches!(k.as_str(), "Reference" | "Value" | "Footprint") {
                    properties.push((k, v));
                }
            }
        }

        let mut pins = Vec::new();
        if let Some(def) = def {
            for lp in &def.pins {
                // Only this instance's unit (and the common unit 0).
                if lp.unit != 0 && lp.unit != inst_unit {
                    continue;
                }
                let abs = place_pin(lp.at, (sx, sy, srot), mirror.as_deref());
                let node = self.node_at(sheet, abs);
                let pin_idx = pins.len();
                pins.push(Pin {
                    number: lp.number.clone(),
                    net: None,
                    function: lp.name.clone(),
                    kind: lp.etype.clone(),
                    position: Some((abs.0 as f64 / SNAP, abs.1 as f64 / SNAP)),
                });
                self.pin_sites.push(PinSite {
                    comp: comp_idx,
                    pin_idx,
                    node,
                });

                // A power symbol's `power_in` pin imposes its Value as a global
                // net name (GND, +5V, VPP). A power symbol whose pin is
                // `power_out` is an ERC flag (PWR_FLAG): it only marks a net as
                // driven and must NOT name it, or every flagged net would
                // collapse into one "PWR_FLAG" net.
                if is_power && lp.etype == "power_in" {
                    self.push_anchor(
                        sheet,
                        NamedAnchor {
                            name: normalize_label(&value),
                            scope: NameScope::Global,
                            node,
                        },
                    );
                }
            }
        }

        self.components.push(Component {
            reference,
            value,
            lib_id: lib_id.to_string(),
            footprint,
            position: Some((sx, sy, srot)),
            layer: String::new(),
            properties,
            pins,
        });
    }

    fn add_label(&mut self, sheet: SheetId, lbl: &List, scope: NameScope) {
        if let (Some(name), Some(at)) = (lbl.arg_value(0), at_xy(lbl)) {
            let node = self.node_at(sheet, at);
            let name = normalize_label(&name);
            self.push_anchor(sheet, NamedAnchor { name, scope, node });
        }
    }

    fn add_subsheet(
        &mut self,
        sheet: SheetId,
        sub: &List,
        parent_path: &str,
        base_dir: Option<&Path>,
    ) -> Result<(), ExtractError> {
        let sheet_uuid = sub.find_value("uuid").unwrap_or_default();
        // The parent path is already rooted at the schematic uuid, so a child
        // instance path is just the parent's with this sheet's uuid appended.
        let child_path = if parent_path == "/" {
            format!("/{sheet_uuid}")
        } else {
            format!("{parent_path}/{sheet_uuid}")
        };

        // Record parent-side sheet pins. Each becomes a node at its coordinate
        // on *this* (parent) sheet, and is keyed so the child's matching
        // hierarchical label can be joined to it.
        for pin in sub.find_all("pin") {
            if let (Some(name), Some(at)) = (pin.arg_value(0), at_xy(pin)) {
                let node = self.node_at(sheet, at);
                self.hier_parent
                    .push((child_path.clone(), normalize_label(&name), node));
            }
        }

        // Recurse into the child file.
        let file = sheet_property(sub, "Sheetfile");
        if let (Some(file), Some(dir)) = (file, base_dir) {
            let child = dir.join(&file);
            if let Ok(text) = std::fs::read_to_string(&child) {
                if let Ok(doc) = forge_sexpr::parse(&text) {
                    let child_dir = child.parent().map(Path::to_path_buf);
                    self.add_sheet_doc(
                        &doc,
                        &child_path,
                        child_dir.as_deref(),
                        None,
                    )?;
                }
            } else {
                // Missing sub-sheet file: not fatal, just an incomplete board.
                let _ = PathBuf::from(&file);
            }
        }
        Ok(())
    }

    /// Resolve the union-find into nets, name them KiCad-style, and assign net
    /// ids to component pins.
    fn finish(mut self, name: String) -> Result<ExtractedBoard, ExtractError> {
        // 1. Hierarchical join: parent sheet-pin node ↔ child hierarchical
        //    label node sharing the same (child path, pin name).
        let parent_index: HashMap<(String, String), usize> = self
            .hier_parent
            .iter()
            .enumerate()
            .map(|(i, (p, n, _))| ((p.clone(), n.clone()), i))
            .collect();
        let joins: Vec<(usize, usize)> = self
            .hier_child
            .iter()
            .filter_map(|(path, name, child_node)| {
                parent_index
                    .get(&(path.clone(), name.clone()))
                    .map(|&pi| (self.hier_parent[pi].2, *child_node))
            })
            .collect();
        for (a, b) in joins {
            self.uf.union(a, b);
        }

        // 2. Named-anchor unification. Group anchor nodes by (scope, name).
        //    Local labels unify per-sheet only, so they are keyed including a
        //    sheet discriminator already baked into their node coordinates;
        //    but two different sheets can legitimately share a local label
        //    text meaning different nets. We therefore key local labels by the
        //    union-find *root after geometric pass is irrelevant*; instead we
        //    bucket by name within the same sheet via the node's origin. To
        //    keep it simple and correct, we key globals/power by name across
        //    everything and locals by (name) only after grouping per sheet.
        //
        // Implementation: globals & hierarchical-by-name unify across design;
        // locals unify only among anchors we recorded on the same sheet. We
        // approximate sheet identity for locals by the fact that their nodes
        // were created with a sheet-scoped key; to recover that we re-bucket
        // using a parallel record.
        let mut global_by_name: HashMap<String, usize> = HashMap::new();
        for a in &self.anchors {
            if a.scope == NameScope::Global {
                match global_by_name.get(&a.name) {
                    Some(&first) => self.uf.union(first, a.node),
                    None => {
                        global_by_name.insert(a.name.clone(), a.node);
                    }
                }
            }
        }
        // Local labels: unify by name *within a sheet*. We recorded the sheet
        // in `local_scope` alongside the anchor index.
        let mut local_by_key: HashMap<(SheetId, String), usize> = HashMap::new();
        for (i, a) in self.anchors.iter().enumerate() {
            if a.scope == NameScope::Local {
                let sid = self.anchor_sheet[i];
                let key = (sid, a.name.clone());
                match local_by_key.get(&key) {
                    Some(&first) => self.uf.union(first, a.node),
                    None => {
                        local_by_key.insert(key, a.node);
                    }
                }
            }
        }

        // 3. Resolve every pin's net root.
        let mut root_of_pin: Vec<(usize, usize)> = Vec::with_capacity(self.pin_sites.len());
        for (i, ps) in self.pin_sites.iter().enumerate() {
            let r = self.uf.find(ps.node);
            root_of_pin.push((i, r));
        }

        // 4. Decide each net's name.
        //    - If any anchor on the net is global/power → use that name.
        //    - else if any local/hier label → use that.
        //    - else synthesize Net-(Ref-PadN) from the lowest member pin.
        let mut anchor_name_of_root: HashMap<usize, (u8, String)> = HashMap::new();
        for a in &self.anchors {
            let r = self.uf.find(a.node);
            let prio = match a.scope {
                NameScope::Global => 3,
                NameScope::Hierarchical => 2,
                NameScope::Local => 1,
            };
            anchor_name_of_root
                .entry(r)
                .and_modify(|e| {
                    if prio > e.0 || (prio == e.0 && a.name < e.1) {
                        *e = (prio, a.name.clone());
                    }
                })
                .or_insert((prio, a.name.clone()));
        }

        // Group pins by root.
        let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (i, r) in &root_of_pin {
            members.entry(*r).or_default().push(*i);
        }

        // Assign net ids deterministically: named nets sorted by name, then
        // unnamed nets by their synthesized name. Net 0 stays "unconnected".
        let mut net_name: BTreeMap<usize, String> = BTreeMap::new();
        for (root, pin_idxs) in &members {
            let name = if let Some((_, n)) = anchor_name_of_root.get(root) {
                n.clone()
            } else {
                synth_name(&self.components, &self.pin_sites, pin_idxs)
            };
            net_name.insert(*root, name);
        }

        // Build the Net table. A single-pin net that is unnamed and sits on a
        // no-connect, or a power/flag pseudo pin alone, is still a net; we keep
        // it so lint can see it, matching the PCB extractor which keeps every
        // pad net.
        let mut roots: Vec<usize> = net_name.keys().copied().collect();
        roots.sort_by(|a, b| net_name[a].cmp(&net_name[b]).then(a.cmp(b)));
        let mut net_id_of_root: HashMap<usize, i64> = HashMap::new();
        let mut nets = Vec::new();
        let mut next_id: i64 = 1;
        for r in roots {
            let id = next_id;
            next_id += 1;
            net_id_of_root.insert(r, id);
            nets.push(Net { id, name: net_name[&r].clone() });
        }

        // 5. Write net ids back onto pins.
        for (i, r) in &root_of_pin {
            let ps = &self.pin_sites[*i];
            let id = net_id_of_root.get(r).copied();
            self.components[ps.comp].pins[ps.pin_idx].net = id;
        }

        // 5b. Merge multi-unit instances. A part drawn as several gates shares
        //     one reference across several symbol instances; KiCad's netlist
        //     lists it once, with the union of every unit's pins. We fold all
        //     instances with the same reference into the first, deduping pins
        //     by number (unit-0 pins such as VCC/GND are repeated on every
        //     gate's instance and must collapse to one).
        self.components = merge_units(std::mem::take(&mut self.components));

        // 6. Drop power/flag pseudo-components (references starting with '#')
        //    from the component list: they are not devices, only net sources,
        //    exactly as KiCad omits them from the netlist's component section.
        //    Their pins already imposed net names in step 2.
        self.components
            .retain(|c| !c.reference.starts_with('#'));

        // Re-number nets to be contiguous and drop nets that now have zero
        // real members (a power net with only a #PWR pin still has its name
        // applied to whatever real pins joined it; if none joined, it carries
        // no real pin and we drop it to match KiCad).
        let mut used: BTreeSet<i64> = BTreeSet::new();
        for c in &self.components {
            for p in &c.pins {
                if let Some(id) = p.net {
                    used.insert(id);
                }
            }
        }
        nets.retain(|n| used.contains(&n.id));

        Ok(ExtractedBoard { name, nets, components: self.components })
    }
}

/// Fold symbol instances that share a reference (the gates of a multi-unit
/// part) into one component, preserving instance order and deduping pins by
/// number. Components with an empty reference are left untouched (they cannot
/// be meaningfully merged and should not all collapse together).
fn merge_units(components: Vec<Component>) -> Vec<Component> {
    let mut order: Vec<String> = Vec::new();
    let mut by_ref: HashMap<String, Component> = HashMap::new();
    let mut anon: Vec<Component> = Vec::new();

    for c in components {
        if c.reference.is_empty() {
            anon.push(c);
            continue;
        }
        match by_ref.get_mut(&c.reference) {
            None => {
                order.push(c.reference.clone());
                by_ref.insert(c.reference.clone(), c);
            }
            Some(existing) => {
                let have: BTreeSet<String> =
                    existing.pins.iter().map(|p| p.number.clone()).collect();
                for p in c.pins {
                    // A duplicated pin number from a unit-0 (common) pin keeps
                    // the first occurrence; if a later one carries a net while
                    // the first did not, prefer the connected one.
                    if let Some(slot) =
                        existing.pins.iter_mut().find(|e| e.number == p.number)
                    {
                        if slot.net.is_none() {
                            slot.net = p.net;
                        }
                        let _ = &have;
                    } else {
                        existing.pins.push(p);
                    }
                }
            }
        }
    }

    let mut out: Vec<Component> = order
        .into_iter()
        .map(|r| by_ref.remove(&r).expect("ref recorded"))
        .collect();
    out.extend(anon);

    // Final safety net: a real part cannot have the same pad number twice.
    // Duplicates can sneak in when a library symbol lists a pin in both its
    // common (unit 0) body and a unit body, or when a socketed part is drawn
    // on two sheets. Collapse them, preferring a connected occurrence.
    for c in &mut out {
        if c.reference.is_empty() {
            continue;
        }
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut deduped: Vec<Pin> = Vec::with_capacity(c.pins.len());
        for p in std::mem::take(&mut c.pins) {
            match seen.get(&p.number).copied() {
                Some(idx) => {
                    if deduped[idx].net.is_none() {
                        deduped[idx].net = p.net;
                    }
                }
                None => {
                    seen.insert(p.number.clone(), deduped.len());
                    deduped.push(p);
                }
            }
        }
        c.pins = deduped;
    }
    out
}

// ---------------------------------------------------------------------------
// lib_symbols: pin geometry per lib_id
// ---------------------------------------------------------------------------

/// A pin as defined in a lib symbol: its connection point in the symbol's
/// local frame, its number, name, electrical type, and which unit it belongs
/// to (0 = common to every unit).
struct LibPin {
    at: (f64, f64, f64),
    number: String,
    name: String,
    etype: String,
    unit: u32,
}

struct LibDef {
    pins: Vec<LibPin>,
    is_power: bool,
}

/// All lib symbol definitions embedded in one schematic, indexed by lib_id.
struct LibSymbols {
    defs: HashMap<String, LibDef>,
}

impl LibSymbols {
    fn parse(root: &List) -> LibSymbols {
        let mut defs = HashMap::new();
        if let Some(lib) = root.find("lib_symbols") {
            for sym in lib.find_all("symbol") {
                let Some(lib_id) = sym.arg_value(0) else { continue };
                let is_power = sym.find("power").is_some();
                let mut pins = Vec::new();
                // Pins live in sub-symbol units named "<base>_<unit>_<style>".
                // The top-level symbol can also directly hold pins (rare).
                collect_pins(sym, &lib_id, &mut pins);
                for unit_sym in sym.find_all("symbol") {
                    let unit = unit_sym
                        .arg_value(0)
                        .as_deref()
                        .and_then(|n| unit_of(n, &lib_id))
                        .unwrap_or(0);
                    for p in unit_sym.find_all("pin") {
                        if let Some(mut lp) = lib_pin(p) {
                            lp.unit = unit;
                            pins.push(lp);
                        }
                    }
                }
                defs.insert(lib_id, LibDef { pins, is_power });
            }
        }
        LibSymbols { defs }
    }

    fn get(&self, lib_id: &str) -> Option<&LibDef> {
        self.defs.get(lib_id)
    }
}

/// Pins held directly on the top-level lib symbol (not inside a unit).
fn collect_pins(sym: &List, _lib_id: &str, pins: &mut Vec<LibPin>) {
    for p in sym.children.iter().filter_map(|c| c.as_list()) {
        if p.name() == Some("pin") {
            if let Some(mut lp) = lib_pin(p) {
                lp.unit = 0;
                pins.push(lp);
            }
        }
    }
}

/// Parse `<base>_<unit>_<bodystyle>` → unit number. Returns None when the name
/// doesn't carry a unit suffix.
fn unit_of(sub_name: &str, _lib_id: &str) -> Option<u32> {
    let mut parts = sub_name.rsplitn(3, '_');
    let _style = parts.next()?;
    let unit = parts.next()?;
    unit.parse().ok()
}

fn lib_pin(p: &List) -> Option<LibPin> {
    // (pin <etype> <graphic> (at x y a) (length L) (name "..") (number ".."))
    let etype = p.arg_value(0).unwrap_or_default();
    let at = match p.find("at") {
        Some(a) => (
            a.arg_f64(0).unwrap_or(0.0),
            a.arg_f64(1).unwrap_or(0.0),
            a.arg_f64(2).unwrap_or(0.0),
        ),
        None => return None,
    };
    let name = p.find("name").and_then(|n| n.arg_value(0)).unwrap_or_default();
    let number = p
        .find("number")
        .and_then(|n| n.arg_value(0))
        .unwrap_or_default();
    Some(LibPin { at, number, name, etype, unit: 0 })
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// Transform a lib-symbol pin's local connection point into absolute
/// schematic coordinates given the symbol instance's placement.
///
/// KiCad schematic coordinates have y pointing down, and a symbol's placement
/// angle rotates it counter-clockwise as seen on screen. On a y-down canvas a
/// CCW screen rotation is the matrix `(x cosθ + y sinθ, −x sinθ + y cosθ)`
/// (the transpose of the textbook y-up CCW matrix). Getting this handedness
/// right is what makes pin "1" land where eeschema put it rather than at pin
/// "2"'s spot, which is the difference between a correct netlist and a
/// scrambled one.
///
/// Mirroring is applied to the local point *before* rotation, matching
/// eeschema's transform order: `mirror x` flips across the x axis (negate y),
/// `mirror y` flips across the y axis (negate x).
fn place_pin(local: (f64, f64, f64), inst: (f64, f64, f64), mirror: Option<&str>) -> Pt {
    // Library symbols are drawn in a y-*up* frame; the schematic canvas is
    // y-*down*. Flip the pin's local y first so "top of symbol" stays at the
    // top once placed. (Skipping this silently swaps the two pins of every
    // vertical 2-pin part, which is exactly the kind of error that produces a
    // plausible-but-wrong netlist.)
    let (mut lx, mut ly) = (local.0, -local.1);
    match mirror {
        Some("x") => ly = -ly,
        Some("y") => lx = -lx,
        _ => {}
    }
    let theta = inst.2.to_radians();
    let (s, c) = theta.sin_cos();
    let rx = lx * c + ly * s;
    let ry = -lx * s + ly * c;
    snap(inst.0 + rx, inst.1 + ry)
}

/// `(at x y [rot])` for an instance/element.
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

/// `(at x y ...)` → snapped point.
fn at_xy(list: &List) -> Option<Pt> {
    let at = list.find("at")?;
    Some(snap(at.arg_f64(0)?, at.arg_f64(1)?))
}

/// Normalise a label / net name the way KiCad does before comparing nets.
/// KiCad escapes a literal `/` inside a label as `{slash}` (since `/` is the
/// sheet-path separator), so the same net can appear written both ways in one
/// file; they must compare equal or the net splits in two.
fn normalize_label(name: &str) -> String {
    if name.contains("{slash}") {
        name.replace("{slash}", "/")
    } else {
        name.to_string()
    }
}

/// The reference designator for this symbol *in the sheet instance we are
/// expanding*. A reused sheet lists one `(path .. (reference ..))` per
/// instantiation; we pick the one whose path equals the current instance path.
/// Returns None when the symbol has no instances block (older/flat files).
fn instance_reference(sym: &List, inst_path: &str) -> Option<String> {
    let instances = sym.find("instances")?;
    for project in instances.find_all("project") {
        for path in project.find_all("path") {
            if path.arg_value(0).as_deref() == Some(inst_path) {
                return path.find_value("reference");
            }
        }
    }
    None
}

/// A symbol-instance property value by key.
fn property(sym: &List, key: &str) -> Option<String> {
    for prop in sym.find_all("property") {
        if prop.arg_value(0).as_deref() == Some(key) {
            return prop.arg_value(1);
        }
    }
    None
}

/// A `(sheet ...)` property value by key (Sheetname/Sheetfile).
fn sheet_property(sheet: &List, key: &str) -> Option<String> {
    for prop in sheet.find_all("property") {
        if prop.arg_value(0).as_deref() == Some(key) {
            return prop.arg_value(1);
        }
    }
    None
}

/// KiCad's name for an unnamed net: `Net-(Ref-PadN)` after the alphabetically
/// lowest member pin (by reference then pin number).
fn synth_name(components: &[Component], sites: &[PinSite], pin_idxs: &[usize]) -> String {
    let mut best: Option<(String, String)> = None;
    for &pi in pin_idxs {
        let ps = &sites[pi];
        let c = &components[ps.comp];
        // Power/flag pseudo pins (#PWR, #FLG) never name a synthesized net.
        if c.reference.starts_with('#') {
            continue;
        }
        let key = (c.reference.clone(), ps_pin_number(components, ps));
        best = match best {
            Some(b) if b <= key => Some(b),
            _ => Some(key),
        };
    }
    match best {
        Some((r, n)) => format!("Net-({r}-Pad{n})"),
        None => "unconnected".to_string(),
    }
}

fn ps_pin_number(components: &[Component], ps: &PinSite) -> String {
    components[ps.comp].pins[ps.pin_idx].number.clone()
}
