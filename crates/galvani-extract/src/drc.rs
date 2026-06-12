//! Copper short / clearance detection (geometric DRC).
//!
//! Galvani simulates from a real layout, so two pieces of copper that touch
//! when they belong to different nets are an *electrical* fact the simulation
//! must know about: a solder bridge, an overlapping pad, a pour eating into a
//! track. This module finds those from geometry alone and hands them to the
//! engine, which can then merge the shorted nets and show the consequence.
//!
//! ## What is checked
//!
//! Every conductive primitive on a copper layer is reduced to one of three
//! shapes, all in board millimetres:
//!   - **Capsule** (a "stadium"): a track segment of finite width, or an arc
//!     sampled into a short capsule chain. Distance to anything else is the
//!     segment-to-segment distance minus both half-widths.
//!   - **Disc**: a round pad, or a via / through-hole pad annular ring. Spans
//!     the layers it touches (vias and `*.Cu` pads sit on every copper layer).
//!   - **Polygon**: a rectangular / rounded / oval / custom pad outline, or a
//!     filled zone area. Distance uses closed-polygon edge distance, and a
//!     point-in-polygon test catches full containment.
//!
//! Primitives are bucketed per copper layer and indexed in an [`rstar`]
//! R*-tree, so each one is only distance-tested against neighbours whose
//! bounding boxes are within the clearance window. That keeps an 85 MB board
//! (hundreds of thousands of primitives) to a few seconds instead of the
//! O(n²) blow-up a naive all-pairs sweep would cost.
//!
//! ## Overlap vs clearance
//!
//! For a candidate pair on different nets we compute the signed gap (copper
//! edge to copper edge, already accounting for widths):
//!   - gap `<= 0` → the copper actually intersects: a **short**
//!     ([`ViolationKind::Short`]).
//!   - `0 < gap < clearance` → they do not touch but sit closer than the
//!     design rule allows: a **clearance violation**
//!     ([`ViolationKind::Clearance`]), a near-short risk, lower severity.
//!
//! Clearance is read from the board's design rules when present, else a sane
//! default ([`DEFAULT_CLEARANCE_MM`]).

use std::collections::HashMap;

use forge_sexpr::{Document, List};
use rstar::{RTree, RTreeObject, AABB};
use serde::{Deserialize, Serialize};

/// Default copper-to-copper clearance (mm) when the board states no rule.
pub const DEFAULT_CLEARANCE_MM: f64 = 0.2;

/// Arcs are flattened into this many straight capsule links. Eight keeps the
/// chord error under a few microns for typical track-radius arcs while staying
/// cheap.
const ARC_SEGMENTS: usize = 8;

/// Whether a finding is a true short or only a clearance violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViolationKind {
    /// Copper from two nets physically overlaps (gap <= 0): an electrical short.
    Short,
    /// Copper from two nets is closer than the design clearance but not
    /// touching: a near-short manufacturing risk.
    Clearance,
}

impl ViolationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ViolationKind::Short => "short",
            ViolationKind::Clearance => "clearance",
        }
    }
}

/// What kind of copper primitive was involved in a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemKind {
    Track,
    Arc,
    Via,
    Pad,
    Zone,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Track => "track",
            ItemKind::Arc => "arc",
            ItemKind::Via => "via",
            ItemKind::Pad => "pad",
            ItemKind::Zone => "zone",
        }
    }
}

/// One copper primitive involved in a finding (the human-facing description).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub kind: ItemKind,
    /// Net id this copper belongs to.
    pub net: i64,
    /// Owning component reference for pads (e.g. "U3"), empty otherwise.
    pub owner: String,
}

/// One short / clearance finding between two different nets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrcFinding {
    pub kind: ViolationKind,
    /// The two nets involved (ids), lower id first for stable ordering.
    pub net_a: i64,
    pub net_b: i64,
    /// Net names, matching `net_a` / `net_b`.
    pub net_a_name: String,
    pub net_b_name: String,
    /// Copper layer the violation is on (e.g. "F.Cu").
    pub layer: String,
    /// Representative location (mm), the midpoint of the closest approach.
    pub x: f64,
    pub y: f64,
    /// Signed copper-edge gap (mm). <= 0 for an overlap, the penetration depth
    /// as a negative number; positive for a clearance violation.
    pub gap_mm: f64,
    /// The two primitives that came closest.
    pub item_a: Item,
    pub item_b: Item,
}

/// The full DRC report for a board.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DrcReport {
    /// Clearance rule used (mm).
    pub clearance_mm: f64,
    /// Every finding, shorts and clearance violations together.
    pub findings: Vec<DrcFinding>,
    /// Number of copper primitives indexed (diagnostics / perf reporting).
    pub primitive_count: usize,
}

impl DrcReport {
    /// Only the true overlaps (electrical shorts).
    pub fn shorts(&self) -> impl Iterator<Item = &DrcFinding> {
        self.findings
            .iter()
            .filter(|f| f.kind == ViolationKind::Short)
    }

    /// Only the clearance (near-short) violations.
    pub fn clearance_violations(&self) -> impl Iterator<Item = &DrcFinding> {
        self.findings
            .iter()
            .filter(|f| f.kind == ViolationKind::Clearance)
    }

    pub fn short_count(&self) -> usize {
        self.shorts().count()
    }

    pub fn is_clean(&self) -> bool {
        self.short_count() == 0
    }

    /// Distinct unordered net pairs that are shorted together, as (id, id) with
    /// the lower id first. The engine uses these to merge nets.
    pub fn shorted_net_pairs(&self) -> Vec<(i64, i64)> {
        let mut pairs: Vec<(i64, i64)> = self
            .shorts()
            .map(|f| (f.net_a.min(f.net_b), f.net_a.max(f.net_b)))
            .collect();
        pairs.sort_unstable();
        pairs.dedup();
        pairs
    }
}

