//! Geometry primitives and distance math shared by the DRC, the gerber
//! connectivity tracer and the SI checks: capsules, polygons, weighted
//! multi-contour pours, and the signed copper-gap between any two of them. One
//! set of numerics, so a touch here means the same thing a short means there.

/// A "stadium": a segment of finite width. A round flash/via is the degenerate
/// case `a == b`. All coordinates in board millimetres.
#[derive(Debug, Clone, Copy)]
pub struct Capsule {
    pub ax: f64,
    pub ay: f64,
    pub bx: f64,
    pub by: f64,
    pub r: f64,
}

/// A solid primitive's outline.
#[derive(Debug, Clone)]
pub enum Shape {
    /// Track segment, arc link, round pad/via.
    Capsule(Capsule),
    /// Closed polygon (rect/oval/poly/custom flash, region pour). `r` inflates
    /// the outline (corner radius of a roundrect carried as polygon + radius).
    Polygon { pts: Vec<(f64, f64)>, r: f64 },
    /// One connected piece of pour copper with holes: the first contour is the
    /// outer boundary, the rest carry signed coverage weights. Ordinary region
    /// holes have weight -1. Clear images applied to a dark region also have
    /// weight -1, while an island inside an annular clear has weight +1.
    /// Containment is the positive-coverage rule: a point is copper iff the sum
    /// of the weights of contours enclosing it is greater than zero. Unlike raw
    /// even-odd parity, two overlapping clear images therefore remain a void in
    /// their overlap instead of flipping that lens back to copper. Disjoint
    /// islands of one region are split into separate shapes upstream, so a
    /// `MultiPolygon` built by the region reader is a single electrically-connected
    /// piece and the union-find may rely on one shape describing one connected
    /// filled area. No inflation radius: pours are drawn at their true outline.
    MultiPolygon {
        contours: Vec<Vec<(f64, f64)>>,
        weights: Vec<i16>,
    },
}

impl Shape {
    pub fn disc(x: f64, y: f64, r: f64) -> Shape {
        Shape::Capsule(Capsule {
            ax: x,
            ay: y,
            bx: x,
            by: y,
            r,
        })
    }

    /// This shape shifted by `(dx, dy)` board millimetres. Used to tile a
    /// step-and-repeat base cell across its grid.
    pub fn translated(&self, dx: f64, dy: f64) -> Shape {
        let shift = |pts: &[(f64, f64)]| pts.iter().map(|(x, y)| (x + dx, y + dy)).collect();
        match self {
            Shape::Capsule(c) => Shape::Capsule(Capsule {
                ax: c.ax + dx,
                ay: c.ay + dy,
                bx: c.bx + dx,
                by: c.by + dy,
                r: c.r,
            }),
            Shape::Polygon { pts, r } => Shape::Polygon {
                pts: shift(pts),
                r: *r,
            },
            Shape::MultiPolygon { contours, weights } => Shape::MultiPolygon {
                contours: contours.iter().map(|c| shift(c)).collect(),
                weights: weights.clone(),
            },
        }
    }

    /// Inflated AABB (minx, miny, maxx, maxy).
    pub fn bounds(&self) -> [f64; 4] {
        match self {
            Shape::Capsule(c) => [
                c.ax.min(c.bx) - c.r,
                c.ay.min(c.by) - c.r,
                c.ax.max(c.bx) + c.r,
                c.ay.max(c.by) + c.r,
            ],
            Shape::Polygon { pts, r } => {
                let b = polygon_bounds(pts);
                [b[0] - r, b[1] - r, b[2] + r, b[3] + r]
            }
            Shape::MultiPolygon { contours, .. } => polygon_bounds(contours.iter().flatten()),
        }
    }

    /// A representative interior point (centroid-ish), for pad/flash matching
    /// and zone containment. For a ring this may fall in a hole, but the centre
    /// is only a *representative* point and pours never anchor pads.
    pub fn center(&self) -> (f64, f64) {
        match self {
            Shape::Capsule(c) => ((c.ax + c.bx) / 2.0, (c.ay + c.by) / 2.0),
            Shape::Polygon { pts, .. } => vertex_mean(pts),
            Shape::MultiPolygon { contours, .. } => {
                vertex_mean(contours.first().map(|c| c.as_slice()).unwrap_or(&[]))
            }
        }
    }
}

/// AABB (minx, miny, maxx, maxy) of a point list; infinite for an empty one.
pub fn polygon_bounds<'a>(pts: impl IntoIterator<Item = &'a (f64, f64)>) -> [f64; 4] {
    pts.into_iter().fold(
        [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ],
        |b, &(x, y)| [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)],
    )
}

fn vertex_mean(pts: &[(f64, f64)]) -> (f64, f64) {
    let n = pts.len().max(1) as f64;
    (
        pts.iter().map(|p| p.0).sum::<f64>() / n,
        pts.iter().map(|p| p.1).sum::<f64>() / n,
    )
}

// ── Distance helpers (all in mm) ─────────────────────────────────────────────

/// Closest point on segment AB to point P, with the squared distance.
pub fn point_seg_closest(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> ((f64, f64), f64) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let c = if len2 <= f64::EPSILON {
        a
    } else {
        let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0);
        (a.0 + t * dx, a.1 + t * dy)
    };
    let (ex, ey) = (p.0 - c.0, p.1 - c.1);
    (c, ex * ex + ey * ey)
}

/// Squared distance from point P to segment AB.
pub fn point_seg_dist2(px: f64, py: f64, ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    point_seg_closest((px, py), (ax, ay), (bx, by)).1
}

