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

use hauksbee_ir::eprintln;
use std::collections::HashMap;
use std::sync::Arc;

use hauksbee_extract::{is_placeholder_value, Component, ExtractedBoard};
use hauksbee_ir::evidence::{Assumption, ModelSource, ModelUncertainty};
use hauksbee_ir::{
    BDep, BOutput, BjtModel, Circuit, CompiledExpr, Device, DeviceId, DiodeModel, MosLevel,
    MosfetModel, NodeId, Polarity, Rail, SourceKind,
};
use hauksbee_models::rating_basis::Limit;
use hauksbee_models::schema::{CurrentProgram, CurrentProgramSemantics};
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
use crate::mcu_clock::{classify as classify_clock, ClockBoard, ClockKind, McuClock};
use crate::power_supply::{PowerSupply, SupplyLeg};
use crate::program_resistor::{is_ground_net, program_resistance_to_ground};
use crate::stress::DeviceMeta;
use hauksbee_extract::assembly::{AssemblyState, FittedComponent};
use hauksbee_ir::units::{format_amps, format_engineering_web, format_percent, format_volts};

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
    /// The clock the board gives the core, read off its oscillator pins. The
    /// Renode descriptor consumes [`McuClock::external_present`] (a missing or
    /// DNP crystal must not make HSERDY); the execution evidence reports it.
    pub clock: McuClock,
    /// The card's `internal_clock`: the oscillator the part can run from with no
    /// external parts (unset when the card does not say).
    pub internal_clock: Option<String>,
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
    /// The placeholder or guessed card `max_supply_v` came from, when it is
    /// not the part's own: passing it is a stated condition, not a fault.
    pub supply_stand_in: Option<hauksbee_models::rating_basis::StandIn>,
    /// Lowest recommended operating supply voltage (V) across the model's
    /// `supply_range` envelope rows, when it carries any. The co-sim's
    /// execution evidence reads a supply below this as "supply not valid", so
    /// firmware that "ran" on an under-powered core is not counted as a pass.
    pub min_supply_v: Option<f64>,
    /// Recommended operating minimum (V) per supply role, from the card's
    /// per-pin `supply_range` rows. A part with several supply domains (the
    /// RP2040's 3.3 V IOVDD and 1.1 V DVDD) has each pad judged against its
    /// own row rather than one part-wide floor.
    pub supply_floors: HashMap<String, f64>,
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
    /// The board nodes on the part's bus pins (`sda`, `scl`): where firmware's
    /// writes to the DAC enter the board. Ground and unconnected pins are
    /// absent.
    pub bus_nodes: Vec<NodeId>,
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
    /// Which strap levels were assumed rather than read off the board (a
    /// floating or firmware-driven bus-select or address strap), and why.
    /// Surfaced as a partial-model assumption on the bind row.
    pub straps_assumed: Option<String>,
    /// Datasheet current envelope and the exact board rails that power this
    /// peripheral. `None` means the model deliberately makes no electrical
    /// current/power-gating claim; protocol behavior remains usable but cannot
    /// be promoted to bus-coupled electrical behavior in model coverage.
    pub power: Option<BoundPeripheralPower>,
    /// The board nodes on the part's bus pins (I2C `scl`/`sda`; SPI clock,
    /// data and chip select): where firmware's writes enter the board.
    /// Ground and unconnected pins are absent.
    pub bus_nodes: Vec<NodeId>,
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
    /// claim on the large internal test board. To turn a `NodeId` back into a net name, use
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
pub fn bind_board_with(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    custom: &crate::behavioral::CustomRegistry,
) -> BoundBoard {
    bind_board_driven(board, lib, custom, &[], None)
}

