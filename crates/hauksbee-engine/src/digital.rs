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
    /// Cross-coupled NOR set/reset latch (a 74HC02 latch pair). Roles `set`
    /// (active-high SET input), `reset` (active-high RESET input), and output
    /// `q`. Active-LOW Q semantics of the Tarski spike recorder: at reset Q is
    /// HIGH (idle), a SET pulse drives Q LOW and the cross-couple HOLDS it LOW
    /// until the next RESET. Modelled directly from the NOR-latch truth table so
    /// the firmware-driven 74HC165 readback samples the same level the real
    /// board's latch Q presents (idle HIGH -> 0xFFC0, captured spike LOW).
    NorLatch,
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
    /// NOR-latch held state: (Q, Qb). Initialised to the post-reset idle level
    /// (Q HIGH) so a board that has never been driven reads the cleared word.
    latch_q: bool,
    latch_qb: bool,
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
            // Power-on idle = the cleared latch state (Q HIGH, Qb LOW), matching
            // the real 74HC02 NOR latch after RESET_SR with no spike captured.
            latch_q: true,
            latch_qb: false,
        }
    }

    /// Build a NOR SR latch component (one latch = one cross-coupled gate
    /// pair). `roles` must carry `set`, `reset`, and `q`; the caller has stamped
    /// the `q` output [`PinDriver`]. Uses the supplied logic levels (the 74HC02
    /// model entry's). State initialises to the cleared idle (Q HIGH).
    pub fn new_nor_latch(
        reference: String,
        levels: LogicLevels,
        roles: HashMap<String, NodeId>,
        drivers: HashMap<String, PinDriver>,
    ) -> Self {
        DigitalComponent {
            reference,
            kind: DigitalKind::NorLatch,
            levels,
            roles,
            drivers,
            input_state: HashMap::new(),
            shift_reg: vec![false; 1],
            out_reg: vec![false; 1],
            bits: 1,
            prev_srclk: false,
            prev_rclk: false,
            prev_clk: false,
            prev_pl: false,
            latch_q: true,
            latch_qb: false,
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
            DigitalKind::NorLatch => self.tick_nor_latch(node_v),
            DigitalKind::Buffer => self.tick_buffer(node_v),
        }
        self.drive_outputs(circuit);
    }

    /// Latch a parallel-output byte directly into the output register and push
    /// it onto the analog drivers, bypassing the serial shift/clock sequence.
    /// This is the model-level "the host already streamed and latched the
    /// chain" shortcut (the same end state as the edge-driven `Hc595Chain`),
    /// for harnesses that drive a 74HC595's known latched value without
    /// simulating the SPI bit-bang. `out_reg[i]` (i.e. `qa+i`) gets bit `i` of
    /// `byte`. No-op for non-595 kinds.
    pub fn latch_byte(&mut self, circuit: &mut Circuit, byte: u8) {
        if self.kind != DigitalKind::Hc595 {
            return;
        }
        for i in 0..self.bits.min(8) {
            self.out_reg[i] = (byte >> i) & 1 == 1;
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

    /// Cross-coupled NOR SR latch (74HC02 latch pair). Q = NOR(set, Qb),
    /// Qb = NOR(reset, Q). `set` and `reset` are sampled active-HIGH from the
    /// analog nets (SPIKE<n> and RESET_SR); the held (Q, Qb) is re-settled each
    /// tick by iterating the two NOR equations to a fixed point.
    ///
    /// Truth table (matching the Tarski spike recorder):
    ///   reset=1            -> Q forced LOW? NO. With this wiring Q = NOR(set, Qb)
    ///                         and Qb = NOR(reset, Q): reset HIGH pulls Qb LOW, so
    ///                         Q = NOR(set, 0) = !set. With set idle (0) Q is HIGH
    ///                         = the cleared/idle level (active-low: idle reads
    ///                         HIGH). A captured spike is set=1 -> Q LOW, held.
    ///   set=1,reset=0      -> Q LOW (a spike is latched).
    ///   set=0,reset=0      -> HOLD (Q keeps its last value -- the latch memory).
    fn tick_nor_latch(&mut self, node_v: &dyn Fn(NodeId) -> f64) {
        let set = self.sample("set", node_v);
        let reset = self.sample("reset", node_v);
        let nor = |a: bool, b: bool| !(a || b);
        // Iterate to the latch's stable fixed point (≤4 passes suffices for a
        // 2-gate cross-couple; HOLD keeps the prior state).
        let (mut q, mut qb) = (self.latch_q, self.latch_qb);
        for _ in 0..4 {
            let nq = nor(set, qb);
            let nqb = nor(reset, q);
            if nq == q && nqb == qb {
                break;
            }
            q = nq;
            qb = nqb;
        }
        self.latch_q = q;
        self.latch_qb = qb;
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
            DigitalKind::NorLatch => {
                if let Some(drv) = self.drivers.get("q") {
                    drv.set_volts(circuit, self.levels.drive_volts(self.latch_q));
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
            // u8 wraps to 8 bits on shift, so no explicit & 0xFF mask is needed.
            *s = (*s << 1) | carry;
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

/// Recover the SEPARATE physical 74HC165 serial-out chains on a board, returned
/// HEAD-FIRST per chain. The HEAD is the chip whose `qh` serial output is read
/// by the MCU (it is not consumed by another 165's `ser` input); each subsequent
/// chip is the one feeding the previous chip's `ser` from its own `qh`. So
/// walking a chain head→tail follows the serial bitstream backward from QH/MISO
/// into the upstream chips — the order the firmware shifts bits OUT.
///
/// Mirror of [`order_595_chains`] for the read direction. A chip not reachable
/// from any head becomes its own singleton chain rather than being merged.
pub fn order_165_chains(digital: &[DigitalComponent]) -> Vec<Vec<usize>> {
    let chips: Vec<usize> = digital
        .iter()
        .enumerate()
        .filter(|(_, d)| d.kind == DigitalKind::Hc165)
        .map(|(i, _)| i)
        .collect();

    // node -> chip whose `ser` input is that node (the consumer of a QH).
    let mut consumer: HashMap<i64, usize> = HashMap::new();
    for &i in &chips {
        if let Some(n) = digital[i].roles.get("ser") {
            consumer.insert(n.0 as i64, i);
        }
    }
    let qh_of = |i: usize| digital[i].roles.get("qh").map(|n| n.0 as i64);

    // Head: a chip whose `qh` is not consumed by any chip's `ser` (it feeds the
    // MCU's MISO instead).
    let mut heads: Vec<usize> = chips
        .iter()
        .copied()
        .filter(|&i| match qh_of(i) {
            Some(q) => !consumer.contains_key(&q),
            None => true,
        })
        .collect();
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
            // The next (upstream) chip is the one whose `qh` feeds THIS chip's
            // `ser`.
            let ser_node = digital[i].roles.get("ser").map(|n| n.0 as i64);
            cur = ser_node.and_then(|node| {
                chips
                    .iter()
                    .copied()
                    .find(|&j| qh_of(j) == Some(node) && !seen.contains(&j))
            });
        }
        if !chain.is_empty() {
            out.push(chain);
        }
    }
    for &i in &chips {
        if seen.insert(i) {
            out.push(vec![i]);
        }
    }
    out
}

/// An edge-driven model of one MCU-bit-banged 74HC165 parallel-in / serial-out
/// chain — the READ-direction analogue of [`Hc595Chain`].
///
/// The firmware reads the chain by pulsing PL (parallel-load) low to capture the
/// parallel inputs, then bit-banging the shared SCLK while sampling the head
/// chip's QH on its MISO input pin. Both the PL pulse and the SCLK pulse train
/// are sub-µs back-to-back `digitalWrite`s, far below the analog chunk rate, so
/// they MUST be resolved in the EVENT domain at edge granularity — exactly like
/// the 595 write path. The crucial difference: the firmware `digitalRead`s the
/// serial-out bit *between its own clock edges, inside the same `run_micros`*,
/// so this chain runs synchronously from the MCU's GPIO-output hook (via the
/// MCU's input-responder) and drives the next QH bit straight onto the MISO
/// input pin, before the firmware's next instruction.
///
/// On PL falling edge it samples every chip's parallel inputs (a..h) into a
/// bit sequence ordered the way bits emerge at QH (head chip's h,g,…,a, then the
/// upstream chip's h,…,a, …) and presents bit 0 on MISO. On each SCLK RISING
/// edge it advances to the next bit and presents it. This reproduces the exact
/// `value` the firmware's `_ReadShiftRegisterWord` accumulates.
pub struct Hc165Chain {
    /// Chip indices into the scheduler's `digital` vec, HEAD first (QH→MISO).
    pub order: Vec<usize>,
    /// MCU GPIO `(port, bit)` for the broadcast parallel-load (active-low).
    pub pl_n: (char, u8),
    /// MCU GPIO `(port, bit)` for the broadcast shift clock.
    pub clk: (char, u8),
    /// MCU input pin `(port, bit)` the head chip's QH drives (MISO).
    pub miso: (char, u8),
    /// Per chip, its 8 parallel-input net nodes in role order a..h. `None`
    /// where that input is unconnected (reads low).
    pub inputs: Vec<[Option<NodeId>; 8]>,
    /// The captured serial bit sequence in QH-emit order (index 0 = first bit
    /// out, before any clock). Rebuilt on each PL load.
    seq: Vec<bool>,
    /// Index of the bit currently presented at QH.
    pos: usize,
    /// Live decoded control levels (carried across edges within a chunk).
    lvl_pl_n: bool,
    lvl_clk: bool,
    /// The current QH level being presented on MISO.
    lvl_qh: bool,
}

impl Hc165Chain {
    /// Build a 165 read-chain controller from the ordered 165 chips and the
    /// MCU's GPIO net map. PL and CLK are broadcast control nets read off the
    /// head chip; the head's `qh` net must map to an MCU input pin (MISO).
    /// Returns `None` if the essential PL / CLK / QH→MISO bindings are missing.
    pub fn build(
        digital: &[DigitalComponent],
        order: Vec<usize>,
        gpio_node: &HashMap<i64, (char, u8)>,
        input_node: &HashMap<i64, (char, u8)>,
    ) -> Option<Self> {
        let head = *order.first()?;
        let chip = |i: usize| &digital[i];
        let role_gpio = |i: usize, role: &str| -> Option<(char, u8)> {
            let node = chip(i).roles.get(role)?;
            gpio_node.get(&(node.0 as i64)).copied()
        };

        let pl_n = role_gpio(head, "pl_n")?;
        let clk = role_gpio(head, "clk")?;
        // The head chip's QH must be wired to an MCU input pin (MISO).
        let qh_node = chip(head).roles.get("qh")?;
        let miso = input_node.get(&(qh_node.0 as i64)).copied()?;

        // Capture each chip's parallel-input nodes (a..h) for sampling on load.
        let roles = ["a", "b", "c", "d", "e", "f", "g", "h"];
        let inputs: Vec<[Option<NodeId>; 8]> = order
            .iter()
            .map(|&ci| {
                let mut arr = [None; 8];
                for (k, r) in roles.iter().enumerate() {
                    arr[k] = digital[ci].roles.get(*r).copied();
                }
                arr
            })
            .collect();

        Some(Hc165Chain {
            order,
            pl_n,
            clk,
            miso,
            inputs,
            seq: Vec::new(),
            pos: 0,
            lvl_pl_n: true,
            lvl_clk: false,
            lvl_qh: false,
        })
    }

    /// MCU GPIO pins this chain consumes edges from: PL, CLK. (MISO is an
    /// output of the chain, an input to the MCU.)
    pub fn watches(&self, pin: (char, u8)) -> bool {
        pin == self.pl_n || pin == self.clk
    }

    /// Latch the parallel inputs into the QH-emit-ordered bit sequence, using a
    /// snapshot of the current solved node voltages and the chips' logic
    /// thresholds. Head chip's h,g,…,a come first (they reach QH first), then
    /// each upstream chip's h,…,a.
    pub fn load(&mut self, node_v: &dyn Fn(NodeId) -> f64, levels: &LogicLevels) {
        self.seq.clear();
        for chip_inputs in &self.inputs {
            // Emit order within a chip is h,g,f,e,d,c,b,a (h reaches QH first).
            for k in (0..8).rev() {
                let bit = match chip_inputs[k] {
                    Some(n) => node_v(n) >= levels.vih,
                    None => false,
                };
                self.seq.push(bit);
            }
        }
        self.pos = 0;
        self.lvl_qh = self.seq.first().copied().unwrap_or(false);
    }

    /// Process one GPIO output edge. Returns the MISO drive `(pin, level)` if the
    /// presented QH bit changed (so the responder can push it onto the MCU input
    /// pin). PL low (re)loads; SCLK rising advances to the next bit.
    pub fn on_edge(
        &mut self,
        pin: (char, u8),
        high: bool,
        node_v: &dyn Fn(NodeId) -> f64,
        levels: &LogicLevels,
    ) -> Option<((char, u8), bool)> {
        let prev_qh = self.lvl_qh;
        if pin == self.pl_n {
            let falling = !high && self.lvl_pl_n;
            self.lvl_pl_n = high;
            // 74HC165 loads asynchronously while PL is LOW; capture on the
            // falling edge (data is stable by then in the firmware's PL pulse).
            if falling {
                self.load(node_v, levels);
            }
        } else if pin == self.clk {
            let rising = high && !self.lvl_clk;
            self.lvl_clk = high;
            // Shifts only happen in shift mode (PL released high). A rising CLK
            // advances the register one stage toward QH.
            if rising && self.lvl_pl_n {
                self.pos += 1;
                self.lvl_qh = self.seq.get(self.pos).copied().unwrap_or(false);
            }
        }
        if self.lvl_qh != prev_qh || pin == self.pl_n {
            // Always (re)assert MISO on a load so the first bit is present before
            // the firmware's first read, even when it equals the prior level.
            Some((self.miso, self.lvl_qh))
        } else {
            None
        }
    }

    /// Logic thresholds of the head chip (for input sampling). The chips share a
    /// family, so any chip's levels serve.
    pub fn levels(&self, digital: &[DigitalComponent]) -> LogicLevels {
        self.order
            .first()
            .map(|&i| digital[i].levels)
            .unwrap_or(LogicLevels {
                voh: 4.4,
                vol: 0.1,
                vih: 3.15,
                vil: 1.35,
                ro: DEFAULT_RO,
            })
    }

    /// The current word the chain would have shifted out given a captured load,
    /// MSB-first as the firmware accumulates it (bit 15 = first bit out). For
    /// diagnostics / tests; reflects `seq` independent of clocking position.
    pub fn loaded_word(&self) -> u16 {
        let mut w = 0u16;
        for (i, &b) in self.seq.iter().enumerate().take(16) {
            if b {
                w |= 1 << (15 - i);
            }
        }
        w
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
        DigitalKind::Hc595 => [
            "qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh", "qh_serial",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        DigitalKind::Hc165 => ["qh", "qh_n"].iter().map(|s| s.to_string()).collect(),
        DigitalKind::NorLatch => vec!["q".to_string()],
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

        // The full PATH B expectation is the distinct-byte pattern below; a
        // collapsed single-edge replay must NOT reproduce it (tighter than a
        // per-position count, which could tolerate a lucky partial match). In
        // fact a single SRCLK edge shifts in only one bit, so the registers are
        // all zero, which we also assert to keep the test non-vacuous.
        let expected: Vec<u8> = (0..n).map(|p| weights[n - 1 - p]).collect();
        assert_ne!(
            chain.latched, expected,
            "collapsed single-edge replay must NOT reproduce the full PATH B latch; \
             the edge path is what makes it work"
        );
        assert_eq!(
            chain.latched,
            vec![0u8; n],
            "a single SRCLK edge shifts in only one bit, so nothing meaningful latches"
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

        // Build BOTH controllers. Chain A's SER is PB3; chain B's SER is a
        // different head net (here unbound to any MCU GPIO, so B sees no SER
        // edges). Both share SRCLK/RCLK so B is clocked by the same pulse train.
        let ser_a = circuit.node("SER_A");
        let mut gpio_a: HashMap<i64, (char, u8)> = HashMap::new();
        gpio_a.insert(srclk.0 as i64, ('B', 5));
        gpio_a.insert(rclk.0 as i64, ('D', 6));
        gpio_a.insert(ser_a.0 as i64, ('B', 3));
        // Chain B: same clock/latch GPIO, but its SER head is a DIFFERENT MCU
        // pin (PB4) that never toggles in the log below, so B's serial input
        // stays low regardless of A's data.
        let ser_b = circuit.node("SER_B");
        let mut gpio_b = gpio_a.clone();
        gpio_b.remove(&(ser_a.0 as i64));
        gpio_b.insert(ser_b.0 as i64, ('B', 4));

        let find = |head: &str, chains: &[Vec<usize>]| -> Vec<usize> {
            chains
                .iter()
                .find(|c| chips[c[0]].reference == head)
                .cloned()
                .expect("chain present")
        };
        let mut chain_a = Hc595Chain::build(&chips, find("U_A0", &chains), &gpio_a)
            .expect("chain A binds");
        let mut chain_b = Hc595Chain::build(&chips, find("U_B0", &chains), &gpio_b)
            .expect("chain B binds");

        // Clock 16 ones into A while feeding the SAME pulse train to B.
        let mut log = Vec::new();
        for _ in 0..16 {
            log.push(('B', 3, true)); // SER (A's head) high
            log.push(('B', 5, true)); // SRCLK rising (broadcast)
            log.push(('B', 5, false)); // SRCLK falling
        }
        log.push(('D', 6, true)); // RCLK latch (broadcast)
        chain_a.replay(&log);
        chain_b.replay(&log);

        assert_eq!(chain_a.latched, vec![0xFF, 0xFF], "chain A fills with ones");
        assert_eq!(
            chain_b.latched,
            vec![0x00, 0x00],
            "chain B, clocked by the same SRCLK but with no SER, stays zero: \
             A's serial does NOT bleed across the chain boundary"
        );
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

    /// Resolve the builtin 74HC165 model entry for test fixtures.
    fn hc165_model() -> hauksbee_models::ModelEntry {
        let lib = ModelLibrary::builtin();
        let q = ComponentQuery::new(None, Some("74HC165".to_string()), None);
        lib.resolve(&q).model.expect("builtin 74HC165 model")
    }

    /// Build the real Tarski 2-chip 165 read chain: U15002 is the head whose QH
    /// feeds MISO; U15001 is upstream (its QH → U15002.ser). The parallel inputs
    /// carry the spike latches; we wire a chosen set HIGH and the rest to GND, and
    /// drive their net voltages so the chain samples them on a PL load.
    fn build_165_chain(
        circuit: &mut Circuit,
        head_inputs_hi: &[&str], // role letters a..h on the HEAD (U15002) to set high
        up_inputs_hi: &[&str],   // role letters a..h on the UPSTREAM (U15001) to set high
    ) -> (Vec<DigitalComponent>, Hc165Chain) {
        let model = hc165_model();
        let pl = circuit.node("PARALLEL_LOAD");
        let clk = circuit.node("SCLK");
        let miso = circuit.node("MISO");
        let inter = circuit.node("U15001_Q7"); // U15001.qh -> U15002.ser

        let make = |circuit: &mut Circuit,
                    refn: &str,
                    ser: NodeId,
                    qh: NodeId,
                    hi: &[&str]|
         -> DigitalComponent {
            let mut roles: HashMap<String, NodeId> = HashMap::new();
            roles.insert("pl_n".into(), pl);
            roles.insert("clk".into(), clk);
            roles.insert("ser".into(), ser);
            roles.insert("qh".into(), qh);
            // Each parallel input gets its own net; set high ones to +5, rest GND.
            for r in ["a", "b", "c", "d", "e", "f", "g", "h"] {
                let n = circuit.node(&format!("{refn}_{r}"));
                roles.insert(r.into(), n);
            }
            let d = DigitalComponent::new(refn.into(), &model, roles.clone(), HashMap::new());
            // Drive the input nets via voltage sources so node_v reads them.
            for r in ["a", "b", "c", "d", "e", "f", "g", "h"] {
                let n = roles[r];
                let v = if hi.contains(&r) { 5.0 } else { 0.0 };
                circuit.add(hauksbee_ir::Device::Vsource {
                    name: format!("V_{refn}_{r}"),
                    p: n,
                    n: NodeId::GROUND,
                    kind: hauksbee_ir::SourceKind::Dc(v),
                });
            }
            d
        };

        // U15001 upstream (ser unused → tie to GND node), QH → inter.
        let up = make(circuit, "U15001", NodeId::GROUND, inter, up_inputs_hi);
        // U15002 head: ser ← inter, QH → MISO.
        let head = make(circuit, "U15002", inter, miso, head_inputs_hi);
        let chips = vec![up, head];

        // GPIO map: PL=PD4, SCLK=PB5, MISO=PB4 (the firmware mapping).
        let mut gpio: HashMap<i64, (char, u8)> = HashMap::new();
        gpio.insert(pl.0 as i64, ('D', 4));
        gpio.insert(clk.0 as i64, ('B', 5));
        gpio.insert(miso.0 as i64, ('B', 4));

        let order = order_165_chains(&chips);
        assert_eq!(order.len(), 1, "one 165 chain recovered");
        // Head-first: U15002 (feeds MISO) before U15001 (upstream).
        let refs: Vec<&str> = order[0].iter().map(|&i| chips[i].reference.as_str()).collect();
        assert_eq!(refs, vec!["U15002", "U15001"], "head-first chain order");

        let chain = Hc165Chain::build(&chips, order.into_iter().next().unwrap(), &gpio, &gpio)
            .expect("165 chain binds to GPIO/MISO");
        (chips, chain)
    }

    /// Replay the firmware's exact ReadOutput sequence against the edge-driven
    /// chain: PL low/high (load), then 16×(read MISO, pulse SCLK high/low), and
    /// reconstruct the same `value` `_ReadShiftRegisterWord` accumulates. The
    /// known latch pattern must read back bit-exact.
    #[test]
    fn hc165_reads_known_latch_pattern_via_edges() {
        let mut circuit = Circuit::new();
        // Map the real spike-latch wiring: head=U15002 inputs a..h = L3..L10,
        // upstream=U15001 inputs g,h = L1,L2 (others unconnected). The emit order
        // at QH is head.h,g,..,a then up.h,g,..,a, so the 16-bit word MSB..LSB is
        //   [L10 L9 L8 L7 L6 L5 L4 L3  L2 L1 . . . . . .]
        // Choose a recognizable pattern: L1,L4,L7,L10 high (a "digit" spike set).
        // head (U15002): a=L3 b=L4 c=L5 d=L6 e=L7 f=L8 g=L9 h=L10
        let head_hi = ["b" /*L4*/, "e" /*L7*/, "h" /*L10*/];
        // upstream (U15001): g=L1 h=L2  (a..f unconnected on the real board)
        let up_hi = ["g" /*L1*/];
        let (_chips, mut chain) = build_165_chain(&mut circuit, &head_hi, &up_hi);

        let levels = LogicLevels::from_params(&hc165_model());
        // node_v reads the chain's input nets from the circuit's DC vsources.
        // We don't run MNA here; instead resolve each input net by its source.
        // Simplest: the build wired V_* sources at 5/0; emulate node_v by reading
        // the source value back. Build a node->volt map from the vsources.
        let mut volts: HashMap<i64, f64> = HashMap::new();
        for dev in &circuit.devices {
            if let hauksbee_ir::Device::Vsource { p, kind, .. } = dev {
                if let hauksbee_ir::SourceKind::Dc(v) = kind {
                    volts.insert(p.0 as i64, *v);
                }
            }
        }
        let node_v = |n: NodeId| volts.get(&(n.0 as i64)).copied().unwrap_or(0.0);

        // Firmware ReadOutput: PL low then high (load on the low pulse).
        let mut value: u16 = 0;
        let mut miso_level = false;
        let apply = |out: Option<((char, u8), bool)>, miso: &mut bool| {
            if let Some(((_p, _b), lvl)) = out {
                *miso = lvl;
            }
        };
        apply(chain.on_edge(('D', 4), false, &node_v, &levels), &mut miso_level); // PL low
        apply(chain.on_edge(('D', 4), true, &node_v, &levels), &mut miso_level); // PL high

        // _ReadShiftRegisterWord: 16 × { value<<=1; value|=read(MISO); pulse SCLK }
        for _ in 0..16 {
            value <<= 1;
            value |= miso_level as u16;
            apply(chain.on_edge(('B', 5), true, &node_v, &levels), &mut miso_level); // SCLK rise
            apply(chain.on_edge(('B', 5), false, &node_v, &levels), &mut miso_level); // SCLK fall
        }

        // Expected MSB-first word: L10 L9 L8 L7 L6 L5 L4 L3 | L2 L1 . . . . . .
        // highs: L4,L7,L10 on head; L1 upstream.
        // bit15=L10=1, bit14=L9=0, bit13=L8=0, bit12=L7=1, bit11=L6=0,
        // bit10=L5=0, bit9=L4=1, bit8=L3=0, bit7=L2=0, bit6=L1=1, bit5..0=0.
        let expected: u16 = (1 << 15) | (1 << 12) | (1 << 9) | (1 << 6);
        assert_eq!(
            value, expected,
            "165 readback word 0x{value:04X} should match known latch pattern 0x{expected:04X}"
        );
        // The model-level loaded_word must agree with the bit-banged readback.
        assert_eq!(chain.loaded_word(), expected, "loaded_word matches readback");
    }

    /// Regression guard: a single SCLK edge (the collapsed once-per-chunk view)
    /// cannot reproduce the full 16-bit readback. Proves the per-edge path is
    /// load-bearing for the 165 just as it is for the 595.
    #[test]
    fn hc165_collapsed_single_edge_does_not_read_full_word() {
        let mut circuit = Circuit::new();
        let (_chips, mut chain) =
            build_165_chain(&mut circuit, &["a", "h"], &["a"]);
        let levels = LogicLevels::from_params(&hc165_model());
        let node_v = |_n: NodeId| 0.0; // collapsed: never re-sample
        // One PL + one SCLK edge, no full pulse train.
        let _ = chain.on_edge(('D', 4), false, &node_v, &levels);
        let _ = chain.on_edge(('D', 4), true, &node_v, &levels);
        let before = chain.pos;
        let _ = chain.on_edge(('B', 5), true, &node_v, &levels);
        assert_eq!(chain.pos, before + 1, "a single SCLK rise advances exactly one bit");
        // With only one clocked bit you cannot have walked all 16 stages.
        assert!(chain.pos < 16, "collapsed single edge cannot shift the whole word");
    }

    /// 74HC02 NOR SR spike latch: the truth table the firmware-driven readback
    /// depends on. Idle (after reset, no spike) => Q HIGH (active-low idle, the
    /// real board's 0xFFC0). A SET pulse (SPIKE) => Q LOW and HELD low after the
    /// pulse clears. RESET_SR => Q back HIGH. This is the polarity that makes an
    /// idle board decode as NO spikes (not a 10-way tie).
    #[test]
    fn nor_latch_spike_polarity_idle_high_spike_low_held() {
        let mut circuit = Circuit::new();
        let set_n = circuit.node("SPIKE1");
        let reset_n = circuit.node("RESET_SR");
        let q_n = circuit.node("L1");
        let levels = LogicLevels {
            voh: 4.4,
            vol: 0.1,
            vih: 3.15,
            vil: 1.35,
            ro: crate::drivers::DEFAULT_RO,
        };
        let mut roles = HashMap::new();
        roles.insert("set".to_string(), set_n);
        roles.insert("reset".to_string(), reset_n);
        roles.insert("q".to_string(), q_n);
        // No real driver needed for the state check; drive into an empty map and
        // inspect latch_q directly.
        let mut latch =
            DigitalComponent::new_nor_latch("U_L1".to_string(), levels, roles, HashMap::new());

        // Drive set/reset by a node-voltage closure.
        let make_v = |set_hi: bool, reset_hi: bool| {
            move |n: NodeId| -> f64 {
                if n == set_n {
                    if set_hi { 4.5 } else { 0.0 }
                } else if n == reset_n {
                    if reset_hi { 4.5 } else { 0.0 }
                } else {
                    0.0
                }
            }
        };

        // 1. Power-on idle: Q HIGH.
        assert!(latch.latch_q, "power-on idle latch Q must be HIGH (cleared/idle)");

        // 2. Assert RESET_SR (set low): Q stays HIGH (the cleared level).
        latch.tick_nor_latch(&make_v(false, true));
        assert!(latch.latch_q, "RESET with no spike holds Q HIGH (idle)");

        // 3. Release reset, no spike: HOLD HIGH.
        latch.tick_nor_latch(&make_v(false, false));
        assert!(latch.latch_q, "idle hold keeps Q HIGH");

        // 4. A spike (SET pulse HIGH): Q goes LOW.
        latch.tick_nor_latch(&make_v(true, false));
        assert!(!latch.latch_q, "a SET pulse (spike) drives Q LOW");

        // 5. Spike clears (SET low), no reset: Q HELD LOW (the latch memory).
        latch.tick_nor_latch(&make_v(false, false));
        assert!(!latch.latch_q, "Q stays LOW after the spike clears (held by cross-couple)");

        // 6. RESET_SR pulse: Q back HIGH (idle).
        latch.tick_nor_latch(&make_v(false, true));
        assert!(latch.latch_q, "RESET_SR returns Q to HIGH (idle)");
    }
}
