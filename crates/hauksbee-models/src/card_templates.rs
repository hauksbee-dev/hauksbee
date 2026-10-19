//! Finished cards of the same kind, shown to the drafter as worked examples.
//!
//! A person writing a card for a new part opens the library card of its
//! nearest sibling and copies its shape: which groups it fills, how a
//! multi-package pin table is carried, what it declares as not modelled. The
//! extraction prompt used to show fixed examples of other kinds, which teach
//! the file format but not how a finished card of THIS kind reads, and a
//! cheaper drafter then wrote a narrow card (one package, the fields this
//! board needed) where the library's own cards are rounded out.
//!
//! The selection rule, fixed so a run's templates are explainable:
//!
//! 1. Candidates are library cards of the part's kind (builtin plus the
//!    user's `~/.hauksbee/models` and `~/.config/hauksbee/models`), never one
//!    whose match rules already accept the part, and never a second member of
//!    a variant family already chosen.
//! 2. A candidate sharing a family prefix with the part (the longest common
//!    prefix of the part number and the card's id or value pattern, at least
//!    [`MIN_PREFIX`] characters) ranks first, the longer prefix first.
//! 3. Within a rank, the richest card first: the most checklist groups
//!    filled ([`crate::card_coverage::needs`]), then rows, packages and
//!    sources carried.
//!
//! The text shown is trimmed to a budget ([`MAX_CARD_CHARS`]) by keeping the
//! structure and a few representative rows: the pin map keeps its first
//! pads, repeated characteristic rows keep their first few and every
//! per-package thermal row, and comment-only lines go first when the card is
//! still long.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::card_coverage;
use crate::schema::{ComponentKind, DbFile, ModelEntry};

/// Templates one prompt carries.
pub const LIMIT: usize = 2;
/// The shortest shared prefix that counts as one family.
pub const MIN_PREFIX: usize = 4;
/// A template longer than this is trimmed to fit.
pub const MAX_CARD_CHARS: usize = 5_500;
/// Pins kept in a trimmed template's pin map.
const KEEP_PINS: usize = 12;
/// Characteristic rows kept in a trimmed template besides thermal ones.
const KEEP_ROWS: usize = 4;
/// The directory of the sandbox the templates are written to.
pub const DIR: &str = "templates";

/// One chosen template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    pub id: String,
    /// Where the card came from: `builtin/<file>` or the user file's path.
    pub origin: String,
    pub kind: String,
    /// Why it was chosen: its family prefix and richness.
    pub reason: String,
    /// The card as shown, trimmed to the budget.
    pub text: String,
}

/// A library card as read, before ranking.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub origin: String,
    /// The `[[models]]` block as written.
    pub text: String,
    pub entry: ModelEntry,
}

/// Split a TOML file into its `[[models]]` blocks as written, less the
/// comment header of the card that follows each.
fn blocks_of(text: &str) -> Vec<String> {
    let starts: Vec<usize> = text
        .match_indices("[[models]]\n")
        .map(|(i, _)| i)
        .filter(|&i| i == 0 || text.as_bytes()[i - 1] == b'\n')
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(n, &start)| {
            let end = starts.get(n + 1).copied().unwrap_or(text.len());
            let mut block = &text[start..end];
            while let Some(cut) = block.trim_end().rfind('\n') {
                let last = block[cut + 1..].trim();
                if last.is_empty() || last.starts_with('#') {
                    block = &block[..cut];
                } else {
                    break;
                }
            }
            block.to_string()
        })
        .collect()
}

fn parse(origin: &str, block: String) -> Option<Candidate> {
    let db: DbFile = toml::from_str(&block).ok()?;
    let entry = db.models.into_iter().next()?;
    Some(Candidate {
        origin: origin.to_string(),
        text: block,
        entry,
    })
}

/// Every builtin card.
#[must_use]
pub fn builtin_candidates() -> Vec<Candidate> {
    let mut out = Vec::new();
    for (name, text) in crate::BUILTIN_TOML_FILES {
        for block in blocks_of(text) {
            out.extend(parse(&format!("builtin/{name}"), block));
        }
    }
    out
}

