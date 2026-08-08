//! Connectivity reconstruction: copper geometry -> nets -> component pads.
//!
//! Input: per-copper-layer solid primitives (flashes, tracks, regions, plus
//! plated-drill discs) and the placed components from the P&P file. Output: an
//! [`ExtractedBoard`] with synthetic nets and pads.
//!
//! ## How nets form
//!
//! Two pieces of copper that touch are the same conductor. We index every
//! primitive on a layer in an `rstar` R*-tree (the same prune the DRC uses for
//! its O(n) sweep) and union any pair whose signed copper gap is `<= eps`. A
//! plated drill becomes a barrel on each copper layer its [`LayerSpan`] reaches
//! and unions the primitives that barrel touches, so a through-hole stitches the
//! whole stack, a blind or buried via stitches only its own pair, and a hit whose
//! span the files never gave us stitches nothing at all. A slot's barrel is the
//! stadium swept along its routed path, so it connects copper anywhere along the
//! wall. The connected components of the union-find are the nets; each gets a
//! synthetic name `NET_n`, except the largest pour-touching component which is
//! labelled `GND` as a heuristic (copper pours are overwhelmingly ground).
//!
//! ## How pads bind to components
//!
//! Each placed component sits at a known (x, y). We collect the flash
//! primitives near it (within a footprint-size window inferred from the package
//! name) and assign each to that component as a pad, tagged with the net of the
//! copper it sits on. Flashes claimed by no component are reported as
//! `unassigned` (honest accounting, surfaced in the docs/accuracy numbers).
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-extract/gerber.md.

use std::collections::HashMap;
use std::sync::Arc;

use rstar::{RTree, RTreeObject, AABB};

use crate::{Component, ExtractedBoard, Net, Pin};

use super::geo::{shape_gap, Shape};
use super::placement::Placement;
use super::rs274x::{CopperPrim, PrimKind, X2Attrs};

/// Copper that touches within this gap (mm) is treated as one conductor. Small
/// positive value absorbs export rounding and the convex-hull over-approx of
/// macro flashes without bridging genuinely separate nets (design clearances
/// are >= 0.1 mm).
const TOUCH_EPS: f64 = 0.005;

/// A primitive placed on a concrete copper layer, ready for indexing.
struct LayerPrim {
    shape: Shape,
    kind: PrimKind,
    bounds: [f64; 4],
    /// X2 identity carried over from the film (empty on stripped films).
    attrs: X2Attrs,
}

struct Leaf {
    bounds: [f64; 4],
    idx: usize,
}

impl RTreeObject for Leaf {
    type Envelope = AABB<[f64; 2]>;
    fn envelope(&self) -> Self::Envelope {
        AABB::from_corners(
            [self.bounds[0], self.bounds[1]],
            [self.bounds[2], self.bounds[3]],
        )
    }
}

/// Disjoint-set (union-find) with path compression + union by size.
struct Dsu {
    parent: Vec<usize>,
    size: Vec<usize>,
}

impl Dsu {
    fn new(n: usize) -> Self {
        Dsu {
            parent: (0..n).collect(),
            size: vec![1; n],
        }
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
        let (big, small) = if self.size[ra] >= self.size[rb] {
            (ra, rb)
        } else {
            (rb, ra)
        };
        self.parent[small] = big;
        self.size[big] += self.size[small];
    }
}

/// Which copper layers a plated hit's barrel actually reaches.
///
/// A through-hole touches the whole stack. A blind or buried via touches only
/// the layers its drill file pairs, and treating it as a through-hole merges
/// nets the real stackup keeps apart: a phantom short, invented by the reader.
/// [`LayerSpan::Unknown`] is the third, deliberate answer: the files describe a
/// multi-span drill set but do not say what THIS file's span is, so the barrel
/// stitches nothing and the reader says so out loud. A refused stitch under-
/// reports connectivity, which is recoverable; a guessed one fabricates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerSpan {
    /// Reaches every copper layer in the stack.
    Through,
    /// Reaches stack indices `from..=to` inclusive, 0-based from the top.
    Range { from: usize, to: usize },
    /// The span is not derivable from the files provided. Stitches nothing.
    Unknown,
}

/// A drilled, plated hit: a via, a through-hole pad, or a plated slot.
pub struct PlatedHole {
    pub x: f64,
    pub y: f64,
    /// Barrel diameter (round hole) or routed width (slot), in mm.
    pub diameter: f64,
    /// Far end of a plated slot; `None` for a round hole. The plated wall is
    /// then the stadium swept from `(x, y)` to here, and it connects every
    /// piece of copper that wall touches along its whole length, not merely
    /// what sits over the two endpoints.
    pub to: Option<(f64, f64)>,
    /// The copper layers this barrel reaches.
    pub span: LayerSpan,
}

impl PlatedHole {
    /// A round plated hole that reaches the whole stack.
    pub fn through(x: f64, y: f64, diameter: f64) -> Self {
        PlatedHole {
            x,
            y,
            diameter,
            to: None,
            span: LayerSpan::Through,
        }
    }

    /// The barrel's copper footprint: a disc for a round hole, a stadium of the
    /// routed width for a slot.
    ///
    /// The radius is the drill's own, never floored. A floor inflates a small
    /// barrel until it reaches copper the real one does not, which is copper
    /// invented out of a rounding convenience, and a tool table can say
    /// `T1C0.0` outright. A hit whose tool has no size is therefore a point:
    /// it connects copper that actually covers it and nothing else, which is
    /// the most the file supports.
    fn barrel(&self) -> Shape {
        let r = (self.diameter / 2.0).max(0.0);
        match self.to {
            None => Shape::disc(self.x, self.y, r),
            Some((tx, ty)) => Shape::Capsule(super::geo::Capsule {
                ax: self.x,
                ay: self.y,
                bx: tx,
                by: ty,
                r,
            }),
        }
    }

    /// The inclusive stack-index range this barrel occupies, or `None` when the
    /// span is unknown and the reader refuses to stitch.
    fn layer_range(&self, n_layers: usize) -> Option<(usize, usize)> {
        match self.span {
            LayerSpan::Through => Some((0, n_layers.saturating_sub(1))),
            LayerSpan::Unknown => None,
            LayerSpan::Range { from, to } => {
                let last = n_layers.saturating_sub(1);
                // A declared span that names a layer this job does not have is
                // not a through-hole; it is a span we failed to resolve.
                (from <= last && to <= last && from <= to).then_some((from, to))
            }
        }
    }
}

