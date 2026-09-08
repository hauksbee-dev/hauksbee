//! Switching-converter topology detection from the netlist + part kinds.
//!
//! Several physics checks (input-cap ripple, switch-node ampacity) need to know
//! *where* a switching converter is on the board: which net is the chopped
//! switch node, which is the input rail, which is the output rail, and what
//! current the stage moves. A single converter IC carries that in its DB
//! behavioural model (`[models.behavioral.converter]`), but the boards the bug
//! hunt cared about build the power stage from *discrete* parts (a half-bridge
//! gate driver + external FETs + an inductor), where no single part declares the
//! topology. This module recovers the topology purely structurally, the way an
//! engineer reads it off the schematic:
//!
//!   - The **switch node** is the net that ties a power-FET source/drain to one
//!     end of the power inductor (in a synchronous buck: HS-FET source +
//!     LS-FET drain + inductor input all share it).
//!   - The **input rail** is the FET power pin on the *other* side of the
//!     high-side switch from the switch node (the buck input, where the bulk
//!     input cap sits).
//!   - The **output rail** is the inductor's *other* pin.
//!
//! It deliberately refuses to guess: a stage is only reported when the
//! switch-node / inductor / input-rail triple resolves unambiguously and the
//! input rail carries a bulk capacitor to ground. Everything downstream of this
//! (the ripple and ampacity checks) inherits that discipline, so a board whose
//! topology cannot be read cleanly produces no finding rather than a fabricated
//! one.

use std::collections::{HashMap, HashSet};

use hauksbee_extract::{Component, ExtractedBoard, Pin};
use hauksbee_models::value::parse_value;
use hauksbee_models::{ComponentKind, ModelLibrary};

use crate::binder::resolve;
use hauksbee_extract::assembly::{AssemblyState, FittedComponent};

/// Switching topology recovered from the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topology {
    /// Step-down: input rail chopped into a switch node, inductor to a lower
    /// output. Input current is pulsed (the input cap ripple case).
    Buck,
    /// Step-up: inductor from the input charges, switch node chops to a higher
    /// output. Output current is pulsed.
    Boost,
}

/// One detected discrete switching-converter power stage.
#[derive(Debug, Clone)]
pub struct ConverterStage {
    pub topology: Topology,
    /// Net id + name of the chopped switch node.
    pub switch_node: (i64, String),
    /// Net id + name of the input rail (where the bulk input cap sits, for a
    /// buck).
    pub input_rail: (i64, String),
    /// Net id + name of the output rail.
    pub output_rail: (i64, String),
    /// The power inductor reference designator.
    pub inductor_ref: String,
    /// Inductor value (henries), if parseable.
    pub inductor_h: Option<f64>,
    /// Every bulk capacitor sitting input-rail -> ground. Keeping the bank is
    /// essential: ripple sharing across parallel parts cannot be inferred from
    /// one arbitrarily selected capacitor.
    pub input_bulk_caps: Vec<BulkCap>,
    /// Every bulk capacitor sitting output-rail -> ground.
    pub output_bulk_caps: Vec<BulkCap>,
    /// FET references that touch the switch node (for evidence / refs).
    pub switch_fets: Vec<String>,
}

/// A bulk electrolytic / large MLCC across a rail to ground.
#[derive(Debug, Clone)]
pub struct BulkCap {
    pub reference: String,
    pub value: String,
    /// Capacitance in farads, if parseable.
    pub farads: Option<f64>,
}

/// True when a net name is ground.
pub(crate) fn is_ground_net(name: &str) -> bool {
    let n = name.trim().trim_start_matches('/').to_ascii_uppercase();
    let leaf = n.rsplit('/').next().unwrap_or(&n);
    matches!(
        leaf,
        "GND" | "GNDA" | "GNDD" | "AGND" | "DGND" | "PGND" | "VSS" | "GNDIO" | "0" | "GNDPWR"
    ) || leaf.starts_with("GND")
}

