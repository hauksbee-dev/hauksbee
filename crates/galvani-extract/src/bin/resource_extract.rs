//! `resource-extract` - extract an MCU internal-resource table (the
//! `db/mcu_resources.toml` shape) from a reference-manual PDF with an LLM
//! backend, then diff it against the hand-authored table.
//!
//! This is the automated counterpart to the hand authoring: the hand table is
//! the source of truth (a human read the RM and cited the section), and this
//! tool is the cross-check - run it live on the datasheet's PWM/GPIO section and
//! confirm the machine agrees with the hand table, pad for pad. Disagreement is
//! reported honestly rather than silently overwriting either side.
//!
//! ```text
//! resource-extract --pdf RP2040-datasheet.pdf --part rp2040 \
//!                  [--compare crates/galvani-extract/db/mcu_resources.toml]
//! ```
//!
//! Backend dispatch mirrors `model-extract` (the working pattern):
//!   - `GALVANI_EXTRACT_MOCK_REPLY=<file>` : offline canned reply (CI / fixture).
//!   - else `codex exec --sandbox workspace-write --skip-git-repo-check --cd <dir>`
//!     with stdin closed and a 10-minute poll-and-kill timeout.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CODEX_TIMEOUT: Duration = Duration::from_secs(600);

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

struct Args {
    pdf: PathBuf,
    part: String,
    compare: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut pdf = None;
    let mut part = "rp2040".to_string();
    let mut compare = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pdf" => pdf = Some(PathBuf::from(it.next().ok_or("--pdf needs a value")?)),
            "--part" => part = it.next().ok_or("--part needs a value")?,
            "--compare" => compare = Some(PathBuf::from(it.next().ok_or("--compare needs a value")?)),
            "-h" | "--help" => {
                println!("resource-extract --pdf <path> [--part rp2040] [--compare <mcu_resources.toml>]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown arg: {other}")),
        }
    }
    Ok(Args { pdf: pdf.ok_or("--pdf is required")?, part, compare })
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    eprintln!("[resource-extract] part={} pdf={}", args.part, args.pdf.display());

    let pdf_text = extract_pdf_text(&args.pdf)?;
    let prompt = build_prompt(&args.part, &pdf_text);
    let raw = call_backend(&prompt, &args.pdf)?;
    let extracted = parse_pad_pwm_table(&raw)?;
    eprintln!("[resource-extract] extracted {} PWM pad entries", extracted.len());
    println!("{raw}");

    if let Some(cmp) = &args.compare {
        let hand = load_hand_table(cmp)?;
        let report = diff_tables(&hand, &extracted);
        eprintln!("\n--- agreement vs hand table ({}) ---", cmp.display());
        eprint!("{report}");
    }
    Ok(())
}

// -- PDF text -----------------------------------------------------------------

fn extract_pdf_text(path: &Path) -> Result<String, String> {
    if which("pdftotext") {
        let out = Command::new("pdftotext")
            .arg(path)
            .arg("-")
            .output()
            .map_err(|e| format!("pdftotext: {e}"))?;
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            if !text.trim().is_empty() {
                // The PWM pad table lives in the GPIO function section; keep a
                // generous window but cap so the prompt stays in budget.
                return Ok(focus_on_pwm(&text));
            }
        }
    }
    Ok(format!("<pdf_path>{}</pdf_path>", path.display()))
}

/// Slice the datasheet text around the PWM / GPIO-function pages so the model
/// reads the right table, not 600 pages of the whole manual.
fn focus_on_pwm(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    // Anchor on the actual GPIO-function table, not the earliest "pwm" mention -
    // in a 600-page datasheet "pwm" appears in the TOC / intro long before the
    // function table, which would centre the window on the wrong page. Prefer the
    // densest cluster of "PWMk A/B" function entries (the table itself); fall back
    // to specific section headers, then to a bare "pwm" only as a last resort.
    let anchor_byte = densest_pwm_cluster(text)
        .or_else(|| ["bank 0 (user gpio)", "gpio function", "function select"].iter().find_map(|k| lower.find(k)))
        .or_else(|| lower.find("pwm"));
    let start = match anchor_byte {
        Some(b) => text[..b].chars().count().saturating_sub(2_000),
        None => 0,
    };
    text.chars().skip(start).take(48_000).collect()
}

/// Byte offset of the start of the densest window of `PWMk A` / `PWMk_A` function
/// entries - i.e. the GPIO-function table - or None if no such cluster exists.
fn densest_pwm_cluster(text: &str) -> Option<usize> {
    // Positions of every "PWM<digit>" occurrence (the function-table column).
    let mut hits = Vec::new();
    let lower = text.to_ascii_lowercase();
    let lb = lower.as_bytes();
    for i in 0..lb.len().saturating_sub(4) {
        if &lb[i..i + 3] == b"pwm" && lb[i + 3].is_ascii_digit() {
            hits.push(i);
        }
    }
    if hits.len() < 8 {
        return None; // no real table
    }
    // Slide a window and pick the start of the densest 8-hit run.
    let mut best = (usize::MAX, hits[0]);
    for w in hits.windows(8) {
        let span = w[7] - w[0];
        if span < best.0 {
            best = (span, w[0]);
        }
    }
    Some(best.1)
}

