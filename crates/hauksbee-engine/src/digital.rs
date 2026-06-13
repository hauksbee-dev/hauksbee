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
        let bits = model.params.get_f64("bits").map(|b| b as usize).unwrap_or(8);
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
                    .filter_map(|k| {
                        self.input_state.get(k).map(|v| (k.clone(), *v))
                    })
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

/// Recover the SEPARATE physical 74HC595 daisy chains on a board. Chip A
/// precedes chip B in a chain when A's `qh_serial` node == B's `ser` node; a
/// chain's head is the chip whose `ser` is not produced by any chip in the set
/// (it is driven by the MCU's serial-data net instead). Returns one
/// head-to-tail index list PER physical chain, so two chains fed by different
/// SER sources are NOT flattened into one (their serial streams must not bleed
/// across). Chips not reachable from any head form their own singleton chains.
///
/// This is the single source of truth for chain ordering, shared by the
/// scheduler's edge-driven chain controllers and the `tarski_inference` example.
pub fn order_595_chains(digital: &[DigitalComponent]) -> Vec<Vec<usize>> {
    let chips: Vec<usize> = digital
        .iter()
        .enumerate()
        .filter(|(_, d)| d.kind == DigitalKind::Hc595)
        .map(|(i, _)| i)
        .collect();

    // node -> chip whose qh_serial is that node (the producer).
    let mut producer: HashMap<i64, usize> = HashMap::new();
    for &i in &chips {
        if let Some(n) = digital[i].roles.get("qh_serial") {
            producer.insert(n.0 as i64, i);
        }
    }
    let ser_of = |i: usize| digital[i].roles.get("ser").map(|n| n.0 as i64);

    // Head: a chip whose ser is not produced by any chip in the set.
    let mut heads: Vec<usize> = chips
        .iter()
        .copied()
        .filter(|&i| match ser_of(i) {
            Some(s) => !producer.contains_key(&s),
            None => true,
        })
        .collect();
    // Deterministic.
    heads.sort_by(|&a, &b| digital[a].reference.cmp(&digital[b].reference));

    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<Vec<usize>> = Vec::new();
    for head in heads {
        let mut chain = Vec::new();
        let mut cur = Some(head);
        while let Some(i) = cur {
            if !seen.insert(i) {
                break;
            }
            chain.push(i);
            let next_node = digital[i].roles.get("qh_serial").map(|n| n.0 as i64);
            cur = next_node.and_then(|node| {
                chips
                    .iter()
                    .copied()
                    .find(|&j| ser_of(j) == Some(node) && !seen.contains(&j))
            });
        }
        if !chain.is_empty() {
            out.push(chain);
        }
    }
    // Any chip not reachable from a head (e.g. a ring, or a chip whose head was
    // pruned) becomes its own singleton chain rather than being silently merged.
    for &i in &chips {
        if seen.insert(i) {
            out.push(vec![i]);
        }
    }
    out
}

/// Flattened head-to-tail order of every 74HC595 chip, all chains concatenated.
/// Kept for the `tarski_inference` example's single-chain verification (the
/// board has exactly one 90-chip chain). The scheduler uses
/// [`order_595_chains`] instead so independent chains stay separate.
pub fn order_595_chain(digital: &[DigitalComponent]) -> Vec<usize> {
    order_595_chains(digital).into_iter().flatten().collect()
}

