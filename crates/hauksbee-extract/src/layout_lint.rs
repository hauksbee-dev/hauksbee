//! Layout-level wiring checks that need more than the netlist: copper that
//! never reaches part of its net, and a layout whose connections drifted from
//! its schematic. Both report through the netlint finding shape so every
//! surface (text, `--json`, the web report, `--strict`) carries them.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use crate::drc::connectivity::copper_connectivity_from_text;
use crate::netlint::{LintCheck, LintFinding, NetLintReport, Severity};
use crate::{sample_list, Component, ExtractedBoard};

/// Island findings itemised before the rest are summarised in one line.
const MAX_ISLANDS: usize = 20;

impl ExtractedBoard {
    /// Copper-island findings for `layout_text` and, when `schematic` is given,
    /// the PCB-versus-schematic connection comparison. `self` is the layout.
    #[must_use]
    pub fn layout_lint(&self, layout_text: &str, schematic: Option<&Self>) -> NetLintReport {
        let mut report = NetLintReport::default();
        copper_islands(layout_text, &mut report.findings);
        if let Some(sch) = schematic {
            schematic_parity(self, sch, &mut report.findings);
        }
        // Findings point at this board's nets; a name the netlist spells
        // differently stays in the message only.
        let names: BTreeSet<&str> = self.nets.iter().map(|n| n.name.as_str()).collect();
        for f in &mut report.findings {
            f.nets.retain(|n| names.contains(n.as_str()));
        }
        report
    }

    /// The same-stem `.kicad_sch` beside a `.kicad_pcb`, when one exists and reads.
    #[must_use]
    pub fn companion_schematic(board_path: &Path) -> Option<Self> {
        let sch = board_path.with_extension("kicad_sch");
        let is_pcb = board_path.extension().is_some_and(|e| e == "kicad_pcb");
        (is_pcb && sch.is_file())
            .then(|| Self::from_kicad_schematic_path(&sch).ok())
            .flatten()
    }
}

const fn finding(
    severity: Severity,
    message: String,
    refs: Vec<String>,
    nets: Vec<String>,
) -> LintFinding {
    LintFinding {
        check: LintCheck::UnroutedPad,
        severity,
        message,
        refs,
        nets,
    }
}

fn refs_of(pads: &[String]) -> Vec<String> {
    let set: BTreeSet<String> = pads
        .iter()
        .map(|p| {
            p.rsplit_once('.')
                .map_or(p.as_str(), |(r, _)| r)
                .to_string()
        })
        .collect();
    set.into_iter().collect()
}

fn copper_islands(text: &str, out: &mut Vec<LintFinding>) {
    let Some(conn) = copper_connectivity_from_text(text) else {
        return;
    };
    let (with_pads, stray): (Vec<_>, Vec<_>) =
        conn.islands.iter().partition(|i| !i.pads.is_empty());
    if !conn.routed && !with_pads.is_empty() {
        // Placed pads and no copper between them: a layout not routed yet. The
        // circuit checks still run on its netlist, but boards made from it would
        // connect nothing, so this one finding blocks the ordering verdict.
        let nets: BTreeSet<String> = with_pads.iter().map(|i| i.net.clone()).collect();
        let nets: Vec<String> = nets.into_iter().collect();
        out.push(finding(
            Severity::High,
            format!(
                "the board is not routed yet, so it cannot be ordered as is: no copper joins its {} pad group(s) on {}. The circuit was checked as a netlist",
                with_pads.len(),
                sample_list(&nets)
            ),
            Vec::new(),
            nets,
        ));
        return;
    }
    for island in with_pads.iter().take(MAX_ISLANDS) {
        let rest = if island.main_pads.is_empty() {
            String::new()
        } else {
            format!(" (the rest of the net: {})", sample_list(&island.main_pads))
        };
        out.push(finding(
            Severity::High,
            format!(
                "net '{}': {} {} not joined by copper to the rest of the net{rest}, near ({:.2}, {:.2}) mm. The board as drawn leaves this connection open",
                island.net,
                sample_list(&island.pads),
                if island.pads.len() == 1 { "is" } else { "are" },
                island.x,
                island.y
            ),
            refs_of(&island.pads),
            vec![island.net.clone()],
        ));
    }
    if with_pads.len() > MAX_ISLANDS {
        let nets: BTreeSet<String> = with_pads[MAX_ISLANDS..]
            .iter()
            .map(|i| i.net.clone())
            .collect();
        let nets: Vec<String> = nets.into_iter().collect();
        out.push(finding(
            Severity::High,
            format!(
                "{} more pad groups are not joined by copper to their nets ({}); the board is largely unrouted",
                with_pads.len() - MAX_ISLANDS,
                sample_list(&nets)
            ),
            Vec::new(),
            nets,
        ));
    }
    if !stray.is_empty() {
        let nets: BTreeSet<String> = stray.iter().map(|i| i.net.clone()).collect();
        let nets: Vec<String> = nets.into_iter().collect();
        out.push(finding(
            Severity::Low,
            format!(
                "{} piece(s) of track or via copper join nothing else on their net ({}), first near ({:.2}, {:.2}) mm: leftover copper, or a route that stops short",
                stray.len(),
                sample_list(&nets),
                stray[0].x,
                stray[0].y
            ),
            Vec::new(),
            nets,
        ));
    }
    if !conn.unfilled_zone_nets.is_empty() {
        out.push(finding(
            Severity::Low,
            format!(
                "zones not filled: the copper pours on {} have no computed fill in the file, so whether their pads are connected was not checked. Refill the zones (KiCad: B) and save",
                sample_list(&conn.unfilled_zone_nets)
            ),
            Vec::new(),
            conn.unfilled_zone_nets,
        ));
    }
}

