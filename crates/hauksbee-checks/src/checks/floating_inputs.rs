//! Floating CMOS inputs: a logic input on no net, or on a net with no driver,
//! no pull resistor and no other connection.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! A CMOS input is a gate capacitor with no DC path of its own. Left open it
//! charges to whatever leakage and coupling put on it: the part reads random
//! bits, and an input resting near mid-rail turns both input transistors half
//! on and draws shoot-through current. Unlike bipolar TTL (which floats high),
//! nothing about the input defines its level, so every CMOS input must be
//! driven, pulled, or tied.
//!
//! ## Evidence the check rests on
//!
//! - The part is CMOS logic: its value names a CMOS logic family (74HC/HCT/
//!   AC/AHC/LVC/AUP/..., CD4000/HEF4000/MC14000, NC7S/NC7W tiny logic, TI
//!   `TXB010x` translators).
//!   Bipolar and bus-hold families (74LS, 74ABT, 74LVT) are not judged.
//! - The pin is an input: a `logic.inputs` role of the part's model card mapped
//!   through the card's pin table, or a schematic pin whose electrical type is
//!   `input`.
//! - Its net carries nothing but CMOS inputs: no driver, no pull resistor, no
//!   connector, no other part. Test points and mounting holes do not count as
//!   a connection, and nor does a do-not-populate part. A net named as a
//!   supply rail or ground abstains (the tie is the designer's).
//!
//! An explicit no-connect flag is not an excuse: a CMOS input marked NC still
//! floats. The exception is an input the card lists in `logic.pulled_inputs`,
//! which the datasheet gives an internal pull, so leaving it open is allowed.
//!
//! The extract-layer `FloatingControlPin` lint already reports a named
//! enable/reset/chip-select input alone on its net; such a pin is left to it
//! here so one fault does not read as two.
//!
//! ## What holds a net's level
//!
//! `Classifier` is the one answer to "what holds this net's level", shared
//! with the strap check. A net is `Hold::Defined` when a rail, a ground, a
//! resistor to either (one hop through a signal net is followed), a driver or
//! any part the classifier cannot show to be weak is on it. It is
//! `Hold::Floats` when nothing but capacitors, test points and other inputs
//! are on it. It is `Hold::MayFloat` when the only holders are parts that set
//! a level only some of the time: a push button or switch to one rail, a
//! connector pin (nothing while unplugged), an NPN collector or N-channel drain
//! (it only pulls low, and only when on) and, in strict mode, an MCU pad (high
//! impedance from reset until firmware configures it). A jumper header whose
//! other pins reach both a rail and ground is a selector and defines the level.
//!
//! The check judges three kinds of pad. A CMOS input open on a net nothing
//! holds is `High`, as before. A card-bound digital input or a control input
//! whose only holder is a button, connector pin or open-collector sink may
//! float and is `Medium`, with the pull resistor that fixes it. A fully open
//! op-amp or comparator input is `Medium`. A data input fed only by a
//! connector is left alone: the off-board driver is its pull.

use std::collections::{BTreeMap, BTreeSet};

use hauksbee_bind::binder::power_rail_voltage;
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    control_role, is_capacitor, is_connector_like, is_ground, is_logic_or_switch_ref,
    is_plain_resistor, is_test_point_or_hole, is_unconnected_net, pin_is_no_connect, Component,
    ExtractedBoard, LintCheck, LintFinding, NetLintReport, Pin, Severity,
};
use hauksbee_models::netclass::may_be_supply;
use hauksbee_models::refdes::{designator, RefClass};
use hauksbee_models::{ComponentKind, ModelEntry, ModelLibrary};

use super::model_controls::low_side_sink;

/// CMOS logic families, matched after any vendor prefix is stripped from the
/// value. Ordered longest first so `AHCT` is not read as `AHC`.
const CMOS_74_FAMILIES: [&str; 20] = [
    "AHCT", "VHCT", "ALVC", "LVC", "AHC", "VHC", "HCT", "ACT", "AUP", "AXP", "AVC", "AUC", "LVX",
    "LCX", "VCX", "FCT", "HC", "AC", "LV", "C",
];

/// Vendor / grade prefixes that precede the bare `74xx` part number.
const VENDOR_PREFIXES: [&str; 8] = ["SN", "MC", "NLV", "NL", "TC", "HD", "IDT", "M"];

/// Does this part value name a CMOS logic family?
#[must_use]
pub fn is_cmos_logic_value(value: &str) -> bool {
    let v = value.trim().to_ascii_uppercase();
    // 4000-series CMOS: CD4xxx, HEF4xxx, MC14xxx.
    for (prefix, digits) in [("CD4", 3), ("HEF4", 3), ("MC14", 3)] {
        if let Some(rest) = v.strip_prefix(prefix) {
            if rest.len() >= digits && rest.chars().take(digits).all(|c| c.is_ascii_digit()) {
                return true;
            }
        }
    }
    // Fairchild/onsemi and Toshiba tiny logic: NC7S.., NC7SZ.., NC7WZ.., TC7S...
    // TI TXB010x push-pull translators: OE is a plain CMOS input with no
    // internal pull (TXB0101 SCES639F 5.6, II +-1 uA; 7.3 asks for an OE
    // pull-down), so it must not float either.
    if ["NC7S", "NC7W", "NC7N", "NC7P", "TC7S", "TC7W", "TXB0"]
        .iter()
        .any(|p| v.starts_with(p))
    {
        return true;
    }
    let mut core = v.as_str();
    for p in VENDOR_PREFIXES {
        if let Some(rest) = core.strip_prefix(p) {
            if rest.starts_with("74") {
                core = rest;
                break;
            }
        }
    }
    let Some(rest) = core.strip_prefix("74") else {
        return false;
    };
    // `74` then the family letters then a digit (`74HC165`, `74LVC1G04`,
    // `74C14`); `74LS..`, `74ABT..` and `74LVT..` never reach a digit.
    CMOS_74_FAMILIES.iter().any(|fam| {
        rest.strip_prefix(fam)
            .is_some_and(|tail| tail.starts_with(|c: char| c.is_ascii_digit()))
    })
}

/// One input pad of a fitted part, with the name it is reported by and
/// whether the card lets it be left open.
struct InputPin<'a> {
    pin: &'a Pin,
    label: String,
    pull_allowed: bool,
    /// The card role of the pad, when the card names it.
    role: Option<String>,
    /// An op-amp or comparator input: judged only when wholly open.
    analog: bool,
}

/// Bipolar logic keeps its TTL pull-up input: 74LS, 74ALS, 74S, 74F, 74ABT,
/// 74LVT and the ULN Darlington arrays do not need an input held.
fn is_bipolar_value(value: &str) -> bool {
    let v = value.trim().to_ascii_uppercase();
    let core = ["SN", "DM", "MC", "HD", "M"]
        .iter()
        .find_map(|p| v.strip_prefix(p).filter(|r| r.starts_with("74")))
        .unwrap_or(&v);
    [
        "74LS", "74ALS", "74S", "74F", "74ABT", "74LVT", "74BCT", "UL",
    ]
    .iter()
    .any(|p| core.starts_with(p))
}

