//! What a solved board can physically say: the source envelope, and the
//! co-sim-local clamps that keep a node nothing holds inside it.
//!
//! A node with no DC path to a voltage source or ground (an unconnected MCU
//! pad, a supply pin the board never powers) is held only by the solver's
//! gmin. If any current source injects into it (a pin driver's draw from its
//! supply pin, a model peripheral's idle current) KCL balances at `I / gmin`
//! volts: a numerically consistent answer to a question the board never asked
//! (measured: -3.1e9 V from a 3 mA driver on a floating 3V3 pad). A real pad
//! cannot do that: its ESD structure clamps it one diode drop outside the
//! rails, which is what [`stamp_floating_clamps`] stamps, only on such nodes.
//!
//! [`Envelope`] is the backstop invariant behind it: no solved node may sit
//! outside `[min source - 1 V, max source + 1 V]`. A state that does is a
//! solver fault, refused like any non-physical solve, never a reading.

use hauksbee_ir::{BOutput, Circuit, Device, DiodeModel, NodeId, SourceKind};
use std::collections::HashSet;

/// How far past the board's own sources a node may sit before the solve is
/// refused (one diode drop of clamp, plus the lead-in of a real edge).
pub const MARGIN_V: f64 = 1.0;

/// A resistor at or above this carries no DC level: it is a leakage leg (the
/// 1 GΩ off-state of a tri-stated pin), not a connection to a reference.
const CONDUCTIVE_BELOW_OHMS: f64 = 1e8;

/// Name prefix of the hidden clamp rails; their values are not board sources.
const CLAMP_RAIL_PREFIX: &str = "Vclamp_";

/// The `[lo, hi]` volts a board's independent sources can produce, extended
/// by [`MARGIN_V`] each side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Envelope {
    pub lo: f64,
    pub hi: f64,
}

/// The smallest and largest value `kind` ever takes.
fn source_extremes(kind: &SourceKind) -> (f64, f64) {
    match kind {
        SourceKind::Dc(v) => (*v, *v),
        SourceKind::Sin {
            offset, amplitude, ..
        } => (offset - amplitude.abs(), offset + amplitude.abs()),
        SourceKind::Pulse { v1, v2, .. } => (v1.min(*v2), v1.max(*v2)),
        SourceKind::Pwl(points) => points
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
                (lo.min(p.v), hi.max(p.v))
            }),
        SourceKind::Ramped { inner, .. } => {
            let (lo, hi) = source_extremes(inner);
            (lo.min(0.0), hi.max(0.0))
        }
    }
}

/// Name prefixes of the current sources that only ever draw from a rail: pin
/// drivers' supply legs, quiescent and peripheral loads, adjustable-regulator
/// bias, model loads and laws. They shift a node by their current times the
/// board's own impedance and cannot lift it past what the sources hold. Any
/// other current source (a stimulus, a reference, an imported SPICE source)
/// can drive a resistor to an arbitrary level.
const DRAW_PREFIXES: [&str; 7] = [
    "Idrh_",
    "Idrl_",
    "Iq_",
    "Iload_",
    "Iperipheral_",
    "Iadj_",
    "Ibeh_",
];

/// The extremes of every board voltage source referenced to ground, ignoring
/// the hidden clamp rails, widened by what a comparator output swings to.
/// `None` when there is no such source.
fn board_source_range(circuit: &Circuit) -> Option<(f64, f64)> {
    let mut range: Option<(f64, f64)> = None;
    for dev in &circuit.devices {
        let (name, p, n, kind) = match dev {
            Device::Vsource { name, p, n, kind } => (name, p, n, kind),
            Device::Comparator { out_lo, out_hi, .. } => {
                let (lo, hi) = (out_lo.min(*out_hi), out_lo.max(*out_hi));
                if lo.is_finite() && hi.is_finite() {
                    range = Some(range.map_or((lo, hi), |(a, b)| (a.min(lo), b.max(hi))));
                }
                continue;
            }
            _ => continue,
        };
        if name.starts_with(CLAMP_RAIL_PREFIX) || !(p.is_ground() || n.is_ground()) {
            continue;
        }
        let (mut lo, mut hi) = source_extremes(kind);
        if p.is_ground() {
            (lo, hi) = (-hi, -lo);
        }
        if !lo.is_finite() || !hi.is_finite() {
            continue;
        }
        range = Some(range.map_or((lo, hi), |(a, b)| (a.min(lo), b.max(hi))));
    }
    range
}

