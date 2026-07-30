//! The `/api/models/*` backend: draft a device model from a datasheet the user
//! uploaded in the browser, then let them review it before anything is kept.
//!
//! This is the web half of `hauksbee models extract`, and it holds the same
//! consent contract that command holds. The contract, in order:
//!
//!   1. Nothing is sent until asked. The browser must show
//!      `datasheet::CONSENT_NOTICE` (served verbatim from [`ready_json`], so the
//!      web and CLI wordings cannot drift) and get a click before it may even
//!      offer a file picker.
//!   2. The user is told BEFORE choosing a file whether an extraction can run at
//!      all. An extraction that dies on "codex is not signed in" after the
//!      datasheet has been picked has already wasted the one thing the notice
//!      was asking permission for.
//!   3. The result is a draft, labelled `datasheet-extracted`, with every value
//!      the model admitted it assumed called out.
//!   4. Nothing reaches the user's model library until [`save`] is called, which
//!      only an explicit accept does.
//!
//! The extraction itself is `hauksbee_models::datasheet`, unchanged: same
//! sandbox, same prompt, same validation, so a model drafted from the browser is
//! the same model the CLI would have drafted.

// A browser reaches this module, and `hauksbee serve` runs on a machine whose
// owner did not write the page's JS, so a panic here is a denial of service
// rather than a CLI crash. Failures are typed messages the caller reports.
// Test code is exempt: an unwrap in a test is an assertion.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hauksbee_models::datasheet;
use hauksbee_server::frontdoor::{DatasheetHooks, DatasheetJob};

/// The provenance every model drafted this way carries. The string is the one
/// `pack.toml` validates against (`hauksbee_models::pack::Provenance`), so a
/// drafted model can be published as a pack without relabelling.
const PROVENANCE: &str = "datasheet-extracted";

/// How often the stream says it is still alive while codex works.
///
/// A codex extraction is silent for one to three minutes (its own output goes to
/// the server's stderr, not to us), and a stream that says nothing for three
/// minutes is indistinguishable from a hung one. The heartbeat is the honest
/// minimum: it does not claim progress it cannot see, it reports elapsed time.
const HEARTBEAT: Duration = Duration::from_secs(10);

/// Component kinds an extraction accepts, and what each is for.
///
/// The browser's kind picker is generated from this list rather than carrying
/// its own copy, because a hardcoded frontend list rots the moment a kind is
/// added and then quietly offers a kind the extractor rejects. Mirrors
/// `model-extract --help`; the behavioural families (charger / pmic / balancer)
/// and the declarative sensors (i2c_sensor / spi_sensor) are included because
/// `datasheet::run` has a real path for each.
const KINDS: &[(&str, &str)] = &[
    ("passive", "resistor, capacitor, inductor"),
    ("diode", "diode, Schottky, Zener, LED"),
    ("bjt_npn", "NPN bipolar transistor"),
    ("bjt_pnp", "PNP bipolar transistor"),
    ("nmos", "N-channel MOSFET"),
    ("pmos", "P-channel MOSFET"),
    ("vreg", "linear regulator or LDO"),
    ("opamp", "operational amplifier"),
    ("comparator", "comparator"),
    ("analog_switch", "analogue switch or load switch"),
    ("digital", "logic gate or digital part"),
    ("dac", "digital-to-analogue converter"),
    ("adc", "analogue-to-digital converter"),
    ("shift_register", "shift register"),
    ("mcu", "microcontroller"),
    ("connector", "connector"),
    ("charger", "battery charger (behavioural)"),
    ("pmic", "power-management IC (behavioural)"),
    ("balancer", "cell balancer or monitor (behavioural)"),
    ("i2c_sensor", "I2C sensor (register-map spec)"),
    ("spi_sensor", "SPI sensor (register-map spec)"),
];

