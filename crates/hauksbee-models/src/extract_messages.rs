//! The words `models extract` says about finding, downloading and reusing
//! datasheets, in one place.
//!
//! The terminal command prints these and a surface that shows the same events
//! (the web app's extract flow) routes to them rather than keeping its own
//! copy, so a rewording lands everywhere at once. Each function returns one
//! line, ready to print; none prints.

use crate::datasheet::Backend;
use crate::datasheet_discovery::Missing;
use crate::extract_capacity::{Basis, Plan, PoolStats, Warning, RAMP_START};
use crate::extract_usage::{totals, CallRecord};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// What a datasheet search sends and where, shown before the first search (the
/// existing [`crate::datasheet::CONSENT_NOTICE`] covers what is sent to read a
/// PDF; this covers the search before it).
pub const SEARCH_NOTICE: &str =
    "Where a part has no datasheet link, the agent backend also searches the web for it: it is \
     sent the part number, the board's value, footprint and description for that part, and runs \
     with web search on. hauksbee downloads whatever it finds itself and uses the file only if \
     its own text names the part (the part number, or its family name); anything it doubts is \
     skipped, never used.";

/// A web search for a part's datasheet, the next move when there is none.
#[must_use]
pub fn datasheet_search(part: &str) -> String {
    let query: String = part
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c.to_string()
            } else {
                format!("%{:02X}", c as u32)
            }
        })
        .collect();
    format!("https://duckduckgo.com/?q={query}+datasheet+pdf")
}

/// The `--datasheet` flag that supplies a datasheet by hand.
#[must_use]
pub fn datasheet_flag(part: &str) -> String {
    format!("--datasheet '{part}=<file.pdf or URL>'")
}

/// A part the board gives no datasheet for, when nothing here can search for it.
/// An API backend cannot search; the line names the agent backends that can.
#[must_use]
pub fn no_datasheet(part: &str, backend: Backend) -> String {
    let manual = format!("pass {} ({})", datasheet_flag(part), datasheet_search(part));
    if backend.can_search() {
        format!("no datasheet for {part}: {manual}")
    } else {
        format!(
            "no datasheet for {part}: {manual}, or extract with an agent backend that can search \
             the web for it ({}; `hauksbee models backend show`)",
            Backend::searchers().join(", ")
        )
    }
}

/// The consent question for a batch.
#[must_use]
pub fn batch_question(ready: usize, searching: usize, doubted: usize) -> String {
    let search = match searching {
        0 => String::new(),
        n => format!(", {n} of them found by web search"),
    };
    let doubt = match doubted {
        0 => String::new(),
        n => format!(" ({n} of them look like another part's, see above)"),
    };
    format!("Fetch and send {ready} datasheet(s){search} and draft a model for each{doubt}?")
}

/// The search for one part is starting.
#[must_use]
pub fn searching(part: &str, why: &str, backend: &str) -> String {
    format!("{part}: {why}; searching the web for its datasheet with {backend}")
}

/// The first search gave nothing that worked; the agent is asked once more.
#[must_use]
pub fn searching_again(part: &str, backend: &str) -> String {
    format!("{part}: none of those links worked; asking {backend} once more for a different one")
}

/// The datasheet a search found and hauksbee downloaded and checked.
#[must_use]
pub fn found(
    part: &str,
    url: &str,
    found_on: Option<&str>,
    backend: &str,
    evidence: &str,
) -> String {
    let source = found_on.map_or_else(String::new, |p| format!(" (found on {p})"));
    format!(
        "{part}: datasheet found by {backend}: {url}{source}; downloaded and checked, {evidence}"
    )
}

/// A board link that needed a second route to download.
#[must_use]
pub fn followed_landing_page(part: &str, page: &str, pdf: &str) -> String {
    format!("{part}: {page} is a web page; followed its one datasheet link to {pdf}")
}

/// A board datasheet link that did not download, and what happens next.
#[must_use]
pub fn link_failed(part: &str, url: &str, why: &str, next: &str) -> String {
    format!("{part}: the board's datasheet link {url} did not download: {why}; {next}")
}

/// A board link that downloaded but is some other part's datasheet.
#[must_use]
pub fn board_link_names_another_part(part: &str, url: &str, why: &str) -> String {
    format!(
        "{part}: the board's datasheet link {url} downloaded, but {why}; looking for the \
         part's own datasheet (the board's link is kept if none is found)"
    )
}

/// A found datasheet hauksbee will not use.
#[must_use]
pub fn doubted(part: &str, url: &str, why: &str) -> String {
    format!("{part}: not using {url}: {why}")
}

/// A part no datasheet could be obtained for, said when it happens.
///
/// One wording whether the board gave a link that failed, gave none, or the search
/// found nothing it could vouch for: it is "no datasheet", never an extraction
/// failure, and the batch's closing list says how to supply it.
#[must_use]
pub fn no_datasheet_found(part: &str, why: &str) -> String {
    format!("no datasheet for {part}: {why}. Nothing was sent for it; it is on the list at the end")
}

/// A PDF a person saved for the part is used instead of fetching one.
#[must_use]
pub fn using_supplied(part: &str, pdf: &Path) -> String {
    format!(
        "{part}: using the datasheet you supplied, {}",
        pdf.display()
    )
}

/// A host that refused (or went quiet) is skipped for the rest of the run.
#[must_use]
pub fn host_skipped(host: &str, why: &str) -> String {
    format!("{host}: {why}; skipping its links for the rest of this run and looking elsewhere")
}

