//! The crate doc's layer table must state every resolution layer. It said
//! five for months after user-config-dir (priority 25) landed, so the layer
//! list a pack author reads first disagreed with the resolver. Pin the doc to
//! the enum.

#[test]
fn crate_doc_lists_all_six_layers_with_their_priorities() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"),
    )
    .expect("read lib.rs");
    let doc: String = src
        .lines()
        .take_while(|l| l.starts_with("//!"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(doc.contains("six explicit priority layers"), "doc: {doc}");
    for (name, priority) in [
        ("built-in db", "| 0 "),
        ("installed packs", "| 10 "),
        ("user model dir", "| 20 "),
        ("user config dir", "| 25 "),
        ("`--models-dir`", "| 30 "),
        ("user SPICE cards", "| 40 "),
    ] {
        assert!(
            doc.contains(name) && doc.contains(priority),
            "layer '{name}' ({priority}) missing from the crate doc table"
        );
    }
}
