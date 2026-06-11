//! `model-extract` — datasheet extraction binary.
//!
//! Extracts a simulation model entry from a PDF datasheet using an LLM backend
//! and validates it against the galvani-models schema.
//!
//! # Usage
//!
//! ```text
//! model-extract --pdf path/to/datasheet.pdf \
//!               --part BCM847BS \
//!               --kind bjt_npn \
//!               [--out-dir ~/.galvani/models/]
//! ```
//!
//! # Backends (in priority order)
//!
//! 1. **codex** (default): shells out to `codex exec --full-auto` with a
//!    carefully constructed prompt. Requires `codex` in PATH.
//! 2. **API** (optional): if `GALVANI_LLM_API_KEY` and `GALVANI_LLM_MODEL`
//!    are set, calls an OpenAI-compatible chat completions endpoint via
//!    `GALVANI_LLM_BASE_URL` (defaults to `https://api.openai.com/v1`).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

fn main() -> Result<()> {
    let args = parse_args()?;

    eprintln!(
        "[model-extract] part={} kind={} pdf={}",
        args.part,
        args.kind_str,
        args.pdf.display()
    );

    // 1. Extract text from PDF
    let pdf_text = extract_pdf_text(&args.pdf)?;

    // 2. Build the extraction prompt
    let prompt = build_prompt(&args.part, &args.kind_str, &pdf_text);

    // 3. Call backend and get raw TOML reply
    let raw = call_backend(&prompt, &args)?;

    // 4. Parse and validate
    let entry = parse_and_validate_reply(&raw, &args.part, &args.kind_str)?;

    // 5. Write to output directory
    let default_dir = default_out_dir();
    let out_dir: &Path = args.out_dir.as_deref().unwrap_or(&default_dir);
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("creating output directory {}", out_dir.display()))?;
    let out_path = out_dir.join(format!("{}.toml", sanitise_filename(&args.part)));
    std::fs::write(&out_path, &raw)
        .with_context(|| format!("writing output to {}", out_path.display()))?;

    println!("Written: {}", out_path.display());
    println!("{}", entry.id);
    Ok(())
}

// ── Argument parsing ──────────────────────────────────────────────────────────

struct Args {
    pdf: PathBuf,
    part: String,
    kind_str: String,
    out_dir: Option<PathBuf>,
    /// Retry count for LLM calls (default 1)
    retries: usize,
}

fn parse_args() -> Result<Args> {
    let mut args = std::env::args().skip(1);
    let mut pdf: Option<PathBuf> = None;
    let mut part: Option<String> = None;
    let mut kind_str = "bjt_npn".to_string();
    let mut out_dir: Option<PathBuf> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--pdf" => {
                pdf = Some(args.next().context("--pdf requires a value")?.into());
            }
            "--part" => {
                part = Some(args.next().context("--part requires a value")?);
            }
            "--kind" => {
                kind_str = args.next().context("--kind requires a value")?;
            }
            "--out-dir" => {
                out_dir = Some(args.next().context("--out-dir requires a value")?.into());
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => bail!("unknown argument: {}", other),
        }
    }

    Ok(Args {
        pdf: pdf.context("--pdf is required")?,
        part: part.context("--part is required")?,
        kind_str,
        out_dir,
        retries: 1,
    })
}

fn print_help() {
    println!(
        r"model-extract — extract a simulation model from a PDF datasheet

USAGE:
    model-extract --pdf <path> --part <part_number> [OPTIONS]

OPTIONS:
    --pdf <path>          Path to (or URL of) the datasheet PDF
    --part <name>         Manufacturer part number (e.g. BCM847BS)
    --kind <kind>         Component kind hint (default: bjt_npn)
                          passive|diode|bjt_npn|bjt_pnp|nmos|pmos|
                          vreg|opamp|comparator|analog_switch|digital|
                          dac|adc|shift_register|mcu|connector|ignore
    --out-dir <dir>       Output directory (default: ~/.galvani/models/)

ENVIRONMENT:
    GALVANI_LLM_API_KEY   API key for OpenAI-compatible backend
    GALVANI_LLM_MODEL     Model ID for API backend (e.g. gpt-4o)
    GALVANI_LLM_BASE_URL  Base URL (default: https://api.openai.com/v1)
"
    );
}

// ── PDF text extraction ───────────────────────────────────────────────────────

fn extract_pdf_text(path: &Path) -> Result<String> {
    // Try pdftotext first
    if which("pdftotext") {
        let output = Command::new("pdftotext")
            .arg(path)
            .arg("-") // output to stdout
            .output()
            .context("running pdftotext")?;
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout).into_owned();
            if !text.trim().is_empty() {
                return Ok(truncate_to_chars(&text, 60_000));
            }
        }
    }

    // Fallback: read raw bytes and let the LLM handle it (works with codex)
    // Return a placeholder that tells the prompt to read the PDF directly
    eprintln!(
        "[model-extract] pdftotext not found; LLM backend will read the PDF directly"
    );
    Ok(format!(
        "<pdf_path>{}</pdf_path>\n\
         [Note: pdftotext not available. The LLM should read the PDF at the path above directly.]",
        path.display()
    ))
}