/// A long batch is not silent: how far it has got.
#[must_use]
pub fn progress(done: usize, total: usize, running: usize) -> String {
    format!("[batch] {done} of {total} parts done, {running} running")
}

/// A percentage of a limit, to a whole point (a tenth below one).
fn pct(x: f64) -> String {
    if x < 1.0 {
        format!("{x:.1}%")
    } else {
        format!("{x:.0}%")
    }
}

fn agents(n: usize) -> String {
    format!("{n} agent{}", if n == 1 { "" } else { "s" })
}

/// The one line before a batch: what it will cost the plan and how many agents
/// run at once. Shown by the CLI and by the app's extract flow alike.
#[must_use]
pub fn preflight(plan: &Plan) -> String {
    let parts = format!(
        "{} part{}",
        plan.parts,
        if plan.parts == 1 { "" } else { "s" }
    );
    let jobs = pool_phrase(plan.jobs);
    let how = pool_note(plan);
    match plan.basis {
        Basis::Known {
            session_use,
            week_use,
            ..
        } => format!(
            "{parts}, about {} of your 5-hour window and {} of your weekly; running {jobs} {how}",
            pct(session_use),
            pct(week_use)
        ),
        Basis::Metered => format!(
            "{parts}, billed per call through your API key (no plan to run out of); running {jobs} {how}"
        ),
        Basis::Unknown => {
            format!("{parts}, plan headroom unknown; running {jobs} {how}")
        }
    }
}

/// How many agents run at once, as the most the pool will reach.
fn pool_phrase(jobs: usize) -> String {
    if jobs <= 1 {
        "1 agent at a time".to_string()
    } else {
        format!("up to {} at once", agents(jobs))
    }
}

/// What the pool does on the way: ramps up, backs off, and why it is that size.
fn pool_note(plan: &Plan) -> String {
    let moves = match (plan.jobs > RAMP_START, plan.jobs > 1) {
        (true, _) => "ramping up; fewer if the service rate-limits",
        (false, true) => "fewer if the service rate-limits",
        (false, false) => "",
    };
    let why = if plan.overridden {
        format!("as asked; the default is {}", plan.recommended)
    } else if matches!(plan.basis, Basis::Unknown) {
        "conservative".to_string()
    } else {
        String::new()
    };
    let parts: Vec<String> = [why, moves.to_string()]
        .into_iter()
        .filter(|t| !t.is_empty())
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("({})", parts.join("; "))
    }
}

/// One reason to look twice before a batch starts.
#[must_use]
pub fn plan_warning(w: &Warning) -> String {
    match *w {
        Warning::WeekShare(x) => format!(
            "warning: this batch alone would use about {} of your weekly allowance",
            pct(x)
        ),
        Warning::WeekPast(a) => format!(
            "warning: it would take your weekly usage to about {}",
            pct(a)
        ),
        Warning::SessionPast(a) => format!(
            "warning: it would take your 5-hour usage to about {}",
            pct(a)
        ),
        Warning::WindowExceeded(a) => format!(
            "warning: it needs more than your 5-hour window has left (about {}): the run will pause for the window to reset",
            pct(a)
        ),
    }
}

/// Added to the consent question when the plan warned.
pub const PLAN_QUESTION_SUFFIX: &str = " Go ahead despite the plan warning above?";

/// Under `--yes`, what is said when a warning does not stop the run.
#[must_use]
pub fn plan_continues(pool_held: bool, jobs: usize) -> String {
    if pool_held {
        format!(
            "continuing because of --yes, with {} (held to what the window can carry)",
            agents(jobs)
        )
    } else {
        "continuing because of --yes".to_string()
    }
}

/// The backend is rate limiting; the pool shrinks.
#[must_use]
pub fn rate_limited(from: usize, to: usize) -> String {
    format!(
        "[batch] the backend is rate limiting; running {} instead of {from} from now on",
        agents(to)
    )
}

/// The pool grows back one step after a quiet stretch with no rate limiting.
#[must_use]
pub fn pool_regrew(from: usize, to: usize) -> String {
    format!(
        "[batch] the backend has stopped rate limiting; running {} instead of {from} again",
        agents(to)
    )
}

/// The one line of the end tally on how the pool moved: the ceiling it was
/// planned to, the most agents that really ran at once, where it ended, and how
/// often the service refused.
#[must_use]
pub fn pool_line(pool: &PoolStats) -> String {
    if pool.planned <= 1 {
        return "agents: 1 at a time".to_string();
    }
    let refused = match pool.rate_limits {
        0 => "no rate-limit refusals".to_string(),
        1 => "1 rate-limit refusal (429)".to_string(),
        n => format!("{n} rate-limit refusals (429)"),
    };
    format!(
        "agents: up to {} planned, peak {} at once, finished with {}; {refused}",
        pool.planned, pool.peak, pool.last
    )
}

/// The refined cost says the window will not last; the pool shrinks.
#[must_use]
pub fn window_tight(from: usize, to: usize) -> String {
    format!(
        "[batch] the 5-hour window is draining faster than expected; running {} instead of {from} from now on",
        agents(to)
    )
}

/// The plan's state, for the progress line.
#[must_use]
pub fn plan_now(session: f64, per_part: f64, observed: bool) -> String {
    format!(
        "5-hour window at {}, about {:.1} points per part ({})",
        pct(session),
        per_part,
        if observed { "measured" } else { "estimated" }
    )
}

