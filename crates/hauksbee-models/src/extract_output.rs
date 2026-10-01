//! What an extraction leaves on disk.
//!
//! That is the draft stamped with its provenance (the `datasheet-derived` tier
//! and the PDF's sha256) and a normalised, separator-insensitive match rule,
//! published atomically without clobbering a model already there, which may
//! hold a user's hand corrections.

use std::io::Write;
use std::path::Path;

use regex_syntax::hir::literal::{Extractor, Literal};
use regex_syntax::hir::Look;
use sha2::{Digest, Sha256};
use toml_edit::{value, ArrayOfTables, Item, Table, Value};

/// Write `text` to `path` through a synced temp file in the same directory, so
/// a library scan never sees half a file.
///
/// An existing file is replaced only when `overwrite`; otherwise the persist
/// fails with `AlreadyExists`.
///
/// # Errors
///
/// Fails when the temp file cannot be created, written or synced beside `path`,
/// or when the persist fails, including `AlreadyExists` without `overwrite`.
pub fn publish(path: &Path, text: &str, overwrite: bool) -> std::io::Result<()> {
    let mut draft =
        tempfile::NamedTempFile::new_in(path.parent().unwrap_or_else(|| Path::new(".")))?;
    draft.write_all(text.as_bytes())?;
    draft.as_file().sync_all()?;
    let persisted = if overwrite {
        draft.persist(path)
    } else {
        draft.persist_noclobber(path)
    };
    persisted.map(drop).map_err(|e| e.error)
}

/// The exact, finite set of strings an anchored pattern matches.
///
/// The pattern is compiled case-insensitive, as the matcher compiles it. `None`
/// when it admits a class wider than a case fold or separator set, a repeat, or
/// no anchors.
#[must_use]
pub fn literal_set(pattern: &str) -> Option<Vec<String>> {
    let hir = regex_syntax::parse(&format!("(?i){pattern}")).ok()?;
    let props = hir.properties();
    let anchored = props.look_set_prefix().contains(Look::Start)
        && props.look_set_suffix().contains(Look::End);
    let mut extractor = Extractor::new();
    extractor.limit_class(4).limit_repeat(2).limit_total(4096);
    let seq = extractor.extract(&hir);
    let literals = seq.literals().filter(|_| anchored && seq.is_exact())?;
    let text = |l: &Literal| String::from_utf8_lossy(l.as_bytes()).into_owned();
    Some(literals.iter().map(text).collect())
}

/// A name without case or `-`/`_`/space separators.
fn fold(name: &str) -> String {
    name.replace(['-', '_', ' '], "").to_uppercase()
}

/// The match rule for a part and its ordering codes: case- and
/// separator-insensitive (`[-_ ]?` where a separator is printed), anchored, and
/// nothing else, so a sibling with other digits never matches.
#[must_use]
pub fn part_rule(part: &str, codes: &[&str]) -> String {
    format!(
        "(?i)^(?:{})$",
        rule_arms(std::iter::once(part).chain(codes.iter().copied()))
    )
}

/// The `|`-joined, separator-insensitive arms of [`part_rule`], unflagged.
fn rule_arms<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let arm = |name: &str| {
        let tokens = name.split(['-', '_', ' ']).filter(|t| !t.is_empty());
        let escaped: Vec<_> = tokens.map(|t| regex::escape(&t.to_uppercase())).collect();
        escaped.join("[-_ ]?")
    };
    let arms: Vec<String> = names.map(arm).filter(|a| !a.is_empty()).collect();
    arms.join("|")
}