fn which(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn truncate_to_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

// ── Prompt construction ───────────────────────────────────────────────────────

fn build_prompt(part: &str, kind: &str, pdf_text: &str) -> String {
    format!(
        r#"You are a SPICE model extraction assistant. Your task is to extract a simulation
model entry from the datasheet text below for the component: {part}

Component kind: {kind}

DATASHEET TEXT (truncated):
---
{pdf_text}
---

Produce a TOML model entry that exactly conforms to the galvani-models schema.
The entry must use [[models]] array syntax and include:
- id: lowercase part number (e.g. "{part_lower}")
- kind: "{kind}"
- description: brief human-readable description
- [models.match] section with at least value_re or mpn_re
- [models.params] section with all required numeric params for the kind
- [models.pins] section mapping pad numbers to pin roles

For kind="{kind}", the required params are:
{required_params}

IMPORTANT RULES:
1. Add a comment on each param line citing where in the datasheet you found the value,
   e.g.: `# Source: Table 6.3, typ column`
2. Use only values explicitly stated in the datasheet — do NOT guess.
3. If a required param is missing from the datasheet, use a conservative typical
   value for the part family and add a comment: `# estimated - not in datasheet`
4. Output ONLY the TOML block, starting with [[models]] — no prose, no markdown fences.
5. Values must be within these physical bounds:
   - is: 1e-20 to 1e-3
   - bf/beta: 1 to 2000
   - n (emission): 0.5 to 3.0
   - vaf (Early V): 1 to 500
   - vto (MOSFET threshold): -10 to +10
   - kp (transconductance): 1e-6 to 1.0
   - ron (switch): 0.01 to 10000
   - roff (switch): 1e3 to 1e12

OUTPUT (TOML only):
"#,
        part = part,
        part_lower = part.to_lowercase(),
        kind = kind,
        pdf_text = &pdf_text[..pdf_text.len().min(40_000)],
        required_params = required_params_for_kind(kind),
    )
}

fn required_params_for_kind(kind: &str) -> &'static str {
    match kind {
        "diode"              => "is, n, rs  (also cjo, vj, m, bv if available)",
        "bjt_npn" | "bjt_pnp" => "is, bf, nf, vaf, br  (also rb, rc, re, cje, cjc, tf)",
        "nmos" | "pmos"     => "vto, kp  (also lambda, rd, rs, cgd, cgs)",
        "vreg"               => "vout, dropout_v, iq_a",
        "opamp"              => "gain, rail_lo, rail_hi",
        "comparator"         => "out_lo, out_hi, hysteresis, tpd_s",
        "analog_switch"      => "ron, roff, vth",
        "digital" | "shift_register" => "voh, vol, vih, vil, tpd_s, supply_pin, gnd_pin",
        "dac"                => "bits, vref_int, i2c_addr (or spi mode)",
        "mcu"                => "backend (e.g. simavr:atmega328p)",
        _                    => "(see db/README.md for kind-specific requirements)",
    }
}

// ── Backend dispatch ──────────────────────────────────────────────────────────

fn call_backend(prompt: &str, args: &Args) -> Result<String> {
    // Check for API key first
    if std::env::var("GALVANI_LLM_API_KEY").is_ok() {
        return call_api_backend(prompt, args);
    }

    // Default: codex
    if which("codex") {
        call_codex_backend(prompt, args)
    } else {
        bail!(
            "No LLM backend available. Install codex in PATH, or set \
             GALVANI_LLM_API_KEY + GALVANI_LLM_MODEL environment variables."
        )
    }
}

