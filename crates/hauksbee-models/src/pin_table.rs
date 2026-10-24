//! A datasheet's pin table, read deterministically from word positions.
//!
//! A model reading `pdftotext` text alone sees a six-package pin table as rows
//! of numbers whose columns drift wherever a cell is blank, and measured, a
//! cheaper model took ten pads of an LQFP100 map from the LQFP144 column and
//! lint passed it. Word boxes (`pdftotext -bbox`) keep the columns: each
//! package's header word fixes an x band, every pad number in that band is a
//! row, and the pin name is the name-column text on the same line. The harness
//! writes one `pins/<PACKAGE>.tsv` per column it finds, the `ds pins` helper
//! prints them, and [`crate::ds_toolkit`] compares a draft's pin map against the
//! column its pad count and package select.
//!
//! The reuse decision (poppler's word boxes over pdfplumber, camelot or a Rust
//! PDF crate) and the measured accuracy are in

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

/// One word and its box, in PDF points from the page's top left.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
    pub text: String,
}

impl Word {
    const fn xc(&self) -> f64 {
        f64::midpoint(self.x0, self.x1)
    }
    const fn yc(&self) -> f64 {
        f64::midpoint(self.y0, self.y1)
    }
}

/// One package column: its header label and pad -> (printed name, PDF page).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Column {
    pub label: String,
    pub pads: BTreeMap<String, (String, usize)>,
    /// Pads given two different names (a parse doubt, never silently resolved).
    pub conflicts: Vec<String>,
}

impl Column {
    /// The highest numeric pad, the package's pad count when the table is whole.
    #[must_use]
    pub fn max_pad(&self) -> usize {
        self.pads
            .keys()
            .filter_map(|p| p.parse::<usize>().ok())
            .max()
            .unwrap_or(0)
    }

    /// Numeric pads 1..=max with none missing.
    #[must_use]
    pub fn missing(&self) -> Vec<usize> {
        (1..=self.max_pad())
            .filter(|n| !self.pads.contains_key(&n.to_string()))
            .collect()
    }

    /// Whole (every pad 1..=max named) and free of conflicting names.
    #[must_use]
    pub fn is_whole(&self) -> bool {
        self.max_pad() >= 3 && self.missing().is_empty() && self.conflicts.is_empty()
    }

    /// The table as TSV: `pad<TAB>name<TAB>page`, pads in numeric order.
    #[must_use]
    pub fn tsv(&self) -> String {
        let mut rows: Vec<_> = self.pads.iter().collect();
        rows.sort_by_key(|(p, _)| pad_key(p));
        let mut out = String::from("# pad\tname\tpdf_page\n");
        for conflict in &self.conflicts {
            let _ = writeln!(out, "{CONFLICT}{conflict}");
        }
        for (pad, (name, page)) in rows {
            let _ = writeln!(out, "{pad}\t{name}\t{page}");
        }
        out
    }

    /// Read back a TSV written by [`Column::tsv`].
    #[must_use]
    pub fn from_tsv(label: &str, text: &str) -> Self {
        let mut col = Self {
            label: label.to_string(),
            ..Self::default()
        };
        col.conflicts = text
            .lines()
            .filter_map(|l| l.strip_prefix(CONFLICT))
            .map(str::to_string)
            .collect();
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let mut f = line.split('\t');
            if let (Some(pad), Some(name)) = (f.next(), f.next()) {
                let page = f.next().and_then(|p| p.trim().parse().ok()).unwrap_or(0);
                col.pads
                    .insert(pad.trim().to_string(), (name.trim().to_string(), page));
            }
        }
        col
    }
}

/// The TSV comment that carries a pad read with two names.
const CONFLICT: &str = "# two names read for ";

fn pad_key(p: &str) -> (usize, String) {
    (p.parse().unwrap_or(usize::MAX), p.to_string())
}

/// `pdftotext -bbox` output (an XHTML page of `<word>` boxes) as words per page.
#[must_use]
pub fn parse_bbox(html: &str) -> Vec<Vec<Word>> {
    let mut pages = Vec::new();
    for page in html.split("<page ").skip(1) {
        let mut words = Vec::new();
        for chunk in page.split("<word ").skip(1) {
            let attr = |name: &str| -> Option<f64> {
                let at = chunk.find(&format!("{name}=\""))? + name.len() + 2;
                let end = chunk[at..].find('"')? + at;
                chunk[at..end].parse().ok()
            };
            let text = chunk
                .split_once('>')
                .and_then(|(_, rest)| rest.split_once("</word>"))
                .map(|(t, _)| unescape(t));
            if let (Some(x0), Some(y0), Some(x1), Some(y1), Some(text)) =
                (attr("xMin"), attr("yMin"), attr("xMax"), attr("yMax"), text)
            {
                words.push(Word {
                    x0,
                    y0,
                    x1,
                    y1,
                    text,
                });
            }
        }
        pages.push(words);
    }
    pages
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Package families a pin table's column header names. Letters only: the pad
/// count, when printed, follows (`LQFP100`, `SOT-23`, `6-Pin`).
const FAMILIES: &[&str] = &[
    "LQFP", "TQFP", "PQFP", "QFP", "LQFN", "VQFN", "WQFN", "UQFN", "XQFN", "HVQFN", "UFQFPN",
    "VFQFPN", "QFN", "UDFN", "WDFN", "DFN", "WSON", "VSON", "USON", "X2SON", "SON", "UFBGA",
    "TFBGA", "LFBGA", "VFBGA", "FBGA", "BGA", "WLCSP", "DSBGA", "CSP", "HTSSOP", "TSSOP", "MSOP",
    "VSSOP", "SSOP", "SOIC", "SOP", "SO", "TSOT", "SOT", "SC70", "PDIP", "DIP", "TO",
];

/// The header's family and pad count, when it names a package.
#[must_use]
pub fn package_of(label: &str) -> Option<(&'static str, Option<usize>)> {
    let up: String = label
        .to_ascii_uppercase()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    // The longest family the label starts with and ends in digits after:
    // "SOT" is SOT, not SO followed by a stray T.
    let family = FAMILIES
        .iter()
        .filter(|f| up.starts_with(**f) && up[f.len()..].chars().all(|c| c.is_ascii_digit()))
        .max_by_key(|f| f.len())?;
    let rest = &up[family.len()..];
    // A bare "TO" is the English word, not a package.
    if *family == "TO" && rest.is_empty() {
        return None;
    }
    // SC70, SOT23 and TO220 name an outline, not a count.
    let count = rest
        .parse::<usize>()
        .ok()
        .filter(|_| !(matches!(*family, "SC70" | "TO") || (*family == "SOT" && rest.len() >= 2)));
    Some((family, count))
}

fn is_pad_number(s: &str) -> bool {
    !s.is_empty() && s.len() <= 3 && s.chars().all(|c| c.is_ascii_digit()) && s != "0"
}

fn is_name_header(s: &str) -> bool {
    matches!(
        s.trim_end_matches(':').to_ascii_lowercase().as_str(),
        "name" | "pin" | "number" | "names" | "no." | "terminal" | "signal" | "symbol" | "symbols"
    )
}

/// A header that labels the pin-name column itself (not the pad numbers).
fn is_name_column_header(s: &str) -> bool {
    matches!(
        s.trim_end_matches(':').to_ascii_lowercase().as_str(),
        "name" | "names" | "symbol" | "symbols"
    )
}

/// Cell text that is a column of its own (type, I/O structure, a blank).
fn is_type_cell(s: &str) -> bool {
    matches!(
        s,
        "I" | "O"
            | "I/O"
            | "IO"
            | "S"
            | "P"
            | "G"
            | "FT"
            | "FTf"
            | "TTa"
            | "TC"
            | "B"
            | "RST"
            | "-"
            | "—"
            | "–"
            | "A"
            | "AI"
            | "AO"
            | "PWR"
            | "GND"
            | "NC"
    ) && s != "GND"
        && s != "NC"
}

/// Text in the name band that is not the pin's name: a parenthesised alias
/// ("(JTDO/", "TRACESWO)"), a column header, or another column's cell.
fn not_a_name(s: &str) -> bool {
    s.starts_with('(')
        || (s.ends_with(')') && !s.contains('('))
        || is_name_header(s)
        || is_type_cell(s)
}

fn name_like(s: &str) -> bool {
    let first = s.chars().next().unwrap_or(' ');
    (first.is_ascii_alphabetic() || first == '/' || first == '_')
        && !s.starts_with('(')
        && !is_type_cell(s)
        && package_of(s).is_none()
}

struct Header {
    label: String,
    x: f64,
    /// The header word's left edge (a left-aligned column starts here).
    x0: f64,
    y1: f64,
    tol: f64,
    /// A single-package table's number column: a wrapped list of pads
    /// ("3, 5, 8, 11,14,") is centred on the header but its words are not.
    wide: bool,
}

/// The pad numbers a cell word gives: `4,` -> 4; `11,14,` -> 11 and 14. A word
/// with a leading zero or anything else in it is no pad cell.
fn pad_tokens(text: &str) -> Option<Vec<String>> {
    let parts: Vec<&str> = text.split(',').filter(|p| !p.is_empty()).collect();
    (!parts.is_empty()
        && parts
            .iter()
            .all(|p| is_pad_number(p) && !p.starts_with('0')))
    .then(|| parts.into_iter().map(str::to_string).collect())
}

/// The pad cells under a header, in no particular order.
fn header_cells<'a>(words: &'a [Word], h: &Header) -> Vec<&'a Word> {
    let mut cells: Vec<&Word> = words
        .iter()
        .filter(|w| (w.xc() - h.x).abs() <= h.tol && w.y0 > h.y1 - 1.0)
        // "0" only ever continues a wrapped number ("10" over "0").
        .filter(|w| pad_tokens(&w.text).is_some() || w.text == "0")
        .collect();
    if h.wide {
        let mut all: Vec<&Word> = words
            .iter()
            .filter(|w| w.y0 > h.y1 - 1.0 && pad_tokens(&w.text).is_some())
            .collect();
        all.sort_by(|a, b| a.yc().total_cmp(&b.yc()));
        let mut lines: Vec<Vec<&Word>> = Vec::new();
        for w in all {
            match lines.last_mut() {
                Some(l) if (l[0].yc() - w.yc()).abs() < 1.5 => l.push(w),
                _ => lines.push(vec![w]),
            }
        }
        for mut line in lines {
            line.sort_by(|a, b| a.x0.total_cmp(&b.x0));
            let mut runs: Vec<Vec<&Word>> = Vec::new();
            for w in line {
                match runs.last_mut() {
                    Some(r) if w.x0 - r[r.len() - 1].x1 < 8.0 => r.push(w),
                    _ => runs.push(vec![w]),
                }
            }
            for run in runs {
                let (x0, x1) = (run[0].x0, run[run.len() - 1].x1);
                let centred = (f64::midpoint(x0, x1) - h.x).abs() <= h.tol;
                if centred || (x0 - h.x0).abs() < 6.0 {
                    for w in run {
                        if !cells.iter().any(|c| std::ptr::eq(*c, w)) {
                            cells.push(w);
                        }
                    }
                }
            }
        }
    }
    cells
}

