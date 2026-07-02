//! Cluster blocks by fingerprint, derive templates, and compute anomalies.
//!
//! After partitioning + fingerprinting we have a set of blocks each labelled
//! with a [`Fingerprint`]. Blocks sharing a fingerprint form a *cluster*. For a
//! cluster of size >= 2 we:
//!
//! * derive a *template*: the per-role expected `(lib_id, value)` taken by
//!   majority vote across instances;
//! * for each instance, compute a rigid placement (translation + rotation of the
//!   whole block) relative to a reference instance, when the geometry matches;
//! * diff each instance against the template, producing an [`Anomaly`] when a
//!   role's value/lib_id differs, or a component is missing/extra.

use crate::fingerprint::{natural_ref_cmp, BlockGraph, Fingerprint, Role};
use crate::netlist::Netlist;
use crate::partition::Partition;
use std::collections::HashMap;

/// A single component's expected identity within a cluster template.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateRole {
    /// Stable role key (WL colour). Used to align instances.
    pub role: Role,
    /// Majority lib_id for this role.
    pub lib_id: String,
    /// Majority value for this role.
    pub value: String,
    /// Number of pads expected (majority).
    pub pad_count: usize,
    /// A stable slot index 0..N for nicer naming/codegen.
    pub slot: usize,
}

/// One instance of a cluster: the concrete components plus its placement.
#[derive(Debug, Clone)]
pub struct Instance {
    /// Index into the partition's `blocks`.
    pub block_index: usize,
    /// Component indices (into the netlist), aligned to template role order
    /// where possible. `None` marks a template role with no matching component
    /// in this instance (a missing part).
    pub comps_by_slot: Vec<Option<usize>>,
    /// Extra components present in the instance that don't match any template
    /// role (component indices into the netlist).
    pub extra_comps: Vec<usize>,
    /// Rigid placement of the whole block relative to the reference instance:
    /// `(dx, dy, drot_degrees)`. `None` when the geometry could not be matched
    /// rigidly (instances vary geometrically).
    pub placement: Option<Placement>,
    /// Anomalies detected for this instance.
    pub anomalies: Vec<Anomaly>,
}

/// A rigid placement: rotate by `rot` degrees then translate by `(dx, dy)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub dx: f64,
    pub dy: f64,
    pub rot: f64,
}

/// A field-level deviation of an instance from its cluster template.
#[derive(Debug, Clone, PartialEq)]
pub enum Anomaly {
    /// A role's value differs from the template.
    ValueMismatch {
        slot: usize,
        reference: String,
        expected: String,
        found: String,
    },
    /// A role's lib_id differs from the template.
    LibIdMismatch {
        slot: usize,
        reference: String,
        expected: String,
        found: String,
    },
    /// A template role had no matching component in this instance.
    MissingComponent {
        slot: usize,
        expected_lib_id: String,
        expected_value: String,
    },
    /// The instance has a component beyond the template.
    ExtraComponent {
        reference: String,
        lib_id: String,
        value: String,
    },
}

/// A cluster of structurally-identical blocks.
#[derive(Debug, Clone)]
pub struct Cluster {
    pub fingerprint: Fingerprint,
    /// Heuristic human name, e.g. "block_2x_bcm857bs_sn74lvc1g3157".
    pub name: String,
    pub template: Vec<TemplateRole>,
    pub instances: Vec<Instance>,
}

impl Cluster {
    pub fn size(&self) -> usize {
        self.instances.len()
    }

    /// Total anomalies across all instances.
    pub fn anomaly_count(&self) -> usize {
        self.instances.iter().map(|i| i.anomalies.len()).sum()
    }
}

/// A complete analysis of a board.
#[derive(Debug, Clone)]
pub struct Analysis {
    /// Clusters with >= 2 instances, sorted by descending size (then name).
    pub clusters: Vec<Cluster>,
    /// Singleton blocks (their single instance), as clusters of size 1, sorted
    /// by component count descending.
    pub singletons: Vec<Cluster>,
    /// Total component count on the board.
    pub total_comps: usize,
}

