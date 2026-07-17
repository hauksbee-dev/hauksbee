//! Rendering the outcome of a CI run into the formats a pipeline consumes: a
//! human-readable terminal report, JUnit XML for any CI system to ingest, and
//! GitHub Actions workflow-command annotations. [`CiResult`] also owns the process
//! exit code and the honest ensemble-coverage wording, so a green run never
//! over-claims worst-case proof and a diverged co-sim refuses rather than
//! reporting a fake verdict.

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
    /// Present when the run was a component-tolerance ensemble: what kind of
    /// coverage the members provide. Drives the honest headline wording
    /// (sampled coverage vs monotonic-only bounds), so a green ensemble can
    /// never read as a worst-case proof.
    pub coverage: Option<EnsembleCoverage>,
    /// One message per MCU co-simulated on a SUBSTITUTE core (requested part not
    /// modelled, run on a less-specific one). Surfaced in every report format so a
    /// GREEN verdict never silently vouches for firmware on the wrong silicon.
    pub substitutions: Vec<String>,
}

/// What a tolerance-ensemble run covered, for the report headline.
#[derive(Debug, Clone)]
pub enum EnsembleCoverage {
    /// Random sampling: statistical evidence over the tolerance space. `seeds`
    /// is the number of genuinely SAMPLED seeds, excluding the nominal baseline
    /// (member 0, which draws no random sample).
    MonteCarlo { seeds: u32, components: usize },
    /// Deterministic all-min/all-max enumeration.
    Corners { corners: u32, components: usize },
    /// A single pinned ensemble member (`--seed N`): the runner filtered the
    /// ensemble down to exactly this one, so the nominal-baseline / sampled-count
    /// arithmetic doesn't apply — report the member honestly instead. `corners`
    /// distinguishes a deterministic corner (corners mode) from a random draw
    /// (Monte-Carlo), so the banner matches the mode-aware per-assertion wording.
    SingleMember {
        seed: u32,
        components: usize,
        corners: bool,
    },
}