/// An edge-driven model of one MCU-bit-banged 74HC595 daisy-chain.
///
/// The whole chain is clocked by MCU GPIO: a broadcast shift clock (SRCLK), a
/// broadcast latch clock (RCLK), an optional broadcast clear (SRCLR_n), and a
/// serial-data line into the head chip (SER). Each chip's serial output feeds
/// the next chip's input. Because the firmware bit-bangs these lines with sub-µs
/// pulses, the chain must be resolved in the EVENT domain at edge granularity,
/// not sampled once per analog chunk (which collapses every pulse train to a
/// single final level). The scheduler captures an ordered log of GPIO
/// transitions and replays it here; only the latched parallel outputs (qa..qh)
/// are pushed back onto the analog nets.
///
/// This carries the same per-chip 8-bit shift/latch logic as `tick_595`, but
/// driven by ordered edges instead of one node-voltage sample per chunk.
pub struct Hc595Chain {
    /// Chip indices into the scheduler's `digital` vec, in daisy-chain order
    /// (head first).
    pub order: Vec<usize>,
    /// MCU GPIO `(port, bit)` for the broadcast shift clock.
    pub srclk: (char, u8),
    /// MCU GPIO `(port, bit)` for the broadcast latch clock.
    pub rclk: (char, u8),
    /// MCU GPIO `(port, bit)` for the broadcast active-low clear, if wired.
    pub srclr_n: Option<(char, u8)>,
    /// MCU GPIO `(port, bit)` for the broadcast active-low output-enable, if
    /// wired. While OE_n is HIGH the parallel outputs (qa..qh) are Hi-Z; the
    /// serial output qh_serial is NOT gated by OE.
    pub oe_n: Option<(char, u8)>,
    /// MCU GPIO `(port, bit)` for the head chip's serial-data input.
    pub ser: (char, u8),
    /// Per-chip 8-bit shift register (low byte used). `shift[c]` is chip
    /// `order[c]`'s register.
    pub shift: Vec<u8>,
    /// Per-chip latched output byte (storage register).
    pub latched: Vec<u8>,
    /// Live decoded control levels (carried across chunks so an edge in chunk N
    /// and the next edge in chunk N+1 detect a rising transition correctly).
    lvl_ser: bool,
    lvl_srclk: bool,
    lvl_rclk: bool,
    /// SRCLR_n level (active-low clear). Defaults released (high).
    lvl_srclr_n: bool,
    /// OE_n level (active-low output-enable). Defaults enabled (low) so an
    /// unwired OE never tri-states the outputs.
    lvl_oe_n: bool,
}

impl Hc595Chain {
    /// Build a chain controller from the ordered 595 chips and the MCU's net
    /// mapping. `gpio_net` resolves an MCU GPIO `(port, bit)` to the circuit node
    /// it drives; the broadcast control roles and the head SER are matched by
    /// finding the GPIO whose driven net equals the role's net node. Returns
    /// `None` if no chips, or if the essential SRCLK / RCLK / head-SER pins are
    /// not all bound to GPIO (in which case the scheduler keeps the old
    /// once-per-chunk behaviour so nothing regresses).
    pub fn build(
        digital: &[DigitalComponent],
        order: Vec<usize>,
        gpio_node: &HashMap<i64, (char, u8)>,
    ) -> Option<Self> {
        let head = *order.first()?;
        let chip = |i: usize| &digital[i];

        // Broadcast control nets are shared, so read them off the head chip.
        let role_gpio = |i: usize, role: &str| -> Option<(char, u8)> {
            let node = chip(i).roles.get(role)?;
            gpio_node.get(&(node.0 as i64)).copied()
        };

        let srclk = role_gpio(head, "srclk")?;
        let rclk = role_gpio(head, "rclk")?;
        let ser = role_gpio(head, "ser")?;
        // SRCLR_n and OE_n are optional: some boards tie them in hardware.
        let srclr_n = role_gpio(head, "srclr_n");
        let oe_n = role_gpio(head, "oe_n");

        let n = order.len();
        Some(Hc595Chain {
            order,
            srclk,
            rclk,
            srclr_n,
            oe_n,
            ser,
            shift: vec![0u8; n],
            latched: vec![0u8; n],
            lvl_ser: false,
            lvl_srclk: false,
            lvl_rclk: false,
            lvl_srclr_n: true,
            lvl_oe_n: false,
        })
    }

