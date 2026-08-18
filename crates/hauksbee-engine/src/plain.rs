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

use hauksbee_extract::{
    DrcReport, ItemKind, LintCheck, NetLintReport, Severity, SiCheck, SiFinding, SiReport,
    SiSeverity, ViolationKind, DNP_PULLUP_MESSAGE_MARKER, SPI_PULLUP_MESSAGE_MARKER,
};

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
    fn tag(self) -> &'static str {
        match self {
            PlainLevel::Serious => "SERIOUS",
            PlainLevel::Warning => "WARNING",
            PlainLevel::Note => "note",
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
}

/// An actionable "heads up" note: worth knowing, not a failure. Carries the same
/// what / why / what-to-do shape a finding does, so the web / TUI / CLI can all
/// give a novice the translation instead of a bare jargon line (the persona-panel
/// "Zdiff 173 vs 90 with no what-to-do" fix). `why` / `fix` may be empty for a
/// note that is already a complete self-contained sentence (e.g. a co-sim
/// caveat); renderers omit those lines when empty.
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
        HeadsUp {
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
        HeadsUp {
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
    /// the hobbyist persona lost is exactly this (Fix #3 / Theme A). A "Looks
    /// healthy" verdict that hides the only actionable observation is the breach
    /// of trust we refuse to ship. Each note carries a what / why / what-to-do
    /// gloss so it translates the jargon rather than dumping it.
    pub heads_up: Vec<HeadsUp>,
    /// Current-carrying / active parts with no model
    /// ([`crate::result::unmodelled_critical_refs`]). Non-empty forces the
    /// verdict off "Looks healthy": a clean check over an unmodelled power FET
    /// or main IC is vacuous, and the verdict must read INCONCLUSIVE, naming
    /// the parts and the unlocking input, instead of a clean bill. The emit
    /// sites that hold a bind report set this; renderers with no bind context
    /// leave it empty and are unchanged.
    pub unmodelled_critical: Vec<String>,
    /// A report-wide qualification that changes how the finding count may be
    /// read. Copper findings on a newer-than-validated board format are kept
    /// visible but demoted, so the headline must explain that demotion in the
    /// same breath as the severity count.
    pub unvalidated: Option<String>,
}

/// De-escape KiCad's label escapes so a plain sentence names the net the way
/// the schematic does. KiCad writes a literal `/` inside a label as `{slash}`
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
        PlainReport {
            subject: subject.to_string(),
            verdict_noun: None,
            findings: Vec::new(),
            heads_up: Vec::new(),
            unmodelled_critical: Vec::new(),
            unvalidated: None,
        }
    }

    fn push(&mut self, level: PlainLevel, what: String, why: String, fix: String) {
        self.findings.push(PlainFinding {
            level,
            what: readable(what),
            why,
            fix,
            loc_mm: None,
        });
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
        });
    }

    /// Number of findings that are "serious" (the M in "N issues, M serious").
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

    /// The one-line overall verdict.
    pub fn verdict(&self) -> String {
        let n = self.findings.len();
        if let Some(reason) = &self.unvalidated {
            let reason = reason.trim_end_matches(['.', ';']);
            let serious = self.serious_count();
            if n == 0 {
                return format!(
                    "No potential copper issues listed, but this result is UNVALIDATED: {reason}"
                );
            }
            let issues = if n == 1 { "issue" } else { "issues" };
            if serious > 0 {
                let remaining = n.saturating_sub(serious);
                return format!(
                    "{n} potential copper {issues} found; {serious} oracle-confirmed short(s) \
                     count as serious. The board format remains UNVALIDATED ({reason}); the \
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
        let (healthy_noun, failure_noun) = match &self.verdict_noun {
            Some(noun) => (noun.clone(), noun.clone()),
            None => {
                let lower = self.subject.to_lowercase();
                (format!("{lower} problems"), format!("{lower} failures"))
            }
        };
        if n == 0 {
            // A clean check over unmodelled current-carrying / active parts is
            // vacuous, not healthy. Refuse the clean bill and name what unlocks
            // a conclusive verdict. This does NOT change any exit code; it is
            // verdict prose only (the exit contract lives in docs/ci/CI.md).
            // Actionable heads-up notes stay on the verdict line (Fix #3's
            // never-bury rule): INCONCLUSIVE must not hide the one observation
            // the user may have come to check.
            if !self.unmodelled_critical.is_empty() {
                let mut v = crate::result::inconclusive_verdict(&self.unmodelled_critical);
                if !self.heads_up.is_empty() {
                    let hn = self.heads_up.len();
                    let things = if hn == 1 { "thing" } else { "things" };
                    v.push_str(&format!(" Plus {hn} {things} worth a look (see below)."));
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
    pub fn render(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "{}", self.verdict());
        // With real findings, the verdict line counts them (not a clean bill),
        // but the coverage hole must still be said out loud: fixing the listed
        // findings would otherwise flip the verdict straight to a vacuous pass.
        if !self.findings.is_empty() && !self.unmodelled_critical.is_empty() {
            let _ = writeln!(
                s,
                "{}",
                crate::result::inconclusive_verdict(&self.unmodelled_critical)
            );
        }
        if !self.findings.is_empty() {
            let _ = writeln!(s);
            for (i, f) in self.sorted().iter().enumerate() {
                let _ = writeln!(s, "{}. [{}] {}", i + 1, f.level.tag(), f.what);
                let _ = writeln!(s, "     Why it matters: {}", f.why);
                let _ = writeln!(s, "     What to do:     {}", f.fix);
                let _ = writeln!(s);
            }
        }
        // Actionable info notes are NEVER dropped, even when the verdict reads
        // "healthy". This is the anti-false-comfort guarantee (Fix #3).
        if !self.heads_up.is_empty() {
            let _ = writeln!(s, "\nHeads up (worth knowing, not a failure):");
            for note in &self.heads_up {
                let _ = writeln!(s, "  - {}", note.what);
                if !note.why.is_empty() {
                    let _ = writeln!(s, "       Why it matters: {}", note.why);
                }
                if !note.fix.is_empty() {
                    let _ = writeln!(s, "       What to do:     {}", note.fix);
                }
            }
        }
        s
    }
}

/// The compact decision surface at the front of a combined `--check --plain`
/// report. It classifies findings that already exist; it never runs a check or
/// derives a new electrical claim.
#[derive(Debug, Clone, Default)]
pub(crate) struct OrderTriage {
    do_not_order: Vec<String>,
    inspect_before_ordering: Vec<String>,
    checked_and_ok: Vec<String>,
    unmodelled_parts: usize,
}

impl OrderTriage {
    fn write_bucket(out: &mut String, heading: &str, entries: &[String]) {
        let _ = writeln!(out, "{heading}:");
        if entries.is_empty() {
            let _ = writeln!(out, "  - Empty.");
        } else {
            for entry in entries {
                let _ = writeln!(out, "  - {entry}");
            }
        }
    }

    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "== ORDER / DON'T-ORDER TRIAGE ==");
        Self::write_bucket(&mut out, "DO NOT ORDER", &self.do_not_order);
        Self::write_bucket(
            &mut out,
            "INSPECT BEFORE ORDERING",
            &self.inspect_before_ordering,
        );
        Self::write_bucket(&mut out, "CHECKED AND OK", &self.checked_and_ok);
        let noun = if self.unmodelled_parts == 1 {
            "part lacks"
        } else {
            "parts lack"
        };
        let _ = writeln!(
            out,
            "NOT COVERED: {} {noun} an executable simulation model; model-dependent \
             firmware/analog/AC/thermal claims involving those parts are not covered. Copper \
             geometry and datasheet decode rules do not require a simulation model.",
            self.unmodelled_parts,
        );
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
pub(crate) fn order_triage(
    drc: &crate::result::DrcStructured,
    lint: &NetLintReport,
    si: &SiReport,
    usbc: Option<&crate::checks::usb_c::UsbcReport>,
    usbc_reliable: bool,
    unmodelled_parts: usize,
) -> OrderTriage {
    let mut triage = OrderTriage {
        unmodelled_parts,
        ..OrderTriage::default()
    };

    let drc_plain = plain_drc_structured(drc);
    let mut serious_shorts = Vec::new();
    let mut tool_only_shorts = Vec::new();
    for (short, finding) in drc.shorts.iter().zip(&drc_plain.findings) {
        if finding.level == PlainLevel::Serious {
            serious_shorts.push(finding.what.clone());
        } else {
            let evidence = if short.oracle_agreement().is_some() {
                String::new()
            } else {
                " Tool-only: no matching KiCad-oracle confirmation is attached.".to_string()
            };
            tool_only_shorts.push(format!("{}{evidence}", finding.what));
        }
    }
    push_triage_class(
        &mut triage.do_not_order,
        serious_shorts,
        "oracle-confirmed copper shorts",
    );
    push_triage_class(
        &mut triage.inspect_before_ordering,
        tool_only_shorts,
        "tool-only potential copper shorts with coordinates",
    );
    // The structured/plain order is shorts, below-rule violations, then
    // at-limit observations. Only actual below-rule groups belong on the order
    // screen; no-margin observations remain in the detail below.
    let clearance_findings: Vec<String> = drc_plain
        .findings
        .iter()
        .skip(drc.shorts.len())
        .take(drc.violations.len())
        .map(|finding| finding.what.clone())
        .collect();
    push_triage_class(
        &mut triage.inspect_before_ordering,
        clearance_findings,
        "below-rule clearance groups with coordinates",
    );

    let lint_plain = plain_netlint(lint);
    for (raw, finding) in lint.findings.iter().zip(&lint_plain.findings) {
        if finding.level == PlainLevel::Serious
            || (raw.check == LintCheck::DeviceDecode && raw.severity != Severity::Low)
        {
            triage.do_not_order.push(finding.what.clone());
        } else if raw.check == LintCheck::UncheckedMcu {
            triage.inspect_before_ordering.push(finding.what.clone());
        }
    }

    let si_plain = plain_si(si);
    for finding in &si_plain.findings {
        if finding.level == PlainLevel::Serious {
            triage.do_not_order.push(finding.what.clone());
        }
    }

    if let Some(report) = usbc {
        use crate::checks::usb_c::UsbcLevel;
        let item = format!("USB-C CC: {}", report.headline);
        match report.level {
            UsbcLevel::Serious => triage.do_not_order.push(item),
            UsbcLevel::Info => triage.inspect_before_ordering.push(item),
            UsbcLevel::Ok if usbc_reliable => triage.checked_and_ok.push(item),
            UsbcLevel::Ok => triage.inspect_before_ordering.push(format!(
                "{item} Nominal result only: an unresolved part on the CC nets prevents relying on it."
            )),
        }
    }

    triage
}

/// Keep the leading surface genuinely screen-sized without dropping a risk
/// class. Three or fewer existing findings remain verbatim; a larger class is
/// one counted line with its first existing finding as an example and an
/// explicit pointer to the complete coordinate detail below.
fn push_triage_class(target: &mut Vec<String>, entries: Vec<String>, class: &str) {
    match entries.as_slice() {
        [] => {}
        [one, _, _, _, ..] => target.push(format!(
            "{} {class}. Example: {one} Full list follows below.",
            entries.len(),
        )),
        _ => target.extend(entries),
    }
}

// ── Severity bridges ─────────────────────────────────────────────────────────

fn from_lint_sev(s: Severity) -> PlainLevel {
    match s {
        Severity::High => PlainLevel::Serious,
        Severity::Medium => PlainLevel::Warning,
        Severity::Low => PlainLevel::Note,
    }
}

fn from_si_sev(s: SiSeverity) -> PlainLevel {
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

/// Expand KiCad's copper-layer codes to plain language on first sight. A novice
/// (persona-panel) has no idea `F.Cu` / `B.Cu` mean the front / back copper
/// layer, so spell it out while keeping the code in parentheses for the expert.
fn friendly_layer(layer: &str) -> String {
    match layer {
        "F.Cu" => "the front copper layer (F.Cu)".to_string(),
        "B.Cu" => "the back copper layer (B.Cu)".to_string(),
        other => other.to_string(),
    }
}

/// Pick a friendly noun for a copper item ("the wire", "the chip pad", ...).
fn item_noun(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Track => "a copper wire",
        ItemKind::Arc => "a copper wire",
        ItemKind::Via => "a via (layer-to-layer hole)",
        ItemKind::Pad => "a component pad",
        ItemKind::Zone => "a copper fill area",
        ItemKind::Graphic => "a drawn copper shape",
    }
}

// ── DRC (copper shorts / clearance) ───────────────────────────────────────────

/// Translate the geometric DRC report (copper shorts and near-shorts).
pub fn plain_drc(report: &DrcReport) -> PlainReport {
    let mut out = PlainReport::new("Copper spacing (DRC)");
    for f in &report.findings {
        let a = if f.net_a_name.is_empty() {
            "an unnamed net"
        } else {
            &f.net_a_name
        };
        let b = if f.net_b_name.is_empty() {
            "an unnamed net"
        } else {
            &f.net_b_name
        };
        let where_ = format!(
            "near x={:.1} mm, y={:.1} mm on {}",
            f.x,
            f.y,
            friendly_layer(&f.layer)
        );
        match f.kind {
            ViolationKind::Short => out.push(
                PlainLevel::Serious,
                format!(
                    "Two separate connections, \"{a}\" and \"{b}\", are touching ({} touches {}), {where_}.",
                    item_noun(f.item_a.kind),
                    item_noun(f.item_b.kind),
                ),
                format!(
                    "These are meant to be electrically separate. Where they touch they become one connection (a short), so \"{a}\" and \"{b}\" will be forced to the same voltage. That usually means the board does the wrong thing, and if one is a power rail it can pull large current and overheat."
                ),
                "Pull the two pieces of copper apart so there is a clear gap between them, or remove the bit of copper that bridges them. If they really are supposed to connect, give them the same net name.".to_string(),
            ),
            ViolationKind::Clearance => out.push(
                PlainLevel::Warning,
                format!(
                    "\"{a}\" and \"{b}\" are very close but not quite touching ({:.3} mm apart, your rule wants {:.3} mm), {where_}.",
                    f.gap_mm, f.required_clearance_mm,
                ),
                "They are not shorted today, but the gap is below the spacing the board asks for. Small manufacturing variation, a solder smear, or contamination could bridge them, so it is a reliability risk rather than a guaranteed failure.".to_string(),
                "Open up the spacing between these two so the gap meets your clearance rule, or relax the rule deliberately if you know this spot is fine.".to_string(),
            ),
        }
    }
    out
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
pub fn plain_drc_structured(st: &crate::result::DrcStructured) -> PlainReport {
    let mut out = PlainReport::new("Copper spacing (DRC)");
    out.unvalidated = st.version_warning.clone();

    // Nothing to check is not the same as nothing wrong.
    //
    // A netlist (.net, IPC-D-356) carries connectivity and no copper, so the
    // clearance sweep examines zero primitives and finds zero problems. Left
    // alone, that rendered "Looks healthy: no copper spacing problems found",
    // which a reader takes as "your copper is fine". It was never looked at.
    // Found by running the flagship board in as a newcomer would, where the
    // whole report came back clean on input that has no copper in it.
    //
    // Guarded on the primitive count rather than the file extension, so this
    // covers every format that arrives without copper, including ones added
    // later. The web front door has a narrower version of this for gerbers
    // only; this is the general case.
    if st.primitive_count == 0 {
        out.push_note(HeadsUp::glossed(
            "No copper was checked: this input carries connectivity but no traces or pours."
                .to_string(),
            "Clearance DRC compares copper shapes against each other. A netlist has no \
             copper at all, and a layout file can simply not be routed yet. Everything \
             else in this report still applies.",
            "If this was a layout file (.kicad_pcb, .brd, .PcbDoc), it has no routed \
             copper yet: route it, or point hauksbee at the fab gerbers. If it was a \
             netlist, run hauksbee against the layout file or a gerber archive instead.",
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
        let a = if sh.net_a.is_empty() {
            "an unnamed net"
        } else {
            &sh.net_a
        };
        let b = if sh.net_b.is_empty() {
            "an unnamed net"
        } else {
            &sh.net_b
        };
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
             geometry alone; no matching KiCad-oracle line confirms it."
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

    // Genuinely below-rule clearance groups (gap < rule).
    for g in &st.violations {
        let a = if g.net_a.is_empty() {
            "an unnamed net"
        } else {
            &g.net_a
        };
        let b = if g.net_b.is_empty() {
            "an unnamed net"
        } else {
            &g.net_b
        };
        let places = format!(
            "{} location{}",
            g.count,
            if g.count == 1 { "" } else { "s" }
        );
        let what = if g.below_count == g.count {
            format!(
                "\"{a}\" and \"{b}\" are very close but not quite touching at {places} on {} (tightest {:.3} mm, below your {:.3} mm rule).",
                friendly_layer(&g.layer), g.min_gap_mm, g.rule_mm
            )
        } else {
            format!(
                "\"{a}\" and \"{b}\" are close at {places} on {}: {} below your {:.3} mm rule (tightest {:.3} mm), the rest exactly at the limit.",
                friendly_layer(&g.layer), g.below_count, g.rule_mm, g.min_gap_mm,
            )
        };
        out.push_at(
            PlainLevel::Warning,
            what,
            "They are not shorted today, but at least one spot is below the spacing the board asks for. Small manufacturing variation, a solder smear, or contamination could bridge them, so it is a reliability risk rather than a guaranteed failure.".to_string(),
            "Open up the spacing between these two so the gap meets your clearance rule, or relax the rule deliberately if you know this spot is fine.".to_string(),
            g.min_gap_loc_mm,
        );
    }

    // At-the-limit groups (gap == rule, no margin). NOT "below" the rule.
    push_at_limit_findings(&mut out, &st.at_limit);

    out
}

fn push_at_limit_findings(out: &mut PlainReport, at_limit: &[crate::result::DrcGroup]) {
    for g in at_limit {
        let a = if g.net_a.is_empty() {
            "an unnamed net"
        } else {
            &g.net_a
        };
        let b = if g.net_b.is_empty() {
            "an unnamed net"
        } else {
            &g.net_b
        };
        let places = format!(
            "{} location{}",
            g.count,
            if g.count == 1 { "" } else { "s" }
        );
        out.push_at(
            PlainLevel::Warning,
            format!(
                "\"{a}\" and \"{b}\" sit at minimum clearance (no margin) at {places} on {} ({:.3} mm, exactly your {:.3} mm rule).",
                friendly_layer(&g.layer), g.min_gap_mm, g.rule_mm
            ),
            "These meet your clearance rule exactly, with nothing to spare. They are not below the rule, so this is not a violation, but there is no margin left, so any small manufacturing variation eats into a gap that is already at its allowed minimum.".to_string(),
            "If you want some safety margin, open these gaps up a little beyond the rule. If the rule already reflects your process limits, this is acceptable as-is; just be aware there is no slack.".to_string(),
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
pub fn render_drc_condensed(st: &crate::result::DrcStructured, verbose: bool) -> String {
    use std::fmt::Write as _;
    /// How many clearance findings keep the full three-line gloss.
    const FULL: usize = 3;

    let pr = plain_drc_structured(st);
    let summary = format!(
        "Summary: {} short(s), {} net pair(s) below the clearance rule, {} at minimum \
         clearance (no margin).",
        st.shorts.len(),
        st.violations.len(),
        st.at_limit.len()
    );

    let warning_groups = st.violations.len() + st.at_limit.len();
    if verbose || warning_groups <= FULL + 1 {
        // Nothing worth condensing (or the user asked for everything): the full
        // report, with the trailing summary count appended.
        let mut s = pr.render();
        if warning_groups + st.shorts.len() > 0 {
            let _ = writeln!(s, "{summary}");
        }
        return s;
    }

    // Condensed: verdict, full shorts + first FULL clearance findings, then one
    // aggregate line per (rule, layer) for the rest.
    let mut s = String::new();
    let _ = writeln!(s, "{}", pr.verdict());
    let _ = writeln!(s);
    let mut shown = 0usize;
    let mut idx = 0usize;
    for f in pr.sorted() {
        // Shorts always print in full (Serious, or Note when downgraded on an
        // unvalidated format); only the Warning-level clearance groups condense.
        let full = match f.level {
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
        let _ = writeln!(s, "{}. [{}] {}", idx, f.level.tag(), f.what);
        let _ = writeln!(s, "     Why it matters: {}", f.why);
        let _ = writeln!(s, "     What to do:     {}", f.fix);
        let _ = writeln!(s);
    }
    // The clearance findings enter `pr` in order: violations, then at_limit.
    // Everything past the first FULL of that combined list aggregates by
    // (layer, rule); below-rule and at-limit groups aggregate separately since
    // they mean different things.
    let shown_v = FULL.min(st.violations.len());
    let shown_l = FULL.saturating_sub(st.violations.len());
    use std::collections::BTreeMap;
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
             {tightest:.3} mm vs your {rule:.3} mm rule); pass --verbose for every instance.",
            if *groups == 1 { "" } else { "s" },
            friendly_layer(layer),
            if *locs == 1 { "" } else { "s" },
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
            "  ...and {groups} more net pair{} at exactly the {:.3} mm limit on {} \
             ({locs} location{}); pass --verbose for every instance.",
            if *groups == 1 { "" } else { "s" },
            *rule_um as f64 / 1000.0,
            friendly_layer(layer),
            if *locs == 1 { "" } else { "s" },
        );
    }
    // Heads-up notes are never dropped, condensed or not.
    if !pr.heads_up.is_empty() {
        let _ = writeln!(s, "\nHeads up (worth knowing, not a failure):");
        for note in &pr.heads_up {
            let _ = writeln!(s, "  - {}", note.what);
            if !note.why.is_empty() {
                let _ = writeln!(s, "       Why it matters: {}", note.why);
            }
            if !note.fix.is_empty() {
                let _ = writeln!(s, "       What to do:     {}", note.fix);
            }
        }
    }
    let _ = writeln!(s, "{summary}");
    s
}

// ── Net lint + straps + MCU resource conflicts ────────────────────────────────

/// Translate a [`NetLintReport`]. This covers the connectivity lint checks
/// (`--lint` core), the boot strap-pin lint, and the MCU resource-conflict check
/// (`--resources`), since they all report through this one report shape.
pub fn plain_netlint(report: &NetLintReport) -> PlainReport {
    let mut out = PlainReport::new("connectivity");
    for f in &report.findings {
        let level = from_lint_sev(f.severity);
        let parts = join_refs(&f.refs);
        let net = f.nets.first().map(|s| s.as_str()).unwrap_or("the net");
        let (what, why, fix) = match f.check {
            // The DNP variant of either pull-up check: the resistor exists in
            // the layout but is do-not-populate, so the assembled board still
            // floats. The finding message is the precise statement (it names
            // the part); the generic "has no pull-up resistor" template would
            // contradict the schematic the reader is looking at.
            LintCheck::MissingI2cPullup | LintCheck::MissingSdPullup
                if f.message.contains(DNP_PULLUP_MESSAGE_MARKER) =>
            {
                (
                    f.message.clone(),
                    "The design contains the pull-up, but the part is marked do-not-populate, so the boards that come back from assembly do not have it fitted. Simulation fits DNP parts by default (they are usually placed eventually), which is why other results can look healthy while the real line floats.".to_string(),
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
                format!("The LED + resistor on {parts} looks mis-sized: its current is outside the sensible range for an indicator."),
                "Too much current and the LED runs hot and ages fast (or pulls more than the driving pin should source); too little and it is too dim to see. The series resistor sets this current.".to_string(),
                "Re-pick the series resistor for roughly 1 to 20 mA: resistor = (supply voltage minus LED forward voltage) / target current. Check the LED's datasheet for its forward voltage and max current.".to_string(),
            ),
            LintCheck::OutputContention => (
                format!("Two chip outputs ({parts}) are wired directly together on \"{net}\"."),
                "Both pins actively drive the wire. If one drives high while the other drives low, they fight: large current flows through the two chips, voltages land in a bad middle zone, and you can damage the outputs over time.".to_string(),
                "Only one push-pull output should drive a wire. Remove one driver, add a resistor / buffer / proper bus arrangement, or change one output to open-drain with a single pull-up so they can share the line safely.".to_string(),
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
            LintCheck::DeviceDecode => (
                format!("A configuration pin on {parts} (net \"{net}\") decodes to the wrong setting. {}", f.message),
                // Deliberately generic: this template covers EVERY decoder in the
                // device_decode family (a PD sink's requested voltage, a charger's
                // safety-timer length, an eFuse's current limit). An earlier version
                // narrated the PD case specifically, so an eFuse ILIM finding was
                // explained as a USB-C voltage table: the right math wearing the
                // wrong story, which reads as a bug even when the numbers are right.
                "Some chips read a resistor on a configuration pin and decode its value against a table in the datasheet to set a mode or a limit: which voltage a USB-PD sink requests, how long a charger's safety timer runs, how much current an eFuse passes. If the fitted resistor lands in the wrong band, the chip silently takes the wrong setting: every part is in spec and every wire connects, so a normal value or short check cannot see it. This finding applies the datasheet's own decode rule to the fitted value, which is why it works even when the part has no simulation model and the report lists it as unresolved.".to_string(),
                "Change the resistor so the decoded value matches the intent; the finding above names the fitted value, what it decodes to, and the value that would decode correctly. Then re-check against the part's own decode table and any min/max override note.".to_string(),
            ),
            LintCheck::BackPower => (
                format!("A pin on {parts} (net \"{net}\") sits in a higher voltage domain than the chip's own supply. {}", f.message),
                "Chip inputs have a small protection diode from the pin to the chip's supply. Pull the pin above that supply and the diode conducts: current flows from the higher rail through the chip into the lower rail. With the chip's supply off, the higher rail keeps the \"off\" domain half-powered (parts misbehave instead of resetting cleanly); powered on, the pin sits past its absolute-maximum rating unless it is specifically rated tolerant.".to_string(),
                "Pull the signal up to the chip's own supply rail instead, or level-shift between the domains. If the pin is documented as tolerant of the higher rail (some are), record that so the warning is a verified exception.".to_string(),
            ),
            LintCheck::I2cBusLoading => (
                format!("The I2C line \"{net}\" has pull-ups, but they are mis-sized for the bus. {}", f.message),
                "I2C devices can only pull the line LOW; the resistor pulls it back HIGH. Too strong a pull-up and a device cannot sink enough current to make a valid low (the spec guarantees only 3 mA), so reads fail intermittently. Too weak and the line rises too slowly for the clock rate, corrupting edges as more devices load the bus.".to_string(),
                "Size the pull-up between the two limits: above (rail voltage - 0.4 V) / 3 mA (so lows work), and low enough that the rise time fits the bus speed at the real bus capacitance. 2.2k to 4.7k suits most 3.3 V buses.".to_string(),
            ),
        };
        out.push(level, what, why, fix);
    }
    out
}

// ── Signal integrity ──────────────────────────────────────────────────────────

/// Translate a [`SiReport`]. Info notes are dropped (they are observations, not
/// problems); only real findings are translated.
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
                format!("Adjust the trace width and pair spacing for your actual layer stackup so the estimated impedance hits the target (tools and the formulas in {} give the geometry); or have the fab build a controlled-impedance stackup to your spec.", hauksbee_ir::docs_url("docs/checks/SI_CHECKS.md")),
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

    // Fix #3: promote ACTIONABLE info notes into "Heads up" rather than dropping
    // them. An info note is actionable when it reports a real off-target value
    // (the controlled-impedance "+N% from target" note; the 171-ohm USB case),
    // as opposed to a within-tolerance "ok" or a "no judgement" observation.
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
/// The controlled-impedance note gets the same what / why / what-to-do treatment
/// a finding does (persona-panel fix): a novice saw a bare "Zdiff ~ 173 ohm
/// [target 90]" with no idea what to do. We keep the honest number and add the
/// translation.
fn actionable_info_note(f: &SiFinding) -> Option<HeadsUp> {
    let m = &f.message;
    let off_target = m.contains("from target") && !m.contains("within");
    if !off_target {
        return None;
    }
    // Name the actual copper: the impedance info notes carry their NETS (refs
    // are empty), and the old refs-only fallback printed "the trace pair noted
    // above", a pointer at nothing, on every one of them.
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
            format!(
                "If this pair is NOT a high-speed link (USB/Ethernet/HDMI), you can ignore it. \
                 If it is: adjust the trace width and pair spacing for your actual layer \
                 stackup so the estimate lands near the target (the formulas are in \
                 {}), or ask your fab to build a controlled-impedance stackup to spec.",
                hauksbee_ir::docs_url("docs/checks/SI_CHECKS.md")
            ),
        ),
        SiCheck::ControlledImpedance => HeadsUp::glossed(
            format!("A single-ended trace ({affected}) is off its impedance target ({core})."),
            "A 50 ohm single-ended line is the convention for an RF or antenna feed: the \
             trace has to present that impedance end to end or part of the signal reflects \
             back instead of reaching the antenna, costing range and sensitivity. This is a \
             computed estimate, flagged only because the board did not formally declare a \
             controlled-impedance stackup.",
            format!(
                "If this is not a controlled-impedance RF path, this estimate is informational. \
                 If it is: widen or narrow the trace for your actual layer stackup so the \
                 estimate lands near 50 ohm (the formulas are in {}), or \
                 ask your fab to build a controlled-impedance stackup to spec.",
                hauksbee_ir::docs_url("docs/checks/SI_CHECKS.md")
            ),
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
        .map(|(h, _)| h)
        .unwrap_or(&truncated);
    format!("{}…", cut.trim_end())
}

// ── Stress / datasheet-rating faults ──────────────────────────────────────────

/// Translate the stress-monitor fault events (over-current, over-voltage, etc.)
/// raised while co-simulating against datasheet ratings.
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
                    amps(f.value),
                    amps(f.limit),
                ),
                "Sustained current above a part's rating heats it up. The part runs hot, drifts out of spec, ages fast, and eventually fails (a resistor can char, a trace can burn open).".to_string(),
                "Lower the current (raise the relevant resistance, split the load, add parts in parallel) or use a part rated for this current. If a resistor, also pick a higher-wattage package.".to_string(),
            ),
            FaultKind::SurgeCurrent => (
                format!(
                    "{c} sees a brief current spike of about {}, past its {} surge limit.",
                    amps(f.value),
                    amps(f.limit),
                ),
                "Even short spikes (at power-on, or when charging a big capacitor) can exceed what a part survives. Repeated surges weaken or pop the part even if the average current looks fine.".to_string(),
                "Add inrush limiting: a series resistor / NTC, a soft-start, or a part rated for the surge. Reducing the capacitance being charged through it also helps.".to_string(),
            ),
            FaultKind::Overpower => (
                format!(
                    "{c} is dissipating about {}, past its {} power rating.",
                    watts(f.value),
                    watts(f.limit),
                ),
                "Power turns into heat. Over its rating the part overheats: a resistor scorches, a regulator shuts down or fails, and nearby parts get cooked too.".to_string(),
                "Reduce the power (less current or less voltage across it), spread it over more parts, or move to a larger / higher-wattage package with better heat handling.".to_string(),
            ),
            FaultKind::Overvoltage => (
                format!(
                    "{c} has about {} across it, past its {} voltage rating.",
                    volts(f.value),
                    volts(f.limit),
                ),
                "Every part has a maximum safe voltage. Above it the insulation/junction can break down (a capacitor can short or vent, a semiconductor can be punched through), often instantly and permanently.".to_string(),
                "Use a part rated for at least this voltage (with margin), or lower the voltage it sees (clamp, divider, or correct the supply/rail it is on).".to_string(),
            ),
            FaultKind::ReverseBias => (
                format!("{c} (a polarised part) is being driven backwards (reverse voltage of about {}).", volts(f.value)),
                "Polarised parts like electrolytic and tantalum capacitors only tolerate voltage one way round. Reverse voltage damages them, and a tantalum can fail short and catch fire.".to_string(),
                "Check the part's orientation/footprint against the schematic and flip it if it is backwards, or move it to a net where its polarity is correct. If the net genuinely swings both ways, use a non-polarised part.".to_string(),
            ),
            FaultKind::PinOvercurrent => (
                format!(
                    "A single pin of {c} is sourcing/sinking about {}, past its {} per-pin limit.",
                    amps(f.value),
                    amps(f.limit),
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
                format!(
                    "{c} reaches about {:.0} C, past its {:.0} C junction-temperature limit.",
                    f.value, f.limit,
                ),
                "The power this part dissipates raises its internal (junction) temperature above ambient. Past its rated junction temperature it degrades fast, drifts out of spec, and eventually fails; a hot part also heats its neighbours.".to_string(),
                "Reduce the power it dissipates, move to a package with lower thermal resistance, add copper pour / a heatsink to carry heat away, or improve airflow / lower the ambient. The estimate assumes still air, so real cooling helps.".to_string(),
            ),
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

fn amps(a: f64) -> String {
    let a = a.abs();
    if a < 1.0 {
        format!("{:.0} mA", a * 1000.0)
    } else {
        format!("{a:.2} A")
    }
}

fn watts(w: f64) -> String {
    let w = w.abs();
    if w < 1.0 {
        format!("{:.0} mW", w * 1000.0)
    } else {
        format!("{w:.2} W")
    }
}

fn volts(v: f64) -> String {
    format!("{:.2} V", v.abs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::{DrcFinding, Item, LintFinding, SiFinding};

    /// The INCONCLUSIVE verdict must not bury actionable heads-up notes: the
    /// verdict line refuses the clean bill AND still points at the notes
    /// below (Fix #3's never-bury rule), and with real findings the sentence
    /// rides the render under the counted verdict.
    #[test]
    fn inconclusive_verdict_keeps_the_heads_up_pointer() {
        let mut r = PlainReport::new("signal-integrity");
        r.unmodelled_critical = vec!["Q1".to_string()];
        r.heads_up.push(HeadsUp::note("USB pair off target"));
        let v = r.verdict();
        assert!(v.starts_with("INCONCLUSIVE"), "{v}");
        assert!(
            v.contains("1 thing worth a look (see below)"),
            "the heads-up pointer survives the refusal: {v}"
        );
        assert!(!v.contains("Looks healthy"), "{v}");
        // With findings, the verdict counts them and the sentence still prints.
        r.push(PlainLevel::Warning, "w".into(), "why".into(), "fix".into());
        let rendered = r.render();
        assert!(rendered.contains("1 issue found"), "{rendered}");
        assert!(
            rendered.contains("INCONCLUSIVE: 1 current-carrying / active part(s)"),
            "the coverage hole is still said out loud next to real findings:\n{rendered}"
        );
    }

    fn drc_short() -> DrcFinding {
        DrcFinding {
            kind: ViolationKind::Short,
            net_a: 1,
            net_b: 2,
            net_a_name: "+5V".to_string(),
            net_b_name: "GND".to_string(),
            layer: "F.Cu".to_string(),
            x: 12.3,
            y: 45.6,
            gap_mm: -0.05,
            required_clearance_mm: 0.2,
            item_a: Item {
                kind: ItemKind::Track,
                net: 1,
                owner: String::new(),
            },
            item_b: Item {
                kind: ItemKind::Pad,
                net: 2,
                owner: "U1".to_string(),
            },
        }
    }

    #[test]
    fn drc_short_is_serious_with_why_and_fix() {
        let mut report = DrcReport {
            clearance_mm: 0.2,
            ..Default::default()
        };
        report.findings.push(drc_short());
        let plain = plain_drc(&report);
        assert_eq!(plain.findings.len(), 1);
        let f = &plain.findings[0];
        assert_eq!(f.level, PlainLevel::Serious);
        // What / why / fix are all populated and non-trivial.
        assert!(f.what.contains("+5V") && f.what.contains("GND"));
        assert!(f.why.to_lowercase().contains("short"));
        assert!(!f.fix.is_empty());
        // Verdict counts it as serious.
        assert_eq!(plain.serious_count(), 1);
        assert!(plain.verdict().contains("1 serious"));
    }

    #[test]
    fn drc_clearance_is_a_warning_not_serious() {
        let mut f = drc_short();
        f.kind = ViolationKind::Clearance;
        f.gap_mm = 0.12;
        let report = DrcReport {
            clearance_mm: 0.2,
            findings: vec![f],
            primitive_count: 2,
            version_warning: None,
            zone_pad_overlaps_suppressed: Some(0),
        };
        let plain = plain_drc(&report);
        assert_eq!(plain.findings[0].level, PlainLevel::Warning);
        assert_eq!(plain.serious_count(), 0);
        assert!(plain.verdict().contains("none serious"));
    }

    #[test]
    fn kicad_10_plain_caveat_describes_exact_parity_not_unhandled_zone_fill() {
        const CLEARANCE_MM: f64 = 0.2;
        const PRIMITIVE_COUNT: usize = 2;
        let report = DrcReport {
            clearance_mm: CLEARANCE_MM,
            findings: vec![drc_short()],
            primitive_count: PRIMITIVE_COUNT,
            version_warning: Some(
                "KiCad 10 name-only nets and keyhole antipads are handled, but remaining \
                 findings are UNVALIDATED"
                    .to_string(),
            ),
            zone_pad_overlaps_suppressed: Some(0),
        };
        let plain = plain_drc_structured(&crate::result::DrcStructured::from_report(&report));
        let verdict = plain.verdict();
        let text = plain.render().to_lowercase();

        assert!(
            verdict.contains("UNVALIDATED") && verdict.contains("BECAUSE unvalidated"),
            "the headline must qualify why no finding counted as serious: {verdict}"
        );
        assert!(
            !verdict.contains("none serious (worth a look)"),
            "an unvalidated copper report must not use the ordinary all-clear-shaped headline: {verdict}"
        );

        assert!(text.contains("name-only nets"), "{text}");
        assert!(text.contains("keyhole antipads"), "{text}");
        assert!(
            text.contains("exact") && text.contains("parity"),
            "the remaining limitation is exact native-DRC parity:\n{text}"
        );
        assert!(
            text.contains("downgrad"),
            "the user-facing caveat must explain the safety demotion:\n{text}"
        );
        for stale_claim in ["ground pour", "shorts every net", "zone fill is unhandled"] {
            assert!(
                !text.contains(stale_claim),
                "stale KiCad-10 claim {stale_claim:?} remained:\n{text}"
            );
        }
    }

    #[test]
    fn plain_structured_drc_groups_and_uses_at_limit_wording() {
        use crate::result::DrcStructured;
        // Three findings for the SAME net pair + layer, all with gap == rule
        // (exactly at minimum clearance, NOT below). The structured plain
        // renderer must (a) collapse them into ONE finding, and (b) describe
        // them as "at minimum clearance (no margin)", never "below".
        let at_limit = || {
            let mut f = drc_short();
            f.kind = ViolationKind::Clearance;
            f.net_a_name = "SIG_A".to_string();
            f.net_b_name = "SIG_B".to_string();
            f.layer = "F.Cu".to_string();
            f.required_clearance_mm = 0.2;
            f.gap_mm = 0.2; // exactly at the rule
            f
        };
        let report = DrcReport {
            clearance_mm: 0.2,
            findings: vec![at_limit(), at_limit(), at_limit()],
            primitive_count: 6,
            version_warning: None,
            zone_pad_overlaps_suppressed: Some(0),
        };
        let st = DrcStructured::from_report(&report);
        let plain = plain_drc_structured(&st);

        // Grouped: 3 raw findings -> 1 plain finding.
        assert_eq!(
            plain.findings.len(),
            1,
            "duplicates were not grouped: {:?}",
            plain.findings
        );
        let f = &plain.findings[0];
        // gap == rule is at-limit, not below: not serious, and worded correctly.
        assert_eq!(f.level, PlainLevel::Warning);
        assert!(
            f.what.contains("at minimum clearance (no margin)"),
            "expected at-limit wording, got: {}",
            f.what
        );
        assert!(
            !f.what.to_lowercase().contains("below"),
            "at-limit finding must not say 'below': {}",
            f.what
        );
        // The count reflects all three locations.
        assert!(
            f.what.contains("3 locations"),
            "missing grouped count: {}",
            f.what
        );
        // Genuinely-below findings DO say "below".
        let mut below = at_limit();
        below.gap_mm = 0.10; // below the 0.2 rule
        let below_report = DrcReport {
            clearance_mm: 0.2,
            findings: vec![below],
            primitive_count: 2,
            version_warning: None,
            zone_pad_overlaps_suppressed: Some(0),
        };
        let below_plain = plain_drc_structured(&DrcStructured::from_report(&below_report));
        assert_eq!(below_plain.findings.len(), 1);
        assert!(
            below_plain.findings[0]
                .what
                .to_lowercase()
                .contains("below"),
            "below-rule finding should say 'below': {}",
            below_plain.findings[0].what
        );
    }

    #[test]
    fn condensed_plain_drc_keeps_three_full_findings_and_aggregates_the_rest() {
        use crate::result::DrcStructured;
        // Fifty distinct below-rule net pairs (the Watchy --drc --plain cry-wolf
        // case): the condensed renderer keeps the first three full what/why/fix
        // blocks, collapses the other 47 into one aggregate line per (rule,
        // layer), and ends with a one-line summary. --verbose restores all 50.
        let findings: Vec<_> = (0..50)
            .map(|i| {
                let mut f = drc_short();
                f.kind = ViolationKind::Clearance;
                f.net_a_name = format!("NET_{i}");
                f.net_b_name = format!("NET_{}", i + 100);
                f.layer = "F.Cu".to_string();
                f.required_clearance_mm = 0.2;
                f.gap_mm = 0.15;
                f
            })
            .collect();
        let report = DrcReport {
            clearance_mm: 0.2,
            findings,
            primitive_count: 100,
            version_warning: None,
            zone_pad_overlaps_suppressed: Some(0),
        };
        let st = DrcStructured::from_report(&report);

        let condensed = render_drc_condensed(&st, false);
        assert_eq!(
            condensed.matches("Why it matters:").count(),
            3,
            "exactly three findings keep the full gloss:\n{condensed}"
        );
        assert!(
            condensed.contains("47 more net pairs like this") && condensed.contains("--verbose"),
            "the rest aggregate into one line pointing at --verbose:\n{condensed}"
        );
        assert!(
            condensed.contains("tightest 0.150 mm") && condensed.contains("0.200 mm rule"),
            "the aggregate names the tightest gap and the rule:\n{condensed}"
        );
        assert!(
            condensed
                .trim_end()
                .lines()
                .last()
                .is_some_and(|l| l.starts_with("Summary:") && l.contains("50 net pair(s)")),
            "a trailing one-line summary closes the report:\n{condensed}"
        );
        // The verdict still tells the truth about the total.
        assert!(
            condensed.contains("50 issues found"),
            "the verdict keeps the real count:\n{condensed}"
        );

        // --verbose restores every instance, still with the trailing summary.
        let verbose = render_drc_condensed(&st, true);
        assert_eq!(
            verbose.matches("Why it matters:").count(),
            50,
            "verbose prints all findings in full"
        );
        assert!(verbose.trim_end().ends_with(
            "Summary: 0 short(s), 50 net pair(s) below the clearance rule, 0 at minimum \
             clearance (no margin)."
        ));

        // A short is never condensed away.
        let mut with_short = report;
        with_short.findings.push(drc_short());
        let st2 = DrcStructured::from_report(&with_short);
        let s2 = render_drc_condensed(&st2, false);
        assert!(
            s2.contains("are touching"),
            "the short keeps its full block:\n{s2}"
        );
    }

    #[test]
    fn empty_drc_reads_healthy() {
        let plain = plain_drc(&DrcReport {
            clearance_mm: 0.2,
            ..Default::default()
        });
        assert!(plain.verdict().to_lowercase().contains("healthy"));
        assert!(plain.render().to_lowercase().contains("healthy"));
    }

    #[test]
    fn every_lint_check_maps_to_a_template() {
        // One finding per LintCheck variant; each must produce non-empty
        // what/why/fix so no kind is left without a plain translation.
        // LintCheck::ALL, not a hand-list: a hand-list here silently went
        // stale (DeviceDecode/BackPower/I2cBusLoading were never added), so
        // the guard was not guarding.
        let checks = LintCheck::ALL.map(|c| (c, Severity::Medium));
        for (check, sev) in checks {
            let report = NetLintReport {
                findings: vec![LintFinding {
                    check,
                    severity: sev,
                    message: "expert message goes here".to_string(),
                    refs: vec!["U3".to_string(), "C1".to_string()],
                    nets: vec!["SDA".to_string()],
                }],
            };
            let plain = plain_netlint(&report);
            assert_eq!(plain.findings.len(), 1, "{check:?} produced no finding");
            let f = &plain.findings[0];
            assert!(!f.what.is_empty(), "{check:?} has empty what");
            assert!(!f.why.is_empty(), "{check:?} has empty why");
            assert!(!f.fix.is_empty(), "{check:?} has empty fix");
        }
    }

    #[test]
    fn every_si_check_maps_to_a_template() {
        let checks = [
            SiCheck::CrystalLoadCap,
            SiCheck::I2cRiseTime,
            SiCheck::AntennaKeepout,
            SiCheck::UsbDiffPair,
        ];
        for check in checks {
            let report = SiReport {
                findings: vec![SiFinding {
                    check,
                    severity: SiSeverity::High,
                    message: "rise time 1200 ns exceeds 300 ns".to_string(),
                    refs: vec!["Y1".to_string()],
                    nets: vec!["SCL".to_string()],
                }],
            };
            let plain = plain_si(&report);
            assert_eq!(plain.findings.len(), 1, "{check:?} produced no finding");
            let f = &plain.findings[0];
            assert!(!f.what.is_empty() && !f.why.is_empty() && !f.fix.is_empty());
        }
    }

    #[test]
    fn actionable_info_note_is_promoted_to_heads_up_even_when_healthy() {
        // Fix #3: an off-target controlled-impedance info note (the 171-ohm USB
        // case) must NOT be silently dropped by --plain. The verdict can still be
        // "healthy" (it is not a finding), but the note appears under "Heads up".
        let report = SiReport {
            findings: vec![SiFinding {
                check: SiCheck::ControlledImpedance,
                severity: SiSeverity::Info,
                message: "/USB_D+ / /USB_D-: Zdiff ~ 171 ohm [target 90 ohm USB]: estimate +90% from target - info only (defaulted stackup)".to_string(),
                refs: vec!["J1".to_string()],
                nets: vec!["/USB_D+".to_string(), "/USB_D-".to_string()],
            }],
        };
        let plain = plain_si(&report);
        // Not a finding (so it never escalates to a failure), but the verdict must
        // ACKNOWLEDGE the note rather than claim "no problems found", and the note
        // itself survives under "Heads up".
        assert!(plain.findings.is_empty());
        assert_eq!(plain.heads_up.len(), 1, "off-target info promoted");
        let verdict = plain.verdict().to_lowercase();
        assert!(
            verdict.contains("worth a look") && !verdict.contains("no signal-integrity problems"),
            "verdict must point at the heads-up, not claim 'no problems found': {verdict}"
        );
        let rendered = plain.render();
        assert!(
            rendered.contains("Heads up"),
            "render must include the Heads up section: {rendered}"
        );
        assert!(
            rendered.contains("171 ohm"),
            "the value is shown: {rendered}"
        );
    }

    #[test]
    fn within_tolerance_info_note_is_not_promoted() {
        // A "- ok" / within-tolerance info note is a pure observation; it must
        // NOT nag the user in plain mode.
        let report = SiReport {
            findings: vec![SiFinding {
                check: SiCheck::ControlledImpedance,
                severity: SiSeverity::Info,
                message: "Zdiff ~ 92 ohm vs target 90 ohm (+2%, within +-10%) - ok".to_string(),
                refs: vec!["J1".to_string()],
                nets: vec![],
            }],
        };
        let plain = plain_si(&report);
        assert!(
            plain.heads_up.is_empty(),
            "within-tolerance note not promoted"
        );
    }

    #[test]
    fn si_info_notes_are_not_findings() {
        let report = SiReport {
            findings: vec![SiFinding {
                check: SiCheck::CrystalLoadCap,
                severity: SiSeverity::Info,
                message: "computed CL = 18 pF".to_string(),
                refs: vec!["Y1".to_string()],
                nets: vec![],
            }],
        };
        let plain = plain_si(&report);
        assert!(plain.findings.is_empty());
        assert!(plain.verdict().to_lowercase().contains("healthy"));
    }

    #[test]
    fn every_fault_kind_maps_to_a_template() {
        let kinds = [
            FaultKind::Overcurrent,
            FaultKind::SurgeCurrent,
            FaultKind::Overpower,
            FaultKind::Overvoltage,
            FaultKind::ReverseBias,
            FaultKind::PinOvercurrent,
            FaultKind::Short,
        ];
        for kind in kinds {
            let f = FaultEvent {
                component: "IC3906".to_string(),
                kind,
                value: 0.689,
                limit: 0.1,
                t: 0.01,
                destroyed: false,
            };
            let plain = plain_faults(&[f]);
            assert_eq!(plain.findings.len(), 1);
            let pf = &plain.findings[0];
            assert!(pf.what.contains("IC3906"));
            assert!(!pf.why.is_empty() && !pf.fix.is_empty());
        }
    }

    #[test]
    fn pin_overcurrent_reads_like_the_brief_example() {
        // The brief's worked example: a transistor pushed to 689 mA past 100 mA.
        let f = FaultEvent {
            component: "IC3906".to_string(),
            kind: FaultKind::PinOvercurrent,
            value: 0.689,
            limit: 0.1,
            t: 0.01,
            destroyed: false,
        };
        let plain = plain_faults(&[f]);
        let pf = &plain.findings[0];
        assert!(pf.what.contains("689 mA"), "what was: {}", pf.what);
        assert!(pf.what.contains("100 mA"), "what was: {}", pf.what);
    }

    #[test]
    fn destroyed_fault_is_serious() {
        let f = FaultEvent {
            component: "C1".to_string(),
            kind: FaultKind::Overvoltage,
            value: 16.0,
            limit: 6.3,
            t: 0.01,
            destroyed: true,
        };
        let plain = plain_faults(&[f]);
        assert_eq!(plain.findings[0].level, PlainLevel::Serious);
        assert!(plain.findings[0].why.to_lowercase().contains("failure"));
    }

    /// The device-decode plain-language template covers every decoder in that
    /// family, so it must not narrate one part's story. A blind first-use test
    /// read an eFuse current-limit finding explained as a USB-C PD voltage
    /// table and filed the whole finding as copy-paste noise, which is the
    /// worst outcome for a check whose numbers were right.
    #[test]
    fn device_decode_plain_text_is_not_usb_c_specific() {
        let mut report = NetLintReport::default();
        report.findings.push(LintFinding {
            check: LintCheck::DeviceDecode,
            severity: Severity::Medium,
            message: "U19 eFuse connector budget: R48 = 100 ohm decodes to 12.85 A minimum".into(),
            refs: vec!["U19".to_string()],
            nets: vec!["ILIM".to_string()],
        });
        let why = plain_netlint(&report).findings[0].why.to_lowercase();
        assert!(
            !why.contains("here, the usb-c voltage"),
            "the template must not assert this finding IS the PD case: {why}"
        );
        assert!(
            why.contains("efuse") && why.contains("timer"),
            "it should name the decoder family's range, not one member: {why}"
        );
        assert!(
            why.contains("no simulation model"),
            "the template must answer the blind tester's doubt about deciding \
             on an unresolved part: {why}"
        );
    }

    #[test]
    fn findings_render_serious_first() {
        let mut report = NetLintReport::default();
        report.findings.push(LintFinding {
            check: LintCheck::LedCurrentSanity,
            severity: Severity::Low,
            message: String::new(),
            refs: vec!["R1".to_string()],
            nets: vec!["LED".to_string()],
        });
        report.findings.push(LintFinding {
            check: LintCheck::MissingI2cPullup,
            severity: Severity::High,
            message: String::new(),
            refs: vec!["U1".to_string()],
            nets: vec!["SDA".to_string()],
        });
        let plain = plain_netlint(&report);
        let rendered = plain.render();
        let serious_pos = rendered.find("SERIOUS").unwrap();
        let note_pos = rendered.find("note").unwrap();
        assert!(
            serious_pos < note_pos,
            "serious finding should render first"
        );
    }

    fn structured_short(net_b: &str, severity: &str, x: f64) -> crate::result::DrcShort {
        crate::result::DrcShort {
            net_a: "GND".into(),
            net_b: net_b.into(),
            layer: "F.Cu".into(),
            gap_mm: 0.0,
            loc_mm: [x, 2.0],
            severity: severity.into(),
            plain: if severity == "serious" {
                format!("GND shorts {net_b}")
            } else {
                format!(
                    "GND shorts {net_b}; TOOL-ONLY: Hauksbee reports this contact from an \
                     unvalidated board format; no matching KiCad-oracle confirmation is attached"
                )
            },
            fix: "separate the copper".into(),
        }
    }

    #[test]
    fn oracle_confirmed_short_renders_before_clearance_warning() {
        let mut short = structured_short("+3V3", "serious", 1.0);
        short.attach_oracle_agreement("10.0.5");
        let clearance = crate::result::DrcGroup {
            net_a: "SDA".into(),
            net_b: "SCL".into(),
            layer: "F.Cu".into(),
            count: 1,
            below_count: 1,
            at_limit: false,
            min_gap_mm: 0.1,
            min_gap_loc_mm: [3.0, 4.0],
            rule_mm: 0.2,
            between: "track ↔ track".into(),
            plain: "SDA vs SCL below rule".into(),
            fix: "increase spacing".into(),
        };
        let report = crate::result::DrcStructured {
            clearance_rule_mm: 0.2,
            primitive_count: 4,
            shorts: vec![short],
            violations: vec![clearance],
            at_limit: Vec::new(),
            version_warning: Some("KiCad 10 format is unvalidated".into()),
            suppression_note: None,
        };

        let rendered = plain_drc_structured(&report).render();
        let short_pos = rendered.find("[SERIOUS]").expect("short severity");
        let clearance_pos = rendered.find("[WARNING]").expect("clearance severity");
        assert!(
            short_pos < clearance_pos,
            "a confirmed short must outrank a clearance warning:\n{rendered}"
        );
    }

    #[test]
    fn order_triage_buckets_confirmed_short_ratings_unconfirmed_short_and_clean_usb() {
        let mut confirmed = structured_short("+3V3", "serious", 1.0);
        confirmed.attach_oracle_agreement("10.0.5");
        let drc = crate::result::DrcStructured {
            clearance_rule_mm: 0.2,
            primitive_count: 6,
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

        let triage = order_triage(&drc, &lint, &SiReport::default(), Some(&usb), true, 7);
        assert_eq!(triage.do_not_order.len(), 2, "{triage:#?}");
        assert!(
            triage
                .do_not_order
                .iter()
                .any(|item| item.contains("+3V3") && item.contains("KiCad")),
            "{triage:#?}"
        );
        assert!(
            triage
                .do_not_order
                .iter()
                .any(|item| item.contains("eFuse connector budget")),
            "{triage:#?}"
        );
        assert_eq!(triage.inspect_before_ordering.len(), 1, "{triage:#?}");
        assert!(
            triage.inspect_before_ordering[0].contains("PYRO4_FIRE")
                && triage.inspect_before_ordering[0].contains("Tool-only"),
            "{triage:#?}"
        );
        assert_eq!(triage.checked_and_ok.len(), 1, "{triage:#?}");
        assert!(triage.checked_and_ok[0].contains("USB-C CC"));
        let rendered = triage.render();
        assert!(rendered.starts_with("== ORDER / DON'T-ORDER TRIAGE =="));
        assert!(rendered.contains("NOT COVERED: 7 parts lack"));
        assert!(rendered.contains("DETAIL: Full findings and evidence follow"));

        let empty = OrderTriage::default().render();
        assert_eq!(empty.matches("  - Empty.").count(), 3, "{empty}");
    }
}
