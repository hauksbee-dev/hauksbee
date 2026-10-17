//! The report depends on the board, not on the order its file happens to list
//! things in.
//!
//! The same design saved by two tools, or re-exported after an edit, lists its
//! components, signals and nets in a different order. The `--json` document
//! (finding text, the `group` hash a waiver is keyed on, evidence and
//! assumption lists, clearance minimum gaps and locations) must come out the
//! same either way. Each test builds a permuted copy of an existing board and
//! compares the two documents byte for byte once the input path, the input
//! digest and timing fields are set aside (those describe the file, not the
//! board).

use crate::support::{board, run_board, stderr, stdout};
use serde_json::Value;
use std::path::Path;

/// `(`-balanced child spans `(start, end)` of the first list in `node`.
fn child_spans(node: &str) -> Vec<(usize, usize)> {
    let bytes = node.as_bytes();
    let mut spans = Vec::new();
    let (mut depth, mut start, mut in_str, mut escaped) = (0usize, 0usize, false, false);
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'(' => {
                depth += 1;
                if depth == 2 {
                    start = i;
                }
            }
            b')' => {
                if depth == 2 {
                    spans.push((start, i + 1));
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }
    spans
}

/// The list head of `node` (`"footprint"` for `(footprint "R_0603" ...)`).
fn head(node: &str) -> &str {
    let rest = node.trim_start_matches('(');
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .unwrap_or(rest.len());
    &rest[..end]
}

/// `node` with, for each of `kinds`, its same-kind children listed in the
/// reverse order. Text between the children stays where it was, so a bare atom
/// (`locked`, `placed`) keeps its place. A footprint's pads are reversed too.
fn permute(node: &str, kinds: &[&str]) -> String {
    let spans = child_spans(node);
    let mut texts: Vec<String> = spans
        .iter()
        .map(|&(s, e)| {
            let child = &node[s..e];
            if matches!(head(child), "footprint" | "module") {
                permute(child, &["pad"])
            } else {
                child.to_string()
            }
        })
        .collect();
    for kind in kinds {
        let slots: Vec<usize> = (0..spans.len())
            .filter(|&i| head(&node[spans[i].0..spans[i].1]) == *kind)
            .collect();
        let reversed: Vec<String> = slots.iter().rev().map(|&i| texts[i].clone()).collect();
        for (&slot, text) in slots.iter().zip(reversed) {
            texts[slot] = text;
        }
    }
    let mut out = String::with_capacity(node.len());
    let mut at = 0;
    for (&(s, e), text) in spans.iter().zip(&texts) {
        out.push_str(&node[at..s]);
        out.push_str(text);
        at = e;
    }
    out.push_str(&node[at..]);
    out
}

/// A KiCad board with its footprints, tracks, vias, zones and net table listed
/// in the opposite order. Net ids stay with their names, so it is the same
/// board.
fn permuted_kicad(text: &str) -> String {
    permute(
        text,
        &[
            "footprint",
            "module",
            "segment",
            "arc",
            "via",
            "zone",
            "net",
            "gr_line",
        ],
    )
}

/// Run the report with the given flags on a copy of `bytes` named `name` in a
/// fresh directory and return its normalised JSON document and exit code.
fn report(name: &str, bytes: &[u8], flags: &[&str]) -> (String, Option<i32>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    let out = run_board(&path, flags);
    let mut doc: Value = serde_json::from_str(&stdout(&out))
        .unwrap_or_else(|e| panic!("{name}: JSON ({e}): {}", stderr(&out)));
    let roots = [
        dir.path().to_string_lossy().into_owned(),
        dir.path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf())
            .to_string_lossy()
            .into_owned(),
    ];
    normalise(&mut doc, &roots);
    (
        serde_json::to_string_pretty(&doc).unwrap(),
        out.status.code(),
    )
}

/// Drop what describes the input file or the run rather than the board: the
/// scratch directory, input digests and timing fields.
fn normalise(value: &mut Value, roots: &[String]) {
    match value {
        Value::Object(map) => {
            map.retain(|key, _| {
                !(key == "sha256"
                    || key.contains("elapsed")
                    || key.ends_with("_ms")
                    || key.ends_with("_secs"))
            });
            for v in map.values_mut() {
                normalise(v, roots);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| normalise(v, roots)),
        Value::String(s) => {
            for root in roots {
                if s.contains(root.as_str()) {
                    *s = s.replace(root.as_str(), "<dir>");
                }
            }
            if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
                *s = "<sha256>".to_string();
            }
        }
        _ => {}
    }
}

