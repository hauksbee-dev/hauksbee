//! Partition a netlist into functional blocks.
//!
//! A *block* is a connected component of footprints once "global" nets
//! (power/ground rails and anything fanning out across the board) are removed.
//! Without that removal a single GND net would merge the entire board into one
//! blob, which is useless for repeat detection.

use crate::netlist::{NetId, Netlist};
use std::collections::HashSet;

/// Tunable knobs for block partitioning.
#[derive(Debug, Clone)]
pub struct PartitionConfig {
    /// A net touching more than this fraction of all components is treated as
    /// global (a power/ground rail) and dropped from the connectivity graph.
    pub global_net_fraction: f64,
    /// Absolute cap: a net touching at least this many components is global
    /// regardless of fraction (matters on huge boards where 5% is still huge).
    pub global_net_min_abs: usize,
    /// Minimum fanout before fanout-based globality even applies. A net wiring
    /// only a handful of components is structural local connectivity, never a
    /// rail, no matter how small the board. Protects tiny boards where 5% rounds
    /// down to 2.
    pub global_net_fanout_floor: usize,
    /// Extra net-name substrings (uppercased) that always count as global.
    /// Sensible defaults are added on top of these.
    pub extra_global_names: Vec<String>,
    /// A block larger than this is assumed to be several functional blocks
    /// daisy-chained through shared signal/bus nets rather than one real block.
    /// Such blocks are recursively split by cutting their geometrically long
    /// (bridging) nets (see [`refine_block`]). Set to `usize::MAX` to disable.
    pub max_block_size: usize,
    /// Absolute floor (mm) for the bridging-net cut: a net spanning at least
    /// this far is always considered a bridge during refinement, regardless of
    /// the block's median net span. Stops a tight block from being shredded.
    pub span_cut_floor_mm: f64,
}

impl Default for PartitionConfig {
    fn default() -> Self {
        PartitionConfig {
            // Aggressive global removal: on a densely-interconnected board the
            // 5% fraction is far too permissive, so the absolute cap dominates.
            // A net touching more than a handful of components is a shared
            // signal/bus that should not fuse distinct functional blocks.
            global_net_fraction: 0.05,
            global_net_min_abs: 3,
            global_net_fanout_floor: 3,
            extra_global_names: Vec::new(),
            // Functional blocks on real boards are small (a few to a couple
            // dozen parts). Anything bigger is a chained backbone to be split.
            max_block_size: 24,
            // Real intra-block nets span single-digit mm; bridging nets span
            // tens of mm. 6mm cleanly separates the two on dense SMD boards.
            span_cut_floor_mm: 6.0,
        }
    }
}

/// Default substrings that mark a net as a power/ground rail. Matched
/// case-insensitively against the full net name.
const DEFAULT_GLOBAL_SUBSTRINGS: &[&str] = &[
    "GND", "GROUND", "VCC", "VDD", "VSS", "VBUS", "VIN", "VOUT", "VREF",
    "+5V", "+3V3", "+3.3V", "+1V8", "+12V", "-12V", "+1V2", "+2V5", "+15V",
    "-15V", "VEE", "AVDD", "AVSS", "DVDD", "DGND", "AGND", "PGND", "VBAT",
    "POWER",
];

/// A connected block of components, identified by their indices into the
/// netlist's `comps`.
#[derive(Debug, Clone)]
pub struct Block {
    pub comp_indices: Vec<usize>,
}

/// Result of partitioning, including which nets were judged global (for
/// reporting / debugging).
#[derive(Debug, Clone)]
pub struct Partition {
    pub blocks: Vec<Block>,
    pub global_nets: HashSet<NetId>,
}

/// Decide which nets are "global" and should not connect blocks.
pub fn global_nets(nl: &Netlist, cfg: &PartitionConfig) -> HashSet<NetId> {
    let fanout = nl.net_fanout();
    let total = nl.comps.len().max(1);
    let frac_threshold = (cfg.global_net_fraction * total as f64).ceil() as usize;
    // The effective component-count threshold is whichever is *smaller* between
    // the fraction-derived count and the absolute cap (either condition makes a
    // net global), but never below the fanout floor: a net touching only a
    // handful of components is local structure, not a rail.
    let threshold = frac_threshold
        .min(cfg.global_net_min_abs)
        .max(cfg.global_net_fanout_floor);

    let mut global = HashSet::new();
    for (id, name) in nl.net_names.iter().enumerate() {
        let id = id as NetId;
        if fanout[id as usize] >= threshold {
            global.insert(id);
            continue;
        }
        if is_named_global(name, cfg) {
            global.insert(id);
        }
    }
    global
}

fn is_named_global(name: &str, cfg: &PartitionConfig) -> bool {
    let up = name.to_uppercase();
    for sub in DEFAULT_GLOBAL_SUBSTRINGS {
        if up.contains(sub) {
            return true;
        }
    }
    for sub in &cfg.extra_global_names {
        if up.contains(&sub.to_uppercase()) {
            return true;
        }
    }
    false
}

/// Partition the netlist into connected blocks.
pub fn partition(nl: &Netlist, cfg: &PartitionConfig) -> Partition {
    let global = global_nets(nl, cfg);

    // Initial connected components over all comps with the board-wide global
    // nets cut.
    let all: Vec<usize> = (0..nl.comps.len()).collect();
    let initial = connected_components(nl, &all, &global);

    // Recursively refine any block that is too large to be one functional unit.
    let mut blocks: Vec<Block> = Vec::new();
    for comps in initial {
        refine_block(nl, comps, &global, cfg, &mut blocks);
    }

    // Deterministic ordering: by first component index.
    blocks.sort_by_key(|b| b.comp_indices[0]);

    Partition {
        blocks,
        global_nets: global,
    }
}

