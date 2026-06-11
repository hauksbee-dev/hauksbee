//! Physical device models and their temperature dependence.
//!
//! Parameters follow SPICE naming and defaults so a `.model` card maps across
//! directly. Temperature-dependent quantities (junction saturation current,
//! thermal voltage, built-in potentials) are computed from the circuit's
//! global temperature via the helpers here, keeping that physics in one place.

use serde::{Deserialize, Serialize};

/// serde adapter for `f64` fields that may legitimately hold infinity (used as
/// a "disabled" sentinel for breakdown / Early voltages). JSON has no infinity
/// literal, so we round-trip non-finite values through a tagged string.
mod nonfinite_f64 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
        if v.is_finite() {
            s.serialize_f64(*v)
        } else if v.is_nan() {
            s.serialize_str("nan")
        } else if *v > 0.0 {
            s.serialize_str("inf")
        } else {
            s.serialize_str("-inf")
        }
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Num(f64),
        Tag(String),
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        match Repr::deserialize(d)? {
            Repr::Num(x) => Ok(x),
            Repr::Tag(s) => Ok(match s.as_str() {
                "inf" | "+inf" => f64::INFINITY,
                "-inf" => f64::NEG_INFINITY,
                _ => f64::NAN,
            }),
        }
    }
}

/// Boltzmann constant over electron charge, k/q (V/K).
pub const KB_OVER_Q: f64 = 8.617_333_262e-5;
/// Silicon bandgap energy at 0 K (eV), used for IS(T) extrapolation.
pub const EG_SI: f64 = 1.11;
/// SPICE nominal measurement temperature (Kelvin), `TNOM = 27 C`.
pub const TNOM_K: f64 = 300.15;

/// Convert Celsius to Kelvin.
pub fn celsius_to_kelvin(t_c: f64) -> f64 {
    t_c + 273.15
}

/// Thermal voltage `Vt = kT/q` at temperature `t_c` (Celsius).
pub fn thermal_voltage(t_c: f64) -> f64 {
    KB_OVER_Q * celsius_to_kelvin(t_c)
}

/// Saturation current scaled from its nominal value to temperature `t_c`.
///
/// The standard SPICE law:
/// `IS(T) = IS * (T/Tnom)^(XTI/N) * exp(-Eg/(N*Vt) * (1 - T/Tnom))`.
pub fn saturation_current(is_nom: f64, n: f64, xti: f64, eg: f64, t_c: f64) -> f64 {
    let t = celsius_to_kelvin(t_c);
    let ratio = t / TNOM_K;
    let vt = thermal_voltage(t_c);
    is_nom * ratio.powf(xti / n) * (-eg / (n * vt) * (1.0 - ratio)).exp()
}

/// Carrier polarity for transistors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Polarity {
    /// NPN bipolar or N-channel MOS.
    N,
    /// PNP bipolar or P-channel MOS.
    P,
}

impl Polarity {
    /// `+1.0` for N-type, `-1.0` for P-type; multiplies terminal voltages and
    /// currents so one set of equations serves both polarities.
    pub fn sign(self) -> f64 {
        match self {
            Polarity::N => 1.0,
            Polarity::P => -1.0,
        }
    }
}

/// PN junction diode model (SPICE level-1 essentials).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DiodeModel {
    /// Saturation current at `TNOM` (A).
    pub is: f64,
    /// Emission (ideality) coefficient.
    pub n: f64,
    /// Ohmic series resistance (Ohms). Zero disables it.
    pub rs: f64,
    /// Zero-bias junction capacitance (F).
    pub cjo: f64,
    /// Junction built-in potential (V).
    pub vj: f64,
    /// Grading coefficient.
    pub m: f64,
    /// Transit time (s), sets diffusion capacitance.
    pub tt: f64,
    /// Reverse breakdown voltage (V); `f64::INFINITY` disables breakdown.
    #[serde(with = "nonfinite_f64")]
    pub bv: f64,
    /// Saturation-current temperature exponent.
    pub xti: f64,
    /// Activation energy / bandgap (eV).
    pub eg: f64,
}

