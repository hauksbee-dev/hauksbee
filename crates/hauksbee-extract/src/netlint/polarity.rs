//! Polarity lint: miswires the netlist states outright, read from names and
//! potentials alone, with no source, solve or model.
//!
//! - **Supply and ground exchanged on a connector**: a connector pin whose own
//!   name says supply (`VBUS`, `VCC`, `+5V`, `VIN`, ...) sitting on a ground net,
//!   or a pin named `GND` sitting on a positive supply net.
//! - **Diode across a rail, forward**: a unidirectional diode (TVS, Zener,
//!   Schottky, rectifier) between ground and a rail with its anode at the higher
//!   potential. It conducts as soon as the rail is powered.
//! - **Polarised capacitor, reversed**: an electrolytic or tantalum capacitor
//!   whose + terminal sits on the lower-potential of two nets whose order is
//!   known.
//!
//! Every rule needs the *order* of two potentials, and takes it only from what
//! the board states: a net that is ground, a rail whose name states its voltage
//! (`+12V`, `-5V`) or a bare supply name (`VIN`, `VBAT`), which is positive.
//! Only ground against a rail, or a positive rail against a negative one, is
//! ordered. Two nets of unknown order (`Net-(D1-K)`, two differently named
//! positive rails) are never judged: a blocking or OR-ing diode between rails
//! and an AC-coupling capacitor on signals are legal both ways round.
//!
//! The same goes for polarity: an anode, a cathode, a + terminal. A pin
//! function names it (`A`/`K`, `+`/`-`); failing that, the library convention
//! (pad 1 is the cathode of a `Diode_SMD`/`Diode_THT` footprint and the + of a
//! `CP_*` capacitor footprint or `C_Polarized` symbol) when the symbol names no
//! pins. A part whose polarity cannot be told, or that may be bidirectional,
//! is left alone.

use std::cmp::Ordering;

use super::topology::is_connector;
use super::{
    is_capacitor, is_ground, is_led, is_positive_supply, is_resistor, is_supply,
    is_unconnected_net, norm, parse_ohms, LintCheck, LintFinding, NetLintReport, Severity,
};
use crate::assembly::AssemblyState;
use crate::part_class::{connected_pads, ref_designator};
use crate::{Component, ExtractedBoard};
use hauksbee_models::netclass::rail_voltage;
use hauksbee_models::refdes::{prefix, RefClass};

/// Where a net sits relative to ground, as far as its name says.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Level {
    Ground,
    /// A positive supply, with its voltage when the name states one.
    Positive(Option<f64>),
    /// A negative supply, with its voltage when the name states one.
    Negative(Option<f64>),
    Unknown,
}

fn level_of(name: &str) -> Level {
    if is_unconnected_net(name) {
        return Level::Unknown;
    }
    if is_ground(name) {
        return Level::Ground;
    }
    match rail_voltage(name) {
        Some(v) if v > 0.0 => return Level::Positive(Some(v)),
        Some(v) if v < 0.0 => return Level::Negative(Some(v)),
        _ => {}
    }
    if !is_supply(name) {
        return Level::Unknown;
    }
    let n = norm(name);
    // A differential or return-named supply (`VIN-`, `VOUT_NEG`, `V_RTN`) can
    // be either side of its pair, and `VEE` is negative by convention.
    let negative_word = n.ends_with('-')
        || n.split(|c: char| !c.is_ascii_alphanumeric())
            .any(|t| matches!(t, "NEG" | "MINUS" | "RTN" | "RET" | "RETURN" | "N"));
    if n.contains("VEE") {
        Level::Negative(None)
    } else if is_positive_supply(name) && !negative_word {
        Level::Positive(None)
    } else {
        Level::Unknown
    }
}

/// Whether `a` is above or below `b`, when the two names fix the order: ground
/// against a rail, or a positive rail against a negative one. Two rails of the
/// same sign are never ordered (`+5V` against `+3V3` is a name, not a measured
/// potential, and a diode or capacitor between them is legal either way round).
const fn order(a: Level, b: Level) -> Option<Ordering> {
    use Level::{Ground, Negative, Positive};
    match (a, b) {
        (Ground, Positive(_)) | (Negative(_), Ground | Positive(_)) => Some(Ordering::Less),
        (Positive(_), Ground | Negative(_)) | (Ground, Negative(_)) => Some(Ordering::Greater),
        _ => None,
    }
}

