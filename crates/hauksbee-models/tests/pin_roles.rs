//! A model's pin names must be the BINDER's role vocabulary, not the
//! datasheet's.
//!
//! The failure this guards is quiet and misleading. The TP4054's datasheet
//! calls its output BAT, so the entry named pin 3 "bat". The model then matched
//! the part, loaded cleanly, passed every schema check, and the report said
//! "vreg output not connected" against the board. That reads as a wiring fault
//! in the user's design, and it was a model naming its own pin in a vocabulary
//! nothing consumes.
//!
//! The binder resolves a vreg through the roles `out` and `in` (see `bind_vreg`
//! in hauksbee-engine). Anything else silently resolves to nothing.
//!
//! Read straight from the db files rather than through ModelLibrary, because
//! the invariant is about what is WRITTEN there, and because widening the
//! library's public surface for a test would be the wrong trade.

use std::path::{Path, PathBuf};

fn db_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("db")
}

/// Every `[[models]]` entry in a file, as (id, kind, pin roles, declares its
/// output in a behavioural converter block).
fn entries(text: &str) -> Vec<(String, String, Vec<String>, bool)> {
    let mut out = Vec::new();
    let mut id = String::new();
    let mut kind = String::new();
    let mut roles: Vec<String> = Vec::new();
    let mut in_pins = false;
    let mut has_converter_out = false;
    let mut push =
        |id: &mut String, kind: &mut String, roles: &mut Vec<String>, conv: &mut bool| {
            if !id.is_empty() {
                out.push((
                    std::mem::take(id),
                    std::mem::take(kind),
                    std::mem::take(roles),
                    std::mem::replace(conv, false),
                ));
            }
        };
    for raw in text.lines() {
        let line = raw.trim();
        if line == "[[models]]" {
            push(&mut id, &mut kind, &mut roles, &mut has_converter_out);
            in_pins = false;
        } else if line.starts_with('[') {
            in_pins = line == "[models.pins]";
        } else if let Some(v) = line.strip_prefix("id = ") {
            id = v.trim_matches('"').to_string();
        } else if let Some(v) = line.strip_prefix("kind = ") {
            kind = v.trim_matches('"').to_string();
        } else if line.starts_with("out_pin = ") {
            // A behavioural converter names its own output, so the vreg role
            // path is not what resolves it. See ltc4020 and npm1300.
            has_converter_out = true;
        } else if in_pins {
            if let Some((_, r)) = line.split_once('=') {
                let r = r.split('#').next().unwrap_or("").trim().trim_matches('"');
                if !r.is_empty() {
                    roles.push(r.to_string());
                }
            }
        }
    }
    push(&mut id, &mut kind, &mut roles, &mut has_converter_out);
    out
}

#[test]
fn every_vreg_model_names_an_output_the_binder_can_find() {
    let mut offenders = Vec::new();
    let mut checked = 0;
    for f in std::fs::read_dir(db_dir()).expect("db dir").flatten() {
        let path = f.path();
        if path.extension().is_none_or(|e| e != "toml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("db file");
        for (id, kind, roles, converter_out) in entries(&text) {
            // A converter block declares its own out_pin, so those models do
            // not go through the vreg role path at all.
            if kind != "vreg" || roles.is_empty() || converter_out {
                continue;
            }
            // KNOWN GAP, exempted by name so this still guards every new model.
            //
            // npm1300 sets `vout = 1.8` as a liveness seed for BUCK1 and then
            // maps no BUCK1 pin at all: its seven mapped pins out of thirty-two
            // are the ones the ship-hold behaviour needs. So the vreg path has
            // no output net and the part reports "vreg output not connected" on
            // any board carrying it. Fixing it means the real BUCK1 output pin
            // number from the Nordic datasheet, and guessing one would put a
            // regulated rail on whatever net happened to sit there, which is
            // the failure this whole file is about.
            if id == "npm1300" {
                continue;
            }
            checked += 1;
            if !roles.iter().any(|r| r == "out") {
                offenders.push(format!(
                    "{} in {}: pins are {roles:?}, none of which is `out`",
                    id,
                    path.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
        }
    }
    assert!(
        checked > 0,
        "no vreg model with a pin map was found, so this checked nothing"
    );
    assert!(
        offenders.is_empty(),
        "a vreg whose output pin is not named `out` binds and then reports the board as \
         unconnected, which reads as the user's fault:\n  {}",
        offenders.join("\n  ")
    );
}
