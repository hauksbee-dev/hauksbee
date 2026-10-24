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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use forge_sexpr::{Document, List};

use crate::dsu::Dsu;
use crate::pcb::at_of;
use crate::{Component, ExtractError, ExtractedBoard, Net, Pin};

/// Coordinates are snapped to this grid (in mm) before comparison so that
/// floating-point noise in the file never splits a connection. KiCad places
/// everything on a 1.27 mm (50 mil) grid; 1e-3 mm tolerance is far tighter
/// than any real spacing yet absorbs textual rounding.
const SNAP: f64 = 1000.0; // points per mm → 0.001 mm resolution

/// A point snapped to the comparison grid: integer micrometres.
type Pt = (i64, i64);

#[allow(clippy::cast_possible_truncation, reason = "um coords fit i64")]
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
    crate::reject_merge_conflict(text)?;
    let doc = forge_sexpr::parse(text)?;
    let mut builder = NetlistBuilder::new();
    let root = builder.add_sheet_doc(&doc, "/", None, None)?;
    Ok(builder.finish(root))
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
/// *wrong* about connectivity: every cross-sheet driver reads as a floating
/// control pin.
///
/// So when handed a sub-sheet, this resolves the hierarchy it belongs to and
/// extracts from the root, which is the only file that can answer a connectivity
/// question about it. If no parent can be found, it refuses rather than return a
/// netlist that will be read as fact ([`ExtractError::OrphanSubSheet`]).
pub fn extract_from_path(path: &Path) -> Result<ExtractedBoard, ExtractError> {
    extract_from_path_with_notes(path).map(|(board, _)| board)
}

/// [`extract_from_path`], plus the coverage notes the read owes the caller: one
/// whole sentence per sub-sheet file that could not be read (missing, cut short,
/// empty or not a schematic). The hierarchy is still extracted from the sheets
/// that were read, so a fault on one of them is still found, but the caller must
/// carry these notes to the verdict: a result that did not read a sheet is not
/// an unconditional "clean". An unreadable sheet is wholly absent (a file that
/// does not parse yields no partial document), so the nets of the sheets that
/// were read are exactly what they are with that sheet missing.
///
/// # Errors
/// As [`extract_from_path`].
pub fn extract_from_path_with_notes(
    path: &Path,
) -> Result<(ExtractedBoard, Vec<String>), ExtractError> {
    let text = crate::read_regular_string(path)
        .map_err(|e| ExtractError::Xml(format!("read {}: {e}", path.display())))?;
    crate::reject_merge_conflict(&text)?;
    let doc = forge_sexpr::parse(&text)?;

    if !is_root_sheet(&doc) {
        let Some(root_path) = find_hierarchy_root(path) else {
            // A schematic with nothing on it (a valid header and no symbols,
            // sheets, wires or labels) is no sub-sheet: a sub-sheet exists to
            // be wired into a parent. Calling it one sent the user hunting for
            // a root schematic when the file simply has no parts.
            if is_blank_schematic(&doc) {
                return Err(ExtractError::Corrupt(format!(
                    "{} is a valid KiCad schematic but has no parts on it (no \
                     symbols, sheets, wires or labels), so there is nothing to \
                     check. Open it in KiCad and draw the circuit, or point \
                     hauksbee at the schematic that holds your design.",
                    path.display()
                )));
            }
            let needs = path.parent().map_or_else(
                || "the root schematic of its project".to_string(),
                orphan_sheet_needs,
            );
            let sheet = path.display().to_string();
            return Err(ExtractError::OrphanSubSheet { sheet, needs });
        };
        return extract_root_from_path(&root_path);
    }

    extract_root_from_path(path)
}

/// Whether a `(kicad_sch ...)` document carries nothing at all: no placed
/// symbol, sheet, wire, bus, junction, no-connect or label of any kind.
fn is_blank_schematic(doc: &Document) -> bool {
    const CONTENT: [&str; 11] = [
        "symbol",
        "sheet",
        "wire",
        "bus",
        "junction",
        "no_connect",
        "label",
        "global_label",
        "hierarchical_label",
        "bus_entry",
        "directive_label",
    ];
    doc.root().is_some_and(|r| {
        r.name() == Some("kicad_sch") && CONTENT.iter().all(|k| r.find_all(k).next().is_none())
    })
}

/// What an orphan sub-sheet in `dir` needs: the root schematic of its project.
/// When the project has a root whose hierarchy never uses this file, the file
/// is a leftover sheet, not a piece of the design, so name the root that is.
fn orphan_sheet_needs(dir: &Path) -> String {
    project_root(dir).map_or_else(
        || {
            format!(
                "the root schematic of its project. No .kicad_sch in {} \
                 carries a (sheet_instances) block and reaches this file \
                 through its (sheet ... Sheetfile) references. Point \
                 hauksbee at the root schematic, or supply the whole \
                 project directory.",
                dir.display()
            )
        },
        |root| {
            format!(
                "the root schematic of its project, and the project's root \
                 schematic {} does not use it: no (sheet ... Sheetfile) in \
                 that hierarchy names this file, so it is not part of the \
                 design. Point hauksbee at {} to check the design.",
                root.display(),
                root.display()
            )
        },
    )
}

