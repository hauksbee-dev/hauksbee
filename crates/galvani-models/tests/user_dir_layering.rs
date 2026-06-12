//! A custom behavioural model dropped into a user dir loads (and overrides
//! builtin) without recompiling, via `builtin_with_user_dirs(--models-dir)`.

use galvani_models::{ComponentQuery, ModelLibrary};

#[test]
fn user_models_dir_loads_custom_behavioral_part() {
    let dir = std::env::temp_dir().join("galvani_user_models_test");
    let _ = std::fs::create_dir_all(&dir);
    let toml = r#"
[[models]]
id = "my_crazy_charger"
kind = "vreg"
description = "user custom"
[models.match]
value_re = "(?i)CRAZYCHG999"
[models.params]
vout = 5.0
dropout_v = 0.3
iq_a = 0.001
[models.behavioral.converter]
topology = "boost"
out_pin = "out"
in_pin = "in"
vout_setpoint = 12.0
"#;
    std::fs::write(dir.join("crazy.toml"), toml).unwrap();

    let lib = ModelLibrary::builtin_with_user_dirs(&[dir.as_path()]);
    let q = ComponentQuery { value: Some("CRAZYCHG999".into()), ..Default::default() };
    let r = lib.resolve(&q);
    let m = r.model.expect("user part should resolve from --models-dir");
    assert_eq!(m.id, "my_crazy_charger");
    assert!(!m.behavioral.is_empty(), "user part carries its behavioural block");
    assert_eq!(r.source.as_deref(), Some("user"));

    let _ = std::fs::remove_dir_all(&dir);
}
