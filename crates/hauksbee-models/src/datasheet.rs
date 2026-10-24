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
//!    prompt. Requires `codex` in PATH. Defaults to gpt-6.1-sol at high effort.
//! 2. **claude-code**: shells out to headless `claude -p` with the same
//!    prompt contract. Requires `claude` in PATH. Defaults to Opus 5.5 at high
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

use crate::extract_cache::Prompt;
use crate::extract_config::{self, ProcessHost, Resolved, ResolvedApi};
use crate::extract_harness as harness;
use crate::extract_output as output;
use crate::extract_progress as progress;
use crate::schema::{ComponentKind, ModelEntry};
use crate::sensor_spec::{Bus, SensorSpec};

/// Narration on stderr that never shares a line with the CLI spinner.
macro_rules! say {
    ($($arg:tt)*) => { progress::say(&format!($($arg)*)) };
}

mod sections;

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
    reasons: Why,
    supplemental_reasons: Why,
    score: usize,
}

/// Topic labels a page was selected for.
type Why = Vec<&'static str>;

impl SelectedPage {
    const fn new(number: usize, reasons: Why, supplemental_reasons: Why, score: usize) -> Self {
        Self {
            number,
            reasons,
            supplemental_reasons,
            score,
        }
    }
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
/// codex gets a named permission profile instead (`codex_confinement`):
/// shell commands may read this directory, the OS/runtime paths codex calls
/// `:minimal`, the package prefixes in `CONFINED_READ_ROOTS` and the checker
/// executable, and nothing under home, `/tmp` or `$TMPDIR` (`read-only` mode gets
/// the same profile with the workspace read-only). Claude Code gets
/// `--restricted` file tools and a Bash sandbox denying home, user, volume and
/// temp roots (`claude_confinement`); its shell can still read other system
/// paths. agy gets only its own `--sandbox`, which hauksbee cannot verify; codex's
/// `danger-full-access` mode and custom commands are NOT confined by hauksbee.
/// The CLI process itself still reads
/// its own sign-in, and whatever the agent reads goes to the vendor.
///
/// The rest of the contract still matters: the run happens only when the user
/// asked for it, the output is a small TOML document validated against a
/// schema before anything is saved, and the user reviews the card.
///
/// Dropping this removes the directory, so a killed run leaves nothing behind.
#[derive(Debug)]
pub struct Workspace {
    dir: crate::extract_cache::slots::Sandbox,
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
    /// Where each agent call's prompt and event stream are kept, when traces
    /// are on ([`crate::extract_trace`]).
    pub trace: Option<std::sync::Arc<crate::extract_trace::Trace>>,
}

impl Workspace {
    /// An empty sandbox with no datasheet in it, for a connectivity check that
    /// runs the backend on a one-line prompt. Same isolation as a real run.
    ///
    /// # Errors
    ///
    /// Fails when the sandbox directory cannot be created under the temp root.
    pub fn scratch() -> Result<Self> {
        let dir = workspace_tempdir(&std::env::temp_dir(), "creating the check sandbox")?;
        Ok(Self {
            pdf: dir.path().join("datasheet.pdf"),
            dir,
            pages: Vec::new(),
            selected_pages: Vec::new(),
            category_disclosures: Vec::new(),
            has_text_dump: false,
            checker: None,
            trace: None,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Where codex must write its answer. Reading a file beats scraping stdout:
    /// stdout carries the agent's narration too, and a model that says "here
    /// is the TOML" twice leaves two candidate blocks to choose between.
    #[must_use]
    pub fn answer_path(&self) -> PathBuf {
        self.dir.path().join("model.toml")
    }

    /// The sandbox directory's name: which agent a streamed event belongs to.
    #[must_use]
    pub fn label(&self) -> String {
        let name = self.dir.path().file_name().unwrap_or_default();
        name.to_string_lossy().into_owned()
    }

    /// A private copy of this sandbox (PDF, text, pages, renders), so agents
    /// running at once do not share `prompt.md` or the `model.toml` each writes.
    fn fork(&self) -> Result<Self> {
        fn copy_all(from: &Path, to: &Path) -> std::io::Result<()> {
            for entry in std::fs::read_dir(from)? {
                let entry = entry?;
                let (src, dst) = (entry.path(), to.join(entry.file_name()));
                if src.is_dir() {
                    std::fs::create_dir_all(&dst)?;
                    copy_all(&src, &dst)?;
                } else {
                    std::fs::copy(&src, &dst)?;
                }
            }
            Ok(())
        }
        let parent = self.path().parent().unwrap_or_else(|| Path::new("."));
        let dir = workspace_tempdir(parent, "creating a section sandbox")?;
        copy_all(self.path(), dir.path()).context("copying the sandbox for a section agent")?;
        let moved = |p: &PathBuf| dir.path().join(p.file_name().unwrap_or_default());
        Ok(Self {
            pdf: moved(&self.pdf),
            pages: self.pages.iter().map(moved).collect(),
            selected_pages: self.selected_pages.clone(),
            category_disclosures: self.category_disclosures.clone(),
            has_text_dump: self.has_text_dump,
            checker: self.checker.clone(),
            trace: self.trace.clone(),
            dir,
        })
    }
}

/// Create a sandbox directory whose retained path is the canonical absolute
/// path used by every file operation, child cwd, and provider path argument.
/// Its name is the process's lowest free slot, not a random one (see
/// [`crate::extract_cache::slots`]), so the path the agent is told is the same
/// for every part that runs in that slot.
fn workspace_tempdir(root: &Path, context: &str) -> Result<crate::extract_cache::slots::Sandbox> {
    crate::extract_cache::slots::Sandbox::create(root, context)
}

/// Build a sandbox, for the tests that check the boundary holds.
///
/// The boundary is the whole security story of an agent running full-auto, so
/// it is asserted rather than trusted. Exposed for `tests/extract_sandbox.rs`.
///
/// # Errors
///
/// Fails when the sandbox cannot be created or the PDF cannot be copied into it.
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
    let dir = workspace_tempdir(root, "creating the extraction sandbox")?;

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
    let front_pages = || {
        let why = "front-page fallback because the PDF text layer was unavailable";
        (1..=MAX_RENDERED_PAGES)
            .map(|n| SelectedPage::new(n, vec![why], vec![], 0))
            .collect()
    };
    // The contents pages and the pin table, read once here for the agent's `ds`
    // helpers (and the deterministic checks) to print.
    let texts: Vec<String> = page_text.iter().flatten().map(|p| p.text.clone()).collect();
    let toc = crate::ds_toolkit::write_datasheet_files(dir.path(), &copied, &texts);
    let selections = page_text.as_deref().map_or_else(front_pages, |pages| {
        let limit = render_limit(pages.len());
        with_key_sections(select_relevant_pages(pages, limit), &toc, limit)
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
                );
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
    // Per-page text and an index, for a staged agent that must not read it all.
    let topics = |text: &str| -> Vec<&'static str> {
        let found = PAGE_TOPICS.iter().filter(|t| topic_score(text, t.2) > 0);
        found.map(|t| t.0).collect()
    };
    let indexed = page_text.iter().flatten();
    harness::write_pages(
        dir.path(),
        indexed.map(|p| (p.number, p.text.as_str(), topics(&p.text))),
    );

    Ok(Workspace {
        pdf: copied,
        pages,
        selected_pages,
        category_disclosures,
        has_text_dump,
        dir,
        checker: None,
        trace: None,
    })
}

/// Write a sandbox's text files for `pdf` into `into` (page texts, the page
/// index, `toc.md`, `pins/`), with no renders and no agent: for replaying
/// saved drafts against the deterministic checks and scoring page selection.
/// Returns the pages the selection picks without the contents and with them.
///
/// # Errors
///
/// The PDF has no text layer, or `into` cannot be written.
#[doc(hidden)]
pub fn prepare_files(pdf: &Path, into: &Path) -> Result<(Vec<usize>, Vec<usize>)> {
    std::fs::create_dir_all(into)?;
    let pages = extract_pdf_pages_text(pdf)?;
    let texts: Vec<String> = pages.iter().map(|p| p.text.clone()).collect();
    let toc = crate::ds_toolkit::write_datasheet_files(into, pdf, &texts);
    let before = select_relevant_pages(&pages, MAX_RENDERED_PAGES);
    let limit = render_limit(pages.len());
    let now = with_key_sections(select_relevant_pages(&pages, limit), &toc, limit);
    let topics = |text: &str| -> Vec<&'static str> {
        let found = PAGE_TOPICS.iter().filter(|t| topic_score(text, t.2) > 0);
        found.map(|t| t.0).collect()
    };
    harness::write_pages(
        into,
        pages
            .iter()
            .map(|p| (p.number, p.text.as_str(), topics(&p.text))),
    );
    let numbers = |v: Vec<SelectedPage>| v.into_iter().map(|p| p.number).collect();
    Ok((numbers(before), numbers(now)))
}

/// Run one extraction end to end, as the CLI does.
///
/// # Errors
///
/// Fails when the input is not a PDF, the kind cannot be identified, the settings
/// cannot be resolved, the backend is unavailable or fails, the draft never
/// validates within the retries, the run times out, or the result cannot be
/// written.
pub fn run(args: Args) -> Result<PathBuf> {
    // The usage ledger and the plan reading are process-wide, one run's worth.
    // With several runs at once (`--unmodelled --jobs N`) the batch resets the
    // ledger once and reports the whole cost at its end.
    if progress::concurrent() {
        return run_inner(args);
    }
    crate::extract_usage::reset();
    crate::extract_plan::clear();
    let outcome = run_inner(args);
    if let Some(line) = crate::extract_plan::end() {
        say!("[model-extract] {line}");
        crate::extract_usage::note_plan(line);
    }
    let cost = crate::extract_usage::summary();
    if !cost.is_empty() {
        say!("[model-extract] {cost}");
    }
    outcome
}

/// A draft that failed validation on its last attempt, saved beside the
/// output as `rejected/<file>` (not a directory the model scan reads) with the
/// errors on top, so it can be fixed by hand rather than only read in a log.
fn keep_rejected(e: anyhow::Error, dest: &Path, args: &Args) -> anyhow::Error {
    let Some(r) = e.downcast_ref::<Rejected>() else {
        return e;
    };
    let name = dest.file_name().unwrap_or_default();
    let path = dest.with_file_name("rejected").join(name);
    let body = output::finalise(&r.draft, &args.part, &args.aliases, &args.pdf)
        .unwrap_or_else(|_| r.draft.clone());
    let errors = r
        .error
        .lines()
        .fold(String::new(), |acc, l| acc + "# " + l + "\n");
    let saved = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| {
            std::fs::write(
                &path,
                format!("# hauksbee rejected this draft:\n{errors}\n{body}"),
            )
        });
    match saved {
        Ok(()) => Rejected {
            kept: Some((path, dest.to_path_buf())),
            ..r.clone()
        }
        .into(),
        Err(io) => e.context(format!(
            "(could not save the draft to {}: {io})",
            path.display()
        )),
    }
}

/// Add the board's footprints for `part` to the prompt's tail, so the card models
/// the package the board places.
fn name_footprints(prompt: &mut Prompt, part: &str, footprints: &[String]) {
    use std::fmt::Write as _;
    if footprints.is_empty() {
        return;
    }
    let _ = write!(
        prompt.tail,
        "\n\nTHE BOARD places {part} on footprint {}: model the package that footprint \
         names (P4) and name it in the description.",
        footprints.join(", ")
    );
}

/// The datasheet's text, after the checks that come before anything is sent.
///
/// Magic bytes rather than extension, and BEFORE any backend is chosen: a
/// non-PDF (a saved HTML page, a .docx) would otherwise be text-dumped and
/// shipped to the LLM, producing a confidently wrong model; a downloaded
/// datasheet with no extension still passes.
fn precheck(args: &Args) -> Result<String> {
    if !is_pdf(&args.pdf) {
        bail!(
            "'{}' is not a PDF (no %PDF header); the extractor reads PDF datasheets only. \
             Nothing was sent.",
            args.pdf.display()
        );
    }
    progress::start();
    let pdf_text = extract_pdf_text(&args.pdf);
    if let Some(why) = wrong_datasheet(&args.part, &args.aliases, &pdf_text) {
        bail!(
            "{why}: this looks like the wrong datasheet for {}. Nothing was sent; fetch the \
             datasheet that names {} and run again.",
            args.part,
            args.part
        );
    }
    Ok(pdf_text)
}

/// What kind of card to draft for the part: asks the agent to identify it from
/// the datasheet's first page and says what it answered.
fn identified_card_kind(args: &Args, pdf_text: &str) -> Result<String> {
    let identified = identify_kind(args, pdf_text)?;
    let kind = card_kind(&identified).to_string();
    say!(
        "[model-extract] identified {} as kind '{identified}'{}",
        args.part,
        if identified == kind {
            String::new()
        } else {
            format!("; drafting the '{kind}' card a board binds")
        }
    );
    Ok(kind)
}

/// Work out which kind of card the part's datasheet calls for, without drafting.
///
/// The same checks and the same identification [`run`] makes when no kind is
/// given, so a batch can do this as soon as a datasheet lands. Pass the result
/// as `kind_str` and the run skips it.
///
/// # Errors
///
/// When the file is not a PDF, is the wrong datasheet, or no supported kind fits.
pub fn classify_part(args: &Args) -> Result<String> {
    let pdf_text = precheck(args)?;
    identified_card_kind(args, &pdf_text)
}

// One run, top to bottom, as the user sees it.
#[allow(clippy::too_many_lines)]
fn run_inner(mut args: Args) -> Result<PathBuf> {
    let pdf_text = precheck(&args)?;

    // An empty kind means "you work it out". Being made to classify a part
    // before the tool will look at it is a barrier at exactly the wrong
    // moment: the datasheet says what the part is on its first page, and the
    // person asking for a model is precisely the one who may not know which
    // of our categories their part falls into.
    if args.kind_str.trim().is_empty() {
        args.kind_str = identified_card_kind(&args, &pdf_text)?;
    }
    say!(
        "[model-extract] part={} kind={} pdf={}",
        args.part,
        args.kind_str,
        args.pdf.display()
    );

    if !is_sensor_kind(&args.kind_str) && args.templates.is_empty() {
        args.templates =
            crate::card_templates::select(&args.part, &args.kind_str, crate::card_templates::LIMIT);
    }
    let (part, kind) = (args.part.as_str(), args.kind_str.as_str());
    // The destination is settled before anything is sent: an existing model
    // may be hand-corrected, and finding that out after a paid run is too late.
    let sensor = is_sensor_kind(kind);
    let suffix = if sensor { ".sensor.toml" } else { ".toml" };
    let out_dir = args.out_dir.clone().unwrap_or_else(default_out_dir);
    let out_path = out_dir.join(format!("{}{suffix}", sanitise_filename(part)));
    if !args.force && out_path.exists() {
        bail!("{} already exists, and it may hold your corrections; hauksbee will not overwrite it. Nothing was sent. Move or delete it, write the draft elsewhere with --out-dir, or pass --force to replace it.", out_path.display());
    }
    // Declarative register-map sensor kinds emit a `[sensor]` spec validated
    // as a `SensorSpec`, NOT the SPICE `[[models]]` schema.
    let (raw, id, timed_out) = if sensor {
        let prompt = build_sensor_prompt(part, kind, &pdf_text);
        let (raw, timed_out) =
            call_backend(&Prompt::from(prompt), &args, Reply::Sensor { part, kind })
                .map_err(|e| keep_rejected(e, &out_path, &args))?;
        let name = validate_sensor_reply(&raw, part, kind)?.sensor.name;
        (raw, name, timed_out)
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
        let mut prompt = build_prompt_parts(part, kind, &pdf_text, &args.templates);
        name_footprints(&mut prompt, part, &args.footprints);
        let (raw, timed_out) = call_backend(&prompt, &args, Reply::Model { part, kind })
            .map_err(|e| keep_rejected(e, &out_path, &args))?;
        let entry = parse_and_validate_reply(&raw, part, kind)?;
        if let Some(warning) = crate::validation::unknown_pads_warning(&entry) {
            say!("[model-extract] warning: {warning}");
        }
        for warning in crate::validation::missing_rating_warnings(&entry) {
            say!("[model-extract] warning: [models.ratings] {warning}");
        }
        let id = entry.id;
        // A board library names an orderable part by a wildcard of its code
        // (`STM32F407VETx`): the card must bind that spelling too.
        let mut aliases = args.aliases.clone();
        aliases.extend(crate::card_coverage::ordering_wildcards(part));
        (
            output::finalise(&raw, part, &aliases, &args.pdf)?,
            id,
            timed_out,
        )
    };

    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("creating output directory {}", out_dir.display()))?;
    output::publish(&out_path, &raw, args.force)
        .with_context(|| format!("writing output to {}", out_path.display()))?;
    if let Some(t) = timed_out {
        bail!(TimedOut(t.0, t.1, Some(out_path)));
    }
    progress::out(&format!(
        "[model-extract] written: {}\n{id}",
        out_path.display()
    ));
    Ok(out_path)
}

/// The kind an identified part is drafted as. A register-map sensor kind gives
/// a bare `[sensor]` spec no board binds (no match, pads or ratings): measured,
/// five round-3 parts (an RTC, a fuel gauge, an EEPROM, a touch controller, an
/// encoder) identified that way came back without a card, and each `digital`
/// rerun bound. So an identified sensor is drafted as the digital card; the
/// spec stays available by asking for its kind with `--kind`.
fn card_kind(identified: &str) -> &str {
    if is_sensor_kind(identified) {
        "digital"
    } else {
        identified
    }
}

/// The part-number stem a datasheet must print for `part`: its first
/// alphanumeric token through its last digit (`PCF8563BS` → `PCF8563`,
/// `BSS84-7-F` → `BSS84`), upper-cased; none under four characters (`S4B-XH-A`
/// is a template position, not a searchable name).
fn part_stem(part: &str) -> Option<String> {
    let token = part
        .split(|c: char| !c.is_ascii_alphanumeric())
        .find(|t| !t.is_empty())?;
    let end = token.rfind(|c: char| c.is_ascii_digit())? + 1;
    let stem = token[..end].to_ascii_uppercase();
    (stem.len() >= 4 && stem.chars().any(|c| c.is_ascii_alphabetic())).then_some(stem)
}

/// A stem's family: through its first run of letters (`MPSA42` → `MPSA`,
/// `24AA025` → `24AA`).
fn stem_family(stem: &str) -> &str {
    let first_letter = stem.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(0);
    let end = stem[first_letter..]
        .find(|c: char| !c.is_ascii_alphabetic())
        .map_or(stem.len(), |n| first_letter + n);
    &stem[..end]
}

/// Whether a printed part stem names `stem`: equal, or a family name it
/// begins (`TPS6213` / `TPS6213x` for `TPS62130`, `STM32F103x8` for
/// `STM32F103C8T6`, an `x` standing for any one character).
fn names_stem(printed: &str, stem: &str) -> bool {
    printed.len() >= 5.min(stem.len())
        && printed.len() <= stem.len()
        && printed
            .chars()
            .zip(stem.chars())
            .all(|(p, s)| p == s || p == 'X')
}

/// The part-number stems printed on one line of datasheet text.
fn line_stems(line: &str) -> Vec<String> {
    line.split(|c: char| !c.is_ascii_alphanumeric())
        .filter_map(part_stem)
        .collect()
}

/// Whether one line of datasheet text prints any of the `wanted` stems, or a
/// family name covering one (see [`names_stem`]).
fn line_names(line: &str, wanted: &[String]) -> bool {
    let squashed = squash(line);
    wanted
        .iter()
        .any(|w| squashed.contains(w.as_str()) || line_stems(line).iter().any(|p| names_stem(p, w)))
}

/// Letters and digits only, upper-cased: how a part number is compared.
#[must_use]
pub fn squash(text: &str) -> String {
    (text.chars())
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// The stem a datasheet must print for `part` (see `part_stem`), if the name
/// is long enough to search for.
#[must_use]
pub fn stem_of(part: &str) -> Option<String> {
    part_stem(part)
}

/// How a datasheet's text vouches for the part it was fetched for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Vouch {
    /// The part number (or a board value for it) is printed in full.
    Exact(String),
    /// Only the part's family base name is printed (`TPS6213x` for
    /// `TPS62130`), per the same family match the wrong-sheet guard uses.
    Family(String),
}

impl std::fmt::Display for Vouch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exact(name) => write!(f, "the PDF prints {name}"),
            Self::Family(stem) => write!(f, "the PDF prints the {stem} family name"),
        }
    }
}

/// Positive evidence that `pdf_text` is `part`'s datasheet.
///
/// For a PDF nobody on the board vouched for (one an agent found on the web): it
/// must print the part number (or a board value for it) or its family base name,
/// and must not be the wrong-sheet guard's catch. `wrong_datasheet` alone passes a sheet
/// that never names the part when nothing else contradicts it; that is right
/// for a link the board's author chose and wrong for a stranger's.
///
/// # Errors
///
/// The reason, as a clause, when the text cannot vouch: no readable text layer,
/// no mention of the part or its family, or a catch of the wrong-sheet guard.
pub fn vouches_for(part: &str, aliases: &[String], pdf_text: &str) -> Result<Vouch, String> {
    if pdf_text == PDF_TEXT_UNAVAILABLE || pdf_text.trim().len() < 500 {
        return Err(
            "the PDF has no readable text, so it cannot be checked against the part number".into(),
        );
    }
    let squashed = squash(pdf_text);
    let exact = std::iter::once(part)
        .chain(aliases.iter().map(String::as_str))
        .map(|n| (n, squash(n)))
        .find(|(_, s)| s.len() >= 3 && squashed.contains(s.as_str()));
    let verdict = if let Some((name, _)) = exact {
        Vouch::Exact(name.to_string())
    } else {
        let wanted: Vec<String> = std::iter::once(part_stem(part))
            .chain(aliases.iter().map(|a| part_stem(a)))
            .flatten()
            .collect();
        let family = wanted.iter().find(|w| {
            pdf_text
                .lines()
                .any(|l| line_names(l, std::slice::from_ref(*w)))
        });
        match family {
            Some(stem) => Vouch::Family(stem.clone()),
            None => {
                return Err(format!(
                    "the PDF never prints {part}{}",
                    wanted
                        .first()
                        .map_or_else(String::new, |s| format!(" or its family name {s}"))
                ))
            }
        }
    };
    wrong_datasheet(part, aliases, pdf_text).map_or(Ok(verdict), Err)
}

/// The text layer of a PDF for [`vouches_for`], read quietly (no progress or
/// narration). Empty when `pdftotext` is missing or finds no text.
#[must_use]
pub fn pdf_text(path: &Path) -> String {
    pdftotext(path, false)
        .map(|text| truncate_to_chars(&text, 400_000))
        .unwrap_or_default()
}

/// Why `pdf_text` is not `part`'s datasheet, checked before anything is sent:
/// its title block names another part of the same family and `part` appears
/// only as a complementary or related part, or nowhere (measured: a file saved
/// as MPSA42 was the MPSA92 sheet, naming MPSA42 only as its complementary
/// NPN). A family name covering the part (`TPS6213x`) is the part's own sheet.
/// A name too short to search, or a PDF with no text layer, is not judged.
fn wrong_datasheet(part: &str, aliases: &[String], pdf_text: &str) -> Option<String> {
    let stem = part_stem(part)?;
    if pdf_text == PDF_TEXT_UNAVAILABLE || pdf_text.len() < 2000 {
        return None;
    }
    let lines: Vec<&str> = pdf_text.lines().filter(|l| !l.trim().is_empty()).collect();
    let related = |i: usize| {
        lines[i.saturating_sub(3)..=i].iter().any(|l| {
            let l = l.to_lowercase();
            l.contains("complement") || l.contains("komplement")
        })
    };
    let wanted: Vec<String> = std::iter::once(stem.clone())
        .chain(aliases.iter().filter_map(|a| part_stem(a)))
        .collect();
    let stems = line_stems;
    let names = |l: &str| line_names(l, &wanted);
    let mentioned = |own: bool| {
        lines
            .iter()
            .enumerate()
            .any(|(i, l)| names(l) && (!own || !related(i)))
    };
    if mentioned(true) {
        return None;
    }
    let page_one = pdf_text.split('\u{000c}').next().unwrap_or_default();
    let family = stem_family(&stem);
    let other = page_one
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(6)
        .flat_map(stems)
        .find(|s| s != &stem && family.len() >= 2 && stem_family(s) == family)?;
    let anywhere = if mentioned(false) {
        "only as a complementary or related part"
    } else {
        "nowhere"
    };
    Some(format!(
        "the PDF's title names {other}, and {part} appears in it {anywhere}"
    ))
}

/// Whether `path` starts with the `%PDF-` magic, whatever its extension says.
#[must_use]
pub fn is_pdf(path: &Path) -> bool {
    let mut head = [0u8; 5];
    std::fs::File::open(path)
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head))
        .is_ok_and(|()| &head == b"%PDF-")
}

// ── Declarative register-map sensor extraction (i2c_sensor / spi_sensor) ──────

/// Is this a declarative register-map sensor kind?
fn is_sensor_kind(kind: &str) -> bool {
    matches!(kind.trim(), "i2c_sensor" | "spi_sensor")
}

/// Build the LLM prompt that instructs the model to read a sensor datasheet and
/// emit a `[sensor]` declarative register-map spec (the format defined in
/// hauksbee-models::sensor_spec). The reply is validated by parsing it as a [`SensorSpec`].
// The prompt is one template read top to bottom, as the model reads it.
#[allow(clippy::too_many_lines)]
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
    style = "i2c_pointer"      # master writes the register addr, then reads N bytes
                               # (every I2C sensor: a command-driven part with no
                               # registers keeps it and states [sensor.read_frame])"#
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
    # A command-driven part (write a command, then read the result) states its
    # measurement read here: one expr per byte in wire order (e.g. the high and
    # low byte of the raw word). A CRC byte the expr cannot compute is left out
    # and named in [sensor.source]; never a reason to write no frame at all.
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

/// An agent run killed at its time limit.
///
/// When the sandbox held a lint-clean draft it is still saved for review
/// (`kept`), and this stays the run's error so the exit status says the draft
/// is incomplete. Fields: the tool, the limit in seconds, and where the kept
/// draft was published.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0} timed out after {1}s{tail}", tail = timeout_tail(.2.as_deref()))]
pub struct TimedOut(pub String, pub u64, pub Option<PathBuf>);

/// A draft that still failed validation on the last attempt.
///
/// `draft` is the last reply; once `run` has saved it, `kept` is (where it is,
/// where it belongs when fixed) and the message gives the next commands.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{tool} produced an answer that failed validation after {attempts} attempt(s): {error}{tail}", tail = rejected_tail(.draft, .kept.as_ref()))]
pub struct Rejected {
    pub tool: String,
    pub attempts: usize,
    pub error: String,
    pub draft: String,
    pub kept: Option<(PathBuf, PathBuf)>,
}

impl Rejected {
    /// A tier whose redraft fails with exactly the error it was told to fix.
    fn repeated(tool: &str, attempt: usize, e: &anyhow::Error, draft: String) -> Self {
        Self {
            tool: tool.to_string(),
            attempts: attempt,
            error: format!(
                "attempt {attempt} fails with the same error as the attempt before, though that \
                 error was quoted to the agent, so another redraft would repeat it: {e:#}"
            ),
            draft,
            kept: None,
        }
    }

    /// A tier whose redraft repeats the draft it was told to fix.
    fn unchanged(tool: &str, attempt: usize, e: &anyhow::Error, draft: String) -> Self {
        Self {
            tool: tool.to_string(),
            attempts: attempt,
            error: format!(
                "attempt {attempt} returned the previous failing draft unchanged, so this tier \
                 is not acting on the feedback: {e:#}"
            ),
            draft,
            kept: None,
        }
    }
}

fn rejected_tail(draft: &str, kept: Option<&(PathBuf, PathBuf)>) -> String {
    let Some((saved, dest)) = kept else {
        return format!("\nRaw reply was:\n{draft}");
    };
    let quote = |p: &Path| {
        let p = p.display().to_string();
        let plain = |c: char| c.is_ascii_alphanumeric() || "/._-~+".contains(c);
        if p.chars().all(plain) {
            p
        } else {
            format!("'{}'", p.replace('\'', "'\\''"))
        }
    };
    let (s, d) = (quote(saved), quote(dest));
    format!(
        "\n\nThe last draft was kept for you to fix, with the errors above it: {s}\n  \
         check it after editing:  hauksbee models lint {s}\n  \
         keep it once it passes:  mv {s} {d}"
    )
}

/// A streaming agent run killed because no event arrived for the idle period.
///
/// Distinct from [`TimedOut`]: the run had not used its time budget, it had
/// gone silent (measured: codex calls stalling about 310 s with no event).
/// `validated_attempts` retries it once and discloses that it did.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0} emitted no event for {1}s and was killed")]
pub struct IdleStalled(pub String, pub u64);

/// What a timeout leaves the user: nothing, or a kept draft to review.
fn timeout_tail(kept: Option<&Path>) -> String {
    kept.map_or_else(
        || {
            " with no answer; retry with a tighter prompt, raise timeout_secs, or pick another \
             backend (hauksbee models backend setup)"
                .into()
        },
        |p| {
            format!(
                "; its last lint-clean draft was kept at {} for review, but it is INCOMPLETE and \
                 may be missing sections beyond its [models.coverage] missing list. Review it, or \
                 raise timeout_secs and re-run.",
                p.display()
            )
        },
    )
}

// ── Argument parsing ──────────────────────────────────────────────────────────

/// Which LLM backend an extraction talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    /// `codex exec`, the Codex agent CLI.
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
    pub const ALL: [Self; 4] = [Self::ClaudeCode, Self::Agy, Self::Codex, Self::Api];

    /// The stable id used on the command line, in the config file and on
    /// the wire.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
            Self::Agy => "agy",
            Self::Api => "api",
        }
    }

    /// The display name.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::ClaudeCode => "Claude Code",
            Self::Agy => "Antigravity (agy)",
            Self::Api => "OpenAI-compatible API",
        }
    }

    /// The executable an agent backend needs on PATH; none for the API.
    #[must_use]
    pub const fn tool(self) -> Option<&'static str> {
        match self {
            Self::Codex => Some("codex"),
            Self::ClaudeCode => Some("claude"),
            Self::Agy => Some("agy"),
            Self::Api => None,
        }
    }

    /// Whether the backend is an agent that can search the web, which datasheet
    /// discovery needs (the API backend answers from the text it is sent only).
    #[must_use]
    pub const fn can_search(self) -> bool {
        !matches!(self, Self::Api)
    }

    /// The agent backends that can search the web, as a list for a hint.
    #[must_use]
    pub fn searchers() -> Vec<&'static str> {
        Self::ALL
            .iter()
            .filter(|b| b.can_search())
            .map(|b| b.name())
            .collect()
    }

    /// The one command that makes it available.
    #[must_use]
    pub const fn install_hint(self) -> &'static str {
        match self {
            Self::Codex => "npm install -g @openai/codex   # then: codex login",
            Self::ClaudeCode => {
                "npm install -g @anthropic-ai/claude-code   # then: claude auth login"
            }
            Self::Agy => "install Antigravity from https://antigravity.google, then: agy install",
            Self::Api => {
                "export OPENAI_API_KEY=...   # or name another variable with api.api_key_env"
            }
        }
    }

    /// Where the datasheet's text goes when this backend reads it.
    #[must_use]
    pub const fn sends_data_to(self) -> &'static str {
        match self {
            Self::Codex => "OpenAI, via the account codex is signed in with",
            Self::ClaudeCode => "Anthropic, via the account claude is signed in with",
            Self::Agy => "Google, via the account agy is signed in with",
            Self::Api => "the endpoint named by api.base_url",
        }
    }
}

impl std::str::FromStr for Backend {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "codex" => Ok(Self::Codex),
            "claude-code" | "claude" => Ok(Self::ClaudeCode),
            "agy" | "antigravity" => Ok(Self::Agy),
            "api" => Ok(Self::Api),
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

#[derive(Debug, Clone)]
pub struct Args {
    pub pdf: PathBuf,
    pub part: String,
    pub kind_str: String,
    /// Board values the draft is for; the card's value rule is widened to
    /// match each one exactly (see [`output::finalise`]).
    pub aliases: Vec<String>,
    /// Board footprints the part sits on: they name the package whose pin map
    /// the draft must read when the datasheet covers several.
    pub footprints: Vec<String>,
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
    /// The board part being drafted for (value, footprint, pads and nets),
    /// written into the sandbox as `board.md` and checked against the draft's
    /// match rules and pin map ([`crate::card_coverage`]).
    pub board: Option<crate::card_coverage::Board>,
    /// Keep the agent's prompt and event stream under `<out-dir>/trace/`
    /// ([`crate::extract_trace`]); on unless `--no-trace` or
    /// `HAUKSBEE_EXTRACT_NO_TRACE` is set.
    pub trace: bool,
    /// Finished library cards of this kind shown as worked examples
    /// ([`crate::card_templates`]); chosen by the run itself.
    pub templates: Vec<crate::card_templates::Template>,
}

impl Args {
    /// Build the arguments for one extraction, for a caller that is not the
    /// standalone binary's own `--flag` parser.
    #[must_use]
    pub const fn new(pdf: PathBuf, part: String, kind_str: String) -> Self {
        Self {
            pdf,
            part,
            kind_str,
            aliases: Vec::new(),
            footprints: Vec::new(),
            out_dir: None,
            retries: 2,
            model: None,
            effort: None,
            backend: None,
            api_base: None,
            api_key_env: None,
            checker: None,
            force: false,
            board: None,
            trace: true,
            templates: Vec::new(),
        }
    }

    /// Name the board part this draft is for; see [`Args::board`].
    #[must_use]
    pub fn board(mut self, board: Option<crate::card_coverage::Board>) -> Self {
        if let Some(b) = &board {
            if !b.value.is_empty() && !self.aliases.contains(&b.value) {
                self.aliases.push(b.value.clone());
            }
            if !b.footprint.is_empty() && !self.footprints.contains(&b.footprint) {
                self.footprints.push(b.footprint.clone());
            }
        }
        self.board = board;
        self
    }

    /// Keep (or not) the agent trace; see [`Args::trace`].
    #[must_use]
    pub const fn trace(mut self, keep: bool) -> Self {
        self.trace = keep;
        self
    }

    #[must_use]
    pub fn out_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.out_dir = dir;
        self
    }

    /// Pick the model the extraction agent runs on. An empty string means "not
    /// chosen", which is what a form field the user left alone posts, and must
    /// fall through to the default rather than become `--model ""`.
    #[must_use]
    pub fn model(mut self, model: Option<String>) -> Self {
        self.model = model.filter(|m| !m.trim().is_empty());
        self
    }

    /// Pick the reasoning effort. Empty means "not chosen", as for `model`.
    #[must_use]
    pub fn effort(mut self, effort: Option<String>) -> Self {
        self.effort = effort.filter(|e| !e.trim().is_empty());
        self
    }

    /// Pick the backend. `None` keeps the configured / auto-detected one.
    #[must_use]
    pub const fn backend(mut self, backend: Option<Backend>) -> Self {
        self.backend = backend;
        self
    }

    #[must_use]
    pub fn api_base(mut self, base: Option<String>) -> Self {
        self.api_base = base.filter(|b| !b.trim().is_empty());
        self
    }

    #[must_use]
    pub fn api_key_env(mut self, name: Option<String>) -> Self {
        self.api_key_env = name.filter(|n| !n.trim().is_empty());
        self
    }

