//! Plain-language verdict mode: translate hauksbee's findings for someone who is
//! NOT an electrical engineer.
//!
//! Every static surface (`--drc`, `--lint`, `--si`, `--resources`) and the
//! stress/ratings fault report already produces structured findings. This module
//! takes those SAME findings (it never re-runs or forks the analysis) and maps
//! each finding kind to a plain-language template: **what** it is, **why** it
//! matters, and **what to do** about it, in everyday words.
//!
//! The output leads with a one-line overall verdict ("Looks healthy" / "N issues
//! found, M serious"), then lists each finding ordered by severity (serious
//! first). The expert tables stay the default; plain mode is strictly opt-in via
//! `--plain` and is derived here so the two never drift apart.

use std::fmt::Write as _;

use schemars::JsonSchema;
use serde::Serialize;

use hauksbee_extract::{
    DrcReport, LintCheck, NetLintReport, Severity, SiCheck, SiFinding, SiReport, SiSeverity,
    CHECK_BEFORE_ORDERING, DNP_FITTED_NOTE_MARKER, DNP_PULLUP_MESSAGE_MARKER,
    SPI_PULLUP_MESSAGE_MARKER,
};

use hauksbee_ir::units::{format_amps, format_celsius, format_gap_mm, format_volts, format_watts};
use hauksbee_ir::vocabulary::INSPECT_BEFORE_ORDERING;

use crate::stress::{FaultEvent, FaultKind};

/// How serious a finding is, in plain terms. Ordered worst-first so a simple
/// sort by `(level)` puts the things that will actually break a board on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlainLevel {
    /// Will almost certainly fail or behave wrong. Fix before ordering boards.
    Serious,
    /// Risky / cuts your margin. Works in the good case, bites in the bad one.
    Warning,
    /// Worth knowing, unlikely to actually bite.
    Note,
}

impl PlainLevel {
    /// A short word for the bullet marker in the rendered output.
    const fn tag(self) -> &'static str {
        match self {
            Self::Serious => "SERIOUS",
            Self::Warning => "WARNING",
            Self::Note => "note",
        }
    }
}

/// One finding, translated. Three plain sentences: what happened, why it
/// matters, and what to do about it.
#[derive(Debug, Clone)]
pub struct PlainFinding {
    pub level: PlainLevel,
    /// "What it is"; the headline, in everyday language.
    pub what: String,
    /// "Why it matters"; the consequence if left as-is.
    pub why: String,
    /// "Suggested fix"; the concrete thing to change.
    pub fix: String,
    /// Board location (mm, layout coordinate space) when the finding points at
    /// a physical spot (a DRC short, the tightest clearance gap). Lets the web
    /// report pan its board map to the finding instead of making the user
    /// parse "near x=12.3 mm" prose. None for findings with no single spot.
    pub loc_mm: Option<[f64; 2]>,
    /// The one part this finding is about, when it names exactly one. Findings
    /// that read the same for different parts print once in the text report
    /// (`write_findings`); everything else leaves it `None` and prints alone.
    pub part: Option<String>,
}

/// An actionable "heads up" note: worth knowing, not a failure.
///
/// Carries the same what / why / what-to-do shape a finding does, so the web /
/// TUI / CLI can all give a novice the translation instead of a bare jargon
/// line. `why` / `fix` may be empty for a note that is already a complete
/// self-contained sentence (e.g. a co-sim caveat); renderers omit those lines
/// when empty.
#[derive(Debug, Clone, Default)]
pub struct HeadsUp {
    /// "What it is"; the observation, in everyday language.
    pub what: String,
    /// "Why it matters"; the consequence. May be empty.
    pub why: String,
    /// "What to do"; the concrete next step. May be empty.
    pub fix: String,
}

impl HeadsUp {
    /// A self-contained note with no separate why/what-to-do (already one
    /// complete sentence).
    pub fn note(what: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            why: String::new(),
            fix: String::new(),
        }
    }

    /// A fully-glossed three-part note.
    pub fn glossed(
        what: impl Into<String>,
        why: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            what: what.into(),
            why: why.into(),
            fix: fix.into(),
        }
    }
}

/// A whole report, translated: the headline verdict plus the ordered findings.
#[derive(Debug, Clone, Default)]
pub struct PlainReport {
    /// What was checked, e.g. "Copper spacing (DRC)". Drives the verdict line.
    pub subject: String,
    /// Overrides the noun phrase the verdict line builds from `subject`. Set it
    /// when the default ("no {subject, lowercased} problems found") would either
    /// mangle an acronym or name the wrong thing. `--resources` is both: it runs
    /// one member of the lint family, so the shared `plain_netlint` subject
    /// ("connectivity") had it reporting "no connectivity problems found" over a
    /// pass that only ever looked at MCU resource conflicts.
    pub verdict_noun: Option<String>,
    pub findings: Vec<PlainFinding>,
    /// Actionable info-level notes promoted into a "Heads up:" section. These are
    /// NOT counted as findings (they don't change the verdict), but they are also
    /// NEVER silently dropped in `--plain` mode: the 171-ohm USB-impedance note
    /// the 171-ohm USB-impedance note is exactly this. A "Looks healthy" verdict
    /// that hides the only actionable observation is a breach of trust. Each note
    /// carries a what / why / what-to-do gloss so it translates the jargon rather
    /// than dumping it.
    pub heads_up: Vec<HeadsUp>,
    /// Current-carrying / active parts with no model
    /// ([`crate::result::unmodelled_critical_refs`]). Non-empty forces the
    /// verdict off "Looks healthy": a clean check over an unmodelled power FET
    /// or main IC is vacuous, and the verdict must read INCONCLUSIVE, naming
    /// the parts and the unlocking input, instead of a clean bill. The emit
    /// sites that hold a bind report set this; renderers with no bind context
    /// leave it empty and are unchanged.
    pub unmodelled_critical: Vec<String>,
    /// The report already printed the INCONCLUSIVE banner for
    /// `unmodelled_critical` once, above every section, so this section says
    /// [`INCONCLUSIVE_SEE_TOP`] instead of repeating it.
    pub inconclusive_said_above: bool,
    /// A report-wide qualification that changes how the finding count may be
    /// read. Copper findings on a newer-than-validated board format are kept
    /// visible but demoted, so the headline must explain that demotion in the
    /// same breath as the severity count.
    pub unvalidated: Option<String>,
}

/// What a model-dependent web section says when an earlier section already
/// named the parts with no model: the fact once, then a pointer, never the
/// same paragraph twice.
pub const WEB_INCONCLUSIVE_SEE_ABOVE: &str =
    "INCONCLUSIVE: the key parts named in the section above have no model, so this is not a clean bill either.";

/// What a model-dependent section says when the INCONCLUSIVE banner naming the
/// unmodelled parts was already printed once, above every section.
pub const INCONCLUSIVE_SEE_TOP: &str =
    "Not a clean bill: key parts have no model (see the verdict at the top).";

/// De-escape `KiCad`'s label escapes so a plain sentence names the net the way
/// the schematic does. `KiCad` writes a literal `/` inside a label as `{slash}`
/// (`/` is its sheet-path separator), and that token travels all the way
/// through extraction into the finding text: the reader saw
/// `Net-(U4-LNA_IN{slash}RF)` where the schematic says `Net-(U4-LNA_IN/RF)`.
/// The escaped form stays the identity used for matching; only what a person
/// reads is unescaped, and only here, at the last step before rendering.
fn readable(s: String) -> String {
    if s.contains("{slash}") {
        s.replace("{slash}", "/")
    } else {
        s
    }
}

impl PlainReport {
    fn new(subject: &str) -> Self {
        Self {
            subject: subject.to_string(),
            verdict_noun: None,
            findings: Vec::new(),
            heads_up: Vec::new(),
            unmodelled_critical: Vec::new(),
            inconclusive_said_above: false,
            unvalidated: None,
        }
    }

    /// The INCONCLUSIVE banner for this section: in full, or as a pointer to
    /// the one printed above every section.
    fn inconclusive(&self) -> String {
        if self.inconclusive_said_above {
            INCONCLUSIVE_SEE_TOP.to_string()
        } else {
            crate::result::inconclusive_verdict(&self.unmodelled_critical)
        }
    }

    /// [`Self::inconclusive`] for the web app: the same fact (the count, the
    /// parts), without the terminal commands. The buttons that fix it are the
    /// section's `actions`.
    fn web_inconclusive(&self) -> String {
        if self.inconclusive_said_above {
            WEB_INCONCLUSIVE_SEE_ABOVE.to_string()
        } else {
            crate::result::web_inconclusive_verdict(&self.unmodelled_critical)
        }
    }

    fn push(&mut self, level: PlainLevel, what: String, why: String, fix: String) {
        self.findings.push(PlainFinding {
            level,
            what: readable(what),
            why,
            fix,
            loc_mm: None,
            part: None,
        });
    }

    /// [`Self::push`] for a finding about exactly one part, so a finding that
    /// repeats across parts can be printed once.
    fn push_for_part(
        &mut self,
        level: PlainLevel,
        what: String,
        why: String,
        fix: String,
        part: Option<&str>,
    ) {
        self.push(level, what, why, fix);
        if let Some(last) = self.findings.last_mut() {
            last.part = part.map(|p| readable(p.to_string()));
        }
    }

    /// [`Self::push`] for a heads-up note, so notes get the same name
    /// de-escaping every other plain sentence gets.
    fn push_note(&mut self, note: HeadsUp) {
        self.heads_up.push(HeadsUp {
            what: readable(note.what),
            why: note.why,
            fix: note.fix,
        });
    }

    /// [`Self::push`] with a board location, for findings that point at one
    /// physical spot the UI can pan to.
    fn push_at(
        &mut self,
        level: PlainLevel,
        what: String,
        why: String,
        fix: String,
        loc_mm: [f64; 2],
    ) {
        self.findings.push(PlainFinding {
            level,
            what: readable(what),
            why,
            fix,
            loc_mm: Some(loc_mm),
            part: None,
        });
    }

    /// Number of findings that are "serious" (the M in "N issues, M serious").
    #[must_use]
    pub fn serious_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.level == PlainLevel::Serious)
            .count()
    }

    /// Sort worst-first so the rendered list leads with what will break a board.
    fn sorted(&self) -> Vec<&PlainFinding> {
        let mut v: Vec<&PlainFinding> = self.findings.iter().collect();
        v.sort_by_key(|f| f.level);
        v
    }

    /// How many potential copper issues this report lists: the one number the
    /// plain verdict and the closing `VERDICT:` line both read, so the two
    /// surfaces cannot count differently.
    #[must_use]
    pub const fn potential_copper_issue_count(&self) -> usize {
        self.findings.len()
    }

    /// The one-line overall verdict.
    #[must_use]
    pub fn verdict(&self) -> String {
        self.verdict_for(false)
    }

    /// [`Self::verdict`] as the web app words it: identical except that an
    /// INCONCLUSIVE section names no terminal command (the app offers buttons).
    #[must_use]
    pub fn web_verdict(&self) -> String {
        self.verdict_for(true)
    }

    fn verdict_for(&self, web: bool) -> String {
        let n = self.findings.len();
        if let Some(reason) = &self.unvalidated {
            let reason = reason.trim_end_matches(['.', ';']);
            let serious = self.serious_count();
            if n == 0 {
                return format!(
                    "{}, but this result is UNVALIDATED: {reason}",
                    sentence_case(&potential_copper_issues(0))
                );
            }
            let issues = if n == 1 { "issue" } else { "issues" };
            if serious > 0 {
                let remaining = n.saturating_sub(serious);
                return format!(
                    "{n} potential copper {issues} found; {serious} short(s) confirmed by KiCad's own \
                     check count as serious. The board format remains UNVALIDATED ({reason}); the \
                     other {remaining} finding(s) keep their qualified severity."
                );
            }
            return format!(
                "{n} potential copper {issues} found; UNVALIDATED: {reason}; none counted as \
                 serious BECAUSE unvalidated. Each unmatched short is tool-only until KiCad's \
                 own DRC confirms that specific net pair."
            );
        }
        // The two noun phrases the verdict slots in. `verdict_noun` supplies both
        // verbatim; otherwise they are built from the heading-cased `subject`.
        let (healthy_noun, failure_noun) = self.verdict_noun.as_ref().map_or_else(
            || {
                let lower = self.subject.to_lowercase();
                (format!("{lower} problems"), format!("{lower} failures"))
            },
            |noun| (noun.clone(), noun.clone()),
        );
        if n == 0 {
            // A clean check over unmodelled current-carrying / active parts is
            // vacuous, not healthy. Refuse the clean bill and name what unlocks
            // a conclusive verdict. This does NOT change any exit code; it is
            // verdict prose only (the exit contract lives in docs/ci/CI.md).
            // Actionable heads-up notes stay on the verdict line: INCONCLUSIVE
            // must not hide the one observation the user may have come to check.
            if !self.unmodelled_critical.is_empty() {
                let mut v = if web {
                    self.web_inconclusive()
                } else {
                    self.inconclusive()
                };
                if !self.heads_up.is_empty() {
                    let hn = self.heads_up.len();
                    let things = if hn == 1 { "thing" } else { "things" };
                    let _ = write!(v, " Plus {hn} {things} worth a look (see below).");
                }
                return v;
            }
            // No failures, but if there are actionable heads-up notes (e.g. a USB
            // pair off its impedance target), don't claim "no problems found",
            // that buries the one thing the user may have come to check. Point at
            // the notes below without escalating them to a failure (cry wolf).
            if !self.heads_up.is_empty() {
                let hn = self.heads_up.len();
                let things = if hn == 1 { "thing" } else { "things" };
                return format!("No {failure_noun}, but {hn} {things} worth a look (see below).");
            }
            return format!("Looks healthy: no {healthy_noun} found.");
        }
        let serious = self.serious_count();
        let issues = if n == 1 { "issue" } else { "issues" };
        if serious == 0 {
            format!("{n} {issues} found, none serious (worth a look).")
        } else {
            format!("{n} {issues} found, {serious} serious.")
        }
    }

    /// Render the full plain-language block: verdict line, then each finding as
    /// what / why / fix, ordered by severity.
    #[must_use]
    pub fn render(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "{}", self.verdict());
        // With real findings, the verdict line counts them (not a clean bill),
        // but the coverage hole must still be said out loud: fixing the listed
        // findings would otherwise flip the verdict straight to a vacuous pass.
        // When the report already said it once above every section, the
        // pointer back to it is only for a section with nothing else to show
        // (its verdict line would otherwise read as a clean bill).
        if !self.findings.is_empty()
            && !self.unmodelled_critical.is_empty()
            && !self.inconclusive_said_above
        {
            let _ = writeln!(s, "{}", self.inconclusive());
        }
        if !self.findings.is_empty() {
            let _ = writeln!(s);
            write_findings(&mut s, 1, &self.sorted());
        }
        write_heads_up(&mut s, &self.heads_up);
        s
    }
}

/// The numbered finding blocks, from number `first`. Findings that differ only
/// in which one part they name (same level, same why and fix, same wording
/// around the designator) print once as `N parts like A, B, C and M more`; the
/// JSON and the web list keep every one. Returns the next free number.
fn write_findings(out: &mut String, first: usize, findings: &[&PlainFinding]) -> usize {
    let groups = hauksbee_extract::repeat::group_repeats(findings.iter(), |f| {
        let part = f.part.as_ref()?;
        let shape = hauksbee_extract::repeat::repeat_shape(&f.what, std::slice::from_ref(part));
        Some(format!("{:?}/{}/{}/{shape}", f.level, f.why, f.fix))
    });
    let mut index = first;
    for group in groups {
        let lead = group[0];
        if group.len() == 1 {
            write_finding(out, index, lead);
        } else {
            let parts: Vec<&str> = group.iter().filter_map(|f| f.part.as_deref()).collect();
            let mut merged = (**lead).clone();
            merged.what = format!(
                "{}: {}",
                hauksbee_extract::repeat::describe_parts(&parts),
                lead.what
            );
            write_finding(out, index, &merged);
        }
        index += 1;
    }
    index
}

/// One finding as the numbered what / why / what-to-do block. The single
/// rendering of a [`PlainFinding`]: both the full report and the condensed DRC
/// view print findings through here, so the two can never drift apart.
fn write_finding(out: &mut String, index: usize, finding: &PlainFinding) {
    let _ = writeln!(out, "{}. [{}] {}", index, finding.level.tag(), finding.what);
    let _ = writeln!(out, "     Why it matters: {}", finding.why);
    let _ = writeln!(out, "     What to do:     {}", finding.fix);
    let _ = writeln!(out);
}

/// The "Heads up" section, or nothing when there are no notes. Actionable info
/// notes are NEVER dropped, even when the verdict reads "healthy" and even when
/// the finding list above them was condensed: that is the anti-false-comfort
/// guarantee, so it has one implementation.
fn write_heads_up(out: &mut String, notes: &[HeadsUp]) {
    if notes.is_empty() {
        return;
    }
    let _ = writeln!(out, "\nHeads up (worth knowing, not a failure):");
    for note in notes {
        let _ = writeln!(out, "  - {}", note.what);
        if !note.why.is_empty() {
            let _ = writeln!(out, "       Why it matters: {}", note.why);
        }
        if !note.fix.is_empty() {
            let _ = writeln!(out, "       What to do:     {}", note.fix);
        }
    }
}

/// The notice a pre-layout board carries in place of its copper findings.
pub const PRE_LAYOUT_NOTICE: &str = "Circuit, model and firmware checks ran on the netlist; copper checks (shorts, spacing) wait until the board is routed.";

/// The "Fix these first" item for the not-routed state: what to do, since the
/// notice and the headline already say what the state is.
pub const ROUTE_THE_BOARD: &str = "Route the board";

fn is_unrouted_state(f: &PlainFinding) -> bool {
    f.what
        .starts_with(hauksbee_extract::NOT_ROUTED_MESSAGE_PREFIX)
}

/// The verdict reason when routing is the only cause listed.
pub const PRE_LAYOUT_REASON: &str = "the board must be routed before it can be ordered.";