/// [`progress`] with the plan's state after it.
#[must_use]
pub fn progress_plan(done: usize, total: usize, running: usize, plan: &str) -> String {
    format!("{}; {plan}", progress(done, total, running))
}

/// The batch's usage record, for the summary file: totals first, then every
/// call with the tokens it used and the plan readings around it.
#[must_use]
pub fn usage_record(calls: &[CallRecord]) -> String {
    if calls.is_empty() {
        return "usage: no backend calls were recorded\n".to_string();
    }
    let t = totals(calls);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "usage: {} call(s), {} reported tokens: {} input ({} cached{}), {} output",
        t.calls,
        t.known,
        t.tokens.input,
        t.tokens.cached,
        t.cache_share()
            .map_or_else(String::new, |s| format!(", {:.0}% of input", s * 100.0)),
        t.tokens.output
    );
    let span = |name: &str, s: Option<(f64, f64)>| {
        s.map_or_else(
            || format!("{name}: not reported"),
            |(a, b)| format!("{name}: {a:.1}% -> {b:.1}% ({:+.1} points)", b - a),
        )
    };
    let _ = writeln!(
        out,
        "plan: {}; {}",
        span("5-hour", t.session),
        span("week", t.week)
    );
    let one = |x: Option<f64>| x.map_or_else(|| "-".to_string(), |v| format!("{v:.1}"));
    let _ = writeln!(
        out,
        "call\tpart\tmodel\tinput\tcached\toutput\t5h_before\t5h_after\tweek_before\tweek_after"
    );
    for (n, c) in calls.iter().enumerate() {
        let tok = |f: fn(&crate::extract_usage::Usage) -> u64| {
            c.usage
                .as_ref()
                .map_or_else(|| "-".to_string(), |u| f(u).to_string())
        };
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            n + 1,
            if c.part.is_empty() { "-" } else { &c.part },
            c.model,
            tok(|u| u.input),
            tok(|u| u.cached),
            tok(|u| u.output),
            one(c.session_before),
            one(c.session_after),
            one(c.week_before),
            one(c.week_after),
        );
    }
    out
}

/// Said while the plan's headroom is read (it can take a few seconds).
pub const CHECKING_HEADROOM: &str = "Checking your plan's headroom...";

/// The file the end-of-run record is saved as, beside the cards.
pub const BATCH_RECORD_FILE: &str = "batch-summary.txt";

/// The whole end-of-run record: what the batch was planned as, how it ended,
/// how the pool moved, and the usage of every backend call.
#[must_use]
pub fn batch_record(
    planned: &str,
    summary: &str,
    pool: &PoolStats,
    calls: &[CallRecord],
) -> String {
    let mut out = format!("plan: {planned}\n{summary}\n");
    let _ = writeln!(out, "{}", pool_line(pool));
    out.push_str(&usage_record(calls));
    out
}

/// The search found nothing hauksbee could use.
#[must_use]
pub fn nothing_found(reply_note: &str) -> String {
    format!("the web search found no usable datasheet ({reply_note})")
}

/// The last resort for a blocked download: the person fetches it.
#[must_use]
pub fn download_yourself(part: &str, url: &str) -> String {
    format!(
        "download it yourself: open {url} in a browser, save the PDF, and pass --datasheet '{part}=<file.pdf>'"
    )
}

/// Why `part` already has a card in the target library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingCard {
    pub path: PathBuf,
    /// Set when the card is an identity-only abstention: the card's own reason.
    pub identity_only: Option<String>,
}

/// The card file for `part` in `dir`, if one is there.
///
/// An identity-only card (`identity_only = true` in `[models.params]`) is an
/// abstention: it names the part, its pins and ratings but cannot simulate it,
/// and says why in its `warning`.
#[must_use]
pub fn existing_card(dir: &Path, part: &str) -> Option<ExistingCard> {
    let stem = crate::datasheet::sanitise_filename(part);
    let path = [".toml", ".sensor.toml"]
        .iter()
        .map(|suffix| dir.join(format!("{stem}{suffix}")))
        .find(|p| p.is_file())?;
    let identity_only = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|doc| identity_reason(&doc));
    Some(ExistingCard {
        path,
        identity_only,
    })
}

fn identity_reason(doc: &toml::Table) -> Option<String> {
    let entries = doc.get("models")?.as_array()?;
    entries.iter().find_map(|entry| {
        let params = entry.get("params")?.as_table()?;
        if params.get("identity_only")?.as_bool() != Some(true) {
            return None;
        }
        let text = |key: &str| {
            params
                .get(key)
                .and_then(toml::Value::as_str)
                .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
                .filter(|s| !s.is_empty())
        };
        let warning = text("warning").unwrap_or_else(|| "it declares no behaviour".into());
        Some(match text("unlocked_by") {
            Some(unlock) => format!("{}; unlocked by: {unlock}", warning.trim_end_matches('.')),
            None => warning,
        })
    })
}