// ── Geometry primitives ──────────────────────────────────────────────────────

/// A capsule: a line segment with a radius (half copper width). Tracks and arc
/// links are capsules; a disc is a degenerate capsule with `a == b`.
#[derive(Debug, Clone, Copy)]
struct Capsule {
    ax: f64,
    ay: f64,
    bx: f64,
    by: f64,
    r: f64,
}

/// A primitive's solid shape, all coordinates in board mm.
#[derive(Debug, Clone)]
enum Shape {
    /// Track / arc-link / disc (via, round pad).
    Capsule(Capsule),
    /// Closed polygon outline (rect/oval/custom pad, zone fill). The points are
    /// the vertices in order; `r` is an extra inflation radius (0 for zones,
    /// the corner radius for a roundrect treated as a polygon + radius).
    Polygon { pts: Vec<(f64, f64)>, r: f64 },
}

/// One indexed copper primitive on a single layer.
#[derive(Debug, Clone)]
struct Primitive {
    shape: Shape,
    net: i64,
    kind: ItemKind,
    owner: String,
    /// Axis-aligned bounds (minx, miny, maxx, maxy), already inflated by the
    /// primitive's own radius so a box-overlap query within `clearance` of two
    /// boxes is a superset of the real close pairs.
    bounds: [f64; 4],
    /// Index back into the per-layer primitive vector (set after build).
    idx: usize,
}

impl Primitive {
    fn item(&self) -> Item {
        Item {
            kind: self.kind,
            net: self.net,
            owner: self.owner.clone(),
        }
    }
}

/// An R*-tree leaf: a primitive's bounding box plus its index. We keep the
/// heavy shape data out of the tree and look it up by index, so the tree nodes
/// stay small.
#[derive(Debug, Clone)]
struct Leaf {
    bounds: [f64; 4],
    idx: usize,
}

impl RTreeObject for Leaf {
    type Envelope = AABB<[f64; 2]>;
    fn envelope(&self) -> Self::Envelope {
        AABB::from_corners([self.bounds[0], self.bounds[1]], [self.bounds[2], self.bounds[3]])
    }
}

// ── Distance helpers (all in mm) ─────────────────────────────────────────────

/// Squared distance from point P to segment AB.
fn point_seg_dist2(px: f64, py: f64, ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let dx = bx - ax;
    let dy = by - ay;
    let len2 = dx * dx + dy * dy;
    if len2 <= f64::EPSILON {
        let ex = px - ax;
        let ey = py - ay;
        return ex * ex + ey * ey;
    }
    let t = (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0);
    let cx = ax + t * dx;
    let cy = ay + t * dy;
    let ex = px - cx;
    let ey = py - cy;
    ex * ex + ey * ey
}

/// Minimum distance between two segments (centerlines), 0 if they cross.
fn seg_seg_dist(
    a1: (f64, f64),
    a2: (f64, f64),
    b1: (f64, f64),
    b2: (f64, f64),
) -> f64 {
    if segments_intersect(a1, a2, b1, b2) {
        return 0.0;
    }
    let d = point_seg_dist2(a1.0, a1.1, b1.0, b1.1, b2.0, b2.1)
        .min(point_seg_dist2(a2.0, a2.1, b1.0, b1.1, b2.0, b2.1))
        .min(point_seg_dist2(b1.0, b1.1, a1.0, a1.1, a2.0, a2.1))
        .min(point_seg_dist2(b2.0, b2.1, a1.0, a1.1, a2.0, a2.1));
    d.sqrt()
}

/// Orientation sign of the triplet (p, q, r): >0 ccw, <0 cw, 0 colinear.
fn orient(p: (f64, f64), q: (f64, f64), r: (f64, f64)) -> f64 {
    (q.0 - p.0) * (r.1 - p.1) - (q.1 - p.1) * (r.0 - p.0)
}

fn on_seg(p: (f64, f64), q: (f64, f64), r: (f64, f64)) -> bool {
    q.0 <= p.0.max(r.0) && q.0 >= p.0.min(r.0) && q.1 <= p.1.max(r.1) && q.1 >= p.1.min(r.1)
}

/// Proper / improper segment intersection test.
fn segments_intersect(p1: (f64, f64), p2: (f64, f64), p3: (f64, f64), p4: (f64, f64)) -> bool {
    let d1 = orient(p3, p4, p1);
    let d2 = orient(p3, p4, p2);
    let d3 = orient(p1, p2, p3);
    let d4 = orient(p1, p2, p4);
    if ((d1 > 0.0) != (d2 > 0.0)) && ((d3 > 0.0) != (d4 > 0.0)) {
        return true;
    }
    (d1 == 0.0 && on_seg(p3, p1, p4))
        || (d2 == 0.0 && on_seg(p3, p2, p4))
        || (d3 == 0.0 && on_seg(p1, p3, p2))
        || (d4 == 0.0 && on_seg(p1, p4, p2))
}