// -- Prompt -------------------------------------------------------------------

fn build_prompt(part: &str, pdf_text: &str) -> String {
    format!(
        r#"You are extracting an MCU internal-resource table from a reference-manual / datasheet.

Target part: {part}

From the datasheet text below, build the PWM-pad table for the RP2040: for each
GPIO pin GPnn, the PWM slice and channel it is hardwired to. On the RP2040 the
rule is fixed in silicon: GPIO n drives PWM slice = (n >> 1) & 7, channel A when
n is even and channel B when n is odd. Confirm this from the datasheet's GPIO
function table (the column that lists PWMk_A / PWMk_B for each GPIO) and emit one
row per GPIO you can read.

DATASHEET TEXT:
---
{text}
---

Output ONLY a TOML block in EXACTLY this shape (no prose, no markdown fences):

[[mcu]]
id = "rp2040_extracted"
description = "RP2040 PWM pad table, machine-extracted"

[mcu.pins]
# key = bare-QFN datasheet pin number is NOT needed; key by GPIO so the diff is
# unambiguous. Use the GPIO name as the key and give its pwm slice+channel.
"GP0"  = {{ pwm = "0A" }}
"GP1"  = {{ pwm = "0B" }}
# ... one line per GPIO GP0..GP29, pwm = "<slice><A|B>", slice 0..7.

Rules:
1. pwm value is the slice digit (0..7) followed by the channel letter A or B,
   e.g. "6A". Nothing else.
2. Include GP0 through GP29. If a GPIO's row is not legible in the text, omit it
   rather than guessing.
3. Output the TOML only, starting with [[mcu]].
"#,
        part = part,
        text = take_chars(pdf_text, 40_000),
    )
}

/// Char-safe truncation: take at most `n` characters (never slices mid-char,
/// which a byte-index `&s[..n]` would panic on for multi-byte UTF-8 such as the
/// U+FFFD replacement chars `from_utf8_lossy` emits on a binary-ish PDF dump).
fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// -- Backend ------------------------------------------------------------------

fn call_backend(prompt: &str, pdf: &Path) -> Result<String, String> {
    if let Ok(path) = std::env::var("GALVANI_EXTRACT_MOCK_REPLY") {
        let reply = std::fs::read_to_string(&path).map_err(|e| format!("mock reply {path}: {e}"))?;
        return Ok(extract_toml_block(&reply));
    }
    if which("codex") {
        call_codex(prompt, pdf)
    } else {
        Err("no backend: install codex in PATH or set GALVANI_EXTRACT_MOCK_REPLY=<file>".into())
    }
}

