//! The binder: [`ExtractedBoard`] + [`ModelLibrary`] -> [`BoundBoard`].
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/binder.md.
//!
//! Every component is resolved to a model and turned into something the
//! co-sim can run:
//!   - passives (R/C/L) -> analog IR devices from the parsed `Value`;
//!   - diode / BJT / MOSFET -> analog IR devices with SPICE params;
//!   - vreg -> a behavioral ideal source on its output net;
//!   - `analog_switch` -> `VSwitch`; opamp/comparator -> behavioral IR devices;
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

use hauksbee_extract::{is_placeholder_value, Component, ExtractedBoard};
use hauksbee_ir::evidence::{Assumption, ModelSource, ModelUncertainty};
use hauksbee_ir::{
    BDep, BOutput, BjtModel, Circuit, CompiledExpr, Device, DeviceId, DiodeModel, MosLevel,
    MosfetModel, NodeId, Polarity, Rail, SourceKind,
};
use hauksbee_models::sensor_spec::StrapTie;
use hauksbee_models::validation::repeatable_role;
use hauksbee_models::value::parse_value;
use hauksbee_models::{
    Bus, ComponentKind, ComponentQuery, Confidence, ModelEntry, ModelLibrary, PeripheralPower,
    PeripheralSpec, SensorSpec,
};

use crate::bind_report::{BindOutcome, BindReport, BindRow, SimpleRegulator};
use crate::digital::{output_roles, DigitalComponent, SupplyDraw};
use crate::drivers::{PinDriver, DEFAULT_RO};
use crate::power_supply::{PowerSupply, SupplyLeg};
use crate::stress::DeviceMeta;
use hauksbee_extract::assembly::{AssemblyState, FittedComponent};

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
    ///. May be empty when the board gives no value string.
    pub requested_part: String,
    /// True when an assembled crystal/resonator physically bridges this MCU's
    /// `osc_in` and `osc_out` nets. The Renode descriptor consumes this as
    /// external-clock presence; a missing or DNP crystal must not make HSERDY.
    pub external_clock_present: bool,
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
    /// carry a digital port pin (e.g. `ATmega328P` A0..A5 = PC0..PC5). ADC-ONLY
    /// channels (A6/A7) have NO entry. The scheduler uses this to decide whether a
    /// channel was promoted to output by checking THIS channel's own driver,
    /// never merely whether some other pin's driver shares the net.
    pub adc_pin: HashMap<u8, (char, u8)>,
    /// Whether this is a module wrapper (`Arduino_Nano`) using header pad names.
    pub module: bool,
    /// Absolute-maximum supply voltage (V) from the model's ratings
    /// (`max_voltage_v`), when the model carries one. The scheduler turns this
    /// plus the vcc/vdd `role_nets` entries into supply-rail stress watches, so
    /// a rail driven past the chip's abs-max Vcc raises an overvoltage fault
    /// instead of nothing.
    pub max_supply_v: Option<f64>,
}

/// One MCP4728 quad DAC discovered on the board.
///
/// Carries the assigned 7-bit I2C address, the board reference/value config
/// (VREF, gain), and the four VOUT-channel [`PinDriver`]s already stamped into
/// the circuit. The scheduler realizes each binding as a spec-driven
/// `RegisterMapSensor` (in `hauksbee_cosim::peripherals`) instance of
/// `testdata/sensor-specs/mcp4728.toml` on a shared bus, binding these drivers to
/// the spec's per-channel outputs; the slave then drives the VOUT nets itself at
/// each transaction end (the ctx-bearing `on_stop`), so the analog solve sees the
/// DAC output voltages.
pub struct DacBinding {
    pub reference: String,
    pub address: u8,
    pub vref: f64,
    pub gain: u8,
    /// VOUT channel A..D -> stamped driver (None if that channel net is
    /// unconnected / tied to ground on this board).
    pub vout_drivers: [Option<PinDriver>; 4],
}

/// Firmware-visible peripheral discovered from a resolved model card. The
/// scheduler constructs the protocol slave automatically; the board supplies
/// only wiring facts such as the SPI chip-select net.
#[derive(Debug, Clone)]
pub struct PeripheralBinding {
    pub reference: String,
    pub spec: PeripheralSpec,
    pub cs_net: Option<NodeId>,
    /// Address selected by an exact board strap. `None` means use the address
    /// embedded in the portable sensor spec.
    pub i2c_address_override: Option<u8>,
    /// Why the override is the spec's default rather than what the board's
    /// straps select (a floating or signal-net strap). Surfaced as a
    /// partial-model assumption on the bind row.
    pub i2c_address_assumed: Option<String>,
    /// Datasheet current envelope and the exact board rails that power this
    /// peripheral. `None` means the model deliberately makes no electrical
    /// current/power-gating claim; protocol behavior remains usable but cannot
    /// be promoted to bus-coupled electrical behavior in model coverage.
    pub power: Option<BoundPeripheralPower>,
}

#[derive(Debug, Clone)]
pub struct BoundPeripheralPower {
    pub spec: PeripheralPower,
    pub supply_node: NodeId,
    pub return_node: NodeId,
}

/// The bound board: a ready-to-solve circuit plus the event-driven layer.
#[derive(Default)]
pub struct BoundBoard {
    pub name: String,
    pub circuit: Circuit,
    /// Net name -> circuit node.
    pub net_nodes: HashMap<String, NodeId>,
    /// All net names in *net-declaration order* (for `board_info` / frame counts).
    ///
    /// WARNING: this is NOT a `NodeId` reverse map. It is pushed once per real
    /// net (skipping `KiCad`'s no-net id 0), so it has no entry for the ground
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
    /// Fitted parts recognised as a processor family that no emulator models,
    /// as (reference, family). Lets a zero-processor refusal name the family
    /// ("STM32G4 is recognised but has no emulator backend yet") instead of
    /// implying the part went unrecognised.
    pub unsupported_mcus: Vec<(String, String)>,
    /// reference -> resolved model kind string (for `board_info` coloring).
    pub component_kinds: HashMap<String, String>,
    /// Named controllable input sources: reference -> `DeviceId` of a Vsource /
    /// Isource the UI can override (sliders).
    pub input_sources: HashMap<String, hauksbee_ir::DeviceId>,
    /// Configurable power supplies, one per detected supply net.
    /// Default to [`PowerSupply::Ideal`] at the rail's nominal voltage, so a
    /// board whose supplies are left unconfigured sees perfect rails.
    pub supplies: Vec<SupplyLeg>,
    /// Behavioural devices (power ICs with a declarative behavioural model:
    /// chargers, PMICs, balancers). Iterated by the scheduler each chunk, the
    /// same cadence as the supplies.
    pub behavioral: Vec<crate::behavioral::BehavioralDevice>,
    /// Per-device metadata for the fault/stress monitor.
    pub device_meta: Vec<DeviceMeta>,
    /// MCP4728 quad DACs discovered on the board, with their VOUT drivers. The
    /// scheduler turns these into I2C slaves and drives the analog VOUT nets.
    pub dacs: Vec<DacBinding>,
    /// Source-bound I2C/SPI slaves declared by component model cards.
    pub peripherals: Vec<PeripheralBinding>,
    /// Set when some part's stamp uses a [`Device::VSwitch`] as a LATCH rather
    /// than as a pass element (today: `bind_timer_555`). `switch_model` is a
    /// global solver option, and under `SwitchModel::Smooth` the switch is
    /// explicitly memoryless, so such a part would silently free-run and print
    /// a plausible wrong frequency. The scheduler forces `Hysteretic` on a
    /// board carrying one; an analog switch, which Smooth models correctly, is
    /// deliberately not counted here.
    pub latching_switches: bool,
    pub report: BindReport,
}

impl BoundBoard {
    /// Every resistor whose overpower check could not run because its package was
    /// unreadable, in message form.
    ///
    /// Lives here so any caller holding a `BoundBoard` can surface it, not only
    /// the CI runner: an abstention visible on one surface and silent on the
    /// others is still a silent abstention on those others.
    #[must_use]
    pub fn power_coverage_gaps(&self) -> Vec<String> {
        crate::stress::aggregate_power_coverage_gaps(&self.device_meta)
    }

    /// Where `part` takes the power it regulates onto a rail: a simple
    /// regulator's input (with the least voltage it needs there, when the
    /// card states a dropout and the rail is a fixed positive one) or a
    /// converter's input. `None` for any other
    /// part, including a shunt reference, which is fed through its resistor.
    #[must_use]
    pub fn feed_of(&self, part: &str) -> Option<(NodeId, Option<f64>)> {
        if let Some(r) = self.report.regulators.iter().find(|r| r.reference == part) {
            // A magnitude: stated as "at least" only for a positive rail.
            let positive = matches!(
                self.circuit.devices.get(r.source.0 as usize),
                Some(Device::Vsource { kind: SourceKind::Dc(v), .. }) if *v > 0.0
            );
            let need = r.dropout_v.map(|_| r.min_in_v);
            let need = need.filter(|v| positive && r.adj_return.is_none() && v.is_finite());
            return Some((r.input, need));
        }
        let b = self.behavioral.iter().find(|b| b.reference == part)?;
        Some((b.converter_in_node()?, None))
    }