/// The stated voltage of a level, ground being 0 V.
const fn volts(l: Level) -> Option<f64> {
    match l {
        Level::Ground => Some(0.0),
        Level::Positive(v) | Level::Negative(v) => v,
        Level::Unknown => None,
    }
}

/// One pad of a part that carries a net.
struct Pad<'a> {
    number: &'a str,
    function: &'a str,
    net: i64,
}

/// The part's pads that carry a net, one per pad number.
fn pads(c: &Component) -> Vec<Pad<'_>> {
    let mut out: Vec<Pad<'_>> = Vec::new();
    for p in &c.pins {
        let Some(net) = p.net else { continue };
        if let Some(at) = out.iter().position(|q| q.number == p.number) {
            if out[at].function.trim().is_empty() {
                out[at].function = &p.function;
            }
        } else {
            out.push(Pad {
                number: &p.number,
                function: &p.function,
                net,
            });
        }
    }
    out
}

fn net_name(board: &ExtractedBoard, id: i64) -> &str {
    board.net(id).map_or("", |n| n.name.as_str())
}

fn fitted(c: &Component) -> bool {
    AssemblyState::of(c).is_present()
}

// ---------------------------------------------------------------------------
// Supply and ground exchanged on a connector.
// ---------------------------------------------------------------------------

/// A pin name with the pad suffix `KiCad` appends to repeated pins removed:
/// `VBUS_A4` reads `VBUS`, `Data_In_3V3` reads `DATA_IN`.
fn function_base(function: &str) -> String {
    let f = function.trim().to_ascii_uppercase();
    match f.rsplit_once('_') {
        Some((head, tail))
            if !head.is_empty()
                && tail.len() <= 6
                && tail.chars().any(|c| c.is_ascii_digit())
                && tail.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            head.to_string()
        }
        _ => f,
    }
}

/// A pin whose own name says it is a positive supply: a power word
/// (`VBUS`, `VCC`, `VDD`, `VIN`, `VBAT`) or a stated voltage (`+5V`, `3V3`).
fn is_supply_function(function: &str) -> bool {
    let f = function_base(function);
    if matches!(
        f.as_str(),
        "VBUS" | "VCC" | "VDD" | "VIN" | "VBAT" | "VBATT" | "VSYS" | "VPWR" | "V+" | "+V"
    ) {
        return true;
    }
    (f.starts_with('+') || f.starts_with(|c: char| c.is_ascii_digit()))
        && rail_voltage(&f).is_some_and(|v| v > 0.0)
}

/// A pin whose own name says it is ground.
fn is_ground_function(function: &str) -> bool {
    let f = function_base(function);
    !f.is_empty() && is_ground(&f)
}

/// A part that plugs or wires a cable to the board: a connector proper, or a
/// header, battery clip or USB receptacle by designator.
fn is_cable_part(c: &Component) -> bool {
    if is_connector(c) {
        return true;
    }
    let r = ref_designator(&c.reference);
    ["JP", "X", "USB", "BT", "BAT"].iter().any(|p| {
        r.strip_prefix(p)
            .is_some_and(|rest| rest.starts_with(|ch: char| ch.is_ascii_digit()))
    })
}

/// A connector pin: pad number, pin name, net name.
type PinOnNet<'a> = (&'a str, &'a str, &'a str);

pub(super) fn check_connector_supply_pins(board: &ExtractedBoard, report: &mut NetLintReport) {
    for c in board
        .components
        .iter()
        .filter(|c| fitted(c) && is_cable_part(c))
    {
        let mut supply_on_ground: Vec<PinOnNet<'_>> = Vec::new();
        let mut ground_on_supply: Vec<PinOnNet<'_>> = Vec::new();
        for pad in pads(c) {
            let name = net_name(board, pad.net);
            let level = level_of(name);
            if is_supply_function(pad.function) && level == Level::Ground {
                supply_on_ground.push((pad.number, pad.function, name));
            } else if is_ground_function(pad.function) && matches!(level, Level::Positive(_)) {
                ground_on_supply.push((pad.number, pad.function, name));
            }
        }
        if supply_on_ground.is_empty() && ground_on_supply.is_empty() {
            continue;
        }
        report
            .findings
            .push(connector_finding(c, &supply_on_ground, &ground_on_supply));
    }
}

