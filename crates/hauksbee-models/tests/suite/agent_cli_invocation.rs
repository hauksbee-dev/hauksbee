//! Backend naming, consent and default contracts shared by every agent CLI.
//! The exact argv each CLI receives is pinned in `datasheet.rs`'s
//! `custom_command_runs_the_selected_protocol_and_never_falls_back`.

use hauksbee_models::datasheet::{Backend, CONSENT_NOTICE};
use hauksbee_models::extract_config::{presets, ExtractConfig, FakeHost};

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
fn the_documented_defaults_are_the_haiku_ladder_high_and_gemini_flash_high() {
    let host = FakeHost::default();
    let r = ExtractConfig::default().resolve(&host, Some(Backend::ClaudeCode));
    assert_eq!((r.model(), r.effort()), ("claude-haiku-5-5", Some("high")));
    let r = ExtractConfig::default().resolve(&host, Some(Backend::Agy));
    assert_eq!((r.model(), r.effort()), ("gemini-3.8-flash", Some("high")));
    // And the presets that a fresh setup is steered to say the same thing.
    let claude = presets().iter().find(|p| p.id == "claude-code").unwrap();
    assert_eq!((claude.model, claude.effort), ("claude-haiku-5-5", "high"));
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
