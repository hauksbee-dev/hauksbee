//! What a BOM changes about binding, and what it must never change.
//!
//! Every behaviour here is tested from both sides, because one side alone proves
//! nothing. A BOM that identifies a part the layout could not is only worth
//! having if a BOM that CONTRADICTS the layout is refused rather than believed,
//! and a precedence rule is only real if the case it decides against is tested
//! too.

use hauksbee_engine::binder::{
    apply_bom_identity, apply_placement_identity, bind_board, FitAdvice, IdentityFinding,
    IdentityRefusal,
};
use hauksbee_engine::report::BindOutcome;
use hauksbee_extract::bom::{Bom, ColumnOverrides};
use hauksbee_extract::dnp::DnpPolicy;
use hauksbee_extract::placement::PlacementFile;
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::{Confidence, ModelLibrary};

/// A minimal layout carrying the given `(reference, value)` parts.
///
/// Hand-built rather than taken from a fixture, because these tests are about the
/// JOIN between two files and the layout side has to be exactly one controlled
/// thing at a time. The pad count comes from the designator prefix, since a
/// three-terminal part given two pads fails to bind for a reason that has nothing
/// to do with the BOM and would make every assertion here meaningless.
fn board(parts: &[(&str, &str)]) -> ExtractedBoard {
    let mut text = String::from("(kicad_pcb (version 20171130) (host pcbnew 5.1.0)\n");
    text.push_str("  (net 0 \"\")\n  (net 1 \"GND\")\n  (net 2 \"+5V\")\n");
    for i in 3..40 {
        text.push_str(&format!("  (net {i} \"N{i}\")\n"));
    }
    for (i, (reference, value)) in parts.iter().enumerate() {
        let prefix: String = reference
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
        let pads = match prefix.as_str() {
            "TP" => 1,
            "Q" => 3,
            "U" => 10,
            _ => 2,
        };
        text.push_str(&format!(
            "  (module Package_TO_SOT_SMD:SOT-23 (layer F.Cu)\n    (at {} 100)\n\
             \x20   (fp_text reference {reference} (at 0 0) (layer F.SilkS))\n\
             \x20   (fp_text value {} (at 0 2) (layer F.Fab))\n",
            100 + i * 10,
            if value.is_empty() { "\"\"" } else { value }
        ));
        for pad in 1..=pads {
            let net = match pad {
                1 => 2,
                2 => 1,
                p => 3 + (i * 10 + p) % 30,
            };
            text.push_str(&format!(
                "    (pad {pad} smd rect (at {pad} 0) (net {net} \"n\"))\n"
            ));
        }
        text.push_str("  )\n");
    }
    text.push(')');
    ExtractedBoard::from_kicad_pcb(&text).expect("the synthesized board reads")
}

fn bom(text: &str) -> Bom {
    Bom::from_text(text, "bom.csv", &ColumnOverrides::new()).expect("the BOM reads")
}

fn outcome_of(board: &ExtractedBoard, reference: &str) -> BindOutcome {
    bind_board(board, &ModelLibrary::builtin())
        .report
        .rows
        .iter()
        .find(|r| r.reference == reference)
        .map(|r| r.outcome.clone())
        .unwrap_or_else(|| panic!("{reference} has no bind row"))
}

// ── The gain: a BOM identifies what the layout could not ────────────────────

