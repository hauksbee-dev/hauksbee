//! Shared fixture builders for the S2 graded-board benchmark harness
//!.
//!
//! This file is the single source of truth for the two benchmark topologies.
//! It is not a library module: keeping it out of `lib.rs` avoids shipping test
//! scaffolding in the public API. Instead both consumers pull it in by path:
//!
//! * `benches/graded_boards.rs` via `#[path = "fixtures.rs"] mod fixtures;`
//! * `tests/rail_tear.rs`       via `#[path = "../benches/fixtures.rs"] mod fixtures;`
//!
//! so the mirror-array builder the exactness test relies on and the one the
//! benchmark times are literally the same code, and cannot drift apart.

use hauksbee_ir::{BjtModel, Circuit, NodeId, Polarity};
use hauksbee_testkit::circuits::{bjt, cap_ic, res, vdc};

/// An `n`-stage linear RC ladder driven by a 1 V DC source.
///
/// ```text
/// in(1V) --[R0]-- n0 --[R1]-- n1 --[R2]-- ... --[R{n-1}]-- n{n-1}
///                 |          |                              |
///               [C0]       [C1]                          [C{n-1}]
///                 |          |                              |
///                gnd        gnd                            gnd
/// ```
///
/// The near stages charge within a short run while the far stages barely move
/// (diffusive `~k^2` delay down the ladder), so it exercises the monolithic
/// sparse path on a genuinely large (`n`-node) but cheap-per-step matrix. It is
/// the "guard the easy linear case against regression" board from §9.1: the
/// parallel/compiled paths must not make this slower.
///
/// `r_ohms` / `c_farads` per stage set the stage time constant (`R*C`); the
/// defaults below give `1 us`.
#[must_use]
pub fn build_rc_ladder(n: usize) -> Circuit {
    let r_ohms = 1e3;
    let c_farads = 1e-9;
    let mut c = Circuit::new();
    let vin = c.node("in");
    vdc(&mut c, "VIN", vin, 1.0);
    // Chain: each stage is a series R from the previous node plus a shunt C to
    // ground. `prev` walks the chain so stage k reads stage k-1's node.
    let mut prev = vin;
    for k in 0..n {
        let node = c.node(&format!("n{k}"));
        res(&mut c, &format!("R{k}"), prev, node, r_ohms);
        cap_ic(
            &mut c,
            &format!("C{k}"),
            node,
            NodeId::GROUND,
            c_farads,
            0.0,
        );
        prev = node;
    }
    c
}

/// Build an `n`-block current-mirror array sharing a SHUNT-FED analog rail.
///
/// +5V --[`R_shunt` 1k]-- `VDD_A` --(per block: mirror emitters + load R)
/// Each block: ref resistor -> diode-connected Q1 mirror reference -> Q2 mirror
/// output charges an RC load pulled up to `VDD_A`. Bias is quasi-static
/// (DC drive) so the array reaches a steady operating point, exactly like a
/// steady-state window with latched mirror references.
///
/// This is the fixture from `tests/rail_tear.rs`: the rail is NOT an ideal
/// source, it is fed from +5 V through a 1 kOhm sense shunt, so the rail sags
/// with total array current and couples every block to every other through that
/// one shared node. That single shared, non-ideal node is what the tear has to
/// reproduce, and what makes the array a clean scaling sweep (24/90/240) on
/// identical topology.
///
/// Returns the circuit plus the list of load-node names, so a caller can
/// probe the per-block outputs.
#[must_use]
pub fn build_shunt_array(n: usize) -> (Circuit, Vec<String>) {
    let mut c = Circuit::new();
    let p5 = c.node("+5V");
    vdc(&mut c, "V5", p5, 5.0);
    // The defining element: the sense shunt feeding the analog rail.
    let avdd = c.node("VDD_A");
    res(&mut c, "R_shunt", p5, avdd, 1e3);

    // PNP high-side mirror: emitters on the rail, like a high-side
    // mirror array on a shared rail.
    let model = BjtModel {
        polarity: Polarity::P,
        is: 1e-15,
        bf: 150.0,
        vaf: 80.0,
        nf: 1.0,
        ..BjtModel::default()
    };

    let mut hold_nodes = Vec::new();
    for k in 0..n {
        // A current mirror whose transistor EMITTERS sit on the shared rail, the
        // way large PNP mirror arrays hang many emitters off one supply rail. Each block
        // therefore draws real supply current through the shunt, so the rail sags
        // with the total array load - the exact coupling we must reproduce.
        // Reference leg: rail -> RR -> diode-connected mirror reference.
        let rref = c.node(&format!("ref{k}"));
        res(&mut c, &format!("RR{k}"), avdd, rref, 10e3);
        // Diode-connected reference transistor with emitter on the rail.
        bjt(&mut c, &format!("Q1_{k}"), rref, rref, avdd, &model);
        // Mirror output transistor, emitter on the rail, collector -> load node.
        let hold = c.node(&format!("hold{k}"));
        bjt(&mut c, &format!("Q2_{k}"), hold, rref, avdd, &model);
        // Load RC to ground.
        res(&mut c, &format!("RH{k}"), hold, NodeId::GROUND, 47e3);
        cap_ic(&mut c, &format!("CH{k}"), hold, NodeId::GROUND, 1e-9, 0.0);
        hold_nodes.push(format!("hold{k}"));
    }
    (c, hold_nodes)
}