    /// Name the checker executable; see [`Args::checker`].
    #[must_use]
    pub fn checker(mut self, exe: Option<PathBuf>) -> Self {
        self.checker = exe;
        self
    }
}

/// Reject anything that is not a plausible environment-variable NAME.
///
/// The flag takes the variable's name, never the key itself: a value with `-`,
/// `.`, or other non-identifier characters is almost certainly a pasted key,
/// and accepting it would put a secret on a world-readable argv.
///
/// # Errors
///
/// Fails when `name` is empty, starts with a digit, or holds any character
/// outside `[A-Za-z0-9_]`.
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
     show` or the Settings page says which, and what confines it). The API gets the extracted \
     text. An agent CLI runs a model with a shell on a scratch copy of the PDF, its text and \
     page images, and whatever it reads can reach that vendor. hauksbee confines Codex to that \
     copy plus system paths, offline (not in danger-full-access mode; in approve-for-me its \
     reviewer may allow more). Claude Code's file tools stay in the copy; its shell cannot read \
     home, user, volume or temp directories but can read other system files. Antigravity only \
     gets its own sandbox, which hauksbee cannot verify, and hauksbee cannot confine a custom \
     command: either may read anything your account can. Nothing is sent until you ask for it. \
     The result is a draft for you to check, not a measurement: a model it writes carries \
     tier \"datasheet-derived\" and the sha256 of the PDF it read.";

/// Ask a yes/no question on the terminal, the prompt going to `out`; only `y`
/// or `yes` is a yes.
///
/// A pipe cannot answer, and guessing "yes" on its behalf would act because
/// someone ran the wrong command, so a non-terminal stdin fails with `refusal`
/// instead.
///
/// # Errors
///
/// Fails with `refusal` when stdin is not a terminal, and when writing the prompt
/// or reading the answer fails.
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

/// Parse the process's own command line; see [`parse_args_from`].
///
/// # Errors
///
/// As [`parse_args_from`].
pub fn parse_args() -> Result<Args> {
    parse_args_from(std::env::args().skip(1))
}

/// The flag parser, over any argument source so tests can drive it without a
/// process spawn. `--help` still exits: it is a terminal answer, not a value.
///
/// # Errors
///
/// Fails on an unknown flag, a flag missing its value, an unknown backend, an
/// `--api-key-env` that is not a variable name, or a missing `--pdf` or
/// `--part`.
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
                out.backend = Some(value("a value (codex, claude-code, agy, or api)")?.parse()?);
            }
            "--api-base" => out.api_base = Some(value("a value")?),
            "--force" => out.force = true,
            "--no-trace" => out.trace = false,
            "--footprint" | "--value" => {
                let v = value("a value")?;
                let mut board = out.board.take().unwrap_or_default();
                if flag == "--footprint" {
                    board.footprint = v;
                } else {
                    board.value = v;
                }
                out = out.board(Some(board));
            }
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
    --footprint <name>    The board footprint the part sits on (picks the
                          package's pin-table column and is checked against
                          the card's footprint_re)
    --value <text>        The board's value for the part (the card must bind it)
    --no-trace            Keep no agent trace (default: <out-dir>/trace/)
    --model <id>          Model for the extraction agent (default: the
                          backend's own, e.g. claude-opus-5-5 for claude-code,
                          gemini-3.8-flash for agy, gpt-6.1-sol for codex)
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
    HAUKSBEE_LLM_PRICE        in,cached,out USD per million tokens, to price a
                              model the cost estimate has no list price for
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
fn extract_pdf_text(path: &Path) -> String {
    if let Ok(text) = pdftotext(path, false) {
        progress::pages(text.matches('\u{000c}').count());
        if !text.trim().is_empty() {
            return truncate_to_chars(&text, 60_000);
        }
    }
    say!("[model-extract] pdftotext unavailable or produced no text; local agent backends will read the copied PDF directly");
    PDF_TEXT_UNAVAILABLE.to_string()
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
const PAGE_TOPICS: &[(&str, usize, &[&str])] = &[
    ("recommended operating conditions", 80_000, &["recommended operating conditions", "推荐工作条件|推荐操作条件|推奨動作条件"]),
    ("absolute maximum ratings", 80_000, &["absolute maximum ratings", "limiting values", "极限|最大额定值|絶対最大定格"]),
    ("switching characteristics", 70_000, &["switching characteristics", "开关特性|时序特性|スイッチング特性"]),
    ("electrical characteristics", 60_000, &["electrical characteristics", "电特性|电气特性|电气参数|電気的特性"]),
    ("pin functions or package pinout", 50_000, &["pin functions", "terminal functions", "pin configuration",
        "package pinout", "top view", "引脚|管脚|顶视图|ピン配置|端子説明"]),
    ("functional description or function table", 55_000, &["functional description", "function table",
        "truth table", "功能描述|真值表|逻辑功能|機能説明"]),
    ("application information or programming equations", 40_000, &["application information",
        "typical application", "design procedure", "programming equation", "典型应用|应用信息|应用电路|アプリケーション"]),
    ("ordering or package information", 0, &["ordering information", "订购信息|订货信息|封装信息|外形尺寸"]),
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
            terms.iter().any(|term| {
                if term.is_ascii() {
                    heading_text.starts_with(term)
                        || table_heading.is_some_and(|title| title.starts_with(term))
                        || detailed_heading.is_some_and(|title| title.starts_with(term))
                } else {
                    first.is_some_and(|t| t.contains(term))
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

    let why = "page 1 feature and supply summary";
    let first = SelectedPage::new(1, vec![why], vec![], usize::MAX);
    let mut selected = std::collections::BTreeMap::from([(1, first)]);

    for (label, _, terms) in PAGE_TOPICS {
        // Repeated datasheet tables usually progress from the lowest bias to
        // the highest and most representative operating point. Prefer the
        // later page within each category, then disclose every sibling page so
        // the model can inspect another voltage or package variant if needed.
        let scored = pages.iter().map(|p| (p, topic_score(&p.text, terms)));
        if let Some((page, score)) = scored.filter(|s| s.1 > 0).max_by_key(|s| s.0.number) {
            if let Some(chosen) = selected.get_mut(&page.number) {
                chosen.reasons.push(label);
                chosen.score = chosen.score.saturating_add(score);
            } else if selected.len() < limit {
                let page = SelectedPage::new(page.number, vec![label], vec![], score);
                selected.insert(page.number, page);
            }
        }
    }

    if selected.len() < limit {
        let mut remaining: Vec<SelectedPage> = pages
            .iter()
            .filter(|page| !selected.contains_key(&page.number))
            .filter_map(|p| {
                let topics = PAGE_TOPICS.iter().filter(|t| topic_score(&p.text, t.2) > 0);
                let (reasons, weights): (Vec<_>, Vec<_>) = topics.map(|t| (t.0, t.1)).unzip();
                let whole = !p.text.to_ascii_lowercase().contains("(continued)");
                let score = weights.into_iter().max().unwrap_or(0) + usize::from(whole) * 1_000;
                let score = score + p.number;
                (score > 0).then(|| SelectedPage::new(p.number, vec![], reasons, score))
            })
            .collect();
        remaining.sort_by_key(|page| (std::cmp::Reverse(page.score), page.number));
        let mut supplemented = std::collections::HashSet::new();
        for page in remaining {
            let reasons = page.supplemental_reasons.iter();
            // Inserts every reason: a page with none new adds nothing.
            if reasons.fold(false, |fresh, r| supplemented.insert(*r) | fresh) {
                selected.insert(page.number, page);
            }
            if selected.len() == limit {
                break;
            }
        }
    }
    // Keyed by page number, so already in page order.
    selected.into_values().collect()
}

/// How many pages a datasheet of `pages` pages gets rendered and inlined: the
/// base seven, one more per fifty pages up to eleven. A 200-page MCU datasheet
/// spreads a card's sources over twice the sections a 40-page LDO has.
const fn render_limit(pages: usize) -> usize {
    let extra = pages / 50;
    let limit = MAX_RENDERED_PAGES + extra;
    if limit > MAX_RENDERED_PAGES + 4 {
        MAX_RENDERED_PAGES + 4
    } else {
        limit
    }
}

/// The selection with the contents' key sections first: a section the
/// contents name (the pin table, operating conditions, thermal data, boot
/// configuration) is the page a card needs even when its heading reads unlike
/// the topic terms, and a heading match elsewhere (a revision note, a figure
/// list) is not. Measured on a 206-page MCU datasheet against the 23 pages an
/// expert card cites: the heading heuristic alone picked 2 of them and none of
/// the pin table; with the contents first, 6 including the pin table's start.
fn with_key_sections(
    selected: Vec<SelectedPage>,
    toc: &[crate::ds_toolkit::TocEntry],
    limit: usize,
) -> Vec<SelectedPage> {
    let keys = crate::ds_toolkit::key_pages(toc);
    if keys.is_empty() {
        return selected;
    }
    // Each section's first page before any section's second, so a late
    // section (thermal data, ordering codes) is not crowded out by an early
    // one split over several contents entries.
    let mut seen = std::collections::HashSet::new();
    let (firsts, repeats): (Vec<_>, Vec<_>) =
        keys.into_iter().partition(|&(_, label)| seen.insert(label));
    let keys = firsts.into_iter().chain(repeats);
    let mut chosen: std::collections::BTreeMap<usize, SelectedPage> =
        std::collections::BTreeMap::new();
    let mut rest = Vec::new();
    for page in selected {
        if page.number == 1 {
            chosen.insert(1, page);
        } else {
            rest.push(page);
        }
    }
    for (number, label) in keys {
        if chosen.len() >= limit {
            break;
        }
        chosen
            .entry(number)
            .or_insert_with(|| SelectedPage::new(number, vec![], vec![], 0))
            .reasons
            .push(label);
    }
    rest.sort_by_key(|p| std::cmp::Reverse(p.score));
    for page in rest {
        if chosen.len() >= limit {
            break;
        }
        if let Some(had) = chosen.get_mut(&page.number) {
            had.reasons.extend(page.reasons);
        } else {
            chosen.insert(page.number, page);
        }
    }
    chosen.into_values().collect()
}

fn category_disclosures(pages: &[PdfPageText], selected: &[SelectedPage]) -> Vec<String> {
    PAGE_TOPICS
        .iter()
        .filter_map(|(label, _, terms)| {
            let matching = pages.iter().filter(|p| topic_score(&p.text, terms) > 0);
            let matching: Vec<&PdfPageText> = matching.collect();
            let preferred = matching.last()?;
            let matching_numbers: Vec<usize> = matching.iter().map(|page| page.number).collect();
            let chosen = |n: &&usize| selected.iter().any(|page| page.number == **n);
            let (attached, omitted): (Vec<usize>, Vec<_>) = matching_numbers.iter().partition(chosen);
            // Labels are lowercase ASCII words.
            let capital = |w: &str| w[..1].to_ascii_uppercase() + &w[1..];
            let title = label.split(' ').map(capital).collect::<Vec<_>>().join(" ");

            if *label == "switching characteristics" {
                let squash = |line: &str| line.split_whitespace().collect::<Vec<_>>().join(" ");
                let heading = |l: &&str| l.to_ascii_lowercase().contains("switching characteristics");
                let headings = matching.iter().flat_map(|p| p.text.lines()).filter(heading);
                let headings: Vec<String> = headings.map(squash).collect();
                let points = headings.iter().filter_map(|h| h.split("VCCA =").nth(1));
                let points = points.map(|v| v.split(" (").next().unwrap_or(v).trim());
                let operating_points: std::collections::BTreeSet<_> = points.collect();
                let package_summary = match (
                    headings.iter().any(|line| line.contains("(DRY)")),
                    headings.iter().any(|line| line.contains("(Other Packages)")),
                ) {
                    (true, true) => ", with DRY and Other Packages variants",
                    (true, false) => ", with DRY variants",
                    (false, true) => ", with Other Packages variants",
                    (false, false) => "",
                };
                let chosen_heading = preferred.text.lines().find(heading).map(squash);
                let chosen_heading = chosen_heading.unwrap_or_else(|| "the later matching table".into());
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
        many => {
            let list = many.iter().map(usize::to_string).collect::<Vec<_>>();
            format!("pages {}", list.join(", "))
        }
    }
}

fn number_word(n: usize) -> String {
    ["no", "one", "two", "three", "four", "five", "six"]
        .get(n)
        .map_or_else(|| n.to_string(), ToString::to_string)
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
        .map(str::to_ascii_lowercase)
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

const SOURCE_QUANTITY_GUIDANCE: &str = r"SOURCE QUANTITIES:
- Define each destination quantity and its units before matching a datasheet row to it.
- Every printed row gets a record, whether or not a key exists for it; never delete a
  sourced record to pass the checker. A typical value is never a guaranteed limit.
- Keep every number with its stated test conditions, supply, load, temperature, package,
  and min/typ/max column; do not transplant it to another condition.
- Every condition variant of a row (temperature range, supply, package, bias) is its
  own record. Where two places state different values for one quantity, record each
  with its own basis plus an `unknown` record for it naming the conflict.
- Omit unsupported facts, or mark them explicitly unknown only where the destination
  schema provides that representation. Never invent a field or uncertainty schema.";

const PIN_ROLE_GUIDANCE: &str = r"The listed names are canonical electrical aliases, not an exhaustive list of valid pin
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
keeps its source-named role. Never invent a pin role.";

const RATINGS_RULES: &str = concat!(
    r#"[models.ratings]: these are the ONLY valid keys (any other key fails
validation). Read them from the Absolute Maximum Ratings / Limiting Values and
Thermal Characteristics tables, or the table a line names. Include every key the
datasheet states for the selected package; omit a key it does not state (do NOT
invent it). Ratings are magnitudes: for a limit printed negative (PNP, negative
rail) write |value| and keep the printed sign in the source comment.
max_junction_temp_c is the junction temperature limit, never a thermal-shutdown
or protection threshold; where the absolute maximum says internally limited, it
is the top of the operating junction range. A stated limit with no key here goes
in a characteristic record (rule 0), never under an invented key. A rating
printed for several conditions (TA, TC or TJ, board copper or mounting, package)
is one characteristic per printed row with `rating = "<key>"`, its temperature in
`conditions` and any `package` or `mounting` as printed. A current or power limit
printed per path, channel or output is one row per path, `pins` naming its
[models.pins] roles (IN1 to OUT: `pins = ["in1"]`), and a printed total a row
without `pins`. Hauksbee sets each tagged key itself to "#,
    standard_condition!(),
    ", so write any tagged row there; it reads the rows at the scenario's temperature:"
);

const ENVELOPE_RULES: &str = r"[[models.envelope]]: read the Recommended Operating Conditions and Absolute
Maximum Ratings tables. Emit supply_range for every SUPPLY-class pin whose
recommended minimum and maximum are both published, and rail_order for each
explicit relation such as VCCA <= VCCB. From an op-amp's or comparator's
VICR/CMVR row, emit one input_common_mode per channel: `supply` its positive
supply, `inputs` that channel's two inputs, `headroom_v` the test supply minus
the range's upper end (0 V to V+ - 1.5 V is 1.5; none reaching V+). Emit one
supply_range or input_common_mode per printed temperature row, each with its
`conditions` (TA = 25 °C; TA = −40 °C to 85 °C). Where an input's VI row is a
fixed voltage whatever the supply (a 5 V-tolerant input: VI −0.5 V to 6.5 V,
not VCC + 0.5 V; an MCU's five-volt-tolerant pins), emit one input_tolerance:
`pins` the roles that row covers that are inputs in every mode (data and
control inputs; never an output or a bidirectional I/O, which is driven to
VCC + 0.5 V), `max_v` the recommended VI maximum, `abs_max_v` the
absolute-maximum VI; a VI written relative to VCC is no input_tolerance. The
upper end is the guaranteed
(min/max) cell, never typ (typ alone: omit the entry). Pins, supplies and inputs
are roles from [models.pins], never pad numbers. Every `basis` names the exact
table and row; add `abs_max_v` only when the absolute-maximum table publishes
it, and then also `abs_max_basis` naming that table and row (a different table
from `basis`, so a finding that states the figure cites both). No table row to cite, no envelope: omit this section then, and never infer
a bound from a typical value, application schematic, or family convention.";

const PIN_NUMBERING: &str = r#"PIN NUMBERING, read this before you fill in [models.pins]:

  The pin map is the one field where being wrong is worse than being absent: a
  wrong map simulates a different circuit and still binds cleanly, so nobody
  finds out. Treat it as the hardest part of this job, not the easiest.

  P1. PREFER A NUMBERED TABLE. A pin-function or terminal table with a "Pin"/
      "No."/"Terminal" column is the answer: use it and cite it. Do not
      re-derive the numbering from a picture when a table exists.
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
      often differ, and an MCU's 48-pin QFN and 64-pin QFP columns share
      nothing. If the datasheet covers several packages, state which one
      this map is for in the description, and pick the package the value/
      footprint you were given implies (an ordering code's package letters
      name it). A multi-variant datasheet takes the pin numbers from the
      column or package drawing that matches the order code being carded,
      and records that package in the pinout column: the CH340 sheet's
      10-pin table column is the CH340K's ESSOP10 and swaps pads 4 and 6
      against the CH340X's own MSOP-10 drawing. `models lint` warns when a
      pinout column's package is not the card's. Every numbered map records its package (P6): the checker
      holds the map against that package's pin count, and the board binds it
      only to a footprint of that package.
  P5. Still unsure? Say so in a comment on the pins block, never present a
      guess as read: `# LOW CONFIDENCE: ...` helps, a confident wrong map hurts.
  P6. ONE COLUMN PER PACKAGE. Record the pin table's column for your package,
      every pad's name as printed (NC included; never unknown_pad_ in a
      column), and when it prints a column per package record every column,
      after [models.source]:
        [[models.source.pinout]]
        package = "<package as printed>"
        pins = { "1" = "<printed name>", "2" = "<printed name>" }
      The description names only your package. Read [models.pins] from that
      package's own column and its own figure: a figure labelled with another
      package is not yours, and the checker compares the two.
  P7. LETTERS ONLY (G/S/D, B/C/E at unnumbered leads) with the view and pin 1
      drawn (a three-lead SOT's drawn top view fixes its leads by position
      alone): number [models.pins] by the package standard and name it and the
      view in the P6 block (pins as printed); results mark those pins assumed:
        standard = "<the package's standard, from the P7 table below>"
      The checker refuses letters on a package that table lists. Otherwise,
      with no view or pin 1 shown, or a two-pin diode (A/K): keep the letters.
  P8. THE SWITCH NODE AND FEEDBACK ARE NOT THE OUTPUT. On a switching
      converter or LED driver (buck, boost, buck-boost), the pin printed SW,
      LX, PH or L1/L2 drives the inductor: map it 'sw', never 'out'. FB, VFB
      or ADJ senses the output through a divider: map it 'fb' or 'adj', never
      'out'. 'out' is only a pin the table calls the regulated output (VOUT,
      OUT, or a VOS/SENSE pin tied to it); a non-synchronous boost, whose
      output is past an external diode, has no 'out' pad at all. The binder
      finds a buck's regulated rail past its inductor itself."#;

const EXTRACTION_RULES: &str = r#"IMPORTANT RULES:
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
     iq_a = { typ = 65, max = 90, unit = "uA", conditions = "VIN = 5 V", basis = "Table 7.5, IQ, typ/max" }
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
   A per-channel row (IN1, OUT2, CH2, channel B) is attributed by the channel its
   own label names: a param, pin or law of channel 1 takes only rows labelled 1.
1. Add a comment on each param/rating line citing where in the datasheet you found
   the value, e.g.: `# Source: Table 6.3, typ column`
2. Use only values explicitly stated in the datasheet; do NOT guess. A SPICE
   model the sheet prints (.MODEL/.SUBCKT) is copied verbatim, citing it. Other
   SPICE params (e.g. `is`, `n`, `rs`) you may derive from a stated operating
   point (e.g. VBE at a known IC) or fit to a typical-characteristics figure
   (e.g. an IF-VF or ID-VDS curve) read across its plotted range, saying so in
   the comment with the figure, the points read and their conditions; a curve
   reading is typical only, never a min/max, a rating or an uncertainty interval.
   A diode is/n/rs law gives every printed VF row (typ, else max) within 10%; fit to
   max rows alone, their basis adds "law fitted to guaranteed maxima, a conservative
   stand-in for typical". Curve points may fix n outside a sub-decade printed range; rows win.
   A BJT `is` comes only from a forward-active VBE(on) row or curve, lowest IC
   first; never from VBE(sat) (saturated, forced beta) or a high-current point.
   Record every fT, Cob/Ccb, Cib/Ceb, Ciss, Crss, CT, trr, V(BR) and VZ row as a
   characteristic with its symbol, unit and bias: Hauksbee derives tf, cjc, cje,
   cgd, cgs, cjo, tt and bv/ibv from them. A Schottky (name it in the
   description) takes no tt: its trr is capacitive recovery, not stored charge.
3. Identity-only is a last resort. A typ-only or max-only row is usable, cited
   as printed; Hauksbee fits diode is/n/rs to VF rows and a MOSFET's vto/kp come
   from VGS(th) and RDS(on) rows as above, so the checker refuses an identity-only
   card whose cited rows suffice. Key [models.pins] by pad number wherever one is
   printed (pin table, package drawing, beside the letters) or P7 numbers it; else
   key by lead letters A/K, G/S/D or B/C/E and list `pad_numbers` in
   [models.coverage] missing. A required param genuinely absent and not derivable or
   fittable is never estimated: one missing param takes an `unknown` record where the
   kind's params above allow one. Only when the kind's behaviour cannot be written
   even so, emit an identity-only card (`identity_only = true`, `warning`,
   `unlocked_by`) still carrying every field the datasheet states, and name the
   missing fact as `ABSTAIN: datasheet does not state ...` in the description or an
   unknown source record. A named abstention is correct; a guessed field is wrong.
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
   the magnitude would make a negative supply into a second positive one."#;

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
         [Not embedded whole for this local agent backend: the selected pages are under \
         \"Before you answer\" below. If a needed fact is absent, find its page in \
         page-index.md and read that page.]\n\
         {DATASHEET_TEXT_END}{after}"
    )
}

/// The whole drafting prompt as one text.
#[cfg(test)]
fn build_prompt(part: &str, kind: &str, pdf_text: &str) -> String {
    build_prompt_parts(part, kind, pdf_text, &[]).full()
}

// The prompt is one template read top to bottom, as the model reads it. Its
// `base` is everything that depends on the kind alone and is byte-identical for
// every part of that kind (so a prompt cache reads it); the part, its template
// cards and its datasheet text come last, in the `tail`. With finished library
// cards of the kind (`templates`, chosen per part) the fixed other-kind format
// examples give way to them, and they ride in the tail.
#[allow(clippy::too_many_lines)]
fn build_prompt_parts(
    part: &str,
    kind: &str,
    pdf_text: &str,
    templates: &[crate::card_templates::Template],
) -> Prompt {
    let examples = format_examples_for_kind(kind);
    let other_kind_cards = examples
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
    // Two variants per kind, so the base stays shared either way.
    let example_cards = if templates.is_empty() {
        format!(
            "FORMAT EXAMPLES ONLY:\nThese are NOT the part being extracted. Each is from a \
             different part class.\nYour answer must have the SAME SHAPE, using this datasheet's \
             part, kind, pins,\nparameters, ratings, and sources. Copying the example's part \
             number or numbers\nis a WRONG ANSWER.\n\n{other_kind_cards}"
        )
    } else {
        "WORKED EXAMPLES: finished library cards of this kind follow THE PART below.".to_owned()
    };
    let template_cards = if templates.is_empty() {
        String::new()
    } else {
        format!("{}\n\n", crate::card_templates::prompt_section(templates))
    };
    let base = format!(
        r#"You are a SPICE model extraction assistant.

YOUR ANSWER IS ONE THING ONLY: the complete [[models]] TOML entry for the
component named in THE PART block that follows these instructions, extracted from
its datasheet, written in full to model.toml. There is no shorter valid answer. In particular, a component
kind, a classification, a summary, or any single field on its own is NOT an
answer and fails validation.

Component kind: {kind_line}

If the request names a family of variants (output voltages, packages, grades),
model the variant the request implies. Never merge variants, and widen
`value_re` only to that variant's ordering-table codes, never to a family root
its siblings share. Variants are the exception, only where the datasheet itself
prints distinct orderable variants: when the name you were given does not pin
one and they differ in pinout, output form or hFE gain group, emit one complete [[models]] per
variant, each with the shared family `value_re`, `variant_of = "<family>"` and
`[models.variant]` (`orderable_re`, `orderable` = its ordering-table codes,
`package`, `basis`; no default). A BJT's bf is the hFE row (min/typ/max) of the
gain group it models, as a row param; name the group in its `parameter`.

The `kind` FIELD inside your entry must come from this closed list (the schema
accepts nothing else): {legal_kinds}. When the "Component kind" line above is
empty, choosing the kind is merely the first field you fill in while writing the
complete entry. If the part's natural category is not on the list (a battery
charger, a level shifter, an addressable LED driver), do not invent a kind: use
the closest legal kind for the part's primary electrical behavior (a charger's
power path fits vreg; a level shifter fits digital) and say in `description`
what real behavior the chosen kind does not model.

{example_cards}

Produce a TOML model entry that exactly conforms to the hauksbee-models schema.
The entry must use [[models]] array syntax and include:
- id: the lowercase part number (given in THE PART block)
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
id = "the lowercase part number from THE PART block"
kind = "{entry_kind}"
description = "one line"

[models.match]
# {match_rule}
value_re = '(?i)^<the part regex from THE PART block>'

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
{required_params}{conventions}
{optional_params}
{digital_hint}
{card_fields}
{ratings_rules}
{ratings_hint}

{envelope_rules}

{pin_abs_max_rules}

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

{pin_numbering}

{extraction_rules}
{param_bounds}
{behavioral_hint}
"#,
        legal_kinds = crate::validation::KIND_NAMES.join(", "),
        entry_kind = behavioral_family_base_kind(kind).unwrap_or(kind),
        kind_line = behavioral_family_base_kind(kind).map_or_else(
            || kind.to_string(),
            |base| format!(
                "{base} (the {kind} family, written as a {base} entry; see its notes below)"
            )
        ),
        required_params = required_params_for_kind(kind),
        conventions = conventions_for_kind(kind),
        optional_params = optional_params_for_kind(kind),
        pin_roles = pin_roles_for_kind(kind),
        ratings_hint = ratings_hint_for_kind(kind),
        param_bounds = param_bounds_for_kind(kind),
        behavioral_hint = behavioral_hint_for_kind(kind),
        digital_hint = digital_hint_for_kind(kind),
        card_fields =
            crate::card_fields::prompt_guidance(behavioral_family_base_kind(kind).unwrap_or(kind)),
        pin_role_guidance = PIN_ROLE_GUIDANCE,
        ratings_rules = RATINGS_RULES,
        envelope_rules = ENVELOPE_RULES,
        pin_abs_max_rules = crate::validation::PIN_ABS_MAX_RULES,
        pin_numbering = pin_numbering(),
        extraction_rules = EXTRACTION_RULES,
        source_quantity_guidance = SOURCE_QUANTITY_GUIDANCE,
        match_rule = crate::validation::MATCH_PATTERN_RULE,
    );
    let tail = format!(
        "THE PART\n\nComponent: {part}\nIts entry's id is \"{part_lower}\" and its match line is \
         value_re = '(?i)^{part_re}' (the skeleton's placeholders above stand for these).\n\n\
         {template_cards}{library_patterns}{datasheet_text}\n\n\
         OUTPUT (TOML only, starting with [[models]]):\n",
        part_lower = part.to_lowercase(),
        part_re = regex::escape(&part.to_uppercase()),
        library_patterns = crate::extract_examples::prompt_section(
            part,
            behavioral_family_base_kind(kind).unwrap_or(kind),
            pdf_text,
        ),
        datasheet_text = datasheet_text_block(pdf_text),
    );
    Prompt::new(format!("{base}\n"), tail)
}

/// Drop the end anchor ([`crate::validation::MATCH_PATTERN_RULE`]) from each match
/// pattern of `table`; the `(key, written pattern, rewritten pattern)` triples.
fn unanchor_match(table: &mut toml_edit::Table) -> Vec<(&'static str, String, String)> {
    let mut rewritten = Vec::new();
    let Some(rules) = table
        .get_mut("match")
        .and_then(toml_edit::Item::as_table_like_mut)
    else {
        return rewritten;
    };
    for key in ["value_re", "mpn_re"] {
        let text = rules.get(key).and_then(toml_edit::Item::as_str);
        let bare = text.and_then(crate::validation::without_end_anchor);
        if let (Some(written), Some(bare)) = (text, bare.filter(|b| !b.is_empty())) {
            let (written, bare) = (written.to_string(), bare.to_string());
            rules.insert(key, toml_edit::value(bare.clone()));
            rewritten.push((key, written, bare));
        }
    }
    rewritten
}

/// The rating key guide every extraction prompt offers: the keys validation
/// accepts for `kind` ([`crate::validation::rating_guide`]), or all of them for a
/// kind that is not one of ours (nothing to filter by).
fn rating_key_guide(kind: &str) -> String {
    component_kind(kind).map_or_else(
        || crate::schema::Ratings::KEY_GUIDE.to_string(),
        crate::validation::rating_guide,
    )
}

/// The rating keys that apply to the kind, then its rating categories as a
/// checklist. Measured: two tiers both dropped a printed TJ maximum, and one
/// put a per-pin limit where the rated output current belonged.
fn ratings_hint_for_kind(kind: &str) -> String {
    let guide = rating_key_guide(kind);
    let Some(kind) = component_kind(kind) else {
        return guide;
    };
    let expected = crate::validation::expected_ratings(kind);
    if expected.is_empty() {
        return guide;
    }
    let list: Vec<String> = expected
        .iter()
        .map(|(k, what)| format!("{k} ({what})"))
        .collect();
    // A driver or switch is a digital card with no logic; the card decides, so
    // the prompt names the extra category rather than the checklist guessing.
    let output_stage = if kind == crate::schema::ComponentKind::Digital {
        " A part with a power output stage (motor, gate or LED driver, load switch, pack-FET \
         driver) also states max_current_a, its rated continuous or peak output current from \
         the Absolute Maximum Ratings or Electrical Characteristics."
    } else {
        ""
    };
    format!(
        "{guide}\nRATINGS BY CATEGORY: check each of these in turn: {}. Each one the datasheet \
         prints goes in [models.ratings]; for one it does not print, add \
         [[models.source.uncertainty]] status = \"unknown\", parameter = \"<key>\", reason \
         naming the tables you checked. A per-pin limit is never the rated current.{output_stage}",
        list.join("; ")
    )
}

/// Rule 7, from the checker's own rules: a hand copy once capped `kp` at 1.0
/// while the checker (rightly, for power FETs) allowed 1000.
/// The value-plausibility rules follow, from the table the validator holds a
/// draft to ([`crate::plausibility::RULES`]).
fn param_bounds_for_kind(kind: &str) -> String {
    let lines = component_kind(kind).map_or_else(Vec::new, crate::validation::param_bounds);
    let plausibility = crate::plausibility::prompt_guide();
    if lines.is_empty() {
        return format!("   (none for this kind)\n{plausibility}");
    }
    format!("   - {}\n{plausibility}", lines.join("\n   - "))
}

/// [`PIN_NUMBERING`] with the package-split rule and the P7 lead-numbering
/// table the checker holds a draft to ([`crate::validation::PACKAGE_SPLIT_RULE`],
/// [`crate::validation::LEAD_STANDARDS`]).
fn pin_numbering() -> String {
    format!(
        "{PIN_NUMBERING}\n  P4 SPLIT: {}.\n  P7 TABLE (package: standard lead numbering):\n{}",
        crate::validation::PACKAGE_SPLIT_RULE,
        crate::validation::lead_standards_guide()
    )
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
        card: r#"[[models]]
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
[[models.source.pinout]]
package = "SOT-23-5 (DBV)"
pins = { "1" = "OUT", "2" = "V-", "3" = "IN+", "4" = "IN-", "5" = "V+" }
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
basis = "Recommended Operating Conditions, supply voltage definition: VEE < VCC""#,
    },
    FormatExample {
        part: "TLV3201",
        kind: "comparator",
        card: r#"[[models]]
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
[[models.source.pinout]]
package = "SOT-23-5 (DBV)"
pins = { "1" = "OUT", "2" = "V-", "3" = "IN+", "4" = "IN-", "5" = "V+" }
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
basis = "Recommended Operating Conditions, supply voltage definition: VEE < VCC""#,
    },
    FormatExample {
        part: "PCA9306",
        kind: "digital",
        card: r#"[[models]]
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
[[models.source.pinout]]
package = "VSSOP-8 (DCU)"
pins = { "1" = "GND", "2" = "VREF1", "3" = "SCL1", "4" = "SDA1", "5" = "SDA2", "6" = "SCL2", "7" = "VREF2", "8" = "EN" }
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
basis = "Recommended Operating Conditions, VREF1 must not exceed VREF2""#,
    },
];

/// A behavioural-family kind (charger / pmic / balancer) maps to a base
/// `ComponentKind` for the TOML `kind = "..."` line, and triggers the extra
/// `[models.behavioral]` prompt section. Returns `None` for ordinary kinds.
fn behavioral_family_base_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "charger" | "pmic" => Some("vreg"),
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
        return match kind {
            "vreg" => SWITCHER_HINT.to_string(),
            "digital" => SUPPLY_DRAW_HINT.to_string(),
            _ => String::new(),
        };
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
  out_pin = "<role>"                # the regulated output pin role ("sw" for a buck: never call SW 'out')
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

/// How a switching regulator is written. Measured: told only the simple
/// regulator's `in, out, gnd`, a buck draft mapped PH as `sw`, had no pad left
/// for `out`, and failed all three attempts on the missing output pin.
const SWITCHER_HINT: &str = r#"
SWITCHING REGULATOR (buck, boost, buck-boost; a part with an SW/PH/LX switch
node and an inductor): its regulated rail is past the inductor, on no pad of the
IC, so it has NO 'out' pin and no vout/vref/dropout_v params. Map the switch
node 'sw' (BOOT 'boot', VSENSE/FB 'fb', COMP 'comp', SS 'ss', EN 'en') and add:

  [models.behavioral.converter]
  topology = "buck"            # or "boost" / "buck_boost"
  out_pin = "sw"
  in_pin = "in"
  vout_setpoint = <V>          # the fixed output; for an adjustable part the
                               # datasheet's typical-application output (cite
                               # it), a seed for a board with no divider, and
                               # NEVER the reference: vout_setpoint = vref_v
                               # declares a fixed part whose FB senses VOUT
  iout_limit_a = <A>           # rated output current (optional)
  max_duty = <0..1>            # maximum duty cycle row (optional)
  switching_hz = <Hz>          # switching frequency row (optional)
  [models.behavioral.converter.feedback]   # adjustable parts
  pin = "fb"
  vref_v = <V>                 # feedback reference, typ; its min/max as an
                               # interval record for `feedback_reference`

`iq_a` (operating quiescent current) may stay in [models.params]; a `warning`
param names what is not modelled (ripple, loop stability, soft-start, light-load
modes, protection). A linear regulator or reference ignores this block.
"#;

