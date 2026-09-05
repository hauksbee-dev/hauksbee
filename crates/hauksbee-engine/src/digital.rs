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
//! [`LogicComponent`] evaluator compiles at bind time. No part's shift, latch
//! or gate behaviour is hardcoded in Rust; the 74HC595, 74HC165, buffer and
//! NOR-latch specs are pinned edge by edge against goldens in
//! `tests/logic_migration.rs`. A digital model WITHOUT a logic block falls
//! back to a synthesized transparent passthrough over its wired `a*`/`y*`
//! role pairs (plain buffer semantics, which is what `adc`-kind passthroughs
//! and unmodelled parts want).
//!
//! What stays Rust, deliberately: the MCU-facing chain controllers
//! ([`Hc595Chain`], [`Hc165Chain`]) and their net-walking recovery
//! (`order_*_chains`). They are GPIO-integration machinery, mapping edge
//! logs and MISO responders onto daisy chains, not part behaviour; the
//! per-chip shift/latch semantics they mirror INTO the components now live
//! in the components' specs. Their chain-candidacy test is structural (a
//! part declaring a `ser` input and a `qh_serial`/`qh` output participates),
//! so the contract is data-visible role names, not a Rust enum.

use std::collections::HashMap;

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId, SourceKind};
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
/// cross-couple HOLDS it LOW until the next RESET, so the firmware-driven
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
/// The ordered, cycle-stamped edge event: it carries the MCU
/// cycle counter at the instant of the edge alongside the pin and its new level,
/// so a sub-µs `shiftOut` SCLK burst replays in true order and multiplicity
/// instead of collapsing to a resting level. Within a chunk the log preserves
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
/// waveforms, the shape the analog PWL side consumes.
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

/// Generalized digital edge replay: drain a cycle-stamped edge log and
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
                let Some(logic) = d.logic.as_ref() else {
                    continue;
                };
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

/// VCC supply-current draw of one digital package: a controllable `Isource`
/// stamped from the part's VCC pin net to its GND pin net, refreshed once per
/// scheduler chunk by [`DigitalComponent::update_supply`].
///
/// The Thevenin output [`PinDriver`]s are referenced to GROUND, not to the
/// part's VCC net, so a switching output moves charge that the supply rail
/// never sees, a metering shunt in series with VCC reads zero forever. This
/// leg is what the shunt sees. Two terms, both defaulting to zero, so a model
/// without the params draws nothing:
///
/// - **static** (`supply_static_ua`, µA): the datasheet quiescent ICC per
///   package, drawn whenever the rail is up.
/// - **dynamic** (`supply_cpd_pf`, pF): charge `Q = Cpd_eff · VCC` per output
///   transition, converted to a current pulse averaged over the chunk it
///   occurred in (`I = n · Cpd_eff · VCC / dt`). `Cpd_eff` plays the role of
///   `Cpd + C_L` in the standard CMOS dissipation formula
///   `P = (Cpd + C_L) · VCC² · f`: the model's internal power-dissipation
///   capacitance plus whatever load estimate the operator folds in.
///
/// Deliberately NOT modelled: crowbar/shoot-through current during the output
/// transition. It lasts nanoseconds; the solver evaluates in 100 µs chunks, so
/// it cannot be resolved as a waveform, and folding its charge into `Cpd_eff`
/// is the standard datasheet convention anyway (Cpd is MEASURED with the
/// crowbar term included).
///
/// Transitions are counted in `drive_outputs`, the single choke point every
/// output level change passes through (per-chunk tick, chain `apply`,
/// `latch_byte`). Sub-chunk glitches that never reach a driver are invisible
/// here, the same tick-quantisation contract the rest of the digital layer
/// already has.
#[derive(Debug, Clone)]
pub struct SupplyDraw {
    /// The controllable `Isource` (p = VCC net, n = GND net) in the circuit.
    pub isource: DeviceId,
    /// The VCC pin's net node, read each chunk for the actual rail voltage.
    pub vcc: NodeId,
    /// Static quiescent draw while powered (amps).
    pub static_a: f64,
    /// Effective switched capacitance per output transition (farads).
    pub cpd_f: f64,
    /// Output transitions accumulated since the last `update_supply` drain.
    pub transitions: u32,
    /// Last driven level per output role, for transition detection.
    last_levels: HashMap<String, bool>,
}

impl SupplyDraw {
    /// Rail voltage below which the part is treated as unpowered: no static
    /// draw, no switching charge. Keeps the constant-current leg from dragging
    /// an unpowered rail negative during supply ramp.
    pub const POWERED_THRESHOLD_V: f64 = 1.0;

    /// Stamp a zero-valued controllable `Isource` from `vcc` to `gnd` and
    /// return the handle. `static_ua`/`cpd_pf` are the model params verbatim
    /// (µA / pF); conversion to SI happens here.
    pub fn stamp(
        circuit: &mut Circuit,
        vcc: NodeId,
        gnd: NodeId,
        tag: &str,
        static_ua: f64,
        cpd_pf: f64,
    ) -> Self {
        let isource = circuit.add(Device::Isource {
            name: format!("Iq_{tag}"),
            p: vcc,
            n: gnd,
            kind: SourceKind::Dc(0.0),
        });
        SupplyDraw {
            isource,
            vcc,
            static_a: static_ua * 1e-6,
            cpd_f: cpd_pf * 1e-12,
            transitions: 0,
            last_levels: HashMap::new(),
        }
    }
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
    /// such a part keeps its stamped drivers at their initial low level.
    pub logic: Option<LogicComponent>,
    /// VCC supply draw (`None` for models without supply params: no leg is
    /// stamped and the package draws no rail current).
    pub supply: Option<SupplyDraw>,
}