impl Analysis {
    /// Fraction of components covered by multi-instance clusters (0.0..=1.0).
    pub fn cluster_coverage(&self) -> f64 {
        if self.total_comps == 0 {
            return 0.0;
        }
        let covered: usize = self
            .clusters
            .iter()
            .flat_map(|c| &c.instances)
            .map(|i| {
                i.comps_by_slot.iter().filter(|c| c.is_some()).count() + i.extra_comps.len()
            })
            .sum();
        covered as f64 / self.total_comps as f64
    }
}

/// Build the full analysis from a netlist + its partition.
pub fn analyze(nl: &Netlist, partition: &Partition) -> Analysis {
    // Fingerprint every block.
    let graphs: Vec<BlockGraph> = partition
        .blocks
        .iter()
        .map(|b| BlockGraph::analyze(nl, b))
        .collect();

    // Group block indices by fingerprint.
    let mut by_fp: HashMap<Fingerprint, Vec<usize>> = HashMap::new();
    for (bi, g) in graphs.iter().enumerate() {
        by_fp.entry(g.fingerprint).or_default().push(bi);
    }

    let mut clusters = Vec::new();
    let mut singletons = Vec::new();

    for (fp, block_indices) in by_fp {
        if block_indices.len() >= 2 {
            clusters.push(build_cluster(nl, partition, &graphs, fp, &block_indices));
        } else {
            singletons.push(build_cluster(nl, partition, &graphs, fp, &block_indices));
        }
    }

    // Deterministic ordering.
    clusters.sort_by(|a, b| {
        b.size()
            .cmp(&a.size())
            .then(b.template.len().cmp(&a.template.len()))
            .then(a.name.cmp(&b.name))
            .then(a.fingerprint.cmp(&b.fingerprint))
    });
    singletons.sort_by(|a, b| {
        b.template
            .len()
            .cmp(&a.template.len())
            .then(a.name.cmp(&b.name))
            .then(a.fingerprint.cmp(&b.fingerprint))
    });

    Analysis {
        clusters,
        singletons,
        total_comps: nl.comps.len(),
    }
}

fn build_cluster(
    nl: &Netlist,
    _partition: &Partition,
    graphs: &[BlockGraph],
    fp: Fingerprint,
    block_indices: &[usize],
) -> Cluster {
    // --- Derive the template ---
    // For each instance, get its components grouped by role. Roles that appear
    // exactly once per instance in (nearly) every instance become template
    // slots. We take the set of roles from the instance with the most
    // components as the canonical role set (a "complete" instance is the best
    // template source), then majority-vote lib_id/value per role.

    // Pick the reference instance: the one with the most components, tiebreak by
    // smallest first-component-index for determinism.
    let ref_bi = *block_indices
        .iter()
        .max_by(|&&a, &&b| {
            graphs[a]
                .comps
                .len()
                .cmp(&graphs[b].comps.len())
                .then(graphs[b].comps[0].cmp(&graphs[a].comps[0]))
        })
        .unwrap();

    // Canonical roles, in template order, from the reference instance.
    let ref_order = graphs[ref_bi].ordered_comps();
    let ref_roles = graphs[ref_bi].ordered_roles();

    // A role can repeat (e.g. two identical resistors). Build template slots:
    // one slot per component in the reference instance, but we need to be able
    // to align other instances by consuming roles greedily.
    let mut template: Vec<TemplateRole> = Vec::new();
    for (slot, (&ci, &role)) in ref_order.iter().zip(ref_roles.iter()).enumerate() {
        let c = &nl.comps[ci];
        template.push(TemplateRole {
            role,
            lib_id: c.lib_id.clone(),
            value: c.value.clone(),
            pad_count: c.pads.len(),
            slot,
        });
    }

    // Majority-vote lib_id/value per (role, occurrence) by aligning all
    // instances. We collect, per template slot, the observed values.
    let aligned: Vec<InstanceAlign> = block_indices
        .iter()
        .map(|&bi| align_instance(nl, &graphs[bi], bi, &template))
        .collect();

    majority_vote_template(nl, &mut template, &aligned);

    // --- Build instances with placement + anomalies ---
    // Reference instance placement is identity; others computed relative to it.
    let ref_align = aligned
        .iter()
        .find(|a| a.block_index == ref_bi)
        .expect("ref instance aligned");

    let mut instances: Vec<Instance> = aligned
        .iter()
        .map(|a| {
            let placement = compute_placement(nl, ref_align, a, &template);
            let anomalies = diff_instance(nl, a, &template);
            Instance {
                block_index: a.block_index,
                comps_by_slot: a.comps_by_slot.clone(),
                extra_comps: a.extra_comps.clone(),
                placement,
                anomalies,
            }
        })
        .collect();

    // Stable instance ordering: by placement (top-left first), falling back to
    // first component index.
    instances.sort_by(|a, b| {
        let ka = instance_sort_key(nl, a);
        let kb = instance_sort_key(nl, b);
        ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
    });

    let name = cluster_name(&template, fp);

    Cluster {
        fingerprint: fp,
        name,
        template,
        instances,
    }
}