/// The label of a pin table that names no package: the datasheet's only table,
/// read from a NO. / NUMBER column ([`columns`]).
pub const SINGLE_TABLE: &str = "TABLE";

/// A page's columns: (label, [(pad, printed name)]).
type PageColumns = Vec<(String, Vec<(String, String)>)>;

/// Every package column of every pin table in `pages` (index 0 is PDF page 1).
///
/// A datasheet whose pin table has no package header (TI's `NAME | NO.` for a
/// part sold in one package) has no such column; its one NO. / NUMBER column is
/// then read as a single table labelled [`SINGLE_TABLE`], for the board's package.
#[must_use]
pub fn columns(pages: &[Vec<Word>]) -> Vec<Column> {
    let found = collect_columns(pages, page_columns, false);
    if found.is_empty() {
        collect_columns(pages, single_page_columns, true)
    } else {
        found
    }
}

/// `split`: a page whose rows contradict the table so far (a datasheet with a
/// pin table per device or package, none naming its package) starts the next
/// table (`TABLE2`), never a column of conflicts.
fn collect_columns(
    pages: &[Vec<Word>],
    page_fn: fn(&[Word]) -> PageColumns,
    split: bool,
) -> Vec<Column> {
    let mut by_label: BTreeMap<String, Column> = BTreeMap::new();
    let mut current = SINGLE_TABLE.to_string();
    let mut last_table: Option<usize> = None;
    for (index, words) in pages.iter().enumerate() {
        let found = page_fn(words);
        // A page is a pin table when some column has a few rows, or when it
        // continues the previous page's table (its last row or two).
        let table = found.iter().any(|(_, rows)| rows.len() >= 3)
            || (!found.is_empty()
                && last_table == Some(index.wrapping_sub(1))
                && found
                    .iter()
                    .all(|(l, _)| by_label.get(l).is_some_and(|c| c.pads.len() >= 3)));
        if !table {
            continue;
        }
        last_table = Some(index);
        for (label, rows) in found {
            let label = if split {
                let clash = by_label.get(&current).is_some_and(|c| {
                    rows.iter().any(|(p, n)| {
                        p != EXPOSED_PAD
                            && c.pads.get(p).is_some_and(|(had, _)| norm(had) != norm(n))
                    })
                });
                if clash {
                    current = format!("{SINGLE_TABLE}{}", by_label.len() + 1);
                }
                current.clone()
            } else {
                label
            };
            let col = by_label.entry(label.clone()).or_insert_with(|| Column {
                label,
                ..Column::default()
            });
            for (pad, name) in rows {
                match col.pads.get(&pad) {
                    // The exposed pad is read two ways; never a parse doubt.
                    Some((had, _)) if pad != EXPOSED_PAD && norm(had) != norm(&name) => {
                        col.conflicts
                            .push(format!("pad {pad}: '{had}' or '{name}'"));
                    }
                    Some(_) => {}
                    None => {
                        col.pads.insert(pad, (name, index + 1));
                    }
                }
            }
        }
    }
    by_label
        .into_values()
        .filter(|c| c.pads.len() >= 3)
        .collect()
}

/// The pad key of an exposed / thermal pad the table gives no number.
pub const EXPOSED_PAD: &str = "EP";

/// A page's package columns: (label, [(pad, printed name)]).
fn page_columns(words: &[Word]) -> PageColumns {
    let names: Vec<&Word> = words.iter().filter(|w| is_name_header(&w.text)).collect();
    let mut heads: Vec<Header> = words
        .iter()
        .filter(|w| package_of(&w.text).is_some())
        // A table header sits beside a NAME / Pin / number header; a pinout
        // drawing's caption ("6-Pin WSON", "5-Pin SOT Top View") does not.
        .filter(|w| {
            names
                .iter()
                .any(|n| (n.yc() - w.yc()).abs() < 30.0 && !std::ptr::eq(*n, *w))
                && !words.iter().any(|v| {
                    (v.yc() - w.yc()).abs() < 1.5
                        && v.x1 <= w.x0
                        && w.x0 - v.x1 < 6.0
                        && v.text.to_ascii_lowercase().ends_with("-pin")
                })
        })
        .map(|w| Header {
            label: w.text.to_ascii_uppercase().replace(['-', ' '], ""),
            x: w.xc(),
            x0: w.x0,
            y1: w.y1,
            tol: 0.0,
            wide: false,
        })
        .collect();
    if heads.is_empty() {
        return Vec::new();
    }
    let xs: Vec<f64> = heads.iter().map(|h| h.x).collect();
    for h in &mut heads {
        let nearest = xs
            .iter()
            .filter(|x| (**x - h.x).abs() > 1.0)
            .map(|x| (x - h.x).abs())
            .fold(f64::INFINITY, f64::min);
        h.tol = (nearest / 2.0).clamp(4.0, 12.0);
    }
    page_rows(words, &heads, &names)
}

