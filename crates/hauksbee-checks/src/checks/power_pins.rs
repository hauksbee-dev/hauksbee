//! A carded IC whose power pads the board leaves open, grounds or swaps.
//!
//! The card's `[models.pins]` names every pad's role. The primary supply pads
//! (`vcc`, `vdd`, `avdd`, `vddio`...) and the return pads (`gnd`, `agnd`,
//! `vss`...) are the same on every part, so a part's power wiring can be judged
//! from the roles alone, with no `supply_range` row and no rail level
//! ([`hauksbee_models::pin_roles`] is the one table that says which roles count).
//!
//! * **No supply**: every pad of a supply the part needs is open. The part is
//!   unpowered; it cannot work, and a pin driven from another supply
//!   back-feeds it through its protection diodes.
//! * **No return**: every return pad is open. The supply current has no way
//!   back and the part has no reference.
//! * **Supply on ground**: a supply pad (all pads of its family) sits on a
//!   ground net, so the part gets 0 V in place of its supply.
//! * **Return on a rail**: a return pad sits on a positive rail above every
//!   supply the part has. Together with a supply on ground this is the supply
//!   and ground exchanged.
//!
//! What keeps it silent. A pad is open only when it is on no net, on a
//! KiCad `unconnected-*` net, the only pad on a net that is not named for a
//! rail (two pads of one part tied together, a USB bridge's regulator output
//! wired to its I/O supply, are wired), or (in a netlist, which lists connected pads only) absent
//! from the part; a layout or schematic lists every pad, so there an unlisted
//! pad is an unplaced unit and not judged. A supply family counts only when the
//! card names its base pad ([`PowerPin::base`]), so a lone optional `vddio2` is
//! never required. A return pad on a negative rail or a signal net, a supply
//! pad the part's own `supply_range` row already judges on ground (the envelope
//! check reports it), a role the card lists as a supply output (a module's
//! regulated `5v` pin), and a return that is above a supply of unknown level
//! are all left alone.

use std::collections::HashMap;

use hauksbee_bind::binder::{is_ground, resolve};
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_unconnected_net, Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity,
    REMEDY_MARKER,
};
use hauksbee_models::netclass::{is_positive_supply, is_supply, rail_voltage};
use hauksbee_models::pin_roles::{power_pin, PowerKind, PowerPin};
use hauksbee_models::rating_basis::Limit;
use hauksbee_models::refdes::RefClass;
use hauksbee_models::{ComponentKind, Confidence, ModelEntry, ModelLibrary, OperatingEnvelope};

/// Where one power pad is wired.
#[derive(Debug, Clone, Copy, PartialEq)]
enum PadState {
    /// On no net or alone on one: connected to nothing.
    Open,
    /// Not listed, and the board's format lists only connected pads when it
    /// lists unconnected ones too: not judged.
    Unknown,
    Ground,
    /// A positive rail, with its level when the name states one.
    Positive(Option<f64>),
    /// A negative supply (`-12V`, `VEE`).
    Negative,
    /// Any other net: a signal, or a rail no name recognises.
    Other,
}

struct Pad {
    number: String,
    role: String,
    power: PowerPin,
    state: PadState,
    /// The net the pad is on, when it is on a named one.
    net: String,
    /// The pin's function name from the board, or the role when it has none.
    label: String,
}

/// The board facts every part's judgement shares.
struct Wiring<'a> {
    names: HashMap<i64, &'a str>,
    /// Pads on each net, of any part: two pads of one part tied together (a
    /// supply pin wired to an internal regulator's output pin, say) are not
    /// open.
    pads_on: HashMap<i64, usize>,
    /// Whether the board lists unconnected pads (a layout or a schematic does,
    /// a netlist does not), so an absent pad is not one.
    lists_unconnected: bool,
}