/// The user's model directories, lowest priority first.
fn user_dirs() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    vec![
        home.join(".hauksbee").join("models"),
        home.join(".config").join("hauksbee").join("models"),
    ]
}

/// Cards in the user's model directories (best-effort: unreadable or invalid
/// files are skipped, sensor specs are not cards).
#[must_use]
pub fn user_candidates(dirs: &[PathBuf]) -> Vec<Candidate> {
    let mut out = Vec::new();
    for dir in dirs {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().is_some_and(|e| e == "toml")
                    && !p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.ends_with(".sensor.toml"))
            })
            .collect();
        files.sort();
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for block in blocks_of(&text) {
                out.extend(parse(&path.display().to_string(), block));
            }
        }
    }
    out
}

/// The whole library the templates are drawn from.
#[must_use]
pub fn library_candidates() -> Vec<Candidate> {
    let mut all = builtin_candidates();
    all.extend(user_candidates(&user_dirs()));
    all
}

fn squash(s: &str) -> String {
    s.to_ascii_uppercase()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect()
}

fn common_prefix(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

/// The shared family prefix between the part and a card, in characters.
fn family_prefix(part: &str, entry: &ModelEntry) -> usize {
    let part = squash(part);
    let value: String = entry
        .r#match
        .value_re
        .as_deref()
        .or(entry.r#match.mpn_re.as_deref())
        .map(|re| squash(re.trim_start_matches("(?i)")))
        .unwrap_or_default();
    let id = squash(&entry.id);
    common_prefix(&part, &id).max(common_prefix(&part, &value))
}

fn accepts(entry: &ModelEntry, part: &str) -> bool {
    let rules = &entry.r#match;
    [rules.value_re.as_deref(), rules.mpn_re.as_deref()]
        .into_iter()
        .flatten()
        .any(|p| regex::Regex::new(&format!("(?i){p}")).is_ok_and(|re| re.is_match(part)))
}

/// How rounded a card is: checklist groups filled first, then what it carries.
fn richness(c: &Candidate) -> usize {
    let e = &c.entry;
    let needs = card_coverage::needs(e, &c.text, &[]);
    let filled = needs
        .iter()
        .filter(|n| card_coverage::satisfied(e, n))
        .count();
    let src = e.source.as_ref();
    let rows = src.map_or(0, |s| s.characteristic.len());
    let thermal = src.map_or(0, |s| {
        s.characteristic
            .iter()
            .filter(|r| r.rating.as_deref() == Some("theta_ja_c_per_w") && r.package.is_some())
            .count()
    });
    let packages = src.map_or(0, |s| s.pinout.len());
    let curated = src.is_some_and(|s| format!("{:?}", s.tier).contains("Curated"));
    filled * 10
        + rows.min(12)
        + packages.min(4) * 2
        + thermal.min(3) * 2
        + usize::from(!e.straps.is_empty()) * 2
        + e.envelope.len().min(6)
        + e.supply_outputs.len().min(3) * 2
        + usize::from(curated) * 3
}

/// The templates for `part` of `kind` from `cands`, best first, at most `limit`.
#[must_use]
pub fn select_from(
    cands: &[Candidate],
    part: &str,
    kind: ComponentKind,
    limit: usize,
) -> Vec<Template> {
    let mut ranked: Vec<(usize, usize, &Candidate)> = cands
        .iter()
        .filter(|c| c.entry.kind == kind && !accepts(&c.entry, part))
        .map(|c| {
            let prefix = family_prefix(part, &c.entry);
            (
                if prefix >= MIN_PREFIX { prefix } else { 0 },
                richness(c),
                c,
            )
        })
        .collect();
    // Longest family prefix first, then richest, then id for a stable order.
    ranked.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.cmp(&a.1))
            .then(a.2.entry.id.cmp(&b.2.entry.id))
    });
    let mut chosen: Vec<(usize, usize, &Candidate)> = Vec::new();
    for r in ranked {
        let same_family = chosen.iter().any(|(_, _, c)| {
            let (a, b) = (&c.entry, &r.2.entry);
            (a.variant_of.is_some() && a.variant_of == b.variant_of)
                || b.id.starts_with(&a.id)
                || a.id.starts_with(&b.id)
        });
        if !same_family {
            chosen.push(r);
        }
        if chosen.len() == limit {
            break;
        }
    }
    chosen
        .into_iter()
        .map(|(prefix, rich, c)| Template {
            id: c.entry.id.clone(),
            origin: c.origin.clone(),
            kind: kind_name(kind),
            reason: if prefix > 0 {
                format!("same family ({prefix} shared leading characters), richness {rich}")
            } else {
                format!("richest card of this kind, richness {rich}")
            },
            text: trim(&c.text),
        })
        .collect()
}

