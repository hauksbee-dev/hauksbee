//! BYTE-EXACT regression: the four legacy `DigitalKind` implementations
//! (`Hc595`, `Hc165`, `Buffer`, `NorLatch`) versus their `[models.logic]`
//! spec-driven reimplementations on the generic [`LogicComponent`] evaluator.
//!
//! This is the §1.2 migration proof (the same method that proved the MCP4728
//! write side): BOTH implementations are driven through IDENTICAL edge
//! sequences — one pin transition per tick, the granularity the edge-replay
//! path delivers — and their register states AND driven-output voltages are
//! compared AT EVERY EDGE, not just at the end. Only after this passes for
//! all four kinds is the old enum deleted.
//!
//! ## Deliberately excluded corners (documented, not hidden)
//!
//! * **SRCLK rising while SRCLR_n is HELD low (74HC595).** The legacy
//!   `tick_595` cleared the register and THEN shifted within the same
//!   sample, so a clock during held-clear shifted SER into a cleared
//!   register. The TI SN74HC595 function table (SCLS041I, "SRCLR L, SRCLK X
//!   -> shift register clear") says the register stays cleared — which is
//!   what the edge-driven `Hc595Chain::replay` already implemented and what
//!   the spec's reset-dominant semantics implement. The corner is a legacy
//!   modeling artifact no firmware sequence exercises (nothing clocks a part
//!   it is holding in reset); sequences here pulse SRCLR between bursts.
//! * **Simultaneous SET+RESET release on the NOR latch.** Physically a race
//!   (silicon leaves the settled state undefined); the legacy Jacobi loop
//!   silently exited on an oscillating non-fixpoint for this input. The
//!   sequences release the two lines on separate edges, which is defined and
//!   identical in both implementations.
//! * **74HC165 clocked-shift direction.** The legacy `tick_165` shifted
//!   AWAY from QH (`shift_reg[i] = shift_reg[i+1]` with SER entering at the
//!   QH end), so QH read SER after one clock — inverted from the TI
//!   SN74HC165 datasheet (SCLS116, "the data is shifted toward the QH
//!   output": QH shows H, then G, ...). The real read path (`Hc165Chain` +
//!   responder, which this migration does NOT touch) implements the correct
//!   direction; nothing in the test corpus exercises `tick_165`'s clocked
//!   shifts. The regression below proves the evaluator reproduces the legacy
//!   semantics byte-exactly using a `shift_right` spec; the SHIPPING 74HC165
//!   spec uses the silicon-correct `shift_left` with its own datasheet test.

use std::collections::HashMap;

use hauksbee_engine::digital::{DigitalComponent, Hc595Chain, LogicLevels};
use hauksbee_engine::drivers::{PinDriver, DEFAULT_RO};
use hauksbee_engine::logic::LogicComponent;
use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_models::logic_spec::Logic;
use hauksbee_models::{ComponentQuery, ModelLibrary};

// ─────────────────────────────────────────────────────────────────────────────
// Harness
// ─────────────────────────────────────────────────────────────────────────────

/// Read back the voltage a stamped [`PinDriver`] is currently pushing.
fn driver_volts(circuit: &Circuit, drv: &PinDriver) -> f64 {
    match &circuit.devices[drv.vsource.0 as usize] {
        Device::Vsource {
            kind: SourceKind::Dc(v),
            ..
        } => *v,
        other => panic!("driver vsource slot holds {other:?}"),
    }
}

/// The legacy side of one comparison: a real [`DigitalComponent`] with
/// drivers stamped into a circuit, plus the pin-state map that backs its
/// node-voltage sampling.
struct LegacyRig {
    circuit: Circuit,
    comp: DigitalComponent,
    /// pin name -> node
    nets: HashMap<String, NodeId>,
    /// pin name -> current level
    levels: HashMap<String, bool>,
    logic_levels: LogicLevels,
}

