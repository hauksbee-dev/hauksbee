//! Companion documents: the application note or reference manual a datasheet
//! defers a fact to.
//!
//! A datasheet is often not the whole answer. An MCU's boot-mode truth table
//! is in its reference manual or a hardware guide, a regulator's layout
//! limits in an application note; the datasheet says "refer to RM0090" and
//! stops. A drafter confined to the datasheet then records the group as
//! unknown, or fills it from memory. A person extracting the card opens the
//! document the datasheet names. So does the harness, once, before the agent
//! starts:
//!
//! 1. [`deferrals`] reads the datasheet's text for the checklist groups it
//!    cannot answer itself and the documents it names for them. Only a
//!    document the datasheet names is fetched; none is guessed.
//! 2. [`fetch`] finds the named document with the same agent web search the
//!    datasheet discovery uses ([`crate::datasheet_discovery`]), downloads it
//!    with the same transport and limits ([`crate::datasheet_fetch::fetch_pdf`]:
//!    http(s) only, never a local host, a PDF by its magic bytes, bounded
//!    time) and accepts it only when its own text names the document
//!    ([`vouches`]).
//! 3. [`write_to`] puts each document's page text in the sandbox under
//!    `companions/<DOC>/`, with `index.md` (what it is, where from, its
//!    sha256) and per-group `hits`, so `ds find` and `ds page` reach it and the
//!    provenance check counts its numbers.
//!
//! Provenance is kept three ways: the card's header comment, a
//! `[[models.source.references]]` row with the document's sha256
//! ([`references`]), and the trace.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::card_coverage::Need;
use crate::datasheet_discovery::{parse_answer, Searcher};
use crate::datasheet_fetch::{self as fetch, Http};

/// Documents fetched for one run, at most.
pub const MAX_DOCS: usize = 2;
/// Pages of one document kept (a reference manual runs to 2000).
pub const MAX_PAGES: usize = 3000;
/// The sandbox directory.
pub const DIR: &str = "companions";
/// Agent searches per document: once, and once more with the failures.
const ROUNDS: usize = 2;

/// A checklist group the datasheet defers, and the document it names for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deferral {
    /// The checklist key (`straps`).
    pub need: String,
    /// The document's identifier (`RM0090`, `AN4488`) or, when the datasheet
    /// names it only by title, that title (`STM32F4xx reference manual`).
    pub doc: String,
    /// The datasheet's own words naming it.
    pub phrase: String,
    /// PDF page (1-based) the datasheet names it on.
    pub page: usize,
}

/// What a checklist group is about, in the words a document uses for it, and
/// whether the datasheet already prints it.
struct Topic {
    key: &'static str,
    /// Lines of the datasheet about the group.
    about: &'static str,
    /// Text that shows the datasheet prints the group's table itself.
    printed: &'static str,
    /// Document kinds that hold it when the datasheet names no document for
    /// the group itself: a reference manual documents boot configuration.
    manual_ok: bool,
}

const TOPICS: &[Topic] = &[Topic {
    key: "straps",
    about: r"(?i)\bboot(?:\s*(?:mode|pin|configuration|loader|option))?s?\b|\bstrap",
    // A truth table: the boot pins and the memory they select, on one page.
    printed: r"(?is)boot[01]?.{0,400}(?:main flash|user flash|system memory).{0,400}boot[01]|boot mode selection pins",
    manual_ok: true,
}];

static DOC_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(?:AN\s?\d{3,5}|RM\s?\d{4}|UM\s?\d{4,5}|PM\s?\d{4}|(?:SLVA|SLUA|SNVA|SBVA|SPRA|SZZA|SLAA|SLLA|SCAA|SNOA|SBOA|SLVU|SPRU|SWRU|SBAU)\d{3,4}[A-Z]?)\b",
    )
    .expect("document id")
});
static MANUAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)((?:[A-Z][A-Za-z0-9]+(?:\s|-))?[A-Z0-9]*[Xx]*\s*(?:reference manual|technical reference manual|user'?s? (?:guide|manual)|programming manual))")
        .expect("manual title")
});
static DEFER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:refer|see|described|details|detailed|available|further|more information|in conjunction)\b")
        .expect("deferral verb")
});

