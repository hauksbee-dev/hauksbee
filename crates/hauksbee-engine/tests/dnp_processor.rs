//! What happens to Do-Not-Populate parts, and whether the run says so.
//!
//! Most DNP footprints get placed eventually (a socketed module bought
//! separately, a part stuffed by hand later), so `--fit-dnp` simulates them as
//! fitted (the default leaves them out, as the source marked them). The
//! exception is a near-zero-ohm link: a 0R bridge between two
//! ground planes is the electrical decision itself, and fitting it would merge
//! nets the designer split on purpose. Both halves are reported, never assumed.

use hauksbee_engine::binder::{bind_board, no_processor_message, FitRemedy};
use hauksbee_extract::dnp::{DnpPolicy, DnpReason};
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;
use std::fmt::Write as _;

/// A board with a DNP processor and a DNP 0R link between the two grounds.
const DNP_BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "D13")
  (net 4 "GNDA")

  (module Package_DIP:DIP-28_W7.62mm (layer F.Cu)
    (at 100 100)
    (attr dnp)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value ATmega328P (at 0 2) (layer F.Fab))
    (pad 7  smd rect (at -3 0) (net 2 "+5V"))
    (pad 8  smd rect (at -3 1) (net 1 "GND"))
    (pad 19 smd rect (at 3 0) (net 3 "D13"))
  )

  (module Resistor_SMD:R_0402_1005Metric (layer F.Cu)
    (at 120 100)
    (attr dnp)
    (fp_text reference R7 (at 0 0) (layer F.SilkS))
    (fp_text value 0R (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 1 "GND"))
    (pad 2 smd rect (at 1 0) (net 4 "GNDA"))
  )

  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 330 (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 3 "D13"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
)
"#;

fn board() -> ExtractedBoard {
    ExtractedBoard::from_auto(DNP_BOARD).expect("parse board")
}

#[test]
fn the_default_leaves_every_dnp_part_out_and_fit_except_links_fits_the_processor() {
    let mut b = board();
    let d = b.apply_dnp_policy(DnpPolicy::default(), &[], &[]).unwrap();
    assert!(d.fitted.is_empty(), "the default fits nothing: {d:?}");
    assert_eq!(d.left_open.len(), 2, "processor and link both stay out");
    let mut b = board();
    let d = b
        .apply_dnp_policy(DnpPolicy::FitExceptLinks, &[], &[])
        .expect("policy applies");

    assert_eq!(d.fitted.len(), 1, "the processor is fitted");
    assert_eq!(d.fitted[0].reference, "U1");
    assert_eq!(d.fitted[0].reason, DnpReason::Policy);

    assert_eq!(d.left_open.len(), 1, "the 0R link stays open");
    assert_eq!(d.left_open[0].reference, "R7");
    assert_eq!(d.left_open[0].reason, DnpReason::ZeroOhmLink);

    let bound = bind_board(&b, &ModelLibrary::builtin());
    assert_eq!(bound.mcus.len(), 1, "the fitted processor binds");
}

#[test]
fn the_decision_is_always_reported() {
    let mut b = board();
    let d = b
        .apply_dnp_policy(DnpPolicy::FitExceptLinks, &[], &[])
        .unwrap();
    let text = d.lines().join("\n");
    assert!(text.contains("do-not-populate:"), "{text}");
    assert!(text.contains("fitted:    U1"), "{text}");
    assert!(text.contains("left open: R7"), "{text}");
    assert!(
        text.contains("merges the nets it bridges"),
        "the link exception must explain itself: {text}"
    );
}

#[test]
fn a_board_with_no_dnp_parts_says_nothing_about_dnp() {
    let plain = DNP_BOARD.replace("    (attr dnp)\n", "");
    let mut b = ExtractedBoard::from_auto(&plain).expect("parse");
    let d = b
        .apply_dnp_policy(DnpPolicy::FitExceptLinks, &[], &[])
        .unwrap();
    assert!(d.is_empty());
    assert!(d.lines().is_empty(), "no DNP parts, no DNP chatter");
}

