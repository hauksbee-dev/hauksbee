//! Geometry primitives and distance math for copper connectivity tracing.
//!
//! This mirrors the shape model and distance helpers proven in [`crate::drc`]
//! (capsules, polygons, segment/segment and polygon edge distances), but stands
//! alone because the gerber path builds primitives from a different source and
//! needs a couple of extra operations (segment-vs-polygon touch, polygon
//! centroid) the DRC didn't expose. The numerics are identical so a touch here
//! means the same thing a short means there.

/// A "stadium": a segment of finite width. A round flash/via is the degenerate
/// case `a == b`. All coordinates in board millimetres.
#[derive(Debug, Clone)]
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
    /// outer boundary, the rest are holes cut out of it (RS-274X 4.10.4 lets a
    /// single G36/G37 region carry several contours). Containment is even-odd,
    /// a point is copper iff it lies inside an odd number of contours, so the
    /// ring reads as copper and a hole's interior reads as empty. Disjoint
    /// islands of one region are split into separate shapes upstream (one shape
    /// = one conductor for the union-find), so a `MultiPolygon` is always a
    /// single electrically-connected piece. No inflation radius: pours are
    /// drawn at their true outline.
    MultiPolygon { contours: Vec<Vec<(f64, f64)>> },
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
        match self {
            Shape::Capsule(c) => Shape::Capsule(Capsule {
                ax: c.ax + dx,
                ay: c.ay + dy,
                bx: c.bx + dx,
                by: c.by + dy,
                r: c.r,
            }),
            Shape::Polygon { pts, r } => Shape::Polygon {
                pts: pts.iter().map(|(x, y)| (x + dx, y + dy)).collect(),
                r: *r,
            },
            Shape::MultiPolygon { contours } => Shape::MultiPolygon {
                contours: contours
                    .iter()
                    .map(|c| c.iter().map(|(x, y)| (x + dx, y + dy)).collect())
                    .collect(),
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
                let mut b = [
                    f64::INFINITY,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::NEG_INFINITY,
                ];
                for &(x, y) in pts {
                    b[0] = b[0].min(x);
                    b[1] = b[1].min(y);
                    b[2] = b[2].max(x);
                    b[3] = b[3].max(y);
                }
                [b[0] - r, b[1] - r, b[2] + r, b[3] + r]
            }
            Shape::MultiPolygon { contours } => {
                let mut b = [
                    f64::INFINITY,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::NEG_INFINITY,
                ];
                for c in contours {
                    for &(x, y) in c {
                        b[0] = b[0].min(x);
                        b[1] = b[1].min(y);
                        b[2] = b[2].max(x);
                        b[3] = b[3].max(y);
                    }
                }
                b
            }
        }
    }

    /// A representative interior point (centroid-ish), for pad/flash matching.
    pub fn center(&self) -> (f64, f64) {
        match self {
            Shape::Capsule(c) => ((c.ax + c.bx) / 2.0, (c.ay + c.by) / 2.0),
            Shape::Polygon { pts, .. } => {
                let n = pts.len().max(1) as f64;
                (
                    pts.iter().map(|p| p.0).sum::<f64>() / n,
                    pts.iter().map(|p| p.1).sum::<f64>() / n,
                )
            }
            // The outer boundary's vertex average. For a ring this may fall in
            // a hole, but the centre is only a *representative* point for
            // pad/flash matching, and pours never anchor pads.
            Shape::MultiPolygon { contours } => {
                let pts = contours.first().map(|c| c.as_slice()).unwrap_or(&[]);
                let n = pts.len().max(1) as f64;
                (
                    pts.iter().map(|p| p.0).sum::<f64>() / n,
                    pts.iter().map(|p| p.1).sum::<f64>() / n,
                )
            }
        }
    }
}

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

fn orient(p: (f64, f64), q: (f64, f64), r: (f64, f64)) -> f64 {
    (q.0 - p.0) * (r.1 - p.1) - (q.1 - p.1) * (r.0 - p.0)
}

fn on_seg(p: (f64, f64), q: (f64, f64), r: (f64, f64)) -> bool {
    q.0 <= p.0.max(r.0) && q.0 >= p.0.min(r.0) && q.1 <= p.1.max(r.1) && q.1 >= p.1.min(r.1)
}

