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
//! plated drill at (x, y) is a disc on *every* copper layer and also unions the
//! layers' primitives that cover it, so vias and through-hole pads stitch the
//! stack. The connected components of the union-find are the nets; each gets a
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

use rstar::{RTree, RTreeObject, AABB};

use crate::{Component, ExtractedBoard, Net, Pin};

use super::geo::{shape_gap, Shape};
use super::placement::Placement;
use super::rs274x::{CopperPrim, PrimKind};

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

/// A drilled, plated hole: a via/PTH that exists on every copper layer.
pub struct PlatedHole {
    pub x: f64,
    pub y: f64,
    pub diameter: f64,
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
    // Plated holes become a disc primitive on each layer (PrimKind::Flash so a
    // through-hole pad anchors a component) plus a cross-layer stitch.
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
            });
            prim_layer.push(li);
        }
    }

    // Hole discs: one per layer, recorded so we can stitch them together after.
    // hole_prims[h] = vector of global prim indices, one per layer.
    let mut hole_prims: Vec<Vec<usize>> = Vec::with_capacity(holes.len());
    for h in &holes {
        let mut idxs = Vec::with_capacity(n_layers);
        for li in 0..n_layers {
            let shape = Shape::disc(h.x, h.y, (h.diameter / 2.0).max(0.05));
            let bounds = shape.bounds();
            let gi = prims.len();
            prims.push(LayerPrim {
                shape,
                kind: PrimKind::Via,
                bounds,
            });
            prim_layer.push(li);
            // Extend that layer's range to include the hole disc. Holes are
            // appended after all layer primitives, so we widen the end marker.
            idxs.push(gi);
        }
        hole_prims.push(idxs);
    }

    let mut dsu = Dsu::new(prims.len());

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
                    if let Shape::Polygon { pts, .. } = &prims[rgi].shape {
                        let grid = region_grids.get(&rgi);
                        // (a) Containment: a sample point inside the filled
                        // outline means the pour copper is *on* that primitive.
                        // Large pours use the grid; small ones test directly.
                        let inside = test_pts.iter().any(|&(px, py)| {
                            if px < b[0] || px > b[2] || py < b[1] || py > b[3] {
                                return false;
                            }
                            match grid {
                                Some(g) => g.contains(px, py),
                                None => super::geo::point_in_polygon(px, py, pts),
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

    // GND heuristic: among region-touching nets, the one with the most copper.
    // Break count ties by lowest net id (Reverse) so the GND label is stable —
    // iterating the HashMap keys left it dependent on iteration order when two
    // pours tied on primitive count.
    let gnd_net = net_touches_region
        .keys()
        .copied()
        .max_by_key(|&n| (net_prim_count.get(&n).copied().unwrap_or(0), std::cmp::Reverse(n)));

    // Build the Net table with names.
    let mut nets: Vec<Net> = root_to_net
        .values()
        .copied()
        .map(|id| {
            let name = if Some(id) == gnd_net {
                "GND".to_string()
            } else {
                format!("NET_{id}")
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

    for &gi in &flash_idxs {
        let (cx, cy) = prims[gi].shape.center();
        // Candidate components whose window covers this flash; pick the nearest.
        let mut best: Option<(f64, usize)> = None;
        let point_box = AABB::from_corners([cx, cy], [cx, cy]);
        for leaf in comp_tree.locate_in_envelope_intersecting(point_box) {
            let pl = &placements[leaf.idx];
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

    let mut components: Vec<Component> = Vec::with_capacity(placements.len());
    for (i, pl) in placements.iter().enumerate() {
        components.push(Component {
            reference: pl.reference.clone(),
            value: pl.value.clone(),
            lib_id: String::new(),
            footprint: pl.package.clone(),
            position: Some((pl.x, pl.y, pl.rotation)),
            layer: if pl.top { "F.Cu".into() } else { "B.Cu".into() },
            properties: Vec::new(),
            dnp: false,
            pins: std::mem::take(&mut comp_pads[i]),
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
        gnd_detected: gnd_net.is_some(),
        net_copper,
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

/// Parse a `RxC` pin grid from a connector footprint name (`2x18`, `01x02`).
/// Returns `(rows, cols)`; either may be 1.
fn grid_hint(p: &str) -> Option<(u32, u32)> {
    let bytes: Vec<char> = p.chars().collect();
    for i in 0..bytes.len() {
        if bytes[i] == 'x' && i > 0 && i + 1 < bytes.len() {
            let mut a = i;
            while a > 0 && bytes[a - 1].is_ascii_digit() {
                a -= 1;
            }
            let mut b = i + 1;
            while b < bytes.len() && bytes[b].is_ascii_digit() {
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
    let idx = p.find('p')?;
    let tail = &p[idx + 1..];
    let num: String = tail
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let v: f64 = num.parse().ok()?;
    if (0.3..=10.0).contains(&v) {
        Some(v)
    } else {
        None
    }
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

    fn cap(ax: f64, ay: f64, bx: f64, by: f64, r: f64, kind: PrimKind) -> CopperPrim {
        CopperPrim {
            shape: Shape::Capsule(Capsule { ax, ay, bx, by, r }),
            kind,
        }
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
        // land on whichever tied net came first in iteration order — flaky
        // across extractions. The tiebreak now always labels the lowest net id.
        let layer = vec![
            cap(0.0, 0.0, 0.0, 0.0, 1.0, PrimKind::Region),
            cap(10.0, 0.0, 10.0, 0.0, 1.0, PrimKind::Region),
        ];
        let (board, stats) = reconstruct("t", vec![layer], vec![], vec![]);
        assert_eq!(stats.n_nets, 2, "two separate pours are two nets");
        let gnd = board.nets.iter().find(|n| n.name == "GND").expect("a GND net");
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
        let hole = PlatedHole {
            x: 0.0,
            y: 0.0,
            diameter: 0.3,
        };
        let (_b, s1) = reconstruct("t", vec![top, bot], vec![hole], vec![]);
        assert_eq!(s1.n_nets, 1, "via stitches top and bottom into one net");
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
        };
        let (board, stats) = reconstruct("t", vec![layer], vec![], vec![pl]);
        assert_eq!(board.components.len(), 1);
        assert_eq!(board.components[0].pins.len(), 2, "0402 claims both pads");
        assert_eq!(stats.assigned_flashes, 2);
    }
}