/// How a driver or power IC with no logic outputs is written. Measured: told
/// only "logic, else identity-only", a pack FET driver came back identity-only
/// although its supply current and range were printed.
const SUPPLY_DRAW_HINT: &str = r#"
SUPPLY-DRAW FORM (a driver or power IC whose outputs are not logic levels: a
motor H-bridge, gate driver, LED driver or battery-pack FET driver): when the
function table cannot honestly be [models.logic], do not stop at identity-only
if the datasheet prints the operating supply current. Omit [models.logic] and
write the supply draw, citing the rows:

  [models.params]
  supply_current_a = <A>     # operating-mode supply current, typ
  supply_on_v = <V>          # minimum recommended supply voltage
  warning = "source-bound supply-draw model: <what is executable>; <what is not>"

  [[models.behavioral.laws]]
  name = "supply_draw"
  kind = "current"
  a = "<supply role>"        # e.g. vm, vcc, bat
  b = "gnd"
  expr = "supply_current_a * min(1.0, max(0.0, v_<supply role> / supply_on_v))"

with the [[models.envelope]] supply_range for that supply pin and every pad in
[models.pins]. Identity-only is for a part whose supply current is not printed.
"#;

/// The pin role names the BINDER accepts for a kind, verbatim.
///
/// The binder looks these up by exact string (`roles.get("out")`), so a model
/// that calls the pin "output" passes every other check and then binds OPEN.
/// Validation catches that, but only once the run is over, and a retry costs
/// another three minutes and another bill. Stating the vocabulary up front is
/// the cheap half of the same guarantee.
fn pin_roles_for_kind(kind: &str) -> &'static str {
    match kind {
        "diode" => "anode, cathode; a multi-diode package numbers each diode's own electrode (anode1, cathode2), an unnumbered one is shared (common cathode: anode1, anode2, cathode), and a series pair's shared pad is common",
        "bjt_npn" | "bjt_pnp" => "collector, base, emitter; a dual: collector_q1, base_q1, ..._q2",
        "nmos" | "pmos" => "drain, gate, source",
        "vreg" | "charger" | "pmic" => "in, out, gnd  (and en / adj / fb when the part has them; a fixed-output part's ADJ/GND pin is gnd; a switching regulator's switch node is sw and it has no out pin: see SWITCHING REGULATOR below)",
        "opamp" | "comparator" => {
            "in_plus, in_minus, out, vcc, vee; a dual or quad suffixes each unit's signal roles \
             _a to _d (out_a, in_minus_a, in_plus_a, out_b, in_minus_b, ...), supplies unsuffixed"
        }
        "analog_switch" => {
            "in_out_a, in_out_b (SPST) or com, s0, s1, ctrl (SPDT). ctrl is the select/enable pin \
             whatever the datasheet calls it; s0 is the throw joined to com while ctrl is LOW, s1 \
             while it is HIGH (an active-low EN selecting input 1 makes input 1 s0)"
        }
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
DIGITAL BEHAVIOR (required unless the part takes the SUPPLY-DRAW FORM below or
must be an explicit identity-only abstention): add `[models.logic]`. Its `inputs` and `outputs` arrays contain pin
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
When an output needs several conditions at once (a direction-controlled bus
transceiver with an output enable drives each side only while OE is active AND
DIR selects that side), add `and = [{ pin = "input_role", active = "low" }]`:
every listed condition must be active together with `enable`. For bidirectional
pins, declare each as an output (`a`) plus a sensed input (`a_in`, listed in `inputs`) and
map the input to its output in `[models.logic.sense]` (`a_in = "a"`, never `a = "a_in"`).
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
do not mix corners. `voh` and `vol` are the LIGHT-LOAD levels (the |IOH|/|IOL| of
about 20-100 uA row, often `VCC - 0.2 V`, written as a `[models.levels]` `voh`
table when it follows VCC), never a row printed at a heavy test current such as
IOH = -24 mA: the output is driven as `voh` behind `ro`, so derive
`ro` = |VOH_light - VOH_heavy| / |IOH| from the heaviest row at the same supply
and corner, and keep that row as a characteristic.
State whether each source value is typical or guaranteed and
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
        "diode" => "is, n, rs, fitted to the cited VF-at-IF rows (25 C; typ, else max; a VF \
             curve read at several currents fits n and rs)",
        "bjt_npn" | "bjt_pnp" => "is, bf, nf (no VBE(on) row or curve: an `unknown` record each)",
        // Never the word "is" here: optional_params_for_kind would drop the `is` param.
        "nmos" | "pmos" => {
            "vto, kp. Level-1 square law with w_over_l unset (1): vto keeps the SPICE sign \
             (negative for an enhancement PMOS), kp in A/V^2, and rd/rs are the drain and source \
             ohmic series resistances, not the channel. Small-VDS on-resistance here equals \
             1/(kp*(|VGS| - |vto|)) + rd + rs, so an RDS(on) typ row fixes kp and rd + rs \
             together; the law must give every RDS(on) row, a typ within 10% and a max-only row at \
             or below its max. With rows at two VGS values, solve kp from their difference and \
             split the residue at the higher-VGS row evenly between rd and rs; with one row, take \
             kp from gFS at its ID (kp = gFS^2 / (2*ID)) and put the residue in rd + rs, or, \
             without gFS, derive kp from that row alone with rd = rs = 0. Comment each value \
             `derived from RDS(on) row at VGS=..., ID=..., typ column`"
        }
        "vreg" => {
            "fixed: vout, dropout_v, iq_a; adjustable (ADJ/FB pin): vref (the reference, \
             never the output), dropout_v and the quiescent current param the checker asks \
             for, with no vout. vout: the ordered variant's nominal output, with its \
             output-voltage-accuracy row's min/max if printed; a range spanning the family's \
             selectable outputs is never vout. iq_a: the no-load ground (or supply) current; \
             max_current_a (a [models.ratings] key, never a param): the rated output current, \
             never the absolute-maximum IOUT; dropout_v: the row for this part's own \
             output-voltage option, at the rated output current when several loads are printed. \
             A SWITCHING regulator takes none of these: its behaviour is the converter block \
             described below"
        }
        "opamp" => {
            "gain, and swing_lo/swing_hi (output headroom from each supply pin, max column) or \
             rail_lo/rail_hi (swing limits at a stated test supply, VS minus the swing, never the \
             supply or ground itself); gain is V/V (AOL in dB is 10^(dB/20), V/mV ×1e3, \
             V/µV ×1e6); pole_hz is GBW / gain, slew is in V/us"
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
        "analog_switch" => {
            "ron, roff, vth (no off-state leakage, shutdown-current or off-resistance row at \
             all: an `unknown` record for roff). ron is \
             the typ column at 25 C, as for RDS(on). With on-resistance printed per path, add \
             ron_s0/ron_s1 for the path each leg role names; ron keeps the main or first path"
        }
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
             [models.current_program]). Only a dropout_v or iq_a the conventions below cannot \
             read (no dropout, RDS(on) or supply-current row) is omitted with an `unknown` \
             record for it naming why, its rows staying characteristic records: never a reason \
             for identity-only"
        }
        "balancer" => "(the leak law is in [models.behavioral]; no required numeric params)",
        // Never a file the sandbox lacks: an agent sent to db/README.md asked for it.
        _ => "(no kind-specific required params beyond the format below)",
    }
}

/// The derivation conventions Hauksbee applies to the kind's cited rows
/// ([`crate::conventions`]), so the model cites the row instead of abstaining
/// on a param it does not know how to read, or restating the arithmetic.
const fn conventions_for_kind(kind: &str) -> &'static str {
    match kind.as_bytes() {
        b"diode" => {
            "\nConventions (cite the row; Hauksbee computes the param): a single VF row \
             gives is at the ideal n = 1, rs = 0 (one point fixes no slope); with no V(BR) row, \
             bv is the rated VRRM/VR and ibv the IR row at it."
        }
        b"opamp" => {
            "\nConventions (cite the row; Hauksbee computes the param): a VOH/VOL row \
             printed below 1 V is headroom from the supply, swing_hi/swing_lo."
        }
        b"comparator" => {
            "\nConventions (cite the row; Hauksbee computes the param): out_lo is \
             VOL max, out_hi VOH min (a VOH printed as headroom below 1 V is the test supply \
             minus it); tpd_s is the slower edge at the largest printed overdrive; \
             supply_static_ua the operating-mode supply current."
        }
        b"digital" | b"shift_register" => {
            "\nConventions (cite the row; Hauksbee computes the \
             param): a level printed as a fraction of a supply (0.7 × VDD, 70 % VDDIO) or an \
             offset (VDD − 0.4 V) is that fraction of the supply's printed value; tpd_s is the \
             slower edge (tPLH/tPHL); supply_static_ua the operating-mode supply current."
        }
        b"analog_switch" => {
            "\nConventions (cite the row; Hauksbee computes the param): roff is \
             the off-state leakage's test voltage over its max current (5.5 V / 1 uA = 5.5 MΩ; \
             a load switch with only a shutdown current uses that row); vth is the EN or \
             control threshold row (typ), else the control input's VIH min."
        }
        b"vreg" | b"charger" | b"pmic" => {
            "\nConventions (cite the row; Hauksbee computes the \
             param): iq_a is the supply current while charging (a charger), else the no-load \
             ground current; dropout_v is the dropout row at the rated load, else the pass \
             FET's RDS(on) max times the current limit (a FET in dropout is fully on)."
        }
        _ => "",
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

/// A characteristic row printing no number (a supply-relative limit such as
/// 0.7 VCC) becomes the `unknown` uncertainty record the checker asks for, its
/// text quoted, instead of sending the whole draft back. Whether it moved any.
fn unnumbered_rows_to_unknowns(source: &mut toml_edit::Table) -> bool {
    use toml_edit::{ArrayOfTables, Item, Table};
    let numbered = |r: &Table| ["min", "typ", "max"].iter().any(|k| r.contains_key(k));
    let rows = source
        .get("characteristic")
        .and_then(Item::as_array_of_tables);
    let bare: Vec<Table> = rows.map_or_else(Vec::new, |rows| {
        rows.iter().filter(|r| !numbered(r)).cloned().collect()
    });
    if bare.is_empty() {
        return false;
    }
    let records = source.entry("uncertainty");
    let Some(records) = records
        .or_insert(ArrayOfTables::new().into())
        .as_array_of_tables_mut()
    else {
        return false;
    };
    let text = |r: &Table, k: &str| {
        r.get(k)
            .and_then(Item::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    for row in &bare {
        let reason = format!("{} ({})", text(row, "conditions"), text(row, "basis"));
        let parameter = text(row, "parameter");
        let fields = [
            ("status", "unknown"),
            ("parameter", &parameter),
            ("reason", &reason),
        ];
        records.push(Table::from_iter(fields));
    }
    if let Some(Item::ArrayOfTables(rows)) = source.get_mut("characteristic") {
        rows.retain(numbered);
    }
    true
}

/// A non-current row names `pins` but no `rating`: only a current row tagged with
/// one bounds a path, so the checker rejects the names. They go; whether any did.
fn drop_pins_from_untagged_rows(source: &mut toml_edit::Table) -> bool {
    use toml_edit::Item;
    let rows = source
        .get_mut("characteristic")
        .and_then(Item::as_array_of_tables_mut);
    let not_current = |r: &toml_edit::Table| {
        let unit = r.get("unit").and_then(Item::as_str);
        !matches!(unit, None | Some("A" | "mA" | "uA" | "nA" | "pA"))
            && !r.contains_key("rating")
            && r.contains_key("pins")
    };
    let mut dropped = false;
    for row in rows
        .into_iter()
        .flat_map(|rows| rows.iter_mut())
        .filter(|r| not_current(r))
    {
        dropped |= row.remove("pins").is_some();
    }
    dropped
}

/// A param written as a printed row its name cannot read (a nominal printing
/// only a max, a unit it does not convert) moves to a characteristic record of
/// the same name, as the checker prescribes. Whether it moved any of `keys`.
fn unreadable_params_to_rows(entry: &mut toml_edit::Table, keys: &[&str]) -> bool {
    use toml_edit::{ArrayOfTables, Item, Table, Value};
    let mut moved = false;
    for key in keys {
        let found = entry.get("params").and_then(Item::as_table_like);
        let mut row = match found.and_then(|p| p.get(key)) {
            Some(Item::Value(Value::InlineTable(t))) => t.clone().into_table(),
            Some(Item::Table(t)) => t.clone(),
            _ => continue,
        };
        row.insert("parameter", toml_edit::value(*key));
        let source = entry.entry("source").or_insert(Item::Table(Table::new()));
        let rows = source.as_table_mut().map(|t| t.entry("characteristic"));
        let rows = rows.and_then(|e| {
            e.or_insert(ArrayOfTables::new().into())
                .as_array_of_tables_mut()
        });
        let Some(rows) = rows else { continue };
        rows.push(row);
        if let Some(params) = entry.get_mut("params").and_then(Item::as_table_like_mut) {
            params.remove(key);
        }
        moved = true;
    }
    moved
}

impl Reply<'_> {
    /// The TOML table the answer starts with (empty for a bare word).
    const fn table(self) -> &'static str {
        match self {
            Reply::Model { .. } => "[[models]]",
            Reply::Sensor { .. } => "[sensor]",
            Reply::Word => "",
        }
    }

    /// The reply with the conservative unknown model-error record added where
    /// one is missing (never replaced): it only qualifies results; a retry costs a run.
    fn complete(self, raw: String) -> String {
        let done = self.complete_with(raw.clone(), true);
        // An identity-only card converted to a model that still fails its check
        // stays as written, to be refused by the abstention rule as before.
        if done != raw && self.check(&done).is_err() {
            return self.complete_with(raw, false);
        }
        done
    }

    /// [`Self::complete`], converting an identity-only card its own rows make
    /// needless into the model they give when `convert`.
    #[allow(clippy::too_many_lines)] // one pass over each entry's table
    fn complete_with(self, raw: String, convert: bool) -> String {
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
        let (mut inserted, mut repaired) = (false, false);
        let tables: Vec<&mut Table> = match doc.get_mut(path) {
            Some(Item::ArrayOfTables(a)) => a.iter_mut().collect(),
            t => t.and_then(Item::as_table_mut).into_iter().collect(),
        };
        for table in tables {
            let source = table.entry("source").or_insert(Item::Table(blank.clone()));
            // A source table another part of the draft opened (a pinout) still gets its tier.
            if let Some(t) = source.as_table_mut().filter(|t| !t.contains_key("tier")) {
                for (k, v) in &blank {
                    if !t.contains_key(k) {
                        t.insert(k, v.clone());
                        repaired = true;
                    }
                }
            }
            if source
                .as_table_mut()
                .is_some_and(drop_pins_from_untagged_rows)
            {
                say!("[model-extract] rows naming pins without a rating: pins dropped");
                repaired = true;
            }
            if source
                .as_table_mut()
                .is_some_and(unnumbered_rows_to_unknowns)
            {
                say!("[model-extract] rows printing no number kept as unknown uncertainty records");
                repaired = true;
            }
            let records = source.as_table_mut().map(|t| t.entry("uncertainty"));
            let records = records.map(|e| e.or_insert(ArrayOfTables::new().into()));
            match records {
                Some(Item::ArrayOfTables(a)) if !a.iter().any(is_model) => {
                    a.push(record.clone().collect::<Table>());
                }
                _ => continue,
            }
            inserted = true;
        }
        if inserted {
            say!("[model-extract] warning: model-error record missing; inserted the conservative unknown record");
        }
        inserted |= repaired;
        // A param omitted where a cited row fixes it is filled, with its working,
        // and a diode law off its cited VF rows is replaced by their fit.
        let db =
            toml::from_str::<crate::schema::DbFile>(&doc.to_string()).map_or(vec![], |d| d.models);
        let models = doc.get_mut(path).and_then(Item::as_array_of_tables_mut);
        let tables = models.into_iter().flat_map(ArrayOfTables::iter_mut);
        for (original, table) in db.iter().zip(tables) {
            // A match pattern anchored at the end binds only the bare name; a variant's
            // pattern is its own to anchor.
            if original.variant.is_none() {
                for (key, written, bare) in unanchor_match(table) {
                    say!(
                        "[model-extract] {}: {key} {written} ends with an end anchor; \
                         rewritten to {bare}: {}",
                        original.id,
                        crate::validation::MATCH_PATTERN_RULE
                    );
                    inserted = true;
                }
            }
            let rows = original.params.rows();
            let unread: Vec<&str> = rows
                .filter(|(k, r)| crate::param_names::row_value(k, r).is_err())
                .map(|(k, _)| k)
                .collect();
            if unreadable_params_to_rows(table, &unread) {
                say!(
                    "[model-extract] {}: params {} cannot read their rows; kept as characteristics",
                    original.id,
                    unread.join(", ")
                );
                inserted = true;
            }
            // An identity-only abstention the card's own cited rows make needless
            // (they fix every required param) becomes the model those rows give:
            // the params are derived below rather than the reply refused.
            let mut converted = original.clone();
            let needless = convert
                && original.params.get_bool("identity_only") == Some(true)
                && crate::validation::needless_identity_only(original);
            if needless {
                for key in ["identity_only", "warning", "unlocked_by"] {
                    converted.params.0.remove(key);
                    let params = table.get_mut("params").and_then(Item::as_table_like_mut);
                    params.map(|p| p.remove(key));
                }
                say!("[model-extract] {}: identity-only abstention refused by its own rows; writing the model they give", original.id);
                inserted = true;
            }
            let entry = &converted;
            // A rating's scalar is its tagged row at the standard condition, which
            // code picks from the rows rather than the model from the table.
            // A row the model left untagged but its own text names (an ID, PD or
            // θJA row) counts, read in memory, for a key it tagged no row of.
            let (tagged, implied) = crate::derating::with_implied_tags(entry);
            let rows = tagged.source.iter().flat_map(|s| &s.characteristic);
            let mut keys: Vec<&str> = rows.filter_map(|r| r.rating.as_deref()).collect();
            keys.sort_unstable();
            keys.dedup();
            let ratings = toml::Table::try_from(&entry.ratings).unwrap_or_default();
            for key in keys {
                let Some(v) = crate::derating::standard_rating(&tagged, key) else {
                    continue;
                };
                let had = ratings.get(key).and_then(toml::Value::as_float);
                if had.is_some_and(|h| (h - v).abs() <= 1e-9 * v.abs()) {
                    continue;
                }
                // A key read from a row the model left untagged only fills a scalar it omits.
                if implied.contains(&key) && had.is_some() {
                    continue;
                }
                let ratings = table.entry("ratings").or_insert(Item::Table(Table::new()));
                if let Some(ratings) = ratings.as_table_like_mut() {
                    say!("[model-extract] {}: ratings.{key} = {v}, its tagged row at the standard condition (was {had:?})", entry.id);
                    ratings.insert(key, toml_edit::value(v));
                    inserted = true;
                }
            }
            if let Some((v, how)) = crate::conventions::junction_limit(entry) {
                let ratings = table.entry("ratings").or_insert(Item::Table(Table::new()));
                if let Some(ratings) = ratings.as_table_like_mut() {
                    say!(
                        "[model-extract] {}: ratings.max_junction_temp_c = {v}, {how}",
                        entry.id
                    );
                    ratings.insert("max_junction_temp_c", toml_edit::value(v));
                    inserted = true;
                }
            }
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
                let mut value = text.parse().unwrap_or_else(|_| toml_edit::Value::from(v));
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

    /// The lint warnings a passing reply still draws, the ones `models lint` prints
    /// for its cited rows ([`crate::param_names::source_warnings`]): a drafter that
    /// is shown them can fix them, as it is shown a validation error.
    fn warnings(self, raw: &str) -> Vec<String> {
        let Reply::Model { part, kind } = self else {
            return Vec::new();
        };
        parse_and_validate_all(raw, part, kind)
            .map(|all| {
                let mut out = crate::param_names::source_warnings(&all[0]);
                // The fields a board check reads that a draft can leave out
                // (enable behaviour, supply capacitors, open-drain pads, bias
                // rows, pin roles, crystal caps): card-side, so no datasheet text.
                for (i, entry) in all.iter().enumerate() {
                    out.extend(crate::card_fields::warnings(entry).into_iter().map(|w| {
                        if i == 0 {
                            w
                        } else {
                            format!("[[models]] '{}': {w}", entry.id)
                        }
                    }));
                }
                // The binding, pin-table, coverage, scope and provenance checks
                // the agent's `ds check` runs, against this run's sandbox facts.
                out.extend(
                    crate::card_coverage::current_reply_findings(&all)
                        .into_iter()
                        .filter(|f| f.severity == crate::card_coverage::Severity::Warn)
                        .map(|f| format!("{}: {}", f.check, f.message)),
                );
                out
            })
            .unwrap_or_default()
    }

    /// The one validator of a drafted card, the same for the post-session loop
    /// ([`validated_attempts`]) and the in-session `ds check` ([`check_draft`]), so
    /// the agent is shown every refusal and warning while it can still fix it: the
    /// refusal `raw` draws, else (when `lint`) the warnings a passing card still draws.
    /// `raw` is the completed reply ([`Self::complete`]).
    fn assess(self, raw: &str, lint: bool) -> Result<Vec<String>> {
        self.check(raw)?;
        Ok(if lint { self.warnings(raw) } else { Vec::new() })
    }

    fn check(self, raw: &str) -> Result<()> {
        match self {
            Reply::Model { part, kind } => parse_and_validate_reply(raw, part, kind).map(drop),
            Reply::Sensor { part, kind } => validate_sensor_reply(raw, part, kind).map(drop),
            Reply::Word if raw.trim().is_empty() => bail!("the backend returned nothing"),
            Reply::Word => check_kind_reply(raw),
        }
    }
}

/// The canned reply of `HAUKSBEE_EXTRACT_MOCK_REPLY`, validated like a real one.
fn mock_reply(path: &str, reply: Reply<'_>) -> Result<(String, Option<TimedOut>)> {
    // Test / offline hook: HAUKSBEE_EXTRACT_MOCK_REPLY points to a file holding
    // a canned backend reply. This exercises the full parse + validate path
    // with no codex and no network, and the reply is validated exactly like a
    // real one so the hook cannot smuggle garbage past the pipeline.
    let canned =
        std::fs::read_to_string(path).with_context(|| format!("reading mock reply from {path}"))?;
    let raw = reply.complete(extract_toml_block(&canned, reply.table()));
    if let Err(e) = reply.check(&raw) {
        return Err(Rejected {
            tool: "mock reply".into(),
            attempts: 1,
            error: format!("{e:#}"),
            draft: raw,
            kept: None,
        }
        .into());
    }
    Ok((raw, None))
}

/// The validated reply, plus the timeout when it is a timed-out run's last
/// lint-clean draft rather than a finished answer.
fn call_backend(
    prompt: &Prompt,
    args: &Args,
    reply: Reply<'_>,
) -> Result<(String, Option<TimedOut>)> {
    if let Ok(path) = std::env::var("HAUKSBEE_EXTRACT_MOCK_REPLY") {
        return mock_reply(&path, reply);
    }

    // Flags, then environment, then the config file, then defaults: one
    // resolver for every surface, so the CLI and the web page cannot disagree
    // about what is about to read the datasheet.
    let resolved = resolve_settings(args)?;
    let chosen = resolved.backend;
    // The "typical" time shown is this model's own, not another's.
    progress::model(resolved.model());
    say!(
        "[model-extract] backend: {} ({})",
        resolved.summary(),
        resolved.backend_source.describe()
    );
    // The API backend has no local-file access, so a prompt carrying the
    // no-text sentinel would ship it nothing to read. Refuse before sending
    // rather than spend the call.
    if chosen == Backend::Api && !api_source_available(&prompt.full()) {
        bail!(
            "the API backend cannot read the local PDF and pdftotext is unavailable. \
             Install Poppler/pdftotext, then retry; nothing was sent."
        );
    }

    if chosen == Backend::Api {
        let request = ApiRequest::prepare(&resolved.api)?;
        let raw = validated_attempts(resolved.retries, prompt, reply, ("api", false), |p| {
            let (text, usage) = request.send_with_usage(&p.full())?;
            let metered = crate::extract_usage::Billing::Metered { fast: false };
            crate::extract_usage::record("api", &request.model, metered, usage);
            Ok(extract_toml_block(&text, reply.table()))
        });
        return raw.map(|raw| (stamp_doubts(&raw, reply, None), None));
    }
    require_tool(&resolved)?;

    // An agent backend reads the sandboxed PDF, text dump and page renders
    // itself, so the embedded text comes back out of its instruction.
    // The sandbox is a scratch copy, never the user's own directory. See
    // `Workspace`: the agent runs full-auto, so what it can reach is the whole
    // of the security story.
    progress::phase("rendering the datasheet pages");
    let mut ws = prepare_workspace_in(&args.pdf, &std::env::temp_dir())?;
    ws.checker = args.checker.as_deref().and_then(|c| c.canonicalize().ok());
    ws.trace = crate::extract_trace::begin(args.out_dir.as_deref(), &args.part, args.trace);
    if let Reply::Model { part, kind } = reply {
        // Worked examples and the documents the datasheet defers to, in the
        // sandbox before the agent starts and in the trace after it.
        let mut notes = fetch_companions(&ws, &resolved, part, kind);
        crate::card_templates::write_to(ws.path(), &args.templates);
        notes.extend(crate::card_templates::provenance_line(&args.templates));
        if let (Some(trace), false) = (ws.trace.as_ref(), notes.is_empty()) {
            let _ = std::fs::write(trace.dir().join("rounding.txt"), notes.join("\n") + "\n");
        }
    }
    if let Reply::Model { part, kind } | Reply::Sensor { part, kind } = reply {
        crate::ds_toolkit::write_run_files(ws.path(), part, kind, args.board.as_ref());
    }
    say!(
        "[model-extract] sandbox {} with {} page render(s)",
        ws.path().display(),
        ws.pages.len()
    );
    // The reply's shape, enforced by a backend that can (see `schema_flag`).
    let schema = if reply.table().is_empty() {
        identify_schema()
    } else {
        envelope_schema()
    };
    std::fs::write(ws.path().join(REPLY_SCHEMA), schema.to_string())?;
    let prompts = drafting_prompts(prompt, &mut ws, reply, chosen);
    // The escalation ladder: each tier a model, the next tried only when this
    // one fails, times out, doubts itself or leaves its pin map unresolved. A
    // kind identification is one cheap call on the first model.
    let ladder = if reply.table().is_empty() {
        vec![resolved.model().to_string()]
    } else {
        resolved.ladder()
    };
    // A subscription run is reported as its share of the plan's limits, read
    // before and after; dollars only for a metered (API-key) run.
    let command = resolved.agent(chosen).expect("agent").command.clone();
    let billing = billing_for(&resolved);
    // `run` reads the plan again when the extraction ends.
    if billing == crate::extract_usage::Billing::Plan {
        crate::extract_plan::begin(chosen.name(), &command);
    }
    // The deterministic binding, pin-table and coverage checks read the same
    // sandbox files the agent's `ds` helpers print.
    let facts = crate::ds_toolkit::facts_from_dir(ws.path());
    crate::card_coverage::with_facts(facts, || {
        run_ladder(&resolved, &prompts, reply, &ws, &ladder, billing)
    })
}

/// `raw`, a finished card, with what is left unconfirmed on it stamped as
/// `[[models.source.doubt]]` rows ([`crate::card_trust`]): the trust problems the
/// drafter did not fix, a pad map that disagrees with the datasheet's column, and
/// the drafter's own `low` confidence (`confidence` is its level and reason). A
/// finding that rests only on a doubted field of this card is then a note to check
/// the card, not a board fault.
fn stamp_doubts(raw: &str, reply: Reply<'_>, confidence: Option<(String, String)>) -> String {
    let Reply::Model { part, kind } = reply else {
        return raw.to_string();
    };
    let Ok(entries) = parse_and_validate_all(raw, part, kind) else {
        return raw.to_string();
    };
    let mut doubts = crate::card_coverage::current_doubts(&entries);
    if let Some((level, why)) = confidence.filter(|(level, _)| level == "low") {
        let reason = format!("the drafter reported {level} confidence: {why}");
        for (_, rows) in &mut doubts {
            rows.push(crate::schema::Doubt {
                area: crate::schema::DoubtArea::Card,
                reason: reason.clone(),
            });
        }
    }
    for (id, rows) in &doubts {
        for d in rows {
            say!(
                "[model-extract] {id}: left unconfirmed on the card ({:?}): {}",
                d.area,
                d.reason
            );
        }
    }
    crate::card_trust::stamp(raw, &doubts)
}

/// The instruction an agent run is given, in its single and staged shapes: the
/// prompt with the verification clause (or, for naming a kind, its own short
/// one) added. Its `base` depends on the kind, the checker and the backend
/// alone, never on the part, so a base can be warmed before any part is read.
fn drafting_prompts(
    prompt: &Prompt,
    ws: &mut Workspace,
    reply: Reply<'_>,
    chosen: Backend,
) -> Prompts {
    let agent_prompt = prompt.map_tail(prompt_for_agent_backend);
    // Naming the kind is one word read off the front page, whose text is in
    // the prompt. Measured: given the drafting clause and page images, the
    // agent read the whole datasheet to answer it (430k to 790k input tokens,
    // over half of a part's total, and two idle kills).
    // Each clause is its fixed rules (the base, the same for every part of this
    // kind) and the pages and files of this part (the tail).
    let (clause, staged_clause) = if reply.table().is_empty() {
        ws.pages.clear();
        let same = (IDENTIFY_CLAUSE.to_string(), String::new());
        (same.clone(), same)
    } else {
        // A staged tier's drafting agents read pages by number and its fix agent
        // edits a draft in place, so the clause it gets inlines no page text.
        (
            verification_parts(ws, reply.table(), true),
            verification_parts(ws, reply.table(), false),
        )
    };
    let agy_note = if chosen == Backend::Agy {
        agy_tool_note(ws)
    } else {
        String::new()
    };
    let join = |(rules, here): (String, String)| {
        let tail = if here.is_empty() {
            String::new()
        } else {
            format!("\n\n{here}")
        };
        agent_prompt
            .with_base(&format!("\n\n{rules}{agy_note}"))
            .then(&tail)
    };
    Prompts {
        single: join(clause),
        staged: join(staged_clause),
    }
}

/// A tier's prompt with the words every part of a ladder (or a staged run)
/// shares added to its base.
fn tier_base(prompt: Prompt, staged: bool, laddered: bool) -> Prompt {
    if laddered || staged {
        // The same words for every part of the run: part of the shared base.
        prompt.with_base(harness::CONFIDENCE_CLAUSE)
    } else {
        prompt
    }
}

/// Warm ahead of time the shared codex base a call of `reply`'s kind will fork
/// from, so the first part to need it does not wait for it. The prompt is built
/// by the same code as the real call's, so the base is the same bytes and the
/// real call finds it warm. Only a codex run on its first tier with the single
/// agent forks; anything else is left alone. Returns whether a base is warm.
fn warm_ahead(args: &Args, prompt: &Prompt, reply: Reply<'_>) -> bool {
    if std::env::var_os("HAUKSBEE_EXTRACT_MOCK_REPLY").is_some() {
        return false;
    }
    let Ok(resolved) = resolve_settings(args) else {
        return false;
    };
    if resolved.backend != Backend::Codex {
        return false;
    }
    let naming = reply.table().is_empty();
    let ladder = if naming {
        vec![resolved.model().to_string()]
    } else {
        resolved.ladder()
    };
    let model = &ladder[0];
    let staged = !naming && extract_config::context_budget(&ProcessHost, model) > 0;
    if staged {
        return false;
    }
    let tier = resolved.with_model(model);
    let Ok(mut ws) = Workspace::scratch() else {
        return false;
    };
    ws.checker = args.checker.as_deref().and_then(|c| c.canonicalize().ok());
    let schema = if naming {
        identify_schema()
    } else {
        envelope_schema()
    };
    if std::fs::write(ws.path().join(REPLY_SCHEMA), schema.to_string()).is_err() {
        return false;
    }
    let prompts = drafting_prompts(prompt, &mut ws, reply, resolved.backend);
    let wanted = tier_base(prompts.single, staged, ladder.len() > 1);
    fork_base(&tier, &wanted, &ws, false).is_some()
}

/// Warm the shared base that naming a part's kind forks from (one base for
/// every part, since the instruction does not depend on the part).
#[must_use]
pub fn warm_identify_base(template: &Args) -> bool {
    warm_ahead(template, &identify_parts("", ""), Reply::Word)
}

/// Warm the shared base that drafting a card of `kind` forks from; `part` picks
/// which worked examples the instruction carries (it has two shapes).
#[must_use]
pub fn warm_kind_base(template: &Args, part: &str, kind: &str) -> bool {
    if is_sensor_kind(kind) || component_kind(kind).is_none() {
        return false;
    }
    let templates = crate::card_templates::select(part, kind, crate::card_templates::LIMIT);
    warm_ahead(
        template,
        &build_prompt_parts(part, kind, "", &templates),
        Reply::Model { part, kind },
    )
}

/// Fetch the documents the datasheet defers the kind checklist's facts to
/// into the sandbox ([`crate::companion_docs`]); the lines to trace.
///
/// Off when the backend cannot search the web, when the kind is unknown, or
/// when `HAUKSBEE_EXTRACT_NO_COMPANIONS` is set.
fn fetch_companions(ws: &Workspace, resolved: &Resolved, part: &str, kind: &str) -> Vec<String> {
    let off = std::env::var("HAUKSBEE_EXTRACT_NO_COMPANIONS")
        .is_ok_and(|v| !v.trim().is_empty() && v.trim() != "0");
    if off || !resolved.backend.can_search() {
        return Vec::new();
    }
    let Some(kind) = crate::ds_toolkit::kind_from_name(kind) else {
        return Vec::new();
    };
    let facts = crate::ds_toolkit::facts_from_dir(ws.path());
    let needs = crate::card_coverage::kind_needs(kind, &facts.pages.join("\n"), &facts.pin_columns);
    let searcher = crate::datasheet_discovery::AgentSearcher {
        resolved: resolved.clone(),
    };
    let notes = crate::companion_docs::prepare(
        ws.path(),
        part,
        &facts.pages,
        &needs,
        &crate::datasheet_fetch::CurlHttp::default(),
        &searcher,
        &|pdf| {
            extract_pdf_pages_text(pdf)
                .map(|pages| pages.into_iter().map(|p| p.text).collect())
                .unwrap_or_default()
        },
    );
    for n in &notes {
        say!("[model-extract] companion: {n}");
    }
    notes
}

/// How the active backend bills a run: a plan's share, metered dollars, or unknown.
pub(crate) fn billing_for(resolved: &Resolved) -> crate::extract_usage::Billing {
    let Some(a) = resolved.agent(resolved.backend) else {
        return crate::extract_usage::Billing::Unknown;
    };
    match resolved.backend {
        Backend::Codex => crate::extract_usage::codex_billing(&a.command, &a.extra_args),
        Backend::ClaudeCode => crate::extract_plan::claude_billing(&a.command),
        _ => crate::extract_usage::Billing::Unknown,
    }
}

/// One web-searching agent run in a scratch sandbox (the datasheet discovery step).
///
/// The active agent backend with its search tool, its usage recorded, returning
/// the agent's final message. Files and shell are confined exactly as for an
/// extraction; the sandbox holds only the prompt.
///
/// # Errors
///
/// Fails for the API backend (no search), when the agent CLI is missing, and
/// when the run fails or times out.
pub fn run_agent_searching(resolved: &Resolved, prompt: &str) -> Result<String> {
    if !resolved.backend.can_search() {
        bail!(
            "the {} backend cannot search the web; use one of: {}",
            resolved.backend.label(),
            Backend::searchers().join(", ")
        );
    }
    require_tool(resolved)?;
    let ws = Workspace::scratch()?;
    run_and_record_with(
        resolved,
        &Prompt::from(prompt),
        &ws,
        0,
        billing_for(resolved),
        true,
    )
}

/// The escalation ladder over `ladder`'s models in one sandbox (see `call_backend`).
fn run_ladder(
    resolved: &Resolved,
    prompts: &Prompts,
    reply: Reply<'_>,
    ws: &Workspace,
    ladder: &[String],
    billing: crate::extract_usage::Billing,
) -> Result<(String, Option<TimedOut>)> {
    let chosen = resolved.backend;
    let (tiers, mut why, mut fallback) = (ladder.len(), Vec::<String>::new(), None);
    for (i, model) in ladder.iter().enumerate() {
        let tier = resolved.with_model(model);
        let staged = !reply.table().is_empty();
        let budget = extract_config::context_budget(&ProcessHost, model) * u64::from(staged);
        let harness_name = if budget > 0 { "staged" } else { "single" };
        progress::event(
            "tier",
            serde_json::json!({"model": model, "tier": i + 1, "tiers": tiers, "harness": harness_name, "budget": budget}),
        );
        if tiers > 1 {
            say!(
                "[model-extract] tier {} of {tiers}: {model} ({harness_name})",
                i + 1
            );
        }
        let last = i + 1 == tiers;
        let ran = run_tier(
            &tier,
            (prompts, billing),
            reply,
            ws,
            budget,
            why.last(),
            tiers > 1,
        );
        let done = |(raw, t): (String, Option<TimedOut>), n: usize, why: &[String]| {
            // A kind identification answers one word: no card, no provenance line.
            if reply.table().is_empty() {
                return Ok((raw, t));
            }
            let raw = harness::finish(
                &raw,
                ws.path(),
                (chosen.name(), &ladder[n]),
                (n + 1, tiers),
                why,
            );
            let raw = stamp_doubts(&raw, reply, Some(harness::confidence(ws.path())));
            Ok((raw, t))
        };
        let reason = match ran {
            Ok(answer) if last => return done(answer, i, &why),
            Ok(answer) => {
                let pins = match reply {
                    Reply::Model { part, kind } => parse_and_validate_reply(&answer.0, part, kind)
                        .ok()
                        .and_then(|e| crate::validation::unknown_pads_warning(&e)),
                    _ => None,
                };
                let timed = answer
                    .1
                    .as_ref()
                    .map(|t| format!("{t}; its last lint-clean draft is the fallback"));
                match timed.or_else(|| harness::doubt(ws.path(), pins)) {
                    None => return done(answer, i, &why),
                    Some(doubt) => {
                        fallback = Some((answer, i, why.clone()));
                        doubt
                    }
                }
            }
            Err(e) if last => {
                return match fallback {
                    Some((answer, n, earlier)) => {
                        let mut why = earlier;
                        why.push(format!(
                            "{} failed too ({})",
                            ladder[i],
                            harness::reason(&e)
                        ));
                        done(answer, n, &why)
                    }
                    None => Err(e),
                };
            }
            Err(e) => harness::reason(&e),
        };
        let next = &ladder[i + 1];
        say!("[model-extract] escalating from {model} to {next}: {reason}");
        progress::event(
            "escalate",
            serde_json::json!({"from": model, "to": next, "reason": reason}),
        );
        why.push(reason);
    }
    unreachable!("the last tier returns")
}

/// The drafting instruction in its two shapes: the single agent's, which
/// inlines the selected pages' text, and a staged tier's, which does not (its
/// section agents and its fix agent open pages by number, and a fix edits a
/// draft in place: measured, inlined text is re-sent on every turn of a fix
/// agent that mostly does not read it).
struct Prompts {
    single: Prompt,
    staged: Prompt,
}

impl Prompts {
    fn for_harness(&self, staged: bool) -> Prompt {
        let prompt = if staged { &self.staged } else { &self.single };
        prompt.clone()
    }
}

/// A staged tier's first pass. A model card is first drafted by four section
/// agents, merged with no model call: a clean merge is returned for the validator,
/// a flawed one is left for a fix agent told what the merge found. A sensor spec
/// is first located: the pin map and key tables are noted for the drafting agent.
fn first_pass(
    resolved: &Resolved,
    ws: &Workspace,
    reply: Reply<'_>,
    budget: u64,
    billing: crate::extract_usage::Billing,
) -> Result<Option<String>> {
    match reply {
        Reply::Model { part, kind } => {
            progress::phase("drafting the card's sections in parallel");
            let merged = sections::draft(resolved, ws, (part, kind), budget, billing)?;
            let problems = merged.problems();
            std::fs::write(ws.answer_path(), &merged.card)?;
            std::fs::write(
                ws.path().join(harness::NOTES),
                format!("Section agents ran.{problems}\n"),
            )?;
            Ok(problems.is_empty().then_some(merged.card))
        }
        Reply::Sensor { part, kind } => {
            progress::phase("locating the pin map and key tables");
            progress::event(
                "stage",
                serde_json::json!({"stage": "locate", "attempt": 1, "attempts": 1}),
            );
            let locate = harness::locate_parts(part, kind, required_params_for_kind(kind));
            run_and_record(
                resolved,
                &locate.then(&harness::page_list(ws.path())),
                ws,
                budget,
                billing,
            )?;
            if !ws.path().join(harness::NOTES).is_file() {
                bail!("the locating agent wrote no {}", harness::NOTES);
            }
            Ok(None)
        }
        Reply::Word => Ok(None),
    }
}

/// One model's attempt at the extraction in the shared sandbox: the staged
/// harness when `budget` is set (locate, then draft and fix, each a fresh agent
/// bounded to `budget` tokens of context), else one agent with the full prompt.
fn run_tier(
    resolved: &Resolved,
    (prompts, billing): (&Prompts, crate::extract_usage::Billing),
    reply: Reply<'_>,
    ws: &Workspace,
    budget: u64,
    escalated_because: Option<&String>,
    laddered: bool,
) -> Result<(String, Option<TimedOut>)> {
    let chosen = resolved.backend;
    let tool = resolved.agent(chosen).expect("agent").command.as_str();
    let staged = budget > 0;
    let _ = std::fs::remove_file(ws.path().join(harness::CONFIDENCE));
    let run = |p: &Prompt| run_and_record(resolved, p, ws, budget, billing);
    let mut merged_draft = if staged && !ws.path().join(harness::NOTES).is_file() {
        first_pass(resolved, ws, reply, budget, billing)?
    } else {
        None
    };
    let mut prompt = prompts.for_harness(staged);
    if !reply.table().is_empty() {
        prompt = prompt.then(&harness::handoff(
            ws.path(),
            staged,
            escalated_because.map(String::as_str),
        ));
    }
    prompt = tier_base(prompt, staged, laddered);
    let mut timed_out = None;
    // The last card-shaped draft, so a retry edits it instead of a fresh agent
    // re-reading the datasheet from page one: measured, an unstaged retry cost
    // as many tokens as the first draft (555k) to fix one missing pin.
    let mut last_draft = None::<String>;
    let table = reply.table();
    let edit_in_place = !table.is_empty();
    harness::forget_seen_warnings(ws.path());
    let raw = validated_attempts_seeing(
        resolved.retries,
        &prompt,
        reply,
        (tool, edit_in_place),
        |p| {
            if let Some(draft) = merged_draft.take() {
                last_draft = Some(draft.clone());
                return Ok(draft);
            }
            let answer = ws.answer_path();
            // A fix agent finds the failed draft where it will edit it, and
            // `previous.toml` as a copy; a first attempt starts clean.
            let previous = last_draft
                .take()
                .filter(|d| !table.is_empty() && d.contains(table));
            if let Some(draft) = &previous {
                if !answer.is_file() {
                    let _ = std::fs::write(&answer, draft);
                }
                let _ = std::fs::copy(&answer, ws.path().join(harness::PREVIOUS));
            } else if staged && answer.is_file() {
                let _ = std::fs::copy(&answer, ws.path().join(harness::PREVIOUS));
            } else {
                let _ = std::fs::remove_file(&answer);
            }
            let ran = run(p);
            let stdout = match ran {
                // A killed run keeps its draft only if it passes the finished-answer check.
                Err(e) if e.is::<TimedOut>() && !reply.table().is_empty() => {
                    let draft = std::fs::read_to_string(ws.answer_path()).unwrap_or_default();
                    let draft = extract_toml_block(&draft, reply.table());
                    let t = e.downcast::<TimedOut>()?;
                    let checked = reply.check(&reply.complete(draft.clone()));
                    checked.map_err(|_| t.clone())?;
                    timed_out = Some(t);
                    return Ok(draft);
                }
                // A staged agent stopped at its budget after writing model.toml has
                // that draft checked like any other: a failure goes to a fix agent
                // that edits it, instead of a fresh agent redoing the reading.
                Err(e) if staged && e.is::<harness::Stopped>() => {
                    let draft = std::fs::read_to_string(ws.answer_path()).unwrap_or_default();
                    if reply.table().is_empty() || draft.trim().is_empty() {
                        return Err(e);
                    }
                    say!("[model-extract] {e}; checking the draft it wrote");
                    return Ok(extract_toml_block(&draft, reply.table()));
                }
                outcome => outcome?,
            };
            // Prefer the file we asked for. Falling back to stdout keeps a model
            // that answered in prose from failing outright, but the file is the
            // reliable path: stdout also carries the agent's narration, and one
            // that says "here is the TOML" twice yields two candidates.
            let text = std::fs::read_to_string(ws.answer_path())
                .ok()
                .filter(|t| !t.trim().is_empty())
                .unwrap_or(stdout);
            let draft = extract_toml_block(&text, table);
            last_draft = Some(draft.clone());
            Ok(draft)
        },
        || harness::seen_warnings(ws.path()),
    );
    raw.map(|raw| (raw, timed_out))
}

/// One agent run, its usage recorded, returning the agent's final prose: codex
/// and Claude Code stream JSONL events, so theirs is `last-message.txt` and the
/// `result` event's text respectively.
fn run_and_record(
    resolved: &Resolved,
    prompt: &Prompt,
    ws: &Workspace,
    budget: u64,
    billing: crate::extract_usage::Billing,
) -> Result<String> {
    run_and_record_with(resolved, prompt, ws, budget, billing, false)
}

/// [`run_and_record`], optionally with the agent's web search tool.
fn run_and_record_with(
    resolved: &Resolved,
    prompt: &Prompt,
    ws: &Workspace,
    budget: u64,
    billing: crate::extract_usage::Billing,
    web: bool,
) -> Result<String> {
    use crate::extract_usage as usage;
    let (chosen, model) = (resolved.backend, resolved.model());
    let ran = with_backoff(&BACKOFF, || {
        run_agent_with(resolved, prompt, ws, budget, web)
    });
    let stdout = ran.as_ref().map(String::as_str).unwrap_or_default();
    let said = match chosen {
        Backend::ClaudeCode => {
            let result = harness::claude_result(stdout);
            let per_model = result
                .as_ref()
                .map(usage::from_claude_result)
                .unwrap_or_default();
            if per_model.is_empty() {
                usage::record(chosen.name(), model, billing, None);
            }
            for (name, used, cost) in per_model {
                usage::record_priced(chosen.name(), &name, billing, Some(used), cost);
            }
            let text = result.as_ref().and_then(|r| r["result"].as_str());
            let text = text.map_or_else(|| stdout.to_string(), str::to_string);
            ran.map(|_| text)
        }
        Backend::Codex => {
            usage::record_codex_call(model, billing, stdout);
            if billing == usage::Billing::Plan {
                crate::extract_plan::note_stream(stdout);
            }
            let last = std::fs::read_to_string(ws.path().join("last-message.txt"));
            ran.map(|_| last.unwrap_or_default())
        }
        _ => {
            usage::record(chosen.name(), model, billing, None);
            ran
        }
    };
    if let Ok(text) = &said {
        note_envelope(ws, text);
    }
    said
}

/// The waits before re-running an agent whose CLI failed on a transient
/// backend error. Measured: a seven-minute codex outage ("Reconnecting... 5/5
/// (workspace routing discovery failed)") failed six runs at once, retries
/// unspent, where a few minutes' wait would have carried every one through.
const BACKOFF: [Duration; 3] = [
    Duration::from_secs(30),
    Duration::from_secs(90),
    Duration::from_secs(240),
];

/// What a backend says when it, not the request, failed for now: a dropped
/// connection, an overloaded or rerouting service, a short rate limit.
const TRANSIENT_BACKEND: &[&str] = &[
    "reconnecting",
    "routing discovery failed",
    "rate limit",
    "too many requests",
    "overloaded",
    "service unavailable",
    "bad gateway",
    "gateway timeout",
    "internal server error",
    "temporarily unavailable",
    "connection reset",
    "connection refused",
    "connection closed",
    "stream disconnected",
    "error sending request",
    "network error",
    "econnreset",
    "etimedout",
];

/// Whether `e` is a backend failure worth waiting out: never a timeout, an
/// idle stall or a budget stop (the attempt loop owns those), nor a usage limit.
fn transient_backend_error(e: &anyhow::Error) -> bool {
    if e.is::<TimedOut>() || e.is::<IdleStalled>() || e.is::<harness::Stopped>() {
        return false;
    }
    let text = format!("{e:#}").to_lowercase();
    !text.contains("usage limit") && TRANSIENT_BACKEND.iter().any(|p| text.contains(p))
}

/// One short reason for a transient backend error, for the progress line. The
/// error may hold codex's whole JSON event stream: the last `error` or
/// `turn.failed` message is the reason, else the first line. A 429 is named as
/// such, and a long reason is cut at 160 characters. The full error stays in
/// the logs and in the final failure.
fn short_backend_reason(e: &str) -> String {
    let reason = last_event_message(e)
        .unwrap_or_else(|| e.lines().next().unwrap_or_default().trim().to_string());
    if reason.contains("429") {
        return "rate limited (429 Too Many Requests)".into();
    }
    if reason.chars().count() > 160 {
        let cut: String = reason.chars().take(160).collect();
        return format!("{cut}...");
    }
    reason
}

/// The message of the last `"type":"error"` or `turn.failed` event in `text`.
fn last_event_message(text: &str) -> Option<String> {
    const ERROR: &str = "\"type\":\"error\",\"message\":\"";
    const MESSAGE: &str = "\"message\":\"";
    let errors = text.match_indices(ERROR).map(|(i, _)| i + ERROR.len());
    let failed = text.match_indices("\"turn.failed\"").filter_map(|(i, _)| {
        let rest = &text[i..];
        let next_event = rest[1..].find("\"type\":\"").map_or(rest.len(), |n| n + 1);
        let at = rest[..next_event].find(MESSAGE)?;
        Some(i + at + MESSAGE.len())
    });
    // The latest event starts last, so the largest start is the last message.
    errors
        .chain(failed)
        .max()
        .and_then(|start| json_string_body(&text[start..]))
}

/// The text of a JSON string whose body starts at the head of `s`, up to its
/// closing quote; `None` if it never closes.
fn json_string_body(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => out.push(chars.next()?),
            c => out.push(c),
        }
    }
    None
}

/// `call`, re-run after each of `delays` while it fails on a transient backend
/// error; the last failure, or any other, is returned as is.
///
/// A rate-limit refusal (the service turning a session away at its first
/// request) is not an outage: it is retried after a few jittered seconds that
/// grow ([`crate::extract_capacity::rate_limit_wait`]), through the batch's gate
/// so the caller's place in the pool is free for another part while it waits,
/// and it widens the spacing of session starts. `delays` is for the other
/// transient errors.
fn with_backoff<T>(delays: &[Duration], mut call: impl FnMut() -> Result<T>) -> Result<T> {
    let (mut outages, mut refusals) = (0, 0);
    loop {
        let e = match call() {
            Err(e) if transient_backend_error(&e) => e,
            done => return done,
        };
        let text = format!("{e:#}");
        let (delay, tries, of) = if crate::extract_capacity::is_rate_limited(&text) {
            progress::note_rate_limit();
            crate::extract_admission::refused();
            let wait = crate::extract_capacity::rate_limit_wait(
                refusals,
                crate::extract_admission::jitter_unit(),
            );
            refusals += 1;
            (
                wait,
                refusals + 1,
                crate::extract_capacity::RATE_LIMIT_WAITS.len() + 1,
            )
        } else {
            outages += 1;
            (
                delays.get(outages - 1).copied(),
                outages + 1,
                delays.len() + 1,
            )
        };
        let Some(delay) = delay else {
            return Err(e);
        };
        say!(
            "[model-extract] transient backend error ({}); waiting {}s before try {tries} of {of}",
            short_backend_reason(&text),
            delay.as_secs().max(1)
        );
        crate::extract_admission::pause(delay);
    }
}

/// The JSON Schema an agent's final message must match, written into the
/// sandbox by the step that wants one and enforced only by a backend that can.
const REPLY_SCHEMA: &str = "reply-schema.json";

/// The identification reply: one supported kind, or `unsupported`.
fn identify_schema() -> serde_json::Value {
    let mut kinds = supported_kinds();
    kinds.push("unsupported");
    serde_json::json!({
        "type": "object",
        "properties": {"kind": {"type": "string", "enum": kinds}},
        "required": ["kind"],
        "additionalProperties": false
    })
}

/// A drafting agent's closing envelope. The card itself stays in model.toml
/// and the validator stays its authority: its pin and parameter maps are keyed
/// by free text, which a strict schema (every key named) cannot express.
fn envelope_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "status": {"type": "string", "enum": ["written", "identity_only", "not_written"]},
            "confidence": {
                "type": "string",
                "enum": ["high", "medium", "low"],
                "description": "low while the last checker run still reports a finding"
            },
            "reason": {"type": "string"},
            "questions": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["status", "confidence", "reason", "questions"],
        "additionalProperties": false
    })
}

