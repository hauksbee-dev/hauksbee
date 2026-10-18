//! Copper connectivity: is every pad joined by copper to the rest of its net?
//!
//! The short sweep asks whether copper of two *different* nets touches; this
//! asks the converse question of the *same* geometry. Every pad, track, arc,
//! via and filled zone polygon becomes one item; items of one net that touch on
//! a shared copper layer are unioned, and an item that spans layers (a via, a
//! through-hole pad) joins them simply by being one item on several layers. A
//! net whose items fall into two or more islands has a pad the copper never
//! reaches: KiCad's "unconnected items".
//!
//! Unknown is not unconnected. A zone whose fill was never computed has no
//! copper to read, so a net carrying one is not judged and is named instead.

use std::collections::{HashMap, HashSet};

use forge_sexpr::List;
use rstar::{RTree, AABB};

use super::{
    collect_primitives, point_in_polygon, shape_gap_at, Capsule, ItemKind, NetResolver, Shape,
};
use hauksbee_ir::order::quantize_mm;

use crate::dsu::Dsu;
use crate::gerber::geo::BoundsLeaf;

/// Copper closer than this (mm) is one conductor; absorbs coordinate rounding.
const TOUCH_MM: f64 = 1e-3;

/// Whether an item carries current, and on which layers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Copper {
    /// No current: an NPTH pad, an unfilled zone outline.
    #[default]
    None,
    /// Copper on the layers the item lists.
    Listed,
    /// A plated through-hole pad: on every copper layer, whatever it lists.
    Through,
}

/// What connectivity needs to know about one board item.
#[derive(Debug, Clone, Default)]
pub(super) struct ItemMeta {
    kind: Option<ItemKind>,
    net: i64,
    owner: String,
    /// Ordinal of the footprint a pad belongs to (set by the collector). The
    /// designator `owner` is not unique on a panel, which repeats them.
    pub(super) footprint: usize,
    pad: String,
    at: (f64, f64),
    /// A pad that must reach its net: plated, numbered and not `no_connect`.
    required: bool,
    /// How the item carries current, if at all.
    copper: Copper,
    /// Half the stroke KiCad 5 draws fill outlines with.
    slack: f64,
    /// A zone outline whose fill was never computed.
    unfilled: bool,
}

impl ItemMeta {
    pub(super) fn wire(kind: ItemKind, net: i64, at: (f64, f64)) -> Self {
        Self {
            kind: Some(kind),
            net,
            at,
            copper: Copper::Listed,
            ..Self::default()
        }
    }

    pub(super) fn pad(pad: &List, net: i64, owner: &str, at: (f64, f64)) -> Self {
        let plated = pad.arg_value(1).as_deref() != Some("np_thru_hole");
        let number = pad.arg_value(0).unwrap_or_default();
        let no_connect = pad
            .find_value("pintype")
            .is_some_and(|t| t.contains("no_connect"));
        Self {
            owner: owner.to_string(),
            required: plated && !no_connect && !number.is_empty(),
            copper: match pad.arg_value(1).as_deref() {
                Some("np_thru_hole") => Copper::None,
                Some("thru_hole") => Copper::Through,
                _ => Copper::Listed,
            },
            pad: number,
            ..Self::wire(ItemKind::Pad, net, at)
        }
    }

    pub(super) fn fill(net: i64, at: (f64, f64), zone: &List, root: &List) -> Self {
        // Before KiCad 6 a fill polygon is an outline stroked at the zone's
        // minimum thickness; `(filled_areas_thickness no)` marks exact copper.
        let stroked = zone.find_value("filled_areas_thickness").map_or_else(
            || root.find("version").and_then(|v| v.arg_i64(0)) < Some(20_200_000),
            |v| v != "no",
        );
        let slack = if stroked {
            zone.find_f64("min_thickness").unwrap_or(0.0) / 2.0
        } else {
            0.0
        };
        Self {
            slack,
            ..Self::wire(ItemKind::Zone, net, at)
        }
    }

    pub(super) fn unfilled(net: i64, outline: &[(f64, f64)], zone: &List) -> Self {
        Self {
            unfilled: zone.find("keepout").is_none(),
            copper: Copper::None,
            ..Self::wire(
                ItemKind::Zone,
                net,
                outline.first().copied().unwrap_or_default(),
            )
        }
    }
}

/// One piece of a net that copper does not join to the rest of it.
#[derive(Debug, Clone, PartialEq)]
pub struct CopperIsland {
    pub net: String,
    /// Pads in the island as `REF.pad`, sorted; empty for stray copper.
    pub pads: Vec<String>,
    /// Pads of the same net on its largest island, as `REF.pad`.
    pub main_pads: Vec<String>,
    /// A point on the island (mm, board frame).
    pub x: f64,
    pub y: f64,
}

