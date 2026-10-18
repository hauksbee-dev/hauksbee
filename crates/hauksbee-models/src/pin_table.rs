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
//! `docs/how-and-why/hauksbee-models/datasheet-toolkit.md`.

use std::collections::BTreeMap;
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
        "name" | "pin" | "number" | "names" | "no." | "terminal" | "signal"
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
    y1: f64,
    tol: f64,
}

/// Every package column of every pin table in `pages` (index 0 is PDF page 1).
#[must_use]
pub fn columns(pages: &[Vec<Word>]) -> Vec<Column> {
    let mut by_label: BTreeMap<String, Column> = BTreeMap::new();
    let mut last_table: Option<usize> = None;
    for (index, words) in pages.iter().enumerate() {
        let found = page_columns(words);
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
            let col = by_label.entry(label.clone()).or_insert_with(|| Column {
                label,
                ..Column::default()
            });
            for (pad, name) in rows {
                match col.pads.get(&pad) {
                    Some((had, _)) if norm(had) != norm(&name) => {
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

/// A page's package columns: (label, [(pad, printed name)]).
fn page_columns(words: &[Word]) -> Vec<(String, Vec<(String, String)>)> {
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
            y1: w.y1,
            tol: 0.0,
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
    anchors.sort_by(f64::total_cmp);
    anchors.dedup_by(|a, b| (*a - *b).abs() < 3.0);
    let band = name_band(words, &heads, &names);
    let out: Vec<(String, Vec<(String, String)>)> = heads
        .iter()
        .map(|h| (h.label.clone(), column_rows(words, h, &anchors, band)))
        .filter(|(_, rows)| !rows.is_empty())
        .collect();
    out
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

/// One header's rows on a page: (pad, printed name) for every pad cell in its band.
fn column_rows(
    words: &[Word],
    h: &Header,
    anchors: &[f64],
    band: Option<(f64, f64)>,
) -> Vec<(String, String)> {
    let mut cells: Vec<&Word> = words
        .iter()
        .filter(|w| (w.xc() - h.x).abs() <= h.tol && w.y0 > h.y1 - 1.0)
        // "0" only ever continues a wrapped number ("10" over "0").
        .filter(|w| is_pad_number(&w.text.replace(',', "")) || w.text == "0")
        .collect();
    cells.sort_by(|a, b| a.y0.total_cmp(&b.y0));
    let mut rows = Vec::new();
    let mut used = std::collections::HashSet::new();
    for (i, w) in cells.iter().enumerate() {
        if used.contains(&i) || w.text == "0" {
            continue;
        }
        let mut num = w.text.trim_end_matches(',').to_string();
        let mut y = w.yc();
        // A number wrapped in a narrow column ("10" over "0" is pad 100).
        if let Some((j, below)) = cells.iter().enumerate().find(|(j, v)| {
            *j > i && v.text.len() == 1 && (6.0..14.0).contains(&(v.y0 - w.y0)) && num.len() == 2
        }) {
            let below_is_row = anchors
                .iter()
                .any(|a| (a - below.yc()).abs() < 1.0 && (a - w.yc()).abs() > 3.0);
            if !below_is_row || below.text == "0" {
                num.push_str(&below.text);
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
        let more = words
            .iter()
            .filter(|v| {
                (v.yc() - w.yc()).abs() < 1.5
                    && v.x0 > w.x1
                    && v.x0 - w.x1 < 8.0
                    && w.text.ends_with(',')
                    && is_pad_number(v.text.trim_end_matches(','))
            })
            .map(|v| v.text.trim_end_matches(',').to_string());
        let name = band.map_or_else(String::new, |b| name_at(words, b, row_extent(anchors, y)));
        if name.is_empty() {
            continue;
        }
        for pad in std::iter::once(num).chain(more) {
            rows.push((pad, name.clone()));
        }
    }
    rows
}

/// The x band of the pin-name column: a NAME header's, else the column of
/// name-like words that lines up with the most pad rows.
fn name_band(words: &[Word], heads: &[Header], names: &[&Word]) -> Option<(f64, f64)> {
    let top = heads.iter().map(|h| h.y1).fold(f64::INFINITY, f64::min);
    let in_head = |w: &Word| heads.iter().any(|h| (w.xc() - h.x).abs() <= h.tol);
    let rows: Vec<&Word> = words
        .iter()
        .filter(|w| w.y0 > top && is_pad_number(&w.text) && in_head(w))
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
    let header = names
        .iter()
        .find(|n| n.text.eq_ignore_ascii_case("name") || n.text.eq_ignore_ascii_case("names"))
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
    let lo = best.iter().map(|x| x.1).fold(f64::INFINITY, f64::min);
    let hi = best.iter().map(|x| x.2).fold(f64::NEG_INFINITY, f64::max);
    Some((lo - 4.0, hi + 4.0))
}

/// The name cell text in `band` between `lo` and `hi`: subscripts glued on,
/// a wrapped name joined, footnote marks dropped.
fn name_at(words: &[Word], band: (f64, f64), (lo, hi): (f64, f64)) -> String {
    let mut cell: Vec<&Word> = words
        .iter()
        .filter(|w| w.xc() >= band.0 && w.xc() <= band.1 && w.yc() > lo && w.yc() < hi)
        .filter(|w| !not_a_name(&w.text))
        .collect();
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