fn squash_id(id: &str) -> String {
    id.chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_uppercase()
}

/// The documents the datasheet names for the checklist groups in `needs`
/// that it does not print itself.
///
/// A line about the group that names a document is the strongest evidence;
/// failing that, for a group a reference manual documents, any reference
/// manual the datasheet names. At most [`MAX_DOCS`], each once.
///
/// # Panics
/// Never: the topic patterns are fixed and tested.
#[must_use]
pub fn deferrals<S: AsRef<str>>(pages: &[S], needs: &[Need]) -> Vec<Deferral> {
    let mut out: Vec<Deferral> = Vec::new();
    for topic in TOPICS {
        if !needs.iter().any(|n| n.key == topic.key) {
            continue;
        }
        let printed = Regex::new(topic.printed).expect("printed pattern");
        if pages
            .iter()
            .any(|p| !p.as_ref().contains(".....") && printed.is_match(p.as_ref()))
        {
            continue;
        }
        let about = Regex::new(topic.about).expect("about pattern");
        let mut near: Option<Deferral> = None;
        // A manual named by its identifier (`RM0090`) is better than one named by title.
        let mut manual: Option<Deferral> = None;
        let mut titled: Option<Deferral> = None;
        for (pi, page) in pages.iter().enumerate() {
            let lines: Vec<&str> = page.as_ref().lines().collect();
            // A contents page names sections, not documents.
            if page.as_ref().contains(".....") {
                continue;
            }
            for (li, line) in lines.iter().enumerate() {
                let window = lines[li.saturating_sub(2)..(li + 3).min(lines.len())].join(" ");
                if let Some(id) = DOC_ID.find(line) {
                    if about.is_match(&window) && DEFER.is_match(&window) && near.is_none() {
                        near = Some(Deferral {
                            need: topic.key.into(),
                            doc: squash_id(id.as_str()),
                            phrase: one_line(line),
                            page: pi + 1,
                        });
                    }
                    if topic.manual_ok
                        && manual.is_none()
                        && id.as_str().starts_with("RM")
                        && line.to_ascii_lowercase().contains("reference manual")
                    {
                        manual = Some(Deferral {
                            need: topic.key.into(),
                            doc: squash_id(id.as_str()),
                            phrase: one_line(line),
                            page: pi + 1,
                        });
                    }
                } else if topic.manual_ok && titled.is_none() && DEFER.is_match(&window) {
                    if let Some(title) = MANUAL.find(line) {
                        let title = one_line(title.as_str());
                        if title.to_ascii_lowercase().contains("reference manual")
                            && title.split_whitespace().count() >= 3
                        {
                            titled = Some(Deferral {
                                need: topic.key.into(),
                                doc: title,
                                phrase: one_line(line),
                                page: pi + 1,
                            });
                        }
                    }
                }
            }
        }
        if let Some(d) = near.or(manual).or(titled) {
            if !out.iter().any(|o| o.doc == d.doc) {
                out.push(d);
            }
        }
    }
    out.truncate(MAX_DOCS);
    out
}

fn one_line(s: &str) -> String {
    let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    s.chars().take(160).collect()
}

/// The web-search prompt for one deferral.
#[must_use]
pub fn search_prompt(part: &str, d: &Deferral) -> String {
    format!(
        "Find the OFFICIAL manufacturer PDF of this document, which the datasheet of {part} \
         names. Use web search. Prefer the manufacturer's own website over distributors and \
         aggregators. The datasheet says: \"{phrase}\".\n\nDocument: {doc}\n\n\
         Reply with only these lines, and nothing else:\n\
         URL: <the direct https link to the PDF file>\n\
         SOURCE: <the page where you found the link>\n\
         If you cannot find that exact document, reply with exactly one line: NOT FOUND: <why>",
        phrase = d.phrase,
        doc = d.doc
    )
}