/// The copper-connectivity result for one KiCad board.
#[derive(Debug, Clone, Default)]
pub struct CopperConnectivity {
    /// Every island beyond the largest one of its net.
    pub islands: Vec<CopperIsland>,
    /// Nets left unjudged because a zone of theirs has no computed fill.
    pub unfilled_zone_nets: Vec<String>,
    /// The board carries routing copper (a track, arc, via, fill or net
    /// graphic) at all; a layout of placed pads only is a netlist carrier.
    pub routed: bool,
    /// A panel of repeated copies of one circuit, when the board is one.
    pub panel: Option<PanelCopies>,
}

/// A board that repeats one routed circuit in several unjoined copies.
///
/// The copies share designators and net names. `KiCad` merges same-named nets
/// across them and draws the cross-copy ratsnest; each copy is judged on its
/// own, and this records what was set aside.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PanelCopies {
    /// How many copies the board repeats.
    pub copies: usize,
    /// Nets that are one same-named net across copies with no copper between
    /// them, sorted.
    pub split_nets: Vec<String>,
}

/// Connectivity of a parsed `.kicad_pcb`; `None` for anything else.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "one pass from items to islands to verdicts"
)]
pub fn copper_connectivity(doc: &forge_sexpr::Document) -> Option<CopperConnectivity> {
    let root = doc.root().filter(|r| r.name() == Some("kicad_pcb"))?;
    if renamed_copper(root) {
        return None;
    }
    let mut nets = NetResolver::from_root(root);
    let mut buckets = collect_primitives(root, &mut nets);
    push_net_graphics(root, &mut nets, &mut buckets);
    spread_through_holes(&mut buckets, &super::copper_layers_of(root));
    let items = &buckets.items;
    // An item with no shape on a copper layer the board declares carries
    // nothing to judge (a pad on mask layers only).
    let mut on_copper = vec![false; items.len()];
    for p in buckets.by_layer.values().flatten() {
        on_copper[p.item] = true;
    }
    for z in buckets.zones.values().flatten() {
        on_copper[z.item] = true;
    }
    let no_net = |net: i64| nets.is_no_net(net) || nets.name_of(net).is_empty();
    let live = |i: usize| on_copper[i] && items[i].copper != Copper::None && !no_net(items[i].net);
    let mut dsu = Dsu::new(items.len());
    let reach = 2.0f64.mul_add(items.iter().map(|m| m.slack).fold(0.0, f64::max), TOUCH_MM);

    for (layer, prims) in &buckets.by_layer {
        let zones = buckets.zones.get(layer).map_or(&[][..], Vec::as_slice);
        join_layer(prims, zones, &live, items, reach, &mut dsu);
    }
    join_footprint_links(root, items, &mut dsu);

    let unfilled: HashSet<i64> = items
        .iter()
        .filter(|m| m.unfilled && !no_net(m.net))
        .map(|m| m.net)
        .collect();
    // net -> island root -> member items, in board order.
    let mut by_net: HashMap<i64, Vec<(usize, Vec<usize>)>> = HashMap::new();
    for i in (0..items.len()).filter(|&i| live(i) && !unfilled.contains(&items[i].net)) {
        let root = dsu.find(i);
        let islands = by_net.entry(items[i].net).or_default();
        match islands.iter_mut().find(|(r, _)| *r == root) {
            Some((_, members)) => members.push(i),
            None => islands.push((root, vec![i])),
        }
    }
    let pads_of = |members: &[usize]| island_pads(items, members);
    let anchor_of = |members: &[usize]| island_anchor(items, members);
    let mut out = CopperConnectivity {
        routed: (0..items.len()).any(|i| live(i) && items[i].kind != Some(ItemKind::Pad)),
        ..CopperConnectivity::default()
    };
    let panel = PanelMap::build(items, &live, &mut dsu);
    let mut split_nets: Vec<String> = Vec::new();
    let mut net_ids: Vec<i64> = by_net.keys().copied().collect();
    net_ids.sort_unstable();
    for net in net_ids {
        let mut islands = by_net.remove(&net).unwrap_or_default();
        if islands.len() < 2 {
            continue;
        }
        // The largest island by pad count, then by size, is the net itself.
        // Islands equal on both are told apart by their pads and position, never
        // by the order the file listed them in: which one is "the rest of the
        // net" must not depend on it.
        islands.sort_by(|(_, a), (_, b)| {
            let size = |m: &Vec<usize>| {
                std::cmp::Reverse((m.iter().filter(|&&i| items[i].required).count(), m.len()))
            };
            size(a)
                .cmp(&size(b))
                .then_with(|| pads_of(a).cmp(&pads_of(b)))
                .then_with(|| {
                    let (pa, pb) = (anchor_of(a), anchor_of(b));
                    pa.0.total_cmp(&pb.0).then(pa.1.total_cmp(&pb.1))
                })
        });
        let main_pads = pads_of(&islands[0].1);
        // On a panel, copies of one circuit are told apart from a copy that
        // is itself broken: only the latter is an open connection.
        let judged = panel.judge(items, &islands);
        if judged.set_aside {
            split_nets.push(nets.name_of(net));
        }
        for (index, (_, members)) in islands.iter().enumerate().skip(1) {
            if judged.skip.contains(&index) {
                continue;
            }
            // A detached piece of pour, or a pad marked no-connect, is an
            // island KiCad leaves alone; a stray track or via is reported,
            // with no pads.
            let pads = pads_of(members);
            let kind_of = |&i: &usize| items[i].kind;
            let only_pour = members.iter().all(|i| kind_of(i) == Some(ItemKind::Zone));
            let no_connect = members.iter().any(|i| kind_of(i) == Some(ItemKind::Pad));
            if pads.is_empty() && (only_pour || no_connect) {
                continue;
            }
            let (x, y) = anchor_of(members);
            out.islands.push(CopperIsland {
                net: nets.name_of(net),
                pads,
                main_pads: judged
                    .main_of
                    .get(&index)
                    .map_or_else(|| main_pads.clone(), |m| pads_of(m)),
                x: quantize_mm(x),
                y: quantize_mm(y),
            });
        }
    }
    // Nets by name and islands by their pads, so the report order (and what a
    // cap on the number listed keeps) does not follow the file's net numbering.
    out.islands.sort_by(|a, b| {
        a.net
            .cmp(&b.net)
            .then_with(|| a.pads.cmp(&b.pads))
            .then(a.x.total_cmp(&b.x))
            .then(a.y.total_cmp(&b.y))
    });
    out.unfilled_zone_nets = unfilled.into_iter().map(|n| nets.name_of(n)).collect();
    out.unfilled_zone_nets.sort();
    split_nets.sort();
    split_nets.dedup();
    if !split_nets.is_empty() {
        out.panel = Some(PanelCopies {
            copies: panel.copies,
            split_nets,
        });
    }
    Some(out)
}