/// The headline of a pre-layout board.
///
/// `total` / `serious` are the counters
/// the report shows (see [`without_held_copper`]): they include the not-routed
/// state, because that finding is listed and tagged serious, so the headline
/// quotes the same numbers the counters and the sections do.
#[must_use]
pub fn pre_layout_headline(total: usize, serious: usize, copper_held: usize) -> String {
    let lead = "Not routed yet:";
    let issues = if total == 1 { "issue" } else { "issues" };
    let mut headline = if total <= 1 {
        format!("{lead} the board cannot be ordered as is, and nothing else was found.")
    } else {
        format!("{lead} {total} {issues} found, {serious} serious, including the missing routing.")
    };
    if copper_held > 0 {
        let _ = write!(
            headline,
            " {copper_held} copper finding(s) are held back until it is routed."
        );
    }
    headline
}

/// The report's `(total, serious)` without the held-back copper findings.
///
/// A pre-layout board holds copper findings back (they say nothing until the board is routed, and the web
/// lists them apart, labelled as held back). The not-routed state itself IS
/// counted: it is listed and tagged serious, and the CLI verdict counts it too.
#[must_use]
pub fn without_held_copper(copper: &PlainReport, total: usize, serious: usize) -> (usize, usize) {
    let held = |level: Option<PlainLevel>| {
        copper
            .findings
            .iter()
            .filter(|f| level.is_none_or(|l| f.level == l))
            .count()
    };
    (
        total.saturating_sub(held(None)),
        serious.saturating_sub(held(Some(PlainLevel::Serious))),
    )
}

/// How many causes "Fix these first" lists.
pub const FIX_FIRST_MAX: usize = 3;

/// One cause to fix, with the findings that share its remedy folded in.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct FixFirstItem {
    pub what: String,
    pub why: String,
    pub fix: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
    /// Other serious findings with this remedy, listed once here.
    pub similar: usize,
}

/// What to fix first, worst cause first. The one ordering the CLI report and
/// the web report both print.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct FixFirst {
    /// Set on a layout with placed pads and no copper: its copper findings are
    /// not listed (they wait for routing) and `notice` says so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<&'static str>,
    pub items: Vec<FixFirstItem>,
    /// Serious causes beyond the ones listed.
    pub more: usize,
    /// On a pre-layout board, how many copper findings were held back.
    #[serde(skip_serializing_if = "is_zero")]
    pub copper_held: usize,
}

// serde's `skip_serializing_if` hands the field by reference, so the signature is fixed.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl FixFirst {
    /// Nothing to show: no serious cause and no pre-layout notice.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.notice.is_none() && self.items.is_empty()
    }

    /// The `FIX THESE FIRST` block for the plain report, or nothing.
    #[must_use]
    pub fn render(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut out = String::from("== FIX THESE FIRST ==\n");
        if let Some(notice) = self.notice {
            let _ = writeln!(out, "{notice}");
        }
        for (i, item) in self.items.iter().enumerate() {
            let similar = match item.similar {
                0 => String::new(),
                n => format!(" (+{n} more like this)"),
            };
            let _ = writeln!(out, "{}. {}{similar}", i + 1, item.what);
            let _ = writeln!(out, "     What to do: {}", item.fix);
        }
        if self.more > 0 {
            let _ = writeln!(
                out,
                "...and {} more serious cause(s) in the sections below.",
                self.more
            );
        }
        out.push('\n');
        out
    }
}

/// The USB-C verdict as a finding, when it is serious.
fn usbc_finding(usbc: &crate::checks::usb_c::UsbcReport) -> Option<PlainFinding> {
    let (what, why, fix) = usbc.web_gloss().filter(|_| usbc.is_serious())?;
    Some(PlainFinding {
        level: PlainLevel::Serious,
        what,
        why,
        fix,
        loc_mm: None,
        part: None,
    })
}

/// Pick what to fix first from the serious findings of the report's sections.
///
/// Causes, not findings: findings with the same remedy in the same check family
/// fold into one item. Copper comes first (a short wrecks a board), then wiring,
/// USB-C and signal integrity. On a board with placed pads and no copper at all
/// the copper findings are left out (they say nothing until it is routed) and
/// "route it" goes last, after any real defect in the netlist.
#[must_use]
pub fn fix_first(
    copper: &PlainReport,
    wiring: &PlainReport,
    usbc: Option<&crate::checks::usb_c::UsbcReport>,
    si: &PlainReport,
) -> FixFirst {
    let is_unrouted = is_unrouted_state;
    let pre_layout = wiring.findings.iter().any(is_unrouted);
    let usbc = usbc.and_then(usbc_finding);
    let all_copper = copper.findings.len();
    let copper = if pre_layout {
        &[][..]
    } else {
        &copper.findings[..]
    };
    // (family order, unrouted last): sort is stable, so ties keep report order.
    let mut serious: Vec<(usize, &PlainFinding)> =
        [copper, &wiring.findings, usbc.as_slice(), &si.findings]
            .into_iter()
            .enumerate()
            .flat_map(|(family, fs)| fs.iter().map(move |f| (family, f)))
            .filter(|(_, f)| f.level == PlainLevel::Serious)
            .collect();
    serious.sort_by_key(|(family, f)| (is_unrouted(f), *family));
    let mut causes: Vec<(usize, &PlainFinding, usize)> = Vec::new();
    for (family, f) in serious {
        match causes
            .iter_mut()
            .find(|c| c.0 == family && c.1.fix == f.fix)
        {
            Some(cause) => cause.2 += 1,
            None => causes.push((family, f, 0)),
        }
    }
    let more = causes.len().saturating_sub(FIX_FIRST_MAX);
    FixFirst {
        notice: pre_layout.then_some(PRE_LAYOUT_NOTICE),
        items: causes
            .into_iter()
            .take(FIX_FIRST_MAX)
            .map(|(_, f, similar)| FixFirstItem {
                what: if is_unrouted(f) {
                    ROUTE_THE_BOARD.to_string()
                } else {
                    f.what.clone()
                },
                why: f.why.clone(),
                fix: f.fix.clone(),
                x: f.loc_mm.map(|l| l[0]),
                y: f.loc_mm.map(|l| l[1]),
                similar,
            })
            .collect(),
        more,
        copper_held: if pre_layout { all_copper } else { 0 },
    }
}

/// The compact decision surface at the front of a combined `--check --plain`
/// report. It classifies findings that already exist; it never runs a check or
/// derives a new electrical claim.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct TriageEntry {
    /// Stable check-family id (`drc`, `lint`, `si`, `usb_c`).
    pub id: String,
    /// Stable finding kind within that family.
    pub kind: String,
    /// The exact one-line statement rendered on the triage screen.
    pub statement: String,
    /// Layout-space location for findings that identify one physical spot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location_mm: Option<[f64; 2]>,
}

impl TriageEntry {
    fn new(
        id: impl Into<String>,
        kind: impl Into<String>,
        statement: impl Into<String>,
        location_mm: Option<[f64; 2]>,
    ) -> Self {
        let statement = statement
            .into()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        Self {
            id: id.into(),
            kind: kind.into(),
            statement,
            location_mm,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema, PartialEq)]
pub struct OrderTriage {
    pub do_not_order: Vec<TriageEntry>,
    pub inspect: Vec<TriageEntry>,
    pub checked_ok: Vec<TriageEntry>,
    pub not_covered: String,
}

/// The not-covered sentence for parts whose values the input cannot carry, or
/// `None` when every fitted part has a value or a placeholder the designer set.
///
/// This is a reduced scope and not a defect: an Altium `.PcbDoc`, an EasyEDA
/// Pro `.epcb` or an IPC-D-356 netlist has nowhere to hold a value, so the
/// lint raises no placeholder finding for such a part, and this line says what
/// was therefore not judged and how to supply the values (the BOM). A part
/// whose value is present, even a placeholder such as `R` or `?`, is not
/// counted here: the lint still flags it.
pub(crate) fn values_not_carried_note(board: &hauksbee_extract::ExtractedBoard) -> Option<String> {
    let refs: Vec<String> = board
        .components
        .iter()
        // A manufacturer part number names the part outright (a BOM supplied
        // one), so the unknown value is not what keeps it from being judged.
        .filter(|c| {
            !c.dnp
                && c.value_not_carried()
                && !c.properties.iter().any(|(key, value)| {
                    hauksbee_extract::bom::is_mpn_property_key(key) && !value.trim().is_empty()
                })
        })
        .map(|c| c.reference.clone())
        .collect();
    (!refs.is_empty()).then(|| {
        hauksbee_ir::vocabulary::values_not_carried_scope(
            refs.len(),
            &crate::result::refs_summary(&refs),
        )
    })
}

impl OrderTriage {
    /// Append one sentence to the not-covered line (nothing for `None`).
    pub(crate) fn add_not_covered(&mut self, note: Option<String>) {
        let Some(note) = note else { return };
        if !self.not_covered.is_empty() {
            self.not_covered.push(' ');
        }
        self.not_covered.push_str(&note);
    }

    /// One triage bucket; an empty bucket prints nothing (see `render`).
    fn write_bucket(out: &mut String, heading: &str, entries: &[TriageEntry]) {
        if !entries.is_empty() {
            let _ = writeln!(out, "{heading}:");
        }
        let mut i = 0;
        while i < entries.len() {
            let entry = &entries[i];
            let same_kind = entries[i..]
                .iter()
                .take_while(|candidate| candidate.id == entry.id && candidate.kind == entry.kind)
                .count();
            let class = match (heading, entry.id.as_str(), entry.kind.as_str()) {
                ("DO NOT ORDER", "drc", "short") => {
                    Some("copper shorts confirmed by KiCad's own check")
                }
                (INSPECT_BEFORE_ORDERING, "drc", "short") => {
                    Some("tool-only potential copper shorts with coordinates")
                }
                (INSPECT_BEFORE_ORDERING, "drc", "clearance") => {
                    Some("below-rule clearance groups with coordinates")
                }
                _ => None,
            };
            if let Some(class) = class.filter(|_| same_kind >= 4) {
                let _ = writeln!(
                    out,
                    "  - {same_kind} {}. Example: {} Full list follows below.",
                    class, entry.statement
                );
                i += same_kind;
            } else {
                let _ = writeln!(out, "  - {}", entry.statement);
                i += 1;
            }
        }
    }

    /// The headline for a triage with nothing to list. It follows the run's
    /// machine verdict ([`crate::result::RunVerdict`], the one `--json` prints),
    /// so an empty triage can never say OK TO ORDER over a failed or
    /// inconclusive run.
    fn empty_headline(verdict: &crate::result::RunVerdict) -> String {
        // A failing run's not-judged sentence is its own line, below the
        // failures (see `render`), so the headline does not carry it too.
        let reason = verdict.reason_without_not_judged().unwrap_or_default();
        // A schematic that passes its checks is not "OK TO ORDER": there is no
        // board to order. The lead says what was and was not judged instead.
        if let Some(why) = verdict.unorderable.as_deref() {
            let rest = verdict.conditions_without(why);
            return if rest.is_empty() {
                format!("{}: {why}", hauksbee_ir::vocabulary::NOT_ORDERABLE_LEAD)
            } else {
                format!(
                    "{}: {why} Also: {}",
                    hauksbee_ir::vocabulary::NOT_ORDERABLE_LEAD,
                    rest.join(" ")
                )
            };
        }
        match verdict.verdict {
            // The reason starts "PARTLY JUDGED (not a pass)".
            _ if verdict.partial => reason.to_string(),
            "pass" if verdict.conditions.is_empty() => {
                "OK TO ORDER: nothing found that blocks ordering or needs inspecting first."
                    .to_string()
            }
            "pass" => format!(
                "OK TO ORDER, conditionally: nothing found that blocks ordering or needs \
                 inspecting first; this rests on: {}",
                verdict.conditions.join(" ")
            ),
            "fail" => format!("NOT OK TO ORDER: this run failed. {reason}"),
            _ => format!(
                "{}: nothing listed below blocks ordering, but the run cannot vouch \
                 for this board. {reason}",
                hauksbee_ir::vocabulary::INCONCLUSIVE_LEAD
            ),
        }
    }

    /// The pass conditions this triage does not already list as an item: an
    /// abstention the inspect bucket names is stated there, in the same words,
    /// so the block says it once.
    fn unlisted_conditions<'a>(&self, verdict: &'a crate::result::RunVerdict) -> Vec<&'a str> {
        let norm = |s: &str| {
            s.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .trim_end_matches('.')
                .to_string()
        };
        let listed: std::collections::HashSet<String> = (self.do_not_order.iter())
            .chain(&self.inspect)
            .map(|e| norm(&e.statement))
            .collect();
        // An abstention said once for several parts ("... (D1 and D2).") is
        // the listed statement with the parts it covers appended.
        let is_listed = |c: &str| {
            let c = norm(c);
            listed.contains(&c)
                || (listed.iter()).any(|l| {
                    c.strip_prefix(l.as_str())
                        .is_some_and(|rest| rest.starts_with(" ("))
                })
        };
        (verdict.conditions.iter())
            .filter(|c| !is_listed(c))
            .map(String::as_str)
            .collect()
    }

    pub(crate) fn render(&self, verdict: &crate::result::RunVerdict) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "== ORDER / DON'T-ORDER TRIAGE ==");
        // Three empty lists read as a missing answer, so a clean triage says
        // so in one line and the empty buckets are left out. A failed run says
        // so in the same place even when the buckets below do not name its cause.
        if (self.do_not_order.is_empty() && self.inspect.is_empty())
            || verdict.partial
            || verdict.unorderable.is_some()
        {
            let _ = writeln!(out, "{}", Self::empty_headline(verdict));
        } else if verdict.verdict == "fail" && self.do_not_order.is_empty() {
            let _ = writeln!(
                out,
                "NOT OK TO ORDER YET: this run failed. {}",
                verdict.reason_without_not_judged().unwrap_or_default()
            );
        } else if verdict.is_pass() && !self.unlisted_conditions(verdict).is_empty() {
            // A pass with items to inspect still rests on its conditions; say
            // so here, not only on the closing line. A condition that IS one of
            // the items below (an abstention to check by hand) is said there.
            let _ = writeln!(
                out,
                "CONDITIONALLY: whatever the items below decide, this rests on: {}",
                self.unlisted_conditions(verdict).join(" ")
            );
        }
        Self::write_bucket(&mut out, "DO NOT ORDER", &self.do_not_order);
        Self::write_bucket(&mut out, INSPECT_BEFORE_ORDERING, &self.inspect);
        Self::write_bucket(&mut out, "CHECKED AND OK", &self.checked_ok);
        // What a failing run did not judge, said once, on one line after its
        // failures: the verdict, then the failures, then this.
        if let Some(line) = verdict.not_judged_line() {
            let _ = writeln!(out, "{line}");
        }
        if !self.not_covered.is_empty() {
            let _ = writeln!(out, "NOT COVERED: {}", self.not_covered);
        }
        let _ = writeln!(
            out,
            "DETAIL: Full findings and evidence follow; add --verbose to expand every clearance finding.\n"
        );
        out
    }
}

/// Build the order triage from the already-computed combined-check reports.
///
/// Classification policy is intentionally narrow:
/// - every existing Serious finding is a do-not-order item;
/// - `DeviceDecode` is also do-not-order because that check already asserts a
///   definite wrong datasheet-selected mode/limit (including ratings budgets);
/// - copper shorts and below-rule clearance findings with coordinates go to
///   inspect when their evidence is not serious;
/// - explicit abstentions (`UncheckedMcu`, USB-C `Info`, or a nominal USB-C
///   result undermined by a model gap on its own CC nets) go to inspect;
/// - only an unqualified USB-C `Ok` is emitted as a positive result.
#[allow(clippy::too_many_lines, reason = "one pass per finding family")]
pub(crate) fn order_triage_with_rule_source(
    drc: &crate::result::DrcStructured,
    clearance_rule_source: &crate::result::ClearanceRuleProvenance,
    lint: &NetLintReport,
    si: &SiReport,
    usbc: Option<&crate::checks::usb_c::UsbcReport>,
    usbc_reliable: bool,
    unmodelled_parts: usize,
    include_clearance_unlock: bool,
) -> OrderTriage {
    // Nothing to say about models when every part has one.
    let models = match unmodelled_parts {
        0 => String::new(),
        n => format!("{n} {} an executable simulation model; model-dependent firmware/analog/AC/thermal claims involving those parts are not covered (run --report for the bind table). Copper geometry and datasheet decode rules do not require a simulation model.", if n == 1 { "part lacks" } else { "parts lack" }),
    };
    let clearance = match clearance_rule_source.source {
        crate::result::ClearanceRuleSource::Defaulted
            if include_clearance_unlock && clearance_rule_source.project_file_found =>
        {
            " Clearance findings use DEFAULT rules, not your rules, because the sibling .kicad_pro was present but its netclass rules were not read. Repair or replace that .kicad_pro with readable netclass rules and rerun to check your rules."
        }
        crate::result::ClearanceRuleSource::Defaulted if include_clearance_unlock => " Clearance findings use DEFAULT rules, not your rules, because no usable .kicad_pro netclass rules were read. Place the matching .kicad_pro next to the board and rerun to check your rules.",
        crate::result::ClearanceRuleSource::Defaulted => " Clearance findings use DEFAULT rules, not your rules, because no usable .kicad_pro netclass rules were read.",
        crate::result::ClearanceRuleSource::ToolDefault => " Clearance findings use TOOL DEFAULT rules, not your rules; this importer did not read design rules.",
        _ => "",
    };
    let custom_rules = clearance_rule_source
        .custom_rules_not_covered()
        .unwrap_or_default();
    let mut triage = OrderTriage {
        not_covered: format!("{models}{clearance} {custom_rules}")
            .trim()
            .to_string(),
        ..OrderTriage::default()
    };

    let drc_plain = plain_drc_structured_with_rule_source(drc, clearance_rule_source);
    for (short, finding) in drc.shorts.iter().zip(&drc_plain.findings) {
        let entry = |what| TriageEntry::new("drc", "short", what, finding.loc_mm);
        if finding.level == PlainLevel::Serious {
            triage.do_not_order.push(entry(finding.what.clone()));
        } else {
            let tool_only = " Tool-only: KiCad's own check has not confirmed it.";
            let evidence = short.oracle_agreement().map_or(tool_only, |_| "");
            triage
                .inspect
                .push(entry(format!("{}{evidence}", finding.what)));
        }
    }
    // The structured/plain order is shorts, stale-fill notes, below-rule
    // violations, then at-limit observations. Only actual below-rule groups
    // belong on the order screen; no-margin observations remain in the detail
    // below, and a stale fill is not an order blocker (it never gates and is
    // no short). It is still an item to inspect: gerbers are plotted from the
    // stored fill, so ordering without refilling sends the fab the old pour. A
    // board whose only finding is a stale fill therefore reads "inspect before
    // ordering", never a clean OK TO ORDER.
    triage.inspect.extend(
        drc_plain
            .findings
            .iter()
            .skip(drc.shorts.len())
            .take(drc.stale_fills.len())
            .map(|finding| {
                TriageEntry::new("drc", "stale_fill", finding.what.clone(), finding.loc_mm)
            }),
    );
    let below_rule = drc_plain
        .findings
        .iter()
        .skip(drc.shorts.len() + drc.stale_fills.len());
    triage.inspect.extend(
        below_rule.take(drc.violations.len()).map(|finding| {
            TriageEntry::new("drc", "clearance", finding.what.clone(), finding.loc_mm)
        }),
    );

    // Anything the machine verdict gates on (`JsonFinding::gates`: a lint
    // warning, any real SI finding) is at least an inspect item. Asking the
    // same predicate the verdict asks is what keeps this headline from reading
    // OK TO ORDER over a run whose `--json` verdict is `fail`.
    let lint_json = crate::result::lint_findings_json(lint);
    for ((raw, finding), json) in lint
        .findings
        .iter()
        .zip(&plain_netlint(lint).findings)
        .zip(&lint_json)
    {
        let entry = TriageEntry::new(
            "lint",
            raw.check.as_str(),
            finding.what.clone(),
            finding.loc_mm,
        );
        let decoded = matches!(
            raw.check,
            LintCheck::DeviceDecode | LintCheck::OperatingEnvelope
        );
        if finding.level == PlainLevel::Serious || (decoded && raw.severity != Severity::Low) {
            triage.do_not_order.push(entry);
        } else if raw.check == LintCheck::UncheckedMcu
            || json.gates()
            || raw.message.starts_with(CHECK_BEFORE_ORDERING)
        {
            triage.inspect.push(entry);
        }
    }

    let si_json = crate::result::si_findings_json(si);
    let si_real = si_json.iter().filter(|f| f.severity != "info");
    for ((raw, finding), json) in si.findings_only().zip(&plain_si(si).findings).zip(si_real) {
        let entry = TriageEntry::new(
            "si",
            raw.check.as_str(),
            finding.what.clone(),
            finding.loc_mm,
        );
        if finding.level == PlainLevel::Serious {
            triage.do_not_order.push(entry);
        } else if json.gates() {
            triage.inspect.push(entry);
        }
    }

    if let Some(report) = usbc {
        use crate::checks::usb_c::UsbcLevel;
        let item = format!("USB-C CC: {}", report.headline);
        let (bucket, item) = match report.level {
            UsbcLevel::Serious => (&mut triage.do_not_order, item),
            UsbcLevel::Info => (&mut triage.inspect, item),
            UsbcLevel::Ok if usbc_reliable => (&mut triage.checked_ok, item),
            UsbcLevel::Ok => (&mut triage.inspect, format!(
                "{item} Nominal result only: an unresolved part on the CC nets prevents relying on it."
            )),
        };
        bucket.push(TriageEntry::new("usb_c", "cc_compliance", item, None));
    }

    triage
}