pub(crate) fn segments_intersect(
    p1: (f64, f64),
    p2: (f64, f64),
    p3: (f64, f64),
    p4: (f64, f64),
) -> bool {
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

fn seg_seg_dist(a1: (f64, f64), a2: (f64, f64), b1: (f64, f64), b2: (f64, f64)) -> f64 {
    if segments_intersect(a1, a2, b1, b2) {
        return 0.0;
    }
    point_seg_dist2(a1.0, a1.1, b1.0, b1.1, b2.0, b2.1)
        .min(point_seg_dist2(a2.0, a2.1, b1.0, b1.1, b2.0, b2.1))
        .min(point_seg_dist2(b1.0, b1.1, a1.0, a1.1, a2.0, a2.1))
        .min(point_seg_dist2(b2.0, b2.1, a1.0, a1.1, a2.0, a2.1))
        .sqrt()
}

/// Even-odd containment over a set of closed contours: inside iff enclosed by
/// an odd number of them. For a pour-with-holes (outer + holes) the ring reads
/// inside and a hole's interior reads outside; this is the containment rule a
/// [`Shape::MultiPolygon`] carries.
pub fn point_in_contours(px: f64, py: f64, contours: &[Vec<(f64, f64)>]) -> bool {
    contours
        .iter()
        .fold(false, |inside, c| inside ^ point_in_polygon(px, py, c))
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

fn poly_poly_edge_dist(a: &[(f64, f64)], b: &[(f64, f64)]) -> f64 {
    let mut best = f64::INFINITY;
    let na = a.len();
    let nb = b.len();
    if na < 2 || nb < 2 {
        for &pa in a {
            for &pb in b {
                best = best.min((pa.0 - pb.0).hypot(pa.1 - pb.1));
            }
        }
        return best;
    }
    let mut ja = na - 1;
    for ia in 0..na {
        let (a1, a2) = (a[ja], a[ia]);
        let mut jb = nb - 1;
        for ib in 0..nb {
            best = best.min(seg_seg_dist(a1, a2, b[jb], b[ib]));
            jb = ib;
        }
        ja = ia;
    }
    best
}

/// Signed copper-edge gap between two shapes. `<= 0` means the copper overlaps
/// (they are the same conductor); positive is the clear gap. Mirrors
/// `drc::shape_gap` but returns only the scalar (callers here don't need the
/// witness point).
pub fn shape_gap(a: &Shape, b: &Shape) -> f64 {
    match (a, b) {
        (Shape::Capsule(ca), Shape::Capsule(cb)) => {
            seg_seg_dist(
                (ca.ax, ca.ay),
                (ca.bx, ca.by),
                (cb.ax, cb.ay),
                (cb.bx, cb.by),
            ) - ca.r
                - cb.r
        }
        (Shape::Capsule(c), Shape::Polygon { pts, r })
        | (Shape::Polygon { pts, r }, Shape::Capsule(c)) => {
            let best = seg_contour_dist((c.ax, c.ay), (c.bx, c.by), pts);
            let contained = point_in_polygon(c.ax, c.ay, pts) || point_in_polygon(c.bx, c.by, pts);
            if contained {
                -(c.r + r).max(0.0) - 1e-6
            } else {
                best - c.r - r
            }
        }
        (Shape::Polygon { pts: pa, r: ra }, Shape::Polygon { pts: pb, r: rb }) => {
            let edge = poly_poly_edge_dist(pa, pb) - ra - rb;
            let contained = pa.first().is_some_and(|&(x, y)| point_in_polygon(x, y, pb))
                || pb.first().is_some_and(|&(x, y)| point_in_polygon(x, y, pa));
            if contained {
                edge.min(0.0) - 1e-6
            } else {
                edge
            }
        }
        // The multi-contour arms mirror the polygon arms above: the copper edge
        // is the nearest of ALL contour boundaries (a hole's rim is copper edge
        // just like the outer rim), and containment is even-odd (a capsule
        // endpoint sitting in a hole is NOT contained; the hole is empty).
        (Shape::Capsule(c), Shape::MultiPolygon { contours })
        | (Shape::MultiPolygon { contours }, Shape::Capsule(c)) => {
            let best = contours
                .iter()
                .map(|pts| seg_contour_dist((c.ax, c.ay), (c.bx, c.by), pts))
                .fold(f64::INFINITY, f64::min);
            let contained =
                point_in_contours(c.ax, c.ay, contours) || point_in_contours(c.bx, c.by, contours);
            if contained {
                -c.r.max(0.0) - 1e-6
            } else {
                best - c.r
            }
        }
        (Shape::Polygon { pts, r }, Shape::MultiPolygon { contours })
        | (Shape::MultiPolygon { contours }, Shape::Polygon { pts, r }) => {
            let edge = contours
                .iter()
                .map(|c| poly_poly_edge_dist(pts, c))
                .fold(f64::INFINITY, f64::min)
                - r;
            // Contained when a polygon vertex sits in the region's copper
            // (even-odd), or any contour rim vertex sits inside the polygon
            // (the polygon laps over that piece of boundary copper).
            let contained = pts
                .first()
                .is_some_and(|&(x, y)| point_in_contours(x, y, contours))
                || contours
                    .iter()
                    .any(|c| c.first().is_some_and(|&(x, y)| point_in_polygon(x, y, pts)));
            if contained {
                edge.min(0.0) - 1e-6
            } else {
                edge
            }
        }
        (Shape::MultiPolygon { contours: ca }, Shape::MultiPolygon { contours: cb }) => {
            let edge = ca
                .iter()
                .flat_map(|a| cb.iter().map(move |b| poly_poly_edge_dist(a, b)))
                .fold(f64::INFINITY, f64::min);
            let contained = ca
                .first()
                .and_then(|c| c.first())
                .is_some_and(|&(x, y)| point_in_contours(x, y, cb))
                || cb
                    .first()
                    .and_then(|c| c.first())
                    .is_some_and(|&(x, y)| point_in_contours(x, y, ca));
            if contained {
                edge.min(0.0) - 1e-6
            } else {
                edge
            }
        }
    }
}

/// Nearest distance from segment AB to a closed contour's edges (0 when they
/// cross). Degenerate contours fall back to point distance.
fn seg_contour_dist(a: (f64, f64), b: (f64, f64), pts: &[(f64, f64)]) -> f64 {
    let mut best = f64::INFINITY;
    let n = pts.len();
    if n >= 2 {
        let mut j = n - 1;
        for i in 0..n {
            best = best.min(seg_seg_dist(a, b, pts[j], pts[i]));
            j = i;
        }
    } else if n == 1 {
        best = point_seg_dist2(pts[0].0, pts[0].1, a.0, a.1, b.0, b.1).sqrt();
    }
    best
}

/// A grid-accelerated point-in-polygon tester for a large fixed polygon (a
/// copper pour). Point-in-polygon is O(vertices); a board-spanning pour has
/// tens of thousands of vertices and is tested against every primitive, so the
/// naive cost is quadratic. This rasterises the polygon's bounding box into a
/// coarse grid once: each cell is tagged fully-inside, fully-outside, or
/// boundary. A query is then an O(1) grid lookup, falling back to the exact
/// even-odd test only for the few points that land in a boundary cell.
pub struct PolyGrid<'a> {
    /// The contours the grid classifies. Even-odd across ALL of them, so one
    /// grid describes a plain polygon (one contour) and a pour with hundreds of
    /// voids cut out of it alike. A negative-drawn plane is exactly the second
    /// shape, and it is board-sized: leaving multi-contour pours to the exact
    /// test made every primitive on the layer pay the pour's whole vertex count.
    contours: &'a [Vec<(f64, f64)>],
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
        // query landing here falls back to the exact even-odd test.
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
        // Classify the non-boundary cells exactly, by *scanline parity* rather
        // than by per-cell even-odd (which would be O(cells x vertices)). For each
        // grid row we intersect the edges crossing that row's centre line with it,
        // sort the crossing x's, and fill the spans between consecutive crossings
        // as inside. Crossings of ALL contours go into one sorted list, which is
        // what makes the parity even-odd over the whole shape: a hole's two
        // crossings close the span its outer opened. This is exact (no flood-fill
        // leakage). Cells already marked boundary keep their exact-test flag.
        //
        // The edges are bucketed by the rows they span first. Walking every edge
        // for every row is O(rows x vertices), and rows scale with the vertex
        // count, so a plane with 6084 annular antipads (each a 64-gon plus a 32-gon
        // rim, ~600k vertices over 2048 rows) spent 3.2 s here. Bucketing makes it
        // O(vertices + rows spanned + cells), and an antipad spans two or three
        // rows.
        if contours.iter().any(|c| c.len() >= 3) {
            let row_of = |y: f64| -> isize { ((y - miny) * inv_cell).floor() as isize };
            let mut rows: Vec<Vec<(f64, f64, f64, f64)>> = vec![Vec::new(); ny];
            for pts in contours {
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
                    for r in r0..=r1 {
                        rows[r].push((a.0, a.1, b.0, b.1));
                    }
                }
            }
            let mut xs: Vec<f64> = Vec::new();
            for (gy, edges) in rows.iter().enumerate() {
                let yc = miny + (gy as f64 + 0.5) / inv_cell;
                xs.clear();
                for &(ax, ay, bx, by) in edges {
                    if (ay > yc) != (by > yc) {
                        xs.push((bx - ax) * (yc - ay) / (by - ay) + ax);
                    }
                }
                xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                // Inside the spans [xs[0],xs[1]], [xs[2],xs[3]], ...
                let mut k = 0;
                while k + 1 < xs.len() {
                    let (x0, x1) = (xs[k], xs[k + 1]);
                    let g0 = (((x0 - minx) * inv_cell).floor() as isize).max(0);
                    let g1 = (((x1 - minx) * inv_cell).ceil() as isize).min(nx as isize - 1);
                    for gx in g0..=g1 {
                        let cxc = minx + (gx as f64 + 0.5) / inv_cell;
                        let idx = gy * nx + gx as usize;
                        if cells[idx] != 2 && cxc >= x0 && cxc <= x1 {
                            cells[idx] = 1;
                        }
                    }
                    k += 2;
                }
            }
        }

        PolyGrid {
            contours,
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
    /// visited. Those missed cells then took their classification from the
    /// scanline parity at their CENTRE, so every query in one of them on the far
    /// side of the boundary got the wrong answer, and `near_boundary` could
    /// answer "no boundary here" over a boundary that was really there. A rotated
    /// square at 512 cells missed 524 crossed cells.
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
            _ => point_in_contours(px, py, self.contours),
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
    fn poly_grid_near_boundary_never_misses_a_crossed_cell() {
        // `near_boundary` returning false has to be a SOUND refusal: it is used to
        // skip an exact poly-distance test, so a false negative drops a real
        // pad-to-pour connection. Brute-force every cell against every edge and
        // require that each cell an edge genuinely crosses is stamped.
        let poly: Vec<(f64, f64)> = (0..7)
            .map(|k| {
                let a = 0.37 + k as f64 * std::f64::consts::TAU / 7.0;
                (3.3 + 9.0 * a.cos(), -1.1 + 9.0 * a.sin())
            })
            .collect();
        let contours = vec![poly.clone()];
        let grid = PolyGrid::new(&contours, 64);
        let cell = 1.0 / grid.inv_cell;
        for gy in 0..grid.ny {
            for gx in 0..grid.nx {
                let (x0, y0) = (grid.minx + gx as f64 * cell, grid.miny + gy as f64 * cell);
                let b = [x0, y0, x0 + cell, y0 + cell];
                // Does any edge cross this cell?
                let corners = [(b[0], b[1]), (b[2], b[1]), (b[2], b[3]), (b[0], b[3])];
                let n = poly.len();
                let mut crossed = false;
                let mut j = n - 1;
                for i in 0..n {
                    for k in 0..4 {
                        if segments_intersect(poly[j], poly[i], corners[k], corners[(k + 1) % 4]) {
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
                if crossed {
                    assert!(
                        grid.near_boundary(b),
                        "cell ({gx},{gy}) is crossed but not stamped"
                    );
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