/// Which footprints sit in one of several repeated copies of a circuit.
///
/// A panel repeats designators and net names, so a name alone cannot say
/// which pad belongs to which copy. Copper can: footprints that share an
/// island of any net are one copy, and copies with the same set of
/// designators are repeats of each other.
struct PanelMap {
    /// Footprint ordinal -> id of the copy it is in.
    copy_of: HashMap<usize, usize>,
    /// How many copies the most repeated circuit has.
    copies: usize,
}

/// What the panel makes of one net's islands.
#[derive(Default)]
struct PanelVerdict {
    /// Island indices that are a whole copy of the net, not an open connection.
    skip: HashSet<usize>,
    /// Island index -> the island it is compared against, when that is not
    /// the net's largest island overall (it is the largest in its own copy).
    main_of: HashMap<usize, Vec<usize>>,
    /// Something was set aside.
    set_aside: bool,
}

impl PanelMap {
    fn build(items: &[ItemMeta], live: &dyn Fn(usize) -> bool, dsu: &mut Dsu) -> Self {
        let mut refs: HashMap<usize, &str> = HashMap::new();
        let mut first_in: HashMap<usize, usize> = HashMap::new();
        let mut groups = Dsu::new(items.iter().map(|m| m.footprint + 1).max().unwrap_or(0));
        for (i, m) in items.iter().enumerate() {
            if m.kind != Some(ItemKind::Pad) || !live(i) {
                continue;
            }
            refs.insert(m.footprint, m.owner.as_str());
            let first = *first_in.entry(dsu.find(i)).or_insert(m.footprint);
            groups.union(first, m.footprint);
        }
        let mut copies: HashMap<usize, Vec<usize>> = HashMap::new();
        for &fp in refs.keys() {
            copies.entry(groups.find(fp)).or_default().push(fp);
        }
        // A copy is a group of two or more parts whose designators another
        // group repeats exactly. A lone part says nothing about repetition.
        let mut shapes: HashMap<Vec<&str>, Vec<usize>> = HashMap::new();
        for (id, fps) in &copies {
            let mut names: Vec<&str> = fps.iter().map(|fp| refs[fp]).collect();
            names.sort_unstable();
            if fps.len() >= 2 && names.iter().all(|n| !n.is_empty()) {
                shapes.entry(names).or_default().push(*id);
            }
        }
        let mut copy_of = HashMap::new();
        let mut most = 0;
        for ids in shapes.values().filter(|ids| ids.len() >= 2) {
            most = most.max(ids.len());
            for id in ids {
                for &fp in &copies[id] {
                    copy_of.insert(fp, *id);
                }
            }
        }
        Self {
            copy_of,
            copies: most,
        }
    }