// ── Severity bridges ─────────────────────────────────────────────────────────

const fn from_lint_sev(s: Severity) -> PlainLevel {
    match s {
        Severity::High => PlainLevel::Serious,
        Severity::Medium => PlainLevel::Warning,
        Severity::Low => PlainLevel::Note,
    }
}

const fn from_si_sev(s: SiSeverity) -> PlainLevel {
    match s {
        SiSeverity::High => PlainLevel::Serious,
        SiSeverity::Medium => PlainLevel::Warning,
        // Low and Info both land as a note; Info findings are filtered out
        // upstream so they never reach here.
        SiSeverity::Low | SiSeverity::Info => PlainLevel::Note,
    }
}

/// Join a list of reference designators into "U3", "U3 and C1", or
/// "U3, C1 and R7" so the prose reads naturally.
fn join_refs(refs: &[String]) -> String {
    match refs {
        [] => "the part".to_string(),
        [a] => a.clone(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{} and {}", rest.join(", "), last),
    }
}

/// Expand `KiCad`'s copper-layer codes to plain language on first sight. A novice
/// has no idea `F.Cu` / `B.Cu` mean the front / back copper
/// layer, so spell it out while keeping the code in parentheses for the expert.
fn friendly_layer(layer: &str) -> String {
    match layer {
        "F.Cu" => "the front copper layer (F.Cu)".to_string(),
        "B.Cu" => "the back copper layer (B.Cu)".to_string(),
        other => other.to_string(),
    }
}

// ── DRC (copper shorts / clearance) ───────────────────────────────────────────

/// A net name as a plain sentence names it: an unnamed net has to be described,
/// not quoted as an empty string.
const fn net_label(name: &str) -> &str {
    if name.is_empty() {
        "an unnamed net"
    } else {
        name
    }
}

/// Translate the geometric DRC report (copper shorts and near-shorts).
#[must_use]
pub fn plain_drc(report: &DrcReport) -> PlainReport {
    // This context has no filesystem/project provenance. Conservative is the
    // only honest default: callers that did read a project attach that fact to
    // DrcStructured before rendering.
    plain_drc_structured(&crate::result::DrcStructured::from_report(report))
}

/// The copper issue count as a phrase, shared by the plain verdict and the
/// closing `VERDICT:` line of an UNVALIDATED copper result.
#[must_use]
pub(crate) fn potential_copper_issues(count: usize) -> String {
    match count {
        0 => "no potential copper issues listed".to_string(),
        1 => "1 potential copper issue".to_string(),
        n => format!("{n} potential copper issues"),
    }
}

fn sentence_case(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// Translate the *grouped* DRC ([`crate::result::DrcStructured`]) for the plain /
/// web surfaces.
///
/// This is the single source of truth shared with the default text and `--json`
/// paths: duplicates are collapsed by (net pair + layer), and, crucially, a
/// group whose gap is exactly *at* the rule (`gap == rule`) is described as "at
/// minimum clearance (no margin)", NOT "below the spacing the board asks for".
/// Saying "below" when the gap merely equals the rule is the dishonest wording
/// the honesty change removed; this renderer keeps `--plain` and the web report
/// in step with that.
#[must_use]
pub fn plain_drc_structured(st: &crate::result::DrcStructured) -> PlainReport {
    let provenance = crate::result::ClearanceRuleProvenance::defaulted(st.clearance_rule_mm, false);
    plain_drc_structured_with_rule_source(st, &provenance)
}

#[must_use]
#[allow(clippy::too_many_lines, reason = "shorts, then clearance groups")]
pub fn plain_drc_structured_with_rule_source(
    st: &crate::result::DrcStructured,
    provenance: &crate::result::ClearanceRuleProvenance,
) -> PlainReport {
    // Keep the verdict subject stable for downstream contracts. The enclosing
    // CLI/web section title and the source notice carry the provenance before
    // any finding; folding it into the grammatical subject would turn
    // "copper spacing (DRC)" into a different check name.
    let mut out = PlainReport::new("Copper spacing (DRC)");
    out.unvalidated.clone_from(&st.version_warning);
    let st = &st.actionable_view(provenance);

    // Nothing to check is not the same as nothing wrong.
    //
    // A netlist (.net, IPC-D-356) carries connectivity and no copper, so the
    // clearance sweep examines zero primitives and finds zero problems. Left
    // alone, that rendered "Looks healthy: no copper spacing problems found",
    // which a reader takes as "your copper is fine". It was never looked at.
    // Found by running a large internal test board in as a newcomer would, where the
    // whole report came back clean on input that has no copper in it.
    //
    // Guarded on the primitive count rather than the file extension, so this
    // covers every format that arrives without copper, including ones added
    // later. The web front door has a narrower version of this for gerbers
    // only; this is the general case.
    if st.primitive_count == 0
        && st.shorts.is_empty()
        && st.violations.is_empty()
        && st.at_limit.is_empty()
    {
        // The input read no clearance rule at all: a netlist or fabrication
        // files (gerbers, ODB++, IPC-2581, EasyEDA). Their copper, where they have any,
        // was read for connectivity and never measured, and routing is not the
        // fix: the layout file is.
        if provenance.no_rule_read {
            out.push_note(HeadsUp::glossed(
                "Copper spacing was not checked: this input gave Hauksbee connectivity (what \
                 connects to what) but no copper geometry it measures gaps on, even where \
                 the files do contain traces and pours."
                    .to_string(),
                "Clearance DRC compares copper shapes against each other. A netlist has no \
                 copper at all, and fabrication files (gerbers, ODB++, IPC-2581) and EasyEDA boards are read for \
                 connectivity, not measured for gaps. Everything else in this report still \
                 applies.",
                "Run hauksbee on the board's layout file (.kicad_pcb, .brd, .PcbDoc) to \
                 check spacing, or run your CAD tool's design-rule check on the layout \
                 before ordering.",
            ));
            return out;
        }
        out.push_note(HeadsUp::glossed(
            "No copper was checked: copper spacing was not measured on these inputs, which \
             gave Hauksbee connectivity but no copper geometry it measures gaps on."
                .to_string(),
            "Clearance DRC compares copper shapes against each other. A netlist has no \
             copper at all, a layout file can simply not be routed yet, and fabrication \
             files (gerbers, ODB++) are read for connectivity, not measured for gaps. \
             Everything else in this report still applies.",
            "If this was a layout file (.kicad_pcb, .brd, .PcbDoc), it may have no routed \
             copper yet: route it. If it was a netlist or fabrication files (which do \
             carry copper, but not in a form this check measures), run hauksbee against \
             the layout file, or run your CAD tool's design-rule check to confirm spacing.",
        ));
        return out;
    }

    // KiCad 10 name-only nets and keyhole antipads are handled, but exact native
    // DRC finding parity remains unvalidated. Tool-only results carry a
    // downgraded severity (set once in DrcStructured::from_report); an exact
    // oracle pair match can later restore one result to serious. Surface the
    // caveat as a never-dropped heads-up so unmatched claims stay qualified.
    // A suppressed finding class must be visible on the human report too: a
    // reader who is never told the rule was applied cannot audit it.
    if let Some(n) = &st.suppression_note {
        out.push_note(HeadsUp::glossed(
            n.clone(),
            "KiCad always carves a different-net pad out of a pour, and KiCad 10 draws that \
             carve as a keyhole slit running through the pad interior, so the geometry reads \
             as a negative gap. Reporting those as shorts produced over a thousand false \
             positives on a single board, so the whole Zone-versus-Pad overlap class is \
             dropped.",
            "Pour incursions by a track, via or arc are still reported normally. If you need \
             the suppressed class audited, run KiCad's own DRC (Inspect -> Design Rules \
             Checker) on the board.",
        ));
    }

    if let Some(w) = &st.version_warning {
        out.push_note(HeadsUp::glossed(
            format!("Tool-only KiCad 10 copper findings remain downgraded pending exact native-DRC parity: {w}"),
            "Hauksbee handles KiCad 10's name-only nets and keyhole antipads. The remaining \
             limitation is narrower: its complete finding set can still differ from KiCad's \
             own DRC, and project clearance rules may live in the sibling .kicad_pro rather \
             than the board text checked here.",
            "Cross-check any short in KiCad 10's own DRC (Inspect -> Design Rules Checker), or \
             run with --oracle. Tool-only findings remain notes and stay out of strict CI \
             gating; an exact net-pair agreement from KiCad's own DRC is independently \
             confirmed, restored to serious, and allowed to gate.",
        ));
    }

    // Real shorts first; the things that actually break a board.
    for sh in &st.shorts {
        let a = net_label(&sh.net_a);
        let b = net_label(&sh.net_b);
        let where_ = format!(
            "near x={:.1} mm, y={:.1} mm on {}",
            sh.loc_mm[0],
            sh.loc_mm[1],
            friendly_layer(&sh.layer)
        );
        // Read the per-short severity (the single source); a downgraded short is
        // a Note, a real one is Serious.
        let level = if sh.severity == "serious" {
            PlainLevel::Serious
        } else {
            PlainLevel::Note
        };
        // A schematic declaration adds context but does not authorize where the
        // physical contact belongs.
        // The headline still says the two connections are touching and still
        // gives the location, so a reader who disagrees with the designer can
        // see exactly what to go and look at.
        if sh.severity != "serious" && sh.plain.contains("schematic declares the tie") {
            let oracle = sh
                .oracle_agreement()
                .map(|agreement| format!(" KiCad agreement: {agreement}."))
                .unwrap_or_default();
            out.push_at(
                level,
                format!(
                    "\"{a}\" and \"{b}\" are joined in copper, {where_}, and your schematic says that is on purpose.{oracle}"
                ),
                format!(
                    "The two are connected where they touch, so \"{a}\" and \"{b}\" are one node there. {}",
                    sh.plain
                ),
                format!(
                    "Nothing to change if that is what you meant. Worth a look at two things: that the join really happens where the schematic puts it (a star ground should meet at a single point, not in several places), and that the copper there is wide enough for the return current. If you did NOT mean to join \"{a}\" and \"{b}\", the schematic is what is wrong, not the layout."
                ),
                sh.loc_mm,
            );
            continue;
        }
        let oracle = sh
            .oracle_agreement()
            .map(|agreement| format!(" KiCad agreement: {agreement}."))
            .unwrap_or_default();
        let tool_only = if sh.severity != "serious"
            && sh.oracle_agreement().is_none()
            && st.version_warning.is_some()
        {
            " TOOL-ONLY: this net-pair claim comes from Hauksbee's unvalidated-format \
             geometry alone; no matching line from KiCad's own check confirms it."
        } else {
            ""
        };
        out.push_at(
            level,
            format!(
                "Two separate connections, \"{a}\" and \"{b}\", are touching, {where_}.{oracle}"
            ),
            format!(
                "These are meant to be electrically separate. Where they touch they become one connection (a short), so \"{a}\" and \"{b}\" will be forced to the same voltage. That usually means the board does the wrong thing, and if one is a power rail it can pull large current and overheat.{tool_only}"
            ),
            sh.fix.clone(),
            sh.loc_mm,
        );
    }

    // A track, via or pad inside another net's stored zone fill: the fill is
    // out of date, the copper is not shorted. One action, and it never gates.
    for sf in &st.stale_fills {
        let a = net_label(&sf.zone_net);
        let b = net_label(&sf.copper_net);
        let places = crate::result::locations(sf.count);
        out.push_at(
            PlainLevel::Warning,
            format!("{}.", hauksbee_ir::vocabulary::sentence_case(&sf.plain)),
            format!(
                "The saved copper pour for \"{a}\" was filled before the {} for \"{b}\" was routed, placed or moved, so it still covers that copper, at {places} on {}. Nothing is shorted today: KiCad's own check reports the same spots as clearance violations, and refilling the zones cuts the pour back by its clearance. Until it is refilled, the spacing and short results around these spots reflect the old pour.",
                sf.copper_kind,
                friendly_layer(&sf.layer),
            ),
            sf.fix.clone(),
            sf.loc_mm,
        );
    }

    // Genuinely below-rule clearance groups (gap < rule).
    for g in &st.violations {
        let a = net_label(&g.net_a);
        let b = net_label(&g.net_b);
        let places = crate::result::locations(g.count);
        let what = if g.below_count == g.count {
            format!(
                "\"{a}\" and \"{b}\" are very close but not quite touching at {places} on {} (tightest {}, below {}).",
                friendly_layer(&g.layer),
                format_gap_mm(g.min_gap_mm),
                provenance.rule_reference(g.rule_mm)
            )
        } else {
            format!(
                "\"{a}\" and \"{b}\" are close at {places} on {}: {} below {} (tightest {}), the rest exactly at the limit.",
                friendly_layer(&g.layer),
                g.below_count,
                provenance.rule_reference(g.rule_mm),
                format_gap_mm(g.min_gap_mm),
            )
        };
        let (why, fix) = if provenance.is_defaulted() {
            (
                "They are not shorted today, but at least one spot is below the fallback spacing Hauksbee applied. Because the project rules were not read, this finding is not evidence that the board violates its own rules; it identifies geometry to recheck once the project file is available.".to_string(),
                "Recheck this gap after the project rules are available; change the layout only if it is below the resolved netclass rule.".to_string(),
            )
        } else {
            (
                "They are not shorted today, but at least one spot is below the spacing the board asks for. Small manufacturing variation, a solder smear, or contamination could bridge them, so it is a reliability risk rather than a guaranteed failure.".to_string(),
                "Open up the spacing between these two so the gap meets your clearance rule, or relax the rule deliberately if you know this spot is fine.".to_string(),
            )
        };
        out.push_at(PlainLevel::Warning, what, why, fix, g.min_gap_loc_mm);
    }

    // At-the-limit groups (gap == rule, no margin). NOT "below" the rule.
    push_at_limit_findings(&mut out, &st.at_limit, provenance);

    out
}

fn push_at_limit_findings(
    out: &mut PlainReport,
    at_limit: &[crate::result::DrcGroup],
    provenance: &crate::result::ClearanceRuleProvenance,
) {
    for g in at_limit {
        let a = net_label(&g.net_a);
        let b = net_label(&g.net_b);
        let places = crate::result::locations(g.count);
        out.push_at(
            PlainLevel::Warning,
            format!(
                "\"{a}\" and \"{b}\" sit at minimum clearance (no margin) at {places} on {} ({}, exactly {}).",
                friendly_layer(&g.layer),
                format_gap_mm(g.min_gap_mm),
                provenance.rule_reference(g.rule_mm)
            ),
            if provenance.is_defaulted() {
                "These meet only the fallback clearance exactly. The project rules were not read, so this does not establish whether the board is at its own limit.".to_string()
            } else {
                "These meet your clearance rule exactly, with nothing to spare. They are not below the rule, so this is not a violation, but there is no margin left, so any small manufacturing variation eats into a gap that is already at its allowed minimum.".to_string()
            },
            if provenance.is_defaulted() {
                "Recheck this gap after the project rules are available before deciding whether it needs to change.".to_string()
            } else {
                "If you want some safety margin, open these gaps up a little beyond the rule. If the rule already reflects your process limits, this is acceptable as-is; just be aware there is no slack.".to_string()
            },
            g.min_gap_loc_mm,
        );
    }
}

/// Render the plain DRC for a terminal, condensing repeated near-identical
/// clearance findings so fifty warnings do not bury the three worth reading.
///
/// The full findings (shorts always, plus the first few clearance groups) keep
/// the complete what/why/what-to-do gloss; the remaining clearance groups
/// collapse to one aggregated line per (rule, layer). The `--json` surface is
/// untouched (always complete), and `verbose` restores every instance here.
#[must_use]
pub fn render_drc_condensed(st: &crate::result::DrcStructured, verbose: bool) -> String {
    let provenance = crate::result::ClearanceRuleProvenance::defaulted(st.clearance_rule_mm, false);
    render_drc_condensed_with_rule_source(st, &provenance, verbose)
}

#[must_use]
pub fn render_drc_condensed_with_rule_source(
    st: &crate::result::DrcStructured,
    provenance: &crate::result::ClearanceRuleProvenance,
    verbose: bool,
) -> String {
    render_drc_condensed_with_rule_source_and_unlock(st, provenance, verbose, true)
}

#[allow(clippy::cast_possible_truncation, reason = "whole-um grouping key")]
#[allow(clippy::cast_sign_loss, reason = "clearance rules are positive")]
#[allow(clippy::cast_precision_loss, reason = "um keys are far below 2^52")]
#[allow(
    clippy::too_many_lines,
    reason = "verdict, full findings, then the aggregates"
)]
pub(crate) fn render_drc_condensed_with_rule_source_and_unlock(
    st: &crate::result::DrcStructured,
    provenance: &crate::result::ClearanceRuleProvenance,
    verbose: bool,
    include_unlock: bool,
) -> String {
    use std::collections::BTreeMap;
    /// How many clearance findings keep the full three-line gloss.
    const FULL: usize = 3;

    let full = st;
    let st = &st.actionable_view(provenance);
    let pr = plain_drc_structured_with_rule_source(st, provenance);
    let summary = format!(
        "Summary: {} short(s), {} net pair(s) below the clearance rule, {} at minimum \
         clearance (no margin){}.",
        st.shorts.len(),
        st.violations.len(),
        st.at_limit.len(),
        if st.stale_fills.is_empty() {
            String::new()
        } else {
            format!(", {} out-of-date zone fill(s)", st.stale_fills.len())
        }
    );
    let mut lead = format!(
        "{}\n",
        if include_unlock {
            provenance.cli_notice()
        } else {
            provenance.source_notice()
        }
    );
    if let Some(notice) = provenance.custom_rules_notice() {
        let _ = writeln!(lead, "{notice}");
    }
    for note in full.default_rule_notes(provenance) {
        let _ = writeln!(lead, "{note}");
    }

    let warning_groups = st.violations.len() + st.at_limit.len();
    if verbose || warning_groups <= FULL + 1 {
        // Nothing worth condensing (or the user asked for everything): the full
        // report, with the trailing summary count appended.
        let mut s = lead;
        s.push_str(&pr.render());
        if warning_groups + st.shorts.len() + st.stale_fills.len() > 0 {
            let _ = writeln!(s, "{summary}");
        }
        return s;
    }

    // Condensed: verdict, full shorts + first FULL clearance findings, then one
    // aggregate line per (rule, layer) for the rest.
    let mut s = lead;
    let _ = writeln!(s, "{}", pr.verdict());
    let _ = writeln!(s);
    let mut shown = 0usize;
    let mut idx = 0usize;
    // Stale-fill notes lead the Warning findings and always print in full: each
    // names a refill, and there is one per net pair, not one per location.
    let mut stale_left = st.stale_fills.len();
    for f in pr.sorted() {
        // Shorts always print in full (Serious, or Note when downgraded on an
        // unvalidated format); only the Warning-level clearance groups condense.
        let full = match f.level {
            PlainLevel::Warning if stale_left > 0 => {
                stale_left -= 1;
                true
            }
            PlainLevel::Warning => {
                shown += 1;
                shown <= FULL
            }
            _ => true,
        };
        if !full {
            continue;
        }
        idx += 1;
        write_finding(&mut s, idx, f);
    }
    // The clearance findings enter `pr` in order: violations, then at_limit.
    // Everything past the first FULL of that combined list aggregates by
    // (layer, rule); below-rule and at-limit groups aggregate separately since
    // they mean different things.
    let shown_v = FULL.min(st.violations.len());
    let shown_l = FULL.saturating_sub(st.violations.len());
    // (layer, rule in um) -> (group count, location count, tightest gap)
    let mut rest_below: BTreeMap<(String, u64), (usize, usize, f64)> = BTreeMap::new();
    for g in &st.violations[shown_v..] {
        let e = rest_below
            .entry((g.layer.clone(), (g.rule_mm * 1000.0).round() as u64))
            .or_insert((0, 0, f64::INFINITY));
        e.0 += 1;
        e.1 += g.count;
        e.2 = e.2.min(g.min_gap_mm);
    }
    for ((layer, rule_um), (groups, locs, tightest)) in &rest_below {
        let rule = *rule_um as f64 / 1000.0;
        let _ = writeln!(
            s,
            "  ...and {groups} more net pair{} like this on {} ({locs} location{}, tightest \
             {} vs {}); pass --verbose for every instance.",
            if *groups == 1 { "" } else { "s" },
            friendly_layer(layer),
            if *locs == 1 { "" } else { "s" },
            format_gap_mm(*tightest),
            provenance.rule_reference(rule),
        );
    }
    let mut rest_limit: BTreeMap<(String, u64), (usize, usize)> = BTreeMap::new();
    for g in &st.at_limit[shown_l.min(st.at_limit.len())..] {
        let e = rest_limit
            .entry((g.layer.clone(), (g.rule_mm * 1000.0).round() as u64))
            .or_insert((0, 0));
        e.0 += 1;
        e.1 += g.count;
    }
    for ((layer, rule_um), (groups, locs)) in &rest_limit {
        let _ = writeln!(
            s,
            "  ...and {groups} more net pair{} at exactly {} on {} \
             ({locs} location{}); pass --verbose for every instance.",
            if *groups == 1 { "" } else { "s" },
            provenance.rule_reference(*rule_um as f64 / 1000.0),
            friendly_layer(layer),
            if *locs == 1 { "" } else { "s" },
        );
    }
    write_heads_up(&mut s, &pr.heads_up);
    let _ = writeln!(s, "{summary}");
    s
}

