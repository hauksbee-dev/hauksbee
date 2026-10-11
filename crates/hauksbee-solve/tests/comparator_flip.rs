//! Comparator output transitions in the primary adaptive march.
//!
//! A comparator whose flip feeds back to its own input (a relaxation
//! oscillator, an integrate-and-fire oscillator whose output resets its integrator) or
//! drives another comparator's input must march through every flip on the
//! plain adaptive solve, under the Newton budget the co-sim scheduler gives a
//! chunk, and put each edge where the circuit puts it. Both boards here are
//! astable, so their t = 0 point is the staged DC with the astable comparators
//! pinned, and the march is the staged one, whose comparators latch their
//! decisions between steps and follow them with the part's propagation delay
//! (`hauksbee_solve::stamp::ComparatorLatch`). Edges and periods are checked
//! against their analytic values.

use hauksbee_ir::Circuit;
use hauksbee_solve::{SolverOptions, Transient, Waveforms};
use hauksbee_testkit::circuits::{
    comparator_cascade, comparator_relaxation, RELAX_C, RELAX_RC, RELAX_RD,
};

/// The co-sim scheduler's chunk march: default adaptive options under its
/// Newton budget.
fn primary_opts() -> SolverOptions {
    SolverOptions {
        max_failed_steps: Some(64),
        ..SolverOptions::default()
    }
}

fn march(c: &Circuit, tstop: f64) -> Waveforms {
    Transient::new(primary_opts())
        .run(c, tstop)
        .unwrap_or_else(|e| panic!("the primary march must carry every flip: {e}"))
}

/// Times `w` crosses `level` going up (`rising`) or down, linearly
/// interpolated between accepted samples.
fn crossings(w: &[f64], t: &[f64], level: f64, rising: bool) -> Vec<f64> {
    (1..w.len())
        .filter(|&i| {
            let (a, b) = (w[i - 1] - level, w[i] - level);
            if rising {
                a < 0.0 && b >= 0.0
            } else {
                a > 0.0 && b <= 0.0
            }
        })
        .map(|i| {
            let f = (level - w[i - 1]) / (w[i] - w[i - 1]);
            f.mul_add(t[i] - t[i - 1], t[i - 1])
        })
        .collect()
}

/// The accepted samples an undelayed output steps AFTER: its decision times.
/// Such an output holds its old rail at the step the decision is taken on and
/// stands at the new one a step later, so an interpolated crossing would sit
/// half that step late.
fn decisions(out: &[f64], t: &[f64], rising: bool) -> Vec<f64> {
    (1..out.len())
        .filter(|&i| (out[i - 1] < 2.5) == rising && (out[i] < 2.5) != rising)
        .map(|i| t[i - 1])
        .collect()
}

#[test]
fn relaxation_oscillator_phases_match_their_rc_solutions() {
    let mut c = Circuit::new();
    comparator_relaxation(&mut c, "", 0.0);
    let w = march(&c, 100e-6);
    let wave = |n: &str| w.node(&c, n).unwrap();
    let rises = decisions(wave("out"), &w.time, true);
    let falls = decisions(wave("out"), &w.time, false);
    assert!(
        rises.len() >= 10,
        "the oscillator runs: rising decisions {rises:?}"
    );

    // Low phase: the diode blocks and the cap discharges from 3.0 V to
    // 2.0 V through the discharge resistor alone.
    let low = RELAX_RD * RELAX_C * 1.5f64.ln();
    // High phase: it charges from 2.0 V to 3.0 V toward the Thevenin target
    // of the rail less the diode drop, read off the board at mid-charge.
    let (out, steer, ct) = (wave("out"), wave("steer"), wave("ct"));
    // The first decision releases the pinned t = 0 state, not a trip.
    let falls = &falls[1..];
    let mut checked = 0;
    for (&fall, &rise) in falls.iter().zip(rises.iter().filter(|r| **r > falls[0])) {
        let measured = rise - fall;
        assert!(
            (measured / low - 1.0).abs() < 5e-3,
            "low phase {measured:.4e} s vs RC solution {low:.4e} s"
        );
        let Some(next_fall) = falls.iter().find(|f| **f > rise) else {
            continue;
        };
        let mid = (1..w.time.len())
            .find(|&i| w.time[i] > rise && ct[i] >= 2.5)
            .expect("the cap passes mid-band while charging");
        let vd = out[mid] - steer[mid];
        let v_inf = (5.0 - vd) * RELAX_RD / (RELAX_RC + RELAX_RD);
        let tau = RELAX_RC * RELAX_RD / (RELAX_RC + RELAX_RD) * RELAX_C;
        let high = tau * ((v_inf - 2.0) / (v_inf - 3.0)).ln();
        let measured = next_fall - rise;
        // The drop moves a few mV across the phase with the falling charge
        // current; 1% covers it.
        assert!(
            (measured / high - 1.0).abs() < 1e-2,
            "high phase {measured:.4e} s vs RC solution {high:.4e} s (diode drop {vd:.3} V)"
        );
        checked += 1;
    }
    assert!(checked >= 8, "every full cycle is checked, got {checked}");
}

