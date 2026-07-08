//! Event-driven behavioral digital components.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/digital.md.
//!
//! These are NOT solved in MNA. Each step the scheduler:
//!   1. samples the input net voltages and converts them to logic levels with
//!      the part's `vih`/`vil` thresholds (with hysteresis between them);
//!   2. lets the component process clock/latch edges and update its register;
//!   3. writes the component's output logic levels back onto the analog nets
//!      through Thevenin [`PinDriver`]s (`voh`/`vol`, `ro`).
//!
//! Behaviour is DECLARATIVE: each part's model entry carries a
//! `[models.logic]` block (06-extensibility §1.1) that the generic
//! [`LogicComponent`] evaluator compiles at bind time — the old hardcoded
//! `DigitalKind::{Hc595, Hc165, Buffer, NorLatch}` enum and its per-kind tick
//! methods were deleted after a byte-exact regression proved the spec-driven
//! reimplementations identical at every edge (`tests/logic_migration.rs`).
//! A digital model WITHOUT a logic block falls back to a synthesized
//! transparent passthrough over its wired `a*`/`y*` role pairs (the old
//! `Buffer` semantics, preserved for `adc`-kind passthroughs and unmodelled
//! parts).
//!
//! What stays Rust, deliberately: the MCU-facing chain controllers
//! ([`Hc595Chain`], [`Hc165Chain`]) and their net-walking recovery
//! (`order_*_chains`). They are GPIO-integration machinery — mapping edge
//! logs and MISO responders onto daisy chains — not part behaviour; the
//! per-chip shift/latch semantics they mirror INTO the components now live
//! in the components' specs. Their chain-candidacy test is structural (a
//! part declaring a `ser` input and a `qh_serial`/`qh` output participates),
//! so the contract is data-visible role names, not a Rust enum.

use std::collections::HashMap;

use hauksbee_ir::{Circuit, NodeId};
use hauksbee_models::logic_spec::Logic;
use hauksbee_models::ModelEntry;

use crate::drivers::{PinDriver, DEFAULT_RO};
use crate::logic::{LogicCompileError, LogicComponent};

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

/// The `[models.logic]` spec of the cross-coupled NOR SR latch the binder
/// synthesizes for a 74HC02 latch pair (the Tarski spike recorder). Roles
/// `set` (active-high SET), `reset` (active-high RESET), output `q` (`qb` is
/// the internal cross-couple node, unwired on the board). Active-LOW Q
/// semantics: at reset Q is HIGH (idle), a SET pulse drives Q LOW and the
/// cross-couple HOLDS it LOW until the next RESET — so the firmware-driven
/// 74HC165 readback samples the level the real board's latch Q presents
/// (idle HIGH -> 0xFFC0, captured spike LOW). The cross-coupled `comb` pair
/// resolves by the evaluator's fixpoint machinery; `init` seeds the cleared
/// power-on state (a symmetric seed would be the classic SR metastability).
pub const NOR_LATCH_SPEC_ID: &str = "nor_sr_latch";
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

/// A single cycle-stamped GPIO output transition captured from an MCU.
///
/// This is the ordered, cycle-stamped edge event of 05 §1.1: it carries the MCU
/// cycle counter at the instant of the edge alongside the pin and its new level,
/// so a sub-µs `shiftOut` SCLK burst replays in true order and multiplicity
/// instead of collapsing to a resting level (numerical lore #8,
/// `docs/learn/tarski-saga.md` §5). Within a chunk the log preserves
/// order and multiplicity; `cycle` is exact on push backends (simavr) and the
/// coarse poll-slice time on poll backends (Renode/QEMU), flagged by
/// `Mcu::cycle_exact`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinEdge {
    /// MCU cycle counter at the instant of the edge.
    pub cycle: u64,
    /// Port letter of the pin that transitioned.
    pub port: char,
    /// Bit index within the port.
    pub bit: u8,
    /// New logic level after the transition.
    pub level: bool,
}

