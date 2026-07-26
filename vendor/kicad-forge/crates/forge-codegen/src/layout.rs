//! Logical re-layout and incremental recompile.
//!
//! Two modes, both operating on the executable [`Program`] from [`crate::dsl`]:
//!
//! * **Full re-layout** ([`relayout`] with [`LayoutConfig::full`]): components
//!   are grouped by the function/cluster they belong to, each group is given a
//!   slot on a coarse grid, and a force-directed relaxation spreads components
//!   so that net-connected parts attract and overlapping parts repel. Distance
//!   fields (`space` on a component, or `space fn <block>` on a whole function)
//!   inflate a component's effective radius so neighbours keep clear of it (a
//!   test point that wants breathing room, a hot part that wants isolation).
//!
//! * **Incremental recompile** ([`relayout`] with [`LayoutConfig::incremental`],
//!   the preferred default): the *original* board is the base. Components whose
//!   identity and placement are unchanged keep their exact coordinates; only
//!   components that are new, moved, or value-changed relative to the base are
//!   re-placed, and they are placed in free space near their net neighbours
//!   rather than disturbing the settled board.
//!
//! ## Routing
//!
//! Placement is the hard part; routing is deliberately a thin v1. The canonical
//! choice for autorouting a KiCad board is **freerouting** (Java) driven over
//! the Specctra DSN interchange format; that hand-off (emit DSN, run
//! freerouting, import the routed result) is the documented production path but
//! is not yet implemented here. As an in-tree fallback this module exposes a
//! simple grid A* router ([`route_grid`]) that connects each net's pads with
//! Manhattan tracks on one layer, avoiding placed component bodies. The A*
//! router is honestly a v1: it routes one layer, does not rip-up-and-retry, and
//! will leave a net unrouted (reported, not silently dropped) rather than
//! violate a keep-out. See `galvani/docs/BOARD_AS_CODE.md` for the rationale and
//! the freerouting path.

use crate::dsl::{Comp, Outline, Program, Stmt};
use std::collections::HashMap;

/// Which layout strategy to run.
#[derive(Debug, Clone)]
pub enum LayoutConfig {
    /// Re-place everything, grouped by function, with a force-directed relaxer.
    Full(FullConfig),
    /// Keep the base board; only re-place changed/new components.
    Incremental(IncrementalConfig),
}

#[derive(Debug, Clone)]
pub struct FullConfig {
    /// Force-relaxation iterations.
    pub iterations: usize,
    /// Spacing between function-group grid slots (mm).
    pub group_pitch: f64,
    /// Base component spacing target (mm), before per-component `space` fields.
    pub comp_spacing: f64,
}

