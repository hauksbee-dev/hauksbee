//! A panel repeats one routed circuit with the same designators and net names.
//! `KiCad` joins the same-named nets across the copies and reports the ratsnest
//! between them; the tool must judge each copy on its own: silent when every
//! copy is routed (one note), and loud when a copy is missing a connection
//! inside itself.

use std::fmt::Write as _;

use hauksbee_extract::{
    ExtractedBoard, LintCheck, LintFinding, Severity, PANEL_COPIES_MESSAGE_PREFIX,
};

const PITCH: f64 = 30.0;

/// Which of a copy's three tracks are drawn.
#[derive(Clone, Copy)]
struct Routing {
    vcc: bool,
    sig: bool,
    gnd: bool,
}

const ALL: Routing = Routing {
    vcc: true,
    sig: true,
    gnd: true,
};

/// One copy of a three-net circuit at x offset `ox`: R1 (VCC, SIG), a LED D1
/// (SIG, GND) and a header J1 (VCC, GND).
fn copy(ox: f64, r1_value: &str, routing: Routing) -> String {
    let (x4, x6) = (ox + 4.0, ox + 6.0);
    let mut out = format!(
        r#"  (footprint "lib:R" (layer "F.Cu") (at {ox} 0)
    (property "Reference" "R1") (property "Value" "{r1_value}")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "VCC"))
    (pad "2" smd rect (at 4 0) (size 1 1) (layers "F.Cu") (net 2 "SIG")))
  (footprint "lib:LED" (layer "F.Cu") (at {x4} 6)
    (property "Reference" "D1") (property "Value" "LED")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 2 "SIG"))
    (pad "2" smd rect (at 0 3) (size 1 1) (layers "F.Cu") (net 3 "GND")))
  (footprint "lib:J" (layer "F.Cu") (at {ox} 9)
    (property "Reference" "J1") (property "Value" "HDR")
    (pad "1" smd rect (at 0 0) (size 1.6 1.6) (layers "F.Cu") (net 1 "VCC"))
    (pad "2" smd rect (at 6 0) (size 1.6 1.6) (layers "F.Cu") (net 3 "GND")))
"#
    );
    let mut track = |on: bool, (x1, y1, x2, y2): (f64, f64, f64, f64), net: u8| {
        if on {
            let _ = writeln!(
                out,
                "  (segment (start {x1} {y1}) (end {x2} {y2}) (width 0.25) (layer \"F.Cu\") (net {net}))"
            );
        }
    };
    track(routing.vcc, (ox, 0.0, ox, 9.0), 1);
    track(routing.sig, (x4, 0.0, x4, 6.0), 2);
    track(routing.gnd, (x4, 9.0, x6, 9.0), 3);
    out
}