/// Connected components of `comps` under the comp-net graph, treating any net in
/// `cut` as absent. Returns each component's sorted comp-index list.
fn connected_components(
    nl: &Netlist,
    comps: &[usize],
    cut: &HashSet<NetId>,
) -> Vec<Vec<usize>> {
    // Local dense indexing.
    let local: std::collections::HashMap<usize, usize> =
        comps.iter().enumerate().map(|(i, &c)| (c, i)).collect();

    // net -> local comp indices (within this subset).
    let mut net_to_comps: std::collections::HashMap<NetId, Vec<usize>> =
        std::collections::HashMap::new();
    for &ci in comps {
        let li = local[&ci];
        let mut nets: Vec<NetId> = nl.comps[ci]
            .pads
            .iter()
            .filter_map(|p| p.net)
            .filter(|n| !cut.contains(n))
            .collect();
        nets.sort_unstable();
        nets.dedup();
        for net in nets {
            net_to_comps.entry(net).or_default().push(li);
        }
    }

    let mut uf = UnionFind::new(comps.len());
    for locals in net_to_comps.values() {
        if locals.len() < 2 {
            continue;
        }
        let first = locals[0];
        for &other in &locals[1..] {
            uf.union(first, other);
        }
    }

    let mut groups: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    for (li, &ci) in comps.iter().enumerate() {
        groups.entry(uf.find(li)).or_default().push(ci);
    }
    let mut out: Vec<Vec<usize>> = groups
        .into_values()
        .map(|mut v| {
            v.sort_unstable();
            v
        })
        .collect();
    out.sort_by_key(|v| v[0]);
    out
}

/// Recursively split an oversized block by cutting its *geometrically long*
/// nets.
///
/// A block above `cfg.max_block_size` is several functional blocks daisy-chained
/// through shared routing/signal nets. On this kind of board the discriminator
/// is not fanout (a synapse's internal junction and a bridging net can both be
/// 2-pin) but **geometry**: a real functional block's internal nets are
/// physically compact (sub-millimetre to a few mm, pads of adjacent parts),
/// whereas a net that bridges two distant blocks spans tens of millimetres. We
/// therefore cut every internal net whose pad bounding-box diagonal exceeds
/// `span_threshold`, then take connected components of what remains and recurse.
///
/// The threshold adapts: we take the median internal-net span as the compact
/// baseline and cut everything well above it, but never below an absolute floor
/// so a block of slightly-spread-but-genuine parts isn't shredded. If no net is
/// long enough to cut, the block is emitted as-is (a genuinely large unit).
fn refine_block(
    nl: &Netlist,
    comps: Vec<usize>,
    base_cut: &HashSet<NetId>,
    cfg: &PartitionConfig,
    out: &mut Vec<Block>,
) {
    if comps.len() <= cfg.max_block_size {
        out.push(Block { comp_indices: comps });
        return;
    }

    // Geometric span of each internal (non-cut) net: bounding-box diagonal of
    // the positions of the components it touches within this block.
    let mut net_pts: std::collections::HashMap<NetId, Vec<(f64, f64)>> =
        std::collections::HashMap::new();
    for &ci in &comps {
        let (x, y, _) = nl.comps[ci].at;
        let mut nets: Vec<NetId> = nl.comps[ci]
            .pads
            .iter()
            .filter_map(|p| p.net)
            .filter(|n| !base_cut.contains(n))
            .collect();
        nets.sort_unstable();
        nets.dedup();
        for net in nets {
            net_pts.entry(net).or_default().push((x, y));
        }
    }

    let mut net_span: Vec<(NetId, f64)> = net_pts
        .iter()
        .filter(|(_, pts)| pts.len() >= 2)
        .map(|(&n, pts)| (n, bbox_diag(pts)))
        .collect();
    if net_span.is_empty() {
        out.push(Block { comp_indices: comps });
        return;
    }

    // Adaptive threshold: well above the median compact net, with a floor.
    let mut spans: Vec<f64> = net_span.iter().map(|(_, s)| *s).collect();
    spans.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = spans[spans.len() / 2];
    let span_threshold = (median * 1.5).max(cfg.span_cut_floor_mm);

    let cut_nets: Vec<NetId> = net_span
        .iter()
        .filter(|(_, s)| *s >= span_threshold)
        .map(|(n, _)| *n)
        .collect();

    if cut_nets.is_empty() {
        // No bridging net to cut; emit as-is.
        out.push(Block { comp_indices: comps });
        return;
    }
    net_span.clear();

    let mut cut = base_cut.clone();
    for n in cut_nets {
        cut.insert(n);
    }

    let pieces = connected_components(nl, &comps, &cut);
    // No progress: emit as-is to avoid spinning.
    if pieces.len() == 1 && pieces[0].len() == comps.len() {
        out.push(Block { comp_indices: comps });
        return;
    }

    for piece in pieces {
        refine_block(nl, piece, &cut, cfg, out);
    }
}

/// Bounding-box diagonal of a set of points (mm).
fn bbox_diag(pts: &[(f64, f64)]) -> f64 {
    let (mut minx, mut miny, mut maxx, mut maxy) =
        (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for &(x, y) in pts {
        minx = minx.min(x);
        miny = miny.min(y);
        maxx = maxx.max(x);
        maxy = maxy.max(y);
    }
    let dx = maxx - minx;
    let dy = maxy - miny;
    (dx * dx + dy * dy).sqrt()
}

/// Minimal union-find with path compression and union by size.
struct UnionFind {
    parent: Vec<usize>,
    size: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind {
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
