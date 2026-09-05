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
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::schema::{ComponentKind, ModelEntry};
use crate::sensor_spec::{Bus, SensorSpec};

/// How long to let a single agent-CLI run (codex or claude) go before we kill
/// it and (maybe) retry.
///
/// Keep this fixed rather than scaling it with rendered-page count. Page count
/// is a poor proxy for extraction work (one dense pin table can take longer
/// than several simple pages), while a model that has produced nothing after
/// ten minutes is more likely stuck than productively reading page fourteen.
/// Scaling upward would make the observed no-answer failure slower without
/// evidence that it improves card quality; scaling downward would penalise
/// short but difficult datasheets. `MAX_RENDERED_PAGES` already bounds input.
const CLI_BACKEND_TIMEOUT: Duration = Duration::from_secs(600);

/// Pages rendered and attached as images.
///
/// Seven letter-sized pages at 200 DPI contain fewer pixels than fourteen at
/// 150 DPI. Selecting those seven from the text layer preserves that request
/// budget while making table subscripts and footnotes easier to read.
const MAX_RENDERED_PAGES: usize = 7;

/// Render resolution, in DPI. Enough to read a small table footnote, which is
/// often exactly where the condition a value was measured under is hiding.
const RENDER_DPI: u32 = 200;

#[derive(Debug, Clone)]
struct PdfPageText {
    number: usize,
    text: String,
}

#[derive(Debug, Clone)]
struct SelectedPage {
    number: usize,
    reasons: Vec<&'static str>,
    supplemental_reasons: Vec<&'static str>,
    score: usize,
}

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
    selected_pages: Vec<SelectedPage>,
    category_disclosures: Vec<String>,
    has_text_dump: bool,
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

    let page_text = extract_pdf_pages_text(&copied).ok();
    let has_text_dump = page_text.as_ref().is_some_and(|pages| {
        let dump = pages
            .iter()
            .map(|page| page.text.as_str())
            .collect::<Vec<_>>()
            .join("\u{000c}");
        std::fs::write(dir.path().join("datasheet.txt"), dump).is_ok()
    });
    let selections = page_text
        .as_deref()
        .map(|pages| select_relevant_pages(pages, MAX_RENDERED_PAGES))
        .unwrap_or_else(|| {
            (1..=MAX_RENDERED_PAGES)
                .map(|number| SelectedPage {
                    number,
                    reasons: vec!["front-page fallback because the PDF text layer was unavailable"],
                    supplemental_reasons: Vec::new(),
                    score: 0,
                })
                .collect()
        });

    // Page renders are best-effort. Without poppler the extraction still runs
    // on text alone, so a missing optional tool degrades the result rather
    // than failing the command.
    let mut pages = Vec::new();
    let mut selected_pages = Vec::new();
    if which("pdftoppm") {
        for selection in selections {
            let stem = dir.path().join(format!("page-{:03}", selection.number));
            let page = selection.number.to_string();
            let out = Command::new("pdftoppm")
                .args([
                    "-png",
                    "-r",
                    &RENDER_DPI.to_string(),
                    "-f",
                    &page,
                    "-l",
                    &page,
                ])
                .arg("-singlefile")
                .arg(&copied)
                .arg(&stem)
                .output();
            match out {
                Ok(o) if o.status.success() => {
                    let rendered = stem.with_extension("png");
                    if rendered.is_file() {
                        pages.push(rendered);
                        selected_pages.push(selection);
                    }
                }
                Ok(o) => {
                    eprintln!(
                    "[model-extract] PDF page {page} failed to render, continuing without it: {}",
                    String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or("")
                )
                }
                Err(_) => {}
            }
        }
    } else {
        eprintln!(
            "[model-extract] pdftoppm not found, so the model sees text only. Install poppler \
             for page images: the pinout and the ratings table survive a render far better \
             than a text dump."
        );
    }

    let category_disclosures = page_text
        .as_deref()
        .map(|pages| category_disclosures(pages, &selected_pages))
        .unwrap_or_default();

    Ok(Workspace {
        pdf: copied,
        pages,
        selected_pages,
        category_disclosures,
        has_text_dump,
        dir,
    })
}

/// Run one extraction end to end, as the CLI does.
pub fn run(args: Args) -> Result<PathBuf> {
    // Magic bytes rather than extension, and BEFORE any backend is chosen: a
    // non-PDF (a saved HTML page, a .docx) would otherwise be text-dumped and
    // shipped to the LLM, producing a confidently wrong model; a downloaded
    // datasheet with no extension still passes.
    let mut head = [0u8; 5];
    let is_pdf = std::fs::File::open(&args.pdf)
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head))
        .is_ok_and(|()| &head == b"%PDF-");
    if !is_pdf {
        bail!(
            "'{}' is not a PDF (no %PDF header); the extractor reads PDF datasheets only. \
             Nothing was sent.",
            args.pdf.display()
        );
    }
    let pdf_text = extract_pdf_text(&args.pdf)?;

    // An empty kind means "you work it out". Being made to classify a part
    // before the tool will look at it is a barrier at exactly the wrong
    // moment: the datasheet says what the part is on its first page, and the
    // person asking for a model is precisely the one who may not know which
    // of our categories their part falls into.
    let mut args = args;
    if args.kind_str.trim().is_empty() {
        args.kind_str = identify_kind(&args, &pdf_text)?;
        eprintln!(
            "[model-extract] identified {} as kind '{}'",
            args.part, args.kind_str
        );
    }
    eprintln!(
        "[model-extract] part={} kind={} pdf={}",
        args.part,
        args.kind_str,
        args.pdf.display()
    );

    let (part, kind) = (args.part.as_str(), args.kind_str.as_str());
    // Declarative register-map sensor kinds emit a `[sensor]` spec validated
    // as a `SensorSpec`, NOT the SPICE `[[models]]` schema.
    let (raw, id, suffix) = if is_sensor_kind(kind) {
        let prompt = build_sensor_prompt(part, kind, &pdf_text);
        let raw = call_backend(&prompt, &args, Reply::Sensor { part, kind })?;
        let name = validate_sensor_reply(&raw, part, kind)?.sensor.name;
        (raw, name, ".sensor.toml")
    } else {
        // An explicit kind the schema cannot accept fails HERE, before any
        // datasheet text is sent anywhere. A behavioural FAMILY (charger/pmic
        // on vreg, balancer on digital) is legal: the prompt appends the
        // `[models.behavioral]` guidance and the draft comes back under the
        // base kind, which the schema accepts.
        if !kind_is_legal(kind) && behavioral_family_base_kind(kind).is_none() {
            bail!(
                "hauksbee has no '{kind}' model kind. Legal kinds: {}; behavioral families \
                 (drafted on a base kind with a [models.behavioral] block): charger, pmic, \
                 balancer. Pick the closest and declare the unmodeled behavior in the \
                 description, or omit --kind and let the extraction choose.",
                legal_kinds().join(", ")
            );
        }
        let prompt = build_prompt(part, kind, &pdf_text);
        let raw = call_backend(&prompt, &args, Reply::Model { part, kind })?;
        let id = parse_and_validate_reply(&raw, part, kind)?.id;
        (raw, id, ".toml")
    };

    let out_dir = args.out_dir.clone().unwrap_or_else(default_out_dir);
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("creating output directory {}", out_dir.display()))?;
    let out_path = out_dir.join(format!("{}{suffix}", sanitise_filename(part)));
    std::fs::write(&out_path, &raw)
        .with_context(|| format!("writing output to {}", out_path.display()))?;
    println!("[model-extract] written: {}", out_path.display());
    println!("{id}");
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

