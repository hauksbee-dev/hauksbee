//! Rails whose only would-be source is a part the board marks do-not-populate.
//!
//! A plug-in controller module is often drawn on the schematic and marked DNP
//! so the assembler leaves its socket empty; the module is pushed in by hand
//! and powers a rail from its own 5V or 3V3 pin. Honouring the DNP mark is
//! right, but the rail it alone feeds is then unpowered for the whole run: it
//! back-powers through input clamps to a fraction of a volt, and every level
//! judged on it is judged at that fiction. Unless its name states a voltage,
//! nothing said so, and the user had to guess the fit.
//!
//! This names each such rail and the DNP part that would source it, judged by
//! the same evidence the binder uses to source a rail from a FITTED part: the
//! part card's `[[models.supply_outputs]]` pins and the schematic's
//! `power_out` pins that name a voltage. Fitting the part (`fit = [...]`,
//! `--fit`) makes the binder source the rail, and it leaves this list.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{is_test_point_or_hole, Component, ExtractedBoard};
use hauksbee_ir::NodeId;
use hauksbee_models::ModelLibrary;

use crate::binder::{is_ground, library_resolution, power_rail_voltage, BoundBoard};

/// How every [`DnpSourcedRail::cause`] opens, so a reader of `NET: cause`
/// lines can tell a do-not-populate cause from a held-off regulator's.
pub const DNP_CAUSE_OPENING: &str = "powered only by ";

/// A rail only do-not-populate parts would power.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnpSourcedRail {
    /// The rail's net name.
    pub net: String,
    /// The DNP parts whose supply pin sits on it, by reference.
    pub sources: Vec<String>,
}

impl DnpSourcedRail {
    /// The cause sentence and its remedy, without the net's name (callers
    /// print `NET: cause`): which part would power the rail and how to fit it.
    #[must_use]
    pub fn cause(&self) -> String {
        let one = self.sources.len() == 1;
        let parts = self.sources.join(", ");
        let quoted: Vec<String> = self.sources.iter().map(|s| format!("\"{s}\"")).collect();
        let flags: Vec<String> = self.sources.iter().map(|s| format!("--fit {s}")).collect();
        format!(
            "{DNP_CAUSE_OPENING}{parts}, which the board marks do-not-populate and the run \
             leaves open, so nothing powered it. Fit {} (`fit = [{}]`, or `{}`) if {} plugged \
             in on the real board",
            if one { "it" } else { "them" },
            quoted.join(", "),
            flags.join(" "),
            if one { "it is" } else { "they are" },
        )
    }
}

/// The nets `comp` would source if fitted: its card's supply-output pins and
/// its `power_out` pins that name a voltage.
fn would_source(lib: &ModelLibrary, comp: &Component) -> BTreeSet<i64> {
    let mut nets = BTreeSet::new();
    for pin in &comp.pins {
        let names_rail = pin.kind.starts_with("power_out")
            && !pin.kind.contains("no_connect")
            && power_rail_voltage(&pin.function).is_some();
        if let Some(id) = pin.net.filter(|id| names_rail && *id != 0) {
            nets.insert(id);
        }
    }
    if let Some(model) = library_resolution(lib, comp).model {
        for supply in &model.supply_outputs {
            let pads = model.pins.iter().filter(|(_, role)| **role == supply.role);
            for (pad, _) in pads {
                let pin = comp.pins.iter().find(|p| p.number == *pad);
                if let Some(id) = pin.and_then(|p| p.net).filter(|id| *id != 0) {
                    nets.insert(id);
                }
            }
        }
    }
    nets
}

/// Every rail of `bound` that nothing powers and that only do-not-populate
/// parts would source, ordered by net name. A rail a supply leg, a regulator
/// or a switch from a powered rail already feeds is not one, nor is a net no
/// fitted part touches (it feeds nothing).
#[must_use]
pub fn dnp_sourced_rails(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    bound: &BoundBoard,
) -> Vec<DnpSourcedRail> {
    let mut by_net: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
    for comp in &board.components {
        if !matches!(AssemblyState::of(comp), AssemblyState::DnpAbsent(_)) {
            continue;
        }
        for id in would_source(lib, comp) {
            by_net.entry(id).or_default().insert(comp.reference.clone());
        }
    }
    if by_net.is_empty() {
        return Vec::new();
    }
    let legs: Vec<_> = bound.supplies.iter().map(|l| l.vsource).collect();
    let mut domains = crate::rail_owner::RailDomains::new(&bound.circuit, &bound.behavioral, &legs);
    let board_nodes: HashSet<NodeId> = bound.net_nodes.values().copied().collect();
    domains.join_switches(&bound.circuit, |n| board_nodes.contains(&n));
    let powered: HashSet<&str> = bound.supplies.iter().map(|l| l.net_name.as_str()).collect();
    let fed = |name: &str| {
        powered.contains(name)
            || bound.node(name).is_some_and(|n| {
                domains.fed_by_driver(n)
                    || bound.supplies.iter().any(|l| domains.fed_joined(l.net, n))
            })
    };
    let mut out: Vec<DnpSourcedRail> = by_net
        .into_iter()
        .filter_map(|(id, sources)| {
            let net = board.net(id)?;
            if is_ground(&net.name) || fed(&net.name) {
                return None;
            }
            let feeds = board
                .net_members(id)
                .into_iter()
                .any(|(c, _)| AssemblyState::of(c).is_present() && !is_test_point_or_hole(c));
            feeds.then(|| DnpSourcedRail {
                net: net.name.clone(),
                sources: sources.into_iter().collect(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.net.cmp(&b.net));
    out
}