/// Build the connectivity graph and emit an [`ExtractedBoard`].
///
/// `layers` is top-to-bottom; each entry is that layer's copper primitives.
/// `holes` are plated through-holes (vias + PTH pads). `placements` are the
/// placed components from the P&P file (may be empty: copper-only mode).
pub fn reconstruct(
    name: &str,
    layers: Vec<Vec<CopperPrim>>,
    holes: Vec<PlatedHole>,
    placements: Vec<Placement>,
) -> (ExtractedBoard, ReconStats) {
    // ── 1. Flatten every primitive into one global vector, remembering layer ──
    // Plated hits become a barrel primitive on each layer their span reaches,
    // tagged `PrimKind::Via`, plus a stitch joining those barrels to each other.
    let n_layers = layers.len().max(1);
    let mut prims: Vec<LayerPrim> = Vec::new();
    // global prim index -> layer
    let mut prim_layer: Vec<usize> = Vec::new();

    for (li, layer) in layers.into_iter().enumerate() {
        for cp in layer {
            let bounds = cp.shape.bounds();
            prims.push(LayerPrim {
                shape: cp.shape,
                kind: cp.kind,
                bounds,
                attrs: cp.attrs,
            });
            prim_layer.push(li);
        }
    }

    // Barrel primitives: one per copper layer the hit actually reaches, recorded
    // so we can stitch them together after. `hole_prims[h]` is that hit's global
    // prim indices. A hit whose span we could not resolve contributes NO barrel
    // on any layer: it stitches nothing, which is the refusal, not a guess.
    let mut hole_prims: Vec<Vec<usize>> = Vec::with_capacity(holes.len());
    let mut n_slots = 0usize;
    let mut refused_span_holes = 0usize;
    for h in &holes {
        if h.to.is_some() {
            n_slots += 1;
        }
        let Some((first, last)) = h.layer_range(n_layers) else {
            refused_span_holes += 1;
            hole_prims.push(Vec::new());
            continue;
        };
        let mut idxs = Vec::with_capacity(last - first + 1);
        for li in first..=last {
            let shape = h.barrel();
            let bounds = shape.bounds();
            let gi = prims.len();
            prims.push(LayerPrim {
                shape,
                kind: PrimKind::Via,
                bounds,
                attrs: X2Attrs::default(),
            });
            prim_layer.push(li);
            // Extend that layer's range to include the barrel. Holes are
            // appended after all layer primitives, so we widen the end marker.
            idxs.push(gi);
        }
        hole_prims.push(idxs);
    }

    let mut dsu = Dsu::new(prims.len());
    let mut notes: Vec<String> = Vec::new();

    // ── 2. Per-layer R-tree sweep: union touching copper ────────────────────
    // We index per layer. A hole disc must be tested against its layer's
    // primitives too, so we add hole discs to the relevant layer's index.
    let mut per_layer_members: Vec<Vec<usize>> = vec![Vec::new(); n_layers];
    for gi in 0..prims.len() {
        per_layer_members[prim_layer[gi]].push(gi);
    }

    for (li, members) in per_layer_members.iter().enumerate() {
        if members.is_empty() {
            continue;
        }
        // Edge-touch sweep over NON-region copper (tracks, flashes, vias). A
        // copper pour (region) is handled by a separate containment pass below,
        // never by edge proximity: a real pour weaves a single keyholed contour
        // whose boundary passes microns from every antipad-isolated pad, so
        // edge-distance to the pour boundary would falsely bridge unrelated
        // nets. Membership in a pour is "the copper sits *inside* the filled
        // area", which is a containment fact, not a proximity one.
        let solids: Vec<usize> = members
            .iter()
            .copied()
            .filter(|&gi| prims[gi].kind != PrimKind::Region)
            .collect();
        let leaves: Vec<Leaf> = solids
            .iter()
            .map(|&gi| Leaf {
                bounds: prims[gi].bounds,
                idx: gi,
            })
            .collect();
        let tree = RTree::bulk_load(leaves);
        for &gi in &solids {
            let p = &prims[gi];
            let query = AABB::from_corners(
                [p.bounds[0] - TOUCH_EPS, p.bounds[1] - TOUCH_EPS],
                [p.bounds[2] + TOUCH_EPS, p.bounds[3] + TOUCH_EPS],
            );
            for leaf in tree.locate_in_envelope_intersecting(query) {
                let gj = leaf.idx;
                if gj <= gi {
                    continue;
                }
                if shape_gap(&prims[gi].shape, &prims[gj].shape) <= TOUCH_EPS {
                    dsu.union(gi, gj);
                }
            }
        }

        // Region overlap pass: a non-region primitive joins a pour when its
        // copper genuinely *overlaps* the filled area, not merely runs near the
        // boundary. We test the primitive's representative point AND (for
        // tracks) its endpoints for containment inside the keyholed contour. A
        // thermal spoke or a pad that the pour floods up to has a point inside
        // the fill; an antipad-isolated pad sits in a pocket the even-odd test
        // puts *outside*, and a track merely skirting the keyhole boundary has
        // no point inside. This keeps legitimate pour connections (GND flood +
        // thermal spokes) while never bridging the nets a pour weaves around.
        let regions: Vec<usize> = members
            .iter()
            .copied()
            .filter(|&gi| prims[gi].kind == PrimKind::Region)
            .collect();
        if !regions.is_empty() {
            let _ = li;
            // Index regions by bbox so each primitive only tests the (usually
            // one or two) pours that actually cover its location, not all of a
            // board's hundreds of separate pour islands.
            let region_tree = RTree::bulk_load(
                regions
                    .iter()
                    .map(|&rgi| Leaf {
                        bounds: prims[rgi].bounds,
                        idx: rgi,
                    })
                    .collect(),
            );
            // Grid-accelerate containment only for *large* pours: a board-
            // spanning plane carries tens of thousands of vertices and is tested
            // against every primitive, so a raw even-odd test there is
            // quadratic; the grid makes each query O(1) outside the boundary
            // band. Small pours (the common case: a few hundred separate fill
            // islands) are cheaper tested directly than gridded, so they are
            // left to plain `point_in_polygon` and skip the build cost.
            const GRID_VERT_THRESHOLD: usize = 2000;
            let region_grids: HashMap<usize, super::geo::PolyGrid> = regions
                .iter()
                .filter_map(|&rgi| {
                    if let Shape::Polygon { pts, .. } = &prims[rgi].shape {
                        if pts.len() >= GRID_VERT_THRESHOLD {
                            let cells = (pts.len() / 4).clamp(64, 512);
                            return Some((rgi, super::geo::PolyGrid::new(pts, cells)));
                        }
                    }
                    None
                })
                .collect();
            for &gi in members {
                if prims[gi].kind == PrimKind::Region {
                    continue;
                }
                let pb = prims[gi].bounds;
                let near_regions: Vec<usize> = region_tree
                    .locate_in_envelope_intersecting(AABB::from_corners(
                        [pb[0], pb[1]],
                        [pb[2], pb[3]],
                    ))
                    .map(|l| l.idx)
                    .collect();
                if near_regions.is_empty() {
                    continue;
                }
                // Test points: the centre, capsule endpoints, and (for a track)
                // a few interior samples along the segment. A pour that floods
                // onto a pad/spoke contains at least one of these points inside
                // its filled outline; an antipad-isolated pad has none inside
                // (the even-odd keyhole puts the pocket outside); a track merely
                // skirting the boundary also has none inside. Pure point-in-
                // polygon (no poly-poly distance) keeps this near-linear in the
                // pour's vertex count, so big copper pours stay cheap.
                let mut test_pts = vec![prims[gi].shape.center()];
                if let Shape::Capsule(c) = &prims[gi].shape {
                    test_pts.push((c.ax, c.ay));
                    test_pts.push((c.bx, c.by));
                    // Sample the segment so a long track that dips into a pour
                    // mid-span is caught even if both ends are outside.
                    for k in 1..4 {
                        let t = k as f64 / 4.0;
                        test_pts.push((c.ax + (c.bx - c.ax) * t, c.ay + (c.by - c.ay) * t));
                    }
                }
                for &rgi in &near_regions {
                    let b = prims[rgi].bounds;
                    // Only polygonal regions participate: a capsule-shaped
                    // region primitive has no filled outline to contain into.
                    if !matches!(prims[rgi].shape, Shape::Capsule(_)) {
                        // (a) Containment: a sample point inside the filled
                        // outline means the pour copper is *on* that primitive.
                        // Large pours use the grid; small ones test directly. A
                        // multi-contour pour (an outer with holes) uses even-odd
                        // containment, inside the ring is in, inside a hole is
                        // out; these are rare and small, so they take the exact
                        // test without a grid.
                        let inside = test_pts.iter().any(|&(px, py)| {
                            if px < b[0] || px > b[2] || py < b[1] || py > b[3] {
                                return false;
                            }
                            match &prims[rgi].shape {
                                Shape::Polygon { pts, .. } => match region_grids.get(&rgi) {
                                    Some(g) => g.contains(px, py),
                                    None => super::geo::point_in_polygon(px, py, pts),
                                },
                                Shape::MultiPolygon { contours } => {
                                    super::geo::point_in_contours(px, py, contours)
                                }
                                Shape::Capsule(_) => false,
                            }
                        });
                        // (b) Edge penetration: a pad/track whose finite-width
                        // copper laps onto the pour but whose centre-line stays
                        // just outside the outline (thermal-relief pads, pour
                        // edge feathering). The pour boundary genuinely cuts
                        // *through* the primitive's copper, so the signed gap is
                        // clearly negative. An antipad-isolated pad keeps the
                        // drawn clearance (~0.2 mm) to the boundary, so its gap
                        // stays positive and it is NOT joined. We only pay this
                        // poly distance when the cheap containment missed and the
                        // bounding boxes actually overlap.
                        // Only pads (flashes) pay this poly distance: a track
                        // long enough to reach a pour lands a sample point inside
                        // it (caught by containment), so the costly poly-poly is
                        // confined to the comparatively few pad primitives, which
                        // keeps board-sized pours from making this quadratic.
                        let penetrates = !inside && prims[gi].kind == PrimKind::Flash && {
                            let bp = prims[gi].bounds;
                            !(bp[2] < b[0] || bp[0] > b[2] || bp[3] < b[1] || bp[1] > b[3])
                                && shape_gap(&prims[gi].shape, &prims[rgi].shape) < -0.04
                        };
                        if inside || penetrates {
                            dsu.union(gi, rgi);
                        }
                    }
                }
            }
        }
    }

    // ── 3. Stitch layers through plated holes ───────────────────────────────
    for idxs in &hole_prims {
        for w in idxs.windows(2) {
            dsu.union(w[0], w[1]);
        }
    }

    // ── 3b. X2 net identity: union copper the film NAMES onto one net ───────
    // An X2 film states each object's net outright (`%TO.N,<name>`), so two
    // primitives carrying the same name are one conductor by the film's own
    // declaration, whether or not their copper touches in the films we
    // classified (the routing may pass through an inner layer this job did
    // not ship). This runs AFTER the geometric passes so that every join the
    // film asserts and the copper does not make on its own is COUNTED and
    // surfaced: that gap is either routing on films we never saw, or a
    // genuine open the film's intent would otherwise paper over, and hiding
    // it would mask exactly the defect a connectivity reader exists to see.
    // On a stripped film no primitive carries a name and this pass does
    // nothing, leaving the geometric reconstruction bit-for-bit alone.
    // A net-tie object carries SEVERAL names (`%TO.N,A,B*%`); each name is
    // unioned through the object itself, which is exactly what a net tie is.
    // The same co-occurrence feeds `tied_names`, so the naming pass below can
    // tell a film-declared tie from a genuine geometric conflict.
    let mut x2_net_first: HashMap<&str, usize> = HashMap::new();
    let mut x2_fragment_joins: usize = 0;
    let mut tied_names: Vec<(Arc<str>, Arc<str>)> = Vec::new();
    for gi in 0..prims.len() {
        let names = prims[gi].attrs.net_names();
        for name in names {
            match x2_net_first.get(&**name) {
                Some(&first) => {
                    if dsu.find(first) != dsu.find(gi) {
                        x2_fragment_joins += 1;
                    }
                    dsu.union(first, gi);
                }
                None => {
                    x2_net_first.insert(name, gi);
                }
            }
        }
        for pair in names.windows(2) {
            tied_names.push((pair[0].clone(), pair[1].clone()));
        }
    }
    let x2_nets_present = !x2_net_first.is_empty();
    if x2_fragment_joins > 0 {
        notes.push(format!(
            "{x2_fragment_joins} join(s) in this reconstruction come from the film's X2 net \
             names alone: copper the traced geometry does NOT connect was united because its \
             %TO.N attributes name the same net. That usually means routing lives on films this \
             job did not ship (or did not classify), but it can also hide a genuine open, the \
             copper as manufactured would not carry these nets. Check the layer count note if \
             one is present; supply the missing films to make the geometry stand on its own."
        ));
    }

    // ── 4. Connected components -> nets ─────────────────────────────────────
    let mut root_to_net: HashMap<usize, i64> = HashMap::new();
    let mut next_net: i64 = 1;
    let mut net_of_prim: Vec<i64> = vec![0; prims.len()];
    // Track whether a component touches a region (pour) -> GND-class label.
    let mut net_touches_region: HashMap<i64, bool> = HashMap::new();
    let mut net_prim_count: HashMap<i64, usize> = HashMap::new();

    for gi in 0..prims.len() {
        let root = dsu.find(gi);
        let net = *root_to_net.entry(root).or_insert_with(|| {
            let id = next_net;
            next_net += 1;
            id
        });
        net_of_prim[gi] = net;
        *net_prim_count.entry(net).or_default() += 1;
        if prims[gi].kind == PrimKind::Region {
            net_touches_region.insert(net, true);
        }
    }

    // Net names. When the film carries X2 net identity, the name comes from
    // the film: every primitive of a reconstructed net that has a `%TO.N`
    // agrees on one name in the healthy case (the by-name union above makes
    // same-name copper one net; a net carrying TWO different film names means
    // the *geometry* bridged copper the film says is separate, a disagreement
    // that is NAMED in the notes rather than silently resolved). Without X2,
    // the GND heuristic labels the biggest pour-touching net and the rest get
    // synthetic `NET_n` names, exactly as before.
    let mut net_x2_names: HashMap<i64, std::collections::BTreeSet<&str>> = HashMap::new();
    for gi in 0..prims.len() {
        for name in prims[gi].attrs.net_names() {
            net_x2_names
                .entry(net_of_prim[gi])
                .or_default()
                .insert(name);
        }
    }
    // Name-level tie groups: names that co-occur on ONE object are declared
    // one conductor by the film (a net tie), so a reconstructed net carrying
    // exactly the members of a tie group is film-consistent, not a conflict.
    let name_ids: HashMap<&str, usize> = x2_net_first
        .keys()
        .enumerate()
        .map(|(i, k)| (*k, i))
        .collect();
    let mut name_dsu = Dsu::new(name_ids.len());
    for (a, b) in &tied_names {
        name_dsu.union(name_ids[&**a], name_ids[&**b]);
    }
    let mut x2_named_nets = 0usize;

    // GND heuristic: among region-touching nets, the one with the most copper.
    // Break count ties by lowest net id (Reverse) so the GND label is stable,
    // iterating the HashMap keys left it dependent on iteration order when two
    // pours tied on primitive count. Only consulted when the film carries no
    // X2 net names: a film that names its nets outranks any heuristic.
    let gnd_net = if x2_nets_present {
        None
    } else {
        net_touches_region.keys().copied().max_by_key(|&n| {
            (
                net_prim_count.get(&n).copied().unwrap_or(0),
                std::cmp::Reverse(n),
            )
        })
    };

    // Build the Net table with names.
    let mut nets: Vec<Net> = root_to_net
        .values()
        .copied()
        .map(|id| {
            let name = match net_x2_names.get(&id) {
                Some(names) => {
                    let groups: std::collections::HashSet<usize> =
                        names.iter().map(|n| name_dsu.find(name_ids[n])).collect();
                    if groups.len() == 1 {
                        // Film-consistent: one name, or several names the film
                        // itself ties on one object. A tie keeps every member
                        // visible in the label.
                        x2_named_nets += 1;
                        names.iter().copied().collect::<Vec<_>>().join("/")
                    } else {
                        notes.push(format!(
                            "the reconstructed copper of net NET_{id} carries {} different X2 \
                             net names ({}) that no net-tie object declares connected: the \
                             geometry joins copper the film assigns to separate nets. This is \
                             either a real short in the layout or an over-merge in the \
                             geometric reconstruction; the net keeps its synthetic name so the \
                             disagreement stays visible.",
                            names.len(),
                            names
                                .iter()
                                .map(|n| format!("\"{n}\""))
                                .collect::<Vec<_>>()
                                .join(", "),
                        ));
                        format!("NET_{id}")
                    }
                }
                None if Some(id) == gnd_net => "GND".to_string(),
                None => format!("NET_{id}"),
            };
            Net { id, name }
        })
        .collect();
    nets.sort_by_key(|n| n.id);

    // ── 5. Assign flashes to placed components ──────────────────────────────
    // Flash-centric assignment: each flash goes to the *nearest* placed
    // component whose footprint window contains it. Doing it per-flash (rather
    // than letting each component greedily sweep its window) means a flash is
    // never double-claimed and no component is starved by an earlier neighbour
    // that over-reached. Two coincident flashes (a THT pad's F.Cu + B.Cu rings
    // and its drill discs) collapse to one pad via a per-component cell set.
    let flash_idxs: Vec<usize> = (0..prims.len())
        .filter(|&gi| prims[gi].kind == PrimKind::Flash)
        .collect();
    let total_flashes = flash_idxs.len();

    // X2 pin identity: flashes whose film states outright which component pad
    // they are (`%TO.P,<refdes>,<pin>`). Where it exists, pad->refdes->pin
    // binding comes from the film, not from the footprint-window guess.
    let x2_pin_flashes: Vec<usize> = flash_idxs
        .iter()
        .copied()
        .filter(|&gi| prims[gi].attrs.pin.is_some())
        .collect();
    let x2_pins_present = !x2_pin_flashes.is_empty();
    // Every flash the film did not bind by `.P` keeps its geometric fallback,
    // UNLESS the film stated what the flash is and it is not a component pad
    // (a fiducial, an antipad, a washer; via flashes were already classified
    // as vias at parse time). Absence of `.P` is NOT a non-pad assertion: a
    // partially attributed film (a merged film, an exporter that attributes
    // only a subset of its pads) must not have its bare pads silently
    // deleted just because another flash somewhere carried pin identity. On
    // a stripped job every flash lands here: the pre-X2 path, unchanged.
    let geometric_flashes: Vec<usize> = flash_idxs
        .iter()
        .copied()
        .filter(|&gi| {
            prims[gi].attrs.pin.is_none()
                && !prims[gi]
                    .attrs
                    .function
                    .as_ref()
                    .is_some_and(super::rs274x::function_is_nonpad)
        })
        .collect();

    // Index the placed components so each flash can find its nearest one.
    struct CompLeaf {
        bounds: [f64; 4],
        idx: usize,
    }
    impl RTreeObject for CompLeaf {
        type Envelope = AABB<[f64; 2]>;
        fn envelope(&self) -> Self::Envelope {
            AABB::from_corners(
                [self.bounds[0], self.bounds[1]],
                [self.bounds[2], self.bounds[3]],
            )
        }
    }
    let half_extents: Vec<f64> = placements
        .iter()
        .map(|p| footprint_half_extent(&p.package))
        .collect();
    // Grid footprints (headers/connectors) additionally get an ORIENTED
    // window: the P&P stores each part's rotation, and a 1x40 header is a
    // 100 mm line, not a 100 mm square. See [`grid_window`].
    let grid_windows: Vec<Option<GridWindow>> = placements
        .iter()
        .map(|p| grid_window(&p.package, p.rotation))
        .collect();
    let comp_leaves: Vec<CompLeaf> = placements
        .iter()
        .enumerate()
        .map(|(i, p)| CompLeaf {
            bounds: [
                p.x - half_extents[i],
                p.y - half_extents[i],
                p.x + half_extents[i],
                p.y + half_extents[i],
            ],
            idx: i,
        })
        .collect();
    let comp_tree = RTree::bulk_load(comp_leaves);

    // For each component, the pads it claims (deduped by cell).
    let mut comp_pads: Vec<Vec<Pin>> = vec![Vec::new(); placements.len()];
    let mut comp_cells: Vec<std::collections::HashSet<(i64, i64)>> =
        vec![std::collections::HashSet::new(); placements.len()];
    let mut assigned_flashes = 0usize;
    let mut x2_bound_pads = 0usize;
    // Components the FILM names that the P&P does not place (or there is no
    // P&P at all): refdes -> (pads, dedupe cells, layer tally). BTreeMap so
    // film-only components come out in a stable order.
    let mut film_comps: std::collections::BTreeMap<
        String,
        (Vec<Pin>, std::collections::HashSet<(i64, i64)>, Vec<usize>),
    > = std::collections::BTreeMap::new();

    if x2_pins_present {
        // ── Film-identity binding ────────────────────────────────────────────
        // Every pad flash names its component and pin, so binding is exact:
        // the pin NUMBER is the film's, not an invention from claim order, and
        // a flash with no `.P` is, by the film's own account, not a component
        // pad (a via, a fiducial, thermal stitching), so the footprint window
        // never gets the chance to claim it as one.
        let refdes_to_placement: HashMap<&str, usize> = placements
            .iter()
            .enumerate()
            .map(|(i, p)| (p.reference.as_str(), i))
            .collect();
        for &gi in &x2_pin_flashes {
            let (refdes, pin_name) = prims[gi]
                .attrs
                .pin
                .clone()
                .expect("x2_pin_flashes holds only flashes with a pin attribute");
            let (cx, cy) = prims[gi].shape.center();
            let cell = ((cx * 20.0).round() as i64, (cy * 20.0).round() as i64);
            let net = net_of_prim[gi];
            let pin = Pin {
                number: pin_name.to_string(),
                net: if net == 0 { None } else { Some(net) },
                function: String::new(),
                kind: String::new(),
                position: Some((cx, cy)),
            };
            assigned_flashes += 1;
            x2_bound_pads += 1;
            match refdes_to_placement.get(&*refdes) {
                Some(&ci) => {
                    // The same physical pad flashed on several films (a THT
                    // pad's top+bottom rings) collapses to one, like before.
                    if comp_cells[ci].insert(cell) {
                        comp_pads[ci].push(pin);
                    }
                }
                None => {
                    let entry = film_comps.entry(refdes.to_string()).or_default();
                    if entry.1.insert(cell) {
                        entry.0.push(pin);
                        entry.2.push(prim_layer[gi]);
                    }
                }
            }
        }
        // The film's pin names order the pads (numeric-aware, so "10" follows
        // "2"); claim order meant nothing once numbers stopped being invented.
        fn pin_key(p: &Pin) -> (bool, Option<u64>, String) {
            let n = p.number.parse::<u64>().ok();
            (n.is_none(), n, p.number.clone())
        }
        for pads in comp_pads.iter_mut() {
            pads.sort_by_key(pin_key);
        }
        for (pads, _, _) in film_comps.values_mut() {
            pads.sort_by_key(pin_key);
        }
    }
    {
        // ── Geometric fallback ───────────────────────────────────────────────
        // On a stripped job this is every flash: the pre-X2 path, unchanged.
        // On an X2 job it is only the flashes of layers whose film carries no
        // pin identity (a legacy film mixed in); attributed layers were fully
        // accounted for above. Invented claim-order numbers continue after
        // any film-bound pins the same component already holds.
        for &gi in &geometric_flashes {
            let (cx, cy) = prims[gi].shape.center();
            // Candidate components whose window covers this flash; pick the
            // nearest.
            let mut best: Option<(f64, usize)> = None;
            let point_box = AABB::from_corners([cx, cy], [cx, cy]);
            for leaf in comp_tree.locate_in_envelope_intersecting(point_box) {
                let pl = &placements[leaf.idx];
                // A grid footprint's window is oriented by the part's stored
                // rotation; the square envelope alone let a long header claim
                // flashes sitting far off its pin row.
                if let Some(w) = &grid_windows[leaf.idx] {
                    if !w.contains(cx - pl.x, cy - pl.y) {
                        continue;
                    }
                }
                let d = (pl.x - cx).hypot(pl.y - cy);
                if best.map(|(bd, _)| d < bd).unwrap_or(true) {
                    best = Some((d, leaf.idx));
                }
            }
            let Some((_, ci)) = best else { continue };
            assigned_flashes += 1;
            let cell = ((cx * 20.0).round() as i64, (cy * 20.0).round() as i64);
            if !comp_cells[ci].insert(cell) {
                continue; // same physical pad already recorded for this component
            }
            let net = net_of_prim[gi];
            let n = comp_pads[ci].len() + 1;
            comp_pads[ci].push(Pin {
                number: n.to_string(),
                net: if net == 0 { None } else { Some(net) },
                function: String::new(),
                kind: String::new(),
                position: Some((cx, cy)),
            });
        }
    }

    let mut components: Vec<Component> = Vec::with_capacity(placements.len() + film_comps.len());
    for (i, pl) in placements.iter().enumerate() {
        components.push(Component {
            reference: pl.reference.clone(),
            value: pl.value.clone(),
            lib_id: String::new(),
            footprint: pl.package.clone(),
            position: Some((pl.x, pl.y, pl.rotation)),
            layer: if pl.top { "F.Cu".into() } else { "B.Cu".into() },
            properties: Vec::new(),
            dnp: pl.dnp,
            pins: std::mem::take(&mut comp_pads[i]),
        });
    }
    // Components the film binds that no P&P row places. The film knows the
    // refdes, the pins and their nets; it does not know the value or package,
    // so those stay empty rather than being invented. Position is the pad
    // centroid; the layer is the one most of its pads sit on.
    let x2_film_components = film_comps.len();
    for (refdes, (pins, _, layers_of)) in film_comps {
        let n = pins.len().max(1) as f64;
        let (sx, sy) = pins
            .iter()
            .filter_map(|p| p.position)
            .fold((0.0, 0.0), |(ax, ay), (x, y)| (ax + x, ay + y));
        let top_pads = layers_of.iter().filter(|&&l| l == 0).count();
        components.push(Component {
            reference: refdes,
            value: String::new(),
            lib_id: String::new(),
            footprint: String::new(),
            position: Some((sx / n, sy / n, 0.0)),
            layer: if top_pads * 2 >= layers_of.len() {
                "F.Cu".into()
            } else {
                "B.Cu".into()
            },
            properties: Vec::new(),
            dnp: false,
            pins,
        });
    }

    // ── Per-net copper geometry (for the gerber trace-current surface) ───────
    // A drawn copper track (`PrimKind::Track`) is a finite-width capsule whose
    // width is exactly `2*r` (the aperture diameter). A pour (`PrimKind::Region`)
    // is a plane: its true cross-section is not a discrete-segment width, so a
    // net carrying any region is `Poured` and out of the discrete-width check's
    // reach, exactly as in the native-CAD trace_current module. Flashes (pads)
    // and vias are not conductor-length segments, so they do not set the
    // bottleneck width. This is computed here, where `net_of_prim` and the
    // primitive shapes are both in scope, and surfaced for the gerber
    // trace-current sweep.
    let net_name_of: HashMap<i64, String> = nets.iter().map(|n| (n.id, n.name.clone())).collect();
    let mut min_w: HashMap<i64, f64> = HashMap::new();
    let mut max_w: HashMap<i64, f64> = HashMap::new();
    let mut track_count: HashMap<i64, usize> = HashMap::new();
    let mut region_count: HashMap<i64, usize> = HashMap::new();
    for gi in 0..prims.len() {
        let net = net_of_prim[gi];
        if net == 0 {
            continue;
        }
        match prims[gi].kind {
            PrimKind::Track => {
                if let Shape::Capsule(c) = &prims[gi].shape {
                    let w = c.r * 2.0;
                    if w > 0.0 {
                        min_w.entry(net).and_modify(|m| *m = m.min(w)).or_insert(w);
                        max_w.entry(net).and_modify(|m| *m = m.max(w)).or_insert(w);
                        *track_count.entry(net).or_default() += 1;
                    }
                }
            }
            PrimKind::Region => {
                *region_count.entry(net).or_default() += 1;
            }
            _ => {}
        }
    }
    let mut net_copper: Vec<GerberNetCopper> = nets
        .iter()
        .map(|n| {
            let rc = region_count.get(&n.id).copied().unwrap_or(0);
            let tc = track_count.get(&n.id).copied().unwrap_or(0);
            let kind = if rc > 0 {
                GerberCopperKind::Poured
            } else if tc > 0 {
                GerberCopperKind::Traces
            } else {
                GerberCopperKind::None
            };
            GerberNetCopper {
                net_id: n.id,
                name: net_name_of.get(&n.id).cloned().unwrap_or_default(),
                kind,
                min_track_width_mm: min_w.get(&n.id).copied(),
                max_track_width_mm: max_w.get(&n.id).copied(),
                track_count: tc,
                region_count: rc,
            }
        })
        .collect();
    net_copper.sort_by_key(|nc| nc.net_id);

    let stats = ReconStats {
        n_layers,
        n_nets: nets.len(),
        n_components: components.len(),
        n_holes: holes.len(),
        total_flashes,
        assigned_flashes,
        unassigned_flashes: total_flashes.saturating_sub(assigned_flashes),
        gnd_detected: gnd_net.is_some() || nets.iter().any(|n| n.name == "GND"),
        net_copper,
        n_slots,
        refused_span_holes,
        refused_plating_files: 0,
        n_castellations: 0,
        x2_named_nets,
        x2_bound_pads,
        x2_film_components,
        notes,
    };

    (
        ExtractedBoard {
            name: name.to_string(),
            nets,
            components,
        },
        stats,
    )
}