/// Call `codex exec --full-auto` with the prompt written to a temp file.
fn call_codex_backend(prompt: &str, args: &Args) -> Result<String> {
    // Write the prompt to a temp file so codex can read the PDF path from it
    let tmp = std::env::temp_dir().join("galvani_extract_prompt.txt");
    std::fs::write(&tmp, prompt).context("writing codex prompt")?;

    let codex_task = format!(
        "Read the attached prompt file and produce ONLY the TOML model entry as described. \
         Prompt file: {}",
        tmp.display()
    );

    for attempt in 0..=args.retries {
        let output = Command::new("codex")
            .args(["exec", "--full-auto", &codex_task])
            .output()
            .context("running codex")?;

        let raw = String::from_utf8_lossy(&output.stdout).into_owned();
        let raw = extract_toml_block(&raw);

        match parse_and_validate_reply(&raw, &args.part, &args.kind_str) {
            Ok(_) => return Ok(raw),
            Err(e) if attempt < args.retries => {
                eprintln!("[model-extract] attempt {} failed: {}; retrying...", attempt + 1, e);
                // On retry, append the error to the prompt
                let retry_prompt = format!(
                    "{}\n\nPREVIOUS ATTEMPT FAILED WITH:\n{}\n\nPlease fix the issues and try again.",
                    prompt, e
                );
                std::fs::write(&tmp, &retry_prompt).ok();
                continue;
            }
            Err(e) => return Err(e),
        }
    }

    unreachable!()
}

/// Call an OpenAI-compatible API endpoint via `curl`.
fn call_api_backend(prompt: &str, args: &Args) -> Result<String> {
    let api_key = std::env::var("GALVANI_LLM_API_KEY").unwrap();
    let model = std::env::var("GALVANI_LLM_MODEL")
        .unwrap_or_else(|_| "gpt-5.3-chat-latest".to_string());
    let base_url = std::env::var("GALVANI_LLM_BASE_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    let body = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "You are a SPICE model extraction assistant. Output TOML only."},
            {"role": "user", "content": prompt}
        ],
        "max_tokens": 2048,
        "temperature": 0.0
    });

    let body_str = serde_json::to_string(&body)?;

    for attempt in 0..=args.retries {
        let output = Command::new("curl")
            .args([
                "-s",
                "-X", "POST",
                &url,
                "-H", "Content-Type: application/json",
                "-H", &format!("Authorization: Bearer {}", api_key),
                "-d", &body_str,
            ])
            .output()
            .context("running curl for API call")?;

        let resp_str = String::from_utf8_lossy(&output.stdout);
        let resp: serde_json::Value = serde_json::from_str(&resp_str)
            .context("parsing API response as JSON")?;

        let content = resp["choices"][0]["message"]["content"]
            .as_str()
            .context("missing content in API response")?
            .to_string();

        let raw = extract_toml_block(&content);

        match parse_and_validate_reply(&raw, &args.part, &args.kind_str) {
            Ok(_) => return Ok(raw),
            Err(e) if attempt < args.retries => {
                eprintln!("[model-extract] attempt {} failed: {}; retrying...", attempt + 1, e);
            }
            Err(e) => return Err(e),
        }
    }

    unreachable!()
}

// ── TOML parsing and validation ───────────────────────────────────────────────

/// Extract a TOML block from potentially prose-wrapped LLM output.
fn extract_toml_block(s: &str) -> String {
    // If the reply contains a ```toml ... ``` fence, extract the content
    if let Some(start) = s.find("```toml") {
        let after = &s[start + 7..];
        if let Some(end) = after.find("```") {
            return after[..end].trim().to_string();
        }
    }
    // If the reply contains a plain ``` ... ``` fence
    if let Some(start) = s.find("```") {
        let after = &s[start + 3..];
        if let Some(end) = after.find("```") {
            return after[..end].trim().to_string();
        }
    }
    // Otherwise return as-is, trimmed
    s.trim().to_string()
}

/// Parse raw TOML and validate the first model entry.
fn parse_and_validate_reply(
    raw: &str,
    part: &str,
    _kind_str: &str,
) -> Result<galvani_models::schema::ModelEntry> {
    let db: galvani_models::schema::DbFile =
        toml::from_str(raw).with_context(|| format!("parsing TOML reply for {}", part))?;

    let entry = db
        .models
        .into_iter()
        .next()
        .with_context(|| format!("no [[models]] entry in reply for {}", part))?;

    // Validate physical ranges
    galvani_models::validation::validate(&entry)
        .map_err(|errs| {
            let msg = errs
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            anyhow::anyhow!("validation failed: {}", msg)
        })?;

    Ok(entry)
}