/// Collapse a cycle-stamped edge log into per-pin ordered `(cycle, level)`
/// waveforms, the shape the analog PWL side consumes (05 §1.1/§1.3).
///
/// The input log is append-ordered within a chunk, so each pin's resulting
/// series is already cycle-monotonic; a `(port,bit)` maps to its ordered edge
/// times so the solver can normalise a cycle to a fraction of the chunk's cycle
/// span and drive a `SourceKind::Pwl` waveform on the net that pin feeds.
pub fn pin_edges_by_pin(edges: &[PinEdge]) -> HashMap<(char, u8), Vec<(u64, bool)>> {
    let mut per_pin: HashMap<(char, u8), Vec<(u64, bool)>> = HashMap::new();
    for e in edges {
        per_pin
            .entry((e.port, e.bit))
            .or_default()
            .push((e.cycle, e.level));
    }
    per_pin
}

/// Generalized digital edge replay (05 §1.2): drain a cycle-stamped edge log and
/// micro-tick a set of GPIO-driven digital components in cycle order, one
/// micro-tick per edge-group sharing a cycle.
///
/// This is the write-path generalization of `Hc595Chain::replay` beyond the 595:
/// any [`DigitalComponent`] whose clock/data inputs come from MCU GPIO pins (not
/// from a 595 chain, not from the 165 synchronous responder) advances at edge
/// granularity here instead of being sampled once per chunk (which collapses the
/// pulse train). `pin_nets` maps an MCU GPIO `(port,bit)` to the net node it
/// drives; `high_v`/`low_v` are that MCU's rail levels so the overlaid net
/// voltage crosses the component's `vih`/`vil` thresholds. Inputs NOT driven by a
/// replayed pin read their last solved voltage from `base_volts` (case (b) of the
/// §1.2 cadence rule: analog-driven inputs only change at solve boundaries).
///
/// Returns the number of micro-ticks executed (distinct cycle groups that
/// touched a watched pin) so a caller can assert N edges produced N ordered
/// micro-ticks rather than one collapsed level.
#[allow(clippy::too_many_arguments)]
pub fn replay_components_on_edges(
    components: &mut [DigitalComponent],
    which: &[usize],
    pin_nets: &HashMap<(char, u8), NodeId>,
    edges: &[PinEdge],
    base_volts: &[f64],
    high_v: f64,
    low_v: f64,
    circuit: &mut Circuit,
) -> usize {
    if edges.is_empty() || which.is_empty() {
        return 0;
    }
    // Net-voltage overlay accumulated as edges are applied. A driven net holds
    // its last edge level until the next edge on that pin; everything else falls
    // back to the previous chunk's solved voltage.
    let mut overlay: HashMap<NodeId, f64> = HashMap::new();
    let sample = |overlay: &HashMap<NodeId, f64>, base: &[f64], n: NodeId| -> f64 {
        overlay
            .get(&n)
            .copied()
            .unwrap_or_else(|| base.get(n.0 as usize).copied().unwrap_or(0.0))
    };
    let mut microticks = 0usize;
    let mut i = 0;
    // The log is pushed in cycle order, so equal cycles are contiguous: one
    // group per distinct cycle is one micro-tick.
    while i < edges.len() {
        let c = edges[i].cycle;
        let mut touched = false;
        let mut j = i;
        while j < edges.len() && edges[j].cycle == c {
            let e = &edges[j];
            if let Some(&net) = pin_nets.get(&(e.port, e.bit)) {
                overlay.insert(net, if e.level { high_v } else { low_v });
                touched = true;
            }
            j += 1;
        }
        if touched {
            {
                let ov = &overlay;
                let node_v = |n: NodeId| sample(ov, base_volts, n);
                for &ci in which {
                    components[ci].tick(circuit, &node_v);
                }
            }
            // Propagate the ticked components' driven outputs into the overlay
            // AFTER the whole group ticked, so chips sharing a clock edge all
            // sampled the PRE-edge levels (simultaneous-clock silicon
            // semantics) and a daisy chain's qh_serial -> next ser carry works
            // at edge granularity through plain comb outputs (§1.1: chaining
            // needs nothing special).
            for &ci in which {
                let d = &components[ci];
                let Some(logic) = d.logic.as_ref() else { continue };
                for (name, level, enabled) in logic.outputs() {
                    if !enabled {
                        continue;
                    }
                    if let Some(&net) = d.roles.get(name) {
                        overlay.insert(net, d.levels.drive_volts(level));
                    }
                }
            }
            microticks += 1;
        }
        i = j;
    }
    microticks
}

