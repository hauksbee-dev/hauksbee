//! Output formats: a human-readable terminal report, JUnit XML for any CI
//! system to ingest, and GitHub Actions workflow-command annotations.

use std::time::Duration;

use crate::assertions::AssertResult;

/// The full result of a CI run: the spec name, per-assertion results, the seed
/// count, and timing.
#[derive(Debug)]
pub struct CiResult {
    pub spec_name: String,
    pub board: String,
    pub results: Vec<AssertResult>,
    pub seeds: u32,
    pub elapsed: Duration,
}

impl CiResult {
    /// True if every assertion passed.
    pub fn passed(&self) -> bool {
        self.results.iter().all(|r| r.passed)
    }

    pub fn pass_count(&self) -> usize {
        self.results.iter().filter(|r| r.passed).count()
    }

    /// Process exit code: 0 all green, 1 any red.
    pub fn exit_code(&self) -> i32 {
        if self.passed() {
            0
        } else {
            1
        }
    }

    /// Human-readable terminal report.
    pub fn render_human(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "galvani-ci: {}\n  board: {}\n  seeds: {}\n\n",
            self.spec_name, self.board, self.seeds
        ));
        for r in &self.results {
            let mark = if r.passed { "PASS" } else { "FAIL" };
            out.push_str(&format!("  [{mark}] {}\n        {}\n", r.label, r.detail));
        }
        let total = self.results.len();
        let passed = self.pass_count();
        out.push_str(&format!(
            "\n{}/{} assertions passed in {:.2}s — {}\n",
            passed,
            total,
            self.elapsed.as_secs_f64(),
            if self.passed() { "GREEN" } else { "RED" }
        ));
        out
    }

    /// JUnit XML: each assertion is a `<testcase>`, failures carry a
    /// `<failure>` with the detail. Any CI (GitLab, Jenkins, GitHub, Buildkite)
    /// ingests this.
    pub fn render_junit(&self) -> String {
        let failures = self.results.iter().filter(|r| !r.passed).count();
        let mut out = String::new();
        out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        out.push_str(&format!(
            "<testsuites name=\"galvani-ci\" tests=\"{}\" failures=\"{}\" time=\"{:.3}\">\n",
            self.results.len(),
            failures,
            self.elapsed.as_secs_f64()
        ));
        out.push_str(&format!(
            "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" time=\"{:.3}\">\n",
            xml_escape(&self.spec_name),
            self.results.len(),
            failures,
            self.elapsed.as_secs_f64()
        ));
        for r in &self.results {
            out.push_str(&format!(
                "    <testcase classname=\"{}\" name=\"{}\">\n",
                xml_escape(&r.kind),
                xml_escape(&r.label)
            ));
            if !r.passed {
                out.push_str(&format!(
                    "      <failure message=\"{}\">{}</failure>\n",
                    xml_escape(&r.detail),
                    xml_escape(&r.detail)
                ));
            } else {
                out.push_str(&format!(
                    "      <system-out>{}</system-out>\n",
                    xml_escape(&r.detail)
                ));
            }
            out.push_str("    </testcase>\n");
        }
        out.push_str("  </testsuite>\n");
        out.push_str("</testsuites>\n");
        out
    }

    /// GitHub Actions annotations: `::error` / `::notice` workflow commands so
    /// failures surface inline in the Checks UI. Emitted to stdout when
    /// `GITHUB_ACTIONS` is set.
    pub fn render_github_annotations(&self) -> String {
        let mut out = String::new();
        for r in &self.results {
            if r.passed {
                out.push_str(&format!(
                    "::notice title=galvani-ci PASS::{} — {}\n",
                    gh_escape(&r.label),
                    gh_escape(&r.detail)
                ));
            } else {
                out.push_str(&format!(
                    "::error title=galvani-ci FAIL::{} — {}\n",
                    gh_escape(&r.label),
                    gh_escape(&r.detail)
                ));
            }
        }
        // A summary line.
        if self.passed() {
            out.push_str(&format!(
                "::notice title=galvani-ci::{}/{} assertions passed\n",
                self.pass_count(),
                self.results.len()
            ));
        } else {
            out.push_str(&format!(
                "::error title=galvani-ci::{}/{} assertions passed — hardware check RED\n",
                self.pass_count(),
                self.results.len()
            ));
        }
        out
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Escape the characters that are special in GitHub workflow-command data.
fn gh_escape(s: &str) -> String {
    s.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}