/// A sort key: (dy, dx) of the first present component, so instances read
/// top-to-bottom, left-to-right.
fn instance_sort_key(nl: &Netlist, inst: &Instance) -> (f64, f64, i64) {
    let first = inst
        .comps_by_slot
        .iter()
        .flatten()
        .chain(inst.extra_comps.iter())
        .next();
    match first {
        Some(&ci) => {
            let (x, y, _) = nl.comps[ci].at;
            (y, x, ci as i64)
        }
        None => (0.0, 0.0, 0),
    }
}

/// Intermediate per-instance alignment to the template slots.
struct InstanceAlign {
    block_index: usize,
    comps_by_slot: Vec<Option<usize>>,
    extra_comps: Vec<usize>,
}

/// Align an instance's components to the template slots by greedy role matching.
fn align_instance(
    nl: &Netlist,
    graph: &BlockGraph,
    block_index: usize,
    template: &[TemplateRole],
) -> InstanceAlign {
    let order = graph.ordered_comps();
    let roles = graph.ordered_roles();

    // Bucket this instance's components by role.
    let mut by_role: HashMap<Role, Vec<usize>> = HashMap::new();
    for (&ci, &role) in order.iter().zip(roles.iter()) {
        by_role.entry(role).or_default().push(ci);
    }
    // Sort each bucket by natural reference for determinism.
    for v in by_role.values_mut() {
        v.sort_by(|&a, &b| natural_ref_cmp(&nl.comps[a].reference, &nl.comps[b].reference));
    }

    let mut comps_by_slot = vec![None; template.len()];
    let mut consumed: HashMap<Role, usize> = HashMap::new();

    for (slot, tr) in template.iter().enumerate() {
        let bucket = by_role.get(&tr.role);
        let taken = consumed.entry(tr.role).or_insert(0);
        if let Some(b) = bucket {
            if *taken < b.len() {
                comps_by_slot[slot] = Some(b[*taken]);
                *taken += 1;
            }
        }
    }

    // Extras: any component not consumed by a slot.
    let mut used: Vec<usize> = comps_by_slot.iter().flatten().copied().collect();
    used.sort_unstable();
    let extra_comps: Vec<usize> = order
        .iter()
        .copied()
        .filter(|ci| used.binary_search(ci).is_err())
        .collect();

    InstanceAlign {
        block_index,
        comps_by_slot,
        extra_comps,
    }
}

/// Majority-vote each template slot's lib_id/value/pad_count across instances.
fn majority_vote_template(
    nl: &Netlist,
    template: &mut [TemplateRole],
    aligned: &[InstanceAlign],
) {
    for (slot, tr) in template.iter_mut().enumerate() {
        let mut lib_votes: HashMap<&str, usize> = HashMap::new();
        let mut val_votes: HashMap<&str, usize> = HashMap::new();
        let mut pad_votes: HashMap<usize, usize> = HashMap::new();
        for a in aligned {
            if let Some(ci) = a.comps_by_slot[slot] {
                let c = &nl.comps[ci];
                *lib_votes.entry(c.lib_id.as_str()).or_default() += 1;
                *val_votes.entry(c.value.as_str()).or_default() += 1;
                *pad_votes.entry(c.pads.len()).or_default() += 1;
            }
        }
        if let Some(lib) = majority(&lib_votes) {
            tr.lib_id = lib.to_string();
        }
        if let Some(val) = majority(&val_votes) {
            tr.value = val.to_string();
        }
        if let Some(pc) = majority_copy(&pad_votes) {
            tr.pad_count = pc;
        }
    }
}