fn orient(p: (f64, f64), q: (f64, f64), r: (f64, f64)) -> f64 {
    (q.0 - p.0) * (r.1 - p.1) - (q.1 - p.1) * (r.0 - p.0)
}

fn on_seg(p: (f64, f64), q: (f64, f64), r: (f64, f64)) -> bool {
    q.0 <= p.0.max(r.0) && q.0 >= p.0.min(r.0) && q.1 <= p.1.max(r.1) && q.1 >= p.1.min(r.1)
}

pub fn segments_intersect(p1: (f64, f64), p2: (f64, f64), p3: (f64, f64), p4: (f64, f64)) -> bool {
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

/// Minimum distance between two segments plus the closest point pair (on AB,
/// on CD). 0 with the crossing point (both) when they cross; a colinear touch
/// falls through to the endpoint candidates, which find a zero-distance pair.
pub fn seg_seg_closest(
    a1: (f64, f64),
    a2: (f64, f64),
    b1: (f64, f64),
    b2: (f64, f64),
) -> (f64, (f64, f64), (f64, f64)) {
    if segments_intersect(a1, a2, b1, b2) {
        let (d1x, d1y) = (a2.0 - a1.0, a2.1 - a1.1);
        let (d2x, d2y) = (b2.0 - b1.0, b2.1 - b1.1);
        let denom = d1x * d2y - d1y * d2x;
        if denom.abs() > 1e-12 {
            let t = (((b1.0 - a1.0) * d2y - (b1.1 - a1.1) * d2x) / denom).clamp(0.0, 1.0);
            let p = (a1.0 + t * d1x, a1.1 + t * d1y);
            return (0.0, p, p);
        }
    }
    let candidates = [
        (b1, a1, a2, false),
        (b2, a1, a2, false),
        (a1, b1, b2, true),
        (a2, b1, b2, true),
    ];
    let mut best = (f64::INFINITY, a1, b1);
    for (p, s1, s2, p_on_a) in candidates {
        let (q, d2) = point_seg_closest(p, s1, s2);
        if d2 < best.0 {
            best = if p_on_a { (d2, p, q) } else { (d2, q, p) };
        }
    }
    (best.0.sqrt(), best.1, best.2)
}

pub fn seg_seg_dist(a1: (f64, f64), a2: (f64, f64), b1: (f64, f64), b2: (f64, f64)) -> f64 {
    seg_seg_closest(a1, a2, b1, b2).0
}

/// Even-odd containment over a set of closed contours: inside iff enclosed by
/// an odd number of them. This remains useful for one self-contained aperture
/// image (outer + holes); painted [`Shape::MultiPolygon`] values use signed
/// coverage instead.
pub fn point_in_contours(px: f64, py: f64, contours: &[Vec<(f64, f64)>]) -> bool {
    contours
        .iter()
        .fold(false, |inside, c| inside ^ point_in_polygon(px, py, c))
}

/// Signed containment for a painted region. Each contour contributes its
/// weight when it encloses the point; strictly positive coverage is copper.
/// The length mismatch is a construction defect, so fail closed as no copper
/// instead of silently reverting to parity.
pub fn point_in_weighted_contours(
    px: f64,
    py: f64,
    contours: &[Vec<(f64, f64)>],
    weights: &[i16],
) -> bool {
    contours.len() == weights.len()
        && contours
            .iter()
            .zip(weights)
            .filter(|(contour, _)| point_in_polygon(px, py, contour))
            .map(|(_, weight)| i32::from(*weight))
            .sum::<i32>()
            > 0
}

pub fn point_in_polygon(px: f64, py: f64, poly: &[(f64, f64)]) -> bool {
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

/// Minimum boundary-to-boundary distance between two polygons (0 if their
/// edges cross) plus the closest point pair (on `a`, on `b`). Containment is
/// the caller's business (point-in-polygon).
pub fn poly_poly_closest(a: &[(f64, f64)], b: &[(f64, f64)]) -> (f64, (f64, f64), (f64, f64)) {
    let mut best = (f64::INFINITY, (0.0, 0.0), (0.0, 0.0));
    if a.is_empty() || b.is_empty() {
        return best;
    }
    if a.len() < 2 || b.len() < 2 {
        // Degenerate: nearest a-vertex against b's boundary.
        for &pa in a {
            for (b1, b2) in contour_edges(b) {
                let (q, d2) = point_seg_closest(pa, b1, b2);
                if d2.sqrt() < best.0 {
                    best = (d2.sqrt(), pa, q);
                }
            }
        }
        return best;
    }
    for (a1, a2) in contour_edges(a) {
        for (b1, b2) in contour_edges(b) {
            let cand = seg_seg_closest(a1, a2, b1, b2);
            if cand.0 < best.0 {
                best = cand;
            }
        }
    }
    best
}

/// Nearest distance from segment AB to a closed contour's edges (0 when they
/// cross), with the closest points (on AB, on the contour). A one-point
/// contour falls back to point distance.
fn seg_contour_closest(
    a: (f64, f64),
    b: (f64, f64),
    pts: &[(f64, f64)],
) -> (f64, (f64, f64), (f64, f64)) {
    let mut best = (f64::INFINITY, a, a);
    if pts.len() == 1 {
        let (p, d2) = point_seg_closest(pts[0], a, b);
        return (d2.sqrt(), p, pts[0]);
    }
    for (c, d) in contour_edges(pts) {
        let cand = seg_seg_closest(a, b, c, d);
        if cand.0 < best.0 {
            best = cand;
        }
    }
    best
}

/// The reported location for a closest centerline/boundary point pair
/// `pa`/`pb` carrying copper radii `ra`/`rb`: the midpoint of the copper
/// edge-to-edge span along the closest-approach line. For a positive gap that
/// is the middle of the air gap; for an overlap it lands inside the shared
/// copper, clamped between `pa` and `pb` so deep penetrations stay on copper.
fn closest_approach_point(pa: (f64, f64), pb: (f64, f64), ra: f64, rb: f64) -> (f64, f64) {
    let (dx, dy) = (pb.0 - pa.0, pb.1 - pa.1);
    let d = (dx * dx + dy * dy).sqrt();
    if d <= f64::EPSILON {
        return pa;
    }
    let t = ((ra + d - rb) / (2.0 * d)).clamp(0.0, 1.0);
    (pa.0 + dx * t, pa.1 + dy * t)
}

/// Is there painted copper arbitrarily close to this stored contour point?
/// A clear contour can be buried inside another clear image, in which case it
/// is bookkeeping rather than a physical copper edge.
fn weighted_copper_near(p: (f64, f64), contours: &[Vec<(f64, f64)>], weights: &[i16]) -> bool {
    const E: f64 = 1e-7;
    [
        (E, 0.0),
        (-E, 0.0),
        (0.0, E),
        (0.0, -E),
        (E, E),
        (E, -E),
        (-E, E),
        (-E, -E),
    ]
    .into_iter()
    .any(|(dx, dy)| point_in_weighted_contours(p.0 + dx, p.1 + dy, contours, weights))
}

fn contour_edges(contour: &[(f64, f64)]) -> impl Iterator<Item = ((f64, f64), (f64, f64))> + '_ {
    let n = contour.len();
    (0..n).filter_map(move |i| (n >= 2).then_some((contour[(i + n - 1) % n], contour[i])))
}

/// Whether `(px, py)` lies on the copper of `shape`, corner radius included.
pub fn shape_contains_point(shape: &Shape, px: f64, py: f64) -> bool {
    match shape {
        Shape::Capsule(c) => point_seg_dist2(px, py, c.ax, c.ay, c.bx, c.by) <= c.r * c.r,
        Shape::Polygon { pts, r } => point_reaches_polygon((px, py), pts, *r),
        Shape::MultiPolygon { contours, weights } => {
            point_in_weighted_contours(px, py, contours, weights)
        }
    }
}

fn point_reaches_polygon(p: (f64, f64), polygon: &[(f64, f64)], radius: f64) -> bool {
    point_in_polygon(p.0, p.1, polygon)
        || contour_edges(polygon)
            .any(|(a, b)| point_seg_dist2(p.0, p.1, a.0, a.1, b.0, b.1) <= radius * radius)
}

/// The smallest candidate gap whose witness point sits on EXPOSED copper of
/// the weighted region; `None` when no candidate does.
fn nearest_exposed_gap(
    mut candidates: Vec<(f64, (f64, f64))>,
    contours: &[Vec<(f64, f64)>],
    weights: &[i16],
) -> Option<(f64, (f64, f64))> {
    candidates.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    candidates
        .into_iter()
        .find(|(_, point)| weighted_copper_near(*point, contours, weights))
}

fn capsule_weighted_gap(
    c: &Capsule,
    contours: &[Vec<(f64, f64)>],
    weights: &[i16],
) -> Option<(f64, (f64, f64))> {
    let (a, b) = ((c.ax, c.ay), (c.bx, c.by));
    if let Some(p) = [a, b, ((a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5)]
        .into_iter()
        .find(|p| point_in_weighted_contours(p.0, p.1, contours, weights))
    {
        return Some((-c.r.max(0.0) - 1e-6, p));
    }
    let candidates = contours
        .iter()
        .flat_map(|contour| contour_edges(contour))
        .map(|(e1, e2)| {
            let (distance, q, _) = seg_seg_closest(e1, e2, a, b);
            (distance - c.r, q)
        })
        .collect();
    nearest_exposed_gap(candidates, contours, weights)
}

fn polygon_weighted_gap(
    polygon: &[(f64, f64)],
    radius: f64,
    contours: &[Vec<(f64, f64)>],
    weights: &[i16],
) -> Option<(f64, (f64, f64))> {
    if let Some(&p) = polygon
        .iter()
        .find(|&&(x, y)| point_in_weighted_contours(x, y, contours, weights))
    {
        return Some((-radius.max(0.0) - 1e-6, p));
    }
    let mut candidates = Vec::new();
    for contour in contours {
        if let Some(&(x, y)) = contour
            .iter()
            .find(|&&(x, y)| point_reaches_polygon((x, y), polygon, radius))
        {
            candidates.push((-1e-6, (x, y)));
        }
        for (a, b) in contour_edges(contour) {
            let mut nearest = (f64::INFINITY, a);
            for (pa, pb) in contour_edges(polygon) {
                let (distance, q, _) = seg_seg_closest(a, b, pa, pb);
                if distance < nearest.0 {
                    nearest = (distance, q);
                }
            }
            candidates.push((nearest.0 - radius, nearest.1));
        }
    }
    nearest_exposed_gap(candidates, contours, weights)
}

fn weighted_regions_gap(
    ca: &[Vec<(f64, f64)>],
    wa: &[i16],
    cb: &[Vec<(f64, f64)>],
    wb: &[i16],
) -> Option<(f64, (f64, f64))> {
    for contour in ca {
        for &p in contour {
            if point_in_weighted_contours(p.0, p.1, cb, wb) && weighted_copper_near(p, ca, wa) {
                return Some((-1e-6, p));
            }
        }
        for (a, b) in contour_edges(contour) {
            for other in cb {
                for (c, d) in contour_edges(other) {
                    let (distance, q, r) = seg_seg_closest(a, b, c, d);
                    if distance == 0.0
                        && weighted_copper_near(q, ca, wa)
                        && weighted_copper_near(r, cb, wb)
                    {
                        return Some((0.0, q));
                    }
                }
            }
        }
    }
    cb.iter()
        .flatten()
        .find(|&&p| point_in_weighted_contours(p.0, p.1, ca, wa) && weighted_copper_near(p, cb, wb))
        .map(|&p| (-1e-6, p))
}

/// Signed copper-edge gap between two shapes. `<= 0` means the copper overlaps
/// (they are the same conductor); positive is the clear gap.
pub fn shape_gap(a: &Shape, b: &Shape) -> f64 {
    shape_gap_at(a, b).0
}

/// [`shape_gap`] plus the point of closest approach (see
/// [`closest_approach_point`]; for full containment, a point of the contained
/// copper), which is where a DRC finding is reported.
pub fn shape_gap_at(a: &Shape, b: &Shape) -> (f64, (f64, f64)) {
    match (a, b) {
        (Shape::Capsule(ca), Shape::Capsule(cb)) => {
            let (d, pa, pb) = seg_seg_closest(
                (ca.ax, ca.ay),
                (ca.bx, ca.by),
                (cb.ax, cb.ay),
                (cb.bx, cb.by),
            );
            (d - ca.r - cb.r, closest_approach_point(pa, pb, ca.r, cb.r))
        }
        (Shape::Capsule(c), Shape::Polygon { pts, r })
        | (Shape::Polygon { pts, r }, Shape::Capsule(c)) => {
            // Either capsule endpoint inside the polygon is a hard overlap
            // whatever the edge distance says, located at that endpoint.
            let (ea, eb) = ((c.ax, c.ay), (c.bx, c.by));
            for e in [ea, eb] {
                if point_in_polygon(e.0, e.1, pts) {
                    return (-(c.r + r).max(0.0) - 1e-6, e);
                }
            }
            let (d, pc, pp) = seg_contour_closest(ea, eb, pts);
            (d - c.r - r, closest_approach_point(pc, pp, c.r, *r))
        }
        (Shape::Polygon { pts: pa, r: ra }, Shape::Polygon { pts: pb, r: rb }) => {
            let (d, qa, qb) = poly_poly_closest(pa, pb);
            let edge = d - ra - rb;
            // Containment either way is a hard overlap, located at a vertex of
            // the contained outline.
            if pa.first().is_some_and(|&(x, y)| point_in_polygon(x, y, pb)) {
                return (edge.min(0.0) - 1e-6, pa[0]);
            }
            if pb.first().is_some_and(|&(x, y)| point_in_polygon(x, y, pa)) {
                return (edge.min(0.0) - 1e-6, pb[0]);
            }
            (edge, closest_approach_point(qa, qb, *ra, *rb))
        }
        // The multi-contour arms mirror the polygon arms above, but only an
        // EXPOSED contour is a copper edge. A clear contour buried under another
        // clear is bookkeeping, not copper. Containment uses signed coverage (a
        // capsule endpoint sitting in a hole is NOT contained; the hole is empty).
        (Shape::Capsule(c), Shape::MultiPolygon { contours, weights })
        | (Shape::MultiPolygon { contours, weights }, Shape::Capsule(c)) => {
            capsule_weighted_gap(c, contours, weights).unwrap_or((f64::INFINITY, a.center()))
        }
        (Shape::Polygon { pts, r }, Shape::MultiPolygon { contours, weights })
        | (Shape::MultiPolygon { contours, weights }, Shape::Polygon { pts, r }) => {
            polygon_weighted_gap(pts, *r, contours, weights).unwrap_or((f64::INFINITY, a.center()))
        }
        (
            Shape::MultiPolygon {
                contours: ca,
                weights: wa,
            },
            Shape::MultiPolygon {
                contours: cb,
                weights: wb,
            },
        ) => weighted_regions_gap(ca, wa, cb, wb).unwrap_or((f64::INFINITY, a.center())),
    }
}

/// A grid-accelerated point-in-polygon tester for a large fixed polygon (a
/// copper pour). Point-in-polygon is O(vertices); a board-spanning pour has
/// tens of thousands of vertices and is tested against every primitive, so the
/// naive cost is quadratic. This rasterises the polygon's bounding box into a
/// coarse grid once: each cell is tagged fully-inside, fully-outside, or
/// boundary. A query is then an O(1) grid lookup, falling back to the selected
/// exact containment rule only for points in a boundary cell.
pub struct PolyGrid<'a> {
    /// The contours the grid classifies. `weights == None` selects even-odd;
    /// otherwise the grid uses the same positive signed-coverage rule as a
    /// painted [`Shape::MultiPolygon`]. A negative-drawn plane is board-sized;
    /// leaving it to the exact test made every primitive pay its whole vertex count.
    contours: &'a [Vec<(f64, f64)>],
    weights: Option<&'a [i16]>,
    minx: f64,
    miny: f64,
    inv_cell: f64,
    nx: usize,
    ny: usize,
    /// 0 = outside, 1 = inside, 2 = boundary (needs exact test).
    cells: Vec<u8>,
}

impl<'a> PolyGrid<'a> {
    /// Build a grid for `contours`. `target_cells` is the rough number of cells
    /// along the longer axis (more = finer = fewer exact fallbacks, more build
    /// cost).
    pub fn new(contours: &'a [Vec<(f64, f64)>], target_cells: usize) -> Self {
        Self::build(contours, None, target_cells)
    }

    /// Build a grid for a signed painted region. A mismatched weight vector is
    /// represented as entirely outside, matching [`point_in_weighted_contours`].
    pub fn new_weighted(
        contours: &'a [Vec<(f64, f64)>],
        weights: &'a [i16],
        target_cells: usize,
    ) -> Self {
        Self::build(contours, Some(weights), target_cells)
    }

    fn build(
        contours: &'a [Vec<(f64, f64)>],
        weights: Option<&'a [i16]>,
        target_cells: usize,
    ) -> Self {
        let mut minx = f64::INFINITY;
        let mut miny = f64::INFINITY;
        let mut maxx = f64::NEG_INFINITY;
        let mut maxy = f64::NEG_INFINITY;
        for &(x, y) in contours.iter().flatten() {
            minx = minx.min(x);
            miny = miny.min(y);
            maxx = maxx.max(x);
            maxy = maxy.max(y);
        }
        let span = (maxx - minx).max(maxy - miny).max(1e-6);
        let cell = span / target_cells.max(1) as f64;
        let inv_cell = 1.0 / cell;
        let nx = (((maxx - minx) * inv_cell).ceil() as usize + 1).max(1);
        let ny = (((maxy - miny) * inv_cell).ceil() as usize + 1).max(1);

        // Mark cells any contour's boundary passes through as boundary (2): a
        // query landing here falls back to the selected exact containment rule.
        let mut cells = vec![0u8; nx * ny];
        for pts in contours {
            let n = pts.len();
            if n < 2 {
                continue;
            }
            let mut j = n - 1;
            for i in 0..n {
                Self::stamp_edge(&mut cells, nx, ny, minx, miny, inv_cell, pts[j], pts[i]);
                j = i;
            }
        }
        // Classify the non-boundary cells exactly by a scanline sweep rather than
        // per-cell polygon tests (which would be O(cells x vertices)). For each
        // grid row we intersect the edges crossing that row's centre line with it,
        // sort the crossing x's, and fill the inside spans. The unweighted path
        // uses parity. The weighted path pairs each contour's crossings into
        // intervals and sweeps signed enter/leave events, filling only positive
        // coverage. This is exact (no flood-fill leakage). Boundary cells retain
        // their exact-test flag.
        //
        // The edges are bucketed by the rows they span first. Walking every edge
        // for every row is O(rows x vertices), and rows scale with the vertex
        // count, so a plane with thousands of annular antipads pays seconds here.
        // Bucketing makes it O(vertices + rows swept + cells): each EDGE of an
        // antipad's outline spans two or three rows.
        if contours.iter().any(|c| c.len() >= 3) {
            let row_of = |y: f64| -> isize { ((y - miny) * inv_cell).floor() as isize };
            // One copy of each non-horizontal edge, plus the row it becomes active
            // in and the row it expires after: an active-edge sweep. Pushing a copy
            // of an edge into every row it spans is O(vertices x rows) MEMORY, and
            // rows scale with the vertex count, so a comb-shaped pour whose fingers
            // each span the board height (2048 fingers, 8192 vertices) took 396 MB
            // here. An edge is now stored once.
            let mut edges: Vec<(f64, f64, f64, f64, usize)> = Vec::new();
            let mut expires: Vec<usize> = Vec::new();
            let mut starts: Vec<Vec<usize>> = vec![Vec::new(); ny];
            for (contour_index, pts) in contours.iter().enumerate() {
                let n = pts.len();
                if n < 3 {
                    continue;
                }
                let mut j = n - 1;
                for i in 0..n {
                    let (a, b) = (pts[j], pts[i]);
                    j = i;
                    if a.1 == b.1 {
                        continue; // horizontal: crosses no row centre line
                    }
                    let (lo, hi) = if a.1 < b.1 { (a.1, b.1) } else { (b.1, a.1) };
                    let r0 = row_of(lo).clamp(0, ny as isize - 1) as usize;
                    let r1 = row_of(hi).clamp(0, ny as isize - 1) as usize;
                    starts[r0].push(edges.len());
                    edges.push((a.0, a.1, b.0, b.1, contour_index));
                    expires.push(r1);
                }
            }
            let mut active: Vec<usize> = Vec::new();
            let mut xs: Vec<f64> = Vec::new();
            for gy in 0..ny {
                active.extend(starts[gy].iter().copied());
                active.retain(|&e| expires[e] >= gy);
                if active.is_empty() {
                    continue;
                }
                let yc = miny + (gy as f64 + 0.5) / inv_cell;
                xs.clear();
                let mut fill_span = |x0: f64, x1: f64| {
                    let g0 = (((x0 - minx) * inv_cell).floor() as isize).max(0);
                    let g1 = (((x1 - minx) * inv_cell).ceil() as isize).min(nx as isize - 1);
                    for gx in g0..=g1 {
                        let cxc = minx + (gx as f64 + 0.5) / inv_cell;
                        let idx = gy * nx + gx as usize;
                        if cells[idx] != 2 && cxc >= x0 && cxc <= x1 {
                            cells[idx] = 1;
                        }
                    }
                };
                if let Some(weights) = weights.filter(|weights| weights.len() == contours.len()) {
                    let mut crossings: Vec<(usize, f64)> = Vec::with_capacity(active.len());
                    for &e in &active {
                        let (ax, ay, bx, by, contour_index) = edges[e];
                        if (ay > yc) != (by > yc) {
                            crossings.push((contour_index, (bx - ax) * (yc - ay) / (by - ay) + ax));
                        }
                    }
                    crossings.sort_by(|a, b| {
                        a.0.cmp(&b.0).then_with(|| {
                            a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)
                        })
                    });
                    let mut events: Vec<(f64, i32)> = Vec::with_capacity(crossings.len());
                    let mut k = 0;
                    while k < crossings.len() {
                        let contour_index = crossings[k].0;
                        let mut end = k + 1;
                        while end < crossings.len() && crossings[end].0 == contour_index {
                            end += 1;
                        }
                        let mut pair = k;
                        while pair + 1 < end {
                            let weight = i32::from(weights[contour_index]);
                            events.push((crossings[pair].1, weight));
                            events.push((crossings[pair + 1].1, -weight));
                            pair += 2;
                        }
                        k = end;
                    }
                    events
                        .sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
                    let mut coverage = 0i32;
                    let mut previous: Option<f64> = None;
                    let mut event = 0;
                    while event < events.len() {
                        let x = events[event].0;
                        if coverage > 0 {
                            if let Some(x0) = previous {
                                fill_span(x0, x);
                            }
                        }
                        let mut delta = 0;
                        while event < events.len() && events[event].0 == x {
                            delta += events[event].1;
                            event += 1;
                        }
                        coverage += delta;
                        previous = Some(x);
                    }
                } else if weights.is_none() {
                    for &e in &active {
                        let (ax, ay, bx, by, _) = edges[e];
                        if (ay > yc) != (by > yc) {
                            xs.push((bx - ax) * (yc - ay) / (by - ay) + ax);
                        }
                    }
                    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    // Inside the spans [xs[0],xs[1]], [xs[2],xs[3]], ...
                    let mut k = 0;
                    while k + 1 < xs.len() {
                        fill_span(xs[k], xs[k + 1]);
                        k += 2;
                    }
                }
            }
        }

        PolyGrid {
            contours,
            weights,
            minx,
            miny,
            inv_cell,
            nx,
            ny,
            cells,
        }
    }

    /// Mark every cell the segment `a..b` passes through.
    ///
    /// A supercover traversal (Amanatides-Woo): step from the cell holding `a` to
    /// the cell holding `b`, always crossing whichever of the next vertical or
    /// horizontal cell boundary the segment reaches first. Point-sampling the
    /// segment at one-cell spacing does NOT do this: whenever a step advances
    /// both cell coordinates, the cell the segment crossed in between is never
    /// visited. A missed cell then takes its classification from the scanline
    /// parity at its CENTRE, so every query in it on the far side of the boundary
    /// gets the wrong answer and `near_boundary` can answer "no boundary here"
    /// over a boundary that is really there.
    fn stamp_edge(
        cells: &mut [u8],
        nx: usize,
        ny: usize,
        minx: f64,
        miny: f64,
        inv_cell: f64,
        a: (f64, f64),
        b: (f64, f64),
    ) {
        let mark = |cells: &mut [u8], gx: isize, gy: isize| {
            if gx >= 0 && gy >= 0 && (gx as usize) < nx && (gy as usize) < ny {
                cells[gy as usize * nx + gx as usize] = 2;
            }
        };
        // Cell coordinates, in units where one cell is 1.0.
        let (ax, ay) = ((a.0 - minx) * inv_cell, (a.1 - miny) * inv_cell);
        let (bx, by) = ((b.0 - minx) * inv_cell, (b.1 - miny) * inv_cell);
        let (mut gx, mut gy) = (ax.floor() as isize, ay.floor() as isize);
        let (tx, ty) = (bx.floor() as isize, by.floor() as isize);
        mark(cells, gx, gy);
        if (gx, gy) == (tx, ty) {
            return;
        }
        let (dx, dy) = (bx - ax, by - ay);
        let stepx: isize = if dx > 0.0 {
            1
        } else if dx < 0.0 {
            -1
        } else {
            0
        };
        let stepy: isize = if dy > 0.0 {
            1
        } else if dy < 0.0 {
            -1
        } else {
            0
        };
        // Parameter (0..1 along the segment) of the next crossing on each axis,
        // and the parameter step between successive crossings.
        let next = |v: f64, g: isize, step: isize, d: f64| -> f64 {
            if step == 0 {
                f64::INFINITY
            } else {
                let edge = if step > 0 { g as f64 + 1.0 } else { g as f64 };
                (edge - v) / d
            }
        };
        let mut tmx = next(ax, gx, stepx, dx);
        let mut tmy = next(ay, gy, stepy, dy);
        let ddx = if stepx == 0 {
            f64::INFINITY
        } else {
            1.0 / dx.abs()
        };
        let ddy = if stepy == 0 {
            f64::INFINITY
        } else {
            1.0 / dy.abs()
        };
        // The traversal visits at most one cell per unit crossed on either axis,
        // so this bound cannot cut a legitimate walk short; it only stops a walk
        // that non-finite coordinates would otherwise run forever.
        let limit = (dx.abs() + dy.abs()).ceil().max(1.0) as usize + 4;
        for _ in 0..limit {
            if tmx <= tmy {
                gx += stepx;
                tmx += ddx;
            } else {
                gy += stepy;
                tmy += ddy;
            }
            mark(cells, gx, gy);
            if (gx, gy) == (tx, ty) {
                return;
            }
        }
    }

    /// Is `(px, py)` inside the shape? O(1) grid lookup, exact test only on a
    /// boundary cell.
    pub fn contains(&self, px: f64, py: f64) -> bool {
        // `floor`, not a truncating cast: a query up to one cell left of `minx` or
        // below `miny` truncates to index 0 and reads the edge row/column instead
        // of falling outside the grid. Unreachable through today's callers, which
        // pre-filter by the grid's own extent, and a trap for the next one.
        let gx = ((px - self.minx) * self.inv_cell).floor() as isize;
        let gy = ((py - self.miny) * self.inv_cell).floor() as isize;
        if gx < 0 || gy < 0 || gx as usize >= self.nx || gy as usize >= self.ny {
            return false;
        }
        match self.cells[gy as usize * self.nx + gx as usize] {
            0 => false,
            1 => true,
            _ => self.weights.map_or_else(
                || point_in_contours(px, py, self.contours),
                |weights| point_in_weighted_contours(px, py, self.contours, weights),
            ),
        }
    }

    /// Could any contour's boundary pass through this bounding box?
    ///
    /// A boundary edge that cuts a primitive's copper necessarily crosses a cell
    /// the primitive's bounds cover, and every cell an edge crosses is stamped
    /// boundary, so `false` is a sound refusal: nothing there to penetrate. The
    /// point is to keep the exact poly-poly distance off the hot path, which on a
    /// board-sized pour every primitive's bounds would otherwise reach.
    pub fn near_boundary(&self, b: [f64; 4]) -> bool {
        let cx0 = (((b[0] - self.minx) * self.inv_cell).floor() as isize).max(0);
        let cy0 = (((b[1] - self.miny) * self.inv_cell).floor() as isize).max(0);
        let cx1 = (((b[2] - self.minx) * self.inv_cell).ceil() as isize).min(self.nx as isize - 1);
        let cy1 = (((b[3] - self.miny) * self.inv_cell).ceil() as isize).min(self.ny as isize - 1);
        if cx1 < cx0 || cy1 < cy0 {
            return false;
        }
        for gy in cy0..=cy1 {
            for gx in cx0..=cx1 {
                if self.cells[gy as usize * self.nx + gx as usize] == 2 {
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poly_grid_matches_exact() {
        // An L-shaped polygon: grid containment must equal exact for many points.
        let poly = vec![
            (0.0, 0.0),
            (4.0, 0.0),
            (4.0, 2.0),
            (2.0, 2.0),
            (2.0, 4.0),
            (0.0, 4.0),
        ];
        let ring = vec![poly.clone()];
        let grid = PolyGrid::new(&ring, 32);
        for i in 0..50 {
            for j in 0..50 {
                let x = i as f64 * 0.1 - 0.5;
                let y = j as f64 * 0.1 - 0.5;
                assert_eq!(
                    grid.contains(x, y),
                    point_in_polygon(x, y, &poly),
                    "mismatch at ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn poly_grid_matches_exact_on_diagonal_edges_and_holes() {
        // The case a point-sampled edge stamp got wrong. Axis-aligned edges hide
        // it: a diagonal edge advances both cell coordinates in one step, and the
        // cell it crossed in between goes unstamped, so it takes its answer from
        // the scanline parity at its CENTRE and every query in it on the far side
        // of the boundary is wrong. A rotated square at 512 cells missed 524 such
        // cells. Rotated, off-origin, with a rotated hole, at several resolutions,
        // sampled on a grid deliberately incommensurate with the cells.
        let rot = |pts: &[(f64, f64)], t: f64, ox: f64, oy: f64| -> Vec<(f64, f64)> {
            pts.iter()
                .map(|&(x, y)| {
                    (
                        ox + x * t.cos() - y * t.sin(),
                        oy + x * t.sin() + y * t.cos(),
                    )
                })
                .collect()
        };
        let square = [(-10.0, -10.0), (10.0, -10.0), (10.0, 10.0), (-10.0, 10.0)];
        let hole = [(-3.0, -3.0), (3.0, -3.0), (3.0, 3.0), (-3.0, 3.0)];
        let contours = vec![rot(&square, 0.41, 1.7, -2.3), rot(&hole, 0.93, 2.1, -1.9)];
        for cells in [32usize, 64, 97, 128, 512] {
            let grid = PolyGrid::new(&contours, cells);
            let mut bad = 0;
            for i in 0..311 {
                for j in 0..311 {
                    let x = -14.0 + i as f64 * 0.10353;
                    let y = -16.0 + j as f64 * 0.10171;
                    if grid.contains(x, y) != point_in_contours(x, y, &contours) {
                        bad += 1;
                    }
                }
            }
            assert_eq!(bad, 0, "grid disagreed with exact at {cells} cells");
        }
    }

    #[test]
    fn weighted_grid_keeps_overlapping_clear_images_empty() {
        let square = |cx: f64, cy: f64, half: f64| {
            vec![
                (cx - half, cy - half),
                (cx + half, cy - half),
                (cx + half, cy + half),
                (cx - half, cy + half),
            ]
        };
        let contours = vec![
            square(5.0, 5.0, 5.0),
            square(4.0, 5.0, 1.5),
            square(6.0, 5.0, 1.5),
        ];
        let weights = vec![1, -1, -1];
        let grid = PolyGrid::new_weighted(&contours, &weights, 64);
        for i in 0..101 {
            for j in 0..101 {
                let x = i as f64 * 0.1;
                let y = j as f64 * 0.1;
                assert_eq!(
                    grid.contains(x, y),
                    point_in_weighted_contours(x, y, &contours, &weights),
                    "weighted grid mismatch at ({x},{y})"
                );
            }
        }
        assert!(!grid.contains(5.0, 5.0), "the overlap lens stays clear");
    }

    #[test]
    fn a_clear_contour_buried_by_another_clear_is_not_a_copper_edge() {
        let square =
            |x0: f64, y0: f64, x1: f64, y1: f64| vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
        let pour = Shape::MultiPolygon {
            contours: vec![
                square(0.0, 0.0, 10.0, 10.0),
                square(4.0, 4.0, 6.0, 6.0),
                square(5.0, 4.0, 7.0, 6.0),
            ],
            weights: vec![1, -1, -1],
        };
        let isolated = Shape::Capsule(Capsule {
            ax: 5.9,
            ay: 5.0,
            bx: 5.9,
            by: 5.0,
            r: 0.15,
        });
        assert!(
            shape_gap(&isolated, &pour) > 0.0,
            "crossing x=6 only crosses a bookkeeping contour buried in the other void"
        );
        let touching_copper = Shape::Capsule(Capsule {
            ax: 6.95,
            ay: 5.0,
            bx: 6.95,
            by: 5.0,
            r: 0.1,
        });
        assert!(
            shape_gap(&touching_copper, &pour) < 0.0,
            "the exposed x=7 void edge still connects to the surrounding pour"
        );
    }

    #[test]
    fn poly_grid_near_boundary_never_misses_a_crossed_cell() {
        // `near_boundary` returning false has to be a SOUND refusal: it is used to
        // skip an exact poly-distance test, so a false negative drops a real
        // pad-to-pour connection, which is a fabricated open. Brute-force every cell
        // against every edge and require that each cell an edge genuinely crosses is
        // stamped. Swept over resolutions and over a rotated shape WITH a rotated
        // hole, because a single convex ring at one resolution misses the case the
        // supercover walk exists for: a near-diagonal edge advancing both cell
        // coordinates in one step.
        let rot = |pts: &[(f64, f64)], t: f64, ox: f64, oy: f64| -> Vec<(f64, f64)> {
            pts.iter()
                .map(|&(x, y)| {
                    (
                        ox + x * t.cos() - y * t.sin(),
                        oy + x * t.sin() + y * t.cos(),
                    )
                })
                .collect()
        };
        let star: Vec<(f64, f64)> = (0..7)
            .map(|k| {
                let a = 0.37 + k as f64 * std::f64::consts::TAU / 7.0;
                (9.0 * a.cos(), 9.0 * a.sin())
            })
            .collect();
        let square = [(-10.0, -10.0), (10.0, -10.0), (10.0, 10.0), (-10.0, 10.0)];
        let hole = [(-3.5, -3.5), (3.5, -3.5), (3.5, 3.5), (-3.5, 3.5)];
        let shapes: Vec<Vec<Vec<(f64, f64)>>> = vec![
            vec![rot(&star, 0.0, 3.3, -1.1)],
            vec![rot(&square, 0.41, 1.7, -2.3), rot(&hole, 0.93, 2.1, -1.9)],
            vec![rot(&star, 0.79, -1.4, 2.6), rot(&hole, 0.11, -1.2, 2.4)],
        ];
        for contours in &shapes {
            for cells in [16usize, 37, 64, 128] {
                let grid = PolyGrid::new(contours, cells);
                let cell = 1.0 / grid.inv_cell;
                for gy in 0..grid.ny {
                    for gx in 0..grid.nx {
                        let (x0, y0) = (grid.minx + gx as f64 * cell, grid.miny + gy as f64 * cell);
                        let b = [x0, y0, x0 + cell, y0 + cell];
                        let corners = [(b[0], b[1]), (b[2], b[1]), (b[2], b[3]), (b[0], b[3])];
                        let mut crossed = false;
                        for poly in contours {
                            let n = poly.len();
                            let mut j = n - 1;
                            for i in 0..n {
                                for k in 0..4 {
                                    if segments_intersect(
                                        poly[j],
                                        poly[i],
                                        corners[k],
                                        corners[(k + 1) % 4],
                                    ) {
                                        crossed = true;
                                    }
                                }
                                // An edge wholly inside the cell crosses no side.
                                if poly[i].0 >= b[0]
                                    && poly[i].0 <= b[2]
                                    && poly[i].1 >= b[1]
                                    && poly[i].1 <= b[3]
                                {
                                    crossed = true;
                                }
                                j = i;
                            }
                        }
                        if crossed {
                            assert!(
                                grid.near_boundary(b),
                                "cell ({gx},{gy}) at {cells} cells is crossed but not stamped"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn touching_discs() {
        let a = Shape::disc(0.0, 0.0, 0.5);
        let b = Shape::disc(0.9, 0.0, 0.5); // centres 0.9 apart, radii sum 1.0 -> overlap
        assert!(shape_gap(&a, &b) < 0.0);
        let c = Shape::disc(2.0, 0.0, 0.5);
        assert!(shape_gap(&a, &c) > 0.0);
    }

    #[test]
    fn track_into_pad() {
        let pad = Shape::Polygon {
            pts: vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)],
            r: 0.0,
        };
        // Track ending inside the pad.
        let track = Shape::Capsule(Capsule {
            ax: 0.5,
            ay: 0.5,
            bx: 3.0,
            by: 0.5,
            r: 0.1,
        });
        assert!(shape_gap(&pad, &track) < 0.0);
    }
}