/// The input pads of a logic part: card `logic.inputs` roles mapped through
/// the card's pin table, plus (for a CMOS-valued part) schematic `input`-typed
/// pins. `card_only` drops the typed pins, for a part judged by its card alone.
fn input_pins<'a>(
    comp: &'a Component,
    model: Option<&ModelEntry>,
    card_only: bool,
) -> Vec<InputPin<'a>> {
    let card = model.filter(|m| {
        matches!(
            m.kind,
            ComponentKind::Digital | ComponentKind::ShiftRegister
        ) && !m.logic.inputs.is_empty()
    });
    let mut out = Vec::new();
    for pin in &comp.pins {
        let role = card.and_then(|m| {
            m.pins
                .get(&pin.number)
                .filter(|r| m.logic.inputs.iter().any(|i| i == *r))
        });
        let typed_input =
            !card_only && pin.kind.to_ascii_lowercase().split('+').next() == Some("input");
        if role.is_none() && !typed_input {
            continue;
        }
        let pull_allowed = match (card, role) {
            (Some(m), Some(r)) => m.logic.pulled_inputs.iter().any(|p| p == r),
            _ => false,
        };
        let label = if !pin.function.trim().is_empty() {
            display_name(&pin.function)
        } else if let Some(r) = role {
            r.to_ascii_uppercase()
        } else {
            format!("pin {}", pin.number)
        };
        out.push(InputPin {
            pin,
            label,
            pull_allowed,
            role: role.cloned(),
            analog: false,
        });
    }
    out
}

/// The op-amp and comparator inputs (`in_plus*`, `in_minus*` roles) of a
/// bound analog part.
fn analog_input_pins<'a>(comp: &'a Component, model: &ModelEntry) -> Vec<InputPin<'a>> {
    if !matches!(model.kind, ComponentKind::Opamp | ComponentKind::Comparator) {
        return Vec::new();
    }
    comp.pins
        .iter()
        .filter_map(|pin| {
            let role = model.pins.get(&pin.number)?;
            let lower = role.to_ascii_lowercase();
            (lower.starts_with("in_plus") || lower.starts_with("in_minus")).then(|| InputPin {
                pin,
                label: if pin.function.trim().is_empty() {
                    role.to_ascii_uppercase()
                } else {
                    display_name(&pin.function)
                },
                pull_allowed: false,
                role: Some(role.clone()),
                analog: true,
            })
        })
        .collect()
}

/// The roles a card says must never be left to float
/// (`params.must_not_float_roles`, comma separated).
fn must_not_float_roles(model: &ModelEntry) -> Vec<String> {
    model
        .params
        .get_str("must_not_float_roles")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A schematic pin name without `KiCad`'s overbar markup: `~{CE}` reads `/CE`.
fn display_name(function: &str) -> String {
    let f = function.trim();
    f.strip_prefix("~{")
        .and_then(|r| r.strip_suffix('}'))
        .map_or_else(|| f.replace(['{', '}'], ""), |inner| format!("/{inner}"))
}

/// Would the extract-layer `FloatingControlPin` lint already report this pin?
/// Mirrors its predicate so the two checks never report one pin twice.
pub(super) fn control_lint_covers(board: &ExtractedBoard, comp: &Component, pin: &Pin) -> bool {
    let active = is_logic_or_switch_ref(comp);
    let Some(net) = pin.net.and_then(|id| board.net(id)) else {
        return false;
    };
    active
        && control_role(&pin.function).is_some()
        && !pin_is_no_connect(pin)
        && !is_unconnected_net(&net.name)
        && board.net_members(net.id).len() == 1
}

/// What kind of part is the only thing holding a net.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WeakKind {
    /// A push button or switch with a terminal off the net.
    Switch,
    /// A connector or header pin: nothing while unplugged.
    Connector,
    /// An MCU pad: high impedance from reset until firmware drives it.
    Gpio,
    /// An MCU pad that is an output once firmware runs: the symbol types it
    /// `output`/`bidirectional`, its card lists the pad as a peripheral output
    /// (SPI clock or data-out, UART transmit, PWM), or it has no electrical type
    /// to say otherwise. The net has a driver; it is high impedance only while
    /// that MCU is in reset.
    McuOutput,
    /// An NPN collector or N-channel drain: pulls low only while on.
    Sink,
    /// A two-pad jumper (`JP`, `SJ`) the user can leave open: nothing while
    /// it is open.
    Jumper,
}

/// One part that holds a net's level only some of the time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Weak {
    pub kind: WeakKind,
    /// How a finding names it: `SW1 pin 2`, `Q1's collector`.
    pub label: String,
}

/// What holds a net's level, seen from one pad.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Hold {
    /// A rail, ground, a resistor to either, a driver, or a part not shown weak.
    Defined,
    /// Nothing sets a level: no part, or only capacitors and other inputs.
    Floats,
    /// Only parts that hold a level some of the time are on the net.
    MayFloat(Vec<Weak>),
}

/// The shared "what holds this net's level" judgement of the floating-input
/// and strap checks. Conservative: any part it cannot show to be weak counts as
/// holding the level, so a finding needs positive evidence of weakness.
pub(super) struct Classifier<'a> {
    pub board: &'a ExtractedBoard,
    pub models: &'a BTreeMap<&'a str, ModelEntry>,
    /// Pads that only sense their net (judged inputs): never a holder.
    pub senses: &'a BTreeSet<(&'a str, &'a str)>,
    /// Count an MCU pad as a weak holder (high impedance at reset).
    pub strict: bool,
    /// Count a two-pad jumper the user can leave open as a weak holder (the
    /// strap lint reads the open position). Off, a jumper is a series link.
    pub open_jumpers: bool,
}

/// A net that ties a level: ground, or anything that may be a supply.
fn is_level_net(name: &str) -> bool {
    is_ground(name) || power_rail_voltage(name).is_some() || may_be_supply(name)
}