/// What a Claude Code drafting agent is told in place of `--json-schema`: its
/// final message is the envelope as plain JSON. Empty for any other schema
/// (the identification answer lives in model.toml).
fn envelope_note(schema_path: &Path) -> String {
    let schema = std::fs::read_to_string(schema_path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    let Some(schema) = schema.filter(|s| s["properties"]["status"].is_object()) else {
        return String::new();
    };
    format!(
        "\n\nYour final message is ONLY one JSON object with exactly these keys and no other \
         text (no code fence): status ({}), confidence (high, medium or low: {}), reason \
         (one sentence), questions (an array of strings, empty when you have none).",
        schema["properties"]["status"]["enum"]
            .as_array()
            .map(|v| {
                v.iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" | ")
            })
            .unwrap_or_default(),
        schema["properties"]["confidence"]["description"]
            .as_str()
            .unwrap_or_default()
    )
}

/// The envelope a final message holds: the whole message, or the outermost
/// `{...}` of one wrapped in a fence or a sentence. Only a reply whose
/// `confidence` is one of the schema's three words is an envelope.
fn envelope_in(text: &str) -> Option<serde_json::Value> {
    let text = text.trim();
    let braced = text
        .find('{')
        .zip(text.rfind('}'))
        .map(|(a, b)| &text[a..=b]);
    let v = [Some(text), braced]
        .into_iter()
        .flatten()
        .find_map(|t| serde_json::from_str::<serde_json::Value>(t).ok())?;
    matches!(v["confidence"].as_str(), Some("high" | "medium" | "low")).then_some(v)
}

/// Announce a drafting agent's envelope, and keep its confidence where the
/// ladder reads one when the agent wrote no `confidence.txt` itself.
fn note_envelope(ws: &Workspace, text: &str) {
    let Some(v) = envelope_in(text) else {
        return;
    };
    let Some(level) = v["confidence"].as_str() else {
        return;
    };
    let reason = v["reason"].as_str().unwrap_or_default();
    let status = v["status"].as_str().unwrap_or("unstated");
    say!("[model-extract] reply envelope: {status}, confidence {level}: {reason}");
    for q in v["questions"].as_array().into_iter().flatten() {
        let q = q.as_str().unwrap_or_default().trim();
        if !q.is_empty() {
            progress::question(q);
        }
    }
    let path = ws.path().join(harness::CONFIDENCE);
    if !path.is_file() {
        let _ = std::fs::write(path, format!("{level}\n{reason}\n"));
    }
}

/// The flag with which `exe` constrains its final message to a JSON Schema, if
/// this install has one: probed once per executable from its help text and
/// disclosed. Without it the reply is checked by the validator alone; it is
/// never a reason to switch provider.
fn schema_flag(backend: Backend, exe: &Path) -> Option<&'static str> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    let (flag, help): (&'static str, &[&str]) = match backend {
        Backend::Codex => ("--output-schema", &["exec", "--help"]),
        // Claude Code's `--json-schema` is not constrained decoding: the CLI
        // makes the model answer, then forces one more turn to call a
        // StructuredOutput tool (validate and retry). Measured on staged
        // section agents, that turn re-sends the whole context once per agent
        // for a closing envelope the prompt can ask for in plain text
        // (`envelope_note`), parsed and checked locally by `note_envelope`.
        Backend::ClaudeCode | Backend::Agy | Backend::Api => return None,
    };
    // The unit tests' fake agents count their invocations; a probe is one more.
    if cfg!(test) {
        return None;
    }
    let mut seen = SEEN.get_or_init(Mutex::default).lock().ok()?;
    let has = *seen.entry(exe.to_path_buf()).or_insert_with(|| {
        let has = help_mentions(exe, help, flag);
        let how = if has {
            "the reply's shape is schema-constrained, then validated"
        } else {
            "the reply is checked by the validator alone"
        };
        let verb = if has { "takes" } else { "lacks" };
        say!("[model-extract] {} {verb} {flag}: {how}", exe.display());
        has
    });
    drop(seen);
    has.then_some(flag)
}

