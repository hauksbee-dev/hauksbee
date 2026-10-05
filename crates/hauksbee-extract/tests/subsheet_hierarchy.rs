//! A hierarchical sub-sheet is not a design, and asking connectivity questions
//! of one alone produces confident nonsense.
//!
//! A child sheet's `(hierarchical_label "X")` is wired to a `(sheet (pin "X"))`
//! in its parent. Read the child file on its own and that net touches exactly one
//! pin, which is the same shape as a genuinely floating stub. `net_lint`'s
//! floating-control-pin check duly raised six [high] findings across four MNT
//! Reform sub-sheets, one of them on `USB_PWR_EN`, a net driven from
//! `reform2-lpc.kicad_sch` in the same project. The top-level
//! `reform2-motherboard30.kicad_sch` was clean the whole time.
//!
//! The fix is in the loader, not in the check: `from_kicad_schematic_path`
//! resolves the hierarchy a sub-sheet belongs to and extracts from its root, and
//! refuses with a message naming what it needs when no parent exists. These tests
//! pin the false-positive case, the clean top-level case, and the refusal.

use std::path::PathBuf;

use hauksbee_extract::{ExtractedBoard, LintCheck, Severity};

use crate::support::fixture;

fn floating_control_highs(board: &ExtractedBoard) -> Vec<String> {
    board
        .net_lint()
        .of_check(LintCheck::FloatingControlPin)
        .filter(|f| f.severity == Severity::High)
        .map(|f| f.message.clone())
        .collect()
}

/// The bug, reproduced against the single-sheet reader: the child's `USB_PWR_EN`
/// net touches only U1's EN pin, so the check calls it floating. This is the
/// behaviour the loader now keeps callers away from, and it is asserted here so
/// the fix cannot be mistaken for the check having gone quiet.
#[test]
fn a_sub_sheet_read_alone_looks_like_a_floating_control_pin() {
    let text = std::fs::read_to_string(fixture("subsheet_child.kicad_sch")).expect("read fixture");
    let board = ExtractedBoard::from_kicad_schematic(&text).expect("extract single sheet");
    let highs = floating_control_highs(&board);
    assert_eq!(
        highs.len(),
        1,
        "the single-sheet read is expected to mis-see the sheet pin as floating; \
         got {highs:#?}"
    );
    assert!(
        highs[0].contains("USB_PWR_EN"),
        "the mis-seen net is the one wired to the parent's sheet pin: {}",
        highs[0]
    );
}

/// The fix: handed the same child file *by path*, the loader finds the parent
/// that owns it and extracts the whole hierarchy, where `USB_PWR_EN` is pulled
/// up by R1. No finding.
#[test]
fn a_sub_sheet_by_path_pulls_in_its_parent_and_is_clean() {
    let board = ExtractedBoard::from_kicad_schematic_path(&fixture("subsheet_child.kicad_sch"))
        .expect("the child resolves through its parent");
    // The parent's pull-up is present, which is how we know the hierarchy came in
    // and not just that the check stopped firing.
    assert!(
        board.components.iter().any(|c| c.reference == "R1"),
        "the parent's R1 pull-up must be in the extraction, got {:?}",
        board
            .components
            .iter()
            .map(|c| &c.reference)
            .collect::<Vec<_>>()
    );
    assert!(
        board.components.iter().any(|c| c.reference == "U1"),
        "the child's U1 must still be in the extraction"
    );
    let highs = floating_control_highs(&board);
    assert!(
        highs.is_empty(),
        "USB_PWR_EN is driven from the parent sheet; nothing floats: {highs:#?}"
    );
}

/// The top-level sheet of the same hierarchy: clean, and clean for the same
/// reason. Loading the root was never the broken case, and this pins that the
/// sub-sheet path now agrees with it.
#[test]
fn the_top_level_sheet_is_clean() {
    let board = ExtractedBoard::from_kicad_schematic_path(&fixture("subsheet_parent.kicad_sch"))
        .expect("extract the root");
    let highs = floating_control_highs(&board);
    assert!(highs.is_empty(), "the root sheet is clean: {highs:#?}");
}

