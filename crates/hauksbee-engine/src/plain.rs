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
    SiSeverity, ViolationKind,
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
    pub findings: Vec<PlainFinding>,
    /// Actionable info-level notes promoted into a "Heads up:" section. These are
    /// NOT counted as findings (they don't change the verdict), but they are also
    /// NEVER silently dropped in `--plain` mode: the 171-ohm USB-impedance note
    /// the hobbyist persona lost is exactly this (Fix #3 / Theme A). A "Looks
    /// healthy" verdict that hides the only actionable observation is the breach
    /// of trust we refuse to ship. Each note carries a what / why / what-to-do
    /// gloss so it translates the jargon rather than dumping it.
    pub heads_up: Vec<HeadsUp>,
}

impl PlainReport {
    fn new(subject: &str) -> Self {
        PlainReport {
            subject: subject.to_string(),
            findings: Vec::new(),
            heads_up: Vec::new(),
        }
    }

    fn push(&mut self, level: PlainLevel, what: String, why: String, fix: String) {
        self.findings.push(PlainFinding {
            level,
            what,
            why,
            fix,
            loc_mm: None,
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
            what,
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
        if n == 0 {
            // No failures, but if there are actionable heads-up notes (e.g. a USB
            // pair off its impedance target), don't claim "no problems found",
            // that buries the one thing the user may have come to check. Point at
            // the notes below without escalating them to a failure (cry wolf).
            if !self.heads_up.is_empty() {
                let hn = self.heads_up.len();
                let things = if hn == 1 { "thing" } else { "things" };
                return format!(
                    "No {} failures, but {hn} {things} worth a look (see below).",
                    self.subject.to_lowercase()
                );
            }
            return format!(
                "Looks healthy: no {} problems found.",
                self.subject.to_lowercase()
            );
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

    // On an unvalidated board format (KiCad 10+), the shorts may be phantom (an
    // unhandled zone fill engulfing every net), so they carry a downgraded
    // severity (set once in DrcStructured::from_report). Surface the caveat as a
    // never-dropped heads-up so the verdict is "worth a look", not "N serious".
    if let Some(w) = &st.version_warning {
        out.heads_up.push(HeadsUp::glossed(
            format!("The copper-short results below may be unreliable on this file: {w}"),
            "This board was saved by a newer KiCad than hauksbee's copper reader was \
             validated against, so its zone/fill format can be misread: a filled ground \
             pour can look like it shorts every net it surrounds, producing shorts that \
             are not really there (false alarms), or hiding a real one.",
            "Treat any short below as \"check, don't panic\": open the board in KiCad and \
             run its own DRC (Inspect -> Design Rules Checker) to confirm. If KiCad reports \
             no short at that spot, it is a false alarm from the format gap.",
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
        out.push_at(
            level,
            format!("Two separate connections, \"{a}\" and \"{b}\", are touching, {where_}."),
            format!(
                "These are meant to be electrically separate. Where they touch they become one connection (a short), so \"{a}\" and \"{b}\" will be forced to the same voltage. That usually means the board does the wrong thing, and if one is a power rail it can pull large current and overheat."
            ),
            "Pull the two pieces of copper apart so there is a clear gap between them, or remove the bit of copper that bridges them. If they really are supposed to connect, give them the same net name.".to_string(),
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
    for g in &st.at_limit {
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

    out
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
            LintCheck::MissingI2cPullup => (
                format!("The I2C data line \"{net}\" has no pull-up resistor."),
                "I2C is an \"open-drain\" bus: parts can only pull the line low, so it needs a resistor to a power rail to pull it back high. Without one the line floats, and the bus will not communicate reliably (or at all).".to_string(),
                format!("Add a pull-up resistor (typically 2.2k to 10k ohm) from \"{net}\" to its power rail. One resistor per signal line (SDA and SCL)."),
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
            LintCheck::McuResourceConflict => (
                format!("Two different functions need the same internal block of the microcontroller at once ({parts}). {}", f.message),
                "A microcontroller has a fixed number of internal blocks (timers/PWM channels, communication units, pin groups). Two board features have been wired to pins that share one block, so the chip physically cannot run both at the same time: one feature will not work.".to_string(),
                "Move one of the functions to a different pin that maps to a free internal block, or give up one of the two features. The chip's datasheet pin-mux table shows which pins share which block.".to_string(),
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
                "Some chips read a resistor-divider voltage on a config pin and decode it against a datasheet table to pick a mode (here, the USB-C voltage a PD sink requests). If the chosen resistors land the pin in the wrong band, the chip silently selects the wrong mode: every resistor is in spec and every wire connects, so a normal value/short check cannot see it.".to_string(),
                "Re-pick the divider resistors so the pin voltage lands in the intended datasheet band, using the single pull-up / single pull-down the datasheet specifies per setting (not a permanent pull-down with an extra switched leg). Check the part's decode table and any min/max override note.".to_string(),
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
                "Adjust the trace width and pair spacing for your actual layer stackup so the estimated impedance hits the target (tools and the formulas in docs/checks/SI_CHECKS.md give the geometry); or have the fab build a controlled-impedance stackup to your spec.".to_string(),
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
            out.heads_up.push(note);
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
    // "pair" only when it IS one: the single-ended estimates name one net.
    let subject = if f.refs.len() >= 2 || f.nets.len() >= 2 {
        "A high-speed trace pair"
    } else {
        "A high-speed trace"
    };
    Some(match f.check {
        SiCheck::ControlledImpedance => HeadsUp::glossed(
            format!("{subject} ({affected}) is off its impedance target ({core})."),
            "Fast links like USB (90 ohm differential) or Ethernet need their traces to \
             present a specific impedance so the signal does not reflect off the wire and \
             smear. This is a computed estimate, flagged only because the board did not \
             formally declare a controlled-impedance stackup; it is informational, not a \
             confirmed failure, but a value well off target on a real high-speed net can \
             make the link marginal or fail to enumerate.",
            "If this pair is NOT a high-speed link (USB/Ethernet/HDMI), you can ignore it. \
             If it is: adjust the trace width and pair spacing for your actual layer \
             stackup so the estimate lands near the target (the formulas are in \
             docs/checks/SI_CHECKS.md), or ask your fab to build a controlled-impedance stackup \
             to spec.",
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
        };
        let plain = plain_drc(&report);
        assert_eq!(plain.findings[0].level, PlainLevel::Warning);
        assert_eq!(plain.serious_count(), 0);
        assert!(plain.verdict().contains("none serious"));
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
        let checks = [
            (LintCheck::MissingI2cPullup, Severity::High),
            (LintCheck::FloatingControlPin, Severity::High),
            (LintCheck::LedCurrentSanity, Severity::Medium),
            (LintCheck::OutputContention, Severity::High),
            (LintCheck::StrapPin, Severity::High),
            (LintCheck::McuResourceConflict, Severity::High),
            (LintCheck::DesignatorFootprintMismatch, Severity::Medium),
            (LintCheck::ValuePackageSanity, Severity::Medium),
            (LintCheck::PlaceholderValue, Severity::Medium),
            (LintCheck::UncheckedMcu, Severity::Low),
        ];
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
}
