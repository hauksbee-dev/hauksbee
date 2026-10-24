//! Model-aware driver-contention lint: two MODELLED push-pull outputs on one net.
//!
//! The extract layer already has an output-contention check
//! ([`LintCheck::OutputContention`] in `hauksbee-extract/src/netlint.rs`). That
//! one reads the schematic's PIN ELECTRICAL TYPES: it fires when two pins a
//! symbol author typed `output` / `power_out` share a net with nothing to
//! resolve them. It is calibrated to zero false positives, and to get there it
//! treats an `input` or `bidirectional` pin anywhere on the net as a resolver,
//! because symbol authors routinely type shared/IRQ pins as outputs.
//!
//! That calibration leaves a hole exactly where hauksbee knows the most. On the
//! field board that motivated this check, a mis-mapped logic model put a 74HC08
//! (quad AND, push-pull, no output enable) onto a net an MCU also drove as a
//! GPIO. Two drivers, one net, no series element. The MCU's GPIO pad is typed
//! `bidirectional` in the symbol, so the pin-type check saw a resolver and went
//! quiet, and `--lint` printed "Looks healthy" over a real fight.
//!
//! This check answers the same question from the other side: not "what did the
//! symbol author type" but "what did hauksbee BIND". A part that resolves to a
//! digital model has a pad->role map and a declared output set; the binder
//! stamps a Thevenin [`PinDriver`](hauksbee_bind::drivers::PinDriver) on every connected
//! output role, which is precisely a push-pull driver. If two DIFFERENT parts
//! stamp a driver onto the same net, the co-simulation itself is about to solve
//! two voltage sources fighting through their output resistances, and the answer
//! it prints will be a meaningless mid-rail number. That is worth a High finding
//! whatever the symbol's pin types said.
//!
//! ## What counts as a push-pull output
//!
//! [`output_roles`] is the binder's own answer, so
//! this check and the binder cannot disagree: a model with a `[models.logic]`
//! block declares `outputs` explicitly; a model without one falls back to the
//! `y*` role convention the synthesized passthrough mirrors onto. The check maps
//! each such role back to a pad through the model's `[models.pins]` map and
//! reads that pad's net off the extracted component.
//!
//! Only the model's OWN pad map is consulted, never the pin-function or
//! pin-rule-table inference the binder layers on top
//! (`role_node_map_guessed`). A guessed role is exactly the kind of soft
//! evidence a High-severity finding must not rest on.
//!
//! ## Exclusions
//!
//! - **Tri-state outputs.** Any output covered by a `[models.logic.tristate]`
//!   group is skipped. That is how the db expresses a 3-state part: the 74HC125
//!   quad 3-state buffer tri-states `y1..y4` on its per-gate `oe_n_*`, and the
//!   74HC595 tri-states `qa..qh` on `oe_n`. Sharing a net is the intended
//!   arrangement for those parts (that is what an output enable is FOR), so they
//!   must never fire here. Open-drain is not expressible for a digital-kind
//!   model at all (the `open_drain` flag lives on the behavioural pin schema, a
//!   different kind), so within this check's scope "declared output and not
//!   tri-stated" is push-pull.
//! - **One part driving its own net twice.** Two output pads of a single chip on
//!   one net is an internal short the symbol expresses, not an inter-part fight,
//!   and it is usually a deliberate parallel-drive idiom. Two DISTINCT
//!   references are required.
//! - **Ground, unconnected, and no-net.** Nothing to contend over.
//! - **DNP parts.** Not assembled, so electrically absent.
//! - **A series resistor does not separate two drivers.** Net extraction never
//!   merges across a two-terminal passive, so two outputs behind one small
//!   series resistor are on two nets. They are still wired to each other: the
//!   resistor limits the current of the fight, not the fight. A single resistor
//!   (or one element of a resistor network) of up to a few kilohms between two
//!   signal nets is read as a wire (`signal_direction::series`), and the two
//!   nets are one line here. An R on a net as a pull-up, a divider leg or a load
//!   is not a series element between the drivers and joins nothing, and a net
//!   that two resistors reach is a junction, not a hop.
//!
//! ## What it deliberately does not reach
//!
//! MCU GPIO direction is firmware state, not netlist state: whether PB5 is an
//! output at the moment the 74HC08 drives is decided by a DDR write the lint
//! cannot see. The binder itself stamps every MCU GPIO driver DISABLED
//! (high-impedance) and only the scheduler enables one, on the pin's first
//! firmware drive. Firing statically on "modelled output shares a net with an
//! MCU pad" would flag every logic output feeding an MCU input, which is the
//! single most common way a logic gate is used, so the model-to-MCU half of the
//! field case is out of static reach. This check catches MODEL-to-MODEL
//! contention only, and its finding prose says so; the model-to-firmware half
//! belongs to co-sim time, where the scheduler knows the real pin directions.
//! ## Pads bound to a UART transmit function
//!
//! [`transmit_contention_lint`] reaches the one MCU pad whose direction the
//! board itself settles. A pad is a bound push-pull output when the card states
//! it is a UART transmit pad and no receive pad (its `uart_tx` table, or the
//! transmit line its role names) AND the line is a serial one (the net is named
//! for a serial port, or a part on it has a serial pin). A transmit pad on a
//! serial line is the hardware UART's output; two of them, or one facing another
//! part's transmit pin typed output, fight. Where the card gives the pad both
//! directions, none, or the net is not a serial line, the pad is a GPIO whose
//! direction is firmware state and the check abstains. SPI `MOSI`/`SCK` and the
//! like are not read here: whether the MCU is the controller (and so drives
//! them) or a target is a mode the netlist cannot state.
//!
//! That half is covered by the scheduler's runtime monitor
//! (`Scheduler::detect_driver_contention`), which fires when a firmware-enabled
//! GPIO driver and an enabled modelled push-pull output share a net, using the
//! same `output_roles`/tri-state classification this check rests on.