/// A sub-sheet with no parent anywhere: refused, with the refusal naming both
/// what the file is and what would be needed to answer. Returning a netlist here
/// would be returning a guess dressed as a measurement.
#[test]
fn an_orphan_sub_sheet_is_refused_with_a_message_naming_what_it_needs() {
    let err = ExtractedBoard::from_kicad_schematic_path(&fixture("subsheet_orphan.kicad_sch"))
        .expect_err("an orphan sub-sheet cannot be extracted honestly");
    let msg = err.to_string();
    assert!(
        msg.contains("sub-sheet"),
        "the message must say what the file is: {msg}"
    );
    assert!(
        msg.contains("root schematic"),
        "the message must name what it needs: {msg}"
    );
    assert!(
        msg.contains("subsheet_orphan"),
        "the message must name the file: {msg}"
    );
}

// ---------------------------------------------------------------------------
// The corpus case this was found on. Corpus-gated; a scan of zero fails.
// ---------------------------------------------------------------------------

/// Every MNT Reform motherboard sub-sheet, plus the top-level sheet: no
/// floating-control-pin [high] anywhere. Six of these fired before the loader
/// resolved the hierarchy.
#[test]
fn mnt_reform_sub_sheets_raise_no_floating_control_pin() {
    let Some(root) = hauksbee_testkit::corpus_boards_root_or_skip(
        env!("CARGO_MANIFEST_DIR"),
        "MNT Reform sub-sheet hierarchy guard",
    ) else {
        return;
    };
    let dir = root.join("mnt_reform");
    if !dir.is_dir() {
        assert!(
            !hauksbee_testkit::require_assets(),
            "HAUKSBEE_REQUIRE_CORPUS=1 but {} is absent",
            dir.display()
        );
        eprintln!("NOT RUN  MNT Reform absent");
        return;
    }
    // Every schematic in the laptop, across all 19 boards: the motherboard
    // revisions and the keyboard revisions each have their own hierarchy, and
    // both contributed to the original findings.
    let mut sheets: Vec<PathBuf> = Vec::new();
    let mut stack = vec![dir];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|x| x.to_str()) == Some("kicad_sch") {
                sheets.push(p);
            }
        }
    }
    sheets.sort();

    let mut offenders: Vec<String> = Vec::new();
    let mut checked = 0usize;
    // What the single-sheet read makes of the same files. Counted so this guard
    // states the size of the bug it protects against rather than asserting the
    // absence of something that might never have been there: six [high] findings
    // across four sub-sheets, on this project, at this pinned revision.
    let mut single_sheet_highs = 0usize;
    for sheet in &sheets {
        if let Ok(text) = std::fs::read_to_string(sheet) {
            if let Ok(alone) = ExtractedBoard::from_kicad_schematic(&text) {
                single_sheet_highs += floating_control_highs(&alone).len();
            }
        }
        let board = match ExtractedBoard::from_kicad_schematic_path(sheet) {
            Ok(b) => b,
            // An orphan sub-sheet is a refusal, not a finding: the loader said it
            // cannot answer, which is the honest outcome and not a false positive.
            Err(e) => {
                eprintln!("REFUSED  {}: {e}", sheet.display());
                continue;
            }
        };
        checked += 1;
        for m in floating_control_highs(&board) {
            offenders.push(format!("{}: {m}", sheet.display()));
        }
    }
    hauksbee_testkit::scanned("MNT Reform sub-sheet hierarchy guard", checked);
    eprintln!(
        "single-sheet reads of the same {} file(s) produce {single_sheet_highs} \
         floating-control-pin [high] finding(s)",
        sheets.len()
    );
    assert!(
        single_sheet_highs > 0,
        "the sub-sheet mis-read is what this guard exists for; if the single-sheet \
         read is now clean too, either the project changed or the check stopped \
         firing, and this test is no longer proving anything"
    );
    assert!(
        offenders.is_empty(),
        "no MNT Reform sheet, top-level or sub, has a floating control pin; \
         a finding here is the sibling-sheet driver being missed:\n  {}",
        offenders.join("\n  ")
    );
}

