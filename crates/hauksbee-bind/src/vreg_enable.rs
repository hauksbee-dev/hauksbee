//! A simple regulator's enable or shutdown pin.
//!
//! The ideal-source stamp of [`crate::binder`]'s `bind_vreg` holds its rail
//! whatever the board does with the part's `en`/`shdn` pin. Here that pin
//! switches the rail: the source returns to a private node, and a
//! [`Device::VSwitch`] controlled by the pin (against the part's GND) closes
//! that node to GND. On, the rail is the old one (through [`GATE_RON`]); off,
//! the source floats, nothing is driven, and the rail decays through whatever
//! loads and capacitance the board hangs on it, or through the card's active
//! discharge when it states one. The board's latching (hysteretic) switch model
//! holds the state between the guaranteed-off and guaranteed-on levels, so a
//! slow edge does not chatter.
//!
//! The part's own supply current follows the same switch ([`Plan::quiescent`]):
//! on, IN draws the card's `iq_a`; off, only its shutdown current `i_shdn_a`,
//! or nothing when the card states none (said as a partial model).
//!
//! The card supplies the levels (`en_on_v`/`en_off_v`), the polarity
//! (`en_on_level`), an internal pull and an active discharge; without levels the
//! gate uses [`DEFAULT_ON_V`]/[`DEFAULT_OFF_V`] and says so as a partial model.
//!
//! A pin whose level the board fixes needs no solve to read, and is decided
//! here: tied to the part's input (on, for an on-high pin, so no gate at all),
//! tied to GND, or pulled to GND by resistors alone with nothing else on the
//! net, or left open on a card with an internal pull. A rail held off that way
//! is recorded on the gate ([`EnableGate::held_off`]), so the unpowered-rail
//! census names it, rather than leaving a 0 V rail for a reader to discover.

use std::collections::HashMap;

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::Component;
use hauksbee_ir::evidence::Assumption;
use hauksbee_ir::{Circuit, Device, DeviceId, NodeId};
use hauksbee_models::ModelEntry;

use crate::program_resistor::{is_ground_net, program_resistance_to_ground};

/// Guaranteed-on level assumed for an on-high pin whose card states none:
/// the common CMOS regulator enable reads high from 0.9 to 2 V, and a 1.8 V
/// or 3.3 V logic high clears this one.
pub const DEFAULT_ON_V: f64 = 1.2;
/// Guaranteed-off level assumed alongside [`DEFAULT_ON_V`]: the logic-low
/// maximum most regulator enables print.
pub const DEFAULT_OFF_V: f64 = 0.4;
/// The closed gate's resistance: far below any regulator's own output
/// impedance, so an enabled rail reads as it did ungated.
pub const GATE_RON: f64 = 1e-4;
/// The open gate's (and the open discharge switch's) leakage path.
pub const GATE_ROFF: f64 = 1e9;
/// Narrowest band between the two levels, so a card printing one threshold for
/// both still gives the latch a band to switch across.
const MIN_BAND_V: f64 = 0.01;

/// A regulator whose output its enable pin switches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnableGate {
    /// The switch between the rail source's private return and GND.
    pub switch: DeviceId,
    /// The part's GND: the return the rail and its input headroom are measured
    /// against, which the source itself no longer touches.
    pub ret: NodeId,
    /// Why the board holds this regulator off whatever runs, when it does: the
    /// pin is tied or pulled to its off level with nothing else to move it.
    pub held_off: Option<String>,
}

/// The card's levels for the pin as (stated?, on, off), the defaults standing
/// in for a card with none, off on the far side of on by at least the minimum
/// band.
fn levels(model: &ModelEntry, on_high: bool) -> (bool, f64, f64) {
    let params = &model.params;
    let stated = params.get_f64("en_on_v").zip(params.get_f64("en_off_v"));
    let (on_v, off_v) = stated.unwrap_or(if on_high {
        (DEFAULT_ON_V, DEFAULT_OFF_V)
    } else {
        (DEFAULT_OFF_V, DEFAULT_ON_V)
    });
    let off_v = if on_high {
        off_v.min(on_v - MIN_BAND_V)
    } else {
        off_v.max(on_v + MIN_BAND_V)
    };
    (stated.is_some(), on_v, off_v)
}

