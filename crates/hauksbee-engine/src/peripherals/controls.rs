//! Analog / contact controls: pushbutton, toggle switch, potentiometer, rotary
//! encoder, and a generic voltage/current stimulus.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/peripherals.md.
//!
//! Each control attaches to one or more nets and drives them through the same
//! stamped-source machinery the rest of the engine uses (an ideal `Vsource`
//! behind a series resistor, exactly the [`SupplyLeg`](crate::power_supply)
//! and [`PinDriver`](crate::drivers) pattern). Updating the control between
//! chunks is just mutating the source value or a switch resistance; MNA
//! resolves contention with whatever else is on the net.
//!
//! Connection by net name or connector ref+pin is resolved to a [`NodeId`] by
//! the binder before construction, so these types only ever see nodes.

use std::collections::HashMap;

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId, PwlPoint, SourceKind};

use super::{Peripheral, TickCtx};

/// Default contact resistance of a closed switch / pressed button (ohms).
pub const CONTACT_RON: f64 = 1.0;
/// Open contact resistance (ohms); effectively an open circuit.
pub const CONTACT_ROFF: f64 = 1e9;

/// Stamp a controllable resistor between two nodes and return its device id.
fn stamp_contact(circuit: &mut Circuit, a: NodeId, b: NodeId, tag: &str, ohms: f64) -> DeviceId {
    circuit.add(Device::Resistor {
        name: format!("Rsw_{tag}"),
        a,
        b,
        ohms,
        tc1: None,
    })
}

fn set_resistor(circuit: &mut Circuit, id: DeviceId, value: f64) {
    if let Some(Device::Resistor { ohms, .. }) = circuit.devices.get_mut(id.0 as usize) {
        *ohms = value;
    }
}

