//! Loop-stability metrics from an AC sweep: Bode data, gain crossover, and
//! phase margin.
//!
//! ## How the loop is broken / injected
//!
//! Loop-gain measurement needs the loop broken at one point and a small signal
//! injected there. Hauksbee follows the standard SPICE practice: the user names
//! an **injection source** (an independent `Vsource` placed in series at the
//! break point, e.g. between the feedback divider and the error-amp input) and
//! an **output node** (the other side of the break). The AC analysis drives
//! every independent source with unit amplitude, so the node phasors are already
//! the response to that injection. The loop gain magnitude/phase is then read at
//! the output node:
//!
//! ```text
//!            inject Vac = 1
//!   ...err-amp---[ Vinj ]---o output node (feedback)---...
//! ```
//!
//! `T(jw) = V_out(jw)` (the response at the output node to the unit injection
//! around the open loop). Practically, the board author breaks the feedback net
//! and inserts a 0 V `Vsource` named e.g. `VLOOP`; the AC run then yields the
//! loop response directly at the node on the far side. This is the voltage
//! injection / single-break method; it is exact for the small-signal loop gain
//! when the break point is a low-impedance-driving / high-impedance-load node,
//! which is the normal case for an op-amp feedback divider tap.
//!
//! ## Margins
//!
//! - **Gain crossover frequency** `f_c`: the highest-frequency *downward* 0 dB
//!   crossing of `|T|` (gain falling through unity as frequency increases),
//!   found by linear interpolation (in log-frequency, dB) between the
//!   bracketing sweep points. Non-monotonic loops can cross 0 dB several
//!   times; the final descent through unity is the one that governs stability.
//! - **Phase margin**: `180 deg + phase(T)` at `f_c` (how far the phase is from
//!   -180 deg when the gain hits unity). >= 45 deg is the usual comfort bar.
//! - **Phase crossover / gain margin** are reported too: the lowest-frequency
//!   -180 deg phase crossing at or above `f_c`, and the gain (in dB, negated)
//!   there.

use crate::ac::AcResponse;
use crate::{SolveError, SolveResult};
use hauksbee_ir::Circuit;

/// Computed stability margins of a loop response.
#[derive(Debug, Clone)]
pub struct StabilityMargins {
    /// Gain crossover frequency `f_c` (Hz) where |T| = 0 dB, if the gain crosses
    /// unity within the swept band.
    pub gain_crossover_hz: Option<f64>,
    /// Phase margin (degrees) at `f_c`. `None` if there is no gain crossover.
    pub phase_margin_deg: Option<f64>,
    /// Phase crossover frequency (Hz) where phase passes -180 deg.
    pub phase_crossover_hz: Option<f64>,
    /// Gain margin (dB): how far below 0 dB the gain is at the phase crossover.
    pub gain_margin_db: Option<f64>,
    /// The DC / low-frequency gain (dB) at the first swept point.
    pub dc_gain_db: f64,
}

/// A loop-stability analysis bound to one AC response and output node.
pub struct LoopStability<'a> {
    /// `(freq, mag_db, phase_deg)` of the loop gain T(jw), in sweep order.
    pub bode: Vec<(f64, f64, f64)>,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> LoopStability<'a> {
    /// Build from an AC response, reading the loop gain at `output_node`.
    ///
    /// The loop gain is the negative of the return ratio at the break point:
    /// `T(jw) = -V_out(jw) / V_inj`, where `V_inj` is the unit AC injection.
    /// The minus sign is the loop-summing-junction convention (the returned
    /// signal is subtracted at the error node), so a stable single-pole loop
    /// reads ~+90 deg phase margin rather than appearing inverted. Magnitude is
    /// unchanged; only the phase carries the 180 deg of the convention.
    pub fn from_response(
        resp: &'a AcResponse,
        circuit: &Circuit,
        output_node: &str,
    ) -> SolveResult<Self> {
        let mut bode = Vec::with_capacity(resp.points.len());
        for p in &resp.points {
            let v = p.node(circuit, output_node).ok_or_else(|| {
                SolveError::invalid(format!(
                    "node '{output_node}' not found in AC response (cannot measure loop gain)"
                ))
            })?;
            // Loop gain T = -V_out: magnitude same, phase + 180 deg.
            let t = -v;
            let mag = t.norm();
            bode.push((p.freq, 20.0 * mag.max(1e-300).log10(), t.arg().to_degrees()));
        }
        if bode.is_empty() {
            return Err(SolveError::invalid(format!(
                "node '{output_node}' not found in AC response (cannot measure loop gain)"
            )));
        }
        Ok(LoopStability {
            bode,
            _marker: std::marker::PhantomData,
        })
    }

    /// Compute the stability margins from the Bode data.
    pub fn margins(&self) -> StabilityMargins {
        margins_from_bode(&self.bode)
    }
}