/// The pin's board net at `node`: how many other fitted parts touch it, and
/// whether they are all resistors to GND (or capacitors, open at DC).
fn census(
    comp: &Component,
    (components, circuit): (&[Component], &Circuit),
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
    node: NodeId,
) -> (usize, bool) {
    let me = components
        .iter()
        .position(|c| c.reference == comp.reference);
    let Some(net) = comp
        .pins
        .iter()
        .find(|p| node_of(p.net) == Some(node))
        .and_then(|p| p.net)
    else {
        return (0, false);
    };
    let on_net = |c: &Component| c.pins.iter().any(|p| p.net == Some(net));
    let others = components
        .iter()
        .enumerate()
        .filter(|(i, c)| Some(*i) != me && on_net(c))
        .filter(|(_, c)| !matches!(AssemblyState::of(c), AssemblyState::DnpAbsent(_)))
        .count();
    let is_ground =
        |id: i64| node_of(Some(id)).map(|n| n.is_ground() || is_ground_net(circuit.node_name(n)));
    let resistors = program_resistance_to_ground(components, &is_ground, net, 0, me).is_some();
    (others, resistors)
}

/// The card's internal pull on the pin.
#[derive(Clone, Copy)]
enum Pull {
    Ohms { up: bool, ohms: f64 },
    Amps { up: bool, amps: f64 },
}

impl Pull {
    fn of(model: &ModelEntry) -> Option<Self> {
        let p = |k| {
            model
                .params
                .get_f64(k)
                .filter(|v| v.is_finite() && *v > 0.0)
        };
        let ohms = |up, ohms| Self::Ohms { up, ohms };
        let amps = |up, amps| Self::Amps { up, amps };
        p("en_pull_down_ohms")
            .map(|v| ohms(false, v))
            .or_else(|| p("en_pull_up_ohms").map(|v| ohms(true, v)))
            .or_else(|| p("en_pull_down_a").map(|v| amps(false, v)))
            .or_else(|| p("en_pull_up_a").map(|v| amps(true, v)))
    }

    const fn up(self) -> bool {
        match self {
            Self::Ohms { up, .. } | Self::Amps { up, .. } => up,
        }
    }

    const fn word(self) -> &'static str {
        if self.up() {
            "pull-up"
        } else {
            "pull-down"
        }
    }
}

/// What the binder does with one regulator's enable pin.
pub struct Plan {
    /// The pin as the card names it (`EN`, `SHDN`), for the notes.
    pin: &'static str,
    /// `None`: the rail is not gated (no pin, or one the board holds on).
    gate: Option<Gate>,
    /// The disclosure for a pin the model reads only on an assumption.
    caveat: Option<String>,
}

struct Gate {
    /// The node the switch reads: the pin's board node, or GND for a pin the
    /// board leaves open on a card whose pull holds it off.
    control: NodeId,
    /// Whether the output is on with the pin HIGH.
    on_high: bool,
    on_v: f64,
    off_v: f64,
    /// The internal pull, stamped only on a board node.
    pull: Option<Pull>,
    discharge_ohms: Option<f64>,
    held_off: Option<String>,
}

impl Gate {
    /// The switch's control pair and the sign that carries the thresholds: the
    /// pin against GND for an `on`-high reading, the reverse for an on-low one.
    const fn sense(&self, gnd: NodeId, on: bool) -> (NodeId, NodeId, f64) {
        if on {
            (self.control, gnd, 1.0)
        } else {
            (gnd, self.control, -1.0)
        }
    }
}