impl Classifier<'_> {
    fn net_is_level(&self, id: i64) -> bool {
        self.board.net(id).is_some_and(|n| is_level_net(&n.name))
    }

    /// Does a part (other than pad `at`) reach both a rail and ground through
    /// its own other pads? A jumper header or selector switch does.
    fn selects_between_rails(&self, part: &Component, at: &Pin) -> bool {
        let (mut rail, mut ground) = (false, false);
        for p in part.pins.iter().filter(|p| p.number != at.number) {
            if let Some(n) = p.net.and_then(|id| self.board.net(id)) {
                ground |= is_ground(&n.name);
                rail |= !is_ground(&n.name) && is_level_net(&n.name);
            }
        }
        rail && ground
    }

    /// What holds the net `pin` of `comp` sits on.
    pub(super) fn hold(&self, comp: &Component, pin: &Pin) -> Hold {
        let Some(net) = pin
            .net
            .filter(|&id| id != 0)
            .and_then(|id| self.board.net(id))
        else {
            return Hold::Floats;
        };
        if is_ground(&net.name) || power_rail_voltage(&net.name).is_some() {
            return Hold::Defined;
        }
        let (defined, mut weak) = self.holders(net.id, (&comp.reference, &pin.number), 1);
        weak.dedup();
        if defined {
            Hold::Defined
        } else if weak.is_empty() {
            Hold::Floats
        } else {
            Hold::MayFloat(weak)
        }
    }

    /// Whether some other member of the net holds its level, and the weak
    /// holders found. `skip` is the pad being judged; `hops` is how many series
    /// resistors to follow.
    fn holders(&self, net_id: i64, skip: (&str, &str), hops: u8) -> (bool, Vec<Weak>) {
        let mut weak = Vec::new();
        for (o, q) in self.board.net_members(net_id) {
            if (o.reference == skip.0 && q.number == skip.1)
                || !AssemblyState::of(o).is_present()
                || is_test_point_or_hole(o)
                || self
                    .senses
                    .contains(&(o.reference.as_str(), q.number.as_str()))
                || is_capacitor(o)
            {
                continue;
            }
            let at = format!("{} pin {}", o.reference, q.number);
            let class = RefClass::of(&o.reference);
            if self.open_jumpers && is_openable_jumper(o) {
                // The user can leave it open, which leaves nothing on the far side.
                weak.push(Weak {
                    kind: WeakKind::Jumper,
                    label: at,
                });
            } else if is_jumper(o) || is_plain_resistor(o) {
                // A series element: the far side holds the level, or is a signal net.
                for far in o
                    .pins
                    .iter()
                    .filter(|p| p.number != q.number)
                    .filter_map(|p| {
                        p.net
                            .filter(|&id| id != 0 && id != net_id)
                            .map(|id| (id, p))
                    })
                {
                    if self.net_is_level(far.0) || hops == 0 {
                        return (true, Vec::new());
                    }
                    let (defined, more) =
                        self.holders(far.0, (&o.reference, &far.1.number), hops - 1);
                    if defined {
                        return (true, Vec::new());
                    }
                    weak.extend(more);
                }
            } else if class == RefClass::Switch || designator(&o.reference).starts_with("SW") {
                if self.selects_between_rails(o, q) {
                    return (true, Vec::new());
                }
                weak.push(Weak {
                    kind: WeakKind::Switch,
                    label: at,
                });
            } else if is_connector_like(o) {
                if o.pins.len() <= 3 && self.selects_between_rails(o, q) {
                    return (true, Vec::new());
                }
                weak.push(Weak {
                    kind: WeakKind::Connector,
                    label: at,
                });
            } else if class == RefClass::Transistor {
                let model = self.models.get(o.reference.as_str());
                if let Some(label) = low_side_sink(o, q, model) {
                    weak.push(Weak {
                        kind: WeakKind::Sink,
                        label,
                    });
                } else if !is_control_terminal(q, model) {
                    return (true, Vec::new());
                }
            } else {
                let kind = q.kind.to_ascii_lowercase();
                let kind = kind.split('+').next().unwrap_or_default();
                let model = self.models.get(o.reference.as_str());
                let card_input = model.is_some_and(|m| {
                    m.pins
                        .get(&q.number)
                        .is_some_and(|r| m.logic.inputs.iter().any(|i| i == r))
                });
                // A card-named logic input senses the net, it does not set it. A
                // schematic `input` pin type is not trusted: MCU symbols type
                // output-capable pads (PWM, UART TX) as inputs.
                if card_input {
                    continue;
                }
                if self.strict
                    && !kind.starts_with("power")
                    && model.is_some_and(|m| m.kind == ComponentKind::Mcu)
                {
                    let name = q.function.trim();
                    let label = if name.is_empty() {
                        at
                    } else {
                        format!("{at} ({name})")
                    };
                    let kind = if model.is_some_and(|m| mcu_pad_drives(q, m)) {
                        WeakKind::McuOutput
                    } else {
                        WeakKind::Gpio
                    };
                    weak.push(Weak { kind, label });
                    continue;
                }
                return (true, Vec::new());
            }
        }
        (false, weak)
    }
}

/// Whether an MCU pad drives its net once firmware runs: the schematic types it
/// as an output (`output`, `bidirectional`, `tri_state`, `open_collector`,
/// `open_emitter`), or the card lists the pad as a peripheral output (SPI clock
/// or data-out, UART transmit, PWM, a tri-state or open-drain output). A pad the
/// symbol types `input` and the card does not list is shown only to listen. A
/// pad with no electrical type at all (a layout-only extraction keeps pad
/// numbers and nets, not symbol types) is not shown to listen either, so it is
/// read as an MCU pad firmware may drive.
pub(super) fn mcu_pad_drives(pin: &Pin, model: &ModelEntry) -> bool {
    let kind = pin.kind.to_ascii_lowercase();
    let kind = kind.split('+').next().unwrap_or_default().trim();
    if kind.is_empty()
        || matches!(
            kind,
            "output" | "bidirectional" | "tri_state" | "open_collector" | "open_emitter"
        )
    {
        return true;
    }
    let caps = &model.pin_capability;
    let listed = [&caps.spi_mosi, &caps.spi_sck, &caps.uart_tx, &caps.pwm]
        .into_iter()
        .flatten()
        .any(|t| t.pads.contains_key(&pin.number));
    listed
        || caps
            .tri_state
            .as_ref()
            .is_some_and(|t| t.pads.contains_key(&pin.number))
        || caps
            .open_drain
            .as_ref()
            .is_some_and(|t| t.pads.contains_key(&pin.number))
}

/// A transistor's base or gate pad: it senses the net like an input.
fn is_control_terminal(p: &Pin, model: Option<&ModelEntry>) -> bool {
    let role = model
        .and_then(|m| {
            m.pins
                .get(&p.number)
                .or_else(|| m.pins.get(p.function.trim()))
        })
        .map_or_else(|| p.function.trim(), String::as_str)
        .to_ascii_lowercase();
    matches!(
        role.split('_').next().unwrap_or_default(),
        "b" | "base" | "g" | "gate"
    )
}

