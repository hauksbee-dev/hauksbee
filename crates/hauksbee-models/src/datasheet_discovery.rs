//! Getting a datasheet PDF for a board part, by whatever route works, and
//! only ever one that has been checked.
//!
//! The routes, in order:
//!
//! 1. The board's own `Datasheet` link: a local file is used as it is; a URL is
//!    downloaded with [`crate::datasheet_fetch`] (retries, browser headers, landing
//!    pages). The board's author chose it, so the existing wrong-sheet guard in
//!    the extraction is the check.
//! 2. When the board names none, or its link cannot be downloaded: an agent
//!    backend with web search finds the official PDF. What it answers is
//!    untrusted text. hauksbee downloads the URL itself and
//!    [`datasheet::vouches_for`] demands positive evidence in the PDF's own
//!    text (the part number, or its family base name) before it is used. A
//!    doubt skips the part; nothing is sent to the extraction for it.
//!
//! The search is the [`Searcher`] trait so everything here is tested without a
//! model or a network.

use crate::datasheet;
use crate::datasheet_fetch::{self as fetch, FetchError, Http, Via};
use std::path::{Path, PathBuf};

/// What the board (and the person) tell hauksbee about one part.
#[derive(Debug, Clone, Default)]
pub struct Wanted {
    pub part: String,
    /// Other names the board uses for it (its values).
    pub aliases: Vec<String>,
    pub footprints: Vec<String>,
    /// The board symbol's description, when it has one.
    pub description: Option<String>,
    /// The board's manufacturer property, when it has one.
    pub manufacturer: Option<String>,
}

/// Something that can search the web for a datasheet. The real one is
/// [`AgentSearcher`].
pub trait Searcher {
    /// What does the work, for the lines hauksbee prints (`codex gpt-6-luna`).
    fn describe(&self) -> String;
    /// Run the search prompt and return the agent's reply.
    ///
    /// # Errors
    ///
    /// The agent could not be run or failed.
    fn search(&self, prompt: &str) -> anyhow::Result<String>;
}

/// About how long one datasheet search takes an agent with web search.
const SEARCH_TYPICAL_SECS: u64 = 60;

/// The configured agent backend, with web search on.
pub struct AgentSearcher {
    pub resolved: crate::extract_config::Resolved,
}

impl Searcher for AgentSearcher {
    fn describe(&self) -> String {
        self.resolved.summary()
    }

    fn search(&self, prompt: &str) -> anyhow::Result<String> {
        // A search's duration says nothing about how long a draft takes, so
        // its spinner is not remembered in the extraction history.
        crate::extract_progress::with_unrecorded_spinner(|| {
            crate::extract_progress::start();
            crate::extract_progress::phase("searching the web for the datasheet");
            crate::extract_progress::expect(SEARCH_TYPICAL_SECS);
            datasheet::run_agent_searching(&self.resolved, prompt)
        })
    }
}

/// The prompt for one part. The board's own words come first, quoted as data.
#[must_use]
pub fn search_prompt(want: &Wanted) -> String {
    use std::fmt::Write as _;
    let mut context = format!("Part number: {}\n", want.part);
    let mut line = |label: &str, values: &[String]| {
        let values: Vec<&str> = values
            .iter()
            .map(String::as_str)
            .filter(|v| !v.trim().is_empty())
            .collect();
        if !values.is_empty() {
            let _ = writeln!(context, "{label}: {}", values.join("; "));
        }
    };
    line("Board value", &want.aliases);
    line("Footprint", &want.footprints);
    line(
        "Board description",
        &want.description.iter().cloned().collect::<Vec<_>>(),
    );
    line(
        "Manufacturer",
        &want.manufacturer.iter().cloned().collect::<Vec<_>>(),
    );
    format!(
        "Find the OFFICIAL manufacturer datasheet PDF for this electronic component. Use web \
         search. Prefer the manufacturer's own website over distributors and aggregators, and \
         the datasheet over an application note, product brief or errata. Check that the \
         document is for this exact part number or its family.\n\n{context}\n\
         Reply with only these lines, and nothing else:\n\
         URL: <the direct https link to the PDF file>\n\
         SOURCE: <the page where you found the link>\n\
         If you cannot find the official datasheet, reply with exactly one line: NOT FOUND: <why>"
    )
}