/// A pin's net on one side: a named net, or (no net) a set of its own. A net
/// is told apart by its id, not its name: a schematic read can carry two nets
/// of one name (a sheet-local label and a global label of the same text), and
/// keyed by name they would read as one net the layout keeps split.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Side {
    Net(bool, i64, String),
    Alone(String),
}

impl Side {
    fn label(&self) -> String {
        match self {
            Self::Net(_, _, n) => format!("'{n}'"),
            Self::Alone(_) => "no net".to_string(),
        }
    }
}

/// Each compared pin's (schematic, layout) net, keyed and ordered by
/// "REF.pad", and how many parts were left out of the comparison.
fn pin_sides(
    pcb: &ExtractedBoard,
    sch: &ExtractedBoard,
) -> (BTreeMap<String, (Side, Side)>, usize) {
    let fitted = |b: &ExtractedBoard| -> HashMap<String, Component> {
        b.components
            .iter()
            .filter(|c| !crate::dnp::unbought(c))
            .map(|c| (c.reference.clone(), c.clone()))
            .collect()
    };
    let (pcb_parts, sch_parts) = (fitted(pcb), fitted(sch));
    let all_refs: BTreeSet<&String> = pcb
        .components
        .iter()
        .chain(&sch.components)
        .map(|c| &c.reference)
        .collect();
    let side = |b: &ExtractedBoard, id: Option<i64>, pin: &str, layout: bool| -> Side {
        id.and_then(|id| b.nets.iter().find(|n| n.id == id))
            .filter(|n| n.id != 0 && !n.name.is_empty())
            .map_or_else(
                || Side::Alone(pin.to_string()),
                |n| Side::Net(layout, n.id, n.name.clone()),
            )
    };
    let mut pins: BTreeMap<String, (Side, Side)> = BTreeMap::new();
    let mut conflicted: BTreeSet<String> = BTreeSet::new();
    let mut compared = 0usize;
    for (reference, pc) in &pcb_parts {
        let Some(sc) = sch_parts.get(reference) else {
            continue;
        };
        compared += 1;
        for pad in &pc.pins {
            let key = format!("{reference}.{}", pad.number);
            let Some(sp) = sc.pins.iter().find(|p| p.number == pad.number) else {
                continue;
            };
            let p = side(pcb, pad.net, &key, true);
            // A pad number the layout repeats (a tab, a split pad) takes the
            // net its copies agree on; copies on different nets are skipped.
            match pins.get_mut(&key) {
                None => {
                    pins.insert(key.clone(), (side(sch, sp.net, &key, false), p));
                }
                Some((_, old)) if matches!(old, Side::Alone(_)) => *old = p,
                Some((_, old)) if *old != p && !matches!(p, Side::Alone(_)) => {
                    conflicted.insert(key);
                }
                Some(_) => {}
            }
        }
    }
    for key in &conflicted {
        pins.remove(key);
    }
    (pins, all_refs.len() - compared)
}

/// Union schematic nets and layout nets that share a pin; each component of
/// that graph is one group of pins (a group holding more than one net of
/// either side is a mismatch).
fn pin_groups(pins: &BTreeMap<String, (Side, Side)>) -> Vec<Vec<&String>> {
    let mut ids: BTreeMap<&Side, usize> = BTreeMap::new();
    for (s, p) in pins.values() {
        for side in [s, p] {
            let n = ids.len();
            ids.entry(side).or_insert(n);
        }
    }
    let mut dsu = crate::dsu::Dsu::new(ids.len());
    for (s, p) in pins.values() {
        dsu.union(ids[s], ids[p]);
    }
    let mut groups: BTreeMap<usize, Vec<&String>> = BTreeMap::new();
    for (key, (s, _)) in pins {
        groups.entry(dsu.find(ids[s])).or_default().push(key);
    }
    groups.into_values().collect()
}