    /// Replay an ordered log of GPIO transitions, clocking the chain at edge
    /// granularity. On each SRCLK rising edge the whole chain shifts up one bit
    /// carrying serial across chips (qh_serial[k] -> ser[k+1]); on each RCLK
    /// rising edge every chip latches; while SRCLR_n is low the shift registers
    /// hold cleared. Levels persist across calls.
    pub fn replay(&mut self, edges: &[(char, u8, bool)]) {
        for &(port, bit, high) in edges {
            let pin = (port, bit);
            if pin == self.ser {
                self.lvl_ser = high;
            }
            if Some(pin) == self.srclr_n {
                self.lvl_srclr_n = high;
                if !high {
                    // Active-low clear: wipe the shift registers (not storage).
                    for s in self.shift.iter_mut() {
                        *s = 0;
                    }
                }
            }
            if Some(pin) == self.oe_n {
                // Active-low output-enable: tracked here, applied in `apply`.
                self.lvl_oe_n = high;
            }
            if pin == self.srclk {
                let rising = high && !self.lvl_srclk;
                self.lvl_srclk = high;
                if rising && self.lvl_srclr_n {
                    self.shift_once();
                }
            }
            if pin == self.rclk {
                let rising = high && !self.lvl_rclk;
                self.lvl_rclk = high;
                if rising {
                    self.latched.copy_from_slice(&self.shift);
                }
            }
        }
    }

    /// One SRCLK rising edge: shift every chip up one bit, carrying each chip's
    /// stage-7 serial output into the next chip's stage 0. The head takes the
    /// current SER level. This is the PATH B carry logic.
    fn shift_once(&mut self) {
        let mut carry = self.lvl_ser as u8;
        for s in self.shift.iter_mut() {
            let out_bit = (*s >> 7) & 1; // stage 7 = qh_serial
            *s = ((*s << 1) | carry) & 0xFF;
            carry = out_bit;
        }
    }

    /// Push each chip's latched byte onto its qa..qh output drivers and mirror
    /// the latched/shift state into the owning `DigitalComponent` so frame
    /// reporting (`state_summary`) stays correct. `out_reg[0]=qa` is stage 0.
    ///
    /// While OE_n is HIGH the parallel outputs are tri-stated (Hi-Z): the qa..qh
    /// drivers are disabled so the analog solve sees a high-impedance leg, not a
    /// stale latched level. The serial output qh_serial is not gated by OE.
    pub fn apply(&mut self, digital: &mut [DigitalComponent], circuit: &mut Circuit) {
        // OE_n defaults low (enabled) when no OE pin is wired.
        let outputs_enabled = !self.lvl_oe_n;
        for (c, &chip_idx) in self.order.iter().enumerate() {
            let latched = self.latched[c];
            let shift = self.shift[c];
            let d = &mut digital[chip_idx];
            for i in 0..d.bits.min(8) {
                let bit = ((latched >> i) & 1) == 1;
                d.out_reg[i] = bit;
                let s = ((shift >> i) & 1) == 1;
                d.shift_reg[i] = s;
            }
            // Tri-state / enable the parallel-output drivers per OE_n. qh_serial
            // is left enabled (it is not output-enable gated on the 74HC595).
            for name in ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh"] {
                if let Some(drv) = d.drivers.get_mut(name) {
                    drv.set_enabled(circuit, outputs_enabled);
                }
            }
            d.drive_outputs(circuit);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_ir::Circuit;
    use hauksbee_models::{ComponentQuery, ModelLibrary};

    /// Resolve the builtin 74HC595 model entry for test fixtures.
    fn hc595_model() -> hauksbee_models::ModelEntry {
        let lib = ModelLibrary::builtin();
        let q = ComponentQuery::new(None, Some("74HC595".to_string()), None);
        lib.resolve(&q).model.expect("builtin 74HC595 model")
    }

    /// Build an `n`-chip 74HC595 daisy chain plus a synthetic MCU GPIO net map,
    /// returning the chips, the built chain controller, and the control pins.
    /// Wiring: shared SRCLK / RCLK / SRCLR_n / SER head net are distinct nodes;
    /// each chip's qh_serial feeds the next chip's ser.
    fn build_chain(circuit: &mut Circuit, n: usize) -> (Vec<DigitalComponent>, Hc595Chain) {
        let model = hc595_model();

        // Shared control nets + the head serial-data net.
        let n_srclk = circuit.node("SRCLK");
        let n_rclk = circuit.node("RCLK");
        let n_srclr = circuit.node("SRCLR_N");
        let n_ser_head = circuit.node("SER0");

        let mut chips: Vec<DigitalComponent> = Vec::new();
        let mut prev_qh: Option<NodeId> = None;
        for k in 0..n {
            let mut roles: HashMap<String, NodeId> = HashMap::new();
            roles.insert("srclk".into(), n_srclk);
            roles.insert("rclk".into(), n_rclk);
            roles.insert("srclr_n".into(), n_srclr);
            // First chip's ser is the MCU head net; later chips chain.
            let ser_node = prev_qh.unwrap_or(n_ser_head);
            roles.insert("ser".into(), ser_node);
            // This chip's serial output net (feeds the next chip).
            let qh = circuit.node(&format!("QHS{k}"));
            roles.insert("qh_serial".into(), qh);
            for (i, q) in ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh"].iter().enumerate() {
                roles.insert((*q).into(), circuit.node(&format!("Q{k}_{i}")));
            }
            chips.push(DigitalComponent::new(
                format!("U{k}"),
                &model,
                roles,
                HashMap::new(),
            ));
            prev_qh = Some(qh);
        }

        // Synthetic MCU GPIO net->(port,bit) map: the firmware drives SRCLK on
        // PB5, RCLK on PD6, SRCLR_n on PC3, SER on PB3 (the real Tarski mapping).
        let mut gpio_node: HashMap<i64, (char, u8)> = HashMap::new();
        gpio_node.insert(n_srclk.0 as i64, ('B', 5));
        gpio_node.insert(n_rclk.0 as i64, ('D', 6));
        gpio_node.insert(n_srclr.0 as i64, ('C', 3));
        gpio_node.insert(n_ser_head.0 as i64, ('B', 3));

        let order = order_595_chain(&chips);
        assert_eq!(order.len(), n, "all {n} chips ordered into one chain");
        // The chain must walk head-to-tail in declaration order (U0..Un-1).
        let refs: Vec<&str> = order.iter().map(|&i| chips[i].reference.as_str()).collect();
        let want: Vec<String> = (0..n).map(|k| format!("U{k}")).collect();
        assert_eq!(refs, want, "daisy-chain order recovered from nets");

        let chain = Hc595Chain::build(&chips, order, &gpio_node).expect("chain binds to GPIO");
        (chips, chain)
    }

