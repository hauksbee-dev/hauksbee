//! Layout-level wiring checks that need more than the netlist: copper that
//! never reaches part of its net, and a layout whose connections drifted from
//! its schematic. Both report through the netlint finding shape so every
//! surface (text, `--json`, the web report, `--strict`) carries them.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use crate::drc::connectivity::{copper_connectivity_from_text, CopperIsland};
use crate::netlint::{LintCheck, LintFinding, NetLintReport, Severity};
use crate::{sample_list, Component, ExtractedBoard};

/// What the finding for a layout with placed pads and no copper at all begins
/// with; the reports key their "not routed yet" state off it.
pub const NOT_ROUTED_MESSAGE_PREFIX: &str = "the board is not routed yet";

/// The remedy for that whole-board state (not the single open connection of a
/// routed board).
pub const NOT_ROUTED_FIX: &str = "Route all the nets, pour and refill the zones, then run the layout editor's DRC until it reports no unconnected items.";

/// What the note for a panel of repeated copies begins with.
///
/// The copies share designators and net names, so `KiCad` joins the same-named
/// nets across them and draws the ratsnest between copies; this tool judges
/// each copy alone and says so in one note.
pub const PANEL_COPIES_MESSAGE_PREFIX: &str = "panel of repeated copies";

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
            part_identity_parity(self, sch, &mut report.findings);
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