fn majority<'a>(votes: &HashMap<&'a str, usize>) -> Option<&'a str> {
    votes
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
        .map(|(k, _)| *k)
}

fn majority_copy(votes: &HashMap<usize, usize>) -> Option<usize> {
    votes
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
        .map(|(k, _)| *k)
}

/// Diff an aligned instance against the template, producing anomalies.
fn diff_instance(
    nl: &Netlist,
    a: &InstanceAlign,
    template: &[TemplateRole],
) -> Vec<Anomaly> {
    let mut out = Vec::new();
    for (slot, tr) in template.iter().enumerate() {
        match a.comps_by_slot[slot] {
            Some(ci) => {
                let c = &nl.comps[ci];
                if c.lib_id != tr.lib_id {
                    out.push(Anomaly::LibIdMismatch {
                        slot,
                        reference: c.reference.clone(),
                        expected: tr.lib_id.clone(),
                        found: c.lib_id.clone(),
                    });
                }
                if c.value != tr.value {
                    out.push(Anomaly::ValueMismatch {
                        slot,
                        reference: c.reference.clone(),
                        expected: tr.value.clone(),
                        found: c.value.clone(),
                    });
                }
            }
            None => {
                out.push(Anomaly::MissingComponent {
                    slot,
                    expected_lib_id: tr.lib_id.clone(),
                    expected_value: tr.value.clone(),
                });
            }
        }
    }
    for &ci in &a.extra_comps {
        let c = &nl.comps[ci];
        out.push(Anomaly::ExtraComponent {
            reference: c.reference.clone(),
            lib_id: c.lib_id.clone(),
            value: c.value.clone(),
        });
    }
    out
}

/// Compute a rigid placement of `inst` relative to `reference`.
///
/// We solve for a rotation + translation that maps the reference instance's
/// component centroid-relative positions onto this instance's. With matched
/// slots we have point correspondences; we estimate rotation from the dominant
/// pair of corresponding vectors and translation from centroids. If the fit
/// residual is large the instance's geometry is judged non-rigid and we return
/// `None`.
fn compute_placement(
    nl: &Netlist,
    reference: &InstanceAlign,
    inst: &InstanceAlign,
    template: &[TemplateRole],
) -> Option<Placement> {
    // Collect matched slot point pairs.
    let mut ref_pts = Vec::new();
    let mut ins_pts = Vec::new();
    for slot in 0..template.len() {
        if let (Some(rc), Some(ic)) = (reference.comps_by_slot[slot], inst.comps_by_slot[slot]) {
            let (rx, ry, _) = nl.comps[rc].at;
            let (ix, iy, _) = nl.comps[ic].at;
            ref_pts.push((rx, ry));
            ins_pts.push((ix, iy));
        }
    }
    if ref_pts.is_empty() {
        return None;
    }

    // Centroids.
    let rc = centroid(&ref_pts);
    let ic = centroid(&ins_pts);

    // Estimate rotation via the Kabsch-style 2D closed form:
    //   theta = atan2( sum(rx*iy - ry*ix), sum(rx*ix + ry*iy) )
    // over centroid-relative coordinates.
    let mut sxy = 0.0; // sum(r x i cross)
    let mut sxx = 0.0; // sum(r dot i)
    for (&(rx, ry), &(ix, iy)) in ref_pts.iter().zip(ins_pts.iter()) {
        let (rx, ry) = (rx - rc.0, ry - rc.1);
        let (ix, iy) = (ix - ic.0, iy - ic.1);
        sxy += rx * iy - ry * ix;
        sxx += rx * ix + ry * iy;
    }
    let theta = sxy.atan2(sxx); // radians; maps ref -> inst
    let (s, c) = theta.sin_cos();

    // Translation: ic = R*rc + t  =>  t = ic - R*rc.
    let dx = ic.0 - (c * rc.0 - s * rc.1);
    let dy = ic.1 - (s * rc.0 + c * rc.1);

    // Residual check: apply and measure max error.
    if ref_pts.len() >= 2 {
        let mut max_err: f64 = 0.0;
        for (&(rx, ry), &(ix, iy)) in ref_pts.iter().zip(ins_pts.iter()) {
            let px = c * rx - s * ry + dx;
            let py = s * rx + c * ry + dy;
            let err = ((px - ix).powi(2) + (py - iy).powi(2)).sqrt();
            max_err = max_err.max(err);
        }
        // 0.5 mm tolerance: KiCad placement is exact to microns for true copies.
        if max_err > 0.5 {
            return None;
        }
    }

    let rot_deg = theta.to_degrees();
    Some(Placement {
        dx,
        dy,
        rot: normalize_angle(rot_deg),
    })
}