/// What the agent said.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Answer {
    /// Candidate PDF URLs, in the order given (at most three).
    pub urls: Vec<String>,
    /// The page it says it found them on.
    pub source: Option<String>,
    /// The agent's `NOT FOUND` reason, if it gave one.
    pub not_found: Option<String>,
}

fn first_url(text: &str) -> Option<String> {
    let start = text.find("http://").or_else(|| text.find("https://"))?;
    let rest = &text[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || matches!(c, ')' | ']' | '>' | '"' | '\'' | '`' | '<'))
        .unwrap_or(rest.len());
    let url = rest[..end].trim_end_matches(['.', ',', ';', '*']);
    (url.len() > "https://x".len()).then(|| url.to_string())
}

/// Read the agent's reply. Only `URL:` lines are candidates; a reply with none
/// falls back to its first link that is not the `SOURCE:`.
#[must_use]
pub fn parse_answer(reply: &str) -> Answer {
    let mut answer = Answer::default();
    for line in reply.lines() {
        let trimmed = line.trim().trim_start_matches(['-', '*', ' ']);
        let upper = trimmed.to_ascii_uppercase();
        if let Some(why) = upper
            .starts_with("NOT FOUND")
            .then(|| trimmed["NOT FOUND".len()..].trim_start_matches([':', ' ']))
        {
            answer.not_found = Some(why.trim().to_string());
        } else if upper.starts_with("URL") {
            if let Some(url) = first_url(trimmed) {
                if !answer.urls.contains(&url) && answer.urls.len() < 3 {
                    answer.urls.push(url);
                }
            }
        } else if upper.starts_with("SOURCE") {
            answer.source = first_url(trimmed).or(answer.source);
        }
    }
    if answer.urls.is_empty() && answer.not_found.is_none() {
        if let Some(url) = reply
            .lines()
            .filter(|l| !l.trim().to_ascii_uppercase().starts_with("SOURCE"))
            .find_map(first_url)
        {
            answer.urls.push(url);
        }
    }
    answer
}

/// A datasheet that is ready to extract from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    pub pdf: PathBuf,
}

/// What came of getting a datasheet for one part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Use this PDF.
    Ready(Ready),
    /// Leave this part alone: nothing trustworthy to extract from. Not an
    /// error; the line says why and what to pass instead.
    Skipped(String),
    /// A datasheet the board named could not be obtained and there was no
    /// other route. An error for the batch.
    Failed(String),
}

/// The ways and means of one acquisition.
pub struct Acquirer<'a> {
    pub http: &'a dyn Http,
    /// `None` when the backend cannot search (the API).
    pub searcher: Option<&'a dyn Searcher>,
    /// Where downloaded PDFs are kept.
    pub dir: &'a Path,
    /// A PDF's text, for the check that it is the part's own datasheet
    /// ([`datasheet::pdf_text`] in use; a canned text in tests).
    pub read_text: fn(&Path) -> String,
}

