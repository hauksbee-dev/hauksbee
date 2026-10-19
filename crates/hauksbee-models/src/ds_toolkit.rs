//! `ds`: one-line datasheet helpers for the drafting agent, and the files the
//! harness precomputes with the same code.
//!
//! A strong agent improvises its way round a 200-page datasheet with shell
//! tools: it greps the contents pages for the section it needs, prints one
//! page, parses the pin table's word boxes in a throwaway script, and diffs its
//! draft against what the board says. A cheaper model does less of that, and
//! less well. So each of those moves is a deterministic command here, put on
//! the agent's `PATH` as `ds` (a shim that runs the hidden `hauksbee models ds`),
//! and the harness writes their output into the sandbox before the agent
//! starts, so a model that never calls them still gets the result:
//!
//! | command | precomputed file | what it gives |
//! |---|---|---|
//! | `ds toc` | `toc.md` | contents, tables and figures mapped to PDF pages |
//! | `ds find <regex>` | | page-numbered hits over every page |
//! | `ds page N` | `pages/page-NNN.txt` | one page's text, and its render |
//! | `ds pins` | `pins/<PACKAGE>.tsv` | the pin table column for one package |
//! | `ds board` | `board.md` | the board part's value, footprint, pads, nets |
//! | `ds checklist` | `checklist.md` | the fields this kind needs, and where |
//! | `ds check [file]` | | every validation, binding, pin and coverage finding |
//!
//! The helpers only read the sandbox (and `ds page --render` writes a PNG into
//! it); none touches the network.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{bail, Context, Result};
use regex::Regex;

use crate::card_coverage::{self, Board, Facts};
use crate::pin_table::{self, Column};

/// The sandbox file naming the part and kind being drafted.
pub const PART_FILE: &str = "part.md";
pub const BOARD_FILE: &str = "board.md";
pub const TOC_FILE: &str = "toc.md";
pub const CHECKLIST_FILE: &str = "checklist.md";
pub const PINS_DIR: &str = "pins";

/// One contents line: a section, table or figure and the PDF page it is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocEntry {
    pub title: String,
    /// The page number the contents print.
    pub printed: usize,
    /// The PDF page (1-based) after calibrating printed numbers to the file.
    pub pdf: usize,
}

static TOC_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?P<title>[A-Za-z0-9][^\n]*?[A-Za-z0-9)\]])\s*(?:\.\s?){3,}\s*(?P<page>\d{1,4})\b")
        .expect("toc pattern")
});

fn squash(s: &str) -> String {
    s.to_ascii_lowercase()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect()
}

/// The title's words, less a leading section or table number.
fn bare_title(title: &str) -> &str {
    let t = title.trim();
    let t = t
        .strip_prefix("Table")
        .or_else(|| t.strip_prefix("Figure"))
        .unwrap_or(t)
        .trim_start();
    t.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c.is_whitespace())
}

/// The contents, list of tables and list of figures, mapped to PDF pages.
///
/// Printed page numbers are calibrated to the file: the offset (-3..=12) at
/// which most titles appear on their page wins, so a datasheet whose cover
/// pages are unnumbered still maps right.
#[must_use]
pub fn parse_toc(pages: &[String]) -> Vec<TocEntry> {
    let n = pages.len();
    let mut raw: Vec<(String, usize)> = Vec::new();
    for page in pages.iter().take(12) {
        for line in page.lines() {
            if line.trim_start().starts_with(['•', '–', '-', '*']) {
                continue;
            }
            for c in TOC_LINE.captures_iter(line) {
                let title = c["title"].split_whitespace().collect::<Vec<_>>().join(" ");
                let Ok(printed) = c["page"].parse::<usize>() else {
                    continue;
                };
                if printed == 0 || printed > n + 12 || bare_title(&title).len() < 3 {
                    continue;
                }
                raw.push((title, printed));
            }
        }
    }
    if raw.is_empty() {
        return Vec::new();
    }
    let squashed: Vec<String> = pages.iter().map(|p| squash(p)).collect();
    let on = |title: &str, pdf: usize| {
        let key = squash(bare_title(title));
        let key: String = key.chars().take(24).collect();
        pdf >= 1 && pdf <= n && key.len() >= 4 && squashed[pdf - 1].contains(&key)
    };
    let offset = (-3i64..=12)
        .max_by_key(|off| {
            let hits = raw
                .iter()
                .filter(|(t, p)| {
                    usize::try_from(i64::try_from(*p).unwrap_or(0) + off)
                        .is_ok_and(|pdf| on(t, pdf))
                })
                .count();
            // Ties go to the smallest shift.
            (hits, -off.abs())
        })
        .unwrap_or(0);
    let mut seen = std::collections::HashSet::new();
    raw.into_iter()
        .filter(|e| seen.insert(e.clone()))
        .filter_map(|(title, printed)| {
            let pdf = usize::try_from(i64::try_from(printed).ok()? + offset).ok()?;
            (1..=n).contains(&pdf).then_some(TocEntry {
                title,
                printed,
                pdf,
            })
        })
        .collect()
}