#[test]
fn a_bom_mpn_binds_a_part_whose_layout_value_says_nothing() {
    // The layout carries a footprint and no value, which is every part on an
    // Altium `.PcbDoc` and any board whose exporter dropped the field.
    let mut b = board(&[("U9", "")]);
    assert!(
        matches!(outcome_of(&b, "U9"), BindOutcome::Unresolved { .. }),
        "without the BOM this part cannot bind"
    );

    let report = apply_bom_identity(
        &mut b,
        &bom("Designator,Value,MPN\nU9,,MCP4728\n"),
        &ModelLibrary::builtin(),
    )
    .expect("the BOM agrees with the layout");

    // It binds, and the bind is attributable to the file that made it possible.
    assert!(!matches!(
        outcome_of(&b, "U9"),
        BindOutcome::Unresolved { .. }
    ));
    assert_eq!(report.identified.len(), 1);
    let a = &report.identified[0];
    assert_eq!(a.reference, "U9");
    assert_eq!(a.mpn, "MCP4728");
    assert_eq!(a.source, "bom.csv");
    assert_eq!(a.before, Confidence::Unresolved);
    assert_ne!(a.after, Confidence::Unresolved);
    assert!(a.model_id.is_some());
    assert!(
        report
            .lines()
            .iter()
            .any(|l| l.contains("U9 identified from bom.csv")),
        "{:?}",
        report.lines()
    );
}

#[test]
fn a_placement_file_fills_an_empty_layout_value_but_never_claims_a_part_number() {
    let mut b = board(&[("C1", ""), ("R1", "")]);
    let text = "Designator,Val,Package,Mid X,Mid Y,Rotation,Layer\n\
                C1,100nF,C_0402,100.0,100.0,0,top\n\
                R1,10k,R_0402,110.0,100.0,0,top\n";
    let file = PlacementFile::from_text(text, "cpl.csv").expect("the CPL reads");
    let report = apply_placement_identity(&mut b, &file, &ModelLibrary::builtin())
        .expect("the CPL agrees with the layout");

    assert_eq!(report.values_filled.len(), 2);
    assert_eq!(b.component("C1").unwrap().value, "100nF");
    // A placement file's Val column is a value, so it can fill a hole and can
    // never be an identity claim.
    assert!(report.identified.is_empty());
}

// ── The other side: a contradicting BOM is refused, not used ─────────────────

#[test]
fn a_bom_that_calls_a_mosfet_a_ten_k_resistor_is_refused_and_says_what_to_do() {
    // The layout resolves Q5 as a MOSFET. The BOM calls it 10k. That is not a
    // disagreement about a number, it is two different boards.
    let mut b = board(&[("Q5", "BSS138")]);
    assert!(
        !matches!(outcome_of(&b, "Q5"), BindOutcome::Unresolved { .. }),
        "the layout alone must resolve Q5 for this test to mean anything"
    );

    let err = apply_bom_identity(
        &mut b,
        &bom("Designator,Value\nQ5,10k\n"),
        &ModelLibrary::builtin(),
    )
    .expect_err("must refuse");

    assert_eq!(err.exit_code(), 3);
    let msg = err.to_string();
    assert!(msg.contains("Q5"), "{msg}");
    assert!(msg.contains("different revisions of the board"), "{msg}");
    assert!(
        msg.contains("Use the BOM that was exported from this layout"),
        "{msg}"
    );
    assert!(matches!(err, IdentityRefusal::Contradiction { .. }));
    // And nothing was applied: a refused BOM leaves the board as it was.
    assert_eq!(b.component("Q5").unwrap().value, "BSS138");
    assert!(b
        .component("Q5")
        .unwrap()
        .properties
        .iter()
        .all(|(k, _)| k != hauksbee_extract::bom::MPN_PROPERTY));
}

#[test]
fn a_bom_whose_dimension_disagrees_with_the_layout_is_refused() {
    // Ohms against farads on the same designator. Neither side needs a model for
    // this to be decisive.
    let mut b = board(&[("R1", "10k")]);
    let err = apply_bom_identity(
        &mut b,
        &bom("Designator,Value\nR1,100nF\n"),
        &ModelLibrary::builtin(),
    )
    .expect_err("must refuse");
    assert_eq!(err.exit_code(), 3);
    let msg = err.to_string();
    assert!(
        msg.contains("\"10k\" (a resistance) on the layout"),
        "{msg}"
    );
    assert!(
        msg.contains("\"100nF\" (a capacitance) in the BOM"),
        "{msg}"
    );
}