use super::is_unconnected_net;
use super::pin_mux::{receive_pads, serial_pin, uart_pad, UartPad};
use super::signal_direction::series::SeriesLinks;
use super::signal_direction::{classify, Dir, Member};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_connector_like, Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Pin,
    Severity,
};
use hauksbee_ir::pin_functions::names_a_uart;
use hauksbee_models::{ComponentKind, ModelEntry, ModelLibrary};

use hauksbee_bind::digital::output_roles;

/// One modelled push-pull output pin found on a net.
struct Driver {
    reference: String,
    pad: String,
    role: String,
}

/// One component that bound to a digital-kind model, as this check saw it.
///
/// This is the calibration's evidence surface: it says how far each candidate
/// part got through the classification, so a corpus sweep can prove its silence
/// is real (parts bound, outputs presented, exclusions applied) rather than
/// vacuous (nothing ever reached the classifier).
pub struct BoundDigitalPart {
    pub reference: String,
    pub value: String,
    /// The resolved model's stable id, e.g. `"74hc125"`.
    pub model_id: String,
    /// Output roles the model declares (what the binder would stamp drivers on).
    pub output_roles: usize,
    /// Of those, roles excluded here because a `[models.logic.tristate]` group
    /// covers them.
    pub tristateable_roles: usize,
    /// Push-pull output pads whose pad number is absent from the extracted
    /// footprint (the model maps a pad the board does not carry).
    pub pads_absent: usize,
    /// Push-pull output pads present but on no net.
    pub pads_unrouted: usize,
    /// Push-pull output pads on ground or KiCad-unconnected nets.
    pub pads_on_excluded_nets: usize,
    /// Push-pull output pads that survived onto an eligible net, i.e. the pins
    /// the contention logic actually reasons about.
    pub pushpull_pins: usize,
}

/// Everything the classification pass saw on one board: the per-part evidence
/// trail plus the net map `contention_lint` judges.
pub struct Scan {
    pub parts: Vec<BoundDigitalPart>,
    by_net: BTreeMap<i64, Vec<Driver>>,
}

impl Scan {
    /// Total modelled push-pull output pins on eligible nets.
    pub fn pushpull_driver_pins(&self) -> usize {
        self.by_net.values().map(Vec::len).sum()
    }

    /// Total output roles the bound digital parts presented to the classifier,
    /// tri-stateable ones included. This is the honest "did the check engage"
    /// measure: a board full of 3-state buffers exercises resolution, role
    /// enumeration, and the tri-state exclusion even though it yields zero
    /// push-pull pins.
    #[must_use]
    pub fn output_roles_presented(&self) -> usize {
        self.parts.iter().map(|p| p.output_roles).sum()
    }
}

/// Kinds the binder routes through `bind_digital`, i.e. the kinds whose
/// connected output roles get a stamped Thevenin driver. Kept in step with
/// `binder::bind_component`'s match arms.
const fn is_digital_kind(kind: ComponentKind) -> bool {
    matches!(
        kind,
        ComponentKind::Digital | ComponentKind::ShiftRegister | ComponentKind::Adc
    )
}

/// The output roles of `model` that are tri-stateable, i.e. that the part can
/// release to high impedance under an output-enable pin. Expanded from the
/// `[models.logic.tristate]` group keys, which may be a single output name
/// (`"y1"`) or an inclusive range over the outputs declaration order
/// (`"qa..qh"`).
///
/// A group key that fails to expand means the spec is malformed in a way this
/// check cannot reason about; rather than guess, every output of the part is
/// reported as tri-stateable, which suppresses the part entirely. Staying quiet
/// on a spec we do not understand is the only safe direction for a
/// High-severity finding.
fn tristateable_outputs(model: &ModelEntry) -> HashSet<String> {
    let mut out = HashSet::new();
    for group in model.logic.tristate.keys() {
        match model.logic.expand_tristate_group(group) {
            Ok(names) => out.extend(names),
            Err(_) => return model.logic.outputs.iter().cloned().collect(),
        }
    }
    out
}

/// Walk the board once and classify every modelled digital output pad. The
/// whole of the check's classification lives here; `contention_lint` only
/// decides which nets in the result are worth a finding.
pub fn scan(board: &ExtractedBoard, lib: &ModelLibrary) -> Scan {
    let net_name: HashMap<i64, &str> = board.nets.iter().map(|n| (n.id, n.name.as_str())).collect();

    let mut parts = Vec::new();
    let mut by_net: BTreeMap<i64, Vec<Driver>> = BTreeMap::new();

    // A component with no model says nothing about its pin directions.
    for (comp, model) in super::resolved_components(board, lib) {
        if !is_digital_kind(model.kind) {
            continue;
        }
        let all_roles = output_roles(&model);
        let tri = tristateable_outputs(&model);
        let pushpull: HashSet<&str> = all_roles
            .iter()
            .filter(|r| !tri.contains(r.as_str()))
            .map(String::as_str)
            .collect();

        let mut part = BoundDigitalPart {
            reference: comp.reference.clone(),
            value: comp.value.clone(),
            model_id: model.id.clone(),
            output_roles: all_roles.len(),
            tristateable_roles: all_roles
                .iter()
                .filter(|r| tri.contains(r.as_str()))
                .count(),
            pads_absent: 0,
            pads_unrouted: 0,
            pads_on_excluded_nets: 0,
            pushpull_pins: 0,
        };

        // The model's own pad->role map is the only role source consulted; see
        // the module docs on why the binder's guessed roles are excluded.
        for (pad, role) in &model.pins {
            if !pushpull.contains(role.as_str()) {
                continue;
            }
            let Some(pin) = comp.pins.iter().find(|p| &p.number == pad) else {
                part.pads_absent += 1; // the footprint does not carry that pad
                continue;
            };
            let Some(id) = pin.net.filter(|&id| id != 0) else {
                part.pads_unrouted += 1;
                continue;
            };
            let name = net_name.get(&id).copied().unwrap_or("");
            if super::is_ground_name(name) || is_unconnected_net(name) {
                part.pads_on_excluded_nets += 1;
                continue;
            }
            part.pushpull_pins += 1;
            by_net.entry(id).or_default().push(Driver {
                reference: comp.reference.clone(),
                pad: pad.clone(),
                role: role.clone(),
            });
        }
        parts.push(part);
    }

    Scan { parts, by_net }
}