// ── Net lint + straps + MCU resource conflicts ────────────────────────────────

/// Translate a [`NetLintReport`].
///
/// This covers the connectivity lint checks (`--lint` core), the boot strap-pin
/// lint, and the MCU resource-conflict check (`--resources`), since they all
/// report through this one report shape.
#[must_use]
#[allow(clippy::too_many_lines, reason = "one arm per lint check")]
pub fn plain_netlint(report: &NetLintReport) -> PlainReport {
    let mut out = PlainReport::new("connectivity");
    for f in &report.findings {
        let level = from_lint_sev(f.severity);
        let parts = join_refs(&f.refs);
        let net = f.nets.first().map_or("the net", |s| s.as_str());
        let (what, why, fix) = match f.check {
            // The DNP variant of either pull-up check: the resistor exists in
            // the layout but is do-not-populate, so the assembled board still
            // floats. The finding message is the precise statement (it names
            // the part); the generic "has no pull-up resistor" template would
            // contradict the schematic the reader is looking at.
            LintCheck::MissingI2cPullup | LintCheck::MissingSdPullup
                if f.message.contains(DNP_PULLUP_MESSAGE_MARKER) =>
            {
                let why = if f.message.contains(DNP_FITTED_NOTE_MARKER) {
                    "The design contains the pull-up, but the part is marked do-not-populate, so the boards that come back from assembly do not have it fitted. This run fits DNP parts, which is why other results can look healthy while the real line floats."
                } else {
                    "The design contains the pull-up, but the part is marked do-not-populate, so the boards that come back from assembly do not have it fitted and it is not counted here. The line floats unless something else defines its level."
                };
                (
                    f.message.clone(),
                    why.to_string(),
                    "Either fit the resistor in this build (remove the DNP mark, or name it as fitted), or, if leaving it off is deliberate, document what else defines the line's level.".to_string(),
                )
            }
            // The SPI-wired-socket variant: the lint's own message avoids the
            // SD-mode open-drain claim (false for SPI), so the plain surface
            // must not reinstate it.
            LintCheck::MissingSdPullup if f.message.contains(SPI_PULLUP_MESSAGE_MARKER) => (
                format!("The SD card line \"{net}\" has no pull-up resistor (socket appears wired for SPI mode)."),
                "This socket looks wired for SPI mode (DAT1/DAT2 are not driven by the host). In SPI mode the CMD pin is the host's DI line and DAT0 is the card's DO, which floats whenever the card is deselected; the SD specification still recommends pull-ups on these lines so they rest at a defined level.".to_string(),
                format!("Add a pull-up resistor (typically 10k to 100k ohm) from \"{net}\" to the card's supply rail, or record the omission as a deliberate choice for this SPI design."),
            ),
            LintCheck::MissingI2cPullup => (
                format!("The I2C data line \"{net}\" has no pull-up resistor."),
                "I2C is an \"open-drain\" bus: parts can only pull the line low, so it needs a resistor to a power rail to pull it back high. Without one the line floats, and the bus will not communicate reliably (or at all).".to_string(),
                format!("Add a pull-up resistor (typically 2.2k to 10k ohm) from \"{net}\" to its power rail. One resistor per signal line (SDA and SCL)."),
            ),
            LintCheck::MissingSdPullup => (
                format!("The SD card line \"{net}\" has no pull-up resistor."),
                "SD and microSD cards rely on host-side pull-ups: the CMD line is driven open-drain while the card is being identified, and the DAT lines float whenever the card is not driving them. Without a pull-up the line's level is undefined at exactly the moments the protocol depends on it, so card detection or initialization can hang in ways firmware cannot see. An MCU's internal pull-ups, if enabled, only help after firmware turns them on.".to_string(),
                format!("Add a pull-up resistor (typically 10k to 100k ohm) from \"{net}\" to the card's supply rail. The SD specification asks for one on CMD and on each DAT line."),
            ),
            LintCheck::FloatingControlPin => (
                format!("A control pin on {parts} is left floating (net \"{net}\" has nothing else on it)."),
                "Enable / reset / chip-select style pins decide whether a chip is on, held in reset, or selected. Left floating, the voltage is undefined, so the chip may randomly turn on/off or sit in reset. Behaviour will be flaky and may differ board to board.".to_string(),
                format!("Tie \"{net}\" to a defined level: a pull-up resistor to power (to hold it high/enabled) or a pull-down to ground (to hold it low), per the chip's datasheet, or drive it from a known output."),
            ),
            LintCheck::LedCurrentSanity => (
                format!("The LED string {parts} carries more current than a part in it is rated for."),
                "The series resistor sets an LED's current. Too small a resistor and the LED, the resistor or the pin driving them carries more than its rating, so it runs hot and fails early.".to_string(),
                "Raise the series resistor (the finding names the smallest value that keeps every rating): resistor = (supply voltage minus LED forward voltage) / target current, for roughly 1 to 20 mA. Put the LED's part number on it so its own datasheet ratings are judged.".to_string(),
            ),
            LintCheck::OutputContention => (
                format!("Two chip outputs ({parts}) are wired directly together on \"{net}\"."),
                "Both pins actively drive the wire. If one drives high while the other drives low, they fight: large current flows through the two chips, voltages land in a bad middle zone, and you can damage the outputs over time.".to_string(),
                "Only one push-pull output should drive a wire. Remove one driver, add a resistor / buffer / proper bus arrangement, or change one output to open-drain with a single pull-up so they can share the line safely.".to_string(),
            ),
            LintCheck::PinFunction => (
                f.message.clone(),
                "The pin names and pin types on this connection say what each pin does (transmit, fault flag, gate/source/drain). Wired the way they are, those roles do not work together, even though every wire connects.".to_string(),
                "Rewire the pins the way the finding above describes, then re-check against each part's datasheet.".to_string(),
            ),
            LintCheck::UnroutedPad
                if f.message
                    .starts_with(hauksbee_extract::PANEL_COPIES_MESSAGE_PREFIX) =>
            {
                (
                    f.message.clone(),
                    "A panel repeats one board several times with the same designators and net names, so the layout editor treats each name as one net across all the copies and draws unconnected lines between them. Those lines are the panel itself, not a missing route; open connections inside a single copy are reported separately.".to_string(),
                    "Nothing to route for the copies being separate. If the copies should share a net (a panel-wide ground), join them with copper; otherwise no action.".to_string(),
                )
            }
            LintCheck::UnroutedPad if f.severity == Severity::Low => (
                f.message.clone(),
                "This is a note about the state of the layout file (zones never filled, or leftover copper), not a connection a finished route left open.".to_string(),
                "Refill the zones or remove the stray copper in the layout editor, save, and re-check.".to_string(),
            ),
            LintCheck::UnroutedPad
                if f.message
                    .starts_with(hauksbee_extract::NOT_ROUTED_MESSAGE_PREFIX) =>
            {
                (
                    f.message.clone(),
                    "No track, via or copper pour joins any of the placed pads yet, so a board made from these files would connect nothing. The circuit checks ran on the netlist; the copper checks wait until it is routed.".to_string(),
                    hauksbee_extract::NOT_ROUTED_FIX.to_string(),
                )
            }
            LintCheck::UnroutedPad => (
                f.message.clone(),
                "The pads are on the right net, but no track, via or copper pour actually reaches them. A board made from these files would leave the connection open: the ratsnest line the layout editor shows here was never routed.".to_string(),
                "Route the missing connection (or extend the pour to it), refill the zones, and run the layout editor's DRC until it reports no unconnected items.".to_string(),
            ),
            // Below High is the part-identity comparison: the wiring agrees but a
            // reference names a different value or part in each file. Not gating.
            LintCheck::PcbSchematicMismatch if f.severity != Severity::High => (
                f.message.clone(),
                "The schematic and the layout disagree about which part this reference is. The layout's part is the one that gets built, and the check results describe it, so this is a thing to confirm rather than a fault.".to_string(),
                "Confirm which part is intended; if the schematic's is right, update the PCB from it (KiCad: Tools > Update PCB from Schematic) and re-check.".to_string(),
            ),
            LintCheck::PcbSchematicMismatch => (
                f.message.clone(),
                "The layout is what gets manufactured. When it wires a pin differently from the schematic, the board does not match the design that was reviewed: a pin that should reach ground or a signal is left off it, or joined to the wrong one.".to_string(),
                "In the layout editor, update the PCB from the schematic (KiCad: Tools > Update PCB from Schematic), re-route the changed connections, and re-check.".to_string(),
            ),
            // Low is the strap lint's abstention (see `straps::examine_strap`):
            // the level was not judged, so it must not read as a failure.
            LintCheck::StrapPin if f.severity == Severity::Low => (
                format!("The power-up level of a boot/strap pin on {parts} (net \"{net}\") could not be judged, so it is unchecked, not known to be wrong."),
                format!("Some chips read certain pins at reset to decide how to boot. This pin has pull resistors both ways, so its level is set by the divider they form, and hauksbee could not compute it. Detail: {}", f.message),
                format!("Confirm from the datasheet that the divider on \"{net}\" gives a clear high or low at reset, or supply what the detail names as missing so the check can judge it."),
            ),
            LintCheck::StrapPin => (
                format!("A boot/strap pin on {parts} (net \"{net}\") will not be held at the level the chip needs when it powers up."),
                "Some chips read certain pins at the instant they reset to decide how to boot (which mode, where to load code from). If that pin is at the wrong level (or wobbling, e.g. a clock signal sits on it), the chip can boot into the wrong mode or fail to start.".to_string(),
                format!("Hold \"{net}\" firmly at the level the datasheet wants during reset, usually with a pull-up or pull-down resistor, and keep fast/active signals off that pin until after boot."),
            ),
            // The header must match what the finding actually claims. High is a
            // genuine two-function contention; the low-severity form is a single
            // function occupying a pin-locked group (no contention, the board can
            // work as designed), and calling that "two functions ... at once"
            // was the SparkFun severity/wording overclaim.
            LintCheck::McuResourceConflict if f.severity == Severity::High => (
                format!("Two different functions need the same internal block of the microcontroller at once ({parts}). {}", f.message),
                "A microcontroller has a fixed number of internal blocks (timers/PWM channels, communication units, pin groups). Two board features have been wired to pins that share one block, so the chip physically cannot run both at the same time: one feature will not work.".to_string(),
                "Move one of the functions to a different pin that maps to a free internal block, or give up one of the two features. The chip's datasheet pin-mux table shows which pins share which block.".to_string(),
            ),
            // Medium: two functions share one hardware channel, but the chip has
            // a programmable engine that can drive either pin another way, so
            // the wiring is fine and the constraint lands on the firmware.
            LintCheck::McuResourceConflict if f.severity == Severity::Medium => (
                format!("Two functions share one hardware channel of the microcontroller, so firmware must generate one of them another way ({parts}). {}", f.message),
                "Two board features are wired to pins that share one internal hardware block, so both cannot use that block at the same time. This chip can also drive those pins from a programmable engine, so the board can still do both, but only if the firmware generates one of the two signals that way. Stock example code often does not.".to_string(),
                "No board change is needed if the firmware generates one of the two signals with the chip's programmable engine (the finding names it). Otherwise move one function to a pin on a free hardware channel.".to_string(),
            ),
            LintCheck::McuResourceConflict => (
                format!("A pin-locked peripheral group of the microcontroller is committed to a single function ({parts}). {}", f.message),
                "Some MCU peripherals only work on fixed pins. Wiring those pins to a different function is a legitimate design choice, but it permanently gives up the pin-locked peripheral for them; the netlist alone cannot tell whether that trade was intended.".to_string(),
                "If this arrangement is deliberate (the firmware drives the device another way), nothing needs to change. If the pin-locked peripheral was intended, move the device to that peripheral's full fixed pin set per the datasheet pin-mux table.".to_string(),
            ),
            LintCheck::DesignatorFootprintMismatch => (
                f.message.clone(),
                "The reference designator tells the BOM and assembly process what kind of part this is, while the footprint points at a different passive family. The board may route electrically, but the assembly house can load the wrong bin or stop for clarification.".to_string(),
                "Make the reference/value and footprint agree: use a resistor footprint for R parts, a capacitor footprint for C parts, and regenerate the BOM from the corrected design.".to_string(),
            ),
            LintCheck::ValuePackageSanity => (
                f.message.clone(),
                "The value is far outside what that small package can physically provide. This is usually a typo in the value or footprint, and it can turn into a wrong purchased part or an impossible BOM line.".to_string(),
                "Check the intended value against the selected package and manufacturer part. Either lower the value, pick a larger package, or correct the copied value field.".to_string(),
            ),
            LintCheck::PlaceholderValue => (
                f.message.clone(),
                "A placeholder passive value leaves the BOM and any physics checks without the actual resistor, capacitor, or inductor value. On charge-current, divider, or timing parts, that can hide the behavior that matters.".to_string(),
                "Replace the placeholder with the actual value before ordering or relying on simulation results.".to_string(),
            ),
            LintCheck::UncheckedMcu => (
                f.message.clone(),
                "The boot strap-pin check needs the part's model to know which pins are straps and what level they want at reset, so a strap-bearing MCU that is not in the model database is skipped. A clean lint result therefore does NOT mean its boot straps were verified, and a mis-strapped boot pin is latched by hardware at reset, before any firmware runs.".to_string(),
                "Check the boot/strap pins (e.g. BOOT0, or the ESP32 strapping pins) by hand against the datasheet, or supply a device model with --models-dir so hauksbee can check them automatically.".to_string(),
            ),
            // A decode that could not read the setting resistor says which part
            // and why; that is not a missing rail level.
            LintCheck::DeviceDecode if hauksbee_models::rating_basis::is_not_judged(&f.message) => (
                format!("A configuration resistor on {parts} (net \"{net}\") could not be read, so the setting it programs is unchecked, not known to be right."),
                format!("The part's current or mode is set by a resistor, and the design files do not let the tool read it as one (a duplicate designator, a designator that does not say resistor, or no resistor to ground). The verdict is not a pass. Detail: {}", f.message),
                "Make the resistor readable as the detail says (rename the duplicate, use a conventional R designator and a numeric value), then re-check.".to_string(),
            ),
            // The abstention is the check saying it did not judge, so it must
            // not read as the violation the arm below describes.
            LintCheck::OperatingEnvelope | LintCheck::DeviceDecode if f.is_abstention() => (
                format!("An operating-condition check on {parts} (net \"{net}\") could not be judged, so it is unchecked, not known to be wrong."),
                format!("The check needs the level the net actually sits at, or the part's identity, and the design files do not give it. Detail: {}", f.message),
                "Declare the net's level (a net_drive), bind the regulator that defines it, or supply what the detail names as missing so the check can judge it.".to_string(),
            ),
            LintCheck::DeviceDecode => (
                format!("A configuration pin on {parts} (net \"{net}\") decodes to the wrong setting. {}", f.message),
                // Deliberately generic: this template covers EVERY decoder in the
                // device_decode family (a PD sink's requested voltage, a charger's
                // safety-timer length, an eFuse's current limit). Narrating one
                // decoder specifically would explain an eFuse ILIM finding as a
                // USB-C voltage table: the right math wearing the wrong story.
                "Some chips read a resistor on a configuration pin and decode its value against a table in the datasheet to set a mode or a limit: which voltage a USB-PD sink requests, how long a charger's safety timer runs, how much current an eFuse passes. If the fitted resistor lands in the wrong band, the chip silently takes the wrong setting: every part is in spec and every wire connects, so a normal value or short check cannot see it. This finding applies the datasheet's own decode rule to the fitted value, which is why it works even when the part has no simulation model and the report lists it as unresolved.".to_string(),
                "Change the resistor so the decoded value matches the intent; the finding above names the fitted value, what it decodes to, and the value that would decode correctly. Then re-check against the part's own decode table and any min/max override note.".to_string(),
            ),
            LintCheck::OperatingEnvelope => (
                format!("A sourced operating condition is violated on {parts} at net \"{net}\". {}", f.message),
                "The model card quotes the part's recommended operating table and compares it with the full rail range the board can present. A nominal point inside the table is not enough when a battery or regulator tolerance crosses a published bound.".to_string(),
                "Move the pin to a compatible rail, or change the rail so its complete minimum-to-maximum range satisfies the quoted datasheet condition.".to_string(),
            ),
            LintCheck::BackPower => (
                format!("A pin on {parts} (net \"{net}\") sits in a higher voltage domain than the chip's own supply. {}", f.message),
                "Chip inputs have a small protection diode from the pin to the chip's supply. Pull the pin above that supply and the diode conducts: current flows from the higher rail through the chip into the lower rail. With the chip's supply off, the higher rail keeps the \"off\" domain half-powered (parts misbehave instead of resetting cleanly); powered on, the pin sits past its absolute-maximum rating unless it is specifically rated tolerant.".to_string(),
                format!(
                    "Pull the signal up to the chip's own supply rail instead, or level-shift between the domains. {}",
                    hauksbee_ir::vocabulary::back_power_tolerance_fix(hauksbee_extract::model_card_file_in(&f.message))
                ),
            ),
            LintCheck::RailConflict => (
                format!("More than one source sets the voltage of one rail (net \"{net}\"). {}", f.message),
                "A regulator output holds its rail at a fixed voltage. Tie two of them together, or join two differently named rails through a jumper, ferrite or sense resistor, and they fight: current flows between them according to tiny internal differences, one may back-drive the other, and the rail's real voltage and load are not what either part assumes.".to_string(),
                "Leave one source on the rail: remove or leave unfitted the extra regulator or jumper, or split the rail so each source feeds its own net.".to_string(),
            ),
            LintCheck::MissingDecoupling => (
                format!("The supply rail \"{net}\" looks short of decoupling capacitors. {}", f.message),
                "Every chip pulls its supply current in short bursts each time it switches, faster than the regulator can respond. A small capacitor (about 100 nF) beside each chip's supply pin supplies those bursts, and a larger bulk capacitor (microfarads) on the rail refills them. With too few, the rail bounces on every edge: logic can double-clock and comparators chatter, and it looks like a firmware fault. The design files do not say where parts sit, so this is a count of capacitors against chips, not a layout check.".to_string(),
                format!("Add about 100 nF from \"{net}\" to ground at each chip's supply pin, plus a bulk capacitor (for example 10 uF) where the rail enters the board or section."),
            ),
            // The abstention is the pass saying a rating was never really
            // tested, so it must not read as the overstress the arm below
            // describes.
            LintCheck::DcOverstress if hauksbee_models::rating_basis::is_not_judged(&f.message) => (
                "The datasheet-ratings check did not run to a result, so the parts it names were not judged: unverified, not known to be met, and the verdict is not a pass.".to_string(),
                format!("The ratings are judged at the board's solved resting state. Here the solve failed, or a named rail sat far below its level in it (nothing on the board powers its input, or a supply's model could not start), so every rating on it was held at a dead circuit and proves nothing. Detail: {}", f.message),
                "Declare the level of each input the board takes power from (a net_drive for the connector's rail), or give the part the note blames a working model, then re-check.".to_string(),
            ),
            LintCheck::DcOverstress if f.is_abstention() => (
                format!("A datasheet rating on {parts} could not be checked, so it is unverified, not known to be met."),
                format!("The board's resting state was solved and every rated part held to its ratings there, but this part's current runs into a part with no working model: one the design files give no usable value or model for, left open, or a trimmer whose setting they do not give. The simulation carried no current (or a guessed one) through it, so a clean reading here proves nothing. Detail: {}", f.message),
                "Give the part the detail names a model or a usable value (a card, a numeric resistance, the trimmer's setting), then re-check.".to_string(),
            ),
            // The resting draw is past what the rail's supply can give, so
            // the ideal rail's overstress figures are the solve's, not the
            // board's: a supply or model question, never a part overstress.
            LintCheck::DcOverstress
                if f.message.starts_with(crate::checks::rest_supply::OPENING) =>
            (
                format!("The board's resting draw is more than its supply can give, so the overstress computed on {} part(s) is not what the board sees. {}", f.refs.len(), f.message),
                "Every rail is solved as an ideal source unless a supply is declared, so it holds its level whatever the board pulls. Here the resting state pulls more current than a bench supply, USB port or module pin gives: the real rail sags first, and each part carries only its share. Held to what the supply can give, the parts named are within their ratings, so they are not reported as overstressed. A draw this large usually means a model or binding is wrong (a switch's resting state, a pin map, a part that is not fitted); if the real board does pull it, the board crowbars its rail.".to_string(),
                "Declare the real supply with its current limit, then trace the path into the first part named against its datasheet.".to_string(),
            ),
            // A part straight across a source with nothing limiting its
            // current: the simulated figure is the ideal source's, so none is
            // shown, but a missing series resistor or a part fitted the wrong
            // way is a real flaw the board must not pass clean over.
            LintCheck::DcOverstress
                if f.message.contains(hauksbee_bind::stress::UNBOUNDED_MARKER) =>
            (
                f.message.clone(),
                "Every rail is solved as an ideal source, so a part wired straight across one with nothing in series draws a current with no upper bound, and the simulation cannot print a believable figure for it. That is a real flaw when the part is fitted the wrong way round or has no series resistor (an LED with no resistor, a diode across a rail), and a modelling error when the part's model or value is wrong.".to_string(),
                "Check the part's orientation against its datasheet and the schematic. If it is meant to carry current from this rail, add a series resistor sized for the current it should draw; if its model or value is wrong, correct it. Then re-check.".to_string(),
            ),
            LintCheck::DcOverstress => (
                format!("A part is pushed past its datasheet rating as soon as the board powers up. {}", f.message),
                "This is the board's resting state, solved from the design: every part with a datasheet rating was held to it there. A part over its current, power or temperature rating in that state is overstressed the whole time the board is on; a transistor base or diode driven from a supply or logic output through too little resistance is the usual cause.".to_string(),
                "Find the path feeding the named part and add or enlarge the series resistance (a base resistor of a few kilohms for a small transistor) so the current stays inside its rating and the driver's.".to_string(),
            ),
            LintCheck::GroundIsland => (
                format!("A ground net is not connected to the board's ground. {}", f.message),
                "A net named as a ground is where the parts on it return their current. If nothing on the board joins it to the main ground, those parts have no return path: a signal referenced to it floats, audio and analog inputs pick up noise, and anything powered through it does not work. Split grounds are fine when they meet at one point through copper, a net tie, a 0 ohm link or a ferrite; this one meets the main ground nowhere.".to_string(),
                "Connect it to the board ground: use the same ground symbol, or add a net tie or 0 ohm link where the two should meet. If it is meant to be a separate, isolated domain, name it so (for example with ISO in the name).".to_string(),
            ),
            LintCheck::UnwiredConnector => (
                format!("A connector on {parts} is not wired to anything. {}", f.message),
                "Every signal pin of this connector sits on a net that touches only that pin. A label or wire was drawn on it, so a connection was intended, but nothing else on the board shares the net: whatever plugs in here receives and sends nothing. A mismatched net label is the usual cause.".to_string(),
                "Route the connector to the circuit it is meant to serve (check that its net labels match the ones at the other end), or remove it if it is not needed.".to_string(),
            ),
            LintCheck::LedReversed => (
                format!("An LED on {parts} is fitted backwards. {}", f.message),
                "An LED conducts and lights only when its anode is more positive than its cathode. Here the anode sits at ground and the cathode at a positive supply, so it is reverse-biased all the time: it never lights, and supplies above about 5 V can exceed its reverse-voltage rating.".to_string(),
                "Turn the LED round (anode toward the supply, cathode toward ground) in the schematic symbol and on the footprint.".to_string(),
            ),
            LintCheck::FloatingInput => (
                format!("Logic inputs on {parts} are left floating. {}", f.message),
                "A CMOS logic input has no level of its own: left unconnected it drifts with leakage and nearby signals, so the chip reads random values, and an input resting halfway between the rails makes the chip draw extra current. A no-connect mark in the schematic does not change that unless the datasheet says the pin has an internal pull resistor.".to_string(),
                "Tie each unused input to ground or to the chip's supply (directly, or through a resistor), or drive it from an output.".to_string(),
            ),
            LintCheck::SiblingMismatch => (
                format!("{parts} is wired differently from the identical copies of its cell. {}", f.message),
                "This part is one of many copies of the same sub-circuit, and every other copy is wired the same way except this one. A copy that deviates is rarely a design choice: it is usually a swapped pair of pins, a mislabelled net or a mirrored footprint in a single place, and each part looks fine when checked on its own.".to_string(),
                "Compare it pin by pin with a neighbouring copy and correct the schematic (and the footprint if the pads are mirrored). If the difference is deliberate, say so in a note next to it.".to_string(),
            ),
            LintCheck::UnlimitedBaseDrive => (
                format!("The base of {parts} is driven with nothing to limit the current. {}", f.message),
                "A transistor's base-emitter junction is a diode. Held a volt or more above the emitter with no resistor in series, it conducts whatever current the driver can supply, which overheats the transistor and pulls the supply or the driving chip down with it.".to_string(),
                "Put a resistor in series with the base (a few kilohms for a small-signal transistor) so the base current is set by the resistor.".to_string(),
            ),
            LintCheck::CapNoDcPath => (
                format!("A node on {parts} has no path for DC. {}", f.message),
                "A capacitor passes no DC, and a gate or amplifier input draws almost none. A node touched by nothing else has no defined voltage: leakage and the first power-up edge decide it, and charge stays on it with nowhere to go.".to_string(),
                "Add a resistor from the node to the supply or to ground (a bias or bleed resistor), or take the capacitor out if it is not needed.".to_string(),
            ),
            LintCheck::FloatingGate => (
                format!("A gate or select input on {parts} has no pull resistor. {}", f.message),
                "A microcontroller pin is high-impedance while it resets and boots, and a connector pin is open when the cable is out. An input driven only from such a pin has no level then, so a transistor can turn on or a switch can pick the wrong side for as long as the pin is undefined.".to_string(),
                "Add a pull-down or pull-up resistor (about 100 kilohms) to the level that is safe when the part is off.".to_string(),
            ),
            LintCheck::GateUnderdrive => (
                format!("Whether {parts} turns on depends on which part is fitted. {}", f.message),
                "The board names this transistor only by its class, with no part number, so its gate threshold is anywhere in the class's range. The gate is driven to a voltage inside that range: a low-threshold part turns on, a high-threshold one may stay off, and the circuit behaves differently depending on which one the assembler fits.".to_string(),
                "Put the manufacturer part number on the transistor so its real threshold is checked, or drive the gate further (a level shifter or a higher gate supply), or pick a logic-level part rated at this drive.".to_string(),
            ),
            LintCheck::RegulatorHeldOff => (
                format!("The regulator {parts} can never turn on. {}", f.message),
                "This regulator's datasheet says its enable pin turns the part off when nothing drives it. On this board nothing can raise that pin: it is unconnected, or only touches parts that cannot pull it up (a capacitor, a pull-down, a part that is not fitted). The output rail stays at 0 V and every part it powers is dead.".to_string(),
                "Pull the enable pin up to the regulator's input through a 10k to 100k resistor, or tie it straight to the input. If a microcontroller pin is meant to switch the rail, keep the resistor so the rail has a defined state before firmware runs.".to_string(),
            ),
            LintCheck::PowerPinWiring => (
                format!("The chip {parts} is not wired for power. {}", f.message),
                "Every chip needs its supply pins on a power rail and its ground pins on ground, or it has nothing to run on. Here the datasheet's own supply or ground pins are left unconnected, or the supply is tied to ground (or the ground to the rail), so the chip is dead, and any signal that another part drives into it can leak power backwards through its protection diodes.".to_string(),
                "Connect each supply pin to the right power rail and each ground pin to ground, as the datasheet's pin table shows, and put a decoupling capacitor at each supply pin.".to_string(),
            ),
            LintCheck::SupplyGroundSwapped => (
                format!("A connector pin on {parts} is wired to the wrong kind of net. {}", f.message),
                "The connector's own pin names say which pin is the supply and which is ground, but the board wires a supply pin to ground or a ground pin to a supply rail. Whatever is plugged in here is shorted or fed backwards, and the board never receives the supply it expects.".to_string(),
                "Swap the nets of the supply and ground pins so each reaches the rail its name says, in the schematic and on the layout.".to_string(),
            ),
            LintCheck::DiodeReversed => (
                format!("A diode on {parts} is fitted backwards across a rail. {}", f.message),
                "A TVS, Zener or clamp diode across a supply rail is meant to block until the voltage passes its breakdown value. With the anode on the rail and the cathode on ground it is forward-biased instead, so it conducts as soon as the rail is powered and holds the rail at about one diode drop.".to_string(),
                "Turn the diode round so its cathode is on the more positive net and its anode on the more negative one, in the schematic symbol and on the footprint.".to_string(),
            ),
            LintCheck::PolarisedCapReversed => (
                format!("A polarised capacitor on {parts} is fitted backwards. {}", f.message),
                "A solid tantalum capacitor tolerates only a small reverse voltage (about a tenth of its rating at room temperature) and an aluminium electrolytic none at all. Wrong polarity short-circuits or damages the part, and an electrolytic can open its pressure vent, which can take the rail down with it.".to_string(),
                "Turn the capacitor round so its + terminal is on the more positive net, in the schematic symbol and on the footprint.".to_string(),
            ),
            LintCheck::RailUnpowered => (
                format!("A supply rail's source is missing or only assumed. {}", f.message),
                "A rail nothing on the board drives is held at the voltage its name states so the parts on it can be checked. That is right when the rail comes in on a connector or test point. When its only source is a part marked do-not-fit (its regulator, or the ferrite or 0 ohm link from another rail), the real rail stays at 0 V and every part on it is unpowered, so no result for those parts stands.".to_string(),
                "Fit the part that sources the rail, or bring the rail in on a connector or test point. If the board is powered from outside, check that supply on its own.".to_string(),
            ),
            LintCheck::ControlHeldOff => (
                format!("The enable or reset pin of {parts} is held in its off state. {}", f.message),
                "This pin has a resistor to ground, and nothing else on its net can raise it: no pull-up, no GPIO or supervisor output, no switch to a supply. An enable that stays low never turns the part on, and a reset that stays low holds the part in reset so it never runs. The usual cause is a pull-up whose far end was wired to ground, or a pull-up that is not fitted.".to_string(),
                "Connect the pull resistor to the part's supply rail instead of ground, or drive the pin from a GPIO or a supervisor.".to_string(),
            ),
            LintCheck::DatasheetMismatch => (
                f.message
                    .rsplit_once(hauksbee_extract::REMEDY_MARKER)
                    .map_or_else(|| f.message.clone(), |(said, _)| said.trim().to_string()),
                "The Datasheet field is what the model extractor reads and what a reviewer opens, and its file name here is another part's. It is usually a link left over from the part the symbol was copied from. Nothing about the circuit is wrong, but a model drafted from that file would describe the wrong device.".to_string(),
                "Replace the Datasheet field with the datasheet for this part's own part number, then re-check.".to_string(),
            ),
            LintCheck::I2cBusLoading => (
                format!("The I2C line \"{net}\" has pull-ups, but they are mis-sized for the bus. {}", f.message),
                "I2C devices can only pull the line LOW; the resistor pulls it back HIGH. Too strong a pull-up and a device cannot sink enough current to make a valid low (the spec guarantees only 3 mA, or 20 mA in the fast-mode plus class), so reads fail intermittently. Too weak and the line rises too slowly for the bus speed (1000 ns at 100 kHz, 300 ns at 400 kHz, 120 ns at 1 MHz), corrupting edges as more devices load the bus. The capacitance used is a floor, so the real rise is slower than the one reported.".to_string(),
                "Size the pull-up between the two limits: above (rail voltage - 0.4 V) / 3 mA (so lows work), and low enough that the rise time fits the bus speed at the real bus capacitance. 2.2k to 4.7k suits most 3.3 V buses; name the net for its speed (for example SDA_400K) so it is judged against that speed.".to_string(),
            ),
            LintCheck::PullupIneffective if f.is_abstention() => (
                format!("The pull-up on \"{net}\" was not judged. {}", f.message),
                "A pull-up holds a line high only if its rail reaches the receiver's input-high level and the receiver's leakage does not drop it below that level through the resistor. The part's datasheet levels are what decide it, and here they were not available, so the check says it could not tell rather than guess.".to_string(),
                "Give the receiving part a model card with its VIL/VIH levels and input leakage, then re-check; until then verify the pull-up against the datasheet by hand.".to_string(),
            ),
            LintCheck::PullupIneffective => (
                format!("A pull-up resistor on \"{net}\" cannot hold the line high. {}", f.message),
                "A pull-up resistor only works if the voltage it pulls to is above the level the receiving chip needs to read a high (VIH), and stays above it once the resistor's voltage drop from the chip's input leakage is subtracted. A pull-up to a rail that is too low, a rail that never comes up, or a resistor too large for the load leaves the line idling where the chip reads low or in the band where its datasheet promises nothing.".to_string(),
                "Pull the line up to the receiver's own supply rail (or to a rail at least VIH(min) high), use a smaller resistor if the leakage is the problem, or add a level shifter between the two voltage domains.".to_string(),
            ),
            LintCheck::PowerDomainBackfeed => (
                format!("A switched-off chip ({parts}) is held half-alive through a signal pin (net \"{net}\"). {}", f.message),
                "A rail the microcontroller can switch off is only off if nothing else feeds the chips on it. Every chip input has a small protection diode to the chip's supply; pull the pin high from a rail that stays on, through a resistor or from another chip's output, and the diode conducts into the dead supply. The chip sits at about a diode drop instead of zero: it can brown out, run partly, never reset cleanly, and draw current that was never budgeted. A chip rated for signals with its supply off (an Ioff or partial-power-down part) is fine; most are not.".to_string(),
                "Pull the line up to the switched rail instead (so the pull-up goes off with the chip), or put a buffer or analog switch with a power-down rating in the line, or add a series resistor and park the driving pin low or high-impedance in firmware before switching the rail off. If the datasheet states the pin tolerates input voltage with the supply off, record it in the card's input_tolerance row.".to_string(),
            ),
            LintCheck::PowerDomainBusLockup => (
                format!("An I2C bus (net \"{net}\") stops working when a switched rail goes off ({parts}). {}", f.message),
                "An I2C bus needs its pull-up resistors powered, and every chip on it must let go of the lines when it is off. If the pull-ups hang on a rail the board can switch off while other chips keep using the bus, the lines float. If a chip on the bus loses its supply while the pull-ups stay up, its input protection diodes clamp the lines to about 0.6 V, which every other chip reads as a permanent low: the bus is dead until the chip is powered again.".to_string(),
                "Put the pull-ups on a rail that stays on as long as any chip on the bus does, and keep every chip on the bus powered together, or isolate the switched chip with a bus switch (or an I2C buffer) that opens when its rail is off. If the datasheet says the chip's lines are high-impedance with its supply off, record it in its card.".to_string(),
            ),
            LintCheck::AudioBusWiring => (
                format!("The digital-audio wiring on {parts} (net \"{net}\") disagrees with the pins' roles. {}", f.message),
                "An I2S or TDM audio bus has one bit clock and one frame clock, each driven by exactly one part (the master) and listened to by the rest, and data lines that each have a transmitter and a receiver. Two outputs on one line, a line nobody sends on, a clock on the wrong pin, a clock driven by two parts or none, or a codec missing the master clock it needs all leave the bus silent or noisy, and nothing about the board looks wrong to a power or level check.".to_string(),
                "Rewire the named pins so each signal reaches the pin with its own role (BCLK to BCLK, LRCLK to LRCLK, one part's data out to the other's data in), make exactly one part the clock master, and connect MCLK where the codec needs it.".to_string(),
            ),
        };
        // A finding that worked out its own remedy is acted on by that, not by
        // the check kind's generic advice.
        let fix = f.own_remedy().map_or(fix, str::to_string);
        out.push_for_part(
            level,
            what,
            why,
            fix,
            match f.refs.as_slice() {
                [part] => Some(part.as_str()),
                _ => None,
            },
        );
    }
    out
}

