//! A model's pin names must be the BINDER's role vocabulary, not the
//! datasheet's.
//!
//! The failure this guards is quiet and misleading. The TP4054's datasheet
//! calls its output BAT, so the entry named pin 3 "bat". The model then matched
//! the part, loaded cleanly, passed every schema check, and the report said
//! "vreg output not connected" against the board. That reads as a wiring fault
//! in the user's design, and it was a model naming its own pin in a vocabulary
//! nothing consumes.
//!
//! The roles each binder path reads are the lint's own table
//! (`validation::missing_pin_roles`), so this sweep and `models lint` cannot
//! disagree. Read straight from the db files: built-ins are not linted at load.

#[test]
fn every_builtin_card_declares_the_roles_its_binder_reads() {
    let db = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("db");
    let (mut offenders, mut checked) = (Vec::new(), 0);
    for path in std::fs::read_dir(db)
        .expect("db dir")
        .flatten()
        .map(|f| f.path())
    {
        let text = std::fs::read_to_string(&path).expect("db file");
        let Ok(file) = toml::from_str::<hauksbee_models::schema::DbFile>(&text) else {
            continue;
        };
        for entry in &file.models {
            checked += 1;
            let missing = hauksbee_models::validation::missing_pin_roles(entry);
            offenders.extend(missing.into_iter().map(|m| format!("{}: {m}", entry.id)));
        }
    }
    assert!(
        checked > 100,
        "only {checked} cards parsed, so this checked little"
    );
    assert!(
        offenders.is_empty(),
        "cards that bind OPEN:\n  {}",
        offenders.join("\n  ")
    );
}