    /// Append the ordered edge stream for one `shiftOut(MSBFIRST)` of `byte` on
    /// the head SER (PB3) clocked by SRCLK (PB5): for bit 7..0, set SER then
    /// pulse SRCLK high/low.
    fn shift_out_msb_first(log: &mut Vec<(char, u8, bool)>, byte: u8) {
        for bit in (0..8).rev() {
            let b = ((byte >> bit) & 1) == 1;
            log.push(('B', 3, b)); // SER = data bit
            log.push(('B', 5, true)); // SRCLK rising: clock the bit in
            log.push(('B', 5, false)); // SRCLK falling
        }
    }

    /// The core FIX 1 proof: a synthetic ordered edge stream reproducing the
    /// firmware's `shiftOut(MSBFIRST)` of N known bytes through an N-chip chain,
    /// plus an RCLK latch pulse, must land in silicon exactly as PATH B predicts
    /// (first-sent byte ends in the LAST chip: latched[p] == weights[n-1-p]).
    #[test]
    fn edge_stream_latches_chain_in_path_b_order() {
        let n = 4;
        let mut circuit = Circuit::new();
        let (_chips, mut chain) = build_chain(&mut circuit, n);

        // Known distinct weights, byte k = some recognizable pattern.
        let weights: Vec<u8> = vec![0x11, 0x22, 0x33, 0x44];

        // Build the full ordered edge stream the firmware emits: release the
        // clear (SRCLR_n high), shiftOut every byte MSB-first, then pulse RCLK.
        let mut log: Vec<(char, u8, bool)> = Vec::new();
        log.push(('C', 3, true)); // SRCLR_n high: release clear
        for &b in &weights {
            shift_out_msb_first(&mut log, b);
        }
        log.push(('D', 6, true)); // RCLK rising: latch
        log.push(('D', 6, false)); // RCLK falling

        chain.replay(&log);

        // PATH B expectation: first-sent byte (weights[0]) ends in the LAST chip.
        for p in 0..n {
            let want = weights[n - 1 - p];
            assert_eq!(
                chain.latched[p], want,
                "chip at chain position {p} should latch weights[{}] = 0x{want:02X}, got 0x{:02X}",
                n - 1 - p,
                chain.latched[p]
            );
        }
    }