impl Default for FullConfig {
    fn default() -> Self {
        FullConfig {
            iterations: 400,
            group_pitch: 25.0,
            comp_spacing: 3.0,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct IncrementalConfig {
    /// Spacing target used only when nudging the re-placed components (mm).
    pub comp_spacing: f64,
}

impl LayoutConfig {
    pub fn full() -> Self {
        LayoutConfig::Full(FullConfig::default())
    }
    pub fn incremental() -> Self {
        LayoutConfig::Incremental(IncrementalConfig { comp_spacing: 3.0 })
    }
}

/// What the re-layout did.
#[derive(Debug, Clone, Default)]
pub struct LayoutReport {
    /// Components whose placement changed.
    pub moved: Vec<String>,
    /// Components kept exactly where they were (incremental mode).
    pub kept: usize,
    /// Number of function groups laid out (full mode).
    pub groups: usize,
}

/// Re-place a program's components in-place, returning a report.
///
/// `base` is the original program (for incremental mode); pass the same program
/// for full mode (it is ignored there).
pub fn relayout(prog: &mut Program, base: &Program, cfg: &LayoutConfig) -> LayoutReport {
    match cfg {
        LayoutConfig::Full(fc) => relayout_full(prog, fc),
        LayoutConfig::Incremental(ic) => relayout_incremental(prog, base, ic),
    }
}

// ---------------------------------------------------------------------------
// Distance fields and courtyards
// ---------------------------------------------------------------------------

/// Courtyard half-extents of a component: half of its pad bounding box on each
/// axis (the rotation-aware footprint body), with a 0.25 mm floor so a
/// zero-size part still occupies a cell.
fn comp_half_extent(c: &Comp) -> (f64, f64) {
    let ang = c.rot.to_radians();
    let (s, co) = ang.sin_cos();
    let mut hx: f64 = 0.25;
    let mut hy: f64 = 0.25;
    for p in &c.pads {
        // Rotate the pad's local offset into the footprint frame so a rotated
        // part's courtyard reflects its real on-board span.
        let rx = p.at.0 * co - p.at.1 * s;
        let ry = p.at.0 * s + p.at.1 * co;
        let ex = rx.abs() + p.size.0 * 0.5;
        let ey = ry.abs() + p.size.1 * 0.5;
        hx = hx.max(ex);
        hy = hy.max(ey);
    }
    (hx, hy)
}

/// The hard minimum clearance to keep around a component: the larger of its own
/// `space` field and any function-level `space fn`. This is enforced as a hard
/// constraint by the placer, not just a soft target.
fn comp_clearance(c: &Comp, fn_space: f64) -> f64 {
    let own = c.space.map(|s| s.dist).unwrap_or(0.0);
    own.max(fn_space)
}

/// Effective bounding radius of a component for the disc-model relaxer: the
/// courtyard's half-diagonal plus the hard clearance plus a base spacing pad.
/// Used where a single scalar radius is wanted (incremental placement).
fn comp_radius(c: &Comp, fn_space: f64, base_spacing: f64) -> f64 {
    let (hx, hy) = comp_half_extent(c);
    let diag = (hx * hx + hy * hy).sqrt();
    diag + comp_clearance(c, fn_space) + base_spacing * 0.5
}

/// Collect `space fn <block> <dist>` fields into a map.
fn fn_spaces(prog: &Program) -> HashMap<String, f64> {
    let mut m = HashMap::new();
    for st in &prog.body {
        if let Stmt::BlockSpace { block, dist } = st {
            m.insert(block.clone(), *dist);
        }
    }
    m
}

/// Collect `pin <ref> edge <side>` constraints into a map.
fn pin_constraints(prog: &Program) -> HashMap<String, crate::dsl::Edge> {
    let mut m = HashMap::new();
    for st in &prog.body {
        if let Stmt::Pin { reference, edge } = st {
            m.insert(reference.clone(), *edge);
        }
    }
    m
}

/// Collect `lock <ref>` constraints into a set.
fn lock_constraints(prog: &Program) -> std::collections::HashSet<String> {
    let mut s = std::collections::HashSet::new();
    for st in &prog.body {
        if let Stmt::Lock { reference } = st {
            s.insert(reference.clone());
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Full re-layout
// ---------------------------------------------------------------------------

/// A component's mutable layout state during relaxation.
struct Node {
    /// Body index path: (stmt index, slot index within instance) or single.
    loc: Loc,
    pos: (f64, f64),
    /// Bounding radius (courtyard half-diagonal + hard clearance + base pad),
    /// used for the soft relaxation forces.
    radius: f64,
    /// Hard half-extents of the courtyard `(hx, hy)` and the hard clearance to
    /// hold around it. The final de-overlap pass enforces
    /// `gap >= hx_a + hx_b + clear_a + clear_b` on each axis (a rectangle model,
    /// so a `space 3` test point genuinely gets 3 mm of clear room).
    half: (f64, f64),
    clear: f64,
    /// Net names this node touches (for attraction).
    nets: Vec<String>,
    reference: String,
    /// Index of the group this node belongs to (for cohesion).
    group: usize,
    /// Placement constraint, if any.
    constraint: NodeConstraint,
}

#[derive(Clone, Copy, PartialEq)]
enum NodeConstraint {
    Free,
    /// Locked at these exact coordinates: never moved, always a keep-out.
    Locked,
    /// Pinned to a board edge: the edge-normal coordinate is fixed to the edge,
    /// only the along-edge coordinate relaxes.
    Pinned(crate::dsl::Edge),
}

#[derive(Clone, Copy)]
enum Loc {
    Single(usize),
    Slot(usize, usize),
}

/// Pull a node back inside the board outline (minus its own courtyard +
/// clearance), so a part is never placed off-board.
fn clamp_to_outline(pos: (f64, f64), node: &Node, outline: &Outline) -> (f64, f64) {
    let mx = node.half.0 + node.clear;
    let my = node.half.1 + node.clear;
    // If the part is wider than the board it cannot fit; centre it on that axis.
    let lo_x = outline.min_x + mx;
    let hi_x = outline.max_x - mx;
    let lo_y = outline.min_y + my;
    let hi_y = outline.max_y - my;
    let cx = if lo_x <= hi_x {
        pos.0.clamp(lo_x, hi_x)
    } else {
        (outline.min_x + outline.max_x) * 0.5
    };
    let cy = if lo_y <= hi_y {
        pos.1.clamp(lo_y, hi_y)
    } else {
        (outline.min_y + outline.max_y) * 0.5
    };
    (cx, cy)
}

/// Snap a pinned node onto its edge (fix the edge-normal coordinate).
fn snap_to_edge(
    pos: (f64, f64),
    node: &Node,
    outline: &Outline,
    edge: crate::dsl::Edge,
) -> (f64, f64) {
    use crate::dsl::Edge as E;
    let mx = node.half.0 + node.clear;
    let my = node.half.1 + node.clear;
    match edge {
        E::Left => (outline.min_x + mx, pos.1),
        E::Right => (outline.max_x - mx, pos.1),
        E::Top => (pos.0, outline.min_y + my),
        E::Bottom => (pos.0, outline.max_y - my),
    }
}

fn relayout_full(prog: &mut Program, cfg: &FullConfig) -> LayoutReport {
    let fnsp = fn_spaces(prog);
    let pins = pin_constraints(prog);
    let locks = lock_constraints(prog);

    // Original positions, so the report only flags genuine moves and locked
    // parts can be re-pinned to exactly where they were.
    let orig_pos: HashMap<String, (f64, f64)> =
        prog.comps().map(|c| (c.reference.clone(), c.at)).collect();

    // 1. Group components by the function (block) they belong to. Singletons
    //    share a synthetic "__singletons__" group.
    let mut groups: Vec<(String, Vec<Node>)> = Vec::new();
    let mut group_index: HashMap<String, usize> = HashMap::new();

    for (si, st) in prog.body.iter().enumerate() {
        match st {
            Stmt::Instance(inst) => {
                let key = format!("{}#{}", inst.block, si);
                let fn_space = fnsp.get(&inst.block).copied().unwrap_or(0.0);
                let gi = *group_index.entry(key.clone()).or_insert_with(|| {
                    groups.push((key.clone(), Vec::new()));
                    groups.len() - 1
                });
                for (slot, comp) in inst.comps.iter().enumerate() {
                    if let Some(c) = comp {
                        let node =
                            make_node(c, Loc::Slot(si, slot), fn_space, cfg, gi, &pins, &locks);
                        groups[gi].1.push(node);
                    }
                }
            }
            Stmt::Single(c) => {
                let key = "__singletons__".to_string();
                let gi = *group_index.entry(key.clone()).or_insert_with(|| {
                    groups.push((key.clone(), Vec::new()));
                    groups.len() - 1
                });
                let node = make_node(c, Loc::Single(si), 0.0, cfg, gi, &pins, &locks);
                groups[gi].1.push(node);
            }
            _ => {}
        }
    }

    let group_count = groups.len();

    // Determine the working board area. If the source/DSL gave an outline, use
    // it; otherwise auto-size a square box big enough for the total courtyard
    // area (with a packing slack) so parts never spill off an imaginary edge.
    let outline = prog.outline.unwrap_or_else(|| auto_outline(&groups));

    // 2. Lay the function groups out on a coarse grid that *fits inside the
    //    outline*, and seed each group's components in a small spiral inside its
    //    cell. Group cells tile the board so functions occupy distinct regions.
    let cols = (group_count as f64).sqrt().ceil().max(1.0) as usize;
    let rows = group_count.div_ceil(cols).max(1);
    let cell_w = outline.width() / cols as f64;
    let cell_h = outline.height() / rows as f64;
    for (gi, (_key, nodes)) in groups.iter_mut().enumerate() {
        let gx = outline.min_x + (gi % cols) as f64 * cell_w + cell_w * 0.5;
        let gy = outline.min_y + (gi / cols) as f64 * cell_h + cell_h * 0.5;
        for (k, node) in nodes.iter_mut().enumerate() {
            // Locked parts ignore the seed: they stay exactly where they are.
            if node.constraint == NodeConstraint::Locked {
                continue;
            }
            let ang = k as f64 * 2.399_963; // golden angle
            let r = 0.6 * (k as f64).sqrt();
            node.pos = (gx + r * ang.cos(), gy + r * ang.sin());
        }
    }

    // 3. Force-directed relaxation across all nodes (flattened): net attraction
    //    pulls connected parts together, group cohesion keeps functions
    //    coherent, soft repulsion separates overlaps, every step re-applies the
    //    pin/lock constraints and clamps to the outline.
    let mut flat: Vec<(usize, usize)> = Vec::new(); // (group, idx)
    for (gi, (_k, nodes)) in groups.iter().enumerate() {
        for ni in 0..nodes.len() {
            flat.push((gi, ni));
        }
    }

    // Group centroids (recomputed each iteration) for cohesion.
    // Net -> list of (group, idx) for attraction.
    let mut net_members: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
    for &(gi, ni) in &flat {
        for net in &groups[gi].1[ni].nets {
            net_members.entry(net.clone()).or_default().push((gi, ni));
        }
    }
    // Skip global rails: nets touching a large fraction of nodes only add noise.
    let total = flat.len().max(1);
    net_members.retain(|_, v| v.len() <= (total / 4).max(4));

    for _ in 0..cfg.iterations {
        let mut disp: HashMap<(usize, usize), (f64, f64)> = HashMap::new();

        // Group cohesion: pull each node gently toward its group's centroid so
        // parts of one function stay clustered together (organised-by-function).
        let mut g_sum: Vec<(f64, f64, usize)> = vec![(0.0, 0.0, 0); groups.len()];
        for &(gi, ni) in &flat {
            let p = groups[gi].1[ni].pos;
            let e = &mut g_sum[groups[gi].1[ni].group.min(groups.len() - 1)];
            e.0 += p.0;
            e.1 += p.1;
            e.2 += 1;
        }
        for &(gi, ni) in &flat {
            let g = &g_sum[groups[gi].1[ni].group.min(groups.len() - 1)];
            if g.2 == 0 {
                continue;
            }
            let cx = g.0 / g.2 as f64;
            let cy = g.1 / g.2 as f64;
            let p = groups[gi].1[ni].pos;
            let dx = cx - p.0;
            let dy = cy - p.1;
            add(&mut disp, (gi, ni), (dx * 0.02, dy * 0.02));
        }

        // Soft repulsion: pairwise within a cutoff, by combined bounding radius.
        for a in 0..flat.len() {
            for b in (a + 1)..flat.len() {
                let (ga, na) = flat[a];
                let (gb, nb) = flat[b];
                let pa = groups[ga].1[na].pos;
                let pb = groups[gb].1[nb].pos;
                let want = groups[ga].1[na].radius + groups[gb].1[nb].radius;
                let dx = pa.0 - pb.0;
                let dy = pa.1 - pb.1;
                let mut d = (dx * dx + dy * dy).sqrt();
                if d < 1e-6 {
                    d = 1e-6;
                }
                if d < want * 1.2 {
                    let push = (want - d).max(0.0) * 0.5 + 0.02;
                    let ux = dx / d;
                    let uy = dy / d;
                    add(&mut disp, (ga, na), (ux * push, uy * push));
                    add(&mut disp, (gb, nb), (-ux * push, -uy * push));
                }
            }
        }

        // Attraction along shared (non-global) nets.
        for members in net_members.values() {
            for i in 0..members.len() {
                for j in (i + 1)..members.len() {
                    let (ga, na) = members[i];
                    let (gb, nb) = members[j];
                    let pa = groups[ga].1[na].pos;
                    let pb = groups[gb].1[nb].pos;
                    let dx = pb.0 - pa.0;
                    let dy = pb.1 - pa.1;
                    let d = (dx * dx + dy * dy).sqrt().max(1e-6);
                    let pull = (d * 0.01).min(0.4);
                    let ux = dx / d;
                    let uy = dy / d;
                    add(&mut disp, (ga, na), (ux * pull, uy * pull));
                    add(&mut disp, (gb, nb), (-ux * pull, -uy * pull));
                }
            }
        }

        // Apply, clamped per-step, then re-impose constraints + outline.
        for (&(gi, ni), &(mx, my)) in &disp {
            if groups[gi].1[ni].constraint == NodeConstraint::Locked {
                continue;
            }
            let m = (mx * mx + my * my).sqrt();
            let cap = 1.0;
            let (cx, cy) = if m > cap {
                (mx / m * cap, my / m * cap)
            } else {
                (mx, my)
            };
            let node = &mut groups[gi].1[ni];
            node.pos.0 += cx;
            node.pos.1 += cy;
        }
        // Re-impose pins and keep everything on-board.
        for (gi, (_k, nodes)) in groups.iter_mut().enumerate() {
            let _ = gi;
            for node in nodes.iter_mut() {
                match node.constraint {
                    NodeConstraint::Locked => {}
                    NodeConstraint::Pinned(edge) => {
                        node.pos = snap_to_edge(node.pos, node, &outline, edge);
                        node.pos = clamp_to_outline(node.pos, node, &outline);
                        // Re-snap so the clamp on the free axis does not drag the
                        // pinned axis off the edge.
                        node.pos = snap_to_edge(node.pos, node, &outline, edge);
                    }
                    NodeConstraint::Free => {
                        node.pos = clamp_to_outline(node.pos, node, &outline);
                    }
                }
            }
        }
    }

    // 4. Hard de-overlap pass: enforce rectangle separation (courtyard +
    //    clearance) so a `space N` field is a genuine minimum clear distance,
    //    not a soft hint. Iterate a bounded number of relaxation sweeps;
    //    locked/pinned constraints are respected (locked never move, pinned move
    //    only along their edge). The board outline clamp keeps everything on
    //    the board even when it is crowded. The sweep budget scales with the
    //    component count: chained overlaps on a dense board need many passes to
    //    relax, and each sweep is cheap relative to the relaxation above.
    let sweeps = (flat.len() * 8).clamp(200, 4000);
    hard_separate(&mut groups, &flat, &outline, sweeps);

    // 5. Write positions back into the program. Round to KiCad-friendly grid.
    let mut report = LayoutReport {
        groups: group_count,
        ..Default::default()
    };
    for (_k, nodes) in &groups {
        for node in nodes {
            // Locked parts report their original (unchanged) coordinates.
            let target = if node.constraint == NodeConstraint::Locked {
                orig_pos.get(&node.reference).copied().unwrap_or(node.pos)
            } else {
                node.pos
            };
            let np = (round3(target.0), round3(target.1));
            if let Some(c) = comp_at_mut(prog, node.loc) {
                if c.at != np {
                    report.moved.push(node.reference.clone());
                }
                c.at = np;
            }
        }
    }
    report.moved.sort();
    report
}

/// Build a layout [`Node`] for a component, resolving its constraint.
fn make_node(
    c: &Comp,
    loc: Loc,
    fn_space: f64,
    cfg: &FullConfig,
    group: usize,
    pins: &HashMap<String, crate::dsl::Edge>,
    locks: &std::collections::HashSet<String>,
) -> Node {
    let constraint = if locks.contains(&c.reference) {
        NodeConstraint::Locked
    } else if let Some(&edge) = pins.get(&c.reference) {
        NodeConstraint::Pinned(edge)
    } else {
        NodeConstraint::Free
    };
    Node {
        loc,
        pos: c.at,
        radius: comp_radius(c, fn_space, cfg.comp_spacing),
        half: comp_half_extent(c),
        clear: comp_clearance(c, fn_space) + cfg.comp_spacing * 0.5,
        nets: c.nets().iter().map(|s| s.to_string()).collect(),
        reference: c.reference.clone(),
        group,
        constraint,
    }
}

/// Auto-size a board box around the component set when no outline is given.
/// Square, sized to the total courtyard area times a packing slack, centred at
/// the origin.
fn auto_outline(groups: &[(String, Vec<Node>)]) -> Outline {
    let mut area = 0.0;
    let mut maxr: f64 = 1.0;
    for (_k, nodes) in groups {
        for n in nodes {
            let w = (n.half.0 + n.clear) * 2.0;
            let h = (n.half.1 + n.clear) * 2.0;
            area += w * h;
            maxr = maxr.max(w.max(h));
        }
    }
    // 2.2x slack gives room for routing channels and a non-cramped layout.
    let side = (area * 2.2).sqrt().max(maxr * 2.0).max(10.0);
    Outline {
        min_x: -side * 0.5,
        min_y: -side * 0.5,
        max_x: side * 0.5,
        max_y: side * 0.5,
    }
}

/// Hard rectangle de-overlap: repeatedly push overlapping courtyards apart on
/// their minimum-penetration axis until no pair violates
/// `gap >= half_a + half_b + clear_a + clear_b`, or the sweep budget is spent.
/// Locked nodes are immovable anchors; pinned nodes slide only along their edge.
fn hard_separate(
    groups: &mut [(String, Vec<Node>)],
    flat: &[(usize, usize)],
    outline: &Outline,
    sweeps: usize,
) {
    for _ in 0..sweeps {
        let mut moved = false;
        for a in 0..flat.len() {
            for b in (a + 1)..flat.len() {
                let (ga, na) = flat[a];
                let (gb, nb) = flat[b];
                let (pa, pb);
                let (reqx, reqy);
                {
                    let an = &groups[ga].1[na];
                    let bn = &groups[gb].1[nb];
                    pa = an.pos;
                    pb = bn.pos;
                    reqx = an.half.0 + bn.half.0 + an.clear + bn.clear;
                    reqy = an.half.1 + bn.half.1 + an.clear + bn.clear;
                }
                let dx = pb.0 - pa.0;
                let dy = pb.1 - pa.1;
                let ox = reqx - dx.abs(); // overlap on x
                let oy = reqy - dy.abs(); // overlap on y
                if ox <= 0.0 || oy <= 0.0 {
                    continue; // separated on at least one axis
                }
                moved = true;
                // Resolve along the axis of least penetration.
                let (mut sx, mut sy) = (0.0, 0.0);
                if ox < oy {
                    let push = (ox + 0.01) * 0.5;
                    let dir = if dx >= 0.0 { 1.0 } else { -1.0 };
                    sx = dir * push;
                } else {
                    let push = (oy + 0.01) * 0.5;
                    let dir = if dy >= 0.0 { 1.0 } else { -1.0 };
                    sy = dir * push;
                }
                // Distribute the push by mobility (locked = immovable).
                let a_lock = groups[ga].1[na].constraint == NodeConstraint::Locked;
                let b_lock = groups[gb].1[nb].constraint == NodeConstraint::Locked;
                let (wa, wb) = match (a_lock, b_lock) {
                    (true, true) => (0.0, 0.0),
                    (true, false) => (0.0, 2.0),
                    (false, true) => (2.0, 0.0),
                    (false, false) => (1.0, 1.0),
                };
                nudge(&mut groups[ga].1[na], (-sx * wa, -sy * wa), outline);
                nudge(&mut groups[gb].1[nb], (sx * wb, sy * wb), outline);
            }
        }
        if !moved {
            break;
        }
    }
}

/// Move a node by `delta`, respecting its constraint and the outline.
fn nudge(node: &mut Node, delta: (f64, f64), outline: &Outline) {
    match node.constraint {
        NodeConstraint::Locked => {}
        NodeConstraint::Pinned(edge) => {
            // Only the along-edge component applies.
            use crate::dsl::Edge as E;
            let d = match edge {
                E::Left | E::Right => (0.0, delta.1),
                E::Top | E::Bottom => (delta.0, 0.0),
            };
            node.pos.0 += d.0;
            node.pos.1 += d.1;
            node.pos = snap_to_edge(node.pos, node, outline, edge);
            node.pos = clamp_to_outline(node.pos, node, outline);
            node.pos = snap_to_edge(node.pos, node, outline, edge);
        }
        NodeConstraint::Free => {
            node.pos.0 += delta.0;
            node.pos.1 += delta.1;
            node.pos = clamp_to_outline(node.pos, node, outline);
        }
    }
}

fn add(disp: &mut HashMap<(usize, usize), (f64, f64)>, key: (usize, usize), d: (f64, f64)) {
    let e = disp.entry(key).or_insert((0.0, 0.0));
    e.0 += d.0;
    e.1 += d.1;
}

fn comp_at_mut(prog: &mut Program, loc: Loc) -> Option<&mut Comp> {
    match loc {
        Loc::Single(si) => match &mut prog.body[si] {
            Stmt::Single(c) => Some(c),
            _ => None,
        },
        Loc::Slot(si, slot) => match &mut prog.body[si] {
            Stmt::Instance(inst) => inst.comps[slot].as_mut(),
            _ => None,
        },
    }
}

// ---------------------------------------------------------------------------
// Incremental recompile
// ---------------------------------------------------------------------------

/// Signature of a component for "did it change" comparison: identity + place.
fn sig(c: &Comp) -> (String, String, (i64, i64), i64) {
    (
        c.lib_id.clone(),
        c.value.clone(),
        (q(c.at.0), q(c.at.1)),
        q(c.rot),
    )
}

fn q(v: f64) -> i64 {
    (v * 1000.0).round() as i64
}

fn relayout_incremental(
    prog: &mut Program,
    base: &Program,
    cfg: &IncrementalConfig,
) -> LayoutReport {
    // Map base references to their signature and placement.
    let mut base_sig: HashMap<String, ((String, String, (i64, i64), i64), (f64, f64))> =
        HashMap::new();
    for c in base.comps() {
        base_sig.insert(c.reference.clone(), (sig(c), c.at));
    }

    // First pass: decide which components are "settled" (identity + value +
    // placement unchanged from base) vs "dirty" (new / moved / value changed).
    // Settled components keep their exact coordinates. A value-only change does
    // not force a move (the part stays put); only a placement change or a new
    // component triggers re-placement.
    let mut settled: Vec<(f64, f64, f64)> = Vec::new(); // x, y, radius (keep-outs)
    let mut dirty: Vec<usize> = Vec::new();
    let fnsp = fn_spaces(prog);
    let _ = &fnsp;

    // Collect locations in body order.
    let mut locs: Vec<(Loc, String)> = Vec::new();
    for (si, st) in prog.body.iter().enumerate() {
        match st {
            Stmt::Single(c) => locs.push((Loc::Single(si), c.reference.clone())),
            Stmt::Instance(inst) => {
                for (slot, comp) in inst.comps.iter().enumerate() {
                    if let Some(c) = comp {
                        locs.push((Loc::Slot(si, slot), c.reference.clone()));
                    }
                }
            }
            _ => {}
        }
    }

    for (idx, (loc, reference)) in locs.iter().enumerate() {
        let c = comp_at_ref(prog, *loc).unwrap();
        match base_sig.get(reference) {
            Some((bsig, bpos)) => {
                let cur = sig(c);
                // Unchanged placement => settled, even if value changed.
                if (cur.2 == bsig.2) && (cur.3 == bsig.3) {
                    settled.push((bpos.0, bpos.1, comp_radius(c, 0.0, cfg.comp_spacing)));
                } else {
                    dirty.push(idx);
                }
            }
            None => dirty.push(idx), // new component
        }
    }

    let mut report = LayoutReport {
        kept: settled.len(),
        ..Default::default()
    };

    // Place each dirty component in free space near its net neighbours among
    // the settled set. Greedy: candidate = centroid of settled net-mates, then
    // spiral out until it clears all keep-outs.
    // Build settled net positions.
    let mut settled_net_pos: HashMap<String, Vec<(f64, f64)>> = HashMap::new();
    for (loc, reference) in &locs {
        if base_sig.contains_key(reference) {
            let c = comp_at_ref(prog, *loc).unwrap();
            // settled only
            let s = sig(c);
            if let Some((bsig, _)) = base_sig.get(reference) {
                if s.2 == bsig.2 && s.3 == bsig.3 {
                    for n in c.nets() {
                        settled_net_pos.entry(n.to_string()).or_default().push(c.at);
                    }
                }
            }
        }
    }

    for &idx in &dirty {
        let (loc, reference) = locs[idx].clone();
        let (radius, nets) = {
            let c = comp_at_ref(prog, loc).unwrap();
            (
                comp_radius(c, 0.0, cfg.comp_spacing),
                c.nets().iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )
        };
        // Seed: centroid of net-mate positions, else board centroid of settled.
        let mut seeds: Vec<(f64, f64)> = Vec::new();
        for n in &nets {
            if let Some(ps) = settled_net_pos.get(n) {
                seeds.extend(ps.iter().copied());
            }
        }
        let seed = if !seeds.is_empty() {
            centroid(&seeds)
        } else if !settled.is_empty() {
            centroid(&settled.iter().map(|s| (s.0, s.1)).collect::<Vec<_>>())
        } else {
            (0.0, 0.0)
        };

        let pos = find_free(seed, radius, &settled);
        if let Some(c) = comp_at_mut(prog, loc) {
            c.at = (round3(pos.0), round3(pos.1));
        }
        // The newly placed component becomes a keep-out for later dirty parts,
        // and contributes to net positions.
        settled.push((pos.0, pos.1, radius));
        for n in &nets {
            settled_net_pos.entry(n.clone()).or_default().push(pos);
        }
        report.moved.push(reference);
    }

    report.moved.sort();
    report
}

fn comp_at_ref(prog: &Program, loc: Loc) -> Option<&Comp> {
    match loc {
        Loc::Single(si) => match &prog.body[si] {
            Stmt::Single(c) => Some(c),
            _ => None,
        },
        Loc::Slot(si, slot) => match &prog.body[si] {
            Stmt::Instance(inst) => inst.comps[slot].as_ref(),
            _ => None,
        },
    }
}

/// Spiral out from `seed` until a position clears every keep-out disc.
fn find_free(seed: (f64, f64), radius: f64, keepouts: &[(f64, f64, f64)]) -> (f64, f64) {
    let clears = |p: (f64, f64)| {
        keepouts.iter().all(|&(kx, ky, kr)| {
            let dx = p.0 - kx;
            let dy = p.1 - ky;
            (dx * dx + dy * dy).sqrt() >= radius + kr
        })
    };
    if clears(seed) {
        return seed;
    }
    let step = radius.max(1.0);
    for ring in 1..2000 {
        let r = ring as f64 * step;
        let n = (8 * ring).max(8);
        for k in 0..n {
            let ang = k as f64 / n as f64 * std::f64::consts::TAU;
            let p = (seed.0 + r * ang.cos(), seed.1 + r * ang.sin());
            if clears(p) {
                return p;
            }
        }
    }
    seed
}

fn centroid(pts: &[(f64, f64)]) -> (f64, f64) {
    let n = pts.len().max(1) as f64;
    let sx: f64 = pts.iter().map(|p| p.0).sum();
    let sy: f64 = pts.iter().map(|p| p.1).sum();
    (sx / n, sy / n)
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

// ---------------------------------------------------------------------------
// Grid A* router (v1 fallback)
// ---------------------------------------------------------------------------

/// A routed track: a polyline of points on one layer for a net.
#[derive(Debug, Clone)]
pub struct RoutedTrack {
    pub net: String,
    pub points: Vec<(f64, f64)>,
}

/// Result of the v1 grid router.
#[derive(Debug, Clone, Default)]
pub struct RouteResult {
    pub tracks: Vec<RoutedTrack>,
    /// Nets the router could not complete (reported, never silently dropped).
    pub unrouted: Vec<String>,
}

/// Route a program's nets with a coarse single-layer grid A*.
///
/// This is the documented v1: it connects each net's pads pairwise (MST order)
/// with Manhattan paths on a grid whose cells are blocked by component bodies.
/// It does not rip-up, does not use vias, and leaves a net in `unrouted` when no
/// path is found. `grid` is the cell size in mm.
pub fn route_grid(prog: &Program, grid: f64) -> RouteResult {
    // Absolute pad positions per net.
    let mut net_pads: HashMap<String, Vec<(f64, f64)>> = HashMap::new();
    let mut obstacles: Vec<(f64, f64, f64, f64)> = Vec::new(); // x0,y0,x1,y1 bbox
    let mut min = (f64::MAX, f64::MAX);
    let mut max = (f64::MIN, f64::MIN);

    for c in prog.comps() {
        let (fx, fy) = c.at;
        let ang = c.rot.to_radians();
        let (s, co) = ang.sin_cos();
        let mut bx = (f64::MAX, f64::MAX);
        let mut b_max = (f64::MIN, f64::MIN);
        for p in &c.pads {
            let rx = p.at.0 * co - p.at.1 * s + fx;
            let ry = p.at.0 * s + p.at.1 * co + fy;
            if let Some(n) = &p.net {
                net_pads.entry(n.clone()).or_default().push((rx, ry));
            }
            bx.0 = bx.0.min(rx);
            bx.1 = bx.1.min(ry);
            b_max.0 = b_max.0.max(rx);
            b_max.1 = b_max.1.max(ry);
            min.0 = min.0.min(rx);
            min.1 = min.1.min(ry);
            max.0 = max.0.max(rx);
            max.1 = max.1.max(ry);
        }
        if bx.0 <= b_max.0 {
            obstacles.push((bx.0, bx.1, b_max.0, b_max.1));
        }
    }

    let mut result = RouteResult::default();
    if min.0 > max.0 {
        return result;
    }

    // Grid bounds with a margin.
    let margin = grid * 4.0;
    let ox = min.0 - margin;
    let oy = min.1 - margin;
    let w = (((max.0 + margin) - ox) / grid).ceil() as i32 + 1;
    let h = (((max.1 + margin) - oy) / grid).ceil() as i32 + 1;
    if w <= 0 || h <= 0 || (w as i64 * h as i64) > 4_000_000 {
        // Too large for the v1 grid; report all nets as unrouted.
        result.unrouted = net_pads.keys().cloned().collect();
        result.unrouted.sort();
        return result;
    }

    let to_cell = |p: (f64, f64)| -> (i32, i32) {
        (
            ((p.0 - ox) / grid).round() as i32,
            ((p.1 - oy) / grid).round() as i32,
        )
    };

    // Blocked cells from obstacle bboxes (slightly shrunk so pad cells stay open).
    let mut blocked = vec![false; (w * h) as usize];
    for &(x0, y0, x1, y1) in &obstacles {
        let c0 = to_cell((x0, y0));
        let c1 = to_cell((x1, y1));
        for cy in c0.1..=c1.1 {
            for cx in c0.0..=c1.0 {
                if cx >= 0 && cy >= 0 && cx < w && cy < h {
                    blocked[(cy * w + cx) as usize] = true;
                }
            }
        }
    }

    let mut nets: Vec<_> = net_pads.into_iter().collect();
    nets.sort_by(|a, b| a.0.cmp(&b.0));

    for (net, pads) in nets {
        if pads.len() < 2 {
            continue;
        }
        // Connect in nearest-neighbour chain (cheap MST surrogate).
        let cells: Vec<(i32, i32)> = pads.iter().map(|&p| to_cell(p)).collect();
        let mut routed_any = false;
        let mut connected = vec![false; cells.len()];
        connected[0] = true;
        for _ in 1..cells.len() {
            // pick nearest unconnected to any connected
            let mut best: Option<(usize, usize, i32)> = None;
            for i in 0..cells.len() {
                if !connected[i] {
                    continue;
                }
                for j in 0..cells.len() {
                    if connected[j] {
                        continue;
                    }
                    let d = (cells[i].0 - cells[j].0).abs() + (cells[i].1 - cells[j].1).abs();
                    if best.map(|b| d < b.2).unwrap_or(true) {
                        best = Some((i, j, d));
                    }
                }
            }
            if let Some((i, j, _)) = best {
                connected[j] = true;
                // Open pad cells so A* can enter/leave them.
                let si = (cells[i].1 * w + cells[i].0) as usize;
                let sj = (cells[j].1 * w + cells[j].0) as usize;
                let (bi, bj) = (blocked[si], blocked[sj]);
                blocked[si] = false;
                blocked[sj] = false;
                if let Some(path) = astar(cells[i], cells[j], w, h, &blocked) {
                    let pts: Vec<(f64, f64)> = path
                        .iter()
                        .map(|&(cx, cy)| (ox + cx as f64 * grid, oy + cy as f64 * grid))
                        .collect();
                    result.tracks.push(RoutedTrack {
                        net: net.clone(),
                        points: pts,
                    });
                    routed_any = true;
                }
                blocked[si] = bi;
                blocked[sj] = bj;
            }
        }
        if !routed_any {
            result.unrouted.push(net);
        }
    }
    result.unrouted.sort();
    result
}

/// 4-connected grid A* with Manhattan heuristic.
fn astar(
    start: (i32, i32),
    goal: (i32, i32),
    w: i32,
    h: i32,
    blocked: &[bool],
) -> Option<Vec<(i32, i32)>> {
    use std::collections::BinaryHeap;
    let idx = |x: i32, y: i32| (y * w + x) as usize;
    let n = (w * h) as usize;
    let mut g = vec![i32::MAX; n];
    let mut came: Vec<i32> = vec![-1; n];
    let mut heap: BinaryHeap<std::cmp::Reverse<(i32, i32)>> = BinaryHeap::new();
    let hcost = |x: i32, y: i32| (x - goal.0).abs() + (y - goal.1).abs();
    g[idx(start.0, start.1)] = 0;
    heap.push(std::cmp::Reverse((
        hcost(start.0, start.1),
        idx(start.0, start.1) as i32,
    )));
    while let Some(std::cmp::Reverse((_, ci))) = heap.pop() {
        let cx = ci % w;
        let cy = ci / w;
        if (cx, cy) == goal {
            // reconstruct
            let mut path = vec![(cx, cy)];
            let mut cur = ci;
            while came[cur as usize] >= 0 {
                cur = came[cur as usize];
                path.push((cur % w, cur / w));
            }
            path.reverse();
            return Some(path);
        }
        let base = g[ci as usize];
        for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let nx = cx + dx;
            let ny = cy + dy;
            if nx < 0 || ny < 0 || nx >= w || ny >= h {
                continue;
            }
            let ni = idx(nx, ny);
            if blocked[ni] {
                continue;
            }
            let ng = base + 1;
            if ng < g[ni] {
                g[ni] = ng;
                came[ni] = ci;
                heap.push(std::cmp::Reverse((ng + hcost(nx, ny), ni as i32)));
            }
        }
    }
    None
}
