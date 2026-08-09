//! The web front door's two-sided contract for Eagle `.brd` uploads.
//!
//! Both halves are the same question asked from opposite ends: does the drop
//! zone's report tell the truth about what it read?
//!
//! * An Eagle 6+ board (XML) must come back as a REPORT: `ok`, a headline, an
//!   inventory, check sections. Everything the browser's export surface is
//!   derived from hangs off that payload, so a board whose report is missing any
//!   of it silently loses its "download the JSON" affordance.
//! * A pre-Eagle-6 board (binary) is a format hauksbee does not read, and must
//!   come back as a REFUSAL: not `ok`, no sections, no invented inventory, and
//!   an error that names the format and the one action that unlocks the file.
//!   A refusal that returned an empty-but-`ok` report would hand the browser
//!   something exportable about a file nothing opened.
//!
//! The release browser gate drives both sides through a real Chromium drop, over
//! the corpus's declared pre-Eagle-6 inputs and its Eagle XML boards; this test
//! pins the payload the gate's assertions rest on.

use hauksbee_engine::frontdoor::analyze;

/// A minimal Eagle 6 board: two 0805 resistors on one net, which is enough to
/// exercise extraction, binding and every section the report renders.
fn eagle_xml_board() -> String {
    r#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE eagle SYSTEM "eagle.dtd">
<eagle version="6.6.0">
<drawing>
<layers>
<layer number="1" name="Top" color="4" fill="1" visible="yes" active="yes"/>
<layer number="16" name="Bottom" color="1" fill="1" visible="yes" active="yes"/>
</layers>
<board>
<libraries>
<library name="lib">
<packages>
<package name="R0805">
<smd name="1" x="-0.95" y="0" dx="1.3" dy="1.5" layer="1"/>
<smd name="2" x="0.95" y="0" dx="1.3" dy="1.5" layer="1"/>
</package>
</packages>
</library>
</libraries>
<elements>
<element name="R1" library="lib" package="R0805" value="10k" x="10" y="10" rot="R0"/>
<element name="R2" library="lib" package="R0805" value="4k7" x="20" y="10" rot="R0"/>
</elements>
<signals>
<signal name="MID">
<contactref element="R1" pad="2"/>
<contactref element="R2" pad="1"/>
<wire x1="10.95" y1="10" x2="19.05" y2="10" width="0.3" layer="1"/>
</signal>
<signal name="GND">
<contactref element="R2" pad="2"/>
</signal>
</signals>
</board>
</drawing>
</eagle>
"#
    .to_string()
}

/// The head of a pre-Eagle-6 drawing record, followed by enough plausible record
/// bytes that nothing could read it as text. `era` is the 3.x (`0x80`) or
/// 4.x/5.x (`0x00`) marker that forms the magic with the `0x10` tag; `version`
/// is the byte after it, which is NOT part of the magic and varies by file.
fn eagle_binary_board(era: u8, version: u8) -> Vec<u8> {
    let mut bytes = vec![0x10, era, version, 0x00];
    bytes.extend_from_slice(&[0x21, 0x13, 0x00, 0x00, 0x05, 0x09, 0x04, 0x3c]);
    bytes.extend((0u16..600).map(|i| (i % 251) as u8));
    bytes
}

#[test]
fn an_eagle_xml_board_gets_the_full_exportable_report() {
    let report = analyze("shades_v40.brd", eagle_xml_board().as_bytes());
    assert!(report.ok, "an Eagle 6 board must read: {:?}", report.error);
    assert_eq!(report.error, None);
    assert_eq!(report.file_name, "shades_v40.brd");
    // The inventory line the browser renders under the verdict, and which the
    // release gate compares byte-for-byte against the downloaded JSON.
    assert_eq!(report.num_components, 2, "both elements extracted");
    assert!(report.num_nets >= 2, "signals became nets");
    assert!(
        !report.headline.trim().is_empty(),
        "the verdict headline carries the report's bottom line"
    );
    assert!(
        !report.sections.is_empty(),
        "an exportable report has check sections"
    );
}

#[test]
fn a_pre_eagle_6_binary_board_refuses_and_names_the_re_save() {
    // Both eras, and an era byte from each of the two real populations: the
    // Mutable Instruments corpus reports 0x64 and KiCad's own pre-v6 regression
    // boards report 0x30, 0x31, 0x6a and 0x72 there. Pinning that byte is how a
    // detector passes this crate's corpus and misses everyone else's files.
    for (flags, era) in [(0x00u8, 0x64u8), (0x80, 0x64), (0x00, 0x72), (0x80, 0x30)] {
        let report = analyze("braids_v50.brd", &eagle_binary_board(flags, era));
        assert!(
            !report.ok,
            "a pre-Eagle-6 binary board must not read (flags {flags:#04x})"
        );
        let error = report
            .error
            .as_deref()
            .unwrap_or_else(|| panic!("a refusal must say why (flags {flags:#04x})"));
        // Names the format rather than reciting the accepted-format list at
        // someone whose file becomes readable after one re-save.
        assert!(
            error.contains("pre-Eagle-6"),
            "the refusal must name the format: {error}"
        );
        assert!(
            error.contains("Eagle 6 or later") && error.contains("re-save"),
            "the refusal must name the action that unlocks the file: {error}"
        );
        // A lossy UTF-8 decode turns the `0x80` flags byte into U+FFFD and
        // destroys the header before any detector sees it, which is how the
        // generic message used to win on 28 of the corpus's 35 boards.
        assert!(
            !error.contains("unrecognized board format"),
            "the generic recital must not win: {error}"
        );
        // Nor may the accepted-format list be appended to it. The list answers
        // "what CAN you read" for a file nobody could identify; printed under a
        // refusal that just named this file's format and its fix, it reads as a
        // contradiction, and it lands directly above the browser's own list.
        assert!(
            !error.contains("Supported:"),
            "an identified format must not carry the accepted-format recital: {error}"
        );
        // Nothing exportable, and no invented inventory: this file was not read.
        assert!(
            report.sections.is_empty(),
            "a refusal has no check sections"
        );
        assert_eq!(report.num_components, 0);
        assert_eq!(report.num_nets, 0);
        assert!(report.components.is_empty());
    }
}

#[test]
fn a_binary_eagle_board_is_not_read_as_a_fab_netlist_by_a_stray_record() {
    // The IPC-D-356 reader claims any input with a line beginning `317`, `327`
    // or `367` ANYWHERE in it, scanning the whole file rather than a header
    // window. Binary board records can contain those three bytes just after a
    // newline, and when they did, the file was claimed, parsed as a fab netlist,
    // and came back as an `ok` board report listing a part it had invented from
    // binary noise — the exact confident-nonsense outcome this format's refusal
    // exists to prevent, and something the browser would happily export.
    //
    // Detection therefore runs on the RAW bytes ahead of reader selection, not
    // as a nicer message for whatever the readers left over.
    let mut bytes = vec![0x10, 0x80, 0x72, 0x00];
    bytes.extend(1u8..60);
    bytes.extend_from_slice(
        b"\n317GND              R1    -1    D0472PA00X+019000Y+029450X0945Y0945R180S0\n",
    );
    bytes.extend((0u16..400).map(|i| ((i * 7) % 251) as u8));

    let report = analyze("stray_record.brd", &bytes);
    assert!(
        !report.ok,
        "a stray IPC record must not turn a binary Eagle file into a board"
    );
    assert!(
        report
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("pre-Eagle-6"),
        "the refusal must still name the real format: {:?}",
        report.error
    );
    assert!(
        report.components.is_empty(),
        "no part may be invented from binary noise: {:?}",
        report.components
    );
}