/// The finding for a part's inputs left with no driver, pull or connection.
fn open_inputs_finding(
    board: &ExtractedBoard,
    comp: &Component,
    open: &[&InputPin<'_>],
) -> LintFinding {
    let flagged_nc = open.iter().filter(|p| pin_is_no_connect(p.pin)).count();
    let names = compress_labels(open.iter().map(|p| p.label.as_str()));
    let nc_note = if flagged_nc > 0 {
        " They carry a no-connect flag, but the part has no internal pull on them, so the flag leaves them floating."
    } else {
        ""
    };
    let nets: Vec<String> = open
        .iter()
        .filter_map(|p| p.pin.net.and_then(|id| board.net(id)))
        .map(|n| n.name.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    LintFinding {
        check: LintCheck::FloatingInput,
        severity: Severity::High,
        message: format!(
            "{} {}: {} {} {} unconnected (no driver, pull resistor or other connection); CMOS inputs must not float. A floating input reads random levels and can draw shoot-through current; tie each one high or low, or drive it.{nc_note}",
            comp.reference,
            comp.value,
            if open.len() == 1 { "input" } else { "inputs" },
            names,
            if open.len() == 1 { "is" } else { "are" },
        ),
        refs: vec![comp.reference.clone()],
        nets,
    }
}

/// Is this input's name an active-low one (`~{OE}`, `/CE`, `OE_N`, `nRST`)?
fn active_low(pin: &InputPin<'_>) -> bool {
    let f = pin.pin.function.trim();
    let r = pin.role.as_deref().unwrap_or_default();
    f.starts_with(['~', '/', '!'])
        || f.ends_with('#')
        || f.to_ascii_uppercase().ends_with("_N")
        || (f.starts_with('n') && f.chars().nth(1).is_some_and(|c| c.is_ascii_uppercase()))
        || r.ends_with("_n")
        || r.contains("_n_")
}

/// The name of the supply net a part runs from: the net of its `vcc`/`vdd`
/// pad by the card, else of a supply-typed pin on a rail-named net.
pub(super) fn supply_net_name(
    board: &ExtractedBoard,
    comp: &Component,
    model: Option<&ModelEntry>,
) -> Option<String> {
    let rail = |p: &Pin| {
        p.net
            .and_then(|id| board.net(id))
            .filter(|n| !is_ground(&n.name) && may_be_supply(&n.name))
            .map(|n| n.name.clone())
    };
    let by_card = model.and_then(|m| {
        comp.pins.iter().find_map(|p| {
            let role = m.pins.get(&p.number)?.to_ascii_lowercase();
            (role.starts_with("vcc") || role.starts_with("vdd")).then(|| rail(p))?
        })
    });
    by_card.or_else(|| {
        comp.pins
            .iter()
            .filter(|p| p.kind.to_ascii_lowercase().starts_with("power_in"))
            .find_map(rail)
    })
}

/// When the weak holders of a net let go, in words.
fn when_open(weak: &[Weak]) -> String {
    let kinds: BTreeSet<&str> = weak
        .iter()
        .map(|w| match w.kind {
            WeakKind::Switch => "no button or switch is closed",
            WeakKind::Connector => "nothing is plugged in",
            WeakKind::Gpio | WeakKind::McuOutput => {
                "the MCU is in reset and the pad is high-impedance"
            }
            WeakKind::Sink => "the transistor is off",
            WeakKind::Jumper => "the jumper is open",
        })
        .collect();
    kinds.into_iter().collect::<Vec<_>>().join(" or while ")
}

/// A part's inputs held only by parts that set a level some of the time.
fn may_float_finding(
    board: &ExtractedBoard,
    comp: &Component,
    model: Option<&ModelEntry>,
    open: &[(&InputPin<'_>, Vec<Weak>)],
) -> LintFinding {
    let names = compress_labels(open.iter().map(|(p, _)| p.label.as_str()));
    let mut held: Vec<String> = open
        .iter()
        .flat_map(|(_, w)| w.iter().map(|w| w.label.clone()))
        .collect();
    held.sort();
    held.dedup();
    let weak: Vec<Weak> = open.iter().flat_map(|(_, w)| w.clone()).collect();
    let nets: Vec<String> = open
        .iter()
        .filter_map(|(p, _)| p.pin.net.and_then(|id| board.net(id)))
        .map(|n| n.name.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let supply = supply_net_name(board, comp, model);
    let up = supply.map_or_else(|| "the part's supply".to_string(), |n| format!("'{n}'"));
    let mut fixes: Vec<String> = open
        .iter()
        .map(|(p, _)| {
            if active_low(p) {
                format!("a 10 kOhm pull-up to {up}")
            } else {
                "a 10 kOhm pull-down to GND (or a pull-up to the supply if it must idle active)"
                    .to_string()
            }
        })
        .collect();
    fixes.sort();
    fixes.dedup();
    let one = open.len() == 1;
    LintFinding {
        check: LintCheck::FloatingInput,
        severity: Severity::Medium,
        message: format!(
            "{} {}: {} {} on {} may float: only {} {} on the net, and nothing sets a level while {}. A floating input reads random levels; add {} on {} so it has a defined level.",
            comp.reference,
            comp.value,
            if one { "input" } else { "inputs" },
            names,
            hauksbee_ir::vocabulary::names_summary(&nets),
            held.join(", "),
            if held.len() == 1 { "is" } else { "are" },
            when_open(&weak),
            fixes.join(" or "),
            if one { "that input" } else { "each input" },
        ),
        refs: vec![comp.reference.clone()],
        nets,
    }
}

/// A part's op-amp or comparator inputs open on a net nothing touches.
fn open_analog_finding(
    board: &ExtractedBoard,
    comp: &Component,
    open: &[&InputPin<'_>],
) -> LintFinding {
    let names = compress_labels(open.iter().map(|p| p.label.as_str()));
    let nets: Vec<String> = open
        .iter()
        .filter_map(|p| p.pin.net.and_then(|id| board.net(id)))
        .map(|n| n.name.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    LintFinding {
        check: LintCheck::FloatingInput,
        severity: Severity::Medium,
        message: format!(
            "{} {}: {} {} {} open (nothing else on the net). An open op-amp or comparator input drifts to a supply or oscillates; tie it to a defined level (a bias, or a follower on an unused section) or drive it.",
            comp.reference,
            comp.value,
            if open.len() == 1 { "input" } else { "inputs" },
            names,
            if open.len() == 1 { "is" } else { "are" },
        ),
        refs: vec![comp.reference.clone()],
        nets,
    }
}

/// Every floating input, one finding per part: CMOS and card-bound logic inputs
/// on a net nothing holds (`High`), logic inputs held only by a weak part and
/// open op-amp or comparator inputs (`Medium`).
#[must_use]
#[allow(clippy::too_many_lines)] // one pass over the judged parts reads best in one place
pub fn floating_input_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let models: BTreeMap<&str, ModelEntry> = super::resolved_components(board, lib)
        .map(|(c, m)| (c.reference.as_str(), m))
        .collect();
    let judged: Vec<(&Component, Vec<InputPin<'_>>)> = board
        .components
        .iter()
        .filter(|c| AssemblyState::of(c).is_present())
        .map(|c| {
            let model = models.get(c.reference.as_str());
            let cmos = is_cmos_logic_value(&c.value);
            let by_card = model.is_some_and(|m| {
                matches!(
                    m.kind,
                    ComponentKind::Digital | ComponentKind::ShiftRegister
                ) && !m.logic.inputs.is_empty()
            }) && !is_bipolar_value(&c.value);
            let mut pins = if cmos || by_card {
                input_pins(c, model, !cmos)
            } else {
                Vec::new()
            };
            if let Some(m) = model {
                pins.extend(analog_input_pins(c, m));
            }
            (c, pins)
        })
        .filter(|(_, pins)| !pins.is_empty())
        .collect();
    // Every judged input pad on the board: another input sharing the net is not
    // a driver, so a net of nothing but inputs still floats.
    let senses: BTreeSet<(&str, &str)> = judged
        .iter()
        .flat_map(|(c, pins)| {
            pins.iter()
                .map(|p| (c.reference.as_str(), p.pin.number.as_str()))
        })
        .collect();
    let classify = Classifier {
        board,
        models: &models,
        senses: &senses,
        strict: false,
        open_jumpers: false,
    };

    // The do-not-populate active parts (an IC, a transistor, a module or a
    // connector) on an input's net. A keyboard marks its hand-soldered
    // controller module DNP so the assembler skips it; the module still drives
    // the input on the finished board. A DNP resistor is not one: it is the
    // pull the assembled board leaves out, which is the fault itself. A module
    // is recognised by what it is, not only by an IC-style designator: a
    // plug-in board is often `A101` (IEEE 315 "assembly").
    let dnp_drivers = |p: &Pin| -> BTreeSet<&str> {
        p.net
            .filter(|&id| id != 0)
            .map(|id| board.net_members(id))
            .unwrap_or_default()
            .into_iter()
            .filter(|(c, _)| {
                matches!(AssemblyState::of(c), AssemblyState::DnpAbsent(_))
                    && !is_test_point_or_hole(c)
                    && !is_jumper(c)
                    && (is_logic_or_switch_ref(c) || is_connector_like(c) || is_module_like(c))
            })
            .map(|(c, _)| c.reference.as_str())
            .collect()
    };

    let mut report = NetLintReport::default();
    // Inputs driven only by do-not-populate parts, grouped by those parts: one
    // absent module behind a hundred register inputs is one condition.
    let mut by_dnp: BTreeMap<Vec<&str>, Vec<(&Component, Vec<&InputPin<'_>>)>> = BTreeMap::new();
    for (comp, pins) in &judged {
        let model = models.get(comp.reference.as_str());
        let required = model.map(must_not_float_roles).unwrap_or_default();
        let mut floating: Vec<&InputPin<'_>> = Vec::new();
        let mut analog_open: Vec<&InputPin<'_>> = Vec::new();
        let mut may: Vec<(&InputPin<'_>, Vec<Weak>)> = Vec::new();
        for p in pins {
            if (p.pull_allowed && pin_is_no_connect(p.pin))
                || control_lint_covers(board, comp, p.pin)
            {
                continue;
            }
            match classify.hold(comp, p.pin) {
                // An op-amp input is judged only when it is wholly open: on no
                // net, a no-connect placeholder, or a net nothing else touches.
                // A named net shared by pins (a sheet's signal label) may be
                // driven from outside the file the board was read from.
                Hold::Floats if p.analog => {
                    let alone = p.pin.net.filter(|&id| id != 0).is_none_or(|id| {
                        board.net_members(id).len() == 1
                            || board.net(id).is_some_and(|n| is_unconnected_net(&n.name))
                    });
                    if alone {
                        analog_open.push(p);
                    }
                }
                Hold::Floats => floating.push(p),
                Hold::MayFloat(weak) if !p.analog => {
                    let control = control_role(&p.pin.function).is_some()
                        || p.role.as_deref().is_some_and(|r| control_role(r).is_some());
                    let covered = p.role.as_ref().is_some_and(|r| required.contains(r));
                    let connector_only = weak.iter().all(|w| w.kind == WeakKind::Connector);
                    if !covered && (control || !connector_only) {
                        may.push((p, weak));
                    }
                }
                _ => {}
            }
        }
        let (assumed, open): (Vec<&InputPin<'_>>, Vec<&InputPin<'_>>) = floating
            .into_iter()
            .partition(|p| !dnp_drivers(p.pin).is_empty());
        if !assumed.is_empty() {
            let drivers: BTreeSet<&str> = assumed.iter().flat_map(|p| dnp_drivers(p.pin)).collect();
            by_dnp
                .entry(drivers.into_iter().collect())
                .or_default()
                .push((*comp, assumed));
        }
        if !open.is_empty() {
            report
                .findings
                .push(open_inputs_finding(board, comp, &open));
        }
        if !may.is_empty() {
            report
                .findings
                .push(may_float_finding(board, comp, model, &may));
        }
        if !analog_open.is_empty() {
            report
                .findings
                .push(open_analog_finding(board, comp, &analog_open));
        }
    }
    for (drivers, group) in by_dnp {
        report.findings.push(match group.as_slice() {
            [(comp, assumed)] => driven_only_by_dnp(board, comp, assumed, &dnp_drivers),
            _ => driven_only_by_dnp_grouped(board, &drivers.join(", "), &group),
        });
    }
    report
}

/// A solder or pin jumper (`JP`, `SJ`): the link the assembled board leaves
/// open when it is do-not-populate, so it drives nothing. Its `J` designator
/// would otherwise read as a connector.
fn is_jumper(c: &Component) -> bool {
    matches!(
        hauksbee_models::refdes::prefix(&c.reference).as_str(),
        "JP" | "SJ"
    )
}

/// A two-pad jumper the user can leave open: a pin-header jumper or a solder
/// jumper whose footprint is not pre-bridged. A pre-bridged solder jumper is
/// closed as it ships, and a three-pad one always joins two of its pads.
fn is_openable_jumper(c: &Component) -> bool {
    is_jumper(c)
        && c.distinct_pad_count() == 2
        && !c.footprint.to_ascii_lowercase().contains("bridged")
}

/// A part that may drive a net though its designator does not say IC: three
/// or more pins, and a designator that does not call it a passive or a diode.
/// A plug-in module (`A101`), a sub-assembly or an oddly named IC.
fn is_module_like(c: &Component) -> bool {
    use hauksbee_models::refdes::RefClass;
    let passive = matches!(
        RefClass::of(&c.reference),
        RefClass::Resistor
            | RefClass::ResistorArray
            | RefClass::Varistor
            | RefClass::Thermistor
            | RefClass::Capacitor
            | RefClass::Inductor
            | RefClass::FerriteBead
            | RefClass::Diode
            | RefClass::Led
    );
    !passive && c.pins.len() >= 3
}

/// [`driven_only_by_dnp`] for many parts behind the same do-not-populate
/// drivers: one stated condition naming them all.
fn driven_only_by_dnp_grouped(
    board: &ExtractedBoard,
    drivers: &str,
    group: &[(&Component, Vec<&InputPin<'_>>)],
) -> LintFinding {
    const SHOWN: usize = 3;
    let inputs: usize = group.iter().map(|(_, pins)| pins.len()).sum();
    let listed: Vec<String> = (group.iter().take(SHOWN))
        .map(|(c, pins)| {
            let labels = compress_labels(pins.iter().map(|p| p.label.as_str()));
            format!("{} {labels}", c.reference)
        })
        .collect();
    let more = group.len().saturating_sub(SHOWN);
    let more = if more > 0 {
        format!("; and {more} more part(s)")
    } else {
        String::new()
    };
    let nets: Vec<String> = (group.iter())
        .flat_map(|(_, pins)| pins.iter())
        .filter_map(|p| p.pin.net.and_then(|id| board.net(id)))
        .map(|n| n.name.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    LintFinding {
        check: LintCheck::FloatingInput,
        severity: Severity::Low,
        message: format!(
            "{inputs} CMOS inputs on {} parts ({}{more}) are driven only by {drivers}, marked \
             do-not-populate. Assumed: {drivers} (or the module it represents) is fitted when \
             the board is used; if it is left off, these inputs float, so add pull resistors if \
             that state matters.",
            group.len(),
            listed.join("; "),
        ),
        refs: group.iter().map(|(c, _)| c.reference.clone()).collect(),
        nets,
    }
}

/// The stated condition for inputs whose only driver is a do-not-populate
/// active part: the board works when that part is fitted by hand, and floats
/// when it is left off.
fn driven_only_by_dnp<'b>(
    board: &'b ExtractedBoard,
    comp: &Component,
    pins: &[&InputPin<'_>],
    dnp_drivers: &impl Fn(&Pin) -> BTreeSet<&'b str>,
) -> LintFinding {
    let drivers: BTreeSet<&str> = pins.iter().flat_map(|p| dnp_drivers(p.pin)).collect();
    let drivers = drivers.into_iter().collect::<Vec<_>>().join(", ");
    let nets: Vec<String> = pins
        .iter()
        .filter_map(|p| p.pin.net.and_then(|id| board.net(id)))
        .map(|n| n.name.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let one = pins.len() == 1;
    LintFinding {
        check: LintCheck::FloatingInput,
        severity: Severity::Low,
        message: format!(
            "{} {}: {} {} on {} {} driven only by {drivers}, marked do-not-populate. Assumed: {drivers} (or the module it represents) is fitted when the board is used; if it is left off, the {} {}, so add a pull resistor if that state matters.",
            comp.reference,
            comp.value,
            if one { "input" } else { "inputs" },
            compress_labels(pins.iter().map(|p| p.label.as_str())),
            hauksbee_ir::vocabulary::names_summary(&nets),
            if one { "is" } else { "are" },
            if one { "input" } else { "inputs" },
            if one { "floats" } else { "float" },
        ),
        refs: vec![comp.reference.clone()],
        nets,
    }
}

/// Join pin labels, collapsing runs of one prefix with consecutive numbers:
/// `DS, D0, D1, D2` reads `DS, D0-D2`.
fn compress_labels<'a>(labels: impl Iterator<Item = &'a str>) -> String {
    let split = |s: &str| {
        let digits = s.len() - s.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        let (head, tail) = s.split_at(s.len() - digits);
        (head.to_string(), tail.parse::<u32>().ok())
    };
    let mut items: Vec<(String, Option<u32>, String)> = labels
        .map(|l| {
            let (h, n) = split(l);
            (h, n, l.to_string())
        })
        .collect();
    items.sort_by(|a, b| (a.0.as_str(), a.1).cmp(&(b.0.as_str(), b.1)));
    items.dedup_by(|a, b| a.2 == b.2);
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let mut j = i;
        while let (Some(n), Some(next)) = (
            items[j].1,
            items
                .get(j + 1)
                .filter(|x| x.0 == items[i].0)
                .and_then(|x| x.1),
        ) {
            if next != n + 1 {
                break;
            }
            j += 1;
        }
        if j - i >= 2 {
            parts.push(format!("{}-{}", items[i].2, items[j].2));
        } else {
            parts.extend(items[i..=j].iter().map(|x| x.2.clone()));
        }
        i = j + 1;
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::Net;

    fn pin(number: &str, net: Option<i64>, function: &str, kind: &str) -> Pin {
        Pin {
            number: number.into(),
            net,
            function: function.into(),
            kind: kind.into(),
            ..Default::default()
        }
    }

    /// A 74HC165 with its card's pinout; `wire(pad)` picks each input's net.
    fn hc165(reference: &str, wire: impl Fn(&str) -> (Option<i64>, &'static str)) -> Component {
        let names = [
            ("1", "~{PL}"),
            ("2", "CP"),
            ("3", "D4"),
            ("4", "D5"),
            ("5", "D6"),
            ("6", "D7"),
            ("10", "DS"),
            ("11", "D0"),
            ("12", "D1"),
            ("13", "D2"),
            ("14", "D3"),
            ("15", "~{CE}"),
        ];
        let mut pins: Vec<Pin> = names
            .iter()
            .map(|(n, f)| {
                let (net, kind) = wire(n);
                pin(n, net, f, kind)
            })
            .collect();
        pins.push(pin("7", None, "~{Q7}", "output+no_connect"));
        pins.push(pin("8", Some(1), "GND", "power_in"));
        pins.push(pin("9", Some(3), "Q7", "output"));
        pins.push(pin("16", Some(2), "VCC", "power_in"));
        Component {
            reference: reference.into(),
            value: "74HC165".into(),
            lib_id: "74xx:74HC165".into(),
            pins,
            ..Default::default()
        }
    }

    fn board(components: Vec<Component>, extra: &[(i64, &str)]) -> ExtractedBoard {
        let mut nets = vec![
            Net::new(1, "GND"),
            Net::new(2, "+5V"),
            Net::new(3, "SER_OUT"),
        ];
        nets.extend(extra.iter().map(|(id, n)| Net::new(*id, *n)));
        ExtractedBoard {
            name: "t".into(),
            nets,
            components,
        }
    }

    fn mcu_driving(nets: &[i64]) -> Component {
        Component {
            reference: "U9".into(),
            value: "ATmega328P-AU".into(),
            pins: nets
                .iter()
                .enumerate()
                .map(|(i, n)| pin(&(i + 1).to_string(), Some(*n), "", "bidirectional"))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn cmos_families_are_recognised_and_bipolar_ones_are_not() {
        for v in [
            "74HC165",
            "SN74HC595N",
            "74HCT08",
            "SN74LVC1G3157DBVR",
            "74AHCT1G32",
            "CD4051B",
            "HEF4094",
            "MC14066",
            "NC7SZ125",
            "74AUP1G04",
            "TXB0101DCKR",
        ] {
            assert!(is_cmos_logic_value(v), "{v}");
        }
        for v in [
            "74LS00",
            "SN74ABT245",
            "74LVT245",
            "ATmega328P",
            "LM358",
            "74F00",
        ] {
            assert!(!is_cmos_logic_value(v), "{v}");
        }
    }

    /// A TXB0101 on +5V/GND with A and B driven, OE on net `oe`.
    fn txb0101(oe: Option<i64>, kind: &str) -> Component {
        let pins = vec![
            pin("1", Some(2), "VCCA", "power_in"),
            pin("2", Some(1), "GND", "power_in"),
            pin("3", Some(3), "A", "bidirectional"),
            pin("4", Some(3), "B", "bidirectional"),
            pin("5", oe, "OE", kind),
            pin("6", Some(2), "VCCB", "power_in"),
        ];
        Component {
            reference: "U1".into(),
            value: "TXB0101DCK".into(),
            pins,
            ..Default::default()
        }
    }

    #[test]
    fn a_txb_oe_left_open_floats_and_a_pulldown_ties_it() {
        // SCES639F 7.3: OE "should be tied to GND through a pulldown resistor".
        let open = board(
            vec![txb0101(Some(9), "input+no_connect")],
            &[(9, "unconnected-(U1-OE-Pad5)")],
        );
        let found = floating_input_lint(&open, &ModelLibrary::builtin()).findings;
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0]
                .message
                .starts_with("U1 TXB0101DCK: input OE is unconnected"),
            "{}",
            found[0].message
        );
        let pulldown = Component {
            reference: "R1".into(),
            value: "100k".into(),
            pins: vec![
                pin("1", Some(9), "", "passive"),
                pin("2", Some(1), "", "passive"),
            ],
            ..Default::default()
        };
        let tied = board(vec![txb0101(Some(9), "input"), pulldown], &[(9, "OE")]);
        assert!(floating_input_lint(&tied, &ModelLibrary::builtin())
            .findings
            .is_empty());
    }

    #[test]
    fn no_connect_flagged_inputs_on_a_74hc165_are_one_finding() {
        // Seven data inputs flagged no-connect on their own KiCad nets, the
        // shape of a partly used shift register; everything else is driven or tied.
        let mut extra = vec![(4, "LOAD"), (5, "CLK"), (6, "D6"), (7, "D7")];
        let open = ["10", "11", "12", "13", "14", "3", "4"];
        let names: Vec<String> = open
            .iter()
            .map(|n| format!("unconnected-(U1-X-Pad{n})"))
            .collect();
        for (i, n) in names.iter().enumerate() {
            extra.push((100 + i64::try_from(i).unwrap(), n.as_str()));
        }
        let u1 = hc165("U1", |n| match n {
            "1" => (Some(4), "input"),
            "2" => (Some(5), "input"),
            "5" => (Some(6), "input"),
            "6" => (Some(7), "input"),
            "15" => (Some(1), "input"),
            _ => {
                let i = open.iter().position(|o| *o == n).unwrap();
                (Some(100 + i64::try_from(i).unwrap()), "input+no_connect")
            }
        });
        let b = board(vec![u1, mcu_driving(&[4, 5, 6, 7, 3])], &extra);
        let report = floating_input_lint(&b, &ModelLibrary::builtin());
        let found: Vec<_> = report.of_check(LintCheck::FloatingInput).collect();
        assert_eq!(found.len(), 1, "{:?}", report.findings);
        let m = &found[0].message;
        assert!(
            m.starts_with("U1 74HC165: inputs D0-D5, DS are unconnected"),
            "{m}"
        );
        assert!(m.contains("no-connect flag"), "{m}");
        assert_eq!(found[0].severity, Severity::High);
        assert_eq!(found[0].refs, ["U1"]);
    }

    #[test]
    fn driven_tied_and_pulled_inputs_are_legal() {
        // Every input driven by the MCU, tied to a rail, or pulled by a resistor.
        let r = Component {
            reference: "R1".into(),
            value: "10k".into(),
            lib_id: "Device:R".into(),
            footprint: "R_0603".into(),
            pins: vec![
                pin("1", Some(8), "", "passive"),
                pin("2", Some(2), "", "passive"),
            ],
            ..Default::default()
        };
        let u1 = hc165("U1", |n| match n {
            "1" => (Some(4), "input"),
            "2" => (Some(5), "input"),
            "10" => (Some(8), "input"),
            "15" => (Some(1), "input"),
            _ => (Some(2), "input"),
        });
        let b = board(
            vec![u1, r, mcu_driving(&[4, 5, 3])],
            &[(4, "LOAD"), (5, "CLK"), (8, "SER_PULL")],
        );
        let report = floating_input_lint(&b, &ModelLibrary::builtin());
        assert!(report.is_clean(), "{:?}", report.findings);
    }

    #[test]
    fn inputs_sharing_a_net_with_only_other_inputs_still_float_and_card_pins_need_no_types() {
        // PCB-only shape: no pin types or names, the card's pin table names the
        // inputs. U1's serial input and U2's clock share a net nothing drives.
        let strip = |mut c: Component| {
            for p in &mut c.pins {
                p.kind.clear();
                p.function.clear();
            }
            c
        };
        let wire = |shared: &'static str| {
            move |n: &str| {
                if n == shared {
                    (Some(9), "input")
                } else {
                    (Some(2), "input")
                }
            }
        };
        let u1 = strip(hc165("U1", wire("10")));
        let u2 = strip(hc165("U2", wire("2")));
        let b = board(vec![u1, u2], &[(9, "SHARED")]);
        let report = floating_input_lint(&b, &ModelLibrary::builtin());
        let found: Vec<_> = report.of_check(LintCheck::FloatingInput).collect();
        assert_eq!(found.len(), 2, "{:?}", report.findings);
        assert!(
            found[0].message.contains("input SER is"),
            "{}",
            found[0].message
        );
        assert!(
            found[1].message.contains("input CLK is"),
            "{}",
            found[1].message
        );
    }

    #[test]
    fn a_named_control_pin_alone_on_its_net_is_left_to_the_control_pin_lint() {
        let u1 = hc165("U1", |n| match n {
            "15" => (Some(9), "input"),
            _ => (Some(2), "input"),
        });
        let b = board(vec![u1], &[(9, "CE_FLOAT")]);
        let report = floating_input_lint(&b, &ModelLibrary::builtin());
        assert!(report.is_clean(), "{:?}", report.findings);
        assert_eq!(
            b.net_lint().of_check(LintCheck::FloatingControlPin).count(),
            1
        );
    }

    #[test]
    fn a_bipolar_or_non_logic_part_is_not_judged() {
        let mut u1 = hc165("U1", |_| (None, "input"));
        u1.value = "74LS165".into();
        let report = floating_input_lint(&board(vec![u1], &[]), &ModelLibrary::builtin());
        assert!(report.is_clean(), "{:?}", report.findings);
    }

    /// One 74LVC1G125 whose A input (pad 2) sits on net 7 with `others`.
    fn buffer_input_with(others: Vec<Component>) -> NetLintReport {
        let u1 = Component {
            reference: "U1".into(),
            value: "74LVC1G125".into(),
            pins: vec![
                pin("1", Some(1), "~{OE}", "input"),
                pin("2", Some(7), "A", "input"),
                pin("3", Some(1), "GND", "power_in"),
                pin("4", Some(3), "Y", "output"),
                pin("5", Some(2), "VCC", "power_in"),
            ],
            ..Default::default()
        };
        let mut parts = vec![u1];
        parts.extend(others);
        let b = board(parts, &[(7, "SIG")]);
        floating_input_lint(&b, &ModelLibrary::builtin())
    }

    #[test]
    fn an_input_driven_only_by_a_dnp_module_is_a_stated_condition() {
        // Legal control: the controller module is marked DNP because it is
        // soldered by hand; it drives the input on the finished board.
        let mut module = mcu_driving(&[7]);
        module.reference = "MCU1".into();
        module.dnp = true;
        let r = buffer_input_with(vec![module]);
        assert_eq!(r.findings.len(), 1, "{:?}", r.findings);
        assert_eq!(r.findings[0].severity, Severity::Low);
        assert!(
            r.findings[0].message.contains("driven only by MCU1"),
            "{}",
            r.findings[0].message
        );
    }

    /// A hand-fitted plug-in module named the IEEE 315 way (`A101`), with no
    /// IC-style designator, marked DNP and wired to `nets`.
    fn dnp_module(nets: &[i64]) -> Component {
        let mut module = mcu_driving(nets);
        module.reference = "A101".into();
        module.value = "Arduino_Nano_v3.x".into();
        module.dnp = true;
        module.pins.push(pin("29", Some(1), "GND", "power_in"));
        module.pins.push(pin("30", Some(2), "+5V", "power_in"));
        module
    }

    #[test]
    fn a_dnp_module_with_an_assembly_designator_drives_its_inputs() {
        // The module's lines are wired to an absent part, not unconnected.
        let r = buffer_input_with(vec![dnp_module(&[7])]);
        assert_eq!(r.findings.len(), 1, "{:?}", r.findings);
        assert_eq!(r.findings[0].severity, Severity::Low);
        assert!(
            r.findings[0].message.contains("driven only by A101"),
            "{}",
            r.findings[0].message
        );
    }

    #[test]
    fn many_parts_behind_one_dnp_module_are_one_condition() {
        let hc = |reference: &str, base: i64| {
            hc165(reference, |n| match n {
                "1" => (Some(base), "input"),
                "2" => (Some(base + 1), "input"),
                _ => (Some(2), "input"),
            })
        };
        let parts = vec![hc("U1", 10), hc("U2", 20), hc("U3", 30), hc("U4", 40)];
        let mut all = parts;
        all.push(dnp_module(&[10, 11, 20, 21, 30, 31, 40, 41]));
        let extra: Vec<(i64, String)> = [10, 11, 20, 21, 30, 31, 40, 41]
            .iter()
            .map(|id| (*id, format!("LINE{id}")))
            .collect();
        let extra: Vec<(i64, &str)> = extra.iter().map(|(i, n)| (*i, n.as_str())).collect();
        let report = floating_input_lint(&board(all, &extra), &ModelLibrary::builtin());
        let found: Vec<_> = report.of_check(LintCheck::FloatingInput).collect();
        assert_eq!(found.len(), 1, "{:?}", report.findings);
        let m = &found[0].message;
        assert_eq!(found[0].severity, Severity::Low);
        assert_eq!(found[0].refs, ["U1", "U2", "U3", "U4"]);
        assert!(
            m.starts_with(
                "8 CMOS inputs on 4 parts (U1 /PL, CP; U2 /PL, CP; U3 /PL, CP; and 1 more"
            ) && m.contains("driven only by A101"),
            "{m}"
        );
    }

    #[test]
    fn a_dnp_jumper_or_resistor_is_not_a_driver() {
        // Control for the module rule: a three-pad solder jumper left open is
        // the link the board leaves out, so the input still floats.
        let jumper = Component {
            reference: "JP1".into(),
            value: "SolderJumper_3".into(),
            dnp: true,
            pins: vec![
                pin("1", Some(1), "", "passive"),
                pin("2", Some(7), "", "passive"),
                pin("3", Some(2), "", "passive"),
            ],
            ..Default::default()
        };
        let r = buffer_input_with(vec![jumper]);
        let high = (r.findings.iter())
            .filter(|f| f.refs == ["U1"] && f.severity == Severity::High)
            .count();
        assert_eq!(high, 1, "{:?}", r.findings);
    }

    #[test]
    fn an_input_left_only_a_dnp_pull_or_nothing_still_floats() {
        // A DNP pull-up is the pull the assembled board leaves out.
        let pull = Component {
            reference: "R1".into(),
            value: "10k".into(),
            lib_id: "Device:R".into(),
            dnp: true,
            pins: vec![
                pin("1", Some(7), "", "passive"),
                pin("2", Some(2), "", "passive"),
            ],
            ..Default::default()
        };
        // Another buffer's input is not a driver either.
        let other = Component {
            reference: "U2".into(),
            value: "74LVC1G125".into(),
            pins: vec![pin("2", Some(7), "A", "input")],
            ..Default::default()
        };
        for others in [vec![pull], vec![], vec![other]] {
            let r = buffer_input_with(others);
            let high = r
                .findings
                .iter()
                .filter(|f| f.refs == ["U1"] && f.severity == Severity::High)
                .count();
            assert_eq!(high, 1, "{:?}", r.findings);
        }
    }

    /// A 74LVC1G125 on +5V whose `~{OE}` (pad 1) sits on net 8 `OE_NET` and
    /// whose A input (pad 2) is driven by the MCU, with `others` added.
    fn buffer_oe_with(others: Vec<Component>) -> NetLintReport {
        let u1 = Component {
            reference: "U1".into(),
            value: "74LVC1G125".into(),
            pins: vec![
                pin("1", Some(8), "~{OE}", "input"),
                pin("2", Some(4), "A", "input"),
                pin("3", Some(1), "GND", "power_in"),
                pin("4", Some(3), "Y", "output"),
                pin("5", Some(2), "VCC", "power_in"),
            ],
            ..Default::default()
        };
        let mut parts = vec![u1, mcu_driving(&[4])];
        parts.extend(others);
        floating_input_lint(
            &board(parts, &[(4, "SIG"), (8, "OE_NET"), (9, "FAR")]),
            &ModelLibrary::builtin(),
        )
    }

    fn two_pin(reference: &str, value: &str, a: i64, b: i64) -> Component {
        Component {
            reference: reference.into(),
            value: value.into(),
            lib_id: "Device:R".into(),
            pins: vec![
                pin("1", Some(a), "", "passive"),
                pin("2", Some(b), "", "passive"),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn an_output_enable_held_only_by_a_button_may_float_and_a_pull_up_fixes_it() {
        let button = Component {
            reference: "SW1".into(),
            value: "SW_Push".into(),
            pins: vec![
                pin("1", Some(8), "", "passive"),
                pin("2", Some(1), "", "passive"),
            ],
            ..Default::default()
        };
        let r = buffer_oe_with(vec![button.clone()]);
        assert_eq!(r.findings.len(), 1, "{:?}", r.findings);
        let f = &r.findings[0];
        assert_eq!(f.severity, Severity::Medium);
        for needle in ["may float", "SW1 pin 1", "pull-up to '+5V'", "OE_NET"] {
            assert!(f.message.contains(needle), "{needle}: {}", f.message);
        }
        let fixed = buffer_oe_with(vec![button, two_pin("R1", "10k", 8, 2)]);
        assert!(fixed.is_clean(), "{:?}", fixed.findings);
    }

    #[test]
    fn a_control_input_on_a_connector_alone_may_float_but_a_data_input_is_the_connectors() {
        let header = |net| Component {
            reference: "J1".into(),
            value: "Conn_01x02".into(),
            pins: vec![pin("1", Some(net), "", "passive")],
            ..Default::default()
        };
        let r = buffer_oe_with(vec![header(8)]);
        assert_eq!(r.findings.len(), 1, "{:?}", r.findings);
        assert!(r.findings[0].message.contains("J1 pin 1"));
        // The same header on the data input: the off-board driver is its pull.
        let u1 = Component {
            reference: "U1".into(),
            value: "74LVC1G125".into(),
            pins: vec![
                pin("1", Some(1), "~{OE}", "input"),
                pin("2", Some(8), "A", "input"),
                pin("3", Some(1), "GND", "power_in"),
                pin("5", Some(2), "VCC", "power_in"),
            ],
            ..Default::default()
        };
        let b = board(vec![u1, header(8)], &[(8, "IN")]);
        let r = floating_input_lint(&b, &ModelLibrary::builtin());
        assert!(r.is_clean(), "{:?}", r.findings);
    }

    #[test]
    fn an_open_collector_sink_alone_may_float_and_a_header_selector_defines_the_level() {
        let npn = Component {
            reference: "Q1".into(),
            value: "Q_NPN_BCE".into(),
            lib_id: "Device:Q_NPN_BCE".into(),
            pins: vec![
                pin("1", Some(9), "B", "passive"),
                pin("2", Some(8), "C", "passive"),
                pin("3", Some(1), "E", "passive"),
            ],
            ..Default::default()
        };
        let r = buffer_oe_with(vec![npn]);
        assert_eq!(r.findings.len(), 1, "{:?}", r.findings);
        assert!(r.findings[0].message.contains("Q1's collector"));
        // A three-pin jumper header strapping the pin between +5V and GND is a selector.
        let selector = Component {
            reference: "JP1".into(),
            value: "Conn_01x03".into(),
            pins: vec![
                pin("1", Some(2), "", "passive"),
                pin("2", Some(8), "", "passive"),
                pin("3", Some(1), "", "passive"),
            ],
            ..Default::default()
        };
        let r = buffer_oe_with(vec![selector]);
        assert!(r.is_clean(), "{:?}", r.findings);
    }

    #[test]
    fn a_series_resistor_to_a_pulled_net_holds_the_level() {
        // OE_NET -- 1k -- FAR, and FAR has a 10k up: one hop is followed.
        let r = buffer_oe_with(vec![two_pin("R1", "1k", 8, 9), two_pin("R2", "10k", 9, 2)]);
        assert!(r.is_clean(), "{:?}", r.findings);
        // The same resistor into a far net nothing holds: the input still floats.
        let r = buffer_oe_with(vec![two_pin("R1", "1k", 8, 9)]);
        assert_eq!(r.findings.len(), 1, "{:?}", r.findings);
        assert_eq!(r.findings[0].severity, Severity::High);
    }

    #[test]
    fn a_card_bound_non_cmos_value_is_judged_by_its_card_and_bipolar_is_not() {
        // 28C256 EEPROM: card-bound, value not a 74xx family, ~{CE} (pad 20) open.
        let mut pins: Vec<Pin> = Vec::new();
        for (n, net) in [("14", 1), ("28", 2)] {
            pins.push(pin(n, Some(net), "", "power_in"));
        }
        pins.push(pin("20", Some(9), "", "passive"));
        let eeprom = Component {
            reference: "U4".into(),
            value: "28C256".into(),
            pins,
            ..Default::default()
        };
        let r = floating_input_lint(
            &board(vec![eeprom], &[(9, "Net-(U4-Pad20)")]),
            &ModelLibrary::builtin(),
        );
        assert!(
            r.findings.iter().any(|f| f.refs == ["U4"]),
            "{:?}",
            r.findings
        );
        assert!(is_bipolar_value("SN74LS245") && is_bipolar_value("74ABT245"));
        assert!(!is_bipolar_value("74HC245") && !is_bipolar_value("28C256"));
    }

    #[test]
    fn a_fully_open_op_amp_input_floats_and_a_driven_one_does_not() {
        let amp = |in_plus: i64| Component {
            reference: "U5".into(),
            value: "LM358".into(),
            pins: vec![
                pin("1", Some(4), "", "output"),
                pin("2", Some(4), "", "input"),
                pin("3", Some(in_plus), "", "input"),
                pin("4", Some(1), "", "power_in"),
                pin("5", Some(4), "", "input"),
                pin("6", Some(4), "", "input"),
                pin("7", Some(4), "", "output"),
                pin("8", Some(2), "", "power_in"),
            ],
            ..Default::default()
        };
        let open = floating_input_lint(
            &board(vec![amp(9)], &[(4, "OUT"), (9, "Net-(U5-IN+)")]),
            &ModelLibrary::builtin(),
        );
        assert_eq!(open.findings.len(), 1, "{:?}", open.findings);
        assert_eq!(open.findings[0].severity, Severity::Medium);
        assert!(open.findings[0]
            .message
            .contains("op-amp or comparator input"));
        // Both inputs on one named net nothing else touches: it may be driven from
        // outside the file, so it is not called open.
        let mut shared = amp(9);
        shared.pins[1].net = Some(9);
        let shared = floating_input_lint(
            &board(vec![shared], &[(4, "OUT"), (9, "SENSE")]),
            &ModelLibrary::builtin(),
        );
        assert!(shared.is_clean(), "{:?}", shared.findings);
        let driven = floating_input_lint(
            &board(vec![amp(4)], &[(4, "OUT")]),
            &ModelLibrary::builtin(),
        );
        assert!(driven.is_clean(), "{:?}", driven.findings);
    }

    #[test]
    fn labels_collapse_consecutive_runs() {
        assert_eq!(
            compress_labels(["DS", "D0", "D1", "D2", "D3", "D5"].into_iter()),
            "D0-D3, D5, DS"
        );
        assert_eq!(compress_labels(["A", "B"].into_iter()), "A, B");
        assert_eq!(display_name("~{CE}"), "/CE");
    }
}