impl Acquirer<'_> {
    /// Get a verified datasheet PDF for `want`, from the board's `source` (a
    /// file path or URL) when it has one, else by search. `say` receives each
    /// line to show the person as it happens.
    pub fn acquire(
        &self,
        want: &Wanted,
        source: Option<&str>,
        say: &mut dyn FnMut(String),
    ) -> Outcome {
        use crate::extract_messages as words;
        let part = want.part.as_str();
        let mut failed_board: Option<String> = None;
        let mut tried: Vec<String> = Vec::new();
        let why_search = match source {
            Some(local) if !local.starts_with("http") => {
                return Outcome::Ready(Ready {
                    pdf: PathBuf::from(local),
                });
            }
            Some(url) => match self.download(url, part, true, say) {
                Ok(ready) => return Outcome::Ready(ready),
                Err(e) => {
                    failed_board = Some(url.to_string());
                    tried.push(url.to_string());
                    let next = if self.searcher.is_some() {
                        "looking for another copy".to_string()
                    } else {
                        words::download_yourself(part, url)
                    };
                    say(words::link_failed(part, url, &e.why(), &next));
                    if self.searcher.is_none() {
                        return Outcome::Failed(format!(
                            "{part}: could not download {url}: {}",
                            e.why()
                        ));
                    }
                    format!("its board link did not download ({})", e.why())
                }
            },
            None => "the board gives no datasheet".to_string(),
        };
        let Some(searcher) = self.searcher else {
            return Outcome::Skipped("no datasheet link and no way to search for one".into());
        };
        match self.discover(want, searcher, &why_search, &mut tried, say) {
            Outcome::Skipped(why) => match failed_board {
                Some(url) => Outcome::Failed(format!(
                    "{part}: {why}; {}",
                    words::download_yourself(part, &url)
                )),
                None => Outcome::Skipped(why),
            },
            other => other,
        }
    }

    /// A board URL, downloaded and (if a landing page) followed.
    fn download(
        &self,
        url: &str,
        part: &str,
        reuse: bool,
        say: &mut dyn FnMut(String),
    ) -> Result<Ready, FetchError> {
        std::fs::create_dir_all(self.dir).map_err(|e| FetchError::Transport {
            url: url.to_string(),
            message: format!("creating '{}': {e}", self.dir.display()),
        })?;
        let dest = self
            .dir
            .join(format!("{}.pdf", datasheet::sanitise_filename(part)));
        if reuse && datasheet::is_pdf(&dest) {
            return Ok(Ready { pdf: dest });
        }
        let got = fetch::fetch_pdf(self.http, url, &dest, part)?;
        if let Via::LandingPage { page } = &got.via {
            say(crate::extract_messages::followed_landing_page(
                part, page, &got.url,
            ));
        }
        Ok(Ready { pdf: got.path })
    }

    /// Search, download and verify; the first candidate that is the part's own
    /// datasheet wins. When every link the agent gave fails (a dead link, a
    /// different part's sheet), it is asked once more with those failures.
    fn discover(
        &self,
        want: &Wanted,
        searcher: &dyn Searcher,
        why: &str,
        tried: &mut Vec<String>,
        say: &mut dyn FnMut(String),
    ) -> Outcome {
        use crate::extract_messages as words;
        let part = want.part.as_str();
        let by = searcher.describe();
        let mut doubts: Vec<String> = Vec::new();
        for round in 0..SEARCH_ROUNDS {
            let prompt = if round == 0 {
                say(words::searching(part, why, &by));
                search_prompt(want)
            } else {
                say(words::searching_again(part, &by));
                retry_prompt(want, &doubts)
            };
            let reply = match searcher.search(&prompt) {
                Ok(reply) => reply,
                Err(e) => {
                    return Outcome::Skipped(format!(
                        "the web search for its datasheet failed: {}",
                        first_line(&format!("{e:#}"))
                    ));
                }
            };
            let answer = parse_answer(&reply);
            if answer.urls.is_empty() {
                let note = answer
                    .not_found
                    .as_deref()
                    .filter(|n| !n.is_empty())
                    .map_or_else(
                        || "the agent gave no link".to_string(),
                        |n| format!("the agent said: {n}"),
                    );
                return Outcome::Skipped(words::nothing_found(&note));
            }
            let before = doubts.len();
            for url in &answer.urls {
                match self.try_candidate(want, url, tried, say) {
                    Ok((ready, evidence)) => {
                        say(words::found(
                            part,
                            url,
                            answer.source.as_deref(),
                            &by,
                            &evidence,
                        ));
                        return Outcome::Ready(ready);
                    }
                    Err(why) => {
                        say(words::doubted(part, url, &why));
                        doubts.push(format!("{url}: {why}"));
                    }
                }
            }
            if doubts.len() == before {
                break;
            }
        }
        Outcome::Skipped(format!(
            "no downloaded candidate could be verified as its datasheet ({})",
            doubts.join("; ")
        ))
    }

    /// One link from the agent: not local, not tried already, downloadable, and
    /// vouched for by the PDF's own text. The error is why not.
    fn try_candidate(
        &self,
        want: &Wanted,
        url: &str,
        tried: &mut Vec<String>,
        say: &mut dyn FnMut(String),
    ) -> Result<(Ready, String), String> {
        if fetch::is_local_host(url) {
            return Err(
                "it points at this machine or its network, not a manufacturer's site".into(),
            );
        }
        if tried.iter().any(|t| t == url) {
            return Err("that link was already tried and did not work".into());
        }
        tried.push(url.to_string());
        let ready = self
            .download(url, &want.part, false, say)
            .map_err(|e| e.why())?;
        let text = (self.read_text)(&ready.pdf);
        match datasheet::vouches_for(&want.part, &want.aliases, &text) {
            Ok(vouch) => Ok((ready, vouch.to_string())),
            Err(why) => {
                let _ = std::fs::remove_file(&ready.pdf);
                Err(why)
            }
        }
    }
}