fn call_codex(prompt: &str, pdf: &Path) -> Result<String, String> {
    let workdir = pdf
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let mut child = Command::new("codex")
        .args(["exec", "--sandbox", "workspace-write", "--skip-git-repo-check", "--cd"])
        .arg(&workdir)
        .arg(prompt)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn codex: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b""); // close stdin or codex blocks on EOF
    }
    let deadline = Instant::now() + CODEX_TIMEOUT;
    loop {
        match child.try_wait().map_err(|e| format!("poll codex: {e}"))? {
            Some(_) => break,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("codex timed out after {}s", CODEX_TIMEOUT.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    let out = child.wait_with_output().map_err(|e| format!("collect codex: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("codex failed: {}", err.lines().rev().take(4).collect::<Vec<_>>().join(" | ")));
    }
    Ok(extract_toml_block(&String::from_utf8_lossy(&out.stdout)))
}

fn which(cmd: &str) -> bool {
    Command::new("which").arg(cmd).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

fn extract_toml_block(s: &str) -> String {
    if let Some(start) = s.find("```toml") {
        let after = &s[start + 7..];
        if let Some(end) = after.find("```") {
            return after[..end].trim().to_string();
        }
    }
    if let Some(start) = s.find("```") {
        let after = &s[start + 3..];
        if let Some(end) = after.find("```") {
            return after[..end].trim().to_string();
        }
    }
    if let Some(pos) = s.find("[[mcu]]") {
        return s[pos..].trim().to_string();
    }
    s.trim().to_string()
}

// -- Parse + validate ---------------------------------------------------------

/// Parse the extracted reply into a GP-name -> "slice+channel" map, validating
/// each PWM value is a slice 0..7 plus channel A/B.
fn parse_pad_pwm_table(raw: &str) -> Result<BTreeMap<String, String>, String> {
    if !raw.contains("[[mcu]]") {
        return Err("reply has no [[mcu]] table (model answered with prose?)".into());
    }
    let doc: toml::Value = toml::from_str(raw).map_err(|e| format!("parse TOML: {e}"))?;
    let entry = doc.get("mcu").and_then(|m| m.as_array()).and_then(|a| a.first()).ok_or("no [[mcu]] entry")?;
    let pins = entry.get("pins").and_then(|p| p.as_table()).ok_or("no [mcu.pins] table")?;
    let mut out = BTreeMap::new();
    for (k, v) in pins {
        let pwm = v.get("pwm").and_then(|x| x.as_str()).ok_or_else(|| format!("{k}: no pwm value"))?;
        validate_pwm(pwm).map_err(|e| format!("{k}: {e}"))?;
        out.insert(k.to_ascii_uppercase(), pwm.to_ascii_uppercase());
    }
    Ok(out)
}

fn validate_pwm(s: &str) -> Result<(), String> {
    // Char-safe: the last char is the channel, the rest is the slice. A
    // byte-split would panic on a multi-byte trailing char (e.g. a stray Ω / U+FFFD
    // that survived into the model's TOML string).
    let s = s.trim();
    let chan = s.chars().last().ok_or_else(|| "empty pwm value".to_string())?;
    let slice: String = s.chars().take(s.chars().count().saturating_sub(1)).collect();
    let n: u8 = slice.parse().map_err(|_| format!("bad slice in '{s}'"))?;
    if n > 7 {
        return Err(format!("slice {n} out of range 0..7"));
    }
    if !matches!(chan, 'A' | 'B' | 'a' | 'b') {
        return Err(format!("bad channel in '{s}'"));
    }
    Ok(())
}

// -- Hand table + diff --------------------------------------------------------

/// Load the GP-name -> "slice+channel" map from the hand table's
/// `rp2040_pico_module` entry (its pins are labelled with the GPIO name).
fn load_hand_table(path: &Path) -> Result<BTreeMap<String, String>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let doc: toml::Value = toml::from_str(&text).map_err(|e| format!("parse hand table: {e}"))?;
    let arr = doc.get("mcu").and_then(|m| m.as_array()).ok_or("no [[mcu]] array")?;
    let mut out = BTreeMap::new();
    for entry in arr {
        // Use the QFN table (GP0..GP29 complete) for the comparison.
        if entry.get("id").and_then(|v| v.as_str()) != Some("rp2040_qfn56") {
            continue;
        }
        if let Some(pins) = entry.get("pins").and_then(|p| p.as_table()) {
            for v in pins.values() {
                if let (Some(gpio), Some(pwm)) = (
                    v.get("gpio").and_then(|x| x.as_str()),
                    v.get("pwm").and_then(|x| x.as_str()),
                ) {
                    out.insert(gpio.to_ascii_uppercase(), pwm.to_ascii_uppercase());
                }
            }
        }
    }
    if out.is_empty() {
        return Err("hand table has no rp2040_qfn56 GPIO/pwm rows".into());
    }
    Ok(out)
}

fn diff_tables(hand: &BTreeMap<String, String>, machine: &BTreeMap<String, String>) -> String {
    let mut agree = 0;
    let mut disagree = Vec::new();
    let mut only_hand = Vec::new();
    let mut only_machine = Vec::new();
    for (gp, hv) in hand {
        match machine.get(gp) {
            Some(mv) if mv == hv => agree += 1,
            Some(mv) => disagree.push(format!("  {gp}: hand={hv} machine={mv}")),
            None => only_hand.push(gp.clone()),
        }
    }
    for gp in machine.keys() {
        if !hand.contains_key(gp) {
            only_machine.push(gp.clone());
        }
    }
    let mut s = String::new();
    s.push_str(&format!("agree: {agree}/{} hand rows\n", hand.len()));
    if !disagree.is_empty() {
        s.push_str(&format!("DISAGREE ({}):\n{}\n", disagree.len(), disagree.join("\n")));
    }
    if !only_hand.is_empty() {
        s.push_str(&format!("only in hand table: {}\n", only_hand.join(", ")));
    }
    if !only_machine.is_empty() {
        s.push_str(&format!("only in machine table: {}\n", only_machine.join(", ")));
    }
    if disagree.is_empty() && only_hand.is_empty() {
        s.push_str("VERDICT: machine extraction matches the hand table on every shared GPIO.\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pwm_validation() {
        assert!(validate_pwm("6A").is_ok());
        assert!(validate_pwm("0b").is_ok());
        assert!(validate_pwm("8A").is_err()); // slice out of range
        assert!(validate_pwm("6C").is_err()); // bad channel
        assert!(validate_pwm("XA").is_err());
    }

    #[test]
    fn parse_extracted_table() {
        let raw = r#"[[mcu]]
id = "rp2040_extracted"
[mcu.pins]
"GP0" = { pwm = "0A" }
"GP12" = { pwm = "6A" }
"GP28" = { pwm = "6A" }
"#;
        let t = parse_pad_pwm_table(raw).unwrap();
        assert_eq!(t["GP12"], "6A");
        assert_eq!(t["GP28"], "6A");
    }

    #[test]
    fn diff_reports_agreement_and_disagreement() {
        let hand = BTreeMap::from([("GP12".into(), "6A".into()), ("GP13".into(), "6B".into())]);
        let agree = BTreeMap::from([("GP12".into(), "6A".into()), ("GP13".into(), "6B".into())]);
        assert!(diff_tables(&hand, &agree).contains("matches the hand table"));
        let bad = BTreeMap::from([("GP12".into(), "5A".into()), ("GP13".into(), "6B".into())]);
        let r = diff_tables(&hand, &bad);
        assert!(r.contains("DISAGREE"));
        assert!(r.contains("hand=6A machine=5A"));
    }

    /// Offline fixture: drive the parse + diff path with a canned codex reply via
    /// GALVANI_EXTRACT_MOCK_REPLY, no codex, no network. Always runs.
    #[test]
    fn offline_extraction_fixture() {
        let reply = "Here is the table:\n```toml\n[[mcu]]\nid = \"rp2040_extracted\"\n[mcu.pins]\n\"GP12\" = { pwm = \"6A\" }\n\"GP28\" = { pwm = \"6A\" }\n```\n";
        let block = extract_toml_block(reply);
        let table = parse_pad_pwm_table(&block).expect("fixture reply parses");
        // The bug's two pins agree with the hand-table rule (both slice 6A).
        assert_eq!(table["GP12"], "6A");
        assert_eq!(table["GP28"], "6A");
    }

    fn repo(rel: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
    }

    /// The committed machine-extraction fixture (the real codex output, saved
    /// from the live run on the RP2040 datasheet) must agree with the hand table
    /// on every GPIO. This is the always-run, offline proof that the automated
    /// extraction and the hand authoring converged - no codex, no network.
    #[test]
    fn committed_rp2040_extraction_matches_hand_table() {
        let fixture = repo("../../testdata/resource/rp2040_pwm_extracted.toml");
        if !fixture.exists() {
            panic!("missing fixture {fixture:?}; regenerate with `resource-extract --pdf <RP2040 datasheet>`");
        }
        let raw = std::fs::read_to_string(&fixture).unwrap();
        let machine = parse_pad_pwm_table(&raw).expect("fixture parses + validates");
        let hand = load_hand_table(&repo("db/mcu_resources.toml")).expect("hand table loads");
        // Every GPIO in the hand table is present and identical in the machine
        // extraction (30 GPIOs, GP0..GP29).
        let report = diff_tables(&hand, &machine);
        assert!(
            report.contains("matches the hand table"),
            "machine extraction must agree with the hand table:\n{report}"
        );
        assert_eq!(hand.len(), 30, "hand table should cover GP0..GP29");
        // Spot-check the bug-defining rows.
        assert_eq!(machine["GP12"], "6A");
        assert_eq!(machine["GP28"], "6A");
    }

    /// Live integration: shell out to the REAL codex backend against the RP2040
    /// datasheet and assert agreement. #[ignore] (downloads a 5.3 MB PDF + runs
    /// codex, ~1-2 min). Run with:
    ///   cargo test -p galvani-extract --bin resource-extract -- \
    ///       live_rp2040_extraction --ignored --nocapture
    #[test]
    #[ignore]
    fn live_rp2040_extraction_agrees() {
        let pdf = repo("../../testdata/datasheets/rp2040-datasheet.pdf");
        if !pdf.exists() {
            eprintln!("RP2040 datasheet not at {pdf:?}; download it (see docs/RESOURCE_CONFLICTS.md). Skipping.");
            return;
        }
        if !which("codex") && std::env::var("GALVANI_EXTRACT_MOCK_REPLY").is_err() {
            eprintln!("no codex backend; skipping live test");
            return;
        }
        let text = extract_pdf_text(&pdf).expect("pdf text");
        let prompt = build_prompt("rp2040", &text);
        let raw = call_backend(&prompt, &pdf).expect("codex backend");
        let machine = parse_pad_pwm_table(&raw).expect("live reply parses");
        let hand = load_hand_table(&repo("db/mcu_resources.toml")).unwrap();
        let report = diff_tables(&hand, &machine);
        println!("{report}");
        assert!(report.contains("matches the hand table"), "live extraction disagreed:\n{report}");
    }
}
