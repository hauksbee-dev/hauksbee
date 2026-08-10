//! RS-274X (extended Gerber) copper-layer reader.
//!
//! We adapt the `gerber_parser` crate (which parses RS-274X into the
//! `gerber-types` model) rather than hand-rolling the grammar; aperture macros,
//! coordinate-format scaling, polarity and the deprecated codes are all its
//! problem. Our job is to *replay* its command stream as a plotter would and
//! emit solid copper primitives (capsules / polygons in board mm) for the
//! connectivity tracer.
//!
//! What we model:
//!   - **Apertures**: circle (disc), rectangle, obround, regular polygon,
//!     and macros (the common primitives: circle, center-line, vector-line,
//!     outline, polygon). A flash stamps the aperture shape at the current
//!     point; a draw with a circular aperture sweeps a capsule of that width.
//!   - **Operations**: D01 interpolate (linear -> capsule; circular -> arc
//!     flattened to capsules), D02 move, D03 flash.
//!   - **Regions** (G36/G37): the accumulated contour becomes a filled polygon
//!     (a pour / copper fill).
//!   - **Polarity** (LPD/LPC): clear (LPC) geometry erases copper, and is CUT
//!     rather than skipped. A film drawn negatively (Altium's default for a
//!     plane) is one board-sized dark region plus hundreds of clear antipads and
//!     thermal gaps, so a reader that discards the clears sees a solid sheet and
//!     merges every net on the board. Each void becomes a hole contour on the
//!     pours it lies inside; see [`apply_clears`] for the exact rule and the
//!     invariant that keeps every imprecision on the over-connected side.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-extract/gerber.md.

use std::collections::HashMap;
use std::io::BufReader;
use std::sync::Arc;

use gerber_types::{
    Aperture, Command, ExtendedCode, FunctionCode, GCode, Operation, Polarity, StepAndRepeat,
};
use gerber_types::{
    ApertureAttribute, ApertureFunction, AttributeDeletionCriterion, Net as GNet, ObjectAttribute,
};
use gerber_types::{Circle, Polygon as GPolygon, Rectangular};
use gerber_types::{CoordinateNumber, CoordinateOffset, Coordinates, InterpolationMode};

use super::geo::{point_in_polygon, Capsule, Shape};
use super::macros::instantiate_macro;

/// Arc flattening resolution (matches drc's ARC_SEGMENTS).
const ARC_SEGMENTS: usize = 16;

/// Radius (mm) of the small disc a macro flash falls back to when its aperture
/// macro cannot be instantiated. A fixed physical size, never scaled by the
/// document's unit factor, so an inch-unit board gets the same 0.25 mm anchor
/// as a millimetre one rather than a 6.35 mm blob.
const MACRO_FALLBACK_DISC_MM: f64 = 0.25;

/// One solid copper region the plotter painted, with the aperture/flash kind so
/// the tracer can tell pads (flashes) from routing (draws) from pours.
#[derive(Debug, Clone)]
pub struct CopperPrim {
    pub shape: Shape,
    pub kind: PrimKind,
    /// The X2 identity the film attached to this primitive, empty on a film
    /// with no X2 attributes (the geometry-only fallback path).
    pub attrs: X2Attrs,
}

impl CopperPrim {
    /// A primitive with no X2 identity, exactly what a stripped film yields.
    pub fn bare(shape: Shape, kind: PrimKind) -> Self {
        CopperPrim {
            shape,
            kind,
            attrs: X2Attrs::default(),
        }
    }
}

/// X2 attributes (`%TA`/`%TO`) in effect when a primitive was painted.
///
/// An X2 film states, per object, which net a piece of copper belongs to
/// (`%TO.N`), which component pad a flash is (`%TO.P,<refdes>,<pin>`), which
/// component owns it (`%TO.C`), and what the flashed aperture *is*
/// (`%TA.AperFunction`: a via pad, an SMD pad, a fiducial, ...). These are the
/// facts the geometry-only reconstruction has to infer, so when the film
/// carries them they are read and used; when it does not, every field is `None`
/// and the geometric fallback is untouched.
/// Strings are shared (`Arc<str>`): one `%TO.N` covers every primitive
/// painted while it is in effect, hundreds of thousands on a large film, so
/// attaching the identity is a pointer clone, not a heap allocation per arc
/// segment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct X2Attrs {
    /// `%TA.AperFunction` of the aperture this primitive was painted with.
    pub function: Option<ApertureFunction>,
    /// `%TO.N` net names. Usually one; a net-tie object legitimately carries
    /// SEVERAL (`%TO.N,A,B*%`: copper that belongs to both nets and ties them
    /// by design), so all are kept, collapsing them into one opaque joined
    /// string would neither match either net nor read as the declared tie.
    /// Only *named* nets are stored: the empty name (an object on no net:
    /// logos, tooling) and `N/C` (deliberately unrouted single-pad nets) both
    /// stay `None`, because unioning either would merge copper the film
    /// explicitly says is unconnected.
    pub net: Option<Arc<[Arc<str>]>>,
    /// `%TO.P`: the (refdes, pin name) this flash is the pad of.
    pub pin: Option<(Arc<str>, Arc<str>)>,
    /// `%TO.C`: the refdes of the component this object belongs to.
    pub component: Option<Arc<str>>,
}

impl X2Attrs {
    pub fn is_empty(&self) -> bool {
        self.function.is_none()
            && self.net.is_none()
            && self.pin.is_none()
            && self.component.is_none()
    }

    /// The film's net names for this primitive (usually one; several on a
    /// net-tie object). Empty when the film named none.
    pub fn net_names(&self) -> &[Arc<str>] {
        self.net.as_deref().unwrap_or(&[])
    }
}

/// Whether an X2 aperture function marks a flash as a VIA pad: copper that
/// stitches layers but is not a component pad. Resolves the via-vs-pad
/// ambiguity the geometric reconstruction cannot (a stitching via inside a
/// footprint window looks exactly like a pad).
pub fn function_is_via(f: &ApertureFunction) -> bool {
    matches!(f, ApertureFunction::ViaPad)
}

