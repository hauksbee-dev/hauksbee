//! The Claude Code and Antigravity runners honour the same contract as codex:
//! the model and effort are always named, stderr is captured to a file, and
//! the datasheet never rides on argv.
//!
//! Like `codex_prompt_delivery`, this reads the source, because the
//! alternative is spending a real agent run on every CI build.

use std::path::Path;

use hauksbee_models::datasheet::{Backend, CONSENT_NOTICE};
use hauksbee_models::extract_config::{presets, ExtractConfig, FakeHost};

fn source() -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/datasheet.rs");
    std::fs::read_to_string(p).expect("read datasheet.rs")
}

fn body_of(function: &str) -> String {
    let s = source();
    let after = s
        .split(&format!("fn {function}("))
        .nth(1)
        .unwrap_or_else(|| panic!("{function} exists"));
    after[..after.find("\nfn ").unwrap_or(after.len())].to_string()
}

#[test]
fn one_runner_names_model_and_effort_for_every_cli() {
    let body = body_of("run_agent_once");
    for arm in [
        "Backend::Codex =>",
        "Backend::ClaudeCode =>",
        "Backend::Agy =>",
    ] {
        assert!(body.contains(arm), "run_agent_once must build {arm}");
    }
    for flag in [
        "\"--model\"",
        "\"--effort\"",
        "\"--no-session-persistence\"",
        "\"--permission-mode\"",
        "\"--mode\"",
        "\"--sandbox\"",
        "\"--add-dir\"",
        "--print=",
        "model_reasoning_effort",
    ] {
        assert!(body.contains(flag), "run_agent_once must pass {flag}");
    }
    assert!(
        body.contains("-stderr.log"),
        "every CLI's stderr must be captured"
    );
    assert!(
        !body.contains("--bare"),
        "--bare skips the keychain, so claude would never be signed in"
    );
    assert!(
        body.contains("AGENT_POINTER"),
        "agy gets the same short pointer, never the prompt"
    );
}

#[test]
fn the_consent_notice_names_every_backend_it_could_send_to() {
    for b in Backend::ALL {
        let label = match b {
            Backend::Api => "OpenAI-compatible",
            Backend::Agy => "Antigravity",
            other => other.label(),
        };
        assert!(
            CONSENT_NOTICE.contains(label),
            "CONSENT_NOTICE must mention {label}"
        );
    }
}

#[test]
fn the_documented_defaults_are_opus_5_high_and_gemini_flash_high() {
    let host = FakeHost::default();
    let r = ExtractConfig::default().resolve(&host, Some(Backend::ClaudeCode));
    assert_eq!((r.model(), r.effort()), ("claude-opus-5", Some("high")));
    let r = ExtractConfig::default().resolve(&host, Some(Backend::Agy));
    assert_eq!((r.model(), r.effort()), ("gemini-3.8-flash", Some("high")));
    // And the presets that a fresh setup is steered to say the same thing.
    let claude = presets().iter().find(|p| p.id == "claude-code").unwrap();
    assert_eq!((claude.model, claude.effort), ("claude-opus-5", "high"));
    let agy = presets().iter().find(|p| p.id == "agy").unwrap();
    assert_eq!((agy.model, agy.effort), ("gemini-3.8-flash", "high"));
}

#[test]
fn backend_names_round_trip_through_the_config_wire_format() {
    for b in Backend::ALL {
        let json = serde_json::to_string(&b).unwrap();
        assert_eq!(json, format!("\"{}\"", b.name()));
        let back: Backend = serde_json::from_str(&json).unwrap();
        assert_eq!(back, b);
    }
    assert!(serde_json::from_str::<Backend>("\"telepathy\"").is_err());
}