/// One extraction at a time, per server.
///
/// Not a fairness measure: each run spawns an autonomous agent that renders
/// pages, shells out, and costs the user money. Two at once doubles the bill for
/// a page that can only show one card, and the browser flow has no way to ask
/// for a second while the first is streaming, so a second arriving at all means
/// something is wrong.
static EXTRACTING: AtomicBool = AtomicBool::new(false);

struct ExtractSlot;

impl ExtractSlot {
    fn acquire() -> Option<ExtractSlot> {
        EXTRACTING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| ExtractSlot)
    }
}

impl Drop for ExtractSlot {
    fn drop(&mut self) {
        EXTRACTING.store(false, Ordering::Release);
    }
}

/// The hooks the server mounts at `/api/models/*`.
pub fn hooks() -> DatasheetHooks {
    use std::sync::Arc;
    DatasheetHooks {
        ready: Arc::new(ready_json),
        extract: Arc::new(|job, progress| extract(job, progress)),
        save: Arc::new(save),
        check: Arc::new(check),
    }
}

/// Which backend an extraction would use, and whether it can run right now.
enum Backend {
    /// A canned reply from `HAUKSBEE_EXTRACT_MOCK_REPLY`: the offline test hook.
    Mock,
    /// The OpenAI-compatible API backend (`HAUKSBEE_LLM_API_KEY` is set).
    Api,
    /// codex, signed in and usable.
    Codex,
    /// codex is the only backend available and it cannot run. Carries the
    /// reason and the one command that fixes it, both from the engine's own
    /// dependency probe so this can never disagree with the Environment page.
    Blocked { reason: String, fix: String },
}

/// Pick the backend, asking the SAME probe the Environment page renders.
///
/// Duplicating the codex discovery here is exactly the drift this codebase
/// keeps burning: the panel would say "installed" while the extraction said
/// "not found". `deps::probe_all` costs a handful of `--version` calls, which is
/// nothing against a run measured in minutes.
fn backend() -> Backend {
    if std::env::var_os("HAUKSBEE_EXTRACT_MOCK_REPLY").is_some() {
        return Backend::Mock;
    }
    if std::env::var_os("HAUKSBEE_LLM_API_KEY").is_some() {
        return Backend::Api;
    }
    let probe = crate::deps::probe_all();
    let codex = probe.iter().find(|d| d.id == "codex");
    match codex {
        Some(d) if d.present => Backend::Codex,
        Some(d) => Backend::Blocked {
            reason: d
                .detail
                .clone()
                .unwrap_or_else(|| "codex is not usable on this machine".to_string()),
            fix: d.manual.clone(),
        },
        // The probe list is built in this crate, so a missing codex row is a
        // programming error rather than a machine state. Say that instead of
        // inventing a machine problem the user could go and look for.
        None => Backend::Blocked {
            reason: "this build's dependency probe reports no codex entry, so extraction \
                     readiness cannot be established"
                .to_string(),
            fix: "npm install -g @openai/codex   # then: codex login".to_string(),
        },
    }
}

/// `GET /api/models/extract/ready`: everything the browser needs to decide
/// whether to offer extraction, and what to say while asking.
///
/// The consent notice and the kind list are served from here rather than
/// hardcoded in the page for the same reason: one source, no drift.
pub fn ready_json() -> String {
    let (ready, backend_id, reason, fix) = match backend() {
        Backend::Mock => (true, "mock", None, None),
        Backend::Api => (true, "api", None, None),
        Backend::Codex => (true, "codex", None, None),
        Backend::Blocked { reason, fix } => (false, "codex", Some(reason), Some(fix)),
    };
    let kinds: Vec<serde_json::Value> = KINDS
        .iter()
        .map(|(id, label)| serde_json::json!({ "id": id, "label": label }))
        .collect();
    serde_json::to_string(&serde_json::json!({
        "ready": ready,
        "backend": backend_id,
        "reason": reason,
        "fix": fix,
        "consent_notice": datasheet::CONSENT_NOTICE,
        "provenance": PROVENANCE,
        "kinds": kinds,
        // The cost line is the one that changes minds, and it is the same one
        // the Environment page shows for codex: most people who would want
        // datasheet extraction already pay for ChatGPT, and the only thing
        // between them and it is not knowing that codex signs in with it.
        "cost": "Codex signs in with a ChatGPT account, so if you already pay for one this \
                 costs nothing extra. Otherwise it bills against whatever account you sign in \
                 with.",
    }))
    .unwrap_or_else(|_| {
        "{\"ready\":false,\"reason\":\"could not serialise readiness\"}".to_string()
    })
}

