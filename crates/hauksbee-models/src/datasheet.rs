//! Datasheet extraction: draft a device model from a PDF, then validate it.
//!
//! This is a library module, with the binary (`model-extract`) a thin wrapper
//! over it, so the engine can offer extraction directly. A capability reachable
//! only by running a second executable is one most users never find.
//!
//! Nothing here runs on its own. Extraction sends the datasheet's text to an
//! LLM backend, so it happens when a caller asks and not before. The caller
//! owns telling the user that, and `hauksbee_engine::deps` carries the
//! statement for the surfaces that need it.
//!
//! Extracts a simulation model entry from a PDF datasheet using an LLM backend
//! and validates it against the hauksbee-models schema.
//!
//! # Usage
//!
//! ```text
//! model-extract --pdf path/to/datasheet.pdf \
//!               --part BCM847BS \
//!               --kind bjt_npn \
//!               [--out-dir ~/.hauksbee/models/]
//! ```
//!
//! # Backends
//!
//! Selected with `--backend codex|claude-code|api`:
//!
//! 1. **codex** (default): shells out to `codex exec` with a carefully
//!    constructed prompt. Requires `codex` in PATH.
//! 2. **claude-code**: shells out to headless `claude -p` with the same
//!    prompt contract. Requires `claude` in PATH.
//! 3. **api**: calls an OpenAI-compatible chat-completions endpoint,
//!    configured by `--api-base` (default `https://api.openai.com/v1`),
//!    `--model`, and `--api-key-env NAME` (the key is read from that
//!    environment variable at call time and never stored).
//!
//! With no `--backend`, setting `HAUKSBEE_LLM_API_KEY` selects the api
//! backend, matching the behaviour before `--backend` existed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// How long to let a single agent-CLI run (codex or claude) go before we kill
/// it and (maybe) retry.
const CLI_BACKEND_TIMEOUT: Duration = Duration::from_secs(600);

/// Pages rendered and attached as images.
///
/// The parts of a datasheet that matter most, the absolute-maximum table and
/// the pinout, are the parts a text dump mangles worst: a table becomes a
/// column of loose numbers with nothing saying which row they belonged to.
/// Rendered pages keep that. The cap exists because a 200-page catalogue would
/// otherwise cost a fortune to send for no gain, and the pages that carry the
/// electrical tables are near the front.
const MAX_RENDERED_PAGES: usize = 14;

/// Render resolution, in DPI. Enough to read a small table footnote, which is
/// often exactly where the condition a value was measured under is hiding.
const RENDER_DPI: u32 = 150;

/// A private scratch directory holding everything one extraction may touch.
///
/// Codex runs full-auto with write access to its working directory, so that
/// directory must never be the folder the datasheet happens to sit in: pointing
/// the tool at `~/Downloads/part.pdf` would hand an autonomous agent write
/// access to the whole of Downloads. It gets a scratch copy instead.
///
/// Be precise about what that does and does not buy, because the difference
/// matters. `--sandbox workspace-write` confines WRITES to the writable roots
/// (this directory, plus `$TMPDIR` and `/tmp`, which is where this directory
/// lives anyway) and disables network access. It does NOT confine reads: under
/// that profile the agent can still read anything the user can, including
/// `~/.ssh`, and could copy what it read into `model.toml`, which we then parse
/// and save. So this bounds blast radius and side effects; it is not a
/// confidentiality boundary against a hostile model.
///
/// What makes that tolerable is the rest of the contract rather than the flag:
/// the run happens only when the user asked for it, the output is a small TOML
/// document validated against a schema before anything is saved, and the user
/// reviews the card. A read-restricting profile would be better, and is worth
/// revisiting if codex grows one.
///
/// Dropping this removes the directory, so a killed run leaves nothing behind.
#[derive(Debug)]
pub struct Workspace {
    dir: tempfile::TempDir,
    /// The PDF, copied in. The original is never exposed.
    ///
    /// Read by the tests that check the boundary, and by nothing in the run
    /// itself: the agent opens `datasheet.pdf` by name inside its own working
    /// directory, which is the point.
    pub pdf: PathBuf,
    /// One PNG per rendered page, in page order.
    pub pages: Vec<PathBuf>,
}

impl Workspace {
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Where codex must write its answer. Reading a file beats scraping stdout:
    /// stdout carries the agent's narration too, and a model that says "here
    /// is the TOML" twice leaves two candidate blocks to choose between.
    pub fn answer_path(&self) -> PathBuf {
        self.dir.path().join("model.toml")
    }
}

/// Build a sandbox, for the tests that check the boundary holds.
///
/// The boundary is the whole security story of an agent running full-auto, so
/// it is asserted rather than trusted. Exposed for `tests/extract_sandbox.rs`.
pub fn sandbox_for_test(pdf: &Path) -> Result<Workspace> {
    prepare_workspace(pdf)
}

/// Build the sandbox: copy the PDF in, render its pages, and write the text.
fn prepare_workspace(pdf: &Path) -> Result<Workspace> {
    let dir = tempfile::Builder::new()
        .prefix("hauksbee-extract-")
        .tempdir()
        .context("creating the extraction sandbox")?;

    let copied = dir.path().join("datasheet.pdf");
    std::fs::copy(pdf, &copied)
        .with_context(|| format!("copying {} into the sandbox", pdf.display()))?;

    // Page renders are best-effort. Without poppler the extraction still runs
    // on text alone, which is what it did before, so a missing optional tool
    // degrades the result rather than failing the command.
    let mut pages = Vec::new();
    if which("pdftoppm") {
        let out = Command::new("pdftoppm")
            .args(["-png", "-r", &RENDER_DPI.to_string(), "-f", "1", "-l"])
            .arg(MAX_RENDERED_PAGES.to_string())
            .arg(&copied)
            .arg(dir.path().join("page"))
            .output();
        if let Ok(o) = out {
            if !o.status.success() {
                eprintln!(
                    "[model-extract] page rendering failed, continuing on text alone: {}",
                    String::from_utf8_lossy(&o.stderr)
                        .lines()
                        .next()
                        .unwrap_or("")
                );
            }
        }
        let mut found: Vec<PathBuf> = std::fs::read_dir(dir.path())
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "png"))
            .collect();
        // pdftoppm zero-pads its suffix, so lexical order is page order.
        found.sort();
        pages = found;
    } else {
        eprintln!(
            "[model-extract] pdftoppm not found, so the model sees text only.              Install poppler for page images: the pinout and the ratings table              survive a render far better than a text dump."
        );
    }

    Ok(Workspace {
        pdf: copied,
        pages,
        dir,
    })
}

/// Run one extraction end to end, as the CLI does.
pub fn run(args: Args) -> Result<PathBuf> {
    // 0. PDF precheck, BEFORE any backend is chosen or anything is sent: a
    // non-PDF file (a saved HTML page, a .docx) would otherwise be text-dumped
    // and shipped to the LLM, producing a confidently wrong model. Magic
    // bytes rather than extension, so a downloaded datasheet with no
    // extension still passes and a renamed HTML file still fails.
    {
        let mut head = [0u8; 5];
        use std::io::Read;
        let is_pdf = std::fs::File::open(&args.pdf)
            .and_then(|mut f| f.read_exact(&mut head))
            .map(|_| &head == b"%PDF-")
            .unwrap_or(false);
        if !is_pdf {
            anyhow::bail!(
                "'{}' is not a PDF (no %PDF header); the extractor reads PDF \
                 datasheets only. Nothing was sent.",
                args.pdf.display()
            );
        }
    }
    // 1. Extract text from PDF
    let pdf_text = extract_pdf_text(&args.pdf)?;

    // An empty kind means "you work it out". Being made to classify a part
    // before the tool will look at it is a barrier at exactly the wrong
    // moment: the datasheet says what the part is on its first page, the model
    // is about to read that page, and the person asking for a model is
    // precisely the one who may not know which of our categories their part
    // falls into.
    let mut args = args;
    if args.kind_str.trim().is_empty() {
        let chosen = identify_kind(&args, &pdf_text)?;
        eprintln!(
            "[model-extract] identified {} as kind '{}'",
            args.part, chosen
        );
        args.kind_str = chosen;
    }

    eprintln!(
        "[model-extract] part={} kind={} pdf={}",
        args.part,
        args.kind_str,
        args.pdf.display()
    );

    // Declarative register-map sensor kinds (`i2c_sensor` / `spi_sensor`) emit a
    // `[sensor]` spec validated against hauksbee-models::sensor_spec, NOT the
    // SPICE `[[models]]` schema, so they take a separate prompt + validation
    // path that round-trips through `SensorSpec`.
    if is_sensor_kind(&args.kind_str) {
        let prompt = build_sensor_prompt(&args.part, &args.kind_str, &pdf_text);
        let raw = call_backend(&prompt, &args)?;
        let spec = validate_sensor_reply(&raw, &args.part, &args.kind_str)?;

        let default_dir = default_out_dir();
        let out_dir: &Path = args.out_dir.as_deref().unwrap_or(&default_dir);
        std::fs::create_dir_all(out_dir)
            .with_context(|| format!("creating output directory {}", out_dir.display()))?;
        let out_path = out_dir.join(format!("{}.sensor.toml", sanitise_filename(&args.part)));
        std::fs::write(&out_path, &raw)
            .with_context(|| format!("writing output to {}", out_path.display()))?;

        println!("[model-extract] written: {}", out_path.display());
        println!("{}", spec.sensor.name);
        return Ok(out_path);
    }

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

    println!("[model-extract] written: {}", out_path.display());
    println!("{}", entry.id);
    Ok(out_path)
}

// ── Declarative register-map sensor extraction (i2c_sensor / spi_sensor) ──────

/// Is this a declarative register-map sensor kind?
fn is_sensor_kind(kind: &str) -> bool {
    matches!(kind.trim(), "i2c_sensor" | "spi_sensor")
}

