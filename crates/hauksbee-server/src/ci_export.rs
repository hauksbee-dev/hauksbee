//! The Checks run's formal exports: the `JUnit` XML, the `run --json` document
//! and the reproduction manifest, exactly as `hauksbee-ci run --json --junit
//! --emit-manifest` wrote them for that run.
//!
//! The check runner hands its answer back with an `exports` object carrying the
//! three files' bytes. [`stash`] keeps them under a fresh id and gives the page
//! the run JSON without the bulky copies, plus `export_id`; `GET
//! /api/ci/export/{id}/{file}` then serves each file as a download. Nothing here
//! writes a format: the bytes are the CLI writers' own, so a reviewer gets the
//! same file a pipeline would.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use axum::extract::Path as UrlPath;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

/// The three files one run exported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunExports {
    /// The `hauksbee-ci run --json` line, byte for byte.
    pub json: String,
    /// The `--junit` file.
    pub junit: String,
    /// The `--emit-manifest` file (absent when the run could not capture one).
    pub manifest: Option<String>,
}

/// How many runs are kept; the oldest goes first. A page only ever exports the
/// run it is showing, so a short memory is enough.
const KEPT_RUNS: usize = 16;

type Store = Mutex<VecDeque<(String, RunExports)>>;

fn store() -> &'static Store {
    static STORE: OnceLock<Store> = OnceLock::new();
    STORE.get_or_init(Default::default)
}

/// A fresh id: process start time and a counter, so two servers or two runs
/// never share one.
fn fresh_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    static EPOCH: OnceLock<u128> = OnceLock::new();
    let epoch = EPOCH.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    });
    format!("{epoch:x}-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Keep the runner's `exports` and answer the page's JSON.
///
/// The answer is the same object without `exports`, with `export_id` added. An
/// answer that carries no exports
/// (an error, a scaffold, a catalogue) is returned untouched.
#[must_use]
pub fn stash(runner_json: String) -> String {
    let Ok(serde_json::Value::Object(mut fields)) = serde_json::from_str(&runner_json) else {
        return runner_json;
    };
    let Some(serde_json::Value::Object(exports)) = fields.remove("exports") else {
        return runner_json;
    };
    let text = |key: &str| {
        exports
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let (Some(json), Some(junit)) = (text("json"), text("junit")) else {
        return serde_json::Value::Object(fields).to_string();
    };
    let id = fresh_id();
    {
        let mut kept = store()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if kept.len() >= KEPT_RUNS {
            kept.pop_front();
        }
        let manifest = text("manifest");
        kept.push_back((
            id.clone(),
            RunExports {
                json,
                junit,
                manifest,
            },
        ));
    }
    fields.insert("export_id".into(), id.into());
    serde_json::Value::Object(fields).to_string()
}

/// One exported file of a kept run: its content type, download name and bytes.
/// `file` is `results.xml`, `report.json` or `manifest.json`.
#[must_use]
pub fn lookup(id: &str, file: &str) -> Option<(&'static str, String)> {
    let run = store()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|(kept_id, _)| kept_id == id)?
        .1
        .clone();
    match file {
        "results.xml" => Some(("application/xml", run.junit)),
        "report.json" => Some(("application/json", run.json)),
        "manifest.json" => run.manifest.map(|m| ("application/json", m)),
        _ => None,
    }
}

/// `GET /api/ci/export/{id}/{file}`.
pub fn routes() -> Router {
    Router::new().route("/api/ci/export/{id}/{file}", get(export_handler))
}

async fn export_handler(
    UrlPath((id, file)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Some(refused) = crate::frontdoor::reject_cross_site(&headers) {
        return refused.into_response();
    }
    let Some((content_type, body)) = lookup(&id, &file) else {
        return (
            StatusCode::NOT_FOUND,
            "that run's export is no longer kept: run the checks again",
        )
            .into_response();
    };
    let name = format!("hauksbee-checks-{file}");
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runner_answer_without_exports_is_untouched() {
        let plain = "{\"ok\":false,\"error\":\"no\"}".to_string();
        assert_eq!(stash(plain.clone()), plain);
        assert_eq!(stash("not json".into()), "not json");
    }

    #[test]
    fn exports_are_kept_verbatim_and_dropped_from_the_page_json() {
        let cli_line = "{\"ok\":true,\"passed\":true,\"results\":[]}";
        let runner = serde_json::json!({
            "ok": true, "passed": true, "results": [],
            "exports": { "json": cli_line, "junit": "<testsuites/>\n", "manifest": "{\"m\":1}" },
        })
        .to_string();
        let page: serde_json::Value = serde_json::from_str(&stash(runner)).unwrap();
        assert!(page.get("exports").is_none());
        let id = page["export_id"].as_str().unwrap();
        assert_eq!(lookup(id, "report.json").unwrap().1, cli_line);
        assert_eq!(
            lookup(id, "results.xml").unwrap(),
            ("application/xml", "<testsuites/>\n".into())
        );
        assert_eq!(lookup(id, "manifest.json").unwrap().1, "{\"m\":1}");
        assert!(lookup(id, "../etc").is_none());
        assert!(lookup("unknown", "results.xml").is_none());
    }
}