/// The rows of every header's column on a page.
fn page_rows(words: &[Word], heads: &[Header], names: &[&Word]) -> PageColumns {
    // Row anchors: every pad cell (number or blank) in any column, so a
    // name cell's vertical extent ends where the next row of ANY package starts.
    let in_any = |w: &Word| {
        heads
            .iter()
            .find(|h| (w.xc() - h.x).abs() <= h.tol && w.y0 > h.y1 - 1.0)
    };
    let mut anchors: Vec<f64> = words
        .iter()
        .filter(|w| {
            (is_pad_number(&w.text) || matches!(w.text.as_str(), "-" | "–" | "—"))
                && in_any(w).is_some()
        })
        .map(Word::yc)
        .collect();
    // A wrapped list of pads is a row of its own on every line.
    let wide: Vec<&Word> = heads
        .iter()
        .filter(|h| h.wide)
        .flat_map(|h| header_cells(words, h))
        .collect();
    anchors.extend(wide.iter().map(|w| w.yc()));
    anchors.sort_by(f64::total_cmp);
    anchors.dedup_by(|a, b| (*a - *b).abs() < 3.0);
    let band = name_band(words, heads, names, &wide);
    heads
        .iter()
        .map(|h| (h.label.clone(), column_rows(words, h, &anchors, band)))
        .filter(|(_, rows)| !rows.is_empty())
        .collect()
}

/// A header that numbers the pads of a table with no package column.
fn is_number_header(s: &str) -> bool {
    matches!(
        s.trim_end_matches(':').to_ascii_lowercase().as_str(),
        "no." | "no" | "number" | "numbers"
    )
}

/// `Pin` or `Pin[3]` as the number column's header (the table's one
/// package is implied), where there is no NO. / NUMBER header.
fn is_pin_header(s: &str) -> bool {
    let low = s.to_ascii_lowercase();
    low.strip_prefix("pin").is_some_and(|rest| {
        rest.is_empty() || rest.starts_with('[') || rest.chars().all(|c| c.is_ascii_digit())
    })
}

/// A page's one pin table that names no package: the single NO. / NUMBER
/// column beside a NAME column. Skipped unless exactly one such column has a
/// few pad rows (a page with two is not read, never guessed at).
fn single_page_columns(words: &[Word]) -> PageColumns {
    let names: Vec<&Word> = words.iter().filter(|w| is_name_header(&w.text)).collect();
    let is_label = |w: &Word| {
        matches!(
            w.text.trim_end_matches(':').to_ascii_lowercase().as_str(),
            "name" | "names" | "signal" | "terminal"
        )
    };
    let beside_label = |c: &Word| {
        names
            .iter()
            .any(|n| is_label(n) && (n.yc() - c.yc()).abs() < 30.0 && !std::ptr::eq(*n, c))
    };
    let mut cands: Vec<&Word> = words.iter().filter(|w| is_number_header(&w.text)).collect();
    if cands.is_empty() {
        cands = words.iter().filter(|w| is_pin_header(&w.text)).collect();
    }
    let heads: Vec<Header> = cands
        .into_iter()
        .filter(|c| beside_label(c))
        .map(|c| Header {
            label: SINGLE_TABLE.to_string(),
            x: c.xc(),
            x0: c.x0,
            y1: c.y1,
            tol: ((c.x1 - c.x0) / 2.0 + 6.0).clamp(8.0, 16.0),
            wide: true,
        })
        .filter(|h| {
            let mut ys: Vec<f64> = header_cells(words, h).iter().map(|w| w.yc()).collect();
            ys.sort_by(f64::total_cmp);
            ys.dedup_by(|a, b| (*a - *b).abs() < 3.0);
            ys.len() >= 3
        })
        .collect();
    if heads.len() != 1 {
        return Vec::new();
    }
    page_rows(words, &heads, &names)
}

/// The vertical extent of the row at `y`: halfway to the rows either side.
fn row_extent(anchors: &[f64], y: f64) -> (f64, f64) {
    let i = anchors.partition_point(|a| *a < y - 2.0);
    let lo = if i == 0 {
        y - 12.0
    } else {
        f64::midpoint(anchors[i - 1], y)
    };
    let next = anchors.iter().find(|a| **a > y + 2.0);
    let hi = next.map_or(y + 12.0, |n| f64::midpoint(*n, y));
    (lo, hi)
}

/// One pad cell: its pads, vertical position, and whether its line ends in a comma
/// (the list goes on below).
struct Cell {
    pads: Vec<String>,
    y: f64,
    x1: f64,
    open: bool,
}

/// One header's rows on a page: (pad, printed name) for every pad cell in its band.
fn column_rows(
    words: &[Word],
    h: &Header,
    anchors: &[f64],
    band: Option<(f64, f64)>,
) -> Vec<(String, String)> {
    let mut cells = header_cells(words, h);
    cells.sort_by(|a, b| a.y0.total_cmp(&b.y0));
    let mut found: Vec<Cell> = Vec::new();
    let mut used = std::collections::HashSet::new();
    for (i, w) in cells.iter().enumerate() {
        if used.contains(&i) || w.text == "0" {
            continue;
        }
        let mut pads = pad_tokens(&w.text).unwrap_or_default();
        let mut y = w.yc();
        // A number wrapped in a narrow column ("10" over "0" is pad 100).
        if let Some((j, below)) = cells.iter().enumerate().find(|(j, v)| {
            *j > i
                && v.text.len() == 1
                && (6.0..14.0).contains(&(v.y0 - w.y0))
                && pads.len() == 1
                && pads[0].len() == 2
        }) {
            let below_is_row = anchors
                .iter()
                .any(|a| (a - below.yc()).abs() < 1.0 && (a - w.yc()).abs() > 3.0);
            if !below_is_row || below.text == "0" {
                pads[0].push_str(&below.text);
                used.insert(j);
                // The wrapped cell is centred on its row.
                // Snapped to another column's cell on that row when one is near.
                let mid = f64::midpoint(w.yc(), below.yc());
                y = anchors
                    .iter()
                    .copied()
                    .filter(|a| (a - mid).abs() < 6.0)
                    .min_by(|a, b| (a - mid).abs().total_cmp(&(b - mid).abs()))
                    .unwrap_or(mid);
            }
        }
        // "4, 5": one row, several pads.
        pads.extend(
            words
                .iter()
                .filter(|v| {
                    (v.yc() - w.yc()).abs() < 1.5
                        && v.x0 > w.x1
                        && v.x0 - w.x1 < 8.0
                        && w.text.ends_with(',')
                })
                .filter_map(|v| pad_tokens(&v.text))
                .flatten(),
        );
        found.push(Cell {
            pads,
            y,
            x1: w.x1,
            open: w.text.ends_with(','),
        });
    }
    // Cells on one line are one row; a line ending in a comma goes on below it
    // (one name, centred on the whole list).
    let mut blocks: Vec<(Vec<String>, f64, f64, bool, f64)> = Vec::new();
    for c in found {
        match blocks.last_mut() {
            Some(b) if (c.y - b.2).abs() < 1.5 => {
                b.0.extend(c.pads);
                if c.x1 > b.4 {
                    b.3 = c.open;
                    b.4 = c.x1;
                }
            }
            Some(b) if b.3 && (4.0..16.0).contains(&(c.y - b.2)) => {
                b.0.extend(c.pads);
                b.2 = c.y;
                b.3 = c.open;
                b.4 = c.x1;
            }
            _ => blocks.push((c.pads, c.y, c.y, c.open, c.x1)),
        }
    }
    let mut rows = Vec::new();
    for (pads, first, last, _, _) in blocks {
        let extent = (row_extent(anchors, first).0, row_extent(anchors, last).1);
        let name = band.map_or_else(String::new, |b| name_at(words, b, extent, h.wide));
        if name.is_empty() {
            continue;
        }
        for pad in pads {
            rows.push((pad, name.clone()));
        }
    }
    if let Some(name) = exposed_pad(words, h, anchors, band, &cells) {
        rows.push((EXPOSED_PAD.to_string(), name));
    }
    rows
}