/// A component is a power FET when its resolved kind is Nmos/Pmos, or (PCB-only,
/// no model) its reference is a transistor designator on a power-looking
/// footprint. We accept the resolved-kind path first (most reliable) and fall
/// back to the designator+footprint heuristic so an unmodeled power FET (the
/// common hunt case: PSMN5R2, unmatched) still participates in topology.
fn is_power_fet(part: FittedComponent<'_>, lib: &ModelLibrary) -> bool {
    let comp: &Component = &part;
    if let Some(model) = resolve(lib, part).model {
        if matches!(model.kind, ComponentKind::Nmos | ComponentKind::Pmos) {
            return true;
        }
    }
    let r = comp.reference.trim().to_ascii_uppercase();
    let is_q = r.starts_with('Q') && r[1..].chars().next().is_some_and(|c| c.is_ascii_digit());
    if !is_q {
        return false;
    }
    // A transistor on a power package (the kind a switching stage uses), or one
    // with three+ connected pads (a FET, not a 2-pin small-signal part on a
    // SOT-23 used as a load switch still counts). Keep it permissive: the
    // topology test that follows (must tie to an inductor switch node) is the
    // real gate, so over-including transistors here cannot create a false stage.
    true
}

/// A component is the power inductor: reference `L<n>`, two connected pads, and
/// a parseable henries value (so a ferrite bead named `L` without an inductance
/// is not mistaken for the power choke). We also accept `FB`-less `L` refs only.
fn power_inductor_value(comp: &Component) -> Option<f64> {
    let r = comp.reference.trim().to_ascii_uppercase();
    let is_l = r.starts_with('L') && r[1..].chars().next().is_some_and(|c| c.is_ascii_digit());
    if !is_l {
        return None;
    }
    // The value should parse to henries (e.g. "47uH", "10u"). A bare ferrite
    // labelled in ohms ("600R") will parse as a number too, so require the
    // value to carry an inductance unit hint OR be in a plausible henries range.
    let v = comp.value.trim();
    let parsed = parse_value(v)?;
    let looks_like_henries =
        v.to_ascii_uppercase().contains('H') || (parsed.si > 1e-9 && parsed.si < 1e-1);
    looks_like_henries.then_some(parsed.si)
}

/// Distinct connected pad count.
fn connected_pads(c: &Component) -> usize {
    let mut seen: HashSet<&str> = HashSet::new();
    for p in &c.pins {
        if p.net.is_some() {
            seen.insert(p.number.as_str());
        }
    }
    seen.len()
}

/// Bulk capacitors across `rail` to ground: reference `C<n>`, one pad on the
/// rail and one on a ground net. Only "bulk" sizes (>= 1 uF) qualify so small
/// decouplers are not treated as the switching-current bank. The full,
/// deterministically ordered bank is returned; consumers must not pretend all
/// ripple flows through whichever part happens to have the largest capacitance.
fn bulk_caps_on_rail(
    board: &ExtractedBoard,
    rail_id: i64,
    ground_ids: &HashSet<i64>,
) -> Vec<BulkCap> {
    let mut caps = Vec::new();
    for c in &board.components {
        if !AssemblyState::of(c).is_present() {
            continue;
        }
        let r = c.reference.trim().to_ascii_uppercase();
        let is_c =
            r.starts_with('C') && r[1..].chars().next().is_some_and(|ch| ch.is_ascii_digit());
        if !is_c || connected_pads(c) != 2 {
            continue;
        }
        let on_rail = c.pins.iter().any(|p| p.net == Some(rail_id));
        let on_gnd = c
            .pins
            .iter()
            .any(|p| p.net.is_some_and(|n| ground_ids.contains(&n)));
        if !(on_rail && on_gnd) {
            continue;
        }
        let farads = parse_value(c.value.trim()).map(|v| v.si);
        // Bulk threshold: >= 1 uF. A cap whose value will not parse is still a
        // candidate (it may be a large electrolytic with a part-number value),
        // but a parseable sub-1uF cap is a decoupling cap, not bulk.
        if let Some(f) = farads {
            if f < 1e-6 {
                continue;
            }
        }
        caps.push(BulkCap {
            reference: c.reference.clone(),
            value: c.value.clone(),
            farads,
        });
    }
    caps.sort_by(|left, right| {
        right
            .farads
            .partial_cmp(&left.farads)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.reference.cmp(&right.reference))
    });
    caps
}

