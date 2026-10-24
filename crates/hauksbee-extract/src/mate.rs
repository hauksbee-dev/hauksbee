//! Several boards that mate through board-to-board connectors, read as one.
//!
//! A carrier with potentiometers and a breakout with the MCU are two design
//! files. Checked one at a time, the carrier has no MCU and the breakout has no
//! source, so a signal that meets the wrong pad only in the assembled product is
//! never seen. [`assemble`] joins the boards into the one netlist the product is:
//! the connectors that mate are found (or named), the nets on their matching pins
//! are merged, and every check then runs on the whole.
//!
//! Two connectors mate when they are on different boards, have the same set of
//! pin numbers (at least two), and their footprints agree on the geometry they
//! state (rows x columns, pitch). Pin `n` of one meets pin `n` of the other: a
//! header and its socket number their contacts the same way. The pairing must be
//! unambiguous; a connector with two possible partners is reported and left
//! alone, because a guessed mate is a wrong netlist. A pair the caller names
//! explicitly is mated whatever the footprints say.
//!
//! Designators that collide across boards (`J1`, `R1`) are made unique with the
//! board label as a suffix (`J1_breakout`), keeping the letters a check reads a
//! part's class from.

use std::collections::{BTreeMap, BTreeSet};

use crate::{is_connector_like, Component, ExtractedBoard, Net};

/// One mate that [`assemble`] made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mate {
    /// The connector on the earlier board, by its designator in the result.
    pub a: String,
    /// The connector on the later board, by its designator in the result.
    pub b: String,
    /// How many pin numbers the two connectors share.
    pub pins: usize,
}

/// The boards joined, and what the join decided.
#[derive(Debug, Clone)]
pub struct Assembled {
    /// The one netlist of the assembled product.
    pub board: ExtractedBoard,
    /// The mates that were made.
    pub mates: Vec<Mate>,
    /// Whole-sentence notes: connectors left unmated, nets joined under two names.
    pub notes: Vec<String>,
}

/// A mate the caller names: `(board label, designator)` on each side.
pub type NamedMate = ((String, String), (String, String));

/// A footprint's stated geometry: `rows x columns` and pitch, lower case.
fn geometry(footprint: &str) -> (Option<String>, Option<String>) {
    let lower = footprint.to_ascii_lowercase();
    let rows = regex::Regex::new(r"\d+x\d+")
        .ok()
        .and_then(|re| re.find(&lower).map(|m| m.as_str().to_owned()));
    let pitch = regex::Regex::new(r"p\d+(\.\d+)?mm")
        .ok()
        .and_then(|re| re.find(&lower).map(|m| m.as_str().to_owned()));
    (rows, pitch)
}

/// Whether two connector footprints can be a mating pair.
fn footprints_fit(a: &Component, b: &Component) -> bool {
    let (ga, gb) = (geometry(&a.footprint), geometry(&b.footprint));
    if ga.0.is_some() || gb.0.is_some() || ga.1.is_some() || gb.1.is_some() {
        return ga == gb;
    }
    // Neither footprint states a geometry: the same library part is the evidence.
    !a.lib_id.trim().is_empty() && a.lib_id == b.lib_id
}

/// The distinct, non-empty pad numbers of `c`.
fn pad_set(c: &Component) -> BTreeSet<&str> {
    c.pins
        .iter()
        .map(|p| p.number.trim())
        .filter(|n| !n.is_empty())
        .collect()
}

/// A net name a tool made up (`Net-(R1-Pad1)`, `unconnected-(J1-Pin_3)`).
fn is_auto_name(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    n.is_empty() || n.starts_with("net-(") || n.starts_with("unconnected")
}

/// Union-find over net slots; the lower slot stays the root, so the earlier
/// board's net keeps its name.
struct Dsu(Vec<usize>);

impl Dsu {
    fn find(&mut self, x: usize) -> usize {
        if self.0[x] != x {
            let root = self.find(self.0[x]);
            self.0[x] = root;
        }
        self.0[x]
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra < rb {
            self.0[rb] = ra;
        } else {
            self.0[ra] = rb;
        }
    }
}