/// Whether letters spell an exposed / thermal pad ("THERMALPAD", "EP", "PAD").
fn spells_exposed_pad(letters: &str) -> bool {
    matches!(
        letters,
        "EP" | "EPAD" | "PAD" | "DAP" | "EXPOSEDPAD" | "THERMALPAD" | "POWERPAD" | "DIEPAD"
    ) || ["THERMAL", "EXPOSED", "POWERPAD", "EPAD"]
        .iter()
        .any(|p| letters.starts_with(p) && letters.contains("PAD"))
}

/// The name of an exposed pad the table gives no number: a "Thermal Pad" cell
/// in the number column (its row's name is the pad's name), or a name cell
/// that says so in a row with no number.
fn exposed_pad(
    words: &[Word],
    h: &Header,
    anchors: &[f64],
    band: Option<(f64, f64)>,
    cells: &[&Word],
) -> Option<String> {
    let numbered = |y: f64| cells.iter().any(|c| (c.yc() - y).abs() < 4.0);
    let letters = |ws: &[&Word]| -> String {
        ws.iter()
            .flat_map(|w| w.text.chars())
            .filter(char::is_ascii_alphabetic)
            .collect::<String>()
            .to_ascii_uppercase()
    };
    // In the number column, a wrapped "THERM / AL PAD".
    let mut text: Vec<&Word> = words
        .iter()
        .filter(|w| {
            w.y0 > h.y1 - 1.0
                && w.x0 < h.x + h.tol
                && w.x1 > h.x - h.tol
                && w.text
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic())
        })
        .collect();
    text.sort_by(|a, b| a.y0.total_cmp(&b.y0).then(a.x0.total_cmp(&b.x0)));
    let mut groups: Vec<Vec<&Word>> = Vec::new();
    for w in text {
        match groups.last_mut() {
            Some(g) if w.y0 - g[g.len() - 1].y0 < 14.0 => g.push(w),
            _ => groups.push(vec![w]),
        }
    }
    for g in &groups {
        let (lo, hi) = g
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), w| {
                (lo.min(w.yc()), hi.max(w.yc()))
            });
        if spells_exposed_pad(&letters(g)) && !numbered(lo) && !numbered(hi) {
            let name = band.map_or_else(String::new, |b| {
                name_at(words, b, (lo - 6.0, hi + 6.0), h.wide)
            });
            return Some(if name.is_empty() {
                EXPOSED_PAD.to_string()
            } else {
                name
            });
        }
    }
    // In the name column, a row that is the exposed pad and has no number.
    let b = band?;
    words
        .iter()
        .filter(|w| w.y0 > h.y1 - 1.0 && w.xc() >= b.0 && w.xc() <= b.1)
        .filter(|w| {
            matches!(
                w.text.to_ascii_lowercase().as_str(),
                "thermal" | "exposed" | "powerpad" | "ep" | "epad" | "pad" | "dap"
            ) || w.text.to_ascii_lowercase().starts_with("powerpad")
        })
        .find_map(|w| {
            let name = name_at(words, b, row_extent(anchors, w.yc()), h.wide);
            let letters: String = name
                .chars()
                .filter(char::is_ascii_alphabetic)
                .collect::<String>()
                .to_ascii_uppercase();
            (spells_exposed_pad(&letters) && !numbered(w.yc())).then_some(name)
        })
}

/// The x band of the pin-name column: a NAME header's, else the column of
/// name-like words that lines up with the most pad rows.
fn name_band(
    words: &[Word],
    heads: &[Header],
    names: &[&Word],
    extra_rows: &[&Word],
) -> Option<(f64, f64)> {
    let top = heads.iter().map(|h| h.y1).fold(f64::INFINITY, f64::min);
    let in_head = |w: &Word| heads.iter().any(|h| (w.xc() - h.x).abs() <= h.tol);
    let rows: Vec<&Word> = words
        .iter()
        .filter(|w| w.y0 > top && is_pad_number(&w.text) && in_head(w))
        .chain(extra_rows.iter().copied())
        .collect();
    let on_row = |w: &Word| rows.iter().any(|r| (r.yc() - w.yc()).abs() < 1.5);
    let mut xs: Vec<(f64, f64, f64)> = words
        .iter()
        .filter(|w| w.y0 > top && name_like(&w.text) && !in_head(w) && on_row(w))
        .map(|w| (w.xc(), w.x0, w.x1))
        .collect();
    if xs.is_empty() {
        return None;
    }
    xs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut clusters: Vec<Vec<(f64, f64, f64)>> = Vec::new();
    for x in xs {
        match clusters.last_mut() {
            Some(c) if x.0 - c.last().map_or(x.0, |l| l.0) < 12.0 => c.push(x),
            _ => clusters.push(vec![x]),
        }
    }
    // A "Symbol" header (NXP) labels the name column as "Name" (TI) does; a
    // table without one is told apart by the cluster that lines up with most rows,
    // and its description column's first words can outnumber the real names.
    let header = names
        .iter()
        .find(|n| is_name_column_header(&n.text))
        .map(|n| n.xc());
    let best = clusters.iter().max_by(|a, b| {
        let score = |c: &Vec<(f64, f64, f64)>| {
            let near = header.map_or(0, |h| {
                usize::from(c.iter().any(|x| (x.0 - h).abs() < 40.0)) * 1000
            });
            near + c.len()
        };
        score(a)
            .cmp(&score(b))
            .then_with(|| b[0].0.total_cmp(&a[0].0))
    })?;
    // A single-package table's Name column may hold two names side by side
    // (Cypress: `SCB0_0  GPIO_8`): take every cluster that sits under the header.
    let wide = heads.iter().any(|h| h.wide);
    let chosen: Vec<&Vec<(f64, f64, f64)>> = match header {
        Some(hx) if wide => clusters
            .iter()
            .filter(|c| c.iter().any(|x| (x.0 - hx).abs() < 40.0))
            .collect(),
        _ => vec![best],
    };
    let lo = chosen
        .iter()
        .flat_map(|c| c.iter().map(|x| x.1))
        .fold(f64::INFINITY, f64::min);
    let hi = chosen
        .iter()
        .flat_map(|c| c.iter().map(|x| x.2))
        .fold(f64::NEG_INFINITY, f64::max);
    Some((lo - 4.0, hi + 4.0))
}