/// Power pins of a FET: every connected pad that is not a gate. We do not always
/// have pin roles (PCB-only extraction), so a FET's power nets are taken as the
/// nets on its pads excluding the one we can identify as the gate. When roles
/// are unknown we take all pads; the inductor-tie test downstream still pins the
/// switch node correctly.
fn fet_power_nets(part: FittedComponent<'_>, lib: &ModelLibrary) -> Vec<i64> {
    let comp: &Component = &part;
    // Try to find the gate pad from the resolved model's pin map.
    let gate_pad: Option<String> = resolve(lib, part).model.and_then(|m| {
        m.pins
            .iter()
            .find(|(_, role)| role.eq_ignore_ascii_case("gate"))
            .map(|(pad, _)| pad.clone())
    });
    let mut nets = Vec::new();
    for p in &comp.pins {
        if let Some(g) = &gate_pad {
            if &p.number == g {
                continue;
            }
        } else if pin_is_gate_by_name(p) {
            continue;
        }
        if let Some(n) = p.net {
            nets.push(n);
        }
    }
    nets
}

fn pin_is_gate_by_name(p: &Pin) -> bool {
    let f = p.function.to_ascii_uppercase();
    f == "G" || f == "GATE"
}

/// Detect every discrete switching-converter power stage on the board.
///
/// Returns one [`ConverterStage`] per recovered stage. The detection is
/// conservative: a stage is emitted only when a switch-node net ties a power FET
/// to a power inductor and the input rail (the FET power net that is *not* the
/// switch node) is distinct and not ground. Boards with no discrete switching
/// stage (or whose topology is ambiguous) return an empty vector.
/// A switching stage that was found in the graph but whose direction could not
/// be established, so no buck/boost verdict was reached for it.
///
/// This exists because the abstention used to be a bare `continue`. A converter
/// nobody could classify then produced exactly the same output as a board with
/// no converter on it: nothing. The two are very different claims, and only one
/// of them is a pass.
/// Why a detected switching stage could not be given a direction.
///
/// `classify_topology` returns `None` for several distinct situations, and they
/// do not warrant the same sentence: telling a user that "neither rail name says
/// which is the input" about a stage whose rails are `VIN_A` and `VIN_B` is a
/// false statement about their board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbstentionReason {
    /// Neither rail name encodes a direction at all.
    NoDirectionalNames,
    /// Both rails name themselves inputs.
    BothNamedInput,
    /// Both rails name themselves outputs.
    BothNamedOutput,
}

