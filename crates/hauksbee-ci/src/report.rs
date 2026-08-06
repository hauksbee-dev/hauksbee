//! Rendering the outcome of a CI run into the formats a pipeline consumes: a
//! human-readable terminal report, JUnit XML for any CI system to ingest, and
//! GitHub Actions workflow-command annotations. [`CiResult`] also owns the process
//! exit code and the honest ensemble-coverage wording, so a green run never
//! over-claims worst-case proof and a diverged co-sim refuses rather than
//! reporting a fake verdict.

use std::time::Duration;

use crate::assertions::AssertResult;

/// Published JSON shape for one evaluated assertion. Kept separate from the
/// runtime [`AssertResult`] because subject nets/refs are internal waiver keys,
/// while the causal evidence map is part of the external contract.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CiJsonAssertion {
    pub label: String,
    pub kind: String,
    pub passed: bool,
    pub invalid: bool,
    pub detail: String,
    pub failing_seed: Option<u32>,
    pub failing_seeds: Vec<u32>,
    pub seeds_total: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waived: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<hauksbee_ir::evidence::EvidenceMap>,
}

/// The stable `hauksbee-ci run --json` document. This type, rather than an
/// untyped `json!` tree, generates the checked-in schema drift test.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CiJsonReport {
    pub ok: bool,
    pub spec_name: String,
    pub board: String,
    pub passed: bool,
    pub assertions_passed: bool,
    pub run_valid: bool,
    pub exit_code: i32,
    pub analog_abort: bool,
    pub seeds: u32,
    pub elapsed_s: f64,
    pub coverage: Option<String>,
    pub substitutions: Vec<String>,
    pub coverage_warnings: Vec<String>,
    pub dead_rails: Vec<String>,
    pub waiver_notes: Vec<String>,
    pub inventory: Vec<hauksbee_ir::evidence::ArtifactProvenance>,
    pub assumptions: Vec<hauksbee_ir::evidence::Assumption>,
    pub evidence: Vec<hauksbee_ir::evidence::EvidenceMap>,
    pub results: Vec<CiJsonAssertion>,
}

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
    /// Co-sim coverage warnings (U3): dropped ADC injections (the firmware never
    /// received the solved voltage) and never-exercised bus peripherals (no
    /// matching controller modeled). Surfaced in every report format, exactly
    /// like `substitutions`, a GREEN over an un-run co-sim path must be
    /// qualified everywhere a pipeline reads.
    pub coverage_warnings: Vec<String>,
    /// Nets that name themselves a supply, carry a rail's worth of parts, and
    /// nothing powers (see `runner::dead_rails`). Surfaced FIRST in every
    /// format, ahead of the assertion list: with a rail dead the operating
    /// point is fiction, so this changes how every result below it reads, and a
    /// reader who meets it after a named component fault has already believed
    /// the fault.
    pub dead_rails: Vec<String>,
    /// Waiver-file housekeeping notes (a lapsed waiver whose finding gates
    /// again, an active waiver that matched nothing, a malformed file that was
    /// ignored). Surfaced on every format like `coverage_warnings`: a board
    /// carrying waivers has to look like one.
    pub waiver_notes: Vec<String>,
    /// Exact files consumed by this run, shared with engine/web reports.
    pub inventory: Vec<hauksbee_ir::evidence::ArtifactProvenance>,
    /// The one typed assumption registry every CI renderer projects.
    pub assumptions: Vec<hauksbee_ir::evidence::Assumption>,
    /// One causal evidence map per evaluated assertion.
    pub evidence: Vec<hauksbee_ir::evidence::EvidenceMap>,
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
    /// arithmetic doesn't apply, report the member honestly instead. `corners`
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
                 {components} toleranced component(s): statistical coverage, not \
                 worst-case proof"
            ),
            EnsembleCoverage::Corners {
                corners,
                components,
            } => format!(
                "tolerance corners: {corners} deterministic min/max corner(s) over \
                 {components} component(s): bounds the worst case only where the \
                 response is monotonic in each value"
            ),
            EnsembleCoverage::SingleMember {
                seed,
                components,
                corners: true,
            } => format!(
                "single corner: corner {seed} over {components} toleranced \
                 component(s): one pinned deterministic corner, not full corner coverage"
            ),
            EnsembleCoverage::SingleMember {
                seed,
                components,
                corners: false,
            } => format!(
                "single ensemble member: seed {seed} over {components} toleranced \
                 component(s): one pinned draw, not ensemble coverage"
            ),
        }
    }
}

impl CiResult {
    /// True if every assertion passed or was waived. An INVALID assertion has
    /// `passed == false` and can never be waived, so it is not counted here.
    /// A waived failure is visible-but-not-gating (the whole point of a
    /// waiver): it stays a FAIL on every surface but does not turn the build
    /// red.
    pub fn passed(&self) -> bool {
        self.results.iter().all(|r| r.passed || r.waived.is_some())
    }

