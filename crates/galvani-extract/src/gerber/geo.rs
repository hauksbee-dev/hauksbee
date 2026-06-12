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
}

impl Shape {
    pub fn disc(x: f64, y: f64, r: f64) -> Shape {
        Shape::Capsule(Capsule { ax: x, ay: y, bx: x, by: y, r })
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
                let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
                for &(x, y) in pts {
                    b[0] = b[0].min(x);
                    b[1] = b[1].min(y);
                    b[2] = b[2].max(x);
                    b[3] = b[3].max(y);
                }
                [b[0] - r, b[1] - r, b[2] + r, b[3] + r]
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
            seg_seg_dist((ca.ax, ca.ay), (ca.bx, ca.by), (cb.ax, cb.ay), (cb.bx, cb.by))
                - ca.r
                - cb.r
        }
        (Shape::Capsule(c), Shape::Polygon { pts, r })
        | (Shape::Polygon { pts, r }, Shape::Capsule(c)) => {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let pad = Shape::Polygon { pts: vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)], r: 0.0 };
        // Track ending inside the pad.
        let track = Shape::Capsule(Capsule { ax: 0.5, ay: 0.5, bx: 3.0, by: 0.5, r: 0.1 });
        assert!(shape_gap(&pad, &track) < 0.0);
    }
}
