//! Every doc path the binaries emit must map to a published route.
//!
//! The docs site publishes its URL contract as machine-readable JSON. A copy
//! is vendored at `tests/fixtures/url-contract.json`; refresh it with:
//!
//! ```text
//! curl -s https://hauksbee-docs.eoghancollins0.workers.dev/url-contract.json \
//!   -o crates/hauksbee-ir/tests/fixtures/url-contract.json
//! ```
//!
//! This test sweeps every non-comment `docs/**.md` (and top-level `*.md`)
//! reference in the workspace's crate sources, renders it through
//! [`hauksbee_ir::docs_url`], and asserts the contract maps that exact repo
//! path to that exact route. A failure means either (a) a user-facing string
//! points at a doc page the site does not publish, or (b) the page's
//! published route diverges
//! from the plain lowercase rule, in which case the call site must change,
//! not the helper.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use hauksbee_ir::docs_url;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

/// Collect `path -> route` from the vendored contract: explicit special
/// cases win, then the `pages` map.
fn contract_routes(json: &serde_json::Value) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for key in ["pages", "specialCases"] {
        let obj = json[key]
            .as_object()
            .unwrap_or_else(|| panic!("url-contract.json has no `{key}` object"));
        for (path, route) in obj {
            map.insert(
                path.clone(),
                route.as_str().expect("route string").to_string(),
            );
        }
    }
    map
}

/// Find doc-path references in one Rust source file, skipping comment lines
/// (`//`, `//!`, `///`): comments are not user-facing output.
fn doc_refs_in(source: &str) -> Vec<String> {
    let mut refs = Vec::new();
    for line in source.lines() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        let bytes = line.as_bytes();
        let mut i = 0;
        while let Some(pos) = line[i..].find("docs/") {
            let start = i + pos;
            // Require the reference to start a path component (not e.g. a URL
            // that already went through the helper, `.dev/docs/...`).
            if start > 0 && bytes[start - 1] == b'/' {
                i = start + 5;
                continue;
            }
            let rest = &line[start..];
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || "/_-.".contains(c)))
                .unwrap_or(rest.len());
            let mut candidate = &rest[..end];
            // Trim trailing punctuation that the char class admits ('.', a
            // sentence ending) while keeping the `.md` suffix intact.
            while let Some(stripped) = candidate.strip_suffix('.') {
                candidate = stripped;
            }
            if candidate.ends_with(".md") {
                refs.push(candidate.to_string());
            }
            i = start + 5;
        }
    }
    refs
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("readable dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            // Skip test code and build output: the sweep is about what the
            // shipped binaries emit.
            if name == "tests" || name == "target" || name == "benches" || name == "examples" {
                continue;
            }
            rust_sources(&path, out);
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

#[test]
fn every_emitted_doc_path_maps_to_a_published_route() {
    let root = workspace_root();
    let contract: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/url-contract.json"),
        )
        .expect("vendored url-contract.json (see module docs for the refresh command)"),
    )
    .expect("valid contract JSON");
    let routes = contract_routes(&contract);
    let base = contract["base"].as_str().expect("contract base URL");

    let mut sources = Vec::new();
    rust_sources(&root.join("crates"), &mut sources);
    assert!(
        sources.len() > 50,
        "sweep found suspiciously few sources ({}): wrong root?",
        sources.len()
    );

    let mut failures = Vec::new();
    let mut checked = 0usize;
    for file in &sources {
        let text = fs::read_to_string(file).expect("readable source");
        for doc_path in doc_refs_in(&text) {
            checked += 1;
            let rel = file.strip_prefix(&root).unwrap_or(file).display();
            match routes.get(&doc_path) {
                None => failures.push(format!(
                    "{rel}: `{doc_path}` is not a published page in url-contract.json"
                )),
                Some(route) => {
                    let expected = format!("{base}{route}");
                    if docs_url(&doc_path) != expected {
                        failures.push(format!(
                            "{rel}: `{doc_path}` publishes at {expected} but docs_url renders {}",
                            docs_url(&doc_path)
                        ));
                    }
                }
            }
        }
    }

    assert!(
        checked > 10,
        "sweep found suspiciously few doc references ({checked}): regex or layout drift?"
    );
    assert!(
        failures.is_empty(),
        "doc references that break the URL contract:\n  {}",
        failures.join("\n  ")
    );
}