/// Whether the board can legitimately put a node outside its own sources: an
/// energy store that steps up (an inductor, a flying capacitor beside a
/// diode or switch), a controlled or behavioural source, an amplifier, a
/// current source that is not a plain draw ([`DRAW_PREFIXES`]), or a source
/// stacked on another rather than on ground. The envelope does not apply.
fn can_exceed_sources(circuit: &Circuit) -> bool {
    let mut flying_cap = false;
    let mut commutating = false;
    for dev in &circuit.devices {
        match dev {
            Device::Inductor { .. }
            | Device::Coupling { .. }
            | Device::Vcvs { .. }
            | Device::Ccvs { .. }
            | Device::Vccs { .. }
            | Device::Cccs { .. }
            | Device::OpAmp { .. }
            | Device::Behavioral {
                output: BOutput::Voltage,
                ..
            } => return true,
            Device::Isource { name, .. } if !DRAW_PREFIXES.iter().any(|p| name.starts_with(p)) => {
                return true
            }
            Device::Vsource { name, p, n, .. }
                if !name.starts_with(CLAMP_RAIL_PREFIX) && !p.is_ground() && !n.is_ground() =>
            {
                return true
            }
            Device::Capacitor { a, b, .. } if !a.is_ground() && !b.is_ground() => {
                flying_cap = true;
            }
            Device::Diode { .. } | Device::VSwitch { .. } | Device::Mosfet { .. } => {
                commutating = true;
            }
            _ => {}
        }
    }
    flying_cap && commutating
}

impl Envelope {
    /// The envelope of `circuit`, or `None` when it cannot be stated: no
    /// ground-referenced source to bound by, or a board that can legitimately
    /// leave its sources ([`can_exceed_sources`]).
    #[must_use]
    pub fn of(circuit: &Circuit) -> Option<Self> {
        if can_exceed_sources(circuit) {
            return None;
        }
        let (lo, hi) = board_source_range(circuit)?;
        // Ground is always a reference, so the envelope always contains 0 V.
        Some(Self {
            lo: lo.min(0.0) - MARGIN_V,
            hi: hi.max(0.0) + MARGIN_V,
        })
    }

    /// The first `(index, volts)` of `volts` outside the envelope.
    #[must_use]
    pub fn first_outside(&self, volts: &[f64]) -> Option<(usize, f64)> {
        volts
            .iter()
            .copied()
            .enumerate()
            .find(|&(_, v)| v < self.lo || v > self.hi)
    }
}

/// Union-find over node ids.
struct Components {
    parent: Vec<u32>,
}

impl Components {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..u32::try_from(n).unwrap_or(u32::MAX)).collect(),
        }
    }

    fn find(&mut self, x: NodeId) -> u32 {
        let mut root = x.0;
        while self.parent[root as usize] != root {
            root = self.parent[root as usize];
        }
        let mut cur = x.0;
        while self.parent[cur as usize] != root {
            let next = self.parent[cur as usize];
            self.parent[cur as usize] = root;
            cur = next;
        }
        root
    }

    fn union(&mut self, a: NodeId, b: NodeId) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[ra as usize] = rb;
        }
    }
}