/// One bound digital component: its pin→net wiring, drivers, and the
/// compiled spec evaluator holding all state.
pub struct DigitalComponent {
    pub reference: String,
    pub levels: LogicLevels,
    /// Role name -> net node it is wired to (only connected roles present).
    pub roles: HashMap<String, NodeId>,
    /// Output drivers keyed by role name.
    pub drivers: HashMap<String, PinDriver>,
    /// The compiled `[models.logic]` evaluator. `None` for an inert part (a
    /// model with no logic block and no mirrorable `a*`/`y*` role pairs):
    /// such a part keeps its stamped drivers at their initial low level,
    /// exactly what the old kind-`Buffer` fallback did when it had nothing
    /// to mirror.
    pub logic: Option<LogicComponent>,
}

/// Synthesize the transparent-passthrough spec for a digital model WITHOUT a
/// `[models.logic]` block: every wired `a<idx>` input role mirrors onto its
/// wired `y<idx>` output role (the 74HCxx buffer/gate naming). This is the
/// old kind-`Buffer` behaviour, generated as data at bind time from the
/// ACTUAL wiring so partial wiring behaves identically (an unpaired wired
/// `y*` keeps its stamped driver's initial low level; an unwired pair simply
/// does not exist). Returns `None` when there is nothing to mirror.
fn synth_passthrough_spec(roles: &HashMap<String, NodeId>) -> Option<Logic> {
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut comb = std::collections::BTreeMap::new();
    let mut names: Vec<&String> = roles.keys().collect();
    names.sort();
    for role in names {
        if let Some(idx) = role.strip_prefix('a') {
            let y = format!("y{idx}");
            if roles.contains_key(&y) {
                inputs.push(role.clone());
                comb.insert(y.clone(), role.clone());
                outputs.push(y);
            }
        }
    }
    if outputs.is_empty() {
        return None;
    }
    Some(Logic {
        inputs,
        outputs,
        comb,
        registers: Vec::new(),
        tristate: Default::default(),
        init: Default::default(),
    })
}

impl DigitalComponent {
    /// Build a digital component from its model entry and a role→node map. The
    /// caller has already stamped output [`PinDriver`]s and passes them in.
    ///
    /// The model's `[models.logic]` block is compiled here (bind time — the
    /// expressions are never re-parsed on the tick path). A model without a
    /// logic block gets the synthesized `a*`/`y*` passthrough. A model WITH a
    /// logic block that fails to compile is a hard error: the caller decides
    /// whether to skip the part loudly (nets float, lore #9) — it is never
    /// silently downgraded to a passthrough.
    pub fn new(
        reference: String,
        model: &ModelEntry,
        roles: HashMap<String, NodeId>,
        drivers: HashMap<String, PinDriver>,
    ) -> Result<Self, LogicCompileError> {
        let logic = if model.logic.is_empty() {
            synth_passthrough_spec(&roles)
                .map(|spec| LogicComponent::compile(&format!("{}_passthrough", model.id), &spec))
                .transpose()?
        } else {
            Some(LogicComponent::compile(&model.id, &model.logic)?)
        };
        Ok(DigitalComponent {
            reference,
            levels: LogicLevels::from_params(model),
            roles,
            drivers,
            logic,
        })
    }

    /// Build a NOR SR latch component (one latch = one cross-coupled gate
    /// pair). `roles` must carry `set`, `reset`, and `q`; the caller has stamped
    /// the `q` output [`PinDriver`]. Uses the supplied logic levels (the 74HC02
    /// model entry's). State initialises to the cleared idle (Q HIGH) via the
    /// spec's `init`.
    pub fn new_nor_latch(
        reference: String,
        levels: LogicLevels,
        roles: HashMap<String, NodeId>,
        drivers: HashMap<String, PinDriver>,
    ) -> Self {
        let spec: Logic =
            toml::from_str(NOR_LATCH_SPEC).expect("builtin NOR latch spec parses");
        let logic = LogicComponent::compile(NOR_LATCH_SPEC_ID, &spec)
            .expect("builtin NOR latch spec compiles");
        DigitalComponent {
            reference,
            levels,
            roles,
            drivers,
            logic: Some(logic),
        }
    }

