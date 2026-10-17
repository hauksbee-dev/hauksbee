//! A `.board` file (Board-as-Code) whose content is another format is read as
//! that format and the user is told the extension was wrong (adversarial
//! hostile-input finding W4). Every other wrong extension was already sniffed.

use hauksbee_bind::board_input::{from_bytes, from_path, InputKind};

const KICAD_PCB: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
 (net 0 "") (net 1 "GND")
 (module R:R_0805 (layer F.Cu) (at 10 10)
  (fp_text reference R1 (at 0 0) (layer F.SilkS))
  (fp_text value 10k (at 0 2) (layer F.Fab))
  (pad 1 smd rect (at -1 0) (net 1 "GND"))))
"#;

fn says_wrong_extension(notes: &[String]) {
    let note = notes
        .iter()
        .find(|n| n.contains("Board-as-Code"))
        .unwrap_or_else(|| panic!("a note must say the extension is wrong: {notes:?}"));
    for phrase in ["a.board", "a KiCad board", "Rename the file"] {
        assert!(
            note.contains(phrase),
            "note must mention {phrase:?}: {note}"
        );
    }
}

#[test]
fn kicad_content_in_a_board_file_is_read_with_a_note_from_bytes() {
    let norm = from_bytes("a.board", KICAD_PCB.as_bytes()).expect("read as a KiCad board");
    assert_eq!(norm.kind, InputKind::Text);
    assert_eq!(norm.board.components.len(), 1);
    says_wrong_extension(&norm.notes);
}

#[test]
fn kicad_content_in_a_board_file_is_read_with_a_note_from_a_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("a.board");
    std::fs::write(&path, KICAD_PCB).expect("write fixture");
    let norm = from_path(&path).expect("read as a KiCad board");
    assert_eq!(norm.board.components.len(), 1);
    says_wrong_extension(&norm.notes);
}

#[test]
fn a_real_board_as_code_file_gets_no_such_note() {
    let source = "// Board-as-Code\nboard version 1\n";
    // Whatever the compiler makes of this stub, the extension is not blamed.
    if let Ok(norm) = from_bytes("a.board", source.as_bytes()) {
        assert!(!norm.notes.iter().any(|n| n.contains("Rename the file")));
    }
}
