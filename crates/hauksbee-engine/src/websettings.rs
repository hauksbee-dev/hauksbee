//! The `/api/settings/extract` backend: read, save, apply a preset, and run a
//! live connectivity check against `~/.config/hauksbee/extract.toml`.
//!
//! This is the web half of `hauksbee models backend ...`. The persistent
//! settings layer itself (`hauksbee_models::extract_config`) is the single
//! source of truth; everything here is a thin JSON wrapper around it, so the
//! CLI, the Settings page, and the extraction itself can never disagree about
//! which backend is configured or what it will run on.
//!
//! [`webextract`](crate::webextract) resolves through the same
//! `extract_config` layer for its own readiness check, so a value changed
//! here is picked up there without either module knowing about the other.

// A browser reaches this module. See `webextract`'s equivalent note: a panic
// here is a denial of service, not a CLI crash, so failures are typed
// messages the caller reports. Test code is exempt.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use hauksbee_frontdoor_api::frontdoor::ExtractSettingsHooks;
use hauksbee_models::datasheet::{self, Backend};
use hauksbee_models::extract_config::{self, ExtractConfig, ProcessHost};

/// The hooks the server mounts at `/api/settings/extract*`.
pub fn hooks() -> ExtractSettingsHooks {
    use std::sync::Arc;
    ExtractSettingsHooks {
        get: Arc::new(settings_json),
        save: Arc::new(settings_save),
        preset: Arc::new(settings_preset),
        test: Arc::new(settings_test),
    }
}

/// `GET /api/settings/extract`: the whole settings payload as JSON.
///
/// A config file that exists but fails to parse must not blank the page: the
/// user still needs to see the rest of the settings surface (and the presets,
/// which are how they would fix it) with `load_error` naming what is wrong.
pub fn settings_json() -> String {
    let (config, path, exists, load_error) = match extract_config::load() {
        Ok(loaded) => (loaded.config, loaded.path, loaded.exists, None),
        Err(e) => {
            let path = extract_config::config_path().unwrap_or_default();
            (
                ExtractConfig::default(),
                path,
                false,
                Some(format!("{e:#}")),
            )
        }
    };
    build_payload(&config, &path, exists, load_error)
}

/// Assemble the settings payload for a config already loaded (or defaulted).
fn build_payload(
    config: &ExtractConfig,
    path: &std::path::Path,
    exists: bool,
    load_error: Option<String>,
) -> String {
    let resolved = config.resolve(&ProcessHost, None);
    let backends = extract_config::availability(&ProcessHost, &resolved);

    // One row per agent CLI (`extract_config::AGENTS`) builds the three option
    // lists together, plus the api backend's model suggestions, which has no
    // `AgentSpec` of its own.
    let mut efforts = serde_json::Map::new();
    let mut permission_modes = serde_json::Map::new();
    let mut models = serde_json::Map::new();
    for a in extract_config::AGENTS {
        let id = a.backend.name().to_string();
        efforts.insert(id.clone(), serde_json::json!(a.efforts));
        permission_modes.insert(id.clone(), serde_json::json!(a.permission_modes));
        models.insert(id, serde_json::json!(a.models));
    }
    models.insert(
        "api".to_string(),
        serde_json::json!(extract_config::suggested_models(Backend::Api)),
    );

    let mut value = serde_json::json!({
        "path": path.display().to_string(),
        "exists": exists,
        "config": config,
        "resolved": resolved,
        "summary": resolved.summary(),
        "backends": backends,
        "presets": extract_config::presets(),
        "options": {
            "efforts": efforts,
            "permission_modes": permission_modes,
            "models": models,
        },
        "keys": ExtractConfig::keys(),
        "consent_notice": datasheet::CONSENT_NOTICE,
    });
    if let Some(message) = load_error {
        value["load_error"] = serde_json::Value::String(message);
    }
    serde_json::to_string(&value).unwrap_or_else(|e| {
        format!("{{\"error\":\"could not serialise extraction settings: {e}\"}}")
    })
}

/// `PUT /api/settings/extract`: replace the saved config with the JSON body
/// (the same shape `GET` returns under `config`). Refused, with a readable
/// reason, before anything is written: an unknown field, a bad backend name,
/// or an out-of-range value never reaches the file.
pub fn settings_save(body: &str) -> Result<String, String> {
    let mut cfg: ExtractConfig = serde_json::from_str(body)
        .map_err(|e| format!("the settings body is not a valid extraction config: {e}"))?;
    cfg.normalise();
    cfg.validate().map_err(|e| format!("{e:#}"))?;
    let path = extract_config::config_path().map_err(|e| format!("{e:#}"))?;
    extract_config::save(&cfg, &path).map_err(|e| format!("{e:#}"))?;
    Ok(settings_json())
}

