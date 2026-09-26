//! Datasheet extraction: draft a device model from a PDF, then validate it.
//!
//! This is a library module, with the binary (`model-extract`) a thin wrapper
//! over it, so the engine can offer extraction directly. A capability reachable
//! only by running a second executable is one most users never find.
//!
//! Nothing here runs on its own. Extraction sends the datasheet (and what an
//! agent reads) to an LLM backend, so it happens when a caller asks and not before. The caller
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
//! Selected with `--backend codex|claude-code|agy|api`, or saved once in
//! `~/.config/hauksbee/extract.toml` (see [`crate::extract_config`]):
//!
//! 1. **codex**: shells out to `codex exec` with a carefully constructed
//!    prompt. Requires `codex` in PATH.
//! 2. **claude-code**: shells out to headless `claude -p` with the same
//!    prompt contract. Requires `claude` in PATH. Defaults to Opus 5 at high
//!    reasoning effort.
//! 3. **agy**: shells out to Antigravity's `agy --print` under the same
//!    contract. Requires `agy` in PATH. Defaults to Gemini 3.8 Flash at high
//!    effort.
//! 4. **api**: calls an OpenAI-compatible chat-completions endpoint,
//!    configured by `--api-base` (default `https://api.openai.com/v1`),
//!    `--model`, and `--api-key-env NAME` (the key is read from that
//!    environment variable at call time and never stored).
//!
//! With no `--backend` anywhere, setting `HAUKSBEE_LLM_API_KEY` selects the
//! api backend (the behaviour before `--backend` existed); otherwise the first
//! agent CLI found on PATH, codex then claude then agy.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::extract_config::{self, ProcessHost, Resolved, ResolvedApi};
use crate::extract_output as output;
use crate::extract_progress as progress;
use crate::schema::{ComponentKind, ModelEntry};
use crate::sensor_spec::{Bus, SensorSpec};

/// Narration on stderr that never shares a line with the CLI spinner.
macro_rules! say {
    ($($arg:tt)*) => { progress::say(&format!($($arg)*)) };
}

// How long one agent-CLI run may take before it is killed is
// `extract_config::DEFAULT_TIMEOUT_SECS` (ten minutes), configurable per
// machine. It is deliberately not scaled with rendered-page count: page count
// is a poor proxy for extraction work (one dense pin table can take longer
// than several simple pages), while a model that has produced nothing after
// ten minutes is more likely stuck than productively reading page fourteen.
// `MAX_RENDERED_PAGES` already bounds input.

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
/// matters. Codex's own `--sandbox workspace-write` confines WRITES but not
/// reads: a real run under it searched the home directory, another project and
/// `/tmp`, and read an installed codex skill. So in its workspace-write modes
/// codex gets a named permission profile instead ([`codex_confinement`]):
/// shell commands may read this directory, the OS/runtime paths codex calls
/// `:minimal`, the package prefixes in [`CONFINED_READ_ROOTS`] and the checker
/// executable, and nothing under home, `/tmp` or `$TMPDIR`. Claude Code gets the
/// same boundary from its Bash sandbox plus `--restricted` file tools
/// ([`claude_confinement`]). agy and codex's read-only / danger-full-access
/// modes are NOT read-confined by hauksbee. The CLI process itself still reads
/// its own sign-in, and whatever the agent reads goes to the vendor.
///
/// The rest of the contract still matters: the run happens only when the user
/// asked for it, the output is a small TOML document validated against a
/// schema before anything is saved, and the user reviews the card.
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
    /// The product's own checker executable (canonical), named in the prompt
    /// and the only file outside the workspace the confined agent may run.
    pub checker: Option<PathBuf>,
}

impl Workspace {
    /// An empty sandbox with no datasheet in it, for a connectivity check that
    /// runs the backend on a one-line prompt. Same isolation as a real run.
    pub fn scratch() -> Result<Self> {
        let dir = workspace_tempdir(
            &std::env::temp_dir(),
            "hauksbee-extract-check-",
            "creating the check sandbox",
        )?;
        Ok(Workspace {
            pdf: dir.path().join("datasheet.pdf"),
            dir,
            pages: Vec::new(),
            selected_pages: Vec::new(),
            category_disclosures: Vec::new(),
            has_text_dump: false,
            checker: None,
        })
    }

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

/// Create a temporary directory whose retained path is the canonical absolute
/// path used by every file operation, child cwd, and provider path argument.
fn workspace_tempdir(root: &Path, prefix: &str, context: &str) -> Result<tempfile::TempDir> {
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalizing temporary root {}", root.display()))?;
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(root)
        .with_context(|| context.to_string())
}

/// Build a sandbox, for the tests that check the boundary holds.
///
/// The boundary is the whole security story of an agent running full-auto, so
/// it is asserted rather than trusted. Exposed for `tests/extract_sandbox.rs`.
pub fn sandbox_for_test(pdf: &Path) -> Result<Workspace> {
    prepare_workspace_in(pdf, &std::env::temp_dir())
}

/// Build a sandbox under an explicit temporary root, for path-invariant tests.
#[doc(hidden)]
pub fn sandbox_for_test_in(pdf: &Path, root: &Path) -> Result<Workspace> {
    prepare_workspace_in(pdf, root)
}

/// Build the sandbox: copy the PDF in, render its pages, and write the text.
fn prepare_workspace_in(pdf: &Path, root: &Path) -> Result<Workspace> {
    let dir = workspace_tempdir(root, "hauksbee-extract-", "creating the extraction sandbox")?;

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
    if which_path("pdftoppm").is_some() {
        // Render every selected page at once, then collect them in selection order.
        let dpi = RENDER_DPI.to_string();
        let renders: Vec<_> = selections
            .into_iter()
            .map(|selection| {
                let stem = dir.path().join(format!("page-{:03}", selection.number));
                let page = selection.number.to_string();
                let child = Command::new("pdftoppm")
                    .args(["-png", "-singlefile", "-r", &dpi, "-f", &page, "-l", &page])
                    .arg(&copied)
                    .arg(&stem)
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn();
                (selection, stem, page, child)
            })
            .collect();
        for (selection, stem, page, child) in renders {
            match child.and_then(Child::wait_with_output) {
                Ok(o) if o.status.success() => {
                    let rendered = stem.with_extension("png");
                    if rendered.is_file() {
                        pages.push(rendered);
                        selected_pages.push(selection);
                    }
                }
                Ok(o) => {
                    say!(
                    "[model-extract] PDF page {page} failed to render, continuing without it: {}",
                    String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or("")
                )
                }
                Err(_) => {}
            }
        }
    } else {
        say!(
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
        checker: None,
    })
}

/// Run one extraction end to end, as the CLI does.
pub fn run(mut args: Args) -> Result<PathBuf> {
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
    progress::start();
    let pdf_text = extract_pdf_text(&args.pdf)?;

    // An empty kind means "you work it out". Being made to classify a part
    // before the tool will look at it is a barrier at exactly the wrong
    // moment: the datasheet says what the part is on its first page, and the
    // person asking for a model is precisely the one who may not know which
    // of our categories their part falls into.
    if args.kind_str.trim().is_empty() {
        args.kind_str = identify_kind(&args, &pdf_text)?;
        say!(
            "[model-extract] identified {} as kind '{}'",
            args.part,
            args.kind_str
        );
    }
    say!(
        "[model-extract] part={} kind={} pdf={}",
        args.part,
        args.kind_str,
        args.pdf.display()
    );

    let (part, kind) = (args.part.as_str(), args.kind_str.as_str());
    // The destination is settled before anything is sent: an existing model
    // may be hand-corrected, and finding that out after a paid run is too late.
    let sensor = is_sensor_kind(kind);
    let suffix = if sensor { ".sensor.toml" } else { ".toml" };
    let out_dir = args.out_dir.clone().unwrap_or_else(default_out_dir);
    let out_path = out_dir.join(format!("{}{suffix}", sanitise_filename(part)));
    if !args.force && out_path.exists() {
        let shown = out_path.display();
        bail!("{shown} already exists, and it may hold your corrections; hauksbee will not overwrite it. Nothing was sent. Move or delete it, write the draft elsewhere with --out-dir, or pass --force to replace it.");
    }
    // Declarative register-map sensor kinds emit a `[sensor]` spec validated
    // as a `SensorSpec`, NOT the SPICE `[[models]]` schema.
    let (raw, id) = if sensor {
        let prompt = build_sensor_prompt(part, kind, &pdf_text);
        let raw = call_backend(&prompt, &args, Reply::Sensor { part, kind })?;
        let name = validate_sensor_reply(&raw, part, kind)?.sensor.name;
        (raw, name)
    } else {
        // An explicit kind the schema cannot accept fails HERE, before any
        // datasheet text is sent anywhere. A behavioural FAMILY (charger/pmic
        // on vreg, balancer on digital) is legal: the prompt appends the
        // `[models.behavioral]` guidance and the draft comes back under the
        // base kind, which the schema accepts.
        if component_kind(kind).is_none() {
            bail!(
                "hauksbee has no '{kind}' model kind. Legal kinds: {}; behavioral families \
                 (drafted on a base kind with a [models.behavioral] block): charger, pmic, \
                 balancer. Pick the closest and declare the unmodeled behavior in the \
                 description, or omit --kind and let the extraction choose.",
                crate::validation::KIND_NAMES.join(", ")
            );
        }
        let prompt = build_prompt(part, kind, &pdf_text);
        let raw = call_backend(&prompt, &args, Reply::Model { part, kind })?;
        let id = parse_and_validate_reply(&raw, part, kind)?.id;
        (output::finalise(&raw, part, &args.pdf)?, id)
    };

    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("creating output directory {}", out_dir.display()))?;
    output::publish(&out_path, &raw, args.force)
        .with_context(|| format!("writing output to {}", out_path.display()))?;
    progress::out(&format!(
        "[model-extract] written: {}\n{id}",
        out_path.display()
    ));
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
                               # in value bits, MSB first: an R/W a datasheet numbers "bit 0"
                               # in clocking order is still rw_bit<<7; map before abstaining.
    rw_read_is_high = true     # true if a READ sets the high command bit to 1
                               # (most ST / InvenSense parts). false otherwise.
    addr_mask = 0x7f           # mask to recover the register address
    spi_mode = 0               # CPOL/CPHA mode 0..3 from the timing diagram
    spi_modes = [0, 3]         # every mode the datasheet allows, when it names several
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

OPTIONAL BLOCKS (add one only when the datasheet specifies every field it needs):
  channels = N                            # independent copies of each state
  [[sensor.state]] name, default          # per-channel state a write updates
  [[sensor.write_register]] addr, encoding (fixed-width), store = "<var>",
    default = <POR value>; [[sensor.write_register.field]] name, bits = [high, low]
  [[sensor.write_command]] name, match_mask, match_value, prefix, group_bytes,
    channel = {{ source = "auto" | "prefix_bits" | "group_bits", bits = [high, low] }},
    [[sensor.write_command.field]] name, bits; [sensor.write_command.update] state = "expr"
  [[sensor.output]] name, channel, expr   # driven-voltage law over state
  [sensor.read_frame] per_channel, bytes = ["expr", ...]   # instead of registers
  write_register and write_command are mutually exclusive.
  [sensor.pins] "<pad number>" = "<role>"   # the package's pin table, every pad
    # Bus roles: scl, sda (I2C) or cs, sck, mosi, miso (SPI), named exactly so.
    # Other roles: supplies (vdd, vddio), gnd, address straps (a0, a1, ...),
    # interrupt/alert outputs (int, int1, alert), each lowercase from the table.
  [[sensor.address_select]] address = 0x??, straps = {{ a0 = "low", a1 = "high" }}
    # one row per strap combination the datasheet lists; each strap is a
    # [sensor.pins] role tied "low" (GND) or "high" (VDD), every row names the
    # same straps, and i2c_address is one of the rows. I2C only; omit when the
    # address is fixed.

RULES:
1. Include the WHO_AM_I / device-ID register if the part has one (with its exact
   constant), plus the primary data register(s) firmware actually reads.
2. Every `expr` may only reference names declared in a [[sensor.input]].
3. Output ONLY the TOML, starting with `[sensor]`; no prose, no markdown fences.
4. Use only values stated in the datasheet; do not invent register addresses.
5. End with this provenance block. The tier and the model-error record are
   required; the model-error record is not the only record allowed:
     [sensor.source]
     tier = "datasheet-derived"
     validation = "unvalidated"
     [[sensor.source.uncertainty]]
     status = "unknown"
     parameter = "model"
     reason = "datasheet publishes no validated model error interval"
   Every printed row of the electrical, timing and ratings tables (supply and
   standby currents, timing, temperature ranges, I/O levels), field or not, is
   a `[[sensor.source.characteristic]]` record: parameter, only the printed
   min/typ/max columns, the printed unit, conditions, and a basis naming the
   printed columns; {printed_reference}. A row printing min and max yields its
   interval, never write one: no column is 0 and a typ is never a bound.
6. A control/config register firmware reads back is a const register holding
   its power-on (reset) value bytes.

{source_quantity_guidance}

{datasheet_text}

OUTPUT (TOML only, starting with [sensor]):
"#,
        source_quantity_guidance = SOURCE_QUANTITY_GUIDANCE,
        printed_reference = crate::schema::PRINTED_REFERENCE,
        datasheet_text = datasheet_text_block(pdf_text),
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
    if !spec
        .sensor
        .source
        .as_ref()
        .is_some_and(crate::schema::ModelSourceSpec::declares_model_error)
    {
        bail!("reply for {part} has no model-error record (rule 5)");
    }
    supply_relative_rows(spec.sensor.source.as_ref())?;
    Ok(spec)
}

// ── Argument parsing ──────────────────────────────────────────────────────────

/// Which LLM backend an extraction talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    /// `codex exec`, OpenAI's agent CLI.
    Codex,
    /// Headless `claude -p`, the same prompt contract as codex.
    ClaudeCode,
    /// Antigravity's `agy --print`, the same contract again.
    Agy,
    /// An OpenAI-compatible chat-completions endpoint.
    Api,
}

impl Backend {
    /// Every backend, in the order the surfaces list them.
    pub const ALL: [Backend; 4] = [
        Backend::ClaudeCode,
        Backend::Agy,
        Backend::Codex,
        Backend::Api,
    ];

    /// The stable id used on the command line, in the config file and on
    /// the wire.
    pub fn name(self) -> &'static str {
        match self {
            Backend::Codex => "codex",
            Backend::ClaudeCode => "claude-code",
            Backend::Agy => "agy",
            Backend::Api => "api",
        }
    }

    /// The display name.
    pub fn label(self) -> &'static str {
        match self {
            Backend::Codex => "Codex",
            Backend::ClaudeCode => "Claude Code",
            Backend::Agy => "Antigravity (agy)",
            Backend::Api => "OpenAI-compatible API",
        }
    }

    /// The executable an agent backend needs on PATH; none for the API.
    pub fn tool(self) -> Option<&'static str> {
        match self {
            Backend::Codex => Some("codex"),
            Backend::ClaudeCode => Some("claude"),
            Backend::Agy => Some("agy"),
            Backend::Api => None,
        }
    }

    /// The one command that makes it available.
    pub fn install_hint(self) -> &'static str {
        match self {
            Backend::Codex => "npm install -g @openai/codex   # then: codex login",
            Backend::ClaudeCode => {
                "npm install -g @anthropic-ai/claude-code   # then: claude login"
            }
            Backend::Agy => {
                "install Antigravity from https://antigravity.google, then: agy install"
            }
            Backend::Api => {
                "export OPENAI_API_KEY=...   # or name another variable with api.api_key_env"
            }
        }
    }

    /// Where the datasheet's text goes when this backend reads it.
    pub fn sends_data_to(self) -> &'static str {
        match self {
            Backend::Codex => "OpenAI, via the account codex is signed in with",
            Backend::ClaudeCode => "Anthropic, via the account claude is signed in with",
            Backend::Agy => "Google, via the account agy is signed in with",
            Backend::Api => "the endpoint named by api.base_url",
        }
    }
}