fn centroid(pts: &[(f64, f64)]) -> (f64, f64) {
    let n = pts.len() as f64;
    let sx: f64 = pts.iter().map(|p| p.0).sum();
    let sy: f64 = pts.iter().map(|p| p.1).sum();
    (sx / n, sy / n)
}

fn normalize_angle(mut a: f64) -> f64 {
    while a <= -180.0 {
        a += 360.0;
    }
    while a > 180.0 {
        a -= 360.0;
    }
    // Snap near-integer angles to clean values (floating noise).
    let r = a.round();
    if (a - r).abs() < 1e-6 {
        r
    } else {
        a
    }
}

/// Build a heuristic name for a cluster from its dominant components.
///
/// Strategy: count lib_id leaf names (after the `:`), pick the up to two most
/// frequent non-passive parts (skip plain resistors/caps when richer parts
/// exist), prefix with the multiplicity of the most common part.
pub fn cluster_name(template: &[TemplateRole], fp: Fingerprint) -> String {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for tr in template {
        let leaf = lib_leaf(&tr.lib_id);
        *counts.entry(leaf).or_default() += 1;
    }
    if counts.is_empty() {
        return format!("block_{:08x}", (fp & 0xffff_ffff) as u32);
    }

    // Rank: prefer non-passive (not starting R_/C_/L_/D_/TestPoint), then by
    // count, then name.
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| {
        passive_rank(&a.0)
            .cmp(&passive_rank(&b.0))
            .then(b.1.cmp(&a.1))
            .then(a.0.cmp(&b.0))
    });

    let total: usize = template.len();
    let parts: Vec<String> = ranked
        .iter()
        .take(2)
        .map(|(name, n)| {
            if *n > 1 {
                format!("{}x_{}", n, sanitize(name))
            } else {
                sanitize(name)
            }
        })
        .collect();

    format!("block_{}c_{}", total, parts.join("_"))
}

fn passive_rank(leaf: &str) -> u8 {
    let l = leaf.to_uppercase();
    if l.starts_with("R_") || l.starts_with("C_") || l.starts_with("L_") {
        2
    } else if l.starts_with("D_") || l.contains("TESTPOINT") || l.contains("MOUNTINGHOLE") {
        1
    } else {
        0
    }
}

fn lib_leaf(lib_id: &str) -> String {
    lib_id.rsplit(':').next().unwrap_or(lib_id).to_string()
}

fn sanitize(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if c == '_' || c == '-' || c == '.' {
            out.push('_');
        }
    }
    // Collapse repeated underscores.
    let mut collapsed = String::new();
    let mut prev_us = false;
    for c in out.chars() {
        if c == '_' {
            if !prev_us {
                collapsed.push(c);
            }
            prev_us = true;
        } else {
            collapsed.push(c);
            prev_us = false;
        }
    }
    collapsed.trim_matches('_').to_string()
}