/// Whether `text` (a downloaded PDF's) is the document `d` names: it prints
/// the identifier on its first pages, or, for a document named only by title,
/// every distinctive word of the title.
///
/// # Errors
/// The reason, as a clause, when it cannot vouch.
pub fn vouches(d: &Deferral, pages: &[String]) -> Result<(), String> {
    let head: String = pages.iter().take(6).cloned().collect::<Vec<_>>().join("\n");
    if head.trim().len() < 200 {
        return Err("the PDF has no readable text on its first pages".into());
    }
    let squashed = squash_id(&head);
    if DOC_ID.is_match(&d.doc) {
        return if squashed.contains(&squash_id(&d.doc)) {
            Ok(())
        } else {
            Err(format!("its first pages never print {}", d.doc))
        };
    }
    let low = head.to_ascii_lowercase();
    let words: Vec<String> = d
        .doc
        .split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_ascii_lowercase()
        })
        .filter(|w| w.len() >= 3)
        .collect();
    // Family names print with an `x` wildcard (`STM32F4xx`): compare the stem.
    let missing: Vec<&String> = words
        .iter()
        .filter(|w| !low.contains(w.trim_end_matches('x')))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "its first pages never print {}",
            missing
                .iter()
                .map(|w| format!("'{w}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

/// One fetched, verified document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Companion {
    pub doc: String,
    pub need: String,
    pub url: String,
    pub sha256: String,
    pub title: String,
    pub pages: Vec<String>,
}

/// Download the document `d` names and verify it.
///
/// Asks `searcher` for the link (up to [`ROUNDS`] times, the second time
/// with the failures), downloads each candidate with `http` into `dir`, and
/// takes the first whose text [`vouches`]. `read_pages` turns a PDF into its
/// page texts.
///
/// # Errors
/// Every reason no candidate was accepted.
pub fn fetch(
    part: &str,
    d: &Deferral,
    http: &dyn Http,
    searcher: &dyn Searcher,
    dir: &Path,
    read_pages: &dyn Fn(&Path) -> Vec<String>,
) -> Result<Companion, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let dest = dir.join(format!(
        "{}.pdf",
        crate::datasheet::sanitise_filename(&d.doc)
    ));
    let mut failures: Vec<String> = Vec::new();
    let mut tried: Vec<String> = Vec::new();
    for round in 0..ROUNDS {
        let mut prompt = search_prompt(part, d);
        if round > 0 {
            let _ = write!(
                prompt,
                "\n\nThe links you gave before did not work:\n- {}\nDo not give those again.",
                failures.join("\n- ")
            );
        }
        let reply = searcher
            .search(&prompt)
            .map_err(|e| format!("the search for {} failed: {e:#}", d.doc))?;
        let answer = parse_answer(&reply);
        if answer.urls.is_empty() {
            return Err(format!(
                "the agent found no link for {}{}",
                d.doc,
                answer
                    .not_found
                    .map(|n| format!(": {n}"))
                    .unwrap_or_default()
            ));
        }
        let before = failures.len();
        for url in &answer.urls {
            if fetch::is_local_host(url) {
                failures.push(format!("{url}: it points at this machine or its network"));
                continue;
            }
            if tried.contains(url) {
                continue;
            }
            tried.push(url.clone());
            let got = match fetch::fetch_pdf(http, url, &dest, &d.doc) {
                Ok(g) => g,
                Err(e) => {
                    failures.push(format!("{url}: {}", e.why()));
                    continue;
                }
            };
            let pages = read_pages(&got.path);
            match vouches(d, &pages) {
                Ok(()) => {
                    let bytes = std::fs::read(&got.path).map_err(|e| e.to_string())?;
                    let sha = Sha256::digest(&bytes)
                        .iter()
                        .fold(String::new(), |mut s, b| {
                            let _ = write!(s, "{b:02x}");
                            s
                        });
                    let mut pages = pages;
                    pages.truncate(MAX_PAGES);
                    return Ok(Companion {
                        doc: d.doc.clone(),
                        need: d.need.clone(),
                        url: got.url,
                        sha256: sha,
                        title: d.phrase.clone(),
                        pages,
                    });
                }
                Err(why) => {
                    let _ = std::fs::remove_file(&got.path);
                    failures.push(format!("{url}: {why}"));
                }
            }
        }
        if failures.len() == before {
            break;
        }
    }
    Err(format!(
        "no candidate for {} could be verified ({})",
        d.doc,
        failures.join("; ")
    ))
}