/// Every floating node a current source injects into.
///
/// A node floats when its DC component holds no reference (ground, a voltage
/// source, an amplifier or comparator output): the solver would otherwise
/// pick its level as `I/gmin`.
#[must_use]
pub fn floating_injection_nodes(circuit: &Circuit) -> Vec<NodeId> {
    let mut comps = Components::new(circuit.max_node() as usize + 1);
    let mut anchors: Vec<NodeId> = vec![NodeId::GROUND];
    let mut injected: Vec<NodeId> = Vec::new();
    for dev in &circuit.devices {
        match dev {
            Device::Resistor { a, b, ohms, .. } if *ohms < CONDUCTIVE_BELOW_OHMS => {
                comps.union(*a, *b);
            }
            Device::Inductor { a, b, .. }
            | Device::Diode { a, k: b, .. }
            | Device::VSwitch { a, b, .. } => comps.union(*a, *b),
            Device::Mosfet { d, s, b, .. } => {
                comps.union(*d, *s);
                if let Some(b) = b {
                    comps.union(*b, *s);
                }
            }
            Device::Bjt { c, b, e, .. } => {
                comps.union(*c, *e);
                comps.union(*b, *e);
            }
            Device::Vsource { p, n, .. }
            | Device::Vcvs { p, n, .. }
            | Device::Ccvs { p, n, .. }
            | Device::Behavioral {
                p,
                n,
                output: BOutput::Voltage,
                ..
            } => {
                comps.union(*p, *n);
                anchors.push(*p);
            }
            Device::OpAmp { out, .. } | Device::Comparator { out, .. } => anchors.push(*out),
            // A behavioural current law is as often a conductance (the
            // sink-only pin element) as an injection: count it as a connection.
            Device::Behavioral {
                p,
                n,
                output: BOutput::Current,
                ..
            } => comps.union(*p, *n),
            Device::Isource { p, n, .. }
            | Device::Vccs { p, n, .. }
            | Device::Cccs { p, n, .. } => injected.extend([*p, *n]),
            Device::Resistor { .. } | Device::Capacitor { .. } | Device::Coupling { .. } => {}
        }
    }
    let anchored: HashSet<u32> = anchors.into_iter().map(|a| comps.find(a)).collect();
    let mut seen = HashSet::new();
    injected
        .into_iter()
        .filter(|n| !n.is_ground() && (n.0 as usize) < circuit.node_count())
        .filter(|n| !anchored.contains(&comps.find(*n)))
        .filter(|n| seen.insert(n.0))
        .collect()
}

