//! Hostile and damaged Eagle files are refused with a sentence that names the
//! cause and what to do, never "parsed, but is empty" and never a partial read
//! passed off as the whole board (adversarial hostile-input findings W5, W6,
//! W10). Fixtures are built inline: the smallest text that reproduces each.

use hauksbee_extract::reader::Registry;
use hauksbee_extract::ExtractedBoard;

const BOARD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<eagle version="9.6.2"><drawing><board><libraries><library name="l"><packages>
<package name="P"><smd name="1" x="0" y="0" dx="1" dy="1" layer="1"/></package>
</packages></library></libraries>
<elements><element name="R1" library="l" package="P" value="10k" x="0" y="0"/></elements>
<signals><signal name="GND"><contactref element="R1" pad="1"/></signal>
<signal name="VCC"><contactref element="R1" pad="1"/></signal></signals>
</board></drawing></eagle>"#;

fn refusal(text: &str) -> String {
    match ExtractedBoard::from_eagle_brd(text) {
        Ok(board) => panic!("expected a refusal, read {} nets", board.nets.len()),
        Err(error) => error.to_string(),
    }
}

fn says(msg: &str, phrases: &[&str]) {
    for phrase in phrases {
        assert!(
            msg.contains(phrase),
            "message must mention {phrase:?}; got: {msg}"
        );
    }
    assert!(!msg.contains("parsed, but is empty"), "got: {msg}");
}

#[test]
fn the_intact_board_still_reads() {
    let board = ExtractedBoard::from_eagle_brd(BOARD).expect("intact board reads");
    assert_eq!(board.components.len(), 1);
}

#[test]
fn w5_an_eagle_schematic_alone_says_it_needs_its_board_file() {
    let sch = r#"<eagle version="9.6.2"><drawing><schematic><parts>
<part name="R1" library="l" deviceset="R" device="" value="10k"/></parts>
<sheets><sheet><nets/></sheet></sheets></schematic></drawing></eagle>"#;
    says(&refusal(sch), &["Eagle schematic", ".brd", "--schematic"]);
}

#[test]
fn w6_xml_cut_after_a_complete_signal_is_a_truncated_file() {
    // The cut lands between two signals: everything before it is well formed,
    // so a streaming read used to return the nets that survived.
    let cut = BOARD.find("<signal name=\"VCC\">").expect("second signal");
    let msg = refusal(&BOARD[..cut]);
    says(&msg, &["truncated", "Re-save or re-copy"]);
}

#[test]
fn w6_xml_cut_inside_a_tag_is_a_truncated_file() {
    let cut = BOARD.find("<contactref").expect("a contact") + 8;
    says(&refusal(&BOARD[..cut]), &["truncated"]);
}

#[test]
fn w10_runaway_nesting_is_refused_for_safety_naming_the_limit() {
    let deep = format!(
        "<eagle><drawing><board>{}{}</board></drawing></eagle>",
        "<a>".repeat(200),
        "</a>".repeat(200)
    );
    says(
        &refusal(&deep),
        &["nests XML elements", "64 levels", "refused"],
    );
}

#[test]
fn w10_a_huge_attribute_is_refused_naming_the_attribute_and_limit() {
    let big = format!(
        r#"<eagle><drawing><board><elements><element name="R1" value="{}"/></elements></board></drawing></eagle>"#,
        "9".repeat(200_000)
    );
    says(&refusal(&big), &["value=", "KiB", "refused for safety"]);
}

#[test]
fn w10_an_entity_declaration_is_refused_and_never_resolved() {
    let xxe = r#"<?xml version="1.0"?>
<!DOCTYPE eagle [<!ENTITY x SYSTEM "file:///etc/passwd">]>
<eagle><drawing><board><signals><signal name="&x;"/></signals></board></drawing></eagle>"#;
    let msg = refusal(xxe);
    says(&msg, &["entities", "refused"]);
    assert!(!msg.contains("root:"), "the file must not be read: {msg}");
}

#[test]
fn w10_an_entity_bomb_is_named_by_the_sniffer_not_called_unrecognized_garbage() {
    let bomb = r#"<?xml version="1.0"?>
<!DOCTYPE lolz [<!ENTITY lol "lol"><!ENTITY lol2 "&lol;&lol;&lol;&lol;">]>
<lolz>&lol2;</lolz>"#;
    if let Some(reader) = Registry::builtin().detect(bomb.as_bytes(), None) {
        panic!("no reader may claim an entity bomb, got {}", reader.name());
    }
    let msg = hauksbee_extract::reader::unrecognized_message(bomb.as_bytes());
    says(&msg, &["entities", "refused"]);
}