/// How many times the agent is asked: once, and once more when every link it
/// gave failed.
const SEARCH_ROUNDS: usize = 2;

/// The prompt for the second ask: the first one plus what went wrong.
#[must_use]
pub fn retry_prompt(want: &Wanted, failures: &[String]) -> String {
    let list = failures.iter().fold(String::new(), |mut acc, f| {
        acc.push_str("- ");
        acc.push_str(f);
        acc.push('\n');
        acc
    });
    format!(
        "{}\n\nThe links you gave before did not work:\n{list}Do not give those again. Find a \
         different, working link to the same official datasheet (the manufacturer's current \
         product page, its document library, or another official host).",
        search_prompt(want)
    )
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasheet_fetch::{Request, Response, TransportError};
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// A transport that serves fixed bodies by URL and counts requests.
    struct Site {
        pages: HashMap<&'static str, (u16, Vec<u8>)>,
        hits: RefCell<Vec<String>>,
    }

    impl Http for Site {
        fn get(&self, req: &Request<'_>) -> Result<Response, TransportError> {
            self.hits.borrow_mut().push(req.url.to_string());
            match self.pages.get(req.url) {
                Some((status, body)) => {
                    std::fs::write(req.dest, body).unwrap();
                    Ok(Response {
                        status: *status,
                        final_url: req.url.to_string(),
                    })
                }
                None => Err(TransportError {
                    code: Some(6),
                    message: "curl failed: could not resolve host".into(),
                }),
            }
        }
    }

    struct Said(&'static str);

    impl Searcher for Said {
        fn describe(&self) -> String {
            "codex gpt-6-luna".into()
        }
        fn search(&self, _prompt: &str) -> anyhow::Result<String> {
            Ok(self.0.to_string())
        }
    }

    /// A searcher that gives its answers in turn and records each prompt.
    struct Sequence {
        answers: RefCell<Vec<&'static str>>,
        prompts: RefCell<Vec<String>>,
    }

    impl Sequence {
        fn new(answers: &[&'static str]) -> Self {
            Self {
                answers: RefCell::new(answers.iter().rev().copied().collect()),
                prompts: RefCell::default(),
            }
        }
    }

    impl Searcher for Sequence {
        fn describe(&self) -> String {
            "codex gpt-6-luna".into()
        }
        fn search(&self, prompt: &str) -> anyhow::Result<String> {
            self.prompts.borrow_mut().push(prompt.to_string());
            Ok(self
                .answers
                .borrow_mut()
                .pop()
                .unwrap_or("NOT FOUND: out of answers")
                .to_string())
        }
    }

    struct Failing;

    impl Searcher for Failing {
        fn describe(&self) -> String {
            "codex".into()
        }
        fn search(&self, _prompt: &str) -> anyhow::Result<String> {
            anyhow::bail!("codex timed out\nmore detail")
        }
    }

    /// A "PDF" whose body is its own text layer (the tests read it back as
    /// the text, so no `pdftotext` is needed).
    fn pdf_with(text: &str) -> Vec<u8> {
        format!("%PDF-1.4\n{text}").into_bytes()
    }

    /// A PDF with no readable text.
    fn pdf_bytes() -> Vec<u8> {
        pdf_with("x")
    }

    /// A believable datasheet page for `title`, long enough to be judged.
    fn sheet(title: &str) -> Vec<u8> {
        let body = "Features: wide supply range, low input offset, two channels. ".repeat(20);
        pdf_with(&format!(
            "{title} Dual operational amplifier\n{body}\nOrdering information {title}\n"
        ))
    }

    fn want() -> Wanted {
        Wanted {
            part: "LM358B".into(),
            aliases: vec!["LM358B".into()],
            footprints: vec!["Package_SO:SOIC-8_3.9x4.9mm_P1.27mm".into()],
            description: Some("Dual op amp".into()),
            manufacturer: Some("Texas Instruments".into()),
        }
    }

    fn site(pages: Vec<(&'static str, u16, Vec<u8>)>) -> Site {
        Site {
            pages: pages.into_iter().map(|(u, s, b)| (u, (s, b))).collect(),
            hits: RefCell::default(),
        }
    }

    fn acquire(
        http: &Site,
        searcher: Option<&dyn Searcher>,
        source: Option<&str>,
    ) -> (Outcome, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = Vec::new();
        let outcome = Acquirer {
            http,
            searcher,
            dir: dir.path(),
            read_text: |p| String::from_utf8_lossy(&std::fs::read(p).unwrap()).into_owned(),
        }
        .acquire(&want(), source, &mut |l| lines.push(l));
        (outcome, lines)
    }

    #[test]
    fn the_prompt_carries_the_boards_context_and_the_reply_format() {
        let p = search_prompt(&want());
        for needle in [
            "Part number: LM358B",
            "Footprint: Package_SO:SOIC-8",
            "Board description: Dual op amp",
            "Manufacturer: Texas Instruments",
            "URL:",
            "NOT FOUND",
            "OFFICIAL",
        ] {
            assert!(p.contains(needle), "{needle} missing from {p}");
        }
    }

    #[test]
    fn replies_are_parsed_into_candidates_a_source_and_a_refusal() {
        let a = parse_answer(
            "URL: https://www.ti.com/lit/ds/symlink/lm358b.pdf\nSOURCE: https://www.ti.com/product/LM358B\n",
        );
        assert_eq!(a.urls, ["https://www.ti.com/lit/ds/symlink/lm358b.pdf"]);
        assert_eq!(
            a.source.as_deref(),
            Some("https://www.ti.com/product/LM358B")
        );
        let md =
            parse_answer("- URL: [ds](https://a.example/x.pdf).\n- URL: https://b.example/y.pdf\n");
        assert_eq!(
            md.urls,
            ["https://a.example/x.pdf", "https://b.example/y.pdf"]
        );
        let none = parse_answer("NOT FOUND: only distributors list it");
        assert!(none.urls.is_empty());
        assert_eq!(none.not_found.as_deref(), Some("only distributors list it"));
        let bare =
            parse_answer("Here you go: https://c.example/z.pdf\nSOURCE: https://c.example/p");
        assert_eq!(bare.urls, ["https://c.example/z.pdf"]);
        let many = parse_answer("URL: https://1.example/a\nURL: https://2.example/b\nURL: https://3.example/c\nURL: https://4.example/d");
        assert_eq!(many.urls.len(), 3, "at most three candidates");
    }

    /// A local file the board names is used as it is; nothing is fetched or searched.
    #[test]
    fn a_local_board_datasheet_is_used_without_the_network() {
        let http = site(vec![]);
        let (out, lines) = acquire(&http, Some(&Said("never asked")), Some("/boards/ds.pdf"));
        assert_eq!(
            out,
            Outcome::Ready(Ready {
                pdf: PathBuf::from("/boards/ds.pdf")
            })
        );
        assert!(lines.is_empty() && http.hits.borrow().is_empty());
    }

    /// A board URL that downloads is used with no search.
    #[test]
    fn a_board_url_that_downloads_is_used_without_a_search() {
        let http = site(vec![("https://ti.example/ds.pdf", 200, pdf_bytes())]);
        let (out, _) = acquire(&http, Some(&Failing), Some("https://ti.example/ds.pdf"));
        assert!(matches!(out, Outcome::Ready(_)), "{out:?}");
    }

    /// A part with no link and an API backend (no searcher) is skipped with a
    /// reason that the caller turns into the hint.
    #[test]
    fn no_link_and_no_searcher_skips() {
        let http = site(vec![]);
        let (out, _) = acquire(&http, None, None);
        assert!(matches!(out, Outcome::Skipped(_)), "{out:?}");
    }

    /// The positive path: the agent's link downloads, the PDF prints the part
    /// number, and it is used, with the URL, source and evidence shown.
    #[test]
    fn a_found_pdf_that_prints_the_part_is_used_and_shown() {
        let http = site(vec![(
            "https://ti.example/lm358b.pdf",
            200,
            sheet("LM358B"),
        )]);
        let said =
            Said("URL: https://ti.example/lm358b.pdf\nSOURCE: https://ti.example/product/lm358b");
        let (out, lines) = acquire(&http, Some(&said), None);
        assert!(matches!(out, Outcome::Ready(_)), "{out:?} {lines:?}");
        let found = lines
            .iter()
            .find(|l| l.contains("datasheet found by"))
            .unwrap();
        for needle in [
            "LM358B",
            "codex gpt-6-luna",
            "https://ti.example/lm358b.pdf",
            "found on https://ti.example/product/lm358b",
            "the PDF prints LM358B",
        ] {
            assert!(found.contains(needle), "{needle} missing: {found}");
        }
    }

    /// A family sheet is accepted on the family base name; a sheet for another
    /// part of the family, or for something else entirely, is not.
    #[test]
    fn family_names_vouch_and_other_parts_do_not() {
        let mut w = want();
        w.part = "TPS62130".into();
        w.aliases = vec!["TPS62130".into()];
        let http = site(vec![("https://ti.example/f.pdf", 200, sheet("TPS6213x"))]);
        let said = Said("URL: https://ti.example/f.pdf");
        let dir = tempfile::tempdir().unwrap();
        let mut lines = Vec::new();
        let acq = Acquirer {
            http: &http,
            searcher: Some(&said),
            dir: dir.path(),
            read_text: |p| String::from_utf8_lossy(&std::fs::read(p).unwrap()).into_owned(),
        };
        let out = acq.acquire(&w, None, &mut |l| lines.push(l));
        assert!(matches!(out, Outcome::Ready(_)), "{out:?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("TPS6213 family name") || l.contains("family name")),
            "{lines:?}"
        );
        let http = site(vec![("https://ti.example/o.pdf", 200, sheet("NE555"))]);
        let said = Said("URL: https://ti.example/o.pdf");
        let acq = Acquirer {
            http: &http,
            searcher: Some(&said),
            dir: dir.path(),
            read_text: |p| String::from_utf8_lossy(&std::fs::read(p).unwrap()).into_owned(),
        };
        let out = acq.acquire(&w, None, &mut |_| {});
        assert!(matches!(out, Outcome::Skipped(_)), "{out:?}");
    }

    /// The agent's answer is only a lead: a PDF that cannot vouch for the part
    /// (here, one with no readable text) is refused, the file removed, and the
    /// part skipped, never extracted from.
    #[test]
    fn a_found_pdf_that_cannot_vouch_for_the_part_is_skipped() {
        let http = site(vec![("https://ti.example/other.pdf", 200, pdf_bytes())]);
        let said = Said("URL: https://ti.example/other.pdf\nSOURCE: https://ti.example/p");
        let (out, lines) = acquire(&http, Some(&said), None);
        let Outcome::Skipped(why) = out else {
            panic!("expected a skip, got {out:?}");
        };
        assert!(
            why.contains("no downloaded candidate could be verified")
                && why.contains("https://ti.example/other.pdf"),
            "{why}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("searching the web") && l.contains("codex gpt-6-luna")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("not using https://ti.example/other.pdf")),
            "{lines:?}"
        );
    }

    /// Addresses on this machine are never fetched, whatever the agent says.
    #[test]
    fn a_local_address_from_the_agent_is_never_fetched() {
        let http = site(vec![]);
        let said = Said("URL: http://127.0.0.1:9/secret.pdf");
        let (out, _) = acquire(&http, Some(&said), None);
        assert!(matches!(out, Outcome::Skipped(_)));
        assert!(http.hits.borrow().is_empty(), "{:?}", http.hits.borrow());
    }

    #[test]
    fn a_failed_or_empty_search_is_a_skip_with_the_reason() {
        let http = site(vec![]);
        let (out, _) = acquire(&http, Some(&Failing), None);
        let Outcome::Skipped(why) = out else {
            panic!("{out:?}");
        };
        assert!(
            why.contains("web search")
                && why.contains("codex timed out")
                && !why.contains("more detail"),
            "{why}"
        );
        let nf = Said("NOT FOUND: it is a custom part");
        let (out, _) = acquire(&http, Some(&nf), None);
        let Outcome::Skipped(why) = out else {
            panic!("{out:?}");
        };
        assert!(why.contains("the agent said: it is a custom part"), "{why}");
    }

    /// A board link blocked with a 403 falls back to search; if that also
    /// finds nothing the person is told to download it themselves, by name.
    #[test]
    fn a_blocked_board_link_then_no_discovery_says_download_it_yourself() {
        let http = site(vec![("https://m.example/ds.pdf", 403, b"no".to_vec())]);
        let nf = Said("NOT FOUND: nothing official");
        let (out, lines) = acquire(&http, Some(&nf), Some("https://m.example/ds.pdf"));
        let Outcome::Failed(why) = out else {
            panic!("{out:?}");
        };
        assert!(
            why.contains("download it yourself") && why.contains("https://m.example/ds.pdf"),
            "{why}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("HTTP 403") && l.contains("looking for another copy")),
            "{lines:?}"
        );
    }

    /// Without a searcher, a board link that cannot be downloaded fails with
    /// the same instruction.
    #[test]
    fn a_failed_board_link_without_a_searcher_fails_with_instructions() {
        let http = site(vec![("https://m.example/ds.pdf", 404, b"gone".to_vec())]);
        let (out, lines) = acquire(&http, None, Some("https://m.example/ds.pdf"));
        let Outcome::Failed(why) = out else {
            panic!("{out:?}");
        };
        assert!(why.contains("HTTP 404"), "{why}");
        assert!(
            lines.iter().any(|l| l.contains("download it yourself")),
            "{lines:?}"
        );
    }

    /// A link that 404s is told back to the agent, which is asked once more;
    /// the second answer is the one used.
    #[test]
    fn a_dead_link_gets_one_more_search_with_the_failure_in_the_prompt() {
        let http = site(vec![("https://ti.example/good.pdf", 200, sheet("LM358B"))]);
        let said = Sequence::new(&[
            "URL: https://ti.example/dead.pdf",
            "URL: https://ti.example/good.pdf",
        ]);
        let (out, lines) = acquire(&http, Some(&said), None);
        assert!(matches!(out, Outcome::Ready(_)), "{out:?} {lines:?}");
        let prompts = said.prompts.borrow();
        assert_eq!(prompts.len(), 2);
        assert!(!prompts[0].contains("did not work"), "{}", prompts[0]);
        assert!(
            prompts[1].contains("did not work")
                && prompts[1].contains("https://ti.example/dead.pdf"),
            "{}",
            prompts[1]
        );
        assert!(lines.iter().any(|l| l.contains("once more")), "{lines:?}");
    }

    /// The agent is asked at most twice, and a link already tried is not
    /// downloaded again.
    #[test]
    fn a_repeated_dead_link_is_not_fetched_twice_and_searching_stops_at_two_rounds() {
        let http = site(vec![]);
        let said = Sequence::new(&[
            "URL: https://ti.example/dead.pdf",
            "URL: https://ti.example/dead.pdf",
            "URL: https://ti.example/never-asked-for.pdf",
        ]);
        let (out, lines) = acquire(&http, Some(&said), None);
        let Outcome::Skipped(why) = out else {
            panic!("{out:?}");
        };
        assert_eq!(said.prompts.borrow().len(), 2, "asked at most twice");
        let fetched = http
            .hits
            .borrow()
            .iter()
            .filter(|u| u.contains("dead.pdf"))
            .count();
        assert_eq!(fetched, 1, "{:?}", http.hits.borrow());
        assert!(why.contains("already tried"), "{why} {lines:?}");
    }

    /// The board link that failed is not tried again when the agent repeats it.
    #[test]
    fn the_failed_board_link_is_not_downloaded_again_by_discovery() {
        let http = site(vec![("https://m.example/ds.pdf", 404, b"gone".to_vec())]);
        let said = Sequence::new(&["URL: https://m.example/ds.pdf", "NOT FOUND: nothing else"]);
        let (out, _) = acquire(&http, Some(&said), Some("https://m.example/ds.pdf"));
        assert!(matches!(out, Outcome::Failed(_)), "{out:?}");
        let n = http
            .hits
            .borrow()
            .iter()
            .filter(|u| u.as_str() == "https://m.example/ds.pdf")
            .count();
        assert_eq!(n, 1, "{:?}", http.hits.borrow());
    }

    /// Any failed board link (not only a 403) followed by a fruitless search
    /// ends as a failure naming the link, not a quiet skip.
    #[test]
    fn a_board_link_that_will_not_download_then_no_discovery_is_a_named_failure() {
        // Unknown host: the transport fails (curl could not resolve the host).
        let http = site(vec![]);
        let nf = Said("NOT FOUND: nothing official");
        let (out, _) = acquire(&http, Some(&nf), Some("https://gone.example/ds.pdf"));
        let Outcome::Failed(why) = out else {
            panic!("{out:?}");
        };
        assert!(
            why.contains("download it yourself") && why.contains("https://gone.example/ds.pdf"),
            "{why}"
        );
    }
}