/// Whether an X2 aperture function states outright that a flash is NOT a
/// component pad (a fiducial, an antipad, a washer, a thermal relief). Such a
/// flash is real copper, but the footprint window must not claim it as a pin.
/// Absence of any function says nothing, that flash keeps its geometric
/// fallback, because a partially attributed film is not a film asserting its
/// bare flashes are non-pads.
pub fn function_is_nonpad(f: &ApertureFunction) -> bool {
    matches!(
        f,
        ApertureFunction::FiducialPad(_)
            | ApertureFunction::AntiPad
            | ApertureFunction::WasherPad
            | ApertureFunction::ThermalReliefPad
            | ApertureFunction::NonConductor
            | ApertureFunction::CopperBalancing
            | ApertureFunction::Border
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimKind {
    /// D03 flash of a pad-like aperture: a candidate component pad.
    Flash,
    /// D01 draw: routing track / arc.
    Track,
    /// G36/G37 region: a pour / fill.
    Region,
    /// A synthesised plated-drill disc (via / through-hole barrel). Stitches
    /// layers and carries a net like any copper, but is *not* a component pad,
    /// so it is excluded from pad assignment. Real through-hole component pads
    /// still bind via their copper-gerber annular-ring flash.
    Via,
}

/// Parse one RS-274X copper layer's text into solid copper primitives.
pub fn parse_layer(text: &str) -> Result<Vec<CopperPrim>, String> {
    let normalized = normalize_rs274x(text);
    // `parse` returns the partially-built document even on a hard error (the
    // error and the doc-so-far are paired). Per-command parse errors are kept
    // inside `doc.commands` as `Err` and skipped by `doc.commands()`. We want
    // every primitive we *can* recover, so we take the doc in both cases; a
    // truly empty/garbage file simply yields no primitives.
    let doc = match gerber_parser::parse(BufReader::new(normalized.as_bytes())) {
        Ok(doc) => doc,
        Err((doc, _err)) => doc,
    };

    let mut plotter = Plotter::new(&doc);
    for cmd in doc.commands() {
        plotter.run(cmd);
    }
    // A well-formed file closes every `%SR%` with `%SR*%`, but tolerate a block
    // left open at end-of-file (M02 without an explicit close) by flushing it.
    plotter.flush_step_repeat();
    let mut out = plotter.out;
    apply_clears(&mut out, &plotter.clears);
    Ok(out)
}

/// The closed outlines that bound a shape's copper, for use as cut contours.
fn shape_contours(shape: &Shape) -> Vec<Vec<(f64, f64)>> {
    match shape {
        Shape::Capsule(c) => vec![stadium_outline(c)],
        // `r` (a carried corner radius) is deliberately not inflated away here:
        // a void read slightly SMALL leaves copper standing, which under-cuts
        // connectivity in the safe direction; a void read large would erase
        // copper the film actually paints.
        Shape::Polygon { pts, .. } => vec![pts.clone()],
        Shape::MultiPolygon { contours } => contours.clone(),
    }
}

/// Axis-aligned bounds of a contour: `[minx, miny, maxx, maxy]`.
fn contour_bounds(c: &[(f64, f64)]) -> [f64; 4] {
    c.iter().fold(
        [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ],
        |b, &(x, y)| [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)],
    )
}

/// Cut each clear-polarity void out of the pours it was painted over.
///
/// A film drawn "negatively" (Altium's default for a plane, and what several
/// CAM post-processors emit for split planes) paints ONE board-sized dark
/// region and then hundreds of `%LPC*%` voids: every clearance, antipad and
/// thermal gap. Read the darks and ignore the clears and the film is a solid
/// sheet of copper, so the union-find sees the whole board as one conductor and
/// the job reconstructs to a single net. The voids are the connectivity.
///
/// A void becomes extra contours on every pour it sits inside, which is exactly
/// what [`Shape::MultiPolygon`] already means: even-odd containment reads the
/// void's interior as empty and the copper around it as copper. Even-odd does
/// most of the work by itself: a thermal relief's separate arc voids leave the
/// spokes standing, so the pad stays on the pour exactly as fabricated, and an
/// annular void's inner rim leaves its copper island standing.
///
/// This is not a general polygon boolean, so it is imprecise, and the shape of
/// that imprecision is the whole safety argument. **Appending a contour flips
/// the even-odd parity inside it, so wherever a void lands on copper the film
/// really kept, the parity flips from empty to COPPER, not the other way.** A
/// void placed imperfectly therefore leaves a phantom speck of pour, which reads
/// as over-connection: the same direction as the bug this fixes, recoverable,
/// and never a fabricated break in a conductor that is really whole. What that
/// argument depends on is the void's own GEOMETRY never being larger than the
/// void the film cleared, which is why over-approximated clear images are
/// refused outright (see `aperture_image_is_exact`, `declared_line_width`, and
/// the arc refusals in `close_region` and the clear-stroke branch).
///
/// The deliberate limits, all of which leave copper standing:
///
///   - A void is cut only from `Region` primitives. Pours are what negative
///     films void; a clear laid over a track or a pad is not how any exporter
///     draws a break, and scanning every flash for every void is quadratic on
///     boards where neither is small.
///   - A void is cut only from a pour whose OWN contours, as it was drawn,
///     contain every vertex of the void. That rejects a void whose corner falls
///     outside the pour; a void whose vertices all sit in copper while an edge
///     crosses out would need a true clip, and by the parity argument above the
///     part that crosses out reads as copper.
///   - A void is cut only from copper painted BEFORE it (gerber is a painter's
///     model), and from EVERY enclosing pour, not just one: a void inside two
///     overlapping pours has to void both, or the other one fills it back in.
///   - A void already covered whole by an earlier void is skipped: that copper
///     is gone, and re-cutting it would flip it back to copper. Voids that only
///     PARTIALLY overlap do flip their intersection back to copper.
///
/// Finally, a void that RINGS copper (an annular clear flash, or a clear region
/// carrying an island contour) leaves that island electrically separate from the
/// pour around it, so the cut pour is re-split into its connected pieces by
/// [`group_contours_into_pieces`]. Without that the island stayed in the pour's
/// own primitive and the union-find shorted it to the plane, which for a
/// negative-drawn plane is the very merge this function exists to break. A ring
/// formed by SEVERAL separate voids is not a nesting relationship and is not
/// found this way; that island stays on the pour, over-connected.
fn apply_clears(out: &mut Vec<CopperPrim>, clears: &[Clear]) {
    if clears.is_empty() {
        return;
    }
    // Cut candidates, in emission order.
    let regions: Vec<usize> = (0..out.len())
        .filter(|&i| {
            matches!(out[i].kind, PrimKind::Region) && !matches!(out[i].shape, Shape::Capsule(_))
        })
        .collect();
    if regions.is_empty() {
        return;
    }
    // Each pour's own contours and bounds, as drawn. Enclosure is judged against
    // these and never against the voids accumulated since: a void must not be
    // rejected because an earlier, partially-overlapping void clipped one of its
    // vertices. Voiding only removes copper, so the bounds never move either.
    let originals: Vec<Vec<Vec<(f64, f64)>>> = regions
        .iter()
        .map(|&i| match &out[i].shape {
            Shape::Polygon { pts, .. } => vec![pts.clone()],
            Shape::MultiPolygon { contours } => contours.clone(),
            Shape::Capsule(_) => Vec::new(),
        })
        .collect();
    let bounds: Vec<[f64; 4]> = regions.iter().map(|&i| out[i].shape.bounds()).collect();
    // A pour whose own outline is detailed is tested for enclosure once per void,
    // and each test walks every vertex of every void against every vertex of the
    // outline. On an 8000-vertex plane outline with 6000 antipads that alone was
    // 1.1 s. The same scanline grid the connectivity pass uses answers each point
    // in O(1) and is exact, so build one per detailed pour and reuse it. The
    // threshold matches `connect`'s: below it the direct test is cheaper than the
    // build.
    const ENCLOSE_GRID_VERTS: usize = 2000;
    let enclose_grids: Vec<Option<super::geo::PolyGrid>> = originals
        .iter()
        .map(|cs| {
            let verts: usize = cs.iter().map(Vec::len).sum();
            (verts >= ENCLOSE_GRID_VERTS)
                .then(|| super::geo::PolyGrid::new(cs, (verts / 4).clamp(64, 2048)))
        })
        .collect();
    // The void contours already cut from each pour, bucketed spatially so the
    // already-void probe below is not a scan of every earlier void. Without the
    // grid a plane carrying tens of thousands of antipads paid O(voids^2) bound
    // comparisons for an answer that is "no" almost every time.
    let mut applied: Vec<VoidIndex> = bounds.iter().map(|&b| VoidIndex::new(b)).collect();
    // Which pours took a cut, so only those are re-split afterwards.
    let mut cut: Vec<usize> = Vec::new();

    for clear in clears {
        let cb = contour_bounds(&clear.contours.concat());
        for (slot, &i) in regions.iter().enumerate() {
            if i >= clear.painted_before {
                continue; // painted after the void: an island, not cut copper
            }
            let rb = bounds[slot];
            if cb[0] < rb[0] || cb[1] < rb[1] || cb[2] > rb[2] || cb[3] > rb[3] {
                continue;
            }
            let enclosed = match &enclose_grids[slot] {
                Some(g) => clear
                    .contours
                    .iter()
                    .flatten()
                    .all(|&(x, y)| g.contains(x, y)),
                None => contours_enclose(&originals[slot], &clear.contours),
            };
            if !enclosed {
                continue;
            }
            if already_void(&out[i].shape, &applied[slot], &clear.contours, cb) {
                continue;
            }
            let shape = &mut out[i].shape;
            if let Shape::Polygon { pts, .. } = shape {
                let outer = std::mem::take(pts);
                *shape = Shape::MultiPolygon {
                    contours: vec![outer],
                };
            }
            let Shape::MultiPolygon { contours } = shape else {
                continue;
            };
            let mut indices = Vec::with_capacity(clear.contours.len());
            for c in &clear.contours {
                indices.push(contours.len());
                contours.push(c.clone());
            }
            applied[slot].insert(indices, cb);
            cut.push(slot);
        }
    }

    // Free the islands the voids ringed off. See `free_ringed_islands`.
    cut.sort_unstable();
    cut.dedup();
    for slot in cut {
        let i = regions[slot];
        for shape in free_ringed_islands(&mut out[i].shape, &applied[slot]) {
            // An island is still region copper, no longer the same conductor. It
            // does NOT inherit the pour's X2 identity: `%TO.N` on a pour names the
            // pour's net, and copper the film deliberately cut free of the pour is
            // on some other net, so copying the name would assert a connection the
            // geometry just denied. The aperture function survives, being a
            // property of how the copper was drawn rather than of what it is.
            let kind = out[i].kind;
            let function = out[i].attrs.function.clone();
            out.push(CopperPrim {
                shape,
                kind,
                attrs: X2Attrs {
                    function,
                    net: None,
                    pin: None,
                    component: None,
                },
            });
        }
    }
}

/// Move the copper islands a void RINGED off out of the pour's primitive, and
/// return them as their own shapes.
///
/// A void with a hole in it, the classic being an annular clear flash antipad,
/// removes copper only under its ring and leaves the disc at its centre standing.
/// That island is electrically separate from the pour around it, and
/// [`Shape::MultiPolygon`]'s contract is that one shape is one conductor, so
/// leaving the island as a contour of the pour shorted it straight back to the
/// plane: the merge this reader exists to break, in miniature, in the one
/// construct where a negative film most often puts a real pad.
///
/// An island is promoted only when nothing else can have touched it: no other
/// void's bounding box overlaps it. That matters because the general
/// nesting-depth classifier cannot be used here. Its precondition is that
/// contours never cross, which holds inside one region statement and NOT across
/// the voids of a whole film: two overlapping antipads put one void's witness
/// vertex inside the other, which reads as even depth, i.e. an outer boundary,
/// and the void was then promoted to a phantom polygon of COPPER and removed from
/// the pour. On a 12000-antipad plane whose columns overlap, 11890 voids were
/// promoted that way and the board-sized sheet came back. The bounding-box test
/// is conservative in the safe direction: an island it declines to promote stays a
/// contour of the pour, i.e. over-connected.
fn free_ringed_islands(shape: &mut Shape, applied: &VoidIndex) -> Vec<Shape> {
    let Shape::MultiPolygon { contours } = shape else {
        return Vec::new();
    };
    // Which contour indices are a void's HOLE (not its outer boundary).
    let holes: Vec<(usize, usize)> = applied
        .units
        .iter()
        .enumerate()
        .flat_map(|(uid, idxs)| idxs.iter().skip(1).map(move |&h| (uid, h)))
        .collect();
    if holes.is_empty() {
        return Vec::new();
    }
    let mut promoted = vec![false; contours.len()];
    let mut freed: Vec<Shape> = Vec::new();
    for (uid, h) in holes {
        let Some(contour) = contours.get(h) else {
            continue;
        };
        if applied.any_other_overlaps(contour_bounds(contour), uid) {
            continue;
        }
        promoted[h] = true;
        freed.push(Shape::Polygon {
            pts: contour.clone(),
            r: 0.0,
        });
    }
    if freed.is_empty() {
        return Vec::new();
    }
    // Drop the promoted contours from the pour in ONE pass. Removing them one at a
    // time shifts the tail each time, which is quadratic when half the contours go.
    // The pour's parity inside a removed hole becomes outer(1) ^ void-outer(1) =
    // empty, which is right: the island's copper is now the island's own shape,
    // and the pour has a plain hole there. Contour 0 is the pour's own boundary and
    // a void's outer is never promoted, so the pour always keeps at least two
    // contours and stays a `MultiPolygon`.
    let mut keep = promoted.iter();
    contours.retain(|_| !keep.next().copied().unwrap_or(false));
    freed
}

/// Does `outer`'s copper (even-odd over its contours) contain every vertex of
/// every contour in `inner`?
fn contours_enclose(outer: &[Vec<(f64, f64)>], inner: &[Vec<(f64, f64)>]) -> bool {
    if outer.is_empty() {
        return false;
    }
    inner.iter().flatten().all(|&(x, y)| {
        if outer.len() == 1 {
            point_in_polygon(x, y, &outer[0])
        } else {
            super::geo::point_in_contours(x, y, outer)
        }
    })
}

/// A uniform grid over one pour's bounds holding the voids already cut from it.
///
/// The unit of storage is a whole void, not a contour: a void with a hole (an
/// annular clear flash) removes copper only under its RING, so asking whether a
/// candidate sits inside its outer boundary is the wrong question. Indexing
/// contours separately answered yes for a later void at the ring's centre, which
/// the ring had deliberately left standing, and skipped the cut that should have
/// removed it, keeping the pad on the pour.
///
/// A void can only be swallowed by an earlier void whose bounds contain it, and
/// such a void necessarily covers the candidate's own min-corner cell, so one
/// bucket holds every candidate. Buckets hold entries in insertion order, so the
/// answer depends on nothing but the film.
struct VoidIndex {
    origin: (f64, f64),
    cell: (f64, f64),
    /// Every void, in insertion order. This is the whole index until a pour
    /// collects more voids than `GRID_AT`, so the common pour, which takes a
    /// handful, never allocates `RES * RES` buckets it would put one entry in.
    all: Vec<(usize, [f64; 4])>,
    /// `RES * RES` buckets of `(void id, void bounds)`, once the pour has enough
    /// voids for a bucket lookup to beat scanning `all`.
    cells: Vec<Vec<(usize, [f64; 4])>>,
    /// Per void id, the contour indices it occupies in the pour's contour list.
    /// The first is its outer boundary, the rest its holes.
    units: Vec<Vec<usize>>,
}

impl VoidIndex {
    const RES: usize = 48;
    /// Void count at which the grid is worth its allocation.
    const GRID_AT: usize = 128;

    fn new(bounds: [f64; 4]) -> Self {
        let w = (bounds[2] - bounds[0]).max(f64::MIN_POSITIVE);
        let h = (bounds[3] - bounds[1]).max(f64::MIN_POSITIVE);
        VoidIndex {
            origin: (bounds[0], bounds[1]),
            cell: (w / Self::RES as f64, h / Self::RES as f64),
            all: Vec::new(),
            cells: Vec::new(),
            units: Vec::new(),
        }
    }

    /// The inclusive cell range a bounding box covers, clamped to the grid.
    fn span(&self, b: [f64; 4]) -> (usize, usize, usize, usize) {
        let ix =
            |v: f64, o: f64, c: f64| (((v - o) / c).floor().max(0.0) as usize).min(Self::RES - 1);
        (
            ix(b[0], self.origin.0, self.cell.0),
            ix(b[1], self.origin.1, self.cell.1),
            ix(b[2], self.origin.0, self.cell.0),
            ix(b[3], self.origin.1, self.cell.1),
        )
    }

    /// Register a void occupying `indices` in the pour's contour list, bounded
    /// by `b`.
    fn insert(&mut self, indices: Vec<usize>, b: [f64; 4]) {
        let id = self.units.len();
        self.units.push(indices);
        self.all.push((id, b));
        if self.cells.is_empty() {
            if self.all.len() < Self::GRID_AT {
                return;
            }
            // Crossing the threshold: build the grid from everything so far, in
            // insertion order, so the buckets read the same as if it had always
            // existed.
            self.cells = vec![Vec::new(); Self::RES * Self::RES];
            let seen = std::mem::take(&mut self.all);
            for &(prev, pb) in &seen {
                self.stamp(prev, pb);
            }
            self.all = seen;
            return;
        }
        self.stamp(id, b);
    }

    fn stamp(&mut self, id: usize, b: [f64; 4]) {
        let (x0, y0, x1, y1) = self.span(b);
        for cy in y0..=y1 {
            for cx in x0..=x1 {
                self.cells[cy * Self::RES + cx].push((id, b));
            }
        }
    }

    /// Does any void other than `own` have bounds overlapping `b`?
    ///
    /// A void is stamped into every cell its bounds span, so a void overlapping
    /// `b` has an entry in some cell `b` spans; scanning that span is therefore
    /// complete. Without the grid this was a scan of every void ever cut from the
    /// pour, once per island, which is quadratic in the antipad count: a plane
    /// with 64000 annular antipads spent 9.3 s there.
    fn any_other_overlaps(&self, b: [f64; 4], own: usize) -> bool {
        let overlaps =
            |ob: [f64; 4]| !(ob[2] < b[0] || ob[0] > b[2] || ob[3] < b[1] || ob[1] > b[3]);
        if self.cells.is_empty() {
            return self
                .all
                .iter()
                .any(|&(other, ob)| other != own && overlaps(ob));
        }
        let (x0, y0, x1, y1) = self.span(b);
        for cy in y0..=y1 {
            for cx in x0..=x1 {
                if self.cells[cy * Self::RES + cx]
                    .iter()
                    .any(|&(other, ob)| other != own && overlaps(ob))
                {
                    return true;
                }
            }
        }
        false
    }

    /// Every void whose bounds could contain `b`, each listed once.
    fn candidates(&self, b: [f64; 4]) -> &[(usize, [f64; 4])] {
        if self.cells.is_empty() {
            return &self.all;
        }
        let (x0, y0, ..) = self.span(b);
        &self.cells[y0 * Self::RES + x0]
    }
}

/// Is this void already covered whole by ONE void cut earlier from the same
/// pour? Then there is no copper left to remove, and appending it again would
/// flip the overlap back to copper under even-odd.
///
/// "Covered" means inside the earlier void's REMOVED area, even-odd over its own
/// contours, so the copper island an annular void leaves standing does not count
/// as covered.
///
/// Each vertex is nudged a whisker toward the candidate's own centroid before the
/// test. A void emitted TWICE (panelisers do it; so does any re-plot that
/// concatenates films) puts the duplicate's vertices exactly ON the first void's
/// boundary, and `point_in_polygon` is half-open, so the duplicate read as
/// not-covered, was appended a second time, and even-odd flipped the clearance
/// back to COPPER. The nudge is 0.1 um, three orders below any real clearance and
/// well above coordinate noise; being wrong either way is survivable (a missed
/// skip over-connects, an over-eager skip leaves copper that is already gone),
/// but the coincident case is common enough to be worth getting right.
fn already_void(
    shape: &Shape,
    applied: &VoidIndex,
    contours: &[Vec<(f64, f64)>],
    bounds: [f64; 4],
) -> bool {
    let Shape::MultiPolygon { contours: all } = shape else {
        return false;
    };
    const NUDGE_MM: f64 = 1e-4;
    let (cx, cy) = ((bounds[0] + bounds[2]) / 2.0, (bounds[1] + bounds[3]) / 2.0);
    let probes: Vec<(f64, f64)> = contours
        .iter()
        .flatten()
        .map(|&(x, y)| {
            let (dx, dy) = (cx - x, cy - y);
            let len = dx.hypot(dy);
            if len <= NUDGE_MM {
                (x, y)
            } else {
                (x + dx / len * NUDGE_MM, y + dy / len * NUDGE_MM)
            }
        })
        .collect();
    applied.candidates(bounds).iter().any(|&(id, ab)| {
        if bounds[0] < ab[0] - NUDGE_MM
            || bounds[1] < ab[1] - NUDGE_MM
            || bounds[2] > ab[2] + NUDGE_MM
            || bounds[3] > ab[3] + NUDGE_MM
        {
            return false;
        }
        let prev: Vec<&Vec<(f64, f64)>> = applied.units[id]
            .iter()
            .filter_map(|&i| all.get(i))
            .collect();
        !prev.is_empty()
            && probes.iter().all(|&(x, y)| {
                prev.iter()
                    .fold(false, |inside, c| inside ^ point_in_polygon(x, y, c))
            })
    })
}

/// Normalise older / vendor RS-274X dialects into the strict form the
/// `gerber_parser` regexes accept. Real fab gerbers (e.g. Allegro `.art`
/// exports like the uConsole mainboard) differ from the textbook form in two
/// ways the parser rejects outright, which otherwise drops the *entire* layer:
///
///   1. **Multi-statement extended blocks.** A single `%...%` may pack several
///      statements: `%FSAX55Y55*MOIN*%` or
///      `%IR0*IPPOS*OFA0B0*MIA0B0*SFA1B1*%`. The parser expects one statement
///      per `%...%`. We split each inner `...*` into its own `%...*%`.
///   2. **FS without a zero-omission char.** Allegro writes `%FSAX55Y55*`
///      (absolute, 5.5) with no leading `L`/`T`; the parser's regex requires
///      one. Coordinates in these files are zero-padded to the full width, so
///      inserting `L` (omit-leading, a no-op on full-width numbers) is exact.
///
/// Everything else is passed through untouched, so well-formed KiCad/JLCPCB
/// gerbers are unaffected (their `%...%` blocks are already single-statement
/// and their FS already carries the zero char).
fn normalize_rs274x(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let trimmed = line.trim();
        // Only extended-code lines (start and end with %) can need splitting.
        if trimmed.starts_with('%') && trimmed.ends_with('%') && trimmed.len() > 2 {
            let inner = &trimmed[1..trimmed.len() - 1];
            // An aperture macro (`%AM<name>*<primitive>*...*%`) is a SINGLE
            // statement whose `*`-separated parts are its primitives, not
            // independent extended codes. Splitting it would yield an empty
            // `%AM<name>*%` plus orphan primitive blocks, silently collapsing
            // the pad to a fallback disc. Pass macro blocks through untouched.
            if inner.starts_with("AM") {
                out.push_str(line);
                out.push('\n');
                continue;
            }
            // Count statements (each ends with '*'). One is the normal case.
            let stmts: Vec<&str> = inner.split('*').filter(|s| !s.is_empty()).collect();
            if stmts.len() > 1 {
                for s in stmts {
                    out.push('%');
                    out.push_str(&patch_fs(s));
                    out.push_str("*%\n");
                }
                continue;
            } else if stmts.len() == 1 {
                out.push('%');
                out.push_str(&patch_fs(stmts[0]));
                out.push_str("*%\n");
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Insert the leading-zero-omission char into a bare `FSA…`/`FSI…` statement.
fn patch_fs(stmt: &str) -> String {
    if let Some(rest) = stmt.strip_prefix("FS") {
        // Already `FSL…`/`FST…`? leave it.
        if rest.starts_with('L') || rest.starts_with('T') {
            return stmt.to_string();
        }
        // `FSA…` / `FSI…` (absolute/incremental with no zero char): add `L`.
        if rest.starts_with('A') || rest.starts_with('I') {
            return format!("FSL{rest}");
        }
    }
    stmt.to_string()
}

fn num(c: &CoordinateNumber) -> f64 {
    // gerber-types stores nanounits; Into<f64> yields the value in the
    // document's unit (mm or inch). We normalise inch->mm at the call site.
    (*c).into()
}

struct Plotter<'a> {
    doc: &'a gerber_parser::GerberDoc,
    /// inch->mm factor (1.0 if already mm).
    to_mm: f64,
    x: f64,
    y: f64,
    aperture: Option<i32>,
    interp: InterpolationMode,
    /// Inside a G36 region: the accumulated contours. RS-274X 4.10.4 lets one
    /// region carry several closed contours (each begun by a D02 move), an
    /// outer boundary plus holes cut out of it, or several disjoint islands,
    /// so contours are kept SEPARATE; the last entry is the contour currently
    /// being drawn. Flattening them into one ring bridged the pieces with
    /// phantom edges (false shorts across islands, holes filled back in).
    region: Option<Vec<Vec<(f64, f64)>>>,
    /// Polarity in effect when the current region OPENED. A region is a single
    /// primitive, so its whole fill takes the polarity at G36 time; captured
    /// here so a clear (LPC) region is dropped like a clear draw/flash, instead
    /// of being materialized as phantom additive copper.
    region_dark: bool,
    /// Did the current region's boundary carry a circular (G02/G03) segment?
    /// An arc is flattened into inscribed chords, which cut across the copper
    /// side of any CONCAVE stretch of a void's boundary, so a clear region with
    /// one cannot be trusted to erase only what the film erased.
    region_has_arc: bool,
    /// Current load polarity. Clear (LPC) geometry is banked as a void and cut
    /// from the copper beneath it (see [`apply_clears`]).
    dark: bool,
    /// Is the object transform (`%LS%` scaling, `%LR%` rotation, `%LM%`
    /// mirroring) the identity?
    ///
    /// The plotter does not apply them, which only ever misplaces or misshapes
    /// ADDITIVE copper. A clear image is another matter: a `2x1` rectangle under
    /// `%LS0.5*%` really clears `1x0.5`, so subtracting the unscaled rectangle
    /// erases copper the film kept, which is the one thing this reader must not
    /// do. While a transform is loaded, clears are refused outright. The three
    /// are tracked separately because they load independently: one flag would let
    /// a later `%LR0*%` clear the memory of an active `%LS0.5*%`.
    scale_identity: bool,
    rotation_identity: bool,
    mirror_identity: bool,
    /// How many `%AB` aperture-block definitions are open.
    ///
    /// This plotter does not implement blocks: `gerber-types` hands the body over
    /// as ordinary commands, so it is plotted where it is DEFINED rather than
    /// where the block is flashed. For dark objects that has always been a
    /// harmless over-paint. A clear object inside a body is not harmless: it
    /// would be cut from whatever pour happens to lie under the definition
    /// coordinates, erasing copper the film never clears. No clear is banked
    /// while a body is open, and the polarity in force is saved and restored
    /// across it, because a body that ends under `%LPC*%` would otherwise leave
    /// every later region and flash on the film being read as a void.
    ab_depth: usize,
    /// Polarity to restore when the outermost `%AB` body closes.
    ab_saved_dark: bool,
    /// Arc interpolation quadrant mode. `false` = multi-quadrant (G75, the
    /// modern default and what KiCad emits): I/J are signed vectors to the
    /// centre. `true` = single-quadrant (G74, legacy CAM dialects): I/J are
    /// unsigned magnitudes and the true centre is one of the four ±I,±J
    /// candidates, chosen per RS-274X §4.5.
    single_quadrant: bool,
    /// An open step-and-repeat (`%SR%`) block: the primitives emitted while it
    /// is open form the base cell, replicated across the grid when it closes.
    sr: Option<SrBlock>,
    /// The X2 aperture-attribute dictionary's `.AperFunction` entry: a `%TA`
    /// applies to every aperture DEFINED while it is in effect (until `%TD`),
    /// so it is captured here and bound to the aperture code at its `%AD`.
    cur_aper_function: Option<ApertureFunction>,
    /// Aperture code -> the `.AperFunction` in effect at its definition.
    aper_functions: HashMap<i32, ApertureFunction>,
    /// The X2 object-attribute dictionary (`%TO.N` / `%TO.P` / `%TO.C`): what
    /// the current objects being painted ARE, until changed or `%TD`-deleted.
    obj_net: Option<Arc<[Arc<str>]>>,
    obj_pin: Option<(Arc<str>, Arc<str>)>,
    obj_component: Option<Arc<str>>,
    out: Vec<CopperPrim>,
    /// Clear-polarity (`%LPC*%`) geometry, in the order it was painted. These
    /// are the VOIDS a negative-drawn pour cuts out of itself; see
    /// [`Clear`] and [`apply_clears`].
    clears: Vec<Clear>,
}

/// One image painted under clear polarity: copper the film removes.
///
/// Altium (and every other exporter that draws a pour "negatively") writes a
/// plane as ONE dark region covering the whole board followed by `%LPC*%` and a
/// few hundred clear regions, one per clearance, antipad and thermal gap. The
/// clears are not decoration: they are the only thing that makes the plane
/// anything other than a solid slab. Dropping them left a board-sized sheet of
/// copper on every signal layer, which unioned every net on the board into one.
#[derive(Clone)]
struct Clear {
    /// EVERY contour of the void, kept together as one unit. An annular clear
    /// flash (a standard aperture with a hole, or a macro with an exposure-off
    /// primitive) removes copper only under its ring; its inner rim must be
    /// applied alongside its outer boundary or the void swallows the copper
    /// island the ring leaves standing.
    contours: Vec<Vec<(f64, f64)>>,
    /// How many primitives had already been painted when this void was cut.
    /// Gerber is a painter's model: a void only removes copper that was
    /// already on the film, never copper drawn after it (that is an island
    /// deliberately re-added inside the void).
    painted_before: usize,
}

impl Clear {
    fn translated(&self, dx: f64, dy: f64, painted_before: usize) -> Clear {
        Clear {
            contours: self
                .contours
                .iter()
                .map(|c| c.iter().map(|&(x, y)| (x + dx, y + dy)).collect())
                .collect(),
            painted_before,
        }
    }
}

/// State for an open `%SRXnYnInJn*%` step-and-repeat block: the grid to tile the
/// base cell over and the index into `out` where the base cell's primitives
/// begin. Distances are stored in board millimetres.
struct SrBlock {
    start: usize,
    /// Index into `Plotter::clears` where the base cell's voids begin, so a
    /// replicated negative-drawn cell carries its voids and not only its copper.
    clears_start: usize,
    repeat_x: u32,
    repeat_y: u32,
    step_x_mm: f64,
    step_y_mm: f64,
}

impl<'a> Plotter<'a> {
    fn new(doc: &'a gerber_parser::GerberDoc) -> Self {
        let to_mm = match doc.units {
            Some(gerber_types::Unit::Inches) => 25.4,
            _ => 1.0,
        };
        Plotter {
            doc,
            to_mm,
            x: 0.0,
            y: 0.0,
            aperture: None,
            interp: InterpolationMode::Linear,
            region: None,
            region_dark: true,
            region_has_arc: false,
            dark: true,
            scale_identity: true,
            rotation_identity: true,
            mirror_identity: true,
            ab_depth: 0,
            ab_saved_dark: true,
            single_quadrant: false,
            sr: None,
            cur_aper_function: None,
            aper_functions: HashMap::new(),
            obj_net: None,
            obj_pin: None,
            obj_component: None,
            out: Vec::new(),
            clears: Vec::new(),
        }
    }

    /// Record a shape painted under clear polarity as one void.
    fn add_clear(&mut self, shape: &Shape) {
        self.add_clear_contours(shape_contours(shape));
    }

    /// Is the object transform the identity? Only then may a clear image, which
    /// this plotter draws untransformed, be trusted to subtract.
    fn identity_transform(&self) -> bool {
        self.scale_identity && self.rotation_identity && self.mirror_identity
    }

    /// May clear geometry drawn right now be trusted to subtract? Not under an
    /// unapplied object transform, and not inside an `%AB` body (see `ab_depth`).
    fn may_bank_clears(&self) -> bool {
        self.identity_transform() && self.ab_depth == 0
    }

    /// Record contours painted under clear polarity as voids, ONE PER CONNECTED
    /// PIECE, each piece's outer boundary first and its holes after.
    ///
    /// A region statement may carry several contours in any order, and RS-274X
    /// 4.10.4 allows them to be an outer plus its holes OR several disjoint
    /// islands. Banking them verbatim as one void and then treating everything
    /// after the first as a hole is a guess about draw order, and when it is wrong
    /// the void is undone and re-emitted as copper: a second disjoint void in one
    /// statement was cancelled outright, and an annular void drawn hole-first had
    /// its cleared RING promoted to copper. Splitting into pieces here makes
    /// "index 0 is the outer, the rest are its holes" true by construction.
    /// Nesting-depth classification is valid at this point, and only at this
    /// point: the contours of one region statement do not cross, which is exactly
    /// what it needs and exactly what the voids of a whole film violate.
    fn add_clear_contours(&mut self, contours: Vec<Vec<(f64, f64)>>) {
        if !self.may_bank_clears() {
            return;
        }
        let contours: Vec<Vec<(f64, f64)>> =
            contours.into_iter().filter(|c| c.len() >= 3).collect();
        if contours.is_empty() {
            return;
        }
        let painted_before = self.out.len();
        for piece in group_contours(contours) {
            self.clears.push(Clear {
                contours: piece,
                painted_before,
            });
        }
    }

    /// The X2 identity for a primitive painted right now: the object
    /// attributes in effect, plus the painting aperture's `.AperFunction`.
    /// Empty (all `None`) on a film that carries no X2 attributes.
    fn cur_attrs(&self) -> X2Attrs {
        X2Attrs {
            function: self
                .aperture
                .and_then(|code| self.aper_functions.get(&code))
                .cloned(),
            net: self.obj_net.clone(),
            pin: self.obj_pin.clone(),
            component: self.obj_component.clone(),
        }
    }

    fn coord(&self, c: &Coordinates) -> (f64, f64) {
        let nx =
            c.x.as_ref()
                .map(num)
                .map(|v| v * self.to_mm)
                .unwrap_or(self.x);
        let ny =
            c.y.as_ref()
                .map(num)
                .map(|v| v * self.to_mm)
                .unwrap_or(self.y);
        (nx, ny)
    }

    fn run(&mut self, cmd: &Command) {
        match cmd {
            Command::FunctionCode(FunctionCode::GCode(g)) => match g {
                GCode::InterpolationMode(m) => self.interp = *m,
                GCode::RegionMode(true) => {
                    // A region takes the polarity in effect when it opens.
                    self.region_dark = self.dark;
                    self.region_has_arc = false;
                    // One (empty) current contour; further contours are opened
                    // by D02 moves while the region is open.
                    self.region = Some(vec![Vec::new()]);
                }
                GCode::RegionMode(false) => self.close_region(),
                GCode::QuadrantMode(m) => {
                    self.single_quadrant = matches!(m, gerber_types::QuadrantMode::Single);
                }
                _ => {}
            },
            Command::FunctionCode(FunctionCode::DCode(d)) => match d {
                gerber_types::DCode::SelectAperture(code) => self.aperture = Some(*code),
                gerber_types::DCode::Operation(op) => self.operation(op),
            },
            Command::ExtendedCode(ExtendedCode::LoadPolarity(p)) => {
                self.dark = matches!(p, Polarity::Dark);
            }
            // The object transforms. Not applied (see `identity_transform`), but
            // tracked, because a transform we ignore must not be allowed to
            // subtract the untransformed image.
            Command::ExtendedCode(ExtendedCode::LoadScaling(s)) => {
                self.scale_identity = (s.scale - 1.0).abs() < f64::EPSILON;
            }
            Command::ExtendedCode(ExtendedCode::LoadRotation(r)) => {
                self.rotation_identity = r.rotation.abs() < f64::EPSILON;
            }
            Command::ExtendedCode(ExtendedCode::LoadMirroring(m)) => {
                self.mirror_identity = matches!(m, gerber_types::Mirroring::None);
            }
            Command::ExtendedCode(ExtendedCode::ApertureBlock(ab)) => match ab {
                gerber_types::ApertureBlock::Open { .. } => {
                    if self.ab_depth == 0 {
                        self.ab_saved_dark = self.dark;
                    }
                    self.ab_depth += 1;
                }
                gerber_types::ApertureBlock::Close => {
                    self.ab_depth = self.ab_depth.saturating_sub(1);
                    if self.ab_depth == 0 {
                        self.dark = self.ab_saved_dark;
                    }
                }
            },
            Command::ExtendedCode(ExtendedCode::StepAndRepeat(sr)) => match sr {
                StepAndRepeat::Open {
                    repeat_x,
                    repeat_y,
                    distance_x,
                    distance_y,
                } => {
                    // A new SR implicitly closes any open one. The primitives
                    // drawn until the matching `%SR*%` are the base cell.
                    self.flush_step_repeat();
                    self.sr = Some(SrBlock {
                        start: self.out.len(),
                        clears_start: self.clears.len(),
                        repeat_x: (*repeat_x).max(1),
                        repeat_y: (*repeat_y).max(1),
                        step_x_mm: distance_x * self.to_mm,
                        step_y_mm: distance_y * self.to_mm,
                    });
                }
                StepAndRepeat::Close => self.flush_step_repeat(),
            },
            // ── X2 attributes ────────────────────────────────────────────────
            // A `%TA.AperFunction` enters the aperture-attribute dictionary and
            // is attached to every aperture DEFINED while it is in effect.
            Command::ExtendedCode(ExtendedCode::ApertureAttribute(
                ApertureAttribute::ApertureFunction(f),
            )) => {
                self.cur_aper_function = Some(f.clone());
            }
            Command::ExtendedCode(ExtendedCode::ApertureDefinition(def)) => {
                if let Some(f) = &self.cur_aper_function {
                    self.aper_functions.insert(def.code, f.clone());
                }
            }
            // `%TO.N` / `%TO.P` / `%TO.C` set the identity of the objects
            // painted from here on, until changed or deleted by `%TD`.
            Command::ExtendedCode(ExtendedCode::ObjectAttribute(o)) => match o {
                ObjectAttribute::Net(n) => {
                    // Only a NAMED net identifies a conductor. `%TO.N,*%` (no
                    // net: logos, tooling) and `N/C` (each such pad is its own
                    // single-pad net) must clear the state, not carry a name,
                    // or the copper painted under them would be unioned.
                    self.obj_net = match n {
                        GNet::Connected(names) if !names.is_empty() => Some(
                            names
                                .iter()
                                .map(|s| Arc::from(s.as_str()))
                                .collect::<Arc<[Arc<str>]>>(),
                        ),
                        _ => None,
                    };
                }
                ObjectAttribute::Pin(p) => {
                    self.obj_pin = Some((Arc::from(p.refdes.as_str()), Arc::from(p.name.as_str())));
                }
                ObjectAttribute::Component(refdes) => {
                    self.obj_component = Some(Arc::from(refdes.as_str()));
                }
                _ => {}
            },
            Command::ExtendedCode(ExtendedCode::DeleteAttribute(crit)) => match crit {
                AttributeDeletionCriterion::AllApertureAndObjectAttributes => {
                    self.cur_aper_function = None;
                    self.obj_net = None;
                    self.obj_pin = None;
                    self.obj_component = None;
                }
                AttributeDeletionCriterion::SingleObjectAttribute(name) => {
                    match name.trim_start_matches("TO") {
                        ".N" => self.obj_net = None,
                        ".P" => self.obj_pin = None,
                        ".C" => self.obj_component = None,
                        _ => {}
                    }
                }
                AttributeDeletionCriterion::SingleApertureAttribute(name) => {
                    if name.trim_start_matches("TA") == ".AperFunction" {
                        self.cur_aper_function = None;
                    }
                }
            },
            _ => {}
        }
    }

    /// Close the open step-and-repeat block (if any) by tiling the base cell,
    /// the primitives appended since the block opened, across the `repeat_x` ×
    /// `repeat_y` grid at the I/J step. The base copy at cell (0,0) is already in
    /// `out`; only the other cells are cloned, each translated by its grid
    /// offset. Without this the repeated copies of a panelized/arrayed layer
    /// were silently dropped, losing every pad/track/pour but the first.
    fn flush_step_repeat(&mut self) {
        let Some(block) = self.sr.take() else {
            return;
        };
        if block.start > self.out.len() {
            return;
        }
        let base: Vec<CopperPrim> = self.out[block.start..].to_vec();
        // The base cell's voids, if it was drawn negatively. Each replica needs
        // its own translated copies, and their painter-order index has to point
        // at the REPLICA's copper: a void carrying the base cell's index would
        // be read as painted before the copy it belongs to and cut nothing.
        let base_clears: Vec<Clear> = self.clears[block.clears_start.min(self.clears.len())..]
            .iter()
            .filter(|c| c.painted_before >= block.start)
            .cloned()
            .collect();
        // A cell may be voids ALONE: a pour painted before the block, then an
        // arrayed set of clear antipads over it. Returning on empty base copper
        // replicated none of them and left every repeat but the first solid.
        if base.is_empty() && base_clears.is_empty() {
            return;
        }
        // Copy order is the spec's, not the loop's convenience: "Blocks are
        // copied first in the positive Y direction and then in the positive X
        // direction" (Gerber Layer Format Specification 2021.02, 4.12). Order is
        // the painter's order, so it decides the image wherever repeats overlap
        // and the cell mixes dark and clear objects: an X-fastest loop let a
        // void from one copy erase copper that the spec's later copy restores,
        // which is the one direction this reader must never fabricate.
        for ix in 0..block.repeat_x {
            for iy in 0..block.repeat_y {
                if ix == 0 && iy == 0 {
                    continue; // the base cell is already emitted in place
                }
                let dx = f64::from(ix) * block.step_x_mm;
                let dy = f64::from(iy) * block.step_y_mm;
                let replica_start = self.out.len();
                for prim in &base {
                    self.out.push(CopperPrim {
                        shape: prim.shape.translated(dx, dy),
                        kind: prim.kind,
                        attrs: prim.attrs.clone(),
                    });
                }
                for c in &base_clears {
                    let at = replica_start + (c.painted_before - block.start);
                    self.clears.push(c.translated(dx, dy, at));
                }
            }
        }
    }

    fn operation(&mut self, op: &Operation) {
        match op {
            Operation::Move(coord) => {
                if let Some(c) = coord {
                    let (nx, ny) = self.coord(c);
                    self.x = nx;
                    self.y = ny;
                }
                // Inside a region, a D02 move also TERMINATES the current
                // contour and begins the next one (RS-274X 4.10.4: every
                // contour of a region starts with a D02). The new contour is
                // seeded lazily by its first D01 (which reads the moved-to
                // point as its start), so a redundant D02 before any draw
                // does not leave an empty contour behind.
                if let Some(contours) = self.region.as_mut() {
                    if contours.last().is_some_and(|c| !c.is_empty()) {
                        contours.push(Vec::new());
                    }
                }
            }
            Operation::Interpolate(coord, offset) => {
                let (sx, sy) = (self.x, self.y);
                let (ex, ey) = coord.as_ref().map(|c| self.coord(c)).unwrap_or((sx, sy));
                if self.region.is_some() {
                    // Region contour: collect the boundary vertices. A segment
                    // drawn under circular interpolation (G02/G03) contributes
                    // its flattened arc; the same centre/sweep geometry a
                    // stroked arc sweeps, NOT just its chord: chord-collapsing
                    // turned a round pour drawn as two semicircles into a
                    // zero-area polygon, vanishing its copper entirely.
                    let seg: Vec<(f64, f64)> = match self.interp {
                        InterpolationMode::Linear => vec![(ex, ey)],
                        InterpolationMode::ClockwiseCircular
                        | InterpolationMode::CounterclockwiseCircular => {
                            self.region_has_arc = true;
                            let (ox, oy) = self.offset_mm(offset);
                            let ccw =
                                matches!(self.interp, InterpolationMode::CounterclockwiseCircular);
                            self.arc_samples(sx, sy, ex, ey, ox, oy, ccw)
                        }
                    };
                    let contour = self
                        .region
                        .as_mut()
                        .and_then(|c| c.last_mut())
                        .expect("an open region always has a current contour");
                    if contour.is_empty() {
                        contour.push((sx, sy));
                    }
                    contour.extend(seg);
                } else if self.dark {
                    // A routed segment of the current aperture's width.
                    let width = self.aperture_line_width();
                    match self.interp {
                        InterpolationMode::Linear => {
                            self.push_capsule(sx, sy, ex, ey, width / 2.0);
                        }
                        InterpolationMode::ClockwiseCircular
                        | InterpolationMode::CounterclockwiseCircular => {
                            let (ox, oy) = self.offset_mm(offset);
                            let ccw =
                                matches!(self.interp, InterpolationMode::CounterclockwiseCircular);
                            let mut prev = (sx, sy);
                            for p in self.arc_samples(sx, sy, ex, ey, ox, oy, ccw) {
                                self.push_capsule(prev.0, prev.1, p.0, p.1, width / 2.0);
                                prev = p;
                            }
                        }
                    }
                } else if let (InterpolationMode::Linear, Some(width), true) = (
                    self.interp,
                    self.declared_line_width(),
                    self.may_bank_clears(),
                ) {
                    // Under clear polarity the same stroke SCRAPES copper out of
                    // whatever is beneath it (how some exporters draw a plane's
                    // splits), so it is banked as a void rather than painted.
                    //
                    // Only a straight stroke of a DECLARED width is banked. A
                    // circular D01 flattens into inscribed chords, and an
                    // aperture with no width takes the 0.1 mm hairline: both are
                    // conservative when they ADD copper, and both erase copper
                    // the film never cleared when they SUBTRACT it, which
                    // fabricates an open in a conductor that is really whole.
                    // Refusing leaves the copper standing, the same direction
                    // every other limit in `apply_clears` fails in.
                    self.add_clear(&Shape::Capsule(Capsule {
                        ax: sx,
                        ay: sy,
                        bx: ex,
                        by: ey,
                        r: width / 2.0,
                    }));
                }
                self.x = ex;
                self.y = ey;
            }
            Operation::Flash(coord) => {
                if let Some(c) = coord {
                    let (nx, ny) = self.coord(c);
                    self.x = nx;
                    self.y = ny;
                }
                if self.dark {
                    self.flash();
                } else if let Some(shape) = self.flash_shape_for_clear() {
                    // A clear flash is an antipad: the classic way a negative
                    // plane film states "no copper here". Banked as a void, but
                    // only when the aperture's image is reproduced faithfully
                    // enough to subtract with; an over-approximated image may add
                    // copper, never erase it.
                    self.add_clear(&shape);
                }
            }
        }
    }

    /// The effective draw width: a draw is only well-defined with a circular
    /// aperture; for non-circular we use the smaller dimension as a width
    /// (KiCad always routes with round apertures, so this is the common path).
    fn aperture_line_width(&self) -> f64 {
        match self.aperture.and_then(|a| self.doc.apertures.get(&a)) {
            Some(Aperture::Circle(Circle { diameter, .. })) => diameter * self.to_mm,
            Some(Aperture::Rectangle(Rectangular { x, y, .. }))
            | Some(Aperture::Obround(Rectangular { x, y, .. })) => x.min(*y) * self.to_mm,
            // Unknown aperture (polygon/macro/undefined): a fixed 0.1 mm hairline,
            // enough to connect endpoints. This is a physical millimetre size and
            // must NOT be scaled by `to_mm`, on an inch-unit file (`%MOIN%`,
            // to_mm=25.4) `0.1 * to_mm` = 2.54 mm, a fat stroke that union-merges
            // adjacent copper into a false short (the same unit-scaling hazard the
            // MACRO_FALLBACK_DISC_MM constant is documented to avoid).
            _ => 0.1,
        }
    }

    /// The draw width the FILE states, or `None` when the aperture gives none
    /// and [`Self::aperture_line_width`] would substitute its hairline. A
    /// substituted width may only ever add copper; see the clear-draw branch.
    /// The circle is exact, and the rect/obround min-dimension under-covers the
    /// true swept image, so both under-remove: the safe direction.
    fn declared_line_width(&self) -> Option<f64> {
        // A HOLED aperture declares no usable stroke width. The hole is bare board
        // (RS-274X 4.4.6), so the swept clear is an annular band with a strip of
        // copper standing along the centreline, and subtracting the solid capsule
        // would erase it. Stroking with a holed aperture is spec-illegal, so this
        // only guards a malformed file, but the direction it guards is the one that
        // fabricates an open.
        let holed = |h: &Option<f64>| h.is_some_and(|h| h > 0.0);
        match self.aperture.and_then(|a| self.doc.apertures.get(&a)) {
            Some(Aperture::Circle(Circle {
                diameter,
                hole_diameter,
            })) if !holed(hole_diameter) => Some(diameter * self.to_mm),
            Some(Aperture::Rectangle(Rectangular {
                x,
                y,
                hole_diameter,
            }))
            | Some(Aperture::Obround(Rectangular {
                x,
                y,
                hole_diameter,
            })) if !holed(hole_diameter) => Some(x.min(*y) * self.to_mm),
            _ => None,
        }
    }

    /// Is the current aperture's image reproduced EXACTLY enough to erase copper
    /// with?
    ///
    /// A macro is not. [`instantiate_macro`] returns the convex HULL of the
    /// macro's primitives and drops voids it cannot represent, and an
    /// unevaluable macro falls back to a fixed disc. Every one of those is a
    /// deliberate over-approximation, correct while it only ADDS copper (a
    /// flash that claims slightly too much never invents a gap) and destructive
    /// the moment it subtracts: the hull between two disjoint macro elements,
    /// or a whole fallback disc, would erase pour copper the film never cleared
    /// and split a conductor that is really whole. Standard apertures are
    /// polygonized INSCRIBED, so they under-remove, which is the safe
    /// direction.
    ///
    /// An aperture the document does not define is not exact either, and neither
    /// is an aperture BLOCK (`%AB`): this plotter does not implement blocks, so a
    /// block code resolves to nothing here and must not be trusted to subtract.
    fn aperture_image_is_exact(&self) -> bool {
        !matches!(
            self.aperture.and_then(|a| self.doc.apertures.get(&a)),
            None | Some(Aperture::Macro(..))
        )
    }

    /// The image to subtract for a clear flash, or `None` when this aperture's
    /// image is not reproduced faithfully enough to erase copper with.
    fn flash_shape_for_clear(&self) -> Option<Shape> {
        if !self.may_bank_clears() || !self.aperture_image_is_exact() {
            return None;
        }
        let (shape, rim_escaped) = self.flash_image()?;
        // A hole almost as wide as its aperture pushes the circumscribed rim
        // outside the outer boundary, where even-odd would read the excursion as
        // a void over untouched pour copper.
        (!rim_escaped).then_some(shape)
    }

    /// The image the current aperture paints at the current point, independent
    /// of polarity. Dark flashes push it as copper; clear flashes bank it as a
    /// void, so both need the same geometry.
    fn flash_shape(&self) -> Option<Shape> {
        self.flash_image().map(|(shape, _)| shape)
    }

    /// The aperture image, plus whether a circumscribed hole rim escaped the
    /// outer boundary (only ever true under clear polarity; see `with_hole`).
    fn flash_image(&self) -> Option<(Shape, bool)> {
        let code = self.aperture?;
        let ap = self.doc.apertures.get(&code)?;
        let (cx, cy) = (self.x, self.y);
        let s = self.to_mm;
        // A standard aperture's optional hole diameter: the hole is BARE BOARD
        // (RS-274X 4.4.6: the hole is not part of the aperture image), so a
        // flash that carries one must not paint copper there. Discarding it
        // read the hole as solid, and foreign copper passing through a large
        // hole was unioned onto the pad's net. A holed flash materializes as
        // an outer contour plus a hole contour (even-odd containment); a flash
        // with no hole keeps its exact old shape.
        let hole_mm = match ap {
            Aperture::Circle(Circle { hole_diameter, .. }) => *hole_diameter,
            Aperture::Rectangle(Rectangular { hole_diameter, .. })
            | Aperture::Obround(Rectangular { hole_diameter, .. }) => *hole_diameter,
            Aperture::Polygon(GPolygon { hole_diameter, .. }) => *hole_diameter,
            Aperture::Macro(..) => None,
        }
        .map(|h| h * s)
        .filter(|h| *h > 0.0);
        // The hole rim as a 32-gon, sized so the polygon errs toward COPPER for
        // the polarity that is painting.
        //
        // Dark: inscribed, under-cutting the hole by <0.5% of its radius, so the
        // annular ring reads slightly wide rather than the hole reading wide.
        //
        // Clear: the reverse, because the hole of a clear flash is the copper
        // ISLAND the void leaves standing (the aperture hole is bare board, so
        // the film clears nothing there). An inscribed rim would shrink that
        // island and eat copper the film never cleared, which fabricates a break
        // in a conductor that is really whole. Circumscribing puts the rim
        // outside the true hole, so the island reads slightly wide.
        let rim_r = hole_mm.unwrap_or(0.0) / 2.0
            * if self.dark {
                1.0
            } else {
                1.0 / (std::f64::consts::PI / 32.0).cos()
            };
        // The two contours are read even-odd, not as a guaranteed
        // `outer minus rim`, so a circumscribed rim that pokes OUTSIDE the outer
        // boundary would flip parity out there and, on a clear flash, erase pour
        // copper the aperture never covered. A hole almost as wide as its
        // aperture does exactly that (a 2 mm square with a 1.999 mm hole puts the
        // rim's vertices at radius 1.0043). When it does, the clear image is not
        // exact and the caller must refuse it; see `flash_shape_for_clear`.
        let rim_escapes = |outer: &[(f64, f64)], rim: &[(f64, f64)]| -> bool {
            rim.iter().any(|&(x, y)| !point_in_polygon(x, y, outer))
        };
        let mut rim_escaped = false;
        let mut with_hole = |outer: Vec<(f64, f64)>| -> Shape {
            let rim: Vec<(f64, f64)> = (0..32)
                .map(|k| {
                    let a = k as f64 * std::f64::consts::TAU / 32.0;
                    (cx + rim_r * a.cos(), cy + rim_r * a.sin())
                })
                .collect();
            rim_escaped |= rim_escapes(&outer, &rim);
            Shape::MultiPolygon {
                contours: vec![outer, rim],
            }
        };
        let shape = match ap {
            Aperture::Circle(Circle { diameter, .. }) => match hole_mm {
                None => Shape::disc(cx, cy, diameter * s / 2.0),
                Some(_) => {
                    // The outer rim as a 64-gon (radius error < 0.13%).
                    let r = diameter * s / 2.0;
                    let outer: Vec<(f64, f64)> = (0..64)
                        .map(|k| {
                            let a = k as f64 * std::f64::consts::TAU / 64.0;
                            (cx + r * a.cos(), cy + r * a.sin())
                        })
                        .collect();
                    with_hole(outer)
                }
            },
            Aperture::Rectangle(Rectangular { x, y, .. }) => {
                let rect = rect_polygon(cx, cy, x * s, y * s, 0.0);
                match (hole_mm, rect) {
                    (Some(_), Shape::Polygon { pts, .. }) => with_hole(pts),
                    (_, rect) => rect,
                }
            }
            Aperture::Obround(Rectangular { x, y, .. }) => {
                // Obround = stadium; model as a capsule along the long axis.
                let (w, h) = (x * s, y * s);
                let capsule = if w >= h {
                    let r = h / 2.0;
                    Capsule {
                        ax: cx - (w - h) / 2.0,
                        ay: cy,
                        bx: cx + (w - h) / 2.0,
                        by: cy,
                        r,
                    }
                } else {
                    let r = w / 2.0;
                    Capsule {
                        ax: cx,
                        ay: cy - (h - w) / 2.0,
                        bx: cx,
                        by: cy + (h - w) / 2.0,
                        r,
                    }
                };
                match hole_mm {
                    None => Shape::Capsule(capsule),
                    // A holed obround polygonizes its stadium boundary so the
                    // hole contour can be carried alongside it.
                    Some(_) => with_hole(stadium_outline(&capsule)),
                }
            }
            Aperture::Polygon(GPolygon {
                diameter,
                vertices,
                rotation,
                ..
            }) => {
                let poly = regular_polygon(
                    cx,
                    cy,
                    diameter * s / 2.0,
                    *vertices,
                    rotation.unwrap_or(0.0),
                );
                match (hole_mm, poly) {
                    (Some(_), Shape::Polygon { pts, .. }) => with_hole(pts),
                    (_, poly) => poly,
                }
            }
            Aperture::Macro(name, args) => {
                match self.doc.commands().iter().find_map(|c| match c {
                    Command::ExtendedCode(ExtendedCode::ApertureMacro(m)) if &m.name == name => {
                        Some(m)
                    }
                    _ => None,
                }) {
                    Some(m) => {
                        let ms = instantiate_macro(m, args.as_deref().unwrap_or(&[]), cx, cy, s);
                        if ms.hull.len() >= 3 {
                            if ms.holes.is_empty() {
                                Shape::Polygon {
                                    pts: ms.hull,
                                    r: 0.0,
                                }
                            } else {
                                // Exposure-off primitives punched real voids
                                // out of the pad: carry them, so foreign
                                // copper routed through a void is NOT read as
                                // touching this pad (a false short).
                                let mut contours = vec![ms.hull];
                                contours.extend(ms.holes);
                                Shape::MultiPolygon { contours }
                            }
                        } else {
                            // Couldn't evaluate (variables/expressions we don't
                            // support): fall back to a small disc so the flash
                            // still anchors a pad rather than vanishing. The
                            // radius is a fixed physical size (mm); `cx`/`cy` are
                            // already mm, so it must NOT be scaled by `to_mm`,
                            // doing so bloated the anchor to 6.35 mm (0.25 inch)
                            // on an inch-unit file, big enough to merge adjacent
                            // copper into one net.
                            Shape::disc(cx, cy, MACRO_FALLBACK_DISC_MM)
                        }
                    }
                    None => Shape::disc(cx, cy, MACRO_FALLBACK_DISC_MM),
                }
            }
        };
        Some((shape, rim_escaped))
    }

    fn flash(&mut self) {
        let Some(shape) = self.flash_shape() else {
            return;
        };
        let attrs = self.cur_attrs();
        // The film can state outright that this flash is a VIA pad
        // (`%TA.AperFunction,ViaPad`). A via stitches copper like any flash but
        // is not a component pad, so it takes `PrimKind::Via` (excluded from
        // pad assignment) instead of being left for the footprint window to
        // mistake for one. Absent the attribute, nothing changes.
        let kind = match &attrs.function {
            Some(f) if function_is_via(f) => PrimKind::Via,
            _ => PrimKind::Flash,
        };
        self.out.push(CopperPrim { shape, kind, attrs });
    }

    fn push_capsule(&mut self, ax: f64, ay: f64, bx: f64, by: f64, r: f64) {
        self.out.push(CopperPrim {
            shape: Shape::Capsule(Capsule { ax, ay, bx, by, r }),
            kind: PrimKind::Track,
            attrs: self.cur_attrs(),
        });
    }

    /// Single-quadrant (G74) centre selection: try the four ±I,±J offsets and
    /// return the one whose start/end radii agree best while keeping the arc
    /// sweep within 90 degrees. `None` if no candidate has a positive radius.
    #[allow(clippy::too_many_arguments)]
    fn single_quadrant_center(
        &self,
        sx: f64,
        sy: f64,
        ex: f64,
        ey: f64,
        ox: f64,
        oy: f64,
        ccw: bool,
    ) -> Option<(f64, f64)> {
        use std::f64::consts::{FRAC_PI_2, TAU};
        let mut best: Option<(f64, f64)> = None;
        let mut best_score = f64::INFINITY;
        for &(sox, soy) in &[(ox, oy), (-ox, oy), (ox, -oy), (-ox, -oy)] {
            let (cx, cy) = (sx + sox, sy + soy);
            let rs = ((sx - cx).powi(2) + (sy - cy).powi(2)).sqrt();
            if rs <= f64::EPSILON {
                continue;
            }
            let re = ((ex - cx).powi(2) + (ey - cy).powi(2)).sqrt();
            let a0 = (sy - cy).atan2(sx - cx);
            let mut a1 = (ey - cy).atan2(ex - cx);
            if ccw {
                while a1 <= a0 {
                    a1 += TAU;
                }
            } else {
                while a1 >= a0 {
                    a1 -= TAU;
                }
            }
            let sweep = (a1 - a0).abs();
            // Consistent radius, and penalise a sweep past the 90-degree
            // single-quadrant limit so the correct centre wins.
            let score = (rs - re).abs() + if sweep > FRAC_PI_2 + 1e-6 { 1e3 } else { 0.0 };
            if score < best_score {
                best_score = score;
                best = Some((cx, cy));
            }
        }
        best
    }

    /// The I/J arc offset of a circular D01, scaled to board millimetres
    /// ((0, 0) when the offset, or either axis, is absent).
    fn offset_mm(&self, offset: &Option<CoordinateOffset>) -> (f64, f64) {
        offset
            .as_ref()
            .map(|o| {
                (
                    o.x.as_ref().map(num).unwrap_or(0.0) * self.to_mm,
                    o.y.as_ref().map(num).unwrap_or(0.0) * self.to_mm,
                )
            })
            .unwrap_or((0.0, 0.0))
    }

    /// Resolve a circular D01's geometry: centre, radius, start angle and
    /// signed sweep, honouring the quadrant mode. This is the ONE place the
    /// centre/sweep math lives; the stroked-arc path and the
    /// region-contour path both flatten from these numbers, so a pour boundary
    /// arc lands on byte-identical points to the same arc drawn as a track.
    /// `None` when the radius degenerates (centre on the start point): the
    /// "arc" is then just its chord.
    #[allow(clippy::too_many_arguments)]
    fn arc_params(
        &self,
        sx: f64,
        sy: f64,
        ex: f64,
        ey: f64,
        ox: f64,
        oy: f64,
        ccw: bool,
    ) -> Option<(f64, f64, f64, f64, f64)> {
        let (cx, cy) = if self.single_quadrant {
            // G74: I/J are unsigned magnitudes, so the true centre is one of the
            // four ±I,±J offsets from the start. Per RS-274X §4.5, pick the
            // candidate whose start- and end-radius agree and whose sweep (in
            // the requested direction) is <= 90 degrees; the single-quadrant
            // guarantee. Fall back to the multi-quadrant formula if none fits.
            self.single_quadrant_center(sx, sy, ex, ey, ox, oy, ccw)
                .unwrap_or((sx + ox, sy + oy))
        } else {
            (sx + ox, sy + oy)
        };
        let radius = ((sx - cx) * (sx - cx) + (sy - cy) * (sy - cy)).sqrt();
        if radius <= f64::EPSILON {
            return None;
        }
        let a0 = (sy - cy).atan2(sx - cx);
        let mut a1 = (ey - cy).atan2(ex - cx);
        // Choose sweep direction.
        use std::f64::consts::TAU;
        if ccw {
            while a1 <= a0 {
                a1 += TAU;
            }
        } else {
            while a1 >= a0 {
                a1 -= TAU;
            }
        }
        Some((cx, cy, radius, a0, a1 - a0))
    }

    /// The flattened arc: `ARC_SEGMENTS` points sampled from just past the
    /// start through the endpoint (the start itself is NOT included, so the
    /// samples chain onto a path that already holds it). A degenerate radius
    /// yields just the endpoint; the chord.
    #[allow(clippy::too_many_arguments)]
    fn arc_samples(
        &self,
        sx: f64,
        sy: f64,
        ex: f64,
        ey: f64,
        ox: f64,
        oy: f64,
        ccw: bool,
    ) -> Vec<(f64, f64)> {
        let Some((cx, cy, radius, a0, sweep)) = self.arc_params(sx, sy, ex, ey, ox, oy, ccw) else {
            return vec![(ex, ey)];
        };
        (1..=ARC_SEGMENTS)
            .map(|i| {
                let a = a0 + sweep * (i as f64 / ARC_SEGMENTS as f64);
                (cx + radius * a.cos(), cy + radius * a.sin())
            })
            .collect()
    }

    fn close_region(&mut self) {
        let Some(contours) = self.region.take() else {
            return;
        };
        // A clear-polarity region is a cut-out, not copper. It is never painted
        // as copper (that would union nets across a gap) but it is not thrown
        // away either: it is banked as a void and subtracted from the copper
        // underneath it once the whole film is plotted. A negative-drawn pour
        // IS its voids; without them the film is a solid slab.
        //
        // A region counts as clear only when the polarity is clear at BOTH ends.
        // Which end decides is a reading of when the region object is created, and
        // the reader must not need to be right about that: requiring both is
        // copper whenever either end says copper, so an ambiguous film is painted
        // rather than subtracted. A region that flips polarity mid-way is still
        // painted, never dropped, because dropping it under-connects exactly as a
        // fabricated open does.
        //
        // A clear region whose boundary carries an arc is refused: the flattening
        // is inscribed, which stays inside a CONVEX stretch of the boundary but
        // cuts across the copper on a CONCAVE one (the inner rim of an annular or
        // thermal-sector void), so the void it describes can be larger than the
        // void the film cleared. Same refusal, and the same reason, as the clear
        // circular STROKE. A refused void is DROPPED, not painted: the film clears
        // that area, and painting it would put phantom copper on the film, which
        // could bridge nets outside the pour rather than merely leaving the pour's
        // own copper standing.
        if !self.region_dark && !self.dark {
            if !self.region_has_arc {
                self.add_clear_contours(contours);
            }
            return;
        }
        // Contours that enclose no area (a stray D02 with no draws, a lone
        // segment) are dropped, as the flat model always did.
        let contours: Vec<Vec<(f64, f64)>> =
            contours.into_iter().filter(|c| c.len() >= 3).collect();
        if contours.is_empty() {
            return;
        }
        // The overwhelmingly common case (KiCad emits one contour per G36
        // block): a plain polygon, exactly as before.
        if contours.len() == 1 {
            let pts = contours.into_iter().next().unwrap();
            self.out.push(CopperPrim {
                shape: Shape::Polygon { pts, r: 0.0 },
                kind: PrimKind::Region,
                attrs: self.cur_attrs(),
            });
            return;
        }
        // Several contours in one region (RS-274X 4.10.4): group them into the
        // physically-connected pieces of copper they fill, because the
        // connectivity tracer unions per PRIMITIVE, two disjoint islands
        // sharing one primitive would falsely short their nets.
        for shape in group_contours_into_pieces(contours) {
            self.out.push(CopperPrim {
                shape,
                kind: PrimKind::Region,
                attrs: self.cur_attrs(),
            });
        }
    }
}

/// An index entry for [`group_contours_into_pieces`]: a contour's bounding box.
struct BoundsLeaf {
    bounds: [f64; 4],
    idx: usize,
}

impl rstar::RTreeObject for BoundsLeaf {
    type Envelope = rstar::AABB<[f64; 2]>;
    fn envelope(&self) -> Self::Envelope {
        rstar::AABB::from_corners(
            [self.bounds[0], self.bounds[1]],
            [self.bounds[2], self.bounds[3]],
        )
    }
}

/// Split a set of closed contours into the physically-connected pieces of copper
/// they fill, one [`Shape`] per piece.
///
/// The connectivity tracer unions per PRIMITIVE, and [`Shape::MultiPolygon`]
/// promises that one shape is one conductor, so two disjoint islands sharing a
/// shape would falsely short their nets. Nesting depth (how many other contours
/// enclose a contour) classifies each one: an even-depth contour is an outer
/// boundary and its own piece of copper; an odd-depth contour is a hole cut out
/// of its immediate (depth-1) parent. Legal contours never cross, so any single
/// vertex is a valid containment witness. An island nested inside a hole (depth
/// 2) is an outer again: its copper is electrically separate from the
/// surrounding ring's, which is exactly what an annular void leaves behind on a
/// negative-drawn plane.
///
/// The enclosure search is R-tree pruned. Comparing every ordered pair of
/// contours, even by bounding box alone, is quadratic in the contour count, and a
/// cut plane's contour count IS its antipad count: on a 6000-antipad plane that
/// pair scan was the single most expensive thing in the reader.
fn group_contours_into_pieces(contours: Vec<Vec<(f64, f64)>>) -> Vec<Shape> {
    group_contours(contours)
        .into_iter()
        .map(|mut bucket| {
            if bucket.len() == 1 {
                // A hole-less island: a plain polygon, like a lone contour.
                Shape::Polygon {
                    pts: bucket.remove(0),
                    r: 0.0,
                }
            } else {
                // Outer + holes: even-odd containment reads the ring as copper
                // and the hole interiors as empty.
                Shape::MultiPolygon { contours: bucket }
            }
        })
        .collect()
}

/// Group closed contours into one bucket per connected piece of copper, each
/// bucket's OUTER boundary first and its holes after. The classification is the
/// nesting depth described on [`group_contours_into_pieces`], and its precondition
/// is the same: the contours must not cross one another.
fn group_contours(contours: Vec<Vec<(f64, f64)>>) -> Vec<Vec<Vec<(f64, f64)>>> {
    if contours.len() <= 1 {
        return contours.into_iter().map(|c| vec![c]).collect();
    }
    let n = contours.len();
    let bounds: Vec<[f64; 4]> = contours.iter().map(|c| contour_bounds(c)).collect();
    let tree = rstar::RTree::bulk_load(
        (0..n)
            .map(|i| BoundsLeaf {
                bounds: bounds[i],
                idx: i,
            })
            .collect(),
    );
    // encloses[i] = the contours strictly containing contour i. Only a contour
    // whose bounding box covers i's witness point can be one, which the tree
    // answers directly; the exact test then runs on that handful.
    let encloses: Vec<Vec<usize>> = (0..n)
        .map(|i| {
            let (px, py) = contours[i][0];
            let mut hits: Vec<usize> = tree
                .locate_in_envelope_intersecting(rstar::AABB::from_point([px, py]))
                .map(|l| l.idx)
                .filter(|&j| j != i && point_in_polygon(px, py, &contours[j]))
                .collect();
            // The tree yields in its own traversal order; the depth counts below
            // and the `max_by_key` tie-break must not depend on it.
            hits.sort_unstable();
            hits
        })
        .collect();
    // Each contour's emit group: an outer owns itself; a hole belongs to its
    // immediate parent; the DEEPEST contour enclosing it (depth-1, which is
    // even, i.e. an outer).
    let group_of: Vec<usize> = (0..n)
        .map(|i| {
            if encloses[i].len() % 2 == 0 {
                i
            } else {
                encloses[i]
                    .iter()
                    .copied()
                    .max_by_key(|&j| encloses[j].len())
                    .expect("an odd-depth contour has at least one encloser")
            }
        })
        .collect();
    let mut buckets: Vec<Vec<Vec<(f64, f64)>>> = (0..n).map(|_| Vec::new()).collect();
    for (i, c) in contours.into_iter().enumerate() {
        if group_of[i] == i {
            buckets[i].insert(0, c); // the outer boundary leads its group
        } else {
            buckets[group_of[i]].push(c);
        }
    }
    buckets.into_iter().filter(|b| !b.is_empty()).collect()
}

/// The boundary of a stadium (capsule) as a polygon: each end cap sampled as
/// a 16-segment half-circle, joined by the straight flanks. Used only when an
/// obround aperture carries a hole and must become a contour pair.
fn stadium_outline(c: &Capsule) -> Vec<(f64, f64)> {
    let (dx, dy) = (c.bx - c.ax, c.by - c.ay);
    let len = dx.hypot(dy);
    let (ux, uy) = if len > f64::EPSILON {
        (dx / len, dy / len)
    } else {
        (1.0, 0.0)
    };
    let base_a = uy.atan2(ux) + std::f64::consts::FRAC_PI_2;
    let mut pts = Vec::with_capacity(34);
    // Cap around B (from +90° to -90° relative to the axis), then around A.
    for k in 0..=16 {
        let a = base_a - k as f64 * std::f64::consts::PI / 16.0;
        pts.push((c.bx + c.r * a.cos(), c.by + c.r * a.sin()));
    }
    for k in 0..=16 {
        let a = base_a + std::f64::consts::PI - k as f64 * std::f64::consts::PI / 16.0;
        pts.push((c.ax + c.r * a.cos(), c.ay + c.r * a.sin()));
    }
    pts
}

/// Axis-aligned rectangle polygon centred at (cx, cy).
fn rect_polygon(cx: f64, cy: f64, w: f64, h: f64, _r: f64) -> Shape {
    let (hw, hh) = (w / 2.0, h / 2.0);
    Shape::Polygon {
        pts: vec![
            (cx - hw, cy - hh),
            (cx + hw, cy - hh),
            (cx + hw, cy + hh),
            (cx - hw, cy + hh),
        ],
        r: 0.0,
    }
}

/// Regular n-gon inscribed in a circle of radius `r`, rotated `rot_deg`.
fn regular_polygon(cx: f64, cy: f64, r: f64, vertices: u8, rot_deg: f64) -> Shape {
    let n = vertices.max(3) as usize;
    let rot = rot_deg.to_radians();
    let mut pts = Vec::with_capacity(n);
    for i in 0..n {
        let a = rot + (i as f64) * std::f64::consts::TAU / n as f64;
        pts.push((cx + r * a.cos(), cy + r * a.sin()));
    }
    Shape::Polygon { pts, r: 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &str = "\
%FSLAX46Y46*%
%MOMM*%
%ADD10C,0.500000*%
%ADD11R,1.000000X2.000000*%
G01*
D10*
X0Y0D02*
X5000000Y0D01*
D11*
X10000000Y0D03*
M02*
";

    #[test]
    fn parses_track_and_pad() {
        let prims = parse_layer(SIMPLE).unwrap();
        // One track (the draw) and one flash (the rect pad).
        assert!(prims.iter().any(|p| p.kind == PrimKind::Track));
        let flash = prims.iter().find(|p| p.kind == PrimKind::Flash).unwrap();
        // Rect flash -> polygon centred at (10, 0).
        if let Shape::Polygon { pts, .. } = &flash.shape {
            let cx = pts.iter().map(|p| p.0).sum::<f64>() / pts.len() as f64;
            assert!((cx - 10.0).abs() < 1e-6, "pad centre at {cx}");
        } else {
            panic!("rect flash should be a polygon");
        }
    }

    #[test]
    fn normalizes_allegro_fs_and_combined_blocks() {
        // Allegro dialect: FS without zero char, FS+MO combined in one block.
        let g = "\
%FSAX55Y55*MOIN*%
%IR0*IPPOS*OFA0.00000B0.00000*%
%ADD10C,0.040000*%
D10*
X0000050000Y0000050000D03*
M02*
";
        let prims = parse_layer(g).unwrap();
        // The flash must survive: FS was patched and the combined block split,
        // so the format spec is set and the coordinate op is honoured.
        let f = prims.iter().find(|p| p.kind == PrimKind::Flash).unwrap();
        if let Shape::Capsule(c) = &f.shape {
            // 5.5 inch format: 0000050000 = 0.5 inch = 12.7 mm.
            assert!((c.ax - 12.7).abs() < 1e-3, "x was {}", c.ax);
        } else {
            panic!("round flash should be a disc");
        }
    }

    #[test]
    fn round_flash_is_disc() {
        let g = "\
%FSLAX46Y46*%
%MOMM*%
%ADD10C,1.000000*%
D10*
X2000000Y3000000D03*
M02*
";
        let prims = parse_layer(g).unwrap();
        let f = prims.iter().find(|p| p.kind == PrimKind::Flash).unwrap();
        if let Shape::Capsule(c) = &f.shape {
            assert!((c.ax - 2.0).abs() < 1e-6 && (c.ay - 3.0).abs() < 1e-6);
            assert!((c.r - 0.5).abs() < 1e-6);
        } else {
            panic!("round flash should be a disc/capsule");
        }
    }

    // A G36/G37 square region; body differs only by the leading %LPC*%.
    fn region_layer(polarity: &str) -> &'static str {
        // (returns one of the two consts below)
        if polarity == "clear" {
            "\
%FSLAX46Y46*%
%MOMM*%
%LPC*%
G36*
X0Y0D02*
X5000000Y0D01*
X5000000Y5000000D01*
X0Y5000000D01*
X0Y0D01*
G37*
M02*
"
        } else {
            "\
%FSLAX46Y46*%
%MOMM*%
G36*
X0Y0D02*
X5000000Y0D01*
X5000000Y5000000D01*
X0Y5000000D01*
X0Y0D01*
G37*
M02*
"
        }
    }

    #[test]
    fn clear_polarity_region_is_never_additive_copper() {
        // R6: a region drawn under LPC (clear) is a cut-out, not copper; it must
        // never materialize as an additive Region primitive (which would union
        // nets across the gap). With no copper beneath it there is nothing for
        // it to cut, so it produces no primitive at all. A dark region does.
        let dark = parse_layer(region_layer("dark")).unwrap();
        assert!(
            dark.iter().any(|p| p.kind == PrimKind::Region),
            "a DARK region must materialize as copper"
        );
        let clear = parse_layer(region_layer("clear")).unwrap();
        assert!(
            !clear.iter().any(|p| p.kind == PrimKind::Region),
            "a CLEAR (LPC) region must never be additive copper"
        );
    }

    /// The region pieces of a negative film, each as its contour set. A pour that
    /// a void rings into two conductors comes back as two pieces.
    fn pieces_of(gerber: &str) -> Vec<Vec<Vec<(f64, f64)>>> {
        parse_layer(gerber)
            .unwrap()
            .into_iter()
            .filter(|p| p.kind == PrimKind::Region)
            .map(|p| match p.shape {
                Shape::MultiPolygon { contours } => contours,
                Shape::Polygon { pts, .. } => vec![pts],
                Shape::Capsule(_) => panic!("a region is never a capsule"),
            })
            .collect()
    }

    /// Which piece, if any, has copper at this point.
    fn piece_at(pieces: &[Vec<Vec<(f64, f64)>>], x: f64, y: f64) -> Option<usize> {
        pieces
            .iter()
            .position(|c| super::super::geo::point_in_contours(x, y, c))
    }

    /// The single pour of a negative film: its contour count, and whether a point
    /// on it is copper. Panics if the voids split it into more than one conductor.
    fn pour_of(gerber: &str) -> (usize, Box<dyn Fn(f64, f64) -> bool>) {
        let mut pieces = pieces_of(gerber);
        assert_eq!(pieces.len(), 1, "one conductor expected");
        let contours = pieces.pop().unwrap();
        (
            contours.len(),
            Box::new(move |x, y| super::super::geo::point_in_contours(x, y, &contours)),
        )
    }

    /// A 20x20 mm dark pour, then whatever `body` paints under `%LPC*%`.
    fn negative_pour(apertures: &str, body: &str) -> String {
        format!(
            "\
%FSLAX46Y46*%
%MOMM*%
{apertures}\
G36*
X0Y0D02*
X20000000Y0D01*
X20000000Y20000000D01*
X0Y20000000D01*
X0Y0D01*
G37*
%LPC*%
{body}\
M02*
"
        )
    }

    #[test]
    fn an_annular_clear_flash_frees_its_copper_island() {
        // A clear flash of a 6 mm circle with a 2 mm hole is a RING of removed
        // copper: the 2 mm disc at its centre is bare board on the film's image,
        // so that copper stays, and the ring around it has cut it free of the
        // plane. Two conductors. Two things went wrong here in turn. Splitting the
        // aperture's outer boundary and its rim into independent voids cut the
        // outer first and then dropped the rim as already-void, so the ring became
        // a solid disc and the island was erased. Keeping the island but leaving
        // it in the pour's own primitive shorted it straight back to the plane,
        // because the union-find unions per primitive. This is the antipad of a
        // through-hole pad on a plane: erasing it loses a pin, shorting it invents
        // one.
        let pieces = pieces_of(&negative_pour(
            "%ADD10C,6.000000X2.000000*%\n",
            "D10*\nX10000000Y10000000D03*\n",
        ));
        assert_eq!(pieces.len(), 2, "the ring cuts the island free of the pour");
        let island = piece_at(&pieces, 10.0, 10.0).expect("the island is copper");
        let pour = piece_at(&pieces, 14.5, 10.0).expect("the pour is copper");
        assert_ne!(island, pour, "the island is not the pour's conductor");
        assert_eq!(
            piece_at(&pieces, 12.0, 10.0),
            None,
            "the ring itself is void"
        );
        // And the island reaches the hole's true rim. A 32-gon INSCRIBED in the
        // 2 mm hole would leave it 0.48% short, eating copper the film never
        // cleared; for a void the rim is circumscribed instead.
        assert_eq!(piece_at(&pieces, 11.0, 10.0), Some(island));
        // The island carries none of the pour's X2 identity: a `%TO.N` on the
        // pour names the pour's net, and this copper is on some other one.
        let prims = parse_layer(&negative_pour(
            "%ADD10C,6.000000X2.000000*%\n",
            "%TO.N,GND*%\nD10*\nX10000000Y10000000D03*\n",
        ))
        .unwrap();
        let named = prims
            .iter()
            .filter(|p| p.kind == PrimKind::Region && p.attrs.net.is_some())
            .count();
        assert_eq!(named, 0, "no piece may claim a net the geometry denied");
    }

    #[test]
    fn a_void_already_covered_by_an_earlier_void_is_not_re_cut() {
        // 4x4 mm cleared, then 2x2 mm cleared inside it. The copper is already
        // gone; appending the inner void as another contour would flip it back
        // to copper under even-odd containment, planting a phantom speck of
        // pour in the middle of a clearance.
        let (n_contours, is_copper) = pour_of(&negative_pour(
            "",
            "\
G36*
X8000000Y8000000D02*
X12000000Y8000000D01*
X12000000Y12000000D01*
X8000000Y12000000D01*
X8000000Y8000000D01*
G37*
G36*
X9000000Y9000000D02*
X11000000Y9000000D01*
X11000000Y11000000D01*
X9000000Y11000000D01*
X9000000Y9000000D01*
G37*
",
        ));
        assert_eq!(n_contours, 2, "the nested void adds nothing to remove");
        assert!(!is_copper(10.0, 10.0), "the clearance stays clear");
    }

    #[test]
    fn an_over_approximated_clear_image_erases_nothing() {
        // The whole class: geometry we only approximate is safe while it ADDS
        // copper (a flash that claims a little too much never invents a gap) and
        // destructive the moment it subtracts, because it erases copper the film
        // never cleared and splits a conductor that is really whole. Three
        // approximations reach the clear path, and none may cut:
        //
        //   - a macro flash (the hull of its primitives, or a fallback disc when
        //     it cannot be evaluated),
        //   - a stroke whose aperture declares no width (the 0.1 mm hairline),
        //   - a circular stroke (inscribed chords, which bite into the inside of
        //     the curve by up to ~1.9% of the radius).
        //
        // Each must leave the pour whole: one contour, still solid.
        let refused = [
            // An unevaluable macro: `flash_shape` would hand back a 0.25 mm disc.
            (
                "%AMWEIRD*\n1,1,$1,0,0*\n%\n%ADD10WEIRD*%\n",
                "D10*\nX10000000Y10000000D03*\n",
            ),
            // A polygon aperture stroke: `aperture_line_width` substitutes its
            // hairline because a draw has no defined width for one.
            (
                "%ADD10P,3.000000X6*%\n",
                "D10*\nX4000000Y10000000D02*\nX16000000Y10000000D01*\n",
            ),
            // A circular stroke under G03: a real width, flattened boundary.
            (
                "%ADD10C,1.000000*%\n",
                "G75*\nG03*\nD10*\nX14000000Y10000000D02*\nX6000000Y10000000I-4000000J0D01*\n",
            ),
            // A clear REGION with an arc on its boundary. Inscribed chords stay
            // inside a convex stretch but cut across the copper on a concave one.
            (
                "",
                "\
G75*
G36*
X14000000Y10000000D02*
G03*
X6000000Y10000000I-4000000J0D01*
G01*
X14000000Y10000000D01*
G37*
",
            ),
        ];
        for (apertures, body) in refused {
            let (n_contours, is_copper) = pour_of(&negative_pour(apertures, body));
            assert_eq!(
                n_contours, 1,
                "an approximated clear image must not cut the pour: {body}"
            );
            assert!(is_copper(10.0, 10.0), "the pour stays whole: {body}");
        }
        // The counter-side: a straight stroke of a DECLARED width is exact, so
        // it does cut, and the slit it scrapes is where the copper goes.
        let (n_contours, is_copper) = pour_of(&negative_pour(
            "%ADD10C,2.000000*%\n",
            "D10*\nX10000000Y4000000D02*\nX10000000Y16000000D01*\n",
        ));
        assert_eq!(n_contours, 2, "an exact clear stroke cuts");
        assert!(!is_copper(10.0, 10.0), "the stroke's path is void");
        assert!(is_copper(4.0, 10.0), "the copper either side stands");
    }

    #[test]
    fn a_clear_under_an_unapplied_object_transform_erases_nothing() {
        // `%LS`, `%LR` and `%LM` transform the object being painted, and this
        // plotter does not apply them. Ignoring a transform only ever misplaces
        // ADDITIVE copper; a 2x1 rectangle flashed clear under `%LS0.5*%` really
        // clears 1x0.5, so subtracting the unscaled rectangle erases copper the
        // film kept. Refused while any transform is loaded, and cutting again
        // once it is back to the identity.
        for transform in ["%LS0.500000*%\n", "%LR90.000000*%\n", "%LMX*%\n"] {
            let (n, is_copper) = pour_of(&negative_pour(
                "%ADD10R,2.000000X1.000000*%\n",
                &format!("{transform}D10*\nX10000000Y10000000D03*\n"),
            ));
            assert_eq!(n, 1, "a transformed clear must not cut: {transform}");
            assert!(is_copper(10.0, 10.0), "the pour stays whole: {transform}");
        }
        // Reset to the identity and the same flash cuts again, so the refusal is
        // the transform's doing and not the aperture's.
        let (n, is_copper) = pour_of(&negative_pour(
            "%ADD10R,2.000000X1.000000*%\n",
            "%LS0.500000*%\n%LS1.000000*%\nD10*\nX10000000Y10000000D03*\n",
        ));
        assert_eq!(n, 2);
        assert!(!is_copper(10.0, 10.0));
    }

    #[test]
    fn a_hole_almost_as_wide_as_its_aperture_erases_nothing() {
        // The circumscribed rim of a clear flash's hole is only safe while it
        // stays INSIDE the outer boundary. The two contours are read even-odd,
        // not as a guaranteed `outer minus rim`, so a rim poking out flips parity
        // over untouched pour copper and erases it. A 2 mm square with a 1.999 mm
        // hole puts the rim's vertices at radius 1.0043, outside the square.
        let (n, is_copper) = pour_of(&negative_pour(
            "%ADD10R,2.000000X2.000000X1.999000*%\n",
            "D10*\nX10000000Y10000000D03*\n",
        ));
        assert_eq!(n, 1, "the rim escapes the aperture, so nothing is cut");
        assert!(
            is_copper(11.002, 10.0),
            "the copper outside the square stands"
        );
    }

    #[test]
    fn a_void_inside_an_earlier_voids_island_is_still_cut() {
        // The already-void probe asks whether the candidate sits in an earlier
        // void's REMOVED area, not merely inside its outer boundary. An annular
        // clear leaves a copper island at its centre; a later solid clear over
        // that island has real copper to remove. Treating the ring's outer
        // boundary as the covered area skipped the second cut and left the island
        // on the plane, which is exactly the merge this reader exists to break.
        let pieces = pieces_of(&negative_pour(
            "%ADD10C,6.000000X2.000000*%\n%ADD11C,2.000000*%\n",
            "D10*\nX10000000Y10000000D03*\nD11*\nX10000000Y10000000D03*\n",
        ));
        assert_eq!(
            piece_at(&pieces, 10.0, 10.0),
            None,
            "the island is cut away in turn"
        );
        assert!(
            piece_at(&pieces, 14.5, 10.0).is_some(),
            "the pour beyond the void stands"
        );
        // The two rims differ by the 0.48% circumscription, so a hair-thin ring of
        // island copper survives between them, 4 um wide. The second void's bounds
        // overlap it, so it is NOT freed into its own conductor: an island another
        // void may have touched stays a contour of the pour, over-connected, which
        // is the conservative side. Asserted so the choice is on the record.
        assert_eq!(pieces.len(), 1);
    }

    #[test]
    fn a_clear_inside_an_aperture_block_body_erases_nothing() {
        // `%AB` defines a block to be flashed later. This plotter does not
        // implement blocks, so the body arrives as ordinary commands and is
        // plotted where it is DEFINED. For dark objects that is a harmless
        // over-paint; a clear object in a body would be cut from whatever pour
        // lies under the definition coordinates, erasing copper the film never
        // clears.
        let (n, is_copper) = pour_of(&negative_pour(
            "",
            "\
%ABD12*%
G36*
X8000000Y8000000D02*
X12000000Y8000000D01*
X12000000Y12000000D01*
X8000000Y12000000D01*
X8000000Y8000000D01*
G37*
%AB*%
",
        ));
        assert_eq!(n, 1, "a clear inside a block body must not cut");
        assert!(is_copper(10.0, 10.0));
    }

    #[test]
    fn an_aperture_block_does_not_leak_its_polarity() {
        // The graphics state is restored when a block closes, so a body that ends
        // under `%LPC*%` leaves the film dark. Carrying the body's polarity out
        // read every later region and flash as a void: the film lost all its
        // copper instead of gaining a void.
        let prims = parse_layer(
            "\
%FSLAX46Y46*%
%MOMM*%
%ADD10C,1.000000*%
%ABD12*%
%LPC*%
%AB*%
G36*
X0Y0D02*
X20000000Y0D01*
X20000000Y20000000D01*
X0Y20000000D01*
X0Y0D01*
G37*
D10*
X5000000Y5000000D03*
M02*
",
        )
        .unwrap();
        assert_eq!(
            prims.iter().filter(|p| p.kind == PrimKind::Region).count(),
            1,
            "the pour after the block is copper, not a void"
        );
        assert_eq!(
            prims.iter().filter(|p| p.kind == PrimKind::Flash).count(),
            1,
            "the flash after the block is copper too"
        );
    }

    #[test]
    fn a_region_that_closes_dark_is_copper_however_it_opened() {
        // A region counts as clear only when the polarity is clear at BOTH ends.
        // Which end decides is a reading of when the region object is created, and
        // the reader must not need to be right about that: requiring both means an
        // ambiguous film is painted rather than subtracted, so a wrong reading
        // over-connects instead of fabricating a break. And it must be PAINTED,
        // not dropped, because dropping copper under-connects exactly as a
        // fabricated open does.
        let pieces = pieces_of(&negative_pour(
            "",
            "\
G36*
X8000000Y8000000D02*
X12000000Y8000000D01*
X12000000Y12000000D01*
X8000000Y12000000D01*
X8000000Y8000000D01*
%LPD*%
G37*
",
        ));
        assert_eq!(pieces.len(), 2, "the pour, plus the region it paints");
        assert_eq!(
            pieces[0].len(),
            1,
            "the pour is uncut: a region that closes DARK is not a void"
        );
        assert!(
            piece_at(&pieces, 10.0, 10.0).is_some(),
            "the ambiguous region is copper, not a hole and not thrown away"
        );
    }

    #[test]
    fn overlapping_antipads_both_stay_void() {
        // Two vias on 0.7 mm pitch with 1 mm plane clearance: ordinary geometry,
        // and the two clear discs overlap. Classifying the cut pour's contours by
        // nesting depth is invalid here, because that classifier's precondition is
        // that contours never cross and voids across a film DO cross: the lower
        // disc's witness vertex lands inside the upper one, reads as even depth,
        // i.e. an outer boundary, and the void was promoted to a phantom polygon
        // of COPPER and dropped from the pour. On a 12000-antipad plane whose
        // columns overlap that promoted 11890 voids and the board-sized sheet came
        // straight back.
        let pieces = pieces_of(&negative_pour(
            "%ADD10C,1.000000*%\n",
            "D10*\nX5000000Y5000000D03*\nX5000000Y5700000D03*\n",
        ));
        assert_eq!(
            piece_at(&pieces, 5.0, 5.0),
            None,
            "the lower antipad is void"
        );
        assert_eq!(
            piece_at(&pieces, 5.0, 5.7),
            None,
            "the upper antipad is void"
        );
        assert!(piece_at(&pieces, 2.0, 2.0).is_some(), "the plane stands");
    }

    #[test]
    fn one_clear_region_may_hold_several_voids_in_any_order() {
        // RS-274X 4.10.4 lets ONE region statement carry several contours, an
        // outer plus its holes OR several disjoint islands, in any order. Banking
        // them as one void and calling everything after the first a hole is a guess
        // about draw order, and it undoes the void: the contour is dropped from the
        // pour AND re-emitted as copper.
        //
        // (a) Two disjoint voids in one statement. The second was cancelled
        // outright, so on a film whose exporter batches a net's clearances into one
        // region statement only the first antipad survived and the sheet came back.
        let pieces = pieces_of(&negative_pour(
            "",
            "\
G36*
X4000000Y4000000D02*
X6000000Y4000000D01*
X6000000Y6000000D01*
X4000000Y6000000D01*
X4000000Y4000000D01*
X14000000Y14000000D02*
X16000000Y14000000D01*
X16000000Y16000000D01*
X14000000Y16000000D01*
X14000000Y14000000D01*
G37*
",
        ));
        assert_eq!(piece_at(&pieces, 5.0, 5.0), None, "the first void is cut");
        assert_eq!(piece_at(&pieces, 15.0, 15.0), None, "so is the second");
        // (b) An annular void drawn HOLE FIRST. Promoting its outer boundary read
        // the cleared ring as copper and shorted the island back to the plane.
        let pieces = pieces_of(&negative_pour(
            "",
            "\
G36*
X9000000Y9000000D02*
X11000000Y9000000D01*
X11000000Y11000000D01*
X9000000Y11000000D01*
X9000000Y9000000D01*
X6000000Y6000000D02*
X14000000Y6000000D01*
X14000000Y14000000D01*
X6000000Y14000000D01*
X6000000Y6000000D01*
G37*
",
        ));
        assert_eq!(
            piece_at(&pieces, 12.5, 10.0),
            None,
            "the ring the film cleared is void whichever contour came first"
        );
        let island = piece_at(&pieces, 10.0, 10.0).expect("the island is copper");
        let pour = piece_at(&pieces, 18.0, 10.0).expect("the pour is copper");
        assert_ne!(island, pour, "and the island is not the pour's conductor");
    }

    #[test]
    fn a_void_emitted_twice_is_cut_once() {
        // The same clear flash written twice, which panelisers and re-plots do.
        // The duplicate's vertices sit exactly ON the first void's boundary, and
        // point-in-polygon is half-open, so it read as not-covered, was appended a
        // second time, and even-odd flipped the clearance back to COPPER: a pad
        // there shorts to the plane, which is the merge again.
        let (n, is_copper) = pour_of(&negative_pour(
            "%ADD10C,4.000000*%\n",
            "D10*\nX10000000Y10000000D03*\nX10000000Y10000000D03*\n",
        ));
        assert_eq!(n, 2, "the duplicate adds nothing to remove");
        assert!(!is_copper(10.0, 10.0), "the clearance stays clear");
    }

    #[test]
    fn a_step_and_repeat_cell_carries_its_voids_into_every_copy() {
        // A negatively-drawn cell inside `%SRX2Y1I25.0J0*%`. The replica gets a
        // translated copy of the pour, so it must get translated copies of the
        // voids too; replicating only the copper leaves every repeat but the
        // first as a solid slab.
        let g = "\
%FSLAX46Y46*%
%MOMM*%
%SRX2Y1I25.000000J0.000000*%
G36*
X0Y0D02*
X20000000Y0D01*
X20000000Y20000000D01*
X0Y20000000D01*
X0Y0D01*
G37*
%LPC*%
G36*
X8000000Y8000000D02*
X12000000Y8000000D01*
X12000000Y12000000D01*
X8000000Y12000000D01*
X8000000Y8000000D01*
G37*
%SR*%
M02*
";
        let prims = parse_layer(g).unwrap();
        let pours: Vec<&CopperPrim> = prims
            .iter()
            .filter(|p| p.kind == PrimKind::Region)
            .collect();
        assert_eq!(pours.len(), 2, "the cell is repeated twice");
        for (i, p) in pours.iter().enumerate() {
            let dx = 25.0 * i as f64;
            let Shape::MultiPolygon { contours } = &p.shape else {
                panic!("copy {i} kept no void, so it is a solid slab");
            };
            assert_eq!(contours.len(), 2);
            assert!(!super::super::geo::point_in_contours(
                10.0 + dx,
                10.0,
                contours
            ));
            assert!(super::super::geo::point_in_contours(
                17.0 + dx,
                10.0,
                contours
            ));
        }
    }

    #[test]
    fn a_step_and_repeat_cell_of_voids_alone_is_still_repeated() {
        // The cell need not contain copper. A pour painted before the block, then
        // an arrayed set of clear antipads over it, is a legal way to write a
        // plane's via clearances once. Bailing out on an empty base CELL
        // replicated none of the voids, so every repeat but the first kept its
        // copper and the merge survived exactly where the file said it must not.
        let g = "\
%FSLAX46Y46*%
%MOMM*%
%ADD10C,4.000000*%
G36*
X0Y0D02*
X40000000Y0D01*
X40000000Y20000000D01*
X0Y20000000D01*
X0Y0D01*
G37*
%LPC*%
%SRX3Y1I10.000000J0.000000*%
D10*
X10000000Y10000000D03*
%SR*%
M02*
";
        let prims = parse_layer(g).unwrap();
        let pours: Vec<&CopperPrim> = prims
            .iter()
            .filter(|p| p.kind == PrimKind::Region)
            .collect();
        assert_eq!(pours.len(), 1, "the copper was painted before the block");
        let Shape::MultiPolygon { contours } = &pours[0].shape else {
            panic!("no void was cut at all");
        };
        assert_eq!(contours.len(), 4, "one boundary plus all three antipads");
        for k in 0..3 {
            let x = 10.0 + 10.0 * k as f64;
            assert!(
                !super::super::geo::point_in_contours(x, 10.0, contours),
                "antipad {k} at x={x} must be void"
            );
        }
    }

    #[test]
    fn single_quadrant_g74_arc_uses_correct_center() {
        // R6: a quarter arc from (1,0) to (0,1). Under G74 the I/J offset is an
        // unsigned magnitude (I1 J0); the true centre is the origin (radius 1).
        // The multi-quadrant formula would put the centre at start + (I,J) =
        // (2,0), throwing every arc point up to 2 mm off its real position.
        let g = "\
%FSLAX46Y46*%
%MOMM*%
%ADD10C,0.100000*%
G74*
G03*
D10*
X1000000Y0D02*
X0Y1000000I1000000J0D01*
M02*
";
        let prims = parse_layer(g).unwrap();
        let tracks: Vec<_> = prims.iter().filter(|p| p.kind == PrimKind::Track).collect();
        assert!(!tracks.is_empty(), "the arc should produce track segments");
        for p in &tracks {
            if let Shape::Capsule(c) = &p.shape {
                for (x, y) in [(c.ax, c.ay), (c.bx, c.by)] {
                    let radius = (x * x + y * y).sqrt();
                    assert!(
                        (radius - 1.0).abs() < 0.05,
                        "arc point ({x:.3},{y:.3}) is {radius:.3} mm from the origin, \
                         expected ~1 mm (a wrong centre puts it up to 3 mm out)"
                    );
                }
            }
        }
    }

    #[test]
    fn region_arc_segment_is_flattened_not_chorded() {
        // A filled circle drawn as a G36 region of two G03 semicircles (centre
        // at the origin, radius 1 mm). Recording only each D01's ENDPOINT,
        // ignoring the circular interpolation mode and the I/J offset, collapses
        // the contour to the degenerate chord polygon
        // [(-1,0),(1,0),(-1,0)]: zero area, and the whole pour vanishes from
        // connectivity (false OPEN for anything connecting through it).
        let g = "\
%FSLAX46Y46*%
%MOMM*%
G75*
G36*
X-1000000Y0D02*
G03*
X1000000Y0I1000000J0D01*
X-1000000Y0I-1000000J0D01*
G37*
M02*
";
        let prims = parse_layer(g).unwrap();
        let region = prims
            .iter()
            .find(|p| p.kind == PrimKind::Region)
            .expect("the pour must materialize as a region");
        let Shape::Polygon { pts, .. } = &region.shape else {
            panic!("a single-contour region stays a plain polygon");
        };
        assert!(
            pts.len() > 3,
            "two flattened semicircles carry many vertices, got {} (3 = the chord collapse)",
            pts.len()
        );
        // Every boundary vertex sits on the 1 mm circle (the same centre/sweep
        // math as a stroked arc), and the disc INTERIOR is inside the polygon.
        for &(x, y) in pts {
            let radius = (x * x + y * y).sqrt();
            assert!(
                (radius - 1.0).abs() < 1e-6,
                "contour vertex ({x:.4},{y:.4}) is off the arc circle"
            );
        }
        assert!(
            point_in_polygon(0.0, 0.0, pts),
            "the disc centre must be INSIDE the filled region (a chord polygon has no inside)"
        );
    }

    #[test]
    fn region_disjoint_contours_do_not_bridge() {
        // One G36 region holding TWO disjoint square islands (RS-274X 4.10.4:
        // each contour begins with a D02 move): [0,5]x[0,5] and [95,100]x[0,5],
        // 90 mm apart. A flat contour vector never splits on the second
        // D02: it drops that contour's start vertex and emits ONE polygon
        // with a phantom bridge edge, reading two electrically-isolated pads
        // (one per island) onto the same net, a false SHORT.
        let g = "\
%FSLAX46Y46*%
%MOMM*%
G36*
X0Y0D02*
X5000000Y0D01*
X5000000Y5000000D01*
X0Y5000000D01*
X0Y0D01*
X95000000Y0D02*
X100000000Y0D01*
X100000000Y5000000D01*
X95000000Y5000000D01*
X95000000Y0D01*
G37*
M02*
";
        let prims = parse_layer(g).unwrap();
        let regions: Vec<_> = prims
            .iter()
            .filter(|p| p.kind == PrimKind::Region)
            .collect();
        assert_eq!(
            regions.len(),
            2,
            "two disjoint islands are two separate copper pieces, not one bridged polygon"
        );
        // Containment: a point in each square is copper, the 90 mm gap is not.
        let covered = |x: f64, y: f64| {
            regions.iter().any(|p| match &p.shape {
                Shape::Polygon { pts, .. } => point_in_polygon(x, y, pts),
                _ => panic!("hole-less islands stay plain polygons"),
            })
        };
        assert!(covered(2.5, 2.5), "inside the first island");
        assert!(covered(97.5, 2.5), "inside the second island");
        assert!(
            !covered(50.0, 2.5),
            "the gap between the islands is NOT copper"
        );
        // Connectivity: a pad on each island must land on DIFFERENT nets. With
        // the bridged single polygon both pads unioned through the one region
        // primitive onto one net.
        let mut layer: Vec<CopperPrim> = prims.clone();
        layer.push(CopperPrim::bare(
            Shape::disc(2.5, 2.5, 0.5),
            PrimKind::Flash,
        ));
        layer.push(CopperPrim::bare(
            Shape::disc(97.5, 2.5, 0.5),
            PrimKind::Flash,
        ));
        let (_board, stats) = crate::gerber::connect::reconstruct("t", vec![layer], vec![], vec![]);
        assert_eq!(
            stats.n_nets, 2,
            "one pad per island: two isolated nets, not a false short"
        );
    }

    #[test]
    fn region_hole_contour_is_cut_out_not_filled() {
        // A region with a hole: outer square [0,20]x[0,20], inner square hole
        // [8,12]x[8,12] as a second contour. The ring is copper; the hole
        // interior is NOT. A flat concatenation bridges the two contours
        // and drops the hole's start vertex, so two non-coincident bridge
        // edges enclose a sliver of RING copper, (0,0)-(12,8)-(8,8), whose
        // parity reads OUTSIDE (false open through the ring).
        let g = "\
%FSLAX46Y46*%
%MOMM*%
G36*
X0Y0D02*
X20000000Y0D01*
X20000000Y20000000D01*
X0Y20000000D01*
X0Y0D01*
X8000000Y8000000D02*
X12000000Y8000000D01*
X12000000Y12000000D01*
X8000000Y12000000D01*
X8000000Y8000000D01*
G37*
M02*
";
        let prims = parse_layer(g).unwrap();
        let regions: Vec<_> = prims
            .iter()
            .filter(|p| p.kind == PrimKind::Region)
            .collect();
        assert_eq!(regions.len(), 1, "outer + its hole is ONE piece of copper");
        let Shape::MultiPolygon { contours } = &regions[0].shape else {
            panic!("a region with a hole must carry both contours, not one flat ring");
        };
        assert_eq!(contours.len(), 2, "the outer boundary and its hole");
        use crate::gerber::geo::point_in_contours;
        assert!(
            point_in_contours(2.0, 15.0, contours),
            "a point in the ring is copper"
        );
        assert!(
            point_in_contours(6.0, 4.5, contours),
            "ring copper on the old bridge-edge sliver must still be INSIDE"
        );
        assert!(
            !point_in_contours(10.0, 10.0, contours),
            "the hole interior is NOT copper"
        );
        assert!(
            !point_in_contours(30.0, 30.0, contours),
            "outside the outer boundary is NOT copper"
        );
    }

    #[test]
    fn step_and_repeat_replicates_the_base_cell() {
        // R14: a %SRX2Y1I10J0*% block flashing one pad must produce TWO pads,
        // the base copy at (0,0) and a repeated copy 10 mm along x. The old
        // plotter dropped every StepAndRepeat command, so the repeated copies
        // (all copper/pads but the first) vanished from a panelized layer.
        let g = "\
%FSLAX46Y46*%
%MOMM*%
%ADD10C,0.500000*%
D10*
%SRX2Y1I10.0J0.0*%
X0Y0D03*
%SR*%
M02*
";
        let prims = parse_layer(g).unwrap();
        let flashes: Vec<_> = prims.iter().filter(|p| p.kind == PrimKind::Flash).collect();
        assert_eq!(
            flashes.len(),
            2,
            "SR X2 must emit the base + 1 repeated flash"
        );
        let xs: Vec<f64> = flashes
            .iter()
            .filter_map(|p| match &p.shape {
                Shape::Capsule(c) => Some(c.ax),
                _ => None,
            })
            .collect();
        assert!(
            xs.iter().any(|x| x.abs() < 1e-6),
            "the base flash sits at x≈0, got {xs:?}"
        );
        assert!(
            xs.iter().any(|x| (x - 10.0).abs() < 1e-6),
            "the repeated flash sits 10 mm along x, got {xs:?}"
        );
    }

    #[test]
    fn inch_unit_macro_fallback_disc_is_a_fixed_physical_size() {
        // R14: when an aperture macro can't be instantiated (here a Circle whose
        // diameter references an undefined variable), the flash falls back to a
        // small anchor disc. Its radius is a fixed 0.25 mm, NOT scaled by the
        // document's unit factor. On an inch board (to_mm = 25.4) a scaled
        // `0.25 * to_mm` bloats it to 6.35 mm, big enough to merge nets.
        let g = "\
%FSLAX46Y46*%
%MOIN*%
%AMBADX*1,1,$1,0,0*%
%ADD10BADX*%
D10*
X0Y0D03*
M02*
";
        let prims = parse_layer(g).unwrap();
        let flash = prims
            .iter()
            .find(|p| p.kind == PrimKind::Flash)
            .expect("a fallback flash");
        let r = match &flash.shape {
            Shape::Capsule(c) => c.r,
            _ => panic!("fallback should be a disc/capsule"),
        };
        assert!(
            (r - MACRO_FALLBACK_DISC_MM).abs() < 1e-9,
            "fallback disc radius must be a fixed {MACRO_FALLBACK_DISC_MM} mm regardless of \
             inch units, got {r} mm (0.25*25.4 = 6.35 was the bug)"
        );
    }

    #[test]
    fn inch_unit_unknown_aperture_stroke_is_a_fixed_hairline() {
        // R40: the fallback stroke width for a non-circle/rect/obround aperture was
        // `0.1 * to_mm`. On an inch board (to_mm = 25.4) that is 2.54 mm, a fat
        // capsule (1.27 mm radius) that union-merges adjacent copper into a false
        // short, not the intended 0.1 mm hairline. The width is a fixed physical
        // mm and must not be unit-scaled (same rule as MACRO_FALLBACK_DISC_MM).
        // A polygon aperture (P) hits the unknown-aperture fallback arm.
        let g = "\
%FSLAX46Y46*%
%MOIN*%
%ADD10P,0.5X4*%
D10*
X0Y0D02*
X0100000Y0D01*
M02*
";
        let prims = parse_layer(g).unwrap();
        let track = prims
            .iter()
            .find(|p| p.kind == PrimKind::Track)
            .expect("a stroked track");
        let r = match &track.shape {
            Shape::Capsule(c) => c.r,
            _ => panic!("a stroke should be a capsule"),
        };
        // width 0.1 mm -> radius 0.05 mm, regardless of inch units.
        assert!(
            (r - 0.05).abs() < 1e-9,
            "inch-unit fallback stroke radius must be a fixed 0.05 mm, got {r} mm (0.1*25.4/2 = 1.27 was the bug)"
        );
    }

    // One film, twice: with its X2 attributes and stripped of them. The
    // geometry must be identical either way; only the identity differs.
    const X2_FILM: &str = "\
%FSLAX46Y46*%
%MOMM*%
%TF.FileFunction,Copper,L1,Top*%
%TA.AperFunction,SMDPad,CuDef*%
%ADD10C,1.000000*%
%TD*%
%TA.AperFunction,ViaPad*%
%ADD11C,0.600000*%
%TD*%
D10*
%TO.P,R1,1*%
%TO.N,VCC*%
X0Y0D03*
%TO.P,R1,2*%
%TO.N,SIG*%
X2000000Y0D03*
%TD*%
D11*
%TO.N,VCC*%
X5000000Y0D03*
%TD*%
M02*
";
    const STRIPPED_FILM: &str = "\
%FSLAX46Y46*%
%MOMM*%
%ADD10C,1.000000*%
%ADD11C,0.600000*%
D10*
X0Y0D03*
X2000000Y0D03*
D11*
X5000000Y0D03*
M02*
";

    #[test]
    fn x2_attributes_bind_pin_net_and_function_to_flashes() {
        let prims = parse_layer(X2_FILM).unwrap();
        let flashes: Vec<_> = prims.iter().filter(|p| p.kind == PrimKind::Flash).collect();
        assert_eq!(flashes.len(), 2, "two pad flashes (the via is not a pad)");
        let pin_of = |a: &X2Attrs| a.pin.as_ref().map(|(r, p)| (r.to_string(), p.to_string()));
        assert_eq!(
            pin_of(&flashes[0].attrs),
            Some(("R1".to_string(), "1".to_string()))
        );
        let names_of = |a: &X2Attrs| {
            a.net_names()
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names_of(&flashes[0].attrs), vec!["VCC"]);
        assert!(matches!(
            flashes[0].attrs.function,
            Some(ApertureFunction::SmdPad(_))
        ));
        assert_eq!(
            pin_of(&flashes[1].attrs),
            Some(("R1".to_string(), "2".to_string()))
        );
        assert_eq!(names_of(&flashes[1].attrs), vec!["SIG"]);

        // The `%TA.AperFunction,ViaPad` flash is classified as a via outright:
        // the film said what it is, so it is not left for a footprint window
        // to mistake for a component pad.
        let via = prims
            .iter()
            .find(|p| p.kind == PrimKind::Via)
            .expect("the ViaPad flash becomes PrimKind::Via");
        assert_eq!(names_of(&via.attrs), vec!["VCC"]);
        assert_eq!(via.attrs.pin, None, "a via names no component pin");
    }

    #[test]
    fn stripped_film_yields_identical_geometry_and_no_attributes() {
        // The two-sided guarantee: stripping the X2 attributes changes NOTHING
        // about the geometry. Every primitive's shape must be bit-for-bit the
        // shape the attributed film produced, every attrs empty, and the via
        // honestly degrades to an (ambiguous) plain flash.
        let with = parse_layer(X2_FILM).unwrap();
        let without = parse_layer(STRIPPED_FILM).unwrap();
        assert_eq!(with.len(), without.len(), "same primitive count");
        for (a, b) in with.iter().zip(without.iter()) {
            assert_eq!(
                format!("{:?}", a.shape),
                format!("{:?}", b.shape),
                "geometry must not depend on X2 attributes"
            );
            assert!(b.attrs.is_empty(), "a stripped film carries no identity");
        }
        assert!(
            without.iter().all(|p| p.kind == PrimKind::Flash),
            "without the attribute the via is indistinguishable from a pad"
        );
    }

    #[test]
    fn td_clears_the_object_dictionary() {
        // The via flash after `%TD*%` must NOT inherit R1's pin: a dangling
        // object dictionary would bind every later object to the last pad.
        let g = "\
%FSLAX46Y46*%
%MOMM*%
%ADD10C,1.000000*%
D10*
%TO.P,R1,1*%
X0Y0D03*
%TD*%
X2000000Y0D03*
M02*
";
        let prims = parse_layer(g).unwrap();
        let flashes: Vec<_> = prims.iter().filter(|p| p.kind == PrimKind::Flash).collect();
        assert_eq!(&*flashes[0].attrs.pin.as_ref().unwrap().0, "R1");
        assert_eq!(flashes[1].attrs.pin, None, "%TD cleared the dictionary");
    }

    // A macro pad with a punched-out void (dark 4x4 square, clear 2 mm circle),
    // flashed at the origin, plus one small foreign disc whose position the
    // test chooses. Two-sided: copper in the VOID stays separate; copper on
    // the RING is a genuine overlap and unions.
    fn donut_job(foreign_x_mm: f64) -> String {
        format!(
            "%FSLAX46Y46*%\n\
             %MOMM*%\n\
             %AMDONUT*21,1,4,4,0,0,0*1,0,2,0,0*%\n\
             %ADD10DONUT*%\n\
             %ADD11C,0.500000*%\n\
             D10*\n\
             X0Y0D03*\n\
             D11*\n\
             X{}Y0D03*\n\
             M02*\n",
            (foreign_x_mm * 1e6) as i64
        )
    }

    #[test]
    fn macro_void_does_not_swallow_foreign_copper() {
        // Foreign 0.5 mm disc at the void centre: with the void read as solid
        // (the old convex-hull-only behavior) this was one net, a false
        // short. It must be TWO nets.
        let prims = parse_layer(&donut_job(0.0)).unwrap();
        assert_eq!(prims.len(), 2, "the macro pad and the foreign disc");
        assert!(
            matches!(prims[0].shape, Shape::MultiPolygon { .. }),
            "a voided macro flash carries its hole contour"
        );
        let (_b, s) = crate::gerber::connect::reconstruct("t", vec![prims], vec![], vec![]);
        assert_eq!(
            s.n_nets, 2,
            "copper inside the macro's void is NOT touching the pad"
        );

        // The same foreign disc moved onto the ring: genuinely touching.
        let prims = parse_layer(&donut_job(1.6)).unwrap();
        let (_b, s) = crate::gerber::connect::reconstruct("t", vec![prims], vec![], vec![]);
        assert_eq!(s.n_nets, 1, "copper on the ring is a genuine overlap");
    }

    #[test]
    fn aperture_hole_diameter_is_bare_board_not_copper() {
        // `%ADD10C,2.0X1.0*%`: a 2 mm disc with a 1 mm hole. The hole is not
        // part of the aperture image (RS-274X 4.4.6), so foreign copper
        // passing through it must NOT union with the pad, while copper on the
        // annular ring must. Discarding the hole diameter (the old behavior)
        // made the first case a false short.
        let job = |fx_mm: f64| {
            format!(
                "%FSLAX46Y46*%\n%MOMM*%\n%ADD10C,2.000000X1.000000*%\n%ADD11C,0.200000*%\n\
                 D10*\nX0Y0D03*\nD11*\nX{}Y0D03*\nM02*\n",
                (fx_mm * 1e6) as i64
            )
        };
        let prims = parse_layer(&job(0.0)).unwrap();
        assert!(
            matches!(prims[0].shape, Shape::MultiPolygon { .. }),
            "a holed flash carries its hole contour"
        );
        let (_b, s) = crate::gerber::connect::reconstruct("t", vec![prims], vec![], vec![]);
        assert_eq!(s.n_nets, 2, "copper in the hole is not on the pad");
        let prims = parse_layer(&job(0.8)).unwrap();
        let (_b, s) = crate::gerber::connect::reconstruct("t", vec![prims], vec![], vec![]);
        assert_eq!(s.n_nets, 1, "copper on the annular ring is");
    }

    #[test]
    fn single_line_aperture_macro_survives_normalization() {
        // A single-line aperture macro packs its primitives with '*'
        // separators. The block splitter must not treat them as independent
        // extended codes and collapse the macro to an empty def, that shrinks
        // the pad to a fallback disc. AMBOX is a 2×2 CenterLine rectangle.
        let g = "\
%FSLAX46Y46*%
%MOMM*%
%AMBOX*21,1,2,2,0,0,0*%
%ADD10BOX*%
D10*
X0Y0D03*
M02*
";
        let prims = parse_layer(g).unwrap();
        let f = prims
            .iter()
            .find(|p| p.kind == PrimKind::Flash)
            .expect("flash");
        if let Shape::Polygon { pts, .. } = &f.shape {
            let (minx, maxx) = pts
                .iter()
                .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.0), b.max(p.0)));
            assert!(
                (maxx - minx - 2.0).abs() < 1e-3,
                "macro rect width {} (expected ~2 mm; the macro was destroyed if it collapsed)",
                maxx - minx
            );
        } else {
            panic!("macro flash must instantiate a polygon, not a fallback disc");
        }
    }
}