/// Whether `exe <args>` prints `flag` within five seconds.
fn help_mentions(exe: &Path, args: &[&str], flag: &str) -> bool {
    use std::io::Read;
    let dir = std::env::temp_dir().join("hauksbee-flag-probe");
    let _ = std::fs::create_dir_all(&dir);
    let spawned = Command::new(exe)
        .args(args)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = spawned else {
        return false;
    };
    let mut pipe = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(p) = pipe.as_mut() {
            let _ = p.read_to_string(&mut text);
        }
        text
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return reader.join().is_ok_and(|t| t.contains(flag));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

/// Tell the progress stream which attempt is starting.
fn announce_attempt(attempt: usize, attempts: usize, reply: Reply<'_>) {
    let doing = match (attempt, reply) {
        (1, Reply::Word) => "identifying the part kind",
        (1, _) => "drafting the model",
        _ => "redrafting with the checker's feedback",
    };
    progress::phase(format!("{doing} (attempt {attempt} of {attempts})"));
    let stage = match (attempt, reply) {
        (_, Reply::Word) => "identify",
        (1, _) => "draft",
        _ => "fix",
    };
    progress::event(
        "stage",
        serde_json::json!({"stage": stage, "attempt": attempt, "attempts": attempts}),
    );
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
    base_prompt: impl Into<Prompt>,
    reply: Reply<'_>,
    tool: (&str, bool),
    fetch: impl FnMut(&Prompt) -> Result<String>,
) -> Result<String> {
    validated_attempts_seeing(retries, base_prompt, reply, tool, fetch, Default::default)
}

/// [`validated_attempts`], where `seen` yields the lint warnings the agent has
/// already been shown in-session (`ds check` runs the same validator, see
/// [`Reply::assess`]). A warning it was shown and left in the card made no
/// progress, so a redraft would only repeat the session: the card is kept with it
/// instead, and a redraft quotes only the warnings the agent had not seen.
fn validated_attempts_seeing(
    retries: usize,
    base_prompt: impl Into<Prompt>,
    reply: Reply<'_>,
    (tool, edit_in_place): (&str, bool),
    mut fetch: impl FnMut(&Prompt) -> Result<String>,
    seen: impl Fn() -> std::collections::BTreeSet<String>,
) -> Result<String> {
    let base_prompt: Prompt = base_prompt.into();
    // A retry's feedback joins the tail only, so the base stays the cacheable prefix.
    let mut prompt = base_prompt.clone();
    let mut idle_retried = false;
    // A draft that passed the checker but drew lint warnings is redrafted once with
    // them quoted; if that redraft fails, this one stands.
    let mut accepted: Option<String> = None;
    let mut previous_failure: Option<String> = None;
    let mut previous_error: Option<String> = None;
    for attempt in 1..=retries + 1 {
        let started = Instant::now();
        announce_attempt(attempt, retries + 1, reply);
        let fetched = match fetch(&prompt) {
            Err(e) if accepted.is_some() => {
                say!("[model-extract] redraft for lint warnings failed ({e:#}); keeping the accepted draft");
                return Ok(accepted.unwrap_or_default());
            }
            // One idle kill per extraction call is retried, on the same prompt,
            // spending an attempt like any other retry; a second stall fails.
            Err(e) if e.is::<IdleStalled>() && !idle_retried && attempt <= retries => {
                idle_retried = true;
                say!(
                    "[model-extract] attempt {attempt} stalled: {e}; retrying once \
                     (attempt {} of {})",
                    attempt + 1,
                    retries + 1
                );
                continue;
            }
            // A staged agent stopped at its context budget hands over to a fresh
            // one, which starts from its partial draft (`previous.toml`) and the
            // notes instead of losing the reading done so far.
            Err(e) if e.is::<harness::Stopped>() && attempt <= retries => {
                say!(
                    "[model-extract] attempt {attempt} {e}; a fresh agent continues from its draft"
                );
                prompt = base_prompt.then(harness::CONTINUE_AFTER_STOP);
                continue;
            }
            fetched => fetched?,
        };
        let raw = reply.complete(fetched);
        progress::phase("checking the answer");
        say!(
            "[model-extract] {tool} attempt {attempt} returned {} chars in {:.0}s",
            raw.len(),
            started.elapsed().as_secs_f64()
        );
        let checked = reply.assess(&raw, accepted.is_none() && attempt <= retries);
        let error = checked.as_ref().err().map(harness::reason);
        progress::event(
            "validator",
            serde_json::json!({"ok": error.is_none(), "error": error}),
        );
        match checked {
            Ok(warnings) => {
                if warnings.is_empty() {
                    return Ok(raw);
                }
                let Some(warnings) = unseen_warnings(warnings, &seen(), attempt) else {
                    return Ok(raw);
                };
                let note = format!(
                    "the draft passes validation, but the model lint warns about it:\n- {}",
                    warnings.join("\n- ")
                );
                prompt = base_prompt.then(&format!(
                    "\n\n{}",
                    retry_feedback(&raw, &note, reply.table(), edit_in_place)
                ));
                accepted = Some(raw);
            }
            Err(_) if accepted.is_some() => return Ok(accepted.unwrap_or_default()),
            // A redraft identical to the failure it was told to fix ignores the
            // feedback: measured, three identical BSS84 drafts. Another attempt
            // at this tier would repeat it, so the tier stops and the ladder
            // escalates (or, on its last tier, the draft is refused).
            Err(e) if previous_failure.as_deref() == Some(raw.trim()) => {
                return Err(Rejected::unchanged(tool, attempt, &e, raw).into());
            }
            // A redraft that differs from the last but fails with the very same
            // message (measured: PCF8523's eight-pad disagreement, three
            // agent runs of 0.6-1.2M input tokens each) did not act on it either. A one-word
            // reply costs nothing to ask again, so only a drafted entry stops here.
            Err(e)
                if !matches!(reply, Reply::Word)
                    && previous_error.as_deref() == Some(format!("{e:#}").as_str()) =>
            {
                return Err(Rejected::repeated(tool, attempt, &e, raw).into());
            }
            Err(e) if attempt <= retries => {
                previous_failure = Some(raw.trim().to_string());
                previous_error = Some(format!("{e:#}"));
                say!("[model-extract] attempt {attempt} failed: {e:#}; retrying with feedback...");
                prompt = base_prompt.then(&format!(
                    "\n\n{}",
                    retry_feedback(&raw, &format!("{e:#}"), reply.table(), edit_in_place)
                ));
            }
            Err(e) => {
                return Err(Rejected {
                    tool: tool.to_string(),
                    attempts: attempt,
                    error: format!("{e:#}"),
                    draft: raw,
                    kept: None,
                }
                .into())
            }
        }
    }
    unreachable!("the loop returns or bails on its last attempt")
}

/// The warnings a redraft should quote: those `shown` has not already put in front of
/// the agent. `None` when every one was shown and left in the card, so a redraft would
/// make no progress and the card stands as written (said, with the warnings).
fn unseen_warnings(
    warnings: Vec<String>,
    shown: &std::collections::BTreeSet<String>,
    attempt: usize,
) -> Option<Vec<String>> {
    let (left, warnings): (Vec<String>, Vec<String>) =
        warnings.into_iter().partition(|w| shown.contains(w));
    if warnings.is_empty() {
        say!(
            "[model-extract] attempt {attempt} keeps {} lint warning(s) the agent was shown by \
             `ds check` and left, so a redraft would make no progress: {}",
            left.len(),
            left.join(" | ")
        );
        return None;
    }
    say!(
        "[model-extract] attempt {attempt} draws {} new lint warning(s); redrafting with them: {}",
        warnings.len(),
        warnings.join(" | ")
    );
    Some(warnings)
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
fn retry_feedback(raw: &str, validation_error: &str, table: &str, edit_in_place: bool) -> String {
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
    let fix = if table.is_empty() {
        format!(
            "YOUR PREVIOUS REPLY WAS: {}\nWrite only the one kind identifier to model.toml.",
            truncate_to_chars(raw.trim(), 300)
        )
    } else if edit_in_place {
        format!("Fix exactly those issues. {}", harness::EDIT_IN_PLACE)
    } else {
        format!(
            "Fix exactly those issues and write the corrected WHOLE `{table}` TOML table \
             to model.toml."
        )
    };
    format!(
        "{fragment_correction}YOUR PREVIOUS ANSWER FAILED VALIDATION WITH:\n\
         {validation_error}\n\n{fix}{honest_exits}"
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
    let (reply, _) = call_backend(&identify_parts(&args.part, pdf_text), args, Reply::Word)?;
    kind_from_reply(&args.part, &reply.to_ascii_lowercase())
}

/// The whole identification prompt as one text.
#[cfg(test)]
fn identify_prompt(part: &str, pdf_text: &str) -> String {
    identify_parts(part, pdf_text).full()
}

/// The identification prompt: the rules and the kind list are the base, the same
/// for every part; the part number and the front pages are the tail.
fn identify_parts(part: &str, pdf_text: &str) -> Prompt {
    // The front of the datasheet is where the part describes itself. Sending
    // the whole thing to answer one question would cost far more for no gain.
    let base = format!(
        "You are identifying what kind of electronic part a datasheet describes, so a \
         simulator can pick the right model schema.\n\n\
         Answer with EXACTLY ONE identifier (before the colon) and nothing else:\n\
         {}\n\
         No kind need model everything: pick the one closest to the part's primary \
         pins and supply. The extraction then writes what that kind holds, else an \
         identity-only card (ratings, supply envelope, pin map) declaring the unmodelled \
         behaviour. Answer exactly `unsupported` only when not even that fits.\n\n\
         Do not explain. Do not add punctuation. One word.\n\n",
        supported_kinds()
            .iter()
            .map(|k| format!("{k}: {}\n", kind_definition(k)))
            .collect::<Vec<_>>()
            .concat(),
    );
    let tail = format!(
        "Part number: {part}\n\nDatasheet (first pages):\n{}",
        truncate_to_chars(pdf_text, 6000)
    );
    Prompt::new(base, tail)
}

/// The supported kinds a reply names as whole words. The model may answer with
/// surrounding prose despite the instruction, so a kind is looked for rather
/// than the whole reply trusted.
fn kinds_named(reply: &str) -> Vec<&'static str> {
    let reply = reply.to_ascii_lowercase();
    supported_kinds()
        .into_iter()
        .filter(|k| {
            reply
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .any(|w| w == *k)
        })
        .collect()
}

/// Whether an identification reply is usable: exactly one kind, or the
/// explicit `unsupported`. Anything else (prose, a refusal that claims the
/// sandbox is unreadable, two kinds) is retried with the reason quoted back.
fn check_kind_reply(raw: &str) -> Result<()> {
    let found = kinds_named(raw);
    let unsupported = raw.to_ascii_lowercase().contains("unsupported");
    match found.as_slice() {
        [_] => Ok(()),
        [] if unsupported => Ok(()),
        [] => bail!(
            "the reply named no kind from the list. Your working directory and every file in it \
             are readable; if the text in the prompt is not enough, open page-index.md or \
             pages/page-001.txt. Answer with exactly one kind identifier"
        ),
        many => bail!(
            "the reply named several kinds ({}); answer with the ONE closest to the part's \
             primary pins and supply",
            many.join(", ")
        ),
    }
}

fn kind_from_reply(part: &str, reply: &str) -> Result<String> {
    let found = kinds_named(reply);
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
        "vreg" => {
            "linear regulator, switching regulator or DC-DC converter (buck, boost), reference, \
             or other part whose job is a rail"
        }
        "opamp" => "amplifier with an analog output (op-amp, instrumentation, audio)",
        "comparator" => "comparator or supervisor with a two-level output",
        "analog_switch" => "analog switch, multiplexer or load switch",
        "digital" => {
            "logic, buffer, level shifter, or a driver IC steered by logic inputs (motor, gate, \
             LED or battery-pack FET driver, H-bridge): a driver whose outputs are not logic \
             becomes a supply-draw card"
        }
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
#[must_use]
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
///
/// Returned as two texts: the fixed rules (the same for every part of a kind, so
/// they belong to the cacheable base) and this part's pages and files.
fn verification_parts(ws: &Workspace, table: &str, inline: bool) -> (String, String) {
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
                format!("- PDF page {}: {}", page.number, reasons)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let category_coverage = ws.category_disclosures.join("\n");
        // Measured: an unbounded agent printed the whole dump, and every later
        // turn re-sent it (500k+ input tokens per draft); the hand-writer read
        // the few pages it needed by number.
        let (shown, how) = if ws.has_text_dump {
            (
                if inline {
                    "their text (pdftotext -layout) is below"
                } else {
                    "their text is in `pages/page-NNN.txt`"
                },
                "`page-index.md` lists every other page's topics and `pages/page-NNN.txt` holds \
                 each page's text: read further pages by number, several in one command. Never \
                 print `datasheet.txt`, the PDF or `prompt.md` (this instruction); everything you \
                 read is re-sent on every later turn. The renders are `page-NNN.png`: open one \
                 only where the text leaves a table's columns or the pinout ambiguous.",
            )
        } else {
            (
                "their renders are attached",
                "No text layer was available: read values off the IMAGES for anything in a table \
                 or pinout.",
            )
        };
        format!(
            "{} relevance-selected page(s); {shown}:\n{inventory}\n\nPer-category coverage:\n\
             {category_coverage}\n\nThis is not the whole datasheet. The complete PDF is \
             `datasheet.pdf`. {how} If a needed fact is on an omitted page, read that page \
             before answering.{}",
            ws.pages.len(),
            if inline {
                selected_page_text(ws)
            } else {
                String::new()
            },
        )
    };

    let check = ws.checker.as_ref().map_or_else(
        || "No checker is provided; do not look for one.".to_string(),
        |c| {
            format!(
                "Check model.toml by running exactly `'{}' models lint model.toml` and fix \
                 what it reports, except its `is omitted but its cited row gives` lines: \
                 Hauksbee writes those params, and a tagged rating's key, itself from the \
                 cited rows. Never run any other `hauksbee`.",
                c.display()
            )
        },
    );
    let rules = format!(
        "## Before you answer\n\n         Prefer the supplied authoritative schema and datasheet. Use ONLY the \
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
         TOML document and nothing else.{}",
        helper_commands(ws)
    );
    let here = format!(
        "## The pages and files for this part\n\n{pages}{}",
        helper_files(ws)
    );
    (rules, here)
}

/// [`verification_parts`] as one text, pages first.
#[cfg(test)]
fn verification_clause(ws: &Workspace, table: &str, inline: bool) -> String {
    let (rules, here) = verification_parts(ws, table, inline);
    format!("{here}\n\n{rules}")
}

/// What the agent is told about the precomputed files and the `ds` helpers.
///
/// Measured: a strong drafter found the pin table through the contents pages,
/// parsed its word boxes to get the right package column, and diffed its draft
/// against the board; a cheaper one read the pins off the text dump and took
/// ten pads from the neighbouring package. The harness now does those steps
/// (see [`crate::ds_toolkit`]); this tells the drafter where the results are.
fn helper_files(ws: &Workspace) -> String {
    let has = |name: &str| ws.path().join(name).exists();
    let mut files = vec!["`toc.md` (contents mapped to PDF pages, key sections first)"];
    if has("pins/index.md") {
        files.push(
            "`pins/` (the pin table read per package column from the PDF's word positions; \
             `pins/index.md` lists the columns)",
        );
    }
    if has(crate::ds_toolkit::CHECKLIST_FILE) {
        files.push("`checklist.md` (every field this kind of card needs, and where it lives)");
    }
    if has(crate::ds_toolkit::BOARD_FILE) {
        files.push("`board.md` (the board part: value, footprint, pad count, each pad's net)");
    }
    if has(crate::card_templates::DIR) {
        files.push(
            "`templates/` (finished library cards of this kind to copy the structure and \
             coverage from, never their values; `templates/index.md` says why each was chosen)",
        );
    }
    if has(crate::companion_docs::DIR) {
        files.push(
            "`companions/` (documents the datasheet defers to, fetched and verified; \
             `companions/index.md` lists them, `ds find` searches them, `ds page N --doc DOC` \
             reads one page)",
        );
    }
    format!(
        "\n\nPrecomputed in your working directory (your current directory: use these relative paths): {}.",
        files.join(", ")
    )
}

/// The `ds` commands, fixed text for a given checker (so part of the shared base).
fn helper_commands(ws: &Workspace) -> String {
    if ws.checker.is_none() {
        return String::new();
    }
    "\n\nThe precomputed files listed under \"The pages and files for this part\" are also \
     one-line commands on your PATH: `ds toc`, `ds find <regex>` \
     (page-numbered hits), `ds page N`, `ds pins [--package P | --pads N]`, `ds board`, \
     `ds checklist`, `ds similar [--kind K]` (finished cards to copy the shape from), \
     and `ds check` (validation, value_re/footprint_re binding, the pin map \
     against the pin table, and checklist coverage, each finding worded as its fix). Take \
     the pin map from `ds pins` for THIS package's column rather than counting columns in \
     the text. Before you finish, run `ds check` and fix each finding, or record the field \
     as an `unknown` uncertainty when the datasheet has no row for it."
        .to_string()
}

/// The selected pages' text, inline, so the agent starts with the tables it
/// needs instead of spending turns opening them (each turn re-sends all).
fn selected_page_text(ws: &Workspace) -> String {
    use std::fmt::Write as _;
    const CAP: usize = 48_000;
    if !ws.has_text_dump {
        return String::new();
    }
    let mut out = String::new();
    for page in &ws.selected_pages {
        let path = ws.path().join(format!("pages/page-{:03}.txt", page.number));
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        if out.len() + text.len() > CAP {
            let n = page.number;
            let _ = write!(
                out,
                "\n\n[page {n} and later omitted for length: read them by number]"
            );
            break;
        }
        let _ = write!(
            out,
            "\n\n===== PDF page {} =====\n{}",
            page.number,
            text.trim_end()
        );
    }
    out
}

/// The identification call's instruction in place of the drafting one.
const IDENTIFY_CLAUSE: &str = "## How to answer\n\nThe datasheet's front-page text is in this \
    prompt and is normally enough. Only if it does not say what the part is, read \
    `page-index.md` and then one or two `pages/page-NNN.txt` files; do not read `datasheet.txt` \
    or `datasheet.pdf` whole and do not run any checker. Write the one kind identifier to \
    `model.toml` in your working directory and nothing else.";

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
///
/// # Errors
///
/// Fails when the extraction config cannot be located, read, parsed or
/// validated.
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
        resolved.api.base_url.clone_from(base);
    }
    if let Some(name) = &args.api_key_env {
        resolved.api.api_key_env.clone_from(name);
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
/// the checker, read (and when `write`, write) the workspace, and read nothing
/// else (home, `/Users`, `/Volumes`, `/Library`, `/tmp` and `$TMPDIR` included).
/// Checked against codex-cli 0.156.1 with `codex sandbox -P hauksbee`: seatbelt
/// refuses `ls ~`, `~/.codex/skills`, `/tmp` and a `$TMPDIR` sibling while the
/// workspace and checker work, and a `"read"` workspace refuses writes. An
/// explicit `--sandbox` flag overrides `default_permissions`, so none is passed.
fn codex_confinement(ws: &Workspace, write: bool) -> [String; 4] {
    let reads = CONFINED_READ_ROOTS
        .iter()
        .map(|root| (*root).to_string())
        .chain(ws.checker.iter().map(|c| c.display().to_string()))
        .map(|path| format!("{}=\"read\",", toml::Value::String(path)))
        .collect::<Vec<_>>()
        .concat();
    let access = if write { "write" } else { "read" };
    let profile = format!(
        "permissions={{hauksbee={{filesystem={{\":minimal\"=\"read\",\":slash_tmp\"=\"none\",\
         \":tmpdir\"=\"none\",{reads}\":workspace_roots\"={{\".\"=\"{access}\"}}}}}}}}"
    );
    ["-c", "default_permissions=\"hauksbee\"", "-c", &profile].map(String::from)
}

/// `--settings` for Claude Code's Bash sandbox, strict (no unsandboxed retry,
/// no silent start without it): reads denied under home, every user's home,
/// mounted volumes and the temp roots except the workspace and the checker.
/// Other system paths (`/etc`, `/Library`, `/Applications`) stay readable.
/// `--restricted` already confines Read/Edit/Write to the working directory.
fn claude_confinement(ws: &Workspace) -> String {
    let temp_root = ws.path().parent().unwrap_or_else(|| ws.path());
    let allow: Vec<String> = std::iter::once(ws.path())
        .chain(ws.checker.as_deref())
        .map(|p| p.display().to_string())
        .collect();
    serde_json::json!({"sandbox": {
        "enabled": true, "failIfUnavailable": true, "allowUnsandboxedCommands": false,
        "filesystem": {
            "denyRead": ["~/", "/Users", "/home", "/Volumes", "/mnt", "/media", "/tmp",
                "/private/tmp", "/private/var/folders", temp_root.display().to_string()],
            "allowRead": allow,
        },
    }})
    .to_string()
}

/// The user's PATH behind the workspace's `.bin`, whose `hauksbee` shim runs only the
/// checker the prompt names (or `false` when there is none), so a bare `hauksbee`
/// never reaches another install while every other tool on the user's PATH,
/// such as a wrapper's `claude` sharing `~/.local/bin` with `hauksbee`, stays
/// reachable. The shim is a `/bin/sh` script inside the workspace, which both
/// confinements already let the agent read, and it execs the allowed checker.
/// A `hauksbee.cmd` beside it does the same for Windows cmd and `PowerShell`,
/// which find it by PATHEXT; Git Bash runs the extensionless script.
fn agent_path(ws: &Workspace) -> Result<std::ffi::OsString> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let (bin, shim) = (ws.path().join(".bin"), ws.path().join(".bin/hauksbee"));
    let checker = ws.checker.clone().unwrap_or_else(|| "false".into());
    let quoted = checker.to_string_lossy().replace('\'', r"'\''");
    std::fs::create_dir_all(&bin)?;
    std::fs::write(&shim, format!("#!/bin/sh\nexec '{quoted}' \"$@\"\n"))?;
    std::fs::write(
        bin.join("hauksbee.cmd"),
        format!("@\"{}\" %*", checker.display()),
    )?;
    #[cfg(unix)]
    std::fs::set_permissions(&shim, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    // `ds`: the datasheet helpers, the same checker's hidden `models ds`.
    let here = ws.path().to_string_lossy().replace('\'', r"'\''");
    let ds = bin.join("ds");
    let body = if ws.checker.is_some() {
        format!("#!/bin/sh\nexec '{quoted}' models ds --dir '{here}' \"$@\"\n")
    } else {
        "#!/bin/sh\necho 'ds is unavailable in this run: read toc.md, pins/, board.md and \
         checklist.md directly' >&2\nexit 1\n"
            .to_string()
    };
    std::fs::write(&ds, body)?;
    std::fs::write(
        bin.join("ds.cmd"),
        format!(
            "@\"{}\" models ds --dir \"{}\" %*",
            checker.display(),
            ws.path().display()
        ),
    )?;
    #[cfg(unix)]
    std::fs::set_permissions(&ds, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    let dirs = std::iter::once(bin).chain(std::env::split_paths(&path));
    Ok(std::env::join_paths(dirs)?)
}

/// One sandboxed run of whichever agent CLI the settings name.
///
/// The three CLIs differ only in how they spell the same four things: the
/// model, the reasoning effort, the permission mode that lets them write
/// `model.toml` without a prompt, and where the instruction goes. Each is
/// always named rather than inherited: a CLI's own default varies by plan and
/// config and would silently decide how good the extraction is.
///
///   * codex: in every mode but `danger-full-access` (`--sandbox`, unconfined)
///     the read-confining `codex_confinement` profile, its workspace read-only
///     in `read-only` mode, plus `--approve-for-me` in that mode. Network is off
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
///     subagents and web access; `claude_confinement` sandboxes Bash reads;
///     background tasks, which keep `-p` alive, are off. Never
///     `--bare`: it skips the keychain, losing a subscription sign-in.
///   * agy: `--mode <mode> --sandbox ... --add-dir <sandbox>`, and the pointer as
///     the `--print=` value because agy reads nothing from stdin. `--sandbox` is
///     agy's own terminal sandbox (workspace-only files, no network, per its
///     docs); hauksbee cannot verify it or confine agy's file tools.
///
/// Every CLI gets a PATH from `agent_path` and the workspace as its cwd.
/// This is the one place an agent CLI is launched (`model-extract`, the smoke
/// test and `resource-extract` all come here), so confinement cannot drift.
///
/// # Errors
///
/// Fails for the API backend, when the executable is missing, and when the run
/// fails or times out.
pub fn run_agent_once(resolved: &Resolved, prompt: &str, ws: &Workspace) -> Result<String> {
    run_agent_budgeted(resolved, prompt, ws, 0)
}

/// [`run_agent_once`], stopped with [`harness::Stopped`] once a streamed turn
/// shows more than `budget` tokens of context (0 = no budget).
///
/// # Errors
///
/// As [`run_agent_once`], and when the budget is passed.
pub fn run_agent_budgeted(
    resolved: &Resolved,
    prompt: &str,
    ws: &Workspace,
    budget: u64,
) -> Result<String> {
    run_agent_with(resolved, &Prompt::from(prompt), ws, budget, false)
}

/// Codex's web search. Its own flag is `--search` on the top-level command only;
/// `exec` takes the same setting as a config override (checked against
/// codex-cli 0.159.0: the run shows `web_search` events).
const fn codex_search_args(web: bool) -> &'static [&'static str] {
    if web {
        &["-c", "web_search=\"live\""]
    } else {
        &[]
    }
}

/// Claude Code's tools: a search run gets its web tools and a file to write the
/// answer to, no shell; an extraction run gets the shell and file tools.
const fn claude_tools(web: bool) -> &'static str {
    if web {
        "WebSearch,WebFetch,Write"
    } else {
        "Bash,Read,Edit,Write"
    }
}

/// One agent run; with `web` the agent is given its web search tool and nothing
/// else changes (same confinement of files and shell, no page images).
///
/// A codex run whose prompt has a shared base is, when it can be, an ephemeral
/// fork of a warmed base session (see [`crate::extract_cache::codex`]); it falls
/// back to a fresh call whenever forking is unavailable or the base is gone.
pub(crate) fn run_agent_with(
    resolved: &Resolved,
    prompt: &Prompt,
    ws: &Workspace,
    budget: u64,
    web: bool,
) -> Result<String> {
    use crate::extract_cache::codex as shared;
    let Some((key, base)) = fork_base(resolved, prompt, ws, web) else {
        return launch_agent(resolved, prompt, ws, budget, web, Launch::Fresh);
    };
    match launch_agent(resolved, prompt, ws, budget, web, Launch::Fork(&base)) {
        Err(e) if base_is_gone(&e) => {
            say!("[model-extract] cache: the shared codex base could not be forked ({e:#}); using fresh calls");
            shared::invalidate(&key);
            launch_agent(resolved, prompt, ws, budget, web, Launch::Fresh)
        }
        ran => ran,
    }
}

/// How a codex run starts: a fresh process (`exec --ephemeral`), the warm-up of a
/// shared base (`exec`, persisted), or an ephemeral fork of a warmed base.
#[derive(Clone, Copy)]
pub(crate) enum Launch<'a> {
    Fresh,
    Warm,
    Fork(&'a crate::extract_cache::codex::Base),
}

/// One launch of the agent CLI (see [`run_agent_with`] and [`run_agent_once`]).
#[allow(clippy::too_many_lines)] // one launcher: every CLI's flags stay side by side
pub(crate) fn launch_agent(
    resolved: &Resolved,
    prompt: &Prompt,
    ws: &Workspace,
    budget: u64,
    web: bool,
    launch: Launch<'_>,
) -> Result<String> {
    use crate::extract_cache as cache;
    let backend = resolved.backend;
    let Some(a) = resolved.agent(backend) else {
        bail!("the api backend is not an agent CLI");
    };
    // A batch spaces the starts of its sessions (a burst is what the service refuses).
    crate::extract_admission::before_session();
    let cache_on = cache::Mode::from_env() != cache::Mode::Off;
    // What the trace and prompt.md hold: the whole instruction, however it is sent.
    let whole = agent_stdin(backend, &prompt.full());
    let mut stdin_text = match launch {
        Launch::Fresh => whole.clone(),
        Launch::Warm => prompt.tail.clone(),
        Launch::Fork(_) => cache::codex::fork_prompt(&prompt.tail),
    };
    // Claude Code takes the shared base as its appended system prompt, which it
    // caches by content; the stdin is then the part's tail alone.
    let mut claude_base = None;
    if backend == Backend::ClaudeCode {
        if cache_on && !prompt.base.is_empty() {
            let text = format!("{WORKSPACE_NOTE}\n\n{}", prompt.base);
            claude_base = Some(cache::base_file(&text).context("writing the shared base prompt")?);
            stdin_text.clone_from(&prompt.tail);
        }
        stdin_text += &envelope_note(&ws.path().join(REPLY_SCHEMA));
    }
    let tool = &a.command;
    let executable = which_path(tool)
        .with_context(|| format!("executable `{tool}` was not found; no fallback attempted"))?;
    let exe = std::path::absolute(executable)?;
    let mut cmd = Command::new(&exe);
    let schema_path = ws.path().join(REPLY_SCHEMA);
    let schema = schema_path.is_file().then(|| schema_flag(backend, &exe));
    let schema = schema.flatten();
    match backend {
        Backend::Codex => {
            // `--json` streams one JSONL event per step; the idle watchdog reads that stream.
            // A fork takes the exec-level `--ephemeral` before `fork`, and no `--cd`,
            // `-p`, `--sandbox` or `--approve-for-me`: its cwd is the process's.
            cmd.arg("exec");
            if matches!(launch, Launch::Fork(_)) {
                cmd.args(["--ephemeral", "fork"]);
            }
            cmd.arg("--json");
            if let Some(profile) = &a.profile {
                cmd.args(["-p", profile]);
            }
            let effort = format!("model_reasoning_effort=\"{}\"", a.effort);
            cmd.args(["--model", &a.model, "-c", &effort]);
            cmd.args(codex_search_args(web));
            match a.permission_mode.as_str() {
                "danger-full-access" => cmd.args(["--sandbox", "danger-full-access"]),
                mode => cmd
                    .args((mode == "approve-for-me").then_some("--approve-for-me"))
                    .args(codex_confinement(ws, mode != "read-only")),
            };
            cmd.arg("--skip-git-repo-check");
            if !matches!(launch, Launch::Fork(_)) {
                cmd.arg("--cd").arg(ws.path());
            }
            cmd.arg("--output-last-message")
                .arg(ws.path().join("last-message.txt"));
            if matches!(launch, Launch::Fresh) {
                cmd.arg("--ephemeral");
            }
            cmd.args(a.profile.is_none().then_some("--ignore-user-config"))
                .args(["--ignore-rules", "-c", "skills.include_instructions=false"]);
            if cache_on {
                // The environment block (date, working directory) ahead of the prompt
                // would end a cache hit at the first line; the model needs neither to
                // read a datasheet, and its files are named relative to where it runs.
                cmd.args(["-c", "include_environment_context=false"]);
            }
            for feature in CODEX_DISABLED_FEATURES {
                cmd.args(["--disable", feature]);
            }
            // One value, so it goes before the many-valued `--image`.
            if let Some(flag) = schema {
                cmd.arg(flag).arg(&schema_path);
            }
            // With a text layer the selected pages' text is in the prompt and the
            // renders stay on disk: measured, seven attached renders cost about
            // 2.4k input tokens each on EVERY turn of the session.
            for page in ws.pages.iter().filter(|_| !ws.has_text_dump) {
                cmd.arg("--image").arg(page);
            }
        }
        Backend::ClaudeCode => {
            cmd.args([
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
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
            .args([claude_tools(web), "--settings", &claude_confinement(ws)])
            .env("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS", "1")
            .env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1");
            if cache_on {
                // The base as the cached system prompt, the per-machine sections
                // moved behind it, and the long cache lifetime (see `claude_flags`).
                cmd.args(cache::claude_flags(claude_base.as_deref()))
                    .envs(cache::CLAUDE_CACHE_ENV);
            }
            // No `--json-schema` (see `schema_flag`): the envelope is asked for
            // in the prompt, so the run has no forced extra turn.
        }
        Backend::Agy => {
            cmd.args(["--output-format", "text", "--mode", &a.permission_mode])
                .arg("--sandbox")
                .args(["--model", &a.model, "--effort", &a.effort, "--add-dir"])
                .arg(ws.path())
                .arg(format!("--print={}", agent_pointer()));
        }
        Backend::Api => unreachable!("handled above"),
    }
    cmd.env("PATH", agent_path(ws)?);
    // A helper script the agent runs leaves no __pycache__ in the sandbox.
    cmd.env("PYTHONDONTWRITEBYTECODE", "1");
    cmd.args(&a.extra_args).current_dir(ws.path());
    if let Launch::Fork(base) = launch {
        // The positionals last: the session to fork, and the prompt from stdin.
        cmd.arg(&base.id).arg("-");
    }
    let log = ws.path().join(format!("{}-stderr.log", backend.name()));
    let prompt_file = if matches!(launch, Launch::Warm) {
        prompt.tail.clone()
    } else {
        prompt.full()
    };
    let stdout = run_cli_agent(
        tool,
        &mut cmd,
        ws,
        (&prompt_file, &stdin_text),
        &log,
        Duration::from_secs(resolved.timeout_secs),
        // Codex streams an event per step; claude streams one per turn, and a
        // long turn (a whole TOML table written at high effort) is a normal
        // silence, as agy's single print is, so only codex gets the watchdog.
        (backend == Backend::Codex && resolved.idle_secs > 0)
            .then(|| Duration::from_secs(resolved.idle_secs)),
        Some(harness::watch(&a.model, &ws.label(), budget)),
    );
    // A fork reports the usage of the base it came from too: take that out.
    let stdout = match (stdout, launch) {
        (Ok(out), Launch::Fork(base)) => Ok(cache::codex::without_base(&out, base.used)),
        (ran, _) => ran,
    };
    // The trace keeps the whole instruction, however it was split between a
    // shared base and this call's own text.
    let traced = match launch {
        Launch::Warm => stdin_text.clone(),
        _ if backend == Backend::ClaudeCode => {
            format!("{whole}{}", envelope_note(&ws.path().join(REPLY_SCHEMA)))
        }
        _ => whole,
    };
    record_trace(ws, &exe, &cmd, &traced, &stdout);
    let stdout = stdout?;
    if backend == Backend::Agy {
        agy_denial(&stdout, &log, ws)?;
    }
    let (how, tokens) = match (backend, launch) {
        (Backend::Codex, _) => (
            match launch {
                Launch::Fresh => "fresh",
                Launch::Warm => "base warm-up",
                Launch::Fork(_) => "fork",
            },
            cache::codex::tokens_of(&stdout),
        ),
        (Backend::ClaudeCode, _) => (
            if claude_base.is_some() {
                "system-cached base"
            } else {
                "fresh"
            },
            harness::claude_result(&stdout)
                .as_ref()
                .and_then(cache::Tokens::from_claude),
        ),
        _ => ("fresh", None),
    };
    if let Some(tokens) = tokens {
        cache::record_call(backend.name(), how, tokens);
    }
    Ok(stdout)
}

/// Keep this call's argv, prompt and event stream in the run's trace, when
/// it keeps one; a failed call's error becomes its one event.
fn record_trace(
    ws: &Workspace,
    exe: &Path,
    cmd: &Command,
    stdin_text: &str,
    stdout: &Result<String>,
) {
    let Some(trace) = &ws.trace else {
        return;
    };
    let argv: Vec<String> = std::iter::once(exe.to_string_lossy().into_owned())
        .chain(cmd.get_args().map(|a| a.to_string_lossy().into_owned()))
        .collect();
    let events = match stdout {
        Ok(s) => s.clone(),
        Err(e) => {
            let message = serde_json::Value::String(format!("{e:#}"));
            format!("{{\"type\":\"hauksbee.error\",\"message\":{message}}}\n")
        }
    };
    trace.record(&argv, stdin_text, &events);
}

/// The shared codex base to fork this run from, with its pool key: `None` when
/// the run must be a fresh call. Forking needs a codex run with a shared base
/// and no profile, `--sandbox` or `--approve-for-me` (`exec fork` takes none of
/// them), no attached page images, and a codex that has `exec fork`.
fn fork_base(
    resolved: &Resolved,
    prompt: &Prompt,
    ws: &Workspace,
    web: bool,
) -> Option<(String, crate::extract_cache::codex::Base)> {
    use crate::extract_cache as cache;
    let a = resolved.agent(resolved.backend)?;
    let mode = cache::Mode::from_env();
    let eligible = resolved.backend == Backend::Codex
        && mode.forks()
        && !crate::extract_admission::solo()
        && !web
        && !prompt.base.is_empty()
        && a.profile.is_none()
        && !matches!(
            a.permission_mode.as_str(),
            "danger-full-access" | "approve-for-me"
        )
        && (ws.pages.is_empty() || ws.has_text_dump);
    if !eligible || (mode == cache::Mode::Auto && !fork_supported(&a.command)) {
        return None;
    }
    let base_text = format!("{WORKSPACE_NOTE}\n\n{}", prompt.base);
    let schema = std::fs::read_to_string(ws.path().join(REPLY_SCHEMA)).unwrap_or_default();
    let key = cache::pool_key(&[
        &a.command,
        &a.model,
        &a.effort,
        &a.permission_mode,
        &a.extra_args.join("\u{1f}"),
        &ws.checker
            .as_deref()
            .map(|c| c.display().to_string())
            .unwrap_or_default(),
        &schema,
        &base_text,
    ]);
    let base = cache::codex::base_for(&key, || warm_codex_base(resolved, ws, &base_text))?;
    Some((key, base))
}

/// Run the one warm-up call of a shared base: the briefing and a request to say
/// READY, persisted so forks can start from it. Its usage is recorded like any call.
fn warm_codex_base(
    resolved: &Resolved,
    ws: &Workspace,
    base_text: &str,
) -> Result<crate::extract_cache::codex::Base> {
    use crate::extract_cache::codex as shared;
    let mut scratch = Workspace::scratch()?;
    scratch.checker.clone_from(&ws.checker);
    // The same schema file, byte for byte: it is part of the cached prefix.
    if let Ok(schema) = std::fs::read(ws.path().join(REPLY_SCHEMA)) {
        std::fs::write(scratch.path().join(REPLY_SCHEMA), schema)?;
    }
    let prompt = Prompt::from(shared::warm_prompt(base_text));
    say!(
        "[model-extract] cache: warming a shared codex base for {}",
        resolved.model()
    );
    let stdout = launch_agent(resolved, &prompt, &scratch, 0, false, Launch::Warm)?;
    let id = shared::session_id(&stdout)
        .context("the warm-up run reported no session id (is codex too old for `exec fork`?)")?;
    crate::extract_usage::record(
        resolved.backend.name(),
        resolved.model(),
        billing_for(resolved),
        crate::extract_usage::from_codex_events(&stdout),
    );
    Ok(shared::Base {
        id,
        used: shared::tokens_of(&stdout).unwrap_or_default(),
    })
}

/// Whether `exec fork` exists in this codex (one `--help` per executable per process).
fn fork_supported(command: &str) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, PoisonError};
    static KNOWN: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);
    if let Some(known) = KNOWN
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(|m| m.get(command))
    {
        return *known;
    }
    let found = which_path(command).is_some_and(|exe| {
        Command::new(exe)
            .args(["exec", "fork", "--help"])
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .is_ok_and(|o| {
                o.status.success() && String::from_utf8_lossy(&o.stdout).contains("Fork a previous")
            })
    });
    KNOWN
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(HashMap::new)
        .insert(command.to_string(), found);
    found
}

/// Whether a failed fork says the base session it needed is not there (deleted
/// rollout, unknown id), as opposed to the run itself failing or timing out.
fn base_is_gone(e: &anyhow::Error) -> bool {
    if e.is::<TimedOut>() || e.is::<IdleStalled>() || e.is::<harness::Stopped>() {
        return false;
    }
    let text = format!("{e:#}").to_ascii_lowercase();
    [
        "session not found",
        "no session",
        "not found",
        "no such",
        "unknown session",
        "rollout",
        "could not find",
        "failed to fork",
    ]
    .iter()
    .any(|m| text.contains(m))
}

/// What agy is told beyond the shared prompt. Headless agy auto-denies every
/// shell command that has no allow-rule, then stops with no answer and exit 0,
/// and it honours rules only in the user's own settings.json (probed: nothing
/// under the workspace is read). So it is steered to its file tools, with the
/// lint command as the one command it may run.
fn agy_tool_note(ws: &Workspace) -> String {
    let lint = ws.checker.as_ref().map_or_else(
        || "Run no shell command at all.".to_string(),
        |c| {
            format!(
                "The only shell command you may run is exactly `'{}' models lint model.toml`.",
                c.display()
            )
        },
    );
    format!(
        "\n\nTool rule for this run: list and read files with your file tools, never with \
         shell commands (no `ls`, `cat`, `find`, `head`). {lint} Never run `--help` or any \
         other command, `ds` included (read its precomputed files instead): a denied command \
         ends the whole run with no answer."
    )
}

/// Turn agy's silent permission stop into an error that names the fix.
///
/// A run that printed nothing and left no `model.toml` is never a usable
/// reply. agy's own message ("a tool required the \"command\" permission ...
/// auto-denied") goes to stderr, but a denial can also leave nothing at all, so
/// both read as a likely permission denial rather than "empty reply".
fn agy_denial(stdout: &str, log: &Path, ws: &Workspace) -> Result<()> {
    let answered = std::fs::read_to_string(ws.answer_path()).is_ok_and(|t| !t.trim().is_empty());
    if answered || !stdout.trim().is_empty() {
        return Ok(());
    }
    let stderr = std::fs::read_to_string(log).unwrap_or_default();
    let said = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map_or_else(String::new, |l| format!(" It said: {l}"));
    let denied = [
        "auto-denied",
        "no output produced",
        "user denied permission",
    ]
    .iter()
    .any(|m| stderr.contains(m));
    let cmd = ws.checker.as_ref().map_or_else(
        || "<the denied command>".to_string(),
        |c| format!("'{}' models lint model.toml", c.display()),
    );
    let cause = if denied {
        "a tool needed a permission that headless agy cannot prompt for, so it was auto-denied"
    } else {
        "it exited 0 with no output and no model.toml, which headless agy does when a tool \
         permission (usually `command`) is auto-denied"
    };
    bail!(
        "agy stopped without an answer: {cause}.{said} Fix: add an allow-rule to \
         permissions.allow in ~/.gemini/antigravity-cli/settings.json (agy reads no \
         workspace-level rules), e.g. `command({cmd})`; or use another backend."
    )
}

/// Run the configured backend on a one-line prompt and return what it said.
///
/// The cheapest end-to-end proof that a setup works: the CLI is found, it is
/// signed in, the model name is accepted, and an answer comes back. Costs one
/// tiny model call; sends nothing but the prompt.
///
/// # Errors
///
/// Fails when the API request cannot be prepared or sent, the agent CLI is
/// missing, the sandbox cannot be built, the agent run fails, or the agent does
/// not write `model.toml`.
///
/// # Panics
///
/// Never in practice: an agent backend always resolves an agent section.
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
/// to one vendor; it would also brush `ARG_MAX`), stdin carries only a pointer to
/// that file (codex and Claude Code get the whole prompt on stdin, which is no
/// argv either: measured, an agent told to read the file spent two turns
/// re-reading a truncated `cat` of it, and every later turn re-sent both), and
/// the answer is expected in `model.toml`.
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
    (prompt, stdin_text): (&str, &str),
    log: &Path,
    timeout: Duration,
    idle: Option<Duration>,
    watch: Option<harness::Watch>,
) -> Result<String> {
    std::fs::write(ws.path().join("prompt.md"), prompt)
        .context("writing the prompt into the sandbox")?;
    let stderr = std::fs::File::create(log).map_or_else(|_| Stdio::null(), Stdio::from);
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
        let _ = stdin.write_all(stdin_text.as_bytes());
    }
    wait_for_cli_backend(child, tool, log, (timeout, idle), watch)
}

/// What goes on the agent's stdin: the prompt itself, except for agy, whose
/// prompt goes on argv and so is only a pointer to prompt.md.
fn agent_stdin(backend: Backend, prompt: &str) -> String {
    if backend == Backend::Agy {
        agent_pointer()
    } else {
        format!("{WORKSPACE_NOTE}\n\n{prompt}")
    }
}

/// agy's instruction (its `--print` value): a pointer to prompt.md, since its
/// prompt goes on argv. codex and Claude Code get the prompt itself on stdin.
fn agent_pointer() -> String {
    format!("Read prompt.md in your working directory and follow it exactly. {WORKSPACE_NOTE}")
}

/// What every agent run is told about its working directory, ahead of the
/// prompt (inline) or the pointer to it.
const WORKSPACE_NOTE: &str = "The supplied files and output location are in your working directory; read nothing outside \
    it, use the file tools available to you, and report any denied required read or write instead of claiming success. \
    The working directory sits inside the system temporary folder, which the sandbox policy \
    denies in general but grants for this directory: open its files rather than concluding \
    from the policy text that you cannot, and report a denial only after a read has failed. \
    Where the instruction does not carry the page text, read pages/page-NNN.txt by number; \
    open a rendered page (page-NNN.png) only where the text leaves a table column or the pinout \
    ambiguous. Write your answer to the file \
    the instruction names (model.toml unless it names another).";

/// Poll a spawned CLI agent to completion, killing its process tree at
/// `timeout` (or, when `idle` is set, after that long with no stdout), and
/// return its stdout. On a non-zero exit the error quotes the last few
/// non-empty lines of BOTH stdout and the captured stderr log: a CLI agent puts
/// the reason it gave up (a refused model, a rate limit, a bad config key) on
/// stderr, so a failure with an empty stdout would otherwise read as "exited
/// with status 1: " and nothing after the colon.
///
/// Stdout is drained by a reader thread while polling, which stamps the time of
/// every read: that stamp is the stream's heartbeat, and draining also keeps a
/// chatty agent from filling the pipe and blocking in write(2).
fn wait_for_cli_backend(
    mut child: Child,
    tool: &str,
    log_path: &Path,
    (timeout, idle): (Duration, Option<Duration>),
    mut watch: Option<harness::Watch>,
) -> Result<String> {
    use std::io::Read;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    let start = Instant::now();
    let beat = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(Mutex::new(None::<String>));
    let mut pipe = child.stdout.take().context("capturing agent stdout")?;
    let reader = {
        let (beat, stop) = (Arc::clone(&beat), Arc::clone(&stop));
        let label = progress::label();
        std::thread::spawn(move || {
            progress::set_label(label);
            let (mut out, mut buf) = (Vec::new(), [0u8; 8192]);
            let mut line_start = 0;
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
                // Each complete line goes to the watcher, which may ask for a stop.
                while let Some(end) = out[line_start..].iter().position(|b| *b == b'\n') {
                    let line = String::from_utf8_lossy(&out[line_start..line_start + end]);
                    if let Some(why) = watch.as_mut().and_then(|w| w(&line)) {
                        *stop
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(why);
                    }
                    line_start += end + 1;
                }
                let millis = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                beat.store(millis, Ordering::Relaxed);
            }
            out
        })
    };
    let deadline = start + timeout;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("polling {tool}"))?
        {
            break status;
        }
        let silent = start
            .elapsed()
            .saturating_sub(Duration::from_millis(beat.load(Ordering::Relaxed)));
        if Instant::now() >= deadline {
            kill_process_tree(&mut child);
            return Err(TimedOut(tool.to_string(), timeout.as_secs(), None).into());
        }
        let stopped = stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(why) = stopped {
            kill_process_tree(&mut child);
            return Err(harness::Stopped(why).into());
        }
        if idle.is_some_and(|limit| silent >= limit) {
            kill_process_tree(&mut child);
            let secs = idle.map_or(0, |d| d.as_secs());
            return Err(IdleStalled(tool.to_string(), secs).into());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let stdout = reader.join().unwrap_or_default();
    let output = std::process::Output {
        status,
        stdout,
        stderr: Vec::new(),
    };
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

/// Every descendant pid of `root`, from one `ps` snapshot (empty where `ps` is absent).
fn descendants(root: u32) -> Vec<u32> {
    let Ok(out) = Command::new("ps").args(["-axo", "pid=,ppid="]).output() else {
        return Vec::new();
    };
    let pairs: Vec<(u32, u32)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace().map(|n| n.parse::<u32>().ok());
            Some((f.next()??, f.next()??))
        })
        .collect();
    let mut found = vec![root];
    let mut i = 0;
    while i < found.len() {
        let parent = found[i];
        found.extend(pairs.iter().filter(|p| p.1 == parent).map(|p| p.0));
        i += 1;
    }
    found.split_off(1)
}

/// Stop an agent and everything it spawned: SIGTERM the tree, give it two
/// seconds to leave, then SIGKILL what remains. Killing only the direct child
/// leaves its helpers holding the stdout pipe and burning quota.
fn kill_process_tree(child: &mut Child) {
    let mut pids = descendants(child.id());
    pids.push(child.id());
    let signal = |sig: &str, pids: &[u32]| {
        let _ = Command::new("kill")
            .arg(sig)
            .args(pids.iter().map(u32::to_string))
            .stderr(Stdio::null())
            .status();
    };
    signal("-TERM", &pids);
    let grace = Instant::now() + Duration::from_secs(2);
    while Instant::now() < grace && child.try_wait().ok().flatten().is_none() {
        std::thread::sleep(Duration::from_millis(50));
    }
    signal("-KILL", &pids);
    let _ = child.kill();
    let _ = child.wait();
}