impl<'a> Wiring<'a> {
    fn of(board: &'a ExtractedBoard) -> Self {
        let mut pads_on: HashMap<i64, usize> = HashMap::new();
        let mut lists_unconnected = false;
        for comp in &board.components {
            for pin in &comp.pins {
                match pin.net.filter(|id| *id != 0) {
                    Some(id) => *pads_on.entry(id).or_default() += 1,
                    None => lists_unconnected = true,
                }
            }
        }
        Self {
            names: board.nets.iter().map(|n| (n.id, n.name.as_str())).collect(),
            pads_on,
            lists_unconnected,
        }
    }

    /// The state and net name of pad `number` of `comp`. `renamed` is set when
    /// the board's part has pads the card does not number (an `EPAD` where the
    /// card says `57`): the numbering differs, so an absent pad proves nothing.
    fn pad(&self, comp: &Component, number: &str, renamed: bool) -> (PadState, String, String) {
        let Some(pin) = comp.pins.iter().find(|p| p.number == number) else {
            let absent = if self.lists_unconnected || renamed {
                PadState::Unknown
            } else {
                PadState::Open
            };
            return (absent, String::new(), String::new());
        };
        let function = pin.function.trim().to_string();
        let Some(id) = pin.net.filter(|id| *id != 0) else {
            return (PadState::Open, String::new(), function);
        };
        let name = self.names.get(&id).copied().unwrap_or("");
        let lone = self.pads_on.get(&id).copied().unwrap_or(0) <= 1;
        let state = if is_unconnected_net(name) {
            PadState::Open
        } else if is_ground(name) {
            PadState::Ground
        } else if lone && !names_a_rail(name) {
            // A KiCad `Net-(U1-VDD)` carries the pad's own name; it is no
            // rail, just a pad with nothing else on it.
            PadState::Open
        } else if is_supply(name) {
            if is_positive_supply(name) {
                PadState::Positive(rail_voltage(name))
            } else {
                PadState::Negative
            }
        } else {
            PadState::Other
        };
        let net = if state == PadState::Open {
            String::new()
        } else {
            name.to_string()
        };
        (state, net, function)
    }
}

/// A name that states a rail by itself: a voltage or a supply token on a net
/// that is not an auto-generated `Net-(...)` of a pad.
fn names_a_rail(name: &str) -> bool {
    !name.contains('(') && (rail_voltage(name).is_some() || is_supply(name))
}

/// The kinds of part whose card power pads are judged. A regulator's `in` and
/// `out` are not primary supply roles, only its return pad is read.
const fn judged_kind(kind: ComponentKind) -> bool {
    matches!(
        kind,
        ComponentKind::Vreg
            | ComponentKind::Opamp
            | ComponentKind::Comparator
            | ComponentKind::Timer
            | ComponentKind::AnalogSwitch
            | ComponentKind::Digital
            | ComponentKind::Dac
            | ComponentKind::Adc
            | ComponentKind::ShiftRegister
            | ComponentKind::Mcu
    )
}

/// One High finding per fitted, carded IC whose power pads the board leaves
/// open, grounds or exchanges.
#[must_use]
pub fn power_pin_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    let wiring = Wiring::of(board);
    for comp in &board.components {
        let Some(part) = AssemblyState::of(comp).fitted() else {
            continue;
        };
        if RefClass::of(&comp.reference).is_rlc() {
            continue;
        }
        let res = resolve(lib, part);
        if matches!(res.confidence, Confidence::Guessed | Confidence::Unresolved) {
            continue;
        }
        let Some(model) = res.model else { continue };
        // A stand-in card's pin map is another part's: no measure of this one.
        if !judged_kind(model.kind)
            || !model.ratings.basis.judges(Limit::Envelope)
            || !super::card_fits(board, comp, &model)
        {
            continue;
        }
        report.findings.extend(judge(&wiring, comp, &model));
    }
    report
}