/// Reconstruction accounting, surfaced for the accuracy/honesty reporting.
#[derive(Debug, Clone)]
pub struct ReconStats {
    pub n_layers: usize,
    pub n_nets: usize,
    pub n_components: usize,
    pub n_holes: usize,
    pub total_flashes: usize,
    pub assigned_flashes: usize,
    pub unassigned_flashes: usize,
    pub gnd_detected: bool,
    /// Per-net copper geometry reconstructed from the gerber primitives: the
    /// narrowest drawn track width (the series bottleneck) and whether the net
    /// carries a pour. Feeds the gerber trace-current surface, where copper
    /// width is exact from the manufacturing files.
    pub net_copper: Vec<GerberNetCopper>,
    /// Plated hits recovered as slots (a routed stadium) rather than round
    /// holes. Counted inside `n_holes`, not on top of it.
    pub n_slots: usize,
    /// Drill files dropped whole because nothing in the job said whether their
    /// holes are plated. Their hits are absent from `n_holes` and stitch
    /// nothing; each is named in `notes`.
    pub refused_plating_files: usize,
    /// Plated hits whose copper layer span the files did not let us resolve.
    /// These stitch nothing and are named in `notes`. A non-zero value means
    /// the reconstruction is deliberately UNDER-connected on this job.
    pub refused_span_holes: usize,
    /// Plated hits whose barrel is cut by the board outline: castellations and
    /// edge slots. The connectivity claim for one is that its pad and its
    /// plated wall are a single node, which the geometry pass already makes;
    /// this count exists so that claim is auditable rather than assumed.
    pub n_castellations: usize,
    /// Nets whose name came from the film's own `%TO.N` X2 attribute instead
    /// of a synthetic `NET_n`. Non-zero means net identity is the exporter's,
    /// not a geometric inference.
    pub x2_named_nets: usize,
    /// Pad flashes bound to their component and pin by the film's `%TO.P`
    /// attribute. Non-zero means pin numbers are real, not claim-order.
    pub x2_bound_pads: usize,
    /// Components created purely from film identity: refdes the X2 attributes
    /// name that no P&P row places (or the job has no P&P at all).
    pub x2_film_components: usize,
    /// Reader notes: what this job made us refuse, and why. Surfaced verbatim
    /// so a refusal is visible instead of looking like a clean extraction.
    pub notes: Vec<String>,
}

