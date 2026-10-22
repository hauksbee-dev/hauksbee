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
//! KiCad `unconnected-*` net, on a net not named for a rail that nothing on
//! the board can source, or (in a netlist, which lists connected pads only)
//! absent from the part. A net nothing sources holds only pads of the part's
//! own family, capacitors, test points, unfitted parts and two-terminal parts
//! whose other end is on ground or open; another part's pin, a connector or a
//! series part from elsewhere may source it. A pad of the same part with a
//! non-power role (a USB bridge's `3V3OUT` on its `VCCIO`) sources it only
//! while another of the part's supplies is on a rail, since that output is
//! made from it. A drawn part (a layout footprint or a schematic symbol,
//! read from the pad and part positions the format carries) lists every pad,
//! so an unlisted pad there is an unplaced unit and not judged. A part whose
//! board pads include a number the card has no row for and cannot be an
//! omitted no-connect (a name like `EPAD`, or a number past the card's last
//! pad) is numbered differently from the card and not judged at all. A
//! supply family counts only when the card names its base pad
//! ([`PowerPin::base`]) and does not list it under `optional_supplies`, so a
//! lone `vddio2` or a bypassable regulator input is never required. A return
//! pad on a negative rail or a signal net, a supply pad the part's own
//! `supply_range` row already judges on ground (the envelope check reports
//! it), a role the card lists as a supply output (a module's regulated `5v`
//! pin), and a return that is above a supply of unknown level are all left
//! alone.
//!
//! When the board's own symbol names a flagged pad differently from the card
//! (a five-lead symbol on a six-lead part puts `V_CC` on pad 4 where the
//! datasheet has it on pad 6), the finding says so: the board is still wired
//! wrong for the part the card describes, and the message tells the user which
//! of the two to fix.

use std::collections::HashMap;

use hauksbee_bind::binder::{is_ground, resolve};
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_capacitor, is_test_point_or_hole, is_unconnected_net, Component, ExtractedBoard, LintCheck,
    LintFinding, NetLintReport, Pin, Severity, REMEDY_MARKER,
};
use hauksbee_ir::units::format_number;
use hauksbee_models::behavioral::LawKind;
use hauksbee_models::netclass::{is_positive_supply, is_supply, rail_voltage};
use hauksbee_models::pin_roles::{power_pin, PowerKind, PowerPin};
use hauksbee_models::rating_basis::Limit;
use hauksbee_models::refdes::RefClass;
use hauksbee_models::{ComponentKind, Confidence, ModelEntry, ModelLibrary, OperatingEnvelope};

/// Where one power pad is wired.
#[derive(Debug, Clone, Copy, PartialEq)]
enum PadState {
    /// On no net, or on one nothing on the board can source.
    Open,
    /// On a net only another of the part's own non-power pads (an internal
    /// regulator's output) can source: wired while another of the part's
    /// supplies is on a rail, open otherwise.
    Tied,
    /// Not listed, on a drawn part that lists every pad: not judged.
    Unknown,
    Ground,
    /// A positive rail, with its level when the name states one.
    Positive(Option<f64>),
    /// A negative supply (`-12V`, `VEE`).
    Negative,
    /// Any other net: a signal, or a rail no name recognises.
    Other,
}

impl PadState {
    /// How connected a reading is, so that of several pads with one number
    /// the connected one counts.
    const fn rank(self) -> u8 {
        match self {
            Self::Open | Self::Unknown => 0,
            Self::Tied => 1,
            _ => 2,
        }
    }
}

struct Pad {
    number: String,
    role: String,
    power: PowerPin,
    state: PadState,
    /// The net the pad is on, when it is on a named one.
    net: String,
    /// The pin's function name from the board's symbol, empty when it has none.
    function: String,
    /// Whether that name reads as the same power pad the card says.
    agrees: bool,
}

impl Pad {
    /// The board's name for the pad when it agrees with the card, else the
    /// card's role.
    fn label(&self) -> String {
        if self.agrees {
            self.function.clone()
        } else {
            self.role.to_ascii_uppercase()
        }
    }
}

/// How a net with no rail name can be fed.
#[derive(PartialEq, Eq)]
enum Feed {
    /// Nothing on it can source it.
    None,
    /// Only another pad of the part itself.
    Own,
    /// Another fitted part that may source it.
    Board,
}

/// The board facts every part's judgement shares.
struct Wiring<'a> {
    board: &'a ExtractedBoard,
    names: HashMap<i64, &'a str>,
    /// Every pad on each net: (component index, pin index).
    members: HashMap<i64, Vec<(usize, usize)>>,
}

impl<'a> Wiring<'a> {
    fn of(board: &'a ExtractedBoard) -> Self {
        let mut members: HashMap<i64, Vec<(usize, usize)>> = HashMap::new();
        for (ci, comp) in board.components.iter().enumerate() {
            for (pi, pin) in comp.pins.iter().enumerate() {
                if let Some(id) = pin.net.filter(|id| *id != 0) {
                    members.entry(id).or_default().push((ci, pi));
                }
            }
        }
        Self {
            board,
            names: board.nets.iter().map(|n| (n.id, n.name.as_str())).collect(),
            members,
        }
    }

    fn name(&self, id: Option<i64>) -> &str {
        id.filter(|id| *id != 0)
            .and_then(|id| self.names.get(&id).copied())
            .unwrap_or("")
    }

