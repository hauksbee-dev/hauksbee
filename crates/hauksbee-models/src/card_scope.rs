//! Rounded scope: a card describes the part, not this board's use of it.
//!
//! A drafter that is shown the board tends to model only the board's package
//! and the board's pins. The card then fails the next board that uses another
//! package of the same part, and the thermal rows it carries are easy to take
//! from the wrong package's column (a measured error: an LDO card for the SOT
//! package carried the neighbouring SC70 package's 307.6 °C/W instead of its
//! own 235.9). The checks here hold a card to the whole part:
//!
//! * [`thermal_rows`] reads every package's `θJA` from the thermal table, in
//!   either layout datasheets print it (one column per package, or one row per
//!   package); a card's `θJA` that is another package's is a finding, and a
//!   package with no `[[models.source.characteristic]]` row of its own is one
//!   too, since the binder selects the row whose package rates the footprint;
//! * [`package_coverage`] holds the reply to the pin table: every package the
//!   table lists needs a pin map (a `[[models.source.pinout]]` column, or a
//!   `[[models]]` card with its own `footprint_re`);
//! * a `footprint_re` anchored to one library's full footprint name binds
//!   nothing else, so it is flagged in favour of the package-family form;
//! * two `supply_outputs` rows for one role are an ambiguity, not coverage.

use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::Regex;

use crate::card_coverage::{Facts, Finding, Severity};
use crate::pin_table::{self, Column};
use crate::schema::ModelEntry;

/// One package's `θJA` as the datasheet prints it.
#[derive(Debug, Clone, PartialEq)]
pub struct ThermalRow {
    /// The package as the table names it (`DDC [SOT]`, `LQFP100`).
    pub label: String,
    /// The package family the label spells (`SOT`, `LQFP`), when it spells one.
    pub family: Option<&'static str>,
    /// The pad count the label or the table's pin line prints.
    pub pads: Option<usize>,
    pub theta: f64,
    /// PDF page (1-based).
    pub page: usize,
    /// Printed as one column per package (a header names each), so the
    /// attribution is unambiguous.
    pub columnar: bool,
}

static BRACKET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([A-Za-z][A-Za-z0-9]{1,5})\s*\[([^\]]+)\]").expect("bracket"));
static PINS_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(\d{1,3})\s*pins?\b").expect("pins"));
static THETA_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:R\s*|[θΘ]\s*)(?:[θΘ]\s*|th\s*)?_?\s*JA\b|junction[- ]to[- ]ambient|junction[- ]ambient")
        .expect("theta label")
});
static PACKAGE_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(LQFP|TQFP|UFBGA|TFBGA|LFBGA|VFBGA|WLCSP|BGA|UFQFPN|VQFN|WQFN|QFN|WSON|SON|DFN|HTSSOP|TSSOP|MSOP|SSOP|SOIC|SOT|TSOT|PDIP|DIP)\s?-?\s?(\d{1,3})\b",
    )
    .expect("package token")
});

fn cells(line: &str) -> Vec<&str> {
    line.split("  ")
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect()
}

fn number(cell: &str) -> Option<f64> {
    let t = cell.trim().trim_end_matches("°C/W").trim();
    t.chars()
        .all(|c| c.is_ascii_digit() || c == '.')
        .then(|| t.parse().ok())
        .flatten()
}

/// Every package's `θJA` the pages print.
#[must_use]
pub fn thermal_rows<S: AsRef<str>>(pages: &[S]) -> Vec<ThermalRow> {
    let mut out = Vec::new();
    for (pi, page) in pages.iter().enumerate() {
        let lines: Vec<&str> = page.as_ref().lines().collect();
        columnar(&lines, pi + 1, &mut out);
        per_row(&lines, pi + 1, &mut out);
    }
    let mut seen = Vec::new();
    out.retain(|r| {
        let key = (r.label.clone(), r.theta.to_bits());
        let fresh = !seen.contains(&key);
        seen.push(key);
        fresh
    });
    out
}