/// Sections a card draws on, by contents title: (label, title pattern).
const KEY_SECTIONS: &[(&str, &str)] = &[
    (
        "pin table",
        r"(?i)pin(out)?( and ball)? (definitions|functions|descriptions?|assignments?)|terminal functions|ball definitions|pin configuration",
    ),
    (
        "absolute maximum ratings",
        r"(?i)^((table )?\d+(\.\d+)*\.? )?(voltage )?absolute maximum|limiting values",
    ),
    (
        "operating conditions",
        r"(?i)(general|recommended) operating conditions",
    ),
    (
        "thermal",
        r"(?i)thermal (information|characteristics)|package thermal",
    ),
    ("boot configuration", r"(?i)\bboot (mode|configuration|pin)"),
    (
        "power supply scheme",
        r"(?i)power supply (scheme|recommendations)|embedded regulator|vcap",
    ),
    (
        "electrical characteristics",
        r"(?i)^(\d+(\.\d+)* )?electrical characteristics|i/o static characteristics",
    ),
    (
        "ordering information",
        r"(?i)ordering information|part numbering",
    ),
];

/// The PDF pages the key sections start on, with their labels, in page order.
#[must_use]
pub fn key_pages(toc: &[TocEntry]) -> Vec<(usize, &'static str)> {
    static RES: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
        KEY_SECTIONS
            .iter()
            .map(|(l, p)| (*l, Regex::new(p).expect("key section pattern")))
            .collect()
    });
    let mut out: BTreeMap<usize, &'static str> = BTreeMap::new();
    for (label, re) in RES.iter() {
        // Every entry that matches: a pin table is often split over several tables.
        for e in toc.iter().filter(|e| re.is_match(&e.title)) {
            out.entry(e.pdf).or_insert(label);
        }
    }
    out.into_iter().collect()
}

/// `toc.md`: key sections first, then every contents line.
#[must_use]
pub fn toc_markdown(toc: &[TocEntry]) -> String {
    let mut out = String::from("# Contents mapped to PDF pages\n\n");
    if toc.is_empty() {
        out.push_str("No contents pages found; use `ds find <regex>` to locate sections.\n");
        return out;
    }
    out.push_str("## Key sections for a card\n\n");
    for (page, label) in key_pages(toc) {
        let title = toc
            .iter()
            .find(|e| e.pdf == page)
            .map_or("", |e| e.title.as_str());
        let _ = writeln!(out, "- {label}: PDF page {page} ({title})");
    }
    out.push_str("\n## Every entry\n\n| PDF page | printed | title |\n|---|---|---|\n");
    for e in toc {
        let _ = writeln!(
            out,
            "| {} | {} | {} |",
            e.pdf,
            e.printed,
            e.title.replace('|', "/")
        );
    }
    out
}

/// Write the precomputed helper files for a prepared sandbox: `toc.md`,
/// `pins/*.tsv` and `pins/index.md`. Best-effort: a missing poppler leaves the
/// pin table out and the rest in place.
#[must_use]
pub fn write_datasheet_files(dir: &Path, pdf: &Path, pages: &[String]) -> Vec<TocEntry> {
    let toc = parse_toc(pages);
    let _ = std::fs::write(dir.join(TOC_FILE), toc_markdown(&toc));
    if let Some(cols) = pin_table::columns_of_pdf(pdf) {
        write_pins(dir, &cols);
    }
    toc
}

