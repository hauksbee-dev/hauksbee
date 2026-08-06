//! The binder: [`ExtractedBoard`] + [`ModelLibrary`] -> [`BoundBoard`].
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/binder.md.
//!
//! Every component is resolved to a model and turned into something the
//! co-sim can run:
//!   - passives (R/C/L) -> analog IR devices from the parsed `Value`;
//!   - diode / BJT / MOSFET -> analog IR devices with SPICE params;
//!   - vreg -> a behavioral ideal source on its output net;
//!   - analog_switch -> `VSwitch`; opamp/comparator -> behavioral IR devices;
//!   - digital ICs (74HC595, ...) -> event-driven [`DigitalComponent`]s whose
//!     outputs are Thevenin [`PinDriver`]s stamped into the circuit;
//!   - mcu -> an [`McuBinding`] (instantiated lazily by the scheduler) with a
//!     pad->port-pin map and GPIO Thevenin drivers;
//!   - power nets (VCC/+5V/GND/...) -> ideal rails;
//!   - ignore-kind -> skipped.
//!
//! Unresolved analog parts sitting on connected nets raise a loud warning and
//! default to an open circuit.

use std::collections::HashMap;

use hauksbee_extract::{Component, ExtractedBoard};
use hauksbee_ir::{
    BjtModel, Circuit, Device, DiodeModel, MosLevel, MosfetModel, NodeId, Polarity, SourceKind,
};
use hauksbee_models::value::parse_value;
use hauksbee_models::{ComponentKind, ComponentQuery, Confidence, ModelEntry, ModelLibrary};

use crate::digital::{output_roles, DigitalComponent, SupplyDraw};
use crate::drivers::{PinDriver, DEFAULT_RO};
use crate::power_supply::{PowerSupply, SupplyLeg};
use crate::report::{BindOutcome, BindReport, BindRow};
use crate::stress::DeviceMeta;

/// Default supply voltage for an ideal +5V rail.
pub const DEFAULT_VCC: f64 = 5.0;

/// One MCU instance discovered on the board (instantiated by the scheduler).
pub struct McuBinding {
    pub reference: String,
    /// hauksbee-mcu backend string, e.g. `"simavr:atmega328p"`.
    pub backend: String,
    /// The exact part the board asked for (the component value, e.g.
    /// `"STM32F411RET6"`), captured BEFORE `route_mcu_family_str` collapses it to
    /// a coarse family backend. Used by the scheduler to emit a chip-substitution
    /// warning when the modelled core is less specific than the requested part
    /// (Track B). May be empty when the board gives no value string.
    pub requested_part: String,
    /// Pad number -> role string from the model pin map (e.g. "19"->"pb5_sck").
    pub pad_roles: HashMap<String, String>,
    /// Role string -> net node it is wired to (only connected pins).
    pub role_nets: HashMap<String, NodeId>,
    /// GPIO output drivers, keyed by `PinId`-style "Pb" (port,bit) → driver,
    /// filled for every port pin wired to a net. The scheduler flips these on
    /// pin-change events.
    pub gpio_drivers: HashMap<(char, u8), PinDriver>,
    /// ADC channel -> net node, so the scheduler can inject node voltages.
    pub adc_nets: HashMap<u8, NodeId>,
    /// ADC channel -> its OWN GPIO `(port,bit)`, for analog-capable pins that also
    /// carry a digital port pin (e.g. ATmega328P A0..A5 = PC0..PC5). ADC-ONLY
    /// channels (A6/A7) have NO entry. The scheduler uses this to decide whether a
    /// channel was promoted to output by checking THIS channel's own driver,
    /// never merely whether some other pin's driver shares the net.
    pub adc_pin: HashMap<u8, (char, u8)>,
    /// Whether this is a module wrapper (Arduino_Nano) using header pad names.
    pub module: bool,
    /// Absolute-maximum supply voltage (V) from the model's ratings
    /// (`max_voltage_v`), when the model carries one. The scheduler turns this
    /// plus the vcc/vdd `role_nets` entries into supply-rail stress watches, so
    /// a rail driven past the chip's abs-max Vcc raises an overvoltage fault
    /// instead of nothing.
    pub max_supply_v: Option<f64>,
}

/// One MCP4728 quad DAC discovered on the board. Carries the assigned 7-bit
/// I2C address, the board reference/value config (VREF, gain), and the four
/// VOUT-channel [`PinDriver`]s already stamped into the circuit. The scheduler
/// realizes each binding as a spec-driven
/// [`RegisterMapSensor`](crate::RegisterMapSensor) instance of
/// `testdata/sensor-specs/mcp4728.toml` on a shared bus, binding these drivers to
/// the spec's per-channel outputs; the slave then drives the VOUT nets itself
/// at each transaction end (the ctx-bearing `on_stop`, 05 §3.1), so the analog
/// solve sees the DAC output voltages.
pub struct DacBinding {
    pub reference: String,
    pub address: u8,
    pub vref: f64,
    pub gain: u8,
    /// VOUT channel A..D -> stamped driver (None if that channel net is
    /// unconnected / tied to ground on this board).
    pub vout_drivers: [Option<PinDriver>; 4],
}

/// The bound board: a ready-to-solve circuit plus the event-driven layer.
pub struct BoundBoard {
    pub name: String,
    pub circuit: Circuit,
    /// Net name -> circuit node.
    pub net_nodes: HashMap<String, NodeId>,
    /// All net names in *net-declaration order* (for board_info / frame counts).
    ///
    /// WARNING: this is NOT a `NodeId` reverse map. It is pushed once per real
    /// net (skipping KiCad's no-net id 0), so it has no entry for the ground
    /// node that occupies `NodeId(0)` in the circuit. Indexing it by `NodeId.0`
    /// is therefore OFF BY ONE and relabels every node as its successor, which
    /// is exactly what produced a spurious "comparator +IN -> SCL" mis-wiring
    /// claim on the Tarski board. To turn a `NodeId` back into a net name, use
    /// [`Circuit::node_name`] (the circuit's authoritative reverse map).
    pub net_names: Vec<String>,
    pub digital: Vec<DigitalComponent>,
    pub mcus: Vec<McuBinding>,
    /// Processors that were skipped because the board file marks them DNP, as
    /// (reference, value). Non-empty with an empty `mcus` means the board has
    /// no simulable processor for a reason the user can undo (`--fit`), which
    /// is what the firmware gates report instead of running vacuously.
    pub dnp_mcus: Vec<(String, String)>,
    /// reference -> resolved model kind string (for board_info coloring).
    pub component_kinds: HashMap<String, String>,
    /// Named controllable input sources: reference -> DeviceId of a Vsource /
    /// Isource the UI can override (sliders).
    pub input_sources: HashMap<String, hauksbee_ir::DeviceId>,
    /// Configurable power supplies, one per detected supply net (Feature 1).
    /// Default to [`PowerSupply::Ideal`] at the rail's nominal voltage, so a
    /// board whose supplies are left unconfigured sees perfect rails.
    pub supplies: Vec<SupplyLeg>,
    /// Behavioural devices (power ICs with a declarative behavioural model:
    /// chargers, PMICs, balancers). Iterated by the scheduler each chunk, the
    /// same cadence as the supplies.
    pub behavioral: Vec<crate::behavioral::BehavioralDevice>,
    /// Per-device metadata for the fault/stress monitor (Feature 2).
    pub device_meta: Vec<DeviceMeta>,
    /// MCP4728 quad DACs discovered on the board, with their VOUT drivers. The
    /// scheduler turns these into I2C slaves and drives the analog VOUT nets.
    pub dacs: Vec<DacBinding>,
    pub report: BindReport,
}

impl BoundBoard {
    /// Look up a net node by name.
    pub fn node(&self, net: &str) -> Option<NodeId> {
        self.net_nodes.get(net).copied()
    }

    /// Remove the auto-rail feeding `net` so the net floats except for whatever
    /// the board itself pushes onto it. Returns `true` when a rail was actually
    /// removed.
    ///
    /// This has to live here, next to the code that stamps a rail, because the
    /// topology is not what a caller would guess. A [`SupplyLeg`] does NOT put
    /// its source on the rail node: it interns a private `__supply_<net>` node,
    /// puts the `Vsource` there, and joins it to the rail through a milliohm
    /// series resistor so the scheduler can measure rail current. A suppression
    /// that looks for a `Vsource` on the RAIL node therefore matches nothing,
    /// drops the leg from `supplies` (so the scheduler stops re-commanding it),
    /// and leaves the source stamped at its nominal voltage. The rail keeps
    /// reading 5.000 V and a brownout test silently tests nothing.
    ///
    /// Both shapes are handled: a `SupplyLeg`'s internal source, and a bare
    /// `Vrail_*` / `Vsupply_*` ideal source sitting directly on the rail node.
    /// Each is replaced by a 1 TΩ open rather than deleted, so no `DeviceId`
    /// shifts and every index held elsewhere stays valid.
    pub fn suppress_rail(&mut self, net: &str) -> bool {
        let Some(node) = self.node(net) else {
            return false;
        };
        let mut opened = false;
        // The leg's internal source, reached through the leg itself rather than
        // guessed at from the rail node.
        for leg in self.supplies.iter().filter(|s| s.net == node) {
            if let Some(dev) = self.circuit.devices.get_mut(leg.vsource.0 as usize) {
                if let Device::Vsource { name, p, .. } = dev {
                    let (nm, a) = (name.clone(), *p);
                    *dev = Device::Resistor {
                        name: nm,
                        a,
                        b: NodeId::GROUND,
                        ohms: SUPPRESSED_RAIL_OHMS,
                        tc1: None,
                    };
                    opened = true;
                }
            }
        }
        self.supplies.retain(|s| s.net != node);
        // A bare ideal rail source stamped straight onto the net.
        let leg_name = format!("Vsupply_{net}");
        for dev in self.circuit.devices.iter_mut() {
            if let Device::Vsource { name, p, .. } = dev {
                if *p == node && (*name == leg_name || name.starts_with("Vrail")) {
                    let (nm, a) = (name.clone(), *p);
                    *dev = Device::Resistor {
                        name: nm,
                        a,
                        b: NodeId::GROUND,
                        ohms: SUPPRESSED_RAIL_OHMS,
                        tc1: None,
                    };
                    opened = true;
                }
            }
        }
        opened
    }
}

/// Resistance a suppressed rail's source is replaced by: an open, not a delete,
/// so no `DeviceId` shifts under a caller holding one.
const SUPPRESSED_RAIL_OHMS: f64 = 1e12;

/// Which override syntax to suggest when a DNP processor blocked a run.
#[derive(Clone, Copy)]
pub enum FitRemedy {
    /// `hauksbee run --fit A101`
    Cli,
    /// `fit = ["A101"]` in a `hauksbee-ci` spec.
    Spec,
}