/// What to say about a part that already has a card in the target library. An
/// identity-only card is the commonest case after an earlier batch; it is
/// reported, not counted as a failure.
#[must_use]
pub fn has_card(part: &str, card: &ExistingCard) -> String {
    card.identity_only.as_ref().map_or_else(
        || {
            format!(
                "{part}: already has a card ({}); not overwritten. Re-extract with --force to replace it",
                card.path.display()
            )
        },
        |reason| {
            format!(
                "{part}: has a card that cannot simulate it yet (identity-only): {}; open it in \
                 the model editor or re-extract with --force ({})",
                reason.trim_end_matches('.'),
                card.path.display()
            )
        },
    )
}

/// The closing line of a batch.
///
/// It uses the three words the exit status is read by: parts `saved` (a card
/// was written), parts `for hand supply` (no datasheet could be obtained,
/// listed by [`supply_report`]; nothing was sent for them) and parts that `failed` (the extraction ran and was refused). `kept` parts
/// already had a card, and `skipped` ones failed on an earlier batch with these
/// very inputs and were not sent again; neither is a failure of this run.
#[must_use]
pub fn batch_summary(
    saved: usize,
    kept: usize,
    hand_supply: usize,
    skipped: usize,
    failed: &[&str],
) -> String {
    let mut bits = vec![format!("{saved} saved")];
    if kept > 0 {
        bits.push(format!("{kept} already had a card"));
    }
    if hand_supply > 0 {
        bits.push(format!("{hand_supply} for hand supply"));
    }
    if skipped > 0 {
        bits.push(format!(
            "{skipped} skipped (failed before; --retry-failed tries them again)"
        ));
    }
    if !failed.is_empty() {
        bits.push(format!("{} failed ({})", failed.len(), failed.join(", ")));
    }
    format!("Batch: {}", bits.join(", "))
}

/// A part left out because the library remembers it failing on these inputs.
#[must_use]
pub fn skipped_failed(part: &str, reason: &str, ago: &str) -> String {
    format!(
        "{part}: skipped, it failed {ago} with this same datasheet, model and hauksbee version \
         ({reason}); --retry-failed tries it again, and a new datasheet or a different model \
         does so by itself"
    )
}

/// A part whose datasheet search is not repeated because the last one, with
/// these inputs, found nothing a few days ago.
#[must_use]
pub fn skipped_search(part: &str, ago: &str) -> String {
    format!(
        "{part}: not searching again, the last search {ago} found no datasheet; it stays on the \
         list for hand supply. --retry-failed searches anyway, and a datasheet you supply is \
         picked up by itself"
    )
}

/// The marker [`crate::datasheet::vouches_for`] puts in its reason when a PDF
/// never prints the part's name (a test keeps the two in step).
pub const NOT_NAMED: &str = "never prints";

/// Whether the part's own name is a well-known module's.
///
/// The Espressif WROOM/WROVER/MINI-1 modules, the ESP-12 series, u-blox NINA,
/// Seeed XIAO, Adafruit Feather, Teensy and RAK4631 boards. Bare chips and
/// system-in-package parts (ESP32-PICO) are not.
#[must_use]
pub fn named_like_module(part: &str) -> bool {
    const NAMES: [&str; 8] = [
        "WROOM", "WROVER", "MINI-1", "ESP-12", "NINA-", "XIAO", "FEATHER", "TEENSY",
    ];
    let name = part.to_ascii_uppercase().replace(['_', ' '], "-");
    NAMES.iter().any(|n| name.contains(n)) || name.starts_with("RAK4631")
}

/// Whether the board's own words say the part is a module, not a bare chip.
///
/// The part's name, its footprints and its description are read for a
/// ready-made module, carrier or breakout board, whose datasheet is then the
/// module's own: the bare chip's sheet numbers the chip's pads, not the module's.
#[must_use]
pub fn looks_like_module(part: &str, footprints: &[String], description: Option<&str>) -> bool {
    const WORDS: [&str; 9] = [
        "module",
        "breakout",
        "carrier",
        "dev board",
        "devboard",
        "dev kit",
        "devkit",
        "development board",
        "evaluation board",
    ];
    if named_like_module(part) {
        return true;
    }
    footprints
        .iter()
        .map(String::as_str)
        .chain(description)
        .map(str::to_ascii_lowercase)
        .any(|text| WORDS.iter().any(|w| text.contains(w)))
}

/// The plain-words hint for a part that may be a module: which datasheet names
/// it. `None` when neither the PDF found nor the board points that way.
fn module_hint(part: &str, missing: &Missing, module_like: bool) -> Option<String> {
    let not_named =
        missing.why.contains(NOT_NAMED) || missing.tried.iter().any(|t| t.why.contains(NOT_NAMED));
    let ask = "supply the module's own datasheet (the vendor's module datasheet, not the bare \
               chip's: the chip's sheet numbers the chip's pads, not the module's) under the \
               name below";
    if not_named {
        Some(format!(
            "the PDF that was found does not name {part}. If {part} is a module or carrier \
             board, {ask}"
        ))
    } else {
        module_like.then(|| {
            format!(
                "{part} looks like a module or carrier board, which a chip's datasheet may not \
                 name. If no sheet does, {ask}"
            )
        })
    }
}

/// What each exit status of `models extract --unmodelled` means. The `--help`
/// text, the closing note of a batch and the docs all read this one table.
pub const EXIT_STATUS_HELP: &str = "Exit status of --unmodelled:\n  \
0  every part attempted was saved or listed for hand supply (parts that already had a card, or were skipped as failed before, do not count against it)\n  \
1  parts failed and none was saved, or a hard error (unreadable board, bad settings, no consent)\n  \
4  PARTIAL SUCCESS, not a total failure: some cards were saved and kept, some parts failed; --retry-failed tries the failed ones again";