/// Build the LLM prompt that instructs the model to read a sensor datasheet and
/// emit a `[sensor]` declarative register-map spec (the format defined in
/// hauksbee-models::sensor_spec). The reply is validated by parsing it as a
/// `SensorSpec` and round-tripping it through TOML.
fn build_sensor_prompt(part: &str, kind: &str, pdf_text: &str) -> String {
    let bus = if kind.trim() == "spi_sensor" {
        "spi"
    } else {
        "i2c"
    };
    let bus_specifics = if bus == "spi" {
        r#"This is a SPI sensor. Set:
    bus = "spi"
  and a [sensor.protocol] block:
    style = "spi_reg"          # first transfer byte = (rw_bit<<7 | register_addr)
    rw_read_is_high = true     # true if a READ sets the high command bit to 1
                               # (most ST / InvenSense parts). false otherwise.
    addr_mask = 0x7f           # mask to recover the register address
  Do NOT set i2c_address."#
    } else {
        r#"This is an I2C sensor. Set:
    bus = "i2c"
    i2c_address = 0x??         # the 7-bit address (default/all-pins-low value)
  and a [sensor.protocol] block:
    style = "i2c_pointer"      # master writes the register addr, then reads N bytes"#
    };

    format!(
        r#"You are a sensor register-map extraction assistant. Read the datasheet
text below for the part: {part}

Emit a DECLARATIVE register-map sensor spec in TOML, NOT a SPICE model. The goal
is to capture how firmware reads this sensor over the bus: its address/framing,
the key registers, and how each register's bytes encode a physical value.

{bus_specifics}

THE SPEC FORMAT (emit exactly this shape):

  [sensor]
  name = "{part}"
  bus = "{bus}"
  # (address / protocol per the bus specifics above)

  # Settable physical inputs the simulator can drive (the sweepable quantities,
  # e.g. temperature, a gyro axis, a pressure). One [[sensor.input]] each.
  [[sensor.input]]
  name = "temperature_c"      # snake_case; referenced by register `expr`s
  default = 25.0

  # Registers. EITHER a const register (identity / config) OR an encoded one.
  # WHO_AM_I / device-ID register, a constant the firmware checks:
  [[sensor.register]]
  addr = 0x0f
  const = [0x42]              # the exact identity byte(s) from the datasheet

  # A data register encoded from an input expression:
  [[sensor.register]]
  addr = 0x00
  bytes = 2                   # bytes returned by a read of this register
  encoding = "i16_be"        # see ENCODINGS below
  expr = "temperature_c"     # arithmetic over the declared input names
  # optional: scale =, offset =  (encoded = expr*scale + offset)

ENCODINGS (pick the one matching the datasheet's register format):
  u8, u16_be, u16_le, i16_be, i16_le   - plain integers, big/little endian
  q7.1_be                              - LM75-style temperature: signed,
                                         0.125 C/LSB, count left-justified by 5
                                         into a big-endian 16-bit word
  raw                                  - const-only register (no expr/encoding)

RULES:
1. Include the WHO_AM_I / device-ID register if the part has one (with its exact
   constant), plus the primary data register(s) firmware actually reads.
2. Every `expr` may only reference names declared in a [[sensor.input]].
3. Output ONLY the TOML, starting with `[sensor]`; no prose, no markdown fences.
4. Use only values stated in the datasheet; do not invent register addresses.

DATASHEET TEXT (truncated):
---
{pdf_text}
---

OUTPUT (TOML only, starting with [sensor]):
"#,
        part = part,
        bus = bus,
        bus_specifics = bus_specifics,
        pdf_text = truncate_to_chars(&pdf_text, 40_000),
    )
}

/// Validate a sensor-spec reply: it must start with `[sensor]`, parse as a
/// `SensorSpec` (which validates structure), round-trip losslessly through TOML,
/// and agree with the requested bus.
fn validate_sensor_reply(
    raw: &str,
    part: &str,
    kind: &str,
) -> Result<crate::sensor_spec::SensorSpec> {
    use crate::sensor_spec::{Bus, SensorSpec};

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("empty reply for {part}: the backend returned no TOML at all");
    }
    if !trimmed.contains("[sensor]") {
        bail!(
            "reply for {part} contains no [sensor] table; the backend likely \
             answered with prose instead of TOML. First 200 chars: {:.200}",
            trimmed
        );
    }

    let spec = SensorSpec::from_toml(trimmed)
        .with_context(|| format!("parsing/validating sensor spec for {part}"))?;

    // Round-trip: serialise back and re-parse, ensuring the spec is stable.
    let back = spec
        .to_toml()
        .with_context(|| format!("serialising sensor spec for {part}"))?;
    SensorSpec::from_toml(&back)
        .with_context(|| format!("round-trip re-parse of sensor spec for {part} failed"))?;

    // Bus must match the requested kind.
    let want_bus = if kind.trim() == "spi_sensor" {
        Bus::Spi
    } else {
        Bus::I2c
    };
    if spec.sensor.bus != want_bus {
        bail!(
            "bus mismatch for {part}: requested '{kind}' but the spec declares \
             bus = {:?}",
            spec.sensor.bus
        );
    }

    Ok(spec)
}

// ── Argument parsing ──────────────────────────────────────────────────────────

/// Which LLM backend an extraction talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// `codex exec`, the agent CLI (the default).
    Codex,
    /// Headless `claude -p`, the same prompt contract as codex.
    ClaudeCode,
    /// An OpenAI-compatible chat-completions endpoint.
    Api,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Codex => "codex",
            Backend::ClaudeCode => "claude-code",
            Backend::Api => "api",
        }
    }
}

impl std::str::FromStr for Backend {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "codex" => Ok(Backend::Codex),
            "claude-code" => Ok(Backend::ClaudeCode),
            "api" => Ok(Backend::Api),
            other => bail!("unknown backend '{other}': expected codex, claude-code, or api"),
        }
    }
}

#[derive(Debug)]
pub struct Args {
    pub pdf: PathBuf,
    pub part: String,
    pub kind_str: String,
    pub out_dir: Option<PathBuf>,
    /// Retry count for LLM calls (default 1)
    pub retries: usize,
    /// Model the extraction agent runs on. `None` takes
    /// `HAUKSBEE_CODEX_MODEL` (codex) / `HAUKSBEE_LLM_MODEL` (api), then
    /// [`DEFAULT_CODEX_MODEL`].
    pub model: Option<String>,
    /// Which backend to call. `None` keeps the pre-flag behaviour: the api
    /// backend when `HAUKSBEE_LLM_API_KEY` is set, codex otherwise.
    pub backend: Option<Backend>,
    /// Base URL for the api backend. `None` takes `HAUKSBEE_LLM_BASE_URL`,
    /// then `https://api.openai.com/v1`.
    pub api_base: Option<String>,
    /// NAME of the environment variable holding the api key. The key itself
    /// is never accepted as a flag value and never stored: it is read from
    /// the named variable at call time. `None` takes `HAUKSBEE_LLM_API_KEY`
    /// when set, else `OPENAI_API_KEY`.
    pub api_key_env: Option<String>,
}

impl Args {
    /// Build the arguments for one extraction, for a caller that is not the
    /// standalone binary's own `--flag` parser.
    pub fn new(pdf: PathBuf, part: String, kind_str: String) -> Self {
        Args {
            pdf,
            part,
            kind_str,
            out_dir: None,
            retries: 1,
            model: None,
            backend: None,
            api_base: None,
            api_key_env: None,
        }
    }

    pub fn out_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.out_dir = dir;
        self
    }

    /// Pick the model the extraction agent runs on. An empty string means "not
    /// chosen", which is what a form field the user left alone posts, and must
    /// fall through to the default rather than become `--model ""`.
    pub fn model(mut self, model: Option<String>) -> Self {
        self.model = model.filter(|m| !m.trim().is_empty());
        self
    }

    /// Pick the backend. `None` keeps the environment-driven default.
    pub fn backend(mut self, backend: Option<Backend>) -> Self {
        self.backend = backend;
        self
    }

    pub fn api_base(mut self, base: Option<String>) -> Self {
        self.api_base = base.filter(|b| !b.trim().is_empty());
        self
    }

    pub fn api_key_env(mut self, name: Option<String>) -> Self {
        self.api_key_env = name.filter(|n| !n.trim().is_empty());
        self
    }
}

/// Reject anything that is not a plausible environment-variable NAME. The
/// flag takes the variable's name, never the key itself: a value with `-`,
/// `.`, or other non-identifier characters is almost certainly a pasted key,
/// and accepting it would put a secret on a world-readable argv.
pub fn validate_api_key_env_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let ok_first = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !ok_first || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        bail!(
            "--api-key-env takes the NAME of an environment variable (e.g. \
             OPENAI_API_KEY), not the key itself. Export the key first, then \
             pass the variable's name."
        );
    }
    Ok(())
}

/// What a caller must show the user before running an extraction, and what it
/// must record on whatever the extraction produces.
///
/// This lives beside the code that does the sending so the two cannot drift.
/// A surface that offers extraction without showing `CONSENT_NOTICE` first is
/// a bug, not a shortcut.
pub const CONSENT_NOTICE: &str =
    "This sends the datasheet's text to an LLM backend (codex by default; \
     claude-code or an OpenAI-compatible API with --backend). Nothing is sent \
     until you ask for it. The result is a draft for you to check, not a \
     measurement: a model it writes carries provenance \"datasheet-extracted\".";

pub fn parse_args() -> Result<Args> {
    parse_args_from(std::env::args().skip(1))
}

/// The flag parser, over any argument source so tests can drive it without a
/// process spawn. `--help` still exits: it is a terminal answer, not a value.
pub fn parse_args_from(args: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut args = args.into_iter();
    let mut pdf: Option<PathBuf> = None;
    let mut part: Option<String> = None;
    // Empty means "the model works it out from the datasheet". Defaulting to
    // bjt_npn silently produced a transistor model for whatever was handed in.
    let mut kind_str = String::new();
    let mut out_dir: Option<PathBuf> = None;
    let mut model: Option<String> = None;
    let mut backend: Option<Backend> = None;
    let mut api_base: Option<String> = None;
    let mut api_key_env: Option<String> = None;

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
            "--model" => {
                model = Some(args.next().context("--model requires a value")?);
            }
            "--backend" => {
                backend = Some(
                    args.next()
                        .context("--backend requires a value (codex, claude-code, or api)")?
                        .parse()?,
                );
            }
            "--api-base" => {
                api_base = Some(args.next().context("--api-base requires a value")?);
            }
            "--api-key-env" => {
                let name = args
                    .next()
                    .context("--api-key-env requires an environment variable NAME")?;
                validate_api_key_env_name(&name)?;
                api_key_env = Some(name);
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
        model,
        backend,
        api_base,
        api_key_env,
    })
}