/// The name cell text in `band` between `lo` and `hi`: subscripts glued on,
/// a wrapped name joined, footnote marks dropped.
fn name_at(words: &[Word], band: (f64, f64), (lo, hi): (f64, f64), drop_titles: bool) -> String {
    let mut cell: Vec<&Word> = words
        .iter()
        .filter(|w| w.xc() >= band.0 && w.xc() <= band.1 && w.yc() > lo && w.yc() < hi)
        .filter(|w| !not_a_name(&w.text))
        .collect();
    // A section title ("USB Downstream Signals") sits in the name column alone
    // on its line: nothing else of the table shares it. A name's own lines do.
    if drop_titles {
        let accompanied = |w: &Word| {
            words
                .iter()
                .any(|v| (v.yc() - w.yc()).abs() < 2.5 && (v.xc() < band.0 || v.xc() > band.1))
        };
        // (a name wrapped at a slash goes on below, so that line stays)
        let goes_on = |w: &Word| {
            cell.iter()
                .any(|u| (u.yc() - w.yc()).abs() < 2.5 && u.text.ends_with(['/', '-']))
        };
        if cell.iter().any(|w| accompanied(w)) {
            let kept: Vec<&Word> = cell
                .iter()
                .copied()
                .filter(|w| accompanied(w) || goes_on(w))
                .collect();
            cell = kept;
        }
    }
    // Reading order, so a cell holding a long wrapped name keeps it whole.
    cell.sort_by(|a, b| a.y0.total_cmp(&b.y0).then(a.x0.total_cmp(&b.x0)));
    let mut text = String::new();
    let mut last: Option<&Word> = None;
    for w in cell {
        let glue = last.is_none_or(|l| {
            let same_line = (w.y0 - l.y0).abs() < 6.0;
            (same_line && w.x0 - l.x1 < 1.5) || text.ends_with(['/', '-', '_'])
        });
        if !glue {
            text.push(if (w.y0 - last.map_or(w.y0, |l| l.y0)).abs() < 6.0 {
                ' '
            } else {
                '/'
            });
        }
        text.push_str(&w.text);
        last = Some(w);
    }
    text.trim().trim_end_matches('/').to_string()
}

/// A printed pin name or a card role, reduced for comparison: lower case,
/// `+`/`-` spelled out at the end, separators dropped.
#[must_use]
pub fn norm(s: &str) -> String {
    let s = s.trim().to_ascii_lowercase();
    let s = s
        .strip_suffix('+')
        .map_or_else(|| s.clone(), |b| format!("{b}plus"));
    let s = s
        .strip_suffix('-')
        .map_or_else(|| s.clone(), |b| format!("{b}minus"));
    s.chars().filter(char::is_ascii_alphanumeric).collect()
}

/// The names a printed cell gives one pad: `PB2/BOOT1` -> pb2, boot1.
#[must_use]
pub fn name_tokens(printed: &str) -> Vec<String> {
    let mut out = vec![norm(printed)];
    for part in printed.split(['/', ' ']) {
        out.push(norm(part));
        // PC14-OSC32_IN: a hyphen joining two names (not a trailing minus).
        if let Some((a, b)) = part.split_once('-') {
            if b.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
                out.push(norm(a));
                out.push(norm(b));
            }
        }
    }
    out.retain(|t| !t.is_empty());
    out
}

/// Run `pdftotext -bbox` on `pdf` and parse its columns. `None` without poppler.
#[must_use]
pub fn columns_of_pdf(pdf: &Path) -> Option<Vec<Column>> {
    let out = std::process::Command::new("pdftotext")
        .arg("-bbox")
        .arg(pdf)
        .arg("-")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| columns(&parse_bbox(&String::from_utf8_lossy(&out.stdout))))
}

/// The column a board footprint or package name and pad count select.
///
/// The name may be `Package_QFP:LQFP-100_14x14mm`, `SOT-23-5` or `LQFP100`.
/// The outline must agree when both name one ([`crate::package::outline`]).
/// The pad count (given, or read off the name by
/// [`crate::package::pin_count`]) must equal the count the column header
/// prints, or, for a header without one, the column's highest pad. Several
/// columns with the same pin map (SOT and SC70 sharing one) are no ambiguity;
/// the first is returned.
///
/// # Errors
/// No column fits, or several fit with different pin maps; the message names
/// the columns found.
pub fn choose<'a>(
    cols: &'a [Column],
    package: Option<&str>,
    pads: Option<usize>,
) -> Result<&'a Column, String> {
    let want_outline = package.and_then(crate::package::outline);
    let pads = pads.or_else(|| {
        package
            .and_then(crate::package::pin_count)
            .and_then(|n| usize::try_from(n).ok())
    });
    let fits: Vec<&Column> = cols
        .iter()
        .filter(|c| {
            let outline_ok = want_outline
                .zip(crate::package::outline(&c.label))
                .is_none_or(|(a, b)| a == b);
            let count = package_of(&c.label).and_then(|p| p.1);
            // A header's printed count wins over a table missing its last row.
            let count_ok = pads.is_none_or(|n| {
                count.map_or_else(|| c.max_pad() == n, |k| k == n && c.max_pad() <= n)
            });
            outline_ok && count_ok
        })
        .collect();
    let labels = |v: &[&Column]| {
        v.iter()
            .map(|c| format!("{} ({} pads)", c.label, c.max_pad()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    // Among columns that fit, the one the package name spells comes first
    // (SOT-23-5 picks the SOT column over an SC70 column with the same map).
    let spelled = |c: &Column| {
        let name: String = package
            .unwrap_or("")
            .to_ascii_uppercase()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect();
        package_of(&c.label).is_some_and(|(family, _)| name.contains(family))
    };
    let mut fits = fits;
    fits.sort_by_key(|c| !spelled(c));
    match fits.as_slice() {
        [] => Err(format!(
            "no pin-table column fits {}{}; columns found: {}",
            package.unwrap_or("any package"),
            pads.map_or(String::new(), |n| format!(" with {n} pads")),
            labels(&cols.iter().collect::<Vec<_>>())
        )),
        [one] => Ok(one),
        [first, rest @ ..] if rest.iter().all(|c| same_map(first, c)) => Ok(first),
        many => Err(format!(
            "several columns fit with different pin maps: {}; name one with --package",
            labels(many)
        )),
    }
}

/// The column a board's footprint and pads select, allowing the exposed pad.
///
/// Like [`choose`] with the board's distinct pad count. A table that leaves the
/// exposed pad out (it lists "Thermal Pad" with no number, or none at all)
/// gives a column one pad short of a board that has it; that fits when the board
/// has exactly one pad the table does not number (a pad above the table's
/// highest, or a non-numeric one) and the table names a thermal / exposed pad or
/// the footprint says `EP`. The chosen column is then a copy with the board's
/// extra pad mapped to the exposed pad (`EP/<the name the table gives it>`).
///
/// # Errors
/// As [`choose`], for the whole board.
pub fn choose_for_board<'a>(
    cols: &'a [Column],
    package: Option<&str>,
    board_pads: &[&str],
) -> Result<Cow<'a, Column>, String> {
    let distinct: BTreeSet<&str> = board_pads
        .iter()
        .copied()
        .filter(|p| !p.is_empty())
        .collect();
    let n = (!distinct.is_empty()).then_some(distinct.len());
    let first = match choose(cols, package, n) {
        Ok(c) => return Ok(Cow::Borrowed(c)),
        Err(e) => e,
    };
    let Some(short) = n.and_then(|n| n.checked_sub(1)).filter(|k| *k >= 3) else {
        return Err(first);
    };
    choose(cols, package, Some(short))
        .ok()
        .and_then(|c| with_exposed_pad(c, package, &distinct))
        .map(Cow::Owned)
        .ok_or(first)
}

/// Whether a footprint name says it has an exposed pad (`QFN-64-1EP_9x9mm_EP7.3x7.3mm`).
fn footprint_says_ep(package: &str) -> bool {
    package
        .rsplit(':')
        .next()
        .unwrap_or(package)
        .to_ascii_uppercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|t| {
            matches!(t, "EP" | "EPAD" | "THERMALPAD" | "EXPOSEDPAD")
                || t.strip_prefix("EP")
                    .is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()))
                || t.strip_suffix("EP")
                    .is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()))
        })
}