impl EnsembleCoverage {
    /// The one-line coverage claim, worded so it cannot over-claim: Monte-Carlo
    /// is sampled coverage (never proof); corners bound only monotonic
    /// responses.
    pub fn describe(&self) -> String {
        match self {
            EnsembleCoverage::MonteCarlo { seeds, components } => format!(
                "tolerance ensemble: nominal baseline + {seeds} sampled seed(s) over \
                 {components} toleranced component(s) — statistical coverage, not \
                 worst-case proof"
            ),
            EnsembleCoverage::Corners { corners, components } => format!(
                "tolerance corners: {corners} deterministic min/max corner(s) over \
                 {components} component(s) — bounds the worst case only where the \
                 response is monotonic in each value"
            ),
            EnsembleCoverage::SingleMember { seed, components, corners: true } => format!(
                "single corner: corner {seed} over {components} toleranced \
                 component(s) — one pinned deterministic corner, not full corner coverage"
            ),
            EnsembleCoverage::SingleMember { seed, components, corners: false } => format!(
                "single ensemble member: seed {seed} over {components} toleranced \
                 component(s) — one pinned draw, not ensemble coverage"
            ),
        }
    }
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
            "hauksbee-ci: {}\n  board: {}\n  seeds: {}\n",
            self.spec_name, self.board, self.seeds
        ));
        if let Some(cov) = &self.coverage {
            out.push_str(&format!("  {}\n", cov.describe()));
        }
        out.push('\n');
        for r in &self.results {
            let mark = if r.invalid {
                "INVALID"
            } else if r.passed {
                "PASS"
            } else {
                "FAIL"
            };
            out.push_str(&format!("  [{mark}] {}\n        {}\n", r.label, r.detail));
            // On a real red (not a pass, not an INVALID refusal), add one
            // actionable "why / where to look" line keyed off the assertion kind.
            // Deliberately one line — a pointer at the likely cause and the doc
            // section, not a plain-language engine.
            if !r.passed && !r.invalid {
                if let Some(hint) = failure_hint(&r.kind) {
                    out.push_str(&format!("        why: {hint}\n"));
                }
            }
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
        // Substitution honesty: a GREEN over a substitute MCU core cannot vouch
        // for firmware behaviour on the real silicon. Say so plainly.
        for msg in &self.substitutions {
            out.push_str(&format!("  co-sim ran on a SUBSTITUTE chip — {msg}\n"));
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
        //
        // An analog abort that no assertion happened to cover (05 §3b) still
        // forces exit 3, so it must surface here too: `render_human` and
        // `render_github_annotations` both special-case that state, and a JUnit
        // that said `failures="0" errors="0"` would be a false ALL-GREEN on the
        // one surface most CI dashboards actually read. Emit one synthetic
        // errored testcase (the same `<error>` shape `render_junit_error` uses)
        // and count it in tests/errors.
        let synthetic_abort = self.analog_abort && self.invalid_count() == 0;
        let errors = self.invalid_count() + usize::from(synthetic_abort);
        let tests = self.results.len() + usize::from(synthetic_abort);
        let failures = self.results.iter().filter(|r| !r.passed && !r.invalid).count();
        let mut out = String::new();
        out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        out.push_str(&format!(
            "<testsuites name=\"hauksbee-ci\" tests=\"{}\" failures=\"{}\" errors=\"{}\" time=\"{:.3}\">\n",
            tests,
            failures,
            errors,
            self.elapsed.as_secs_f64()
        ));
        out.push_str(&format!(
            "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" errors=\"{}\" time=\"{:.3}\">\n",
            xml_escape(&self.spec_name),
            tests,
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
        if synthetic_abort {
            let msg = "analog co-sim aborted (solve failed on too many chunks in a row); \
                       the run is INVALID for analysis (05 §3b)";
            out.push_str("    <testcase classname=\"analog\" name=\"analog co-sim converged\">\n");
            out.push_str(&format!(
                "      <error message=\"{}\">{}</error>\n",
                xml_escape(msg),
                xml_escape(msg)
            ));
            out.push_str("    </testcase>\n");
        }
        // Substitution honesty as a suite-level system-out note (dashboards show
        // it alongside the results): a pass over a substitute core is qualified.
        for msg in &self.substitutions {
            out.push_str(&format!(
                "    <system-out>co-sim ran on a SUBSTITUTE chip — {}</system-out>\n",
                xml_escape(msg)
            ));
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
        // Substitution honesty: a warning annotation (a pass over a substitute
        // core cannot vouch for firmware on the real silicon).
        for msg in &self.substitutions {
            out.push_str(&format!(
                "::warning title=hauksbee-ci SUBSTITUTE MCU::{}\n",
                gh_escape(msg)
            ));
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

/// A synthetic JUnit document for a spec/board error (exit 2), so a CI that only
/// reads the JUnit/Checks tab still sees *something* — a single errored testcase
/// carrying the error message — instead of an empty report. Reuses the same
/// `<error>` shape the per-assertion INVALID path emits, so downstream ingestors
/// (GitLab, Jenkins, GitHub) render it as an errored test, distinct from a
/// failure. `message` is the spec/board error text.
pub fn render_junit_error(message: &str) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(
        "<testsuites name=\"hauksbee-ci\" tests=\"1\" failures=\"0\" errors=\"1\" time=\"0.000\">\n",
    );
    out.push_str(
        "  <testsuite name=\"spec error\" tests=\"1\" failures=\"0\" errors=\"1\" time=\"0.000\">\n",
    );
    out.push_str("    <testcase classname=\"spec\" name=\"spec/board loads\">\n");
    out.push_str(&format!(
        "      <error message=\"{}\">{}</error>\n",
        xml_escape(message),
        xml_escape(message)
    ));
    out.push_str("    </testcase>\n");
    out.push_str("  </testsuite>\n");
    out.push_str("</testsuites>\n");
    out
}

/// One actionable "why / where to look" line for a failed assertion of the
/// given `kind`. Points at the likely physical cause and the docs/CI.md section
/// to read — one line, per kind, not a full explanation engine. `None` for kinds
/// that have no useful generic pointer.
fn failure_hint(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "voltage" => "the rail left its window — check the supply feeding this net \
            and the load pulling it down (docs/CI.md, \"voltage\").",
        "rail_window" => "the rail dipped/recovered outside the window — check the \
            scenario's load step and the decoupling on this net (docs/CI.md, \
            \"rail_window\").",
        "boot-coverage" => "the firmware never drove the control net in time — check \
            `firmware = ...` points at the right image and the net is a GPIO the \
            firmware actually drives (docs/CI.md, \"boot-coverage\" caveat).",
        "no_faults" => "the stress monitor tripped — the named component exceeded a \
            rating (over-current / -voltage / -power / -temp / reverse-bias); check \
            its part value and supply (docs/CI.md, \"no_faults\").",
        "max_current" => "the part drew more than its limit — check its load and the \
            override value (docs/CI.md, \"max_current\").",
        "max_temp" => "the junction ran hotter than the limit — check dissipation and \
            the `ambient_c` assumption (docs/CI.md, \"max_temp\").",
        "uart" => "the expected UART text never appeared — check the firmware image \
            and baud, and that the MCU booted (docs/CI.md, \"uart\").",
        "toggle" => "the net toggled the wrong number of times — check the firmware's \
            drive rate and the deadline window (docs/CI.md, \"toggle\").",
        "protection_trip" => "the supply's protection did/did not latch as asserted — \
            check the supply's limits and the load that triggers it (docs/CI.md, \
            \"protection_trip\").",
        "phase_margin" => "the loop's phase margin missed the bound — check the \
            compensation network and the `[ac]` sweep range (docs/CI.md, \
            \"phase_margin\").",
        "ac_gain" => "the small-signal gain missed the band — check the AC stimulus \
            net and the sweep points (docs/CI.md, \"ac_gain\").",
        _ => return None,
    })
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

#[cfg(test)]
mod ensemble_coverage_tests {
    use super::EnsembleCoverage;

    #[test]
    fn monte_carlo_coverage_excludes_the_nominal_from_the_sampled_count() {
        // `seeds` is the SAMPLED count (nominal baseline excluded). A one-member
        // Monte-Carlo ran only the nominal → 0 sampled seeds, and the wording
        // must say so rather than claim it sampled one.
        let one = EnsembleCoverage::MonteCarlo { seeds: 0, components: 3 }.describe();
        assert!(one.contains("nominal baseline + 0 sampled seed(s)"), "{one}");
        let many = EnsembleCoverage::MonteCarlo { seeds: 31, components: 3 }.describe();
        assert!(many.contains("nominal baseline + 31 sampled seed(s)"), "{many}");
    }

    #[test]
    fn single_member_names_the_seed_and_does_not_claim_ensemble_coverage() {
        // R12: `--seed N` runs exactly one member; the banner must name it and
        // NOT report "nominal baseline + 0 sampled" (the nominal didn't run) or
        // "1 corner" (over-claiming one of 2^n corners).
        let d =
            EnsembleCoverage::SingleMember { seed: 7, components: 3, corners: false }.describe();
        assert!(d.contains("seed 7"), "{d}");
        assert!(!d.contains("nominal baseline"), "must not claim the nominal ran: {d}");
        assert!(!d.contains("corner"), "must not claim corner coverage: {d}");
    }

    #[test]
    fn single_member_in_corners_mode_names_a_corner_not_a_seed() {
        // R41: a pinned `--seed N` in CORNERS mode is a deterministic corner, not
        // a random draw. The banner must say "corner N" (matching the mode-aware
        // per-assertion FAIL/INVALID wording), never "seed"/"draw".
        let d =
            EnsembleCoverage::SingleMember { seed: 2, components: 2, corners: true }.describe();
        assert!(d.contains("corner 2"), "corners member must be named a corner: {d}");
        assert!(!d.contains("seed"), "a corner must not be called a seed: {d}");
        assert!(!d.contains("draw"), "a corner is deterministic, not a draw: {d}");
    }
}

#[cfg(test)]
mod substitution_tests {
    use super::CiResult;
    use std::time::Duration;

    // U2: the `run` binary surfaces an MCU substitution on every honesty surface;
    // the CI report did not, so a GREEN vouched for firmware on the wrong silicon.
    // It must now appear in the human, JUnit, and GitHub outputs.
    #[test]
    fn a_substituted_mcu_is_surfaced_in_every_report_format() {
        let result = CiResult {
            spec_name: "t".into(),
            board: "b.kicad_pcb".into(),
            results: Vec::new(),
            seeds: 1,
            elapsed: Duration::from_secs(0),
            analog_abort: false,
            coverage: None,
            substitutions: vec![
                "co-sim: U1 requested STM32F411RET6 but it is modelled as an STM32F407 core"
                    .to_string(),
            ],
        };
        let human = result.render_human();
        assert!(
            human.contains("SUBSTITUTE chip") && human.contains("STM32F411RET6"),
            "human report must name the substitution: {human}"
        );
        let junit = result.render_junit();
        assert!(
            junit.contains("SUBSTITUTE chip") && junit.contains("system-out"),
            "junit must carry the substitution note: {junit}"
        );
        let gh = result.render_github_annotations();
        assert!(
            gh.contains("SUBSTITUTE MCU") && gh.contains("::warning"),
            "github annotations must warn on the substitution: {gh}"
        );
        // No substitution → none of the surfaces mention it.
        let clean = CiResult { substitutions: Vec::new(), ..result };
        assert!(!clean.render_human().contains("SUBSTITUTE"));
    }
}
