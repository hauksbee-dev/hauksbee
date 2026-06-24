//! Controllable Thevenin pin drivers.
//!
//! A digital IC output or an MCU GPIO pin drives an analog net not by clamping
//! it, but through a Thevenin equivalent: an ideal voltage source behind an
//! output resistance `ro`. We stamp this into the [`Circuit`] as a hidden
//! driver node, a `Vsource` from that node to ground, and a `Resistor` from
//! the driver node to the real net node. Updating the pin level is then just
//! mutating the `Vsource`'s value between solver chunks — cheap, and contention
//! between multiple drivers on one net resolves naturally through the resistor
//! network in MNA.

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId, SourceKind};

/// Default output resistance for a logic driver (ohms).
pub const DEFAULT_RO: f64 = 50.0;

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
}

impl PinDriver {
    /// Stamp a fresh Thevenin driver onto `net` in `circuit` and return its
    /// handle. `tag` names the hidden driver node and devices for diagnostics.
    pub fn stamp(circuit: &mut Circuit, net: NodeId, net_name: &str, tag: &str, ro: f64) -> Self {
        let drv_node_name = format!("__drv_{tag}_{net_name}");
        let drv = circuit.node(&drv_node_name);
        let vsource = circuit.add(Device::Vsource {
            name: format!("Vdrv_{tag}"),
            p: drv,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(0.0),
        });
        let resistor = circuit.add(Device::Resistor {
            name: format!("Rdrv_{tag}"),
            a: drv,
            b: net,
            ohms: ro,
            tc1: None,
        });
        PinDriver {
            vsource,
            net,
            enabled: true,
            roff: 1e9,
            resistor,
            ron: ro,
        }
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