#[test]
fn naming_a_link_fits_it_anyway() {
    let mut b = board();
    let d = b
        .apply_dnp_policy(DnpPolicy::FitExceptLinks, &["R7".to_string()], &[])
        .unwrap();
    let r7 = d
        .fitted
        .iter()
        .find(|p| p.reference == "R7")
        .expect("fitted");
    assert_eq!(r7.reason, DnpReason::NamedFit);
    assert!(d.left_open.is_empty());
}

#[test]
fn no_fit_overrides_the_policy() {
    let mut b = board();
    let d = b
        .apply_dnp_policy(DnpPolicy::FitAll, &[], &["U1".to_string()])
        .unwrap();
    let u1 = d
        .left_open
        .iter()
        .find(|p| p.reference == "U1")
        .expect("left open on request");
    assert_eq!(u1.reason, DnpReason::NamedOpen);
    // fit-all still fits the link, since that is what fit-all means.
    assert!(d.fitted.iter().any(|p| p.reference == "R7"));
}

#[test]
fn no_fit_can_make_a_normally_populated_part_absent_for_an_assembly_variant() {
    let mut b = board();
    let r1 = b.component("R1").expect("fixture resistor");
    assert!(!r1.dnp, "R1 starts as an ordinary fitted layout part");

    let d = b
        .apply_dnp_policy(DnpPolicy::FitExceptLinks, &[], &["R1".to_string()])
        .expect("explicit variant decision applies");
    let r1 = d
        .left_open
        .iter()
        .find(|part| part.reference == "R1")
        .expect("ordinary layout part is left open by name");
    assert_eq!(r1.reason, DnpReason::NamedOpen);
    assert!(b.component("R1").expect("R1 remains in inventory").dnp);
}

#[test]
fn honour_leaves_everything_open() {
    let mut b = board();
    let d = b.apply_dnp_policy(DnpPolicy::Honour, &[], &[]).unwrap();
    assert!(d.fitted.is_empty());
    assert_eq!(d.left_open.len(), 2);
    let bound = bind_board(&b, &ModelLibrary::builtin());
    assert!(bound.mcus.is_empty());
    assert_eq!(
        bound.dnp_mcus,
        vec![("U1".to_string(), "ATmega328P".to_string())],
        "the skipped processor is recorded so the firmware gate can name it"
    );
}

#[test]
fn a_skipped_sole_processor_warns_on_its_own_row() {
    let mut b = board();
    b.apply_dnp_policy(DnpPolicy::Honour, &[], &[]).unwrap();
    let bound = bind_board(&b, &ModelLibrary::builtin());
    let row = bound.report.row("U1").expect("U1 has a bind row");
    let w = row.warning.as_deref().expect("must not vanish quietly");
    assert!(w.contains("only processor"), "{w}");
    assert!(w.contains("--fit U1"), "{w}");
}

/// Two-sided `DnpAbsent` contract: the left-open 0R link contributes NOTHING to
/// the bound circuit (the two ground planes it would bridge stay separate
/// nodes), and its absence is on the bind report with the reason the policy
/// recorded, not as a silent hole.
#[test]
fn a_left_open_link_contributes_nothing_and_its_absence_is_reported() {
    let mut b = board();
    let d = b
        .apply_dnp_policy(DnpPolicy::FitExceptLinks, &[], &[])
        .unwrap();
    assert_eq!(d.left_open[0].reference, "R7", "the link stays open");

    let bound = bind_board(&b, &ModelLibrary::builtin());
    // Side 1: nothing anywhere. GND and GNDA are exactly the nets R7 would
    // merge; distinct circuit nodes prove no device, wiring, or short was
    // stamped for it.
    let gnd = bound.net_nodes.get("GND").expect("GND is a net");
    let gnda = bound.net_nodes.get("GNDA").expect("GNDA is a net");
    assert_ne!(
        gnd, gnda,
        "a left-open DNP link must not merge the planes it would bridge"
    );
    // Side 2: visible. The row says why the part is absent, carrying the
    // policy's recorded reason through the assembly contract.
    let row = bound.report.row("R7").expect("R7 has a bind row");
    match &row.outcome {
        hauksbee_engine::reports::bind::BindOutcome::Skipped { reason } => {
            assert_eq!(
                reason,
                &DnpReason::ZeroOhmLink.describe().to_string(),
                "the skip must carry the policy's reason, not a generic DNP line"
            );
        }
        other => panic!("a DNP part must row as Skipped, got {other:?}"),
    }
}