static ABOUT: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    TOPICS
        .iter()
        .map(|t| Regex::new(t.about).expect("about pattern"))
        .collect()
});

/// Whether a companion page is about one of the groups a datasheet defers
/// (at least three mentions): only such pages vouch for a card's numbers.
#[must_use]
pub fn on_topic(page: &str) -> bool {
    !page.contains(".....") && ABOUT.iter().any(|re| re.find_iter(page).count() >= 3)
}

/// The pages of `c` most about the group it was fetched for: the pages
/// matching the topic's wording, densest first.
///
/// # Panics
/// Never: the topic patterns are fixed and tested.
#[must_use]
pub fn hit_pages(c: &Companion) -> Vec<(usize, usize)> {
    let Some(topic) = TOPICS.iter().find(|t| t.key == c.need) else {
        return Vec::new();
    };
    let re = Regex::new(topic.about).expect("about pattern");
    let mut hits: Vec<(usize, usize)> = c
        .pages
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.contains("....."))
        .map(|(i, p)| (i + 1, re.find_iter(p).count()))
        .filter(|(_, n)| *n >= 3)
        .collect();
    hits.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    hits.truncate(8);
    hits
}

/// Write the companions into the sandbox: each document's page texts under
/// `companions/<DOC>/page-NNNN.txt`, and `companions/index.md`.
pub fn write_to(dir: &Path, docs: &[Companion]) {
    if docs.is_empty() {
        return;
    }
    let root = dir.join(DIR);
    let mut index = String::from(
        "# Companion documents\n\nFetched because the datasheet names them for a group this \
         kind of card needs. Search them with `ds find` (hits are labelled `DOC p.N`) and read \
         a page with `ds page N --doc DOC`.\n\n| doc | for | url | sha256 | pages |\n|---|---|---|---|---|\n",
    );
    for c in docs {
        let sub = root.join(crate::datasheet::sanitise_filename(&c.doc));
        if std::fs::create_dir_all(&sub).is_err() {
            continue;
        }
        for (i, page) in c.pages.iter().enumerate() {
            let _ = std::fs::write(sub.join(format!("page-{:04}.txt", i + 1)), page);
        }
        let _ = writeln!(
            index,
            "| {} | {} | {} | {} | {} |",
            c.doc,
            c.need,
            c.url,
            c.sha256,
            c.pages.len()
        );
        let hits = hit_pages(c);
        if !hits.is_empty() {
            let _ = writeln!(
                index,
                "\nMost about `{}` in {}: {}\n",
                c.need,
                c.doc,
                hits.iter()
                    .map(|(p, n)| format!("p.{p} ({n} mentions)"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    let _ = std::fs::write(root.join("index.md"), index);
}

/// A row of `companions/index.md`.
fn rows(dir: &Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(dir.join(DIR).join("index.md"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("| ") && l.contains("http"))
        .map(|l| {
            l.trim_matches('|')
                .split('|')
                .map(|c| c.trim().to_string())
                .collect()
        })
        .collect()
}

/// The companions a sandbox holds, as one provenance sentence.
#[must_use]
pub fn index_note(dir: &Path) -> Option<String> {
    let docs: Vec<String> = rows(dir)
        .into_iter()
        .filter_map(|r| {
            Some(format!(
                "{} ({}, sha256 {})",
                r.first()?,
                r.get(2)?,
                r.get(3)?
            ))
        })
        .collect();
    (!docs.is_empty()).then(|| format!("companion documents: {}", docs.join(", ")))
}

/// The companions' page texts, by document.
#[must_use]
pub fn read_all(dir: &Path) -> Vec<(String, Vec<String>)> {
    rows(dir)
        .into_iter()
        .filter_map(|r| r.first().cloned())
        .map(|doc| {
            let sub = dir
                .join(DIR)
                .join(crate::datasheet::sanitise_filename(&doc));
            let mut files: Vec<PathBuf> = std::fs::read_dir(&sub)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "txt"))
                .collect();
            files.sort();
            let pages = files
                .iter()
                .map(|f| std::fs::read_to_string(f).unwrap_or_default())
                .collect();
            (doc, pages)
        })
        .collect()
}

/// The `[[models.source.references]]` rows for the companions a sandbox
/// holds: title, locator and sha256.
#[must_use]
pub fn references(dir: &Path) -> Vec<(String, String, String)> {
    rows(dir)
        .into_iter()
        .filter_map(|r| {
            Some((
                format!("{} (companion document)", r.first()?),
                format!("{}; the whole PDF, as fetched", r.get(2)?),
                r.get(3)?.clone(),
            ))
        })
        .collect()
}

/// Everything the harness does about companions before the agent starts:
/// find the deferrals among `needs`, fetch each, write them into `dir`.
/// Returns the one-line notes to print and trace.
pub fn prepare(
    dir: &Path,
    part: &str,
    pages: &[String],
    needs: &[Need],
    http: &dyn Http,
    searcher: &dyn Searcher,
    read_pages: &dyn Fn(&Path) -> Vec<String>,
) -> Vec<String> {
    let mut notes = Vec::new();
    let mut got: Vec<Companion> = Vec::new();
    let cache = dir.join("companion-downloads");
    for d in deferrals(pages, needs) {
        notes.push(format!(
            "the datasheet defers '{}' to {} (p.{}: \"{}\")",
            d.need, d.doc, d.page, d.phrase
        ));
        match fetch(part, &d, http, searcher, &cache, read_pages) {
            Ok(c) => {
                notes.push(format!(
                    "fetched {} from {} ({} pages, sha256 {})",
                    c.doc,
                    c.url,
                    c.pages.len(),
                    c.sha256
                ));
                got.push(c);
            }
            Err(why) => notes.push(format!("could not fetch {}: {why}", d.doc)),
        }
    }
    write_to(dir, &got);
    let _ = std::fs::remove_dir_all(&cache);
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasheet_fetch::{Request, Response, TransportError};

    fn need(key: &str) -> Need {
        Need {
            key: key.into(),
            ask: String::new(),
            look: "x",
        }
    }

    const STM_BOOT: &str = "3.13 Boot modes\n At startup, boot pins are used to select one out of \
        three boot options:\n Boot from user flash\n Boot from system memory\n";
    const STM_RM: &str = "intro\n For more details refer to the application note\n \
        (refer to RM0090 reference manual).\n";

    #[test]
    fn a_reference_manual_the_datasheet_names_is_the_deferral_for_boot_straps() {
        let d = deferrals(&[STM_BOOT, STM_RM], &[need("straps"), need("theta_ja")]);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].doc, "RM0090");
        assert_eq!(d[0].need, "straps");
        assert_eq!(d[0].page, 2);
    }

    #[test]
    fn only_pages_about_the_deferred_group_vouch_for_numbers() {
        assert!(on_topic(
            "Boot configuration\n BOOT0 BOOT1 select the boot mode\n boot from flash"
        ));
        assert!(!on_topic(
            "Table of registers 0x4000 0000 reset value 0x0000"
        ));
        assert!(!on_topic("Boot ..... 12\n boot ..... 13\n boot ..... 14"));
    }

    #[test]
    fn a_boot_line_naming_an_application_note_wins_and_a_printed_table_defers_nothing() {
        let near = "Boot configuration: for the boot mode selection\n see application note AN2606 for details\n";
        let d = deferrals(&[STM_RM, near], &[need("straps")]);
        assert_eq!(d[0].doc, "AN2606");
        let printed = "BOOT1 BOOT0 Boot mode\n x 0 Main Flash memory\n 0 1 System memory\n 1 1 Embedded SRAM BOOT0 BOOT1";
        assert!(deferrals(&[printed, STM_RM], &[need("straps")]).is_empty());
        // Nothing is named, nothing is fetched; and an absent need defers nothing.
        assert!(deferrals(&[STM_BOOT], &[need("straps")]).is_empty());
        assert!(deferrals(&[STM_RM], &[need("backend")]).is_empty());
    }

    #[test]
    fn a_manual_named_only_by_title_is_found_and_vouched_by_its_words() {
        let ds = "The datasheet should be read in conjunction with the\n STM32F4xx reference manual which is available from the website\n";
        let d = deferrals(&[ds], &[need("straps")]);
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].doc.contains("reference manual"), "{}", d[0].doc);
        let title = format!("{}\nSTM32F405/415, STM32F407/417, STM32F427/437 and STM32F429/439 advanced Arm-based 32-bit MCUs reference manual\n{}", "RM0090", "x".repeat(300));
        assert!(vouches(&d[0], &[title]).is_ok());
        assert!(vouches(
            &d[0],
            &["a different reference manual for another family".repeat(10)]
        )
        .is_err());
    }

    struct Site;
    impl Http for Site {
        fn get(&self, req: &Request<'_>) -> Result<Response, TransportError> {
            let body: &[u8] = if req.url.contains("good") {
                b"%PDF-1.4 RM0090 reference manual"
            } else {
                b"%PDF-1.4 something else"
            };
            std::fs::write(req.dest, body).unwrap();
            Ok(Response {
                status: 200,
                final_url: req.url.to_string(),
            })
        }
    }

    struct Said(&'static str);
    impl Searcher for Said {
        fn describe(&self) -> String {
            "agent".into()
        }
        fn search(&self, _prompt: &str) -> anyhow::Result<String> {
            Ok(self.0.to_string())
        }
    }

    fn read(path: &Path) -> Vec<String> {
        let t = String::from_utf8_lossy(&std::fs::read(path).unwrap()).into_owned();
        vec![format!("{t} {}", "boot ".repeat(200)); 4]
    }

    #[test]
    fn only_a_verified_document_lands_in_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let d = Deferral {
            need: "straps".into(),
            doc: "RM0090".into(),
            phrase: "refer to RM0090 reference manual".into(),
            page: 7,
        };
        let notes = prepare(
            dir.path(),
            "STM32F407VET6",
            &[STM_RM.to_string()],
            &[need("straps")],
            &Site,
            &Said("URL: https://www.st.com/good/rm0090.pdf\nSOURCE: https://www.st.com/"),
            &read,
        );
        assert!(
            notes.iter().any(|n| n.starts_with("fetched RM0090")),
            "{notes:?}"
        );
        assert!(dir.path().join("companions/RM0090/page-0001.txt").is_file());
        let note = index_note(dir.path()).unwrap();
        assert!(note.contains("RM0090") && note.contains("sha256"), "{note}");
        assert_eq!(read_all(dir.path())[0].1.len(), 4);
        assert_eq!(references(dir.path())[0].0, "RM0090 (companion document)");
        assert!(!dir.path().join("companion-downloads").exists());

        // A candidate that does not print the document id is refused, and so is a local host.
        let other = tempfile::tempdir().unwrap();
        let notes = prepare(
            other.path(),
            "STM32F407VET6",
            &[STM_RM.to_string()],
            &[need("straps")],
            &Site,
            &Said("URL: https://www.st.com/other/x.pdf\nURL: http://localhost/y.pdf"),
            &read,
        );
        assert!(
            notes.iter().any(|n| n.contains("could not fetch RM0090")),
            "{notes:?}"
        );
        assert!(!other.path().join("companions").exists());
        let _ = d;
    }
}