/// Synthesize the transparent-passthrough spec for a digital model WITHOUT a
/// `[models.logic]` block: every wired `a<idx>` input role mirrors onto its
/// wired `y<idx>` output role (the 74HCxx buffer/gate naming). Plain buffer
/// behaviour, generated as data at bind time from the ACTUAL wiring so
/// partial wiring behaves sanely (an unpaired wired
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
        ..Default::default()
    })
}

impl DigitalComponent {
    /// Build a digital component from its model entry and a role→node map. The
    /// caller has already stamped output [`PinDriver`]s and passes them in.
    ///
    /// The model's `[models.logic]` block is compiled here (bind time; the
    /// expressions are never re-parsed on the tick path). A model without a
    /// logic block gets the synthesized `a*`/`y*` passthrough. A model WITH a
    /// logic block that fails to compile is a hard error: the caller decides
    /// whether to skip the part loudly (its nets then float); it is never
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
            supply: None,
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
        let spec: Logic = toml::from_str(NOR_LATCH_SPEC).expect("builtin NOR latch spec parses");
        let logic = LogicComponent::compile(NOR_LATCH_SPEC_ID, &spec)
            .expect("builtin NOR latch spec compiles");
        DigitalComponent {
            reference,
            levels,
            roles,
            drivers,
            logic: Some(logic),
            supply: None,
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
        logic.tick(&mut |name, prev| roles.get(name).map(|&n| levels.decide(node_v(n), prev)));
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
    /// the stamped drivers. Every output level change on every path (per-chunk
    /// tick, edge-driven chain `apply`, `latch_byte`) funnels through here, so
    /// this is also where supply-draw transition counting lives: a level
    /// change on a driven output role increments the accumulator that
    /// [`update_supply`](Self::update_supply) drains once per chunk. Counted
    /// whether or not the driver is currently tri-stated, the part's internal
    /// nodes switch (and draw Cpd charge) either way.
    fn drive_outputs(&mut self, circuit: &mut Circuit) {
        let Some(logic) = self.logic.as_ref() else {
            return;
        };
        for (name, level, enabled) in logic.outputs() {
            if let Some(drv) = self.drivers.get_mut(name) {
                if let Some(s) = self.supply.as_mut() {
                    if let Some(prev) = s.last_levels.insert(name.to_string(), level) {
                        if prev != level {
                            s.transitions += 1;
                        }
                    }
                }
                drv.set_enabled(circuit, enabled);
                if enabled {
                    drv.set_volts(circuit, self.levels.drive_volts(level));
                }
            }
        }
    }

    /// Refresh the part's VCC supply draw for the coming chunk: drain the
    /// transition accumulator and set the `Isource` to
    /// `static + n · Cpd_eff · VCC / dt`. Called once per scheduler chunk for
    /// EVERY digital component (including chain-owned chips that the per-chunk
    /// tick skips, their transitions were accumulated on the edge path).
    /// No-op for parts without a supply leg. While the rail reads below
    /// [`SupplyDraw::POWERED_THRESHOLD_V`] the draw is zero.
    pub fn update_supply(
        &mut self,
        circuit: &mut Circuit,
        dt: f64,
        node_v: &dyn Fn(NodeId) -> f64,
    ) {
        let Some(s) = self.supply.as_mut() else {
            return;
        };
        let n = std::mem::take(&mut s.transitions) as f64;
        let vcc_v = node_v(s.vcc);
        let amps = if vcc_v >= SupplyDraw::POWERED_THRESHOLD_V && dt > 0.0 {
            s.static_a + n * s.cpd_f * vcc_v / dt
        } else {
            0.0
        };
        if let Some(Device::Isource { kind, .. }) = circuit.devices.get_mut(s.isource.0 as usize) {
            *kind = SourceKind::Dc(amps);
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
    /// controllers key on, so a new model joins a chain by declaring the
    /// roles, with no Rust change.
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
    /// candidacy, see the scheduler's generalized replay).
    pub fn is_sequential(&self) -> bool {
        self.logic
            .as_ref()
            .map(|l| l.is_sequential())
            .unwrap_or(false)
    }

    /// Compiled parallel-memory ports, if this declarative part owns any.
    pub(crate) fn memory_ports(&self) -> Vec<crate::logic::ParallelMemoryPort> {
        self.logic
            .as_ref()
            .map(LogicComponent::memory_ports)
            .unwrap_or_default()
    }

    /// True when the named memory is the component's only tick-owned behavior.
    pub(crate) fn has_exclusive_memory_port(&self, name: &str) -> bool {
        self.logic
            .as_ref()
            .map(|logic| logic.has_exclusive_memory_port(name))
            .unwrap_or(false)
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
    order_serial_chains(digital, DigitalComponent::chains_as_595, "qh_serial", "ser")
}

/// Recover the separate physical daisy chains formed by a serial link between
/// chips of one family.
///
/// `link_out` is the role carrying the serial signal OUT of a chip and
/// `link_in` the role carrying it IN. A chain's head is the chip whose
/// `link_in` is not produced by any chip in the set (it is driven from
/// elsewhere: the MCU's serial-data net for a 595 write chain, the MCU's MISO
/// for a 165 read chain), and each chain is walked head-to-tail by following
/// `link_out` to the chip that consumes it. Two chains fed by different sources
/// are NOT flattened into one, so their serial streams cannot bleed across, and
/// a chip not reachable from any head (a ring, or a chip whose head was pruned)
/// becomes its own singleton chain rather than being silently merged.
fn order_serial_chains(
    digital: &[DigitalComponent],
    is_member: impl Fn(&DigitalComponent) -> bool,
    link_out: &str,
    link_in: &str,
) -> Vec<Vec<usize>> {
    let chips: Vec<usize> = digital
        .iter()
        .enumerate()
        .filter(|(_, d)| is_member(d))
        .map(|(i, _)| i)
        .collect();
    let node_of = |i: usize, role: &str| digital[i].roles.get(role).map(|n| n.0 as i64);

    // node -> chip whose `link_out` is that node (the producer).
    let producer: HashMap<i64, usize> = chips
        .iter()
        .filter_map(|&i| Some((node_of(i, link_out)?, i)))
        .collect();

    let mut heads: Vec<usize> = chips
        .iter()
        .copied()
        .filter(|&i| node_of(i, link_in).is_none_or(|n| !producer.contains_key(&n)))
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
            cur = node_of(i, link_out).and_then(|node| {
                chips
                    .iter()
                    .copied()
                    .find(|&j| node_of(j, link_in) == Some(node) && !seen.contains(&j))
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
#[derive(Debug, Clone)]
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
        // registers by NAME ("shift"/"store", 8 bits); the documented contract
        // between the Rust chain fast-path and a 595-shaped [models.logic]
        // spec. A chip that chains (ser + qh_serial roles) but lacks the
        // registers would silently desynchronize from its own analog outputs,
        // so refuse loudly and leave the chain to the once-per-chunk tick.
        for &ci in &order {
            let chip = &digital[ci];
            let ok = chip
                .logic
                .as_ref()
                .map(|l| l.register_bits("shift") == Some(8) && l.register_bits("store") == Some(8))
                .unwrap_or(false);
            if !ok {
                eprintln!(
                    "ERROR: 595-chain chip '{}' ({}): [models.logic] must declare 8-bit \
                     'shift' and 'store' registers to ride the edge-driven chain; \
                     falling back to once-per-chunk sampling for this chain",
                    chip.reference,
                    chip.logic
                        .as_ref()
                        .map(|l| l.spec_id())
                        .unwrap_or("no logic"),
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
    /// carrying serial across chips (`qh_serial[k]` -> `ser[k+1]`); on each RCLK
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

    /// Establish a GPIO control level when firmware first changes the pin from
    /// high-impedance input to driven output. This is not an observable edge
    /// from a known prior logic level, so clocks and latches must not advance.
    /// Level-sensitive active-low clear still applies immediately.
    pub(crate) fn establish_control(&mut self, pin: (char, u8), high: bool) {
        if pin == self.ser {
            self.lvl_ser = high;
        }
        if Some(pin) == self.srclr_n {
            self.lvl_srclr_n = high;
            if !high {
                self.shift.fill(0);
            }
        }
        if Some(pin) == self.oe_n {
            self.lvl_oe_n = high;
        }
        if pin == self.srclk {
            self.lvl_srclk = high;
        }
        if pin == self.rclk {
            self.lvl_rclk = high;
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
            // the per-chunk tick, so its own sampled tristate state is stale,
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
/// into the upstream chips; the order the firmware shifts bits OUT.
///
/// Mirror of [`order_595_chains`] for the read direction. A chip not reachable
/// from any head becomes its own singleton chain rather than being merged.
pub fn order_165_chains(digital: &[DigitalComponent]) -> Vec<Vec<usize>> {
    order_serial_chains(digital, DigitalComponent::chains_as_165, "ser", "qh")
}

/// An edge-driven model of one MCU-bit-banged 74HC165 parallel-in / serial-out
/// chain; the READ-direction analogue of [`Hc595Chain`].
///
/// The firmware reads the chain by pulsing PL (parallel-load) low to capture the
/// parallel inputs, then bit-banging the shared SCLK while sampling the head
/// chip's QH on its MISO input pin. Both the PL pulse and the SCLK pulse train
/// are sub-µs back-to-back `digitalWrite`s, far below the analog chunk rate, so
/// they MUST be resolved in the EVENT domain at edge granularity, exactly like
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
/// synthesized buffer passthrough mirrors onto.
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

    fn model(value: &str) -> hauksbee_models::ModelEntry {
        let q = ComponentQuery::new(None, Some(value.to_string()), None);
        ModelLibrary::builtin()
            .resolve(&q)
            .model
            .unwrap_or_else(|| panic!("builtin {value} model"))
    }

    fn edge(cycle: u64, port: char, bit: u8, level: bool) -> PinEdge {
        PinEdge {
            cycle,
            port,
            bit,
            level,
        }
    }

    fn roles(pairs: &[(&str, NodeId)]) -> HashMap<String, NodeId> {
        pairs.iter().map(|(r, n)| (r.to_string(), *n)).collect()
    }

    const Q_OUTPUTS: [&str; 8] = ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh"];

    /// An `n`-chip 74HC595 daisy chain (shared SRCLK/RCLK/SRCLR_n, each
    /// qh_serial feeding the next ser) bound to the Tarski GPIO map: SRCLK on
    /// PB5, RCLK on PD6, SRCLR_n on PC3, SER on PB3.
    fn build_chain(circuit: &mut Circuit, n: usize) -> (Vec<DigitalComponent>, Hc595Chain) {
        let model = model("74HC595");
        let n_srclk = circuit.node("SRCLK");
        let n_rclk = circuit.node("RCLK");
        let n_srclr = circuit.node("SRCLR_N");
        let n_ser_head = circuit.node("SER0");

        let mut chips: Vec<DigitalComponent> = Vec::new();
        let mut prev_qh: Option<NodeId> = None;
        for k in 0..n {
            let qh = circuit.node(&format!("QHS{k}"));
            let mut r = roles(&[
                ("srclk", n_srclk),
                ("rclk", n_rclk),
                ("srclr_n", n_srclr),
                ("ser", prev_qh.unwrap_or(n_ser_head)),
                ("qh_serial", qh),
            ]);
            for (i, q) in Q_OUTPUTS.iter().enumerate() {
                r.insert((*q).into(), circuit.node(&format!("Q{k}_{i}")));
            }
            chips.push(
                DigitalComponent::new(format!("U{k}"), &model, r, HashMap::new())
                    .expect("builtin 595 logic compiles"),
            );
            prev_qh = Some(qh);
        }
        let gpio_node: HashMap<i64, (char, u8)> = HashMap::from([
            (n_srclk.0 as i64, ('B', 5)),
            (n_rclk.0 as i64, ('D', 6)),
            (n_srclr.0 as i64, ('C', 3)),
            (n_ser_head.0 as i64, ('B', 3)),
        ]);
        let order = order_595_chain(&chips);
        let refs: Vec<&str> = order.iter().map(|&i| chips[i].reference.as_str()).collect();
        let want: Vec<String> = (0..n).map(|k| format!("U{k}")).collect();
        assert_eq!(refs, want, "daisy-chain order recovered from nets");
        let chain = Hc595Chain::build(&chips, order, &gpio_node).expect("chain binds to GPIO");
        (chips, chain)
    }

    /// The ordered edge stream of one `shiftOut(MSBFIRST)` on SER (PB3)
    /// clocked by SRCLK (PB5).
    fn shift_out_msb_first(log: &mut Vec<(char, u8, bool)>, byte: u8) {
        for bit in (0..8).rev() {
            log.push(('B', 3, ((byte >> bit) & 1) == 1));
            log.push(('B', 5, true));
            log.push(('B', 5, false));
        }
    }

    /// A lone GPIO-clocked 74HC595 (SER PB3, SRCLK PB5, RCLK PD6) and its
    /// `(port,bit) -> net` map for the generalized replay path.
    fn build_standalone_595(
        circuit: &mut Circuit,
    ) -> (Vec<DigitalComponent>, HashMap<(char, u8), NodeId>) {
        let n_ser = circuit.node("SS_SER");
        let n_srclk = circuit.node("SS_SRCLK");
        let n_rclk = circuit.node("SS_RCLK");
        let comp = DigitalComponent::new(
            "U0".into(),
            &model("74HC595"),
            roles(&[("ser", n_ser), ("srclk", n_srclk), ("rclk", n_rclk)]),
            HashMap::new(),
        )
        .expect("builtin 595 logic compiles");
        let pin_nets = HashMap::from([(('B', 3), n_ser), (('B', 5), n_srclk), (('D', 6), n_rclk)]);
        (vec![comp], pin_nets)
    }

    /// A cycle-stamped `shiftOut(MSBFIRST)`, one distinct cycle per edge.
    fn stamped_shift_out(log: &mut Vec<PinEdge>, cyc: &mut u64, byte: u8) {
        for bit in (0..8).rev() {
            for (pin, level) in [(3, ((byte >> bit) & 1) == 1), (5, true), (5, false)] {
                log.push(edge(*cyc, 'B', pin, level));
                *cyc += 1;
            }
        }
    }

    fn rclk_pulse(log: &mut Vec<PinEdge>, cyc: u64) {
        log.push(edge(cyc, 'D', 6, true));
        log.push(edge(cyc + 1, 'D', 6, false));
    }

    fn pack_out(c: &DigitalComponent) -> u8 {
        c.register("store").expect("595 store register") as u8
    }

    fn replay(
        comps: &mut [DigitalComponent],
        which: &[usize],
        pin_nets: &HashMap<(char, u8), NodeId>,
        log: &[PinEdge],
        base: &[f64],
        circuit: &mut Circuit,
    ) -> usize {
        replay_components_on_edges(comps, which, pin_nets, log, base, 5.0, 0.0, circuit)
    }

    /// Two spec-driven 595s wired qh_serial -> ser through a shared net: the
    /// generalized replay carries the serial bit across the chip boundary, so
    /// a 16-bit MSB-first stream lands the first-sent byte downstream.
    #[test]
    fn generalized_replay_carries_serial_across_chained_chips() {
        let model = model("74HC595");
        let mut circuit = Circuit::new();
        let n_ser0 = circuit.node("SER0");
        let n_srclk = circuit.node("SRCLK");
        let n_rclk = circuit.node("RCLK");
        let n_tap = circuit.node("QHS0");
        let n_tap1 = circuit.node("U1_TAP");
        let mk = |ser, tap, name: &str| {
            DigitalComponent::new(
                name.into(),
                &model,
                roles(&[
                    ("ser", ser),
                    ("srclk", n_srclk),
                    ("rclk", n_rclk),
                    ("qh_serial", tap),
                ]),
                HashMap::new(),
            )
            .expect("builtin 595 logic compiles")
        };
        let mut comps = vec![mk(n_ser0, n_tap, "U0"), mk(n_tap, n_tap1, "U1")];
        let pin_nets = HashMap::from([(('B', 3), n_ser0), (('B', 5), n_srclk), (('D', 6), n_rclk)]);

        let (first, second) = (0x9Du8, 0x3Cu8);
        let mut log = Vec::new();
        let mut cyc = 0u64;
        stamped_shift_out(&mut log, &mut cyc, first);
        stamped_shift_out(&mut log, &mut cyc, second);
        rclk_pulse(&mut log, cyc);

        let base = vec![0.0; circuit.node_count()];
        let ticks = replay(&mut comps, &[0, 1], &pin_nets, &log, &base, &mut circuit);
        assert_eq!(ticks, log.len(), "one micro-tick per distinct-cycle edge");
        assert_eq!(
            pack_out(&comps[1]),
            first,
            "the first-sent byte crossed downstream"
        );
        assert_eq!(pack_out(&comps[0]), second);
    }

    /// N distinct-cycle edges are N ordered micro-ticks that latch the byte
    /// bit-exact; edges sharing one cycle are a single micro-tick.
    #[test]
    fn generalized_replay_micro_ticks_per_distinct_cycle() {
        let mut circuit = Circuit::new();
        let (mut comps, pin_nets) = build_standalone_595(&mut circuit);
        let base = vec![0.0; circuit.node_count()];
        let mut log = Vec::new();
        let mut cyc = 0u64;
        stamped_shift_out(&mut log, &mut cyc, 0xA6);
        rclk_pulse(&mut log, cyc);
        let ticks = replay(&mut comps, &[0], &pin_nets, &log, &base, &mut circuit);
        assert_eq!(ticks, log.len());
        assert_eq!(pack_out(&comps[0]), 0xA6);

        let mut circuit = Circuit::new();
        let (mut comps, pin_nets) = build_standalone_595(&mut circuit);
        let base = vec![0.0; circuit.node_count()];
        let log = vec![edge(5, 'B', 3, true), edge(5, 'B', 5, true)];
        assert_eq!(
            replay(&mut comps, &[0], &pin_nets, &log, &base, &mut circuit),
            1
        );
    }

    #[test]
    fn pin_edges_are_cycle_monotonic_per_pin() {
        let log = vec![
            edge(0, 'B', 5, true),
            edge(1, 'B', 3, true),
            edge(2, 'B', 5, false),
            edge(5, 'B', 5, true),
            edge(7, 'B', 3, false),
        ];
        let by_pin = pin_edges_by_pin(&log);
        for (pin, series) in &by_pin {
            for w in series.windows(2) {
                assert!(w[0].0 <= w[1].0, "pin {pin:?}: {series:?}");
            }
        }
        assert_eq!(by_pin[&('B', 5)].len(), 3);
        assert_eq!(by_pin[&('B', 3)].len(), 2);
    }

    /// A single edge in the chunk yields the same state through the
    /// generalized replay as the once-per-chunk collapsed tick.
    #[test]
    fn single_edge_matches_collapsed_tick() {
        let mut circuit = Circuit::new();
        let (mut comps, pin_nets) = build_standalone_595(&mut circuit);
        let mut base = vec![0.0; circuit.node_count()];
        base[pin_nets[&('B', 3)].0 as usize] = 5.0;
        let log = vec![edge(10, 'B', 5, true)];
        assert_eq!(
            replay(&mut comps, &[0], &pin_nets, &log, &base, &mut circuit),
            1
        );
        let replay_shift = comps[0].register("shift").expect("shift register");

        let mut circuit2 = Circuit::new();
        let (mut comps2, pin_nets2) = build_standalone_595(&mut circuit2);
        let mut volts = vec![0.0; circuit2.node_count()];
        volts[pin_nets2[&('B', 3)].0 as usize] = 5.0;
        volts[pin_nets2[&('B', 5)].0 as usize] = 5.0;
        let node_v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
        comps2[0].tick(&mut circuit2, &node_v);
        assert_eq!(comps2[0].register("shift"), Some(replay_shift));
        assert!(
            replay_shift & 1 == 1,
            "the one SRCLK edge shifted SER(high) in"
        );
    }

    /// A `shiftOut(MSBFIRST)` of N bytes through an N-chip chain plus an RCLK
    /// pulse lands the first-sent byte in the LAST chip; a latest-level
    /// collapse of the same train (one SRCLK edge) latches nothing.
    #[test]
    fn edge_stream_latches_chain_in_path_b_order_and_a_collapse_does_not() {
        let n = 4;
        let weights: Vec<u8> = vec![0x11, 0x22, 0x33, 0x44];
        let expected: Vec<u8> = (0..n).map(|p| weights[n - 1 - p]).collect();

        let mut circuit = Circuit::new();
        let (_chips, mut chain) = build_chain(&mut circuit, n);
        let mut log = vec![('C', 3, true)];
        for &b in &weights {
            shift_out_msb_first(&mut log, b);
        }
        log.push(('D', 6, true));
        log.push(('D', 6, false));
        chain.replay(&log);
        assert_eq!(chain.latched, expected);

        let mut circuit = Circuit::new();
        let (_chips, mut chain) = build_chain(&mut circuit, n);
        let last_bit = (weights[n - 1] & 1) == 1;
        chain.replay(&[
            ('C', 3, true),
            ('B', 3, last_bit),
            ('B', 5, true),
            ('D', 6, true),
        ]);
        assert_ne!(chain.latched, expected);
        assert_eq!(
            chain.latched,
            vec![0u8; n],
            "a single SRCLK edge shifts one bit"
        );
    }

    /// Two chains with distinct SER heads are recovered separately, and one
    /// chain's serial never bleeds into the other's.
    #[test]
    fn independent_chains_are_not_merged() {
        let model = model("74HC595");
        let mut circuit = Circuit::new();
        let srclk = circuit.node("SRCLK");
        let rclk = circuit.node("RCLK");
        let mut chips: Vec<DigitalComponent> = Vec::new();
        for tag in ["A", "B"] {
            let head_ser = circuit.node(&format!("SER_{tag}"));
            let mut prev_qh: Option<NodeId> = None;
            for k in 0..2 {
                let qh = circuit.node(&format!("QHS_{tag}{k}"));
                let r = roles(&[
                    ("srclk", srclk),
                    ("rclk", rclk),
                    ("ser", prev_qh.unwrap_or(head_ser)),
                    ("qh_serial", qh),
                ]);
                chips.push(
                    DigitalComponent::new(format!("U_{tag}{k}"), &model, r, HashMap::new())
                        .expect("builtin 595 logic compiles"),
                );
                prev_qh = Some(qh);
            }
        }
        let chains = order_595_chains(&chips);
        assert_eq!(chains.len(), 2);
        assert!(chains.iter().all(|ch| ch.len() == 2));

        // Chain A's SER is PB3; chain B's SER head is PB4, which never toggles.
        let ser_a = circuit.node("SER_A");
        let ser_b = circuit.node("SER_B");
        let mut gpio_a = HashMap::from([
            (srclk.0 as i64, ('B', 5)),
            (rclk.0 as i64, ('D', 6)),
            (ser_a.0 as i64, ('B', 3)),
        ]);
        let mut gpio_b = gpio_a.clone();
        gpio_b.remove(&(ser_a.0 as i64));
        gpio_b.insert(ser_b.0 as i64, ('B', 4));
        gpio_a.remove(&(ser_b.0 as i64));
        let find = |head: &str| -> Vec<usize> {
            chains
                .iter()
                .find(|c| chips[c[0]].reference == head)
                .cloned()
                .expect("chain present")
        };
        let mut chain_a = Hc595Chain::build(&chips, find("U_A0"), &gpio_a).expect("chain A binds");
        let mut chain_b = Hc595Chain::build(&chips, find("U_B0"), &gpio_b).expect("chain B binds");

        let mut log = Vec::new();
        for _ in 0..16 {
            log.push(('B', 3, true));
            log.push(('B', 5, true));
            log.push(('B', 5, false));
        }
        log.push(('D', 6, true));
        chain_a.replay(&log);
        chain_b.replay(&log);
        assert_eq!(chain_a.latched, vec![0xFF, 0xFF]);
        assert_eq!(
            chain_b.latched,
            vec![0x00, 0x00],
            "A's serial must not bleed into B"
        );
    }

    #[test]
    fn oe_high_tristates_parallel_outputs() {
        use crate::drivers::{PinDriver, DEFAULT_RO};
        let mut circuit = Circuit::new();
        let srclk = circuit.node("SRCLK");
        let rclk = circuit.node("RCLK");
        let ser = circuit.node("SER");
        let oe = circuit.node("OE_N");
        let mut r = roles(&[("srclk", srclk), ("rclk", rclk), ("ser", ser), ("oe_n", oe)]);
        let mut drivers: HashMap<String, PinDriver> = HashMap::new();
        for q in Q_OUTPUTS {
            let net = circuit.node(&q.to_uppercase());
            r.insert(q.into(), net);
            let drv = PinDriver::stamp(&mut circuit, net, q, &format!("U_{q}"), DEFAULT_RO);
            drivers.insert(q.into(), drv);
        }
        let mut chips = vec![
            DigitalComponent::new("U0".into(), &model("74HC595"), r, drivers)
                .expect("builtin 595 logic compiles"),
        ];
        let gpio = HashMap::from([
            (srclk.0 as i64, ('B', 5)),
            (rclk.0 as i64, ('D', 6)),
            (ser.0 as i64, ('B', 3)),
            (oe.0 as i64, ('C', 2)),
        ]);
        let order = order_595_chains(&chips).into_iter().next().unwrap();
        let mut chain = Hc595Chain::build(&chips, order, &gpio).expect("binds");
        assert_eq!(chain.oe_n, Some(('C', 2)));

        chain.replay(&[('C', 2, true)]);
        chain.apply(&mut chips, &mut circuit);
        assert!(chips[0].drivers.values().all(|d| !d.enabled));
        chain.replay(&[('C', 2, false)]);
        chain.apply(&mut chips, &mut circuit);
        assert!(chips[0].drivers.values().all(|d| d.enabled));
    }

    /// The Tarski 2-chip 165 read chain: U15002 (head) feeds MISO, U15001 is
    /// upstream. Parallel inputs in `*_hi` are driven to 5 V, the rest to GND.
    fn build_165_chain(
        circuit: &mut Circuit,
        head_inputs_hi: &[&str],
        up_inputs_hi: &[&str],
    ) -> (Vec<DigitalComponent>, Hc165Chain) {
        let model = model("74HC165");
        let pl = circuit.node("PARALLEL_LOAD");
        let clk = circuit.node("SCLK");
        let miso = circuit.node("MISO");
        let inter = circuit.node("U15001_Q7");
        let make = |circuit: &mut Circuit, refn: &str, ser, qh, hi: &[&str]| {
            let mut r = roles(&[("pl_n", pl), ("clk", clk), ("ser", ser), ("qh", qh)]);
            for input in ["a", "b", "c", "d", "e", "f", "g", "h"] {
                let n = circuit.node(&format!("{refn}_{input}"));
                r.insert(input.into(), n);
                circuit.add(hauksbee_ir::Device::Vsource {
                    name: format!("V_{refn}_{input}"),
                    p: n,
                    n: NodeId::GROUND,
                    kind: hauksbee_ir::SourceKind::Dc(if hi.contains(&input) { 5.0 } else { 0.0 }),
                });
            }
            DigitalComponent::new(refn.into(), &model, r, HashMap::new())
                .expect("builtin 165 logic compiles")
        };
        let up = make(circuit, "U15001", NodeId::GROUND, inter, up_inputs_hi);
        let head = make(circuit, "U15002", inter, miso, head_inputs_hi);
        let chips = vec![up, head];
        let gpio = HashMap::from([
            (pl.0 as i64, ('D', 4)),
            (clk.0 as i64, ('B', 5)),
            (miso.0 as i64, ('B', 4)),
        ]);
        let order = order_165_chains(&chips);
        assert_eq!(order.len(), 1, "one 165 chain recovered");
        let refs: Vec<&str> = order[0]
            .iter()
            .map(|&i| chips[i].reference.as_str())
            .collect();
        assert_eq!(refs, vec!["U15002", "U15001"], "head-first chain order");
        let chain = Hc165Chain::build(&chips, order.into_iter().next().unwrap(), &gpio, &gpio)
            .expect("165 chain binds to GPIO/MISO");
        (chips, chain)
    }

    /// The firmware's exact ReadOutput sequence (PL low/high, then 16 × read
    /// MISO + pulse SCLK) reads a known latch pattern back bit-exact.
    #[test]
    fn hc165_reads_known_latch_pattern_via_edges() {
        let mut circuit = Circuit::new();
        // head a..h = L3..L10, upstream g,h = L1,L2; L1, L4, L7, L10 high.
        let (_chips, mut chain) = build_165_chain(&mut circuit, &["b", "e", "h"], &["g"]);
        let levels = LogicLevels::from_params(&model("74HC165"));
        let mut volts: HashMap<i64, f64> = HashMap::new();
        for dev in &circuit.devices {
            if let hauksbee_ir::Device::Vsource {
                p,
                kind: hauksbee_ir::SourceKind::Dc(v),
                ..
            } = dev
            {
                volts.insert(p.0 as i64, *v);
            }
        }
        let node_v = |n: NodeId| volts.get(&(n.0 as i64)).copied().unwrap_or(0.0);

        let mut miso_level = false;
        let mut drive = |pin, high, miso: &mut bool| {
            if let Some((_, lvl)) = chain.on_edge(pin, high, &node_v, &levels) {
                *miso = lvl;
            }
        };
        drive(('D', 4), false, &mut miso_level);
        drive(('D', 4), true, &mut miso_level);
        let mut value: u16 = 0;
        for _ in 0..16 {
            value = (value << 1) | miso_level as u16;
            drive(('B', 5), true, &mut miso_level);
            drive(('B', 5), false, &mut miso_level);
        }
        // MSB-first word: L10 L9 L8 L7 L6 L5 L4 L3 | L2 L1 . . . . . .
        let expected: u16 = (1 << 15) | (1 << 12) | (1 << 9) | (1 << 6);
        assert_eq!(value, expected, "0x{value:04X} vs 0x{expected:04X}");
        assert_eq!(chain.loaded_word(), expected);
    }

    #[test]
    fn hc165_collapsed_single_edge_does_not_read_full_word() {
        let mut circuit = Circuit::new();
        let (_chips, mut chain) = build_165_chain(&mut circuit, &["a", "h"], &["a"]);
        let levels = LogicLevels::from_params(&model("74HC165"));
        let node_v = |_n: NodeId| 0.0;
        let _ = chain.on_edge(('D', 4), false, &node_v, &levels);
        let _ = chain.on_edge(('D', 4), true, &node_v, &levels);
        let before = chain.pos;
        let _ = chain.on_edge(('B', 5), true, &node_v, &levels);
        assert_eq!(
            chain.pos,
            before + 1,
            "a single SCLK rise advances exactly one bit"
        );
        assert!(chain.pos < 16);
    }

    /// 74HC02 NOR SR spike latch: idle Q HIGH, a SET pulse drives Q LOW and
    /// holds it, RESET returns Q HIGH.
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
        let r = roles(&[("set", set_n), ("reset", reset_n), ("q", q_n)]);
        let mut latch =
            DigitalComponent::new_nor_latch("U_L1".to_string(), levels, r, HashMap::new());
        let make_v = |set_hi: bool, reset_hi: bool| {
            move |n: NodeId| -> f64 {
                let hi = (n == set_n && set_hi) || (n == reset_n && reset_hi);
                if hi {
                    4.5
                } else {
                    0.0
                }
            }
        };
        let q = |l: &DigitalComponent| l.output_level("q").expect("latch q output");

        assert!(q(&latch), "power-on idle Q HIGH");
        latch.tick(&mut circuit, &make_v(false, true));
        assert!(q(&latch), "RESET with no spike holds idle");
        latch.tick(&mut circuit, &make_v(false, false));
        assert!(q(&latch), "hold");
        latch.tick(&mut circuit, &make_v(true, false));
        assert!(!q(&latch), "a SET pulse drives Q LOW");
        latch.tick(&mut circuit, &make_v(false, false));
        assert!(!q(&latch), "held LOW after the spike clears");
        latch.tick(&mut circuit, &make_v(false, true));
        assert!(q(&latch), "RESET returns idle HIGH");
    }

    /// A toggling gate draws `static + Cpd_eff · VCC / dt` per output
    /// transition from its VCC net, and nothing at all from a dead rail.
    #[test]
    fn toggling_gate_draws_expected_supply_charge() {
        let model = model("74HC00");
        let static_ua = model
            .params
            .get_f64("supply_static_ua")
            .expect("supply_static_ua");
        let cpd_pf = model
            .params
            .get_f64("supply_cpd_pf")
            .expect("supply_cpd_pf");
        assert!(static_ua > 0.0 && cpd_pf > 0.0);

        let mut circuit = Circuit::new();
        let n_a = circuit.node("A");
        let n_b = circuit.node("B");
        let n_y = circuit.node("Y1");
        let n_vcc = circuit.node("VDD_EVAL");
        let r = roles(&[("a1", n_a), ("b1", n_b), ("y1", n_y), ("vcc", n_vcc)]);
        let drivers = HashMap::from([(
            "y1".to_string(),
            PinDriver::stamp(&mut circuit, n_y, "Y1", "U1_y1", DEFAULT_RO),
        )]);
        let mut gate =
            DigitalComponent::new("U1".into(), &model, r, drivers).expect("74HC00 logic compiles");
        gate.supply = Some(SupplyDraw::stamp(
            &mut circuit,
            n_vcc,
            NodeId::GROUND,
            "U1",
            static_ua,
            cpd_pf,
        ));
        let isource = gate.supply.as_ref().unwrap().isource;
        let drawn_amps = |c: &Circuit| -> f64 {
            match c.devices.get(isource.0 as usize) {
                Some(Device::Isource {
                    kind: SourceKind::Dc(v),
                    ..
                }) => *v,
                other => panic!("supply Isource missing: {other:?}"),
            }
        };

        const VCC: f64 = 5.0;
        const DT: f64 = 100e-6;
        let make_v = |a_high: bool| {
            move |n: NodeId| -> f64 {
                if n == n_vcc || n == n_b || (n == n_a && a_high) {
                    VCC
                } else {
                    0.0
                }
            }
        };
        let step = |gate: &mut DigitalComponent, circuit: &mut Circuit, a_high: bool| {
            gate.tick(circuit, &make_v(a_high));
            gate.update_supply(circuit, DT, &make_v(a_high));
            drawn_amps(circuit)
        };

        // The first drive is level establishment, not a transition.
        let i_static = step(&mut gate, &mut circuit, false);
        assert!((i_static - static_ua * 1e-6).abs() < 1e-15, "{i_static}");
        // A rises -> Y falls: one transition.
        let i_one = step(&mut gate, &mut circuit, true);
        let want_one = static_ua * 1e-6 + cpd_pf * 1e-12 * VCC / DT;
        assert!(
            (i_one - want_one).abs() < 1e-15,
            "got {i_one}, want {want_one}"
        );
        assert!(((i_one - i_static) * DT - cpd_pf * 1e-12 * VCC).abs() < 1e-20);
        // A falls -> Y rises: same charge; then no change: back to static.
        assert!((step(&mut gate, &mut circuit, false) - want_one).abs() < 1e-15);
        assert!((step(&mut gate, &mut circuit, false) - i_static).abs() < 1e-15);

        gate.update_supply(&mut circuit, DT, &|_n: NodeId| 0.0);
        assert_eq!(drawn_amps(&circuit), 0.0, "unpowered rail draws nothing");
    }
}