/// Compute margins directly from a `(freq, mag_db, phase_deg)` Bode table.
///
/// Phase is unwrapped before searching, so a sweep that winds past -180 deg is
/// handled. The interpolation is linear in `log10(f)` for the crossover
/// frequency and linear in the measured quantity between bracket points.
pub fn margins_from_bode(bode: &[(f64, f64, f64)]) -> StabilityMargins {
    let dc_gain_db = bode.first().map(|p| p.1).unwrap_or(f64::NAN);
    if bode.len() < 2 {
        return StabilityMargins {
            gain_crossover_hz: None,
            phase_margin_deg: None,
            phase_crossover_hz: None,
            gain_margin_db: None,
            dc_gain_db,
        };
    }

    // Unwrap phase so -179 -> -181 doesn't read as a +358 jump.
    let mut phase = Vec::with_capacity(bode.len());
    let mut prev = bode[0].2;
    let mut offset = 0.0;
    phase.push(prev);
    for p in &bode[1..] {
        let mut ph = p.2 + offset;
        while ph - prev > 180.0 {
            ph -= 360.0;
            offset -= 360.0;
        }
        while ph - prev < -180.0 {
            ph += 360.0;
            offset += 360.0;
        }
        phase.push(ph);
        prev = ph;
    }

    // Gain crossover: the unity-gain crossover is the HIGHEST-frequency
    // *downward* 0 dB crossing (gain going from >0 dB to <=0 dB as frequency
    // increases). A non-monotonic loop can cross 0 dB several times (dip below,
    // peak back above); the margin that governs stability is read at the final
    // descent through unity, so taking the first crossing would report an
    // optimistic phase margin. If the gain never crosses 0 dB downward (always
    // below, always above, or only rising through it at the band edge) no
    // crossover is reported, preserving the documented degenerate behavior.
    // Interpolate linearly in log-f / dB between the bracketing points.
    let mut gain_crossover_hz = None;
    let mut phase_margin_deg = None;
    for i in 1..bode.len() {
        let (f0, m0) = (bode[i - 1].0, bode[i - 1].1);
        let (f1, m1) = (bode[i].0, bode[i].1);
        if m0 > 0.0 && m1 <= 0.0 {
            let frac = m0 / (m0 - m1); // where mag hits 0 dB
            let lf = f0.log10() + frac * (f1.log10() - f0.log10());
            let fc = 10f64.powf(lf);
            let ph = phase[i - 1] + frac * (phase[i] - phase[i - 1]);
            gain_crossover_hz = Some(fc);
            phase_margin_deg = Some(180.0 + ph);
            // No break: keep scanning so the last (highest-frequency) downward
            // crossing wins.
        }
    }

    // Phase crossover / gain margin: collect EVERY -180 deg crossing of the
    // unwrapped phase (either direction), interpolating the frequency (in
    // log-f) and the gain at each. Convention: the gain margin is read at the
    // lowest-frequency -180 deg crossing at or above the gain crossover; the
    // first point past unity gain where extra loop gain would push the
    // response onto the critical point. If no crossing lies at/above the gain
    // crossover (or the gain never crosses unity), fall back to the
    // lowest-frequency -180 deg crossing, which matches the single-crossing
    // behavior and errs conservative for conditionally stable loops.
    let mut crossings: Vec<(f64, f64)> = Vec::new(); // (freq_hz, mag_db)
    for i in 1..bode.len() {
        let p0 = phase[i - 1] + 180.0;
        let p1 = phase[i] + 180.0;
        if p0.signum() != p1.signum() && p0 != p1 {
            let frac = p0 / (p0 - p1);
            let f0 = bode[i - 1].0;
            let f1 = bode[i].0;
            let lf = f0.log10() + frac * (f1.log10() - f0.log10());
            let fp = 10f64.powf(lf);
            let m = bode[i - 1].1 + frac * (bode[i].1 - bode[i - 1].1);
            crossings.push((fp, m));
        }
    }
    let chosen = match gain_crossover_hz {
        Some(fc) => crossings
            .iter()
            .find(|&&(fp, _)| fp >= fc)
            .or_else(|| crossings.first()),
        None => crossings.first(),
    };
    let (phase_crossover_hz, gain_margin_db) = match chosen {
        Some(&(fp, m)) => (Some(fp), Some(-m)),
        None => (None, None),
    };

    StabilityMargins {
        gain_crossover_hz,
        phase_margin_deg,
        phase_crossover_hz,
        gain_margin_db,
        dc_gain_db,
    }
}