/// One prepared OpenAI-compatible chat-completions request: everything but
/// the prompt, resolved once so every retry hits the same endpoint, model and
/// key.
///
/// The key is read from the environment at call time and appears nowhere the
/// system can echo it: not in argv (world-readable via `ps` / /proc), not in
/// a log line, not in an error. curl gets it through `--config -` on stdin,
/// and the request body travels as a private temp file rather than an
/// argument (it embeds the datasheet text, and can outgrow `ARG_MAX` anyway).
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
        Ok(Self {
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
        self.send_with_usage(prompt).map(|(text, _)| text)
    }

    /// [`Self::send`], with the token usage the endpoint reported (`None` when it gave none).
    fn send_with_usage(
        &self,
        prompt: &str,
    ) -> Result<(String, Option<crate::extract_usage::Usage>)> {
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
        let text = resp["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_owned)
            .context("missing content in API response")?;
        Ok((text, crate::extract_usage::from_chat_reply(&resp)))
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
    parse_and_validate_all(raw, part, kind_str).map(|mut all| all.swap_remove(0))
}

/// Parse raw TOML and validate every model entry it holds (never empty).
fn parse_and_validate_all(raw: &str, part: &str, kind_str: &str) -> Result<Vec<ModelEntry>> {
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
    if let Some(v) = toml::from_str::<toml::Value>(&text).ok().as_ref() {
        if let Some(misplaced) = crate::validation::misplaced_param_keys(v).first() {
            bail!("{misplaced}");
        }
    }
    if let Some(problem) = crate::plausibility::raw_problems(&text).first() {
        bail!("implausible value: {problem}");
    }
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
    let all: Vec<ModelEntry> = std::iter::once(first).chain(entries).collect();
    if !names_part(&all, part) {
        bail!(
            "no [models.match] value_re or mpn_re matches '{part}', the name this card was \
             requested for, so a board naming the part that way would never bind it. Widen the \
             pattern to accept '{part}' as well as the ordering codes it already accepts: for \
             example value_re = '{}', ending in (?:[-_ /]|$) only where a longer sibling part \
             number shares that prefix",
            names_part_pattern(part)
        );
    }
    if let Some(problem) = crate::variants::lint(&all).into_iter().next() {
        bail!("variant set in the reply for {part}: {problem}");
    }
    if let Some(problem) = ambiguous_cards(&all, part) {
        bail!("{problem}");
    }
    let blocking: Vec<String> = crate::card_coverage::current_reply_findings(&all)
        .into_iter()
        .filter(|f| f.severity == crate::card_coverage::Severity::Error)
        .map(|f| format!("{}: {}", f.check, f.message))
        .collect();
    if !blocking.is_empty() {
        bail!("{}", blocking.join("\n"));
    }
    Ok(all)
}

/// The base card kind a requested kind drafts as (`charger` -> `vreg`).
#[must_use]
pub fn base_kind_name(kind: &str) -> Option<&'static str> {
    behavioral_family_base_kind(kind)
}

/// What `ds check` shows about a draft: the validator's verdict on it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DraftCheck {
    /// The validation error that refuses the draft, if one does.
    pub refusal: Option<String>,
    /// The lint warnings, binding, pin-table and coverage findings a passing
    /// draft still draws (what makes the harness redraft it).
    pub warnings: Vec<String>,
    /// What the harness rewrites in the draft itself before it checks it (a
    /// missing model-error record, a match pattern's end anchor, derived params):
    /// the verdict above is on the rewritten card.
    pub repairs: Vec<String>,
}

/// Everything the drafting loop would say about `raw` against `facts`; what `ds check` prints.
///
/// Exactly the harness's own steps in its own order, through the same functions
/// (`Reply::complete`, then `Reply::assess`): the agent is shown every
/// refusal and warning the post-session validator will find, while it can fix it.
#[must_use]
pub fn check_draft(
    raw: &str,
    part: &str,
    kind: &str,
    facts: &crate::card_coverage::Facts,
) -> DraftCheck {
    let reply = Reply::Model { part, kind };
    crate::card_coverage::with_facts(facts.clone(), || {
        let (completed, said) = progress::capture(|| reply.complete(raw.to_string()));
        let repairs = said
            .iter()
            .map(|l| l.trim_start_matches("[model-extract] ").to_string())
            .collect();
        match reply.assess(&completed, true) {
            Err(e) => DraftCheck {
                refusal: Some(format!("{e:#}")),
                warnings: Vec::new(),
                repairs,
            },
            Ok(warnings) => DraftCheck {
                refusal: None,
                warnings,
                repairs,
            },
        }
    })
}

