//! Persistent settings for datasheet extraction: which backend reads the
//! datasheet, on which model, at what effort, and the knobs around it.
//!
//! One file, `~/.config/hauksbee/extract.toml`, serves every surface: the
//! terminal (`hauksbee models backend ...`), the web Settings page, and the
//! extraction itself. Every field is optional so the file records only what the
//! user chose; the defaults live in code, in one place, and a field left alone
//! follows them when they change.
//!
//! The three agent CLIs (Claude Code, Antigravity's `agy`, codex) are one
//! shape here: a model, an effort, a permission mode, extra arguments. What
//! differs between them (the flag each spells those with, and the values it
//! accepts) is a row in [`AGENTS`], not a separate code path.
//!
//! # Precedence
//!
//! Highest first, per field:
//!
//! 1. a command-line flag (`--backend`, `--model`, `--effort`, ...);
//! 2. an environment variable (`HAUKSBEE_EXTRACT_BACKEND`,
//!    `HAUKSBEE_CODEX_MODEL`, `HAUKSBEE_CLAUDE_MODEL`, ...), which is how a
//!    script or a CI job overrides a developer's saved choice for one run;
//! 3. this file;
//! 4. the built-in defaults.
//!
//! With no backend chosen anywhere, extraction takes the API backend when
//! `HAUKSBEE_LLM_API_KEY` is exported (the pre-flag behaviour), otherwise the
//! first agent CLI found on PATH in the order codex, claude, agy. The consent
//! surfaces name whichever wins, so "auto" never sends a datasheet somewhere
//! the user was not told about.
//!
//! # Secrets
//!
//! No secret is ever stored here. The API backend records the NAME of the
//! environment variable holding its key; the key itself is read at call time.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::datasheet::Backend;

// ── Defaults ──────────────────────────────────────────────────────────────────

/// The file's name under the hauksbee config directory.
pub const FILE_NAME: &str = "extract.toml";
/// Points at a different config file (tests, or a per-project setup).
pub const ENV_CONFIG_PATH: &str = "HAUKSBEE_EXTRACT_CONFIG";
/// Overrides the saved backend for one run.
pub const ENV_BACKEND: &str = "HAUKSBEE_EXTRACT_BACKEND";

pub const DEFAULT_RETRIES: usize = 2;
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;
pub const DEFAULT_API_BASE: &str = "https://api.openai.com/v1";
pub const DEFAULT_API_MODEL: &str = "gpt-5.6-sol";
pub const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";

/// What one agent CLI is called, what it defaults to, and what it accepts.
///
/// Reading a datasheet is not a cheap task. The values are easy (a table cell
/// is a table cell); the pin map is where a weak model fails, because package
/// drawings are rotated, mirrored, and labelled without numbers, and getting
/// one wrong produces a part that binds cleanly and simulates a different
/// device. So every CLI defaults to its strongest general tier at `high`
/// reasoning effort (high, deliberately not the maximum: the extra cost has
/// not been shown to buy a better pin map) rather than whatever the CLI
/// happens to default to.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct AgentSpec {
    pub backend: Backend,
    /// `HAUKSBEE_<prefix>_MODEL` / `_EFFORT` override the file for one run.
    pub env_prefix: &'static str,
    pub model: &'static str,
    pub effort: &'static str,
    /// The mode that lets the agent write `model.toml` in its sandbox without
    /// an interactive prompt; the sandbox directory bounds what it touches.
    pub permission_mode: &'static str,
    /// Effort levels the CLI accepts, from its own `--help`; the validator
    /// refuses anything else so a typo fails here, not ten minutes into a run.
    pub efforts: &'static [&'static str],
    pub permission_modes: &'static [&'static str],
    /// Models worth offering in a picker. Free text is always allowed too:
    /// vendors ship new names faster than this list can follow.
    pub models: &'static [&'static str],
}

/// The agent CLIs, in the order the surfaces list them.
pub const AGENTS: [AgentSpec; 3] = [
    AgentSpec {
        backend: Backend::ClaudeCode,
        env_prefix: "HAUKSBEE_CLAUDE",
        model: "claude-opus-5",
        effort: "high",
        permission_mode: "acceptEdits",
        efforts: &["low", "medium", "high", "xhigh", "max"],
        permission_modes: &[
            "acceptEdits",
            "auto",
            "bypassPermissions",
            "dontAsk",
            "manual",
            "plan",
        ],
        models: &[
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-fable-5-1",
            "opus",
            "sonnet",
        ],
    },
    AgentSpec {
        backend: Backend::Agy,
        env_prefix: "HAUKSBEE_AGY",
        model: "gemini-3.8-flash",
        effort: "high",
        permission_mode: "accept-edits",
        efforts: &["low", "medium", "high"],
        permission_modes: &["accept-edits", "plan"],
        models: &[
            "gemini-3.8-flash",
            "gemini-3.7-flash",
            "gemini-3.1-pro",
            "claude-opus-4-6-thinking",
        ],
    },
    AgentSpec {
        backend: Backend::Codex,
        env_prefix: "HAUKSBEE_CODEX",
        model: "gpt-5.6-sol",
        effort: "high",
        // codex's `--sandbox` level: writes confined to the sandbox, no network.
        permission_mode: "workspace-write",
        efforts: &["minimal", "low", "medium", "high", "xhigh"],
        permission_modes: &["read-only", "workspace-write", "danger-full-access"],
        models: &["gpt-5.6-sol", "gpt-5.6", "gpt-5.5"],
    },
];

/// The spec for an agent backend; `None` for the API.
pub fn agent_spec(backend: Backend) -> Option<&'static AgentSpec> {
    AGENTS.iter().find(|a| a.backend == backend)
}

/// Kept as names for the callers that only want to say them.
pub const DEFAULT_CLAUDE_MODEL: &str = AGENTS[0].model;
pub const DEFAULT_CLAUDE_EFFORT: &str = AGENTS[0].effort;
pub const DEFAULT_AGY_MODEL: &str = AGENTS[1].model;
pub const DEFAULT_AGY_EFFORT: &str = AGENTS[1].effort;
pub const DEFAULT_CODEX_MODEL: &str = AGENTS[2].model;
pub const DEFAULT_CODEX_EFFORT: &str = AGENTS[2].effort;