    /// Look up a net node by name.
    #[must_use]
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
    /// Both shapes are handled: a `SupplyLeg`'s internal sources (its current
    /// fold reads the source's branch, so it opens with it), and a bare
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
            opened |= leg.open(&mut self.circuit, SUPPRESSED_RAIL_OHMS);
        }
        self.supplies.retain(|s| s.net != node);
        // A bare ideal rail source stamped straight onto the net.
        let leg_name = format!("Vsupply_{net}");
        for dev in &mut self.circuit.devices {
            if let Device::Vsource { name, p, .. } = dev {
                if *p == node && (*name == leg_name || name.starts_with("Vrail")) {
                    let (nm, a) = (name.clone(), *p);
                    *dev = Device::resistor(nm, a, NodeId::GROUND, SUPPRESSED_RAIL_OHMS);
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

impl BoundBoard {
    /// [`no_processor_message`] for this board's unfitted and unemulated MCUs.
    #[must_use]
    pub fn no_processor_message(&self, remedy: FitRemedy) -> String {
        no_processor_message(&self.dnp_mcus, &self.unsupported_mcus, remedy)
    }
}

/// Why a run that needs firmware cannot proceed, and what to do about it.
///
/// Firmware on a board with zero processors is unanswerable rather than
/// merely suspicious: nothing executes, so every firmware assertion passes
/// without being tested. Callers pair this with
/// [`EXIT_INVALID_FOR_ANALYSIS`](crate::EXIT_INVALID_FOR_ANALYSIS)
/// (or their spec-error equivalent) so a vacuous green is impossible.
#[must_use]
pub fn no_processor_message(
    dnp_mcus: &[(String, String)],
    unsupported_mcus: &[(String, String)],
    remedy: FitRemedy,
) -> String {
    if dnp_mcus.is_empty() && !unsupported_mcus.is_empty() {
        let named = unsupported_mcus
            .iter()
            .map(|(r, family)| {
                format!("{family} ({r}) is recognised but has no emulator backend yet")
            })
            .collect::<Vec<_>>()
            .join("; ");
        return format!(
            "cannot run firmware: {named}, so this board bound zero processors. \
             Firmware-less analyses (lint, report, DRC) still work; to force a backend, \
             override the part with a --models-dir entry that sets `backend` explicitly."
        );
    }
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
#[must_use]
pub fn bind_board(board: &ExtractedBoard, lib: &ModelLibrary) -> BoundBoard {
    bind_board_with(board, lib, &crate::behavioral::CustomRegistry::new())
}

/// Bind an extracted board, consulting `custom` for escape-hatch Rust behaviours.
///
/// A component whose resolved model id / value / MPN matches a registered factory is realised
/// by that [`CustomBehavior`](crate::behavioral::CustomBehavior) instead of the declarative
/// layer.
#[must_use]
#[allow(clippy::too_many_lines)] // the ordered bind passes share many locals
pub fn bind_board_with(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    custom: &crate::behavioral::CustomRegistry,
) -> BoundBoard {
    let mut circuit = Circuit::new();
    let mut net_nodes: HashMap<String, NodeId> = HashMap::new();
    let mut net_names: Vec<String> = Vec::new();
    let mut report = BindReport {
        board_name: board.name.clone(),
        ..Default::default()
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
    let mut peripherals: Vec<PeripheralBinding> = Vec::new();
    let input_sources: HashMap<String, hauksbee_ir::DeviceId> = HashMap::new();

    // DNP parts the model DB recognises as processors, as (reference, value).
    let mut dnp_mcus: Vec<(String, String)> = Vec::new();

    // ── Pass 2: bind every component ────────────────────────────────────────
    for comp in &board.components {
        // The three-state assembled-component contract, asked once per part:
        // only a Present record reaches model resolution; the two absent
        // states each leave a visible row rather than a silent hole.
        let part = match AssemblyState::of(comp) {
            AssemblyState::Present(part) => part,
            AssemblyState::DnpAbsent(absence) => {
                // A DNP part sits on the layout but is not assembled: it is
                // electrically ABSENT. It must contribute no device and no
                // pin-to-net wiring, a DNP bridge resistor stamped anyway
                // would join two nets that are open on the real board.
                //
                // A DNP processor is the one skip that can hollow out a whole
                // run: with no MCU there is no firmware, so every "the
                // firmware must ..." assertion passes vacuously. Note which
                // ones they were so the report can say so out loud (patched in
                // below, once it is known whether any MCU bound at all). This
                // asks what the absent part WOULD be, which is a library
                // question, not a licence to model it.
                if matches!(
                    library_resolution(lib, comp).model.as_ref().map(|m| m.kind),
                    Some(ComponentKind::Mcu)
                ) {
                    dnp_mcus.push((comp.reference.clone(), comp.value.clone()));
                }
                let reason = absence.describe().to_string();
                report.push(BindRow::without_model(
                    comp.reference.clone(),
                    comp.value.clone(),
                    Confidence::Exact,
                    BindOutcome::Skipped { reason },
                ));
                continue;
            }
            AssemblyState::IdentityUnknown(refusal) => {
                // A refused identity is not a model match: the extractor keeps
                // the record so connectivity/DRC stay available, but binding it
                // would turn an unresolved identity choice into precise
                // simulation. Left open, and said out loud.
                let reason = format!("{}; left open", refusal.reason());
                let warning = format!(
                    "cannot bind '{}' ({}): {reason}",
                    comp.reference, comp.value
                );
                report.push(BindRow {
                    warning: Some(warning),
                    ..BindRow::without_model(
                        comp.reference.clone(),
                        comp.value.clone(),
                        Confidence::Unresolved,
                        BindOutcome::Unresolved { reason },
                    )
                });
                continue;
            }
        };
        let res = resolve(lib, part);
        let model = res.model.clone();
        let conf = res.confidence;
        let source = res.provenance.clone();
        let model_id = model.as_ref().map(|m| m.id.clone());

        let (kind_str, outcome, warning, guesses) = match (&model, &res.selection) {
            (None, Some(s)) => {
                let reason = s.open_reason(&comp.reference);
                let warning = format!("cannot bind '{}' ({}): {reason}", s.reference, s.value);
                let outcome = BindOutcome::Unresolved { reason };
                (None, outcome, Some(warning), Vec::new())
            }
            (None, None) => {
                let (kind_str, outcome, warning) = unresolved_outcome(comp, &node_of, lib);
                (kind_str, outcome, warning, Vec::new())
            }
            (Some(m), _) => {
                let kind_str = format!("{:?}", m.kind).to_ascii_lowercase();
                let (outcome, warning, guesses) = bind_component(
                    comp,
                    m,
                    source.as_ref(),
                    &mut circuit,
                    &node_of,
                    &mut digital,
                    &mut mcus,
                    &mut dacs,
                    &mut peripherals,
                    &mut report.regulators,
                    (&board.components, &power_nets),
                    lib.pin_rules(),
                );
                (Some(kind_str), outcome, warning, guesses)
            }
        };

        let warning = with_tie_warning(comp, &res, warning);
        if let Some(k) = &kind_str {
            component_kinds.insert(comp.reference.clone(), k.clone());
        }
        // A port's numeric part number parses as a value, so it lands here too;
        // nothing is plugged into it in the specified scenario, so it drives nothing.
        let is_port = is_port_class(&ref_prefix(&comp.reference));
        if matches!(outcome, BindOutcome::Unresolved { .. }) && !is_port {
            let can_drive = |kind: &str| {
                let k = kind.to_ascii_lowercase();
                !(k.starts_with("input") || k.starts_with("power_in") || k.contains("no_connect"))
            };
            let pin = |p: &hauksbee_extract::Pin| Some((node_of(p.net)?, can_drive(&p.kind)));
            report.open_parts.push(crate::bind_report::OpenPart {
                reference: comp.reference.clone(),
                value: comp.value.clone(),
                pins: comp.pins.iter().filter_map(pin).collect(),
            });
        }
        if let Some(selection) = res.selection {
            report.selections.insert(report.rows.len(), selection);
        }
        report.push(BindRow {
            reference: comp.reference.clone(),
            value: comp.value.clone(),
            model_id,
            confidence: conf,
            source,
            outcome,
            warning,
            guesses,
        });
    }

    // Board truth for clock-ready emulation. The MCU pin map provides the
    // semantic oscillator roles; the board provides population and
    // connectivity. No reference-name or board-name special case is involved.
    for mcu in &mut mcus {
        let Some(&osc_in) = mcu.role_nets.get("osc_in") else {
            continue;
        };
        let Some(&osc_out) = mcu.role_nets.get("osc_out") else {
            continue;
        };
        mcu.external_clock_present =
            assembled_crystal_bridges(board, lib, osc_in, osc_out, &node_of);
    }

    // ── Assign MCP4728 I2C addresses deterministically by reference order ────
    // A board reprograms its DACs to 0x60/0x61/0x62 during bring-up (the
    // bit-banged address-reprogramming itself is out of scope). Model the
    // post-bring-up state: 0x60+i by ascending reference designator. ASSUMPTION:
    // the netlist does not encode the final address per device, so by-ref
    // ordering is the deterministic stand-in.
    //
    // Sort by a NATURAL key (alpha prefix, then the parsed trailing integer),
    // not raw String Ord: byte-lexicographic ordering puts "U10" before "U2" and
    // would hand out addresses in the wrong order for non-uniform-width
    // designators.
    dacs.sort_by_key(|d| crate::bind_report::natural_ref_key(&d.reference));
    for (d, address) in dacs.iter_mut().zip(0x60u8..) {
        d.address = address;
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
    // A supply net takes an ideal source (default Ideal at the rail's nominal
    // voltage) only when nothing on the board already sets its rail: see
    // `rail_owner`. A regulator's or converter's rail, including nets a series
    // conductor joins to it (a buck's rail past its inductor, a ferrite-fed
    // analog rail), keeps the regulator as its only source; an undriven rail
    // takes one source, on a net a connector feeds when it has one. The supply
    // is a controllable Vsource behind a tiny series resistor, so the
    // scheduler can read rail current and update the supply per chunk.
    let mut domains = crate::rail_owner::RailDomains::new(&circuit, &behavioral, &[]);
    let board_nodes: std::collections::HashSet<NodeId> = net_nodes.values().copied().collect();
    domains.join_switches(&circuit, |n| board_nodes.contains(&n));
    let entry = |node: NodeId| {
        board.components.iter().any(|c| {
            let port = component_kinds
                .get(&c.reference)
                .is_some_and(|k| k == "connector")
                || is_port_class(&ref_prefix(&c.reference));
            port && c.pins.iter().any(|p| node_of(p.net) == Some(node))
        })
    };
    let mut supplies: Vec<SupplyLeg> = Vec::new();
    // Deterministic order so supply indices are stable across runs.
    let mut supply_names: Vec<(&String, f64, NodeId, bool)> = power_nets
        .iter()
        .filter_map(|(name, &volts)| {
            let node = net_nodes.get(name).copied().filter(|n| !n.is_ground())?;
            Some((name, volts, node, entry(node)))
        })
        .collect();
    // A connector's rail first, then the shorter name (+5V before +5V_USB,
    // the rail before its branches), so the source a branch is fed from
    // takes the supply.
    supply_names.sort_by(|a, b| {
        let len = |s: &String| s.len();
        b.3.cmp(&a.3)
            .then_with(|| len(a.0).cmp(&len(b.0)))
            .then_with(|| a.0.cmp(b.0))
    });
    let mut conflicts = domains.conflicts(&circuit);
    for &(name, volts, node, _) in &supply_names {
        if !domains.drivers_of(node).is_empty() {
            continue;
        }
        if let Some(leg) = supplies.iter().find(|l| domains.joined(l.net, node)) {
            let held = leg.supply.nominal_volts();
            if (held - volts).abs() > 0.01 * held.abs().max(volts.abs()) {
                conflicts.push(crate::rail_owner::RailConflict {
                    parts: Vec::new(),
                    nets: vec![leg.net_name.clone(), name.clone()],
                    message: format!(
                        "'{name}' ({volts:.2} V by name) and '{}' ({held:.2} V) are one rail \
                         through a series conductor; only '{}' takes a supply, so '{name}' \
                         reads {held:.2} V",
                        leg.net_name, leg.net_name
                    ),
                });
            }
            continue;
        }
        // A switched branch of a driven rail, or of a rail already supplied at
        // this voltage, is fed through its switch.
        let same = |l: &SupplyLeg| {
            let held = l.supply.nominal_volts();
            (held - volts).abs() <= 0.01 * held.abs().max(volts.abs())
        };
        let switched = |l: &&SupplyLeg| domains.fed_joined(l.net, node) && same(l);
        if domains.fed_by_driver(node) || supplies.iter().any(|l| switched(&l)) {
            continue;
        }
        let leg = SupplyLeg::stamp(&mut circuit, node, name, PowerSupply::Ideal { volts });
        supplies.push(leg);
        report.push(BindRow::without_model(
            format!("RAIL:{name}"),
            format!("{volts:.2}V"),
            Confidence::Exact,
            BindOutcome::PowerRail { volts },
        ));
    }
    // Said on a row too, so every surface that lists bind warnings shows it.
    for c in &conflicts {
        let mut row = BindRow::without_model(
            format!("RAIL:{}", c.nets.join("/")),
            String::new(),
            Confidence::Exact,
            BindOutcome::Skipped {
                reason: "rail driven by more than one source".to_string(),
            },
        );
        row.warning = Some(c.message.clone());
        report.push(row);
    }
    report.rail_conflicts = conflicts;

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
        for row in &mut report.rows {
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
    let device_meta = gather_device_meta(board, lib, &circuit, &node_of, &mcus, &digital, &dacs);

    // Computed before the move: see `BoundBoard::latching_switches`.
    let latching_switches = component_kinds.values().any(|k| k == "timer");
    let unsupported_mcus = board
        .components
        .iter()
        .filter(|c| AssemblyState::of(c).fitted().is_some())
        .filter(|c| !mcus.iter().any(|m| m.reference == c.reference))
        .filter_map(|c| match route_mcu_family(c) {
            Some(McuFamilyRoute::NoPlatform { family }) => {
                Some((c.reference.clone(), family.to_string()))
            }
            _ => None,
        })
        .collect();

    BoundBoard {
        name: board.name.clone(),
        circuit,
        net_nodes,
        net_names,
        digital,
        mcus,
        dnp_mcus,
        unsupported_mcus,
        component_kinds,
        input_sources,
        supplies,
        behavioral,
        device_meta,
        dacs,
        peripherals,
        latching_switches,
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
    let mut resistor_ohms: HashMap<String, Option<f64>> = HashMap::new();
    for comp in &board.components {
        let state = AssemblyState::of(comp);
        // A DNP resistor is absent from the assembled board. Named converter
        // programs become unsupported; generic `_from_ref` params retain their
        // explicit open-resistance semantics below. An identity-refused
        // record stays in the map with no observed value, so contradictory
        // identity evidence can never program a limit.
        if matches!(state, AssemblyState::DnpAbsent(_)) {
            continue;
        }
        if comp.reference.starts_with('R') {
            let observed = parse_value(&comp.value)
                .filter(|_| state.is_present())
                .map(|parsed| parsed.si)
                .filter(|ohms| ohms.is_finite() && *ohms > 0.0);
            // A second populated physical record with the same designator is
            // contradictory identity evidence. Never make iteration order a
            // circuit fact, even if the two values happen to match.
            resistor_ohms
                .entry(comp.reference.clone())
                .and_modify(|value| *value = None)
                .or_insert(observed);
        }
    }
    let board_resistor =
        |refdes: &str| -> Option<f64> { resistor_ohms.get(refdes).copied().flatten() };

    let mut out = Vec::new();
    for comp in &board.components {
        // Not assembled, or identity refused -> no behavioural device (same
        // three-state rule as pass 2). Custom factories may match raw value
        // strings even when normal model resolution refuses the component, so
        // they sit behind the same witness as declarative binding.
        let Some(part) = AssemblyState::of(comp).fitted() else {
            continue;
        };
        let res = resolve(lib, part);
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
        let model_pins = model.as_ref().map_or(&empty_pins, |m| &m.pins);
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
#[allow(clippy::too_many_lines)] // one pass per meta source, in order
fn gather_device_meta(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    circuit: &Circuit,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
    mcus: &[McuBinding],
    digital: &[crate::digital::DigitalComponent],
    dacs: &[DacBinding],
) -> Vec<DeviceMeta> {
    // Index components by reference, with their resolved kind/ratings/footprint.
    struct CompInfo {
        kind: ComponentKind,
        ratings: hauksbee_models::schema::Ratings,
        derating: hauksbee_models::derating::Derating,
        footprint: String,
        /// Role nodes, for a card rating current or power per path.
        roles: HashMap<String, NodeId>,
    }
    let mut by_ref: HashMap<String, CompInfo> = HashMap::new();
    for comp in &board.components {
        // Only a present part's ratings may describe a monitored device: a DNP
        // or identity-refused record sharing the reference designator must not
        // supply the ratings for a device another record stamped.
        let Some(part) = AssemblyState::of(comp).fitted() else {
            continue;
        };
        let res = resolve(lib, part);
        if let Some(m) = res.model {
            let mounting = (comp.properties.iter().rev())
                .find(|(k, _)| k == hauksbee_models::derating::MOUNTING_PROPERTY)
                .map(|(_, v)| v.as_str());
            let derating = m
                .derating
                .fitted(&comp.reference, &comp.footprint, mounting);
            let roles = role_node_map(comp, &m, node_of);
            by_ref.insert(
                comp.reference.clone(),
                CompInfo {
                    kind: m.kind,
                    ratings: m.ratings.clone(),
                    derating,
                    footprint: comp.footprint.clone(),
                    roles,
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
                    derating: info.derating.clone(),
                });
            }
        }
    }

    // Vreg output sources: name is "Vreg_<ref>". A vreg whose model carries a
    // behavioural converter stamps "Vbeh_<ref>_conv" instead of the ideal
    // source; that Vsource is the same monitoring seam (its branch current is
    // the delivered output current), so it takes the same meta, otherwise a
    // converter-modelled regulator silently loses its stress/max_temp watch.
    // An ADJUSTABLE regulator stamps "Vref_<ref>" (the VREF source between OUT
    // and ADJ, see `bind_adj_vreg`) and nothing else touches its OUT net, so
    // that branch unknown IS the total output current, divider included, which
    // is the quantity both `max_current_a` and `min_current_a` are specified
    // against. Unmatched here it carried no meta at all, so an adjustable part
    // was exempt from BOTH checks: a divider-only LM317 read cleanly regulated.
    for (id, dev) in circuit.iter() {
        if let Device::Vsource { name, .. } = dev {
            let reference = name
                .strip_prefix("Vreg_")
                .or_else(|| name.strip_prefix("Vref_"))
                .or_else(|| {
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
                            derating: info.derating.clone(),
                        });
                    }
                }
            }
        }
    }

    // A card rating current or power per path: which of the part's devices
    // carry each path, so the monitor judges the path on their current.
    let part = |m: &DeviceMeta| crate::stress::strip_unit_suffix(&m.reference).to_owned();
    for i in 0..metas.len() {
        let (r, units) = (part(&metas[i]), metas.iter().map(|m| (part(m), m.device)));
        let units: Vec<_> = units.filter(|u| u.0 == r).map(|u| u.1).collect();
        if let Some(c) = by_ref.get(&r) {
            let paths = crate::stress::rated_paths(circuit, &r, &c.derating, &c.roles, &units);
            metas[i].derating.carriers = paths;
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
                    derating: info.derating.clone(),
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
        for (letter, drv) in (b'a'..).zip(&d.vout_drivers) {
            if let Some(drv) = drv {
                let pin = format!("vout_{}", char::from(letter));
                push_pin(&mut metas, &d.reference, pin, drv.vsource);
            }
        }
    }

    metas
}

/// Resolve one assembled component into a model entry.
///
/// This is the only door to a component's electrical model: it takes the
/// [`FittedComponent`] witness, which only [`AssemblyState::of`] hands out, so
/// a caller cannot reach a model without having answered the three-state
/// assembled-component question. DNP-absent and identity-unknown parts have no
/// witness and therefore no model.
#[must_use]
pub fn resolve(lib: &ModelLibrary, part: FittedComponent<'_>) -> hauksbee_models::Resolution {
    library_resolution(lib, part.component())
}

/// What the model bound to `reference` says about that part's chip-select.
///
/// This is what promotes SPI transaction framing from the chunk-boundary
/// heuristic to exact without anyone hand-writing `cs_net` in the run spec: the
/// model DB already maps pads to binder roles, and `cs` is already one of those
/// roles, so a modeled SPI slave knows which of its own pads is chip-select.
///
/// `None` means the reference produced no usable model at all, for one of the
/// reasons the rest of the tree already refuses on:
///
/// - exactly ONE record must exist under `reference`. Two records sharing a
///   designator that the extractor did NOT flag as a conflict would otherwise be
///   resolved by iteration order, so whichever happened to come first would
///   decide which edge stream the co-sim called exact;
/// - [`AssemblyState::of`] must classify it `Present`, so a DNP-absent part (not
///   on the assembled board, its CS net electrically meaningless) and an
///   identity-refused part (nothing about the record is evidence, including
///   which pad is CS) both contribute nothing;
/// - it must bind to a model, through the [`FittedComponent`]-gated `resolve`
///   and no other door.
///
/// `Some` always carries the bound model's id, and carries a `cs_net` only when
/// that model maps a `cs` role landing on exactly one real, non-ground net
/// (`crate::component_evidence::role_net` refuses a role split across two nets
/// rather than picking one).
///
/// The id comes back EVEN WHEN there is no `cs_net`, and that separation is
/// load-bearing. A caller that knows which part it expected (the CI runner knows
/// the peripheral kind) has to be able to catch a `ref` pointing at the wrong
/// device whether or not that device happens to declare a chip-select; folding
/// the two together would silently accept the contradiction in exactly the cases
/// where no CS was found. This function reports what answered; the layer that
/// knows what was asked decides whether the answer is admissible.
pub struct ModelRoleCs {
    /// Id of the model the referenced component bound to.
    pub model_id: String,
    /// Whether that model came from the embedded `db/*.toml` database, as opposed
    /// to a user pack or model directory.
    ///
    /// A caller judging "is this the part I expected" can be strict about a
    /// built-in (the shipped DB's ids are knowable, so a mismatch is a real
    /// contradiction) and permissive about anything else (a user pack may
    /// legitimately model a SPI slave under an id no code here can predict).
    pub from_builtin_db: bool,
    /// The board net on that model's `cs` pin role, when it declares a usable one.
    pub cs_net: Option<String>,
}

/// See [`ModelRoleCs`].
#[must_use]
pub fn model_role_cs(
    board: &ExtractedBoard,
    reference: &str,
    lib: &ModelLibrary,
) -> Option<ModelRoleCs> {
    // Exactly one match, never the first of several: `find` would let iteration
    // order pick the record, and two same-designator records can carry different
    // CS nets. Ambiguity here has to be refused, not resolved by luck.
    let mut matching = board.components.iter().filter(|c| c.reference == reference);
    let comp = matching.next()?;
    if matching.next().is_some() {
        return None;
    }
    let part = AssemblyState::of(comp).fitted()?;
    let resolution = resolve(lib, part);
    let from_builtin_db = resolution.layer == Some(hauksbee_models::SourceLayer::Builtin);
    let model = resolution.model?;
    let cs_net = model_cs_net(board, part, &model);
    Some(ModelRoleCs {
        model_id: model.id,
        from_builtin_db,
        cs_net,
    })
}

/// The board net on a bound model's `cs` pin role, or `None` when it declares
/// none or the one it declares cannot frame anything.
fn model_cs_net(
    board: &ExtractedBoard,
    part: FittedComponent<'_>,
    model: &hauksbee_models::ModelEntry,
) -> Option<String> {
    let net_id = crate::component_evidence::role_net(part, model, "cs").ok()?;
    // Net id 0 is the extractor's "no net" sentinel, not a real node. A pad on it
    // is unconnected, so there is no chip-select to frame from.
    if net_id == 0 {
        return None;
    }
    // A chip-select strapped to ground is a permanently-selected slave, not a
    // framing signal: installing a CS frame on it would report exact framing off
    // a net that never edges.
    let net = board.net(net_id)?;
    if is_ground(&net.name) {
        return None;
    }
    Some(net.name.clone())
}

/// What the model library would say about one component RECORD, assembled or
/// not.
///
/// This is the identity-machinery and diagnostics view, deliberately narrow:
/// [`apply_identity`] compares layout records against BOM/placement probes
/// before any assembly state is meaningful (a DNP part's identity is still
/// improvable), the DNP-processor warning asks what an absent part would have
/// been, and the `models resolve` pack-author table reports what each record
/// WOULD resolve to. None of them may stamp a device from the answer.
/// Everything that models an assembled part goes through [`resolve`] and its
/// [`FittedComponent`] witness.
///
/// # Panics
///
/// Never in practice: the engine-fallback provenance is built from static,
/// known-valid values and only `expect`s that they stay valid.
#[must_use]
pub fn library_resolution(lib: &ModelLibrary, comp: &Component) -> hauksbee_models::Resolution {
    let mut q = ComponentQuery::new(
        non_empty(&comp.lib_id),
        non_empty(&comp.value),
        non_empty(&comp.footprint),
    );
    q.reference = Some(comp.reference.clone());
    q.properties.clone_from(&comp.properties);
    let pads = comp.pins.iter().map(|pin| pin.number.trim().to_string());
    q.pads = pads.filter(|number| !number.is_empty()).collect();
    // A refused identity is not a model match even here: the record cannot say
    // WHICH part it is, so no identity string in it is evidence. The extractor
    // keeps the record so connectivity/DRC remain available.
    if matches!(AssemblyState::of(comp), AssemblyState::IdentityUnknown(_)) {
        return hauksbee_models::Resolution::unresolved(q);
    }
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
    let mut res = lib.resolve(&q);
    // `led_generic` is a two-pad guess from the footprint: WS2812B/RGB parts are not it.
    if res.model.as_ref().is_some_and(|m| m.id == "led_generic")
        && logical_passive_pins(comp).map_or(0, |pins| pins.len()) > 2
    {
        res = hauksbee_models::Resolution::unresolved(q.clone());
    }
    // A colour-only value ("RED") is no part number: it takes that colour's LED card.
    let named =
        res.model.as_ref().is_some_and(|m| m.id != "led_generic") || res.selection.is_some();
    if let Some(card) = led_colour(comp).filter(|_| !named) {
        let mut by_colour = q.clone();
        (by_colour.value, by_colour.mpn) = (Some(format!("{card} LED")), None);
        let mut coloured = lib.resolve(&by_colour);
        if let Some(model) = coloured.model.as_mut() {
            let note = format!("value '{}' names only a colour: assumed a typical {card} LED, forward voltage included ({})", comp.value, model.id);
            model.params.set_str("warning", note);
            (coloured.confidence, coloured.query) = (Confidence::Guessed, q);
            return coloured;
        }
    }
    // An open variant set is the library's answer too: no fallback may guess past it.
    if res.confidence != Confidence::Unresolved || res.selection.is_some() {
        return res;
    }
    // ── Engine-layer fallbacks ──────────────────────────────────────────────
    // The model DB matches passives by `Device:R` lib_id or a THT footprint
    // prefix; layout-only extraction yields neither for SMD parts. But the
    // architecture says passives are "resolved purely from their Value field".
    // So when the library is stumped, recover R/C/L from the ref prefix +
    // a parseable value, and recover common bare-AVR MCUs by value prefix.
    if let Some(entry) = fallback_entry(comp) {
        let model_id = entry.id.clone();
        return hauksbee_models::Resolution {
            model: Some(entry),
            confidence: Confidence::Guessed,
            source: Some("engine-fallback".to_string()),
            origin: Some("engine-fallback".to_string()),
            provenance: Some(
                hauksbee_ir::evidence::ModelSource::new(
                    hauksbee_ir::evidence::ModelSourceTier::EstimatedFallback,
                    hauksbee_ir::evidence::ModelLayer::EngineFallback,
                    "engine-fallback",
                    hauksbee_ir::evidence::ModelValidation::Unvalidated,
                    vec![hauksbee_ir::evidence::ModelUncertainty::unknown(
                        format!("{model_id}.model"),
                        "the engine fallback is estimated and has no validated numeric error interval",
                    )
                    .expect("static fallback uncertainty is valid")],
                )
                .expect("static fallback provenance is valid"),
            ),
            ..hauksbee_models::Resolution::unresolved(q)
        };
    }
    res
}

/// The colour a bare-colour value ("RED", "MF-LED-0603-RED") names on a plain two-pad LED
/// (a `D*`/`LED*` designator with an LED symbol or footprint), and the card word it binds
/// to: orange and amber read as yellow, white as blue.
fn led_colour(comp: &Component) -> Option<&'static str> {
    let led = [&comp.lib_id, &comp.footprint]
        .iter()
        .any(|id| id.to_ascii_uppercase().contains("LED"));
    let pads = logical_passive_pins(comp).map_or(0, |pins| pins.len());
    if !matches!(ref_prefix(&comp.reference).as_str(), "D" | "LED") || !led || pads > 2 {
        return None;
    }
    let value = comp.value.to_ascii_uppercase();
    let mut named = value
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter_map(|t| match t {
            "RED" => Some("RED"),
            "GREEN" => Some("GREEN"),
            "BLUE" | "WHITE" => Some("BLUE"),
            "YELLOW" | "ORANGE" | "AMBER" => Some("YELLOW"),
            _ => None,
        });
    named.next().filter(|_| named.next().is_none())
}

/// The upper-cased alphabetic designator prefix of a reference ("JP3" -> "JP").
fn ref_prefix(reference: &str) -> String {
    let letters = reference.chars().take_while(char::is_ascii_alphabetic);
    letters.collect::<String>().to_ascii_uppercase()
}

/// Which passive ('R', 'C' or 'L') a designator's first letter claims. A word
/// rather than a designator ("COIL", "LOAD") yields to a `KiCad` footprint family:
/// a relay winding on `Resistor_SMD:R_0805` is 167 ohms, not a 167 F capacitor.
fn passive_letter(comp: &Component) -> Option<char> {
    let prefix = ref_prefix(&comp.reference);
    let name = comp.footprint.rsplit(':').next().unwrap_or("");
    let family = [("R_", 'R'), ("C_", 'C'), ("CP_", 'C'), ("L_", 'L')];
    let by_footprint = family
        .iter()
        .find(|(f, _)| prefix.len() > 2 && name.starts_with(f));
    let by_ref = prefix
        .chars()
        .next()
        .filter(|c| matches!(c, 'R' | 'C' | 'L'));
    by_ref.map(|c| by_footprint.map_or(c, |(_, letter)| *letter))
}

/// Connector-class prefixes: ports and mechanical parts, never simulated drivers.
fn is_port_class(prefix: &str) -> bool {
    matches!(prefix, "J" | "P" | "X" | "JP" | "TP" | "MP" | "H")
}

/// Synthesize a model entry for a component the library could not resolve,
/// using only the reference-designator class and a parseable value. Returns
/// `None` when no confident fallback applies.
#[allow(clippy::too_many_lines)] // an ordered heuristic ladder; order is the logic
fn fallback_entry(comp: &Component) -> Option<ModelEntry> {
    use std::collections::BTreeMap;
    let prefix = ref_prefix(&comp.reference);

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

    if let Some(McuFamilyRoute::Backend { family, backend }) = route_mcu_family(comp) {
        let mut params = hauksbee_models::Params::default();
        params.set_str("backend", backend);
        params.set_str("auto_bind", "family_router");
        params.set_str("auto_bind_family", family);
        let derived = derive_mcu_pin_roles(comp);
        let int = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
        params.set_int("auto_bind_pin_names", int(derived.named_pin_count));
        params.set_int("auto_bind_derived_pins", int(derived.roles.len()));
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

    // Connector-class references: not simulatable parts; classify so they
    // don't count against resolution (J5 "Power", P1, test points, jumpers).
    if is_port_class(&prefix) && parse_value(&comp.value).is_none() {
        return Some(make_entry(
            "connector_fallback",
            ComponentKind::Ignore,
            "connector / mechanical (engine fallback by reference class)",
            hauksbee_models::Params::default(),
            std::collections::BTreeMap::new(),
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
            hauksbee_models::Params::default(),
            BTreeMap::new(),
        ));
    }

    // Preserve generic two-terminal junctions, but never infer a body from its
    // library (e.g. RPI_COMPILED:SOIC-8) or an embedded word (LED_DRIVER).
    // A diode body is not proof of a 1N4148: this remains an unvalidated guess.
    let diode_prefix = (prefix.starts_with('D') && prefix != "DAC")
        || matches!(prefix.as_str(), "CR" | "VD" | "ZD" | "VR");
    let fp = comp
        .footprint
        .rsplit(':')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    let body = fp
        .strip_prefix("D_")
        .or_else(|| fp.strip_prefix("DIODE_"))
        .unwrap_or(&fp);
    // A custom LED missed by the model DB must not become a silicon signal diode.
    if body.starts_with("LED") || body.split('_').any(|token| token == "DRIVER") {
        return None;
    }
    let package = body.split('_').next().unwrap_or("");
    let fp_is_diode = matches!(package, "SMA" | "SMB" | "SMC" | "MELF" | "MINIMELF")
        || ["SOD", "DO"].iter().any(|family| {
            package.strip_prefix(family).is_some_and(|suffix| {
                let code = suffix.strip_prefix('-').unwrap_or(suffix);
                let suffix = code.trim_start_matches(|c: char| c.is_ascii_digit());
                code.len() > suffix.len() && matches!(suffix, "" | "F" | "FL" | "W")
            })
        });
    // Voltage/current ratings (e.g. a zener's 5.1V) are not passive magnitudes.
    let val_is_passive_magnitude =
        parse_value(&comp.value).is_some_and(|p| !matches!(p.unit.as_deref(), Some("V" | "A")));
    let ref_is_passive = matches!(prefix.chars().next(), Some('R' | 'C' | 'L'));
    if diode_prefix || fp_is_diode {
        let v = comp.value.trim();
        let val_is_generic = ["", "D", "diode", "1N4148", "1N4148W", "1N4148WS"]
            .iter()
            .any(|name| v.eq_ignore_ascii_case(name));
        // Bind the fallback when the value is a generic diode token, OR the
        // footprint is a diode body and the value is not a passive magnitude.
        if val_is_generic || (fp_is_diode && !val_is_passive_magnitude) {
            // Repeated physical pads may share an electrical pin; arrays may not.
            let pins: std::collections::BTreeSet<_> = comp
                .pins
                .iter()
                .map(|pin| pin.number.as_str())
                .filter(|pin| !pin.is_empty())
                .collect();
            if pins.len() > 2 {
                return None;
            }
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

    // CR1 must not become a capacitor; genuine R/C/L magnitudes on a shared
    // package (e.g. R_MELF 10k) may still reach the passive fallback.
    if diode_prefix || (fp_is_diode && !(ref_is_passive && val_is_passive_magnitude)) {
        return None;
    }

    // Passives by reference class, when a magnitude can be recovered from the
    // value field. Handles plain engineering values ("4k7", "100n") and the
    // structured naming some teams use ("R_47k_0402", "C_22u_25V_0805",
    // "CTEB_2.2UF_35V_10%_...): the first underscore-separated token that
    // parses as a value wins.
    let kind_pins: Option<(ComponentKind, [(&str, &str); 2])> = match passive_letter(comp) {
        Some('C') => Some((ComponentKind::Passive, [("1", "pos"), ("2", "neg")])),
        Some('R' | 'L') => Some((ComponentKind::Passive, [("1", "a"), ("2", "b")])),
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
/// prefix, and a trailing "Hz" ("16MHz", "8 Mhz", "32.768kHz").
///
/// The whole-value test matters: a ferrite bead is conventionally valued as
/// impedance@frequency ("600@100MHz"), which also ends in "hz", but a bead sits
/// in *series* in a power/signal path, so binding it Ignore would open that path
/// and re-create the exact solve-collapse this crystal handling exists to
/// prevent. The shared grammar rejects it because the '@' is not part of a
/// frequency, while accepting real crystal values.
///
/// This defers to the value parser rather than doing its own suffix arithmetic:
/// a hand-rolled `strip_suffix("hz")` here and the parser's own unit table would
/// be two answers to one question, free to drift apart.
fn value_is_frequency(value: &str) -> bool {
    hauksbee_models::value::parse_frequency_hz(value).is_some()
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
/// none, the family router decides from the part's identity strings: a DB entry
/// that exists only for strap-lint data (esp32s3, esp32s2) must not silently
/// inherit the AVR default, which would send an ESP32-S3 into simavr (wrong ISA,
/// and the GPL-gated `avr` feature the GPL-free build excludes) instead of
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

/// (name prefixes, family, backend); `None` is a family with no co-sim
/// platform. Order is load-bearing: the F072 descriptor models only that
/// silicon, so an F030/F091 stays unresolved rather than running on it; and the
/// RISC-V ESP32 variants (C6/C2/H2, P4) must be caught before the generic
/// "ESP32" catch-all, which would run their firmware on the Xtensa core.
const MCU_ROUTES: &[(&[&str], &str, Option<&str>)] = &[
    (&["STM32F072"], "STM32F072", Some("renode:stm32f072")),
    (&["STM32F0"], "STM32F0", None),
    (&["STM32F4"], "STM32F4", Some("renode:stm32f4")),
    (&["STM32F1"], "STM32F1", Some("renode:stm32f103")),
    // Recognised STM32 lines with no emulator platform yet: named so a refusal
    // can say which family it was rather than "bound zero processors".
    (&["STM32F2"], "STM32F2", None),
    (&["STM32F3"], "STM32F3", None),
    (&["STM32F7"], "STM32F7", None),
    (&["STM32G0"], "STM32G0", None),
    (&["STM32G4"], "STM32G4", None),
    (&["STM32H7"], "STM32H7", None),
    (&["STM32L0"], "STM32L0", None),
    (&["STM32L1"], "STM32L1", None),
    (&["STM32L4"], "STM32L4", None),
    (&["STM32L5"], "STM32L5", None),
    (&["STM32U5"], "STM32U5", None),
    (&["STM32WB"], "STM32WB", None),
    (&["STM32WL"], "STM32WL", None),
    (&["STM32C0"], "STM32C0", None),
    (&["CY7C6801", "CY7C6811"], "Cypress FX2 (8051)", None),
    (&["ESP32C3"], "ESP32-C3", Some("qemu:esp32c3")),
    (&["ESP32S3"], "ESP32-S3", Some("qemu:esp32s3")),
    (&["ESP32S2"], "ESP32-S2", None),
    (&["ESP32C6"], "ESP32-C6", None),
    (&["ESP32C2"], "ESP32-C2", None),
    (&["ESP32H2"], "ESP32-H2", None),
    (&["ESP32P4"], "ESP32-P4", None),
    (
        &["ESP32", "ESPWROOM32", "ESP32WROOM32", "ESP32WROVER"],
        "ESP32",
        Some("qemu:esp32"),
    ),
    (&["NRF52"], "nRF52", Some("renode:nrf52840")),
];

fn route_mcu_family_str(s: &str) -> Option<McuFamilyRoute> {
    family_tokens(s).into_iter().find_map(|token| {
        let compact = token.replace(['-', '_', ' '], "");
        let &(_, family, backend) = MCU_ROUTES
            .iter()
            .find(|(prefixes, ..)| prefixes.iter().any(|p| compact.starts_with(p)))?;
        Some(
            backend.map_or(McuFamilyRoute::NoPlatform { family }, |backend| {
                McuFamilyRoute::Backend { family, backend }
            }),
        )
    })
}

fn looks_like_mcu_candidate(comp: &Component) -> bool {
    let prefix = ref_prefix(&comp.reference);
    if matches!(prefix.as_str(), "U" | "IC" | "MCU") {
        return true;
    }
    // `X` is a module/socket designator as often as a connector one (Eagle's
    // microbuilder library places WROOM modules as X2). It is a candidate only
    // when its own value or footprint names an MCU family; a plain header stays
    // a connector.
    if prefix == "X" {
        return [&comp.value, &comp.footprint]
            .iter()
            .any(|s| route_mcu_family_str(s).is_some());
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
    let bit_of = |digits: &str, max: u8| {
        let ok = (1..=2).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit());
        ok.then(|| digits.parse::<u8>().ok())
            .flatten()
            .filter(|&n| n < max)
    };
    // Nordic "P0.13" / "P1.15-LF": numeric port, dotted bit -> "p013" / "p115".
    if let Some((port, rest)) = token.strip_prefix('P').and_then(|r| r.split_once('.')) {
        if matches!(port, "0" | "1") {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Some(bit) = bit_of(&digits, 32) {
                return Some(format!("p{port}{bit:02}"));
            }
        }
    }
    // Espressif module pin names spell GPIOn as "IOn" ("IO36/SVP").
    if let Some(bit) = token.strip_prefix("IO").and_then(|d| bit_of(d, 64)) {
        return Some(format!("gpio{bit}"));
    }
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
        let digits: String = chars.take_while(char::is_ascii_digit).collect();
        if digits.is_empty() || digits.len() > 2 {
            return None;
        }
        if digits.parse::<u8>().ok()? < 32 {
            return Some(format!("p{}{}", port.to_ascii_lowercase(), digits));
        }
    }
    if let Some(rest) = token.strip_prefix("GPIO") {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
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
    // Every other field defaults, so a new optional field needs no edit here.
    ModelEntry {
        id: id.to_string(),
        kind,
        description: description.to_string(),
        params,
        pins,
        ..Default::default()
    }
}

/// The `ATmega328P` DIP-28 / TQFP-32 pad→role map (mirrors db/mcu.toml).
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
    (!s.trim().is_empty()).then(|| s.to_string())
}

/// Collapse a multi-line TOML string into one report sentence: the abstention
/// table's `because`/`unlocked_by` are written as wrapped block strings so they are
/// readable in the file, and a report line wants them unwrapped.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Build the outcome for a PRESENT component with no resolved model. The two
/// absent assembly states never reach here: pass 2 rows them before resolving.
fn unresolved_outcome(
    comp: &Component,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
    lib: &ModelLibrary,
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
    let warning = connected.then(|| {
        format!(
            "unresolved part '{}' ({}) on connected net(s): defaulting to OPEN circuit",
            comp.reference, comp.value
        )
    });
    // A valueless Altium part carries the extractor's explanation as a
    // component property; surface it as the row's reason so the table says
    // WHY there is nothing to resolve, not a bare "no model".
    let value_unresolved = comp
        .properties
        .iter()
        .find(|(k, _)| k == hauksbee_extract::altium::VALUE_UNRESOLVED_KEY)
        .map(|(_, v)| v.clone());
    // A NAMED ABSTENTION beats the generic sentence. `db/unmodelled.toml` carries
    // the parts this library has looked at and cannot model honestly, each with the
    // input that would unlock it, and the report prints the binder's `reason` as the
    // disclosure's "because". So a part on that list says WHY it is open and what
    // would close it, where an unexamined part says "no model" and means it.
    //
    // The table is consulted only HERE, after resolution has already failed, and it
    // contributes only this string: it cannot bind anything, and a stale entry for a
    // part that has since gained a real model is unreachable rather than wrong.
    let mpn = comp
        .properties
        .iter()
        .find(|(k, _)| k.to_ascii_lowercase().contains("mpn"))
        .map_or("", |(_, v)| v.as_str());
    let abstention = lib.unmodelled().note_for(&comp.value, mpn);
    let reason = match (value_unresolved, abstention) {
        // The extractor's own explanation for a valueless part wins: it says the
        // value is missing, which is a different and more immediate fact than any
        // statement about a named part.
        (Some(why), _) => why,
        (None, Some(note)) => format!(
            "{}{}{}",
            one_line(&note.because),
            hauksbee_ir::evidence::Assumption::UNLOCKED_BY_MARKER,
            one_line(&note.unlocked_by)
        ),
        (None, None) if two_terminal => "no model; left open".to_string(),
        (None, None) => "no model".to_string(),
    };
    (None, BindOutcome::Unresolved { reason }, warning)
}

/// Bind a single resolved component. Mutates the circuit and the digital/mcu
/// collections; returns its outcome and any warning.
#[allow(clippy::too_many_lines)] // the per-kind dispatch table
fn bind_component(
    comp: &Component,
    model: &ModelEntry,
    source: Option<&ModelSource>,
    circuit: &mut Circuit,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
    digital: &mut Vec<DigitalComponent>,
    mcus: &mut Vec<McuBinding>,
    dacs: &mut Vec<DacBinding>,
    peripherals: &mut Vec<PeripheralBinding>,
    regulators: &mut Vec<SimpleRegulator>,
    board: (&[Component], &HashMap<String, f64>),
    pin_rules: &hauksbee_models::PinRuleTable,
) -> (BindOutcome, Option<String>, Vec<String>) {
    use ComponentKind::{
        Adc, AnalogSwitch, BjtNpn, BjtPnp, Comparator, Connector, Dac, Digital, Diode, Ignore, Mcu,
        Nmos, Opamp, Passive, Pmos, ShiftRegister, Timer, Vreg,
    };
    let (components, power_nets) = board;

    // A source-bound identity is not executable behavior. Older identity cards
    // were represented as empty `digital` models; the digital binder accepted
    // them, so `critical_parts_bound` increased even though every functional
    // pin still floated. Preserve the exact winning model/source on the report
    // row, but leave the circuit OPEN and state the missing primitive.
    if model.params.get_bool("identity_only").unwrap_or(false)
        || (model.kind == Digital && !hauksbee_models::validation::digital_has_behavior(model))
    {
        let because = model
            .params
            .get_str("warning")
            .unwrap_or("component identity is known, but device behavior is not modeled");
        let unlocked_by = model
            .params
            .get_str("unlocked_by")
            .unwrap_or("a validated behavioral or solver model for this exact device");
        let connected = comp.pins.iter().any(|pin| node_of(pin.net).is_some());
        let warning = connected.then(|| {
            format!(
                "{} ({}): {because}; identity matched model '{}', but the part remains OPEN",
                comp.reference, comp.value, model.id
            )
        });
        return (
            BindOutcome::Unresolved {
                reason: format!(
                    "identity matched model '{}' but behavior is unavailable: {}{}{}",
                    model.id,
                    one_line(because),
                    Assumption::UNLOCKED_BY_MARKER,
                    one_line(unlocked_by)
                ),
            },
            warning,
            Vec::new(),
        );
    }

    // role -> node for this component's connected pins. Explicit pin-functions
    // and the model's pad map come first; for any role-dependent pad still
    // without a role the pin-rule table is consulted, and each such inference is
    // recorded as a guess-warning (so nothing is silently guessed).
    let (role_nets, guesses, clash) = role_node_map_guessed(comp, model, node_of, pin_rules);
    if let Some(reason) = clash.or_else(|| split_role(comp, model, node_of)) {
        let warning = format!("{}: {reason}; the part is left OPEN", comp.reference);
        return (BindOutcome::Unresolved { reason }, Some(warning), guesses);
    }

    // A model-declared firmware peripheral is executable behavior in its own
    // right, but only when the board actually wires the bus roles.
    if let Some(spec) = model.peripheral.clone() {
        return match bind_model_peripheral(
            comp, model, spec, &role_nets, node_of, power_nets, circuit,
        ) {
            Ok(binding) => {
                let assumed = binding.i2c_address_assumed.as_ref().map(|why| {
                    format!(
                        "{}{} ({}): {why}",
                        Assumption::PARTIAL_MODEL_MARKER,
                        comp.reference,
                        comp.value
                    )
                });
                let warning = merge_bind_warnings(entry_warning(comp, model), assumed);
                let device = match &binding.spec {
                    PeripheralSpec::I2cEeprom { .. } => "model-declared I2C EEPROM",
                    PeripheralSpec::SpiNorFlash { .. } => "model-declared SPI NOR flash",
                    PeripheralSpec::RegisterMap { .. } => "model-declared register-map peripheral",
                };
                peripherals.push(binding);
                (
                    BindOutcome::Behavioral {
                        device: device.to_string(),
                    },
                    warning,
                    guesses,
                )
            }
            Err((reason, warning)) => (BindOutcome::Unresolved { reason }, Some(warning), guesses),
        };
    }
    // pad number -> node, regardless of role.
    let pad_nodes = |pad: &str| -> Option<NodeId> {
        comp.pins
            .iter()
            .find(|p| p.number == pad)
            .and_then(|p| node_of(p.net))
    };

    // A crystal, resonator or packaged oscillator is `kind = "passive"` in the
    // DB because that is the shape of its pad map, but it is NOT a two-terminal
    // R/C/L: its value field is a FREQUENCY or a part number, never a magnitude.
    // Sending it through `bind_passive` asks the value parser for ohms and gets
    // `None` for "16Mhz" or "Abracon_ABM11" (reported as an unresolved open) or,
    // worse, a NUMBER for a bare "32.768", stamping a 32.768 Ω resistor across
    // an oscillator that has no such conductance. Bind it high-impedance
    // instead, with the clock supplied by the MCU model; the two load caps
    // beside it are genuine passives and are unaffected.
    //
    // `BindOutcome::Skipped` is `is_ignored()`, so the row leaves
    // `non_ignored()`: it drops out of the resolved/total denominator, and
    // `BoardEvidence::from_bound` builds an `open_part` assumption only for
    // `Unresolved`. Skipping a part therefore makes the board read as MORE
    // fully handled. For a bare two-terminal quartz that is right, because
    // there is no motional arm to model and nothing a reader could act on. For
    // a PACKAGED OSCILLATOR it is not: `crystal_footprint` in db/passives.toml
    // matches `^Oscillator...:` too, and a TCXO has a supply pin and DRIVES a
    // clock, so its supply draw and its driven output both go unmodelled. That
    // case carries a WARNING, which `BindReport::warnings` collects over every
    // row including the ignored ones.
    if model.passive_class == Some(hauksbee_models::schema::PassiveClass::Crystal) {
        // THE DISCRIMINATOR IS THE WIRING, NOT THE FOOTPRINT STRING.
        // `crystal_value` in db/passives.toml matches a bare frequency with no
        // footprint constraint, so a TCXO in an Eagle-style land
        // ("complib:OSC_SMD_3225_4P", "XO-5032") resolves to this class; testing
        // only `footprint.contains("oscillator")` (the KiCad spelling and
        // nobody else's) would take the silent path for it.
        //
        // What separates the two kinds is that a POWERED part has a pin on a
        // supply rail: a quartz blank has two terminals going to the MCU's
        // oscillator pins and, on a 4-pad part, two shield pads going to ground,
        // and is never wired to a rail. A COUNT OF NON-GROUND NETS DOES NOT
        // WORK: corne-cherry grounds its 4-pad crystal's shield pads to a net
        // called "GNDR" (not `NodeId::GROUND`), and lily58's Pro_V2 wires its
        // shield pads to the load-capacitor nets, giving four non-ground nets on
        // an ordinary 12 MHz crystal. So the test is a pin on a RESOLVED SUPPLY
        // RAIL, with the footprint string as a third-chance signal for a
        // KiCad-library part whose supply net the rail detector missed.
        let mut on_a_supply_rail = false;
        for pin in &comp.pins {
            let Some(node) = node_of(pin.net) else {
                continue;
            };
            if node.is_ground() {
                continue;
            }
            if power_nets.contains_key(circuit.node_name(node)) {
                on_a_supply_rail = true;
                break;
            }
        }
        let is_powered_oscillator =
            on_a_supply_rail || comp.footprint.to_ascii_lowercase().contains("oscillator");
        let warning = is_powered_oscillator.then(|| {
            format!(
                "{}{} ({}): packaged oscillator skipped as high impedance; it has a \
                 supply pin and drives a clock, and neither its supply draw nor its \
                 output is modelled, so any result that depends on this clock \
                 existing is not evidence",
                Assumption::PARTIAL_MODEL_MARKER,
                comp.reference,
                comp.value
            )
        });
        return (
            BindOutcome::Skipped {
                reason: "crystal, resonator or packaged oscillator: modelled as \
                         high impedance, with no motional arm and no oscillator \
                         core, so a powered oscillator's supply draw and its \
                         driven clock output are absent too"
                    .to_string(),
            },
            warning,
            guesses,
        );
    }

    let (outcome, warning) = match model.kind {
        Passive => bind_passive(comp, model, circuit, node_of),
        Diode => bind_diode(comp, model, circuit, &role_nets),
        BjtNpn | BjtPnp => bind_bjt(comp, model, circuit, &role_nets),
        Nmos | Pmos => bind_mosfet(comp, model, circuit, &role_nets),
        Vreg => bind_vreg(
            comp, model, source, circuit, &role_nets, regulators, components, node_of,
        ),
        Opamp => bind_opamp(comp, model, circuit, &role_nets),
        Comparator => bind_comparator(comp, model, circuit, &role_nets),
        Timer => bind_timer_555(comp, model, circuit, &role_nets),
        AnalogSwitch => bind_analog_switch(comp, model, circuit, &role_nets, power_nets),
        Digital | ShiftRegister => {
            let kind = if model.kind == ShiftRegister {
                "shift_register"
            } else {
                "digital"
            };
            bound_digital_outcome(comp, model, circuit, &role_nets, digital, kind)
        }
        Dac => {
            // MCP4728-class quad I2C DAC: stamp Thevenin drivers on the four
            // VOUT channel nets and record a DacBinding. The scheduler creates
            // the I2C slave and pushes computed VOUT onto these drivers each
            // chunk. The I2C transactions reach the slave through the MCU's TWI
            // `on_i2c` hook, not these nets, so no digital buffer is stamped.
            bind_mcp4728_dac(comp, model, circuit, &role_nets, dacs)
        }
        // Treated as a behavioral passthrough buffer for now.
        Adc => bound_digital_outcome(comp, model, circuit, &role_nets, digital, "adc"),
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

/// Bind one declarative-logic part and turn the result into a report outcome.
///
/// A part whose logic spec does not compile is NOT bound: its nets float.
/// Reporting it as `Digital` anyway would make a broken part look healthy on
/// every report surface (`critical_parts_bound` included), so the failure is
/// recorded as unresolved and warned about.
fn bound_digital_outcome(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    role_nets: &HashMap<String, NodeId>,
    digital: &mut Vec<DigitalComponent>,
    kind: &str,
) -> (BindOutcome, Option<String>) {
    match bind_digital(comp, model, circuit, role_nets, digital) {
        Ok(()) => (
            BindOutcome::Digital {
                kind: kind.to_string(),
            },
            entry_warning(comp, model),
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

/// Resolve a model-declared firmware peripheral (EEPROM, SPI NOR, register-map
/// sensor) against the board's own wiring.
///
/// `Err((reason, warning))` when the board does not wire the bus roles, the
/// supply roles, or the strap levels the spec needs: a flash with no CS or an
/// EEPROM with no SDA is left open, not counted as modelled because its part
/// number was recognised. A register-map spec's own `[sensor.pins]` fills the
/// roles the card's `[models.pins]` leaves out, and its address-select table
/// picks the address from the board's straps.
#[allow(clippy::too_many_lines)] // one refusal per unwired bus/power role
fn bind_model_peripheral(
    comp: &Component,
    model: &ModelEntry,
    spec: PeripheralSpec,
    role_nets: &HashMap<String, NodeId>,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
    power_nets: &HashMap<String, f64>,
    circuit: &Circuit,
) -> Result<PeripheralBinding, (String, String)> {
    let sensor = match &spec {
        PeripheralSpec::RegisterMap { spec_toml, .. } => Some(
            SensorSpec::from_toml(spec_toml)
                .expect("model validation guarantees a valid register-map spec")
                .sensor,
        ),
        _ => None,
    };
    let mut role_nets = role_nets.clone();
    for pins in sensor.iter().map(|s| &s.pins) {
        for (role, node) in role_node_map_pins(comp, pins, None, node_of) {
            role_nets.entry(role).or_insert(node);
        }
    }
    let role_nets = &role_nets;
    // Every refusal reads the same way: the machine-facing reason, then the
    // human warning prefixed with which board part it is about.
    let refuse = |reason: String, detail: String| -> (String, String) {
        (
            reason,
            format!("{} ({}): {detail}", comp.reference, comp.value),
        )
    };
    let all_connected = |roles: &[&str]| roles.iter().all(|role| role_nets.contains_key(*role));

    let power = match model.peripheral_power.clone() {
        Some(power) => {
            let node = |role: &str, which: &str| -> Result<NodeId, (String, String)> {
                role_nets.get(role).copied().ok_or_else(|| {
                    refuse(
                        format!(
                            "model declares peripheral power but {which} role '{role}' is not connected"
                        ),
                        format!(
                            "peripheral model '{}' cannot attach its electrical load because {which} role '{role}' is not connected",
                            model.id
                        ),
                    )
                })
            };
            let supply_node = node(&power.supply_role, "supply")?;
            let return_node = node(&power.return_role, "return")?;
            Some(BoundPeripheralPower {
                spec: power,
                supply_node,
                return_node,
            })
        }
        None => None,
    };

    let mut i2c_address_assumed = None;
    let (cs_net, i2c_address_override) = match &spec {
        PeripheralSpec::I2cEeprom { .. } => {
            if !all_connected(&["scl", "sda"]) {
                return Err(refuse(
                    "model declares an I2C EEPROM but SCL/SDA are not both connected".to_string(),
                    format!(
                        "I2C EEPROM model '{}' cannot attach because SCL/SDA are not both connected",
                        model.id
                    ),
                ));
            }
            (None, None)
        }
        PeripheralSpec::SpiNorFlash {
            cs_role,
            clk_role,
            mosi_role,
            miso_role,
            ..
        } => {
            let Some(cs_net) = role_nets.get(cs_role).copied() else {
                return Err(refuse(
                    format!(
                        "model declares SPI NOR but chip-select role '{cs_role}' is not connected"
                    ),
                    format!(
                        "SPI NOR model '{}' cannot attach because chip-select role '{cs_role}' is not connected",
                        model.id
                    ),
                ));
            };
            if !all_connected(&[clk_role, mosi_role, miso_role]) {
                return Err(refuse(
                    format!(
                        "model declares SPI NOR but {clk_role}/{mosi_role}/{miso_role} are not all connected"
                    ),
                    format!(
                        "SPI NOR model '{}' cannot attach because {clk_role}/{mosi_role}/{miso_role} are not all connected",
                        model.id
                    ),
                ));
            }
            (Some(cs_net), None)
        }
        PeripheralSpec::RegisterMap {
            scl_role,
            sda_role,
            cs_role,
            clk_role,
            mosi_role,
            miso_role,
            required_high_roles,
            required_low_roles,
            address_select_role,
            address_when_low,
            address_when_high,
            ..
        } => {
            let sensor = sensor.as_ref().expect("parsed above for a register map");
            // A strap role must resolve to ground or to a rail the board's own
            // power-net resolution accepted; anything else is not a level.
            let strap_level = |role: &str| -> Result<bool, String> {
                let node = role_nets
                    .get(role)
                    .copied()
                    .ok_or_else(|| format!("strap role '{role}' is not connected"))?;
                if node.is_ground() {
                    Ok(false)
                } else if power_nets.contains_key(circuit.node_name(node)) {
                    Ok(true)
                } else {
                    Err(format!(
                        "strap role '{role}' is on '{}' rather than a resolved supply or ground",
                        circuit.node_name(node)
                    ))
                }
            };
            let require_strap = |role: &str, want: bool| -> Result<(), (String, String)> {
                if strap_level(role) == Ok(want) {
                    return Ok(());
                }
                let level = if want { "high" } else { "low" };
                let opposite = if want {
                    "resolved low"
                } else {
                    "resolved high"
                };
                let actual = strap_level(role).map_or_else(|error| error, |_| opposite.to_string());
                Err(refuse(
                    format!(
                        "register-map bus personality requires role '{role}' {level}, but it is {actual}"
                    ),
                    format!(
                        "model '{}' register-map behavior not attached: role '{role}' must resolve {level} ({actual})",
                        model.id
                    ),
                ))
            };
            for role in required_high_roles {
                require_strap(role, true)?;
            }
            for role in required_low_roles {
                require_strap(role, false)?;
            }
            let address = match (
                address_select_role.as_deref(),
                address_when_low,
                address_when_high,
            ) {
                (Some(role), Some(low), Some(high)) => match strap_level(role) {
                    Ok(false) => Some(*low),
                    Ok(true) => Some(*high),
                    Err(error) => {
                        return Err(refuse(
                            format!("register-map I2C address cannot be selected: {error}"),
                            format!(
                                "model '{}' register-map behavior not attached because {error}",
                                model.id
                            ),
                        ))
                    }
                },
                _ if sensor.address_selects.is_empty() => None,
                _ => sensor
                    .resolve_address(|role| {
                        strap_level(role)
                            .map(|high| if high { StrapTie::High } else { StrapTie::Low })
                    })
                    .map(|choice| {
                        i2c_address_assumed = choice.assumed;
                        choice.address
                    }),
            };
            match sensor.bus {
                Bus::I2c => {
                    if !all_connected(&[scl_role, sda_role]) {
                        return Err(refuse(
                            format!(
                                "model declares an I2C register map but roles '{scl_role}'/'{sda_role}' are not both connected"
                            ),
                            format!(
                                "register-map model '{}' cannot attach because I2C roles '{scl_role}'/'{sda_role}' are not both connected",
                                model.id
                            ),
                        ));
                    }
                    (None, address)
                }
                Bus::Spi => {
                    let Some(cs_net) = role_nets.get(cs_role).copied() else {
                        return Err(refuse(
                            format!(
                                "model declares an SPI register map but chip-select role '{cs_role}' is not connected"
                            ),
                            format!(
                                "register-map model '{}' cannot attach because chip-select role '{cs_role}' is not connected",
                                model.id
                            ),
                        ));
                    };
                    if !all_connected(&[clk_role, mosi_role, miso_role]) {
                        return Err(refuse(
                            format!(
                                "model declares an SPI register map but {clk_role}/{mosi_role}/{miso_role} are not all connected"
                            ),
                            format!(
                                "register-map model '{}' cannot attach because {clk_role}/{mosi_role}/{miso_role} are not all connected",
                                model.id
                            ),
                        ));
                    }
                    (Some(cs_net), None)
                }
            }
        }
    };

    Ok(PeripheralBinding {
        reference: comp.reference.clone(),
        spec,
        cs_net,
        i2c_address_override,
        i2c_address_assumed,
        power,
    })
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
/// number and no electrode name, and never re-roles a pad the board or card
/// names: a rule that names such a pad otherwise is the wrong pinout, so when it
/// would fill a gap the part is refused (the third element) rather than rewired.
/// A card numbering its pads from a package standard (its sheet prints lead
/// letters alone) is a guess too, reported per pad and refused on a clash.
fn role_node_map_guessed(
    comp: &Component,
    model: &ModelEntry,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
    pin_rules: &hauksbee_models::PinRuleTable,
) -> (HashMap<String, NodeId>, Vec<String>, Option<String>) {
    let (mut m, kind) = (role_node_map(comp, model, node_of), model.kind);
    let (mut guesses, mut clash, pad_count) = (Vec::new(), None, comp.pins.len());
    let canon = |r: &str| role_from_pinfunction(kind, r).unwrap_or_else(|| r.to_ascii_lowercase());
    let standard = hauksbee_models::validation::pin_standard(model);
    for pin in &comp.pins {
        let (n, node) = (&pin.number, node_of(pin.net));
        let roles = pad_role_sources(pin, &model.pins, Some(kind));
        let named = roles.iter().flatten().next().cloned();
        // A role the card's pads give this pad bound its node, even beside a pin-function.
        let own = |r: &&String| roles[0].as_ref() != Some(*r) && m.get(*r) == node.as_ref();
        let carded = node.and_then(|_| roles[1].iter().chain(&roles[3]).find(own));
        if let (Some(convention), Some(role)) = (standard, carded) {
            guesses.push(format!("pad {n} role '{role}' numbered from package standard \"{convention}\" (the datasheet prints lead letters only; footprint \"{}\")", comp.footprint));
        }
        let found = pin_rules.role_for_pad(&comp.footprint, Some(kind), pad_count, n);
        let (Some(node), Some(inf)) = (node, found) else {
            continue;
        };
        let (id, role) = (&inf.rule_id, &inf.role);
        if let Some(own) = named {
            let why = format!("pad {n} is named '{own}', not '{role}' as pin rule '{id}' reads it");
            clash = clash.or_else(|| (canon(&own) != canon(role)).then_some(why));
        } else if !m.contains_key(role) {
            m.insert(role.clone(), node);
            guesses.push(format!(
                "pad {n} role '{role}' guessed from rule '{id}' (no explicit pin-function; \
                 footprint \"{}\", {pad_count} pads)",
                comp.footprint
            ));
        }
    }
    // A clash matters only when the rule or the package standard supplies a role.
    match clash.filter(|_| !guesses.is_empty()) {
        Some(why) => (m, Vec::new(), Some(why)),
        None => (m, guesses, None),
    }
}

/// Two pads of one signal role on two nets: the role map keeps one, floating a
/// unit. (One pad on two nets is a board contradiction, refused elsewhere.)
/// Only multi-unit kinds: elsewhere a repeated role is one internal node (an
/// SOT-223 tab, a flow-through ESD pin) that a board may leave on its own net.
fn split_role(
    comp: &Component,
    model: &ModelEntry,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> Option<String> {
    use ComponentKind::{BjtNpn, BjtPnp, Comparator, Diode, Opamp};
    let unit_role = |r: &&String| match model.kind {
        Opamp | Comparator | BjtNpn | BjtPnp => !repeatable_role(r),
        Diode => matches!(r.as_str(), "anode" | "cathode"),
        _ => false,
    };
    let mut first = HashMap::new();
    comp.pins.iter().find_map(|pin| {
        let role = model.pins.get(&pin.number).filter(unit_role)?;
        let node = node_of(pin.net)?;
        let (pad, at) = *first.entry(role).or_insert((&pin.number, node));
        let why = "sit on different nets, so only one would be bound: a multi-unit part needs per-unit roles";
        (at != node && *pad != pin.number).then(|| format!("pads {pad} and {} both carry role '{role}' but {why}", pin.number))
    })
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
    // Each source visits ALL pads before the next source. Per-pad fallback would
    // let an earlier model pad override a later pinfunction for the same role.
    for source in 0..4 {
        for pin in &comp.pins {
            let role = pad_role_sources(pin, pins, kind)[source].take();
            if let (Some(role), Some(node)) = (role, node_of(pin.net)) {
                m.entry(role).or_insert(node);
            }
        }
    }
    m
}

/// Each source's role for a pad, in precedence order: the pin-function, the card's
/// pads, a kind-aware electrode letter (C: cathode on a diode, collector on a BJT)
/// and an Eagle P$1/P$2 ordinal re-reading the card's pads.
fn pad_role_sources(
    pin: &hauksbee_extract::Pin,
    pins: &std::collections::BTreeMap<String, String>,
    kind: Option<ComponentKind>,
) -> [Option<String>; 4] {
    let card = |pad: &str| pins.get(pad).cloned();
    [
        kind.and_then(|k| role_from_pinfunction(k, &pin.function)),
        card(&pin.number),
        kind.and_then(|k| role_from_electrode_pad(k, &pin.number)),
        ordinal_pad(&pin.number).and_then(|ord| card(&ord)),
    ]
}

/// Interpret an electrode-letter pad "number" as a binder role for `kind`.
/// Only the kinds whose pads are conventionally lettered participate (diode,
/// BJT, MOSFET), on everything else a lettered pad is not an electrode name.
/// A purely numeric pad is a pad number, never an electrode.
fn role_from_electrode_pad(kind: ComponentKind, pad: &str) -> Option<String> {
    use ComponentKind::{BjtNpn, BjtPnp, Diode, Nmos, Pmos};
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
        // A regulator's pad NUMBERS are the least portable thing about it, and
        // its pin NAMES are among the most: "VIN"/"VOUT"/"GND"/"CE" mean the
        // same on every LDO ever built, while the pad they land on changes with
        // the package and even between a vendor's own package options (the
        // XC6204's pads 1 and 4 trade places between SOT-25 and SOT-89-5, so a
        // board fitting one land with a symbol carrying the other's numbers puts
        // the input pin on a pad the model calls "nc"). The board's own
        // pinfunction is the evidence that settles it.
        //
        // `fb` covers the adjustable parts' feedback node. `shdn` is deliberately
        // NOT folded into `en`: it is the same pin with the opposite sense, and a
        // model that ever gates on `en` would read a shutdown as an enable.
        ComponentKind::Vreg => match f.as_str() {
            "vin" | "in" | "input" | "vi" => "in",
            "vout" | "out" | "output" | "vo" => "out",
            "gnd" | "vss" | "ground" | "gnd1" => "gnd",
            "ce" | "en" | "enable" => "en",
            // `shdn` gets its OWN role and is deliberately not folded into `en`: it
            // is the active-LOW spelling, so a binder that later learns to gate on
            // the enable must be able to tell the two apart or it will invert half
            // the library. Mapped rather than omitted so that a board naming the pad
            // `SHDN` is read from its pin function instead of falling through to
            // pin-rule guessing; `bind_vreg` treats both as "an enable exists and is
            // not modelled" and says so.
            "shdn" | "shutdown" | "nshdn" => "shdn",
            "fb" | "adj" | "feedback" => "fb",
            "nc" => "nc",
            _ => return None,
        },
        // A 555's schematic pin names are unusually varied (CONT/CV/CTRL for
        // pin 5, DIS/DISCH for 7), and the pad-number map cannot settle a
        // symbol that numbers them differently, so the board's own pin
        // function is read first. `~{RESET}` is the active-low spelling KiCad
        // writes; the sense is handled in the stamp, not by the role name.
        ComponentKind::Timer => match f.as_str() {
            "vcc" | "vdd" | "v+" => "vcc",
            "gnd" | "vss" | "ground" => "gnd",
            "out" | "output" | "q" => "out",
            "trig" | "trigger" | "tr" => "trig",
            "thres" | "thresh" | "threshold" | "thr" | "th" => "thres",
            "disch" | "discharge" | "dis" | "dsch" => "disch",
            "ctrl" | "cont" | "control" | "cv" | "cont_v" | "cvolt" => "ctrl",
            "reset" | "rst" | "~{reset}" | "reset_n" | "nreset" => "reset",
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

#[allow(clippy::too_many_lines)] // array split, series path, then per-class stamping
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
    // Extractors preserve physical pad records. A scalar chip can therefore
    // carry repeated records for the same numbered terminals (for example
    // 1,2,1,2 across copper layers/primitives). Classify and stamp passives by
    // their logical numbered terminals, while leaving the extraction records
    // untouched for geometry/provenance consumers.
    let logical_pins = match logical_passive_pins(comp) {
        Ok(pins) => pins,
        Err(reason) => {
            return (
                BindOutcome::Unresolved {
                    reason: reason.clone(),
                },
                Some(format!(
                    "{} ({}): {reason}; left open rather than guessing connectivity",
                    comp.reference, comp.value
                )),
            );
        }
    };
    // A passive with more than two LOGICAL terminals is an ARRAY (a bussed or
    // isolated R/C network), not one 2-terminal element. Silently taking the
    // first two terminals and stamping ONE device deletes every other element
    // in the pack: a 4-resistor array becomes a single resistor. Split it into
    // per-element devices instead.
    if logical_pins.len() > 2 {
        return bind_passive_array(comp, &logical_pins, parsed.as_ref(), circuit, node_of);
    }
    let (a, b) = two_terminal_nodes(&logical_pins, node_of);
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

    // A passive whose behavioural model declares a series path owns its
    // between-terminal conduction. Stamping the ordinary scalar passive here
    // as well would put a permanent resistor in parallel with the FSM-controlled
    // path, so a fuse could never open and a relay could never disconnect.
    // `bind_behavioral` stamps the path in pass 3a from the same fitted-part
    // witness; this row records that ownership without inventing a second path.
    if !model.behavioral.series_paths.is_empty() {
        return (
            BindOutcome::Analog {
                device: "behavioural state-controlled series path".to_string(),
            },
            entry_warning(comp, model),
        );
    }

    // A fuse or ferrite's board Value normally names a CURRENT RATING or an
    // impedance-at-frequency ("1.8A", "120R@100MHz"), not the DC resistance
    // the solver needs. Exact/family model cards already carry `ohms`; the old
    // binder ignored it, tried to parse the display string, and either left the
    // part OPEN or stamped a fantastical 120 H inductor. The declared passive
    // class is the semantic discriminator, so use the model's sourced DC value
    // only for the two classes where the display value is not a resistance.
    if matches!(
        model.passive_class,
        Some(
            hauksbee_models::schema::PassiveClass::Fuse
                | hauksbee_models::schema::PassiveClass::FerriteBead
        )
    ) {
        let Some(ohms) = model
            .params
            .get_f64("ohms")
            .filter(|v| v.is_finite() && *v > 0.0)
        else {
            return (
                BindOutcome::Unresolved {
                    reason: format!(
                        "{} model '{}' has no positive finite DC resistance",
                        if model.passive_class
                            == Some(hauksbee_models::schema::PassiveClass::Fuse)
                        {
                            "fuse"
                        } else {
                            "ferrite-bead"
                        },
                        model.id
                    ),
                },
                Some(format!(
                    "{} ({}): model '{}' identifies the part class but does not provide the DC resistance needed to close the path; left open",
                    comp.reference, comp.value, model.id
                )),
            );
        };
        circuit.add(Device::resistor(comp.reference.clone(), a, b, ohms));
        return (
            BindOutcome::Analog {
                device: format!("R {}", fmt_eng(ohms, "Ω")),
            },
            entry_warning(comp, model),
        );
    }

    let Some(p) = parsed else {
        return unparseable(comp);
    };

    // Source-bound package parasitics are real circuit elements: `esr`/`esl` on
    // a capacitor card and DCR (`ohms`) on an inductor card. Realize them as an
    // explicit series chain with internal nodes so DC, transient, AC, loss and
    // thermal consumers all see the same physics. Arrays keep the per-element
    // path: a single pack-level ESR/DCR cannot be assigned to multiple elements
    // without a per-element schema.
    let is_inductor = model.passive_class == Some(hauksbee_models::schema::PassiveClass::Inductor)
        || passive_letter(comp) == Some('L')
        || p.unit
            .as_deref()
            .is_some_and(|unit| unit.eq_ignore_ascii_case("H"));
    if is_inductor {
        if let Some(dcr) = model
            .params
            .get_f64("series_ohms")
            .or_else(|| model.params.get_f64("ohms"))
            .filter(|v| v.is_finite() && *v > 0.0)
        {
            let mid = circuit.node(&format!("__{}_dcr", comp.reference));
            circuit.add(Device::inductor(
                comp.reference.clone(),
                a,
                mid,
                rkm_henries(&p),
            ));
            circuit.add(Device::resistor(
                format!("{}_dcr", comp.reference),
                mid,
                b,
                dcr,
            ));
            return (
                BindOutcome::Analog {
                    device: format!(
                        "L {} + DCR {}",
                        fmt_eng(rkm_henries(&p), "H"),
                        fmt_eng(dcr, "Ω")
                    ),
                },
                entry_warning(comp, model),
            );
        }
    }

    let is_capacitor = model.passive_class
        == Some(hauksbee_models::schema::PassiveClass::Capacitor)
        || passive_letter(comp) == Some('C')
        || p.unit
            .as_deref()
            .is_some_and(|unit| unit.eq_ignore_ascii_case("F"));
    if is_capacitor {
        let esr = model
            .params
            .get_f64("esr")
            .filter(|v| v.is_finite() && *v > 0.0);
        let esl = model
            .params
            .get_f64("esl")
            .filter(|v| v.is_finite() && *v > 0.0);
        if esr.is_some() || esl.is_some() {
            let mut left = a;
            if let Some(esr) = esr {
                let mid = circuit.node(&format!("__{}_esr", comp.reference));
                circuit.add(Device::resistor(
                    format!("{}_esr", comp.reference),
                    left,
                    mid,
                    esr,
                ));
                left = mid;
            }
            if let Some(esl) = esl {
                let mid = circuit.node(&format!("__{}_esl", comp.reference));
                circuit.add(Device::inductor(
                    format!("{}_esl", comp.reference),
                    left,
                    mid,
                    esl,
                ));
                left = mid;
            }
            circuit.add(Device::capacitor(comp.reference.clone(), left, b, p.si));
            return (
                BindOutcome::Analog {
                    device: format!("C {} with sourced parasitics", fmt_eng(p.si, "F")),
                },
                entry_warning(comp, model),
            );
        }
    }
    let (device, note) = passive_device(comp, comp.reference.clone(), a, b, &p);
    let label = device_label(&device);
    circuit.add(device);
    (
        BindOutcome::Analog { device: label },
        merge_bind_warnings(note, entry_warning(comp, model)),
    )
}

/// One electrical terminal reconstructed from one or more physical pin
/// records. Numbered records with the same number are the same terminal when
/// their known nets agree; an unconnected copy is enriched by a connected
/// copy. Conflicting known nets are rejected by [`logical_passive_pins`].
#[derive(Debug)]
struct LogicalPassivePin {
    number: String,
    net: Option<i64>,
}

/// Collapse repeated physical records into logical passive terminals.
///
/// Empty pad numbers carry no identity evidence, so they deliberately remain
/// separate records. For a numbered terminal, net `0` has the same no-net
/// meaning as `None`: either may be enriched by a known connected copy. Two
/// different non-zero nets for one number are contradictory evidence and must
/// make the passive open rather than selecting a record by input order.
fn logical_passive_pins(comp: &Component) -> Result<Vec<LogicalPassivePin>, String> {
    let mut pins: Vec<LogicalPassivePin> = Vec::new();
    let mut index_by_number: HashMap<String, usize> = HashMap::new();

    for physical in &comp.pins {
        let number = physical.number.trim().to_string();
        let net = physical.net.filter(|net| *net != 0);
        if number.is_empty() {
            pins.push(LogicalPassivePin { number, net });
            continue;
        }

        let Some(&index) = index_by_number.get(&number) else {
            index_by_number.insert(number.clone(), pins.len());
            pins.push(LogicalPassivePin { number, net });
            continue;
        };
        let logical = &mut pins[index];
        match (logical.net, net) {
            (None, Some(known)) => logical.net = Some(known),
            (Some(existing), Some(known)) if existing != known => {
                return Err(format!(
                    "passive pad {} has conflicting nets {existing} and {known}",
                    logical.number
                ));
            }
            _ => {}
        }
    }

    Ok(pins)
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

/// Henries for an inductor's parsed value. An RKM code with `R` as the decimal
/// point ("2R2", "4R7") is microhenries on an inductor, the EIA inductor marking
/// convention; read raw it would be 2.2 H. Ohm-marked ferrite beads ("600R")
/// never reach here: the model library classes them as beads first.
fn rkm_henries(p: &hauksbee_models::value::ParsedValue) -> f64 {
    let rkm = p.suffix.as_deref() == Some("R") && p.unit.is_none();
    if rkm {
        p.si * 1e-6
    } else {
        p.si
    }
}

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
    let letter = passive_letter(comp);
    if letter == Some('C') || unit.eq_ignore_ascii_case("F") {
        (Device::capacitor(name, a, b, p.si), None)
    } else if letter == Some('L') || unit.eq_ignore_ascii_case("H") {
        (Device::inductor(name, a, b, rkm_henries(p)), None)
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
            Device::resistor(name, a, b, ZERO_OHM_JUMPER_OHMS),
            Some(note),
        )
    } else {
        (Device::resistor(name, a, b, p.si.max(1e-6)), None)
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
    logical_pins: &[LogicalPassivePin],
    parsed: Option<&hauksbee_models::value::ParsedValue>,
    circuit: &mut Circuit,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> (BindOutcome, Option<String>) {
    let Some(p) = parsed else {
        return unparseable(comp);
    };
    // Natural pad order: numeric pads sort numerically ("2" before "10"), any
    // non-numeric pads after them lexicographically.
    let mut pads: Vec<&LogicalPassivePin> = logical_pins.iter().collect();
    pads.sort_by_key(|pin| {
        (
            pin.number.trim().parse::<u64>().unwrap_or(u64::MAX),
            pin.number.clone(),
        )
    });

    let mut notes: Vec<String> = Vec::new();
    // (element name, a, b) before connectivity filtering.
    let mut elements: Vec<(String, Option<NodeId>, Option<NodeId>)> = Vec::new();
    if pads.len().is_multiple_of(2) {
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
    pins: &[LogicalPassivePin],
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> (Option<NodeId>, Option<NodeId>) {
    let mut it = pins.iter();
    let a = it.next().and_then(|p| node_of(p.net));
    let b = it.next().and_then(|p| node_of(p.net));
    (a, b)
}

/// One diode, or several in one package. A package's units number their electrodes
/// (`anode1`, `cathode2`); an electrode a unit does not number falls back to the
/// unnumbered `anode`/`cathode` (a shared common cathode or anode), and `common`, the
/// pad a series pair's first cathode and second anode share, fills the one it still lacks.
fn bind_diode(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
) -> (BindOutcome, Option<String>) {
    let mut units: std::collections::BTreeSet<&str> = (roles.keys())
        .filter_map(|r| {
            r.strip_prefix("anode")
                .or_else(|| r.strip_prefix("cathode"))
        })
        .filter(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
        .collect();
    if units.is_empty() {
        units.insert("");
    }
    let own = |name: &str, alias: &[&str], n: &str| {
        let get = |role: &str| roles.get(role).copied();
        get(&format!("{name}{n}"))
            .or_else(|| get(name))
            .or_else(|| pick(roles, alias).copied())
    };
    let (model_d, common) = (diode_model_from(model), roles.get("common").copied());
    let diode = |n: &&str| {
        let (a, k) = match (own("anode", &["a", "p"], n), own("cathode", &["k", "n"], n)) {
            (Some(a), k) => (Some(a), k.or(common)),
            (None, k) => (common, k),
        };
        let name = match n {
            &"" => comp.reference.clone(),
            _ => format!("{}_e{n}", comp.reference),
        };
        Some(Device::Diode {
            name,
            a: a?,
            k: k?,
            model: model_d,
        })
    };
    let found: Vec<Device> = units.iter().filter_map(diode).collect();
    if found.is_empty() {
        return open_warning(comp, "diode pins not both connected");
    }
    for d in found {
        circuit.add(d);
    }
    (
        BindOutcome::Analog {
            device: "diode".to_string(),
        },
        entry_warning(comp, model),
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
                model: m,
            });
            stamped += 1;
        }
        if stamped == 0 {
            return open_warning(comp, "paired BJT: no complete c/b/e group connected");
        }
        let warning = (!partial.is_empty()).then(|| {
            let units = partial.join(", ");
            format!(
                "{}: paired BJT unit(s) {units} missing a c/b/e connection, left open",
                comp.reference
            )
        });
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
        ..d
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
        vto: p.get_f64("vto").map_or(def.vto, |v| fold * v),
        kp: p.get_f64("kp").unwrap_or(def.kp),
        lambda: p.get_f64("lambda").unwrap_or(def.lambda),
        gamma: p.get_f64("gamma").unwrap_or(def.gamma),
        phi: p.get_f64("phi").unwrap_or(def.phi),
        w_over_l: p.get_f64("w_over_l").unwrap_or(def.w_over_l),
        n_sub: p.get_f64("n_sub").unwrap_or(def.n_sub),
        // Gate charge: the db carries TOTAL capacitances
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
        entry_warning(comp, model),
    )
}

/// An entry's own `warning` param, as a caveat ready for the bind report AND the
/// evidence map.
///
/// ONE helper rather than a copy per binder: a per-kind copy lets an entry
/// describe its own gap in the database and have that text reach nobody, which
/// is the disclosure mechanism this file builds being unavailable to most of the
/// coverage it ships.
///
/// The marker is what makes it evidence. See [`Assumption::PARTIAL_MODEL_MARKER`]:
/// without it the text lands only on `--report`, and a caveat present on one surface
/// and absent on another is worse than one absent everywhere.
fn entry_warning(comp: &Component, model: &ModelEntry) -> Option<String> {
    let w = model.params.get_str("warning")?;
    Some(format!(
        "{}{} ({}): {w}",
        Assumption::PARTIAL_MODEL_MARKER,
        comp.reference,
        comp.value
    ))
}

/// A library tie broken only by entry id is a guess about which part (which
/// variant) this is, so it leads the part's warning as evidence on every
/// surface; the choice itself stays the deterministic smallest id.
fn with_tie_warning(
    comp: &Component,
    res: &hauksbee_models::Resolution,
    warning: Option<String>,
) -> Option<String> {
    let Some(chosen) = res.model.as_ref().filter(|_| !res.tied.is_empty()) else {
        return warning;
    };
    let (marker, part) = (
        Assumption::PARTIAL_MODEL_MARKER,
        format!("{} ({}): ", comp.reference, comp.value),
    );
    let rest = warning
        .as_deref()
        .map(|w| w.strip_prefix(marker).unwrap_or(w));
    let rest = rest.map(|w| format!("; also {}", w.strip_prefix(&part).unwrap_or(w)));
    let (id, tied) = (&chosen.id, res.tied.join(", "));
    Some(format!(
        "{marker}{part}matched library entries {id}, {tied} equally well; bound '{id}', the first \
         by id, so where they differ (pins, values) results describe that entry{}",
        rest.unwrap_or_default()
    ))
}

/// The open outcome for a passive whose value text does not parse. A blank or
/// placeholder value (`""`, `?`) is the `placeholder_value` lint's finding and is
/// named so, not called unparseable.
fn unparseable(comp: &Component) -> (BindOutcome, Option<String>) {
    let what = if is_placeholder_value(&comp.value) {
        "placeholder"
    } else {
        "unparseable"
    };
    let reason = format!("{what} value '{}'", comp.value);
    let warning = format!("{}: {reason}, left open", comp.reference);
    (BindOutcome::Unresolved { reason }, Some(warning))
}

fn merge_bind_warnings(a: Option<String>, b: Option<String>) -> Option<String> {
    a.into_iter().chain(b).reduce(|a, b| format!("{a}; {b}"))
}

/// The control loop of a linear regulator is not simulated, so its datasheet
/// output-capacitor requirement is either checked against the board or stated
/// as a gap; ESR is never guessed because the board file does not carry it.
fn vreg_stability_warning(
    comp: &Component,
    model: &ModelEntry,
    (out, gnd): (NodeId, NodeId),
    components: &[Component],
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> Option<String> {
    let across_output = |cap: &Component| {
        let pins = logical_passive_pins(cap).ok()?;
        let nodes: Vec<_> = pins.iter().filter_map(|pin| node_of(pin.net)).collect();
        let fitted = AssemblyState::of(cap).is_present() && passive_letter(cap) == Some('C');
        let spans =
            matches!(*nodes.as_slice(), [a, b] if (a, b) == (out, gnd) || (a, b) == (gnd, out));
        let farads = parse_value(cap.value.trim()).map(|v| v.si)?;
        (fitted && spans && farads.is_finite() && farads > 0.0).then_some(farads)
    };
    let caps: Vec<_> = components
        .iter()
        .filter_map(|cap| Some((cap.reference.as_str(), across_output(cap)?)))
        .collect();
    let found = caps.iter().map(|(_, farads)| farads).sum::<f64>();
    let refs: Vec<_> = caps.iter().map(|(reference, _)| *reference).collect();
    let counted = if refs.is_empty() {
        "none".to_owned()
    } else {
        refs.join(", ")
    };
    let esr = |name| model.params.get_f64(name);
    let (marker, gap) = match model.params.get_f64("cout_min_f") {
        None => (
            "",
            "control-loop stability is not simulated and this card does not state the \
             output-capacitor requirement; add `cout_min_f` and, when required, \
             `cout_esr_min_ohm` plus `cout_esr_max_ohm` to the card"
                .to_owned(),
        ),
        Some(required) if found < required => (
            Assumption::OPERATING_LIMIT_MARKER,
            format!(
                "{} requires at least {} output capacitance for stability, but {} was found \
                 directly from output to ground (counted: {counted})",
                comp.reference,
                fmt_eng(required, "F"),
                fmt_eng(found, "F")
            ),
        ),
        Some(_) if esr("cout_esr_min_ohm").is_some() => (
            "",
            format!(
                "control-loop stability requires output-capacitor ESR in {}..{}, but the board \
                 file does not carry capacitor ESR and it is not guessed from value or footprint \
                 (counted: {counted})",
                fmt_eng(esr("cout_esr_min_ohm")?, "Ω"),
                fmt_eng(esr("cout_esr_max_ohm")?, "Ω")
            ),
        ),
        Some(_) => return None,
    };
    Some(format!(
        "{}{marker}{} ({}): {gap}",
        Assumption::PARTIAL_MODEL_MARKER,
        comp.reference,
        comp.value
    ))
}

#[allow(clippy::too_many_lines)] // converter, shunt, adjustable, fixed, then caveats
fn bind_vreg(
    comp: &Component,
    model: &ModelEntry,
    source: Option<&ModelSource>,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
    regulators: &mut Vec<SimpleRegulator>,
    components: &[Component],
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
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
        // An unstated loop bandwidth scales every load-step deviation, so it is
        // a disclosed default (a Qualified assumption downstream), never silent.
        let (fc, stated) = conv.loop_crossover_hz();
        let loop_note = (!stated).then(|| {
            format!(
                "{} ({}): converter loop `crossover_hz` not stated; assumed {fc:.0} Hz \
                 (f_sw/10); designs span f_sw/20..f_sw/5, so load-step deviation is \
                 uncertain about 2x either way",
                comp.reference, comp.value
            )
        });
        return (
            BindOutcome::Behavioral {
                device: format!("vreg {:.1}V behavioral converter", conv.vout_setpoint),
            },
            merge_bind_warnings(entry_warning(comp, model), loop_note),
        );
    }
    // A SHUNT reference (TL431 class) is not a pass element at all and has no
    // input pin: it SINKS cathode current to hold REF at Vref above its anode,
    // and the board's series resistor and divider set the cathode voltage.
    // Checked before `vref` because such a card carries `vref` too.
    if model.params.get_bool("shunt").unwrap_or(false) {
        return bind_shunt_ref(comp, model, circuit, roles);
    }
    // An ADJUSTABLE regulator has no output setpoint: it holds Vref against its
    // ADJ/FB pin and the BOARD's divider sets the rail. `vref` on the card
    // selects that path, and its pin map the law (see `bind_adj_vreg`).
    if let Some(vref) = model.params.get_f64("vref").filter(|v| v.is_finite()) {
        return bind_adj_vreg(
            comp, model, circuit, roles, regulators, vref, components, node_of,
        );
    }
    let Some(vout) = model.params.get_f64("vout").filter(|v| v.is_finite()) else {
        return open_warning(comp, "vreg requires an explicit finite `vout` param");
    };
    let Some(input) = roles.get("in").copied() else {
        return open_warning(comp, "vreg input not connected");
    };
    let Some(out) = roles.get("out").copied() else {
        return open_warning(comp, "vreg output not connected");
    };
    let Some(gnd) = roles.get("gnd").copied() else {
        return open_warning(comp, "vreg local return not connected");
    };
    if out == gnd || out == input || input == gnd {
        return open_warning(
            comp,
            "vreg input, output, and local return must be distinct",
        );
    }
    let source_id = circuit.add(Device::vsource(
        format!("Vreg_{}", comp.reference),
        out,
        gnd,
        SourceKind::Dc(vout),
    ));
    // The headroom the ideal source assumes: the card's widest output endpoint
    // plus its declared dropout limit. A missing dropout is recorded, not invented.
    let vout_limit = source
        .into_iter()
        .flat_map(ModelSource::uncertainty)
        .find_map(|u| match u {
            ModelUncertainty::Interval {
                parameter,
                low,
                high,
                ..
            } if parameter == "vout" => Some(low.abs().max(high.abs())),
            _ => None,
        })
        .unwrap_or_else(|| vout.abs());
    // IN draws `Iout + Iq` (the pass draw as in `bind_adj_vreg`, then `iq_a`):
    // the ideal source alone drew nothing there, hiding the load upstream.
    let dropout_v = model.params.get_f64("dropout_v");
    let ends = (input, out, gnd);
    let (draw, pass_level) =
        pass_draw(circuit, &comp.reference, ends, (source_id, vout), dropout_v);
    let mut helpers = vec![draw];
    if let Some(amps) = model.params.get_f64("iq_a").filter(|i| i.is_finite()) {
        let name = format!("Iq_{}", comp.reference);
        helpers.push(circuit.add(Device::isource(name, input, gnd, SourceKind::Dc(amps))));
    }
    regulators.push(SimpleRegulator {
        reference: comp.reference.clone(),
        source: source_id,
        input,
        min_in_v: vout_limit + dropout_v.unwrap_or(0.0),
        max_in_v: model.ratings.max_voltage_v.unwrap_or(f64::INFINITY),
        dropout_v,
        adj_return: None,
        helpers,
        pass_level: Some(pass_level),
    });
    // THE ENABLE IS NOT MODELLED, and this says so for every entry that has one
    // rather than making each entry repeat the sentence.
    //
    // Nothing above reads an `en` or `shdn` role. The stamp is an unconditional ideal
    // source, so a regulator held OFF by its enable still presents its output rail at
    // full voltage, and the board reads as having a rail it does not have. That is
    // the silent-wrongness direction: the rail is asserted, not withheld.
    //
    // Keyed on the ENTRY's pin map, not on the resolved roles, because an enable pad
    // the board left unconnected produces the same asserted rail and deserves the
    // same caveat. Emitted here rather than in the database so that every regulator
    // entry, including ones added later, is covered by construction; when `bind_vreg`
    // learns to gate on the enable this block is what to delete.
    let names_an_enable = model
        .pins
        .values()
        .any(|r| matches!(r.as_str(), "en" | "shdn"));
    let enable_warning = names_an_enable.then(|| {
        format!(
            "{}{} ({}): its {} pin is not modelled; the output rail is stamped as an \
             unconditional ideal source, so a board that holds this regulator OFF \
             still reads as having this rail present at {vout:.2} V",
            Assumption::PARTIAL_MODEL_MARKER,
            comp.reference,
            comp.value,
            if model.pins.values().any(|r| r == "shdn") {
                "shutdown"
            } else {
                "enable"
            },
        )
    });
    // SEVERAL caveats can apply to one regulator and there is one channel, so they
    // are joined rather than one silently winning. Each is a full sentence already;
    // the leading one keeps the marker that makes the whole string evidence, and the
    // followers drop their own "REF (VALUE): " so the reference is not repeated.
    let caveats = [
        vreg_stability_warning(comp, model, (out, gnd), components, node_of),
        entry_warning(comp, model),
        enable_warning,
    ];
    // The follower's own "REF (VALUE): " comes off so the reference is not said
    // twice in one sentence. The KNOWN prefix is stripped rather than cutting at
    // the first ": ", which a component value containing a colon would break.
    let prefix = format!("{} ({}): ", comp.reference, comp.value);
    let warning = caveats.into_iter().flatten().reduce(|lead, c| {
        let c = c
            .strip_prefix(Assumption::PARTIAL_MODEL_MARKER)
            .unwrap_or(&c);
        format!(
            "{lead}; also {}",
            c.strip_prefix(prefix.as_str()).unwrap_or(c)
        )
    });
    (
        BindOutcome::Behavioral {
            device: format!("vreg {vout:.1}V source"),
        },
        warning,
    )
}

/// Headroom over which a linear regulator's input draw fades out when its card
/// states no dropout: the draw is whole from `V(in) = V(out)` and gone 0.1 V
/// below it.
const PASS_FADE_V: f64 = 0.1;

/// A linear regulator's pass draw, `IN -> ret`: the current `source` delivers
/// into OUT (`stamp_vsource` counts it as LEAVING `p`, hence the sign), drawn
/// out of IN and returned where `source` took it from, so `Iin = Iout`.
/// Returns the draw and the hidden source holding its transient level.
///
/// At DC the draw reads the branch directly, so every operating point is
/// exact. In a transient it reads that hidden source instead (1 V per A),
/// which the co-sim scheduler sets each chunk to the mean current `source`
/// delivered over the chunk before (its charge over the chunk's time). Read
/// live, the branch carries an output capacitor's `C*dV/dt` of Newton's
/// residual on OUT: at a picosecond step a 22 uF capacitor turns 1e-11 V into
/// 0.2 A, and mirrored onto IN that noise stalled an upstream converter's
/// loop. The charge is noise-free (the trapezoid sums the capacitor's share
/// to `C*dV`); the price is one chunk of lag on load steps.
///
/// The ideal source holds OUT whatever IN does, so the draw is also weighted by
/// a smoothstep of the pass element's headroom `h = V(in) - V(out)` (in the
/// regulator's polarity, `setpoint`'s sign): whole for `h >= dropout`, none for
/// `h <= dropout - max(dropout, PASS_FADE_V)`, C1 in between. A collapsed or
/// unpowered input is then not dragged below ground by a load it cannot carry.
/// Every faded state has `h` below the declared dropout, where `regulation`
/// already refuses the rail, so wherever the rail is a result IN draws `Iout`.
fn pass_draw(
    circuit: &mut Circuit,
    reference: &str,
    (input, out, ret): (NodeId, NodeId, NodeId),
    (source, setpoint): (DeviceId, f64),
    dropout_v: Option<f64>,
) -> (DeviceId, DeviceId) {
    let level = circuit.node(&format!("{reference}_vreg_pass"));
    let name = format!("Vpass_{reference}");
    let held = circuit.add(Device::vsource(
        name,
        level,
        NodeId::GROUND,
        SourceKind::Dc(0.0),
    ));
    let full = dropout_v
        .filter(|d| d.is_finite() && *d > 0.0)
        .unwrap_or(0.0);
    let width = full.max(PASS_FADE_V);
    let sign = if setpoint < 0.0 { -1.0 } else { 1.0 };
    let x = format!(
        "min(max(({sign:?}*(__d2-__d3)-{:?})/{width:?},0.0),1.0)",
        full - width
    );
    let draw = circuit.add(Device::Behavioral {
        name: format!("Bpass_{reference}"),
        p: input,
        n: ret,
        output: BOutput::Current,
        expr: CompiledExpr::compile(&format!("if(time>0.0,__d0,-__d1)*{x}*{x}*(3.0-2.0*{x})"))
            .expect("pass draw expression compiles"),
        deps: vec![
            BDep::Volt(level),
            BDep::Branch(source),
            BDep::Volt(input),
            BDep::Volt(out),
        ],
    });
    (draw, held)
}

/// The adjustable vreg stamp, from primitives the IR already has. The LM317
/// class (no `gnd` in the card's `[models.pins]`) holds `Vref` between OUT and
/// ADJ; [`pass_draw`] moves that branch current onto IN (what the pass element
/// does, so the input draw is honest and ADJ carries only divider current), and IADJ
/// leaves the ADJ pin. KCL at ADJ then gives `Vout = Vref*(1 + R2/R1) + Iadj*R2`
/// from the BOARD's own divider, solved by MNA rather than parsed out of the
/// netlist. A card that maps a `gnd` pin is GROUND-referenced (the common LDO
/// with ADJ/FB and GND): its error amplifier holds `V(fb) - V(gnd)` at `Vref`.
/// That is the same `Vref` source, from OUT to a private node a unity VCVS holds
/// `V(out) - V(fb)` above GND, with the pass current and IQ drawn from IN into
/// GND, so MNA gives `Vout = Vref*(1 + R1/R2)` with R1 on OUT-FB. One divider
/// is a different rail under each law, so the card's pin map decides which.
///
/// The enable caveat `bind_vreg` emits does not apply: an adjustable
/// three-terminal part has no enable pin to leave unmodelled.
fn bind_adj_vreg(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
    regulators: &mut Vec<SimpleRegulator>,
    vref: f64,
    components: &[Component],
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> (BindOutcome, Option<String>) {
    let Some(input) = roles.get("in").copied() else {
        return open_warning(comp, "vreg input not connected");
    };
    let Some(out) = roles.get("out").copied() else {
        return open_warning(comp, "vreg output not connected");
    };
    // The schematic pin-function pass folds `adj`/`feedback` onto `fb`, but a
    // `[models.pins]` value is a role verbatim, so both spellings are read here.
    // Without one there is nothing to regulate against and no rail to report.
    let Some(adj) = roles.get("fb").or_else(|| roles.get("adj")).copied() else {
        return open_warning(comp, "adjustable vreg ADJ pin not connected");
    };
    // Read off the CARD, not the resolved roles: an unconnected GND pad must not
    // silently move a ground-referenced part onto the LM317 law.
    let grounded = model.pins.values().any(|r| r == "gnd");
    let gnd = match roles.get("gnd") {
        Some(&gnd) if grounded => gnd,
        None if grounded => return open_warning(comp, "vreg local return not connected"),
        _ => NodeId::GROUND,
    };
    // OUT on ADJ shorts an LM317's reference, but FB on OUT is a grounded part's
    // unity setting (Vout = Vref); FB on GND leaves that part nothing to hold.
    let tied = if grounded {
        gnd == input || gnd == out || gnd == adj
    } else {
        out == adj
    };
    if tied || out == input || input == adj {
        return open_warning(comp, "vreg input, output, ADJ and any GND must be distinct");
    }
    // A grounded part's `Vref` ends on a private node the VCVS drives. The VCVS
    // is no helper: it reads no branch, and left in place when `source` is
    // swapped out it keeps that node defined.
    let (low, ret) = if grounded {
        (circuit.node(&format!("{}_vreg_drive", comp.reference)), gnd)
    } else {
        (adj, adj)
    };
    let source = circuit.add(Device::vsource(
        format!("Vref_{}", comp.reference),
        out,
        low,
        SourceKind::Dc(vref),
    ));
    if grounded {
        circuit.add(Device::Vcvs {
            name: format!("Evreg_{}", comp.reference),
            p: low,
            n: gnd,
            cp: out,
            cn: adj,
            gain: 1.0,
        });
    }
    // The same I is drawn out of IN and returned where `source` took it from.
    let dropout_v = model.params.get_f64("dropout_v");
    let ends = (input, out, ret);
    let (draw, pass_level) = pass_draw(circuit, &comp.reference, ends, (source, vref), dropout_v);
    let mut helpers = vec![draw];
    // IADJ leaves the ADJ pin; a grounded part's quiescent current runs IN -> GND.
    for (prefix, key, to, on) in [("Iadj", "iadj_a", adj, true), ("Iq", "iq_a", gnd, grounded)] {
        let Some(amps) = model.params.get_f64(key).filter(|i| on && i.is_finite()) else {
            continue;
        };
        helpers.push(circuit.add(Device::isource(
            format!("{prefix}_{}", comp.reference),
            input,
            to,
            SourceKind::Dc(amps),
        )));
    }
    regulators.push(SimpleRegulator {
        reference: comp.reference.clone(),
        source,
        input,
        // The rail is a solved value: `regulation::evaluate` overwrites this from
        // |V(out) - V(return)|, and until it has, no input is enough (fail closed).
        min_in_v: f64::INFINITY,
        max_in_v: model.ratings.max_voltage_v.unwrap_or(f64::INFINITY),
        dropout_v,
        // The part's own GND, or board ground for a three-terminal part with none.
        adj_return: Some(gnd),
        helpers,
        pass_level: Some(pass_level),
    });
    (
        BindOutcome::Analog {
            device: format!("vreg {vref:.3}V adjustable reference"),
        },
        merge_bind_warnings(
            vreg_stability_warning(comp, model, (out, gnd), components, node_of),
            entry_warning(comp, model),
        ),
    )
}

/// A TL431-class adjustable SHUNT reference, from four primitives the IR
/// already has (internal reference, railed error amplifier, cathode sink,
/// ammeter) rather than a new solver device. Roles reuse the vreg vocabulary:
/// CATHODE is `out`, ANODE is `gnd` (the part's local return), REF is `fb`.
/// `Ika = ikmax*V(gate)`, so the amplifier's rails ARE the cathode-current
/// limits: railed low the part sinks nothing and the cathode sags to whatever
/// the board's divider makes it, which is how a starved reference falls out of
/// regulation here. In between the BOARD's R1/R2 closes the loop and MNA
/// produces `VKA = Vref*(1 + R1/R2) + Iref*R1` from the resistors actually
/// fitted, never parsed out. Neither shape number is chosen: closed-loop
/// `dVka/dIka` is `1/(AOL*ikmax)`, so `AOL = 1/(zka*ikmax)` reproduces the
/// datasheet `|ZKA|` exactly and linearly, and the internal reference is
/// lowered by `iktest*zka` to put `VKA` at the datasheet `Vref` at the current
/// it is specified at. The hard clamp is deliberate: its 20 mV input band is
/// far wider than the op-amp rail-crossing limiter needs. The ammeter is named
/// `Vreg_<ref>` because that is the seam `gather_device_meta` scores against the
/// card's `max_current_a`; it sits ANODE-side so its `p` stays private. The
/// cathode sink is what `rail_owner` counts as driving the cathode net, so a
/// supply-named cathode takes no ideal source to fight the shunt.
fn bind_shunt_ref(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
) -> (BindOutcome, Option<String>) {
    let r = &comp.reference;
    let p = |key: &str, default: f64| model.params.get_f64(key).unwrap_or(default);
    let anode = roles.get("gnd").copied().unwrap_or(NodeId::GROUND);
    let wired = |pin: Option<&NodeId>| pin.copied().filter(|&node| node != anode);
    let (Some(cathode), Some(refpin)) = (
        wired(roles.get("out")),
        wired(pick(roles, &["fb", "adj", "ref"])),
    ) else {
        return open_warning(
            comp,
            "shunt reference CATHODE or REF is open, or is tied to its ANODE: the internal \
             reference can then never be reached, so the part can never conduct and never \
             regulates",
        );
    };
    let (vref, zka, ikmax) = (p("vref", 2.495), p("zka_ohms", 0.2), p("ikmax_a", 0.1));
    let vr = circuit.node(&format!("{r}_shunt_vref"));
    circuit.add(Device::vsource(
        format!("Vint_{r}"),
        vr,
        anode,
        SourceKind::Dc(vref - p("iktest_a", 0.01) * zka),
    ));
    let gate = circuit.node(&format!("{r}_shunt_gate"));
    circuit.add(Device::OpAmp {
        name: r.clone(),
        out: gate,
        inp: refpin,
        inn: vr,
        reference: None,
        gain: 1.0 / (zka * ikmax),
        pole_hz: None,
        slew: None,
        rail_lo: Rail::fixed(0.0),
        rail_hi: Rail::fixed(1.0),
    });
    let kint = circuit.node(&format!("{r}_shunt_ik"));
    circuit.add(Device::Vccs {
        name: format!("{}{r}", crate::rail_owner::SHUNT_SINK_PREFIX),
        p: cathode,
        n: kint,
        cp: gate,
        cn: NodeId::GROUND,
        gm: ikmax,
    });
    circuit.add(Device::vsource(
        format!("Vreg_{r}"),
        kint,
        anode,
        SourceKind::Dc(0.0),
    ));
    circuit.add(Device::isource(
        format!("Iref_{r}"),
        refpin,
        anode,
        SourceKind::Dc(p("iref_a", 0.0)),
    ));
    (
        BindOutcome::Analog {
            device: format!("shunt reference {vref:.3}V (the board's divider sets the cathode)"),
        },
        // WHICH PADS the card's map took as each terminal, said next to the
        // result: this part's pad order is package-dependent, so a map that
        // disagrees with the fitted package is otherwise invisible behind a
        // confident voltage.
        entry_warning(comp, model).map(|w| {
            let pad = |want: &[&str]| {
                let hit = model.pins.iter().find(|(_, r)| want.contains(&r.as_str()));
                hit.map_or("(unmapped)", |(p, _)| p.as_str()).to_string()
            };
            let (cathode, anode) = (pad(&["out"]), pad(&["gnd"]));
            let reference = pad(&["fb", "adj", "ref"]);
            format!("{w}; pads taken as CATHODE {cathode} / REF {reference} / ANODE {anode}")
        }),
    )
}

/// The fully wired channels of a multi-channel package (LM358 dual, INA2181
/// dual, LM324 quad), as `(unit index, out, in+, in-)` for every complete
/// `out_X`/`in_plus_X`/`in_minus_X` triple. A channel-A-only lookup would leave
/// channel B/C/D outputs silently floating.
fn wired_channels(roles: &HashMap<String, NodeId>) -> Vec<(usize, NodeId, NodeId, NodeId)> {
    ["_a", "_b", "_c", "_d"]
        .iter()
        .enumerate()
        .filter_map(|(unit, sfx)| {
            Some((
                unit,
                roles.get(&format!("out{sfx}")).copied()?,
                roles.get(&format!("in_plus{sfx}")).copied()?,
                roles.get(&format!("in_minus{sfx}")).copied()?,
            ))
        })
        .collect()
}

/// The two output saturation levels for a card-bound op-amp, or the open-part
/// refusal when the card needs a supply pin the board does not give it: falling
/// back to the fixed `rail_hi` with V+ missing would answer a single-supply
/// saturation question with the part's ABSOLUTE-MAXIMUM supply (32 V on a 5 V
/// board) and call it clean, the very false-safe `swing_hi` exists to close.
/// Ground on V- is no refusal: it is a real rail, spelt as the offset alone.
fn supply_rails(
    comp: &Component,
    model: &ModelEntry,
    roles: &HashMap<String, NodeId>,
) -> Result<(Rail, Rail), (BindOutcome, Option<String>)> {
    let rail = |swing: &str, fixed: &str, dflt: f64, sign: f64, pins: &[&str]| {
        let Some(headroom) = model.params.get_f64(swing) else {
            return Ok(Rail::fixed(model.params.get_f64(fixed).unwrap_or(dflt)));
        };
        pick(roles, pins)
            .copied()
            .filter(|n| sign > 0.0 || !n.is_ground())
            .map(|pin| Rail { pin, offset: sign * headroom })
            .ok_or_else(|| open_warning(comp, "op-amp supply pin is not on a net, so the card's datasheet output swing has no rail to sit on"))
    };
    Ok((
        rail("swing_lo", "rail_lo", 0.0, 1.0, &["vss", "vee", "v_minus"])?,
        rail("swing_hi", "rail_hi", 5.0, -1.0, &["vcc", "vs", "vdd"])?,
    ))
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
    // A card's `swing_lo`/`swing_hi` bind the rails to the supply-pin NETS, so
    // the ceiling follows the real rail; `rail_lo`/`rail_hi` are the fallback.
    let (rail_lo, rail_hi) = match supply_rails(comp, model, roles) {
        Ok(rails) => rails,
        Err(open) => return open,
    };
    let warning = entry_warning(comp, model);

    // Per-unit names use the `_q<N>` key the CI thermal aggregation matches
    // (as bind_bjt does for paired BJTs).
    let channels = wired_channels(roles);
    let (units, device) = if channels.is_empty() {
        // Single-channel parts (INA181/186, generic role names): one device
        // under the bare reference, exactly as before.
        let out = pick(roles, &["out", "out_1"]).copied();
        let inp = pick(roles, &["in_plus", "inp", "in+"]).copied();
        let inn = pick(roles, &["in_minus", "inn", "in-"]).copied();
        let (Some(out), Some(inp), Some(inn)) = (out, inp, inn) else {
            return open_warning(comp, "opamp pins not all connected");
        };
        let single = vec![(comp.reference.clone(), out, inp, inn)];
        (single, "opamp".to_string())
    } else {
        let units = channels
            .iter()
            .map(|&(unit, out, inp, inn)| {
                (format!("{}_q{}", comp.reference, unit + 1), out, inp, inn)
            })
            .collect();
        (units, format!("opamp x{}", channels.len()))
    };
    for (name, out, inp, inn) in units {
        circuit.add(Device::OpAmp {
            name,
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
    }
    (BindOutcome::Behavioral { device }, warning)
}

/// One smoothed comparator, as a canonical B-source sub-expression: 0 while
/// `__d{a}` is below `__d{b}`, 1 above, over a transition of `vcc/gain` volts.
/// Dividing by VCC is what makes the trip a fixed FRACTION of the swing and
/// therefore the whole model supply-independent (an absolute gain measured
/// clean at 5 V and broke into a limit cycle at 15 V); the `max(.., 1.0)` floor
/// is for the DC homotopy, which walks VCC down through zero.
fn timer_cmp(a: &str, b: &str, vcc: &str, gain: f64) -> String {
    format!("0.5*(1.0+math::tanh({gain}*({a}-{b})/max({vcc},1.0)))")
}

/// A 555 is not a formula: it is the datasheet block diagram stamped out of
/// primitives the solver already has, so the BOARD's RA/RB/C set the interval
/// by the SOLVE. Eight devices: the internal 5k/5k/5k divider as three Resistors,
/// both comparators and the RESET gate folded into ONE Behavioral control node
/// `ctl`, and one hysteretic [`Device::VSwitch`] that is at once the SR LATCH
/// (its own hysteresis IS the latch memory -- there is no latch device) and the
/// discharge transistor. A second `VSwitch` on the same control plus a pull-up exposes the latch state as
/// a node `qb`, which both closes the latch's own feedback (see `ctl_expr`) and
/// drives the output stage: a Behavioral source behind a series R, so VOL/VOH
/// move under load.
///
/// The two legs are named `_disch` and `_q`, never `_s0`/`_s1`:
/// `SpdtPairs::analyze` pairs `VSwitch` legs by that suffix and applies
/// break-before-make to them, and these two must be free to close together.
///
/// What this owes: the interval is solved, not printed, so the paired ngspice
/// deck (`hauksbee-solve/tests/decks/ne555_astable.*`) is what pins the latch
/// band and the comparator gain; a wiring the part cannot work from REFUSES
/// with a named reason rather than reporting a fabricated frequency.
#[allow(clippy::too_many_lines)] // one stamp of the documented 555 macro-model
fn bind_timer_555(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
) -> (BindOutcome, Option<String>) {
    let r = &comp.reference;
    let p = |name: &str, fallback: f64| model.params.get_f64(name).unwrap_or(fallback);
    let Some(vcc) = roles.get("vcc").copied().filter(|n| !n.is_ground()) else {
        return open_warning(comp, "555 VCC not connected");
    };
    let gnd = roles.get("gnd").copied().unwrap_or(NodeId::GROUND);
    let Some(thres) = roles.get("thres").copied() else {
        return open_warning(
            comp,
            "555 THRES not connected: there is no timing node to solve",
        );
    };
    let Some(trig) = roles.get("trig").copied() else {
        return open_warning(comp, "555 TRIG not connected");
    };
    let (out, disch) = (roles.get("out").copied(), roles.get("disch").copied());
    if out.is_none() && disch.is_none() {
        return open_warning(
            comp,
            "555 has neither OUT nor DISCH connected: nothing it does is observable",
        );
    }
    // A RESET pin left unconnected reads as RELEASED (tied to VCC), which is
    // what the datasheet tells a board to do with it; a genuinely floating
    // RESET is a hardware defect this model does not represent (card note).
    let reset = roles.get("reset").copied().unwrap_or(vcc);
    // The internal divider, as the two FRACTIONS rather than three equal legs:
    // a CMOS-555 card overriding `divider_ohms` keeps the same trip points.
    let (th, tr) = (p("threshold_frac", 2.0 / 3.0), p("trigger_frac", 1.0 / 3.0));
    let leg = 3.0 * p("divider_ohms", 5000.0);
    // CTRL wired (the usual 10 nF bypass, or an external reference) uses the
    // board's own node, so the board's voltage moves BOTH thresholds together.
    let ctrl = roles
        .get("ctrl")
        .copied()
        .unwrap_or_else(|| circuit.node(&format!("{r}_555_ctrl")));
    let tref = circuit.node(&format!("{r}_555_tref"));
    for (a, b, ohms) in [
        (vcc, ctrl, leg * (1.0 - th)),
        (ctrl, tref, leg * (th - tr)),
        (tref, gnd, leg * tr),
    ] {
        circuit.add(Device::resistor(
            format!("Rdiv_{r}_{}", circuit.node_name(b)),
            a,
            b,
            ohms,
        ));
    }

    let gain = p("comparator_gain", 1200.0);
    let hold = p("latch_hold", 0.3);
    let qb = circuit.node(&format!("{r}_555_q"));
    // ctl = hold + (1-hold)*(max(Rt, 1-Rstn) + Q) - St*Rstn. Neither comparator
    // leaves it at `hold`, inside the relay's band, so the latch HOLDS;
    // THRES sets it to 1 (>= von), TRIG drives it to hold-1 (<= voff), and
    // both together read as TRIG, which is the datasheet's precedence. RESET
    // (Rstn -> 0) forces the set branch and outranks everything.
    //
    // `Q = 1 - qb/VCC` is the latch's OWN state fed back into its control, which
    // is what makes this an SR latch rather than a high-gain loop: a relay's
    // conductance RAMPS over a band past its threshold, and comparators alone
    // leave a stable root inside that ramp where partial discharge cancels RA
    // and the part never oscillates. ADDED, not max()-ed in: at that root Rt
    // already exceeds Q, so a max() gates the feedback out (measured both ways).
    let rstn = timer_cmp(
        "__d3",
        &p("reset_threshold_v", 0.7).to_string(),
        "__d2",
        gain,
    );
    let ctl_expr = format!(
        "{hold} + {}*(max({}, 1.0-{rstn}) + 1.0-__d6/max(__d2,1.0)) - {}*{rstn}",
        1.0 - hold,
        timer_cmp("__d0", "__d1", "__d2", gain),
        timer_cmp("__d4", "__d5", "__d2", gain),
    );
    let ctl_node = circuit.node(&format!("{r}_555_ctl"));
    let deps = [thres, ctrl, vcc, reset, tref, trig, qb];
    let control = Device::behavioral(
        format!("Bctl_{r}"),
        (ctl_node, gnd),
        BOutput::Voltage,
        &ctl_expr,
        &deps,
    );
    circuit.add(control.expect("555 control expression"));
    let (von, voff) = (p("latch_von", 0.75), p("latch_voff", 0.25));
    let pullup = p("sense_ohms", 10_000.0);
    let leg_of = |name: &str, a: NodeId, ron: f64| Device::VSwitch {
        name: format!("{r}_555{name}"),
        a,
        b: gnd,
        ctrl_p: ctl_node,
        ctrl_n: gnd,
        von,
        voff,
        ron,
        roff: 1e9,
    };
    // The sense leg alone carries the latch, so a board that leaves DISCH open
    // (a monostable driving only OUT) still latches.
    if let Some(disch) = disch {
        let d = leg_of("_disch", disch, p("rsat_ohms", 12.0));
        circuit.add(d);
    }
    let q = leg_of("_q", qb, pullup * 0.01);
    circuit.add(q);
    circuit.add(Device::resistor(format!("Rq_{r}"), vcc, qb, pullup));
    if let Some(out) = out {
        let (vol, drop) = (p("vol_v", 0.1), p("voh_drop_v", 1.4));
        let outi = circuit.node(&format!("{r}_555_outi"));
        // Gain 20, not the comparators' 1200: the latch node this reads
        // already swings rail to rail in ONE step, and a 1200 cliff on top
        // of that is what Newton could not cross. 20 still saturates.
        let expr = format!(
            "{vol} + (__d1-{drop}-{vol})*{}",
            timer_cmp("__d0", "0.5*__d1", "__d1", 20.0)
        );
        let buf = Device::behavioral(
            format!("Bout_{r}"),
            (outi, gnd),
            BOutput::Voltage,
            &expr,
            &[qb, vcc],
        );
        circuit.add(buf.expect("555 output expression"));
        circuit.add(Device::resistor(
            format!("Rout_{r}"),
            outi,
            out,
            p("out_ohms", 10.0),
        ));
    }
    (
        BindOutcome::Analog {
            device: "555 timer (hysteretic-switch SR latch)".to_string(),
        },
        entry_warning(comp, model),
    )
}

/// One section of an OPEN-COLLECTOR comparator (LM393/LM339 class), as two
/// devices the IR already has rather than a new `Device` variant: a
/// [`Device::OpAmp`] railed `0..1` as the gate (inputs deliberately SWAPPED so
/// gate = 1 means SINKING, which is what an inverting open-collector stage
/// does; `gain` is the card's smoothing scale, not the datasheet AVD), and a
/// [`Device::Behavioral`] current sink on the output pin as the whole output
/// stage: `I = gate*max(V(out),0)/RON + V(out)*GLEAK`, two datasheet numbers
/// and nothing else. Nothing drives the high rail, so the BOARD's pull-up sets
/// the high level, two sections can wire-OR onto one net, and no pull-up leaves
/// the output floating -- all of which a push-pull [`Device::Comparator`] gets
/// wrong. Hysteresis is likewise the BOARD's feedback resistor, solved.
fn bind_open_collector(
    circuit: &mut Circuit,
    name: &str,
    (out, inp, inn, gnd): (NodeId, NodeId, NodeId, NodeId),
    model: &ModelEntry,
) {
    let p = |k: &str, d: f64| model.params.get_f64(k).unwrap_or(d);
    let gate = circuit.node(&format!("{name}_oc_gate"));
    circuit.add(Device::OpAmp {
        name: name.to_string(),
        out: gate,
        inp: inn,
        inn: inp,
        reference: None,
        gain: p("gain", 1000.0),
        pole_hz: None,
        slew: None,
        rail_lo: Rail::fixed(0.0),
        rail_hi: Rail::fixed(1.0),
    });
    // `max(V(out), 0)` keeps the sink one-directional: the DC homotopy walks
    // the output node below ground on its way in, and an unclamped V(out)/RON
    // there would SOURCE current and run away. There is deliberately NO
    // `min(.., IOL)` ceiling: a capped sink is FLAT in V(out) above the cap,
    // contributing NO conductance to the output row, and Newton then crawls and
    // times out at the crossing (measured). RON reproduces the datasheet's VOL
    // test point; the ceiling is declared not modelled on the card instead.
    let expr = format!(
        "__d0*max(__d1,0.0)/{} + __d1*{}",
        p("ron_ohms", 32.5),
        p("leak_s", 2e-11),
    );
    let sink = Device::behavioral(
        format!("Boc_{name}"),
        (out, gnd),
        BOutput::Current,
        &expr,
        &[gate, out],
    );
    circuit.add(sink.expect("open-collector sink expression"));
}

fn bind_comparator(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
) -> (BindOutcome, Option<String>) {
    let out_lo = model.params.get_f64("out_lo").unwrap_or(0.0);
    let out_hi = model.params.get_f64("out_hi").unwrap_or(5.0);
    // Full-band VHYS (see `Device::Comparator`); validation requires it on checked cards.
    let hyst = model.params.get_f64("hysteresis").unwrap_or(0.01);

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
            circuit.add(Device::isource(
                format!("Iq_{}", comp.reference),
                vcc,
                gnd,
                SourceKind::Dc(static_ua * 1e-6),
            ));
        }
    }

    // Multi-channel packages (LM393 dual, LM339 quad): one section per wired
    // channel, keyed `_q<N>`, same shape and rationale as bind_opamp above. A
    // section whose three pins are not all connected is not bound at all, so
    // an unused half leaves no device and no floating internal node behind.
    let channels = wired_channels(roles);
    let sections: Vec<(String, NodeId, NodeId, NodeId)> = if channels.is_empty() {
        let out = pick(roles, &["out", "out_1", "q"]).copied();
        let inp = pick(roles, &["in_plus", "inp", "in+"]).copied();
        let inn = pick(roles, &["in_minus", "inn", "in-"]).copied();
        let (Some(out), Some(inp), Some(inn)) = (out, inp, inn) else {
            return open_warning(comp, "comparator pins not all connected");
        };
        vec![(comp.reference.clone(), out, inp, inn)]
    } else {
        channels
            .iter()
            .map(|&(unit, out, inp, inn)| {
                (format!("{}_q{}", comp.reference, unit + 1), out, inp, inn)
            })
            .collect()
    };
    // `open_collector` picks the two-device stage (see `bind_open_collector`);
    // a push-pull part keeps the single `Device::Comparator` stamp unchanged.
    let open_collector = model.params.get_bool("open_collector").unwrap_or(false);
    let gnd = pick(roles, &["vss", "gnd"])
        .copied()
        .unwrap_or(NodeId::GROUND);
    for (name, out, inp, inn) in &sections {
        if open_collector {
            bind_open_collector(circuit, name, (*out, *inp, *inn, gnd), model);
        } else {
            circuit.add(Device::Comparator {
                name: name.clone(),
                out: *out,
                inp: *inp,
                inn: *inn,
                out_lo,
                out_hi,
                hysteresis: hyst,
            });
        }
    }
    let device = match (open_collector, channels.is_empty()) {
        (true, true) => "open-collector comparator".to_string(),
        (true, false) => format!("open-collector comparator x{}", sections.len()),
        (false, true) => "comparator".to_string(),
        (false, false) => format!("comparator x{}", sections.len()),
    };
    (
        BindOutcome::Behavioral { device },
        entry_warning(comp, model),
    )
}

/// Stamp one SPDT as two complementary `VSwitch` legs named `{name}_s{offset+1}`
/// and `{name}_s{offset}`. Select high closes com<->s1; the s0 leg conducts when the
/// select is LOW by sensing (vcc - select) against the rail voltage `vcc_v`
/// (or, passed vss as `vcc` and 0 as `vcc_v`, (vss - select) against -vth).
/// Each leg takes its own on-resistance (`[ron_s0, ron_s1]`).
fn stamp_spdt(
    circuit: &mut Circuit,
    name: &str,
    offset: u8,
    [com, s0, s1]: [NodeId; 3],
    [sel, vss, vcc]: [NodeId; 3],
    vcc_v: f64,
    ([ron_s0, ron_s1], roff, vth): ([f64; 2], f64, f64),
) {
    for (leg, b, ctrl_p, ctrl_n, von, ron) in [
        (offset + 1, s1, sel, vss, vth, ron_s1),
        (offset, s0, vcc, sel, vcc_v - vth, ron_s0),
    ] {
        circuit.add(Device::VSwitch {
            name: format!("{name}_s{leg}"),
            a: com,
            b,
            ctrl_p,
            ctrl_n,
            von: von + 0.25,
            voff: von - 0.25,
            ron,
            roff,
        });
    }
}

#[allow(clippy::too_many_lines)] // one arm per switch topology, in precedence order
fn bind_analog_switch(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
    power_nets: &HashMap<String, f64>,
) -> (BindOutcome, Option<String>) {
    // Shared-select DPDT parts (USB differential-pair muxes, dual audio
    // switches): two independent common/throw groups selected together. A
    // single-SPDT binder silently handles only one lane; the other differential
    // conductor then stays OPEN even though the report calls the package
    // resolved. The numbered roles make the topology explicit and reusable.
    let vss = pick(roles, &["vss", "gnd"])
        .copied()
        .unwrap_or(NodeId::GROUND);
    let ron = model.params.get_f64("ron").unwrap_or(50.0);
    let roff = model.params.get_f64("roff").unwrap_or(1e9);
    let vth = model.params.get_f64("vth").unwrap_or(1.5);
    // A part printing on-resistance per path gives each throw its own figure.
    let legs = ["ron_s0", "ron_s1"].map(|k| model.params.get_f64(k).unwrap_or(ron));
    let shared_dpdt = (
        roles.get("com_1").copied(),
        roles.get("s0_1").copied(),
        roles.get("s1_1").copied(),
        roles.get("com_2").copied(),
        roles.get("s0_2").copied(),
        roles.get("s1_2").copied(),
        pick(roles, &["ctrl", "sel", "s"]).copied(),
        roles.get("vcc").copied(),
    );
    if let (
        Some(com_1),
        Some(s0_1),
        Some(s1_1),
        Some(com_2),
        Some(s0_2),
        Some(s1_2),
        Some(sel),
        Some(vcc),
    ) = shared_dpdt
    {
        // A VSwitch has one control. It can model this package exactly when
        // active-low OE is strapped on, as on many boards. If OE is dynamic,
        // fail closed instead of pretending the mux can never be disabled.
        if roles
            .get("oe_n")
            .or_else(|| roles.get("enable_n"))
            .is_some_and(|node| *node != vss)
        {
            return open_warning(
                comp,
                "shared-select DPDT has a dynamic OE_N input; the current switch primitive has one control and cannot combine select with output-enable, so both lanes are left open rather than modeled as always enabled",
            );
        }
        let rail = power_nets.get(circuit.node_name(vcc)).copied();
        let vcc_warning = rail.is_none().then(|| {
            format!(
                "{} ({}): shared-select DPDT VCC net '{}' has no resolved rail voltage; modeling select thresholds against an assumed {DEFAULT_VCC} V rail",
                comp.reference,
                comp.value,
                circuit.node_name(vcc),
            )
        });
        let vcc_v = rail.unwrap_or(DEFAULT_VCC);
        // Flat `_s<N>` keys keep both lanes in one physical package for stress,
        // ratings and CI; lane 1 owns s0/s1, lane 2 owns s2/s3.
        for (offset, com, s0, s1) in [(0, com_1, s0_1, s1_1), (2, com_2, s0_2, s1_2)] {
            stamp_spdt(
                circuit,
                &comp.reference,
                offset,
                [com, s0, s1],
                [sel, vss, vcc],
                vcc_v,
                (legs, roff, vth),
            );
        }
        let warning = [entry_warning(comp, model), vcc_warning]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("; ");
        return (
            BindOutcome::Behavioral {
                device: "shared-select dpdt".to_string(),
            },
            (!warning.is_empty()).then_some(warning),
        );
    }

    // True SPDT when both throws are wired (com + s0 + s1 + ctrl): two
    // complementary VSwitch legs. select low -> com<->s0, select high ->
    // com<->s1 (the SN74LVC1G3157 convention). The s0 leg senses the
    // INVERTED control by measuring (vcc - select), or (vss - select) against
    // -vth on a part with no vcc pin (a power mux, powered from its inputs).
    let com = pick(roles, &["com", "a"]).copied();
    let s0 = pick(roles, &["s0", "b1"]).copied();
    let s1 = pick(roles, &["s1", "b2"]).copied();
    let sel = pick(roles, &["ctrl", "s", "in"]).copied();
    if let (Some(com), Some(s0), Some(s1), Some(sel)) = (com, s0, s1, sel) {
        let vcc = pick(roles, &["vcc"]).copied();
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
        let rail = vcc.and_then(|vcc| power_nets.get(circuit.node_name(vcc)).copied());
        let vcc_warning = vcc.filter(|_| rail.is_none()).map(|vcc| {
            format!(
                "{} ({}): analog-switch VCC net '{}' has no resolved rail voltage; \
                 modeling its SPDT thresholds against an assumed {DEFAULT_VCC} V rail; \
                 on a lower-voltage board the common<->s0 throw may read as open, \
                 so verify the switch's actual supply",
                comp.reference,
                comp.value,
                circuit.node_name(vcc),
            )
        });
        // No vcc pin: the s0 leg senses (vss - select) against -vth instead.
        let (rail_node, vcc_v) = vcc.map_or((vss, 0.0), |vcc| (vcc, rail.unwrap_or(DEFAULT_VCC)));
        stamp_spdt(
            circuit,
            &comp.reference,
            0,
            [com, s0, s1],
            [sel, vss, rail_node],
            vcc_v,
            (legs, roff, vth),
        );
        return (
            BindOutcome::Analog {
                device: "spdt x2".to_string(),
            },
            vcc.map_or_else(|| entry_warning(comp, model), |_| vcc_warning),
        );
    }

    // Multi-gate bilateral packages (CD74HC4066 quad): numbered gate roles
    // in_out_<n>a / in_out_<n>b switched by ctrl_<n>, each gate electrically
    // independent. Stamp one VSwitch per complete gate; a single-SPST
    // fall-through would bind gate 1 only and silently drop gates 2..4. Per-unit
    // names use the `_s<n>` key the CI thermal aggregation matches (the same
    // family as the SPDT's `_s0`/`_s1` legs above).
    {
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
            let warning = (!partial.is_empty()).then(|| {
                let gates = partial.iter().map(ToString::to_string).collect::<Vec<_>>();
                format!(
                    "{}: analog-switch gate(s) {} missing a connection, left open",
                    comp.reference,
                    gates.join(", ")
                )
            });
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
            // AN ENTRY THAT NAMES ITS TERMINALS IS BELIEVED ABOUT WHICH PADS THEY
            // ARE. If the map declares `in_out_a`/`in_out_b` (or `com`/`s0`) and the
            // pair did not resolve, the board did not connect one of them, and the
            // honest answer is an open switch. Scanning for two other pads instead
            // invents a conducting path between terminals the datasheet says are not
            // a switch at all.
            //
            // The PCA9306 shows why: its map names SCL2 `in_out_a` and SCL1
            // `in_out_b`, and its other pads (VREF1, VREF2, SDA1, SDA2) are all
            // candidates for a blind two-pad scan, since `is_power_role` matches
            // only vcc/vdd/vss/gnd. A board leaving one SCL pad unconnected would
            // get 25.5 Ω stamped from VREF1 onto SDA1, a path that exists on no
            // board, while the entry's own disclosure named SCL1<->SCL2.
            let names_its_terminals = model.pins.values().any(|r| {
                matches!(
                    r.as_str(),
                    "com" | "s0" | "s1" | "in_out_a" | "in_out_b" | "in_out_1a" | "in_out_1b"
                )
            });
            if names_its_terminals {
                return open_warning(
                    comp,
                    "analog switch terminal not connected: the model names which pads \
                     are the switched path and one of them has no net, so no path is \
                     stamped rather than guessing which other pads to bridge",
                );
            }
            // Only for an entry with NO named terminals, where the roles came from
            // the pin-rule table and there is nothing better to go on. The control
            // role is EXCLUDED: it is the switch's gate, not a signal terminal.
            // Wiring it as a throw would stamp a VSwitch whose `b` equals its own
            // `ctrl_p`, fabricating a ~ron path that shorts a signal net to the
            // control line (and injects/loads the control voltage) when the gate goes
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
    let Some(ctrl) = ctrl else {
        return open_warning(comp, "analog switch control not connected");
    };
    // The `s0` / NC throw conducts when the control is LOW (role_from_pinfunction
    // maps nc->s0 with exactly this contract, and the true-SPDT branch honours it).
    // The default control-HIGH polarity below would invert it, modelling the
    // contact open exactly when the real one is closed. When `b` is the s0 net,
    // sense the inverted control (vss - ctrl) so com<->s0 closes on control LOW.
    let b_is_control_low = pick(roles, &["s0"]).copied() == Some(b);
    let (cp, cn, von, voff) = if b_is_control_low {
        (vss, ctrl, -vth + 0.1, -vth - 0.1)
    } else {
        (ctrl, vss, vth + 0.1, vth - 0.1)
    };
    circuit.add(Device::VSwitch {
        name: comp.reference.clone(),
        a,
        b,
        ctrl_p: cp,
        ctrl_n: cn,
        von,
        voff,
        ron: if b_is_control_low { legs[0] } else { ron },
        roff,
    });
    // An entry's own `warning` reaches the report from here. This path stamps
    // ONE switch, so a multi-channel part (TXS0108E: eight channels, PCA9306:
    // two) still has a coverage gap that has to be disclosed
    // where a report surface reads it rather than in a TOML comment.
    (
        BindOutcome::Behavioral {
            device: "vswitch".to_string(),
        },
        entry_warning(comp, model),
    )
}

/// Bind an MCP4728-class quad I2C DAC: stamp a low-impedance Thevenin
/// [`PinDriver`] on each connected VOUT channel net (ROUT ~1 Ω per the
/// datasheet) and push a [`DacBinding`] the scheduler turns into an I2C slave.
/// The address is assigned later by reference order; we leave a placeholder.
// `gain` is a rounded model param (1 or 2); `as` saturates anything out of range.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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
/// Swallowing the error while still reporting the part as bound would show a
/// part that cannot possibly work as a healthy `Digital` row in the bind
/// coverage report.
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
///     (`RESET_SR`), that gate's output is Qb (internal);
///   - the OTHER gate is the Q gate: its non-cross input is `set` (SPIKE<n>),
///     its output net is `q` (the observable L<n>, wired to the 165 inputs).
///
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
    let levels = LogicLevels::from_params(model).on_supply(model, roles);
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
            let i_is_reset =
                net_name(in_i).is_some_and(|s| s.to_ascii_uppercase().contains("RESET"));
            let j_is_reset =
                net_name(in_j).is_some_and(|s| s.to_ascii_uppercase().contains("RESET"));
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
            let (Some(set_node), Some(reset_node)) = (set_in, reset_in) else {
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
            lroles.insert("set".to_string(), set_node);
            lroles.insert("reset".to_string(), reset_node);
            lroles.insert("q".to_string(), q_net);
            digital.push(DigitalComponent::new_nor_latch(
                format!("{}_L{}", comp.reference, gates[q_gate].idx),
                levels.clone(),
                lroles,
                HashMap::from([("q".to_string(), drv)]),
            ));
            used.insert(i);
            used.insert(j);
            bound += 1;
            break;
        }
    }
    bound > 0
}

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
    let derived_when_empty = model.pins.is_empty().then(|| derive_mcu_pin_roles(comp));
    // Merge the two role sources PER PAD instead of treating a non-empty model
    // map as exhaustive. A DB pin map is curated for one package's numbering
    // (often a module, e.g. the ESP32-S3 entry's WROOM-1 strap pads); applied to
    // a different footprint it covers a handful of pads, and a model-only rule
    // would then discard every OTHER pad's own pinfunction, leaving pins the
    // board names explicitly with no GPIO driver at all. Model roles still win
    // on pads they name (they carry curated semantic suffixes like "pc6_reset"
    // that the plain pinfunction derivation would weaken); the derivation only
    // fills pads the model map does not cover.
    let effective_pins: std::collections::BTreeMap<String, String> = {
        let mut merged = derived_when_empty
            .as_ref()
            .map_or_else(|| derive_mcu_pin_roles(comp).roles, |d| d.roles.clone());
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
        external_clock_present: false,
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

/// Whether a populated crystal/resonator bridges two MCU oscillator nodes.
///
/// Recognition reuses the binder's existing crystal evidence and the model
/// library's footprint rules. Connectivity requires both oscillator nodes on
/// the same component, so unrelated crystals elsewhere on the board do not
/// enable this MCU's clock.
fn assembled_crystal_bridges(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    osc_in: NodeId,
    osc_out: NodeId,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> bool {
    if osc_in == osc_out {
        return false;
    }
    board.components.iter().any(|comp| {
        // Only an assembled part with an established identity can supply the
        // MCU's external clock: DNP and identity-refused records are absent
        // or unprovable, and must not enable HSE.
        let Some(part) = AssemblyState::of(comp).fitted() else {
            return false;
        };
        let prefix = ref_prefix(&comp.reference);
        let model_is_crystal = resolve(lib, part)
            .model
            .as_ref()
            .is_some_and(|m| m.id.starts_with("crystal"));
        if !is_crystal_like(&prefix, &comp.value) && !model_is_crystal {
            return false;
        }
        let mut has_in = false;
        let mut has_out = false;
        for pin in &comp.pins {
            match node_of(pin.net) {
                Some(node) if node == osc_in => has_in = true,
                Some(node) if node == osc_out => has_out = true,
                _ => {}
            }
        }
        has_in && has_out
    })
}

// The pin counts were written by the family router as small non-negative integers.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn log_mcu_auto_decision(
    comp: &Component,
    model: &ModelEntry,
    derived_when_empty: Option<&DerivedMcuPins>,
) -> Option<String> {
    let backend = mcu_backend_string(comp, model);
    let auto_router = model.params.get_str("auto_bind") == Some("family_router");
    if auto_router {
        let family = model.params.get_str("auto_bind_family").unwrap_or("MCU");
        let count = |key| model.params.get_f64(key).unwrap_or(0.0) as usize;
        let (named, derived) = (
            count("auto_bind_pin_names"),
            count("auto_bind_derived_pins"),
        );
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
        || r == "oe_n"
        || r == "enable_n"
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
    if value == 0.0 || !value.is_finite() {
        return format!("0 {unit}");
    }
    let neg = value < 0.0;
    let v = value.abs();
    // Pick the largest prefix whose scale leaves a mantissa >= 1 (so 390 pF uses
    // "p", 1.5 kΩ uses "k"). Fall back to the smallest prefix for tiny values.
    let idx = PREFIXES
        .iter()
        .position(|(s, _)| v >= *s)
        .unwrap_or(PREFIXES.len() - 1);
    let (mut scale, mut prefix) = PREFIXES[idx];
    let mut mantissa = v / scale;
    // 3 significant figures: more decimals for small mantissas, fewer for large.
    let decimals = |m: f64| -> u8 {
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
    let round_to = |m: f64, d: u8| {
        let p = 10f64.powi(i32::from(d));
        (m * p).round() / p
    };
    if idx > 0 && round_to(mantissa, decimals(mantissa)) >= 1000.0 {
        (scale, prefix) = PREFIXES[idx - 1];
        mantissa = v / scale;
    }
    let s = format!("{mantissa:.*}", usize::from(decimals(mantissa)));
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

/// Map an `ATmega328P` / Arduino-Nano role string to a `(port, bit)` GPIO id.
#[must_use]
pub fn gpio_of_role(role: &str, module: bool) -> Option<(char, u8)> {
    let r = role.to_ascii_lowercase();
    if module {
        // Arduino Nano header: d0..d13 map to Arduino digital pins.
        // d0=PD0 .. d7=PD7, d8=PB0 .. d13=PB5.
        //
        // This `d<n>` table is the NANO's, not "every module's": the same
        // silkscreened D5 is PC6 on a SparkFun Pro Micro and PD5 on a Nano, so a
        // module entry whose header numbers do not follow the Nano's ATmega328P
        // mapping must NOT use `d<n>` role names. Those entries name the port
        // pin directly ("pc6", "pf7_adc7") and fall through to the standard
        // parser below, which is why this arm no longer swallows every role on a
        // module: returning None here silently unbound every Pro Micro pin.
        if let Some(rest) = r.strip_prefix('d') {
            if let Ok(num) = rest.split('_').next().unwrap_or("").parse::<u8>() {
                return arduino_digital_to_port(num);
            }
        }
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
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
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
    let rest = r.strip_prefix('p')?;
    let port = rest.chars().next()?.to_ascii_uppercase();
    // Lettered ports reach I on larger STM32s; numeric ports are banks 0 and 1.
    if !matches!(port, 'A'..='I' | '0' | '1') {
        return None;
    }
    let digits: String = rest[1..].chars().take_while(char::is_ascii_digit).collect();
    let bit = digits.parse::<u8>().ok()?;
    (bit < 32).then_some((port, bit))
}

/// Arduino digital pin number -> `ATmega328P` (port, bit).
const fn arduino_digital_to_port(num: u8) -> Option<(char, u8)> {
    match num {
        0..=7 => Some(('D', num)),
        8..=13 => Some(('B', num - 8)),
        _ => None,
    }
}

/// Map a dual-purpose analog-pin role to its GPIO `(port, bit)`, so `bind_mcu`
/// can stamp the tri-stated driver half of the dual ADC+GPIO bind (dynamic
/// promotion).
///
/// On the `ATmega328P` the analog pins A0..A5 are port C bits 0..5 (PC0..PC5);
/// A6/A7 (Nano modules) are ADC-only with no port pin and return `None`, so
/// they stay pure ADC probes. Handles both the Nano module role ("a2",
/// "a3_...") and the bare role carrying an adc index ("`pc2_adc2`").
#[must_use]
pub fn apin_gpio_of_role(role: &str, module: bool) -> Option<(char, u8)> {
    let r = role.to_ascii_lowercase();
    // As in `gpio_of_role`, the bare `a<n>` table is the NANO's. A module whose
    // header analog pins are not the ATmega328P's port C names its pins by port
    // instead, and those fall through to the standard parser.
    if module {
        if let Some(rest) = r.strip_prefix('a') {
            if let Ok(n) = rest.split('_').next().unwrap_or("").parse::<u8>() {
                // A0..A5 = PC0..PC5; A6/A7 have no digital port pin.
                return (n <= 5).then_some(('C', n));
            }
        }
    }
    // Bare role like "pc2_adc2": let the standard parser recover the port pin.
    gpio_of_role(role, false)
}

/// Map a role string to an ADC channel number, if it is an analog input.
fn adc_of_role(role: &str, module: bool) -> Option<u8> {
    let r = role.to_ascii_lowercase();
    if module {
        // Nano "a0".."a7". A module that names its pins by port instead
        // ("pf7_adc7" on a Pro Micro) falls through to the bare form below,
        // which reads the channel number out of the role.
        if let Some(rest) = r.strip_prefix('a') {
            if let Ok(n) = rest.split('_').next().unwrap_or("").parse::<u8>() {
                return (n <= 7).then_some(n);
            }
        }
    }
    // Bare "pc0_adc0" .. "pc5_adc5".
    if let Some(idx) = r.find("adc") {
        let after = &r[idx + 3..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
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
/// `CHASSIS_GND` are real, distinct nets whose whole point is that they are NOT
/// the same copper as GND until something (a ferrite bead, a 0 Ω link, a star
/// point) joins them. Folding them all onto node 0 before binding turned every
/// such bridge into an inert self-loop and erased the board's ground topology,
/// the split-ground / galvanic-isolation structure this tool exists to check.
/// Rail-default and rating heuristics keep the broad [`is_ground`]; only the
/// pass-1 node assignment uses this.
///
/// `VSS` IS canonical ground: it is the IC-pin spelling of "the" ground (`KiCad`'s
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
#[must_use]
pub fn is_ground(name: &str) -> bool {
    let n = name.trim().trim_start_matches('/').to_ascii_uppercase();
    matches!(
        n.as_str(),
        "GND" | "GNDA" | "GNDD" | "AGND" | "DGND" | "VSS" | "0" | "VEE"
    ) || n.ends_with("GND")
}

/// If `name` is a recognised supply rail, return its nominal voltage.
#[must_use]
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
            let supply_token =
                n.starts_with('+') || n.contains("VCC") || n.contains("VBUS") || n.contains("VDD");
            negative_rail_fallback(&n)
                .or_else(|| positive_rail_fallback(&n))
                // A supply-token-prefixed rail carries its magnitude in an embedded
                // voltage token ("VCC_5V", "VDD_3V3", "VDD_1V8", "VCC_15V",
                // "VBUS_25V"). This precise extraction MUST run before the loose
                // "contains 5V"/"3V3" substring heuristics below: "VCC_15V" contains
                // the substring "5V" and "VDD_13V3" contains "3V3", so the substring
                // branches misread them as 5 V / 3.3 V. embedded_rail_magnitude reads
                // the whole "15V"/"13V3" digit run and returns 15.0 / 13.3.
                .or_else(|| embedded_rail_magnitude(&n))
                // Fallback for voltage-suffixed names embedded_rail_magnitude cannot
                // parse a numeric token from but that still carry a "5V"/"3V3"
                // substring next to a supply token (e.g. "VCC_5VOLTS").
                //
                // Same gate for the 3.3 V substring fallback: a bare domain-suffixed
                // SIGNAL net (an open-drain data line "SDA_3V3", a monitor "SENSE_3V3_MON",
                // an interrupt "IRQ_3.3V") is NOT a rail and must stay unresolved, or
                // Pass 3 stamps an ideal 3.3 V supply onto it and pins the line high.
                // Genuine rails still resolve: exact names hit the match arm above,
                // token-prefixed forms (VDD_3V3) via embedded_rail_magnitude, numeric
                // forms (+3V3_ANALOG) via positive_rail_fallback.
                .or_else(|| {
                    if supply_token && n.contains("5V") {
                        Some(5.0)
                    } else if supply_token && (n.contains("3V3") || n.contains("3.3V")) {
                        Some(3.3)
                    } else {
                        None
                    }
                })
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
#[must_use]
pub fn names_a_supply_of_unknown_voltage(name: &str) -> bool {
    const SUPPLY_TOKENS: [&str; 12] = [
        "VDD", "VCC", "VEE", "VBAT", "VBUS", "VSYS", "VIN", "AVDD", "AVCC", "DVDD", "VREG", "VAUX",
    ];
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
    SUPPLY_TOKENS.iter().any(|t| leaf.contains(t))
}

/// How many parts must hang off a supply-named net before its silence counts as
/// a dead rail rather than an oddly-named signal. An enable called `VCC_EN`
/// reaches a handful of pins; a rail reaches the board.
const DEAD_RAIL_MIN_PARTS: usize = 6;

/// Nets that name themselves a supply, carry a rail's worth of parts, and are
/// powered by nothing.
///
/// The binder refuses to guess a voltage for `ANALOG_VDD` or a bare `VDD`,
/// which is right: inventing one would over- or under-drive every part on the
/// net. But the net then sits at 0 V, the operating point around it is fiction,
/// and the stress monitor reports on that fiction with the same confidence it
/// reports a real overload -- a named accusation against a specific part, from
/// a run where the rails were dead. Naming the dead rails is what lets a reader
/// tell that report from a true one.
///
/// Returns the net names, ordered, deduped.
#[must_use]
pub fn unpowered_supply_nets(
    board: &ExtractedBoard,
    bound: &BoundBoard,
    also_powered: &[&str],
) -> Vec<String> {
    let mut powered: std::collections::BTreeSet<&str> = bound
        .supplies
        .iter()
        .map(|leg| leg.net_name.as_str())
        .collect();
    powered.extend(also_powered.iter().copied());

    // Distinct parts per net id, so a two-pin device straddling the net counts
    // once and a bypass-capacitor farm counts as the many parts it is.
    let mut parts_on: HashMap<i64, std::collections::BTreeSet<&str>> = HashMap::new();
    for comp in &board.components {
        for pin in &comp.pins {
            if let Some(id) = pin.net {
                parts_on
                    .entry(id)
                    .or_default()
                    .insert(comp.reference.as_str());
            }
        }
    }

    // A net a part drives (a regulator's output, a converter's rail, a switched
    // branch of either) or that a series conductor ties to a powered leg is
    // powered too: naming it dead would send the user to declare a supply
    // the runner then refuses as a fight with that part.
    let legs: Vec<_> = bound.supplies.iter().map(|l| l.vsource).collect();
    let mut domains = crate::rail_owner::RailDomains::new(&bound.circuit, &bound.behavioral, &legs);
    let board_nodes: std::collections::HashSet<NodeId> =
        bound.net_nodes.values().copied().collect();
    domains.join_switches(&bound.circuit, |n| board_nodes.contains(&n));
    let fed = |name: &str| {
        bound.node(name).is_some_and(|n| {
            domains.fed_by_driver(n) || bound.supplies.iter().any(|l| domains.fed_joined(l.net, n))
        })
    };

    let mut out: Vec<String> = board
        .nets
        .iter()
        .filter(|net| !powered.contains(net.name.as_str()) && !fed(&net.name))
        .filter(|net| names_a_supply_of_unknown_voltage(&net.name))
        .filter(|net| {
            parts_on
                .get(&net.id)
                .is_some_and(|p| p.len() >= DEAD_RAIL_MIN_PARTS)
        })
        .map(|net| net.name.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Extract a rail magnitude from a voltage token embedded MID-name, for
/// suffixed rails like `VDD_1V8`, `AVCC_2V5`, `VCC_1V2` whose voltage is neither
/// the canonical 5 V nor 3.3 V and whose name does not START with the digit (so
/// [`positive_rail_fallback`] skips it). Gated on a supply token so an arbitrary
/// signal net that merely contains a "1V2"-like substring is not misread as a
/// rail. Accepts both `KiCad` digit-V-digit ("1V8") and dotted ("1.8V") forms.
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

/// "+15V", "+`24V_RAIL`", "+15V0", "+9V", or a bare "15V" -> the positive rail
/// voltage. The mirror of [`negative_rail_fallback`]: an OPTIONAL leading '+',
/// then a numeric magnitude in plain ("15V") or `KiCad` digit-V-digit ("5V0",
/// "3V3") form. A name that does not start with a digit after the optional '+'
/// (e.g. "`VDD_5V`", "VCC") returns None and is left to the token heuristic.
/// Expects `n` already trimmed / uppercased (as in [`power_rail_voltage`]).
fn positive_rail_fallback(n: &str) -> Option<f64> {
    rail_magnitude(n.strip_prefix('+').unwrap_or(n))
}

fn rail_magnitude(rest: &str) -> Option<f64> {
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
        .take_while(char::is_ascii_digit)
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
    const RAIL_DOMAIN_EXCEPTIONS: [&str; 2] = ["SENSOR", "FBUS"];
    const TAP_ROOTS: [&str; 8] = ["FB", "FEEDBACK", "SENS", "MON", "DIV", "TAP", "MEAS", "SNS"];
    let first: String = tail
        .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
        .chars()
        .take_while(char::is_ascii_alphanumeric)
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
    if RAIL_DOMAIN_EXCEPTIONS.contains(&first.as_str()) {
        return false;
    }
    TAP_ROOTS.iter().any(|r| first.starts_with(r))
}

/// "-5V", "-`12V_RAIL`", "-5V0", "-`3V3_ANALOG`" -> the negative rail voltage.
/// The mirror of [`positive_rail_fallback`]: a leading '-', then a numeric
/// magnitude in plain ("12V") or `KiCad` digit-V-digit ("3V3", "5V0") form.
/// Expects `n` already trimmed / uppercased (as in [`power_rail_voltage`]).
fn negative_rail_fallback(n: &str) -> Option<f64> {
    // Strip exactly one sign; do not accept -+5V by calling the positive wrapper.
    rail_magnitude(n.strip_prefix('-')?).map(|magnitude| -magnitude)
}

/// A net that carries a `power_out` (or `power_out+no_connect`) pin whose
/// pinfunction names a voltage is a supply rail at that voltage, no matter what
/// the NET is named. `KiCad` netlists from boards that use a non-canonical rail
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
        // A DNP part's power_out pin drives nothing on the real board, and a
        // refused identity is not evidence of a rail either.
        if !AssemblyState::of(comp).is_present() {
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
    /// The layout explicitly marks a part do-not-populate while the BOM
    /// explicitly says it is fitted. Never resolved here; see
    /// [`IdentityReport::advice`]. A false layout DNP flag is unspecified, not
    /// an explicit fitted assertion, and therefore cannot create this finding.
    PopulateDisagrees {
        reference: String,
        layout_says_dnp: bool,
        artifact_says_populate: bool,
        source: String,
    },
}

impl IdentityFinding {
    /// The one line a report prints for this finding.
    #[must_use]
    pub fn line(&self) -> String {
        match self {
            Self::ValueDisagrees {
                reference,
                layout,
                artifact,
                source,
            } => format!(
                "{reference} is {layout:?} on the layout and {artifact:?} in {source}; the part \
                 number from {source} is what bound, so check which revision is current"
            ),
            Self::NotOnBoard { references, source } => format!(
                "{source} names {} parts the board does not have: {}",
                references.len(),
                references.join(", ")
            ),
            Self::NotInArtifact { references, source } => format!(
                "{} parts on the board are not in {source}: {}",
                references.len(),
                references.join(", ")
            ),
            Self::QuantityDisagrees {
                references,
                stated,
                enumerated,
                source,
            } => format!(
                "{source} states a quantity of {stated} for a row that lists {enumerated} \
                 designators ({}); the list is what bound",
                references.join(", ")
            ),
            Self::PopulateDisagrees {
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
    /// Parts without an explicit layout DNP marker that the artifact says are
    /// not populated. This is information the layout reader may not carry at
    /// all, so it remains explicit policy advice rather than a contradiction.
    pub no_fit: Vec<String>,
}

impl FitAdvice {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.fit.is_empty() && self.no_fit.is_empty()
    }
}

/// What [`apply_identity`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdentityReport {
    /// Positive evidence that an auxiliary artifact was reconciled rather than
    /// merely accepted because no contradiction happened to fire.
    pub evidence: Vec<String>,
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
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut out = self.evidence.clone();
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
                "  the BOM says these parts are not populated, while the layout has no explicit \
                 DNP marker: {}. \
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

const fn confidence_word(c: Confidence) -> &'static str {
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
/// board that does not exist. That is [`crate::EXIT_INVALID_FOR_ANALYSIS`],
/// the same treatment a board still carrying Git merge-conflict markers gets.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum IdentityRefusal {
    #[error(
        "{artifact} and the board disagree about what {} of the same parts ARE, which \
         means they are different revisions of the board rather than two views of \
         one: {detail}. Anything computed from the pair would describe a board that \
         does not exist. Use the {artifact_kind} that was exported from this layout, \
         or drop it and analyse the layout alone",
        contradictions.len()
    )]
    Contradiction {
        artifact: String,
        artifact_kind: String,
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
    /// Always [`crate::EXIT_INVALID_FOR_ANALYSIS`].
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        crate::EXIT_INVALID_FOR_ANALYSIS
    }
}

/// Below `1/N` of an artifact's designators matching the board (N is this
/// divisor, so half), the two are not the same board. A BOM may legitimately carry mechanical parts and panel
/// extras, but those cannot outnumber the actual board population. Treating ten
/// matching rows among a hundred as sufficient made a same-prefix board family
/// look compatible even though ninety per cent of the asserted identities were
/// for another design.
const WRONG_BOARD_MATCH_DIVISOR: usize = 2;

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
///
/// # Errors
///
/// Returns [`IdentityRefusal::Contradiction`] when a hint names a part the
/// layout identifies differently, and [`IdentityRefusal::WrongBoard`] when most
/// of the hinted references are not on this board.
#[allow(clippy::too_many_lines)] // the documented reconciliation rules, in order
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
    let artifact_kind = identity_artifact_kind(&hints[0]);

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
    if matched * WRONG_BOARD_MATCH_DIVISOR < named.len() {
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
            artifact_kind: artifact_kind.to_string(),
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
            let before = library_resolution(lib, &board.components[idx]);
            let probe = probe_with_mpn(&board.components[idx], mpn);
            let substituted = probe.value != board.components[idx].value;
            let after = library_resolution(lib, &probe);
            if identity_improves(&before, &after) {
                let comp = &mut board.components[idx];
                let layout_value = comp.value.clone();
                comp.properties
                    .push((MPN_PROPERTY.to_string(), mpn.clone()));
                comp.properties
                    .push((IDENTITY_SOURCE_PROPERTY.to_string(), hint.source.clone()));
                if substituted {
                    comp.value.clone_from(mpn);
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
                comp.value.clone_from(value);
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
            if dnp && populate {
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

/// [`apply_identity`] for a whole BOM, plus quantity reconciliation.
///
/// The one reconciliation a BOM carries that a bare hint list cannot express is a row whose stated quantity disagrees
/// with the number of designators the same row enumerates.
///
/// This is the entry point a caller with a BOM wants. Call it BEFORE
/// `ExtractedBoard::apply_dnp_policy`, so that the policy sees the board this
/// function may have added values to, and feed [`FitAdvice`] into that call if the
/// BOM's populate column is to be honoured.
///
/// # Errors
///
/// Returns the [`IdentityRefusal`] from [`apply_identity`] when the BOM
/// contradicts the layout or belongs to a different board.
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
/// the parts sit.
///
/// A placement file whose positions disagree with the layout's is from another
/// revision, and that is refused for the same reason a contradicting BOM is.
///
/// # Errors
///
/// Returns [`IdentityRefusal::Contradiction`] when the placement file puts
/// parts at other positions, sides or rotations than the layout, and otherwise
/// any refusal [`apply_identity`] raises for its identity hints.
pub fn apply_placement_identity(
    board: &mut ExtractedBoard,
    file: &hauksbee_extract::placement::PlacementFile,
    lib: &ModelLibrary,
) -> Result<IdentityReport, IdentityRefusal> {
    let check = file.cross_check(board);
    if check.is_different_board() {
        let detail = check.lines().join("; ");
        let mut contradictions: Vec<String> = check
            .position_disagreements
            .iter()
            .map(|d| d.reference.clone())
            .chain(check.side_disagreements.iter().map(|d| d.reference.clone()))
            .chain(
                check
                    .rotation_disagreements
                    .iter()
                    .map(|d| d.reference.clone()),
            )
            .chain(check.only_in_placement.iter().cloned())
            .collect();
        contradictions.sort();
        contradictions.dedup();
        return Err(IdentityRefusal::Contradiction {
            artifact: file.provenance.path.clone(),
            artifact_kind: "placement file".to_string(),
            contradictions,
            detail,
        });
    }
    let mut report = apply_identity(board, &file.identity_hints(), lib)?;
    report.evidence.push(check.evidence_line());
    Ok(report)
}

/// Did the LAYOUT name a part, as opposed to stating a parameter or nothing?
///
/// A non-empty value that does not parse as a magnitude is a name: `ATmega328P-AU`,
/// `BSS138`, `LM4040BIM3-2.0`. A parseable magnitude (`10k`, `100nF`) is a
/// parameter. An empty value, `KiCad`'s `~`, and the marker an Altium read leaves
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
/// Vendor-specific package and reel suffixes are the reason this exists:
/// `ATmega328P-AU`, `BSS138-7-F` and `BC847B,215` may be written without their
/// ordering code in a layout. The suffix is removed only when the artifact also
/// names the vendor that defines it; globally rewriting an arbitrary `-AU` or
/// `,215` could merge two genuinely different orderable devices. A shared prefix
/// is never identity.
fn names_same_part(a: &str, b: &str, manufacturer: Option<&str>) -> bool {
    fn norm(s: &str) -> String {
        s.chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_uppercase()
    }

    fn without_vendor_ordering_suffix(s: &str, manufacturer: Option<&str>) -> String {
        let upper = s.trim().to_ascii_uppercase();
        let vendor = manufacturer.map(norm).unwrap_or_default();
        for (suffix, vendor_matches) in [
            ("-7-F", vendor.starts_with("DIODESINC")),
            (",215", vendor.contains("NEXPERIA")),
            (
                "-AU",
                vendor.contains("MICROCHIP") || vendor.contains("ATMEL"),
            ),
        ] {
            if !vendor_matches {
                continue;
            }
            if let Some(core) = upper.strip_suffix(suffix) {
                if !core.is_empty() {
                    return norm(core);
                }
            }
        }
        norm(&upper)
    }

    let (normal_a, normal_b) = (norm(a), norm(b));
    if normal_a.is_empty() || normal_b.is_empty() {
        return false;
    }
    normal_a == normal_b
        || without_vendor_ordering_suffix(a, manufacturer)
            == without_vendor_ordering_suffix(b, manufacturer)
}

#[cfg(test)]
mod part_name_tests {
    use super::names_same_part;

    #[test]
    fn only_documented_package_and_reel_suffixes_name_the_same_part() {
        assert!(!names_same_part("TPS62130", "TPS62135", None));
        assert!(!names_same_part("ATmega328P", "ATmega328PB", None));
        assert!(names_same_part(
            "ATmega328P-AU",
            "ATmega328P",
            Some("Microchip Technology")
        ));
        assert!(names_same_part("BSS138", "BSS138-7-F", Some("Diodes Inc")));
        assert!(names_same_part(
            "BC847B",
            "BC847B,215",
            Some("Nexperia USA Inc.")
        ));
        assert!(!names_same_part("SENSOR-AU", "SENSOR", None));
        assert!(!names_same_part("DRIVER-7-F", "DRIVER", None));
        assert!(!names_same_part("SENSOR-AU", "SENSOR", Some("Acme")));
    }
}

/// The component as it looks with an artifact's part number attached.
///
/// Two things are attached, and the second needs explaining. The part number goes
/// on the reserved property, which is where `resolve` looks for it. And where the
/// layout's value carries no electrical parameter, the part number ALSO stands in
/// for the value.
///
/// The substitution is not a shortcut. A model's match rules are `ANDed`, and nearly
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
    let prefix = ref_prefix(reference);
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
    let prefix = ref_prefix(reference);
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
#[allow(clippy::too_many_lines)] // one early return per contradiction detector
fn contradiction_between(
    comp: &Component,
    hint: &hauksbee_extract::bom::IdentityHint,
    lib: &ModelLibrary,
) -> Option<String> {
    let reference = &comp.reference;
    let artifact_kind = identity_artifact_kind(hint);

    // Two explicit manufacturers cannot both be true. Most layout formats do
    // not carry a manufacturer at all; absence is unknown and never treated as
    // a contradiction.
    if let (Some(layout), Some(artifact)) = (
        explicit_manufacturer(comp),
        hint.manufacturer
            .as_deref()
            .filter(|s| !s.trim().is_empty()),
    ) {
        if normalise_identity_text(layout) != normalise_identity_text(artifact) {
            return Some(format!(
                "{reference} names manufacturer {layout:?} on the layout and {artifact:?} in the {artifact_kind}"
            ));
        }
    }

    // Package names vary by CAD library, so compare only standardized package
    // families and explicit electrical-pad counts. Distinct numbered pads are
    // stronger evidence than raw shapes and catch a stale QFN-16 row against a
    // ten-pad footprint without counting paste apertures as invented pins.
    if let Some(artifact_package) = hint.footprint.as_deref().filter(|s| !s.trim().is_empty()) {
        if let Some(expected_pads) = package_electrical_pad_count(artifact_package) {
            let actual_pads = component_electrical_pad_count(comp);
            if actual_pads > 0 && expected_pads != actual_pads {
                return Some(format!(
                    "{reference} is {artifact_package:?} in the {artifact_kind} ({expected_pads} electrical pads) but the board footprint has {actual_pads} distinct numbered electrical pads"
                ));
            }
        }
        if let (Some(layout_family), Some(artifact_family)) = (
            hauksbee_models::variants::package_family(&comp.footprint),
            hauksbee_models::variants::package_family(artifact_package),
        ) {
            if layout_family != artifact_family {
                return Some(format!(
                    "{reference} uses package family {layout_family} on the layout and {artifact_family} ({artifact_package:?}) in the {artifact_kind}"
                ));
            }
        }
    }

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
            let layout = library_resolution(lib, comp);
            let artifact = library_resolution(lib, &probe_with_mpn(comp, mpn));
            let decisive = matches!(artifact.confidence, Confidence::Exact | Confidence::Family);
            if let (Some(l), Some(a)) = (&layout.model, &artifact.model) {
                // Different models are not automatically different parts: the
                // layout's reading is often an engine stand-in for the very part
                // the BOM names (`ATmega328P-AU` reaching a fallback while
                // `ATmega328P` reaches the library entry). Two strings that name
                // the same part are not a disagreement, so the strings decide and
                // the models only widen it to a kind mismatch.
                let same_part = names_same_part(&comp.value, mpn, hint.manufacturer.as_deref());
                if decisive && l.id != a.id && !same_part {
                    return Some(format!(
                        "{reference} is {:?} (\"{}\") on the layout and {:?} (\"{}\") in the {artifact_kind}",
                        comp.value,
                        model_words(l),
                        mpn,
                        model_words(a),
                    ));
                }
                if l.kind != a.kind && !same_part {
                    return Some(format!(
                        "{reference} is {:?} (a {}) on the layout and {:?} (a {}) in the {artifact_kind}",
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
                "{reference} is {:?} ({l}) on the layout and {artifact_value:?} ({a}) in the {artifact_kind}",
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
                    .take_while(char::is_ascii_alphabetic)
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
                        .take_while(char::is_ascii_alphabetic)
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
        let layout_kind = library_resolution(lib, comp).model.map(|m| m.kind);
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
                 in the {artifact_kind}",
                comp.value,
                kind_word(layout_kind.expect("matched above"))
            ));
        }
    }
    None
}

fn normalise_identity_text(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_uppercase()
}

fn explicit_manufacturer(comp: &Component) -> Option<&str> {
    comp.properties.iter().find_map(|(key, value)| {
        let key = normalise_identity_text(key);
        matches!(
            key.as_str(),
            "MANUFACTURER" | "MANUFACTURERNAME" | "MFR" | "MFG"
        )
        .then_some(value.as_str())
    })
}

/// Number of electrical pads explicitly described by a package name.
///
/// `KiCad`'s `QFN-56-1EP` spelling means 56 perimeter pads plus one exposed
/// electrical pad. Paste-aperture subdivisions are not part of that count.
fn package_electrical_pad_count(package: &str) -> Option<usize> {
    let upper = package.to_ascii_uppercase();
    for family in [
        "TSSOP", "SSOP", "SOIC", "TQFP", "LQFP", "QFN", "DFN", "BGA", "DIP",
    ] {
        let Some(start) = upper.find(family) else {
            continue;
        };
        let suffix = &upper[start + family.len()..];
        let Some(digit_start) = suffix.find(|c: char| c.is_ascii_digit()) else {
            continue;
        };
        let digits: String = suffix
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(char::is_ascii_digit)
            .collect();
        if let Ok(pads) = digits.parse::<usize>() {
            let after_count = &suffix[digit_start + digits.len()..];
            let exposed_pad = after_count.starts_with("-1EP") || after_count.starts_with("_1EP");
            return Some(pads + usize::from(exposed_pad));
        }
    }
    None
}

/// Distinct numbered pads are electrical terminals. An empty pad number marks
/// a mechanical or paste-only aperture and must not turn stencil geometry into
/// an invented IC pin. Repeated pad numbers are one electrical terminal.
fn component_electrical_pad_count(comp: &Component) -> usize {
    comp.pins
        .iter()
        .map(|pin| pin.number.trim())
        .filter(|number| !number.is_empty())
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

fn identity_artifact_kind(hint: &hauksbee_extract::bom::IdentityHint) -> &'static str {
    match hint.source_kind.as_str() {
        "kicad_pos_ascii" | "kicad_pos_csv" | "altium_pick_and_place" | "cpl" => "placement file",
        _ => "BOM",
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::{Net, Pin};
    use hauksbee_models::{ComponentQuery, ModelLibrary};

    fn comp(reference: &str, value: &str) -> Component {
        Component {
            reference: reference.to_string(),
            value: value.to_string(),
            ..Default::default()
        }
    }

    fn pin(number: &str, net: i64) -> Pin {
        Pin {
            number: number.to_string(),
            net: Some(net),
            ..Default::default()
        }
    }

    fn net(id: i64, name: &str) -> Net {
        Net::new(id, name.to_string())
    }

    /// Resolve `value` the way the binder does (value and MPN both set).
    fn resolve_value(lib: &ModelLibrary, value: &str, footprint: &str) -> ModelEntry {
        let fp = (!footprint.is_empty()).then(|| footprint.to_string());
        let mut q = ComponentQuery::new(None, Some(value.to_string()), fp);
        q.mpn = Some(value.to_string());
        lib.resolve(&q)
            .model
            .unwrap_or_else(|| panic!("{value} must resolve"))
    }

    /// `bind_component` with empty side tables, returning the outcome and warning.
    fn bind_one(
        c: &Component,
        model: &ModelEntry,
        circuit: &mut Circuit,
        node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
        power_nets: &HashMap<String, f64>,
    ) -> (BindOutcome, Option<String>) {
        let (outcome, warning, _) = bind_component(
            c,
            model,
            None,
            circuit,
            node_of,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            (std::slice::from_ref(c), power_nets),
            ModelLibrary::builtin().pin_rules(),
        );
        (outcome, warning)
    }

    type Switch = (NodeId, NodeId, NodeId, NodeId, f64, f64);

    fn vswitches(circuit: &Circuit) -> Vec<Switch> {
        circuit
            .devices
            .iter()
            .filter_map(|d| match d {
                Device::VSwitch {
                    a,
                    b,
                    ctrl_p,
                    ctrl_n,
                    von,
                    voff,
                    ..
                } => Some((*a, *b, *ctrl_p, *ctrl_n, *von, *voff)),
                _ => None,
            })
            .collect()
    }

    /// Each role on the node its name gives, or `role:node` on that node.
    fn roles(circuit: &mut Circuit, names: &[&str]) -> HashMap<String, NodeId> {
        let node = |(r, n): (&str, &str)| (r.to_string(), circuit.node(&n.to_uppercase()));
        let at = names.iter().map(|r| r.split_once(':').unwrap_or((*r, *r)));
        at.map(node).collect()
    }

    #[test]
    fn vss_is_canonical_ground_but_split_families_and_vee_are_not() {
        for ground in ["VSS", "vss", "/Power/VSS", "GND", "0"] {
            assert!(is_canonical_ground(ground), "{ground}");
        }
        for split in ["AGND", "DGND", "PGND", "ISOGND", "CHASSIS_GND", "VEE"] {
            assert!(
                !is_canonical_ground(split),
                "{split} must stay a distinct node"
            );
        }
    }

    #[test]
    fn inferred_ambiguous_altium_identity_binds_only_with_an_authoritative_uid() {
        let lib = ModelLibrary::builtin();
        let marker = (
            hauksbee_extract::altium::REFERENCE_AMBIGUOUS_KEY.to_string(),
            "hierarchy could not distinguish repeated source designators".to_string(),
        );
        let mut ambiguous = comp("R1", "1k");
        ambiguous.lib_id = "Device:R".into();
        ambiguous.footprint = "R_0603".into();
        ambiguous.properties.push(marker);
        assert!(AssemblyState::of(&ambiguous).fitted().is_none());
        assert!(library_resolution(&lib, &ambiguous).model.is_none());

        let mut authoritative = ambiguous.clone();
        authoritative.properties.push((
            hauksbee_extract::altium::SOURCE_UNIQUE_ID_KEY.into(),
            "ABC-123".into(),
        ));
        let part = AssemblyState::of(&authoritative)
            .fitted()
            .expect("the source UID makes the component identity authoritative");
        assert!(resolve(&lib, part).model.is_some());
    }

    #[test]
    fn conflicting_duplicate_identity_is_left_open_and_reported() {
        let conflict = "records named 'R1' were kept distinct: footprints differ";
        let mut r1 = comp("R1", "1k");
        r1.footprint = "R_0603".into();
        r1.properties.push((
            hauksbee_extract::DUPLICATE_REFERENCE_CONFLICT_KEY.into(),
            conflict.into(),
        ));
        let mut power_out = pin("1", 1);
        power_out.function = "+5V".into();
        power_out.kind = "power_out".into();
        r1.pins = vec![power_out, pin("2", 2)];
        let board = ExtractedBoard {
            name: "ambiguous".into(),
            nets: vec![net(1, "SIGNAL"), net(2, "GND")],
            components: vec![r1],
        };

        let bound = bind_board(&board, &ModelLibrary::builtin());
        let row = bound.report.row("R1").unwrap();
        assert_eq!(row.model_id, None);
        assert_eq!(row.confidence, Confidence::Unresolved);
        assert!(matches!(
            &row.outcome,
            BindOutcome::Unresolved { reason } if reason.contains(conflict)
        ));
        assert!(row.warning.as_deref().is_some_and(|w| w.contains(conflict)));
        assert!(
            bound.supplies.is_empty(),
            "a conflicted power_out record must not manufacture an ideal supply"
        );
    }

    /// Bind one peripheral card on a board of SDA/+3V3/SCL/IRQ nets (board
    /// nets 0/1 ground, 2 SDA, 3 +3V3, 4 SCL, 5 a signal net).
    fn bind_peripheral(
        component: &Component,
        model: &ModelEntry,
    ) -> (BindOutcome, Option<String>, Vec<PeripheralBinding>) {
        let lib = ModelLibrary::builtin();
        let mut circuit = Circuit::new();
        let nodes = ["SDA", "+3V3", "SCL", "IRQ"].map(|name| circuit.node(name));
        let node_of = |n: Option<i64>| match n {
            Some(0 | 1) => Some(NodeId::GROUND),
            Some(n @ 2..=5) => nodes.get(usize::try_from(n - 2).ok()?).copied(),
            _ => None,
        };
        let mut peripherals = Vec::new();
        let (outcome, warning, _) = bind_component(
            component,
            model,
            None,
            &mut circuit,
            &node_of,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut peripherals,
            &mut Vec::new(),
            (
                std::slice::from_ref(component),
                &HashMap::from([("+3V3".to_string(), 3.3)]),
            ),
            lib.pin_rules(),
        );
        (outcome, warning, peripherals)
    }

    fn pins_on(wiring: &[(&str, i64)]) -> Vec<Pin> {
        wiring.iter().map(|&(pad, net)| pin(pad, net)).collect()
    }

    #[test]
    fn register_map_address_and_bus_personality_come_from_exact_board_straps() {
        let model = resolve_value(&ModelLibrary::builtin(), "BMA423", "");
        let mut component = comp("U6", "BMA423");
        // SDO -> ground selects 0x18; CSB -> +3V3 selects I2C.
        component.pins = pins_on(&[
            ("1", 1),
            ("2", 2),
            ("3", 3),
            ("7", 3),
            ("8", 0),
            ("9", 0),
            ("10", 3),
            ("12", 4),
        ]);
        let (outcome, warning, peripherals) = bind_peripheral(&component, &model);
        assert!(
            matches!(outcome, BindOutcome::Behavioral { .. }),
            "{outcome:?} / {warning:?}"
        );
        assert_eq!(peripherals.len(), 1);
        assert_eq!(peripherals[0].i2c_address_override, Some(0x18));
    }

    /// A card whose register map carries its own `[sensor.pins]` and a
    /// two-strap address table: the pad map finds the bus, the board's straps
    /// pick the address, and a strap on a signal net leaves it assumed.
    #[test]
    fn sensor_pin_map_binds_the_bus_and_straps_select_the_address() {
        let card = r#"
[[models]]
id = "sensor_fixture"
kind = "digital"
[models.peripheral]
kind = "register_map"
spec_toml = '''
[sensor]
name = "SENSOR_FIXTURE"
bus = "i2c"
i2c_address = 0x40
[[sensor.register]]
addr = 0x00
const = [0x13]
[sensor.pins]
1 = "scl"
2 = "sda"
3 = "a0"
4 = "a1"
5 = "gnd"
6 = "vdd"
7 = "int"
[[sensor.address_select]]
address = 0x40
straps = { a0 = "low", a1 = "low" }
[[sensor.address_select]]
address = 0x41
straps = { a0 = "high", a1 = "low" }
[[sensor.address_select]]
address = 0x44
straps = { a0 = "low", a1 = "high" }
[sensor.protocol]
style = "i2c_pointer"
'''
"#;
        let mut db: hauksbee_models::schema::DbFile = toml::from_str(card).unwrap();
        let model = db.models.remove(0);
        hauksbee_models::validation::validate(&model).expect("the card validates");
        let mut component = comp("U9", "SENSOR_FIXTURE");
        // SCL, SDA, A0 -> `a0_net`, A1 -> GND, GND, VDD -> +3V3, INT -> IRQ.
        let wire = |a0_net| {
            pins_on(&[
                ("1", 4),
                ("2", 2),
                ("3", a0_net),
                ("4", 0),
                ("5", 0),
                ("6", 3),
                ("7", 5),
            ])
        };
        component.pins = wire(3);
        let (outcome, warning, peripherals) = bind_peripheral(&component, &model);
        assert!(
            matches!(outcome, BindOutcome::Behavioral { .. }),
            "{outcome:?} / {warning:?}"
        );
        assert_eq!(peripherals[0].i2c_address_override, Some(0x41));
        assert_eq!(peripherals[0].i2c_address_assumed, None);
        assert_eq!(warning, None);

        component.pins = wire(5);
        let (outcome, warning, peripherals) = bind_peripheral(&component, &model);
        assert!(matches!(outcome, BindOutcome::Behavioral { .. }));
        assert_eq!(peripherals[0].i2c_address_override, Some(0x40));
        let warning = warning.expect("an assumed address is disclosed");
        assert!(
            warning.starts_with(Assumption::PARTIAL_MODEL_MARKER)
                && warning.contains("0x40 is assumed")
                && warning.contains("strap role 'a0' is on 'IRQ'"),
            "{warning}"
        );

        // Without SCL wired the bus personality is not attached.
        component.pins.retain(|p| p.number != "1");
        let (outcome, _, peripherals) = bind_peripheral(&component, &model);
        assert!(matches!(outcome, BindOutcome::Unresolved { .. }));
        assert!(peripherals.is_empty());
    }

    #[test]
    fn digital_output_driver_honours_model_ro() {
        let mut model = resolve_value(&ModelLibrary::builtin(), "74HC595", "");
        let custom_ro = 123.0;
        assert!((custom_ro - DEFAULT_RO).abs() > 1e-9);
        model.params.set_f64("ro", custom_ro);
        let mut circuit = Circuit::new();
        let roles = roles(&mut circuit, &["srclk", "rclk", "ser", "qa", "qb"]);
        let mut digital = Vec::new();
        bind_digital(&comp("U1", ""), &model, &mut circuit, &roles, &mut digital)
            .expect("the builtin 595 spec compiles");
        assert_eq!(digital.len(), 1);
        assert!((digital[0].drivers["qa"].ron - custom_ro).abs() < 1e-12);
    }

    /// A digital part whose [models.logic] fails to compile must surface the
    /// error and push nothing, so the row reads UNRESOLVED rather than bound.
    #[test]
    fn broken_logic_spec_reports_unresolved_not_bound() {
        let mut model = resolve_value(&ModelLibrary::builtin(), "74HC595", "");
        model
            .logic
            .comb
            .insert("qa".to_string(), "no_such_signal & ser".to_string());
        let mut circuit = Circuit::new();
        let roles = roles(&mut circuit, &["srclk", "rclk", "ser", "qa", "qb"]);
        let mut digital = Vec::new();
        let err = bind_digital(&comp("U1", ""), &model, &mut circuit, &roles, &mut digital)
            .expect_err("a spec that cannot compile must surface its error");
        assert!(!err.is_empty());
        assert!(digital.is_empty());
    }

    #[test]
    fn identity_only_model_keeps_critical_part_unresolved() {
        let mut params = hauksbee_models::Params::default();
        params.set_bool("identity_only", true);
        params.set_str("warning", "register behavior is not modeled");
        params.set_str("unlocked_by", "a validated register and bus model");
        let model = make_entry(
            "identity_only_sensor",
            ComponentKind::Digital,
            "identity only",
            params,
            std::collections::BTreeMap::new(),
        );
        let mut circuit = Circuit::new();
        let mut digital = Vec::new();
        let (outcome, _, _) = bind_component(
            &comp("U1", ""),
            &model,
            None,
            &mut circuit,
            &|_| None,
            &mut digital,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            (&[], &HashMap::new()),
            ModelLibrary::builtin().pin_rules(),
        );
        assert!(
            matches!(outcome, BindOutcome::Unresolved { .. }),
            "{outcome:?}"
        );
        assert!(digital.is_empty(), "identity-only cards stamp no logic");

        let mut report = BindReport::default();
        report.push(BindRow {
            model_id: Some(model.id),
            warning: Some("identity only; remains OPEN".into()),
            ..BindRow::without_model("U1".into(), "SENSOR".into(), Confidence::Exact, outcome)
        });
        let summary = crate::bind_report::BindSummary::from_report(&report);
        assert_eq!(summary.critical_parts_bound, "0/1");
        assert_eq!(summary.active_path_unresolved.len(), 1);
    }

    fn simple_vreg(vout: Option<f64>) -> ModelEntry {
        let mut model = make_entry(
            "generic_ldo",
            ComponentKind::Vreg,
            "simple fixed regulator",
            hauksbee_models::Params::default(),
            std::collections::BTreeMap::new(),
        );
        if let Some(vout) = vout {
            model.params.set_f64("vout", vout);
        }
        // Synthetic ideal regulators have no physical loop or capacitor
        // requirement; omitting this field would instead assert that it is unknown.
        model.params.set_f64("cout_min_f", 0.0);
        model
    }

    /// `bind_vreg` for U9, returning what it recorded as well.
    fn bind_u9(
        model: &ModelEntry,
        source: Option<&ModelSource>,
        circuit: &mut Circuit,
        roles: &HashMap<String, NodeId>,
    ) -> (BindOutcome, Option<String>, Vec<SimpleRegulator>) {
        let mut regs = Vec::new();
        let component = comp("U9", "");
        let (outcome, warning) = bind_vreg(
            &component,
            model,
            source,
            circuit,
            roles,
            &mut regs,
            std::slice::from_ref(&component),
            &|_| None,
        );
        (outcome, warning, regs)
    }

    #[test]
    fn simple_vreg_requires_finite_voltage_and_distinct_roles() {
        for case in [
            "missing vout | - | in out gnd | finite `vout`",
            "NaN vout | NaN | in out gnd | finite `vout`",
            "infinite vout | inf | in out gnd | finite `vout`",
            "missing input | 3.3 | out gnd | input not connected",
            "missing output | 3.3 | in gnd | output not connected",
            "missing return | 3.3 | in out | local return not connected",
            "out-return tied | 3.3 | gnd:a in:b out:a | must be distinct",
            "out-in tied | 3.3 | gnd:a in:b out:b | must be distinct",
            "in-return tied | 3.3 | gnd:a in:a out:b | must be distinct",
        ] {
            let [_, vout, names, token] = case.split(" | ").collect::<Vec<_>>()[..] else {
                panic!("{case}")
            };
            let mut circuit = Circuit::new();
            let roles = roles(&mut circuit, &names.split(' ').collect::<Vec<_>>());
            let model = simple_vreg(vout.parse().ok());
            let (outcome, warning, regs) = bind_u9(&model, None, &mut circuit, &roles);
            assert!(matches!(outcome, BindOutcome::Unresolved { .. }), "{case}");
            assert!(warning.is_some_and(|w| w.contains(token)), "{case}");
            assert!(circuit.devices.is_empty() && regs.is_empty(), "{case}");
        }
    }

    /// Lead-letter keys bind by the board's pin names; on a layout-only board a
    /// SOT-23 takes the generic pin rule's order as a disclosed guess, one with no
    /// rule stays OPEN, and a rule that names a board-named pad otherwise is
    /// refused, never rewired. A numbered card wins over the rule. A fixed
    /// regulator whose ADJ/GND pad is `adj` has no `gnd` return and is left OPEN.
    #[test]
    fn letter_keyed_pins_bind_by_name_or_disclosed_rule_and_adj_pad_is_open() {
        let node_of = |net: Option<i64>| net.map(|id| NodeId(u32::try_from(id).unwrap()));
        let kv = |s: &'static str| s.split(' ').filter_map(|kv| kv.split_once('='));
        let bind = |kind, footprint: &str, params, pins, names: &str| {
            let mut p = hauksbee_models::Params::default();
            kv(params).for_each(|(k, v)| p.set_f64(k, v.parse().unwrap()));
            let pins = kv(pins).map(|(k, v)| (k.into(), v.into())).collect();
            let mut c = comp("U1", "FIXTURE");
            (c.footprint, c.pins) = (footprint.into(), pins_on(&[("1", 1), ("2", 2), ("3", 3)]));
            for (pin, f) in c.pins.iter_mut().zip(names.split(' ')) {
                pin.function = f.into();
            }
            let mut circuit = Circuit::new();
            let model = make_entry("fixture", kind, "", p, pins);
            let lib = ModelLibrary::builtin();
            let guesses = role_node_map_guessed(&c, &model, &node_of, lib.pin_rules()).1;
            let (outcome, _) = bind_one(&c, &model, &mut circuit, &node_of, &HashMap::new());
            let gate = circuit.devices.iter().find_map(|d| match d {
                Device::Mosfet { g, .. } => Some(*g),
                _ => None,
            });
            (format!("{outcome:?} {guesses:?}"), gate, guesses.len())
        };
        let (letters, numbers) = ("G=gate S=source D=drain", "1=source 2=gate 3=drain");
        let (rule, open) = (
            "guessed from rule 'nmos_sot23_g1_s2_d3'",
            "not all connected",
        );
        let clash = "pad 1 is named 'source', not 'gate' as pin rule 'nmos_sot23_g1_s2_d3'";
        for (footprint, pins, names, gate, guessed, needle) in [
            ("SOT-23", letters, "S G D", Some(NodeId(2)), 0, "Analog"),
            ("SOT-23", letters, "", Some(NodeId(1)), 3, rule),
            ("SOT-89-3", letters, "", None, 0, open),
            ("SOT-23", letters, "S", None, 0, clash),
            ("SOT-89-3", numbers, "", Some(NodeId(2)), 0, "Analog"),
            ("SOT-23", numbers, "", Some(NodeId(2)), 0, "Analog"),
        ] {
            let (why, got, n) = bind(ComponentKind::Nmos, footprint, "vto=1 kp=0.1", pins, names);
            assert!(
                (got, n) == (gate, guessed) && why.contains(needle),
                "{footprint} {pins} {names}: {why}"
            );
        }
        let ldo = "vout=3.3 dropout_v=1 iq_a=1e-3 cout_min_f=0";
        for (pins, open) in [("1=adj 2=out 3=in", true), ("1=gnd 2=out 3=in", false)] {
            let (why, _, _) = bind(ComponentKind::Vreg, "SOT-223", ldo, pins, "");
            assert_eq!(why.contains("return not connected"), open, "{pins}: {why}");
        }
    }

    /// A standard-numbered card's role is a guess even on a pad a pin-function names otherwise.
    #[test]
    fn a_standard_numbered_role_beside_a_pin_function_is_still_a_guess() {
        let node_of = |net: Option<i64>| net.map(|id| NodeId(u32::try_from(id).unwrap()));
        let pins = [("1", "gate"), ("2", "source"), ("3", "drain")];
        let pins = pins.map(|(k, v)| (k.into(), v.into())).into();
        let (nmos, p) = (ComponentKind::Nmos, hauksbee_models::Params::default());
        let mut model = make_entry("fixture", nmos, "", p, pins);
        model.pin_standard = Some("JEDEC TO-236 top view".into());
        let mut c = comp("Q1", "FIXTURE");
        (c.footprint, c.pins) = ("SOT-89-3".into(), pins_on(&[("1", 1), ("2", 2), ("3", 3)]));
        (c.pins[0].function, c.pins[1].function) = ("D".into(), "S".into());
        let lib = ModelLibrary::builtin();
        let g = role_node_map_guessed(&c, &model, &node_of, lib.pin_rules()).1;
        assert!(
            matches!(&g[..], [s] if s.starts_with("pad 1 role 'gate' numbered from")),
            "{g:?}"
        );
    }

    #[test]
    fn simple_vreg_stamps_exact_voltage_between_output_and_return() {
        let lib = ModelLibrary::builtin();
        for case in [
            "card | AMS1117-3.3 | 3.3 | OUT 0 | 1.3 | 4.699 | 15 | at least 22 µF output capacitance",
            "offset | LM7905 | -5 | OUT RET | 1.1 | 6.1 | 25 | does not state the output-capacitor",
            "abs max | MIC5205-3.3 | 3.3 | OUT 0 | 0.35 | 3.65 | 20 | does not state the output-capacitor",
            "bare | - | 5 | 0 RET | - | 5 | inf | -",
        ] {
            let [_, card, vout, pins, dropout_v, min_in_v, max_in_v, token] =
                case.split(" | ").collect::<Vec<_>>()[..]
            else {
                panic!("{case}")
            };
            let (out, gnd) = pins.split_once(' ').expect("two pins");
            let parse = |v: &str| v.parse::<f64>().unwrap_or(f64::NAN);
            let [vout, min_in_v, max_in_v] = [vout, min_in_v, max_in_v].map(parse);
            let (card, dropout_v) = ((card != "-").then_some(card), dropout_v.parse().ok());
            let mut circuit = Circuit::new();
            let roles = HashMap::from([
                ("in".into(), circuit.node("IN")),
                ("out".into(), circuit.node(out)),
                ("gnd".into(), circuit.node(gnd)),
            ]);
            let resolved = card.map(|v| lib.resolve(&ComponentQuery::new(None, Some(v.into()), None)));
            let model = resolved.as_ref().map_or_else(
                || simple_vreg(Some(vout)),
                |r| r.model.clone().expect("card resolves"),
            );
            let source = resolved.as_ref().and_then(|r| r.provenance.as_ref());
            let (outcome, warning, regs) = bind_u9(&model, source, &mut circuit, &roles);
            let marked = |w: &str| w.starts_with(Assumption::PARTIAL_MODEL_MARKER) && w.contains(token);
            let caveat = warning.as_deref().map(marked);
            assert_eq!(caveat, (token != "-").then_some(true), "{case}: {warning:?}");
            assert!(
                matches!(outcome, BindOutcome::Behavioral { device } if device == format!("vreg {vout:.1}V source")),
                "{case}"
            );
            assert!(
                matches!(circuit.devices.as_slice(), [Device::Vsource { p, n, kind: SourceKind::Dc(v), .. }, ..] if *p == roles["out"] && *n == roles["gnd"] && (*v - vout).abs() < 1e-12),
                "{case}"
            );
            let [reg] = regs.as_slice() else {
                panic!("{case}: exactly one regulator is recorded");
            };
            // IN draws the pass current, whole above dropout and none with IN
            // at OUT, plus any card `iq_a`, both IN -> GND. The draw reads the
            // branch at DC and the hidden level source in a transient.
            let iq = model.params.get_f64("iq_a");
            let names: Vec<_> = circuit.devices[1..].iter().map(Device::name).collect();
            let mut want = vec!["Vpass_U9", "Bpass_U9"];
            want.extend(iq.map(|_| "Iq_U9"));
            assert_eq!(names, want, "{case}");
            let helpers: Vec<u32> = reg.helpers.iter().map(|h| h.0).collect();
            assert_eq!((helpers[0], reg.pass_level.map(|l| l.0)), (2, Some(1)), "{case}");
            let into_ret = |d: &Device| d.conduction_nodes() == [roles["in"], roles["gnd"]];
            assert!(circuit.devices[2..].iter().all(into_ret), "{case}");
            let Device::Behavioral { expr, .. } = &circuit.devices[2] else {
                panic!("{case}: the draw is a B-source");
            };
            let (amps, sign) = (0.25, vout.signum());
            let draw = |h: f64, time: f64| {
                let level = if time > 0.0 { amps } else { 0.0 };
                let branch = if time > 0.0 { 0.0 } else { -amps };
                expr.eval(&[level, branch, sign.mul_add(h, vout), vout], time).unwrap()
            };
            let full = dropout_v.unwrap_or(0.0);
            for time in [0.0, 1e-3] {
                assert!((draw(full, time) - amps).abs() < 1e-12, "{case} at {time}");
                assert!(draw(0.1f64.mul_add(-full.max(0.1), full), time) < amps, "{case} at {time}");
                assert!(draw(full - full.max(PASS_FADE_V), time).abs() < 1e-12, "{case}");
            }
            assert_eq!((reg.reference.as_str(), reg.input), ("U9", roles["in"]));
            assert_eq!((reg.source.0, reg.dropout_v), (0, dropout_v));
            assert!((reg.min_in_v - min_in_v).abs() < 1e-12, "{case}: {reg:?}");
            // Card ratings pass through untouched (including the unrated `inf`).
            assert_eq!(reg.max_in_v.to_bits(), max_in_v.to_bits(), "{case}");
        }
    }

    /// A row param binds exactly as the scalar it replaces, its min/max giving
    /// the vout interval the headroom reads; a full library tie leads the caveat.
    #[test]
    fn row_params_bind_like_scalars_and_a_full_tie_is_a_caveat() {
        let dir = tempfile::tempdir().expect("tempdir");
        let row = |cols: &str| format!("{{ {cols}, conditions = \"c\", basis = \"b\" }}");
        let vout = row("min = 3.2, typ = 3.3, max = 3.4, unit = \"V\"");
        let dropout = row("typ = 250, unit = \"mV\"");
        let limits = "[[models.source.uncertainty]]\nstatus = \"interval\"\nparameter = \"vout\"\n\
             low = 3.2\nhigh = 3.4\nunit = \"V\"\nkind = \"specification-limits\"\nbasis = \"b\"\n";
        let tie = "{}\nvout = 3.3\ndropout_v = 0.25";
        let scalar = limits.to_owned() + tie;
        let row = format!("{{}}\nvout = {vout}\ndropout_v = {dropout}");
        for (id, value, body) in [
            ("zz_scalar", "ZZ_SCALAR", &*scalar),
            ("zz_row", "ZZ_ROW", &row),
            ("zz_tie_b", "ZZ_TIE", tie),
            ("zz_tie_a", "ZZ_TIE", tie),
        ] {
            let head = format!(
                "[[models]]\nid = \"{id}\"\nkind = \"vreg\"\ndescription = \"synthetic\"\n\
                 [models.match]\nvalue_re = '^{value}$'\n[models.source]\ntier = \"datasheet-derived\"\n\
                 [[models.source.uncertainty]]\nstatus = \"unknown\"\nparameter = \"model\"\nreason = \"-\"\n"
            );
            let card = head + &body.replace("{}", "[models.params]\ncout_min_f = 0.0\niq_a = 1e-5");
            std::fs::write(dir.path().join(format!("{id}.toml")), card).expect("card written");
        }
        let lib = ModelLibrary::builtin()
            .with_user_dir(dir.path())
            .expect("cards load");
        let resolve = |v: &str| lib.resolve(&ComponentQuery::new(None, Some(v.into()), None));
        let [scalar, row] = ["ZZ_SCALAR", "ZZ_ROW"].map(|value| {
            let (resolved, mut circuit) = (resolve(value), Circuit::new());
            let roles = roles(&mut circuit, &["in", "out", "gnd"]);
            let model = resolved.model.expect("the synthetic card resolves");
            let (outcome, warning, regs) =
                bind_u9(&model, resolved.provenance.as_ref(), &mut circuit, &roles);
            let limits = format!("{:.9?} {:.9}", regs[0].dropout_v, regs[0].min_in_v);
            let bound = format!("{outcome:?} {warning:?} {:?}", circuit.devices);
            (bound, limits)
        });
        assert_eq!(scalar, row);
        assert_eq!(row.1, "Some(0.250000000) 3.650000000");

        let (resolved, component) = (resolve("ZZ_TIE"), comp("U9", "ZZ_TIE"));
        let marker = Assumption::PARTIAL_MODEL_MARKER;
        let bare = with_tie_warning(&component, &resolved, None).unwrap_or_default();
        let tie = "U9 (ZZ_TIE): matched library entries zz_tie_a, zz_tie_b equally well; bound 'zz_tie_a'";
        assert!(bare.starts_with(&format!("{marker}{tie}")), "{bare}");
        let other = Some(format!("{marker}U9 (ZZ_TIE): enable off"));
        let combined = with_tie_warning(&component, &resolved, other);
        assert_eq!(combined, Some(format!("{bare}; also enable off")));
    }

    fn analog_switch_entry(description: &str) -> ModelEntry {
        make_entry(
            "generic_analog_switch",
            ComponentKind::AnalogSwitch,
            description,
            hauksbee_models::Params::default(),
            std::collections::BTreeMap::new(),
        )
    }

    /// An analog switch with only its common and control pins wired must be
    /// left OPEN: the control net is the gate, never a signal terminal.
    #[test]
    fn spst_fallback_does_not_wire_control_as_a_switch_terminal() {
        let model = analog_switch_entry("SPST with only com + ctrl wired");
        let mut circuit = Circuit::new();
        let roles = HashMap::from([
            ("com".to_string(), circuit.node("SIG")),
            ("ctrl".to_string(), circuit.node("GATE")),
        ]);
        let (outcome, _) = bind_analog_switch(
            &comp("U7", ""),
            &model,
            &mut circuit,
            &roles,
            &HashMap::new(),
        );
        assert!(vswitches(&circuit).is_empty());
        assert!(
            matches!(outcome, BindOutcome::Unresolved { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn shared_select_dpdt_stamps_both_differential_lanes_and_refuses_unmodeled_oe() {
        let card = |ron: &[(&str, f64)]| {
            let mut params = hauksbee_models::Params::default();
            for (k, v) in ron {
                params.set_f64(*k, *v);
            }
            let what = "two SPDT lanes with one select and active-low output enable";
            make_entry(
                "generic_shared_select_dpdt",
                ComponentKind::AnalogSwitch,
                what,
                params,
                std::collections::BTreeMap::new(),
            )
        };
        let model = card(&[("ron", 5.0)]);
        let build_roles = |circuit: &mut Circuit, oe: NodeId| {
            HashMap::from([
                ("com_1".to_string(), circuit.node("D_PLUS")),
                ("s0_1".to_string(), circuit.node("D1_PLUS")),
                ("s1_1".to_string(), circuit.node("D2_PLUS")),
                ("com_2".to_string(), circuit.node("D_MINUS")),
                ("s0_2".to_string(), circuit.node("D1_MINUS")),
                ("s1_2".to_string(), circuit.node("D2_MINUS")),
                ("ctrl".to_string(), circuit.node("SELECT")),
                ("vcc".to_string(), circuit.node("+3V3")),
                ("vss".to_string(), NodeId::GROUND),
                ("oe_n".to_string(), oe),
            ])
        };
        let power_nets = HashMap::from([("+3V3".to_string(), 3.3)]);

        let mut enabled = Circuit::new();
        let roles = build_roles(&mut enabled, NodeId::GROUND);
        let (outcome, warning) =
            bind_analog_switch(&comp("U8", ""), &model, &mut enabled, &roles, &power_nets);
        assert!(warning.is_none(), "{warning:?}");
        assert!(
            matches!(outcome, BindOutcome::Behavioral { ref device } if device == "shared-select dpdt")
        );
        assert_eq!(vswitches(&enabled).len(), 4, "two SPDT lanes are four legs");
        // Package metadata must cover all four legs, including distinct rated paths.
        assert_dpdt_package_metadata_and_carriers(&enabled, &roles);
        // A literal U8_l1 reference still owns its own flat-stamped switch legs.
        let mut literal = Circuit::new();
        let literal_roles = build_roles(&mut literal, NodeId::GROUND);
        let _ = bind_analog_switch(
            &comp("U8_l1", ""),
            &model,
            &mut literal,
            &literal_roles,
            &power_nets,
        );
        assert_eq!(literal.devices.len(), 4);
        assert!(literal
            .devices
            .iter()
            .all(|d| crate::stress::strip_unit_suffix(d.name()) == "U8_l1"));
        // Each leg takes its own path's ron; a card with only `ron` gives both legs it.
        let per_path = card(&[("ron", 5.0), ("ron_s0", 0.25), ("ron_s1", 1.3)]);
        for (model, s0, s1) in [(&model, 5.0, 5.0), (&per_path, 0.25, 1.3)] {
            let mut c = Circuit::new();
            let roles = build_roles(&mut c, NodeId::GROUND);
            let _ = bind_analog_switch(&comp("U8", ""), model, &mut c, &roles, &power_nets);
            let rons: Vec<_> = c
                .devices
                .iter()
                .filter_map(|d| match d {
                    Device::VSwitch { name, ron, .. } => Some((name.clone(), *ron)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                rons,
                [("U8_s1", s1), ("U8_s0", s0), ("U8_s3", s1), ("U8_s2", s0)]
                    .map(|(name, ron)| (name.to_owned(), ron))
            );
        }

        let mut dynamic_oe = Circuit::new();
        let oe = dynamic_oe.node("OE_N");
        let roles = build_roles(&mut dynamic_oe, oe);
        let (outcome, warning) = bind_analog_switch(
            &comp("U8", ""),
            &model,
            &mut dynamic_oe,
            &roles,
            &power_nets,
        );
        assert!(matches!(outcome, BindOutcome::Unresolved { .. }));
        assert!(
            vswitches(&dynamic_oe).is_empty(),
            "never model a dynamic OE as always enabled"
        );
        assert!(
            warning.as_deref().is_some_and(|m| m.contains("OE")),
            "{warning:?}"
        );
    }

    fn assert_dpdt_package_metadata_and_carriers(
        enabled: &Circuit,
        roles: &HashMap<String, NodeId>,
    ) {
        let lib = ModelLibrary::builtin();
        let fsusb = resolve_value(&lib, "FSUSB42", "");
        let mut part = comp("U8", "FSUSB42");
        part.pins = fsusb
            .pins
            .iter()
            .map(|(pad, role)| pin(pad, i64::from(roles[role].0)))
            .collect();
        let board = ExtractedBoard {
            name: "dpdt-rating".into(),
            nets: vec![],
            components: vec![part],
        };
        let node_of =
            |n: Option<i64>| n.map(|n| NodeId(u32::try_from(n).expect("valid synthetic node")));
        let metas = gather_device_meta(&board, &lib, enabled, &node_of, &[], &[], &[]);
        let mut names: Vec<_> = metas.iter().map(|m| m.reference.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["U8_s0", "U8_s1", "U8_s2", "U8_s3"]);
        for meta in &metas {
            assert_eq!(meta.kind, ComponentKind::AnalogSwitch);
            assert_eq!(meta.ratings.max_voltage_v, Some(5.6));
            let stamped = enabled.devices[meta.device.0 as usize].name();
            assert_eq!(stamped, meta.reference);
        }
        // Each throw has its lane's path; the whole package owns all four legs.
        let row = |lane| {
            (
                (
                    hauksbee_models::derating::TempRef::Ambient,
                    None,
                    None,
                    vec![format!("s0_{lane}")],
                ),
                vec![(25.0, 0.1)],
            )
        };
        let mut derating = hauksbee_models::derating::Derating {
            current: hauksbee_models::derating::Curve(vec![row(1), row(2)]),
            ..Default::default()
        };
        let units: Vec<_> = metas.iter().map(|m| m.device).collect();
        derating.carriers = crate::stress::rated_paths(enabled, "U8", &derating, roles, &units);
        for lane in [1, 2] {
            let path = vec![format!("s0_{lane}")];
            let carriers = &derating
                .carriers
                .iter()
                .find(|(p, _)| *p == path)
                .unwrap()
                .1;
            assert_eq!(carriers.len(), 1, "one s0 leg on lane {lane}");
            assert_eq!(
                enabled.devices[carriers[0].0 .0 as usize].name(),
                format!("U8_s{}", (lane - 1) * 2)
            );
        }
        let whole_package = derating.carriers.iter().find(|(path, _)| path.is_empty());
        assert_eq!(whole_package.expect("whole-package carrier").1.len(), 4);
    }

    /// The s0 / NC throw conducts when the control is LOW, so a partially
    /// wired com + s0 + ctrl switch stamps a `VSwitch` with inverted sense.
    #[test]
    fn spst_fallback_s0_throw_conducts_on_control_low() {
        let model = analog_switch_entry("NC SPST: com + s0 + ctrl");
        let mut circuit = Circuit::new();
        let gate = circuit.node("GATE");
        let roles = HashMap::from([
            ("com".to_string(), circuit.node("SIG")),
            ("s0".to_string(), circuit.node("OUT")),
            ("ctrl".to_string(), gate),
        ]);
        let _ = bind_analog_switch(
            &comp("U8", ""),
            &model,
            &mut circuit,
            &roles,
            &HashMap::new(),
        );
        let (_, _, cp, cn, von, voff) = vswitches(&circuit)[0];
        assert_eq!(cp, NodeId::GROUND, "s0 throw senses (vss - ctrl)");
        assert_eq!(cn, gate);
        assert!(von < 0.0 && voff < von, "got von={von} voff={voff}");
    }

    /// A com/s0/s1/ctrl SPDT conducts com<->s0 on a low ctrl and com<->s1 on a
    /// high one through a DC solve, with a `vcc` pin (the s0 leg senses vcc -
    /// ctrl) or without one (a power mux: both legs reference vss).
    #[test]
    fn spdt_with_or_without_vcc_follows_ctrl_through_a_dc_solve() {
        use hauksbee_solve::{dc_operating_point, SolverOptions, Workspace};
        use hauksbee_testkit::circuits::{res, vdc};
        let names = ["com", "s0:in1", "s1:in2", "ctrl", "vss:gnd", "vcc:+3v3"];
        let rails = HashMap::from([("+3V3".to_string(), 3.3)]);
        for (n, ctrl_v, want) in [(5, 0.0, 5.0), (5, 3.3, 3.0), (6, 0.0, 5.0), (6, 3.3, 3.0)] {
            let (mut c, model) = (Circuit::new(), analog_switch_entry("SPDT"));
            let r = roles(&mut c, &names[..n]);
            let bound = bind_analog_switch(&comp("U9", ""), &model, &mut c, &r, &rails);
            for (role, v) in [("s0", 5.0), ("s1", 3.0), ("ctrl", ctrl_v), ("vcc", 3.3)] {
                if let Some(&node) = r.get(role) {
                    vdc(&mut c, role, node, v);
                }
            }
            res(&mut c, "RLOAD", r["com"], NodeId::GROUND, 1e6);
            let mut ws = Workspace::new(&c);
            dc_operating_point(&mut ws, &c, &SolverOptions::default()).unwrap();
            let got = ws.layout.node(r["com"]).map_or(0.0, |i| ws.x[i]);
            // Before, the no-vcc card fell to the single-throw path: com floated
            // at 5 mV (open, loaded through s0's roff) with ctrl high.
            assert!((got - want).abs() < 1e-3, "{n} roles/{ctrl_v} V: {got}");
            assert!(
                matches!(bound, (BindOutcome::Analog { .. }, None)),
                "{bound:?}"
            );
        }
    }

    #[test]
    fn spdt_no_nc_map_to_the_correct_throws() {
        let role = |f: &str| role_from_pinfunction(ComponentKind::AnalogSwitch, f);
        assert_eq!(role("nc").as_deref(), Some("s0"));
        assert_eq!(role("no").as_deref(), Some("s1"));
        assert_eq!(role("b1").as_deref(), Some("s0"));
        assert_eq!(role("b2").as_deref(), Some("s1"));
    }

    /// A matched-pair BJT whose second unit is partially wired must warn
    /// "left open" for that unit rather than dropping it silently.
    #[test]
    fn partially_wired_paired_bjt_unit_warns_left_open() {
        let model: ModelEntry = toml::from_str(
            "id = \"bcm847bs\"\nkind = \"bjt_npn\"\n[match]\nvalue_re = \"BCM847\"\n",
        )
        .unwrap();
        let mut circuit = Circuit::new();
        let mut roles = roles(
            &mut circuit,
            &["collector_q1", "base_q1", "emitter_q1", "base_q2"],
        );
        let (outcome, warning) = bind_bjt(&comp("Q1", "BCM847BS"), &model, &mut circuit, &roles);
        assert!(
            matches!(outcome, BindOutcome::Analog { .. }),
            "Q1 still stamps"
        );
        let w = warning.expect("a partially-wired Q2 must warn");
        assert!(w.contains("left open") && w.contains("q2"), "{w}");

        roles.insert("collector_q2".to_string(), circuit.node("C2"));
        roles.insert("emitter_q2".to_string(), circuit.node("E2"));
        let (_, warning) = bind_bjt(&comp("Q1", "BCM847BS"), &model, &mut circuit, &roles);
        assert!(warning.is_none(), "{warning:?}");
    }

    #[test]
    fn frequency_values_are_recognised() {
        for v in [
            "16MHz",
            "8Mhz",
            "16.000MHz",
            "32.768kHz",
            "100 Hz",
            "16 MHz",
            "1 GHz",
        ] {
            assert!(value_is_frequency(v), "{v} should read as a frequency");
        }
        for v in [
            "22pF",
            "10k",
            "4k7",
            "100nF",
            "BCM857BS",
            "Hz",
            "",
            "600@100MHz",
            "1k@100MHz",
        ] {
            assert!(!value_is_frequency(v), "{v} must NOT read as a frequency");
        }
    }

    #[test]
    fn crystal_detected_by_reference_or_frequency_value() {
        assert!(is_crystal_like("Y", "16MHz"));
        assert!(is_crystal_like("CRYSTAL", ""));
        assert!(is_crystal_like("XTAL", "8MHz"));
        assert!(is_crystal_like("C", "16Mhz"));
        assert!(!is_crystal_like("C", "22pF"));
        assert!(!is_crystal_like("R", "10k"));
    }

    #[test]
    fn sourced_inductor_dcr_and_ferrite_dc_resistance_reach_the_circuit() {
        let lib = ModelLibrary::builtin();
        let mut circuit = Circuit::new();
        let a = circuit.node("VIN");
        let b = circuit.node("SW");
        let node_of = |n: Option<i64>| match n {
            Some(1) => Some(a),
            Some(2) => Some(b),
            _ => None,
        };

        let mut inductor = comp("L5", "SRN6045TA-3R3Y");
        inductor.pins = vec![pin("1", 1), pin("2", 2)];
        let l_model = lib
            .resolve(&ComponentQuery::new(
                None,
                Some(inductor.value.clone()),
                None,
            ))
            .model
            .expect("exact Bourns inductor model");
        let (outcome, warning) = bind_passive(&inductor, &l_model, &mut circuit, &node_of);
        assert!(matches!(outcome, BindOutcome::Analog { .. }));
        assert!(warning.as_deref().is_some_and(|w| w.contains("saturation")));
        assert!(circuit.devices.iter().any(|d| matches!(d,
            Device::Inductor { name, henries, .. }
                if name == "L5" && (*henries - 3.3e-6).abs() < 1e-12
        )));
        assert!(circuit.devices.iter().any(|d| matches!(d,
            Device::Resistor { name, ohms, .. }
                if name == "L5_dcr" && (*ohms - 0.021).abs() < 1e-12
        )));

        let mut bead = comp("FB1", "220@100MHz 1.4A");
        bead.footprint = "Inductor_SMD:L_0603_1608Metric".into();
        bead.pins = vec![pin("1", 1), pin("2", 2)];
        let bead_model = lib
            .resolve(&ComponentQuery::new(
                None,
                Some(bead.value.clone()),
                Some(bead.footprint.clone()),
            ))
            .model
            .expect("exact rated ferrite family model");
        let before = circuit.devices.len();
        let (outcome, _) = bind_passive(&bead, &bead_model, &mut circuit, &node_of);
        assert!(matches!(outcome, BindOutcome::Analog { .. }));
        let stamped = &circuit.devices[before..];
        assert_eq!(stamped.len(), 1, "ferrite DC behavior is one resistor");
        assert!(
            matches!(&stamped[0], Device::Resistor { ohms, .. } if (*ohms - 0.1).abs() < 1e-12)
        );
    }

    /// A bead's impedance-at-100-MHz marking on an L designator is ohms, never
    /// henries; ordinary inductor values on the same designator still bind as L.
    #[test]
    fn ferrite_bead_ratings_on_inductor_designators_never_become_henries() {
        let lib = ModelLibrary::builtin();
        for (value, henries) in [
            ("600R/2A/UPZ2012E-601-2R0TF", None),
            ("120Ω@100MHz", None),
            ("120R @ 100MHz", None),
            ("600R", None),
            ("4.7uH", Some(4.7e-6)),
            ("10u", Some(10e-6)),
            ("2R2", Some(2.2e-6)),
        ] {
            let mut circuit = Circuit::new();
            let (a, b) = (circuit.node("A"), circuit.node("B"));
            let node_of = |n: Option<i64>| [None, Some(a), Some(b)][usize::try_from(n?).ok()?];
            let mut l = comp("L1", value);
            l.footprint = "Inductor_SMD:L_0805_2012Metric".into();
            l.pins = vec![pin("1", 1), pin("2", 2)];
            let model = library_resolution(&lib, &l).model.expect(value);
            let (outcome, _) = bind_passive(&l, &model, &mut circuit, &node_of);
            let got = circuit.devices.iter().find_map(|d| match d {
                Device::Inductor { henries, .. } => Some(*henries),
                _ => None,
            });
            let close_to = |h: f64| got.is_some_and(|g| (g - h).abs() < h * 1e-9);
            let ok = henries.map_or_else(|| got.is_none(), close_to);
            assert!(ok, "{value}: {got:?} H, {outcome:?}, model {}", model.id);
        }
        // Ohm-marked resistors and jumpers stay resistors, not beads.
        for value in ["0R", "100R"] {
            let mut r = comp("R1", value);
            r.footprint = "Resistor_SMD:R_0603_1608Metric".into();
            let id = library_resolution(&lib, &r).model.map(|m| m.id);
            assert!(
                !id.as_deref().is_some_and(|i| i.starts_with("ferrite")),
                "{value}: {id:?}"
            );
        }
    }

    /// A library-resolved crystal is Skipped and never stamped (a bare
    /// "32.768" used to become a 32.768 ohm resistor); a packaged oscillator
    /// additionally carries a warning naming its unmodelled supply and clock.
    #[test]
    fn a_library_resolved_crystal_is_skipped_and_never_stamped() {
        let lib = ModelLibrary::builtin();
        for (reference, value, footprint) in [
            ("Q1", "16Mhz", ""),
            ("Y1", "40MHz", ""),
            (
                "X201",
                "XTCLH40M000CHJA0P0",
                "Oscillator:Oscillator_SMD_Fordahl_DFAS15-4Pin_5.0x3.2mm",
            ),
            (
                "Y401",
                "Abracon_ABM11",
                "Crystal:Crystal_SMD_Abracon_ABM3-4Pin_2.0x1.6mm",
            ),
            ("Y2", "32.768", ""),
            ("Y3", "32.768kHz", ""),
        ] {
            let mut c = comp(reference, value);
            c.footprint = footprint.to_string();
            c.pins = vec![pin("1", 1), pin("2", 2)];
            let mut circuit = Circuit::new();
            let a = circuit.node("XA");
            let b = circuit.node("XB");
            let node_of = move |n: Option<i64>| match n {
                Some(1) => Some(a),
                Some(2) => Some(b),
                _ => None,
            };
            let devices_before = circuit.devices.len();
            let model = resolve_value(&lib, value, footprint);
            assert_eq!(
                model.passive_class,
                Some(hauksbee_models::schema::PassiveClass::Crystal),
                "{reference} ({value}) resolved to {}",
                model.id
            );
            let (outcome, warning) = bind_one(&c, &model, &mut circuit, &node_of, &HashMap::new());
            assert!(
                matches!(&outcome, BindOutcome::Skipped { reason } if reason.contains("crystal")),
                "{reference} ({value}): {outcome:?}"
            );
            if footprint.to_ascii_lowercase().contains("oscillator") {
                let w = warning.unwrap_or_else(|| panic!("{reference} oscillator must warn"));
                assert!(
                    w.contains(reference) && w.contains("supply") && w.contains("clock"),
                    "{w}"
                );
            } else {
                assert!(warning.is_none(), "{reference} ({value}): {warning:?}");
            }
            assert_eq!(
                circuit.devices.len(),
                devices_before,
                "{reference} stamped a device"
            );
        }
    }

    /// A 'C'-referenced crystal with a frequency value must not bind as a
    /// (gigafarad) capacitor; a real 'C' capacitor still binds as a passive.
    #[test]
    fn crystal_named_with_c_prefix_is_high_impedance_not_a_capacitor() {
        let entry = fallback_entry(&comp("Crystal1", "16Mhz")).expect("crystal binds");
        assert_eq!(entry.id, "crystal_fallback");
        assert_eq!(entry.kind, ComponentKind::Ignore);
        let cap = fallback_entry(&comp("C7", "22pF")).expect("cap binds");
        assert_eq!(cap.kind, ComponentKind::Passive);
    }

    /// The three-terminal SOT-23 pin rule reads every CAD tool's spelling of
    /// the package, is guarded by `pad_count = 3`, and never applies to TO-92.
    #[test]
    fn the_three_terminal_pin_rules_read_every_cad_tools_package_spelling() {
        let lib = ModelLibrary::builtin();
        let rules = lib.pin_rules();
        for footprint in [
            "SOT-23",
            "SOT-23_Handsoldering",
            "Package_TO_SOT_SMD:SOT-23",
            "SOT23-3",
            "SOT23-3@1",
            "SOT23",
            "SOT323",
            "SOT-323",
            "SC-70",
            "SC70-3",
            "TO-236",
        ] {
            for (kind, expected) in [
                (ComponentKind::Nmos, ["gate", "source", "drain"]),
                (ComponentKind::Pmos, ["gate", "source", "drain"]),
                (ComponentKind::BjtNpn, ["base", "emitter", "collector"]),
                (ComponentKind::BjtPnp, ["base", "emitter", "collector"]),
            ] {
                for (i, role) in expected.iter().enumerate() {
                    let pad = (i + 1).to_string();
                    let got = rules.role_for_pad(footprint, Some(kind), 3, &pad);
                    assert_eq!(
                        got.as_ref().map(|r| r.role.as_str()),
                        Some(*role),
                        "{footprint} {kind:?} pad {pad}"
                    );
                }
            }
        }
        for (footprint, pads) in [
            ("SOT-23-5", 5),
            ("SOT-23-6", 6),
            ("SOT23-6L", 6),
            ("SOT-353_SC-70-5", 5),
            ("SC-70_6", 6),
        ] {
            assert!(
                rules
                    .role_for_pad(footprint, Some(ComponentKind::Nmos), pads, "1")
                    .is_none(),
                "{footprint} has {pads} pads"
            );
        }
        for footprint in ["TO-92", "TO92-3", "Package_TO_SOT_THT:TO-92"] {
            assert!(
                rules
                    .role_for_pad(footprint, Some(ComponentKind::BjtNpn), 3, "1")
                    .is_none(),
                "{footprint} must not take the SOT-23 pad order"
            );
        }
    }

    #[test]
    fn role_sources_have_global_priority_not_per_pad_priority() {
        let mut c = comp("D1", "D");
        let map = std::iter::once(("1".into(), "anode".into())).collect();
        let node_of = |net: Option<i64>| net.map(|id| NodeId(u32::try_from(id).unwrap()));
        let mut named = pin("2", 2);
        named.function = "A".into();
        for (pins, kind, expected) in [
            (
                vec![pin("1", 1), named, pin("A", 3), pin("P$1", 4)],
                Some(ComponentKind::Diode),
                2,
            ),
            (
                vec![pin("A", 3), pin("1", 1), pin("P$1", 4)],
                Some(ComponentKind::Diode),
                1,
            ),
            (
                vec![pin("P$1", 4), pin("A", 3)],
                Some(ComponentKind::Diode),
                3,
            ),
            (vec![pin("A", 3), pin("P$1", 4)], None, 4),
        ] {
            c.pins = pins;
            assert_eq!(
                role_node_map_pins(&c, &map, kind, &node_of)["anode"],
                NodeId(expected)
            );
        }
    }

    /// On a module the Nano d-number table applies first, then a port-named
    /// role (Pro Micro) falls through to the standard port parser; the ADC
    /// channel comes from the role's own digits.
    #[test]
    fn a_module_role_named_by_its_port_falls_through_the_nano_d_number_table() {
        assert_eq!(gpio_of_role("d0_rx", true), Some(('D', 0)));
        assert_eq!(gpio_of_role("d5_pwm", true), Some(('D', 5)));
        assert_eq!(gpio_of_role("d8", true), Some(('B', 0)));
        assert_eq!(gpio_of_role("d13_sck", true), Some(('B', 5)));
        assert_eq!(gpio_of_role("a2", true), None);
        assert_eq!(apin_gpio_of_role("a2", true), Some(('C', 2)));
        assert_eq!(apin_gpio_of_role("a6", true), None, "A6/A7 are ADC-only");
        assert_eq!(adc_of_role("a3", true), Some(3));

        for (role, want) in [
            ("pd3_txd1", ('D', 3)),
            ("pc6", ('C', 6)),
            ("pe6", ('E', 6)),
            ("pb4_adc11", ('B', 4)),
            ("pf7_adc7", ('F', 7)),
        ] {
            assert_eq!(gpio_of_role(role, true), Some(want), "{role}");
            assert_eq!(gpio_of_role(role, false), Some(want), "{role}");
        }
        assert_eq!(adc_of_role("pd4_adc8", true), Some(8));
        assert_eq!(adc_of_role("pb4_adc11", true), Some(11));
        assert_eq!(adc_of_role("pc6", true), None);
        for role in ["vcc", "gnd", "raw", "reset", ""] {
            assert_eq!(gpio_of_role(role, true), None, "{role} is not a GPIO");
        }
    }

    /// The powered-oscillator caveat keys on a pin sitting on a resolved
    /// supply rail, not on the footprint string containing "oscillator".
    #[test]
    fn a_powered_oscillator_is_disclosed_by_its_wiring_not_by_its_footprint_string() {
        let lib = ModelLibrary::builtin();
        for (footprint, on_rail, expect_warning) in [
            ("complib:OSC_SMD_3225_4P", true, true),
            ("Oscillator:Oscillator_SMD_2520_4Pin", false, true),
            ("Crystal:Crystal_SMD_3225-4Pin_3.2x2.5mm", false, false),
        ] {
            let mut c = comp("X1", "40MHz");
            c.footprint = footprint.to_string();
            c.pins = vec![pin("1", 1), pin("2", 0), pin("3", 2), pin("4", 3)];
            let mut circuit = Circuit::new();
            let xin = circuit.node("XIN");
            let clk = circuit.node("CLK_OUT");
            let rail = circuit.node("+3V3");
            let node_of = move |n: Option<i64>| match n {
                Some(1) => Some(xin),
                Some(2) => Some(clk),
                Some(3) => Some(rail),
                Some(0) => Some(NodeId::GROUND),
                _ => None,
            };
            let mut power_nets = HashMap::new();
            if on_rail {
                power_nets.insert("+3V3".to_string(), 3.3);
            }
            let model = resolve_value(&lib, "40MHz", footprint);
            let (outcome, warning) = bind_one(&c, &model, &mut circuit, &node_of, &power_nets);
            assert!(
                matches!(outcome, BindOutcome::Skipped { .. }),
                "{footprint}: {outcome:?}"
            );
            if expect_warning {
                let w = warning.unwrap_or_else(|| panic!("{footprint} must warn"));
                assert!(w.starts_with(Assumption::PARTIAL_MODEL_MARKER), "{w}");
            } else {
                assert!(warning.is_none(), "{footprint}: {warning:?}");
            }
        }
    }

    /// Wire a resolved part's pads BY ROLE out of its own pin map.
    fn pad_for<'a>(model: &'a ModelEntry, role: &str) -> Option<&'a str> {
        model
            .pins
            .iter()
            .find(|(_, r)| r.as_str() == role)
            .map(|(pad, _)| pad.as_str())
    }

    /// A regulator whose entry names an enable, wired OFF, must say the enable
    /// is not modelled; a three-terminal regulator stays silent.
    #[test]
    fn a_regulator_says_its_enable_is_not_modelled_and_a_plain_one_says_nothing() {
        let lib = ModelLibrary::builtin();
        for (value, expects_enable_caveat) in [("XC6204B332MR", true), ("LM7805", false)] {
            let mut model = resolve_value(&lib, value, "");
            if !expects_enable_caveat {
                // TI LM340/LM7805 SNOSBT0L Rev. L, page 1 says no output
                // capacitor is required for stability. Keep that vendor-specific
                // fact local: the broad 7805 card also matches unrelated vendors.
                model.params.set_f64("cout_min_f", 0.0);
            }
            let mut c = comp("U1", value);
            let mut pins = vec![pin(pad_for(&model, "out").expect("an out pad"), 1)];
            if let Some(inp) = pad_for(&model, "in") {
                pins.push(pin(inp, 2));
            }
            if let Some(gnd) = pad_for(&model, "gnd").or_else(|| pad_for(&model, "vss")) {
                pins.push(pin(gnd, 0));
            }
            if let Some(en) = pad_for(&model, "en").or_else(|| pad_for(&model, "shdn")) {
                pins.push(pin(en, 0));
            }
            c.pins = pins;
            let mut circuit = Circuit::new();
            let (out, inn) = (circuit.node("RAIL_OUT"), circuit.node("RAIL_IN"));
            let node_of = move |n: Option<i64>| match n {
                Some(1) => Some(out),
                Some(2) => Some(inn),
                Some(0) => Some(NodeId::GROUND),
                _ => None,
            };
            let (outcome, warning) = bind_one(&c, &model, &mut circuit, &node_of, &HashMap::new());
            assert!(
                !matches!(outcome, BindOutcome::Unresolved { .. }),
                "{value}: {outcome:?}"
            );
            if expects_enable_caveat {
                let w = warning.unwrap_or_else(|| panic!("{value} must disclose its enable"));
                assert!(w.starts_with(Assumption::PARTIAL_MODEL_MARKER), "{w}");
                assert!(w.contains("not modelled") && w.contains("OFF"), "{w}");
            } else {
                assert!(warning.is_none(), "{value}: {warning:?}");
            }
        }
    }

    /// One role on pads of two nets refuses the part; on one net it binds.
    #[test]
    fn one_role_on_pads_of_two_nets_is_refused_not_half_bound() {
        let pins =
            [("1", "anode"), ("2", "cathode"), ("3", "cathode")].map(|(p, r)| (p.into(), r.into()));
        let model = make_entry(
            "dual",
            ComponentKind::Diode,
            "dual",
            hauksbee_models::Params::default(),
            pins.into(),
        );
        for (third, refused) in [(1, false), (2, true)] {
            let mut c = comp("D1", "dual");
            c.pins = vec![pin("1", 0), pin("2", 1), pin("3", third)];
            let mut circuit = Circuit::new();
            let nodes = [NodeId::GROUND, circuit.node("A"), circuit.node("B")];
            let node_of = move |n: Option<i64>| Some(nodes[usize::try_from(n?).ok()?]);
            let (outcome, warning) = bind_one(&c, &model, &mut circuit, &node_of, &HashMap::new());
            let split =
                warning.is_some_and(|w| w.contains("pads 2 and 3 both carry role 'cathode'"));
            assert_eq!(
                matches!(outcome, BindOutcome::Unresolved { .. }) && split,
                refused,
                "{outcome:?}"
            );
        }
    }

    /// A part with its pads on nets `nets` (-1: pad unwired), numbered from 1.
    fn part(r: &str, v: &str, lib: &str, fp: &str, nets: &[i64]) -> Component {
        let pin = |(&net, i): (&i64, i32)| Pin {
            number: i.to_string(),
            net: (net >= 0).then_some(net),
            ..Default::default()
        };
        let pins = nets.iter().zip(1..).map(pin).collect();
        let mut c = comp(r, v);
        (c.lib_id, c.footprint, c.pins) = (lib.into(), fp.into(), pins);
        c
    }

    /// Bind `c` to what the builtin library resolves it to, net `n` on node "N<n>":
    /// the circuit, the bind outcome (`None`: no model) and its warning.
    fn bind_resolved(c: &Component) -> (Circuit, Option<BindOutcome>, Option<String>) {
        let model = library_resolution(&ModelLibrary::builtin(), c).model;
        let mut circuit = Circuit::new();
        let nodes: Vec<_> = (0..4).map(|i| circuit.node(&format!("N{i}"))).collect();
        let node_of = move |n: Option<i64>| Some(nodes[usize::try_from(n?).ok()?]);
        let bound = model.map(|m| bind_one(c, &m, &mut circuit, &node_of, &HashMap::new()));
        let (outcome, warning) = bound.unzip();
        (circuit, outcome, warning.flatten())
    }

    fn analog(outcome: Option<&BindOutcome>) -> bool {
        matches!(outcome, Some(BindOutcome::Analog { .. }))
    }

    /// Each diode stamped, as "name anode>cathode".
    fn diodes(circuit: &Circuit) -> String {
        let name = |n: NodeId| circuit.node_name(n).to_string();
        let found = circuit.devices.iter().filter_map(|d| match d {
            Device::Diode { name: d, a, k, .. } => Some(format!("{d} {}>{}", name(*a), name(*k))),
            _ => None,
        });
        found.collect::<Vec<_>>().join(" ")
    }

    /// Two diodes in one package bind one device each, whichever pads they share; with
    /// the shared pad unwired the part stays open, and one wired unit binds alone.
    #[test]
    fn dual_diode_packages_bind_each_unit() {
        for (value, nets, want) in [
            ("BAV70", [1, 2, 3], "D1_e1 N1>N3 D1_e2 N2>N3"),
            ("BAT54C", [1, 2, 3], "D1_e1 N1>N3 D1_e2 N2>N3"),
            ("BAT54A", [1, 2, 3], "D1_e1 N3>N1 D1_e2 N3>N2"),
            ("BAT54S", [1, 2, 3], "D1_e1 N1>N3 D1_e2 N3>N2"),
            ("BAV99W", [1, 2, 3], "D1_e1 N1>N3 D1_e2 N3>N2"),
            ("BAT54", [1, 2, 3], "D1 N1>N3"),
            ("BAV70", [1, 2, -1], ""),
            ("BAV99", [1, -1, 3], "D1_e1 N1>N3"),
        ] {
            let c = part("D1", value, "Device:D", "Package_TO_SOT_SMD:SOT-23", &nets);
            let (circuit, outcome, _) = bind_resolved(&c);
            assert_eq!(diodes(&circuit), want, "{value} {nets:?}");
            assert_eq!(
                analog(outcome.as_ref()),
                !want.is_empty(),
                "{value} {outcome:?}"
            );
        }
    }

    /// A net tie of any pad count is a milliohm link, never an open part; a blank or
    /// placeholder passive value is called a placeholder, a malformed one unparseable.
    #[test]
    fn net_ties_bind_as_links_and_placeholders_are_named() {
        for (value, fp, nets, links) in [
            ("NetTie_2", "", &[1, 2][..], 1),
            ("NetTie_3", "", &[1, 2, 3], 2),
            ("", "NetTie:NetTie-2_SMD_Pad0.5mm", &[1, 2], 1),
        ] {
            let (circuit, outcome, _) = bind_resolved(&part("NT1", value, "", fp, nets));
            let bound = analog(outcome.as_ref()) && circuit.devices.len() == links;
            assert!(bound, "{value} {nets:?} {outcome:?}");
        }
        for (value, want) in [("", "placeholder"), ("banana", "unparseable")] {
            let (_, _, warning) = bind_resolved(&part("R1", value, "Device:R", "", &[1, 2]));
            let named = warning.is_some_and(|w| w.contains(&format!("{want} value")));
            assert!(named, "{value:?}");
        }
        let (_, outcome, warning) = bind_resolved(&part("R1", "10k", "Device:R", "", &[1, 2]));
        assert!(analog(outcome.as_ref()) && warning.is_none());
    }

    /// A colour-only LED value takes that colour's card and records the assumption; an
    /// addressable LED, a resistor or an RGB part valued with a colour never does.
    #[test]
    fn colour_only_led_values_bind_the_colour_card() {
        let (led, custom) = ("Device:LED", "Custom:LED_0603");
        for (value, lib, fp, want) in [
            ("red", led, custom, "led_red"),
            ("GREEN", led, custom, "led_green"),
            ("BLUE", led, custom, "led_blue"),
            ("ORANGE", led, custom, "led_yellow"),
            ("WHITE", led, custom, "led_blue"),
            ("MF-LED-0603-RED", led, custom, "led_red"),
            ("RED", "Device:D", "LED_SMD:LED_0603_1608Metric", "led_red"),
        ] {
            let (ckt, out, warn) = bind_resolved(&part("D1", value, lib, fp, &[1, 0]));
            assert!(
                analog(out.as_ref()) && !diodes(&ckt).is_empty(),
                "{value} {out:?}"
            );
            assert!(warn.is_some_and(|w| w.contains("assumed a typical") && w.contains(want)));
        }
        for (value, lib, fp, nets) in [
            ("WS2812B", led, "LED_SMD:LED_WS2812B-PLCC4", &[1; 4][..]),
            ("RED", "LED:RGB", "", &[1; 4]),
            ("RED", "Device:R", "", &[1, 0]),
            ("RED", "Device:D", "", &[1, 0]),
        ] {
            let (_, _, warn) = bind_resolved(&part("D2", value, lib, fp, nets));
            let plain = warn.is_none_or(|w| !w.contains("names only a colour"));
            assert!(plain, "{value} {lib}");
        }
    }

    /// A behaviorless `digital` entry stays open and explains why.
    #[test]
    fn a_logic_entry_that_models_only_a_pad_map_stays_open() {
        let lib = ModelLibrary::builtin();
        let mut model = resolve_value(&lib, "FT231X-Q", "");
        assert_eq!(model.params.get_bool("identity_only"), Some(true));
        model.params.0.remove("identity_only");
        let mut c = comp("U2", "FT231X-Q");
        c.pins = vec![pin("12", 1), pin("3", 0), pin("17", 2), pin("20", 1)];
        let mut circuit = Circuit::new();
        let (vcc, txd) = (circuit.node("V3V3"), circuit.node("MCU_RX"));
        let node_of = move |n: Option<i64>| match n {
            Some(1) => Some(vcc),
            Some(2) => Some(txd),
            Some(0) => Some(NodeId::GROUND),
            _ => None,
        };
        let (outcome, warning) = bind_one(&c, &model, &mut circuit, &node_of, &HashMap::new());
        assert!(
            matches!(outcome, BindOutcome::Unresolved { .. }),
            "{outcome:?}"
        );
        let w = warning.expect("the identity-only warning must surface");
        assert!(w.contains("USB stack") && w.contains("remains OPEN"), "{w}");
    }

    /// When an entry names the switched pads and one is unwired, no path may
    /// be stamped between other pads.
    #[test]
    fn a_switch_with_one_terminal_unwired_stamps_nothing_rather_than_bridging_other_pads() {
        let lib = ModelLibrary::builtin();
        let model = resolve_value(&lib, "PCA9306", "");
        // Everything except pad 3 (SCL1, the modelled channel's low side).
        let mut c = comp("U3", "PCA9306");
        c.pins = vec![
            pin("1", 0),
            pin("2", 3),
            pin("4", 4),
            pin("5", 5),
            pin("6", 1),
            pin("7", 6),
            pin("8", 2),
        ];
        let mut circuit = Circuit::new();
        let nodes: Vec<_> = ["SCL_HI", "EN", "VREF1", "SDA1", "SDA2", "VREF2"]
            .iter()
            .map(|n| circuit.node(n))
            .collect();
        let node_of = move |n: Option<i64>| match n {
            Some(0) => Some(NodeId::GROUND),
            Some(i @ 1..=6) => Some(nodes[usize::try_from(i).unwrap() - 1]),
            _ => None,
        };
        let before = circuit.devices.len();
        let (outcome, warning) = bind_one(&c, &model, &mut circuit, &node_of, &HashMap::new());
        assert_eq!(circuit.devices.len(), before, "no switch may be stamped");
        assert!(
            matches!(outcome, BindOutcome::Unresolved { .. }),
            "{outcome:?}"
        );
        assert!(warning.expect("an open switch warns").contains("terminal"));
    }

    fn bind_load_switch(
        lib: &ModelLibrary,
        value: &str,
        pads: [&str; 4],
    ) -> (Circuit, [NodeId; 3], BindOutcome, Option<String>) {
        let [a_pad, b_pad, ctrl_pad, vss_pad] = pads;
        let mut c = comp("U1", value);
        c.pins = vec![
            pin(a_pad, 1),
            pin(b_pad, 2),
            pin(ctrl_pad, 3),
            pin(vss_pad, 0),
        ];
        let mut circuit = Circuit::new();
        let nodes = [
            circuit.node("SWITCHED"),
            circuit.node("RAIL"),
            circuit.node("ENABLE"),
        ];
        let node_of = move |n: Option<i64>| match n {
            Some(0) => Some(NodeId::GROUND),
            Some(i @ 1..=3) => Some(nodes[usize::try_from(i).unwrap() - 1]),
            _ => None,
        };
        let model = resolve_value(lib, value, "");
        let (outcome, warning) = bind_one(&c, &model, &mut circuit, &node_of, &HashMap::new());
        (circuit, nodes, outcome, warning)
    }

    /// A switch entry that models only part of the part (or assumes an
    /// enable polarity) discloses it through its `warning` param; a completely
    /// modelled part stays silent.
    #[test]
    fn a_partly_modelled_switch_discloses_the_part_it_leaves_out() {
        let lib = ModelLibrary::builtin();
        for (value, terminals, phrase) in [
            ("TXS0108E", ["in_out_a", "in_out_b"], Some("eight")),
            ("PCA9306", ["in_out_a", "in_out_b"], Some("SDA")),
            // The mux still qualifies typical resistance and omitted transition timing.
            ("TPS2104", ["com", "s0"], Some("switching timing")),
            ("TPS22916YFP", ["in_out_a", "in_out_b"], Some("active-high")),
            ("MIC94090C6", ["in_out_a", "in_out_b"], None),
        ] {
            let model = resolve_value(&lib, value, "");
            let [a, b] = terminals;
            let pads = [a, b, "ctrl", "vss"]
                .map(|role| pad_for(&model, role).unwrap_or_else(|| panic!("{value} `{role}`")));
            let (_, _, outcome, warning) = bind_load_switch(&lib, value, pads);
            assert!(
                !matches!(outcome, BindOutcome::Unresolved { .. }),
                "{value}: {outcome:?}"
            );
            match phrase {
                Some(phrase) => {
                    let w = warning.unwrap_or_else(|| panic!("{value} must disclose its gap"));
                    assert!(w.contains("U1") && w.contains(phrase), "{value}: {w}");
                }
                None => assert!(warning.is_none(), "{value}: {warning:?}"),
            }
        }
    }

    /// The stamped switch bridges rail and switched net (never ground or the
    /// control line) and closes on a HIGH control with positive thresholds.
    #[test]
    fn an_active_high_load_switch_conducts_on_a_high_control() {
        let lib = ModelLibrary::builtin();
        for (value, pads) in [
            ("TPS22916YFP", ["A1", "A2", "B2", "B1"]),
            ("MIC94090C6", ["1", "4", "6", "2"]),
        ] {
            let (circuit, [out, inn, ctrl], outcome, _) = bind_load_switch(&lib, value, pads);
            assert!(
                !matches!(outcome, BindOutcome::Unresolved { .. }),
                "{value}: {outcome:?}"
            );
            let (a, b, ctrl_p, ctrl_n, von, voff) = *vswitches(&circuit)
                .first()
                .unwrap_or_else(|| panic!("{value} stamped no switch"));
            let terminals = [a, b];
            assert!(
                terminals.contains(&out) && terminals.contains(&inn),
                "{value}"
            );
            assert!(
                !terminals.contains(&NodeId::GROUND) && !terminals.contains(&ctrl),
                "{value}"
            );
            assert!(
                ctrl_p == ctrl && ctrl_n == NodeId::GROUND,
                "{value}: the switch must sense the control, not its inverse"
            );
            assert!(
                von > 0.0 && voff > 0.0 && von > voff,
                "{value}: von={von} voff={voff}"
            );
        }
    }

    /// The builtin mux selects both sources at guaranteed logic levels and
    /// predicts each path's voltage drop under load using its own resistance.
    #[test]
    fn the_power_mux_selects_both_paths_with_their_own_resistance() {
        use hauksbee_solve::{dc_operating_point, SolverOptions, Workspace};
        use hauksbee_testkit::circuits::{res, vdc};
        let lib = ModelLibrary::builtin();
        let model = resolve_value(&lib, "TPS2104", "");
        for (ctrl_v, supply, ron) in [
            (0.0, 5.0, 0.25),
            (0.8, 5.0, 0.25),
            (2.0, 3.3, 1.3),
            (3.3, 3.3, 1.3),
        ] {
            let mut part = comp("U1", "TPS2104");
            part.pins = vec![
                pin("1", 1),
                pin("2", 0),
                pin("3", 2),
                pin("4", 3),
                pin("5", 4),
            ];
            let mut circuit = Circuit::new();
            let nodes = ["EN", "IN2", "OUT", "IN1"].map(|name| circuit.node(name));
            let node_of = |n: Option<i64>| {
                n.and_then(|n| usize::try_from(n).ok()).map(|n| {
                    if n == 0 {
                        NodeId::GROUND
                    } else {
                        nodes[n - 1]
                    }
                })
            };
            let (outcome, warning) =
                bind_one(&part, &model, &mut circuit, &node_of, &HashMap::new());
            assert!(matches!(outcome, BindOutcome::Analog { .. }), "{outcome:?}");
            assert!(warning.unwrap().contains("switching timing"));
            assert_eq!(vswitches(&circuit).len(), 2);
            for (name, node, volts) in [
                ("VEN", nodes[0], ctrl_v),
                ("VAUX", nodes[1], 3.3),
                ("VMAIN", nodes[3], 5.0),
            ] {
                vdc(&mut circuit, name, node, volts);
            }
            res(&mut circuit, "RLOAD", nodes[2], NodeId::GROUND, 100.0);
            let mut ws = Workspace::new(&circuit);
            dc_operating_point(&mut ws, &circuit, &SolverOptions::default()).unwrap();
            let got = ws.x[ws.layout.node(nodes[2]).unwrap()];
            let want = supply * 100.0 / (100.0 + ron);
            assert!(
                (got - want).abs() < 1e-5,
                "ctrl {ctrl_v} V: {got} != {want}"
            );
        }
    }

    /// Regulator pin NAMES are portable where pad numbers are not; SHDN is
    /// its own role and must never be read as an active-high enable.
    #[test]
    fn a_regulator_binds_by_pin_name_when_the_board_gives_one() {
        let role = |f: &str| role_from_pinfunction(ComponentKind::Vreg, f);
        for (function, want) in [
            ("VIN", "in"),
            ("VOUT", "out"),
            ("GND", "gnd"),
            ("VSS", "gnd"),
            ("CE", "en"),
            ("EN", "en"),
            ("FB", "fb"),
            ("ADJ", "fb"),
            ("NC", "nc"),
        ] {
            assert_eq!(role(function).as_deref(), Some(want), "{function}");
        }
        for function in ["SHDN", "shutdown", "nSHDN"] {
            assert_eq!(role(function).as_deref(), Some("shdn"), "{function}");
        }
        for function in ["A", "B", "SDA", "P07", "OUT_B", ""] {
            assert_eq!(role(function), None, "{function}");
        }
    }

    fn stm32_clock_board(crystal_dnp: bool) -> ExtractedBoard {
        let mut mcu = comp("U1", "STM32F103C8T6");
        mcu.footprint = "Package_QFP:LQFP-48_7x7mm_P0.5mm".to_string();
        mcu.pins = vec![pin("5", 1), pin("6", 2)];
        let mut crystal = comp("Y1", "8MHz");
        crystal.footprint = "Crystal:Crystal_SMD_3225-4Pin".to_string();
        crystal.dnp = crystal_dnp;
        crystal.pins = vec![pin("1", 1), pin("3", 2), pin("2", 3), pin("4", 3)];
        ExtractedBoard {
            name: "clock-evidence".to_string(),
            nets: vec![net(1, "OSC_IN"), net(2, "OSC_OUT"), net(3, "GND")],
            components: vec![mcu, crystal],
        }
    }

    #[test]
    fn an_assembled_crystal_on_oscillator_pins_is_clock_presence_evidence_but_a_dnp_one_is_not() {
        let lib = ModelLibrary::builtin();
        let bound = bind_board(&stm32_clock_board(false), &lib);
        assert_eq!(bound.mcus.len(), 1);
        assert!(bound.mcus[0].role_nets.contains_key("osc_in"));
        assert!(bound.mcus[0].role_nets.contains_key("osc_out"));
        assert!(bound.mcus[0].external_clock_present);
        let bound = bind_board(&stm32_clock_board(true), &lib);
        assert!(!bound.mcus[0].external_clock_present);
    }

    #[test]
    fn melf_footprint_resistor_binds_as_passive_not_open() {
        for (reference, value, footprint, kind) in [
            (
                "R5",
                "10k",
                "Resistor_SMD:R_MELF_MELF0207",
                ComponentKind::Passive,
            ),
            ("L2", "10uH", "Inductor_SMD:L_MELF", ComponentKind::Passive),
            ("CR1", "5.1V", "Diode_SMD:D_SOD-123", ComponentKind::Diode),
            ("D9", "D", "Diode_SMD:D_SOD-123", ComponentKind::Diode),
        ] {
            let mut c = comp(reference, value);
            c.footprint = footprint.into();
            assert_eq!(fallback_entry(&c).unwrap().kind, kind, "{footprint}");
        }
    }

    #[test]
    fn a_word_reference_takes_its_passive_kind_from_the_footprint_family() {
        let letter = |reference, footprint: &str| {
            let mut c = comp(reference, "167");
            c.footprint = footprint.into();
            passive_letter(&c)
        };
        assert_eq!(letter("COIL", "Resistor_SMD:R_0805_2012Metric"), Some('R'));
        assert_eq!(letter("COIL", ""), Some('C'));
        // A real designator outranks a borrowed land pattern.
        assert_eq!(letter("R1", "Capacitor_SMD:C_0603_1608Metric"), Some('R'));
        assert_eq!(letter("U1", "Resistor_SMD:R_0805_2012Metric"), None);
    }

    #[test]
    fn diode_fallback_requires_a_body_not_a_library_or_driver_name() {
        let check = |reference, value, footprint: &str, want| {
            let mut c = comp(reference, value);
            c.footprint = footprint.into();
            assert_eq!(fallback_entry(&c).map(|m| m.kind), want, "{footprint}");
        };
        for (value, footprint) in [
            ("NCP1117ST33T3G", "RPI_COMPILED:onsemi_ncp1117st33t3g"),
            ("NCP1117ST33T3G", "RPI_COMPILED:SOT-223"),
            ("W25Q128JVSIQ", "RPI_COMPILED:generic_soic127p790x195-8"),
            ("unknown", "RPI_COMPILED:QFN-32"),
            ("unknown", "DiodeLib:SOIC-8"),
            ("unknown", "custom:XSOD-123"),
            ("unknown", "custom:SMALL_QFN"),
            ("unknown", "custom:D_DRIVER"),
            ("unknown", "custom:LED_DRIVER"),
            ("unknown", "custom:DIODE_DRIVER"),
            ("unknown", "custom:SOD123_DRIVER"),
            ("unknown", "custom:SOD--123"),
            ("unknown", "custom:DO9DRIVER"),
        ] {
            check("U1", value, footprint, None);
        }
        for name in [
            "D_SOD-123",
            "sod-323",
            "D_DO-35",
            "DO35",
            "SMA",
            "SMB",
            "SMC",
            "MELF",
            "MINIMELF",
            "DIODE_SOD123",
            "SOD-123F",
            "D_SOD-123FL_HandSoldering",
        ] {
            for library in ["", "RPI_COMPILED:"] {
                check(
                    "U1",
                    "unknown",
                    &format!("{library}{name}"),
                    Some(ComponentKind::Diode),
                );
            }
        }
        let mut diode = comp("D9", "D");
        for pads in [
            vec![pin("1", 1), pin("2", 2)],
            vec![pin("1", 1), pin("1", 1), pin("2", 2)],
        ] {
            diode.pins = pads;
            assert!(
                fallback_entry(&diode).is_some(),
                "deduplicate electrical pads"
            );
        }
        diode.pins.push(pin("3", 3));
        assert!(
            fallback_entry(&diode).is_none(),
            "an array is not one diode"
        );
        // Refusing an unmodelled LED corrects false coverage; it does not add coverage.
        for name in ["LED_0603", "D_LED_0603", "LED_DRIVER"] {
            check("D9", "D", &format!("custom:{name}"), None);
        }
        assert_eq!(
            resolve_value(&ModelLibrary::builtin(), "unknown", "LED_SMD:LED_0603").id,
            "led_generic"
        );
    }

    #[test]
    fn fmt_eng_scales_to_the_magnitude_and_carries_across_decades() {
        for (value, unit, want) in [
            (390e-12, "F", "390 pF"),
            (4.7e-9, "F", "4.7 nF"),
            (0.1e-6, "F", "100 nF"),
            (10e-6, "F", "10 µF"),
            (1200e-6, "F", "1.2 mF"),
            (2.2e-6, "H", "2.2 µH"),
            (4700.0, "Ω", "4.7 kΩ"),
            (1_000_000.0, "Ω", "1 MΩ"),
            (0.05, "Ω", "50 mΩ"),
            (0.0, "F", "0 F"),
            (f64::NAN, "Ω", "0 Ω"),
            (999_600.0, "Ω", "1 MΩ"),
            (999.6, "Ω", "1 kΩ"),
            (0.9996, "Ω", "1 Ω"),
            (999.6e-6, "F", "1 mF"),
            (990.0, "Ω", "990 Ω"),
            (2_200_000_000.0, "Ω", "2200 MΩ"),
        ] {
            assert_eq!(fmt_eng(value, unit), want);
        }
    }

    /// Rail names carry their own magnitude (never a "5V"/"3V3" substring
    /// guess), supply tokens without a magnitude resolve to nothing, and
    /// monitor / feedback taps and domain-suffixed signals are not rails.
    #[test]
    fn power_rail_voltage_reads_full_magnitudes_and_refuses_taps_and_signals() {
        for magnitude in ["+5V", "-5V", "NaNV", "infV", "0V", "1..2V", "5V_MON"] {
            assert_eq!(positive_rail_fallback(&format!("+{magnitude}")), None);
            assert_eq!(negative_rail_fallback(&format!("-{magnitude}")), None);
        }
        for (name, volts) in [
            ("+15V", 15.0),
            ("+15V0", 15.0),
            ("+15V_ANALOG", 15.0),
            ("+9V", 9.0),
            ("+5V", 5.0),
            ("+5V_USB", 5.0),
            ("VCC_5V", 5.0),
            ("-15V", -15.0),
            ("-5V", -5.0),
            ("-3V3_ANALOG", -3.3),
            ("VDD_1V8", 1.8),
            ("AVCC_2V5", 2.5),
            ("DVDD_0V9", 0.9),
            ("VDD_1.2V", 1.2),
            ("VCC_15V", 15.0),
            ("VDD_13V3", 13.3),
            ("VDD_3V3", 3.3),
            ("VCC_3.3V", 3.3),
            ("VBUS_65V", 65.0),
            ("VDD_63V3", 63.3),
            ("+65V", 65.0),
            ("3V3", 3.3),
            ("+3V3", 3.3),
            ("+3V3_DIG", 3.3),
            ("12V", 12.0),
            ("5V_SENSOR", 5.0),
            ("3V3_FBUS", 3.3),
        ] {
            assert_eq!(power_rail_voltage(name), Some(volts), "{name}");
        }
        for name in [
            "VDD",
            "VEE",
            "SENSE_1V2_MON",
            "SDA_3V3",
            "SCL_3V3",
            "IRQ_3.3V",
            "SDA_5V",
            "12V_FB",
            "3V3_SENSE",
            "5V_MON",
            "12V_DIV",
            "-12V_MON",
            "VDD_1V8_MON",
            "VCC_5V_MON",
            "VOUT_1V0_FB",
            "12V_DIVIDER",
            "3V3_SENSED",
            "5V_SENSING",
            "12V_MEASURE",
            "5V_MONITORED",
            "VDD_1V8_DIVIDER",
            "-12V_SENSED",
        ] {
            assert_eq!(power_rail_voltage(name), None, "{name}");
        }
    }

    /// RISC-V ESP32 variants must not fall through to the Xtensa catch-all,
    /// and only the STM32F072 has an F072 machine.
    #[test]
    fn mcu_family_routing_never_runs_firmware_on_the_wrong_core() {
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
        assert!(matches!(
            route_mcu_family_str("STM32F072CBT6"),
            Some(McuFamilyRoute::Backend {
                family: "STM32F072",
                backend: "renode:stm32f072"
            })
        ));
        for sibling in ["STM32F030C8T6", "STM32F091CBT6"] {
            assert!(matches!(
                route_mcu_family_str(sibling),
                Some(McuFamilyRoute::NoPlatform { family: "STM32F0" })
            ));
        }
    }

    #[test]
    fn gpio_roles_cover_module_analog_pins_and_stm32_ports_past_e() {
        for (role, want) in [
            ("PI31", ('I', 31)),
            ("p031_suffix", ('0', 31)),
            ("p119", ('1', 19)),
        ] {
            assert_eq!(gpio_of_role(role, false), Some(want), "{role}");
        }
        for role in [
            "p", "pÉ1", "p231", "pj1", "pa32", "p132", "pa256", "pa", "pa-1",
        ] {
            assert_eq!(gpio_of_role(role, false), None, "{role}");
        }
        for role in ["a0", "a2", "a5", "a3_scl"] {
            assert_eq!(gpio_of_role(role, true), None, "{role}");
        }
        assert_eq!(apin_gpio_of_role("a0", true), Some(('C', 0)));
        assert_eq!(apin_gpio_of_role("a5", true), Some(('C', 5)));
        assert_eq!(apin_gpio_of_role("a6", true), None, "A6 is ADC-only");
        assert_eq!(
            role_from_mcu_pinfunction_token("PF9"),
            Some("pf9".to_string())
        );
        assert_eq!(
            role_from_mcu_pinfunction_token("PI15"),
            Some("pi15".to_string())
        );
        assert_eq!(role_from_mcu_pinfunction_token("PZ0"), None);
        assert_eq!(gpio_of_role("pa0", false), Some(('A', 0)));
        assert_eq!(gpio_of_role("pf9", false), Some(('F', 9)));
        assert_eq!(gpio_of_role("pi15", false), Some(('I', 15)));
        assert_eq!(gpio_of_role("pz0", false), None);
    }

    /// A board with one 25xx SPI EEPROM at U5 whose pad 1 (`cs`) lands on
    /// `cs_pad_net`.
    fn eeprom_board(cs_pad_net: Option<i64>) -> ExtractedBoard {
        let mut ee = comp("U5", "25LC256-I/SN");
        ee.footprint = "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm".to_string();
        ee.pins = vec![
            Pin {
                net: cs_pad_net,
                ..pin("1", 0)
            },
            pin("2", 2),
            pin("4", 9),
            pin("5", 3),
            pin("6", 4),
            pin("8", 8),
        ];
        ExtractedBoard {
            name: "spi-cs-evidence".to_string(),
            nets: vec![
                net(1, "EE_CS"),
                net(2, "SPI_MISO"),
                net(3, "SPI_MOSI"),
                net(4, "SPI_SCK"),
                net(8, "+3V3"),
                net(9, "GND"),
            ],
            components: vec![ee],
        }
    }

    fn cs_net(board: &ExtractedBoard, reference: &str) -> Option<String> {
        model_role_cs(board, reference, &ModelLibrary::builtin()).and_then(|r| r.cs_net)
    }

    #[test]
    fn a_cs_wired_modeled_slave_yields_its_cs_net() {
        let lib = ModelLibrary::builtin();
        assert_eq!(
            model_role_cs(&eeprom_board(Some(1)), "U5", &lib).map(|r| (r.cs_net, r.model_id)),
            Some((Some("EE_CS".to_string()), "eeprom_25xx_spi".to_string()))
        );
    }

    /// Every case where a CS net must NOT be invented: an unconnected or
    /// grounded cs pad, a DNP or identity-refused or unmodeled slave, a
    /// duplicated designator, a missing reference, and a modeled part with no
    /// `cs` role at all.
    #[test]
    fn slaves_that_cannot_supply_a_framing_net_yield_nothing() {
        assert_eq!(
            cs_net(&eeprom_board(None), "U5"),
            None,
            "unconnected cs pad"
        );
        assert_eq!(cs_net(&eeprom_board(Some(9)), "U5"), None, "cs tied to GND");
        assert_eq!(
            cs_net(&eeprom_board(Some(1)), "U99"),
            None,
            "no such reference"
        );

        let mut dnp = eeprom_board(Some(1));
        dnp.components[0].dnp = true;
        assert_eq!(cs_net(&dnp, "U5"), None, "DNP slave");

        let mut refused = eeprom_board(Some(1));
        refused.components[0].properties.push((
            hauksbee_extract::DUPLICATE_REFERENCE_CONFLICT_KEY.to_string(),
            "U5 appears twice with different values".to_string(),
        ));
        assert!(matches!(
            AssemblyState::of(&refused.components[0]),
            AssemblyState::IdentityUnknown(_)
        ));
        assert_eq!(cs_net(&refused, "U5"), None, "identity-refused slave");

        let mut unmodeled = eeprom_board(Some(1));
        unmodeled.components[0].value = "SOME-UNLISTED-EEPROM".to_string();
        assert_eq!(cs_net(&unmodeled, "U5"), None, "unmodeled slave");

        let mut duplicated = eeprom_board(Some(1));
        let mut twin = duplicated.components[0].clone();
        twin.pins[0].net = Some(4);
        duplicated.components.push(twin);
        assert!(duplicated
            .components
            .iter()
            .all(|c| matches!(AssemblyState::of(c), AssemblyState::Present(_))));
        assert_eq!(cs_net(&duplicated, "U5"), None, "duplicated designator");

        let mut no_cs_role = eeprom_board(Some(1));
        no_cs_role.components[0].value = "74HC595".to_string();
        no_cs_role.components[0].footprint = "Package_SO:SOIC-16_3.9x9.9mm_P1.27mm".to_string();
        assert_eq!(cs_net(&no_cs_role, "U5"), None, "no `cs` role");
    }

    #[test]
    fn module_pin_functions_name_esp_and_nordic_gpios() {
        for (token, want) in [
            ("IO0", Some("gpio0")),
            ("IO39", Some("gpio39")),
            ("P0.13", Some("p013")),
            ("P1.15-LF", Some("p115")),
            ("P0.04/AIN2", Some("p004")),
        ] {
            assert_eq!(
                role_from_mcu_pinfunction_token(token).as_deref(),
                want,
                "{token}"
            );
        }
        for token in ["IO64", "IOREF", "P2.01", "P0.32"] {
            assert_eq!(role_from_mcu_pinfunction_token(token), None, "{token}");
        }
    }

    /// Value strings as real libraries spell them resolve to the module and
    /// part cards; package-gated AVR cards carry the datasheet pad numbering.
    #[test]
    fn module_and_avr_values_resolve_to_cards_with_backends() {
        let lib = ModelLibrary::builtin();
        for (value, footprint, id, backend) in [
            ("ESP32_WROOM32_SKINNY", "", "esp32_wroom", "qemu:esp32"),
            ("T-Micro32_Plus", "", "lilygo_t_micro32", "qemu:esp32"),
            ("nice_nano", "", "nice_nano", "renode:nrf52840"),
            (
                "ATTINY85-20S",
                "Package_SO:SOIC-8_5.3x5.3mm_P1.27mm",
                "attiny85_8pin",
                "simavr:attiny85",
            ),
            (
                "ATtiny85-MU",
                "Package_DFN_QFN:QFN-20-1EP_4x4mm_P0.5mm",
                "attiny85_qfn20",
                "simavr:attiny85",
            ),
            (
                "ATMEGA32U4-MU",
                "Package_DFN_QFN:QFN-44-1EP_7x7mm_P0.5mm",
                "atmega32u4",
                "simavr:atmega32u4",
            ),
        ] {
            let m = resolve_value(&lib, value, footprint);
            assert_eq!(m.id, id, "{value}");
            assert_eq!(m.params.get_str("backend"), Some(backend), "{value}");
        }
        let pins = |value: &str, fp: &str| resolve_value(&lib, value, fp).pins;
        let pb0 = |p: &std::collections::BTreeMap<String, String>, pad: &str| {
            p.get(pad)
                .map(String::as_str)
                .and_then(|r| gpio_of_role(r, false))
        };
        assert_eq!(
            pb0(&pins("ATtiny85-20PU", "Package_DIP:DIP-8_W7.62mm"), "5"),
            Some(('B', 0))
        );
        assert_eq!(
            pb0(
                &pins("ATtiny85-MU", "Package_DFN_QFN:QFN-20-1EP_4x4mm_P0.5mm"),
                "11"
            ),
            Some(('B', 0))
        );
        assert_eq!(
            pb0(
                &pins("ATMEGA32U4-AU", "Package_QFP:TQFP-44_10x10mm_P0.8mm"),
                "8"
            ),
            Some(('B', 0))
        );
        // Legal control: a part only one character away is not an ATtiny85.
        let mut q = ComponentQuery::new(None, Some("ATtiny84".to_string()), None);
        q.mpn = q.value.clone();
        assert_ne!(
            lib.resolve(&q).model.map(|m| m.id),
            Some("attiny85".to_string())
        );
    }

    fn named_pin(number: &str, function: &str, net: i64) -> Pin {
        Pin {
            function: function.to_string(),
            ..pin(number, net)
        }
    }

    /// An `X` refdes is a processor only when its value or footprint names
    /// one; an `X` connector stays a connector.
    #[test]
    fn an_x_module_binds_as_a_processor_and_an_x_connector_does_not() {
        let mut module = comp("X2", "ESP32_WROOM32_SKINNY");
        module.pins = vec![
            named_pin("1", "GND", 9),
            named_pin("2", "3V3", 8),
            named_pin("24", "IO2", 1),
        ];
        let mut usb = comp("X1", "USB_C_RECEPTACLE");
        usb.footprint = "Connector_USB:USB_C_Receptacle_GCT_USB4085".to_string();
        usb.pins = vec![pin("A4", 8), pin("A1", 9)];
        let board = ExtractedBoard {
            name: "x-module".to_string(),
            nets: vec![net(1, "LED"), net(8, "+3V3"), net(9, "GND")],
            components: vec![module, usb],
        };
        let bound = bind_board(&board, &ModelLibrary::builtin());
        let refs: Vec<_> = bound
            .mcus
            .iter()
            .map(|m| (m.reference.as_str(), m.backend.as_str()))
            .collect();
        assert_eq!(refs, [("X2", "qemu:esp32")]);
        assert_eq!(
            bound.mcus[0].pad_roles.get("24").map(String::as_str),
            Some("gpio2")
        );
        assert!(bound.unsupported_mcus.is_empty());
    }

    /// A recognised family with no emulator is named in the refusal; a board
    /// with no processor at all keeps the generic message.
    #[test]
    fn an_unemulated_family_is_named_when_nothing_binds() {
        let mut g4 = comp("U3", "STM32G474CEU6");
        g4.footprint = "Package_DFN_QFN:QFN-48-1EP_7x7mm_P0.5mm".to_string();
        g4.pins = vec![pin("1", 8), pin("2", 9)];
        let board = ExtractedBoard {
            name: "g4".to_string(),
            nets: vec![net(8, "+3V3"), net(9, "GND")],
            components: vec![g4],
        };
        let bound = bind_board(&board, &ModelLibrary::builtin());
        assert!(bound.mcus.is_empty());
        assert_eq!(
            bound.unsupported_mcus,
            [("U3".to_string(), "STM32G4".to_string())]
        );
        let msg = no_processor_message(&bound.dnp_mcus, &bound.unsupported_mcus, FitRemedy::Cli);
        assert!(
            msg.contains("STM32G4 (U3) is recognised but has no emulator backend yet"),
            "{msg}"
        );
        let generic = no_processor_message(&[], &[], FitRemedy::Cli);
        assert!(!generic.contains("recognised"), "{generic}");
    }
}
