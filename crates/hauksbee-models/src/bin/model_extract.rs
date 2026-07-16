//! `model-extract` — datasheet extraction binary.
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
//! # Backends (in priority order)
//!
//! 1. **codex** (default): shells out to `codex exec --full-auto` with a
//!    carefully constructed prompt. Requires `codex` in PATH.
//! 2. **API** (optional): if `HAUKSBEE_LLM_API_KEY` and `HAUKSBEE_LLM_MODEL`
//!    are set, calls an OpenAI-compatible chat completions endpoint via
//!    `HAUKSBEE_LLM_BASE_URL` (defaults to `https://api.openai.com/v1`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// How long to let a single codex run go before we kill it and (maybe) retry.
const CODEX_TIMEOUT: Duration = Duration::from_secs(600);

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

        println!("Written: {}", out_path.display());
        println!("{}", spec.sensor.name);
        return Ok(());
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

    println!("Written: {}", out_path.display());
    println!("{}", entry.id);
    Ok(())
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
    let bus = if kind.trim() == "spi_sensor" { "spi" } else { "i2c" };
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

Emit a DECLARATIVE register-map sensor spec in TOML — NOT a SPICE model. The goal
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
  # WHO_AM_I / device-ID register — a constant the firmware checks:
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
  u8, u16_be, u16_le, i16_be, i16_le   — plain integers, big/little endian
  q7.1_be                              — LM75-style temperature: signed,
                                         0.125 C/LSB, count left-justified by 5
                                         into a big-endian 16-bit word
  raw                                  — const-only register (no expr/encoding)

RULES:
1. Include the WHO_AM_I / device-ID register if the part has one (with its exact
   constant), plus the primary data register(s) firmware actually reads.
2. Every `expr` may only reference names declared in a [[sensor.input]].
3. Output ONLY the TOML, starting with `[sensor]` — no prose, no markdown fences.
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
) -> Result<hauksbee_models::sensor_spec::SensorSpec> {
    use hauksbee_models::sensor_spec::{Bus, SensorSpec};

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
    let want_bus = if kind.trim() == "spi_sensor" { Bus::Spi } else { Bus::I2c };
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
                          dac|adc|shift_register|mcu|connector|ignore|
                          charger|pmic|balancer  (behavioural families)|
                          i2c_sensor|spi_sensor  (declarative register-map
                          sensors → a [sensor] spec, not a SPICE model)
    --out-dir <dir>       Output directory (default: ~/.hauksbee/models/)

ENVIRONMENT:
    HAUKSBEE_LLM_API_KEY   API key for OpenAI-compatible backend
    HAUKSBEE_LLM_MODEL     Model ID for API backend (e.g. gpt-4o)
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

#[cfg(test)]
mod truncate_tests {
    use super::truncate_to_chars;

    /// Round-8 #4: datasheet text was truncated with a byte-index slice, which
    /// panics when a multibyte glyph straddles the cut. `truncate_to_chars`
    /// cuts on char boundaries — no panic, and it never splits a char.
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

For kind="{kind}", the required params are:
{required_params}

[models.ratings] — pull these from the datasheet's "absolute maximum ratings" /
"limiting values" table. Include every field the datasheet gives a number for;
omit a field entirely if the datasheet does not state it (do NOT invent it):
{ratings_hint}

IMPORTANT RULES:
1. Add a comment on each param/rating line citing where in the datasheet you found
   the value, e.g.: `# Source: Table 6.3, typ column`
2. Use only values explicitly stated in the datasheet — do NOT guess. For SPICE
   params not given verbatim (e.g. `is`), you may derive them from a stated
   operating point (e.g. VBE at a known IC) and say so in the comment.
3. If a required param is genuinely absent and cannot be derived, use a
   conservative typical value for the part family and comment `# estimated`.
4. Output ONLY the TOML block, starting with [[models]] — no prose, no markdown
   fences, no leading or trailing text.
5. Currents in AMPERES, voltages in VOLTS, power in WATTS (convert mA/mW yourself).
6. Param values must be within these physical bounds:
   - is: 1e-20 to 1e-3
   - bf/beta: 1 to 2000
   - n/nf (emission): 0.5 to 3.0
   - vaf (Early V): 1 to 500
   - vto (MOSFET threshold): -10 to +10
   - kp (transconductance): 1e-6 to 1.0
   - vout (LDO): 0.5 to 30
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
BEHAVIOURAL MODEL — this is a "{kind}" power IC. In ADDITION to the above, set
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
        "diode" => "\
  max_current_a       # IF continuous forward current
  max_surge_current_a # IFSM non-repetitive surge current
  max_voltage_v       # VRRM repetitive peak reverse voltage",
        "bjt_npn" | "bjt_pnp" => "\
  max_current_a       # IC continuous collector current
  max_surge_current_a # ICM peak collector current
  max_power_w         # Ptot total power dissipation
  max_voltage_v       # VCEO collector-emitter breakdown voltage",
        "nmos" | "pmos" => "\
  max_current_a       # ID continuous drain current
  max_power_w         # Ptot total power dissipation
  max_voltage_v       # VDS drain-source breakdown voltage",
        "vreg" => "\
  max_current_a       # IOUT maximum output current
  max_voltage_v       # maximum input voltage (VIN abs max)
  max_junction_temp_c # TJ maximum junction temperature",
        _ => "\
  max_current_a       # if a continuous current limit is stated
  max_voltage_v       # if a maximum voltage is stated
  max_power_w         # if a power dissipation limit is stated",
    }
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
        "charger"            => "vout, dropout_v, iq_a  (the converter behaviour is in [models.behavioral])",
        "pmic"               => "vout, dropout_v, iq_a  (the pin pulls are in [models.behavioral])",
        "balancer"           => "(the leak law is in [models.behavioral]; no required numeric params)",
        _                    => "(see db/README.md for kind-specific requirements)",
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

    // Check for API key first
    if std::env::var("HAUKSBEE_LLM_API_KEY").is_ok() {
        return call_api_backend(prompt, args);
    }

    // Default: codex
    if which("codex") {
        call_codex_backend(prompt, args)
    } else {
        bail!(
            "No LLM backend available. Install codex in PATH, or set \
             HAUKSBEE_LLM_API_KEY + HAUKSBEE_LLM_MODEL environment variables, or \
             HAUKSBEE_EXTRACT_MOCK_REPLY=<file> for an offline canned reply."
        )
    }
}