fn safe_label(label: &str) -> String {
    label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

fn write_pins(dir: &Path, cols: &[Column]) {
    let pins = dir.join(PINS_DIR);
    if std::fs::create_dir_all(&pins).is_err() {
        return;
    }
    let mut index = String::from(
        "# Pin-table columns read from the PDF's word positions\n\n\
         | package | pads | whole | file |\n|---|---|---|---|\n",
    );
    for c in cols {
        let name = format!("{}.tsv", safe_label(&c.label));
        let _ = std::fs::write(pins.join(&name), c.tsv());
        let _ = writeln!(
            index,
            "| {} | {} | {} | pins/{name} |",
            c.label,
            c.max_pad(),
            if c.is_whole() { "yes" } else { "no" }
        );
    }
    let _ = std::fs::write(pins.join("index.md"), index);
}

/// Write `part.md`, `board.md` and `checklist.md` for one run.
pub fn write_run_files(dir: &Path, part: &str, kind: &str, board: Option<&Board>) {
    let _ = std::fs::write(dir.join(PART_FILE), format!("part: {part}\nkind: {kind}\n"));
    if let Some(b) = board {
        let _ = std::fs::write(dir.join(BOARD_FILE), b.markdown());
    }
    let facts = facts_from_dir(dir);
    if let Some(text) = checklist(dir, &facts) {
        let _ = std::fs::write(dir.join(CHECKLIST_FILE), text);
    }
}

fn read(dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name)).ok()
}

fn part_field(dir: &Path, key: &str) -> Option<String> {
    read(dir, PART_FILE)?.lines().find_map(|l| {
        l.strip_prefix(key)
            .and_then(|r| r.strip_prefix(": "))
            .map(|v| v.trim().to_string())
    })
}

/// The page texts in a sandbox's `pages/` directory, page 1 first.
fn page_texts(dir: &Path) -> Vec<String> {
    let mut found: Vec<(usize, PathBuf)> = std::fs::read_dir(dir.join("pages"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let n = name
                .strip_prefix("page-")?
                .strip_suffix(".txt")?
                .parse()
                .ok()?;
            Some((n, e.path()))
        })
        .collect();
    found.sort();
    let mut out = Vec::new();
    for (n, path) in found {
        out.resize(n.saturating_sub(1).max(out.len()), String::new());
        out.push(std::fs::read_to_string(path).unwrap_or_default());
    }
    out
}

fn pin_columns(dir: &Path) -> Vec<Column> {
    let mut cols: Vec<Column> = std::fs::read_dir(dir.join(PINS_DIR))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let label = name.strip_suffix(".tsv")?;
            let text = std::fs::read_to_string(e.path()).ok()?;
            Some(Column::from_tsv(label, &text))
        })
        .collect();
    cols.sort_by(|a, b| a.label.cmp(&b.label));
    cols
}

/// The facts a sandbox holds: what `ds check` and the harness check against.
#[must_use]
pub fn facts_from_dir(dir: &Path) -> Facts {
    Facts {
        part: part_field(dir, "part").unwrap_or_default(),
        pages: page_texts(dir),
        pin_columns: pin_columns(dir),
        board: read(dir, BOARD_FILE).map(|t| Board::from_markdown(&t)),
        companions: crate::companion_docs::read_all(dir),
    }
}

/// The card kind a kind name (or a `kind/subtype` spelling) denotes.
#[must_use]
pub fn kind_from_name(name: &str) -> Option<crate::schema::ComponentKind> {
    let base = name.split(['/', ' ']).next().unwrap_or(name);
    kind_of(base).or_else(|| crate::datasheet::base_kind_name(base).and_then(kind_of))
}

fn kind_of(name: &str) -> Option<crate::schema::ComponentKind> {
    serde_json::from_value(serde_json::Value::String(name.to_string())).ok()
}