impl std::str::FromStr for Backend {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "codex" => Ok(Backend::Codex),
            "claude-code" | "claude" => Ok(Backend::ClaudeCode),
            "agy" | "antigravity" => Ok(Backend::Agy),
            "api" => Ok(Backend::Api),
            other => bail!("unknown backend '{other}': expected codex, claude-code, agy, or api"),
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl serde::Serialize for Backend {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.name())
    }
}

impl<'de> serde::Deserialize<'de> for Backend {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug)]
pub struct Args {
    pub pdf: PathBuf,
    pub part: String,
    pub kind_str: String,
    pub out_dir: Option<PathBuf>,
    /// Retry count for LLM calls (default 2, for at most three attempts)
    pub retries: usize,
    /// Model the extraction agent runs on. `None` takes the environment,
    /// then the config file, then the backend's default (see
    /// [`crate::extract_config`]).
    pub model: Option<String>,
    /// Reasoning effort for the agent backends, on the same precedence.
    pub effort: Option<String>,
    /// Which backend to call. `None` takes `HAUKSBEE_EXTRACT_BACKEND`, then
    /// the config file, then the api backend when `HAUKSBEE_LLM_API_KEY` is
    /// set, then the first agent CLI on PATH.
    pub backend: Option<Backend>,
    /// Base URL for the api backend. `None` takes `HAUKSBEE_LLM_BASE_URL`,
    /// then `https://api.openai.com/v1`.
    pub api_base: Option<String>,
    /// NAME of the environment variable holding the api key. The key itself
    /// is never accepted as a flag value and never stored: it is read from
    /// the named variable at call time. `None` takes `HAUKSBEE_LLM_API_KEY`
    /// when set, else `OPENAI_API_KEY`.
    pub api_key_env: Option<String>,
    /// The product's own `hauksbee` executable, which the agent must use as
    /// `<checker> models lint model.toml`. `None` (the standalone binary)
    /// tells the agent there is no checker rather than letting it find one.
    pub checker: Option<PathBuf>,
    /// Replace an existing `<part>.toml` rather than refuse (`--force`).
    pub force: bool,
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
            effort: None,
            backend: None,
            api_base: None,
            api_key_env: None,
            checker: None,
            force: false,
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

    /// Pick the reasoning effort. Empty means "not chosen", as for `model`.
    pub fn effort(mut self, effort: Option<String>) -> Self {
        self.effort = effort.filter(|e| !e.trim().is_empty());
        self
    }

    /// Pick the backend. `None` keeps the configured / auto-detected one.
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

    /// Name the checker executable; see [`Args::checker`].
    pub fn checker(mut self, exe: Option<PathBuf>) -> Self {
        self.checker = exe;
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
    "This sends the datasheet to an LLM backend: whichever is configured for extraction \
     (Claude Code, Antigravity, Codex, or an OpenAI-compatible API; `hauksbee models backend \
     show` or the Settings page says which). The API gets the extracted text. An agent CLI \
     runs a model with a shell on a scratch copy of the PDF, its text and page images, and \
     whatever it reads can reach that vendor: hauksbee keeps Codex (workspace-write) and Claude \
     Code out of your home and temporary directories and off the web, but Antigravity, or \
     Codex in another sandbox mode, can read anything your account can. Nothing is sent until \
     you ask for it. The result is a draft for you to check, not a measurement: a model it \
     writes carries tier \"datasheet-derived\" and the sha256 of the PDF it read.";

/// Ask a yes/no question on the terminal, the prompt going to `out`; only `y`
/// or `yes` is a yes. A pipe cannot answer, and guessing "yes" on its behalf
/// would act because someone ran the wrong command, so a non-terminal stdin
/// fails with `refusal` instead.
pub fn confirm(out: &mut dyn Write, question: &str, refusal: &str) -> Result<bool> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        bail!("{refusal}");
    }
    write!(out, "{question} [y/N] ")?;
    out.flush().ok();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

pub fn parse_args() -> Result<Args> {
    parse_args_from(std::env::args().skip(1))
}

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
            "--effort" => out.effort = Some(value("a value (low, medium, high, ...)")?),
            "--backend" => {
                out.backend = Some(value("a value (codex, claude-code, agy, or api)")?.parse()?)
            }
            "--api-base" => out.api_base = Some(value("a value")?),
            "--force" => out.force = true,
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
                          out from the datasheet): {kinds}
                          (charger|pmic|balancer are behavioural families;
                          i2c_sensor|spi_sensor give a [sensor] spec, not SPICE)
    --out-dir <dir>       Output directory (default: ~/.hauksbee/models/)
    --force               Replace an existing model of the same name
    --model <id>          Model for the extraction agent (default: the
                          backend's own, e.g. claude-opus-5 for claude-code,
                          gemini-3.8-flash for agy, gpt-5.6-sol for codex)
    --effort <level>      Reasoning effort for an agent backend (default: high)
    --backend <name>      LLM backend: claude-code, agy, codex, or api
                          (default: ~/.config/hauksbee/extract.toml, else the
                          first agent CLI found on PATH)
    --api-base <url>      Base URL for the api backend
                          (default: https://api.openai.com/v1)
    --api-key-env <NAME>  Environment variable holding the api key
                          (default: OPENAI_API_KEY). The NAME, never the key:
                          the key is read from the environment at call time.

ENVIRONMENT (each overrides the config file for one run):
    HAUKSBEE_EXTRACT_BACKEND  Backend name, as for --backend
    HAUKSBEE_EXTRACT_CONFIG   Path of the config file to read instead
    HAUKSBEE_CLAUDE_COMMAND / HAUKSBEE_AGY_COMMAND / HAUKSBEE_CODEX_COMMAND
                              Custom executable; retains the backend protocol
    HAUKSBEE_CLAUDE_MODEL / HAUKSBEE_CLAUDE_EFFORT   claude-code model / effort
    HAUKSBEE_AGY_MODEL / HAUKSBEE_AGY_EFFORT         agy model / effort
    HAUKSBEE_CODEX_MODEL / HAUKSBEE_CODEX_EFFORT     codex model / effort
    HAUKSBEE_CODEX_PROFILE    codex auth profile
    HAUKSBEE_LLM_API_KEY      API key for the api backend (setting it selects
                              that backend when nothing else chose one)
    HAUKSBEE_LLM_MODEL        Model ID for the api backend
    HAUKSBEE_LLM_BASE_URL     Base URL (default: https://api.openai.com/v1)
",
        kinds = supported_kinds().join("|"),
    );
}

// ── PDF text extraction ───────────────────────────────────────────────────────