impl Plan {
    /// Read the card and the board around the pin. `components` and `node_of`
    /// are the binder's; `circuit` is only read, for net names.
    #[must_use]
    pub fn new(
        comp: &Component,
        model: &ModelEntry,
        roles: &HashMap<String, NodeId>,
        (components, circuit): (&[Component], &Circuit),
        node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
        (input, gnd): (NodeId, NodeId),
    ) -> Self {
        let Some(role) = ["en", "shdn"]
            .into_iter()
            .find(|r| model.pins.values().any(|v| v == r))
        else {
            return Self::ungated("EN", None);
        };
        let pin = if role == "en" { "EN" } else { "SHDN" };
        let params = &model.params;
        let on_high = params.get_str("en_on_level") != Some("low");
        let (stated, on_v, off_v) = levels(model, on_high);
        let pull = Pull::of(model);
        let who = format!("{} ({})", comp.reference, comp.value);
        let gate = |control, held_off: Option<String>| Gate {
            control,
            on_high,
            on_v,
            off_v,
            pull: pull.filter(|_| control != NodeId::GROUND),
            discharge_ohms: params
                .get_f64("en_discharge_ohms")
                .filter(|r| r.is_finite() && *r > 0.0),
            held_off,
        };
        let gated = |g: Gate, caveat| Self {
            pin,
            gate: Some(g),
            caveat,
        };
        // With nothing on the pin, the card's pull decides; with no pull the
        // datasheet does not say, so the rail is assumed on and that is said.
        let open = |control: NodeId| match pull {
            Some(p) if p.up() == on_high => {
                if control == NodeId::GROUND {
                    Self::ungated(pin, None)
                } else {
                    gated(gate(control, None), None)
                }
            }
            Some(p) => {
                let why = format!(
                    "its {pin} pin is left open and the card's internal {} holds it off",
                    p.word()
                );
                let mut g = gate(control, Some(why));
                if control == NodeId::GROUND {
                    // No board node to read: the gate reads 0 V as an on-high
                    // pin's off level, whichever way the part is wired.
                    (g.on_high, g.on_v, g.off_v) = (true, DEFAULT_ON_V, DEFAULT_OFF_V);
                }
                gated(g, None)
            }
            None => Self::ungated(
                pin,
                Some(format!(
                    "{}{who}: its {pin} pin is left open and the card states no internal pull, \
                     so the datasheet does not say whether the part is on; the output rail is \
                     assumed on",
                    Assumption::PARTIAL_MODEL_MARKER
                )),
            ),
        };
        let Some(node) = roles.get(role).copied() else {
            return open(NodeId::GROUND);
        };
        if node == input {
            return if on_high {
                Self::ungated(pin, None)
            } else {
                let why = format!("its {pin} pin is tied to its input, which turns it off");
                gated(gate(node, Some(why)), None)
            };
        }
        if node.is_ground() || node == gnd || is_ground_net(circuit.node_name(node)) {
            return if on_high {
                gated(
                    gate(node, Some(format!("its {pin} pin is tied to GND"))),
                    None,
                )
            } else {
                Self::ungated(pin, None)
            };
        }
        let (others, resistors_to_gnd) = census(comp, (components, circuit), node_of, node);
        if others == 0 {
            return open(node);
        }
        // No internal pull-up to make the resistors a divider: held low
        // whatever runs.
        if on_high && resistors_to_gnd && !pull.is_some_and(Pull::up) {
            let why = format!(
                "its {pin} pin is pulled to GND through resistors with nothing else on its net"
            );
            return gated(gate(node, Some(why)), None);
        }
        let caveat = (!stated).then(|| {
            format!(
                "{}{who}: its {pin} pin levels are not on the card, so the output is switched \
                 at an assumed {on_v:.2} V on / {off_v:.2} V off; a level between them, or a \
                 part whose real levels differ, may read differently on the board",
                Assumption::PARTIAL_MODEL_MARKER
            )
        });
        gated(gate(node, None), caveat)
    }

    const fn ungated(pin: &'static str, caveat: Option<String>) -> Self {
        Self {
            pin,
            gate: None,
            caveat,
        }
    }

    /// The node the rail's source returns to: a private one the gate closes to
    /// `gnd`, or `gnd` itself when the rail is not gated.
    pub fn source_return(&self, circuit: &mut Circuit, reference: &str, gnd: NodeId) -> NodeId {
        if self.gate.is_some() {
            circuit.node(&format!("{reference}_vreg_en_ret"))
        } else {
            gnd
        }
    }