/// Model names worth offering in a picker, per backend.
pub fn suggested_models(backend: Backend) -> &'static [&'static str] {
    match agent_spec(backend) {
        Some(a) => a.models,
        None => &[
            "gpt-5.6-sol",
            "gpt-5.6",
            "anthropic/claude-opus-5",
            "google/gemini-3.8-flash",
        ],
    }
}

// ── The file ──────────────────────────────────────────────────────────────────

/// Everything `extract.toml` can hold. Every field optional; see the module
/// docs for what a missing one means.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExtractConfig {
    /// Which backend reads the datasheet. `None` means auto-detect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<Backend>,
    /// Validation retries per extraction (attempts = retries + 1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retries: Option<usize>,
    /// How long one agent-CLI run may take before it is killed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(rename = "claude-code", skip_serializing_if = "AgentConfig::is_empty")]
    pub claude_code: AgentConfig,
    #[serde(skip_serializing_if = "AgentConfig::is_empty")]
    pub agy: AgentConfig,
    #[serde(skip_serializing_if = "AgentConfig::is_empty")]
    pub codex: AgentConfig,
    #[serde(skip_serializing_if = "ApiConfig::is_empty")]
    pub api: ApiConfig,
}

/// `[claude-code]`, `[agy]`, `[codex]`: one headless agent CLI run in the
/// sandbox. The same four knobs for each; the CLI's own spelling of them is
/// the runner's business.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Claude's `--permission-mode`, agy's `--mode`, codex's `--sandbox`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// A CLI auth/config profile (`codex -p NAME`); ignored by the others.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Extra arguments appended to the command line, verbatim.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
}

/// `[api]`: an OpenAI-compatible chat-completions endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The NAME of the environment variable holding the key. Never the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

impl AgentConfig {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}
impl ApiConfig {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Trim, and turn a blank into "not set": a web form posts the fields the
/// user left alone as empty strings, and an empty string must mean the
/// default, never `--model ""`.
fn tidy(field: &mut Option<String>) {
    *field = field
        .take()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
}

fn is_env_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn check_choice(what: &str, value: &Option<String>, allowed: &[&str]) -> Result<()> {
    match value {
        Some(v) if !allowed.contains(&v.as_str()) => {
            bail!("{what} must be one of {}, not '{v}'", allowed.join(", "))
        }
        _ => Ok(()),
    }
}

fn check_model(what: &str, value: &Option<String>) -> Result<()> {
    match value {
        Some(v) if v.chars().any(char::is_whitespace) || v.starts_with('-') => {
            bail!("{what} does not look like a model name: '{v}'")
        }
        _ => Ok(()),
    }
}

impl ExtractConfig {
    /// The section for an agent backend; `None` for the API.
    pub fn agent(&self, backend: Backend) -> Option<&AgentConfig> {
        match backend {
            Backend::ClaudeCode => Some(&self.claude_code),
            Backend::Agy => Some(&self.agy),
            Backend::Codex => Some(&self.codex),
            Backend::Api => None,
        }
    }

    pub fn agent_mut(&mut self, backend: Backend) -> Option<&mut AgentConfig> {
        match backend {
            Backend::ClaudeCode => Some(&mut self.claude_code),
            Backend::Agy => Some(&mut self.agy),
            Backend::Codex => Some(&mut self.codex),
            Backend::Api => None,
        }
    }

    pub fn normalise(&mut self) {
        for a in AGENTS {
            let s = self.agent_mut(a.backend).expect("agent");
            tidy(&mut s.model);
            tidy(&mut s.effort);
            tidy(&mut s.permission_mode);
            tidy(&mut s.profile);
            s.extra_args.retain(|x| !x.trim().is_empty());
        }
        tidy(&mut self.api.base_url);
        tidy(&mut self.api.model);
        tidy(&mut self.api.api_key_env);
    }

    /// Refuse what cannot work, with the fix in the message. Runs on every
    /// save so a bad value is caught while the user is looking at the form,
    /// not ten minutes into an extraction.
    pub fn validate(&self) -> Result<()> {
        if let Some(r) = self.retries.filter(|r| *r > 10) {
            bail!("retries must be at most 10 (each retry is a full model run), not {r}");
        }
        if let Some(t) = self.timeout_secs.filter(|t| !(30..=7200).contains(t)) {
            bail!("timeout_secs must be between 30 and 7200, not {t}");
        }
        for a in AGENTS {
            let (name, s) = (a.backend.name(), self.agent(a.backend).expect("agent"));
            check_model(&format!("{name}.model"), &s.model)?;
            check_choice(&format!("{name}.effort"), &s.effort, a.efforts)?;
            check_choice(
                &format!("{name}.permission_mode"),
                &s.permission_mode,
                a.permission_modes,
            )?;
        }
        check_model("api.model", &self.api.model)?;
        if let Some(url) = self
            .api
            .base_url
            .as_deref()
            .filter(|u| !u.starts_with("http://") && !u.starts_with("https://"))
        {
            bail!("api.base_url must start with http:// or https://, not '{url}'");
        }
        if self
            .api
            .api_key_env
            .as_deref()
            .is_some_and(|n| !is_env_name(n))
        {
            bail!(
                "api.api_key_env takes the NAME of an environment variable (e.g. \
                 OPENAI_API_KEY), not the key itself. Export the key first, then \
                 name the variable."
            );
        }
        Ok(())
    }

    // ── Key/value access, for `hauksbee models backend set KEY=VALUE` ────────