/// Stamp a validated `[[models]]` draft with what the tool knows for certain.
///
/// That is the machine-draft tier, a reference pinning the exact PDF bytes,
/// and, where the match rule is the literal part name, the normalised rule over
/// the part and the ordering codes the draft lists. (A `[sensor]` spec has no
/// source.) `aliases` are the board values the draft was requested for: one
/// the value rule does not already match becomes one more exact arm, so the
/// card binds to the part it was drafted for.
///
/// # Errors
///
/// Fails when the PDF cannot be read or `raw` is not valid TOML.
pub fn finalise(raw: &str, part: &str, aliases: &[String], pdf: &Path) -> anyhow::Result<String> {
    let sha = Sha256::digest(std::fs::read(pdf)?);
    // 32 bytes once per extraction: a string per byte costs nothing here.
    #[allow(clippy::format_collect)]
    let sha: String = sha.iter().map(|b| format!("{b:02x}")).collect();
    let title = pdf.file_name().unwrap_or_default().to_string_lossy();
    let mut doc: toml_edit::DocumentMut = raw.parse()?;
    let tables = doc.get_mut("models").and_then(Item::as_array_of_tables_mut);
    for table in tables.into_iter().flat_map(ArrayOfTables::iter_mut) {
        let orderable = table.get("variant").and_then(|v| v.get("orderable"));
        let codes = orderable.and_then(Item::as_array).into_iter().flatten();
        let codes: Vec<String> = codes.filter_map(Value::as_str).map(str::to_owned).collect();
        let codes: Vec<&str> = codes.iter().map(String::as_str).collect();
        let rule = part_rule(part, &codes);
        let same = |s: &String| !s.is_ascii() || fold(s) == fold(part);
        if let Some(found) = table.get_mut("match").and_then(Item::as_table_like_mut) {
            let set = found.get("value_re").and_then(Item::as_str);
            let literal = set
                .and_then(literal_set)
                .is_some_and(|s| s.iter().all(same));
            let base = if literal {
                Some(rule)
            } else {
                set.map(str::to_owned)
            };
            let re = base
                .as_ref()
                .and_then(|b| regex::Regex::new(&format!("(?i){b}")).ok());
            let unmatched: Vec<&str> = (aliases.iter())
                .filter(|a| re.as_ref().is_some_and(|r| !r.is_match(a)))
                .map(String::as_str)
                .collect();
            // One case flag for the whole rule: a literal rule takes the board
            // values as more arms, a family rule gains one anchored group.
            let rule = match base {
                Some(_) if literal => Some(part_rule(part, &[&codes[..], &unmatched[..]].concat())),
                Some(base) if !unmatched.is_empty() => {
                    let family = base.strip_prefix("(?i)").unwrap_or(&base);
                    Some(format!(
                        "(?i)(?:{family}|^(?:{})$)",
                        rule_arms(unmatched.into_iter())
                    ))
                }
                _ => None,
            };
            if let Some(rule) = rule {
                found.insert("value_re", value(rule));
            }
        }
        let source = table.entry("source").or_insert(Item::Table(Table::new()));
        let Some(source) = source.as_table_mut() else {
            continue;
        };
        source.insert("tier", value("datasheet-derived"));
        let mut reference = Table::new();
        reference.insert("title", value(title.as_ref()));
        reference.insert("locator", value("the whole PDF, as extracted"));
        reference.insert("sha256", value(&sha));
        let empty = Item::ArrayOfTables(ArrayOfTables::new());
        match source.entry("references").or_insert(empty) {
            Item::ArrayOfTables(a) => a.push(reference),
            Item::Value(Value::Array(a)) => a.push(reference.into_inline_table()),
            _ => {}
        }
    }
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::read_to_string;

    #[test]
    fn part_rule_takes_spellings_and_codes_but_not_siblings() {
        let rule = part_rule("ABC123-002DD", &["ABC123-002DDR"]);
        let re = regex::Regex::new(&rule).unwrap();
        for ok in "ABC123-002DD,abc123_002dd,ABC123 002DD,ABC123002DD,ABC123-002DDR".split(',') {
            assert!(re.is_match(ok), "{rule} must match {ok}");
        }
        for bad in "ABC123-003DD,ABC1234-002DD,ABC123-002D,XABC123-002DD,ABC123-002DDX".split(',') {
            assert!(!re.is_match(bad), "{rule} must reject {bad}");
        }
        assert!(literal_set(&rule).is_some(), "still an exact rule");
        assert!(literal_set("^ABC12[0-9]$").is_none() && literal_set("ABC123").is_none());
    }

    #[test]
    fn finalise_stamps_tier_reference_and_rule_and_refuses_to_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("sheet.pdf");
        std::fs::write(&pdf, b"%PDF-1.4 x").unwrap();
        let raw = "[[models]]\nid = \"abc\"\n[models.match]\nvalue_re = '(?i)^ABC123\\-002DD$'\n\
                   [models.source]\ntier = \"curated-library\"\n";
        let out = finalise(raw, "ABC123-002DD", &[], &pdf).unwrap();
        let model = &toml::from_str::<toml::Value>(&out).unwrap()["models"][0];
        assert_eq!(model["source"]["tier"].as_str(), Some("datasheet-derived"));
        let cite = model["source"]["references"][0].clone();
        let cite: crate::schema::ModelSourceReference = cite.try_into().unwrap();
        assert!(cite.validate().is_ok() && cite.sha256.is_some(), "{cite:?}");
        let rule = regex::Regex::new(model["match"]["value_re"].as_str().unwrap()).unwrap();
        assert!(rule.is_match("abc123_002dd"), "{rule}");
        // A family rule the draft chose is left alone.
        let family = raw.replace("ABC123\\-002DD$", "ABC12[0-9]");
        let kept = finalise(&family, "ABC123-002DD", &[], &pdf).unwrap();
        assert!(kept.contains("ABC12[0-9]"), "{kept}");
        // A board value the rule misses becomes one more exact arm, on a literal
        // or a family rule alike; one it already matches adds nothing.
        let aliases = ["IC_ABC123-002DD/ALT".to_string(), "abc120".to_string()];
        for draft in [raw.to_string(), family] {
            let out = finalise(&draft, "ABC123-002DD", &aliases, &pdf).unwrap();
            let model = &toml::from_str::<toml::Value>(&out).unwrap()["models"][0];
            let rule = model["match"]["value_re"].as_str().unwrap();
            let re = regex::Regex::new(rule).unwrap();
            assert!(
                re.is_match("ic_abc123_002dd/alt") && re.is_match("ABC123-002DD"),
                "{rule}"
            );
            assert!(
                !re.is_match("IC_ABC123-002DD/ALTX") && !re.is_match("XABC120"),
                "{rule}"
            );
            assert_eq!(rule.matches("ALT").count(), 1, "{rule}");
            assert_eq!(rule.matches("(?i)").count(), 1, "one case flag: {rule}");
        }

        let target = dir.path().join("abc.toml");
        publish(&target, "one", false).unwrap();
        let refused = publish(&target, "two", false).unwrap_err().kind();
        assert_eq!(refused, std::io::ErrorKind::AlreadyExists);
        assert_eq!(read_to_string(&target).unwrap(), "one");
        publish(&target, "two", true).unwrap();
        assert_eq!(read_to_string(&target).unwrap(), "two");
    }
}
