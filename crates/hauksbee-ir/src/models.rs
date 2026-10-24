//! Physical device models and their temperature dependence.
//!
//! Parameters follow SPICE naming and defaults so a `.model` card maps across
//! directly. Temperature-dependent quantities (junction saturation current,
//! thermal voltage, built-in potentials) are computed from the circuit's
//! global temperature via the helpers here, keeping that physics in one place.
//!

use serde::{Deserialize, Serialize};

/// serde adapter for `f64` fields that may legitimately hold infinity (used as
/// a "disabled" sentinel for breakdown / Early voltages). JSON has no infinity
/// literal, so we round-trip non-finite values through a tagged string.
mod nonfinite_f64 {
    use serde::{Deserialize, Deserializer, Serializer};

    // serde's `serialize_with` contract fixes the `&T` parameter.
    #[allow(clippy::trivially_copy_pass_by_ref)]
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
#[must_use]
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
    #[must_use]
    pub const fn sign(self) -> f64 {
        match self {
            Self::N => 1.0,
            Self::P => -1.0,
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
    /// Current at which the breakdown knee is placed (A), SPICE's `IBV`.
    ///
    /// `bv` alone does not pin a Zener down. A datasheet states its voltage AT
    /// a test current (a 1N4733A is 5.1 V at 49 mA), and the pair is what makes
    /// the knee sharp enough to regulate. Scaling breakdown by `is` instead
    /// makes the knee as soft as the forward saturation current, which puts a
    /// 5.1 V part at 5.8 V under a normal shunt load. Every vendor Zener card
    /// carries BV and IBV together, so both are read here.
    ///
    /// Defaults to `is`, which is the continuous-at-`-bv` behaviour a model
    /// that names no IBV had before.
    #[serde(default)]
    pub ibv: Option<f64>,
    /// Saturation-current temperature exponent.
    pub xti: f64,
    /// Activation energy / bandgap (eV).
    pub eg: f64,
}

impl Default for DiodeModel {
    fn default() -> Self {
        Self {
            is: 1e-14,
            n: 1.0,
            rs: 0.0,
            cjo: 0.0,
            vj: 1.0,
            m: 0.5,
            tt: 0.0,
            bv: f64::INFINITY,
            ibv: None,
            xti: 3.0,
            eg: EG_SI,
        }
    }
}

impl DiodeModel {
    /// Temperature-corrected saturation current at `t_c` (Celsius).
    #[must_use]
    pub fn is_at(&self, t_c: f64) -> f64 {
        saturation_current(self.is, self.n, self.xti, self.eg, t_c)
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
    /// SPICE IRB (A): the base current at which the base resistance falls
    /// halfway from `rb` toward `rbm` (current crowding). `INFINITY` leaves
    /// the base resistance at `rb` unless `rbm` alone is stated.
    #[serde(with = "nonfinite_f64", default = "f64_infinity")]
    pub irb: f64,
    /// SPICE RBM (Ohms): the base resistance at high current. `None` is
    /// SPICE's default, RBM = RB, so the base resistance never moves.
    #[serde(default)]
    pub rbm: Option<f64>,
    /// Base-emitter and base-collector zero-bias depletion caps (F).
    pub cje: f64,
    pub cjc: f64,
    /// Per-junction depletion grading coefficients (SPICE MJE/MJC). A vendor
    /// card states them; the 0.33 default in their place moves the switching
    /// edge, so the loader passes the card's own values through.
    #[serde(default = "f64_mj")]
    pub mje: f64,
    #[serde(default = "f64_mj")]
    pub mjc: f64,
    /// Per-junction built-in potentials (SPICE VJE/VJC, V) and the shared
    /// forward-bias capacitance knee (SPICE FC). Same story as MJE/MJC: the
    /// equations were always general in them, only the loader was not.
    #[serde(default = "f64_vj")]
    pub vje: f64,
    #[serde(default = "f64_vj")]
    pub vjc: f64,
    #[serde(default = "f64_fc")]
    pub fc: f64,
    /// Forward and reverse transit times (s).
    pub tf: f64,
    pub tr: f64,
    /// High-current (Webster) knee currents (A); `INFINITY` disables the roll-off.
    /// Dropping them contradicts a vendor card rather than approximating it;
    /// `tests/decks/bjt_sgp_high_injection.cir` is the guard and the numbers.
    #[serde(with = "nonfinite_f64", default = "f64_infinity")]
    pub ikf: f64,
    #[serde(with = "nonfinite_f64", default = "f64_infinity")]
    pub ikr: f64,
    /// Base-emitter and base-collector leakage (recombination) saturation
    /// currents (A) and their emission coefficients. These are the low-current
    /// beta droop; zero leakage disables each term.
    #[serde(default)]
    pub ise: f64,
    #[serde(default = "f64_two")]
    pub ne: f64,
    #[serde(default)]
    pub isc: f64,
    #[serde(default = "f64_two")]
    pub nc: f64,
    /// Saturation-current temperature exponent and bandgap (eV).
    pub xti: f64,
    pub eg: f64,
}

impl Default for BjtModel {
    fn default() -> Self {
        Self {
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
            irb: f64::INFINITY,
            rbm: None,
            cje: 0.0,
            cjc: 0.0,
            mje: 0.33,
            mjc: 0.33,
            vje: 0.75,
            vjc: 0.75,
            fc: 0.5,
            tf: 0.0,
            tr: 0.0,
            ikf: f64::INFINITY,
            ikr: f64::INFINITY,
            ise: 0.0,
            ne: 2.0,
            isc: 0.0,
            nc: 2.0,
            xti: 3.0,
            eg: EG_SI,
        }
    }
}

impl BjtModel {
    /// The base resistance at base current `ib` (A) and inverse base charge
    /// `qb_inv` (1/qb, 1 with no base-charge effects), SPICE3's `bjtload`:
    ///
    /// - IRB stated: `rbm + 3(rb - rbm)(tan z - z)/(z tan^2 z)` with
    ///   `z = (-1 + sqrt(1 + 144 ib/(pi^2 IRB))) / ((24/pi^2) sqrt(ib/IRB))`,
    ///   which is `rb` at zero current and falls toward `rbm` as the base
    ///   current crowds to the emitter edge;
    /// - otherwise `rbm + (rb - rbm)/qb`.
    ///
    /// With neither stated it is exactly `rb`, so a card without them is
    /// unchanged.
    #[must_use]
    pub fn base_resistance(&self, ib: f64, qb_inv: f64) -> f64 {
        let Some(rbm) = self
            .rbm
            .filter(|r| r.is_finite() && *r >= 0.0 && *r < self.rb)
        else {
            return self.rb;
        };
        let span = self.rb - rbm;
        if self.irb.is_finite() && self.irb > 0.0 {
            // The SPICE3 constants: 14.59025 = 144/pi^2, 2.4317 = 24/pi^2.
            let a = (ib / self.irb).max(1e-9);
            let z = (-1.0 + 14.590_25f64.mul_add(a, 1.0).sqrt()) / 2.4317 / a.sqrt();
            let t = z.tan();
            3.0f64.mul_add(span * (t - z) / z / t / t, rbm)
        } else {
            span.mul_add(qb_inv, rbm)
        }
    }

    /// Whether the base resistance moves with bias (IRB or RBM stated below RB).
    #[must_use]
    pub fn base_resistance_modulated(&self) -> bool {
        self.rbm
            .is_some_and(|r| r.is_finite() && r >= 0.0 && r < self.rb)
    }
}

/// serde defaults for the SGP parameters whose "off" value is not zero.
const fn f64_infinity() -> f64 {
    f64::INFINITY
}
const fn f64_two() -> f64 {
    2.0
}
const fn f64_mj() -> f64 {
    0.33
}
const fn f64_vj() -> f64 {
    0.75
}
const fn f64_fc() -> f64 {
    0.5
}

impl BjtModel {
    /// Temperature-corrected transport saturation current.
    #[must_use]
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
    /// Threshold voltage at zero `Vsb` (V), stored polarity-FOLDED: positive
    /// for an enhancement device of EITHER polarity (the solver evaluates the
    /// channel in N-channel space). SPICE cards and the model db state VTO in
    /// device convention (negative for an enhancement PMOS); the loaders fold
    /// it by the polarity sign on the way in.
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
    /// TOTAL gate-source overlap/constant capacitance (F): `CGSO·W` from a
    /// SPICE card, or a datasheet-style total `cgs`. `0` = absent (the
    /// default: existing decks stamp no gate charge).
    pub cgs_ov: f64,
    /// TOTAL gate-drain overlap/constant capacitance (F): `CGDO·W` or a
    /// datasheet-style total `cgd`. `0` = absent.
    pub cgd_ov: f64,
    /// TOTAL intrinsic oxide capacitance `Cox·W·L` (F), derived from `TOX`
    /// (with `W`/`L`) at load. Drives the operating-region-dependent
    /// (Meyer-limit) part of the gate charge; `0` = overlap-only caps.
    ///
    /// DELIBERATE DEVIATION from ngspice: ngspice materializes a default
    /// `TOX = 1e-7 m` (plus default `W = L = 100 um`) even when the card
    /// omits them, so every ngspice level-1 MOS carries intrinsic Meyer
    /// capacitance. Here an omitted `TOX` yields `c_ox = 0`, no intrinsic
    /// gate charge, because the bit-identity bar requires default models to
    /// stamp exactly what a model without the field stamps. A deck
    /// that wants the intrinsic caps states `TOX` (and real `W`/`L`).
    pub c_ox: f64,
    /// Body-diode saturation current (A), SPICE `IS`. `0` disables the body
    /// diode entirely.
    ///
    /// DELIBERATE DEVIATION from ngspice: ngspice defaults MOS `IS` to
    /// `1e-14 A`, so its bulk junctions always exist. Here the default is
    /// `0`, no bulk junctions, for the same bit-identity reason as `c_ox`.
    /// A deck that wants the body diode (synchronous rectification, flyback
    /// reverse conduction) states `IS` on the card.
    pub body_is: f64,
    /// Zero-bias bulk-drain depletion capacitance (F), SPICE `CBD`. `0` = absent.
    pub cbd: f64,
    /// Zero-bias bulk-source depletion capacitance (F), SPICE `CBS`. `0` = absent.
    pub cbs: f64,
    /// Bulk-junction built-in potential (V), SPICE `PB`.
    pub pb: f64,
    /// Bulk-junction grading coefficient, SPICE `MJ`.
    pub mj: f64,
    /// Drain ohmic series resistance (ohms), SPICE `RD`. One half of the
    /// datasheet-Rds(on) split (`rd + rs + channel`): a power FET carries most
    /// of its on-state drop here, not in the channel. `0` = ideal drain (the
    /// default). Default-zero is load-bearing: a model without `rd` allocates
    /// no internal drain node and stamps as an ideal drain.
    pub rd: f64,
    /// Source ohmic series resistance (ohms), SPICE `RS`. The other half of the
    /// datasheet-Rds(on) split. `0` = ideal source (the default), bit-identical.
    pub rs: f64,
}

/// Body-diode saturation current (A, ideality 1) for a card that states no `is`.
///
/// Every discrete power MOSFET has a source-drain body diode, and a binder
/// that omits it cannot see a FET wired backwards (the diode bypassing the
/// switch, or a reverse-polarity FET that does not block). An ASSUMED value:
/// the binder records each part bound with it
/// (`BindReport::assumed_body_diodes`).
///
/// Chosen to land on the body-diode forward voltages the datasheets print:
/// at 1e-12 A the diode drops 0.76 V at 4.5 A, against the Vishay `Si7461DP`'s
/// VSD of 0.8 V typical (1.2 V max) at IS = -4.5 A, and 0.83 V at 62 A
/// against the `IRF3205`'s VSD of 1.3 V max at IS = 62 A (before the
/// package's series resistance); at 1 mA it drops 0.54 V. The model field
/// itself still defaults to 0 ([`MosfetModel::body_is`]): a SPICE deck states
/// its own `IS` and stays bit-identical.
pub const GENERIC_BODY_DIODE_IS: f64 = 1e-12;

impl Default for MosfetModel {
    fn default() -> Self {
        Self {
            level: MosLevel::Level1,
            polarity: Polarity::N,
            vto: 1.0,
            kp: 2e-5,
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
            rd: 0.0,
            rs: 0.0,
        }
    }
}

impl MosfetModel {
    /// Effective transconductance `beta = KP * (W/L)`.
    #[must_use]
    pub fn beta(&self) -> f64 {
        self.kp * self.w_over_l
    }

    /// Whether the model carries any gate capacitance (overlap or intrinsic).
    /// The gate terminal conducts (displacement current) exactly when this
    /// holds; a default model keeps the gate an ideal open, bit-identically.
    #[must_use]
    pub fn has_gate_charge(&self) -> bool {
        self.cgs_ov > 0.0 || self.cgd_ov > 0.0 || self.c_ox > 0.0
    }

    /// Whether the model carries bulk-junction physics: the body-diode DC
    /// branch (`body_is`) or either depletion capacitance. The bulk terminal
    /// conducts exactly when this holds.
    #[must_use]
    pub fn has_body_diode(&self) -> bool {
        self.body_is > 0.0 || self.cbd > 0.0 || self.cbs > 0.0
    }
}

#[cfg(test)]
mod base_resistance_tests {
    use super::BjtModel;

    /// The BC847 vendor card: RB 150, IRB 8.356e-4, RBM 0.8289.
    fn bc847() -> BjtModel {
        BjtModel {
            rb: 150.0,
            irb: 8.356e-4,
            rbm: Some(0.8289),
            ..BjtModel::default()
        }
    }

    #[test]
    fn irb_crowding_falls_from_rb_to_rbm_through_the_halfway_point() {
        let m = bc847();
        assert!(m.base_resistance_modulated());
        // Zero current: the full zero-bias RB.
        assert!((m.base_resistance(0.0, 1.0) - 150.0).abs() < 0.1);
        // At IRB the SPICE3 expression sits within a few percent of halfway.
        let mid = 0.5f64.mul_add(150.0 - 0.8289, 0.8289);
        let at_irb = m.base_resistance(m.irb, 1.0);
        assert!((at_irb - mid).abs() < 0.03 * 150.0, "{at_irb} vs {mid}");
        // Monotone falling, and approaching RBM at high current.
        let mut last = f64::INFINITY;
        for k in 0..12 {
            let r = m.base_resistance(1e-7 * 4f64.powi(k), 1.0);
            assert!(r <= last && r >= 0.8289, "step {k}: {r}");
            last = r;
        }
        assert!(m.base_resistance(1.0, 1.0) < 5.0);
    }

    #[test]
    fn without_irb_rbm_scales_with_inverse_base_charge() {
        let m = BjtModel {
            irb: f64::INFINITY,
            ..bc847()
        };
        assert!((m.base_resistance(1e-3, 1.0) - 150.0).abs() < 1e-9);
        let want = 0.5f64.mul_add(150.0 - 0.8289, 0.8289);
        assert!((m.base_resistance(1e-3, 0.5) - want).abs() < 1e-9);
    }

    #[test]
    fn a_card_without_rbm_keeps_rb_fixed() {
        for m in [
            BjtModel {
                rbm: None,
                ..bc847()
            },
            // RBM at or above RB is not a modulation either.
            BjtModel {
                rbm: Some(200.0),
                ..bc847()
            },
        ] {
            assert!(!m.base_resistance_modulated());
            assert!((m.base_resistance(0.1, 0.3) - 150.0).abs() < 1e-12);
        }
    }
}