    /// Every settable key, its meaning, and its default, for `--help` and the
    /// web form's hints. Order is display order.
    pub fn keys() -> Vec<KeyInfo> {
        let mut keys = vec![
            KeyInfo::new(
                "backend",
                "claude-code | agy | codex | api (blank = auto-detect)",
                "auto",
            ),
            KeyInfo::new("retries", "validation retries per extraction", "2"),
            KeyInfo::new("timeout_secs", "seconds one agent run may take", "600"),
        ];
        for a in AGENTS {
            let n = a.backend.name();
            keys.push(KeyInfo::new(
                format!("{n}.model"),
                format!("{} model", a.backend.label()),
                a.model,
            ));
            keys.push(KeyInfo::new(
                format!("{n}.effort"),
                a.efforts.join(" | "),
                a.effort,
            ));
            keys.push(KeyInfo::new(
                format!("{n}.permission_mode"),
                a.permission_modes.join(" | "),
                a.permission_mode,
            ));
            if a.backend == Backend::Codex {
                keys.push(KeyInfo::new(
                    "codex.profile",
                    "codex auth profile (codex -p NAME)",
                    "",
                ));
            }
            keys.push(KeyInfo::new(
                format!("{n}.extra_args"),
                "extra CLI arguments, space-separated",
                "",
            ));
        }
        keys.push(KeyInfo::new(
            "api.base_url",
            "OpenAI-compatible base URL",
            DEFAULT_API_BASE,
        ));
        keys.push(KeyInfo::new(
            "api.model",
            "model id at that endpoint",
            DEFAULT_API_MODEL,
        ));
        keys.push(KeyInfo::new(
            "api.api_key_env",
            "NAME of the env var holding the key",
            DEFAULT_API_KEY_ENV,
        ));
        keys
    }

    /// Set one key. An empty value clears it (back to the default). A value
    /// that fails validation leaves the config exactly as it was, so
    /// `set a=bad b=good` writes nothing.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let mut next = self.clone();
        next.set_unchecked(key.trim(), value.trim())?;
        next.validate()?;
        *self = next;
        Ok(())
    }

