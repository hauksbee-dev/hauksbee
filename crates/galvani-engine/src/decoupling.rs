//! Capacitor parasitics: equivalent series resistance (ESR) and inductance
//! (ESL).
//!
//! An ideal capacitor is a perfect short to dI/dt; a real one is not. Its ESR
//! sets the floor on how far a rail sags during a fast load step (the step
//! current flows through ESR as an instantaneous IR drop), and its ESL sets how
//! fast the cap can respond at all. Modelling decoupling caps as ideal makes
//! every rail look better-decoupled than it is; ESR/ESL makes it honest.
//!
//! ## How it is stamped
//!
//! Rather than widen the `Device::Capacitor` IR (shared across crates and both
//! solver paths), an ESR/ESL capacitor is stamped as a **series R-L-C network**
//! between the two original pads, introducing one or two internal nodes:
//!
//! ```text
//!   pad_a ──[ R_esr ]── n1 ──[ L_esl ]── n2 ──[ C ]── pad_b
//! ```
//!
//! This is purely additive: the solver already handles R, L and C, and it works
//! identically in the monolithic and partitioned paths (the RLC island is
//! linear). When ESR and ESL are both zero the network collapses to the ideal
//! capacitor (we skip the zero legs), so existing analytic tests are unchanged.
//!
//! ## Defaults
//!
//! Defaults are looked up from package / dielectric class. They are **opt-in**:
//! the binder stamps ideal capacitors unless decoupling parasitics are requested
//! (a scenario flag or per-part DB metadata). The defaults table is small and
//! cited; see [`EsrEsl::for_class`] and `docs/TRANSIENTS.md`.

use galvani_ir::{Circuit, Device, NodeId};

/// A capacitor's series parasitics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EsrEsl {
    /// Equivalent series resistance (ohms).
    pub esr_ohms: f64,
    /// Equivalent series inductance (henries).
    pub esl_henries: f64,
}

impl EsrEsl {
    /// Zero parasitics: an ideal capacitor.
    pub const IDEAL: EsrEsl = EsrEsl {
        esr_ohms: 0.0,
        esl_henries: 0.0,
    };

    /// True when both parasitics are zero (ideal).
    pub fn is_ideal(&self) -> bool {
        self.esr_ohms == 0.0 && self.esl_henries == 0.0
    }

    /// Default parasitics for a capacitor class. The class is a coarse bucket
    /// chosen from package size and dielectric; per-part overrides win.
    ///
    /// Cited typical values (room temperature, low-frequency ESR):
    ///
    /// | class        | ESR     | ESL    | source                                   |
    /// |--------------|---------|--------|------------------------------------------|
    /// | MLCC 0402    | 50 mΩ   | 0.4 nH | Murata GRM155 datasheets / SimSurfing    |
    /// | MLCC 0603    | 30 mΩ   | 0.6 nH | Murata GRM188 datasheets / SimSurfing    |
    /// | MLCC 0805    | 20 mΩ   | 0.8 nH | Murata GRM21 datasheets / SimSurfing     |
    /// | MLCC 1206    | 15 mΩ   | 1.2 nH | Murata GRM31 datasheets / SimSurfing     |
    /// | Electrolytic | 1.0 Ω   | 5 nH   | Nichicon/Panasonic alum-poly datasheets  |
    /// | Tantalum     | 0.5 Ω   | 3 nH   | KEMET T49x / AVX TAJ datasheets          |
    ///
    /// MLCC ESR is the high-frequency series-resistance minimum; ESL is the
    /// mounted self-inductance (the body plus a short pad loop). Electrolytic /
    /// tantalum ESR is the datasheet 100 kHz figure for a mid-value part; these
    /// are deliberately representative, not a per-MPN table.
    pub fn for_class(class: CapClass) -> EsrEsl {
        match class {
            CapClass::Mlcc0402 => EsrEsl { esr_ohms: 0.050, esl_henries: 0.4e-9 },
            CapClass::Mlcc0603 => EsrEsl { esr_ohms: 0.030, esl_henries: 0.6e-9 },
            CapClass::Mlcc0805 => EsrEsl { esr_ohms: 0.020, esl_henries: 0.8e-9 },
            CapClass::Mlcc1206 => EsrEsl { esr_ohms: 0.015, esl_henries: 1.2e-9 },
            CapClass::Electrolytic => EsrEsl { esr_ohms: 1.0, esl_henries: 5e-9 },
            CapClass::Tantalum => EsrEsl { esr_ohms: 0.5, esl_henries: 3e-9 },
        }
    }

