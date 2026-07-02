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
    /// True if any seed's analog co-sim tripped the consecutive-failed-chunk
    /// abort (05 §3b). Forces exit 3 on its own, even when no single assertion's
    /// window overlapped a failed span (e.g. a spec with only a UART assertion
    /// over a run whose analog side collapsed).
    pub analog_abort: bool,
}

impl CiResult {
    /// True if every assertion passed. An INVALID assertion has `passed == false`,
    /// so it is not counted as passed here.
    pub fn passed(&self) -> bool {
        self.results.iter().all(|r| r.passed)
    }

    pub fn pass_count(&self) -> usize {
        self.results.iter().filter(|r| r.passed).count()
    }

    /// Count of assertions that could not be honestly evaluated (their window
    /// overlapped a failed analog chunk, 05 §3b).
    pub fn invalid_count(&self) -> usize {
        self.results.iter().filter(|r| r.invalid).count()
    }

    /// True when the run is invalid-for-analysis: at least one assertion is
    /// INVALID, or the analog co-sim tripped the strict abort.
    pub fn analog_invalid(&self) -> bool {
        self.analog_abort || self.results.iter().any(|r| r.invalid)
    }

    /// Process exit code: 3 invalid-for-analysis (any INVALID assertion or a
    /// tripped analog abort, 05 §3b), else 1 any ordinary red, else 0 all green.
    /// The invalid path is checked first so a diverged co-sim refuses rather than
    /// reports a fake pass/fail.
    pub fn exit_code(&self) -> i32 {
        if self.analog_invalid() {
            hauksbee_engine::result::EXIT_INVALID_FOR_ANALYSIS
        } else if self.passed() {
            0
        } else {
            1
        }
    }

    /// Human-readable terminal report.
    pub fn render_human(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "hauksbee-ci: {}\n  board: {}\n  seeds: {}\n\n",
            self.spec_name, self.board, self.seeds
        ));
        for r in &self.results {
            let mark = if r.invalid {
                "INVALID"
            } else if r.passed {
                "PASS"
            } else {
                "FAIL"
            };
            out.push_str(&format!("  [{mark}] {}\n        {}\n", r.label, r.detail));
        }
        let total = self.results.len();
        let passed = self.pass_count();
        // An analog abort that no assertion happened to cover still forces a
        // refusal, so surface it plainly rather than printing a misleading GREEN.
        if self.analog_abort && self.invalid_count() == 0 {
            out.push_str(
                "  analog co-sim aborted (solve failed on too many chunks in a row); \
                 the run is INVALID for analysis (05 §3b)\n",
            );
        }
        let verdict = if self.analog_invalid() {
            "INVALID (analog co-sim did not converge)"
        } else if self.passed() {
            "GREEN"
        } else {
            "RED"
        };
        out.push_str(&format!(
            "\n{}/{} assertions passed in {:.2}s - {}\n",
            passed,
            total,
            self.elapsed.as_secs_f64(),
            verdict
        ));
        out
    }

    /// JUnit XML: each assertion is a `<testcase>`, failures carry a
    /// `<failure>` with the detail. Any CI (GitLab, Jenkins, GitHub, Buildkite)
    /// ingests this.
    pub fn render_junit(&self) -> String {
        // INVALID assertions map to JUnit `<error>` (the test could not run to a
        // verdict), ordinary reds to `<failure>`. Keep the two counts distinct so
        // a CI dashboard shows "errored" apart from "failed".
        let errors = self.invalid_count();
        let failures = self.results.iter().filter(|r| !r.passed && !r.invalid).count();
        let mut out = String::new();
        out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        out.push_str(&format!(
            "<testsuites name=\"hauksbee-ci\" tests=\"{}\" failures=\"{}\" errors=\"{}\" time=\"{:.3}\">\n",
            self.results.len(),
            failures,
            errors,
            self.elapsed.as_secs_f64()
        ));
        out.push_str(&format!(
            "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" errors=\"{}\" time=\"{:.3}\">\n",
            xml_escape(&self.spec_name),
            self.results.len(),
            failures,
            errors,
            self.elapsed.as_secs_f64()
        ));
        for r in &self.results {
            out.push_str(&format!(
                "    <testcase classname=\"{}\" name=\"{}\">\n",
                xml_escape(&r.kind),
                xml_escape(&r.label)
            ));
            if r.invalid {
                out.push_str(&format!(
                    "      <error message=\"{}\">{}</error>\n",
                    xml_escape(&r.detail),
                    xml_escape(&r.detail)
                ));
            } else if !r.passed {
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
            if r.invalid {
                out.push_str(&format!(
                    "::error title=hauksbee-ci INVALID::{} - {}\n",
                    gh_escape(&r.label),
                    gh_escape(&r.detail)
                ));
            } else if r.passed {
                out.push_str(&format!(
                    "::notice title=hauksbee-ci PASS::{} - {}\n",
                    gh_escape(&r.label),
                    gh_escape(&r.detail)
                ));
            } else {
                out.push_str(&format!(
                    "::error title=hauksbee-ci FAIL::{} - {}\n",
                    gh_escape(&r.label),
                    gh_escape(&r.detail)
                ));
            }
        }
        // A summary line.
        if self.analog_invalid() {
            out.push_str(&format!(
                "::error title=hauksbee-ci::analog co-sim did not converge - {} assertion(s) INVALID, run is invalid for analysis (05 §3b)\n",
                self.invalid_count()
            ));
        } else if self.passed() {
            out.push_str(&format!(
                "::notice title=hauksbee-ci::{}/{} assertions passed\n",
                self.pass_count(),
                self.results.len()
            ));
        } else {
            out.push_str(&format!(
                "::error title=hauksbee-ci::{}/{} assertions passed - hardware check RED\n",
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
