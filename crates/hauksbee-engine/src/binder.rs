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

/// The bound board: a ready-to-solve circuit plus the event-driven layer.
pub struct BoundBoard {
    pub name: String,
    pub circuit: Circuit,
    /// Net name -> circuit node.
    pub net_nodes: HashMap<String, NodeId>,
    /// All net names in declaration order (for board_info / frames).
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
        if let Some(v) = power_rail_voltage(&net.name) {
            power_nets.insert(net.name.clone(), v);
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

        let (kind_str, outcome, warning) = match &model {
            None => unresolved_outcome(comp, &node_of),
            Some(m) => {
                let kind_str = format!("{:?}", m.kind).to_ascii_lowercase();
                let (outcome, warning) = bind_component(
                    comp,
                    m,
                    conf,
                    &mut circuit,
                    &node_of,
                    &mut digital,
                    &mut mcus,
                    has_vreg,
                    &power_nets,
                );
                (Some(kind_str), outcome, warning)
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
        });
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
    has_vreg: bool,
    power_nets: &HashMap<String, f64>,
) -> (BindOutcome, Option<String>) {
    use ComponentKind::*;

    // role -> node for this component's connected pins.
    let role_nets = role_node_map(comp, model, node_of);
    // pad number -> node, regardless of role.
    let pad_nodes = |pad: &str| -> Option<NodeId> {
        comp.pins
            .iter()
            .find(|p| p.number == pad)
            .and_then(|p| node_of(p.net))
    };

    match model.kind {
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
        Dac | Adc => {
            // Treated as behavioral passthrough buffers for now.
            bind_digital(comp, model, circuit, &role_nets, digital);
            (
                BindOutcome::Digital {
                    kind: format!("{:?}", model.kind).to_ascii_lowercase(),
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
    }
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
    let m = MosfetModel {
        level: MosLevel::Level1,
        polarity,
        vto: p.get_f64("vto").unwrap_or(def.vto),
        kp: p.get_f64("kp").unwrap_or(def.kp),
        lambda: p.get_f64("lambda").unwrap_or(def.lambda),
        gamma: p.get_f64("gamma").unwrap_or(def.gamma),
        phi: p.get_f64("phi").unwrap_or(def.phi),
        w_over_l: p.get_f64("w_over_l").unwrap_or(def.w_over_l),
        n_sub: p.get_f64("n_sub").unwrap_or(def.n_sub),
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
    let (Some(out), Some(inp), Some(inn)) = (out, inp, inn) else {
        return open_warning(comp, "opamp pins not all connected");
    };
    let gain = model.params.get_f64("gain").unwrap_or(1e5);
    let rail_lo = model.params.get_f64("rail_lo").unwrap_or(0.0);
    let rail_hi = model.params.get_f64("rail_hi").unwrap_or(5.0);
    circuit.add(Device::OpAmp {
        name: comp.reference.clone(),
        out,
        inp,
        inn,
        gain,
        rail_lo,
        rail_hi,
    });
    (
        BindOutcome::Behavioral {
            device: "opamp".to_string(),
        },
        None,
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

fn bind_digital(
    comp: &Component,
    model: &ModelEntry,
    circuit: &mut Circuit,
    roles: &HashMap<String, NodeId>,
    digital: &mut Vec<DigitalComponent>,
) {
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
    digital.push(DigitalComponent::new(
        comp.reference.clone(),
        model,
        roles.clone(),
        drivers,
    ));
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
    // A pin wired to an ADC channel is an analog *input*: it must not get an
    // output driver, or the (default-0 V) Thevenin leg would clamp the net the
    // firmware is trying to read. GPIO output drivers are stamped tri-stated
    // (high-impedance) and only enabled when the firmware actually drives the
    // pin — captured as the first `on_pin_change` edge by the scheduler.
    let mut gpio_drivers = HashMap::new();
    let mut adc_nets = HashMap::new();
    for (role, &node) in &role_nets {
        if node.is_ground() {
            continue;
        }
        let adc_ch = adc_of_role(role, module);
        if let Some(ch) = adc_ch {
            adc_nets.insert(ch, node);
        }
        if adc_ch.is_some() {
            // Treat ADC pins as inputs; no output driver.
            continue;
        }
        if let Some((port, bit)) = gpio_of_role(role, module) {
            let net_name = circuit.node_name(node).to_string();
            let mut drv = PinDriver::stamp(
                circuit,
                node,
                &net_name,
                &format!("{}_{port}{bit}", comp.reference),
                DEFAULT_RO,
            );
            // Start high-impedance: the firmware enables the driver by toggling
            // the pin (DDR + PORT writes surface as an output edge).
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
        Device::Resistor { ohms, .. } => format!("R {ohms:.0}Ω"),
        Device::Capacitor { farads, .. } => format!("C {:.3}µF", farads * 1e6),
        Device::Inductor { henries, .. } => format!("L {:.3}µH", henries * 1e6),
        other => other.name().to_string(),
    }
}

/// Map an ATmega328P / Arduino-Nano role string to a `(port, bit)` GPIO id.
fn gpio_of_role(role: &str, module: bool) -> Option<(char, u8)> {
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
fn is_ground(name: &str) -> bool {
    let n = name.trim().trim_start_matches('/').to_ascii_uppercase();
    matches!(
        n.as_str(),
        "GND" | "GNDA" | "GNDD" | "AGND" | "DGND" | "VSS" | "0" | "VEE"
    ) || n.ends_with("GND")
}

/// If `name` is a recognised supply rail, return its nominal voltage.
fn power_rail_voltage(name: &str) -> Option<f64> {
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