    /// Infer a capacitor class from a KiCad footprint string and value, then
    /// return its default parasitics. Electrolytic/tantalum footprints (CP_*,
    /// large can / EIA tant codes) get the electrolytic/tantalum row; an MLCC
    /// footprint is bucketed by its imperial size code. Falls back to 0603 MLCC.
    pub fn from_footprint(footprint: &str, value_farads: f64) -> EsrEsl {
        let fp = footprint.to_ascii_uppercase();
        // Polarised / bulk classes first.
        if fp.contains("CP_") || fp.contains("ELECTROLYTIC") || fp.contains("CASE-") {
            // Big aluminium-can footprints and large values are electrolytic;
            // small EIA tant case codes (A/B/C/D) are tantalum.
            if value_farads >= 47e-6 || fp.contains("RADIAL") || fp.contains("AXIAL") {
                return EsrEsl::for_class(CapClass::Electrolytic);
            }
            return EsrEsl::for_class(CapClass::Tantalum);
        }
        // MLCC by imperial size code in the footprint name.
        let class = if fp.contains("0402") {
            CapClass::Mlcc0402
        } else if fp.contains("0603") {
            CapClass::Mlcc0603
        } else if fp.contains("0805") {
            CapClass::Mlcc0805
        } else if fp.contains("1206") || fp.contains("1210") {
            CapClass::Mlcc1206
        } else {
            CapClass::Mlcc0603
        };
        EsrEsl::for_class(class)
    }
}

/// Coarse capacitor class buckets for default parasitics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapClass {
    Mlcc0402,
    Mlcc0603,
    Mlcc0805,
    Mlcc1206,
    Electrolytic,
    Tantalum,
}