/// Why a run that needs firmware cannot proceed, and what to do about it.
///
/// Firmware on a board with zero processors is unanswerable rather than
/// merely suspicious: nothing executes, so every firmware assertion passes
/// without being tested. Callers pair this with
/// [`EXIT_INVALID_FOR_ANALYSIS`](crate::result::EXIT_INVALID_FOR_ANALYSIS)
/// (or their spec-error equivalent) so a vacuous green is impossible.
pub fn no_processor_message(dnp_mcus: &[(String, String)], remedy: FitRemedy) -> String {
    if dnp_mcus.is_empty() {
        return "cannot run firmware: this board bound zero processors, and none were \
                skipped for DNP. Check that the board's processor is a part the model \
                library recognises; `hauksbee models resolve <board>` lists what resolved."
            .to_string();
    }
    let named = dnp_mcus
        .iter()
        .map(|(r, v)| format!("{r} ({v})"))
        .collect::<Vec<_>>()
        .join(", ");
    let refs = dnp_mcus
        .iter()
        .map(|(r, _)| r.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let is_are = if dnp_mcus.len() == 1 { "is" } else { "are" };
    match remedy {
        FitRemedy::Cli => format!(
            "cannot run firmware: this board bound zero processors. {named} {is_are} marked \
             DNP in the board file, so it was not simulated. If the module is really fitted \
             (socketed modules are often marked DNP because they are bought separately), \
             re-run with --fit {refs}. If it is really absent, drop --firmware and analyse \
             the board without it."
        ),
        FitRemedy::Spec => format!(
            "spec names firmware but the board bound zero processors: {named} {is_are} marked \
             DNP in the board file. If the module is really fitted, add fit = [\"{refs}\"] to \
             the spec; if it is really absent, remove `firmware` and the firmware assertions, \
             which cannot be evaluated without a processor."
        ),
    }
}

/// Bind an extracted board against a model library (no custom behaviours).
pub fn bind_board(board: &ExtractedBoard, lib: &ModelLibrary) -> BoundBoard {
    bind_board_with(board, lib, &crate::behavioral::CustomRegistry::new())
}

/// Bind an extracted board, consulting `custom` for escape-hatch Rust
/// behaviours. A component whose resolved model id / value / MPN matches a
/// registered factory is realised by that [`CustomBehavior`](crate::behavioral::CustomBehavior)
/// instead of the declarative layer.
pub fn bind_board_with(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    custom: &crate::behavioral::CustomRegistry,
) -> BoundBoard {
    let mut circuit = Circuit::new();
    let mut net_nodes: HashMap<String, NodeId> = HashMap::new();
    let mut net_names: Vec<String> = Vec::new();
    let mut report = BindReport {
        rows: Vec::new(),
        board_name: board.name.clone(),
    };

    // ── Pass 1: intern every net as a node, classify power/ground ───────────
    // net id -> node id (for pin wiring). Net 0 is KiCad's "no net".
    let mut netid_node: HashMap<i64, NodeId> = HashMap::new();
    let mut power_nets: HashMap<String, f64> = HashMap::new();
    // Rails identified by a `power_out` source pin's function (net id -> volts),
    // catching supply nets whose NAME is non-canonical (e.g. `+5P`) but whose
    // driving pin is tagged `power_out`.
    let power_out_nets = power_out_net_voltages(board);
    for net in &board.nets {
        if net.id == 0 {
            continue;
        }
        let node = if is_canonical_ground(&net.name) {
            NodeId::GROUND
        } else {
            circuit.node(&net.name)
        };
        netid_node.insert(net.id, node);
        net_nodes.insert(net.name.clone(), node);
        net_names.push(net.name.clone());
        // A rail is recognised either by its canonical name or by a `power_out`
        // pin that drives it at a named voltage (the general case). Ground nets
        // are never rails.
        if !node.is_ground() {
            if let Some(v) =
                power_rail_voltage(&net.name).or_else(|| power_out_nets.get(&net.id).copied())
            {
                power_nets.insert(net.name.clone(), v);
            }
        }
    }

    // Resolve a net id to a node, defaulting to ground for the no-net bucket.
    let node_of = |id: Option<i64>| -> Option<NodeId> {
        match id {
            None | Some(0) => None,
            Some(i) => netid_node.get(&i).copied(),
        }
    };

    let mut component_kinds: HashMap<String, String> = HashMap::new();
    let mut digital: Vec<DigitalComponent> = Vec::new();
    let mut mcus: Vec<McuBinding> = Vec::new();
    let mut dacs: Vec<DacBinding> = Vec::new();
    let input_sources: HashMap<String, hauksbee_ir::DeviceId> = HashMap::new();

    // Detect whether the board has its own regulator chain we can solve. If a
    // vreg is present we let it source its output net rather than overriding
    // with an ideal rail (only the input rail stays ideal).
    let has_vreg = board.components.iter().any(|c| {
        !c.dnp
            && matches!(
                resolve(lib, c).model.as_ref().map(|m| m.kind),
                Some(ComponentKind::Vreg)
            )
    });

    // DNP parts the model DB recognises as processors, as (reference, value).
    let mut dnp_mcus: Vec<(String, String)> = Vec::new();

    // ── Pass 2: bind every component ────────────────────────────────────────
    for comp in &board.components {
        // A DNP part sits on the layout but is not assembled: it is
        // electrically ABSENT. It must contribute no device and no pin-to-net
        // wiring, a DNP bridge resistor stamped anyway would join two nets
        // that are open on the real board. Every checks/* module already
        // filters `dnp`; the binder must too.
        if comp.dnp {
            // A DNP processor is the one skip that can hollow out a whole run:
            // with no MCU there is no firmware, so every "the firmware must
            // ..." assertion passes vacuously. Note which ones they were so the
            // report can say so out loud (patched in below, once it is known
            // whether any MCU bound at all).
            if matches!(
                resolve(lib, comp).model.as_ref().map(|m| m.kind),
                Some(ComponentKind::Mcu)
            ) {
                dnp_mcus.push((comp.reference.clone(), comp.value.clone()));
            }
            report.push(BindRow {
                reference: comp.reference.clone(),
                value: comp.value.clone(),
                model_id: None,
                confidence: Confidence::Exact,
                outcome: BindOutcome::Skipped {
                    reason: "DNP (not populated)".to_string(),
                },
                warning: None,
                guesses: Vec::new(),
            });
            continue;
        }
        let res = resolve(lib, comp);
        let model = res.model.clone();
        let conf = res.confidence;
        let model_id = model.as_ref().map(|m| m.id.clone());

        let (kind_str, outcome, warning, guesses) = match &model {
            None => {
                let (kind_str, outcome, warning) = unresolved_outcome(comp, &node_of);
                (kind_str, outcome, warning, Vec::new())
            }
            Some(m) => {
                let kind_str = format!("{:?}", m.kind).to_ascii_lowercase();
                let (outcome, warning, guesses) = bind_component(
                    comp,
                    m,
                    conf,
                    &mut circuit,
                    &node_of,
                    &mut digital,
                    &mut mcus,
                    &mut dacs,
                    has_vreg,
                    &power_nets,
                    lib.pin_rules(),
                );
                (Some(kind_str), outcome, warning, guesses)
            }
        };

        if let Some(k) = &kind_str {
            component_kinds.insert(comp.reference.clone(), k.clone());
        }
        report.push(BindRow {
            reference: comp.reference.clone(),
            value: comp.value.clone(),
            model_id,
            confidence: conf,
            outcome,
            warning,
            guesses,
        });
    }

    // ── Assign MCP4728 I2C addresses deterministically by reference order ────
    // The board reprograms the three DACs to 0x60/0x61/0x62 during bring-up
    // (that bit-banged address-reprogramming is OUT OF SCOPE here). We model the
    // post-bring-up state: addresses 0x60+i assigned by ascending reference
    // designator (U1101->0x60, U1102->0x61, U1103->0x62). ASSUMPTION: the
    // netlist does not encode the final address per device, so by-ref ordering
    // is the deterministic stand-in; it matches the board's U1101/02/03 layout
    // and the firmware's CONF_MCP4728_ADDRS = {0x60,0x61,0x62}.
    //
    // Sort by a NATURAL key (alpha prefix, then the parsed trailing integer),
    // not raw String Ord: byte-lexicographic ordering puts "U10" before "U2",
    // which would hand out addresses in the wrong order for non-uniform-width
    // designators (U2 -> 0x61, U10 -> 0x60). Natural order gives U2 -> 0x60,
    // U10 -> 0x61.
    dacs.sort_by(|a, b| natural_ref_key(&a.reference).cmp(&natural_ref_key(&b.reference)));
    for (i, d) in dacs.iter_mut().enumerate() {
        d.address = 0x60 + i as u8;
    }

    // ── Pass 3a: stamp behavioural devices (power ICs) ──────────────────────
    // Any resolved model carrying a non-empty `[models.behavioral]` block (a
    // charger / PMIC / balancer the SPICE kinds cannot express) is stamped as a
    // behavioural device: controllable Thevenin legs + sense resistors the
    // scheduler iterates each chunk. Programmable limits read board resistor
    // values through `board_resistor`. Runs BEFORE the supply pass so a
    // converter-driven supply net is known before the ideal auto-rails attach.
    let behavioral = bind_behavioral(board, lib, &mut circuit, &node_of, custom);

    // ── Pass 3b: attach configurable power supplies ──────────────────────────
    // Every detected supply net gets a behavioral supply (default Ideal at the
    // rail's nominal voltage, electrically an ideal Vsource), unless a
    // vreg already sources that exact net (we keep the regulator chain). The
    // supply is stamped as a controllable Vsource behind a tiny series resistor
    // so the scheduler can read rail current and update the supply per chunk.
    let mut supplies: Vec<SupplyLeg> = Vec::new();
    // Deterministic order so supply indices are stable across runs.
    let mut supply_names: Vec<(&String, &f64)> = power_nets.iter().collect();
    supply_names.sort_by(|a, b| a.0.cmp(b.0));
    for (name, volts) in supply_names {
        let node = match net_nodes.get(name) {
            Some(&n) if !n.is_ground() => n,
            _ => continue,
        };
        // Skip if a vreg already drives this exact net (handled in bind).
        if circuit.devices.iter().any(|d| {
            matches!(d, Device::Vsource { p, name: dn, .. }
                if *p == node && dn.starts_with("Vreg_"))
        }) {
            continue;
        }
        // Skip a net a behavioural converter drives: the converter IS this
        // net's supply. An ideal auto-rail on top would out-stiffen the
        // converter leg, so the rail reads ideal-stiff and the converter's
        // reflected input current reads zero.
        if behavioral
            .iter()
            .any(|d| d.converter_out_node() == Some(node))
        {
            continue;
        }
        let leg = SupplyLeg::stamp(
            &mut circuit,
            node,
            name,
            PowerSupply::Ideal { volts: *volts },
        );
        supplies.push(leg);
        report.push(BindRow {
            reference: format!("RAIL:{name}"),
            value: format!("{volts:.2}V"),
            model_id: None,
            confidence: Confidence::Exact,
            outcome: BindOutcome::PowerRail { volts: *volts },
            warning: None,
            guesses: Vec::new(),
        });
    }

    // A board whose only processor was skipped for DNP still analyses fine as
    // copper, but it can no longer run firmware, and a silent zero-MCU board is
    // how a "the firmware must drive RESET high" assertion passes without ever
    // executing an instruction. Say it on the row that caused it.
    if mcus.is_empty() && !dnp_mcus.is_empty() {
        let refs = dnp_mcus
            .iter()
            .map(|(r, _)| r.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        for row in report.rows.iter_mut() {
            if dnp_mcus.iter().any(|(r, _)| r == &row.reference) {
                row.warning = Some(format!(
                    "{} is marked DNP in the board file, so it is not simulated, and it is the \
                     board's only processor: no firmware can run on this board as bound. \
                     Socketed modules (an Arduino Nano, an ESP32 carrier) are often marked DNP \
                     because they ship separately; if this one is really fitted, pass --fit {}, \
                     or add fit = [\"{}\"] to your check spec.",
                    row.reference, refs, refs
                ));
            }
        }
    }

    // ── Pass 4: gather fault-monitor metadata ───────────────────────────────
    // Match each monitorable IR device back to its component (device name ==
    // reference for the parts we stamp) and the component's resolved ratings +
    // footprint, so the stress monitor can evaluate it. Supplies/regulators are
    // matched by their Vsource device id directly.
    let device_meta = gather_device_meta(board, lib, &circuit, &mcus, &digital, &dacs);

    BoundBoard {
        name: board.name.clone(),
        circuit,
        net_nodes,
        net_names,
        digital,
        mcus,
        dnp_mcus,
        component_kinds,
        input_sources,
        supplies,
        behavioral,
        device_meta,
        dacs,
        report,
    }
}

/// Stamp every component whose resolved model carries a behavioural block.
///
/// Builds, per component, the role->node map (schematic pin functions first,
/// then the model's pad->role map, same precedence as [`role_node_map`]) and a
/// board-resistor lookup (reference designator -> parsed ohms) so a programmable
/// limit reads the actual on-board resistor. Returns the live devices.
fn bind_behavioral(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    circuit: &mut Circuit,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
    custom: &crate::behavioral::CustomRegistry,
) -> Vec<crate::behavioral::BehavioralDevice> {
    // Board resistor lookup: reference designator -> ohms, parsed from the
    // component value. Used by programmable current limits (e.g. LTC4020 ILIMIT
    // reads R8). Built once.
    let mut resistor_ohms: HashMap<String, f64> = HashMap::new();
    for comp in &board.components {
        // A DNP resistor is absent from the assembled board: the limit it
        // would have programmed must fall back to the open-resistance default.
        if comp.dnp {
            continue;
        }
        if comp.reference.starts_with('R') {
            if let Some(p) = parse_value(&comp.value) {
                resistor_ohms.insert(comp.reference.clone(), p.si);
            }
        }
    }
    let board_resistor = |refdes: &str| -> Option<f64> { resistor_ohms.get(refdes).copied() };

    let mut out = Vec::new();
    for comp in &board.components {
        // Not assembled -> no behavioural device (same rule as pass 2).
        if comp.dnp {
            continue;
        }
        let res = resolve(lib, comp);
        let model = res.model;

        // Escape-hatch keys: the resolved model id, the component value, and the
        // MPN, any of which a custom factory may be registered under.
        let model_id = model.as_ref().map(|m| m.id.clone());
        let mut keys: Vec<&str> = Vec::new();
        if let Some(id) = &model_id {
            keys.push(id.as_str());
        }
        if !comp.value.trim().is_empty() {
            keys.push(comp.value.as_str());
        }

        // role -> node for this component's connected pins (functions then pads).
        // Drop roles whose pin sits on an `unconnected-*` placeholder net: such a
        // pin is electrically no-connect, so the behavioural model must treat it
        // as absent (an FSM guard or law referencing `v_<role>` then stays
        // false / unbound). This is what makes the LTC4020 RNG/SS destabilise
        // only when the pin is genuinely DRIVEN (mb2.0's GPIO), not merely left
        // floating at 0 V (mb2.5+ NC).
        let empty_pins = std::collections::BTreeMap::new();
        let model_pins = model.as_ref().map(|m| &m.pins).unwrap_or(&empty_pins);
        let role_nets =
            role_node_map_pins(comp, model_pins, model.as_ref().map(|m| m.kind), node_of);
        let role_map: std::collections::BTreeMap<String, NodeId> = role_nets
            .into_iter()
            .filter(|(_role, node)| !circuit.node_name(*node).starts_with("unconnected-"))
            .collect();

        // 1. A registered custom behaviour wins (the escape hatch). It can bind
        //    even for a part with no declarative block / no DB model at all.
        if !custom.is_empty() {
            if let Some(boxed) = custom.build_for(&keys) {
                let params = model.as_ref().map(|m| m.params.clone()).unwrap_or_default();
                out.push(crate::behavioral::BehavioralDevice::from_custom(
                    circuit,
                    &comp.reference,
                    &params,
                    &role_map,
                    boxed,
                ));
                continue;
            }
        }

        // 2. Otherwise the declarative behavioural block, if the model has one.
        let Some(mut model) = model else { continue };
        if model.behavioral.is_empty() {
            continue;
        }
        // Resolve board-programmable params: any `<name>_from_ref = "Rxx"` param
        // is rewritten to `<name> = ohms(Rxx)`, reading the value off the board.
        // A missing board resistor means the part it programmed is gone (the
        // Reform mb2.5 fix replaced the LTC6803 tie R52 with a diode); we
        // substitute a large open resistance so a law dividing by it gives ~0.
        resolve_from_ref_params(&mut model.params, &board_resistor);
        if let Some(dev) = crate::behavioral::BehavioralDevice::stamp(
            circuit,
            &comp.reference,
            &model.behavioral,
            &model.params,
            &role_map,
            &board_resistor,
        ) {
            out.push(dev);
        }
    }
    out
}

/// Rewrite every `<name>_from_ref = "Rxx"` param into `<name> = ohms(Rxx)`,
/// reading the referenced resistor off the board. A missing resistor resolves to
/// a large open resistance (the part it programmed has been removed). The marker
/// `*_from_ref` keys are dropped afterwards.
fn resolve_from_ref_params(
    params: &mut hauksbee_models::Params,
    board_resistor: &dyn Fn(&str) -> Option<f64>,
) {
    /// Stand-in for an absent programming resistor (open circuit).
    const OPEN_OHMS: f64 = 1e12;
    let refs: Vec<(String, String)> = params
        .0
        .iter()
        .filter_map(|(k, v)| {
            let stem = k.strip_suffix("_from_ref")?;
            let refdes = v.as_str()?;
            Some((stem.to_string(), refdes.to_string()))
        })
        .collect();
    for (stem, refdes) in &refs {
        let ohms = board_resistor(refdes).unwrap_or(OPEN_OHMS);
        params.set_f64(stem.clone(), ohms);
    }
    for (stem, _) in &refs {
        params.0.remove(&format!("{stem}_from_ref"));
    }
}

/// Build the per-device metadata the stress monitor needs. Walks the bound
/// circuit and matches each monitorable device to its originating component
/// (by reference == device name) and that component's resolved ratings +
/// footprint. Vreg sources and the configurable supplies are matched too.
fn gather_device_meta(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    circuit: &Circuit,
    mcus: &[McuBinding],
    digital: &[crate::digital::DigitalComponent],
    dacs: &[DacBinding],
) -> Vec<DeviceMeta> {
    // Index components by reference, with their resolved kind/ratings/footprint.
    struct CompInfo {
        kind: ComponentKind,
        ratings: hauksbee_models::schema::Ratings,
        footprint: String,
    }
    let mut by_ref: HashMap<String, CompInfo> = HashMap::new();
    for comp in &board.components {
        let res = resolve(lib, comp);
        if let Some(m) = res.model {
            by_ref.insert(
                comp.reference.clone(),
                CompInfo {
                    kind: m.kind,
                    ratings: m.ratings.clone(),
                    footprint: comp.footprint.clone(),
                },
            );
        }
    }

    let mut metas = Vec::new();
    for (id, dev) in circuit.iter() {
        // Match analog devices by name. Multi-unit packages stamp one device
        // per unit with a suffix ("IC3906_q2", "SW1_s0", "RN1_e3" for passive
        // array elements); strip it so the package's ratings apply to every
        // unit.
        let name = dev.name();
        let base = crate::stress::strip_unit_suffix(name);
        if let Some(info) = by_ref.get(base) {
            // Only monitor kinds the evaluator knows how to score as a whole
            // analog device (one through-current / across-voltage). MCU / logic
            // / DAC / ADC parts are NOT in this list on purpose: their
            // stress-relevant quantity is per-PIN current, covered by the
            // pin-driver pass below.
            let monitor = matches!(
                info.kind,
                ComponentKind::Passive
                    | ComponentKind::Diode
                    | ComponentKind::BjtNpn
                    | ComponentKind::BjtPnp
                    | ComponentKind::Nmos
                    | ComponentKind::Pmos
                    | ComponentKind::AnalogSwitch
            );
            if monitor {
                metas.push(DeviceMeta {
                    reference: name.to_string(),
                    device: id,
                    kind: info.kind,
                    footprint: info.footprint.clone(),
                    ratings: info.ratings.clone(),
                });
            }
        }
    }

    // Vreg output sources: name is "Vreg_<ref>". A vreg whose model carries a
    // behavioural converter stamps "Vbeh_<ref>_conv" instead of the ideal
    // source; that Vsource is the same monitoring seam (its branch current is
    // the delivered output current), so it takes the same meta, otherwise a
    // converter-modelled regulator silently loses its stress/max_temp watch.
    for (id, dev) in circuit.iter() {
        if let Device::Vsource { name, .. } = dev {
            let reference = name.strip_prefix("Vreg_").or_else(|| {
                name.strip_prefix("Vbeh_")
                    .and_then(|s| s.strip_suffix("_conv"))
            });
            if let Some(reference) = reference {
                if let Some(info) = by_ref.get(reference) {
                    if info.kind == ComponentKind::Vreg {
                        metas.push(DeviceMeta {
                            reference: reference.to_string(),
                            device: id,
                            kind: ComponentKind::Vreg,
                            footprint: info.footprint.clone(),
                            ratings: info.ratings.clone(),
                        });
                    }
                }
            }
        }
    }

    // Per-pin driver legs: pin-overcurrent for MCU / logic / DAC / ADC pins.
    // These parts have no single through-current an analog meta could score,
    // what their datasheets limit is the current each PIN sources or sinks
    // (`max_pin_current_a`). Every driven pin is stamped as a Thevenin
    // [`crate::drivers::PinDriver`] (a hidden Vsource behind the output
    // resistance), and that Vsource's branch unknown IS the pin current, so
    // the honest check is one meta per driver Vsource: the monitor's generic
    // Vsource operating-point arm then reports the pin current (voltage and
    // power stay zero there, which is right, a pin check is current-only).
    // A tri-stated driver leg carries ~0 A through its 1 GΩ leg, so undriven
    // pins never false-trip. Parts whose model carries no `max_pin_current_a`
    // get no pin metas: there is no rating to check, only noise to add.
    //
    // References are keyed "<ref>:<pin>" ("U1:PB5", "U2:qa", "U3:vout_a") so a
    // fault names the offending pin; ':' never appears in stamped device names,
    // so these keys cannot collide with the _q/_s/_e unit-suffix rule.
    // Driver maps are HashMaps, so each part's pins are sorted for a
    // deterministic meta (and therefore fault/frame) order across runs.
    let push_pin = |metas: &mut Vec<DeviceMeta>, reference: &str, pin: String, vsource| {
        if let Some(info) = by_ref.get(reference) {
            if info.ratings.max_pin_current_a.is_some() {
                metas.push(DeviceMeta {
                    reference: format!("{reference}:{pin}"),
                    device: vsource,
                    kind: info.kind,
                    footprint: info.footprint.clone(),
                    ratings: info.ratings.clone(),
                });
            }
        }
    };
    for m in mcus {
        let mut pins: Vec<_> = m.gpio_drivers.iter().collect();
        pins.sort_by_key(|(pb, _)| **pb);
        for ((port, bit), drv) in pins {
            let pin = format!("P{}{bit}", port.to_ascii_uppercase());
            push_pin(&mut metas, &m.reference, pin, drv.vsource);
        }
    }
    for d in digital {
        let mut roles: Vec<_> = d.drivers.iter().collect();
        roles.sort_by_key(|(role, _)| role.as_str());
        for (role, drv) in roles {
            push_pin(&mut metas, &d.reference, role.clone(), drv.vsource);
        }
    }
    for d in dacs {
        for (ch, drv) in d.vout_drivers.iter().enumerate() {
            if let Some(drv) = drv {
                let pin = format!("vout_{}", char::from(b'a' + ch as u8));
                push_pin(&mut metas, &d.reference, pin, drv.vsource);
            }
        }
    }

    metas
}

/// Resolve one component into a model entry.
pub(crate) fn resolve(lib: &ModelLibrary, comp: &Component) -> hauksbee_models::Resolution {
    let mut q = ComponentQuery::new(
        non_empty(&comp.lib_id),
        non_empty(&comp.value),
        non_empty(&comp.footprint),
    );
    q.reference = Some(comp.reference.clone());
    // Pull a likely manufacturer part-number out of properties for mpn match.
    // Prefer a dedicated manufacturer-PN field; distributor part numbers
    // (e.g. "Mouser Part Number" = "621-BCM857BS-7-F") carry a distributor
    // prefix that breaks anchored mpn regexes, so avoid them. Fall back to the
    // value field, which is what most mpn_re rules are actually written for.
    //
    // A part number a BOM supplied sits under the reserved
    // [`hauksbee_extract::bom::MPN_PROPERTY`] key and is preferred over the
    // heuristic scan below, because the scan cannot tell a manufacturer part
    // number from a distributor code and a board carrying both would lose the
    // bind to the one that does not match.
    q.mpn = comp
        .properties
        .iter()
        .find(|(k, _)| k == hauksbee_extract::bom::MPN_PROPERTY)
        .map(|(_, v)| v.clone())
        .or_else(|| {
            comp.properties
                .iter()
                .find(|(k, _)| {
                    let k = k.to_ascii_lowercase().replace([' ', '-'], "_");
                    k.contains("mpn")
                        || k.contains("manufacturer_part")
                        || k == "part_number"
                        || k == "mfr_part"
                })
                .map(|(_, v)| v.clone())
        })
        .or_else(|| non_empty(&comp.value));
    let res = lib.resolve(&q);
    if res.confidence != Confidence::Unresolved {
        return res;
    }
    // ── Engine-layer fallbacks ──────────────────────────────────────────────
    // The model DB matches passives by `Device:R` lib_id or a THT footprint
    // prefix; layout-only extraction yields neither for SMD parts. But the
    // architecture says passives are "resolved purely from their Value field".
    // So when the library is stumped, recover R/C/L from the ref prefix +
    // a parseable value, and recover common bare-AVR MCUs by value prefix.
    if let Some(entry) = fallback_entry(comp) {
        return hauksbee_models::Resolution {
            model: Some(entry),
            confidence: Confidence::Guessed,
            query: q,
            source: Some("engine-fallback".to_string()),
            layer: None,
            origin: Some("engine-fallback".to_string()),
        };
    }
    res
}

/// Synthesize a model entry for a component the library could not resolve,
/// using only the reference-designator class and a parseable value. Returns
/// `None` when no confident fallback applies.
fn fallback_entry(comp: &Component) -> Option<ModelEntry> {
    use std::collections::BTreeMap;
    let prefix: String = comp
        .reference
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();

    // Bare AVR MCUs whose value carries a package suffix the DB regex misses
    // (e.g. "ATmega328P-PU", "ATMEGA328-AU").
    let val_up = comp.value.to_ascii_uppercase();
    if val_up.starts_with("ATMEGA328") {
        let mut params = hauksbee_models::Params::default();
        params.set_str("backend", "simavr:atmega328p");
        // Standard ATmega328P DIP-28 / TQFP-32 pad map (matches db/mcu.toml).
        let pins = atmega328p_pin_map();
        return Some(make_entry(
            "atmega328p_fallback",
            ComponentKind::Mcu,
            "ATmega328P (engine fallback by value prefix)",
            params,
            pins,
        ));
    }

    if let Some(route) = route_mcu_family(comp) {
        if let McuFamilyRoute::Backend { family, backend } = route {
            let mut params = hauksbee_models::Params::default();
            params.set_str("backend", backend);
            params.set_str("auto_bind", "family_router");
            params.set_str("auto_bind_family", family);
            let derived = derive_mcu_pin_roles(comp);
            params.set_int("auto_bind_pin_names", derived.named_pin_count as i64);
            params.set_int("auto_bind_derived_pins", derived.roles.len() as i64);
            params.set_str("auto_bind_pin_summary", pin_role_summary(&derived.roles));
            return Some(make_entry(
                &format!(
                    "{}_family_router",
                    family.to_ascii_lowercase().replace('-', "_")
                ),
                ComponentKind::Mcu,
                &format!("{family} MCU (engine family-router fallback)"),
                params,
                derived.roles,
            ));
        }
    }

    // Connector-class references: not simulatable parts; classify so they
    // don't count against resolution (J5 "Power", P1, test points, jumpers).
    if matches!(prefix.as_str(), "J" | "P" | "X" | "JP" | "TP" | "MP" | "H")
        && parse_value(&comp.value).is_none()
    {
        return Some(make_entry(
            "connector_fallback",
            ComponentKind::Ignore,
            "connector / mechanical (engine fallback by reference class)",
            Default::default(),
            BTreeMap::new(),
        ));
    }

    // Crystals / ceramic resonators must be caught BEFORE the passive
    // first-char heuristic below: their reference often starts with 'C'
    // ("Crystal1") and their value is a *frequency* ("16MHz"), so the 'C' =>
    // capacitor branch would parse "16M" as 16 megafarads. That absurd cap
    // silently wrecks the whole circuit solve (every node collapses), which in
    // co-sim makes every firmware-driven net read as "never driven / Hi-Z",
    // a false result on essentially any crystal-clocked MCU board. Bind the
    // crystal high-impedance (ignored): the co-sim clock is supplied by the MCU
    // model, and a quartz crystal's motional R-L-C is negligible at the
    // solver's operating point, so removing it changes nothing real (the two
    // load caps, which are genuine passives, stay).
    if is_crystal_like(&prefix, &comp.value) {
        return Some(make_entry(
            "crystal_fallback",
            ComponentKind::Ignore,
            "crystal / resonator (engine fallback; high-impedance, clock from MCU model)",
            Default::default(),
            BTreeMap::new(),
        ));
    }

    // Generic signal diode by reference class or footprint evidence. KiCad's
    // stock "Device:D" symbol carries the value "D" with no MPN, so the model
    // db cannot resolve it; a bare SOD/SMA/SMB/MELF/DO diode footprint with no
    // resolved model is the same case. These are real silicon junctions on the
    // board (on Tarski the ~94 D_stretch/D_inject/D_hyst pulse-stretcher
    // diodes), so leaving them OPEN silently deletes a conducting path. Mirror
    // the r_/c_/l_ fallbacks: when the part is unmistakably a 2-terminal
    // diode, bind a generic 1N4148 signal diode (datasheet-grounded params
    // below).
    //
    // The reference gate covers both the KiCad "D" class and the MIL-STD/ANSI
    // diode designators (CR, VD, ZD, VR), a "CR1" zener must never reach the
    // C-first-letter capacitor heuristic below. The footprint gate is
    // reference-independent: a diode body is a diode whatever the ref says.
    let diode_prefix = (prefix.starts_with('D') && prefix != "DAC")
        || matches!(prefix.as_str(), "CR" | "VD" | "ZD" | "VR");
    let fp = comp.footprint.to_ascii_uppercase();
    // Footprint families for small 2-pin signal/switching diodes. The "D_"
    // test is anchored to the footprint-name position (after the "Lib:"
    // separator) rather than a bare `contains("D_")`: now that this evidence
    // is consulted for EVERY reference class, a bare substring would false-
    // positive on any "..._SMD_..." footprint name.
    let fp_is_diode = fp.contains("SOD")
        || fp.contains("SMA")
        || fp.contains("SMB")
        || fp.contains("SMC")
        || fp.contains("MELF")
        || fp.contains("DO-")
        || fp.contains("DIODE")
        || fp.contains("LED")
        || fp.starts_with("D_")
        || fp.contains(":D_");
    // A value that parses only as an electrical RATING (volts/amps) is not
    // a passive magnitude: a zener marked "5.1V" is 5.1 volts of breakdown,
    // not 5.1 farads. Such a value must not veto the diode fallback, and it is
    // what tells an R/C/L reference on a diode-shaped footprint apart from a
    // real diode.
    let val_is_passive_magnitude = parse_value(&comp.value)
        .map(|p| !matches!(p.unit.as_deref(), Some("V") | Some("A")))
        .unwrap_or(false);
    let ref_is_passive = matches!(prefix.chars().next(), Some('R') | Some('C') | Some('L'));
    if diode_prefix || fp_is_diode {
        let v = comp.value.trim();
        let val_is_generic = v.is_empty()
            || v.eq_ignore_ascii_case("D")
            || v.eq_ignore_ascii_case("diode")
            || v.eq_ignore_ascii_case("1N4148")
            || v.eq_ignore_ascii_case("1N4148W")
            || v.eq_ignore_ascii_case("1N4148WS");
        // Bind the fallback when the value is a generic diode token, OR the
        // footprint is a diode body and the value is not a passive magnitude.
        if val_is_generic || (fp_is_diode && !val_is_passive_magnitude) {
            return Some(make_entry(
                "signal_diode_1n4148_fallback",
                ComponentKind::Diode,
                "generic signal diode (engine fallback: value=\"D\"/bare diode \
                 footprint -> 1N4148, Philips/Vishay SPICE model)",
                diode_1n4148_params(),
                // KiCad Device:D Sim.Pins is "1=K 2=A" (SOD cathode=pin1); the
                // netlist also carries pinfunction K/A which the binder reads
                // first, so this pad map is the PCB-only-extraction fallback.
                [("1", "cathode"), ("2", "anode")]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ));
        }
    }

    // A part whose reference class or footprint says "diode" must never fall
    // through to the R/C/L first-letter heuristic below: "CR1" with value
    // "5.1V" would land there via 'C' and bind as a 5.1 FARAD capacitor.
    // Exception: a genuine R/C/L *reference* on a diode-shaped *footprint*
    // (e.g. a 10k R_MELF resistor) whose value clearly parses as a passive
    // magnitude must not be deleted; it yields to the passive fallback below.
    // A diode *reference* (D*, CR, VD, ZD, VR) is checked first in the OR, so it
    // still always bails regardless of its first letter.
    if diode_prefix || (fp_is_diode && !(ref_is_passive && val_is_passive_magnitude)) {
        return None;
    }

    // Passives by reference class, when a magnitude can be recovered from the
    // value field. Handles plain engineering values ("4k7", "100n") and the
    // structured naming some teams use ("R_47k_0402", "C_22u_25V_0805",
    // "CTEB_2.2UF_35V_10%_...): the first underscore-separated token that
    // parses as a value wins.
    let kind_pins: Option<(ComponentKind, [(&str, &str); 2])> = match prefix.chars().next() {
        Some('R') => Some((ComponentKind::Passive, [("1", "a"), ("2", "b")])),
        Some('C') => Some((ComponentKind::Passive, [("1", "pos"), ("2", "neg")])),
        Some('L') => Some((ComponentKind::Passive, [("1", "a"), ("2", "b")])),
        _ => None,
    };
    if let Some((kind, pinmap)) = kind_pins {
        let direct = parse_value(&comp.value).is_some();
        // Token split includes parentheses: Olimex writes resistor-array values
        // both as "RMAT (4x0603) 100R/5%" and, on rev D, spaceless as
        // "RMAT(4x0603)100K/5%". Without ')' in the split set the spaceless
        // form yields no parsable token and a real 100k array was left OPEN
        // (the rev-D bind-rate drop the proof hunt flagged). "4x0603" does not
        // parse as a magnitude, so the package token cannot win by mistake.
        let structured = !direct
            && comp
                .value
                .split(['_', ' ', '(', ')'])
                .filter(|t| !t.is_empty())
                .any(|t| parse_value(t).is_some());
        if direct || structured {
            let value_hint = if direct {
                None
            } else {
                comp.value
                    .split(['_', ' ', '(', ')'])
                    .find(|t| parse_value(t).is_some())
                    .map(str::to_string)
            };
            let pins: BTreeMap<String, String> = pinmap
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let mut params = hauksbee_models::Params::default();
            if let Some(hint) = value_hint {
                params.set_str("value_override", hint);
            }
            return Some(make_entry(
                &format!("{}_fallback", prefix.to_ascii_lowercase()),
                kind,
                "passive (engine fallback from value field)",
                params,
                pins,
            ));
        }
    }
    None
}

/// True for a 2-pin crystal / ceramic resonator: a reference whose alphabetic
/// prefix is a crystal designator ("Y1", "Crystal2", "XTAL1"), or *any* part
/// whose value is a frequency ("16MHz", "32.768kHz"). The frequency test is the
/// load-bearing one: it catches a crystal whose reference starts with 'C'
/// before the passive heuristic mis-reads it as a capacitor.
fn is_crystal_like(prefix: &str, value: &str) -> bool {
    matches!(prefix, "Y" | "CRYSTAL" | "XTAL" | "RESONATOR") || value_is_frequency(value)
}

/// A value string that is *wholly* a frequency: a number, an optional k/M/G SI
/// prefix, and a trailing "Hz" ("16MHz", "8 Mhz", "32.768kHz"). The whole-value
/// test matters: a ferrite bead is conventionally valued as impedance@frequency
/// ("600@100MHz"), which also ends in "hz", but a bead sits in *series* in a
/// power/signal path, so binding it Ignore would open that path and re-create
/// the exact solve-collapse this crystal handling exists to prevent. Requiring
/// the remainder (after stripping "hz" and the SI prefix) to be purely numeric
/// rejects "600@100MHz" (the '@' survives) while accepting real crystal values.
fn value_is_frequency(value: &str) -> bool {
    let v = value.trim().to_ascii_lowercase();
    let Some(num) = v.strip_suffix("hz") else {
        return false;
    };
    // Trim AGAIN after stripping the SI prefix: a space-separated value ("16 MHz")
    // leaves the space between the magnitude and prefix exposed only once the 'm'
    // is stripped ("16 mhz" -> strip "hz" -> "16 m" -> strip 'm' -> "16 "), and
    // without this final trim the trailing space fails the all-digits check, so
    // the extremely common "16 MHz" / "32.768 kHz" form was rejected and a
    // C-prefixed crystal fell through to the capacitor heuristic (solve collapse).
    let num = num.trim().trim_end_matches(['k', 'm', 'g']).trim();
    !num.is_empty() && num.chars().all(|c| c.is_ascii_digit() || c == '.')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McuFamilyRoute {
    Backend {
        family: &'static str,
        backend: &'static str,
    },
    NoPlatform {
        family: &'static str,
    },
}

fn route_mcu_family(comp: &Component) -> Option<McuFamilyRoute> {
    if !looks_like_mcu_candidate(comp) {
        return None;
    }
    mcu_identity_strings(comp)
        .iter()
        .find_map(|s| route_mcu_family_str(s))
}

/// The backend string for a component the model DB resolved as an MCU.
///
/// The model's explicit `backend` param always wins. When the entry carries
/// none, the family router decides from the part's identity strings, a DB
/// entry that exists for strap-lint data (esp32s3, esp32s2) must not silently
/// inherit the AVR default: that sent an ESP32-S3 into simavr (wrong ISA, and
/// the GPL-gated `avr` feature the GPL-free build excludes) instead of
/// `qemu:esp32s3`. A recognized family with no co-sim platform gets an
/// explicit `none:<family>` token the scheduler refuses loudly at
/// instantiation. Only a part NO family route recognizes keeps the historical
/// `simavr:atmega328p` default (bare AVR-ish user entries).
///
/// This deliberately skips `looks_like_mcu_candidate`: the model DB already
/// classified the part as an MCU, so the reference-prefix gate (meant to stop
/// non-MCU parts reaching the router) would only mask the identity here.
fn mcu_backend_string(comp: &Component, model: &ModelEntry) -> String {
    if let Some(backend) = model.params.get_str("backend") {
        return backend.to_string();
    }
    let route = mcu_identity_strings(comp)
        .iter()
        .find_map(|s| route_mcu_family_str(s));
    match route {
        Some(McuFamilyRoute::Backend { backend, .. }) => backend.to_string(),
        Some(McuFamilyRoute::NoPlatform { family }) => {
            format!("none:{}", family.to_ascii_lowercase())
        }
        None => "simavr:atmega328p".to_string(),
    }
}

fn route_mcu_family_str(s: &str) -> Option<McuFamilyRoute> {
    for token in family_tokens(s) {
        let compact = token.replace(['-', '_', ' '], "");
        if compact.starts_with("STM32F4") {
            return Some(McuFamilyRoute::Backend {
                family: "STM32F4",
                backend: "renode:stm32f4",
            });
        }
        if compact.starts_with("STM32F1") {
            return Some(McuFamilyRoute::Backend {
                family: "STM32F1",
                backend: "renode:stm32f103",
            });
        }
        if compact.starts_with("ESP32C3") {
            return Some(McuFamilyRoute::Backend {
                family: "ESP32-C3",
                backend: "qemu:esp32c3",
            });
        }
        if compact.starts_with("ESP32S3") {
            return Some(McuFamilyRoute::Backend {
                family: "ESP32-S3",
                backend: "qemu:esp32s3",
            });
        }
        if compact.starts_with("ESP32S2") {
            return Some(McuFamilyRoute::NoPlatform { family: "ESP32-S2" });
        }
        // The RISC-V ESP32 variants (C6/C2/H2 and the P4 app processor) are a
        // different ISA from the original Xtensa ESP32. They MUST be caught
        // before the generic "ESP32" catch-all below, which would otherwise
        // mis-route them onto the Xtensa `qemu:esp32` core and silently execute
        // RISC-V firmware on the wrong machine. No platform is wired for them
        // yet, so they route to NoPlatform (the same honest treatment as S2).
        if compact.starts_with("ESP32C6") {
            return Some(McuFamilyRoute::NoPlatform { family: "ESP32-C6" });
        }
        if compact.starts_with("ESP32C2") {
            return Some(McuFamilyRoute::NoPlatform { family: "ESP32-C2" });
        }
        if compact.starts_with("ESP32H2") {
            return Some(McuFamilyRoute::NoPlatform { family: "ESP32-H2" });
        }
        if compact.starts_with("ESP32P4") {
            return Some(McuFamilyRoute::NoPlatform { family: "ESP32-P4" });
        }
        if compact.starts_with("ESP32")
            || compact.starts_with("ESPWROOM32")
            || compact.starts_with("ESP32WROOM32")
            || compact.starts_with("ESP32WROVER")
        {
            return Some(McuFamilyRoute::Backend {
                family: "ESP32",
                backend: "qemu:esp32",
            });
        }
        if compact.starts_with("NRF52") {
            return Some(McuFamilyRoute::Backend {
                family: "nRF52",
                backend: "renode:nrf52840",
            });
        }
    }
    None
}

fn looks_like_mcu_candidate(comp: &Component) -> bool {
    let prefix: String = comp
        .reference
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    if matches!(prefix.as_str(), "U" | "IC" | "MCU") {
        return true;
    }
    let lib = comp.lib_id.to_ascii_uppercase();
    lib.contains("MCU") || lib.contains("MICROCONTROLLER") || lib.contains("RF_MODULE")
}

fn mcu_identity_strings(comp: &Component) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(v) = non_empty(&comp.value) {
        out.push(v);
    }
    for (k, v) in &comp.properties {
        let key = k.to_ascii_lowercase().replace([' ', '-'], "_");
        if key.contains("mpn")
            || key.contains("manufacturer_part")
            || key == "part_number"
            || key == "mfr_part"
        {
            if let Some(v) = non_empty(v) {
                out.push(v);
            }
        }
    }
    if let Some(v) = non_empty(&comp.lib_id) {
        out.push(v);
    }
    out
}

fn family_tokens(s: &str) -> Vec<String> {
    let upper = s.to_ascii_uppercase();
    let mut tokens = vec![upper.clone()];
    tokens.extend(
        upper
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .filter(|t| !t.is_empty())
            .map(str::to_string),
    );
    tokens
}

struct DerivedMcuPins {
    roles: std::collections::BTreeMap<String, String>,
    named_pin_count: usize,
}

fn derive_mcu_pin_roles(comp: &Component) -> DerivedMcuPins {
    let mut roles = std::collections::BTreeMap::new();
    let mut named_pin_count = 0usize;
    for pin in &comp.pins {
        if pin.function.trim().is_empty() {
            continue;
        }
        named_pin_count += 1;
        if let Some(role) = role_from_mcu_pinfunction(&pin.function) {
            roles.insert(pin.number.clone(), role);
        }
    }
    DerivedMcuPins {
        roles,
        named_pin_count,
    }
}

fn role_from_mcu_pinfunction(function: &str) -> Option<String> {
    let f = function.trim();
    if f.is_empty() {
        return None;
    }
    let upper = f.to_ascii_uppercase();
    let mut base = None;
    for token in pinfunction_tokens(&upper) {
        if let Some(role) = role_from_mcu_pinfunction_token(&token) {
            base = Some(role);
            break;
        }
    }
    let mut role = base?;
    let tx = upper.contains("USART") && upper.contains("TX")
        || upper.contains("UART") && upper.contains("TX")
        || upper.contains("TXD");
    let rx = upper.contains("USART") && upper.contains("RX")
        || upper.contains("UART") && upper.contains("RX")
        || upper.contains("RXD");
    if tx && !role.ends_with("_txd") && (role.starts_with('p') || role.starts_with("gpio")) {
        role.push_str("_txd");
    } else if rx && !role.ends_with("_rxd") && (role.starts_with('p') || role.starts_with("gpio")) {
        role.push_str("_rxd");
    }
    Some(role)
}

fn pinfunction_tokens(upper: &str) -> Vec<String> {
    let mut tokens = vec![upper.to_string()];
    tokens.extend(
        upper
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(str::to_string),
    );
    tokens
}

fn role_from_mcu_pinfunction_token(token: &str) -> Option<String> {
    if token == "BOOT0" {
        return Some("boot0".to_string());
    }
    if token.starts_with("VDD") || token.starts_with("VCC") {
        return Some("vdd".to_string());
    }
    if token.starts_with("VSS") || token == "GND" {
        return Some("vss".to_string());
    }
    if let Some(rest) = token.strip_prefix('P') {
        let mut chars = rest.chars();
        let port = chars.next()?;
        // STM32 (and larger AVR/other) parts expose ports well past E: an
        // STM32F4/F7 in a 100+-pin package has GPIO banks up to port I (PF/PG/
        // PH/PI). Capping at E silently dropped every pin on those banks.
        if !matches!(port, 'A'..='I') {
            return None;
        }
        let digits: String = chars.take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() || digits.len() > 2 {
            return None;
        }
        if digits.parse::<u8>().ok()? < 32 {
            return Some(format!("p{}{}", port.to_ascii_lowercase(), digits));
        }
    }
    if let Some(rest) = token.strip_prefix("GPIO") {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && digits.parse::<u8>().ok()? < 64 {
            return Some(format!("gpio{digits}"));
        }
    }
    None
}

fn pin_role_summary(pins: &std::collections::BTreeMap<String, String>) -> String {
    let mut roles: Vec<&str> = pins
        .values()
        .filter(|role| gpio_of_role(role, false).is_some())
        .map(String::as_str)
        .collect();
    roles.sort_unstable();
    roles.dedup();
    match (roles.first(), roles.last()) {
        (Some(first), Some(last)) if first != last => format!("{first}..{last}"),
        (Some(only), _) => (*only).to_string(),
        _ => "no GPIO roles".to_string(),
    }
}

/// Datasheet-grounded SPICE parameters for a generic 1N4148 small-signal
/// switching diode, used by the value-"D" diode fallback. Values are the
/// canonical Philips/Vishay 1N4148 model (the part on Tarski is LCSC C2099 =
/// JSCJ 1N4148W in SOD-323, a standard 1N4148-family switching diode):
///   IS=4.352e-9 A, N=1.906, RS=0.6458 Ohm, CJO=7.048e-13 F, VJ=0.869 V,
///   M=0.0306, TT=3.48e-9 s, BV=110 V.
/// Source: Philips/Vishay 1N4148 .MODEL card (the de-facto reference set,
/// e.g. spice-padiwa-amps/1N4148.lib); BOM part LCSC C2099 (1N4148W).
fn diode_1n4148_params() -> hauksbee_models::Params {
    let mut p = hauksbee_models::Params::default();
    p.set_f64("is", 4.352e-9);
    p.set_f64("n", 1.906);
    p.set_f64("rs", 0.6458);
    p.set_f64("cjo", 7.048e-13);
    p.set_f64("vj", 0.869);
    p.set_f64("m", 0.0306);
    p.set_f64("tt", 3.48e-9);
    p.set_f64("bv", 110.0);
    p
}

/// Construct a [`ModelEntry`] for an engine-layer fallback. Centralised so a
/// schema change (new optional fields) is handled in one place.
fn make_entry(
    id: &str,
    kind: ComponentKind,
    description: &str,
    params: hauksbee_models::Params,
    pins: std::collections::BTreeMap<String, String>,
) -> ModelEntry {
    // Parse a tiny TOML stub to get a fully-defaulted entry, then fill it in.
    // This keeps us forward-compatible with new `#[serde(default)]` fields on
    // ModelEntry without hand-listing them.
    let stub = format!("id = \"{id}\"\nkind = \"{}\"\n", kind_str(kind));
    let mut entry: ModelEntry = toml::from_str(&stub).expect("fallback ModelEntry stub parses");
    entry.description = description.to_string();
    entry.params = params;
    entry.pins = pins;
    entry
}

/// The snake_case discriminant string for a [`ComponentKind`] (matches the
/// TOML `#[serde(rename_all = "snake_case")]` encoding).
fn kind_str(kind: ComponentKind) -> &'static str {
    use ComponentKind::*;
    match kind {
        Passive => "passive",
        Diode => "diode",
        BjtNpn => "bjt_npn",
        BjtPnp => "bjt_pnp",
        Nmos => "nmos",
        Pmos => "pmos",
        Vreg => "vreg",
        Opamp => "opamp",
        Comparator => "comparator",
        AnalogSwitch => "analog_switch",
        Digital => "digital",
        Dac => "dac",
        Adc => "adc",
        ShiftRegister => "shift_register",
        Mcu => "mcu",
        Connector => "connector",
        Ignore => "ignore",
    }
}

/// The ATmega328P DIP-28 / TQFP-32 pad→role map (mirrors db/mcu.toml).
fn atmega328p_pin_map() -> std::collections::BTreeMap<String, String> {
    [
        ("1", "pc6_reset"),
        ("2", "pd0_rxd"),
        ("3", "pd1_txd"),
        ("4", "pd2_int0"),
        ("5", "pd3_int1_oc2b"),
        ("6", "pd4_t0_xck"),
        ("7", "vcc"),
        ("8", "gnd"),
        ("9", "pb6_xtal1"),
        ("10", "pb7_xtal2"),
        ("11", "pd5_t1_oc0b"),
        ("12", "pd6_ain0_oc0a"),
        ("13", "pd7_ain1"),
        ("14", "pb0_icp1"),
        ("15", "pb1_oc1a"),
        ("16", "pb2_ss_oc1b"),
        ("17", "pb3_mosi_oc2a"),
        ("18", "pb4_miso"),
        ("19", "pb5_sck"),
        ("20", "avcc"),
        ("21", "aref"),
        ("22", "gnd2"),
        ("23", "pc0_adc0"),
        ("24", "pc1_adc1"),
        ("25", "pc2_adc2"),
        ("26", "pc3_adc3"),
        ("27", "pc4_adc4_sda"),
        ("28", "pc5_adc5_scl"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn non_empty(s: &str) -> Option<String> {
    if s.trim().is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Build the outcome for a component with no resolved model.
fn unresolved_outcome(
    comp: &Component,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> (Option<String>, BindOutcome, Option<String>) {
    if let Some(McuFamilyRoute::NoPlatform { family }) = route_mcu_family(comp) {
        let msg = format!(
            "[auto-bind] {} \"{}\" recognized {family} but no co-sim platform; leaving UNRESOLVED. Override with a --models-dir entry.",
            comp.reference, comp.value
        );
        eprintln!("{msg}");
        return (
            None,
            BindOutcome::Unresolved {
                reason: format!("recognized {family} but no co-sim platform"),
            },
            Some(msg),
        );
    }
    let connected = comp.pins.iter().any(|p| node_of(p.net).is_some());
    let two_terminal = comp.pins.len() == 2;
    let warning = if connected {
        Some(format!(
            "unresolved part '{}' ({}) on connected net(s): defaulting to OPEN circuit",
            comp.reference, comp.value
        ))
    } else {
        None
    };
    // A valueless Altium part carries the extractor's explanation as a
    // component property; surface it as the row's reason so the table says
    // WHY there is nothing to resolve, not a bare "no model".
    let value_unresolved = comp
        .properties
        .iter()
        .find(|(k, _)| k == hauksbee_extract::altium::VALUE_UNRESOLVED_KEY)
        .map(|(_, v)| v.clone());
    let reason = match value_unresolved {
        Some(why) => why,
        None if two_terminal => "no model; left open".to_string(),
        None => "no model".to_string(),
    };
    (None, BindOutcome::Unresolved { reason }, warning)
}

/// Bind a single resolved component. Mutates the circuit and the digital/mcu
/// collections; returns its outcome and any warning.
#[allow(clippy::too_many_arguments)]
fn bind_component(
    comp: &Component,
    model: &ModelEntry,
    _conf: Confidence,
    circuit: &mut Circuit,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
    digital: &mut Vec<DigitalComponent>,
    mcus: &mut Vec<McuBinding>,
    dacs: &mut Vec<DacBinding>,
    has_vreg: bool,
    power_nets: &HashMap<String, f64>,
    pin_rules: &hauksbee_models::PinRuleTable,
) -> (BindOutcome, Option<String>, Vec<String>) {
    use ComponentKind::*;

    // role -> node for this component's connected pins. Explicit pin-functions
    // and the model's pad map come first; for any role-dependent pad still
    // without a role the pin-rule table is consulted, and each such inference is
    // recorded as a guess-warning (so nothing is silently guessed).
    let (role_nets, guesses) = role_node_map_guessed(comp, model, node_of, pin_rules);
    // pad number -> node, regardless of role.
    let pad_nodes = |pad: &str| -> Option<NodeId> {
        comp.pins
            .iter()
            .find(|p| p.number == pad)
            .and_then(|p| node_of(p.net))
    };

    let (outcome, warning) = match model.kind {
        Passive => bind_passive(comp, model, circuit, node_of),
        Diode => bind_diode(comp, model, circuit, &role_nets),
        BjtNpn | BjtPnp => bind_bjt(comp, model, circuit, &role_nets),
        Nmos | Pmos => bind_mosfet(comp, model, circuit, &role_nets),
        Vreg => bind_vreg(comp, model, circuit, &role_nets, has_vreg),
        Opamp => bind_opamp(comp, model, circuit, &role_nets),
        Comparator => bind_comparator(comp, model, circuit, &role_nets),
        AnalogSwitch => bind_analog_switch(comp, model, circuit, &role_nets, power_nets),
        Digital | ShiftRegister => {
            let kind = if model.kind == ShiftRegister {
                "shift_register"
            } else {
                "digital"
            };
            match bind_digital(comp, model, circuit, &role_nets, digital) {
                Ok(()) => (
                    BindOutcome::Digital {
                        kind: kind.to_string(),
                    },
                    None,
                ),
                // A part whose logic spec does not compile is NOT bound: its
                // nets float. Reporting it as `Digital` anyway made a broken
                // part look healthy in every report surface (including
                // `critical_parts_bound`); record the truth and warn.
                Err(e) => (
                    BindOutcome::Unresolved {
                        reason: format!("invalid [models.logic]: {e}"),
                    },
                    Some(format!(
                        "{} ({}): invalid [models.logic] in model '{}': {e}; the part is \
                         unmodeled and its output nets float",
                        comp.reference, comp.value, model.id
                    )),
                ),
            }
        }
        Dac => {
            // MCP4728-class quad I2C DAC: stamp Thevenin drivers on the four
            // VOUT channel nets and record a DacBinding. The scheduler creates
            // the I2C slave and pushes computed VOUT onto these drivers each
            // chunk. The I2C transactions reach the slave through the MCU's TWI
            // `on_i2c` hook, not these nets, so no digital buffer is stamped.
            bind_mcp4728_dac(comp, model, circuit, &role_nets, dacs)
        }
        Adc => {
            // Treated as a behavioral passthrough buffer for now.
            match bind_digital(comp, model, circuit, &role_nets, digital) {
                Ok(()) => (
                    BindOutcome::Digital {
                        kind: "adc".to_string(),
                    },
                    None,
                ),
                Err(e) => (
                    BindOutcome::Unresolved {
                        reason: format!("invalid [models.logic]: {e}"),
                    },
                    Some(format!(
                        "{} ({}): invalid [models.logic] in model '{}': {e}; the part is \
                         unmodeled and its output nets float",
                        comp.reference, comp.value, model.id
                    )),
                ),
            }
        }
        Mcu => {
            let backend = mcu_backend_string(comp, model);
            let warning = bind_mcu(comp, model, circuit, node_of, &pad_nodes, power_nets, mcus);
            (BindOutcome::Mcu { backend }, warning)
        }
        Connector => (
            BindOutcome::Skipped {
                reason: "connector".to_string(),
            },
            None,
        ),
        Ignore => (
            BindOutcome::Skipped {
                reason: "ignored".to_string(),
            },
            None,
        ),
    };
    (outcome, warning, guesses)
}

/// Map each connected pin to its model role string.
///
/// Schematic pinfunctions are authoritative when they carry recognizable
/// electrode names; they encode what the symbol's author connected, which
/// the model db's by-pin-number map cannot know for vendor symbols with
/// nonstandard numbering. The db map fills any remaining pins (and is the
/// only source for PCB-only extraction, where pinfunctions are empty).
fn role_node_map(
    comp: &Component,
    model: &ModelEntry,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> HashMap<String, NodeId> {
    role_node_map_pins(comp, &model.pins, Some(model.kind), node_of)
}

/// As [`role_node_map`] but, for any pad still without a role after the explicit
/// pin-function and model-pad-map sources, consults the configurable pin-rule
/// table. Returns the role map plus one guess-warning per pad whose role a rule
/// supplied (naming the component, pad, role, and rule id), so an inferred role
/// is never silent.
///
/// Precedence is preserved: a role an explicit pin-function or the model's pad
/// map already filled is left untouched (no rule, no warning). The rule only
/// fills a *gap*, exactly the layout-only case where the pad carries a bare
/// number and no electrode name.
fn role_node_map_guessed(
    comp: &Component,
    model: &ModelEntry,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
    pin_rules: &hauksbee_models::PinRuleTable,
) -> (HashMap<String, NodeId>, Vec<String>) {
    let mut m = role_node_map(comp, model, node_of);
    let mut guesses = Vec::new();

    // Nothing to infer when the rule table is empty or the part has no pads.
    if pin_rules.is_empty() {
        return (m, guesses);
    }
    let pad_count = comp.pins.len();
    // The set of roles already filled by an explicit source, never overwritten.
    for pin in &comp.pins {
        let Some(node) = node_of(pin.net) else {
            continue;
        };
        let Some(inf) =
            pin_rules.role_for_pad(&comp.footprint, Some(model.kind), pad_count, &pin.number)
        else {
            continue;
        };
        // Only a guess if this role is not already present from an explicit
        // pin-function or the model pad map.
        if m.contains_key(&inf.role) {
            continue;
        }
        m.insert(inf.role.clone(), node);
        guesses.push(format!(
            "pad {} role '{}' guessed from rule '{}' (no explicit pin-function; \
             footprint \"{}\", {} pads)",
            pin.number, inf.role, inf.rule_id, comp.footprint, pad_count,
        ));
    }
    (m, guesses)
}

/// As [`role_node_map`] but taking the pad->role map and (optional) kind
/// directly, so a custom behaviour with no full [`ModelEntry`] can still build a
/// role map from a model's `[models.pins]`.
fn role_node_map_pins(
    comp: &Component,
    pins: &std::collections::BTreeMap<String, String>,
    kind: Option<ComponentKind>,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> HashMap<String, NodeId> {
    let mut m = HashMap::new();
    if let Some(kind) = kind {
        for pin in &comp.pins {
            let Some(node) = node_of(pin.net) else {
                continue;
            };
            if let Some(role) = role_from_pinfunction(kind, &pin.function) {
                m.entry(role).or_insert(node);
            }
        }
    }
    // The db's by-pin-number map fills any role the functions did not cover
    // (and is the only source for PCB-only extraction, where functions are
    // empty).
    for pin in &comp.pins {
        if let (Some(role), Some(node)) = (pins.get(&pin.number), node_of(pin.net)) {
            m.entry(role.clone()).or_insert(node);
        }
    }
    // Electrode-letter pad "numbers" fill any remaining gap. Eagle-style and
    // vendor footprints name pads by electrode ("A"/"K" on a diode, "G"/"D"/
    // "S" on a MOSFET, "C"/"B"/"E" on a BJT) instead of numbering them; with
    // no pinfunction (footprint-only extraction) and a numerically-keyed
    // model pad map, such a part would otherwise match nothing and bind OPEN,
    // silently deleting a real junction. The letters reuse the kind-aware
    // pinfunction vocabulary, so "C" stays cathode-on-a-diode and
    // collector-on-a-BJT.
    if let Some(kind) = kind {
        for pin in &comp.pins {
            let Some(node) = node_of(pin.net) else {
                continue;
            };
            if let Some(role) = role_from_electrode_pad(kind, &pin.number) {
                m.entry(role).or_insert(node);
            }
        }
    }
    // Eagle "P$1"/"P$2" ordinal pads: strip the "P$" prefix and re-consult the
    // model's by-pin-number map, so a P$-numbered footprint still reaches a
    // "1"/"2"-keyed pad map.
    for pin in &comp.pins {
        let Some(ord) = ordinal_pad(&pin.number) else {
            continue;
        };
        if let (Some(role), Some(node)) = (pins.get(ord.as_str()), node_of(pin.net)) {
            m.entry(role.clone()).or_insert(node);
        }
    }
    m
}

/// Interpret an electrode-letter pad "number" as a binder role for `kind`.
/// Only the kinds whose pads are conventionally lettered participate (diode,
/// BJT, MOSFET), on everything else a lettered pad is not an electrode name.
/// A purely numeric pad is a pad number, never an electrode.
fn role_from_electrode_pad(kind: ComponentKind, pad: &str) -> Option<String> {
    use ComponentKind::*;
    if !matches!(kind, Diode | BjtNpn | BjtPnp | Nmos | Pmos) {
        return None;
    }
    let p = pad.trim();
    if !p.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    // Reuse the kind-aware pinfunction vocabulary ("A"->anode on a diode,
    // "B1"->base_q1 on a dual BJT, ...), which already rejects non-electrode
    // tokens per kind.
    role_from_pinfunction(kind, p)
}

/// Eagle ordinal pad names: "P$1" -> "1". Returns `None` for anything else.
fn ordinal_pad(pad: &str) -> Option<String> {
    let p = pad.trim();
    let rest = p.strip_prefix("P$").or_else(|| p.strip_prefix("p$"))?;
    (!rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())).then(|| rest.to_string())
}

/// Normalize a schematic pinfunction into a binder role. Pin names are only
/// meaningful per component kind ("B1" is a base on a transistor pair but a
/// throw on an SPDT switch), so the resolved model kind disambiguates.
fn role_from_pinfunction(kind: ComponentKind, function: &str) -> Option<String> {
    let f = function.trim().to_ascii_lowercase();
    if f.is_empty() {
        return None;
    }
    let role: &str = match kind {
        ComponentKind::BjtNpn | ComponentKind::BjtPnp => match f.as_str() {
            "b" | "base" => "base",
            "c" | "collector" => "collector",
            "e" | "emitter" => "emitter",
            "e1" => "emitter_q1",
            "b1" => "base_q1",
            "c1" => "collector_q1",
            "e2" => "emitter_q2",
            "b2" => "base_q2",
            "c2" => "collector_q2",
            _ => return None,
        },
        ComponentKind::Nmos | ComponentKind::Pmos => match f.as_str() {
            "g" | "gate" => "gate",
            "d" | "drain" => "drain",
            "s" | "source" => "source",
            _ => return None,
        },
        ComponentKind::Diode => match f.as_str() {
            "a" | "anode" => "anode",
            "k" | "c" | "cathode" => "cathode",
            _ => return None,
        },
        ComponentKind::AnalogSwitch => match f.as_str() {
            "a" | "com" => "com",
            // s0 is the throw that conducts when the control/select is LOW (see
            // bind_analog_switch). By the universal SPDT convention the
            // Normally-Closed contact is the one tied to COM at rest / control-low
            // and Normally-Open closes on control-high, so NC → s0, NO → s1.
            // (Swapping the two routes COM to the wrong throw in every control
            // state on any board that names its throws NO/NC.)
            "b1" | "s0" | "nc" => "s0",
            "b2" | "s1" | "no" => "s1",
            "s" | "sel" | "in" | "ctrl" => "ctrl",
            "gnd" | "vss" => "vss",
            "vcc" | "vdd" => "vcc",
            _ => return None,
        },
        ComponentKind::Opamp | ComponentKind::Comparator => match f.as_str() {
            "out" | "output" | "q" => "out",
            "+in" | "in+" | "inp" | "in_p" | "in_plus" | "non-inverting" => "in_plus",
            "-in" | "in-" | "inn" | "in_n" | "in_minus" | "inverting" => "in_minus",
            _ => return None,
        },
        // MCP4728-class DACs: the schematic VOUTA..VOUTD / SDA / SCL / ~{LDAC}
        // pin names are authoritative over the db's pad-number map (which some
        // symbol libraries number differently). This pins each VOUT channel to
        // the right analog net regardless of MSOP vs symbol pin numbering.
        ComponentKind::Dac => match f.as_str() {
            "vouta" | "vout_a" => "vout_a",
            "voutb" | "vout_b" => "vout_b",
            "voutc" | "vout_c" => "vout_c",
            "voutd" | "vout_d" => "vout_d",
            "sda" => "sda",
            "scl" => "scl",
            "ldac" | "~{ldac}" | "ldac_n" => "ldac_n",
            "vdd" => "vdd",
            "vss" | "gnd" => "vss",
            _ => return None,
        },
        _ => return None,
    };
    Some(role.to_string())
}

// ── Per-kind binders ────────────────────────────────────────────────────────

fn bind_passive(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> (BindOutcome, Option<String>) {
    // A fallback entry may carry the magnitude recovered from a structured
    // value string ("R_47k_0402" -> "47k").
    let effective_value = model
        .params
        .get_str("value_override")
        .unwrap_or(&comp.value);
    let parsed = parse_value(effective_value);
    // A passive with more than two pads is an ARRAY (a bussed or isolated
    // R/C network), not one 2-terminal element. Silently taking the first two
    // pads and stamping ONE device deletes every other element in the pack: a
    // 4-resistor array becomes a single resistor. Split it into per-element
    // devices instead.
    if comp.pins.len() > 2 {
        return bind_passive_array(comp, parsed.as_ref(), circuit, node_of);
    }
    let (a, b) = two_terminal_nodes(comp, node_of);
    let (Some(a), Some(b)) = (a, b) else {
        return (
            BindOutcome::Unresolved {
                reason: "passive not connected at both ends".to_string(),
            },
            Some(format!(
                "{} ({}): passive missing a connection, left open",
                comp.reference, comp.value
            )),
        );
    };
    let Some(p) = parsed else {
        return (
            BindOutcome::Unresolved {
                reason: format!("unparseable value '{}'", comp.value),
            },
            Some(format!(
                "{}: value '{}' not parseable, left open",
                comp.reference, comp.value
            )),
        );
    };
    let (device, note) = passive_device(comp, comp.reference.clone(), a, b, &p);
    let label = device_label(&device);
    circuit.add(device);
    (BindOutcome::Analog { device: label }, note)
}

/// Resistance a literal 0 Ω resistor is bound at.
///
/// A `0` in a resistor's value field is a JUMPER: a zero-ohm link placed to
/// route a trace, strap an option, or leave a cut point. It is not a
/// mathematical short. Binding it at the solver's 1 µΩ floor stamps 1e6 S into
/// a matrix whose real entries are milli-siemens, and the resulting condition
/// number is what made a 259-part board (anyshake/explorer, ten `0` resistors)
/// unsolvable with no element named. A milliohm is the physical truth of an
/// 0402 jumper's end-to-end resistance, is three decades better conditioned,
/// and is far below anything a board's behaviour can distinguish from a short.
const ZERO_OHM_JUMPER_OHMS: f64 = 1e-3;

/// Build the concrete R / C / L device for the passive `comp` between `a` and
/// `b`, deciding the kind from the reference-designator prefix and the parsed
/// unit. Shared by the single-element path and the array path so both stay in
/// lockstep on the R/C/L decision. The optional string is a bind warning to
/// surface on the component's report row (currently only the 0-ohm case).
fn passive_device(
    comp: &Component,
    name: String,
    a: NodeId,
    b: NodeId,
    p: &hauksbee_models::value::ParsedValue,
) -> (Device, Option<String>) {
    let unit = p.unit.as_deref().unwrap_or("");
    let prefix = comp
        .reference
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    if prefix.starts_with('C') || unit.eq_ignore_ascii_case("F") {
        (
            Device::Capacitor {
                name,
                a,
                b,
                farads: p.si,
                ic: None,
            },
            None,
        )
    } else if prefix.starts_with('L') || unit.eq_ignore_ascii_case("H") {
        (
            Device::Inductor {
                name,
                a,
                b,
                henries: p.si,
                ic: None,
            },
            None,
        )
    } else if p.si <= 0.0 {
        // A literal 0 (or negative) resistance is a fitted jumper link, not a
        // mathematical short. Binding it at the raw 1e-6 floor leaves ten
        // near-short conductances that wreck the analog solve's conditioning
        // (anyshake/explorer). Bind it at the same milliohm the supply legs use
        // for "electrically negligible but solver-safe", and SAY SO: the value
        // on the board is then not the value in the matrix, and a silent
        // substitution is exactly the class of thing this project refuses.
        let note = format!(
            "{name}: value '{}' is a 0 ohm jumper, bound as a {:.0} mohm link so the solve stays finite (an infinite conductance would poison the matrix)",
            comp.value,
            ZERO_OHM_JUMPER_OHMS * 1e3,
        );
        (
            Device::Resistor {
                name,
                a,
                b,
                ohms: ZERO_OHM_JUMPER_OHMS,
                tc1: None,
            },
            Some(note),
        )
    } else {
        (
            Device::Resistor {
                name,
                a,
                b,
                ohms: p.si.max(1e-6),
                tc1: None,
            },
            None,
        )
    }
}

/// Bind a multi-pad passive array (resistor network / capacitor array) as one
/// device PER ELEMENT, never as a single 2-terminal element.
///
/// Pad conventions:
///   - EVEN pad count -> isolated array: sequential pad pairs in natural pad
///     order (1-2, 3-4, …) each carry one element at the pack's per-element
///     value (the value field of an array is per element, not the pack total).
///   - ODD pad count -> assumed BUSSED array: the lowest-numbered pad is the
///     shared common and every other pad carries one element to it. That
///     pairing is a convention, not a certainty, so this variant always binds
///     WITH a loud warning naming the assumption, never a silent guess.
///
/// Elements are stamped as `<ref>_e<n>`; [`gather_device_meta`] strips the
/// suffix so the pack's ratings apply to every element.
fn bind_passive_array(
    comp: &Component,
    parsed: Option<&hauksbee_models::value::ParsedValue>,
    circuit: &mut Circuit,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> (BindOutcome, Option<String>) {
    let Some(p) = parsed else {
        return (
            BindOutcome::Unresolved {
                reason: format!("unparseable value '{}'", comp.value),
            },
            Some(format!(
                "{}: value '{}' not parseable, left open",
                comp.reference, comp.value
            )),
        );
    };
    // Natural pad order: numeric pads sort numerically ("2" before "10"), any
    // non-numeric pads after them lexicographically.
    let mut pads: Vec<&hauksbee_extract::Pin> = comp.pins.iter().collect();
    pads.sort_by_key(|pin| {
        (
            pin.number.trim().parse::<u64>().unwrap_or(u64::MAX),
            pin.number.clone(),
        )
    });

    let mut notes: Vec<String> = Vec::new();
    // (element name, a, b) before connectivity filtering.
    let mut elements: Vec<(String, Option<NodeId>, Option<NodeId>)> = Vec::new();
    if pads.len() % 2 == 0 {
        // Isolated-array convention: sequential pad pairs.
        for (i, pair) in pads.chunks(2).enumerate() {
            elements.push((
                format!("{}_e{}", comp.reference, i + 1),
                node_of(pair[0].net),
                node_of(pair[1].net),
            ));
        }
        // Sequential 1-2/3-4 pairing is as much a convention as the odd-count
        // bussed one, a mirror-pinout isolated array (some Bourns SIP packs
        // pair 1-8/2-7) would pair differently, and that is not knowable from
        // the netlist alone. Emit the same "verify against the datasheet" note
        // the odd branch does, rather than binding silently.
        notes.push(format!(
            "{}: {}-pad passive array bound as sequential isolated pairs \
             (1-2, 3-4, …) by convention; verify against the part's datasheet",
            comp.reference,
            pads.len(),
        ));
    } else {
        // Odd pad count: ambiguous. Bind the common bussed convention (lowest
        // pad shared), but loudly; the report must show the assumption.
        let common = node_of(pads[0].net);
        for (i, pin) in pads[1..].iter().enumerate() {
            elements.push((
                format!("{}_e{}", comp.reference, i + 1),
                common,
                node_of(pin.net),
            ));
        }
        notes.push(format!(
            "{}: {}-pad passive array is ambiguous; bound as BUSSED by convention \
             (pad {} common, {} elements); verify against the part's datasheet",
            comp.reference,
            pads.len(),
            pads[0].number,
            pads.len() - 1,
        ));
    }

    let mut stamped = 0usize;
    let mut label = String::new();
    for (name, a, b) in elements {
        let (Some(a), Some(b)) = (a, b) else {
            notes.push(format!(
                "{name}: array element missing a connection, left open"
            ));
            continue;
        };
        let (device, note) = passive_device(comp, name, a, b, p);
        if let Some(note) = note {
            notes.push(note);
        }
        if stamped == 0 {
            label = device_label(&device);
        }
        circuit.add(device);
        stamped += 1;
    }
    let warning = (!notes.is_empty()).then(|| notes.join("; "));
    if stamped == 0 {
        return (
            BindOutcome::Unresolved {
                reason: "passive array has no fully-connected element".to_string(),
            },
            warning.or_else(|| {
                Some(format!(
                    "{} ({}): passive array missing connections, left open",
                    comp.reference, comp.value
                ))
            }),
        );
    }
    (
        BindOutcome::Analog {
            device: format!("{label} x{stamped}"),
        },
        warning,
    )
}

fn two_terminal_nodes(
    comp: &Component,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> (Option<NodeId>, Option<NodeId>) {
    let mut it = comp.pins.iter();
    let a = it.next().and_then(|p| node_of(p.net));
    let b = it.next().and_then(|p| node_of(p.net));
    (a, b)
}

fn bind_diode(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
) -> (BindOutcome, Option<String>) {
    let a = roles
        .get("anode")
        .or_else(|| pick(roles, &["a", "p"]))
        .copied();
    let k = roles
        .get("cathode")
        .or_else(|| pick(roles, &["k", "n"]))
        .copied();
    let (Some(a), Some(k)) = (a, k) else {
        return open_warning(comp, "diode pins not both connected");
    };
    let m = diode_model_from(model);
    circuit.add(Device::Diode {
        name: comp.reference.clone(),
        a,
        k,
        model: m,
    });
    (
        BindOutcome::Analog {
            device: "diode".to_string(),
        },
        None,
    )
}

fn diode_model_from(model: &ModelEntry) -> DiodeModel {
    let d = DiodeModel::default();
    let p = &model.params;
    DiodeModel {
        is: p.get_f64("is").unwrap_or(d.is),
        n: p.get_f64("n").unwrap_or(d.n),
        rs: p.get_f64("rs").unwrap_or(d.rs),
        cjo: p.get_f64("cjo").unwrap_or(d.cjo),
        vj: p.get_f64("vj").unwrap_or(d.vj),
        m: p.get_f64("m").unwrap_or(d.m),
        tt: p.get_f64("tt").unwrap_or(d.tt),
        bv: p.get_f64("bv").unwrap_or(d.bv),
        // A Zener entry states its voltage at a test current; both are needed
        // for the knee to be sharp enough to regulate.
        ibv: p.get_f64("ibv").filter(|v| *v > 0.0),
        xti: p.get_f64("xti").unwrap_or(d.xti),
        eg: p.get_f64("eg").unwrap_or(d.eg),
    }
}

fn bind_bjt(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
) -> (BindOutcome, Option<String>) {
    let m = bjt_model_from(model);

    // Multi-transistor packages (matched pairs like BCM847BS/BCM857BS) use
    // suffixed roles: collector_q1/base_q1/emitter_q1, ..._q2. Stamp one
    // BJT per suffix group, sharing the package's model card.
    let suffixes: std::collections::BTreeSet<String> = roles
        .keys()
        .filter_map(|r| r.rsplit_once("_q").map(|(_, n)| format!("_q{n}")))
        .collect();
    if !suffixes.is_empty() {
        let mut stamped = 0;
        let mut partial: Vec<String> = Vec::new();
        for suffix in &suffixes {
            let get = |role: &str| roles.get(&format!("{role}{suffix}")).copied();
            let (Some(c), Some(b), Some(e)) = (get("collector"), get("base"), get("emitter"))
            else {
                // A suffix is present only if >=1 of its pins is wired, so an
                // incomplete c/b/e here is a genuine PARTIAL miswire (not an
                // unused half). Record it so the bind report warns, matching the
                // single-BJT open_warning and the passive-array "left open" note.
                partial.push(suffix.trim_start_matches('_').to_string());
                continue;
            };
            circuit.add(Device::Bjt {
                name: format!("{}{}", comp.reference, suffix),
                c,
                b,
                e,
                model: m.clone(),
            });
            stamped += 1;
        }
        if stamped == 0 {
            return open_warning(comp, "paired BJT: no complete c/b/e group connected");
        }
        let warning = if partial.is_empty() {
            None
        } else {
            Some(format!(
                "{}: paired BJT unit(s) {} missing a c/b/e connection, left open",
                comp.reference,
                partial.join(", ")
            ))
        };
        return (
            BindOutcome::Analog {
                device: format!("bjt x{stamped}"),
            },
            warning,
        );
    }

    let c = roles
        .get("collector")
        .or_else(|| pick(roles, &["c"]))
        .copied();
    let b = roles.get("base").or_else(|| pick(roles, &["b"])).copied();
    let e = roles
        .get("emitter")
        .or_else(|| pick(roles, &["e"]))
        .copied();
    let (Some(c), Some(b), Some(e)) = (c, b, e) else {
        return open_warning(comp, "BJT pins not all connected");
    };
    circuit.add(Device::Bjt {
        name: comp.reference.clone(),
        c,
        b,
        e,
        model: m,
    });
    (
        BindOutcome::Analog {
            device: "bjt".to_string(),
        },
        None,
    )
}

fn bjt_model_from(model: &ModelEntry) -> BjtModel {
    let polarity = if model.kind == ComponentKind::BjtPnp {
        Polarity::P
    } else {
        Polarity::N
    };
    let d = BjtModel::default();
    let p = &model.params;
    BjtModel {
        polarity,
        is: p.get_f64("is").unwrap_or(d.is),
        bf: p.get_f64("bf").unwrap_or(d.bf),
        br: p.get_f64("br").unwrap_or(d.br),
        vaf: p.get_f64("vaf").unwrap_or(d.vaf),
        var: p.get_f64("var").unwrap_or(d.var),
        nf: p.get_f64("nf").unwrap_or(d.nf),
        nr: p.get_f64("nr").unwrap_or(d.nr),
        rb: p.get_f64("rb").unwrap_or(d.rb),
        re: p.get_f64("re").unwrap_or(d.re),
        rc: p.get_f64("rc").unwrap_or(d.rc),
        cje: p.get_f64("cje").unwrap_or(d.cje),
        cjc: p.get_f64("cjc").unwrap_or(d.cjc),
        tf: p.get_f64("tf").unwrap_or(d.tf),
        tr: p.get_f64("tr").unwrap_or(d.tr),
        // A library entry may carry the full SGP set, so a part sourced from a
        // vendor card keeps its beta roll-off and its low-current droop. A
        // knee written as zero means "no roll-off" (SPICE's convention), not a
        // knee at zero amps, which would switch the device off entirely.
        ikf: knee(p.get_f64("ikf"), d.ikf),
        ikr: knee(p.get_f64("ikr"), d.ikr),
        ise: p.get_f64("ise").unwrap_or(d.ise),
        ne: p.get_f64("ne").unwrap_or(d.ne),
        isc: p.get_f64("isc").unwrap_or(d.isc),
        nc: p.get_f64("nc").unwrap_or(d.nc),
        xti: p.get_f64("xti").unwrap_or(d.xti),
        eg: p.get_f64("eg").unwrap_or(d.eg),
    }
}

/// A non-positive high-injection knee disables the roll-off (SPICE convention).
fn knee(v: Option<f64>, default: f64) -> f64 {
    match v {
        Some(x) if x > 0.0 => x,
        Some(_) => f64::INFINITY,
        None => default,
    }
}

fn bind_mosfet(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
) -> (BindOutcome, Option<String>) {
    let d_node = roles.get("drain").or_else(|| pick(roles, &["d"])).copied();
    let g = roles.get("gate").or_else(|| pick(roles, &["g"])).copied();
    let s = roles.get("source").or_else(|| pick(roles, &["s"])).copied();
    let (Some(d_node), Some(g), Some(s)) = (d_node, g, s) else {
        return open_warning(comp, "MOSFET pins not all connected");
    };
    let polarity = if model.kind == ComponentKind::Pmos {
        Polarity::P
    } else {
        Polarity::N
    };
    let def = MosfetModel::default();
    let p = &model.params;
    // The db states vto in SPICE device convention (negative for enhancement
    // PMOS, e.g. AO3401A vto=-1.1); the solver stores it polarity-folded
    // (positive = enhancement either way). Fold like the SPICE loader does.
    let fold = polarity.sign();
    let m = MosfetModel {
        level: MosLevel::Level1,
        polarity,
        vto: p.get_f64("vto").map(|v| fold * v).unwrap_or(def.vto),
        kp: p.get_f64("kp").unwrap_or(def.kp),
        lambda: p.get_f64("lambda").unwrap_or(def.lambda),
        gamma: p.get_f64("gamma").unwrap_or(def.gamma),
        phi: p.get_f64("phi").unwrap_or(def.phi),
        w_over_l: p.get_f64("w_over_l").unwrap_or(def.w_over_l),
        n_sub: p.get_f64("n_sub").unwrap_or(def.n_sub),
        // Gate charge (dev-plan 04 §3.3): the db carries TOTAL capacitances
        // (`cgs`/`cgd` in farads, datasheet-style), which map onto the model's
        // total overlap fields directly. Absent fields leave the pre-§3.3
        // no-gate-charge stamp bit-identically.
        cgs_ov: p.get_f64("cgs").unwrap_or(def.cgs_ov),
        cgd_ov: p.get_f64("cgd").unwrap_or(def.cgd_ov),
        c_ox: def.c_ox,
        // Body diode: only when the db entry states it (`is`/`cbd`/`cbs`).
        body_is: p.get_f64("is").unwrap_or(def.body_is),
        cbd: p.get_f64("cbd").unwrap_or(def.cbd),
        cbs: p.get_f64("cbs").unwrap_or(def.cbs),
        pb: p.get_f64("pb").unwrap_or(def.pb),
        mj: p.get_f64("mj").unwrap_or(def.mj),
        // Drain/source ohmic resistance ("split of datasheet Rds(on)"): the db
        // documents these on every power-FET entry (e.g. IPA045N10N3G carries
        // rd = rs = 1.75 mohm summing to a 3.5 mohm Rds(on)). Absent keys leave
        // rd = rs = 0, bit-identical to a model without them.
        rd: p.get_f64("rd").unwrap_or(def.rd),
        rs: p.get_f64("rs").unwrap_or(def.rs),
    };
    circuit.add(Device::Mosfet {
        name: comp.reference.clone(),
        d: d_node,
        g,
        s,
        b: None,
        model: m,
    });
    (
        BindOutcome::Analog {
            device: "mosfet".to_string(),
        },
        None,
    )
}

fn bind_vreg(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
    _has_vreg: bool,
) -> (BindOutcome, Option<String>) {
    // A vreg whose model carries a behavioural converter is realised by the
    // behavioural layer, which owns the output net and reflects real input
    // current. Stamping the ideal source here too would put two stiff sources
    // on one net, and the ideal one wins: input current reads zero and the
    // converter model does nothing. The part still counts as bound here, and
    // the supply pass suppresses the ideal auto-rail on the converter's
    // output net for the same reason.
    if let Some(conv) = model.behavioral.converter.as_ref() {
        let Some(out) = roles.get(&conv.out_pin).copied() else {
            return open_warning(comp, "vreg output not connected");
        };
        if out.is_ground() {
            return open_warning(comp, "vreg output tied to ground");
        }
        if roles.get(&conv.in_pin).is_none_or(|n| n.is_ground()) {
            return open_warning(comp, "vreg input not connected");
        }
        return (
            BindOutcome::Behavioral {
                device: format!("vreg {:.1}V behavioral converter", conv.vout_setpoint),
            },
            None,
        );
    }
    let out = roles.get("out").copied();
    // A missing `vout` must not silently fabricate a 5 V rail on a board whose
    // regulator is actually 3.3 V (or anything else). Distinguish a present vout
    // from the assumed default and SAY SO, matching the bind_analog_switch
    // DEFAULT_VCC discipline below.
    let (vout, vout_warning) = match model.params.get_f64("vout") {
        Some(v) => (v, None),
        None => (
            DEFAULT_VCC,
            Some(format!(
                "{} ({}): vreg model has no `vout` param; regulating its output net to an \
                 assumed {DEFAULT_VCC:.1} V; verify the regulator's actual output voltage",
                comp.reference, comp.value,
            )),
        ),
    };
    let Some(out) = out else {
        return open_warning(comp, "vreg output not connected");
    };
    if out.is_ground() {
        return open_warning(comp, "vreg output tied to ground");
    }
    // Behavioral ideal source: regulate the output net to vout. A real LDO
    // needs its input above vout+dropout, but for liveness an ideal source is
    // the right first-order model.
    circuit.add(Device::Vsource {
        name: format!("Vreg_{}", comp.reference),
        p: out,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(vout),
    });
    (
        BindOutcome::Behavioral {
            device: format!("vreg {vout:.1}V source"),
        },
        vout_warning,
    )
}

fn bind_opamp(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
) -> (BindOutcome, Option<String>) {
    let reference = pick(roles, &["ref", "reference", "vref"]).copied();
    let gain = model.params.get_f64("gain").unwrap_or(1e5);
    let pole_hz = model.params.get_f64("pole_hz");
    // Datasheet slew rate in V/µs (the unit datasheets quote it in).
    let slew = model.params.get_f64("slew");
    let rail_lo = model.params.get_f64("rail_lo").unwrap_or(0.0);
    let rail_hi = model.params.get_f64("rail_hi").unwrap_or(5.0);
    let warning = model
        .params
        .get_str("warning")
        .map(|w| format!("{} ({}): {w}", comp.reference, comp.value));

    // Multi-channel packages (LM358 dual, INA2181 dual, LM324 quad) carry
    // per-channel roles out_a/in_plus_a/in_minus_a, ..._b/_c/_d. Stamp one
    // OpAmp per complete channel; a channel-A-only lookup would leave channel
    // B/C/D outputs silently floating. Per-unit names use the `_q<N>` key the
    // CI thermal aggregation matches (as bind_bjt does for paired BJTs).
    let mut stamped = 0;
    for (unit, sfx) in ["_a", "_b", "_c", "_d"].iter().enumerate() {
        let (Some(out), Some(inp), Some(inn)) = (
            roles.get(&format!("out{sfx}")).copied(),
            roles.get(&format!("in_plus{sfx}")).copied(),
            roles.get(&format!("in_minus{sfx}")).copied(),
        ) else {
            continue; // channel not (fully) wired on this board
        };
        circuit.add(Device::OpAmp {
            name: format!("{}_q{}", comp.reference, unit + 1),
            out,
            inp,
            inn,
            reference,
            gain,
            pole_hz,
            slew,
            rail_lo,
            rail_hi,
        });
        stamped += 1;
    }
    if stamped > 0 {
        return (
            BindOutcome::Behavioral {
                device: format!("opamp x{stamped}"),
            },
            warning,
        );
    }

    // Single-channel parts (INA181/186, generic role names): one device under
    // the bare reference, exactly as before.
    let out = pick(roles, &["out", "out_1"]).copied();
    let inp = pick(roles, &["in_plus", "inp", "in+"]).copied();
    let inn = pick(roles, &["in_minus", "inn", "in-"]).copied();
    let (Some(out), Some(inp), Some(inn)) = (out, inp, inn) else {
        return open_warning(comp, "opamp pins not all connected");
    };
    circuit.add(Device::OpAmp {
        name: comp.reference.clone(),
        out,
        inp,
        inn,
        reference,
        gain,
        pole_hz,
        slew,
        rail_lo,
        rail_hi,
    });
    (
        BindOutcome::Behavioral {
            device: "opamp".to_string(),
        },
        warning,
    )
}

fn bind_comparator(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
) -> (BindOutcome, Option<String>) {
    let out_lo = model.params.get_f64("out_lo").unwrap_or(0.0);
    let out_hi = model.params.get_f64("out_hi").unwrap_or(5.0);
    let hyst = model.params.get_f64("hysteresis").unwrap_or(0.005);

    // Static supply draw (`supply_static_ua`, default 0 = nothing stamped):
    // the part's quiescent ICC pulled from its supply pin net, so a metering
    // shunt in series with that rail reads the comparator. A fixed Isource is
    // enough here, the behavioral Comparator device has no VCC leg at all,
    // and its dynamic (switching) draw is negligible next to ICC for the
    // LMV7219 class. Same param name as the digital layer's static term.
    let static_ua = model.params.get_f64("supply_static_ua").unwrap_or(0.0);
    if static_ua > 0.0 {
        let vcc = roles
            .get("vcc")
            .or_else(|| roles.get("vs"))
            .or_else(|| roles.get("vdd"))
            .copied();
        if let Some(vcc) = vcc.filter(|n| !n.is_ground()) {
            let gnd = roles
                .get("vss")
                .or_else(|| roles.get("gnd"))
                .copied()
                .unwrap_or(NodeId::GROUND);
            circuit.add(Device::Isource {
                name: format!("Iq_{}", comp.reference),
                p: vcc,
                n: gnd,
                kind: SourceKind::Dc(static_ua * 1e-6),
            });
        }
    }

    // Multi-channel packages (LM393 dual, LM339 quad): one Comparator per
    // complete out_X/in_plus_X/in_minus_X channel, keyed `_q<N>`, same shape
    // and rationale as bind_opamp above.
    let mut stamped = 0;
    for (unit, sfx) in ["_a", "_b", "_c", "_d"].iter().enumerate() {
        let (Some(out), Some(inp), Some(inn)) = (
            roles.get(&format!("out{sfx}")).copied(),
            roles.get(&format!("in_plus{sfx}")).copied(),
            roles.get(&format!("in_minus{sfx}")).copied(),
        ) else {
            continue;
        };
        circuit.add(Device::Comparator {
            name: format!("{}_q{}", comp.reference, unit + 1),
            out,
            inp,
            inn,
            out_lo,
            out_hi,
            hysteresis: hyst,
        });
        stamped += 1;
    }
    if stamped > 0 {
        return (
            BindOutcome::Behavioral {
                device: format!("comparator x{stamped}"),
            },
            None,
        );
    }

    let out = pick(roles, &["out", "out_1", "q"]).copied();
    let inp = pick(roles, &["in_plus", "inp", "in+"]).copied();
    let inn = pick(roles, &["in_minus", "inn", "in-"]).copied();
    let (Some(out), Some(inp), Some(inn)) = (out, inp, inn) else {
        return open_warning(comp, "comparator pins not all connected");
    };
    circuit.add(Device::Comparator {
        name: comp.reference.clone(),
        out,
        inp,
        inn,
        out_lo,
        out_hi,
        hysteresis: hyst,
    });
    (
        BindOutcome::Behavioral {
            device: "comparator".to_string(),
        },
        None,
    )
}

fn bind_analog_switch(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
    power_nets: &HashMap<String, f64>,
) -> (BindOutcome, Option<String>) {
    // True SPDT when both throws are wired (com + s0 + s1 + ctrl): two
    // complementary VSwitch legs. select low -> com<->s0, select high ->
    // com<->s1 (the SN74LVC1G3157 convention). The s0 leg senses the
    // INVERTED control by measuring (vcc - select).
    let com = pick(roles, &["com", "a"]).copied();
    let s0 = pick(roles, &["s0", "b1"]).copied();
    let s1 = pick(roles, &["s1", "b2"]).copied();
    let sel = pick(roles, &["ctrl", "s", "in"]).copied();
    let vss = pick(roles, &["vss", "gnd"])
        .copied()
        .unwrap_or(NodeId::GROUND);
    if let (Some(com), Some(s0), Some(s1), Some(sel), Some(vcc)) =
        (com, s0, s1, sel, pick(roles, &["vcc"]).copied())
    {
        let ron = model.params.get_f64("ron").unwrap_or(50.0);
        let roff = model.params.get_f64("roff").unwrap_or(1e9);
        let vth = model.params.get_f64("vth").unwrap_or(1.5);
        // The s0 leg senses (vcc - select), so its thresholds must be
        // referenced to the ACTUAL rail on the vcc net. A hardcoded 5 V rail
        // put von at 3.75 V on a 3.3 V board, unreachable (vcc - select
        // never exceeds 3.3 V), leaving com<->s0 permanently open.
        // The s0 leg's thresholds are referenced to the ACTUAL rail voltage, so
        // an unresolved VCC net can't be a silent guess: a non-canonically
        // named rail (e.g. "VDD_MUX" on a 3.3 V board) that falls back to 5 V
        // reintroduces the very "com<->s0 permanently open" bug the threshold
        // math above fixes. Fall back to DEFAULT_VCC but SAY SO, matching the
        // passive-array convention warning.
        let (vcc_v, vcc_warning) = match power_nets.get(circuit.node_name(vcc)).copied() {
            Some(v) => (v, None),
            None => (
                DEFAULT_VCC,
                Some(format!(
                    "{} ({}): analog-switch VCC net '{}' has no resolved rail voltage; \
                     modeling its SPDT thresholds against an assumed {DEFAULT_VCC} V rail; \
                     on a lower-voltage board the common<->s0 throw may read as open, \
                     so verify the switch's actual supply",
                    comp.reference,
                    comp.value,
                    circuit.node_name(vcc),
                )),
            ),
        };
        circuit.add(Device::VSwitch {
            name: format!("{}_s1", comp.reference),
            a: com,
            b: s1,
            ctrl_p: sel,
            ctrl_n: vss,
            von: vth + 0.25,
            voff: vth - 0.25,
            ron,
            roff,
        });
        circuit.add(Device::VSwitch {
            name: format!("{}_s0", comp.reference),
            a: com,
            b: s0,
            // Conducts when the select is LOW: sense vcc - select.
            ctrl_p: vcc,
            ctrl_n: sel,
            von: vcc_v - vth + 0.25,
            voff: vcc_v - vth - 0.25,
            ron,
            roff,
        });
        return (
            BindOutcome::Analog {
                device: "spdt x2".to_string(),
            },
            vcc_warning,
        );
    }

    // Multi-gate bilateral packages (CD74HC4066 quad): numbered gate roles
    // in_out_<n>a / in_out_<n>b switched by ctrl_<n>, each gate electrically
    // independent. Stamp one VSwitch per complete gate; a single-SPST
    // fall-through would bind gate 1 only and silently drop gates 2..4. Per-unit
    // names use the `_s<n>` key the CI thermal aggregation matches (the same
    // family as the SPDT's `_s0`/`_s1` legs above).
    {
        let ron = model.params.get_f64("ron").unwrap_or(50.0);
        let roff = model.params.get_f64("roff").unwrap_or(1e9);
        let vth = model.params.get_f64("vth").unwrap_or(1.5);
        let mut stamped = 0;
        let mut partial: Vec<usize> = Vec::new();
        for n in 1..=4 {
            let ra = roles.get(&format!("in_out_{n}a")).copied();
            let rb = roles.get(&format!("in_out_{n}b")).copied();
            let rc = roles.get(&format!("ctrl_{n}")).copied();
            let (Some(a), Some(b), Some(ctrl)) = (ra, rb, rc) else {
                // A gate with SOME but not all three terminals wired is a partial
                // miswire; a fully-unused gate (none wired) is a normal spare and
                // stays quiet. Warn only on the former (the passive-array
                // discipline), instead of dropping it silently.
                if ra.is_some() || rb.is_some() || rc.is_some() {
                    partial.push(n);
                }
                continue;
            };
            circuit.add(Device::VSwitch {
                name: format!("{}_s{n}", comp.reference),
                a,
                b,
                ctrl_p: ctrl,
                ctrl_n: vss,
                von: vth + 0.1,
                voff: vth - 0.1,
                ron,
                roff,
            });
            stamped += 1;
        }
        if stamped > 0 {
            let warning = if partial.is_empty() {
                None
            } else {
                Some(format!(
                    "{}: analog-switch gate(s) {} missing a connection, left open",
                    comp.reference,
                    partial
                        .iter()
                        .map(|n| n.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            };
            return (
                BindOutcome::Behavioral {
                    device: format!("vswitch x{stamped}"),
                },
                warning,
            );
        }
    }

    // SPST fallback: switch COM<->S0 (or in_out_a<->in_out_b) controlled by
    // ctrl vs vss. Model the on-leg only; the other throw is left open.
    let a = pick(roles, &["com", "in_out_a", "in_out_1a", "s0"]).copied();
    let b = pick(roles, &["s0", "in_out_b", "in_out_1b", "com"]).copied();
    // Resolve a and b to distinct nodes.
    let (a, b) = match (a, b) {
        (Some(a), Some(b)) if a != b => (a, b),
        _ => {
            // Fall back: first two non-power roles. The control role is EXCLUDED:
            // it is the switch's gate, not a signal terminal. Wiring it as a
            // throw would stamp a VSwitch whose `b` equals its own `ctrl_p`,
            // fabricating a ~ron path that shorts a signal net to the control
            // line (and injects/loads the control voltage) when the gate goes
            // high, a conductive path that does not exist on the real board.
            let mut nodes: Vec<NodeId> = roles
                .iter()
                .filter(|(r, _)| !is_power_role(r) && !is_ctrl_role(r))
                .map(|(_, n)| *n)
                .collect();
            nodes.sort_by_key(|n| n.0);
            nodes.dedup();
            if nodes.len() < 2 {
                return open_warning(comp, "analog switch path not connected");
            }
            (nodes[0], nodes[1])
        }
    };
    let ctrl = pick(roles, &["ctrl", "ctrl_1", "in"]).copied();
    let ctrl_n = pick(roles, &["vss", "gnd"])
        .copied()
        .unwrap_or(NodeId::GROUND);
    let Some(ctrl) = ctrl else {
        return open_warning(comp, "analog switch control not connected");
    };
    let ron = model.params.get_f64("ron").unwrap_or(50.0);
    let roff = model.params.get_f64("roff").unwrap_or(1e9);
    let vth = model.params.get_f64("vth").unwrap_or(1.5);
    // The `s0` / NC throw conducts when the control is LOW (role_from_pinfunction
    // maps nc->s0 with exactly this contract, and the true-SPDT branch honours it).
    // The default control-HIGH polarity below would invert it, modelling the
    // contact open exactly when the real one is closed. When `b` is the s0 net,
    // sense the inverted control (vss - ctrl) so com<->s0 closes on control LOW.
    let b_is_control_low = pick(roles, &["s0"]).copied() == Some(b);
    let (cp, cn, von, voff) = if b_is_control_low {
        (ctrl_n, ctrl, -vth + 0.1, -vth - 0.1)
    } else {
        (ctrl, ctrl_n, vth + 0.1, vth - 0.1)
    };
    circuit.add(Device::VSwitch {
        name: comp.reference.clone(),
        a,
        b,
        ctrl_p: cp,
        ctrl_n: cn,
        von,
        voff,
        ron,
        roff,
    });
    (
        BindOutcome::Behavioral {
            device: "vswitch".to_string(),
        },
        None,
    )
}

/// Bind an MCP4728-class quad I2C DAC: stamp a low-impedance Thevenin
/// [`PinDriver`] on each connected VOUT channel net (ROUT ~1 Ω per the
/// datasheet) and push a [`DacBinding`] the scheduler turns into an I2C slave.
/// The address is assigned later by reference order; we leave a placeholder.
fn bind_mcp4728_dac(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
    dacs: &mut Vec<DacBinding>,
) -> (BindOutcome, Option<String>) {
    // Output series resistance: datasheet ROUT ~1 Ω (normal mode). A small,
    // non-zero value keeps the MNA well-conditioned while presenting a stiff
    // source onto the synapse set-point nets.
    let rout = model.params.get_f64("rout").unwrap_or(1.0);
    let vref = model.params.get_f64("vref_int").unwrap_or(2.048);
    let gain = model.params.get_f64("gain").unwrap_or(2.0).round() as u8;

    let channel_roles = ["vout_a", "vout_b", "vout_c", "vout_d"];
    let mut vout_drivers: [Option<PinDriver>; 4] = [None, None, None, None];
    let mut stamped = 0;
    for (ch, role) in channel_roles.iter().enumerate() {
        let Some(&net) = roles.get(*role) else {
            continue;
        };
        if net.is_ground() {
            continue;
        }
        let net_name = circuit.node_name(net).to_string();
        let drv = PinDriver::stamp(
            circuit,
            net,
            &net_name,
            &format!("{}_{role}", comp.reference),
            rout,
        );
        vout_drivers[ch] = Some(drv);
        stamped += 1;
    }

    if stamped == 0 {
        return (
            BindOutcome::Unresolved {
                reason: "MCP4728 has no connected VOUT channel".to_string(),
            },
            Some(format!(
                "{} ({}): no VOUT channel connected, DAC left idle",
                comp.reference, comp.value
            )),
        );
    }

    dacs.push(DacBinding {
        reference: comp.reference.clone(),
        address: 0x60, // placeholder; reassigned by reference order post-bind.
        vref,
        gain,
        vout_drivers,
    });
    (
        BindOutcome::Behavioral {
            device: format!("mcp4728 quad DAC ({stamped} VOUT)"),
        },
        None,
    )
}

/// Bind a digital part's [models.logic] spec. `Err` carries the logic-compile
/// failure: the part is NOT modeled (its stamped legs are tri-stated so the
/// output nets genuinely float) and the caller MUST record it as unresolved.
/// Swallowing the error while still reporting the part as bound was the
/// NEP-board study's defect 3: a part that cannot possibly work showed as a
/// healthy `Digital` row in the bind coverage report.
fn bind_digital(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
    digital: &mut Vec<DigitalComponent>,
) -> Result<(), String> {
    // A quad/dual NOR gate (74HC02) wired as cross-coupled SR spike latches: if
    // any gate pair forms a NOR latch (one gate's output net == the other gate's
    // input net and vice-versa), bind one NorLatch behavioral component per latch
    // and return. Falls through to the generic buffer path if no latch found.
    if model.id.to_ascii_lowercase().contains("74hc02")
        && bind_nor_latches(comp, model, circuit, roles, digital)
    {
        return Ok(());
    }

    // Stamp a Thevenin driver on each connected output role, honouring the
    // model's declared output resistance (drive strength), `DEFAULT_RO` is the
    // fallback inside `from_params`, not an override of a specified `ro`.
    let ro = crate::digital::LogicLevels::from_params(model).ro;
    let mut drivers = HashMap::new();
    for role in output_roles(model) {
        if let Some(&net) = roles.get(&role) {
            if net.is_ground() {
                continue;
            }
            let net_name = circuit.node_name(net).to_string();
            let drv = PinDriver::stamp(
                circuit,
                net,
                &net_name,
                &format!("{}_{role}", comp.reference),
                ro,
            );
            drivers.insert(role, drv);
        }
    }
    // Retain clones of the stamped legs so a FAILED construction can tri-state
    // them: the Vsource+Resistor devices are already in the circuit, and dropping
    // the driver handles does NOT remove them. Without this, an output leg starts
    // enabled at 0 V through `ron` (~50 Ω) and, with no DigitalComponent to manage
    // it, actively holds the output net LOW for the whole run; the opposite of
    // the "nets will float" contract, and it can hold a downstream SRCLR/OE
    // asserted. Mirror bind_mcu, which disables its GPIO legs until firmware
    // enables them.
    let leg_handles: Vec<PinDriver> = drivers.values().cloned().collect();
    match DigitalComponent::new(comp.reference.clone(), model, roles.clone(), drivers) {
        Ok(mut d) => {
            // VCC supply draw (`supply_static_ua` / `supply_cpd_pf` params,
            // both defaulting to 0 = no leg stamped): a controllable Isource
            // from the part's VCC pin net to its GND pin net, refreshed once
            // per chunk by the scheduler. This is what makes a metering shunt
            // in series with the part's rail read the part at all, the
            // Thevenin output drivers are referenced to ground and never move
            // charge through VCC.
            let static_ua = model.params.get_f64("supply_static_ua").unwrap_or(0.0);
            let cpd_pf = model.params.get_f64("supply_cpd_pf").unwrap_or(0.0);
            if static_ua > 0.0 || cpd_pf > 0.0 {
                let vcc = roles.get("vcc").or_else(|| roles.get("vdd")).copied();
                match vcc {
                    Some(vcc) if !vcc.is_ground() => {
                        let gnd = roles
                            .get("gnd")
                            .or_else(|| roles.get("vss"))
                            .copied()
                            .unwrap_or(NodeId::GROUND);
                        d.supply = Some(SupplyDraw::stamp(
                            circuit,
                            vcc,
                            gnd,
                            &comp.reference,
                            static_ua,
                            cpd_pf,
                        ));
                    }
                    _ => eprintln!(
                        "warning: {}: model '{}' declares supply draw but its VCC pin is \
                         unwired or grounded; no supply current will be drawn",
                        comp.reference, model.id
                    ),
                }
            }
            digital.push(d);
            Ok(())
        }
        // Never silently downgrade a broken spec to a passthrough: the part is
        // left unmodeled LOUDLY, with its output nets genuinely floating (the
        // stamped legs tri-stated to `roff`), matching the message below. The
        // error is RETURNED, not just printed, so the bind report records the
        // part as unresolved instead of showing a bound-looking row whose
        // nets never drive.
        Err(e) => {
            for mut drv in leg_handles {
                drv.set_enabled(circuit, false);
            }
            eprintln!(
                "ERROR: {}: invalid [models.logic] for model '{}': {e}; the part is left \
                 unmodeled and its output nets will float; fix the spec (`hauksbee models \
                 lint`) or override it with --models-dir",
                comp.reference, model.id
            );
            Err(e.to_string())
        }
    }
}

/// Detect and bind cross-coupled NOR SR latches on a 74HC02 (the Tarski spike
/// recorder). Each gate `g<n>` has output role `g<n>y` and input roles `g<n>a`,
/// `g<n>b`. A latch is a pair of gates (gQ, gQb) where `gQ.y == gQb.<one input>`
/// and `gQb.y == gQ.<one input>` (the cross-couple). For the latch:
///   - `reset` = the gate whose NON-cross-couple input net name contains "RESET"
///     (RESET_SR), that gate's output is Qb (internal);
///   - the OTHER gate is the Q gate: its non-cross input is `set` (SPIKE<n>),
///     its output net is `q` (the observable L<n>, wired to the 165 inputs).
/// Stamps a Thevenin driver on the `q` net and pushes one [`DigitalComponent`]
/// (the builtin NOR-latch spec) per latch. Returns true if ≥1 latch was bound.
fn bind_nor_latches(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
    digital: &mut Vec<DigitalComponent>,
) -> bool {
    use crate::digital::{DigitalComponent, LogicLevels};
    // Collect the gates present (output + its two inputs).
    struct Gate {
        idx: usize,
        y: NodeId,
        ins: Vec<NodeId>,
    }
    let mut gates: Vec<Gate> = Vec::new();
    for n in 1..=4usize {
        let y = roles.get(&format!("g{n}y")).copied();
        let a = roles.get(&format!("g{n}a")).copied();
        let b = roles.get(&format!("g{n}b")).copied();
        if let Some(y) = y {
            let ins: Vec<NodeId> = [a, b].into_iter().flatten().collect();
            gates.push(Gate { idx: n, y, ins });
        }
    }
    let levels = LogicLevels::from_params(model);
    let mut bound = 0usize;
    let mut used: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for i in 0..gates.len() {
        if used.contains(&i) {
            continue;
        }
        for j in 0..gates.len() {
            if i == j || used.contains(&j) {
                continue;
            }
            // Cross-couple: gate i's output feeds gate j's input AND vice-versa.
            let cross = gates[j].ins.contains(&gates[i].y) && gates[i].ins.contains(&gates[j].y);
            if !cross {
                continue;
            }
            // The non-cross-couple input of each gate (the SET / RESET line).
            let other_in = |g: &Gate, partner_y: NodeId| -> Option<NodeId> {
                g.ins.iter().copied().find(|&n| n != partner_y)
            };
            let in_i = other_in(&gates[i], gates[j].y);
            let in_j = other_in(&gates[j], gates[i].y);
            let net_name = |n: Option<NodeId>| n.map(|n| circuit.node_name(n).to_string());
            let i_is_reset = net_name(in_i)
                .map(|s| s.to_ascii_uppercase().contains("RESET"))
                .unwrap_or(false);
            let j_is_reset = net_name(in_j)
                .map(|s| s.to_ascii_uppercase().contains("RESET"))
                .unwrap_or(false);
            // The reset gate's output is Qb; the OTHER gate is Q.
            let (q_gate, set_in) = if j_is_reset && !i_is_reset {
                (i, in_i)
            } else if i_is_reset && !j_is_reset {
                (j, in_j)
            } else {
                // No RESET-named line found, not a recognisable SR latch here.
                continue;
            };
            let reset_in = if q_gate == i { in_j } else { in_i };
            let (Some(set_n), Some(reset_n)) = (set_in, reset_in) else {
                continue;
            };
            let q_net = gates[q_gate].y;
            if q_net.is_ground() {
                continue;
            }
            // Stamp the Q output driver and build the latch component.
            let q_name = circuit.node_name(q_net).to_string();
            let drv = PinDriver::stamp(
                circuit,
                q_net,
                &q_name,
                &format!("{}_latch{}_q", comp.reference, gates[q_gate].idx),
                levels.ro,
            );
            let mut lroles: HashMap<String, NodeId> = HashMap::new();
            lroles.insert("set".to_string(), set_n);
            lroles.insert("reset".to_string(), reset_n);
            lroles.insert("q".to_string(), q_net);
            let mut ldrivers = HashMap::new();
            ldrivers.insert("q".to_string(), drv);
            digital.push(DigitalComponent::new_nor_latch(
                format!("{}_L{}", comp.reference, gates[q_gate].idx),
                levels,
                lroles,
                ldrivers,
            ));
            used.insert(i);
            used.insert(j);
            bound += 1;
            break;
        }
    }
    bound > 0
}

#[allow(clippy::too_many_arguments)]
fn bind_mcu(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
    pad_nodes: &dyn Fn(&str) -> Option<NodeId>,
    _power_nets: &HashMap<String, f64>,
    mcus: &mut Vec<McuBinding>,
) -> Option<String> {
    let backend = mcu_backend_string(comp, model);
    // Value-aware, not presence-aware: an explicit `module = false` (a bare chip
    // in an SDK/extending-guide model) must NOT activate the Arduino-header pad
    // mapping. Only `module = true` does.
    let module = model.params.get_bool("module").unwrap_or(false);
    let derived_when_empty = if model.pins.is_empty() {
        Some(derive_mcu_pin_roles(comp))
    } else {
        None
    };
    // Merge the two role sources PER PAD instead of treating a non-empty model
    // map as exhaustive. A DB pin map is curated for one package's numbering
    // (often a module, e.g. the ESP32-S3 entry's WROOM-1 strap pads); applied
    // to a different footprint it covers a handful of pads and the old
    // model-only rule then discarded every OTHER pad's own pinfunction. On the
    // Watchy v3's bare QFN-56 that left ALL display pins (RES/DC/CS, SCK/MOSI,
    // SDA/SCL, all named "GPIOnn/..." right in the board file) with no GPIO
    // driver at all, so firmware could never drive them and the live sim
    // presented their static levels as measurements. Model roles still win on
    // pads they name (they carry curated semantic suffixes like "pc6_reset"
    // that the plain pinfunction derivation would weaken); the derivation only
    // fills pads the model map does not cover.
    let effective_pins: std::collections::BTreeMap<String, String> = {
        let mut merged = derived_when_empty
            .as_ref()
            .map(|d| d.roles.clone())
            .unwrap_or_else(|| derive_mcu_pin_roles(comp).roles);
        for (pad, role) in &model.pins {
            merged.insert(pad.clone(), role.clone());
        }
        merged
    };
    let effective_pins = &effective_pins;

    let mut pad_roles = HashMap::new();
    let mut role_nets = HashMap::new();
    for pin in &comp.pins {
        if let Some(role) = effective_pins.get(&pin.number) {
            pad_roles.insert(pin.number.clone(), role.clone());
            if let Some(node) = node_of(pin.net) {
                role_nets.insert(role.clone(), node);
            }
        }
    }

    // Map roles to (port,bit) GPIO and ADC channels, then stamp drivers/probes.
    // Dynamic promotion: a dual-purpose ADC/GPIO pin (Nano a0..a5 = PC0..PC5,
    // or bare pc0_adc0..)
    // binds BOTH ways structurally. It keeps its ADC channel mapping AND gets
    // the same tri-stated Thevenin GPIO driver an ordinary digital pin gets.
    // Every driver starts high-impedance (a 1e9 Ω leg from a 0 V source, i.e.
    // electrically inert), so an undriven pin still reads as a pure ADC input
    // with zero electrical effect; the scheduler enables the driver on the
    // pin's first firmware drive (its first `on_pin_change` edge, or a
    // `pins_configured_output` direction report), promoting the pin to a GPIO
    // output. No bind-time usage heuristic is needed: the firmware's actual
    // pinMode decides. This is what un-floats OE'_S / SRCLR'_S (Tarski A2/A3):
    // firmware driving an A-pin digitally now has a driver to enable, where a
    // floating-low SRCLR'_S would have held the whole 74HC595 chain cleared.
    let mut gpio_drivers = HashMap::new();
    let mut adc_nets = HashMap::new();
    let mut adc_pin = HashMap::new();
    for (role, &node) in &role_nets {
        if node.is_ground() {
            continue;
        }
        // Keep the ADC channel mapping for every analog-capable pin, so the
        // scheduler can inject the solved net voltage while the pin is (or
        // stays) an analog input. A6/A7 on the ATmega328P are ADC-only (no
        // port C pin): `apin_gpio_of_role` returns None for them below, so
        // they bind as plain ADC probes with no driver.
        if let Some(ch) = adc_of_role(role, module) {
            adc_nets.insert(ch, node);
        }
        // Stamp a GPIO driver for every pin with a digital port pin, INCLUDING
        // analog-capable ones (that dual bind is the point of the promotion
        // design). `gpio_of_role` covers ordinary digital roles;
        // `apin_gpio_of_role` recovers the port pin behind an analog role.
        let port_bit = gpio_of_role(role, module).or_else(|| apin_gpio_of_role(role, module));
        if let Some((port, bit)) = port_bit {
            // Record this ADC channel's OWN port pin (if it is an analog pin that
            // also has a digital port pin) so the scheduler can tell a genuine
            // self-promotion from a mere same-net neighbour's driver.
            if let Some(ch) = adc_of_role(role, module) {
                adc_pin.insert(ch, (port, bit));
            }
            let net_name = circuit.node_name(node).to_string();
            let mut drv = PinDriver::stamp(
                circuit,
                node,
                &net_name,
                &format!("{}_{port}{bit}", comp.reference),
                DEFAULT_RO,
            );
            // Start high-impedance: the firmware enables the driver by toggling
            // the pin (DDR + PORT writes surface as an output edge). Inertness
            // while disabled is the load-bearing property that lets an ADC pin
            // carry a driver without perturbing the voltage the firmware reads.
            drv.set_enabled(circuit, false);
            gpio_drivers.insert((port, bit), drv);
        }
    }
    let _ = pad_nodes;

    mcus.push(McuBinding {
        reference: comp.reference.clone(),
        backend,
        // The raw requested part string, captured before family routing collapsed
        // e.g. STM32F411RET6 -> the stm32f4 backend. Empty when the board gives no
        // value (we still bind, but cannot name what was asked for).
        requested_part: comp.value.trim().to_string(),
        pad_roles,
        role_nets,
        gpio_drivers,
        adc_nets,
        adc_pin,
        module,
        max_supply_v: model.ratings.max_voltage_v,
    });
    log_mcu_auto_decision(comp, model, derived_when_empty.as_ref())
}

fn log_mcu_auto_decision(
    comp: &Component,
    model: &ModelEntry,
    derived_when_empty: Option<&DerivedMcuPins>,
) -> Option<String> {
    let backend = mcu_backend_string(comp, model);
    let auto_router = model.params.get_str("auto_bind") == Some("family_router");
    if auto_router {
        let family = model.params.get_str("auto_bind_family").unwrap_or("MCU");
        let named = model
            .params
            .0
            .get("auto_bind_pin_names")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0) as usize;
        let derived = model
            .params
            .0
            .get("auto_bind_derived_pins")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0) as usize;
        let summary = model
            .params
            .get_str("auto_bind_pin_summary")
            .unwrap_or("no GPIO roles");
        let pin_msg = if named == 0 {
            "GPIO map cannot be derived: no schematic pin names; leaving GPIO unmapped".to_string()
        } else if derived == 0 {
            format!(
                "GPIO map cannot be derived from {named} schematic pin names; leaving GPIO unmapped"
            )
        } else {
            format!("GPIO map derived from {derived} schematic pin names ({summary})")
        };
        let msg = format!(
            "[auto-bind] {} \"{}\" recognized {family} -> backend {backend} (no DB model); {pin_msg}. Override with a --models-dir entry.",
            comp.reference, comp.value
        );
        eprintln!("{msg}");
        return if derived == 0 { Some(msg) } else { None };
    }

    if let Some(derived) = derived_when_empty {
        if derived.roles.is_empty() {
            let pin_reason = if derived.named_pin_count == 0 {
                "no schematic pin names".to_string()
            } else {
                format!("{} schematic pin names", derived.named_pin_count)
            };
            let msg = format!(
                "[auto-bind] {} \"{}\" explicit model has no pins; GPIO map cannot be derived from {pin_reason}; leaving GPIO unmapped. Override with a --models-dir entry.",
                comp.reference, comp.value
            );
            eprintln!("{msg}");
            return Some(msg);
        }
        let summary = pin_role_summary(&derived.roles);
        let msg = format!(
            "[auto-bind] {} \"{}\" explicit model has no pins; GPIO map derived from {} schematic pin names ({}). Override with a --models-dir entry.",
            comp.reference,
            comp.value,
            derived.roles.len(),
            summary
        );
        eprintln!("{msg}");
    }
    None
}

// ── Role/pin helpers ─────────────────────────────────────────────────────────

fn pick<'a>(roles: &'a HashMap<String, NodeId>, names: &[&str]) -> Option<&'a NodeId> {
    names.iter().find_map(|n| roles.get(*n))
}

fn open_warning(comp: &Component, why: &str) -> (BindOutcome, Option<String>) {
    (
        BindOutcome::Unresolved {
            reason: why.to_string(),
        },
        Some(format!(
            "{} ({}): {why}, left open",
            comp.reference, comp.value
        )),
    )
}

fn is_power_role(role: &str) -> bool {
    let r = role.to_ascii_lowercase();
    r.contains("vcc") || r.contains("vdd") || r.contains("vss") || r == "gnd"
}

/// The analog-switch gate/select roles (mirrors the `ctrl` pick list in the SPST
/// fallback). Kept out of the throw-terminal candidates: the control net drives
/// the switch, it is never one of its signal terminals.
fn is_ctrl_role(role: &str) -> bool {
    let r = role.to_ascii_lowercase();
    // ctrl / in / s / sel are the single-gate control spellings; ctrl_1..ctrl_N are
    // the per-gate controls the multi-gate bilateral-switch branch drives (a quad
    // 4066 has ctrl_1..ctrl_4). Only ctrl_1 was excluded before, so ctrl_2/3/4 could
    // leak into the SPST fallback's throw candidates and get stamped as a switch
    // terminal, fabricating a ~ron short from a signal net onto a control net.
    r == "ctrl"
        || r == "in"
        || r == "s"
        || r == "sel"
        || r.strip_prefix("ctrl_")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn device_label(d: &Device) -> String {
    match d {
        Device::Resistor { ohms, .. } => format!("R {}", fmt_eng(*ohms, "Ω")),
        Device::Capacitor { farads, .. } => format!("C {}", fmt_eng(*farads, "F")),
        Device::Inductor { henries, .. } => format!("L {}", fmt_eng(*henries, "H")),
        other => other.name().to_string(),
    }
}

/// Format a physical quantity with an SI prefix scaled to its magnitude, so a
/// 390 pF cap reads "390 pF" rather than "0.000 µF". Picks the prefix that puts
/// the mantissa in [1, 1000) and prints a sensible number of significant digits.
/// `unit` is the bare unit symbol ("F", "H", "Ω").
pub(crate) fn fmt_eng(value: f64, unit: &str) -> String {
    if value == 0.0 || !value.is_finite() {
        return format!("0 {unit}");
    }
    let neg = value < 0.0;
    let v = value.abs();
    // Prefix table from pico to mega; covers caps (pF..mF), inductors (nH..H),
    // resistors (mΩ..MΩ). Each entry is (10^exponent, prefix).
    const PREFIXES: &[(f64, &str)] = &[
        (1e6, "M"),
        (1e3, "k"),
        (1e0, ""),
        (1e-3, "m"),
        (1e-6, "µ"),
        (1e-9, "n"),
        (1e-12, "p"),
    ];
    // Pick the largest prefix whose scale leaves a mantissa >= 1 (so 390 pF uses
    // "p", 1.5 kΩ uses "k"). Fall back to the smallest prefix for tiny values.
    let idx = PREFIXES
        .iter()
        .position(|(s, _)| v >= *s)
        .unwrap_or(PREFIXES.len() - 1);
    let (mut scale, mut prefix) = PREFIXES[idx];
    let mut mantissa = v / scale;
    // 3 significant figures: more decimals for small mantissas, fewer for large.
    let decimals = |m: f64| {
        if m >= 100.0 {
            0
        } else if m >= 10.0 {
            1
        } else {
            2
        }
    };
    // Decade carry: rounding the mantissa to its significant figures can push it
    // to 1000 (e.g. 999.6 -> "1000"), which renders "1000 kΩ", outside the
    // promised [1,1000) range and inconsistent with the sibling format_engineering
    // (tolerance.rs), which has this exact guard. Promote to the next-larger prefix
    // so it reads "1 MΩ" instead. Only when a larger prefix exists (idx > 0).
    let round_to = |m: f64, d: i32| {
        let p = 10f64.powi(d);
        (m * p).round() / p
    };
    if idx > 0 && round_to(mantissa, decimals(mantissa)) >= 1000.0 {
        (scale, prefix) = PREFIXES[idx - 1];
        mantissa = v / scale;
    }
    let s = format!("{mantissa:.*}", decimals(mantissa) as usize);
    // Trim trailing fractional zeros for a clean read ("4.70" -> "4.7"), but
    // only when a decimal point is present, so "390" is never stripped to "39".
    let s = if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    };
    let sign = if neg { "-" } else { "" };
    format!("{sign}{s} {prefix}{unit}")
}

/// Map an ATmega328P / Arduino-Nano role string to a `(port, bit)` GPIO id.
pub(crate) fn gpio_of_role(role: &str, module: bool) -> Option<(char, u8)> {
    let r = role.to_ascii_lowercase();
    if module {
        // Arduino Nano header: d0..d13 map to Arduino digital pins.
        // d0=PD0 .. d7=PD7, d8=PB0 .. d13=PB5.
        if let Some(rest) = r.strip_prefix('d') {
            let num: u8 = rest.split('_').next().unwrap_or("").parse().ok()?;
            return arduino_digital_to_port(num);
        }
        return None;
    }
    // Port-pin roles, two conventions sharing the `p` prefix:
    //
    //   - Lettered ports (AVR / STM32 / Cortex-M): 'p' <port letter A-G>
    //     <bit, 1-2 digits> [ '_' suffix ], e.g. "pb5_sck", "pc13", "pa9".
    //   - Numeric ports (nRF52 gpio0/gpio1, SiFive FE310 gpio0, ESP32 GPIO
    //     matrix): 'p' <port digit 0-1> <bit, 1-2 digits> [ '_' suffix ], e.g.
    //     "p013" = port '0' bit 13 (nRF52840-DK LED1), "p02" = port '0' bit 2
    //     (ESP32 GPIO2), "p119" = port '1' bit 19. This matches the renode
    //     PortMap letters '0'/'1' and the QEMU GpioBank letter '0'.
    // Flat GPIO-space roles ("gpio0", "gpio15_mtdo"): ESP32 / FE310 style.
    // Bits 0-31 live in numeric port '0', 32+ in port '1', matching the
    // two-bank numeric-port convention below. These names double as the
    // strap-table roles, which match pin roles by exact string.
    if let Some(rest) = r.strip_prefix("gpio") {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse::<u8>() {
            if n < 32 {
                return Some(('0', n));
            }
            if n < 64 {
                return Some(('1', n - 32));
            }
        }
        return None;
    }
    if let Some(rest) = r.strip_prefix('p') {
        let mut chars = rest.chars();
        if let Some(port_c) = chars.next() {
            // Lettered port A-I (STM32F4/F7 large packages reach port I; renode's
            // PortMap and QEMU's GpioBank both expose GPIOF..GPIOI there).
            let port_upper = port_c.to_ascii_uppercase();
            if ('A'..='I').contains(&port_upper) {
                let digits: String = rest[1..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(bit) = digits.parse::<u8>() {
                    if bit < 32 {
                        return Some((port_upper, bit));
                    }
                }
            }
            // Numeric port 0 or 1: the first digit is the port, the remaining
            // leading digits are the bit index.
            if port_c == '0' || port_c == '1' {
                let digits: String = rest[1..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(bit) = digits.parse::<u8>() {
                    if bit < 32 {
                        return Some((port_c, bit));
                    }
                }
            }
        }
    }
    None
}

/// Arduino digital pin number -> ATmega328P (port, bit).
fn arduino_digital_to_port(num: u8) -> Option<(char, u8)> {
    match num {
        0..=7 => Some(('D', num)),
        8..=13 => Some(('B', num - 8)),
        _ => None,
    }
}

/// Map a dual-purpose analog-pin role to its GPIO `(port, bit)`, so `bind_mcu`
/// can stamp the tri-stated driver half of the dual ADC+GPIO bind (dynamic
/// promotion). On the ATmega328P the analog pins
/// A0..A5 are port C bits 0..5 (PC0..PC5); A6/A7 (Nano modules) are ADC-only
/// with no port pin and return `None`, so they stay pure ADC probes. Handles
/// both the Nano module role ("a2", "a3_...") and the bare role carrying an
/// adc index ("pc2_adc2").
pub(crate) fn apin_gpio_of_role(role: &str, module: bool) -> Option<(char, u8)> {
    let r = role.to_ascii_lowercase();
    if module {
        let rest = r.strip_prefix('a')?;
        let n: u8 = rest.split('_').next().unwrap_or("").parse().ok()?;
        // A0..A5 = PC0..PC5; A6/A7 have no digital port pin.
        if n <= 5 {
            return Some(('C', n));
        }
        return None;
    }
    // Bare role like "pc2_adc2": let the standard parser recover the port pin.
    gpio_of_role(role, false)
}

/// Map a role string to an ADC channel number, if it is an analog input.
fn adc_of_role(role: &str, module: bool) -> Option<u8> {
    let r = role.to_ascii_lowercase();
    if module {
        // Nano "a0".."a7".
        if let Some(rest) = r.strip_prefix('a') {
            let n: u8 = rest.split('_').next().unwrap_or("").parse().ok()?;
            if n <= 7 {
                return Some(n);
            }
        }
        return None;
    }
    // Bare "pc0_adc0" .. "pc5_adc5".
    if let Some(idx) = r.find("adc") {
        let after = &r[idx + 3..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        return digits.parse().ok();
    }
    None
}

// ── Power-net detection ──────────────────────────────────────────────────────

/// True only for the canonical ground net itself: `"GND"` or `"0"`, optionally
/// behind a hierarchical sheet path (`"/Power/GND"`). Matches
/// [`Circuit::node`]'s own ground rule so pass-1 interning and later
/// `circuit.node(name)` calls agree on which net is node 0.
///
/// Deliberately NARROWER than [`is_ground`]: AGND / DGND / PGND / ISOGND /
/// CHASSIS_GND are real, distinct nets whose whole point is that they are NOT
/// the same copper as GND until something (a ferrite bead, a 0 Ω link, a star
/// point) joins them. Folding them all onto node 0 before binding turned every
/// such bridge into an inert self-loop and erased the board's ground topology,
/// the split-ground / galvanic-isolation structure this tool exists to check.
/// Rail-default and rating heuristics keep the broad [`is_ground`]; only the
/// pass-1 node assignment uses this.
///
/// `VSS` IS canonical ground: it is the IC-pin spelling of "the" ground (KiCad's
/// `power:VSS`), not a split-ground island like the *GND families. A CMOS/logic
/// board whose sole ground net is labelled `VSS` must still fuse onto node 0, or
/// the reference node floats and the whole MNA solve is singular / offset. `VEE`
/// is intentionally NOT included: on bipolar-supply analog boards it is a
/// negative rail (e.g. -15 V), and pinning that to 0 V would be a hard fault.
fn is_canonical_ground(name: &str) -> bool {
    let n = name.trim();
    // Hierarchical labels export as "/sheet/GND"; the leaf name decides.
    let leaf = n.rsplit('/').next().unwrap_or(n).trim();
    leaf == "0" || leaf.eq_ignore_ascii_case("gnd") || leaf.eq_ignore_ascii_case("vss")
}

/// True for ground-family net names.
pub fn is_ground(name: &str) -> bool {
    let n = name.trim().trim_start_matches('/').to_ascii_uppercase();
    matches!(
        n.as_str(),
        "GND" | "GNDA" | "GNDD" | "AGND" | "DGND" | "VSS" | "0" | "VEE"
    ) || n.ends_with("GND")
}

/// If `name` is a recognised supply rail, return its nominal voltage.
pub fn power_rail_voltage(name: &str) -> Option<f64> {
    let n = name.trim().trim_start_matches('/').to_ascii_uppercase();
    match n.as_str() {
        // Bare "VDD" is deliberately NOT here: it names a supply with no
        // magnitude, and on 3.3 V / 1.8 V boards (STM32/ESP32/nRF52) it is
        // usually the local 3.3 V core rail, stamping it Ideal at 5 V
        // overdrives every device on the net. Same discipline as bare "VEE"
        // below: inventing a voltage would guess; name the net with its
        // voltage (VDD3V3, VDD_5V) instead. Bare "VCC" stays: it is the
        // TTL/bipolar convention and conventionally means 5 V.
        "+5V" | "5V" | "VCC" | "+VCC" | "VBUS" | "+5V0" => Some(5.0),
        "+3V3" | "3V3" | "+3.3V" | "3.3V" | "VCC3V3" | "VDD3V3" => Some(3.3),
        "+3V" | "3V" => Some(3.0),
        "+12V" | "12V" => Some(12.0),
        "+15V" | "+15.0V" | "15V" => Some(15.0),
        "+24V" | "+24.0V" | "24V" => Some(24.0),
        "+1V8" | "1V8" | "1.8V" => Some(1.8),
        // Negative rails (analog supplies, RS-232 drivers, op-amp VEE feeds).
        // Without explicit arms these fall through to the substring fallback
        // below, which requires a leading '+' or a VCC/VBUS token, so "-5V"
        // returns None, gets no SupplyLeg, and silently floats at 0 V. A
        // negative rail is NOT ground: it must keep its own node AND get a
        // supply at the negative voltage. Bare "VEE" is deliberately
        // unresolved: its magnitude is board-dependent (-5/-12/-15 are all
        // common), so inventing one would guess; name the net with its
        // voltage instead.
        "-5V" | "-5V0" | "-5.0V" => Some(-5.0),
        "-12V" | "-12.0V" => Some(-12.0),
        "-15V" | "-15.0V" => Some(-15.0),
        "-3V3" | "-3.3V" => Some(-3.3),
        _ => {
            // A monitor/feedback/sense TAP named after the rail it watches
            // ("12V_FB", "VCC_5V_MON", "VDD_1V8_MON", "AVCC_2V5_SENSE") is a
            // divided tap, never a rail node, reject it up front so NO fallback
            // (numeric, embedded supply-token, or the loose "contains 5V"/"3V3"
            // substring branch) pins the tap to the full nominal. The per-fallback
            // gates below are the same discipline; this catches the cases (like the
            // supply-token substring branch) that would otherwise slip past them.
            if name_is_rail_monitor_tap(&n) {
                return None;
            }
            // '-'-prefixed rails first: "-5V_ANALOG" must resolve negative and
            // never fall into the positive "contains 5V" branch through an
            // incidental VCC/VBUS token elsewhere in the name.
            if let Some(v) = negative_rail_fallback(&n) {
                Some(v)
            // A numeric positive rail carries its own magnitude ("+15V",
            // "+24V", "+9V", "+15V0", "+15V_ANALOG"). This MUST run before the
            // loose "contains 5V" branch: "+15V" contains the substring "5V"
            // and starts with '+', so without this it was silently classified
            // as a 5 V rail, a +15V op-amp supply solved at 5 V.
            } else if let Some(v) = positive_rail_fallback(&n) {
                Some(v)
            // A supply-token-prefixed rail carries its magnitude in an embedded
            // voltage token ("VCC_5V", "VDD_3V3", "VDD_1V8", "VCC_15V",
            // "VBUS_25V"). This precise extraction MUST run before the loose
            // "contains 5V"/"3V3" substring heuristics below: "VCC_15V" contains
            // the substring "5V" and "VDD_13V3" contains "3V3", so the substring
            // branches misread them as 5 V / 3.3 V. embedded_rail_magnitude reads
            // the whole "15V"/"13V3" digit run and returns 15.0 / 13.3.
            } else if let Some(v) = embedded_rail_magnitude(&n) {
                Some(v)
            // Fallback for voltage-suffixed names embedded_rail_magnitude cannot
            // parse a numeric token from but that still carry a "5V"/"3V3"
            // substring next to a supply token (e.g. "VCC_5VOLTS").
            } else if n.contains("5V")
                && (n.starts_with('+')
                    || n.contains("VCC")
                    || n.contains("VBUS")
                    || n.contains("VDD"))
            {
                Some(5.0)
            // Same gate for the 3.3 V substring fallback: a bare domain-suffixed
            // SIGNAL net (an open-drain data line "SDA_3V3", a monitor "SENSE_3V3_MON",
            // an interrupt "IRQ_3.3V") is NOT a rail and must stay unresolved, or
            // Pass 3 stamps an ideal 3.3 V supply onto it and pins the line high.
            // Genuine rails still resolve: exact names hit the match arm above,
            // token-prefixed forms (VDD_3V3) via embedded_rail_magnitude, numeric
            // forms (+3V3_ANALOG) via positive_rail_fallback.
            } else if (n.contains("3V3") || n.contains("3.3V"))
                && (n.starts_with('+')
                    || n.contains("VCC")
                    || n.contains("VBUS")
                    || n.contains("VDD"))
            {
                Some(3.3)
            } else {
                None
            }
        }
    }
}

/// True when a net names itself a supply but carries no magnitude anyone can
/// read off the name, so [`power_rail_voltage`] correctly declines to guess one.
///
/// `ANALOG_VDD`, bare `VDD`, bare `VEE`, `AVDD`: every one is a supply, and not
/// one of them says what voltage. Inventing a number would overdrive or
/// underdrive every part on the net, so the binder leaves them unresolved and
/// they sit at 0 V. That is the right call and it is invisible, which is the
/// problem this predicate exists to fix. A caller that finds one of these with
/// no supply attached knows the board is running with a rail dead, and can say
/// so instead of reporting whatever the resulting operating point implies as
/// though it were a finding about the board.
///
/// Deliberately name-only and deliberately loose. Callers pair it with a
/// structural test (how many parts hang off the net) so a three-pin enable
/// called `VCC_EN` does not read as a dead rail. Ground is excluded: `VSS` and
/// friends are 0 V because that is what they are.
pub fn names_a_supply_of_unknown_voltage(name: &str) -> bool {
    if is_ground(name) || power_rail_voltage(name).is_some() {
        return false;
    }
    let n = name.trim().trim_start_matches('/').to_ascii_uppercase();
    // A tap that watches a rail is not the rail, same exclusion power_rail_voltage
    // makes before any of its fallbacks.
    if name_is_rail_monitor_tap(&n) {
        return false;
    }
    // The last path segment: a hierarchical sheet name must not decide this.
    let leaf = n.rsplit('/').next().unwrap_or(&n);
    const SUPPLY_TOKENS: [&str; 12] = [
        "VDD", "VCC", "VEE", "VBAT", "VBUS", "VSYS", "VIN", "AVDD", "AVCC", "DVDD", "VREG", "VAUX",
    ];
    SUPPLY_TOKENS.iter().any(|t| leaf.contains(t))
}

/// Extract a rail magnitude from a voltage token embedded MID-name, for
/// suffixed rails like `VDD_1V8`, `AVCC_2V5`, `VCC_1V2` whose voltage is neither
/// the canonical 5 V nor 3.3 V and whose name does not START with the digit (so
/// [`positive_rail_fallback`] skips it). Gated on a supply token so an arbitrary
/// signal net that merely contains a "1V2"-like substring is not misread as a
/// rail. Accepts both KiCad digit-V-digit ("1V8") and dotted ("1.8V") forms.
/// Expects `n` already trimmed / uppercased (as in [`power_rail_voltage`]).
fn embedded_rail_magnitude(n: &str) -> Option<f64> {
    const SUPPLY_TOKENS: [&str; 11] = [
        "VDD", "VCC", "VBUS", "VIN", "VSYS", "VBAT", "AVDD", "DVDD", "VOUT", "VREG", "VAUX",
    ];
    if !(n.starts_with('+') || SUPPLY_TOKENS.iter().any(|t| n.contains(t))) {
        return None;
    }
    let b = n.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        // An optional dotted decimal part ("1.8V").
        if i + 1 < b.len() && b[i] == b'.' && b[i + 1].is_ascii_digit() {
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
        let head = &n[start..i];
        if i < b.len() && b[i] == b'V' {
            // KiCad frac form: digits directly after the 'V' ("1V8" → 1.8).
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let frac = &n[i + 1..j];
            let mag: Option<f64> = if head.contains('.') || frac.is_empty() {
                head.parse().ok()
            } else {
                format!("{head}.{frac}").parse().ok()
            };
            if let Some(m) = mag {
                // No upper clamp: mirror positive_rail_fallback (which resolves
                // "+65V" -> 65). A <=60 V clamp made embedded_rail_magnitude
                // return None for a genuine high-voltage rail ("VBUS_65V"), so
                // control fell through to the loose "contains 5V" substring branch,
                // which matched the "5V" INSIDE "65V" and silently solved a 65 V
                // rail at 5 V, masking overvoltage stress. The supply-token + 'V'
                // gate is specific enough without the range clamp.
                if m > 0.0 && m.is_finite() {
                    return Some(m);
                }
            }
        }
    }
    None
}

/// True when the name is a rail MONITOR / FEEDBACK / SENSE tap: a voltage token
/// ("5V", "1V8", "3V3") immediately followed by a tap suffix ("_MON", "_FB",
/// "_SENSE", …). Such a net is a divided TAP of the rail (it sits below the rail
/// voltage), never the rail node itself, so NO rail resolver, numeric, embedded
/// supply-token, or the loose substring fallback, may pin it to the full nominal
/// with an ideal supply (that shorts the divider and masks the very under/over-
/// voltage the tap senses). Checked once, up front, so every fallback is covered.
fn name_is_rail_monitor_tap(n: &str) -> bool {
    let b = n.as_bytes();
    for i in 0..b.len() {
        if b[i] == b'V' && i > 0 && b[i - 1].is_ascii_digit() {
            // Skip a KiCad frac digit run after the 'V' ("1V8", "3V3").
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if is_rail_monitor_suffix(&n[j..]) {
                return true;
            }
        }
    }
    false
}

/// "+15V", "+24V_RAIL", "+15V0", "+9V", or a bare "15V" -> the positive rail
/// voltage. The mirror of [`negative_rail_fallback`]: an OPTIONAL leading '+',
/// then a numeric magnitude in plain ("15V") or KiCad digit-V-digit ("5V0",
/// "3V3") form. A name that does not start with a digit after the optional '+'
/// (e.g. "VDD_5V", "VCC") returns None and is left to the token heuristic.
/// Expects `n` already trimmed / uppercased (as in [`power_rail_voltage`]).
fn positive_rail_fallback(n: &str) -> Option<f64> {
    let rest = n.strip_prefix('+').unwrap_or(n);
    let int_part: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if int_part.is_empty() {
        return None;
    }
    let after = &rest[int_part.len()..];
    if !after.starts_with('V') {
        return None;
    }
    let frac: String = after[1..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    // A voltage-PREFIXED net whose suffix is a monitor/feedback/sense token
    // ("12V_FB", "5V_MON", "3V3_SENSE") is a divided TAP of the rail, not the rail
    // itself: it physically sits below the nominal (near Vref / a divider ratio).
    // The voltage-SUFFIXED sibling ("SENSE_3V3_MON") is already gated out of the
    // substring fallback; without this gate the prefix form slipped through here
    // and Pass 3 pinned the sense node to the full rail with an ideal supply,
    // shorting the divider and masking the very under/over-voltage it monitors.
    if is_rail_monitor_suffix(&after[1 + frac.len()..]) {
        return None;
    }
    let magnitude: f64 = if frac.is_empty() {
        int_part.parse().ok()?
    } else {
        format!("{}.{}", int_part.trim_end_matches('.'), frac)
            .parse()
            .ok()?
    };
    (magnitude > 0.0 && magnitude.is_finite()).then_some(magnitude)
}

/// True when the text right after a rail's voltage token is a monitor / feedback
/// / sense SUFFIX, i.e. the net is a divided TAP of the rail (it sits below the
/// rail voltage, at Vref or a divider fraction), NOT the rail itself. Pinning
/// such a net to the full nominal with an ideal supply defeats the divider and
/// masks the under/over-voltage the sense line exists to reveal. Rail-DOMAIN
/// suffixes (ANALOG, USB, CORE, RAIL, DIG, IO, …) are NOT taps and still resolve.
/// `tail` is the remainder after the voltage token (e.g. "_FB", "_MON", "_USB").
fn is_rail_monitor_suffix(tail: &str) -> bool {
    let first: String = tail
        .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    // Match on the ROOT, not an exact token: the same intent is spelled in
    // longer forms, "DIVIDER" (DIV), "SENSED"/"SENSING" (SENSE), "MEASURE"
    // (MEAS), "MONITORED" (MON), and those must still read as taps, or the net
    // falls through and is pinned to the full rail (the failure this guard
    // prevents).
    //
    // But prefix matching over-reaches onto rail-DOMAIN words that happen to
    // share a root's letters: "SENSOR" starts with "SENS" and "FBUS" with "FB",
    // yet "5V_SENSOR"/"3V3_FBUS" are genuine supply rails, not divided taps,
    // classifying them as taps drops their SupplyLeg and floats the whole domain
    // at 0 V. Those specific rail-domain words are excepted before the root test.
    const RAIL_DOMAIN_EXCEPTIONS: [&str; 2] = ["SENSOR", "FBUS"];
    if RAIL_DOMAIN_EXCEPTIONS.contains(&first.as_str()) {
        return false;
    }
    const TAP_ROOTS: [&str; 8] = ["FB", "FEEDBACK", "SENS", "MON", "DIV", "TAP", "MEAS", "SNS"];
    TAP_ROOTS.iter().any(|r| first.starts_with(r))
}

/// "-5V", "-12V_RAIL", "-5V0", "-3V3_ANALOG" -> the negative rail voltage.
/// The mirror of [`positive_rail_fallback`]: a leading '-', then a numeric
/// magnitude in plain ("12V") or KiCad digit-V-digit ("3V3", "5V0") form.
/// Expects `n` already trimmed / uppercased (as in [`power_rail_voltage`]).
fn negative_rail_fallback(n: &str) -> Option<f64> {
    let rest = n.strip_prefix('-')?;
    let int_part: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if int_part.is_empty() {
        return None;
    }
    let after = &rest[int_part.len()..];
    if !after.starts_with('V') {
        return None;
    }
    // Digits directly after the 'V' are the decimal part ("5V0", "3V3").
    let frac: String = after[1..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    // Same tap-suffix gate as the positive side: "-12V_MON" / "-5V_SENSE" is a
    // monitor of the negative rail, not the rail node itself.
    if is_rail_monitor_suffix(&after[1 + frac.len()..]) {
        return None;
    }
    let magnitude: f64 = if frac.is_empty() {
        int_part.parse().ok()?
    } else {
        format!("{}.{}", int_part.trim_end_matches('.'), frac)
            .parse()
            .ok()?
    };
    (magnitude > 0.0 && magnitude.is_finite()).then_some(-magnitude)
}

/// A net that carries a `power_out` (or `power_out+no_connect`) pin whose
/// pinfunction names a voltage is a supply rail at that voltage, no matter what
/// the NET is named. KiCad netlists from boards that use a non-canonical rail
/// label (e.g. `+5P`, `VBUS_SW`, `SYS_3V3`) still tag the *source pin* of a
/// regulator/MCU/connector as `power_out`, so this catches rails that pure
/// name-matching ([`power_rail_voltage`]) misses. Returns the rail voltage if
/// any `power_out` pin on the board drives this net id with a known function.
///
/// A genuine regulator output is handled separately (the vreg model sources its
/// own net); this only stamps an ideal rail when nothing else drives the net,
/// which is correct for an externally/MCU-supplied rail whose source is not
/// itself solved (e.g. the Arduino's `+5V` pin feeding the analog array).
fn power_out_net_voltages(board: &ExtractedBoard) -> HashMap<i64, f64> {
    let mut out: HashMap<i64, f64> = HashMap::new();
    for comp in &board.components {
        // A DNP part's power_out pin drives nothing on the real board.
        if comp.dnp {
            continue;
        }
        for pin in &comp.pins {
            if !pin.kind.starts_with("power_out") {
                continue;
            }
            let Some(net_id) = pin.net else { continue };
            if net_id == 0 {
                continue;
            }
            // A no_connect power_out pin drives nothing (e.g. an unused 3V3 leg).
            if pin.kind.contains("no_connect") {
                continue;
            }
            if let Some(v) = power_rail_voltage(&pin.function) {
                out.insert(net_id, v);
            }
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Identity from a BOM or a placement file
// ─────────────────────────────────────────────────────────────────────────────

/// What a bind owes to an artifact other than the layout.
///
/// Attribution is not decoration. Once a BOM can change what a part binds to, a
/// reader has to be able to ask "which file decided this?", and the answer has to
/// be per part rather than per run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityAttribution {
    pub reference: String,
    /// The part number the artifact supplied.
    pub mpn: String,
    /// The artifact's path, as the caller gave it.
    pub source: String,
    /// The artifact's kind, e.g. `lcsc_bom`.
    pub source_kind: String,
    /// The value string the layout carried, kept so nothing the layout said is
    /// lost when the part number stands in for an empty or unidentifying value.
    pub layout_value: String,
    /// What the part resolved to on the layout alone.
    pub before: Confidence,
    /// What it resolves to with the artifact's part number.
    pub after: Confidence,
    /// The model the part number reached, when it reached one.
    pub model_id: Option<String>,
}

/// A value string an artifact supplied for a part whose layout value was empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilledValue {
    pub reference: String,
    pub value: String,
    pub source: String,
}

/// Something the artifact and the board disagree about that is worth saying out
/// loud but does not make the pair unusable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityFinding {
    /// Both files name the part and they disagree about what it is, within one
    /// device class: the layout says `4k7` and the BOM says `10k`. The BOM's part
    /// number wins, per the precedence in [`apply_identity`], and the number that
    /// changed is named here so the change is never silent.
    ValueDisagrees {
        reference: String,
        layout: String,
        artifact: String,
        source: String,
    },
    /// The artifact names designators the board does not have. Ordinary in small
    /// numbers: a BOM covers mechanical parts with no footprint, a panel, or a
    /// variant.
    NotOnBoard {
        references: Vec<String>,
        source: String,
    },
    /// The board has parts the artifact does not name. Ordinary: a BOM omits test
    /// points and fiducials. Worth saying because the artifact was the thing that
    /// could have identified them.
    NotInArtifact {
        references: Vec<String>,
        source: String,
    },
    /// A BOM row's stated quantity disagrees with the number of designators the
    /// same row enumerates. The list wins: it is the enumerated fact and the
    /// quantity is a number derived from it.
    QuantityDisagrees {
        references: Vec<String>,
        stated: usize,
        enumerated: usize,
        source: String,
    },
    /// The layout's own do-not-populate flag and the BOM's populate column
    /// disagree. Never resolved here; see [`IdentityReport::advice`].
    PopulateDisagrees {
        reference: String,
        layout_says_dnp: bool,
        artifact_says_populate: bool,
        source: String,
    },
}

impl IdentityFinding {
    /// The one line a report prints for this finding.
    pub fn line(&self) -> String {
        match self {
            IdentityFinding::ValueDisagrees {
                reference,
                layout,
                artifact,
                source,
            } => format!(
                "{reference} is {layout:?} on the layout and {artifact:?} in {source}; the part \
                 number from {source} is what bound, so check which revision is current"
            ),
            IdentityFinding::NotOnBoard { references, source } => format!(
                "{source} names {} parts the board does not have: {}",
                references.len(),
                references.join(", ")
            ),
            IdentityFinding::NotInArtifact { references, source } => format!(
                "{} parts on the board are not in {source}: {}",
                references.len(),
                references.join(", ")
            ),
            IdentityFinding::QuantityDisagrees {
                references,
                stated,
                enumerated,
                source,
            } => format!(
                "{source} states a quantity of {stated} for a row that lists {enumerated} \
                 designators ({}); the list is what bound",
                references.join(", ")
            ),
            IdentityFinding::PopulateDisagrees {
                reference,
                layout_says_dnp,
                artifact_says_populate,
                source,
            } => format!(
                "{reference} is {} on the layout and {} in {source}; the do-not-populate policy \
                 decided it, not the BOM",
                if *layout_says_dnp {
                    "do-not-populate"
                } else {
                    "populated"
                },
                if *artifact_says_populate {
                    "populated"
                } else {
                    "do-not-populate"
                }
            ),
        }
    }
}

/// `--fit` / `--no-fit` names a BOM's populate column implies.
///
/// Deliberately advice rather than action. hauksbee already has a DNP policy
/// ([`hauksbee_extract::dnp::DnpPolicy`], applied by
/// `ExtractedBoard::apply_dnp_policy`), that policy is the single place the
/// question "is this part fitted?" is decided, and a second mechanism quietly
/// overriding it from a purchasing spreadsheet is exactly the kind of hidden
/// second opinion the DNP work exists to avoid. So the BOM's opinion is
/// surfaced, and the caller feeds these two lists into `apply_dnp_policy` if it
/// wants them honoured.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FitAdvice {
    /// Parts the layout marks DNP that the artifact says are populated.
    pub fit: Vec<String>,
    /// Parts the layout does not mark DNP that the artifact says are not
    /// populated. This is the half the layout does not carry at all, so it is the
    /// half worth having.
    pub no_fit: Vec<String>,
}

impl FitAdvice {
    pub fn is_empty(&self) -> bool {
        self.fit.is_empty() && self.no_fit.is_empty()
    }
}

/// What [`apply_identity`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdentityReport {
    /// Every part whose bind is owed to an artifact rather than to the layout.
    pub identified: Vec<IdentityAttribution>,
    /// Parts whose empty layout value an artifact filled.
    pub values_filled: Vec<FilledValue>,
    pub findings: Vec<IdentityFinding>,
    pub advice: FitAdvice,
}

impl IdentityReport {
    /// The lines a report prints. Empty when the artifact agreed with the layout
    /// about everything and added nothing, so a run whose BOM says nothing new
    /// never mentions the subject.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for a in &self.identified {
            out.push(format!(
                "  {} identified from {} as {:?}: {} -> {}",
                a.reference,
                a.source,
                a.mpn,
                confidence_word(a.before),
                confidence_word(a.after)
            ));
        }
        for v in &self.values_filled {
            out.push(format!(
                "  {} had no value on the layout; {} says {:?}",
                v.reference, v.source, v.value
            ));
        }
        for f in &self.findings {
            out.push(format!("  {}", f.line()));
        }
        if !self.advice.fit.is_empty() {
            out.push(format!(
                "  the BOM says these DNP parts are populated: {}. Re-run with --fit {} to \
                 honour it",
                self.advice.fit.join(", "),
                self.advice.fit.join(",")
            ));
        }
        if !self.advice.no_fit.is_empty() {
            out.push(format!(
                "  the BOM says these parts are not populated, and the layout does not: {}. \
                 Re-run with --no-fit {} to honour it",
                self.advice.no_fit.join(", "),
                self.advice.no_fit.join(",")
            ));
        }
        out
    }
}

/// A component kind in the lower-case form a report prints, so a refusal reads
/// as a sentence rather than as a Rust variant name.
fn kind_word(kind: ComponentKind) -> String {
    format!("{kind:?}").to_ascii_lowercase()
}

/// A model in the words a user recognises: its own description, or its kind when
/// it has none.
///
/// Never the model id. An id like `signal_diode_1n4148_fallback` is an internal
/// identifier the user never typed, and `docs/STYLE.md` rules those out of
/// user-facing text for the good reason that they send a reader looking for a file
/// rather than at their board.
fn model_words(model: &ModelEntry) -> String {
    if model.description.trim().is_empty() {
        kind_word(model.kind)
    } else {
        model.description.clone()
    }
}

fn confidence_word(c: Confidence) -> &'static str {
    match c {
        Confidence::Exact => "exact",
        Confidence::Family => "family",
        Confidence::Guessed => "guessed",
        Confidence::Unresolved => "unresolved",
    }
}

/// Why a set of identity hints cannot be used at all.
///
/// Both variants mean the same thing in different words: the two files describe
/// different boards, so anything computed from the pair would be a fact about a
/// board that does not exist. That is [`crate::result::EXIT_INVALID_FOR_ANALYSIS`],
/// the same treatment a board still carrying Git merge-conflict markers gets.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum IdentityRefusal {
    #[error(
        "{artifact} and the board disagree about what {} of the same parts ARE, which \
         means they are different revisions of the board rather than two views of \
         one: {detail}. Anything computed from the pair would describe a board that \
         does not exist. Use the BOM that was exported from this layout, or drop it \
         and analyse the layout alone",
        contradictions.len()
    )]
    Contradiction {
        artifact: String,
        contradictions: Vec<String>,
        detail: String,
    },

    #[error(
        "{artifact} names {total} reference designators and only {matched} of them are \
         on this board, so it is a BOM for a different board. Check which file goes \
         with which layout, then retry"
    )]
    WrongBoard {
        artifact: String,
        total: usize,
        matched: usize,
    },
}

impl IdentityRefusal {
    /// Always [`crate::result::EXIT_INVALID_FOR_ANALYSIS`].
    pub fn exit_code(&self) -> i32 {
        crate::result::EXIT_INVALID_FOR_ANALYSIS
    }
}

/// Below this share of an artifact's designators matching the board, the two are
/// not the same board. One in ten: a BOM legitimately carries mechanical parts
/// and a panel's worth of extras, but not ninety per cent of them.
const WRONG_BOARD_MATCH_RATIO: f64 = 0.1;

/// Apply identity from a BOM or a placement file to a board, before binding.
///
/// ## The precedence, and why
///
/// 1. **The layout decides whenever it can.** If the layout's value resolves the
///    part exactly or by family, that reading stands. The layout is the file the
///    netlist itself came from, so it is the description of the circuit; a BOM is
///    a description of a purchase, and it goes stale between revisions in a way
///    the layout cannot.
/// 2. **A part number decides only where the layout could not.** Where the layout
///    resolves nothing, or resolves only by a guess, an artifact's manufacturer
///    part number is allowed to settle it. This is the whole gain: an MPN is a
///    globally unique key naming exactly one device, so it identifies parts a
///    value string leaves anonymous, and it takes nothing away from the layout
///    because the layout said nothing. Every such bind is in
///    [`IdentityReport::identified`], attributed to the file that supplied it.
/// 3. **Two files naming different parts is refused, not merged.** If both
///    resolve and they reach different models, that is not a disagreement to
///    average: the files describe different revisions. Same for a class
///    disagreement, which is the case this feature exists for, a part the BOM
///    calls a `10k` and the layout calls a MOSFET.
/// 4. **An artifact's VALUE column never outranks the layout's.** It is the same
///    kind of claim, and it only fills a hole: a part whose layout value is empty,
///    which is every part on an Altium `.PcbDoc`, since Altium keeps values in
///    the schematic.
/// 5. **A magnitude disagreement inside one part is reported, not acted on.** The
///    layout says `10k` and the BOM says `4k7`: same device, different number.
///    The layout's number is used and the disagreement is a
///    [`IdentityFinding::ValueDisagrees`], because a number that changes between
///    revisions is worth a line in the report and is not worth refusing a run
///    over.
///
/// ## What it does not do
///
/// It does not touch the DNP decision. `apply_dnp_policy` owns that, and the
/// artifact's populate column arrives as [`IdentityReport::advice`] for the
/// caller to feed back in. A distributor order code is never used for identity at
/// all; [`hauksbee_extract::bom`] drops those before they get here.
pub fn apply_identity(
    board: &mut ExtractedBoard,
    hints: &[hauksbee_extract::bom::IdentityHint],
    lib: &ModelLibrary,
) -> Result<IdentityReport, IdentityRefusal> {
    use hauksbee_extract::bom::{IDENTITY_SOURCE_PROPERTY, MPN_PROPERTY, VALUE_PROPERTY};

    let mut report = IdentityReport::default();
    if hints.is_empty() {
        return Ok(report);
    }
    let source = hints[0].source.clone();

    // ── Is this even the same board? ────────────────────────────────────────
    let on_board: std::collections::HashSet<String> = board
        .components
        .iter()
        .map(|c| c.reference.clone())
        .collect();
    let mut named: Vec<String> = hints.iter().map(|h| h.reference.clone()).collect();
    named.sort();
    named.dedup();
    let matched = named.iter().filter(|r| on_board.contains(*r)).count();
    if (matched as f64) < named.len() as f64 * WRONG_BOARD_MATCH_RATIO {
        return Err(IdentityRefusal::WrongBoard {
            artifact: source,
            total: named.len(),
            matched,
        });
    }

    // ── Contradictions, gathered before anything is applied ─────────────────
    let mut contradictions: Vec<String> = Vec::new();
    for hint in hints {
        let Some(comp) = board.component(&hint.reference) else {
            continue;
        };
        if let Some(text) = contradiction_between(comp, hint, lib) {
            contradictions.push(text);
        }
    }
    if !contradictions.is_empty() {
        let detail = contradictions.join("; ");
        return Err(IdentityRefusal::Contradiction {
            artifact: source,
            contradictions,
            detail,
        });
    }

    // ── Apply ───────────────────────────────────────────────────────────────
    for hint in hints {
        let Some(idx) = board
            .components
            .iter()
            .position(|c| c.reference == hint.reference)
        else {
            continue;
        };

        if let Some(mpn) = hint.mpn.as_ref() {
            let before = resolve(lib, &board.components[idx]);
            let probe = probe_with_mpn(&board.components[idx], mpn);
            let substituted = probe.value != board.components[idx].value;
            let after = resolve(lib, &probe);
            if identity_improves(&before, &after) {
                let comp = &mut board.components[idx];
                let layout_value = comp.value.clone();
                comp.properties
                    .push((MPN_PROPERTY.to_string(), mpn.clone()));
                comp.properties
                    .push((IDENTITY_SOURCE_PROPERTY.to_string(), hint.source.clone()));
                if substituted {
                    comp.value = mpn.clone();
                }
                report.identified.push(IdentityAttribution {
                    reference: hint.reference.clone(),
                    mpn: mpn.clone(),
                    source: hint.source.clone(),
                    source_kind: hint.source_kind.clone(),
                    layout_value,
                    before: before.confidence,
                    after: after.confidence,
                    model_id: after.model.as_ref().map(|m| m.id.clone()),
                });
            }
        }

        // An artifact value fills a hole and nothing else.
        if let Some(value) = &hint.value {
            let comp = &mut board.components[idx];
            if comp.value.trim().is_empty() {
                comp.value = value.clone();
                comp.properties
                    .push((VALUE_PROPERTY.to_string(), value.clone()));
                report.values_filled.push(FilledValue {
                    reference: hint.reference.clone(),
                    value: value.clone(),
                    source: hint.source.clone(),
                });
            } else if let Some(text) = value_disagreement(&comp.value, value) {
                let _ = text;
                report.findings.push(IdentityFinding::ValueDisagrees {
                    reference: hint.reference.clone(),
                    layout: comp.value.clone(),
                    artifact: value.clone(),
                    source: hint.source.clone(),
                });
            }
        }

        // The populate column becomes advice, never an action.
        if let Some(populate) = hint.populate {
            let dnp = board.components[idx].dnp;
            if dnp && populate {
                report.advice.fit.push(hint.reference.clone());
            } else if !dnp && !populate {
                report.advice.no_fit.push(hint.reference.clone());
            }
            if dnp == populate {
                report.findings.push(IdentityFinding::PopulateDisagrees {
                    reference: hint.reference.clone(),
                    layout_says_dnp: dnp,
                    artifact_says_populate: populate,
                    source: hint.source.clone(),
                });
            }
        }
    }

    // ── The two directions of "one file knows about a part the other does not"
    let not_on_board: Vec<String> = named
        .iter()
        .filter(|r| !on_board.contains(*r))
        .cloned()
        .collect();
    if !not_on_board.is_empty() {
        report.findings.push(IdentityFinding::NotOnBoard {
            references: not_on_board,
            source: source.clone(),
        });
    }
    let named_set: std::collections::HashSet<&str> = named.iter().map(String::as_str).collect();
    let mut not_in_artifact: Vec<String> = board
        .components
        .iter()
        .filter(|c| !c.reference.is_empty() && !named_set.contains(c.reference.as_str()))
        .map(|c| c.reference.clone())
        .collect();
    not_in_artifact.sort();
    if !not_in_artifact.is_empty() {
        report.findings.push(IdentityFinding::NotInArtifact {
            references: not_in_artifact,
            source,
        });
    }

    Ok(report)
}

/// [`apply_identity`] for a whole BOM, plus the one reconciliation a BOM carries
/// that a bare hint list cannot express: a row whose stated quantity disagrees
/// with the number of designators the same row enumerates.
///
/// This is the entry point a caller with a BOM wants. Call it BEFORE
/// `ExtractedBoard::apply_dnp_policy`, so that the policy sees the board this
/// function may have added values to, and feed [`FitAdvice`] into that call if the
/// BOM's populate column is to be honoured.
pub fn apply_bom_identity(
    board: &mut ExtractedBoard,
    bom: &hauksbee_extract::bom::Bom,
    lib: &ModelLibrary,
) -> Result<IdentityReport, IdentityRefusal> {
    let mut report = apply_identity(board, &bom.identity_hints(), lib)?;
    for (row, stated, enumerated) in bom.quantity_disagreements() {
        report.findings.push(IdentityFinding::QuantityDisagrees {
            references: row.references.clone(),
            stated,
            enumerated,
            source: bom.provenance.path.clone(),
        });
    }
    Ok(report)
}

/// [`apply_identity`] for a placement file, which additionally reconciles where
/// the parts sit. A placement file whose positions disagree with the layout's is
/// from another revision, and that is refused for the same reason a contradicting
/// BOM is.
pub fn apply_placement_identity(
    board: &mut ExtractedBoard,
    file: &hauksbee_extract::placement::PlacementFile,
    lib: &ModelLibrary,
) -> Result<IdentityReport, IdentityRefusal> {
    let check = file.cross_check(board);
    if check.is_different_board() {
        let detail = check.lines().join("; ");
        return Err(IdentityRefusal::Contradiction {
            artifact: file.provenance.path.clone(),
            contradictions: check
                .position_disagreements
                .iter()
                .map(|d| d.reference.clone())
                .collect(),
            detail,
        });
    }
    apply_identity(board, &file.identity_hints(), lib)
}

/// Did the LAYOUT name a part, as opposed to stating a parameter or nothing?
///
/// A non-empty value that does not parse as a magnitude is a name: `ATmega328P-AU`,
/// `BSS138`, `LM4040BIM3-2.0`. A parseable magnitude (`10k`, `100nF`) is a
/// parameter. An empty value, KiCad's `~`, and the marker an Altium read leaves
/// behind are all nothing.
fn layout_names_a_part(comp: &Component) -> bool {
    let v = comp.value.trim();
    !v.is_empty()
        && v != "~"
        && parse_value(v).is_none()
        && !comp
            .properties
            .iter()
            .any(|(k, _)| k == hauksbee_extract::altium::VALUE_UNRESOLVED_KEY)
}

/// Do two identity strings plausibly name the same part?
///
/// Package and reel suffixes are the reason this exists: `ATmega328P-AU` and
/// `ATmega328P`, `BSS138` and `BSS138-7-F`, `BC847B` and `BC847B,215` are one part
/// written two ways, and refusing a run over a suffix would make the feature
/// unusable. Compared on alphanumerics only, case-insensitively: either a prefix
/// of the other, or a shared prefix of five characters, which is long enough that
/// `STM32F103C8` and `ATmega328P` share nothing.
fn names_same_part(a: &str, b: &str) -> bool {
    let norm = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_uppercase()
    };
    let (a, b) = (norm(a), norm(b));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a.starts_with(&b) || b.starts_with(&a) {
        return true;
    }
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count() >= 5
}

/// The component as it looks with an artifact's part number attached.
///
/// Two things are attached, and the second needs explaining. The part number goes
/// on the reserved property, which is where `resolve` looks for it. And where the
/// layout's value carries no electrical parameter, the part number ALSO stands in
/// for the value.
///
/// The substitution is not a shortcut. A model's match rules are ANDed, and nearly
/// every entry in the library declares a `value_re` written against part numbers
/// (`^MCP4728`, `^BSS138[A-Z0-9-]*$`). So a part number presented only as a part
/// number matches nothing: the query still carries the layout's own value, and the
/// value rule rejects it. Where the layout value is empty or is a part-number-shaped
/// string, standing in for it costs nothing, because that string was not a
/// parameter. Where the layout value PARSES as a magnitude, it is the part's
/// electrical parameter and is never touched: replacing `10k` with
/// `RC0402FR-0710KL` would delete the resistance.
fn probe_with_mpn(comp: &Component, mpn: &str) -> Component {
    let mut probe = comp.clone();
    probe.properties.push((
        hauksbee_extract::bom::MPN_PROPERTY.to_string(),
        mpn.to_string(),
    ));
    if parse_value(&comp.value).is_none() {
        probe.value = mpn.to_string();
    }
    probe
}

/// True when the artifact's part number identified a part the layout could not.
///
/// The narrowest rule that delivers the gain, which is precedence rules 1 and 2
/// together: the part number is used only where the layout did not decide.
///
/// "Did not decide" is `Unresolved` OR `Guessed`, and the second half matters as
/// much as the first. A `Guessed` reading is what a footprint prefix alone
/// produces for a part with no value: it is a shape, not a claim about a device,
/// so a part number that reaches an exact model is better evidence and replaces
/// it. A layout that reached `Exact` or `Family` keeps its reading, and if the
/// part number reaches a different one, [`contradiction_between`] has already
/// refused the whole read rather than letting this function quietly pick a winner.
fn identity_improves(
    before: &hauksbee_models::Resolution,
    after: &hauksbee_models::Resolution,
) -> bool {
    let rank = |c: Confidence| match c {
        Confidence::Exact => 0,
        Confidence::Family => 1,
        Confidence::Guessed => 2,
        Confidence::Unresolved => 3,
    };
    matches!(
        before.confidence,
        Confidence::Unresolved | Confidence::Guessed
    ) && rank(after.confidence) < rank(before.confidence)
        && after.model.is_some()
}

/// The device class of a value string, when it has one.
///
/// Only the three passive dimensions are claimed, because they are the only ones
/// a value string states unambiguously: an `F` suffix is a capacitance and
/// nothing else. A part number, a bare magnitude and an empty string all return
/// `None`, which means "this says nothing about the class" rather than "no
/// class", and nothing is refused on the strength of a `None`.
fn value_dimension(value: &str) -> Option<&'static str> {
    let parsed = parse_value(value)?;
    let unit = parsed.unit?.to_ascii_uppercase();
    match unit.as_str() {
        "F" => Some("a capacitance"),
        "H" => Some("an inductance"),
        "R" | "Ω" | "OHM" | "OHMS" => Some("a resistance"),
        _ => None,
    }
}

/// Is this bare magnitude written on a scale its designator never uses?
///
/// Returns the phrase a refusal uses, or `None` when the value is fine or states
/// its own unit (in which case the dimension detectors have already judged it).
/// The multiplier is the evidence: a capacitor or inductor written with a `k` or
/// `M` multiplier is a resistance value in the wrong row, and a resistor written in
/// pico or femto is a capacitance.
fn wrong_scale_for_designator(reference: &str, value: &str) -> Option<&'static str> {
    let parsed = parse_value(value)?;
    if parsed.unit.is_some() {
        return None;
    }
    let suffix = parsed.suffix?.to_ascii_uppercase();
    let prefix: String = reference
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    match prefix.as_str() {
        "C" | "L" if matches!(suffix.as_str(), "K" | "M" | "MEG" | "G" | "T") => {
            Some("a resistance-scale value")
        }
        "R" if matches!(suffix.as_str(), "P" | "N" | "F" | "A") => {
            Some("a capacitance-scale value")
        }
        _ => None,
    }
}

/// The dimension one value string states for one designator: the unit when the
/// string carries one, otherwise the designator's own convention for a bare
/// magnitude. `None` when neither says anything, which is every part number.
fn dimension_of(reference: &str, value: &str) -> Option<&'static str> {
    value_dimension(value).or_else(|| {
        parse_value(value)
            .is_some()
            .then(|| designator_dimension(reference))
            .flatten()
    })
}