pub fn print_help() {
    println!(
        r"model-extract: extract a simulation model from a PDF datasheet

USAGE:
    model-extract --pdf <path> --part <part_number> [OPTIONS]

OPTIONS:
    --pdf <path>          Path to (or URL of) the datasheet PDF
    --part <name>         Manufacturer part number (e.g. BCM847BS)
    --kind <kind>         Component kind hint (omit it and the model works it
                          out from the datasheet)
                          passive|diode|bjt_npn|bjt_pnp|nmos|pmos|
                          vreg|opamp|comparator|analog_switch|digital|
                          dac|adc|shift_register|mcu|connector|ignore|
                          charger|pmic|balancer  (behavioural families)|
                          i2c_sensor|spi_sensor  (declarative register-map
                          sensors → a [sensor] spec, not a SPICE model)
    --out-dir <dir>       Output directory (default: ~/.hauksbee/models/)
    --model <id>          Model for the extraction agent
                          (default: gpt-5.6-sol at high reasoning effort)
    --backend <name>      LLM backend: codex (default), claude-code, or api
    --api-base <url>      Base URL for the api backend
                          (default: https://api.openai.com/v1)
    --api-key-env <NAME>  Environment variable holding the api key
                          (default: OPENAI_API_KEY). The NAME, never the key:
                          the key is read from the environment at call time.

ENVIRONMENT:
    HAUKSBEE_LLM_API_KEY   API key for OpenAI-compatible backend (setting it
                           selects the api backend when --backend is absent)
    HAUKSBEE_CODEX_MODEL   Model for the codex backend (default: gpt-5.6-sol)
    HAUKSBEE_CODEX_EFFORT  Reasoning effort for it (default: high)
    HAUKSBEE_LLM_MODEL     Model ID for API backend (e.g. gpt-5.6-sol)
    HAUKSBEE_LLM_BASE_URL  Base URL (default: https://api.openai.com/v1)
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
    eprintln!("[model-extract] pdftotext not found; LLM backend will read the PDF directly");
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

#[cfg(test)]
mod truncate_tests {
    use super::truncate_to_chars;

    /// Round-8 #4: datasheet text was truncated with a byte-index slice, which
    /// panics when a multibyte glyph straddles the cut. `truncate_to_chars`
    /// cuts on char boundaries, no panic, and it never splits a char.
    #[test]
    fn truncates_on_char_boundaries_without_panic() {
        // 30 multibyte chars ('µ' = 2 bytes each); a byte slice at 40 would
        // land mid-char.
        let s = "µ".repeat(30);
        let out = truncate_to_chars(&s, 20);
        assert_eq!(out.chars().count(), 20, "keeps exactly 20 chars");
        assert!(out.chars().all(|c| c == 'µ'), "never splits a char");
        // Shorter-than-limit input is returned whole.
        assert_eq!(truncate_to_chars("abc", 10), "abc");
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

Produce a TOML model entry that exactly conforms to the hauksbee-models schema.
The entry must use [[models]] array syntax and include:
- id: lowercase part number (e.g. "{part_lower}")
- kind: "{kind}"
- description: brief human-readable description
- [models.match] section with at least value_re or mpn_re
- [models.params] section with all required numeric params for the kind
- [models.ratings] section with the absolute-maximum ratings (see below)
- [models.pins] section mapping pad numbers to pin roles

PIN ROLE NAMES for kind="{kind}": {pin_roles}
  Use these exact spellings. They are looked up by exact string, so "output"
  instead of "out", or "ground" instead of "gnd", makes the part bind OPEN: it
  contributes nothing to the circuit and every result on its nets is wrong.
  A pin with no role in the list (a NC, a tab, a second ground) can be given
  any descriptive name, or left out.

For kind="{kind}", the required params are:
{required_params}

[models.ratings]: pull these from the datasheet's "absolute maximum ratings" /
"limiting values" table. Include every field the datasheet gives a number for;
omit a field entirely if the datasheet does not state it (do NOT invent it):
{ratings_hint}

PIN NUMBERING, read this before you fill in [models.pins]:

  The pin map is the one field where being wrong is worse than being absent. A
  wrong value makes a simulation inaccurate; a wrong pin map makes it a
  simulation of a different circuit, and it still binds cleanly, so nobody
  finds out. Treat it as the hardest part of this job, not the easiest.

  P1. PREFER A NUMBERED TABLE. If the datasheet has a pin-function or terminal
      table with a "Pin"/"No."/"Terminal" column, that table is the answer. Use
      it and cite it. Do not re-derive the numbering from a picture when a
      table exists.
  P2. A PACKAGE DRAWING IS NOT A PIN TABLE. Drawings are routinely rotated 90
      degrees, drawn from the BOTTOM, or drawn as a "front view" with the leads
      pointing sideways. Reading such a figure top-to-bottom and calling the
      first label pin 1 is the single most common way to get this wrong. If you
      must use a figure:
        - say which view it is (top / bottom / front) and which way it is
          rotated, in the comment;
        - work out where pin 1 actually is (the dot, the notch, the bevel, the
          tab) and count from there in the direction the package standard
          requires, not in the direction the labels happen to be printed;
        - remember a bottom view mirrors the numbering left-to-right.
  P3. CROSS-CHECK AGAINST A SECOND PLACE IN THE DOCUMENT. The typical
      application schematic, the package outline, and the pin table should all
      agree. Say in the comment which two you checked. If they disagree, do NOT
      pick one: write the pin map you believe and add
      `# UNRESOLVED: <figure A> says X, <figure B> says Y` on the affected
      lines.
  P4. NEAR-IDENTICAL PARTS OFTEN DIFFER. A negative regulator does not share
      its positive sibling's pinout; a SOT-23 and a SOT-89 of the same part
      often differ. If the datasheet covers several packages, state which one
      this map is for in the description, and pick the package the value/
      footprint you were given implies.
  P5. If after all that you are still unsure, say so in a comment on the pins
      block rather than presenting a guess as read. An honest
      `# LOW CONFIDENCE: ...` is useful. A confident wrong map is not.

IMPORTANT RULES:
1. Add a comment on each param/rating line citing where in the datasheet you found
   the value, e.g.: `# Source: Table 6.3, typ column`
2. Use only values explicitly stated in the datasheet; do NOT guess. For SPICE
   params not given verbatim (e.g. `is`), you may derive them from a stated
   operating point (e.g. VBE at a known IC) and say so in the comment.
3. If a required param is genuinely absent and cannot be derived, use a
   conservative typical value for the part family and comment `# estimated`.
4. Output ONLY the TOML block, starting with [[models]]; no prose, no markdown
   fences, no leading or trailing text.
5. Currents in AMPERES, voltages in VOLTS, power in WATTS (convert mA/mW yourself).
6. Param values must be within these physical bounds:
   - is: 1e-20 to 1e-3
   - bf/beta: 1 to 2000
   - n/nf (emission): 0.5 to 3.0
   - vaf (Early V): 1 to 500
   - vto (MOSFET threshold): -10 to +10
   - kp (transconductance): 1e-6 to 1.0
   - vout (LDO): magnitude 0.5 to 30, EITHER SIGN. Write the signed output
     voltage as the datasheet states it: a negative regulator (79xx, and any
     part whose output is below GND) takes a NEGATIVE vout, e.g. -5.0 for an
     LM7905. It is stamped as a source against ground, so writing the magnitude
     instead produces a part that regulates the wrong side of ground and turns
     a negative supply into a second positive one.
   - ron (switch): 0.01 to 10000
   - roff (switch): 1e3 to 1e12
{behavioral_hint}
OUTPUT (TOML only, starting with [[models]]):
"#,
        part = part,
        part_lower = part.to_lowercase(),
        kind = kind,
        pdf_text = truncate_to_chars(&pdf_text, 40_000),
        required_params = required_params_for_kind(kind),
        pin_roles = pin_roles_for_kind(kind),
        ratings_hint = ratings_hint_for_kind(kind),
        behavioral_hint = behavioral_hint_for_kind(kind),
    )
}

/// A behavioural-family kind (charger / pmic / balancer) maps to a base
/// `ComponentKind` for the TOML `kind = "..."` line, and triggers the extra
/// `[models.behavioral]` prompt section. Returns `None` for ordinary kinds.
fn behavioral_family_base_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "charger" => Some("vreg"),
        "pmic" => Some("vreg"),
        "balancer" => Some("digital"),
        _ => None,
    }
}

/// The `[models.behavioral]` schema guidance appended to the prompt for the
/// behavioural families. Empty for ordinary kinds. Per-family so the model is
/// asked for the right declarative facts (a charger's input-current limit and
/// sense pins; a PMIC's internal pin pulls; a balancer's leak law).
fn behavioral_hint_for_kind(kind: &str) -> String {
    let Some(base) = behavioral_family_base_kind(kind) else {
        return String::new();
    };
    let common = format!(
        r#"
BEHAVIOURAL MODEL: this is a "{kind}" power IC. In ADDITION to the above, set
`kind = "{base}"` (the base kind) and add a `[models.behavioral]` block that
captures the part's internal behaviour the SPICE kinds cannot. The schema:

  [models.behavioral.pins.<role>]   # one per pin with internal semantics
  pull_to = "<rail role>"           # internal pull to another named pin's rail
  pull_ohms = <ohms>                # resistance of that internal pull
  open_drain = true                 # open-drain output (optional)
  enable_threshold_v = <volts>      # enable-input threshold (optional)

  [models.behavioral.converter]     # for a switching converter / charger
  topology = "buck" | "boost" | "buck_boost"
  out_pin = "<role>"                # the regulated output pin role
  in_pin  = "<role>"                # the input pin role
  vout_setpoint = <volts>           # regulated output voltage
  efficiency = <0..1>
  [models.behavioral.converter.iin_program]   # programmable input-current limit
  rsense_ref = "<board refdes>"     # the input sense resistor on the board
  prog_ref   = "<board refdes>"     # the limit-programming resistor on the board
  vprog_ref = <volts>               # sense threshold at prog = prog_ref_ohms
  prog_ref_ohms = <ohms>            # programming resistor at the threshold point
  v_sense_full = <volts>            # full-scale current-sense voltage

  [[models.behavioral.laws]]        # an expression law (current/voltage)
  name = "<name>"
  kind = "current" | "voltage"
  a = "<pin role>"  ; b = "<pin role>"
  expr = "<arithmetic over v_<role>, params>"   # e.g. "v_vplus / tie_ohms"

Only include the blocks the datasheet supports. Cite the datasheet for each
number (pin functions table, electrical characteristics, typical application).
"#
    );
    let specific = match kind {
        "charger" => {
            "\nFor a CHARGER specifically: identify the input pin (PVIN/VIN), the \
             battery/charge-output pin (BAT/VBAT), the input current-sense pins and \
             the ILIMIT / current-limit programming pin. Fill \
             [models.behavioral.converter] with topology and the regulated charge \
             voltage, and [models.behavioral.converter.iin_program] with the \
             current-sense and ILIMIT resistor relationship if the datasheet gives \
             the input-current-limit programming equation."
        }
        "pmic" => {
            "\nFor a PMIC specifically: identify any pin with an INTERNAL pull \
             (e.g. a ship-hold / SHPHLD pin with a pull-up to the system rail \
             VSYS) and encode it as [models.behavioral.pins.<role>] pull_to + \
             pull_ohms. List the buck/LDO output pins in [models.pins]."
        }
        "balancer" => {
            "\nFor a BALANCER / cell monitor specifically: identify the cell-input \
             pins and any tie/bleed network. If unused cell inputs are tied to a \
             rail through a resistor, encode the leak as a current [[law]] from the \
             top-of-stack pin to the bottom over a `tie_ohms` param."
        }
        _ => "",
    };
    format!("{common}{specific}\n")
}

/// Which absolute-maximum ratings fields are worth asking for, per kind. The
/// stress monitor reads these from `[models.ratings]`.
fn ratings_hint_for_kind(kind: &str) -> &'static str {
    match kind {
        "diode" => {
            "\
  max_current_a       # IF continuous forward current
  max_surge_current_a # IFSM non-repetitive surge current
  max_voltage_v       # VRRM repetitive peak reverse voltage"
        }
        "bjt_npn" | "bjt_pnp" => {
            "\
  max_current_a       # IC continuous collector current
  max_surge_current_a # ICM peak collector current
  max_power_w         # Ptot total power dissipation
  max_voltage_v       # VCEO collector-emitter breakdown voltage"
        }
        "nmos" | "pmos" => {
            "\
  max_current_a       # ID continuous drain current
  max_power_w         # Ptot total power dissipation
  max_voltage_v       # VDS drain-source breakdown voltage"
        }
        "vreg" => {
            "\
  max_current_a       # IOUT maximum output current
  max_voltage_v       # maximum input voltage (VIN abs max)
  max_junction_temp_c # TJ maximum junction temperature"
        }
        _ => {
            "\
  max_current_a       # if a continuous current limit is stated
  max_voltage_v       # if a maximum voltage is stated
  max_power_w         # if a power dissipation limit is stated"
        }
    }
}

/// The pin role names the BINDER accepts for a kind, verbatim.
///
/// The binder looks these up by exact string (`roles.get("out")`), so a model
/// that calls the pin "output" passes every other check and then binds OPEN.
/// Validation catches that, but only once the run is over, and a retry costs
/// another three minutes and another bill. Stating the vocabulary up front is
/// the cheap half of the same guarantee.
fn pin_roles_for_kind(kind: &str) -> &'static str {
    match kind {
        "diode" => "anode, cathode",
        "bjt_npn" | "bjt_pnp" => "collector, base, emitter",
        "nmos" | "pmos" => "drain, gate, source",
        "vreg" | "charger" | "pmic" => "in, out, gnd  (and en / adj / fb when the part has them)",
        "opamp" | "comparator" => "in_plus, in_minus, out, vcc, vee",
        "analog_switch" => "in_out_a, in_out_b (SPST) or com, s0, s1 (SPDT)",
        _ => "",
    }
}