#[test]
fn a_bom_for_a_different_board_entirely_is_refused_by_name() {
    let mut b = board(&[("R1", "10k"), ("R2", "10k")]);
    let err = apply_bom_identity(
        &mut b,
        &bom("Designator,Value\nX1,10k\nX2,10k\nX3,10k\nX4,10k\nX5,10k\n\
             X6,10k\nX7,10k\nX8,10k\nX9,10k\nX10,10k\n"),
        &ModelLibrary::builtin(),
    )
    .expect_err("must refuse");
    assert_eq!(err.exit_code(), 3);
    let msg = err.to_string();
    assert!(
        msg.contains("names 10 reference designators and only 0"),
        "{msg}"
    );
    assert!(
        msg.contains("Check which file goes with which layout"),
        "{msg}"
    );
}

#[test]
fn a_placement_file_from_another_revision_is_refused_on_its_positions() {
    let mut b = board(&[("R1", "10k"), ("R2", "10k")]);
    // Same designators, both in the wrong place.
    let text = "Designator,Mid X,Mid Y,Rotation,Layer\n\
                R1,5.0,5.0,0,top\nR2,6.0,6.0,0,top\n";
    let file = PlacementFile::from_text(text, "old-rev-cpl.csv").expect("the CPL reads");
    let err =
        apply_placement_identity(&mut b, &file, &ModelLibrary::builtin()).expect_err("must refuse");
    assert_eq!(err.exit_code(), 3);
    let msg = err.to_string();
    assert!(msg.contains("R1 sits at"), "{msg}");
    assert!(msg.contains("different revisions of the board"), "{msg}");
}

// ── Precedence, tested in both directions ───────────────────────────────────

#[test]
fn two_files_naming_different_chips_for_one_designator_is_refused() {
    // Both files resolve U1, and they resolve it to different processors. The
    // ComponentKind is the same, so a kind comparison alone would let this
    // through, and a run that silently swaps the simulated chip is the worst
    // outcome available: every firmware assertion would then be evaluated against
    // a core the board does not have.
    let mut b = board(&[("U1", "ATmega328P-AU")]);
    let err = apply_bom_identity(
        &mut b,
        &bom("Designator,Value,MPN\nU1,ATmega328P-AU,STM32F103C8\n"),
        &ModelLibrary::builtin(),
    )
    .expect_err("must refuse");
    assert_eq!(err.exit_code(), 3);
    let msg = err.to_string();
    assert!(msg.contains("atmega328p"), "{msg}");
    assert!(msg.contains("stm32f103c8"), "{msg}");
    assert_eq!(b.component("U1").unwrap().value, "ATmega328P-AU");
}

#[test]
fn the_layout_keeps_deciding_when_it_already_resolved_the_part() {
    // The other side of the same rule: an agreeing part number adds nothing, so
    // the layout's reading stands and no attribution is claimed for a bind the
    // layout made on its own.
    let mut b = board(&[("U1", "ATmega328P-AU")]);
    let report = apply_bom_identity(
        &mut b,
        &bom("Designator,Value,MPN\nU1,ATmega328P-AU,ATmega328P-AU\n"),
        &ModelLibrary::builtin(),
    )
    .expect("agreement");
    assert!(
        report.identified.is_empty(),
        "the layout resolved this alone: {:?}",
        report.identified
    );
}

#[test]
fn a_bom_value_never_overrides_a_layout_value_that_is_already_there() {
    // The other direction of the same rule: the BOM's VALUE column is the same
    // kind of claim the layout makes, so it must not replace it.
    let mut b = board(&[("R1", "10k")]);
    let report = apply_bom_identity(
        &mut b,
        &bom("Designator,Value\nR1,4k7\n"),
        &ModelLibrary::builtin(),
    )
    .expect("a magnitude disagreement is not a class contradiction");
    assert_eq!(
        b.component("R1").unwrap().value,
        "10k",
        "the layout still decides"
    );
    assert!(report.values_filled.is_empty());
    assert!(
        report
            .findings
            .iter()
            .any(|f| matches!(f, IdentityFinding::ValueDisagrees { .. })),
        "but it is reported: {:?}",
        report.findings
    );
}