/// Even-odd point-in-polygon test.
fn point_in_polygon(px: f64, py: f64, poly: &[(f64, f64)]) -> bool {
    let n = poly.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        if ((yi > py) != (yj > py)) && (px < (xj - xi) * (py - yi) / (yj - yi) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Minimum distance from point P to the closed polygon boundary.
fn point_poly_edge_dist(px: f64, py: f64, poly: &[(f64, f64)]) -> f64 {
    let n = poly.len();
    if n == 0 {
        return f64::INFINITY;
    }
    if n == 1 {
        return ((px - poly[0].0).powi(2) + (py - poly[0].1).powi(2)).sqrt();
    }
    let mut best = f64::INFINITY;
    let mut j = n - 1;
    for i in 0..n {
        best = best.min(point_seg_dist2(px, py, poly[j].0, poly[j].1, poly[i].0, poly[i].1));
        j = i;
    }
    best.sqrt()
}

/// Minimum boundary-to-boundary distance between two polygons (0 if their
/// edges cross). Containment is handled by the caller via point-in-polygon.
fn poly_poly_edge_dist(a: &[(f64, f64)], b: &[(f64, f64)]) -> f64 {
    let mut best = f64::INFINITY;
    let na = a.len();
    let nb = b.len();
    if na < 2 || nb < 2 {
        // Degenerate: fall back to nearest-vertex.
        for &pa in a {
            best = best.min(point_poly_edge_dist(pa.0, pa.1, b));
        }
        return best;
    }
    let mut ja = na - 1;
    for ia in 0..na {
        let a1 = a[ja];
        let a2 = a[ia];
        let mut jb = nb - 1;
        for ib in 0..nb {
            let b1 = b[jb];
            let b2 = b[ib];
            best = best.min(seg_seg_dist(a1, a2, b1, b2));
            jb = ib;
        }
        ja = ia;
    }
    best
}

/// Centerline distance between two capsules' segments (radii subtracted by
/// the caller).
fn capsule_centerline_dist(a: &Capsule, b: &Capsule) -> f64 {
    seg_seg_dist((a.ax, a.ay), (a.bx, a.by), (b.ax, b.ay), (b.bx, b.by))
}

/// Signed copper-edge gap between two primitives. Negative means they overlap
/// (the magnitude is roughly the penetration), and the returned `(x, y)` is a
/// representative point of closest approach.
fn shape_gap(a: &Shape, b: &Shape) -> (f64, (f64, f64)) {
    match (a, b) {
        (Shape::Capsule(ca), Shape::Capsule(cb)) => {
            let d = capsule_centerline_dist(ca, cb) - ca.r - cb.r;
            let mid = (
                (ca.ax + ca.bx + cb.ax + cb.bx) / 4.0,
                (ca.ay + ca.by + cb.ay + cb.by) / 4.0,
            );
            (d, mid)
        }
        (Shape::Capsule(c), Shape::Polygon { pts, r }) | (Shape::Polygon { pts, r }, Shape::Capsule(c)) => {
            // True centerline-to-boundary distance: the capsule segment against
            // every polygon edge (0 if it crosses the boundary). This catches a
            // track passing straight through a pad even when neither endpoint is
            // inside and no vertex is near.
            let seg_a = (c.ax, c.ay);
            let seg_b = (c.bx, c.by);
            let mut best = f64::INFINITY;
            let n = pts.len();
            if n >= 2 {
                let mut j = n - 1;
                for i in 0..n {
                    best = best.min(seg_seg_dist(seg_a, seg_b, pts[j], pts[i]));
                    j = i;
                }
            } else if n == 1 {
                best = point_seg_dist2(pts[0].0, pts[0].1, c.ax, c.ay, c.bx, c.by).sqrt();
            }
            // Containment: either capsule endpoint inside the polygon (the
            // track terminates within the pad / pour copper).
            let contained = point_in_polygon(c.ax, c.ay, pts) || point_in_polygon(c.bx, c.by, pts);
            let gap = if contained {
                // Fully engulfed: a hard overlap regardless of edge distance.
                -(c.r + r).max(0.0) - 1e-6
            } else {
                best - c.r - r
            };
            // Representative point: capsule midpoint (close enough for display).
            (gap, ((c.ax + c.bx) / 2.0, (c.ay + c.by) / 2.0))
        }
        (Shape::Polygon { pts: pa, r: ra }, Shape::Polygon { pts: pb, r: rb }) => {
            let edge = poly_poly_edge_dist(pa, pb) - ra - rb;
            // Containment either way is a hard overlap.
            let contained = pa.first().is_some_and(|&(x, y)| point_in_polygon(x, y, pb))
                || pb.first().is_some_and(|&(x, y)| point_in_polygon(x, y, pa));
            let gap = if contained { edge.min(0.0) - 1e-6 } else { edge };
            let mid = pa
                .first()
                .copied()
                .unwrap_or((0.0, 0.0));
            (gap, mid)
        }
    }
}

// ── Net resolution (mirrors pcb.rs, but standalone) ──────────────────────────

/// A net id table that handles all three KiCad encodings the same way `pcb.rs`
/// does: declared `(net N "name")`, name-only `(net "name")`, and the older
/// `(net N)` + separate `(net_name "name")` on zones.
#[derive(Default)]
struct NetResolver {
    by_id: HashMap<i64, String>,
    by_name: HashMap<String, i64>,
    next_synthetic: i64,
}

impl NetResolver {
    fn from_root(root: &List) -> Self {
        let mut r = NetResolver::default();
        for n in root.find_all("net") {
            match (n.arg_i64(0), n.arg_value(0), n.arg_value(1)) {
                (Some(id), _, name) => r.declare(id, name.unwrap_or_default()),
                (None, Some(name), _) => {
                    r.id_of(&name);
                }
                _ => {}
            }
        }
        r
    }

    fn declare(&mut self, id: i64, name: String) {
        self.by_name.entry(name.clone()).or_insert(id);
        self.by_id.entry(id).or_insert(name);
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

    fn name_of(&self, id: i64) -> String {
        self.by_id.get(&id).cloned().unwrap_or_default()
    }

    /// True for nets that carry no real connectivity: KiCad's auto-generated
    /// `unconnected-(...)` placeholders (one per floating pad) and the empty
    /// net 0. Copper on these cannot form an electrical short, so the sweep
    /// skips them the same way it skips net 0.
    fn is_no_net(&self, id: i64) -> bool {
        id == 0 || self.name_of(id).starts_with("unconnected-")
    }

    /// Resolve a `(net ...)` child of a list to an id, handling the numeric,
    /// name-only, and numeric-with-sibling-`net_name` forms.
    fn net_ref(&mut self, list: &List) -> Option<i64> {
        let net = list.find("net")?;
        // Numeric id form.
        if let Some(id) = net.arg(0).filter(|t| !t.is_string()).and_then(|t| t.as_i64()) {
            // Some zones carry both `(net N)` and `(net_name "X")`; declare the
            // name so reporting has it.
            if let Some(name) = list.find_value("net_name") {
                self.declare(id, name);
            }
            return Some(id);
        }
        // Name-only form.
        let name = net.arg_value(0)?;
        Some(self.id_of(&name))
    }
}

// ── Extraction ───────────────────────────────────────────────────────────────

/// A whole filled-zone polygon kept aside for containment tests (a different-net
/// primitive sitting fully inside the pour is a short even if it never crosses
/// the boundary). The boundary itself is indexed as edge capsules so the R-tree
/// prunes the distance sweep; this side-table only serves point-in-polygon.
#[derive(Debug, Clone)]
struct ZonePoly {
    pts: Vec<(f64, f64)>,
    net: i64,
    bounds: [f64; 4],
    /// True when this came from a real `filled_polygon` (the actual copper, with
    /// antipads / thermal reliefs). Outline-only zones (no computed fill, common
    /// in pre-2017 boards) set this false: their drawn boundary is kept for
    /// clearance checks, but the containment short-test is skipped because the
    /// solid outline would falsely engulf every pad of other nets.
    filled: bool,
}

/// Per-layer accumulator of copper primitives.
#[derive(Default)]
struct LayerBuckets {
    /// layer name -> primitives on that layer (indexed for the distance sweep).
    by_layer: HashMap<String, Vec<Primitive>>,
    /// layer name -> full zone polygons (for containment only).
    zones: HashMap<String, Vec<ZonePoly>>,
}

impl LayerBuckets {
    fn push(&mut self, layer: &str, prim: Primitive) {
        self.by_layer.entry(layer.to_string()).or_default().push(prim);
    }

    /// Add a zone polygon: its boundary edges become indexed capsules (radius 0)
    /// so distance/clearance to it is pruned by the R-tree, and the whole
    /// polygon is stashed for the containment pass. `filled` marks whether this
    /// is real fill copper (eligible for the containment short-test) or only a
    /// drawn outline.
    fn push_zone(&mut self, layer: &str, pts: Vec<(f64, f64)>, net: i64, filled: bool) {
        if pts.len() < 3 {
            return;
        }
        let n = pts.len();
        let mut j = n - 1;
        for i in 0..n {
            let cap = Capsule {
                ax: pts[j].0,
                ay: pts[j].1,
                bx: pts[i].0,
                by: pts[i].1,
                r: 0.0,
            };
            self.push(layer, make_prim(Shape::Capsule(cap), net, ItemKind::Zone, String::new()));
            j = i;
        }
        let bounds = polygon_bounds(&pts);
        self.zones
            .entry(layer.to_string())
            .or_default()
            .push(ZonePoly { pts, net, bounds, filled });
    }
}

/// Bounding box (minx, miny, maxx, maxy) of a point list.
fn polygon_bounds(pts: &[(f64, f64)]) -> [f64; 4] {
    let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for &(x, y) in pts {
        b[0] = b[0].min(x);
        b[1] = b[1].min(y);
        b[2] = b[2].max(x);
        b[3] = b[3].max(y);
    }
    b
}

/// Read `(start x y)` / `(end x y)` style coordinate children.
fn xy_pair(list: &List, name: &str) -> Option<(f64, f64)> {
    let l = list.find(name)?;
    Some((l.arg_f64(0)?, l.arg_f64(1)?))
}

/// Inflated bounds of a shape: (minx, miny, maxx, maxy).
fn shape_bounds(shape: &Shape) -> [f64; 4] {
    match shape {
        Shape::Capsule(c) => {
            let minx = c.ax.min(c.bx) - c.r;
            let miny = c.ay.min(c.by) - c.r;
            let maxx = c.ax.max(c.bx) + c.r;
            let maxy = c.ay.max(c.by) + c.r;
            [minx, miny, maxx, maxy]
        }
        Shape::Polygon { pts, r } => {
            let mut minx = f64::INFINITY;
            let mut miny = f64::INFINITY;
            let mut maxx = f64::NEG_INFINITY;
            let mut maxy = f64::NEG_INFINITY;
            for &(x, y) in pts {
                minx = minx.min(x);
                miny = miny.min(y);
                maxx = maxx.max(x);
                maxy = maxy.max(y);
            }
            [minx - r, miny - r, maxx + r, maxy + r]
        }
    }
}

/// A representative interior point of a shape (its centroid-ish point), used
/// for the zone containment test.
fn representative_point(shape: &Shape) -> (f64, f64) {
    match shape {
        Shape::Capsule(c) => ((c.ax + c.bx) / 2.0, (c.ay + c.by) / 2.0),
        Shape::Polygon { pts, .. } => {
            let n = pts.len().max(1) as f64;
            let sx: f64 = pts.iter().map(|p| p.0).sum();
            let sy: f64 = pts.iter().map(|p| p.1).sum();
            (sx / n, sy / n)
        }
    }
}

fn make_prim(shape: Shape, net: i64, kind: ItemKind, owner: String) -> Primitive {
    let bounds = shape_bounds(&shape);
    Primitive {
        shape,
        net,
        kind,
        owner,
        bounds,
        idx: 0,
    }
}

/// Expand a `*.Cu` / `F&B.Cu` layer token to the concrete copper layers it
/// occupies, given the set of copper layers the board declares.
fn expand_layers(token: &str, copper_layers: &[String]) -> Vec<String> {
    if token == "*.Cu" || token.eq_ignore_ascii_case("F&B.Cu") {
        copper_layers.to_vec()
    } else if token.ends_with(".Cu") {
        vec![token.to_string()]
    } else {
        Vec::new()
    }
}

/// Collect the copper layer names the board declares (from `(layers ...)`),
/// falling back to the canonical two-layer stack.
fn copper_layers_of(root: &List) -> Vec<String> {
    let mut layers = Vec::new();
    if let Some(decl) = root.find("layers") {
        for l in decl.lists() {
            // `(0 "F.Cu" signal)`: the name is arg 0 (a string).
            if let Some(name) = l.arg_value(0) {
                if name.ends_with(".Cu") {
                    layers.push(name);
                }
            }
            // `(layer "F.Cu" ...)` style is also possible; name in arg 0 too.
        }
    }
    if layers.is_empty() {
        layers = vec!["F.Cu".to_string(), "B.Cu".to_string()];
    }
    layers
}

/// Build the per-layer copper primitives for a parsed board.
fn collect_primitives(root: &List, nets: &mut NetResolver) -> LayerBuckets {
    let mut buckets = LayerBuckets::default();
    let copper_layers = copper_layers_of(root);

    // ── Track segments ──────────────────────────────────────────────────────
    for seg in root.find_all("segment") {
        let (Some(start), Some(end)) = (xy_pair(seg, "start"), xy_pair(seg, "end")) else {
            continue;
        };
        let width = seg.find_f64("width").unwrap_or(0.0);
        let layer = seg.find_value("layer").unwrap_or_default();
        if !layer.ends_with(".Cu") {
            continue;
        }
        let Some(net) = nets.net_ref(seg) else { continue };
        let cap = Capsule {
            ax: start.0,
            ay: start.1,
            bx: end.0,
            by: end.1,
            r: width / 2.0,
        };
        buckets.push(&layer, make_prim(Shape::Capsule(cap), net, ItemKind::Track, String::new()));
    }

    // ── Arc tracks (KiCad 7+ `(arc (start)(mid)(end)(width)(layer)(net))`) ───
    for arc in root.find_all("arc") {
        let (Some(start), Some(end)) = (xy_pair(arc, "start"), xy_pair(arc, "end")) else {
            continue;
        };
        let mid = xy_pair(arc, "mid");
        let width = arc.find_f64("width").unwrap_or(0.0);
        let layer = arc.find_value("layer").unwrap_or_default();
        if !layer.ends_with(".Cu") {
            continue;
        }
        let Some(net) = nets.net_ref(arc) else { continue };
        for cap in flatten_arc(start, mid, end, width) {
            buckets.push(&layer, make_prim(Shape::Capsule(cap), net, ItemKind::Arc, String::new()));
        }
    }

    // ── Vias (multi-layer discs) ────────────────────────────────────────────
    for via in root.find_all("via") {
        let Some(at) = xy_pair(via, "at") else { continue };
        let size = via.find_f64("size").unwrap_or(0.0);
        let Some(net) = nets.net_ref(via) else { continue };
        let layer_token = via
            .find("layers")
            .and_then(|l| {
                // `(layers "F.Cu" "B.Cu")`: span the named copper layers.
                let names: Vec<String> = (0..)
                    .map_while(|i| l.arg_value(i))
                    .filter(|n| n.ends_with(".Cu"))
                    .collect();
                if names.is_empty() {
                    None
                } else {
                    Some(names)
                }
            })
            .unwrap_or_else(|| copper_layers.clone());
        let disc = Capsule {
            ax: at.0,
            ay: at.1,
            bx: at.0,
            by: at.1,
            r: size / 2.0,
        };
        for layer in &layer_token {
            buckets.push(layer, make_prim(Shape::Capsule(disc), net, ItemKind::Via, String::new()));
        }
    }

    // ── Filled zones (copper pours) ─────────────────────────────────────────
    for zone in root.find_all("zone") {
        let Some(net) = nets.net_ref(zone) else { continue };
        // A zone can fill several layers; each `(filled_polygon (layer ...))`
        // is the actual copper. Fall back to the zone's own `(layer ...)` and
        // its drawn `(polygon ...)` outline when no fill is present.
        let mut any_fill = false;
        for fp in zone.find_all("filled_polygon") {
            let layer = fp
                .find_value("layer")
                .or_else(|| zone.find_value("layer"))
                .unwrap_or_default();
            if !layer.ends_with(".Cu") {
                continue;
            }
            if let Some(pts) = read_pts(fp) {
                if pts.len() >= 3 {
                    any_fill = true;
                    buckets.push_zone(&layer, pts, net, true);
                }
            }
        }
        if !any_fill {
            // No fill computed in the file (common pre-2017): keep the drawn
            // outline for clearance checks but mark it unfilled so the
            // containment short-test skips it (the solid outline would falsely
            // engulf every other-net pad inside the pour, since antipads and
            // thermal reliefs are not represented).
            let layer = zone.find_value("layer").unwrap_or_default();
            if layer.ends_with(".Cu") {
                if let Some(poly) = zone.find("polygon").and_then(read_pts) {
                    buckets.push_zone(&layer, poly, net, false);
                }
            }
        }
    }

    // ── Pads (inside footprints) ────────────────────────────────────────────
    for fp in root.find_all("footprint").chain(root.find_all("module")) {
        let owner = footprint_reference(fp);
        let (fx, fy, frot) = at_of(fp);
        let rot_rad = frot.to_radians();
        let (fsin, fcos) = rot_rad.sin_cos();
        for pad in fp.find_all("pad") {
            let Some(net) = nets.net_ref(pad) else { continue };
            collect_pad(
                pad,
                net,
                &owner,
                (fx, fy),
                (fsin, fcos),
                &copper_layers,
                &mut buckets,
            );
        }
    }

    buckets
}

/// One pad → a primitive on each copper layer it touches.
fn collect_pad(
    pad: &List,
    net: i64,
    owner: &str,
    forigin: (f64, f64),
    frot: (f64, f64),
    copper_layers: &[String],
    buckets: &mut LayerBuckets,
) {
    let (fsin, fcos) = frot;
    let (fx, fy) = forigin;
    // Pad-local placement.
    let (px, py, prot) = at_of(pad);
    // Pad shape token is the 3rd positional arg: (pad "1" smd roundrect ...).
    let shape_tok = pad.arg_value(2).unwrap_or_default();
    let size = pad.find("size");
    let (sx, sy) = match size {
        Some(s) => (s.arg_f64(0).unwrap_or(0.0), s.arg_f64(1).unwrap_or(0.0)),
        None => (0.0, 0.0),
    };

    // World transform for a pad-frame offset (KiCad y-down, footprint rotation
    // counter-clockwise: matches pcb.rs's pad placement).
    let to_world = |ox: f64, oy: f64| -> (f64, f64) {
        (fx + px * fcos + py * fsin + ox, fy - px * fsin + py * fcos + oy)
    };
    let pad_origin = to_world(0.0, 0.0);

    // Rotate a pad-local outline offset into the world frame. KiCad writes the
    // pad's `(at x y rot)` rotation as the pad outline's *absolute* board-frame
    // orientation (the footprint rotation is already folded into it), so the
    // outline is rotated by `prot` alone (NOT composed with the footprint
    // rotation again, which the position transform already applied). The y-down
    // form matches `to_world`: (lx cos + ly sin, -lx sin + ly cos).
    let (psin, pcos) = prot.to_radians().sin_cos();
    let outline_to_world = |lx: f64, ly: f64| -> (f64, f64) {
        let wx = lx * pcos + ly * psin;
        let wy = -lx * psin + ly * pcos;
        (pad_origin.0 + wx, pad_origin.1 + wy)
    };

    // Layers this pad sits on.
    let layers: Vec<String> = pad
        .find("layers")
        .map(|l| {
            let toks: Vec<String> = (0..).map_while(|i| l.arg_value(i)).collect();
            let mut out = Vec::new();
            for t in toks {
                out.extend(expand_layers(&t, copper_layers));
            }
            out
        })
        .filter(|v: &Vec<String>| !v.is_empty())
        .unwrap_or_else(|| copper_layers.to_vec());

    let shape = match shape_tok.as_str() {
        "circle" => {
            let r = sx.max(sy) / 2.0;
            Shape::Capsule(Capsule {
                ax: pad_origin.0,
                ay: pad_origin.1,
                bx: pad_origin.0,
                by: pad_origin.1,
                r,
            })
        }
        "oval" => {
            // A stadium: a capsule whose segment runs along the longer axis,
            // radius = half the shorter dimension.
            let (long, short, along_x) = if sx >= sy { (sx, sy, true) } else { (sy, sx, false) };
            let half = (long - short).max(0.0) / 2.0;
            let (a, b) = if along_x {
                (outline_to_world(-half, 0.0), outline_to_world(half, 0.0))
            } else {
                (outline_to_world(0.0, -half), outline_to_world(0.0, half))
            };
            Shape::Capsule(Capsule {
                ax: a.0,
                ay: a.1,
                bx: b.0,
                by: b.1,
                r: short / 2.0,
            })
        }
        "custom" => {
            // Custom pad: gather its primitive polygon(s); fall back to the box.
            if let Some(poly) = custom_pad_polygon(pad, &outline_to_world) {
                Shape::Polygon { pts: poly, r: 0.0 }
            } else {
                rect_polygon(sx, sy, &outline_to_world, 0.0)
            }
        }
        // rect, roundrect, trapezoid and anything else: a rectangle. For
        // roundrect we keep a corner radius as inflation so the rounded copper
        // is not overstated.
        _ => {
            let rr = if shape_tok == "roundrect" {
                let ratio = pad.find_f64("roundrect_rratio").unwrap_or(0.0);
                ratio * sx.min(sy)
            } else {
                0.0
            };
            // Inset the rectangle by the corner radius and carry `r` so the
            // rounded outline is represented as inset-poly + radius.
            rect_polygon((sx - 2.0 * rr).max(0.0), (sy - 2.0 * rr).max(0.0), &outline_to_world, rr)
        }
    };

    for layer in &layers {
        buckets.push(layer, make_prim(shape.clone(), net, ItemKind::Pad, owner.to_string()));
    }
}

/// A rectangle of size (w, h) centred on the pad origin, built via the world
/// transform, carrying inflation radius `r`.
fn rect_polygon(w: f64, h: f64, to_world: &dyn Fn(f64, f64) -> (f64, f64), r: f64) -> Shape {
    let hw = w / 2.0;
    let hh = h / 2.0;
    let pts = vec![
        to_world(-hw, -hh),
        to_world(hw, -hh),
        to_world(hw, hh),
        to_world(-hw, hh),
    ];
    Shape::Polygon { pts, r }
}

/// Read a custom pad's polygon outline, transformed to world coordinates.
fn custom_pad_polygon(pad: &List, to_world: &dyn Fn(f64, f64) -> (f64, f64)) -> Option<Vec<(f64, f64)>> {
    let prim = pad.find("primitives")?;
    let poly = prim.find("gr_poly").or_else(|| prim.find("poly"))?;
    let pts = poly.find("pts")?;
    let out: Vec<(f64, f64)> = pts
        .find_all("xy")
        .filter_map(|p| Some(to_world(p.arg_f64(0)?, p.arg_f64(1)?)))
        .collect();
    (out.len() >= 3).then_some(out)
}

/// Read a `(pts (xy ..)(xy ..))` child into world-frame coordinates (no
/// transform; zones / filled polygons are already in board frame).
fn read_pts(list: &List) -> Option<Vec<(f64, f64)>> {
    let pts = list.find("pts")?;
    let out: Vec<(f64, f64)> = pts
        .find_all("xy")
        .filter_map(|p| Some((p.arg_f64(0)?, p.arg_f64(1)?)))
        .collect();
    (!out.is_empty()).then_some(out)
}

/// Flatten an arc (start, optional mid, end) of the given width into a chain of
/// capsule links. Without a mid point we approximate with the chord.
fn flatten_arc(start: (f64, f64), mid: Option<(f64, f64)>, end: (f64, f64), width: f64) -> Vec<Capsule> {
    let r = width / 2.0;
    let Some(mid) = mid else {
        return vec![Capsule {
            ax: start.0,
            ay: start.1,
            bx: end.0,
            by: end.1,
            r,
        }];
    };
    // Circumcircle of the three points.
    let Some((cx, cy, radius)) = circle_from_3(start, mid, end) else {
        return vec![Capsule {
            ax: start.0,
            ay: start.1,
            bx: end.0,
            by: end.1,
            r,
        }];
    };
    let ang = |p: (f64, f64)| (p.1 - cy).atan2(p.0 - cx);
    let a0 = ang(start);
    let am = ang(mid);
    let a1 = ang(end);
    // Sweep direction: go start->mid->end the short way through mid.
    // Normalise so the arc passes through mid.
    let through = |from: f64, to: f64, via: f64| {
        let norm = |x: f64| {
            let mut v = x;
            while v <= -std::f64::consts::PI {
                v += std::f64::consts::TAU;
            }
            while v > std::f64::consts::PI {
                v -= std::f64::consts::TAU;
            }
            v
        };
        // Total CCW sweep from->to and whether mid lies on it.
        let mut s = norm(to - from);
        if s < 0.0 {
            s += std::f64::consts::TAU;
        }
        let mut m = norm(via - from);
        if m < 0.0 {
            m += std::f64::consts::TAU;
        }
        if m <= s {
            s
        } else {
            s - std::f64::consts::TAU
        }
    };
    let sweep = through(a0, a1, am);
    let mut caps = Vec::with_capacity(ARC_SEGMENTS);
    let mut prev = start;
    for i in 1..=ARC_SEGMENTS {
        let t = i as f64 / ARC_SEGMENTS as f64;
        let a = a0 + sweep * t;
        let p = (cx + radius * a.cos(), cy + radius * a.sin());
        caps.push(Capsule {
            ax: prev.0,
            ay: prev.1,
            bx: p.0,
            by: p.1,
            r,
        });
        prev = p;
    }
    caps
}

/// Circumcircle (centre, radius) of three points, or None if colinear.
fn circle_from_3(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> Option<(f64, f64, f64)> {
    let d = 2.0 * (a.0 * (b.1 - c.1) + b.0 * (c.1 - a.1) + c.0 * (a.1 - b.1));
    if d.abs() < 1e-12 {
        return None;
    }
    let a2 = a.0 * a.0 + a.1 * a.1;
    let b2 = b.0 * b.0 + b.1 * b.1;
    let c2 = c.0 * c.0 + c.1 * c.1;
    let ux = (a2 * (b.1 - c.1) + b2 * (c.1 - a.1) + c2 * (a.1 - b.1)) / d;
    let uy = (a2 * (c.0 - b.0) + b2 * (a.0 - c.0) + c2 * (b.0 - a.0)) / d;
    let r = ((a.0 - ux).powi(2) + (a.1 - uy).powi(2)).sqrt();
    Some((ux, uy, r))
}

/// `(at x y [rot])` reader.
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

/// Reference designator of a footprint (property "Reference" or fp_text).
fn footprint_reference(fp: &List) -> String {
    for prop in fp.find_all("property") {
        if prop.arg_value(0).as_deref() == Some("Reference") {
            if let Some(v) = prop.arg_value(1) {
                return v;
            }
        }
    }
    for t in fp.find_all("fp_text") {
        if t.arg_value(0).as_deref() == Some("reference") {
            if let Some(v) = t.arg_value(1) {
                return v;
            }
        }
    }
    String::new()
}

// ── Board clearance rule ─────────────────────────────────────────────────────

/// Read the board's design-rule copper clearance (mm), else the default. KiCad
/// stores it in `(setup (rules (min_clearance N)))` (modern) or a default
/// net-class `(clearance N)`. We take the smallest credible rule we can find in
/// `(setup ...)`, since DRC should use the tightest rule. Zone `connect_pads`
/// clearances are intentionally ignored (they are not the trace rule).
fn board_clearance(root: &List) -> f64 {
    let Some(setup) = root.find("setup") else {
        return DEFAULT_CLEARANCE_MM;
    };
    // Modern: (setup (rules (min_clearance N) ...)).
    if let Some(rules) = setup.find("rules") {
        if let Some(c) = rules.find_f64("min_clearance") {
            if c > 0.0 {
                return c;
            }
        }
    }
    // Direct (setup (min_clearance N)) or (setup (clearance N)).
    for key in ["min_clearance", "clearance", "trace_clearance"] {
        if let Some(c) = setup.find_f64(key) {
            if c > 0.0 {
                return c;
            }
        }
    }
    DEFAULT_CLEARANCE_MM
}

// ── Top-level DRC ────────────────────────────────────────────────────────────

/// Run geometric short / clearance detection on a parsed `.kicad_pcb` document.
/// `clearance_override` forces the clearance rule (mm) when `Some`.
pub fn run_drc(doc: &Document, clearance_override: Option<f64>) -> DrcReport {
    let Some(root) = doc.root() else {
        return DrcReport::default();
    };
    if root.name() != Some("kicad_pcb") {
        return DrcReport::default();
    }
    let mut nets = NetResolver::from_root(root);
    let clearance = clearance_override.unwrap_or_else(|| board_clearance(root));
    let buckets = collect_primitives(root, &mut nets);

    // Nets that carry no real connectivity (net 0 and KiCad's per-pad
    // `unconnected-(...)` placeholders): copper on them is never a short.
    let no_net: std::collections::HashSet<i64> = buckets
        .by_layer
        .values()
        .flat_map(|v| v.iter().map(|p| p.net))
        .chain(buckets.zones.values().flat_map(|v| v.iter().map(|z| z.net)))
        .filter(|&id| nets.is_no_net(id))
        .collect();

    let mut report = DrcReport {
        clearance_mm: clearance,
        findings: Vec::new(),
        primitive_count: buckets.by_layer.values().map(Vec::len).sum(),
    };

    // De-dup findings on the same net pair + layer + rounded location, so a
    // pour overlapping a long track does not emit thousands of near-identical
    // rows.
    let mut seen: std::collections::HashSet<(i64, i64, String, i64, i64)> =
        std::collections::HashSet::new();

    // Record one finding, de-duplicating on net pair + layer + ~0.25 mm cell so
    // a pour running alongside a long track does not emit thousands of rows.
    let mut record = |kind: ViolationKind,
                      pa: i64,
                      pb: i64,
                      item_a: Item,
                      item_b: Item,
                      layer: &str,
                      cx: f64,
                      cy: f64,
                      gap: f64| {
        let (na, nb) = (pa.min(pb), pa.max(pb));
        let key = (
            na,
            nb,
            layer.to_string(),
            (cx * 4.0).round() as i64,
            (cy * 4.0).round() as i64,
        );
        if !seen.insert(key) {
            return;
        }
        let (item_a, item_b) = if pa <= pb { (item_a, item_b) } else { (item_b, item_a) };
        report.findings.push(DrcFinding {
            kind,
            net_a: na,
            net_b: nb,
            net_a_name: nets.name_of(na),
            net_b_name: nets.name_of(nb),
            layer: layer.to_string(),
            x: cx,
            y: cy,
            gap_mm: gap,
            item_a,
            item_b,
        });
    };

    let empty_zones: Vec<ZonePoly> = Vec::new();
    for (layer, prims_ref) in &buckets.by_layer {
        let zones = buckets.zones.get(layer).unwrap_or(&empty_zones);
        // Index primitives, recording their position for shape lookup.
        let mut prims = prims_ref.clone();
        for (i, p) in prims.iter_mut().enumerate() {
            p.idx = i;
        }
        let leaves: Vec<Leaf> = prims
            .iter()
            .map(|p| Leaf {
                bounds: p.bounds,
                idx: p.idx,
            })
            .collect();
        let tree = RTree::bulk_load(leaves);

        // ── Edge / clearance sweep (R-tree pruned) ──────────────────────────
        for p in &prims {
            // Query window: this primitive's bounds inflated by the clearance.
            let query = AABB::from_corners(
                [p.bounds[0] - clearance, p.bounds[1] - clearance],
                [p.bounds[2] + clearance, p.bounds[3] + clearance],
            );
            for leaf in tree.locate_in_envelope_intersecting(query) {
                let q = &prims[leaf.idx];
                // Each unordered pair once; skip same-net.
                if q.idx <= p.idx || q.net == p.net {
                    continue;
                }
                // Net 0 and `unconnected-(...)` copper carry no connectivity, so
                // they cannot form an electrical short.
                if no_net.contains(&p.net) || no_net.contains(&q.net) {
                    continue;
                }
                // Two pads of the *same* footprint are positioned by the
                // footprint author, not the router. Some footprints place
                // different-net pads deliberately abutting (fuse clips, jumper
                // bridges, edge-connector fingers). KiCad does not treat that as
                // a board short, so neither do we.
                if p.kind == ItemKind::Pad
                    && q.kind == ItemKind::Pad
                    && !p.owner.is_empty()
                    && p.owner == q.owner
                {
                    continue;
                }
                let (gap, (cx, cy)) = shape_gap(&p.shape, &q.shape);
                if gap >= clearance {
                    continue;
                }
                let kind = if gap <= 0.0 {
                    ViolationKind::Short
                } else {
                    ViolationKind::Clearance
                };
                record(kind, p.net, q.net, p.item(), q.item(), layer, cx, cy, gap);
            }
        }

        // ── Zone containment pass ───────────────────────────────────────────
        // A primitive sitting fully inside a different-net pour (without ever
        // crossing the indexed boundary edges) is still a short. Test each
        // non-zone primitive's representative point against every opposite-net
        // zone whose bounding box contains it.
        if !zones.is_empty() {
            for p in &prims {
                if p.kind == ItemKind::Zone || no_net.contains(&p.net) {
                    continue;
                }
                let (rx, ry) = representative_point(&p.shape);
                for z in zones {
                    if !z.filled || z.net == p.net || no_net.contains(&z.net) {
                        continue;
                    }
                    if rx < z.bounds[0] || rx > z.bounds[2] || ry < z.bounds[1] || ry > z.bounds[3] {
                        continue;
                    }
                    if point_in_polygon(rx, ry, &z.pts) {
                        record(
                            ViolationKind::Short,
                            p.net,
                            z.net,
                            p.item(),
                            Item { kind: ItemKind::Zone, net: z.net, owner: String::new() },
                            layer,
                            rx,
                            ry,
                            -clearance,
                        );
                    }
                }
            }
        }
    }

    // Stable order: shorts first, then by net pair and layer.
    report.findings.sort_by(|a, b| {
        (a.kind == ViolationKind::Clearance)
            .cmp(&(b.kind == ViolationKind::Clearance))
            .then(a.net_a.cmp(&b.net_a))
            .then(a.net_b.cmp(&b.net_b))
            .then(a.layer.cmp(&b.layer))
    });
    report
}

/// Convenience: parse `.kicad_pcb` text and run DRC with the default clearance
/// rule (or the board's own rule when present).
pub fn drc_from_text(text: &str) -> Result<DrcReport, forge_sexpr::ParseError> {
    let doc = forge_sexpr::parse(text)?;
    Ok(run_drc(&doc, None))
}