/// Replace an ideal `Device::Capacitor` (by name) in `circuit` with a series
/// R-L-C network carrying the given parasitics. The capacitor keeps its value,
/// IC and name (the C leg retains the name so reports/assertions still find it);
/// the R and L legs are named `<name>_esr` / `<name>_esl`. Zero legs are
/// skipped, so `EsrEsl::IDEAL` leaves the capacitor untouched.
///
/// Returns true if a capacitor with that name was found and (possibly) modified.
pub fn apply_parasitics(circuit: &mut Circuit, cap_name: &str, p: EsrEsl) -> bool {
    if p.is_ideal() {
        // Nothing to do; the ideal capacitor stays as-is.
        return circuit
            .devices
            .iter()
            .any(|d| matches!(d, Device::Capacitor { name, .. } if name == cap_name));
    }

    // Find the capacitor's endpoints and value.
    let mut found: Option<(usize, NodeId, NodeId, f64, Option<f64>)> = None;
    for (i, d) in circuit.devices.iter().enumerate() {
        if let Device::Capacitor { name, a, b, farads, ic } = d {
            if name == cap_name {
                found = Some((i, *a, *b, *farads, *ic));
                break;
            }
        }
    }
    let Some((idx, a, b, farads, ic)) = found else {
        return false;
    };

    // Build the chain a -[R]- n1 -[L]- n2 -[C]- b, skipping zero legs.
    let mut left = a;
    if p.esr_ohms > 0.0 {
        let n1 = circuit.node(&format!("__esr_{cap_name}"));
        circuit.add(Device::Resistor {
            name: format!("{cap_name}_esr"),
            a: left,
            b: n1,
            ohms: p.esr_ohms,
            tc1: None,
        });
        left = n1;
    }
    if p.esl_henries > 0.0 {
        let n2 = circuit.node(&format!("__esl_{cap_name}"));
        circuit.add(Device::Inductor {
            name: format!("{cap_name}_esl"),
            a: left,
            b: n2,
            henries: p.esl_henries,
            ic: None,
        });
        left = n2;
    }
    // Rewrite the original capacitor in place to span the (possibly new) left
    // node to b, keeping its name/value/IC.
    if let Some(Device::Capacitor { a: ca, b: cb, farads: cf, ic: cic, .. }) =
        circuit.devices.get_mut(idx)
    {
        *ca = left;
        *cb = b;
        *cf = farads;
        *cic = ic;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_table_is_ordered_and_cited() {
        // ESR falls as MLCC gets bigger; electrolytic ESR dwarfs MLCC.
        let c0402 = EsrEsl::for_class(CapClass::Mlcc0402);
        let c1206 = EsrEsl::for_class(CapClass::Mlcc1206);
        let elec = EsrEsl::for_class(CapClass::Electrolytic);
        assert!(c0402.esr_ohms > c1206.esr_ohms);
        assert!(elec.esr_ohms > c0402.esr_ohms * 10.0);
        assert!(elec.esl_henries > c0402.esl_henries);
    }

    #[test]
    fn footprint_inference_buckets_classes() {
        let mlcc = EsrEsl::from_footprint("Capacitor_SMD:C_0402_1005Metric", 100e-9);
        assert_eq!(mlcc, EsrEsl::for_class(CapClass::Mlcc0402));
        let elec = EsrEsl::from_footprint("Capacitor_THT:CP_Radial_D6.3mm_P2.50mm", 100e-6);
        assert_eq!(elec, EsrEsl::for_class(CapClass::Electrolytic));
    }

    #[test]
    fn ideal_leaves_capacitor_untouched() {
        let mut c = Circuit::new();
        let a = c.node("A");
        c.add(Device::Capacitor { name: "C1".into(), a, b: NodeId::GROUND, farads: 1e-6, ic: None });
        let before = c.devices.len();
        assert!(apply_parasitics(&mut c, "C1", EsrEsl::IDEAL));
        assert_eq!(c.devices.len(), before, "ideal must add no devices");
    }

    #[test]
    fn parasitics_insert_series_rlc() {
        let mut c = Circuit::new();
        let a = c.node("RAIL");
        c.add(Device::Capacitor { name: "C1".into(), a, b: NodeId::GROUND, farads: 10e-6, ic: None });
        let p = EsrEsl { esr_ohms: 0.02, esl_henries: 1e-9 };
        assert!(apply_parasitics(&mut c, "C1", p));
        // One R, one L added; the C is rewritten to start at the ESL node.
        let r = c.devices.iter().find(|d| matches!(d, Device::Resistor { name, .. } if name == "C1_esr"));
        let l = c.devices.iter().find(|d| matches!(d, Device::Inductor { name, .. } if name == "C1_esl"));
        assert!(r.is_some(), "ESR leg missing");
        assert!(l.is_some(), "ESL leg missing");
        // The R leg starts at the original rail node.
        if let Some(Device::Resistor { a: ra, .. }) = r {
            assert_eq!(*ra, a, "ESR should start at the cap's original pad");
        }
        // The capacitor no longer touches the rail directly.
        let cap = c.devices.iter().find(|d| matches!(d, Device::Capacitor { name, .. } if name == "C1")).unwrap();
        if let Device::Capacitor { a: ca, b: cb, .. } = cap {
            assert_ne!(*ca, a, "cap should now start at the ESL internal node");
            assert_eq!(*cb, NodeId::GROUND);
        }
    }
}