// ── Signal integrity ──────────────────────────────────────────────────────────

/// Translate a [`SiReport`]. Info notes are dropped (they are observations, not
/// problems); only real findings are translated.
#[must_use]
pub fn plain_si(report: &SiReport) -> PlainReport {
    let mut out = PlainReport::new("signal-integrity");
    for f in report.findings_only() {
        let level = from_si_sev(f.severity);
        let parts = join_refs(&f.refs);
        let (what, why, fix) = match f.check {
            SiCheck::CrystalLoadCap => (
                format!("The crystal's load capacitors ({parts}) do not match what the crystal wants."),
                "A crystal needs a specific load capacitance to oscillate at the right frequency. If the two small capacitors next to it are the wrong value, the clock can run slightly off-frequency, or in the bad case the oscillator will not start at all and the chip stays dead.".to_string(),
                "Set the two load capacitors to match the crystal: each cap is roughly (2 x the crystal's specified load capacitance) minus the board's stray capacitance (a few pF). Use the crystal datasheet's stated load value.".to_string(),
            ),
            SiCheck::I2cRiseTime => (
                format!("The I2C bus rise time is too slow ({}).", short_msg(&f.message)),
                "I2C lines are pulled high through a resistor against the bus capacitance, so they rise like a charging RC. If the pull-up is too weak for how much wiring/parts hang on the bus, the signal rises too slowly to be read correctly at the clock speed you want.".to_string(),
                "Use a stronger (smaller-value) pull-up resistor, reduce the number of devices / wire length on the bus, or slow the I2C clock to a mode the rise time can keep up with.".to_string(),
            ),
            SiCheck::AntennaKeepout => (
                format!("There is copper / ground under or near the antenna ({parts}) where it should be clear."),
                "Antennas need an empty \"keep-out\" zone around them to radiate properly. Copper or a ground pour intruding into that zone detunes the antenna, so wireless range drops sharply or the radio barely works.".to_string(),
                "Clear all copper and ground fill out of the antenna's keep-out region (the antenna's datasheet/footprint shows the exact zone). Re-route any tracks that cross it.".to_string(),
            ),
            SiCheck::UsbDiffPair => (
                format!("The USB data pair ({parts}) is mismatched ({}).", short_msg(&f.message)),
                "USB D+ and D- carry the same signal mirror-imaged, and the receiver compares them. If the two traces are different lengths (skew) or routed differently, the timing between them drifts and high-speed USB can become unreliable or fail to enumerate.".to_string(),
                "Route D+ and D- as a matched pair: same length (length-match / add a small serpentine to the shorter one), same width, kept close together and parallel, with a consistent reference ground beneath.".to_string(),
            ),
            SiCheck::ControlledImpedance => (
                format!("A trace that should be a controlled impedance is out of range ({}).", short_msg(&f.message)),
                "Fast signals like USB or Ethernet need their traces to present a specific impedance (for example 90 ohm differential for USB) so the signal does not reflect off the wire. If the trace width / spacing for your board stackup gives the wrong impedance, you get reflections, and the link can be marginal or fail at speed.".to_string(),
                "Adjust the trace width and pair spacing for your actual layer stackup so the estimated impedance hits the target; or have the fab build a controlled-impedance stackup to your spec.".to_string(),
            ),
            SiCheck::TraceAmpacity => (
                format!("A routed trace is too narrow for the current it has to carry ({}).", short_msg(&f.message)),
                "Copper can only carry so much current before it overheats: the IPC-2221 rule sets how wide a trace must be for a given current and temperature rise. A trace narrower than that runs hot, which can lift the copper, char the board, or in the worst case open up like a fuse.".to_string(),
                "Widen the trace to at least the IPC-2221 minimum width for its current (the message states the required width), pour the rail as a plane / copper fill, use heavier copper (2 oz), or split the current across more layers with stitching vias.".to_string(),
            ),
            SiCheck::InputCapRipple => (
                format!("A switching converter's input bulk capacitor is running over its ripple-current rating ({}).", short_msg(&f.message)),
                "The input capacitor of a buck converter has to supply the chopped, pulsing input current, so it carries a large RMS ripple current. Every capacitor is rated for a maximum ripple current (its internal resistance turns that current into heat); running past the rating overheats the cap from the inside and ages it out early, so the converter fails months or years sooner than it should.".to_string(),
                "Use a capacitor with a higher ripple-current rating, or split the bulk across several caps in parallel so each carries a share, or add low-ESR ceramics across it to take the high-frequency ripple. The message gives the actual vs rated ripple current.".to_string(),
            ),
        };
        out.push(level, what, why, fix);
    }

    // Promote ACTIONABLE info notes into "Heads up" rather than dropping them.
    // An info note is actionable when it reports a real off-target value (the
    // controlled-impedance "+N% from target" note), as opposed to a
    // within-tolerance "ok" or a "no judgement" observation.
    for f in info_only(report) {
        if let Some(note) = actionable_info_note(f) {
            out.push_note(note);
        }
    }
    out
}

