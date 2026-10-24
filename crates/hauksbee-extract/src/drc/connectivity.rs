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
    let mut split_nets: Vec<String> = Vec::new();
    let mut most_copies = 0;
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
        let judged = judge_panel(items, &islands);
        if judged.set_aside() {
            split_nets.push(nets.name_of(net));
            most_copies = most_copies.max(judged.copies);
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
                    .map_or_else(|| main_pads.clone(), |&m| pads_of(&islands[m].1)),
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
            copies: most_copies,
            split_nets,
        });
    }
    Some(out)
}

/// What the panel makes of one net's islands.
#[derive(Default)]
struct PanelVerdict {
    /// Island indices that are another copy's piece of the net, not an open
    /// connection: the first piece of each copy but the net's main island.
    skip: HashSet<usize>,
    /// Island index -> the island of its own copy it is compared against.
    main_of: HashMap<usize, usize>,
    /// How many copies of the net the islands fall into; 1 when the net is
    /// not repeated.
    copies: usize,
}

impl PanelVerdict {
    /// The net is one same-named net across several copies.
    const fn set_aside(&self) -> bool {
        self.copies >= 2
    }
}

/// One copy's share of a net while its islands are being sorted into copies.
struct CopyOfNet {
    /// The copy's first, largest island.
    main: usize,
    /// Pad name -> the footprint that has it in this copy.
    pads: HashMap<String, usize>,
    /// Where the copy's islands anchor.
    anchors: Vec<(f64, f64)>,
}