/// `POST /api/settings/extract/preset/{id}`: apply a named preset (the
/// backend plus that backend's own model/effort) on top of whatever is
/// already saved, and keep it.
pub fn settings_preset(id: &str) -> Result<String, String> {
    let loaded = extract_config::load().map_err(|e| format!("{e:#}"))?;
    let mut cfg = loaded.config;
    cfg.apply_preset(id).map_err(|e| format!("{e:#}"))?;
    extract_config::save(&cfg, &loaded.path).map_err(|e| format!("{e:#}"))?;
    Ok(settings_json())
}

/// `POST /api/settings/extract/test`: run the currently configured backend on
/// a one-line connectivity check, streaming progress as it goes.
pub fn settings_test(progress: &mut dyn FnMut(&str)) -> Result<String, String> {
    let loaded = extract_config::load().map_err(|e| format!("{e:#}"))?;
    let resolved = loaded.config.resolve(&ProcessHost, None);
    datasheet::smoke_test(&resolved, progress).map_err(|e| format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Serialize the env-var mutations: `extract_config::load` (through
    /// `config_path`) reads `HAUKSBEE_EXTRACT_CONFIG`, a process-global, so
    /// two tests pointing it at different files at once would race.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Point `HAUKSBEE_EXTRACT_CONFIG` at a fresh path under a fresh tempdir,
    /// for the lifetime of the guard. The tempdir is returned too so the
    /// caller can keep it alive (dropping it deletes the directory).
    fn with_config_path() -> (
        std::sync::MutexGuard<'static, ()>,
        tempfile::TempDir,
        PathBuf,
    ) {
        let guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("extract.toml");
        // SAFETY: guarded by ENV_LOCK; single-threaded within this test.
        unsafe { std::env::set_var(extract_config::ENV_CONFIG_PATH, &path) };
        (guard, dir, path)
    }

    #[test]
    fn settings_json_parses_and_lists_every_backend_and_preset() {
        let (_guard, _dir, _path) = with_config_path();
        let text = settings_json();
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert!(value.get("load_error").is_none(), "{text}");
        let backends = value["backends"].as_array().expect("backends array");
        assert_eq!(backends.len(), 4, "{text}");
        let presets = value["presets"].as_array().expect("presets array");
        assert!(!presets.is_empty(), "{text}");
        assert!(value["consent_notice"]
            .as_str()
            .is_some_and(|s| !s.is_empty()));
    }

    #[test]
    fn saving_a_valid_config_writes_it_and_resolves_the_chosen_backend() {
        let (_guard, _dir, path) = with_config_path();
        let text = settings_save(
            r#"{"backend":"agy","agy":{"model":"gemini-3.8-flash","effort":"high"}}"#,
        )
        .expect("a valid config saves");
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(value["resolved"]["backend"], "agy", "{text}");
        assert_eq!(value["exists"], true, "{text}");
        assert!(
            path.is_file(),
            "the config file was written to {}",
            path.display()
        );
    }

    #[test]
    fn saving_an_unknown_backend_is_refused_and_names_it() {
        let (_guard, _dir, _path) = with_config_path();
        let err = settings_save(r#"{"backend":"telepathy"}"#).expect_err("unknown backend");
        assert!(err.contains("telepathy"), "{err}");
    }

    #[test]
    fn saving_an_out_of_range_effort_is_refused_and_names_it() {
        let (_guard, _dir, _path) = with_config_path();
        let err = settings_save(r#"{"agy":{"effort":"ultra"}}"#).expect_err("bad effort");
        assert!(err.contains("ultra"), "{err}");
    }

    #[test]
    fn applying_a_preset_resolves_to_its_documented_model_and_effort() {
        let (_guard, _dir, _path) = with_config_path();
        let text = settings_preset("claude-code").expect("a known preset applies");
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(value["resolved"]["backend"], "claude-code", "{text}");
        assert_eq!(
            value["resolved"]["claude_code"]["model"], "claude-opus-5",
            "{text}"
        );
        assert_eq!(value["resolved"]["claude_code"]["effort"], "high", "{text}");
    }
}
