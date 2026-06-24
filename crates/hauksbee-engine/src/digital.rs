//! Event-driven behavioral digital components.
//!
//! These are NOT solved in MNA. Each step the scheduler:
//!   1. samples the input net voltages and converts them to logic levels with
//!      the part's `vih`/`vil` thresholds (with hysteresis between them);
//!   2. lets the component process clock/latch edges and update its register;
//!   3. writes the component's output logic levels back onto the analog nets
//!      through Thevenin [`PinDriver`]s (`voh`/`vol`, `ro`).
//!
//! Two families are modelled in detail because they dominate the corpus:
//! 74HC595 (serial-in, parallel-out) and 74HC165 (parallel-in, serial-out).
//! Anything else with a `digital`/`shift_register` kind falls back to a
//! transparent buffer that mirrors recognised input roles onto output roles.

use std::collections::HashMap;

use hauksbee_ir::{Circuit, NodeId};
use hauksbee_models::ModelEntry;

use crate::drivers::{PinDriver, DEFAULT_RO};

/// Logic thresholds and drive levels pulled from a model entry's params.
#[derive(Debug, Clone, Copy)]
pub struct LogicLevels {
    pub voh: f64,
    pub vol: f64,
    pub vih: f64,
    pub vil: f64,
    pub ro: f64,
}

impl LogicLevels {
    pub fn from_params(m: &ModelEntry) -> Self {
        LogicLevels {
            voh: m.params.get_f64("voh").unwrap_or(4.4),
            vol: m.params.get_f64("vol").unwrap_or(0.1),
            vih: m.params.get_f64("vih").unwrap_or(3.15),
            vil: m.params.get_f64("vil").unwrap_or(1.35),
            ro: m.params.get_f64("ro").unwrap_or(DEFAULT_RO),
        }
    }

    /// Convert a sampled voltage to a logic level using hysteresis. `prev` is
    /// the last decided level for the pin (true=high); between vil and vih the
    /// pin holds its previous state.
    pub fn decide(&self, v: f64, prev: bool) -> bool {
        if v >= self.vih {
            true
        } else if v <= self.vil {
            false
        } else {
            prev
        }
    }

    pub fn drive_volts(&self, high: bool) -> f64 {
        if high {
            self.voh
        } else {
            self.vol
        }
    }
}

/// The behaviour family of a digital component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigitalKind {
    Hc595,
    Hc165,
    /// Generic transparent buffer / unmodelled digital block.
    Buffer,
}

/// One bound digital component: its pin→net wiring, drivers, and state.
pub struct DigitalComponent {
    pub reference: String,
    pub kind: DigitalKind,
    pub levels: LogicLevels,
    /// Role name -> net node it is wired to (only connected roles present).
    pub roles: HashMap<String, NodeId>,
    /// Output drivers keyed by role name.
    pub drivers: HashMap<String, PinDriver>,
    /// Previous decided input levels keyed by role.
    pub input_state: HashMap<String, bool>,
    /// Shift register contents (bit 0 = first stage).
    pub shift_reg: Vec<bool>,
    /// Latched / output register contents.
    pub out_reg: Vec<bool>,
    pub bits: usize,
    /// Edge-detector memory for the clock and latch lines.
    prev_srclk: bool,
    prev_rclk: bool,
    prev_clk: bool,
    prev_pl: bool,
}

impl DigitalComponent {
    /// Build a digital component from its model entry and a role→node map. The
    /// caller has already stamped output [`PinDriver`]s and passes them in.
    pub fn new(
        reference: String,
        model: &ModelEntry,
        roles: HashMap<String, NodeId>,
        drivers: HashMap<String, PinDriver>,
    ) -> Self {
        let kind = classify(model);
        let bits = model
            .params
            .get_f64("bits")
            .map(|b| b as usize)
            .unwrap_or(8);
        DigitalComponent {
            reference,
            kind,
            levels: LogicLevels::from_params(model),
            roles,
            drivers,
            input_state: HashMap::new(),
            shift_reg: vec![false; bits],
            out_reg: vec![false; bits],
            bits,
            prev_srclk: false,
            prev_rclk: false,
            prev_clk: false,
            prev_pl: false,
        }
    }

    /// Sample an input role's net voltage and decide its logic level. Roles not
    /// wired to a node read as low.
    fn sample(&mut self, role: &str, node_v: &dyn Fn(NodeId) -> f64) -> bool {
        let prev = self.input_state.get(role).copied().unwrap_or(false);
        let level = match self.roles.get(role) {
            Some(&n) => self.levels.decide(node_v(n), prev),
            None => false,
        };
        self.input_state.insert(role.to_string(), level);
        level
    }

    /// Process one scheduler tick: read inputs, update register, drive outputs.
    pub fn tick(&mut self, circuit: &mut Circuit, node_v: &dyn Fn(NodeId) -> f64) {
        match self.kind {
            DigitalKind::Hc595 => self.tick_595(node_v),
            DigitalKind::Hc165 => self.tick_165(node_v),
            DigitalKind::Buffer => self.tick_buffer(node_v),
        }
        self.drive_outputs(circuit);
    }

