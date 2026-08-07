//! Adversarial CLI proof that each analysis-mode missing-card path carries the
//! complete C5.3 contract at exit 3, not a one-off error sentence.

use std::process::Command;

fn run(deck: &str, flag: &str) -> (i32, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("request.cir");
    std::fs::write(&path, deck).expect("write deck");
    let out = Command::new(env!("CARGO_BIN_EXE_hauksbee"))
        .args(["sim", path.to_str().unwrap(), flag])
        .output()
        .expect("run hauksbee");
    (
        out.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[test]
fn missing_analysis_cards_are_useful_exit_3_refusals() {
    let base = "contract\nV1 in 0 DC 5\nR1 in 0 1k\n.end\n";
    for flag in ["--tran", "--dc", "--ac"] {
        let (code, text) = run(base, flag);
        assert_eq!(code, 3, "{flag}: {text}");
        for label in [
            "refused claim:",
            "missing prerequisite:",
            "valid partial conclusions:",
            "next action:",
        ] {
            assert!(text.contains(label), "{flag} lost {label}:\n{text}");
        }
    }
}
