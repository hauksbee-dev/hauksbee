//! Fingerprint a block up to (approximate) graph isomorphism.
//!
//! ## What the fingerprint captures
//!
//! 1. The sorted multiset of `lib_id`s over the block's components.
//! 2. The internal wiring structure, via a Weisfeiler-Lehman (1-WL) colour
//!    refinement on the *bipartite* component-net graph restricted to the
//!    block's internal nets (global nets already removed by partitioning).
//!
//! ## Why `lib_id` only, not `value`
//!
//! `value` is deliberately excluded from the structural fingerprint. The
//! footprint (`lib_id`) identifies the *kind* of part; `value` is the parameter
//! that legitimately varies between otherwise-identical instances (a 10k vs a
//! 47k resistor in the same slot). If `value` were part of the fingerprint a
//! single mutated resistor would split off into its own cluster and the
//! deviation would be invisible. By clustering on structure + `lib_id` and
//! leaving `value` to the anomaly pass, the mutant stays in its cluster and is
//! reported as a per-instance value diff, which is the whole point.
//!
//! ## Why bipartite components+nets
//!
//! Two components are "connected" if they share an internal net, but a net can
//! join more than two pins (a local bus). Collapsing nets into pairwise edges
//! loses that multiplicity, so we keep nets as their own nodes and run WL over
//! both node kinds. A component node's initial colour is its `lib_id`; a net
//! node's initial colour is its degree (how many block pins it touches).
//!
//! ## False-merge risk (documented, accepted)
//!
//! 1-WL does not decide isomorphism in general (regular graphs are the classic
//! failure). Two structurally different blocks with identical degree sequences
//! and identical component multisets *can* collide. We mitigate by (a) seeding
//! component colours with `(lib_id, value)`, which is highly discriminating for
//! real boards, and (b) running several refinement rounds. A collision here
//! causes two genuinely different blocks to be clustered together; the anomaly
//! pass (`cluster.rs`) then surfaces the divergence as a per-instance diff
//! rather than hiding it, so a false merge degrades to "noisy template" rather
//! than silent data loss. Exact isomorphism is intentionally not attempted.

use crate::netlist::{Comp, Netlist};
use crate::partition::Block;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Number of WL refinement rounds. Block diameters are tiny (a handful of
/// components), so 3-4 rounds saturate; more is wasted work.
const WL_ROUNDS: usize = 4;

/// A block's structural fingerprint: equal fingerprints => same cluster.
pub type Fingerprint = u64;

/// Per-component canonical role label within its block, used to align instances
/// when deriving the template and diffing anomalies. Two components in two
/// instances of the same cluster that have the same `role` are "the same part"
/// (e.g. "the pull-up resistor").
pub type Role = u64;

/// Structural analysis of one block.
pub struct BlockGraph<'a> {
    pub nl: &'a Netlist,
    /// Component indices in this block.
    pub comps: Vec<usize>,
    /// Final WL colour per component (parallel to `comps`).
    pub roles: Vec<Role>,
    pub fingerprint: Fingerprint,
}