/// Flag every net carrying push-pull outputs of two or more different modelled
/// digital parts. See the module docs for the classification and the exclusions.
#[must_use]
pub fn contention_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    let links = SeriesLinks::of(board);
    // The drivers by line: a net behind a series resistor joins the net on its
    // other side.
    let mut by_line: BTreeMap<i64, Vec<Driver>> = BTreeMap::new();
    for (id, drivers) in scan(board, lib).by_net {
        by_line.entry(links.line(id)).or_default().extend(drivers);
    }

    for (line, drivers) in by_line {
        let refs: BTreeSet<&str> = drivers.iter().map(|d| d.reference.as_str()).collect();
        if refs.len() < 2 {
            continue; // one part driving its own net is not an inter-part fight
        }
        let names = links.names(board, line);
        let name = names.first().cloned().unwrap_or_default();
        let listed: Vec<String> = drivers
            .iter()
            .map(|d| format!("{}.{} ({})", d.reference, d.pad, d.role))
            .collect();
        let (lead, between) = match (links.link(line), names.as_slice()) {
            (Some(link), [first, second]) => (
                format!(
                    "net '{first}' (wired through the series resistor {} to net '{second}')",
                    link.describe()
                ),
                "only a series resistor between them, which limits the current of the fight \
                 and does not end it",
            ),
            _ => (format!("net '{name}'"), "no series element between them"),
        };
        report.findings.push(LintFinding {
            check: LintCheck::OutputContention,
            severity: Severity::High,
            message: format!(
                "{lead} is driven by {} modelled push-pull outputs of {} different parts with \
                 {between}: {}. Two outputs driving one net fight: whichever \
                 one wins, the net sits at an indeterminate level, both parts pass current well \
                 outside their output ratings, and the simulation reports a mid-rail number that \
                 looks like data. Check the schematic for a genuine short, and check the model \
                 mapping with `hauksbee models resolve` in case a part bound to the wrong pinout. \
                 Only model-to-model contention is visible here: an MCU GPIO's direction is set by \
                 firmware, so a firmware-driven fight on this net surfaces at co-sim time instead.",
                drivers.len(),
                refs.len(),
                listed.join(", ")
            ),
            refs: refs.iter().map(std::string::ToString::to_string).collect(),
            nets: names,
        });
    }

    report
}

/// One transmitter found on a serial line, as a message names it.
struct Transmitter {
    reference: String,
    text: String,
}

/// A card's UART-transmit pad on a net: the part, its pin and its card.
type BoundPad<'a> = (&'a Component, &'a Pin, &'a ModelEntry);

/// The cards by reference designator.
type Cards<'a> = HashMap<&'a str, &'a ModelEntry>;

/// A general-purpose port pad of a card's pin map, by its role (`pd1_txd`,
/// `gpio1_u0txd`, `p01_txd`).
fn is_port_pad(role: &str) -> bool {
    let r = role.trim().to_ascii_lowercase();
    let head = r.split('_').next().unwrap_or("");
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let letter_port = head.len() >= 3
        && head.starts_with('p')
        && head.as_bytes()[1].is_ascii_lowercase()
        && digits(&head[2..]);
    letter_port
        || ["gpio", "io", "gp", "p", "d"]
            .iter()
            .any(|p| head.strip_prefix(p).is_some_and(digits))
}

/// The pads each MCU card binds to UART transmit, grouped by net.
fn bound_transmit_pads<'a>(
    resolved: &'a [(&'a Component, ModelEntry)],
) -> BTreeMap<i64, Vec<BoundPad<'a>>> {
    let mut bound: BTreeMap<i64, Vec<BoundPad<'a>>> = BTreeMap::new();
    for (c, m) in resolved {
        if m.kind != ComponentKind::Mcu {
            continue;
        }
        for pin in &c.pins {
            let Some(net) = pin.net.filter(|&id| id != 0) else {
                continue;
            };
            let port = m.pins.get(&pin.number).is_some_and(|r| is_port_pad(r));
            if port && uart_pad(m, &pin.number, true) == UartPad::Tx {
                bound.entry(net).or_default().push((c, pin, m));
            }
        }
    }
    bound
}

/// Whether the line is serial: named for a serial port, or a fitted
/// non-MCU, non-connector part on it has a serial pin.
fn is_serial_line(names: &[String], members: &[(&Component, &Pin)], cards: &Cards<'_>) -> bool {
    names.iter().any(|n| names_a_uart(n))
        || members.iter().any(|(c, p)| {
            let card = cards.get(c.reference.as_str()).copied();
            !is_connector_like(c)
                && AssemblyState::of(c).fitted().is_some()
                && card.is_none_or(|m| m.kind != ComponentKind::Mcu)
                && serial_pin(
                    c,
                    p,
                    card.and_then(|m| m.pins.get(&p.number)).map(String::as_str),
                )
        })
}