    /// Judge one net's islands, largest first. Inside a copy a second island
    /// is a missing connection. Between copies it is the panel, and so is an
    /// island that is a whole copy of the net though its parts did not
    /// group with the rest.
    fn judge(&self, items: &[ItemMeta], islands: &[(usize, Vec<usize>)]) -> PanelVerdict {
        let mut verdict = PanelVerdict::default();
        let copy_of_island = |members: &[usize]| {
            members
                .iter()
                .map(|&i| &items[i])
                .filter(|m| m.kind == Some(ItemKind::Pad))
                .find_map(|m| self.copy_of.get(&m.footprint).copied())
        };
        let copy: Vec<Option<usize>> = islands.iter().map(|(_, m)| copy_of_island(m)).collect();
        if copy.iter().all(Option::is_none) {
            return verdict;
        }
        let mut by_copy: HashMap<usize, Vec<usize>> = HashMap::new();
        let mut whole: HashSet<Vec<String>> = HashSet::new();
        for (index, id) in copy.iter().enumerate() {
            if let Some(id) = id {
                by_copy.entry(*id).or_default().push(index);
            }
        }
        for indices in by_copy.values() {
            // The copy's own largest island is the rest of its net.
            verdict.skip.insert(indices[0]);
            if indices.len() == 1 {
                whole.insert(island_pads(items, &islands[indices[0]].1));
            } else {
                for &index in &indices[1..] {
                    verdict.main_of.insert(index, islands[indices[0]].1.clone());
                }
            }
        }
        for (index, id) in copy.iter().enumerate() {
            let pads = island_pads(items, &islands[index].1);
            if id.is_none() && !pads.is_empty() && whole.contains(&pads) {
                verdict.skip.insert(index);
            }
        }
        verdict.set_aside = !verdict.skip.is_empty();
        verdict
    }
}

/// The required pads of an island, as sorted, deduplicated `REF.PAD` names.
fn island_pads(items: &[ItemMeta], members: &[usize]) -> Vec<String> {
    let mut pads: Vec<String> = members
        .iter()
        .map(|&i| &items[i])
        .filter(|m| m.required)
        .map(|m| format!("{}.{}", m.owner, m.pad))
        .collect();
    pads.sort();
    pads.dedup();
    pads
}

/// A point on an island that does not depend on member order: the first
/// required pad by name, else the lowest-left piece of copper.
fn island_anchor(items: &[ItemMeta], members: &[usize]) -> (f64, f64) {
    let ms = || members.iter().map(|&i| &items[i]);
    let pad = ms().filter(|m| m.required).min_by(|a, b| {
        (&a.owner, &a.pad)
            .cmp(&(&b.owner, &b.pad))
            .then(a.at.0.total_cmp(&b.at.0))
            .then(a.at.1.total_cmp(&b.at.1))
    });
    pad.or_else(|| ms().min_by(|a, b| a.at.0.total_cmp(&b.at.0).then(a.at.1.total_cmp(&b.at.1))))
        .map_or((0.0, 0.0), |m| m.at)
}

/// Whether the board renamed a copper layer.
fn renamed_copper(root: &List) -> bool {
    // A board that renamed its copper layers (KiCad 4/5 "Top", "Front") is
    // copper the extractor cannot place, so it is not judged at all.
    // Copper ordinals are 0-31 before KiCad 9 and the even ones from it on.
    let layers: Vec<&List> = root
        .find("layers")
        .map(|l| l.lists().collect())
        .unwrap_or_default();
    let kicad9 = root.find("version").and_then(|v| v.arg_i64(0)) >= Some(20_241_229)
        || layers
            .iter()
            .any(|l| l.name() == Some("2") && l.arg_value(0).as_deref() == Some("B.Cu"));
    layers.iter().any(|layer| {
        let copper = layer
            .name()
            .and_then(|n| n.parse::<i64>().ok())
            .is_some_and(|id| if kicad9 { id % 2 == 0 } else { id < 32 });
        copper && !layer.arg_value(0).unwrap_or_default().ends_with(".Cu")
    })
}