/// `copies` panel copies side by side; `with(k)` gives copy `k`'s value and routing.
fn panel(copies: usize, with: impl Fn(usize) -> (&'static str, Routing)) -> String {
    let mut text = String::from(
        r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
  (net 0 "")
  (net 1 "VCC")
  (net 2 "SIG")
  (net 3 "GND")
"#,
    );
    for k in 0..copies {
        let (value, routing) = with(k);
        #[allow(clippy::cast_precision_loss)] // a handful of copies
        text.push_str(&copy(k as f64 * PITCH, value, routing));
    }
    text.push_str(")\n");
    text
}

fn lint(text: &str) -> Vec<LintFinding> {
    let pcb = ExtractedBoard::from_kicad_pcb(text).expect("board parses");
    pcb.layout_lint(text, None).findings
}

fn open_pads(findings: &[LintFinding]) -> Vec<&LintFinding> {
    findings
        .iter()
        .filter(|f| f.check == LintCheck::UnroutedPad && f.severity == Severity::High)
        .collect()
}

fn panel_notes(findings: &[LintFinding]) -> Vec<&LintFinding> {
    findings
        .iter()
        .filter(|f| f.message.starts_with(PANEL_COPIES_MESSAGE_PREFIX))
        .collect()
}

/// Case A: every copy fully routed. No open connection, and one note that the
/// nets are split across the copies (what `KiCad` draws as ratsnest between
/// copies).
#[test]
fn a_panel_of_routed_copies_has_no_unrouted_pad_and_one_note() {
    let findings = lint(&panel(4, |_| ("10k", ALL)));
    assert!(open_pads(&findings).is_empty(), "{findings:?}");
    let notes = panel_notes(&findings);
    assert_eq!(notes.len(), 1, "{findings:?}");
    assert_eq!(notes[0].severity, Severity::Low);
    assert!(
        notes[0].message.contains("4 copies"),
        "{}",
        notes[0].message
    );
    assert_eq!(notes[0].nets, ["GND", "SIG", "VCC"]);
}

/// Case B: one copy lacks the GND track between its LED and its header. That
/// copy is missing a connection inside itself, which `KiCad` reports too.
#[test]
fn a_missing_track_inside_one_copy_is_still_an_unrouted_pad() {
    let findings = lint(&panel(4, |k| {
        let gnd = k != 2;
        ("10k", Routing { gnd, ..ALL })
    }));
    let open = open_pads(&findings);
    assert_eq!(open.len(), 1, "{findings:?}");
    assert_eq!(open[0].nets, ["GND"]);
    // Copy 2 sits at x = 60: its pads are D1.2 at (64, 9) and J1.2 at (66, 9).
    assert!(open[0].message.contains("near (6"), "{}", open[0].message);
    assert_eq!(panel_notes(&findings).len(), 1, "{findings:?}");
}

/// Case C: copies that differ in a value keep their duplicate-designator
/// records distinct, and the missing track in one copy is still found.
#[test]
fn copies_that_differ_in_a_value_still_report_the_open_copy() {
    let text = panel(4, |k| {
        (
            if k == 1 { "22k" } else { "10k" },
            Routing { gnd: k != 3, ..ALL },
        )
    });
    let pcb = ExtractedBoard::from_kicad_pcb(&text).expect("board parses");
    assert!(
        pcb.components
            .iter()
            .any(|c| c.reference.starts_with("R1@conflict-")),
        "the value difference keeps the records distinct"
    );
    let findings = pcb.layout_lint(&text, None).findings;
    assert_eq!(open_pads(&findings).len(), 1, "{findings:?}");
}

/// A copy missing the tracks that were its header's only links to the rest of
/// the copy no longer groups with the other copies. Its routed nets are still
/// recognised as whole copies, and its broken ones are still reported.
#[test]
fn a_copy_cut_in_two_is_not_silenced() {
    let findings = lint(&panel(4, |k| {
        let cut = k == 1;
        (
            "10k",
            Routing {
                vcc: !cut,
                gnd: !cut,
                ..ALL
            },
        )
    }));
    let open = open_pads(&findings);
    let nets: Vec<&str> = open
        .iter()
        .flat_map(|f| f.nets.iter().map(String::as_str))
        .collect();
    assert!(nets.contains(&"VCC") && nets.contains(&"GND"), "{open:?}");
    assert!(
        !nets.contains(&"SIG"),
        "SIG is routed in every copy: {open:?}"
    );
}

/// The same missing track in every copy is a design fault, not the panel.
#[test]
fn the_same_gap_in_every_copy_is_reported_in_each() {
    let findings = lint(&panel(4, |_| ("10k", Routing { gnd: false, ..ALL })));
    assert_eq!(open_pads(&findings).len(), 4, "{findings:?}");
}

/// Control: a board with no repeated designators is judged as it always was.
#[test]
fn a_single_copy_has_no_panel_note() {
    let findings = lint(&panel(1, |_| ("10k", ALL)));
    assert!(findings.is_empty(), "{findings:?}");
    let findings = lint(&panel(1, |_| ("10k", Routing { gnd: false, ..ALL })));
    assert_eq!(open_pads(&findings).len(), 1, "{findings:?}");
    assert!(panel_notes(&findings).is_empty(), "{findings:?}");
}

/// One copy of a circuit in two clusters: R1/D1 on the SIG1 track and R2/D2 on
/// the SIG2 track, 20 mm apart. `join_vcc` / `join_gnd` draw the track that
/// ties the clusters on that net.
fn clustered_copy(ox: f64, join_vcc: bool, join_gnd: bool) -> String {
    let mut out = String::new();
    for (cluster, oy, sig) in [(1, 0.0, "SIG1"), (2, 20.0, "SIG2")] {
        let sig_net = cluster + 3;
        let (oy2, x4) = (oy + 6.0, ox + 4.0);
        let _ = write!(
            out,
            r#"  (footprint "lib:R" (layer "F.Cu") (at {ox} {oy})
    (property "Reference" "R{cluster}") (property "Value" "10k")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "VCC"))
    (pad "2" smd rect (at 4 0) (size 1 1) (layers "F.Cu") (net {sig_net} "{sig}")))
  (footprint "lib:LED" (layer "F.Cu") (at {ox} {oy2})
    (property "Reference" "D{cluster}") (property "Value" "LED")
    (pad "1" smd rect (at 4 0) (size 1 1) (layers "F.Cu") (net {sig_net} "{sig}"))
    (pad "2" smd rect (at 8 0) (size 1 1) (layers "F.Cu") (net 2 "GND")))
  (segment (start {x4} {oy}) (end {x4} {oy2}) (width 0.25) (layer "F.Cu") (net {sig_net}))
"#
        );
    }
    let mut join = |on: bool, from: (f64, f64), to: (f64, f64), net: u8| {
        if on {
            let _ = writeln!(
                out,
                "  (segment (start {} {}) (end {} {}) (width 0.25) (layer \"F.Cu\") (net {net}))",
                from.0, from.1, to.0, to.1
            );
        }
    };
    join(join_vcc, (ox, 0.0), (ox, 20.0), 1);
    join(join_gnd, (ox + 8.0, 6.0), (ox + 8.0, 26.0), 2);
    out
}

