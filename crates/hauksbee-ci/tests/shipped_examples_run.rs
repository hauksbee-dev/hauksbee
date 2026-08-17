//! Every example spec we ship must actually run.
//!
//! The examples are the first thing a new user copies, and they are referenced
//! from the README, the install script's closing lines, the bundle's VERIFY
//! block and the Docker page. A spec whose board moved, whose net was renamed,
//! or whose assertion key drifted fails for the reader and, without this gate,
//! passes for us.
//!
//! The gate is deliberately about the SPEC, not the verdict: exit 2 is
//! "hauksbee could not make sense of this file". RED is a legitimate outcome
//! for an example that exists to demonstrate a caught fault, and several do.
//!
//! Specs whose board or firmware lives outside the tree (the fetched corpus,
//! the large testdata firmware) skip, and say which and why. A silent skip is
//! how the corpus gate managed never to run for anyone.

use std::path::{Path, PathBuf};
use std::process::Command;

fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples")
}

/// The board / firmware a spec points at, resolved relative to the spec.
fn referenced_files(spec: &Path) -> Vec<PathBuf> {
    let text = std::fs::read_to_string(spec).expect("read spec");
    let dir = spec.parent().expect("spec has a parent");
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with('#') {
                return None;
            }
            let key = line.split('=').next()?.trim();
            if key != "board" && key != "firmware" {
                return None;
            }
            let val = line.split_once('=')?.1.trim().trim_matches('"');
            Some(dir.join(val))
        })
        .collect()
}

#[test]
fn every_shipped_example_spec_is_one_hauksbee_understands() {
    let mut ran = 0;
    let mut skipped: Vec<String> = Vec::new();

    let mut specs: Vec<PathBuf> = std::fs::read_dir(examples_dir())
        .expect("read examples/")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    specs.sort();
    assert!(
        specs.len() >= 5,
        "the examples directory should not be near-empty: {specs:?}"
    );

    for spec in &specs {
        let name = spec.file_name().unwrap().to_string_lossy().to_string();
        if let Some(missing) = referenced_files(spec).into_iter().find(|p| !p.exists()) {
            skipped.push(format!("{name} (needs {})", missing.display()));
            continue;
        }
        let out = Command::new(env!("CARGO_BIN_EXE_hauksbee-ci"))
            .arg("run")
            .arg(spec)
            .output()
            .expect("run hauksbee-ci");
        let code = out.status.code();
        assert_ne!(
            code,
            Some(2),
            "{name} is a spec hauksbee cannot make sense of, and we ship it as an example:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        ran += 1;
    }

    // Say what was not covered, loudly. A skip nobody reads is a gate nobody has.
    if !skipped.is_empty() {
        eprintln!(
            "skipped {} example spec(s) whose files are not in this tree:\n  {}",
            skipped.len(),
            skipped.join("\n  ")
        );
    }
    assert!(
        ran >= 3,
        "at least a few examples must be runnable from a bare checkout, or this \
         gate proves nothing. ran={ran}, skipped={skipped:?}"
    );
}

// M4: no shipped example may demonstrate the hollow gate. An `ideal` supply
// holds its net at its programmed voltage, so a voltage / rail_window assertion
// on that same net cannot fail for a board reason, and the run says COVERAGE
// HOLE about it. `hauksbee-ci init` refuses to scaffold that shape; the examples
// are what people copy, so they must not ship it either.
//
// Checked by reading the specs, not by running them, so it holds for the
// examples whose boards come from the fetched corpus too.
#[test]
fn no_example_asserts_a_rail_its_own_ideal_source_holds() {
    let mut offenders: Vec<String> = Vec::new();
    for spec in std::fs::read_dir(examples_dir())
        .expect("read examples/")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
    {
        let text = std::fs::read_to_string(&spec).expect("read spec");
        let value: toml::Value = match text.parse() {
            Ok(v) => v,
            Err(e) => panic!("{} is not valid TOML: {e}", spec.display()),
        };
        let array = |key: &str| -> Vec<toml::Value> {
            value
                .get(key)
                .and_then(toml::Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let str_field = |v: &toml::Value, key: &str| -> Option<String> {
            v.get(key).and_then(toml::Value::as_str).map(str::to_string)
        };
        // Nets an ideal source drives directly: a declared `ideal` supply leg or
        // a `[[net_drive]]`. Behavioural legs (bench / wall / usb / battery) are
        // exactly what a rail assertion is for, so they are fine.
        let ideal_fed: Vec<String> = array("supply")
            .iter()
            .filter(|s| str_field(s, "kind").as_deref() == Some("ideal"))
            .filter_map(|s| str_field(s, "net"))
            .chain(
                array("net_drive")
                    .iter()
                    .filter_map(|d| str_field(d, "net")),
            )
            .collect();
        for a in array("assert") {
            let kind = str_field(&a, "kind").unwrap_or_default();
            if kind != "voltage" && kind != "rail_window" {
                continue;
            }
            if let Some(net) = str_field(&a, "net") {
                if ideal_fed.contains(&net) {
                    offenders.push(format!(
                        "{}: {kind} assertion on '{net}', which the spec's own ideal \
                         source holds at its programmed voltage",
                        spec.file_name().unwrap().to_string_lossy()
                    ));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "shipped examples must not demonstrate a hollow gate:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn the_first_run_example_is_real_and_refuses_unresolved_evidence() {
    // The README, START_HERE, install.sh and the bundle all point a newcomer at
    // this one spec. It has to exist, run from a bare checkout, and be about a
    // real board rather than a fixture.
    let spec = examples_dir().join("watchy.toml");
    assert!(spec.exists(), "the documented first-run spec must ship");

    let board = examples_dir().join("boards/watchy.kicad_pcb");
    assert!(
        board.exists(),
        "and its board must be in the tree, not fetched"
    );
    let text = std::fs::read_to_string(&board).expect("read board");
    let footprints = text.matches("(footprint ").count();
    let segments = text.matches("(segment").count();
    assert!(
        footprints >= 20 && segments >= 100,
        "a first impression needs a real board: {footprints} footprints, {segments} copper segments"
    );

    let out = Command::new(env!("CARGO_BIN_EXE_hauksbee-ci"))
        .arg("run")
        .arg(&spec)
        .output()
        .expect("run hauksbee-ci");
    assert_eq!(
        out.status.code(),
        Some(hauksbee_engine::result::EXIT_INVALID_FOR_ANALYSIS),
        "the real-board first run must refuse unresolved model evidence:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("2/2 assertion(s) evaluated") && stdout.contains("INVALID"),
        "the checks still execute, but the report must lead with INVALID, not with \
         the word 'passed' a skimming reader keeps:\n{stdout}"
    );
}