fn pin_list(pins: &[PinOnNet<'_>]) -> String {
    pins.iter()
        .map(|(pad, function, net)| format!("'{function}' (pad {pad}) on '{net}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn connector_finding(
    c: &Component,
    supply_on_ground: &[PinOnNet<'_>],
    ground_on_supply: &[PinOnNet<'_>],
) -> LintFinding {
    let mut nets: Vec<String> = Vec::new();
    for (_, _, net) in supply_on_ground.iter().chain(ground_on_supply) {
        if !nets.iter().any(|n| n == net) {
            nets.push((*net).to_string());
        }
    }
    let mut parts = Vec::new();
    if !supply_on_ground.is_empty() {
        parts.push(format!(
            "supply pin {} is wired to ground",
            pin_list(supply_on_ground)
        ));
    }
    if !ground_on_supply.is_empty() {
        parts.push(format!(
            "ground pin {} is wired to a supply rail",
            pin_list(ground_on_supply)
        ));
    }
    let effect = if supply_on_ground.is_empty() || ground_on_supply.is_empty() {
        "a supply plugged in here is shorted to ground through that pin, or never reaches the board"
    } else {
        "supply and ground are exchanged, so a supply plugged in here is fed to the board backwards"
    };
    let remedy = match (supply_on_ground.first(), ground_on_supply.first()) {
        (Some((sp, sf, _)), Some((gp, gf, _))) => format!(
            "swap the nets of pad {sp} ('{sf}') and pad {gp} ('{gf}') so '{sf}' reaches the supply rail and '{gf}' the ground"
        ),
        (Some((sp, sf, _)), None) => {
            format!("move pad {sp} ('{sf}') from ground to the supply rail it is named for")
        }
        (None, Some((gp, gf, _))) => {
            format!("move pad {gp} ('{gf}') from the supply rail to the board ground")
        }
        (None, None) => String::new(),
    };
    LintFinding {
        check: LintCheck::SupplyGroundSwapped,
        severity: Severity::High,
        message: format!(
            "connector {}: {}; {effect}. Remedy: {remedy}",
            c.reference,
            parts.join("; ")
        ),
        refs: vec![c.reference.clone()],
        nets,
    }
}

// ---------------------------------------------------------------------------
// Polarity of a diode or a polarised capacitor.
// ---------------------------------------------------------------------------

/// Which terminal a pin or pad name denotes.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Terminal {
    /// Anode (diode) or + (capacitor).
    High,
    /// Cathode (diode) or - (capacitor).
    Low,
}

/// The terminal a pin function or pad number names, for a diode (`A`/`K`) or a
/// capacitor (`+`/`-`). Anything else (`1`, `~`, `A1`) names none.
fn named_terminal(name: &str, diode: bool) -> Option<Terminal> {
    let n = name.trim().to_ascii_uppercase();
    if diode {
        match n.as_str() {
            "A" | "ANODE" => Some(Terminal::High),
            "K" | "C" | "CATHODE" | "KATHODE" => Some(Terminal::Low),
            _ => None,
        }
    } else {
        match n.as_str() {
            "+" | "PLUS" | "POS" | "POSITIVE" => Some(Terminal::High),
            "-" | "MINUS" | "NEG" | "NEGATIVE" => Some(Terminal::Low),
            _ => None,
        }
    }
}

/// A pin name that says nothing about polarity either way.
fn is_blank_function(function: &str) -> bool {
    let f = function.trim();
    f.is_empty() || f == "~"
}

/// The library and name of a footprint, upper-cased (`Diode_SMD:D_SOD-123`
/// reads `DIODE_SMD` and `D_SOD-123`; a bare name reads an empty library).
fn footprint_split(footprint: &str) -> (String, String) {
    footprint.split_once(':').map_or_else(
        || (String::new(), footprint.to_ascii_uppercase()),
        |(lib, name)| (lib.to_ascii_uppercase(), name.to_ascii_uppercase()),
    )
}

/// The pad a footprint's own name marks as cathode or anode: `1(K)` and `2(A)`
/// in the name, as some vendor libraries spell it.
fn footprint_marked_terminal(footprint: &str) -> Option<(String, Terminal)> {
    let name = footprint.to_ascii_uppercase();
    for (marker, terminal) in [("(K)", Terminal::Low), ("(A)", Terminal::High)] {
        if let Some(at) = name.find(marker) {
            let digits: String = name[..at]
                .chars()
                .rev()
                .take_while(char::is_ascii_digit)
                .collect::<Vec<char>>()
                .into_iter()
                .rev()
                .collect();
            if !digits.is_empty() {
                return Some((digits, terminal));
            }
        }
    }
    None
}

/// The one pad whose terminal the part's library or footprint fixes, for a
/// part whose symbol names no pins: a footprint that spells it in its name, or
/// the `KiCad` convention that pad 1 is the cathode of a `Diode_SMD`/`Diode_THT`
/// footprint and the + terminal of a polarised-capacitor footprint or symbol.
fn library_convention(c: &Component, diode: bool) -> Option<(String, Terminal)> {
    if diode {
        if let Some(marked) = footprint_marked_terminal(&c.footprint) {
            return Some(marked);
        }
        let (lib, _) = footprint_split(&c.footprint);
        return matches!(lib.as_str(), "DIODE_SMD" | "DIODE_THT")
            .then(|| ("1".to_string(), Terminal::Low));
    }
    let (footprint, symbol) = capacitor_polarity_marks(c);
    (footprint == Some(true) || symbol == Some(true)).then(|| ("1".to_string(), Terminal::High))
}

/// Whether a capacitor's `KiCad` footprint and its symbol mark it polarised.
///
/// Each is `None` unless it comes from the stock library that says: a
/// `Capacitor_*` footprint is polarised when its name starts `CP_`, a `Device`
/// capacitor symbol when it is `CP`, `CP_*` or `C_Polarized*`.
#[must_use]
pub fn capacitor_polarity_marks(c: &Component) -> (Option<bool>, Option<bool>) {
    let (lib, name) = footprint_split(&c.footprint);
    let footprint = matches!(
        lib.as_str(),
        "CAPACITOR_TANTALUM_SMD" | "CAPACITOR_THT" | "CAPACITOR_SMD"
    )
    .then(|| name.starts_with("CP_"));
    let symbol = c.lib_id.split_once(':').and_then(|(lib, symbol)| {
        let symbol = symbol.to_ascii_uppercase();
        (lib.eq_ignore_ascii_case("Device") && symbol.starts_with('C')).then(|| {
            symbol.starts_with("C_POLARIZED") || symbol.starts_with("CP_") || symbol == "CP"
        })
    });
    (footprint, symbol)
}

/// The nets on a two-terminal part's high and low terminals, when its pins or
/// library say which is which: `(anode, cathode)` or `(plus, minus)`.
fn terminals(c: &Component, diode: bool) -> Option<(i64, i64)> {
    let pads = pads(c);
    let [first, second] = pads.as_slice() else {
        return None;
    };
    let role = |pad: &Pad<'_>| {
        named_terminal(pad.function, diode).or_else(|| {
            is_blank_function(pad.function)
                .then(|| named_terminal(pad.number, diode))
                .flatten()
        })
    };
    match (role(first), role(second)) {
        (Some(Terminal::High), Some(Terminal::Low)) => return Some((first.net, second.net)),
        (Some(Terminal::Low), Some(Terminal::High)) => return Some((second.net, first.net)),
        (None, None) => {}
        // One named, or both the same: a polarity this reader does not follow.
        _ => return None,
    }
    // Pin names that say something else (`A1`/`A2` of a bidirectional part) are
    // the symbol's word, and a library convention does not override them.
    let unnamed = |p: &Pad<'_>| is_blank_function(p.function) || p.function.trim() == p.number;
    if !unnamed(first) || !unnamed(second) {
        return None;
    }
    let (known, terminal) = library_convention(c, diode)?;
    let (marked, other) = if first.number == known {
        (first, second)
    } else if second.number == known {
        (second, first)
    } else {
        return None;
    };
    match terminal {
        Terminal::High => Some((marked.net, other.net)),
        Terminal::Low => Some((other.net, marked.net)),
    }
}

// ---------------------------------------------------------------------------
// Diode across a rail, forward.
// ---------------------------------------------------------------------------

/// A two-terminal part the designator names a diode, TVS or Zener (not an LED,
/// which has its own check, nor a photodiode or solar cell, which are sources).
fn is_plain_diode(c: &Component) -> bool {
    if is_led(c) || connected_pads(c) != 2 {
        return false;
    }
    let p = prefix(&c.reference);
    let named_diode = RefClass::of(&c.reference) == RefClass::Diode
        || matches!(p.as_str(), "TVS" | "ZD" | "VD" | "DZ");
    let text = format!("{} {} {}", c.value, c.lib_id, c.footprint).to_ascii_uppercase();
    let source_like = ["PHOTO", "SOLAR", "OPTO"].iter().any(|k| text.contains(k));
    named_diode && !source_like
}

/// Component properties that describe the part in words.
const DESCRIPTIVE_PROPERTIES: [&str; 4] =
    ["Description", "ki_description", "ki_keywords", "Keywords"];

/// Does free text call the part bidirectional: the whole word `bidirectional`,
/// `bi-directional`, `bi directional` or `bidir`/`bi-dir`? Word-bounded, so
/// `unidirectional` and `uni-directional` never match.
fn says_bidirectional(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let words: Vec<&str> = (lower.split(|ch: char| !ch.is_ascii_alphanumeric()))
        .filter(|w| !w.is_empty())
        .collect();
    words.iter().enumerate().any(|(i, w)| {
        matches!(*w, "bidirectional" | "bidir" | "bidirect")
            || (*w == "bi"
                && words
                    .get(i + 1)
                    .is_some_and(|n| matches!(*n, "directional" | "dir" | "direct")))
    })
}

/// Whether the part may conduct both ways: a bidirectional TVS (`SMBJ6.0CA`,
/// `P6KE15C`, "bidir" in its name).
fn maybe_bidirectional(c: &Component) -> bool {
    let text = format!("{} {} {}", c.value, c.lib_id, c.footprint).to_ascii_uppercase();
    if ["BIDIR", "BI-DIR", "BI_DIR", "BIPOLAR"]
        .iter()
        .any(|k| text.contains(k))
    {
        return true;
    }
    // The Description and keywords are often the only field that says so: a
    // custom library names its pins C and A (the unidirectional convention)
    // and its symbol after the part number.
    if (c.properties.iter())
        .filter(|(key, _)| {
            DESCRIPTIVE_PROPERTIES
                .iter()
                .any(|k| key.eq_ignore_ascii_case(k))
        })
        .any(|(_, text)| says_bidirectional(text))
    {
        return true;
    }
    let value = c.value.to_ascii_uppercase();
    let token: String = value
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .take_while(|ch| !matches!(ch, '(' | '/'))
        .collect();
    let digit_before = |suffix: &str| {
        token
            .strip_suffix(suffix)
            .and_then(|head| head.chars().last())
            .is_some_and(|ch| ch.is_ascii_digit())
    };
    digit_before("CA")
        || (digit_before("C")
            && (hauksbee_models::value::TVS_FAMILIES.iter()).any(|(f, _)| token.starts_with(*f)))
}

/// Resistance at or above which a resistor between two supply nets is a bias
/// feed (a current limit for the node it feeds), not a link inside one rail.
const BIAS_FEED_OHMS: f64 = 100.0;

/// Whether `net` is fed from another supply through a resistor: a node that a
/// resistor of a bias value ties to a positive or negative rail. A diode
/// conducting from ground into such a node is a diode-drop bias or clamp by
/// design (the resistor limits what it draws), not a rail shorted through a
/// wrong-way clamp, whatever the node is called.
fn is_resistor_fed(board: &ExtractedBoard, net: i64) -> bool {
    board.net_members(net).into_iter().any(|(r, _)| {
        fitted(r)
            && connected_pads(r) == 2
            && is_resistor(r)
            && parse_ohms(&r.value).is_some_and(|ohms| ohms >= BIAS_FEED_OHMS)
            && (r.pins.iter().filter_map(|p| p.net)).any(|far| {
                far != net
                    && matches!(
                        level_of(net_name(board, far)),
                        Level::Positive(_) | Level::Negative(_)
                    )
            })
    })
}

pub(super) fn check_diodes_across_rails(board: &ExtractedBoard, report: &mut NetLintReport) {
    for c in board
        .components
        .iter()
        .filter(|c| fitted(c) && is_plain_diode(c) && !maybe_bidirectional(c))
    {
        let Some((anode, cathode)) = terminals(c, true) else {
            continue;
        };
        if anode == cathode {
            continue;
        }
        let (a_name, k_name) = (net_name(board, anode), net_name(board, cathode));
        let (a_level, k_level) = (level_of(a_name), level_of(k_name));
        // One end must be ground: two rails joined by one diode is an OR-ing or
        // a drop, legal in either direction.
        let across_a_rail = (a_level == Level::Ground) != (k_level == Level::Ground);
        if !across_a_rail || order(a_level, k_level) != Some(Ordering::Greater) {
            continue;
        }
        let (rail, ground) = if a_level == Level::Ground {
            (k_name, a_name)
        } else {
            (a_name, k_name)
        };
        let rail_id = if a_level == Level::Ground {
            cathode
        } else {
            anode
        };
        if is_resistor_fed(board, rail_id) {
            continue;
        }
        report.findings.push(forward_diode_finding(
            c,
            (a_name, k_name),
            (rail, ground),
            a_level == Level::Ground,
        ));
    }
}

fn forward_diode_finding(
    c: &Component,
    (anode, cathode): (&str, &str),
    (rail, ground): (&str, &str),
    negative_rail: bool,
) -> LintFinding {
    let (fix_cathode, fix_anode) = if negative_rail {
        (ground, rail)
    } else {
        (rail, ground)
    };
    LintFinding {
        check: LintCheck::DiodeReversed,
        severity: Severity::High,
        message: format!(
            "{} ({}) is fitted backwards across the rail '{rail}': its anode is on '{anode}' and its \
             cathode on '{cathode}', so it is forward-biased as soon as '{rail}' is powered and \
             clamps the rail to about one diode drop, drawing whatever the source can supply. \
             A clamp, TVS or Zener across a rail must have its cathode on the more positive net, so \
             it conducts only past its breakdown voltage. Remedy: turn {} round so its cathode is \
             on '{fix_cathode}' and its anode on '{fix_anode}'",
            c.reference,
            c.value.trim(),
            c.reference,
        ),
        refs: vec![c.reference.clone()],
        nets: vec![rail.to_string(), ground.to_string()],
    }
}

// ---------------------------------------------------------------------------
// Polarised capacitor, reversed.
// ---------------------------------------------------------------------------

/// A capacitor the value or symbol says is not polarised, or polarised both
/// ways, whatever footprint it sits on.
fn declares_non_polarised(c: &Component) -> bool {
    let text = format!("{} {}", c.value, c.lib_id).to_ascii_uppercase();
    ["NON-POL", "NONPOL", "NON POL", "BIPOLAR", "BI-POLAR", "NP "]
        .iter()
        .any(|k| text.contains(k))
        || c.value.trim().to_ascii_uppercase().ends_with(" NP")
}

pub(super) fn check_polarised_capacitors(board: &ExtractedBoard, report: &mut NetLintReport) {
    for c in board.components.iter().filter(|c| {
        fitted(c) && connected_pads(c) == 2 && is_capacitor(c) && !declares_non_polarised(c)
    }) {
        let Some((plus, minus)) = terminals(c, false) else {
            continue;
        };
        if plus == minus {
            continue;
        }
        let (p_name, m_name) = (net_name(board, plus), net_name(board, minus));
        let (p_level, m_level) = (level_of(p_name), level_of(m_name));
        if order(p_level, m_level) != Some(Ordering::Less) {
            continue;
        }
        let apart = volts(p_level)
            .zip(volts(m_level))
            .map(|(p, m)| (p - m).abs());
        report
            .findings
            .push(reversed_capacitor_finding(c, (p_name, m_name), apart));
    }
}

fn reversed_capacitor_finding(
    c: &Component,
    (plus, minus): (&str, &str),
    apart: Option<f64>,
) -> LintFinding {
    let across = apart.map_or_else(String::new, |v| format!(" ({v} V apart)"));
    LintFinding {
        check: LintCheck::PolarisedCapReversed,
        severity: Severity::High,
        message: format!(
            "{} ({}) is polarised and fitted backwards: its + terminal is on '{plus}' and its - \
             terminal on '{minus}', the more positive net{across}, so it is reverse-biased whenever \
             the board is powered. A polarised capacitor takes only a small reverse voltage (a \
             solid tantalum about 10 % of its rating at 25 \u{b0}C, an aluminium electrolytic none): \
             wrong polarity short-circuits or damages it, and an electrolytic can open its pressure \
             vent. Remedy: turn {} round so its + terminal is on '{minus}' and its - terminal on \
             '{plus}'",
            c.reference,
            c.value.trim(),
            c.reference,
        ),
        refs: vec![c.reference.clone()],
        nets: vec![minus.to_string(), plus.to_string()],
    }
}

#[cfg(test)]
mod tests;