/// Union the same-net copper on one layer that touches, or that sits inside a
/// same-net fill.
fn join_layer(
    prims: &[super::Primitive],
    zones: &[super::ZonePoly],
    live: &dyn Fn(usize) -> bool,
    items: &[ItemMeta],
    reach: f64,
    dsu: &mut Dsu,
) {
    let leaves: Vec<BoundsLeaf> = prims
        .iter()
        .enumerate()
        .map(|(idx, p)| BoundsLeaf {
            bounds: p.bounds,
            idx,
        })
        .collect();
    let tree = RTree::bulk_load(leaves);
    for (i, p) in prims.iter().enumerate() {
        if !live(p.item) {
            continue;
        }
        let query = AABB::from_corners(
            [p.bounds[0] - reach, p.bounds[1] - reach],
            [p.bounds[2] + reach, p.bounds[3] + reach],
        );
        for leaf in tree.locate_in_envelope_intersecting(query) {
            let q = &prims[leaf.idx];
            if leaf.idx <= i || q.net != p.net || !live(q.item) {
                continue;
            }
            if dsu.find(p.item) == dsu.find(q.item) {
                continue;
            }
            let (gap, _) = shape_gap_at(&p.shape, &q.shape);
            if gap <= TOUCH_MM + items[p.item].slack + items[q.item].slack {
                dsu.union(p.item, q.item);
            }
        }
    }
    // Copper wholly inside a same-net fill never meets its outline; nor
    // does one fill nested inside another.
    for z in zones.iter().filter(|z| z.filled && live(z.item)) {
        let inside = |(x, y): (f64, f64)| {
            x >= z.bounds[0]
                && x <= z.bounds[2]
                && y >= z.bounds[1]
                && y <= z.bounds[3]
                && point_in_polygon(x, y, &z.pts)
        };
        let window = AABB::from_corners([z.bounds[0], z.bounds[1]], [z.bounds[2], z.bounds[3]]);
        for leaf in tree.locate_in_envelope_intersecting(window) {
            let p = &prims[leaf.idx];
            if p.net == z.net
                && p.kind != ItemKind::Zone
                && live(p.item)
                && inside(p.shape.center())
            {
                dsu.union(p.item, z.item);
            }
        }
        for other in zones {
            if other.net == z.net && other.filled && other.item != z.item && inside(other.pts[0]) {
                dsu.union(z.item, other.item);
            }
        }
    }
}

/// `KiCad` gives a plated through-hole pad every copper layer whatever its
/// `(layers)` list says, so a track on any layer that reaches it connects:
/// copy each such pad's shapes from one layer onto every layer missing it.
fn spread_through_holes(buckets: &mut super::LayerBuckets, copper: &[String]) {
    let mut shapes: HashMap<usize, (&String, Vec<super::Primitive>)> = HashMap::new();
    for (layer, prims) in &buckets.by_layer {
        for p in prims
            .iter()
            .filter(|p| buckets.items[p.item].copper == Copper::Through)
        {
            let (from, list) = shapes.entry(p.item).or_insert((layer, Vec::new()));
            if *from == layer {
                list.push(p.clone());
            }
        }
    }
    let mut copies: Vec<(String, super::Primitive)> = Vec::new();
    for layer in copper {
        let present: HashSet<usize> = buckets
            .by_layer
            .get(layer)
            .map(|prims| prims.iter().map(|p| p.item).collect())
            .unwrap_or_default();
        for (item, (_, list)) in &shapes {
            if !present.contains(item) {
                copies.extend(list.iter().map(|p| (layer.clone(), p.clone())));
            }
        }
    }
    for (layer, p) in copies {
        buckets.by_layer.entry(layer).or_default().push(p);
    }
}

/// Board-level copper graphics that carry a net (KiCad 7+ lets a `gr_poly`,
/// `gr_rect`, `gr_circle`, `gr_line` or `gr_arc` on a copper layer belong to a
/// net). They conduct like a track, so a pad reached only through one is
/// routed.
fn push_net_graphics(root: &List, nets: &mut NetResolver, buckets: &mut super::LayerBuckets) {
    for kind in ["gr_poly", "gr_rect", "gr_circle", "gr_line", "gr_arc"] {
        for g in root.find_all(kind) {
            let layer = g.find_value("layer").unwrap_or_default();
            let Some(net) = nets.net_ref(g).filter(|_| layer.ends_with(".Cu")) else {
                continue;
            };
            let half = g
                .find("stroke")
                .and_then(|s| s.find_f64("width"))
                .or_else(|| g.find_f64("width"))
                .unwrap_or(0.0)
                / 2.0;
            let filled = g
                .find("fill")
                .and_then(|f| f.arg_value(0))
                .is_some_and(|f| f == "yes" || f == "solid");
            let (start, end) = (super::xy_pair(g, "start"), super::xy_pair(g, "end"));
            let outline: Option<Vec<(f64, f64)>> = match (kind, start, end) {
                ("gr_poly", ..) => super::read_pts(g),
                ("gr_rect", Some(a), Some(b)) => Some(vec![a, (b.0, a.1), b, (a.0, b.1)]),
                _ => None,
            };
            let shapes: Vec<Shape> = match (kind, outline, start, end) {
                (_, Some(pts), ..) if filled => vec![Shape::Polygon { pts, r: half }],
                (_, Some(pts), ..) => (0..pts.len())
                    .map(|i| {
                        let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
                        Shape::Capsule(Capsule::new(a.0, a.1, b.0, b.1, half))
                    })
                    .collect(),
                ("gr_circle", _, Some(c), Some(e)) => {
                    let radius = (e.0 - c.0).hypot(e.1 - c.1);
                    if filled {
                        vec![Shape::disc(c.0, c.1, radius + half)]
                    } else {
                        super::ring_capsules(c.0, c.1, radius, half)
                            .into_iter()
                            .map(Shape::Capsule)
                            .collect()
                    }
                }
                ("gr_line" | "gr_arc", _, Some(a), Some(b)) => {
                    super::flatten_arc(a, super::xy_pair(g, "mid"), b, 2.0 * half)
                        .into_iter()
                        .map(Shape::Capsule)
                        .collect()
                }
                _ => Vec::new(),
            };
            let at = start.unwrap_or_default();
            buckets.begin_item(ItemMeta::wire(ItemKind::Graphic, net, at));
            for shape in shapes {
                buckets.push(
                    &layer,
                    super::make_prim(shape, net, ItemKind::Graphic, String::new()),
                );
            }
        }
    }
}