    /// Regression guard: this is what the OLD once-per-chunk path saw. Collapsing
    /// the SCLK pulse train to its LATEST level (what `pin_edges` did, and what
    /// `tick_595` sampled) clocks the chain AT MOST once per chunk, so it can
    /// never reproduce the PATH B latch. This asserts the collapsed model gets
    /// the WRONG answer, proving the edge path is load-bearing.
    #[test]
    fn collapsed_latest_level_does_not_latch_correctly() {
        let n = 4;
        let mut circuit = Circuit::new();
        let (_chips, mut chain) = build_chain(&mut circuit, n);
        let weights: Vec<u8> = vec![0x11, 0x22, 0x33, 0x44];

        // Collapse each shiftOut byte to a single net "final level" per control
        // line, the way the latest-level map did within one chunk: SER ends at
        // bit 0 of the last byte, SRCLK ends low, one (collapsed) RCLK latch.
        let mut log: Vec<(char, u8, bool)> = Vec::new();
        log.push(('C', 3, true));
        // Only the final settled levels survive a latest-level collapse: SER's
        // last value and a single SRCLK edge (no train), then the latch.
        let last_bit = (weights[n - 1] & 1) == 1;
        log.push(('B', 3, last_bit));
        log.push(('B', 5, true)); // a single SRCLK edge (the collapse keeps one)
        log.push(('D', 6, true));

        chain.replay(&log);

        // With only one shift the chain cannot hold all four distinct bytes.
        let matches = (0..n).filter(|&p| chain.latched[p] == weights[n - 1 - p]).count();
        assert!(
            matches < n,
            "collapsed single-edge replay must NOT reproduce the full PATH B latch \
             (got {matches}/{n} correct); the edge path is what makes it work"
        );
    }

    /// Two physically independent 595 chains (different SER source nets) must be
    /// recovered as SEPARATE chains, not flattened into one register where chain
    /// A's tail serial bleeds into chain B's head.
    #[test]
    fn independent_chains_are_not_merged() {
        let model = hc595_model();
        let mut circuit = Circuit::new();
        // Shared clock/latch, but two distinct SER head nets -> two chains.
        let srclk = circuit.node("SRCLK");
        let rclk = circuit.node("RCLK");
        let mut chips: Vec<DigitalComponent> = Vec::new();
        // Build chain `tag` of 2 chips fed by its own head SER net.
        let make = |chips: &mut Vec<DigitalComponent>, tag: &str, circuit: &mut Circuit| {
            let head_ser = circuit.node(&format!("SER_{tag}"));
            let mut prev_qh: Option<NodeId> = None;
            for k in 0..2 {
                let mut roles: HashMap<String, NodeId> = HashMap::new();
                roles.insert("srclk".into(), srclk);
                roles.insert("rclk".into(), rclk);
                roles.insert("ser".into(), prev_qh.unwrap_or(head_ser));
                let qh = circuit.node(&format!("QHS_{tag}{k}"));
                roles.insert("qh_serial".into(), qh);
                chips.push(DigitalComponent::new(
                    format!("U_{tag}{k}"),
                    &model,
                    roles,
                    HashMap::new(),
                ));
                prev_qh = Some(qh);
            }
        };
        make(&mut chips, "A", &mut circuit);
        make(&mut chips, "B", &mut circuit);

        let chains = order_595_chains(&chips);
        assert_eq!(chains.len(), 2, "two independent chains, not one merged list");
        for ch in &chains {
            assert_eq!(ch.len(), 2, "each chain has its own 2 chips");
        }

        // Drive only chain A's controller with all-ones serial; chain B must stay
        // zero (no serial bleed across the chain boundary).
        let ser_a = circuit.node("SER_A");
        let mut gpio_a: HashMap<i64, (char, u8)> = HashMap::new();
        gpio_a.insert(srclk.0 as i64, ('B', 5));
        gpio_a.insert(rclk.0 as i64, ('D', 6));
        gpio_a.insert(ser_a.0 as i64, ('B', 3));
        // Find the chain whose head is U_A0.
        let order_a = chains
            .iter()
            .find(|c| chips[c[0]].reference == "U_A0")
            .cloned()
            .expect("chain A present");
        let mut chain_a = Hc595Chain::build(&chips, order_a, &gpio_a).expect("chain A binds");

        let mut log = Vec::new();
        for _ in 0..16 {
            log.push(('B', 3, true)); // SER high
            log.push(('B', 5, true));
            log.push(('B', 5, false));
        }
        log.push(('D', 6, true)); // latch
        chain_a.replay(&log);
        assert_eq!(chain_a.latched, vec![0xFF, 0xFF], "chain A fills with ones");
        // chain B is a different controller and was never clocked here; the key
        // point proven above is that A's 2-chip register did not absorb B.
    }