/// The closing note of a batch that ended with failures: what the exit status
/// is and what it means, said where the person is looking at the tally.
#[must_use]
pub fn exit_status_note(saved: usize, failed: &[&str]) -> String {
    if saved == 0 {
        return format!(
            "Exit status 1: {} part(s) failed and none was saved ({}). --retry-failed tries \
             them again; `hauksbee models extract --help` lists every exit status.",
            failed.len(),
            failed.join(", ")
        );
    }
    format!(
        "Exit status 4 = partial success, not a total failure: {saved} card(s) were saved and \
         kept, {} part(s) failed ({}). Re-run with --retry-failed to try only the failed ones \
         again; `hauksbee models extract --help` lists every exit status.",
        failed.len(),
        failed.join(", ")
    )
}

/// The error a batch with failures ends in.
#[must_use]
pub fn batch_failed(drafted: usize, failed: &[&str]) -> String {
    let partial = if drafted == 0 {
        String::new()
    } else {
        format!(" ({drafted} others were saved and kept)")
    };
    format!(
        "{} of the extractions failed: {}{partial}",
        failed.len(),
        failed.join(", ")
    )
}

/// Something an agent could not decide from the datasheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    pub part: String,
    pub question: String,
}

/// The name a person gives a datasheet for `part`: `<part>.pdf`.
#[must_use]
pub fn supplied_name(part: &str) -> String {
    format!("{}.pdf", crate::datasheet::sanitise_filename(part))
}

/// The end-of-run list of what a person has to supply, or empty when there is
/// nothing.
///
/// Every part no datasheet could be obtained for with each link tried
/// and why it failed, then each question an agent asked that nobody was there
/// to answer. `datasheet_dir` is where `<part>.pdf` files are picked up on a
/// rerun and `rerun` is the one command that reruns the batch with them; both
/// are named exactly. `modules` names the parts whose board footprint or
/// description reads as a module ([`looks_like_module`]). Printed at the end of
/// a batch and saved beside the cards ([`SUPPLY_FILE`]).
#[must_use]
pub fn supply_report(
    missing: &[(String, Missing)],
    modules: &[String],
    needs: &[Need],
    datasheet_dir: &Path,
    out_dir: &Path,
    rerun: &str,
) -> String {
    if missing.is_empty() && needs.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    if !missing.is_empty() {
        let _ = writeln!(
            out,
            "{} part(s) have no datasheet to extract from. Supply these by hand:",
            missing.len()
        );
        for (part, m) in missing {
            let _ = writeln!(out, "  {part}: {}", m.why);
            for t in &m.tried {
                let _ = writeln!(out, "      tried {}: {}", t.url, t.why);
            }
            let module_like = modules.iter().any(|m| m.eq_ignore_ascii_case(part));
            if let Some(hint) = module_hint(part, m, module_like) {
                let _ = writeln!(out, "      hint: {hint}");
            }
            let _ = writeln!(
                out,
                "      save the PDF as {}  (search: {})",
                datasheet_dir.join(supplied_name(part)).display(),
                datasheet_search(part)
            );
        }
        let _ = writeln!(
            out,
            "\nThen rerun the same batch. Files named <part>.pdf in {} are picked up (use \
             --datasheet-dir to point at another folder); parts that already have a card are left \
             alone:\n  {rerun}\nOr give one directly with {}.",
            datasheet_dir.display(),
            datasheet_flag("<part>")
        );
    }
    if !needs.is_empty() {
        if !missing.is_empty() {
            out.push('\n');
        }
        let _ = writeln!(
            out,
            "{} thing(s) an agent needed and nobody was there to answer:",
            needs.len()
        );
        for n in needs {
            let _ = writeln!(out, "  {}: needs: {}", n.part, n.question);
            let card = existing_card(out_dir, &n.part).map_or_else(String::new, |c| {
                format!(" or edit {} by hand", c.path.display())
            });
            let _ = writeln!(
                out,
                "      supply it: save a datasheet or note that states it as {}, then run \
                 `hauksbee models extract --part {} --pdf {} --out-dir {} --force`{card}",
                datasheet_dir.join(supplied_name(&n.part)).display(),
                shell_word(&n.part),
                shell_word(
                    &datasheet_dir
                        .join(supplied_name(&n.part))
                        .display()
                        .to_string()
                ),
                shell_word(&out_dir.display().to_string()),
            );
        }
    }
    out
}

/// The list written beside the cards by [`supply_report`].
pub const SUPPLY_FILE: &str = "supply-by-hand.txt";