/// A sub-sheet placed TWICE is two sets of parts, not one.
///
/// This is the ordinary way to draw a multi-channel board, and the reader used
/// to skip every placement after the first: the cycle guard that stops a sheet
/// referencing itself was keyed on the file rather than on the ancestor chain,
/// so a reused sheet looked like a sheet already in the netlist. The cost was
/// invisible and large. LumenPnP's motherboard places `mosfet.kicad_sch` four
/// times and `motor_driver.kicad_sch` six, and extracted 188 of its 359 parts,
/// with three of its six MOSFETs missing and every coverage ratio computed over
/// what survived.
///
/// KiCad's own answer is in each symbol's `(instances)` block, which maps the
/// sheet-instance UUID path to that placement's designator. The reader already
/// read it; nothing ever reached it a second time.
#[test]
fn a_sub_sheet_placed_twice_yields_both_placements() {
    let board = ExtractedBoard::from_kicad_schematic_path(&fixture("reused_sheet_top.kicad_sch"))
        .expect("extract the reused-sheet hierarchy");
    let mut refs: Vec<&str> = board
        .components
        .iter()
        .filter(|c| !c.reference.starts_with('#'))
        .map(|c| c.reference.as_str())
        .collect();
    refs.sort_unstable();
    assert_eq!(
        refs,
        vec!["R1", "R2"],
        "both placements of the child sheet must appear, each with its own \
         per-instance designator"
    );
    // The two channels are separate nets: merging them would be the other half
    // of the same bug, a reused sheet whose placements short together through
    // their identical local names.
    let chan_nets = board
        .nets
        .iter()
        .filter(|n| n.name.contains("CHAN"))
        .count();
    assert!(
        chan_nets >= 2,
        "each placement's CHAN is its own net, got {chan_nets}: {:#?}",
        board.nets.iter().map(|n| &n.name).collect::<Vec<_>>()
    );
}

/// A reused sheet can draw a different gate of one shared part in each
/// placement: the instances block gives each placement its own `(unit N)`.
/// Taking the template's unit for both placements put the same pin in both
/// channels, shorting them, and dropped the other gate's pins.
#[test]
fn a_reused_sheet_takes_each_placements_unit() {
    let board = ExtractedBoard::from_kicad_schematic_path(&fixture("reused_unit_top.kicad_sch"))
        .expect("extract the reused-unit hierarchy");
    let u1 = board
        .components
        .iter()
        .find(|c| c.reference == "U1")
        .expect("U1");
    let net = |number: &str| {
        u1.pins
            .iter()
            .find(|p| p.number == number)
            .and_then(|p| p.net)
    };
    let (gate_a, gate_b) = (net("1"), net("3"));
    assert!(gate_a.is_some() && gate_b.is_some(), "{:?}", u1.pins);
    assert_ne!(gate_a, gate_b, "each placement's gate is on its own CHAN");
}

/// A local label is scoped to one placement of its sheet: a sheet placed
/// twice whose `CLAMP` label joins two parts makes two nets, named the way
/// `kicad-cli sch export netlist` names them (`/bank_a/CLAMP`,
/// `/bank_b/CLAMP`), not one net shorting both placements together.
#[test]
fn a_local_label_in_a_reused_sheet_is_one_net_per_placement() {
    let board =
        ExtractedBoard::from_kicad_schematic_path(&fixture("local_label_reuse_top.kicad_sch"))
            .expect("extract the reused-sheet hierarchy");
    let net_of = |reference: &str, number: &str| {
        let net = board
            .components
            .iter()
            .find(|c| c.reference == reference)
            .and_then(|c| c.pins.iter().find(|p| p.number == number))
            .and_then(|p| p.net)
            .expect("pin is connected");
        board
            .nets
            .iter()
            .find(|n| n.id == net)
            .map(|n| n.name.clone())
            .expect("net exists")
    };
    assert_eq!(net_of("R1", "2"), net_of("R2", "1"), "joined within bank_a");
    assert_eq!(net_of("R3", "2"), net_of("R4", "1"), "joined within bank_b");
    assert_ne!(
        net_of("R1", "2"),
        net_of("R3", "2"),
        "placements stay apart"
    );
    assert_eq!(net_of("R1", "2"), "/bank_a/CLAMP");
    assert_eq!(net_of("R3", "2"), "/bank_b/CLAMP");
}

/// Each pin's net id in a hierarchy, keyed `REF.PIN`, for partition checks
/// against what `kicad-cli sch export netlist` reports for the same fixture.
fn pin_nets(top: &str) -> std::collections::HashMap<String, i64> {
    let board =
        ExtractedBoard::from_kicad_schematic_path(&fixture(top)).expect("extract hierarchy");
    board
        .components
        .iter()
        .flat_map(|c| {
            c.pins
                .iter()
                .filter_map(move |p| Some((format!("{}.{}", c.reference, p.number), p.net?)))
        })
        .collect()
}