    /// Results that gate the build red: real failures no active waiver covers.
    fn gating_failures(&self) -> impl Iterator<Item = &AssertResult> {
        self.results
            .iter()
            .filter(|r| !r.passed && !r.invalid && r.waived.is_none())
    }

    /// Count of failures an active waiver covers (visible, not gating).
    pub fn waived_count(&self) -> usize {
        self.results.iter().filter(|r| r.waived.is_some()).count()
    }

    /// Machine-readable run result (the `--json` surface, and what the web
    /// checks panel consumes via `/api/check`). One stable JSON object: the
    /// overall verdict, the per-assertion results verbatim, and every honesty
    /// qualifier the human report carries (analog abort, substitutions,
    /// coverage wording), a consumer must never see a cleaner story than the
    /// terminal does.
    pub fn render_json(&self) -> String {
        let results = self
            .results
            .iter()
            .map(|result| CiJsonAssertion {
                label: result.label.clone(),
                kind: result.kind.clone(),
                passed: result.passed,
                invalid: result.invalid,
                detail: result.detail.clone(),
                failing_seed: result.failing_seed,
                failing_seeds: result.failing_seeds.clone(),
                seeds_total: result.seeds_total,
                why: result.why.clone(),
                waived: result.waived.clone(),
                evidence: self.evidence_for(&result.label).cloned(),
            })
            .collect();
        let value = CiJsonReport {
            ok: true,
            spec_name: self.spec_name.clone(),
            board: self.board.clone(),
            // The OVERALL verdict is the process verdict: green only when the
            // run was valid AND every assertion held. `passed()` alone ignores
            // an analog abort (exit 3, every assertion left false-but-not-failed),
            // which would render a green "all passed" over an untrustworthy run.
            passed: self.exit_code() == 0,
            // The two components, so a consumer can tell "an assertion failed"
            // from "the run itself was not trustworthy".
            assertions_passed: self.passed(),
            run_valid: !self.analog_invalid(),
            exit_code: self.exit_code(),
            analog_abort: self.analog_abort,
            seeds: self.seeds,
            elapsed_s: self.elapsed.as_secs_f64(),
            coverage: self.coverage.as_ref().map(EnsembleCoverage::describe),
            substitutions: self.substitutions.clone(),
            coverage_warnings: self.coverage_warnings.clone(),
            dead_rails: self.dead_rails.clone(),
            waiver_notes: self.waiver_notes.clone(),
            inventory: self.inventory.clone(),
            assumptions: self.assumptions.clone(),
            evidence: self.evidence.clone(),
            results,
        };
        serde_json::to_string(&value).unwrap_or_else(|e| {
            format!("{{\"ok\":false,\"error\":\"could not serialize the run result: {e}\"}}")
        })
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

    fn evidence_for(&self, label: &str) -> Option<&hauksbee_ir::evidence::EvidenceMap> {
        self.evidence.iter().find(|map| map.assertion() == label)
    }

    fn evidence_text(&self, label: &str) -> String {
        let Some(map) = self.evidence_for(label) else {
            return String::new();
        };
        let mut out = format!("evidence: {}", map.status());
        for id in map.assumptions() {
            if let Some(assumption) = self.assumptions.iter().find(|a| a.id() == id) {
                out.push_str(&format!(
                    "\n[{}] {} Why: {} Effect: {} Fix: {}",
                    id,
                    assumption.statement(),
                    assumption.because(),
                    assumption.consequence(),
                    assumption.replacement()
                ));
            }
        }
        if map.error_budget().is_some() {
            out.push_str("\nerror budget: attached in JSON");
        }
        out
    }

    /// The unpowered-rail warning, or empty when every rail is fed.
    ///
    /// Printed BEFORE the assertions, because it changes what they mean rather
    /// than adding to them. A rail sitting at 0 V because nobody could work out
    /// its voltage makes the operating point around it fiction, and the stress
    /// monitor will report on that fiction as confidently as on a real
    /// overload. Someone who reads "R_Shunt15301 overpower" first and the
    /// caveat afterwards has already believed the accusation.
    fn dead_rail_banner(&self) -> String {
        if self.dead_rails.is_empty() {
            return String::new();
        }
        let mut s = String::from("\n  UNPOWERED RAIL: ");
        s.push_str(&self.dead_rails.join(", "));
        s.push('\n');
        s.push_str(
            "        These nets name a supply but not a voltage, so nothing powered\n\
             \x20       them and they sat at 0 V. Every analog result below was solved\n\
             \x20       around that, so a fault it names may be an artifact rather than\n\
             \x20       a finding about your board. Add a [[supply]] for each, then run\n\
             \x20       again before acting on anything here.\n",
        );
        s
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
        out.push_str(&self.dead_rail_banner());
        out.push('\n');
        for r in &self.results {
            let mark = if r.invalid {
                "INVALID"
            } else if r.passed {
                "PASS"
            } else if r.waived.is_some() {
                "WAIVED"
            } else {
                "FAIL"
            };
            out.push_str(&format!("  [{mark}] {}\n        {}\n", r.label, r.detail));
            for line in self.evidence_text(&r.label).lines() {
                out.push_str(&format!("        {line}\n"));
            }
            // On a real red (not a pass, not an INVALID refusal), add one
            // actionable "why" line.
            if !r.passed && !r.invalid {
                if let Some(why) = why_line(r) {
                    out.push_str(&format!("        why: {why}\n"));
                }
                if let Some(w) = &r.waived {
                    out.push_str(&format!(
                        "        waived: {w}; visible here, not gating the build\n"
                    ));
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
            out.push_str(&format!("  co-sim ran on a SUBSTITUTE chip: {msg}\n"));
        }
        // Coverage honesty: a GREEN over a co-sim path that never ran (dropped
        // ADC injection, unexercised bus device) must be qualified plainly.
        for msg in &self.coverage_warnings {
            out.push_str(&format!("  co-sim COVERAGE HOLE: {msg}\n"));
        }
        // Waiver housekeeping: lapsed / stale / malformed waiver-file notes.
        for msg in &self.waiver_notes {
            out.push_str(&format!("  waivers: {msg}\n"));
        }
        let verdict = if self.analog_invalid() {
            "INVALID (run or assertion evidence is not trustworthy)"
        } else if self.passed() {
            "GREEN"
        } else {
            "RED"
        };
        let waived = self.waived_count();
        let waived_note = if waived > 0 {
            format!(" ({waived} failure(s) waived, visible above)")
        } else {
            String::new()
        };
        out.push_str(&format!(
            "\n{}/{} assertions passed in {:.2}s - {}{}\n",
            passed,
            total,
            self.elapsed.as_secs_f64(),
            verdict,
            waived_note
        ));
        // A RED report ends with where to read about the failing check: the
        // assertion catalog's section for the first gating failure's kind.
        if verdict == "RED" {
            if let Some(r) = self
                .results
                .iter()
                .find(|r| !r.passed && !r.invalid && r.waived.is_none())
            {
                out.push_str(&format!(
                    "next: the \"{}\" section of {} explains this check and its knobs\n",
                    r.kind,
                    hauksbee_ir::docs_url("docs/ci/CI.md")
                ));
            }
        }
        out
    }

    /// JUnit XML: each assertion is a `<testcase>`, failures carry a
    /// `<failure>` with the detail. Any CI (GitLab, Jenkins, GitHub, Buildkite)
    /// ingests this.
    pub fn render_junit(&self) -> String {
        render_junit_document(std::slice::from_ref(&self.junit_suite()))
    }

    /// This run as ONE `<testsuite>` fragment plus its counters, so a
    /// multi-spec invocation can merge several runs (and spec errors) into a
    /// single `<testsuites>` document with honest aggregate counts.
    pub fn junit_suite(&self) -> JunitSuite {
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
        // Waived failures are JUnit `<skipped>`: visible in every dashboard's
        // test list (with the waiver reason as the message) without counting
        // as a failure, which is exactly the visible-but-not-gating contract.
        let failures = self.gating_failures().count();
        let skipped = self.waived_count();
        let mut out = String::new();
        out.push_str(&format!(
            "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\" time=\"{:.3}\">\n",
            xml_escape(&self.spec_name),
            tests,
            failures,
            errors,
            skipped,
            self.elapsed.as_secs_f64()
        ));
        for r in &self.results {
            let evidence = self.evidence_text(&r.label);
            out.push_str(&format!(
                "    <testcase classname=\"{}\" name=\"{}\">\n",
                xml_escape(&r.kind),
                xml_escape(&r.label)
            ));
            if r.invalid {
                let body = if evidence.is_empty() {
                    r.detail.clone()
                } else {
                    format!("{}\n{evidence}", r.detail)
                };
                out.push_str(&format!(
                    "      <error message=\"{}\">{}</error>\n",
                    xml_escape(&r.detail),
                    xml_escape(&body)
                ));
            } else if let (false, Some(w)) = (r.passed, &r.waived) {
                out.push_str(&format!(
                    "      <skipped message=\"waived FAIL: {} ({})\"/>\n",
                    xml_escape(&r.detail),
                    xml_escape(w)
                ));
                if !evidence.is_empty() {
                    out.push_str(&format!(
                        "      <system-out>{}</system-out>\n",
                        xml_escape(&evidence)
                    ));
                }
            } else if !r.passed {
                // The body carries the `why:` line as well as the measured one.
                // Plenty of people only ever read the Tests tab of a CI run, and
                // the why is the actionable half: without it the failure says
                // what the number was and nothing about what to do next.
                let mut body = match why_line(r) {
                    Some(why) => format!("{}\nwhy: {why}", r.detail),
                    None => r.detail.clone(),
                };
                if !evidence.is_empty() {
                    body.push('\n');
                    body.push_str(&evidence);
                }
                out.push_str(&format!(
                    "      <failure message=\"{}\">{}</failure>\n",
                    xml_escape(&r.detail),
                    xml_escape(&body)
                ));
            } else {
                let body = if evidence.is_empty() {
                    r.detail.clone()
                } else {
                    format!("{}\n{evidence}", r.detail)
                };
                out.push_str(&format!(
                    "      <system-out>{}</system-out>\n",
                    xml_escape(&body)
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
        // A dead rail rides the same suite-level channel: a dashboard reader who
        // only ever sees the JUnit tab must not read a fault as a finding when
        // the operating point it came from had a rail at 0 V.
        if !self.dead_rails.is_empty() {
            out.push_str(&format!(
                "    <system-out>UNPOWERED RAIL: {} sat at 0 V; \
                 any analog result here may be an artifact</system-out>\n",
                xml_escape(&self.dead_rails.join(", "))
            ));
        }
        // Substitution honesty as a suite-level system-out note (dashboards show
        // it alongside the results): a pass over a substitute core is qualified.
        for msg in &self.substitutions {
            out.push_str(&format!(
                "    <system-out>co-sim ran on a SUBSTITUTE chip: {}</system-out>\n",
                xml_escape(msg)
            ));
        }
        // Coverage honesty: dropped-ADC / unexercised-bus warnings ride the same
        // suite-level channel so the JUnit surface carries the qualification too.
        for msg in &self.coverage_warnings {
            out.push_str(&format!(
                "    <system-out>co-sim COVERAGE HOLE: {}</system-out>\n",
                xml_escape(msg)
            ));
        }
        // Waiver housekeeping notes ride along so a dashboard-only reader sees
        // a lapsed or stale waiver too.
        for msg in &self.waiver_notes {
            out.push_str(&format!(
                "    <system-out>waivers: {}</system-out>\n",
                xml_escape(msg)
            ));
        }
        out.push_str("  </testsuite>\n");
        JunitSuite {
            xml: out,
            tests,
            failures,
            errors,
            time_s: self.elapsed.as_secs_f64(),
        }
    }

    /// GitHub Actions annotations: `::error` / `::warning` / `::notice`
    /// workflow commands so failures surface inline in the Checks UI. Emitted
    /// to stdout when `GITHUB_ACTIONS` is set.
    ///
    /// The budget matters: GitHub shows at most 10 annotations per type per
    /// step and silently drops the rest, so this surface spends them on
    /// verdicts only. Passing assertions get NO per-assertion `::notice` (the
    /// log and JUnit carry them; a 12-assertion green spec must not burn the
    /// whole notice budget), failures/INVALIDs get at most
    /// [`Self::MAX_ERROR_ANNOTATIONS`] `::error`s plus one overflow line and
    /// the rollup, and warnings are capped at
    /// [`Self::MAX_WARNING_ANNOTATIONS`] plus one overflow line.
    pub fn render_github_annotations(&self) -> String {
        let mut out = String::new();
        // Per-assertion verdict errors, capped to leave room for the overflow
        // line and the rollup inside GitHub's 10-per-type truncation.
        let bad: Vec<&AssertResult> = self
            .results
            .iter()
            .filter(|r| !r.passed && r.waived.is_none())
            .collect();
        for r in bad.iter().take(Self::MAX_ERROR_ANNOTATIONS) {
            let title = if r.invalid { "INVALID" } else { "FAIL" };
            out.push_str(&format!(
                "::error title=hauksbee-ci {title}::{} - {}\n",
                gh_escape(&r.label),
                gh_escape(&r.detail)
            ));
        }
        if bad.len() > Self::MAX_ERROR_ANNOTATIONS {
            out.push_str(&format!(
                "::error title=hauksbee-ci::...and {} more failing assertion(s); see the job log or the JUnit report for the full list\n",
                bad.len() - Self::MAX_ERROR_ANNOTATIONS
            ));
        }

        // Warnings: dead rails first (they change what every other line
        // means), then waived failures, substitutions, coverage holes and
        // waiver notes, all through one capped channel.
        let mut warnings: Vec<String> = Vec::new();
        if !self.dead_rails.is_empty() {
            warnings.push(format!(
                "UNPOWERED RAIL::{} sat at 0 V (no voltage could be read from the name and no [[supply]] fed it); any analog result above may be an artifact",
                gh_escape(&self.dead_rails.join(", "))
            ));
        }
        for r in self.results.iter().filter(|r| r.waived.is_some()) {
            warnings.push(format!(
                "WAIVED FAIL::{} - {} (waived: {})",
                gh_escape(&r.label),
                gh_escape(&r.detail),
                gh_escape(r.waived.as_deref().unwrap_or(""))
            ));
        }
        let mut surfaced_assumptions = std::collections::HashSet::new();
        for map in self
            .evidence
            .iter()
            .filter(|map| map.status() != hauksbee_ir::evidence::EvidenceStatus::Clean)
        {
            for id in map.assumptions() {
                if !surfaced_assumptions.insert(id.clone()) {
                    continue;
                }
                if let Some(assumption) = self.assumptions.iter().find(|a| a.id() == id) {
                    warnings.push(format!(
                        "ASSUMPTION {}::{} ({})",
                        gh_escape(&id.to_string()),
                        gh_escape(assumption.statement()),
                        gh_escape(assumption.replacement())
                    ));
                }
            }
        }
        for msg in &self.substitutions {
            warnings.push(format!("SUBSTITUTE MCU::{}", gh_escape(msg)));
        }
        for msg in &self.coverage_warnings {
            warnings.push(format!("COSIM COVERAGE HOLE::{}", gh_escape(msg)));
        }
        for msg in &self.waiver_notes {
            warnings.push(format!("WAIVERS::{}", gh_escape(msg)));
        }
        for w in warnings.iter().take(Self::MAX_WARNING_ANNOTATIONS) {
            out.push_str(&format!("::warning title=hauksbee-ci {w}\n"));
        }
        if warnings.len() > Self::MAX_WARNING_ANNOTATIONS {
            out.push_str(&format!(
                "::warning title=hauksbee-ci::...and {} more warning(s); see the job log for the full list\n",
                warnings.len() - Self::MAX_WARNING_ANNOTATIONS
            ));
        }

        // The rollup: exactly one summary annotation, always emitted.
        if self.analog_invalid() {
            out.push_str(&format!(
                "::error title=hauksbee-ci::{} assertion(s) INVALID - run evidence is not trustworthy\n",
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

    /// Per-assertion `::error` cap: 8 verdicts + 1 overflow + 1 rollup fits
    /// exactly inside GitHub's 10-errors-per-step truncation.
    pub const MAX_ERROR_ANNOTATIONS: usize = 8;
    /// `::warning` cap: 9 + 1 overflow fits the 10-warnings-per-step budget.
    pub const MAX_WARNING_ANNOTATIONS: usize = 9;
}

/// One rendered `<testsuite>` fragment plus the counters the `<testsuites>`
/// envelope aggregates. A multi-spec `hauksbee-ci run a.toml b.toml --junit`
/// merges one of these per spec (a spec that failed to LOAD contributes the
/// [`junit_error_suite`] shape) into a single document.
#[derive(Debug, Clone)]
pub struct JunitSuite {
    /// The `  <testsuite ...>...</testsuite>\n` fragment, indented for the
    /// envelope.
    pub xml: String,
    pub tests: usize,
    pub failures: usize,
    pub errors: usize,
    pub time_s: f64,
}

/// Wrap one or more suites in the `<testsuites>` envelope with honest
/// aggregate counts. Every JUnit document this crate emits goes through here,
/// so the single-spec and merged multi-spec shapes cannot drift.
pub fn render_junit_document(suites: &[JunitSuite]) -> String {
    let tests: usize = suites.iter().map(|s| s.tests).sum();
    let failures: usize = suites.iter().map(|s| s.failures).sum();
    let errors: usize = suites.iter().map(|s| s.errors).sum();
    let time: f64 = suites.iter().map(|s| s.time_s).sum();
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(&format!(
        "<testsuites name=\"hauksbee-ci\" tests=\"{tests}\" failures=\"{failures}\" errors=\"{errors}\" time=\"{time:.3}\">\n"
    ));
    for s in suites {
        out.push_str(&s.xml);
    }
    out.push_str("</testsuites>\n");
    out
}

/// The suite for a spec/board error (exit 2): a single errored testcase
/// carrying the message, so a CI that only reads the JUnit/Checks tab still
/// sees *something*. Reuses the `<error>` shape the per-assertion INVALID path
/// emits, so downstream ingestors (GitLab, Jenkins, GitHub) render it as an
/// errored test, distinct from a failure. `name` labels the suite (the spec
/// path in a multi-spec run, "spec error" for one).
pub fn junit_error_suite(name: &str, message: &str) -> JunitSuite {
    let mut out = String::new();
    out.push_str(&format!(
        "  <testsuite name=\"{}\" tests=\"1\" failures=\"0\" errors=\"1\" time=\"0.000\">\n",
        xml_escape(name)
    ));
    out.push_str("    <testcase classname=\"spec\" name=\"spec/board loads\">\n");
    out.push_str(&format!(
        "      <error message=\"{}\">{}</error>\n",
        xml_escape(message),
        xml_escape(message)
    ));
    out.push_str("    </testcase>\n");
    out.push_str("  </testsuite>\n");
    JunitSuite {
        xml: out,
        tests: 1,
        failures: 0,
        errors: 1,
        time_s: 0.0,
    }
}

/// A synthetic JUnit document for a single spec/board error (exit 2); the
/// one-spec convenience over [`junit_error_suite`] + [`render_junit_document`].
pub fn render_junit_error(message: &str) -> String {
    render_junit_document(std::slice::from_ref(&junit_error_suite(
        "spec error",
        message,
    )))
}

/// The one `why:` line a failed assertion gets, from the best source available:
///
/// 1. the shortfall the check MEASURED (`why`), always preferred,
/// 2. guidance specific to a degenerate outcome the detail describes
///    ([`degenerate_hint`]),
/// 3. the generic per-kind pointer at the likely physical cause
///    ([`failure_hint`]).
///
/// Every surface that prints a `why:` goes through here, so the terminal and the
/// JUnit body cannot end up saying different things about the same failure.
pub fn why_line(r: &AssertResult) -> Option<String> {
    r.why
        .clone()
        .or_else(|| degenerate_hint(&r.detail))
        .or_else(|| failure_hint(&r.kind))
}

/// Guidance for a failure whose measured line says the check never got data.
///
/// The per-kind hint below assumes the check RAN and disagreed with the board,
/// and on a degenerate outcome it contradicts the line above it: "net 'VCC' was
/// never sampled (no window at 500ms)" followed by "the rail left its window;
/// check the supply feeding this net" sends someone to the bench over a spec
/// whose sample window starts after the run ends. These variants get their own
/// line naming the knob that is wrong instead.
fn degenerate_hint(detail: &str) -> Option<String> {
    // boot_coverage: the whole boot window sat past the end of the run.
    if let (Some(deadline), Some(sim)) = (
        number_after(detail, "boot deadline "),
        number_after(detail, "past the end of the "),
    ) {
        return Some(format!(
            "the boot deadline ({deadline} ms) is past the end of the run ({sim} ms), so \
             the window was never observed; raise duration_ms above {deadline} or lower \
             deadline_ms below {sim}."
        ));
    }
    // voltage: no sample window exists at the assertion's threshold at all.
    if let Some(ms) = number_after(detail, "no window at ") {
        if !is_zero(&ms) {
            return Some(format!(
                "the sample window starts at {ms} ms, at or after the end of the run, so \
                 nothing was measured; raise duration_ms above {ms} or lower after_ms \
                 below it."
            ));
        }
    }
    // voltage: the window exists but no frame landed inside it.
    if let Some(ms) = number_after(detail, "had no samples after ") {
        return Some(format!(
            "no frame landed after {ms} ms, so the window is empty; raise duration_ms \
             above {ms}, lower after_ms, or shorten frame_ms so a frame falls inside it."
        ));
    }
    if detail.contains("was never sampled in scenario window") {
        return Some(
            "the scenario window produced no samples for this net; check the net name and \
             that the [[scenario]]'s `start_ms` falls inside duration_ms."
                .to_string(),
        );
    }
    if detail.contains("was never sampled") || detail.contains("had no samples") {
        return Some(
            "nothing was measured, so there is no board finding here yet; this is the spec \
             or the run's coverage, not the hardware."
                .to_string(),
        );
    }
    None
}

/// The number immediately following `prefix` in `text`, verbatim, so a hint can
/// quote the value the spec actually carries. `None` when the prefix is absent
/// or is not followed by a number.
fn number_after(text: &str, prefix: &str) -> Option<String> {
    let rest = text.split_once(prefix)?.1;
    let n: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if n.chars().any(|c| c.is_ascii_digit()) {
        Some(n)
    } else {
        None
    }
}

/// A threshold of zero means the window was never past the end of anything, so
/// "raise duration_ms above 0" would be nonsense advice; those fall through to
/// the generic no-data line.
fn is_zero(n: &str) -> bool {
    n.parse::<f64>().map(|v| v == 0.0).unwrap_or(false)
}

/// One actionable "why / where to look" line for a failed assertion of the
/// given `kind`. Points at the likely physical cause and the assertion-catalog
/// section to read, one line, per kind, not a full explanation engine. `None`
/// for kinds that have no useful generic pointer.
///
/// Only reached once [`degenerate_hint`] has ruled out the outcomes where the
/// check never measured anything: these lines all presume a real measurement.
fn failure_hint(kind: &str) -> Option<String> {
    let url = hauksbee_ir::docs_url("docs/ci/CI.md");
    Some(match kind {
        "voltage" => format!(
            "the rail left its window; check the supply feeding this net \
            and the load pulling it down ({url}, \"voltage\")."
        ),
        "rail_window" => format!(
            "the rail dipped/recovered outside the window; check the \
            scenario's load step and the decoupling on this net ({url}, \
            \"rail_window\")."
        ),
        "boot_coverage" | "boot-coverage" => format!(
            "the firmware never drove the control net in time; check \
            `firmware = ...` points at the right image and the net is a GPIO the \
            firmware actually drives ({url}, \"boot_coverage\" caveat)."
        ),
        "no_faults" => format!(
            "the stress monitor tripped; the named component exceeded a \
            rating (over-current / -voltage / -power / -temp / reverse-bias); check \
            its part value and supply ({url}, \"no_faults\")."
        ),
        "max_current" => format!(
            "the part drew more than its limit; check its load and the \
            override value ({url}, \"max_current\")."
        ),
        "max_temp" => format!(
            "the junction ran hotter than the limit; check dissipation and \
            the `ambient_c` assumption ({url}, \"max_temp\")."
        ),
        "uart" => format!(
            "the expected UART text never appeared; check the firmware image \
            and baud, and that the MCU booted ({url}, \"uart\")."
        ),
        "toggle" => format!(
            "the net toggled the wrong number of times; check the firmware's \
            drive rate and the deadline window ({url}, \"toggle\")."
        ),
        "protection_trip" => format!(
            "the supply's protection did/did not latch as asserted; \
            check the supply's limits and the load that triggers it ({url}, \
            \"protection_trip\")."
        ),
        "phase_margin" => format!(
            "the loop's phase margin missed the bound; check the \
            compensation network and the `[ac]` sweep range ({url}, \
            \"phase_margin\")."
        ),
        "ac_gain" => format!(
            "the small-signal gain missed the band; check the AC stimulus \
            net and the sweep points ({url}, \"ac_gain\")."
        ),
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
        let one = EnsembleCoverage::MonteCarlo {
            seeds: 0,
            components: 3,
        }
        .describe();
        assert!(
            one.contains("nominal baseline + 0 sampled seed(s)"),
            "{one}"
        );
        let many = EnsembleCoverage::MonteCarlo {
            seeds: 31,
            components: 3,
        }
        .describe();
        assert!(
            many.contains("nominal baseline + 31 sampled seed(s)"),
            "{many}"
        );
    }

    #[test]
    fn single_member_names_the_seed_and_does_not_claim_ensemble_coverage() {
        // R12: `--seed N` runs exactly one member; the banner must name it and
        // NOT report "nominal baseline + 0 sampled" (the nominal didn't run) or
        // "1 corner" (over-claiming one of 2^n corners).
        let d = EnsembleCoverage::SingleMember {
            seed: 7,
            components: 3,
            corners: false,
        }
        .describe();
        assert!(d.contains("seed 7"), "{d}");
        assert!(
            !d.contains("nominal baseline"),
            "must not claim the nominal ran: {d}"
        );
        assert!(!d.contains("corner"), "must not claim corner coverage: {d}");
    }

    #[test]
    fn single_member_in_corners_mode_names_a_corner_not_a_seed() {
        // R41: a pinned `--seed N` in CORNERS mode is a deterministic corner, not
        // a random draw. The banner must say "corner N" (matching the mode-aware
        // per-assertion FAIL/INVALID wording), never "seed"/"draw".
        let d = EnsembleCoverage::SingleMember {
            seed: 2,
            components: 2,
            corners: true,
        }
        .describe();
        assert!(
            d.contains("corner 2"),
            "corners member must be named a corner: {d}"
        );
        assert!(
            !d.contains("seed"),
            "a corner must not be called a seed: {d}"
        );
        assert!(
            !d.contains("draw"),
            "a corner is deterministic, not a draw: {d}"
        );
    }
}

#[cfg(test)]
mod why_line_tests {
    use super::*;
    use crate::assertions::AssertResult;
    use std::time::Duration;

    fn failing(kind: &str, detail: &str) -> AssertResult {
        AssertResult {
            label: format!("{kind} check"),
            kind: kind.to_string(),
            passed: false,
            invalid: false,
            detail: detail.to_string(),
            failing_seed: None,
            failing_seeds: Vec::new(),
            seeds_total: 1,
            why: None,
            waived: None,
            subject_nets: Vec::new(),
            subject_refs: Vec::new(),
        }
    }

    fn result_of(results: Vec<AssertResult>) -> CiResult {
        CiResult {
            spec_name: "t".into(),
            board: "b.kicad_pcb".into(),
            results,
            seeds: 1,
            elapsed: Duration::from_secs(0),
            analog_abort: false,
            coverage: None,
            substitutions: Vec::new(),
            coverage_warnings: Vec::new(),
            dead_rails: Vec::new(),
            waiver_notes: Vec::new(),
            inventory: Vec::new(),
            assumptions: Vec::new(),
            evidence: Vec::new(),
        }
    }

    // M1: the canned per-kind hint contradicted the measured line whenever the
    // check never got data. "never sampled (no window at 500ms)" told the user
    // to go look at the supply and the load; the spec's window simply starts
    // after the run ends.
    #[test]
    fn a_never_sampled_voltage_names_the_window_not_the_supply() {
        let r = failing(
            "voltage",
            "net 'VCC' was never sampled (no window at 500ms)",
        );
        let why = why_line(&r).expect("a degenerate failure still gets a why");
        assert!(why.contains("duration_ms"), "{why}");
        assert!(why.contains("after_ms"), "{why}");
        assert!(why.contains("500"), "it must quote the value: {why}");
        assert!(
            !why.contains("check the supply"),
            "the misleading board-cause hint must be gone: {why}"
        );
        assert!(!why.contains("the load pulling it down"), "{why}");
    }

    #[test]
    fn a_boot_deadline_past_the_run_names_the_two_knobs() {
        let r = failing(
            "boot_coverage",
            "boot deadline 500 ms is past the end of the 200.00 ms simulation, so boot \
             coverage for control net 'EN' cannot be confirmed; extend the run duration",
        );
        let why = why_line(&r).expect("a degenerate failure still gets a why");
        assert!(why.contains("500") && why.contains("200.00"), "{why}");
        assert!(
            why.contains("duration_ms") && why.contains("deadline_ms"),
            "{why}"
        );
        assert!(
            !why.contains("the firmware never drove"),
            "the firmware-cause hint is wrong here: {why}"
        );
    }

    #[test]
    fn a_real_measured_failure_still_gets_the_per_kind_hint() {
        // The generic hint is right whenever the check DID measure something,
        // so the fix must not have swallowed it.
        let r = failing("voltage", "+5V: min=3.100V < required 4.75V <- FAILED HERE");
        let why = why_line(&r).expect("a measured failure gets the per-kind hint");
        assert!(why.contains("check the supply feeding this net"), "{why}");
    }

    #[test]
    fn a_measured_why_outranks_every_hint() {
        let mut r = failing("voltage", "+5V: min=3.100V < required 4.75V");
        r.why = Some("+5V settled 1.650 V below your floor".to_string());
        assert_eq!(
            why_line(&r).as_deref(),
            Some("+5V settled 1.650 V below your floor")
        );
    }

    // M7: the JUnit `<failure>` body carried the measured line and dropped the
    // why, which is the half that says what to do. Plenty of readers only ever
    // open the Tests tab.
    #[test]
    fn the_junit_failure_body_carries_the_why_line() {
        let mut r = failing("voltage", "+5V: min=3.100V < required 4.75V");
        r.why = Some("+5V settled 1.650 V below your floor".to_string());
        let junit = result_of(vec![r]).render_junit();
        assert!(junit.contains("<failure"), "{junit}");
        assert!(
            junit.contains("why: +5V settled 1.650 V below your floor"),
            "the failure body must carry the why: {junit}"
        );
        // The message attribute stays the one-line measured detail.
        assert!(junit.contains("message=\"+5V: min=3.100V"), "{junit}");
    }

    #[test]
    fn the_junit_failure_body_and_the_terminal_agree() {
        let r = failing(
            "voltage",
            "net 'VCC' was never sampled (no window at 500ms)",
        );
        let result = result_of(vec![r]);
        let why = why_line(&result.results[0]).unwrap();
        assert!(result.render_human().contains(&format!("why: {why}")));
        assert!(result.render_junit().contains(&format!("why: {why}")));
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
            coverage_warnings: Vec::new(),
            dead_rails: Vec::new(),
            waiver_notes: Vec::new(),
            inventory: Vec::new(),
            assumptions: Vec::new(),
            evidence: Vec::new(),
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
        let clean = CiResult {
            substitutions: Vec::new(),
            ..result
        };
        assert!(!clean.render_human().contains("SUBSTITUTE"));
    }

    // U3: co-sim coverage holes (dropped ADC injection, unexercised bus device)
    // must reach every report format, exactly like substitutions do.
    #[test]
    fn a_cosim_coverage_hole_is_surfaced_in_every_report_format() {
        let result = CiResult {
            spec_name: "t".into(),
            board: "b.kicad_pcb".into(),
            results: Vec::new(),
            seeds: 1,
            elapsed: Duration::from_secs(0),
            analog_abort: false,
            coverage: None,
            substitutions: Vec::new(),
            coverage_warnings: vec![
                "co-sim: ADC channel 0 on U1 (net 'TEMP_SENSE') was driven by the \
                 analog solve but this platform has no ADC injection map"
                    .to_string(),
            ],
            dead_rails: Vec::new(),
            waiver_notes: Vec::new(),
            inventory: Vec::new(),
            assumptions: Vec::new(),
            evidence: Vec::new(),
        };
        let human = result.render_human();
        assert!(
            human.contains("COVERAGE HOLE") && human.contains("TEMP_SENSE"),
            "human report must carry the coverage hole: {human}"
        );
        let junit = result.render_junit();
        assert!(
            junit.contains("COVERAGE HOLE") && junit.contains("system-out"),
            "junit must carry the coverage hole: {junit}"
        );
        let gh = result.render_github_annotations();
        assert!(
            gh.contains("COSIM COVERAGE HOLE") && gh.contains("::warning"),
            "github annotations must warn on the coverage hole: {gh}"
        );
        // No hole → no mention on any surface.
        let clean = CiResult {
            coverage_warnings: Vec::new(),
            dead_rails: Vec::new(),
            ..result
        };
        assert!(!clean.render_human().contains("COVERAGE HOLE"));
        assert!(!clean.render_junit().contains("COVERAGE HOLE"));
        assert!(!clean.render_github_annotations().contains("COVERAGE HOLE"));
    }
}