/// Compare the net partitions of a layout and its schematic, pin by pin.
///
/// Only parts fitted on both sides are compared; a net renamed but holding the
/// same pins is the same connection. Each group of pins whose joins differ is
/// one finding naming the pins that moved.
fn schematic_parity(pcb: &ExtractedBoard, sch: &ExtractedBoard, out: &mut Vec<LintFinding>) {
    let (pins, skipped) = pin_sides(pcb, sch);
    let groups = pin_groups(&pins);
    let members = |side: &Side, of_layout: bool| -> Vec<&String> {
        pins.iter()
            .filter(|(_, (s, p))| if of_layout { p == side } else { s == side })
            .map(|(k, _)| k)
            .collect()
    };
    // The layout's net names were written from the schematic at its last
    // update, so a schematic net whose name the layout does not carry means
    // this reading of the schematic, not the layout, may be the odd one out
    // (a reused sheet, a name spelled differently): such groups are not
    // judged. A pin with no net on the schematic side needs no name.
    let layout_names: BTreeSet<&str> = pcb.nets.iter().map(|n| n.name.as_str()).collect();
    let known = |side: &Side| match side {
        Side::Net(_, _, name) => layout_names.contains(name.as_str()),
        Side::Alone(_) => true,
    };
    let mut first = true;
    for keys in &groups {
        let sch_nets: BTreeSet<&Side> = keys.iter().map(|k| &pins[*k].0).collect();
        let pcb_nets: BTreeSet<&Side> = keys.iter().map(|k| &pins[*k].1).collect();
        if (sch_nets.len() < 2 && pcb_nets.len() < 2) || !sch_nets.iter().all(|s| known(s)) {
            continue;
        }
        // Each net's partner is the other side's net holding most of its pins;
        // a pin off its net's partner is one that moved.
        let partner = |side: &Side, of_layout: bool| -> Option<Side> {
            let mut count: BTreeMap<&Side, usize> = BTreeMap::new();
            for k in members(side, of_layout) {
                let (s, p) = &pins[k];
                *count.entry(if of_layout { s } else { p }).or_default() += 1;
            }
            count
                .into_iter()
                .max_by_key(|(n, c)| (*c, std::cmp::Reverse(*n)))
                .map(|(n, _)| n.clone())
        };
        let moved: Vec<&String> = keys
            .iter()
            .copied()
            .filter(|k| {
                let (s, p) = &pins[*k];
                partner(s, false).as_ref() != Some(p) || partner(p, true).as_ref() != Some(s)
            })
            .collect();
        let Some(&pin) = moved.first() else {
            continue;
        };
        let (s, p) = &pins[pin];
        let with = |side: &Side, of_layout: bool| -> String {
            let others: Vec<String> = members(side, of_layout)
                .into_iter()
                .filter(|k| *k != pin)
                .cloned()
                .collect();
            if others.is_empty() {
                format!("{} with no other pin", side.label())
            } else {
                format!("{} with {}", side.label(), sample_list(&others))
            }
        };
        let also = if moved.len() > 1 {
            let rest: Vec<String> = moved[1..].iter().map(|k| (*k).clone()).collect();
            format!(" Also differing in this group: {}.", sample_list(&rest))
        } else {
            String::new()
        };
        let note = if first && skipped > 0 {
            format!(" ({skipped} part(s) not fitted, or on only one side, were not compared.)")
        } else {
            String::new()
        };
        first = false;
        let moved_owned: Vec<String> = moved.iter().map(|k| (*k).clone()).collect();
        // Only layout names: the finding is located on the layout's nets.
        let nets: Vec<String> = pcb_nets
            .iter()
            .filter_map(|n| match n {
                Side::Net(_, _, name) => Some(name.clone()),
                Side::Alone(_) => None,
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        out.push(LintFinding {
            check: LintCheck::PcbSchematicMismatch,
            severity: Severity::High,
            message: format!(
                "{pin} is connected differently on the PCB than in the schematic: the schematic puts it on {}, the PCB on {}.{also} The layout is out of date with its schematic (or was edited by hand); update the PCB from the schematic and re-route.{note}",
                with(s, false),
                with(p, true)
            ),
            refs: refs_of(&moved_owned),
            nets,
        });
    }
}