/// Info-severity findings (the auditable computed values), in report order.
fn info_only(report: &SiReport) -> impl Iterator<Item = &SiFinding> {
    report
        .findings
        .iter()
        .filter(|f| f.severity == SiSeverity::Info)
}

/// A fully-glossed actionable info note, or `None` if the note is a pure
/// observation that should not nag the user. The honest rule: a note is
/// actionable iff it expresses a deviation from a target (its message says
/// "from target") and is not a within-tolerance "ok".
///
/// The controlled-impedance note gets the same what / why / what-to-do
/// treatment a finding does: the honest number ("Zdiff ~ 173 ohm [target 90]")
/// plus the translation a novice needs to act on it.
fn actionable_info_note(f: &SiFinding) -> Option<HeadsUp> {
    let m = &f.message;
    let off_target = m.contains("from target") && !m.contains("within");
    if !off_target {
        return None;
    }
    // Name the actual copper: the impedance info notes carry their NETS (refs
    // are empty), so a refs-only fallback would point at nothing.
    let affected = if !f.refs.is_empty() {
        join_refs(&f.refs)
    } else if !f.nets.is_empty() {
        f.nets.join(" / ")
    } else {
        String::from("the flagged trace")
    };
    // The measured substance, kept WHOLE: everything up to the "- info only
    // (...)" caveat, which the gloss below restates in plain words. Running
    // this through `short_msg`'s 160-char cut lost the target/deviation clause
    // mid-parenthesis. The message leads with the net names; the sentence
    // already names them via `affected`, so drop that duplicate prefix.
    let core = m.split(" - info only").next().unwrap_or(m).trim();
    let core = core
        .strip_prefix(&format!("{affected}:"))
        .unwrap_or(core)
        .trim();
    // "pair" only when it IS one: the single-ended estimates name one net, and
    // they are a different animal (a 50 ohm RF / antenna feed, not a USB or
    // Ethernet link), so they get their own why and what-to-do below. Reusing
    // the differential-pair advisory told the reader of a one-net RF trace to
    // check whether "this pair" is a USB/Ethernet/HDMI link.
    let is_pair = f.refs.len() >= 2 || f.nets.len() >= 2;
    Some(match f.check {
        SiCheck::ControlledImpedance if is_pair => HeadsUp::glossed(
            format!("A high-speed trace pair ({affected}) is off its impedance target ({core})."),
            "Fast links like USB (90 ohm differential) or Ethernet need their traces to \
             present a specific impedance so the signal does not reflect off the wire and \
             smear. This is a computed estimate, flagged only because the board did not \
             formally declare a controlled-impedance stackup; it is informational, not a \
             confirmed failure, but a value well off target on a real high-speed net can \
             make the link marginal or fail to enumerate.",
            "If this pair is NOT a high-speed link (USB/Ethernet/HDMI), you can ignore it. \
             If it is: adjust the trace width and pair spacing for your actual layer \
             stackup so the estimate lands near the target, or ask your fab to build a \
             controlled-impedance stackup to spec.",
        ),
        SiCheck::ControlledImpedance => HeadsUp::glossed(
            format!("A single-ended trace ({affected}) is off its impedance target ({core})."),
            "A 50 ohm single-ended line is the convention for an RF or antenna feed: the \
             trace has to present that impedance end to end or part of the signal reflects \
             back instead of reaching the antenna, costing range and sensitivity. This is a \
             computed estimate, flagged only because the board did not formally declare a \
             controlled-impedance stackup.",
            "If this is not a controlled-impedance RF path, this estimate is informational. \
             If it is: widen or narrow the trace for your actual layer stackup so the \
             estimate lands near 50 ohm, or ask your fab to build a controlled-impedance \
             stackup to spec.",
        ),
        _ => HeadsUp::note(format!("{core} ({affected}).")),
    })
}

/// Keep an embedded expert message short enough to sit inside a plain sentence
/// without dumping a whole table row.
fn short_msg(msg: &str) -> String {
    let m = msg.trim();
    // Split only on clause boundaries ("; ", ". "), NOT a bare ".", so decimal
    // numbers like "0.200 mm" and "Zdiff ~ 171 ohm [target 90 ohm]" survive intact,
    // a bare "." chopped the controlled-impedance note down to "W~0".
    let first = m.split("; ").next().unwrap_or(m).trim();
    let first = first.split(". ").next().unwrap_or(first).trim();
    if first.chars().count() <= 160 {
        return first.to_string();
    }
    // Cut back to a word boundary so the summary never ends mid-word, then mark
    // the elision with an ellipsis.
    let truncated: String = first.chars().take(160).collect();
    let cut = truncated
        .rsplit_once(' ')
        .map_or(truncated.as_str(), |(h, _)| h);
    format!("{}…", cut.trim_end())
}

// ── Stress / datasheet-rating faults ──────────────────────────────────────────

/// Translate the stress-monitor fault events (over-current, over-voltage, etc.)
/// raised while co-simulating against datasheet ratings.
#[must_use]
pub fn plain_faults(faults: &[FaultEvent]) -> PlainReport {
    let mut out = PlainReport::new("electrical stress");
    for f in faults {
        let level = if f.destroyed {
            PlainLevel::Serious
        } else {
            PlainLevel::Warning
        };
        let c = &f.component;
        let (what, why, fix) = match f.kind {
            FaultKind::Overcurrent => (
                format!(
                    "{c} is carrying about {} of continuous current, past its {} limit.",
                    format_amps(f.value.abs()),
                    format_amps(f.limit.abs()),
                ),
                "Sustained current above a part's rating heats it up. The part runs hot, drifts out of spec, ages fast, and eventually fails (a resistor can char, a trace can burn open).".to_string(),
                "Lower the current (raise the relevant resistance, split the load, add parts in parallel) or use a part rated for this current. If a resistor, also pick a higher-wattage package.".to_string(),
            ),
            FaultKind::SurgeCurrent => (
                format!(
                    "{c} sees a brief current spike of about {}, past its {} surge limit.",
                    format_amps(f.value.abs()),
                    format_amps(f.limit.abs()),
                ),
                "Even short spikes (at power-on, or when charging a big capacitor) can exceed what a part survives. Repeated surges weaken or pop the part even if the average current looks fine.".to_string(),
                "Add inrush limiting: a series resistor / NTC, a soft-start, or a part rated for the surge. Reducing the capacitance being charged through it also helps.".to_string(),
            ),
            FaultKind::Overpower => (
                format!(
                    "{c} is dissipating about {}, past its {} power rating.",
                    format_watts(f.value.abs()),
                    format_watts(f.limit.abs()),
                ),
                "Power turns into heat. Over its rating the part overheats: a resistor scorches, a regulator shuts down or fails, and nearby parts get cooked too.".to_string(),
                "Reduce the power (less current or less voltage across it), spread it over more parts, or move to a larger / higher-wattage package with better heat handling.".to_string(),
            ),
            FaultKind::Overvoltage => (
                format!(
                    "{c} has about {} across it, past its {} voltage rating.",
                    format_volts(f.value.abs()),
                    format_volts(f.limit.abs()),
                ),
                "Every part has a maximum safe voltage. Above it the insulation/junction can break down (a capacitor can short or vent, a semiconductor can be punched through), often instantly and permanently.".to_string(),
                "Use a part rated for at least this voltage (with margin), or lower the voltage it sees (clamp, divider, or correct the supply/rail it is on).".to_string(),
            ),
            FaultKind::GateOvervoltage => (
                format!(
                    "{c} has about {} between its gate and source, past its {} gate-source rating.",
                    format_volts(f.value.abs()),
                    format_volts(f.limit.abs()),
                ),
                "A MOSFET's gate is a thin insulating oxide. Past its gate-source absolute maximum the oxide can rupture, leaving the gate leaky or shorted to the channel, often permanently.".to_string(),
                "Clamp the gate below the rating (a lower-voltage Zener from gate to source, or a divider on the gate drive), or use a part rated for this gate voltage.".to_string(),
            ),
            FaultKind::ReverseBias => (
                format!("{c} (a polarised part) is being driven backwards (reverse voltage of about {}).", format_volts(f.value.abs())),
                "Polarised parts like electrolytic and tantalum capacitors only tolerate voltage one way round. Reverse voltage damages them, and a tantalum can fail short and catch fire.".to_string(),
                "Check the part's orientation/footprint against the schematic and flip it if it is backwards, or move it to a net where its polarity is correct. If the net genuinely swings both ways, use a non-polarised part.".to_string(),
            ),
            FaultKind::PinOvercurrent => (
                format!(
                    "A single pin of {c} is sourcing/sinking about {}, past its {} per-pin limit.",
                    format_amps(f.value.abs()),
                    format_amps(f.limit.abs()),
                ),
                "Each chip pin can only drive so much current. Pushing more overheats that pin's internal driver, droops its output voltage, and can permanently damage the pin or the chip.".to_string(),
                "Drive less current per pin: add a series resistor, use a transistor/driver to handle the load instead of the pin directly, or spread the load across multiple pins.".to_string(),
            ),
            FaultKind::Short => (
                format!("{c} is involved in a short (two nets bridged together)."),
                "Two connections that should be separate are tied together, so they are forced to the same voltage. If one is a power rail this can dump large current through the short and overheat parts.".to_string(),
                "Find and remove the bridge (see the copper-spacing / DRC report for the exact spot), or, if the connection is intended, make them one named net deliberately.".to_string(),
            ),
            FaultKind::Overtemperature => (
                format!( "{c} reaches about {}, past its {} junction-temperature limit.", format_celsius(f.value), format_celsius(f.limit)),
                "The power this part dissipates raises its internal (junction) temperature above ambient. Past its rated junction temperature it degrades fast, drifts out of spec, and eventually fails; a hot part also heats its neighbours.".to_string(),
                "Reduce the power it dissipates, move to a package with lower thermal resistance, add copper pour / a heatsink to carry heat away, or improve airflow / lower the ambient. The estimate assumes still air, so real cooling helps.".to_string(),
            ),
        };
        // Far past its rating the figure is an idealised model's number, not
        // the part's: say so in the one stress wording, with no number.
        let what = if f.is_far_past_rating() {
            format!("{}.", f.statement())
        } else {
            what
        };
        let why = if f.destroyed {
            format!("{why} In this simulation the part was pushed past the point of failure.")
        } else {
            why
        };
        out.push(level, what, why, fix);
    }
    out
}