impl LegacyRig {
    /// Build a legacy component: `inputs` become wired roles the harness
    /// drives; `outputs` get stamped [`PinDriver`]s.
    fn new(
        model: &hauksbee_models::ModelEntry,
        inputs: &[&str],
        outputs: &[&str],
        nor_latch: bool,
    ) -> Self {
        let mut circuit = Circuit::new();
        let mut nets = HashMap::new();
        let mut roles = HashMap::new();
        for p in inputs {
            let n = circuit.node(&p.to_uppercase());
            nets.insert(p.to_string(), n);
            roles.insert(p.to_string(), n);
        }
        let mut drivers = HashMap::new();
        for p in outputs {
            let n = circuit.node(&p.to_uppercase());
            nets.insert(p.to_string(), n);
            roles.insert(p.to_string(), n);
            let drv = PinDriver::stamp(&mut circuit, n, p, &format!("U_{p}"), DEFAULT_RO);
            drivers.insert(p.to_string(), drv);
        }
        let logic_levels = LogicLevels::from_params(model);
        let comp = if nor_latch {
            DigitalComponent::new_nor_latch("U_OLD".into(), logic_levels, roles, drivers)
        } else {
            DigitalComponent::new("U_OLD".into(), model, roles, drivers)
        };
        LegacyRig {
            circuit,
            comp,
            nets,
            levels: HashMap::new(),
            logic_levels,
        }
    }

    /// Apply one pin edge and tick (the legacy per-sample cadence).
    fn edge(&mut self, pin: &str, high: bool) {
        self.levels.insert(pin.to_string(), high);
        let volts: HashMap<i64, f64> = self
            .levels
            .iter()
            .filter_map(|(p, &h)| {
                self.nets
                    .get(p)
                    .map(|n| (n.0 as i64, if h { 5.0 } else { 0.0 }))
            })
            .collect();
        let node_v = move |n: NodeId| volts.get(&(n.0 as i64)).copied().unwrap_or(0.0);
        self.comp.tick(&mut self.circuit, &node_v);
    }

    fn out_volts(&self, pin: &str) -> f64 {
        driver_volts(&self.circuit, &self.comp.drivers[pin])
    }

    fn pack(&self, bits: &[bool]) -> u64 {
        bits.iter()
            .enumerate()
            .fold(0u64, |acc, (i, &b)| acc | ((b as u64) << i))
    }

    fn shift_reg(&self) -> u64 {
        self.pack(&self.comp.shift_reg)
    }

    fn out_reg(&self) -> u64 {
        self.pack(&self.comp.out_reg)
    }
}

/// The new side: a compiled [`LogicComponent`] sampled with the SAME
/// threshold/hysteresis decision the legacy side uses.
struct SpecRig {
    lc: LogicComponent,
    levels: HashMap<String, bool>,
    logic_levels: LogicLevels,
    /// pins the legacy rig wired (everything else samples as unwired).
    wired: Vec<String>,
}

