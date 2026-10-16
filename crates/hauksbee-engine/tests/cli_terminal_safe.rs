//! A third-party board file must not be able to drive the reader's terminal: net
//! names and references are printed back by the human surfaces, and a terminal
//! obeys escape sequences and bidi overrides instead of showing them.

use crate::support::{combined, run_board};

fn board_with_net(net: &str) -> String {
    format!(
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
 (net 0 "")
 (net 1 "GND")
 (net 2 "{net}")
 (module Resistor_SMD:R_0805_2012Metric (layer F.Cu) (at 110 100)
  (fp_text reference "R3" (at 0 0) (layer F.SilkS))
  (fp_text value "1k" (at 0 2) (layer F.Fab))
  (pad 1 smd rect (at -1 0) (net 2 "{net}"))
  (pad 2 smd rect (at 1 0) (net 1 "GND"))
 )
)
"#
    )
}

fn terminal_active(text: &str) -> Vec<char> {
    text.chars()
        .filter(|c| hauksbee_ir::terminal_safe::is_terminal_active(*c))
        .collect()
}

fn output_for(net: &str, args: &[&str]) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("hostile.kicad_pcb");
    std::fs::write(&path, board_with_net(net)).expect("write board");
    combined(&run_board(&path, args))
}

#[test]
fn hostile_net_names_are_shown_not_obeyed() {
    for (label, net) in [
        ("escape", "PROG\u{1b}[31mRED\u{1b}[0m"),
        ("bell", "PROG\u{7}X"),
        ("rlo", "\u{202e}PROG"),
    ] {
        for args in [
            &["--check", "--plain"][..],
            &["--check"][..],
            &["--list-nets"][..],
        ] {
            let text = output_for(net, args);
            assert!(
                terminal_active(&text).is_empty(),
                "{label} {args:?} reached the terminal raw: {:?}",
                terminal_active(&text)
            );
        }
    }
}

#[test]
fn ordinary_names_pass_through_unchanged() {
    // `--list-nets` prints every net name; a healthy `--check` report does not
    // name a net at all, so it cannot show a name passing through.
    let text = output_for("VBUS_5V", &["--list-nets"]);
    assert!(text.contains("VBUS_5V"), "{text}");
    assert!(!text.contains("\\u00"), "{text}");
}