impl ReconStats {
    /// Everything about this reconstruction a user must be told, as report
    /// sentences: the reader's own refusal notes, plus the pad-location
    /// accounting.
    ///
    /// The accounting existed on this struct from the start and reached nothing
    /// but an example binary, so a job where a third of the pads landed on no
    /// component produced a report that looked exactly like a complete one. A
    /// closed-loop percentage computed off this reconstruction only scores the
    /// pads that WERE located, which is a claim about part of the board being
    /// presented as a claim about the board.
    ///
    /// Note the precise scope of an unmatched flash: it is copper, so it still
    /// joins whatever net it touches during connectivity reconstruction. What it
    /// lacks is a component and a pin, which is what makes every per-part figure
    /// partial.
    pub fn coverage_notes(&self) -> Vec<String> {
        let mut out = self.notes.clone();
        if self.unassigned_flashes > 0 {
            let pct = if self.total_flashes > 0 {
                100.0 * self.assigned_flashes as f64 / self.total_flashes as f64
            } else {
                0.0
            };
            out.push(format!(
                "gerber reconstruction: {} of {} aperture flashes ({:.0}%) were matched to a \
                 placed component; {} were not. Not every flash is a component pad (via lands, \
                 fiducials and test points are flashed too), so the unmatched count is an \
                 upper bound on missing pads rather than a list of them. An unmatched flash \
                 still joins the copper net it touches, but belongs to no component here, so \
                 it carries no pin, and every component-level figure (including any \
                 closed-loop percentage) scores only the matched ones. Where the missing \
                 flashes ARE component pads, a pick-and-place file (.csv / .pos, or an Allegro \
                 smt_loc.txt) covering those parts will place them.",
                self.assigned_flashes, self.total_flashes, pct, self.unassigned_flashes,
            ));
        }
        out
    }
}