/// Pads a footprint itself joins: a net tie's pad groups and KiCad 9 jumper
/// groups (explicit, or every pad sharing a number unless the footprint says
/// duplicate numbers are not jumpers). Only pads already on one net are joined;
/// the tie between different nets is the short sweep's business.
fn join_footprint_links(root: &List, items: &[ItemMeta], dsu: &mut Dsu) {
    // Keyed by footprint ordinal, never by designator: a panel repeats every
    // designator, and joining same-named pads of different copies would
    // knit the copies into one conductor.
    let mut pads: HashMap<(usize, &str), Vec<usize>> = HashMap::new();
    // Each footprint's own pad numbers, so a footprint reads its pads and not the board's.
    let mut numbers: HashMap<usize, Vec<&str>> = HashMap::new();
    for (i, m) in items.iter().enumerate() {
        if m.kind == Some(ItemKind::Pad) && m.copper != Copper::None {
            let lands = pads.entry((m.footprint, m.pad.as_str())).or_default();
            if lands.is_empty() && !m.pad.is_empty() {
                numbers.entry(m.footprint).or_default().push(m.pad.as_str());
            }
            lands.push(i);
        }
    }
    for (owner, fp) in root
        .find_all("footprint")
        .chain(root.find_all("module"))
        .enumerate()
    {
        // Net ties write each group as one "1, 2" string; jumpers (KiCad 9)
        // as a list of quoted pad numbers.
        let ties = fp.find("net_tie_pad_groups");
        let mut groups: Vec<Vec<String>> = ties
            .iter()
            .flat_map(|g| (0..).map_while(move |i| g.arg_value(i)))
            .map(|spec| spec.split(',').map(|p| p.trim().to_string()).collect())
            .collect();
        groups.extend(
            fp.find("jumper_pad_groups")
                .iter()
                .flat_map(|g| g.lists())
                .map(|l| {
                    let atoms = l.children.iter().filter_map(|c| match c {
                        forge_sexpr::Sexpr::Token(t) => Some(t.value()),
                        forge_sexpr::Sexpr::List(_) => None,
                    });
                    atoms.collect()
                }),
        );
        // KiCad 9 writes the flag on every footprint; before it there was no
        // way to say so, and a number shared by several lands (a connector
        // shell's tabs, a split exposed pad) named one pin of the part, whose
        // metal joins them. So only an explicit `no` leaves them to copper.
        if fp
            .find_value("duplicate_pad_numbers_are_jumpers")
            .as_deref()
            != Some("no")
        {
            groups.extend(
                numbers
                    .get(&owner)
                    .into_iter()
                    .flatten()
                    .map(|n| vec![(*n).to_string()]),
            );
        }
        for group in groups {
            let members: Vec<usize> = group
                .iter()
                .filter_map(|n| pads.get(&(owner, n.as_str())))
                .flatten()
                .copied()
                .collect();
            // Every member on one net joins the first on it (not each pair: a
            // group of k pads would cost k squared).
            let mut first_on: HashMap<i64, usize> = HashMap::new();
            for &a in &members {
                let first = *first_on.entry(items[a].net).or_insert(a);
                if first != a {
                    dsu.union(first, a);
                }
            }
        }
    }
}

/// [`copper_connectivity`] over board text; `None` when it is not a parseable
/// `.kicad_pcb`.
#[must_use]
pub fn copper_connectivity_from_text(text: &str) -> Option<CopperConnectivity> {
    forge_sexpr::parse(text)
        .ok()
        .and_then(|doc| copper_connectivity(&doc))
}