/// The class a reference-designator prefix implies, for the three passives.
fn designator_dimension(reference: &str) -> Option<&'static str> {
    let prefix: String = reference
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    match prefix.as_str() {
        "R" => Some("a resistance"),
        "C" => Some("a capacitance"),
        "L" => Some("an inductance"),
        _ => None,
    }
}

/// Do the two files disagree about the CLASS of one part, rather than about its
/// value?
///
/// Two independent detectors, because either alone misses half the cases. The
/// model library sees a part number the value parser cannot read: it resolves
/// `AO3400A` to a MOSFET and `RC0402FR-0710KL` to a resistor, so a disagreement
/// in [`ComponentKind`] is decisive. The value parser sees a dimension the model
/// library has no entry for: `10k` against `100nF` is ohms against farads whether
/// or not either resolves. And a designator prefix is itself a class claim, which
/// catches the case the brief exists for: a `Q5` the BOM calls `10k`.
fn contradiction_between(
    comp: &Component,
    hint: &hauksbee_extract::bom::IdentityHint,
    lib: &ModelLibrary,
) -> Option<String> {
    let reference = &comp.reference;

    // 1. Two files, two parts.
    //
    // The layout resolved the designator and the artifact's own part number
    // resolves it to something else. Different KINDS is the obvious case; a
    // different model of the same kind is the same problem in a quieter voice
    // (two MCUs, two regulators), and is worth the same refusal because a run
    // that silently swaps the simulated chip is exactly the confident wrong
    // answer this feature must not produce.
    //
    // The part number is resolved on its own rather than alongside the layout's
    // value, because the model library ANDs its match rules: a query carrying
    // both a value and a part number that name different parts matches neither,
    // and the disagreement would go unnoticed.
    //
    // The test is whether the LAYOUT NAMED A PART, not what confidence it reached.
    // Those come apart in both directions and each way costs something real. A
    // bare `Diode_SMD:D_SOD-123` footprint with no value at all reaches a model
    // (a generic 1N4148 stand-in, at `Guessed`), and treating that as the layout
    // naming a part refuses the whole file over a BOM that AGREES with it. A
    // layout value of `ATmega328P-AU` also reaches only `Guessed`, because the
    // library has no entry for that exact suffix, but the board plainly named a
    // part, and a BOM saying `STM32F103C8` is then two different boards.
    //
    // So: the layout named a part when its value is a non-empty string that is not
    // a magnitude. A magnitude is a parameter, not a name.
    if let Some(mpn) = &hint.mpn {
        if layout_names_a_part(comp) {
            let layout = resolve(lib, comp);
            let artifact = resolve(lib, &probe_with_mpn(comp, mpn));
            let decisive = matches!(artifact.confidence, Confidence::Exact | Confidence::Family);
            if let (Some(l), Some(a)) = (&layout.model, &artifact.model) {
                // Different models are not automatically different parts: the
                // layout's reading is often an engine stand-in for the very part
                // the BOM names (`ATmega328P-AU` reaching a fallback while
                // `ATmega328P` reaches the library entry). Two strings that name
                // the same part are not a disagreement, so the strings decide and
                // the models only widen it to a kind mismatch.
                let same_part = names_same_part(&comp.value, mpn);
                if decisive && l.id != a.id && !same_part {
                    return Some(format!(
                        "{reference} is {:?} (\"{}\") on the layout and {:?} (\"{}\") in the BOM",
                        comp.value,
                        model_words(l),
                        mpn,
                        model_words(a),
                    ));
                }
                if l.kind != a.kind && !same_part {
                    return Some(format!(
                        "{reference} is {:?} (a {}) on the layout and {:?} (a {}) in the BOM",
                        comp.value,
                        kind_word(l.kind),
                        mpn,
                        kind_word(a.kind),
                    ));
                }
            }
        }
    }

    // 2. Two dimensions.
    //
    // A bare magnitude states no unit, so the designator supplies it: `10k` on an
    // `R` is ohms and on a `C` is farads, which is the convention every value
    // string relies on. That makes `10k` against `100nF` on one designator
    // decisive without either side needing a model.
    let artifact_value = hint.value.as_deref().unwrap_or("");
    if let (Some(l), Some(a)) = (
        dimension_of(reference, &comp.value),
        dimension_of(reference, artifact_value),
    ) {
        if l != a {
            return Some(format!(
                "{reference} is {:?} ({l}) on the layout and {artifact_value:?} ({a}) in the BOM",
                comp.value
            ));
        }
    }

    // 3. A designator prefix against a stated dimension, for the case where the
    //    layout's own value says nothing at all.
    if let (Some(d), Some(a)) = (
        designator_dimension(reference),
        value_dimension(artifact_value),
    ) {
        if d != a {
            return Some(format!(
                "{reference} is a {} designator, so it is {d}, and the BOM calls it \
                 {artifact_value:?} ({a})",
                reference
                    .chars()
                    .take_while(|c| c.is_ascii_alphabetic())
                    .collect::<String>()
            ));
        }
    }
    // 4. A magnitude on the wrong scale for its designator.
    //
    // `C9` with an empty layout value and a BOM value of `10k`. No dimension is
    // stated on either side, no model resolves, and the designator prefix agrees
    // with the BOM that this is a capacitor, so detectors 1 to 3 all pass it
    // through and the fill path builds a 10 kilofarad capacitor: a dead short at
    // every frequency, from a plausible-looking cell.
    //
    // The tell is the MULTIPLIER, not the magnitude. Capacitors and inductors are
    // written in pico, nano, micro and milli; a `k` or a `M` on one is a resistance
    // value pasted into the wrong row. The reverse holds for a resistor written in
    // pico. Absolute bounds would be the wrong tool, because a 3000 F supercapacitor
    // is a real part.
    if !artifact_value.is_empty() {
        if let Some(scale) = wrong_scale_for_designator(reference, artifact_value) {
            if comp.value.trim() != artifact_value.trim() {
                return Some(format!(
                    "{reference} is a {} designator and the BOM calls it \
                     {artifact_value:?}, which is {scale}",
                    reference
                        .chars()
                        .take_while(|c| c.is_ascii_alphabetic())
                        .collect::<String>()
                ));
            }
        }
    }

    // 5. A passive value against a part the layout resolved as a semiconductor.
    //
    // This is the case the feature exists for: the BOM calls Q5 a `10k` and the
    // layout calls it a MOSFET. It needs its own detector because `10k` carries
    // no unit, so no dimension is stated and detectors 2 and 3 cannot see it. A
    // bare magnitude IS a passive value by convention, and a transistor's value
    // is never one, so the pair is decisive.
    if !artifact_value.is_empty() && parse_value(artifact_value).is_some() {
        let layout_kind = resolve(lib, comp).model.map(|m| m.kind);
        let semiconductor = matches!(
            layout_kind,
            Some(
                ComponentKind::Nmos
                    | ComponentKind::Pmos
                    | ComponentKind::BjtNpn
                    | ComponentKind::BjtPnp
                    | ComponentKind::Diode
                    | ComponentKind::Mcu
                    | ComponentKind::Vreg
                    | ComponentKind::Opamp
                    | ComponentKind::Dac
                    | ComponentKind::Adc
            )
        );
        if semiconductor {
            return Some(format!(
                "{reference} is {:?} ({}) on the layout and {artifact_value:?}, a passive value, \
                 in the BOM",
                comp.value,
                kind_word(layout_kind.expect("matched above"))
            ));
        }
    }
    None
}

