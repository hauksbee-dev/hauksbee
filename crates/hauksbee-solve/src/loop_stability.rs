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
//! - **Gain crossover frequency** `f_c`: where `|T| = 1` (0 dB), found by linear
//!   interpolation (in log-frequency, dB) between the bracketing sweep points.
//! - **Phase margin**: `180 deg + phase(T)` at `f_c` (how far the phase is from
//!   -180 deg when the gain hits unity). >= 45 deg is the usual comfort bar.
//! - **Phase crossover / gain margin** are reported too: the frequency where the
//!   phase passes -180 deg and the gain (in dB, negated) there.

use crate::ac::AcResponse;
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
    ) -> Result<Self, String> {
        let mut bode = Vec::with_capacity(resp.points.len());
        for p in &resp.points {
            let v = p.node(circuit, output_node).ok_or_else(|| {
                format!("node '{output_node}' not found in AC response (cannot measure loop gain)")
            })?;
            // Loop gain T = -V_out: magnitude same, phase + 180 deg.
            let t = -v;
            let mag = t.norm();
            bode.push((p.freq, 20.0 * mag.max(1e-300).log10(), t.arg().to_degrees()));
        }
        if bode.is_empty() {
            return Err(format!(
                "node '{output_node}' not found in AC response (cannot measure loop gain)"
            ));
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

    // Gain crossover: first point where mag_db crosses 0 going downward (or any
    // crossing of 0 dB). Interpolate in log-f / dB.
    let mut gain_crossover_hz = None;
    let mut phase_margin_deg = None;
    for i in 1..bode.len() {
        let (f0, m0) = (bode[i - 1].0, bode[i - 1].1);
        let (f1, m1) = (bode[i].0, bode[i].1);
        if (m0 - 0.0).signum() != (m1 - 0.0).signum() && m0 != m1 {
            let frac = m0 / (m0 - m1); // where mag hits 0
            let lf = f0.log10() + frac * (f1.log10() - f0.log10());
            let fc = 10f64.powf(lf);
            let ph = phase[i - 1] + frac * (phase[i] - phase[i - 1]);
            gain_crossover_hz = Some(fc);
            phase_margin_deg = Some(180.0 + ph);
            break;
        }
    }

    // Phase crossover: where unwrapped phase passes -180 deg.
    let mut phase_crossover_hz = None;
    let mut gain_margin_db = None;
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
            phase_crossover_hz = Some(fp);
            gain_margin_db = Some(-m);
            break;
        }
    }

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
}
