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
use hauksbee_extract::si::{
    crystal_sites, driver_caps_with, CrystalCards, CrystalFacts, IntegratedCaps,
};
use hauksbee_extract::{ExtractedBoard, SiCheck, SiFinding, SiReport, SiSeverity};
use hauksbee_models::{Confidence, ModelLibrary};

/// What the bound cards say about every crystal on the board.
///
/// That is its own card's load capacitance (`cl_f`) and shunt/motional
/// capacitance (`c0_f`, `c1_f`), and the on-chip load capacitance of the
/// oscillator pins that drive it (`pin_capability.xtal_int_caps`). The
/// extract-layer `crystal_load_cap` check judges the board against these.
///
/// The driver is found on either crystal net, or one series resistor away (the
/// RP2040 damping-resistor topology).
#[must_use]
pub fn crystal_cards(board: &ExtractedBoard, lib: &ModelLibrary) -> CrystalCards {
    let mut out = CrystalCards::new();
    for site in crystal_sites(board) {
        let mut facts = CrystalFacts::default();
        if let Some(fitted) = AssemblyState::of(site.crystal).fitted() {
            let res = resolve(lib, fitted);
            if !matches!(res.confidence, Confidence::Guessed | Confidence::Unresolved) {
                if let Some(m) = res.model.as_ref() {
                    if let Some(cl) = m.params.get_f64("cl_f") {
                        let cite = res.references.first().map_or_else(
                            || format!("card '{}'", m.id),
                            |r| format!("card '{}': {}, {}", m.id, r.title, r.locator),
                        );
                        facts.cl_pf = Some((cl * 1e12, cite));
                    }
                    facts.c0_pf = m.params.get_f64("c0_f").map(|f| f * 1e12);
                    facts.c1_ff = m.params.get_f64("c1_f").map(|f| f * 1e15);
                }
            }
        }
        facts.driver_caps = driver_caps(board, lib, &site.nets);
        if facts != CrystalFacts::default() {
            out.insert(site.crystal.reference.clone(), facts);
        }
    }
    out
}

/// The on-chip load capacitance the card of an IC on `nets` states for the
/// oscillator pad it connects, if any: [`driver_caps_with`] over the binder's
/// resolution, which a guessed or unresolved part does not pass.
fn driver_caps(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    nets: &[i64; 2],
) -> Option<IntegratedCaps> {
    driver_caps_with(board, nets, |c| {
        let res = resolve(lib, AssemblyState::of(c).fitted()?);
        if matches!(res.confidence, Confidence::Guessed | Confidence::Unresolved) {
            return None;
        }
        res.model
    })
}

/// How far past the card's range a load cap must sit before it is a finding
/// (a factor on the maximum, its inverse on the minimum).
const FAR_OUTSIDE: f64 = 2.0;

/// The stray capacitance folded into the board CL the message reports (pF),
/// the same 4 pF the extract-layer crystal check assumes.
const CSTRAY_PF: f64 = 4.0;