/// The checklist for the sandbox's kind, its roles read from the pin column
/// the board (or a lone column) selects.
fn checklist(dir: &Path, facts: &Facts) -> Option<String> {
    let kind_name = part_field(dir, "kind")?;
    let base = kind_name.split(['/', ' ']).next().unwrap_or(&kind_name);
    let kind =
        kind_of(base).or_else(|| crate::datasheet::base_kind_name(base).and_then(kind_of))?;
    let roles: Vec<String> = chosen_column(facts, None, None)
        .ok()
        .map(|c| {
            c.pads
                .values()
                .flat_map(|(n, _)| pin_table::name_tokens(n).into_iter().skip(1))
                .collect()
        })
        .unwrap_or_default();
    let roles: Vec<&str> = roles.iter().map(String::as_str).collect();
    let mut text =
        card_coverage::checklist_text(kind, &roles, &facts.pages.join("\n"), &facts.pin_columns);
    text.push_str(&crate::card_scope::package_lines(facts));
    if let Some(note) = crate::companion_docs::index_note(dir) {
        let _ = writeln!(
            text,
            "- {note}: search them with `ds find`; a fact the datasheet defers is answered there"
        );
    }
    Some(text)
}

fn chosen_column<'a>(
    facts: &'a Facts,
    package: Option<&str>,
    pads: Option<usize>,
) -> Result<&'a Column, String> {
    // An explicit --package or --pads asks about that column, not the board's.
    let board = facts
        .board
        .as_ref()
        .filter(|_| package.is_none() && pads.is_none());
    let package = package.or_else(|| {
        board
            .map(|b| b.footprint.as_str())
            .filter(|f| !f.is_empty())
    });
    let pads = pads.or_else(|| board.and_then(Board::pad_count));
    if package.is_none() && pads.is_none() && facts.pin_columns.len() > 1 {
        return Err(format!(
            "{} package columns; name one with --package P or --pads N",
            facts.pin_columns.len()
        ));
    }
    pin_table::choose(&facts.pin_columns, package, pads)
}

const USAGE: &str = "ds: datasheet helpers (read-only, offline)
  ds toc                     contents / tables / figures -> PDF pages, key sections first
  ds find <regex>            page-numbered hits on every page, and in companion documents
                             (`DOC p.N`), case-insensitive
  ds page N [--render]       page N's text, and its PNG render
  ds page N --doc DOC        page N of a companion document (`companions/index.md`)
  ds similar [--kind K]      finished library cards of this kind to copy the shape from
  ds pins [--package P | --pads N]
                             the pin table column for one package (pad, name, page)
  ds board                   the board part: value, footprint, pad count, nets
  ds checklist               the fields this kind of card needs, and where they live
  ds check [model.toml]      every validation, binding, pin and coverage finding, as fixes";