    fn tick_595(&mut self, node_v: &dyn Fn(NodeId) -> f64) {
        let ser = self.sample("ser", node_v);
        let srclk = self.sample("srclk", node_v);
        let rclk = self.sample("rclk", node_v);
        // SRCLR_n active-low clears the shift register.
        let srclr = if self.roles.contains_key("srclr_n") {
            self.sample("srclr_n", node_v)
        } else {
            true
        };
        if !srclr {
            for b in self.shift_reg.iter_mut() {
                *b = false;
            }
        }
        // Rising edge of SRCLK shifts ser into stage 0; stages move up.
        if srclk && !self.prev_srclk {
            for i in (1..self.bits).rev() {
                self.shift_reg[i] = self.shift_reg[i - 1];
            }
            self.shift_reg[0] = ser;
        }
        // Rising edge of RCLK latches shift register into the output register.
        if rclk && !self.prev_rclk {
            self.out_reg.copy_from_slice(&self.shift_reg);
        }
        self.prev_srclk = srclk;
        self.prev_rclk = rclk;
    }

    fn tick_165(&mut self, node_v: &dyn Fn(NodeId) -> f64) {
        // PL_n (active low) parallel-loads inputs A..H into the register.
        let pl = self.sample("pl_n", node_v);
        let clk = self.sample("clk", node_v);
        let clk_inh = if self.roles.contains_key("clk_inh") {
            self.sample("clk_inh", node_v)
        } else {
            false
        };
        // Parallel-load on PL_n falling edge / while low.
        if !pl {
            let parallel = ["a", "b", "c", "d", "e", "f", "g", "h"];
            for (i, role) in parallel.iter().enumerate().take(self.bits) {
                self.shift_reg[i] = self.sample(role, node_v);
            }
        } else if clk && !self.prev_clk && !clk_inh {
            // Rising edge shifts toward QH (stage bits-1 is QH output).
            for i in 0..self.bits - 1 {
                self.shift_reg[i] = self.shift_reg[i + 1];
            }
            // New data shifted in at SER (or 0).
            self.shift_reg[self.bits - 1] = self.sample("ser", node_v);
        }
        self.prev_pl = pl;
        self.prev_clk = clk;
        self.out_reg.copy_from_slice(&self.shift_reg);
    }

    fn tick_buffer(&mut self, node_v: &dyn Fn(NodeId) -> f64) {
        // Transparent: copy any sampled "a*" input role onto matching "y*"
        // output role (74HCxx buffer/gate naming), else hold.
        let roles: Vec<String> = self.roles.keys().cloned().collect();
        for role in roles {
            if let Some(idx) = role.strip_prefix('a') {
                let v = self.sample(&role, node_v);
                let out = format!("y{idx}");
                self.input_state.insert(out, v);
            }
        }
    }

    /// Push the current output-register / decided output levels onto drivers.
    fn drive_outputs(&mut self, circuit: &mut Circuit) {
        match self.kind {
            DigitalKind::Hc595 => {
                // qa..qh map to out_reg[0..8]; qa is stage 0.
                let names = ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh"];
                for (i, name) in names.iter().enumerate().take(self.bits) {
                    if let Some(drv) = self.drivers.get(*name) {
                        drv.set_volts(circuit, self.levels.drive_volts(self.out_reg[i]));
                    }
                }
                // Serial out = last stage.
                if let Some(drv) = self.drivers.get("qh_serial") {
                    drv.set_volts(
                        circuit,
                        self.levels.drive_volts(self.shift_reg[self.bits - 1]),
                    );
                }
            }
            DigitalKind::Hc165 => {
                // QH = last stage; QH_n is its complement.
                let qh = self.shift_reg[self.bits - 1];
                if let Some(drv) = self.drivers.get("qh") {
                    drv.set_volts(circuit, self.levels.drive_volts(qh));
                }
                if let Some(drv) = self.drivers.get("qh_n") {
                    drv.set_volts(circuit, self.levels.drive_volts(!qh));
                }
            }
            DigitalKind::Buffer => {
                let outs: Vec<(String, bool)> = self
                    .drivers
                    .keys()
                    .filter_map(|k| self.input_state.get(k).map(|v| (k.clone(), *v)))
                    .collect();
                for (role, v) in outs {
                    if let Some(drv) = self.drivers.get(&role) {
                        drv.set_volts(circuit, self.levels.drive_volts(v));
                    }
                }
            }
        }
    }

    /// Compact register state for frame reporting (e.g. "reg" = packed byte).
    pub fn state_summary(&self) -> HashMap<String, f64> {
        let pack = |bits: &[bool]| -> f64 {
            let mut v = 0u32;
            for (i, b) in bits.iter().enumerate().take(32) {
                if *b {
                    v |= 1 << i;
                }
            }
            v as f64
        };
        let mut m = HashMap::new();
        m.insert("shift_reg".to_string(), pack(&self.shift_reg));
        m.insert("out_reg".to_string(), pack(&self.out_reg));
        m
    }
}

/// Decide a digital component's behaviour family from its model id / params.
fn classify(model: &ModelEntry) -> DigitalKind {
    let id = model.id.to_ascii_lowercase();
    if id.contains("595") {
        DigitalKind::Hc595
    } else if id.contains("165") {
        DigitalKind::Hc165
    } else {
        DigitalKind::Buffer
    }
}

/// Which pin roles a digital component treats as outputs (gets a driver).
/// Used by the binder to decide which pins to stamp Thevenin drivers on.
pub fn output_roles(model: &ModelEntry) -> Vec<String> {
    match classify(model) {
        DigitalKind::Hc595 => ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh", "qh_serial"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        DigitalKind::Hc165 => ["qh", "qh_n"].iter().map(|s| s.to_string()).collect(),
        DigitalKind::Buffer => {
            // Any pin role starting with 'y' (74HCxx convention) is an output.
            model
                .pins
                .values()
                .filter(|r| r.starts_with('y'))
                .cloned()
                .collect()
        }
    }
}