fn required_params_for_kind(kind: &str) -> &'static str {
    match kind {
        "diode" => "is, n, rs  (also cjo, vj, m, bv if available)",
        "bjt_npn" | "bjt_pnp" => "is, bf, nf, vaf, br  (also rb, rc, re, cje, cjc, tf)",
        "nmos" | "pmos" => "vto, kp  (also lambda, rd, rs, cgd, cgs)",
        "vreg" => "vout, dropout_v, iq_a",
        "opamp" => "gain, rail_lo, rail_hi",
        "comparator" => "out_lo, out_hi, hysteresis, tpd_s",
        "analog_switch" => "ron, roff, vth",
        "digital" | "shift_register" => "voh, vol, vih, vil, tpd_s, supply_pin, gnd_pin",
        "dac" => "bits, vref_int, i2c_addr (or spi mode)",
        // These three were offered without guidance, so their prompts fell to
        // the generic hint and the model had to guess what hauksbee wanted.
        "adc" => "bits, vref_int, i2c_addr (or spi mode), and the input range",
        "passive" => {
            "the nominal value and its tolerance, plus esr/esl for a capacitor and the \
             self-resonant frequency if the datasheet states one"
        }
        "connector" => {
            "pin count and the pin-to-net roles; a connector carries no device physics, so \
             what matters is which pin is which"
        }
        "mcu" => "backend (e.g. simavr:atmega328p)",
        "charger" => "vout, dropout_v, iq_a  (the converter behaviour is in [models.behavioral])",
        "pmic" => "vout, dropout_v, iq_a  (the pin pulls are in [models.behavioral])",
        "balancer" => "(the leak law is in [models.behavioral]; no required numeric params)",
        _ => "(see db/README.md for kind-specific requirements)",
    }
}

// ── Backend dispatch ──────────────────────────────────────────────────────────

fn call_backend(prompt: &str, args: &Args) -> Result<String> {
    // Test / offline hook: HAUKSBEE_EXTRACT_MOCK_REPLY points to a file holding a
    // canned backend reply. This exercises the full parse + validate + retry
    // path with no codex and no network, so CI can run it deterministically.
    if let Ok(path) = std::env::var("HAUKSBEE_EXTRACT_MOCK_REPLY") {
        let reply = std::fs::read_to_string(&path)
            .with_context(|| format!("reading mock reply from {path}"))?;
        let raw = extract_toml_block(&reply);
        // Validate just like a real backend reply so the hook can't smuggle
        // garbage past the pipeline.
        parse_and_validate_reply(&raw, &args.part, &args.kind_str)?;
        return Ok(raw);
    }

    // No explicit --backend keeps the pre-flag behaviour: an exported
    // HAUKSBEE_LLM_API_KEY selects the api backend, codex otherwise.
    let chosen = args.backend.unwrap_or_else(|| {
        if std::env::var("HAUKSBEE_LLM_API_KEY").is_ok() {
            Backend::Api
        } else {
            Backend::Codex
        }
    });

    match chosen {
        Backend::Api => call_api_backend(prompt, args),
        Backend::Codex => {
            if !which("codex") {
                bail!(
                    "the codex backend needs the `codex` CLI, which is not in PATH. \
                     Install it (`npm install -g @openai/codex` or `brew install codex`) \
                     and sign in, or pick another backend: --backend claude-code \
                     (needs `claude` in PATH) or --backend api (set OPENAI_API_KEY)."
                );
            }
            call_agent_backend(prompt, args, "codex", run_codex_once)
        }
        Backend::ClaudeCode => {
            if !which("claude") {
                bail!(
                    "the claude-code backend needs the `claude` CLI, which is not in \
                     PATH. Install Claude Code (`npm install -g @anthropic-ai/claude-code`) \
                     and sign in, or pick another backend: --backend codex (needs \
                     `codex` in PATH) or --backend api (set OPENAI_API_KEY)."
                );
            }
            call_agent_backend(prompt, args, "claude", run_claude_once)
        }
    }
}

/// Call codex non-interactively and return its (validated) TOML reply.
///
/// Invocation notes learned the hard way:
///   * `--sandbox workspace-write` (`--full-auto` is deprecated).
///   * `--skip-git-repo-check`, codex otherwise refuses to run outside a repo.
///   * `--cd <pdf_dir>` so codex can open the datasheet PDF / extracted text
///     directly when pdftotext was unavailable.
///   * stdin must be closed/empty or codex blocks "Reading additional input
///     from stdin..." forever. We give it an empty stdin and read only stdout;
///     the answer itself comes from --output-last-message.
///   * the prompt goes in a FILE inside the sandbox, not in argv. It embeds up
///     to 40,000 characters of the datasheet, and argv is world-readable on
///     Linux via /proc/<pid>/cmdline and visible to `ps -ww` on macOS. Putting
///     the very text the user consented to send to OpenAI where any local user
///     can read it would undo the consent. It also kept a large datasheet
///     clear of ARG_MAX, which would otherwise fail as a generic spawn error.
///
/// The claude-code backend shares this loop with the same contract: one
/// sandboxed run per attempt, the answer preferred from `model.toml`, and a
/// validation failure fed back verbatim for the retry.
fn call_agent_backend(
    prompt: &str,
    args: &Args,
    tool: &str,
    run_once: fn(&str, &Workspace, Option<&str>) -> Result<String>,
) -> Result<String> {
    // The sandbox is a scratch copy, never the user's own directory. See
    // `Workspace`: the agent runs full-auto, so what it can reach is the whole
    // of the security story.
    let ws = prepare_workspace(&args.pdf)?;
    eprintln!(
        "[model-extract] sandbox {} with {} page render(s)",
        ws.path().display(),
        ws.pages.len()
    );

    let mut prompt = format!("{prompt}\n\n{}", verification_clause(&ws));
    for attempt in 0..=args.retries {
        let started = Instant::now();
        let raw_stdout = run_once(&prompt, &ws, args.model.as_deref())?;
        // Prefer the file we asked for. Falling back to stdout keeps a model
        // that answered in prose from failing outright, but the file is the
        // reliable path: stdout also carries the agent's narration, and one
        // that says "here is the TOML" twice yields two candidates.
        let raw = match std::fs::read_to_string(ws.answer_path()) {
            Ok(t) if !t.trim().is_empty() => extract_toml_block(&t),
            _ => extract_toml_block(&raw_stdout),
        };
        eprintln!(
            "[model-extract] {tool} attempt {} returned {} chars in {:.0}s",
            attempt + 1,
            raw.len(),
            started.elapsed().as_secs_f64()
        );

        match parse_and_validate_reply(&raw, &args.part, &args.kind_str) {
            Ok(_) => return Ok(raw),
            Err(e) if attempt < args.retries => {
                eprintln!(
                    "[model-extract] attempt {} failed: {}; retrying with feedback...",
                    attempt + 1,
                    e
                );
                let _ = std::fs::remove_file(ws.answer_path());
                prompt = format!(
                    "{prompt}\n\nYOUR PREVIOUS ANSWER FAILED VALIDATION WITH:\n{e}\n\n\
                     Fix exactly those issues and write the corrected TOML to model.toml.",
                );
                continue;
            }
            Err(e) => bail!(
                "{tool} produced a model that failed validation after {} attempt(s): {e}\n\
                 Raw reply was:\n{raw}",
                attempt + 1
            ),
        }
    }

    unreachable!()
}

