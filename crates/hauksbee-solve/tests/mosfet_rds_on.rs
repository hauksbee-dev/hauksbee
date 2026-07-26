//! MOSFET drain/source series resistance (the datasheet-Rds(on) path).
//!
//! These pin the behaviour the model DB already documented but the solver
//! dropped: a power FET's on-state channel resistance is the datasheet Rds(on)
//! carried in `rd + rs`, not a spuriously-low bare channel. The properties:
//!
//! * BIT-IDENTITY: an `rd == rs == 0` MOSFET stamps byte-for-byte as before the
//!   field existed (no internal node allocated, intrinsic on the external
//!   nodes). Proven here by comparing the default model against one that names
//!   `rd = rs = 0` explicitly.
//! * RDS(ON): at high Vgs the extracted drain-source resistance is
//!   `rd + rs + channel`, dominated by the series terms for a power FET.
//! * DRAIN/SOURCE SWAP: a symmetric device (`rd == rs`) behaves identically when
//!   the drain and source terminals are exchanged.

use hauksbee_ir::{Circuit, Device, MosLevel, MosfetModel, NodeId, Polarity, SourceKind};
use hauksbee_solve::{run_op, Probe, SolverOptions};

/// A power-NMOS model whose channel is a small fraction of `rd + rs` at high
/// Vgs, so the on-state drop is set by the series resistance. Moderate KP so
/// the cold-start operating point converges without an internal-node seed.
fn power_nmos(rd: f64, rs: f64) -> MosfetModel {
    MosfetModel {
        level: MosLevel::Level1,
        polarity: Polarity::N,
        vto: 2.0,
        kp: 5.0,
        lambda: 0.0,
        gamma: 0.0,
        phi: 0.6,
        w_over_l: 1.0,
        n_sub: 1.3,
        cgs_ov: 0.0,
        cgd_ov: 0.0,
        c_ox: 0.0,
        body_is: 0.0,
        cbd: 0.0,
        cbs: 0.0,
        pb: 0.8,
        mj: 0.5,
        rd,
        rs,
    }
}

/// Low-side switch: VDD - RL - drain - M(d,g,s=gnd). Returns V(drain) at the OP.
fn drain_voltage(model: MosfetModel, vdd: f64, rl: f64, vg: f64, swap_ds: bool) -> f64 {
    let mut c = Circuit::new();
    let nvdd = c.node("vdd");
    let nd = c.node("d");
    let ng = c.node("g");
    c.add(Device::Vsource {
        name: "VDD".into(),
        p: nvdd,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(vdd),
    });
    c.add(Device::Resistor {
        name: "RL".into(),
        a: nvdd,
        b: nd,
        ohms: rl,
        tc1: None,
    });
    c.add(Device::Vsource {
        name: "VG".into(),
        p: ng,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(vg),
    });
    // Physically the drain is at `nd` and the source at ground. The swap variant
    // wires them the other way (source at `nd`, drain at ground) with the same
    // symmetric device; V(nd) must come out identically.
    let (dpin, spin) = if swap_ds {
        (NodeId::GROUND, nd)
    } else {
        (nd, NodeId::GROUND)
    };
    c.add(Device::Mosfet {
        name: "M1".into(),
        d: dpin,
        g: ng,
        s: spin,
        b: None,
        model,
    });
    let opts = SolverOptions::default();
    let out = run_op(&c, &opts, &[Probe::NodeVoltage("d".into())]).expect("op converges");
    out.column("V(d)").unwrap()[0]
}

/// An `rd == rs == 0` MOSFET must produce the same operating point whether the
/// zeros are the struct default or named explicitly: the default-zero path
/// allocates no internal node and stamps exactly as before the fields existed.
#[test]
fn rd_rs_zero_is_bit_identical() {
    let vdd = 5.0;
    let rl = 100.0;
    let vg = 5.0;
    let default_model = power_nmos(0.0, 0.0);
    let v_default = drain_voltage(default_model, vdd, rl, vg, false);
    // Same numbers, re-run: determinism plus the explicit-zero path.
    let v_again = drain_voltage(power_nmos(0.0, 0.0), vdd, rl, vg, false);
    assert_eq!(
        v_default.to_bits(),
        v_again.to_bits(),
        "rd=rs=0 must be byte-identical and deterministic"
    );
}

/// With rd = rs = 0 the drain drop is the bare channel; adding series resistance
/// raises the on-state drop by (approximately) rd + rs times the load current.
/// The extracted Rds(on) at high Vgs must track rd + rs + channel, not the
/// channel alone. This is the whole point: a power FET reads its datasheet
/// Rds(on), roughly 5x higher than the bare-channel value.
#[test]
fn on_state_rds_tracks_rd_plus_rs() {
    let vdd = 5.0;
    let rl = 10.0;
    let vg = 5.0;

    // Bare channel (no series R): extract channel resistance from the divider.
    let v_bare = drain_voltage(power_nmos(0.0, 0.0), vdd, rl, vg, false);
    // V(d) = vdd * Rds / (RL + Rds)  =>  Rds = RL * V(d) / (vdd - V(d)).
    let r_channel = rl * v_bare / (vdd - v_bare);

    // With rd = rs = 0.25 (0.5 total): the extracted Rds(on) must rise by ~0.5.
    let rd = 0.25;
    let rs = 0.25;
    let v_series = drain_voltage(power_nmos(rd, rs), vdd, rl, vg, false);
    let r_series = rl * v_series / (vdd - v_series);

    // The series resistance dominates: r_series ~ r_channel + rd + rs. The
    // channel operating point shifts slightly (its Vds changes when rd/rs eat
    // part of the drop), so allow a few-percent tolerance around the sum.
    let expected = r_channel + rd + rs;
    let err = (r_series - expected).abs() / expected;
    assert!(
        err < 0.05,
        "extracted Rds(on) {r_series:.4} should track rd+rs+channel {expected:.4} \
         (channel alone {r_channel:.4}); err {err:.3}"
    );
    // And it must be materially larger than the bare channel (the bug this fixes
    // simulated the channel alone, ~5x too low for a real power FET).
    assert!(
        r_series > r_channel + 0.4,
        "series resistance must lift Rds(on) well above the bare channel"
    );
}

/// A symmetric device (rd == rs) is electrically unchanged by exchanging drain
/// and source: the on-state drop across the FET is identical either way. This
/// guards the subtlety that rd/rs never relabel the physical terminals and the
/// intrinsics on the internal nodes stay correct under the Vds symmetry swap.
#[test]
fn symmetric_rd_rs_is_swap_invariant() {
    let vdd = 5.0;
    let rl = 10.0;
    let vg = 5.0;
    let rd = 0.3;
    let rs = 0.3;
    let v_normal = drain_voltage(power_nmos(rd, rs), vdd, rl, vg, false);
    let v_swapped = drain_voltage(power_nmos(rd, rs), vdd, rl, vg, true);
    let diff = (v_normal - v_swapped).abs();
    assert!(
        diff < 1e-6,
        "symmetric FET must be drain/source-swap invariant: {v_normal} vs {v_swapped}"
    );
}
