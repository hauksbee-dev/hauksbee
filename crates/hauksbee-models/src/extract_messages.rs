//! The words `models extract` says about finding, downloading and reusing
//! datasheets, in one place.
//!
//! The terminal command prints these and a surface that shows the same events
//! (the web app's extract flow) routes to them rather than keeping its own
//! copy, so a rewording lands everywhere at once. Each function returns one
//! line, ready to print; none prints.

use crate::datasheet::Backend;
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

/// Under `--yes` (and interactively) a part whose datasheet cannot be trusted
/// is left alone.
#[must_use]
pub fn skipped(part: &str, why: &str) -> String {
    format!(
        "skipped {part}: {why}. Nothing was sent for it. Pass {} to use one you trust",
        datasheet_flag(part)
    )
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
#[must_use]
pub fn batch_summary(drafted: usize, kept: usize, skipped: usize, failed: &[&str]) -> String {
    let mut bits = vec![format!("{drafted} drafted")];
    if kept > 0 {
        bits.push(format!("{kept} already had a card"));
    }
    if skipped > 0 {
        bits.push(format!("{skipped} skipped"));
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

#[cfg(test)]
mod tests {
    use super::*;

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
            "Batch: 2 drafted, 1 already had a card, 3 skipped, 2 failed (A, B)"
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