    fn set_unchecked(&mut self, key: &str, value: &str) -> Result<()> {
        let opt = (!value.is_empty()).then(|| value.to_string());
        let number = |what: &str| -> Result<Option<u64>> {
            Ok(match value {
                "" => None,
                v => Some(
                    v.parse()
                        .with_context(|| format!("{what} must be a number, not '{v}'"))?,
                ),
            })
        };
        let unknown = || {
            anyhow::anyhow!(
                "unknown setting '{key}'. Known settings: {}",
                Self::keys()
                    .iter()
                    .map(|k| k.key.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let Some((section, field)) = key.split_once('.') else {
            return match key {
                "backend" => {
                    self.backend = opt.filter(|v| v != "auto").map(|v| v.parse()).transpose()?;
                    Ok(())
                }
                "retries" => {
                    self.retries = number("retries")?.map(|n| n as usize);
                    Ok(())
                }
                "timeout_secs" => {
                    self.timeout_secs = number("timeout_secs")?;
                    Ok(())
                }
                _ => Err(unknown()),
            };
        };
        if section == "api" {
            let slot = match field {
                "base_url" => &mut self.api.base_url,
                "model" => &mut self.api.model,
                "api_key_env" => &mut self.api.api_key_env,
                _ => return Err(unknown()),
            };
            *slot = opt;
            return Ok(());
        }
        let agent = section
            .parse::<Backend>()
            .ok()
            .and_then(|b| self.agent_mut(b))
            .ok_or_else(unknown)?;
        match field {
            "model" => agent.model = opt,
            "effort" => agent.effort = opt,
            "permission_mode" => agent.permission_mode = opt,
            "profile" => agent.profile = opt,
            "extra_args" => {
                agent.extra_args = value.split_whitespace().map(str::to_string).collect()
            }
            _ => return Err(unknown()),
        }
        Ok(())
    }

    /// Parse `KEY=VALUE` and set it.
    pub fn set_pair(&mut self, pair: &str) -> Result<()> {
        let (key, value) = pair
            .split_once('=')
            .with_context(|| format!("expected KEY=VALUE, got '{pair}'"))?;
        self.set(key, value)
    }

    /// The file's contents, as they would be written.
    pub fn to_toml(&self) -> String {
        format!(
            "{FILE_HEADER}\n{}",
            toml::to_string_pretty(self).unwrap_or_default()
        )
    }

    /// Apply a preset: set the backend and that backend's own section. Other
    /// sections are left alone, so switching back keeps what was there.
    pub fn apply_preset(&mut self, id: &str) -> Result<&'static Preset> {
        let preset = presets().iter().find(|p| p.id == id).with_context(|| {
            format!(
                "unknown preset '{id}'. Presets: {}",
                presets()
                    .iter()
                    .map(|p| p.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        self.backend = Some(preset.backend);
        match self.agent_mut(preset.backend) {
            Some(agent) => {
                agent.model = Some(preset.model.to_string());
                agent.effort = Some(preset.effort.to_string());
            }
            None => {
                self.api.model = Some(preset.model.to_string());
                self.api.base_url = Some(preset.api_base.to_string());
                self.api.api_key_env = Some(preset.api_key_env.to_string());
            }
        }
        Ok(preset)
    }

    /// Fold the file, the environment and the defaults into one answer.
    /// `flag_backend` is a `--backend` flag, which wins outright.
    pub fn resolve(&self, host: &dyn Host, flag_backend: Option<Backend>) -> Resolved {
        let env = |name: &str| host.var(name).filter(|v| !v.trim().is_empty());
        let pick = |var: &str, file: &Option<String>, default: &str| -> String {
            env(var)
                .or_else(|| file.clone())
                .unwrap_or_else(|| default.to_string())
        };

        let (backend, backend_source) = if let Some(b) = flag_backend {
            (b, BackendSource::Flag)
        } else if let Some(b) = env(ENV_BACKEND).and_then(|v| v.parse::<Backend>().ok()) {
            (b, BackendSource::Env(ENV_BACKEND))
        } else if let Some(b) = self.backend {
            (b, BackendSource::Config)
        } else if env("HAUKSBEE_LLM_API_KEY").is_some() {
            (Backend::Api, BackendSource::Env("HAUKSBEE_LLM_API_KEY"))
        } else {
            // Nothing installed: codex, so the error names one install command.
            let found = [Backend::Codex, Backend::ClaudeCode, Backend::Agy]
                .into_iter()
                .find(|b| b.tool().is_some_and(|t| host.find(t).is_some()));
            (found.unwrap_or(Backend::Codex), BackendSource::Auto)
        };

        let agent = |spec: &AgentSpec| {
            let s = self.agent(spec.backend).expect("agent");
            ResolvedAgent {
                model: pick(&format!("{}_MODEL", spec.env_prefix), &s.model, spec.model),
                effort: pick(
                    &format!("{}_EFFORT", spec.env_prefix),
                    &s.effort,
                    spec.effort,
                ),
                permission_mode: s
                    .permission_mode
                    .clone()
                    .unwrap_or_else(|| spec.permission_mode.to_string()),
                profile: env(&format!("{}_PROFILE", spec.env_prefix)).or_else(|| s.profile.clone()),
                extra_args: s.extra_args.clone(),
            }
        };

        Resolved {
            backend,
            backend_source,
            retries: self.retries.unwrap_or(DEFAULT_RETRIES),
            timeout_secs: env("HAUKSBEE_EXTRACT_TIMEOUT_SECS")
                .and_then(|v| v.parse().ok())
                .or(self.timeout_secs)
                .unwrap_or(DEFAULT_TIMEOUT_SECS),
            claude_code: agent(&AGENTS[0]),
            agy: agent(&AGENTS[1]),
            codex: agent(&AGENTS[2]),
            api: ResolvedApi {
                base_url: pick(
                    "HAUKSBEE_LLM_BASE_URL",
                    &self.api.base_url,
                    DEFAULT_API_BASE,
                ),
                model: pick("HAUKSBEE_LLM_MODEL", &self.api.model, DEFAULT_API_MODEL),
                api_key_env: env("HAUKSBEE_API_KEY_ENV")
                    .or_else(|| self.api.api_key_env.clone())
                    .unwrap_or_else(|| {
                        if env("HAUKSBEE_LLM_API_KEY").is_some() {
                            "HAUKSBEE_LLM_API_KEY".to_string()
                        } else {
                            DEFAULT_API_KEY_ENV.to_string()
                        }
                    }),
            },
            env_overrides: KNOWN_ENV_VARS
                .iter()
                .filter(|v| env(v).is_some())
                .map(|v| v.to_string())
                .collect(),
        }
    }
}

/// One settable key, for help text.
#[derive(Debug, Clone, Serialize)]
pub struct KeyInfo {
    pub key: String,
    pub help: String,
    pub default: String,
}

impl KeyInfo {
    fn new(key: impl Into<String>, help: impl Into<String>, default: impl Into<String>) -> Self {
        KeyInfo {
            key: key.into(),
            help: help.into(),
            default: default.into(),
        }
    }
}

/// The environment variables that override the file. A settings surface shows
/// the ones that are set, because a saved choice that is silently overridden
/// by a stale `export` is the first thing people ask about.
pub const KNOWN_ENV_VARS: &[&str] = &[
    ENV_BACKEND,
    "HAUKSBEE_LLM_API_KEY",
    "HAUKSBEE_CLAUDE_MODEL",
    "HAUKSBEE_CLAUDE_EFFORT",
    "HAUKSBEE_AGY_MODEL",
    "HAUKSBEE_AGY_EFFORT",
    "HAUKSBEE_CODEX_MODEL",
    "HAUKSBEE_CODEX_EFFORT",
    "HAUKSBEE_CODEX_PROFILE",
    "HAUKSBEE_LLM_MODEL",
    "HAUKSBEE_LLM_BASE_URL",
    "HAUKSBEE_API_KEY_ENV",
    "HAUKSBEE_EXTRACT_TIMEOUT_SECS",
    ENV_CONFIG_PATH,
];

const FILE_HEADER: &str = "\
# hauksbee datasheet extraction settings.
#
# Edit here, or run `hauksbee models backend setup` / open Settings in the
# web UI. Every field is optional: a missing one takes hauksbee's default.
# Command-line flags and HAUKSBEE_* environment variables override this file.
# No secret belongs in this file: the api backend names an environment
# variable and reads the key from it at call time.
";

// ── Presets ───────────────────────────────────────────────────────────────────

/// A named, ready-to-use configuration. Enough of these that the common
/// setups are one command; the fields are still editable afterwards.
#[derive(Debug, Clone, Serialize)]
pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    /// One line on what it costs / needs.
    pub summary: &'static str,
    pub backend: Backend,
    pub model: &'static str,
    pub effort: &'static str,
    /// API presets only.
    pub api_base: &'static str,
    pub api_key_env: &'static str,
    /// True for the preset a fresh install should be steered to when its
    /// tool is present.
    pub recommended: bool,
}

const fn agent_preset(
    id: &'static str,
    label: &'static str,
    summary: &'static str,
    backend: Backend,
    model: &'static str,
    effort: &'static str,
    recommended: bool,
) -> Preset {
    Preset {
        id,
        label,
        summary,
        backend,
        model,
        effort,
        api_base: "",
        api_key_env: "",
        recommended,
    }
}

const fn api_preset(
    id: &'static str,
    label: &'static str,
    summary: &'static str,
    model: &'static str,
    api_base: &'static str,
    api_key_env: &'static str,
) -> Preset {
    Preset {
        id,
        label,
        summary,
        backend: Backend::Api,
        model,
        effort: "",
        api_base,
        api_key_env,
        recommended: false,
    }
}

pub fn presets() -> &'static [Preset] {
    &PRESETS
}

static PRESETS: [Preset; 8] = [
    agent_preset(
        "claude-code",
        "Claude Code · Opus 5, high effort",
        "Strongest reading of pin maps. Signs in with your Claude account (`claude login`).",
        Backend::ClaudeCode,
        DEFAULT_CLAUDE_MODEL,
        DEFAULT_CLAUDE_EFFORT,
        true,
    ),
    agent_preset(
        "claude-code-fast",
        "Claude Code · Sonnet 5, medium effort",
        "Faster and cheaper; fine for simple parts with a numbered pinout.",
        Backend::ClaudeCode,
        "claude-sonnet-5",
        "medium",
        false,
    ),
    agent_preset(
        "agy",
        "Antigravity (agy) · Gemini 3.8 Flash, high effort",
        "Google's agent CLI. Signs in with your Google account.",
        Backend::Agy,
        DEFAULT_AGY_MODEL,
        DEFAULT_AGY_EFFORT,
        true,
    ),
    agent_preset(
        "agy-pro",
        "Antigravity (agy) · Gemini 3.1 Pro, high effort",
        "Slower, stronger Gemini tier for dense datasheets.",
        Backend::Agy,
        "gemini-3.1-pro",
        "high",
        false,
    ),
    agent_preset(
        "codex",
        "Codex · gpt-5.6-sol, high effort",
        "OpenAI's agent CLI. Signs in with your ChatGPT account (`codex login`).",
        Backend::Codex,
        DEFAULT_CODEX_MODEL,
        DEFAULT_CODEX_EFFORT,
        true,
    ),
    api_preset(
        "openai",
        "OpenAI API · gpt-5.6-sol",
        "Direct API billing. Needs OPENAI_API_KEY exported; text only (no page images).",
        DEFAULT_API_MODEL,
        DEFAULT_API_BASE,
        DEFAULT_API_KEY_ENV,
    ),
    api_preset(
        "openrouter",
        "OpenRouter API · Claude Opus 5",
        "Any OpenRouter model by id. Needs OPENROUTER_API_KEY exported; text only.",
        "anthropic/claude-opus-5",
        "https://openrouter.ai/api/v1",
        "OPENROUTER_API_KEY",
    ),
    api_preset(
        "ollama",
        "Ollama (local) · your pulled model",
        "Nothing leaves this machine. Set api.model to a model you have pulled; no key needed.",
        "qwen3",
        "http://localhost:11434/v1",
        "OLLAMA_API_KEY",
    ),
];

// ── Resolution ────────────────────────────────────────────────────────────────

/// What the resolver reads from the machine. A trait so tests can resolve
/// against a fake environment without touching the process's own.
pub trait Host {
    fn var(&self, name: &str) -> Option<String>;
    /// The executable's path when `tool` resolves on PATH.
    fn find(&self, tool: &str) -> Option<PathBuf>;
}

/// The real process environment and PATH.
pub struct ProcessHost;

impl Host for ProcessHost {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
    fn find(&self, tool: &str) -> Option<PathBuf> {
        crate::datasheet::which_path(tool)
    }
}

/// A fixed environment, for tests and dry runs.
#[derive(Debug, Default, Clone)]
pub struct FakeHost {
    pub vars: BTreeMap<String, String>,
    pub tools: BTreeMap<String, PathBuf>,
}

impl Host for FakeHost {
    fn var(&self, name: &str) -> Option<String> {
        self.vars.get(name).cloned()
    }
    fn find(&self, tool: &str) -> Option<PathBuf> {
        self.tools.get(tool).cloned()
    }
}

/// Where the active backend choice came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "name", rename_all = "kebab-case")]
pub enum BackendSource {
    /// A `--backend` flag.
    Flag,
    /// The named environment variable.
    Env(&'static str),
    /// The config file.
    Config,
    /// Nothing chose it: the first installed agent CLI.
    Auto,
}

impl BackendSource {
    pub fn describe(self) -> String {
        match self {
            BackendSource::Flag => "chosen with --backend".to_string(),
            BackendSource::Env(name) => format!("from ${name}"),
            BackendSource::Config => "from the config file".to_string(),
            BackendSource::Auto => "auto-detected (nothing configured)".to_string(),
        }
    }
}

/// One agent CLI's effective settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedAgent {
    pub model: String,
    pub effort: String,
    pub permission_mode: String,
    pub profile: Option<String>,
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedApi {
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
}

/// The settings an extraction actually runs with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Resolved {
    pub backend: Backend,
    pub backend_source: BackendSource,
    pub retries: usize,
    pub timeout_secs: u64,
    pub claude_code: ResolvedAgent,
    pub agy: ResolvedAgent,
    pub codex: ResolvedAgent,
    pub api: ResolvedApi,
    /// The HAUKSBEE_* variables currently set, which override the file.
    pub env_overrides: Vec<String>,
}

impl Resolved {
    /// The effective settings for an agent backend; `None` for the API.
    pub fn agent(&self, backend: Backend) -> Option<&ResolvedAgent> {
        match backend {
            Backend::ClaudeCode => Some(&self.claude_code),
            Backend::Agy => Some(&self.agy),
            Backend::Codex => Some(&self.codex),
            Backend::Api => None,
        }
    }

    fn agent_mut(&mut self, backend: Backend) -> Option<&mut ResolvedAgent> {
        match backend {
            Backend::ClaudeCode => Some(&mut self.claude_code),
            Backend::Agy => Some(&mut self.agy),
            Backend::Codex => Some(&mut self.codex),
            Backend::Api => None,
        }
    }

    /// The active backend's model.
    pub fn model(&self) -> &str {
        self.agent(self.backend)
            .map_or(&self.api.model, |a| &a.model)
    }

    /// The active backend's reasoning effort (the API backend has none).
    pub fn effort(&self) -> Option<&str> {
        self.agent(self.backend).map(|a| a.effort.as_str())
    }

    /// Override the model / effort for the active backend, from flags. Empty
    /// strings mean "not chosen".
    pub fn override_model(&mut self, model: Option<&str>, effort: Option<&str>) {
        let chosen = |v: Option<&str>| {
            v.map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let backend = self.backend;
        match self.agent_mut(backend) {
            Some(a) => {
                if let Some(m) = chosen(model) {
                    a.model = m;
                }
                if let Some(e) = chosen(effort) {
                    a.effort = e;
                }
            }
            None => {
                if let Some(m) = chosen(model) {
                    self.api.model = m;
                }
            }
        }
    }

    /// "Claude Code (claude-opus-5, high effort)": the line every consent
    /// surface prints, so the user is told exactly what is about to read their
    /// datasheet.
    pub fn summary(&self) -> String {
        match self.effort() {
            Some(effort) => format!(
                "{} ({}, {effort} effort)",
                self.backend.label(),
                self.model()
            ),
            None => format!(
                "{} ({} at {})",
                self.backend.label(),
                self.model(),
                self.api.base_url
            ),
        }
    }

    /// The per-run flags a script would use to reproduce this setup.
    pub fn as_flags(&self) -> String {
        let mut s = format!("--backend {} --model {}", self.backend.name(), self.model());
        if let Some(e) = self.effort() {
            s.push_str(&format!(" --effort {e}"));
        } else {
            s.push_str(&format!(
                " --api-base {} --api-key-env {}",
                self.api.base_url, self.api.api_key_env
            ));
        }
        s
    }
}

// ── Availability ──────────────────────────────────────────────────────────────

/// Can a backend run on this machine, as far as can be told without spending
/// a model call. Sign-in state is checked only when an extraction is asked
/// for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackendStatus {
    pub id: &'static str,
    pub label: &'static str,
    pub backend: Backend,
    pub available: bool,
    /// The CLI's path, or the env var that holds the key.
    pub detail: String,
    /// The command that makes it available when it is not.
    pub install: &'static str,
    /// Where the datasheet goes when this backend is used.
    pub sends_data_to: &'static str,
}

/// Every backend's status against `host`, with `resolved` deciding what the
/// API backend needs.
pub fn availability(host: &dyn Host, resolved: &Resolved) -> Vec<BackendStatus> {
    Backend::ALL
        .iter()
        .map(|&backend| {
            let (available, detail) = match backend.tool() {
                Some(tool) => match host.find(tool) {
                    Some(path) => (true, path.display().to_string()),
                    None => (false, format!("`{tool}` is not on PATH")),
                },
                None => {
                    let name = &resolved.api.api_key_env;
                    match host.var(name).filter(|v| !v.trim().is_empty()) {
                        Some(_) => (true, format!("${name} is set")),
                        None if is_local_url(&resolved.api.base_url) => (
                            true,
                            format!("local endpoint, no key needed (${name} unset)"),
                        ),
                        None => (false, format!("${name} is not set")),
                    }
                }
            };
            BackendStatus {
                id: backend.name(),
                label: backend.label(),
                backend,
                available,
                detail,
                install: backend.install_hint(),
                sends_data_to: backend.sends_data_to(),
            }
        })
        .collect()
}

/// A loopback endpoint: nothing leaves the machine and no key is expected.
pub fn is_local_url(url: &str) -> bool {
    let rest = url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let host = rest.split(['/', ':']).next().unwrap_or("");
    matches!(
        host,
        "localhost" | "127.0.0.1" | "::1" | "[::1]" | "0.0.0.0"
    )
}

// ── The file on disk ──────────────────────────────────────────────────────────

/// Where the file lives: `$HAUKSBEE_EXTRACT_CONFIG`, else
/// `$XDG_CONFIG_HOME/hauksbee/extract.toml`, else `~/.config/hauksbee/extract.toml`.
/// Windows has no `$HOME`: there it is `%APPDATA%\hauksbee\extract.toml`, else
/// `%USERPROFILE%\.config\hauksbee\extract.toml`.
pub fn config_path() -> Result<PathBuf> {
    config_path_in(&ProcessHost)
}

pub fn config_path_in(host: &dyn Host) -> Result<PathBuf> {
    let set = |name: &str| {
        host.var(name)
            .filter(|p| !p.trim().is_empty())
            .map(PathBuf::from)
    };
    if let Some(p) = set(ENV_CONFIG_PATH) {
        return Ok(p);
    }
    if let Some(xdg) = set("XDG_CONFIG_HOME") {
        return Ok(xdg.join("hauksbee").join(FILE_NAME));
    }
    if let Some(home) = set("HOME") {
        return Ok(home.join(".config").join("hauksbee").join(FILE_NAME));
    }
    // Windows: no $HOME. %APPDATA% is the per-user roaming config root;
    // %USERPROFILE% is the home directory itself.
    if let Some(appdata) = set("APPDATA") {
        return Ok(appdata.join("hauksbee").join(FILE_NAME));
    }
    let profile = set("USERPROFILE").context(
        "none of $HAUKSBEE_EXTRACT_CONFIG, $XDG_CONFIG_HOME, $HOME, %APPDATA% or %USERPROFILE% is set, so there is nowhere to keep extraction settings",
    )?;
    Ok(profile.join(".config").join("hauksbee").join(FILE_NAME))
}

/// A config as loaded: the values, where they came from, and whether the
/// file existed.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: ExtractConfig,
    pub path: PathBuf,
    pub exists: bool,
}

/// Read the config file (a missing file is an empty config, not an error). A
/// file that exists but cannot be parsed IS an error, with the path in it:
/// silently ignoring it would run an extraction on settings the user did not
/// choose.
pub fn load() -> Result<Loaded> {
    load_from(&config_path()?)
}

pub fn load_from(path: &Path) -> Result<Loaded> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Loaded {
                config: ExtractConfig::default(),
                path: path.to_path_buf(),
                exists: false,
            })
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut config: ExtractConfig = toml::from_str(&text)
        .with_context(|| format!("{} is not a valid extraction config", path.display()))?;
    config.normalise();
    config
        .validate()
        .with_context(|| format!("{} holds a value hauksbee cannot use", path.display()))?;
    Ok(Loaded {
        config,
        path: path.to_path_buf(),
        exists: true,
    })
}

/// Load the config, or the defaults with a warning on stderr when the file is
/// unusable. For surfaces that must keep working (a status page) rather than
/// refuse (an extraction).
pub fn load_or_default() -> Loaded {
    load().unwrap_or_else(|e| {
        eprintln!("[hauksbee] ignoring extraction config: {e:#}");
        Loaded {
            config: ExtractConfig::default(),
            path: config_path().unwrap_or_default(),
            exists: false,
        }
    })
}

/// Write the config. Validated first; written atomically (a temp file then a
/// rename) so a crash mid-write cannot leave half a file behind.
pub fn save(config: &ExtractConfig, path: &Path) -> Result<()> {
    let mut config = config.clone();
    config.normalise();
    config.validate()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, config.to_toml()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("moving {} into place", tmp.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(vars: &[(&str, &str)], tools: &[&str]) -> FakeHost {
        FakeHost {
            vars: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            tools: tools
                .iter()
                .map(|t| (t.to_string(), PathBuf::from(format!("/usr/local/bin/{t}"))))
                .collect(),
        }
    }

    #[test]
    fn an_empty_config_resolves_to_the_documented_defaults() {
        let r = ExtractConfig::default().resolve(&host(&[], &["claude"]), None);
        assert_eq!(r.backend, Backend::ClaudeCode);
        assert_eq!(r.backend_source, BackendSource::Auto);
        assert_eq!(
            (r.claude_code.model.as_str(), r.claude_code.effort.as_str()),
            ("claude-opus-5", "high")
        );
        assert_eq!(
            (r.agy.model.as_str(), r.agy.effort.as_str()),
            ("gemini-3.8-flash", "high")
        );
        assert_eq!(
            (r.codex.model.as_str(), r.codex.permission_mode.as_str()),
            ("gpt-5.6-sol", "workspace-write")
        );
        assert_eq!((r.retries, r.timeout_secs), (2, 600));
        assert_eq!(r.summary(), "Claude Code (claude-opus-5, high effort)");
        for a in AGENTS {
            assert_eq!(a.effort, "high", "{}: high, not max", a.backend);
            assert!(
                a.efforts.contains(&a.effort) && a.permission_modes.contains(&a.permission_mode)
            );
        }
    }

    #[test]
    fn auto_detection_prefers_codex_then_claude_then_agy() {
        let cfg = ExtractConfig::default();
        assert_eq!(
            cfg.resolve(&host(&[], &["codex", "claude", "agy"]), None)
                .backend,
            Backend::Codex
        );
        assert_eq!(
            cfg.resolve(&host(&[], &["claude", "agy"]), None).backend,
            Backend::ClaudeCode
        );
        assert_eq!(
            cfg.resolve(&host(&[], &["agy"]), None).backend,
            Backend::Agy
        );
        assert_eq!(cfg.resolve(&host(&[], &[]), None).backend, Backend::Codex);
    }

    #[test]
    fn an_exported_llm_key_still_selects_the_api_backend() {
        let r = ExtractConfig::default()
            .resolve(&host(&[("HAUKSBEE_LLM_API_KEY", "k")], &["codex"]), None);
        assert_eq!(r.backend, Backend::Api);
        assert_eq!(r.api.api_key_env, "HAUKSBEE_LLM_API_KEY");
    }

    #[test]
    fn precedence_is_flag_then_env_then_file_then_default() {
        let mut cfg = ExtractConfig::default();
        cfg.set("backend", "agy").unwrap();
        cfg.set("claude-code.model", "from-file").unwrap();
        let h = host(
            &[("HAUKSBEE_CLAUDE_MODEL", "from-env")],
            &["codex", "claude", "agy"],
        );

        let r = cfg.resolve(&h, None);
        assert_eq!(
            (r.backend, r.backend_source),
            (Backend::Agy, BackendSource::Config)
        );
        assert_eq!(r.claude_code.model, "from-env");

        let r = cfg.resolve(&h, Some(Backend::ClaudeCode));
        assert_eq!(
            (r.backend, r.backend_source),
            (Backend::ClaudeCode, BackendSource::Flag)
        );

        let r = cfg.resolve(
            &host(&[("HAUKSBEE_EXTRACT_BACKEND", "codex")], &["codex"]),
            None,
        );
        assert_eq!(
            (r.backend, r.backend_source),
            (Backend::Codex, BackendSource::Env(ENV_BACKEND))
        );
        assert_eq!(r.claude_code.model, "from-file");
        assert!(r
            .env_overrides
            .contains(&"HAUKSBEE_EXTRACT_BACKEND".to_string()));
    }

    #[test]
    fn presets_apply_to_their_own_section_only() {
        let mut cfg = ExtractConfig::default();
        cfg.set("codex.model", "keep-me").unwrap();
        cfg.apply_preset("claude-code").unwrap();
        assert_eq!(cfg.backend, Some(Backend::ClaudeCode));
        assert_eq!(
            (
                cfg.claude_code.model.as_deref(),
                cfg.claude_code.effort.as_deref()
            ),
            (Some("claude-opus-5"), Some("high"))
        );
        assert_eq!(cfg.codex.model.as_deref(), Some("keep-me"));

        cfg.apply_preset("agy").unwrap();
        assert_eq!(
            (cfg.backend, cfg.agy.model.as_deref()),
            (Some(Backend::Agy), Some("gemini-3.8-flash"))
        );

        cfg.apply_preset("ollama").unwrap();
        assert_eq!(
            cfg.api.base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
        assert!(cfg.apply_preset("nope").is_err());

        for p in presets() {
            let mut cfg = ExtractConfig::default();
            cfg.apply_preset(p.id).unwrap();
            cfg.validate()
                .unwrap_or_else(|e| panic!("preset {}: {e:#}", p.id));
        }
    }

    #[test]
    fn set_validates_and_blank_clears() {
        let mut cfg = ExtractConfig::default();
        for bad in [
            ("claude-code.effort", "turbo"),
            ("codex.permission_mode", "acceptEdits"),
            ("api.api_key_env", "sk-abc-123"),
            ("api.base_url", "openai.com"),
            ("nonsense.key", "x"),
            ("claude-code.colour", "x"),
            ("api.colour", "x"),
            ("retries", "eleven"),
            ("retries", "11"),
            ("backend", "telepathy"),
        ] {
            assert!(cfg.set(bad.0, bad.1).is_err(), "{bad:?} must be refused");
        }
        assert_eq!(
            cfg,
            ExtractConfig::default(),
            "a refused value leaves nothing behind"
        );
        cfg.set_pair("claude-code.effort=max").unwrap();
        assert_eq!(cfg.claude_code.effort.as_deref(), Some("max"));
        cfg.set_pair("claude-code.effort=").unwrap();
        assert_eq!(cfg.claude_code.effort, None);
        cfg.set("backend", "auto").unwrap();
        assert_eq!(cfg.backend, None);
        cfg.set("agy.extra_args", "--sandbox  --log-file x")
            .unwrap();
        assert_eq!(cfg.agy.extra_args, vec!["--sandbox", "--log-file", "x"]);
        cfg.set("codex.profile", "work").unwrap();
        assert_eq!(cfg.codex.profile.as_deref(), Some("work"));
        // Every documented key is settable to its own default.
        for k in ExtractConfig::keys() {
            let value = if k.default == "auto" { "" } else { &k.default };
            cfg.set(&k.key, value)
                .unwrap_or_else(|e| panic!("{}: {e:#}", k.key));
        }
    }

    #[test]
    fn the_file_round_trips_and_records_only_what_was_set() {
        let mut cfg = ExtractConfig::default();
        cfg.apply_preset("claude-code").unwrap();
        cfg.set("timeout_secs", "900").unwrap();
        let text = cfg.to_toml();
        assert!(
            text.contains("backend = \"claude-code\"") && text.contains("[claude-code]"),
            "{text}"
        );
        assert!(
            !text.contains("[codex]") && !text.contains("[api]"),
            "untouched sections stay out: {text}"
        );
        assert_eq!(toml::from_str::<ExtractConfig>(&text).unwrap(), cfg);
        let err = toml::from_str::<ExtractConfig>("backedn = \"codex\"\n").unwrap_err();
        assert!(
            err.to_string().contains("backedn"),
            "unknown keys are refused: {err}"
        );
    }

    #[test]
    fn a_web_form_with_blank_fields_normalises_to_unset() {
        let mut cfg: ExtractConfig = serde_json::from_str(
            r#"{"backend":"agy","agy":{"model":"  ","effort":""},"api":{"base_url":" "}}"#,
        )
        .unwrap();
        cfg.normalise();
        assert_eq!(
            (cfg.agy, cfg.api, cfg.backend),
            (
                AgentConfig::default(),
                ApiConfig::default(),
                Some(Backend::Agy)
            )
        );
    }

    #[test]
    fn save_and_load_round_trip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("extract.toml");
        let missing = load_from(&path).unwrap();
        assert!(!missing.exists && missing.config == ExtractConfig::default());

        let mut cfg = ExtractConfig::default();
        cfg.apply_preset("agy-pro").unwrap();
        save(&cfg, &path).unwrap();
        let loaded = load_from(&path).unwrap();
        assert!(loaded.exists && loaded.config == cfg);
        assert!(!path.with_extension("toml.tmp").exists());

        std::fs::write(&path, "backend = \"telepathy\"\n").unwrap();
        let err = load_from(&path).unwrap_err();
        assert!(format!("{err:#}").contains("extract.toml"), "{err:#}");
    }

    #[test]
    fn the_config_path_honours_the_override_then_xdg_then_home() {
        let at = |vars: &[(&str, &str)]| config_path_in(&host(vars, &[]));
        assert_eq!(
            at(&[("HAUKSBEE_EXTRACT_CONFIG", "/x/y.toml"), ("HOME", "/h")]).unwrap(),
            PathBuf::from("/x/y.toml")
        );
        assert_eq!(
            at(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/h")]).unwrap(),
            PathBuf::from("/xdg/hauksbee/extract.toml")
        );
        assert_eq!(
            at(&[("HOME", "/h")]).unwrap(),
            PathBuf::from("/h/.config/hauksbee/extract.toml")
        );
        // Windows: no $HOME, so the roaming config root, then the profile.
        assert_eq!(
            at(&[("APPDATA", "/ad"), ("USERPROFILE", "/up")]).unwrap(),
            PathBuf::from("/ad/hauksbee/extract.toml")
        );
        assert_eq!(
            at(&[("USERPROFILE", "/up")]).unwrap(),
            PathBuf::from("/up/.config/hauksbee/extract.toml")
        );
        assert!(at(&[]).is_err());
    }

    #[test]
    fn availability_reports_each_backend_against_the_machine() {
        let h = host(&[("OPENAI_API_KEY", "k")], &["agy"]);
        let r = ExtractConfig::default().resolve(&h, None);
        let rows = availability(&h, &r);
        let ok = |id: &str| rows.iter().find(|s| s.id == id).unwrap().available;
        assert!(ok("agy") && ok("api") && !ok("claude-code") && !ok("codex"));

        let mut local = ExtractConfig::default();
        local.apply_preset("ollama").unwrap();
        let r = local.resolve(&host(&[], &[]), None);
        assert!(
            availability(&host(&[], &[]), &r)
                .iter()
                .find(|s| s.id == "api")
                .unwrap()
                .available
        );
    }

    #[test]
    fn flag_overrides_apply_to_the_active_backend_only() {
        let mut r = ExtractConfig::default().resolve(&host(&[], &["claude"]), None);
        r.override_model(Some("sonnet"), Some(""));
        assert_eq!(
            (r.claude_code.model.as_str(), r.claude_code.effort.as_str()),
            ("sonnet", "high")
        );
        assert_eq!(r.codex.model, DEFAULT_CODEX_MODEL);
        assert_eq!(
            r.as_flags(),
            "--backend claude-code --model sonnet --effort high"
        );
    }
}
