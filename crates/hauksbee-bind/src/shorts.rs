//! Applying copper shorts to a live circuit (Feature: geometric DRC → sim).
//!
//! Detection ([`hauksbee_extract::drc`]) finds copper from two different nets
//! that overlaps. This module turns such a finding into something the solver
//! feels: the two shorted nets are bridged by a small resistance
//! ([`BRIDGE_OHMS`], a few milliohms, the resistance of a real solder blob),
//! so current flows between them and the stress monitor sees the consequences
//! (rails collapsing, parts over-current). Each applied bridge is also surfaced
//! as a [`FaultEvent`] of kind [`FaultKind::Short`] so the frontend highlights
//! it through the existing fault channel, no frontend change required.
//!
//! Two entry points, both on the scheduler:
//!   - apply every short a [`DrcReport`] detected from the layout, and
//!   - short an arbitrary pair of nets on demand (the what-if solder-bridge
//!     scenario).

use hauksbee_extract::DrcReport;
use hauksbee_ir::{Circuit, Device, NodeId};

use crate::stress::{FaultEvent, FaultKind};

/// Bridge resistance for an applied short (ohms). A real solder bridge / copper
/// whisker is a few milliohms; small enough to drag the two nets together hard,
/// large enough to keep the MNA matrix well conditioned.
pub const BRIDGE_OHMS: f64 = 5e-3;

/// A bridge stamped between two nets.
#[derive(Debug, Clone)]
pub struct AppliedShort {
    pub net_a: String,
    pub net_b: String,
    /// The bridge resistor's device name in the circuit
    /// (`SHORT_<a>_<b>_n<lo>_n<hi>`, the node ids disambiguating name collisions).
    pub device_name: String,
}

/// Stamp a bridge resistor between two distinct nodes, returning the device
/// name, or `None` if the nodes are the same / already bridged.
pub fn stamp_bridge(
    circuit: &mut Circuit,
    a: NodeId,
    b: NodeId,
    name_a: &str,
    name_b: &str,
) -> Option<String> {
    if a == b {
        return None;
    }
    // Idempotency keys on the NODE PAIR, not the concatenated name: net names
    // routinely contain underscores, so "SHORT_{a}_{b}" is not injective over
    // pairs, ("GPIO_1","2") and ("GPIO","1_2") both render "SHORT_GPIO_1_2",
    // and keying on the name silently dropped the second, genuinely distinct
    // short. The node ids are also folded into the device name so two colliding
    // name-pairs still get unique device names in the circuit.
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let already = circuit.devices.iter().any(|d| match d {
        Device::Resistor {
            a: da, b: db, name, ..
        } if name.starts_with("SHORT_") => {
            let (dl, dh) = if da <= db { (*da, *db) } else { (*db, *da) };
            dl == lo && dh == hi
        }
        _ => false,
    });
    if already {
        return None;
    }
    let device_name = format!("SHORT_{name_a}_{name_b}_n{}_n{}", lo.0, hi.0);
    circuit.add(Device::Resistor {
        name: device_name.clone(),
        a,
        b,
        ohms: BRIDGE_OHMS,
        tc1: None,
    });
    Some(device_name)
}

/// Build the [`FaultEvent`] that surfaces an applied short through the fault
/// channel. `value`/`limit` carry the bridge resistance (Ω) so the readout is
/// meaningful even though a short is not a rating violation.
pub fn short_fault(name_a: &str, name_b: &str, t: f64) -> FaultEvent {
    FaultEvent {
        component: format!("SHORT:{name_a}-{name_b}"),
        kind: FaultKind::Short,
        value: BRIDGE_OHMS,
        limit: BRIDGE_OHMS,
        t,
        destroyed: false,
    }
}

/// Resolve the shorted net *names* a [`DrcReport`] found, mapped onto the
/// board's node table. Returns the (name_a, name_b) pairs whose copper actually
/// overlaps (clearance-only violations are not applied), de-duplicated.
pub fn shorted_name_pairs(report: &DrcReport) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = report
        .shorts()
        .map(|f| {
            // Stable ordering by name so the same physical bridge is one pair.
            if f.net_a_name <= f.net_b_name {
                (f.net_a_name.clone(), f.net_b_name.clone())
            } else {
                (f.net_b_name.clone(), f.net_a_name.clone())
            }
        })
        .filter(|(a, b)| !a.is_empty() && !b.is_empty() && a != b)
        .collect();
    pairs.sort();
    pairs.dedup();
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn underscore_net_names_do_not_collide_into_a_dropped_short() {
        // R12: two DISTINCT shorted pairs whose names concatenate to the same
        // "SHORT_GPIO_1_2" string. Keyed on the node pair, both must stamp; the
        // old name-keyed idempotency dropped the second as a false duplicate.
        let mut c = Circuit::new();
        let n1 = c.node("GPIO_1");
        let n2 = c.node("2");
        let n3 = c.node("GPIO");
        let n4 = c.node("1_2");
        let s1 = stamp_bridge(&mut c, n1, n2, "GPIO_1", "2");
        let s2 = stamp_bridge(&mut c, n3, n4, "GPIO", "1_2");
        assert!(
            s1.is_some() && s2.is_some(),
            "both distinct shorts must stamp"
        );
        assert_ne!(s1, s2, "the two bridges must have distinct device names");
        let bridges = c
            .devices
            .iter()
            .filter(|d| d.name().starts_with("SHORT_"))
            .count();
        assert_eq!(bridges, 2, "two bridge resistors present");
        // Re-stamping the SAME node pair is still idempotent (no third bridge).
        assert!(stamp_bridge(&mut c, n2, n1, "2", "GPIO_1").is_none());
    }
}
