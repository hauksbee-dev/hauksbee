//! Real public boards running their real public firmware, end to end.
//!
//! Each case is a corpus board (`corpus.toml`) with firmware built from its
//! upstream source, run through `hauksbee-ci run` exactly as a user would, and
//! judged per assertion: what the firmware visibly did must PASS, and what the
//! run cannot honestly vouch for must stay INVALID with the part that makes it
//! so named. A case that starts passing an INVALID row has gained a model and
//! should be tightened; one that loses a PASS row is a regression.
//!
//! The firmware is not committed. Build it with the recipes recorded in the
//! run notes (pico-examples `sdk-2.2.0`, emonTx3 `411de961`, Arduino-ESP32
//! 1.0.6) and point `HAUKSBEE_REALFW_DIR` at a directory holding
//! `emontx34_ds_jeelib_classic.hex`, `thingplus_blink.elf` and
//! `esp32evb_getchipid.elf`. Each run takes several minutes of emulation, so
//! the cases are `#[ignore]`d: run them with `--run-ignored only`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::support::{bin, require_corpus, stdout, toml_path};

/// The corpus board at `rel`, or `None` with a skip note.
fn board(rel: &str) -> Option<PathBuf> {
    let found = hauksbee_testkit::corpus_board(env!("CARGO_MANIFEST_DIR"), rel);
    if found.is_none() {
        assert!(!require_corpus(), "corpus required but {rel} is missing");
        eprintln!("SKIP: corpus board {rel} absent");
    }
    found
}

/// The built firmware `name` under `HAUKSBEE_REALFW_DIR`, or `None` with a
/// skip note naming what is missing.
fn firmware(name: &str) -> Option<PathBuf> {
    let Some(dir) = std::env::var_os("HAUKSBEE_REALFW_DIR") else {
        eprintln!("SKIP: HAUKSBEE_REALFW_DIR unset (firmware {name} is built from source, not committed)");
        return None;
    };
    let path = Path::new(&dir).join(name);
    if !path.is_file() {
        eprintln!("SKIP: {} not built", path.display());
        return None;
    }
    Some(path)
}

/// Run `spec_body` (everything after `board`/`firmware`) and return the exit
/// code and the human report.
fn run(name: &str, board: &Path, firmware: &Path, spec_body: &str) -> (i32, String) {
    let dir = tempfile::tempdir().expect("temp dir");
    let spec = dir.path().join(format!("{name}.toml"));
    let text = format!(
        "name = \"{name}\"\nboard = {}\nfirmware = {}\n{spec_body}",
        toml_path(board),
        toml_path(firmware)
    );
    std::fs::write(&spec, text).expect("spec writes");
    let out = Command::new(bin())
        .arg("run")
        .arg(&spec)
        .output()
        .expect("hauksbee-ci runs");
    (out.status.code().unwrap_or(-1), stdout(&out))
}

/// The verdict tag (`PASS`, `FAIL`, `INVALID`) the report printed for the
/// assertion whose label starts with `label`, and the lines of its block.
fn verdict<'a>(report: &'a str, label: &str) -> (&'a str, Vec<&'a str>) {
    let mut lines = report.lines();
    while let Some(line) = lines.next() {
        let t = line.trim_start();
        let Some(rest) = t.strip_prefix('[') else {
            continue;
        };
        let Some((tag, text)) = rest.split_once("] ") else {
            continue;
        };
        if text.starts_with(label) {
            let indent = line.len() - t.len();
            let block = lines
                .by_ref()
                .take_while(|l| l.len() - l.trim_start().len() > indent)
                .collect();
            return (tag, block);
        }
    }
    panic!("no assertion labelled {label:?} in:\n{report}");
}

fn assert_verdict(report: &str, label: &str, want: &str, named: &[&str]) {
    let (tag, block) = verdict(report, label);
    assert_eq!(tag, want, "{label}:\n{report}");
    let block = block.join("\n");
    for part in named {
        assert!(block.contains(part), "{label} must name {part}:\n{block}");
    }
}