fn with_exposed_pad(col: &Column, package: Option<&str>, board: &BTreeSet<&str>) -> Option<Column> {
    let max = col.max_pad();
    let mut extra = board
        .iter()
        .filter(|p| p.parse::<usize>().map_or(true, |n| n > max));
    let pad = *extra.next()?;
    if extra.next().is_some() {
        return None;
    }
    let table_names_it = col.pads.contains_key(EXPOSED_PAD);
    if !(table_names_it || package.is_some_and(footprint_says_ep)) {
        return None;
    }
    let mut out = col.clone();
    let (name, page) = out.pads.remove(EXPOSED_PAD).unwrap_or_else(|| {
        (
            String::new(),
            out.pads.values().map(|v| v.1).min().unwrap_or(0),
        )
    });
    let letters: String = name
        .chars()
        .filter(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_uppercase();
    let printed = if name.is_empty() || spells_exposed_pad(&letters) {
        EXPOSED_PAD.to_string()
    } else {
        format!("{EXPOSED_PAD}/{name}")
    };
    out.pads.insert(pad.to_string(), (printed, page));
    Some(out)
}

/// Whether two columns give every pad the same name.
#[must_use]
pub fn same_map(a: &Column, b: &Column) -> bool {
    a.pads.len() == b.pads.len()
        && a.pads
            .iter()
            .all(|(p, (n, _))| b.pads.get(p).is_some_and(|(m, _)| norm(m) == norm(n)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(x0: f64, y0: f64, x1: f64, text: &str) -> Word {
        Word {
            x0,
            y0,
            x1,
            y1: y0 + 8.0,
            text: text.into(),
        }
    }

    /// A TI-style table: NAME then three package columns, one row with two pads.
    fn ti_table() -> Vec<Word> {
        let mut v = vec![
            w(62.0, 374.0, 86.0, "NAME"),
            w(103.0, 374.0, 127.0, "WSON"),
            w(146.0, 374.0, 166.0, "SC70"),
            w(188.0, 374.0, 205.0, "SOT"),
            w(129.0, 360.0, 142.0, "PIN"),
        ];
        let rows = [
            ("IN", "1", "1", "1"),
            ("GND", "2", "2", "2"),
            ("EN", "6", "3", "3"),
            ("NC", "4,", "4", "4"),
            ("OUT", "3", "5", "5"),
        ];
        for ((name, a, b, c), i) in rows.iter().zip(0u32..) {
            let y = 22.0f64.mul_add(f64::from(i), 396.0);
            v.push(w(66.0, y, 82.0, name));
            v.push(w(112.0, y, 117.0, a));
            v.push(w(154.0, y, 158.0, b));
            v.push(w(194.0, y, 199.0, c));
        }
        v.push(w(118.0, 22.0f64.mul_add(3.0, 396.0), 122.0, "5"));
        v
    }

    #[test]
    fn each_package_column_keeps_its_own_pads() {
        let cols = columns(&[vec![], ti_table()]);
        let get = |l: &str| cols.iter().find(|c| c.label == l).unwrap();
        let sot = get("SOT");
        assert_eq!(sot.pads["3"].0, "EN");
        assert_eq!(sot.pads["5"], ("OUT".to_string(), 2));
        assert!(sot.is_whole(), "{sot:?}");
        let wson = get("WSON");
        assert_eq!(wson.pads["6"].0, "EN");
        assert_eq!(wson.pads["3"].0, "OUT");
        assert_eq!(wson.pads["5"].0, "NC", "'4, 5' is two pads of one row");
        let chosen = choose(&cols, Some("Package_TO_SOT_SMD:SOT-23-5"), None).unwrap();
        assert_eq!(chosen.label, "SOT");
        // SOT and SC70 share a map, so a bare 5-pad request is not ambiguous.
        assert!(choose(&cols, None, Some(5)).is_ok());
        let tsv = Column::from_tsv("SOT", &sot.tsv());
        assert_eq!(tsv.pads, sot.pads);
    }

    /// NXP's layout (PCF8523 Table 5): Symbol, one Pin column per package,
    /// Type, then a wrapped Description whose first words fall in a column of
    /// their own. The names are the Symbol column's, not the Description's.
    fn nxp_table() -> Vec<Word> {
        let mut v = vec![
            w(52.0, 417.0, 84.6, "Symbol"),
            w(124.4, 417.0, 138.3, "Pin"),
            w(356.4, 417.0, 376.7, "Type"),
            w(409.7, 417.0, 459.1, "Description"),
            w(124.4, 432.0, 142.4, "SO8"),
            w(180.7, 432.0, 217.6, "HVSON8"),
            w(238.0, 432.0, 258.0, "TSSOP14"),
        ];
        let rows = [
            ("OSCI", "1", "1", "1", "input"),
            ("OSCO", "2", "2", "2", "output"),
            ("VBAT", "3", "3", "4", "supply"),
            ("VSS", "4", "4", "5", "supply"),
            ("SDA", "5", "5", "10", "input/output"),
            ("SCL", "6", "6", "11", "input"),
        ];
        for ((name, a, b, c, kind), i) in rows.iter().zip(0u32..) {
            let y = 26.0f64.mul_add(f64::from(i), 458.0);
            v.push(w(52.0, y, 76.0, name));
            v.push(w(124.3, y, 129.3, a));
            v.push(w(180.6, y, 185.6, b));
            v.push(w(242.8, y, 247.8, c));
            v.push(w(356.4, y, 380.0, kind));
            // The description's first words: more of them, in a chain, than names.
            for k in 0..5u32 {
                v.push(w(
                    f64::from(k).mul_add(9.0, 409.7),
                    y,
                    f64::from(k).mul_add(9.0, 417.7),
                    "word",
                ));
            }
        }
        v
    }

    #[test]
    fn a_symbol_header_names_the_name_column_not_the_description() {
        let cols = columns(&[nxp_table()]);
        let so8 = cols.iter().find(|c| c.label == "SO8").unwrap();
        let names: Vec<_> = so8.pads.values().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            ["OSCI", "OSCO", "VBAT", "VSS", "SDA", "SCL"],
            "{so8:?}"
        );
        let tssop = cols.iter().find(|c| c.label == "TSSOP14").unwrap();
        assert_eq!(tssop.pads["10"].0, "SDA");
    }

    #[test]
    fn a_wrapped_number_and_a_subscript_name_are_joined() {
        let mut v = vec![
            w(100.0, 130.0, 112.0, "Pin"),
            w(100.0, 150.0, 120.0, "LQFP100"),
            w(125.0, 150.0, 145.0, "LQFP144"),
        ];
        for ((n100, n144, name), i) in [("98", "141", "PE1"), ("99", "142", "VSS")]
            .iter()
            .zip(0u32..)
        {
            let y = 30.0f64.mul_add(f64::from(i), 200.0);
            v.push(w(107.0, y, 113.0, n100));
            v.push(w(131.0, y, 139.0, n144));
            v.push(w(220.0, y, 238.0, name));
        }
        // Pad 100: "10" over "0", and VDD printed as V + subscript DD.
        v.push(w(107.0, 260.0, 113.0, "10"));
        v.push(w(109.0, 270.0, 112.0, "0"));
        v.push(w(131.0, 260.0, 139.0, "143"));
        v.push(w(220.0, 260.0, 226.0, "V"));
        v.push(Word {
            x0: 226.0,
            y0: 264.0,
            x1: 236.0,
            y1: 271.0,
            text: "DD".into(),
        });
        let cols = columns(&[v]);
        let c100 = cols.iter().find(|c| c.label == "LQFP100").unwrap();
        assert_eq!(c100.pads["100"].0, "VDD", "{c100:?}");
        assert_eq!(c100.pads["98"].0, "PE1");
        let c144 = cols.iter().find(|c| c.label == "LQFP144").unwrap();
        assert_eq!(c144.pads["141"].0, "PE1");
    }

    #[test]
    fn a_drawing_caption_is_no_column_and_a_last_row_alone_still_counts() {
        // A pinout drawing above the table: "6-Pin WSON" under the section
        // title "Pin Configuration", its pad numbers in another order.
        let mut page = vec![
            w(70.0, 300.0, 90.0, "Pin"),
            w(150.0, 314.0, 170.0, "6-Pin"),
            w(172.0, 314.0, 197.0, "WSON"),
            w(150.0, 330.0, 160.0, "OUT"),
            w(182.0, 330.0, 186.0, "6"),
        ];
        page.extend(ti_table());
        let cols = columns(&[page]);
        let wson = cols.iter().find(|c| c.label == "WSON").unwrap();
        assert_eq!(wson.pads["6"].0, "EN", "{wson:?}");
        assert!(wson.conflicts.is_empty(), "{wson:?}");

        // The table's last row alone on a continuation page.
        let head = |y: f64| {
            vec![
                w(70.0, y, 90.0, "NAME"),
                w(103.0, y, 127.0, "QFN8"),
                w(146.0, y, 166.0, "SOIC8"),
            ]
        };
        let mut p1 = head(100.0);
        // Pin-like names: a lone "A" or "G" reads as a type cell, not a name.
        for (name, i) in ["VDD", "PA1", "PA2", "PA3", "PA4", "PA5", "PA6"]
            .iter()
            .zip(0u32..)
        {
            let y = 20.0f64.mul_add(f64::from(i), 130.0);
            let pad = (i + 1).to_string();
            p1.push(w(66.0, y, 82.0, name));
            p1.push(w(112.0, y, 117.0, &pad));
            p1.push(w(154.0, y, 158.0, &pad));
        }
        let mut p2 = head(100.0);
        p2.push(w(66.0, 130.0, 82.0, "GND"));
        p2.push(w(112.0, 130.0, 117.0, "8"));
        p2.push(w(154.0, 130.0, 158.0, "8"));
        let cols = columns(&[p1, p2]);
        assert!(cols.iter().all(Column::is_whole), "{cols:?}");
    }

    #[test]
    fn footprints_and_headers_name_a_package() {
        assert_eq!(
            package_of("SOT"),
            Some(("SOT", None)),
            "SOT, not SO and a T"
        );
        assert_eq!(package_of("to"), None, "the English word");
        assert_eq!(package_of("TO220"), Some(("TO", None)));
        assert_eq!(package_of("LQFP64"), Some(("LQFP", Some(64))));
        assert_eq!(package_of("SC70"), Some(("SC70", None)));
        assert_eq!(package_of("SOT23"), Some(("SOT", None)));
        assert_eq!(package_of("Pin"), None);
        assert_eq!(name_tokens("PB2/BOOT1"), ["pb2boot1", "pb2", "boot1"]);
        assert!(name_tokens("VREF+").contains(&norm("vref_plus")));
        assert!(name_tokens("PC14-OSC32_IN").contains(&"pc14".to_string()));
    }

    /// TPD12S521 (TI): `NAME | NO.` with no package word, pads listed several to
    /// a row and wrapped over two lines (word boxes from the real PDF, page 3).
    fn tpd12s521() -> Vec<Word> {
        let mut v = vec![
            w(127.1, 314.45, 140.42, "PIN"),
            w(57.0, 327.65, 80.544, "NAME"),
            w(171.6, 327.65, 185.8, "NO."),
            w(235.5, 321.05, 256.36, "TYPE"),
            w(57.0, 340.85, 102.76, "5V_SUPPLY"),
            w(176.5, 340.85, 180.94, "1"),
            w(236.7, 347.45, 255.34, "PWR"),
            w(57.0, 354.05, 102.76, "LV_SUPPLY"),
            w(176.5, 354.05, 180.94, "2"),
            // GND: a list wrapped over two lines, its name between them.
            w(147.2, 367.25, 153.86, "3,"),
            w(156.26, 367.25, 162.92, "5,"),
            w(165.32, 367.25, 171.98, "8,"),
            w(174.38, 367.25, 196.6, "11,14,"),
            w(199.0, 367.25, 210.11, "25,"),
            w(154.0, 376.25, 165.11, "28,"),
            w(167.51, 376.25, 178.62, "31,"),
            w(181.02, 376.25, 192.13, "34,"),
            w(194.53, 376.25, 203.42, "36"),
            w(57.0, 371.75, 76.984, "GND,"),
            w(79.38, 371.75, 124.24, "TMDS_GND"),
            w(237.1, 371.75, 254.86, "GND"),
        ];
        for (name, a, b, y) in [
            ("TMDS_D2+", "4,", "35", 389.45),
            ("TMDS_D2\u{2013}", "6,", "33", 402.65),
            ("TMDS_D1+", "7,", "32", 415.85),
        ] {
            v.push(w(57.0, y, 98.98, name));
            v.push(w(169.7, y, 176.36, a));
            v.push(w(178.76, y, 187.65, b));
        }
        v
    }

    #[test]
    fn a_table_with_no_package_header_is_the_boards_package() {
        let cols = columns(&[vec![], tpd12s521()]);
        let [t] = cols.as_slice() else {
            panic!("one single-package table: {cols:?}")
        };
        assert_eq!(t.label, SINGLE_TABLE);
        assert_eq!(t.pads["1"], ("5V_SUPPLY".to_string(), 2));
        // The wrapped list is one row: every pad of it carries the name.
        for pad in ["3", "5", "8", "11", "14", "25", "28", "31", "34", "36"] {
            assert_eq!(t.pads[pad].0, "GND, TMDS_GND", "pad {pad}: {t:?}");
        }
        assert_eq!(
            (t.pads["4"].0.as_str(), t.pads["35"].0.as_str()),
            ("TMDS_D2+", "TMDS_D2+")
        );
        assert_eq!(t.pads["33"].0, "TMDS_D2\u{2013}");
        assert!(t.conflicts.is_empty(), "{t:?}");
        // Its highest pad fits the board's package (and no package word is needed).
        assert_eq!(t.max_pad(), 36);
        assert!(choose(&cols, Some("Package_SO:HTSSOP-36"), Some(36)).is_ok());
        assert!(choose(&cols, None, Some(36)).is_ok());
        // ... and a package of another size is still refused.
        assert!(choose(&cols, None, Some(48)).is_err());
    }

    #[test]
    fn a_page_with_two_number_columns_is_not_guessed_at() {
        let mut page = tpd12s521();
        // A second NO. header with its own numbered rows, beside the first.
        page.push(w(371.6, 327.65, 385.8, "NO."));
        for (i, y) in [340.85, 354.05, 367.25].into_iter().enumerate() {
            page.push(w(376.5, y, 380.94, &(i + 1).to_string()));
        }
        assert!(columns(&[page]).is_empty());
    }

    #[test]
    fn tables_that_contradict_each_other_stay_separate() {
        let table = |names: [&str; 3]| {
            let mut v = vec![w(57.0, 100.0, 80.0, "NAME"), w(171.6, 100.0, 185.8, "NO.")];
            for (n, i) in names.iter().zip(0u32..) {
                let y = 20.0f64.mul_add(f64::from(i), 130.0);
                v.push(w(57.0, y, 90.0, n));
                v.push(w(176.5, y, 181.0, &(i + 1).to_string()));
            }
            v
        };
        // A device per page, each with its own pin 1: never one column of conflicts.
        let cols = columns(&[table(["VIN", "GND", "VOUT"]), table(["D1", "D2", "D3"])]);
        let labels: Vec<_> = cols.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["TABLE", "TABLE2"]);
        assert!(cols.iter().all(|c| c.conflicts.is_empty()));
        // The same table continued on the next page is one table.
        let cols = columns(&[table(["VIN", "GND", "VOUT"]), table(["VIN", "GND", "VOUT"])]);
        assert_eq!(cols.len(), 1);
    }

    /// A pad, its type cell, and its names (x0, x1, text).
    type SideBySide<'a> = (&'a str, &'a str, &'a [(f64, f64, &'a str)]);

    /// Cypress CY7C65215: the header is `Pin[3]`, and the name column holds two
    /// names side by side.
    #[test]
    fn a_pin_header_and_two_names_side_by_side() {
        let mut v = vec![
            w(49.38, 140.93, 65.26, "Pin"),
            w(65.28, 138.73, 74.95, "[3]"),
            w(95.4, 140.93, 118.9, "Type"),
            w(175.56, 140.93, 203.75, "Name"),
            w(254.46, 140.93, 289.98, "Default"),
        ];
        let rows: [SideBySide; 4] = [
            ("1", "Power", &[(176.7, 202.66, "VDDD")]),
            (
                "2",
                "SCB/GPIO",
                &[(144.66, 179.61, "SCB0_0"), (200.1, 234.25, "GPIO_8")],
            ),
            (
                "3",
                "SCB/GPIO",
                &[(144.66, 179.61, "SCB0_5"), (200.1, 234.25, "GPIO_9")],
            ),
            ("4", "Power", &[(176.7, 202.66, "VSSD")]),
        ];
        for ((pad, ty, names), i) in rows.iter().zip(0u32..) {
            let y = 16.5f64.mul_add(f64::from(i), 157.43);
            v.push(w(59.46, y, 64.9, pad));
            v.push(w(84.4, y, 129.9, ty));
            for (x0, x1, n) in *names {
                v.push(w(*x0, y, *x1, n));
            }
            v.push(w(269.6, y, 274.9, "\u{2013}"));
        }
        let cols = columns(&[v]);
        let t = cols
            .iter()
            .find(|c| c.label == SINGLE_TABLE)
            .expect("table");
        assert_eq!(t.pads["1"].0, "VDDD", "{t:?}");
        assert_eq!(t.pads["2"].0, "SCB0_0 GPIO_8", "{t:?}");
        assert_eq!(t.pads["4"].0, "VSSD", "{t:?}");
    }

    /// TUSB8041 (VQFN-64 with a thermal pad): the pad column of the VSS row says
    /// `THERM / AL PAD`, and the table has no row numbered 65.
    fn tusb8041() -> Vec<Word> {
        let mut v = vec![
            w(112.2, 92.86, 125.52, "PIN"),
            w(89.6, 106.05, 113.14, "NAME"),
            w(159.1, 106.05, 173.3, "NO."),
            w(195.8, 99.46, 206.44, "I/O"),
        ];
        // A section title above the first row: alone on its line, so no name.
        v.push(w(57.0, 121.0, 77.0, "Clock"));
        v.push(w(79.0, 121.0, 97.0, "Reset"));
        for (name, pad, y) in [
            ("GRSTz", "50", 130.0),
            ("XI", "62", 160.0),
            ("USB_R1", "64", 190.0),
        ] {
            v.push(w(57.0, y, 100.0, name));
            v.push(w(163.0, y, 172.0, pad));
            v.push(w(190.0, y, 195.0, "I"));
        }
        // VDD: four lines of pads centred under NO., the name between them.
        for (pad_line, y) in [
            ("5, 8,", 220.0),
            ("13, 21,", 229.0),
            ("28, 31,", 238.0),
            ("51, 57", 247.0),
        ] {
            let mut x = 150.0;
            for tok in pad_line.split(' ') {
                let len = f64::from(u32::try_from(tok.len()).unwrap_or(0));
                v.push(w(x, y, x + 3.0f64.mul_add(len, 1.0), tok));
                x += 3.0f64.mul_add(len, 4.0);
            }
        }
        v.push(w(57.0, 233.5, 75.0, "VDD"));
        v.push(w(190.0, 233.5, 214.0, "PWR"));
        // VSS: the thermal pad, in the pad column, over two lines.
        v.push(w(57.0, 270.0, 75.0, "VSS"));
        v.push(w(161.0, 263.5, 190.0, "THERM"));
        v.push(w(161.0, 276.5, 170.0, "AL"));
        v.push(w(172.0, 276.5, 187.0, "PAD"));
        v
    }

    #[test]
    fn an_unnumbered_thermal_pad_is_read_as_the_exposed_pad() {
        let cols = columns(&[tusb8041()]);
        let [t] = cols.as_slice() else {
            panic!("{cols:?}")
        };
        for pad in ["5", "8", "13", "21", "28", "31", "51", "57"] {
            assert_eq!(t.pads[pad].0, "VDD", "pad {pad}: {t:?}");
        }
        assert_eq!(t.pads["50"].0, "GRSTz", "no section title in a name: {t:?}");
        assert_eq!(t.pads[EXPOSED_PAD].0, "VSS", "{t:?}");
        assert_eq!(t.max_pad(), 64);
        // The tsv round trip keeps it.
        assert_eq!(Column::from_tsv("TABLE", &t.tsv()).pads, t.pads);
        // The EP pad is not a numbered pad: pads 1..=64 are what is whole.
        assert!(!t.missing().contains(&65));
    }

    #[test]
    fn a_board_with_one_more_pad_than_the_table_takes_the_exposed_pad() {
        let cols = columns(&[tusb8041()]);
        let fp = "Package_DFN_QFN:VQFN-64-1EP_9x9mm_P0.5mm_EP7.15x7.15mm";
        let numbers: Vec<String> = (1..=65).map(|n| n.to_string()).collect();
        let board: Vec<&str> = numbers.iter().map(String::as_str).collect();
        // Thermal-via copies of pad 65 are one pad.
        let mut copies = board.clone();
        copies.extend(["65"; 9]);
        for pads in [&board, &copies] {
            let got = choose_for_board(&cols, Some(fp), pads).expect("fits with the EP");
            assert!(matches!(got, Cow::Owned(_)), "a copy with the pad added");
            assert_eq!(got.pads["65"].0, "EP/VSS");
            assert!(!got.pads.contains_key(EXPOSED_PAD));
            assert_eq!(got.max_pad(), 65);
        }
        // The table's own column is untouched.
        assert!(cols[0].pads.contains_key(EXPOSED_PAD));
        // A board that matches the table as it is needs no allowance.
        let plain: Vec<&str> = board[..64].to_vec();
        assert!(matches!(
            choose_for_board(&cols, Some(fp), &plain),
            Ok(Cow::Borrowed(_))
        ));
        // Two extra pads, or an extra pad numbered inside the table, is another package.
        let mut two = board.clone();
        two.push("66");
        assert!(choose_for_board(&cols, Some(fp), &two).is_err());
        // The name alone (EP in the footprint) also licenses it, with no EP row read.
        let mut bare = cols[0].clone();
        bare.pads.remove(EXPOSED_PAD);
        let bare = [bare];
        let got = choose_for_board(&bare, Some(fp), &board).expect("footprint says EP");
        assert_eq!(got.pads["65"].0, "EP");
        // Neither the table nor the footprint says so: one pad too many is a mismatch.
        assert!(
            choose_for_board(&bare, Some("Package_DFN_QFN:VQFN-64_9x9mm_P0.5mm"), &board).is_err()
        );
    }

    #[test]
    fn footprint_names_that_say_ep() {
        for yes in [
            "Package_DFN_QFN:QFN-64-1EP_9x9mm_P0.5mm_EP7.15x7.15mm",
            "QFN-32-EP",
            "Package_SO:HTSSOP-16-1EP_4.4x5mm",
            "WQFN-20_EP_3x3mm",
        ] {
            assert!(footprint_says_ep(yes), "{yes}");
        }
        for no in [
            "Package_QFP:LQFP-100_14x14mm",
            "QFN-48_7x7mm",
            "SOT-23-5",
            "Package_SO:SOIC-8_3.9x4.9mm",
        ] {
            assert!(!footprint_says_ep(no), "{no}");
        }
    }

    #[test]
    fn bbox_xhtml_is_read_per_page() {
        let html = r#"<doc><page width="612" height="792">
<word xMin="1.5" yMin="2.0" xMax="3.0" yMax="4.0">A&amp;B</word>
</page><page width="612" height="792"><word xMin="5" yMin="6" xMax="7" yMax="8">C</word></page></doc>"#;
        let pages = parse_bbox(html);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0][0].text, "A&B");
        assert!((pages[1][0].x0 - 5.0).abs() < 1e-9);
    }
}
