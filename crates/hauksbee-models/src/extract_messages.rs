//! The words `models extract` says about finding, downloading and reusing
//! datasheets, in one place.
//!
//! The terminal command prints these and a surface that shows the same events
//! (the web app's extract flow) routes to them rather than keeping its own
//! copy, so a rewording lands everywhere at once. Each function returns one
//! line, ready to print; none prints.

use crate::datasheet::Backend;
use crate::datasheet_discovery::Missing;
use crate::extract_capacity::{Basis, Plan, Warning};
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
    let jobs = agents(plan.jobs);
    let how = if plan.overridden {
        format!("(as asked; the highest safe is {})", plan.recommended)
    } else {
        "(highest safe)".to_string()
    };
    match plan.basis {
        Basis::Known {
            session_use,
            week_use,
            ..
        } => format!(
            "{parts}, about {} of your 5-hour window and {} of your weekly; running {jobs} in parallel {how}",
            pct(session_use),
            pct(week_use)
        ),
        Basis::Metered => format!(
            "{parts}, billed per call through your API key (no plan to run out of); running {jobs} in parallel {how}"
        ),
        Basis::Unknown => format!(
            "{parts}, plan headroom unknown; running {jobs} in parallel (conservative)"
        ),
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
    jobs: (usize, usize),
    calls: &[CallRecord],
) -> String {
    let mut out = format!("plan: {planned}\n{summary}\n");
    if jobs.1 < jobs.0 {
        let _ = writeln!(
            out,
            "agents: started with {}, finished with {} (the pool shrank during the run)",
            jobs.0, jobs.1
        );
    } else {
        let _ = writeln!(out, "agents: {} throughout", jobs.0);
    }
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

/// The closing line of a batch. `no_datasheet` is its own count: parts nothing
/// could be fetched for are not extraction failures, and are listed separately
/// ([`supply_report`]).
#[must_use]
pub fn batch_summary(drafted: usize, kept: usize, no_datasheet: usize, failed: &[&str]) -> String {
    let mut bits = vec![format!("{drafted} drafted")];
    if kept > 0 {
        bits.push(format!("{kept} already had a card"));
    }
    if no_datasheet > 0 {
        bits.push(format!("{no_datasheet} without a datasheet"));
    }
    if !failed.is_empty() {
        bits.push(format!("{} failed ({})", failed.len(), failed.join(", ")));
    }
    format!("Batch: {}", bits.join(", "))
}

/// The error a batch with failures ends in.
#[must_use]
pub fn batch_failed(drafted: usize, failed: &[&str]) -> String {
    let partial = if drafted == 0 {
        String::new()
    } else {
        format!(" ({drafted} others were drafted and kept)")
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
/// are named exactly. Printed at the end of a batch and saved beside the cards
/// ([`SUPPLY_FILE`]).
#[must_use]
pub fn supply_report(
    missing: &[(String, Missing)],
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
        let text = supply_report(&miss, &[], Path::new("/d"), Path::new("/out"), rerun);
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
        let text = supply_report(&[], &needs, Path::new("/d"), Path::new("/out"), "rerun");
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
            supply_report(&[], &[], Path::new("/d"), Path::new("/o"), "r"),
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
            "38 parts, about 30% of your 5-hour window and 2% of your weekly; running 6 agents in parallel (highest safe)"
        );
        let asked = plan(38, known, Some(CODEX_LUNA), 6, Some(3), false);
        assert!(
            preflight(&asked).contains("3 agents in parallel (as asked; the highest safe is 6)")
        );
        let unknown = plan(1, Headroom::Unknown, None, 6, None, false);
        assert_eq!(
            preflight(&unknown),
            "1 part, plan headroom unknown; running 1 agent in parallel (conservative)"
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
        let shrank = batch_record("3 parts, plan", "Batch: 3 drafted", (4, 2), &[]);
        assert!(
            shrank.starts_with("plan: 3 parts, plan\nBatch: 3 drafted\n"),
            "{shrank}"
        );
        assert!(
            shrank.contains("started with 4, finished with 2"),
            "{shrank}"
        );
        assert!(
            shrank.contains("no backend calls were recorded"),
            "{shrank}"
        );
        assert!(batch_record("p", "s", (4, 4), &[]).contains("agents: 4 throughout"));
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
            batch_summary(2, 1, 3, &["A", "B"]),
            "Batch: 2 drafted, 1 already had a card, 3 without a datasheet, 2 failed (A, B)"
        );
        assert_eq!(batch_summary(4, 0, 0, &[]), "Batch: 4 drafted");
        assert!(batch_failed(2, &["A"]).contains("2 others were drafted and kept"));
        assert!(!batch_failed(0, &["A"]).contains("others"));
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