#[test]
fn each_edge_lands_one_propagation_delay_after_its_trip() {
    // Each decision is taken where the cap passes a trip, and the output
    // passes mid-swing `tpd` later, while the cap runs on past the trip.
    let tpd = 200e-9;
    let mut c = Circuit::new();
    comparator_relaxation(&mut c, "", tpd);
    let w = march(&c, 100e-6);
    let (out, ct) = (w.node(&c, "out").unwrap(), w.node(&c, "ct").unwrap());
    for (trip, out_rising) in [(3.0, false), (2.0, true)] {
        let trips = crossings(ct, &w.time, trip, !out_rising);
        let edges = crossings(out, &w.time, 2.5, out_rising);
        assert!(
            trips.len() >= 10,
            "the cap passes {trip} V repeatedly: {trips:?}"
        );
        // A trip in the run's last delay has no edge yet.
        for t_trip in trips
            .into_iter()
            .filter(|t| *t < 2.0f64.mul_add(-tpd, 100e-6))
        {
            let edge = edges
                .iter()
                .find(|e| **e > t_trip)
                .unwrap_or_else(|| panic!("no edge follows the {trip} V trip at {t_trip:.4e} s"));
            let lag = edge - t_trip;
            assert!(
                (lag / tpd - 1.0).abs() < 2e-2,
                "edge {lag:.4e} s after the {trip} V trip, against tpd {tpd:.1e} s"
            );
        }
    }
}

/// The integrator's fires and its lowest voltage within `window` of each.
fn fires_and_resets(w: &Waveforms, c: &Circuit, window: f64) -> Vec<(f64, f64)> {
    let m = w.node(c, "m").unwrap();
    crossings(w.node(c, "o3").unwrap(), &w.time, 2.5, true)
        .into_iter()
        .map(|t_fire| {
            let reset = (w.time.iter().zip(m))
                .filter(|(t, _)| **t >= t_fire && **t <= t_fire + window)
                .map(|(_, v)| *v)
                .fold(f64::INFINITY, f64::min);
            (t_fire, reset)
        })
        .collect()
}

/// `K3` fires several times in each of `K2`'s high phases (the integrator
/// charges toward 5 V with a 2 us time constant and trips at 2.1 V, a ~4 us
/// phase), and each fire resets its own integrator.
fn assert_relaxation_stage_fires(w: &Waveforms, c: &Circuit) {
    let phases = crossings(w.node(c, "o2").unwrap(), &w.time, 2.5, true).len();
    let fires = fires_and_resets(w, c, 300e-9);
    assert!(
        phases >= 10 && fires.len() >= 2 * phases,
        "K3 fires at least twice per K2 high phase: {} fires, {phases} phases",
        fires.len()
    );
    for (t_fire, reset) in fires {
        assert!(
            reset < 0.5,
            "fire at {t_fire:.4e} s resets the integrator, min {reset:.3} V"
        );
    }
}

#[test]
fn a_flip_driving_another_comparator_marches_through_the_cascade() {
    let tpd = 20e-9;
    let c = comparator_cascade(tpd);
    let w = march(&c, 100e-6);
    let wave = |n: &str| w.node(&c, n).unwrap();

    // K2 inverts every K1 edge one propagation delay later: it decides as
    // K1's output passes its 2.5 V +- 50 mV trip, which the slewing output
    // reaches within 1% of a delay of its own mid-swing.
    for k1_rising in [true, false] {
        let e1 = crossings(wave("out1"), &w.time, 2.5, k1_rising);
        let e2 = crossings(wave("o2"), &w.time, 2.5, !k1_rising);
        assert!(e1.len() >= 10, "K1 oscillates, edges {e1:?}");
        assert_eq!(
            e1.len(),
            e2.len(),
            "K2 follows every K1 edge: {e1:?} vs {e2:?}"
        );
        for (a, b) in e1.iter().zip(&e2) {
            let lag = b - a;
            assert!(
                (lag / tpd - 1.0).abs() < 5e-2,
                "K2's edge lags K1's by its delay: {lag:.3e} s against {tpd:.1e} s"
            );
        }
    }
    assert_relaxation_stage_fires(&w, &c);
}

#[test]
fn an_undelayed_cascade_marches_on_the_primary_solve_too() {
    // Every output steps at its decision, so K2's input is a rail-to-rail
    // step the event locator must not chase.
    let c = comparator_cascade(0.0);
    let w = march(&c, 100e-6);
    let e1 = crossings(w.node(&c, "out1").unwrap(), &w.time, 2.5, true);
    let e2 = crossings(w.node(&c, "o2").unwrap(), &w.time, 2.5, false);
    assert!(e1.len() >= 10 && e1.len() == e2.len(), "{e1:?} vs {e2:?}");
    assert_relaxation_stage_fires(&w, &c);
}