/// Bind with the voltages the caller will hold `driven` nets at.
///
/// A spec's `[[net_drive]]` or a searched rail: every part reads its
/// supply-referred levels (a switch's control threshold, a strap's level) at
/// the rail it will actually see, not at a name-guessed default. A driven
/// value wins over a name's; it only informs part binding, never stamps a
/// rail source: the caller drives the net itself.
///
/// `facts` holds what the library decides about the board's parts (see
/// [`BoardFacts`]); `None` works it out here.
#[must_use]
#[allow(clippy::too_many_lines)] // the ordered bind passes share many locals
pub fn bind_board_driven(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    custom: &crate::behavioral::CustomRegistry,
    driven: &[(String, f64)],
    facts: Option<&BoardFacts>,
) -> BoundBoard {
    debug_assert!(facts.is_none_or(|f| f.parts.len() == board.components.len()));
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
    // Rails a fitted module sources on its own pins, per its card.
    let module_supply_nets = module_supply_net_voltages(board, lib, facts);
    // Nets that are a regulator-shaped part's output: a convention-guessed
    // magnitude (bare `VCC` = 5 V) must not be stamped on one.
    let regulator_outputs = regulator_output_nets(board);
    // A separate circuit's own ground is its reference: see
    // `island_reference_grounds`.
    let island_grounds = facts.map_or_else(
        || std::borrow::Cow::Owned(ground_references(board)),
        |f| std::borrow::Cow::Borrowed(&*f.islands),
    );
    for net in &board.nets {
        if net.id == 0 {
            continue;
        }
        if island_grounds.contains(&net.id) && island_grounds.main != Some(net.id) {
            // The board's own ground when it has no canonical one (`main`) has
            // no other ground to be separate from, so it is not disclosed.
            let main_name = island_grounds
                .main
                .and_then(|id| board.net(id))
                .map_or("GND", |n| n.name.as_str());
            let mut row = BindRow::without_model(
                format!("RAIL:{}", net.name),
                "0.00V".to_string(),
                Confidence::Exact,
                BindOutcome::Skipped {
                    reason: format!("ground of a circuit no part joins to {main_name}"),
                },
            );
            row.warning = Some(format!(
                "'{}' shares no part with {main_name}: it is the ground of a separate circuit on \
                 this board (a second half, a separate sub-board), so it is simulated as that \
                 circuit's 0 V reference, the same node as {main_name}",
                net.name
            ));
            report.push(row);
        }
        let node = if is_canonical_ground(&net.name) || island_grounds.contains(&net.id) {
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
            if let Some(source) = regulator_outputs
                .get(&net.id)
                .filter(|_| name_states_no_magnitude(&net.name))
            {
                // The name says "a supply" and nothing about how many volts;
                // the convention (VCC = 5 V) is a guess, and the part driving
                // the net is the better witness: a 3.3 V LDO's output is not 5 V.
                // Leave the rail without a magnitude rather than invent one.
                let mut row = BindRow::without_model(
                    format!("RAIL:{}", net.name),
                    String::new(),
                    Confidence::Exact,
                    BindOutcome::Skipped {
                        reason: "rail voltage not stated by its name".to_string(),
                    },
                );
                row.warning = Some(format!(
                    "'{}' states no voltage and is the output of {source}; its voltage is not \
                     guessed from the name (a bare VCC is only assumed 5 V when no regulator \
                     drives it). Name the net with its voltage (+3V3) or supply it with a \
                     [[supply]]",
                    net.name
                ));
                report.push(row);
            } else if let Some(v) = power_rail_voltage(&net.name)
                .or_else(|| power_out_nets.get(&net.id).copied())
                .or_else(|| module_supply_nets.get(&net.id).copied())
            {
                power_nets.insert(net.name.clone(), v);
            }
        }
    }

    // The levels parts bind against: the named rails, then the driven nets.
    let mut level_nets = power_nets.clone();
    level_nets.extend(driven.iter().cloned());

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
    hauksbee_ir::progress::bind_derived(board.components.len());
    for (index, comp) in board.components.iter().enumerate() {
        hauksbee_ir::progress::advance_in(hauksbee_ir::progress::Stage::Bind, 1);
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
        let found = part_facts(facts, index, lib, part);
        let res = &found.resolution;
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
                // A pin map read off another package's column binds cleanly
                // and simulates a different circuit: refuse it before binding.
                let misfit = {
                    let circuit = &circuit;
                    let net = |n: Option<i64>| {
                        node_of(n)
                            .map(|id| (id == NodeId::GROUND, circuit.node_name(id).to_string()))
                    };
                    crate::package_fit::misfit(comp, m, &net)
                };
                if let Some(why) = misfit {
                    let reason = format!("package mismatch: {why}; the part stays unmodelled");
                    let warning = format!(
                        "cannot bind '{}' ({}): {reason}",
                        comp.reference, comp.value
                    );
                    (
                        Some(kind_str),
                        BindOutcome::Unresolved { reason },
                        Some(warning),
                        Vec::new(),
                    )
                } else {
                    // A card that reached binding without validation (a built-in, an
                    // older saved draft) may carry a unit slip (`cout_min_f = 1.0`);
                    // bind it with those values dropped and say so, never demand them.
                    let suspect = found.suspect();
                    let bound_card = suspect.as_ref().map_or(m, |q| &q.model);
                    let suspect_note = suspect.as_ref().map(|q| {
                        format!(
                            "{}{} ({}): {}",
                            Assumption::PARTIAL_MODEL_MARKER,
                            comp.reference,
                            comp.value,
                            q.summary()
                        )
                    });
                    let (outcome, warning, guesses) = bind_component(
                        comp,
                        bound_card,
                        source.as_ref(),
                        &mut circuit,
                        &node_of,
                        &mut digital,
                        &mut mcus,
                        &mut dacs,
                        &mut peripherals,
                        &mut report.regulators,
                        (&board.components, &level_nets),
                        lib.pin_rules(),
                    );
                    let rail = |n: Option<i64>| {
                        let id = node_of(n)?;
                        let name = circuit.node_name(id);
                        if id == NodeId::GROUND {
                            Some("GND".to_string())
                        } else {
                            power_nets.contains_key(name).then(|| name.to_string())
                        }
                    };
                    let unknown = crate::package_fit::unknown_pads(comp, m, &rail);
                    (
                        Some(kind_str),
                        outcome,
                        merge_bind_warnings(merge_bind_warnings(suspect_note, warning), unknown),
                        guesses,
                    )
                }
            }
        };

        let warning = with_tie_warning(comp, res, warning);
        if let Some(k) = &kind_str {
            component_kinds.insert(comp.reference.clone(), k.clone());
        }
        // A port's numeric part number parses as a value, so it lands here too;
        // nothing is plugged into it in the specified scenario, so it drives nothing.
        let is_port = is_port_class(&ref_prefix(&comp.reference));
        if matches!(outcome, BindOutcome::Unresolved { .. }) && !is_port {
            let pin = |p: &hauksbee_extract::Pin| {
                Some((node_of(p.net)?, crate::bind_report::pin_can_drive(&p.kind)))
            };
            report.open_parts.push(crate::bind_report::OpenPart {
                reference: comp.reference.clone(),
                value: comp.value.clone(),
                pins: comp.pins.iter().filter_map(pin).collect(),
            });
        }
        if let Some(selection) = res.selection.clone() {
            report.selections.insert(report.rows.len(), selection);
        }
        if hauksbee_extract::is_active_ic(comp)
            && !hauksbee_models::matcher::is_active_ic_ref(&comp.reference)
        {
            report.identity_ics.insert(comp.reference.clone());
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

    // Board truth for clock-ready emulation and the execution evidence. The MCU
    // pin roles/names give the oscillator pins; the board gives population and
    // connectivity. No reference-name or board-name special case is involved.
    let is_rail = |n: NodeId| n.is_ground() || power_nets.contains_key(circuit.node_name(n));
    let net_name = |n: NodeId| circuit.node_name(n).to_string();
    let fitted = |c: &Component| AssemblyState::of(c).is_present();
    let is_crystal = |c: &Component| fitted_crystal(c, lib);
    let is_resistor = |c: &Component| c.pins.len() == 2 && passive_letter(c) == Some('R');
    let clock_board = ClockBoard {
        board,
        node_of: &node_of,
        is_rail: &is_rail,
        net_name: &net_name,
        fitted: &fitted,
        is_crystal: &is_crystal,
        is_resistor: &is_resistor,
    };
    for mcu in &mut mcus {
        let Some(comp) = board
            .components
            .iter()
            .find(|c| c.reference == mcu.reference)
        else {
            continue;
        };
        mcu.clock = classify_clock(
            &clock_board,
            comp,
            &mcu.pad_roles,
            mcu.internal_clock.as_deref(),
        );
        if mcu.clock.kind == ClockKind::Broken {
            let note = format!("clock source broken: {}", mcu.clock.detail);
            if let Some(row) = report
                .rows
                .iter_mut()
                .find(|r| r.reference == mcu.reference)
            {
                row.warning = Some(match row.warning.take() {
                    Some(w) => format!("{w}; {note}"),
                    None => note,
                });
            }
        }
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
    let behavioral = bind_behavioral(board, lib, facts, &mut circuit, &node_of, custom);

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
    // A discrete high-side FET between two supply nets of one nominal voltage
    // switches one rail onto the other; nets of unlike voltage (a power-path
    // FET, a level shifter) are not branches of each other.
    let rail_volts = |n: NodeId| {
        board_nodes
            .contains(&n)
            .then(|| power_nets.get(circuit.node_name(n)).copied())
            .flatten()
    };
    domains.join_pass_fets(&circuit, |d, s| {
        matches!((rail_volts(d), rail_volts(s)), (Some(a), Some(b))
            if (a - b).abs() <= 0.01 * a.abs().max(b.abs()))
    });
    report.supply_groups =
        supply_groups(board, &circuit, &domains, &report.regulators, &behavioral);
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
    crate::rail_owner::mark_unfitted_alternates(
        &mut conflicts,
        &circuit,
        &report.regulators,
        |net| power_nets.get(net).copied(),
    );
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
                    unfitted_alternate: None,
                    message: format!(
                        "'{name}' ({} by name) and '{}' ({}) are one rail \
                         through a series conductor; only '{}' takes a supply, so '{name}' \
                         reads {}",
                        format_volts(volts),
                        leg.net_name,
                        format_volts(held),
                        leg.net_name,
                        format_volts(held)
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
            format_volts(volts),
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
                reason: if c.unfitted_alternate.is_some() {
                    "rail drawn with an alternate regulator footprint".to_string()
                } else {
                    "rail driven by more than one source".to_string()
                },
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

    report.linear_heat = charger_heat(&circuit);
    report.port_heat = port_heat(&circuit);

    // ── Pass 4: gather fault-monitor metadata ───────────────────────────────
    // Match each monitorable IR device back to its component (device name ==
    // reference for the parts we stamp) and the component's resolved ratings +
    // footprint, so the stress monitor can evaluate it. Supplies/regulators are
    // matched by their Vsource device id directly.
    let device_meta = gather_device_meta(
        board, lib, facts, &circuit, &node_of, &mcus, &digital, &dacs,
    );

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
    facts: Option<&BoardFacts>,
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
    for (index, comp) in board.components.iter().enumerate() {
        // Not assembled, or identity refused -> no behavioural device (same
        // three-state rule as pass 2). Custom factories may match raw value
        // strings even when normal model resolution refuses the component, so
        // they sit behind the same witness as declarative binding.
        let Some(part) = AssemblyState::of(comp).fitted() else {
            continue;
        };
        let found = part_facts(facts, index, lib, part);
        let model = found.resolution.model.as_ref();

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
        let model_pins = model.map_or(&empty_pins, |m| &m.pins);
        let role_nets = role_node_map_pins(comp, model_pins, model.map(|m| m.kind), node_of);
        let role_map: std::collections::BTreeMap<String, NodeId> = role_nets
            .into_iter()
            .filter(|(_role, node)| !circuit.node_name(*node).starts_with("unconnected-"))
            .collect();

        // 1. A registered custom behaviour wins (the escape hatch). It can bind
        //    even for a part with no declarative block / no DB model at all.
        if !custom.is_empty() {
            if let Some(boxed) = custom.build_for(&keys) {
                let params = model.map(|m| m.params.clone()).unwrap_or_default();
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
        let Some(model) = model.filter(|m| !m.behavioral.is_empty()) else {
            continue;
        };
        let mut model = model.clone();
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
    facts: Option<&BoardFacts>,
    circuit: &Circuit,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
    mcus: &[McuBinding],
    digital: &[crate::digital::DigitalComponent],
    dacs: &[DacBinding],
) -> Vec<DeviceMeta> {
    // Index components by reference, with their resolved kind/ratings/footprint.
    struct CompInfo {
        kind: ComponentKind,
        rated: std::sync::Arc<Rated>,
        footprint: String,
        /// Role nodes, for a card rating current or power per path.
        roles: HashMap<String, NodeId>,
    }
    let mut by_ref: HashMap<String, CompInfo> = HashMap::new();
    for (index, comp) in board.components.iter().enumerate() {
        // Only a present part's ratings may describe a monitored device: a DNP
        // or identity-refused record sharing the reference designator must not
        // supply the ratings for a device another record stamped.
        let Some(part) = AssemblyState::of(comp).fitted() else {
            continue;
        };
        let found = part_facts(facts, index, lib, part);
        if let (Some(m), Some(rated)) = (&found.resolution.model, found.rated(comp)) {
            let roles = role_node_map(comp, m, node_of);
            by_ref.insert(
                comp.reference.clone(),
                CompInfo {
                    kind: m.kind,
                    rated,
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
                    ratings: info.rated.ratings.clone(),
                    derating: info.rated.derating.clone(),
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
                            ratings: info.rated.ratings.clone(),
                            derating: info.rated.derating.clone(),
                        });
                    }
                }
            }
        }
    }

    // A card rating current or power per path: which of the part's devices
    // carry each path, so the monitor judges the path on their current.
    // Every meta of one part shares its units and so its paths: group once
    // and solve each part's paths once (per meta, both were quadratic).
    let parts: Vec<String> = (metas.iter())
        .map(|m| crate::stress::strip_unit_suffix(&m.reference).to_owned())
        .collect();
    let mut units_of: HashMap<&str, Vec<DeviceId>> = HashMap::new();
    for (r, m) in parts.iter().zip(&metas) {
        units_of.entry(r).or_default().push(m.device);
    }
    let index = crate::stress::DevicesByNode::new(circuit);
    let mut paths_of: HashMap<&str, Vec<_>> = HashMap::new();
    for (r, meta) in parts.iter().zip(metas.iter_mut()) {
        if let Some(c) = by_ref.get(r) {
            let paths = paths_of.entry(r).or_insert_with(|| {
                let units = &units_of[r.as_str()];
                crate::stress::rated_paths_indexed(
                    circuit,
                    &index,
                    r,
                    &c.rated.derating,
                    &c.roles,
                    units,
                )
            });
            meta.derating.carriers.clone_from(paths);
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
            if info.rated.ratings.max_pin_current_a.is_some() {
                metas.push(DeviceMeta {
                    reference: format!("{reference}:{pin}"),
                    device: vsource,
                    kind: info.kind,
                    footprint: info.footprint.clone(),
                    ratings: info.rated.ratings.clone(),
                    derating: info.rated.derating.clone(),
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
///
/// The model's ratings carry their [basis](hauksbee_models::rating_basis):
/// whether they are the part's own card's, or a placeholder or guessed card's
/// stand-in that no check may fail the board on.
#[must_use]
pub fn resolve(lib: &ModelLibrary, part: FittedComponent<'_>) -> hauksbee_models::Resolution {
    let mut res = library_resolution(lib, part.component());
    let basis = hauksbee_models::rating_basis::RatingBasis::of(&res);
    if let Some(m) = res.model.as_mut() {
        m.ratings.basis = basis;
    }
    res
}

/// What the library alone decides about one fitted part, shared by every board
/// that has that part exactly.
pub struct PartFacts {
    pub resolution: hauksbee_models::Resolution,
    rated: std::sync::OnceLock<Arc<Rated>>,
    /// The card with its out-of-range values distrusted, worked out on first ask.
    suspect: std::sync::OnceLock<Option<Arc<hauksbee_models::ranges::Quarantined>>>,
}

/// A part's ratings and derating, as the stress monitor holds them.
pub struct Rated {
    ratings: hauksbee_models::schema::Ratings,
    derating: hauksbee_models::derating::Derating,
}

impl PartFacts {
    const fn new(resolution: hauksbee_models::Resolution) -> Self {
        Self {
            resolution,
            rated: std::sync::OnceLock::new(),
            suspect: std::sync::OnceLock::new(),
        }
    }

    /// The resolved card read with its out-of-range values distrusted
    /// ([`hauksbee_models::ranges::quarantine`]), or `None` when it has none.
    fn suspect(&self) -> Option<Arc<hauksbee_models::ranges::Quarantined>> {
        let m = self.resolution.model.as_ref()?;
        self.suspect
            .get_or_init(|| hauksbee_models::ranges::quarantine(m).map(Arc::new))
            .clone()
    }

    /// The ratings the model gives the part `comp`, worked out on first ask.
    fn rated(&self, comp: &Component) -> Option<Arc<Rated>> {
        let m = self.resolution.model.as_ref()?;
        let rated = self.rated.get_or_init(|| {
            let mounting = (comp.properties.iter().rev())
                .find(|(k, _)| k == hauksbee_models::derating::MOUNTING_PROPERTY)
                .map(|(_, v)| v.as_str());
            let derating = m
                .derating
                .fitted(&comp.reference, &comp.footprint, mounting);
            // A fuse or polyfuse limits by its hold current: the card's, else the
            // one its value states ("6V 0.5A"), so a load past it is a trip.
            // A rating the value states is the part's own, so it stands in
            // place of a stand-in card's and is judged ([`Limit`]).
            let mut ratings = m.ratings.clone();
            let stand_in = ratings.basis.stand_in.is_some();
            if m.passive_class == Some(hauksbee_models::schema::PassiveClass::Fuse) {
                ratings.trips_over_current = true;
                let stated = hauksbee_models::value::parse_current_rating_a(&comp.value);
                if stated.is_some() && (stand_in || ratings.max_current_a.is_none()) {
                    ratings.max_current_a = stated;
                    ratings.basis.stated_on_board(Limit::Current);
                }
            }
            // A capacitor's value often states its rated voltage ("10uF/16V");
            // the card's rating, when it has one of its own, wins.
            let capacitor = m.passive_class
                == Some(hauksbee_models::schema::PassiveClass::Capacitor)
                || (m.kind == ComponentKind::Passive && passive_letter(comp) == Some('C'));
            if capacitor && (stand_in || ratings.max_voltage_v.is_none()) {
                let stated = capacitor_stated_voltage(comp);
                if stated.is_some() {
                    ratings.max_voltage_v = stated;
                    ratings.basis.stated_on_board(Limit::Voltage);
                }
            }
            // A capacitor that nothing states a voltage for is not unlimited:
            // a 10 uF MLCC is sold from 6.3 V up, and a rail above the lowest
            // class destroys the 6.3 V part. Hold it to that floor as an
            // ASSUMED limit, so a rail past it reads "rating unstated, give
            // it one" instead of a pass, while every other limit of its card
            // stays judged.
            if capacitor && ratings.max_voltage_v.is_none() {
                ratings.max_voltage_v = Some(UNRATED_CAPACITOR_VOLTAGE_FLOOR_V);
                ratings.basis.assume(
                    Limit::Voltage,
                    hauksbee_models::rating_basis::StandIn {
                        card: m.id.clone(),
                        kind: hauksbee_models::rating_basis::StandInKind::Unstated,
                    },
                );
            }
            Arc::new(Rated { ratings, derating })
        });
        Some(Arc::clone(rated))
    }
}

/// The assumed rated voltage (V) of a capacitor whose record states none.
///
/// The lowest voltage capacitors are commonly sold with (6.3 V MLCC,
/// electrolytic and tantalum classes): a floor, not a rating.
pub const UNRATED_CAPACITOR_VOLTAGE_FLOOR_V: f64 = 6.3;

/// Field names that state a capacitor's rated voltage, compared trimmed,
/// case-insensitively and with `_` or `-` read as a space. A field merely
/// mentioning a voltage ("Test Voltage", "Voltage Coefficient", "Ripple
/// Voltage") states no rating.
const RATED_VOLTAGE_FIELDS: [&str; 8] = [
    "voltage",
    "rated voltage",
    "voltage rating",
    "rating",
    "wv",
    "vdc",
    "working voltage",
    "max voltage",
];

/// The rated voltage a capacitor's own record states: its value ("10uF/16V",
/// "100nF 25V X7R"), else a rated-voltage field ([`RATED_VOLTAGE_FIELDS`])
/// whose text is a voltage, or a bare number of volts on a field whose name
/// is itself a voltage (a bare "Rating" of "0.25" is no voltage).
fn capacitor_stated_voltage(comp: &Component) -> Option<f64> {
    hauksbee_models::value::parse_voltage_rating_v(&comp.value).or_else(|| {
        comp.properties.iter().find_map(|(k, v)| {
            let key = k.trim().to_ascii_lowercase().replace(['_', '-'], " ");
            if !RATED_VOLTAGE_FIELDS.contains(&key.as_str()) {
                return None;
            }
            hauksbee_models::value::parse_voltage_rating_v(v).or_else(|| {
                (key != "rating")
                    .then(|| v.trim().parse::<f64>().ok())
                    .flatten()
                    .filter(|x| x.is_finite() && *x > 0.0)
            })
        })
    })
}

/// What a board and the library alone decide, for the binder to take.
///
/// Each fitted part's [`PartFacts`] (by component index) and the board's
/// separate-circuit grounds. An ensemble's boards differ in a few sampled
/// values, so each is [derived](Self::derived) from a base's facts, holding
/// pointers to what it has alike, not copies.
pub struct BoardFacts {
    parts: Vec<Option<Arc<PartFacts>>>,
    islands: Arc<GroundReferences>,
}

impl BoardFacts {
    /// The facts of `board`, sharing with `base` (a board and its facts) what
    /// the two have alike: a part with the same identity and pins, and the
    /// ground islands when every pin is on the same net.
    #[must_use]
    pub fn derived(
        board: &ExtractedBoard,
        lib: &ModelLibrary,
        base: Option<(&ExtractedBoard, &Self)>,
    ) -> Self {
        let shared = |i: usize, comp: &Component| {
            let (b, facts) = base?;
            same_part(b.components.get(i)?, comp).then(|| facts.parts[i].clone())?
        };
        let part = |(i, comp): (usize, &Component)| {
            let fitted = AssemblyState::of(comp).fitted()?;
            shared(i, comp).or_else(|| Some(Arc::new(PartFacts::new(resolve(lib, fitted)))))
        };
        let islands = match base {
            Some((b, facts)) if same_topology(b, board) => Arc::clone(&facts.islands),
            _ => Arc::new(ground_references(board)),
        };
        Self {
            parts: board.components.iter().enumerate().map(part).collect(),
            islands,
        }
    }

    /// `(component index, model)` for each fitted part a model answers.
    pub fn models(&self) -> impl Iterator<Item = (usize, &ModelEntry)> {
        let each = self.parts.iter().enumerate();
        each.filter_map(|(i, p)| Some((i, p.as_ref()?.resolution.model.as_ref()?)))
    }
}

/// `part`'s facts: `facts`' for component `index`, else worked out now.
fn part_facts(
    facts: Option<&BoardFacts>,
    index: usize,
    lib: &ModelLibrary,
    part: FittedComponent<'_>,
) -> Arc<PartFacts> {
    let held = facts.and_then(|f| f.parts[index].clone());
    held.unwrap_or_else(|| Arc::new(PartFacts::new(resolve(lib, part))))
}

/// Whether two records carry the same pins, each on the same net.
fn same_pins(a: &Component, b: &Component) -> bool {
    let alike = |(p, q): (&hauksbee_extract::Pin, &hauksbee_extract::Pin)| {
        (&p.number, p.net, &p.function, &p.kind) == (&q.number, q.net, &q.function, &q.kind)
    };
    a.pins.len() == b.pins.len() && a.pins.iter().zip(&b.pins).all(alike)
}

/// Whether two records say the same about a part: everything its resolution
/// and ratings read (a position or a layer is not read).
fn same_part(a: &Component, b: &Component) -> bool {
    a.reference == b.reference
        && (&a.value, &a.lib_id, &a.footprint) == (&b.value, &b.lib_id, &b.footprint)
        && (&a.properties, a.dnp) == (&b.properties, b.dnp)
        && same_pins(a, b)
}

/// Whether two boards put every pin on the same net.
fn same_topology(a: &ExtractedBoard, b: &ExtractedBoard) -> bool {
    a.components.len() == b.components.len()
        && a.components
            .iter()
            .zip(&b.components)
            .all(|(p, q)| same_pins(p, q))
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
    // The answer reads the part's identity (value, footprint, library id, fields,
    // pad numbers and names) and the class of its designator; not where it sits,
    // which nets its pads are on, or the digits of its designator. So a board of
    // thousands of parts that differ only by those asks the question once.
    let mut res = lib.memoised_part(&part_identity_key(comp), || {
        library_resolution_uncached(lib, comp)
    });
    res.query.reference = Some(comp.reference.clone());
    if let Some(selection) = res.selection.as_mut() {
        selection.reference.clone_from(&comp.reference);
    }
    res
}

/// Everything [`library_resolution_uncached`] reads about a part, as one string.
fn part_identity_key(comp: &Component) -> String {
    use std::fmt::Write;
    let reference = comp.reference.as_str();
    let letters = reference.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    let class = if letters.len() < reference.len() && letters.bytes().all(|b| b.is_ascii_digit()) {
        // A letters-then-digits designator ("R1017"): only its class is read.
        format!("{}~", ref_prefix(reference))
    } else {
        format!("={reference}")
    };
    let active = hauksbee_extract::is_active_ic(comp);
    let mut key = format!(
        "{class}\x1f{}\x1f{}\x1f{}\x1f{}\x1f{}\x1d",
        u8::from(active),
        comp.value,
        comp.lib_id,
        comp.footprint,
        u8::from(comp.dnp)
    );
    for (k, v) in &comp.properties {
        let _ = write!(key, "{k}\x1e{v}\x1f");
    }
    key.push('\x1d');
    for p in &comp.pins {
        let _ = write!(key, "{}\x1e{}\x1e{}\x1f", p.number, p.function, p.kind);
    }
    key
}

fn library_resolution_uncached(
    lib: &ModelLibrary,
    comp: &Component,
) -> hauksbee_models::Resolution {
    let mut res = record_resolution(lib, comp);
    // A part-number FIELD on the design (`MPN`, `Manufacturer_No`, an Eagle
    // `PART_NUMBER` attribute) reaches only the few cards written with an
    // `mpn_re`; nearly every card matches on `value_re`. So where the value
    // decided nothing (unresolved, or only a guess), the field is tried the way
    // a BOM's part number is: standing in for a value that is not a magnitude.
    // The value keeps precedence wherever it reached a card itself. After the
    // field, the library part the schematic placed is tried on the same terms:
    // the part number an Eagle device name embeds (`MOSFET_PCH` / `-DMG2305UX-7`
    // under a `20V/4.2A` value) and the KiCad symbol name
    // (`Regulator_Linear:AP2112K-3.3` under an `AP2112K` value), but only where
    // the value cannot contradict them; see [`library_part_names`].
    let mpn = mpn_properties(comp)
        .find(|m| !m.eq_ignore_ascii_case(comp.value.trim()))
        .map(|m| probe_with_mpn(comp, m));
    let library = library_part_names(comp).into_iter().map(|name| {
        let mut probe = comp.clone();
        probe.value = name;
        probe
    });
    for probe in mpn.into_iter().chain(library) {
        if !matches!(res.confidence, Confidence::Unresolved | Confidence::Guessed) {
            break;
        }
        let after = record_resolution(lib, &probe);
        if identity_improves(&res, &after) {
            res = after;
        }
    }
    res
}

/// Part names the schematic library states for `comp`, as stand-ins for a
/// value that resolved nothing, most specific first.
///
/// The sources are the part number the Eagle library device embeds (the
/// [`hauksbee_extract::bom::LIBRARY_DEVICE_PROPERTY`] the board reader adopts,
/// `AP2112K-3.3V` from `V_REG_AP2112` + `K-3.3V`) and the KiCad symbol name
/// (`Regulator_Linear:AP2112K-3.3`). Neither is the value, and a symbol is
/// often borrowed for a different part (an `AP2112K-3.3` symbol carrying an
/// `RT9080` value), so a name is admitted only when the value cannot
/// contradict it:
///
/// - the value is empty, or
/// - the value, letters and digits only, is part-number shaped and contained in
///   the name (`AP2112K` in `AP2112K-3.3`: the value dropped a suffix), or
/// - the value is a rating string (`3.3V/600mA`, `20V/4.2A/52mΩ`), which names
///   no part under any designator, or
/// - the designator is an active IC and the value is only a voltage (`3.3V`,
///   `3V3`: a regulator's output rating; on a `D` it is a zener's voltage).
///
/// A layout-only record's `lib_id` is its footprint, which names a package, not
/// a part, so it is never offered. A name still prefixed with a library class
/// (`V_REG_AP2112K-3.3V`) is offered whole and then from each `_` on.
fn library_part_names(comp: &Component) -> Vec<String> {
    let alnum = |s: &str| -> String {
        s.chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|c| c.to_ascii_uppercase())
            .collect()
    };
    let part_shaped = |s: &str| {
        s.chars().any(|c| c.is_ascii_digit()) && s.chars().any(|c| c.is_ascii_alphabetic())
    };
    let symbol = Some(symbol_name(&comp.lib_id))
        .filter(|_| comp.lib_id.contains(':') && comp.lib_id != comp.footprint)
        .filter(|name| *name != symbol_name(&comp.footprint));
    let device = (comp.properties.iter())
        .filter(|(k, _)| k == hauksbee_extract::bom::LIBRARY_DEVICE_PROPERTY)
        .map(|(_, v)| v.as_str());
    let value = comp.value.trim();
    let rating = is_rating_string(value)
        || (is_voltage_rating(value)
            && hauksbee_models::matcher::is_active_ic_ref(&comp.reference));
    let admissible = |name: &str| {
        let (name, value) = (alnum(name), alnum(value));
        value.is_empty()
            || (part_shaped(&value) && value.len() >= 4 && name.contains(&value))
            || rating
    };
    let mut out: Vec<String> = Vec::new();
    for name in device.chain(symbol).map(str::trim) {
        if !part_shaped(name) || !admissible(name) || alnum(name) == alnum(value) {
            continue;
        }
        let tails = name.match_indices('_').map(|(i, _)| &name[i + 1..]);
        for candidate in std::iter::once(name).chain(tails) {
            if part_shaped(candidate) && !out.iter().any(|o| o == candidate) {
                out.push(candidate.to_string());
            }
        }
    }
    out
}

/// Is `value` only a voltage (`3.3V`, `3V3`, `5V`, `1.8`), the way a fixed
/// regulator's value often states its output instead of its part number?
fn is_voltage_rating(value: &str) -> bool {
    let v = value.trim().to_ascii_uppercase();
    let v = v.strip_suffix('V').unwrap_or(&v);
    let mut parts = v.splitn(2, ['.', 'V']);
    let whole = parts.next().unwrap_or_default();
    let frac = parts.next();
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    digits(whole) && frac.is_none_or(digits)
}

/// Is `value` a `/`-separated list of ratings with units (`3.3V/600mA`,
/// `20V/4.2A/52mΩ/1.4W`), the way some libraries describe a part instead of
/// naming it? Every field must be a number followed by an optional SI prefix
/// and a unit of voltage, current, power or resistance.
fn is_rating_string(value: &str) -> bool {
    let rated = |field: &str| {
        let field = field.trim();
        let unit = field.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
        let number = &field[..field.len() - unit.len()];
        let unit = unit.trim_start_matches(['p', 'n', 'u', 'µ', 'm', 'k', 'K', 'M']);
        number.chars().any(|c| c.is_ascii_digit())
            && matches!(unit, "V" | "v" | "A" | "W" | "Ω" | "R" | "ohm" | "Ohm")
    };
    value.contains('/') && value.split('/').all(rated)
}

/// [`library_resolution`] from the record's own value, footprint and fields,
/// without trying a part-number field in place of the value.
fn record_resolution(lib: &ModelLibrary, comp: &Component) -> hauksbee_models::Resolution {
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
    q.mpn = component_mpn(comp);
    let mut res = lib.resolve(&q);
    // A MOSFET named only by class binds that class's assumed-corner set, ahead
    // of any package catch-all (which cannot tell N from P), but never ahead of
    // a card that names the part.
    let named = res
        .provenance
        .as_ref()
        .is_some_and(|p| p.tier() != hauksbee_ir::evidence::ModelSourceTier::EstimatedFallback);
    if let Some(class) = generic_mosfet_class(comp).filter(|_| !named) {
        let mut by_class = q.clone();
        (by_class.value, by_class.mpn) = (Some(class.to_string()), None);
        let mut classed = lib.resolve(&by_class);
        if classed.model.is_some() {
            (classed.confidence, classed.query) = (Confidence::Guessed, q.clone());
            res = classed;
        }
    }
    // A KiCad generic transistor value ("Q_NPN_BCE") spells its symbol's pad
    // order; for a class corner set, so does the symbol name itself
    // (`Device:Q_PMOS_GSD`), and it wins: the netlist's pin numbers are the
    // symbol's, whatever the value text says. A part's own card keeps its
    // datasheet pinout.
    let corners = res.selection.as_ref().is_some_and(|s| s.corners);
    if let Some(model) = res.model.as_mut() {
        let symbol = Some(symbol_name(&comp.lib_id)).filter(|_| corners);
        let mut spelled = symbol.into_iter().chain([comp.value.as_str()]);
        if let Some(pins) = spelled.find_map(|name| generic_symbol_pins(name, model.kind)) {
            model.pins = pins;
        }
    }
    // The generic LED corner set answers the value "LED" alone; only a plain
    // two-pad LED takes it (a WS2812B/RGB part or a U-designated "LED" is not it).
    let led = plain_led(comp);
    if res.model.as_ref().is_some_and(is_generic_led) && !led {
        res = hauksbee_models::Resolution::unresolved(q.clone());
    }
    // A card names the part unless it is the generic LED set itself, or only a
    // connector/mechanical class: a panel-mount LED on a header footprint is
    // still an LED.
    let named = res.model.as_ref().is_some_and(|m| {
        !is_generic_led(m) && !matches!(m.kind, ComponentKind::Ignore | ComponentKind::Connector)
    }) || res.selection.as_ref().is_some_and(|s| !s.corners);
    // A colour-only value ("RED") is no part number: it takes that colour's LED card.
    if let Some(card) = led_colour(comp).filter(|_| !named) {
        let mut by_colour = q.clone();
        (by_colour.value, by_colour.mpn) = (Some(format!("{card} LED")), None);
        let mut coloured = lib.resolve(&by_colour);
        if let Some(model) = coloured.model.as_mut() {
            let note = format!(
                "value '{}' names only a colour: assumed a typical {card} LED, forward voltage included ({})",
                comp.value, model.id
            );
            model.params.set_str("warning", note);
            (coloured.confidence, coloured.query) = (Confidence::Guessed, q);
            return coloured;
        }
    }
    // Otherwise an LED the record names by nothing but being an LED binds the
    // ASSUMED forward-voltage corner set (db/diodes.toml): its colour, and so
    // most of its circuit's current, is unknown.
    if led && !named {
        let mut generic = q.clone();
        (generic.value, generic.mpn) = (Some("LED".to_string()), None);
        let mut set = lib.resolve(&generic);
        if set.model.is_some() {
            (set.confidence, set.query) = (Confidence::Guessed, q);
            return set;
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
/// (a `D*`/`LED*` designator with an LED symbol or footprint, or any designator
/// [`plain_led`] accepts: a `TXLED` or `PWR` indicator is still an LED), and the card
/// word it binds to: orange and amber read as yellow, white as blue.
fn led_colour(comp: &Component) -> Option<&'static str> {
    let led = [&comp.lib_id, &comp.footprint]
        .iter()
        .any(|id| id.to_ascii_uppercase().contains("LED"));
    let pads = logical_passive_pins(comp).map_or(0, |pins| pins.len());
    let designated = matches!(ref_prefix(&comp.reference).as_str(), "D" | "LED") && led;
    if !(designated || plain_led(comp)) || pads > 2 {
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

/// Whether `model` is a member of the generic LED corner set
/// (`variant_of = "led_generic"` in db/diodes.toml).
fn is_generic_led(model: &ModelEntry) -> bool {
    model.variant_of.as_deref() == Some("led_generic")
}

/// A plain two-terminal LED by the record's own identity: two logical pads
/// and [`hauksbee_extract::is_led_identity`] (the one definition of what the
/// record calls an LED, shared with the checks that trace indicators).
///
/// Never an IC designator (`U`/`IC`), and never an infrared part, whose
/// forward voltage lies below the visible colours the generic corner set
/// spans. The designator is otherwise free: indicator LEDs are often named for
/// their job (`STAT`, `CHG`, `PWR`, `TXLED`). A header footprint does not veto
/// it: a panel-mount LED is wired through one.
fn plain_led(comp: &Component) -> bool {
    let pads = logical_passive_pins(comp).map_or(0, |pins| pins.len());
    if pads != 2 || hauksbee_models::matcher::is_active_ic_ref(&comp.reference) {
        return false;
    }
    let infrared = [&comp.value, &comp.lib_id, &comp.footprint]
        .iter()
        .any(|s| {
            s.to_ascii_uppercase()
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|t| matches!(t, "IR" | "INFRARED"))
        });
    !infrared && hauksbee_extract::is_led_identity(comp)
}

/// The board's power tree by net name: the fed groups `domains` holds, the
/// parts passing power within each, and the regulators and converters feeding
/// each from its input (see [`crate::rail_owner::RailDomains::supply_groups`]).
fn supply_groups(
    board: &ExtractedBoard,
    circuit: &Circuit,
    domains: &crate::rail_owner::RailDomains,
    regulators: &[SimpleRegulator],
    behavioral: &[crate::behavioral::BehavioralDevice],
) -> Vec<crate::rail_owner::SupplyGroup> {
    use crate::rail_owner::Regulation;
    let refs: std::collections::HashSet<&str> = board
        .components
        .iter()
        .map(|c| c.reference.as_str())
        .collect();
    // A stamp is named for its part: the reference itself (`L1`), the
    // reference and a suffix (`U5_s1`) or a prefix (`Vreg_U3`, `Rbeh_F1_fuse`).
    // A reference may hold `_` itself, so the longest run of `_`-separated
    // words that is a reference on this board names the owner.
    let owner = |name: &str| {
        let words: Vec<&str> = name.split('_').collect();
        let runs = (0..words.len()).flat_map(|i| (i + 1..=words.len()).map(move |j| (i, j)));
        runs.map(|(i, j)| words[i..j].join("_"))
            .filter(|run| refs.contains(run.as_str()))
            .max_by_key(String::len)
    };
    let nets: std::collections::HashSet<&str> = (board.nets.iter())
        .filter(|net| net.id != 0)
        .map(|net| net.name.as_str())
        .collect();
    let board_net = |n: NodeId| {
        let name = circuit.node_name(n);
        nets.contains(name).then(|| name.to_string())
    };
    let mut regulations: Vec<Regulation<'_>> = regulators
        .iter()
        .filter_map(|r| {
            let Some(Device::Vsource { p, .. }) = circuit.devices.get(r.source.0 as usize) else {
                return None;
            };
            Some(Regulation {
                part: &r.reference,
                output: *p,
                input: r.input,
            })
        })
        .collect();
    regulations.extend(behavioral.iter().filter_map(|b| {
        Some(Regulation {
            part: &b.reference,
            output: b.converter_out_node()?,
            input: b.converter_in_node()?,
        })
    }));
    domains.supply_groups(circuit, board_net, owner, &regulations)
}

/// The upper-cased alphabetic designator prefix of a reference ("JP3" -> "JP"),
/// read through the one designator table, so a split keyboard's mirrored `rC6`
/// is a `C`, not an `RC` resistor.
fn ref_prefix(reference: &str) -> String {
    hauksbee_models::refdes::prefix(&hauksbee_models::refdes::designator(reference))
}

/// Which passive ('R', 'C' or 'L') a designator's first letter claims, or an
/// Eagle `PC` polarised capacitor that two pads and a capacitance corroborate. A word
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
        .filter(|c| matches!(c, 'R' | 'C' | 'L'))
        .or_else(|| polarised_capacitor(comp, &prefix).then_some('C'));
    by_ref.map(|c| by_footprint.map_or(c, |(_, letter)| *letter))
}

/// An Eagle-style `PC` designator (polarised capacitor) that really is one:
/// two terminals and a capacitance value ("47uF/25V", "47u"). `PC` also names
/// photocouplers, so the reference alone never decides it.
fn polarised_capacitor(comp: &Component, prefix: &str) -> bool {
    prefix == "PC"
        && crate::program_resistor::electrical_terminal_nets(comp).is_some()
        && parse_value(&comp.value).is_some_and(|v| match v.unit.as_deref() {
            Some(unit) => unit == "F",
            None => v.si > 0.0 && v.si < 1.0,
        })
}

/// Connector-class prefixes: ports and mechanical parts, never simulated drivers.
fn is_port_class(prefix: &str) -> bool {
    matches!(prefix, "J" | "P" | "X" | "JP" | "TP" | "MP" | "H")
}

/// The pad map a KiCad generic transistor symbol spells in its name: in
/// `Q_NPN_BCE` or `Q_PMOS_GSD` pad n is the n-th electrode letter (a fourth
/// repeats one, the tab). `None` for any other value or a non-transistor kind.
fn generic_symbol_pins(
    value: &str,
    kind: ComponentKind,
) -> Option<std::collections::BTreeMap<String, String>> {
    use ComponentKind::{BjtNpn, BjtPnp, Nmos, Pmos};
    let upper = value.trim().to_ascii_uppercase();
    let (class, order) = upper.strip_prefix("Q_")?.split_once('_')?;
    let class_kind = matches!((class, kind), ("NPN", BjtNpn) | ("PNP", BjtPnp))
        || matches!((class, kind), ("NMOS", Nmos) | ("PMOS", Pmos));
    let pins = (order.chars().enumerate())
        .map(|(i, c)| {
            Some((
                (i + 1).to_string(),
                role_from_pinfunction(kind, &c.to_string())?,
            ))
        })
        .collect::<Option<std::collections::BTreeMap<_, _>>>()?;
    let roles: std::collections::BTreeSet<_> = pins.values().collect();
    (class_kind && (3..=4).contains(&pins.len()) && roles.len() == 3).then_some(pins)
}

/// The canonical generic-class value (`Q_NMOS` / `Q_PMOS`) of a MOSFET the
/// component names only by class, never by part.
///
/// The class comes from KiCad's generic symbol name (`Q_PMOS_GSD`, in the value
/// or as the `lib_id` symbol) or from a value that is only a class word
/// (`PMOS`, `MOSFET_P`, `P-MOSFET`, `PFET`, `P-CH`). It is `None` when the
/// component carries a part number anywhere: a filled MPN field, or a value
/// that is neither blank nor a class word (`AO3400A`, a rating string). It is
/// also `None` when two sources disagree on the polarity, since then the board
/// contradicts itself and no class is safe to assume.
fn generic_mosfet_class(comp: &Component) -> Option<&'static str> {
    use ComponentKind::{Nmos, Pmos};
    fn symbol(name: &str) -> Option<ComponentKind> {
        let upper = name.trim().to_ascii_uppercase();
        let rest = upper.strip_prefix("Q_")?;
        let (class, order) = rest.split_once('_').unwrap_or((rest, ""));
        let order_ok = order.is_empty()
            || ((3..=4).contains(&order.len()) && order.chars().all(|c| "DGS".contains(c)));
        match class {
            "NMOS" if order_ok => Some(Nmos),
            "PMOS" if order_ok => Some(Pmos),
            _ => None,
        }
    }
    /// What a value says about the part: nothing (blank or a bare `MOSFET`),
    /// a polarity class word, or a part number (`Named`).
    enum Value {
        Silent,
        Class(ComponentKind),
        Named,
    }
    fn word(value: &str) -> Value {
        let w: String = value.chars().filter(char::is_ascii_alphanumeric).collect();
        let w = w.to_ascii_uppercase().replace("CHANNEL", "CH");
        match w.as_str() {
            "" | "MOSFET" | "FET" | "MOS" => Value::Silent,
            "NMOS" | "NFET" | "NMOSFET" | "MOSFETN" | "NCH" | "NCHMOSFET" | "MOSFETNCH"
            | "NCHFET" | "NCHMOS" => Value::Class(Nmos),
            "PMOS" | "PFET" | "PMOSFET" | "MOSFETP" | "PCH" | "PCHMOSFET" | "MOSFETPCH"
            | "PCHFET" | "PCHMOS" => Value::Class(Pmos),
            _ => Value::Named,
        }
    }
    if mpn_properties(comp).next().is_some() || !(3..=4).contains(&comp.pins.len()) {
        return None;
    }
    let value = comp.value.trim().trim_matches('~');
    // A value that is neither a generic name nor a class word names a part.
    let by_value = match (symbol(value), word(value)) {
        (Some(kind), _) | (None, Value::Class(kind)) => Some(kind),
        (None, Value::Silent) => None,
        (None, Value::Named) => return None,
    };
    let by_symbol = symbol(symbol_name(&comp.lib_id));
    let kind = match (by_value, by_symbol) {
        (Some(a), Some(b)) if a != b => return None,
        (a, b) => a.or(b)?,
    };
    Some(if kind == Nmos { "Q_NMOS" } else { "Q_PMOS" })
}

/// A memory-card socket placed under any designator (a `U3` microSD socket is
/// still a connector): its symbol, footprint or value names the socket, or its
/// pins carry the SD-bus contacts (CMD, CLK, DAT0) beside a mechanical contact or
/// card-detect switch. The mechanical pin is what separates a socket from an SD
/// NAND or eMMC chip, which names the same bus pins but has no shell or detect.
fn is_card_socket(comp: &Component) -> bool {
    const MECHANICAL: [&str; 7] = [
        "SHIELD",
        "SHELL",
        "CD",
        "DET",
        "DETECT",
        "CARD_DETECT",
        "SW",
    ];
    let named = [&comp.value, &comp.footprint, &comp.lib_id]
        .iter()
        .any(|s| {
            let s = s.to_ascii_uppercase().replace(['-', '_', ' '], "");
            let socket = ["MICROSD", "SDCARD", "TFCARD", "CARDSOCKET"]
                .iter()
                .any(|k| s.contains(k));
            socket && !s.contains("NAND") && !s.contains("EMMC")
        });
    let has = |names: &[&str]| {
        comp.pins.iter().any(|p| {
            let f = p.function.trim().to_ascii_uppercase();
            names.iter().any(|n| f == *n)
        })
    };
    let bus = has(&["CMD"]) && has(&["CLK"]) && has(&["DAT0", "D0"]);
    named || (bus && has(&MECHANICAL))
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
        // The DIP-28 and 32-pad packages number the port pins differently
        // (matches the two db/mcu.toml entries).
        let pins = atmega328p_pin_map(comp);
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
    if is_card_socket(comp) {
        return Some(make_entry(
            "card_socket_fallback",
            ComponentKind::Connector,
            "memory-card socket (engine fallback by socket name or SD contact pins)",
            hauksbee_models::Params::default(),
            BTreeMap::new(),
        ));
    }
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
            // A Zener's reverse breakdown is its function. Bound as a 110 V
            // signal diode it silently loses every clamp it provides, so a
            // part marked as a Zener (its value or symbol says so, or its
            // value is only a voltage) keeps the stated voltage as its
            // breakdown, and one with no stated voltage stays unmodelled.
            // A TVS named by its family (SMAJ5.0A, P6KE15A) states the
            // voltage it stands off in its part number. A bidirectional one
            // (SMBJ6.0CA) conducts both ways past it, which no one-way diode
            // curve stands in for, so it stays unmodelled.
            if let Some(tvs) = hauksbee_models::value::parse_tvs_name(&comp.value) {
                return tvs_fallback(&tvs);
            }
            let zener = zener_mark(comp);
            if matches!(zener, ZenerMark::Unstated) {
                return None;
            }
            if let ZenerMark::Volts(vz) = zener {
                let mut params = diode_1n4148_params();
                params.set_f64("bv", vz);
                params.set_f64("ibv", ZENER_FALLBACK_KNEE_A);
                return Some(make_entry(
                    "zener_diode_fallback",
                    ComponentKind::Diode,
                    &format!(
                        "generic Zener diode (engine fallback: breakdown {vz} V from the \
                         part's value, knee current ASSUMED {} mA, forward junction \
                         estimated from the 1N4148 card; put the ordering code on the \
                         component to replace it)",
                        ZENER_FALLBACK_KNEE_A * 1e3
                    ),
                    params,
                    [("1", "cathode"), ("2", "anode")]
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                ));
            }
            // The forward curve is a stand-in; the BLOCKING voltage is not
            // known at all. The 1N4148 card's 110 V breakdown belongs to that
            // part, and a board's unrecognised diode (an SS12 is 20 V) may be
            // rated a fifth of it, so the junction gets no breakdown and the
            // part is held to the lowest VRRM any rectifier or Schottky is
            // sold with ([`UNKNOWN_DIODE_VRRM_FLOOR_V`]) as a stand-in limit.
            // Past it the finding is a stated condition naming the part, never
            // a pass and never a failure ([`hauksbee_models::rating_basis`]).
            let mut params = diode_1n4148_params();
            params.0.remove("bv");
            let mut entry = make_entry(
                "signal_diode_1n4148_fallback",
                ComponentKind::Diode,
                "generic diode (engine fallback: unrecognised value or bare diode \
                 footprint -> 1N4148 forward curve, Philips/Vishay SPICE model; blocking \
                 voltage UNKNOWN, held to a 20 V class floor)",
                params,
                // KiCad Device:D Sim.Pins is "1=K 2=A" (SOD cathode=pin1); the
                // netlist also carries pinfunction K/A which the binder reads
                // first, so this pad map is the PCB-only-extraction fallback.
                [("1", "cathode"), ("2", "anode")]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            );
            entry.ratings.max_voltage_v = Some(UNKNOWN_DIODE_VRRM_FLOOR_V);
            // Nor is its current rating known: the 1N4148's own is a small
            // signal diode's. Held to the lowest forward current a small
            // rectifier is sold with, a stand-in like the voltage floor, so a
            // power path through an unrecognised diode is named, not passed.
            entry.ratings.max_current_a = Some(UNKNOWN_DIODE_IF_FLOOR_A);
            return Some(entry);
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
        // A module value (`nice_nano`, `T-Micro32_Plus`) routes as the MCU it
        // carries, per the data table `db/mcu_modules.toml`.
        let token = hauksbee_models::mcu_modules::mcu_module_alias(&token)
            .map_or(token, |alias| alias.mcu.to_ascii_uppercase());
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
        return [comp.value.as_str(), symbol_name(&comp.footprint)]
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
    out.extend(mpn_properties(comp).map(str::to_string));
    // The symbol name only: the library nickname before the `:` names a
    // library, a sheet cache or a project (`ESP32-PoE_Rev_M1-cache:SN74LVC1G04`),
    // not the part, so it is no evidence of the part's family.
    if let Some(v) = non_empty(symbol_name(&comp.lib_id)) {
        out.push(v);
    }
    out
}

/// The part-name half of a `Library:Symbol` id (the whole id when it has no
/// library nickname).
fn symbol_name(lib_id: &str) -> &str {
    lib_id.rsplit_once(':').map_or(lib_id, |(_, name)| name)
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
    // An on-chip regulator's OUTPUT pad (the ESP flash LDO's VDD_SDIO/VDD_SPI,
    // a SAM VDDCORE) is not a supply input: left unconnected it reads 0 V and
    // failed the supply check of a module that was powered. A module's bare
    // "3V3" pad, by contrast, is its supply input.
    if ["VDD_SDIO", "VDD_SPI", "VDDCORE"]
        .iter()
        .any(|out| upper.starts_with(out))
    {
        return None;
    }
    if matches!(upper.as_str(), "3V3" | "+3V3" | "3.3V") {
        return Some("vdd".to_string());
    }
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

/// The names a pad's printed function lists, whole string first. A GPIO name
/// that follows an underscore (`RTC_GPIO4`, `LP_GPIO4`, `RTC_IO4`) is the
/// low-power domain's numbering, which matches the pad's GPIO on some chips
/// (ESP32-S3 RTC_GPIO4 is GPIO4, ESP32-C6 LP_GPIO4 is GPIO4) and not on others
/// (the ESP32 pad `GPI34/ADC1_CH6/RTC_GPIO4` is GPIO34). The name alone can't
/// tell which, so it is never offered as a bare `GPIOn`; those chips also print
/// the plain `GPIOn` on the pad. `ADC1_CHn` and `TOUCHn` name no GPIO.
fn pinfunction_tokens(upper: &str) -> Vec<String> {
    fn push(seg: &str, sep_before: Option<char>, tokens: &mut Vec<String>) {
        if seg.is_empty() || (sep_before == Some('_') && is_gpio_name(seg)) {
            return;
        }
        tokens.push(seg.to_string());
    }
    let mut tokens = vec![upper.to_string()];
    let mut prev_sep: Option<char> = None;
    let mut seg_start = 0usize;
    for (i, c) in upper.char_indices() {
        if !c.is_ascii_alphanumeric() {
            push(&upper[seg_start..i], prev_sep, &mut tokens);
            seg_start = i + c.len_utf8();
            prev_sep = Some(c);
        }
    }
    push(&upper[seg_start..], prev_sep, &mut tokens);
    tokens
}

/// `GPIO4` or `IO4`: a bare GPIO number with nothing else in the name.
fn is_gpio_name(seg: &str) -> bool {
    let digits = seg
        .strip_prefix("GPIO")
        .or_else(|| seg.strip_prefix("IO"))
        .unwrap_or("");
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
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
/// canonical Philips/Vishay 1N4148 model (e.g. LCSC C2099 = JSCJ 1N4148W in SOD-323,
/// a standard 1N4148-family switching diode):
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

/// The blocking voltage (V) an unrecognised diode is held to.
///
/// The lowest VRRM the common rectifier and Schottky families are sold with
/// (SS12, 1N5820, MBR0520L, SB120 are all 20 V). A stand-in limit, never a rating: the real
/// part may block far more, so passing it proves nothing and exceeding it is a
/// condition to resolve by naming the part, not a fault.
pub const UNKNOWN_DIODE_VRRM_FLOOR_V: f64 = 20.0;

/// Knee current given to a Zener bound by the engine fallback. ASSUMED: an
/// estimate for small-signal Zeners, not read from any datasheet; it only
/// keeps the knee sharp enough that the clamp sits at the stated voltage.
const ZENER_FALLBACK_KNEE_A: f64 = 0.005;

/// The forward current an unrecognised diode is held to.
///
/// It is the lowest average forward current small rectifiers and Schottkys
/// are sold with (BAT54-class parts are 0.2 A). A stand-in, so passing it is a
/// stated condition.
pub const UNKNOWN_DIODE_IF_FLOOR_A: f64 = 0.2;

/// The fallback for a TVS whose part number names its family and voltage
/// ([`hauksbee_models::value::parse_tvs_name`]).
///
/// It takes the 1N4148 forward curve as a stand-in, no breakdown modelled (it
/// would clamp a rail on an estimate), and the standoff the name states as its
/// voltage rating. The card is an engine fallback, so a rail past that
/// standoff is a stated condition naming the part, never a failure.
fn tvs_fallback(
    &(family, volts, names, bidirectional): &(
        &str,
        f64,
        hauksbee_models::value::TvsNameVoltage,
        bool,
    ),
) -> Option<ModelEntry> {
    use hauksbee_models::value::{TvsNameVoltage, TVS_STANDOFF_PER_BREAKDOWN};
    if bidirectional {
        return None;
    }
    let (standoff, how) = match names {
        TvsNameVoltage::Standoff => (volts, "the standoff its part number names"),
        TvsNameVoltage::Breakdown => (
            volts * TVS_STANDOFF_PER_BREAKDOWN,
            "the lowest standoff its part number's breakdown voltage gives",
        ),
    };
    let mut params = diode_1n4148_params();
    params.0.remove("bv");
    let mut entry = make_entry(
        "tvs_diode_fallback",
        ComponentKind::Diode,
        &format!(
            "generic {family} TVS diode (engine fallback: standoff {}, {how}; \
             forward curve estimated from the 1N4148 card, clamp not modelled; put the \
             ordering code on the component to replace it)",
            format_volts(standoff)
        ),
        params,
        [("1", "cathode"), ("2", "anode")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    );
    entry.ratings.max_voltage_v = Some(standoff);
    Some(entry)
}

/// Whether a diode is marked as a Zener, and the breakdown voltage it states.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ZenerMark {
    /// Not marked as a Zener. A rating string on another diode (`SS34 40V 3A`)
    /// is not a Zener mark.
    NotZener,
    /// A Zener whose voltage the board does not state (`BZT52C18` on a
    /// `D_Zener` symbol), which must not be guessed.
    Unstated,
    /// The value states this many volts (`18V Zener 200mW`, or a value that is
    /// only a voltage, `5.1V` / `5V1`, which on a diode is a Zener's voltage).
    Volts(f64),
}

fn zener_mark(comp: &Component) -> ZenerMark {
    let says_zener = |s: &str| s.to_ascii_lowercase().contains("zener");
    let value = comp.value.trim();
    // A value that is only a voltage: `18V`, `5.1V`, or the RKM `5V1`.
    let only_voltage = value
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'V' | 'v'))
        .then(|| {
            let (whole, frac) = value.split_once(['V', 'v'])?;
            let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
            if frac.is_empty() {
                whole.parse::<f64>().ok()
            } else if digits(whole) && digits(frac) {
                format!("{whole}.{frac}").parse::<f64>().ok()
            } else {
                None
            }
        })
        .flatten();
    if !(only_voltage.is_some() || says_zener(value) || says_zener(symbol_name(&comp.lib_id))) {
        return ZenerMark::NotZener;
    }
    let stated = only_voltage.or_else(|| {
        says_zener(value)
            .then(|| hauksbee_models::value::parse_voltage_rating_v(value))
            .flatten()
    });
    stated
        .filter(|v| v.is_finite() && *v > 0.0)
        .map_or(ZenerMark::Unstated, ZenerMark::Volts)
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

/// Whether an `ATmega328P` sits in a 32-pad package (TQFP-32, VQFN/MLF-32):
/// its footprint names one, or its ordering code carries the `-AU`/`-MU`/`-MMH`
/// package suffix. Anything else keeps the DIP-28 numbering.
fn atmega328p_is_32pad(comp: &Component) -> bool {
    let footprint = comp.footprint.to_ascii_uppercase();
    let names_32pad = ["QFP", "QFN", "MLF"].iter().any(|family| {
        footprint.match_indices(family).any(|(at, _)| {
            let rest = footprint[at + family.len()..].trim_start_matches(['-', '_']);
            rest.strip_prefix("32")
                .is_some_and(|tail| !tail.starts_with(|c: char| c.is_ascii_digit()))
        })
    });
    let value = comp.value.trim().to_ascii_uppercase();
    let coded_32pad = value
        .strip_prefix("ATMEGA328P")
        .or_else(|| value.strip_prefix("ATMEGA328"))
        .map(|rest| rest.trim_start_matches(['-', '_']))
        .is_some_and(|code| {
            matches!(
                code,
                "AU" | "AUR" | "MU" | "MUR" | "MMH" | "MMHR" | "MN" | "MNR"
            )
        });
    names_32pad || coded_32pad
}

/// The `ATmega328P` pad→role map for `comp`'s package (mirrors the
/// `atmega328p` and `atmega328p_32pad` entries in db/mcu.toml).
fn atmega328p_pin_map(comp: &Component) -> std::collections::BTreeMap<String, String> {
    let map: &[(&str, &str)] = if atmega328p_is_32pad(comp) {
        &ATMEGA328P_32PAD_PINS
    } else {
        &ATMEGA328P_DIP28_PINS
    };
    map.iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

/// `ATmega328P` 32-lead TQFP / 32-pad VQFN numbering (DS40001984 section 4);
/// pad 33 is the VQFN's exposed ground pad.
const ATMEGA328P_32PAD_PINS: [(&str, &str); 33] = [
    ("1", "pd3_int1_oc2b"),
    ("2", "pd4_t0_xck"),
    ("3", "gnd"),
    ("4", "vcc"),
    ("5", "gnd2"),
    ("6", "vcc2"),
    ("7", "pb6_xtal1"),
    ("8", "pb7_xtal2"),
    ("9", "pd5_t1_oc0b"),
    ("10", "pd6_ain0_oc0a"),
    ("11", "pd7_ain1"),
    ("12", "pb0_icp1"),
    ("13", "pb1_oc1a"),
    ("14", "pb2_ss_oc1b"),
    ("15", "pb3_mosi_oc2a"),
    ("16", "pb4_miso"),
    ("17", "pb5_sck"),
    ("18", "avcc"),
    ("19", "adc6"),
    ("20", "aref"),
    ("21", "gnd3"),
    ("22", "adc7"),
    ("23", "pc0_adc0"),
    ("24", "pc1_adc1"),
    ("25", "pc2_adc2"),
    ("26", "pc3_adc3"),
    ("27", "pc4_adc4_sda"),
    ("28", "pc5_adc5_scl"),
    ("29", "pc6_reset"),
    ("30", "pd0_rxd"),
    ("31", "pd1_txd"),
    ("32", "pd2_int0"),
    ("33", "gnd4"),
];

/// `ATmega328P` DIP-28 numbering.
const ATMEGA328P_DIP28_PINS: [(&str, &str); 28] = [
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
];

/// A component's likely manufacturer part number, as model matching uses it.
///
/// Prefer a dedicated manufacturer-PN field; distributor part numbers (e.g.
/// "Mouser Part Number" = "621-BCM857BS-7-F") carry a distributor prefix that
/// breaks anchored mpn regexes, so avoid them. Fall back to the value field,
/// which is what most `mpn_re` rules are actually written for. A part number a
/// BOM supplied sits under the reserved [`hauksbee_extract::bom::MPN_PROPERTY`]
/// key and is preferred over the heuristic scan, because the scan cannot tell a
/// manufacturer part number from a distributor code and a board carrying both
/// would lose the bind to the one that does not match. The scan takes the
/// first NON-EMPTY MPN field: an empty `MPN` field ahead of a filled
/// `Manufacturer_No` is no evidence and must not hide it.
#[must_use]
pub fn component_mpn(comp: &Component) -> Option<String> {
    (comp.properties.iter())
        .find(|(k, v)| k == hauksbee_extract::bom::MPN_PROPERTY && !v.trim().is_empty())
        .map(|(_, v)| v.trim())
        .or_else(|| mpn_properties(comp).next())
        .map(str::to_string)
        .or_else(|| non_empty(&comp.value))
}

/// The component's non-empty manufacturer-part-number fields, in board order.
fn mpn_properties(comp: &Component) -> impl Iterator<Item = &str> {
    comp.properties
        .iter()
        .filter(|(k, v)| !v.trim().is_empty() && hauksbee_extract::bom::is_mpn_property_key(k))
        .map(|(_, v)| v.trim())
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
    let mpn = mpn_properties(comp).next().unwrap_or("");
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
        return match bind_model_peripheral(comp, model, spec, &role_nets, node_of, board, circuit) {
            Ok(binding) => {
                let assumed = binding.straps_assumed.as_ref().map(|why| {
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

    // A strap resistor that selects a row of the card's table sets those
    // params for this board before any kind's binder reads them; a strap that
    // cannot be read, or reads between rows, is said, never interpolated.
    let (resolved, strap_note) =
        resolve_resistor_bins(comp, model, &role_nets, circuit, components, node_of);
    let model = resolved.as_ref();

    let (outcome, warning) = match model.kind {
        Passive => bind_passive(comp, model, circuit, node_of),
        Diode => bind_diode(comp, model, circuit, &role_nets),
        BjtNpn | BjtPnp => bind_bjt(comp, model, circuit, &role_nets),
        Nmos | Pmos => bind_mosfet(comp, model, circuit, &role_nets),
        Vreg => bind_vreg(
            comp, model, source, circuit, &role_nets, regulators, components, node_of,
        ),
        Opamp => bind_opamp(comp, model, circuit, &role_nets),
        Comparator => bind_comparator(comp, model, circuit, &role_nets, power_nets),
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
        Mcu => unpowerable_mcu_card(comp, model, node_of).map_or_else(
            || {
                let backend = mcu_backend_string(comp, model);
                let warning = bind_mcu(comp, model, circuit, node_of, &pad_nodes, power_nets, mcus);
                (BindOutcome::Mcu { backend }, warning)
            },
            |reason| {
                eprintln!("[bind] {}: {reason}", comp.reference);
                let warning = Some(format!("{}: {reason}", comp.reference));
                (BindOutcome::Unresolved { reason }, warning)
            },
        ),
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
    // The strap note leads: it carries the partial-model marker, which only
    // counts at the front of the joined string.
    let warning = if strap_note.is_some() {
        join_caveats(comp, [strap_note, warning])
    } else {
        warning
    };
    (outcome, warning, guesses)
}

/// The card with each `[[models.resistor_bins]]` table resolved against this
/// board: the strap resistance the table's pin reads to ground (the same
/// bounded nodal solve [`charge_limit`] uses) picks a row, and that row's
/// params join the card's. Borrowed unchanged for a card with no tables.
///
/// The note is a partial-model caveat for every table that set nothing:
/// a strap that cannot be read, one that lands between the printed rows, or a
/// row that selects a mode the card names (`note`) rather than parameterises.
fn resolve_resistor_bins<'m>(
    comp: &Component,
    model: &'m ModelEntry,
    roles: &HashMap<String, NodeId>,
    circuit: &Circuit,
    components: &[Component],
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> (std::borrow::Cow<'m, ModelEntry>, Option<String>) {
    use hauksbee_models::schema::ParamValue;
    if model.resistor_bins.is_empty() {
        return (std::borrow::Cow::Borrowed(model), None);
    }
    let is_ground =
        |id| node_of(Some(id)).map(|n| n.is_ground() || is_ground_net(circuit.node_name(n)));
    let excluded = components
        .iter()
        .position(|c| c.reference == comp.reference);
    let mut entry = model.clone();
    let mut notes = Vec::new();
    for table in &model.resistor_bins {
        let pin = &table.pin;
        let names = {
            let mut all: Vec<&str> = table
                .bins
                .iter()
                .flat_map(|b| b.params.keys().map(String::as_str))
                .collect();
            all.sort_unstable();
            all.dedup();
            all.join(", ")
        };
        let read = roles
            .get(pin)
            .and_then(|node| {
                comp.pins
                    .iter()
                    .find(|p| node_of(p.net) == Some(*node))?
                    .net
            })
            .and_then(|net| program_resistance_to_ground(components, &is_ground, net, 4, excluded));
        let Some((ohms, path)) = read else {
            notes.push(format!(
                "its `{pin}` strap resistor could not be read off the board (no populated \
                 resistor path to ground, or a part on the net that is not a plain resistor), \
                 so the settings it selects ({names}) are not set and what depends on them is \
                 not modelled"
            ));
            continue;
        };
        let via = path.join("+");
        let Some(bin) = table.select(ohms) else {
            notes.push(format!(
                "its `{pin}` strap {via} reads {ohms:.0} ohm, which is within {} of no row \
                 of the datasheet's table, so the settings it selects ({names}) are not set and \
                 what depends on them is not modelled",
                format_percent(table.tolerance * 100.0, 0)
            ));
            continue;
        };
        for (name, value) in &bin.params {
            entry
                .params
                .0
                .insert(name.clone(), ParamValue::Float(*value));
        }
        if let Some(note) = &bin.note {
            notes.push(format!(
                "its `{pin}` strap {via} reads {ohms:.0} ohm, the datasheet's {} row: {note}",
                bin.label()
            ));
        }
    }
    let note = (!notes.is_empty()).then(|| {
        format!(
            "{}{} ({}): {}",
            Assumption::PARTIAL_MODEL_MARKER,
            comp.reference,
            comp.value,
            notes.join("; ")
        )
    });
    (std::borrow::Cow::Owned(entry), note)
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
/// `Err((reason, warning))` when the board does not wire the bus roles or the
/// supply roles the spec needs, or holds a bus-select strap at the level of a
/// personality the spec does not model: a flash with no CS or an EEPROM with
/// no SDA is left open, not counted as modelled because its part number was
/// recognised. A register-map spec's own `[sensor.pins]` fills the roles the
/// card's `[models.pins]` leaves out, and its address-select table picks the
/// address from the board's straps.
#[allow(clippy::too_many_lines)] // one refusal per unwired bus/power role
fn bind_model_peripheral(
    comp: &Component,
    model: &ModelEntry,
    spec: PeripheralSpec,
    role_nets: &HashMap<String, NodeId>,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
    (components, power_nets): (&[Component], &HashMap<String, f64>),
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

    let mut straps_assumed = None;
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
            // A level is ground, a rail the board's power-net resolution
            // accepted, or the part's own supply or ground pin: a datasheet's
            // "tie CSB to VDDIO" names the pin, whatever the rail is called.
            let own = |ground: bool| {
                (role_nets.iter())
                    .filter(move |(role, _)| {
                        let r = role.to_ascii_lowercase();
                        if ground {
                            r.starts_with("gnd") || r.contains("vss")
                        } else {
                            r.contains("vdd") || r.contains("vcc") || r == "vio"
                        }
                    })
                    .map(|(_, node)| *node)
            };
            let rail_level = |node: NodeId| {
                let high = power_nets.contains_key(circuit.node_name(node))
                    || own(false).any(|n| n == node);
                let low = node.is_ground() || own(true).any(|n| n == node);
                (high || low).then_some(!low)
            };
            let strap_level = |role: &str| -> Result<bool, String> {
                let node = role_nets
                    .get(role)
                    .copied()
                    .ok_or_else(|| format!("strap role '{role}' is not connected"))?;
                let held = board_strap_level(node, (comp, components), node_of, &rail_level);
                held.map_err(|why| {
                    let net = circuit.node_name(node);
                    format!("strap role '{role}' is on '{net}', {why}")
                })
            };
            // A strap the board holds at the other level selects a personality
            // this map does not model, so it stays unattached. One the board
            // does not hold (floating, or on a part that can drive it) is set
            // by firmware or the part's own default: the map attaches, and the
            // level it assumed is disclosed, as an unresolved address strap is.
            let mut assumed: Vec<String> = Vec::new();
            for (role, want) in (required_high_roles.iter().map(|r| (r, true)))
                .chain(required_low_roles.iter().map(|r| (r, false)))
            {
                let level = if want { "high" } else { "low" };
                match strap_level(role) {
                    Ok(held) if held == want => {}
                    Ok(_) => {
                        let actual = if want {
                            "resolved low"
                        } else {
                            "resolved high"
                        };
                        return Err(refuse(
                            format!(
                                "register-map bus personality requires role '{role}' {level}, but it is {actual}"
                            ),
                            format!(
                                "model '{}' register-map behavior not attached: role '{role}' must resolve {level} ({actual})",
                                model.id
                            ),
                        ));
                    }
                    Err(why) => assumed.push(format!(
                        "bus personality assumed ({role} {level}); its level is unknown, \
                         firmware-driven or floating: {why}"
                    )),
                }
            }
            let address = match (
                address_select_role.as_deref(),
                address_when_low,
                address_when_high,
            ) {
                (Some(role), Some(low), Some(high)) => match strap_level(role) {
                    Ok(false) => Some(*low),
                    Ok(true) => Some(*high),
                    Err(why) => {
                        let default = sensor.i2c_address;
                        let shown = default.map_or_else(|| "?".into(), |a| format!("0x{a:02x}"));
                        assumed.push(format!(
                            "I2C address {shown} is assumed (the spec default): {why}"
                        ));
                        default
                    }
                },
                _ if sensor.address_selects.is_empty() => None,
                _ => sensor
                    .resolve_address(|role| {
                        // A strap wired straight onto a bus line is at that line's
                        // level, which a four-level address pin reads as such.
                        let node = role_nets.get(role);
                        let on = |bus: &String| node.is_some() && role_nets.get(bus) == node;
                        if on(scl_role) {
                            return Ok(StrapTie::Scl);
                        }
                        if on(sda_role) {
                            return Ok(StrapTie::Sda);
                        }
                        strap_level(role)
                            .map(|high| if high { StrapTie::High } else { StrapTie::Low })
                    })
                    .map(|choice| {
                        assumed.extend(choice.assumed);
                        choice.address
                    }),
            };
            straps_assumed = (!assumed.is_empty()).then(|| assumed.join("; "));
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

    let bus_roles: Vec<&str> = match &spec {
        PeripheralSpec::I2cEeprom { .. } => vec!["scl", "sda"],
        PeripheralSpec::SpiNorFlash {
            cs_role,
            clk_role,
            mosi_role,
            miso_role,
            ..
        } => [cs_role, clk_role, mosi_role, miso_role]
            .map(String::as_str)
            .to_vec(),
        PeripheralSpec::RegisterMap {
            scl_role,
            sda_role,
            cs_role,
            clk_role,
            mosi_role,
            miso_role,
            ..
        } => match sensor.as_ref().map(|s| s.bus) {
            Some(Bus::Spi) => [cs_role, clk_role, mosi_role, miso_role]
                .map(String::as_str)
                .to_vec(),
            _ => [scl_role, sda_role].map(String::as_str).to_vec(),
        },
    };
    let bus_nodes = bus_nodes_of(role_nets, &bus_roles);
    Ok(PeripheralBinding {
        reference: comp.reference.clone(),
        spec,
        cs_net,
        i2c_address_override,
        straps_assumed,
        power,
        bus_nodes,
    })
}

/// The level the board's own parts hold the strap node `start` at, for the
/// part `owner`, whatever order the parts are bound in.
///
/// The node's tie set is everything joined to it through closed links (a 0 Ω
/// part, a closed solder jumper). A tie set holding a `level` is at it, and
/// one holding both is refused. Otherwise every assembled resistor leaving the
/// set must end on the same level, a pull. Capacitors, open links, connectors
/// and test points set no level; any other part on the set may drive it, so
/// the level is firmware-driven, and nothing at all leaves it floating. The
/// `Err` says which, in words that follow "strap role 'x' is on 'NET', ".
fn board_strap_level(
    start: NodeId,
    (owner, components): (&Component, &[Component]),
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
    level: &dyn Fn(NodeId) -> Option<bool>,
) -> Result<bool, String> {
    use crate::program_resistor::{classify_series, electrical_terminal_nets, Series};
    let parts = || {
        (components.iter()).filter(|c| {
            c.reference != owner.reference
                && !matches!(AssemblyState::of(c), AssemblyState::DnpAbsent(_))
        })
    };
    let ends = |c: &Component| {
        let (a, b) = electrical_terminal_nets(c)?;
        Some((node_of(Some(a))?, node_of(Some(b))?))
    };
    let mut ties = vec![start];
    let mut grew = true;
    while grew {
        grew = false;
        for (a, b) in parts()
            .filter(|c| matches!(classify_series(c), Series::Closed))
            .filter_map(ends)
        {
            for (near, far) in [(a, b), (b, a)] {
                // A tie through a rail would join every net on that rail.
                if ties.contains(&near) && level(near).is_none() && !ties.contains(&far) {
                    ties.push(far);
                    grew = true;
                }
            }
        }
    }
    let held: Vec<bool> = ties.iter().filter_map(|n| level(*n)).collect();
    if let Some(&first) = held.first() {
        return if held.iter().all(|h| *h == first) {
            Ok(first)
        } else {
            Err("which is tied to both a supply and ground".to_string())
        };
    }
    let on_ties = |c: &&Component| {
        c.pins
            .iter()
            .any(|p| node_of(p.net).is_some_and(|n| ties.contains(&n)))
    };
    let mut pulls = Vec::new();
    for c in parts().filter(on_ties) {
        if is_port_class(&ref_prefix(&c.reference)) {
            continue;
        }
        let pull = ends(c).filter(|_| !matches!(classify_series(c), Series::Unknown));
        let Some((a, b)) = pull else {
            return Err(format!(
                "which {} ({}) can drive, so its level is firmware-driven",
                c.reference, c.value
            ));
        };
        match classify_series(c) {
            Series::Ohms(_) => {
                let far = if ties.contains(&a) { b } else { a };
                let Some(to) = level(far) else {
                    return Err(format!("pulled by {} to an unresolved net", c.reference));
                };
                pulls.push(to);
            }
            Series::Closed | Series::Open | Series::Unknown => {}
        }
    }
    match pulls.first() {
        Some(&first) if pulls.iter().all(|p| *p == first) => Ok(first),
        Some(_) => Err("which is pulled both high and low".to_string()),
        None => Err("which nothing on the board holds at a level (floating)".to_string()),
    }
}

/// The distinct non-ground nodes the named bus `roles` are wired to, in the
/// order named; a role with no connection is skipped.
fn bus_nodes_of(role_nets: &HashMap<String, NodeId>, roles: &[&str]) -> Vec<NodeId> {
    let mut out: Vec<NodeId> = Vec::new();
    for node in roles.iter().filter_map(|r| role_nets.get(*r)) {
        if !node.is_ground() && !out.contains(node) {
            out.push(*node);
        }
    }
    out
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
    let (mut guesses, mut clash, pad_count) = (Vec::new(), None, comp.distinct_pad_count());
    let canon = |r: &str| role_from_pinfunction(kind, r).unwrap_or_else(|| r.to_ascii_lowercase());
    let standard = hauksbee_models::validation::pin_standard(model);
    for pin in &comp.pins {
        let (n, node) = (&pin.number, node_of(pin.net));
        let roles = pad_role_sources(pin, &model.pins, Some(kind));
        let named = roles.iter().flatten().next().cloned();
        // A role the card's pads give this pad bound its node, even beside a pin-function.
        let own = |r: &&String| roles[0].as_ref() != Some(*r) && m.get(*r) == node.as_ref();
        let carded = node.and_then(|_| roles[1].iter().chain(&roles[3]).find(own));
        // A pad number repeated on one footprint (a TO-263/SOT-223 tab sharing
        // the centre lead's number) is one pad of the card: one guess, since a
        // guess's assumption id is keyed by the pad number.
        let said = |g: &Vec<String>| g.iter().any(|s| s.starts_with(&format!("pad {n} role ")));
        if let (Some(convention), Some(role), false) = (standard, carded, said(&guesses)) {
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
            guesses.push(crate::bind_report::rule_guess_text(
                n,
                role,
                id,
                &comp.footprint,
                pad_count,
            ));
        }
    }
    // A clash matters only when the rule or the package standard supplies a role.
    match clash.filter(|_| !guesses.is_empty()) {
        Some(why) => (m, Vec::new(), Some(why)),
        None => (m, guesses, None),
    }
}

/// Each role of `model` on `comp` and the board net id its pad sits on.
///
/// Read exactly as the binder reads them (pin-functions, the card's pads, then
/// the pin-rule table). Empty when a rule contradicts a pad the board names,
/// where the binder leaves the part open.
#[must_use]
pub fn role_nets(
    comp: &Component,
    model: &ModelEntry,
    pin_rules: &hauksbee_models::PinRuleTable,
) -> HashMap<String, i64> {
    let node_of = |net: Option<i64>| {
        net.filter(|&id| id > 0)
            .and_then(|id| u32::try_from(id).ok())
            .map(NodeId)
    };
    let (roles, _, clash) = role_node_map_guessed(comp, model, &node_of, pin_rules);
    if clash.is_some() {
        return HashMap::new();
    }
    (roles.into_iter())
        .map(|(role, node)| (role, i64::from(node.0)))
        .collect()
}

/// The pads of `comp` that carry `role` as the binder binds it.
///
/// Each pad whose own role (its first naming source, then the pin-rule table
/// for an unnamed pad) is `role` and which sits on the net [`role_nets`] binds
/// `role` to.
///
/// This is how a terminal of a stamped device is traced back to copper (the
/// as-built overlay's cuts and lifts, mirrored onto the static board). Empty
/// when the role is unbound or the part would be left open.
#[must_use]
pub fn role_pads(
    comp: &Component,
    model: &ModelEntry,
    pin_rules: &hauksbee_models::PinRuleTable,
    role: &str,
) -> Vec<String> {
    let Some(&net) = role_nets(comp, model, pin_rules).get(role) else {
        return Vec::new();
    };
    let (kind, pad_count) = (model.kind, comp.distinct_pad_count());
    let own_role = |pin: &hauksbee_extract::Pin| {
        let named = pad_role_sources(pin, &model.pins, Some(kind))
            .into_iter()
            .flatten()
            .next();
        named.or_else(|| {
            (pin_rules.role_for_pad(&comp.footprint, Some(kind), pad_count, &pin.number))
                .map(|inferred| inferred.role)
        })
    };
    (comp.pins.iter())
        .filter(|pin| pin.net == Some(net) && own_role(pin).as_deref() == Some(role))
        .map(|pin| pin.number.clone())
        .collect()
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
        kind.and_then(|k| role_from_shared_pinfunction(k, &pin.function, card(&pin.number))),
        card(&pin.number),
        kind.and_then(|k| role_from_electrode_pad(k, &pin.number)),
        ordinal_pad(&pin.number).and_then(|ord| card(&ord)),
    ]
}

/// A pin-function's role, reading a symbol shared by a part's variants
/// (`ADJ/GND`: the adjustable part's ADJ, the fixed part's GND) as the one
/// alternative the card gives this pad. Alternatives naming no role the card
/// gives it leave the pad unnamed, so a pin rule or the package standard still
/// reports its guess.
fn role_from_shared_pinfunction(
    kind: ComponentKind,
    function: &str,
    card: Option<String>,
) -> Option<String> {
    role_from_pinfunction(kind, function).or_else(|| {
        let mut alternatives = function
            .split('/')
            .filter_map(|f| role_from_pinfunction(kind, f));
        card.filter(|c| function.contains('/') && alternatives.any(|r| r == *c))
    })
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
                        if model.passive_class == Some(hauksbee_models::schema::PassiveClass::Fuse)
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
                device: format!("R {}", format_engineering_web(ohms, "Ω")),
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
                        format_engineering_web(rkm_henries(&p), "H"),
                        format_engineering_web(dcr, "Ω")
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
                    device: format!(
                        "C {} with sourced parasitics",
                        format_engineering_web(p.si, "F")
                    ),
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

/// Resistance a literal 0 Ω resistor is bound at, and the conditioning floor
/// below which any resistor is raised to it (see `passive_device`).
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
    } else if p.si <= 0.0 || p.si.is_nan() {
        // A literal 0 resistance is a fitted jumper link, not a
        // mathematical short. Binding it at the raw 1e-6 floor leaves ten
        // near-short conductances that wreck the analog solve's conditioning
        // (anyshake/explorer). Bind it at the same milliohm the supply legs use
        // for "electrically negligible but solver-safe", and SAY SO: the value
        // on the board is then not the value in the matrix, and a silent
        // substitution is exactly the class of thing this project refuses.
        // A negative (or unreadable) value is no resistance at all; it is bound
        // the same way so the board still solves, but the note says the value is
        // wrong rather than calling it a jumper.
        let note = if p.si < 0.0 || p.si.is_nan() {
            format!(
                "{name}: value '{}' is not a resistance (negative or unreadable), bound as a {:.0} mohm link so the solve stays finite; correct the value",
                comp.value,
                ZERO_OHM_JUMPER_OHMS * 1e3,
            )
        } else {
            format!(
                "{name}: value '{}' is a 0 ohm jumper, bound as a {:.0} mohm link so the solve stays finite (an infinite conductance would poison the matrix)",
                comp.value,
                ZERO_OHM_JUMPER_OHMS * 1e3,
            )
        };
        (
            Device::resistor(name, a, b, ZERO_OHM_JUMPER_OHMS),
            Some(note),
        )
    } else if p.si < ZERO_OHM_JUMPER_OHMS {
        // A positive but sub-milliohm value (a 15 µΩ shunt, a 1e-9 typo) stamps
        // 1e4..1e9 S into the matrix. Past ~1e4 S the elimination loses the
        // gmin-sized pivots of the diode-off nodes around it (the cancellation
        // error is eps times the largest conductance, so 2e4 S already costs
        // 2e-12 S, more than gmin), the LU reports a singular pivot and the
        // whole board's DC operating point fails to solve while a literal 0
        // (bound at a milliohm above) solves. Bind it at the same milliohm
        // and say so, as for the 0 ohm link: the drop across it is the current
        // times a milliohm, far below the rails a board is judged against.
        let note = format!(
            "{name}: value '{}' ({}) is below the {} conditioning floor, bound as a {} link so the solve stays finite (a stiffer conductance would lose the matrix's small pivots)",
            comp.value,
            format_engineering_web(p.si, "ohm"),
            format_engineering_web(ZERO_OHM_JUMPER_OHMS, "ohm"),
            format_engineering_web(ZERO_OHM_JUMPER_OHMS, "ohm"),
        );
        (
            Device::resistor(name, a, b, ZERO_OHM_JUMPER_OHMS),
            Some(note),
        )
    } else {
        (Device::resistor(name, a, b, p.si), None)
    }
}

/// Bind a multi-pad passive array (resistor network / capacitor array) as one
/// device PER ELEMENT, never as a single 2-terminal element.
///
/// Pad conventions:
///   - EVEN pad count -> isolated array: pads paired across the package
///     (1-N, 2-(N-1), …, the chip-array and DIP convention), or neighbours
///     (1-2, 3-4, …) for a SIP footprint; each pair carries one element at the
///     pack's per-element value (an array's value is per element).
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
        // Isolated arrays. Chip arrays (convex/concave 2x/4x, KiCad R_Pack,
        // Eagle RESPACK) and DIP/SO networks pair pads across the package,
        // 1-N, 2-(N-1), …; only a SIP isolated network pairs neighbours.
        let sip = comp.footprint.to_ascii_uppercase().contains("SIP");
        let n = pads.len();
        for i in 0..n / 2 {
            let (x, y) = if sip {
                (2 * i, 2 * i + 1)
            } else {
                (i, n - 1 - i)
            };
            elements.push((
                format!("{}_e{}", comp.reference, i + 1),
                node_of(pads[x].net),
                node_of(pads[y].net),
            ));
        }
        // The pairing is read from the footprint family, not the netlist, so
        // the report says which convention was bound.
        notes.push(format!(
            "{}: {n}-pad passive array bound as isolated pairs {} by the {} \
             convention; verify against the part's datasheet",
            comp.reference,
            if sip {
                "1-2, 3-4, …"
            } else {
                "1-N, 2-(N-1), …"
            },
            if sip { "SIP" } else { "chip-array/DIP" },
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
        entry_warning(comp, model),
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
        // Base-resistance modulation (SPICE IRB/RBM). A zero IRB is SPICE's
        // "not stated", as with the knees; RBM defaults to RB.
        irb: knee(p.get_f64("irb"), d.irb),
        rbm: p.get_f64("rbm"),
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

/// The fitted capacitors spanning a regulator's `out` and `gnd` nodes: those
/// whose value reads as a capacitance with it, then the references of those
/// whose size is unknown (never counted as 0 F). Unitless values count only
/// below 10 mF: a bare "4.7" or "10k" names no known unit.
fn caps_across_output<'c>(
    (out, gnd): (NodeId, NodeId),
    components: &'c [Component],
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> (Vec<(&'c str, f64)>, Vec<&'c str>) {
    let across_output = |cap: &Component| {
        let pins = logical_passive_pins(cap).ok()?;
        let nodes: Vec<_> = pins.iter().filter_map(|pin| node_of(pin.net)).collect();
        let fitted = AssemblyState::of(cap).is_present() && passive_letter(cap) == Some('C');
        let spans =
            matches!(*nodes.as_slice(), [a, b] if (a, b) == (out, gnd) || (a, b) == (gnd, out));
        let farads = parse_value(cap.value.trim())
            .filter(|v| v.unit.as_deref().map_or(v.si < 1e-2, |u| u == "F"))
            .map(|v| v.si)
            .filter(|f| f.is_finite() && *f > 0.0);
        (fitted && spans).then_some(farads)
    };
    let (mut caps, mut unvalued) = (Vec::new(), Vec::new());
    for cap in components {
        match across_output(cap) {
            Some(Some(farads)) => caps.push((cap.reference.as_str(), farads)),
            Some(None) => unvalued.push(cap.reference.as_str()),
            None => {}
        }
    }
    (caps, unvalued)
}

/// The control loop of a linear regulator is not simulated, so its datasheet
/// output-capacitor requirement is either checked against the board or stated
/// as a gap; ESR is never guessed because the board file does not carry it.
fn vreg_stability_warning(
    comp: &Component,
    model: &ModelEntry,
    nodes: (NodeId, NodeId),
    components: &[Component],
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> Option<String> {
    let (caps, unvalued) = caps_across_output(nodes, components, node_of);
    let found = caps.iter().map(|(_, farads)| farads).sum::<f64>();
    let refs: Vec<_> = caps.iter().map(|(reference, _)| *reference).collect();
    let counted = if refs.is_empty() {
        "none".to_owned()
    } else {
        refs.join(", ")
    };
    let esr = |name| model.params.get_f64(name);
    // A part-per-billion slack: "10uF" parses to 10 x 1e-6, one ulp short of
    // the card's 10e-6, and a capacitor of exactly the stated value meets it.
    let short = |need: f64| found < need * (1.0 - 1e-9);
    let recommended = model.params.get_f64("cout_recommended_f");
    let (marker, gap) = match model.params.get_f64("cout_min_f") {
        None if recommended.is_none() => (
            "",
            "control-loop stability is not simulated and this card does not state the \
             output-capacitor requirement; add `cout_min_f` (a stated minimum) or \
             `cout_recommended_f` (a recommended value) and, when required, \
             `cout_esr_min_ohm` plus `cout_esr_max_ohm` to the card"
                .to_owned(),
        ),
        // Capacitors whose size is unknown may make up the shortfall, so the
        // requirement is stated as not judged rather than violated.
        Some(required) if short(required) && !unvalued.is_empty() => (
            "",
            format!(
                "{} requires at least {} output capacitance for stability; {} was counted \
                 directly from output to ground (counted: {counted}), and {} across the output \
                 {} no readable capacitance value, so the requirement is not judged",
                comp.reference,
                format_engineering_web(required, "F"),
                format_engineering_web(found, "F"),
                unvalued.join(", "),
                if unvalued.len() == 1 { "has" } else { "have" }
            ),
        ),
        Some(required) if short(required) => (
            Assumption::OPERATING_LIMIT_MARKER,
            format!(
                "{} requires at least {} output capacitance for stability, but {} was found \
                 directly from output to ground (counted: {counted})",
                comp.reference,
                format_engineering_web(required, "F"),
                format_engineering_web(found, "F")
            ),
        ),
        // A recommended (sufficient, not minimum) value: a shortfall is
        // disclosed as unconfirmed stability, never as a violated limit.
        _ if recommended.is_some_and(short) => (
            "",
            format!(
                "the datasheet recommends {} output capacitance for stability (a recommended \
                 value, not a stated minimum), and {} was found directly from output to ground \
                 (counted: {counted}{}); stability below the recommendation is not confirmed",
                format_engineering_web(recommended?, "F"),
                format_engineering_web(found, "F"),
                if unvalued.is_empty() {
                    String::new()
                } else {
                    format!("; no readable value: {}", unvalued.join(", "))
                }
            ),
        ),
        _ if esr("cout_esr_min_ohm").is_some() => (
            "",
            format!(
                "control-loop stability requires output-capacitor ESR in {}..{}, but the board \
                 file does not carry capacitor ESR and it is not guessed from value or footprint \
                 (counted: {counted})",
                format_engineering_web(esr("cout_esr_min_ohm")?, "Ω"),
                format_engineering_web(esr("cout_esr_max_ohm")?, "Ω")
            ),
        ),
        _ => return None,
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
        // A feedback-programmed converter's rail is the board's divider; with
        // the pin unwired or nothing on it to set it, there is no rail to report.
        if let Some(feedback) = &conv.feedback {
            let Some(fb) = roles.get(&feedback.pin).copied() else {
                return open_warning(comp, FEEDBACK_UNWIRED);
            };
            if feedback_unset(comp, (fb, out), components, node_of) {
                return open_warning(comp, FEEDBACK_UNSET);
            }
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
                device: format!(
                    "vreg {} behavioral converter",
                    format_volts(conv.vout_setpoint)
                ),
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
    // A linear charger is a CC/CV source, not an ideal one.
    if let Some(program) = linear_charger(model) {
        let pins = ChargerPins {
            input,
            out,
            gnd,
            prog: roles.get(&program.pin).copied(),
            vfloat: vout,
        };
        let stage = ChargerStage {
            port: false,
            headroom_v: model.params.get_f64("dropout_v").unwrap_or(0.0),
            iq_a: model.params.get_f64("iq_a"),
            cap_a: None,
        };
        let stamped = stamp_charger(comp, program, (pins, stage), circuit, components, node_of);
        let stability = vreg_stability_warning(comp, model, (out, gnd), components, node_of);
        let caveats = [stamped.assumption, stability, entry_warning(comp, model)];
        return (
            BindOutcome::Behavioral {
                device: stamped.label,
            },
            join_caveats(comp, caveats),
        );
    }
    // The enable pin switches the rail (see `vreg_enable`): the source returns
    // to a private node the gate closes to GND.
    let mut enable = crate::vreg_enable::Plan::new(
        comp,
        model,
        roles,
        (components, circuit),
        node_of,
        (input, gnd),
    );
    let ret = enable.source_return(circuit, &comp.reference, gnd);
    let source_id = circuit.add(Device::vsource(
        format!("Vreg_{}", comp.reference),
        out,
        ret,
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
    // Iq leaves the part's GND pin and returns through IN for a negative
    // regulator (its input is the negative rail), so it is drawn from that rail
    // too: the card states it as a magnitude, the polarity follows `vout`.
    // The enable pin picks between that and the card's shutdown current.
    if let Some(amps) = model.params.get_f64("iq_a").filter(|i| i.is_finite()) {
        helpers.extend(enable.quiescent(circuit, comp, model, (input, gnd), amps.copysign(vout)));
    }
    let (gate, gated, [enable_caveat, held_off]) =
        enable.stamp(circuit, comp, (input, out, gnd, ret), Some(vout));
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
        enable: enable_nodes(roles),
        gate,
    });
    // SEVERAL caveats can apply to one regulator and there is one channel, so they
    // are joined rather than one silently winning. Each is a full sentence already;
    // the leading one keeps the marker that makes the whole string evidence, and the
    // followers drop their own "REF (VALUE): " so the reference is not repeated.
    // A power-path part's charge port, beside the system rail just stamped.
    let (port_label, port_caveat) = bind_charge_port(
        comp,
        model,
        (input, gnd),
        roles,
        circuit,
        components,
        node_of,
    );
    let caveats = [
        vreg_stability_warning(comp, model, (out, gnd), components, node_of),
        entry_warning(comp, model),
        enable_caveat,
        port_caveat,
    ];
    let source = gated.map_or_else(|| "source".to_string(), |g| format!("source, {g}"));
    let device = port_label.map_or_else(
        || format!("vreg {} {source}", format_volts(vout)),
        |port| format!("vreg {} {source}; {port}", format_volts(vout)),
    );
    // A held-off rail is a finding about the board, not a gap in the model, so
    // it rides after the caveats rather than taking their marker.
    (
        BindOutcome::Behavioral { device },
        merge_bind_warnings(join_caveats(comp, caveats), held_off),
    )
}

/// The card's programming block and charge-port role when the part is a
/// POWER-PATH charger: a regulated current that flows in at `in` and out at
/// one role other than `out` (a `bat` port), while `out` is a system rail that
/// [`bind_vreg`] stamps as an ordinary regulator. [`linear_charger`] is the
/// other shape, the programmed current leaving at `out` itself.
fn charge_port(model: &ModelEntry) -> Option<(&CurrentProgram, &str)> {
    let program = model.current_program.as_ref().filter(|p| {
        p.semantics == CurrentProgramSemantics::RegulatedCurrent
            && p.current_in_roles.iter().any(|r| r == "in")
    })?;
    match program.current_out_roles.as_slice() {
        [port] if port != "out" => Some((program, port.as_str())),
        _ => None,
    }
}

/// A power-path part's charge port: the CC/CV stamp of [`stamp_charger`] from
/// IN to the port, floating at the card's `vbat_reg_v`, at the programmed
/// current capped by `input_limit_a`, behind the `dropout_v` of the input path
/// plus the `bat_dropout_v` of the port's own pass element. It draws from IN
/// directly, so IN carries the system load plus the charge current, as the
/// part's input does. Returns the stamp's label and its caveat.
///
/// Not a port at all (no charge program, an unwired port) returns nothing. A
/// port whose float voltage the card does not set for this board (its strap
/// table selected no float, or the card has none) is not stamped, and that is
/// said unless the strap note already said why.
fn bind_charge_port(
    comp: &Component,
    model: &ModelEntry,
    (input, gnd): (NodeId, NodeId),
    roles: &HashMap<String, NodeId>,
    circuit: &mut Circuit,
    components: &[Component],
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> (Option<String>, Option<String>) {
    let Some((program, port_role)) = charge_port(model) else {
        return (None, None);
    };
    let Some(port) = roles
        .get(port_role)
        .copied()
        .filter(|n| *n != input && *n != gnd)
    else {
        return (None, None);
    };
    let Some(vfloat) = model
        .params
        .get_f64("vbat_reg_v")
        .filter(|v| v.is_finite() && *v > 0.0)
    else {
        let strap_said = model
            .resistor_bins
            .iter()
            .any(|t| t.bins.iter().any(|b| b.params.contains_key("vbat_reg_v")));
        let caveat = (!strap_said).then(|| {
            format!(
                "{}{} ({}): its `{port_role}` charge port is not stamped because the card \
                 sets no charge regulation voltage (`vbat_reg_v`), so no charge current is \
                 modelled and IN carries the system rail's draw only",
                Assumption::PARTIAL_MODEL_MARKER,
                comp.reference,
                comp.value
            )
        });
        return (None, caveat);
    };
    let headroom = ["dropout_v", "bat_dropout_v"]
        .iter()
        .filter_map(|k| {
            model
                .params
                .get_f64(k)
                .filter(|d| d.is_finite() && *d > 0.0)
        })
        .sum();
    let pins = ChargerPins {
        input,
        out: port,
        gnd,
        prog: roles.get(&program.pin).copied(),
        vfloat,
    };
    let stage = ChargerStage {
        port: true,
        headroom_v: headroom,
        iq_a: None,
        cap_a: model.params.get_f64("input_limit_a"),
    };
    let stamped = stamp_charger(comp, program, (pins, stage), circuit, components, node_of);
    (Some(stamped.label), stamped.assumption)
}

/// Several caveats can apply to one part and there is one channel, so they are
/// joined rather than one silently winning. The follower's own "REF (VALUE): "
/// comes off so the reference is not said twice in one sentence. The KNOWN
/// prefix is stripped rather than cutting at the first ": ", which a component
/// value containing a colon would break.
fn join_caveats(
    comp: &Component,
    caveats: impl IntoIterator<Item = Option<String>>,
) -> Option<String> {
    let prefix = format!("{} ({}): ", comp.reference, comp.value);
    caveats.into_iter().flatten().reduce(|lead, c| {
        let c = c
            .strip_prefix(Assumption::PARTIAL_MODEL_MARKER)
            .unwrap_or(&c);
        format!(
            "{lead}; also {}",
            c.strip_prefix(prefix.as_str()).unwrap_or(c)
        )
    })
}

/// The supply voltage over which a part's quiescent draw fades in.
const SUPPLY_FADE_V: f64 = 1.0;

/// A linear regulator's quiescent supply current, `hi -> lo`, as a draw that needs a
/// supply to draw from: `amps` once `hi` stands [`SUPPLY_FADE_V`] above `lo`,
/// none with no supply, C1 in between. A negative `amps` is a negative rail,
/// read the other way round.
///
/// A plain DC current source here pulls a supply pin nothing feeds to
/// `amps / gmin`, billions of volts, and the solve is refused. Wherever a
/// supply is present the draw is the card's figure, unchanged.
///
/// Used for regulators only. Op-amps and comparators carry their draw on
/// their own device instead (`Supply::quiescent_draw`), which adds no device
/// and no matrix slot per part.
pub(crate) fn supply_draw(
    circuit: &mut Circuit,
    name: String,
    (hi, lo): (NodeId, NodeId),
    amps: f64,
) -> DeviceId {
    let sign = if amps < 0.0 { -1.0 } else { 1.0 };
    let x = format!("min(max({sign:?}*(__d0-__d1)/{SUPPLY_FADE_V:?},0.0),1.0)");
    circuit.add(Device::Behavioral {
        name,
        p: hi,
        n: lo,
        output: BOutput::Current,
        expr: CompiledExpr::compile(&format!("{amps:?}*{x}*{x}*(3.0-2.0*{x})"))
            .expect("supply draw expression compiles"),
        deps: vec![BDep::Volt(hi), BDep::Volt(lo)],
    })
}

/// The non-ground board nodes on a regulator's `en` / `shdn` pins (see
/// `SimpleRegulator::enable`).
fn enable_nodes(roles: &HashMap<String, NodeId>) -> Vec<NodeId> {
    ["en", "shdn"]
        .iter()
        .filter_map(|r| roles.get(*r).copied())
        .filter(|n| !n.is_ground())
        .collect()
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

/// A charger's constant-voltage output droops by this many volts as its output
/// current rises from zero to the programmed limit, so its output impedance in
/// regulation is `CV_BAND_V / I_limit` (about 25 mOhm for a 400 mA part).
const CV_BAND_V: f64 = 0.01;

/// What a charger's output looks like to a cell above its float voltage: the
/// pass element blocks reverse current, leaving microamps through this much
/// resistance.
const REVERSE_OHMS: f64 = 1e6;

/// The pin nodes a charger stamp needs.
#[derive(Clone, Copy)]
struct ChargerPins {
    input: NodeId,
    out: NodeId,
    gnd: NodeId,
    /// The node on the part's programming pin, when it is wired.
    prog: Option<NodeId>,
    /// The float voltage: the card's `vout`, or a power-path part's
    /// `vbat_reg_v`.
    vfloat: f64,
}

/// Which charger a stamp is and what it carries beyond its pins.
#[derive(Clone, Copy)]
struct ChargerStage {
    /// The charge port of a power-path part, beside its `Vreg_` system rail:
    /// its devices take a `_port` suffix and its ammeter is named by
    /// [`PORT_AMMETER_PREFIX`] plus the reference, so neither collides with
    /// the rail's.
    port: bool,
    /// The input headroom the pass path needs above the float.
    headroom_v: f64,
    /// Quiescent draw on IN, when this stage carries the part's.
    iq_a: Option<f64>,
    /// A ceiling on the programmed current from another of the card's
    /// settings (a power-path part's input-current limit).
    cap_a: Option<f64>,
}

/// The ammeter of a power-path part's charge port.
const PORT_AMMETER_PREFIX: &str = "Vport_";

/// What a charger stamp tells the bind report.
struct StampedCharger {
    label: String,
    /// Said when the charge-current limit is not read from the board.
    assumption: Option<String>,
}

/// The card's programming block when the part is a linear charger: a current
/// regulated by a resistor, flowing in at one set of roles and out at another.
/// A protection-limit block (a converter's input limit) is not a charger, and
/// neither is a power-path part whose programmed current leaves at another
/// port (a `bat` role) than the `out` this stamp drives: its `out` is a system
/// rail that carries more than the charge current, so limiting it to
/// `I_prog` would be wrong.
fn linear_charger(model: &ModelEntry) -> Option<&CurrentProgram> {
    let has = |roles: &[String], role: &str| roles.iter().any(|r| r == role);
    model.current_program.as_ref().filter(|p| {
        p.semantics == CurrentProgramSemantics::RegulatedCurrent
            && has(&p.current_in_roles, "in")
            && has(&p.current_out_roles, "out")
    })
}

/// A charger's current limit and, when it is not read from the board, the
/// assumption for the bind report: the PROG network's resistance through the
/// card's equation, else the card's operating ceiling.
fn charge_limit(
    comp: &Component,
    program: &CurrentProgram,
    prog: Option<NodeId>,
    components: &[Component],
    is_ground: &dyn Fn(i64) -> Option<bool>,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> (f64, Option<String>) {
    let reference = &comp.reference;
    let read = prog
        .and_then(|node| comp.pins.iter().find(|p| node_of(p.net) == Some(node))?.net)
        .and_then(|net| {
            let excluded = components.iter().position(|c| c.reference == *reference);
            program_resistance_to_ground(components, is_ground, net, 4, excluded)
        });
    let ceiling = program.max_operating_current_a;
    let (amps, assumed) = match read {
        Some((ohms, path)) => {
            let amps = program.operating_current_a(ohms);
            let why = amps.is_none().then(|| {
                format!(
                    "the {} ohm programming network {} asks for a current outside this \
                     card's sourced operating domain",
                    // A sub-ohm network keeps its significant figures, never "0".
                    if ohms >= 0.9995 {
                        format!("{ohms:.0}")
                    } else {
                        format!("{ohms:.3e}")
                    },
                    path.join("+")
                )
            });
            (amps.or(ceiling), why)
        }
        None => (
            ceiling,
            Some(format!(
                "its `{}` programming resistor could not be read off the board (no populated \
                 resistor path to ground, or a part on the net that is not a plain resistor)",
                program.pin
            )),
        ),
    };
    let ilim = amps.unwrap_or(0.0).max(1e-6);
    let assumption = assumed.map(|why| {
        format!(
            "{}{reference} ({}): {why}, so the charge-current limit is the card's {} \
             normal-operating ceiling, not a value read from this board; the charge current \
             and the input current it draws are upper bounds",
            Assumption::PARTIAL_MODEL_MARKER,
            comp.value,
            format_amps(ilim)
        )
    });
    (ilim, assumption)
}

/// A linear Li-ion charger's CC/CV output, from primitives the IR already has.
///
/// The ideal-source stamp of a plain regulator is wrong for a charger: it has
/// no current limit, so a battery on the output is a fight between two voltage
/// sources and the charge current is whatever the solver splits. The real part
/// is a pass element that forces `I_prog` (set by the board's PROG resistor)
/// into the cell until the cell reaches the float voltage, then tapers the
/// current to hold it there. The stamp is that I-V curve as a Thevenin source,
/// which Newton solves in one pass whatever the load (a flat current-source
/// plateau does not: from a cold start it overshoots to `I_prog * R_load` and
/// back):
///
/// - `Vchg_<ref>` holds `set` at `Vfloat`, rounded down over [`PASS_FADE_V`]
///   to `V(in) - dropout` when the input cannot pass it, so a collapsed or
///   unpowered input delivers nothing (the LDO path's headroom, on the output
///   rather than the draw);
/// - `Ichg_<ref>`, between `set` and the ammeter's node, drops `R_cv * I` up to
///   `I_prog` (`R_cv = CV_BAND_V / I_prog`), then `R_cc * (I - I_prog)` with a
///   large `R_cc`, which holds the current at the limit into any load, and
///   [`REVERSE_OHMS`] for current back into the part;
/// - `Vreg_<ref>` is a 0 V ammeter from there to OUT, the seam the stress
///   monitor and the draw below read;
/// - `Bchg_<ref>` draws the output current out of IN, read back off the drop
///   (the inverse of the expression above) so an output capacitor's `C*dV/dt`
///   never reaches IN (see [`pass_draw`]), and the card's `iq_a` is added.
///
/// `rail_owner` knows `Ichg_` as a current-limited feed, so a battery declared
/// on OUT is the charger's load, not a rival source.
///
/// `I_prog` is [`CurrentProgram::operating_current_a`] of the PROG network's
/// resistance, read by [`program_resistance_to_ground`] (the same reading the
/// ampacity check cites). When the network cannot be read, or the equation
/// declines that resistance, the limit is the card's normal-operating ceiling
/// and the assumption is returned for the bind report; no value is invented.
fn stamp_charger(
    comp: &Component,
    program: &CurrentProgram,
    (pins, stage): (ChargerPins, ChargerStage),
    circuit: &mut Circuit,
    components: &[Component],
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> StampedCharger {
    let ChargerPins {
        input,
        out,
        gnd,
        prog,
        vfloat,
    } = pins;
    let reference = &comp.reference;
    let (programmed, assumption) = charge_limit(
        comp,
        program,
        prog,
        components,
        &|id| node_of(Some(id)).map(|n| n.is_ground() || is_ground_net(circuit.node_name(n))),
        node_of,
    );
    let ilim = stage
        .cap_a
        .filter(|c| c.is_finite() && *c > 0.0)
        .map_or(programmed, |c| programmed.min(c));
    let full = Some(stage.headroom_v)
        .filter(|d| d.is_finite() && *d > 0.0)
        .unwrap_or(0.0);
    let (sfx, node_tag, ammeter_name) = if stage.port {
        ("_port", "port", format!("{PORT_AMMETER_PREFIX}{reference}"))
    } else {
        ("", "chg", format!("Vreg_{reference}"))
    };
    let (set, cc) = (
        circuit.node(&format!("{reference}_{node_tag}_set")),
        circuit.node(&format!("{reference}_{node_tag}_out")),
    );
    // The set voltage: the float, held down to what the input can pass.
    // `m` is `min(u, 0)` rounded over `PASS_FADE_V`, `u` the input's headroom
    // above `Vfloat + dropout`.
    let w = PASS_FADE_V;
    let u = format!("((__d0-__d1)-{:?})", vfloat + full);
    let m = format!(
        "if({u}>=0.0,0.0,if({u}>=-{w:?},-{u}*{u}/{:?},{u}+{:?}))",
        2.0 * w,
        w / 2.0
    );
    circuit.add(Device::Behavioral {
        name: format!("Vchg_{reference}{sfx}"),
        p: set,
        n: gnd,
        output: BOutput::Voltage,
        expr: CompiledExpr::compile(&format!("{vfloat:?}+{m}"))
            .expect("charger set voltage compiles"),
        deps: vec![BDep::Volt(input), BDep::Volt(gnd)],
    });
    // The output ammeter: a 0 V source whose branch reads `-Iout`, the seam the
    // stress monitor and the input draw below both read.
    let ammeter = circuit.add(Device::vsource(ammeter_name, out, cc, SourceKind::Dc(0.0)));
    let (droop_ohms, hold_ohms) = (CV_BAND_V / ilim, 1e4 * vfloat / ilim);
    let i_out = "(-__d0)";
    let drop = format!(
        "{REVERSE_OHMS:?}*min({i_out},0.0)+{droop_ohms:?}*max(min({i_out},{ilim:?}),0.0)\
         +{hold_ohms:?}*max({i_out}-{ilim:?},0.0)"
    );
    circuit.add(Device::Behavioral {
        name: format!(
            "{}{reference}{sfx}",
            crate::rail_owner::LIMITED_SOURCE_PREFIX
        ),
        p: set,
        n: cc,
        output: BOutput::Voltage,
        expr: CompiledExpr::compile(&drop).expect("charger drop compiles"),
        deps: vec![BDep::Branch(ammeter)],
    });
    // IN draws what the output delivers, read back off the drop it makes (the
    // inverse of the expression above) rather than off the ammeter's branch, so
    // an output capacitor's C*dV/dt never reaches IN (see [`pass_draw`]).
    let delta = "(__d0-__d1)";
    let draw = format!(
        "min(max({delta},0.0)/{droop_ohms:?},{ilim:?})+max({delta}-{:?},0.0)/{hold_ohms:?}",
        droop_ohms * ilim
    );
    circuit.add(Device::Behavioral {
        name: format!("{CHARGER_DRAW_PREFIX}{reference}{sfx}"),
        p: input,
        n: gnd,
        output: BOutput::Current,
        expr: CompiledExpr::compile(&draw).expect("charger draw compiles"),
        deps: vec![BDep::Volt(set), BDep::Volt(cc)],
    });
    // The quiescent draw needs a supply to draw from: a plain current source
    // on an input nothing feeds (a USB rail with no cable) pulls that rail
    // below ground through whatever resistor holds it.
    if let Some(amps) = stage.iq_a.filter(|i| i.is_finite()) {
        supply_draw(circuit, format!("Iq_{reference}"), (input, gnd), amps);
    }
    let (vf, il) = (format_volts(vfloat), format_amps(ilim));
    let label = if stage.port {
        let capped = if ilim < programmed {
            " (the input-current limit)"
        } else {
            ""
        };
        format!("charge port {vf} CC/CV, {il} limit{capped}")
    } else {
        format!("linear charger {vf} CC/CV, {il} limit")
    };
    StampedCharger { label, assumption }
}

/// The name prefix of a charger's input draw, the copy of its output current
/// taken out of IN.
const CHARGER_DRAW_PREFIX: &str = "Bchg_";

/// Each charger's ammeter and input node, for the dissipation watch: the
/// stress monitor reads `(V(in)*Iin - V(out)*Iout)` off them like a linear
/// regulator's.
fn charger_heat(circuit: &Circuit) -> Vec<(DeviceId, NodeId)> {
    circuit
        .iter()
        .filter_map(|(_, d)| match d {
            Device::Behavioral { name, p, .. } => {
                let reference = name.strip_prefix(CHARGER_DRAW_PREFIX)?;
                let ammeter = circuit.iter().find(|(_, a)| {
                    matches!(a, Device::Vsource { name, .. } if name.strip_prefix("Vreg_") == Some(reference))
                })?;
                Some((ammeter.0, *p))
            }
            _ => None,
        })
        .collect()
}

/// Each power-path part's system-rail source, the input node both of its
/// paths draw from, and its charge-port ammeter: the package dissipates
/// `(V(in) - V(sys)) * I_sys + (V(in) - V(bat)) * I_bat`, the datasheet's
/// `(VIN - VSYS) * IIN + (VSYS - VBAT) * IBAT` with `IIN = ISYS + IBAT`.
fn port_heat(circuit: &Circuit) -> Vec<(DeviceId, NodeId, DeviceId)> {
    let vsource = |want: &str| {
        circuit
            .iter()
            .find(|(_, d)| matches!(d, Device::Vsource { name, .. } if name == want))
            .map(|(id, _)| id)
    };
    circuit
        .iter()
        .filter_map(|(port, d)| {
            let Device::Vsource { name, .. } = d else {
                return None;
            };
            let reference = name.strip_prefix(PORT_AMMETER_PREFIX)?;
            let rail = vsource(&format!("Vreg_{reference}"))?;
            let draw = format!("{CHARGER_DRAW_PREFIX}{reference}_port");
            let input = circuit.iter().find_map(|(_, b)| match b {
                Device::Behavioral { name, p, .. } if *name == draw => Some(*p),
                _ => None,
            })?;
            Some((rail, input, port))
        })
        .collect()
}

/// Why an adjustable regulator is left open when its ADJ/FB pin is unwired.
const FEEDBACK_UNWIRED: &str = "adjustable vreg ADJ/FB pin not connected, so nothing sets its \
     output voltage";
/// Why one is left open when nothing on the board sets its ADJ/FB pin.
const FEEDBACK_UNSET: &str = "adjustable vreg ADJ/FB pin has no divider on the board (nothing but \
     capacitors on its net), so its output voltage cannot be determined; check the divider \
     resistors were extracted, or use a fixed-output model";

/// True when nothing on the board sets the adjustable regulator `comp`'s
/// ADJ/FB node `fb`: it is not the output (unity feedback) or ground, and no
/// fitted part other than capacitors, open links and connectors touches it.
/// The MNA solves whatever divider is there, so any resistor, link or driver
/// counts; a bypass capacitor alone sets no DC level, and stamping one would
/// invent the rail.
fn feedback_unset(
    comp: &Component,
    (fb, out): (NodeId, NodeId),
    components: &[Component],
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> bool {
    use crate::program_resistor::{classify_series, Series};
    if fb == out || fb.is_ground() {
        return false;
    }
    !components.iter().any(|c| {
        c.reference != comp.reference
            && !matches!(AssemblyState::of(c), AssemblyState::DnpAbsent(_))
            && !is_port_class(&ref_prefix(&c.reference))
            && !matches!(classify_series(c), Series::Open)
            && c.pins.iter().any(|p| node_of(p.net) == Some(fb))
    })
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
/// A grounded part's `en`/`shdn` pin gates the VCVS's return to GND, the way
/// `bind_vreg` gates its source's (see `vreg_enable`).
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
        return open_warning(comp, FEEDBACK_UNWIRED);
    };
    if feedback_unset(comp, (adj, out), components, node_of) {
        return open_warning(comp, FEEDBACK_UNSET);
    }
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
    // A grounded part's enable gates the VCVS's return, as `bind_vreg` gates
    // its source's (see `vreg_enable`). An LM317-class part has no return to
    // gate: an enable on such a card stays unmodelled, and says so.
    let mut enable = grounded.then(|| {
        crate::vreg_enable::Plan::new(
            comp,
            model,
            roles,
            (components, circuit),
            node_of,
            (input, gnd),
        )
    });
    let gate_ret = enable
        .as_ref()
        .map_or(gnd, |e| e.source_return(circuit, &comp.reference, gnd));
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
            n: gate_ret,
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
    helpers.extend(adj_supply_draws(
        circuit,
        (comp, model),
        enable.as_mut(),
        ((input, adj, gnd), vref),
    ));
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
        enable: enable_nodes(roles),
        gate: None,
    });
    let nodes = (input, out, gnd, gate_ret);
    let (gated, [enable_caveat, held_off]) =
        gate_last_regulator(enable, (circuit, regulators), comp, model, nodes);
    let caveats = [
        vreg_stability_warning(comp, model, (out, gnd), components, node_of),
        entry_warning(comp, model),
        enable_caveat,
    ];
    (
        BindOutcome::Analog {
            device: format!("vreg {} adjustable reference{gated}", format_volts(vref)),
        },
        merge_bind_warnings(join_caveats(comp, caveats), held_off),
    )
}

/// An adjustable regulator's supply draws. IADJ leaves the ADJ pin; a grounded
/// part's quiescent current runs IN -> GND, through its enable's `plan` (a
/// grounded part always has one). Both are magnitudes on the card; a negative
/// regulator (`vref` < 0) draws them the other way, as `bind_vreg`'s Iq does.
fn adj_supply_draws(
    circuit: &mut Circuit,
    (comp, model): (&Component, &ModelEntry),
    plan: Option<&mut crate::vreg_enable::Plan>,
    ((input, adj, gnd), vref): ((NodeId, NodeId, NodeId), f64),
) -> Vec<DeviceId> {
    let amps = |key| {
        model
            .params
            .get_f64(key)
            .filter(|i| i.is_finite())
            .map(|i| i.copysign(vref))
    };
    let mut draws = Vec::new();
    if let Some(amps) = amps("iadj_a") {
        let name = format!("Iadj_{}", comp.reference);
        draws.push(circuit.add(Device::isource(name, input, adj, SourceKind::Dc(amps))));
    }
    if let (Some(amps), Some(plan)) = (amps("iq_a"), plan) {
        draws.extend(plan.quiescent(circuit, comp, model, (input, gnd), amps));
    }
    draws
}

/// Stamp `plan` for the regulator just pushed onto `regulators` and record
/// its gate there. Gives the bind row's label suffix and the enable notes; a
/// part with no plan gets the not-modelled disclosure when its card names a pin.
fn gate_last_regulator(
    plan: Option<crate::vreg_enable::Plan>,
    (circuit, regulators): (&mut Circuit, &mut [SimpleRegulator]),
    comp: &Component,
    model: &ModelEntry,
    nodes: (NodeId, NodeId, NodeId, NodeId),
) -> (String, [Option<String>; 2]) {
    let (gate, gated, enable_notes) = plan.map_or_else(
        || (None, None, [ungated_enable_caveat(comp, model), None]),
        |plan| plan.stamp(circuit, comp, nodes, None),
    );
    if let Some(reg) = regulators.last_mut() {
        reg.gate = gate;
    }
    (
        gated.map_or_else(String::new, |g| format!(", {g}")),
        enable_notes,
    )
}

/// The disclosure for an enable pin no stamp reads: the card names one, but the
/// part's shape leaves the gate nothing to switch (an LM317-class adjustable
/// with no return of its own), so the rail stays unconditional.
fn ungated_enable_caveat(comp: &Component, model: &ModelEntry) -> Option<String> {
    let role = ["en", "shdn"]
        .into_iter()
        .find(|r| model.pins.values().any(|v| v == r))?;
    Some(format!(
        "{}{} ({}): its {} pin is not modelled; the output rail is stamped as an \
         unconditional source, so a board that holds this regulator OFF still reads as \
         having this rail present",
        Assumption::PARTIAL_MODEL_MARKER,
        comp.reference,
        comp.value,
        role.to_ascii_uppercase()
    ))
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
        supply: hauksbee_ir::Supply::default(),
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
            device: format!(
                "shunt reference {} (the board's divider sets the cathode)",
                format_volts(vref)
            ),
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
    // The output stage's current is drawn from the part's supply pins, so a
    // load on the output shows up on the rail that feeds the part.
    let supply = stage_supply(roles);
    // The card's `iq_a` is the whole package's supply current with the outputs
    // unloaded (what datasheets print as ICC); a card without one draws none.
    // The first unit carries it, drawn only once V+ is up (see
    // `Supply::quiescent_draw`), so an unfed V+ is not dragged below ground.
    let mut quiescent_a = model
        .params
        .get_f64("iq_a")
        .filter(|i| i.is_finite())
        .unwrap_or(0.0);
    for (name, out, inp, inn) in units {
        let supply = hauksbee_ir::Supply {
            quiescent_a: std::mem::take(&mut quiescent_a),
            ..supply
        };
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
            supply,
        });
    }
    (BindOutcome::Behavioral { device }, warning)
}

/// An op-amp's or push-pull comparator's supply pins as the output stage
/// returns its current through them: `V+` (`vcc`, `vs`, `vdd`) and `V-` (`vss`, `vee`, `v_minus`, or the
/// `gnd` pin of a single-supply sense amplifier), each only when it sits on a
/// net. A ground `V-` is no pin to draw into.
fn stage_supply(roles: &HashMap<String, NodeId>) -> hauksbee_ir::Supply {
    let pin = |names: &[&str]| pick(roles, names).copied().filter(|n| !n.is_ground());
    hauksbee_ir::Supply {
        hi: pin(&["vcc", "vs", "vdd"]),
        lo: pin(&["vss", "vee", "v_minus", "gnd"]),
        quiescent_a: 0.0,
    }
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
///
/// `supply` carries only the package's quiescent draw (on one section): the
/// gate stage drives an internal node with no load, so the output current it
/// would mirror onto the supply pins is gmin-sized, and the draw rides on an
/// existing device instead of adding a behavioural source per package.
fn bind_open_collector(
    circuit: &mut Circuit,
    name: &str,
    (out, inp, inn, gnd): (NodeId, NodeId, NodeId, NodeId),
    model: &ModelEntry,
    supply: hauksbee_ir::Supply,
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
        supply,
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
    power_nets: &HashMap<String, f64>,
) -> (BindOutcome, Option<String>) {
    // A card's `swing_lo`/`swing_hi` (datasheet VOL/VOH as headroom from the
    // supply pins) put the output levels on the rail the part actually sits
    // at, so a 4.5 V supply gives a 4.45 V high, not the 5 V table's 4.95 V;
    // `out_lo`/`out_hi` remain where the supply net's voltage is unknown.
    let swing = |key: &str, fixed: &str, dflt: f64, sign: f64, pins: &[&str]| {
        let fallback = model.params.get_f64(fixed).unwrap_or(dflt);
        let Some(headroom) = model.params.get_f64(key) else {
            return fallback;
        };
        let rail = pick(roles, pins).and_then(|n| {
            if n.is_ground() {
                Some(0.0)
            } else {
                power_nets.get(circuit.node_name(*n)).copied()
            }
        });
        rail.map_or(fallback, |v| sign.mul_add(headroom, v))
    };
    let out_lo = swing("swing_lo", "out_lo", 0.0, 1.0, &["vss", "gnd", "vee"]);
    let out_hi = swing("swing_hi", "out_hi", 5.0, -1.0, &["vcc", "vs", "vdd"]);
    // Full-band VHYS (see `Device::Comparator`); validation requires it on checked cards.
    let hyst = model.params.get_f64("hysteresis").unwrap_or(0.01);
    // Propagation delay: the transient output follows each decision this
    // much later (see `Device::Comparator::tpd_s`). A card without one, or
    // with a nonsensical one, steps at the decision.
    let tpd_s = model
        .params
        .get_f64("tpd_s")
        .filter(|t| t.is_finite() && *t > 0.0)
        .unwrap_or(0.0);

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
    // A driven output's current is drawn from the part's supply pins, so a
    // load on it shows on the rail that feeds the part.
    let supply = stage_supply(roles);
    // Static supply draw (`supply_static_ua`, default 0 = nothing drawn): the
    // part's quiescent ICC, pulled from its supply pin net so a metering shunt
    // in series with that rail reads the comparator. Same param name as the
    // digital layer's static term. A push-pull package's first section carries
    // it, drawn only once VCC is up (see `Supply::quiescent_draw`), as does an
    // open-collector package's first gate stage. An unfed VCC is then not
    // dragged below ground.
    let mut quiescent_a = model
        .params
        .get_f64("supply_static_ua")
        .filter(|ua| ua.is_finite() && *ua > 0.0)
        .map_or(0.0, |ua| ua * 1e-6);
    for (name, out, inp, inn) in &sections {
        if open_collector {
            let draw = hauksbee_ir::Supply {
                quiescent_a: std::mem::take(&mut quiescent_a),
                ..supply
            };
            // Only the section carrying the draw takes the supply pins.
            let draw = if draw.quiescent_a > 0.0 {
                draw
            } else {
                hauksbee_ir::Supply::default()
            };
            bind_open_collector(circuit, name, (*out, *inp, *inn, gnd), model, draw);
        } else {
            circuit.add(Device::Comparator {
                name: name.clone(),
                out: *out,
                inp: *inp,
                inn: *inn,
                out_lo,
                out_hi,
                hysteresis: hyst,
                tpd_s,
                supply: hauksbee_ir::Supply {
                    quiescent_a: std::mem::take(&mut quiescent_a),
                    ..supply
                },
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

/// The switching point of an analog switch's control: the middle of the
/// card's `[models.levels]` VIL..VIH band read at the control's resolved supply
/// rail (CMOS control thresholds scale with VCC), else the card's flat `vth`.
/// The band itself is judged by the undefined-logic-level check, so a control
/// left between VIL and VIH is reported rather than trusted to this midpoint.
fn control_threshold(
    model: &ModelEntry,
    roles: &HashMap<String, NodeId>,
    circuit: &Circuit,
    power_nets: &HashMap<String, f64>,
) -> f64 {
    let flat = model.params.get_f64("vth").unwrap_or(1.5);
    let ctrl = ["ctrl", "ctrl_1", "sel", "s", "in"]
        .into_iter()
        .find(|r| roles.contains_key(*r));
    let rail = ctrl
        .and_then(|r| model.level_rail_of(r))
        .and_then(|r| roles.get(r))
        .and_then(|node| power_nets.get(circuit.node_name(*node)).copied());
    let tabled = model.levels.contains_key("vih") && model.levels.contains_key("vil");
    rail.filter(|_| tabled)
        .and_then(|v| model.input_band_at(v))
        .map_or(flat, |(vil, vih)| 0.5 * (vil + vih))
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
    let vth = control_threshold(model, roles, circuit, power_nets);
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
    // source onto the set-point nets.
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
        let tag = format!("{}_{role}", comp.reference);
        let mut drv = PinDriver::stamp(circuit, net, &net_name, &tag, rout);
        // The output current comes from the part's supply and returns into its
        // negative pin, so a load shows on the rail that feeds the part.
        if let Some(&hi) = pick(roles, &["vdd", "vcc"]) {
            let lo = pick(roles, &["vss", "gnd"]).copied();
            drv.route_supply(circuit, &tag, hi, lo.unwrap_or(NodeId::GROUND));
        }
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
        bus_nodes: bus_nodes_of(roles, &["sda", "scl"]),
    });
    (
        BindOutcome::Behavioral {
            device: format!("mcp4728 quad DAC ({stamped} VOUT)"),
        },
        None,
    )
}

/// Draw every output driver's current from its part's supply pins: the pin a
/// `[models.level_rails]` entry names for that role, else `vcc`/`vdd`, returning
/// into `gnd`/`vss`. A driver the part gives no positive supply stays as it was.
/// A sink-only (open-collector) card has nothing to draw from a positive pin: its
/// sunk current returns into `gnd`/`vss` alone.
fn route_driver_supplies(circuit: &mut Circuit, model: &ModelEntry, d: &mut DigitalComponent) {
    if model.params.get_bool("open_collector").unwrap_or(false) {
        let Some(lo) = pick(&d.roles, &["gnd", "vss"]).copied() else {
            return;
        };
        let mut roles: Vec<String> = d.drivers.keys().cloned().collect();
        roles.sort_unstable();
        for role in roles {
            let tag = format!("{}_{role}", d.reference);
            if let Some(drv) = d.drivers.get_mut(&role) {
                drv.route_return(circuit, &tag, lo);
            }
        }
        return;
    }
    let lo = pick(&d.roles, &["gnd", "vss"]).copied();
    let supply = pick(&d.roles, &["vcc", "vdd"]).copied();
    let mut roles: Vec<String> = d.drivers.keys().cloned().collect();
    roles.sort_unstable();
    for role in roles {
        let rail = (model.level_rails.iter())
            .find(|(_, pins)| pins.contains(&role))
            .and_then(|(rail, _)| d.roles.get(rail).copied());
        let tag = format!("{}_{role}", d.reference);
        if let (Some(hi), Some(drv)) = (rail.or(supply), d.drivers.get_mut(&role)) {
            drv.route_supply(circuit, &tag, hi, lo.unwrap_or(NodeId::GROUND));
        }
    }
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
    // A quad/dual NOR gate (74HC02) wired as cross-coupled SR pulse-capture latches: if
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
    // An open-collector or open-drain card's outputs only sink: they pull the
    // net to their low level and never source into a load below it.
    let sink_only = model.params.get_bool("open_collector").unwrap_or(false);
    let mut drivers = HashMap::new();
    for role in output_roles(model) {
        if let Some(&net) = roles.get(&role) {
            if net.is_ground() {
                continue;
            }
            let net_name = circuit.node_name(net).to_string();
            let tag = format!("{}_{role}", comp.reference);
            let drv = if sink_only {
                PinDriver::stamp_sink_only(circuit, net, &net_name, &tag, ro)
            } else {
                PinDriver::stamp(circuit, net, &net_name, &tag, ro)
            };
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
            route_driver_supplies(circuit, model, &mut d);
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

/// Detect and bind cross-coupled NOR SR latches on a 74HC02 (a pulse-capture
/// register). Each gate `g<n>` has output role `g<n>y` and input roles `g<n>a`,
/// `g<n>b`. A latch is a pair of gates (gQ, gQb) where `gQ.y == gQb.<one input>`
/// and `gQb.y == gQ.<one input>` (the cross-couple). For the latch:
///   - `reset` = the gate whose NON-cross-couple input net name contains "RESET", that gate's output is Qb (internal);
///   - the OTHER gate is the Q gate: its non-cross input is `set` (the pulse input),
///     its output net is `q` (the observable output, wired to a shift register input).
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
            let tag = format!("{}_latch{}_q", comp.reference, gates[q_gate].idx);
            let mut drv = PinDriver::stamp(circuit, q_net, &q_name, &tag, levels.ro);
            if let Some(&hi) = pick(roles, &["vcc", "vdd"]) {
                let lo = pick(roles, &["gnd", "vss"]).copied();
                drv.route_supply(circuit, &tag, hi, lo.unwrap_or(NodeId::GROUND));
            }
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

/// Supply roles an MCU or module can be powered through (chip or module pins).
const MCU_SUPPLY_ROLES: [&str; 8] = ["vddio", "iovdd", "vdd", "vcc", "dvdd", "5v", "3v3d", "3v3"];

/// Is `name` (a module header pin's own name) a supply input or rail pin?
/// Carrier modules label their power pins by rail, not by chip role: `3V3`,
/// `5V`, `VIN`, `VBUS`, `BAT+`, `RAW`. Any of these on a live net can power
/// the module, so a module carrying one is not unpowerable.
fn is_module_supply_pin_name(name: &str) -> bool {
    let upper = name.trim().trim_start_matches('+').to_ascii_uppercase();
    // "3V3", "3.3V", "5V", "1V8": a voltage spelled as a rail name.
    let rail = upper.starts_with(|c: char| c.is_ascii_digit())
        && upper.contains('V')
        && upper
            .chars()
            .all(|c| c.is_ascii_digit() || c == 'V' || c == '.');
    rail || upper.starts_with("VDD")
        || upper.starts_with("VCC")
        || matches!(
            upper.as_str(),
            "VIN" | "VBUS" | "VBAT" | "VSYS" | "BAT" | "BAT+" | "RAW" | "USB" | "V+"
        )
}

/// Why a carrier MODULE (a dev board wrapping an MCU: a module-alias value or
/// a `module = true` card) cannot be bound on this board: its card maps zero
/// pins, and the board's own pin names establish no connected supply pin. The
/// module's header pins are its own, not the chip's, so a "binding" would run
/// firmware on a chip connected to nothing that powers it; it abstains
/// instead. A bare chip binds through its own pin names as before, and a card
/// with a curated pin map, and the family router (which records its own
/// derivation), are not judged here.
fn unpowerable_mcu_card(
    comp: &Component,
    model: &ModelEntry,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> Option<String> {
    if !model.pins.is_empty() || model.params.get_str("auto_bind") == Some("family_router") {
        return None;
    }
    let module = model.params.get_bool("module") == Some(true)
        || hauksbee_models::mcu_modules::mcu_module_alias(&comp.value).is_some();
    if !module {
        return None;
    }
    let derived = derive_mcu_pin_roles(comp);
    let powered = comp.pins.iter().any(|p| {
        let supply = derived
            .roles
            .get(&p.number)
            .is_some_and(|r| MCU_SUPPLY_ROLES.contains(&r.as_str()))
            || is_module_supply_pin_name(&p.function);
        supply && node_of(p.net).is_some_and(|n| !n.is_ground())
    });
    if powered {
        return None;
    }
    let mapped = if derived.roles.is_empty() {
        "name no pins".to_string()
    } else {
        format!("name only {} signal/ground pin(s)", derived.roles.len())
    };
    Some(format!(
        "\"{}\" not bound: this MCU module's card has no pin map and the board's pin names \
         {mapped} with no supply pin, so the part cannot be powered in simulation; add a pin \
         map (--models-dir) to simulate it",
        comp.value
    ))
}

/// Whether the first function a pad's name lists (`GPIO4` of
/// `GPIO4/ADC2_CH0/TOUCH0`) is the GPIO `role` stands for, as opposed to the
/// pad only listing that GPIO among its alternates (`GPI34/ADC1_CH6/RTC_GPIO4`
/// is GPIO34, whose RTC channel is numbered 4).
fn pad_names_its_gpio(function: &str, role: &str, module: bool) -> bool {
    let first = function
        .split('/')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase();
    role_from_mcu_pinfunction_token(&first)
        .and_then(|r| gpio_of_role(&r, module))
        .is_some_and(|pb| gpio_of_role(role, module) == Some(pb))
}

/// The role that owns each port pin when several roles reach it: a pad naming
/// the GPIO as its own function beats one that only lists it among its
/// alternates, and the first role by name settles a tie. `by_role` is in name
/// order, so the owner never depends on hash order.
fn port_pin_owners<'a>(
    by_role: &[(&'a String, NodeId)],
    role_primary: &HashMap<String, bool>,
    module: bool,
) -> HashMap<(char, u8), &'a str> {
    let primary = |r: &str| role_primary.get(r).copied().unwrap_or(false);
    let mut owners: HashMap<(char, u8), &str> = HashMap::new();
    for &(role, node) in by_role {
        if node.is_ground() {
            continue;
        }
        let port_bit = gpio_of_role(role, module).or_else(|| apin_gpio_of_role(role, module));
        if let Some(pb) = port_bit {
            let owner = owners.entry(pb).or_insert(role.as_str());
            if !primary(owner) && primary(role) {
                *owner = role.as_str();
            }
        }
    }
    owners
}

/// The roles of `role_nets` with their nets, in role-name order.
fn roles_by_name(role_nets: &HashMap<String, NodeId>) -> Vec<(&String, NodeId)> {
    let mut by_role: Vec<_> = role_nets.iter().map(|(r, &n)| (r, n)).collect();
    by_role.sort_unstable_by(|a, b| a.0.cmp(b.0));
    by_role
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
    // Whether the pad that supplied each role names that GPIO as its primary
    // function, not as one of its alternates (see `pad_names_its_gpio`).
    let mut role_primary: HashMap<String, bool> = HashMap::new();
    for pin in &comp.pins {
        if let Some(role) = effective_pins.get(&pin.number) {
            pad_roles.insert(pin.number.clone(), role.clone());
            if let Some(node) = node_of(pin.net) {
                role_nets.insert(role.clone(), node);
                role_primary.insert(
                    role.clone(),
                    pad_names_its_gpio(&pin.function, role, module),
                );
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
    // pinMode decides. This is what un-floats shift-register OE / SRCLR pins wired to
    // analog-capable pins: firmware driving an A-pin digitally now has a
    // driver to enable, where a floating-low SRCLR would have held the whole 74HC595 chain cleared.
    let mut gpio_drivers = HashMap::new();
    let mut adc_nets = HashMap::new();
    let mut adc_pin = HashMap::new();
    // Roles in name order, never hash order: two roles reaching one port pin
    // would otherwise race for it (see `port_pin_owners`).
    let by_role = roles_by_name(&role_nets);
    let port_pin_owner = port_pin_owners(&by_role, &role_primary, module);
    for (role, node) in by_role {
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
        let port_bit = gpio_of_role(role, module)
            .or_else(|| apin_gpio_of_role(role, module))
            .filter(|pb| port_pin_owner.get(pb).is_some_and(|o| *o == role.as_str()));
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

    // A pin's output current comes from the chip's I/O supply and returns to
    // its ground pin, so a load on a GPIO shows on the rail that feeds the chip.
    let roles_net = |names: &[&str]| pick(&role_nets, names).copied();
    // A module's logic supply is its `5v` (Nano) or `3v3`/`3v3d` pin.
    let io_supply = roles_net(&MCU_SUPPLY_ROLES);
    let io_return = roles_net(&["vss", "gnd"]).unwrap_or(NodeId::GROUND);
    let mut pins: Vec<(char, u8)> = gpio_drivers.keys().copied().collect();
    pins.sort_unstable();
    for (port, bit) in pins {
        if let (Some(hi), Some(drv)) = (io_supply, gpio_drivers.get_mut(&(port, bit))) {
            let tag = format!("{}_{port}{bit}", comp.reference);
            drv.route_supply(circuit, &tag, hi, io_return);
        }
    }

    mcus.push(McuBinding {
        reference: comp.reference.clone(),
        backend,
        // The raw requested part string, captured before family routing collapsed
        // e.g. STM32F411RET6 -> the stm32f4 backend. Empty when the board gives no
        // value (we still bind, but cannot name what was asked for).
        requested_part: comp.value.trim().to_string(),
        clock: McuClock::default(),
        internal_clock: model.params.get_str("internal_clock").map(str::to_string),
        pad_roles,
        role_nets,
        gpio_drivers,
        adc_nets,
        adc_pin,
        module,
        max_supply_v: model.ratings.max_voltage_v,
        supply_stand_in: model.ratings.basis.stand_in_for(Limit::Voltage).cloned(),
        min_supply_v: model
            .envelope
            .iter()
            .filter_map(|e| match e {
                hauksbee_models::OperatingEnvelope::SupplyRange { pin, min_v, .. }
                    if !hauksbee_models::pin_roles::is_sense_input_role(pin) =>
                {
                    Some(*min_v)
                }
                _ => None,
            })
            .reduce(f64::min),
        supply_floors: supply_floors(model),
    });
    log_mcu_auto_decision(comp, model, derived_when_empty.as_ref())
}

/// Each supply role's recommended operating minimum across the card's
/// `supply_range` rows (the loosest row when temperature rows repeat a pin).
fn supply_floors(model: &ModelEntry) -> HashMap<String, f64> {
    let mut floors: HashMap<String, f64> = HashMap::new();
    for row in &model.envelope {
        if let hauksbee_models::OperatingEnvelope::SupplyRange { pin, min_v, .. } = row {
            floors
                .entry(pin.trim().to_ascii_lowercase())
                .and_modify(|v| *v = v.min(*min_v))
                .or_insert(*min_v);
        }
    }
    floors
}

/// Whether `comp` is an assembled crystal or resonator: recognised by its
/// designator or frequency value, or by the model library's footprint rules.
fn fitted_crystal(comp: &Component, lib: &ModelLibrary) -> bool {
    // Only an assembled part with an established identity can supply the MCU's
    // external clock: DNP and identity-refused records are absent or unprovable.
    let Some(part) = AssemblyState::of(comp).fitted() else {
        return false;
    };
    is_crystal_like(&ref_prefix(&comp.reference), &comp.value)
        || resolve(lib, part)
            .model
            .as_ref()
            .is_some_and(|m| m.id.starts_with("crystal"))
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
        Device::Resistor { ohms, .. } => format!("R {}", format_engineering_web(*ohms, "Ω")),
        Device::Capacitor { farads, .. } => format!("C {}", format_engineering_web(*farads, "F")),
        Device::Inductor { henries, .. } => format!("L {}", format_engineering_web(*henries, "H")),
        other => other.name().to_string(),
    }
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

/// The ground nets the binder fuses onto node 0 beyond the canonical ones.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GroundReferences {
    /// Net ids that are the 0 V reference of their circuit.
    pub nodes: std::collections::HashSet<i64>,
    /// When no net is a canonical ground, the reference of the board's largest
    /// circuit: the board's own ground, not a separate circuit's. It binds to
    /// node 0 without a disclosure row, because there is no other ground for it
    /// to be separate from.
    pub main: Option<i64>,
}

impl GroundReferences {
    /// Whether net `id` is one of the references.
    #[must_use]
    pub fn contains(&self, id: &i64) -> bool {
        self.nodes.contains(id)
    }
}

/// [`ground_references`]'s nodes alone.
#[must_use]
pub fn island_reference_grounds(board: &ExtractedBoard) -> std::collections::HashSet<i64> {
    ground_references(board).nodes
}

/// The ground-family nets that are the reference of a circuit no canonical
/// ground reaches, by net id: one per such circuit.
///
/// A board file can carry more than one circuit: the two halves of a split
/// keyboard panelised on one PCB (`GND`/`+5V` on one, `GNDA`/`+5VA` on the
/// other), or a sub-board beside the main one. Nothing joins such a ground to
/// node 0, yet the parts on it are not referenced to it: a
/// regulator's pass source and an MCU's pin drivers stamp against node 0, and
/// the bench supply the binder assumes for its rails returns to node 0. The
/// island's ground then has no DC definition but gmin, every part on it sits
/// wherever that leakage puts it (the second half's ground read 5 V), and the
/// operating point hinges on how one indicator LED happens to conduct. Its own
/// ground IS that circuit's reference, and with no shared copper nothing can
/// tell it apart from node 0, so it is node 0.
///
/// Only a ground no part reaches from canonical ground qualifies, counting
/// every part, fitted or not, as joining all its pins' nets: an `AGND` that a
/// ferrite, a 0 ohm link, a net tie, an isolated converter or a cable header
/// joins to `GND` keeps its own node, so the split-ground and isolation
/// structure stays checkable. Of several ground-family nets in one such
/// circuit, the one with the most pins is its reference (ties by name) and the
/// rest stay split from it. `VEE` is never one: it is a negative rail.
///
/// A board with no canonical ground at all (its ground is `GND1`, `GROUND`,
/// `GNDREF` or `AGND`, a spelling and nothing more) is the same circuit as one
/// that says `GND`: each of its circuits takes a reference by the same rule, and
/// the largest is the board's own ground ([`GroundReferences::main`]). Where no
/// net is named as a ground at all, the net that most pins call a ground
/// (`GND`, `VSS`, ... as printed on the part) is the reference, so a board whose
/// ground carries a name no table lists is still solved against it.
#[must_use]
pub fn ground_references(board: &ExtractedBoard) -> GroundReferences {
    // Union-find over net ids: each part joins all of its pins' nets.
    let root = |parent: &HashMap<i64, i64>, mut id: i64| {
        while let Some(&up) = parent.get(&id).filter(|&&up| up != id) {
            id = up;
        }
        id
    };
    let mut parent: HashMap<i64, i64> = HashMap::new();
    let mut pins: HashMap<i64, usize> = HashMap::new();
    for comp in &board.components {
        let nets: Vec<i64> = comp
            .pins
            .iter()
            .filter_map(|p| p.net.filter(|&id| id != 0))
            .collect();
        for &id in &nets {
            *pins.entry(id).or_default() += 1;
        }
        for pair in nets.windows(2) {
            let (a, b) = (root(&parent, pair[0]), root(&parent, pair[1]));
            if a != b {
                parent.insert(a, b);
            }
        }
    }
    let canonical: std::collections::HashSet<i64> = board
        .nets
        .iter()
        .filter(|n| n.id != 0 && is_canonical_ground(&n.name) && pins.contains_key(&n.id))
        .map(|n| root(&parent, n.id))
        .collect();
    let mut best: HashMap<i64, (usize, &str, i64)> = HashMap::new();
    for net in &board.nets {
        let leaf = net.name.rsplit('/').next().unwrap_or(&net.name).trim();
        if net.id == 0
            || !is_ground(&net.name)
            || is_canonical_ground(&net.name)
            || leaf.eq_ignore_ascii_case("vee")
        {
            continue;
        }
        let island = root(&parent, net.id);
        let count = pins.get(&net.id).copied().unwrap_or(0);
        if count == 0 || canonical.contains(&island) {
            continue;
        }
        let entry = best.entry(island).or_insert((count, &net.name, net.id));
        if (count, std::cmp::Reverse(net.name.as_str())) > (entry.0, std::cmp::Reverse(entry.1)) {
            *entry = (count, &net.name, net.id);
        }
    }
    let mut nodes: std::collections::HashSet<i64> = best.values().map(|&(_, _, id)| id).collect();
    if !canonical.is_empty() {
        return GroundReferences { nodes, main: None };
    }
    // No canonical ground: the reference carrying the most pins is the board's
    // own ground, and the other circuits' references are disclosed as separate.
    let mut main = best
        .values()
        .max_by_key(|&&(count, name, _)| (count, std::cmp::Reverse(name)))
        .map(|&(_, _, id)| id);
    if main.is_none() {
        main = ground_by_pin_function(board);
        nodes.extend(main);
    }
    GroundReferences { nodes, main }
}

/// The net most pins call a ground by their own printed function (`GND`, `VSS`,
/// `AGND`), when no net name says ground. Never a net named as a supply.
fn ground_by_pin_function(board: &ExtractedBoard) -> Option<i64> {
    let mut called: HashMap<i64, usize> = HashMap::new();
    for pin in board.components.iter().flat_map(|c| &c.pins) {
        if let Some(id) = pin.net.filter(|&id| id != 0) {
            if !pin.function.trim().is_empty() && is_ground(&pin.function) {
                *called.entry(id).or_default() += 1;
            }
        }
    }
    board
        .nets
        .iter()
        .filter(|n| {
            !names_a_supply_of_unknown_voltage(&n.name) && power_rail_voltage(&n.name).is_none()
        })
        .filter_map(|n| Some((called.get(&n.id).copied()?, &n.name, n.id)))
        .max_by_key(|&(count, name, _)| (count, std::cmp::Reverse(name)))
        .map(|(_, _, id)| id)
}

/// Net-name classification lives in one table, `hauksbee_models::netclass`;
/// these are its names as the binder has always exported them.
pub use hauksbee_models::netclass::{
    is_canonical_ground, is_ground, names_a_supply_of_unknown_voltage,
    rail_voltage as power_rail_voltage,
};

/// How many parts must hang off a supply-named net before its silence counts as
/// a dead rail rather than an oddly-named signal. An enable called `VCC_EN`
/// reaches a handful of pins; a rail reaches the board.
const DEAD_RAIL_MIN_PARTS: usize = 6;

/// Nets that name themselves a supply, carry a rail's worth of parts, and are
/// powered by nothing.
///
/// The binder refuses to guess a voltage for `VDD_A` or a bare `VDD`,
/// which is right: inventing one would over- or under-drive every part on the
/// net. But the net then sits at 0 V, the operating point around it is fiction,
/// and the stress monitor reports on that fiction with the same confidence it
/// reports a real overload -- a named accusation against a specific part, from
/// a run where the rails were dead. Naming the dead rails is what lets a reader
/// tell that report from a true one.
///
/// A regulator the board statically holds off ([`held_off_rails`]) leaves its
/// output rail just as dead, whatever the net is called, so those rails are
/// named here too unless `also_powered` lists them.
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
    // A regulator the board holds off powers nothing, whatever its rail is
    // called and however few parts hang off it.
    out.extend(
        held_off_rails(bound)
            .into_iter()
            .map(|(net, _)| net)
            .filter(|net| !powered.contains(net.as_str())),
    );
    out.sort();
    out.dedup();
    out
}

/// The output net of each simple regulator the board holds off (its enable
/// pin tied or pulled to its off level with nothing to move it; see
/// [`crate::vreg_enable`]), with the sentence saying why.
#[must_use]
pub fn held_off_rails(bound: &BoundBoard) -> Vec<(String, String)> {
    bound
        .report
        .regulators
        .iter()
        .filter_map(|reg| {
            let why = reg.gate.as_ref()?.held_off.as_ref()?;
            let Some(Device::Vsource { p, .. }) = bound.circuit.devices.get(reg.source.0 as usize)
            else {
                return None;
            };
            let net = bound.circuit.node_name(*p).to_string();
            let because = format!("{} is held off: {why}", reg.reference);
            Some((net, because))
        })
        .collect()
}

/// True when a supply net's name carries no magnitude at all (`VCC`, `+VCC`),
/// so any voltage [`power_rail_voltage`] returns for it is a convention, not a
/// statement. `VBUS` and the digit-bearing names (`+5V`, `VCC_3V3`) state theirs.
fn name_states_no_magnitude(name: &str) -> bool {
    let leaf = name.rsplit('/').next().unwrap_or(name).trim();
    let leaf = leaf.trim_start_matches('+');
    leaf.eq_ignore_ascii_case("vcc")
}

/// Nets driven by the output pin of a regulator-shaped part: one pad that
/// reads as an output (`OUT`, `VOUT`, `VO`, `OUTPUT`) on a part that also has
/// an input pad (`IN`, `VIN`, `VI`, `INPUT`). Judged from pin names alone, so
/// it holds whether or not the part resolved to a model. Maps net id to
/// `"U5 (LD1086D2T33TR)"` for the warning.
///
/// A pad name carries `KiCad`'s `_<pad>` suffix on a layout-only extraction
/// (`OUT_2`), which is removed before comparing.
fn regulator_output_nets(board: &ExtractedBoard) -> HashMap<i64, String> {
    fn base(function: &str) -> String {
        let f = function.trim().to_ascii_uppercase();
        match f.rsplit_once('_') {
            Some((head, tail)) if !head.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => {
                head.to_string()
            }
            _ => f,
        }
    }
    let is_out = |p: &hauksbee_extract::Pin| {
        matches!(base(&p.function).as_str(), "OUT" | "VOUT" | "VO" | "OUTPUT")
    };
    let is_in = |p: &hauksbee_extract::Pin| {
        matches!(base(&p.function).as_str(), "IN" | "VIN" | "VI" | "INPUT")
    };
    let mut out = HashMap::new();
    for comp in &board.components {
        if !AssemblyState::of(comp).is_present() || !comp.pins.iter().any(is_in) {
            continue;
        }
        for pin in comp.pins.iter().filter(|p| is_out(p)) {
            if let Some(net_id) = pin.net.filter(|n| *n != 0) {
                out.insert(net_id, format!("{} ({})", comp.reference, comp.value));
            }
        }
    }
    out
}

/// Rails a fitted part sources on its own pins per its card's
/// `[[models.supply_outputs]]` (a dev-board module's 5V/3V3 header pins),
/// as net id -> nominal volts. This is the card-side twin of
/// [`power_out_net_voltages`] for a board whose symbol does not tag the pin
/// `power_out`; a regulator output on the net is excluded the same way, by the
/// caller's regulator check and the vreg model sourcing its own net.
fn module_supply_net_voltages(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    facts: Option<&BoardFacts>,
) -> HashMap<i64, f64> {
    let mut out = HashMap::new();
    for (index, comp) in board.components.iter().enumerate() {
        let Some(part) = AssemblyState::of(comp).fitted() else {
            continue;
        };
        let found = part_facts(facts, index, lib, part);
        let Some(model) = &found.resolution.model else {
            continue;
        };
        for supply in &model.supply_outputs {
            let pads = model.pins.iter().filter(|(_, role)| **role == supply.role);
            for (pad, _) in pads {
                let pin = comp.pins.iter().find(|p| p.number == *pad);
                let net = pin.and_then(|p| p.net).filter(|id| *id != 0);
                // A pin on a net no other part touches feeds nothing.
                let fed = |id: i64| {
                    let others = board.components.iter().filter(|c| !std::ptr::eq(*c, comp));
                    others.flat_map(|c| &c.pins).any(|p| p.net == Some(id))
                };
                if let Some(net_id) = net.filter(|id| fed(*id)) {
                    out.entry(net_id).or_insert(supply.nominal);
                }
            }
        }
    }
    out
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

    /// The `ATmega328P` 32-pad packages number their pads differently from
    /// DIP-28: binding DIP numbering to a TQFP put VCC on XTAL1 (emonTx V3.4).
    #[test]
    fn atmega328p_pin_numbering_follows_the_package() {
        let lib = ModelLibrary::builtin();
        let tqfp = resolve_value(&lib, "ATMEGA328P", "TQFP32-08");
        assert_eq!(tqfp.id, "atmega328p_32pad");
        assert_eq!(tqfp.pins.get("4").map(String::as_str), Some("vcc"));
        assert_eq!(tqfp.pins.get("7").map(String::as_str), Some("pb6_xtal1"));
        let dip = resolve_value(&lib, "ATMEGA328P", "Package_DIP:DIP-28_W7.62mm");
        assert_eq!(dip.id, "atmega328p");
        assert_eq!(dip.pins.get("7").map(String::as_str), Some("vcc"));

        let mut c = comp("U1", "ATMEGA328P-AU");
        assert_eq!(atmega328p_pin_map(&c)["10"], "pd6_ain0_oc0a");
        c.value = "ATMEGA328P".into();
        c.footprint = "Housings_QFP:TQFP-32_7x7mm_Pitch0.8mm".into();
        assert_eq!(atmega328p_pin_map(&c)["10"], "pd6_ain0_oc0a");
        c.footprint = "Package_DIP:DIP-28_W7.62mm".into();
        assert_eq!(atmega328p_pin_map(&c)["12"], "pd6_ain0_oc0a");
        c.footprint = "QFN-320".into();
        assert_eq!(atmega328p_pin_map(&c)["12"], "pd6_ain0_oc0a");
    }

    /// A value still reading as KiCad's generic transistor symbol binds to the
    /// class card, disclosed as an estimated fallback, with the pad order the
    /// symbol name spells (not the SOT-23 rule's B-E-C guess).
    #[test]
    fn generic_kicad_transistor_symbols_bind_by_class_in_their_pad_order() {
        use hauksbee_ir::evidence::ModelSourceTier;
        let lib = ModelLibrary::builtin();
        for (value, id, roles) in [
            (
                "Q_NPN_BCE",
                "generic_kicad_npn",
                ["base", "collector", "emitter"],
            ),
            (
                "Q_PNP_EBC",
                "generic_kicad_pnp",
                ["emitter", "base", "collector"],
            ),
            (
                "Q_PMOS_GSD",
                "generic_pmos_sot_corner_high_vth",
                ["gate", "source", "drain"],
            ),
            (
                "q_nmos_dgs",
                "generic_nmos_sot_corner_high_vth",
                ["drain", "gate", "source"],
            ),
        ] {
            let mut c = comp("Q1", value);
            c.footprint = "Package_TO_SOT_SMD:SOT-23".into();
            c.pins = vec![pin("1", 1), pin("2", 2), pin("3", 3)];
            let res = library_resolution(&lib, &c);
            let model = res
                .model
                .unwrap_or_else(|| panic!("{value} must bind by class"));
            assert_eq!(model.id, id);
            let tier = res.provenance.as_ref().map(ModelSource::tier);
            assert_eq!(tier, Some(ModelSourceTier::EstimatedFallback), "{value}");
            assert!(model
                .params
                .get_str("warning")
                .is_some_and(|w| w.contains("ASSUMED")));
            let got: Vec<_> = ["1", "2", "3"].map(|p| model.pins[p].clone()).into();
            assert_eq!(got, roles, "{value}");
        }
        // Only the generic symbol name carries a pad order, and only for its own class.
        assert!(generic_symbol_pins("2N7002", ComponentKind::Nmos).is_none());
        assert!(generic_symbol_pins("Q_NMOS_GSD", ComponentKind::Pmos).is_none());
        assert!(generic_symbol_pins("Q_NPN_BBE", ComponentKind::BjtNpn).is_none());
        let tab = generic_symbol_pins("Q_NMOS_GDSD", ComponentKind::Nmos).expect("tab pad");
        assert_eq!(tab["4"], "drain");
    }

    /// A MOSFET the board names only by class binds that class's assumed
    /// corner set, the envelope chosen by package, in the pad order its symbol
    /// spells; a part number anywhere, or a value and symbol disagreeing on the
    /// polarity, binds no class.
    #[test]
    fn a_mosfet_named_only_by_class_binds_its_corner_set_in_the_symbol_pad_order() {
        let lib = ModelLibrary::builtin();
        let fet = |value: &str, lib_id: &str, footprint: &str, props: &[(&str, &str)]| {
            let mut c = comp("Q1", value);
            (c.lib_id, c.footprint) = (lib_id.into(), footprint.into());
            c.pins = vec![pin("1", 1), pin("2", 2), pin("3", 3)];
            c.properties = props
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            library_resolution(&lib, &c)
        };
        let roles = |m: &ModelEntry| ["1", "2", "3"].map(|p| m.pins[p].clone()).to_vec();
        let corners =
            |r: &hauksbee_models::Resolution| r.selection.as_ref().is_some_and(|s| s.corners);
        #[rustfmt::skip]
        let classed = [
            // A class word on a generic symbol in a power package: this bound the
            // N-channel power catch-all before, on an id tie, for a P-channel part.
            ("PMOS", "Device:Q_PMOS_GDS", "Package_TO_THT:TO-220-3_Vertical", "generic_pmos_power_corner_high_vth", ["gate", "drain", "source"]),
            ("MOSFET_N", "Transistor_FET:Q_NMOS_GSD", "Package_TO_SOT_SMD:SOT-23", "generic_nmos_sot_corner_high_vth", ["gate", "source", "drain"]),
            ("", "Device:Q_NMOS_SGD", "Custom:FET3", "generic_nmos_class_corner_high_vth", ["source", "gate", "drain"]),
        ];
        for (value, lib_id, footprint, id, want) in classed {
            let res = fet(value, lib_id, footprint, &[]);
            let model = res
                .model
                .as_ref()
                .unwrap_or_else(|| panic!("{value} {lib_id}"));
            assert_eq!(
                (&*model.id, roles(model)),
                (id, want.map(String::from).to_vec())
            );
            let s = res.selection.as_ref().expect("a corner set");
            assert!(
                s.corners && s.determined.is_none() && s.same_pinout,
                "{}",
                s.listing()
            );
            assert_eq!(s.members.len(), 2, "{}", s.listing());
            assert_eq!(res.confidence, Confidence::Guessed);
        }
        // A class word with no symbol spells no pad order: the card keeps its
        // electrode-letter pins and the package rule (a disclosed guess) maps pads.
        let worded = fet("P-Channel MOSFET", "", "SOT-23", &[]);
        let model = worded.model.expect("class word binds");
        assert_eq!(model.id, "generic_pmos_sot_corner_high_vth");
        assert_eq!(model.pins["G"], "gate");
        // KiCad 10's unordered Device:Q_NMOS numbers its pads by electrode letter.
        let mut lettered = comp("Q2", "Q_NMOS");
        lettered.lib_id = "Device:Q_NMOS".into();
        lettered.pins = vec![pin("D", 1), pin("G", 2), pin("S", 3)];
        let model = library_resolution(&lib, &lettered)
            .model
            .expect("bare symbol");
        assert_eq!(model.id, "generic_nmos_class_corner_high_vth");
        assert_eq!(model.pins["G"], "gate");
        // Controls: a part number, in an MPN field or the value, or a polarity
        // the value and symbol dispute, binds no class corner.
        let mpn = fet(
            "MOSFET_P",
            "Device:Q_PMOS_GSD",
            "SOT-23",
            &[("Part Number", "XQ-NOCARD-1")],
        );
        assert!(mpn.model.is_none() && !corners(&mpn));
        let disputed = fet("PMOS", "Device:Q_NMOS_GSD", "SOT-23", &[]);
        assert!(!corners(&disputed), "{:?}", disputed.model.map(|m| m.id));
        let rated = fet("20V/4.2A", "", "SOT-23", &[]);
        assert!(!corners(&rated));
        let named = fet("AO3400A", "Device:Q_NMOS_GSD", "SOT-23", &[]);
        assert_eq!(named.model.as_ref().map(|m| &*m.id), Some("ao3400a"));
    }

    /// The schematic library part (a KiCad symbol, an Eagle device) identifies a
    /// part whose value is a truncation of it or only a rating, and never one
    /// whose value names a different part or a passive magnitude.
    #[test]
    fn library_part_names_stand_in_only_where_the_value_cannot_contradict() {
        let lib = ModelLibrary::builtin();
        let sot5 = || (1..=5).map(|n| pin(&n.to_string(), n)).collect::<Vec<_>>();
        let symbol = |reference: &str, value: &str, lib_id: &str| {
            let mut c = comp(reference, value);
            c.lib_id = lib_id.into();
            c.footprint = "Package_TO_SOT_SMD:SOT-23-5".into();
            c.pins = sot5();
            library_resolution(&lib, &c).model.map(|m| m.id)
        };
        let device = |reference: &str, value: &str, name: &str| {
            let mut c = comp(reference, value);
            c.lib_id = "SOT23-5".into();
            c.footprint = "SOT23-5".into();
            c.pins = sot5();
            c.properties = vec![(
                hauksbee_extract::bom::LIBRARY_DEVICE_PROPERTY.to_string(),
                name.to_string(),
            )];
            library_resolution(&lib, &c).model.map(|m| m.id)
        };
        let ap = Some("ap2112k_3v3".to_string());
        // The value dropped the voltage code the symbol keeps.
        assert_eq!(symbol("U1", "AP2112K", "Regulator_Linear:AP2112K-3.3"), ap);
        // An Eagle device behind a rating value, prefixed with a library class.
        assert_eq!(device("U1", "3.3V", "V_REG_AP2112K-3.3V"), ap);
        assert_eq!(device("U1", "3V3", "V_REG_AP2112K-3.3V"), ap);
        assert_eq!(device("U1", "", "V_REG_AP2112K-3.3V"), ap);
        // The Eagle reader's embedded part number under a rating string.
        assert_eq!(device("U1", "3.3V/600mA", "AP2112K-3.3V"), ap);
        assert!(is_rating_string("20V/4.2A/52mΩ/1.4W"));
        assert!(!is_rating_string("100nF/50V") && !is_rating_string("10k/1%"));
        // Controls: a borrowed symbol under another part's value, a device with
        // no voltage code, a rating on a non-IC designator, and a layout-only
        // record whose lib_id is its footprint.
        let borrowed = symbol("U1", "RT9080-33GJ5", "Regulator_Linear:AP2112K-3.3");
        assert_eq!(borrowed.as_deref(), Some("rt9080_3v3"));
        assert_eq!(
            symbol("U1", "XQ9-NOCARD", "Regulator_Linear:AP2112K-3.3"),
            None
        );
        assert_eq!(device("U1", "3.3V", "V_REG_AP2112K"), None);
        assert_eq!(device("D1", "3.3V", "V_REG_AP2112K-3.3V"), None);
        assert_eq!(device("U1", "100nF", "V_REG_AP2112K-3.3V"), None);
        assert_eq!(device("U1", "10k", "V_REG_AP2112K-3.3V"), None);
        assert_eq!(device("U1", "100nF/50V", "V_REG_AP2112K-3.3V"), None);
        assert_eq!(symbol("U1", "", "SOT-23-5"), None);
    }

    /// A part number held in a design FIELD under any common spelling identifies
    /// a part its value leaves anonymous; a catalogue code (`PROD_ID`) never
    /// does, an empty MPN field does not hide a filled one, and a value that
    /// reaches a card itself keeps precedence over the field.
    #[test]
    fn part_number_fields_identify_parts_the_value_does_not() {
        let lib = ModelLibrary::builtin();
        let bound = |value: &str, props: &[(&str, &str)]| {
            let mut c = comp("Q1", value);
            c.footprint = "SOT-23".into();
            c.pins = vec![pin("1", 1), pin("2", 2), pin("3", 3)];
            c.properties = props
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            library_resolution(&lib, &c).model.map(|m| m.id)
        };
        let ao = Some("ao3400a".to_string());
        // An Eagle attribute, a nonstandard KiCad field and a `#` spelling.
        assert_eq!(bound("XQ-17", &[("MANUFACTURER-PN", "AO3400A")]), ao);
        assert_eq!(bound("XQ-17", &[("Manufacturer_No", "AO3400A")]), ao);
        assert_eq!(bound("XQ-17", &[("MFR.Part #", "AO3400A")]), ao);
        // The first FILLED field wins over an empty one ahead of it.
        assert_eq!(
            bound("XQ-17", &[("MPN", " "), ("PART_NUMBER", "AO3400A")]),
            ao
        );
        // Controls: a catalogue code and a manufacturer name are not part numbers.
        assert_ne!(bound("XQ-17", &[("PROD_ID", "AO3400A")]), ao);
        assert_ne!(bound("XQ-17", &[("Manufacturer", "AO3400A")]), ao);
        // A value that names a card keeps it.
        assert_eq!(
            bound("BSS138", &[("MPN", "AO3400A")]).as_deref(),
            Some("bss138")
        );
        let mut c = comp("Q1", "XQ-17");
        c.properties = vec![
            ("MPN".into(), String::new()),
            ("Manufacturer_No".into(), "AO3400A".into()),
        ];
        assert_eq!(component_mpn(&c).as_deref(), Some("AO3400A"));
    }

    /// A part number the schematic's library device name embeds identifies a
    /// part whose value is a rating string, on the MPN field's terms: only
    /// where the value decided nothing, and only when it reaches a card.
    #[test]
    fn a_library_device_part_number_identifies_a_rating_string_value() {
        let lib = ModelLibrary::builtin();
        let key = hauksbee_extract::bom::LIBRARY_DEVICE_PROPERTY;
        let bound = |value: &str, footprint: &str, device: &str| {
            let mut c = comp("U1", value);
            c.footprint = footprint.into();
            c.pins = (1..=5).map(|n| pin(&n.to_string(), n)).collect();
            c.properties = vec![(key.to_string(), device.to_string())];
            library_resolution(&lib, &c).model.map(|m| m.id)
        };
        let ap = Some("ap2112k_3v3".to_string());
        assert_eq!(bound("3.3V/600mA", "SOT-23-5", "AP2112K-3.3V"), ap);
        assert_eq!(bound("", "SOT-23-5", "AP2112K-3.3V"), ap);
        let mut fet = comp("Q1", "30V/5.7A");
        fet.footprint = "SOT-23".into();
        fet.pins = vec![pin("1", 1), pin("2", 2), pin("3", 3)];
        fet.properties = vec![(key.to_string(), "AO3400A".to_string())];
        let fet = library_resolution(&lib, &fet).model.map(|m| m.id);
        assert_eq!(fet.as_deref(), Some("ao3400a"));
        // Controls: a value that names a card keeps it, and a candidate that
        // reaches no card changes nothing.
        assert_eq!(
            bound("MIC5205-3.3", "SOT-23-5", "AP2112K-3.3V").as_deref(),
            bound("MIC5205-3.3", "SOT-23-5", "").as_deref()
        );
        assert_eq!(
            bound("3.3V/600mA", "SOT-23-5", "XQ-NOCARD-1"),
            bound("3.3V/600mA", "SOT-23-5", "")
        );
        assert!(!hauksbee_extract::bom::is_mpn_property_key(key));
    }

    /// A memory-card socket is a connector under any designator; an SD NAND chip
    /// that names the same bus pins but has no shell or detect contact is not.
    #[test]
    fn card_sockets_are_connectors_whatever_their_designator() {
        let named = |names: &[&str]| -> Vec<Pin> {
            (names.iter().enumerate())
                .map(|(i, f)| Pin {
                    function: (*f).to_string(),
                    ..pin(&(i + 1).to_string(), 1)
                })
                .collect()
        };
        let bus = [
            "DAT2", "CD/DAT3", "CMD", "VDD", "CLK", "VSS", "DAT0", "DAT1",
        ];
        let mut socket = comp("U3", "XYZ2075");
        socket.pins = named(&[&bus[..], &["CD", "SHIELD"]].concat());
        let entry = fallback_entry(&socket).expect("socket");
        assert_eq!(entry.kind, ComponentKind::Connector);
        let mut by_name = comp("U9", "ABC");
        by_name.footprint = "Connector_Card:microSD_HC_Molex".into();
        assert!(is_card_socket(&by_name));
        let mut nand = comp("U4", "SD-NAND 4G");
        nand.pins = named(&bus);
        assert!(!is_card_socket(&nand));
        nand.footprint = "MicroSD_NAND_LGA8".into();
        assert!(!is_card_socket(&nand));
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

    /// A ground no part joins to GND (the second half of a panelised split
    /// keyboard) is that circuit's reference and binds onto node 0; a split
    /// ground any part bridges to GND, even through a ferrite, keeps its own
    /// node, and so does `VEE`.
    #[test]
    fn a_ground_no_part_joins_to_gnd_is_its_circuits_reference() {
        let part = |r: &str, value: &str, nets: &[i64]| {
            let mut c = comp(r, value);
            c.lib_id = "Device:R".into();
            c.pins = (1..)
                .zip(nets)
                .map(|(n, &id)| pin(&n.to_string(), id))
                .collect();
            c
        };
        // Left half on GND/+5V, right half on GNDA/+5VA with an AGND inside it
        // (via FB2), and an AGND2 that FB1 joins to the left half's GND.
        let nets = [
            (1, "GND"),
            (2, "+5V"),
            (3, "GNDA"),
            (4, "+5VA"),
            (5, "AGND"),
            (6, "AGND2"),
            (7, "VEE"),
            (8, "-5V"),
        ];
        let board = ExtractedBoard {
            name: "two halves".into(),
            nets: nets.iter().map(|&(id, name)| net(id, name)).collect(),
            components: vec![
                part("C1", "100n", &[2, 1]),
                part("FB1", "FB", &[1, 6]),
                part("C2", "100n", &[4, 3]),
                part("C3", "100n", &[4, 3]),
                part("FB2", "FB", &[3, 5]),
                part("C4", "100n", &[7, 8]),
            ],
        };
        // GNDA carries the most pins of its circuit (AGND rides on it through
        // FB2 and stays split); the VEE circuit has no ground-family reference.
        assert_eq!(
            island_reference_grounds(&board),
            std::collections::HashSet::from([3])
        );
        let bound = bind_board(&board, &ModelLibrary::builtin());
        assert_eq!(bound.node("GNDA"), Some(NodeId::GROUND));
        for split in ["AGND", "AGND2", "VEE"] {
            assert_ne!(bound.node(split), Some(NodeId::GROUND), "{split}");
        }
        let row = bound
            .report
            .row("RAIL:GNDA")
            .expect("the fusion is disclosed");
        assert!(row
            .warning
            .as_deref()
            .is_some_and(|w| w.contains("shares no part with GND")));

        // With no part on GND there is no main circuit to be separate from:
        // the right half is the whole board, GNDA is its ground (node 0) and
        // nothing is disclosed as a separate circuit.
        let mut alone = board.clone();
        alone
            .components
            .retain(|c| !matches!(c.reference.as_str(), "C1" | "FB1"));
        let refs = ground_references(&alone);
        assert_eq!(refs.nodes, std::collections::HashSet::from([3]));
        assert_eq!(refs.main, Some(3));
        let alone_bound = bind_board(&alone, &ModelLibrary::builtin());
        assert_eq!(alone_bound.node("GNDA"), Some(NodeId::GROUND));
        assert_ne!(alone_bound.node("AGND"), Some(NodeId::GROUND));
        assert!(alone_bound.report.row("RAIL:GNDA").is_none());

        // Bridge the halves (a cable header carrying both grounds): GNDA is a
        // split ground of one circuit again and keeps its own node.
        let mut joined = board;
        joined.components.push(part("J1", "CONN", &[1, 3]));
        assert!(island_reference_grounds(&joined).is_empty());
        assert_ne!(
            bind_board(&joined, &ModelLibrary::builtin()).node("GNDA"),
            Some(NodeId::GROUND)
        );
    }

    /// A one-circuit board with a decoupling cap and a pull-up on `ground`.
    fn board_grounded_as(ground: &str) -> ExtractedBoard {
        let part = |r: &str, value: &str, nets: [i64; 2]| {
            let mut c = comp(r, value);
            c.lib_id = "Device:R".into();
            c.pins = vec![pin("1", nets[0]), pin("2", nets[1])];
            c
        };
        ExtractedBoard {
            name: "one circuit".into(),
            nets: vec![net(1, ground), net(2, "+3V3"), net(3, "SIG")],
            components: vec![
                part("C1", "100n", [2, 1]),
                part("R1", "10k", [3, 2]),
                part("R2", "10k", [3, 1]),
            ],
        }
    }

    /// A ground is a reference whatever it is called: `GND1`, `GROUND`,
    /// `GNDREF`, `AGND` and `0V` on a board with no other ground bind to node 0
    /// exactly as `GND` does, and say nothing about it.
    #[test]
    fn a_boards_only_ground_is_node_zero_whatever_it_is_called() {
        for ground in ["GND", "0V", "GND1", "GROUND", "GNDREF", "AGND", "/pwr/GND2"] {
            let board = board_grounded_as(ground);
            let bound = bind_board(&board, &ModelLibrary::builtin());
            let leaf = ground.rsplit('/').next().unwrap_or(ground);
            assert_eq!(
                bound.node(ground).or_else(|| bound.node(leaf)),
                Some(NodeId::GROUND),
                "{ground} must be the 0 V reference"
            );
            assert!(
                bound.report.rows.iter().all(|r| !r
                    .warning
                    .as_deref()
                    .is_some_and(|w| w.contains("separate circuit"))),
                "{ground}: the board's own ground is not a separate circuit"
            );
        }
    }

    /// A ground named by no table is still the reference when the parts print it
    /// as a ground: the net most pins call `GND` is the board's reference.
    #[test]
    fn a_ground_no_name_table_lists_is_found_by_its_pins() {
        let mut board = board_grounded_as("RTN_X");
        for comp in &mut board.components {
            for p in &mut comp.pins {
                if p.net == Some(1) {
                    p.function = "GND".into();
                }
            }
        }
        let bound = bind_board(&board, &ModelLibrary::builtin());
        assert_eq!(bound.node("RTN_X"), Some(NodeId::GROUND));
        // A board whose pins say nothing about ground has no reference to find.
        let plain = board_grounded_as("RTN_X");
        assert!(ground_references(&plain).nodes.is_empty());
        // And a supply is never taken for the ground, whatever its pins say.
        let mut supply = board_grounded_as("RTN_X");
        for comp in &mut supply.components {
            for p in &mut comp.pins {
                if p.net == Some(2) {
                    p.function = "GND".into();
                }
            }
        }
        assert!(ground_references(&supply).nodes.is_empty());
    }

    /// Two circuits on one board, neither on a canonical ground: the larger is
    /// the board's own ground, silent; the other is disclosed as a separate
    /// circuit against it, never merged without a word.
    #[test]
    fn two_circuits_with_no_canonical_ground_disclose_the_smaller_one() {
        let part = |r: &str, nets: [i64; 2]| {
            let mut c = comp(r, "10k");
            c.lib_id = "Device:R".into();
            c.pins = vec![pin("1", nets[0]), pin("2", nets[1])];
            c
        };
        let board = ExtractedBoard {
            name: "two circuits".into(),
            nets: vec![
                net(1, "GNDMAIN"),
                net(2, "+5V"),
                net(3, "GNDSUB"),
                net(4, "+3V3"),
            ],
            components: vec![
                part("R1", [2, 1]),
                part("R2", [2, 1]),
                part("R3", [2, 1]),
                part("R4", [4, 3]),
            ],
        };
        let refs = ground_references(&board);
        assert_eq!(refs.main, Some(1));
        assert_eq!(refs.nodes, std::collections::HashSet::from([1, 3]));
        let bound = bind_board(&board, &ModelLibrary::builtin());
        assert_eq!(bound.node("GNDMAIN"), Some(NodeId::GROUND));
        assert_eq!(bound.node("GNDSUB"), Some(NodeId::GROUND));
        assert!(bound.report.row("RAIL:GNDMAIN").is_none());
        let row = bound.report.row("RAIL:GNDSUB").expect("disclosed");
        assert!(row
            .warning
            .as_deref()
            .is_some_and(|w| w.contains("shares no part with GNDMAIN")));
    }

    /// A split ground that a part bridges stays split on a board with no
    /// canonical ground too: AGND and DGND joined by a ferrite are two nodes,
    /// the larger of them the reference.
    #[test]
    fn split_grounds_joined_by_a_ferrite_stay_split_without_a_canonical_ground() {
        let part = |r: &str, value: &str, nets: [i64; 2]| {
            let mut c = comp(r, value);
            c.lib_id = "Device:R".into();
            c.pins = vec![pin("1", nets[0]), pin("2", nets[1])];
            c
        };
        let board = ExtractedBoard {
            name: "split".into(),
            nets: vec![net(1, "DGND"), net(2, "AGND"), net(3, "+3V3")],
            components: vec![
                part("C1", "100n", [3, 1]),
                part("C2", "100n", [3, 1]),
                part("C3", "100n", [3, 2]),
                part("FB1", "FB", [1, 2]),
            ],
        };
        let bound = bind_board(&board, &ModelLibrary::builtin());
        assert_eq!(bound.node("DGND"), Some(NodeId::GROUND));
        assert_ne!(bound.node("AGND"), Some(NodeId::GROUND));
        assert!(bound.report.row("RAIL:DGND").is_none());
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

    /// An adjustable regulator's rail is its board divider: with one it binds;
    /// with only a bypass capacitor on ADJ, or ADJ unwired, it is left open
    /// with that reason rather than stamped at Vref.
    #[test]
    fn an_adjustable_regulator_without_a_divider_is_left_open() {
        let lm317 = |adj: Option<i64>| Component {
            footprint: "Package_TO_SOT_SMD:SOT-223-3_TabPin2".into(),
            pins: [
                adj.map(|n| pin("1", n)),
                Some(pin("2", 2)),
                Some(pin("3", 1)),
            ]
            .into_iter()
            .flatten()
            .collect(),
            ..comp("U1", "LM317")
        };
        let part = |reference: &str, value: &str, (a, b): (i64, i64)| Component {
            pins: vec![pin("1", a), pin("2", b)],
            ..comp(reference, value)
        };
        let outcome = |parts: Vec<Component>| {
            let board = ExtractedBoard {
                name: "adj".into(),
                nets: vec![net(1, "VIN"), net(2, "VOUT"), net(3, "ADJ"), net(4, "GND")],
                components: parts,
            };
            let bound = bind_board(&board, &ModelLibrary::builtin());
            let row = bound.report.row("U1").unwrap();
            (row.outcome.clone(), row.warning.clone())
        };
        let load = || part("RL", "500", (2, 4));
        // Legal control: R1 OUT-ADJ, R2 ADJ-GND.
        let (bound, warning) = outcome(vec![
            lm317(Some(3)),
            part("R1", "240", (2, 3)),
            part("R2", "720", (3, 4)),
            part("C1", "10uF", (3, 4)),
            load(),
        ]);
        assert!(
            !matches!(bound, BindOutcome::Unresolved { .. }),
            "{bound:?} / {warning:?}"
        );
        // Faulty: a capacitor alone on ADJ, and ADJ unwired.
        for (parts, why) in [
            (
                vec![lm317(Some(3)), part("C1", "10uF", (3, 4)), load()],
                "has no divider",
            ),
            (vec![lm317(None), load()], "ADJ/FB pin not connected"),
        ] {
            let (open, warning) = outcome(parts);
            assert!(
                matches!(&open, BindOutcome::Unresolved { reason } if reason.contains(why)),
                "{open:?}"
            );
            assert!(warning.is_some_and(|w| w.contains(why)));
        }
    }

    /// Bind one peripheral card on a board of SDA/+3V3/SCL/IRQ nets (board
    /// nets 0/1 ground, 2 SDA, 3 +3V3, 4 SCL, 5 a signal net).
    fn bind_peripheral(
        component: &Component,
        model: &ModelEntry,
    ) -> (BindOutcome, Option<String>, Vec<PeripheralBinding>) {
        bind_peripheral_pulled(component, model, &[])
    }

    /// [`bind_peripheral`] on a board that also carries the given parts
    /// (board nets 0/1 ground, 2 SDA, 3 +3V3, 4 SCL, 5 IRQ, 6 a spare net).
    fn bind_peripheral_pulled(
        component: &Component,
        model: &ModelEntry,
        others: &[Component],
    ) -> (BindOutcome, Option<String>, Vec<PeripheralBinding>) {
        let lib = ModelLibrary::builtin();
        let mut circuit = Circuit::new();
        let nodes = ["SDA", "+3V3", "SCL", "IRQ", "SPARE"].map(|name| circuit.node(name));
        let node_of = |n: Option<i64>| match n {
            Some(0 | 1) => Some(NodeId::GROUND),
            Some(n @ 2..=6) => nodes.get(usize::try_from(n - 2).ok()?).copied(),
            _ => None,
        };
        let board: Vec<Component> = std::iter::once(component.clone())
            .chain(others.iter().cloned())
            .collect();
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
            (&board, &HashMap::from([("+3V3".to_string(), 3.3)])),
            lib.pin_rules(),
        );
        (outcome, warning, peripherals)
    }

    fn pins_on(wiring: &[(&str, i64)]) -> Vec<Pin> {
        wiring.iter().map(|&(pad, net)| pin(pad, net)).collect()
    }

    /// A two-pad board part between nets `a` and `b`.
    fn two_pad(reference: &str, value: &str, (a, b): (i64, i64)) -> Component {
        Component {
            pins: pins_on(&[("1", a), ("2", b)]),
            ..comp(reference, value)
        }
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

    /// The bus-select strap resolves through the board's own parts: a 0 Ω
    /// link or a pull to the rail selects I2C, a tie to ground refuses, and a
    /// strap nothing holds (floating, or on a pin another part drives) is
    /// firmware's business, so the map attaches with the level disclosed.
    #[test]
    fn bus_select_strap_resolves_through_links_and_pulls() {
        let model = resolve_value(&ModelLibrary::builtin(), "BMA423", "");
        let mut component = comp("U6", "BMA423");
        // As above, but CSB (pad 10) on the spare net 6.
        component.pins = pins_on(&[
            ("1", 1),
            ("2", 2),
            ("3", 3),
            ("7", 3),
            ("8", 0),
            ("9", 0),
            ("10", 6),
            ("12", 4),
        ]);
        let bound = |others: &[Component]| {
            let (outcome, warning, peripherals) =
                bind_peripheral_pulled(&component, &model, others);
            assert!(
                matches!(outcome, BindOutcome::Behavioral { .. }),
                "{outcome:?} / {warning:?}"
            );
            assert_eq!(peripherals.len(), 1, "{warning:?}");
            (warning, peripherals.into_iter().next().unwrap())
        };
        // Legal: a 0R link or a 10k pull to the rail is a resolved high.
        for tie in [two_pad("R5", "0R", (6, 3)), two_pad("R5", "10k", (6, 3))] {
            let (warning, peripheral) = bound(&[tie]);
            assert_eq!(peripheral.straps_assumed, None);
            assert_eq!(warning, None);
        }
        // Floating: attached, with the assumption named.
        let (warning, peripheral) = bound(&[]);
        let why = peripheral.straps_assumed.expect("floating CSB is assumed");
        assert!(
            why.contains("csb high") && why.contains("floating"),
            "{why}"
        );
        assert!(warning.is_some_and(|w| w.contains(Assumption::PARTIAL_MODEL_MARKER)));
        // Driven by a controller pin: firmware-driven, not a fault.
        let mcu = Component {
            pins: pins_on(&[("1", 6), ("2", 3), ("3", 0)]),
            ..comp("U1", "ESP32-S3")
        };
        let (_, peripheral) = bound(&[mcu]);
        let why = peripheral.straps_assumed.expect("driven CSB is assumed");
        assert!(
            why.contains("U1") && why.contains("firmware-driven"),
            "{why}"
        );
        // Faulty: tied low through a link selects SPI, which the map does not
        // model, so it stays unattached; pulled both ways is unresolved.
        let (outcome, warning, peripherals) =
            bind_peripheral_pulled(&component, &model, &[two_pad("R5", "0R", (6, 0))]);
        assert!(!matches!(outcome, BindOutcome::Behavioral { .. }));
        assert!(peripherals.is_empty());
        assert!(warning.is_some_and(|w| w.contains("must resolve high")));
        let both = [two_pad("R5", "10k", (6, 3)), two_pad("R6", "10k", (6, 0))];
        let (_, peripheral) = bound(&both);
        assert!(peripheral
            .straps_assumed
            .is_some_and(|w| w.contains("pulled both high and low")));
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
        assert_eq!(peripherals[0].straps_assumed, None);
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

        // A strap on a signal net pulled to one rail takes that rail's level;
        // pulls to both rails leave it assumed.
        let (_, warning, peripherals) =
            bind_peripheral_pulled(&component, &model, &[two_pad("R1", "10k", (5, 3))]);
        assert_eq!(peripherals[0].i2c_address_override, Some(0x41));
        assert_eq!(warning, None);
        let (_, warning, _) = bind_peripheral_pulled(
            &component,
            &model,
            &[two_pad("R1", "10k", (5, 3)), two_pad("R2", "10k", (0, 5))],
        );
        assert!(warning.is_some_and(|w| w.contains("0x40 is assumed")));

        // Without SCL wired the bus personality is not attached.
        component.pins.retain(|p| p.number != "1");
        let (outcome, _, peripherals) = bind_peripheral(&component, &model);
        assert!(matches!(outcome, BindOutcome::Unresolved { .. }));
        assert!(peripherals.is_empty());
    }

    /// A four-level strap (GND, VS, SDA or SCL on each of A0 and A1) selects
    /// one of sixteen addresses; a strap on any other net binds the spec
    /// default, disclosed as assumed.
    #[test]
    fn four_level_address_straps_read_the_bus_lines_as_levels() {
        let model = resolve_value(&ModelLibrary::builtin(), "INA233", "");
        let mut component = comp("U3", "INA233");
        // A1, A0, ALERT -> IRQ, SDA, SCL, VS, GND, VBUS, IN-, IN+.
        let wire = |a1, a0| {
            pins_on(&[
                ("1", a1),
                ("2", a0),
                ("3", 5),
                ("4", 2),
                ("5", 4),
                ("6", 3),
                ("7", 0),
                ("8", 3),
                ("9", 3),
                ("10", 3),
            ])
        };
        for (a1, a0, address) in [
            (0, 0, 0x40),
            (0, 3, 0x41),
            (3, 0, 0x44),
            (3, 3, 0x45),
            (2, 4, 0x4B),
            (4, 2, 0x4E),
            (4, 4, 0x4F),
        ] {
            component.pins = wire(a1, a0);
            let (outcome, warning, peripherals) = bind_peripheral(&component, &model);
            assert!(
                matches!(outcome, BindOutcome::Behavioral { .. }),
                "{outcome:?} / {warning:?}"
            );
            assert_eq!(
                peripherals[0].i2c_address_override,
                Some(address),
                "a1 {a1} a0 {a0}"
            );
            assert_eq!(warning, None);
        }
        // Control: A0 on the alert line is none of the four levels.
        component.pins = wire(0, 5);
        let (_, warning, peripherals) = bind_peripheral(&component, &model);
        assert_eq!(peripherals[0].i2c_address_override, Some(0x40));
        assert!(warning.is_some_and(|w| w.contains("0x40 is assumed")));
    }

    /// The BME280 attaches its chip-id register only when CSB selects I2C, at
    /// the address SDO selects; CSB low (SPI) leaves it open.
    #[test]
    fn bme280_attaches_on_i2c_at_the_sdo_selected_address() {
        let model = resolve_value(&ModelLibrary::builtin(), "BME280", "");
        let mut component = comp("U4", "BME280");
        // GND, CSB, SDI, SCK, SDO, VDDIO, GND, VDD.
        let wire = |csb, sdo| {
            pins_on(&[
                ("1", 0),
                ("2", csb),
                ("3", 2),
                ("4", 4),
                ("5", sdo),
                ("6", 3),
                ("7", 0),
                ("8", 3),
            ])
        };
        for (sdo, address) in [(0, 0x76), (3, 0x77)] {
            component.pins = wire(3, sdo);
            let (outcome, warning, peripherals) = bind_peripheral(&component, &model);
            assert!(
                matches!(outcome, BindOutcome::Behavioral { .. }),
                "{outcome:?} / {warning:?}"
            );
            assert_eq!(peripherals[0].i2c_address_override, Some(address));
        }
        component.pins = wire(0, 0);
        let (outcome, _, peripherals) = bind_peripheral(&component, &model);
        assert!(
            matches!(outcome, BindOutcome::Unresolved { .. }),
            "{outcome:?}"
        );
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
            // DMG1013T: a lettered SOT523 top view on KiCad's SOT-523 footprint.
            (
                "Package_TO_SOT_SMD:SOT-523",
                letters,
                "",
                Some(NodeId(1)),
                3,
                rule,
            ),
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
            // AMS1117's 22 µF is a datasheet recommendation (p.4), not a minimum.
            "card | AMS1117-3.3 | 3.3 | OUT 0 | 1.3 | 4.699 | 15 | recommends 22 µF output capacitance",
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
            // A card with an enable pin has it tied to IN (on), so the rail is
            // the plain ungated stamp this test is about.
            let roles = HashMap::from([
                ("in".into(), circuit.node("IN")),
                ("en".into(), circuit.node("IN")),
                ("out".into(), circuit.node(out)),
                ("gnd".into(), circuit.node(gnd)),
            ]);
            let resolved =
                card.map(|v| lib.resolve(&ComponentQuery::new(None, Some(v.into()), None)));
            let model = resolved.as_ref().map_or_else(
                || simple_vreg(Some(vout)),
                |r| r.model.clone().expect("card resolves"),
            );
            let source = resolved.as_ref().and_then(|r| r.provenance.as_ref());
            let (outcome, warning, regs) = bind_u9(&model, source, &mut circuit, &roles);
            let marked =
                |w: &str| w.starts_with(Assumption::PARTIAL_MODEL_MARKER) && w.contains(token);
            let caveat = warning.as_deref().map(marked);
            assert_eq!(
                caveat,
                (token != "-").then_some(true),
                "{case}: {warning:?}"
            );
            assert!(
                matches!(outcome, BindOutcome::Behavioral { device } if device == format!("vreg {} source", format_volts(vout))),
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
            assert_eq!(
                (helpers[0], reg.pass_level.map(|l| l.0)),
                (2, Some(1)),
                "{case}"
            );
            let into_ret = |d: &Device| d.conduction_nodes() == [roles["in"], roles["gnd"]];
            assert!(circuit.devices[2..].iter().all(into_ret), "{case}");
            let Device::Behavioral { expr, .. } = &circuit.devices[2] else {
                panic!("{case}: the draw is a B-source");
            };
            let (amps, sign) = (0.25, vout.signum());
            let draw = |h: f64, time: f64| {
                let level = if time > 0.0 { amps } else { 0.0 };
                let branch = if time > 0.0 { 0.0 } else { -amps };
                expr.eval(&[level, branch, sign.mul_add(h, vout), vout], time)
                    .unwrap()
            };
            let full = dropout_v.unwrap_or(0.0);
            for time in [0.0, 1e-3] {
                assert!((draw(full, time) - amps).abs() < 1e-12, "{case} at {time}");
                assert!(
                    draw(0.1f64.mul_add(-full.max(0.1), full), time) < amps,
                    "{case} at {time}"
                );
                assert!(
                    draw(full - full.max(PASS_FADE_V), time).abs() < 1e-12,
                    "{case}"
                );
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
    fn a_comparator_with_a_datasheet_swing_sits_on_its_actual_supply() {
        let mut params = hauksbee_models::Params::default();
        for (k, v) in [
            ("out_lo", 0.05),
            ("out_hi", 4.95),
            ("hysteresis", 0.0075),
            ("swing_lo", 0.05),
            ("swing_hi", 0.05),
        ] {
            params.set_f64(k, v);
        }
        let model = make_entry(
            "swing_cmp",
            ComponentKind::Comparator,
            "push-pull comparator",
            params,
            std::collections::BTreeMap::new(),
        );
        let levels = |rails: &[(&str, f64)]| {
            let mut circuit = Circuit::new();
            let roles = HashMap::from([
                ("out".to_string(), circuit.node("OUT")),
                ("in_plus".to_string(), circuit.node("P")),
                ("in_minus".to_string(), circuit.node("N")),
                ("vcc".to_string(), circuit.node("VDD_A")),
                ("vss".to_string(), NodeId::GROUND),
            ]);
            let power_nets = rails.iter().map(|(n, v)| ((*n).to_string(), *v)).collect();
            let _ = bind_comparator(
                &comp("U1", "LMV7219"),
                &model,
                &mut circuit,
                &roles,
                &power_nets,
            );
            circuit
                .devices
                .iter()
                .find_map(|d| match d {
                    Device::Comparator { out_lo, out_hi, .. } => Some((*out_lo, *out_hi)),
                    _ => None,
                })
                .expect("a comparator is stamped")
        };
        let (lo, hi) = levels(&[("VDD_A", 4.5)]);
        assert!(
            (hi - 4.45).abs() < 1e-12 && (lo - 0.05).abs() < 1e-12,
            "{lo} {hi}"
        );
        // An unknown supply voltage keeps the card's printed levels.
        assert_eq!(levels(&[]), (0.05, 4.95));
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
        let metas = gather_device_meta(&board, &lib, None, enabled, &node_of, &[], &[], &[]);
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
            "Package_TO_SOT_SMD:SOT-523",
            "SOT523",
            "Package_TO_SOT_SMD:SOT-416",
            "SC-75",
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

    /// A TO-252 whose tab is a fourth pad numbered 2 (and a mechanical pad with
    /// no number) is a three-pad part: the three-terminal rule reads it.
    #[test]
    fn a_tab_sharing_a_lead_number_does_not_hide_the_three_terminal_rule() {
        let lib = ModelLibrary::builtin();
        let model = make_entry(
            "fixture",
            ComponentKind::Nmos,
            "",
            hauksbee_models::Params::default(),
            std::collections::BTreeMap::new(),
        );
        let part = |pads: &[(&str, i64)]| {
            let mut c = comp("Q1", "FIXTURE");
            c.footprint = "Package_TO_SOT_SMD:TO-252-3_TabPin2".into();
            c.pins = pins_on(pads);
            role_nets(&c, &model, lib.pin_rules())
        };
        let roles = part(&[("1", 1), ("2", 2), ("3", 3), ("2", 2), ("", 4)]);
        assert_eq!(roles.get("gate"), Some(&1), "{roles:?}");
        assert_eq!(roles.get("drain"), Some(&2), "{roles:?}");
        assert_eq!(roles.get("source"), Some(&3), "{roles:?}");
        // A fourth numbered pad is a four-pad part, which no rule reads.
        assert!(part(&[("1", 1), ("2", 2), ("3", 3), ("4", 4)]).is_empty());
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

    /// A regulator whose entry names an enable, wired OFF, says the board holds
    /// it off and its rail is unpowered; a three-terminal regulator stays silent.
    #[test]
    fn a_regulator_wired_off_says_its_rail_is_held_off_and_a_plain_one_says_nothing() {
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
                let w = warning.unwrap_or_else(|| panic!("{value} must say it is held off"));
                assert!(
                    w.contains("held off: its EN pin is tied to GND") && w.contains("unpowered"),
                    "{w}"
                );
                assert!(
                    !w.contains("not modelled"),
                    "the enable is modelled now: {w}"
                );
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

    /// A plain two-pad part the record calls an LED by symbol, package or the bare
    /// value "LED" binds the ASSUMED forward-voltage corner set as a guess, whatever
    /// its designator and even on a header footprint; a colour on a name-style
    /// designator takes the colour card. A signal diode, a TVS, an infrared LED, a
    /// multi-die part and a card that names the part never take the set.
    #[test]
    fn an_obvious_led_binds_the_generic_corner_set_and_a_diode_never_does() {
        let lib = ModelLibrary::builtin();
        let res = |r: &str, value: &str, lib_id: &str, fp: &str, pads: i64| {
            let mut c = comp(r, value);
            (c.lib_id, c.footprint) = (lib_id.into(), fp.into());
            c.pins = (1..=pads).map(|n| pin(&n.to_string(), n)).collect();
            library_resolution(&lib, &c)
        };
        let corners =
            |r: &hauksbee_models::Resolution| r.selection.as_ref().is_some_and(|s| s.corners);
        let header = "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical";
        for (r, value, lib_id, fp) in [
            // A panel-mount LED wired through a header.
            ("D1", "LED", "Device:LED", header),
            // An Eagle CHIPLED package under a name-style designator.
            ("STAT", "", "", "adafruit:CHIPLED_0603"),
            ("CHG", "CHG", "", "CHIPLED_0805_NOOUTLINE"),
            // A KiCad PCM library footprint and an old KiCad 4 LED library.
            ("PWR", "PWR", "", "PCM_LED_SMD_AKL:LED_0805_2012Metric"),
            ("D5", "LED", "", "LEDs:LED_0603"),
            ("LED2", "+3V3", "", "LED_THT:LED_D5.0mm"),
            // The value alone.
            ("D7", "led", "", "Custom:FOOT2"),
        ] {
            // The checks that trace indicators read the same part as an LED.
            let mut c = comp(r, value);
            (c.lib_id, c.footprint) = (lib_id.into(), fp.into());
            assert!(hauksbee_extract::is_led(&c), "{r} {fp}: not seen as an LED");
            let got = res(r, value, lib_id, fp, 2);
            let model = got.model.as_ref().unwrap_or_else(|| panic!("{r} {fp}"));
            assert_eq!(model.id, "led_generic_corner_high_vf", "{r} {fp}");
            assert!(corners(&got), "{r}: {:?}", got.selection);
            assert_eq!(got.confidence, Confidence::Guessed);
            let s = got.selection.as_ref().expect("a corner set");
            assert!(
                s.determined.is_none() && s.members.len() == 2,
                "{}",
                s.listing()
            );
        }
        // A name-style designator with a colour takes the colour card.
        let colour = res("TXLED", "YELLOW", "", "CHIPLED_0805_NOOUTLINE", 2);
        assert_eq!(colour.model.map(|m| m.id).as_deref(), Some("led_yellow"));
        // Controls: a signal diode, a TVS, an infrared LED, a multi-die LED, an IC
        // designator, an OLED module and a named diode card never take the set.
        for (r, value, lib_id, fp, pads) in [
            ("D3", "1N4148", "Device:D", "Diode_SMD:D_SOD-123", 2),
            ("D4", "SMBJ5.0A", "Device:D_TVS", "Diode_SMD:D_SMB", 2),
            ("D6", "D", "Device:D", "Diode_SMD:D_SOD-323", 2),
            (
                "D8",
                "IR LED",
                "Device:LED",
                "LED_THT:LED_D5.0mm_IRBlack",
                2,
            ),
            ("D9", "LED", "Device:LED_RGB", "LED_SMD:LED_RGB_5050-6", 4),
            ("U1", "LED", "", "LED_SMD:LED_0603_1608Metric", 2),
            ("DS1", "SSD1306", "", "Display:OLED-128O064D", 2),
            (
                "D10",
                "1N4148W",
                "Device:D",
                "LED_SMD:LED_0603_1608Metric",
                2,
            ),
        ] {
            let got = res(r, value, lib_id, fp, pads);
            assert!(
                !got.model.as_ref().is_some_and(is_generic_led) && !corners(&got),
                "{r} {value}: {:?}",
                got.model.map(|m| m.id)
            );
        }
    }

    /// A behaviorless `digital` entry stays open and explains why.
    #[test]
    fn a_logic_entry_that_models_only_a_pad_map_stays_open() {
        let lib = ModelLibrary::builtin();
        // The card's LDO law and input pulls are its only behavior; with them
        // removed it is exactly a pad map plus levels.
        let mut model = resolve_value(&lib, "FT231X-Q", "");
        model.params.0.remove("identity_only");
        model.behavioral = hauksbee_models::behavioral::Behavioral::default();
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

    /// A bus-powered FT231XQ (VCC on +5V, VCCIO and RESET# on its own 3V3OUT,
    /// `FT_000565` Table 3.1) binds, and its internal LDO is the supply of the
    /// 3V3OUT rail, so that rail takes no name-derived ideal source while +5V
    /// does.
    #[test]
    fn ft231x_binds_and_its_3v3out_ldo_supplies_the_io_rail() {
        let mut u1 = comp("U1", "FT231XQ");
        u1.footprint = "Package_DFN_QFN:QFN-20-1EP_4x4mm_P0.5mm_EP2.5x2.5mm".into();
        u1.pins = vec![
            pin("1", 4),
            pin("3", 3),
            pin("10", 2),
            pin("11", 2),
            pin("12", 1),
            pin("13", 3),
            pin("17", 5),
            pin("20", 2),
            pin("21", 3),
        ];
        let board = ExtractedBoard {
            name: "ft231x-bus-powered".into(),
            nets: vec![
                net(1, "+5V"),
                net(2, "+3V"),
                net(3, "GND"),
                net(4, "UART_RX"),
                net(5, "UART_TX"),
            ],
            components: vec![u1],
        };
        let bound = bind_board(&board, &ModelLibrary::builtin());
        let row = bound.report.row("U1").unwrap();
        assert_eq!(row.model_id.as_deref(), Some("ft231x"));
        assert!(
            !matches!(row.outcome, BindOutcome::Unresolved { .. }),
            "{:?}",
            row.outcome
        );
        let rail = bound.circuit.find_node("+3V").unwrap();
        let ldo = bound.behavioral.iter().find(|b| b.reference == "U1");
        assert!(
            ldo.is_some_and(|b| b.driven_nodes().contains(&rail)),
            "3V3OUT must drive its rail"
        );
        let supplied: Vec<&str> = bound.supplies.iter().map(|s| s.net_name.as_str()).collect();
        assert!(supplied.contains(&"+5V"), "{supplied:?}");
        assert!(!supplied.contains(&"+3V"), "{supplied:?}");
    }

    /// A self-powered CP2102N QFN28 (VDD and VREGIN on one 3.3 V rail, Silicon
    /// Labs CP2102N Rev. 1.5 Figure 2.3) binds by its Figure 1.1 ordering code
    /// and the library spelling, and claims no rail of its own: its internal
    /// regulator is unmodelled, so the 3.3 V rail keeps its own supply.
    #[test]
    fn cp2102n_qfn28_binds_without_claiming_its_vdd_rail() {
        for value in ["CP2102N-A02-GQFN28R", "CP2102N-Axx-xQFN28"] {
            let mut u1 = comp("U1", value);
            u1.pins = vec![
                pin("3", 3),
                pin("6", 2),
                pin("7", 2),
                pin("8", 1),
                pin("19", 4),
                pin("25", 5),
                pin("26", 6),
                pin("29", 3),
            ];
            let board = ExtractedBoard {
                name: "cp2102n-self-powered".into(),
                nets: vec![
                    net(1, "VBUS_DIV"),
                    net(2, "+3V3"),
                    net(3, "GND"),
                    net(4, "TX_LED"),
                    net(5, "UART_TX"),
                    net(6, "UART_RX"),
                ],
                components: vec![u1],
            };
            let bound = bind_board(&board, &ModelLibrary::builtin());
            let row = bound.report.row("U1").unwrap();
            assert_eq!(row.model_id.as_deref(), Some("cp2102n_qfn28"), "{value}");
            assert!(
                !matches!(row.outcome, BindOutcome::Unresolved { .. }),
                "{value}: {:?}",
                row.outcome
            );
            let rail = bound.circuit.find_node("+3V3").unwrap();
            assert!(
                bound
                    .behavioral
                    .iter()
                    .filter(|b| b.reference == "U1")
                    .all(|b| !b.driven_nodes().contains(&rail)),
                "{value}: the part must not drive VDD"
            );
            let supplied: Vec<&str> = bound.supplies.iter().map(|s| s.net_name.as_str()).collect();
            assert!(supplied.contains(&"+3V3"), "{value}: {supplied:?}");
        }
        // The QFN24 and QFN20 members number their pads differently.
        let lib = ModelLibrary::builtin();
        for value in ["CP2102N-A02-GQFN24", "CP2102N-A02-GQFN20"] {
            let mut q = ComponentQuery::new(None, Some(value.to_string()), None);
            q.mpn = Some(value.to_string());
            let id = lib.resolve(&q).model.map(|m| m.id);
            assert_ne!(id.as_deref(), Some("cp2102n_qfn28"), "{value}");
        }
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

    /// One symbol drawn for a fixed and an adjustable part (`ADJ/GND`) names
    /// the alternative the card gives the pad, and nothing when the card gives
    /// it neither.
    #[test]
    fn a_shared_pin_name_reads_as_the_alternative_the_card_gives_the_pad() {
        let role = |f: &str, card: Option<&str>| {
            role_from_shared_pinfunction(ComponentKind::Vreg, f, card.map(str::to_owned))
        };
        assert_eq!(role("ADJ/GND", Some("gnd")).as_deref(), Some("gnd"));
        assert_eq!(role("ADJ/GND", Some("fb")).as_deref(), Some("fb"));
        assert_eq!(role("ADJ/GND", Some("out")), None);
        assert_eq!(role("ADJ/GND", None), None);
        assert_eq!(role("VIN", Some("gnd")).as_deref(), Some("in"));
        assert_eq!(role("GND", Some("gnd")).as_deref(), Some("gnd"));
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
        assert!(bound.mcus[0].clock.external_present());
        let bound = bind_board(&stm32_clock_board(true), &lib);
        assert!(!bound.mcus[0].clock.external_present());
    }

    /// An ESP32 module drawn as the Olimex ESP32-PoE does: pad 26 is GPIO4 and
    /// pad 6 is GPIO34 whose pin name lists its RTC channel `RTC_GPIO4`, so both
    /// pads read as port pin 04. The roles were walked in hash order, so which
    /// pad got the driver (and so which net the checks examined for GPIO4)
    /// changed from run to run on the same file.
    #[test]
    fn two_pads_reaching_one_port_pin_give_it_to_the_same_pad_every_run() {
        let lib = ModelLibrary::builtin();
        let named = |number: &str, function: &str, net: i64| {
            let mut p = pin(number, net);
            p.function = function.to_string();
            p
        };
        let mut module = comp("U6", "ESP32-WROOM-32E");
        module.footprint = "RF_Module:ESP32-WROOM-32".to_string();
        module.pins = vec![
            named("1", "GND", 3),
            named("2", "3V3", 4),
            named("6", "GPI34/ADC1_CH6/RTC_GPIO4", 1),
            named("26", "GPIO4/ADC2_CH0/TOUCH0/RTC_GPIO10/HSPIHD", 2),
        ];
        let board = ExtractedBoard {
            name: "gpio-collision".to_string(),
            nets: vec![
                net(1, "GPI34_BUT1"),
                net(2, "GPIO4_U1TXD"),
                net(3, "GND"),
                net(4, "+3V3"),
            ],
            components: vec![module],
        };
        let runs: Vec<Vec<((char, u8), String)>> = (0..40)
            .map(|_| {
                let bound = bind_board(&board, &lib);
                assert_eq!(bound.mcus.len(), 1, "the module binds as an MCU");
                let net_name = |n: NodeId| {
                    bound
                        .net_nodes
                        .iter()
                        .find(|(_, &node)| node == n)
                        .map(|(name, _)| name.clone())
                        .unwrap_or_default()
                };
                let mut drivers: Vec<_> = (bound.mcus[0].gpio_drivers.iter())
                    .map(|(&pb, d)| (pb, net_name(d.net)))
                    .collect();
                drivers.sort();
                drivers
            })
            .collect();
        for run in &runs {
            assert_eq!(run, &runs[0], "the pins driven must not vary between runs");
        }
        // The pad that names GPIO4 as its own function owns port pin 04.
        assert!(
            runs[0].contains(&(('0', 4), "GPIO4_U1TXD".to_string())),
            "{:?}",
            runs[0]
        );
    }

    /// STM32F103 on `OSC_IN`/`OSC_OUT` (nets 1/2) with `extra` parts, ground net 3.
    fn clock_case(extra: Vec<Component>) -> BoundBoard {
        let mut board = stm32_clock_board(false);
        board.components.truncate(1);
        board.components.extend(extra);
        board.nets.push(net(4, "XO_FLOAT"));
        board.nets.push(net(5, "+3V3"));
        bind_board(&board, &ModelLibrary::builtin())
    }

    fn crystal(pins: Vec<Pin>) -> Component {
        let mut y = comp("Y1", "8MHz");
        y.pins = pins;
        y
    }

    fn clock_of(bound: &BoundBoard) -> (ClockKind, String) {
        let c = &bound.mcus[0].clock;
        (c.kind, c.detail.clone())
    }

    #[test]
    fn crystal_across_the_oscillator_pins_is_a_valid_clock() {
        let bound = clock_case(vec![crystal(vec![pin("1", 1), pin("2", 2), pin("3", 3)])]);
        let (kind, detail) = clock_of(&bound);
        assert_eq!(kind, ClockKind::Crystal, "{detail}");
        assert!(detail.contains("Y1"), "{detail}");
        assert!(bound.mcus[0].clock.external_present());
    }

    #[test]
    fn an_mcu_with_nothing_on_its_oscillator_pins_runs_from_its_internal_oscillator() {
        let bound = clock_case(vec![]);
        let (kind, detail) = clock_of(&bound);
        assert_eq!(kind, ClockKind::Internal, "{detail}");
        assert!(detail.contains("HSI"), "{detail}");
        assert!(!bound.mcus[0].clock.external_present());
        // A DNP crystal is absent: the same internal-only verdict.
        let mut y = crystal(vec![pin("1", 1), pin("2", 2)]);
        y.dnp = true;
        assert_eq!(clock_of(&clock_case(vec![y])).0, ClockKind::Internal);
    }

    #[test]
    fn an_oscillator_driving_the_input_pin_is_a_valid_external_clock() {
        let mut osc = comp("X1", "8MHz");
        let mut out = pin("3", 1);
        out.kind = "output".to_string();
        osc.pins = vec![pin("2", 3), out, pin("4", 5)];
        let (kind, detail) = clock_of(&clock_case(vec![osc]));
        assert_eq!(kind, ClockKind::External, "{detail}");
    }

    #[test]
    fn a_crystal_with_an_open_leg_is_a_broken_clock_and_is_disclosed() {
        let bound = clock_case(vec![crystal(vec![pin("1", 1), pin("2", 4), pin("3", 3)])]);
        let (kind, detail) = clock_of(&bound);
        assert_eq!(kind, ClockKind::Broken, "{detail}");
        assert!(detail.contains("Y1 reaches only"), "{detail}");
        assert!(!bound.mcus[0].clock.external_present());
        let row = bound
            .report
            .rows
            .iter()
            .find(|r| r.reference == "U1")
            .unwrap();
        assert!(row
            .warning
            .as_deref()
            .unwrap()
            .contains("clock source broken"));
    }

    /// The RP2040 hardware guide puts 1 kOhm between XOUT and the crystal
    /// (`SparkFun` Thing Plus R11): the crystal still spans the pin pair.
    #[test]
    fn a_crystal_reaching_one_pin_through_a_series_resistor_is_a_valid_clock() {
        let mut r = comp("R11", "1k");
        r.pins = vec![pin("1", 4), pin("2", 2)];
        let y = crystal(vec![pin("1", 1), pin("2", 4), pin("3", 3)]);
        let (kind, detail) = clock_of(&clock_case(vec![y.clone(), r.clone()]));
        assert_eq!(kind, ClockKind::Crystal, "{detail}");
        assert!(detail.contains("through series R11"), "{detail}");
        // A resistor from the pin to a rail is a load, not a series leg.
        r.pins = vec![pin("1", 5), pin("2", 2)];
        let (kind, detail) = clock_of(&clock_case(vec![y.clone(), r.clone()]));
        assert_eq!(kind, ClockKind::Broken, "{detail}");
        // A DNP series resistor leaves the leg open.
        r.pins = vec![pin("1", 4), pin("2", 2)];
        r.dnp = true;
        let (kind, detail) = clock_of(&clock_case(vec![y, r]));
        assert_eq!(kind, ClockKind::Broken, "{detail}");
    }

    #[test]
    fn a_clock_pin_tied_to_a_rail_is_a_broken_clock() {
        let mut board = stm32_clock_board(false);
        board.components[0].pins = vec![pin("5", 3), pin("6", 2)];
        let bound = bind_board(&board, &ModelLibrary::builtin());
        let (kind, detail) = clock_of(&bound);
        assert_eq!(kind, ClockKind::Broken, "{detail}");
        assert!(detail.contains("tied to"), "{detail}");
        // The same pin shorted to the output pin is also a fault.
        board.components[0].pins = vec![pin("5", 1), pin("6", 1)];
        assert_eq!(
            clock_of(&bind_board(&board, &ModelLibrary::builtin())).0,
            ClockKind::Broken
        );
    }

    #[test]
    fn load_capacitors_without_a_recognised_source_leave_the_clock_unknown() {
        let mut c1 = comp("C1", "22pF");
        c1.pins = vec![pin("1", 1), pin("2", 3)];
        let (kind, _) = clock_of(&clock_case(vec![c1]));
        assert_eq!(kind, ClockKind::Unknown);
    }

    #[test]
    fn oscillator_pins_are_found_from_schematic_names_on_a_part_without_a_card_pin_map() {
        let mut board = stm32_clock_board(false);
        let mcu = &mut board.components[0];
        mcu.value = "STM32F411RET6".to_string();
        mcu.pins = vec![pin("5", 1), pin("6", 2)];
        mcu.pins[0].function = "PH0-OSC_IN".to_string();
        mcu.pins[1].function = "PH1-OSC_OUT".to_string();
        let bound = bind_board(&board, &ModelLibrary::builtin());
        assert_eq!(bound.mcus.len(), 1);
        assert_eq!(clock_of(&bound).0, ClockKind::Crystal);
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

    /// Eagle names polarised capacitors `PC`, and their values carry the
    /// rated voltage; a `PC` photocoupler or an unreadable value stays open.
    #[test]
    fn a_polarised_pc_capacitor_binds_with_its_rated_voltage() {
        let part = |reference: &str, value: &str, pads: &[(&str, i64)]| Component {
            pins: pins_on(pads),
            footprint: "PANASONIC_D".into(),
            ..comp(reference, value)
        };
        let two = [("+", 3), ("-", 0)];
        for (value, farads, volts) in [
            ("47uF+/6.3V+", 47e-6, Some(6.3)),
            ("47uF/25V", 47e-6, Some(25.0)),
            ("10uF/16V", 10e-6, Some(16.0)),
            ("47u", 47e-6, None),
        ] {
            let c = part("PC1", value, &two);
            assert_eq!(passive_letter(&c), Some('C'), "{value}");
            let entry = fallback_entry(&c).expect("a PC capacitor binds");
            assert_eq!(entry.kind, ComponentKind::Passive);
            let parsed = parse_value(&c.value).unwrap();
            assert!((parsed.si - farads).abs() < 1e-9, "{value}");
            assert_eq!(hauksbee_models::value::parse_voltage_rating_v(value), volts);
        }
        // Controls: a four-pin photocoupler, a part-number value, no value.
        let opto = part("PC1", "PC817", &[("1", 2), ("2", 0), ("3", 4), ("4", 0)]);
        assert_eq!(passive_letter(&opto), None);
        assert_eq!(passive_letter(&part("PC2", "PC817", &two)), None);
        assert_eq!(passive_letter(&part("PC3", "CAP", &two)), None);
        assert!(fallback_entry(&part("PC3", "CAP", &two)).is_none());
    }

    /// A Zener bound by the fallback clamps at the voltage its value states,
    /// not at a signal diode's 110 V; one whose voltage the board does not
    /// state stays unmodelled; a rating string on a rectifier is not a Zener.
    #[test]
    fn zener_fallback_keeps_the_stated_breakdown() {
        let diode = |value: &str, lib_id: &str| Component {
            footprint: "Diode_SMD:D_SOD-123".into(),
            lib_id: lib_id.into(),
            pins: vec![pin("1", 1), pin("2", 2)],
            ..comp("D3", value)
        };
        for (value, lib_id, vz) in [
            ("18V Zener 200mW", "Device:D", 18.0),
            ("10V Zener 200mW", "Device:D", 10.0),
            ("5.1V", "Device:D", 5.1),
            ("5V1", "Device:D_Zener", 5.1),
        ] {
            let entry = fallback_entry(&diode(value, lib_id)).expect("a stated Zener binds");
            assert_eq!(entry.kind, ComponentKind::Diode, "{value}");
            let bv = entry.params.get_f64("bv").expect("bv");
            assert!(
                (bv - vz).abs() < 1e-9,
                "{value}: breakdown {bv} V, want {vz} V"
            );
            assert!(
                entry.params.get_f64("ibv").is_some_and(|i| i > 0.0),
                "{value}: knee"
            );
        }
        // Marked as a Zener, voltage not stated: not guessed.
        assert!(fallback_entry(&diode("BZT52C18", "Device:D_Zener")).is_none());
        assert!(fallback_entry(&diode("Zener", "Device:D")).is_none());
        // Controls: a generic diode keeps the signal-diode card.
        for (value, lib_id) in [("D", "Device:D"), ("1N4148W", "Device:D")] {
            let entry = fallback_entry(&diode(value, lib_id)).expect("binds");
            assert_eq!(entry.id, "signal_diode_1n4148_fallback", "{value}");
            // No invented breakdown, and a stand-in blocking floor instead.
            assert_eq!(entry.params.get_f64("bv"), None, "{value}");
            assert_eq!(
                entry.ratings.max_voltage_v,
                Some(UNKNOWN_DIODE_VRRM_FLOOR_V),
                "{value}"
            );
            // And a stand-in forward-current floor.
            assert_eq!(
                entry.ratings.max_current_a,
                Some(UNKNOWN_DIODE_IF_FLOOR_A),
                "{value}"
            );
        }
    }

    /// A TVS named by its family stands off the voltage its part number
    /// gives, as a fallback card's (stand-in) rating, with no clamp invented.
    #[test]
    fn a_tvs_fallback_takes_its_standoff_from_the_part_number() {
        let diode = |value: &str| Component {
            footprint: "Diode_SMD:D_SMA".into(),
            lib_id: "Device:D_TVS".into(),
            pins: vec![pin("1", 1), pin("2", 2)],
            ..comp("D4", value)
        };
        for (value, standoff) in [
            ("SMAJ5.0A", 5.0),
            ("SMBJ24A", 24.0),
            // Named by breakdown: the lowest standoff that breakdown gives.
            (
                "P6KE15A",
                15.0 * hauksbee_models::value::TVS_STANDOFF_PER_BREAKDOWN,
            ),
        ] {
            let entry = fallback_entry(&diode(value)).expect("a TVS binds");
            assert_eq!(entry.id, "tvs_diode_fallback", "{value}");
            assert_eq!(entry.params.get_f64("bv"), None, "{value}");
            let got = entry.ratings.max_voltage_v.expect("standoff");
            assert!((got - standoff).abs() < 1e-9, "{value}: {got} V");
        }
        // A bidirectional TVS has no stand-in; a rectifier keeps its own path.
        assert!(fallback_entry(&diode("SMBJ6.0CA")).is_none());
        let ss = fallback_entry(&diode("SS14")).map(|e| e.id);
        assert_ne!(ss.as_deref(), Some("tvs_diode_fallback"));
    }

    /// Only a field named for the rated voltage states it; one that merely
    /// mentions a voltage does not.
    #[test]
    fn a_capacitor_rating_is_read_only_from_a_rated_voltage_field() {
        let cap = |key: &str, text: &str| Component {
            properties: vec![(key.to_string(), text.to_string())],
            ..comp("C1", "10uF")
        };
        for (key, text, want) in [
            ("Voltage", "25V", 25.0),
            ("Rated Voltage", "16", 16.0),
            ("voltage_rating", "50 V", 50.0),
            ("  WV ", "35", 35.0),
            ("Rating", "25V", 25.0),
        ] {
            assert_eq!(
                capacitor_stated_voltage(&cap(key, text)),
                Some(want),
                "{key}"
            );
        }
        for (key, text) in [
            ("Test Voltage", "10V"),
            ("Voltage Coefficient", "-80%/V"),
            ("Ripple Voltage", "1V"),
            ("Rating", "0.25"),
            ("Breakdown Voltage", "100"),
        ] {
            assert_eq!(capacitor_stated_voltage(&cap(key, text)), None, "{key}");
        }
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
        // The binder still binds such a part, as the generic LED corner set.
        let mut led = comp("D9", "unknown");
        led.footprint = "LED_SMD:LED_0603".into();
        led.pins = vec![pin("1", 1), pin("2", 2)];
        let res = library_resolution(&ModelLibrary::builtin(), &led);
        assert_eq!(
            res.model.map(|m| m.id).as_deref(),
            Some("led_generic_corner_high_vf")
        );
    }

    #[test]
    fn engineering_values_scale_to_the_magnitude_and_carry_across_decades() {
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
            (f64::NAN, "Ω", "not a number"),
            (-0.0, "Ω", "0 Ω"),
            (999_600.0, "Ω", "1 MΩ"),
            (999.6, "Ω", "1 kΩ"),
            (0.9996, "Ω", "1 Ω"),
            (999.6e-6, "F", "1 mF"),
            (990.0, "Ω", "990 Ω"),
            (2_200_000_000.0, "Ω", "2.2 GΩ"),
        ] {
            assert_eq!(format_engineering_web(value, unit), want);
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

    /// A prefixed function name never becomes a bare GPIO: its number matches
    /// the pad's GPIO on the ESP32-S3 and C6 but not on the ESP32, where the
    /// pad `GPI34/ADC1_CH6/RTC_GPIO4` is GPIO34, so the name alone can't decide.
    /// `ADC1_CH6` / `TOUCH0` name no GPIO. Only the pad's own unprefixed name
    /// (`GPIO4`, `IO36`, `PA9`) is its GPIO, wherever it sits in the list.
    #[test]
    fn a_prefixed_function_name_never_becomes_a_bare_gpio() {
        for (function, want) in [
            ("GPI34/ADC1_CH6/RTC_GPIO4", None),
            ("ADC1_CH6/RTC_GPIO4", None),
            ("RTC_GPIO4", None),
            ("LP_GPIO4", None),
            ("LP_IO3", None),
            ("ADC1_CH6", None),
            ("TOUCH0", None),
            ("TOUCH0/RTC_GPIO10", None),
            ("GPIO4/ADC2_CH0/TOUCH0/RTC_GPIO10/HSPIHD", Some("gpio4")),
            ("RTC_GPIO10/GPIO4", Some("gpio4")),
            ("ADC2_CH0 GPIO4", Some("gpio4")),
            ("IO36/SVP", Some("gpio36")),
            ("PA9/USART1_TX", Some("pa9_txd")),
        ] {
            assert_eq!(
                role_from_mcu_pinfunction(function).as_deref(),
                want,
                "{function}"
            );
        }
    }

    /// The smartknob T-Micro32 module (2026-10-01 sweep): its unconnected
    /// `VDD_SDIO` pad, the flash LDO's output, read as a dead supply.
    #[test]
    fn regulator_output_pads_are_not_supply_inputs() {
        for (function, want) in [
            ("VDD_SDIO", None),
            ("VDD_SPI", None),
            ("VDDCORE", None),
            ("3V3", Some("vdd")),
            ("VDD3P3_RTC", Some("vdd")),
            ("3V3_EN", None),
        ] {
            assert_eq!(
                role_from_mcu_pinfunction(function).as_deref(),
                want,
                "{function}"
            );
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

    /// Module value strings route through `db/mcu_modules.toml` to the MCU
    /// they carry, whether spelled as a value or inside a library id.
    #[test]
    fn module_aliases_route_to_the_family_of_the_mcu_they_carry() {
        let esp32 = Some(McuFamilyRoute::Backend {
            family: "ESP32",
            backend: "qemu:esp32",
        });
        let nrf = Some(McuFamilyRoute::Backend {
            family: "nRF52",
            backend: "renode:nrf52840",
        });
        assert_eq!(route_mcu_family_str("T-Micro32_Plus"), esp32);
        assert_eq!(route_mcu_family_str("ESP32_WROOM32_SKINNY"), esp32);
        assert_eq!(route_mcu_family_str("nice_nano"), nrf);
        assert_eq!(route_mcu_family_str("MCU_Module:nice_nano"), nrf);
        assert_eq!(
            route_mcu_family_str("XIAO_ESP32C6"),
            Some(McuFamilyRoute::NoPlatform { family: "ESP32-C6" })
        );
        assert_eq!(route_mcu_family_str("NANO_HEADER"), None);
    }

    /// A part's MCU identity comes from its own value, MPN and symbol name,
    /// never from the library nickname: a board named `ESP32-PoE` puts every
    /// part in a `ESP32-PoE_Rev_M1-cache` library, which must not turn an
    /// inverter or a `PoE` controller into an ESP32.
    #[test]
    fn a_library_nickname_naming_a_family_does_not_make_a_part_an_mcu() {
        for (value, lib_id) in [
            ("SN74LVC1G04DBVR", "ESP32-PoE_Rev_M1-cache:SN74LVC1G04DBVR"),
            ("TPS2375D", "ESP32-PoE_Rev_M1-cache:TPS2375D"),
            ("TPS2375D", "STM32F4_Discovery:TPS2375D"),
        ] {
            let mut part = comp("U8", value);
            part.lib_id = lib_id.into();
            assert_eq!(route_mcu_family(&part), None, "{lib_id}");
            assert!(mcu_identity_strings(&part)
                .iter()
                .all(|s| !s.contains("CACHE") && !s.contains("ESP32") && !s.contains("STM32")));
        }
        // The symbol name still identifies a real module, whatever the nickname.
        let mut module = comp("U6", "");
        module.lib_id = "ESP32-PoE_Rev_M1-cache:ESP32-WROVER-E".into();
        assert!(matches!(
            route_mcu_family(&module),
            Some(McuFamilyRoute::Backend {
                backend: "qemu:esp32",
                ..
            })
        ));
        // And a module value still routes on its own.
        let mut module = comp("U6", "ESP32-WROVER-E");
        module.lib_id = "ESP32-PoE_Rev_M1-cache:RF_Module".into();
        assert!(route_mcu_family(&module).is_some());
        assert_eq!(symbol_name("MCU_Module:nice_nano"), "nice_nano");
        assert_eq!(symbol_name("bare_symbol"), "bare_symbol");
    }

    /// The alias table alone binds a module on an `X` refdes: no per-module
    /// card is needed, and the card, where one exists, agrees with the alias.
    #[test]
    fn an_aliased_x_module_binds_without_a_card_and_agrees_with_its_card() {
        let builtin = ModelLibrary::builtin();
        for (value, backend) in [
            ("nice_nano", "renode:nrf52840"),
            ("T-Micro32_Plus", "qemu:esp32"),
            ("ESP32_WROOM32_SKINNY", "qemu:esp32"),
        ] {
            let mut module = comp("X1", value);
            module.pins = vec![named_pin("1", "GND", 9), named_pin("2", "3V3", 8)];
            let board = ExtractedBoard {
                name: "alias".to_string(),
                nets: vec![net(8, "+3V3"), net(9, "GND")],
                components: vec![module],
            };
            for lib in [&ModelLibrary::empty(), &builtin] {
                let bound = bind_board(&board, lib);
                let refs: Vec<_> = bound
                    .mcus
                    .iter()
                    .map(|m| (m.reference.as_str(), m.backend.as_str()))
                    .collect();
                assert_eq!(refs, [("X1", backend)], "{value}");
            }
        }
    }

    /// Module header pins are named by rail, not chip role; those names count
    /// as supply pins when judging whether a pinless module card can be
    /// powered. Signal and ground names do not.
    #[test]
    fn module_rail_pin_names_count_as_supply_pins() {
        for name in [
            "3V3", "+3V3", "3.3V", "5V", "VIN", "VBUS", "BAT+", "RAW", "VCC",
        ] {
            assert!(is_module_supply_pin_name(name), "{name}");
        }
        for name in ["GND", "P0.06", "IO4", "RST", "TX", "V_IO_EN", ""] {
            assert!(!is_module_supply_pin_name(name), "{name}");
        }
    }

    /// A module whose MCU family has no emulator is refused by family name.
    #[test]
    fn an_aliased_module_on_an_unemulated_family_is_refused_by_name() {
        let mut module = comp("X1", "XIAO_ESP32C6");
        module.pins = vec![pin("1", 8), pin("2", 9)];
        let board = ExtractedBoard {
            name: "c6".to_string(),
            nets: vec![net(8, "+3V3"), net(9, "GND")],
            components: vec![module],
        };
        let bound = bind_board(&board, &ModelLibrary::builtin());
        assert!(bound.mcus.is_empty());
        assert_eq!(
            bound.unsupported_mcus,
            [("X1".to_string(), "ESP32-C6".to_string())]
        );
        let msg = no_processor_message(&bound.dnp_mcus, &bound.unsupported_mcus, FitRemedy::Cli);
        assert!(msg.contains("ESP32-C6 (X1)"), "{msg}");
    }

    /// A recognised family with no emulator is named in the refusal; a board
    /// with no processor at all keeps the generic message.
    #[test]
    fn an_unemulated_family_is_named_when_nothing_binds() {
        let mut h7 = comp("U3", "STM32H743VIT6");
        h7.footprint = "Package_DFN_QFN:QFN-48-1EP_7x7mm_P0.5mm".to_string();
        h7.pins = vec![pin("1", 8), pin("2", 9)];
        let board = ExtractedBoard {
            name: "h7".to_string(),
            nets: vec![net(8, "+3V3"), net(9, "GND")],
            components: vec![h7],
        };
        let bound = bind_board(&board, &ModelLibrary::builtin());
        assert!(bound.mcus.is_empty());
        assert_eq!(
            bound.unsupported_mcus,
            [("U3".to_string(), "STM32H7".to_string())]
        );
        let msg = no_processor_message(&bound.dnp_mcus, &bound.unsupported_mcus, FitRemedy::Cli);
        assert!(
            msg.contains("STM32H7 (U3) is recognised but has no emulator backend yet"),
            "{msg}"
        );
        let generic = no_processor_message(&[], &[], FitRemedy::Cli);
        assert!(!generic.contains("recognised"), "{generic}");
    }

    /// The rows a bind left for one rail net, as (outcome is a stamped rail, warning).
    fn rail_rows(bound: &BoundBoard, net: &str) -> Vec<(Option<f64>, Option<String>)> {
        bound
            .report
            .rows
            .iter()
            .filter(|r| r.reference == format!("RAIL:{net}"))
            .map(|r| match r.outcome {
                BindOutcome::PowerRail { volts } => (Some(volts), r.warning.clone()),
                _ => (None, r.warning.clone()),
            })
            .collect()
    }

    /// A three-pad regulator-shaped IC (named IN / GND / OUT) with a load on OUT.
    fn regulated_board(value: &str, out_net_name: &str) -> ExtractedBoard {
        let mut ldo = comp("U5", value);
        ldo.pins = vec![
            named_pin("3", "IN", 1),
            named_pin("1", "ADJ/GND_1", 3),
            named_pin("2", "OUT_2", 2),
        ];
        let mut load = comp("R1", "10k");
        load.pins = vec![pin("1", 2), pin("2", 3)];
        ExtractedBoard {
            name: "ldo".to_string(),
            nets: vec![net(1, "/DC-IN"), net(2, out_net_name), net(3, "GND")],
            components: vec![ldo, load],
        }
    }

    /// A bare `VCC` is a convention (5 V), not a statement. The net a
    /// regulator-shaped part's OUT pad drives is not guessed at 5 V, whether or
    /// not the part resolved: a 3.3 V LDO's output read 5.000 V on a board whose
    /// 3.3 V rail was only ever called VCC.
    #[test]
    fn a_bare_vcc_driven_by_a_regulator_output_is_not_guessed_at_five_volts() {
        let lib = ModelLibrary::builtin();
        // Faulty input: an unmodelled LDO drives the bare-named net.
        let bound = bind_board(&regulated_board("NOSUCH-LDO-33", "/VCC"), &lib);
        let rows = rail_rows(&bound, "/VCC");
        assert!(
            rows.iter().all(|(v, _)| v.is_none()),
            "no 5 V rail on a regulator output: {rows:?}"
        );
        let warning = rows[0].1.as_deref().expect("the skip says why");
        assert!(
            warning.contains("U5") && warning.contains("states no voltage"),
            "{warning}"
        );
        // Legal controls: a bare VCC nothing regulates keeps its convention; a
        // name that states its voltage keeps it under the same regulator.
        let mut load_only = regulated_board("NOSUCH-LDO-33", "/VCC");
        load_only.components.remove(0);
        let bound = bind_board(&load_only, &lib);
        assert_eq!(rail_rows(&bound, "/VCC")[0].0, Some(5.0));
        let bound = bind_board(&regulated_board("NOSUCH-LDO-33", "+3V3"), &lib);
        assert_eq!(rail_rows(&bound, "+3V3")[0].0, Some(3.3));
    }

    /// A regulator's quiescent draw needs an input to draw from. With nothing
    /// feeding IN it is an ideal current source into a node held only by gmin,
    /// which put the input at -4.3e9 V and refused the whole solve; now the
    /// floating input rests at 0 V, and a fed one still carries `Iq`.
    #[test]
    fn a_regulator_with_no_input_supply_does_not_drag_it_below_ground() {
        use hauksbee_solve::{dc_operating_point, SolverOptions, Workspace};
        use hauksbee_testkit::circuits::vdc;
        let lib = ModelLibrary::builtin();
        let model = resolve_value(&lib, "LM7805", "");
        for (fed, want_in) in [(None, 0.0), (Some(9.0), 9.0)] {
            let mut part = comp("U3", "LM7805");
            part.pins = vec![pin("1", 1), pin("2", 0), pin("3", 2)];
            let mut circuit = Circuit::new();
            let nodes = ["IN", "OUT"].map(|name| circuit.node(name));
            let node_of = |n: Option<i64>| match n {
                Some(1) => Some(nodes[0]),
                Some(2) => Some(nodes[1]),
                Some(_) => Some(NodeId::GROUND),
                None => None,
            };
            let (outcome, _) = bind_one(&part, &model, &mut circuit, &node_of, &HashMap::new());
            assert!(
                matches!(outcome, BindOutcome::Behavioral { .. }),
                "{outcome:?}"
            );
            if let Some(v) = fed {
                vdc(&mut circuit, "VIN", nodes[0], v);
            }
            let mut ws = Workspace::new(&circuit);
            dc_operating_point(&mut ws, &circuit, &SolverOptions::default()).unwrap();
            let got = ws.x[ws.layout.node(nodes[0]).unwrap()];
            assert!((got - want_in).abs() < 1e-3, "fed {fed:?}: IN at {got} V");
        }
    }

    /// An op-amp's quiescent draw needs a supply to draw from, as a regulator's
    /// does. Stamped as an ideal current source on a V+ nothing feeds, it put
    /// V+ at `-iq_a / gmin` (billions of volts below ground) and the solve was
    /// refused; now the unfed V+ rests at 0 V, and a fed one carries exactly
    /// the card's `iq_a`, once per package (both units of the dual are bound).
    #[test]
    fn an_opamp_with_no_supply_does_not_drag_its_v_plus_below_ground() {
        use hauksbee_solve::{dc_operating_point, SolverOptions, Workspace};
        use hauksbee_testkit::circuits::vdc;
        let lib = ModelLibrary::builtin();
        let mut model = resolve_value(&lib, "LM358", "");
        let iq = 1.5e-3;
        model.params.set_f64("iq_a", iq);
        for fed in [None, Some(5.0)] {
            // Both units as grounded-input followers: no output current, so
            // V+ carries the quiescent draw alone.
            let mut part = comp("U1", "LM358");
            let nets = [1, 1, 0, 0, 0, 2, 2, 3];
            part.pins = (1..=8)
                .zip(nets)
                .map(|(p, n)| pin(&p.to_string(), n))
                .collect();
            let mut circuit = Circuit::new();
            let nodes = ["OUTA", "OUTB", "VCC"].map(|name| circuit.node(name));
            let node_of = |n: Option<i64>| match n {
                Some(i @ 1..=3) => Some(nodes[usize::try_from(i - 1).unwrap()]),
                Some(_) => Some(NodeId::GROUND),
                None => None,
            };
            let (outcome, _) = bind_one(&part, &model, &mut circuit, &node_of, &HashMap::new());
            assert!(
                matches!(outcome, BindOutcome::Behavioral { .. }),
                "{outcome:?}"
            );
            let source = fed.map(|v| vdc(&mut circuit, "VCC", nodes[2], v));
            let mut ws = Workspace::new(&circuit);
            dc_operating_point(&mut ws, &circuit, &SolverOptions::default()).unwrap();
            // Unfed, V+ is held by gmin alone, so the outputs' own
            // gmin-sized currents still move it by millivolts: not by volts.
            let vcc = ws.x[ws.layout.node(nodes[2]).unwrap()];
            assert!(
                (vcc - fed.unwrap_or(0.0)).abs() < 0.1,
                "fed {fed:?}: V+ at {vcc} V"
            );
            if let Some(id) = source {
                let drawn = ws.x[ws.layout.branch(id).unwrap()].abs();
                assert!(
                    (drawn - iq).abs() < 1e-3 * iq,
                    "V+ supplies {drawn} A, card iq_a {iq} A"
                );
            }
        }
    }
}

#[cfg(test)]
mod identity_ic_tests {
    use super::bind_board;
    use hauksbee_extract::{Component, ExtractedBoard, Net, Pin};
    use hauksbee_models::ModelLibrary;

    fn part(reference: &str, value: &str, lib_id: &str, footprint: &str, pads: i64) -> Component {
        Component {
            reference: reference.into(),
            value: value.into(),
            lib_id: lib_id.into(),
            footprint: footprint.into(),
            pins: (1..=pads)
                .map(|n| Pin {
                    number: n.to_string(),
                    net: Some(n),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    /// A chip drawn under a switch-like designator is recorded as an active IC
    /// by identity; a tactile button, and a chip whose designator already says
    /// IC, are not recorded (the designator rule covers the latter).
    #[test]
    fn the_report_records_chips_whose_designator_does_not_say_ic() {
        let board = ExtractedBoard {
            name: "b".into(),
            nets: (1..=8).map(|n| Net::new(n, format!("N{n}"))).collect(),
            components: vec![
                part(
                    "S1",
                    "TS3A5017",
                    "Analog_Switch:TS3A5017",
                    "Package_SO:SOIC-16_3.9x9.9mm_P1.27mm",
                    8,
                ),
                part(
                    "SW2",
                    "SW_Push",
                    "Switch:SW_Push",
                    "Button_Switch_SMD:SW_SPST_TL3342",
                    2,
                ),
                part("U3", "74HC00", "74xx:74HC00", "Package_SO:SOIC-14", 8),
            ],
        };
        let bound = bind_board(&board, &ModelLibrary::builtin());
        let ids: Vec<&str> = bound
            .report
            .identity_ics
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(ids, ["S1"]);
    }
}