/// [`extract_from_path`] with the sub-sheet question already settled.
fn extract_root_from_path(path: &Path) -> Result<(ExtractedBoard, Vec<String>), ExtractError> {
    let text = crate::read_regular_string(path)
        .map_err(|e| ExtractError::Xml(format!("read {}: {e}", path.display())))?;
    let doc = forge_sexpr::parse(&text)?;
    let base = path.parent().map(Path::to_path_buf);
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let mut builder = NetlistBuilder::new();
    builder.design_bus_aliases = project_bus_aliases(&path.with_extension("kicad_pro"));
    builder.scan_hierarchy(&doc, base.as_deref());
    // The root sheet is the first ancestor, so a sub-sheet pointing back at the
    // top file closes the cycle here instead of recursing into it.
    builder
        .ancestor_sheets
        .push(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
    let root = builder.add_sheet_doc(&doc, "/", base.as_deref(), Some(name))?;
    let notes = std::mem::take(&mut builder.unread_sheets);
    Ok((builder.finish(root), notes))
}

/// Every `(bus_alias "NAME" (members "A" "B" ...))` a sheet defines, in
/// file order.
fn bus_alias_defs(root: &List) -> Vec<(String, Vec<String>)> {
    root.find_all("bus_alias")
        .filter_map(|ba| {
            let name = ba.arg_value(0)?;
            let members = ba
                .find("members")
                .map(|ms| (0..).map_while(|i| ms.arg_value(i)).collect())
                .unwrap_or_default();
            Some((name, members))
        })
        .collect()
}

/// The bus aliases a KiCad project file defines (`schematic.bus_aliases`, a
/// map of alias name to member tokens; KiCad 9 and later). A missing or
/// unreadable project file defines none: older projects keep their aliases in
/// the sheet files, which `add_sheet_doc` reads.
fn project_bus_aliases(pro: &Path) -> HashMap<String, Vec<String>> {
    let Ok(text) = crate::read_regular_string(pro) else {
        return HashMap::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return HashMap::new();
    };
    json.pointer("/schematic/bus_aliases")
        .and_then(serde_json::Value::as_object)
        .map(|aliases| {
            aliases
                .iter()
                .filter_map(|(name, members)| {
                    let members = members
                        .as_array()?
                        .iter()
                        .filter_map(|m| m.as_str().map(str::to_string))
                        .collect();
                    Some((name.clone(), members))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a parsed schematic is the ROOT of its hierarchy.
///
/// KiCad writes `(sheet_instances ...)`, the map of instance paths to page
/// numbers, into the root schematic and into no other file. Every root in the
/// board corpus carries it and no sub-sheet does, across KiCad 6 through 10.
/// A flat single-sheet design is its own root and carries it too, so this does
/// not mistake a one-file project for a fragment.
#[must_use]
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
        let Ok(text) = crate::read_regular_string(&cand) else {
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

/// The root schematic of the KiCad project in `dir`: the `.kicad_sch` named
/// like a `.kicad_pro` beside it, when that file is a root. `None` when the
/// directory holds no project file or its namesake is not a root.
fn project_root(dir: &Path) -> Option<PathBuf> {
    let mut projects: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("kicad_pro"))
        .collect();
    projects.sort();
    projects.into_iter().find_map(|pro| {
        let sch = pro.with_extension("kicad_sch");
        let text = crate::read_regular_string(&sch).ok()?;
        let doc = forge_sexpr::parse(&text).ok()?;
        is_root_sheet(&doc).then_some(sch)
    })
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
        let Ok(text) = crate::read_regular_string(&p) else {
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
    /// Power symbol pin, or a hidden `power_in` pin's implicit connection.
    Power,
    /// Local label.
    Local,
    /// Hierarchical label / bus member bound through one.
    Hier,
    /// Member of a bus driven only by sheet pins. Joins a same-named net on
    /// its sheet but, unlike a label, not another bus's same-named member.
    SheetPin,
}

impl AnchorKind {
    /// Naming precedence, highest wins. Mirrors KiCad's driver priority
    /// (`connection_graph.h`): GLOBAL > `GLOBAL_POWER_PIN` > `LOCAL_LABEL` >
    /// `HIER_LABEL`. Ties still break to the lexicographically smallest name.
    const fn prio(self) -> u8 {
        match self {
            Self::Global => 4,
            Self::Power => 3,
            Self::Local => 2,
            Self::Hier => 1,
            Self::SheetPin => 0,
        }
    }
}

struct NetlistBuilder {
    uf: Dsu,
    components: Vec<Component>,
    pin_sites: Vec<PinSite>,
    anchors: Vec<NamedAnchor>,
    /// Sheet each anchor was declared on, parallel to `anchors`. Used to scope
    /// local-label unification to a single sheet.
    anchor_sheet: Vec<SheetId>,
    /// Wire endpoints / junctions / no-connects, keyed by coordinate, so any
    /// two things at the same coordinate end up unioned.
    point_node: HashMap<(SheetId, Pt), usize>,
    /// How many things claim each point (pins, labels, wire ends, ...), the
    /// points wires end at, the junctions, and the symbol and sheet pins. A
    /// point on a wire's interior joins that wire when a junction marks it,
    /// or when a single label sits there alone; a wire end or a pin there
    /// does not join, nor do two things sharing the point. This is
    /// eeschema's behaviour as its netlist export shows it.
    claims: HashMap<(SheetId, Pt), u32>,
    wire_ends: HashSet<(SheetId, Pt)>,
    junctions: HashSet<(SheetId, Pt)>,
    pin_points: HashSet<(SheetId, Pt)>,
    /// Every wire / bus segment, per sheet, as an (a, b) endpoint pair tagged
    /// with whether it is a bus (thick) segment. KiCad connects a label lying
    /// *along* a segment, not only at its two endpoints: a label placed
    /// mid-span on a wire joins that wire (a pin there needs a junction). We record the segments and run an
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
    /// Bus aliases visible on every sheet of the design: the project file's
    /// (`<root>.kicad_pro`, `schematic.bus_aliases`, where KiCad 9 and later
    /// keep them), then each sheet file's `(bus_alias ...)` in hierarchy
    /// order, the first definition of a name winning. eeschema resolves an
    /// alias schematic-wide, so a sheet may use `{LVDS}` that only a sibling
    /// sheet defines; a sheet's own definition still wins on that sheet.
    /// Without these, a group bus such as `{FX2}` crossing into a sub-sheet
    /// expands to one literal member and every net it carries splits at the
    /// sheet boundary.
    design_bus_aliases: HashMap<String, Vec<String>>,
    /// Every sheet file of the hierarchy, parsed once by
    /// [`Self::scan_hierarchy`] and keyed by canonical path, so the alias
    /// pre-pass costs no second parse and a reused sheet is parsed once.
    parsed_sheets: HashMap<PathBuf, Rc<Document>>,
    /// The root file's `(symbol_instances (path "/sheet/symbol" (reference ..)
    /// (unit ..)))` table, which KiCad 6 and early 7 files keep their per
    /// placement designators in instead of in each symbol. Keyed by the path
    /// without the root uuid; see [`Self::legacy_instance`].
    legacy_instances: HashMap<String, (String, Option<i64>)>,
    /// The root sheet's uuid, which a sub-sheet's instance path starts with
    /// but a legacy `symbol_instances` path does not.
    root_uuid: String,
    /// Hierarchical wiring: (sheet instance path, pin name) → union-find node,
    /// recorded for both the parent sheet-pin and the child hierarchical-label
    /// so the two sides can be joined.
    hier_parent: Vec<(String, String, usize)>,
    hier_child: Vec<(String, String, usize)>,
    /// Bus labels placed on a sheet: the coordinate they sit at and the member
    /// names that bus carries. Used to resolve a bus that crosses a sheet
    /// boundary under a *different* name (a bus `DQ[0..31]` feeding a sheet pin
    /// `DPC[0..31]`): the members map positionally by index, so we need the
    /// bus's own member list at the pin's location. The last field is each
    /// member's group-local name when the label is a group bus (see
    /// [`group_locals`]), which is how a group bus matches members.
    bus_labels: Vec<BusLabel>,
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
    /// Each sheet instance's KiCad display path (`/IO Banks/Bank A/`; `/`
    /// for the root), indexed by `SheetId`. Qualifies a sheet-scoped net
    /// name that more than one instance carries.
    sheet_display: Vec<String>,
    /// The display path the next `add_sheet_doc` call is entering.
    pending_display: Option<String>,
    /// The chain of sub-sheet files currently being expanded, canonicalized,
    /// innermost last. A `Sheetfile` that points back at one of its own
    /// ancestors (a sheet that references itself, or two sheets that reference
    /// each other) is a cycle, and following it recurses until the thread's
    /// stack aborts the process. Both shapes occur in real projects: a sheet
    /// duplicated by copy-paste keeps the original's `Sheetfile`, and a renamed
    /// file can leave a stale mutual reference.
    ///
    /// An ancestor CHAIN and not a set of every file ever seen, which is the
    /// distinction between a cycle and a reused sub-sheet. A hierarchy that
    /// places one sub-sheet several times is the ordinary way to draw a
    /// multi-channel board, and each placement is a separate set of parts with
    /// its own designators, numbered through each symbol's `(instances)` block.
    /// A set-based guard would read the second placement as already in the
    /// netlist and silently drop half the board.
    ancestor_sheets: Vec<PathBuf>,
    /// How deep the current `Sheetfile` recursion is. The visited set stops
    /// cycles; this stops a pathological but acyclic chain (a generated project
    /// nesting thousands of unique sheets) from reaching the same stack limit.
    sheet_depth: usize,
    /// One whole sentence per sub-sheet file that could not be read, each once
    /// (a sheet placed several times is one file). The read goes on without it.
    unread_sheets: Vec<String>,
    /// The files behind `unread_sheets`, so a file placed under several sheet
    /// names is said once (under the first sheet that referenced it).
    unread_files: Vec<PathBuf>,
}

/// The deepest `Sheetfile` chain followed. KiCad's own hierarchies are a
/// handful of levels; 64 is far past any real design and still nowhere near
/// the stack budget of `add_sheet_doc`.
const MAX_SHEET_DEPTH: usize = 64;

/// The most sheet INSTANCES one hierarchy may expand to. Each placement of a
/// sub-sheet is a separate set of parts, so a sheet drawn twice on every level
/// of an N-level hierarchy ("a diamond") is 2^N instances from N tiny files:
/// 24 files of 300 bytes ran for minutes. Real boards stay in the hundreds, and
/// a 5000-sheet flat fan is still read; the cap only stops the multiplication.
const MAX_SHEET_INSTANCES: usize = 10_000;

/// The most distinct sheet files the alias pre-pass parses. Far above any real
/// hierarchy; a bound so a generated project cannot make the pre-pass unbounded.
const MAX_HIERARCHY_FILES: usize = 4096;

type SheetId = u32;

/// A bus label: its sheet, position, members, and (for a group bus) each
/// member's group-local name.
type BusLabel = (SheetId, Pt, Vec<String>, Option<Vec<String>>);

/// One side of a bus crossing a sheet boundary.
struct BusBoundary {
    sheet: SheetId,
    at: Pt,
    /// The pin/label's own member names (`DPC0..DPC31`). These key the
    /// cross-boundary join so they match the other side by name.
    own_members: Vec<String>,
    /// The own members' group-local names when the pin/label is a group bus
    /// (`{Q[0..7]}` -> `Q0..Q7`).
    own_locals: Option<Vec<String>>,
    /// Hierarchy key path (the child instance path).
    path: String,
    /// Parent (sheet pin) or child (hierarchical label) side.
    parent_side: bool,
}

impl NetlistBuilder {
    fn new() -> Self {
        Self {
            uf: Dsu::new(0),
            components: Vec::new(),
            pin_sites: Vec::new(),
            anchors: Vec::new(),
            anchor_sheet: Vec::new(),
            point_node: HashMap::new(),
            claims: HashMap::new(),
            wire_ends: HashSet::new(),
            junctions: HashSet::new(),
            pin_points: HashSet::new(),
            segments: Vec::new(),
            bus_aliases: HashMap::new(),
            design_bus_aliases: HashMap::new(),
            parsed_sheets: HashMap::new(),
            legacy_instances: HashMap::new(),
            root_uuid: String::new(),
            hier_parent: Vec::new(),
            hier_child: Vec::new(),
            bus_labels: Vec::new(),
            bus_boundaries: Vec::new(),
            no_connect_nodes: Vec::new(),
            next_sheet: 0,
            sheet_display: Vec::new(),
            pending_display: None,
            ancestor_sheets: Vec::new(),
            sheet_depth: 0,
            unread_sheets: Vec::new(),
            unread_files: Vec::new(),
        }
    }

    /// A union-find node anchored to a coordinate within a sheet. The first
    /// thing to claim a coordinate creates the node; later things at the same
    /// coordinate union into it.
    fn node_at(&mut self, sheet: SheetId, at: Pt) -> usize {
        *self.claims.entry((sheet, at)).or_default() += 1;
        if let Some(&n) = self.point_node.get(&(sheet, at)) {
            return n;
        }
        let n = self.uf.make();
        self.point_node.insert((sheet, at), n);
        n
    }

    /// [`Self::node_at`] for a wire's own end point.
    fn wire_end_at(&mut self, sheet: SheetId, at: Pt) -> usize {
        self.wire_ends.insert((sheet, at));
        self.node_at(sheet, at)
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
    #[allow(clippy::too_many_lines, reason = "one pass over a sheet's items")]
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

        if self.next_sheet as usize >= MAX_SHEET_INSTANCES {
            return Err(self.too_many_instances());
        }
        let sheet: SheetId = self.next_sheet;
        self.next_sheet += 1;
        let display = self
            .pending_display
            .take()
            .unwrap_or_else(|| "/".to_string());
        self.sheet_display.push(display);

        // The hierarchical instance path KiCad records on each symbol begins
        // with the *root* schematic's uuid. At the top level we are handed
        // "/", so seed the real path from this file's uuid; sub-sheets already
        // arrive with a fully-qualified path from their parent.
        let inst_path: String = if inst_path == "/" {
            if let Some(uuid) = root.find_value("uuid") {
                self.root_uuid = uuid;
            }
            self.legacy_instances = legacy_symbol_instances(root);
            root.find_value("uuid")
                .map_or_else(|| "/".to_string(), |u| format!("/{u}"))
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
                let a = self.wire_end_at(sheet, win[0]);
                let b = self.wire_end_at(sheet, win[1]);
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
        for (name, members) in bus_alias_defs(root) {
            self.bus_aliases.insert((sheet, name), members);
        }

        // -- Junctions: a junction is a connection point; touching wires/pins
        //    at that coordinate are already unioned via node_at, so we just
        //    materialise the node.
        for j in root.find_all("junction") {
            if let Some(at) = at_xy(j) {
                self.junctions.insert((sheet, at));
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
                let raw = name;
                let name = normalize_label(&raw);
                if let Some(members) = self.expand_bus_on_sheet(sheet, &raw) {
                    // A bus passing through a hierarchical label connects
                    // member-wise one level up. Recorded and resolved in
                    // `finish` (child side), where the bus this label sits on is
                    // known so member nets bind positionally.
                    self.bus_boundaries.push(BusBoundary {
                        sheet,
                        at,
                        own_locals: group_locals(&raw, &members),
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
        // A reused sheet picks the unit per placement, in its instances entry.
        let (instance_ref, instance_unit) = self.placement_entry(sym, inst_path);
        let unit = instance_unit.or_else(|| sym.find_i64("unit"));
        let inst_unit = u32::try_from(unit.unwrap_or(1).max(0)).unwrap_or(u32::MAX);
        // Likewise a De Morgan part draws each unit in two body styles, and
        // only the placed style's pins exist: `(convert N)` through KiCad 8,
        // `(body_style N)` from KiCad 9. The alternate style can carry other
        // pin numbers at the same spots, so admitting it adds phantom pins.
        let style = sym
            .find_i64("convert")
            .or_else(|| sym.find_i64("body_style"));
        let inst_style = u32::try_from(style.unwrap_or(1).max(0)).unwrap_or(u32::MAX);
        // Per-instance reference. When a sheet is instantiated more than once
        // (reused hierarchy), each placement gets its own designator, recorded
        // in `(instances (project .. (path "/uuid/uuid" (reference "R201"))))`.
        // The static `(property "Reference")` is only a template, so we prefer
        // the instances entry whose path matches the sheet instance we are
        // currently expanding. Falls back to the template for flat designs.
        let reference = instance_ref
            .or_else(|| property(sym, "Reference"))
            .unwrap_or_default();
        let value = property(sym, "Value").unwrap_or_default();
        let footprint = property(sym, "Footprint").unwrap_or_default();

        let def = lib.for_instance(sym, lib_id);
        // A symbol is a power-net source only when its library symbol carries
        // the `(power)` flag; a `#PWR` reference alone is not one (kicad-cli
        // leaves its visible `power_in` pin unnamed, and a hidden one is caught
        // as an implicit pin below). The power_in/power_out distinction
        // (handled per pin below) then decides whether it *names* the net.
        let is_power = def.is_some_and(|d| d.is_power);
        let power_scope = def.map_or(NameScope::Global, |d| d.power_scope);

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
        carry_library_text(&mut properties, def);

        let mut pins = Vec::new();
        if let Some(def) = def {
            for lp in &def.pins {
                // Only this instance's unit and body style (0 = common).
                if (lp.unit != 0 && lp.unit != inst_unit)
                    || (lp.style != 0 && lp.style != inst_style)
                {
                    continue;
                }
                let abs = place_pin(lp.at, (sx, sy, srot), mirror.as_deref());
                let node = self.node_at(sheet, abs);
                self.pin_points.insert((sheet, abs));
                let pin_idx = pins.len();
                #[allow(clippy::cast_precision_loss, reason = "um coords fit f64 exactly")]
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
                            name: normalize_label(lib.power_net_name(lp, &value)),
                            scope: power_scope,
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
            .is_some_and(|v| v.eq_ignore_ascii_case("yes") || v == "true");

        for (flag, key) in [
            ("in_bom", crate::dnp::EXCLUDED_FROM_BOM_KEY),
            ("on_board", crate::dnp::EXCLUDED_FROM_BOARD_KEY),
        ] {
            if (sym.find_value(flag)).is_some_and(|v| v.eq_ignore_ascii_case("no")) {
                properties.push((key.to_string(), String::new()));
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
            dnp,
            pins,
        });
    }

    /// The designator and unit recorded for this symbol on the sheet placement
    /// `inst_path`: its own `(instances ...)` entry, else (KiCad 6) the root
    /// file's `symbol_instances` entry. `None`s for a flat or unannotated
    /// file, where the symbol's own properties apply.
    fn placement_entry(&self, sym: &List, inst_path: &str) -> (Option<String>, Option<i64>) {
        if let Some(entry) = instance_path(sym, inst_path) {
            return (entry.find_value("reference"), entry.find_i64("unit"));
        }
        self.legacy_instance(sym, inst_path)
            .map_or((None, None), |(r, u)| (Some(r.clone()), *u))
    }

    /// The designator and unit a KiCad 6 root file's `symbol_instances` table
    /// gives this symbol on the sheet placement `inst_path` (`/rootuuid` for
    /// the root, `/rootuuid/sheetuuid/...` below it). A reused sheet lists one
    /// entry per placement; the symbol's own `Reference` property is only the
    /// last annotation written, so every placement but one would otherwise
    /// carry the wrong designator.
    fn legacy_instance(&self, sym: &List, inst_path: &str) -> Option<&(String, Option<i64>)> {
        if self.legacy_instances.is_empty() {
            return None;
        }
        let uuid = sym.find_value("uuid")?;
        let below_root = inst_path
            .strip_prefix('/')?
            .strip_prefix(self.root_uuid.as_str())?;
        self.legacy_instances.get(&format!("{below_root}/{uuid}"))
    }

    /// Expand a bus label on `sheet`, resolving any bus-alias reference against
    /// the aliases recorded for that sheet. A bare alias token inside a group
    /// bus (`MEM{ADDR}` where `ADDR` is an alias) expands to the alias's
    /// members, each token of which is itself expanded (an alias member can be a
    /// vector like `A[7..0]`). Falls back to plain expansion when the name uses
    /// no alias.
    fn expand_bus_on_sheet(&self, sheet: SheetId, name: &str) -> Option<Vec<String>> {
        let aliases = &self.bus_aliases;
        let project = &self.design_bus_aliases;
        let resolve = move |tok: &str| -> Option<Vec<String>> {
            let members = aliases
                .get(&(sheet, tok.to_string()))
                .or_else(|| project.get(tok))?;
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
        // Expand the label as written, then normalise each member exactly as
        // a plain label is normalised. Normalising the whole bus text first
        // would leave alias members (`~{ALERT}`, `NAFE_{~{CS}}`) spelled
        // differently from the same-named labels they must join, and would
        // re-normalise members already normalised, which is not idempotent
        // for nested markup.
        let members = expand_bus_aliased(name, &resolve)?;
        Some(members.iter().map(|m| normalize_label(m)).collect())
    }

    fn add_label(&mut self, sheet: SheetId, lbl: &List, scope: NameScope) {
        if let (Some(name), Some(at)) = (lbl.arg_value(0), at_xy(lbl)) {
            let node = self.node_at(sheet, at);
            let raw = name;
            let name = normalize_label(&raw);
            // A label whose text is a bus expression (vector `D[0..7]` or group
            // `NAME{...}`) names a *bus*, not a single net. On a flat sheet the
            // bus is cosmetic: every member already reaches the bus via its own
            // labelled wire, and those member labels (`D0`, `D1`, …) unify by
            // name on their own. So a bus label imposes no single net name (it
            // must not, or `D[0..7]` would become one giant net). We record the
            // bus label's members and position so a sheet pin sitting on this
            // bus under a different name can map its members positionally.
            if let Some(members) = self.expand_bus_on_sheet(sheet, &raw) {
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
                let locals = group_locals(&raw, &members);
                self.bus_labels.push((sheet, at, members, locals));
            } else {
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
                self.pin_points.insert((sheet, at));
                let raw = name;
                let name = normalize_label(&raw);
                if let Some(members) = self.expand_bus_on_sheet(sheet, &raw) {
                    // A bus sheet pin connects member-wise to the child's
                    // matching bus hierarchical label. Recorded and resolved in
                    // `finish` (parent side): the pin may sit on a parent bus
                    // carrying *differently-named* members (`DQ[0..31]` feeding
                    // a `DPC[0..31]` pin), which map positionally by index.
                    let _ = node;
                    self.bus_boundaries.push(BusBoundary {
                        sheet,
                        at,
                        own_locals: group_locals(&raw, &members),
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
            // Cycle and depth guards BEFORE the read: a sheet that references
            // itself (or a pair that reference each other) would otherwise
            // recurse until the stack aborts the whole process, which on
            // `hauksbee serve` is a denial of service rather than a bad parse.
            // Canonicalize so `sub/../a.kicad_sch` and `a.kicad_sch` are one
            // key; fall back to the joined path when the file does not exist,
            // which the read below then reports as a missing sub-sheet.
            let key = child.canonicalize().unwrap_or_else(|_| child.clone());
            let is_cycle = self.ancestor_sheets.contains(&key);
            if self.sheet_depth < MAX_SHEET_DEPTH && !is_cycle {
                // A sheet that cannot be read is left out and said so (see
                // `load_child_sheet`); the rest of the hierarchy is still read.
                let loaded = match self.load_child_sheet(sub, &file, &child, &key) {
                    Ok(doc) => Some(doc),
                    Err(note) => {
                        if !self.unread_files.contains(&key) {
                            self.unread_files.push(key.clone());
                            self.unread_sheets.push(note);
                        }
                        None
                    }
                };
                if let Some(doc) = loaded {
                    let child_dir = child.parent().map(Path::to_path_buf);
                    self.sheet_depth += 1;
                    self.ancestor_sheets.push(key);
                    let parent = self
                        .sheet_display
                        .get(sheet as usize)
                        .map_or("/", String::as_str);
                    let title = sheet_property(sub, "Sheetname")
                        .or_else(|| sheet_property(sub, "Sheet name"))
                        .unwrap_or_else(|| sheet_uuid.clone());
                    self.pending_display = Some(format!("{parent}{title}/"));
                    let result = self.add_sheet_doc(&doc, &child_path, child_dir.as_deref(), None);
                    self.ancestor_sheets.pop();
                    self.sheet_depth -= 1;
                    result?;
                }
            }
        }
        Ok(())
    }

    /// The file name of the sheet currently being expanded: the one whose
    /// `(sheet ...)` entry is being followed.
    fn current_sheet_file(&self) -> String {
        self.ancestor_sheets
            .last()
            .map_or_else(|| "the root schematic".to_string(), |p| file_label(p))
    }

    /// The error for a hierarchy that expands past [`MAX_SHEET_INSTANCES`],
    /// naming the sheet chain that was being expanded when the cap was hit.
    fn too_many_instances(&self) -> ExtractError {
        let names: Vec<String> = self.ancestor_sheets.iter().map(|p| file_label(p)).collect();
        // A deep chain keeps its top and its tail, the two ends a user acts on.
        let chain = if names.len() > 8 {
            format!(
                "{} > ... ({} sheets) ... > {}",
                names[..3].join(" > "),
                names.len() - 6,
                names[names.len() - 3..].join(" > ")
            )
        } else {
            names.join(" > ")
        };
        let at = self
            .pending_display
            .as_deref()
            .map_or(String::new(), |d| format!(" (sheet path {d})"));
        ExtractError::Corrupt(format!(
            "this schematic hierarchy expands to more than {MAX_SHEET_INSTANCES} sheet \
             instances, so hauksbee stopped reading it. It was still expanding \
             {chain}{at} when it hit the limit. Every placement of a sub-sheet is a \
             separate copy of its parts, so a sheet that places the same child \
             several times, inside a child that is itself placed several times, \
             multiplies out of control (two placements per level over 14 levels is \
             already 16384). Check the (sheet ... Sheetfile) references along that \
             chain for a sheet placed more often than intended, or open a smaller \
             part of the design directly."
        ))
    }

    /// A sub-sheet's parsed document for the recursion, or the coverage note
    /// for a sheet that cannot be read: missing, unreadable, cut short, empty or
    /// not a schematic. Such a sheet is left out of the read, never silently: the
    /// note names the child file and the sheet that references it, says that
    /// nothing on the sheet was checked, and says how to cover it. The caller
    /// carries the note to the verdict, which then cannot read as unconditionally
    /// clean ([`extract_from_path_with_notes`]).
    fn load_child_sheet(
        &self,
        sub: &List,
        file: &str,
        child: &Path,
        key: &Path,
    ) -> Result<Rc<Document>, String> {
        if let Some(doc) = self.parsed_sheets.get(key) {
            return Ok(Rc::clone(doc));
        }
        let parent = self.current_sheet_file();
        let name = sheet_property(sub, "Sheetname").or_else(|| sheet_property(sub, "Sheet name"));
        let subject = name.map_or_else(
            || format!("'{file}' (a sheet in {parent})"),
            |n| format!("'{file}' (the sheet '{n}' in {parent})"),
        );
        let consequence = format!(
            "That sheet was NOT read, so nothing on it was checked and a net that \
             runs to it may look unconnected. Put a good copy of '{file}' next to \
             {parent} (restore it from version control, or re-save it from KiCad), \
             or in KiCad open {parent}, select the sheet and correct its File name, \
             then run again to cover it."
        );
        let note = |what: String| Err(crate::unread_sheet_note(&subject, &what));
        let text = match crate::read_regular_string(child) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return note(format!(
                    "the file does not exist (looked for {}). {consequence}",
                    child.display()
                ));
            }
            Err(e) => return note(format!("the file cannot be read ({e}). {consequence}")),
        };
        let doc = match forge_sexpr::parse(&text) {
            Ok(d) => d,
            Err(e) => {
                return note(format!(
                    "the file is damaged or cut short and cannot be parsed: {e}. {consequence}"
                ));
            }
        };
        match doc.root().and_then(List::name) {
            Some("kicad_sch") => Ok(Rc::new(doc)),
            Some(other) => note(format!(
                "the file is not a KiCad schematic: it starts with ({other} ...) where a \
                 (kicad_sch ...) sheet was expected. {consequence}"
            )),
            None => note(format!(
                "the file is empty or is not a KiCad schematic: it has no (kicad_sch ...) \
                 section. {consequence}"
            )),
        }
    }

    /// A sub-sheet's parsed document: from the hierarchy pre-pass when it ran,
    /// else read and parsed now. `None` when it cannot be read or parsed.
    fn sheet_doc(&self, path: &Path, key: &Path) -> Option<Rc<Document>> {
        if let Some(doc) = self.parsed_sheets.get(key) {
            return Some(Rc::clone(doc));
        }
        let text = crate::read_regular_string(path).ok()?;
        // Only a real `(kicad_sch ...)` is cached: an empty or foreign file
        // must reach `load_child_sheet`, which names it, not be waved through.
        forge_sexpr::parse(&text)
            .ok()
            .filter(|d| d.root().and_then(List::name) == Some("kicad_sch"))
            .map(Rc::new)
    }

    /// Parse every sheet file the root reaches, once each, and gather their
    /// bus aliases into [`Self::design_bus_aliases`] (depth-first in
    /// `(sheet ...)` order; a name already defined keeps its first
    /// definition). Runs before the netlist pass because a sheet may use an
    /// alias that only a later sheet defines.
    fn scan_hierarchy(&mut self, root: &Document, base_dir: Option<&Path>) {
        let Some(base_dir) = base_dir else { return };
        // Record a document's aliases; return its sub-sheet files, in order.
        let visit = |this: &mut Self, doc: &Document, dir: &Path| -> Vec<PathBuf> {
            let Some(node) = doc.root() else {
                return Vec::new();
            };
            for (name, members) in bus_alias_defs(node) {
                this.design_bus_aliases.entry(name).or_insert(members);
            }
            node.find_all("sheet")
                .filter_map(|sub| sheet_property(sub, "Sheetfile"))
                .map(|file| dir.join(file))
                .collect()
        };
        let mut pending: Vec<PathBuf> = visit(self, root, base_dir);
        pending.reverse();
        while let Some(child) = pending.pop() {
            if self.parsed_sheets.len() >= MAX_HIERARCHY_FILES {
                break;
            }
            let key = child.canonicalize().unwrap_or_else(|_| child.clone());
            if self.parsed_sheets.contains_key(&key) {
                continue;
            }
            let Some(doc) = self.sheet_doc(&child, &key) else {
                continue;
            };
            self.parsed_sheets.insert(key, Rc::clone(&doc));
            let dir = child.parent().map_or_else(PathBuf::new, Path::to_path_buf);
            let mut next = visit(self, &doc, &dir);
            next.reverse();
            pending.extend(next);
        }
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
                let key = (sheet, p);
                let joins = self.junctions.contains(&key)
                    || (!self.wire_ends.contains(&key)
                        && !self.pin_points.contains(&key)
                        && self.claims.get(&key) == Some(&1));
                if p == a || p == b || !joins {
                    continue;
                }
                if point_strictly_inside(p, a, b) {
                    self.uf.union(seg_node, node);
                }
            }
        }
    }

    /// Resolve bus-valued sheet pins / hierarchical labels into member-wise
    /// hierarchical connections.
    ///
    /// This follows eeschema's bus model. Every bus item on one connected bus
    /// (a bus label, a bus hierarchical label, a bus sheet pin) is part of one
    /// bus subgraph whose *driver* names its members: a bus label first, then
    /// a hierarchical label, then a sheet pin (KiCad's driver priority). Each
    /// boundary on the subgraph takes the driver's member that matches each
    /// of its own (`matchBusMember`): by member name when both are group
    /// buses (`{Q[0..7]}` on `A{DIR[0..7] Q[0..7]}` takes `A.Q0..A.Q7`), by
    /// index otherwise (`DPC[0..31]` on `DQ[0..31]`). The driver's members
    /// join same-named nets on the sheet, and the boundary keys the
    /// hierarchical join under its own member names so it meets the other
    /// side of the sheet boundary.
    ///
    /// A bus driven only by sheet pins has no label, so its members join a
    /// same-named label on the sheet but not each other's buses: two
    /// placements of one sheet whose `Control{CTL}` pins each sit on their own
    /// unlabelled bus stay apart.
    fn resolve_bus_boundaries(&mut self) {
        let boundaries = std::mem::take(&mut self.bus_boundaries);
        // Group boundaries by bus subgraph: same sheet and reachable over bus
        // segments. A boundary off every bus segment is its own group.
        let mut groups: Vec<(SheetId, BTreeSet<Pt>, Vec<usize>)> = Vec::new();
        for (i, b) in boundaries.iter().enumerate() {
            let found = groups.iter().position(|(gs, reach, _)| {
                *gs == b.sheet && self.point_on_reach(b.sheet, b.at, reach)
            });
            if let Some(g) = found {
                groups[g].2.push(i);
            } else {
                groups.push((b.sheet, self.bus_reach(b.sheet, b.at), vec![i]));
            }
        }
        for (sheet, reach, members) in &groups {
            let sheet = *sheet;
            // The driver: a bus label on the subgraph, else the first
            // hierarchical label, else the first sheet pin.
            let label = self
                .bus_labels
                .iter()
                .find(|(s, at, _, _)| *s == sheet && self.point_on_reach(sheet, *at, reach))
                .map(|(_, _, m, l)| (m.clone(), l.clone()));
            let first_of = |parent_side: bool| {
                members
                    .iter()
                    .map(|&i| &boundaries[i])
                    .find(|b| b.parent_side == parent_side)
            };
            let (driver_members, driver_locals, kind) = if let Some((m, l)) = label {
                (m, l, AnchorKind::Hier)
            } else if let Some(b) = first_of(false) {
                (
                    b.own_members.clone(),
                    b.own_locals.clone(),
                    AnchorKind::Hier,
                )
            } else if let Some(b) = first_of(true) {
                (
                    b.own_members.clone(),
                    b.own_locals.clone(),
                    AnchorKind::SheetPin,
                )
            } else {
                continue;
            };
            // One node per driver member, bound to the member's net on this
            // sheet by name. Ranked below the member's own label for naming:
            // it stands in for a sheet pin / hierarchical label.
            let driver_nodes: Vec<usize> = driver_members
                .iter()
                .map(|m| {
                    let node = self.uf.make();
                    self.push_anchor(
                        sheet,
                        NamedAnchor {
                            name: m.clone(),
                            scope: NameScope::Local,
                            kind,
                            node,
                        },
                    );
                    node
                })
                .collect();
            let by_local: Option<HashMap<&str, usize>> = driver_locals
                .as_ref()
                .map(|dl| dl.iter().map(String::as_str).zip(0..).collect());
            for &bi in members {
                let b = &boundaries[bi];
                for (i, own) in b.own_members.iter().enumerate() {
                    let matched = match (&by_local, &b.own_locals) {
                        (Some(map), Some(own_locals)) => {
                            own_locals.get(i).and_then(|l| map.get(l.as_str()).copied())
                        }
                        _ => (i < driver_nodes.len()).then_some(i),
                    };
                    // A member the driver does not carry keeps its own name
                    // (a malformed or mismatched bus).
                    let node = if let Some(j) = matched {
                        driver_nodes[j]
                    } else {
                        let node = self.uf.make();
                        self.push_anchor(
                            sheet,
                            NamedAnchor {
                                name: own.clone(),
                                scope: NameScope::Local,
                                kind,
                                node,
                            },
                        );
                        node
                    };
                    let key = (b.path.clone(), own.clone(), node);
                    if b.parent_side {
                        self.hier_parent.push(key);
                    } else {
                        self.hier_child.push(key);
                    }
                }
            }
        }
    }

    /// The bus segment endpoints reachable over `sheet`'s bus segments from
    /// `at` (which lies on an endpoint or the interior of one). Empty when
    /// `at` touches no bus segment.
    fn bus_reach(&self, sheet: SheetId, at: Pt) -> BTreeSet<Pt> {
        let segs: Vec<[Pt; 2]> = self
            .segments
            .iter()
            .filter(|(s, _, _, is_bus)| *s == sheet && *is_bus)
            .map(|(_, a, b, _)| [*a, *b])
            .collect();
        let mut reached: BTreeSet<Pt> = BTreeSet::new();
        let mut frontier: Vec<Pt> = vec![at];
        while let Some(p) = frontier.pop() {
            for &seg in &segs {
                let [a, b] = seg;
                if a == p || b == p || point_strictly_inside(p, a, b) {
                    for q in seg {
                        if reached.insert(q) {
                            frontier.push(q);
                        }
                    }
                }
            }
        }
        reached
    }

    /// Whether `pt` lies on the bus subgraph `reach` (from [`Self::bus_reach`])
    /// on `sheet`: at one of its endpoints or inside one of its segments.
    fn point_on_reach(&self, sheet: SheetId, pt: Pt, reach: &BTreeSet<Pt>) -> bool {
        reach.contains(&pt)
            || self.segments.iter().any(|&(s, a, b, is_bus)| {
                s == sheet
                    && is_bus
                    && reach.contains(&a)
                    && reach.contains(&b)
                    && point_strictly_inside(pt, a, b)
            })
    }

    /// KiCad names a net by a sheet-scoped label (local or hierarchical)
    /// with the sheet's path whenever that label text is not unique to one
    /// net: a sub-sheet instantiated several times carries the same local
    /// label in every instance, and each instance is its own net
    /// (`/IO Banks/Bank A/V_CLAMP`, `/IO Banks/Bank B/V_CLAMP`, ...). Bare,
    /// those names collide, and anything keyed by net name merges distinct
    /// nets. Globally-scoped names (global labels, power symbols) stay bare.
    fn qualify_sheet_scoped_names(
        &self,
        net_name: &mut BTreeMap<usize, String>,
        winner: &HashMap<usize, (u8, String, usize)>,
    ) {
        let mut uses: HashMap<&str, usize> = HashMap::new();
        for n in net_name.values() {
            *uses.entry(n.as_str()).or_default() += 1;
        }
        let renames: Vec<(usize, String)> = net_name
            .iter()
            .filter(|(_, n)| uses.get(n.as_str()).copied().unwrap_or(0) > 1)
            .filter_map(|(root, n)| {
                let &(_, _, i) = winner.get(root)?;
                let a = &self.anchors[i];
                if !matches!(
                    a.kind,
                    AnchorKind::Local | AnchorKind::Hier | AnchorKind::SheetPin
                ) {
                    return None;
                }
                let display = self.sheet_display.get(self.anchor_sheet[i] as usize)?;
                Some((*root, format!("{display}{n}")))
            })
            .collect();
        for (root, name) in renames {
            net_name.insert(root, name);
        }
    }

    /// Resolve the union-find into nets, name them KiCad-style, and assign net
    /// ids to component pins.
    #[allow(clippy::too_many_lines, reason = "sequential netlist phases")]
    fn finish(mut self, name: String) -> ExtractedBoard {
        // 0. Mid-span incidence. KiCad joins a label lying *on* a wire (or
        //    bus) segment, not only at its endpoints: a net label placed
        //    mid-span on a wire is electrically on that wire. Almost
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

        // 2. Named-anchor unification. Globals and hierarchical-by-name unify
        //    across the whole design; locals unify only among anchors recorded
        //    on the SAME sheet, because two sheets can legitimately carry the
        //    same local label text meaning different nets. Sheet identity for a
        //    local comes from the parallel record its node was created with.
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
        // Within one sheet instance every name joins, whatever its kind: a
        // local label meets a same-named global label, power symbol or
        // hierarchical label on its own sheet (kicad-cli netlists), though
        // never a local label on another sheet.
        // A sheet-pin-driven bus member is not a label: it joins the
        // same-named net but never directly another such member, so those
        // are folded in after every labelled name is keyed.
        let mut local_by_key: HashMap<(SheetId, String), usize> = HashMap::new();
        for (i, a) in self.anchors.iter().enumerate() {
            if a.kind == AnchorKind::SheetPin {
                continue;
            }
            let key = (self.anchor_sheet[i], a.name.clone());
            match local_by_key.get(&key) {
                Some(&first) => self.uf.union(first, a.node),
                None => {
                    local_by_key.insert(key, a.node);
                }
            }
        }
        for (i, a) in self.anchors.iter().enumerate() {
            if a.kind != AnchorKind::SheetPin {
                continue;
            }
            if let Some(&named) = local_by_key.get(&(self.anchor_sheet[i], a.name.clone())) {
                self.uf.union(named, a.node);
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
        let mut anchor_name_of_root: HashMap<usize, (u8, String, usize)> = HashMap::new();
        for (i, a) in self.anchors.iter().enumerate() {
            let r = self.uf.find(a.node);
            let prio = a.kind.prio();
            anchor_name_of_root
                .entry(r)
                .and_modify(|e| {
                    if prio > e.0 || (prio == e.0 && a.name < e.1) {
                        *e = (prio, a.name.clone(), i);
                    }
                })
                .or_insert_with(|| (prio, a.name.clone(), i));
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
            let name = if let Some((_, n, _)) = anchor_name_of_root.get(root) {
                n.clone()
            } else {
                synth_name(&self.components, &self.pin_sites, pin_idxs)
            };
            net_name.insert(*root, name);
        }
        self.qualify_sheet_scoped_names(&mut net_name, &anchor_name_of_root);

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
            nets.push(Net::new(id, net_name[&r].clone()));
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

        ExtractedBoard {
            name,
            nets,
            components: self.components,
        }
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
            if let Some(idx) = prior {
                match (deduped[idx].net, p.net) {
                    (None, other) => deduped[idx].net = other,
                    // Same physical pin on two nets: bridge them, as above.
                    (Some(a), Some(b)) if a != b => bridged.push((a, b)),
                    _ => {}
                }
            } else {
                if !p.number.is_empty() {
                    seen.insert(p.number.clone(), deduped.len());
                }
                deduped.push(p);
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
/// local frame, its number, name, electrical type, and which unit and body
/// style it belongs to (0 = common to every unit / style).
struct LibPin {
    at: (f64, f64, f64),
    number: String,
    name: String,
    etype: String,
    unit: u32,
    style: u32,
    /// Hidden pin. A hidden `power_in` pin auto-connects to the global power
    /// net named after the pin's *name* (KiCad's implicit power connection),
    /// even with no wire drawn to it.
    hidden: bool,
}

struct LibDef {
    pins: Vec<LibPin>,
    is_power: bool,
    /// Reach of the net a power symbol names: `(power local)` (KiCad 9+) is
    /// scoped to its sheet, any other power symbol is global.
    power_scope: NameScope,
    /// The library symbol's descriptive text ([`LIB_TEXT_PROPERTIES`]): the
    /// only place a part's `Description` or `ki_keywords` live when the placed
    /// instance does not repeat them.
    text: Vec<(String, String)>,
}

/// The placed instance often omits the descriptive text the library symbol
/// carries (KiCad stores `ki_keywords` only there, and a symbol edited in place
/// keeps its Description in the embedded definition): carry it over so checks
/// that read words see the same part the designer picked. An instance's own,
/// non-empty property wins.
fn carry_library_text(properties: &mut Vec<(String, String)>, def: Option<&LibDef>) {
    for (k, v) in def.map_or(&[][..], |d| d.text.as_slice()) {
        if !properties.iter().any(|(pk, pv)| pk == k && !pv.is_empty()) {
            properties.retain(|(pk, _)| pk != k);
            properties.push((k.clone(), v.clone()));
        }
    }
}

/// Library-symbol properties that describe the part in words and so are
/// carried onto the placed component when the instance lacks them.
const LIB_TEXT_PROPERTIES: [&str; 3] = ["Description", "ki_keywords", "ki_description"];

/// All lib symbol definitions embedded in one schematic, indexed by `lib_id`.
struct LibSymbols {
    defs: HashMap<String, LibDef>,
    /// The file predates KiCad 8's format, where a power symbol's hidden
    /// `power_in` pin name, not its Value, names the net (see
    /// [`LEGACY_POWER_NAME_BEFORE`]).
    legacy_power_names: bool,
}

/// The first `.kicad_sch` format version (KiCad 8.0) whose power symbols are
/// named by their Value. In older files a power symbol whose Value was edited
/// away from its hidden pin's name (`+3.3V` on a `+3V3` pin) still names the
/// pin's net: `kicad-cli sch export netlist` joins it with every `+3V3`
/// symbol and names the net `+3V3`. Files from KiCad 8 on carry `~` or an
/// empty pin name and the Value decides.
const LEGACY_POWER_NAME_BEFORE: i64 = 20_231_120;

impl LibSymbols {
    fn parse(root: &List) -> Self {
        let mut defs = HashMap::new();
        if let Some(lib) = root.find("lib_symbols") {
            for sym in lib.find_all("symbol") {
                let Some(lib_id) = sym.arg_value(0) else {
                    continue;
                };
                let power = sym.find("power");
                let is_power = power.is_some();
                let power_scope = match power.and_then(|p| p.arg_value(0)).as_deref() {
                    Some("local") => NameScope::Local,
                    _ => NameScope::Global,
                };
                let mut pins = Vec::new();
                // Pins live in sub-symbol units named "<base>_<unit>_<style>".
                // The top-level symbol can also directly hold pins (rare).
                collect_pins(sym, &lib_id, &mut pins);
                for unit_sym in sym.find_all("symbol") {
                    let (unit, style) = unit_sym
                        .arg_value(0)
                        .as_deref()
                        .and_then(unit_of)
                        .unwrap_or((0, 0));
                    for p in unit_sym.find_all("pin") {
                        if let Some(mut lp) = lib_pin(p) {
                            lp.unit = unit;
                            lp.style = style;
                            pins.push(lp);
                        }
                    }
                }
                let text = (sym.find_all("property"))
                    .filter_map(|prop| Some((prop.arg_value(0)?, prop.arg_value(1)?)))
                    .filter(|(k, v)| LIB_TEXT_PROPERTIES.contains(&k.as_str()) && !v.is_empty())
                    .collect();
                defs.insert(
                    lib_id,
                    LibDef {
                        pins,
                        is_power,
                        power_scope,
                        text,
                    },
                );
            }
        }
        let legacy_power_names = root
            .find("version")
            .and_then(|v| v.arg_value(0))
            .and_then(|v| v.parse::<i64>().ok())
            .is_some_and(|v| v < LEGACY_POWER_NAME_BEFORE);
        Self {
            defs,
            legacy_power_names,
        }
    }

    /// The definition a symbol instance draws. An instance whose symbol was
    /// edited away from the library carries `(lib_name "X_1")` and its
    /// embedded definition is stored under that name, not under `lib_id`;
    /// looking up `lib_id` alone found nothing and the part lost every pin.
    fn for_instance(&self, sym: &List, lib_id: &str) -> Option<&LibDef> {
        sym.find_value("lib_name")
            .and_then(|name| self.defs.get(&name))
            .or_else(|| self.defs.get(lib_id))
    }

    /// The net a power symbol's `power_in` pin names: its Value, or in a
    /// pre-KiCad-8 file its hidden pin's name.
    fn power_net_name<'a>(&self, pin: &'a LibPin, value: &'a str) -> &'a str {
        let legacy = self.legacy_power_names && pin.hidden;
        if legacy && !pin.name.is_empty() && pin.name != "~" {
            &pin.name
        } else {
            value
        }
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

/// Parse `<base>_<unit>_<bodystyle>` → (unit, body style). Returns None when
/// the name doesn't carry both suffixes.
fn unit_of(sub_name: &str) -> Option<(u32, u32)> {
    let mut parts = sub_name.rsplitn(3, '_');
    let style = parts.next()?.parse().ok()?;
    let unit = parts.next()?.parse().ok()?;
    Some((unit, style))
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
        style: 0,
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
/// is both rotated and mirrored, where mirroring first swaps the part's two
/// pins.
fn place_pin(local: (f64, f64, f64), inst: (f64, f64, f64), mirror: Option<&str>) -> Pt {
    // Library symbols are drawn in a y-*up* frame; the schematic canvas is
    // y-*down*. Flip the pin's local y first so "top of symbol" stays at the
    // top once placed. (Skipping this silently swaps the two pins of every
    // vertical 2-pin part.)
    let (lx, ly) = (local.0, -local.1);
    let theta = inst.2.to_radians();
    let (s, c) = theta.sin_cos();
    let mut rx = lx.mul_add(c, ly * s);
    let mut ry = (-lx).mul_add(s, ly * c);
    match mirror {
        Some("x") => ry = -ry,
        Some("y") => rx = -rx,
        _ => {}
    }
    snap(inst.0 + rx, inst.1 + ry)
}

/// `(at x y ...)` → snapped point.
fn at_xy(list: &List) -> Option<Pt> {
    let at = list.find("at")?;
    Some(snap(at.arg_f64(0)?, at.arg_f64(1)?))
}

/// True if `p` lies strictly between `a` and `b` on the segment a–b (collinear,
/// excluding the endpoints). Coordinates are integer micrometres, so the cross
/// product is exact.
const fn point_strictly_inside(p: Pt, a: Pt, b: Pt) -> bool {
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
/// Where a group bus `PREFIX{...}` opens, or `None` when `name` is not one.
///
/// KiCad text markup, overbar `~{X}`, subscript `_{X}`, superscript `^{X}`,
/// is NOT bus syntax: the char immediately before `{` is the markup marker
/// and KiCad keeps the literal text (`~{RST}`) as the net name. Treating it
/// as a group bus (prefix `~`, member `~.RST`) silently splits ubiquitous
/// active-low nets like `~{RESET}` / `~{CS}`. A named escape (`{slash}`,
/// `{brace}`, ...) is not a group either: bus text is expanded before it is
/// unescaped. Either may also appear in a group's prefix.
fn group_bus_open(name: &str) -> Option<usize> {
    if !name.ends_with('}') {
        return None;
    }
    let bytes = name.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        let markup = i > 0 && matches!(bytes[i - 1], b'~' | b'_' | b'^');
        if !markup && !crate::netname::starts_with_named_escape(&name[i..]) {
            return Some(i);
        }
        // Step over the markup or escape so a group whose prefix carries one
        // (`I_{2}C1{SCL SDA}`) still opens at its own brace.
        i += name[i..].find('}')? + 1;
    }
    None
}

/// The group-local name of each member of a group bus `PREFIX{...}`: the
/// member with its `PREFIX.` qualifier removed (`A{DIR[0..1]}` carries
/// `A.DIR0`, `A.DIR1`, whose local names are `DIR0`, `DIR1`). `None` when
/// `raw` (the label text as written) is not a group bus.
fn group_locals(raw: &str, members: &[String]) -> Option<Vec<String>> {
    let open = group_bus_open(raw)?;
    let prefix = normalize_label(&raw[..open]);
    let qualifier = format!("{prefix}.");
    Some(
        members
            .iter()
            .map(|m| {
                if prefix.is_empty() {
                    m.clone()
                } else {
                    m.strip_prefix(&qualifier).unwrap_or(m).to_string()
                }
            })
            .collect(),
    )
}

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
    if let Some(open) = group_bus_open(name) {
        let prefix = &name[..open];
        let inner = &name[open + 1..name.len() - 1];
        let mut out = Vec::new();
        // Members are separated by whitespace or commas (`USB{VBUS, CC1}`),
        // as in eeschema's bus-group parser.
        for tok in inner
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|t| !t.is_empty())
        {
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

/// This symbol's `(path .. (reference ..) (unit ..))` entry *in the sheet
/// instance we are expanding*. A reused sheet lists one per instantiation; we
/// pick the one whose path equals the current instance path. Returns None when
/// the symbol has no instances block (older/flat files).
fn instance_path<'a>(sym: &'a List, inst_path: &str) -> Option<&'a List> {
    sym.find("instances")?
        .find_all("project")
        .flat_map(|project| project.find_all("path"))
        .find(|path| path.arg_value(0).as_deref() == Some(inst_path))
}

/// The root file's KiCad 6 `(symbol_instances (path P (reference R) (unit U)))`
/// table, keyed by path.
fn legacy_symbol_instances(root: &List) -> HashMap<String, (String, Option<i64>)> {
    let Some(table) = root.find("symbol_instances") else {
        return HashMap::new();
    };
    table
        .find_all("path")
        .filter_map(|path| {
            let key = path.arg_value(0)?;
            Some((key, (path.find_value("reference")?, path.find_i64("unit"))))
        })
        .collect()
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

/// A `(sheet ...)` property value by key (Sheetname/Sheetfile). KiCad 6
/// writes these keys with a space (`"Sheet file"`, `"Sheet name"`) and
/// KiCad 7 onward without, so keys compare with spaces ignored; matching only
/// one spelling silently dropped every sub-sheet of a KiCad 6 hierarchy.
/// A sheet file's own name, for messages (the directory is the user's already).
fn file_label(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn sheet_property(sheet: &List, key: &str) -> Option<String> {
    let bare = |k: &str| k.chars().filter(|c| *c != ' ').collect::<String>();
    let key = bare(key);
    for prop in sheet.find_all("property") {
        if prop.arg_value(0).is_some_and(|k| bare(&k) == key) {
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
    use crate::testutil::part;
    use crate::{Component, Pin};

    fn pin(number: &str, net: i64) -> Pin {
        crate::testutil::pin(number, Some(net))
    }

    fn comp(reference: &str, pins: Vec<Pin>) -> Component {
        part(reference, "", "", "", pins)
    }

    #[test]
    fn empty_pin_numbers_are_never_bridged_or_dropped() {
        // Two blank-numbered pins on two DIFFERENT nets must each survive and
        // must NOT bridge those nets; deduping them by number merges the second
        // pin's net onto the first. (A lib pin with no `(number)` yields "".)
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
    fn group_members_may_be_comma_separated() {
        // `USB{VBUS, CC1}` is the same bus as `USB{VBUS CC1}`.
        assert_eq!(
            expand_bus("USB{VBUS, CC1}"),
            Some(vec!["USB.VBUS".to_string(), "USB.CC1".to_string()])
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
