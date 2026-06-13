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
    DrcReport, ItemKind, LintCheck, NetLintReport, Severity, SiCheck, SiReport, SiSeverity,
    ViolationKind,
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
    /// "What it is" — the headline, in everyday language.
    pub what: String,
    /// "Why it matters" — the consequence if left as-is.
    pub why: String,
    /// "Suggested fix" — the concrete thing to change.
    pub fix: String,
}

/// A whole report, translated: the headline verdict plus the ordered findings.
#[derive(Debug, Clone, Default)]
pub struct PlainReport {
    /// What was checked, e.g. "Copper spacing (DRC)". Drives the verdict line.
    pub subject: String,
    pub findings: Vec<PlainFinding>,
}

impl PlainReport {
    fn new(subject: &str) -> Self {
        PlainReport {
            subject: subject.to_string(),
            findings: Vec::new(),
        }
    }

    fn push(&mut self, level: PlainLevel, what: String, why: String, fix: String) {
        self.findings.push(PlainFinding {
            level,
            what,
            why,
            fix,
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
            return format!("Looks healthy: no {} problems found.", self.subject.to_lowercase());
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
        if self.findings.is_empty() {
            return s;
        }
        let _ = writeln!(s);
        for (i, f) in self.sorted().iter().enumerate() {
            let _ = writeln!(s, "{}. [{}] {}", i + 1, f.level.tag(), f.what);
            let _ = writeln!(s, "     Why it matters: {}", f.why);
            let _ = writeln!(s, "     What to do:     {}", f.fix);
            let _ = writeln!(s);
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
        let a = if f.net_a_name.is_empty() { "an unnamed net" } else { &f.net_a_name };
        let b = if f.net_b_name.is_empty() { "an unnamed net" } else { &f.net_b_name };
        let where_ = format!("near x={:.1} mm, y={:.1} mm on layer {}", f.x, f.y, f.layer);
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
                    f.gap_mm, report.clearance_mm,
                ),
                "They are not shorted today, but the gap is below the spacing the board asks for. Small manufacturing variation, a solder smear, or contamination could bridge them, so it is a reliability risk rather than a guaranteed failure.".to_string(),
                "Open up the spacing between these two so the gap meets your clearance rule, or relax the rule deliberately if you know this spot is fine.".to_string(),
            ),
        }
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
                format!("The LED + resistor on {parts} looks mis-sized — its current is outside the sensible range for an indicator."),
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
                "Some chips read certain pins at the instant they reset to decide how to boot (which mode, where to load code from). If that pin is at the wrong level — or wobbling, e.g. a clock signal sits on it — the chip can boot into the wrong mode or fail to start.".to_string(),
                format!("Hold \"{net}\" firmly at the level the datasheet wants during reset, usually with a pull-up or pull-down resistor, and keep fast/active signals off that pin until after boot."),
            ),
            LintCheck::McuResourceConflict => (
                format!("Two different functions need the same internal block of the microcontroller at once ({parts}). {}", f.message),
                "A microcontroller has a fixed number of internal blocks (timers/PWM channels, communication units, pin groups). Two board features have been wired to pins that share one block, so the chip physically cannot run both at the same time — one feature will not work.".to_string(),
                "Move one of the functions to a different pin that maps to a free internal block, or give up one of the two features. The chip's datasheet pin-mux table shows which pins share which block.".to_string(),
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
                "Adjust the trace width and pair spacing for your actual layer stackup so the estimated impedance hits the target (tools and the formulas in docs/SI_CHECKS.md give the geometry); or have the fab build a controlled-impedance stackup to your spec.".to_string(),
            ),
        };
        out.push(level, what, why, fix);
    }
    out
}

/// Keep an embedded expert message short enough to sit inside a plain sentence
/// without dumping a whole table row.
fn short_msg(msg: &str) -> String {
    let m = msg.trim();
    let first = m.split(['.', ';']).next().unwrap_or(m).trim();
    let truncated: String = first.chars().take(90).collect();
    truncated
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
                "Every part has a maximum safe voltage. Above it the insulation/junction can break down — a capacitor can short or vent, a semiconductor can be punched through — often instantly and permanently.".to_string(),
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
            item_a: Item { kind: ItemKind::Track, net: 1, owner: String::new() },
            item_b: Item { kind: ItemKind::Pad, net: 2, owner: "U1".to_string() },
        }
    }

    #[test]
    fn drc_short_is_serious_with_why_and_fix() {
        let mut report = DrcReport { clearance_mm: 0.2, ..Default::default() };
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
        };
        let plain = plain_drc(&report);
        assert_eq!(plain.findings[0].level, PlainLevel::Warning);
        assert_eq!(plain.serious_count(), 0);
        assert!(plain.verdict().contains("none serious"));
    }

    #[test]
    fn empty_drc_reads_healthy() {
        let plain = plain_drc(&DrcReport { clearance_mm: 0.2, ..Default::default() });
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
        assert!(serious_pos < note_pos, "serious finding should render first");
    }
}
