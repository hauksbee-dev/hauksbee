//! The binder: [`ExtractedBoard`] + [`ModelLibrary`] -> [`BoundBoard`].
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

use crate::digital::{output_roles, DigitalComponent};
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
    /// Whether this is a module wrapper (Arduino_Nano) using header pad names.
    pub module: bool,
}

/// One MCP4728 quad DAC discovered on the board. Carries the assigned 7-bit
/// I2C address, the board reference/value config (VREF, gain), and the four
/// VOUT-channel [`PinDriver`]s already stamped into the circuit. The scheduler
/// realizes each binding as a spec-driven
/// [`RegisterMapSensor`](crate::RegisterMapSensor) instance of
/// `docs/hunts/specs/mcp4728.toml` on a shared bus, binding these drivers to
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
    /// is therefore OFF BY ONE and relabels every node as its successor — which
    /// is exactly what produced a spurious "comparator +IN -> SCL" mis-wiring
    /// claim on the Tarski board. To turn a `NodeId` back into a net name, use
    /// [`Circuit::node_name`] (the circuit's authoritative reverse map).
    pub net_names: Vec<String>,
    pub digital: Vec<DigitalComponent>,
    pub mcus: Vec<McuBinding>,
    /// reference -> resolved model kind string (for board_info coloring).
    pub component_kinds: HashMap<String, String>,
    /// Named controllable input sources: reference -> DeviceId of a Vsource /
    /// Isource the UI can override (sliders).
    pub input_sources: HashMap<String, hauksbee_ir::DeviceId>,
    /// Configurable power supplies, one per detected supply net (Feature 1).
    /// Default to [`PowerSupply::Ideal`] at the rail's nominal voltage, which
    /// preserves the old ideal-rail behaviour bit-for-bit.
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
        let node = if is_ground(&net.name) {
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
        matches!(
            resolve(lib, c).model.as_ref().map(|m| m.kind),
            Some(ComponentKind::Vreg)
        )
    });

    // ── Pass 2: bind every component ────────────────────────────────────────
    for comp in &board.components {
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
    dacs.sort_by(|a, b| a.reference.cmp(&b.reference));
    for (i, d) in dacs.iter_mut().enumerate() {
        d.address = 0x60 + i as u8;
    }

    // ── Pass 3: attach configurable power supplies ──────────────────────────
    // Every detected supply net gets a behavioral supply (default Ideal at the
    // rail's nominal voltage — identical to the old ideal Vsource), unless a
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

    // ── Pass 3b: stamp behavioural devices (power ICs) ──────────────────────
    // Any resolved model carrying a non-empty `[models.behavioral]` block (a
    // charger / PMIC / balancer the SPICE kinds cannot express) is stamped as a
    // behavioural device: controllable Thevenin legs + sense resistors the
    // scheduler iterates each chunk. Programmable limits read board resistor
    // values through `board_resistor`.
    let behavioral = bind_behavioral(board, lib, &mut circuit, &node_of, custom);

    // ── Pass 4: gather fault-monitor metadata ───────────────────────────────
    // Match each monitorable IR device back to its component (device name ==
    // reference for the parts we stamp) and the component's resolved ratings +
    // footprint, so the stress monitor can evaluate it. Supplies/regulators are
    // matched by their Vsource device id directly.
    let device_meta = gather_device_meta(board, lib, &circuit);

    BoundBoard {
        name: board.name.clone(),
        circuit,
        net_nodes,
        net_names,
        digital,
        mcus,
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
        if comp.reference.starts_with('R') {
            if let Some(p) = parse_value(&comp.value) {
                resistor_ohms.insert(comp.reference.clone(), p.si);
            }
        }
    }
    let board_resistor = |refdes: &str| -> Option<f64> { resistor_ohms.get(refdes).copied() };

    let mut out = Vec::new();
    for comp in &board.components {
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
        // per unit with a suffix ("IC3906_q2", "SW1_s0"); strip it so the
        // package's ratings apply to every unit.
        let name = dev.name();
        let base = name
            .rsplit_once("_q")
            .filter(|(_, n)| n.chars().all(|c| c.is_ascii_digit()))
            .map(|(b, _)| b)
            .or_else(|| {
                name.rsplit_once("_s")
                    .filter(|(_, n)| n.chars().all(|c| c.is_ascii_digit()))
                    .map(|(b, _)| b)
            })
            .unwrap_or(name);
        if let Some(info) = by_ref.get(base) {
            // Only monitor kinds the evaluator knows how to score.
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

    // Vreg output sources: name is "Vreg_<ref>".
    for (id, dev) in circuit.iter() {
        if let Device::Vsource { name, .. } = dev {
            if let Some(reference) = name.strip_prefix("Vreg_") {
                if let Some(info) = by_ref.get(reference) {
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
    q.mpn = comp
        .properties
        .iter()
        .find(|(k, _)| {
            let k = k.to_ascii_lowercase().replace([' ', '-'], "_");
            k.contains("mpn")
                || k.contains("manufacturer_part")
                || k == "part_number"
                || k == "mfr_part"
        })
        .map(|(_, v)| v.clone())
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
    // co-sim makes every firmware-driven net read as "never driven / Hi-Z" —
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

    // Generic signal diode by reference class. KiCad's stock "Device:D" symbol
    // carries the value "D" with no MPN, so the model db cannot resolve it; a
    // bare SOD/SMA/SMB/MELF/DO diode footprint with no resolved model is the
    // same case. These are real silicon junctions on the board (on Tarski the
    // ~94 D_stretch/D_inject/D_hyst pulse-stretcher diodes), so leaving them
    // OPEN silently deletes a conducting path. Mirror the r_/c_/l_ fallbacks:
    // when the part is unmistakably a 2-terminal diode, bind a generic 1N4148
    // signal diode (datasheet-grounded params below).
    if prefix.starts_with('D') && prefix != "DAC" {
        let v = comp.value.trim();
        let val_is_generic = v.is_empty()
            || v.eq_ignore_ascii_case("D")
            || v.eq_ignore_ascii_case("diode")
            || v.eq_ignore_ascii_case("1N4148")
            || v.eq_ignore_ascii_case("1N4148W")
            || v.eq_ignore_ascii_case("1N4148WS");
        let fp = comp.footprint.to_ascii_uppercase();
        // Footprint families for small 2-pin signal/switching diodes.
        let fp_is_diode = fp.contains("SOD")
            || fp.contains("SMA")
            || fp.contains("SMB")
            || fp.contains("SMC")
            || fp.contains("MELF")
            || fp.contains("DO-")
            || fp.contains("D_")
            || fp.contains("DIODE");
        // Bind the fallback when the value is a generic diode token, OR the
        // value is unparseable-as-a-passive but the footprint is a diode body.
        if val_is_generic || (parse_value(&comp.value).is_none() && fp_is_diode) {
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
        let structured = !direct
            && comp
                .value
                .split(['_', ' '])
                .filter(|t| !t.is_empty())
                .any(|t| parse_value(t).is_some());
        if direct || structured {
            let value_hint = if direct {
                None
            } else {
                comp.value
                    .split(['_', ' '])
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
/// ("600@100MHz"), which also ends in "hz" — but a bead sits in *series* in a
/// power/signal path, so binding it Ignore would open that path and re-create
/// the exact solve-collapse this crystal handling exists to prevent. Requiring
/// the remainder (after stripping "hz" and the SI prefix) to be purely numeric
/// rejects "600@100MHz" (the '@' survives) while accepting real crystal values.
fn value_is_frequency(value: &str) -> bool {
    let v = value.trim().to_ascii_lowercase();
    let Some(num) = v.strip_suffix("hz") else {
        return false;
    };
    let num = num.trim().trim_end_matches(['k', 'm', 'g']);
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
        if !matches!(port, 'A'..='E') {
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
    let reason = if two_terminal {
        "no model; left open".to_string()
    } else {
        "no model".to_string()
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
        AnalogSwitch => bind_analog_switch(comp, model, circuit, &role_nets),
        Digital | ShiftRegister => {
            bind_digital(comp, model, circuit, &role_nets, digital);
            let kind = if model.kind == ShiftRegister {
                "shift_register"
            } else {
                "digital"
            };
            (
                BindOutcome::Digital {
                    kind: kind.to_string(),
                },
                None,
            )
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
            bind_digital(comp, model, circuit, &role_nets, digital);
            (
                BindOutcome::Digital {
                    kind: "adc".to_string(),
                },
                None,
            )
        }
        Mcu => {
            let backend = model
                .params
                .get_str("backend")
                .unwrap_or("simavr:atmega328p")
                .to_string();
            let warning = bind_mcu(
                comp,
                model,
                circuit,
                node_of,
                &pad_nodes,
                power_nets,
                mcus,
            );
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
/// electrode names — they encode what the symbol's author connected, which
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
/// fills a *gap* — exactly the layout-only case where the pad carries a bare
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
    // The set of roles already filled by an explicit source — never overwritten.
    for pin in &comp.pins {
        let Some(node) = node_of(pin.net) else { continue };
        let Some(inf) = pin_rules.role_for_pad(&comp.footprint, Some(model.kind), pad_count, &pin.number)
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
    m
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
            "b1" | "s0" | "no" => "s0",
            "b2" | "s1" | "nc" => "s1",
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
    // Decide R / C / L from the reference designator prefix and the unit.
    let unit = p.unit.as_deref().unwrap_or("");
    let prefix = comp
        .reference
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    let device = if prefix.starts_with('C') || unit.eq_ignore_ascii_case("F") {
        Device::Capacitor {
            name: comp.reference.clone(),
            a,
            b,
            farads: p.si,
            ic: None,
        }
    } else if prefix.starts_with('L') || unit.eq_ignore_ascii_case("H") {
        Device::Inductor {
            name: comp.reference.clone(),
            a,
            b,
            henries: p.si,
            ic: None,
        }
    } else {
        Device::Resistor {
            name: comp.reference.clone(),
            a,
            b,
            ohms: p.si.max(1e-6),
            tc1: None,
        }
    };
    let label = device_label(&device);
    circuit.add(device);
    (BindOutcome::Analog { device: label }, None)
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
        for suffix in &suffixes {
            let get = |role: &str| roles.get(&format!("{role}{suffix}")).copied();
            let (Some(c), Some(b), Some(e)) = (get("collector"), get("base"), get("emitter"))
            else {
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
        return (
            BindOutcome::Analog {
                device: format!("bjt x{stamped}"),
            },
            None,
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
        xti: p.get_f64("xti").unwrap_or(d.xti),
        eg: p.get_f64("eg").unwrap_or(d.eg),
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
    let out = roles.get("out").copied();
    let vout = model.params.get_f64("vout").unwrap_or(DEFAULT_VCC);
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
        None,
    )
}

fn bind_opamp(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
) -> (BindOutcome, Option<String>) {
    // Use the first (A) channel: out_a / in_plus_a / in_minus_a, falling back
    // to generic role names.
    let out = pick(roles, &["out_a", "out", "out_1"]).copied();
    let inp = pick(roles, &["in_plus_a", "in_plus", "inp", "in+"]).copied();
    let inn = pick(roles, &["in_minus_a", "in_minus", "inn", "in-"]).copied();
    let reference = pick(roles, &["ref", "reference", "vref"]).copied();
    let (Some(out), Some(inp), Some(inn)) = (out, inp, inn) else {
        return open_warning(comp, "opamp pins not all connected");
    };
    let gain = model.params.get_f64("gain").unwrap_or(1e5);
    let pole_hz = model.params.get_f64("pole_hz");
    let rail_lo = model.params.get_f64("rail_lo").unwrap_or(0.0);
    let rail_hi = model.params.get_f64("rail_hi").unwrap_or(5.0);
    circuit.add(Device::OpAmp {
        name: comp.reference.clone(),
        out,
        inp,
        inn,
        reference,
        gain,
        pole_hz,
        rail_lo,
        rail_hi,
    });
    let warning = model
        .params
        .get_str("warning")
        .map(|w| format!("{} ({}): {w}", comp.reference, comp.value));
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
    let out = pick(roles, &["out_a", "out", "out_1", "q"]).copied();
    let inp = pick(roles, &["in_plus_a", "in_plus", "inp", "in+"]).copied();
    let inn = pick(roles, &["in_minus_a", "in_minus", "inn", "in-"]).copied();
    let (Some(out), Some(inp), Some(inn)) = (out, inp, inn) else {
        return open_warning(comp, "comparator pins not all connected");
    };
    let out_lo = model.params.get_f64("out_lo").unwrap_or(0.0);
    let out_hi = model.params.get_f64("out_hi").unwrap_or(5.0);
    let hyst = model.params.get_f64("hysteresis").unwrap_or(0.005);
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
            von: 5.0 - vth + 0.25,
            voff: 5.0 - vth - 0.25,
            ron,
            roff,
        });
        return (
            BindOutcome::Analog {
                device: "spdt x2".to_string(),
            },
            None,
        );
    }

    // SPST fallback: switch COM<->S0 (or in_out_a<->in_out_b) controlled by
    // ctrl vs vss. Model the on-leg only; the other throw is left open.
    let a = pick(roles, &["com", "in_out_a", "in_out_1a", "s0"]).copied();
    let b = pick(roles, &["s0", "in_out_b", "in_out_1b", "com"]).copied();
    // Resolve a and b to distinct nodes.
    let (a, b) = match (a, b) {
        (Some(a), Some(b)) if a != b => (a, b),
        _ => {
            // Fall back: first two non-power roles.
            let mut nodes: Vec<NodeId> = roles
                .iter()
                .filter(|(r, _)| !is_power_role(r))
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
    circuit.add(Device::VSwitch {
        name: comp.reference.clone(),
        a,
        b,
        ctrl_p: ctrl,
        ctrl_n,
        von: vth + 0.1,
        voff: vth - 0.1,
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

fn bind_digital(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
    digital: &mut Vec<DigitalComponent>,
) {
    // A quad/dual NOR gate (74HC02) wired as cross-coupled SR spike latches: if
    // any gate pair forms a NOR latch (one gate's output net == the other gate's
    // input net and vice-versa), bind one NorLatch behavioral component per latch
    // and return. Falls through to the generic buffer path if no latch found.
    if model.id.to_ascii_lowercase().contains("74hc02")
        && bind_nor_latches(comp, model, circuit, roles, digital)
    {
        return;
    }

    // Stamp a Thevenin driver on each connected output role.
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
                DEFAULT_RO,
            );
            drivers.insert(role, drv);
        }
    }
    match DigitalComponent::new(comp.reference.clone(), model, roles.clone(), drivers) {
        Ok(d) => digital.push(d),
        // Never silently downgrade a broken spec to a passthrough: the part is
        // left unmodeled LOUDLY (its nets float — exactly the lore-#9 failure
        // this message exists to surface early).
        Err(e) => eprintln!(
            "ERROR: {}: invalid [models.logic] for model '{}': {e}; the part is left \
             unmodeled and its output nets will float — fix the spec (`hauksbee models \
             lint`) or override it with --models-dir",
            comp.reference, model.id
        ),
    }
}

/// Detect and bind cross-coupled NOR SR latches on a 74HC02 (the Tarski spike
/// recorder). Each gate `g<n>` has output role `g<n>y` and input roles `g<n>a`,
/// `g<n>b`. A latch is a pair of gates (gQ, gQb) where `gQ.y == gQb.<one input>`
/// and `gQb.y == gQ.<one input>` (the cross-couple). For the latch:
///   - `reset` = the gate whose NON-cross-couple input net name contains "RESET"
///     (RESET_SR) — that gate's output is Qb (internal);
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
                // No RESET-named line found — not a recognisable SR latch here.
                continue;
            };
            let reset_in = if q_gate == i { in_j } else { in_i };
            let (Some(set_n), Some(reset_n)) = (set_in, reset_in) else { continue };
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
                DEFAULT_RO,
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
    let backend = model
        .params
        .get_str("backend")
        .unwrap_or("simavr:atmega328p")
        .to_string();
    let module = model.params.0.get("module").is_some();
    let derived_when_empty = if model.pins.is_empty() {
        Some(derive_mcu_pin_roles(comp))
    } else {
        None
    };
    let effective_pins = derived_when_empty
        .as_ref()
        .map(|d| &d.roles)
        .unwrap_or(&model.pins);

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
    // Dynamic promotion (05-cosim-fidelity §4.1, closes TARSKI_RESULTS §5.2): a
    // dual-purpose ADC/GPIO pin (Nano a0..a5 = PC0..PC5, or bare pc0_adc0..)
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
        module,
    });
    log_mcu_auto_decision(comp, model, derived_when_empty.as_ref())
}

fn log_mcu_auto_decision(
    comp: &Component,
    model: &ModelEntry,
    derived_when_empty: Option<&DerivedMcuPins>,
) -> Option<String> {
    let backend = model
        .params
        .get_str("backend")
        .unwrap_or("simavr:atmega328p");
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
    let (scale, prefix) = PREFIXES
        .iter()
        .find(|(s, _)| v >= *s)
        .copied()
        .unwrap_or(*PREFIXES.last().unwrap());
    let mantissa = v / scale;
    // 3 significant figures: more decimals for small mantissas, fewer for large.
    let s = if mantissa >= 100.0 {
        format!("{mantissa:.0}")
    } else if mantissa >= 10.0 {
        format!("{mantissa:.1}")
    } else {
        format!("{mantissa:.2}")
    };
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
            // Lettered port A-G.
            let port_upper = port_c.to_ascii_uppercase();
            if ('A'..='G').contains(&port_upper) {
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
/// promotion, 05-cosim-fidelity §4.1). On the ATmega328P the analog pins
/// A0..A5 are port C bits 0..5 (PC0..PC5); A6/A7 (Nano modules) are ADC-only
/// with no port pin and return `None`, so they stay pure ADC probes. Handles
/// both the Nano module role ("a2", "a3_...") and the bare role carrying an
/// adc index ("pc2_adc2").
fn apin_gpio_of_role(role: &str, module: bool) -> Option<(char, u8)> {
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
        "+5V" | "5V" | "VCC" | "VDD" | "+VCC" | "VBUS" | "+5V0" => Some(5.0),
        "+3V3" | "3V3" | "+3.3V" | "3.3V" | "VCC3V3" | "VDD3V3" => Some(3.3),
        "+3V" | "3V" => Some(3.0),
        "+12V" | "12V" => Some(12.0),
        "+1V8" | "1V8" | "1.8V" => Some(1.8),
        _ => {
            // "+5V_USB", "VCC_5V" style names.
            if n.contains("5V") && (n.starts_with('+') || n.contains("VCC") || n.contains("VBUS")) {
                Some(5.0)
            } else if n.contains("3V3") || n.contains("3.3V") {
                Some(3.3)
            } else {
                None
            }
        }
    }
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
    fn frequency_values_are_recognised() {
        for v in ["16MHz", "8Mhz", "16.000MHz", "32.768kHz", "100 Hz", "12000000Hz"] {
            assert!(value_is_frequency(v), "{v} should read as a frequency");
        }
        // Real passive values, and ferrite-bead impedance@frequency values
        // (which end in "hz" but are NOT crystals) must not trip it.
        for v in [
            "22pF", "10k", "4k7", "100nF", "BCM857BS", "Hz", "Choke", "",
            "600@100MHz", "1k@100MHz", "120@100MHz",
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
}

#[cfg(test)]
mod fmt_tests {
    use super::fmt_eng;

    #[test]
    fn capacitor_scales_to_pico_nano_micro() {
        // The reported bug: 390 pF used to print as "0.000 µF".
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

}