/// Cards in one reply that a board could not tell apart: a repeated id (the
/// second shadows the first), or two independent cards (no `variant_of`, no
/// `footprint_re`) that both accept the requested name, so which one binds
/// depends on file order. Measured: two package cards both `id = "max17048"`.
fn ambiguous_cards(entries: &[ModelEntry], part: &str) -> Option<String> {
    let mut seen = std::collections::BTreeSet::new();
    if let Some(dup) = entries.iter().find(|e| !seen.insert(e.id.as_str())) {
        return Some(format!(
            "two [[models]] entries share id '{}': every card needs its own id (a package or \
             variant suffix such as '{}_<package>')",
            dup.id, dup.id
        ));
    }
    let loose: Vec<&str> = entries
        .iter()
        .filter(|e| e.variant_of.is_none() && e.r#match.footprint_re.is_none())
        .filter(|e| names_part(std::slice::from_ref(*e), part))
        .map(|e| e.id.as_str())
        .collect();
    (loose.len() > 1).then(|| {
        format!(
            "cards {} all accept '{part}' and nothing on the board tells them apart, so the \
             one that binds would depend on file order: make them one variant set (shared \
             variant_of with [models.variant]), gate each by footprint_re, or narrow each \
             value_re to its own suffix",
            loose.join(", ")
        )
    })
}

/// A start-anchored pattern accepting `part` and every longer ordering code it
/// begins: the corrected value a names-part refusal quotes. Measured: told only
/// to "widen the pattern", a drafter returned the same `^BSS84-(7|13)-F$` twice.
fn names_part_pattern(part: &str) -> String {
    format!("(?i)^{}", regex::escape(part.trim()))
}

/// Whether some entry's `value_re` or `mpn_re` accepts `part` itself (an
/// entry with neither names it by other means). Measured: a draft matched
/// only the tape-and-reel code `<part>R`, so the requested name never bound.
fn names_part(entries: &[ModelEntry], part: &str) -> bool {
    let part = part.trim();
    part.is_empty()
        || entries.iter().any(|e| {
            let rules = &e.r#match;
            let patterns = [rules.value_re.as_deref(), rules.mpn_re.as_deref()];
            let mut patterns = patterns.into_iter().flatten().peekable();
            patterns.peek().is_none()
                || patterns.any(|p| {
                    regex::Regex::new(&format!("(?i){p}")).is_ok_and(|re| re.is_match(part))
                })
        })
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
    let implausible = crate::plausibility::problems(entry);
    if !implausible.is_empty() {
        bail!("implausible values: {}", implausible.join(" | "));
    }
    let refit = crate::param_names::diode_refit(entry).map(|[is, n, rs]| format!("diode is/n/rs are absent or over 10% off the cited VF rows, which fit is = {is:.4e}, n = {n:.4}, rs = {rs:.4}"));
    if let Some(why) = refit.or_else(|| crate::param_names::ron_miss(entry)) {
        bail!("{why}");
    }
    crate::validation::validate(entry).map_err(|errs| {
        let msg: Vec<String> = errs.iter().map(ToString::to_string).collect();
        anyhow::anyhow!("validation failed: {}", msg.join("; "))
    })?;
    let problems = crate::validation::draft_pin_map_problems(entry);
    if !problems.is_empty() {
        bail!("pin map: {}", problems.join("; "));
    }
    let unrated = crate::validation::pin_abs_max_problems(entry);
    if !unrated.is_empty() {
        bail!("pin absolute maxima: {}", unrated.join("; "));
    }
    if let Some(problem) = crate::validation::adjustable_seed_problem(entry) {
        bail!("{problem}");
    }
    // The lint checks the behaviour block too; a card it would refuse is no draft.
    if !entry.logic.is_empty() {
        entry
            .logic
            .validate()
            .map_err(|e| anyhow::anyhow!("[models.logic]: {e}"))?;
    }
    Ok(())
}

fn supply_relative_rows(source: Option<&crate::schema::ModelSourceSpec>) -> Result<()> {
    let mut rows = source.into_iter().flat_map(|s| &s.characteristic);
    let problem = rows.find_map(crate::schema::Characteristic::supply_relative_problem);
    problem.map_or(Ok(()), |e| Err(anyhow::anyhow!(e)))
}

/// The `snake_case` name a kind serialises as (the serde `rename_all`).
fn kind_discriminant(kind: ComponentKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// A part name as a file stem: anything but alphanumerics, `-` and `_` becomes `_`.
#[must_use]
pub fn sanitise_filename(s: &str) -> String {
    s.replace(|c: char| !(c.is_alphanumeric() || "-_".contains(c)), "_")
}

/// Where a draft lands without `--out-dir`: the user model directory.
#[must_use]
pub fn default_out_dir() -> PathBuf {
    dirs_next().join(".hauksbee").join("models")
}

/// The user's home, by whichever name this OS gives it. An empty value is not
/// a home; falling through to the temp dir keeps a model written to a
/// well-defined place instead of `/.hauksbee`.
fn dirs_next() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .map_or_else(std::env::temp_dir, PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    static MOCK_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
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
    fn reply_schemas_are_strict_and_parse_back() {
        // Strict structured output requires every property named as required
        // and no others allowed; a lax schema is refused by the backend.
        for schema in [identify_schema(), envelope_schema()] {
            let props = schema["properties"].as_object().unwrap();
            let required = schema["required"].as_array().unwrap();
            assert_eq!(props.len(), required.len(), "{schema}");
            assert_eq!(schema["additionalProperties"], false);
        }
        let kinds = identify_schema()["properties"]["kind"]["enum"].clone();
        assert!(kinds.as_array().unwrap().iter().any(|k| k == "unsupported"));
        assert_eq!(kinds_named(r#"{"kind":"vreg"}"#), ["vreg"]);
        let ws = Workspace::scratch().unwrap();
        note_envelope(
            &ws,
            r#"{"status":"written","confidence":"low","reason":"two packages","questions":[]}"#,
        );
        assert_eq!(harness::confidence(ws.path()).0, "low");
    }

    /// Claude Code is asked for the envelope in the prompt, not through
    /// `--json-schema` (a forced extra turn per agent), so a fenced or
    /// sentence-wrapped envelope must still be read, and a non-envelope not.
    #[test]
    fn claude_envelope_comes_from_the_prompt_and_is_checked_locally() {
        let dir = tempfile::tempdir().unwrap();
        let (drafting, identify) = (dir.path().join("a.json"), dir.path().join("b.json"));
        std::fs::write(&drafting, envelope_schema().to_string()).unwrap();
        std::fs::write(&identify, identify_schema().to_string()).unwrap();
        let note = envelope_note(&drafting);
        assert!(
            note.contains("written | identity_only | not_written"),
            "{note}"
        );
        assert!(note.contains("low while the last checker run still reports a finding"));
        assert_eq!(envelope_note(&identify), "");
        assert_eq!(envelope_note(&dir.path().join("none.json")), "");
        assert_eq!(schema_flag(Backend::ClaudeCode, Path::new("claude")), None);
        let body = r#"{"status":"written","confidence":"medium","reason":"r","questions":["q"]}"#;
        for text in [body.to_string(), format!("Done.\n```json\n{body}\n```")] {
            assert_eq!(
                envelope_in(&text).unwrap()["confidence"],
                "medium",
                "{text}"
            );
        }
        assert!(envelope_in(r#"{"confidence":"certain"}"#).is_none());
        assert!(envelope_in("model.toml written").is_none());
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

    /// A pin table split over many contents entries must not crowd the thermal
    /// section out of a tight page budget: each section's first page comes first.
    #[test]
    fn every_key_section_gets_a_page_before_any_gets_a_second() {
        let entry = |title: &str, pdf| crate::ds_toolkit::TocEntry {
            title: title.into(),
            printed: pdf,
            pdf,
        };
        let mut toc: Vec<_> = (10..16)
            .map(|p| entry("Pin definitions (continued)", p))
            .collect();
        toc.push(entry("Thermal characteristics", 90));
        let first = SelectedPage::new(1, vec![], vec![], usize::MAX);
        let picked: Vec<usize> = with_key_sections(vec![first], &toc, 4)
            .iter()
            .map(|p| p.number)
            .collect();
        assert_eq!(picked, [1, 10, 11, 90]);
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
        assert!(bjt.contains("never from VBE(sat)") && bjt.contains("or hFE gain group, emit one"));
        assert!(bjt.contains("takes no tt: its trr is capacitive recovery"));
        assert!(bjt.contains("is, bf, nf (no VBE(on) row or curve: an `unknown` record each)"));
        let switch = build_prompt("SWITCH_FIXTURE", "analog_switch", "x");
        assert!(switch.contains("off-resistance row at all: an `unknown` record for roff"));
        assert!(switch.contains("add ron_s0/ron_s1 for the path each leg role names"));
        assert!(switch.contains("s0 is the throw joined to com while ctrl is LOW, s1"));
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
        assert!(vreg.contains("junction temperature limit, never a thermal-shutdown"));
        assert!(vreg.contains("absolute maximum says internally limited, it"));
        assert!(bjt.contains("   - bf: 1 to 2000\n"));
        assert!(bjt.contains("(.MODEL/.SUBCKT) is copied verbatim"));
        assert!(bjt.contains("derives tf, cjc"));
        assert!(bjt.contains("rated IOUT") && bjt.contains("least favourable row the part"));
        assert!(bjt.contains("one characteristic per printed row with `rating = \"<key>\"`"));
        assert!(bjt.contains("Hauksbee sets each tagged key itself to the row at the standard"));
        // Each convention the code applies is named in its kind's prompt, no other's.
        for (kind, rule) in [
            ("diode", "a single VF row"),
            ("analog_switch", "(5.5 V / 1 uA = 5.5 MΩ"),
            ("digital", "70 % VDDIO"),
            ("comparator", "largest printed overdrive"),
            ("charger", "supply current while charging"),
            ("opamp", "headroom from the supply, swing_hi/swing_lo"),
        ] {
            let prompt = build_prompt("X_FIXTURE", kind, "x");
            assert!(prompt.contains(rule), "{kind}");
        }
        assert!(!bjt.contains("Conventions (cite the row"));
        assert!(bjt.contains("one row per path, `pins` naming its\n[models.pins] roles"));
        assert!(vreg.contains("at the rated output current when several loads are printed"));
        let nmos = build_prompt("FET_FIXTURE", "nmos", "x");
        assert!(nmos.contains("   - kp: 1e-6 to 1000\n") && nmos.contains("cgd, is, cbd"));
        assert!(nmos.contains("1/(kp*(|VGS| - |vto|)) + rd + rs"));
        let comparator = build_prompt("CMP_FIXTURE", "comparator", "x");
        assert!(comparator.contains("hysteresis = 0.0") && comparator.contains("leak_s: 0 to 1"));
        assert!(comparator.contains("(VT+ minus VT-), never the input offset"));
        assert!(comparator.contains("VICR/CMVR row, emit one input_common_mode per channel"));
        assert!(comparator.contains("input_common_mode per printed temperature row, each with its"));
        assert!(comparator.contains("(min/max) cell, never typ (typ alone: omit the entry)"));
        let logic = build_prompt("LVC_FIXTURE", "digital", "x");
        assert!(logic.contains("fixed voltage whatever the supply (a 5 V-tolerant input"));
        assert!(logic.contains("never an output or a bidirectional I/O"));
        assert!(bjt.contains("Identity-only is a last resort. A typ-only or max-only row"));
        assert!(bjt.contains("P7 numbers it; else\n   key by lead letters A/K, G/S/D"));
        assert!(bjt.contains("one missing"));
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
    fn identification_retries_prose_and_refusals() {
        for good in ["vreg", "digital\n", "unsupported"] {
            assert!(check_kind_reply(good).is_ok(), "{good}");
        }
        let refusal = "I can't complete the task because the workspace is under \
                       `/private/var/folders/x/T`, which the active filesystem policy denies";
        let err = format!("{:#}", check_kind_reply(refusal).unwrap_err());
        assert!(err.contains("readable"), "{err}");
        assert!(check_kind_reply("either vreg or digital").is_err());
        let fix = retry_feedback(refusal, &err, "", false);
        assert!(fix.contains("YOUR PREVIOUS REPLY WAS: I can't") && fix.contains("one kind"));
        assert!(IDENTIFY_CLAUSE.contains("page-index.md"));
        // The refusal is retried with its reason, not taken as the answer.
        let mut prompts = Vec::new();
        let word = validated_attempts(1, "p", Reply::Word, ("codex", false), |p| {
            prompts.push(p.to_string());
            Ok(if prompts.len() == 1 { refusal } else { "vreg" }.into())
        })
        .unwrap();
        assert_eq!(word, "vreg");
        assert!(prompts[1].contains("readable"), "{}", prompts[1]);
    }

    #[test]
    fn a_card_must_match_the_name_it_was_requested_for() {
        let card = |value_re: Option<&str>| {
            let mut e = ModelEntry::default();
            e.r#match.value_re = value_re.map(str::to_string);
            e
        };
        let reel_only = card(Some("^DRV1234DDAR$"));
        assert!(!names_part(std::slice::from_ref(&reel_only), "DRV1234DDA"));
        assert!(names_part(
            &[reel_only, card(Some("^drv1234dda(r)?$"))],
            "DRV1234DDA"
        ));
        assert!(names_part(&[card(None)], "DRV1234DDA"));
        assert!(names_part(&[card(Some("^X$"))], ""));
    }

    #[test]
    fn prompt_shows_switcher_and_driver_shapes() {
        let vreg = build_prompt("REG_FIXTURE", "vreg", "x");
        assert!(vreg.contains(SWITCHER_HINT) && !vreg.contains("LIBRARY PATTERNS"));
        let digital = build_prompt("LOGIC_FIXTURE", "digital", "x");
        assert!(digital.contains(SUPPLY_DRAW_HINT));
        assert!(!build_prompt("BC847", "bjt_npn", "x").contains(SUPPLY_DRAW_HINT));
        // A front page naming a buck brings the library's converter cards,
        // ahead of the datasheet text.
        let buck = build_prompt("REG_FIXTURE", "vreg", "2-A step-down buck converter");
        let (patterns, text) = (
            buck.find("LIBRARY PATTERNS"),
            buck.find(DATASHEET_TEXT_BEGIN),
        );
        assert!(
            patterns.zip(text).is_some_and(|(p, t)| p < t),
            "{patterns:?} {text:?}"
        );
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
        let sensor = verification_clause(&ws, "[sensor]", true);
        assert!(sensor.contains("[[sensor.source.uncertainty]]"));
        assert!(!sensor.contains("models.source") && !sensor.contains("identity-only"));
        let footer = verification_clause(&ws, "[[models]]", true);
        let needles = "Prefer the supplied authoritative schema and datasheet \
            | Use ONLY the files in your working directory | No checker is provided \
            | Never let an external or stale schema override the supplied contract \
            | abstain if the available evidence cannot resolve it";
        assert!(needles.split(" | ").all(|n| footer.contains(n)), "{footer}");
        assert!(!footer.contains("vendor source") && !footer.contains("diagnostic tools"));
        let mut ws = ws;
        ws.checker = Some("/opt/x/hauksbee".into());
        let footer = verification_clause(&ws, "[[models]]", true);
        assert!(footer.contains("`'/opt/x/hauksbee' models lint model.toml`"));
        assert!(footer.contains("except its `is omitted but its cited row gives` lines"));
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
        // The examples are the shared base; the part and its text come after them.
        let prompt = build_prompt_parts("TXB0101", "digital", "datasheet text", &[]);
        let start = prompt.base.find("FORMAT EXAMPLES ONLY").unwrap();
        assert!(!prompt.base[start..].contains("TXB0101"));
        assert!(prompt.tail.contains("DATASHEET TEXT (truncated):"));
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
        assert!(retry_feedback("digital", "no table", "[[models]]", false)
            .contains("--- BEGIN REPLY ---\ndigital\n"));
        // A missing required param offers the honest exits, never "produce a number".
        let card = FORMAT_EXAMPLES[0].card;
        let missing = retry_feedback(card, "missing required param 'vout'", "[[models]]", false);
        assert!(missing.contains("NOT a request") && missing.contains("identity_only"));
        assert!(!retry_feedback(card, "no table", "[[models]]", false).contains("identity_only"));
        // A whole-table rewrite is the unstaged ask; a staged run edits the draft in place.
        let whole = retry_feedback(card, "bad", "[[models]]", false);
        assert!(whole.contains("write the corrected WHOLE `[[models]]` TOML table"));
        assert!(!whole.contains(harness::EDIT_IN_PLACE));
        let edit = retry_feedback(card, "bad", "[[models]]", true);
        assert!(edit.contains(harness::EDIT_IN_PLACE) && !edit.contains("WHOLE"));
        let edit = retry_feedback(card, "missing required param 'vout'", "[[models]]", true);
        assert!(
            edit.contains("NOT a request"),
            "the honest exits still follow"
        );
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
[[models.source.pinout]]
package = "UHF"
pins = { "36" = "PVIN", "20" = "BAT", "25" = "ILIMIT" }
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
        let pins =
            "[models.pins]\n\"1\" = \"in\"\n\"2\" = \"out\"\n\"3\" = \"prog\"\n\"4\" = \"gnd\"\n";
        let column = "[[models.source.pinout]]\npackage = \"DFN-4\"\npins = { \"1\" = \"IN\", \"2\" = \"OUT\", \"3\" = \"PROG\", \"4\" = \"GND\" }\n";
        let linear = format!("{plain}{pins}{snippet}{MODEL_SOURCE}{column}");
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
        let (raw, _) = call_backend(&Prompt::from(prompt), &args, Reply::Model { part, kind })
            .expect("backend");
        let entry = parse_and_validate_reply(&raw, part, kind).unwrap();
        (raw, entry)
    }

    fn live_backend_available() -> bool {
        let loaded = extract_config::load().expect("valid extraction config");
        let resolved = loaded.config.resolve(&ProcessHost, None);
        resolved.status(&ProcessHost, resolved.backend).available
    }

    /// Paid-call opt-in for the `*_live` tests. They call external model APIs
    /// that cost money, so they run only when `HAUKSBEE_PAID_LIVE_TESTS=1` is
    /// set; otherwise this prints a SKIP line and the caller returns at once.
    fn paid_live_tests_enabled(test: &str) -> bool {
        if std::env::var("HAUKSBEE_PAID_LIVE_TESTS").as_deref() == Ok("1") {
            return true;
        }
        eprintln!("SKIP {test}: paid live test; set HAUKSBEE_PAID_LIVE_TESTS=1 to run it");
        false
    }

    /// Live: the real backend against the BC846-series datasheet in testdata.
    /// Paid: calls the external model API. Runs only with
    /// `HAUKSBEE_PAID_LIVE_TESTS=1` set; otherwise it prints SKIP and returns.
    /// Run with
    /// `HAUKSBEE_PAID_LIVE_TESTS=1 cargo test -p hauksbee-models -- extract_bc846_live --ignored --nocapture`.
    ///
    /// The part asked for is the part the sheet covers. Asking it for a BC847
    /// is the shape of request that produces a wrong number rather than no
    /// number: the BC846 series sheet states 65 V VCEO for its own types and
    /// says nothing at all about the 45 V BC847, so a model that answers
    /// anyway has substituted a rating from a different device.
    #[test]
    #[ignore = "live: needs a signed-in extraction backend"]
    fn extract_bc846_live() {
        if !paid_live_tests_enabled("extract_bc846_live") {
            return;
        }
        let pdf = testdata("datasheets/BC846.pdf");
        assert!(pdf.exists(), "BC846 datasheet not found at {pdf:?}");
        if !live_backend_available() {
            return;
        }
        let text = extract_pdf_text(&pdf);
        let (_, entry) = draft(pdf, &text, "BC846", "bjt_npn");
        let bf = entry.params.get_f64("bf").expect("bf present");
        assert!(
            (100.0..=460.0).contains(&bf),
            "bf {bf} outside the BC846 hFE band"
        );
        assert_eq!(entry.ratings.max_voltage_v, Some(65.0));
    }

    /// Live: `--kind charger` against the LTC4020 excerpt in testdata.
    /// Paid: calls the external model API. Runs only with
    /// `HAUKSBEE_PAID_LIVE_TESTS=1` set; otherwise it prints SKIP and returns.
    #[test]
    #[ignore = "live: needs a signed-in extraction backend"]
    fn extract_ltc4020_charger_live() {
        if !paid_live_tests_enabled("extract_ltc4020_charger_live") {
            return;
        }
        let src = testdata("datasheets/LTC4020_excerpt.txt");
        if !src.exists() || !live_backend_available() {
            return;
        }
        let text = extract_pdf_text(&src);
        let (_, entry) = draft(src, &text, "LTC4020", "charger");
        assert_eq!(entry.kind, crate::ComponentKind::Vreg);
        let c = entry.behavioral.converter.expect("converter block");
        assert_eq!(c.in_pin, "pvin");
        assert_eq!(c.out_pin, "bat");
        assert!(c.iin_program.is_some());
    }

    const MODEL_SOURCE: &str = "[models.source]\ntier = \"datasheet-derived\"\n\
        uncertainty = [{ status = \"unknown\", parameter = \"model\", reason = \"none\" }]\n";

    /// A drafted `value_re` loses its end anchor, whatever the pattern's shape; a
    /// variant's pattern and an escaped `\$` stay.
    #[test]
    fn completion_unanchors_the_end_of_a_match_pattern() {
        let card = |re: &str, extra: &str| {
            format!("[[models]]\nid = \"q\"\nkind = \"diode\"\n{extra}[models.match]\nvalue_re = '{re}'\nmpn_re = '^Q1$'\n{MODEL_SOURCE}")
        };
        let model = Reply::Model {
            part: "Q",
            kind: "diode",
        };
        let patterns = |done: &str| {
            let db: crate::schema::DbFile = toml::from_str(done).expect("completed card parses");
            let m = db.models[0].r#match.clone();
            (m.value_re.unwrap_or_default(), m.mpn_re.unwrap_or_default())
        };
        let done = model.complete(card("^(?:Q)$", ""));
        assert_eq!(patterns(&done), ("^(?:Q)".into(), "^Q1".into()));
        let done = model.complete(card(r"^Q\$", ""));
        assert_eq!(patterns(&done).0, r"^Q\$");
        let member = "variant_of = \"q\"\nvariant = { orderable_re = '^QX$', orderable = [\"QX\"], basis = \"p. 2\" }\n";
        let done = model.complete(card("^Q$", member));
        assert_eq!(patterns(&done).0, "^Q$");
        assert_eq!(
            crate::validation::without_end_anchor("(?i)^PART$"),
            Some("(?i)^PART")
        );
        assert_eq!(crate::validation::without_end_anchor("^PART"), None);
    }

    /// A draft the checker accepts but the lint warns about is redrafted once with
    /// the warnings quoted; the redraft's failure leaves the accepted draft standing.
    #[test]
    fn lint_warnings_are_fed_back_like_validation_errors() {
        let card = |tag: &str| {
            format!("[[models]]\nid = \"q\"\nkind = \"diode\"\ndescription = \"d\"\n[models.params]\nidentity_only = true\nwarning = \"w\"\nunlocked_by = \"u\"\n[models.pins]\n\"1\" = \"anode\"\n\"2\" = \"cathode\"\n{MODEL_SOURCE}[[models.source.characteristic]]\nparameter = \"PD power dissipation\"\nmax = 250.0\nunit = \"mW\"\nconditions = \"TA = 25 °C\"\nbasis = \"b\"\n{tag}")
        };
        let (bad, good) = (card(""), card("rating = \"max_power_w\"\n"));
        let model = Reply::Model {
            part: "Q",
            kind: "diode",
        };
        let seen = std::cell::RefCell::new(Vec::new());
        let run = |second: Result<String>| {
            seen.borrow_mut().clear();
            validated_attempts(1, "BASE", model, ("t", false), |p| {
                let mut seen = seen.borrow_mut();
                seen.push(p.to_string());
                if seen.len() == 1 {
                    Ok(bad.clone())
                } else {
                    second
                        .as_ref()
                        .map(Clone::clone)
                        .map_err(|e| anyhow::anyhow!("{e}"))
                }
            })
        };
        let out = run(Ok(good)).unwrap();
        assert!(out.contains("rating = \"max_power_w\""), "{out}");
        let prompts = seen.borrow().clone();
        assert_eq!(prompts.len(), 2);
        assert!(prompts[1].contains("lint warns") && prompts[1].contains("prints a rating"));
        let out = run(Err(anyhow::anyhow!("backend down"))).unwrap();
        assert_eq!(out, model.complete(bad.clone()));
        assert_eq!(seen.borrow().len(), 2);
        let out = run(Ok("not a card".into())).unwrap();
        assert_eq!(out, model.complete(bad.clone()));
    }

    /// The drafting prompt asks for the board-check fields on the kinds that carry
    /// them, in the base the kind's prompts share, and not on a discrete part.
    #[test]
    fn the_prompt_asks_for_the_board_check_fields_by_kind() {
        let switch = build_prompt("SWITCH_FIXTURE", "analog_switch", "x");
        assert!(switch.contains("LOAD SWITCH") && switch.contains("[[models.supply_cap]]"));
        assert!(switch.contains("[models.pin_capability.open_drain]"));
        let driver = build_prompt("DRV_FIXTURE", "digital", "x");
        assert!(driver.contains("MOTOR DRIVER") && driver.contains("[models.pins] MUST be"));
        let mcu = build_prompt("MCU_FIXTURE", "mcu", "x");
        assert!(mcu.contains("ROM bootloader") && mcu.contains("xtal_cap_min_f"));
        let diode = build_prompt("D_FIXTURE", "diode", "x");
        assert!(!diode.contains("supply_cap") && !diode.contains("open_drain"));
        // In the cacheable base, not the part's tail.
        let parts = build_prompt_parts("SWITCH_FIXTURE", "analog_switch", "x", &[]);
        assert!(parts.full().contains("LOAD SWITCH"));
        let other = build_prompt_parts("OTHER_FIXTURE", "analog_switch", "y", &[]);
        assert_eq!(
            parts.full().split("THE PART").next(),
            other.full().split("THE PART").next()
        );
    }

    /// A load switch draft that passes the checker but leaves out its capacitors and
    /// open-enable behaviour is redrafted once with those warnings quoted; the
    /// redraft that states them stands, and `ds check` shows the same warnings.
    #[test]
    fn a_draft_missing_card_fields_is_redrafted_with_the_warning_quoted() {
        let card = |extra: &str, params: &str| {
            format!(
                "[[models]]\nid = \"xsw1\"\nkind = \"analog_switch\"\n\
                 description = \"XSW1 high-side load switch\"\n\
                 [models.match]\nvalue_re = \"(?i)^XSW1\"\n\
                 [models.params]\nron = 0.07\nroff = 5.0e7\nvth = 1.0\nen_on_level = \"high\"\n{params}\
                 [models.pins]\n\"1\" = \"in_out_b\"\n\"2\" = \"vss\"\n\"3\" = \"ctrl\"\n\"4\" = \"in_out_a\"\n\
                 {MODEL_SOURCE}\
                 [[models.source.pinout]]\npackage = \"SOT-23\"\n\
                 pins = {{ \"1\" = \"VIN\", \"2\" = \"GND\", \"3\" = \"ON\", \"4\" = \"VOUT\" }}\n{extra}"
            )
        };
        let bad = card("", "");
        let good = card(
            "[[models.supply_cap]]\nrole = \"in_out_b\"\nside = \"input\"\nmin_f = 1.0e-6\nbasis = \"p.19\"\n",
            "en_open = \"off\"\nen_open_basis = \"p.3\"\n",
        );
        let model = Reply::Model {
            part: "XSW1",
            kind: "analog_switch",
        };
        let warned = |raw: &str| {
            crate::card_coverage::with_facts(crate::card_coverage::Facts::default(), || {
                model.assess(&model.complete(raw.to_owned()), true)
            })
            .unwrap_or_else(|e| panic!("{e:#}"))
        };
        let found = warned(&bad);
        assert!(
            found.iter().any(|w| w.contains("no [[models.supply_cap]]"))
                && found.iter().any(|w| w.contains("open-pin")),
            "{found:?}"
        );
        assert!(warned(&good).is_empty(), "{:?}", warned(&good));
        let shown = check_draft(
            &bad,
            "XSW1",
            "analog_switch",
            &crate::card_coverage::Facts::default(),
        );
        assert!(
            shown.warnings.iter().any(|w| w.contains("supply_cap")),
            "{shown:?}"
        );

        let prompts = std::cell::RefCell::new(Vec::new());
        let out = validated_attempts(1, "BASE", model, ("t", false), |p| {
            prompts.borrow_mut().push(p.to_string());
            Ok(if prompts.borrow().len() == 1 {
                bad.clone()
            } else {
                good.clone()
            })
        })
        .unwrap();
        assert!(out.contains("[[models.supply_cap]]"), "{out}");
        let prompts = prompts.into_inner();
        assert_eq!(prompts.len(), 2);
        assert!(
            prompts[1].contains("lint warns")
                && prompts[1].contains("no [[models.supply_cap]]")
                && prompts[1].contains("en_open"),
            "{}",
            prompts[1]
        );
    }

    /// The card LM62460 drafted (ramp-20261021): a start-anchored `mpn_re`, and a
    /// `value_re` whose `$` sits inside a group. The run's log said its `mpn_re`
    /// "loses its end anchor ((?i)^LM62460)", quoting the already-rewritten pattern,
    /// which read as a false positive on a start-anchored pattern.
    #[test]
    fn a_start_anchored_pattern_is_not_reported_and_a_rewrite_names_both_patterns() {
        let card = |value_re: &str, mpn_re: &str| {
            format!("[[models]]\nid = \"lm62460qrphrq1\"\nkind = \"diode\"\n[models.match]\nvalue_re = '{value_re}'\nmpn_re = '{mpn_re}'\n{MODEL_SOURCE}")
        };
        let model = Reply::Model {
            part: "LM62460QRPHRQ1",
            kind: "diode",
        };
        let said = |raw: String| progress::capture(|| model.complete(raw));
        // The patterns the draft kept, as in the trace: nothing is rewritten or said.
        let kept = card("(?i)^(?:LM62460QRPHRQ|LM62460$)", "(?i)^LM62460");
        let (done, lines) = said(kept.clone());
        assert_eq!(done, kept);
        assert!(lines.iter().all(|l| !l.contains("anchor")), "{lines:?}");
        // The same draft with its `mpn_re` end-anchored: rewritten, and the line
        // quotes what was written and what it became.
        let (done, lines) = said(card("(?i)^(?:LM62460QRPHRQ|LM62460$)", "(?i)^LM62460$"));
        // (the rewrite writes the key's string in TOML's other quoting, hence the parse)
        let patterns = |toml: &str| {
            let db: crate::schema::DbFile = toml::from_str(toml).expect("card parses");
            let m = db.models[0].r#match.clone();
            (m.value_re, m.mpn_re)
        };
        assert_eq!(patterns(&done), patterns(&kept));
        let line = lines
            .iter()
            .find(|l| l.contains("anchor"))
            .unwrap_or_else(|| panic!("{lines:?}"));
        assert!(
            line.contains("mpn_re (?i)^LM62460$ ends with an end anchor")
                && line.contains("rewritten to (?i)^LM62460:"),
            "{line}"
        );
        // An end anchor that was a whole alternative leaves no empty alternative
        // (`^LM62460|` would bind every value).
        assert_eq!(
            crate::validation::without_end_anchor("(?i)^LM62460[-_ ]|$"),
            Some("(?i)^LM62460[-_ ]")
        );
        assert_eq!(
            crate::validation::without_end_anchor(r"^A\|$"),
            Some(r"^A\|")
        );
        assert_eq!(crate::validation::without_end_anchor("(?i)^LM62460"), None);
        assert_eq!(crate::validation::without_end_anchor(r"^LM\$"), None);
    }

    /// `ds check` is the harness's validator: its refusal and warnings are the
    /// ones the post-session loop acts on, for a refused card, a card the harness
    /// rewrites into a passing one, and one that draws a warning.
    #[test]
    fn ds_check_reports_what_the_post_session_validator_finds() {
        let facts = crate::card_coverage::Facts::default();
        let model = Reply::Model {
            part: "Q",
            kind: "diode",
        };
        let diode = |rows: &str| {
            format!("[[models]]\nid = \"q\"\nkind = \"diode\"\ndescription = \"d\"\n[models.params]\nidentity_only = true\nwarning = \"w\"\nunlocked_by = \"u\"\n[models.pins]\n\"1\" = \"anode\"\n\"2\" = \"cathode\"\n{MODEL_SOURCE}{rows}")
        };
        let row = "[[models.source.characteristic]]\nparameter = \"PD power dissipation\"\nmax = 250.0\nunit = \"mW\"\nconditions = \"TA = 25 °C\"\nbasis = \"b\"\n";
        let vf = "[[models.source.characteristic]]\nparameter = \"VF forward voltage\"\nmax = 0.9\nunit = \"V\"\nconditions = \"IF = 10 mA\"\nbasis = \"b\"\n";
        for raw in [
            diode(row),
            diode(&format!("{row}rating = \"max_power_w\"\n")),
            diode(vf),
            "kind = \"diode\"".to_string(),
        ] {
            let seen = crate::card_coverage::with_facts(facts.clone(), || {
                model.assess(&model.complete(raw.clone()), true)
            });
            let shown = check_draft(&raw, "Q", "diode", &facts);
            assert_eq!(
                shown.refusal,
                seen.as_ref().err().map(|e| format!("{e:#}")),
                "{raw}"
            );
            assert_eq!(shown.warnings, seen.unwrap_or_default(), "{raw}");
        }
        // The warning the lint test feeds back is shown in-session too, and the
        // harness's repair of an identity-only abstention its rows make needless is
        // reported as a repair, on a card that is not refused.
        let shown = check_draft(&diode(row), "Q", "diode", &facts);
        assert!(
            shown.warnings.iter().any(|w| w.contains("prints a rating")),
            "{shown:?}"
        );
        let shown = check_draft(&diode(vf), "Q", "diode", &facts);
        assert!(
            shown.refusal.is_none() && !shown.repairs.is_empty(),
            "{shown:?}"
        );
        // An end-anchored `value_re` is a repair the agent is told of while it can write it itself.
        let anchored = diode(row).replace(
            "[models.pins]",
            "[models.match]\nvalue_re = '^Q$'\n[models.pins]",
        );
        let shown = check_draft(&anchored, "Q", "diode", &facts);
        assert!(
            shown
                .repairs
                .iter()
                .any(|r| r.contains("^Q$") && r.contains("rewritten to ^Q")),
            "{shown:?}"
        );
    }

    /// A warning the agent was shown by `ds check` and left in the card is not
    /// redrafted (the redraft would repeat the session); one it had not seen still is,
    /// quoting only the unseen.
    #[test]
    fn a_warning_the_agent_was_shown_and_left_is_not_redrafted() {
        let card = |extra: &str| {
            format!("[[models]]\nid = \"q\"\nkind = \"diode\"\ndescription = \"d\"\n[models.params]\nidentity_only = true\nwarning = \"w\"\nunlocked_by = \"u\"\n[models.pins]\n\"1\" = \"anode\"\n\"2\" = \"cathode\"\n{MODEL_SOURCE}[[models.source.characteristic]]\nparameter = \"PD power dissipation\"\nmax = 250.0\nunit = \"mW\"\nconditions = \"TA = 25 °C\"\nbasis = \"b\"\n{extra}")
        };
        let (bad, good) = (card(""), card("rating = \"max_power_w\"\n"));
        let model = Reply::Model {
            part: "Q",
            kind: "diode",
        };
        let warnings =
            crate::card_coverage::with_facts(crate::card_coverage::Facts::default(), || {
                model.assess(&model.complete(bad.clone()), true).unwrap()
            });
        assert!(!warnings.is_empty());
        let run = |seen: std::collections::BTreeSet<String>| {
            let prompts = std::cell::RefCell::new(Vec::new());
            let out = validated_attempts_seeing(
                1,
                "BASE",
                model,
                ("t", false),
                |p| {
                    prompts.borrow_mut().push(p.to_string());
                    Ok(if prompts.borrow().len() == 1 {
                        bad.clone()
                    } else {
                        good.clone()
                    })
                },
                || seen.clone(),
            )
            .unwrap();
            (out, prompts.into_inner())
        };
        // Shown in-session and still there: one agent run, the card kept as written.
        let (out, prompts) = run(warnings.iter().cloned().collect());
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert_eq!(out, model.complete(bad.clone()));
        // Not shown: redrafted, as before.
        let (out, prompts) = run(std::collections::BTreeSet::default());
        assert_eq!(prompts.len(), 2);
        assert!(out.contains("rating = \"max_power_w\""));
        // Some shown: the redraft quotes only the ones that were not.
        let seen = std::iter::once(warnings[0].clone()).collect();
        let (_, prompts) = run(seen);
        let quoted = prompts.get(1).map_or(0, |p| p.matches("\n- ").count());
        assert_eq!(quoted, warnings.len() - 1, "{warnings:?}");
    }

    /// Completion writes a tagged rating's scalar from its 25 °C row, whatever
    /// cell the model picked, and leaves a scalar already there as it was.
    #[test]
    fn completion_sets_a_tagged_rating_to_its_standard_row() {
        let row = |v: f64, at: &str| {
            format!("[[models.source.characteristic]]\nparameter = \"ID\"\nrating = \"max_current_a\"\nmax = {v}\nunit = \"A\"\nconditions = \"{at}\"\nbasis = \"b\"\n")
        };
        let rows = row(5.7, "TA = 25 °C") + &row(4.7, "TA = 70 °C") + &row(40.0, "TC = 25 °C");
        let card = |scalar: f64| {
            format!("[[models]]\nid = \"q\"\nkind = \"nmos\"\n[models.ratings]\nmax_current_a = {scalar}\n{MODEL_SOURCE}{rows}")
        };
        let model = Reply::Model {
            part: "Q",
            kind: "nmos",
        };
        for picked in [4.7, 40.0, 5.7] {
            let done = model.complete(card(picked));
            let db: crate::schema::DbFile = toml::from_str(&done).expect("completed card parses");
            assert_eq!(
                db.models[0].ratings.max_current_a,
                Some(5.7),
                "{picked}: {done}"
            );
        }
        assert_eq!(model.complete(card(5.7)), card(5.7));
    }

    /// An identity-only diode whose one cited VF row fixes its law becomes that
    /// model (a convention, not a refusal); one with no row stays identity-only,
    /// and a junction maximum a row prints fills the omitted rating.
    #[test]
    fn completion_converts_a_needless_abstention_and_fills_the_junction_limit() {
        let row = |p: &str, v: &str, unit: &str, at: &str| {
            format!("[[models.source.characteristic]]\nparameter = \"{p}\"\nmax = {v}\nunit = \"{unit}\"\nconditions = \"{at}\"\nbasis = \"b\"\n")
        };
        let card = |rows: &str| {
            format!("[[models]]\nid = \"q\"\nkind = \"diode\"\ndescription = \"d\"\n[models.params]\nidentity_only = true\nwarning = \"w\"\nunlocked_by = \"u\"\n[models.pins]\n\"1\" = \"anode\"\n\"2\" = \"cathode\"\n{MODEL_SOURCE}{rows}")
        };
        let model = Reply::Model {
            part: "Q",
            kind: "diode",
        };
        let rows = row("VF forward voltage", "0.9", "V", "IF = 10 mA")
            + &row("TJ junction temperature", "150", "°C", "absolute maximum");
        let done = model.complete(card(&rows));
        let db: crate::schema::DbFile = toml::from_str(&done).expect("completed card parses");
        let entry = &db.models[0];
        assert!(entry.params.get_bool("identity_only").is_none(), "{done}");
        assert!(entry.params.get_f64("is").is_some() && entry.params.get_f64("n") == Some(1.0));
        assert_eq!(entry.ratings.max_junction_temp_c, Some(150.0));
        assert!(model.check(&done).is_ok(), "{done}");
        // No VF row: the abstention stands, nothing is derived.
        let bare = card(&row(
            "TJ junction temperature",
            "150",
            "°C",
            "absolute maximum",
        ));
        let kept = model.complete(bare);
        let db: crate::schema::DbFile = toml::from_str(&kept).expect("card parses");
        assert_eq!(db.models[0].params.get_bool("identity_only"), Some(true));
        assert!(db.models[0].params.get_f64("is").is_none());
    }

    #[test]
    // One table of reply fixtures, each beside the verdict it must get.
    #[allow(clippy::too_many_lines)]
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
pinout = [{ package = "SOT-363", pins = { "1" = "B1", "2" = "E1", "6" = "C1" } }]
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
        let (part, kind) = ("BCM847BS", "bjt_npn");
        let model = Reply::Model { part, kind };
        // (Lead-lettered pads: a numbered map would also need its package column.)
        let lettered = no_source.replace(
            "\"1\" = \"base\"\n\"2\" = \"emitter\"\n\"6\" = \"collector\"",
            "b = \"base\"\ne = \"emitter\"\nc = \"collector\"",
        );
        // (Two cards both accepting the requested name would be refused as
        // ambiguous, so the second names a sibling code.)
        let second = lettered
            .replace("\"bcm847bs\"", "\"b\"")
            .replace("(?i)BCM847BS", "(?i)BCM847BS-Q");
        let filled = model.complete(format!("{lettered}{second}"));
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
            let b = good
                .replace("\"bcm847bs\"", "\"bcm847bs_b\"")
                .replace("(?i)BCM847BS", "(?i)BCM847BS-Q");
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
            let v =
                (1.2 * hauksbee_ir::thermal_voltage_c(27.0)).mul_add((ma * 1e5).ln_1p(), ma * 5e-4);
            format!("[[models.source.characteristic]]\nparameter = \"VF\"\ntyp = {v}\nunit = \"V\"\nconditions = \"IF = {ma} mA\"\nbasis = \"b\"\n")
        };
        let rows = [1.0, 10.0, 100.0, 1000.0].map(vf).concat();
        let off = format!("[[models]]\nid = \"d\"\nkind = \"diode\"\n[models.match]\nvalue_re = \"D\"\n[models.params]\nis = 1e-14\nn = 1.0\nrs = 0.0\n{MODEL_SOURCE}{rows}");
        let error = parse_and_validate_reply(&off, "D", "diode").unwrap_err();
        assert!(format!("{error:#}").contains("over 10% off"), "{error:#}");
        // Two rows 3× apart are refit through both, so a law off them is refused.
        let thin = off.replace(&rows, &(vf(10.0) + &vf(30.0)));
        let thin = parse_and_validate_reply(&thin, "D", "diode").unwrap_err();
        assert!(format!("{thin:#}").contains("over 10% off"), "{thin:#}");
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
    // One mock run end to end: the wrapper script, its config, and every protocol it must see.
    #[allow(clippy::too_many_lines)]
    fn custom_command_runs_the_selected_protocol_and_never_falls_back() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = MOCK_ENV.lock().unwrap();
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let command = dir.path().join("custom wrapper");
        std::fs::write(
            &command,
            "#!/bin/sh\ncat >/dev/null\nif grep -q 'connectivity check' prompt.md; then printf 'OK\\n' > model.toml; fi\nprintf '%s\\n' \"$@\"\nprintf 'wrapper stderr' >&2\nprintf '%s' \"$CLAUDE_CODE_DISABLE_BACKGROUND_TASKS$CLAUDE_CODE_DISABLE_AUTO_MEMORY\" > env.txt\n{ x; hauksbee lint; } > tools.txt\npwd -P > cwd.txt\n",
        )
        .unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut cfg = extract_config::ExtractConfig::default();
        cfg.set("claude-code.command", command.to_str().unwrap())
            .unwrap();
        cfg.set("claude-code.model", "gpt-6.1-sol").unwrap();
        cfg.set("claude-code.effort", "high").unwrap();
        let mut resolved = cfg.resolve(
            &extract_config::FakeHost::default(),
            Some(Backend::ClaudeCode),
        );
        assert_eq!(which_path(command.to_str().unwrap()), Some(command.clone()));
        require_tool(&resolved).unwrap();
        let mut ws = Workspace::scratch().unwrap();
        ws.checker = Some(dir.path().join("hauksbee"));
        std::fs::create_dir(dir.path().join("t")).unwrap();
        for (file, out) in [("hauksbee", "ok $1"), ("t/hauksbee", "no"), ("t/x", "hi")] {
            let file = dir.path().join(file);
            std::fs::write(&file, format!("#!/bin/sh\necho {out}\n")).unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let old_path = std::env::var("PATH").unwrap();
        std::env::set_var("PATH", format!("{}/t:{old_path}", dir.path().display()));
        let settings = claude_confinement(&ws);
        let sandbox: serde_json::Value = serde_json::from_str(&settings).unwrap();
        let fs = &sandbox["sandbox"]["filesystem"];
        assert_eq!(sandbox["sandbox"]["allowUnsandboxedCommands"], false);
        assert!(settings.contains(r#""denyRead":["~/","/Users","/home","/Volumes","#));
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
                    "stream-json",
                    "--verbose",
                    "--permission-mode",
                    mode,
                    "--model",
                    "gpt-6.1-sol",
                    "--effort",
                    "high",
                    "--no-session-persistence",
                    "--restricted",
                    "--strict-mcp-config",
                    "--tools",
                    "Bash,Read,Edit,Write",
                    "--settings",
                    &settings,
                    "--exclude-dynamic-system-prompt-sections"
                ]
            );
            let read = |name: &str| std::fs::read_to_string(ws.path().join(name)).unwrap();
            assert_eq!(read("env.txt"), "11", "no background tasks or auto memory");
            assert_eq!(read("tools.txt"), "hi\nok lint\n"); // not t/hauksbee
            assert_eq!(read("cwd.txt").trim_end(), ws.path().to_str().unwrap());
        }
        std::env::set_var("PATH", old_path);
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
        let [confine, read_only] = [true, false].map(|w| codex_confinement(&ws, w).to_vec());
        for (args, access) in [(&confine, "write"), (&read_only, "read")] {
            let profile: toml::Table = toml::from_str(&args[3]).expect("valid codex -c value");
            let fs = &profile["permissions"]["hauksbee"]["filesystem"];
            let checker = dir.path().join("hauksbee").display().to_string();
            let got = [&fs[&checker], &fs[":tmpdir"], &fs[":workspace_roots"]["."]];
            assert_eq!(got.map(|v| v.as_str().unwrap()), ["read", "none", access]);
        }
        let approve = [vec!["--approve-for-me".into()], confine.clone()].concat();
        let full = vec!["--sandbox".into(), "danger-full-access".into()];
        let modes = [
            ("read-only", read_only),
            ("workspace-write", confine),
            ("approve-for-me", approve),
            ("danger-full-access", full),
        ];
        for (mode, permission_args) in modes {
            resolved.codex.permission_mode = mode.into();
            let output = run_agent_once(&resolved, "fake prompt", &ws).unwrap();
            let mut expected = vec![
                "exec".to_string(),
                "--json".into(),
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
            expected.extend(["-c", "include_environment_context=false"].map(Into::into));
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
        assert!(output.starts_with("exec\n--json\n--model\n"), "{output}");
        resolved.backend = Backend::Agy;
        resolved.agy.command = command.to_str().unwrap().into();
        let a = &resolved.agy;
        let expected = format!(
            "--output-format\ntext\n--mode\n{}\n--sandbox\n--model\n{}\n--effort\n{}\n--add-dir\n{}\n--print={}",
            a.permission_mode,
            a.model,
            a.effort,
            ws.path().display(),
            agent_pointer()
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

    /// The prompt-cache contract: for one kind, everything but the part-specific
    /// text is byte-identical across parts, so a cache reads it every time.
    #[test]
    fn two_parts_of_one_kind_share_a_byte_identical_base() {
        for kind in [
            "digital", "vreg", "bjt_npn", "opamp", "diode", "mcu", "charger",
        ] {
            let a = build_prompt_parts("SN74LVC1T45", kind, "front page text one", &[]);
            let b = build_prompt_parts("TXB0101", kind, "a different datasheet entirely", &[]);
            assert_eq!(a.base, b.base, "{kind}: the base differs between parts");
            assert!(a.base.len() > 4_000, "{kind}: the shared base is the bulk");
            assert!(a.tail.contains("SN74LVC1T45") && b.tail.contains("TXB0101"));
            assert!(
                !a.base.contains("SN74LVC1T45") && !a.base.to_lowercase().contains("sn74lvc1t45")
            );
            assert!(!a.base.contains("front page text one"));
            assert_ne!(a.tail, b.tail);
            // Splitting loses nothing: the whole is still the old prompt's content.
            let whole = a.full();
            assert!(
                whole.contains("Its entry's id is \"sn74lvc1t45\"")
                    && whole.contains("PIN ROLE NAMES")
            );
            assert!(whole.ends_with("OUTPUT (TOML only, starting with [[models]]):\n"));
        }
        // A different kind is a different base: the checklist and examples are the kind's.
        assert_ne!(
            build_prompt_parts("X", "vreg", "", &[]).base,
            build_prompt_parts("X", "digital", "", &[]).base
        );
        let (a, b) = (
            identify_parts("LM1117", "text a"),
            identify_parts("BC847", "text b"),
        );
        assert_eq!(a.base, b.base);
        assert!(!a.base.contains("LM1117") && a.tail.contains("LM1117"));
        let (a, b) = (
            harness::locate_parts("A1", "i2c_sensor", "needs"),
            harness::locate_parts("B2", "i2c_sensor", "needs"),
        );
        assert_eq!(a.base, b.base);
        assert!(a.tail.contains("A1") && !a.base.contains("A1"));
    }

    /// The clause after the prompt follows the same rule: its rules are the base,
    /// the page inventory and the precomputed-file list this part's tail.
    #[test]
    fn the_verification_rules_are_shared_and_the_pages_are_per_part() {
        let (one, two) = (Workspace::scratch().unwrap(), Workspace::scratch().unwrap());
        std::fs::write(two.path().join("board.md"), "board").unwrap();
        for table in ["[[models]]", "[sensor]"] {
            let (rules_a, here_a) = verification_parts(&one, table, true);
            let (rules_b, here_b) = verification_parts(&two, table, true);
            assert_eq!(rules_a, rules_b, "{table}");
            assert_ne!(here_a, here_b, "{table}: the file list is the part's");
            assert!(here_b.contains("`board.md`") && !rules_b.contains("board.md` ("));
            assert!(rules_a.contains("VERIFY EACH NUMBER") && rules_a.contains("model.toml"));
        }
    }

    /// Codex forking, with a fake codex that logs every call: the first call of a
    /// shared base warms it (persisted, nothing else asked), every later call is an
    /// ephemeral fork of it in its own sandbox with the same confinement flags and
    /// only its own text on stdin, and the base's usage is taken off a fork's report.
    #[cfg(unix)]
    #[test]
    #[allow(clippy::too_many_lines)] // one scripted scenario, read top to bottom
    fn codex_runs_fork_a_warmed_base_in_their_own_sandbox() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = MOCK_ENV.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (command, log) = (dir.path().join("fake-codex"), dir.path().join("calls.log"));
        std::fs::write(
            &command,
            r#"#!/bin/sh
[ "$1" = login ] && echo 'Logged in using an API key' && exit 0
cat > stdin.txt
{ echo "CALL cwd=$PWD"; printf 'ARG %s\n' "$@"; echo "STDIN<<"; cat stdin.txt; echo ">>"; } >> "$FAKE_LOG"
case " $* " in *" fork "*) FORKED=1;; esac
if [ -n "$FORKED" ]; then
  echo '{"type":"thread.started","thread_id":"cccc-0002"}'
  echo '{"type":"turn.completed","usage":{"input_tokens":2500,"cached_input_tokens":900,"cache_write_input_tokens":0,"output_tokens":35}}'
elif [ -n "$FAKE_FAIL_WARM" ] && grep -q "single word READY" stdin.txt; then
  echo "boom" >&2; exit 1
else
  echo '{"type":"thread.started","thread_id":"aaaa-0001"}'
  echo '{"type":"turn.completed","usage":{"input_tokens":1000,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":10}}'
fi
printf 'done' > last-message.txt
"#,
        )
        .unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut cfg = extract_config::ExtractConfig::default();
        cfg.set("codex.command", command.to_str().unwrap()).unwrap();
        let mut resolved = cfg.resolve(&extract_config::FakeHost::default(), Some(Backend::Codex));
        resolved.codex.permission_mode = "workspace-write".into();
        resolved.codex.profile = None;
        std::env::set_var("FAKE_LOG", &log);
        std::env::set_var("CODEX_HOME", dir.path().join("codex-home"));
        std::env::set_var(crate::extract_cache::ENV_CACHE, "force");
        let calls = || {
            std::fs::read_to_string(&log)
                .unwrap_or_default()
                .split("CALL ")
                .skip(1)
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        let (one, two) = (Workspace::scratch().unwrap(), Workspace::scratch().unwrap());
        let prompt = |tail: &str| Prompt::new("SHARED BASE RULES", tail);

        let first = run_agent_with(&resolved, &prompt("PART ONE TAIL"), &one, 0, false).unwrap();
        let seen = calls();
        assert_eq!(seen.len(), 2, "one warm-up call, then the fork: {seen:?}");
        let warm = &seen[0];
        assert!(warm.contains("ARG exec\nARG --json\n") && !warm.contains("ARG --ephemeral"));
        assert!(warm.contains("SHARED BASE RULES") && warm.contains("single word READY"));
        assert!(
            !warm.contains("PART ONE TAIL"),
            "the warm-up carries no part text"
        );
        let fork = &seen[1];
        assert!(
            fork.contains(&format!("cwd={}", one.path().display())),
            "{fork}"
        );
        assert!(
            fork.contains("ARG exec\nARG --ephemeral\nARG fork\nARG --json\n"),
            "{fork}"
        );
        assert!(fork.contains("ARG aaaa-0001\nARG -\n") && fork.trim_end().contains(">>"));
        for flag in ["--cd", "-p", "--sandbox", "--approve-for-me"] {
            assert!(
                !fork.contains(&format!("ARG {flag}\n")),
                "a fork takes no {flag}"
            );
        }
        for kept in [
            "ARG default_permissions=\"hauksbee\"",
            "ARG --ignore-user-config",
            "ARG --ignore-rules",
            "ARG --skip-git-repo-check",
            "ARG include_environment_context=false",
            "ARG --output-last-message",
        ] {
            assert!(fork.contains(kept), "the fork keeps {kept}: {fork}");
        }
        assert!(fork.contains("PART ONE TAIL") && !fork.contains("SHARED BASE RULES"));
        let used = crate::extract_cache::codex::tokens_of(&first).unwrap();
        assert_eq!(
            (used.input, used.cached, used.output),
            (1500, 900, 25),
            "{first}"
        );

        // A second part in another sandbox forks the same base: no second warm-up.
        run_agent_with(&resolved, &prompt("PART TWO TAIL"), &two, 0, false).unwrap();
        let seen = calls();
        assert_eq!(seen.len(), 3);
        assert!(seen[2].contains(&format!("cwd={}", two.path().display())));
        assert!(seen[2].contains("ARG aaaa-0001\nARG -\n") && seen[2].contains("PART TWO TAIL"));
        assert_ne!(one.path(), two.path());

        // Caching off: a fresh, ephemeral call carrying the whole prompt.
        std::env::set_var(crate::extract_cache::ENV_CACHE, "off");
        run_agent_with(&resolved, &prompt("PART THREE TAIL"), &one, 0, false).unwrap();
        let fresh = calls().pop().unwrap();
        assert!(fresh.contains("ARG --ephemeral\n") && !fresh.contains("ARG fork"));
        assert!(fresh.contains("SHARED BASE RULES") && fresh.contains("PART THREE TAIL"));
        assert!(!fresh.contains("include_environment_context=false"));
        std::env::set_var(crate::extract_cache::ENV_CACHE, "force");

        // A prompt with no shared base, a profile or danger-full-access never forks.
        let before = calls().len();
        run_agent_with(&resolved, &Prompt::from("NO BASE"), &one, 0, false).unwrap();
        resolved.codex.profile = Some("p".into());
        run_agent_with(&resolved, &prompt("PROFILE TAIL"), &one, 0, false).unwrap();
        resolved.codex.profile = None;
        resolved.codex.permission_mode = "danger-full-access".into();
        run_agent_with(&resolved, &prompt("FULL ACCESS TAIL"), &one, 0, false).unwrap();
        let rest = calls().split_off(before);
        assert_eq!(rest.len(), 3);
        assert!(rest
            .iter()
            .all(|c| !c.contains("ARG fork") && c.contains("ARG --ephemeral")));
        resolved.codex.permission_mode = "workspace-write".into();

        // A base that cannot be warmed means fresh calls, not a failed extraction.
        std::env::set_var("FAKE_FAIL_WARM", "1");
        resolved.codex.model = "another-model".into(); // another pool key
        let before = calls().len();
        let ran = run_agent_with(&resolved, &prompt("AFTER FAILURE"), &one, 0, false).unwrap();
        std::env::remove_var("FAKE_FAIL_WARM");
        let rest = calls().split_off(before);
        assert_eq!(
            rest.len(),
            2,
            "the failed warm-up, then one fresh call: {rest:?}"
        );
        assert!(rest[1].contains("ARG --ephemeral\n") && !rest[1].contains("ARG fork"));
        assert!(rest[1].contains("SHARED BASE RULES") && rest[1].contains("AFTER FAILURE"));
        assert!(
            !ran.contains("cccc-0002"),
            "no fork from a base that failed: {ran}"
        );
        crate::extract_cache::codex::release_bases();
        std::env::remove_var("CODEX_HOME");
        std::env::remove_var(crate::extract_cache::ENV_CACHE);
        std::env::remove_var("FAKE_LOG");
    }

    /// Claude Code takes the shared base as its appended system prompt (cached by
    /// content), keeps every restriction of a fresh call, and gets only the part's
    /// own text on stdin; with caching off it is the whole prompt on stdin.
    #[cfg(unix)]
    #[test]
    fn claude_runs_carry_the_base_as_a_cached_system_prompt_with_the_same_restrictions() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = MOCK_ENV.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let command = dir.path().join("fake-claude");
        std::fs::write(
            &command,
            "#!/bin/sh\ncat > stdin.txt\nprintf '%s\\n' \"$@\"\nprintf 'TTL=%s\\n' \"$CLAUDE_CODE_PROMPT_CACHE_TTL\"\n\
             f=''; for a in \"$@\"; do [ -n \"$f\" ] && cp \"$a\" base-seen.txt && f=''; [ \"$a\" = --append-system-prompt-file ] && f=1; done\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut cfg = extract_config::ExtractConfig::default();
        cfg.set("claude-code.command", command.to_str().unwrap())
            .unwrap();
        let resolved = cfg.resolve(
            &extract_config::FakeHost::default(),
            Some(Backend::ClaudeCode),
        );
        let ws = Workspace::scratch().unwrap();
        let prompt = Prompt::new("CLAUDE SHARED BASE", "CLAUDE PART TAIL");
        let restrictions = [
            "--restricted",
            "--strict-mcp-config",
            "--no-session-persistence",
            "--settings",
        ];

        std::env::set_var(crate::extract_cache::ENV_CACHE, "force");
        let out = run_agent_with(&resolved, &prompt, &ws, 0, false).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        for flag in restrictions {
            assert!(lines.contains(&flag), "a cached run keeps {flag}: {out}");
        }
        assert!(
            lines.contains(&"--exclude-dynamic-system-prompt-sections"),
            "{out}"
        );
        assert!(lines.contains(&"TTL=1h"), "{out}");
        let base = std::fs::read_to_string(ws.path().join("base-seen.txt")).unwrap();
        assert!(base.contains("CLAUDE SHARED BASE") && !base.contains("CLAUDE PART TAIL"));
        let stdin = std::fs::read_to_string(ws.path().join("stdin.txt")).unwrap();
        assert!(stdin.contains("CLAUDE PART TAIL") && !stdin.contains("CLAUDE SHARED BASE"));

        std::env::set_var(crate::extract_cache::ENV_CACHE, "off");
        let out = run_agent_with(&resolved, &prompt, &ws, 0, false).unwrap();
        for flag in restrictions {
            assert!(
                out.lines().any(|l| l == flag),
                "an uncached run keeps {flag}: {out}"
            );
        }
        assert!(
            !out.contains("--append-system-prompt-file") && !out.contains("TTL=1h"),
            "{out}"
        );
        let stdin = std::fs::read_to_string(ws.path().join("stdin.txt")).unwrap();
        assert!(stdin.contains("CLAUDE SHARED BASE") && stdin.contains("CLAUDE PART TAIL"));
        std::env::remove_var(crate::extract_cache::ENV_CACHE);
    }

    /// Headless agy auto-denies a command with no allow-rule and stops with
    /// exit 0 and no answer: that must surface as a permission error naming the
    /// fix, never as an empty reply, while a normal run still passes.
    #[cfg(unix)]
    #[test]
    fn agy_permission_denial_is_named_not_an_empty_reply() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let command = dir.path().join("fake-agy");
        let mut cfg = extract_config::ExtractConfig::default();
        cfg.set("agy.command", command.to_str().unwrap()).unwrap();
        let resolved = cfg.resolve(&extract_config::FakeHost::default(), Some(Backend::Agy));
        let mut ws = Workspace::scratch().unwrap();
        ws.checker = Some(dir.path().join("hauksbee"));
        let note = agy_tool_note(&ws);
        assert!(note.contains("models lint model.toml") && note.contains("`--help`"));
        let run = |body: &str| {
            let script = format!("#!/bin/sh\n[ \"$1\" = login ] && exit 0\ncat >/dev/null\n{body}");
            std::fs::write(&command, script).unwrap();
            std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
            let _ = std::fs::remove_file(ws.answer_path());
            run_agent_once(&resolved, "fake prompt", &ws)
        };
        let said = run(
            "echo 'jetski: no output produced - the \"command\" permission was auto-denied' >&2",
        )
        .unwrap_err()
        .to_string();
        assert!(
            said.contains("auto-denied") && said.contains("permissions.allow"),
            "{said}"
        );
        assert!(said.contains("models lint model.toml") && !said.contains("empty reply"));
        let silent = run("exit 0").unwrap_err().to_string();
        assert!(
            silent.contains("permission") && silent.contains("no model.toml"),
            "{silent}"
        );
        assert!(run("printf 'x = 1\\n' > model.toml").is_ok());
        assert!(run("echo 'some prose'").is_ok());
    }

    /// A timed-out agent's lint-clean draft is published, stamped and flagged,
    /// and the run still fails; an invalid one is dropped as before.
    #[cfg(unix)]
    #[test]
    fn timeout_keeps_only_a_lint_clean_draft_and_still_fails() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = MOCK_ENV.lock().unwrap();
        let (dir, ex) = (tempfile::tempdir().unwrap(), FORMAT_EXAMPLES[0]);
        let [pdf, agent, draft, cfg] = ["a.pdf", "agent", "draft", "c"].map(|f| dir.path().join(f));
        let script = format!(
            "#!/bin/sh\n[ \"$1\" = login ] && exit 0\ncp {draft:?} model.toml\nexec sleep 30\n"
        );
        for (path, text) in [(&pdf, "%PDF-1.4\n"), (&cfg, ""), (&agent, script.as_str())] {
            std::fs::write(path, text).unwrap();
        }
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
        let env = [
            (extract_config::ENV_CONFIG_PATH, &cfg),
            ("HAUKSBEE_CLAUDE_COMMAND", &agent),
        ];
        for (k, v) in env {
            std::env::set_var(k, v);
        }
        std::env::set_var("HAUKSBEE_EXTRACT_TIMEOUT_SECS", "1");
        let [(clean, kept), (bad, dropped)] = [ex.card, "[[models]]\nid = 1\n"].map(|text| {
            std::fs::write(&draft, text).unwrap();
            let out = tempfile::tempdir_in(dir.path()).unwrap();
            let args = Args::new(pdf.clone(), ex.part.into(), ex.kind.into());
            let args = args.backend(Some(Backend::ClaudeCode));
            let args = args.out_dir(Some(out.path().into()));
            let err = run(args).unwrap_err().downcast::<TimedOut>().unwrap();
            let file = format!("{}.toml", sanitise_filename(ex.part));
            (err, std::fs::read_to_string(out.path().join(file)).ok())
        });
        for (k, _) in env {
            std::env::remove_var(k);
        }
        std::env::remove_var("HAUKSBEE_EXTRACT_TIMEOUT_SECS");
        assert!(clean.to_string().contains("INCOMPLETE"), "{clean}");
        assert!(kept.is_some_and(|k| k.contains("datasheet-derived") && k.contains("sha256")));
        assert!(bad.2.is_none() && dropped.is_none(), "{bad}");
    }

    /// A draft that fails every attempt is saved with its errors, off the model
    /// scan path, the command to re-check it is printed, and the codex usage
    /// from each call is totalled at list price.
    #[cfg(unix)]
    #[test]
    fn a_rejected_draft_is_kept_for_editing_and_the_cost_is_totalled() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = MOCK_ENV.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let [pdf, agent, draft, cfg] = ["a.pdf", "agent", "draft", "c"].map(|f| dir.path().join(f));
        let event = r#"{"type":"turn.completed","usage":{"input_tokens":1000000,"cached_input_tokens":0,"output_tokens":100000}}"#;
        let script = format!(
            "#!/bin/sh\n[ \"$1\" = login ] && echo 'Logged in using an API key' && exit 0\ncat >/dev/null\ncp {draft:?} model.toml\necho '{event}'\n"
        );
        let bad =
            "[[models]]\nid = \"tp\"\nkind = \"digital\"\n[models.source]\nidentity_only = true\n";
        for (path, text) in [
            (&pdf, "%PDF-1.4\n"),
            (&cfg, ""),
            (&agent, script.as_str()),
            (&draft, bad),
        ] {
            std::fs::write(path, text).unwrap();
        }
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
        let env = [
            (extract_config::ENV_CONFIG_PATH, cfg.to_str().unwrap()),
            ("HAUKSBEE_CODEX_COMMAND", agent.to_str().unwrap()),
            ("HAUKSBEE_CODEX_MODEL", "gpt-6-luna"),
        ];
        for (k, v) in env {
            std::env::set_var(k, v);
        }
        let out = tempfile::tempdir_in(dir.path()).unwrap();
        let args = Args::new(pdf, "TP".into(), "digital".into());
        let args = args
            .backend(Some(Backend::Codex))
            .out_dir(Some(out.path().into()));
        let err = run(args).unwrap_err();
        for (k, _) in env {
            std::env::remove_var(k);
        }
        let kept = out.path().join("rejected/TP.toml");
        let text = err.to_string();
        let saved = std::fs::read_to_string(&kept).unwrap();
        assert!(
            saved.starts_with("# hauksbee rejected this draft:\n# "),
            "{saved}"
        );
        assert!(saved.contains("identity_only = true") && !out.path().join("TP.toml").exists());
        let lint = format!("hauksbee models lint {}", kept.display());
        let keep = format!(
            "mv {} {}",
            kept.display(),
            out.path().join("TP.toml").display()
        );
        assert!(text.contains(&lint) && text.contains(&keep), "{text}");
        assert!(!text.contains("Raw reply was"), "{text}");
        assert!(
            text.contains("`identity_only` is set directly in [models.source]"),
            "{text}"
        );
        // Two attempts of 1M in / 100k out at luna's 0.10 / 0.50 per million:
        // the second repeats the first failing draft unchanged, so the tier stops.
        let cost = crate::extract_usage::summary();
        assert!(
            cost.contains("2000000 input") && cost.contains("about $0.3000"),
            "{cost}"
        );
        let spaced = Path::new("/tmp/a b/it's.toml");
        let tail = rejected_tail("", Some(&(spaced.into(), "/x.toml".into())));
        assert!(tail.contains(r"lint '/tmp/a b/it'\''s.toml'"), "{tail}");
    }

    /// A codex run on a `ChatGPT` plan shows no dollars: its share of the plan
    /// comes from the `token_count` rate limits in its own `--json` stream, even
    /// when the app-server reading fails.
    #[cfg(unix)]
    #[test]
    fn a_codex_plan_run_reports_streamed_plan_usage_not_dollars() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = MOCK_ENV.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let [pdf, agent, cfg] = ["a.pdf", "agent", "c"].map(|f| dir.path().join(f));
        let limits = |five: u8, week: u8| {
            format!(
                r#"{{"type":"token_count","rate_limits":{{"primary":{{"used_percent":{five},"window_minutes":300}},"secondary":{{"used_percent":{week},"window_minutes":10080}}}},"plan_type":"plus"}}"#
            )
        };
        let turn = r#"{"type":"turn.completed","usage":{"input_tokens":1000,"cached_input_tokens":0,"output_tokens":100}}"#;
        let script = format!(
            "#!/bin/sh\n[ \"$1\" = login ] && echo 'Logged in using ChatGPT' && exit 0\n[ \"$1\" = app-server ] && exit 0\ncat >/dev/null\necho '{}'\necho '{turn}'\necho '{}'\n",
            limits(10, 30),
            limits(12, 31)
        );
        for (path, text) in [(&pdf, "%PDF-1.4\n"), (&cfg, ""), (&agent, script.as_str())] {
            std::fs::write(path, text).unwrap();
        }
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
        let env = [
            (extract_config::ENV_CONFIG_PATH, cfg.to_str().unwrap()),
            ("HAUKSBEE_CODEX_COMMAND", agent.to_str().unwrap()),
            ("HAUKSBEE_CODEX_MODEL", "gpt-6-luna"),
        ];
        for (k, v) in env {
            std::env::set_var(k, v);
        }
        let out = tempfile::tempdir_in(dir.path()).unwrap();
        let args = Args::new(pdf, "TP".into(), "digital".into());
        let _ = run(args
            .backend(Some(Backend::Codex))
            .out_dir(Some(out.path().into())));
        for (k, _) in env {
            std::env::remove_var(k);
        }
        let summary = crate::extract_usage::summary();
        assert!(summary.contains("week 30% -> 31%"), "{summary}");
        assert!(summary.contains("5-hour 10% -> 12%"), "{summary}");
        assert!(!summary.contains('$'), "{summary}");
    }

    /// The escalation list behind a Haiku first tier in the ladder tests.
    #[cfg(unix)]
    const HAIKU_LADDER: &str = "claude-sonnet-5-5,claude-opus-5-5";

    /// Fake Claude Code for the ladder: every call logs its `--model`, writes
    /// a valid card (a section agent's whole fragment), and reports usage in a `result`
    /// event; a model named in `doubtful` says `low` confidence, one in `bad`
    /// writes an invalid card. Returns the run, the models called in order,
    /// the plan/usage summary and the activity events.
    #[cfg(unix)]
    fn run_ladder_fake(
        escalate: &str,
        doubtful: &str,
        bad: &str,
    ) -> (Result<String>, Vec<String>, String, String) {
        run_ladder_fake_loud(escalate, doubtful, bad, ("", ""))
    }

    /// [`run_ladder_fake`] where a model named in `loud` also streams an
    /// assistant turn far past any context budget, after it has written its files.
    #[cfg(unix)]
    fn run_ladder_fake_loud(
        escalate: &str,
        doubtful: &str,
        bad: &str,
        (loud, skip): (&str, &str),
    ) -> (Result<String>, Vec<String>, String, String) {
        use std::os::unix::fs::PermissionsExt;
        let _guard = MOCK_ENV.lock().unwrap();
        let (dir, ex) = (tempfile::tempdir().unwrap(), FORMAT_EXAMPLES[0]);
        let [pdf, agent, draft, cfg, log, events] =
            ["a.pdf", "agent", "draft", "c", "log", "events"].map(|f| dir.path().join(f));
        let result = r#"{"type":"result","result":"done","modelUsage":{"MODEL":{"inputTokens":1000,"outputTokens":100,"cacheReadInputTokens":0,"cacheCreationInputTokens":0,"costUSD":0.01,"costBasis":"list"}}}"#;
        let over = r#"{"type":"assistant","message":{"content":[],"usage":{"input_tokens":2,"cache_read_input_tokens":500000}}}"#;
        let script = format!(
            "#!/bin/sh\n[ \"$1\" = auth ] && echo '{{\"loggedIn\":true,\"authMethod\":\"api_key\"}}' && exit 0\n\
             cat >/dev/null\nm=; p=; for a in \"$@\"; do [ \"$p\" = --model ] && m=$a; p=$a; done\necho \"$m\" >> {log:?}\n\
             printf '## Pin map\\n1 VCC\\n' > notes.md\ncp {draft:?} model.toml\necho high > confidence.txt\n\
             case \" {doubtful} \" in *\" $m \"*) printf 'low\\npin 3 unclear\\n' > confidence.txt;; esac\n\
             case \" {bad} \" in *\" $m \"*) printf '[[models]]\\nid = 1\\n' > model.toml;; esac\n\
             [ -n \"$SKIP_SECTION\" ] && grep -q \"YOUR SECTION: $SKIP_SECTION\" prompt.md && rm -f model.toml\n\
             case \" {loud} \" in *\" $m \"*) echo '{over}'; sleep 20;; esac\n\
             echo '{result}' | sed \"s/MODEL/$m/\"\n"
        );
        // A section agent ends its file with the closing line.
        let finished = format!("{}\n# end\n", ex.card);
        for (path, text) in [
            (&pdf, "%PDF-1.4\n"),
            (&cfg, ""),
            (&agent, script.as_str()),
            (&draft, finished.as_str()),
        ] {
            std::fs::write(path, text).unwrap();
        }
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
        let env = [
            (extract_config::ENV_CONFIG_PATH, cfg.to_str().unwrap()),
            ("HAUKSBEE_CLAUDE_COMMAND", agent.to_str().unwrap()),
            // Haiku is named, not defaulted: these tests drive the staged
            // Haiku harness and its ladder on purpose.
            ("HAUKSBEE_CLAUDE_MODEL", "claude-haiku-5-5"),
            ("HAUKSBEE_CLAUDE_ESCALATE", escalate),
            ("SKIP_SECTION", skip),
            (
                crate::extract_progress::ENV_EVENTS,
                events.to_str().unwrap(),
            ),
        ];
        for (k, v) in env {
            std::env::set_var(k, v);
        }
        let out = tempfile::tempdir_in(dir.path()).unwrap();
        let args = Args::new(pdf, ex.part.into(), ex.kind.into());
        let args = args
            .backend(Some(Backend::ClaudeCode))
            .out_dir(Some(out.path().into()));
        let ran = run(args).map(|_| {
            let file = out
                .path()
                .join(format!("{}.toml", sanitise_filename(ex.part)));
            std::fs::read_to_string(file).unwrap_or_default()
        });
        let summary = crate::extract_usage::summary();
        for (k, _) in env {
            std::env::remove_var(k);
        }
        let models = std::fs::read_to_string(&log).unwrap_or_default();
        let events = std::fs::read_to_string(&events).unwrap_or_default();
        (
            ran,
            models.lines().map(str::to_string).collect(),
            summary,
            events,
        )
    }

    /// The Haiku ladder: a harnessed Haiku that doubts its pin map hands the
    /// part to Sonnet, whose card is kept and says which tier wrote it and why.
    #[cfg(unix)]
    #[test]
    fn a_doubtful_haiku_draft_escalates_and_the_card_names_its_tier() {
        let (ran, models, summary, events) = run_ladder_fake(HAIKU_LADDER, "claude-haiku-5-5", "");
        let card = ran.unwrap();
        // Haiku runs its four section agents; Sonnet takes over.
        let mut expected = ["claude-haiku-5-5"; 4].to_vec();
        expected.push("claude-sonnet-5-5");
        assert_eq!(models, expected);
        assert!(card.starts_with("# extracted by claude-code claude-sonnet-5-5 (tier 2 of 3); escalated: self-reported low confidence: pin 3 unclear"), "{card}");
        assert!(
            summary.contains('$') && !summary.contains("plan"),
            "{summary}"
        );
        for kind in [
            "\"tier\"",
            "\"stage\"",
            "\"usage\"",
            "\"escalate\"",
            "\"final\"",
            "section identity",
            "section ratings",
            "section electrical",
            "section switching",
        ] {
            assert!(events.contains(kind), "{kind} missing: {events}");
        }
        // The identity agent runs alone first; the other two start after it.
        let at = |s: &str| events.find(s).unwrap();
        assert!(at("section identity") < at("section ratings"));
        assert!(at("section identity") < at("section electrical"));
        assert!(at("section identity") < at("section switching"));
    }

    /// A staged agent stopped at its budget after it wrote a valid card is not
    /// redone: its draft is checked and kept, with no continuation agent.
    #[cfg(unix)]
    #[test]
    fn a_stopped_agent_that_wrote_a_valid_card_is_not_redone() {
        let (ran, models, _, _) =
            run_ladder_fake_loud(HAIKU_LADDER, "", "", ("claude-haiku-5-5", ""));
        let card = ran.unwrap();
        assert_eq!(models, ["claude-haiku-5-5"; 4], "four section agents");
        assert!(
            card.starts_with("# extracted by claude-code claude-haiku-5-5 (tier 1 of 3)"),
            "{card}"
        );
    }

    /// Section agents write no `[models.source]` tier lines, but the pinout opens
    /// that table: it still gets its tier and validation like an absent one.
    #[test]
    fn a_source_table_opened_by_a_pinout_still_gets_its_tier() {
        let ex = FORMAT_EXAMPLES[0];
        let kept = ex
            .card
            .lines()
            .filter(|l| !l.starts_with("tier = ") && !l.starts_with("validation = "));
        let bare = kept.collect::<Vec<_>>().join("\n");
        let reply = Reply::Model {
            part: ex.part,
            kind: ex.kind,
        };
        assert!(reply.check(&bare).is_err());
        assert!(reply.check(&reply.complete(bare)).is_ok());
    }

    /// A supply-relative limit prints no number: the row moves to an `unknown`
    /// uncertainty record quoting it, as the checker prescribes, with no redraft.
    #[test]
    fn a_row_printing_no_number_becomes_an_unknown_record() {
        let ex = FORMAT_EXAMPLES[0];
        let row = "parameter = \"VIH\"\nunit = \"V\"\nconditions = \"min = 0.7 VCC\"\nbasis = \"Table 5.3\"";
        let card = format!("{}\n[[models.source.characteristic]]\n{row}\n", ex.card);
        let reply = Reply::Model {
            part: ex.part,
            kind: ex.kind,
        };
        assert!(reply.check(&card).is_err());
        let done = reply.complete(card);
        assert!(reply.check(&done).is_ok(), "{done}");
        assert!(done.contains("status = \"unknown\"") && done.contains("min = 0.7 VCC"));
    }

    /// A non-current row (a voltage, a slew rate) naming pins but no rating loses the pins; a current row keeps them.
    #[test]
    fn a_row_naming_pins_without_a_rating_drops_them() {
        let mut doc: toml_edit::DocumentMut =
            "[[characteristic]]\nunit = \"ns/V\"\npins = [\"a\"]\n\
            [[characteristic]]\nunit = \"uA\"\npins = [\"a\"]\n"
                .parse()
                .unwrap();
        assert!(drop_pins_from_untagged_rows(doc.as_table_mut()));
        let rows = doc["characteristic"].as_array_of_tables().unwrap();
        assert!(!rows.get(0).unwrap().contains_key("pins"));
        assert!(rows.get(1).unwrap().contains_key("pins"));
        assert!(!drop_pins_from_untagged_rows(doc.as_table_mut()));
    }

    /// A nominal param whose row prints only a max cannot be read: its row moves
    /// to a characteristic of the same name, never dropped.
    #[test]
    fn a_param_row_its_name_cannot_read_becomes_a_characteristic() {
        let card = "[[models]]\nid = \"q\"\nkind = \"digital\"\n[models.params]\n\
            supply_static_ua = { max = 4.0, unit = \"uA\", conditions = \"c\", basis = \"b\" }\n\
            [models.source]\ntier = \"datasheet-derived\"\nvalidation = \"physical-bounds-only\"\n";
        let reply = Reply::Model {
            part: "q",
            kind: "digital",
        };
        let done: toml::Table = toml::from_str(&reply.complete(card.into())).unwrap();
        let entry = &done["models"][0];
        assert!(entry["params"].get("supply_static_ua").is_none());
        let row = &entry["source"]["characteristic"][0];
        assert_eq!(row["parameter"].as_str(), Some("supply_static_ua"));
        assert_eq!(row["max"].as_float(), Some(4.0));
    }

    /// A behaviour block the lint refuses (a bidirectional pin's sense written
    /// backwards, as a real Haiku run did) fails the draft check too, so the fix
    /// loop sees it rather than a card that lints dirty being kept.
    #[test]
    fn a_logic_block_the_lint_refuses_fails_the_draft_check() {
        let ex = FORMAT_EXAMPLES
            .iter()
            .find(|e| e.kind == "digital")
            .unwrap();
        let keep = |l: &&str| {
            !["identity_only", "warning = ", "unlocked_by"]
                .iter()
                .any(|k| l.starts_with(k))
        };
        let card = ex.card.lines().filter(keep).collect::<Vec<_>>().join("\n");
        let with = |sense: &str| {
            format!(
                "{card}\n[models.logic]\ninputs = [\"dir\", \"a_in\"]\noutputs = [\"a\", \"b\"]\n\
                 [models.logic.comb]\na = \"dir\"\nb = \"a_in\"\n[models.logic.sense]\n{sense}\n"
            )
        };
        let reply = Reply::Model {
            part: ex.part,
            kind: ex.kind,
        };
        reply.check(&with("a_in = \"a\"")).unwrap();
        let err = format!("{:#}", reply.check(&with("a = \"a_in\"")).unwrap_err());
        assert!(
            err.contains("[models.logic]") && err.contains("sense"),
            "{err}"
        );
    }

    /// A section agent that wrote nothing is not papered over: a fix agent
    /// takes the merged card on, and the card still comes from the first tier.
    #[cfg(unix)]
    #[test]
    fn a_missing_section_goes_to_a_fix_agent() {
        let (ran, models, _, _) = run_ladder_fake_loud(HAIKU_LADDER, "", "", ("", "ratings"));
        assert_eq!(models, ["claude-haiku-5-5"; 5], "four sections, one fix");
        assert!(ran
            .unwrap()
            .starts_with("# extracted by claude-code claude-haiku-5-5 (tier 1 of 3)"));
    }

    /// When the stronger tier fails outright, the doubtful but valid draft is
    /// the result and the card says the escalation failed too.
    #[cfg(unix)]
    #[test]
    fn a_failed_last_tier_falls_back_to_the_doubtful_draft() {
        let (ran, models, _, _) =
            run_ladder_fake("claude-sonnet-5-5", "claude-haiku-5-5", "claude-sonnet-5-5");
        let card = ran.unwrap();
        // Its second attempt repeats the first failing draft unchanged, so the
        // tier stops there instead of spending a third.
        assert_eq!(
            models.iter().filter(|m| *m == "claude-sonnet-5-5").count(),
            2,
            "{models:?}"
        );
        assert!(
            card.starts_with("# extracted by claude-code claude-haiku-5-5 (tier 1 of 2)"),
            "{card}"
        );
        assert!(
            card.lines()
                .next()
                .unwrap()
                .contains("claude-sonnet-5-5 failed too"),
            "{card}"
        );
        // A confident first tier stops there.
        let (ran, models, _, _) = run_ladder_fake(HAIKU_LADDER, "", "");
        assert!(ran
            .unwrap()
            .starts_with("# extracted by claude-code claude-haiku-5-5 (tier 1 of 3)\n"));
        assert_eq!(models.len(), 4);
    }

    /// Fake codex for the idle watchdog: attempt 1 stalls (with a helper
    /// process that must die with it), attempt 2 answers. `chatty` instead
    /// streams slowly for longer than the idle limit, then answers once.
    #[cfg(unix)]
    fn run_idle_fake(chatty: bool) -> (Result<(), anyhow::Error>, usize, bool) {
        use std::os::unix::fs::PermissionsExt;
        let _guard = MOCK_ENV.lock().unwrap();
        let (dir, ex) = (tempfile::tempdir().unwrap(), FORMAT_EXAMPLES[0]);
        let [pdf, agent, draft, cfg, count, helper] =
            ["a.pdf", "agent", "draft", "c", "count", "helper.pid"].map(|f| dir.path().join(f));
        std::fs::write(&draft, ex.card).unwrap();
        let body = if chatty {
            "for i in 1 2 3 4 5 6 7 8; do echo '{\"type\":\"tick\"}'; sleep 0.2; done\n\
             cp DRAFT model.toml\n"
        } else {
            "if [ \"$n\" = 1 ]; then sleep 60 & echo $! > HELPER; wait; fi\n\
             echo '{\"type\":\"done\"}'; cp DRAFT model.toml\n"
        };
        let script = format!(
            "#!/bin/sh\n[ \"$1\" = login ] && exit 0\ncat >/dev/null\necho x >> {count:?}\nn=$(wc -l < {count:?} | tr -d ' ')\n{}",
            body.replace("DRAFT", &format!("{draft:?}")).replace("HELPER", &format!("{helper:?}"))
        );
        for (path, text) in [(&pdf, "%PDF-1.4\n"), (&cfg, ""), (&agent, script.as_str())] {
            std::fs::write(path, text).unwrap();
        }
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
        let env = [
            (extract_config::ENV_CONFIG_PATH, cfg.to_str().unwrap()),
            ("HAUKSBEE_CODEX_COMMAND", agent.to_str().unwrap()),
            // The chatty agent's 0.2 s ticks sit far inside a 10 s idle window, so a
            // loaded machine cannot stretch a gap into a stall; the stalling agent
            // keeps a 1 s window so its kill is quick.
            (
                "HAUKSBEE_EXTRACT_IDLE_SECS",
                if chatty { "10" } else { "1" },
            ),
        ];
        for (k, v) in env {
            std::env::set_var(k, v);
        }
        let out = tempfile::tempdir_in(dir.path()).unwrap();
        let args = Args::new(pdf, ex.part.into(), ex.kind.into());
        let args = args.backend(Some(Backend::Codex));
        let result = run(args.out_dir(Some(out.path().into()))).map(|_| ());
        for (k, _) in env {
            std::env::remove_var(k);
        }
        let runs = std::fs::read_to_string(&count).unwrap().lines().count();
        let helper_alive = std::fs::read_to_string(&helper)
            .ok()
            .and_then(|p| p.trim().parse::<u32>().ok())
            .is_some_and(|pid| {
                std::process::Command::new("kill")
                    .args(["-0", &pid.to_string()])
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success())
            });
        (result, runs, helper_alive)
    }

    /// A codex run silent past the idle limit is killed with its helper and
    /// retried once, and the retried run's answer is the result.
    #[cfg(unix)]
    #[test]
    fn idle_codex_is_killed_with_its_children_and_retried_once() {
        let (result, runs, helper_alive) = run_idle_fake(false);
        result.unwrap();
        assert_eq!(runs, 2, "one stalled run plus one retry");
        assert!(!helper_alive, "the stalled run's process tree must be gone");
    }

    /// Slow output is not silence: a stream that keeps emitting events past
    /// the idle limit runs to completion with no retry.
    #[cfg(unix)]
    #[test]
    fn slow_but_chatty_codex_is_not_killed() {
        let (result, runs, _) = run_idle_fake(true);
        result.unwrap();
        assert_eq!(runs, 1);
    }

    /// Round 3: the file saved as MPSA42.pdf was the MPSA92 sheet (its page-1
    /// text below, as pdftotext gives it), naming MPSA42 only as the
    /// complementary NPN. A family sheet naming its part deep inside (JST XH,
    /// S4B-XH-A) and an RTC sheet naming its ordering code are not judged wrong.
    #[test]
    fn a_datasheet_for_another_part_is_refused_before_drafting() {
        let filler =
            "Characteristics Kennwerte Collector-Emitter cutoff current IC = 1 mA\n".repeat(40);
        let mpsa92 = format!(
            "MPSA92\nMPSA92\nHigh Voltage PNP Transistors\nHochspannungs-PNP-Transistoren\nIC\n= - 500 mA\nhFE1 > 40\n\
             Version 2017-11-21\n{filler}2 x 1.27\nDimensions - Maße [mm]\n\nRecommended complementary NPN transistors\n\
             Empfohlene komplementäre NPN-Transistoren\n\nMPSA42\n\u{000c}"
        );
        let why = wrong_datasheet("MPSA42", &[], &mpsa92).expect("refused");
        assert!(
            why.contains("MPSA92") && why.contains("complementary"),
            "{why}"
        );
        assert_eq!(wrong_datasheet("MPSA92", &[], &mpsa92), None);
        assert_eq!(wrong_datasheet("MPSA92-AT", &[], &mpsa92), None);
        // Another family's part is not judged by this sheet's title.
        assert_eq!(wrong_datasheet("BC547", &[], &mpsa92), None);
        // A family title with a wildcard covers its orderable codes.
        let mcu = format!("STM32F103x8\nSTM32F103xB\nMedium-density performance line\n{filler}");
        assert_eq!(wrong_datasheet("STM32F103C8T6", &[], &mcu), None);
        let buck = format!("TPS6213x 3-V to 17-V 3-A Step-Down Converter\n{filler}");
        assert_eq!(wrong_datasheet("TPS62130RGTR", &[], &buck), None);
        // JST's XH family sheet names S4B-XH-A only in its ordering table.
        let xh = format!("XH CONNECTOR\n2.5 mm pitch/Wire-to-Board connectors\n{filler}S4B-XH-A\n");
        assert_eq!(wrong_datasheet("S4B-XH-A", &[], &xh), None);
        let rtc = format!("PCF8563\nReal-time clock/calendar\n{filler}PCF8563BS/4 HVSON10\n");
        assert_eq!(wrong_datasheet("PCF8563BS", &[], &rtc), None);
        assert_eq!(wrong_datasheet("MPSA42", &[], PDF_TEXT_UNAVAILABLE), None);
        assert_eq!(part_stem("24AA025UIDT").as_deref(), Some("24AA025"));
        assert_eq!(stem_family("24AA025"), "24AA");
    }

    /// Round 3: five parts identified as register-map sensors came back as bare
    /// specs no board binds; an identified sensor is drafted as a digital card.
    #[test]
    fn an_identified_sensor_is_drafted_as_a_card() {
        assert_eq!(card_kind("i2c_sensor"), "digital");
        assert_eq!(card_kind("spi_sensor"), "digital");
        assert_eq!(card_kind("vreg"), "vreg");
    }

    /// Round 3: BSS84 returned the same `^BSS84-(7|13)-F$` draft three times.
    /// The second identical failure stops the tier (for the ladder to escalate),
    /// and the refusal quotes a corrected pattern.
    #[test]
    fn an_unchanged_failing_redraft_stops_the_tier() {
        let mut calls = 0;
        let err = validated_attempts(5, "p", Reply::Word, ("codex", false), |_| {
            calls += 1;
            Ok("either vreg or digital".into())
        })
        .unwrap_err();
        assert_eq!(calls, 2, "{err:#}");
        let rejected = err.downcast::<Rejected>().unwrap();
        assert!(rejected.error.contains("unchanged"), "{}", rejected.error);
        // A changed redraft is retried as before.
        let mut calls = 0;
        let word = validated_attempts(2, "p", Reply::Word, ("codex", false), |_| {
            calls += 1;
            Ok(["either vreg or digital", "vreg or digital", "vreg"][calls - 1].into())
        });
        assert_eq!((word.unwrap(), calls), ("vreg".to_string(), 3));
        let pattern = regex::Regex::new(&names_part_pattern("BSS84")).unwrap();
        let mut reel = vec![ModelEntry::default()];
        reel[0].r#match.value_re = Some("^BSS84-(7|13)-F$".into());
        assert!(!names_part(&reel, "BSS84"));
        reel[0].r#match.value_re = Some(pattern.as_str().into());
        assert!(names_part(&reel, "BSS84") && pattern.is_match("BSS84-7-F"));
    }

    /// PCF8523 (2026-10-21): three drafts that each differed by a few characters
    /// failed the same eight-pad check, 0.6-1.2M input tokens apiece. A redraft
    /// that fails with the error already quoted to it stops the tier.
    #[test]
    fn a_redraft_failing_with_the_same_error_stops_the_tier() {
        let model = Reply::Model {
            part: "Q",
            kind: "diode",
        };
        let drafts = [
            "[[models]]\nid = \"q\"\nkind = \"diode\"\nbroken = [\n",
            "[[models]]\nid = \"q\"\nkind = \"diode\"\nbroken = [\n# redrafted\n",
            "never reached",
        ];
        let mut calls = 0;
        let err = validated_attempts(4, "p", model, ("codex", false), |_| {
            calls += 1;
            Ok(drafts[calls - 1].into())
        })
        .unwrap_err();
        let rejected = err.downcast::<Rejected>().unwrap();
        assert_eq!(calls, 2, "{rejected}");
        assert!(rejected.error.contains("same error"), "{}", rejected.error);
    }

    /// Round 3: two package cards both drafted as `id = "max17048"`, and two
    /// independent cards accepting one bare name.
    #[test]
    fn cards_a_board_cannot_tell_apart_are_refused() {
        let card = |id: &str, re: &str| {
            let mut e = ModelEntry {
                id: id.into(),
                ..ModelEntry::default()
            };
            e.r#match.value_re = Some(re.into());
            e
        };
        let re = "(?i)^(?:MAX17048|MAX17048[GX]\\+(T10)?)$";
        let dup = [card("max17048", re), card("max17048", re)];
        let why = ambiguous_cards(&dup, "MAX17048").unwrap();
        assert!(why.contains("share id 'max17048'"), "{why}");
        let loose = [card("max17048_tdfn", re), card("max17048_wlp", re)];
        assert!(ambiguous_cards(&loose, "MAX17048")
            .unwrap()
            .contains("file order"));
        let mut gated = loose.clone();
        gated[1].r#match.footprint_re = Some("(?i)WLP".into());
        assert_eq!(ambiguous_cards(&gated, "MAX17048"), None);
        let mut set = loose;
        for e in &mut set {
            e.variant_of = Some("max17048".into());
        }
        assert_eq!(ambiguous_cards(&set, "MAX17048"), None);
        let own = [
            card("mic_adj", "(?i)^MIC5205$"),
            card("mic_3v3", "(?i)^MIC5205-3\\.3"),
        ];
        assert_eq!(ambiguous_cards(&own, "MIC5205"), None);
    }

    /// The progress line names one short reason, not codex's whole event stream.
    #[test]
    fn a_backend_error_is_shown_as_one_short_reason() {
        let stream = "codex exited with status exit status: 1: {\"type\":\"item.completed\",\"item\":{\"id\":\"item_0\",\"type\":\"error\",\"message\":\"Codex is ignoring 1 unrecognized configuration setting.\"}} | {\"type\":\"turn.started\"} | {\"type\":\"error\",\"message\":\"exceeded retry limit, last status: 429 Too Many Requests\"} | {\"type\":\"turn.failed\",\"error\":{\"message\":\"exceeded retry limit, last status: 429 Too Many Requests\"}}";
        assert_eq!(
            short_backend_reason(stream),
            "rate limited (429 Too Many Requests)"
        );
        let plain =
            "codex exited with status 1: {\"type\":\"error\",\"message\":\"stream disconnected\"}";
        assert_eq!(short_backend_reason(plain), "stream disconnected");
        let failed = "codex exited with status 1: {\"type\":\"turn.failed\",\"error\":{\"message\":\"boom 500\"}}";
        assert_eq!(short_backend_reason(failed), "boom 500");
        assert_eq!(short_backend_reason("first line\nsecond"), "first line");
        let cut = short_backend_reason(&"x".repeat(300));
        assert_eq!(cut.chars().count(), 163);
        assert!(cut.ends_with("..."));
    }

    /// Round 3: codex's "Reconnecting... 5/5 (workspace routing discovery
    /// failed)" exit is waited out; a refused request or a timeout is not.
    #[test]
    fn a_transient_backend_error_is_retried_after_a_wait() {
        let outage = "codex exited with status exit status: 1: {\"type\":\"error\",\"message\":\"Reconnecting... 5/5 (workspace routing discovery failed)\"}";
        let mut calls = 0;
        let got = with_backoff(&[Duration::ZERO, Duration::ZERO], || {
            calls += 1;
            if calls < 3 {
                Err(anyhow::anyhow!("{outage}"))
            } else {
                Ok("vreg")
            }
        });
        assert_eq!((got.unwrap(), calls), ("vreg", 3));
        let mut calls = 0;
        let err = with_backoff(&[Duration::ZERO; 3], || -> Result<()> {
            calls += 1;
            Err(anyhow::anyhow!("{outage}"))
        });
        assert!(err.is_err() && calls == 4);
        for permanent in [
            anyhow::anyhow!("codex exited with status 1: model 'x' does not exist"),
            anyhow::anyhow!("You've hit your usage limit; rate limit resets in 3 days"),
            TimedOut("codex".into(), 900, None).into(),
            IdleStalled("codex".into(), 300).into(),
        ] {
            assert!(!transient_backend_error(&permanent), "{permanent:#}");
        }
    }

    /// A session turned away at its first request waits a few jittered seconds
    /// through the batch's gate (which frees the caller's place meanwhile) and
    /// tells the gate it was refused; it does not sit out the long outage waits.
    #[test]
    fn a_refused_start_waits_briefly_through_the_gate() {
        use crate::extract_admission::{install, Gate};
        use std::sync::{Arc, Mutex};
        struct Recorder {
            waits: Mutex<Vec<Duration>>,
            refused: Mutex<usize>,
        }
        impl Gate for Recorder {
            fn before_start(&self) {}
            fn refused(&self) {
                *self.refused.lock().unwrap() += 1;
            }
            fn pause(&self, how_long: Duration) {
                self.waits.lock().unwrap().push(how_long);
            }
        }
        let gate = Arc::new(Recorder {
            waits: Mutex::new(Vec::new()),
            refused: Mutex::new(0),
        });
        let _installed = install(Arc::clone(&gate) as Arc<dyn Gate>);
        let mut calls = 0;
        let got = with_backoff(&[Duration::from_secs(30)], || {
            calls += 1;
            if calls < 3 {
                Err(anyhow::anyhow!("HTTP 429 Too Many Requests"))
            } else {
                Ok("drafted")
            }
        });
        assert_eq!((got.unwrap(), calls), ("drafted", 3));
        let waits = gate.waits.lock().unwrap().clone();
        assert_eq!(waits.len(), 2, "{waits:?}");
        assert!(
            (Duration::from_secs(1)..=Duration::from_secs(3)).contains(&waits[0]),
            "a few seconds, not 30: {waits:?}"
        );
        assert!(
            (Duration::from_secs(2)..=Duration::from_secs(6)).contains(&waits[1]),
            "growing: {waits:?}"
        );
        assert_eq!(*gate.refused.lock().unwrap(), 2, "each refusal is pressure");
    }

    /// The base a kind's batch warms is the base every part of that kind forks
    /// from: it does not depend on which part asked.
    #[test]
    fn the_warmed_base_of_a_kind_is_the_same_for_every_part() {
        for kind in ["opamp", "vreg", "comparator", "ldo", "mosfet"] {
            if component_kind(kind).is_none() {
                continue;
            }
            let base = |part: &str| {
                let templates =
                    crate::card_templates::select(part, kind, crate::card_templates::LIMIT);
                build_prompt_parts(part, kind, "", &templates).base
            };
            assert_eq!(base("LM358"), base("TLV9062"), "{kind}");
            assert_eq!(base("LM358"), base("OPA2134"), "{kind}");
        }
    }

    #[test]
    fn a_second_stall_is_not_retried_again() {
        let mut calls = 0;
        let err = validated_attempts(5, "p", Reply::Word, ("codex", false), |_| {
            calls += 1;
            Err(IdleStalled("codex".into(), 1).into())
        })
        .unwrap_err();
        assert_eq!(calls, 2, "{err:#}");
        assert!(err.is::<IdleStalled>());
    }

    #[test]
    fn a_stage_stopped_at_its_context_budget_is_continued_by_a_fresh_agent() {
        let mut prompts = Vec::new();
        let word = validated_attempts(1, "p", Reply::Word, ("claude", false), |p| {
            prompts.push(p.to_string());
            if prompts.len() == 1 {
                Err(harness::Stopped("over budget".into()).into())
            } else {
                Ok("vreg".into())
            }
        })
        .unwrap();
        assert_eq!(word, "vreg");
        assert!(
            prompts[1].ends_with(harness::CONTINUE_AFTER_STOP),
            "{}",
            prompts[1]
        );
        // With no attempt left the stop is the outcome, for the ladder to escalate on.
        let err = validated_attempts(0, "p", Reply::Word, ("claude", false), |_| {
            Err(harness::Stopped("over budget".into()).into())
        })
        .unwrap_err();
        assert!(err.is::<harness::Stopped>());
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
        assert!(local.contains("the selected pages are under"));
        assert!(local.contains("find its page in page-index.md"));
        assert!(local.len() < model_prompt.len());

        let sensor_prompt = build_sensor_prompt("TMP117", "i2c_sensor", &marker);
        let local_sensor = prompt_for_agent_backend(&sensor_prompt);
        assert!(!local_sensor.contains(noise));
        assert!(local_sensor.contains("find its page in page-index.md"));

        // Kind identification is a separate small call made before the
        // workspace exists, so prompts without the bounded source markers are
        // deliberately unchanged.
        let identify = "identify this part from the first-page text";
        assert_eq!(prompt_for_agent_backend(identify), identify);
    }
}