/// The transmitters on a line beyond its bound pads: every fitted pin the
/// symbol types an output that names a serial line. Also whether a tri-state or
/// open-drain pin is on the line, which may be what shares it.
fn other_transmitters(
    members: &[(&Component, &Pin)],
    pads: &[BoundPad<'_>],
    cards: &Cards<'_>,
) -> (Vec<Transmitter>, bool) {
    let mut found = Vec::new();
    let mut released = false;
    for (c, p) in members {
        let is_bound = pads
            .iter()
            .any(|(bc, bp, _)| bc.reference == c.reference && bp.number == p.number);
        if is_bound || is_connector_like(c) || AssemblyState::of(c).fitted().is_none() {
            continue;
        }
        let card = cards.get(c.reference.as_str()).copied();
        match classify(c, p, card) {
            Member::Pin(Dir::Open) => released = true,
            Member::Pin(Dir::Out)
                if serial_pin(
                    c,
                    p,
                    card.and_then(|m| m.pins.get(&p.number)).map(String::as_str),
                ) =>
            {
                let name = if p.function.trim().is_empty() {
                    String::new()
                } else {
                    format!(" ({})", p.function.trim())
                };
                found.push(Transmitter {
                    reference: c.reference.clone(),
                    text: format!("{}.{}{name}, typed output", c.reference, p.number),
                });
            }
            _ => {}
        }
    }
    (found, released)
}

/// The finding for a serial line, opened by `lead` (`net 'A'`), joined by `found`
/// transmitters.
fn transmit_finding(
    lead: &str,
    nets: Vec<String>,
    found: &[Transmitter],
    model: &ModelEntry,
) -> LintFinding {
    let refs: BTreeSet<&str> = found.iter().map(|t| t.reference.as_str()).collect();
    let listed: Vec<&str> = found.iter().map(|t| t.text.as_str()).collect();
    let receive = receive_pads(model);
    let move_to = if receive.is_empty() {
        "wire each transmitter to the other side's receive pin".to_owned()
    } else {
        format!(
            "wire the transmit pad to the other side's receive pin, and give the line a \
             pad that can receive: {receive}"
        )
    };
    LintFinding {
        check: LintCheck::OutputContention,
        severity: Severity::High,
        message: format!(
            "{lead} joins {} UART transmitters: {}. A UART transmit pad drives its \
             line push-pull, so two of them on one wire fight whenever their levels \
             differ, and neither hears the other: the link cannot work (TX must go to \
             the other side's RX). {move_to}. If the firmware never enables the hardware \
             transmit on this pad this does not apply, but then the pad is a plain input \
             and the line has no transmitter",
            found.len(),
            hauksbee_ir::vocabulary::names_summary(&listed),
        ),
        refs: refs.iter().map(|r| (*r).to_owned()).collect(),
        nets,
    }
}

/// The nets a part touches (unconnected pads left out).
fn net_set(c: &Component) -> BTreeSet<i64> {
    c.pins
        .iter()
        .filter_map(|p| p.net.filter(|&n| n != 0))
        .collect()
}

/// Whether two parts are one device drawn twice: a board that carries a
/// footprint for each of two interchangeable chips (a DIP beside a TQFP, an
/// `ATmega168` beside an `ATmega328P`) wires every pad of both to the same nets and
/// fits one. Two parts on the same set of at least six nets are read that way;
/// a second controller on the line has pins of its own.
fn alternate_footprints(a: &Component, b: &Component) -> bool {
    let nets = net_set(a);
    nets.len() >= MIN_SHARED_NETS && nets == net_set(b)
}

/// The fewest nets two parts must share, all of them, to read as alternates.
const MIN_SHARED_NETS: usize = 6;

/// The parts the transmitters belong to, with alternate footprints of one device
/// counted once: only one of them is on the assembled board.
fn distinct_parts<'a>(board: &'a ExtractedBoard, found: &'a [Transmitter]) -> Vec<&'a str> {
    let mut parts: Vec<&str> = Vec::new();
    for t in found {
        let r = t.reference.as_str();
        let twin = |q: &&str| {
            *q == r
                || board
                    .components
                    .iter()
                    .find(|c| c.reference == r)
                    .zip(board.components.iter().find(|c| c.reference == *q))
                    .is_some_and(|(a, b)| alternate_footprints(a, b))
        };
        if !parts.iter().any(twin) {
            parts.push(r);
        }
    }
    parts
}

/// Flag every serial line that carries two transmitters.
///
/// An MCU pad bound to UART transmit beside another MCU's transmit pad or another
/// part's transmit pin typed an output. A line includes the net on the far side of
/// one small series resistor. See the module docs.
#[must_use]
pub fn transmit_contention_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    let resolved: Vec<(&Component, ModelEntry)> = super::resolved_components(board, lib).collect();
    let cards: Cards<'_> = resolved
        .iter()
        .map(|(c, m)| (c.reference.as_str(), m))
        .collect();
    let links = SeriesLinks::of(board);
    let mut by_line: BTreeMap<i64, Vec<BoundPad<'_>>> = BTreeMap::new();
    for (net, pads) in bound_transmit_pads(&resolved) {
        by_line.entry(links.line(net)).or_default().extend(pads);
    }
    for (line, pads) in by_line {
        let names = links.names(board, line);
        if names.is_empty()
            || names
                .iter()
                .any(|n| super::is_ground_name(n) || is_unconnected_net(n))
        {
            continue;
        }
        let members = links.members(board, line);
        if !is_serial_line(&names, &members, &cards) {
            continue;
        }
        let mut found: Vec<Transmitter> = pads
            .iter()
            .map(|(c, p, m)| Transmitter {
                reference: c.reference.clone(),
                text: format!(
                    "{}.{} ({}, the card's UART transmit pad)",
                    c.reference,
                    p.number,
                    m.pins
                        .get(&p.number)
                        .map_or(String::new(), |r| r.to_ascii_uppercase()),
                ),
            })
            .collect();
        let (others, released) = other_transmitters(&members, &pads, &cards);
        found.extend(others);
        let parts = distinct_parts(board, &found);
        if parts.len() >= 2 && !released {
            let lead = match (links.link(line), names.as_slice()) {
                (Some(link), [first, second]) => format!(
                    "net '{first}' (wired through the series resistor {} to net '{second}')",
                    link.describe()
                ),
                _ => format!("net '{}'", names[0]),
            };
            report
                .findings
                .push(transmit_finding(&lead, names, &found, pads[0].2));
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;
    use hauksbee_extract::Component;

    /// A component whose pads are wired by `(pad number, net id)`.
    fn comp(reference: &str, value: &str, pads: &[(&str, i64)]) -> Component {
        Component {
            lib_id: "Logic:X".to_string(),
            ..part(reference, value, pads)
        }
    }

    fn run(b: &ExtractedBoard) -> NetLintReport {
        contention_lint(b, &ModelLibrary::builtin())
    }

    /// Two modelled push-pull outputs on one net fight and must fire: a 74HC08
    /// AND-gate output (pad 3 = `y1`) and a 74HC32 OR-gate output (pad 3 =
    /// `y1`) both land on `SHARED`. Neither part has an output enable.
    #[test]
    fn two_pushpull_outputs_on_one_net_fire() {
        let b = board(
            &[(1, "A"), (2, "B"), (3, "SHARED"), (4, "C"), (5, "D")],
            vec![
                comp("U1", "74HC08", &[("1", 1), ("2", 2), ("3", 3)]),
                comp("U2", "74HC32", &[("1", 4), ("2", 5), ("3", 3)]),
            ],
        );
        let r = run(&b);
        assert_eq!(
            r.findings.len(),
            1,
            "one contention finding: {:?}",
            r.findings
        );
        let f = &r.findings[0];
        assert_eq!(f.check, LintCheck::OutputContention);
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.refs, vec!["U1".to_string(), "U2".to_string()]);
        assert_eq!(f.nets, vec!["SHARED".to_string()]);
        assert!(f.message.contains("hauksbee models resolve"));
        assert!(f.message.contains("co-sim"));
    }

    /// THE FIELD CASE, as reported: a 74HC08 whose gate-4 output (pad 11 =
    /// `y4`) was mis-mapped onto net STROBE, which an `ATmega328P` also drives as
    /// a GPIO output (PB1, pad 13 on the TQFP-32) because the firmware set DDRB
    /// bit 1. Two push-pull drivers, one net, no series element.
    ///
    /// This check does NOT fire on it, and that is a documented limitation, not
    /// an oversight: at lint time there is no firmware, the binder stamps every
    /// MCU GPIO driver disabled (high-impedance), and a logic output sharing a
    /// net with an MCU pad is indistinguishable from the single most common
    /// healthy topology there is, a gate output feeding an MCU input. Firing
    /// here would put a High finding on virtually every board that wires logic
    /// to an MCU, which the zero-false-positive gate forbids. The model-to-MCU
    /// half of the field case is therefore out of STATIC reach; it is caught
    /// at co-sim time, where the scheduler learns real pin directions from the
    /// firmware's DDR writes (`Scheduler::detect_driver_contention`, which has
    /// its own tests). What the static check DOES catch is the other face
    /// of the same field failure: the mis-mapped model output landing on a net
    /// with any OTHER modelled push-pull output (see
    /// `two_pushpull_outputs_on_one_net_fire`).
    ///
    /// If this assertion ever starts failing because the check learned to fire
    /// here, that is a deliberate design change: re-read the paragraph above
    /// and be sure the false-positive argument has actually been answered.
    #[test]
    fn field_case_model_vs_mcu_gpio_is_out_of_static_reach() {
        let b = board(
            &[(1, "A4"), (2, "B4"), (3, "STROBE"), (4, "VCC"), (5, "GND")],
            vec![
                comp("U1", "74HC08", &[("1", 1), ("2", 2), ("11", 3)]),
                // The ATmega328P resolves to an MCU-kind model; its pad 13
                // (PB1) sits on STROBE. Direction is firmware state the lint
                // cannot see.
                comp("U2", "ATmega328P-AU", &[("13", 3), ("4", 4), ("3", 5)]),
            ],
        );
        let r = run(&b);
        assert!(
            r.findings.is_empty(),
            "model-vs-MCU contention is firmware-dependent and must not fire \
             statically: {:?}",
            r.findings
        );
    }

    /// An output feeding an input is the normal case and must stay silent: the
    /// 74HC08's `y1` (pad 3) drives the 74HC32's `a1` (pad 1).
    #[test]
    fn output_into_input_is_silent() {
        let b = board(
            &[(1, "A"), (2, "B"), (3, "MID"), (4, "C"), (5, "OUT")],
            vec![
                comp("U1", "74HC08", &[("1", 1), ("2", 2), ("3", 3)]),
                comp("U2", "74HC32", &[("1", 3), ("2", 4), ("3", 5)]),
            ],
        );
        assert!(run(&b).findings.is_empty(), "{:?}", run(&b).findings);
    }

    /// A 3-state part sharing a net is the intended arrangement, not a fault: the
    /// 74HC125's `y1`/`y2` (pads 3 and 6) are tri-stated by their own `oe_n_*`,
    /// so two of them on one bus net must stay silent.
    #[test]
    fn tristate_buffer_outputs_sharing_a_net_are_silent() {
        let b = board(
            &[(1, "A1"), (2, "A2"), (3, "BUS"), (4, "OE1"), (5, "OE2")],
            vec![
                comp("U1", "74HC125", &[("1", 4), ("2", 1), ("3", 3)]),
                comp("U2", "74HC125", &[("4", 5), ("5", 2), ("6", 3)]),
            ],
        );
        assert!(run(&b).findings.is_empty(), "{:?}", run(&b).findings);
    }

    /// The 74HC595's `qa..qh` are OE-gated, so two shift registers whose Q pins
    /// share a net are silent; but `qh_serial` (pad 9) is NOT OE-gated, so two of
    /// those on one net is a genuine push-pull fight and does fire. This pins the
    /// tri-state exclusion to the exact roles the db marks, not to the whole part.
    #[test]
    fn tristate_exclusion_is_per_role_not_per_part() {
        let shared_q = board(
            &[(1, "QSHARE")],
            vec![
                comp("U1", "74HC595", &[("15", 1)]), // qa
                comp("U2", "74HC595", &[("15", 1)]), // qa
            ],
        );
        assert!(
            run(&shared_q).findings.is_empty(),
            "OE-gated Q outputs are tri-stateable: {:?}",
            run(&shared_q).findings
        );

        let shared_serial = board(
            &[(1, "QHS")],
            vec![
                comp("U1", "74HC595", &[("9", 1)]), // qh_serial, never OE-gated
                comp("U2", "74HC595", &[("9", 1)]),
            ],
        );
        assert_eq!(
            run(&shared_serial).findings.len(),
            1,
            "the non-tri-stated cascade tap is push-pull"
        );
    }

    /// Two output pads of ONE part on a net is not an inter-part fight.
    #[test]
    fn same_part_twice_on_a_net_is_silent() {
        let b = board(
            &[(1, "A"), (2, "B"), (3, "PAR")],
            // 74HC08 y1 (pad 3) and y2 (pad 6) paralleled onto one net.
            vec![comp(
                "U1",
                "74HC08",
                &[("1", 1), ("2", 2), ("3", 3), ("6", 3)],
            )],
        );
        assert!(run(&b).findings.is_empty(), "{:?}", run(&b).findings);
    }

    /// A DNP part is not assembled, so it contributes no driver.
    #[test]
    fn dnp_part_contributes_no_driver() {
        let mut u2 = comp("U2", "74HC32", &[("3", 3)]);
        u2.dnp = true;
        let b = board(
            &[(3, "SHARED")],
            vec![comp("U1", "74HC08", &[("3", 3)]), u2],
        );
        assert!(run(&b).findings.is_empty(), "{:?}", run(&b).findings);
    }

    /// The other absent state: an identity-refused record is not trusted as a
    /// driver either, so it cannot manufacture a contention finding.
    #[test]
    fn identity_refused_part_contributes_no_driver() {
        let mut u2 = comp("U2", "74HC32", &[("3", 3)]);
        u2.properties.push((
            hauksbee_extract::DUPLICATE_REFERENCE_CONFLICT_KEY.to_string(),
            "two records with different values".to_string(),
        ));
        let b = board(
            &[(3, "SHARED")],
            vec![comp("U1", "74HC08", &[("3", 3)]), u2],
        );
        assert!(run(&b).findings.is_empty(), "{:?}", run(&b).findings);
    }

    /// Ground is not a net to contend over (a mis-mapped pad landing on GND is
    /// another check's problem), and neither is `KiCad`'s unconnected placeholder.
    #[test]
    fn ground_and_unconnected_nets_are_skipped() {
        let b = board(
            &[(1, "GND"), (2, "unconnected-(U3-Pad3)")],
            vec![
                comp("U1", "74HC08", &[("3", 1)]),
                comp("U2", "74HC32", &[("3", 1)]),
                comp("U3", "74HC08", &[("6", 2)]),
                comp("U4", "74HC32", &[("6", 2)]),
            ],
        );
        assert!(run(&b).findings.is_empty(), "{:?}", run(&b).findings);
    }

    /// A part with no model contributes nothing: the check speaks only about
    /// pins it actually bound.
    #[test]
    fn unmodelled_part_contributes_no_driver() {
        let b = board(
            &[(3, "SHARED")],
            vec![
                comp("U1", "74HC08", &[("3", 3)]),
                comp("U2", "SOME-UNKNOWN-PART-XYZ", &[("3", 3)]),
            ],
        );
        assert!(run(&b).findings.is_empty(), "{:?}", run(&b).findings);
    }

    /// The scan's evidence trail reports how far each part got, so the corpus
    /// calibration can assert engagement instead of counting on faith: here the
    /// 74HC125 presents 4 outputs, all tri-stateable, and contributes 0
    /// push-pull pins, while the 74HC08 presents 4, none tri-stateable, with 1
    /// on an eligible net, 1 on ground, and 2 pads absent from the footprint.
    #[test]
    fn scan_reports_the_evidence_trail() {
        let b = board(
            &[(1, "SIG"), (2, "GND"), (3, "BUS")],
            vec![
                comp("U1", "74HC08", &[("3", 1), ("6", 2)]),
                comp("U2", "74HC125", &[("3", 3), ("6", 3), ("8", 3), ("11", 3)]),
            ],
        );
        let s = scan(&b, &ModelLibrary::builtin());
        assert_eq!(s.parts.len(), 2);
        let u1 = s.parts.iter().find(|p| p.reference == "U1").unwrap();
        assert_eq!(
            (u1.output_roles, u1.tristateable_roles, u1.pushpull_pins),
            (4, 0, 1)
        );
        assert_eq!((u1.pads_absent, u1.pads_on_excluded_nets), (2, 1));
        let u2 = s.parts.iter().find(|p| p.reference == "U2").unwrap();
        assert_eq!(
            (u2.output_roles, u2.tristateable_roles, u2.pushpull_pins),
            (4, 4, 0)
        );
        assert_eq!(s.pushpull_driver_pins(), 1);
        assert_eq!(s.output_roles_presented(), 8);
    }

    /// A serial peripheral: `(pad, pin name, electrical type, net)` per pin.
    fn serial_part(reference: &str, pins: &[(&str, &str, &str, i64)]) -> Component {
        let pads: Vec<(&str, i64)> = pins.iter().map(|(p, _, _, n)| (*p, *n)).collect();
        let mut u = comp(reference, "GPS-MODULE", &pads);
        for (pin, (_, name, kind, _)) in u.pins.iter_mut().zip(pins) {
            pin.function = (*name).into();
            pin.kind = (*kind).into();
        }
        u
    }

    fn mcu(reference: &str, pads: &[(&str, i64)]) -> Component {
        comp(reference, "ATmega328P", pads)
    }

    fn transmit(b: &ExtractedBoard) -> Vec<LintFinding> {
        transmit_contention_lint(b, &ModelLibrary::builtin()).findings
    }

    const LINK: i64 = 1;

    /// Two UART transmitters on one line: the `ATmega328P`'s TXD pad (PD1, pad 3,
    /// `[models.pin_capability.uart_tx]`) and a GPS module's TXD typed output.
    /// The pad is a GPIO-capable port pad, but the card binds it to the UART
    /// transmit and the line is serial, so the fight is real.
    #[test]
    fn a_uart_transmit_pad_facing_a_transmit_pin_is_contention() {
        let b = board(
            &[(LINK, "GPS_TX")],
            vec![
                mcu("U1", &[("3", LINK)]),
                serial_part("U2", &[("1", "TXD", "output", LINK)]),
            ],
        );
        let found = transmit(&b);
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(
            (f.check, f.severity),
            (LintCheck::OutputContention, Severity::High)
        );
        assert_eq!(f.refs, ["U1", "U2"]);
        assert_eq!(f.nets, ["GPS_TX"]);
        assert!(f.message.contains("U1"), "{}", f.message);
        assert!(f.message.contains("U2"), "{}", f.message);
    }

    /// Two MCUs' transmit pads tied together is the same fight, with no
    /// peripheral on the line: the net name alone says it is serial.
    #[test]
    fn two_mcu_transmit_pads_on_one_serial_net_are_contention() {
        let b = board(
            &[(LINK, "UART_TX")],
            vec![mcu("U1", &[("3", LINK)]), mcu("U2", &[("3", LINK)])],
        );
        let found = transmit(&b);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].refs, ["U1", "U2"]);
    }

    /// One MCU drawn in two footprints (a DIP beside a TQFP), every pad on the
    /// same nets, is one device with one transmitter. Two MCUs that share only
    /// the transmit line are two devices fighting.
    #[test]
    fn one_mcu_in_two_footprints_is_not_two_transmitters() {
        let nets = [
            (LINK, "UART_TX"),
            (2, "GND"),
            (3, "VCC"),
            (4, "AUX"),
            (5, "RESET"),
            (6, "SCK"),
            (7, "MISO"),
        ];
        let wired = [
            ("3", LINK),
            ("8", 2),
            ("7", 3),
            ("1", 5),
            ("19", 6),
            ("18", 7),
        ];
        let twins = board(&nets, vec![mcu("U1", &wired), mcu("U1_TQFP", &wired)]);
        let found = transmit(&twins);
        assert!(found.is_empty(), "{found:#?}");
        let apart = board(
            &nets,
            vec![
                mcu("U1", &wired),
                mcu(
                    "U2",
                    &[
                        ("3", LINK),
                        ("8", 2),
                        ("7", 4),
                        ("1", 5),
                        ("19", 6),
                        ("18", 7),
                    ],
                ),
            ],
        );
        assert_eq!(transmit(&apart).len(), 1);
    }

    /// The correct wiring and every shape the check must leave alone.
    #[test]
    fn correct_serial_wiring_and_undecidable_pads_are_silent() {
        let gps_tx = |kind: &str| serial_part("U2", &[("1", "TXD", kind, LINK)]);
        let nets = [(LINK, "GPS_TX"), (2, "LED"), (3, "UART_TX")];
        let silent = |parts: Vec<Component>, why: &str| {
            let b = board(&nets, parts);
            let found = transmit(&b);
            assert!(found.is_empty(), "{why}: {found:?}");
        };
        // The GPS's TXD into the MCU's RXD pad (PD0, pad 2).
        silent(
            vec![mcu("U1", &[("2", LINK)]), gps_tx("output")],
            "tx into rx",
        );
        // The MCU's TXD pad into a device that only listens.
        silent(
            vec![
                mcu("U1", &[("3", LINK)]),
                serial_part("U2", &[("1", "RXD", "input", LINK)]),
            ],
            "tx into rx pin",
        );
        // One MCU transmit pad on a serial net, alone.
        silent(vec![mcu("U1", &[("3", 3)])], "alone");
        // PB1 (pad 15) is no UART pad: a GPIO's direction is firmware state.
        silent(
            vec![mcu("U1", &[("15", LINK)]), gps_tx("output")],
            "gpio pad",
        );
        // A transmit pad on a line that is not serial.
        let other = serial_part("U2", &[("1", "OUT", "output", 2)]);
        silent(vec![mcu("U1", &[("3", 2)]), other], "not a serial line");
        // A partner that may release the line (tri-state) is how a line is shared.
        silent(
            vec![mcu("U1", &[("3", LINK)]), gps_tx("tri_state")],
            "tri-state",
        );
        // A partner whose direction is untyped.
        silent(vec![mcu("U1", &[("3", LINK)]), gps_tx("")], "untyped");
        // A part not fitted drives nothing.
        let mut dnp = gps_tx("output");
        dnp.dnp = true;
        silent(vec![mcu("U1", &[("3", LINK)]), dnp], "dnp");
    }

    /// A plug-in dev board whose header pads are named by silkscreen label:
    /// `(pad number, header label, net)` per pad.
    fn dev_board(reference: &str, value: &str, pads: &[(&str, &str, i64)]) -> Component {
        let wired: Vec<(&str, i64)> = pads.iter().map(|(p, _, n)| (*p, *n)).collect();
        let mut u = comp(reference, value, &wired);
        for (pin, (_, label, _)) in u.pins.iter_mut().zip(pads) {
            pin.function = (*label).into();
        }
        u
    }

    /// A dev board's UART0 TX header pad is a transmitter like the chip card's
    /// own TXD pad: it faces a peripheral's TXD output as contention, whichever
    /// label the symbol puts on the pad, while the RX pad and a plain GPIO pad
    /// on the same board stay silent. The pad number is the carrier footprint's
    /// own (`J1-7`), so only the label says which pin it is.
    #[test]
    fn a_dev_board_uart_transmit_header_pad_is_a_transmitter() {
        let gps = || serial_part("U2", &[("1", "TXD", "output", LINK)]);
        for (value, tx_labels, rx_label, gpio_label) in [
            (
                "ESP32-DevKitC-32E",
                ["TX", "TXD0", "IO1", "GPIO1"].as_slice(),
                "RX",
                "IO4",
            ),
            (
                "Seeed Studio XIAO ESP32C3",
                ["TX", "D6", "GPIO21"].as_slice(),
                "RX",
                "D0",
            ),
            (
                "Seeed Studio XIAO ESP32S3",
                ["TX", "D6", "GPIO43"].as_slice(),
                "RX",
                "D0",
            ),
            (
                "Seeed Studio XIAO RP2040",
                ["TX", "D6", "GPIO0"].as_slice(),
                "RX",
                "D0",
            ),
            (
                "Adafruit Feather HUZZAH32",
                ["TX", "GPIO17"].as_slice(),
                "RX",
                "SCK",
            ),
            ("Adafruit Feather RP2040", ["TX"].as_slice(), "RX", "SCK"),
        ] {
            for label in tx_labels {
                let b = board(
                    &[(LINK, "GPS_TX")],
                    vec![dev_board("U1", value, &[("J1-7", label, LINK)]), gps()],
                );
                let found = transmit(&b);
                assert_eq!(found.len(), 1, "{value} pad '{label}': {found:#?}");
                assert_eq!(found[0].refs, ["U1", "U2"], "{value} pad '{label}'");
            }
            for (label, why) in [(rx_label, "the UART RX pad"), (gpio_label, "a GPIO pad")] {
                let b = board(
                    &[(LINK, "GPS_TX")],
                    vec![dev_board("U1", value, &[("J1-7", label, LINK)]), gps()],
                );
                let found = transmit(&b);
                assert!(found.is_empty(), "{value} {why} '{label}': {found:#?}");
            }
        }
    }

    fn resistor(reference: &str, value: &str, a: i64, b: i64) -> Component {
        Component {
            lib_id: "Device:R".into(),
            ..part(reference, value, &[("1", a), ("2", b)])
        }
    }

    /// Two push-pull outputs behind one small series resistor are still wired to
    /// each other: the resistor limits the fight's current, not the fight.
    #[test]
    fn two_outputs_behind_a_series_resistor_fight() {
        let nets = [
            (1, "A"),
            (2, "B"),
            (3, "Y_AND"),
            (4, "Y_OR"),
            (5, "+3V3"),
            (6, "GND"),
        ];
        let wired = |value: &str, far: i64| {
            let b = board(
                &nets,
                vec![
                    comp("U1", "74HC08", &[("1", 1), ("2", 2), ("3", 3)]),
                    comp("U2", "74HC32", &[("1", 1), ("2", 2), ("3", far)]),
                    resistor("R1", value, 3, 4),
                ],
            );
            run(&b).findings
        };
        let found = wired("100", 4);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].check, LintCheck::OutputContention);
        assert_eq!(found[0].nets, ["Y_AND", "Y_OR"]);
        assert_eq!(found[0].refs, ["U1", "U2"]);
        assert!(
            found[0].message.contains("R1 (100 Ω)"),
            "{}",
            found[0].message
        );
        assert!(found[0].message.contains("series resistor"));
        // Too large to be a series element, or a pull to a rail: not one wire.
        assert!(wired("47k", 4).is_empty());
        let pulled = board(
            &nets,
            vec![
                comp("U1", "74HC08", &[("1", 1), ("2", 2), ("3", 3)]),
                comp("U2", "74HC32", &[("1", 1), ("2", 2), ("3", 4)]),
                resistor("R1", "100", 3, 5),
                resistor("R2", "100", 4, 6),
            ],
        );
        assert!(run(&pulled).findings.is_empty());
    }

    /// An AVR's TXD pad facing a module's TXD pin through 100 ohms: the same
    /// transmit fight as on a bare wire, found across the resistor.
    #[test]
    fn a_transmit_pad_facing_a_transmit_pin_through_a_resistor_is_contention() {
        let nets = [(LINK, "MCU_TX"), (2, "GPS_TX"), (3, "+3V3"), (4, "GND")];
        let wired = |value: &str| {
            let b = board(
                &nets,
                vec![
                    mcu("U1", &[("3", LINK)]),
                    serial_part("U2", &[("1", "TXD", "output", 2)]),
                    resistor("R1", value, LINK, 2),
                ],
            );
            transmit(&b)
        };
        let found = wired("100");
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].refs, ["U1", "U2"]);
        assert_eq!(found[0].nets, ["MCU_TX", "GPS_TX"]);
        assert!(
            found[0].message.contains("R1 (100 Ω)"),
            "{}",
            found[0].message
        );
        // Controls: a divider-sized resistor, and the GPS's TXD into the RXD pad.
        assert!(wired("22k").is_empty());
        let rx = board(
            &nets,
            vec![
                mcu("U1", &[("2", LINK)]),
                serial_part("U2", &[("1", "TXD", "output", 2)]),
                resistor("R1", "100", LINK, 2),
            ],
        );
        assert!(transmit(&rx).is_empty());
        // A pull-up on the line to a rail is not a hop.
        let pull = board(
            &nets,
            vec![
                mcu("U1", &[("3", LINK)]),
                serial_part("U2", &[("1", "TXD", "output", 2)]),
                resistor("R1", "10k", LINK, 3),
            ],
        );
        assert!(transmit(&pull).is_empty());
    }
}