/// Append a finding for every crystal whose load caps sit far outside the
/// C1/C2 range its driving IC's card states, when the crystal states no CL.
pub fn append_crystal_caps(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    cards: &CrystalCards,
    report: &mut SiReport,
) {
    for site in crystal_sites(board) {
        let card_cl = cards
            .get(&site.crystal.reference)
            .is_some_and(|f| f.cl_pf.is_some());
        if site.stated_cl_pf.is_some() || card_cl {
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

    /// An `ATmega328P` (DIP-28: XTAL1 pad 9, XTAL2 pad 10) on a 16 MHz crystal
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
        append_crystal_caps(b, &ModelLibrary::builtin(), &CrystalCards::new(), &mut r);
        r.findings
    }

    /// Four times the Uno's 22 pF (88 pF each) is four times the `ATmega328P`
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

    /// The crystal findings the engine path produces: cards built from the
    /// library, then the extract-layer check judged with them.
    fn engine_findings(b: &ExtractedBoard) -> Vec<SiFinding> {
        let lib = ModelLibrary::builtin();
        let cards = crystal_cards(b, &lib);
        let mut report = b.si_checks_with(None, &cards);
        append_crystal_caps(b, &lib, &cards, &mut report);
        report
            .findings
            .into_iter()
            .filter(|f| f.check == SiCheck::CrystalLoadCap && f.severity != SiSeverity::Info)
            .collect()
    }

    /// A crystal `Y1` with `props`, on XIN/XOUT with `cap` to ground on each.
    fn crystal_with(value: &str, props: &[(&str, &str)], cap: &str) -> ExtractedBoard {
        let mut y1 = part("Y1", value, &[("1", 1), ("2", 2)]);
        y1.footprint = "Crystal:Crystal_SMD_3215-2Pin".into();
        y1.properties = props
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        board(
            &[(1, "XIN"), (2, "XOUT"), (3, "GND")],
            vec![
                y1,
                part("C1", cap, &[("1", 1), ("2", 3)]),
                part("C2", cap, &[("1", 2), ("2", 3)]),
            ],
        )
    }

    /// The ABM8-272-T3 card (RP2040 reference crystal, CL 10 pF) is found by the
    /// ordering code in the MPN property, whatever the value says: the Pico's
    /// 15 pF pair is right for it and 34 pF is not.
    #[test]
    fn a_crystal_card_found_by_mpn_judges_the_caps() {
        let mpn = [("MPN", "ABM8-272-T3")];
        assert!(engine_findings(&crystal_with("12MHz", &mpn, "15p")).is_empty());
        let f = engine_findings(&crystal_with("12MHz", &mpn, "34p"));
        assert_eq!(f.len(), 1, "{f:?}");
        for says in [
            "CL = 10.0 pF",
            "card 'abracon_abm8_272_t3'",
            "C0 3.0 pF card",
        ] {
            assert!(f[0].message.contains(says), "{says:?} in {}", f[0].message);
        }
        // Another ordering code of the family is not this card: nothing to judge.
        let other = [("MPN", "ABM8-8.000MHZ-B2-T")];
        assert!(engine_findings(&crystal_with("8MHz", &other, "34p")).is_empty());
    }

    /// The extract layer's built-in table (for callers with no model library) and
    /// the crystal cards must agree on every code they both know.
    #[test]
    fn the_builtin_cl_table_agrees_with_the_crystal_cards() {
        let b = crystal_with("12MHz", &[("MPN", "ABM8-272-T3")], "15p");
        let from_table = crystal_sites(&b)[0]
            .stated_cl_pf
            .expect("table knows ABM8-272");
        let from_card = crystal_cards(&b, &ModelLibrary::builtin())["Y1"]
            .cl_pf
            .clone()
            .expect("the card states CL")
            .0;
        assert!(
            (from_table - from_card).abs() < 1e-9,
            "{from_table} vs {from_card}"
        );
    }

    /// The ABS07 32.768 kHz crystal is sold in several load capacitances and the
    /// ordering code carries it: the blank code is 12.5 pF, `-9` is 9 pF. The
    /// same 22 pF pair is right for the first and a mismatch for the second.
    #[test]
    fn the_abs07_ordering_code_picks_its_load_capacitance() {
        let blank = [("MPN", "ABS07-32.768KHZ-T")];
        let nine = [("MPN", "ABS07-32.768KHZ-9-T")];
        assert!(engine_findings(&crystal_with("32.768kHz", &blank, "22p")).is_empty());
        let f = engine_findings(&crystal_with("32.768kHz", &nine, "22p"));
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].message.contains("CL = 9.0 pF"), "{}", f[0].message);
        let heavy = engine_findings(&crystal_with("32.768kHz", &blank, "56p"));
        assert_eq!(heavy.len(), 1, "{heavy:?}");
        assert!(
            heavy[0].message.contains("CL = 12.5 pF"),
            "{}",
            heavy[0].message
        );
    }

    /// A Si5351 clock generator loads its crystal with programmable on-chip
    /// capacitors (12 to 20 pF on each of XA and XB: register 183 sets the CL
    /// across the pair to 6, 8 or 10 pF), so a 10 pF crystal on two 2 pF caps is
    /// reachable and silent, a 6 pF crystal on 22 pF caps is not, and a 12 pF
    /// crystal needs the 2 pF of external capacitance the datasheet allows.
    #[test]
    fn a_si5351s_programmable_crystal_load_is_read_from_its_card() {
        let on_xa_xb = |value: &str, part_no: &str, cap: &str| {
            let mut y1 = part("Y1", value, &[("1", 1), ("2", 2)]);
            y1.footprint = "Crystal:Crystal_SMD_3215-2Pin".into();
            board(
                &[(1, "XA"), (2, "XB"), (3, "GND")],
                vec![
                    part("U19", part_no, &[("1", 1), ("2", 2)]),
                    y1,
                    part("C1", cap, &[("1", 1), ("2", 3)]),
                    part("C2", cap, &[("1", 2), ("2", 3)]),
                ],
            )
        };
        let lib = ModelLibrary::builtin();
        let b = on_xa_xb("25MHz 10pF", "SI5351C", "2p");
        match crystal_cards(&b, &lib)["Y1"].driver_caps.clone() {
            Some(IntegratedCaps::Range {
                lo_pf,
                hi_pf,
                basis,
            }) => {
                assert!((lo_pf - 12.0).abs() < 1e-9 && (hi_pf - 20.0).abs() < 1e-9);
                assert!(basis.contains("AN619") && basis.contains("p.60"), "{basis}");
            }
            other => panic!("Si5351 range expected, got {other:?}"),
        }
        assert!(engine_findings(&b).is_empty(), "{:?}", engine_findings(&b));
        // Each version of the 20-pin QFN carries the same oscillator.
        for part_no in ["SI5351A-B-GM", "SI5351B-B-GM", "Si5351C-B-GM"] {
            let b = on_xa_xb("25MHz 10pF", part_no, "2p");
            assert!(engine_findings(&b).is_empty(), "{part_no}");
        }
        assert!(engine_findings(&on_xa_xb("25MHz 12pF", "SI5351C", "4p")).is_empty());
        let heavy = engine_findings(&on_xa_xb("25MHz 6pF", "SI5351C", "22p"));
        assert_eq!(heavy.len(), 1, "{heavy:?}");
        assert!(heavy[0].message.contains("on-chip per pin"));
    }

    /// An EFM32PG22 whose LFXO pads carry the 32.768 kHz crystal: the card says
    /// the on-chip bank is 4 to 24.5 pF per pin, so the bank alone reaches 12.25
    /// pF: a crystal of 12 pF or less with no external caps is right, a 12.5 pF
    /// one needs external caps. The HFXO pads are tunable with no printed range,
    /// so nothing is judged there, with or without external caps.
    #[test]
    fn a_driver_cards_on_chip_load_caps_are_read_from_the_card() {
        let on_pads = |value: &str, pads: [&str; 2], cap: Option<&str>| {
            let mut y1 = part("Y1", value, &[("1", 1), ("2", 2)]);
            y1.footprint = "Crystal:Crystal_SMD_3215-2Pin".into();
            let mut parts = vec![
                part("U1", "EFM32PG22C200F512IM40", &[(pads[0], 1), (pads[1], 2)]),
                y1,
            ];
            if let Some(cap) = cap {
                parts.push(part("C1", cap, &[("1", 1), ("2", 3)]));
                parts.push(part("C2", cap, &[("1", 2), ("2", 3)]));
            }
            board(&[(1, "XIN"), (2, "XOUT"), (3, "GND")], parts)
        };
        let cards = |b: &ExtractedBoard| crystal_cards(b, &ModelLibrary::builtin());
        let lfxo = on_pads("32.768kHz 12.5pF", ["39", "40"], None);
        match cards(&lfxo)["Y1"].driver_caps.clone() {
            Some(IntegratedCaps::Range { lo_pf, hi_pf, .. }) => {
                assert!((lo_pf - 4.0).abs() < 1e-9 && (hi_pf - 24.5).abs() < 1e-9);
            }
            other => panic!("LFXO range expected, got {other:?}"),
        }
        // Table 4.12 note 2: a crystal above 12 pF needs external load caps, so a
        // 12.5 pF crystal with none fires; one of 9 or 12 pF is silent.
        let f = engine_findings(&lfxo);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].message.contains("reaches at most 12.25 pF"));
        for ok in ["32.768kHz 9pF", "32.768kHz 12pF"] {
            assert!(
                engine_findings(&on_pads(ok, ["39", "40"], None)).is_empty(),
                "{ok}"
            );
        }
        let hfxo = on_pads("12MHz 10pF", ["9", "10"], None);
        assert!(matches!(
            cards(&hfxo)["Y1"].driver_caps,
            Some(IntegratedCaps::Unspecified { .. })
        ));
        assert!(engine_findings(&hfxo).is_empty());
        assert!(engine_findings(&on_pads("12MHz 10pF", ["9", "10"], Some("39p"))).is_empty());
        // Pads that are not an oscillator pair: no on-chip caps, judged as a
        // discrete crystal (39 pF pair -> CL 22.5 pF against 10 pF).
        let gpio = on_pads("12MHz 10pF", ["1", "2"], Some("39p"));
        assert!(cards(&gpio)
            .get("Y1")
            .is_none_or(|f| f.driver_caps.is_none()));
        assert_eq!(engine_findings(&gpio).len(), 1);
    }
}
