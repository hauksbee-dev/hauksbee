//! Controllable Thevenin pin drivers.
//! (the digital-domain essay covers this module too).
//!
//! A digital IC output or an MCU GPIO pin drives an analog net not by clamping
//! it, but through a Thevenin equivalent: an ideal voltage source behind an
//! output resistance `ro`. We stamp this into the [`Circuit`] as a hidden
//! driver node, a `Vsource` from that node to ground, and a `Resistor` from
//! the driver node to the real net node. Updating the pin level is then just
//! mutating the `Vsource`'s value between solver chunks, cheap, and contention
//! between multiple drivers on one net resolves naturally through the resistor
//! network in MNA.

use hauksbee_ir::{BOutput, Circuit, Device, DeviceId, NodeId, SourceKind};

/// Default output resistance for a logic driver (ohms).
pub const DEFAULT_RO: f64 = 50.0;

/// Forward conductance (S) of the one-way element in a sink-only driver.
const SINK_ONLY_S: f64 = 1e3;

/// A handle to one Thevenin driver leg stamped into a circuit.
#[derive(Debug, Clone)]
pub struct PinDriver {
    /// Index into `Circuit.devices` of the controllable `Vsource`.
    pub vsource: DeviceId,
    /// The net node this driver pushes onto.
    pub net: NodeId,
    /// Whether the driver is currently enabled (high-impedance when false).
    pub enabled: bool,
    /// High-impedance output resistance when disabled (e.g. tri-stated).
    pub roff: f64,
    /// Index into `Circuit.devices` of the series resistor (so we can retune ro).
    pub resistor: DeviceId,
    /// The active (enabled) output resistance.
    pub ron: f64,
    /// The legs that draw this driver's output current from its part's supply
    /// pins (`None` until [`PinDriver::route_supply`]: the current then closes
    /// through ground and the part's supply never sees the load).
    pub supply: Option<DriverSupply>,
}

/// The two `Isource` legs that move a [`PinDriver`]'s output current onto its
/// part's supply pins, each from the pin to ground (the Thevenin source's own
/// return), so ground nets nothing.
///
/// A driver sourcing `I` draws it from `hi` (`I_hi = +I`); one sinking it
/// returns it into `lo` (`I_lo = -I`, current leaving `lo` negative), and
/// `lo` is absent when the part's negative pin is ground. A sink-only
/// (open-collector) driver has no `hi`: nothing is drawn from a supply. The scheduler sets
/// both once per chunk from the mean current the driver delivered over the
/// chunk before ([`DriverSupply::set`]), never from the live branch: read
/// live, a load capacitor's `C*dV/dt` Newton residual would be mirrored onto
/// the rail that feeds an upstream converter.
#[derive(Debug, Clone, Copy)]
pub struct DriverSupply {
    /// `Isource` from the positive supply pin to ground, when the part has one.
    pub hi: Option<DeviceId>,
    /// `Isource` from the negative supply pin to ground, when it is not ground.
    pub lo: Option<DeviceId>,
}

impl DriverSupply {
    /// Set the legs: `sourced` (>= 0) leaves `hi`, `sunk` (<= 0) leaves `lo`.
    pub fn set(self, circuit: &mut Circuit, sourced: f64, sunk: f64) {
        for (id, amps) in [(self.hi, sourced), (self.lo, sunk)] {
            let dev = id.and_then(|id| circuit.devices.get_mut(id.0 as usize));
            if let Some(Device::Isource { kind, .. }) = dev {
                *kind = SourceKind::Dc(amps);
            }
        }
    }
}

impl PinDriver {
    /// Stamp a fresh Thevenin driver onto `net` in `circuit` and return its
    /// handle. `tag` names the hidden driver node and devices for diagnostics.
    pub fn stamp(circuit: &mut Circuit, net: NodeId, net_name: &str, tag: &str, ro: f64) -> Self {
        let drv_node_name = format!("__drv_{tag}_{net_name}");
        let drv = circuit.node(&drv_node_name);
        let vsource = circuit.add(Device::vsource(
            format!("Vdrv_{tag}"),
            drv,
            NodeId::GROUND,
            SourceKind::Dc(0.0),
        ));
        let resistor = circuit.add(Device::resistor(format!("Rdrv_{tag}"), drv, net, ro));
        Self {
            vsource,
            net,
            enabled: true,
            roff: 1e9,
            resistor,
            ron: ro,
            supply: None,
        }
    }

