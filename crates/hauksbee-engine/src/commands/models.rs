//! `hauksbee models lint <file>`: standalone validation of model / sensor TOML.

use std::path::Path;

/// `hauksbee models lint <file>`: standalone validation of model TOML.
///
/// Dispatches on the file's root shape: a `[sensor]` table lints as a
/// register-map sensor spec (`SensorSpec::from_toml`, the validation the
/// engine interpreter applies); anything with `[[models]]` entries lints each
/// entry's kind-specific params (`hauksbee_models::validate`) and, when a
/// `[models.logic]` block is present, COMPILES it through the same
/// `LogicComponent::compile` path binding uses — schema validation, expression
/// lowering, and the exhaustive comb-cycle convergence check — so "lint said
/// ok" and "the board binds it" can never disagree.
pub fn lint(file: &Path) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| anyhow::anyhow!("reading '{}': {e}", file.display()))?;
    let root: toml::Value = toml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("'{}' is not TOML: {e}", file.display()))?;

    let mut findings = 0usize;
    let mut checked = 0usize;

    if root.get("sensor").is_some() {
        checked += 1;
        match hauksbee_models::SensorSpec::from_toml(&text) {
            Ok(spec) => println!("sensor '{}': ok", spec.sensor().name),
            Err(e) => {
                findings += 1;
                println!("sensor spec: ERROR: {e}");
            }
        }
    } else if root.get("models").is_some() {
        let db: hauksbee_models::schema::DbFile = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("'{}': [[models]] parse error: {e}", file.display()))?;
        for entry in &db.models {
            checked += 1;
            let mut entry_findings = 0usize;
            if let Err(errors) = hauksbee_models::validation::validate(entry) {
                for err in errors {
                    entry_findings += 1;
                    println!("model '{}': ERROR: {err}", entry.id);
                }
            }
            if !entry.logic.is_empty() {
                match crate::logic::LogicComponent::compile(&entry.id, &entry.logic) {
                    Ok(compiled) => {
                        for w in &compiled.warnings {
                            println!("model '{}' [models.logic]: warning: {w}", entry.id);
                        }
                    }
                    Err(e) => {
                        entry_findings += 1;
                        println!("model '{}' [models.logic]: ERROR: {e}", entry.id);
                    }
                }
            }
            if entry_findings == 0 {
                println!("model '{}': ok", entry.id);
            }
            findings += entry_findings;
        }
    } else {
        anyhow::bail!(
            "'{}' has neither a [sensor] table nor [[models]] entries — nothing to lint",
            file.display()
        );
    }

    println!(
        "{checked} item(s) checked, {findings} finding(s){}",
        if findings == 0 { " — clean" } else { "" }
    );
    if findings > 0 {
        std::process::exit(2);
    }
    Ok(())
}
