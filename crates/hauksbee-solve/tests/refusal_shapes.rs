//! Refusal-shape boards: circuits whose HONEST answer is absurd or an error,
//! pinned so no future "robustness" work can quietly turn them plausible.
//!
//! The failure mode this file guards is the one the adversarial-accuracy loop
//! keeps rediscovering from different directions: a solver whose limiting or
//! clamping machinery converts an unsatisfiable board into a reasonable-looking
//! number, confidently reported. The previous cycle's worst defect was exactly
//! that shape (junction limiting anchored on the lagged iterate accepted a
//! clamped 235 A junction current as a fixed point). These boards are the
//! standing tripwire.

use hauksbee_ir::SpiceLoader;
use hauksbee_solve::{run_op, Probe, SolveResult, SolverOptions};

fn op_volts(deck: &str, node: &str) -> SolveResult<f64> {
    let (c, _) = SpiceLoader::load_with_directives(deck).expect("deck parses");
    let out = run_op(
        &c,
        &SolverOptions::default(),
        &[Probe::parse(&format!("V({node})")).expect("probe")],
    )?;
    Ok(out.rows[0][0])
}

/// 1 mA forced INTO a node whose only exit is a reverse-blocked diode. The
/// model admits no root except through the gmin conditioning shunt, so the
/// honest SPICE answer is the gmin artifact: V = I/gmin = 1e9 V (ngspice-45.2
/// prints exactly 1.000000e+09 on this deck; we land 0.1% away through the
/// reverse-leakage conductance floor). The number is absurd, and that is the
/// point: an engineer sees 1 GV and knows the board is miswired.
///
/// What must NEVER happen is the plausible lie: junction limiting, voltage
/// clamping, or a "robust" fallback turning this into a diode-looking 0.7 V
/// (or any rail-scale voltage) reported as converged. Accept only the two
/// honest outcomes: a refusal, or the absurd gmin root.
#[test]
fn forced_reverse_current_is_absurd_or_refused() {
    let deck = "forced reverse current, no dc path\n\
                I1 0 a DC 1m\n\
                D1 0 a DMOD\n\
                .model DMOD D(IS=1e-14 N=1)\n\
                .op\n.end\n";
    match op_volts(deck, "a") {
        Err(_) => {} // refusal is honest
        Ok(v) => assert!(
            v > 1e8,
            "impossible board reported a PLAUSIBLE root V(a) = {v:.6} V; the only \
             honest converged answer is the gmin artifact near 1e9 V (ngspice: 1e9), \
             so something is clamping this into a lie"
        ),
    }
}

/// The rescued counterpart: one 1 Mohm bleed resistor gives the same board a
/// true root, V(a) = I*R = 1000 V exactly (the reverse diode carries only
/// leakage). The pair is two-sided: the tripwire above cannot be satisfied by
/// a solver that simply refuses everything with a reverse diode in it.
#[test]
fn forced_reverse_current_with_bleed_converges_exactly() {
    let deck = "forced reverse current, 1 Meg bleed\n\
                I1 0 a DC 1m\n\
                D1 0 a DMOD\n\
                RB a 0 1e6\n\
                .model DMOD D(IS=1e-14 N=1)\n\
                .op\n.end\n";
    let v = op_volts(deck, "a").expect("board with a dc path must converge");
    // 1e-3 A * 1e6 ohm = 1000 V; gmin and Is-leakage shift it by ~1e-3 V.
    assert!(
        (v - 1000.0).abs() < 0.01,
        "bleed board must sit at I*R = 1000 V, got {v}"
    );
}