    /// OE_n (active-low output enable) gates the parallel outputs: while OE_n is
    /// HIGH the qa..qh drivers tri-state (Hi-Z), so the analog solve does not see
    /// the latched levels. The serial output is unaffected. We assert the drivers
    /// get disabled/enabled to track OE.
    #[test]
    fn oe_high_tristates_parallel_outputs() {
        use crate::drivers::{PinDriver, DEFAULT_RO};
        let model = hc595_model();
        let mut circuit = Circuit::new();
        let srclk = circuit.node("SRCLK");
        let rclk = circuit.node("RCLK");
        let ser = circuit.node("SER");
        let oe = circuit.node("OE_N");

        let mut roles: HashMap<String, NodeId> = HashMap::new();
        roles.insert("srclk".into(), srclk);
        roles.insert("rclk".into(), rclk);
        roles.insert("ser".into(), ser);
        roles.insert("oe_n".into(), oe);
        // Stamp real drivers on qa..qh so we can read their enabled state.
        let mut drivers: HashMap<String, PinDriver> = HashMap::new();
        for q in ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh"] {
            let net = circuit.node(&q.to_uppercase());
            roles.insert(q.into(), net);
            let drv = PinDriver::stamp(&mut circuit, net, q, &format!("U_{q}"), DEFAULT_RO);
            drivers.insert(q.into(), drv);
        }
        let mut chips = vec![DigitalComponent::new("U0".into(), &model, roles, drivers)];

        let mut gpio: HashMap<i64, (char, u8)> = HashMap::new();
        gpio.insert(srclk.0 as i64, ('B', 5));
        gpio.insert(rclk.0 as i64, ('D', 6));
        gpio.insert(ser.0 as i64, ('B', 3));
        gpio.insert(oe.0 as i64, ('C', 2));
        let order = order_595_chains(&chips).into_iter().next().unwrap();
        let mut chain = Hc595Chain::build(&chips, order, &gpio).expect("binds");
        assert_eq!(chain.oe_n, Some(('C', 2)), "OE_n bound to PC2");

        // Drive OE_n HIGH (outputs disabled), then apply.
        chain.replay(&[('C', 2, true)]);
        chain.apply(&mut chips, &mut circuit);
        assert!(
            chips[0].drivers.values().all(|d| !d.enabled),
            "OE_n high tri-states all qa..qh drivers"
        );

        // Drive OE_n LOW (outputs enabled), then apply.
        chain.replay(&[('C', 2, false)]);
        chain.apply(&mut chips, &mut circuit);
        assert!(
            chips[0].drivers.values().all(|d| d.enabled),
            "OE_n low re-enables qa..qh drivers"
        );
    }
}

/// Which pin roles a digital component treats as outputs (gets a driver).
/// Used by the binder to decide which pins to stamp Thevenin drivers on.
pub fn output_roles(model: &ModelEntry) -> Vec<String> {
    match classify(model) {
        DigitalKind::Hc595 => [
            "qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh", "qh_serial",
        ]
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