    /// Process one scheduler tick: sample inputs (with hysteresis), advance
    /// the spec evaluator, drive outputs.
    pub fn tick(&mut self, circuit: &mut Circuit, node_v: &dyn Fn(NodeId) -> f64) {
        let Some(logic) = self.logic.as_mut() else {
            return;
        };
        let roles = &self.roles;
        let levels = self.levels;
        logic.tick(&mut |name, prev| {
            roles.get(name).map(|&n| levels.decide(node_v(n), prev))
        });
        self.drive_outputs(circuit);
    }

    /// Latch a parallel-output byte directly into the `store` register and
    /// push it onto the analog drivers, bypassing the serial shift/clock
    /// sequence. This is the model-level "the host already streamed and
    /// latched the chain" shortcut (the same end state as the edge-driven
    /// `Hc595Chain`), for harnesses that drive a 74HC595's known latched
    /// value without simulating the SPI bit-bang. Bit `i` of `byte` lands in
    /// `store[i]` (= `qa+i`). No-op for parts without a `store` register.
    pub fn latch_byte(&mut self, circuit: &mut Circuit, byte: u8) {
        let Some(logic) = self.logic.as_mut() else {
            return;
        };
        if logic.set_register("store", byte as u64) {
            logic.refresh_outputs();
            self.drive_outputs(circuit);
        }
    }

    /// Push the evaluator's current output levels and tri-state enables onto
    /// the stamped drivers.
    fn drive_outputs(&mut self, circuit: &mut Circuit) {
        let Some(logic) = self.logic.as_ref() else {
            return;
        };
        for (name, level, enabled) in logic.outputs() {
            if let Some(drv) = self.drivers.get_mut(name) {
                drv.set_enabled(circuit, enabled);
                if enabled {
                    drv.set_volts(circuit, self.levels.drive_volts(level));
                }
            }
        }
    }

    /// Re-evaluate outputs from current register/input state and drive them
    /// (the chain-controller mirror path after `set_register`).
    pub fn drive_from_registers(&mut self, circuit: &mut Circuit) {
        if let Some(logic) = self.logic.as_mut() {
            logic.refresh_outputs();
        }
        self.drive_outputs(circuit);
    }

    /// Current value of a spec register (`None`: no such register / inert).
    pub fn register(&self, name: &str) -> Option<u64> {
        self.logic.as_ref().and_then(|l| l.register(name))
    }

    /// Overwrite a spec register (chain mirror). False if absent.
    pub fn set_register(&mut self, name: &str, value: u64) -> bool {
        self.logic
            .as_mut()
            .map(|l| l.set_register(name, value))
            .unwrap_or(false)
    }

    /// Current logic level of a spec output.
    pub fn output_level(&self, name: &str) -> Option<bool> {
        self.logic.as_ref().and_then(|l| l.output_level(name))
    }

    /// Structural chain candidacy: a part declaring a `ser` serial input and
    /// a `qh_serial` cascade output participates in 74HC595-style write
    /// chains. The role names are the data-visible contract the chain
    /// controllers key on (formerly the `DigitalKind::Hc595` enum tag).
    pub fn chains_as_595(&self) -> bool {
        self.logic
            .as_ref()
            .map(|l| l.has_input("ser") && l.has_output("qh_serial"))
            .unwrap_or(false)
    }

    /// Structural chain candidacy for 74HC165-style read chains: a `ser`
    /// serial input, a `pl_n` parallel-load input, and a `qh` serial output.
    pub fn chains_as_165(&self) -> bool {
        self.logic
            .as_ref()
            .map(|l| l.has_input("ser") && l.has_input("pl_n") && l.has_output("qh"))
            .unwrap_or(false)
    }

    /// Is this a binder-synthesized NOR SR latch?
    pub fn is_nor_latch(&self) -> bool {
        self.logic
            .as_ref()
            .map(|l| l.spec_id() == NOR_LATCH_SPEC_ID)
            .unwrap_or(false)
    }

    /// True when the part has at least one clocked register (edge-replay
    /// candidacy — see the scheduler's generalized replay).
    pub fn is_sequential(&self) -> bool {
        self.logic.as_ref().map(|l| l.is_sequential()).unwrap_or(false)
    }