/// The weakest copper on the best-rated path a current takes between two
/// groups of pads of one net.
#[derive(Debug, Clone, PartialEq)]
pub enum PathNeck {
    /// The pads are joined without any track limiting the path: they touch,
    /// or fills, vias and net graphics carry the current the whole way.
    Untracked,
    /// The lowest-rated track (or arc) on the best path.
    Track { layer: String, width_mm: f64 },
}
/// The weakest track on the best path between two groups of pads of a net.
///
/// `from` and `to` name pads as `(reference, pad number)`. `None` when the
/// board is not a readable `.kicad_pcb`, a pad cannot be found, or copper
/// does not join the two groups.
///
/// A net's narrowest segment is not always on the path its current takes. A
/// buck switch node also reaches its bootstrap capacitor, and that branch
/// carries the gate-drive charge, not the inductor current, so rating the
/// node by its narrowest segment judges a stub that the load current never
/// enters. This finds the path whose weakest track is strongest (a widest-path
/// search: every route is a candidate, and the best one bounds what the board
/// can carry from end to end) and names that weakest track. `rating` turns
/// a track's layer and width into the figure to compare (an ampacity), so a
/// wide track on thin inner copper ranks below a narrower one outside. Pads,
/// vias, fills and net graphics are not limited here: their capacity is a
/// different question from a track's.
#[must_use]
pub fn path_neck_from_text(
    text: &str,
    net: &str,
    from: &[(String, String)],
    to: &[(String, String)],
    rating: &dyn Fn(&str, f64) -> f64,
) -> Option<PathNeck> {
    let doc = forge_sexpr::parse(text).ok()?;
    let root = doc.root().filter(|r| r.name() == Some("kicad_pcb"))?;
    if renamed_copper(root) {
        return None;
    }
    let mut nets = NetResolver::from_root(root);
    let mut buckets = collect_primitives(root, &mut nets);
    push_net_graphics(root, &mut nets, &mut buckets);
    spread_through_holes(&mut buckets, &super::copper_layers_of(root));
    let net_id = *nets.by_name.get(&crate::netname::unescape_net_name(net))?;
    let items = &buckets.items;
    let pad_set = |pads: &[(String, String)]| -> Vec<usize> {
        (0..items.len())
            .filter(|&i| {
                let m = &items[i];
                m.net == net_id
                    && m.kind == Some(ItemKind::Pad)
                    && m.copper != Copper::None
                    && pads.iter().any(|(r, n)| *r == m.owner && *n == m.pad)
            })
            .collect()
    };
    let (sources, sinks) = (pad_set(from), pad_set(to));
    if sources.is_empty() || sinks.is_empty() {
        return None;
    }
    let graph = NetGraph::build(&buckets, net_id, rating);
    let best = graph.widest_from(&sources);
    let reached = sinks
        .iter()
        .filter_map(|t| best.get(t))
        .max_by(|a, b| a.0.total_cmp(&b.0))?;
    Some(
        match reached.1.and_then(|n| graph.limit.get(&n)?.1.clone()) {
            Some((layer, width_mm)) => PathNeck::Track { layer, width_mm },
            None => PathNeck::Untracked,
        },
    )
}

/// One net's copper as a graph of board items that touch.
struct NetGraph {
    /// Each item's own limit, with the track behind it: a track's rating on
    /// its layer; unlimited for anything else.
    limit: HashMap<usize, (f64, Option<(String, f64)>)>,
    adjacent: HashMap<usize, HashSet<usize>>,
}

impl NetGraph {
    fn build(buckets: &super::LayerBuckets, net: i64, rating: &dyn Fn(&str, f64) -> f64) -> Self {
        let items = &buckets.items;
        let mut g = Self {
            limit: HashMap::new(),
            adjacent: HashMap::new(),
        };
        let reach = 2.0f64.mul_add(items.iter().map(|m| m.slack).fold(0.0, f64::max), TOUCH_MM);
        for (layer, all) in &buckets.by_layer {
            let prims: Vec<&super::Primitive> = all
                .iter()
                .filter(|p| p.net == net && items[p.item].copper != Copper::None)
                .collect();
            for p in &prims {
                let own = match (&p.shape, items[p.item].kind) {
                    (Shape::Capsule(c), Some(ItemKind::Track | ItemKind::Arc)) => {
                        let width = 2.0 * c.r;
                        (rating(layer, width), Some((layer.clone(), width)))
                    }
                    _ => (f64::INFINITY, None),
                };
                g.limit.entry(p.item).or_insert(own);
            }
            for (i, p) in prims.iter().enumerate() {
                for q in &prims[i + 1..] {
                    let apart = p.bounds[0] > q.bounds[2] + reach
                        || q.bounds[0] > p.bounds[2] + reach
                        || p.bounds[1] > q.bounds[3] + reach
                        || q.bounds[1] > p.bounds[3] + reach;
                    if p.item == q.item || apart {
                        continue;
                    }
                    let (gap, _) = shape_gap_at(&p.shape, &q.shape);
                    if gap <= TOUCH_MM + items[p.item].slack + items[q.item].slack {
                        g.join(p.item, q.item);
                    }
                }
            }
            // Copper wholly inside a same-net fill never meets its outline.
            let fills = buckets.zones.get(layer).map_or(&[][..], Vec::as_slice);
            for z in fills.iter().filter(|z| z.filled && z.net == net) {
                for p in prims.iter().filter(|p| p.kind != ItemKind::Zone) {
                    let (x, y) = p.shape.center();
                    if x >= z.bounds[0]
                        && x <= z.bounds[2]
                        && y >= z.bounds[1]
                        && y <= z.bounds[3]
                        && point_in_polygon(x, y, &z.pts)
                    {
                        g.join(p.item, z.item);
                    }
                }
            }
        }
        g
    }

