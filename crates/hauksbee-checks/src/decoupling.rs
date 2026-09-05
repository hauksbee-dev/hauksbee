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
//! cited; see [`EsrEsl::for_class`] and `docs/checks/TRANSIENTS.md`.

use hauksbee_ir::{Circuit, Device, NodeId};

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
    /// | MLCC 0201    | 80 mΩ   | 0.3 nH | Murata GRM033 datasheets / SimSurfing    |
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
            CapClass::Mlcc0201 => EsrEsl {
                esr_ohms: 0.080,
                esl_henries: 0.3e-9,
            },
            CapClass::Mlcc0402 => EsrEsl {
                esr_ohms: 0.050,
                esl_henries: 0.4e-9,
            },
            CapClass::Mlcc0603 => EsrEsl {
                esr_ohms: 0.030,
                esl_henries: 0.6e-9,
            },
            CapClass::Mlcc0805 => EsrEsl {
                esr_ohms: 0.020,
                esl_henries: 0.8e-9,
            },
            CapClass::Mlcc1206 => EsrEsl {
                esr_ohms: 0.015,
                esl_henries: 1.2e-9,
            },
            CapClass::Electrolytic => EsrEsl {
                esr_ohms: 1.0,
                esl_henries: 5e-9,
            },
            CapClass::Tantalum => EsrEsl {
                esr_ohms: 0.5,
                esl_henries: 3e-9,
            },
        }
    }

    /// Infer a capacitor class from a KiCad footprint string and value, then
    /// return its default parasitics. Electrolytic/tantalum footprints (CP_*,
    /// large can / EIA tant case codes) get the electrolytic/tantalum row; an
    /// MLCC footprint is bucketed by its size code (imperial or the equivalent
    /// metric code). Falls back to 0603 MLCC.
    pub fn from_footprint(footprint: &str, value_farads: f64) -> EsrEsl {
        if let Some(class) = Self::class_from_footprint(footprint, value_farads) {
            return EsrEsl::for_class(class);
        }
        EsrEsl::for_class(CapClass::Mlcc0603)
    }

    /// The inferred [`CapClass`] for a footprint + value, or `None` when nothing
    /// in the name is recognised (the caller then falls back to a default MLCC).
    /// Split out so the inference is unit-testable on its own.
    pub fn class_from_footprint(footprint: &str, value_farads: f64) -> Option<CapClass> {
        let fp = footprint.to_ascii_uppercase();

        // Polarised / bulk classes first. An explicit tantalum marker is
        // unambiguous; otherwise large aluminium-can footprints and large
        // values are electrolytic, and the small EIA case-code parts are
        // tantalum.
        let tantalum_marker = fp.contains("TANTALUM")
            || fp.contains("TANT")
            // KiCad's tantalum library footprints: CP_EIA-3216-18_..., CASE-A..D.
            || fp.contains("EIA-3216")
            || fp.contains("EIA-3528")
            || fp.contains("EIA-6032")
            || fp.contains("EIA-7343")
            || (fp.contains("CASE-")
                && (fp.contains("CASE-A")
                    || fp.contains("CASE-B")
                    || fp.contains("CASE-C")
                    || fp.contains("CASE-D")));
        if tantalum_marker {
            return Some(CapClass::Tantalum);
        }
        if fp.contains("CP_") || fp.contains("ELECTROLYTIC") || fp.contains("CASE-") {
            if value_farads >= 47e-6 || fp.contains("RADIAL") || fp.contains("AXIAL") {
                return Some(CapClass::Electrolytic);
            }
            return Some(CapClass::Tantalum);
        }

        // MLCC by size code. Recognise both the imperial code (0402) and its
        // metric equivalent (1005 = 0402, 1608 = 0603, 2012 = 0805, 3216/3225 =
        // 1206/1210). KiCad C_0402_1005Metric names carry both; match either.
        let class = if fp.contains("0201") || fp.contains("0603METRIC") {
            // 0201 imperial == 0603 metric; the metric token here is the small
            // part, not the 0603 imperial body.
            CapClass::Mlcc0201
        } else if fp.contains("0402") || fp.contains("1005METRIC") {
            CapClass::Mlcc0402
        } else if fp.contains("0603") || fp.contains("1608METRIC") {
            CapClass::Mlcc0603
        } else if fp.contains("0805") || fp.contains("2012METRIC") {
            CapClass::Mlcc0805
        } else if fp.contains("1206")
            || fp.contains("1210")
            || fp.contains("3216METRIC")
            || fp.contains("3225METRIC")
        {
            CapClass::Mlcc1206
        } else {
            return None;
        };
        Some(class)
    }
}