/// The footprint setting to change, when an open pad has a twin on the other side.
///
/// Pads of one footprint that share a number (a connector shell's tabs, a
/// split thermal pad) appear on both sides under one `REF.pad` label. `KiCad`
/// joins them only when the footprint says so, and reports the same
/// unconnected items otherwise.
fn same_number_hint(island: &CopperIsland) -> String {
    (island.pads.iter())
        .find(|p| island.main_pads.contains(p))
        .map_or_else(String::new, |p| {
            format!(
                ". {p} is also on the other side: pads of one part that share a number are \
                 joined only if the footprint has \"pads with the same number are jumpers\" \
                 set (KiCad reports the same unconnected items without it); set it if the \
                 part's own metal joins them"
            )
        })
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
                "{NOT_ROUTED_MESSAGE_PREFIX}, so it cannot be ordered as is: no copper joins its {} pad group(s)",
                with_pads.len()
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
        let twin = same_number_hint(island);
        out.push(finding(
            Severity::High,
            format!(
                "net '{}': {} {} not joined by copper to the rest of the net{rest}, near ({:.2}, {:.2}) mm. The board as drawn leaves this connection open{twin}",
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
    if let Some(panel) = &conn.panel {
        out.push(finding(
            Severity::Low,
            format!(
                "{PANEL_COPIES_MESSAGE_PREFIX}: this board repeats one circuit in {} copies with the same designators and net names, and the copies are not joined to each other. {} net(s) ({}) are one same-named net across the copies, which KiCad shows as unconnected items between copies. Each copy was checked for open connections on its own",
                panel.copies,
                panel.split_nets.len(),
                sample_list(&panel.split_nets)
            ),
            Vec::new(),
            panel.split_nets.clone(),
        ));
    }
    if !conn.unfilled_zone_nets.is_empty() {
        out.push(finding(
            Severity::Low,
            format!(
                "{}: the copper pours on {} have no computed fill in the file, so whether their pads are connected was not checked. Refill the zones (KiCad: B) and save",
                crate::netlint::ZONES_NOT_FILLED_PREFIX,
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

/// A two-terminal part whose ends are interchangeable: a resistor, an
/// inductor, or a two-pin switch. Exchanging its pads changes no circuit.
fn interchangeable_ends(c: &Component) -> bool {
    crate::part_class::connected_pads(c) == 2
        && (crate::is_plain_resistor(c)
            || crate::is_plain_inductor(c)
            || crate::part_class::ref_designator(&c.reference).starts_with("SW"))
}

/// A layout that lands an interchangeable part's two pads on each other's
/// schematic nets (a footprint numbered the other way round) wires the same
/// circuit: read its layout sides back in schematic order.
fn unswap_interchangeable_pins(pcb: &ExtractedBoard, pins: &mut BTreeMap<String, (Side, Side)>) {
    for c in pcb.components.iter().filter(|c| interchangeable_ends(c)) {
        let prefix = format!("{}.", c.reference);
        let keys: Vec<String> = pins
            .keys()
            .filter(|k| k.strip_prefix(&prefix).is_some_and(|pad| !pad.is_empty()))
            .cloned()
            .collect();
        let [a, b] = keys.as_slice() else { continue };
        let ((sa, pa), (sb, pb)) = (&pins[a], &pins[b]);
        let same_net = |x: &Side, y: &Side| match (x, y) {
            (Side::Net(_, _, m), Side::Net(_, _, n)) => m == n,
            _ => false,
        };
        if sa != sb && same_net(sa, pb) && same_net(sb, pa) && !same_net(sa, pa) {
            let (pa, pb) = (pa.clone(), pb.clone());
            pins.get_mut(a).expect("key listed").1 = pb;
            pins.get_mut(b).expect("key listed").1 = pa;
        }
    }
}

/// Compare the net partitions of a layout and its schematic, pin by pin.
///
/// Only parts fitted on both sides are compared; a net renamed but holding the
/// same pins is the same connection. Each group of pins whose joins differ is
/// one finding naming the pins that moved.
fn schematic_parity(pcb: &ExtractedBoard, sch: &ExtractedBoard, out: &mut Vec<LintFinding>) {
    let (mut pins, skipped) = pin_sides(pcb, sch);
    unswap_interchangeable_pins(pcb, &mut pins);
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

/// A part identity compared across the two files: lower case, whitespace
/// dropped, so "10 k" and "10K" agree.
fn identity_key(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// A part's manufacturer part number property, when it carries one.
fn mpn_property(c: &Component) -> Option<&str> {
    c.properties
        .iter()
        .find(|(k, v)| crate::bom::is_mpn_property_key(k) && !v.trim().is_empty())
        .map(|(_, v)| v.trim())
}

/// What comparing one field of a reference across the two files concluded.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// The two sides name the same thing (or one side is blank).
    Agree,
    /// They cannot be shown to differ, but cannot be shown to agree either:
    /// descriptive text, a distributor code, or the same part with different
    /// ordering/packaging or an extra unrecognised spec. A low-severity note.
    Unsure(String),
    /// Both sides name a different electrical value or a different part: the
    /// verdict analysed the layout's, so this is a check-before-ordering item.
    Differs,
}

/// The primary quantity of a value field: its leading token read by the
/// project's one value parser, and the text left over after that token.
struct Primary {
    si: f64,
    /// `Some` unit with the ohm spellings folded; `None` for a bare number,
    /// which is a wildcard (`10n` and `10nF` are the same capacitor).
    unit: Option<String>,
    rest: String,
}

/// `token` (a bare number ending at byte `end` of `s`) joined to the word after
/// it, when that word is a unit or SI-prefixed unit: the parse of the pair and
/// the text after it. `None` when the next word is a number or a rating word.
fn unit_in_next_word<'a>(
    s: &'a str,
    token: &str,
    end: usize,
) -> Option<(hauksbee_models::value::ParsedValue, &'a str)> {
    let after = s[end..].trim_start();
    let word_end = after
        .find(|c: char| c.is_whitespace() || c == '/')
        .unwrap_or(after.len());
    let word = after[..word_end].trim_end_matches(',');
    // A unit word starts with a letter or Ω/µ; "25V" and "1%" are ratings.
    if !word.chars().next().is_some_and(char::is_alphabetic) || is_spec_token(word) {
        return None;
    }
    let joined = hauksbee_models::value::parse_value(&format!("{token}{word}"))?;
    let consumed = s.len() - after.len() + word_end;
    Some((joined, &s[consumed..]))
}

fn primary(s: &str) -> Option<Primary> {
    let s = s.trim();
    // The first token ends at a space or a `/`: "100uF 50V", "100uF/50V".
    let end = s
        .find(|c: char| c.is_whitespace() || c == '/')
        .unwrap_or(s.len());
    let token = s[..end].trim_end_matches(',');
    let (parsed, rest) = match hauksbee_models::value::parse_value(token) {
        // "220 pF", "1.78 kΩ", "100 nF": the unit stands in the next word. A
        // bare number followed by a word that makes it a quantity is one
        // quantity, read whole, so the SI prefix is not lost to a space.
        Some(p) if p.unit.is_none() => {
            unit_in_next_word(s, token, end).unwrap_or_else(|| (p, &s[end..]))
        }
        Some(p) => (p, &s[end..]),
        // A multi-token string the parser reads whole ("4,7 k") keeps no rest.
        None => (hauksbee_models::value::parse_value(s)?, ""),
    };
    let unit = parsed.unit.map(|u| match u.as_str() {
        "R" | "OHM" | "ohm" => "\u{03a9}".to_string(),
        _ => u,
    });
    Some(Primary {
        si: parsed.si,
        unit,
        rest: rest.to_string(),
    })
}

/// Dielectric, temperature-class and chip-size words that qualify a passive.
const SPEC_WORDS: [&str; 22] = [
    "c0g", "np0", "npo", "x5r", "x6s", "x7r", "x7s", "x7t", "x8r", "y5v", "z5u", "u2j", "0201",
    "0402", "0603", "0805", "1206", "1210", "1812", "2010", "2512", "smd",
];

/// Words that describe a rating, tolerance, dielectric, power or chip size
/// rather than the part's primary quantity. They may sit on one side only.
fn is_spec_token(token: &str) -> bool {
    let t = token
        .trim_matches(|c: char| matches!(c, ',' | ';' | '(' | ')' | '\u{b1}'))
        .to_ascii_lowercase();
    if t.is_empty() {
        return true;
    }
    if t.ends_with('%') || t.ends_with("ppm") {
        return true;
    }
    if SPEC_WORDS.contains(&t.as_str()) {
        return true;
    }
    // A number with an electrical rating unit: 50v, 2a, 750ma, 0.25w, 100vdc.
    let digits = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    digits > 0
        && matches!(
            &t[digits..],
            "" | "v" | "mv" | "kv" | "a" | "ma" | "ua" | "w" | "mw" | "vdc" | "vac" | "vrms"
        )
}

/// The tokens of `rest` that are not a recognised rating/tolerance/dielectric.
fn unrecognised_extras(rest: &str) -> BTreeSet<String> {
    rest.split(|c: char| c.is_whitespace() || c == '/')
        .filter(|t| !is_spec_token(t))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// A part number reduced for comparison: upper case, no whitespace, and the
/// `#...` lead-free/packaging tail some vendors append dropped.
pub fn normalise_mpn(s: &str) -> String {
    let s: String = s
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_uppercase)
        .collect();
    s.split('#').next().unwrap_or("").to_string()
}

/// Does this text read as a single manufacturer part number: one token with a
/// letter and a digit that no passive-value reading explains, and not a
/// distributor's own order code?
pub fn is_part_number(s: &str) -> bool {
    let s = s.trim();
    s.len() >= 5
        && !s.contains(char::is_whitespace)
        && s.chars().any(|c| c.is_ascii_alphabetic())
        && s.chars().any(|c| c.is_ascii_digit())
        && !crate::bom::is_distributor_code(s)
        && primary(s).is_none()
}

/// Compare two part numbers. The boundary is the first `-` (or `/` or `,`):
/// what precedes it is the base part with its variant letters (`CAT24M01YI`),
/// what follows is the ordering/packaging tail (`-GT3`, `-T2`). Heads that
/// differ are different parts, because variant letters before the hyphen carry
/// temperature grade, interface or function (`...YI` vs `...XI`,
/// `CD74HCT640` inverting vs `74HCT245` not). Equal heads with different tails
/// are the same silicon in a different reel, tube or lead finish, and so only a
/// note. A tail-less number may also differ from its twin only by one trailing
/// reel code (`R`/`T`/`TR`: `...DBVR` vs `...DBVT`), accepted when at least six
/// characters precede it so a short number cannot lose its last letter.
fn compare_part_numbers(sch: &str, pcb: &str) -> Verdict {
    let (a, b) = (normalise_mpn(sch), normalise_mpn(pcb));
    if a == b {
        return Verdict::Agree;
    }
    let split = |s: &str| -> (String, String) {
        let i = s.find(['-', '/', ',']).unwrap_or(s.len());
        (s[..i].to_string(), s[i..].to_string())
    };
    let reel_stripped = |h: &str| -> String {
        for tail in ["TR", "R", "T"] {
            if let Some(base) = h.strip_suffix(tail) {
                if base.len() >= 6 {
                    return base.to_string();
                }
            }
        }
        h.to_string()
    };
    let ((ha, ta), (hb, tb)) = (split(&a), split(&b));
    if ha == hb || reel_stripped(&ha) == reel_stripped(&hb) {
        let describe = |t: &str| {
            if t.is_empty() {
                "no suffix".to_string()
            } else {
                format!("suffix '{}'", t.trim_start_matches(['-', '/', ',']))
            }
        };
        return Verdict::Unsure(format!(
            "the same base part with different ordering or packaging ({} versus {})",
            describe(&ta),
            describe(&tb)
        ));
    }
    Verdict::Differs
}

/// Compare the two value fields by what they mean electrically.
///
/// * Both lead with a passive magnitude: different magnitude (or an explicit
///   F against H) is a disagreement; the same magnitude with ratings,
///   tolerance or dielectric on one side only is not even a note; any other
///   extra word on one side only (a switch, a taper) is a note naming it.
/// * Both are part numbers: [`compare_part_numbers`].
/// * Anything else (a description, a distributor code, `?`, a bare frequency
///   against a description, a passive against a part number) cannot be
///   compared, so it is a note naming both strings and never gates.
fn compare_values(sch: &str, pcb: &str) -> Verdict {
    if identity_key(sch) == identity_key(pcb) {
        return Verdict::Agree;
    }
    if let (Some(s), Some(p)) = (primary(sch), primary(pcb)) {
        let same_unit = s.unit == p.unit || s.unit.is_none() || p.unit.is_none();
        let same_si = (s.si - p.si).abs() <= 1e-6 * s.si.abs().max(p.si.abs());
        if !(same_unit && same_si) {
            return Verdict::Differs;
        }
        let (se, pe) = (unrecognised_extras(&s.rest), unrecognised_extras(&p.rest));
        let extra: Vec<&str> = se.symmetric_difference(&pe).map(String::as_str).collect();
        return if extra.is_empty() {
            Verdict::Agree
        } else {
            Verdict::Unsure(format!(
                "the same value, with '{}' on one side only",
                extra.join(" ")
            ))
        };
    }
    if is_part_number(sch) && is_part_number(pcb) {
        return compare_part_numbers(sch, pcb);
    }
    Verdict::Unsure("one or both are descriptions, codes or placeholders".to_string())
}

/// Compare what each reference designator IS on the two sides: its value and,
/// when both carry one, its part number. Connectivity can agree while the
/// layout places a different part than the schematic names, and the verdict
/// then silently analyses the layout's.
///
/// A disagreement that changes what was simulated (a different electrical
/// value, or a different base part number) is one non-gating
/// "check before ordering" item naming both and saying which the verdict used. A pair that merely cannot be compared
/// (see [`compare_values`]) is one low-severity note naming both strings.
/// Blank fields and differences of case or spacing are not disagreements.
fn part_identity_parity(pcb: &ExtractedBoard, sch: &ExtractedBoard, out: &mut Vec<LintFinding>) {
    let fitted = |b: &ExtractedBoard| -> BTreeMap<String, Component> {
        b.components
            .iter()
            .filter(|c| !crate::dnp::unbought(c))
            .map(|c| (c.reference.clone(), c.clone()))
            .collect()
    };
    let sch_parts = fitted(sch);
    for (reference, pc) in fitted(pcb) {
        let Some(sc) = sch_parts.get(&reference) else {
            continue;
        };
        let blank = |a: &str, b: &str| a.trim().is_empty() || b.trim().is_empty();
        let mut fields: Vec<(&str, &str, &str, Verdict)> = Vec::new();
        if !blank(&sc.value, &pc.value) {
            let v = compare_values(&sc.value, &pc.value);
            fields.push(("value", sc.value.trim(), pc.value.trim(), v));
        }
        if let (Some(s), Some(p)) = (mpn_property(sc), mpn_property(&pc)) {
            // Property part numbers are part numbers by construction, so a
            // pair that does not read as one (a description in the MPN
            // column) is only a note.
            let v = if is_part_number(s) && is_part_number(p) {
                compare_part_numbers(s, p)
            } else if identity_key(s) == identity_key(p) {
                Verdict::Agree
            } else {
                Verdict::Unsure("one or both are descriptions or codes".to_string())
            };
            fields.push(("part number", s, p, v));
        }
        let pick = fields
            .iter()
            .find(|f| f.3 == Verdict::Differs)
            .or_else(|| fields.iter().find(|f| matches!(f.3, Verdict::Unsure(_))));
        let Some((what, schematic, layout, verdict)) = pick else {
            continue;
        };
        let (severity, message) = match verdict {
            // Not gating: the layout's part is what gets built and is what was
            // simulated, so this is a list item to confirm before ordering.
            Verdict::Differs => (
                Severity::Low,
                format!(
                    "{}{reference} has {what} '{schematic}' in the schematic but '{layout}' on the PCB. This verdict analysed the PCB's '{layout}', the part that gets built; if the schematic's part is the one to be fitted, update the PCB from the schematic and re-check.",
                    crate::CHECK_BEFORE_ORDERING
                ),
            ),
            Verdict::Unsure(why) => (
                Severity::Low,
                format!(
                    "{reference} has {what} '{schematic}' in the schematic and '{layout}' on the PCB. They are not written the same way and cannot be matched automatically ({why}); check both name the same part."
                ),
            ),
            Verdict::Agree => continue,
        };
        out.push(LintFinding {
            check: LintCheck::PcbSchematicMismatch,
            severity,
            message,
            refs: vec![reference.clone()],
            nets: Vec::new(),
        });
    }
}