/// Stamp the pad-clamp pair on every floating injection node.
///
/// A diode goes up to a hidden rail at the board's highest source (at least
/// 0 V) and one down from a hidden rail at its lowest (at most 0 V). The node then sits one diode drop outside the rails at worst, as a real
/// pad does through its ESD structure, instead of at `I / gmin`. Returns how
/// many nodes were clamped. Idempotent: a clamped node reaches a source
/// through its diodes and is no longer floating, so it is safe to call after
/// every structural edit.
pub fn stamp_floating_clamps(circuit: &mut Circuit) -> usize {
    let nodes = floating_injection_nodes(circuit);
    // With no source to bound by there is no physical level to clamp to: the
    // sanity bound refuses such a solve instead.
    let Some((lo, hi)) = board_source_range(circuit).filter(|_| !nodes.is_empty()) else {
        return 0;
    };
    let (lo, hi) = (lo.min(0.0), hi.max(0.0));
    let rail = |circuit: &mut Circuit, tag: &str, volts: f64| -> NodeId {
        if volts == 0.0 {
            return NodeId::GROUND;
        }
        let name = format!("{CLAMP_RAIL_PREFIX}{tag}");
        let node = circuit.node(&format!("__clamp_{tag}"));
        if !circuit
            .devices
            .iter()
            .any(|d| matches!(d, Device::Vsource { name: n, .. } if *n == name))
        {
            circuit.add(Device::vsource(
                name,
                node,
                NodeId::GROUND,
                SourceKind::Dc(volts),
            ));
        }
        node
    };
    let rail_hi = rail(circuit, "hi", hi);
    let rail_lo = rail(circuit, "lo", lo);
    for node in &nodes {
        let name = circuit.node_name(*node).to_string();
        let model = DiodeModel::default();
        circuit.add(Device::Diode {
            name: format!("Dclamp_hi_{name}"),
            a: *node,
            k: rail_hi,
            model,
        });
        circuit.add(Device::Diode {
            name: format!("Dclamp_lo_{name}"),
            a: rail_lo,
            k: *node,
            model,
        });
    }
    nodes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GND: NodeId = NodeId::GROUND;

    fn dc(c: &mut Circuit, name: &str, node: NodeId, v: f64) {
        c.add(Device::vsource(name, node, GND, SourceKind::Dc(v)));
    }

    #[test]
    fn the_envelope_is_the_sources_plus_a_volt_each_side() {
        let mut c = Circuit::new();
        let (a, b) = (c.node("A"), c.node("B"));
        dc(&mut c, "V1", a, 5.0);
        dc(&mut c, "V2", b, -3.3);
        let env = Envelope::of(&c).expect("bounded");
        assert!((env.hi - 6.0).abs() < 1e-12 && (env.lo + 4.3).abs() < 1e-12);
        assert_eq!(env.first_outside(&[0.0, 5.9, -4.2]), None);
        assert_eq!(env.first_outside(&[0.0, 6.1]), Some((1, 6.1)));
        assert_eq!(env.first_outside(&[0.0, 1.0, -4.4]), Some((2, -4.4)));
    }

    #[test]
    fn a_board_that_can_step_up_has_no_envelope() {
        let mut c = Circuit::new();
        let (a, b) = (c.node("A"), c.node("B"));
        dc(&mut c, "V1", a, 5.0);
        c.add(Device::Inductor {
            name: "L1".into(),
            a,
            b,
            henries: 1e-6,
            ic: None,
        });
        assert_eq!(Envelope::of(&c), None);
    }

    #[test]
    fn a_supply_pin_nothing_holds_is_clamped_inside_the_rails() {
        let mut c = Circuit::new();
        let (vin, float) = (c.node("VIN"), c.node("FLOAT"));
        dc(&mut c, "Vin", vin, 3.3);
        c.add(Device::isource("Idraw", float, GND, SourceKind::Dc(1e-3)));
        assert_eq!(floating_injection_nodes(&c), vec![float]);
        assert_eq!(stamp_floating_clamps(&mut c), 1);
        assert!(floating_injection_nodes(&c).is_empty());
        assert_eq!(stamp_floating_clamps(&mut c), 0, "idempotent");
        // A held node is left alone.
        let mut held = Circuit::new();
        let n = held.node("N");
        dc(&mut held, "V", n, 3.3);
        held.add(Device::isource("I", n, GND, SourceKind::Dc(1e-3)));
        assert_eq!(stamp_floating_clamps(&mut held), 0);
    }

    #[test]
    fn a_current_source_that_is_not_a_draw_lifts_the_envelope() {
        let mut c = Circuit::new();
        let (a, b) = (c.node("A"), c.node("B"));
        dc(&mut c, "V1", a, 5.0);
        c.add(Device::isource("Idrh_G0", a, GND, SourceKind::Dc(1e-3)));
        assert!(Envelope::of(&c).is_some(), "a rail draw does not lift it");
        c.add(Device::isource("IIN", GND, b, SourceKind::Dc(2e-4)));
        assert_eq!(Envelope::of(&c), None, "a generator can drive any level");
    }

    #[test]
    fn a_comparator_output_widens_the_range() {
        let mut c = Circuit::new();
        let (a, out) = (c.node("A"), c.node("OUT"));
        dc(&mut c, "V1", a, 1.0);
        c.add(Device::Comparator {
            name: "K1".into(),
            out,
            inp: a,
            inn: GND,
            out_lo: 0.0,
            out_hi: 5.0,
            hysteresis: 0.0,
            tpd_s: 0.0,
            supply: hauksbee_ir::Supply::default(),
        });
        let env = Envelope::of(&c).expect("bounded");
        assert!((env.hi - 6.0).abs() < 1e-12, "{env:?}");
    }

    #[test]
    fn no_source_means_nothing_to_clamp_to() {
        let mut c = Circuit::new();
        let float = c.node("FLOAT");
        c.add(Device::isource("Idraw", float, GND, SourceKind::Dc(1e-3)));
        assert_eq!(stamp_floating_clamps(&mut c), 0);
    }
}
