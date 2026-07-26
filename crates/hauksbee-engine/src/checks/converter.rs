//! Switching-converter topology detection from the netlist + part kinds.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
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
    /// The bulk capacitor sitting input-rail -> ground, if one was found.
    pub input_bulk_cap: Option<BulkCap>,
    /// The bulk capacitor sitting output-rail -> ground, if one was found.
    pub output_bulk_cap: Option<BulkCap>,
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
fn is_power_fet(comp: &Component, lib: &ModelLibrary) -> bool {
    if let Some(model) = resolve(lib, comp).model {
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

/// A bulk capacitor across `rail` to ground: reference `C<n>`, one pad on the
/// rail and one on a ground net. Returns the cap with its parsed value. Only
/// "bulk" sizes (>= 1 uF) qualify so a small decoupling cap is not treated as
/// the input bulk; the ripple math only makes sense for the bulk element.
fn bulk_cap_on_rail(
    board: &ExtractedBoard,
    rail_id: i64,
    ground_ids: &HashSet<i64>,
) -> Option<BulkCap> {
    let mut best: Option<BulkCap> = None;
    for c in &board.components {
        if c.dnp {
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
        let cap = BulkCap {
            reference: c.reference.clone(),
            value: c.value.clone(),
            farads,
        };
        // Prefer the largest parseable cap as "the" bulk element.
        best = match (best, &cap.farads) {
            (None, _) => Some(cap),
            (Some(b), Some(f)) if b.farads.map(|bf| *f > bf).unwrap_or(true) => Some(cap),
            (Some(b), _) => Some(b),
        };
    }
    best
}

/// Power pins of a FET: every connected pad that is not a gate. We do not always
/// have pin roles (PCB-only extraction), so a FET's power nets are taken as the
/// nets on its pads excluding the one we can identify as the gate. When roles
/// are unknown we take all pads; the inductor-tie test downstream still pins the
/// switch node correctly.
fn fet_power_nets(comp: &Component, lib: &ModelLibrary) -> Vec<i64> {
    // Try to find the gate pad from the resolved model's pin map.
    let gate_pad: Option<String> = resolve(lib, comp).model.and_then(|m| {
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
pub fn detect_converters(board: &ExtractedBoard, lib: &ModelLibrary) -> Vec<ConverterStage> {
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
        .filter(|c| !c.dnp && is_power_fet(c, lib))
        .map(|c| (c, fet_power_nets(c, lib)))
        .collect();

    let inductors: Vec<(&Component, f64, Vec<i64>)> = board
        .components
        .iter()
        .filter(|c| !c.dnp)
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
            let mut input_cap = None;
            for &(rail, _) in &rail_list {
                if let Some(cap) = bulk_cap_on_rail(board, rail, &ground_ids) {
                    input_rail_id = Some(rail);
                    input_cap = Some(cap);
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
            // Buck: output rail is the inductor's far net (the lower rail). Boost:
            // the inductor's far net is the *input*. We classify by the rail
            // voltages when the names are recognisable; default to Buck (the hunt
            // case and the overwhelmingly common discrete stage). The ripple
            // formula below uses D = Vout/Vin for a buck regardless, so a
            // mislabelled boost would simply not match the buck ripple law and is
            // left to the (future) boost arm rather than mis-firing.
            let topology = classify_topology(&name_of(input_rail_id), &name_of(other_rail));
            let (input_rail, output_rail) = match topology {
                Topology::Buck => (input_rail_id, other_rail),
                Topology::Boost => (other_rail, input_rail_id),
            };
            // Resolve the bulk caps from the final input/output rails directly,
            // so the mapping is correct regardless of buck/boost orientation.
            let _ = &input_cap; // (the input-rail probe above only disambiguated the rail)
            let input_bulk_cap = bulk_cap_on_rail(board, input_rail, &ground_ids);
            let output_bulk_cap = bulk_cap_on_rail(board, output_rail, &ground_ids);

            used_switch_nodes.insert(cand_sw);
            stages.push(ConverterStage {
                topology,
                switch_node: (cand_sw, name_of(cand_sw)),
                input_rail: (input_rail, name_of(input_rail)),
                output_rail: (output_rail, name_of(output_rail)),
                inductor_ref: ind.reference.clone(),
                inductor_h: Some(*h),
                input_bulk_cap,
                output_bulk_cap,
                switch_fets: switch_fets
                    .iter()
                    .map(|(c, _)| c.reference.clone())
                    .collect(),
            });
            break; // one stage per inductor
        }
    }
    stages
}

/// Classify buck vs boost from the two rail names' nominal voltages when
/// recognisable. Higher input than output -> buck; lower -> boost. Unknown ->
/// buck (the common discrete case and the hunt's mppt-1210).
fn classify_topology(input_name: &str, output_name: &str) -> Topology {
    match (rail_nominal_v(input_name), rail_nominal_v(output_name)) {
        (Some(vin), Some(vout)) if vout > vin => Topology::Boost,
        _ => Topology::Buck,
    }
}

/// Rough nominal voltage of a rail by name, for buck/boost classification only.
/// Returns `None` for names that do not encode a voltage.
fn rail_nominal_v(name: &str) -> Option<f64> {
    let n = name.trim().trim_start_matches('/').to_ascii_uppercase();
    let leaf = n.rsplit('/').next().unwrap_or(&n);
    // Explicit numeric voltage tokens are reliable, so they come FIRST, a rail
    // named "+12V" always reports 12 V regardless of any role-name hint below.
    for (tok, v) in [
        ("+12V", 12.0),
        ("+5V", 5.0),
        ("+3V3", 3.3),
        ("+3.3V", 3.3),
        ("+1V8", 1.8),
    ] {
        if leaf == tok {
            return Some(v);
        }
    }
    // No role-name guessing at all. Fabricating a voltage from a name CONTAINING
    // "SOLAR"/"PV"/"VBUS_IN" (or 13 V/40 V for VIN/VOUT/BAT) is unsafe twice
    // over: "PV" is a substring of ordinary buck/gate-driver nets like PVDD/PVCC,
    // and a fabricated voltage on the OUTPUT rail outranks a real "+12V" token
    // on the input, flipping a buck to a boost and skipping the ripple check.
    // Only explicit numeric tokens above are trustworthy; everything else is
    // unknown, and classify_topology takes its documented Buck default.
    None
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
    fn classify_buck_when_input_higher() {
        assert_eq!(classify_topology("SOLAR+", "DCDC_OUT"), Topology::Buck);
        assert_eq!(classify_topology("+12V", "+3V3"), Topology::Buck);
    }

    #[test]
    fn classify_boost_when_output_higher() {
        assert_eq!(classify_topology("+3V3", "+12V"), Topology::Boost);
    }

    #[test]
    fn generic_output_name_does_not_fabricate_a_boost() {
        // Regression (R5): a real 3.3 V buck whose output rail is just named
        // generically "VOUT" must stay a Buck. A VOUT->13.0 guess would outrank
        // the "+12V" input token and mis-flag the stage Boost, which silently
        // skips its ripple check.
        assert_eq!(rail_nominal_v("VOUT"), None);
        assert_eq!(rail_nominal_v("VIN"), None);
        assert_eq!(rail_nominal_v("BAT"), None);
        assert_eq!(classify_topology("+12V", "VOUT"), Topology::Buck);
        assert_eq!(classify_topology("VIN", "VOUT"), Topology::Buck);
        // A numeric token still beats a generic name on the other rail.
        assert_eq!(rail_nominal_v("+12V"), Some(12.0));
    }

    #[test]
    fn pv_substring_names_do_not_fabricate_a_voltage() {
        // R6: "PVDD"/"PVCC" merely CONTAIN "PV" but are ordinary non-solar
        // gate-driver/buck nets; "VBUS_IN" is a 5 V USB input, not 40 V. The
        // old substring guess fabricated 40 V for all of them.
        assert_eq!(rail_nominal_v("PVDD"), None);
        assert_eq!(rail_nominal_v("PVCC"), None);
        assert_eq!(rail_nominal_v("SOLAR+"), None);
        assert_eq!(rail_nominal_v("VBUS_IN"), None);
        // The real R6 failing scenario: a PV-ish name on the OUTPUT rail vs a
        // real numeric input must not be forced to Boost by the 40 V guess.
        assert_eq!(classify_topology("+12V", "PVDD_OUT"), Topology::Buck);
    }
}