    /// The state, net name and symbol name of pad `number` of component `ci`,
    /// read as `power`. Of several pads with one number (a split exposed pad,
    /// separate paste apertures) the connected one counts.
    fn pad(
        &self,
        ci: usize,
        model: &ModelEntry,
        number: &str,
        power: &PowerPin,
    ) -> (PadState, String, String) {
        let comp = &self.board.components[ci];
        let best = comp
            .pins
            .iter()
            .filter(|p| p.number == number)
            .map(|pin| self.read(ci, model, pin, power))
            .max_by_key(|r| r.0.rank());
        best.unwrap_or_else(|| {
            // A drawn part lists every pad of its footprint or symbol, so an
            // absent one is an unplaced unit; a netlist lists connected pads
            // only, so there it is open.
            let absent = if drawn(comp) {
                PadState::Unknown
            } else {
                PadState::Open
            };
            (absent, String::new(), String::new())
        })
    }

    fn read(
        &self,
        ci: usize,
        model: &ModelEntry,
        pin: &Pin,
        power: &PowerPin,
    ) -> (PadState, String, String) {
        let function = pin.function.trim().to_string();
        let Some(id) = pin.net.filter(|id| *id != 0) else {
            return (PadState::Open, String::new(), function);
        };
        let name = self.name(Some(id));
        let state = if is_unconnected_net(name) {
            PadState::Open
        } else if is_ground(name) {
            PadState::Ground
        } else if let Some(state) = (!names_a_rail(name))
            .then(|| self.feed(id, ci, model, power))
            .and_then(|feed| match feed {
                // A KiCad `Net-(U1-VDD)` carries the pad's own name; it is no
                // rail, and nothing else on it can supply or return current.
                Feed::None => Some(PadState::Open),
                Feed::Own => Some(PadState::Tied),
                Feed::Board => None,
            })
        {
            state
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

    /// What on net `id`, besides the pad of component `ci` being judged, can
    /// feed a `power` pad: a capacitor, a test point, an unfitted part or a
    /// two-terminal part whose other end is on ground or open cannot; a pad
    /// of the part itself can only when the card makes it a regulated output
    /// not made from this supply ([`own_output_feeds`]); anything else may.
    fn feed(&self, id: i64, ci: usize, model: &ModelEntry, power: &PowerPin) -> Feed {
        let mut own = false;
        for &(oi, pi) in self.members.get(&id).into_iter().flatten() {
            let other = &self.board.components[oi];
            let pin = &other.pins[pi];
            if oi == ci {
                own |= power.kind == PowerKind::Supply
                    && model
                        .pins
                        .get(&pin.number)
                        .is_some_and(|role| own_output_feeds(model, role, &power.family));
                continue;
            }
            if AssemblyState::of(other).fitted().is_none()
                || is_capacitor(other)
                || is_test_point_or_hole(other)
                || self.dead_end(other, pi, power.kind)
            {
                continue;
            }
            return Feed::Board;
        }
        if own {
            Feed::Own
        } else {
            Feed::None
        }
    }

    /// A two-terminal part that carries no current to or from the net through
    /// pad `pi`: its other pad is open or on the same net, or, for a supply,
    /// on ground (a pull-down, clamp or indicator is no source; to a return,
    /// a resistor to ground is its sense path).
    fn dead_end(&self, part: &Component, pi: usize, kind: PowerKind) -> bool {
        if part.pins.len() != 2 {
            return false;
        }
        let here = part.pins[pi].net;
        let other = part.pins[1 - pi].net;
        let name = self.name(other);
        other == here
            || name.is_empty()
            || is_unconnected_net(name)
            || (kind == PowerKind::Supply && is_ground(name))
    }
}

/// Whether the part's own `role` pin is an output the card says it sources
/// (a `supply_outputs` row, or a voltage law forcing it) and that output is
/// not made from the supply `family` it would feed: FT231X `3V3OUT`, forced at
/// `min(v_vcc, 3.3)`, feeds `VCCIO` but never `VCC` itself.
fn own_output_feeds(model: &ModelEntry, role: &str, family: &str) -> bool {
    if model
        .supply_outputs
        .iter()
        .any(|o| o.role.eq_ignore_ascii_case(role))
    {
        return true;
    }
    // An on-chip regulator's output (CP2104 `VDD` from `REGIN`) feeds the part's
    // other supply families tied to it, never its own.
    if model
        .optional_supplies
        .iter()
        .any(|o| o.regulated_from.is_some() && o.role.eq_ignore_ascii_case(role))
    {
        return power_pin(role, model.kind).is_some_and(|p| p.family != family);
    }
    let family_roles: Vec<String> = model
        .pins
        .values()
        .filter(|r| {
            power_pin(r, model.kind)
                .is_some_and(|p| p.kind == PowerKind::Supply && p.family == family)
        })
        .map(|r| format!("v_{}", r.to_ascii_lowercase()))
        .collect();
    model.behavioral.laws.iter().any(|law| {
        law.kind == LawKind::Voltage
            && law.a.eq_ignore_ascii_case(role)
            && !family_roles
                .iter()
                .any(|v| reads_variable(&law.expr.to_ascii_lowercase(), v))
    })
}

/// Whether expression `expr` reads variable `var` (a whole identifier).
fn reads_variable(expr: &str, var: &str) -> bool {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    expr.match_indices(var).any(|(at, _)| {
        let before = expr[..at].chars().next_back();
        let after = expr[at + var.len()..].chars().next();
        !before.is_some_and(ident) && !after.is_some_and(ident)
    })
}

/// A part read from a drawing (a layout footprint or a schematic symbol, which
/// list every pad and carry positions) rather than a connection list.
fn drawn(comp: &Component) -> bool {
    comp.position.is_some() || comp.pins.iter().any(|p| p.position.is_some())
}

/// A board part with a pad number the card has no row for that cannot be a
/// no-connect the card left out: a name (`EPAD`, `P$1`) or a number past the
/// card's last pad. Its pads are numbered differently from the card's, so no
/// card pad can be read off it.
fn numbered_differently(comp: &Component, model: &ModelEntry) -> bool {
    let last = model
        .pins
        .keys()
        .filter_map(|k| k.trim().parse::<u32>().ok())
        .max();
    comp.pins.iter().any(|p| {
        let n = p.number.trim();
        !n.is_empty()
            && !model.pins.contains_key(&p.number)
            && n.parse::<u32>()
                .ok()
                .is_none_or(|v| last.is_none_or(|last| v > last))
    })
}

/// A name that states a rail by itself: a voltage or a supply token on a net
/// that is not an auto-generated `Net-(...)` of a pad.
fn names_a_rail(name: &str) -> bool {
    !name.contains('(') && (rail_voltage(name).is_some() || is_supply(name))
}

/// A board symbol's pin name read as a card role (`V_CC` as `vcc`).
fn symbol_power(function: &str, kind: ComponentKind) -> Option<PowerPin> {
    let f: String = function
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '+')
        .collect();
    power_pin(&f.to_ascii_lowercase(), kind)
}

/// Two readings of one power pad: the same kind, and for a supply the same
/// family.
fn same_power(a: &PowerPin, b: &PowerPin) -> bool {
    a.kind == b.kind && (a.kind == PowerKind::Return || a.family == b.family)
}

/// A symbol pin name that names nothing: blank, `~`, or the pad number again.
fn placeholder_function(function: &str) -> bool {
    function == "~" || function.chars().all(|c| c.is_ascii_digit())
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
    for (ci, comp) in board.components.iter().enumerate() {
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
            || numbered_differently(comp, &model)
        {
            continue;
        }
        report.findings.extend(judge(&wiring, ci, &model));
    }
    report
}

fn judge(wiring: &Wiring<'_>, ci: usize, model: &ModelEntry) -> Vec<LintFinding> {
    let comp = &wiring.board.components[ci];
    let outputs: Vec<String> = model
        .supply_outputs
        .iter()
        .map(|o| o.role.to_ascii_lowercase())
        .collect();
    let mut pads: Vec<Pad> = model
        .pins
        .iter()
        .filter_map(|(number, role)| {
            let mut power = power_pin(role, model.kind)?;
            if outputs.contains(&role.to_ascii_lowercase()) {
                return None;
            }
            // A supply the datasheet lets the board leave open is never one
            // the part is held to.
            if power.kind == PowerKind::Supply
                && model
                    .optional_supplies
                    .iter()
                    .any(|o| o.role.eq_ignore_ascii_case(role))
            {
                power.base = false;
            }
            let (state, net, function) = wiring.pad(ci, model, number, &power);
            let agrees =
                symbol_power(&function, model.kind).is_some_and(|s| same_power(&s, &power));
            Some(Pad {
                number: number.clone(),
                role: role.to_ascii_lowercase(),
                power,
                state,
                net,
                function,
                agrees,
            })
        })
        .collect();
    pads.sort_by(|a, b| natural(&a.number).cmp(&natural(&b.number)));
    // A pad only the part's own regulator output feeds is wired while another
    // of its supplies is on a rail (that output is made from it), and open
    // when none is.
    let fed: Vec<String> = pads
        .iter()
        .filter(|p| {
            p.power.kind == PowerKind::Supply
                && matches!(p.state, PadState::Positive(_) | PadState::Other)
        })
        .map(|p| p.power.family.clone())
        .collect();
    // An on-chip regulator whose input is fed: its output pin feeds what is
    // tied to it ([`own_output_feeds`]).
    let regulator_fed = model.optional_supplies.iter().any(|o| {
        o.regulated_from.as_ref().is_some_and(|input| {
            model.pins.iter().any(|(number, role)| {
                role.eq_ignore_ascii_case(input) && {
                    let as_supply = PowerPin {
                        kind: PowerKind::Supply,
                        family: input.to_ascii_lowercase(),
                        base: false,
                        ground: false,
                    };
                    matches!(
                        wiring.pad(ci, model, number, &as_supply).0,
                        PadState::Positive(_) | PadState::Other
                    )
                }
            })
        })
    });
    for pad in &mut pads {
        if pad.state == PadState::Tied {
            if regulator_fed || fed.iter().any(|f| *f != pad.power.family) {
                pad.state = PadState::Other;
            } else {
                pad.state = PadState::Open;
                pad.net.clear();
            }
        }
    }
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

    /// The finding for `flagged` pads. Where the board's symbol names one of
    /// them differently from the card, the diagnosis says so before the
    /// remedy; it never changes whether the finding is raised.
    fn finding(&self, message: &str, flagged: &[&Pad], nets: Vec<String>) -> LintFinding {
        let note = self.symbol_note(flagged);
        let message = if note.is_empty() {
            message.to_string()
        } else {
            message.replacen(REMEDY_MARKER, &format!("{note} {REMEDY_MARKER}"), 1)
        };
        LintFinding {
            check: LintCheck::PowerPinWiring,
            severity: Severity::High,
            message,
            refs: vec![self.comp.reference.clone()],
            nets,
        }
    }

    /// One sentence such as `The board's symbol calls pad 4 V_CC; the datasheet
    /// puts VCC on pad 6.` for each flagged pad the board's symbol names as something else, or
    /// whose power role the symbol puts on a pad the card does not.
    fn symbol_note(&self, flagged: &[&Pad]) -> String {
        let mut sentences: Vec<String> = Vec::new();
        for pad in flagged {
            let mut names: Vec<String> = Vec::new();
            let own = pad.function.as_str();
            if !pad.agrees && !own.is_empty() && !placeholder_function(own) {
                names.push(format!("pad {} {own}", pad.number));
            }
            // The same power pad on a pad the card gives some other role.
            for pin in &self.comp.pins {
                let moved = pin.number != pad.number
                    && !self
                        .pads
                        .iter()
                        .any(|p| p.number == pin.number && same_power(&p.power, &pad.power))
                    && symbol_power(pin.function.trim(), self.model.kind)
                        .is_some_and(|s| same_power(&s, &pad.power));
                let text = format!("pad {} {}", pin.number, pin.function.trim());
                if moved && !names.contains(&text) {
                    names.push(text);
                }
            }
            if names.is_empty() {
                continue;
            }
            sentences.push(format!(
                "The board's symbol calls {}; the datasheet (card '{}') puts {} on pad {}, so \
                 the symbol or footprint does not match the part fitted.",
                names.join(" and "),
                self.model.id,
                pad.role.to_ascii_uppercase(),
                pad.number,
            ));
        }
        sentences.dedup();
        sentences.join(" ")
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
                format!(
                    " (datasheet supply {}-{} V)",
                    format_number(lo, 2),
                    format_number(hi, 2)
                )
            });
            let to = window.map_or_else(
                || format!("the {} supply rail", family.to_ascii_uppercase()),
                |(lo, hi)| {
                    format!(
                        "a {}-{} V supply rail",
                        format_number(lo, 2),
                        format_number(hi, 2)
                    )
                },
            );
            let message = format!(
                "{} has no supply on its {family_up} input: {pads_text} {is} on no net, or on \
                 one only capacitors, test points, unfitted parts or the part's own pads share, \
                 so nothing can feed {it} (card '{id}' names {it} a supply pad{range}); that part \
                 of the IC is \
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
            out.push(self.finding(&message, &pads, Vec::new()));
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
            "{} has no return: {pads_text} {is} on no net, or on one only capacitors, test \
             points, unfitted parts or the part's own pads share (card '{id}' names {it} the \
             ground return), so the supply current has no way back and the part has no \
             reference; it cannot work, and any signal pin tied to another part back-feeds it \
             through its protection diodes. {REMEDY_MARKER}Connect {pads_text} to {to}.",
            self.who(),
            pads_text = pads_text(&pads),
            is = if pads.len() == 1 { "is" } else { "are" },
            id = self.model.id,
            it = if pads.len() == 1 { "it" } else { "them" },
        );
        Some(self.finding(&message, &pads, Vec::new()))
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
        let flagged: Vec<&Pad> = grounded.iter().chain(lifted.iter()).copied().collect();
        Some(self.finding(&message, &flagged, nets))
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
                    |(lo, hi)| {
                        format!(
                            "a {}-{} V supply rail",
                            format_number(lo, 2),
                            format_number(hi, 2)
                        )
                    },
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
        .map(|p| format!("{} ({})", p.number, p.label()))
        .collect();
    format!(
        "{} {}",
        if pads.len() == 1 { "pad" } else { "pads" },
        list.join(", ")
    )
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
        card_lib_of("digital", pins, extra)
    }

    /// [`card_lib`] for a card of another `kind`.
    fn card_lib_of(kind: &str, pins: &[(&str, &str)], extra: &str) -> ModelLibrary {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let mut map = String::new();
        for (p, r) in pins {
            writeln!(map, "\"{p}\" = \"{r}\"").unwrap();
        }
        // A card declaring behaviour (as FT231X's 3V3OUT law does) is not identity-only.
        let identity = if extra.contains("[[models.behavioral") {
            ""
        } else {
            "[models.params]\nidentity_only = true\nwarning = \"identity only\"\n\
             unlocked_by = \"validated behavior\"\n"
        };
        let card = format!(
            "[[models]]\nid = \"pwr_test\"\nkind = \"{kind}\"\ndescription = \"power pin fixture\"\n\
             [models.match]\nvalue_re = \"^PWRTEST$\"\n{identity}{extra}\n[models.pins]\n{map}"
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

    /// A schematic or layout lists every pad (its parts carry positions), so a
    /// pad it does not list is an unplaced unit, not an open pin; a netlist
    /// lists connected pads only, so there the same absence is an open pad.
    #[test]
    fn an_unlisted_pad_is_judged_only_where_the_board_lists_only_connected_pads() {
        let netlist = part("U1", "PWRTEST", &[("2", GND), ("3", SIG)]);
        let found =
            power_pin_lint(&board(&NETS, vec![netlist.clone(), load()]), &simple()).findings;
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["no supply", "pad 1 (VCC)"]);
        let mut drawn = netlist;
        drawn.position = Some((10.0, 20.0, 0.0));
        let found = power_pin_lint(&board(&NETS, vec![drawn, load()]), &simple()).findings;
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

    /// A decoupling capacitor from `net` to ground.
    fn decap(reference: &str, net: i64) -> Component {
        part(reference, "100n", &[("1", net), ("2", GND)])
    }

    /// Two supply pads of one family joined only to each other, or one with
    /// only its decoupling capacitor, are fed by nothing: open.
    #[test]
    fn a_supply_net_holding_only_its_own_family_and_capacitors_is_open() {
        let lib = card_lib(
            &[("1", "vdd"), ("2", "gnd"), ("4", "vdd2"), ("3", "in_plus")],
            "",
        );
        let u = part(
            "U1",
            "PWRTEST",
            &[("1", LONE), ("4", LONE), ("2", GND), ("3", SIG)],
        );
        let found = power_pin_lint(&board(&NETS, vec![u.clone(), load()]), &lib).findings;
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["no supply", "pads 1 (VDD), 4 (VDD2)"]);
        let with_cap = board(&NETS, vec![u, decap("C1", LONE), load()]);
        let found = power_pin_lint(&with_cap, &lib).findings;
        assert_eq!(found.len(), 1, "{found:#?}");
        // A test point, an unfitted feed and a pull-down add no source.
        let mut dnp = part("R5", "0", &[("1", LONE), ("2", V33)]);
        dnp.dnp = true;
        let u = part("U1", "PWRTEST", &[("1", LONE), ("2", GND), ("3", SIG)]);
        let bd = board(
            &NETS,
            vec![
                u,
                decap("C1", LONE),
                part("TP1", "TP", &[("1", LONE)]),
                dnp,
                part("R6", "10k", &[("1", LONE), ("2", GND)]),
                load(),
            ],
        );
        assert_eq!(power_pin_lint(&bd, &lib).findings.len(), 1);
    }

    /// Two supplies of different families tied only to each other: both open.
    #[test]
    fn two_supply_families_of_one_part_tied_together_are_both_open() {
        let lib = card_lib(
            &[("1", "vdd"), ("2", "gnd"), ("4", "vio"), ("3", "in_plus")],
            "",
        );
        let tied = run(&lib, &[("1", LONE), ("4", LONE), ("2", GND), ("3", SIG)]);
        assert_eq!(tied.len(), 2, "{tied:#?}");
    }

    /// A series part from a rail (a ferrite, a 0 ohm link, a resistor) or
    /// another part's pin may feed the net: wired.
    #[test]
    fn a_supply_net_a_series_part_or_another_pin_feeds_is_wired() {
        let u = part("U1", "PWRTEST", &[("1", LONE), ("2", GND), ("3", SIG)]);
        let fb = part("FB1", "600R", &[("1", V33), ("2", LONE)]);
        let bd = board(&NETS, vec![u.clone(), fb, decap("C1", LONE), load()]);
        assert!(power_pin_lint(&bd, &simple()).findings.is_empty());
        let q = part("Q1", "AO3401", &[("1", SIG), ("2", V33), ("3", LONE)]);
        let bd = board(&NETS, vec![u, q, decap("C1", LONE), load()]);
        assert!(power_pin_lint(&bd, &simple()).findings.is_empty());
    }

    /// A USB bridge's 3V3OUT feeds its VCCIO (FT231X datasheet Table 3.1:
    /// the internal LDO's output, made from VCC): wired while VCC is on a
    /// rail. VCC itself on 3V3OUT is a part powering itself from its own
    /// output, and an output pin the card declares no source for feeds
    /// nothing: both open.
    #[test]
    fn an_internal_regulator_output_feeds_another_supply_only() {
        let law = "[[models.behavioral.laws]]\nname = \"ldo\"\nkind = \"voltage\"\n\
                   a = \"v3v3out\"\nb = \"gnd\"\nexpr = \"max(0.0, min(v_vcc, 3.3))\"\nr_ohms = 1.0\n";
        let pins = [
            ("12", "vcc"),
            ("3", "gnd"),
            ("10", "v3v3out"),
            ("20", "vccio"),
            ("1", "rxd"),
        ];
        let bridge = card_lib(&pins, law);
        let pads = [
            ("12", V5),
            ("3", GND),
            ("10", LONE),
            ("20", LONE),
            ("1", SIG),
        ];
        let u = part("U1", "PWRTEST", &pads);
        let bd = board(&NETS, vec![u, decap("C1", LONE), load()]);
        assert!(power_pin_lint(&bd, &bridge).findings.is_empty());
        // The same tie with no law declaring 3V3OUT an output: open.
        let found = power_pin_lint(&bd, &card_lib(&pins, "")).findings;
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["no supply", "pad 20 (VCCIO)"]);
        // VCC on its own 3V3OUT, VCCIO on the rail: VCC is open.
        let pads = [
            ("12", LONE),
            ("3", GND),
            ("10", LONE),
            ("20", V33),
            ("1", SIG),
        ];
        let u = part("U1", "PWRTEST", &pads);
        let bd = board(&NETS, vec![u, decap("C1", LONE), load()]);
        let found = power_pin_lint(&bd, &bridge).findings;
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["no supply", "pad 12 (VCC)"]);
    }

    /// A return on a net only a capacitor shares is open; through a sense
    /// resistor or a low-side switch to ground it is wired.
    #[test]
    fn a_return_net_holding_only_a_capacitor_is_open() {
        let u = part("U1", "PWRTEST", &[("1", V33), ("2", LONE), ("3", SIG)]);
        let bd = board(
            &NETS,
            vec![
                u.clone(),
                part("C1", "100n", &[("1", V33), ("2", LONE)]),
                load(),
            ],
        );
        let found = power_pin_lint(&bd, &simple()).findings;
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["no return", "pad 2 (GND)"]);
        let sense = part("R7", "0.1", &[("1", LONE), ("2", GND)]);
        let bd = board(&NETS, vec![u, sense, load()]);
        assert!(power_pin_lint(&bd, &simple()).findings.is_empty());
    }

    /// A split exposed pad or separate paste apertures repeat one pad number:
    /// the connected one counts, whichever is listed first.
    #[test]
    fn of_several_pads_with_one_number_the_connected_one_counts() {
        for order in [[("2", 0), ("2", GND)], [("2", GND), ("2", 0)]] {
            let mut u = part(
                "U1",
                "PWRTEST",
                &[("1", V33), order[0], order[1], ("3", SIG)],
            );
            for pin in &mut u.pins {
                if pin.net == Some(0) {
                    pin.net = None;
                }
            }
            let found = power_pin_lint(&board(&NETS, vec![u, load()]), &simple()).findings;
            assert!(found.is_empty(), "{order:?}: {found:#?}");
        }
        // Every aperture open is still an open return.
        let mut u = part(
            "U1",
            "PWRTEST",
            &[("1", V33), ("2", 0), ("2", 0), ("3", SIG)],
        );
        u.pins[1].net = None;
        u.pins[2].net = None;
        assert_eq!(
            power_pin_lint(&board(&NETS, vec![u, load()]), &simple())
                .findings
                .len(),
            1
        );
    }

    /// A board part in an alternate package with more pads than the card: a
    /// card supply pad lands on one of the board's no-connect pins. The pad
    /// numbering is not the card's, so nothing is judged.
    #[test]
    fn a_part_with_pads_past_the_cards_last_is_not_judged() {
        let pads = [("1", UNCON), ("2", GND), ("3", SIG), ("4", V33), ("5", SIG)];
        let found = run(&simple(), &pads);
        assert!(found.is_empty(), "{found:#?}");
    }

    /// A card that leaves out a no-connect pin still reads the board's
    /// numbering: an extra pad inside its range is that pin.
    #[test]
    fn a_no_connect_the_card_omits_does_not_stop_the_judgement() {
        let lib = card_lib(&[("1", "vcc"), ("2", "gnd"), ("4", "in_plus")], "");
        let found = run(&lib, &[("1", UNCON), ("2", GND), ("3", UNCON), ("4", SIG)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["no supply", "pad 1 (VCC)"]);
    }

    /// A supply the card lists under `optional_supplies` (with the datasheet
    /// figure that leaves it open) is never required.
    #[test]
    fn a_supply_the_card_marks_optional_is_not_required() {
        let pins = [("1", "vcc"), ("2", "gnd"), ("3", "uvcc"), ("4", "in_plus")];
        let pads = [("1", V33), ("2", GND), ("4", SIG)];
        let found = run(&card_lib(&pins, ""), &pads);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["UVCC", "pad 3 (UVCC)"]);
        let optional = "[[models.optional_supplies]]\nrole = \"uvcc\"\n\
                        basis = \"datasheet figure: UVCC not connected\"\n";
        assert!(run(&card_lib(&pins, optional), &pads).is_empty());
    }

    /// The shipped USB bridge cards, wired as their datasheets' bus-powered
    /// figures draw them: REGIN and VBUS on the 5 V bus, VDD (the on-chip
    /// regulator's output) on a net that holds only its decoupling capacitor,
    /// and the CP2104's VIO tied to VDD. That is a fed part. The faulty
    /// control leaves VIO on a net nothing feeds, which still fires: only the
    /// regulator's own output pad is excused.
    #[test]
    fn a_bus_powered_bridge_feeds_its_own_vdd() {
        let lib = ModelLibrary::builtin();
        let bridge = |value: &str, vio: Option<i64>| {
            let mut pads = vec![("3", GND), ("6", LONE), ("7", V5), ("8", V5)];
            if value == "CP2104" {
                pads = vec![("2", GND), ("6", LONE), ("7", V5), ("8", V5)];
                pads.extend(vio.map(|n| ("5", n)));
            }
            let parts = vec![
                part("U1", value, &pads),
                part("C1", "100nF", &[("1", LONE), ("2", GND)]),
                part("C2", "4.7uF", &[("1", LONE), ("2", GND)]),
                part("J1", "USB_B_Micro", &[("1", V5), ("5", GND)]),
            ];
            power_pin_lint(&board(&NETS, parts), &lib).findings
        };
        assert!(
            bridge("CP2102", None).is_empty(),
            "{:#?}",
            bridge("CP2102", None)
        );
        let tied = bridge("CP2104", Some(LONE));
        assert!(tied.is_empty(), "{tied:#?}");
        // Faulty control: the regulator's input is left open, so nothing makes
        // the VDD that VIO is tied to.
        let unfed = power_pin_lint(
            &board(
                &NETS,
                vec![
                    part(
                        "U1",
                        "CP2104",
                        &[("2", GND), ("5", LONE), ("6", LONE), ("7", UNCON)],
                    ),
                    part("C1", "100nF", &[("1", LONE), ("2", GND)]),
                ],
            ),
            &lib,
        )
        .findings;
        assert_eq!(unfed.len(), 1, "{unfed:#?}");
        assert!(unfed[0].message.contains("VIO"), "{}", unfed[0].message);
        let open_vio = bridge("CP2104", Some(UNCON));
        assert_eq!(open_vio.len(), 1, "{open_vio:#?}");
        assert!(
            open_vio[0].message.contains("VIO"),
            "{}",
            open_vio[0].message
        );
    }

    /// Parts whose datasheet configuration leaves a primary supply pad on a net
    /// of its own decoupling capacitor only, because the chip's own regulator
    /// drives it: the DRV8353S `DVDD` (5 V regulator output, 1 uF), the MP2650
    /// `VCC` (3.6 V LDO output, 10 uF), the Pro Micro module's `VCC` (the
    /// on-board regulator's output) and the nRF52840 `VDD` in High Voltage
    /// mode (REG0's output). Wired that way the part is silent. The faulty
    /// control leaves every return pad open, which still fires: only the
    /// regulator's output pad is excused, not the rest of the part's power.
    #[test]
    fn a_regulator_output_supply_left_on_its_capacitor_is_silent() {
        /// (value, regulator-output pad, supply-side pads, return pads)
        type Case<'a> = (&'a str, &'a str, &'a [(&'a str, i64)], &'a [&'a str]);
        let lib = ModelLibrary::builtin();
        let check = |value: &str, pads: &[(&str, i64)]| {
            let parts = vec![
                part("U1", value, pads),
                part("C1", "1uF", &[("1", LONE), ("2", GND)]),
            ];
            power_pin_lint(&board(&NETS, parts), &lib).findings
        };
        let cases: [Case; 4] = [
            ("DRV8353S", "38", &[("3", V5), ("4", V5)], &["39", "25"]),
            ("MP2650", "26", &[("1", V5), ("22", V5)], &["4", "19", "27"]),
            ("ProMicro", "21", &[("24", V5)], &["3", "4", "23"]),
            ("nRF52840", "2", &[], &["1"]),
        ];
        for (value, out_pad, supply_side, returns) in cases {
            let mut pads: Vec<(&str, i64)> = supply_side.to_vec();
            pads.push((out_pad, LONE));
            pads.extend(returns.iter().map(|r| (*r, GND)));
            let wired = check(value, &pads);
            assert!(wired.is_empty(), "{value}: {wired:#?}");
            // Faulty control: no return anywhere.
            let mut broken: Vec<(&str, i64)> = supply_side.to_vec();
            broken.push((out_pad, LONE));
            broken.extend(returns.iter().map(|r| (*r, UNCON)));
            let found = check(value, &broken);
            assert_eq!(found.len(), 1, "{value}: {found:#?}");
            says(&found[0], &["no return"]);
        }
    }

    /// The RP2040's `DVDD` pads are wired to its own `VREG_VOUT` off-chip, so
    /// that tie is a fed core supply while the I/O supply is on a rail; with the
    /// I/O and USB supplies unpowered it fires on those, and a `DVDD` tied to
    /// nothing still fires.
    #[test]
    fn an_rp2040_core_supply_tied_to_its_regulator_output_is_fed() {
        let lib = ModelLibrary::builtin();
        let run = |pads: &[(&str, i64)]| {
            // The QFN-56 footprint selects the card that maps every pad; the
            // identity entry for a bare `RP2040` knows only the boot strap.
            let mut u = part("U1", "RP2040", pads);
            u.footprint = "Package_DFN_QFN:QFN-56-1EP_7x7mm_P0.4mm_EP3.2x3.2mm".into();
            let parts = vec![u, part("C1", "1uF", &[("1", LONE), ("2", GND)])];
            power_pin_lint(&board(&NETS, parts), &lib).findings
        };
        let wired = [
            ("1", V33),
            ("48", V33),
            ("44", V33),
            ("23", LONE),
            ("45", LONE),
            ("50", LONE),
            ("57", GND),
        ];
        let found = run(&wired);
        assert!(found.is_empty(), "{found:#?}");
        // DVDD tied to nothing: the regulator output pad is not on its net.
        let untied = [
            ("1", V33),
            ("48", V33),
            ("44", V33),
            ("23", LONE),
            ("57", GND),
        ];
        let found = run(&untied);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["no supply", "pads 23 (DVDD), 50 (DVDD2)"]);
        // The regulator is made from VREG_VIN and IOVDD: with those open, the
        // tied DVDD is not a supply, and IOVDD itself is the finding.
        let dead = [("1", UNCON), ("23", LONE), ("45", LONE), ("57", GND)];
        let found = run(&dead);
        assert!(!found.is_empty(), "{found:#?}");
    }

    /// `VS` is an amplifier's supply; on a gate driver it is the switch node.
    #[test]
    fn vs_is_judged_as_a_supply_only_on_an_amplifier() {
        let pins = [("1", "vs"), ("2", "gnd"), ("3", "in_plus")];
        let pads = [("2", GND), ("3", SIG)];
        let amp = card_lib_of("opamp", &pins, "");
        let found = run(&amp, &pads);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["no supply", "pad 1 (VS)"]);
        assert!(run(&card_lib_of("vreg", &pins, ""), &pads).is_empty());
    }

    /// The board's symbol puts the supply on another pad than the card (a
    /// five-lead symbol on a six-lead part): the finding stands and says so.
    #[test]
    fn a_symbol_that_names_the_pads_differently_is_named_in_the_finding() {
        let lib = card_lib(
            &[
                ("1", "scl"),
                ("2", "vss"),
                ("3", "sda"),
                ("4", "a1"),
                ("5", "a0"),
                ("6", "vcc"),
            ],
            "",
        );
        let mut u = part(
            "U1",
            "PWRTEST",
            &[
                ("1", SIG),
                ("2", GND),
                ("3", SIG),
                ("4", V33),
                ("5", GND),
                ("6", UNCON),
            ],
        );
        for (pin, function) in u
            .pins
            .iter_mut()
            .zip(["SCL", "V_SS", "SDA", "V_CC", "WP", ""])
        {
            pin.function = function.to_string();
        }
        let found = power_pin_lint(&board(&NETS, vec![u, load()]), &lib).findings;
        assert_eq!(found.len(), 1, "{found:#?}");
        says(
            &found[0],
            &[
                "no supply",
                "pad 6 (VCC)",
                "The board's symbol calls pad 4 V_CC; the datasheet (card 'pwr_test') puts VCC on pad 6",
            ],
        );
        // The note sits in the diagnosis, before the remedy.
        let note = found[0].message.find("The board's symbol").unwrap();
        assert!(note < found[0].message.find(REMEDY_MARKER).unwrap());
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
        assert!(run(&module, &[("2", GND)]).is_empty());
        // Without the supply_outputs row the same open pad is a missing supply.
        assert_eq!(
            run(&card_lib(&[("1", "vcc"), ("2", "gnd")], ""), &[("2", GND)]).len(),
            1
        );
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

    /// The ESP32-S3-MINI-1 module's pads, wired the way its datasheet
    /// (Table 3-1) numbers them: 3V3 on pad 3, ground on pads 1, 2, 42, 43 and
    /// 46-65. `supply` and `ground` are the nets put on those pads.
    fn s3_mini_pads(supply: i64, ground: i64) -> Vec<(String, i64)> {
        let mut pads = vec![("3".to_string(), supply)];
        for gnd in [1, 2, 42, 43].into_iter().chain(46..=65) {
            pads.push((gnd.to_string(), ground));
        }
        pads
    }

    fn s3_mini_findings(pads: &[(String, i64)]) -> Vec<LintFinding> {
        let refs: Vec<(&str, i64)> = pads.iter().map(|(p, n)| (p.as_str(), *n)).collect();
        let u = part("U1", "ESP32-S3-MINI-1", &refs);
        power_pin_lint(&board(&NETS, vec![u, load()]), &ModelLibrary::builtin()).findings
    }

    /// A module's footprint pads are the module's, not the chip's: with the
    /// module card's pad map a 3V3 symbol pin landed on a ground pad of the
    /// footprint leaves the supply pad open, and the correctly wired module
    /// stays silent. (A supply pad put on ground is left to the envelope check,
    /// the card's `supply_range` row judges it.)
    #[test]
    fn a_module_with_its_supply_on_a_ground_pad_is_caught_by_its_pad_map() {
        let good = s3_mini_findings(&s3_mini_pads(V33, GND));
        assert!(good.is_empty(), "{good:#?}");

        // The 3V3 net lands on pad 2 (a ground pad of the MINI-1; the WROOM-1
        // has its 3V3 there) and pad 3, the module's real 3V3 pad, is unwired.
        let mut swapped = s3_mini_pads(V33, GND);
        swapped.remove(0);
        swapped.iter_mut().find(|(p, _)| p == "2").unwrap().1 = V33;
        let found = s3_mini_findings(&swapped);
        assert_eq!(found.len(), 1, "{found:#?}");
        says(&found[0], &["no supply", "pad 3 (VDD)"]);
    }

    /// The other tables of supply roles are subsets of the one vocabulary.
    #[test]
    fn the_checks_role_lists_agree_with_the_vocabulary() {
        for role in crate::checks::rail_decoupling::SUPPLY_ROLES {
            assert!(
                power_pin(role, ComponentKind::Digital)
                    .is_some_and(|p| p.kind == PowerKind::Supply),
                "{role}"
            );
        }
        for role in crate::checks::envelope::IC_SUPPLY_ROLES {
            // A bare voltage name is a module's pin, not a primary supply.
            if role != "3v3" {
                assert!(
                    power_pin(role, ComponentKind::Digital)
                        .is_some_and(|p| p.kind == PowerKind::Supply),
                    "{role}"
                );
            }
        }
    }
}