/// A group bus naming a bus alias crosses a sheet boundary and joins each
/// member to the same-named label on both sides. `{CTL}` is defined in the
/// project file (where KiCad 9+ keeps aliases) and `{PAIR}` only in the child
/// sheet, yet both are in force on every sheet; a member spelled with markup
/// (`~{ALERT}`) joins its label like any other. kicad-cli nets: `/SCL` = R1.1
/// R3.1, `/~{ALERT}` = R1.2 R3.2, `/fpga/CLK.P` = R2.1 R4.1.
#[test]
fn a_bus_alias_carries_its_members_across_a_sheet_boundary() {
    let n = pin_nets("bus_alias_project_top.kicad_sch");
    assert_eq!(n["R1.1"], n["R3.1"], "SCL through the project alias");
    assert_eq!(n["R1.2"], n["R3.2"], "~{{ALERT}} through the project alias");
    assert_eq!(
        n["R2.1"], n["R4.1"],
        "CLK.P through a sibling sheet's alias"
    );
    assert_ne!(n["R1.1"], n["R1.2"]);
}

/// Group buses pair members by name, not position: the `{Q[0..1]}` sheet pin
/// on an `A{DIR[0..1] Q[0..1]}` bus carries `A.Q0`/`A.Q1`, not `A.DIR0`. Two
/// sheet pins on one unlabelled bus (`L{P N}`, `R{P N}`) join member-wise,
/// and a second placement of the left sheet on its own unlabelled bus stays
/// apart. kicad-cli nets: `/A.Q0` = R1.1 R2.1, `/A.DIR0` = R1.2,
/// `/left/L.P` = R3.1 R4.1, `/left/L.N` = R3.2 R4.2, `/solo/L.P` = R5.1.
#[test]
fn group_bus_members_pair_by_name_across_sheet_pins() {
    let n = pin_nets("bus_member_match_top.kicad_sch");
    assert_eq!(n["R1.1"], n["R2.1"], "A.Q0 meets the child's Q0");
    assert_ne!(n["R1.2"], n["R2.1"], "A.DIR0 is not Q0");
    assert_ne!(n["R1.2"], n["R2.2"], "A.DIR0 is not Q1");
    assert_eq!(n["R3.1"], n["R4.1"], "L.P meets R.P over the shared bus");
    assert_eq!(n["R3.2"], n["R4.2"], "L.N meets R.N over the shared bus");
    assert_ne!(n["R3.1"], n["R5.1"], "the solo placement stays apart");
    assert_ne!(n["R4.1"], n["R5.1"], "the solo placement stays apart");
}

/// A KiCad 6 hierarchy: its sheets name their file under `"Sheet file"`, a
/// power symbol whose Value (`+3.3V`) differs from its hidden pin's name
/// (`+3V3`) is named by the pin, and a symbol edited away from its library
/// is defined under its `lib_name`. kicad-cli nets: `+3V3` = R1.1 R2.1,
/// `/rails/X` = R1.2 R3.1.
#[test]
fn a_kicad_6_hierarchy_reads_like_kicad_cli_does() {
    let n = pin_nets("legacy_v6_top.kicad_sch");
    assert_eq!(n.len(), 6, "every resistor pin is on a net: {n:?}");
    assert_eq!(
        n["R1.1"], n["R2.1"],
        "+3.3V and +3V3 symbols share pin +3V3"
    );
    assert_eq!(n["R1.2"], n["R3.1"], "the lib_name part keeps its pins");
}

/// A KiCad 6 sheet placed twice keeps each placement's designator in the root
/// file's `symbol_instances` table; the symbol's own `Reference` property is
/// only one of them. kicad-cli nets: `/left/NODE` = R1.1, `/right/NODE` = R2.1.
#[test]
fn a_kicad_6_reused_sheet_takes_each_placements_designator() {
    let n = pin_nets("legacy_v6_reuse_top.kicad_sch");
    assert!(n.contains_key("R1.1"), "left placement is R1: {n:?}");
    assert!(n.contains_key("R2.1"), "right placement is R2: {n:?}");
    assert_ne!(n["R1.1"], n["R2.1"], "local NODE labels stay per placement");
}