/// Convenience: phase margin (deg) for a Bode table, or `None` if the gain never
/// crosses unity in band.
pub fn phase_margin(bode: &[(f64, f64, f64)]) -> Option<f64> {
    margins_from_bode(bode).phase_margin_deg
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single-pole loop: T(jw) = A0 / (1 + jw/wp). Magnitude and phase at a
    /// frequency, in dB / degrees.
    fn single_pole(a0: f64, fp: f64, f: f64) -> (f64, f64, f64) {
        let x = f / fp;
        let mag = a0 / (1.0 + x * x).sqrt();
        (f, 20.0 * mag.log10(), -x.atan().to_degrees())
    }

    #[test]
    fn single_pole_phase_margin_is_90() {
        // A dominant single pole well below crossover gives ~90 deg PM.
        let a0 = 1000.0; // 60 dB
        let fp = 10.0;
        let bode: Vec<_> = (0..=600)
            .map(|i| {
                let f = 10f64.powf(i as f64 / 100.0); // 1 Hz .. 1 MHz
                single_pole(a0, fp, f)
            })
            .collect();
        let m = margins_from_bode(&bode);
        let fc = m.gain_crossover_hz.unwrap();
        // Unity-gain bandwidth ~ A0 * fp = 10 kHz.
        assert!((fc - 10_000.0).abs() / 10_000.0 < 0.05, "fc={fc}");
        let pm = m.phase_margin_deg.unwrap();
        assert!((pm - 90.0).abs() < 2.0, "pm={pm}");
    }

    #[test]
    fn non_monotonic_gain_reads_pm_at_last_downward_crossing() {
        // Gain crosses 0 dB three times: down (10..100 Hz), back up
        // (100..1 kHz, e.g. a resonant peak), then finally down (10..100 kHz).
        // The true unity-gain crossover is the LAST downward crossing; reading
        // the first would report an optimistic ~36 deg margin instead of the
        // real ~11.7 deg.
        let bode = vec![
            (1.0, 40.0, -90.0),
            (10.0, 20.0, -120.0),
            (100.0, -5.0, -150.0),
            (1e3, 10.0, -160.0),
            (1e4, 5.0, -165.0),
            (1e5, -10.0, -175.0),
            (1e6, -30.0, -185.0),
        ];
        let m = margins_from_bode(&bode);
        // Interpolated final downward crossing: frac = 5/15 within 1e4..1e5,
        // fc = 10^(4 + 1/3) ~ 21.54 kHz, phase ~ -168.33 deg -> PM ~ 11.67 deg.
        let fc = m.gain_crossover_hz.unwrap();
        assert!(
            (fc - 21_544.0).abs() / 21_544.0 < 0.01,
            "fc={fc}, expected ~21.5 kHz (last downward crossing), not ~46 Hz (first)"
        );
        let pm = m.phase_margin_deg.unwrap();
        assert!(
            (pm - 11.67).abs() < 0.1,
            "pm={pm}, expected ~11.67 deg; the first crossing would give ~36 deg"
        );
        assert!(
            pm < 20.0,
            "the optimistic first-crossing margin must not leak through"
        );
    }

    #[test]
    fn multi_phase_crossing_gm_read_at_crossing_above_gain_crossover() {
        // Phase dips through -180 deg early (1..10 Hz), recovers (10..100 Hz),
        // and crosses again above the gain crossover (10..100 kHz). The gain
        // margin must come from the crossing at/above the gain crossover, not
        // the first one, which sits at 25 dB of loop gain and would report a
        // meaningless GM of -25 dB.
        let bode = vec![
            (1.0, 30.0, -170.0),
            (10.0, 20.0, -190.0),
            (100.0, 10.0, -170.0),
            (1e3, 8.0, -160.0),
            (1e4, -6.0, -170.0),
            (1e5, -20.0, -190.0),
        ];
        let m = margins_from_bode(&bode);
        // Gain crossover: 8 dB -> -6 dB across 1e3..1e4, fc ~ 3.73 kHz.
        let fc = m.gain_crossover_hz.unwrap();
        assert!((fc - 3_727.0).abs() / 3_727.0 < 0.01, "fc={fc}");
        // Relevant -180 deg crossing: midway (in log-f) through 1e4..1e5,
        // fp = 10^4.5 ~ 31.6 kHz, mag = -13 dB -> GM = +13 dB.
        let fp = m.phase_crossover_hz.unwrap();
        assert!(
            (fp - 31_623.0).abs() / 31_623.0 < 0.01,
            "fp={fp}, expected ~31.6 kHz (crossing above fc), not ~3.2 Hz (first)"
        );
        let gm = m.gain_margin_db.unwrap();
        assert!(
            (gm - 13.0).abs() < 0.1,
            "gm={gm}, expected ~13 dB; the first crossing would give -25 dB"
        );
    }

    #[test]
    fn monotonic_loop_margins_unchanged() {
        // A well-behaved monotonic two-pole-style table: one downward 0 dB
        // crossing, one -180 deg crossing above it. The fixed selection rules
        // must reproduce exactly the same interpolated margins as the original
        // first-crossing scan did on such tables.
        let bode = vec![
            (1.0, 60.0, -95.0),
            (10.0, 40.0, -110.0),
            (100.0, 20.0, -130.0),
            (1e3, 5.0, -150.0),
            (1e4, -10.0, -170.0),
            (1e5, -25.0, -190.0),
        ];
        let m = margins_from_bode(&bode);
        // Only 0 dB crossing: 5 -> -10 across 1e3..1e4, frac = 1/3.
        let fc = m.gain_crossover_hz.unwrap();
        let expect_fc = 10f64.powf(3.0 + 1.0 / 3.0);
        assert!((fc - expect_fc).abs() / expect_fc < 1e-9, "fc={fc}");
        let pm = m.phase_margin_deg.unwrap();
        let expect_pm = 180.0 + (-150.0 + (1.0 / 3.0) * -20.0);
        assert!(
            (pm - expect_pm).abs() < 1e-9,
            "pm={pm} expected {expect_pm}"
        );
        // Only -180 deg crossing: -170 -> -190 across 1e4..1e5, frac = 0.5.
        let fp = m.phase_crossover_hz.unwrap();
        let expect_fp = 10f64.powf(4.5);
        assert!((fp - expect_fp).abs() / expect_fp < 1e-9, "fp={fp}");
        let gm = m.gain_margin_db.unwrap();
        let expect_gm = -(-10.0 + 0.5 * -15.0); // 17.5 dB
        assert!(
            (gm - expect_gm).abs() < 1e-9,
            "gm={gm} expected {expect_gm}"
        );
    }
}