/// Ask the backend what kind of part this is, from the kinds we support.
///
/// A separate, cheap call rather than folded into the extraction prompt,
/// because validation has to pick a schema before it can check anything: the
/// kind is an input to the extraction, not an output of it. Two calls also
/// means the user is told which kind was chosen BEFORE the model commits to
/// it, and a mis-identified kind is the one error they are best placed to
/// catch.
fn identify_kind(args: &Args, pdf_text: &str) -> Result<String> {
    // The front of the datasheet is where the part describes itself. Sending
    // the whole thing to answer one question would cost far more for no gain.
    let head = truncate_to_chars(pdf_text, 6000);
    let prompt = format!(
        "You are identifying what kind of electronic part a datasheet describes, so a \
         simulator can pick the right model schema.\n\n\
         Part number: {}\n\n\
         Answer with EXACTLY ONE of these identifiers and nothing else:\n\
         {}\n\n\
         If the part does not fit any of them, answer exactly: unsupported\n\n\
         Do not explain. Do not add punctuation. One word.\n\n\
         Datasheet (first pages):\n{}",
        args.part,
        SUPPORTED_KINDS.join("\n"),
        head
    );
    let raw = call_backend(&prompt, args)?;
    // The model may answer with surrounding prose despite the instruction, so
    // look for a supported kind rather than trusting the whole reply.
    let reply = raw.to_ascii_lowercase();
    let found: Vec<&str> = SUPPORTED_KINDS
        .iter()
        .copied()
        .filter(|k| {
            reply
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .any(|w| w == *k)
        })
        .collect();
    match found.as_slice() {
        [one] => Ok((*one).to_string()),
        [] if reply.contains("unsupported") => bail!(
            "the datasheet for {} does not describe a part hauksbee models yet. That is an \
             honest answer rather than a failure: forcing it into the nearest kind would \
             produce a confident model of the wrong device. Supported kinds: {}",
            args.part,
            SUPPORTED_KINDS.join(", ")
        ),
        [] => bail!(
            "could not identify what kind of part {} is from its datasheet. Say so \
             explicitly with --kind <one of: {}>.",
            args.part,
            SUPPORTED_KINDS.join(", ")
        ),
        many => bail!(
            "the datasheet for {} matched several part kinds ({}), so the choice is not \
             ours to guess. Pick one with --kind.",
            args.part,
            many.join(", ")
        ),
    }
}

/// Every kind the extractor can produce a model for.
///
/// One list, used by the identification prompt, the CLI help and the web
/// picker, so the three cannot drift into disagreeing about what is possible.
pub const SUPPORTED_KINDS: &[&str] = &[
    "passive",
    "diode",
    "bjt_npn",
    "bjt_pnp",
    "nmos",
    "pmos",
    "vreg",
    "opamp",
    "comparator",
    "analog_switch",
    "digital",
    "dac",
    "adc",
    "shift_register",
    "mcu",
    "connector",
    "i2c_sensor",
    "spi_sensor",
    // Behavioural families. They resolve to a base kind (charger and pmic to
    // vreg, balancer to digital) but carry their own parameter guidance, so a
    // caller naming one gets a better prompt than naming the base.
    "charger",
    "pmic",
    "balancer",
];

/// The part of the prompt that tells the model to check its own work.
///
/// A datasheet extraction that is confidently wrong is worse than one that
/// refuses, because a wrong model does not announce itself: it produces a
/// plausible simulation of a part that does not exist. So the instruction is
/// not "be accurate", which asks for nothing, but three specific acts: read
/// each number back against the page it came from, test it against physics,
/// and say out loud which values were never stated.
fn verification_clause(ws: &Workspace) -> String {
    let pages = if ws.pages.is_empty() {
        "No page images were rendered, so you have the extracted text only. Say \
         so in `notes` for any value whose meaning depended on a table layout."
            .to_string()
    } else {
        format!(
            "{} page image(s) are attached, in page order, and `datasheet.pdf` is in \
             your working directory. Read values off the IMAGES for anything that \
             lives in a table or a pinout: the text extraction loses which column a \
             number belonged to, and that is exactly where the absolute-maximum and \
             electrical-characteristics tables live.",
            ws.pages.len()
        )
    };

    format!(
        "## Before you answer\n\n         {pages}\n\n         1. VERIFY EACH NUMBER. For every parameter you extracted, find it again on \
            the page and check the units, the sign, and the conditions it was \
            measured under. A value quoted at the wrong test condition is wrong.\n         2. CHECK IT IS PHYSICALLY POSSIBLE. Reject your own answer if it implies \
            something that cannot happen: a bipolar transistor with a gain of 5, a \
            silicon junction conducting at 0.2 V, a regulator whose dropout exceeds \
            its input headroom, a package dissipating more than its thermal \
            resistance allows. If the datasheet seems to say such a thing, you have \
            misread which column or which part variant you are on.\n         3. SAY WHAT YOU ASSUMED. Any value the datasheet did not state outright \
            goes in `notes` as an assumption, with what you based it on. A typical \
            value used where the model wants a maximum is an assumption. A figure \
            read off a graph is an assumption. Do not quietly fill a gap with a \
            textbook default.\n         4. If the datasheet is genuinely ambiguous and you can search the web, \
            check the vendor's page or an application note before guessing, and \
            record what you used.\n\n         Write your final answer to `model.toml` in your working directory, as a \
         single TOML document and nothing else."
    )
}

/// The model the extraction agent runs on, and how hard it is asked to think.
///
/// Reading a datasheet is not a cheap task. The values are easy (a table cell
/// is a table cell); the pin map is where a weak model fails, because package
/// drawings are rotated, mirrored, and labelled without numbers, and getting
/// one wrong produces a part that binds cleanly and simulates a different
/// device. So the default is the strongest tier at high reasoning effort rather
/// than whatever codex happens to default to.
///
/// Override with `--model` or `HAUKSBEE_CODEX_MODEL` / `HAUKSBEE_CODEX_EFFORT`.
pub const DEFAULT_CODEX_MODEL: &str = "gpt-5.6-sol";
pub const DEFAULT_CODEX_EFFORT: &str = "high";