fn judge(wiring: &Wiring<'_>, comp: &Component, model: &ModelEntry) -> Vec<LintFinding> {
    let outputs: Vec<String> = model
        .supply_outputs
        .iter()
        .map(|o| o.role.to_ascii_lowercase())
        .collect();
    let renamed = comp
        .pins
        .iter()
        .any(|p| !model.pins.contains_key(&p.number));
    let mut pads: Vec<Pad> = model
        .pins
        .iter()
        .filter_map(|(number, role)| {
            let power = power_pin(role)?;
            if outputs.contains(&role.to_ascii_lowercase()) {
                return None;
            }
            let (state, net, function) = wiring.pad(comp, number, renamed);
            let label = if function.is_empty() {
                role.to_ascii_uppercase()
            } else {
                function
            };
            Some(Pad {
                number: number.clone(),
                role: role.to_ascii_lowercase(),
                power,
                state,
                net,
                label,
            })
        })
        .collect();
    pads.sort_by(|a, b| natural(&a.number).cmp(&natural(&b.number)));
    // The card names the package this board's part is in only when at least
    // one of its pads is wired.
    let evidence = model.pins.keys().any(|k| {
        comp.pins
            .iter()
            .any(|p| p.number == *k && p.net.is_some_and(|n| n != 0))
    });
    if pads.is_empty() || !evidence {
        return Vec::new();
    }
    let ctx = Part {
        comp,
        model,
        pads: &pads,
    };
    let mut findings = Vec::new();
    findings.extend(ctx.unpowered());
    findings.extend(ctx.no_return());
    findings.extend(ctx.exchanged());
    findings
}

/// Pad numbers in a stable order that puts `2` before `10`.
fn natural(number: &str) -> (usize, String) {
    (number.len(), number.to_string())
}

struct Part<'a> {
    comp: &'a Component,
    model: &'a ModelEntry,
    pads: &'a [Pad],
}