fn sanitise_filename(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn default_out_dir() -> PathBuf {
    dirs_next().join(".galvani").join("models")
}

fn dirs_next() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_toml_block_from_fence() {
        let s = "Sure, here you go:\n```toml\n[[models]]\nid = \"test\"\n```\nDone.";
        let block = extract_toml_block(s);
        assert!(block.starts_with("[[models]]"), "got: {}", block);
    }

    #[test]
    fn extract_toml_block_bare() {
        let s = "[[models]]\nid = \"test\"\nkind = \"diode\"\n";
        let block = extract_toml_block(s);
        assert!(block.starts_with("[[models]]"));
    }

    #[test]
    fn prompt_contains_required_params() {
        let prompt = build_prompt("BC847", "bjt_npn", "...datasheet text...");
        assert!(prompt.contains("is, bf, nf"));
        assert!(prompt.contains("BC847"));
    }

    /// Integration test: run the extraction pipeline against the BCM847BS datasheet.
    ///
    /// This test is marked #[ignore] because it requires either `codex` in PATH
    /// or `GALVANI_LLM_API_KEY` to be set. Run it manually with:
    ///   cargo test -p galvani-models -- extract_bcm847bs --ignored --nocapture
    #[test]
    #[ignore]
    fn extract_bcm847bs_integration() {
        let pdf = PathBuf::from(
            "/Users/hauksbee-user/Tarski/Tarski-Repos/Tarski-Schematics/BCM847BS-datasheet.pdf"
        );
        assert!(pdf.exists(), "BCM847BS datasheet not found at {:?}", pdf);

        let text = extract_pdf_text(&pdf).expect("PDF text extraction failed");
        let prompt = build_prompt("BCM847BS", "bjt_npn", &text);

        // Validate prompt structure
        assert!(prompt.contains("BCM847BS"));
        assert!(prompt.contains("bjt_npn"));

        println!("Prompt length: {} chars", prompt.len());
        println!("PDF text sample: {}", &text[..text.len().min(200)]);

        // Attempt actual extraction only if codex is available
        if !which("codex") && std::env::var("GALVANI_LLM_API_KEY").is_err() {
            eprintln!("Skipping LLM call: neither codex nor GALVANI_LLM_API_KEY available");
            return;
        }

        let args = Args {
            pdf: pdf.clone(),
            part: "BCM847BS".to_string(),
            kind_str: "bjt_npn".to_string(),
            out_dir: Some(std::env::temp_dir()),
            retries: 1,
        };

        let raw = call_backend(&prompt, &args).expect("backend call failed");
        let entry = parse_and_validate_reply(&raw, "BCM847BS", "bjt_npn")
            .expect("TOML parse/validation failed");

        assert_eq!(entry.kind, galvani_models::ComponentKind::BjtNpn);
        assert!(entry.params.get_f64("is").is_some(), "is param missing");
        assert!(entry.params.get_f64("bf").is_some(), "bf param missing");
        println!("Extracted entry: {}", entry.id);
        println!("Params: {:?}", entry.params.0.keys().collect::<Vec<_>>());
    }

    /// Test the validation path with a mocked LLM reply.
    #[test]
    fn extraction_validates_mocked_reply() {
        // A well-formed reply that should pass validation
        let good_reply = r#"
[[models]]
id = "bcm847bs"
kind = "bjt_npn"
description = "BCM847BS NPN matched pair (mocked)"

[models.match]
mpn_re = "(?i)BCM847BS"
value_re = "(?i)^BCM847BS"

[models.params]
is  = 1.0e-14   # typical NPN saturation current
bf  = 150.0     # forward beta
nf  = 1.0       # emission coefficient
vaf = 80.0      # Early voltage
br  = 4.0
rb  = 10.0
rc  = 1.0
re  = 0.5

[models.pins]
"1" = "base_q1"
"2" = "emitter_q1"
"6" = "collector_q1"
"#;
        let entry = parse_and_validate_reply(good_reply, "BCM847BS", "bjt_npn")
            .expect("good reply should parse and validate");
        assert_eq!(entry.id, "bcm847bs");

        // A reply with out-of-range bf should fail
        let bad_reply = r#"
[[models]]
id = "bcm847bs_bad"
kind = "bjt_npn"
description = "bad"

[models.match]
value_re = "BCM847BS"

[models.params]
is  = 1.0e-14
bf  = 99999.0   # way out of range
nf  = 1.0
vaf = 80.0
"#;
        let err = parse_and_validate_reply(bad_reply, "BCM847BS", "bjt_npn");
        assert!(err.is_err(), "bad bf should fail validation");
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("bf") || msg.contains("validation"), "error message: {}", msg);
    }
}