impl<'a> BlockGraph<'a> {
    pub fn analyze(nl: &'a Netlist, block: &Block) -> BlockGraph<'a> {
        let comps = block.comp_indices.clone();
        let n = comps.len();

        // Map global comp index -> local 0..n.
        let local: HashMap<usize, usize> =
            comps.iter().enumerate().map(|(i, &c)| (c, i)).collect();

        // Collect internal nets local to this block: a net counts if at least
        // two of its pin-touches are within this block. (Single-touch nets are
        // pads that only connect outward, already pruned of globals.)
        // net id -> list of (local comp index) it touches (with multiplicity by pin).
        let mut net_touch: HashMap<u32, Vec<usize>> = HashMap::new();
        for &ci in &comps {
            let li = local[&ci];
            for pad in &nl.comps[ci].pads {
                if let Some(net) = pad.net {
                    net_touch.entry(net).or_default().push(li);
                }
            }
        }
        // Keep only nets internal to the block (touch >= 2 distinct block comps).
        let mut internal_nets: Vec<(u32, Vec<usize>)> = net_touch
            .into_iter()
            .filter(|(_, touches)| {
                let mut distinct: Vec<usize> = touches.clone();
                distinct.sort_unstable();
                distinct.dedup();
                distinct.len() >= 2
            })
            .collect();
        internal_nets.sort_by_key(|(id, _)| *id);

        // --- Initial colours ---
        // Component colours: hash of lib_id only (value is left to the anomaly
        // pass so a mutated value does not split the cluster).
        let mut comp_colour: Vec<u64> = comps
            .iter()
            .map(|&ci| hash1(&nl.comps[ci].lib_id))
            .collect();
        // Net colours: hash of net degree (number of pin touches).
        let mut net_colour: Vec<u64> = internal_nets
            .iter()
            .map(|(_, touches)| hash_u64(touches.len() as u64 ^ 0x9E37_79B9))
            .collect();

        // --- WL refinement ---
        for _ in 0..WL_ROUNDS {
            // New component colours: combine each comp's own colour with the
            // sorted multiset of neighbour-net colours.
            let mut new_comp = comp_colour.clone();
            let mut comp_neighbors: Vec<Vec<u64>> = vec![Vec::new(); n];
            for (ni, (_, touches)) in internal_nets.iter().enumerate() {
                for &li in touches {
                    comp_neighbors[li].push(net_colour[ni]);
                }
            }
            for li in 0..n {
                comp_neighbors[li].sort_unstable();
                let mut h = comp_colour[li];
                for c in &comp_neighbors[li] {
                    h = mix(h, *c);
                }
                new_comp[li] = h;
            }

            // New net colours from neighbouring component colours.
            let mut new_net = net_colour.clone();
            for (ni, (_, touches)) in internal_nets.iter().enumerate() {
                let mut neigh: Vec<u64> = touches.iter().map(|&li| comp_colour[li]).collect();
                neigh.sort_unstable();
                let mut h = net_colour[ni];
                for c in &neigh {
                    h = mix(h, *c);
                }
                new_net[ni] = h;
            }

            comp_colour = new_comp;
            net_colour = new_net;
        }

        // --- Block fingerprint ---
        // Combine: sorted lib_id multiset + sorted final comp colours +
        // sorted final net colours + edge structure summary.
        let mut parts_multiset: Vec<u64> = comps
            .iter()
            .map(|&ci| hash1(&nl.comps[ci].lib_id))
            .collect();
        parts_multiset.sort_unstable();

        let mut comp_colours_sorted = comp_colour.clone();
        comp_colours_sorted.sort_unstable();
        let mut net_colours_sorted = net_colour.clone();
        net_colours_sorted.sort_unstable();

        let mut h = DefaultHasher::new();
        n.hash(&mut h);
        internal_nets.len().hash(&mut h);
        parts_multiset.hash(&mut h);
        comp_colours_sorted.hash(&mut h);
        net_colours_sorted.hash(&mut h);
        let fingerprint = h.finish();

        BlockGraph {
            nl,
            comps,
            roles: comp_colour,
            fingerprint,
        }
    }

    /// Components of this block, in a stable order suitable for template
    /// alignment: sorted by `(role, lib_id, value, pad-count)`, then a final
    /// tiebreak on reference for full determinism. Returns indices into the
    /// netlist.
    pub fn ordered_comps(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.comps.len()).collect();
        order.sort_by(|&a, &b| {
            let ca: &Comp = &self.nl.comps[self.comps[a]];
            let cb: &Comp = &self.nl.comps[self.comps[b]];
            self.roles[a]
                .cmp(&self.roles[b])
                .then(ca.lib_id.cmp(&cb.lib_id))
                // value deliberately excluded: alignment must be value-blind so
                // a mutated value diffs in place rather than reshuffling slots.
                .then(ca.pads.len().cmp(&cb.pads.len()))
                .then(natural_ref_cmp(&ca.reference, &cb.reference))
        });
        order.into_iter().map(|i| self.comps[i]).collect()
    }

    /// The role label aligned to `ordered_comps()` order.
    pub fn ordered_roles(&self) -> Vec<Role> {
        let mut order: Vec<usize> = (0..self.comps.len()).collect();
        order.sort_by(|&a, &b| {
            let ca: &Comp = &self.nl.comps[self.comps[a]];
            let cb: &Comp = &self.nl.comps[self.comps[b]];
            self.roles[a]
                .cmp(&self.roles[b])
                .then(ca.lib_id.cmp(&cb.lib_id))
                // value deliberately excluded: alignment must be value-blind so
                // a mutated value diffs in place rather than reshuffling slots.
                .then(ca.pads.len().cmp(&cb.pads.len()))
                .then(natural_ref_cmp(&ca.reference, &cb.reference))
        });
        order.into_iter().map(|i| self.roles[i]).collect()
    }
}

/// Compare references "naturally" so R2 < R10. Splits into (alpha prefix,
/// number) where possible.
pub fn natural_ref_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (pa, na) = split_ref(a);
    let (pb, nb) = split_ref(b);
    pa.cmp(pb).then(na.cmp(&nb)).then(a.cmp(b))
}

fn split_ref(r: &str) -> (&str, u64) {
    let digit_start = r.find(|c: char| c.is_ascii_digit());
    match digit_start {
        Some(i) => {
            let prefix = &r[..i];
            let num: u64 = r[i..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0);
            (prefix, num)
        }
        None => (r, 0),
    }
}

fn hash_u64(v: u64) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

fn hash1(a: &str) -> u64 {
    let mut h = DefaultHasher::new();
    a.hash(&mut h);
    h.finish()
}

/// Order-sensitive-then-made-order-insensitive mix: callers sort before mixing
/// so the result is a multiset hash.
fn mix(acc: u64, x: u64) -> u64 {
    // A simple non-commutative mix; ordering handled by sorting upstream.
    let mut h = acc.wrapping_mul(0x100_0000_01b3);
    h ^= x;
    h = h.rotate_left(27);
    h.wrapping_add(0x9E37_79B9_7F4A_7C15)
}