#[test]
fn an_unknown_or_contradictory_reference_is_a_loud_error() {
    let mut b = board();
    let msg = b
        .apply_dnp_policy(DnpPolicy::FitExceptLinks, &["U99".to_string()], &[])
        .expect_err("a typo must not silently do nothing")
        .to_string();
    assert!(msg.contains("U99"), "{msg}");

    let mut b = board();
    let msg = b
        .apply_dnp_policy(
            DnpPolicy::FitExceptLinks,
            &["U1".to_string()],
            &["U1".to_string()],
        )
        .expect_err("naming a part both ways is a contradiction")
        .to_string();
    assert!(msg.contains("both"), "{msg}");
}

#[test]
fn link_detection_covers_the_shapes_that_merge_nets() {
    for value in ["0", "0R", "0.0", "0R0", "R000"] {
        let text = DNP_BOARD.replace("(fp_text value 0R ", &format!("(fp_text value {value} "));
        let mut b = ExtractedBoard::from_auto(&text).expect("parse");
        let d = b
            .apply_dnp_policy(DnpPolicy::FitExceptLinks, &[], &[])
            .unwrap();
        assert!(
            d.left_open.iter().any(|p| p.reference == "R7"),
            "{value} is a link and must stay open"
        );
    }
    // A real resistance is a component, not a link: fitting it is fine.
    let text = DNP_BOARD.replace("(fp_text value 0R ", "(fp_text value 10k ");
    let mut b = ExtractedBoard::from_auto(&text).expect("parse");
    let d = b
        .apply_dnp_policy(DnpPolicy::FitExceptLinks, &[], &[])
        .unwrap();
    assert!(
        d.fitted.iter().any(|p| p.reference == "R7"),
        "a 10k is a component and gets fitted when the policy fits"
    );
}

#[test]
fn the_no_processor_message_tells_each_surface_its_own_fix() {
    let dnp = vec![("A101".to_string(), "Arduino_Nano_v3.x".to_string())];

    let cli = no_processor_message(&dnp, &[], FitRemedy::Cli);
    assert!(cli.contains("A101 (Arduino_Nano_v3.x)"), "{cli}");
    assert!(cli.contains("--fit A101"), "{cli}");

    let spec = no_processor_message(&dnp, &[], FitRemedy::Spec);
    assert!(spec.contains("fit = [\"A101\"]"), "{spec}");

    // No processor at all is a different problem, and must not advise a fit
    // that would fix nothing.
    let none = no_processor_message(&[], &[], FitRemedy::Cli);
    assert!(!none.contains("--fit"), "{none}");
    assert!(none.contains("models resolve"), "{none}");
}

