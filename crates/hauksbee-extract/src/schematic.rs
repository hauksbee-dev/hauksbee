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
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-extract/schematic.md.

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

/// Extract from a `.kicad_sch` on disk, recursing into its hierarchy. This is
/// the path the cross-validation and the CLI use, because the hierarchy lives in
/// sibling files.
///
/// # Sub-sheets
///
/// A hierarchical sub-sheet is not a design. Its hierarchical labels are wired
/// to sheet pins in the PARENT, so a net driven from a sibling sheet appears,
/// inside the child file, as a stub touching exactly one pin. Extracting the
/// child alone therefore produces a netlist that is not merely incomplete but
/// *wrong* about connectivity: `net_lint`'s floating-control-pin check raised six
/// [high] findings across four MNT Reform sub-sheets on exactly this, one of them
/// `USB_PWR_EN`, which is driven from `reform2-lpc.kicad_sch` in the same
/// project. The top-level `reform2-motherboard30.kicad_sch` is clean.
///
/// So when handed a sub-sheet, this resolves the hierarchy it belongs to and
/// extracts from the root, which is the only file that can answer a connectivity
/// question about it. If no parent can be found, it refuses rather than return a
/// netlist that will be read as fact ([`ExtractError::OrphanSubSheet`]).
pub fn extract_from_path(path: &Path) -> Result<ExtractedBoard, ExtractError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ExtractError::Xml(format!("read {}: {e}", path.display())))?;
    let doc = forge_sexpr::parse(&text)?;

    if !is_root_sheet(&doc) {
        return match find_hierarchy_root(path) {
            Some(root_path) => extract_root_from_path(&root_path),
            None => Err(ExtractError::OrphanSubSheet {
                sheet: path.display().to_string(),
                needs: match path.parent() {
                    Some(dir) => format!(
                        "the root schematic of its project. No .kicad_sch in {} \
                         carries a (sheet_instances) block and reaches this file \
                         through its (sheet ... Sheetfile) references. Point \
                         hauksbee at the root schematic, or supply the whole \
                         project directory.",
                        dir.display()
                    ),
                    None => "the root schematic of its project".to_string(),
                },
            }),
        };
    }

    extract_root_from_path(path)
}

/// [`extract_from_path`] with the sub-sheet question already settled.
fn extract_root_from_path(path: &Path) -> Result<ExtractedBoard, ExtractError> {
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

/// Whether a parsed schematic is the ROOT of its hierarchy.
///
/// KiCad writes `(sheet_instances ...)`, the map of instance paths to page
/// numbers, into the root schematic and into no other file. Every root in the
/// board corpus carries it and no sub-sheet does, across KiCad 6 through 10.
/// A flat single-sheet design is its own root and carries it too, so this does
/// not mistake a one-file project for a fragment.
pub fn is_root_sheet(doc: &Document) -> bool {
    doc.root()
        .is_some_and(|r| r.find_all("sheet_instances").next().is_some())
}

/// The root schematic whose hierarchy contains `sheet`, searched among its
/// siblings.
///
/// A project keeps its sheets in one directory, so the search is that directory:
/// take every sibling that is a root, walk its `(sheet ... Sheetfile)` tree, and
/// return the first whose tree reaches `sheet`. Walking rather than trusting a
/// filename match matters, because two projects can sit in one directory (the
/// MNT Reform keyboard revisions do) and only one of them owns a given sheet.
fn find_hierarchy_root(sheet: &Path) -> Option<PathBuf> {
    let dir = sheet.parent()?;
    let target = std::fs::canonicalize(sheet).ok()?;
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("kicad_sch"))
        .collect();
    // Deterministic order: the same corpus must resolve the same root on every
    // machine, and read_dir order is not defined.
    candidates.sort();
    for cand in candidates {
        if cand == *sheet {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&cand) else {
            continue;
        };
        let Ok(doc) = forge_sexpr::parse(&text) else {
            continue;
        };
        if !is_root_sheet(&doc) {
            continue;
        }
        if hierarchy_reaches(&cand, &target) {
            return Some(cand);
        }
    }
    None
}