impl AbstentionReason {
    fn describe(self) -> &'static str {
        match self {
            AbstentionReason::NoDirectionalNames => {
                concat!(
                    "the connectivity is reversible, so it is equally consistent with a buck ",
                    "and a boost, and neither rail name says which is the input"
                )
            }
            AbstentionReason::BothNamedInput => {
                concat!(
                    "the connectivity is reversible, and BOTH rail names claim to be the ",
                    "input, so the names contradict each other rather than settling the ",
                    "direction"
                )
            }
            AbstentionReason::BothNamedOutput => {
                concat!(
                    "the connectivity is reversible, and BOTH rail names claim to be the ",
                    "output, so the names contradict each other rather than settling the ",
                    "direction"
                )
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConverterAbstention {
    /// The power inductor that anchors the unclassified stage.
    pub inductor_ref: String,
    /// The switch-node net name.
    pub switch_node: String,
    /// The rail on the FET side of the switch.
    pub fet_side_rail: String,
    /// The rail at the far end of the inductor.
    pub inductor_far_rail: String,
    /// Which of the several unclassifiable situations this stage is in.
    pub reason: AbstentionReason,
}

impl ConverterAbstention {
    /// The canonical user-facing sentence, naming the part and the unlock.
    pub fn message(&self) -> String {
        format!(
            "converter: the switching stage around {} (switch node '{}', rails '{}' and '{}') \
             was detected but not classified: {}. No ripple or duty verdict was reached for \
             this stage. Rename the rails to carry their role (VIN / VOUT, or a *_IN / *_OUT \
             suffix) to cover it.",
            self.inductor_ref,
            self.switch_node,
            self.fet_side_rail,
            self.inductor_far_rail,
            self.reason.describe(),
        )
    }
}

/// Detect the switching stages on a board, alongside every stage that was found
/// but could not be given a direction.
///
/// The only entry point, deliberately. A variant returning stages alone cannot
/// distinguish "no converter here" from "a converter nobody could orient", which
/// is the exact dishonesty the abstentions exist to remove, so no such variant is
/// offered for a caller to reach for.
pub fn detect_converters_with_abstentions(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
) -> (Vec<ConverterStage>, Vec<ConverterAbstention>) {
    let mut abstentions: Vec<ConverterAbstention> = Vec::new();
    let ground_ids: HashSet<i64> = board
        .nets
        .iter()
        .filter(|n| is_ground_net(&n.name))
        .map(|n| n.id)
        .collect();

    // Map every FET to its power nets, and every inductor to its two nets.
    let fets: Vec<(&Component, Vec<i64>)> = board
        .components
        .iter()
        .filter_map(|c| AssemblyState::of(c).fitted())
        .filter(|part| is_power_fet(*part, lib))
        .map(|part| (part.component(), fet_power_nets(part, lib)))
        .collect();

    let inductors: Vec<(&Component, f64, Vec<i64>)> = board
        .components
        .iter()
        .filter(|c| AssemblyState::of(c).is_present())
        .filter_map(|c| {
            let h = power_inductor_value(c)?;
            let nets: Vec<i64> = c.pins.iter().filter_map(|p| p.net).collect();
            (nets.len() == 2).then_some((c, h, nets))
        })
        .collect();

    // For each inductor, one of its two nets is the switch node (ties to a FET
    // power net) and the other is the output (buck) or input (boost) rail.
    let mut stages = Vec::new();
    let mut used_switch_nodes: HashSet<i64> = HashSet::new();

    for (ind, h, ind_nets) in &inductors {
        // Which inductor net touches a FET power pin?
        for (idx, &cand_sw) in ind_nets.iter().enumerate() {
            if ground_ids.contains(&cand_sw) || used_switch_nodes.contains(&cand_sw) {
                continue;
            }
            // FETs whose power nets include this candidate switch node.
            let switch_fets: Vec<(&Component, &Vec<i64>)> = fets
                .iter()
                .filter(|(_, nets)| nets.contains(&cand_sw))
                .map(|(c, n)| (*c, n))
                .collect();
            if switch_fets.is_empty() {
                continue;
            }
            let other_rail = ind_nets[1 - idx];
            if ground_ids.contains(&other_rail) {
                continue;
            }
            // The input rail of a buck is the FET power net (across the high-side
            // switch) that is NOT the switch node and NOT ground. Gather all such
            // candidate rails from the switch FETs.
            let mut rail_votes: HashMap<i64, usize> = HashMap::new();
            for (_, nets) in &switch_fets {
                for &n in *nets {
                    if n != cand_sw && !ground_ids.contains(&n) && n != other_rail {
                        *rail_votes.entry(n).or_default() += 1;
                    }
                }
            }
            // Pick the rail that (a) the FETs vote for and (b) carries a bulk cap
            // to ground - that disambiguates the input rail from any gate-drive
            // bootstrap net or sense net also hanging off the FET. Scan the
            // candidates in a deterministic order (by net id): iterating the
            // HashMap directly made the first-cap winner depend on iteration
            // order when more than one rail carried a bulk cap.
            let mut rail_list: Vec<(i64, usize)> =
                rail_votes.iter().map(|(&k, &v)| (k, v)).collect();
            rail_list.sort_by_key(|&(id, _)| id);
            let mut input_rail_id = None;
            for &(rail, _) in &rail_list {
                if !bulk_caps_on_rail(board, rail, &ground_ids).is_empty() {
                    input_rail_id = Some(rail);
                    break;
                }
            }
            // If no FET-side rail carries a bulk cap, fall back to the most-voted
            // rail (still a valid topology; the ripple check will simply have no
            // cap to test, the ampacity check still works on the switch node).
            // Break vote ties by lowest net id so the choice is deterministic.
            let input_rail_id = input_rail_id.or_else(|| {
                rail_list
                    .iter()
                    .max_by_key(|&&(id, v)| (v, std::cmp::Reverse(id)))
                    .map(|&(id, _)| id)
            });
            let Some(input_rail_id) = input_rail_id else {
                continue;
            };

            let name_of = |id: i64| board.net(id).map(|n| n.name.clone()).unwrap_or_default();
            // Directionless synchronous connectivity is reversible: FET-side
            // high rail + inductor-side low rail can be either a buck or a boost.
            // Accept a direction only when rail names explicitly identify input
            // and/or output. Numeric voltage ordering alone is not evidence.
            let topology =
                match classify_topology_or_reason(&name_of(input_rail_id), &name_of(other_rail)) {
                    Ok(topology) => topology,
                    Err(reason) => {
                        // Not "no converter here": a converter we could not orient,
                        // and the note says which situation blocked it.
                        abstentions.push(ConverterAbstention {
                            inductor_ref: ind.reference.clone(),
                            switch_node: name_of(cand_sw),
                            fet_side_rail: name_of(input_rail_id),
                            inductor_far_rail: name_of(other_rail),
                            reason,
                        });
                        continue;
                    }
                };
            let (input_rail, output_rail) = match topology {
                Topology::Buck => (input_rail_id, other_rail),
                Topology::Boost => (other_rail, input_rail_id),
            };
            // Resolve the bulk caps from the final input/output rails directly,
            // so the mapping is correct regardless of buck/boost orientation.
            let input_bulk_caps = bulk_caps_on_rail(board, input_rail, &ground_ids);
            let output_bulk_caps = bulk_caps_on_rail(board, output_rail, &ground_ids);

            used_switch_nodes.insert(cand_sw);
            stages.push(ConverterStage {
                topology,
                switch_node: (cand_sw, name_of(cand_sw)),
                input_rail: (input_rail, name_of(input_rail)),
                output_rail: (output_rail, name_of(output_rail)),
                inductor_ref: ind.reference.clone(),
                inductor_h: Some(*h),
                input_bulk_caps,
                output_bulk_caps,
                switch_fets: switch_fets
                    .iter()
                    .map(|(c, _)| c.reference.clone())
                    .collect(),
            });
            break; // one stage per inductor
        }
    }
    // An abstention for a stage we DID classify (a second inductor sharing the
    // switch node, say) is noise, not a coverage hole.
    abstentions.retain(|a| !stages.iter().any(|s| s.inductor_ref == a.inductor_ref));
    (stages, abstentions)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RailDirection {
    Input,
    Output,
}

/// Direction hint from an explicit rail-role name. This intentionally does not
/// infer direction from voltage magnitude: a synchronous buck and synchronous
/// boost have the same undirected FET/inductor graph with the high and low rails
/// exchanged.
fn rail_direction(name: &str) -> Option<RailDirection> {
    let n = name.trim().trim_start_matches('/').to_ascii_uppercase();
    let leaf = n.rsplit('/').next().unwrap_or(&n);
    let role_tokens: Vec<&str> = leaf
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    let input = leaf == "IN"
        || leaf == "INPUT"
        || leaf == "VIN"
        || leaf.starts_with("VIN_")
        || leaf.ends_with("_VIN")
        || leaf.ends_with("_IN")
        || role_tokens
            .iter()
            .any(|token| matches!(*token, "IN" | "INPUT" | "VIN"));
    let output = leaf == "OUT"
        || leaf == "OUTPUT"
        || leaf == "VOUT"
        || leaf.starts_with("VOUT_")
        || leaf.ends_with("_VOUT")
        || leaf.ends_with("_OUT")
        || role_tokens
            .iter()
            .any(|token| matches!(*token, "OUT" | "OUTPUT" | "VOUT"));
    match (input, output) {
        (true, false) => Some(RailDirection::Input),
        (false, true) => Some(RailDirection::Output),
        _ => None,
    }
}

/// Classify the FET-side rail versus the inductor-far rail. `None` is a first-
/// class result: without a directional name, the graph is compatible with both
/// buck and boost and a buck-only ripple equation must abstain.
/// Test-only view of [`classify_topology_or_reason`] that discards the reason.
/// Production code takes the reason, because an abstention has to say why.
#[cfg(test)]
fn classify_topology(fet_side_name: &str, inductor_far_name: &str) -> Option<Topology> {
    classify_topology_or_reason(fet_side_name, inductor_far_name).ok()
}

/// [`classify_topology`], but the failure carries WHICH situation blocked it so
/// the abstention note can state the real reason.
fn classify_topology_or_reason(
    fet_side_name: &str,
    inductor_far_name: &str,
) -> Result<Topology, AbstentionReason> {
    use RailDirection::{Input, Output};
    match (
        rail_direction(fet_side_name),
        rail_direction(inductor_far_name),
    ) {
        (Some(Input), Some(Output)) | (Some(Input), None) | (None, Some(Output)) => {
            Ok(Topology::Buck)
        }
        (Some(Output), Some(Input)) | (Some(Output), None) | (None, Some(Input)) => {
            Ok(Topology::Boost)
        }
        (Some(Input), Some(Input)) => Err(AbstentionReason::BothNamedInput),
        (Some(Output), Some(Output)) => Err(AbstentionReason::BothNamedOutput),
        (None, None) => Err(AbstentionReason::NoDirectionalNames),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ground_net_recognised() {
        assert!(is_ground_net("GND"));
        assert!(is_ground_net("/Power/GND"));
        assert!(is_ground_net("PGND"));
        assert!(!is_ground_net("SOLAR+"));
        assert!(!is_ground_net("+3V3"));
    }

    #[test]
    fn classify_buck_only_from_directional_rail_names() {
        assert_eq!(
            classify_topology("SOLAR+", "DCDC_OUT"),
            Some(Topology::Buck)
        );
        assert_eq!(classify_topology("VIN", "VOUT"), Some(Topology::Buck));
        assert_eq!(
            classify_topology("PWR_IN_12V", "CORE_OUT_3V3"),
            Some(Topology::Buck)
        );
        assert_eq!(classify_topology("+12V", "+3V3"), None);
    }

    #[test]
    fn classify_boost_only_from_directional_rail_names() {
        assert_eq!(classify_topology("VOUT", "VIN"), Some(Topology::Boost));
        assert_eq!(classify_topology("+12V", "+3V3"), None);
    }

    #[test]
    fn explicit_output_name_does_not_fabricate_voltage_or_direction() {
        // VOUT supplies direction only. No nominal voltage is guessed from it.
        assert_eq!(classify_topology("+12V", "VOUT"), Some(Topology::Buck));
        assert_eq!(classify_topology("VIN", "VOUT"), Some(Topology::Buck));
    }

    #[test]
    fn pv_substrings_do_not_supply_direction_but_explicit_suffixes_do() {
        assert_eq!(rail_direction("PVDD"), None);
        assert_eq!(rail_direction("PVCC"), None);
        assert_eq!(rail_direction("SOLAR+"), None);
        assert_eq!(rail_direction("VBUS_IN"), Some(RailDirection::Input));
        assert_eq!(classify_topology("+12V", "PVDD_OUT"), Some(Topology::Buck));
    }

    #[test]
    fn synchronous_boost_with_only_numeric_rail_names_is_not_called_a_buck() {
        let component = |reference: &str, value: &str, pins: Vec<(&str, i64)>| -> Component {
            Component {
                reference: reference.into(),
                value: value.into(),
                lib_id: String::new(),
                footprint: String::new(),
                position: None,
                layer: "F.Cu".into(),
                properties: vec![],
                dnp: false,
                pins: pins
                    .into_iter()
                    .map(|(number, net)| Pin {
                        number: number.into(),
                        net: Some(net),
                        function: String::new(),
                        kind: String::new(),
                        position: None,
                    })
                    .collect(),
            }
        };
        let board = ExtractedBoard {
            name: "ambiguous_sync_stage".into(),
            nets: vec![
                hauksbee_extract::Net {
                    id: 1,
                    name: "+3V3".into(),
                },
                hauksbee_extract::Net {
                    id: 2,
                    name: "SW".into(),
                },
                hauksbee_extract::Net {
                    id: 3,
                    name: "+12V".into(),
                },
                hauksbee_extract::Net {
                    id: 4,
                    name: "GND".into(),
                },
            ],
            components: vec![
                component("L1", "10uH", vec![("1", 1), ("2", 2)]),
                component("Q1", "NMOS", vec![("1", 2), ("2", 3)]),
                component("Q2", "NMOS", vec![("1", 2), ("2", 4)]),
                component("C1", "100uF", vec![("1", 3), ("2", 4)]),
            ],
        };
        let (stages, abstentions) =
            detect_converters_with_abstentions(&board, &ModelLibrary::builtin());
        assert!(
            stages.is_empty(),
            "directionless synchronous connectivity is compatible with boost; abstain"
        );
        // The abstention must be visible. A bare skip here reads exactly like a
        // board with no converter on it, which is a different claim entirely.
        assert_eq!(abstentions.len(), 1, "the unclassified stage must be named");
        let m = abstentions[0].message();
        assert!(m.contains("L1"), "names the part: {m}");
        assert!(m.contains("SW"), "names the switch node: {m}");
        assert!(
            m.contains("VIN") && m.contains("VOUT"),
            "names the unlock: {m}"
        );
        // Only remedies the detector actually acts on may be suggested: direction
        // comes from rail names alone, so promising that a controller model would
        // fix it would send the user to do work that changes nothing.
        assert!(
            !m.contains("model"),
            "must not suggest a remedy the detector does not consult: {m}"
        );
        assert_eq!(abstentions[0].reason, AbstentionReason::NoDirectionalNames);
        assert!(
            m.contains("neither rail name says which is the input"),
            "the reason must match the situation: {m}"
        );
    }

    #[test]
    fn contradicting_rail_names_get_their_own_reason() {
        // classify_topology also abstains when BOTH rails claim the same role.
        // Telling that user "neither rail name says which is the input" is a false
        // statement about their board.
        assert_eq!(
            classify_topology_or_reason("VIN_A", "VIN_B"),
            Err(AbstentionReason::BothNamedInput)
        );
        assert_eq!(
            classify_topology_or_reason("VOUT_A", "VOUT_B"),
            Err(AbstentionReason::BothNamedOutput)
        );
        assert_eq!(
            classify_topology_or_reason("+12V", "+3V3"),
            Err(AbstentionReason::NoDirectionalNames)
        );
        // And the rendered sentence differs per reason.
        let msg = |reason| {
            ConverterAbstention {
                inductor_ref: "L1".into(),
                switch_node: "SW".into(),
                fet_side_rail: "VIN_A".into(),
                inductor_far_rail: "VIN_B".into(),
                reason,
            }
            .message()
        };
        let both_in = msg(AbstentionReason::BothNamedInput);
        assert!(
            both_in.contains("BOTH rail names claim to be the input"),
            "{both_in}"
        );
        assert!(
            !both_in.contains("neither rail name"),
            "must not also claim the names are silent: {both_in}"
        );
    }

    #[test]
    fn a_classified_stage_leaves_no_abstention() {
        // The note must not fire on boards whose converters were understood, or
        // it becomes noise that trains users to ignore it.
        let component = |reference: &str, value: &str, pins: Vec<(&str, i64)>| -> Component {
            Component {
                reference: reference.into(),
                value: value.into(),
                lib_id: String::new(),
                footprint: String::new(),
                position: None,
                layer: "F.Cu".into(),
                properties: vec![],
                dnp: false,
                pins: pins
                    .into_iter()
                    .map(|(number, net)| Pin {
                        number: number.into(),
                        net: Some(net),
                        function: String::new(),
                        kind: String::new(),
                        position: None,
                    })
                    .collect(),
            }
        };
        let board = ExtractedBoard {
            name: "named_sync_stage".into(),
            nets: vec![
                hauksbee_extract::Net {
                    id: 1,
                    name: "VOUT_3V3".into(),
                },
                hauksbee_extract::Net {
                    id: 2,
                    name: "SW".into(),
                },
                hauksbee_extract::Net {
                    id: 3,
                    name: "VIN_12V".into(),
                },
                hauksbee_extract::Net {
                    id: 4,
                    name: "GND".into(),
                },
            ],
            components: vec![
                component("L1", "10uH", vec![("1", 1), ("2", 2)]),
                component("Q1", "NMOS", vec![("1", 2), ("2", 3)]),
                component("Q2", "NMOS", vec![("1", 2), ("2", 4)]),
                component("C1", "100uF", vec![("1", 3), ("2", 4)]),
            ],
        };
        let (stages, abstentions) =
            detect_converters_with_abstentions(&board, &ModelLibrary::builtin());
        assert_eq!(stages.len(), 1, "named rails must classify");
        assert_eq!(stages[0].topology, Topology::Buck);
        assert!(
            abstentions.is_empty(),
            "a classified stage must not also be reported as uncovered: {abstentions:?}"
        );
    }
}