/// Run one `ds` command in sandbox `dir`; returns what to print.
///
/// # Errors
///
/// An unknown command, a bad argument, or a missing sandbox file.
pub fn main(dir: &Path, args: &[String]) -> Result<String> {
    let Some((cmd, rest)) = args.split_first() else {
        return Ok(USAGE.to_string());
    };
    let flag = |name: &str| {
        rest.iter()
            .position(|a| a == name)
            .and_then(|i| rest.get(i + 1))
            .cloned()
    };
    match cmd.as_str() {
        "toc" => {
            Ok(read(dir, TOC_FILE).unwrap_or_else(|| toc_markdown(&parse_toc(&page_texts(dir)))))
        }
        "find" => find(dir, rest),
        "similar" => crate::card_templates::similar(
            dir,
            &part_field(dir, "part").unwrap_or_default(),
            &part_field(dir, "kind").unwrap_or_default(),
            flag("--kind").as_deref(),
        ),
        "page" => page(dir, rest),
        "pins" => {
            let facts = facts_from_dir(dir);
            if facts.pin_columns.is_empty() {
                bail!("no pin table was read from this PDF; read the pinout pages (`ds toc`)");
            }
            let pads = flag("--pads").map(|n| n.parse::<usize>()).transpose()?;
            let package = flag("--package");
            match chosen_column(&facts, package.as_deref(), pads) {
                Ok(c) => {
                    let mut out = format!("# {} column ({} pads)", c.label, c.max_pad());
                    let missing = c.missing();
                    if !missing.is_empty() {
                        let _ = write!(out, "; pads not read: {missing:?} (check the PDF)");
                    }
                    out.push('\n');
                    out.push_str(&c.tsv());
                    Ok(out)
                }
                Err(e) => Ok(format!(
                    "{e}\n{}",
                    read(&dir.join(PINS_DIR), "index.md").unwrap_or_default()
                )),
            }
        }
        "board" => Ok(read(dir, BOARD_FILE).unwrap_or_else(|| {
            "No board part for this run (a single-PDF extraction): match the card to the \
             part number, and to every package the datasheet orders it in."
                .to_string()
        })),
        "checklist" => Ok(read(dir, CHECKLIST_FILE)
            .or_else(|| checklist(dir, &facts_from_dir(dir)))
            .unwrap_or_else(|| "No kind recorded for this sandbox (part.md).".to_string())),
        "check" => {
            let file = rest
                .first()
                .map_or_else(|| dir.join("model.toml"), |f| dir.join(f));
            let raw = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            let part = part_field(dir, "part").unwrap_or_default();
            let kind = part_field(dir, "kind").unwrap_or_default();
            let facts = facts_from_dir(dir);
            let problems = crate::datasheet::check_draft(&raw, &part, &kind, &facts);
            Ok(if problems.is_empty() {
                "ds check: clean".to_string()
            } else {
                let mut out = format!("ds check: {} finding(s)\n", problems.len());
                for p in problems {
                    let _ = writeln!(out, "- {p}");
                }
                out
            })
        }
        // Not on the agent's list: builds a sandbox's files outside a run, to
        // replay saved drafts and score page selection.
        "prepare" => {
            let pdf = rest.first().context(
                "usage: ds prepare <pdf> [--part P] [--kind K] [--footprint F] [--value V]",
            )?;
            let (before, now) = crate::datasheet::prepare_files(Path::new(pdf), dir)?;
            let board =
                (flag("--footprint").is_some() || flag("--value").is_some()).then(|| Board {
                    footprint: flag("--footprint").unwrap_or_default(),
                    value: flag("--value").unwrap_or_default(),
                    ..Board::default()
                });
            write_run_files(
                dir,
                &flag("--part").unwrap_or_default(),
                &flag("--kind").unwrap_or_default(),
                board.as_ref(),
            );
            Ok(format!(
                "selected before: {before:?}\nselected now: {now:?}"
            ))
        }
        "help" | "--help" | "-h" => Ok(USAGE.to_string()),
        other => bail!("unknown ds command '{other}'\n{USAGE}"),
    }
}

fn find(dir: &Path, rest: &[String]) -> Result<String> {
    let Some(pattern) = rest.first() else {
        bail!("usage: ds find <regex>");
    };
    let re = Regex::new(&format!("(?i){pattern}")).context("the pattern is not a valid regex")?;
    let mut out = String::new();
    let mut hits = 0;
    for (i, text) in page_texts(dir).iter().enumerate() {
        for line in text.lines().filter(|l| re.is_match(l)) {
            hits += 1;
            if hits <= 80 {
                let line: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
                let line: String = line.chars().take(160).collect();
                let _ = writeln!(out, "p.{}: {line}", i + 1);
            }
        }
    }
    for (doc, pages) in crate::companion_docs::read_all(dir) {
        for (i, text) in pages.iter().enumerate() {
            for line in text.lines().filter(|l| re.is_match(l)) {
                hits += 1;
                if hits <= 80 {
                    let line: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
                    let line: String = line.chars().take(160).collect();
                    let _ = writeln!(out, "{doc} p.{}: {line}", i + 1);
                }
            }
        }
    }
    if hits > 80 {
        let _ = writeln!(out, "... {} more; narrow the pattern", hits - 80);
    }
    if hits == 0 {
        out.push_str("no hits\n");
    }
    Ok(out)
}