/// Coarse capacitor class buckets for default parasitics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapClass {
    Mlcc0201,
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
        if let Device::Capacitor {
            name,
            a,
            b,
            farads,
            ic,
        } = d
        {
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
    if let Some(Device::Capacitor {
        a: ca,
        b: cb,
        farads: cf,
        ic: cic,
        ..
    }) = circuit.devices.get_mut(idx)
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
    fn footprint_inference_handles_metric_codes_and_0201() {
        // Metric size codes map to the same class as their imperial equivalent.
        assert_eq!(
            EsrEsl::class_from_footprint("Capacitor_SMD:C_0603_1608Metric", 100e-9),
            Some(CapClass::Mlcc0603)
        );
        assert_eq!(
            EsrEsl::class_from_footprint("Capacitor_SMD:C_0805_2012Metric", 1e-6),
            Some(CapClass::Mlcc0805)
        );
        assert_eq!(
            EsrEsl::class_from_footprint("Capacitor_SMD:C_1206_3216Metric", 10e-6),
            Some(CapClass::Mlcc1206)
        );
        // 0201 imperial (== 0603 metric) is its own small class.
        assert_eq!(
            EsrEsl::class_from_footprint("Capacitor_SMD:C_0201_0603Metric", 10e-9),
            Some(CapClass::Mlcc0201)
        );
        // Its ESR is the highest of the MLCC ladder (smallest body).
        let c0201 = EsrEsl::for_class(CapClass::Mlcc0201);
        let c0402 = EsrEsl::for_class(CapClass::Mlcc0402);
        assert!(c0201.esr_ohms > c0402.esr_ohms);
    }

    #[test]
    fn footprint_inference_distinguishes_tantalum_from_electrolytic() {
        // An explicit tantalum case-code footprint is tantalum regardless of value.
        assert_eq!(
            EsrEsl::class_from_footprint("Capacitor_Tantalum_SMD:CP_EIA-3216-18_Kemet-A", 10e-6),
            Some(CapClass::Tantalum)
        );
        assert_eq!(
            EsrEsl::class_from_footprint("CP_CASE-B_Tantalum", 22e-6),
            Some(CapClass::Tantalum)
        );
        // A big aluminium can with a large value is electrolytic.
        assert_eq!(
            EsrEsl::class_from_footprint("CP_Radial_D10.0mm_P5.00mm", 470e-6),
            Some(CapClass::Electrolytic)
        );
        // Tantalum and electrolytic ESR differ (tant is the lower-ESR bulk class).
        let tant = EsrEsl::for_class(CapClass::Tantalum);
        let elec = EsrEsl::for_class(CapClass::Electrolytic);
        assert!(tant.esr_ohms < elec.esr_ohms);
    }

    #[test]
    fn footprint_inference_falls_back_to_default_mlcc() {
        // An unrecognised footprint string yields no class; from_footprint then
        // uses the documented 0603 MLCC default (behaviour preserved).
        assert_eq!(EsrEsl::class_from_footprint("MysteryPackage", 100e-9), None);
        assert_eq!(
            EsrEsl::from_footprint("MysteryPackage", 100e-9),
            EsrEsl::for_class(CapClass::Mlcc0603)
        );
    }

    #[test]
    fn ideal_leaves_capacitor_untouched() {
        let mut c = Circuit::new();
        let a = c.node("A");
        c.add(Device::Capacitor {
            name: "C1".into(),
            a,
            b: NodeId::GROUND,
            farads: 1e-6,
            ic: None,
        });
        let before = c.devices.len();
        assert!(apply_parasitics(&mut c, "C1", EsrEsl::IDEAL));
        assert_eq!(c.devices.len(), before, "ideal must add no devices");
    }

    #[test]
    fn parasitics_insert_series_rlc() {
        let mut c = Circuit::new();
        let a = c.node("RAIL");
        c.add(Device::Capacitor {
            name: "C1".into(),
            a,
            b: NodeId::GROUND,
            farads: 10e-6,
            ic: None,
        });
        let p = EsrEsl {
            esr_ohms: 0.02,
            esl_henries: 1e-9,
        };
        assert!(apply_parasitics(&mut c, "C1", p));
        // One R, one L added; the C is rewritten to start at the ESL node.
        let r = c
            .devices
            .iter()
            .find(|d| matches!(d, Device::Resistor { name, .. } if name == "C1_esr"));
        let l = c
            .devices
            .iter()
            .find(|d| matches!(d, Device::Inductor { name, .. } if name == "C1_esl"));
        assert!(r.is_some(), "ESR leg missing");
        assert!(l.is_some(), "ESL leg missing");
        // The R leg starts at the original rail node.
        if let Some(Device::Resistor { a: ra, .. }) = r {
            assert_eq!(*ra, a, "ESR should start at the cap's original pad");
        }
        // The capacitor no longer touches the rail directly.
        let cap = c
            .devices
            .iter()
            .find(|d| matches!(d, Device::Capacitor { name, .. } if name == "C1"))
            .unwrap();
        if let Device::Capacitor { a: ca, b: cb, .. } = cap {
            assert_ne!(*ca, a, "cap should now start at the ESL internal node");
            assert_eq!(*cb, NodeId::GROUND);
        }
    }
}