// ── Unit formatting (engineer-friendly but readable) ──────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::{DrcFinding, Item, ItemKind, LintFinding, SiFinding, ViolationKind};

    fn dc_overstress(part: &str, ma: &str) -> hauksbee_extract::LintFinding {
        hauksbee_extract::LintFinding {
            check: hauksbee_extract::LintCheck::DcOverstress,
            severity: hauksbee_extract::Severity::High,
            message: format!("{part} is overstressed at power-up: {part}_q2 passes {ma} mA"),
            refs: vec![part.to_string()],
            nets: Vec::new(),
        }
    }

    #[test]
    fn findings_that_repeat_across_parts_print_once() {
        let many = hauksbee_extract::NetLintReport {
            findings: (1..=5)
                .map(|n| dc_overstress(&format!("IC{n}"), "234"))
                .chain(std::iter::once(dc_overstress("IC9", "99")))
                .collect(),
        };
        let report = plain_netlint(&many);
        assert_eq!(
            report.findings.len(),
            6,
            "the report data keeps every finding"
        );
        let text = report.render();
        assert!(
            text.contains("1. [SERIOUS] 5 parts like IC1, IC2, IC3 and 2 more: "),
            "{text}"
        );
        assert!(
            text.contains("2. "),
            "the odd current stays its own finding: {text}"
        );
        assert!(!text.contains("3. "), "{text}");
        assert!(text.contains("IC9 is overstressed"), "{text}");
    }

    fn drc_short() -> DrcFinding {
        let mut f = crate::result::test_finding(
            ViolationKind::Short,
            ("+5V", "GND"),
            (12.3, 45.6),
            (-0.05, 0.2),
        );
        f.item_b = Item {
            kind: ItemKind::Pad,
            net: 2,
            owner: "U1".to_string(),
            uuid: String::new(),
        };
        f
    }

    fn clearance(net_a: &str, net_b: &str, gap_mm: f64) -> DrcFinding {
        let mut f = drc_short();
        f.kind = ViolationKind::Clearance;
        f.net_a_name = net_a.to_string();
        f.net_b_name = net_b.to_string();
        f.gap_mm = gap_mm;
        f
    }

    fn drc_report(findings: Vec<DrcFinding>) -> DrcReport {
        DrcReport {
            clearance_mm: 0.2,
            primitive_count: 2 * findings.len().max(1),
            findings,
            version_warning: None,
            zone_pad_overlaps_suppressed: Some(0),
            excluded: Vec::new(),
            clearance_unjudged: hauksbee_extract::UnjudgedClearance::default(),
        }
    }

    #[test]
    fn inconclusive_verdict_keeps_the_heads_up_pointer() {
        let mut r = PlainReport::new("signal-integrity");
        r.heads_up.push(HeadsUp::note("USB pair off target"));
        assert_eq!(
            r.verdict(),
            "No signal-integrity failures, but 1 thing worth a look (see below)."
        );
        assert!(r.render().contains("USB pair off target"));
        r.unmodelled_critical = vec!["Q1".to_string()];
        let v = r.verdict();
        assert!(v.starts_with("INCONCLUSIVE"), "{v}");
        assert!(v.contains("1 thing worth a look (see below)"), "{v}");
        r.push(PlainLevel::Warning, "w".into(), "why".into(), "fix".into());
        let rendered = r.render();
        assert!(rendered.contains("1 issue found"), "{rendered}");
        assert!(rendered.contains("INCONCLUSIVE"), "{rendered}");
    }

    /// Once the report has said above every section which parts were not
    /// judged, a section that lists findings does not point back at it again;
    /// a section with nothing to show still does, so it never reads as clean.
    #[test]
    fn a_section_with_findings_does_not_repeat_what_the_top_said() {
        let mut r = PlainReport::new("connectivity");
        r.unmodelled_critical = vec!["Q1".to_string()];
        r.inconclusive_said_above = true;
        let empty = r.render();
        assert!(empty.contains(INCONCLUSIVE_SEE_TOP), "{empty}");
        r.push(PlainLevel::Warning, "w".into(), "why".into(), "fix".into());
        let listed = r.render();
        assert!(listed.contains("1 issue found"), "{listed}");
        assert!(!listed.contains(INCONCLUSIVE_SEE_TOP), "{listed}");
        // Said nowhere above: the section keeps the full sentence beside its findings.
        r.inconclusive_said_above = false;
        assert!(r.render().contains("INCONCLUSIVE"), "{}", r.render());
    }

    /// A failing run lists its failures, then names what it did not judge on
    /// one line of its own; the failure sentence does not carry it too. A
    /// passing or merely inconclusive run has no such line.
    #[test]
    fn a_failing_triage_names_what_was_not_judged_once_after_its_failures() {
        let triage = OrderTriage {
            do_not_order: vec![TriageEntry::new(
                "lint",
                "rail_conflict",
                "two drivers",
                None,
            )],
            ..OrderTriage::default()
        };
        let said =
            "1 key part has no usable model (Q1), so the rest of the board is not vouched for.";
        let failing = crate::result::RunVerdict {
            verdict: "fail",
            reason: Some(format!(
                "1 blocking problem(s), first: two drivers; next: remove one.{} {said}",
                hauksbee_ir::vocabulary::NOT_JUDGED_JOINER.trim_end()
            )),
            conditions: Vec::new(),
            partial: false,
            not_judged: Some(said.to_string()),
            inspect: 0,
            unorderable: None,
        };
        let out = triage.render(&failing);
        assert_eq!(out.matches(said).count(), 1, "{out}");
        assert!(
            out.contains(&format!("NOT JUDGED: {said}")),
            "the line is labelled: {out}"
        );
        assert!(
            out.contains(&format!(
                "NOT JUDGED: {said} {}",
                hauksbee_ir::vocabulary::DRAFT_MODELS_NEXT_STEP
            )),
            "the line says how to judge the rest: {out}"
        );
        assert!(
            out.find("DO NOT ORDER:") < out.find("NOT JUDGED:"),
            "failures first, then what was not judged: {out}"
        );
        // With nothing listed, the headline carries the failure and not the
        // not-judged sentence, which keeps its own line.
        let headline = OrderTriage::default().render(&failing);
        assert!(
            headline.contains("NOT OK TO ORDER: this run failed. 1 blocking problem(s)"),
            "{headline}"
        );
        assert_eq!(headline.matches(said).count(), 1, "{headline}");
        assert!(!headline.contains("Also:"), "{headline}");
        // Controls: a run that is not failing, or fails with every part modelled.
        let modelled = crate::result::RunVerdict {
            reason: Some("1 blocking problem(s), first: two drivers; next: remove one.".into()),
            not_judged: None,
            ..failing
        };
        assert!(!triage.render(&modelled).contains("NOT JUDGED"));
        assert!(!triage
            .render(&crate::result::RunVerdict::default())
            .contains("NOT JUDGED"));
    }

    #[test]
    fn drc_short_is_serious_and_clearance_is_a_warning() {
        let plain = plain_drc(&drc_report(vec![drc_short()]));
        let f = &plain.findings[0];
        assert_eq!(f.level, PlainLevel::Serious);
        assert!(f.what.contains("+5V") && f.what.contains("GND"));
        assert!(!f.why.is_empty() && !f.fix.is_empty());
        assert_eq!(plain.serious_count(), 1);
        assert!(plain.verdict().contains("1 serious"));

        let plain = plain_drc(&drc_report(vec![clearance("+5V", "GND", 0.12)]));
        assert_eq!(plain.findings[0].level, PlainLevel::Warning);
        assert_eq!(plain.serious_count(), 0);
        assert!(plain.verdict().contains("none serious"));
    }

    #[test]
    fn plain_structured_drc_groups_and_uses_at_limit_wording() {
        use crate::result::DrcStructured;
        let at_limit = || clearance("SIG_A", "SIG_B", 0.2);
        let report = drc_report(vec![at_limit(), at_limit(), at_limit()]);
        let plain = plain_drc_structured(&DrcStructured::from_report(&report));
        assert_eq!(plain.findings.len(), 1, "{:?}", plain.findings);
        let f = &plain.findings[0];
        assert_eq!(f.level, PlainLevel::Warning);
        assert!(
            f.what.contains("at minimum clearance (no margin)"),
            "{}",
            f.what
        );
        assert!(!f.what.to_lowercase().contains("below"), "{}", f.what);
        assert!(f.what.contains("3 locations"), "{}", f.what);

        let below = drc_report(vec![clearance("SIG_A", "SIG_B", 0.10)]);
        let below_plain = plain_drc_structured(&DrcStructured::from_report(&below));
        assert!(below_plain.findings[0]
            .what
            .to_lowercase()
            .contains("below"));
    }

    #[test]
    fn condensed_plain_drc_keeps_three_full_findings_and_aggregates_the_rest() {
        use crate::result::DrcStructured;
        // 0.10 mm is under the default-rule spacing floor, so every group is an
        // issue rather than an advisory note.
        let findings: Vec<_> = (0..50)
            .map(|i| clearance(&format!("NET_{i}"), &format!("NET_{}", i + 100), 0.10))
            .collect();
        let report = drc_report(findings);
        let st = DrcStructured::from_report(&report);

        let condensed = render_drc_condensed(&st, false);
        assert_eq!(
            condensed.matches("Why it matters:").count(),
            3,
            "{condensed}"
        );
        assert!(
            condensed.contains("47 more net pairs like this"),
            "{condensed}"
        );
        assert!(condensed.contains("50 issues found"), "{condensed}");
        assert!(
            condensed
                .trim_end()
                .lines()
                .last()
                .is_some_and(|l| l.starts_with("Summary:") && l.contains("50 net pair(s)")),
            "{condensed}"
        );

        let verbose = render_drc_condensed(&st, true);
        assert_eq!(verbose.matches("Why it matters:").count(), 50);

        let mut with_short = report;
        with_short.findings.push(drc_short());
        let s2 = render_drc_condensed(&DrcStructured::from_report(&with_short), false);
        assert!(
            s2.contains("are touching"),
            "the short is never condensed:\n{s2}"
        );
    }

    #[test]
    fn empty_drc_refuses_a_healthy_claim_when_no_copper_was_checked() {
        let plain = plain_drc(&DrcReport {
            clearance_mm: 0.2,
            ..Default::default()
        });
        assert!(!plain.verdict().to_lowercase().contains("healthy"));
        assert!(plain.render().contains("No copper was checked"));
    }

    #[test]
    fn every_lint_check_maps_to_a_template() {
        for check in LintCheck::ALL {
            let report = NetLintReport {
                findings: vec![LintFinding {
                    check,
                    severity: Severity::Medium,
                    message: "expert message goes here".to_string(),
                    refs: vec!["U3".to_string(), "C1".to_string()],
                    nets: vec!["SDA".to_string()],
                }],
            };
            let plain = plain_netlint(&report);
            assert_eq!(plain.findings.len(), 1, "{check:?} produced no finding");
            let f = &plain.findings[0];
            assert!(
                !f.what.is_empty() && !f.why.is_empty() && !f.fix.is_empty(),
                "{check:?} has an empty template field"
            );
        }
    }

    /// The back-power fix says how to record a tolerant pin (an
    /// `input_tolerance` envelope row on the card), names the card file when
    /// the message carries one, and the `--json` hint reads the same wording.
    #[test]
    fn back_power_fix_names_the_input_tolerance_row_and_the_card_file() {
        let fix = |message: &str| {
            let report = NetLintReport {
                findings: vec![LintFinding {
                    check: LintCheck::BackPower,
                    severity: Severity::Medium,
                    message: message.to_string(),
                    refs: vec!["U3".to_string()],
                    nets: vec!["SIG".to_string()],
                }],
            };
            plain_netlint(&report).findings[0].fix.clone()
        };
        let generic = fix("U3 pad 2 is pulled up.");
        for needle in ["input_tolerance", "max_v", "abs_max_v", "basis"] {
            assert!(generic.contains(needle), "{generic}");
        }
        assert!(!generic.contains("cards/"), "{generic}");
        let named = fix(&format!(
            "U3 pad 2 is pulled up.{}cards/sense_amp.toml",
            hauksbee_extract::MODEL_CARD_MESSAGE_MARKER
        ));
        assert!(named.contains("`cards/sense_amp.toml`"), "{named}");
        let json_hint = crate::result::lint_fix_hint(LintCheck::BackPower, Severity::Medium)
            .expect("back power has a hint");
        assert!(
            json_hint.contains(&hauksbee_ir::vocabulary::back_power_tolerance_fix(None)),
            "{json_hint}"
        );
    }

    /// A Low strap finding is the lint's abstention: worded as unchecked in the
    /// plain and JSON renderings, while a judged failure still reads as one.
    #[test]
    fn strap_abstention_reads_as_unchecked_not_as_a_failure() {
        for (severity, unchecked) in [(Severity::Low, true), (Severity::Medium, false)] {
            let report = NetLintReport {
                findings: vec![LintFinding {
                    check: LintCheck::StrapPin,
                    severity,
                    message: "no vdd pad".to_string(),
                    refs: vec!["U1".to_string()],
                    nets: vec!["GPIO46".to_string()],
                }],
            };
            let what = &plain_netlint(&report).findings[0].what;
            assert_eq!(what.contains("unchecked"), unchecked, "{what}");
            assert_eq!(what.contains("will not be held"), !unchecked, "{what}");
            let hint = crate::result::lint_fix_hint(LintCheck::StrapPin, severity).unwrap();
            assert_eq!(hint.contains("missing model data"), unchecked, "{hint}");
        }
    }

    /// An operating-envelope abstention says the level was not known; the plain
    /// text must not tell a reader a sourced condition "is violated".
    #[test]
    fn envelope_abstention_reads_as_unchecked_not_as_a_violation() {
        for (message, abstained) in [
            (
                "operating envelope on net 'N1' abstained: declare the level",
                true,
            ),
            (
                "U1 role vdd on 'N1' presents 4.75..5.25 V, outside the envelope",
                false,
            ),
        ] {
            let report = NetLintReport {
                findings: vec![LintFinding {
                    check: LintCheck::OperatingEnvelope,
                    severity: Severity::Low,
                    message: message.to_string(),
                    refs: vec!["U1".to_string()],
                    nets: vec!["N1".to_string()],
                }],
            };
            let what = &plain_netlint(&report).findings[0].what;
            assert_eq!(what.contains("unchecked"), abstained, "{what}");
            assert_eq!(what.contains("is violated"), !abstained, "{what}");
        }
    }

    fn si_info(check: SiCheck, message: &str) -> SiReport {
        SiReport {
            findings: vec![SiFinding {
                check,
                severity: SiSeverity::Info,
                message: message.to_string(),
                refs: vec!["J1".to_string()],
                nets: vec!["/USB_D+".to_string(), "/USB_D-".to_string()],
            }],
        }
    }

    #[test]
    fn si_info_notes_are_never_findings_and_promote_only_when_off_target() {
        let plain = plain_si(&si_info(
            SiCheck::ControlledImpedance,
            "/USB_D+ / /USB_D-: Zdiff ~ 171 ohm [target 90 ohm USB]: estimate +90% from target - info only (defaulted stackup)",
        ));
        assert!(plain.findings.is_empty());
        assert_eq!(plain.heads_up.len(), 1);
        let verdict = plain.verdict().to_lowercase();
        assert!(
            verdict.contains("worth a look") && !verdict.contains("no signal-integrity problems")
        );
        assert!(plain.render().contains("171 ohm"));

        let plain = plain_si(&si_info(
            SiCheck::ControlledImpedance,
            "Zdiff ~ 92 ohm vs target 90 ohm (+2%, within +-10%) - ok",
        ));
        assert!(plain.heads_up.is_empty());

        let plain = plain_si(&si_info(SiCheck::CrystalLoadCap, "computed CL = 18 pF"));
        assert!(plain.findings.is_empty());
        assert!(plain.verdict().to_lowercase().contains("healthy"));
    }

    #[test]
    fn fault_severity_follows_destruction() {
        let fault = |destroyed| FaultEvent {
            component: "C1".to_string(),
            kind: FaultKind::Overvoltage,
            value: 16.0,
            limit: 6.3,
            t: 0.01,
            destroyed,
        };
        let plain = plain_faults(&[fault(true)]);
        assert_eq!(plain.findings[0].level, PlainLevel::Serious);
        assert!(plain.findings[0].what.contains("C1"));
        assert_eq!(
            plain_faults(&[fault(false)]).findings[0].level,
            PlainLevel::Warning
        );
    }

    /// Faulty: R1 computed at 24.95 kW against 250 mW (a near-zero resistor
    /// on an ideal rail) is stated far past its rating, with no figure.
    /// Legal control: 2.5 W against it prints the figure as before.
    #[test]
    fn an_absurd_stress_is_stated_far_past_without_its_figure() {
        let fault = |value| FaultEvent {
            component: "R1".to_string(),
            kind: FaultKind::Overpower,
            value,
            limit: 0.25,
            t: 0.0,
            destroyed: false,
        };
        let absurd = &plain_faults(&[fault(24_950.0)]).findings[0].what;
        assert!(
            absurd.starts_with("R1 dissipates far more than its 250 mW power rating")
                && !absurd.contains("24.95")
                && !absurd.contains("24950"),
            "{absurd}"
        );
        let real = &plain_faults(&[fault(2.5)]).findings[0].what;
        assert!(real.contains("2.5 W") && !real.contains("far"), "{real}");
    }

    #[test]
    fn findings_render_serious_first() {
        let mut report = NetLintReport::default();
        for (check, severity) in [
            (LintCheck::LedCurrentSanity, Severity::Low),
            (LintCheck::MissingI2cPullup, Severity::High),
        ] {
            report.findings.push(LintFinding {
                check,
                severity,
                message: String::new(),
                refs: vec!["R1".to_string()],
                nets: vec!["LED".to_string()],
            });
        }
        let rendered = plain_netlint(&report).render();
        assert!(rendered.find("SERIOUS").unwrap() < rendered.find("note").unwrap());
    }

    fn structured_short(net_b: &str, severity: &str, x: f64) -> crate::result::DrcShort {
        crate::result::DrcShort {
            net_a: "GND".into(),
            net_b: net_b.into(),
            layer: "F.Cu".into(),
            gap_mm: 0.0,
            loc_mm: [x, 2.0],
            severity: severity.into(),
            plain: format!("GND shorts {net_b}"),
            fix: "separate the copper".into(),
            refs: Vec::new(),
        }
    }

    fn clearance_report(violations: usize, shorts: u32) -> crate::result::DrcStructured {
        crate::result::DrcStructured {
            clearance_rule_mm: 0.2,
            primitive_count: 20,
            stale_fills: Vec::new(),
            shorts: (0..shorts)
                .map(|i| structured_short(&format!("SHORT_{i}"), "serious", f64::from(i)))
                .collect(),
            violations: (0..violations)
                .map(|_| crate::result::DrcGroup {
                    net_a: "SDA".into(),
                    net_b: "SCL".into(),
                    layer: "F.Cu".into(),
                    count: 1,
                    below_count: 1,
                    at_limit: false,
                    min_gap_mm: 0.127,
                    min_gap_loc_mm: [3.0, 4.0],
                    rule_mm: 0.2,
                    between: "track ↔ track".into(),
                    plain: String::new(),
                    fix: "increase spacing".into(),
                    refs: Vec::new(),
                })
                .collect(),
            at_limit: Vec::new(),
            version_warning: None,
            suppression_note: None,
        }
    }

    #[test]
    fn default_rule_flood_note_requires_default_source_and_more_than_ten_x_shorts() {
        let defaulted = crate::result::ClearanceRuleProvenance::defaulted(0.2, false);
        let project = crate::result::ClearanceRuleProvenance::project_file(0.2);
        assert!(clearance_report(11, 1)
            .default_rule_flood_note(&defaulted)
            .is_some());
        assert!(clearance_report(10, 1)
            .default_rule_flood_note(&defaulted)
            .is_none());
        assert!(clearance_report(11, 1)
            .default_rule_flood_note(&project)
            .is_none());
        assert!(clearance_report(1, 0)
            .default_rule_flood_note(&defaulted)
            .is_some());
    }

    /// Gerbers are plotted from the stored fill, so a board whose only finding
    /// is an out-of-date fill is not "OK TO ORDER": it is an item to inspect
    /// (refill, re-run). It is still no short and not a do-not-order item.
    #[test]
    fn a_board_whose_only_finding_is_a_stale_fill_reads_inspect_not_clean() {
        for copper_kind in ["track", "via", "pad"] {
            let mut drc = clearance_report(0, 0);
            drc.stale_fills.push(crate::result::DrcStaleFill {
                zone_net: "GND".into(),
                copper_net: "SDA".into(),
                copper_kind: copper_kind.into(),
                layer: "F.Cu".into(),
                count: 2,
                loc_mm: [5.0, 6.0],
                plain: hauksbee_ir::vocabulary::stale_zone_fill("GND", "SDA", copper_kind),
                fix: "refill all zones".into(),
            });
            let provenance = crate::result::ClearanceRuleProvenance::project_file(0.2);
            let triage = order_triage_with_rule_source(
                &drc,
                &provenance,
                &NetLintReport::default(),
                &SiReport::default(),
                None,
                true,
                0,
                true,
            );
            assert!(triage.do_not_order.is_empty(), "{copper_kind}: {triage:#?}");
            assert_eq!(triage.inspect.len(), 1, "{copper_kind}: {triage:#?}");
            let said = &triage.inspect[0];
            assert_eq!(
                (said.id.as_str(), said.kind.as_str()),
                ("drc", "stale_fill")
            );
            assert!(
                said.statement
                    .contains("refill zones (B) in KiCad and re-run")
                    && said.statement.contains(copper_kind),
                "{copper_kind}: {}",
                said.statement
            );
            assert_eq!(
                drc.shorts.len(),
                0,
                "{copper_kind}: a stale fill is no short"
            );

            // The verdict carries the triage's inspect count: a pass with the
            // inspect tag, and a headline that lists the item, never a clean
            // OK TO ORDER.
            let verdict = crate::result::RunVerdict {
                inspect: triage.inspect.len(),
                ..crate::result::RunVerdict::default()
            };
            assert!(verdict.is_pass(), "it does not gate");
            assert_eq!(
                verdict.inspect_tag().as_deref(),
                Some("inspect before ordering (1 item)")
            );
            let rendered = triage.render(&verdict);
            assert!(
                rendered.contains(&format!("{INSPECT_BEFORE_ORDERING}:\n  - ")),
                "{rendered}"
            );
            assert!(!rendered.contains("OK TO ORDER"), "{rendered}");
        }
    }

    #[test]
    fn order_triage_buckets_confirmed_short_ratings_unconfirmed_short_and_clean_usb() {
        let mut confirmed = structured_short("+3V3", "serious", 1.0);
        confirmed.attach_oracle_agreement("10.0.5");
        let drc = crate::result::DrcStructured {
            clearance_rule_mm: 0.2,
            primitive_count: 6,
            stale_fills: Vec::new(),
            shorts: vec![confirmed, structured_short("PYRO4_FIRE", "note", 3.0)],
            violations: Vec::new(),
            at_limit: Vec::new(),
            version_warning: Some("KiCad 10 format is unvalidated".into()),
            suppression_note: None,
        };
        let mut lint = NetLintReport::default();
        lint.findings.push(LintFinding {
            check: LintCheck::DeviceDecode,
            severity: Severity::Medium,
            message: "U19 eFuse connector budget: R48 sets 15 A through a 2 A connector".into(),
            refs: vec!["U19".into(), "R48".into()],
            nets: vec!["ILIM".into()],
        });
        let usb = crate::checks::usb_c::UsbcReport {
            receptacles: Vec::new(),
            shared_net: false,
            cc1_rd_ohms: Some(5_100.0),
            cc2_rd_ohms: Some(5_100.0),
            attach: crate::checks::usb_c::Attach::SinkAttached,
            powers_vbus: true,
            has_discrete_rd: true,
            level: crate::checks::usb_c::UsbcLevel::Ok,
            headline: "both CC pins have their own 5.1 kΩ Rd; a compliant source applies VBUS"
                .into(),
        };

        let provenance = crate::result::ClearanceRuleProvenance::defaulted(0.2, false);
        let triage = order_triage_with_rule_source(
            &drc,
            &provenance,
            &lint,
            &SiReport::default(),
            Some(&usb),
            true,
            7,
            true,
        );
        assert_eq!(triage.do_not_order.len(), 2, "{triage:#?}");
        assert!(triage
            .do_not_order
            .iter()
            .any(|item| item.statement.contains("+3V3") && item.statement.contains("KiCad")));
        assert!(triage
            .do_not_order
            .iter()
            .any(|item| item.statement.contains("eFuse connector budget")));
        assert_eq!(triage.inspect.len(), 1, "{triage:#?}");
        assert!(triage.inspect[0].statement.contains("PYRO4_FIRE"));
        assert_eq!(triage.checked_ok.len(), 1, "{triage:#?}");
        assert!(triage.checked_ok[0].statement.contains("USB-C CC"));
        let rendered = triage.render(&crate::result::RunVerdict::default());
        assert!(rendered.starts_with("== ORDER / DON'T-ORDER TRIAGE =="));
        assert!(rendered.contains("NOT COVERED: 7 parts lack"));
        assert!(!rendered.contains("OK TO ORDER"), "{rendered}");
        let clean = OrderTriage::default().render(&crate::result::RunVerdict::default());
        assert!(
            clean.contains("\nOK TO ORDER: ") && !clean.contains(":\n"),
            "{clean}"
        );
    }

    /// A schematic/PCB value mismatch is a non-gating "check before ordering"
    /// item naming both values; a moved pin (High) still gates. Both carry the
    /// same check, so the severity alone must decide.
    #[test]
    fn a_value_mismatch_is_listed_to_check_but_does_not_gate() {
        let finding = |severity, message: &str| LintFinding {
            check: LintCheck::PcbSchematicMismatch,
            severity,
            message: message.to_string(),
            refs: vec!["C4".into()],
            nets: Vec::new(),
        };
        let value = format!(
            "{CHECK_BEFORE_ORDERING}C4 has value '100n' in the schematic but '10n' on the PCB."
        );
        let lint = NetLintReport {
            findings: vec![finding(Severity::Low, &value)],
        };
        let json = crate::result::lint_findings_json(&lint);
        assert!(!json[0].gates(), "a value mismatch must not gate");
        let drc = crate::result::DrcStructured {
            clearance_rule_mm: 0.2,
            primitive_count: 0,
            stale_fills: Vec::new(),
            shorts: Vec::new(),
            violations: Vec::new(),
            at_limit: Vec::new(),
            version_warning: None,
            suppression_note: None,
        };
        let provenance = crate::result::ClearanceRuleProvenance::defaulted(0.2, false);
        let triage = |lint: &NetLintReport| {
            order_triage_with_rule_source(
                &drc,
                &provenance,
                lint,
                &SiReport::default(),
                None,
                true,
                0,
                true,
            )
        };
        let t = triage(&lint);
        assert!(t.do_not_order.is_empty(), "{t:#?}");
        assert_eq!(t.inspect.len(), 1, "{t:#?}");
        assert!(t.inspect[0].statement.contains("100n") && t.inspect[0].statement.contains("10n"));
        // Control: a pin wired differently on the PCB still gates.
        let moved = NetLintReport {
            findings: vec![finding(Severity::High, "R1.2 is connected differently")],
        };
        assert!(crate::result::lint_findings_json(&moved)[0].gates());
        assert_eq!(triage(&moved).do_not_order.len(), 1);
    }

    /// A board whose only finding is a part straight across a source with
    /// nothing limiting its current is never a clean pass: the finding is a
    /// warning (not serious), the plain verdict names an issue worth a look,
    /// and the order triage lists it to inspect, so the verdict is "pass with
    /// items to inspect" and (with `--strict`, which fails on lint warnings)
    /// a fail.
    #[test]
    fn an_unbounded_overstress_alone_is_not_a_clean_pass() {
        let lint = NetLintReport {
            findings: vec![LintFinding {
                check: LintCheck::DcOverstress,
                severity: Severity::Medium,
                message: hauksbee_bind::stress::unbounded_note("D1", Some("+5V")),
                refs: vec!["D1".into()],
                nets: Vec::new(),
            }],
        };
        let json = crate::result::lint_findings_json(&lint);
        assert_eq!(json[0].severity, "warning");
        assert!(json[0].gates() && json[0].actionable, "{json:?}");
        let plain = plain_netlint(&lint);
        assert_eq!(plain.findings[0].level, PlainLevel::Warning);
        assert_eq!(plain.serious_count(), 0);
        let verdict = plain.verdict();
        assert!(
            verdict.contains("none serious (worth a look)") && !verdict.contains("healthy"),
            "{verdict}"
        );
        assert!(
            plain.findings[0].why.contains("fitted the wrong way round")
                && !plain.findings[0].why.contains("pushed past"),
            "{:?}",
            plain.findings[0]
        );
        let drc = crate::result::DrcStructured {
            clearance_rule_mm: 0.2,
            primitive_count: 0,
            shorts: Vec::new(),
            stale_fills: Vec::new(),
            violations: Vec::new(),
            at_limit: Vec::new(),
            version_warning: None,
            suppression_note: None,
        };
        let provenance = crate::result::ClearanceRuleProvenance::defaulted(0.2, false);
        let t = order_triage_with_rule_source(
            &drc,
            &provenance,
            &lint,
            &SiReport::default(),
            None,
            true,
            0,
            true,
        );
        assert!(t.do_not_order.is_empty(), "{t:#?}");
        assert_eq!(t.inspect.len(), 1, "{t:#?}");
        assert!(t.inspect[0]
            .statement
            .contains("D1 sits directly across +5V"));
        let rendered = t.render(&crate::result::RunVerdict::default());
        assert!(!rendered.contains("OK TO ORDER"), "{rendered}");
    }

    fn found(level: PlainLevel, what: &str, fix: &str) -> PlainFinding {
        PlainFinding {
            level,
            what: what.to_string(),
            why: String::new(),
            fix: fix.to_string(),
            loc_mm: Some([1.0, 2.0]),
            part: None,
        }
    }

    fn report_of(findings: Vec<PlainFinding>) -> PlainReport {
        PlainReport {
            findings,
            ..PlainReport::default()
        }
    }

    #[test]
    fn fix_first_leads_with_copper_then_wiring_and_folds_a_shared_remedy() {
        use PlainLevel::{Note, Serious, Warning};
        let copper = report_of(vec![
            found(Warning, "tight gap", "widen it"),
            found(Serious, "short A", "separate the copper"),
            found(Serious, "short B", "separate the copper"),
        ]);
        let wiring = report_of(vec![
            found(Note, "a note", "n/a"),
            found(Serious, "floating pin", "tie it"),
            found(Serious, "contention", "drop a driver"),
        ]);
        let si = report_of(vec![
            found(Serious, "thin trace", "widen it"),
            found(Serious, "fourth cause", "fix it"),
        ]);
        let first = fix_first(&copper, &wiring, None, &si);
        let whats: Vec<_> = first.items.iter().map(|i| i.what.as_str()).collect();
        assert_eq!(whats, ["short A", "floating pin", "contention"]);
        assert_eq!(first.items[0].similar, 1, "short B folds into short A");
        assert_eq!(first.more, 2, "two more causes beyond the three listed");
        assert_eq!(first.items[0].x, Some(1.0));
        assert!(first.notice.is_none());
        let text = first.render();
        assert!(
            text.starts_with("== FIX THESE FIRST ==\n1. short A (+1 more like this)\n"),
            "{text}"
        );
        assert!(text.contains("...and 2 more serious cause(s)"), "{text}");
    }

    #[test]
    fn fix_first_holds_copper_back_on_an_unrouted_board_and_routes_last() {
        use PlainLevel::Serious;
        let copper = report_of(vec![found(Serious, "pad overlap", "separate the copper")]);
        let unrouted = format!(
            "{} (134 pad groups)",
            hauksbee_extract::NOT_ROUTED_MESSAGE_PREFIX
        );
        let wiring = report_of(vec![
            found(Serious, &unrouted, "route it"),
            found(Serious, "floating pin", "tie it"),
        ]);
        let first = fix_first(&copper, &wiring, None, &PlainReport::default());
        let whats: Vec<_> = first.items.iter().map(|i| i.what.as_str()).collect();
        assert_eq!(whats, ["floating pin", ROUTE_THE_BOARD]);
        assert_eq!(first.notice, Some(PRE_LAYOUT_NOTICE));
        assert!(first.render().contains("wait until the board is routed"));
        // The state is said once (the notice); the item only says what to do.
        assert!(!first.render().contains("pad groups"));
        // The copper waits and is not counted; the not-routed state is listed
        // as serious, so it is counted (1 floating pin + 1 unrouted = 2).
        let (total, serious) = without_held_copper(&copper, 3, 3);
        assert_eq!((total, serious), (2, 2));
    }

    #[test]
    fn a_pre_layout_headline_quotes_the_counters_and_names_the_routing() {
        assert_eq!(
            pre_layout_headline(1, 1, 0),
            "Not routed yet: the board cannot be ordered as is, and nothing else was found."
        );
        assert_eq!(
            pre_layout_headline(12, 1, 0),
            "Not routed yet: 12 issues found, 1 serious, including the missing routing."
        );
        assert_eq!(
            pre_layout_headline(2, 2, 2),
            "Not routed yet: 2 issues found, 2 serious, including the missing routing. 2 copper finding(s) are held back until it is routed."
        );
    }

    #[test]
    fn fix_first_is_silent_without_a_serious_finding() {
        let quiet = report_of(vec![found(PlainLevel::Warning, "tight gap", "widen it")]);
        let first = fix_first(
            &quiet,
            &PlainReport::default(),
            None,
            &PlainReport::default(),
        );
        assert!(first.is_empty());
        assert_eq!(first.render(), "");
    }
}