/// A compute-module board: the module `A1` is bought separately and marked
/// DNP, and plugs into the header `J1` that the board does fit. The pad lists
/// are built from `nets` so a case can change which side is fitted.
fn module_board(module_dnp: bool, socket_dnp: bool, socket: (&str, &str, &str)) -> String {
    let (socket_ref, socket_fp, socket_value) = socket;
    let nets = ["GND", "+3V3", "SDA", "SCL", "TX", "RX", "EN"];
    let mut net_defs = String::new();
    for (i, n) in nets.iter().enumerate() {
        writeln!(net_defs, "  (net {} \"{n}\")", i + 1).unwrap();
    }
    let pads = |dx: f64| -> String {
        let mut out = String::new();
        for (i, n) in nets.iter().enumerate() {
            writeln!(
                out,
                "    (pad {} thru_hole circle (at {dx} {i}) (net {} \"{n}\"))",
                i + 1,
                i + 1
            )
            .unwrap();
        }
        out
    };
    let dnp = |on: bool| if on { "    (attr dnp)\n" } else { "" };
    format!(
        "(kicad_pcb (version 20171130) (host pcbnew 5.1.0)\n  (net 0 \"\")\n{net_defs}\
         \n  (module Module:Compute_Module (layer F.Cu)\n    (at 100 100)\n{}\
         \x20   (fp_text reference A1 (at 0 0) (layer F.SilkS))\n\
         \x20   (fp_text value ComputeModule (at 0 2) (layer F.Fab))\n{}  )\n\
         \n  (module {socket_fp} (layer F.Cu)\n    (at 100 100)\n{}\
         \x20   (fp_text reference {socket_ref} (at 0 0) (layer F.SilkS))\n\
         \x20   (fp_text value {socket_value} (at 0 2) (layer F.Fab))\n{}  )\n\
         \n  (module Resistor_SMD:R_0402_1005Metric (layer F.Cu)\n    (at 120 100)\n\
         \x20   (fp_text reference R1 (at 0 0) (layer F.SilkS))\n\
         \x20   (fp_text value 10k (at 0 2) (layer F.Fab))\n\
         \x20   (pad 1 smd rect (at 0 0) (net 2 \"+3V3\"))\n\
         \x20   (pad 2 smd rect (at 1 0) (net 3 \"SDA\"))\n  )\n)\n",
        dnp(module_dnp),
        pads(0.0),
        dnp(socket_dnp),
        pads(1.0),
    )
}

const HEADER: (&str, &str, &str) = (
    "J1",
    "Connector_PinSocket_2.54mm:PinSocket_1x07_P2.54mm_Vertical",
    "Conn_01x07",
);

fn module_decision(
    module_dnp: bool,
    socket_dnp: bool,
    socket: (&str, &str, &str),
) -> (ExtractedBoard, hauksbee_extract::dnp::DnpDecision) {
    let mut b = ExtractedBoard::from_auto(&module_board(module_dnp, socket_dnp, socket))
        .expect("parse board");
    let d = b.apply_dnp_policy(DnpPolicy::Honour, &[], &[]).unwrap();
    (b, d)
}

#[test]
fn a_dnp_plug_in_module_on_a_fitted_socket_counts_as_fitted() {
    let (b, d) = module_decision(true, false, HEADER);
    assert_eq!(d.fitted.len(), 1, "{d:?}");
    assert_eq!(d.fitted[0].reference, "A1");
    assert_eq!(d.fitted[0].reason, DnpReason::SocketedModule);
    assert!(
        !b.component("A1").unwrap().dnp,
        "the module is in the circuit"
    );
    assert!(d.left_open.is_empty(), "{d:?}");
}

#[test]
fn a_dnp_module_whose_socket_is_also_dnp_stays_out() {
    let (b, d) = module_decision(true, true, HEADER);
    assert!(d.fitted.is_empty(), "{d:?}");
    assert!(b.component("A1").unwrap().dnp);
}

#[test]
fn a_dnp_module_with_no_fitted_socket_stays_out() {
    // The same nets, but the part sharing them is a logic chip, not a socket.
    let chip = ("U9", "Package_SO:SOIC-16_3.9x9.9mm_P1.27mm", "74HC595");
    let (b, d) = module_decision(true, false, chip);
    assert!(d.fitted.is_empty(), "{d:?}");
    assert!(b.component("A1").unwrap().dnp);
}

#[test]
fn naming_the_module_open_overrides_the_socket_rule() {
    let mut b = ExtractedBoard::from_auto(&module_board(true, false, HEADER)).unwrap();
    let d = b
        .apply_dnp_policy(DnpPolicy::Honour, &[], &["A1".to_string()])
        .unwrap();
    assert!(d.fitted.is_empty(), "{d:?}");
    assert_eq!(d.left_open[0].reason, DnpReason::NamedOpen);
}