impl SpecRig {
    fn new(spec_toml: &str, logic_levels: LogicLevels, wired: &[&str]) -> Self {
        let logic: Logic = toml::from_str(spec_toml).expect("spec TOML parses");
        let lc = LogicComponent::compile("regression", &logic).expect("spec compiles");
        SpecRig {
            lc,
            levels: HashMap::new(),
            logic_levels,
            wired: wired.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn edge(&mut self, pin: &str, high: bool) {
        self.levels.insert(pin.to_string(), high);
        let levels = self.levels.clone();
        let wired = self.wired.clone();
        let ll = self.logic_levels;
        self.lc.tick(&mut |name, prev| {
            if !wired.iter().any(|w| w == name) {
                return None;
            }
            let v = levels.get(name).map(|&h| if h { 5.0 } else { 0.0 }).unwrap_or(0.0);
            Some(ll.decide(v, prev))
        });
    }

    fn out_volts(&self, pin: &str) -> f64 {
        self.logic_levels
            .drive_volts(self.lc.output_level(pin).expect("declared output"))
    }
}

/// One edge step of a comparison sequence.
type EdgeSeq = Vec<(&'static str, bool)>;

/// Drive both rigs through the same edge sequence, comparing register values
/// and output voltages at EVERY edge. `regs` maps (legacy accessor) to (spec
/// register name); `outs` lists the output pins to compare. Prints a compact
/// trajectory so the proof is inspectable, not just green.
#[allow(clippy::too_many_arguments)]
fn run_locked(
    tag: &str,
    legacy: &mut LegacyRig,
    spec: &mut SpecRig,
    seq: &[(&str, bool)],
    regs: &[(&str, fn(&LegacyRig) -> u64)],
    outs: &[&str],
    print_every: usize,
) {
    for (i, &(pin, high)) in seq.iter().enumerate() {
        legacy.edge(pin, high);
        spec.edge(pin, high);

        let mut reg_desc = String::new();
        for (name, get) in regs {
            let old = get(legacy);
            let new = spec.lc.register(name).expect("spec register");
            assert_eq!(
                old, new,
                "[{tag}] edge {i} ({pin}={high}): register '{name}' diverged: \
                 legacy {old:#04x} vs spec {new:#04x}"
            );
            reg_desc.push_str(&format!(" {name}={old:02x}"));
        }
        let mut out_desc = String::new();
        for o in outs {
            let old = legacy.out_volts(o);
            let new = spec.out_volts(o);
            assert_eq!(
                old, new,
                "[{tag}] edge {i} ({pin}={high}): output '{o}' diverged: \
                 legacy {old}V vs spec {new}V"
            );
            out_desc.push_str(&format!(" {o}={}", if old >= 2.5 { 1 } else { 0 }));
        }
        if i % print_every == 0 || i + 1 == seq.len() {
            println!("[{tag}] edge {i:03} {pin}={} match:{reg_desc}{out_desc}", high as u8);
        }
    }
    println!("[{tag}] {} edges compared byte-exact", seq.len());
}

fn builtin(id: &str) -> hauksbee_models::ModelEntry {
    let lib = ModelLibrary::builtin();
    let q = ComponentQuery::new(None, Some(id.to_string()), None);
    lib.resolve(&q).model.unwrap_or_else(|| panic!("builtin {id} model"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Specs under test (the [models.logic] reimplementations)
// ─────────────────────────────────────────────────────────────────────────────

const HC595_SPEC: &str = r#"
inputs  = ["ser", "srclk", "rclk", "srclr_n", "oe_n"]
outputs = ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh", "qh_serial"]

[[register]]
name = "shift"
bits = 8
clock = { pin = "srclk", edge = "rising" }
reset = { pin = "srclr_n", active = "low", value = 0 }
op = "shift_left"
data_in = "ser"

[[register]]
name = "store"
bits = 8
clock = { pin = "rclk", edge = "rising" }
op = "load"
data_in = "shift"

[comb]
"qa" = "store[0]"
"qb" = "store[1]"
"qc" = "store[2]"
"qd" = "store[3]"
"qe" = "store[4]"
"qf" = "store[5]"
"qg" = "store[6]"
"qh" = "store[7]"
"qh_serial" = "shift[7]"

[tristate]
"qa..qh" = { enable = "oe_n", active = "low" }
"#;

/// LEGACY-equivalent 74HC165: `shift_right` reproduces `tick_165`'s inverted
/// shift direction byte-exactly (see the module docs — the shipping spec uses
/// the silicon-correct `shift_left`).
const HC165_LEGACY_SPEC: &str = r#"
inputs  = ["pl_n", "clk", "clk_inh", "ser", "a", "b", "c", "d", "e", "f", "g", "h"]
outputs = ["qh", "qh_n"]

[[register]]
name = "reg"
bits = 8
clock = { pin = "clk", edge = "rising" }
clock_enable = { pin = "clk_inh", active = "low" }
op = "shift_right"
data_in = "ser"
load = { pin = "pl_n", active = "low", data = ["a", "b", "c", "d", "e", "f", "g", "h"] }

[comb]
"qh" = "reg[7]"
"qh_n" = "!reg[7]"
"#;

const BUFFER_SPEC: &str = r#"
inputs  = ["a1", "a2", "a3"]
outputs = ["y1", "y2", "y3"]

[comb]
"y1" = "a1"
"y2" = "a2"
"y3" = "a3"
"#;

const NOR_LATCH_SPEC: &str = r#"
inputs  = ["set", "reset"]
outputs = ["q", "qb"]

[comb]
"q"  = "!(set | qb)"
"qb" = "!(reset | q)"

[init]
"q" = 1
"qb" = 0
"#;

// ─────────────────────────────────────────────────────────────────────────────
// Edge-sequence builders (realistic firmware waveforms)
// ─────────────────────────────────────────────────────────────────────────────

/// shiftOut(MSBFIRST): per bit set SER, pulse SRCLK high/low.
fn seq_shift_out(seq: &mut EdgeSeq, byte: u8) {
    for bit in (0..8).rev() {
        seq.push(("ser", (byte >> bit) & 1 == 1));
        seq.push(("srclk", true));
        seq.push(("srclk", false));
    }
}

fn hc595_sequence() -> EdgeSeq {
    let mut seq: EdgeSeq = Vec::new();
    // Firmware init: release clear, enable outputs, idle clocks.
    seq.push(("srclr_n", true));
    seq.push(("oe_n", false));
    seq.push(("srclk", false));
    seq.push(("rclk", false));
    // Burst 1: 0xA6, latch.
    seq_shift_out(&mut seq, 0xA6);
    seq.push(("rclk", true));
    seq.push(("rclk", false));
    // Burst 2: 0x5A shifted but NOT latched (store must hold 0xA6 throughout).
    seq_shift_out(&mut seq, 0x5A);
    // SER wiggles without clocks: nothing may move.
    seq.push(("ser", true));
    seq.push(("ser", false));
    seq.push(("ser", true));
    // Clear pulse between bursts (no clocks while held — see module docs).
    seq.push(("srclr_n", false));
    seq.push(("srclr_n", true));
    // Latch the cleared shift register.
    seq.push(("rclk", true));
    seq.push(("rclk", false));
    // Burst 3: 0x0F, latch, then a second redundant latch pulse.
    seq_shift_out(&mut seq, 0x0F);
    seq.push(("rclk", true));
    seq.push(("rclk", false));
    seq.push(("rclk", true));
    seq.push(("rclk", false));
    seq
}

fn hc165_sequence() -> EdgeSeq {
    let mut seq: EdgeSeq = Vec::new();
    // Init: PL released, clock idle, inhibit released, SER low.
    seq.push(("pl_n", true));
    seq.push(("clk", false));
    seq.push(("clk_inh", false));
    seq.push(("ser", false));
    // Latch inputs pattern A (a, c, f high).
    for (p, h) in [("a", true), ("b", false), ("c", true), ("d", false),
                   ("e", false), ("f", true), ("g", false), ("h", false)] {
        seq.push((p, h));
    }
    // PL pulse (load), then 8 clocks with SER low.
    seq.push(("pl_n", false));
    seq.push(("pl_n", true));
    for _ in 0..8 {
        seq.push(("clk", true));
        seq.push(("clk", false));
    }
    // Pattern B while PL is high (must NOT load), then a real reload.
    for (p, h) in [("a", false), ("h", true), ("g", true)] {
        seq.push((p, h));
    }
    seq.push(("pl_n", false));
    seq.push(("pl_n", true));
    // 4 clocks with SER high, then 4 with CLK_INH asserted (must not shift).
    seq.push(("ser", true));
    for _ in 0..4 {
        seq.push(("clk", true));
        seq.push(("clk", false));
    }
    seq.push(("clk_inh", true));
    for _ in 0..4 {
        seq.push(("clk", true));
        seq.push(("clk", false));
    }
    seq.push(("clk_inh", false));
    seq
}

fn nor_latch_sequence() -> EdgeSeq {
    vec![
        // Idle.
        ("set", false),
        ("reset", false),
        // RESET pulse (firmware RESET_SR): idle stays HIGH.
        ("reset", true),
        ("reset", false),
        // Spike: SET pulse -> Q LOW, held after release.
        ("set", true),
        ("set", false),
        // Second spike while already latched: no change.
        ("set", true),
        ("set", false),
        // RESET pulse: back to idle HIGH.
        ("reset", true),
        ("reset", false),
        // Both asserted (defined: Q LOW), then released on SEPARATE edges.
        ("set", true),
        ("reset", true),
        ("reset", false),
        ("set", false),
        // Final reset.
        ("reset", true),
        ("reset", false),
    ]
}

fn buffer_sequence() -> EdgeSeq {
    let mut seq: EdgeSeq = Vec::new();
    for pat in [0b001u8, 0b011, 0b010, 0b110, 0b111, 0b101, 0b000, 0b100] {
        seq.push(("a1", pat & 1 != 0));
        seq.push(("a2", pat & 2 != 0));
        seq.push(("a3", pat & 4 != 0));
    }
    seq
}

// ─────────────────────────────────────────────────────────────────────────────
// The four byte-exact regressions
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn hc595_byte_exact_against_legacy() {
    let model = builtin("74HC595");
    let inputs = ["ser", "srclk", "rclk", "srclr_n", "oe_n"];
    let outputs = ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh", "qh_serial"];
    let mut legacy = LegacyRig::new(&model, &inputs, &outputs, false);
    let wired: Vec<&str> = inputs.iter().chain(outputs.iter()).copied().collect();
    let mut spec = SpecRig::new(HC595_SPEC, legacy.logic_levels, &wired);

    run_locked(
        "hc595",
        &mut legacy,
        &mut spec,
        &hc595_sequence(),
        &[
            ("shift", |l: &LegacyRig| l.shift_reg()),
            ("store", |l: &LegacyRig| l.out_reg()),
        ],
        &outputs,
        8,
    );
    // Non-vacuity: the sequence latched a real value at some point.
    assert_eq!(spec.lc.register("store"), Some(0x0F), "final latched byte");
}

#[test]
fn hc165_byte_exact_against_legacy() {
    let model = builtin("74HC165");
    let inputs = [
        "pl_n", "clk", "clk_inh", "ser", "a", "b", "c", "d", "e", "f", "g", "h",
    ];
    let outputs = ["qh", "qh_n"];
    let mut legacy = LegacyRig::new(&model, &inputs, &outputs, false);
    let wired: Vec<&str> = inputs.iter().chain(outputs.iter()).copied().collect();
    let mut spec = SpecRig::new(HC165_LEGACY_SPEC, legacy.logic_levels, &wired);

    run_locked(
        "hc165",
        &mut legacy,
        &mut spec,
        &hc165_sequence(),
        &[("reg", |l: &LegacyRig| l.shift_reg())],
        &outputs,
        8,
    );
}

#[test]
fn buffer_byte_exact_against_legacy() {
    // Any digital model without 595/165 in the id classifies as Buffer; a
    // synthetic entry keeps the role names explicit.
    let mut model = builtin("74HC595");
    model.id = "test_buffer".into();
    let inputs = ["a1", "a2", "a3"];
    let outputs = ["y1", "y2", "y3"];
    let mut legacy = LegacyRig::new(&model, &inputs, &outputs, false);
    let wired: Vec<&str> = inputs.iter().chain(outputs.iter()).copied().collect();
    let mut spec = SpecRig::new(BUFFER_SPEC, legacy.logic_levels, &wired);

    run_locked(
        "buffer",
        &mut legacy,
        &mut spec,
        &buffer_sequence(),
        &[],
        &outputs,
        4,
    );
}

#[test]
fn nor_latch_byte_exact_against_legacy() {
    let model = builtin("74HC595"); // levels only; the latch takes them directly
    let levels = LogicLevels::from_params(&model);
    let inputs = ["set", "reset"];
    let outputs = ["q"];
    let mut legacy = LegacyRig::new(&model, &inputs, &outputs, true);
    legacy.logic_levels = levels;
    let mut spec = SpecRig::new(NOR_LATCH_SPEC, levels, &["set", "reset", "q"]);

    run_locked(
        "nor_latch",
        &mut legacy,
        &mut spec,
        &nor_latch_sequence(),
        &[],
        &outputs,
        1,
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Multi-chip daisy chain: qh_serial -> next ser falls out of comb (§1.1)
// ─────────────────────────────────────────────────────────────────────────────

/// A 4-chip chain of spec-driven components, wired qh_serial[k] -> ser[k+1]
/// through emulated nets with simultaneous-clock semantics (all chips sample
/// PRE-edge outputs, then outputs propagate — exactly the overlay semantics
/// of the edge-replay path). Compared per SRCLK edge against the legacy
/// [`Hc595Chain`] controller, the proven reference for PATH B ordering.
#[test]
fn spec_chain_matches_hc595chain_reference() {
    const N: usize = 4;
    let model = builtin("74HC595");
    let levels = LogicLevels::from_params(&model);
    let weights: [u8; N] = [0x11, 0x22, 0x33, 0x44];

    // New side: N spec chips + a net map for the serial taps.
    let logic: Logic = toml::from_str(HC595_SPEC).unwrap();
    let mut chips: Vec<LogicComponent> = (0..N)
        .map(|_| LogicComponent::compile("chain", &logic).expect("compiles"))
        .collect();
    // Net levels: broadcast controls + per-chip serial-in.
    let mut ctrl: HashMap<&str, bool> = HashMap::new();
    let mut ser_in = vec![false; N]; // chip k's SER net level

    // Legacy reference: the Hc595Chain controller over the same edge log.
    let mut circuit = Circuit::new();
    let mut ref_chips: Vec<DigitalComponent> = Vec::new();
    let mut prev_qh: Option<NodeId> = None;
    let n_srclk = circuit.node("SRCLK");
    let n_rclk = circuit.node("RCLK");
    let n_srclr = circuit.node("SRCLR");
    let n_ser0 = circuit.node("SER0");
    for k in 0..N {
        let mut roles = HashMap::new();
        roles.insert("srclk".to_string(), n_srclk);
        roles.insert("rclk".to_string(), n_rclk);
        roles.insert("srclr_n".to_string(), n_srclr);
        roles.insert("ser".to_string(), prev_qh.unwrap_or(n_ser0));
        let qh = circuit.node(&format!("QHS{k}"));
        roles.insert("qh_serial".to_string(), qh);
        ref_chips.push(DigitalComponent::new(
            format!("U{k}"),
            &model,
            roles,
            HashMap::new(),
        ));
        prev_qh = Some(qh);
    }
    let mut gpio: HashMap<i64, (char, u8)> = HashMap::new();
    gpio.insert(n_srclk.0 as i64, ('B', 5));
    gpio.insert(n_rclk.0 as i64, ('D', 6));
    gpio.insert(n_srclr.0 as i64, ('C', 3));
    gpio.insert(n_ser0.0 as i64, ('B', 3));
    let order = hauksbee_engine::digital::order_595_chain(&ref_chips);
    let mut reference =
        Hc595Chain::build(&ref_chips, order, &gpio).expect("reference chain binds");

    // The edge log: release clear, shiftOut all four weights, latch.
    let mut log: Vec<(&str, bool)> = vec![("srclr_n", true)];
    for &b in &weights {
        for bit in (0..8).rev() {
            log.push(("ser", (b >> bit) & 1 == 1));
            log.push(("srclk", true));
            log.push(("srclk", false));
        }
    }
    log.push(("rclk", true));
    log.push(("rclk", false));

    for (i, &(pin, high)) in log.iter().enumerate() {
        // Legacy reference consumes MCU (port,bit) edges.
        let mcu_pin = match pin {
            "srclk" => ('B', 5),
            "rclk" => ('D', 6),
            "srclr_n" => ('C', 3),
            "ser" => ('B', 3),
            _ => unreachable!(),
        };
        reference.replay(&[(mcu_pin.0, mcu_pin.1, high)]);

        // New side: apply the edge to the shared control map / head SER, tick
        // every chip against PRE-edge serial nets, THEN propagate the taps.
        if pin == "ser" {
            ser_in[0] = high;
        } else {
            ctrl.insert(pin, high);
        }
        let pre_ser = ser_in.clone();
        for (k, chip) in chips.iter_mut().enumerate() {
            let ctrl_snapshot = ctrl.clone();
            let ser_level = pre_ser[k];
            chip.tick(&mut |name, prev| {
                let v = match name {
                    "ser" => ser_level,
                    "oe_n" => return None, // unwired, tied enabled
                    other => ctrl_snapshot.get(other).copied().unwrap_or(false),
                };
                Some(levels.decide(if v { 5.0 } else { 0.0 }, prev))
            });
        }
        for k in 0..N - 1 {
            ser_in[k + 1] = chips[k].output_level("qh_serial").expect("tap");
        }

        // Byte-exact at every edge: each chip's shift and store versus the
        // reference controller's per-chip bytes.
        for k in 0..N {
            assert_eq!(
                chips[k].register("shift"),
                Some(reference.shift[k] as u64),
                "edge {i}: chip {k} shift register diverged from Hc595Chain"
            );
            assert_eq!(
                chips[k].register("store"),
                Some(reference.latched[k] as u64),
                "edge {i}: chip {k} storage register diverged from Hc595Chain"
            );
        }
    }

    // PATH B: first-sent byte lands in the LAST chip.
    for p in 0..N {
        assert_eq!(
            chips[p].register("store"),
            Some(weights[N - 1 - p] as u64),
            "chain position {p} latches weights[{}]",
            N - 1 - p
        );
    }
    println!(
        "[chain] {} edges x {N} chips compared byte-exact against Hc595Chain; \
         final latched: {:?}",
        log.len(),
        chips
            .iter()
            .map(|c| c.register("store").unwrap())
            .collect::<Vec<_>>()
    );
}