/// How a reconstructed net's copper is realised, mirroring
/// [`crate::trace_current::CopperKind`] but sourced from gerber primitives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GerberCopperKind {
    /// Only drawn tracks: the narrowest width is the conductor bottleneck.
    Traces,
    /// At least one pour region: the plane's true cross-section is not a
    /// discrete-segment width, so the net is out of the width check's reach.
    Poured,
    /// No drawn track and no region (a net made only of pad flashes / vias).
    None,
}

/// Per-net copper geometry from the gerber reconstruction.
#[derive(Debug, Clone)]
pub struct GerberNetCopper {
    pub net_id: i64,
    pub name: String,
    pub kind: GerberCopperKind,
    /// Narrowest drawn track on the net (mm), if any tracks exist.
    pub min_track_width_mm: Option<f64>,
    /// Widest drawn track on the net (mm).
    pub max_track_width_mm: Option<f64>,
    /// Number of drawn track primitives on the net.
    pub track_count: usize,
    /// Number of pour region primitives on the net.
    pub region_count: usize,
}

/// Half the search window (mm) for a footprint's pads, inferred from the
/// package name. A pad belongs to a component if it falls inside the
/// component-centred box of this half-extent. Generous on purpose: better to
/// consider a far pad and let the nearest-first claim sort it out than to miss
/// a pad of a large IC.
fn footprint_half_extent(package: &str) -> f64 {
    let p = package.to_ascii_lowercase();
    // Two-terminal chip packages: imperial code -> body length.
    for (code, ext) in [
        ("0201", 0.4),
        ("0402", 0.7),
        ("0603", 1.1),
        ("0805", 1.4),
        ("1206", 2.2),
        ("1210", 2.2),
        ("2010", 3.0),
        ("2512", 4.0),
    ] {
        if p.contains(code) {
            return ext;
        }
    }
    // Common IC families -> rough half-body + lead span.
    if p.contains("sot23") {
        return 1.8;
    }
    if p.contains("sot223") {
        return 4.0;
    }
    if p.contains("soic") || p.contains("so8") || p.contains("so-8") {
        return 4.0;
    }
    if p.contains("tssop") || p.contains("msop") {
        return 3.5;
    }
    if p.contains("sod") {
        return 2.0;
    }
    // Pin headers / connectors: `PinHeader_2x18_P2.54mm`, `Conn_01x02`. The
    // body spans (cols * pitch) by (rows * pitch); window the larger span. We
    // parse the `RxC` grid and the `P<pitch>mm` if present (default 2.54mm).
    if let Some((rows, cols)) = grid_hint(&p) {
        let pitch = pitch_hint(&p).unwrap_or(2.54);
        let span = (rows.max(cols) as f64) * pitch;
        // KiCad places a header's origin at pin 1 (a corner), not the body
        // centre, so pads extend the *full* span from the placed point in one
        // direction. Use the whole span (plus a pad margin) as the half-extent
        // so the far end of a long header is still inside the window.
        return span + pitch;
    }
    // QFN/QFP/BGA and large parts: scale with any NxN-mm body hint.
    if let Some(mm) = largest_dimension_hint(&p) {
        return mm / 2.0 + 1.0;
    }
    if p.contains("qfn") || p.contains("qfp") || p.contains("lqfp") || p.contains("bga") {
        return 6.0;
    }
    if p.contains("dip") || p.contains("dil") {
        return 8.0;
    }
    // Connectors, crystals, unknown: a moderate window.
    4.0
}

/// The oriented pad window of a grid footprint (pin header / connector),
/// rotated by the placement's stored rotation.
///
/// The square window sized for a long header's full span is enormous in BOTH
/// axes (a 1x40 header gets a ~100 mm square), while the part itself is a
/// line. The P&P row stores the rotation, so the window can be the part's
/// actual shape: a box `long` half-extent along the pin row and `cross`
/// half-extent across it, rotated by the placement angle. Which of the
/// footprint's two grid dimensions runs along the placement's local x is not
/// derivable from the name alone, so BOTH orientations of the box are
/// accepted (their union is still a small subset of the old square). Non-grid
/// packages keep the square window unchanged.
struct GridWindow {
    /// Half-extent along the pin row (the full span, pin-1-corner origins).
    long: f64,
    /// Half-extent across the rows.
    cross: f64,
    sin: f64,
    cos: f64,
}

impl GridWindow {
    /// Whether the offset (dx, dy) from the placement point falls inside the
    /// oriented window (either orientation of the row axis).
    fn contains(&self, dx: f64, dy: f64) -> bool {
        let u = (dx * self.cos + dy * self.sin).abs();
        let v = (-dx * self.sin + dy * self.cos).abs();
        (u <= self.long && v <= self.cross) || (u <= self.cross && v <= self.long)
    }
}

/// The oriented window for `package`, if it names a pin grid.
fn grid_window(package: &str, rotation_deg: f64) -> Option<GridWindow> {
    let p = package.to_ascii_lowercase();
    let (rows, cols) = grid_hint(&p)?;
    let pitch = pitch_hint(&p).unwrap_or(2.54);
    // Same margins as `footprint_half_extent`: the row span is the FULL grid
    // length (KiCad headers origin at pin 1), plus one pitch of slack.
    let long = (rows.max(cols) as f64) * pitch + pitch;
    let cross = (rows.min(cols) as f64) * pitch + pitch;
    let (sin, cos) = rotation_deg.to_radians().sin_cos();
    Some(GridWindow {
        long,
        cross,
        sin,
        cos,
    })
}

/// Parse a `RxC` pin grid from a connector footprint name (`2x18`, `01x02`).
/// Returns `(rows, cols)`; either may be 1.
fn grid_hint(p: &str) -> Option<(u32, u32)> {
    let bytes: Vec<char> = p.chars().collect();
    for i in 0..bytes.len() {
        if bytes[i] == 'x' && i > 0 && i + 1 < bytes.len() {
            // Walk across a decimal point too, so a body dimension like
            // "3.2x2.5mm" captures left="3.2"/right="2.5" (which then fail the
            // u32 parse and drop out) instead of the integer fragments "2"/"2"
            // that touch the 'x', those parsed as a bogus 2x2 pin grid and
            // oversized the crystal pad window. A real pin grid ("2x18") has no
            // '.', so this does not change it.
            let is_grid_char = |c: char| c.is_ascii_digit() || c == '.';
            let mut a = i;
            while a > 0 && is_grid_char(bytes[a - 1]) {
                a -= 1;
            }
            let mut b = i + 1;
            while b < bytes.len() && is_grid_char(bytes[b]) {
                b += 1;
            }
            let left: String = bytes[a..i].iter().collect();
            let right: String = bytes[i + 1..b].iter().collect();
            if let (Ok(l), Ok(r)) = (left.parse::<u32>(), right.parse::<u32>()) {
                // A pin grid (counts) not a body-size (would have a '.' or 'mm').
                let is_mm = p.get(b..b + 2).map(|s| s == "mm").unwrap_or(false)
                    || left.contains('.')
                    || right.contains('.');
                if l >= 1 && r >= 1 && l <= 200 && r <= 200 && !is_mm {
                    return Some((l, r));
                }
            }
        }
    }
    None
}

