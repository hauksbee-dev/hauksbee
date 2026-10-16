//! Crystal load capacitors against the driving IC's card.
//!
//! The extract-layer `crystal_load_cap` check judges the caps against the
//! crystal's own CL when the crystal's value states it. Most boards write only
//! the frequency ("16MHz"), so that check can only note the board's CL. The
//! oscillator's datasheet still bounds the caps: an AVR's crystal-oscillator
//! table gives a recommended range for each of C1 and C2 (12..22 pF), carried
//! on the card as `xtal_cap_min_f` / `xtal_cap_max_f`.
//!
//! A cap a little outside that range can be right for a crystal with a
//! different CL (27 pF suits an 18 pF crystal), so the range itself is not the
//! line. The finding fires only when a load cap is more than twice the card's
//! maximum or under half its minimum: at twice 22 pF the board presents about
//! 26 pF of load, beyond the CL of the crystals the oscillator is characterised
//! for, so no choice of crystal explains it and the oscillator may not start or
//! runs off frequency. Stating the crystal's CL in its value moves the
//! judgement to the exact CL comparison instead.

use hauksbee_bind::binder::resolve;
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::si::crystal_sites;
use hauksbee_extract::{ExtractedBoard, SiCheck, SiFinding, SiReport, SiSeverity};
use hauksbee_models::{Confidence, ModelLibrary};

/// How far past the card's range a load cap must sit before it is a finding
/// (a factor on the maximum, its inverse on the minimum).
const FAR_OUTSIDE: f64 = 2.0;

/// The stray capacitance folded into the board CL the message reports (pF),
/// the same 4 pF the extract-layer crystal check assumes.
const CSTRAY_PF: f64 = 4.0;

/// Append a finding for every crystal whose load caps sit far outside the
/// C1/C2 range its driving IC's card states, when the crystal states no CL.
pub fn append_crystal_caps(board: &ExtractedBoard, lib: &ModelLibrary, report: &mut SiReport) {
    for site in crystal_sites(board) {
        if site.stated_cl_pf.is_some() {
            continue; // judged exactly against the crystal's own CL
        }
        let driver = site
            .nets
            .iter()
            .flat_map(|n| board.net_members(*n))
            .filter(|(c, _)| c.reference != site.crystal.reference)
            .find_map(|(c, _)| {
                let res = resolve(lib, AssemblyState::of(c).fitted()?);
                if matches!(res.confidence, Confidence::Guessed | Confidence::Unresolved) {
                    return None;
                }
                let m = res.model?;
                let lo = m.params.get_f64("xtal_cap_min_f")? * 1e12;
                let hi = m.params.get_f64("xtal_cap_max_f")? * 1e12;
                (lo > 0.0 && hi >= lo).then(|| (c.reference.clone(), m.id, lo, hi))
            });
        let Some((ic, card, lo, hi)) = driver else {
            continue;
        };
        let net_name = |id: i64| board.net(id).map_or_else(String::new, |n| n.name.clone());
        let off: Vec<String> = site
            .nets
            .iter()
            .zip(site.caps.iter())
            .filter_map(|(net, cap)| {
                let (c, pf) = (*cap)?;
                (pf > hi * FAR_OUTSIDE || pf < lo / FAR_OUTSIDE)
                    .then(|| format!("{} {pf:.0} pF on '{}'", c.reference, net_name(*net)))
            })
            .collect();
        if off.is_empty() {
            continue;
        }
        let cl = match site.caps {
            [Some((_, a)), Some((_, b))] => {
                format!(
                    "; the board presents CL ~ {:.1} pF",
                    a * b / (a + b) + CSTRAY_PF
                )
            }
            _ => String::new(),
        };
        let x = site.crystal;
        report.findings.push(SiFinding {
            check: SiCheck::CrystalLoadCap,
            severity: SiSeverity::Medium,
            message: format!(
                "{} ({}): load capacitor {} is far outside the {lo:.0}-{hi:.0} pF C1/C2 range \
                 card '{card}' states for {ic}'s crystal oscillator (more than {FAR_OUTSIDE:.0}x \
                 past it){cl}. No crystal the oscillator is characterised for needs that load, so \
                 it may not start or runs off frequency. Fit load caps inside {lo:.0}-{hi:.0} pF \
                 (about 2 x (CL - {CSTRAY_PF:.0} pF) for the crystal's CL), or state the crystal's \
                 CL in its value (\"{} 18pF\") to judge the caps against it exactly.",
                x.reference,
                x.value,
                off.join(" and "),
                x.value.trim(),
            ),
            refs: std::iter::once(x.reference.clone())
                .chain(site.caps.iter().flatten().map(|(c, _)| c.reference.clone()))
                .chain(std::iter::once(ic))
                .collect(),
            nets: site.nets.iter().map(|n| net_name(*n)).collect(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;

    /// An ATmega328P (DIP-28: XTAL1 pad 9, XTAL2 pad 10) on a 16 MHz crystal
    /// with `cap` on each terminal to ground.
    fn avr_xtal(crystal: &str, cap: &str) -> ExtractedBoard {
        let mut y1 = part("Y1", crystal, &[("1", 1), ("2", 2)]);
        y1.footprint = "Crystal:Crystal_HC49-U_Vertical".into();
        board(
            &[(1, "XTAL1"), (2, "XTAL2"), (3, "GND")],
            vec![
                part("U1", "ATmega328P", &[("9", 1), ("10", 2), ("8", 3)]),
                y1,
                part("C1", cap, &[("1", 1), ("2", 3)]),
                part("C2", cap, &[("1", 2), ("2", 3)]),
            ],
        )
    }

    fn findings(b: &ExtractedBoard) -> Vec<SiFinding> {
        let mut r = SiReport::default();
        append_crystal_caps(b, &ModelLibrary::builtin(), &mut r);
        r.findings
    }

    /// Four times the Uno's 22 pF (88 pF each) is four times the ATmega328P
    /// datasheet's 22 pF maximum: one Medium finding naming the caps, the range,
    /// the card, the MCU and the board's CL.
    #[test]
    fn load_caps_far_past_the_cards_range_fire() {
        let f = findings(&avr_xtal("16MHz", "88p"));
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].severity, SiSeverity::Medium);
        for says in [
            "C1 88 pF on 'XTAL1' and C2 88 pF on 'XTAL2'",
            "12-22 pF C1/C2 range",
            "card 'atmega328p'",
            "U1's crystal oscillator",
            "CL ~ 48.0 pF",
        ] {
            assert!(f[0].message.contains(says), "{says:?} in {}", f[0].message);
        }
        assert_eq!(f[0].refs, ["Y1", "C1", "C2", "U1"]);
    }

    /// Inside the range, and just outside it (27 pF suits an 18 pF crystal,
    /// 10 pF a small-CL one), is silent; so is a crystal that states its CL,
    /// which the exact CL check judges instead.
    #[test]
    fn legal_and_near_range_load_caps_are_silent() {
        for cap in ["22p", "12p", "27p", "33p", "10p", "6.8p"] {
            assert!(findings(&avr_xtal("16MHz", cap)).is_empty(), "{cap}");
        }
        assert!(findings(&avr_xtal("16MHz 18pF", "88p")).is_empty());
    }
}