/// Whether the sheet tree rooted at `root` reaches `target` (canonicalised).
///
/// Depth-first over `(sheet ... Sheetfile)`, with a visited set: a KiCad
/// hierarchy is a tree, but a hand-edited or malformed one can name the same
/// file twice, and a cycle here would hang the extractor.
fn hierarchy_reaches(root: &Path, target: &Path) -> bool {
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        let canon = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
        if !seen.insert(canon.clone()) {
            continue;
        }
        if canon == *target {
            return true;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(doc) = forge_sexpr::parse(&text) else {
            continue;
        };
        let Some(node) = doc.root() else { continue };
        let Some(dir) = p.parent() else { continue };
        for sub in node.find_all("sheet") {
            if let Some(file) = sheet_property(sub, "Sheetfile") {
                stack.push(dir.join(file));
            }
        }
    }
    false
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
/// how far the name reaches; `kind` decides which name *wins* when one net
/// carries several competing anchors.
struct NamedAnchor {
    name: String,
    scope: NameScope,
    kind: AnchorKind,
    node: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum NameScope {
    /// Local label: unifies only within its own sheet. Hierarchical labels and
    /// sheet pins are also Local for *naming/within-sheet* purposes; their
    /// cross-sheet connection is carried separately by the hierarchical join
    /// (`hier_parent`/`hier_child`), not by this scope.
    Local,
    /// Global label or power net: unifies across the whole design.
    Global,
}

/// The schematic item behind a [`NamedAnchor`], for net-*naming* precedence
/// only, unification reach stays with [`NameScope`]. KiCad picks a net's name
/// from its highest-priority driver (`CONNECTION_SUBGRAPH::PRIORITY`): a
/// global label beats a power pin, which beats a local label, which beats a
/// hierarchical label. Collapsing these onto the two unification scopes
/// (Power→Global, Hier→Local) let the wrong name win whenever a net carried
/// two kinds from the same scope bucket, e.g. "+5V" outranked a global label
/// alphabetically instead of losing to it.
#[derive(Clone, Copy, PartialEq)]
enum AnchorKind {
    /// Global label (and a global bus label's promoted members).
    Global,
    /// Power symbol pin, or a hidden power_in pin's implicit connection.
    Power,
    /// Local label.
    Local,
    /// Hierarchical label / sheet-pin member binding.
    Hier,
}

impl AnchorKind {
    /// Naming precedence, highest wins. Mirrors KiCad's driver priority
    /// (`connection_graph.h`): GLOBAL > GLOBAL_POWER_PIN > LOCAL_LABEL >
    /// HIER_LABEL. Ties still break to the lexicographically smallest name.
    fn prio(self) -> u8 {
        match self {
            AnchorKind::Global => 4,
            AnchorKind::Power => 3,
            AnchorKind::Local => 2,
            AnchorKind::Hier => 1,
        }
    }
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
    /// Every wire / bus segment, per sheet, as an (a, b) endpoint pair tagged
    /// with whether it is a bus (thick) segment. KiCad connects anything lying
    /// *along* a segment, not only at its two endpoints: a label or pin placed
    /// mid-span on a wire joins that wire. We record the segments and run an
    /// incidence pass in `finish` that unions every point sitting on a
    /// segment's interior into it. (Most net labels in real boards sit
    /// mid-span, so without this almost every label floats free of its wire.)
    segments: Vec<(SheetId, Pt, Pt, bool)>,
    /// Bus alias definitions per sheet: alias name -> member tokens. A group bus
    /// `PREFIX{ALIAS}` referencing one is resolved by `expand_bus_on_sheet`,
    /// which substitutes the alias's (themselves-expanded) members. An alias that
    /// names another alias is left as a literal member (KiCad does not chain
    /// aliases either), so resolution cannot recurse through this map.
    bus_aliases: HashMap<(SheetId, String), Vec<String>>,
    /// Hierarchical wiring: (sheet instance path, pin name) → union-find node,
    /// recorded for both the parent sheet-pin and the child hierarchical-label
    /// so the two sides can be joined.
    hier_parent: Vec<(String, String, usize)>,
    hier_child: Vec<(String, String, usize)>,
    /// Bus labels placed on a sheet: the coordinate they sit at and the member
    /// names that bus carries. Used to resolve a bus that crosses a sheet
    /// boundary under a *different* name (a bus `DQ[0..31]` feeding a sheet pin
    /// `DPC[0..31]`): the members map positionally by index, so we need the
    /// bus's own member list at the pin's location.
    bus_labels: Vec<(SheetId, Pt, Vec<String>)>,
    /// Bus boundary anchors: a bus-valued sheet pin (parent side) or bus-valued
    /// hierarchical label (child side) that must connect member-wise. Resolved
    /// in `finish`, once every bus and bus label is known.
    bus_boundaries: Vec<BusBoundary>,
    /// Union-find nodes carrying an explicit `(no_connect ...)` flag. A pin whose
    /// net root matches one of these is a DELIBERATE no-connect, so netlint's
    /// floating-pin suppression must honor it, mirroring the KiCad-netlist
    /// loader, which preserves KiCad's own `pintype "…+no_connect"` /
    /// `unconnected-(…)` signal. Without this the two loaders disagree and the
    /// schematic path cries wolf on a deliberately-unconnected control pin.
    no_connect_nodes: Vec<usize>,
    next_sheet: SheetId,
}

type SheetId = u32;

/// One side of a bus crossing a sheet boundary.
struct BusBoundary {
    sheet: SheetId,
    at: Pt,
    /// The pin/label's own member names (`DPC0..DPC31`). These key the
    /// cross-boundary join so they match the other side by name.
    own_members: Vec<String>,
    /// Hierarchy key path (the child instance path).
    path: String,
    /// Parent (sheet pin) or child (hierarchical label) side.
    parent_side: bool,
}

impl NetlistBuilder {
    fn new() -> Self {
        NetlistBuilder {
            uf: UnionFind::default(),
            components: Vec::new(),
            pin_sites: Vec::new(),
            anchors: Vec::new(),
            anchor_sheet: Vec::new(),
            point_node: HashMap::new(),
            segments: Vec::new(),
            bus_aliases: HashMap::new(),
            hier_parent: Vec::new(),
            hier_child: Vec::new(),
            bus_labels: Vec::new(),
            bus_boundaries: Vec::new(),
            no_connect_nodes: Vec::new(),
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

        // -- Wires: union their two endpoints and record each segment so the
        //    mid-span incidence pass can attach labels/pins lying along it.
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
                self.segments.push((sheet, win[0], win[1], false));
            }
        }

        // -- Buses: a bus is a thick wire that carries several member nets at
        //    once. It is electrically *cosmetic*: members travel by name
        //    (every member wire entering a bus carries its own label, and a bus
        //    cannot be connected to a pin without one, KiCad's rule). We must
        //    therefore NOT union anything through bus geometry: if we did,
        //    every member's bus-entry landing point would collapse into one
        //    node and the whole bus would short into a single net. We record
        //    bus segments only so the incidence pass knows which segments are
        //    buses (and skips them).
        for bus in root.find_all("bus") {
            let mut pts = Vec::new();
            if let Some(p) = bus.find("pts") {
                for xy in p.find_all("xy") {
                    if let (Some(x), Some(y)) = (xy.arg_f64(0), xy.arg_f64(1)) {
                        pts.push(snap(x, y));
                    }
                }
            }
            for win in pts.windows(2) {
                self.segments.push((sheet, win[0], win[1], true));
            }
        }

        // -- Bus entries: the 45-degree stroke joining a member wire to a bus.
        //    Only the *wire* side carries electrical meaning (it is the end of
        //    the member wire, which carries the member's label). The bus side
        //    lands on the cosmetic bus and must stay electrically inert, or all
        //    members short together. We materialise both endpoint nodes (so the
        //    member wire's endpoint is anchored for the incidence pass) but do
        //    NOT union them across the bus.
        for be in root.find_all("bus_entry") {
            if let (Some(at), Some(size)) = (be.find("at"), be.find("size")) {
                if let (Some(x), Some(y), Some(dx), Some(dy)) = (
                    at.arg_f64(0),
                    at.arg_f64(1),
                    size.arg_f64(0),
                    size.arg_f64(1),
                ) {
                    self.node_at(sheet, snap(x, y));
                    self.node_at(sheet, snap(x + dx, y + dy));
                }
            }
        }

        // -- Bus aliases: `(bus_alias "NAME" (members "A" "B" ...))`. Recorded
        //    per sheet so a group bus referencing `{NAME}` can expand it.
        for ba in root.find_all("bus_alias") {
            if let Some(name) = ba.arg_value(0) {
                let mut members = Vec::new();
                if let Some(ms) = ba.find("members") {
                    for i in 0.. {
                        match ms.arg_value(i) {
                            Some(m) => members.push(m),
                            None => break,
                        }
                    }
                }
                self.bus_aliases.insert((sheet, name), members);
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
                // Record the node so a pin sharing its net root can be tagged an
                // explicit no-connect in `finish` (netlint suppression parity
                // with the KiCad-netlist loader).
                let node = self.node_at(sheet, at);
                self.no_connect_nodes.push(node);
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
                if let Some(members) = self.expand_bus_on_sheet(sheet, &name) {
                    // A bus passing through a hierarchical label connects
                    // member-wise one level up. Recorded and resolved in
                    // `finish` (child side), where the bus this label sits on is
                    // known so member nets bind positionally.
                    self.bus_boundaries.push(BusBoundary {
                        sheet,
                        at,
                        own_members: members,
                        path: inst_path.to_string(),
                        parent_side: false,
                    });
                } else {
                    // A hierarchical label is a labelled net on its own sheet:
                    // it unifies with any same-named local label or wire on the
                    // sheet (Local scope), and additionally connects one level
                    // up via the hierarchical join (hier_child key). Using
                    // Local scope here is what links the boundary net to the
                    // pins it actually serves on this sheet; without it the
                    // hier-label node floats free of the local net of the same
                    // name and the cross-sheet join connects nothing useful.
                    self.push_anchor(
                        sheet,
                        NamedAnchor {
                            name: name.clone(),
                            scope: NameScope::Local,
                            kind: AnchorKind::Hier,
                            node,
                        },
                    );
                    // Child side of a hierarchical connection: keyed by this
                    // sheet's own instance path.
                    self.hier_child.push((inst_path.to_string(), name, node));
                }
            }
        }

        // -- Sub-sheets: record their pins, then recurse ----------------------
        for sub in root.find_all("sheet") {
            self.add_subsheet(sheet, sub, inst_path, base_dir)?;
        }

        // Board name: top sheet only.
        let name = name_hint
            .or_else(|| root.find("title_block").and_then(|t| t.find_value("title")))
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
        let is_power = def.map(|d| d.is_power).unwrap_or(false) || reference.starts_with("#PWR");

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
                            kind: AnchorKind::Power,
                            node,
                        },
                    );
                }

                // Implicit power connection: a *hidden* `power_in` pin on an
                // ordinary device (not a power symbol) auto-connects to the
                // global power net named after the pin's *name*, with no wire
                // drawn. This is how KiCad ties a logic chip's hidden GND/VCC
                // pins to the rails. Visible power_in pins are wired normally
                // and must NOT be auto-named, or two different chips' visible
                // supply pins sharing a pin name would wrongly merge.
                if !is_power && lp.etype == "power_in" && lp.hidden && !lp.name.is_empty() {
                    self.push_anchor(
                        sheet,
                        NamedAnchor {
                            name: normalize_label(&lp.name),
                            scope: NameScope::Global,
                            kind: AnchorKind::Power,
                            node,
                        },
                    );
                }
            }
        }

        // Do-Not-Populate: KiCad schematics carry `(dnp yes)` on the symbol when
        // the part is on the layout but not assembled, so it is electrically
        // absent and checks reasoning about populated parts must skip it.
        let dnp = sym
            .find_value("dnp")
            .map(|v| v.eq_ignore_ascii_case("yes") || v == "true")
            .unwrap_or(false);

        self.components.push(Component {
            reference,
            value,
            lib_id: lib_id.to_string(),
            footprint,
            position: Some((sx, sy, srot)),
            layer: String::new(),
            properties,
            dnp,
            pins,
        });
    }

    /// Expand a bus label on `sheet`, resolving any bus-alias reference against
    /// the aliases recorded for that sheet. A bare alias token inside a group
    /// bus (`MEM{ADDR}` where `ADDR` is an alias) expands to the alias's
    /// members, each token of which is itself expanded (an alias member can be a
    /// vector like `A[7..0]`). Falls back to plain expansion when the name uses
    /// no alias.
    fn expand_bus_on_sheet(&self, sheet: SheetId, name: &str) -> Option<Vec<String>> {
        let aliases = &self.bus_aliases;
        let resolve = move |tok: &str| -> Option<Vec<String>> {
            let members = aliases.get(&(sheet, tok.to_string()))?;
            // Expand each alias member token (it may be a vector/group bus).
            let mut out = Vec::new();
            for m in members {
                match expand_bus(m) {
                    Some(exp) => out.extend(exp),
                    None => out.push(m.clone()),
                }
            }
            Some(out)
        };
        expand_bus_aliased(name, &resolve)
    }

    fn add_label(&mut self, sheet: SheetId, lbl: &List, scope: NameScope) {
        if let (Some(name), Some(at)) = (lbl.arg_value(0), at_xy(lbl)) {
            let node = self.node_at(sheet, at);
            let name = normalize_label(&name);
            // A label whose text is a bus expression (vector `D[0..7]` or group
            // `NAME{...}`) names a *bus*, not a single net. On a flat sheet the
            // bus is cosmetic: every member already reaches the bus via its own
            // labelled wire, and those member labels (`D0`, `D1`, …) unify by
            // name on their own. So a bus label imposes no single net name (it
            // must not, or `D[0..7]` would become one giant net). We record the
            // bus label's members and position so a sheet pin sitting on this
            // bus under a different name can map its members positionally.
            match self.expand_bus_on_sheet(sheet, &name) {
                Some(members) => {
                    // A GLOBAL bus label additionally globalizes every member,
                    // exactly as a plain global label globalizes its one net:
                    // `D[0..7]` as a global label means each of D0..D7 connects
                    // across the whole design. Each member gets a fresh node
                    // anchored twice under the member's name, Local scope, so
                    // it joins the member's own labelled net on *this* sheet,
                    // and Global scope, so same-named members unify design-wide
                    // through `global_by_name`. A local bus label must NOT do
                    // this (its members are sheet-local; globalizing them would
                    // short same-named members across unrelated sheets).
                    if scope == NameScope::Global {
                        for m in &members {
                            let mnode = self.uf.make();
                            self.push_anchor(
                                sheet,
                                NamedAnchor {
                                    name: m.clone(),
                                    scope: NameScope::Local,
                                    kind: AnchorKind::Global,
                                    node: mnode,
                                },
                            );
                            self.push_anchor(
                                sheet,
                                NamedAnchor {
                                    name: m.clone(),
                                    scope: NameScope::Global,
                                    kind: AnchorKind::Global,
                                    node: mnode,
                                },
                            );
                        }
                    }
                    self.bus_labels.push((sheet, at, members));
                }
                None => {
                    // A plain label's naming rank follows its scope directly.
                    let kind = match scope {
                        NameScope::Global => AnchorKind::Global,
                        NameScope::Local => AnchorKind::Local,
                    };
                    self.push_anchor(
                        sheet,
                        NamedAnchor {
                            name,
                            scope,
                            kind,
                            node,
                        },
                    );
                }
            }
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
                let name = normalize_label(&name);
                if let Some(members) = self.expand_bus_on_sheet(sheet, &name) {
                    // A bus sheet pin connects member-wise to the child's
                    // matching bus hierarchical label. Recorded and resolved in
                    // `finish` (parent side): the pin may sit on a parent bus
                    // carrying *differently-named* members (`DQ[0..31]` feeding
                    // a `DPC[0..31]` pin), which map positionally by index.
                    let _ = node;
                    self.bus_boundaries.push(BusBoundary {
                        sheet,
                        at,
                        own_members: members,
                        path: child_path.clone(),
                        parent_side: true,
                    });
                } else {
                    // A sheet pin's net on the parent is established
                    // geometrically: the wire touching the pin (which carries
                    // the parent-side label) reaches this node. We do NOT also
                    // register the pin *name* as a parent local label, because
                    // two instances of the same sub-sheet placed on one page
                    // would then wrongly merge through their identical pin
                    // names. The cross-boundary link is keyed by the unique
                    // child instance path instead.
                    self.hier_parent.push((child_path.clone(), name, node));
                }
            }
        }

        // Recurse into the child file.
        let file = sheet_property(sub, "Sheetfile");
        if let (Some(file), Some(dir)) = (file, base_dir) {
            let child = dir.join(&file);
            if let Ok(text) = std::fs::read_to_string(&child) {
                if let Ok(doc) = forge_sexpr::parse(&text) {
                    let child_dir = child.parent().map(Path::to_path_buf);
                    self.add_sheet_doc(&doc, &child_path, child_dir.as_deref(), None)?;
                }
            } else {
                // Missing sub-sheet file: not fatal, just an incomplete board.
                let _ = PathBuf::from(&file);
            }
        }
        Ok(())
    }

    /// Union every anchored point that lies strictly inside a *wire* segment
    /// into that segment, matching KiCad's "anything on a wire is on the net"
    /// rule. Bus segments are skipped (a bus is cosmetic; its members travel by
    /// name). Endpoints are already unioned when the segment was recorded, so
    /// only interior incidence is handled here.
    fn incidence_pass(&mut self) {
        // Index anchored points per sheet for a cheap interior test. We bucket
        // points by sheet so a segment only scans its own sheet's points.
        let mut pts_by_sheet: HashMap<SheetId, Vec<(Pt, usize)>> = HashMap::new();
        for (&(sheet, pt), &node) in &self.point_node {
            pts_by_sheet.entry(sheet).or_default().push((pt, node));
        }

        for &(sheet, a, b, is_bus) in &self.segments {
            if is_bus {
                continue;
            }
            let Some(points) = pts_by_sheet.get(&sheet) else {
                continue;
            };
            let seg_node = self.point_node[&(sheet, a)];
            // Only axis-aligned segments occur in KiCad schematics; handle the
            // general collinear case anyway for robustness.
            for &(p, node) in points {
                if p == a || p == b {
                    continue;
                }
                if point_strictly_inside(p, a, b) {
                    self.uf.union(seg_node, node);
                }
            }
        }
    }

    /// Resolve bus-valued sheet pins / hierarchical labels into member-wise
    /// hierarchical connections, handling buses that cross a sheet boundary
    /// under a different name by mapping members positionally by index.
    fn resolve_bus_boundaries(&mut self) {
        // Per sheet, group bus segments into connected subgraphs and record,
        // for each subgraph, the member list of any bus label sitting on it.
        // A query point is matched to a subgraph by lying on one of its
        // segments (endpoint or interior). We keep it simple: a flat list of
        // (sheet, points-of-subgraph, members) is overkill; instead, for each
        // boundary we directly search bus labels whose subgraph reaches the
        // boundary point. To connect a label to a boundary we test reachability
        // over the sheet's bus segments.
        let boundaries = std::mem::take(&mut self.bus_boundaries);
        for b in &boundaries {
            // Find the bus member list at this boundary: a bus label on the
            // same bus subgraph as the boundary point. If none, fall back to
            // the boundary's own member names (same-named bus both sides).
            let bus_members = self
                .bus_label_on_same_bus(b.sheet, b.at)
                .unwrap_or_else(|| b.own_members.clone());

            // Pair positionally. Lengths normally match; if they differ (a
            // malformed schematic) we connect the common prefix and leave the
            // rest as their own-named members.
            let n = b.own_members.len().min(bus_members.len());
            for i in 0..b.own_members.len() {
                let local_name = if i < n {
                    bus_members[i].clone()
                } else {
                    b.own_members[i].clone()
                };
                let mnode = self.uf.make();
                // Bind the fresh node to the member's net on this sheet via a
                // local label of the *local* (bus) member name. Hier kind for
                // naming: this anchor stands in for a sheet pin / hierarchical
                // label, which KiCad ranks below the member's own local label.
                self.push_anchor(
                    b.sheet,
                    NamedAnchor {
                        name: local_name,
                        scope: NameScope::Local,
                        kind: AnchorKind::Hier,
                        node: mnode,
                    },
                );
                // Key the hierarchical join under the boundary's OWN member
                // name so it matches the other side.
                let key = b.own_members[i].clone();
                if b.parent_side {
                    self.hier_parent.push((b.path.clone(), key, mnode));
                } else {
                    self.hier_child.push((b.path.clone(), key, mnode));
                }
            }
        }
    }

    /// Member list of a bus label sitting on the same bus subgraph as `at`, if
    /// any. Bus segments on the sheet are walked from `at` to discover the
    /// subgraph; a bus label whose coordinate lies on a reached segment names
    /// the bus.
    fn bus_label_on_same_bus(&self, sheet: SheetId, at: Pt) -> Option<Vec<String>> {
        // Collect this sheet's bus segments.
        let segs: Vec<(Pt, Pt)> = self
            .segments
            .iter()
            .filter(|(s, _, _, is_bus)| *s == sheet && *is_bus)
            .map(|(_, a, b, _)| (*a, *b))
            .collect();
        if segs.is_empty() {
            return None;
        }
        // Flood the bus subgraph reachable from `at` (which lies on some bus
        // segment, being the sheet-pin / hierarchical-label connection point).
        let mut reached: BTreeSet<Pt> = BTreeSet::new();
        let mut frontier: Vec<Pt> = Vec::new();
        // Seed: any segment endpoint coincident with `at`, or, if `at` lies on
        // a segment interior, both its endpoints.
        for &(a, b) in &segs {
            if a == at || b == at || point_strictly_inside(at, a, b) {
                for p in [a, b] {
                    if reached.insert(p) {
                        frontier.push(p);
                    }
                }
            }
        }
        if reached.is_empty() {
            return None;
        }
        while let Some(p) = frontier.pop() {
            for &(a, b) in &segs {
                let touch = a == p || b == p || point_strictly_inside(p, a, b);
                if touch {
                    for q in [a, b] {
                        if reached.insert(q) {
                            frontier.push(q);
                        }
                    }
                }
            }
        }
        // A bus label on this sheet whose coordinate lies on a reached segment.
        for (s, lpt, members) in &self.bus_labels {
            if *s != sheet {
                continue;
            }
            let on_bus = reached.contains(lpt)
                || segs.iter().any(|&(a, b)| {
                    (reached.contains(&a) || reached.contains(&b))
                        && (point_strictly_inside(*lpt, a, b) || a == *lpt || b == *lpt)
                });
            if on_bus {
                return Some(members.clone());
            }
        }
        None
    }

    /// Resolve the union-find into nets, name them KiCad-style, and assign net
    /// ids to component pins.
    fn finish(mut self, name: String) -> Result<ExtractedBoard, ExtractError> {
        // 0. Mid-span incidence. KiCad joins anything lying *on* a wire (or
        //    bus) segment, not only at its endpoints: a net label or a pin
        //    placed mid-span on a wire is electrically on that wire. Almost
        //    every net label in a real board sits mid-span, so without this
        //    nearly all labels float free of their wire and the netlist
        //    shatters. For each *wire* segment, union every same-sheet anchored
        //    point that lies strictly inside it into the segment. Bus segments
        //    are skipped: a bus is cosmetic and carries members by name, so
        //    unioning points along it would short every member together.
        self.incidence_pass();

        // 0b. Resolve bus boundaries (bus-valued sheet pins and hierarchical
        //     labels). Each member i gets one fresh union-find node that is
        //     - bound, as a local label, to the member's net on this sheet, and
        //     - keyed for the hierarchical join under the pin/label's OWN member
        //       name (so it matches the other side by name).
        //     The local member name is the bus's member name when the boundary
        //     sits on a bus carrying differently-named members (`DQ[0..31]`
        //     feeding a `DPC[0..31]` pin: index i is `DQi` locally, keyed
        //     `DPCi` across the boundary); otherwise it is the pin's own member
        //     name. Done before the hierarchical join, which then unifies the
        //     two sides through these keyed nodes.
        self.resolve_bus_boundaries();

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

        // 4. Decide each net's name, KiCad driver-priority style:
        //    global label > power pin > local label > hierarchical label
        //    (AnchorKind::prio; ties break to the smallest name), else
        //    synthesize Net-(Ref-PadN) from the lowest member pin. The kind is
        //    separate from the unification scope precisely for this step: a
        //    power pin unifies globally yet must lose the *name* contest to a
        //    global label, and a hierarchical label unifies locally yet must
        //    lose it to a local label.
        let mut anchor_name_of_root: HashMap<usize, (u8, String)> = HashMap::new();
        for a in &self.anchors {
            let r = self.uf.find(a.node);
            let prio = a.kind.prio();
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
            nets.push(Net {
                id,
                name: net_name[&r].clone(),
            });
        }

        // 5. Write net ids back onto pins.
        for (i, r) in &root_of_pin {
            let ps = &self.pin_sites[*i];
            let id = net_id_of_root.get(r).copied();
            self.components[ps.comp].pins[ps.pin_idx].net = id;
        }

        // 5a. Tag every pin sitting on an explicit `(no_connect ...)` root as a
        //     deliberate no-connect, so netlint's floating-control-pin
        //     suppression (which checks `pin.kind` for "no_connect") honors it
        //     on the schematic path exactly as it already does on the
        //     KiCad-netlist path. Done BEFORE the unit merge so the flag
        //     survives pin deduplication.
        if !self.no_connect_nodes.is_empty() {
            let nc_nodes = std::mem::take(&mut self.no_connect_nodes);
            // Tag by pin-site COINCIDENCE, not shared net ROOT: `point_node`
            // dedups union-find nodes by coordinate, so a no_connect placed on a
            // pin yields the same node id as that pin's site. Keying on the root
            // instead blanketed a whole net when an (ERC-invalid) no_connect fell
            // on a connected multi-pin net, suppressing genuine floating-pin
            // findings and dropping a driven power-rail pin. Match the pin the
            // marker actually sits on, mirroring KiCad's per-pin `+no_connect`.
            let nc_set: std::collections::HashSet<usize> = nc_nodes.into_iter().collect();
            for (i, _r) in &root_of_pin {
                let ps = &self.pin_sites[*i];
                if nc_set.contains(&ps.node) {
                    let kind = &mut self.components[ps.comp].pins[ps.pin_idx].kind;
                    if !kind.to_ascii_lowercase().contains("no_connect") {
                        if kind.is_empty() {
                            *kind = "no_connect".to_string();
                        } else {
                            kind.push_str("+no_connect");
                        }
                    }
                }
            }
        }

        // 5b. Merge multi-unit instances. A part drawn as several gates shares
        //     one reference across several symbol instances; KiCad's netlist
        //     lists it once, with the union of every unit's pins. We fold all
        //     instances with the same reference into the first, deduping pins
        //     by number (unit-0 pins such as VCC/GND are repeated on every
        //     gate's instance and must collapse to one).
        let (folded, bridged) = merge_units(std::mem::take(&mut self.components));
        self.components = folded;

        // 5c. A deduped common pin that carried two *different* net ids is one
        //     physical pin bridging two nets: they are electrically a single
        //     net (the layout would show one pad, one net) and must merge
        //     across the whole netlist, not just on that pin. Union each
        //     bridged pair and rewrite every pin onto the surviving id; the
        //     loser then has no pins left and the `used` prune below drops it
        //     from the net table. Winner choice is deterministic: a labelled
        //     net beats a synthesized Net-(Ref-PadN) (KiCad names the merged
        //     net after its driver, and a label always outranks a plain pin),
        //     ties break to the lower id.
        if !bridged.is_empty() {
            let synth_ids: BTreeSet<i64> = net_id_of_root
                .iter()
                .filter(|(root, _)| !anchor_name_of_root.contains_key(root))
                .map(|(_, id)| *id)
                .collect();
            let mut canon: HashMap<i64, i64> = HashMap::new();
            for (a, b) in bridged {
                let (ra, rb) = (canon_of(&canon, a), canon_of(&canon, b));
                if ra == rb {
                    continue;
                }
                // Order by (is-synthesized, id): named nets sort first, then
                // the lower id; the smaller key survives.
                let ka = (synth_ids.contains(&ra), ra);
                let kb = (synth_ids.contains(&rb), rb);
                let (win, lose) = if ka <= kb { (ra, rb) } else { (rb, ra) };
                canon.insert(lose, win);
            }
            for c in &mut self.components {
                for p in &mut c.pins {
                    if let Some(id) = p.net {
                        p.net = Some(canon_of(&canon, id));
                    }
                }
            }
        }

        // 6. Drop power/flag pseudo-components (references starting with '#')
        //    from the component list: they are not devices, only net sources,
        //    exactly as KiCad omits them from the netlist's component section.
        //    Their pins already imposed net names in step 2.
        self.components.retain(|c| !c.reference.starts_with('#'));

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

        Ok(ExtractedBoard {
            name,
            nets,
            components: self.components,
        })
    }
}

