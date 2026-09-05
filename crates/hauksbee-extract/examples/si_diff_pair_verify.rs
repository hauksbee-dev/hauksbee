//! Verification harness for the diff-pair cross-device false-positive fix.
//! Runs the exact production `si_checks` path on the four boards from the bug
//! report and prints the rendered SI report (USB / impedance lines), so the
//! before/after can be quoted without the (unrelated) qemu build break blocking
//! the `hauksbee` binary. Pass board paths as args.
use hauksbee_extract::{render_si, ExtractedBoard};

fn main() {
    for path in std::env::args().skip(1) {
        println!("===== {path} =====");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                println!("  read error: {e}");
                continue;
            }
        };
        let board = match ExtractedBoard::from_auto(&text) {
            Ok(b) => b,
            Err(e) => {
                println!("  extract error: {e:?}");
                continue;
            }
        };
        let report = board.si_checks(Some(&text));
        let rendered = render_si(&report);
        for line in rendered.lines() {
            let l = line.to_ascii_lowercase();
            if l.contains("usb")
                || l.contains("diff")
                || l.contains("impedance")
                || l.contains("skew")
                || l.contains("width")
            {
                println!("{line}");
            }
        }
        println!();
    }
}