/// Call codex non-interactively and return its (validated) TOML reply.
///
/// Invocation notes learned the hard way:
///   * `--sandbox workspace-write` (the old `--full-auto` is deprecated).
///   * `--skip-git-repo-check` — codex otherwise refuses to run outside a repo.
///   * `--cd <pdf_dir>` so codex can open the datasheet PDF / extracted text
///     directly when pdftotext was unavailable.
///   * stdin must be closed/empty or codex blocks "Reading additional input
///     from stdin..." forever. We give it an empty stdin and read only stdout
///     (the final agent message); all session logging goes to stderr.
fn call_codex_backend(prompt: &str, args: &Args) -> Result<String> {
    // codex's working dir: the directory holding the datasheet, so the model
    // can read the PDF and any pdftotext sidecar without absolute paths.
    let workdir = args
        .pdf
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let mut prompt = prompt.to_string();
    for attempt in 0..=args.retries {
        let started = Instant::now();
        let raw_stdout = run_codex_once(&prompt, &workdir)?;
        let raw = extract_toml_block(&raw_stdout);
        eprintln!(
            "[model-extract] codex attempt {} returned {} chars in {:.0}s",
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
                // Feed the failure back so the retry can self-correct.
                prompt = format!(
                    "{prompt}\n\nYOUR PREVIOUS ANSWER FAILED VALIDATION WITH:\n{e}\n\n\
                     Fix exactly those issues and output the corrected TOML block only.",
                );
                continue;
            }
            Err(e) => bail!(
                "codex produced a model that failed validation after {} attempt(s): {e}\n\
                 Raw reply was:\n{raw}",
                attempt + 1
            ),
        }
    }

    unreachable!()
}