fn set_vsource(circuit: &mut Circuit, id: DeviceId, kind: SourceKind) {
    if let Some(Device::Vsource { kind: k, .. }) = circuit.devices.get_mut(id.0 as usize) {
        *k = kind;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Pushbutton (momentary) with optional contact-bounce model
// ─────────────────────────────────────────────────────────────────────────────

/// A momentary pushbutton wiring `net` to a reference rail (`to`, default
/// ground) through a contact resistance. Released = open; pressed = closed.
///
/// With `bounce_ms > 0`, a press or release chatters open/closed for
/// `bounce_ms` before settling, modelling real switch bounce so debounce
/// firmware can be exercised.
pub struct Pushbutton {
    id: String,
    contact: DeviceId,
    ron: f64,
    roff: f64,
    pressed: bool,
    bounce_s: f64,
    /// Set when a press level change is requested; consumed on the next
    /// pre_solve to start the bounce window at the correct sim time.
    pending: Option<bool>,
    /// Sim time the current transition began, while bouncing.
    transition_start: Option<f64>,
    target_pressed: bool,
}

impl Pushbutton {
    /// Stamp a button between `net` and `to` (`NodeId::GROUND` for a
    /// pull-to-ground button). `bounce_ms` of 0 disables the bounce model.
    pub fn new(circuit: &mut Circuit, id: &str, net: NodeId, to: NodeId, bounce_ms: f64) -> Self {
        let contact = stamp_contact(circuit, net, to, id, CONTACT_ROFF);
        Pushbutton {
            id: id.to_string(),
            contact,
            ron: CONTACT_RON,
            roff: CONTACT_ROFF,
            pressed: false,
            bounce_s: (bounce_ms / 1000.0).max(0.0),
            pending: None,
            transition_start: None,
            target_pressed: false,
        }
    }

    fn resistance_now(&self, t: f64) -> f64 {
        match self.transition_start {
            None => {
                if self.pressed {
                    self.ron
                } else {
                    self.roff
                }
            }
            Some(start) => {
                let elapsed = t - start;
                if elapsed >= self.bounce_s {
                    if self.target_pressed {
                        self.ron
                    } else {
                        self.roff
                    }
                } else {
                    // ~5 chatter cycles across the bounce window.
                    let phase = (elapsed / self.bounce_s * 10.0).floor() as i64;
                    if phase % 2 == 0 {
                        self.ron
                    } else {
                        self.roff
                    }
                }
            }
        }
    }
}

impl Peripheral for Pushbutton {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &'static str {
        "pushbutton"
    }

    fn pre_solve(&mut self, ctx: &mut TickCtx) {
        // Start a pending transition at this chunk's time.
        if let Some(want) = self.pending.take() {
            if self.bounce_s > 0.0 {
                self.target_pressed = want;
                self.transition_start = Some(ctx.t);
            } else {
                self.pressed = want;
                self.target_pressed = want;
                self.transition_start = None;
            }
        }
        // Settle when the bounce window elapses.
        if let Some(start) = self.transition_start {
            if ctx.t - start >= self.bounce_s {
                self.pressed = self.target_pressed;
                self.transition_start = None;
            }
        }
        let r = self.resistance_now(ctx.t);
        set_resistor(ctx.circuit, self.contact, r);
    }

    fn set_value(&mut self, value: f64) {
        let want = value >= 0.5;
        if want != self.pressed || self.pending.is_some() {
            self.pending = Some(want);
        }
    }

    fn state(&self) -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("pressed".into(), if self.pressed { 1.0 } else { 0.0 });
        m
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Toggle switch (latching)
// ─────────────────────────────────────────────────────────────────────────────

/// A latching SPST toggle wiring `net` to `to` through a contact. Unlike the
/// button it holds its state and never bounces (idealised as debounced).
pub struct ToggleSwitch {
    id: String,
    contact: DeviceId,
    closed: bool,
}

impl ToggleSwitch {
    pub fn new(circuit: &mut Circuit, id: &str, net: NodeId, to: NodeId, closed: bool) -> Self {
        let r = if closed { CONTACT_RON } else { CONTACT_ROFF };
        let contact = stamp_contact(circuit, net, to, id, r);
        ToggleSwitch {
            id: id.to_string(),
            contact,
            closed,
        }
    }
}

impl Peripheral for ToggleSwitch {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &'static str {
        "toggle"
    }

    fn pre_solve(&mut self, ctx: &mut TickCtx) {
        let r = if self.closed {
            CONTACT_RON
        } else {
            CONTACT_ROFF
        };
        set_resistor(ctx.circuit, self.contact, r);
    }

    fn set_value(&mut self, value: f64) {
        self.closed = value >= 0.5;
    }

    fn state(&self) -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("closed".into(), if self.closed { 1.0 } else { 0.0 });
        m
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Potentiometer (three-terminal, wiper position 0..1)
// ─────────────────────────────────────────────────────────────────────────────

/// A three-terminal potentiometer: terminals `a` and `b`, wiper `w`. The total
/// track resistance `r_total` is split between the a-w and w-b legs by the
/// wiper position `pos` in 0..1 (0 = wiper at `a`, 1 = wiper at `b`). Tied as a
/// voltage divider (a→rail, b→ground) it gives an analog input the firmware can
/// read on the wiper.
pub struct Potentiometer {
    id: String,
    r_aw: DeviceId,
    r_wb: DeviceId,
    r_total: f64,
    pos: f64,
}

impl Potentiometer {
    pub fn new(
        circuit: &mut Circuit,
        id: &str,
        a: NodeId,
        w: NodeId,
        b: NodeId,
        r_total: f64,
        pos: f64,
    ) -> Self {
        let pos = pos.clamp(0.0, 1.0);
        let r_total = r_total.max(1.0);
        // Avoid a hard zero-ohm leg (singular); floor each at 1 mΩ.
        let r_aw = circuit.add(Device::Resistor {
            name: format!("Rpot_{id}_aw"),
            a,
            b: w,
            ohms: (r_total * pos).max(1e-3),
            tc1: None,
        });
        let r_wb = circuit.add(Device::Resistor {
            name: format!("Rpot_{id}_wb"),
            a: w,
            b,
            ohms: (r_total * (1.0 - pos)).max(1e-3),
            tc1: None,
        });
        Potentiometer {
            id: id.to_string(),
            r_aw,
            r_wb,
            r_total,
            pos,
        }
    }

    fn apply(&self, circuit: &mut Circuit) {
        set_resistor(circuit, self.r_aw, (self.r_total * self.pos).max(1e-3));
        set_resistor(
            circuit,
            self.r_wb,
            (self.r_total * (1.0 - self.pos)).max(1e-3),
        );
    }
}

impl Peripheral for Potentiometer {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &'static str {
        "potentiometer"
    }

    fn pre_solve(&mut self, ctx: &mut TickCtx) {
        self.apply(ctx.circuit);
    }

    fn set_value(&mut self, value: f64) {
        self.pos = value.clamp(0.0, 1.0);
    }

    fn state(&self) -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("position".into(), self.pos);
        m
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Rotary encoder (quadrature A/B from a position stream)
// ─────────────────────────────────────────────────────────────────────────────

/// An incremental rotary encoder producing quadrature A/B outputs on two nets,
/// driven from a commanded angular position (in detents). Setting a new
/// position advances the Gray-code state machine the right number of steps,
/// flipping A then B (or B then A) per step direction. Each output is a
/// stamped Thevenin driver pulled to `vhigh` / 0.
pub struct Encoder {
    id: String,
    drv_a: DeviceId,
    drv_b: DeviceId,
    vhigh: f64,
    /// Current position in detents (integer steps).
    detents: i64,
    /// Current quadrature phase 0..3 (Gray code: 00,01,11,10).
    phase: u8,
    a_node: NodeId,
    b_node: NodeId,
}

const GRAY: [(bool, bool); 4] = [(false, false), (false, true), (true, true), (true, false)];

impl Encoder {
    pub fn new(circuit: &mut Circuit, id: &str, a: NodeId, b: NodeId, vhigh: f64) -> Self {
        // Each output is an ideal source behind a small series resistor.
        let drv_a = stamp_driver(circuit, a, &format!("{id}_A"), 0.0);
        let drv_b = stamp_driver(circuit, b, &format!("{id}_B"), 0.0);
        Encoder {
            id: id.to_string(),
            drv_a,
            drv_b,
            vhigh,
            detents: 0,
            phase: 0,
            a_node: a,
            b_node: b,
        }
    }

    fn drive_phase(&self, circuit: &mut Circuit) {
        let (a, b) = GRAY[self.phase as usize];
        set_vsource(
            circuit,
            self.drv_a,
            SourceKind::Dc(if a { self.vhigh } else { 0.0 }),
        );
        set_vsource(
            circuit,
            self.drv_b,
            SourceKind::Dc(if b { self.vhigh } else { 0.0 }),
        );
    }
}

impl Peripheral for Encoder {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &'static str {
        "encoder"
    }

    fn pre_solve(&mut self, ctx: &mut TickCtx) {
        self.drive_phase(ctx.circuit);
    }

    fn set_value(&mut self, value: f64) {
        // value is the target absolute position in detents. Step toward it one
        // quadrature edge per detent so A/B form a valid quadrature sequence.
        let target = value.round() as i64;
        while self.detents != target {
            if target > self.detents {
                self.phase = (self.phase + 1) % 4;
                self.detents += 1;
            } else {
                self.phase = (self.phase + 3) % 4;
                self.detents -= 1;
            }
        }
    }

    fn state(&self) -> HashMap<String, f64> {
        let (a, b) = GRAY[self.phase as usize];
        let mut m = HashMap::new();
        m.insert("detents".into(), self.detents as f64);
        m.insert("a".into(), if a { 1.0 } else { 0.0 });
        m.insert("b".into(), if b { 1.0 } else { 0.0 });
        let _ = (self.a_node, self.b_node);
        m
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Generic voltage / current stimulus (DC, sine, PWL, noise)
// ─────────────────────────────────────────────────────────────────────────────

/// Which source flavour a [`Stimulus`] stamps.
#[derive(Debug, Clone)]
pub enum StimulusKind {
    /// Pass a `SourceKind` waveform straight through (DC / Sin / Pulse / Pwl).
    Wave(SourceKind),
    /// Band-limited-ish white noise: `offset + amplitude * U(-1,1)`, reseeded
    /// deterministically each chunk from a counter.
    Noise {
        offset: f64,
        amplitude: f64,
        seed: u64,
    },
}

/// A generic stimulus driving one net. With `is_current = false` it is a
/// voltage source behind a small series resistor (so its branch current is
/// measurable and it never hard-shorts the net); with `is_current = true` it is
/// an ideal current source injecting into the net.
pub struct Stimulus {
    id: String,
    device: DeviceId,
    kind: StimulusKind,
    is_current: bool,
    counter: u64,
    last_value: f64,
}

impl Stimulus {
    /// Stamp a voltage stimulus on `net` (behind a 50 Ω series resistor).
    pub fn voltage(circuit: &mut Circuit, id: &str, net: NodeId, kind: StimulusKind) -> Self {
        let device = stamp_driver(circuit, net, id, kind.value_at(0.0));
        Stimulus {
            id: id.to_string(),
            device,
            kind,
            is_current: false,
            counter: 0,
            last_value: 0.0,
        }
    }

    /// Stamp a current stimulus injecting into `net` from ground.
    pub fn current(circuit: &mut Circuit, id: &str, net: NodeId, kind: StimulusKind) -> Self {
        let device = circuit.add(Device::Isource {
            name: format!("Istim_{id}"),
            p: NodeId::GROUND,
            n: net,
            kind: SourceKind::Dc(kind.value_at(0.0)),
        });
        Stimulus {
            id: id.to_string(),
            device,
            kind,
            is_current: true,
            counter: 0,
            last_value: 0.0,
        }
    }
}

impl StimulusKind {
    fn value_at(&self, t: f64) -> f64 {
        match self {
            StimulusKind::Wave(k) => k.eval(t),
            StimulusKind::Noise { offset, .. } => *offset,
        }
    }
}

/// Deterministic uniform in [-1, 1] from a counter (splitmix64).
fn noise_sample(seed: u64, counter: u64) -> f64 {
    let mut x = seed ^ counter.wrapping_mul(0x9E3779B97F4A7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    x ^= x >> 31;
    // Map to [-1, 1).
    (x as f64 / u64::MAX as f64) * 2.0 - 1.0
}

impl Peripheral for Stimulus {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &'static str {
        "stimulus"
    }

    fn pre_solve(&mut self, ctx: &mut TickCtx) {
        let v = match &self.kind {
            StimulusKind::Wave(k) => k.eval(ctx.t),
            StimulusKind::Noise {
                offset,
                amplitude,
                seed,
            } => {
                self.counter = self.counter.wrapping_add(1);
                offset + amplitude * noise_sample(*seed, self.counter)
            }
        };
        self.last_value = v;
        if self.is_current {
            if let Some(Device::Isource { kind, .. }) =
                ctx.circuit.devices.get_mut(self.device.0 as usize)
            {
                *kind = SourceKind::Dc(v);
            }
        } else {
            set_vsource(ctx.circuit, self.device, SourceKind::Dc(v));
        }
    }

    fn set_value(&mut self, value: f64) {
        // Live override: shift the offset (DC level) to `value`.
        self.kind = match std::mem::replace(&mut self.kind, StimulusKind::Wave(SourceKind::Dc(0.0)))
        {
            StimulusKind::Wave(_) => StimulusKind::Wave(SourceKind::Dc(value)),
            StimulusKind::Noise {
                amplitude, seed, ..
            } => StimulusKind::Noise {
                offset: value,
                amplitude,
                seed,
            },
        };
    }

    fn state(&self) -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("value".into(), self.last_value);
        m
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Stamp an ideal `Vsource` behind a 50 Ω series resistor on `net`, returning
/// the `Vsource` device id (the controllable handle). Mirrors
/// [`PinDriver`](crate::drivers) but as a free helper so the controls can own
/// the source directly.
fn stamp_driver(circuit: &mut Circuit, net: NodeId, tag: &str, v0: f64) -> DeviceId {
    let drv_node = circuit.node(&format!("__ctrl_{tag}"));
    let vsource = circuit.add(Device::Vsource {
        name: format!("Vctrl_{tag}"),
        p: drv_node,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(v0),
    });
    circuit.add(Device::Resistor {
        name: format!("Rctrl_{tag}"),
        a: drv_node,
        b: net,
        ohms: 50.0,
        tc1: None,
    });
    vsource
}

/// Build a [`SourceKind`] PWL from `(t_s, value)` points.
pub fn pwl(points: Vec<(f64, f64)>) -> SourceKind {
    SourceKind::Pwl(points.into_iter().map(|(t, v)| PwlPoint { t, v }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushbutton_open_closed() {
        let mut c = Circuit::new();
        let net = c.node("BTN");
        let mut b = Pushbutton::new(&mut c, "BTN1", net, NodeId::GROUND, 0.0);
        let volts = vec![0.0; c.node_count()];
        let mut ctx = TickCtx {
            circuit: &mut c,
            node_volts: &volts,
            t: 0.0,
            dt: 1e-4,
        };
        b.pre_solve(&mut ctx);
        // Released = open (high resistance).
        if let Device::Resistor { ohms, .. } = &c.devices[b.contact.0 as usize] {
            assert!(*ohms > 1e6, "released button should be open");
        } else {
            panic!("contact not a resistor");
        }
        b.set_value(1.0);
        let volts = vec![0.0; c.node_count()];
        let mut ctx = TickCtx {
            circuit: &mut c,
            node_volts: &volts,
            t: 1e-4,
            dt: 1e-4,
        };
        b.pre_solve(&mut ctx);
        if let Device::Resistor { ohms, .. } = &c.devices[b.contact.0 as usize] {
            assert!(*ohms < 10.0, "pressed button should be closed, got {ohms}");
        }
    }

    #[test]
    fn potentiometer_splits_resistance() {
        let mut c = Circuit::new();
        let a = c.node("A");
        let w = c.node("W");
        let b = c.node("B");
        let pot = Potentiometer::new(&mut c, "POT1", a, w, b, 10_000.0, 0.25);
        pot.apply(&mut c);
        let r_aw = if let Device::Resistor { ohms, .. } = &c.devices[pot.r_aw.0 as usize] {
            *ohms
        } else {
            panic!()
        };
        let r_wb = if let Device::Resistor { ohms, .. } = &c.devices[pot.r_wb.0 as usize] {
            *ohms
        } else {
            panic!()
        };
        assert!((r_aw - 2500.0).abs() < 1.0, "a-w leg at pos 0.25 = {r_aw}");
        assert!((r_wb - 7500.0).abs() < 1.0, "w-b leg at pos 0.25 = {r_wb}");
    }

    #[test]
    fn encoder_quadrature_sequence() {
        let mut c = Circuit::new();
        let a = c.node("ENC_A");
        let b = c.node("ENC_B");
        let mut enc = Encoder::new(&mut c, "ENC1", a, b, 5.0);
        // Step forward 4 detents -> full Gray cycle back to phase 0.
        let mut seq = Vec::new();
        for step in 1..=4 {
            enc.set_value(step as f64);
            seq.push(GRAY[enc.phase as usize]);
        }
        assert_eq!(
            seq,
            vec![(false, true), (true, true), (true, false), (false, false)]
        );
    }

    #[test]
    fn stimulus_sine_evaluates() {
        let mut c = Circuit::new();
        let net = c.node("SIG");
        let mut s = Stimulus::voltage(
            &mut c,
            "SIG1",
            net,
            StimulusKind::Wave(SourceKind::Sin {
                offset: 2.5,
                amplitude: 2.5,
                freq: 1000.0,
                delay: 0.0,
                theta: 0.0,
                phase: 0.0,
            }),
        );
        let volts = vec![0.0; c.node_count()];
        let mut ctx = TickCtx {
            circuit: &mut c,
            node_volts: &volts,
            t: 250e-6,
            dt: 1e-5,
        };
        s.pre_solve(&mut ctx);
        // Quarter period of a 1 kHz sine -> peak ~5.0 V.
        assert!(
            (s.last_value - 5.0).abs() < 0.1,
            "sine peak {}",
            s.last_value
        );
    }
}