/// Sort one net's islands, largest first, into the copies of a panel.
///
/// A panel repeats designators and net names, so `KiCad` joins a net across
/// the copies and draws ratsnest between them. A pad name tells which copy an
/// island is in only against another island: two islands holding the same
/// `REF.PAD` on different footprints cannot be one copy's, and an island
/// sharing none of a copy's pads can be. That reads only the net's own
/// islands, never how the rest of the copy is routed, so a copy with many
/// connections open is judged as a copy with one.
///
/// Each island joins the nearest copy it does not clash with, or starts a new
/// copy when it clashes with them all. The first island of each copy is the
/// rest of that copy's net and is set aside as the panel's; the others are
/// open connections inside their copy. A net whose pad names never repeat is
/// one copy, and every island after the first is an open connection.
fn judge_panel(items: &[ItemMeta], islands: &[(usize, Vec<usize>)]) -> PanelVerdict {
    let mut verdict = PanelVerdict {
        copies: 1,
        ..PanelVerdict::default()
    };
    let keys: Vec<Vec<(String, usize)>> = islands
        .iter()
        .map(|(_, members)| island_pad_keys(items, members))
        .collect();
    let mut first_seen: HashMap<&str, usize> = HashMap::new();
    let repeated = keys.iter().flatten().any(|(name, footprint)| {
        *first_seen.entry(name.as_str()).or_insert(*footprint) != *footprint
    });
    if !repeated {
        return verdict;
    }
    let clashes = |copy: &CopyOfNet, island: &[(String, usize)]| {
        island
            .iter()
            .any(|(name, footprint)| copy.pads.get(name).is_some_and(|f| f != footprint))
    };
    let mut copies: Vec<CopyOfNet> = Vec::new();
    for (index, (_, members)) in islands.iter().enumerate() {
        let anchor = island_anchor(items, members);
        let nearest = copies
            .iter()
            .enumerate()
            .filter(|(_, copy)| !clashes(copy, &keys[index]))
            .map(|(at, copy)| {
                let gap = copy
                    .anchors
                    .iter()
                    .map(|a| (a.0 - anchor.0).hypot(a.1 - anchor.1))
                    .fold(f64::INFINITY, f64::min);
                (at, gap)
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(at, _)| at);
        let at = nearest.unwrap_or_else(|| {
            copies.push(CopyOfNet {
                main: index,
                pads: HashMap::new(),
                anchors: Vec::new(),
            });
            if index > 0 {
                verdict.skip.insert(index);
            }
            copies.len() - 1
        });
        let copy = &mut copies[at];
        if copy.main != index {
            verdict.main_of.insert(index, copy.main);
        }
        copy.pads.extend(keys[index].iter().cloned());
        copy.anchors.push(anchor);
    }
    verdict.copies = copies.len();
    verdict
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

/// The required pads of an island as (`REF.PAD`, footprint ordinal), sorted and
/// deduplicated. The ordinal tells two parts of one designator apart, as a
/// panel repeats them. A footprint with no real designator (blank, `REF**`, `R?`)
/// is left out: unnamed parts share a name without being copies of anything.
fn island_pad_keys(items: &[ItemMeta], members: &[usize]) -> Vec<(String, usize)> {
    let mut pads: Vec<(String, usize)> = members
        .iter()
        .map(|&i| &items[i])
        .filter(|m| m.required && !m.owner.is_empty() && !m.owner.ends_with(['*', '?']))
        .map(|m| (format!("{}.{}", m.owner, m.pad), m.footprint))
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
    /// Where two touching items meet (keyed by the lower item first).
    contact: HashMap<(usize, usize), (f64, f64)>,
    /// Each item's shape as the sense-path estimate reads it.
    geo: HashMap<usize, ItemGeo>,
}

/// One item's geometry for the shared-copper estimate.
#[derive(Debug, Clone)]
struct ItemGeo {
    kind: Option<ItemKind>,
    layer: String,
    /// A track's width (mm); 0 for anything else.
    width: f64,
    /// The size (mm) of the contact the item makes with copper it touches.
    radius: f64,
    /// A point on the item (a pad's centre).
    at: (f64, f64),
}

impl NetGraph {
    fn build(buckets: &super::LayerBuckets, net: i64, rating: &dyn Fn(&str, f64) -> f64) -> Self {
        let items = &buckets.items;
        let mut g = Self {
            limit: HashMap::new(),
            adjacent: HashMap::new(),
            contact: HashMap::new(),
            geo: HashMap::new(),
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
                let b = p.bounds;
                let (width, radius) = match &p.shape {
                    Shape::Capsule(c) => (2.0 * c.r, c.r),
                    _ => (0.0, 0.5 * (b[2] - b[0]).min(b[3] - b[1])),
                };
                let geo = g.geo.entry(p.item).or_insert_with(|| ItemGeo {
                    kind: items[p.item].kind,
                    layer: layer.clone(),
                    width,
                    radius,
                    at: (f64::midpoint(b[0], b[2]), f64::midpoint(b[1], b[3])),
                });
                if items[p.item].kind == Some(ItemKind::Pad) {
                    geo.at = items[p.item].at;
                    geo.radius = geo.radius.max(radius);
                }
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
                    let (gap, at) = shape_gap_at(&p.shape, &q.shape);
                    if gap <= TOUCH_MM + items[p.item].slack + items[q.item].slack {
                        g.join(p.item, q.item, at);
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
                        g.join(p.item, z.item, (x, y));
                    }
                }
            }
        }
        g
    }

    fn join(&mut self, a: usize, b: usize, at: (f64, f64)) {
        self.adjacent.entry(a).or_default().insert(b);
        self.adjacent.entry(b).or_default().insert(a);
        self.contact.entry((a.min(b), a.max(b))).or_insert(at);
    }

    /// Where `a` and `b` meet.
    fn contact_of(&self, a: usize, b: usize) -> (f64, f64) {
        self.contact
            .get(&(a.min(b), a.max(b)))
            .copied()
            .unwrap_or_default()
    }

    fn neighbours(&self, a: usize) -> impl Iterator<Item = usize> + '_ {
        self.adjacent.get(&a).into_iter().flatten().copied()
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

/// How a current-sense tap's copper reaches its shunt's pad.
#[derive(Debug, Clone, PartialEq)]
pub enum TapCopper {
    /// The tap has copper of its own from the shunt pad (a Kelvin connection):
    /// no load current flows in the copper its reading crosses.
    Dedicated,
    /// The tap reaches the shunt pad only through copper that also carries the
    /// load current, so the reading adds that copper's voltage drop.
    Shared {
        /// Estimated resistance of the shared copper (ohms) at the board's
        /// copper thickness.
        ohms: f64,
        /// Straight distance from the shunt pad to where the tap joins it (mm).
        distance_mm: f64,
        /// The shared copper includes a pour.
        through_pour: bool,
    },
}

/// Copper thickness (mm) assumed when the stackup does not state one: 1 oz.
const DEFAULT_COPPER_MM: f64 = 0.035;

/// Sheet resistance of 1 oz copper (ohms per square), the IPC-2152 figure.
const ONE_OZ_OHMS_PER_SQUARE: f64 = 0.5e-3;

/// Smallest contact (mm) a spreading estimate assumes, so a point contact does
/// not make the resistance run off to infinity.
const MIN_CONTACT_MM: f64 = 0.1;

/// Sheet resistance (ohms per square) of each copper layer the stackup gives a
/// thickness for.
fn sheet_resistance(root: &List) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    let layers = root
        .find("setup")
        .and_then(|s| s.find("stackup"))
        .into_iter()
        .flat_map(|s| s.find_all("layer"));
    for layer in layers {
        let name = layer.arg_value(0).unwrap_or_default();
        let thick = layer.find_f64("thickness").filter(|t| *t > 0.0);
        if let (true, Some(t)) = (name.ends_with(".Cu"), thick) {
            out.insert(name, ONE_OZ_OHMS_PER_SQUARE * DEFAULT_COPPER_MM / t);
        }
    }
    out
}