    /// Stamp the regulator's supply current from `input` to `gnd`, `amps` being
    /// the card's `iq_a` signed for the rail's polarity, and give the sources
    /// to register as the stamp's helpers.
    ///
    /// Ungated, that is the one `Iq_<ref>` draw. Gated, the draw splits:
    /// `i_shdn_a` flows always (the part's shutdown current), and the rest,
    /// `iq_a - i_shdn_a`, flows through a switch `Sq_<ref>` on the same pin,
    /// levels and latch as the pass element's, so the sum is `iq_a` on and
    /// `i_shdn_a` off. A card with no `i_shdn_a` draws nothing off, and says so.
    pub fn quiescent(
        &mut self,
        circuit: &mut Circuit,
        comp: &Component,
        model: &ModelEntry,
        (input, gnd): (NodeId, NodeId),
        amps: f64,
    ) -> Vec<DeviceId> {
        use crate::binder::supply_draw;
        let r = &comp.reference;
        let Some(g) = &self.gate else {
            return vec![supply_draw(circuit, format!("Iq_{r}"), (input, gnd), amps)];
        };
        let shutdown = model
            .params
            .get_f64("i_shdn_a")
            .filter(|i| i.is_finite() && *i >= 0.0 && *i < amps.abs())
            .map(|i| i.copysign(amps));
        // The switched part ends on a private node the switch closes to GND.
        // Off, the draw's own fade (it needs a supply across it) takes the
        // current to nothing as that node rises to IN.
        let tail = circuit.node(&format!("{r}_vreg_iq"));
        let (cp, cn, s) = g.sense(gnd, g.on_high);
        circuit.add(Device::VSwitch {
            name: format!("Sq_{r}"),
            a: tail,
            b: gnd,
            ctrl_p: cp,
            ctrl_n: cn,
            von: s * g.on_v,
            voff: s * g.off_v,
            ron: GATE_RON,
            roff: GATE_ROFF,
        });
        let mut helpers = vec![supply_draw(
            circuit,
            format!("Iq_{r}"),
            (input, tail),
            amps - shutdown.unwrap_or(0.0),
        )];
        let Some(shutdown) = shutdown else {
            let said = format!(
                "the card states no shutdown supply current (`i_shdn_a`), so IN is assumed to \
                 draw 0 A while its {} pin holds the part off; a real part draws its datasheet \
                 shutdown current",
                self.pin
            );
            let first = format!(
                "{}{} ({}): {said}",
                Assumption::PARTIAL_MODEL_MARKER,
                comp.reference,
                comp.value
            );
            self.caveat = Some(
                self.caveat
                    .take()
                    .map_or(first, |c| format!("{c}; also {said}")),
            );
            return helpers;
        };
        // A stated zero is a statement, and needs no source.
        if shutdown.abs() > 0.0 {
            helpers.push(supply_draw(
                circuit,
                format!("Ishdn_{r}"),
                (input, gnd),
                shutdown,
            ));
        }
        helpers
    }

    /// Stamp the gate, the pull and the discharge. `ret` is what
    /// [`Self::source_return`] returned. Gives the gate (if any), a short label
    /// for the bind row, and the notes for the bind report: the caveat, then
    /// the held-off finding.
    pub fn stamp(
        self,
        circuit: &mut Circuit,
        comp: &Component,
        (input, out, gnd, ret): (NodeId, NodeId, NodeId, NodeId),
        vout: Option<f64>,
    ) -> (Option<EnableGate>, Option<String>, [Option<String>; 2]) {
        let Some(g) = self.gate else {
            return (None, None, [self.caveat, None]);
        };
        let r = &comp.reference;
        let (pin, control) = (self.pin, g.control);
        // The switch conducts when V(cp) - V(cn) passes `von`.
        let sense = |on: bool| g.sense(gnd, on);
        let (cp, cn, s) = sense(g.on_high);
        let switch = circuit.add(Device::VSwitch {
            name: format!("Sen_{r}"),
            a: ret,
            b: gnd,
            ctrl_p: cp,
            ctrl_n: cn,
            von: s * g.on_v,
            voff: s * g.off_v,
            ron: GATE_RON,
            roff: GATE_ROFF,
        });
        if let Some(ohms) = g.discharge_ohms {
            // Closed while the gate is open: past the off level, open past on.
            let (cp, cn, s) = sense(!g.on_high);
            circuit.add(Device::VSwitch {
                name: format!("Sdis_{r}"),
                a: out,
                b: gnd,
                ctrl_p: cp,
                ctrl_n: cn,
                von: s * g.off_v,
                voff: s * g.on_v,
                ron: ohms,
                roff: GATE_ROFF,
            });
        }
        match g.pull {
            Some(Pull::Ohms { up, ohms }) => {
                let to = if up { input } else { gnd };
                circuit.add(Device::resistor(format!("Ren_{r}"), control, to, ohms));
            }
            Some(Pull::Amps { up, amps }) => {
                let ends = if up { (input, control) } else { (control, gnd) };
                crate::binder::supply_draw(circuit, format!("Ien_{r}"), ends, amps);
            }
            None => {}
        }
        let rail = vout.map_or_else(String::new, |v| format!("{v:.2} V "));
        let held = g.held_off.as_ref().map(|why| {
            format!(
                "{r} ({}): held off: {why}, so its {rail}output rail is unpowered and \
                 everything it feeds is unpowered for this run",
                comp.value
            )
        });
        let label = format!("{pin}-gated");
        let gate = EnableGate {
            switch,
            ret: gnd,
            held_off: g.held_off,
        };
        (Some(gate), Some(label), [self.caveat, held])
    }
}