/// Connector pairs `(a, b)` (indices into `parts`, `a` on the earlier board) that
/// mate unambiguously, plus the notes for connectors with several candidates.
fn find_pairs(
    parts: &[(usize, Component)],
    skip: &BTreeSet<usize>,
    notes: &mut Vec<String>,
) -> Vec<(usize, usize)> {
    let connectors: Vec<usize> = (0..parts.len())
        .filter(|k| !skip.contains(k))
        .filter(|&k| is_connector_like(&parts[k].1) && pad_set(&parts[k].1).len() >= 2)
        .collect();
    let mut partners: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for &a in &connectors {
        for &b in &connectors {
            if parts[a].0 < parts[b].0
                && pad_set(&parts[a].1) == pad_set(&parts[b].1)
                && footprints_fit(&parts[a].1, &parts[b].1)
            {
                partners.entry(a).or_default().push(b);
                partners.entry(b).or_default().push(a);
            }
        }
    }
    let mut pairs = Vec::new();
    for (&a, bs) in &partners {
        match bs[..] {
            [b] if partners[&b].len() == 1 => {
                if parts[a].0 < parts[b].0 {
                    pairs.push((a, b));
                }
            }
            _ => notes.push(format!(
                "{} could mate with more than one connector ({}), so it was left unmated: name the mate to join them.",
                parts[a].1.reference,
                bs.iter()
                    .map(|&b| parts[b].1.reference.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
    pairs
}

/// The mates the caller names, as part-index pairs, and the connectors they use.
fn named_pairs(
    boards: &[(String, ExtractedBoard)],
    parts: &[(usize, Component)],
    named: &[NamedMate],
    notes: &mut Vec<String>,
) -> (Vec<(usize, usize)>, BTreeSet<usize>) {
    let mut pairs = Vec::new();
    let mut forced = BTreeSet::new();
    for ((la, ra), (lb, rb)) in named {
        let find = |label: &str, r: &str| {
            let board = boards.iter().position(|(l, _)| l == label)?;
            parts.iter().position(|(i, c)| {
                *i == board && (c.reference == r || c.reference == format!("{r}_{label}"))
            })
        };
        match (find(la, ra), find(lb, rb)) {
            (Some(a), Some(b)) if parts[a].0 != parts[b].0 => {
                pairs.push((a.min(b), a.max(b)));
                forced.insert(a);
                forced.insert(b);
            }
            _ => notes.push(format!(
                "The named mate {la}:{ra} with {lb}:{rb} matches no pair of connectors on two boards, so it was not made."
            )),
        }
    }
    (pairs, forced)
}

/// Merge the nets of each mated pair on matching pin numbers; one `Mate` per pair.
fn merge_pairs(
    pairs: &[(usize, usize)],
    parts: &[(usize, Component)],
    slot_of: &[BTreeMap<i64, usize>],
    slot_names: &[String],
    dsu: &mut Dsu,
    notes: &mut Vec<String>,
) -> Vec<Mate> {
    let mut mates = Vec::new();
    for &(a, b) in pairs {
        let (ia, ca) = (parts[a].0, &parts[a].1);
        let (ib, cb) = (parts[b].0, &parts[b].1);
        let shared = pad_set(ca).intersection(&pad_set(cb)).count();
        for pa in &ca.pins {
            let Some(pb) = cb.pins.iter().find(|p| p.number.trim() == pa.number.trim()) else {
                continue;
            };
            let slot = |net: Option<i64>, board: usize| {
                net.filter(|n| *n != 0)
                    .and_then(|n| slot_of[board].get(&n).copied())
            };
            let (Some(na), Some(nb)) = (slot(pa.net, ia), slot(pb.net, ib)) else {
                continue;
            };
            let (ra, rb) = (dsu.find(na), dsu.find(nb));
            if ra != rb
                && !is_auto_name(&slot_names[ra])
                && !is_auto_name(&slot_names[rb])
                && slot_names[ra] != slot_names[rb]
            {
                notes.push(format!(
                    "Nets '{}' and '{}' are one net in the assembly: {} pin {} meets {} pin {}.",
                    slot_names[ra],
                    slot_names[rb],
                    ca.reference,
                    pa.number,
                    cb.reference,
                    pb.number
                ));
            }
            dsu.union(na, nb);
        }
        mates.push(Mate {
            a: ca.reference.clone(),
            b: cb.reference.clone(),
            pins: shared,
        });
    }
    mates
}

/// Join `boards` (each with the label it goes by) into one netlist.
///
/// `named` forces mates the footprints would not find. A name that matches no
/// connector is a note, not an error.
#[must_use]
pub fn assemble(boards: &[(String, ExtractedBoard)], named: &[NamedMate]) -> Assembled {
    let mut notes = Vec::new();
    // Net slots: one per (board, net).
    let mut slot_of: Vec<BTreeMap<i64, usize>> = Vec::new();
    let mut slot_names: Vec<String> = Vec::new();
    for (_, b) in boards {
        let mut map = BTreeMap::new();
        for n in &b.nets {
            map.insert(n.id, slot_names.len());
            slot_names.push(n.name.clone());
        }
        slot_of.push(map);
    }
    let mut dsu = Dsu((0..slot_names.len()).collect());

    // Unique designators.
    let mut taken: BTreeSet<String> = BTreeSet::new();
    let mut parts: Vec<(usize, Component)> = Vec::new();
    for (i, (label, b)) in boards.iter().enumerate() {
        for c in &b.components {
            let mut c = c.clone();
            if taken.contains(&c.reference) {
                c.reference = format!("{}_{label}", c.reference);
            }
            taken.insert(c.reference.clone());
            parts.push((i, c));
        }
    }

    // The mates the caller names, then the ones the footprints find.
    let (mut pairs, forced) = named_pairs(boards, &parts, named, &mut notes);
    pairs.extend(find_pairs(&parts, &forced, &mut notes));
    pairs.sort_unstable();
    pairs.dedup();

    let mates = merge_pairs(&pairs, &parts, &slot_of, &slot_names, &mut dsu, &mut notes);

    // The merged nets: the root's name unless a later member has a real one.
    let mut name_of_root: BTreeMap<usize, String> = BTreeMap::new();
    for (slot, slot_name) in slot_names.iter().enumerate() {
        let root = dsu.find(slot);
        let entry = name_of_root
            .entry(root)
            .or_insert_with(|| slot_name.clone());
        if is_auto_name(entry) && !is_auto_name(slot_name) {
            slot_name.clone_into(entry);
        }
    }
    let mut root_id: BTreeMap<usize, i64> = BTreeMap::new();
    let mut nets: Vec<Net> = Vec::new();
    for (root, name) in &name_of_root {
        let id = i64::try_from(nets.len() + 1).unwrap_or(i64::MAX);
        root_id.insert(*root, id);
        nets.push(Net::new(id, name.clone()));
    }
    let mut components = Vec::new();
    for (i, mut c) in parts {
        for p in &mut c.pins {
            p.net = p.net.map(|n| {
                if n == 0 {
                    0
                } else {
                    slot_of[i].get(&n).map_or(0, |s| root_id[&dsu.find(*s)])
                }
            });
        }
        components.push(c);
    }

    if boards.len() > 1 && mates.is_empty() {
        notes.push(format!(
            "No connectors on the {} boards mate (same pin numbers and footprint geometry), so they are joined as separate islands: the checks see no signal cross between them.",
            boards.len()
        ));
    }
    let name = boards
        .iter()
        .map(|(l, _)| l.as_str())
        .collect::<Vec<_>>()
        .join("+");
    Assembled {
        board: ExtractedBoard {
            name,
            nets,
            components,
        },
        mates,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{board, part, pin};

    const HEADER: &str = "Connector_PinHeader_2.54mm:PinHeader_2x05_P2.54mm_Vertical";

    /// A 10-pin header whose pin `n` is on `nets[n - 1]` (0 is unconnected).
    fn header(reference: &str, footprint: &str, nets: &[i64]) -> Component {
        let pins = nets
            .iter()
            .enumerate()
            .map(|(i, &n)| pin(&(i + 1).to_string(), (n != 0).then_some(n)))
            .collect();
        part(
            reference,
            "Conn_02x05",
            "Connector:Conn_02x05",
            footprint,
            pins,
        )
    }

    fn net_of(b: &ExtractedBoard, reference: &str, pad: &str) -> String {
        let c = b
            .components
            .iter()
            .find(|c| c.reference == reference)
            .unwrap();
        let id = c
            .pins
            .iter()
            .find(|p| p.number == pad)
            .unwrap()
            .net
            .unwrap();
        b.net(id).unwrap().name.clone()
    }

    /// A carrier with a pot wiper on header pin 3, and a breakout whose header
    /// pin 3 reaches the MCU pad net.
    fn pair() -> Vec<(String, ExtractedBoard)> {
        let carrier = board(
            &[(1, "GND"), (2, "+3V3"), (3, "WIPER")],
            vec![
                header("J1", HEADER, &[1, 2, 3, 0, 0, 0, 0, 0, 0, 0]),
                part(
                    "RV1",
                    "10k",
                    "Device:R_Potentiometer",
                    "",
                    vec![pin("1", Some(2)), pin("2", Some(3)), pin("3", Some(1))],
                ),
            ],
        );
        let breakout = board(
            &[(1, "GND"), (2, "VCC"), (3, "A_IN")],
            vec![
                header("J1", HEADER, &[1, 2, 3, 0, 0, 0, 0, 0, 0, 0]),
                part(
                    "U1",
                    "ATmega328P",
                    "MCU:ATmega328P",
                    "",
                    vec![pin("4", Some(3))],
                ),
            ],
        );
        vec![("carrier".into(), carrier), ("breakout".into(), breakout)]
    }

    #[test]
    fn a_header_pair_with_the_same_pins_and_geometry_mates_and_merges_the_nets() {
        let a = assemble(&pair(), &[]);
        assert_eq!(a.mates.len(), 1, "{:?}", a.notes);
        assert_eq!(a.mates[0].pins, 10);
        // The colliding J1 is suffixed with its board; the carrier's name wins.
        assert_eq!(a.mates[0].a, "J1");
        assert_eq!(a.mates[0].b, "J1_breakout");
        assert_eq!(net_of(&a.board, "U1", "4"), "WIPER");
        assert_eq!(net_of(&a.board, "RV1", "2"), "WIPER");
        assert_eq!(a.board.name, "carrier+breakout");
    }

    #[test]
    fn connectors_with_different_pitch_or_pin_sets_do_not_mate() {
        let mut boards = pair();
        boards[1].1.components[0].footprint =
            "Connector_PinHeader_1.27mm:PinHeader_2x05_P1.27mm_Vertical".into();
        let a = assemble(&boards, &[]);
        assert!(a.mates.is_empty());
        assert!(
            a.notes.iter().any(|n| n.contains("No connectors")),
            "{:?}",
            a.notes
        );
        assert_ne!(net_of(&a.board, "U1", "4"), "WIPER");

        let mut boards = pair();
        boards[1].1.components[0].pins.truncate(8);
        assert!(assemble(&boards, &[]).mates.is_empty());
    }

    #[test]
    fn a_connector_with_two_possible_partners_is_left_alone_and_said_so() {
        let mut boards = pair();
        let extra = board(
            &[(1, "GND"), (2, "X"), (3, "Y")],
            vec![header("J9", HEADER, &[1, 2, 3, 0, 0, 0, 0, 0, 0, 0])],
        );
        boards.push(("third".into(), extra));
        let a = assemble(&boards, &[]);
        assert!(a.mates.is_empty(), "{:?}", a.mates);
        assert!(
            a.notes
                .iter()
                .any(|n| n.contains("more than one connector")),
            "{:?}",
            a.notes
        );
    }

    #[test]
    fn a_named_mate_joins_connectors_the_footprints_would_not() {
        let mut boards = pair();
        boards[1].1.components[0].footprint = "Custom:Edge".into();
        boards[1].1.components[0].lib_id = "Custom:Edge".into();
        let named = [(
            ("carrier".to_owned(), "J1".to_owned()),
            ("breakout".to_owned(), "J1".to_owned()),
        )];
        let a = assemble(&boards, &named);
        assert_eq!(a.mates.len(), 1, "{:?}", a.notes);
        assert_eq!(net_of(&a.board, "U1", "4"), "WIPER");
    }

    #[test]
    fn a_named_mate_that_matches_nothing_is_a_note_not_an_error() {
        let named = [(
            ("carrier".to_owned(), "J7".to_owned()),
            ("breakout".to_owned(), "J1".to_owned()),
        )];
        let a = assemble(&pair(), &named);
        assert!(a.notes.iter().any(|n| n.contains("J7")), "{:?}", a.notes);
        // The footprints still find the real pair.
        assert_eq!(a.mates.len(), 1);
    }

    #[test]
    fn a_single_board_assembles_to_itself() {
        let one = pair().remove(0);
        let a = assemble(&[one], &[]);
        assert!(a.mates.is_empty());
        assert!(a.notes.is_empty(), "{:?}", a.notes);
        assert_eq!(a.board.components.len(), 2);
    }
}