fn page(dir: &Path, rest: &[String]) -> Result<String> {
    let n: usize = rest
        .first()
        .context("usage: ds page N [--render]")?
        .parse()
        .context("N is a PDF page number")?;
    if let Some(i) = rest.iter().position(|a| a == "--doc") {
        let doc = rest.get(i + 1).context("usage: ds page N --doc DOC")?;
        let pages = crate::companion_docs::read_all(dir)
            .into_iter()
            .find(|(d, _)| d.eq_ignore_ascii_case(doc))
            .map(|(_, p)| p)
            .with_context(|| format!("no companion document '{doc}' (see companions/index.md)"))?;
        let text = pages
            .get(n.saturating_sub(1))
            .with_context(|| format!("{doc} has {} pages", pages.len()))?;
        return Ok(format!("=== {doc} page {n} ===\n\n{text}"));
    }
    let text = read(dir, &format!("pages/page-{n:03}.txt"))
        .with_context(|| format!("no page {n} in this PDF"))?;
    let png = dir.join(format!("page-{n:03}.png"));
    if rest.iter().any(|a| a == "--render") && !png.is_file() {
        let stem = dir.join(format!("page-{n:03}"));
        let _ = std::process::Command::new("pdftoppm")
            .args([
                "-png",
                "-singlefile",
                "-r",
                "110",
                "-f",
                &n.to_string(),
                "-l",
                &n.to_string(),
            ])
            .arg(dir.join("datasheet.pdf"))
            .arg(&stem)
            .status();
    }
    let render = if png.is_file() {
        format!("render: page-{n:03}.png (open it to read tables and drawings)")
    } else {
        format!("render: none yet (`ds page {n} --render` makes page-{n:03}.png)")
    };
    Ok(format!("=== PDF page {n} ===\n{render}\n\n{text}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contents_lines_map_to_pdf_pages_with_the_offset_calibrated() {
        let mut pages = vec![
            "Contents\n1 Introduction . . . . . . . . 3\n6.2 Absolute maximum ratings . . . . . 5   \
             Table 7. Pin definitions . . . . . 4\n• Changed bullet .......... 1\n"
                .to_string(),
            "cover".into(),
            "notes".into(),
        ];
        // Printed page N is PDF page N+1.
        pages.push("1 Introduction\ntext".into());
        pages.push("Table 7. Pin definitions\n1 VDD".into());
        pages.push("6.2 Absolute maximum ratings\nVDD 4.0".into());
        let toc = parse_toc(&pages);
        let get = |t: &str| toc.iter().find(|e| e.title.contains(t)).unwrap().pdf;
        assert_eq!(get("Introduction"), 4);
        assert_eq!(get("Pin definitions"), 5);
        assert_eq!(get("Absolute maximum"), 6);
        assert!(!toc.iter().any(|e| e.title.contains("bullet")));
        let keys = key_pages(&toc);
        assert!(keys.contains(&(5, "pin table")), "{keys:?}");
        assert!(keys.contains(&(6, "absolute maximum ratings")), "{keys:?}");
        assert!(toc_markdown(&toc).contains("- pin table: PDF page 5"));
    }

    #[test]
    fn helpers_read_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("pages")).unwrap();
        std::fs::write(
            dir.path().join("pages/page-001.txt"),
            "VIN range 2.0 to 5.5 V",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pages/page-002.txt"),
            "EN pin must not float",
        )
        .unwrap();
        let board = Board {
            refdes: "U1".into(),
            value: "TLV70033".into(),
            footprint: "SOT-23-5".into(),
            pads: vec![],
        };
        std::fs::create_dir_all(dir.path().join(PINS_DIR)).unwrap();
        std::fs::write(
            dir.path().join("pins/SOT.tsv"),
            "# pad\tname\tpdf_page\n1\tIN\t2\n2\tGND\t2\n3\tEN\t2\n4\tNC\t2\n5\tOUT\t2\n",
        )
        .unwrap();
        write_run_files(dir.path(), "TLV70033", "vreg", Some(&board));
        let run = |a: &[&str]| {
            main(
                dir.path(),
                &a.iter().map(ToString::to_string).collect::<Vec<_>>(),
            )
            .unwrap()
        };
        assert!(run(&["find", "must not"]).contains("p.2: EN pin must not float"));
        assert!(run(&["page", "1"]).contains("=== PDF page 1 ==="));
        assert!(run(&["board"]).contains("footprint: SOT-23-5"));
        let pins = run(&["pins"]);
        assert!(pins.contains("3\tEN\t2"), "{pins}");
        let list = run(&["checklist"]);
        assert!(list.contains("supply_range:in"), "{list}");
        assert!(list.contains("enable"), "{list}");
        assert!(main(dir.path(), &["nonsense".to_string()]).is_err());
    }
}