impl Part<'_> {
    fn supply_pads(&self) -> impl Iterator<Item = &Pad> {
        self.pads
            .iter()
            .filter(|p| p.power.kind == PowerKind::Supply)
    }

    fn return_pads(&self) -> impl Iterator<Item = &Pad> {
        self.pads
            .iter()
            .filter(|p| p.power.kind == PowerKind::Return)
    }

    /// The supply families the card requires: those it names a base pad of.
    fn required_families(&self) -> Vec<&str> {
        let mut families: Vec<&str> = self
            .supply_pads()
            .filter(|p| p.power.base)
            .map(|p| p.power.family.as_str())
            .collect();
        families.sort_unstable();
        families.dedup();
        families
    }

    fn family(&self, family: &str) -> Vec<&Pad> {
        self.supply_pads()
            .filter(|p| p.power.family == family)
            .collect()
    }

    /// The datasheet's supply window for any of `pads`, when the card has one.
    fn window(&self, pads: &[&Pad]) -> Option<(f64, f64)> {
        self.model.envelope.iter().find_map(|e| match e {
            OperatingEnvelope::SupplyRange {
                pin, min_v, max_v, ..
            } if pads.iter().any(|p| p.role.eq_ignore_ascii_case(pin)) => Some((*min_v, *max_v)),
            _ => None,
        })
    }

    fn who(&self) -> String {
        format!("{} ({})", self.comp.reference, self.comp.value.trim())
    }

    fn finding(&self, message: String, nets: Vec<String>) -> LintFinding {
        LintFinding {
            check: LintCheck::PowerPinWiring,
            severity: Severity::High,
            message,
            refs: vec![self.comp.reference.clone()],
            nets,
        }
    }

    /// A supply family with every pad open.
    fn unpowered(&self) -> Vec<LintFinding> {
        let mut out = Vec::new();
        for family in self.required_families() {
            let pads = self.family(family);
            if !pads.iter().all(|p| p.state == PadState::Open) {
                continue;
            }
            let window = self.window(&pads);
            let range = window.map_or(String::new(), |(lo, hi)| {
                format!(" (datasheet supply {}-{} V)", volts(lo), volts(hi))
            });
            let to = window.map_or_else(
                || format!("the {} supply rail", family.to_ascii_uppercase()),
                |(lo, hi)| format!("a {}-{} V supply rail", volts(lo), volts(hi)),
            );
            let message = format!(
                "{} has no supply on its {family_up} input: {pads_text} {is} not connected to \
                 any net (card '{id}' names {it} a supply pad{range}), so that part of the IC is \
                 unpowered, it cannot work, and any pin another supply drives back-feeds it \
                 through its input protection diodes. {REMEDY_MARKER}Connect {pads_text} to {to} \
                 and decouple it with a capacitor at the pad.",
                self.who(),
                family_up = family.to_ascii_uppercase(),
                pads_text = pads_text(&pads),
                is = if pads.len() == 1 { "is" } else { "are" },
                id = self.model.id,
                it = if pads.len() == 1 { "it" } else { "them" },
            );
            out.push(self.finding(message, Vec::new()));
        }
        out
    }

    /// Every return pad open.
    fn no_return(&self) -> Option<LintFinding> {
        let pads: Vec<&Pad> = self.return_pads().collect();
        if pads.is_empty() || !pads.iter().all(|p| p.state == PadState::Open) {
            return None;
        }
        let ground = pads.iter().any(|p| p.power.ground);
        let to = if ground {
            "the ground net"
        } else {
            "the negative supply (ground on a single-supply board)"
        };
        let message = format!(
            "{} has no return: {pads_text} {is} not connected to any net (card '{id}' names {it} \
             the ground return), so the supply current has no way back and the part has no \
             reference; it cannot work, and any signal pin tied to another part back-feeds it \
             through its protection diodes. {REMEDY_MARKER}Connect {pads_text} to {to}.",
            self.who(),
            pads_text = pads_text(&pads),
            is = if pads.len() == 1 { "is" } else { "are" },
            id = self.model.id,
            it = if pads.len() == 1 { "it" } else { "them" },
        );
        Some(self.finding(message, Vec::new()))
    }

    /// A supply family wholly on ground, a return pad above every supply, or both.
    fn exchanged(&self) -> Option<LintFinding> {
        let returns: Vec<&Pad> = self.return_pads().collect();
        if returns.is_empty() {
            return None;
        }
        // A return on a negative rail or a signal net is a split supply or a
        // part the roles do not describe: nothing here is judged.
        if returns
            .iter()
            .any(|p| matches!(p.state, PadState::Negative | PadState::Other))
        {
            return None;
        }
        let required = self.required_families();
        // Supply pads wholly on ground, by family.
        let mut grounded: Vec<&Pad> = Vec::new();
        for family in &required {
            let pads = self.family(family);
            if pads.iter().all(|p| p.state == PadState::Ground) {
                grounded.extend(pads);
            }
        }
        // The envelope check judges a supply pad on ground when the card has
        // a supply window for it: one finding, not two.
        if !grounded.is_empty() && self.window(&grounded).is_some() {
            return None;
        }
        let supplies: Vec<&Pad> = required
            .iter()
            .flat_map(|f| self.family(f))
            .filter(|p| p.state != PadState::Open && p.state != PadState::Unknown)
            .collect();
        // A return pad above every supply there is, in a known order.
        let lifted: Vec<&Pad> = returns
            .iter()
            .copied()
            .filter(|r| match r.state {
                PadState::Positive(vg) => {
                    !supplies.is_empty()
                        && supplies.iter().all(|s| match s.state {
                            PadState::Ground => true,
                            PadState::Positive(Some(vs)) => vg.is_some_and(|vg| vs <= vg),
                            _ => false,
                        })
                }
                _ => false,
            })
            .collect();
        if grounded.is_empty() && lifted.is_empty() {
            return None;
        }
        let message = self.exchange_message(&grounded, &lifted, &supplies);
        let mut nets: Vec<String> = grounded
            .iter()
            .chain(lifted.iter())
            .map(|p| p.net.clone())
            .collect();
        nets.sort();
        nets.dedup();
        Some(self.finding(message, nets))
    }

    /// The finding's sentence for a supply on ground, a return on a rail, or both.
    fn exchange_message(&self, grounded: &[&Pad], lifted: &[&Pad], supplies: &[&Pad]) -> String {
        // A grounded supply alone is a supply on ground; a lifted return alone
        // is a return on a rail; both together are the exchange.
        let g_net = |pads: &[&Pad]| pads.first().map(|p| p.net.clone()).unwrap_or_default();
        match (grounded.is_empty(), lifted.is_empty()) {
            (false, false) => {
                let rail = g_net(lifted);
                let gnd = g_net(grounded);
                format!(
                    "{} has its supply and ground exchanged: {sup} (supply) is on '{gnd}' and \
                     {ret} (ground return) is on '{rail}'. The IC is powered backwards: its \
                     protection and substrate diodes conduct from the rail through the part, \
                     which draws heavy current, clamps the rail, and never works. {REMEDY_MARKER}\
                     Swap them: connect {sup} to '{rail}' and {ret} to '{gnd}'.",
                    self.who(),
                    sup = pads_text(grounded),
                    ret = pads_text(lifted),
                )
            }
            (false, true) => {
                let window = self.window(grounded);
                let to = window.map_or_else(
                    || "its supply rail".to_string(),
                    |(lo, hi)| format!("a {}-{} V supply rail", volts(lo), volts(hi)),
                );
                format!(
                    "{} has its supply on ground: {sup} (supply, card '{id}') is on '{net}', so \
                     the part gets 0 V in place of its supply and is unpowered: it cannot work, \
                     and any pin another supply drives back-feeds it through its protection \
                     diodes. {REMEDY_MARKER}Connect {sup} to {to}.",
                    self.who(),
                    sup = pads_text(grounded),
                    id = self.model.id,
                    net = g_net(grounded),
                )
            }
            (true, _) => {
                format!(
                    "{} has its ground return on a positive rail: {ret} (ground return, card \
                     '{id}') is on '{net}', above the part's supply {sup}, so the IC sees its \
                     supply reversed and its protection and substrate diodes conduct: heavy \
                     current, a clamped rail, and the part never works. {REMEDY_MARKER}Connect \
                     {ret} to the ground net and check that the supply pad is on the rail.",
                    self.who(),
                    ret = pads_text(lifted),
                    id = self.model.id,
                    net = g_net(lifted),
                    sup = pads_text(supplies),
                )
            }
        }
    }
}