/// How each sense tap's copper reaches one pad of a current-sense shunt.
///
/// `shunt`, `taps` and `loads` name pads of `net` as `(reference, pad number)`;
/// a load is a pad the shunt's current flows to or from. One answer per tap, in
/// order. `None` when the board is not a readable `.kicad_pcb`, the shunt or a
/// tap pad is not found or names more than one footprint, a pour on the net was
/// saved unfilled, or copper does not join a tap and the loads to the shunt pad.
///
/// The load current takes the lowest-resistance copper from the loads to the
/// shunt pad. Copper the loads reach without passing through the shunt pad or
/// the tap is load-carrying; a tap whose own copper reaches the shunt pad
/// without touching it is a Kelvin connection. Otherwise the error is the
/// resistance the tap's junction and a load's current share on their way to the
/// shunt pad: tracks by their squares (length over width), a pour by the
/// spreading resistance between point contacts in a sheet,
/// `ln(r_je * r_el / (r_jl * a)) / 2 pi` squares for a junction j, entry e and
/// load l. Vias and pads count nothing, and the smallest figure over the loads
/// and junctions is the one reported, so the estimate errs low.
#[must_use]
pub fn kelvin_taps_from_text(
    text: &str,
    net: &str,
    shunt: &(String, String),
    taps: &[(String, String)],
    loads: &[(String, String)],
) -> Option<Vec<TapCopper>> {
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
    if items.iter().any(|m| m.unfilled && m.net == net_id) {
        return None;
    }
    let pads_of = |pads: &[(String, String)]| -> HashSet<usize> {
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
    // One pad of one footprint: a panel repeating the designator is not judged.
    let one_pad = |pad: &(String, String)| -> Option<HashSet<usize>> {
        let found = pads_of(std::slice::from_ref(pad));
        let footprints: HashSet<usize> = found.iter().map(|&i| items[i].footprint).collect();
        (footprints.len() == 1).then_some(found)
    };
    let shunt_items = one_pad(shunt)?;
    let load_items = pads_of(loads);
    if load_items.is_empty() {
        return None;
    }
    let graph = NetGraph::build(&buckets, net_id, &|_, _| f64::INFINITY);
    let sheet = sheet_resistance(root);
    let mut out = Vec::new();
    for tap in taps {
        let tap_items = one_pad(tap)?;
        out.push(graph.tap_copper(&shunt_items, &tap_items, &load_items, &sheet)?);
    }
    Some(out)
}

/// Items reachable from `from` without entering `blocked`.
fn reach_avoiding(
    graph: &NetGraph,
    from: &HashSet<usize>,
    blocked: &dyn Fn(usize) -> bool,
) -> HashSet<usize> {
    let mut seen: HashSet<usize> = from.clone();
    let mut stack: Vec<usize> = from.iter().copied().collect();
    while let Some(u) = stack.pop() {
        for v in graph.neighbours(u) {
            if !blocked(v) && seen.insert(v) {
                stack.push(v);
            }
        }
    }
    seen
}

/// The lowest-resistance tree from the shunt pad over load-carrying copper.
struct ShuntTree {
    /// Resistance (ohms) from the shunt pad to where each item is entered.
    dist: HashMap<usize, f64>,
    /// Where each item is entered, and the size of the contact feeding it.
    entry: HashMap<usize, ((f64, f64), f64)>,
    /// The item each one is entered from.
    pred: HashMap<usize, usize>,
}

impl ShuntTree {
    /// The chain from `item` back to the shunt pad, `item` first.
    fn chain(&self, mut item: usize) -> Vec<usize> {
        let mut out = vec![item];
        while let Some(&p) = self.pred.get(&item) {
            out.push(p);
            item = p;
        }
        out
    }
}

/// A heap entry ordered so the smallest resistance pops first.
#[derive(PartialEq)]
struct Cost(f64, usize);
impl Eq for Cost {}
impl PartialOrd for Cost {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Cost {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.0.total_cmp(&self.0).then(other.1.cmp(&self.1))
    }
}

fn dist_mm(a: (f64, f64), b: (f64, f64)) -> f64 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

impl NetGraph {
    /// The geometry of `item`; a contact-free point when it has none.
    fn geo_of(&self, item: usize) -> ItemGeo {
        self.geo.get(&item).cloned().unwrap_or(ItemGeo {
            kind: None,
            layer: String::new(),
            width: 0.0,
            radius: 0.0,
            at: (0.0, 0.0),
        })
    }

    /// Sheet resistance (ohms per square) of the layer `geo` is on.
    fn sheet_of(geo: &ItemGeo, sheet: &HashMap<String, f64>) -> f64 {
        sheet
            .get(&geo.layer)
            .copied()
            .unwrap_or(ONE_OZ_OHMS_PER_SQUARE)
    }

    /// Resistance (ohms) of crossing `item` from contact `a` (size `ra`) to
    /// contact `b` (size `rb`).
    fn crossing(
        &self,
        item: usize,
        (a, ra): ((f64, f64), f64),
        (b, rb): ((f64, f64), f64),
        sheet: &HashMap<String, f64>,
    ) -> f64 {
        let geo = self.geo_of(item);
        let d = dist_mm(a, b);
        let squares = match geo.kind {
            Some(ItemKind::Track | ItemKind::Arc) if geo.width > 0.0 => d / geo.width,
            Some(ItemKind::Zone) => {
                let (ra, rb) = (ra.max(MIN_CONTACT_MM), rb.max(MIN_CONTACT_MM));
                ((d.max(ra) / ra).ln() + (d.max(rb) / rb).ln()) / std::f64::consts::TAU
            }
            _ => 0.0,
        };
        squares * Self::sheet_of(&geo, sheet)
    }

    fn shunt_tree(
        &self,
        shunt: &HashSet<usize>,
        allowed: &HashSet<usize>,
        sheet: &HashMap<String, f64>,
    ) -> ShuntTree {
        let mut tree = ShuntTree {
            dist: HashMap::new(),
            entry: HashMap::new(),
            pred: HashMap::new(),
        };
        let mut heap = std::collections::BinaryHeap::new();
        for &s in shunt {
            let geo = self.geo_of(s);
            tree.dist.insert(s, 0.0);
            tree.entry.insert(s, (geo.at, geo.radius));
            heap.push(Cost(0.0, s));
        }
        let mut done: HashSet<usize> = HashSet::new();
        while let Some(Cost(d, u)) = heap.pop() {
            if !done.insert(u) {
                continue;
            }
            let into_u = tree.entry[&u];
            let ru = self.geo_of(u).radius;
            for v in self.neighbours(u) {
                if done.contains(&v) || !allowed.contains(&v) {
                    continue;
                }
                let at = self.contact_of(u, v);
                let cand = d + self.crossing(u, into_u, (at, self.geo_of(v).radius), sheet);
                if tree.dist.get(&v).is_none_or(|&old| cand < old) {
                    tree.dist.insert(v, cand);
                    tree.entry.insert(v, (at, ru));
                    tree.pred.insert(v, u);
                    heap.push(Cost(cand, v));
                }
            }
        }
        tree
    }

    /// The resistance (ohms) a junction, where tap copper `p` meets load copper
    /// `q`, shares with load `l`'s current on the way to the shunt pad, and
    /// whether that copper includes a pour.
    fn shared(
        &self,
        tree: &ShuntTree,
        (p, q): (usize, usize),
        l: usize,
        sheet: &HashMap<String, f64>,
    ) -> Option<(f64, bool)> {
        let (cj, cl) = (tree.chain(q), tree.chain(l));
        // The deepest item both chains pass through (both end at the pad).
        let on_l: HashSet<usize> = cl.iter().copied().collect();
        let fork_at = cj.iter().position(|i| on_l.contains(i))?;
        let fork = cj[fork_at];
        let pour = cj[fork_at..]
            .iter()
            .any(|&i| self.geo_of(i).kind == Some(ItemKind::Zone));
        // Where a chain leaves the fork, and the size of that contact.
        let leave = |chain: &[usize], end: ((f64, f64), f64)| {
            let at = chain.iter().position(|&i| i == fork)?;
            Some(if at == 0 {
                end
            } else {
                let next = chain[at - 1];
                (self.contact_of(fork, next), self.geo_of(next).radius)
            })
        };
        let (xj, rj) = leave(&cj, (self.contact_of(p, q), self.geo_of(p).radius))?;
        let (xl, rl) = leave(&cl, (self.geo_of(l).at, self.geo_of(l).radius))?;
        let (e, re) = tree.entry[&fork];
        let geo = self.geo_of(fork);
        let squares = match geo.kind {
            Some(ItemKind::Track | ItemKind::Arc) if geo.width > 0.0 => {
                // Along a track the two currents share the stretch up to the
                // nearer exit, when both leave on the same side of the entry.
                let (vj, vl) = ((xj.0 - e.0, xj.1 - e.1), (xl.0 - e.0, xl.1 - e.1));
                if vj.0.mul_add(vl.0, vj.1 * vl.1) > 0.0 {
                    dist_mm(xj, e).min(dist_mm(xl, e)) / geo.width
                } else {
                    0.0
                }
            }
            Some(ItemKind::Zone) => {
                let re = re.max(MIN_CONTACT_MM);
                let junction_entry = dist_mm(xj, e).max(rj.max(re));
                let entry_load = dist_mm(e, xl).max(re.max(rl));
                let junction_load = dist_mm(xj, xl).max(rj.max(rl).max(MIN_CONTACT_MM));
                ((junction_entry * entry_load) / (junction_load * re))
                    .ln()
                    .max(0.0)
                    / std::f64::consts::TAU
            }
            _ => 0.0,
        };
        Some((
            tree.dist[&fork] + squares * Self::sheet_of(&geo, sheet),
            pour,
        ))
    }

    /// How one tap's copper reaches the shunt pad; `None` when copper does not
    /// join them.
    fn tap_copper(
        &self,
        shunt: &HashSet<usize>,
        tap: &HashSet<usize>,
        loads: &HashSet<usize>,
        sheet: &HashMap<String, f64>,
    ) -> Option<TapCopper> {
        let blocked = |i: usize| shunt.contains(&i) || tap.contains(&i);
        let load_side = reach_avoiding(self, loads, &blocked);
        let private = reach_avoiding(self, tap, &|i| shunt.contains(&i) || load_side.contains(&i));
        if private
            .iter()
            .any(|&i| self.neighbours(i).any(|v| shunt.contains(&v)))
        {
            return Some(TapCopper::Dedicated);
        }
        let mut allowed = load_side;
        allowed.extend(shunt.iter().copied());
        let tree = self.shunt_tree(shunt, &allowed, sheet);
        let pad = shunt.iter().map(|&s| self.geo_of(s).at).next()?;
        let mut best: Option<(f64, f64, bool)> = None;
        for &p in &private {
            for q in self.neighbours(p).filter(|q| tree.dist.contains_key(q)) {
                for &l in loads.iter().filter(|l| tree.dist.contains_key(l)) {
                    let Some((ohms, pour)) = self.shared(&tree, (p, q), l, sheet) else {
                        continue;
                    };
                    if best.is_none_or(|b| ohms < b.0) {
                        best = Some((ohms, dist_mm(pad, self.contact_of(p, q)), pour));
                    }
                }
            }
        }
        let (ohms, distance_mm, through_pour) = best?;
        Some(TapCopper::Shared {
            ohms,
            distance_mm,
            through_pour,
        })
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