#[test]
fn a_distributor_order_code_never_reaches_the_binder_as_a_part_number() {
    // An LCSC code matches nothing and would cost the bind if it were treated as
    // identity. The BOM reader drops it, so the hint list carries no part number
    // at all, and the layout keeps deciding.
    let parsed = bom("Comment,Designator,Footprint,LCSC\nMCP4728,U9,QFN-10,C123456\n");
    assert!(parsed.identity_hints().iter().all(|h| h.mpn.is_none()));
    let mut b = board(&[("U9", "")]);
    let report =
        apply_bom_identity(&mut b, &parsed, &ModelLibrary::builtin()).expect("no contradiction");
    assert!(report.identified.is_empty());
    // The `Comment` column still fills the empty value, which is the honest
    // contribution such a BOM makes.
    assert_eq!(b.component("U9").unwrap().value, "MCP4728");
    assert_eq!(report.values_filled.len(), 1);
}

// ── The mismatch cases ──────────────────────────────────────────────────────

#[test]
fn a_bom_refdes_that_is_not_on_the_board_is_reported_and_not_fatal() {
    let mut b = board(&[("R1", "10k"), ("R2", "10k")]);
    let report = apply_bom_identity(
        &mut b,
        &bom("Designator,Value\nR1,10k\nR2,10k\nMH1,M3 screw\n"),
        &ModelLibrary::builtin(),
    )
    .expect("a mechanical part in the BOM is ordinary");
    let names: Vec<&Vec<String>> = report
        .findings
        .iter()
        .filter_map(|f| match f {
            IdentityFinding::NotOnBoard { references, .. } => Some(references),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec![&vec!["MH1".to_string()]]);
}

#[test]
fn a_board_part_absent_from_the_bom_is_reported_and_not_fatal() {
    let mut b = board(&[("R1", "10k"), ("TP1", "")]);
    let report = apply_bom_identity(
        &mut b,
        &bom("Designator,Value\nR1,10k\n"),
        &ModelLibrary::builtin(),
    )
    .expect("a BOM omitting a test point is ordinary");
    assert!(
        report.findings.iter().any(|f| matches!(
            f,
            IdentityFinding::NotInArtifact { references, .. } if references == &vec!["TP1".to_string()]
        )),
        "{:?}",
        report.findings
    );
}

#[test]
fn a_quantity_that_disagrees_with_its_own_reference_list_is_reported() {
    let mut b = board(&[("R1", "10k"), ("R2", "10k")]);
    let report = apply_bom_identity(
        &mut b,
        &bom("Designator,Value,Quantity\n\"R1, R2\",10k,3\n"),
        &ModelLibrary::builtin(),
    )
    .expect("a stale quantity is not a different board");
    let line = report
        .findings
        .iter()
        .find_map(|f| match f {
            IdentityFinding::QuantityDisagrees { .. } => Some(f.line()),
            _ => None,
        })
        .expect("the quantity disagreement is reported");
    assert!(line.contains("quantity of 3"), "{line}");
    assert!(line.contains("lists 2 designators"), "{line}");
    assert!(line.contains("the list is what bound"), "{line}");
}

// ── DNP: the BOM advises, the policy decides ────────────────────────────────

#[test]
fn a_bom_that_says_a_dnp_part_is_populated_advises_and_does_not_act() {
    // The layout marks R1 do-not-populate. The BOM says it is fitted. hauksbee
    // already has one place that decides this question, so the BOM's opinion
    // arrives as advice with the flag that honours it.
    let mut b = board(&[("R1", "10k")]);
    b.components[0].dnp = true;
    let report = apply_bom_identity(
        &mut b,
        &bom("Designator,Value,Populate\nR1,10k,yes\n"),
        &ModelLibrary::builtin(),
    )
    .expect("a populate disagreement is not a different board");

    assert_eq!(
        report.advice,
        FitAdvice {
            fit: vec!["R1".to_string()],
            no_fit: Vec::new()
        }
    );
    assert!(
        b.component("R1").unwrap().dnp,
        "the BOM must not have acted"
    );
    assert!(
        report.lines().iter().any(|l| l.contains("--fit R1")),
        "{:?}",
        report.lines()
    );
    // And the advice is what `apply_dnp_policy` takes, rather than a second
    // mechanism beside it.
    let decision = b
        .apply_dnp_policy(DnpPolicy::Honour, &report.advice.fit, &report.advice.no_fit)
        .expect("the advice names parts that exist");
    assert_eq!(decision.fitted.len(), 1);
    assert!(!b.component("R1").unwrap().dnp);
}

#[test]
fn a_bom_that_says_a_populated_part_is_not_fitted_advises_the_other_way() {
    // The half the layout does not carry at all: nothing in the board file says
    // R1 is unfitted, and the BOM does.
    let mut b = board(&[("R1", "10k")]);
    let report = apply_bom_identity(
        &mut b,
        &bom("Designator,Value,DNP\nR1,10k,yes\n"),
        &ModelLibrary::builtin(),
    )
    .expect("a populate disagreement is not a different board");
    assert_eq!(report.advice.no_fit, vec!["R1".to_string()]);
    assert!(report.advice.fit.is_empty());
    assert!(!b.component("R1").unwrap().dnp, "still not acted on");
    assert!(
        report.findings.iter().any(|f| matches!(
            f,
            IdentityFinding::PopulateDisagrees { reference, .. } if reference == "R1"
        )),
        "{:?}",
        report.findings
    );
}

#[test]
fn a_bom_and_a_layout_that_agree_about_dnp_say_nothing() {
    let mut b = board(&[("R1", "10k")]);
    b.components[0].dnp = true;
    let report = apply_bom_identity(
        &mut b,
        &bom("Designator,Value,DNP\nR1,10k,yes\n"),
        &ModelLibrary::builtin(),
    )
    .expect("agreement");
    assert!(report.advice.is_empty());
    assert!(!report
        .findings
        .iter()
        .any(|f| matches!(f, IdentityFinding::PopulateDisagrees { .. })));
}

// ── The refusal contract itself ─────────────────────────────────────────────

#[test]
fn the_bom_readers_refuse_with_the_same_exit_code_the_engine_defines() {
    // `hauksbee-extract` sits below the engine and cannot import this constant,
    // so it names its own. This test is the thing that stops the two drifting.
    assert_eq!(
        hauksbee_extract::bom::EXIT_INVALID_FOR_ANALYSIS,
        hauksbee_engine::result::EXIT_INVALID_FOR_ANALYSIS
    );
    let bom_err = Bom::from_text("a,b\n1,2\n", "x.csv", &ColumnOverrides::new()).unwrap_err();
    let placement_err = PlacementFile::from_text("a,b\n1,2\n", "x.csv").unwrap_err();
    assert_eq!(
        bom_err.exit_code(),
        hauksbee_engine::result::EXIT_INVALID_FOR_ANALYSIS
    );
    assert_eq!(
        placement_err.exit_code(),
        hauksbee_engine::result::EXIT_INVALID_FOR_ANALYSIS
    );
}

#[test]
fn a_bom_that_agrees_with_the_layout_and_adds_nothing_prints_nothing() {
    let mut b = board(&[("R1", "10k")]);
    let report = apply_bom_identity(
        &mut b,
        &bom("Designator,Value\nR1,10k\n"),
        &ModelLibrary::builtin(),
    )
    .expect("agreement");
    assert!(report.lines().is_empty(), "{:?}", report.lines());
}
