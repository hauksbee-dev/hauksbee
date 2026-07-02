//! Shared fixture builders for the S2 graded-board benchmark harness
//! (`docs/dev-plans/03-solver-performance.md` §9).
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

use hauksbee_ir::{BjtModel, Circuit, Device, NodeId, Polarity, SourceKind};

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
pub fn build_rc_ladder(n: usize) -> Circuit {
    let r_ohms = 1e3;
    let c_farads = 1e-9;
    let mut c = Circuit::new();
    let vin = c.node("in");
    c.add(Device::Vsource {
        name: "VIN".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(1.0),
    });
    // Chain: each stage is a series R from the previous node plus a shunt C to
    // ground. `prev` walks the chain so stage k reads stage k-1's node.
    let mut prev = vin;
    for k in 0..n {
        let node = c.node(&format!("n{k}"));
        c.add(Device::Resistor {
            name: format!("R{k}"),
            a: prev,
            b: node,
            ohms: r_ohms,
            tc1: None,
        });
        c.add(Device::Capacitor {
            name: format!("C{k}"),
            a: node,
            b: NodeId::GROUND,
            farads: c_farads,
            ic: Some(0.0),
        });
        prev = node;
    }
    c
}

/// Build an `n`-block current-mirror array sharing a SHUNT-FED analog rail.
///
/// +5V --[R_shunt 1k]-- ANALOG_VDD --(per block: mirror emitters + membrane R)
/// Each block: ref resistor -> diode-connected Q1 mirror reference -> Q2 mirror
/// output charges a membrane RC pulled up to ANALOG_VDD. Bias is quasi-static
/// (DC drive) so the array reaches a steady operating point, exactly like an
/// inference window with latched weights.
///
/// This is the fixture from `tests/rail_tear.rs`: the rail is NOT an ideal
/// source, it is fed from +5 V through a 1 kOhm sense shunt, so the rail sags
/// with total array current and couples every block to every other through that
/// one shared node. That single shared, non-ideal node is what the tear has to
/// reproduce, and what makes the array a clean scaling sweep (24/90/240) on
/// identical topology.
///
/// Returns the circuit plus the list of membrane node names, so a caller can
/// probe the per-block outputs.
pub fn build_shunt_array(n: usize) -> (Circuit, Vec<String>) {
    let mut c = Circuit::new();
    let p5 = c.node("+5V");
    c.add(Device::Vsource {
        name: "V5".into(),
        p: p5,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });
    // The defining element: the sense shunt feeding the analog rail.
    let avdd = c.node("ANALOG_VDD");
    c.add(Device::Resistor {
        name: "R_shunt".into(),
        a: p5,
        b: avdd,
        ohms: 1e3,
        tc1: None,
    });

    // PNP high-side mirror: emitters on the rail, like the board's ANALOG_VDD
    // current mirrors.
    let model = BjtModel {
        polarity: Polarity::P,
        is: 1e-15,
        bf: 150.0,
        vaf: 80.0,
        nf: 1.0,
        ..BjtModel::default()
    };

    let mut membranes = Vec::new();
    for k in 0..n {
        // A current mirror whose transistor EMITTERS sit on the shared rail, the
        // way the Tarski PNP mirrors hang 720 emitters off ANALOG_VDD. Each block
        // therefore draws real supply current through the shunt, so the rail sags
        // with the total array load - the exact coupling we must reproduce.
        // Reference leg: rail -> RR -> diode-connected mirror reference.
        let rref = c.node(&format!("ref{k}"));
        c.add(Device::Resistor {
            name: format!("RR{k}"),
            a: avdd,
            b: rref,
            ohms: 10e3,
            tc1: None,
        });
        // Diode-connected reference transistor with emitter on the rail.
        c.add(Device::Bjt {
            name: format!("Q1_{k}"),
            c: rref,
            b: rref,
            e: avdd,
            model,
        });
        // Mirror output transistor, emitter on the rail, collector -> membrane.
        let mem = c.node(&format!("mem{k}"));
        c.add(Device::Bjt {
            name: format!("Q2_{k}"),
            c: mem,
            b: rref,
            e: avdd,
            model,
        });
        // Membrane RC to ground.
        c.add(Device::Resistor {
            name: format!("RM{k}"),
            a: mem,
            b: NodeId::GROUND,
            ohms: 47e3,
            tc1: None,
        });
        c.add(Device::Capacitor {
            name: format!("CM{k}"),
            a: mem,
            b: NodeId::GROUND,
            farads: 1e-9,
            ic: Some(0.0),
        });
        membranes.push(format!("mem{k}"));
    }
    (c, membranes)
}