/// Follow a chain of net-id redirects (from bridged-net merging in `finish`
/// step 5c) to the surviving id. A loser is redirected exactly once, so the
/// chain is at most one link per bridged pair and needs no path compression.
fn canon_of(canon: &HashMap<i64, i64>, mut id: i64) -> i64 {
    while let Some(&next) = canon.get(&id) {
        id = next;
    }
    id
}

/// Fold symbol instances that share a reference (the gates of a multi-unit
/// part) into one component, preserving instance order and deduping pins by
/// number. Components with an empty reference are left untouched (they cannot
/// be meaningfully merged and should not all collapse together).
///
/// Also returns every pair of *distinct* net ids found on two occurrences of
/// one pin number. Those occurrences are the same physical pin (a unit-0
/// common pin drawn on several gates), so the two nets are electrically one:
/// the caller must union each pair across the whole netlist, or the short the
/// schematic drew is silently lost.
fn merge_units(components: Vec<Component>) -> (Vec<Component>, Vec<(i64, i64)>) {
    let mut order: Vec<String> = Vec::new();
    let mut by_ref: HashMap<String, Component> = HashMap::new();
    let mut anon: Vec<Component> = Vec::new();
    let mut bridged: Vec<(i64, i64)> = Vec::new();

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
                    // the first did not, prefer the connected one. Two
                    // occurrences on two *different* nets bridge those nets
                    // (same physical pin): recorded for the caller to union.
                    // An empty pad number is NOT a shared physical-pad identity
                    // (mechanical/NC/graphic pins, sloppily-imported connectors).
                    // Only genuinely-equal, NON-EMPTY numbers denote the same pad;
                    // blanks each stay their own pin so unrelated nets never bridge.
                    let slot = (!p.number.is_empty())
                        .then(|| existing.pins.iter_mut().find(|e| e.number == p.number))
                        .flatten();
                    if let Some(slot) = slot {
                        match (slot.net, p.net) {
                            (None, other) => slot.net = other,
                            (Some(a), Some(b)) if a != b => bridged.push((a, b)),
                            _ => {}
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
            // Empty pad numbers are never a shared identity, only consult and
            // record `seen` for non-empty numbers so blanks never dedup/bridge.
            let prior = (!p.number.is_empty())
                .then(|| seen.get(&p.number).copied())
                .flatten();
            match prior {
                Some(idx) => match (deduped[idx].net, p.net) {
                    (None, other) => deduped[idx].net = other,
                    // Same physical pin on two nets: bridge them, as above.
                    (Some(a), Some(b)) if a != b => bridged.push((a, b)),
                    _ => {}
                },
                None => {
                    if !p.number.is_empty() {
                        seen.insert(p.number.clone(), deduped.len());
                    }
                    deduped.push(p);
                }
            }
        }
        c.pins = deduped;
    }
    (out, bridged)
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
    /// Hidden pin. A hidden `power_in` pin auto-connects to the global power
    /// net named after the pin's *name* (KiCad's implicit power connection),
    /// even with no wire drawn to it.
    hidden: bool,
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
                let Some(lib_id) = sym.arg_value(0) else {
                    continue;
                };
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
    let name = p
        .find("name")
        .and_then(|n| n.arg_value(0))
        .unwrap_or_default();
    let number = p
        .find("number")
        .and_then(|n| n.arg_value(0))
        .unwrap_or_default();
    let hidden = p.has_flag("hide");
    Some(LibPin {
        at,
        number,
        name,
        etype,
        unit: 0,
        hidden,
    })
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
/// Mirroring is applied *after* rotation, in the placed frame, matching
/// eeschema's transform order (rotate the symbol, then flip it): `mirror x`
/// flips across the x axis (negate the rotated y), `mirror y` flips across the
/// y axis (negate the rotated x). The order is load-bearing only when a symbol
/// is both rotated and mirrored: applying the mirror first instead swaps the
/// two pins of such a part (e.g. a resistor placed at rot 90 + mirror x), which
/// is exactly the kind of error that yields a plausible-but-wrong netlist.
fn place_pin(local: (f64, f64, f64), inst: (f64, f64, f64), mirror: Option<&str>) -> Pt {
    // Library symbols are drawn in a y-*up* frame; the schematic canvas is
    // y-*down*. Flip the pin's local y first so "top of symbol" stays at the
    // top once placed. (Skipping this silently swaps the two pins of every
    // vertical 2-pin part.)
    let (lx, ly) = (local.0, -local.1);
    let theta = inst.2.to_radians();
    let (s, c) = theta.sin_cos();
    let mut rx = lx * c + ly * s;
    let mut ry = -lx * s + ly * c;
    match mirror {
        Some("x") => ry = -ry,
        Some("y") => rx = -rx,
        _ => {}
    }
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

/// True if `p` lies strictly between `a` and `b` on the segment a–b (collinear,
/// excluding the endpoints). Coordinates are integer micrometres, so the cross
/// product is exact.
fn point_strictly_inside(p: Pt, a: Pt, b: Pt) -> bool {
    let cross =
        (b.0 - a.0) as i128 * (p.1 - a.1) as i128 - (b.1 - a.1) as i128 * (p.0 - a.0) as i128;
    if cross != 0 {
        return false; // not collinear
    }
    let dot = (p.0 - a.0) as i128 * (b.0 - a.0) as i128 + (p.1 - a.1) as i128 * (b.1 - a.1) as i128;
    let len2 =
        (b.0 - a.0) as i128 * (b.0 - a.0) as i128 + (b.1 - a.1) as i128 * (b.1 - a.1) as i128;
    dot > 0 && dot < len2
}

// ---------------------------------------------------------------------------
// Bus syntax
// ---------------------------------------------------------------------------

/// Expand a bus label/pin name into its member net names, or `None` if the
/// name is not a bus expression (a plain net).
///
/// KiCad bus syntax (6+):
/// - **Vector**: `PREFIX[m..n]` → `PREFIXm`, `PREFIXm±1`, … `PREFIXn`
///   (ascending or descending). `D[0..7]` → `D0`…`D7`.
/// - **Group**: `[NAME]{ tok tok … }` → each token expanded; a vector token is
///   expanded in place, a plain token is taken as is. With a `NAME` prefix the
///   members are qualified `NAME.member`; anonymous groups keep bare member
///   names. `USB{DP DM}` → `USB.DP`, `USB.DM`; `{A B[0..1]}` → `A`, `B0`, `B1`.
///
/// The names returned are already `normalize_label`-d (the input should be).
fn expand_bus(name: &str) -> Option<Vec<String>> {
    // No alias context: a bare token never resolves to an alias.
    expand_bus_aliased(name, &|_| None)
}

/// Expand a bus label into its member net names, resolving bus-alias references.
///
/// `alias` maps an alias name to its member tokens (recorded from
/// `(bus_alias "NAME" (members ...))`). Inside a group bus `PREFIX{...}`, a bare
/// token that is *not* itself a vector/group bus is looked up as an alias: if it
/// resolves, its members are spliced in (each then qualified by `PREFIX`, exactly
/// as KiCad expands `MEM{ADDR}` when `ADDR` is an alias for `A[7..0] WE`).
///
/// Returns `None` for a plain (non-bus) label.
fn expand_bus_aliased(
    name: &str,
    alias: &dyn Fn(&str) -> Option<Vec<String>>,
) -> Option<Vec<String>> {
    expand_bus_aliased_depth(name, alias, 0)
}

/// Largest vector-bus span we will materialize. A KiCad bus like `D[0..7]` is a
/// handful of members; a malformed or hostile label such as `A[0..100000000]`
/// would otherwise eagerly allocate a hundred-million-element Vec and OOM. Real
/// EDA tools cap bus width similarly.
const MAX_BUS_WIDTH: u64 = 4096;
/// Bus labels nest at most a couple of levels in practice (`MEM{A[1..0] WE}`);
/// a pathological `A{B{C{...}}}` would otherwise recurse to stack-overflow.
const MAX_BUS_NEST_DEPTH: usize = 16;

fn expand_bus_aliased_depth(
    name: &str,
    alias: &dyn Fn(&str) -> Option<Vec<String>>,
    depth: usize,
) -> Option<Vec<String>> {
    if depth > MAX_BUS_NEST_DEPTH {
        return None;
    }
    // Group bus: optional prefix then `{ ... }`.
    if let Some(open) = name.find('{') {
        // KiCad text markup, overbar `~{X}`, subscript `_{X}`, superscript
        // `^{X}`, is NOT bus syntax: the char immediately before `{` is the
        // markup marker and KiCad keeps the literal text (`~{RST}`) as the net
        // name. Treating it as a group bus (prefix `~`, member `~.RST`) silently
        // splits ubiquitous active-low nets like `~{RESET}` / `~{CS}`. Only a
        // `{` that is not a markup brace opens a group bus.
        let is_markup = open > 0 && matches!(name.as_bytes()[open - 1], b'~' | b'_' | b'^');
        if !is_markup && name.ends_with('}') {
            let prefix = &name[..open];
            let inner = &name[open + 1..name.len() - 1];
            let mut out = Vec::new();
            for tok in inner.split_whitespace() {
                // A token expands as: a nested vector/group bus, OR a bus-alias
                // reference (`{ALIAS}` is written bare inside the group), OR a
                // literal single member.
                let expanded = expand_bus_aliased_depth(tok, alias, depth + 1)
                    .or_else(|| alias(tok))
                    .unwrap_or_else(|| vec![tok.to_string()]);
                for m in expanded {
                    if prefix.is_empty() {
                        out.push(m);
                    } else {
                        out.push(format!("{prefix}.{m}"));
                    }
                }
            }
            if out.is_empty() {
                return None;
            }
            return Some(out);
        }
    }
    // Vector bus: `PREFIX[m..n]`.
    let open = name.find('[')?;
    if !name.ends_with(']') {
        return None;
    }
    let prefix = &name[..open];
    let inner = &name[open + 1..name.len() - 1];
    let (lo, hi) = inner.split_once("..")?;
    let lo: i64 = lo.trim().parse().ok()?;
    let hi: i64 = hi.trim().parse().ok()?;
    // Reject an absurd span before allocating: bus widths past MAX_BUS_WIDTH are
    // malformed, not a real bus, and materializing the range would OOM.
    let width = hi.checked_sub(lo)?.unsigned_abs();
    if width >= MAX_BUS_WIDTH {
        return None;
    }
    let range: Vec<i64> = if lo <= hi {
        (lo..=hi).collect()
    } else {
        (hi..=lo).rev().collect()
    };
    Some(range.into_iter().map(|i| format!("{prefix}{i}")).collect())
}

/// Normalise a label / net name the way KiCad does before comparing nets.
/// KiCad escapes a literal `/` inside a label as `{slash}` (since `/` is the
/// sheet-path separator), so the same net can appear written both ways in one
/// file; they must compare equal or the net splits in two.
fn normalize_label(name: &str) -> String {
    crate::netname::unescape_net_name(name)
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

// ---------------------------------------------------------------------------
// Unit tests for the pure helpers (bus expansion, segment incidence)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{expand_bus, expand_bus_aliased, merge_units, point_strictly_inside};
    use crate::{Component, Pin};

    fn pin(number: &str, net: i64) -> Pin {
        Pin {
            number: number.into(),
            net: Some(net),
            function: String::new(),
            kind: String::new(),
            position: None,
        }
    }

    fn comp(reference: &str, pins: Vec<Pin>) -> Component {
        Component {
            reference: reference.into(),
            value: String::new(),
            lib_id: String::new(),
            footprint: String::new(),
            position: None,
            layer: String::new(),
            properties: Vec::new(),
            dnp: false,
            pins,
        }
    }

    #[test]
    fn empty_pin_numbers_are_never_bridged_or_dropped() {
        // R23 (SCH-EMPTY-PINNO-BRIDGE): pins with empty pad numbers were deduped
        // together; the second was dropped and its net silently merged onto the
        // first. Two blank-numbered pins on two DIFFERENT nets must each survive
        // and must NOT bridge those nets. (A lib pin with no `(number)` yields "".)
        let c = comp("J1", vec![pin("", 1), pin("", 2)]);
        let (out, bridged) = merge_units(vec![c]);
        let j1 = out.iter().find(|c| c.reference == "J1").expect("J1 kept");
        assert_eq!(
            j1.pins.len(),
            2,
            "both empty-numbered pins must survive, none dropped"
        );
        assert!(
            bridged.is_empty(),
            "empty pad numbers must not bridge unrelated nets; got {bridged:?}"
        );
    }

    #[test]
    fn genuine_duplicate_pad_numbers_still_collapse() {
        // The intended collapse (a real unit-0 common pin repeated on two units,
        // one connected) must still happen for NON-empty equal numbers.
        let mut first = pin("7", 5);
        first.net = None; // first occurrence unconnected
        let c = comp("U1", vec![first, pin("7", 5)]);
        let (out, bridged) = merge_units(vec![c]);
        let u1 = out.iter().find(|c| c.reference == "U1").expect("U1 kept");
        assert_eq!(
            u1.pins.len(),
            1,
            "equal non-empty pad numbers still collapse"
        );
        assert_eq!(u1.pins[0].net, Some(5), "the connected occurrence wins");
        assert!(bridged.is_empty());
    }

    #[test]
    fn vector_bus_ascending() {
        assert_eq!(
            expand_bus("D[0..3]"),
            Some(vec!["D0".into(), "D1".into(), "D2".into(), "D3".into()])
        );
    }

    #[test]
    fn vector_bus_descending() {
        // KiCad allows n..m with n > m; members run high to low.
        assert_eq!(
            expand_bus("A[2..0]"),
            Some(vec!["A2".into(), "A1".into(), "A0".into()])
        );
    }

    #[test]
    fn vector_bus_keeps_prefix_punctuation() {
        // A trailing '-' (active-low) or other punctuation in the prefix is
        // preserved: IRQ-[1..3] -> IRQ-1, IRQ-2, IRQ-3.
        assert_eq!(
            expand_bus("IRQ-[1..3]"),
            Some(vec!["IRQ-1".into(), "IRQ-2".into(), "IRQ-3".into()])
        );
    }

    #[test]
    fn group_bus_named_qualifies_members() {
        // USB{DP DM} -> USB.DP, USB.DM (KiCad qualifies named-group members).
        assert_eq!(
            expand_bus("USB{DP DM}"),
            Some(vec!["USB.DP".into(), "USB.DM".into()])
        );
    }

    #[test]
    fn group_bus_anonymous_keeps_bare_members() {
        // {A B[0..1]} -> A, B0, B1 (anonymous group, no prefix qualification).
        assert_eq!(
            expand_bus("{A B[0..1]}"),
            Some(vec!["A".into(), "B0".into(), "B1".into()])
        );
    }

    #[test]
    fn group_bus_mixes_vectors_and_plain() {
        assert_eq!(
            expand_bus("MEM{A[1..0] WE}"),
            Some(vec!["MEM.A1".into(), "MEM.A0".into(), "MEM.WE".into()])
        );
    }

    #[test]
    fn group_bus_expands_an_alias_reference() {
        // `(bus_alias "ADDR" (members "A[7..0]" "WE"))` then a group bus
        // `MEM{ADDR}` must expand ADDR to its (already vector-expanded) members,
        // each qualified by the MEM prefix.
        let alias = |tok: &str| -> Option<Vec<String>> {
            if tok == "ADDR" {
                // The resolver returns the alias members already expanded, the
                // way the builder's expand_bus_on_sheet feeds them.
                Some(vec![
                    "A7".into(),
                    "A6".into(),
                    "A5".into(),
                    "A4".into(),
                    "A3".into(),
                    "A2".into(),
                    "A1".into(),
                    "A0".into(),
                    "WE".into(),
                ])
            } else {
                None
            }
        };
        assert_eq!(
            expand_bus_aliased("MEM{ADDR}", &alias),
            Some(vec![
                "MEM.A7".into(),
                "MEM.A6".into(),
                "MEM.A5".into(),
                "MEM.A4".into(),
                "MEM.A3".into(),
                "MEM.A2".into(),
                "MEM.A1".into(),
                "MEM.A0".into(),
                "MEM.WE".into(),
            ])
        );
    }

    #[test]
    fn group_bus_mixes_alias_with_inline_members() {
        // `{ADDR DATA[1..0]}` (anonymous group): ADDR resolves via the alias,
        // DATA[1..0] expands as a vector, no prefix qualification.
        let alias = |tok: &str| -> Option<Vec<String>> {
            (tok == "ADDR").then(|| vec!["A0".into(), "A1".into()])
        };
        assert_eq!(
            expand_bus_aliased("{ADDR DATA[1..0]}", &alias),
            Some(vec![
                "A0".into(),
                "A1".into(),
                "DATA1".into(),
                "DATA0".into()
            ])
        );
    }

    #[test]
    fn unknown_alias_token_stays_a_literal_member() {
        // A bare token that is neither a bus nor a known alias is a single
        // literal member (the prior behaviour, preserved).
        let none = |_: &str| None;
        assert_eq!(
            expand_bus_aliased("MEM{ADDR}", &none),
            Some(vec!["MEM.ADDR".into()])
        );
    }

    #[test]
    fn plain_label_is_not_a_bus() {
        assert_eq!(expand_bus("VCC"), None);
        assert_eq!(expand_bus("PC-A0"), None);
        assert_eq!(expand_bus("AUTOFD-"), None);
        // A '[' that does not close, or a non-numeric range, is not a vector.
        assert_eq!(expand_bus("D[0..]"), None);
        assert_eq!(expand_bus("D[x..y]"), None);
    }

    #[test]
    fn text_markup_labels_are_not_group_buses() {
        // KiCad text markup, overbar `~{...}`, subscript `_{...}`, superscript
        // `^{...}`, is a single net name, NOT a group bus. Reading `~{RST}` as
        // bus prefix `~` with member `~.RST` silently split ubiquitous active-low
        // nets. These must be plain labels (None), keeping their literal text as
        // the net name.
        assert_eq!(expand_bus("~{RST}"), None);
        assert_eq!(expand_bus("~{RESET}"), None);
        assert_eq!(expand_bus("V_{ref}"), None);
        assert_eq!(expand_bus("N^{2}"), None);
        // A genuine named group bus (marker char is a normal letter) still works.
        assert_eq!(
            expand_bus("MEM{A B}"),
            Some(vec!["MEM.A".into(), "MEM.B".into()])
        );
    }

    #[test]
    fn absurd_vector_bus_span_is_rejected_not_allocated() {
        // A malformed / hostile range must not eagerly materialize a giant Vec
        // (OOM). Past MAX_BUS_WIDTH the label is treated as a non-bus.
        assert_eq!(expand_bus("A[0..100000000]"), None);
        assert_eq!(expand_bus("A[0..4096]"), None); // width 4096 == cap: rejected
                                                    // A sane bus just under the cap still expands.
        assert_eq!(
            expand_bus("A[0..2]"),
            Some(vec!["A0".into(), "A1".into(), "A2".into()])
        );
    }

    #[test]
    fn deeply_nested_group_bus_does_not_stack_overflow() {
        // A pathological brace-nested label must TERMINATE (the depth cap stops
        // the recursion; the over-deep inner token then falls back to a literal)
        // rather than recurse to a stack overflow. The exact members don't matter,
        // that the call returns at all is the guarantee under test.
        let deep = format!("{}{}", "A{".repeat(64), "}".repeat(64));
        let out = expand_bus(&deep).expect("depth-capped expansion still terminates");
        assert!(!out.is_empty());
    }

    #[test]
    fn incidence_interior_and_endpoints() {
        let a = (0, 0);
        let b = (0, 10_000);
        // Strictly inside.
        assert!(point_strictly_inside((0, 5_000), a, b));
        // Endpoints are not "strictly inside".
        assert!(!point_strictly_inside(a, a, b));
        assert!(!point_strictly_inside(b, a, b));
        // Off the line.
        assert!(!point_strictly_inside((1, 5_000), a, b));
        // Past the end (collinear but outside).
        assert!(!point_strictly_inside((0, 11_000), a, b));
    }
}