fn kind_name(kind: ComponentKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The templates for `part` of `kind` from the whole library.
#[must_use]
pub fn select(part: &str, kind: &str, limit: usize) -> Vec<Template> {
    let base = crate::datasheet::base_kind_name(kind).unwrap_or(kind);
    let Ok(kind) =
        serde_json::from_value::<ComponentKind>(serde_json::Value::String(base.trim().to_string()))
    else {
        return Vec::new();
    };
    select_from(&library_candidates(), part, kind, limit)
}

/// Trim a card block to [`MAX_CARD_CHARS`], keeping its structure and a few
/// representative rows.
#[must_use]
pub fn trim(block: &str) -> String {
    // Sections: a header line (`[...]`) and the lines up to the next one.
    let mut sections: Vec<(String, Vec<&str>)> = Vec::new();
    for line in block.lines() {
        if line.starts_with('[') {
            sections.push((line.trim().to_string(), vec![line]));
        } else if let Some(last) = sections.last_mut() {
            last.1.push(line);
        } else {
            sections.push((String::new(), vec![line]));
        }
    }
    let mut rows_kept = 0;
    let mut out: Vec<String> = Vec::new();
    for (header, lines) in &sections {
        match header.as_str() {
            "[models.pins]" => {
                let (head, body): (Vec<&str>, Vec<&str>) =
                    lines.iter().partition(|l| l.starts_with('['));
                out.extend(head.iter().map(ToString::to_string));
                let pins: Vec<&&str> = body.iter().filter(|l| !l.trim().is_empty()).collect();
                out.extend(pins.iter().take(KEEP_PINS).map(|l| (**l).to_string()));
                if pins.len() > KEEP_PINS {
                    out.push(format!(
                        "# ... {} more pads, all of them mapped in the real card",
                        pins.len() - KEEP_PINS
                    ));
                }
            }
            "[[models.source.characteristic]]" => {
                let thermal = lines.iter().any(|l| l.contains("theta_ja_c_per_w"));
                if thermal || rows_kept < KEEP_ROWS {
                    if !thermal {
                        rows_kept += 1;
                    }
                    out.extend(lines.iter().map(ToString::to_string));
                }
            }
            "[[models.source.pinout]]" => {
                let kept: Vec<&&str> = lines.iter().take(8).collect();
                out.extend(kept.iter().map(|l| (**l).to_string()));
                if lines.len() > 8 {
                    out.push("# ... rest of this package's pin map".to_string());
                }
            }
            _ => out.extend(lines.iter().map(ToString::to_string)),
        }
    }
    let mut text = out.join("\n");
    if text.len() > MAX_CARD_CHARS {
        // Comment-only lines carry the least structure.
        text = text
            .lines()
            .filter(|l| !l.trim_start().starts_with('#') || l.contains("more pads"))
            .collect::<Vec<_>>()
            .join("\n");
    }
    if text.len() > MAX_CARD_CHARS {
        let cut = (0..=MAX_CARD_CHARS)
            .rev()
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(0);
        let cut = text[..cut].rfind('\n').unwrap_or(cut);
        text.truncate(cut);
        text.push_str("\n# ... trimmed to fit");
    }
    text.trim_end().to_string() + "\n"
}

/// The prompt section showing the templates, or nothing when there are none.
#[must_use]
pub fn prompt_section(templates: &[Template]) -> String {
    if templates.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "WORKED EXAMPLES -- FINISHED CARDS OF THIS KIND FROM THE LIBRARY. These are other \
         parts. Copy their SHAPE: the groups they fill, how they carry a part sold in several \
         packages (a pin map and a footprint_re for each, one thermal row per package), what \
         they declare as not modelled. A card describes THE PART, every package and every \
         function the datasheet prints, not the one board that asked for it. Their numbers, \
         pin numbers, match patterns and references belong to OTHER parts: every value in your \
         answer comes from this datasheet, and copying one of theirs is a WRONG ANSWER. \
         More of them: `ds similar`.\n\n",
    );
    for (i, t) in templates.iter().enumerate() {
        let _ = write!(
            out,
            "EXAMPLE {} -- {} ({}; {}):\n{}\n",
            i + 1,
            t.id,
            t.kind,
            t.reason,
            t.text
        );
    }
    out
}

