//! `hauksbee-ci add <spec> <kind> key=value ...`: put one assertion into a
//! spec from one command line.
//!
//! A person who has seen an output (a net toggling, a banner on the UART, a
//! reset that should not happen) can say what they saw or what they expect
//! without opening the spec. The fields are the catalogue's: a key the kind
//! does not have is refused with the kind's real keys, a value is read as the
//! type the field has, and the result is checked the way `hauksbee-ci check`
//! checks a spec (against the board too) before a byte of the file changes.
//! With `--observe` the numbers come from one nominal run of the spec, by the
//! same rules the app uses for "assert what this does".

use std::path::{Path, PathBuf};

use crate::catalog;
use crate::check::{check_spec, CheckOptions};
use crate::init::render_assert_block;
use crate::spec::{Assertion, Spec};

/// What [`add_assertion`] did.
#[derive(Debug, Clone)]
pub struct Added {
    /// The `[[assert]]` text that was (or, in a dry run, would be) appended.
    pub block: String,
    /// The assertion as the spec's reports will name it.
    pub label: String,
    /// The spec it went into.
    pub path: PathBuf,
    /// False for a dry run: nothing was written.
    pub wrote: bool,
}

/// How to take the assertion.
#[derive(Debug, Clone, Copy, Default)]
pub struct AddOptions {
    /// Take the missing numbers from one nominal run of the spec.
    pub observe: bool,
    /// Check and print the block; write nothing.
    pub dry_run: bool,
}

/// Append one `[[assert]]` of `kind` with `pairs` (`key=value`) to the spec at `path`.
///
/// # Errors
///
/// A sentence saying what is wrong, before anything is written: an unknown kind or
/// field, a value of the wrong type, a spec that would no longer check, or (with
/// `observe`) a reading the run could not honestly take.
pub fn add_assertion(
    path: &Path,
    kind: &str,
    pairs: &[String],
    opts: AddOptions,
) -> Result<Added, String> {
    let entry = catalog::entry(kind).ok_or_else(|| {
        let kinds: Vec<&str> = catalog::CATALOG.iter().map(|e| e.kind).collect();
        format!(
            "unknown assertion kind '{kind}'{}; see: hauksbee-ci explain",
            crate::error::did_you_mean_hint(kind, &kinds)
        )
    })?;
    let mut fields = parse_pairs(entry.kind, pairs)?;
    let mut notes = Vec::new();
    if opts.observe {
        notes.push(observe_into(path, entry.kind, &mut fields)?);
    }

    let original =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let block = render_assert_block(entry.kind, &fields, &notes, false);
    let mut text = original.clone();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push('\n');
    text.push_str(&block);

    let label = verify(path, &original, &text)?;
    if !opts.dry_run {
        write_atomically(path, &text)?;
    }
    Ok(Added {
        block,
        label,
        path: path.to_path_buf(),
        wrote: !opts.dry_run,
    })
}

/// `key=value` words as typed TOML values, in the order given.
fn parse_pairs(kind: &str, pairs: &[String]) -> Result<Vec<(String, toml::Value)>, String> {
    let mut fields: Vec<(String, toml::Value)> = Vec::new();
    for pair in pairs {
        let (key, raw) = pair.split_once('=').ok_or_else(|| {
            format!(
                "`{pair}` is not key=value; for example `hauksbee-ci add spec.toml {kind} net=D13`"
            )
        })?;
        let key = key.trim();
        if key == "kind" {
            return Err("`kind` is the second argument, not a key=value".into());
        }
        if fields.iter().any(|(k, _)| k == key) {
            return Err(format!("`{key}` is given twice"));
        }
        fields.push((
            key.to_string(),
            catalog::parse_field_value(kind, key, raw.trim())?,
        ));
    }
    Ok(fields)
}

/// Fill the numbers `fields` leaves out from one nominal run of the spec at
/// `path`; returns the note that says where they came from.
fn observe_into(
    path: &Path,
    kind: &str,
    fields: &mut Vec<(String, toml::Value)>,
) -> Result<String, String> {
    let spec = Spec::load(path).map_err(|e| e.to_string())?;
    let chosen = assertion_from(kind, fields)?;
    let reading = crate::measure::measure_assertion(spec, chosen)?;
    let value = reading["value"].as_f64().ok_or_else(|| {
        format!(
            "the run could not read a number for `{kind}`: {}",
            reading["detail"].as_str().unwrap_or("no value")
        )
    })?;
    let unit = reading["unit"].as_str().unwrap_or("");
    for (key, v) in catalog::measured_fields(kind, value) {
        if !fields.iter().any(|(k, _)| k == key) {
            fields.push((key.to_string(), v));
        }
    }
    Ok(format!("measured on one nominal run: {value:.4} {unit}")
        .trim_end()
        .to_string())
}

/// The assertion `kind` + `fields` spell, through the same deserialiser a spec uses.
fn assertion_from(kind: &str, fields: &[(String, toml::Value)]) -> Result<Assertion, String> {
    let mut table = toml::Table::new();
    table.insert("kind".into(), toml::Value::String(kind.into()));
    for (k, v) in fields {
        table.insert(k.clone(), v.clone());
    }
    toml::Value::Table(table)
        .try_into::<Assertion>()
        .map_err(|e| e.to_string())
}