    /// Stamp a SINK-ONLY driver (an open collector or open drain) onto `net`:
    /// the [`PinDriver::stamp`] Thevenin leg, then a one-way element between
    /// the series resistor and the net that conducts only from the net toward
    /// the source. The output pulls the net down to its low level through `ro`
    /// and, whatever the low level, never sources current into a load that sits
    /// below it (a pull-down), as a bare Thevenin source would.
    ///
    /// # Panics
    /// Never: the element's expression is a fixed, valid string.
    pub fn stamp_sink_only(
        circuit: &mut Circuit,
        net: NodeId,
        net_name: &str,
        tag: &str,
        ro: f64,
    ) -> Self {
        let drv = circuit.node(&format!("__drv_{tag}_{net_name}"));
        let mid = circuit.node(&format!("__drvm_{tag}_{net_name}"));
        let vsource = circuit.add(Device::vsource(
            format!("Vdrv_{tag}"),
            drv,
            NodeId::GROUND,
            SourceKind::Dc(0.0),
        ));
        let resistor = circuit.add(Device::resistor(format!("Rdrv_{tag}"), drv, mid, ro));
        // `max` keeps the element one-directional; its conductance is the
        // series resistance's 1/1000th, so the line is `ro` to within 0.1%.
        let expr = format!("{SINK_ONLY_S:e}*max(__d0-__d1,0.0)");
        let diode = Device::behavioral(
            format!("Bdrv_{tag}"),
            (net, mid),
            BOutput::Current,
            &expr,
            &[net, mid],
        );
        circuit.add(diode.expect("sink-only element expression"));
        Self {
            vsource,
            net,
            enabled: true,
            roff: 1e9,
            resistor,
            ron: ro,
            supply: None,
        }
    }

    /// Draw this driver's output current from supply pins `hi` and `lo`
    /// instead of ground. A ground `hi` leaves the driver as it was; a ground
    /// `lo` is the return the current already takes.
    pub fn route_supply(&mut self, circuit: &mut Circuit, tag: &str, hi: NodeId, lo: NodeId) {
        if hi.is_ground() {
            return;
        }
        let leg = |circuit: &mut Circuit, name: &str, pin: NodeId| {
            let name = format!("{name}_{tag}");
            circuit.add(Device::isource(
                name,
                pin,
                NodeId::GROUND,
                SourceKind::Dc(0.0),
            ))
        };
        self.supply = Some(DriverSupply {
            hi: Some(leg(circuit, "Idrh", hi)),
            lo: (!lo.is_ground()).then(|| leg(circuit, "Idrl", lo)),
        });
    }

    /// Return this sink-only driver's current into supply pin `lo` and draw
    /// nothing from a positive pin. A ground `lo` leaves the driver as it was.
    pub fn route_return(&mut self, circuit: &mut Circuit, tag: &str, lo: NodeId) {
        if lo.is_ground() {
            return;
        }
        let name = format!("Idrl_{tag}");
        let leg = Device::isource(name, lo, NodeId::GROUND, SourceKind::Dc(0.0));
        self.supply = Some(DriverSupply {
            hi: None,
            lo: Some(circuit.add(leg)),
        });
    }

    /// Set the driver's target voltage. No-op while disabled.
    pub fn set_volts(&self, circuit: &mut Circuit, volts: f64) {
        if !self.enabled {
            return;
        }
        if let Some(Device::Vsource { kind, .. }) = circuit.devices.get_mut(self.vsource.0 as usize)
        {
            *kind = SourceKind::Dc(volts);
        }
    }

    /// Retune the active output resistance (a drive-strength corner). A
    /// tri-stated leg keeps `roff` and takes the new `ron` when re-enabled.
    pub fn set_ron(&mut self, circuit: &mut Circuit, ron: f64) {
        self.ron = ron;
        if !self.enabled {
            return;
        }
        if let Some(Device::Resistor { ohms, .. }) =
            circuit.devices.get_mut(self.resistor.0 as usize)
        {
            *ohms = ron;
        }
    }

    /// Enable or tri-state the driver by swapping the series resistance between
    /// `ron` and `roff`. A tri-stated leg presents a near-open to the net.
    pub fn set_enabled(&mut self, circuit: &mut Circuit, enabled: bool) {
        if enabled == self.enabled {
            return;
        }
        self.enabled = enabled;
        let ohms = if enabled { self.ron } else { self.roff };
        if let Some(Device::Resistor { ohms: r, .. }) =
            circuit.devices.get_mut(self.resistor.0 as usize)
        {
            *r = ohms;
        }
    }
}