/// Write the templates into a sandbox's `templates/` directory, with an
/// index saying why each was chosen.
pub fn write_to(dir: &Path, templates: &[Template]) {
    if templates.is_empty() {
        return;
    }
    let root = dir.join(DIR);
    if std::fs::create_dir_all(&root).is_err() {
        return;
    }
    let mut index = String::from(
        "# Worked examples: finished library cards of this kind\n\n\
         Other parts, trimmed. Copy their shape, never their numbers.\n\n\
         | file | card | origin | why |\n|---|---|---|---|\n",
    );
    for (i, t) in templates.iter().enumerate() {
        let name = format!(
            "{:02}-{}.toml",
            i + 1,
            crate::datasheet::sanitise_filename(&t.id)
        );
        let _ = std::fs::write(root.join(&name), &t.text);
        let _ = writeln!(index, "| {name} | {} | {} | {} |", t.id, t.origin, t.reason);
    }
    let _ = std::fs::write(root.join("index.md"), index);
}

/// The one-line provenance of the templates a card was drafted with.
#[must_use]
pub fn provenance_line(templates: &[Template]) -> Option<String> {
    (!templates.is_empty()).then(|| {
        format!(
            "worked examples: {}",
            templates
                .iter()
                .map(|t| format!("{} ({})", t.id, t.origin))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// The worked examples a sandbox was given, as one provenance sentence, read
/// back from its `templates/index.md`.
#[must_use]
pub fn index_note(dir: &Path) -> Option<String> {
    let index = std::fs::read_to_string(dir.join(DIR).join("index.md")).ok()?;
    let cards: Vec<String> = index
        .lines()
        .filter(|l| l.starts_with("| ") && l.contains(".toml"))
        .filter_map(|l| {
            let cells: Vec<&str> = l.trim_matches('|').split('|').map(str::trim).collect();
            Some(format!("{} ({})", cells.get(1)?, cells.get(2)?))
        })
        .collect();
    (!cards.is_empty()).then(|| format!("worked examples: {}", cards.join(", ")))
}

/// `ds similar [--kind K]`: the run's own templates, or the nearest cards of
/// another kind computed from the library.
///
/// # Errors
/// A `--kind` that is not a card kind.
pub fn similar(
    dir: &Path,
    part: &str,
    run_kind: &str,
    kind: Option<&str>,
) -> anyhow::Result<String> {
    let wanted = kind.unwrap_or(run_kind);
    let precomputed = dir.join(DIR);
    if kind.is_none_or(|k| k == run_kind) && precomputed.join("index.md").is_file() {
        let mut out = std::fs::read_to_string(precomputed.join("index.md")).unwrap_or_default();
        let mut files: Vec<PathBuf> = std::fs::read_dir(&precomputed)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "toml"))
            .collect();
        files.sort();
        for f in files {
            let _ = write!(
                out,
                "\n===== {} =====\n{}",
                f.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                std::fs::read_to_string(&f).unwrap_or_default()
            );
        }
        return Ok(out);
    }
    let found = select(part, wanted, LIMIT);
    if found.is_empty() {
        anyhow::bail!(
            "no finished library card of kind '{wanted}' to show for {part} (kinds: {})",
            crate::validation::KIND_NAMES.join(", ")
        );
    }
    let mut out = format!("# Nearest finished cards of kind {wanted}\n");
    for t in &found {
        let _ = write!(
            out,
            "\n===== {} ({}; {}) =====\n{}",
            t.id, t.origin, t.reason, t.text
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(toml_text: &str) -> Candidate {
        parse("test", toml_text.to_string()).expect("a card")
    }

    fn card(id: &str, value_re: &str, extra: &str) -> Candidate {
        cand(&format!(
            "[[models]]\nid = \"{id}\"\nkind = \"vreg\"\n[models.match]\nvalue_re = \"{value_re}\"\n\
             [models.pins]\n1 = \"in\"\n2 = \"gnd\"\n3 = \"out\"\n{extra}"
        ))
    }

    #[test]
    fn the_family_prefix_ranks_before_richness() {
        let poor_family = card("tlv70018", "(?i)^TLV70018", "");
        let rich_other = card(
            "lm1117",
            "(?i)^LM1117",
            "[models.ratings]\ntheta_ja_c_per_w = 50.0\nmax_voltage_v = 15.0\n",
        );
        let own = card("tlv70033", "(?i)^TLV70033", "");
        let found = select_from(
            &[rich_other, own, poor_family],
            "TLV70033",
            ComponentKind::Vreg,
            2,
        );
        let ids: Vec<&str> = found.iter().map(|t| t.id.as_str()).collect();
        // The card that already accepts the part is never its own template.
        assert_eq!(ids, ["tlv70018", "lm1117"]);
        assert!(
            found[0].reason.contains("same family"),
            "{}",
            found[0].reason
        );
        assert!(found[1].reason.contains("richest"), "{}", found[1].reason);
    }

    #[test]
    fn variants_of_one_family_are_not_both_shown() {
        let a = card("tlv75533p", "(?i)^TLV75533P$", "");
        let b = card("tlv75533p_drv", "(?i)^TLV75533PDRV", "");
        let c = card("lm317", "(?i)^LM317", "");
        let found = select_from(&[a, b, c], "TLV70033", ComponentKind::Vreg, 2);
        let ids: Vec<&str> = found.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["tlv75533p", "lm317"]);
    }

    #[test]
    fn trimming_keeps_structure_and_a_few_rows() {
        let mut pins = String::new();
        for i in 1..=80 {
            let _ = writeln!(pins, "\"{i}\" = \"pa{i}\"");
        }
        let mut rows = String::new();
        for i in 1..=10 {
            let _ = write!(
                rows,
                "[[models.source.characteristic]]\nparameter = \"p{i}\"\nmax = {i}.5\nunit = \"V\"\n\
                 conditions = \"c\"\nbasis = \"b\"\n"
            );
        }
        let theta = "[[models.source.characteristic]]\nparameter = \"t\"\nrating = \"theta_ja_c_per_w\"\n\
                     package = \"X\"\nmax = 40.0\nunit = \"C/W\"\nconditions = \"c\"\nbasis = \"b\"\n";
        let block = format!(
            "[[models]]\nid = \"m\"\nkind = \"mcu\"\n[models.pins]\n{pins}[models.source]\ntier = \"curated-library\"\n\
             validation = \"physical-bounds-only\"\n{rows}{theta}"
        );
        let t = trim(&block);
        assert!(t.contains("\"12\" = \"pa12\""));
        assert!(!t.contains("\"13\" = \"pa13\""));
        assert!(t.contains("68 more pads"), "{t}");
        assert!(t.contains("parameter = \"p4\"") && !t.contains("parameter = \"p5\""));
        assert!(t.contains("theta_ja_c_per_w"));
        assert!(t.len() <= MAX_CARD_CHARS + 40);
    }

    #[test]
    fn the_prompt_names_each_template_and_warns_against_copying() {
        let found = select_from(
            &[card("lm317", "(?i)^LM317", "")],
            "TLV70033",
            ComponentKind::Vreg,
            2,
        );
        let p = prompt_section(&found);
        assert!(p.contains("EXAMPLE 1 -- lm317"));
        assert!(p.contains("WRONG ANSWER") && p.contains("ds similar"));
        assert!(prompt_section(&[]).is_empty());
        assert!(provenance_line(&found).unwrap().contains("lm317 (test)"));
    }

    #[test]
    fn the_builtin_library_offers_a_family_sibling_to_an_stm32() {
        let found = select("STM32F407VET6", "mcu", LIMIT);
        assert!(!found.is_empty());
        assert!(found[0].id.starts_with("stm32"), "{found:?}");
    }
}