/// The PDF's text layer via poppler's `pdftotext` (`-layout` keeps table
/// columns aligned, which page selection wants and the prompt does not).
fn pdftotext(path: &Path, layout: bool) -> Result<String> {
    if which_path("pdftotext").is_none() {
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
/// extraction still runs: the agent backends read the copied PDF in their own
/// scratch workspace, so a sentinel stands in for the text.
///
/// The sentinel carries no path. The original location is local metadata an
/// API request must not leak, and the API backend cannot open the file anyway,
/// which is why `api_source_available` refuses that combination outright.
fn extract_pdf_text(path: &Path) -> Result<String> {
    if let Ok(text) = pdftotext(path, false) {
        progress::pages(text.matches('\u{000c}').count());
        if !text.trim().is_empty() {
            return Ok(truncate_to_chars(&text, 60_000));
        }
    }
    say!("[model-extract] pdftotext unavailable or produced no text; local agent backends will read the copied PDF directly");
    Ok(PDF_TEXT_UNAVAILABLE.to_string())
}

fn extract_pdf_pages_text(path: &Path) -> Result<Vec<PdfPageText>> {
    let text = pdftotext(path, true)?;
    let pages = split_pdf_pages_text(&text);
    if pages.is_empty() {
        bail!("the PDF text layer contained no pages");
    }
    Ok(pages)
}

fn split_pdf_pages_text(text: &str) -> Vec<PdfPageText> {
    text.split_terminator('\u{000c}')
        .enumerate()
        .map(|(index, text)| PdfPageText {
            number: index + 1,
            text: text.to_string(),
        })
        .collect()
}

/// Heading terms per topic; a `|`-joined CJK term is the topic word a heading
/// contains (电路应用信息 holds 应用信息), matched by [`topic_score`]'s first-token rule.
#[rustfmt::skip] // one topic per line; a vertical list of short terms hides the table
const PAGE_TOPICS: &[(&str, &[&str])] = &[
    ("recommended operating conditions", &["recommended operating conditions", "推荐工作条件|推荐操作条件|推奨動作条件"]),
    ("absolute maximum ratings", &["absolute maximum ratings", "limiting values", "极限|最大额定值|絶対最大定格"]),
    ("switching characteristics", &["switching characteristics", "开关特性|时序特性|スイッチング特性"]),
    ("electrical characteristics", &["electrical characteristics", "电特性|电气特性|电气参数|電気的特性"]),
    ("pin functions or package pinout", &["pin functions", "terminal functions", "pin configuration",
        "package pinout", "top view", "引脚|管脚|顶视图|ピン配置|端子説明"]),
    ("functional description or function table", &["functional description", "function table",
        "truth table", "功能描述|真值表|逻辑功能|機能説明"]),
    ("application information or programming equations", &["application information",
        "typical application", "design procedure", "programming equation", "典型应用|应用信息|应用电路|アプリケーション"]),
    ("ordering or package information", &["ordering information", "订购信息|订货信息|封装信息|外形尺寸"]),
];

fn topic_score(text: &str, terms: &[&str]) -> usize {
    let terms: Vec<&str> = terms.iter().flat_map(|t| t.split('|')).collect();
    let lower = text.to_ascii_lowercase();
    let occurrences: usize = terms.iter().map(|term| lower.matches(term).count()).sum();
    if occurrences == 0 {
        return 0;
    }
    let heading = lower
        .lines()
        .filter(|line| !line.trim().is_empty())
        .any(|line| {
            let heading_text = line.trim().trim_start_matches(|c: char| {
                c.is_ascii_digit() || ".、".contains(c) || c.is_whitespace()
            });
            let first = heading_text
                .split_whitespace()
                .next()
                .filter(|t| t.chars().count() <= 10);
            let table_heading = heading_text
                .strip_prefix("table ")
                .and_then(|text| text.split_once(". ").map(|(_, title)| title));
            let detailed_heading = heading_text.strip_prefix("detailed ");
            terms.iter().any(|term| match term.is_ascii() {
                false => first.is_some_and(|t| t.contains(term)),
                true => {
                    heading_text.starts_with(term)
                        || table_heading.is_some_and(|title| title.starts_with(term))
                        || detailed_heading.is_some_and(|title| title.starts_with(term))
                }
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
                        "functional description or function table" => 55_000,
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

/// Where `cmd` resolves on PATH, if anywhere. Shared with the config
/// resolver so "is it installed" is answered the same way everywhere.
pub(crate) fn which_path(cmd: &str) -> Option<PathBuf> {
    let extensions =
        command_extensions(cmd, cfg!(windows), std::env::var("PATHEXT").ok().as_deref());
    let directories = if Path::new(cmd).is_absolute() || Path::new(cmd).components().count() > 1 {
        vec![PathBuf::new()]
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?).collect()
    };
    directories.into_iter().find_map(|dir| {
        extensions
            .iter()
            .map(|suffix| dir.join(format!("{cmd}{suffix}")))
            .find(|candidate| executable_file(candidate))
    })
}

fn command_extensions(cmd: &str, windows: bool, pathext: Option<&str>) -> Vec<String> {
    if !windows || std::path::Path::new(cmd).extension().is_some() {
        return vec![String::new()];
    }
    pathext
        .unwrap_or(".COM;.EXE;.BAT;.CMD")
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| extension.to_ascii_lowercase())
        .collect()
}

fn executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    let runnable = |m: &std::fs::Metadata| {
        std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 != 0
    };
    #[cfg(not(unix))]
    let runnable = |_: &std::fs::Metadata| true;
    path.metadata().is_ok_and(|m| m.is_file() && runnable(&m))
}

fn truncate_to_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

// ── Prompt construction ───────────────────────────────────────────────────────

const DATASHEET_TEXT_BEGIN: &str = "<<<HAUKSBEE_DATASHEET_TEXT_BEGIN>>>";
const DATASHEET_TEXT_END: &str = "<<<HAUKSBEE_DATASHEET_TEXT_END>>>";
const PDF_TEXT_UNAVAILABLE: &str = "<<<HAUKSBEE_PDF_TEXT_UNAVAILABLE>>>";

/// Both prompts' datasheet section, marked so an agent backend can swap it out.
fn datasheet_text_block(pdf_text: &str) -> String {
    let text = truncate_to_chars(pdf_text, 40_000);
    format!("DATASHEET TEXT (truncated):\n{DATASHEET_TEXT_BEGIN}\n{text}\n{DATASHEET_TEXT_END}")
}

const SOURCE_QUANTITY_GUIDANCE: &str = r#"SOURCE QUANTITIES:
- Define each destination quantity and its units before matching a datasheet row to it.
- Every printed row gets a record, whether or not a key exists for it; never delete a
  sourced record to pass the checker. A typical value is never a guaranteed limit.
- Keep every number with its stated test conditions, supply, load, temperature, package,
  and min/typ/max column; do not transplant it to another condition.
- Every condition variant of a row (temperature range, supply, package, bias) is its
  own record. Where two places state different values for one quantity, record each
  with its own basis plus an `unknown` record for it naming the conflict.
- Omit unsupported facts, or mark them explicitly unknown only where the destination
  schema provides that representation. Never invent a field or uncertainty schema."#;

const PIN_ROLE_GUIDANCE: &str = r#"The listed names are canonical electrical aliases, not an exhaustive list of valid pin
functions. I/O direction alone is not a function. For regulators only, `in`, `out`,
and `gnd` mean supply input, regulated output, and ground; do not use them as generic
input/output labels. Preserve source-named switching, programming, NC, rail, and
exposed-pad functions as stable lowercase roles instead of coercing them into those
aliases. Keep an executable binder's canonical electrical alias distinct from any
additional freeform source name (record additional names in the pad comment); a
freeform role does not imply an executable primitive binds it. Preserve every physical
pad identifier exactly, including alphanumeric and exposed-pad identifiers.
`unknown_pad_<identifier>` (with a comment) is only for a pad whose function the
datasheet leaves unresolved, or the identity-only abstention for an unresolved package;
abstaining on params never makes a resolved pin table unknown: a pad the pin table names
keeps its source-named role. Never invent a pin role."#;

/// Whether the API backend has anything to work from: it cannot open the local
/// PDF, so a prompt carrying the no-text sentinel would reach it with nothing
/// but the sentinel itself.
fn api_source_available(prompt: &str) -> bool {
    !prompt.contains(PDF_TEXT_UNAVAILABLE)
}

/// Agent backends can read the sandboxed PDF, complete text dump, and selected
/// page images directly. Repeating 40,000 characters of layout-lost PDF text in
/// their instruction competes with those higher-fidelity sources: in measured
/// runs it turned a successful table extraction into a one-word component-kind
/// reply. The API backend has no local-file access, so it keeps the text.
fn prompt_for_agent_backend(prompt: &str) -> String {
    let Some((before, rest)) = prompt.split_once(DATASHEET_TEXT_BEGIN) else {
        return prompt.to_string();
    };
    let Some((_embedded, after)) = rest.split_once(DATASHEET_TEXT_END) else {
        return prompt.to_string();
    };
    format!(
        "{before}{DATASHEET_TEXT_BEGIN}\n\
         [Not embedded for this local agent backend. Read the attached page images first. \
         If a needed fact is absent, open datasheet.txt or datasheet.pdf in the sandbox.]\n\
         {DATASHEET_TEXT_END}{after}"
    )
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

Component kind: {kind_line}

If the request names a family of variants (output voltages, packages, grades),
model the variant the request implies. Never merge variants, and widen
`value_re` only to that variant's ordering-table codes, never to a family root
its siblings share. Variants are the exception, only where the datasheet itself
prints distinct orderable variants: when the name you were given does not pin
one and they differ in pinout or output form, emit one complete [[models]] per
variant, each with the shared family `value_re`, `variant_of = "<family>"` and
`[models.variant]` (`orderable_re`, `orderable` = its ordering-table codes,
`package`, `basis`; no default).

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

{datasheet_text}

Produce a TOML model entry that exactly conforms to the hauksbee-models schema.
The entry must use [[models]] array syntax and include:
- id: lowercase part number (e.g. "{part_lower}")
- kind: "{entry_kind}"
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
kind = "{entry_kind}"
description = "one line"

[models.match]
value_re = '(?i)^{part_re}$'

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

PIN ROLE NAMES for kind="{entry_kind}": {pin_roles}
  Use these exact spellings where listed. They are looked up by exact string, so
  "output" instead of "out", or "ground" instead of "gnd", can make the part
  bind OPEN. Map every physical pad identity in the selected package, including
  explicit NCs, exposed pads, and repeated supplies/grounds. Use one stable
  lowercase role per pad (`_n` for active-low), use it verbatim everywhere else,
  and preserve other stated functions/aliases in that pad's TOML comment. Never
  merge or omit a pad merely because its function is unknown. Only pads of one
  net share a role: each unit of a multi-unit part takes its own suffixed roles.

{pin_role_guidance}

For kind="{entry_kind}", the required params are:
{required_params}
{optional_params}
{digital_hint}
[models.ratings]: these are the ONLY valid keys (any other key fails
validation). Read them from the Absolute Maximum Ratings / Limiting Values and
Thermal Characteristics tables, or the table a line names. Include every key
the datasheet states for the selected package; omit a key it does not state (do
NOT invent it). Ratings are magnitudes: for a limit printed negative (PNP,
negative rail) write |value| and keep the printed sign in the source comment.
A stated limit with no key here goes in a characteristic record (rule 0), never
under an invented key. A rating printed for several conditions the part number
does not settle (board copper or mounting, package, ambient) is read from the
least favourable one, as a checker must not trust a better board than the user's:
the lowest current and power, the highest thermal resistance, and
max_current_a, max_power_w and theta_ja_c_per_w all from that same condition;
the other rows go in characteristic records:
{ratings_hint}

[[models.envelope]]: read the Recommended Operating Conditions and Absolute
Maximum Ratings tables. Emit one supply_range entry for every SUPPLY-class pin
whose recommended minimum and maximum are both published. Emit rail_order for
each explicit relation such as VCCA <= VCCB. From an op-amp's or comparator's
VICR/CMVR row, emit one input_common_mode per channel: `supply` its positive
supply, `inputs` that channel's two inputs, and `headroom_v` the test supply
minus the range's upper end (0 V to V+ - 1.5 V is 1.5; none reaching V+), read
from the 25 °C row when a full-temperature-range row is also printed, and `basis`
names that row. Pins, supplies and inputs are roles from [models.pins], never pad
numbers. Every entry requires `basis` naming the exact table and row. Add
`abs_max_v` only when the absolute-maximum table publishes it. There is no
envelope without a table row to cite: omitting this section is correct when the
datasheet lacks the necessary tables. Never infer a bound from a typical value,
application schematic, or family convention.

WHERE TO LOOK AND HOW TO READ IT:
- Supply bounds belong in Recommended Operating Conditions.
- Absolute Maximum Ratings is a separate stress table and does not promise
  functional operation.
- Read the numbered Pin Functions table first, then cross-check it against the
  package top-view figure and a typical application schematic before answering.
- Application Information and Design Procedure sections often contain the
  programming equations. Cite the equation and every stated validity condition.
- Before answering, re-read every cited row and footnote, then double-check its
  units, sign, package variant, min/typ/max column and test bias (in the table
  header, row or footnote; a value is worthless without it: cite it beside it).
- Read each value from one printed cell, in a family table the exact orderable
  part's column; never average, interpolate between cells, or make a midpoint.

{source_quantity_guidance}

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
  P6. ONE COLUMN PER PACKAGE. When the pin table prints a pin-number column per
      package, record every column, names as printed, after [models.source]:
        [[models.source.pinout]]
        package = "<package as printed>"
        pins = {{ "1" = "<printed name>", "2" = "<printed name>" }}
      The description names only your package. Read [models.pins] from that
      package's own column and its own figure: a figure labelled with another
      package is not yours, and the checker compares the two.

IMPORTANT RULES:
0. Emit this machine-readable source block immediately after the model's description:
     [models.source]
     tier = "datasheet-derived"
     validation = "physical-bounds-only"
     [[models.source.uncertainty]]
     status = "unknown"
     parameter = "model"
     reason = "datasheet publishes no validated model error interval"
   (Validated model-error bounds the source publishes replace that record with
   `status = "interval"`, `kind = "empirical-error"`, low, high, unit and basis.)
   A param read from a printed row is written as that whole row, with only the
   columns it prints, in its printed unit:
     iq_a = {{ typ = 65, max = 90, unit = "uA", conditions = "VIN = 5 V", basis = "Table 7.5, IQ, typ/max" }}
   Hauksbee converts the unit, picks the column each param reads (the typ, else
   the min/max midpoint; a guaranteed level such as vol or dropout_v its limit)
   and derives the min/max interval: never pick a column, convert a unit or
   write an interval yourself. A one-sided row is never a nominal. Every other
   printed row of any table (ratings, thermal, each condition), key or no key,
   is a characteristic record with the same fields, also on an identity-only
   card (a breakdown row is not an absolute-maximum rating):
     [[models.source.characteristic]]
     parameter = "<datasheet symbol and name>"
     max = 0.4  # only the printed columns among min, typ, max
     unit = "<printed unit>"
     conditions = "<the row's test conditions>"
     basis = "<table/row citation>, <printed columns, e.g. typ/max>"
   Each number keeps its printed sign in its printed column (±5 is min = -5,
   max = 5; a "—" column stays out). A supply-relative limit (0.7 VDD, VDD + 0.5)
   is no number: quote it in `conditions` and omit that column, never a rail as
   `unit`. min <= typ <= max by signed value (a row printed negative may keep its
   magnitude order); a threshold printed as an offset from another spec is a
   signed offset, ascending, with that spec named in conditions. Never invent a
   percentage, derive a bound from a typical value, or substitute a model clamp
   for a published guarantee. `physical-bounds-only` is the whole validation claim.
1. Add a comment on each param/rating line citing where in the datasheet you found
   the value, e.g.: `# Source: Table 6.3, typ column`
2. Use only values explicitly stated in the datasheet; do NOT guess. A SPICE
   model the sheet prints (.MODEL/.SUBCKT) is copied verbatim, citing it. Other
   SPICE params (e.g. `is`, `n`, `rs`) you may derive from a stated operating
   point (e.g. VBE at a known IC) or fit to a typical-characteristics figure
   (e.g. an IF-VF or ID-VDS curve) read across its plotted range, saying so in
   the comment with the figure, the points read and their conditions; a curve
   reading is typical only, never a min/max, a rating or an uncertainty interval.
   A BJT `is` comes only from a forward-active VBE(on) row or curve, lowest IC
   first; never from VBE(sat) (saturated, forced beta) or a high-current point.
   Record every fT, Cob/Ccb, Cib/Ceb, Ciss, Crss, CT, trr, V(BR) and VZ row as a
   characteristic with its symbol, unit and bias: Hauksbee derives tf, cjc, cje,
   cgd, cgs, cjo, tt and bv/ibv from them. A Schottky (name it in the
   description) takes no tt: its trr is capacitive recovery, not stored charge.
3. Identity-only is a last resort. A typ-only or max-only row is usable, cited
   as printed; Hauksbee fits diode is/n/rs to VF rows and a MOSFET's vto/kp come
   from VGS(th) and RDS(on) rows as above, so the checker refuses an identity-only
   card whose cited rows suffice. Unnumbered leads take pad names A/K, G/D/S or
   B/C/E. A required param genuinely absent and not derivable or fittable is never
   estimated: one missing param takes an `unknown` record where the kind's params
   above allow one. Only when the kind's behaviour cannot be written even so, emit
   an identity-only card (`identity_only = true`, `warning`, `unlocked_by`) still
   carrying every field the datasheet states, and name the missing fact as
   `ABSTAIN: datasheet does not state ...` in the description or an unknown source
   record. A named abstention is correct; a guessed field is wrong.
4. For every real behavior the selected schema and params do not represent,
   the description must say `does not model ...` and name that behavior, and
   `[models.coverage]` must list each as a snake_case id in `missing` (what the
   card does model goes in `implements`, e.g. `missing = ["thermal_shutdown"]`). Do not
   let a partial card read as a complete model of the component. Never delete a
   sourced fact or drop a record between drafts to pass validation: fix the
   record, or move the fact into a characteristic record (rule 0).
5. Output ONLY the TOML block, starting with [[models]]; no prose, no markdown
   fences, no leading or trailing text.
6. A plain number (a rating, a derived or fitted value) is in the unit its key
   or the param meanings name, else in A, V, W, s, F or ohm.
7. The checker rejects params outside these physical bounds, and one sitting on
   a bound unless a cited row prints it: a value the datasheet does not give is
   omitted or an `unknown` record, never a placeholder at a bound. A |x| bound is on
   the magnitude and accepts EITHER SIGN: write a signed value as the datasheet
   states it. A negative regulator (79xx, any output below GND) takes a NEGATIVE
   vout, e.g. -5.0 for an LM7905: it is stamped as a source against ground, so
   the magnitude would make a negative supply into a second positive one.
{param_bounds}
{behavioral_hint}
OUTPUT (TOML only, starting with [[models]]):
"#,
        part_lower = part.to_lowercase(),
        part_re = regex::escape(&part.to_uppercase()),
        legal_kinds = crate::validation::KIND_NAMES.join(", "),
        entry_kind = behavioral_family_base_kind(kind).unwrap_or(kind),
        kind_line = match behavioral_family_base_kind(kind) {
            Some(base) => format!(
                "{base} (the {kind} family, written as a {base} entry; see its notes below)"
            ),
            None => kind.to_string(),
        },
        datasheet_text = datasheet_text_block(pdf_text),
        required_params = required_params_for_kind(kind),
        optional_params = optional_params_for_kind(kind),
        pin_roles = pin_roles_for_kind(kind),
        ratings_hint = crate::schema::Ratings::KEY_GUIDE,
        param_bounds = param_bounds_for_kind(kind),
        behavioral_hint = behavioral_hint_for_kind(kind),
        digital_hint = digital_hint_for_kind(kind),
        pin_role_guidance = PIN_ROLE_GUIDANCE,
        source_quantity_guidance = SOURCE_QUANTITY_GUIDANCE,
    )
}

/// Rule 7, from the checker's own rules: a hand copy once capped `kp` at 1.0
/// while the checker (rightly, for power FETs) allowed 1000.
fn param_bounds_for_kind(kind: &str) -> String {
    let lines = component_kind(kind).map_or_else(Vec::new, crate::validation::param_bounds);
    if lines.is_empty() {
        return "   (none for this kind)".into();
    }
    format!("   - {}", lines.join("\n   - "))
}

/// The schema kind a prompt kind resolves to (a behavioural family to its base).
/// The prompt names only these: a backend that invents "charger" as the entry
/// kind burns every attempt against a parser that was never going to accept it.
fn component_kind(kind: &str) -> Option<ComponentKind> {
    let base = behavioral_family_base_kind(kind).unwrap_or(kind);
    serde_json::from_value(serde_json::Value::from(base)).ok()
}

/// A complete shape demonstration from a different component class.
///
/// Each candidate teaches all four failure-prone shapes. The requested kind is
/// filtered out on purpose: a same-class card offers its numbers to copy
/// (4f94d9cf), and at least two examples from other part classes remain.
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
[[models.source.uncertainty]]
status = "unknown"
parameter = "model"
reason = "datasheet publishes no validated model error interval"
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
[[models.source.uncertainty]]
status = "unknown"
parameter = "model"
reason = "datasheet publishes no validated model error interval"
[models.match]
value_re = "(?i)^TLV3201"
[models.params]
out_lo = { max = 100, unit = "mV", conditions = "VS=5 V, ISINK=4 mA", basis = "Electrical Characteristics, VOL max" }
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
[[models.source.uncertainty]]
status = "unknown"
parameter = "model"
reason = "datasheet publishes no validated model error interval"
[[models.source.characteristic]]
parameter = "IIH input leakage current"
max = 5.0
unit = "uA"
conditions = "VI = 5 V, EN = 0 V"
basis = "Electrical Characteristics, IIH row, max column only"
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
  # Units are checked: v_<role> is volts, a param's suffix is its unit (_v, _a,
  # _ohms, _s, _f, _hz, _w), a bare number is dimensionless, and the result must
  # be amps (current) or volts (voltage). Give a unit-bearing constant a param.

Only include the blocks the datasheet supports, each number cited (rule 1).
"#
    );
    let specific = match kind {
        "charger" => {
            "\nFor a CHARGER specifically: identify the input pin (PVIN/VIN), the \
             battery/charge-output pin (BAT/VBAT), and any current-sense or \
             current-programming pins. A SWITCHING charger fills \
             [models.behavioral.converter] with topology and the regulated charge \
             voltage, and [models.behavioral.converter.iin_program] if the datasheet \
             gives the sense-resistor input-current-limit equation. A LINEAR charger \
             whose charge current is set by one resistor (I = K / R) needs no \
             [models.behavioral] block; it takes, with K's published min/typ/max \
             (or the programmed-current rows) as characteristic records:
  [models.current_program]
  pin = \"<programming pin role>\"
  semantics = \"regulated_current\"
  current_in_roles = [\"in\"]
  current_out_roles = [\"out\"]
  max_operating_current_a = <highest specified programmed current, A>
  equation = \"inverse_resistance\"
  k_volts = <typical K in volts>
  # k_volts is SI (A·Ω = V). A law printed as I(mA) = K / R(kΩ) keeps K
  # unchanged, because mA·kΩ = V: I = 1000 / R(kΩ) in mA is k_volts = 1000."
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

/// The pin role names the BINDER accepts for a kind, verbatim.
///
/// The binder looks these up by exact string (`roles.get("out")`), so a model
/// that calls the pin "output" passes every other check and then binds OPEN.
/// Validation catches that, but only once the run is over, and a retry costs
/// another three minutes and another bill. Stating the vocabulary up front is
/// the cheap half of the same guarantee.
fn pin_roles_for_kind(kind: &str) -> &'static str {
    match kind {
        "diode" => "anode, cathode; an array indexes each diode's own pad: cathode1, cathode2",
        "bjt_npn" | "bjt_pnp" => "collector, base, emitter; a dual: collector_q1, base_q1, ..._q2",
        "nmos" | "pmos" => "drain, gate, source",
        "vreg" | "charger" | "pmic" => "in, out, gnd  (and en / adj / fb when the part has them)",
        "opamp" | "comparator" => {
            "in_plus, in_minus, out, vcc, vee; a dual or quad suffixes each unit's signal roles \
             _a to _d (out_a, in_minus_a, in_plus_a, out_b, in_minus_b, ...), supplies unsuffixed"
        }
        "analog_switch" => "in_out_a, in_out_b (SPST) or com, s0, s1 (SPDT)",
        "digital" | "shift_register" => {
            "vcc or vdd; gnd or vss; functional roles copied from the pin/function table, with repeated channels indexed consistently (for example a1, y1, a2, y2) and active-low roles suffixed _n"
        }
        _ => "",
    }
}

fn digital_hint_for_kind(kind: &str) -> &'static str {
    if !matches!(kind, "digital" | "shift_register") {
        return "";
    }
    r#"
DIGITAL BEHAVIOR (required unless this must be an explicit identity-only
abstention): add `[models.logic]`. Its `inputs` and `outputs` arrays contain pin
ROLE names from `[models.pins]`, not pad numbers. Every functional input and
output must be listed; inputs, outputs, and registers share one UNIQUE namespace.
Do not list a bidirectional signal in both arrays: that fails validation. This
bounded contract does not describe memory buses; abstain if it cannot represent
required behavior. Give every output one `[models.logic.comb]` expression, using
only `!`, `&`, `^`, `|`, parentheses, 0/1, input/output names, and `register[bit]`.
For sequential behavior add `[[models.logic.register]]` with a unique `name`,
`bits` (1..64), and `clock = { pin = "role", edge = "rising" }` (or "falling")
paired with `op`: "shift_left", "shift_right", "load", "count_up", or "count_down".
Shifts require `data_in` naming an input; load requires a same-width register or
an input for a 1-bit register. Count ops must omit `data_in`. Optional controls:
`reset = { pin = "role", active = "low", value = 0 }`, `load = { pin = "role",
active = "low", data = ["bit0_role", "bit1_role"] }` (one input per register bit),
`clock_enable = { pin = "role", active = "high" }`. Either active polarity is
allowed. Fixed priority: asynchronous reset, asynchronous load, enabled clock
edge, else hold. Registers update simultaneously from pre-step values. Bit 0 is
LSB; shift_left inserts at bit 0, shift_right at bits-1. Match the function table;
abstain if its semantics differ, rather than inventing priority controls.
For high-impedance outputs, `[models.logic.tristate]` maps each output name to
`{ enable = "input_role", active = "low" }` (or "high"); inactive means high-Z.
An open-drain or open-collector output has no VOH (the board pull-up sets its
high level): give it the constant `0` and a tristate entry enabled exactly while
it sinks, as `od_out` below; abstain if no single input decides that. For a
purely combinational part omit registers; this symbolic example is valid
syntax, not the target part's function:
```toml
[models.logic]
inputs = ["left", "right"]
outputs = ["result", "od_out"]
[models.logic.comb]
result = "left ^ right"
od_out = "0"
[models.logic.tristate]
od_out = { enable = "right", active = "low" }
```
A level-sensitive (transparent) latch is combinational feedback on its own output:
`q = "le & d | !le & q"` passes `d` while `le` is high and holds otherwise (`d` may be
a `register[bit]`); `[models.logic.init]` may set its power-on level (`q = 0`).

In `[models.params]`, `supply_pin` and `gnd_pin` are STRINGS containing physical
pad identifiers exactly as keyed in `[models.pins]` (for example `"pad_id"`), not
numbers or role names. These fields are currently metadata; the executable
binder does not read them. For supply accounting it instead finds `[models.pins]`
ROLE names `vcc` or `vdd` and `gnd` or `vss`, so map those roles consistently.
Use indexed functional roles (`a1`, `y1`, etc.) consistently for repeated
channels; do not collapse multiple gates onto generic `in`/`out`.
Choose `voh`, `vol`, `vih`, and `vil` at one cited supply/load/temperature corner;
do not mix corners. State whether each source value is typical or guaranteed and
that fixed simulation levels are not universal datasheet bounds. VT+ and VT- are
static switching thresholds; do not substitute dynamic or noise values. A Schmitt
input takes vih = VT+ and vil = VT- as plain typ numbers (no typ: VT+ max, VT- min),
never as a VT row with min and max, which reads inverted. Only when
the requested scenario needs a guaranteed transition bound, use VT+ maximum for
a high-going transition and VT- minimum for a low-going transition. Inputs decide
low at or below `vil`, high at or above `vih`, and hold their previous state in
between: this model has hysteresis even when the datasheet calls that band
undefined. `tpd_s` is metadata today; digital transitions occur without applying
it, so say that propagation delay and timing races are not modeled.
If a selected package's pad function is unresolved, keep the pad as
`unknown_pad_<identifier>` and mark the ambiguity in its TOML comment. If the
package variant itself is unresolved, do not combine variants or guess a map:
use the identity-only abstention and name the unresolved package.
"#
}

fn required_params_for_kind(kind: &str) -> &'static str {
    match kind {
        "diode" => "is, n, rs, fitted to the cited VF-at-IF rows (25 C; typ, else max)",
        "bjt_npn" | "bjt_pnp" => "is, bf, nf (no VBE(on) row or curve: an `unknown` record each)",
        // Never the word "is" here: optional_params_for_kind would drop the `is` param.
        "nmos" | "pmos" => {
            "vto, kp. Level-1 square law with w_over_l unset (1): vto keeps the SPICE sign \
             (negative for an enhancement PMOS), kp in A/V^2, and rd/rs are the drain and source \
             ohmic series resistances, not the channel. Small-VDS on-resistance here equals \
             1/(kp*(|VGS| - |vto|)) + rd + rs, so an RDS(on) typ row fixes kp and rd + rs \
             together. With rows at two VGS values, solve kp from their difference and split the \
             residue at the higher-VGS row evenly between rd and rs; with one row, take kp from \
             gFS at its ID (kp = gFS^2 / (2*ID)) and put the residue in rd + rs, or, without \
             gFS, derive kp from that row alone with rd = rs = 0. Comment each value `derived \
             from RDS(on) row at VGS=..., ID=..., typ column`"
        }
        "vreg" => {
            "fixed: vout, dropout_v, iq_a; adjustable (ADJ/FB pin): vref (the reference, \
             never the output), dropout_v and the quiescent current param the checker asks \
             for, with no vout. vout: the ordered variant's nominal output, with its \
             output-voltage-accuracy row's min/max if printed; a range spanning the family's \
             selectable outputs is never vout. iq_a: the no-load ground (or supply) current; \
             max_current_a (a [models.ratings] key, never a param): the rated output current, \
             never the absolute-maximum IOUT; dropout_v: the row for this part's own \
             output-voltage option, at the rated output current when several loads are printed"
        }
        "opamp" => {
            "gain, and swing_lo/swing_hi (output headroom from each supply pin, max column) or \
             rail_lo/rail_hi (swing limits at a stated test supply, VS minus the swing, never the \
             supply or ground itself); pole_hz is GBW / gain, slew is in V/us"
        }
        "comparator" => {
            "out_lo, out_hi, hysteresis (V). hysteresis is the datasheet VHYS, the FULL width \
             between the rising and falling trip points (VT+ minus VT-), never the input offset \
             VOS. With no hysteresis row, write hysteresis = 0.0 (none designed in) plus an \
             `unknown` record for `hysteresis` whose reason names the absence; that never makes \
             the card identity-only. An open-collector or open-drain output instead sets \
             open_collector = true with ron_ohms and leak_s, and the board pull-up supplies its \
             high level. The binder ignores out_lo/out_hi/hysteresis when open_collector = true, \
             and ron_ohms/leak_s otherwise"
        }
        "analog_switch" => "ron, roff, vth (no printed off-state figure: an `unknown` record for roff)",
        "digital" => {
            "voh (omit for an open-drain output), vol, vih, vil, tpd_s; supply_pin and gnd_pin \
             are quoted physical pad identifiers"
        }
        "shift_register" => {
            "bits, voh, vol, vih, vil, tpd_s; supply_pin and gnd_pin are quoted physical pad identifiers"
        }
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
        "charger" | "pmic" => {
            "vout, dropout_v, iq_a (the behaviour is in [models.behavioral] or \
             [models.current_program]). A dropout_v or iq_a printed as no single value (no \
             dropout row; only a limit or several supply-current rows) is omitted with an \
             `unknown` record for it naming why, its rows staying characteristic records: never \
             a reason for identity-only"
        }
        "balancer" => "(the leak law is in [models.behavioral]; no required numeric params)",
        _ => "(see db/README.md for kind-specific requirements)",
    }
}

/// Every other param the kind's consumers read (`param_names`), so a datasheet
/// fact with a home is not dropped because only the required ones were named.
fn optional_params_for_kind(kind: &str) -> String {
    let named: Vec<&str> = required_params_for_kind(kind)
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .collect();
    let rest: Vec<&str> = component_kind(kind)
        .map_or(&[][..], crate::param_names::known_param_names)
        .iter()
        .copied()
        .filter(|n| !named.contains(n))
        .collect();
    let meanings = component_kind(kind).map(crate::param_names::param_meanings);
    let meanings = meanings
        .filter(|m| !m.is_empty())
        .map_or(String::new(), |m| {
            format!("\nParam meanings and units (write each in that unit): {m}")
        });
    if rest.is_empty() {
        return meanings;
    }
    format!(
        "Optional params it also reads (add each one the datasheet states): {}{meanings}",
        rest.join(", ")
    )
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

    /// The reply with the conservative unknown model-error record added where
    /// one is missing (never replaced): it only qualifies results; a retry costs a run.
    fn complete(self, raw: String) -> String {
        use toml_edit::{ArrayOfTables, DocumentMut, Item, Table};
        let (path, validation) = match self {
            Reply::Model { .. } => ("models", "physical-bounds-only"),
            Reply::Sensor { .. } => ("sensor", "unvalidated"),
            Reply::Word => return raw,
        };
        let Ok(mut doc) = raw.parse::<DocumentMut>() else {
            return raw;
        };
        let blank = Table::from_iter([("tier", "datasheet-derived"), ("validation", validation)]);
        let is_model = |r: &Table| r.get("parameter").and_then(Item::as_str) == Some("model");
        let why = "datasheet publishes no validated model error interval";
        let record = std::iter::zip(["status", "parameter", "reason"], ["unknown", "model", why]);
        let mut inserted = false;
        let tables: Vec<&mut Table> = match doc.get_mut(path) {
            Some(Item::ArrayOfTables(a)) => a.iter_mut().collect(),
            t => t.and_then(Item::as_table_mut).into_iter().collect(),
        };
        for table in tables {
            let source = table.entry("source").or_insert(Item::Table(blank.clone()));
            let records = source.as_table_mut().map(|t| t.entry("uncertainty"));
            let records = records.map(|e| e.or_insert(ArrayOfTables::new().into()));
            match records {
                Some(Item::ArrayOfTables(a)) if !a.iter().any(is_model) => {
                    a.push(Table::from_iter(record.clone()))
                }
                _ => continue,
            }
            inserted = true;
        }
        if inserted {
            say!("[model-extract] warning: model-error record missing; inserted the conservative unknown record");
        }
        // A param omitted where a cited row fixes it is filled, with its working,
        // and a diode law off its cited VF rows is replaced by their fit.
        let db = toml::from_str::<crate::schema::DbFile>(&raw).map_or(vec![], |d| d.models);
        let models = doc.get_mut(path).and_then(Item::as_array_of_tables_mut);
        let tables = models.into_iter().flat_map(ArrayOfTables::iter_mut);
        for (entry, table) in db.iter().zip(tables) {
            let mut derived = crate::param_names::derivations(entry);
            derived.retain(|d| !entry.params.0.contains_key(d.0));
            derived.extend(crate::param_names::diode_refit_params(entry));
            let params = (!derived.is_empty()).then(|| table.entry("params"));
            let params = params.map(|p| p.or_insert(Item::Table(Table::new())));
            let Some(params) = params.and_then(Item::as_table_like_mut) else {
                continue;
            };
            for (name, v, how) in derived {
                say!("[model-extract] {}: {how}", entry.id);
                let text = format!("{v:e}"); // 6.3662e-10, not 0.00000000063662
                let mut value = text.parse().unwrap_or(toml_edit::Value::from(v));
                value.decor_mut().set_suffix(format!("  # {how}"));
                params.insert(name, Item::Value(value));
            }
            inserted = true;
        }
        if inserted {
            return doc.to_string();
        }
        raw
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
        let raw = reply.complete(extract_toml_block(&canned, reply.table()));
        reply.check(&raw)?;
        return Ok(raw);
    }

    // Flags, then environment, then the config file, then defaults: one
    // resolver for every surface, so the CLI and the web page cannot disagree
    // about what is about to read the datasheet.
    let resolved = resolve_settings(args)?;
    let chosen = resolved.backend;
    say!(
        "[model-extract] backend: {} ({})",
        resolved.summary(),
        resolved.backend_source.describe()
    );
    // The API backend has no local-file access, so a prompt carrying the
    // no-text sentinel would ship it nothing to read. Refuse before sending
    // rather than spend the call.
    if chosen == Backend::Api && !api_source_available(prompt) {
        bail!(
            "the API backend cannot read the local PDF and pdftotext is unavailable. \
             Install Poppler/pdftotext, then retry; nothing was sent."
        );
    }

    let tool = match chosen {
        Backend::Api => {
            let request = ApiRequest::prepare(&resolved.api)?;
            return validated_attempts(resolved.retries, prompt, reply, "api", |p| {
                Ok(extract_toml_block(&request.send(p)?, reply.table()))
            });
        }
        agent => resolved.agent(agent).expect("agent").command.as_str(),
    };
    require_tool(&resolved)?;

    // An agent backend reads the sandboxed PDF, text dump and page renders
    // itself, so the embedded text comes back out of its instruction.
    // The sandbox is a scratch copy, never the user's own directory. See
    // `Workspace`: the agent runs full-auto, so what it can reach is the whole
    // of the security story.
    progress::phase("rendering the datasheet pages");
    let mut ws = prepare_workspace_in(&args.pdf, &std::env::temp_dir())?;
    ws.checker = args.checker.as_deref().and_then(|c| c.canonicalize().ok());
    say!(
        "[model-extract] sandbox {} with {} page render(s)",
        ws.path().display(),
        ws.pages.len()
    );
    let agent_prompt = prompt_for_agent_backend(prompt);
    let clause = verification_clause(&ws, reply.table());
    let base_prompt = format!("{agent_prompt}\n\n{clause}");
    validated_attempts(resolved.retries, &base_prompt, reply, tool, |p| {
        let _ = std::fs::remove_file(ws.answer_path());
        let stdout = run_agent_once(&resolved, p, &ws)?;
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
    retries: usize,
    base_prompt: &str,
    reply: Reply<'_>,
    tool: &str,
    mut fetch: impl FnMut(&str) -> Result<String>,
) -> Result<String> {
    let mut prompt = base_prompt.to_string();
    for attempt in 1..=retries + 1 {
        let started = Instant::now();
        let doing = match (attempt, reply) {
            (1, Reply::Word) => "identifying the part kind",
            (1, _) => "drafting the model",
            _ => "redrafting with the checker's feedback",
        };
        progress::phase(format!("{doing} (attempt {attempt} of {})", retries + 1));
        let raw = reply.complete(fetch(&prompt)?);
        progress::phase("checking the answer");
        say!(
            "[model-extract] {tool} attempt {attempt} returned {} chars in {:.0}s",
            raw.len(),
            started.elapsed().as_secs_f64()
        );
        match reply.check(&raw) {
            Ok(()) => return Ok(raw),
            Err(e) if attempt <= retries => {
                say!("[model-extract] attempt {attempt} failed: {e:#}; retrying with feedback...");
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

/// Measured: "fix exactly those issues" alone read as "produce a number", and a
/// part with no regulated output was given a vout. The honest exits come first.
const HONEST_EXITS: &str = "\n\nA missing required param is NOT a request for a number. If the \
    datasheet states no value for it on this part, do not supply one: record it as an `unknown` \
    source record naming why (rule 3) where the kind allows, else write the identity-only form \
    (`identity_only = true`, `warning`, `unlocked_by`, keeping every sourced field). If the kind \
    does not describe the part at all, use the identity-only form and say in `warning` which kind \
    would; an invented value is the only wrong answer.";

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
    let missing_param = validation_error.contains("missing required param");
    let honest_exits = (table == "[[models]]" && missing_param).then_some(HONEST_EXITS);
    let honest_exits = honest_exits.unwrap_or_default();
    format!(
        "{fragment_correction}YOUR PREVIOUS ANSWER FAILED VALIDATION WITH:\n\
         {validation_error}\n\n\
         Fix exactly those issues and write the corrected WHOLE `{table}` TOML table \
         to model.toml.{honest_exits}"
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
    let reply = call_backend(&identify_prompt(&args.part, pdf_text), args, Reply::Word)?;
    kind_from_reply(&args.part, &reply.to_ascii_lowercase())
}

fn identify_prompt(part: &str, pdf_text: &str) -> String {
    // The front of the datasheet is where the part describes itself. Sending
    // the whole thing to answer one question would cost far more for no gain.
    format!(
        "You are identifying what kind of electronic part a datasheet describes, so a \
         simulator can pick the right model schema.\n\n\
         Part number: {part}\n\n\
         Answer with EXACTLY ONE identifier (before the colon) and nothing else:\n\
         {}\n\
         No kind need model everything: pick the one closest to the part's primary \
         pins and supply. The extraction then writes what that kind holds, else an \
         identity-only card (ratings, supply envelope, pin map) declaring the unmodelled \
         behaviour. Answer exactly `unsupported` only when not even that fits.\n\n\
         Do not explain. Do not add punctuation. One word.\n\n\
         Datasheet (first pages):\n{}",
        supported_kinds()
            .iter()
            .map(|k| format!("{k}: {}\n", kind_definition(k)))
            .collect::<String>(),
        truncate_to_chars(pdf_text, 6000)
    )
}

fn kind_from_reply(part: &str, reply: &str) -> Result<String> {
    // The model may answer with surrounding prose despite the instruction, so
    // look for a supported kind rather than trusting the whole reply.
    let found: Vec<&str> = supported_kinds()
        .into_iter()
        .filter(|k| {
            reply
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .any(|w| w == *k)
        })
        .collect();
    let kinds = supported_kinds().join(", ");
    match found.as_slice() {
        [one] => Ok((*one).to_string()),
        [] if reply.contains("unsupported") => bail!(
            "the datasheet for {part} matched no hauksbee model kind, so no card was written. \
             Next: rerun with --kind <closest of: {kinds}> for a card holding what that kind \
             can (else an identity-only card: ratings, supply envelope, pin map), or scaffold \
             one by hand with `hauksbee models new <REF> --board <board>`."
        ),
        [] => bail!(
            "could not identify what kind of part {part} is from its datasheet. Say so \
             explicitly with --kind <one of: {kinds}>."
        ),
        many => bail!(
            "the datasheet for {part} matched several part kinds ({}), so the choice is not \
             ours to guess. Pick one with --kind.",
            many.join(", ")
        ),
    }
}

/// The one-line meaning the identification prompt gives each kind.
fn kind_definition(kind: &str) -> &'static str {
    match kind {
        "passive" => "resistor, capacitor, inductor, ferrite or crystal",
        "diode" => "rectifier, Schottky, Zener, TVS or LED",
        "bjt_npn" | "bjt_pnp" => "discrete bipolar transistor of that polarity",
        "nmos" | "pmos" => "discrete MOSFET of that channel type",
        "vreg" => "linear or switching regulator, reference, or other part whose job is a rail",
        "opamp" => "amplifier with an analog output (op-amp, instrumentation, audio)",
        "comparator" => "comparator or supervisor with a two-level output",
        "analog_switch" => "analog switch, multiplexer or load switch",
        "digital" => "logic, buffer, level shifter or driver whose outputs follow logic inputs",
        "shift_register" => "shift or storage register (serial/parallel in or out)",
        "dac" | "adc" => "digital-to-analog (dac) or analog-to-digital (adc) converter",
        "mcu" => "programmable microcontroller",
        "connector" => "connector, header or test point",
        "i2c_sensor" | "spi_sensor" => "register-map sensor or peripheral on that bus",
        "charger" => "battery charger",
        "pmic" => "multi-rail power-management IC",
        "balancer" => "battery cell balancer",
        _ => "",
    }
}

/// Every kind the extractor can produce a model for.
///
/// One list, used by the identification prompt, the CLI help and the web
/// picker, so the three cannot drift into disagreeing about what is possible.
/// It is the schema's own vocabulary, less the two kinds with no extraction
/// guidance (`timer`, `ignore`), plus the sensor specs and the behavioural
/// families, which resolve to a base kind (charger and pmic to vreg, balancer
/// to digital) but carry their own parameter guidance.
pub fn supported_kinds() -> Vec<&'static str> {
    let schema = crate::validation::KIND_NAMES.iter().copied();
    let schema = schema.filter(|k| !matches!(*k, "timer" | "ignore"));
    schema
        .chain(["i2c_sensor", "spi_sensor", "charger", "pmic", "balancer"])
        .collect()
}

/// The part of the prompt that tells the model to check its own work.
///
/// A datasheet extraction that is confidently wrong is worse than one that
/// refuses, because a wrong model does not announce itself: it produces a
/// plausible simulation of a part that does not exist. So the instruction is
/// not "be accurate", which asks for nothing, but three specific acts: read
/// each number back against the page it came from, test it against physics,
/// and say out loud which values were never stated.
fn verification_clause(ws: &Workspace, table: &str) -> String {
    let abstain = if table == "[sensor]" {
        "omit that register or field and name it in a `status = \"unknown\"` \
         `[[sensor.source.uncertainty]]` record whose reason starts `ABSTAIN:`"
    } else {
        "use rule 3 (an unknown record, else the identity-only form still giving every \
         sourced field) and a named `ABSTAIN:` in `description` or `[models.source.uncertainty]`"
    };
    let pages = if ws.pages.is_empty() {
        "No page images were rendered. If the text cannot resolve a table's \
         layout, abstain from the affected value explicitly."
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

    let check = match &ws.checker {
        None => "No checker is provided; do not look for one.".to_string(),
        Some(c) => format!(
            "Check model.toml by running exactly `'{}' models lint model.toml` and fix what \
             it reports. Never run any other `hauksbee`.",
            c.display()
        ),
    };
    format!(
        "## Before you answer\n\n         {pages}\n\n         Prefer the supplied authoritative schema and datasheet. Use ONLY the \
         files in your working directory: do not list, search or read anything outside it \
         (home, other projects, temporary directories, installed skills) and do not look \
         anything up online. If evidence is missing or ambiguous, name the missing context \
         and abstain rather than look elsewhere. Never let an external or stale schema \
         override the supplied contract. {check}\n\n         1. VERIFY EACH NUMBER against its source page: units, sign, package/part \
            variant, test conditions, and min/typ/max column.\n         2. SANITY-CHECK WITHOUT OVERRULING THE SOURCE. A surprising but plausible \
            value (including low transistor gain) is not proof of an error. Recheck \
            the row and variant; abstain if the available evidence cannot resolve it.\n         3. DO NOT INVENT ASSUMPTIONS. For a missing required fact, {abstain}; \
            omit optional fields that are not stated. TOML comments may record \
            pin/package ambiguity but must not turn a guess into data.\n\n         Write the final answer to `model.toml` in your working directory as one \
         TOML document and nothing else."
    )
}

/// The defaults live in [`crate::extract_config`]; re-exported so callers
/// that only want to name them need not know where they are kept.
pub use crate::extract_config::{
    DEFAULT_AGY_EFFORT, DEFAULT_AGY_MODEL, DEFAULT_CLAUDE_EFFORT, DEFAULT_CLAUDE_MODEL,
    DEFAULT_CODEX_EFFORT, DEFAULT_CODEX_MODEL,
};

/// The settings one extraction runs with: the config file and environment
/// folded together, then the caller's flags on top.
///
/// A config file that exists but cannot be read is an error here rather than
/// a warning: silently running on defaults would send the datasheet somewhere
/// the user did not choose.
pub fn resolve_settings(args: &Args) -> Result<Resolved> {
    let loaded = extract_config::load()?;
    Ok(resolve_with(&loaded.config, &ProcessHost, args))
}

/// The pure half of [`resolve_settings`]: fold `config`, the `host`'s
/// environment and the flags in `args`, in that precedence order. Tests
/// resolve against a fake host with this.
pub fn resolve_with(
    config: &extract_config::ExtractConfig,
    host: &dyn extract_config::Host,
    args: &Args,
) -> Resolved {
    let mut resolved = config.resolve(host, args.backend);
    resolved.override_model(args.model.as_deref(), args.effort.as_deref());
    if let Some(base) = &args.api_base {
        resolved.api.base_url = base.clone();
    }
    if let Some(name) = &args.api_key_env {
        resolved.api.api_key_env = name.clone();
    }
    if config.retries.is_none() {
        resolved.retries = args.retries;
    }
    resolved
}

/// Refuse up front, with the fix, when an agent backend's CLI is missing.
fn require_tool(resolved: &Resolved) -> Result<()> {
    if resolved.agent(resolved.backend).is_none() {
        return Ok(());
    }
    let status = resolved.status(&ProcessHost, resolved.backend);
    if !status.available {
        bail!(
            "{}: {}; {}. No fallback was attempted.",
            resolved.summary(),
            status.detail,
            status.install
        );
    }
    Ok(())
}

/// Features that reach outside the sandbox or pull in operator state; codex rejects unknown names.
const CODEX_DISABLED_FEATURES: &[&str] = &[
    "apps",
    "browser_use",
    "browser_use_external",
    "computer_use",
    "image_generation",
    "memories",
    "plugins",
    "remote_plugin",
    "skill_search",
    "skill_mcp_dependency_install",
    "hooks",
    "goals",
    "multi_agent",
    "multi_agent_v2",
    "shell_snapshot",
];

/// Package prefixes a confined agent may still read, so `pdftotext`/`pdftoppm`
/// run; they hold installed software, not the user's files.
const CONFINED_READ_ROOTS: &[&str] = &["/opt/homebrew", "/usr/local"];

/// codex `-c` overrides selecting a `hauksbee` permission profile: shell
/// commands read codex's `:minimal` OS/runtime set, [`CONFINED_READ_ROOTS`] and
/// the checker, write the workspace, and read nothing else (home, `/tmp` and
/// `$TMPDIR` included). Checked against codex-cli 0.156.1 with
/// `codex sandbox -P hauksbee`: seatbelt refuses `ls ~`, `~/.codex/skills`,
/// `/tmp` and a `$TMPDIR` sibling while the workspace and checker work. An
/// explicit `--sandbox` flag overrides `default_permissions`, so none is passed.
fn codex_confinement(ws: &Workspace) -> [String; 4] {
    let reads: String = CONFINED_READ_ROOTS
        .iter()
        .map(|root| (*root).to_string())
        .chain(ws.checker.iter().map(|c| c.display().to_string()))
        .map(|path| format!("{}=\"read\",", toml::Value::String(path)))
        .collect();
    let profile = format!(
        "permissions={{hauksbee={{filesystem={{\":minimal\"=\"read\",\":slash_tmp\"=\"none\",\
         \":tmpdir\"=\"none\",{reads}\":workspace_roots\"={{\".\"=\"write\"}}}}}}}}"
    );
    ["-c", "default_permissions=\"hauksbee\"", "-c", &profile].map(String::from)
}

/// `--settings` for Claude Code's Bash sandbox, strict (no unsandboxed retry,
/// no silent start without it): reads denied under home and the temp roots
/// except the workspace and the checker. `--restricted` already confines
/// Read/Edit/Write to the working directory.
fn claude_confinement(ws: &Workspace) -> String {
    let temp_root = ws.path().parent().unwrap_or_else(|| ws.path());
    let allow: Vec<String> = std::iter::once(ws.path())
        .chain(ws.checker.as_deref())
        .map(|p| p.display().to_string())
        .collect();
    serde_json::json!({"sandbox": {
        "enabled": true, "failIfUnavailable": true, "allowUnsandboxedCommands": false,
        "filesystem": {
            "denyRead": ["~/", "/tmp", "/private/tmp", temp_root.display().to_string()],
            "allowRead": allow,
        },
    }})
    .to_string()
}

/// `path` without any directory holding a `hauksbee` executable, so the only
/// checker an agent can reach is the absolute one its prompt names.
fn scrub_path(path: &std::ffi::OsStr) -> std::ffi::OsString {
    let kept = std::env::split_paths(path)
        .filter(|d| !d.join("hauksbee").is_file() && !d.join("hauksbee.exe").is_file());
    std::env::join_paths(kept).unwrap_or_default()
}

/// One sandboxed run of whichever agent CLI the settings name.
///
/// The three CLIs differ only in how they spell the same four things: the
/// model, the reasoning effort, the permission mode that lets them write
/// `model.toml` without a prompt, and where the instruction goes. Each is
/// always named rather than inherited: a CLI's own default varies by plan and
/// config and would silently decide how good the extraction is.
///
///   * codex: in `workspace-write` and `approve-for-me` modes the read-confining
///     [`codex_confinement`] profile (plus `--approve-for-me` for the latter);
///     any other mode is `--sandbox <mode>`, reads unconfined. Network is off
///     unless a mode or approval relaxes it. `skills.include_instructions=false`
///     keeps `$CODEX_HOME/skills` out of the model's context (the `skill_search`
///     feature alone does not), `shell_snapshot` is off so the operator's shell
///     rc files never shape the agent's shell. `--skip-git-repo-check` as the
///     sandbox is no repo, `--cd` to open the PDF, `--output-last-message` keeps
///     narration apart, pages as images. `--ephemeral` keeps no transcript,
///     `--ignore-user-config` drops `config.toml` (MCP servers, trusted projects;
///     sign-in stays) unless a profile is set: that is the operator choosing their
///     codex config, which a profile layers on. `--ignore-rules` drops execpolicy rules.
///   * claude: `-p --permission-mode <mode> --no-session-persistence`: a
///     datasheet run is not a conversation anyone resumes, and a transcript
///     per extraction would keep the datasheet's text around after the
///     sandbox is gone. It reads the PDF and page renders itself. `--restricted`
///     drops the operator's settings, CLAUDE.md, memory, agents, hooks and
///     skills (a wrapper's `--settings` still applies); `--tools` drops
///     subagents and web access; [`claude_confinement`] sandboxes Bash reads;
///     background tasks, which keep `-p` alive, are off. Never
///     `--bare`: it skips the keychain, losing a subscription sign-in.
///   * agy: `--mode <mode> --add-dir <sandbox>`, and the pointer as the
///     `--print=` value because agy reads nothing from stdin. Not read-confined.
///
/// Every CLI gets a PATH from [`scrub_path`] and the workspace as its cwd.
fn run_agent_once(resolved: &Resolved, prompt: &str, ws: &Workspace) -> Result<String> {
    let backend = resolved.backend;
    let Some(a) = resolved.agent(backend) else {
        bail!("the api backend is not an agent CLI");
    };
    let pointer = String::from_utf8_lossy(AGENT_POINTER);
    let tool = &a.command;
    let executable = which_path(tool)
        .with_context(|| format!("executable `{tool}` was not found; no fallback attempted"))?;
    let mut cmd = Command::new(std::path::absolute(executable)?);
    match backend {
        Backend::Codex => {
            cmd.arg("exec");
            if let Some(profile) = &a.profile {
                cmd.args(["-p", profile]);
            }
            let effort = format!("model_reasoning_effort=\"{}\"", a.effort);
            cmd.args(["--model", &a.model, "-c", &effort]);
            match a.permission_mode.as_str() {
                "workspace-write" => cmd.args(codex_confinement(ws)),
                "approve-for-me" => cmd.arg("--approve-for-me").args(codex_confinement(ws)),
                mode => cmd.args(["--sandbox", mode]),
            };
            cmd.args(["--skip-git-repo-check", "--cd"])
                .arg(ws.path())
                .arg("--output-last-message")
                .arg(ws.path().join("last-message.txt"))
                .arg("--ephemeral")
                .args(a.profile.is_none().then_some("--ignore-user-config"))
                .args(["--ignore-rules", "-c", "skills.include_instructions=false"]);
            for feature in CODEX_DISABLED_FEATURES {
                cmd.args(["--disable", feature]);
            }
            for page in &ws.pages {
                cmd.arg("--image").arg(page);
            }
        }
        Backend::ClaudeCode => {
            cmd.args([
                "-p",
                "--output-format",
                "text",
                "--permission-mode",
                &a.permission_mode,
            ])
            .args([
                "--model",
                &a.model,
                "--effort",
                &a.effort,
                "--no-session-persistence",
            ])
            .args(["--restricted", "--strict-mcp-config", "--tools"])
            .args([
                "Bash,Read,Edit,Write",
                "--settings",
                &claude_confinement(ws),
            ])
            .env("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS", "1")
            .env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1");
        }
        Backend::Agy => {
            cmd.args(["--output-format", "text", "--mode", &a.permission_mode])
                .args(["--model", &a.model, "--effort", &a.effort, "--add-dir"])
                .arg(ws.path())
                .arg(format!("--print={pointer}"));
        }
        Backend::Api => unreachable!("handled above"),
    }
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", scrub_path(&path));
    }
    cmd.args(&a.extra_args).current_dir(ws.path());
    run_cli_agent(
        tool,
        &mut cmd,
        ws,
        prompt,
        &ws.path().join(format!("{}-stderr.log", backend.name())),
        Duration::from_secs(resolved.timeout_secs),
    )
}

/// Run the configured backend on a one-line prompt and return what it said.
/// The cheapest end-to-end proof that a setup works: the CLI is found, it is
/// signed in, the model name is accepted, and an answer comes back. Costs one
/// tiny model call; sends nothing but the prompt.
pub fn smoke_test(resolved: &Resolved, progress: &mut dyn FnMut(&str)) -> Result<String> {
    const PROMPT: &str =
        "This is a connectivity check. Reply with exactly the word OK and nothing else.";
    if std::env::var_os("HAUKSBEE_EXTRACT_MOCK_REPLY").is_some() {
        progress("mock reply hook set; nothing sent");
        return Ok("OK (mock)".to_string());
    }
    progress(&format!(
        "checking {} ({})",
        resolved.summary(),
        resolved.backend_source.describe()
    ));
    if resolved.backend == Backend::Api {
        let request = ApiRequest::prepare(&resolved.api)?;
        progress(&format!("POST {}", request.url));
        let reply = request.send(PROMPT)?;
        return Ok(reply.trim().to_string());
    }
    require_tool(resolved)?;
    let ws = Workspace::scratch()?;
    progress(&format!(
        "running `{}` in an empty sandbox ({}), model {}",
        resolved.agent(resolved.backend).expect("agent").command,
        ws.path().display(),
        resolved.model()
    ));
    let prompt = format!("{PROMPT} Write that word to model.toml as well.");
    run_agent_once(resolved, &prompt, &ws)?;
    let text = std::fs::read_to_string(ws.answer_path()).with_context(|| {
        format!(
            "{} answered but failed the sandbox read/write check: model.toml was not created",
            resolved.backend.label()
        )
    })?;
    let reply = text.trim();
    if reply != "OK" {
        bail!(
            "{} failed the sandbox read/write check: model.toml must contain exactly OK",
            resolved.backend.label()
        );
    }
    Ok(reply.to_string())
}

/// Run one sandboxed agent invocation to completion and return its stdout.
///
/// The contract every agent CLI shares: the sandbox is the working directory,
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
    timeout: Duration,
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
    // agy takes its prompt as `--print=<text>` and reads nothing from stdin;
    // the pointer is short and secret-free, so argv is fine there. Its pipe
    // is still written and closed so a CLI that does read stdin sees EOF.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(AGENT_POINTER);
    }
    wait_for_cli_backend(child, tool, log, timeout)
}

/// The stdin instruction every agent run receives. The pages are on disk
/// beside the PDF (codex also gets them attached with `--image`), so one
/// pointer serves every CLI (agy gets it as the `--print` value instead).
const AGENT_POINTER: &[u8] = b"Read prompt.md in your working directory and follow it exactly. \
    The supplied files and output location are in that working directory; read nothing outside \
    it, use the file tools available to you, and report any denied required read or write instead of claiming success. \
    Read values off the rendered page images (page-*.png) for anything that lives in a table \
    or pinout, with datasheet.pdf available in the same directory. Write your answer to model.toml.";

/// Poll a spawned CLI agent to completion, killing it at `CLI_BACKEND_TIMEOUT`,
/// and return its stdout. On a non-zero exit the error quotes the last few
/// non-empty lines of BOTH stdout and the captured stderr log: a CLI agent puts
/// the reason it gave up (a refused model, a rate limit, a bad config key) on
/// stderr, so a failure with an empty stdout would otherwise read as "exited
/// with status 1: " and nothing after the colon.
fn wait_for_cli_backend(
    mut child: Child,
    tool: &str,
    log_path: &Path,
    timeout: Duration,
) -> Result<String> {
    let deadline = Instant::now() + timeout;
    while child
        .try_wait()
        .with_context(|| format!("polling {tool}"))?
        .is_none()
    {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{tool} timed out after {}s with no answer; retry with a tighter prompt, raise \
                 timeout_secs, or pick another backend (hauksbee models backend setup)",
                timeout.as_secs()
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
    /// Empty for a loopback endpoint (Ollama, LM Studio), which wants no
    /// Authorization header and leaks nothing off the machine.
    api_key: String,
}

impl ApiRequest {
    fn prepare(settings: &ResolvedApi) -> Result<Self> {
        let key_env = &settings.api_key_env;
        let api_key = match std::env::var(key_env).ok().filter(|k| !k.trim().is_empty()) {
            Some(key) => key,
            None if extract_config::is_local_url(&settings.base_url) => String::new(),
            None => bail!(
                "the api backend reads its key from ${key_env}, which is unset or \
                 empty. Fix: set {key_env} (export {key_env}=<your key>), or name \
                 the variable that holds your key with --api-key-env NAME (or \
                 api.api_key_env in the config). The key is never accepted as a \
                 flag value and never stored."
            ),
        };
        if which_path("curl").is_none() {
            bail!("the api backend needs `curl`, which is not in PATH; install curl");
        }
        Ok(ApiRequest {
            url: format!(
                "{}/chat/completions",
                settings.base_url.trim_end_matches('/')
            ),
            model: settings.model.clone(),
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
        let auth = if self.api_key.is_empty() {
            String::new()
        } else {
            format!("header = \"Authorization: Bearer {}\"\n", self.api_key)
        };
        let curl_config = format!(
            "url = \"{}\"\nrequest = \"POST\"\nheader = \"Content-Type: application/json\"\n\
             {auth}data = \"@{}\"\nsilent\nshow-error\n",
            self.url,
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
    let mut entries = db.models.into_iter();
    let first = entries
        .next()
        .with_context(|| format!("no [[models]] entry in reply for {part}"))?;
    // The whole reply is written to disk, so every entry is checked.
    for entry in std::iter::once(&first).chain(entries.as_slice()) {
        check_reply_entry(entry, part, kind_str)
            .map_err(|e| anyhow::anyhow!("[[models]] entry '{}': {e:#}", entry.id))?;
    }
    let all: Vec<ModelEntry> = std::iter::once(first.clone()).chain(entries).collect();
    if let Some(problem) = crate::variants::lint(&all).into_iter().next() {
        bail!("variant set in the reply for {part}: {problem}");
    }
    Ok(first)
}

fn check_reply_entry(entry: &ModelEntry, part: &str, kind_str: &str) -> Result<()> {
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
    let behaves = !entry.behavioral.is_empty() || entry.current_program.is_some();
    if behavioral_family_base_kind(want).is_some() && !behaves {
        bail!(
            "behavioural extraction for {part} (kind '{want}') produced no [models.behavioral] \
             or [models.current_program] block; the prompt asked for one"
        );
    }
    if !entry
        .source
        .as_ref()
        .is_some_and(crate::schema::ModelSourceSpec::declares_model_error)
    {
        bail!("reply for {part} has no model-error record (rule 0)");
    }
    supply_relative_rows(entry.source.as_ref())?;
    if let Some([is, n, rs]) = crate::param_names::diode_refit(entry) {
        bail!("diode is/n/rs are absent or over 10% off the cited VF rows, which fit is = {is:.4e}, n = {n:.4}, rs = {rs:.4}");
    }
    crate::validation::validate(entry).map_err(|errs| {
        let msg: Vec<String> = errs.iter().map(ToString::to_string).collect();
        anyhow::anyhow!("validation failed: {}", msg.join("; "))
    })
}

fn supply_relative_rows(source: Option<&crate::schema::ModelSourceSpec>) -> Result<()> {
    let mut rows = source.into_iter().flat_map(|s| &s.characteristic);
    let problem = rows.find_map(crate::schema::Characteristic::supply_relative_problem);
    problem.map_or(Ok(()), |e| Err(anyhow::anyhow!(e)))
}

/// The snake_case name a kind serialises as (the serde `rename_all`).
fn kind_discriminant(kind: ComponentKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// A part name as a file stem: anything but alphanumerics, `-` and `_` becomes `_`.
pub fn sanitise_filename(s: &str) -> String {
    s.replace(|c: char| !(c.is_alphanumeric() || "-_".contains(c)), "_")
}

fn default_out_dir() -> PathBuf {
    dirs_next().join(".hauksbee").join("models")
}

/// The user's home, by whichever name this OS gives it. An empty value is not
/// a home; falling through to the temp dir keeps a model written to a
/// well-defined place instead of `/.hauksbee`.
fn dirs_next() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    static MOCK_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
            ("agy", Backend::Agy),
            ("api", Backend::Api),
        ] {
            assert_eq!(parse(&["--backend", flag]).unwrap().backend, Some(want));
        }
        assert!(parse(&["--backend", "gemini"]).is_err());
        assert_eq!(
            parse(&["--effort", "max"]).unwrap().effort.as_deref(),
            Some("max")
        );
        assert!(parse(&["--effort"]).is_err());

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
        let r = resolve_with(
            &extract_config::ExtractConfig::default(),
            &extract_config::FakeHost::default(),
            &a,
        );
        assert_eq!(r.backend, Backend::Api);
        assert_eq!(r.api.api_key_env, "MY_LLM_KEY");
        assert_eq!(r.api.base_url, "https://llm.example/v1");
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
    fn pdf_pages_keep_interior_blanks_but_not_the_trailing_sentinel() {
        let pages = split_pdf_pages_text("first\u{000c}\u{000c}third\u{000c}");
        assert_eq!(
            pages.iter().map(|page| page.number).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(
            pages
                .iter()
                .map(|page| page.text.as_str())
                .collect::<Vec<_>>(),
            ["first", "", "third"]
        );
        assert_eq!(
            pages
                .iter()
                .map(|page| page.text.as_str())
                .collect::<Vec<_>>()
                .join("\u{000c}"),
            "first\u{000c}\u{000c}third"
        );
    }

    #[test]
    fn page_selection_includes_function_tables_within_the_render_budget() {
        let english = "Features|Recommended Operating Conditions|Absolute Maximum Ratings|\
            Switching Characteristics|Electrical Characteristics|Pin Functions|\
            Functional Description|Application Information";
        // Chinese headings prefix or number their topic word; prose naming a pin is no heading.
        let chinese = "产品概述|推荐工作条件|6 极限工作参数|时序特性|电特性 at Ta = 25°C|\
            5、管脚配置|模块功能描述|4 典型应用电路|管脚上的电压是被忽略的，经过一个固定时间后";
        let (all, budget) = ([1, 2, 3, 4, 5, 6, 7, 8], MAX_RENDERED_PAGES);
        for (headings, limit, want) in [(english, budget, &all[..7]), (chinese, 20, &all)] {
            let pages = headings
                .split('|')
                .enumerate()
                .map(|(index, text)| PdfPageText {
                    number: index + 1,
                    text: text.into(),
                });
            let selected = select_relevant_pages(&pages.collect::<Vec<_>>(), limit);
            let numbers = selected.iter().map(|page| page.number).collect::<Vec<_>>();
            assert_eq!(numbers, want, "{headings}");
        }
    }

    #[test]
    fn prompt_forbids_bound_placeholders_saturation_is_and_schottky_tt() {
        let bjt = build_prompt("NPN_FIXTURE", "bjt_npn", "x");
        assert!(bjt.contains("never a placeholder at a bound"));
        assert!(bjt.contains("never from VBE(sat)"));
        assert!(bjt.contains("takes no tt: its trr is capacitive recovery"));
        assert!(bjt.contains("is, bf, nf (no VBE(on) row or curve: an `unknown` record each)"));
        let switch = build_prompt("SWITCH_FIXTURE", "analog_switch", "x");
        assert!(switch.contains("off-state figure: an `unknown` record for roff"));
    }

    #[test]
    fn prompt_carries_kind_specific_guidance() {
        let bjt = build_prompt("BC847", "bjt_npn", "x");
        assert!(bjt.contains("BC847") && bjt.contains("is, bf, nf"));
        assert!(bjt.contains("datasheet states): br, vaf, var, nr, rb"));
        assert!(bjt.contains("VCEO") && bjt.contains("Ratings are magnitudes"));
        assert!(bjt.contains("picks the column each param") && bjt.contains("never a nominal"));
        let vreg = build_prompt("ADJ_FIXTURE", "vreg", "x");
        assert!(vreg.contains("adjustable (ADJ/FB pin): vref"));
        assert!(vreg.contains("a range spanning the family's selectable outputs is never vout"));
        assert!(vreg.contains("max_current_a (a [models.ratings] key, never a param)"));
        assert!(vreg.contains("abstaining on params never makes a resolved pin table unknown"));
        assert!(!bjt.contains("[models.behavioral.converter]"));
        assert!(build_prompt("1N4148", "diode", "x").contains("VRRM"));
        assert!(build_prompt("AMS1117", "vreg", "x").contains("max_junction_temp_c"));
        assert!(bjt.contains("   - bf: 1 to 2000\n"));
        assert!(
            bjt.contains("(.MODEL/.SUBCKT) is copied verbatim") && bjt.contains("derives tf, cjc")
        );
        assert!(bjt.contains("rated IOUT") && bjt.contains("least favourable row the part"));
        assert!(bjt.contains("max_current_a, max_power_w and theta_ja_c_per_w all from that same"));
        assert!(vreg.contains("at the rated output current when several loads are printed"));
        let nmos = build_prompt("FET_FIXTURE", "nmos", "x");
        assert!(nmos.contains("   - kp: 1e-6 to 1000\n") && nmos.contains("cgd, is, cbd"));
        assert!(nmos.contains("1/(kp*(|VGS| - |vto|)) + rd + rs"));
        let comparator = build_prompt("CMP_FIXTURE", "comparator", "x");
        assert!(comparator.contains("hysteresis = 0.0") && comparator.contains("leak_s: 0 to 1"));
        assert!(comparator.contains("(VT+ minus VT-), never the input offset"));
        assert!(comparator.contains("VICR/CMVR row, emit one input_common_mode per channel"));
        assert!(comparator.contains("from the 25 °C row when a full-temperature-range row"));
        assert!(bjt.contains("Identity-only is a last resort. A typ-only or max-only row"));
        assert!(bjt.contains("pad names A/K, G/D/S or\n   B/C/E") && bjt.contains("one missing"));
        assert!(comparator.contains("each unit's signal roles _a to _d (out_a, in_minus_a"));
        assert!(build_prompt("AMP_FIXTURE", "opamp", "x").contains("never the supply or ground"));
        assert!(comparator.contains("leak_s = output-off conductance (IOH / VOH) (S)"));
        assert!(
            build_prompt("D", "diode", "x").contains("cjo = zero-bias junction capacitance (F)")
        );
        assert!(build_prompt("REG_FIXTURE", "vreg", "x").contains("   - vout: |x| 0.5 to 30\n"));
        assert!(build_prompt("CONN_FIXTURE", "connector", "x").contains("(none for this kind)"));

        let charger = build_prompt("LTC4020", "charger", "x");
        assert!(charger.contains("[models.behavioral.converter]"));
        assert!(charger.contains("iin_program") && charger.contains("because mA·kΩ = V"));
        assert!(charger.contains("pins = { \"1\" = \"<printed name>\""));
        assert!(charger.contains("kind = \"vreg\"") && !charger.contains("kind = \"charger\""));
        assert!(charger.contains("Component kind: vreg (the charger family"));
        assert!(
            !charger.contains("for kind=\"charger\"") && charger.contains("Never merge variants")
        );
        assert!(build_prompt("nPM1300", "pmic", "x").contains("pull_to"));
        assert!(build_prompt("LTC6803", "balancer", "x").contains("[[models.behavioral.laws]]"));
    }

    #[test]
    fn digital_prompt_supplies_complete_bounded_scaffolding() {
        for kind in ["digital", "shift_register"] {
            let prompt = build_prompt("LOGIC_FIXTURE", kind, "fixture text");
            let needles = "vcc or vdd; gnd or vss | [models.logic] | inputs` and `outputs` arrays \
                | [models.logic.comb] | [[models.logic.register]] | one UNIQUE namespace \
                | [models.logic.tristate] | Count ops must omit `data_in` | explicit NCs \
                | supply_pin` and `gnd_pin` are STRINGS | physical\npad identifiers exactly as keyed \
                | currently metadata; the executable\nbinder does not read them | max_pin_current_a \
                | whole-device or supply/ground current limit | hold their previous state \
                | `tpd_s` is metadata today | static switching thresholds; do not substitute dynamic \
                | requested scenario needs a guaranteed transition bound | VT+ maximum | VT- minimum";
            for needle in needles.split(" | ") {
                assert!(prompt.contains(needle), "{kind}: {needle}");
            }
            assert!(!prompt.contains("can be given\n  any descriptive name, or left out"));
        }

        let hint = digital_hint_for_kind("digital");
        let (_, rest) = hint.split_once("```toml\n").unwrap();
        let (example, _) = rest.split_once("```").unwrap();
        let parsed: toml::Value = toml::from_str(example).unwrap();
        let logic: crate::logic_spec::Logic = parsed["models"]["logic"].clone().try_into().unwrap();
        logic.validate().expect("the logic example validates");
        let ws = Workspace::scratch().unwrap();
        let sensor = verification_clause(&ws, "[sensor]");
        assert!(sensor.contains("[[sensor.source.uncertainty]]"));
        assert!(!sensor.contains("models.source") && !sensor.contains("identity-only"));
        let footer = verification_clause(&ws, "[[models]]");
        let needles = "Prefer the supplied authoritative schema and datasheet \
            | Use ONLY the files in your working directory | No checker is provided \
            | Never let an external or stale schema override the supplied contract \
            | abstain if the available evidence cannot resolve it";
        assert!(needles.split(" | ").all(|n| footer.contains(n)), "{footer}");
        assert!(!footer.contains("vendor source") && !footer.contains("diagnostic tools"));
        let mut ws = ws;
        ws.checker = Some("/opt/x/hauksbee".into());
        let footer = verification_clause(&ws, "[[models]]");
        assert!(footer.contains("`'/opt/x/hauksbee' models lint model.toml`"));
        assert!(footer.contains("Never run any other `hauksbee`"));
        assert!(!footer.contains("bipolar transistor with a gain of 5"));
        assert!(!footer.contains("`notes`"));
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
        for reply in ["digital", "model = \"digital\"", "answer = \"digital\""] {
            assert!(reply_is_kind_or_fragment(reply, "[[models]]"), "{reply}");
        }
        assert!(!reply_is_kind_or_fragment(
            FORMAT_EXAMPLES[0].card,
            "[[models]]"
        ));
        assert!(retry_feedback("digital", "no table", "[[models]]")
            .contains("--- BEGIN REPLY ---\ndigital\n"));
        // A missing required param offers the honest exits, never "produce a number".
        let card = FORMAT_EXAMPLES[0].card;
        let missing = retry_feedback(card, "missing required param 'vout'", "[[models]]");
        assert!(missing.contains("NOT a request") && missing.contains("identity_only"));
        assert!(!retry_feedback(card, "no table", "[[models]]").contains("identity_only"));
    }

    #[test]
    fn kind_identification_defines_kinds_and_names_next_steps() {
        let prompt = identify_prompt("P", "text");
        for kind in supported_kinds() {
            assert!(!kind_definition(kind).is_empty(), "{kind}");
            assert!(prompt.contains(&format!("{kind}: {}", kind_definition(kind))));
        }
        assert!(prompt.contains("identity-only card"));
        assert_eq!(kind_from_reply("P", "digital").unwrap(), "digital");
        let none = kind_from_reply("P", "unsupported").unwrap_err().to_string();
        assert!(
            none.contains("--kind") && none.contains("models new"),
            "{none}"
        );
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
            &(extract_toml_block(reply, "[[models]]") + "\n" + MODEL_SOURCE),
            "LTC4020",
            "charger",
        )
        .unwrap();
        assert_eq!(entry.kind, crate::ComponentKind::Vreg);
        assert!(entry.behavioral.converter.is_some());

        let plain = "[[models]]\nid = \"x\"\nkind = \"vreg\"\n[models.params]\nvout = 5.0\ndropout_v = 1.0\niq_a = 0.001\n";
        assert!(parse_and_validate_reply(plain, "X", "charger").is_err());

        // The linear-charger snippet the prompt teaches, placeholders filled,
        // is accepted as the family's behaviour without a converter block.
        let hint = behavioral_hint_for_kind("charger");
        let snippet = hint[hint.find("[models.current_program]").unwrap()..]
            .replace("<programming pin role>", "prog")
            .replace("<highest specified programmed current, A>", "0.5")
            .replace("<typical K in volts>", "1000.0");
        let pins = "[models.pins]\n\"1\" = \"in\"\n\"2\" = \"out\"\n\"3\" = \"prog\"\n";
        let linear = format!("{plain}{pins}{snippet}{MODEL_SOURCE}");
        let entry = parse_and_validate_reply(&linear, "X", "charger").unwrap();
        assert!(entry.behavioral.is_empty() && entry.current_program.is_some());
        // Absent dropout/Iq rows abstain by record, never identity-only; lint agrees.
        let unknown =
            |k| format!("{{ status = \"unknown\", parameter = \"{k}\", reason = \"-\" }}, ");
        let records = format!("uncertainty = [{}{}", unknown("dropout_v"), unknown("iq_a"));
        let cut = linear.replace("dropout_v = 1.0\niq_a = 0.001\n", "");
        let abstained = cut.replace("uncertainty = [", &records);
        let entry = parse_and_validate_reply(&abstained, "X", "charger").unwrap();
        let root = toml::from_str(&abstained).unwrap();
        assert!(crate::validation::source_shape_findings(&root).is_empty());
        assert!(crate::schema::model_error_warning(entry.source.as_ref()).is_none());
        let named = abstained.replace("\"dropout_v\"", "\"x\"");
        assert!(parse_and_validate_reply(&named, "X", "charger").is_err());
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
        let reply = reply.to_owned() + MODEL_SOURCE;
        let entry = parse_and_validate_reply(&reply, "ROC_SUPPLY", "digital").unwrap();
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
        let _guard = MOCK_ENV.lock().unwrap();
        let example = FORMAT_EXAMPLES[0];
        let dir = tempfile::tempdir().unwrap();
        let reply_path = dir.path().join("reply.txt");
        std::fs::write(
            &reply_path,
            format!("Here is the model:\n```toml\n{}\n```", example.card),
        )
        .unwrap();
        std::env::set_var("HAUKSBEE_EXTRACT_MOCK_REPLY", &reply_path);

        let pdf = dir.path().join("nonexistent.pdf");
        let (raw, entry) = draft(pdf, "irrelevant", example.part, example.kind);
        std::env::remove_var("HAUKSBEE_EXTRACT_MOCK_REPLY");

        let expected = parse_and_validate_reply(example.card, example.part, example.kind).unwrap();
        assert_eq!(entry.kind, expected.kind);
        assert_eq!(entry.ratings.max_voltage_v, expected.ratings.max_voltage_v);
        assert!(raw.starts_with("[[models]]"), "fence should be stripped");
    }

    /// The prompt -> backend -> validated-entry chain `run` drives, minus the
    /// output file.
    fn draft(src: PathBuf, text: &str, part: &str, kind: &str) -> (String, ModelEntry) {
        let prompt = build_prompt(part, kind, text);
        let args = Args::new(src, part.into(), kind.into());
        let raw = call_backend(&prompt, &args, Reply::Model { part, kind }).expect("backend call");
        let entry = parse_and_validate_reply(&raw, part, kind).unwrap();
        (raw, entry)
    }

    fn live_backend_available() -> bool {
        let loaded = extract_config::load().expect("valid extraction config");
        let resolved = loaded.config.resolve(&ProcessHost, None);
        resolved.status(&ProcessHost, resolved.backend).available
    }

    /// Live: the real backend against the BC846-series datasheet in testdata.
    /// Run with
    /// `cargo test -p hauksbee-models -- extract_bc846_live --ignored --nocapture`.
    ///
    /// The part asked for is the part the sheet covers. Asking it for a BC847
    /// is the shape of request that produces a wrong number rather than no
    /// number: the BC846 series sheet states 65 V VCEO for its own types and
    /// says nothing at all about the 45 V BC847, so a model that answers
    /// anyway has substituted a rating from a different device.
    #[test]
    #[ignore]
    fn extract_bc846_live() {
        let pdf = testdata("datasheets/BC846.pdf");
        assert!(pdf.exists(), "BC846 datasheet not found at {pdf:?}");
        if !live_backend_available() {
            return;
        }
        let text = extract_pdf_text(&pdf).expect("PDF text extraction");
        let (_, entry) = draft(pdf, &text, "BC846", "bjt_npn");
        let bf = entry.params.get_f64("bf").expect("bf present");
        assert!(
            (100.0..=460.0).contains(&bf),
            "bf {bf} outside the BC846 hFE band"
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
        let (_, entry) = draft(src, &text, "LTC4020", "charger");
        assert_eq!(entry.kind, crate::ComponentKind::Vreg);
        let c = entry.behavioral.converter.expect("converter block");
        assert_eq!(c.in_pin, "pvin");
        assert_eq!(c.out_pin, "bat");
        assert!(c.iin_program.is_some());
    }

    const MODEL_SOURCE: &str = "[models.source]\ntier = \"datasheet-derived\"\n\
        uncertainty = [{ status = \"unknown\", parameter = \"model\", reason = \"none\" }]\n";

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
[models.source]
tier = "datasheet-derived"
references = [{ url = "https://vendor.example/bcm847bs.pdf", title = "BCM847BS", locator = "p.3" }]
uncertainty = [
  { status = "unknown", parameter = "model", reason = "none published" },
  { status = "interval", parameter = "bf", low = 100.0, high = 460.0, unit = "", kind = "specification-limits", basis = "Table 7" },
]
"#;
        let entry = parse_and_validate_reply(good, "BCM847BS", "bjt_npn").unwrap();
        assert_eq!(entry.id, "bcm847bs");
        // A variant reply is checked as a set: a code two members match is refused.
        let member = |id: &str, re: &str, code: &str| {
            let head = format!("\"{id}\"\nvariant_of = \"BCM847BS\"\nvariant = {{ orderable_re = \"{re}\", orderable = [\"BCM847BS,{code}\"], basis = \"p. 2\" }}");
            good.replace("\"bcm847bs\"", &head)
        };
        let pair = |y_re| member("x", "115$", "115") + &member("y", y_re, "135");
        assert!(parse_and_validate_reply(&pair("135$"), "BCM847BS", "bjt_npn").is_ok());
        let error = parse_and_validate_reply(&pair("1.5$"), "BCM847BS", "bjt_npn").unwrap_err();
        assert!(error.to_string().contains("variant set"), "{error:#}");
        let no_source = &good[..good.find("[models.source]").unwrap()];
        let error = parse_and_validate_reply(no_source, "BCM847BS", "bjt_npn").unwrap_err();
        assert!(error.to_string().contains("rule 0"), "{error:#}");
        // Completion adds the record to every entry lacking one, keeps the rest.
        let (part, kind) = ("X", "bjt_npn");
        let model = Reply::Model { part, kind };
        let second = no_source.replace("\"bcm847bs\"", "\"b\"");
        let filled = model.complete(format!("{no_source}{second}"));
        assert!(parse_and_validate_reply(&filled, part, kind).is_ok());
        assert_eq!(filled.matches("\"model\"").count(), 2);
        assert_eq!(model.complete(good.to_string()), good);
        // A cited fT row fills the omitted tf, with its working as the comment.
        let ft = "[[models.source.characteristic]]\nparameter = \"fT\"\ntyp = 250\nunit = \"MHz\"\nconditions = \"c\"\nbasis = \"b\"\n";
        let derived = model.complete(format!("{good}{ft}"));
        let working = "tf = 6.3662e-10  # derived: tf = 1/(2π·fT), 'fT' typ 250 MHz (c)";
        assert!(
            derived.contains(working) && parse_and_validate_reply(&derived, part, kind).is_ok()
        );
        for (from, to, needle) in [
            ("\"model\"", "\"bf\"", "rule 0"),
            ("low = 100.0", "low = 999.0", "uncertainty 1 (bf)"),
            ("none published", " ", "uncertainty 0 (model)"),
            ("specification-limits", "vibes", "deserialize"),
            ("https://", "http://", "must use https://"),
        ] {
            let reply = good.replace(from, to);
            let error = parse_and_validate_reply(&reply, "BCM847BS", "bjt_npn").unwrap_err();
            assert!(
                format!("{error:#}").contains(needle),
                "{from} -> {to}: {error:#}"
            );
        }

        let bad_bf = good.replace("bf  = 150.0", "bf  = 99999.0");
        assert!(parse_and_validate_reply(&bad_bf, "BCM847BS", "bjt_npn").is_err());
        // Every entry is checked, not just the first; a row param is checked too.
        let second = |bf: &str| {
            let b = good.replace("\"bcm847bs\"", "\"bcm847bs_b\"");
            format!("{good}{}", b.replace("bf  = 150.0", bf))
        };
        let row = "bf = { min = 100, typ = 150, max = 460, unit = \"\", conditions = \"c\", basis = \"b\" }";
        let entry = parse_and_validate_reply(&second(row), "BCM847BS", "bjt_npn").unwrap();
        assert_eq!(entry.id, "bcm847bs");
        for bf in ["bf  = 99999.0", &row.replace("typ = 150", "typ = 99999")] {
            let error = parse_and_validate_reply(&second(bf), "BCM847BS", "bjt_npn").unwrap_err();
            assert!(
                format!("{error:#}").contains("entry 'bcm847bs_b'"),
                "{error:#}"
            );
        }
        assert!(parse_and_validate_reply("   \n  ", "BC847", "bjt_npn").is_err());
        assert!(
            parse_and_validate_reply("I couldn't find the parameters.", "BC847", "bjt_npn")
                .is_err()
        );
        // A diode law off its cited VF rows is refit by completion, then validates.
        let vf = |ma: f64| {
            let v = 1.2 * hauksbee_ir::thermal_voltage_c(27.0) * (ma * 1e5).ln_1p() + ma * 5e-4;
            format!("[[models.source.characteristic]]\nparameter = \"VF\"\ntyp = {v}\nunit = \"V\"\nconditions = \"IF = {ma} mA\"\nbasis = \"b\"\n")
        };
        let rows = [1.0, 10.0, 100.0, 1000.0].map(vf).concat();
        let off = format!("[[models]]\nid = \"d\"\nkind = \"diode\"\n[models.match]\nvalue_re = \"D\"\n[models.params]\nis = 1e-14\nn = 1.0\nrs = 0.0\n{MODEL_SOURCE}{rows}");
        let error = parse_and_validate_reply(&off, "D", "diode").unwrap_err();
        assert!(format!("{error:#}").contains("over 10% off"), "{error:#}");
        let fixed = Reply::Model {
            part: "D",
            kind: "diode",
        }
        .complete(off);
        assert!(fixed.contains("  # fitted: is/n/rs to the cited VF rows (4 points), the card's"));
        let n = parse_and_validate_reply(&fixed, "D", "diode")
            .unwrap()
            .params
            .get_f64("n");
        assert!(n.is_some_and(|n| (n - 1.2).abs() < 1e-3), "{fixed}");
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
[sensor.pins]
1 = "sda"
2 = "scl"
3 = "a0"
[[sensor.address_select]]
address = 0x48
straps = { a0 = "low" }
[[sensor.address_select]]
address = 0x49
straps = { a0 = "high" }
[sensor.protocol]
style = "i2c_pointer"
[sensor.source]
tier = "datasheet-derived"
uncertainty = [{ status = "unknown", parameter = "model", reason = "none published" }]
"#;
        let spec = validate_sensor_reply(i2c, "LM75", "i2c_sensor").unwrap();
        let json = serde_json::json!(spec.sensor);
        let prompt = build_sensor_prompt("X", "spi_sensor", "");
        let protocol = json["protocol"].as_object().unwrap().keys();
        for key in json.as_object().unwrap().keys().chain(protocol) {
            assert!(
                prompt.contains(key.as_str()),
                "the prompt never names `{key}`"
            );
        }
        let no_source = &i2c[..i2c.find("[sensor.source]").unwrap()];
        let error = validate_sensor_reply(no_source, "LM75", "i2c_sensor").unwrap_err();
        assert!(error.to_string().contains("rule 5"), "{error:#}");
        // Completion adds it (rule 5), also beside a rail-relative row, once.
        let (part, kind) = ("X", "i2c_sensor");
        let sensor = Reply::Sensor { part, kind };
        let row = "[sensor.source]\ntier = \"datasheet-derived\"\n[[sensor.source.characteristic]]\nparameter = \"VIL\"\nmin = -0.3\nunit = \"V\"\nconditions = \"max 0.3 VDDIO\"\nbasis = \"T1, min\"\n";
        for draft in ["", row] {
            let filled = sensor.complete(format!("{no_source}{draft}"));
            assert!(validate_sensor_reply(&filled, part, kind).is_ok());
            assert_eq!(filled.matches("\"model\"").count(), 1);
        }
        let wrong = i2c.replace("\"model\"", "\"temperature_c\"");
        assert!(validate_sensor_reply(&wrong, "LM75", "i2c_sensor").is_err());
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
[sensor.source]
tier = "datasheet-derived"
uncertainty = [{ status = "unknown", parameter = "model", reason = "none published" }]
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
        assert!(!crate::validation::KIND_NAMES.contains(&"charger"));
        assert_eq!(component_kind("charger"), Some(ComponentKind::Vreg));
        for kind in supported_kinds() {
            let sensor = is_sensor_kind(kind);
            let prompt = if sensor {
                build_sensor_prompt("SOURCE_FIXTURE", kind, "fixture text")
            } else {
                build_prompt("SOURCE_FIXTURE", kind, "fixture text")
            };
            assert_eq!(
                prompt.matches(SOURCE_QUANTITY_GUIDANCE).count(),
                1,
                "{kind}"
            );
            assert!(!prompt.contains("{source_quantity_guidance}"));
            if sensor {
                assert!(!prompt.contains("models.source"), "{kind}");
                assert!(prompt.contains("not the only record allowed"));
                assert!(prompt.contains("[[sensor.source.characteristic]]"));
                assert!(
                    prompt.contains("[sensor.source]") && prompt.contains("parameter = \"model\"")
                );
                continue;
            }
            assert!(prompt.contains(PIN_ROLE_GUIDANCE), "{kind}");
            assert!(prompt.contains("I/O direction alone is not a function"));
            assert!(prompt.contains("Validated model-error bounds"));
            assert!(prompt.contains("kind = \"empirical-error\""));
            assert!(prompt.contains("parameter = \"model\""));
            assert!(!prompt.contains("If no defensible interval is published"));
            assert!(component_kind(kind).is_some(), "{kind} has no schema kind");
            assert_ne!(
                required_params_for_kind(kind),
                generic,
                "{kind} has only the generic hint"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn custom_command_runs_the_selected_protocol_and_never_falls_back() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = MOCK_ENV.lock().unwrap();
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let command = dir.path().join("custom wrapper");
        std::fs::write(
            &command,
            "#!/bin/sh\ncat >/dev/null\nif grep -q 'connectivity check' prompt.md; then printf 'OK\\n' > model.toml; fi\nprintf '%s\\n' \"$@\"\nprintf 'wrapper stderr' >&2\nprintf '%s' \"$CLAUDE_CODE_DISABLE_BACKGROUND_TASKS$CLAUDE_CODE_DISABLE_AUTO_MEMORY\" > env.txt\nprintf '%s' \"$PATH\" > path.txt\npwd -P > cwd.txt\n",
        )
        .unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut cfg = extract_config::ExtractConfig::default();
        cfg.set("claude-code.command", command.to_str().unwrap())
            .unwrap();
        cfg.set("claude-code.model", "gpt-5.6-sol").unwrap();
        cfg.set("claude-code.effort", "high").unwrap();
        let mut resolved = cfg.resolve(
            &extract_config::FakeHost::default(),
            Some(Backend::ClaudeCode),
        );
        assert_eq!(which_path(command.to_str().unwrap()), Some(command.clone()));
        require_tool(&resolved).unwrap();
        let mut ws = Workspace::scratch().unwrap();
        ws.checker = Some(dir.path().join("hauksbee"));
        let settings = claude_confinement(&ws);
        let sandbox: serde_json::Value = serde_json::from_str(&settings).unwrap();
        let fs = &sandbox["sandbox"]["filesystem"];
        assert_eq!(sandbox["sandbox"]["allowUnsandboxedCommands"], false);
        assert!(fs["denyRead"].as_array().unwrap().contains(&"~/".into()));
        let allowed = [ws.path(), ws.checker.as_deref().unwrap()].map(|p| p.display().to_string());
        assert_eq!(fs["allowRead"], serde_json::json!(allowed));
        for mode in ["acceptEdits", "auto", "dontAsk", "plan"] {
            resolved.claude_code.permission_mode = mode.to_string();
            let output = run_agent_once(&resolved, "fake prompt", &ws).unwrap();
            assert_eq!(
                output.lines().collect::<Vec<_>>(),
                [
                    "-p",
                    "--output-format",
                    "text",
                    "--permission-mode",
                    mode,
                    "--model",
                    "gpt-5.6-sol",
                    "--effort",
                    "high",
                    "--no-session-persistence",
                    "--restricted",
                    "--strict-mcp-config",
                    "--tools",
                    "Bash,Read,Edit,Write",
                    "--settings",
                    &settings
                ]
            );
            let read = |name: &str| std::fs::read_to_string(ws.path().join(name)).unwrap();
            assert_eq!(read("env.txt"), "11", "no background tasks or auto memory");
            let path = std::env::var_os("PATH").unwrap();
            assert_eq!(read("path.txt"), scrub_path(&path).to_str().unwrap());
            assert_eq!(read("cwd.txt").trim_end(), ws.path().to_str().unwrap());
        }
        assert_eq!(
            std::fs::read_to_string(ws.path().join("claude-code-stderr.log")).unwrap(),
            "wrapper stderr"
        );

        resolved.backend = Backend::Codex;
        resolved.codex.command = command.to_str().unwrap().into();
        resolved.codex.profile = Some("trial-profile".into());
        resolved.codex.model = "configured-model".into();
        resolved.codex.effort = "xhigh".into();
        resolved.codex.extra_args = vec!["--configured-extra".into()];
        let confine = codex_confinement(&ws).to_vec();
        let profile: toml::Table = toml::from_str(&confine[3]).expect("valid codex -c value");
        let fs = &profile["permissions"]["hauksbee"]["filesystem"];
        let checker = dir.path().join("hauksbee").display().to_string();
        assert_eq!(
            [&fs[&checker], &fs[":tmpdir"]].map(|v| v.as_str()),
            [Some("read"), Some("none")]
        );
        assert_eq!(fs[":workspace_roots"]["."].as_str(), Some("write"));
        let read_only = vec!["--sandbox".to_string(), "read-only".into()];
        let approve = [vec!["--approve-for-me".into()], confine.clone()].concat();
        let modes = [
            ("read-only", read_only),
            ("workspace-write", confine),
            ("approve-for-me", approve),
        ];
        for (mode, permission_args) in modes {
            resolved.codex.permission_mode = mode.into();
            let output = run_agent_once(&resolved, "fake prompt", &ws).unwrap();
            let mut expected = vec![
                "exec".to_string(),
                "-p".into(),
                "trial-profile".into(),
                "--model".into(),
                "configured-model".into(),
                "-c".into(),
                "model_reasoning_effort=\"xhigh\"".into(),
            ];
            expected.extend(permission_args);
            expected.extend([
                "--skip-git-repo-check".into(),
                "--cd".into(),
                ws.path().display().to_string(),
                "--output-last-message".into(),
                ws.path().join("last-message.txt").display().to_string(),
            ]);
            // A profile opts into the operator's codex config: no --ignore-user-config.
            expected.extend(["--ephemeral", "--ignore-rules", "-c"].map(Into::into));
            expected.push("skills.include_instructions=false".into());
            for feature in CODEX_DISABLED_FEATURES {
                expected.extend(["--disable".into(), (*feature).into()]);
            }
            expected.push("--configured-extra".into());
            let actual = output.lines().map(str::to_string).collect::<Vec<_>>();
            assert_eq!(actual, expected, "codex permission mode {mode}");
        }
        resolved.codex.profile = None;
        let output = run_agent_once(&resolved, "fake prompt", &ws).unwrap();
        assert!(output.contains("--ephemeral\n--ignore-user-config\n--ignore-rules\n-c\n"));
        assert!(output.contains("--disable\nskill_search\n") && output.contains("shell_snapshot"));
        assert!(output.starts_with("exec\n--model\n"), "{output}");
        resolved.backend = Backend::Agy;
        resolved.agy.command = command.to_str().unwrap().into();
        let a = &resolved.agy;
        let expected = format!(
            "--output-format\ntext\n--mode\n{}\n--model\n{}\n--effort\n{}\n--add-dir\n{}\n--print={}",
            a.permission_mode,
            a.model,
            a.effort,
            ws.path().display(),
            String::from_utf8_lossy(AGENT_POINTER)
        );
        let output = run_agent_once(&resolved, "fake prompt", &ws).unwrap();
        assert_eq!(output.trim_end(), expected);
        resolved.backend = Backend::ClaudeCode;
        let relative = command
            .strip_prefix(std::env::current_dir().unwrap())
            .unwrap();
        resolved.claude_code.command = relative.to_str().unwrap().into();
        assert_eq!(smoke_test(&resolved, &mut |_| {}).unwrap(), "OK");

        std::fs::write(
            &command,
            "#!/bin/sh\ncat >/dev/null\nprintf 'not OK\\n' > model.toml\n",
        )
        .unwrap();
        let invalid = smoke_test(&resolved, &mut |_| {}).unwrap_err().to_string();
        assert!(
            invalid.contains("must contain exactly OK"),
            "unexpected error: {invalid}"
        );

        std::fs::write(&command, "#!/bin/sh\ncat >/dev/null\nprintf 'OK\\n'\n").unwrap();
        let denied = smoke_test(&resolved, &mut |_| {}).unwrap_err().to_string();
        assert!(
            denied.contains("sandbox read/write check") && denied.contains("model.toml"),
            "unexpected error: {denied}"
        );

        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(which_path(command.to_str().unwrap()).is_none());
        std::fs::remove_file(&command).unwrap();
        assert!(require_tool(&resolved)
            .unwrap_err()
            .to_string()
            .contains("No fallback"));
        assert!(run_agent_once(&resolved, "fake prompt", &ws)
            .unwrap_err()
            .to_string()
            .contains("custom wrapper"));
        assert!(smoke_test(&resolved, &mut |_| {}).is_err());
    }

    #[test]
    fn scrubbed_path_drops_every_directory_holding_a_hauksbee() {
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("hauksbee"), "").unwrap();
        let path = std::env::join_paths([other.path(), Path::new("/usr/bin")]).unwrap();
        assert_eq!(scrub_path(&path), "/usr/bin");
    }

    #[test]
    fn windows_command_discovery_uses_pathext_without_unix_which() {
        assert_eq!(
            command_extensions("pdftotext", true, Some(".EXE;.CMD")),
            [".exe", ".cmd"]
        );
        assert_eq!(
            command_extensions(r"C:\Program Files\pdftotext.exe", true, Some(".EXE;.CMD")),
            [""]
        );
        assert_eq!(command_extensions("pdftotext", false, None), [""]);
    }

    #[test]
    fn api_never_receives_a_local_pdf_path_fallback() {
        assert!(!api_source_available(&format!(
            "prompt {PDF_TEXT_UNAVAILABLE}"
        )));
        assert!(api_source_available("prompt with extracted datasheet text"));
    }

    #[test]
    fn local_agent_prompt_does_not_embed_layout_lost_pdf_text() {
        let noise = "RAW_TABLE_NOISE_THAT_MUST_NOT_REACH_THE_AGENT";
        let marker = noise.repeat(1_000);
        let model_prompt = build_prompt("TXB0101", "digital", &marker);
        assert!(
            model_prompt.contains(noise),
            "the API backend keeps the text"
        );
        let local = prompt_for_agent_backend(&model_prompt);
        assert!(!local.contains(noise));
        assert!(local.contains("Read the attached page images first"));
        assert!(local.contains("datasheet.txt or datasheet.pdf"));
        assert!(local.len() < model_prompt.len());

        let sensor_prompt = build_sensor_prompt("TMP117", "i2c_sensor", &marker);
        let local_sensor = prompt_for_agent_backend(&sensor_prompt);
        assert!(!local_sensor.contains(noise));
        assert!(local_sensor.contains("datasheet.txt or datasheet.pdf"));

        // Kind identification is a separate small call made before the
        // workspace exists, so prompts without the bounded source markers are
        // deliberately unchanged.
        let identify = "identify this part from the first-page text";
        assert_eq!(prompt_for_agent_backend(identify), identify);
    }
}