fn assert_same(label: &str, name: &str, original: &str, permuted: &str, flags: &[&str]) {
    let (a, code_a) = report(name, original.as_bytes(), flags);
    let (b, code_b) = report(name, permuted.as_bytes(), flags);
    assert_eq!(code_a, code_b, "{label} {flags:?}: exit code");
    if a != b {
        let first = a
            .lines()
            .zip(b.lines())
            .position(|(x, y)| x != y)
            .unwrap_or(0);
        let show = |s: &str| {
            s.lines()
                .skip(first.saturating_sub(3))
                .take(8)
                .collect::<Vec<_>>()
                .join("\n")
        };
        panic!(
            "{label} {flags:?}: the report changed with the file's order (first difference at line {first})\n--- original\n{}\n--- permuted\n{}",
            show(&a),
            show(&b)
        );
    }
}

fn read(rel: &str) -> String {
    let path = board(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn kicad_reports_do_not_depend_on_file_order() {
    for rel in [
        "../../testdata/boards/pic_programmer.kicad_pcb",
        "../../testdata/boards/stm32_i2c_thermostat.kicad_pcb",
    ] {
        let original = read(rel);
        let permuted = permuted_kicad(&original);
        assert_ne!(
            original, permuted,
            "{rel}: the permutation must change the file"
        );
        let name = Path::new(rel).file_name().unwrap().to_str().unwrap();
        for flags in [&["--check", "--json"][..], &["--drc", "--json"]] {
            assert_same(rel, name, &original, &permuted, flags);
        }
    }
}

/// The committed `declared.brd` fixture grown to four parts and five signals.
/// The tracks sit clearly inside the 6 mil (0.1524 mm) clearance rule, at
/// 0.05 mm and 0.07 mm gaps: the DRC forgives the last 5 micrometres under a
/// rule as routing-to-rule, so a gap within that would raise nothing. Clearance
/// pairs, a short and the signal numbering (an Eagle net id is its position in
/// `<signals>`) all take part. `reversed` lists `<elements>` and `<signals>`
/// back to front.
fn eagle_board(reversed: bool) -> String {
    let base = include_str!("../../hauksbee-extract/tests/fixtures/eagle_ties/declared.brd");
    let head = base.split("<elements>").next().unwrap();
    let tail = base.rsplit("</signals>").next().unwrap();
    let mut elements = [
        r#"<element name="R1" library="device" package="R0603" value="10k" x="20" y="20"/>"#,
        r#"<element name="R2" library="device" package="R0603" value="1k" x="20" y="24"/>"#,
        r#"<element name="R3" library="device" package="R0603" value="4.7k" x="20" y="28"/>"#,
        r#"<element name="R10" library="device" package="R0603" value="100k" x="20" y="32"/>"#,
    ];
    let mut signals = [
        r#"<signal name="GND"><contactref element="R1" pad="1"/><contactref element="R2" pad="1"/><via x="5" y="0" extent="1-16" drill="0.3" diameter="0.8"/></signal>"#,
        r#"<signal name="AGND"><contactref element="R1" pad="2"/><wire x1="5" y1="-5" x2="5" y2="5" width="0.5" layer="1"/><wire x1="5" y1="-5" x2="5" y2="5" width="0.5" layer="16"/></signal>"#,
        r#"<signal name="VCC"><contactref element="R2" pad="2"/><contactref element="R3" pad="1"/><wire x1="10" y1="-5" x2="10" y2="5" width="0.5" layer="1"/></signal>"#,
        r#"<signal name="SIG"><contactref element="R3" pad="2"/><contactref element="R10" pad="1"/><wire x1="10.55" y1="-5" x2="10.55" y2="5" width="0.5" layer="1"/></signal>"#,
        r#"<signal name="AREF"><contactref element="R10" pad="2"/><wire x1="11.12" y1="-5" x2="11.12" y2="5" width="0.5" layer="1"/></signal>"#,
    ];
    if reversed {
        elements.reverse();
        signals.reverse();
    }
    format!(
        "{head}<elements>{}</elements><signals>\n{}\n</signals>{tail}",
        elements.join(""),
        signals.join("\n")
    )
}

#[test]
fn eagle_reports_do_not_depend_on_file_order() {
    let (original, permuted) = (eagle_board(false), eagle_board(true));
    assert_ne!(original, permuted);
    // The fixture must exercise what the test is about: clearance pairs.
    let (doc, _) = report("order.brd", original.as_bytes(), &["--drc", "--json"]);
    assert!(
        doc.matches("\"kind\": \"clearance\"").count() >= 2,
        "the board must raise several clearance findings: {doc}"
    );
    for flags in [&["--check", "--json"][..], &["--drc", "--json"]] {
        assert_same("eagle", "order.brd", &original, &permuted, flags);
    }
}