/// Do two value strings for one part disagree about its magnitude?
///
/// Only fires when BOTH parse and both state a magnitude, so a part number
/// against a value never registers here. A relative difference under a tenth of
/// a per cent is the same value written two ways (`0.1uF` against `100nF`).
fn value_disagreement(layout: &str, artifact: &str) -> Option<String> {
    let l = parse_value(layout)?;
    let a = parse_value(artifact)?;
    let scale = l.si.abs().max(a.si.abs()).max(f64::MIN_POSITIVE);
    ((l.si - a.si).abs() / scale > 1e-3).then(|| format!("{layout} against {artifact}"))
}

/// A natural-order sort key for a reference designator: the leading alpha
/// prefix, then the parsed trailing integer, then the raw string as a
/// tie-break. Ordering by this puts "U2" before "U10" (2 < 10), unlike raw
/// byte-lexicographic `String` Ord which puts "U10" before "U2" because '1' <
/// '2'. Used to assign MCP4728 addresses in ascending device order regardless
/// of designator width.
fn natural_ref_key(reference: &str) -> (String, u64, String) {
    let prefix: String = reference
        .chars()
        .take_while(|c| !c.is_ascii_digit())
        .collect();
    let digits: String = reference[prefix.len()..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let num = digits.parse::<u64>().unwrap_or(0);
    (prefix.to_ascii_uppercase(), num, reference.to_string())
}

#[cfg(test)]
mod canonical_ground_tests {
    use super::*;

    #[test]
    fn vss_is_canonical_ground_but_split_families_and_vee_are_not() {
        // R35: a board whose sole ground is spelled VSS (KiCad power:VSS) must
        // fuse onto node 0, or the reference node floats and the MNA solve is
        // singular. VSS is the IC-pin spelling of "the" ground, not a split
        // island.
        assert!(is_canonical_ground("VSS"));
        assert!(is_canonical_ground("vss"));
        assert!(is_canonical_ground("/Power/VSS"));
        // GND / 0 unchanged.
        assert!(is_canonical_ground("GND"));
        assert!(is_canonical_ground("0"));
        // The deliberately-split ground families stay distinct so bridges
        // (ferrite bead / 0 Ω link / star point) are preserved.
        for split in ["AGND", "DGND", "PGND", "ISOGND", "CHASSIS_GND"] {
            assert!(
                !is_canonical_ground(split),
                "{split} must stay a distinct node"
            );
        }
        // VEE is a negative supply rail on bipolar-supply analog boards; pinning
        // it to 0 V would be a hard fault, so it is NOT canonical ground.
        assert!(!is_canonical_ground("VEE"));
    }
}

#[cfg(test)]
mod natural_ref_key_tests {
    use super::*;

    #[test]
    fn natural_key_orders_numerically_not_lexicographically() {
        // Raw String Ord would rank "U10" < "U2" (byte-wise '1' < '2'); the
        // natural key must rank U2 before U10 so addresses ascend by device.
        let mut refs = vec!["U10", "U2", "U1", "U100"];
        refs.sort_by(|a, b| natural_ref_key(a).cmp(&natural_ref_key(b)));
        assert_eq!(refs, vec!["U1", "U2", "U10", "U100"]);
    }

    #[test]
    fn mcp4728_addresses_ascend_by_natural_device_order() {
        // Simulate the address-assignment pass over DAC bindings whose
        // designators are non-uniform width (U2, U10). U2 must get 0x60 and
        // U10 0x61, which plain lexicographic ordering would reverse.
        let mut dacs = vec![
            DacBinding {
                reference: "U10".into(),
                address: 0,
                vref: 0.0,
                gain: 0,
                vout_drivers: [None, None, None, None],
            },
            DacBinding {
                reference: "U2".into(),
                address: 0,
                vref: 0.0,
                gain: 0,
                vout_drivers: [None, None, None, None],
            },
        ];
        dacs.sort_by(|a, b| natural_ref_key(&a.reference).cmp(&natural_ref_key(&b.reference)));
        for (i, d) in dacs.iter_mut().enumerate() {
            d.address = 0x60 + i as u8;
        }
        let addr = |r: &str| dacs.iter().find(|d| d.reference == r).unwrap().address;
        assert_eq!(addr("U2"), 0x60, "U2 is the first device");
        assert_eq!(addr("U10"), 0x61, "U10 is the second device");
    }
}

#[cfg(test)]
mod digital_ro_tests {
    use super::*;
    use hauksbee_models::{ComponentQuery, ModelLibrary};

    fn bare_comp(reference: &str) -> Component {
        Component {
            reference: reference.to_string(),
            value: String::new(),
            lib_id: String::new(),
            footprint: String::new(),
            position: None,
            layer: String::new(),
            properties: Vec::new(),
            dnp: false,
            pins: Vec::new(),
        }
    }

    #[test]
    fn digital_output_driver_honours_model_ro() {
        // R12: a [models.logic] part's `ro` (drive strength) was parsed but never
        // applied, every stamped Thevenin driver used DEFAULT_RO. Bind a 74HC595
        // whose model declares a custom ro and assert the driver carries it.
        let mut model = ModelLibrary::builtin()
            .resolve(&ComponentQuery::new(
                None,
                Some("74HC595".to_string()),
                None,
            ))
            .model
            .expect("builtin 74HC595");
        let custom_ro = 123.0;
        assert_ne!(
            custom_ro, DEFAULT_RO,
            "the test value must differ from the default"
        );
        model.params.set_f64("ro", custom_ro);

        let mut circuit = Circuit::new();
        let mut roles: HashMap<String, NodeId> = HashMap::new();
        for r in ["srclk", "rclk", "ser", "qa", "qb"] {
            roles.insert(r.into(), circuit.node(&r.to_uppercase()));
        }
        let mut digital = Vec::new();
        bind_digital(&bare_comp("U1"), &model, &mut circuit, &roles, &mut digital)
            .expect("the builtin 595 spec compiles");

        assert_eq!(digital.len(), 1, "the 595 binds");
        let drv = digital[0]
            .drivers
            .get("qa")
            .expect("qa output driver stamped");
        assert_eq!(
            drv.ron, custom_ro,
            "driver must carry the model's ro, not DEFAULT_RO"
        );
    }

    /// NEP-board study defect 3: a digital part whose [models.logic] fails to
    /// compile must NOT report as bound. A compile error that only reaches
    /// stderr, with the caller recording `BindOutcome::Digital` regardless,
    /// makes the report (and `critical_parts_bound`) count a part whose nets
    /// float as healthy.
    /// The error must come back to the caller so the row reads UNRESOLVED.
    #[test]
    fn broken_logic_spec_reports_unresolved_not_bound() {
        let mut model = ModelLibrary::builtin()
            .resolve(&ComponentQuery::new(
                None,
                Some("74HC595".to_string()),
                None,
            ))
            .model
            .expect("builtin 74HC595");
        // Corrupt the spec: a comb expression referencing an undefined signal
        // cannot compile (the shape a bad --models-dir override produces).
        model
            .logic
            .comb
            .insert("qa".to_string(), "no_such_signal & ser".to_string());

        let mut circuit = Circuit::new();
        let mut roles: HashMap<String, NodeId> = HashMap::new();
        for r in ["srclk", "rclk", "ser", "qa", "qb"] {
            roles.insert(r.into(), circuit.node(&r.to_uppercase()));
        }
        let mut digital = Vec::new();
        let err = bind_digital(&bare_comp("U1"), &model, &mut circuit, &roles, &mut digital)
            .expect_err("a spec that cannot compile must surface its error");
        assert!(
            !err.is_empty(),
            "the returned reason must carry the compile error"
        );
        assert!(
            digital.is_empty(),
            "no DigitalComponent may be pushed for a part that failed to compile"
        );
    }

    /// R23 (vreg-silent-5v-default): a vreg model with no `vout` param falls
    /// back to 5.0 V, which overdrives a 3.3 V board. Regulating there
    /// silently is the hazard, so a missing `vout` must emit a warning that
    /// names the assumed default.
    #[test]
    fn vreg_without_vout_param_warns_about_the_assumed_default() {
        let mut model = make_entry(
            "generic_ldo",
            ComponentKind::Vreg,
            "vreg with no vout param",
            hauksbee_models::Params::default(),
            std::collections::BTreeMap::new(),
        );
        // Ensure there is genuinely no vout param.
        assert!(model.params.get_f64("vout").is_none());

        let mut circuit = Circuit::new();
        let mut roles: HashMap<String, NodeId> = HashMap::new();
        roles.insert("out".into(), circuit.node("VOUT"));

        let (_outcome, warning) = bind_vreg(&bare_comp("U9"), &model, &mut circuit, &roles, true);
        let warning = warning.expect("a missing vout must produce a warning");
        assert!(
            warning.contains("vout") && warning.contains("5.0"),
            "the warning must name the missing param and the assumed default: {warning}"
        );

        // With an explicit vout the part binds silently at that voltage.
        model.params.set_f64("vout", 3.3);
        let mut circuit2 = Circuit::new();
        let mut roles2: HashMap<String, NodeId> = HashMap::new();
        roles2.insert("out".into(), circuit2.node("VOUT"));
        let (outcome, warning) = bind_vreg(&bare_comp("U9"), &model, &mut circuit2, &roles2, true);
        assert!(warning.is_none(), "a present vout must not warn");
        assert!(
            matches!(outcome, BindOutcome::Behavioral { device } if device.contains("3.3")),
            "the source regulates to the declared 3.3 V"
        );
    }

    /// R30 (spst-fallback-wires-ctrl-as-terminal): an analog switch with only its
    /// common and control pins wired (the switched throw unconnected/DNP) reaches
    /// the SPST fallback. The fallback must leave the switch OPEN; the control
    /// net is the gate, never a signal terminal. Picking the two lowest-NodeId
    /// non-POWER roles as the two throws wires `ctrl` itself as a terminal and
    /// stamps a VSwitch whose `b` equals its own `ctrl_p`: a fabricated ~ron
    /// path shorting the common signal net to the control line (and injecting
    /// the control voltage) whenever the gate goes high.
    #[test]
    fn ctrl_role_recognises_all_multi_gate_and_select_spellings() {
        // R45: any control spelling is_ctrl_role misses (the multi-gate branch's
        // ctrl_2/ctrl_3/ctrl_4, the SPDT sel/s controls) leaks into the SPST
        // fallback's throw candidates and can be stamped as a switch terminal,
        // shorting a signal net onto a control net. Every control spelling must be
        // excluded.
        for r in [
            "ctrl", "ctrl_1", "ctrl_2", "ctrl_3", "ctrl_4", "in", "s", "sel",
        ] {
            assert!(is_ctrl_role(r), "{r} must be recognised as a control role");
        }
        // Throw terminals and power roles are NOT control roles.
        for r in ["s0", "s1", "in_out_1a", "in_out_2b", "com", "vcc", "vss"] {
            assert!(
                !is_ctrl_role(r),
                "{r} must NOT be treated as a control role"
            );
        }
    }

    #[test]
    fn spst_fallback_does_not_wire_control_as_a_switch_terminal() {
        let model = make_entry(
            "generic_analog_switch",
            ComponentKind::AnalogSwitch,
            "SPST with only com + ctrl wired",
            hauksbee_models::Params::default(),
            std::collections::BTreeMap::new(),
        );
        let mut circuit = Circuit::new();
        let mut roles: HashMap<String, NodeId> = HashMap::new();
        roles.insert("com".into(), circuit.node("SIG"));
        roles.insert("ctrl".into(), circuit.node("GATE"));
        let power_nets: HashMap<String, f64> = HashMap::new();

        let (outcome, _warning) =
            bind_analog_switch(&bare_comp("U7"), &model, &mut circuit, &roles, &power_nets);

        let vswitches = circuit
            .devices
            .iter()
            .filter(|d| matches!(d, Device::VSwitch { .. }))
            .count();
        assert_eq!(
            vswitches, 0,
            "no throw is connected: the switch must be left open, not shorted to its control net"
        );
        assert!(
            matches!(outcome, BindOutcome::Unresolved { .. }),
            "an unconnected switch path must be reported as open/unresolved, got {outcome:?}"
        );
    }

    #[test]
    fn spst_fallback_s0_throw_conducts_on_control_low() {
        // R40: the s0 / NC throw conducts when the control is LOW (role_from_pinfunction
        // maps nc->s0 with this contract). The SPST fallback used the default
        // control-HIGH polarity, inverting it; the modeled com<->s0 contact was
        // OPEN exactly when the real one is CLOSED. A partially-wired 3157
        // (com + s0 + ctrl, s1 floating) must stamp a VSwitch whose sense is
        // inverted so it closes on control LOW.
        let model = make_entry(
            "generic_analog_switch",
            ComponentKind::AnalogSwitch,
            "NC SPST: com + s0 + ctrl",
            hauksbee_models::Params::default(),
            std::collections::BTreeMap::new(),
        );
        let mut circuit = Circuit::new();
        let gate = circuit.node("GATE");
        let mut roles: HashMap<String, NodeId> = HashMap::new();
        roles.insert("com".into(), circuit.node("SIG"));
        roles.insert("s0".into(), circuit.node("OUT"));
        roles.insert("ctrl".into(), gate);
        let power_nets: HashMap<String, f64> = HashMap::new();

        let _ = bind_analog_switch(&bare_comp("U8"), &model, &mut circuit, &roles, &power_nets);

        let (cp, cn, von, voff) = circuit
            .devices
            .iter()
            .find_map(|d| match d {
                Device::VSwitch {
                    ctrl_p,
                    ctrl_n,
                    von,
                    voff,
                    ..
                } => Some((*ctrl_p, *ctrl_n, *von, *voff)),
                _ => None,
            })
            .expect("a VSwitch for the com<->s0 throw");
        // Inverted sense: ctrl_p is the vss/ground reference and ctrl_n is the gate,
        // with negative thresholds; the switch closes when V(gate) is LOW.
        assert_eq!(
            cp,
            NodeId::GROUND,
            "s0 throw senses (vss - ctrl): ctrl_p is vss"
        );
        assert_eq!(cn, gate, "ctrl_n is the control net");
        assert!(
            von < 0.0 && voff < von,
            "control-low polarity requires negative thresholds, got von={von} voff={voff}"
        );
    }

    /// R31 (spdt-no-nc-inverted): the NO/NC pin-function tokens must land on
    /// the right throws. s0 is the throw that conducts when the control is LOW; by
    /// the universal SPDT convention the Normally-Closed contact conducts at rest
    /// (control-low) and Normally-Open closes on control-high. So NC → s0 and
    /// NO → s1. The inverted mapping routes COM to the wrong throw in every
    /// control state on any board that names its throws NO/NC.
    #[test]
    fn spdt_no_nc_map_to_the_correct_throws() {
        assert_eq!(
            role_from_pinfunction(ComponentKind::AnalogSwitch, "nc").as_deref(),
            Some("s0"),
            "NC (normally-closed) conducts at control-low = s0"
        );
        assert_eq!(
            role_from_pinfunction(ComponentKind::AnalogSwitch, "no").as_deref(),
            Some("s1"),
            "NO (normally-open) closes on control-high = s1"
        );
        // The digit aliases keep their established meaning (b1/s0 = low select).
        assert_eq!(
            role_from_pinfunction(ComponentKind::AnalogSwitch, "b1").as_deref(),
            Some("s0")
        );
        assert_eq!(
            role_from_pinfunction(ComponentKind::AnalogSwitch, "b2").as_deref(),
            Some("s1")
        );
    }
}

#[cfg(test)]
mod crystal_fallback_tests {
    use super::*;
    use hauksbee_models::ComponentKind;

    fn comp(reference: &str, value: &str) -> Component {
        Component {
            reference: reference.to_string(),
            value: value.to_string(),
            lib_id: String::new(),
            footprint: String::new(),
            position: None,
            layer: String::new(),
            properties: Vec::new(),
            dnp: false,
            pins: Vec::new(),
        }
    }

    #[test]
    fn partially_wired_paired_bjt_unit_warns_left_open() {
        // R53: a matched-pair BJT (BCM847BS) whose Q2 is partially wired (>=1 pin
        // connected but not a complete c/b/e) was silently dropped with warning
        // None, unlike a single BJT or a passive-array element which each warn
        // "left open". A partial unit must now surface a diagnostic.
        let model: ModelEntry = toml::from_str(
            "id = \"bcm847bs\"\nkind = \"bjt_npn\"\n[match]\nvalue_re = \"BCM847\"\n",
        )
        .unwrap();
        let mut circuit = Circuit::new();
        let (c1, b1, e1, b2) = (
            circuit.node("C1"),
            circuit.node("B1"),
            circuit.node("E1"),
            circuit.node("B2"),
        );
        let mut roles = HashMap::new();
        roles.insert("collector_q1".to_string(), c1);
        roles.insert("base_q1".to_string(), b1);
        roles.insert("emitter_q1".to_string(), e1);
        roles.insert("base_q2".to_string(), b2); // Q2 partial: only the base wired
        let (outcome, warning) = bind_bjt(&comp("Q1", "BCM847BS"), &model, &mut circuit, &roles);
        assert!(
            matches!(outcome, BindOutcome::Analog { .. }),
            "Q1 still stamps"
        );
        let w = warning.expect("a partially-wired Q2 must warn, not drop silently");
        assert!(
            w.contains("left open") && w.contains("q2"),
            "the warning must name the dropped unit: {w}"
        );

        // A fully-wired pair produces no warning.
        let c2 = circuit.node("C2");
        let e2 = circuit.node("E2");
        roles.insert("collector_q2".to_string(), c2);
        roles.insert("emitter_q2".to_string(), e2);
        let (_o, warning) = bind_bjt(&comp("Q1", "BCM847BS"), &model, &mut circuit, &roles);
        assert!(
            warning.is_none(),
            "a fully-wired pair must not warn: {warning:?}"
        );
    }

    #[test]
    fn frequency_values_are_recognised() {
        for v in [
            "16MHz",
            "8Mhz",
            "16.000MHz",
            "32.768kHz",
            "100 Hz",
            "12000000Hz",
        ] {
            assert!(value_is_frequency(v), "{v} should read as a frequency");
        }
        // R52: the SPACE-separated SI-prefixed form ("16 MHz") left a trailing
        // space after the prefix was stripped and was wrongly rejected, so a
        // C-prefixed crystal valued "16 MHz" fell through to the capacitor
        // heuristic (a gigafarad cap that collapses the solve). The doc comment
        // even lists "8 Mhz" as accepted. These must all read as frequencies.
        for v in ["16 MHz", "8 Mhz", "32.768 kHz", "1 GHz", "16 mhz"] {
            assert!(
                value_is_frequency(v),
                "{v} (space-separated) should read as a frequency"
            );
        }
        // Real passive values, and ferrite-bead impedance@frequency values
        // (which end in "hz" but are NOT crystals) must not trip it.
        for v in [
            "22pF",
            "10k",
            "4k7",
            "100nF",
            "BCM857BS",
            "Hz",
            "Choke",
            "",
            "600@100MHz",
            "1k@100MHz",
            "120@100MHz",
        ] {
            assert!(!value_is_frequency(v), "{v} must NOT read as a frequency");
        }
    }

    #[test]
    fn crystal_detected_by_reference_or_frequency_value() {
        assert!(is_crystal_like("Y", "16MHz"));
        assert!(is_crystal_like("CRYSTAL", "")); // KiCad 5 default ref, value missing
        assert!(is_crystal_like("XTAL", "8MHz"));
        assert!(is_crystal_like("C", "16Mhz")); // 'C'-prefixed crystal caught by the value
        assert!(!is_crystal_like("C", "22pF")); // a genuine capacitor
        assert!(!is_crystal_like("R", "10k"));
    }

    /// The load-bearing regression: a crystal whose reference starts with 'C'
    /// and whose value is a frequency must NOT bind as a (gigafarad) capacitor.
    /// Before the fix it bound Passive with value 16e6 F, which collapsed the
    /// whole co-sim solve.
    #[test]
    fn crystal_named_with_c_prefix_is_high_impedance_not_a_capacitor() {
        let entry = fallback_entry(&comp("Crystal1", "16Mhz")).expect("crystal binds");
        assert_eq!(entry.id, "crystal_fallback");
        assert_eq!(entry.kind, ComponentKind::Ignore);

        // A real 'C' capacitor still binds as a passive (no regression).
        let cap = fallback_entry(&comp("C7", "22pF")).expect("cap binds");
        assert_eq!(cap.kind, ComponentKind::Passive);
    }

    fn comp_fp(reference: &str, value: &str, footprint: &str) -> Component {
        let mut c = comp(reference, value);
        c.footprint = footprint.to_string();
        c
    }

    /// R23 (MELF-passive-silent-open): a MELF-footprint resistor/cap has a
    /// diode-shaped body, so an unqualified diode-evidence gate deletes it
    /// (return None -> open circuit). An R/C/L *reference* whose value is a clear passive
    /// magnitude must fall through to the passive fallback instead.
    #[test]
    fn melf_footprint_resistor_binds_as_passive_not_open() {
        let entry = fallback_entry(&comp_fp("R5", "10k", "Resistor_SMD:R_MELF_MELF0207"))
            .expect("a 10k MELF resistor must bind, not be left open");
        assert_eq!(entry.kind, ComponentKind::Passive);

        // Same for an L on a MELF/diode-ish body.
        let ind = fallback_entry(&comp_fp("L2", "10uH", "Inductor_SMD:L_MELF"))
            .expect("a MELF inductor binds");
        assert_eq!(ind.kind, ComponentKind::Passive);

        // A genuine diode REFERENCE on a diode footprint still bails (or binds as
        // a diode), never as a passive; the reference-class gate wins.
        let cr = fallback_entry(&comp_fp("CR1", "5.1V", "Diode_SMD:D_SOD-123"));
        assert!(
            cr.map_or(true, |e| e.kind != ComponentKind::Passive),
            "a CR-referenced zener must never bind as a passive"
        );

        // A footprint-only diode with a bare/generic value still binds as a diode.
        let d = fallback_entry(&comp_fp("D9", "D", "Diode_SMD:D_SOD-123"))
            .expect("a generic diode-footprint part binds as a diode");
        assert_eq!(d.kind, ComponentKind::Diode);
    }
}

#[cfg(test)]
mod fmt_tests {
    use super::fmt_eng;

    #[test]
    fn capacitor_scales_to_pico_nano_micro() {
        // A fixed µF scale renders 390 pF as "0.000 µF"; the scale tracks the
        // magnitude instead.
        assert_eq!(fmt_eng(390e-12, "F"), "390 pF");
        assert_eq!(fmt_eng(1e-9, "F"), "1 nF");
        assert_eq!(fmt_eng(4.7e-9, "F"), "4.7 nF");
        assert_eq!(fmt_eng(100e-9, "F"), "100 nF");
        assert_eq!(fmt_eng(0.1e-6, "F"), "100 nF");
        assert_eq!(fmt_eng(10e-6, "F"), "10 µF");
        assert_eq!(fmt_eng(1200e-6, "F"), "1.2 mF");
    }

    #[test]
    fn inductor_and_resistor_scale_too() {
        assert_eq!(fmt_eng(2.2e-6, "H"), "2.2 µH");
        assert_eq!(fmt_eng(10e-9, "H"), "10 nH");
        assert_eq!(fmt_eng(4700.0, "Ω"), "4.7 kΩ");
        assert_eq!(fmt_eng(1_000_000.0, "Ω"), "1 MΩ");
        assert_eq!(fmt_eng(0.05, "Ω"), "50 mΩ");
    }

    #[test]
    fn zero_and_nonfinite_are_safe() {
        assert_eq!(fmt_eng(0.0, "F"), "0 F");
        assert_eq!(fmt_eng(f64::NAN, "Ω"), "0 Ω");
    }

    #[test]
    fn decade_carry_renormalizes_to_the_next_prefix() {
        // Round-28: a mantissa in [999.5, 1000) rounds to "1000", so fmt_eng
        // rendered "1000 kΩ", a mantissa outside the promised [1,1000) range and
        // inconsistent with the sibling format_engineering. The carry must promote
        // to the next-larger prefix ("1 MΩ"), at every decade boundary.
        assert_eq!(fmt_eng(999_600.0, "Ω"), "1 MΩ");
        assert_eq!(fmt_eng(999.6, "Ω"), "1 kΩ");
        assert_eq!(fmt_eng(0.9996, "Ω"), "1 Ω");
        // µF -> mF carry: 999.6 µF rounds up a decade.
        assert_eq!(fmt_eng(999.6e-6, "F"), "1 mF");
        // Values comfortably inside a decade are unaffected.
        assert_eq!(fmt_eng(4700.0, "Ω"), "4.7 kΩ");
        assert_eq!(fmt_eng(990.0, "Ω"), "990 Ω");
        // The top prefix has nothing larger to carry into: a big mantissa stays.
        assert_eq!(fmt_eng(2_200_000_000.0, "Ω"), "2200 MΩ");
    }
}

#[cfg(test)]
mod rail_voltage_tests {
    use super::power_rail_voltage;

    /// Round-8 #1: a positive numeric rail carries its own magnitude. "+15V"
    /// contains the substring "5V" and starts with '+', so a loose substring
    /// heuristic classifies it as a 5 V rail and solves a +15V op-amp supply
    /// at 5 V. The positive fallback parses the true magnitude instead.
    #[test]
    fn positive_numeric_rails_keep_their_magnitude() {
        assert_eq!(power_rail_voltage("+15V"), Some(15.0));
        assert_eq!(power_rail_voltage("+25V"), Some(25.0));
        assert_eq!(power_rail_voltage("+15V0"), Some(15.0));
        assert_eq!(power_rail_voltage("+24V"), Some(24.0));
        assert_eq!(power_rail_voltage("+9V"), Some(9.0));
        assert_eq!(power_rail_voltage("+15V_ANALOG"), Some(15.0));
        // The genuine 5 V rails still resolve to 5.
        assert_eq!(power_rail_voltage("+5V"), Some(5.0));
        assert_eq!(power_rail_voltage("+5V_USB"), Some(5.0));
        assert_eq!(power_rail_voltage("VCC_5V"), Some(5.0));
        // Symmetry with the negative side.
        assert_eq!(power_rail_voltage("-15V"), Some(-15.0));
        // A voltage-less VDD token carries no magnitude of its own.
        assert_eq!(power_rail_voltage("+15V"), Some(15.0));
    }

    /// R11: voltage-SUFFIXED rails whose magnitude is neither 5 V nor 3.3 V and
    /// whose name does not start with the digit must still resolve; they fell
    /// through every arm and floated at 0 V.
    #[test]
    fn voltage_suffixed_rails_resolve() {
        assert_eq!(power_rail_voltage("VDD_1V8"), Some(1.8));
        assert_eq!(power_rail_voltage("VCC_1V2"), Some(1.2));
        assert_eq!(power_rail_voltage("AVCC_2V5"), Some(2.5));
        assert_eq!(power_rail_voltage("VOUT_1V0"), Some(1.0));
        assert_eq!(power_rail_voltage("DVDD_0V9"), Some(0.9));
        // Dotted form embedded in a supply-token name.
        assert_eq!(power_rail_voltage("VDD_1.2V"), Some(1.2));
        // A bare supply token with no magnitude still resolves to nothing (the
        // deliberate no-guess policy): inventing a voltage would be a guess.
        assert_eq!(power_rail_voltage("VDD"), None);
        assert_eq!(power_rail_voltage("VEE"), None);
        // A plain signal net (no supply token) is never read as a rail even if
        // it happens to contain a "1V2"-looking substring.
        assert_eq!(power_rail_voltage("SENSE_1V2_MON"), None);
    }

    /// R14: a supply-token-prefixed rail whose magnitude's last digits are "5V"
    /// or "3V3" must read its FULL magnitude, not be swallowed by the loose
    /// "contains 5V"/"3V3" substring heuristic. "VCC_15V" is a ±15 V analog
    /// supply, not 5 V; "VDD_13V3" is 13.3 V, not 3.3 V.
    #[test]
    fn token_prefixed_rails_ending_in_5v_or_3v3_read_full_magnitude() {
        assert_eq!(power_rail_voltage("VCC_15V"), Some(15.0));
        assert_eq!(power_rail_voltage("VDD_15V"), Some(15.0));
        assert_eq!(power_rail_voltage("VBUS_25V"), Some(25.0));
        assert_eq!(power_rail_voltage("VDD_13V3"), Some(13.3));
        // The genuine token-prefixed 5 V / 3.3 V rails still resolve correctly.
        assert_eq!(power_rail_voltage("VCC_5V"), Some(5.0));
        assert_eq!(power_rail_voltage("VDD_3V3"), Some(3.3));
        assert_eq!(power_rail_voltage("VCC_3.3V"), Some(3.3));
    }

    /// R33: a supply-token rail ABOVE 60 V that embeds a "5V"/"3V3" digit
    /// substring ("VBUS_65V" contains "5V", "VDD_63V3" contains "3V3") must read
    /// its FULL magnitude. Clamping embedded_rail_magnitude above 60 V to None
    /// drops control into the loose substring branch, which silently solves a
    /// 65 V rail at 5 V and hides overvoltage stress, so there is no upper
    /// clamp. The '+' form ("+65V") resolves via positive_rail_fallback and the
    /// token form matches it.
    #[test]
    fn high_voltage_token_rails_are_not_swallowed_by_the_5v_substring() {
        assert_eq!(power_rail_voltage("VBUS_65V"), Some(65.0));
        assert_eq!(power_rail_voltage("VBUS_75V"), Some(75.0));
        assert_eq!(power_rail_voltage("VCC_95V"), Some(95.0));
        assert_eq!(power_rail_voltage("VDD_63V3"), Some(63.3));
        // The '+' form was already correct and must stay so.
        assert_eq!(power_rail_voltage("+65V"), Some(65.0));
    }

    /// R16: a bare domain-suffixed SIGNAL net that merely contains "3V3"/"3.3V"
    /// (an open-drain I2C line "SDA_3V3", a monitor "SENSE_3V3_MON", an interrupt
    /// "IRQ_3.3V") is not a rail and must stay unresolved, else Pass 3 stamps an
    /// ideal 3.3 V supply onto it and pins the line high, fabricating bus data.
    /// The 3V3 fallback is now gated on a supply token, matching the 5V branch.
    #[test]
    fn domain_suffixed_signal_nets_are_not_read_as_3v3_rails() {
        assert_eq!(power_rail_voltage("SDA_3V3"), None);
        assert_eq!(power_rail_voltage("SCL_3V3"), None);
        assert_eq!(power_rail_voltage("SENSE_3V3_MON"), None);
        assert_eq!(power_rail_voltage("IRQ_3.3V"), None);
        assert_eq!(power_rail_voltage("TXD_3V3"), None);
        // The 5 V sibling was already gated; confirm the symmetry holds.
        assert_eq!(power_rail_voltage("SDA_5V"), None);
        // Genuine 3.3 V rails in every accepted form still resolve.
        assert_eq!(power_rail_voltage("3V3"), Some(3.3));
        assert_eq!(power_rail_voltage("+3V3"), Some(3.3));
        assert_eq!(power_rail_voltage("+3V3_ANALOG"), Some(3.3));
        assert_eq!(power_rail_voltage("VDD_3V3"), Some(3.3));
        assert_eq!(power_rail_voltage("VCC_3.3V"), Some(3.3));
    }

    /// Round-27: the voltage-PREFIXED mirror of the suffix-signal case. A monitor
    /// / feedback / sense TAP named after the rail it watches ("12V_FB",
    /// "3V3_SENSE", "5V_MON") physically sits BELOW the rail voltage, yet
    /// positive_rail_fallback resolved it as a full ideal rail, Pass 3 then pinned
    /// the divider tap to the nominal, shorting the divider and masking the
    /// under/over-voltage the sense line exists to reveal. Such names must stay
    /// unresolved; rail-DOMAIN names must keep resolving.
    #[test]
    fn voltage_prefixed_monitor_taps_are_not_read_as_rails() {
        assert_eq!(power_rail_voltage("12V_FB"), None);
        assert_eq!(power_rail_voltage("3V3_SENSE"), None);
        assert_eq!(power_rail_voltage("5V_MON"), None);
        assert_eq!(power_rail_voltage("12V_MON"), None);
        assert_eq!(power_rail_voltage("5V_FEEDBACK"), None);
        assert_eq!(power_rail_voltage("12V_DIV"), None);
        // The negative mirror is gated too.
        assert_eq!(power_rail_voltage("-12V_MON"), None);
        assert_eq!(power_rail_voltage("-5V_SENSE"), None);
        // Round-28: the supply-token-PREFIXED embedded form ("VDD_1V8_MON") must be
        // gated too; the r27 fix only covered the digit-prefixed and negative
        // paths, leaving embedded_rail_magnitude ungated.
        assert_eq!(power_rail_voltage("VDD_1V8_MON"), None);
        assert_eq!(power_rail_voltage("VCC_5V_MON"), None);
        assert_eq!(power_rail_voltage("AVCC_2V5_SENSE"), None);
        assert_eq!(power_rail_voltage("VOUT_1V0_FB"), None);
        assert_eq!(power_rail_voltage("VDD_13V3_MON"), None);
        // A genuine embedded rail (no tap suffix) still resolves.
        assert_eq!(power_rail_voltage("VDD_1V8"), Some(1.8));
        assert_eq!(power_rail_voltage("AVCC_2V5"), Some(2.5));
        // Genuine rails, including rail-DOMAIN suffixes, still resolve.
        assert_eq!(power_rail_voltage("12V"), Some(12.0));
        assert_eq!(power_rail_voltage("+15V_ANALOG"), Some(15.0));
        assert_eq!(power_rail_voltage("+5V_USB"), Some(5.0));
        assert_eq!(power_rail_voltage("+3V3_ANALOG"), Some(3.3));
        assert_eq!(power_rail_voltage("+9V"), Some(9.0));
        assert_eq!(power_rail_voltage("-5V"), Some(-5.0));
        assert_eq!(power_rail_voltage("-3V3_ANALOG"), Some(-3.3));
    }

    /// R38: the tap-suffix guard matched exact tokens ("DIV","SENSE","MEAS","MON"),
    /// so common longer spellings of the same intent, "DIVIDER", "SENSED",
    /// "SENSING", "MEASURE", "MONITORED", fell through and the divided tap was
    /// resolved as a full rail (then pinned high by an ideal supply, masking the
    /// under/over-voltage the divider exists to sense). The root now matches.
    #[test]
    fn longer_monitor_tap_spellings_are_not_read_as_rails() {
        assert_eq!(power_rail_voltage("12V_DIVIDER"), None);
        assert_eq!(power_rail_voltage("3V3_SENSED"), None);
        assert_eq!(power_rail_voltage("5V_SENSING"), None);
        assert_eq!(power_rail_voltage("12V_MEASURE"), None);
        assert_eq!(power_rail_voltage("5V_MONITORED"), None);
        // Embedded and negative mirrors of the longer spellings too.
        assert_eq!(power_rail_voltage("VDD_1V8_DIVIDER"), None);
        assert_eq!(power_rail_voltage("-12V_SENSED"), None);
        // Rail-DOMAIN suffixes that merely resemble a root must still resolve as
        // rails (none of DIG/IO/USB/CORE/ANALOG starts with a tap root).
        assert_eq!(power_rail_voltage("+3V3_DIG"), Some(3.3));
        assert_eq!(power_rail_voltage("+5V_USB"), Some(5.0));
        assert_eq!(power_rail_voltage("+15V_ANALOG"), Some(15.0));
    }

    /// R39: the R38 root-prefix match over-reached, "SENSOR" starts with the
    /// "SENS" tap root and "FBUS" with "FB", so genuine sensor/fieldbus SUPPLY
    /// rails were misclassified as monitor taps and left floating at 0 V. Those
    /// rail-domain words are excepted; the true tap spellings still resolve as taps.
    #[test]
    fn sensor_and_fieldbus_supply_rails_are_not_mistaken_for_taps() {
        assert_eq!(power_rail_voltage("5V_SENSOR"), Some(5.0));
        assert_eq!(power_rail_voltage("3V3_SENSOR"), Some(3.3));
        assert_eq!(power_rail_voltage("12V_SENSOR"), Some(12.0));
        assert_eq!(power_rail_voltage("3V3_FBUS"), Some(3.3));
        // The genuine taps (including the longer R38 spellings) are still taps.
        assert_eq!(power_rail_voltage("5V_SENSE"), None);
        assert_eq!(power_rail_voltage("3V3_SENSED"), None);
        assert_eq!(power_rail_voltage("5V_SENSING"), None);
        assert_eq!(power_rail_voltage("12V_FB"), None);
    }
}

#[cfg(test)]
mod mcu_route_tests {
    use super::{route_mcu_family_str, McuFamilyRoute};

    /// R11: the RISC-V ESP32 variants (C6/C2/H2/P4) must NOT fall through to the
    /// Xtensa `qemu:esp32` catch-all, that would execute RISC-V firmware on the
    /// wrong ISA. No platform is wired yet, so they route to NoPlatform.
    #[test]
    fn riscv_esp32_variants_do_not_misroute_to_xtensa() {
        for (part, fam) in [
            ("ESP32-C6", "ESP32-C6"),
            ("ESP32-C2", "ESP32-C2"),
            ("ESP32-H2", "ESP32-H2"),
            ("ESP32-P4", "ESP32-P4"),
        ] {
            match route_mcu_family_str(part) {
                Some(McuFamilyRoute::NoPlatform { family }) => assert_eq!(family, fam),
                other => panic!("{part} must be NoPlatform, got {other:?}"),
            }
        }
        // The Xtensa parts still route to their cores.
        assert!(matches!(
            route_mcu_family_str("ESP32-C3"),
            Some(McuFamilyRoute::Backend {
                backend: "qemu:esp32c3",
                ..
            })
        ));
        assert!(matches!(
            route_mcu_family_str("ESP32-WROOM-32E"),
            Some(McuFamilyRoute::Backend {
                backend: "qemu:esp32",
                ..
            })
        ));
    }
}

#[cfg(test)]
mod gpio_role_tests {
    use super::{apin_gpio_of_role, gpio_of_role, role_from_mcu_pinfunction_token};

    #[test]
    fn module_analog_pins_resolve_only_through_the_apin_fallback() {
        // Round-27: on a module (Nano) board the analog roles are "a0".."a5", and
        // gpio_of_role's module branch only understands the 'd' prefix; it returns
        // None for every 'a' role. bind_mcu recovers the port pin via the apin
        // fallback, but the three scheduler boot-hazard/boot-state reporters used
        // gpio_of_role ALONE and silently dropped firmware-driven A-pins. They now
        // apply the same fallback; this guards the mapping contract they rely on.
        for role in ["a0", "a2", "a5", "a3_scl"] {
            assert_eq!(
                gpio_of_role(role, true),
                None,
                "{role}: plain gpio_of_role can't see a module analog pin"
            );
        }
        // The fallback maps A0..A5 to PC0..PC5; A6/A7 stay ADC-only (no port pin).
        assert_eq!(apin_gpio_of_role("a0", true), Some(('C', 0)));
        assert_eq!(apin_gpio_of_role("a2", true), Some(('C', 2)));
        assert_eq!(apin_gpio_of_role("a5", true), Some(('C', 5)));
        assert_eq!(apin_gpio_of_role("a6", true), None, "A6 is ADC-only");
        // The combined lookup the reporters now use resolves the A-pin.
        let combined = |r: &str| gpio_of_role(r, true).or_else(|| apin_gpio_of_role(r, true));
        assert_eq!(
            combined("a2"),
            Some(('C', 2)),
            "A2 = OE_S resolves for hazard reports"
        );
        // A digital "d13" role still resolves the ordinary way, unaffected.
        assert_eq!(combined("d13"), gpio_of_role("d13", true));
    }

    /// R11: STM32 GPIO banks run past port E, an F4/F7 in a large package has
    /// PF/PG/PH/PI. Both the pin-role stage and the role→(port,bit) stage must
    /// cover them; a cap at E or at G silently drops every pin on those banks.
    #[test]
    fn stm32_ports_past_e_map() {
        // Pin-function → role: F/G/H/I survive.
        assert_eq!(
            role_from_mcu_pinfunction_token("PF9"),
            Some("pf9".to_string())
        );
        assert_eq!(
            role_from_mcu_pinfunction_token("PI15"),
            Some("pi15".to_string())
        );
        assert_eq!(role_from_mcu_pinfunction_token("PZ0"), None);
        // Role → (port, bit): the same banks resolve to a GPIO driver target
        // (gpio_of_role returns the uppercase port letter).
        assert_eq!(gpio_of_role("pa0", false), Some(('A', 0)));
        assert_eq!(gpio_of_role("pf9", false), Some(('F', 9)));
        assert_eq!(gpio_of_role("ph1", false), Some(('H', 1)));
        assert_eq!(gpio_of_role("pi15", false), Some(('I', 15)));
        assert_eq!(gpio_of_role("pz0", false), None);
    }
}