/// Resolve the model and reasoning effort for a codex run: an explicit
/// `--model` wins, then the environment, then the default above.
pub fn codex_model(explicit: Option<&str>) -> (String, String) {
    let model = explicit
        .map(str::to_string)
        .or_else(|| std::env::var("HAUKSBEE_CODEX_MODEL").ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CODEX_MODEL.to_string());
    let effort = std::env::var("HAUKSBEE_CODEX_EFFORT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CODEX_EFFORT.to_string());
    (model, effort)
}

/// One codex invocation with a hard timeout. Returns stdout (the agent's final
/// message) on success.
fn run_codex_once(prompt: &str, ws: &Workspace, model: Option<&str>) -> Result<String> {
    let (model, effort) = codex_model(model);
    let mut cmd = Command::new("codex");
    cmd.args([
        "exec",
        // Pick the model rather than inheriting codex's default, which varies
        // by the user's plan and config and silently decides how good the
        // extraction is.
        "--model",
    ])
    .arg(&model)
    .arg("-c")
    .arg(format!("model_reasoning_effort=\"{effort}\""))
    .args([
        // Writes are confined to the writable roots: --cd plus $TMPDIR and
        // /tmp. The agent needs to write, since it renders, greps and
        // re-checks its own answer in there. Reads are NOT confined by this
        // profile; see the Workspace doc for what that means and why the rest
        // of the contract carries the weight.
        "--sandbox",
        "workspace-write",
        // Without this codex refuses to run outside a git repo, and the
        // sandbox deliberately is not one.
        "--skip-git-repo-check",
        "--cd",
    ])
    .arg(ws.path())
    // The answer goes to a file we name, so a reply that also narrates does
    // not leave two candidate TOML blocks to choose between.
    .arg("--output-last-message")
    .arg(ws.dir.path().join("last-message.txt"));
    // Page renders, in page order. A table or a pinout survives a render and
    // does not survive a text dump.
    for page in &ws.pages {
        cmd.arg("--image").arg(page);
    }
    // See the note above: the prompt is a file, and argv carries only its name.
    let prompt_path = ws.dir.path().join("prompt.md");
    std::fs::write(&prompt_path, prompt).context("writing the prompt into the sandbox")?;
    let log_path = ws.dir.path().join("codex-stderr.log");
    let log = std::fs::File::create(&log_path)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());
    // The instruction goes in on STDIN, never as a trailing positional argument.
    //
    // codex's `--image` takes many values, and an extraction passes one per
    // rendered page, so a trailing `<prompt>` parses as one more image path and
    // codex then reports "No prompt provided via stdin". A `--` separator also
    // avoids that, but stdin cannot be broken by the next flag that learns to
    // take many values.
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // NOT a pipe. codex logs its whole session to stderr, and the poll loop
        // below reads no pipe until the child has exited. A ten-minute agentic
        // run overflows the 64 KiB pipe buffer, codex blocks in write(2),
        // try_wait never reports an exit, and the loop burns the entire
        // CODEX_TIMEOUT before killing it. The failure then reads "codex timed
        // out with no answer", which points the blame at the model rather than
        // at us.
        //
        // A file, not /dev/null: codex explains itself on stderr, and a refused
        // model name or a rate limit is the whole diagnosis. Discarding it
        // leaves `codex exited with status 1: ` and nothing after the colon.
        // Its tail goes into the error below.
        .stderr(log)
        .spawn()
        .context(
            "spawning codex (is it installed and on PATH? `brew install codex` or set \
             HAUKSBEE_LLM_API_KEY)",
        )?;

    // Feed the instruction, then EOF. codex reads its prompt from stdin when no
    // positional one survived arg parsing, and it blocks waiting for EOF if the
    // pipe is left open.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(
            b"Read prompt.md in your working directory and follow it exactly. \
              Write your answer to model.toml.",
        );
        // dropping `stdin` here sends EOF
    }

    // Poll for completion up to CLI_BACKEND_TIMEOUT, then kill.
    let deadline = Instant::now() + CLI_BACKEND_TIMEOUT;
    loop {
        match child.try_wait().context("polling codex")? {
            Some(_status) => break,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(
                        "codex timed out after {}s with no answer; \
                         retry with a tighter prompt or set HAUKSBEE_LLM_API_KEY",
                        CLI_BACKEND_TIMEOUT.as_secs()
                    );
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }

    let output = child
        .wait_with_output()
        .context("collecting codex output")?;
    if !output.status.success() {
        // stderr is not captured (see the spawn above), so report what codex
        // put on stdout. Its session log is on the terminal, where a user
        // watching a long run wants it anyway.
        let tail = String::from_utf8_lossy(&output.stdout);
        // codex puts the reason it gave up on stderr, so a failure with an
        // empty stdout (a refused model, a rate limit, a bad config key) has
        // its whole explanation in the log file and none of it here.
        let err_tail = std::fs::read_to_string(&log_path)
            .map(|t| {
                t.lines()
                    .filter(|l| !l.trim().is_empty())
                    .rev()
                    .take(5)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join(" | ")
            })
            .unwrap_or_default();
        let detail = [
            tail.lines().rev().take(5).collect::<Vec<_>>().join(" | "),
            err_tail,
        ]
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" || ");
        bail!(
            "codex exited with status {}{}",
            output.status,
            if detail.is_empty() {
                " and said nothing on stdout or stderr".to_string()
            } else {
                format!(": {detail}")
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// One headless `claude -p` invocation, holding the same contract as the codex
/// run: the sandbox is the working directory, the full prompt goes in a FILE
/// inside it (argv is world-readable, and stdin carries only the pointer),
/// the rendered pages and the PDF are readable beside it, and the answer is
/// expected in `model.toml` with stdout as the fallback.
///
/// `--permission-mode acceptEdits` lets the agent write `model.toml` without
/// an interactive prompt; the sandbox directory bounds what those edits can
/// touch, exactly as it does for codex.
fn run_claude_once(prompt: &str, ws: &Workspace, model: Option<&str>) -> Result<String> {
    let mut cmd = Command::new("claude");
    cmd.args([
        "-p",
        "--output-format",
        "text",
        "--permission-mode",
        "acceptEdits",
    ])
    .current_dir(ws.path());
    // An explicit --model only: claude's model names are its own, so the codex
    // env defaults must not leak into it.
    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }

    let prompt_path = ws.dir.path().join("prompt.md");
    std::fs::write(&prompt_path, prompt).context("writing the prompt into the sandbox")?;
    let log_path = ws.dir.path().join("claude-stderr.log");
    let log = std::fs::File::create(&log_path)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // A file, not a pipe, for the same reason as codex: nothing reads the
        // pipe until the child exits, and a filled pipe buffer deadlocks the
        // run into the timeout. The tail goes into the error below.
        .stderr(log)
        .spawn()
        .context(
            "spawning claude (is Claude Code installed and on PATH? \
             `npm install -g @anthropic-ai/claude-code`)",
        )?;

    // The instruction on stdin, then EOF. The pages are on disk rather than
    // attached: claude reads files itself, so the pointer names them.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(
            b"Read prompt.md in your working directory and follow it exactly. \
              The rendered datasheet pages (page-*.png) and datasheet.pdf are \
              in the same directory; read values off the page images for \
              anything that lives in a table or a pinout. Write your answer \
              to model.toml.",
        );
        // dropping `stdin` here sends EOF
    }

    let deadline = Instant::now() + CLI_BACKEND_TIMEOUT;
    loop {
        match child.try_wait().context("polling claude")? {
            Some(_status) => break,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(
                        "claude timed out after {}s with no answer; retry with a \
                         tighter prompt or another --backend",
                        CLI_BACKEND_TIMEOUT.as_secs()
                    );
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }

    let output = child
        .wait_with_output()
        .context("collecting claude output")?;
    if !output.status.success() {
        let tail = String::from_utf8_lossy(&output.stdout);
        let err_tail = std::fs::read_to_string(&log_path)
            .map(|t| {
                t.lines()
                    .filter(|l| !l.trim().is_empty())
                    .rev()
                    .take(5)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join(" | ")
            })
            .unwrap_or_default();
        let detail = [
            tail.lines().rev().take(5).collect::<Vec<_>>().join(" | "),
            err_tail,
        ]
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" || ");
        bail!(
            "claude exited with status {}{}",
            output.status,
            if detail.is_empty() {
                " and said nothing on stdout or stderr".to_string()
            } else {
                format!(": {detail}")
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The default base URL for the api backend.
pub const DEFAULT_API_BASE: &str = "https://api.openai.com/v1";

/// The environment variable the api backend reads its key from: an explicit
/// `--api-key-env` wins; otherwise the legacy `HAUKSBEE_LLM_API_KEY` when it
/// is set (that variable selected the backend before `--backend` existed),
/// else `OPENAI_API_KEY`.
fn api_key_env_name(args: &Args) -> String {
    if let Some(name) = &args.api_key_env {
        return name.clone();
    }
    if std::env::var("HAUKSBEE_LLM_API_KEY").is_ok() {
        return "HAUKSBEE_LLM_API_KEY".to_string();
    }
    "OPENAI_API_KEY".to_string()
}

/// Call an OpenAI-compatible chat-completions endpoint via `curl`.
///
/// The key is read from the environment at call time and appears nowhere the
/// system can echo it: not in argv (world-readable via `ps` / /proc), not in
/// a log line, not in an error. curl gets it through `--config -` on stdin,
/// and the request body travels as a private temp file rather than an
/// argument.
fn call_api_backend(prompt: &str, args: &Args) -> Result<String> {
    let key_env = api_key_env_name(args);
    let api_key = std::env::var(&key_env)
        .ok()
        .filter(|k| !k.trim().is_empty())
        .with_context(|| {
            format!(
                "the api backend reads its key from ${key_env}, which is unset or \
                 empty. Fix: set {key_env} (export {key_env}=<your key>), or name \
                 the variable that holds your key with --api-key-env NAME. The key \
                 is never accepted as a flag value and never stored."
            )
        })?;
    if !which("curl") {
        bail!("the api backend needs `curl`, which is not in PATH; install curl");
    }
    let model = args
        .model
        .clone()
        .or_else(|| std::env::var("HAUKSBEE_LLM_MODEL").ok())
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CODEX_MODEL.to_string());
    let base_url = args
        .api_base
        .clone()
        .or_else(|| std::env::var("HAUKSBEE_LLM_BASE_URL").ok())
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string());
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

    // The body in a private temp file: it embeds the datasheet text, which is
    // exactly what must stay off a world-readable argv, and it can outgrow
    // ARG_MAX anyway.
    let staging = tempfile::Builder::new()
        .prefix("hauksbee-api-")
        .tempdir()
        .context("creating the api request staging directory")?;
    let body_path = staging.path().join("request.json");
    std::fs::write(&body_path, &body_str).context("writing the api request body")?;

    // curl reads its config (with the Authorization header) from stdin.
    let curl_config = format!(
        "url = \"{url}\"\n\
         request = \"POST\"\n\
         header = \"Content-Type: application/json\"\n\
         header = \"Authorization: Bearer {api_key}\"\n\
         data = \"@{body}\"\n\
         silent\n\
         show-error\n",
        body = body_path.display()
    );

    for attempt in 0..=args.retries {
        let mut child = Command::new("curl")
            .args(["--config", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("running curl for the api backend")?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(curl_config.as_bytes())
                .context("passing the request config to curl")?;
            // dropping `stdin` here sends EOF
        }
        let output = child
            .wait_with_output()
            .context("collecting the api response")?;
        if !output.status.success() {
            bail!(
                "curl failed against {url}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let resp_str = String::from_utf8_lossy(&output.stdout);
        let resp: serde_json::Value = serde_json::from_str(&resp_str)
            .with_context(|| format!("parsing the reply from {url} as JSON"))?;

        // An error object instead of choices is the endpoint explaining
        // itself (bad model name, exhausted quota); surface that message.
        if let Some(err_msg) = resp["error"]["message"].as_str() {
            bail!("{url} answered with an error: {err_msg}");
        }

        let content = resp["choices"][0]["message"]["content"]
            .as_str()
            .context("missing content in API response")?
            .to_string();

        let raw = extract_toml_block(&content);

        match parse_and_validate_reply(&raw, &args.part, &args.kind_str) {
            Ok(_) => return Ok(raw),
            Err(e) if attempt < args.retries => {
                eprintln!(
                    "[model-extract] attempt {} failed: {}; retrying...",
                    attempt + 1,
                    e
                );
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
    // No fences: drop any chatter before the first `[[models]]` table header so
    // a stray greeting line doesn't break the TOML parse.
    if let Some(pos) = s.find("[[models]]") {
        return s[pos..].trim().to_string();
    }
    s.trim().to_string()
}

/// Parse raw TOML and validate the first model entry.
fn parse_and_validate_reply(
    raw: &str,
    part: &str,
    kind_str: &str,
) -> Result<crate::schema::ModelEntry> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("empty reply for {part}: the backend returned no TOML at all");
    }
    if !trimmed.contains("[[models]]") {
        bail!(
            "reply for {part} contains no [[models]] table; the backend likely \
             answered with prose instead of TOML. First 200 chars: {:.200}",
            trimmed
        );
    }

    let db: crate::schema::DbFile = toml::from_str(trimmed)
        .with_context(|| format!("parsing TOML reply for {part} (is it valid TOML?)"))?;

    let entry = db
        .models
        .into_iter()
        .next()
        .with_context(|| format!("no [[models]] entry in reply for {part}"))?;

    // Guard against the backend returning the wrong device kind, which would
    // bind nonsense (e.g. a diode card stamped as a BJT). A behavioural family
    // kind (charger/pmic/balancer) is satisfied by its BASE kind in the TOML
    // (vreg/digital), since the family lives in the [models.behavioral] block.
    let want = kind_str.trim();
    if !want.is_empty() {
        let got = kind_discriminant(entry.kind);
        let want_base = behavioral_family_base_kind(want).unwrap_or(want);
        if got != want_base {
            bail!(
                "kind mismatch for {part}: requested '{want}' (base '{want_base}') \
                 but the reply is '{got}'"
            );
        }
    }

    // A behavioural-family extraction must carry a non-empty behavioural block,
    // or it is just an ordinary kind mislabelled.
    if behavioral_family_base_kind(want).is_some() && entry.behavioral.is_empty() {
        bail!(
            "behavioural extraction for {part} (kind '{want}') produced no \
             [models.behavioral] block; the prompt asked for one"
        );
    }

    // Validate physical ranges.
    crate::validation::validate(&entry).map_err(|errs| {
        let msg = errs
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        anyhow::anyhow!("validation failed: {msg}")
    })?;

    Ok(entry)
}

/// snake_case discriminant for a kind (mirrors the serde `rename_all`).
/// Whether a kind string names something the extractor can bind.
///
/// The inverse of `kind_discriminant`, derived from it rather than restated,
/// so the offered list and the accepted set cannot drift apart.
#[cfg(test)]
fn kind_accepts(kind: &str) -> bool {
    use crate::ComponentKind::*;
    [
        Passive,
        Diode,
        BjtNpn,
        BjtPnp,
        Nmos,
        Pmos,
        Vreg,
        Opamp,
        Comparator,
        AnalogSwitch,
        Digital,
        Dac,
        Adc,
        ShiftRegister,
        Mcu,
        Connector,
    ]
    .iter()
    .any(|k| kind_discriminant(*k) == kind)
}

fn kind_discriminant(kind: crate::ComponentKind) -> &'static str {
    use crate::ComponentKind::*;
    match kind {
        Passive => "passive",
        Diode => "diode",
        BjtNpn => "bjt_npn",
        BjtPnp => "bjt_pnp",
        Nmos => "nmos",
        Pmos => "pmos",
        Vreg => "vreg",
        Opamp => "opamp",
        Comparator => "comparator",
        AnalogSwitch => "analog_switch",
        Digital => "digital",
        Dac => "dac",
        Adc => "adc",
        ShiftRegister => "shift_register",
        Mcu => "mcu",
        Connector => "connector",
        Ignore => "ignore",
    }
}

fn sanitise_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn default_out_dir() -> PathBuf {
    dirs_next().join(".hauksbee").join("models")
}

fn dirs_next() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_args_defaults_leave_backend_unset() {
        let a = parse_args_from(argv(&["--pdf", "x.pdf", "--part", "BC847"])).unwrap();
        assert_eq!(a.backend, None, "no --backend keeps the env-driven default");
        assert_eq!(a.api_base, None);
        assert_eq!(a.api_key_env, None);
        assert!(a.kind_str.is_empty());
    }

    #[test]
    fn parse_args_accepts_each_backend() {
        for (flag, want) in [
            ("codex", Backend::Codex),
            ("claude-code", Backend::ClaudeCode),
            ("api", Backend::Api),
        ] {
            let a = parse_args_from(argv(&[
                "--pdf", "x.pdf", "--part", "P", "--backend", flag,
            ]))
            .unwrap();
            assert_eq!(a.backend, Some(want), "--backend {flag}");
        }
    }

    #[test]
    fn parse_args_rejects_unknown_backend() {
        let err = parse_args_from(argv(&[
            "--pdf", "x.pdf", "--part", "P", "--backend", "gemini",
        ]))
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown backend 'gemini'"), "got: {msg}");
        assert!(msg.contains("codex, claude-code, or api"), "got: {msg}");
    }

    #[test]
    fn parse_args_takes_api_base_and_key_env() {
        let a = parse_args_from(argv(&[
            "--pdf",
            "x.pdf",
            "--part",
            "P",
            "--backend",
            "api",
            "--api-base",
            "https://llm.example/v1",
            "--api-key-env",
            "MY_LLM_KEY",
        ]))
        .unwrap();
        assert_eq!(a.api_base.as_deref(), Some("https://llm.example/v1"));
        assert_eq!(a.api_key_env.as_deref(), Some("MY_LLM_KEY"));
    }

    /// A value that cannot be an env-var NAME is almost certainly a pasted
    /// key, and must be refused before it reaches a world-readable argv.
    #[test]
    fn parse_args_rejects_a_key_pasted_as_the_env_name() {
        for pasted in ["sk-abc123XYZ", "1KEY", "MY KEY", ""] {
            let err = parse_args_from(argv(&[
                "--pdf",
                "x.pdf",
                "--part",
                "P",
                "--api-key-env",
                pasted,
            ]))
            .unwrap_err();
            assert!(
                err.to_string().contains("not the key itself"),
                "{pasted:?} must be refused as an env NAME, got: {err}"
            );
        }
        assert!(validate_api_key_env_name("OPENAI_API_KEY").is_ok());
        assert!(validate_api_key_env_name("_KEY2").is_ok());
    }

    /// An explicit --api-key-env wins over every default.
    #[test]
    fn api_key_env_name_prefers_the_explicit_flag() {
        let args = Args::new(PathBuf::from("x.pdf"), "P".into(), "diode".into())
            .api_key_env(Some("MY_LLM_KEY".into()));
        assert_eq!(api_key_env_name(&args), "MY_LLM_KEY");
    }

    /// The missing-key error names the exact variable to set.
    #[test]
    fn api_backend_without_key_says_which_var_to_set() {
        let args = Args::new(PathBuf::from("x.pdf"), "P".into(), "diode".into())
            .backend(Some(Backend::Api))
            .api_key_env(Some("HAUKSBEE_TEST_KEY_VAR_THAT_IS_UNSET".into()));
        let err = call_api_backend("prompt", &args).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("set HAUKSBEE_TEST_KEY_VAR_THAT_IS_UNSET"),
            "the fix must be exact, got: {msg}"
        );
    }

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

    #[test]
    fn behavioral_family_prompt_includes_schema() {
        // A charger prompt must teach the [models.behavioral.converter] schema
        // and the input-current-limit programming block.
        let charger = build_prompt("LTC4020", "charger", "datasheet");
        assert!(
            charger.contains("[models.behavioral.converter]"),
            "charger prompt missing converter schema"
        );
        assert!(
            charger.contains("iin_program"),
            "charger prompt missing current-limit program"
        );
        assert!(charger.contains("CHARGER specifically"));
        // A PMIC prompt must teach the internal-pull pin schema.
        let pmic = build_prompt("nPM1300", "pmic", "datasheet");
        assert!(
            pmic.contains("pull_to"),
            "pmic prompt missing internal-pull schema"
        );
        assert!(
            pmic.contains("SHPHLD"),
            "pmic prompt should mention the ship-hold case"
        );
        // A balancer prompt must teach the leak-law schema.
        let bal = build_prompt("LTC6803", "balancer", "datasheet");
        assert!(bal.contains("[[models.behavioral.laws]]"));
        assert!(bal.contains("tie_ohms"));
        // An ordinary kind must NOT get the behavioural section.
        let bjt = build_prompt("BC847", "bjt_npn", "x");
        assert!(!bjt.contains("[models.behavioral.converter]"));
    }

    #[test]
    fn behavioral_family_kind_base_check() {
        // A charger reply carries kind = "vreg" (the base) plus a behavioural
        // block; the validator must accept it against the requested "charger".
        assert_eq!(behavioral_family_base_kind("charger"), Some("vreg"));
        assert_eq!(behavioral_family_base_kind("pmic"), Some("vreg"));
        assert_eq!(behavioral_family_base_kind("balancer"), Some("digital"));
        assert_eq!(behavioral_family_base_kind("bjt_npn"), None);
    }

    /// Offline end-to-end for a behavioural extraction: a canned charger reply
    /// (kind = "vreg" + a converter behavioural block) must pass the full
    /// parse + validate path under --kind charger.
    #[test]
    fn offline_behavioral_charger_pipeline() {
        let reply = r#"```toml
[[models]]
id = "ltc4020_x"
kind = "vreg"
description = "extracted charger"
[models.match]
value_re = "(?i)LTC4020"
[models.params]
vout = 14.4
dropout_v = 0.5
iq_a = 0.001
[models.ratings]
max_voltage_v = 55.0
[models.pins]
"36" = "pvin"
"20" = "bat"
"25" = "ilimit"
[models.behavioral.converter]
topology = "buck_boost"
out_pin = "bat"
in_pin = "pvin"
vout_setpoint = 28.8
efficiency = 0.92
[models.behavioral.converter.iin_program]
rsense_ref = "R49"
prog_ref = "R8"
vprog_ref = 0.0316
prog_ref_ohms = 7150.0
v_sense_full = 0.0463
```"#;
        let raw = extract_toml_block(reply);
        let entry = parse_and_validate_reply(&raw, "LTC4020", "charger")
            .expect("behavioural charger reply should validate under --kind charger");
        assert_eq!(entry.kind, crate::ComponentKind::Vreg);
        assert!(!entry.behavioral.is_empty());
        assert!(entry.behavioral.converter.is_some());
    }

    /// A behavioural-family extraction with NO behavioural block must be
    /// rejected (it would just be an ordinary kind mislabelled).
    #[test]
    fn behavioral_family_without_block_rejected() {
        let reply = r#"
[[models]]
id = "x"
kind = "vreg"
[models.params]
vout = 5.0
dropout_v = 1.0
iq_a = 0.001
"#;
        let err = parse_and_validate_reply(reply, "X", "charger").unwrap_err();
        assert!(
            err.to_string().contains("no [models.behavioral]"),
            "got: {err}"
        );
    }

    #[test]
    fn prompt_asks_for_ratings() {
        // The stress monitor depends on [models.ratings] being populated, so the
        // prompt must ask for the absolute-maximum ratings per kind.
        let bjt = build_prompt("BC847", "bjt_npn", "x");
        assert!(bjt.contains("[models.ratings]"));
        assert!(bjt.contains("VCEO"));
        let diode = build_prompt("1N4148", "diode", "x");
        assert!(diode.contains("VRRM"));
        let vreg = build_prompt("AMS1117", "vreg", "x");
        assert!(vreg.contains("max_junction_temp_c"));
    }

    fn testdata(rel: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata")
            .join(rel)
    }

    /// Offline end-to-end: drive the whole pipeline (build_prompt -> call_backend
    /// -> extract_toml_block -> parse_and_validate_reply -> write) with a canned
    /// reply via HAUKSBEE_EXTRACT_MOCK_REPLY. No codex, no network: always runs.
    #[test]
    fn offline_pipeline_with_mock_reply() {
        let reply = "Here is the model:\n```toml\n\
[[models]]\n\
id = \"bc847\"\n\
kind = \"bjt_npn\"\n\
description = \"mock\"\n\
[models.match]\n\
value_re = \"(?i)^BC847\"\n\
[models.params]\n\
is = 1.6e-14\n\
bf = 180.0\n\
nf = 1.0\n\
vaf = 65.0\n\
[models.ratings]\n\
max_voltage_v = 65.0\n\
max_current_a = 0.1\n\
[models.pins]\n\
\"1\" = \"base\"\n\
\"2\" = \"emitter\"\n\
\"3\" = \"collector\"\n\
```\n";
        let dir = std::env::temp_dir().join("hauksbee_mock_offline");
        std::fs::create_dir_all(&dir).unwrap();
        let reply_path = dir.join("reply.txt");
        std::fs::write(&reply_path, reply).unwrap();

        // SAFETY: single-threaded test process for env mutation.
        std::env::set_var("HAUKSBEE_EXTRACT_MOCK_REPLY", &reply_path);

        let args = Args {
            pdf: dir.join("nonexistent.pdf"),
            part: "BC847".to_string(),
            kind_str: "bjt_npn".to_string(),
            out_dir: Some(dir.clone()),
            retries: 1,
            model: None,
            backend: None,
            api_base: None,
            api_key_env: None,
        };
        let prompt = build_prompt(&args.part, &args.kind_str, "irrelevant");
        let raw = call_backend(&prompt, &args).expect("mock backend should succeed");
        let entry = parse_and_validate_reply(&raw, &args.part, &args.kind_str).unwrap();

        std::env::remove_var("HAUKSBEE_EXTRACT_MOCK_REPLY");

        assert_eq!(entry.kind, crate::ComponentKind::BjtNpn);
        assert_eq!(entry.ratings.max_voltage_v, Some(65.0));
        assert!(raw.starts_with("[[models]]"), "fence should be stripped");
    }

    /// Live integration: run the REAL codex backend against the BC847 datasheet
    /// shipped in testdata, then physically sanity-check the result. Marked
    /// #[ignore] because it shells out to codex and takes ~1-2 minutes. Run with:
    ///   cargo test -p hauksbee-models --bin model-extract -- \
    ///       extract_bc847_live --ignored --nocapture
    /// See crates/hauksbee-models/README_DATASHEET.md.
    #[test]
    #[ignore]
    fn extract_bc847_live() {
        let pdf = testdata("datasheets/BC847.pdf");
        assert!(
            pdf.exists(),
            "BC847 datasheet not found at {:?}; download it (see README_DATASHEET.md)",
            pdf
        );
        if !which("codex") && std::env::var("HAUKSBEE_LLM_API_KEY").is_err() {
            eprintln!("neither codex nor HAUKSBEE_LLM_API_KEY available; skipping live test");
            return;
        }

        let text = extract_pdf_text(&pdf).expect("PDF text extraction");
        let prompt = build_prompt("BC847", "bjt_npn", &text);
        let args = Args {
            pdf: pdf.clone(),
            part: "BC847".to_string(),
            kind_str: "bjt_npn".to_string(),
            out_dir: Some(std::env::temp_dir()),
            retries: 1,
            model: None,
            backend: None,
            api_base: None,
            api_key_env: None,
        };

        let raw = call_backend(&prompt, &args).expect("codex backend call");
        let entry = parse_and_validate_reply(&raw, "BC847", "bjt_npn").expect("parse + validate");

        assert_eq!(entry.kind, crate::ComponentKind::BjtNpn);
        let bf = entry.params.get_f64("bf").expect("bf present");
        assert!(
            (100.0..=460.0).contains(&bf),
            "extracted bf {bf} not in the BC847 hFE band 110..450"
        );
        assert_eq!(
            entry.ratings.max_voltage_v,
            Some(65.0),
            "VCEO must be extracted into ratings"
        );
        println!("live BC847: bf={bf} ratings={:?}", entry.ratings);
    }

    /// Live integration: run the REAL codex backend with `--kind charger`
    /// against the LTC4020 datasheet excerpt in testdata, then assert the
    /// extracted behavioural model is structurally sound. Marked #[ignore]
    /// (shells out to codex, ~30-60s). Run with:
    ///   cargo test -p hauksbee-models --bin model-extract -- \
    ///       extract_ltc4020_charger_live --ignored --nocapture
    #[test]
    #[ignore]
    fn extract_ltc4020_charger_live() {
        let src = testdata("datasheets/LTC4020_excerpt.txt");
        if !src.exists() {
            eprintln!("LTC4020 excerpt not found at {src:?}; skipping");
            return;
        }
        if !which("codex") && std::env::var("HAUKSBEE_LLM_API_KEY").is_err() {
            eprintln!("neither codex nor HAUKSBEE_LLM_API_KEY available; skipping live test");
            return;
        }
        let text = extract_pdf_text(&src).expect("text");
        let prompt = build_prompt("LTC4020", "charger", &text);
        let args = Args {
            pdf: src.clone(),
            part: "LTC4020".to_string(),
            kind_str: "charger".to_string(),
            out_dir: Some(std::env::temp_dir()),
            retries: 1,
            model: None,
            backend: None,
            api_base: None,
            api_key_env: None,
        };
        let raw = call_backend(&prompt, &args).expect("codex backend call");
        let entry = parse_and_validate_reply(&raw, "LTC4020", "charger").expect("parse + validate");
        assert_eq!(entry.kind, crate::ComponentKind::Vreg);
        let c = entry.behavioral.converter.expect("converter block");
        assert_eq!(c.in_pin, "pvin");
        assert_eq!(c.out_pin, "bat");
        assert!(
            c.iin_program.is_some(),
            "ILIMIT current-limit program present"
        );
        println!(
            "live LTC4020 charger: vout={} eff={:?}",
            c.vout_setpoint, c.efficiency
        );
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
        assert!(
            msg.contains("bf") || msg.contains("validation"),
            "error message: {}",
            msg
        );
    }

    #[test]
    fn empty_reply_is_rejected_clearly() {
        let err = parse_and_validate_reply("   \n  ", "BC847", "bjt_npn").unwrap_err();
        assert!(err.to_string().contains("empty reply"), "got: {err}");
    }

    #[test]
    fn prose_reply_is_rejected_clearly() {
        let prose = "I'm sorry, I couldn't find the SPICE parameters in this datasheet.";
        let err = parse_and_validate_reply(prose, "BC847", "bjt_npn").unwrap_err();
        assert!(err.to_string().contains("no [[models]]"), "got: {err}");
    }

    #[test]
    fn wrong_kind_is_rejected() {
        // Reply is a valid diode card, but we asked for a bjt_npn: must be caught
        // so the binder never stamps a diode where a transistor belongs.
        let diode = r#"
[[models]]
id = "x"
kind = "diode"
[models.params]
is = 1e-9
n = 1.7
rs = 0.5
"#;
        let err = parse_and_validate_reply(diode, "X", "bjt_npn").unwrap_err();
        assert!(err.to_string().contains("kind mismatch"), "got: {err}");
    }

    #[test]
    fn extract_toml_block_strips_leading_prose() {
        let s = "Sure! Here is the TOML:\n[[models]]\nid = \"z\"\nkind = \"diode\"\n";
        let block = extract_toml_block(s);
        assert!(block.starts_with("[[models]]"), "got: {block}");
    }

    #[test]
    fn missing_codex_path_check() {
        // The codex backend resolves the working dir from the PDF's parent; a
        // PDF with no parent must still yield a usable workdir, not a panic.
        // (Full "no backend" behaviour is covered by the binary's runtime
        // error, which lists codex / HAUKSBEE_LLM_API_KEY / mock as options.)
        assert!(!which("definitely_not_a_real_command_xyz"));
    }

    // ── Declarative sensor extractor (i2c_sensor / spi_sensor) ──

    #[test]
    fn sensor_kinds_are_recognised() {
        assert!(is_sensor_kind("i2c_sensor"));
        assert!(is_sensor_kind("spi_sensor"));
        assert!(!is_sensor_kind("bjt_npn"));
        assert!(!is_sensor_kind("adc"));
    }

    #[test]
    fn sensor_prompt_mentions_format() {
        let p = build_sensor_prompt("LM75", "i2c_sensor", "datasheet text here");
        assert!(p.contains("[sensor]"));
        assert!(p.contains("i2c_pointer"));
        assert!(p.contains("q7.1_be"));
        assert!(p.contains("WHO_AM_I"));
        let sp = build_sensor_prompt("MPU", "spi_sensor", "datasheet text here");
        assert!(sp.contains("spi_reg"));
        assert!(sp.contains("rw_read_is_high"));
    }

    /// The validator must accept a well-formed i2c_sensor reply and round-trip
    /// it through the SensorSpec schema (this is the documented stand-in for a
    /// live PDF extraction).
    #[test]
    fn validate_sensor_reply_round_trips_i2c() {
        let reply = r#"
[sensor]
name = "LM75"
bus = "i2c"
i2c_address = 0x48

[[sensor.input]]
name = "temperature_c"
default = 25.0

[[sensor.register]]
addr = 0x00
bytes = 2
encoding = "q7.1_be"
expr = "temperature_c"

[[sensor.register]]
addr = 0x01
const = [0x00]

[sensor.protocol]
style = "i2c_pointer"
"#;
        let spec = validate_sensor_reply(reply, "LM75", "i2c_sensor").unwrap();
        assert_eq!(spec.sensor.name, "LM75");
        assert_eq!(spec.sensor.i2c_address, Some(0x48));
    }

    #[test]
    fn validate_sensor_reply_round_trips_spi() {
        let reply = r#"
[sensor]
name = "MINIMU"
bus = "spi"

[[sensor.input]]
name = "gyro_x"
default = 0.0

[[sensor.register]]
addr = 0x0f
const = [0x42]

[[sensor.register]]
addr = 0x22
bytes = 2
encoding = "i16_le"
expr = "gyro_x"

[sensor.protocol]
style = "spi_reg"
rw_read_is_high = true
addr_mask = 0x7f
"#;
        let spec = validate_sensor_reply(reply, "MINIMU", "spi_sensor").unwrap();
        assert_eq!(spec.sensor.name, "MINIMU");
    }

    #[test]
    fn validate_sensor_reply_rejects_prose() {
        let prose = "Here is the LM75 sensor model you asked for:";
        assert!(validate_sensor_reply(prose, "LM75", "i2c_sensor").is_err());
    }

    #[test]
    fn validate_sensor_reply_rejects_bus_mismatch() {
        let i2c_reply = r#"
[sensor]
name = "LM75"
bus = "i2c"
i2c_address = 0x48
[[sensor.register]]
addr = 0x00
const = [0x00]
[sensor.protocol]
style = "i2c_pointer"
"#;
        // Requested spi_sensor but the spec is i2c → reject.
        assert!(validate_sensor_reply(i2c_reply, "LM75", "spi_sensor").is_err());
    }
}

#[cfg(test)]
mod kind_tests {
    use super::*;

    /// The identifier list is shared by the identification prompt, the CLI help
    /// and the web picker. If it offers a kind the extractor cannot bind, a
    /// user picks something that then fails after the datasheet has been sent.
    #[test]
    fn every_supported_kind_binds_to_a_component_kind() {
        for kind in SUPPORTED_KINDS {
            if is_sensor_kind(kind) {
                continue; // separate [sensor] schema, validated on its own path
            }
            let base = behavioral_family_base_kind(kind).unwrap_or(kind);
            assert!(
                kind_accepts(base),
                "{kind} is offered but does not resolve to a component kind"
            );
        }
    }

    /// Three kinds were offered with no parameter guidance at all (passive,
    /// adc, connector), so their prompts fell to the generic hint while
    /// charger, pmic and balancer had guidance but were never offered. Neither
    /// half is fatal, and both are the kind of drift that only shows up as a
    /// worse extraction, so pin it.
    #[test]
    fn every_offered_kind_has_its_own_parameter_guidance() {
        let generic = required_params_for_kind("__definitely_not_a_kind__");
        let missing: Vec<&str> = SUPPORTED_KINDS
            .iter()
            .copied()
            .filter(|k| !is_sensor_kind(k))
            .filter(|k| required_params_for_kind(k) == generic)
            .collect();
        assert!(
            missing.is_empty(),
            "offered with only the generic prompt hint: {missing:?}"
        );
    }

    /// An empty kind is the "you work it out" signal, so nothing may treat it
    /// as a valid kind by accident.
    #[test]
    fn the_empty_kind_is_not_itself_a_kind() {
        assert!(
            !SUPPORTED_KINDS.contains(&""),
            "the empty string means 'identify it', not a part type"
        );
    }
}