/// Check `proposed` the way `hauksbee-ci check` would, and refuse it if it
/// introduces a finding `original` did not have. Returns the new assertion's label.
fn verify(path: &Path, original: &str, proposed: &str) -> Result<String, String> {
    let beside = |text: &str, tag: &str| -> Result<PathBuf, String> {
        let name = path
            .file_name()
            .map_or_else(|| "spec.toml".into(), |n| n.to_string_lossy().into_owned());
        let tmp = path.with_file_name(format!(".{name}.{tag}.{}.toml", std::process::id()));
        std::fs::write(&tmp, text).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        Ok(tmp)
    };
    let after = beside(proposed, "add")?;
    let result = (|| {
        let opts = CheckOptions::default();
        let new = check_spec(&after, &opts);
        if new.is_empty() {
            return Ok(());
        }
        // Findings the spec already had are not this command's to refuse.
        let before = beside(original, "before")?;
        let old: Vec<String> = check_spec(&before, &opts)
            .into_iter()
            .map(|d| d.message)
            .collect();
        let _ = std::fs::remove_file(&before);
        let introduced: Vec<String> = new
            .into_iter()
            .filter(|d| !old.contains(&d.message))
            .map(|d| {
                d.fix
                    .map_or_else(|| d.message.clone(), |fix| format!("{} ({fix})", d.message))
            })
            .collect();
        if introduced.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "that assertion would not check, so nothing was written:\n  {}",
                introduced.join("\n  ")
            ))
        }
    })();
    let label = result.and_then(|()| {
        Spec::load(&after)
            .map_err(|e| e.to_string())
            .map(|s| s.asserts.last().map(Assertion::label).unwrap_or_default())
    });
    let _ = std::fs::remove_file(&after);
    label
}

/// Replace `path` with `text` without ever leaving it half-written.
fn write_atomically(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("toml.new");
    std::fs::write(&tmp, text).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("replacing {}: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = "board = \"b.kicad_pcb\"\nfirmware = \"fw.hex\"\nduration_ms = 100\n\n\
                        # keep this comment\n[[assert]]\nkind = \"no_faults\"\n";

    /// A spec in a fresh directory, with no board file: `check` then reports the
    /// missing board and firmware as it always did, which `add` must not blame
    /// on the assertion it is adding.
    fn spec(dir: &tempfile::TempDir) -> PathBuf {
        let p = dir.path().join("spec.toml");
        std::fs::write(&p, SPEC).unwrap();
        p
    }

    fn words(pairs: &[&str]) -> Vec<String> {
        pairs.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_valid_assertion_is_appended_and_the_rest_of_the_file_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let p = spec(&dir);
        let added = add_assertion(
            &p,
            "uart",
            &words(&["contains=hello"]),
            AddOptions::default(),
        )
        .unwrap();
        assert!(added.wrote && added.label.contains("hello"), "{added:?}");
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with(SPEC), "{text}");
        assert!(text.ends_with("[[assert]]\nkind = \"uart\"\ncontains = \"hello\"\n"));
        // The round trip: the file loads and the new assertion is last.
        let loaded = Spec::load(&p).unwrap();
        assert_eq!(loaded.asserts.len(), 2);
        assert_eq!(loaded.asserts[1].contains.as_deref(), Some("hello"));
    }

    #[test]
    fn a_dry_run_prints_the_block_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = spec(&dir);
        let added = add_assertion(
            &p,
            "resets",
            &words(&["max_resets=0"]),
            AddOptions {
                dry_run: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!added.wrote && added.block.contains("max_resets = 0"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), SPEC);
    }

    #[test]
    fn a_bench_kind_is_accepted_into_the_spec() {
        let dir = tempfile::tempdir().unwrap();
        let p = spec(&dir);
        let added = add_assertion(
            &p,
            "bus",
            &words(&["protocol=i2c", "addr=0x48", "min_transactions=2"]),
            AddOptions::default(),
        )
        .unwrap();
        assert!(added.block.contains("addr = 72"), "{}", added.block);
        assert!(added.label.contains("0x48"), "{}", added.label);
    }

    #[test]
    fn a_wrong_field_names_the_real_ones() {
        let dir = tempfile::tempdir().unwrap();
        let p = spec(&dir);
        let err = add_assertion(
            &p,
            "toggle",
            &words(&["net=D13", "frequency=5"]),
            AddOptions::default(),
        )
        .unwrap_err();
        assert!(
            err.contains("no field `frequency`") && err.contains("freq_hz"),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), SPEC);
    }

    #[test]
    fn a_wrong_type_and_an_invalid_combination_are_refused_unwritten() {
        let dir = tempfile::tempdir().unwrap();
        let p = spec(&dir);
        let err = add_assertion(
            &p,
            "toggle",
            &words(&["net=D13", "freq_hz=fast"]),
            AddOptions::default(),
        )
        .unwrap_err();
        assert!(err.contains("wants a number"), "{err}");
        // Valid words, invalid assertion: an I2C address past 7 bits.
        let err = add_assertion(
            &p,
            "bus",
            &words(&["protocol=i2c", "addr=200"]),
            AddOptions::default(),
        )
        .unwrap_err();
        assert!(err.contains("would not check"), "{err}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), SPEC);
        let err = add_assertion(&p, "voltge", &[], AddOptions::default()).unwrap_err();
        assert!(err.contains("did you mean 'voltage'"), "{err}");
    }

    #[test]
    fn a_finding_the_spec_already_had_does_not_block_a_good_addition() {
        // No board or firmware file exists, so `check` has findings before and after.
        let dir = tempfile::tempdir().unwrap();
        let p = spec(&dir);
        assert!(!check_spec(&p, &CheckOptions::default()).is_empty());
        add_assertion(&p, "no_faults", &[], AddOptions::default()).unwrap();
    }
}