    fn join(&mut self, a: usize, b: usize) {
        self.adjacent.entry(a).or_default().insert(b);
        self.adjacent.entry(b).or_default().insert(a);
    }

    /// Widest-path search from `sources`: for each item reached, the strongest
    /// bottleneck any route arrives with, and the item that sets it.
    fn widest_from(&self, sources: &[usize]) -> HashMap<usize, (f64, Option<usize>)> {
        let mut best: HashMap<usize, (f64, Option<usize>)> = HashMap::new();
        let mut frontier: Vec<usize> = Vec::new();
        for &s in sources {
            best.insert(s, (f64::INFINITY, None));
            frontier.push(s);
        }
        let mut done: HashSet<usize> = HashSet::new();
        while let Some(pos) = (0..frontier.len())
            .max_by(|&a, &b| best[&frontier[a]].0.total_cmp(&best[&frontier[b]].0))
        {
            let u = frontier.swap_remove(pos);
            if !done.insert(u) {
                continue;
            }
            let (cap_u, neck_u) = best[&u];
            for &v in self.adjacent.get(&u).into_iter().flatten() {
                if done.contains(&v) {
                    continue;
                }
                let own = self.limit.get(&v).map_or(f64::INFINITY, |l| l.0);
                let cand = if own < cap_u {
                    (own, Some(v))
                } else {
                    (cap_u, neck_u)
                };
                if best.get(&v).is_none_or(|b| cand.0 > b.0) {
                    best.insert(v, cand);
                    frontier.push(v);
                }
            }
        }
        best
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    fn board(stub_w: f64, path_w: f64) -> String {
        // U1 pad 2 (switch) at (0,0) runs to L1 pad 1 at (10,0); a stub from
        // the switch pad reaches C1 pad 1 at (0,3).
        format!(
            "(kicad_pcb (version 20221018) (net 0 \"\") (net 1 \"SW\") \
             (footprint \"U\" (layer \"F.Cu\") (at 0 0) (property \"Reference\" \"U1\") \
               (pad \"2\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net 1 \"SW\"))) \
             (footprint \"L\" (layer \"F.Cu\") (at 10 0) (property \"Reference\" \"L1\") \
               (pad \"1\" smd rect (at 0 0) (size 2 2) (layers \"F.Cu\") (net 1 \"SW\"))) \
             (footprint \"C\" (layer \"F.Cu\") (at 0 3) (property \"Reference\" \"C1\") \
               (pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"F.Cu\") (net 1 \"SW\"))) \
             (segment (start 0 0) (end 5 0) (width {path_w}) (layer \"F.Cu\") (net 1)) \
             (segment (start 5 0) (end 10 0) (width {path_w}) (layer \"F.Cu\") (net 1)) \
             (segment (start 0 0) (end 0 3) (width {stub_w}) (layer \"F.Cu\") (net 1)))"
        )
    }

    fn pads(r: &str, n: &str) -> Vec<(String, String)> {
        vec![(r.to_string(), n.to_string())]
    }

    fn neck(text: &str) -> Option<PathNeck> {
        path_neck_from_text(text, "SW", &pads("U1", "2"), &pads("L1", "1"), &|_, w| w)
    }

    #[test]
    fn a_stub_off_the_path_does_not_limit_it() {
        assert_eq!(
            neck(&board(0.2, 1.0)),
            Some(PathNeck::Track {
                layer: "F.Cu".into(),
                width_mm: 1.0
            })
        );
    }

    #[test]
    fn a_narrow_track_on_the_path_limits_it() {
        assert_eq!(
            neck(&board(1.0, 0.3)),
            Some(PathNeck::Track {
                layer: "F.Cu".into(),
                width_mm: 0.3
            })
        );
    }

    #[test]
    fn unresolved_pads_or_broken_copper_say_nothing() {
        let text = board(0.2, 1.0);
        assert_eq!(
            path_neck_from_text(&text, "SW", &pads("U9", "2"), &pads("L1", "1"), &|_, w| w),
            None
        );
        let broken = text.replace("(start 5 0) (end 10 0)", "(start 7 0) (end 10 0)");
        assert_eq!(neck(&broken), None);
    }
}