/// Parse `P2.54mm` / `P1.27mm` pitch (mm) from a footprint name.
fn pitch_hint(p: &str) -> Option<f64> {
    // Match the pitch token `p<number>mm`, i.e. a 'p' IMMEDIATELY followed by a
    // digit, not merely the first 'p' in the name. `pinheader_2x18_p2.54mm` has
    // its first 'p' in "pinheader"; keying on that read no digits and fell back
    // to the 2.54 mm default, so any non-2.54 header (P1.27mm, P5.08mm, ...) was
    // silently mis-pitched. The caller already lowercased `p`.
    let chars: Vec<char> = p.chars().collect();
    for i in 0..chars.len() {
        if chars[i] == 'p' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit() {
            let num: String = chars[i + 1..]
                .iter()
                .take_while(|c| c.is_ascii_digit() || **c == '.')
                .collect();
            if let Ok(v) = num.parse::<f64>() {
                if (0.3..=10.0).contains(&v) {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Pull an `NxM`-millimetre hint out of a footprint name (e.g.
/// `QFN-56-1EP_7x7mm`). Returns the larger dimension if found.
fn largest_dimension_hint(p: &str) -> Option<f64> {
    // Find a pattern like "7x7" or "7.0x7.0".
    let bytes: Vec<char> = p.chars().collect();
    for i in 0..bytes.len() {
        if bytes[i] == 'x' && i > 0 && i + 1 < bytes.len() {
            // Walk back for the first number.
            let mut a = i;
            while a > 0 && (bytes[a - 1].is_ascii_digit() || bytes[a - 1] == '.') {
                a -= 1;
            }
            let mut b = i + 1;
            while b < bytes.len() && (bytes[b].is_ascii_digit() || bytes[b] == '.') {
                b += 1;
            }
            let left: String = bytes[a..i].iter().collect();
            let right: String = bytes[i + 1..b].iter().collect();
            if let (Ok(l), Ok(r)) = (left.parse::<f64>(), right.parse::<f64>()) {
                if l > 0.0 && r > 0.0 && l < 100.0 && r < 100.0 {
                    return Some(l.max(r));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gerber::geo::Capsule;

    fn stats_with_flashes(total: usize, assigned: usize) -> ReconStats {
        ReconStats {
            n_layers: 2,
            n_nets: 10,
            n_components: 5,
            n_holes: 0,
            total_flashes: total,
            assigned_flashes: assigned,
            unassigned_flashes: total.saturating_sub(assigned),
            gnd_detected: true,
            net_copper: Vec::new(),
            n_slots: 0,
            refused_plating_files: 0,
            refused_span_holes: 0,
            n_castellations: 0,
            notes: Vec::new(),
        }
    }

    #[test]
    fn unlocated_pads_are_reported_with_the_closed_loop_caveat() {
        // The accounting existed on ReconStats from the start and reached nothing
        // but an example binary, so a job where a third of the pads landed on no
        // component read exactly like a complete reconstruction.
        let notes = stats_with_flashes(300, 200).coverage_notes();
        let note = notes
            .iter()
            .find(|n| n.contains("aperture flashes"))
            .expect("the flash accounting must be reported");
        assert!(note.contains("200 of 300"), "{note}");
        assert!(note.contains("67%"), "states the located share: {note}");
        assert!(note.contains("100 were not"), "{note}");
        assert!(
            note.contains("closed-loop"),
            "the percentage caveat must reach the user: {note}"
        );
        assert!(
            note.contains("pick-and-place"),
            "names the unlocking upload: {note}"
        );
        // Not every flash is a pad (via lands, fiducials and test points are
        // flashed too), so the count must not be presented as a list of pads.
        assert!(
            note.contains("upper bound") && note.contains("fiducials"),
            "must not claim every unmatched flash is a component pad: {note}"
        );
    }

    #[test]
    fn a_fully_located_job_gains_no_accounting_note() {
        // Every pad placed: no note, so the channel stays worth reading.
        assert!(stats_with_flashes(300, 300).coverage_notes().is_empty());
    }

    #[test]
    fn reader_refusal_notes_are_carried_verbatim() {
        // coverage_notes must not drop the reader's own refusals.
        let mut stats = stats_with_flashes(10, 10);
        stats.notes.push("refused a drill file".to_string());
        assert_eq!(stats.coverage_notes(), vec!["refused a drill file"]);
    }

    fn cap(ax: f64, ay: f64, bx: f64, by: f64, r: f64, kind: PrimKind) -> CopperPrim {
        CopperPrim::bare(Shape::Capsule(Capsule { ax, ay, bx, by, r }), kind)
    }

    #[test]
    fn pitch_hint_keys_on_the_p_before_a_digit() {
        // Round-26: the pitch token is a 'p' IMMEDIATELY followed by a digit.
        // Keying on the FIRST 'p' matched the 'p' in "pinheader", read no digits,
        // and silently mis-pitched every non-2.54 header. The header's real pitch
        // must be recovered regardless of leading p-words in the name.
        assert_eq!(pitch_hint("pinheader_2x20_p1.27mm"), Some(1.27));
        assert_eq!(pitch_hint("pinheader_1x40_p2.54mm"), Some(2.54));
        assert_eq!(pitch_hint("connector_pin_socket_p5.08mm"), Some(5.08));
        // No pitch token at all → no hint (caller applies its own default).
        assert_eq!(pitch_hint("pinheader_generic"), None);
    }

    #[test]
    fn grid_hint_rejects_decimal_body_sizes() {
        // R36: the digit walk stopped at the decimal point, so a body dimension
        // like "3.2x2.5mm" captured the integer fragments "2"/"2" that touch the
        // 'x' and returned a bogus 2x2 pin grid, oversizing the crystal pad
        // window ~3x and letting it claim stray orphan flashes. A decimal body
        // size must not read as a grid.
        assert_eq!(grid_hint("crystal_smd_3225-2pin_3.2x2.5mm"), None);
        assert_eq!(grid_hint("2.0x1.6mm"), None);
        assert_eq!(grid_hint("5.0x3.2mm"), None);
        // Integer "mm" body sizes are rejected too.
        assert_eq!(grid_hint("12x12mm"), None);
        // Genuine pin grids (integer counts, no '.') still parse.
        assert_eq!(grid_hint("2x18"), Some((2, 18)));
        assert_eq!(grid_hint("01x02"), Some((1, 2)));

        // The decimal crystal body must not inflate the pad-search half-extent:
        // it takes the largest-dimension path (3.2/2 + 1 = 2.6 mm), not the
        // 2x2-grid path (~7.62 mm).
        let he = footprint_half_extent("Crystal_SMD_3225-2Pin_3.2x2.5mm");
        assert!(
            he < 4.0,
            "decimal crystal body half-extent must be small, got {he}"
        );
    }

    #[test]
    fn two_pads_one_track_one_net() {
        // pad - track - pad, all touching: one net, two flashes.
        let layer = vec![
            cap(0.0, 0.0, 0.0, 0.0, 0.5, PrimKind::Flash),
            cap(0.0, 0.0, 5.0, 0.0, 0.1, PrimKind::Track),
            cap(5.0, 0.0, 5.0, 0.0, 0.5, PrimKind::Flash),
        ];
        let (_board, stats) = reconstruct("t", vec![layer], vec![], vec![]);
        assert_eq!(stats.n_nets, 1, "all copper is one conductor");
        assert_eq!(stats.total_flashes, 2);
    }

    #[test]
    fn separate_copper_two_nets() {
        let layer = vec![
            cap(0.0, 0.0, 0.0, 0.0, 0.5, PrimKind::Flash),
            cap(10.0, 0.0, 10.0, 0.0, 0.5, PrimKind::Flash),
        ];
        let (_b, stats) = reconstruct("t", vec![layer], vec![], vec![]);
        assert_eq!(stats.n_nets, 2);
    }

    #[test]
    fn gnd_label_is_deterministic_on_a_copper_count_tie() {
        // Round-8 #14: two separate region pours with EQUAL primitive counts
        // tie on "most copper". Iterating the HashMap keys made the GND label
        // land on whichever tied net came first in iteration order, flaky
        // across extractions. The tiebreak now always labels the lowest net id.
        let layer = vec![
            cap(0.0, 0.0, 0.0, 0.0, 1.0, PrimKind::Region),
            cap(10.0, 0.0, 10.0, 0.0, 1.0, PrimKind::Region),
        ];
        let (board, stats) = reconstruct("t", vec![layer], vec![], vec![]);
        assert_eq!(stats.n_nets, 2, "two separate pours are two nets");
        let gnd = board
            .nets
            .iter()
            .find(|n| n.name == "GND")
            .expect("a GND net");
        let min_id = board.nets.iter().map(|n| n.id).min().unwrap();
        assert_eq!(
            gnd.id, min_id,
            "on a copper-count tie GND must label the lowest net id, deterministically"
        );
    }

    #[test]
    fn via_stitches_layers() {
        let top = vec![cap(0.0, 0.0, 0.0, 0.0, 0.5, PrimKind::Flash)];
        let bot = vec![cap(0.0, 0.0, 0.0, 0.0, 0.5, PrimKind::Flash)];
        // Without a hole, the two layers are independent: 2 nets.
        let (_b, s0) = reconstruct("t", vec![top.clone(), bot.clone()], vec![], vec![]);
        assert_eq!(s0.n_nets, 2);
        // With a plated hole at the shared point: 1 net.
        let hole = PlatedHole::through(0.0, 0.0, 0.3);
        let (_b, s1) = reconstruct("t", vec![top, bot], vec![hole], vec![]);
        assert_eq!(s1.n_nets, 1, "via stitches top and bottom into one net");
    }

    #[test]
    fn blind_via_stitches_only_the_layers_it_spans() {
        // Four layers, a pad at the same (x, y) on each. A blind via spanning
        // L1-L2 must join exactly those two pads and leave L3 and L4 alone:
        // 3 nets (the L1+L2 pair, L3, L4). Treated as a through-hole it would
        // be 1 net, which is a short the stackup does not contain.
        let pad = || vec![cap(0.0, 0.0, 0.0, 0.0, 0.5, PrimKind::Flash)];
        let layers = vec![pad(), pad(), pad(), pad()];
        let blind = PlatedHole {
            x: 0.0,
            y: 0.0,
            diameter: 0.3,
            to: None,
            span: LayerSpan::Range { from: 0, to: 1 },
        };
        let (_b, s) = reconstruct("t", layers.clone(), vec![blind], vec![]);
        assert_eq!(s.n_nets, 3, "a blind L1-L2 via joins only L1 and L2");

        // The same drill as a through-hole is the one net, so the fixture is
        // not passing because the geometry happened not to touch.
        let (_b, s_thru) = reconstruct(
            "t",
            layers,
            vec![PlatedHole::through(0.0, 0.0, 0.3)],
            vec![],
        );
        assert_eq!(s_thru.n_nets, 1);
    }

    #[test]
    fn buried_via_leaves_both_outer_layers_out() {
        // A buried L2-L3 via on a four-layer stack: the two inner pads join,
        // the two outer pads stay separate. 3 nets.
        let pad = || vec![cap(0.0, 0.0, 0.0, 0.0, 0.5, PrimKind::Flash)];
        let buried = PlatedHole {
            x: 0.0,
            y: 0.0,
            diameter: 0.3,
            to: None,
            span: LayerSpan::Range { from: 1, to: 2 },
        };
        let (_b, s) = reconstruct("t", vec![pad(), pad(), pad(), pad()], vec![buried], vec![]);
        assert_eq!(s.n_nets, 3);
    }

    #[test]
    fn an_unresolvable_span_stitches_nothing_and_is_counted() {
        // The refusal: four pads stacked at one point, a plated hit whose span
        // the files did not give us. Every layer stays its own net and the
        // refusal is counted, rather than four nets being merged on a guess.
        let pad = || vec![cap(0.0, 0.0, 0.0, 0.0, 0.5, PrimKind::Flash)];
        let unknown = PlatedHole {
            x: 0.0,
            y: 0.0,
            diameter: 0.3,
            to: None,
            span: LayerSpan::Unknown,
        };
        let (_b, s) = reconstruct("t", vec![pad(), pad(), pad(), pad()], vec![unknown], vec![]);
        assert_eq!(s.n_nets, 4, "an unknown span must not stitch anything");
        assert_eq!(s.refused_span_holes, 1);
        assert_eq!(s.n_holes, 1, "the hit is still reported, just not stitched");
    }

    #[test]
    fn a_declared_span_naming_a_missing_layer_is_refused_not_widened() {
        // `Plated,1,6,PTH` on a two-layer job names a layer the stack does not
        // have. Clamping that to the stack would silently turn it into a
        // through-hole; the honest read is that we could not resolve it.
        let pad = || vec![cap(0.0, 0.0, 0.0, 0.0, 0.5, PrimKind::Flash)];
        let bad = PlatedHole {
            x: 0.0,
            y: 0.0,
            diameter: 0.3,
            to: None,
            span: LayerSpan::Range { from: 0, to: 5 },
        };
        let (_b, s) = reconstruct("t", vec![pad(), pad()], vec![bad], vec![]);
        assert_eq!(s.n_nets, 2);
        assert_eq!(s.refused_span_holes, 1);
    }

    #[test]
    fn a_plated_slot_connects_copper_along_its_whole_wall() {
        // Two pads 6 mm apart on one layer, with nothing between them: two
        // nets. A plated slot routed from one to the other has a wall touching
        // both, so they become one. Neither pad sits over the slot's start
        // point alone, so this cannot pass by endpoint coincidence: a slot read
        // as a round hole at its start leaves the far pad unconnected.
        let layer = vec![
            cap(0.0, 0.0, 0.0, 0.0, 0.5, PrimKind::Flash),
            cap(6.0, 0.0, 6.0, 0.0, 0.5, PrimKind::Flash),
        ];
        let (_b, s0) = reconstruct("t", vec![layer.clone()], vec![], vec![]);
        assert_eq!(s0.n_nets, 2);

        let slot = PlatedHole {
            x: 0.0,
            y: 0.0,
            diameter: 0.6,
            to: Some((6.0, 0.0)),
            span: LayerSpan::Through,
        };
        let (_b, s1) = reconstruct("t", vec![layer.clone()], vec![slot], vec![]);
        assert_eq!(s1.n_nets, 1, "the plated wall joins both pads");
        assert_eq!(s1.n_slots, 1);

        // The same hit read as a round hole at the start point (what the reader
        // did before G85 was understood) leaves the far pad on its own net.
        let (_b, s2) = reconstruct(
            "t",
            vec![layer],
            vec![PlatedHole::through(0.0, 0.0, 0.6)],
            vec![],
        );
        assert_eq!(
            s2.n_nets, 2,
            "a round hole at the start reaches only one pad"
        );
    }

    #[test]
    fn a_slot_wall_joins_copper_it_only_grazes_mid_span() {
        // A pad sitting beside the MIDDLE of a slot, touching the wall but
        // nowhere near either end. Tessellating a slot as two end discs, or as
        // a chord that cuts the corner, misses this contact.
        let layer = vec![
            cap(0.0, 0.0, 0.0, 0.0, 0.2, PrimKind::Flash),
            // Wall of a 0.6 mm-wide slot along y = 0 reaches y = +-0.3. A 0.2 mm
            // pad centred at (3.0, 0.5) has its edge at y = 0.3: tangent.
            cap(3.0, 0.5, 3.0, 0.5, 0.2, PrimKind::Flash),
        ];
        let slot = PlatedHole {
            x: 0.0,
            y: 0.0,
            diameter: 0.6,
            to: Some((6.0, 0.0)),
            span: LayerSpan::Through,
        };
        let (_b, s) = reconstruct("t", vec![layer], vec![slot], vec![]);
        assert_eq!(s.n_nets, 1, "a mid-span tangent contact is a connection");
    }

    /// A pad flash carrying X2 identity: `%TO.P,<refdes>,<pin>` + `%TO.N,<net>`.
    fn x2_pad(x: f64, y: f64, refdes: &str, pin: &str, net: &str) -> CopperPrim {
        CopperPrim {
            shape: Shape::disc(x, y, 0.3),
            kind: PrimKind::Flash,
            attrs: X2Attrs {
                pin: Some((refdes.into(), pin.into())),
                net: Some(Arc::from(vec![Arc::from(net)])),
                ..Default::default()
            },
        }
    }

    fn placement(reference: &str, x: f64, y: f64) -> Placement {
        Placement {
            reference: reference.into(),
            value: String::new(),
            package: "R_0402_1005Metric".into(),
            x,
            y,
            rotation: 0.0,
            top: true,
            dnp: false,
        }
    }

    #[test]
    fn x2_pin_identity_binds_refdes_pin_and_net_from_the_film() {
        // Two pads whose film names their refdes, pin and net. The pin numbers
        // must be the FILM's (here deliberately flashed in reverse order, so
        // claim-order numbering would swap them), and the nets must carry the
        // film's names.
        let layer = vec![
            x2_pad(1.0, 0.0, "R1", "2", "GND"),
            x2_pad(0.0, 0.0, "R1", "1", "VCC"),
        ];
        let (board, stats) = reconstruct("t", vec![layer], vec![], vec![placement("R1", 0.5, 0.0)]);
        assert_eq!(stats.x2_bound_pads, 2);
        assert_eq!(board.components.len(), 1);
        let pins = &board.components[0].pins;
        assert_eq!(pins.len(), 2);
        assert_eq!(
            pins[0].number, "1",
            "the film's pin number, not claim order"
        );
        assert_eq!(pins[1].number, "2");
        let net_name = |id: Option<i64>| {
            board
                .nets
                .iter()
                .find(|n| Some(n.id) == id)
                .map(|n| n.name.clone())
                .unwrap()
        };
        assert_eq!(net_name(pins[0].net), "VCC");
        assert_eq!(net_name(pins[1].net), "GND");
        assert_eq!(stats.x2_named_nets, 2);
    }

    #[test]
    fn x2_pins_build_components_even_without_a_placement_file() {
        // No P&P at all: the film alone names the component and its pads.
        let layer = vec![
            x2_pad(0.0, 0.0, "U3", "1", "VCC"),
            x2_pad(1.0, 0.0, "U3", "2", "GND"),
            x2_pad(5.0, 5.0, "R9", "1", "GND"),
        ];
        let (board, stats) = reconstruct("t", vec![layer], vec![], vec![]);
        assert_eq!(stats.x2_film_components, 2);
        let refs: Vec<&str> = board
            .components
            .iter()
            .map(|c| c.reference.as_str())
            .collect();
        assert_eq!(refs, vec!["R9", "U3"], "stable refdes order");
        let u3 = board
            .components
            .iter()
            .find(|c| c.reference == "U3")
            .unwrap();
        assert_eq!(u3.pins.len(), 2);
        assert_eq!(u3.pins[0].number, "1");
        assert!(
            u3.value.is_empty() && u3.footprint.is_empty(),
            "the film does not know value/package; they are not invented"
        );
    }

    #[test]
    fn x2_net_names_union_copper_the_film_says_is_one_conductor() {
        // Two pads 10 mm apart with NO copper joining them, both declared
        // `%TO.N,SCL`: the film says one net (the routing may live on a film
        // this job did not ship), so they are one net with the film's name.
        let layer = vec![
            x2_pad(0.0, 0.0, "R1", "1", "SCL"),
            x2_pad(10.0, 0.0, "U1", "14", "SCL"),
        ];
        let (board, stats) = reconstruct("t", vec![layer], vec![], vec![]);
        assert_eq!(stats.n_nets, 1, "same %TO.N name = same conductor");
        assert_eq!(board.nets[0].name, "SCL");
        // The join came from the film's word, not from traced copper, so it
        // is disclosed: silently uniting them would mask a genuine open.
        assert!(
            stats.notes.iter().any(|n| n.contains("X2 net names alone")),
            "a film-only join must be named in the notes: {:?}",
            stats.notes
        );

        // Two pads whose copper genuinely touches and shares a name: no
        // film-only join happened, so no note.
        let touching = vec![
            x2_pad(0.0, 0.0, "R1", "1", "SCL"),
            x2_pad(0.4, 0.0, "U1", "14", "SCL"),
        ];
        let (_b, stats) = reconstruct("t", vec![touching], vec![], vec![]);
        assert_eq!(stats.n_nets, 1);
        assert!(
            !stats.notes.iter().any(|n| n.contains("X2 net names alone")),
            "geometry-backed joins are not film-only joins"
        );
    }

    #[test]
    fn a_net_tie_object_ties_its_nets_without_a_phantom_conflict() {
        // A net-tie flash carries SEVERAL names (`%TO.N,A,B*%`): copper that
        // belongs to both nets by design. The tie must union A and B, the
        // merged net keeps both names visible ("A/B"), and NO conflict note
        // fires, the film itself declared the join. (Collapsing the names to
        // one opaque "A,B" string neither unioned them nor read as a tie.)
        let tie = CopperPrim {
            shape: Shape::disc(5.0, 0.0, 0.3),
            kind: PrimKind::Flash,
            attrs: X2Attrs {
                net: Some(Arc::from(vec![Arc::from("A"), Arc::from("B")])),
                ..Default::default()
            },
        };
        let layer = vec![
            x2_pad(0.0, 0.0, "R1", "1", "A"),
            x2_pad(10.0, 0.0, "R2", "1", "B"),
            tie,
        ];
        let (board, stats) = reconstruct("t", vec![layer], vec![], vec![]);
        assert_eq!(stats.n_nets, 1, "the tie object unions A and B");
        assert_eq!(board.nets[0].name, "A/B", "both tied names stay visible");
        assert!(
            !stats.notes.iter().any(|n| n.contains("separate nets")),
            "a film-declared tie is not a conflict: {:?}",
            stats.notes
        );
    }

    #[test]
    fn a_partially_attributed_film_keeps_geometric_binding_for_bare_pads() {
        // One film where SOME pads carry %TO.P and one pad flash does not
        // (partial X2: merged films, subset-attributing exporters). Absence
        // of .P is not a non-pad assertion, so the bare flash must still
        // bind geometrically instead of silently vanishing.
        let layer = vec![
            x2_pad(0.0, 0.0, "R1", "1", "VCC"),
            cap(20.0, 0.0, 20.0, 0.0, 0.3, PrimKind::Flash),
        ];
        let (board, stats) = reconstruct(
            "t",
            vec![layer],
            vec![],
            vec![placement("R1", 0.0, 0.0), placement("R2", 20.0, 0.0)],
        );
        assert_eq!(stats.x2_bound_pads, 1);
        let by_ref: HashMap<&str, usize> = board
            .components
            .iter()
            .map(|c| (c.reference.as_str(), c.pins.len()))
            .collect();
        assert_eq!(by_ref["R1"], 1);
        assert_eq!(by_ref["R2"], 1, "the bare pad on the SAME film still binds");
    }

    #[test]
    fn a_legacy_film_mixed_into_an_x2_job_keeps_its_geometric_binding() {
        // Layer 0 is an X2 film (its pads carry %TO.P); layer 1 is a legacy
        // film with a bare flash sitting under placed component R2. Gating
        // the whole job on the X2 film's presence silently deleted R2's pad;
        // the geometric window must still bind flashes of unattributed films.
        let x2_layer = vec![x2_pad(0.0, 0.0, "R1", "1", "VCC")];
        let legacy_layer = vec![cap(20.0, 0.0, 20.0, 0.0, 0.3, PrimKind::Flash)];
        let (board, stats) = reconstruct(
            "t",
            vec![x2_layer, legacy_layer],
            vec![],
            vec![placement("R1", 0.0, 0.0), placement("R2", 20.0, 0.0)],
        );
        assert_eq!(stats.x2_bound_pads, 1);
        let by_ref: HashMap<&str, usize> = board
            .components
            .iter()
            .map(|c| (c.reference.as_str(), c.pins.len()))
            .collect();
        assert_eq!(by_ref["R1"], 1, "the film-bound pad");
        assert_eq!(
            by_ref["R2"], 1,
            "the legacy film's flash still binds geometrically"
        );
    }

    #[test]
    fn geometry_bridging_two_x2_nets_is_named_not_silently_resolved() {
        // Two pads whose copper TOUCHES while the film assigns them different
        // nets: a real short or an over-merge. The net keeps its synthetic
        // name and the disagreement is a note, never a silent pick.
        let layer = vec![
            x2_pad(0.0, 0.0, "R1", "1", "VCC"),
            x2_pad(0.2, 0.0, "R2", "1", "GND"),
        ];
        let (board, stats) = reconstruct("t", vec![layer], vec![], vec![]);
        assert_eq!(stats.n_nets, 1, "the copper genuinely touches");
        assert!(
            board.nets[0].name.starts_with("NET_"),
            "a conflicted net keeps its synthetic name, got {}",
            board.nets[0].name
        );
        assert!(
            stats
                .notes
                .iter()
                .any(|n| n.contains("VCC") && n.contains("GND")),
            "the disagreement names both film nets: {:?}",
            stats.notes
        );
    }

    #[test]
    fn without_x2_attributes_the_geometric_fallback_is_untouched() {
        // The stripped-film side of the two-sided guarantee, at the unit
        // level: bare prims produce claim-order pin numbers and synthetic
        // net names, exactly as before the X2 reader existed.
        let layer = vec![
            cap(0.0, 0.0, 0.0, 0.0, 0.3, PrimKind::Flash),
            cap(1.0, 0.0, 1.0, 0.0, 0.3, PrimKind::Flash),
        ];
        let (board, stats) = reconstruct("t", vec![layer], vec![], vec![placement("R1", 0.5, 0.0)]);
        assert_eq!(stats.x2_bound_pads, 0);
        assert_eq!(stats.x2_named_nets, 0);
        assert_eq!(stats.x2_film_components, 0);
        let pins = &board.components[0].pins;
        assert_eq!(pins.len(), 2);
        assert_eq!(pins[0].number, "1", "invented claim-order numbering stays");
        assert_eq!(pins[1].number, "2");
        assert!(board.nets.iter().all(|n| n.name.starts_with("NET_")));
    }

    #[test]
    fn a_header_window_follows_its_stored_rotation() {
        // A 1x40 header is a ~100 mm LINE. Its old square window was ~104 mm
        // in BOTH axes, so it claimed flashes sitting nowhere near its pin
        // row. The P&P stores the rotation; at 45° the window must follow it:
        // a pad on the rotated row axis is claimed, a stray flash the same
        // distance out on the unrotated axis is not.
        let header = |rot: f64| Placement {
            reference: "J1".into(),
            value: String::new(),
            package: "PinHeader_1x40_P2.54mm".into(),
            x: 0.0,
            y: 0.0,
            rotation: rot,
            top: true,
            dnp: false,
        };
        let on_axis_45 = 42.4; // (42.4, 42.4) is ~60 mm along the 45° row
        let layer = vec![
            cap(
                on_axis_45,
                on_axis_45,
                on_axis_45,
                on_axis_45,
                0.3,
                PrimKind::Flash,
            ),
            cap(60.0, 0.0, 60.0, 0.0, 0.3, PrimKind::Flash),
        ];
        let (board, stats) = reconstruct("t", vec![layer.clone()], vec![], vec![header(45.0)]);
        assert_eq!(
            stats.assigned_flashes, 1,
            "only the pad on the rotated pin row belongs to the header"
        );
        let pos = board.components[0].pins[0].position.unwrap();
        assert!(
            (pos.0 - on_axis_45).abs() < 1e-6,
            "the claimed pad is the on-axis one"
        );

        // At rotation 0 the same header claims the (60, 0) pad instead.
        let (_b, stats) = reconstruct("t", vec![layer], vec![], vec![header(0.0)]);
        assert_eq!(stats.assigned_flashes, 1);
    }

    #[test]
    fn placement_claims_nearest_pad() {
        let layer = vec![
            cap(0.0, 0.0, 0.0, 0.0, 0.3, PrimKind::Flash),
            cap(1.0, 0.0, 1.0, 0.0, 0.3, PrimKind::Flash),
        ];
        let pl = Placement {
            reference: "R1".into(),
            value: "10k".into(),
            package: "R_0402_1005Metric".into(),
            x: 0.5,
            y: 0.0,
            rotation: 0.0,
            top: true,
            dnp: false,
        };
        let (board, stats) = reconstruct("t", vec![layer], vec![], vec![pl]);
        assert_eq!(board.components.len(), 1);
        assert_eq!(board.components[0].pins.len(), 2, "0402 claims both pads");
        assert_eq!(stats.assigned_flashes, 2);
    }
}