    /// Input pins whose edge timing matters (register clocks / resets /
    /// loads / enables / serial data), per the spec.
    pub fn sequential_pins(&self) -> Vec<&str> {
        self.logic
            .as_ref()
            .map(|l| l.sequential_pins())
            .unwrap_or_default()
    }

    /// Compact register state for frame reporting: one entry per spec
    /// register under its declared name.
    pub fn state_summary(&self) -> HashMap<String, f64> {
        let mut m = HashMap::new();
        if let Some(logic) = self.logic.as_ref() {
            for (name, value) in logic.registers() {
                m.insert(name.to_string(), value as f64);
            }
        }
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
        .filter(|(_, d)| d.chains_as_595())
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

        // The chain controller mirrors its per-chip bytes into the chips' spec
        // registers by NAME ("shift"/"store", 8 bits) — the documented contract
        // between the Rust chain fast-path and a 595-shaped [models.logic]
        // spec. A chip that chains (ser + qh_serial roles) but lacks the
        // registers would silently desynchronize from its own analog outputs,
        // so refuse loudly and leave the chain to the once-per-chunk tick.
        for &ci in &order {
            let chip = &digital[ci];
            let ok = chip
                .logic
                .as_ref()
                .map(|l| {
                    l.register_bits("shift") == Some(8) && l.register_bits("store") == Some(8)
                })
                .unwrap_or(false);
            if !ok {
                eprintln!(
                    "ERROR: 595-chain chip '{}' ({}): [models.logic] must declare 8-bit \
                     'shift' and 'store' registers to ride the edge-driven chain; \
                     falling back to once-per-chunk sampling for this chain",
                    chip.reference,
                    chip.logic.as_ref().map(|l| l.spec_id()).unwrap_or("no logic"),
                );
                return None;
            }
        }

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
            let d = &mut digital[chip_idx];
            // Mirror the chain's bytes into the chip's spec registers so frame
            // reporting and the comb outputs (qa..qh from store, qh_serial from
            // shift) see the edge-accurate state, then drive.
            d.set_register("shift", self.shift[c] as u64);
            d.set_register("store", self.latched[c] as u64);
            d.drive_from_registers(circuit);
            // Tri-state / enable the parallel-output drivers per the CHAIN's
            // OE_n level (tracked from MCU edges; the chip itself is skipped by
            // the per-chunk tick, so its own sampled tristate state is stale —
            // the chain is authoritative here). Applied AFTER the drive so the
            // chain's decision wins; qh_serial stays enabled (not OE-gated on
            // the 74HC595).
            for name in ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh"] {
                if let Some(drv) = d.drivers.get_mut(name) {
                    drv.set_enabled(circuit, outputs_enabled);
                }
            }
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
        .filter(|(_, d)| d.chains_as_165())
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

/// Which pin roles a digital component treats as outputs (gets a driver).
/// Used by the binder to decide which pins to stamp Thevenin drivers on.
/// Declarative parts answer from their `[models.logic]` outputs; parts
/// without a logic block fall back to the `y*` role convention the
/// synthesized passthrough mirrors onto (the old `Buffer` behaviour).
pub fn output_roles(model: &ModelEntry) -> Vec<String> {
    if !model.logic.is_empty() {
        return model.logic.outputs.clone();
    }
    model
        .pins
        .values()
        .filter(|r| r.starts_with('y'))
        .cloned()
        .collect()
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
            chips.push(
                DigitalComponent::new(format!("U{k}"), &model, roles, HashMap::new())
                    .expect("builtin 595 logic compiles"),
            );
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

    /// A lone (non-chained) 74HC595 wired directly to MCU GPIO, plus its
    /// `(port,bit) -> net` map, for the generalized-replay tests. SER on PB3,
    /// SRCLK on PB5, RCLK on PD6 (no daisy chain, so it is NOT an `Hc595Chain`;
    /// it is exactly the standalone GPIO-clocked shift register the generalized
    /// path exists to drive).
    fn build_standalone_595(circuit: &mut Circuit) -> (Vec<DigitalComponent>, HashMap<(char, u8), NodeId>) {
        let model = hc595_model();
        let n_ser = circuit.node("SS_SER");
        let n_srclk = circuit.node("SS_SRCLK");
        let n_rclk = circuit.node("SS_RCLK");
        let mut roles: HashMap<String, NodeId> = HashMap::new();
        roles.insert("ser".into(), n_ser);
        roles.insert("srclk".into(), n_srclk);
        roles.insert("rclk".into(), n_rclk);
        let comp = DigitalComponent::new("U0".into(), &model, roles, HashMap::new())
            .expect("builtin 595 logic compiles");
        let mut pin_nets: HashMap<(char, u8), NodeId> = HashMap::new();
        pin_nets.insert(('B', 3), n_ser);
        pin_nets.insert(('B', 5), n_srclk);
        pin_nets.insert(('D', 6), n_rclk);
        (vec![comp], pin_nets)
    }

    /// Append a cycle-stamped `shiftOut(MSBFIRST)` of `byte`, one distinct cycle
    /// per edge so each edge is its own micro-tick group.
    fn stamped_shift_out(log: &mut Vec<PinEdge>, cyc: &mut u64, byte: u8) {
        for bit in (0..8).rev() {
            let b = ((byte >> bit) & 1) == 1;
            log.push(PinEdge { cycle: *cyc, port: 'B', bit: 3, level: b });
            *cyc += 1;
            log.push(PinEdge { cycle: *cyc, port: 'B', bit: 5, level: true });
            *cyc += 1;
            log.push(PinEdge { cycle: *cyc, port: 'B', bit: 5, level: false });
            *cyc += 1;
        }
    }

    /// Reconstruct the latched byte from a 595 component's storage register
    /// (`store` bit i is qa+i).
    fn pack_out(c: &DigitalComponent) -> u8 {
        c.register("store").expect("595 store register") as u8
    }

    /// Chaining through the REAL generalized replay path (§1.1 "chaining needs
    /// nothing special"): two spec-driven 595s wired qh_serial -> ser through a
    /// shared net, driven by `replay_components_on_edges`. The overlay
    /// propagation (outputs written back AFTER each cycle-group so all chips
    /// sample pre-edge levels) is what carries the serial bit across the chip
    /// boundary; a 16-bit MSB-first stream must land PATH B (first-sent byte in
    /// the DOWNSTREAM chip).
    #[test]
    fn generalized_replay_carries_serial_across_chained_chips() {
        let model = hc595_model();
        let mut circuit = Circuit::new();
        let n_ser0 = circuit.node("SER0");
        let n_srclk = circuit.node("SRCLK");
        let n_rclk = circuit.node("RCLK");
        let n_tap = circuit.node("QHS0"); // chip0.qh_serial -> chip1.ser

        let mk = |ser: NodeId, tap: Option<NodeId>, name: &str, circuit: &mut Circuit| {
            let mut roles: HashMap<String, NodeId> = HashMap::new();
            roles.insert("ser".into(), ser);
            roles.insert("srclk".into(), n_srclk);
            roles.insert("rclk".into(), n_rclk);
            if let Some(t) = tap {
                roles.insert("qh_serial".into(), t);
            } else {
                roles.insert("qh_serial".into(), circuit.node(&format!("{name}_TAP")));
            }
            DigitalComponent::new(name.into(), &model, roles, HashMap::new())
                .expect("builtin 595 logic compiles")
        };
        let chip0 = mk(n_ser0, Some(n_tap), "U0", &mut circuit);
        let chip1 = mk(n_tap, None, "U1", &mut circuit);
        let mut comps = vec![chip0, chip1];

        let mut pin_nets: HashMap<(char, u8), NodeId> = HashMap::new();
        pin_nets.insert(('B', 3), n_ser0);
        pin_nets.insert(('B', 5), n_srclk);
        pin_nets.insert(('D', 6), n_rclk);

        // shiftOut two known bytes MSB-first, then latch — the firmware shape.
        let (first, second) = (0x9Du8, 0x3Cu8);
        let mut log: Vec<PinEdge> = Vec::new();
        let mut cyc = 0u64;
        stamped_shift_out(&mut log, &mut cyc, first);
        stamped_shift_out(&mut log, &mut cyc, second);
        log.push(PinEdge { cycle: cyc, port: 'D', bit: 6, level: true });
        cyc += 1;
        log.push(PinEdge { cycle: cyc, port: 'D', bit: 6, level: false });

        let base = vec![0.0; circuit.node_count()];
        let ticks = replay_components_on_edges(
            &mut comps, &[0, 1], &pin_nets, &log, &base, 5.0, 0.0, &mut circuit,
        );
        assert_eq!(ticks, log.len(), "one micro-tick per distinct-cycle edge");
        assert_eq!(
            pack_out(&comps[1]),
            first,
            "PATH B: the FIRST-sent byte crossed the qh_serial->ser boundary \
             into the downstream chip"
        );
        assert_eq!(pack_out(&comps[0]), second, "the second byte stays in the head chip");
    }

    /// The generalized-path proof (05 §1.2): a cycle-stamped `shiftOut` burst
    /// through the generic `replay_components_on_edges` produces N ordered
    /// micro-ticks (one per distinct-cycle edge), NOT one collapsed level, and
    /// latches the byte bit-exact. This is the headline: N edges -> N micro-ticks.
    #[test]
    fn generalized_replay_micro_ticks_in_order() {
        let mut circuit = Circuit::new();
        let (mut comps, pin_nets) = build_standalone_595(&mut circuit);
        let base = vec![0.0; circuit.node_count()];
        let which = [0usize];

        let mut log: Vec<PinEdge> = Vec::new();
        let mut cyc = 0u64;
        let byte = 0xA6u8;
        stamped_shift_out(&mut log, &mut cyc, byte);
        // RCLK latch pulse (two more distinct-cycle edges).
        log.push(PinEdge { cycle: cyc, port: 'D', bit: 6, level: true });
        cyc += 1;
        log.push(PinEdge { cycle: cyc, port: 'D', bit: 6, level: false });

        let n_edges = log.len();
        let ticks = replay_components_on_edges(
            &mut comps, &which, &pin_nets, &log, &base, 5.0, 0.0, &mut circuit,
        );
        assert_eq!(
            ticks, n_edges,
            "each distinct-cycle edge is its own micro-tick (no collapse)"
        );
        assert_eq!(
            pack_out(&comps[0]),
            byte,
            "the shiftOut burst latched byte-exact through the generalized path"
        );
    }

    /// Edges sharing one cycle are ONE micro-tick (05 §1.2: an edge-group sharing
    /// a cycle is a single micro-tick), unlike distinct-cycle edges.
    #[test]
    fn same_cycle_edges_are_one_micro_tick() {
        let mut circuit = Circuit::new();
        let (mut comps, pin_nets) = build_standalone_595(&mut circuit);
        let base = vec![0.0; circuit.node_count()];
        // SER high and SRCLK high at the SAME cycle -> one group -> one micro-tick.
        let log = vec![
            PinEdge { cycle: 5, port: 'B', bit: 3, level: true },
            PinEdge { cycle: 5, port: 'B', bit: 5, level: true },
        ];
        let ticks = replay_components_on_edges(
            &mut comps, &[0], &pin_nets, &log, &base, 5.0, 0.0, &mut circuit,
        );
        assert_eq!(ticks, 1, "two edges on one cycle collapse to a single micro-tick");
    }

    /// Cycle monotonicity per pin: `pin_edges_by_pin` yields, for each pin, an
    /// ordered `(cycle, level)` series whose cycles never decrease (the analog
    /// PWL side relies on this to normalize edge times, 05 §1.1).
    #[test]
    fn pin_edges_are_cycle_monotonic_per_pin() {
        let log = vec![
            PinEdge { cycle: 0, port: 'B', bit: 5, level: true },
            PinEdge { cycle: 1, port: 'B', bit: 3, level: true },
            PinEdge { cycle: 2, port: 'B', bit: 5, level: false },
            PinEdge { cycle: 5, port: 'B', bit: 5, level: true },
            PinEdge { cycle: 7, port: 'B', bit: 3, level: false },
        ];
        let by_pin = pin_edges_by_pin(&log);
        for (pin, series) in &by_pin {
            for w in series.windows(2) {
                assert!(
                    w[0].0 <= w[1].0,
                    "pin {pin:?} cycle series must be monotonic: {series:?}"
                );
            }
        }
        assert_eq!(by_pin[&('B', 5)].len(), 3, "PB5 saw 3 edges");
        assert_eq!(by_pin[&('B', 3)].len(), 2, "PB3 saw 2 edges");
    }

    /// Bit-identical-when-off (05 §1.6 / master doctrine §5): a SINGLE edge on a
    /// pin in the chunk yields the same digital state through the generalized
    /// replay as the old once-per-chunk collapse (which sampled the settled
    /// level). Nothing needs ordering, so the outcome is identical.
    #[test]
    fn single_edge_matches_collapsed_tick() {
        // Replay path: SER already high from a previous solve (in `base_volts`),
        // one SRCLK rising edge this chunk.
        let mut circuit = Circuit::new();
        let (mut comps, pin_nets) = build_standalone_595(&mut circuit);
        let ser_net = pin_nets[&('B', 3)];
        let mut base = vec![0.0; circuit.node_count()];
        base[ser_net.0 as usize] = 5.0;
        let log = vec![PinEdge { cycle: 10, port: 'B', bit: 5, level: true }];
        let ticks = replay_components_on_edges(
            &mut comps, &[0], &pin_nets, &log, &base, 5.0, 0.0, &mut circuit,
        );
        assert_eq!(ticks, 1);
        let replay_shift = comps[0].register("shift").expect("shift register");

        // Old collapse path: SER high and SRCLK high sampled once per chunk
        // (prev SRCLK low), the pre-change once-per-chunk `tick`.
        let mut circuit2 = Circuit::new();
        let (mut comps2, pin_nets2) = build_standalone_595(&mut circuit2);
        let ser_net2 = pin_nets2[&('B', 3)];
        let srclk_net2 = pin_nets2[&('B', 5)];
        let mut volts = vec![0.0; circuit2.node_count()];
        volts[ser_net2.0 as usize] = 5.0;
        volts[srclk_net2.0 as usize] = 5.0;
        let node_v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
        comps2[0].tick(&mut circuit2, &node_v);

        assert_eq!(
            comps2[0].register("shift"),
            Some(replay_shift),
            "single edge collapses to an identical state"
        );
        assert!(
            replay_shift & 1 == 1,
            "the one SRCLK edge shifted SER(high) into stage 0"
        );
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
                chips.push(
                    DigitalComponent::new(format!("U_{tag}{k}"), &model, roles, HashMap::new())
                        .expect("builtin 595 logic compiles"),
                );
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
        let mut chips = vec![
            DigitalComponent::new("U0".into(), &model, roles, drivers)
                .expect("builtin 595 logic compiles"),
        ];

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
            let d = DigitalComponent::new(refn.into(), &model, roles.clone(), HashMap::new())
                .expect("builtin 165 logic compiles");
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

        let q = |l: &DigitalComponent| l.output_level("q").expect("latch q output");

        // 1. Power-on idle: Q HIGH.
        assert!(q(&latch), "power-on idle latch Q must be HIGH (cleared/idle)");

        // 2. Assert RESET_SR (set low): Q stays HIGH (the cleared level).
        latch.tick(&mut circuit, &make_v(false, true));
        assert!(q(&latch), "RESET with no spike holds Q HIGH (idle)");

        // 3. Release reset, no spike: HOLD HIGH.
        latch.tick(&mut circuit, &make_v(false, false));
        assert!(q(&latch), "idle hold keeps Q HIGH");

        // 4. A spike (SET pulse HIGH): Q goes LOW.
        latch.tick(&mut circuit, &make_v(true, false));
        assert!(!q(&latch), "a SET pulse (spike) drives Q LOW");

        // 5. Spike clears (SET low), no reset: Q HELD LOW (the latch memory).
        latch.tick(&mut circuit, &make_v(false, false));
        assert!(!q(&latch), "Q stays LOW after the spike clears (held by cross-couple)");

        // 6. RESET_SR pulse: Q back HIGH (idle).
        latch.tick(&mut circuit, &make_v(false, true));
        assert!(q(&latch), "RESET_SR returns Q to HIGH (idle)");
    }
}