/// `text` quoted for a POSIX shell when it needs it.
fn shell_word(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=+@".contains(c));
    if plain {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasheet_discovery::Tried;

    fn missing(why: &str, urls: &[(&str, &str)]) -> Missing {
        Missing {
            why: why.into(),
            tried: urls
                .iter()
                .map(|(u, w)| Tried {
                    url: (*u).into(),
                    why: (*w).into(),
                })
                .collect(),
        }
    }

    #[test]
    fn the_supply_list_names_each_link_tried_the_folder_and_the_one_rerun_command() {
        let miss = [
            (
                "LM358".to_string(),
                missing(
                    "the board's link could not be downloaded",
                    &[("https://vendor.example/a.pdf", "HTTP 403")],
                ),
            ),
            ("TPS7A".to_string(), missing("no link", &[])),
        ];
        let rerun = "hauksbee models extract --unmodelled b.kicad_pcb --datasheet-dir /d --yes";
        let text = supply_report(&miss, &[], &[], Path::new("/d"), Path::new("/out"), rerun);
        for want in [
            "2 part(s) have no datasheet",
            "LM358: the board's link could not be downloaded",
            "tried https://vendor.example/a.pdf: HTTP 403",
            "save the PDF as /d/LM358.pdf",
            "save the PDF as /d/TPS7A.pdf",
            rerun,
        ] {
            assert!(text.contains(want), "{want}\n{text}");
        }
        assert!(!text.contains("needs:"), "{text}");
    }

    #[test]
    fn an_unanswered_question_is_a_need_with_the_way_to_supply_it() {
        let needs = [Need {
            part: "U 1".into(),
            question: "which variant is fitted?".into(),
        }];
        let text = supply_report(
            &[],
            &[],
            &needs,
            Path::new("/d"),
            Path::new("/out"),
            "rerun",
        );
        assert!(
            text.contains("U 1: needs: which variant is fitted?"),
            "{text}"
        );
        assert!(
            text.contains("hauksbee models extract --part 'U 1' --pdf"),
            "{text}"
        );
        assert!(!text.contains("Supply these by hand"), "{text}");
    }

    #[test]
    fn nothing_outstanding_is_an_empty_list_and_progress_reads_plainly() {
        assert_eq!(
            supply_report(&[], &[], &[], Path::new("/d"), Path::new("/o"), "r"),
            ""
        );
        assert_eq!(progress(3, 10, 4), "[batch] 3 of 10 parts done, 4 running");
    }

    #[test]
    fn the_preflight_line_reads_the_same_wherever_it_is_shown() {
        use crate::extract_capacity::{plan, Headroom, CODEX_LUNA};
        use crate::extract_plan::Reading;
        let known = Headroom::Plan(Reading {
            week: Some(20.0),
            session: Some(10.0),
        });
        let p = plan(38, known, Some(CODEX_LUNA), 6, None, false);
        assert_eq!(
            preflight(&p),
            "38 parts, about 30% of your 5-hour window and 2% of your weekly; running up to 4 agents at once (ramping up; fewer if the service rate-limits)"
        );
        assert!(!preflight(&p).contains("highest safe"));
        let asked = plan(38, known, Some(CODEX_LUNA), 6, Some(6), false);
        assert!(
            preflight(&asked).contains(
                "up to 6 agents at once (as asked; the default is 4; ramping up; fewer if the service rate-limits)"
            ),
            "{}",
            preflight(&asked)
        );
        let serial = plan(38, known, Some(CODEX_LUNA), 6, Some(1), false);
        assert!(
            preflight(&serial).ends_with("running 1 agent at a time (as asked; the default is 4)")
        );
        let unknown = plan(1, Headroom::Unknown, None, 6, None, false);
        assert_eq!(
            preflight(&unknown),
            "1 part, plan headroom unknown; running 1 agent at a time (conservative)"
        );
        let metered = plan(10, Headroom::Metered, None, 8, None, false);
        assert!(
            preflight(&metered).contains("billed per call"),
            "{}",
            preflight(&metered)
        );
    }

    #[test]
    fn each_plan_warning_says_what_it_would_use() {
        use crate::extract_capacity::Warning;
        assert!(plan_warning(&Warning::WeekShare(19.0)).contains("about 19% of your weekly"));
        assert!(plan_warning(&Warning::WeekPast(86.0)).contains("weekly usage to about 86%"));
        assert!(plan_warning(&Warning::SessionPast(90.0)).contains("5-hour usage to about 90%"));
        assert!(plan_warning(&Warning::WindowExceeded(120.0))
            .contains("will pause for the window to reset"));
        assert_eq!(
            rate_limited(8, 4),
            "[batch] the backend is rate limiting; running 4 agents instead of 8 from now on"
        );
        assert!(progress_plan(1, 5, 2, &plan_now(14.0, 0.9, true))
            .ends_with("; 5-hour window at 14%, about 0.9 points per part (measured)"));
    }

    #[test]
    fn the_usage_record_has_totals_and_every_call() {
        use crate::extract_usage::{CallRecord, Usage};
        let call = |part: &str, usage: Option<Usage>, before: Option<f64>, after: Option<f64>| {
            CallRecord {
                part: part.into(),
                tool: "codex".into(),
                model: "gpt-6-luna".into(),
                usage,
                session_before: before,
                session_after: after,
                week_before: None,
                week_after: None,
            }
        };
        let text = usage_record(&[
            call(
                "U1",
                Some(Usage {
                    input: 1000,
                    cached: 400,
                    output: 50,
                }),
                Some(10.0),
                Some(10.5),
            ),
            call("U2", None, None, None),
        ]);
        assert!(
            text.contains(
                "2 call(s), 1 reported tokens: 1000 input (400 cached, 40% of input), 50 output"
            ),
            "{text}"
        );
        assert!(
            text.contains("5-hour: 10.0% -> 10.5% (+0.5 points); week: not reported"),
            "{text}"
        );
        assert!(
            text.contains("1\tU1\tgpt-6-luna\t1000\t400\t50\t10.0\t10.5\t-\t-"),
            "{text}"
        );
        assert!(
            text.contains("2\tU2\tgpt-6-luna\t-\t-\t-\t-\t-\t-\t-"),
            "{text}"
        );
        assert!(usage_record(&[]).contains("no backend calls"));
    }

    #[test]
    fn the_batch_record_says_how_the_pool_moved_and_carries_the_usage() {
        let moved = PoolStats {
            planned: 4,
            peak: 4,
            last: 2,
            rate_limits: 3,
        };
        let shrank = batch_record("3 parts, plan", "Batch: 3 drafted", &moved, &[]);
        assert!(
            shrank.starts_with("plan: 3 parts, plan\nBatch: 3 drafted\n"),
            "{shrank}"
        );
        assert!(
            shrank.contains(
                "agents: up to 4 planned, peak 4 at once, finished with 2; 3 rate-limit refusals (429)"
            ),
            "{shrank}"
        );
        assert!(
            shrank.contains("no backend calls were recorded"),
            "{shrank}"
        );
        let calm = PoolStats {
            planned: 4,
            peak: 3,
            last: 4,
            rate_limits: 0,
        };
        assert!(batch_record("p", "s", &calm, &[]).contains(
            "agents: up to 4 planned, peak 3 at once, finished with 4; no rate-limit refusals"
        ));
        let one = PoolStats {
            rate_limits: 1,
            ..moved
        };
        assert!(pool_line(&one).ends_with("1 rate-limit refusal (429)"));
        assert_eq!(
            pool_line(&PoolStats {
                planned: 1,
                peak: 1,
                last: 1,
                rate_limits: 0
            }),
            "agents: 1 at a time"
        );
        assert_eq!(
            pool_regrew(2, 3),
            "[batch] the backend has stopped rate limiting; running 3 agents instead of 2 again"
        );
    }

    /// One wording for every way of having no datasheet.
    #[test]
    fn no_datasheet_reads_the_same_whatever_the_cause() {
        for why in [
            "the board gives no link",
            "HTTP 404",
            "the search found nothing",
        ] {
            let line = no_datasheet_found("X1", why);
            assert!(line.starts_with("no datasheet for X1: "), "{line}");
            assert!(!line.contains("failed"), "{line}");
        }
    }

    #[test]
    fn an_api_backend_is_told_which_backends_can_search() {
        let api = no_datasheet("LM358", Backend::Api);
        assert!(api.contains("--datasheet 'LM358="), "{api}");
        for name in Backend::searchers() {
            assert!(api.contains(name), "{api}");
        }
        assert!(!api.contains("api,"), "{api}");
        let codex = no_datasheet("LM358", Backend::Codex);
        assert!(!codex.contains("agent backend"), "{codex}");
        assert!(
            codex.starts_with("no datasheet for LM358: pass --datasheet"),
            "{codex}"
        );
    }

    #[test]
    fn the_consent_question_counts_searches_and_doubts() {
        assert_eq!(
            batch_question(3, 0, 0),
            "Fetch and send 3 datasheet(s) and draft a model for each?"
        );
        assert_eq!(
            batch_question(3, 2, 1),
            "Fetch and send 3 datasheet(s), 2 of them found by web search and draft a model for each (1 of them look like another part's, see above)?"
        );
    }

    #[test]
    fn a_batch_summary_separates_drafts_kept_cards_skips_and_failures() {
        assert_eq!(
            batch_summary(2, 1, 3, 4, &["A", "B"]),
            "Batch: 2 saved, 1 already had a card, 3 for hand supply, 4 skipped (failed before; \
             --retry-failed tries them again), 2 failed (A, B)"
        );
        assert_eq!(batch_summary(4, 0, 0, 0, &[]), "Batch: 4 saved");
        assert!(batch_failed(2, &["A"]).contains("2 others were saved and kept"));
        let partial = exit_status_note(2, &["A"]);
        assert!(
            partial.starts_with("Exit status 4 = partial success, not a total failure")
                && partial.contains("2 card(s) were saved and kept")
                && partial.contains("--retry-failed"),
            "{partial}"
        );
        let none = exit_status_note(0, &["A", "B"]);
        assert!(
            none.starts_with("Exit status 1:") && none.contains("none was saved"),
            "{none}"
        );
        for code in ["0  ", "1  ", "4  PARTIAL SUCCESS"] {
            assert!(EXIT_STATUS_HELP.contains(code), "{code}");
        }
        assert!(!batch_failed(0, &["A"]).contains("others"));
    }

    /// The marker the module hint reads is the one the vouching check writes.
    #[test]
    fn the_not_named_marker_is_what_the_vouching_check_says() {
        let text = "Some other chip. ".repeat(60);
        let why = crate::datasheet::vouches_for("ACME-MOD9", &[], &text).unwrap_err();
        assert!(why.contains(NOT_NAMED), "{why}");
    }

    #[test]
    fn a_module_is_told_to_supply_its_own_or_its_main_chips_datasheet() {
        let named_other = (
            "ACME-MOD9".to_string(),
            missing(
                "no downloaded candidate could be verified as its datasheet",
                &[(
                    "https://v.example/chip.pdf",
                    "the PDF never prints ACME-MOD9",
                )],
            ),
        );
        let bare = ("LM358".to_string(), missing("no link", &[]));
        let text = supply_report(
            &[named_other, bare],
            &[],
            &[],
            Path::new("/d"),
            Path::new("/o"),
            "r",
        );
        assert!(
            text.contains("the PDF that was found does not name ACME-MOD9")
                && text.contains("module's own datasheet (the vendor's module datasheet")
                && !text.contains("datasheet of the main chip"),
            "{text}"
        );
        assert_eq!(
            text.matches("hint:").count(),
            1,
            "a bare chip gets none:\n{text}"
        );
    }

    #[test]
    fn a_part_that_looks_like_a_module_gets_the_hint_without_a_wrong_pdf() {
        let miss = [("X1".to_string(), missing("no link", &[]))];
        let text = supply_report(
            &miss,
            &["x1".to_string()],
            &[],
            Path::new("/d"),
            Path::new("/o"),
            "r",
        );
        assert!(
            text.contains("X1 looks like a module or carrier board")
                && text.contains("module's own datasheet"),
            "{text}"
        );
        let plain = supply_report(&miss, &[], &[], Path::new("/d"), Path::new("/o"), "r");
        assert!(!plain.contains("hint:"), "{plain}");
    }

    #[test]
    fn modules_are_told_by_the_boards_own_words() {
        let fp = |s: &str| vec![s.to_string()];
        assert!(looks_like_module("X1", &fp("Module:Some_Dev_Board"), None));
        assert!(looks_like_module(
            "X1",
            &fp("Package_SO:SOIC-8"),
            Some("Wi-Fi breakout board")
        ));
        assert!(looks_like_module("X1", &[], Some("Sensor carrier")));
        assert!(!looks_like_module(
            "LM358",
            &fp("Package_SO:SOIC-8_3.9x4.9mm"),
            Some("Dual op-amp")
        ));
        assert!(!looks_like_module("LM358", &[], None));
    }

    /// A module is told by its name when the board gives it a plain footprint
    /// and no description, and a module's hand-supply message points at the
    /// module's datasheet, never the bare chip's.
    #[test]
    fn well_known_modules_are_told_by_name() {
        for name in [
            "ESP32-WROOM-32E",
            "ESP32-S3-MINI-1U",
            "esp32_wrover_e",
            "ESP-12E",
            "NINA-B112",
            "Seeed XIAO RP2040",
            "Feather_M0",
            "Teensy 4.0",
            "RAK4631",
        ] {
            assert!(looks_like_module(name, &[], None), "{name}");
        }
        for name in [
            "ESP32-PICO-D4",
            "ESP32-S3",
            "STM32F411CEU6",
            "LM358",
            "NE555",
        ] {
            assert!(!looks_like_module(name, &[], None), "{name}");
        }
        let miss = [("ESP32-S3-MINI-1U".to_string(), missing("no link", &[]))];
        let text = supply_report(
            &miss,
            &["ESP32-S3-MINI-1U".to_string()],
            &[],
            Path::new("/d"),
            Path::new("/o"),
            "r",
        );
        assert!(
            text.contains("looks like a module or carrier board")
                && text.contains("module's own datasheet")
                && !text.contains("main chip"),
            "{text}"
        );
    }

    #[test]
    fn a_skip_says_why_and_how_to_force_it() {
        let failed = skipped_failed("LM358", "the validator refused", "3 days ago");
        assert!(
            failed.starts_with("LM358: skipped, it failed 3 days ago")
                && failed.contains("--retry-failed")
                && failed.contains("a new datasheet or a different model"),
            "{failed}"
        );
        let search = skipped_search("X1", "earlier today");
        assert!(
            search.contains("not searching again")
                && search.contains("hand supply")
                && search.contains("--retry-failed"),
            "{search}"
        );
    }

    /// An identity-only card in the library is found, with its own reason.
    #[test]
    fn an_identity_only_card_is_found_with_its_reason() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("TPS7A02.toml"),
            "[[models]]\nid = \"tps7a02\"\nkind = \"digital\"\n[models.params]\n\
             identity_only = true\nwarning = \"The package is not named.\"\n\
             unlocked_by = \"name the package\"\n",
        )
        .unwrap();
        let card = existing_card(dir.path(), "TPS7A02").unwrap();
        assert_eq!(
            card.identity_only.as_deref(),
            Some("The package is not named; unlocked by: name the package")
        );
        let line = has_card("TPS7A02", &card);
        assert!(
            line.starts_with(
                "TPS7A02: has a card that cannot simulate it yet (identity-only): The package is not named; unlocked by: name the package; open it in the model editor or re-extract with --force"
            ),
            "{line}"
        );
        assert!(existing_card(dir.path(), "OTHER").is_none());
    }

    #[test]
    fn a_working_card_is_kept_not_called_identity_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("R1.toml"),
            "[[models]]\nid = \"r\"\nkind = \"resistor\"\n[models.params]\nr = 1.0\n",
        )
        .unwrap();
        let card = existing_card(dir.path(), "R1").unwrap();
        assert!(card.identity_only.is_none());
        assert!(has_card("R1", &card).contains("not overwritten"));
        // An unreadable card is still a card that is there.
        std::fs::write(dir.path().join("BAD.toml"), "not [toml").unwrap();
        assert!(existing_card(dir.path(), "BAD")
            .unwrap()
            .identity_only
            .is_none());
    }

    #[test]
    fn the_search_url_is_percent_encoded() {
        assert_eq!(
            datasheet_search("AB 1/2"),
            "https://duckduckgo.com/?q=AB%201%2F2+datasheet+pdf"
        );
    }
}