fn clustered_panel(copies: usize, with: impl Fn(usize) -> (bool, bool)) -> String {
    let mut text = String::from(
        r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
  (net 0 "")
  (net 1 "VCC")
  (net 2 "GND")
  (net 4 "SIG1")
  (net 5 "SIG2")
"#,
    );
    for k in 0..copies {
        let (vcc, gnd) = with(k);
        #[allow(clippy::cast_precision_loss)] // a handful of copies
        text.push_str(&clustered_copy(k as f64 * PITCH, vcc, gnd));
    }
    text.push_str(")\n");
    text
}

/// Every copy of a panel is unrouted between its two clusters, the VCC and GND
/// ties missing in each. The clusters are themselves repeated across the
/// copies, which must not make each look like a whole copy: every copy has two
/// open connections of its own.
#[test]
fn the_same_cut_in_every_copy_is_reported_in_each() {
    let findings = lint(&clustered_panel(4, |_| (false, false)));
    let open = open_pads(&findings);
    assert_eq!(open.len(), 8, "{findings:?}");
    for net in ["VCC", "GND"] {
        assert_eq!(
            open.iter().filter(|f| f.nets == [net]).count(),
            4,
            "one {net} gap per copy: {open:?}"
        );
    }
    assert_eq!(panel_notes(&findings).len(), 1, "{findings:?}");
}

/// The same panel with the VCC tie drawn in every copy: only GND is open.
#[test]
fn a_cut_clustered_copy_reports_only_the_net_it_lacks() {
    let findings = lint(&clustered_panel(4, |_| (true, false)));
    let open = open_pads(&findings);
    assert_eq!(open.len(), 4, "{findings:?}");
    assert!(open.iter().all(|f| f.nets == ["GND"]), "{open:?}");
}

/// Routed clustered copies are quiet apart from the panel note, and one copy
/// cut among them is the one finding.
#[test]
fn clustered_copies_routed_are_quiet_and_one_cut_is_found() {
    let findings = lint(&clustered_panel(4, |_| (true, true)));
    assert!(open_pads(&findings).is_empty(), "{findings:?}");
    assert_eq!(panel_notes(&findings).len(), 1, "{findings:?}");
    let findings = lint(&clustered_panel(4, |k| (k != 2, true)));
    let open = open_pads(&findings);
    assert_eq!(open.len(), 1, "{findings:?}");
    assert_eq!(open[0].nets, ["VCC"]);
    assert!(open[0].message.contains("near (6"), "{}", open[0].message);
}

/// Footprints with no real designator (`REF**`) share a pad name without being
/// copies of anything. Two of them on a net, one joined to the rest and one
/// not, is an open connection, not a panel.
#[test]
fn unannotated_footprints_do_not_make_a_panel() {
    let text = r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
  (net 0 "")
  (net 1 "GND")
  (footprint "lib:H" (layer "F.Cu") (at 0 0)
    (property "Reference" "H1") (property "Value" "HOLE")
    (pad "1" smd rect (at 0 0) (size 1.6 1.6) (layers "F.Cu") (net 1 "GND")))
  (footprint "lib:Fid" (layer "F.Cu") (at 10 0)
    (property "Reference" "REF**") (property "Value" "Fid")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "GND")))
  (footprint "lib:Fid" (layer "F.Cu") (at 40 0)
    (property "Reference" "REF**") (property "Value" "Fid")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "GND")))
  (segment (start 0 0) (end 10 0) (width 0.25) (layer "F.Cu") (net 1))
)
"#;
    let findings = lint(text);
    let open = open_pads(&findings);
    assert_eq!(open.len(), 1, "{findings:?}");
    assert!(open[0].message.contains("near (40"), "{}", open[0].message);
    assert!(panel_notes(&findings).is_empty(), "{findings:?}");
}