/// Validate a sensor-spec reply: it must carry a `[sensor]` table, parse as a
/// `SensorSpec` (which validates structure), round-trip losslessly through
/// TOML, and agree with the requested bus.
fn validate_sensor_reply(raw: &str, part: &str, kind: &str) -> Result<SensorSpec> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("empty reply for {part}: the backend returned no TOML at all");
    }
    if !trimmed.contains("[sensor]") {
        bail!(
            "reply for {part} contains no [sensor] table; the backend likely answered with \
             prose instead of TOML. First 200 chars: {trimmed:.200}"
        );
    }
    let spec = SensorSpec::from_toml(trimmed)
        .with_context(|| format!("parsing/validating sensor spec for {part}"))?;
    let back = spec
        .to_toml()
        .with_context(|| format!("serialising sensor spec for {part}"))?;
    SensorSpec::from_toml(&back)
        .with_context(|| format!("round-trip re-parse of sensor spec for {part} failed"))?;
    let want_bus = if kind.trim() == "spi_sensor" {
        Bus::Spi
    } else {
        Bus::I2c
    };
    if spec.sensor.bus != want_bus {
        bail!(
            "bus mismatch for {part}: requested '{kind}' but the spec declares bus = {:?}",
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
            retries: 2,
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
/// The flag parser, over any argument source so tests can drive it without a
/// process spawn. `--help` still exits: it is a terminal answer, not a value.
pub fn parse_args_from(args: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut args = args.into_iter();
    // An empty kind means "the model works it out from the datasheet";
    // defaulting to bjt_npn silently produced a transistor model for whatever
    // was handed in.
    let mut out = Args::new(PathBuf::new(), String::new(), String::new());
    let (mut pdf, mut part) = (None, None);
    while let Some(flag) = args.next() {
        if matches!(flag.as_str(), "--help" | "-h") {
            print_help();
            std::process::exit(0);
        }
        let mut value = |what: &str| {
            args.next()
                .with_context(|| format!("{flag} requires {what}"))
        };
        match flag.as_str() {
            "--pdf" => pdf = Some(PathBuf::from(value("a value")?)),
            "--part" => part = Some(value("a value")?),
            "--kind" => out.kind_str = value("a value")?,
            "--out-dir" => out.out_dir = Some(PathBuf::from(value("a value")?)),
            "--model" => out.model = Some(value("a value")?),
            "--backend" => {
                out.backend = Some(value("a value (codex, claude-code, or api)")?.parse()?)
            }
            "--api-base" => out.api_base = Some(value("a value")?),
            "--api-key-env" => {
                let name = value("an environment variable NAME")?;
                validate_api_key_env_name(&name)?;
                out.api_key_env = Some(name);
            }
            other => bail!("unknown argument: {other}"),
        }
    }
    out.pdf = pdf.context("--pdf is required")?;
    out.part = part.context("--part is required")?;
    Ok(out)
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

/// The PDF's text layer via poppler's `pdftotext` (`-layout` keeps table
/// columns aligned, which page selection wants and the prompt does not).
fn pdftotext(path: &Path, layout: bool) -> Result<String> {
    if !which("pdftotext") {
        bail!("pdftotext is unavailable");
    }
    let mut cmd = Command::new("pdftotext");
    if layout {
        cmd.arg("-layout");
    }
    let output = cmd
        .arg(path)
        .arg("-")
        .output()
        .context("running pdftotext")?;
    if !output.status.success() {
        bail!(
            "pdftotext failed: {}",
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or("unknown error")
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The prompt's datasheet text. Without poppler (or a text layer) the
/// extraction still runs: the agent backends read the PDF themselves, so the
/// placeholder tells them to.
fn extract_pdf_text(path: &Path) -> Result<String> {
    if let Ok(text) = pdftotext(path, false) {
        if !text.trim().is_empty() {
            return Ok(truncate_to_chars(&text, 60_000));
        }
    }
    eprintln!("[model-extract] pdftotext unavailable or produced no text; LLM backend will read the PDF directly");
    Ok(format!(
        "<pdf_path>{}</pdf_path>\n\
         [Note: pdftotext not available. The LLM should read the PDF at the path above directly.]",
        path.display()
    ))
}

fn extract_pdf_pages_text(path: &Path) -> Result<Vec<PdfPageText>> {
    let text = pdftotext(path, true)?;
    let pages: Vec<PdfPageText> = text
        .split('\u{000c}')
        .enumerate()
        .filter(|(index, page)| *index == 0 || !page.is_empty())
        .map(|(index, page)| PdfPageText {
            number: index + 1,
            text: page.to_string(),
        })
        .collect();
    if pages.is_empty() {
        bail!("the PDF text layer contained no pages");
    }
    Ok(pages)
}

const PAGE_TOPICS: &[(&str, &[&str])] = &[
    (
        "recommended operating conditions",
        &["recommended operating conditions"],
    ),
    (
        "absolute maximum ratings",
        &["absolute maximum ratings", "limiting values"],
    ),
    ("switching characteristics", &["switching characteristics"]),
    (
        "electrical characteristics",
        &["electrical characteristics"],
    ),
    (
        "pin functions or package pinout",
        &[
            "pin functions",
            "terminal functions",
            "pin configuration",
            "package pinout",
            "top view",
        ],
    ),
    (
        "application information or programming equations",
        &[
            "application information",
            "typical application",
            "design procedure",
            "programming equation",
        ],
    ),
];

fn topic_score(text: &str, terms: &[&str]) -> usize {
    let lower = text.to_ascii_lowercase();
    let occurrences: usize = terms.iter().map(|term| lower.matches(term).count()).sum();
    if occurrences == 0 {
        return 0;
    }
    let heading = lower
        .lines()
        .filter(|line| !line.trim().is_empty())
        .any(|line| {
            let heading_text = line
                .trim()
                .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c.is_whitespace());
            let table_heading = heading_text
                .strip_prefix("table ")
                .and_then(|text| text.split_once(". ").map(|(_, title)| title));
            let detailed_heading = heading_text.strip_prefix("detailed ");
            terms.iter().any(|term| {
                heading_text.starts_with(term)
                    || table_heading.is_some_and(|title| title.starts_with(term))
                    || detailed_heading.is_some_and(|title| title.starts_with(term))
            })
        });
    if !heading {
        return 0;
    }
    let index_or_revision = (lower.contains("contents") && lower.matches(".....").count() >= 5)
        || lower.contains("revision history")
        || lower.contains("changes from revision")
        || lower.contains("change log");
    if index_or_revision {
        return 0;
    }
    occurrences * 100 + usize::from(heading) * 2_000
}

fn select_relevant_pages(pages: &[PdfPageText], limit: usize) -> Vec<SelectedPage> {
    if pages.is_empty() || limit == 0 {
        return Vec::new();
    }

    let mut selected = std::collections::BTreeMap::<usize, SelectedPage>::new();
    selected.insert(
        1,
        SelectedPage {
            number: 1,
            reasons: vec!["page 1 feature and supply summary"],
            supplemental_reasons: Vec::new(),
            score: usize::MAX,
        },
    );

    for (label, terms) in PAGE_TOPICS {
        // Repeated datasheet tables usually progress from the lowest bias to
        // the highest and most representative operating point. Prefer the
        // later page within each category, then disclose every sibling page so
        // the model can inspect another voltage or package variant if needed.
        let best = pages
            .iter()
            .map(|page| (page, topic_score(&page.text, terms)))
            .filter(|(_, score)| *score > 0)
            .max_by_key(|(page, _score)| page.number);
        if let Some((page, score)) = best {
            if let Some(chosen) = selected.get_mut(&page.number) {
                chosen.reasons.push(label);
                chosen.score = chosen.score.saturating_add(score);
            } else if selected.len() < limit {
                selected.insert(
                    page.number,
                    SelectedPage {
                        number: page.number,
                        reasons: vec![label],
                        supplemental_reasons: Vec::new(),
                        score,
                    },
                );
            }
        }
    }

    if selected.len() < limit {
        let mut remaining: Vec<SelectedPage> = pages
            .iter()
            .filter(|page| !selected.contains_key(&page.number))
            .filter_map(|page| {
                let reasons: Vec<&'static str> = PAGE_TOPICS
                    .iter()
                    .filter_map(|(label, terms)| {
                        (topic_score(&page.text, terms) > 0).then_some(*label)
                    })
                    .collect();
                let lower = page.text.to_ascii_lowercase();
                let score = reasons
                    .iter()
                    .map(|label| match *label {
                        "recommended operating conditions" | "absolute maximum ratings" => 80_000,
                        "switching characteristics" => 70_000,
                        "electrical characteristics" => 60_000,
                        "pin functions or package pinout" => 50_000,
                        "application information or programming equations" => 40_000,
                        _ => 0,
                    })
                    .max()
                    .unwrap_or(0)
                    + usize::from(!lower.contains("(continued)")) * 1_000
                    + page.number;
                (score > 0).then_some(SelectedPage {
                    number: page.number,
                    reasons: Vec::new(),
                    supplemental_reasons: reasons,
                    score,
                })
            })
            .collect();
        remaining.sort_by_key(|page| (std::cmp::Reverse(page.score), page.number));
        let mut supplemented = std::collections::HashSet::new();
        for page in remaining {
            if page
                .supplemental_reasons
                .iter()
                .all(|reason| supplemented.contains(reason))
            {
                continue;
            }
            for reason in &page.supplemental_reasons {
                supplemented.insert(*reason);
            }
            selected.insert(page.number, page);
            if selected.len() == limit {
                break;
            }
        }
    }

    let mut ranked: Vec<SelectedPage> = selected.into_values().collect();
    ranked.sort_by_key(|page| page.number);
    ranked
}

fn category_disclosures(pages: &[PdfPageText], selected: &[SelectedPage]) -> Vec<String> {
    PAGE_TOPICS
        .iter()
        .filter_map(|(label, terms)| {
            let matching: Vec<&PdfPageText> = pages
                .iter()
                .filter(|page| topic_score(&page.text, terms) > 0)
                .collect();
            let preferred = matching.last()?;
            let matching_numbers: Vec<usize> = matching.iter().map(|page| page.number).collect();
            let attached: Vec<usize> = matching_numbers
                .iter()
                .copied()
                .filter(|number| selected.iter().any(|page| page.number == *number))
                .collect();
            let omitted: Vec<usize> = matching_numbers
                .iter()
                .copied()
                .filter(|number| !attached.contains(number))
                .collect();
            let title = label
                .split_whitespace()
                .map(|word| {
                    let mut chars = word.chars();
                    chars
                        .next()
                        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(" ");

            if *label == "switching characteristics" {
                let headings: Vec<String> = matching
                    .iter()
                    .flat_map(|page| page.text.lines())
                    .map(str::trim)
                    .filter(|line| line.to_ascii_lowercase().contains("switching characteristics"))
                    .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
                    .collect();
                let operating_points: std::collections::BTreeSet<String> = headings
                    .iter()
                    .filter_map(|heading| {
                        let value = heading.split("VCCA =").nth(1)?;
                        Some(value.split(" (").next().unwrap_or(value).trim().to_string())
                    })
                    .collect();
                let package_summary = match (
                    headings.iter().any(|line| line.contains("(DRY)")),
                    headings.iter().any(|line| line.contains("(Other Packages)")),
                ) {
                    (true, true) => ", with DRY and Other Packages variants",
                    (true, false) => ", with DRY variants",
                    (false, true) => ", with Other Packages variants",
                    (false, false) => "",
                };
                let chosen_heading = preferred
                    .text
                    .lines()
                    .map(str::trim)
                    .find(|line| line.to_ascii_lowercase().contains("switching characteristics"))
                    .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
                    .unwrap_or_else(|| "the later matching table".to_string());
                let omitted_note = if omitted.is_empty() {
                    "All matching pages are attached.".to_string()
                } else {
                    format!(
                        "The other matching {} are in datasheet.txt and datasheet.pdf in your sandbox.",
                        format_page_numbers(&omitted)
                    )
                };
                return Some(format!(
                    "{title} spans PDF {} across {} VCCA operating points{package_summary}; PDF page {} ({chosen_heading}) is the preferred attachment. {omitted_note}",
                    format_page_numbers(&matching_numbers),
                    number_word(operating_points.len()),
                    preferred.number,
                ));
            }

            let attachment_note = if omitted.is_empty() {
                match attached.as_slice() {
                    [only] => format!("The matching page {only} is attached."),
                    _ => format!("All matching {} are attached.", format_page_numbers(&attached)),
                }
            } else {
                format!(
                    "Attached {}; omitted matching {} are in datasheet.txt and datasheet.pdf in your sandbox.",
                    format_page_numbers(&attached),
                    format_page_numbers(&omitted)
                )
            };
            Some(format!(
                "{title} matches PDF {}; later PDF page {} is preferred. {attachment_note}",
                format_page_numbers(&matching_numbers),
                preferred.number,
            ))
        })
        .collect()
}

fn format_page_numbers(numbers: &[usize]) -> String {
    match numbers {
        [] => "no pages".to_string(),
        [only] => format!("page {only}"),
        many if many.windows(2).all(|pair| pair[1] == pair[0] + 1) => {
            format!("pages {} to {}", many[0], many[many.len() - 1])
        }
        many => format!(
            "pages {}",
            many.iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn number_word(n: usize) -> String {
    ["no", "one", "two", "three", "four", "five", "six"]
        .get(n)
        .map_or_else(|| n.to_string(), |w| w.to_string())
}

fn which(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn truncate_to_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

// ── Prompt construction ───────────────────────────────────────────────────────

/// The kinds the schema deserializes. The extraction surface must speak the
/// same closed vocabulary the validator enforces: a backend that invents
/// "charger" burns every attempt against a parser that was never going to
/// accept it. `kind_vocabulary_and_suggestions` pins this list to the enum.
pub fn legal_kinds() -> Vec<String> {
    crate::validation::KIND_NAMES
        .iter()
        .map(|k| (*k).to_string())
        .collect()
}

/// Is this kind string one the schema accepts?
pub fn kind_is_legal(kind: &str) -> bool {
    crate::validation::KIND_NAMES.contains(&kind)
}

fn build_prompt(part: &str, kind: &str, pdf_text: &str) -> String {
    let examples = format_examples_for_kind(kind);
    let example_cards = examples
        .iter()
        .enumerate()
        .map(|(index, example)| {
            format!(
                "EXAMPLE {} -- DIFFERENT PART CLASS ({}, kind={}):\n{}",
                index + 1,
                example.part,
                example.kind,
                example.card
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    format!(
        r#"You are a SPICE model extraction assistant.

YOUR ANSWER IS ONE THING ONLY: the complete [[models]] TOML entry for the
component {part}, extracted from the datasheet below, written in full to
model.toml. There is no shorter valid answer. In particular, a component
kind, a classification, a summary, or any single field on its own is NOT an
answer and fails validation.

Component kind: {kind}

The `kind` FIELD inside your entry must come from this closed list (the
schema accepts nothing else): {legal_kinds}.
When the "Component kind" line above is empty, choosing the kind is merely
the first field you fill in while writing the complete entry. If the part's
natural category is not on the list (a battery charger, a level shifter, an
addressable LED driver), do not invent a kind: use the closest legal kind
for the part's primary electrical behavior (a charger's power path fits
vreg; a level shifter fits digital) and say in `description` what real
behavior the chosen kind does not model.

FORMAT EXAMPLES ONLY:
These are NOT the part being extracted. Each is from a different part class.
Your answer must have the SAME SHAPE, using this datasheet's part, kind, pins,
parameters, ratings, and sources. Copying the example's part number or numbers
is a WRONG ANSWER.

{example_cards}

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
- [[models.envelope]] entries for sourced recommended operating conditions

THE EXACT SHAPE. Field names are matched literally by a strict parser: a
renamed or invented top-level field (type, part, manufacturer, datasheet, ...)
fails validation. Start from this skeleton and keep every field name exactly
as written:

[[models]]
id = "{part_lower}"
kind = "{kind}"
description = "one line"

[models.match]
value_re = "(?i)^{part_upper}"

[models.params]
# the required params for this kind, listed below

[models.ratings]
# only fields the datasheet states

[models.pins]
"1" = "role_from_the_list_below"

[[models.envelope]]
kind = "supply_range"
pin = "supply_role_from_models_pins"
min_v = 1.0
max_v = 2.0
basis = "Recommended Operating Conditions, table and row"

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

[[models.envelope]]: read the Recommended Operating Conditions and Absolute
Maximum Ratings tables. Emit one supply_range entry for every SUPPLY-class pin
whose recommended minimum and maximum are both published. Emit rail_order for
each explicit relation such as VCCA <= VCCB. `pin`, `lower`, and `upper` are
roles from [models.pins], never pad numbers. Every entry requires `basis` naming
the exact table and row. Add `abs_max_v` only when the absolute-maximum table
publishes it. There is no envelope without a table row to cite: omitting this
section is correct when the datasheet lacks the necessary tables. Never infer a
bound from a typical value, application schematic, or family convention.

WHERE TO LOOK AND HOW TO READ IT:
- Supply bounds belong in Recommended Operating Conditions.
- Absolute Maximum Ratings is a separate stress table and does not promise
  functional operation.
- Read the numbered Pin Functions table first, then cross-check it against the
  package top-view figure and a typical application schematic before answering.
- Electrical Characteristics and Switching Characteristics values are worthless
  without the bias condition in the table header, row, or footnote. Put that
  condition in the citation beside the value.
- Application Information and Design Procedure sections often contain the
  programming equations. Cite the equation and every stated validity condition.
- Before answering, re-read every cited row and footnote, then double-check its
  units, sign, package variant, test bias, and min/typ/max column.

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
0. Emit this machine-readable source block immediately after the model's description:
     [models.source]
     tier = "datasheet-derived"
     validation = "physical-bounds-only"
   For each parameter with source-published finite min/max bounds, add:
     [[models.source.uncertainty]]
     status = "interval"
     parameter = "<parameter>"
     low = <finite lower bound>
     high = <finite upper bound>
     unit = "<unit>"
     kind = "specification-limits"
     basis = "<table/curve citation>"
   If no defensible interval is published, add one explicit unknown record:
     [[models.source.uncertainty]]
     status = "unknown"
     parameter = "model"
     reason = "datasheet publishes no validated model error interval"
   A min/typ row without a finite published max is NOT a two-sided interval:
   emit unknown. Never invent a percentage, derive a bound from a typical
   value, or substitute a model clamp for a published guarantee.
1. Add a comment on each param/rating line citing where in the datasheet you found
   the value, e.g.: `# Source: Table 6.3, typ column`
2. Use only values explicitly stated in the datasheet; do NOT guess. For SPICE
   params not given verbatim (e.g. `is`), you may derive them from a stated
   operating point (e.g. VBE at a known IC) and say so in the comment.
3. If a required param is genuinely absent and cannot be derived, do not
   estimate it. Emit an identity-only card with `identity_only = true`, a
   `warning`, and `unlocked_by`, and name the missing fact as
   `ABSTAIN: datasheet does not state ...` in the description or an unknown
   source record. A named abstention is correct; a guessed field is wrong.
4. For every real behavior the selected schema and params do not represent,
   the description must say `does not model ...` and name that behavior. Do not
   let a partial card read as a complete model of the component.
5. Output ONLY the TOML block, starting with [[models]]; no prose, no markdown
   fences, no leading or trailing text.
6. Currents in AMPERES, voltages in VOLTS, power in WATTS (convert mA/mW yourself).
7. Param values must be within these physical bounds:
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
        part_upper = part.to_uppercase(),
        legal_kinds = legal_kinds().join(", "),
        kind = kind,
        pdf_text = truncate_to_chars(&pdf_text, 40_000),
        required_params = required_params_for_kind(kind),
        pin_roles = pin_roles_for_kind(kind),
        ratings_hint = ratings_hint_for_kind(kind),
        behavioral_hint = behavioral_hint_for_kind(kind),
        example_cards = example_cards,
    )
}

/// A complete shape demonstration from a different component class.
///
/// Each candidate teaches all four failure-prone shapes. Filtering out the
/// requested kind still leaves at least two examples from other part classes.
#[derive(Clone, Copy)]
struct FormatExample {
    part: &'static str,
    kind: &'static str,
    card: &'static str,
}

fn format_examples_for_kind(kind: &str) -> Vec<FormatExample> {
    FORMAT_EXAMPLES
        .iter()
        .copied()
        .filter(|example| example.kind != kind)
        .collect()
}

const FORMAT_EXAMPLES: &[FormatExample] = &[
    FormatExample {
        part: "TLV9001",
        kind: "opamp",
        card: r##"[[models]]
id = "tlv9001"
kind = "opamp"
description = "TLV9001 format example; models bounded DC gain and output rails, but does not model noise, slew rate, or input-bias-current variation; ABSTAIN: the cited table does not state input noise at 125 C"
[models.source]
tier = "datasheet-derived"
validation = "physical-bounds-only"
[[models.source.uncertainty]]
status = "unknown"
parameter = "input_noise_density_at_125c"
reason = "ABSTAIN: datasheet does not state this fact"
[models.match]
value_re = "(?i)^TLV9001"
[models.params]
gain = 100000.0 # Source: Electrical Characteristics, AOL typ, VS=5 V, RL=10 kohm, VOUT=0.5 V to 4.5 V
rail_lo = 0.05 # Source: Electrical Characteristics, VS=5 V, RL=10 kohm
rail_hi = 4.95 # Source: Electrical Characteristics, VS=5 V, RL=10 kohm
[models.ratings]
max_voltage_v = 6.0 # Source: Absolute Maximum Ratings, supply voltage row
max_junction_temp_c = 150.0 # Source: Absolute Maximum Ratings, TJ row
[models.pins]
"1" = "out"
"2" = "vee"
"3" = "in_plus"
"4" = "in_minus"
"5" = "vcc"
[[models.envelope]]
kind = "supply_range"
pin = "vcc"
min_v = 1.8
max_v = 5.5
abs_max_v = 6.0
basis = "Recommended Operating Conditions, supply voltage row"
[[models.envelope]]
kind = "rail_order"
lower = "vee"
upper = "vcc"
basis = "Recommended Operating Conditions, supply voltage definition: VEE < VCC""##,
    },
    FormatExample {
        part: "TLV3201",
        kind: "comparator",
        card: r##"[[models]]
id = "tlv3201"
kind = "comparator"
description = "TLV3201 format example; models thresholds and nominal delay, but does not model overdrive-dependent delay dispersion or output-current curves; ABSTAIN: the cited table does not state delay at 125 C"
[models.source]
tier = "datasheet-derived"
validation = "physical-bounds-only"
[[models.source.uncertainty]]
status = "unknown"
parameter = "tpd_at_125c"
reason = "ABSTAIN: datasheet does not state this fact"
[models.match]
value_re = "(?i)^TLV3201"
[models.params]
out_lo = 0.1 # Source: Electrical Characteristics, VOL max, VS=5 V, ISINK=4 mA
out_hi = 4.9 # Source: Electrical Characteristics, VOH min, VS=5 V, ISOURCE=-4 mA
hysteresis = 0.0012 # Source: Electrical Characteristics, typ, VS=5 V, VCM=2.5 V
tpd_s = 4.0e-8 # Source: Switching Characteristics, typ, VS=5 V, overdrive=100 mV, CL=15 pF
[models.ratings]
max_voltage_v = 7.0 # Source: Absolute Maximum Ratings, supply voltage row
[models.pins]
"1" = "out"
"2" = "vee"
"3" = "in_plus"
"4" = "in_minus"
"5" = "vcc"
[[models.envelope]]
kind = "supply_range"
pin = "vcc"
min_v = 2.7
max_v = 5.5
abs_max_v = 7.0
basis = "Recommended Operating Conditions, supply voltage row"
[[models.envelope]]
kind = "rail_order"
lower = "vee"
upper = "vcc"
basis = "Recommended Operating Conditions, supply voltage definition: VEE < VCC""##,
    },
    FormatExample {
        part: "PCA9306",
        kind: "digital",
        card: r##"[[models]]
id = "pca9306_identity"
kind = "digital"
description = "PCA9306 format example; records identity, pin map, and rail constraints, but does not model the bidirectional pass-FET path; ABSTAIN: the datasheet does not state a fixed propagation delay"
[models.source]
tier = "datasheet-derived"
validation = "physical-bounds-only"
[[models.source.uncertainty]]
status = "unknown"
parameter = "propagation_delay"
reason = "ABSTAIN: datasheet does not state a fixed propagation delay"
[models.match]
value_re = "(?i)^PCA9306"
[models.params]
identity_only = true
warning = "identity and sourced rail constraints only; pass-FET translation behavior is not modeled"
unlocked_by = "a validated bidirectional pass-FET model covering pull-ups, loading, and edge timing"
[models.ratings]
max_voltage_v = 7.0 # Source: Absolute Maximum Ratings, VREF2 row
[models.pins]
"1" = "gnd"
"2" = "vref1"
"3" = "scl1"
"4" = "sda1"
"5" = "sda2"
"6" = "scl2"
"7" = "vref2"
"8" = "en"
[[models.envelope]]
kind = "supply_range"
pin = "vref1"
min_v = 1.2
max_v = 3.3
basis = "Recommended Operating Conditions, VREF1 row, EN=VREF2"
[[models.envelope]]
kind = "supply_range"
pin = "vref2"
min_v = 1.8
max_v = 5.5
basis = "Recommended Operating Conditions, VREF2 row, EN=VREF2"
[[models.envelope]]
kind = "rail_order"
lower = "vref1"
upper = "vref2"
basis = "Recommended Operating Conditions, VREF1 must not exceed VREF2""##,
    },
];

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
  rsense_refs = ["<board refdes>"]  # every matched input sense resistor on the board
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

/// What one backend call must come back with, and how to check it before the
/// reply is accepted or quoted back to the model for a retry.
///
/// The validator is part of the request because the three prompts want three
/// different answers: a full `[[models]]` entry, a `[sensor]` spec, or the one
/// word the kind-identification prompt asks for. One validator for all of them
/// would refuse two of them on every attempt.
#[derive(Clone, Copy)]
enum Reply<'a> {
    /// A complete `[[models]]` entry for `part` under `kind`.
    Model { part: &'a str, kind: &'a str },
    /// A `[sensor]` register-map spec for `part` on the bus `kind` implies.
    Sensor { part: &'a str, kind: &'a str },
    /// One bare identifier.
    Word,
}

impl Reply<'_> {
    /// The TOML table the answer starts with (empty for a bare word).
    fn table(self) -> &'static str {
        match self {
            Reply::Model { .. } => "[[models]]",
            Reply::Sensor { .. } => "[sensor]",
            Reply::Word => "",
        }
    }

    fn check(self, raw: &str) -> Result<()> {
        match self {
            Reply::Model { part, kind } => parse_and_validate_reply(raw, part, kind).map(drop),
            Reply::Sensor { part, kind } => validate_sensor_reply(raw, part, kind).map(drop),
            Reply::Word if raw.trim().is_empty() => bail!("the backend returned nothing"),
            Reply::Word => Ok(()),
        }
    }
}

fn call_backend(prompt: &str, args: &Args, reply: Reply<'_>) -> Result<String> {
    // Test / offline hook: HAUKSBEE_EXTRACT_MOCK_REPLY points to a file holding
    // a canned backend reply. This exercises the full parse + validate path
    // with no codex and no network, and the reply is validated exactly like a
    // real one so the hook cannot smuggle garbage past the pipeline.
    if let Ok(path) = std::env::var("HAUKSBEE_EXTRACT_MOCK_REPLY") {
        let canned = std::fs::read_to_string(&path)
            .with_context(|| format!("reading mock reply from {path}"))?;
        let raw = extract_toml_block(&canned, reply.table());
        reply.check(&raw)?;
        return Ok(raw);
    }

    // No explicit --backend keeps the pre-flag behaviour: an exported
    // HAUKSBEE_LLM_API_KEY selects the api backend, codex otherwise.
    let chosen = args
        .backend
        .unwrap_or(match std::env::var_os("HAUKSBEE_LLM_API_KEY") {
            Some(_) => Backend::Api,
            None => Backend::Codex,
        });
    let (tool, install) = match chosen {
        Backend::Api => {
            let request = ApiRequest::prepare(args)?;
            return validated_attempts(args, prompt, reply, "api", |p| {
                Ok(extract_toml_block(&request.send(p)?, reply.table()))
            });
        }
        Backend::Codex => (
            "codex",
            "Install it (`npm install -g @openai/codex` or `brew install codex`) and sign in, \
             or pick another backend: --backend claude-code (needs `claude` in PATH) or \
             --backend api (set OPENAI_API_KEY).",
        ),
        Backend::ClaudeCode => (
            "claude",
            "Install Claude Code (`npm install -g @anthropic-ai/claude-code`) and sign in, or \
             pick another backend: --backend codex (needs `codex` in PATH) or --backend api \
             (set OPENAI_API_KEY).",
        ),
    };
    if !which(tool) {
        bail!(
            "the {} backend needs the `{tool}` CLI, which is not in PATH. {install}",
            chosen.name()
        );
    }

    // The sandbox is a scratch copy, never the user's own directory. See
    // `Workspace`: the agent runs full-auto, so what it can reach is the whole
    // of the security story.
    let ws = prepare_workspace(&args.pdf)?;
    eprintln!(
        "[model-extract] sandbox {} with {} page render(s)",
        ws.path().display(),
        ws.pages.len()
    );
    let base_prompt = format!("{prompt}\n\n{}", verification_clause(&ws));
    validated_attempts(args, &base_prompt, reply, tool, |p| {
        let _ = std::fs::remove_file(ws.answer_path());
        let stdout = match chosen {
            Backend::Codex => run_codex_once(p, &ws, args.model.as_deref())?,
            _ => run_claude_once(p, &ws, args.model.as_deref())?,
        };
        // Prefer the file we asked for. Falling back to stdout keeps a model
        // that answered in prose from failing outright, but the file is the
        // reliable path: stdout also carries the agent's narration, and one
        // that says "here is the TOML" twice yields two candidates.
        let text = std::fs::read_to_string(ws.answer_path())
            .ok()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(stdout);
        Ok(extract_toml_block(&text, reply.table()))
    })
}

/// Fetch, validate, and retry with feedback: one loop for every backend.
///
/// Each attempt sends `base_prompt` plus an appendix quoting only the LATEST
/// failure (accumulating every prior failure grew the prompt without bound
/// and timed out the final attempt of a long extraction). The error is
/// rendered as its whole cause chain (`{e:#}`): the outer context alone told
/// a retrying model "is it valid TOML?" when the real error was `missing
/// field id` with a line number, so it repeated the same shape mistake.
fn validated_attempts(
    args: &Args,
    base_prompt: &str,
    reply: Reply<'_>,
    tool: &str,
    mut fetch: impl FnMut(&str) -> Result<String>,
) -> Result<String> {
    let mut prompt = base_prompt.to_string();
    for attempt in 1..=args.retries + 1 {
        let started = Instant::now();
        let raw = fetch(&prompt)?;
        eprintln!(
            "[model-extract] {tool} attempt {attempt} returned {} chars in {:.0}s",
            raw.len(),
            started.elapsed().as_secs_f64()
        );
        match reply.check(&raw) {
            Ok(()) => return Ok(raw),
            Err(e) if attempt <= args.retries => {
                eprintln!(
                    "[model-extract] attempt {attempt} failed: {e:#}; retrying with feedback..."
                );
                prompt = format!(
                    "{base_prompt}\n\n{}",
                    retry_feedback(&raw, &format!("{e:#}"), reply.table())
                );
            }
            Err(e) => bail!(
                "{tool} produced an answer that failed validation after {attempt} attempt(s): \
                 {e:#}\nRaw reply was:\n{raw}"
            ),
        }
    }
    unreachable!("the loop returns or bails on its last attempt")
}

/// Is a failed reply visibly only a classification or another tiny fragment?
///
/// The length check catches the measured `digital`, `model = "digital"`, and
/// `answer = "digital"` failures. The shape checks also catch a longer bare
/// scalar or single-key TOML document without misclassifying a malformed full
/// entry as a classification.
fn reply_is_kind_or_fragment(raw: &str, table: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.chars().count() < 200 {
        return true;
    }
    if trimmed.contains(table) {
        return false;
    }
    let bare_scalar = !trimmed.contains('\n') && !trimmed.contains('=');
    let one_key_table = toml::from_str::<toml::Table>(trimmed).is_ok_and(|t| t.len() <= 1);
    bare_scalar || one_key_table
}

/// The retry appendix for the latest failure. Quoting a short reply back to
/// the model makes the classification-vs-card mistake concrete.
fn retry_feedback(raw: &str, validation_error: &str, table: &str) -> String {
    let fragment_correction = if !table.is_empty() && reply_is_kind_or_fragment(raw, table) {
        format!(
            "YOUR PREVIOUS REPLY, QUOTED VERBATIM:\n--- BEGIN REPLY ---\n{raw}\n\
             --- END REPLY ---\n\
             That reply is only the component kind (or another fragment), not the answer. \
             The kind is only the `kind = \"...\"` FIELD inside the answer. The answer is \
             the WHOLE `{table}` table, with every required section shown in the FORMAT \
             EXAMPLE.\n\n"
        )
    } else {
        String::new()
    };
    format!(
        "{fragment_correction}YOUR PREVIOUS ANSWER FAILED VALIDATION WITH:\n\
         {validation_error}\n\n\
         Fix exactly those issues and write the corrected WHOLE `{table}` TOML table \
         to model.toml."
    )
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
        truncate_to_chars(pdf_text, 6000)
    );
    let reply = call_backend(&prompt, args, Reply::Word)?.to_ascii_lowercase();
    // The model may answer with surrounding prose despite the instruction, so
    // look for a supported kind rather than trusting the whole reply.
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
        let inventory = ws
            .selected_pages
            .iter()
            .map(|page| {
                let reasons = page
                    .reasons
                    .iter()
                    .map(|reason| (*reason).to_string())
                    .chain(page.supplemental_reasons.iter().map(|reason| {
                        // A supplemental page is useful context, but it is not
                        // the later representative chosen for that category.
                        format!("additional {reason} context")
                    }))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "- page-{:03}.png is PDF page {}: {}",
                    page.number, page.number, reasons
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let category_coverage = ws.category_disclosures.join("\n");
        let text_dump = if ws.has_text_dump {
            "The complete text dump is `datasheet.txt`."
        } else {
            "No separate text dump was available."
        };
        format!(
            "{} relevance-selected page image(s) are attached:\n{}\n\nPer-category coverage:\n{}\n\nThis is not \
             the whole datasheet. The complete PDF is `datasheet.pdf`. {} If a needed \
             fact is on an omitted page, open the PDF or text dump before answering. \
             Read values off the IMAGES for anything that lives in a table or pinout: \
             text extraction loses which column a number belonged to.",
            ws.pages.len(),
            inventory,
            category_coverage,
            text_dump
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
/// Deliberately keep `high` for non-sol overrides too: silently dropping a
/// weaker model to medium could make the pin-map work less reliable, and
/// `HAUKSBEE_CODEX_EFFORT` remains the explicit escape hatch. A per-model
/// default needs comparative benchmark evidence first.
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

/// Run one sandboxed agent invocation to completion and return its stdout.
///
/// The contract both agent CLIs share: the sandbox is the working directory,
/// the full prompt goes in a FILE inside it (argv is world-readable on Linux
/// via /proc/<pid>/cmdline and visible to `ps -ww` on macOS, and a 40,000
/// character datasheet excerpt would undo the consent the user gave to send it
/// to one vendor; it would also brush ARG_MAX), stdin carries only a pointer to
/// that file, and the answer is expected in `model.toml`.
///
/// stderr goes to `log`, a FILE: not a pipe, because nothing reads a pipe until
/// the child has exited, so a ten-minute agentic run overflows the 64 KiB
/// buffer, the child blocks in write(2), and the loop burns the whole timeout
/// with the blame pointing at the model; and not /dev/null, because a refused
/// model name or a rate limit is the whole diagnosis and discarding it leaves
/// `exited with status 1: ` and nothing after the colon.
fn run_cli_agent(
    tool: &str,
    cmd: &mut Command,
    ws: &Workspace,
    prompt: &str,
    log: &Path,
    retry_hint: &str,
) -> Result<String> {
    std::fs::write(ws.path().join("prompt.md"), prompt)
        .context("writing the prompt into the sandbox")?;
    let stderr = std::fs::File::create(log)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .with_context(|| format!("spawning {tool} (is it installed and on PATH?)"))?;
    // The pointer on stdin, then EOF (dropping `stdin` sends it): codex reads
    // its prompt from stdin when no positional one survived arg parsing, and
    // blocks waiting for EOF if the pipe is left open. Never a trailing
    // positional argument: codex's `--image` takes many values, so a trailing
    // `<prompt>` parsed as one more image path and codex reported "No prompt
    // provided via stdin".
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(AGENT_POINTER);
    }
    wait_for_cli_backend(child, tool, retry_hint, log)
}

/// The stdin instruction every agent run receives. The pages are on disk
/// beside the PDF (codex also gets them attached with `--image`), so one
/// pointer serves both CLIs.
const AGENT_POINTER: &[u8] = b"Read prompt.md in your working directory and follow it exactly. \
    The rendered datasheet pages (page-*.png) and datasheet.pdf are in the same directory; \
    read values off the page images for anything that lives in a table or a pinout. \
    Write your answer to model.toml.";

/// One codex invocation. Invocation notes learned the hard way:
///   * `--sandbox workspace-write` (`--full-auto` is deprecated): writes are
///     confined to the writable roots, reads are NOT (see `Workspace`).
///   * `--skip-git-repo-check`, codex otherwise refuses to run outside a repo,
///     and the sandbox deliberately is not one.
///   * `--cd <sandbox>` so codex can open the datasheet PDF / extracted text
///     directly when pdftotext was unavailable.
///   * the model is named rather than inherited: codex's default varies by the
///     user's plan and config and silently decides how good the extraction is.
fn run_codex_once(prompt: &str, ws: &Workspace, model: Option<&str>) -> Result<String> {
    let (model, effort) = codex_model(model);
    let mut cmd = Command::new("codex");
    cmd.arg("exec");
    // An alternative codex auth profile (an Azure/OpenAI-compatible endpoint
    // configured in ~/.codex/config.toml) rides in via the environment: the
    // personal ChatGPT account is rate-limited exactly when extraction runs
    // are heaviest, and a profile flag is codex's own mechanism for that.
    if let Some(profile) = std::env::var("HAUKSBEE_CODEX_PROFILE")
        .ok()
        .filter(|p| !p.trim().is_empty())
    {
        cmd.args(["-p", profile.trim()]);
    }
    let effort = format!("model_reasoning_effort=\"{effort}\"");
    cmd.args([
        "--model",
        &model,
        "-c",
        &effort,
        "--sandbox",
        "workspace-write",
        "--skip-git-repo-check",
        "--cd",
    ])
    .arg(ws.path())
    // The answer goes to a file we name, so a reply that also narrates does
    // not leave two candidate TOML blocks to choose between.
    .arg("--output-last-message")
    .arg(ws.path().join("last-message.txt"));
    // Page renders, in page order: a table or a pinout survives a render and
    // does not survive a text dump.
    for page in &ws.pages {
        cmd.arg("--image").arg(page);
    }
    run_cli_agent(
        "codex",
        &mut cmd,
        ws,
        prompt,
        &ws.path().join("codex-stderr.log"),
        "retry with a tighter prompt or set HAUKSBEE_LLM_API_KEY",
    )
}

/// One headless `claude -p` invocation under the same contract as codex.
/// `--permission-mode acceptEdits` lets the agent write `model.toml` without
/// an interactive prompt; the sandbox directory bounds what those edits touch.
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
        cmd.args(["--model", m]);
    }
    run_cli_agent(
        "claude",
        &mut cmd,
        ws,
        prompt,
        &ws.path().join("claude-stderr.log"),
        "retry with a tighter prompt or another --backend",
    )
}

/// Poll a spawned CLI agent to completion, killing it at `CLI_BACKEND_TIMEOUT`,
/// and return its stdout. On a non-zero exit the error quotes the last few
/// non-empty lines of BOTH stdout and the captured stderr log: a CLI agent puts
/// the reason it gave up (a refused model, a rate limit, a bad config key) on
/// stderr, so a failure with an empty stdout would otherwise read as "exited
/// with status 1: " and nothing after the colon.
fn wait_for_cli_backend(
    mut child: Child,
    tool: &str,
    retry_hint: &str,
    log_path: &Path,
) -> Result<String> {
    let deadline = Instant::now() + CLI_BACKEND_TIMEOUT;
    while child
        .try_wait()
        .with_context(|| format!("polling {tool}"))?
        .is_none()
    {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{tool} timed out after {}s with no answer; {retry_hint}",
                CLI_BACKEND_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let output = child
        .wait_with_output()
        .with_context(|| format!("collecting {tool} output"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let tail = |text: &str| {
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        lines[lines.len().saturating_sub(5)..].join(" | ")
    };
    let detail = [
        tail(&String::from_utf8_lossy(&output.stdout)),
        std::fs::read_to_string(log_path)
            .map(|t| tail(&t))
            .unwrap_or_default(),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join(" || ");
    if detail.is_empty() {
        bail!(
            "{tool} exited with status {} and said nothing on stdout or stderr",
            output.status
        );
    }
    bail!("{tool} exited with status {}: {detail}", output.status)
}

/// The default base URL for the api backend.
pub const DEFAULT_API_BASE: &str = "https://api.openai.com/v1";

/// The environment variable the api backend reads its key from: an explicit
/// `--api-key-env` wins; otherwise the legacy `HAUKSBEE_LLM_API_KEY` when it
/// is set (that variable selected the backend before `--backend` existed),
/// else `OPENAI_API_KEY`.
fn api_key_env_name(args: &Args) -> String {
    args.api_key_env.clone().unwrap_or_else(|| {
        if std::env::var_os("HAUKSBEE_LLM_API_KEY").is_some() {
            "HAUKSBEE_LLM_API_KEY"
        } else {
            "OPENAI_API_KEY"
        }
        .to_string()
    })
}

/// One prepared OpenAI-compatible chat-completions request: everything but
/// the prompt, resolved once so every retry hits the same endpoint, model and
/// key.
///
/// The key is read from the environment at call time and appears nowhere the
/// system can echo it: not in argv (world-readable via `ps` / /proc), not in
/// a log line, not in an error. curl gets it through `--config -` on stdin,
/// and the request body travels as a private temp file rather than an
/// argument (it embeds the datasheet text, and can outgrow ARG_MAX anyway).
struct ApiRequest {
    url: String,
    model: String,
    api_key: String,
}

impl ApiRequest {
    fn prepare(args: &Args) -> Result<Self> {
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
        let setting = |flag: &Option<String>, var: &str, default: &str| {
            flag.clone()
                .or_else(|| std::env::var(var).ok())
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        let base = setting(&args.api_base, "HAUKSBEE_LLM_BASE_URL", DEFAULT_API_BASE);
        Ok(ApiRequest {
            url: format!("{}/chat/completions", base.trim_end_matches('/')),
            model: setting(&args.model, "HAUKSBEE_LLM_MODEL", DEFAULT_CODEX_MODEL),
            api_key,
        })
    }

    /// POST `prompt` and return the assistant's content.
    fn send(&self, prompt: &str) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": "You are a SPICE model extraction assistant. Output TOML only."},
                {"role": "user", "content": prompt}
            ],
            "max_tokens": 2048,
            "temperature": 0.0
        });
        let staging = tempfile::Builder::new()
            .prefix("hauksbee-api-")
            .tempdir()
            .context("creating the api request staging directory")?;
        let body_path = staging.path().join("request.json");
        std::fs::write(&body_path, serde_json::to_string(&body)?)
            .context("writing the api request body")?;
        let curl_config = format!(
            "url = \"{}\"\nrequest = \"POST\"\nheader = \"Content-Type: application/json\"\n\
             header = \"Authorization: Bearer {}\"\ndata = \"@{}\"\nsilent\nshow-error\n",
            self.url,
            self.api_key,
            body_path.display()
        );
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
        }
        let output = child
            .wait_with_output()
            .context("collecting the api response")?;
        if !output.status.success() {
            bail!(
                "curl failed against {}: {}",
                self.url,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let resp: serde_json::Value = serde_json::from_slice(&output.stdout)
            .with_context(|| format!("parsing the reply from {} as JSON", self.url))?;
        // An error object instead of choices is the endpoint explaining
        // itself (bad model name, exhausted quota); surface that message.
        if let Some(err_msg) = resp["error"]["message"].as_str() {
            bail!("{} answered with an error: {err_msg}", self.url);
        }
        resp["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_owned)
            .context("missing content in API response")
    }
}

// ── TOML parsing and validation ───────────────────────────────────────────────

/// Extract a TOML block from potentially prose-wrapped LLM output: the body of
/// a ```toml / ``` fence when there is one, else everything from the first
/// `table` header (a stray greeting line must not break the parse).
fn extract_toml_block(s: &str, table: &str) -> String {
    for fence in ["```toml", "```"] {
        if let Some(start) = s.find(fence) {
            let after = &s[start + fence.len()..];
            if let Some(end) = after.find("```") {
                return after[..end].trim().to_string();
            }
        }
    }
    let start = if table.is_empty() {
        None
    } else {
        s.find(table)
    };
    s[start.unwrap_or(0)..].trim().to_string()
}

/// Parse raw TOML and validate the first model entry.
fn parse_and_validate_reply(raw: &str, part: &str, kind_str: &str) -> Result<ModelEntry> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("empty reply for {part}: the backend returned no TOML at all");
    }
    // A reply that is the entry BODY without its [[models]] header (a bare
    // `id = ...` / `kind = ...` opening) is a near-miss worth recovering:
    // prepending the header changes no content, and burning a whole LLM
    // attempt on a missing two-token line helps nobody.
    let text = if trimmed.contains("[[models]]") {
        trimmed.to_string()
    } else {
        let bare_entry = trimmed
            .lines()
            .map(str::trim_start)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .is_some_and(|l| {
                ["id ", "id=", "kind ", "kind="]
                    .iter()
                    .any(|p| l.starts_with(p))
            });
        if !bare_entry {
            bail!(
                "reply for {part} contains no [[models]] table; the backend likely answered \
                 with prose instead of TOML. First 200 chars: {trimmed:.200}"
            );
        }
        format!("[[models]]\n{trimmed}")
    };
    let db: crate::schema::DbFile = toml::from_str(&text).with_context(|| {
        format!(
            "the reply for {part} did not deserialize as a hauksbee [[models]] db file (the \
             cause below names the exact field and line)"
        )
    })?;
    let entry = db
        .models
        .into_iter()
        .next()
        .with_context(|| format!("no [[models]] entry in reply for {part}"))?;

    // Guard against the backend returning the wrong device kind, which would
    // bind nonsense (a diode card stamped as a BJT). A behavioural family
    // (charger/pmic/balancer) is satisfied by its BASE kind in the TOML, and
    // must then actually carry the behavioural block it was asked for.
    let want = kind_str.trim();
    let want_base = behavioral_family_base_kind(want).unwrap_or(want);
    let got = kind_discriminant(entry.kind);
    if !want.is_empty() && got != want_base {
        bail!(
            "kind mismatch for {part}: requested '{want}' (base '{want_base}') but the reply \
             is '{got}'"
        );
    }
    if behavioral_family_base_kind(want).is_some() && entry.behavioral.is_empty() {
        bail!(
            "behavioural extraction for {part} (kind '{want}') produced no [models.behavioral] \
             block; the prompt asked for one"
        );
    }
    crate::validation::validate(&entry).map_err(|errs| {
        let msg: Vec<String> = errs.iter().map(ToString::to_string).collect();
        anyhow::anyhow!("validation failed: {}", msg.join("; "))
    })?;
    Ok(entry)
}

/// The snake_case name a kind serialises as (the serde `rename_all`).
fn kind_discriminant(kind: ComponentKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn sanitise_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || "-_".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn default_out_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".hauksbee")
        .join("models")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn parse(extra: &[&str]) -> Result<Args> {
        let mut v = vec!["--pdf", "x.pdf", "--part", "P"];
        v.extend_from_slice(extra);
        parse_args_from(argv(&v))
    }

    #[test]
    fn truncate_to_chars_cuts_on_char_boundaries() {
        let out = truncate_to_chars(&"µ".repeat(30), 20);
        assert_eq!(out.chars().count(), 20);
        assert!(out.chars().all(|c| c == 'µ'));
        assert_eq!(truncate_to_chars("abc", 10), "abc");
    }

    #[test]
    fn parse_args_backend_flags() {
        let a = parse(&[]).unwrap();
        assert_eq!(a.backend, None);
        assert_eq!(a.api_base, None);
        assert_eq!(a.api_key_env, None);
        assert!(a.kind_str.is_empty());

        for (flag, want) in [
            ("codex", Backend::Codex),
            ("claude-code", Backend::ClaudeCode),
            ("api", Backend::Api),
        ] {
            assert_eq!(parse(&["--backend", flag]).unwrap().backend, Some(want));
        }
        assert!(parse(&["--backend", "gemini"]).is_err());

        let a = parse(&[
            "--backend",
            "api",
            "--api-base",
            "https://llm.example/v1",
            "--api-key-env",
            "MY_LLM_KEY",
        ])
        .unwrap();
        assert_eq!(a.api_base.as_deref(), Some("https://llm.example/v1"));
        assert_eq!(a.api_key_env.as_deref(), Some("MY_LLM_KEY"));
        assert_eq!(api_key_env_name(&a), "MY_LLM_KEY");
    }

    #[test]
    fn api_key_env_must_be_a_variable_name_not_a_key() {
        for pasted in ["sk-abc123XYZ", "1KEY", "MY KEY", ""] {
            assert!(parse(&["--api-key-env", pasted]).is_err(), "{pasted:?}");
        }
        assert!(validate_api_key_env_name("OPENAI_API_KEY").is_ok());
        assert!(validate_api_key_env_name("_KEY2").is_ok());
    }

    #[test]
    fn extract_toml_block_finds_the_entry() {
        for s in [
            "Sure, here you go:\n```toml\n[[models]]\nid = \"test\"\n```\nDone.",
            "[[models]]\nid = \"test\"\nkind = \"diode\"\n",
            "Sure! Here is the TOML:\n[[models]]\nid = \"z\"\nkind = \"diode\"\n",
        ] {
            assert!(
                extract_toml_block(s, "[[models]]").starts_with("[[models]]"),
                "{s:?}"
            );
        }
    }

    #[test]
    fn prompt_carries_kind_specific_guidance() {
        let bjt = build_prompt("BC847", "bjt_npn", "x");
        assert!(bjt.contains("is, bf, nf"));
        assert!(bjt.contains("BC847"));
        assert!(bjt.contains("VCEO"));
        assert!(!bjt.contains("[models.behavioral.converter]"));
        assert!(build_prompt("1N4148", "diode", "x").contains("VRRM"));
        assert!(build_prompt("AMS1117", "vreg", "x").contains("max_junction_temp_c"));

        let charger = build_prompt("LTC4020", "charger", "x");
        assert!(charger.contains("[models.behavioral.converter]"));
        assert!(charger.contains("iin_program"));
        assert!(build_prompt("nPM1300", "pmic", "x").contains("pull_to"));
        assert!(build_prompt("LTC6803", "balancer", "x").contains("[[models.behavioral.laws]]"));
    }

    #[test]
    fn format_examples_validate_and_exclude_the_target_kind() {
        for example in FORMAT_EXAMPLES {
            parse_and_validate_reply(example.card, example.part, example.kind)
                .expect("every worked example must be a valid complete model entry");
        }
        for kind in ["digital", "opamp"] {
            let examples = format_examples_for_kind(kind);
            assert!(examples.len() >= 2, "the prompt must be many-shot");
            assert!(examples.iter().all(|e| e.kind != kind));
        }
        let prompt = build_prompt("TXB0101", "digital", "datasheet text");
        let start = prompt.find("FORMAT EXAMPLES ONLY").unwrap();
        let end = prompt.find("DATASHEET TEXT (truncated):").unwrap();
        assert!(!prompt[start..end].contains("TXB0101"));
    }

    #[test]
    fn fragment_replies_are_recognised() {
        assert!(reply_is_kind_or_fragment("digital", "[[models]]"));
        assert!(reply_is_kind_or_fragment(
            "model = \"digital\"",
            "[[models]]"
        ));
        assert!(reply_is_kind_or_fragment(
            "answer = \"digital\"",
            "[[models]]"
        ));
        assert!(!reply_is_kind_or_fragment(
            FORMAT_EXAMPLES[0].card,
            "[[models]]"
        ));
        assert!(retry_feedback("digital", "no table", "[[models]]")
            .contains("--- BEGIN REPLY ---\ndigital\n"));
    }

    #[test]
    fn behavioral_charger_reply_validates_under_its_family_kind() {
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
rsense_refs = ["R49", "R50"]
prog_ref = "R8"
vprog_ref = 0.0316
prog_ref_ohms = 7150.0
v_sense_full = 0.0463
```"#;
        let entry = parse_and_validate_reply(
            &extract_toml_block(reply, "[[models]]"),
            "LTC4020",
            "charger",
        )
        .unwrap();
        assert_eq!(entry.kind, crate::ComponentKind::Vreg);
        assert!(entry.behavioral.converter.is_some());

        let plain = "[[models]]\nid = \"x\"\nkind = \"vreg\"\n[models.params]\nvout = 5.0\ndropout_v = 1.0\niq_a = 0.001\n";
        assert!(parse_and_validate_reply(plain, "X", "charger").is_err());
    }

    #[test]
    fn roc_envelope_round_trips_through_the_schema() {
        let reply = r#"
[[models]]
id = "roc_supply"
kind = "digital"
description = "ROC extraction fixture"
[models.match]
value_re = "^ROC_SUPPLY$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "vcc"
[[models.envelope]]
kind = "supply_range"
pin = "vcc"
min_v = 2.7
max_v = 3.6
abs_max_v = 4.0
basis = "Recommended Operating Conditions, Table 6.3, VCC row"
"#;
        let entry = parse_and_validate_reply(reply, "ROC_SUPPLY", "digital").unwrap();
        let round_trip = toml::to_string(&crate::schema::DbFile {
            models: vec![entry],
        })
        .unwrap();
        assert!(round_trip.contains("[[models.envelope]]"));
        assert!(round_trip.contains("min_v = 2.7"));
        assert!(round_trip.contains("max_v = 3.6"));
        assert!(
            round_trip.contains("basis = \"Recommended Operating Conditions, Table 6.3, VCC row\"")
        );
    }

    fn testdata(rel: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata")
            .join(rel)
    }

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
        std::env::set_var("HAUKSBEE_EXTRACT_MOCK_REPLY", &reply_path);

        let args = Args::new(
            dir.join("nonexistent.pdf"),
            "BC847".into(),
            "bjt_npn".into(),
        )
        .out_dir(Some(dir.clone()));
        let prompt = build_prompt(&args.part, &args.kind_str, "irrelevant");
        let reply = Reply::Model {
            part: &args.part,
            kind: &args.kind_str,
        };
        let raw = call_backend(&prompt, &args, reply).expect("mock backend should succeed");
        let entry = parse_and_validate_reply(&raw, &args.part, &args.kind_str).unwrap();
        std::env::remove_var("HAUKSBEE_EXTRACT_MOCK_REPLY");

        assert_eq!(entry.kind, crate::ComponentKind::BjtNpn);
        assert_eq!(entry.ratings.max_voltage_v, Some(65.0));
        assert!(raw.starts_with("[[models]]"), "fence should be stripped");
    }

    fn live_backend_available() -> bool {
        which("codex") || std::env::var("HAUKSBEE_LLM_API_KEY").is_ok()
    }

    /// Live: the real backend against the BC847 datasheet in testdata. Run with
    /// `cargo test -p hauksbee-models -- extract_bc847_live --ignored --nocapture`.
    #[test]
    #[ignore]
    fn extract_bc847_live() {
        let pdf = testdata("datasheets/BC847.pdf");
        assert!(pdf.exists(), "BC847 datasheet not found at {pdf:?}");
        if !live_backend_available() {
            return;
        }
        let text = extract_pdf_text(&pdf).expect("PDF text extraction");
        let prompt = build_prompt("BC847", "bjt_npn", &text);
        let args =
            Args::new(pdf, "BC847".into(), "bjt_npn".into()).out_dir(Some(std::env::temp_dir()));
        let reply = Reply::Model {
            part: "BC847",
            kind: "bjt_npn",
        };
        let raw = call_backend(&prompt, &args, reply).expect("backend call");
        let entry = parse_and_validate_reply(&raw, "BC847", "bjt_npn").unwrap();
        let bf = entry.params.get_f64("bf").expect("bf present");
        assert!(
            (100.0..=460.0).contains(&bf),
            "bf {bf} outside the BC847 hFE band"
        );
        assert_eq!(entry.ratings.max_voltage_v, Some(65.0));
    }

    /// Live: `--kind charger` against the LTC4020 excerpt in testdata.
    #[test]
    #[ignore]
    fn extract_ltc4020_charger_live() {
        let src = testdata("datasheets/LTC4020_excerpt.txt");
        if !src.exists() || !live_backend_available() {
            return;
        }
        let text = extract_pdf_text(&src).expect("text");
        let prompt = build_prompt("LTC4020", "charger", &text);
        let args =
            Args::new(src, "LTC4020".into(), "charger".into()).out_dir(Some(std::env::temp_dir()));
        let reply = Reply::Model {
            part: "LTC4020",
            kind: "charger",
        };
        let raw = call_backend(&prompt, &args, reply).expect("backend call");
        let entry = parse_and_validate_reply(&raw, "LTC4020", "charger").unwrap();
        assert_eq!(entry.kind, crate::ComponentKind::Vreg);
        let c = entry.behavioral.converter.expect("converter block");
        assert_eq!(c.in_pin, "pvin");
        assert_eq!(c.out_pin, "bat");
        assert!(c.iin_program.is_some());
    }

    #[test]
    fn reply_validation_verdicts() {
        let good = r#"
[[models]]
id = "bcm847bs"
kind = "bjt_npn"
[models.match]
mpn_re = "(?i)BCM847BS"
[models.params]
is  = 1.0e-14
bf  = 150.0
nf  = 1.0
vaf = 80.0
[models.pins]
"1" = "base"
"2" = "emitter"
"6" = "collector"
"#;
        let entry = parse_and_validate_reply(good, "BCM847BS", "bjt_npn").unwrap();
        assert_eq!(entry.id, "bcm847bs");

        let bad_bf = good.replace("bf  = 150.0", "bf  = 99999.0");
        assert!(parse_and_validate_reply(&bad_bf, "BCM847BS", "bjt_npn").is_err());
        assert!(parse_and_validate_reply("   \n  ", "BC847", "bjt_npn").is_err());
        assert!(
            parse_and_validate_reply("I couldn't find the parameters.", "BC847", "bjt_npn")
                .is_err()
        );
        let diode = "[[models]]\nid = \"x\"\nkind = \"diode\"\n[models.params]\nis = 1e-9\nn = 1.7\nrs = 0.5\n";
        assert!(
            parse_and_validate_reply(diode, "X", "bjt_npn").is_err(),
            "kind mismatch"
        );
    }

    #[test]
    fn sensor_replies_round_trip_and_reject_bus_mismatch() {
        assert!(is_sensor_kind("i2c_sensor") && is_sensor_kind("spi_sensor"));
        assert!(!is_sensor_kind("adc"));

        let i2c = r#"
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
        let spec = validate_sensor_reply(i2c, "LM75", "i2c_sensor").unwrap();
        assert_eq!(spec.sensor.name, "LM75");
        assert_eq!(spec.sensor.i2c_address, Some(0x48));
        assert!(validate_sensor_reply(i2c, "LM75", "spi_sensor").is_err());

        let spi = r#"
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
        assert_eq!(
            validate_sensor_reply(spi, "MINIMU", "spi_sensor")
                .unwrap()
                .sensor
                .name,
            "MINIMU"
        );
        assert!(validate_sensor_reply("Here is the model:", "LM75", "i2c_sensor").is_err());
    }

    #[test]
    fn every_supported_kind_binds_and_has_parameter_guidance() {
        let generic = required_params_for_kind("__definitely_not_a_kind__");
        for kind in SUPPORTED_KINDS {
            if is_sensor_kind(kind) {
                continue;
            }
            let base = behavioral_family_base_kind(kind).unwrap_or(kind);
            assert!(
                kind_is_legal(base),
                "{kind} does not resolve to a component kind"
            );
            assert_ne!(
                required_params_for_kind(kind),
                generic,
                "{kind} has only the generic hint"
            );
        }
    }

    #[test]
    fn legal_kinds_match_the_schema() {
        let kinds = legal_kinds();
        assert!(kinds.len() >= 10, "extraction broke: {kinds:?}");
        assert!(kinds.iter().all(|k| kind_is_legal(k)));
        assert!(kinds.iter().any(|k| k == "vreg"));
        assert!(!kind_is_legal("charger"));
    }
}