/// Layout one: a header names a package per column and the `θJA` row has a
/// number under each.
fn columnar(lines: &[&str], page: usize, out: &mut Vec<ThermalRow>) {
    for (i, line) in lines.iter().enumerate() {
        if !THETA_LABEL.is_match(line) {
            continue;
        }
        let nums: Vec<f64> = cells(line).into_iter().filter_map(number).collect();
        if nums.len() < 2 {
            continue;
        }
        let from = i.saturating_sub(6);
        let header: Vec<(String, Option<&'static str>)> = lines[from..i]
            .iter()
            .rev()
            .map(|l| {
                BRACKET
                    .captures_iter(l)
                    .map(|c| {
                        let name = c[2].trim();
                        (
                            format!("{} [{name}]", &c[1]),
                            pin_table::package_of(&squash(name)).map(|p| p.0),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .find(|h| h.len() == nums.len())
            .unwrap_or_default();
        if header.is_empty() {
            continue;
        }
        let counts: Vec<usize> = lines[from..i]
            .iter()
            .rev()
            .map(|l| {
                PINS_LINE
                    .captures_iter(l)
                    .filter_map(|c| c[1].parse().ok())
                    .collect::<Vec<_>>()
            })
            .find(|c| c.len() == nums.len())
            .unwrap_or_default();
        for (k, ((label, family), theta)) in header.into_iter().zip(nums).enumerate() {
            out.push(ThermalRow {
                label,
                family,
                pads: counts.get(k).copied(),
                theta,
                page,
                columnar: true,
            });
        }
    }
}

/// Layout two: one row per package, the package named beside its number.
fn per_row(lines: &[&str], page: usize, out: &mut Vec<ThermalRow>) {
    for (j, line) in lines.iter().enumerate() {
        let Some(c) = PACKAGE_TOKEN.captures(line) else {
            continue;
        };
        let near = lines[j.saturating_sub(4)..(j + 2).min(lines.len())]
            .iter()
            .any(|l| THETA_LABEL.is_match(l));
        if !near {
            continue;
        }
        let own = cells(line).into_iter().filter_map(number).next_back();
        let standalone = |k: usize| {
            lines
                .get(k)
                .and_then(|l| number(l.trim()).filter(|_| cells(l).len() == 1))
        };
        let theta = own
            .or_else(|| j.checked_sub(1).and_then(standalone))
            .or_else(|| standalone(j + 1));
        let Some(theta) = theta else { continue };
        let family = pin_table::package_of(&format!("{}{}", &c[1], &c[2])).map(|p| p.0);
        out.push(ThermalRow {
            label: format!("{}{}", &c[1], &c[2]),
            family,
            pads: c[2].parse().ok(),
            theta,
            page,
            columnar: false,
        });
    }
}

fn squash(s: &str) -> String {
    s.to_ascii_uppercase()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect()
}

/// The package a card is bound for: its own declaration, else the board's
/// footprint when the card binds it, else the thermal row its `footprint_re`
/// names (a card for another package of the part), else its first pin map.
fn bound_package(entry: &ModelEntry, facts: &Facts, rows: &[ThermalRow]) -> Option<String> {
    let board = facts
        .board
        .as_ref()
        .map(|b| b.footprint.clone())
        .filter(|f| !f.is_empty());
    let pattern = entry
        .r#match
        .footprint_re
        .as_deref()
        .and_then(|p| Regex::new(&format!("(?i){p}")).ok());
    entry
        .variant
        .as_ref()
        .and_then(|v| v.package.clone())
        .or_else(|| entry.pin_package.clone())
        .or_else(|| match (&pattern, &board) {
            (Some(re), Some(b)) if !re.is_match(b) => rows
                .iter()
                .find(|r| re.is_match(&r.label))
                .map(|r| r.label.clone()),
            (_, b) => b.clone(),
        })
        .or_else(|| {
            entry
                .source
                .as_ref()
                .and_then(|s| s.pinout.first().map(|p| p.package.clone()))
        })
}

fn row_fits(row: &ThermalRow, bound: &str, pads: Option<usize>) -> bool {
    let b = squash(bound);
    let spelled = row.family.map_or_else(
        || {
            let l = squash(&row.label);
            !l.is_empty() && b.contains(&l)
        },
        |f| b.contains(f),
    );
    let counted = match (row.pads, pads) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    };
    spelled && counted
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.051
}

/// Another package's `θJA` used for the bound package, and a package with no
/// row of its own.
fn theta_findings(entry: &ModelEntry, facts: &Facts, pages: &[String]) -> Vec<Finding> {
    let rows = thermal_rows(pages);
    let mut out = Vec::new();
    if rows.is_empty() {
        return out;
    }
    // A card with a θJA row per package carries the least favourable of them as
    // its scalar fallback (the harness's own rule), so it is each row that is
    // judged against the package it names, not the scalar against the bound one.
    let tagged: Vec<(&str, f64)> = entry
        .source
        .iter()
        .flat_map(|s| &s.characteristic)
        .filter(|c| c.rating.as_deref() == Some("theta_ja_c_per_w"))
        .filter_map(|c| {
            let v = crate::derating::rating_value("theta_ja_c_per_w", c)?;
            Some((c.package.as_deref()?, v))
        })
        .collect();
    let mut loose: Vec<f64> = entry
        .source
        .iter()
        .flat_map(|s| &s.characteristic)
        .filter(|c| c.rating.as_deref() == Some("theta_ja_c_per_w") && c.package.is_none())
        .filter_map(|c| crate::derating::rating_value("theta_ja_c_per_w", c))
        .collect();
    loose.sort_by(f64::total_cmp);
    loose.dedup_by(|a, b| close(*a, *b));
    if loose.len() >= 2 {
        let values: Vec<String> = loose.iter().map(ToString::to_string).collect();
        out.push(Finding {
            severity: Severity::Warn,
            check: "scope",
            message: format!(
                "{} θJA rows ({}) are tagged rating = \"theta_ja_c_per_w\" but name no package, \
                 so the harness reads the least favourable ({}) for every package: give each \
                 row package = ... (the package it is printed for)",
                loose.len(),
                values.join(", "),
                loose.last().copied().unwrap_or_default()
            ),
        });
    }
    if !tagged.is_empty() {
        for (package, v) in tagged {
            let (own, other): (Vec<&ThermalRow>, Vec<&ThermalRow>) =
                rows.iter().partition(|r| covers(package, r));
            if own.is_empty() || own.iter().any(|r| close(r.theta, v)) {
                continue;
            }
            if let Some(wrong) = other.iter().find(|r| close(r.theta, v)) {
                let right = own
                    .iter()
                    .map(|r| format!("{} {} (p.{})", r.label, r.theta, r.page))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push(Finding {
                    severity: if wrong.columnar && own.iter().all(|r| r.columnar) {
                        Severity::Error
                    } else {
                        Severity::Warn
                    },
                    check: "scope",
                    message: format!(
                        "the θJA row for package '{package}' is {v}, the {} package's value \
                         (p.{}); its own is {right}",
                        wrong.label, wrong.page
                    ),
                });
            }
        }
        return out;
    }
    if let (Some(bound), Some(v)) = (
        bound_package(entry, facts, &rows),
        entry.ratings.theta_ja_c_per_w,
    ) {
        let pads = facts
            .board
            .as_ref()
            .and_then(crate::card_coverage::Board::pad_count)
            .or_else(|| {
                entry
                    .pins
                    .keys()
                    .filter_map(|p| p.parse::<usize>().ok())
                    .max()
            });
        let (own, other): (Vec<&ThermalRow>, Vec<&ThermalRow>) =
            rows.iter().partition(|r| row_fits(r, &bound, pads));
        if !own.is_empty() && !own.iter().any(|r| close(r.theta, v)) {
            if let Some(wrong) = other.iter().find(|r| close(r.theta, v)) {
                let right = own
                    .iter()
                    .map(|r| format!("{} {} (p.{})", r.label, r.theta, r.page))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push(Finding {
                    severity: if wrong.columnar && own.iter().all(|r| r.columnar) {
                        Severity::Error
                    } else {
                        Severity::Warn
                    },
                    check: "scope",
                    message: format!(
                        "ratings.theta_ja_c_per_w = {v} is the {} package's θJA (p.{}), but this \
                         card is bound for {bound}: its own row is {right}. Use the bound \
                         package's value, and give every other package its own \
                         [[models.source.characteristic]] row (rating = \"theta_ja_c_per_w\", \
                         package = ...)",
                        wrong.label, wrong.page
                    ),
                });
            }
        }
    }
    out
}

/// Reply-level: every package the thermal table lists has a `θJA` in the reply.
///
/// A package is covered by a `[[models.source.characteristic]]` row naming it,
/// or by the `ratings.theta_ja_c_per_w` of the entry that is for that package.
#[must_use]
pub fn theta_coverage(entries: &[ModelEntry], facts: &Facts) -> Vec<Finding> {
    let rows = thermal_rows(&facts.pages);
    let mut labels: Vec<&ThermalRow> = Vec::new();
    for r in &rows {
        if !labels.iter().any(|l| l.label == r.label) {
            labels.push(r);
        }
    }
    if labels.len() < 2 {
        return Vec::new();
    }
    let missing: Vec<String> = labels
        .iter()
        .filter(|r| !entries.iter().any(|e| theta_covers(e, facts, &rows, r)))
        .map(|r| format!("{} {} °C/W", r.label, r.theta))
        .collect();
    if missing.is_empty() {
        return Vec::new();
    }
    vec![Finding {
        severity: Severity::Warn,
        check: "scope",
        message: format!(
            "the thermal table prints θJA for {} packages but the card has no \
             [[models.source.characteristic]] row (rating = \"theta_ja_c_per_w\", \
             package = ...) for: {}. Add one row per package so the board's package selects \
             its own",
            labels.len(),
            missing.join("; ")
        ),
    }]
}

/// Whether `entry` carries the `θJA` of the package `row` is printed for.
fn theta_covers(entry: &ModelEntry, facts: &Facts, rows: &[ThermalRow], row: &ThermalRow) -> bool {
    let by_row = entry.source.as_ref().is_some_and(|s| {
        s.characteristic
            .iter()
            .filter(|c| c.rating.as_deref() == Some("theta_ja_c_per_w"))
            .filter_map(|c| c.package.as_deref())
            .any(|p| covers(p, row))
    });
    by_row
        || entry.ratings.theta_ja_c_per_w.is_some_and(|v| {
            close(v, row.theta)
                && bound_package(entry, facts, rows).is_some_and(|b| row_fits(row, &b, None))
        })
}

/// Whether a row's `package` names the package a thermal row is printed for.
fn covers(package: &str, row: &ThermalRow) -> bool {
    let p = squash(package);
    let label = squash(&row.label);
    // `DDC [SOT]` is spelled by its code, its family or both.
    let code: String = row
        .label
        .split_whitespace()
        .next()
        .map(squash)
        .unwrap_or_default();
    if p.is_empty() {
        return false;
    }
    if !code.is_empty() && p.contains(&code) {
        return true;
    }
    if p.contains(&label) || label.contains(&p) {
        return true;
    }
    let (Some(f), _) = (row.family, 0) else {
        return false;
    };
    p.contains(f)
        && match (row.pads, crate::package::pin_count(package)) {
            (Some(a), Some(b)) => usize::try_from(b).is_ok_and(|b| a == b),
            _ => true,
        }
}

/// A `footprint_re` tied to one library's full footprint name.
fn anchor_findings(entry: &ModelEntry, facts: &Facts) -> Vec<Finding> {
    let Some(pattern) = entry.r#match.footprint_re.as_deref() else {
        return Vec::new();
    };
    let Ok(re) = Regex::new(&format!("(?i){pattern}")) else {
        return Vec::new();
    };
    let Some(label) = family_label(entry, facts) else {
        return Vec::new();
    };
    let (family, count) = label;
    let bare = [
        format!("{family}{count}"),
        format!("{family}-{count}"),
        format!("{family}_{count}"),
    ];
    let anchored = pattern.contains('^') || pattern.contains('$');
    if bare.iter().any(|b| re.is_match(b)) || !anchored {
        return Vec::new();
    }
    vec![Finding {
        severity: Severity::Warn,
        check: "scope",
        message: format!(
            "footprint_re '{pattern}' is anchored to one library's full footprint name and \
             matches neither '{family}-{count}' nor '{family}{count}', so any other library's \
             spelling of the same package would not bind: use the package-family form, e.g. \
             '(?i){family}[-_ ]?{count}(?:[-_ ]|$)'"
        ),
    }]
}

/// The package family and pad count the card is read for.
fn family_label(entry: &ModelEntry, facts: &Facts) -> Option<(&'static str, usize)> {
    let mut names: Vec<String> = Vec::new();
    names.extend(bound_package(entry, facts, &[]));
    names.extend(entry.pin_package.clone());
    names.iter().find_map(|n| {
        let spelled = n
            .rsplit(':')
            .next()
            .unwrap_or(n)
            .to_ascii_uppercase()
            .replace('_', " ");
        let hit = PACKAGE_TOKEN.captures(&spelled)?;
        let family = pin_table::package_of(&format!("{}{}", &hit[1], &hit[2]))?.0;
        Some((family, hit[2].parse().ok()?))
    })
}

/// Two rows for one supply role.
fn duplicate_outputs(entry: &ModelEntry) -> Vec<Finding> {
    let mut seen: Vec<String> = Vec::new();
    let mut dup: Vec<String> = Vec::new();
    for o in &entry.supply_outputs {
        let role = o.role.to_ascii_lowercase();
        if seen.contains(&role) {
            if !dup.contains(&role) {
                dup.push(role);
            }
        } else {
            seen.push(role);
        }
    }
    dup.into_iter()
        .map(|role| Finding {
            severity: Severity::Warn,
            check: "scope",
            message: format!(
                "[[models.supply_outputs]] has more than one row for role '{role}': a rail has \
                 one row (nominal, and the min/max across its source paths); merge them, or \
                 give each a role of its own pin"
            ),
        })
        .collect()
}

/// The scope findings for one entry.
#[must_use]
pub fn entry_findings(entry: &ModelEntry, facts: &Facts) -> Vec<Finding> {
    let mut out = duplicate_outputs(entry);
    out.extend(anchor_findings(entry, facts));
    out.extend(theta_findings(entry, facts, &facts.pages));
    out
}

/// The checklist lines for the whole part's packages: one pin map and one `θJA`
/// row per package the datasheet lists, however the board uses it.
#[must_use]
pub fn package_lines(facts: &Facts) -> String {
    let whole: Vec<&Column> = facts.pin_columns.iter().filter(|c| c.is_whole()).collect();
    let thermal = thermal_rows(&facts.pages);
    let mut out = String::new();
    if whole.len() > 1 {
        let labels: Vec<&str> = whole.iter().map(|c| c.label.as_str()).collect();
        let _ = writeln!(
            out,
            "- packages: the datasheet's pin table lists {} packages ({}). The card describes the \
             PART: give a [[models.source.pinout]] row (package, pins) for EACH, and keep the \
             board's package for [models.pins] and match; the board never limits scope",
            labels.len(),
            labels.join(", ")
        );
    }
    if !thermal.is_empty() {
        let rows: Vec<String> = thermal
            .iter()
            .map(|r| format!("{} {} C/W (p.{})", r.label, r.theta, r.page))
            .collect();
        let _ = writeln!(
            out,
            "- theta_ja per package: one [[models.source.characteristic]] rating = \
             \"theta_ja_c_per_w\" row for EACH package the thermal table lists, with its \
             `package` set, never another package's value: {}",
            rows.join("; ")
        );
    }
    if !out.is_empty() {
        out.push_str(
            "- footprint_re: the package-family form (`LQFP-?100`), not one library's full \
             footprint name\n",
        );
    }
    out
}

/// Whether some entry carries the package `column` as a pin map.
fn column_covered(entries: &[ModelEntry], column: &Column) -> bool {
    let want = squash(&column.label);
    entries.iter().any(|e| {
        let named = |p: &str| {
            let p = squash(p);
            !p.is_empty() && (p.contains(&want) || want.contains(&p))
        };
        let sourced = e
            .source
            .as_ref()
            .is_some_and(|s| s.pinout.iter().any(|p| named(&p.package)));
        let declared = e.variant.as_ref().and_then(|v| v.package.as_deref());
        let own = e.pin_package.as_deref();
        let by_name = declared.or(own).is_some_and(named);
        let by_pins = !e.pins.is_empty()
            && column.max_pad() >= 3
            && e.pins.len().abs_diff(column.pads.len()) <= 2
            && e.pins
                .iter()
                .filter(|(pad, role)| {
                    column.pads.get(*pad).is_some_and(|(printed, _)| {
                        crate::card_coverage::role_agrees(role, printed, e.kind)
                    })
                })
                .count()
                * 10
                >= column.pads.len() * 9;
        sourced || by_name || by_pins
    })
}

/// Reply-level: every package the pin table lists has a pin map somewhere in
/// the reply.
#[must_use]
pub fn package_coverage(entries: &[ModelEntry], facts: &Facts) -> Vec<Finding> {
    // A column that is not whole is a parse doubt, not a package to demand.
    let columns: Vec<&Column> = facts.pin_columns.iter().filter(|c| c.is_whole()).collect();
    if columns.len() < 2 {
        return Vec::new();
    }
    let mut uncovered: Vec<&Column> = Vec::new();
    for c in columns {
        if !column_covered(entries, c) && !uncovered.iter().any(|u| pin_table::same_map(u, c)) {
            uncovered.push(c);
        }
    }
    if uncovered.is_empty() {
        return Vec::new();
    }
    let names: Vec<String> = uncovered
        .iter()
        .map(|c| format!("{} ({} pads)", c.label, c.max_pad()))
        .collect();
    vec![Finding {
        severity: Severity::Warn,
        check: "scope",
        message: format!(
            "the datasheet's pin table lists packages this card does not cover: {}. A card \
             describes the part, not one board's package: add a [[models.source.pinout]] row \
             (package = ..., pins = the column `ds pins --package P` prints) for each, and for \
             a package whose pinout differs a [[models]] entry of its own with variant_of and a \
             footprint_re",
            names.join(", ")
        ),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card_coverage::Board;

    fn entry(toml_text: &str) -> ModelEntry {
        let db: crate::schema::DbFile = toml::from_str(toml_text).expect("toml");
        db.models.into_iter().next().expect("one")
    }

    const TLV_THERMAL: &str = "6.4 Thermal Information\n\
        \x20                                                                                              TLV700\n\
        \x20                        THERMAL METRIC (1)                                 DCK [SC70]        DDC [SOT]                 DSE [WSON]          UNIT\n\
        \x20                                                                             5 PINS             5 PINS                   6 PINS\n\
        RθJA           Junction-to-ambient thermal resistance                          307.6                235.9                    321.3\n\
        RθJC(top)      Junction-to-case (top) thermal resistance                        79.1                61.9                     207.9\n";

    const STM_THERMAL: &str = "Table 98. Package thermal characteristics\n\
        \x20            Symbol                    Parameter                            Value              Unit\n\
        \n\
        \x20                     Thermal resistance junction-ambient\n\
        \x20                                                                              46\n\
        \x20                     LQFP64 - 10 × 10 mm / 0.5 mm pitch\n\
        \x20                     Thermal resistance junction-ambient\n\
        \x20                                                                              43\n\
        \x20                     LQFP100 - 14 × 14 mm / 0.5 mm pitch\n";

    #[test]
    fn both_thermal_layouts_are_read() {
        let a = thermal_rows(&[TLV_THERMAL]);
        let col: Vec<(String, f64, Option<usize>)> = a
            .iter()
            .map(|r| (r.label.clone(), r.theta, r.pads))
            .collect();
        assert_eq!(
            col,
            [
                ("DCK [SC70]".to_string(), 307.6, Some(5)),
                ("DDC [SOT]".to_string(), 235.9, Some(5)),
                ("DSE [WSON]".to_string(), 321.3, Some(6)),
            ]
        );
        let b = thermal_rows(&[STM_THERMAL]);
        assert_eq!(
            b.iter()
                .map(|r| (r.label.as_str(), r.theta))
                .collect::<Vec<_>>(),
            [("LQFP64", 46.0), ("LQFP100", 43.0)]
        );
    }

    const LDO: &str = r#"
[[models]]
id = "ldo"
kind = "vreg"
[models.match]
value_re = "(?i)^TLV70033"
[models.pins]
1 = "in"
2 = "gnd"
3 = "en"
4 = "nc"
5 = "out"
[models.ratings]
theta_ja_c_per_w = 307.6
"#;

    fn facts() -> Facts {
        Facts {
            part: "TLV70033".into(),
            pages: vec![TLV_THERMAL.to_string()],
            board: Some(Board {
                value: "TLV70033DDCR".into(),
                footprint: "Package_TO_SOT_SMD:SOT-23-5".into(),
                ..Board::default()
            }),
            ..Facts::default()
        }
    }

    #[test]
    fn another_packages_theta_is_an_error_for_the_bound_package() {
        let found = entry_findings(&entry(LDO), &facts());
        let wrong = found
            .iter()
            .find(|f| f.message.contains("307.6"))
            .expect("wrong theta flagged");
        assert_eq!(wrong.severity, Severity::Error);
        assert!(wrong.message.contains("DCK [SC70]"), "{}", wrong.message);
        assert!(
            wrong.message.contains("DDC [SOT] 235.9"),
            "{}",
            wrong.message
        );
        // The right value is clean of that finding, but the other packages still want rows.
        let right = entry(&LDO.replace("307.6", "235.9"));
        let found = entry_findings(&right, &facts());
        assert!(found.iter().all(|f| f.severity != Severity::Error));
        let more = theta_coverage(&[right], &facts());
        assert!(
            more.iter().any(|f| f.message.contains("DSE [WSON] 321.3")),
            "{more:?}"
        );
        assert!(!more[0].message.contains("DDC [SOT]"), "{more:?}");
    }

    #[test]
    fn one_card_per_package_covers_the_thermal_table_by_each_cards_own_value() {
        let make = |id: &str, re: &str, theta: &str| {
            entry(&format!(
                "[[models]]\nid = \"{id}\"\nkind = \"vreg\"\n[models.match]\nvalue_re = \"X\"\n\
                 footprint_re = \"{re}\"\n[models.ratings]\ntheta_ja_c_per_w = {theta}\n"
            ))
        };
        let cards = [
            make("dck", "(?i)SC-?70|DCK", "307.6"),
            make("ddc", "(?i)SOT-?23|DDC", "235.9"),
            make("dse", "(?i)WSON|DSE", "321.3"),
        ];
        assert!(theta_coverage(&cards, &facts()).is_empty());
        for c in &cards {
            assert!(entry_findings(c, &facts()).is_empty(), "{}", c.id);
        }
        // The same set with the DCK card carrying the SOT value is wrong for DCK.
        let wrong = make("dck", "(?i)SC-?70|DCK", "235.9");
        let found = entry_findings(&wrong, &facts());
        assert!(
            found.iter().any(|f| f.message.contains("235.9")),
            "{found:?}"
        );
    }

    #[test]
    fn a_row_per_package_satisfies_the_thermal_scope() {
        let mut rows = String::new();
        for (p, v) in [
            ("DCK (SC70)", 307.6),
            ("DDC (SOT-23-5)", 235.9),
            ("DSE (WSON-6)", 321.3),
        ] {
            let _ = write!(
                rows,
                "[[models.source.characteristic]]\nparameter = \"RθJA\"\nrating = \"theta_ja_c_per_w\"\n\
                 package = \"{p}\"\nmax = {v}\nunit = \"°C/W\"\nconditions = \"c\"\nbasis = \"b\"\n"
            );
        }
        let card = entry(&format!(
            "{}\n[models.source]\ntier = \"datasheet-derived\"\nvalidation = \"physical-bounds-only\"\n{rows}",
            LDO.replace("307.6", "235.9")
        ));
        assert!(entry_findings(&card, &facts()).is_empty());
        assert!(theta_coverage(&[card], &facts()).is_empty());
    }

    #[test]
    fn the_scalar_of_a_card_with_a_row_per_package_is_its_least_favourable_row() {
        let rows = |dck: f64| {
            let mut rows = String::new();
            for (p, v) in [
                ("DCK (SC70)", dck),
                ("DDC (SOT-23-5)", 235.9),
                ("DSE (WSON-6)", 321.3),
            ] {
                let _ = write!(
                    rows,
                    "[[models.source.characteristic]]\nparameter = \"RθJA\"\nrating = \"theta_ja_c_per_w\"\n\
                     package = \"{p}\"\nmax = {v}\nunit = \"°C/W\"\nconditions = \"c\"\nbasis = \"b\"\n"
                );
            }
            rows
        };
        let card = |dck: f64| {
            entry(&format!(
                "{}\n[models.source]\ntier = \"datasheet-derived\"\nvalidation = \"physical-bounds-only\"\n{}",
                LDO.replace("307.6", "321.3"),
                rows(dck)
            ))
        };
        // The harness sets the scalar to the worst row (321.3, another package's),
        // which is not an error while every row is its own package's.
        assert!(entry_findings(&card(307.6), &facts()).is_empty());
        // A row carrying another package's value is.
        let found = entry_findings(&card(235.9), &facts());
        assert!(
            found
                .iter()
                .any(|f| f.severity == Severity::Error && f.message.contains("DCK (SC70)")),
            "{found:?}"
        );
    }

    #[test]
    fn theta_rows_that_name_no_package_are_flagged_when_they_differ() {
        let mut rows = String::new();
        for v in [307.6, 235.9, 321.3] {
            let _ = write!(
                rows,
                "[[models.source.characteristic]]\nparameter = \"RθJA\"\nrating = \"theta_ja_c_per_w\"\n\
                 max = {v}\nunit = \"°C/W\"\nconditions = \"c\"\nbasis = \"b\"\n"
            );
        }
        let card = entry(&format!(
            "{}\n[models.source]\ntier = \"datasheet-derived\"\nvalidation = \"physical-bounds-only\"\n{rows}",
            LDO.replace("307.6", "235.9")
        ));
        let found = entry_findings(&card, &facts());
        assert!(
            found
                .iter()
                .any(|f| f.message.contains("name no package") && f.message.contains("321.3")),
            "{found:?}"
        );
    }

    #[test]
    fn an_over_anchored_footprint_re_is_flagged() {
        let card = entry(
            r#"
[[models]]
id = "mcu"
kind = "mcu"
[models.match]
value_re = "X"
footprint_re = "^Package_QFP:LQFP-100_14x14mm_P0.5mm$"
[models.pins]
1 = "vdd"
"#,
        );
        let f = Facts {
            board: Some(Board {
                footprint: "Package_QFP:LQFP-100_14x14mm_P0.5mm".into(),
                ..Board::default()
            }),
            ..Facts::default()
        };
        let found = entry_findings(&card, &f);
        assert!(
            found.iter().any(|f| f.message.contains("anchored")),
            "{found:?}"
        );
        let loose = entry(
            "[[models]]\nid = \"m\"\nkind = \"mcu\"\n[models.match]\nvalue_re = \"X\"\n\
             footprint_re = \"(?i)LQFP-?100\"\n[models.pins]\n1 = \"vdd\"\n",
        );
        assert!(entry_findings(&loose, &f).is_empty());
    }

    #[test]
    fn duplicate_output_rows_and_missing_packages_are_flagged() {
        let card = entry(
            r#"
[[models]]
id = "ldo"
kind = "vreg"
[models.match]
value_re = "X"
[[models.supply_outputs]]
role = "out"
nominal = 3.3
min = 3.2
max = 3.4
basis = "b"
[[models.supply_outputs]]
role = "OUT"
nominal = 3.3
min = 3.2
max = 3.4
basis = "b"
"#,
        );
        assert!(entry_findings(&card, &Facts::default())
            .iter()
            .any(|f| f.message.contains("more than one row for role 'out'")));

        let mut a = Column {
            label: "LQFP64".into(),
            ..Column::default()
        };
        let mut b = Column {
            label: "LQFP100".into(),
            ..Column::default()
        };
        for i in 1..=64 {
            a.pads.insert(i.to_string(), (format!("PA{i}"), 1));
        }
        for i in 1..=100 {
            b.pads.insert(i.to_string(), (format!("PB{i}"), 1));
        }
        let facts = Facts {
            pin_columns: vec![a, b],
            ..Facts::default()
        };
        let covering = entry(
            r#"
[[models]]
id = "mcu"
kind = "mcu"
[models.match]
value_re = "X"
[models.source]
tier = "datasheet-derived"
validation = "physical-bounds-only"
[[models.source.pinout]]
package = "LQFP64"
pins = { "1" = "PA1" }
"#,
        );
        let found = package_coverage(&[covering], &facts);
        assert_eq!(found.len(), 1);
        assert!(found[0].message.contains("LQFP100 (100 pads)"));
        assert!(!found[0].message.contains("LQFP64"));
    }
}
