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
}

/// Connectivity of a parsed `.kicad_pcb`; `None` for anything else.
#[must_use]
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
    let pads_of = |members: &[usize]| -> Vec<String> {
        let mut pads: Vec<String> = members
            .iter()
            .map(|&i| &items[i])
            .filter(|m| m.required)
            .map(|m| format!("{}.{}", m.owner, m.pad))
            .collect();
        pads.sort();
        pads.dedup();
        pads
    };
    let mut out = CopperConnectivity {
        routed: (0..items.len()).any(|i| live(i) && items[i].kind != Some(ItemKind::Pad)),
        ..CopperConnectivity::default()
    };
    let mut net_ids: Vec<i64> = by_net.keys().copied().collect();
    net_ids.sort_unstable();
    for net in net_ids {
        let mut islands = by_net.remove(&net).unwrap_or_default();
        if islands.len() < 2 {
            continue;
        }
        // The largest island by pad count, then by size, is the net itself.
        islands.sort_by_key(|(_, m)| {
            std::cmp::Reverse((m.iter().filter(|&&i| items[i].required).count(), m.len()))
        });
        let main_pads = pads_of(&islands[0].1);
        for (_, members) in &islands[1..] {
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
            let anchor = members
                .iter()
                .map(|&i| &items[i])
                .find(|m| m.required)
                .unwrap_or(&items[members[0]]);
            out.islands.push(CopperIsland {
                net: nets.name_of(net),
                pads,
                main_pads: main_pads.clone(),
                x: anchor.at.0,
                y: anchor.at.1,
            });
        }
    }
    out.unfilled_zone_nets = unfilled.into_iter().map(|n| nets.name_of(n)).collect();
    out.unfilled_zone_nets.sort();
    Some(out)
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
/// groups (explicit, or every pad sharing a number when the footprint says
/// duplicate numbers are jumpers). Only pads already on one net are joined;
/// the tie between different nets is the short sweep's business.
fn join_footprint_links(root: &List, items: &[ItemMeta], dsu: &mut Dsu) {
    let mut pads: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    for (i, m) in items.iter().enumerate() {
        if m.kind == Some(ItemKind::Pad) && m.copper != Copper::None {
            pads.entry((m.owner.as_str(), m.pad.as_str()))
                .or_default()
                .push(i);
        }
    }
    for fp in root.find_all("footprint").chain(root.find_all("module")) {
        let owner = super::footprint_reference(fp);
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
        if fp
            .find_value("duplicate_pad_numbers_are_jumpers")
            .as_deref()
            == Some("yes")
        {
            groups.extend(
                pads.keys()
                    .filter(|(o, _)| *o == owner)
                    .map(|(_, n)| vec![(*n).to_string()]),
            );
        }
        for group in groups {
            let members: Vec<usize> = group
                .iter()
                .filter_map(|n| pads.get(&(owner.as_str(), n.as_str())))
                .flatten()
                .copied()
                .collect();
            for &a in &members {
                for &b in &members {
                    if a < b && items[a].net == items[b].net {
                        dsu.union(a, b);
                    }
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