/// Run one extraction and return the reviewable model card as JSON.
///
/// Never writes to the model library: the card goes back for review and [`save`]
/// is the only thing that keeps it.
pub fn extract(job: DatasheetJob, progress: &mut dyn FnMut(&str)) -> Result<String, String> {
    let Some(_slot) = ExtractSlot::acquire() else {
        return Err(
            "an extraction is already running on this server; wait for it to finish".to_string(),
        );
    };

    // Refuse before sending, not after. The browser already asked
    // `/api/models/extract/ready`, but that answer is a moment old and the
    // route is reachable without it.
    match backend() {
        Backend::Blocked { reason, fix } => {
            return Err(format!("{reason}\n\nFix it with: {fix}"));
        }
        Backend::Mock | Backend::Api | Backend::Codex => {}
    }

    // A file that is not a PDF cannot be extracted from, and finding that out
    // after the upload has been sent to an LLM would be the one failure mode the
    // consent notice exists to prevent.
    if !job.pdf.starts_with(b"%PDF-") {
        return Err(format!(
            "'{}' does not look like a PDF (it does not start with %PDF-). \
             Datasheet extraction needs the PDF itself, not a screenshot or a web page.",
            job.pdf_name
        ));
    }

    if !KINDS.iter().any(|(id, _)| *id == job.kind) {
        return Err(format!(
            "'{}' is not a component kind this extractor knows. Pick one of: {}",
            job.kind,
            KINDS
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let staging = tempfile::Builder::new()
        .prefix("hauksbee-web-extract-")
        .tempdir()
        .map_err(|e| format!("could not create a staging directory: {e}"))?;
    let pdf_path = staging.path().join("datasheet.pdf");
    std::fs::write(&pdf_path, &job.pdf)
        .map_err(|e| format!("could not stage the uploaded datasheet: {e}"))?;
    // The draft lands here and is read back, never installed. `datasheet::run`
    // insists on writing its answer somewhere; pointing it at a temp directory
    // is what keeps "nothing is saved before you accept" true rather than
    // aspirational.
    let out_dir = staging.path().join("draft");

    progress(&format!(
        "Extracting a {} model for {} from {} ({} KiB).",
        job.kind,
        job.part,
        job.pdf_name,
        job.pdf.len() / 1024
    ));
    progress(
        "The datasheet is copied into a scratch sandbox and its first pages rendered; the \
         agent never sees your own directories.",
    );
    progress(
        "Sent. The model reads the pages, checks each value against the page it came from, and \
         writes the draft. This usually takes one to three minutes.",
    );

    let args = datasheet::Args::new(pdf_path, job.part.clone(), job.kind.clone())
        .out_dir(Some(out_dir.clone()));

    // On its own thread so the heartbeat below can run: `datasheet::run` is one
    // long blocking call with no callback of its own.
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::spawn(move || {
        let outcome = datasheet::run(args).map_err(|e| format!("{e:#}"));
        let _ = tx.send(outcome);
    });
    let started = Instant::now();
    loop {
        match rx.recv_timeout(HEARTBEAT) {
            Ok(Ok(())) => break,
            Ok(Err(e)) => return Err(e),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => progress(&format!(
                "still working ... {}s elapsed",
                started.elapsed().as_secs()
            )),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(
                    "the extraction stopped without reporting a result; see the server log"
                        .to_string(),
                )
            }
        }
    }

    let (path, toml_text) = read_draft(&out_dir)?;
    progress(&format!(
        "Draft written after {}s. Nothing has been saved: review it below.",
        started.elapsed().as_secs()
    ));

    let card = build_card(
        &job,
        &toml_text,
        path.extension().is_some_and(|e| e == "toml"),
    );
    serde_json::to_string(&card).map_err(|e| format!("could not serialise the model card: {e}"))
}

/// Read back the single file `datasheet::run` wrote. It names the file after the
/// part (`<part>.toml`, or `<part>.sensor.toml` for a register-map sensor), so
/// the draft is found by looking rather than by rebuilding its naming rules
/// here, which would break the day either name changes.
fn read_draft(out_dir: &std::path::Path) -> Result<(PathBuf, String), String> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(out_dir)
        .map_err(|e| format!("the extraction wrote no draft directory: {e}"))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    found.sort();
    let Some(path) = found.into_iter().next() else {
        return Err(
            "the extraction reported success but wrote no TOML; see the server log".to_string(),
        );
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("could not read the drafted model back: {e}"))?;
    Ok((path, text))
}

// ── The model card ────────────────────────────────────────────────────────────

/// One value in the drafted model, with the datasheet citation the prompt asked
/// the model to leave beside it.
#[derive(serde::Serialize)]
struct CardValue {
    /// Which block it came from: `params`, `ratings`, `pins`, `match`, or the
    /// entry itself.
    section: String,
    key: String,
    value: String,
    /// The trailing `# ...` comment, verbatim. This is the citation
    /// ("Table 6.3, typ column") that makes a number checkable, and dropping it
    /// would leave the reviewer with a plausible number and no way to check it.
    source: String,
    /// True when the citation admits the value was not stated in the datasheet.
    assumed: bool,
}

/// The reviewable draft, as the browser renders it.
#[derive(serde::Serialize)]
struct ModelCard {
    ok: bool,
    /// The board reference this was drafted for, so the review says which part
    /// on which board it is about.
    reference: String,
    part: String,
    kind: String,
    provenance: &'static str,
    /// The model id the entry declares (what the binder will match on).
    model_id: String,
    description: String,
    /// The file name a save would use.
    file_name: String,
    /// The whole draft, editable before accepting.
    toml: String,
    values: Vec<CardValue>,
    /// Every value the model said it assumed, plus anything it wrote into a
    /// `notes` field. Surfaced separately because these are the numbers a
    /// reviewer must go and check; buried in a 60-line TOML they get skimmed.
    assumptions: Vec<String>,
}

/// Does this citation admit the number was not read off the datasheet?
///
/// Deliberately keyed on what the prompt tells the model to write ("# estimated",
/// an assumption recorded in `notes`), plus the phrasings that mean the same
/// thing. It over-reports rather than under-reports: a value wrongly flagged as
/// assumed costs the reviewer a glance, a real assumption presented as measured
/// costs them a wrong simulation they trusted.
fn looks_assumed(comment: &str) -> bool {
    let c = comment.to_ascii_lowercase();
    [
        "estimat",
        "assum",
        "not stated",
        "unstated",
        "not given",
        "guess",
        "inferred",
        "typical value",
        "read off",
        "from the graph",
        "derived",
    ]
    .iter()
    .any(|needle| c.contains(needle))
}

/// Split a TOML line into `key`, `value` and its trailing comment.
///
/// A line scan, because the citations live in comments and every TOML parser
/// throws comments away. Quoted `#` is respected so a description containing a
/// hash does not lose half of itself to a phantom comment.
fn split_line(line: &str) -> Option<(String, String, String)> {
    let (key, rest) = line.split_once('=')?;
    let key = key.trim();
    if key.is_empty() || key.starts_with('#') || key.starts_with('[') {
        return None;
    }
    let mut in_string = false;
    let mut cut = rest.len();
    for (i, ch) in rest.char_indices() {
        match ch {
            '"' => in_string = !in_string,
            '#' if !in_string => {
                cut = i;
                break;
            }
            _ => {}
        }
    }
    let (value, comment) = rest.split_at(cut);
    Some((
        key.trim_matches('"').to_string(),
        value.trim().trim_end_matches(',').to_string(),
        comment.trim_start_matches('#').trim().to_string(),
    ))
}

/// Assemble the card from the drafted TOML.
fn build_card(job: &DatasheetJob, toml_text: &str, is_model_entry: bool) -> ModelCard {
    let mut values = Vec::new();
    let mut assumptions = Vec::new();
    let mut section = "entry".to_string();
    for line in toml_text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // `[models.params]` -> "params"; `[[models]]` / `[sensor]` -> the
            // entry itself.
            let inner = trimmed.trim_matches(|c| c == '[' || c == ']');
            section = inner
                .rsplit('.')
                .next()
                .filter(|s| *s != "models" && *s != "sensor")
                .unwrap_or("entry")
                .to_string();
            continue;
        }
        let Some((key, value, comment)) = split_line(trimmed) else {
            continue;
        };
        // `notes` is prose the model wrote about itself, and it is already
        // rendered in full as the assumptions list. As a table row it is worse
        // than useless: a multi-line array shows up as the single character `[`.
        if key == "notes" {
            continue;
        }
        // The opening line of any other multi-line array or table carries no
        // value of its own. Nothing in the schema needs one displayed, and a row
        // reading `straps = [` reads as a corrupt extraction.
        if value.is_empty() || value == "[" || value == "{" {
            continue;
        }
        let assumed = looks_assumed(&comment);
        if assumed {
            assumptions.push(format!("{key} = {value}  ({comment})"));
        }
        values.push(CardValue {
            section: section.clone(),
            key,
            value,
            source: comment,
            assumed,
        });
    }

    // A `notes` field is where the prompt tells the model to record what it
    // assumed. The schema drops the key on parse, so it is read off the raw
    // text; the TOML parse (not the line scan) is used because a notes array
    // spans lines.
    let parsed: Option<toml::Value> = toml_text.parse().ok();
    if let Some(root) = &parsed {
        let notes = root
            .get("models")
            .and_then(|m| m.as_array())
            .and_then(|a| a.first())
            .or_else(|| root.get("sensor"))
            .and_then(|entry| entry.get("notes"));
        match notes {
            Some(toml::Value::String(s)) => assumptions.push(s.clone()),
            Some(toml::Value::Array(items)) => {
                for item in items {
                    if let Some(s) = item.as_str() {
                        assumptions.push(s.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    let entry = parsed.as_ref().and_then(|root| {
        root.get("models")
            .and_then(|m| m.as_array())
            .and_then(|a| a.first())
            .or_else(|| root.get("sensor"))
            .cloned()
    });
    let field = |name: &str| -> String {
        entry
            .as_ref()
            .and_then(|e| e.get(name))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let model_id = {
        let id = field("id");
        if id.is_empty() {
            field("name")
        } else {
            id
        }
    };

    ModelCard {
        ok: true,
        reference: job.reference.clone(),
        part: job.part.clone(),
        kind: job.kind.clone(),
        provenance: PROVENANCE,
        model_id,
        description: field("description"),
        file_name: file_name_for(&job.part, is_model_entry && !is_sensor(toml_text)),
        toml: toml_text.to_string(),
        values,
        assumptions,
    }
}

fn is_sensor(toml_text: &str) -> bool {
    toml_text
        .lines()
        .any(|l| l.trim_start().starts_with("[sensor]"))
}

/// The library file name for a part. Mirrors `datasheet::run`'s own naming so a
/// model saved from the browser lands where the CLI would have put it.
fn file_name_for(part: &str, spice_entry: bool) -> String {
    let stem: String = part
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if spice_entry {
        format!("{stem}.toml")
    } else {
        format!("{stem}.sensor.toml")
    }
}

// ── Accepting a draft ─────────────────────────────────────────────────────────

/// The standing user model directory, `~/.hauksbee/models`: the layer
/// `ModelLibrary::builtin_with_user_dirs` documents as "where datasheet
/// extraction writes", and the same directory `hauksbee models extract` uses
/// when given no `--out-dir`.
fn user_model_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".hauksbee").join("models"))
}

/// `POST /api/models/save`: keep a reviewed draft.
///
/// Re-validates the TOML it is handed. The card is editable in the browser, so
/// what arrives here is not necessarily what the extractor produced, and writing
/// an invalid model into the library would turn every later run's bind step into
/// a puzzle.
/// Report what hauksbee can do with a pasted SPICE model, card by card.
///
/// Someone arriving with a vendor `.lib` deserves to know what of it we handle
/// BEFORE they commit to it, and to be told why for anything we refuse. An
/// unexplained "unsupported" is where trust goes: the user cannot tell whether
/// their file is wrong, their part is exotic, or we are simply thin.
///
/// Be honest about the shape of the support. The SPICE reader parses `.model`
/// and `.subckt` cards and keeps their parameters and ports; it is not a SPICE
/// evaluator. A `.model` of a type we map (a BJT, a diode, a MOSFET) becomes a
/// device the solver runs. A `.subckt` is a netlist of other parts, and we
/// report it rather than pretending to flatten it.
pub fn spice_report(text: &str) -> Result<String, String> {
    let cards = hauksbee_models::spice_input::parse_spice_text(text)
        .map_err(|e| format!("this does not parse as SPICE: {e}"))?;
    if cards.is_empty() {
        return Err(
            "no .model or .subckt card found. Paste the card itself, not a whole netlist \
             or a schematic export."
                .to_string(),
        );
    }
    let mut lines = Vec::new();
    for c in &cards {
        match c.kind {
            hauksbee_models::spice_input::SpiceCardKind::Model => {
                let ty = c.model_type.as_deref().unwrap_or("?");
                let mapped = matches!(
                    ty.to_ascii_uppercase().as_str(),
                    "NPN" | "PNP" | "D" | "NMOS" | "PMOS"
                );
                if mapped {
                    lines.push(format!(
                        "SUPPORTED  .model {} ({}), {} parameter(s): hauksbee runs this as a \
                         device",
                        c.name,
                        ty,
                        c.params.len()
                    ));
                } else {
                    lines.push(format!(
                        "NOT MAPPED .model {} ({}): hauksbee maps NPN, PNP, D, NMOS and PMOS \
                         cards. This type is parsed but has no device behind it yet.",
                        c.name, ty
                    ));
                }
            }
            hauksbee_models::spice_input::SpiceCardKind::Subckt => lines.push(format!(
                "SUBCIRCUIT .subckt {} ({} port(s)): a subcircuit is a netlist of other \
                 parts rather than a device. hauksbee reads its ports and text but does not \
                 flatten it into a solvable circuit, so it will not simulate on its own.",
                c.name,
                c.ports.len()
            )),
        }
    }
    Ok(lines.join("\n"))
}

/// Validate a hand-written model WITHOUT saving it.
///
/// The same checks `save` runs, in the same order, minus the write. Someone
/// typing a model needs to know it is wrong while they are typing, not after
/// they commit to keeping it, and the errors they see here have to be the ones
/// that would actually stop the save. Re-implementing a friendlier check would
/// mean a model that validates in the editor and is refused on save, which is
/// worse than no editor at all.
///
/// Returns a one-line description of what the model IS on success, because
/// "valid" alone does not tell an author whether they wrote the part they
/// meant to.
pub fn check(toml_text: &str) -> Result<String, String> {
    if toml_text.trim().is_empty() {
        return Err("nothing to check yet".to_string());
    }
    if is_sensor(toml_text) {
        let spec = hauksbee_models::SensorSpec::from_toml(toml_text)
            .map_err(|e| format!("this is not a valid sensor spec: {e}"))?;
        return Ok(format!(
            "valid sensor spec: {} on {:?}",
            spec.sensor.name, spec.sensor.bus
        ));
    }
    let db: hauksbee_models::schema::DbFile =
        toml::from_str(toml_text).map_err(|e| format!("this is not valid model TOML: {e}"))?;
    let entry = db
        .models
        .first()
        .ok_or_else(|| "there is no [[models]] entry here yet".to_string())?;
    if let Err(errors) = hauksbee_models::validation::validate(entry) {
        return Err(errors
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("; "));
    }
    // Same reason save does it: a model that lints clean and fails to compile
    // breaks at bind time, a long way from the person who wrote it.
    if !entry.logic.is_empty() {
        crate::logic::LogicComponent::compile(&entry.id, &entry.logic)
            .map_err(|e| format!("the [models.logic] block does not compile: {e}"))?;
    }
    // Echo what the model IS, not just that it parsed. An author who typed
    // the wrong kind gets a valid model of the wrong device, and this line is
    // where they notice.
    Ok(format!(
        "valid: {} is a {:?}{}",
        entry.id,
        entry.kind,
        if entry.description.is_empty() {
            String::new()
        } else {
            format!(" ({})", entry.description)
        }
    ))
}

pub fn save(part: &str, kind: &str, toml_text: &str) -> Result<String, String> {
    let sensor = is_sensor(toml_text);
    if sensor {
        hauksbee_models::SensorSpec::from_toml(toml_text)
            .map_err(|e| format!("this is not a valid sensor spec, so it was not saved: {e}"))?;
    } else {
        let db: hauksbee_models::schema::DbFile = toml::from_str(toml_text)
            .map_err(|e| format!("this is not valid model TOML, so it was not saved: {e}"))?;
        let entry = db
            .models
            .first()
            .ok_or_else(|| "there is no [[models]] entry to save".to_string())?;
        if let Err(errors) = hauksbee_models::validation::validate(entry) {
            return Err(format!(
                "the model failed validation, so it was not saved: {}",
                errors
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        // The logic block is compiled through the SAME path binding uses, for
        // the reason `hauksbee models add` does it: a model that lints clean but
        // fails to compile is a model that breaks at bind time, far from here.
        if !entry.logic.is_empty() {
            crate::logic::LogicComponent::compile(&entry.id, &entry.logic)
                .map_err(|e| format!("the model's [models.logic] block does not compile: {e}"))?;
        }
    }

    let dir = user_model_dir().ok_or_else(|| {
        "HOME is not set, so there is no ~/.hauksbee/models to save into. Copy the TOML \
         yourself and put it wherever you point --models-dir."
            .to_string()
    })?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let path = dir.join(file_name_for(part, !sensor));
    // Never clobber. A same-named file is either an earlier draft the user kept
    // or a model they hand-tuned, and silently overwriting the second one loses
    // work no undo can recover.
    if path.exists() {
        return Err(format!(
            "{} already exists. Move or delete it first if you mean to replace it; \
             hauksbee will not overwrite a model you already have.",
            path.display()
        ));
    }
    std::fs::write(&path, toml_text)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;

    serde_json::to_string(&serde_json::json!({
        "ok": true,
        "path": path.display().to_string(),
        "provenance": PROVENANCE,
        "kind": kind,
        "note": format!(
            "Saved as a draft with provenance \"{PROVENANCE}\". It binds on the next analysis. \
             Check any assumed value before you trust a result that depends on it."
        ),
    }))
    .map_err(|e| format!("could not serialise the save result: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> DatasheetJob {
        DatasheetJob {
            pdf_name: "tp4054.pdf".to_string(),
            pdf: Vec::new(),
            reference: "U3".to_string(),
            part: "TP4054".to_string(),
            kind: "vreg".to_string(),
        }
    }

    const DRAFT: &str = r#"[[models]]
id = "tp4054"
kind = "vreg"
description = "Single-cell Li-ion linear charger"
notes = ["iq_a was read off Figure 3; the table gives no quiescent current"]

[models.match]
mpn_re = "TP4054"

[models.params]
vout = 4.2          # Source: Table 1, charge voltage
dropout_v = 0.3     # estimated from the 4.5 V minimum input
iq_a = 0.00015      # Source: Figure 3

[models.ratings]
max_current_a = 0.5 # Source: absolute maximum ratings
"#;

    /// The citation beside each number is the whole point of the review card: a
    /// reviewer with the number but not its source cannot check anything.
    #[test]
    fn card_keeps_each_value_with_its_citation() {
        let card = build_card(&job(), DRAFT, true);
        let vout = card
            .values
            .iter()
            .find(|v| v.key == "vout")
            .expect("vout is in the draft");
        assert_eq!(vout.section, "params");
        assert_eq!(vout.value, "4.2");
        assert_eq!(vout.source, "Source: Table 1, charge voltage");
        assert!(!vout.assumed, "a cited table value is not an assumption");
        assert_eq!(card.provenance, "datasheet-extracted");
        assert_eq!(card.model_id, "tp4054");
        assert_eq!(card.file_name, "TP4054.toml");
        // The `notes` array is prose, rendered in full as the assumptions list.
        // As a table row its multi-line opener showed up as the single
        // character `[`, which reads as a corrupt extraction.
        assert!(
            !card.values.iter().any(|v| v.key == "notes"),
            "notes is not a value row"
        );
    }

    /// An assumed value that reads like a measured one is the failure this whole
    /// surface exists to prevent, so both channels the prompt offers the model
    /// (an `# estimated` comment and a `notes` entry) must reach the reviewer.
    #[test]
    fn assumptions_are_called_out_from_comments_and_notes() {
        let card = build_card(&job(), DRAFT, true);
        let dropout = card
            .values
            .iter()
            .find(|v| v.key == "dropout_v")
            .expect("dropout_v is in the draft");
        assert!(dropout.assumed, "'estimated' is an admitted assumption");
        assert!(
            card.assumptions.iter().any(|a| a.contains("dropout_v")),
            "the estimated param is listed: {:?}",
            card.assumptions
        );
        assert!(
            card.assumptions.iter().any(|a| a.contains("Figure 3")),
            "the notes entry is listed: {:?}",
            card.assumptions
        );
    }

    /// A `#` inside a quoted string is not a comment. The description used to
    /// lose everything after it and the citation column gained a fragment of
    /// prose.
    #[test]
    fn a_hash_inside_a_string_is_not_a_citation() {
        let (key, value, comment) =
            split_line(r#"description = "charger #2 variant"  # Source: page 1"#)
                .expect("the line parses");
        assert_eq!(key, "description");
        assert_eq!(value, r#""charger #2 variant""#);
        assert_eq!(comment, "Source: page 1");
    }

    /// The browser must not be able to put an invalid model in the library by
    /// editing the card before accepting it.
    #[test]
    fn save_refuses_a_model_that_fails_validation() {
        // bf (current gain) of 5 is outside the physical range validation
        // enforces, so this must never reach ~/.hauksbee/models.
        let bad =
            "[[models]]\nid = \"x\"\nkind = \"bjt_npn\"\n\n[models.match]\nmpn_re = \"X\"\n\n\
                   [models.params]\nis = 1e-14\nbf = 0.001\nnf = 1.0\nvaf = 100.0\nbr = 1.0\n";
        let err = save("X", "bjt_npn", bad).expect_err("an invalid model is refused");
        assert!(
            err.contains("not saved"),
            "the refusal says nothing was written: {err}"
        );
    }

    /// The kind picker is generated from this list, so an entry the extractor
    /// would reject must never appear in it.
    #[test]
    fn every_offered_kind_is_one_the_extractor_accepts() {
        for (id, label) in KINDS {
            assert!(!id.is_empty() && !label.is_empty(), "{id} has a label");
            assert!(!id.contains(' '), "{id} is a machine kind, not a phrase");
        }
    }
}