/// `pad 8 (VCC)` or `pads 1 (VDD), 8 (VDD)`.
fn pads_text(pads: &[&Pad]) -> String {
    let list: Vec<String> = pads
        .iter()
        .map(|p| format!("{} ({})", p.number, p.label))
        .collect();
    format!(
        "{} {}",
        if pads.len() == 1 { "pad" } else { "pads" },
        list.join(", ")
    )
}

fn volts(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::checks::test_support::{board, part};

    const GND: i64 = 1;
    const V33: i64 = 2;
    const V5: i64 = 3;
    const NEG12: i64 = 4;
    const POS12: i64 = 5;
    const SIG: i64 = 6;
    const LONE: i64 = 7;
    const UNCON: i64 = 8;
    const VBATT: i64 = 9;

    const NETS: [(i64, &str); 9] = [
        (GND, "GND"),
        (V33, "+3V3"),
        (V5, "+5V"),
        (NEG12, "-12V"),
        (POS12, "+12V"),
        (SIG, "/SIG"),
        (LONE, "Net-(U1-VCC)"),
        (UNCON, "unconnected-(U1-VCC)"),
        (VBATT, "+VBATT"),
    ];

    /// A library holding one `digital` card, `PWRTEST`, with the given pin
    /// map and any extra card text.
    fn card_lib(pins: &[(&str, &str)], extra: &str) -> ModelLibrary {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let mut map = String::new();
        for (p, r) in pins {
            writeln!(map, "\"{p}\" = \"{r}\"").unwrap();
        }
        let card = format!(
            "[[models]]\nid = \"pwr_test\"\nkind = \"digital\"\ndescription = \"power pin fixture\"\n\
             [models.match]\nvalue_re = \"^PWRTEST$\"\n[models.params]\nidentity_only = true\n\
             warning = \"identity only\"\nunlocked_by = \"validated behavior\"\n{extra}\n[models.pins]\n{map}"
        );
        let dir =
            std::env::temp_dir().join(format!("hauksbee_power_pins_{}_{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("models.toml"), card).unwrap();
        let mut lib = ModelLibrary::empty();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "{errors:?}");
        lib
    }

    /// A signal load, so the signal net is not alone on it.
    fn load() -> Component {
        part("R1", "10k", &[("1", SIG), ("2", GND)])
    }

    /// U1 (PWRTEST) with its pads on the listed nets.
    fn run(lib: &ModelLibrary, pads: &[(&str, i64)]) -> Vec<LintFinding> {
        let u = part("U1", "PWRTEST", pads);
        power_pin_lint(&board(&NETS, vec![u, load()]), lib).findings
    }

    /// Pad 1 the supply, pad 2 the ground, pad 3 a signal.
    fn simple() -> ModelLibrary {
        card_lib(&[("1", "vcc"), ("2", "gnd"), ("3", "in_plus")], "")
    }

    fn says(f: &LintFinding, needles: &[&str]) {
        for n in needles {
            assert!(f.message.contains(n), "missing {n:?} in {}", f.message);
        }
        assert!(f.message.contains(REMEDY_MARKER), "{}", f.message);
        assert_eq!(f.check, LintCheck::PowerPinWiring);
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.refs, vec!["U1".to_string()]);
    }

    #[test]
    fn a_wired_part_is_silent() {
        assert!(run(&simple(), &[("1", V33), ("2", GND), ("3", SIG)]).is_empty());
    }

    #[test]
    fn supply_pad_missing_from_a_netlist_leaves_the_part_unpowered() {
        let found = run(&simple(), &[("2", GND), ("3", SIG)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(
            &found[0],
            &[
                "U1 (PWRTEST)",
                "no supply",
                "pad 1 (VCC)",
                "unpowered",
                "'pwr_test'",
            ],
        );
    }

    #[test]
    fn supply_pad_on_no_net_alone_or_unconnected_is_open() {
        for net in [None, Some(LONE), Some(UNCON)] {
            let mut u = part("U1", "PWRTEST", &[("1", 0), ("2", GND), ("3", SIG)]);
            u.pins[0].net = net;
            let found = power_pin_lint(&board(&NETS, vec![u, load()]), &simple()).findings;
            assert_eq!(found.len(), 1, "{net:?}: {found:#?}");
            says(&found[0], &["no supply"]);
        }
    }

    /// A schematic or layout lists every pad, so a pad it does not list is an
    /// unplaced unit, not an open pin.
    #[test]
    fn an_unlisted_pad_is_judged_only_where_the_board_lists_only_connected_pads() {
        let mut u = part("U1", "PWRTEST", &[("2", GND), ("3", SIG), ("4", V33)]);
        u.pins[2].net = None;
        let found = power_pin_lint(&board(&NETS, vec![u, load()]), &simple()).findings;
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn a_lone_pad_on_a_named_rail_is_the_rail_checks_not_this_ones() {
        // Pad 1 alone on "+3V3" (nothing else there): a rail with no source
        // is another check's finding.
        let u = part("U1", "PWRTEST", &[("1", V33), ("2", GND), ("3", SIG)]);
        assert!(power_pin_lint(&board(&NETS, vec![u, load()]), &simple())
            .findings
            .is_empty());
    }

    /// A bus-powered USB bridge ties its regulator output pin to its I/O
    /// supply pin and nothing else is on that net: wired, not open.
    #[test]
    fn two_supply_pads_of_one_part_tied_together_are_not_open() {
        let lib = card_lib(
            &[("1", "vdd"), ("2", "gnd"), ("4", "vio"), ("3", "in_plus")],
            "",
        );
        let tied = run(&lib, &[("1", LONE), ("4", LONE), ("2", GND), ("3", SIG)]);
        assert!(tied.is_empty(), "{tied:#?}");
        // Each alone on its own net is still two open supply inputs.
        let open = run(&lib, &[("2", GND), ("3", SIG)]);
        assert_eq!(open.len(), 2, "{open:#?}");
    }

    /// An Eagle package names its exposed pad `EPAD` where the card says `2`:
    /// the numbering differs, so the card's ground pad is not proven open.
    #[test]
    fn a_pad_the_board_names_differently_from_the_card_is_not_judged() {
        let found = run(&simple(), &[("1", V33), ("EPAD", GND), ("3", SIG)]);
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn ground_pads_all_open_leave_the_part_with_no_return() {
        let found = run(&simple(), &[("1", V33), ("3", SIG)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["no return", "pad 2 (GND)", "no way back"]);
    }

    #[test]
    fn one_open_ground_pad_of_several_is_not_a_missing_return() {
        let lib = card_lib(&[("1", "vcc"), ("2", "gnd"), ("4", "gnd")], "");
        assert!(run(&lib, &[("1", V33), ("2", GND)]).is_empty());
        let found = run(&lib, &[("1", V33)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["pads 2 (GND), 4 (GND)"]);
    }

    #[test]
    fn a_supply_pad_on_ground_is_a_miswire() {
        let found = run(&simple(), &[("1", GND), ("2", GND), ("3", SIG)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(
            &found[0],
            &["supply on ground", "pad 1 (VCC)", "'GND'", "0 V"],
        );
    }

    #[test]
    fn supply_and_ground_exchanged_is_one_finding() {
        let found = run(&simple(), &[("1", GND), ("2", V33), ("3", SIG)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(
            &found[0],
            &[
                "exchanged",
                "pad 1 (VCC)",
                "'GND'",
                "pad 2 (GND)",
                "'+3V3'",
                "Swap them",
            ],
        );
        assert_eq!(found[0].nets, vec!["+3V3".to_string(), "GND".to_string()]);
    }

    #[test]
    fn a_ground_pad_on_a_rail_above_the_supply_is_a_miswire_and_the_reverse_order_is_not() {
        // Return on +5V above a +3V3 supply.
        let found = run(&simple(), &[("1", V33), ("2", V5), ("3", SIG)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(
            &found[0],
            &["ground return on a positive rail", "pad 2 (GND)", "'+5V'"],
        );
        // Return on +3V3 below a +5V supply is a floating high side, not judged.
        assert!(run(&simple(), &[("1", V5), ("2", V33), ("3", SIG)]).is_empty());
        // A return on a rail of unknown level is not ordered against the supply.
        let u = part("U1", "PWRTEST", &[("1", V33), ("2", VBATT), ("3", SIG)]);
        let bd = board(
            &NETS,
            vec![u, part("R2", "1k", &[("1", VBATT), ("2", GND)]), load()],
        );
        assert!(power_pin_lint(&bd, &simple()).findings.is_empty());
    }

    #[test]
    fn a_split_supply_part_is_judged_by_its_own_rails() {
        let amp = card_lib(&[("8", "vcc"), ("4", "vss"), ("1", "out")], "");
        // +12 V and -12 V: legal.
        assert!(run(&amp, &[("8", POS12), ("4", NEG12), ("1", SIG)]).is_empty());
        // A single negative supply (V+ at ground, V- on -12 V): legal.
        assert!(run(&amp, &[("8", GND), ("4", NEG12), ("1", SIG)]).is_empty());
        // A single positive supply, V- grounded: legal.
        assert!(run(&amp, &[("8", V5), ("4", GND), ("1", SIG)]).is_empty());
        // V- floating: no return.
        let found = run(&amp, &[("8", POS12), ("1", SIG)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["no return", "negative supply"]);
        // Supply and V- exchanged on a single supply.
        let found = run(&amp, &[("8", GND), ("4", V5), ("1", SIG)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["exchanged"]);
    }

    #[test]
    fn each_required_supply_family_is_judged_and_an_optional_numbered_one_is_not() {
        let dual = card_lib(&[("1", "vcca"), ("2", "vccb"), ("3", "gnd")], "");
        let found = run(&dual, &[("1", V33), ("3", GND)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["VCCB", "pad 2 (VCCB)"]);
        assert!(run(&dual, &[("1", V33), ("2", V5), ("3", GND)]).is_empty());
        // A card that names only a numbered second supply does not require it.
        let optional = card_lib(&[("1", "vdd"), ("2", "vddio2"), ("3", "gnd")], "");
        assert!(run(&optional, &[("1", V33), ("3", GND)]).is_empty());
        // One open pad of a multi-pad family is not an unpowered family.
        let many = card_lib(&[("1", "vdd"), ("2", "vdd2"), ("3", "gnd")], "");
        assert!(run(&many, &[("1", V33), ("3", GND)]).is_empty());
        let found = run(&many, &[("3", GND)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["pads 1 (VDD), 2 (VDD2)"]);
    }

    #[test]
    fn an_open_exposed_pad_or_internal_rail_is_not_a_missing_power_pin() {
        let lib = card_lib(
            &[
                ("1", "vdd"),
                ("2", "gnd"),
                ("3", "vddcore"),
                ("9", "gnd_ep"),
                ("4", "vbat"),
            ],
            "",
        );
        assert!(run(&lib, &[("1", V33), ("2", GND)]).is_empty());
    }

    #[test]
    fn a_pin_the_card_lists_as_a_supply_output_is_not_a_supply_input() {
        let module = card_lib(
            &[("1", "vcc"), ("2", "gnd")],
            "[[models.supply_outputs]]\nrole = \"vcc\"\nnominal = 3.3\nmin = 3.2\nmax = 3.4\n\
             basis = \"module regulator\"\n",
        );
        assert!(run(&module, &[("2", GND), ("3", SIG)]).is_empty());
    }

    #[test]
    fn a_part_not_fitted_is_not_judged() {
        let mut u = part("U1", "PWRTEST", &[("2", GND), ("3", SIG)]);
        u.dnp = true;
        assert!(power_pin_lint(&board(&NETS, vec![u, load()]), &simple())
            .findings
            .is_empty());
    }

    #[test]
    fn a_supply_on_ground_the_cards_supply_range_judges_is_left_to_the_envelope_check() {
        let window = "[[models.envelope]]\nkind = \"supply_range\"\npin = \"vcc\"\nmin_v = 1.65\n\
                      max_v = 5.5\nabs_max_v = 6.5\nbasis = \"Recommended Operating Conditions, VCC\"\n";
        let lib = card_lib(&[("1", "vcc"), ("2", "gnd"), ("3", "in_plus")], window);
        assert!(run(&lib, &[("1", GND), ("2", GND), ("3", SIG)]).is_empty());
        // An open supply pad is on no rail: this check judges it and quotes
        // the datasheet window.
        let found = run(&lib, &[("2", GND), ("3", SIG)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(
            &found[0],
            &["datasheet supply 1.65-5.5 V", "a 1.65-5.5 V supply rail"],
        );
    }

    /// The other tables of supply roles are subsets of the one vocabulary.
    #[test]
    fn the_checks_role_lists_agree_with_the_vocabulary() {
        for role in crate::checks::rail_decoupling::SUPPLY_ROLES {
            assert!(
                power_pin(role).is_some_and(|p| p.kind == PowerKind::Supply),
                "{role}"
            );
        }
        for role in crate::checks::envelope::IC_SUPPLY_ROLES {
            // A bare voltage name is a module's pin, not a primary supply.
            if role != "3v3" {
                assert!(
                    power_pin(role).is_some_and(|p| p.kind == PowerKind::Supply),
                    "{role}"
                );
            }
        }
    }
}