impl Default for DiodeModel {
    fn default() -> Self {
        DiodeModel {
            is: 1e-14,
            n: 1.0,
            rs: 0.0,
            cjo: 0.0,
            vj: 1.0,
            m: 0.5,
            tt: 0.0,
            bv: f64::INFINITY,
            xti: 3.0,
            eg: EG_SI,
        }
    }
}

impl DiodeModel {
    /// Temperature-corrected saturation current at `t_c` (Celsius).
    pub fn is_at(&self, t_c: f64) -> f64 {
        saturation_current(self.is, self.n, self.xti, self.eg, t_c)
    }

    /// Thermal voltage scaled by the emission coefficient, `N*Vt`.
    pub fn nvt(&self, t_c: f64) -> f64 {
        self.n * thermal_voltage(t_c)
    }
}

/// Bipolar junction transistor, Gummel-Poon basics.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BjtModel {
    /// NPN or PNP.
    pub polarity: Polarity,
    /// Transport saturation current at `TNOM` (A).
    pub is: f64,
    /// Ideal maximum forward beta.
    pub bf: f64,
    /// Ideal maximum reverse beta.
    pub br: f64,
    /// Forward Early voltage (V); `INFINITY` disables base-width modulation.
    #[serde(with = "nonfinite_f64")]
    pub vaf: f64,
    /// Reverse Early voltage (V).
    #[serde(with = "nonfinite_f64")]
    pub var: f64,
    /// Forward emission coefficient.
    pub nf: f64,
    /// Reverse emission coefficient.
    pub nr: f64,
    /// Base, emitter, collector ohmic resistances (Ohms).
    pub rb: f64,
    pub re: f64,
    pub rc: f64,
    /// Base-emitter and base-collector zero-bias depletion caps (F).
    pub cje: f64,
    pub cjc: f64,
    /// Forward and reverse transit times (s).
    pub tf: f64,
    pub tr: f64,
    /// Saturation-current temperature exponent and bandgap (eV).
    pub xti: f64,
    pub eg: f64,
}

impl Default for BjtModel {
    fn default() -> Self {
        BjtModel {
            polarity: Polarity::N,
            is: 1e-16,
            bf: 100.0,
            br: 1.0,
            vaf: f64::INFINITY,
            var: f64::INFINITY,
            nf: 1.0,
            nr: 1.0,
            rb: 0.0,
            re: 0.0,
            rc: 0.0,
            cje: 0.0,
            cjc: 0.0,
            tf: 0.0,
            tr: 0.0,
            xti: 3.0,
            eg: EG_SI,
        }
    }
}

impl BjtModel {
    /// Temperature-corrected transport saturation current.
    pub fn is_at(&self, t_c: f64) -> f64 {
        saturation_current(self.is, self.nf, self.xti, self.eg, t_c)
    }
}

/// Which MOSFET equation set to evaluate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MosLevel {
    /// Shichman-Hodges (SPICE level 1) with a smooth subthreshold tail.
    Level1,
}

/// MOSFET model (level-1 with a subthreshold region).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MosfetModel {
    /// Equation set.
    pub level: MosLevel,
    /// N-channel or P-channel.
    pub polarity: Polarity,
    /// Threshold voltage at zero `Vsb` (V), already signed for the polarity
    /// convention used by the solver (positive for an enhancement NMOS).
    pub vto: f64,
    /// Transconductance parameter `KP = mu*Cox` (A/V^2).
    pub kp: f64,
    /// Channel-length modulation (1/V).
    pub lambda: f64,
    /// Body-effect coefficient (V^0.5).
    pub gamma: f64,
    /// Surface potential (V).
    pub phi: f64,
    /// Geometric width/length ratio (dimensionless, W/L).
    pub w_over_l: f64,
    /// Subthreshold slope factor (ideality); ~1.3 for a real device.
    pub n_sub: f64,
}

impl Default for MosfetModel {
    fn default() -> Self {
        MosfetModel {
            level: MosLevel::Level1,
            polarity: Polarity::N,
            vto: 1.0,
            kp: 2e-5,
            lambda: 0.0,
            gamma: 0.0,
            phi: 0.6,
            w_over_l: 1.0,
            n_sub: 1.3,
        }
    }
}

impl MosfetModel {
    /// Effective transconductance `beta = KP * (W/L)`.
    pub fn beta(&self) -> f64 {
        self.kp * self.w_over_l
    }
}