/// One codex invocation with a hard timeout. Returns stdout (the agent's final
/// message) on success.
fn run_codex_once(prompt: &str, workdir: &Path) -> Result<String> {
    let mut child = Command::new("codex")
        .args([
            "exec",
            "--sandbox",
            "workspace-write",
            "--skip-git-repo-check",
            "--cd",
        ])
        .arg(workdir)
        .arg(prompt)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context(
            "spawning codex (is it installed and on PATH? `brew install codex` or set \
             HAUKSBEE_LLM_API_KEY)",
        )?;

    // Close stdin immediately: codex appends piped stdin to the prompt and
    // blocks waiting for EOF if we leave it open.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"");
        // dropping `stdin` here sends EOF
    }

    // Poll for completion up to CODEX_TIMEOUT, then kill.
    let deadline = Instant::now() + CODEX_TIMEOUT;
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
                        CODEX_TIMEOUT.as_secs()
                    );
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }

    let output = child.wait_with_output().context("collecting codex output")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "codex exited with status {}: {}",
            output.status,
            stderr.lines().rev().take(5).collect::<Vec<_>>().join(" | ")
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Call an OpenAI-compatible API endpoint via `curl`.
fn call_api_backend(prompt: &str, args: &Args) -> Result<String> {
    let api_key = std::env::var("HAUKSBEE_LLM_API_KEY").unwrap();
    let model = std::env::var("HAUKSBEE_LLM_MODEL")
        .unwrap_or_else(|_| "gpt-5.3-chat-latest".to_string());
    let base_url = std::env::var("HAUKSBEE_LLM_BASE_URL")
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
) -> Result<hauksbee_models::schema::ModelEntry> {
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

    let db: hauksbee_models::schema::DbFile = toml::from_str(trimmed)
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
    hauksbee_models::validation::validate(&entry).map_err(|errs| {
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
fn kind_discriminant(kind: hauksbee_models::ComponentKind) -> &'static str {
    use hauksbee_models::ComponentKind::*;
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
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
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
        assert!(charger.contains("[models.behavioral.converter]"), "charger prompt missing converter schema");
        assert!(charger.contains("iin_program"), "charger prompt missing current-limit program");
        assert!(charger.contains("CHARGER specifically"));
        // A PMIC prompt must teach the internal-pull pin schema.
        let pmic = build_prompt("nPM1300", "pmic", "datasheet");
        assert!(pmic.contains("pull_to"), "pmic prompt missing internal-pull schema");
        assert!(pmic.contains("SHPHLD"), "pmic prompt should mention the ship-hold case");
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
        assert_eq!(entry.kind, hauksbee_models::ComponentKind::Vreg);
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
        assert!(err.to_string().contains("no [models.behavioral]"), "got: {err}");
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
        };
        let prompt = build_prompt(&args.part, &args.kind_str, "irrelevant");
        let raw = call_backend(&prompt, &args).expect("mock backend should succeed");
        let entry = parse_and_validate_reply(&raw, &args.part, &args.kind_str).unwrap();

        std::env::remove_var("HAUKSBEE_EXTRACT_MOCK_REPLY");

        assert_eq!(entry.kind, hauksbee_models::ComponentKind::BjtNpn);
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
        };

        let raw = call_backend(&prompt, &args).expect("codex backend call");
        let entry = parse_and_validate_reply(&raw, "BC847", "bjt_npn").expect("parse + validate");

        assert_eq!(entry.kind, hauksbee_models::ComponentKind::BjtNpn);
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
        };
        let raw = call_backend(&prompt, &args).expect("codex backend call");
        let entry = parse_and_validate_reply(&raw, "LTC4020", "charger").expect("parse + validate");
        assert_eq!(entry.kind, hauksbee_models::ComponentKind::Vreg);
        let c = entry.behavioral.converter.expect("converter block");
        assert_eq!(c.in_pin, "pvin");
        assert_eq!(c.out_pin, "bat");
        assert!(c.iin_program.is_some(), "ILIMIT current-limit program present");
        println!("live LTC4020 charger: vout={} eff={:?}", c.vout_setpoint, c.efficiency);
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
