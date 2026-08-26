//! Overruling one finding without switching the check off.
//!
//! A verification tool that cries wolf gets switched off, and a switched-off
//! tool catches nothing. The corpus gate exists to keep checks from crying
//! wolf in the first place, but no corpus is every board, so eventually a check
//! fires on a design where it is wrong. Before this, the only two answers were
//! to live with a red build or to stop running the check, and the second one is
//! how a team quietly loses the rest of the suite along with the bad rule.
//!
//! A waiver is the third answer: overrule this finding, on this board, for a
//! stated reason, until a stated date.
//!
//! Two rules make it a record rather than a mute button, and both are enforced
//! at load rather than left to discipline:
//!
//! 1. **A reason is required.** A waiver with no reason is indistinguishable
//!    from a bug six months later, when the person who added it has moved on.
//! 2. **An expiry is required.** A waiver that never expires is a permanently
//!    disabled check wearing a different hat. On the date it lapses the finding
//!    comes back, which forces someone to look again and decide.
//!
//! Waived findings are reported, never hidden. A spec accumulating waivers
//! should look like a spec accumulating waivers.
//!

use hauksbee_ir::evidence::{parse_ymd_epoch_days, RunDate, WaiverState};
use serde::Deserialize;
use std::path::Path;

/// The file name looked for beside a board when no `--waivers` path is given.
pub const DEFAULT_WAIVER_FILE: &str = "hauksbee-waivers.toml";

/// One overruled finding.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Waiver {
    /// Which check produced the finding: "si", "lint", "drc".
    pub check: String,
    /// The specific rule, e.g. "controlled_impedance", "strap_pin", "short".
    pub kind: String,
    /// Nets the finding must touch. A waiver matches only if every net listed
    /// here appears in the finding.
    #[serde(default)]
    pub nets: Vec<String>,
    /// Component references the finding must touch, same rule as `nets`.
    #[serde(default)]
    pub refs: Vec<String>,
    /// Why this finding is wrong, or why it is accepted. Required.
    pub reason: String,
    /// The date this waiver stops applying, `YYYY-MM-DD`. Required.
    pub until: String,
}

/// Every waiver loaded for a run, plus what each one did.
#[derive(Debug, Clone, Default)]
pub struct WaiverSet {
    waivers: Vec<Waiver>,
    /// Parallel to `waivers`: how many findings each one matched this run. An
    /// entry that matched nothing is stale, and the report says so, because a
    /// waiver outliving the finding it was written for is how a file rots into
    /// a list nobody reads.
    hits: Vec<usize>,
    /// Today, as days since the Unix epoch. Captured once per run so a long run
    /// cannot have a waiver lapse halfway through it.
    today: RunDate,
}

/// One waived finding, for the report.
#[derive(Debug, Clone)]
pub struct WaivedFinding {
    pub check: String,
    pub kind: String,
    pub subject: String,
    pub reason: String,
    pub until: String,
}

/// What a loaded waiver file was wrong about.
#[derive(Debug)]
pub enum WaiverError {
    Io(String),
    Toml(String),
    Invalid(String),
}

impl std::fmt::Display for WaiverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WaiverError::Io(m) | WaiverError::Toml(m) | WaiverError::Invalid(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for WaiverError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaiverFile {
    #[serde(default, rename = "waive")]
    waive: Vec<Waiver>,
}

impl WaiverSet {
    /// Load a waiver file using the run's captured, credibility-checked date.
    pub fn load_at(path: &Path, today: RunDate) -> Result<Self, WaiverError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| WaiverError::Io(format!("reading {}: {e}", path.display())))?;
        let file: WaiverFile = toml::from_str(&text).map_err(|e| {
            WaiverError::Toml(format!("parsing {}: {}", path.display(), e.message()))
        })?;
        for (i, w) in file.waive.iter().enumerate() {
            w.validate(i, path)?;
        }
        let hits = vec![0; file.waive.len()];
        Ok(WaiverSet {
            waivers: file.waive,
            hits,
            today,
        })
    }

    /// Load a waiver file, dating it against the system clock.
    pub fn load(path: &Path) -> Result<Self, WaiverError> {
        Self::load_at(path, RunDate::from_system_clock())
    }

    /// Look for the default waiver file beside `board`. Absent is not an error:
    /// most boards have no waivers, and that is the healthy state.
    pub fn discover(board: &Path) -> Result<Self, WaiverError> {
        let candidate = board
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(DEFAULT_WAIVER_FILE);
        if candidate.is_file() {
            Self::load(&candidate)
        } else {
            Ok(Self::default())
        }
    }

    pub fn is_empty(&self) -> bool {
        self.waivers.is_empty()
    }

    pub fn len(&self) -> usize {
        self.waivers.len()
    }

    fn state(&self, w: &Waiver) -> WaiverState {
        self.today.waiver_state(&w.until)
    }

    /// The first active waiver matching this finding, if any. Recording the hit
    /// needs `&mut`, so callers use [`Self::take_waiver`].
    fn find_match(
        &self,
        check: &str,
        kind: &str,
        nets: &[String],
        refs: &[String],
    ) -> Option<usize> {
        self.waivers.iter().enumerate().position(|(_, w)| {
            w.check.eq_ignore_ascii_case(check)
                && w.kind.eq_ignore_ascii_case(kind)
                && self.state(w) == WaiverState::Active
                && w.nets
                    .iter()
                    .all(|n| nets.iter().any(|f| f.eq_ignore_ascii_case(n)))
                && w.refs
                    .iter()
                    .all(|r| refs.iter().any(|f| f.eq_ignore_ascii_case(r)))
        })
    }

    /// Whether an active waiver covers this finding, recording the hit.
    ///
    /// `subject` is what the report shows so a reader can tell which finding was
    /// overruled without opening the waiver file.
    pub fn take_waiver(
        &mut self,
        check: &str,
        kind: &str,
        nets: &[String],
        refs: &[String],
        subject: &str,
    ) -> Option<WaivedFinding> {
        let i = self.find_match(check, kind, nets, refs)?;
        self.hits[i] += 1;
        let w = &self.waivers[i];
        Some(WaivedFinding {
            check: w.check.clone(),
            kind: w.kind.clone(),
            subject: subject.to_string(),
            reason: w.reason.clone(),
            until: w.until.clone(),
        })
    }

    /// Waivers that are past their date. Their findings gate the build again,
    /// and the report names them so the red is explainable.
    pub fn expired(&self) -> Vec<&Waiver> {
        self.waivers
            .iter()
            .filter(|w| self.state(w) == WaiverState::Expired)
            .collect()
    }

    /// Split a check's findings into the ones that still gate and the ones an
    /// active waiver covers.
    ///
    /// Both halves are kept: the gate reads the first, the report prints the
    /// second. Dropping the waived half instead would make an overruled finding
    /// invisible, and a board carrying overruled findings has to look like one.
    pub fn partition<T>(
        &mut self,
        check: &str,
        findings: Vec<T>,
        key: impl Fn(&T) -> (String, Vec<String>, Vec<String>, String),
    ) -> (Vec<T>, Vec<WaivedFinding>) {
        let mut gating = Vec::with_capacity(findings.len());
        let mut waived = Vec::new();
        for f in findings {
            let (kind, nets, refs, subject) = key(&f);
            match self.take_waiver(check, &kind, &nets, &refs, &subject) {
                Some(w) => waived.push(w),
                None => gating.push(f),
            }
        }
        (gating, waived)
    }

    /// Active waivers that matched nothing this run. Either the finding is
    /// fixed and the waiver should go, or it no longer describes what fires.
    pub fn stale(&self) -> Vec<&Waiver> {
        self.waivers
            .iter()
            .zip(&self.hits)
            .filter(|(w, hits)| **hits == 0 && self.state(w) == WaiverState::Active)
            .map(|(w, _)| w)
            .collect()
    }
}

impl Waiver {
    fn validate(&self, index: usize, path: &Path) -> Result<(), WaiverError> {
        let where_ = format!("waiver {} in {}", index + 1, path.display());
        if self.check.trim().is_empty() {
            return Err(WaiverError::Invalid(format!("{where_}: `check` is empty")));
        }
        if self.kind.trim().is_empty() {
            return Err(WaiverError::Invalid(format!("{where_}: `kind` is empty")));
        }
        if self.reason.trim().is_empty() {
            return Err(WaiverError::Invalid(format!(
                "{where_}: `reason` is required. A waiver with no reason cannot be told \
                 apart from a bug once whoever wrote it has moved on"
            )));
        }
        if parse_ymd_epoch_days(&self.until).is_none() {
            return Err(WaiverError::Invalid(format!(
                "{where_}: `until` must be a date as YYYY-MM-DD, got '{}'. An expiry is \
                 required: a waiver that never lapses is a disabled check wearing a \
                 different hat",
                self.until
            )));
        }
        if self.nets.is_empty() && self.refs.is_empty() {
            return Err(WaiverError::Invalid(format!(
                "{where_}: needs `nets` or `refs`. Without one, the waiver silences the \
                 '{}' rule across the whole board rather than the one finding you judged, \
                 which is the same as turning the check off",
                self.kind
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, body: &str) -> std::path::PathBuf {
        let p = dir.join(DEFAULT_WAIVER_FILE);
        std::fs::write(&p, body).unwrap();
        p
    }

    const GOOD: &str = r#"
[[waive]]
check = "si"
kind = "controlled_impedance"
nets = ["USB_DP"]
reason = "measured 92 ohm on the fab's stackup; our stackup file is wrong"
until = "2030-01-01"
"#;

    #[test]
    fn an_active_waiver_covers_its_finding_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), GOOD);
        let mut set = WaiverSet::load_at(&p, RunDate::from_epoch_days(20_663)).unwrap();

        assert!(
            set.take_waiver(
                "si",
                "controlled_impedance",
                &["USB_DP".into()],
                &[],
                "USB_DP"
            )
            .is_some(),
            "the finding it names is covered"
        );
        assert!(
            set.take_waiver(
                "si",
                "controlled_impedance",
                &["ETH_TX".into()],
                &[],
                "ETH_TX"
            )
            .is_none(),
            "a different net is a different finding"
        );
        assert!(
            set.take_waiver("lint", "controlled_impedance", &["USB_DP".into()], &[], "x")
                .is_none(),
            "a different check is a different finding"
        );
    }

    #[test]
    fn an_expired_waiver_stops_covering_anything() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), GOOD);
        // One day after the 2030-01-01 expiry.
        let mut set = WaiverSet::load_at(
            &p,
            RunDate::from_epoch_days(parse_ymd_epoch_days("2030-01-02").unwrap()),
        )
        .unwrap();
        assert!(
            set.take_waiver(
                "si",
                "controlled_impedance",
                &["USB_DP".into()],
                &[],
                "USB_DP"
            )
            .is_none(),
            "the whole point of an expiry is that the finding comes back"
        );
        assert_eq!(set.expired().len(), 1, "and the report can explain the red");
    }

    #[test]
    fn a_waiver_is_in_force_on_its_expiry_date() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), GOOD);
        let mut set = WaiverSet::load_at(
            &p,
            RunDate::from_epoch_days(parse_ymd_epoch_days("2030-01-01").unwrap()),
        )
        .unwrap();
        assert!(
            set.take_waiver(
                "si",
                "controlled_impedance",
                &["USB_DP".into()],
                &[],
                "USB_DP"
            )
            .is_some(),
            "'until Friday' includes Friday"
        );
    }

    #[test]
    fn a_waiver_without_a_reason_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            r#"
[[waive]]
check = "si"
kind = "controlled_impedance"
nets = ["USB_DP"]
reason = "   "
until = "2030-01-01"
"#,
        );
        let err = WaiverSet::load(&p).unwrap_err().to_string();
        assert!(err.contains("`reason` is required"), "{err}");
    }

    #[test]
    fn a_waiver_without_an_expiry_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            r#"
[[waive]]
check = "si"
kind = "controlled_impedance"
nets = ["USB_DP"]
reason = "measured on the fab's stackup"
until = "forever"
"#,
        );
        let err = WaiverSet::load(&p).unwrap_err().to_string();
        assert!(err.contains("YYYY-MM-DD"), "{err}");
    }

    #[test]
    fn a_rule_wide_waiver_is_refused() {
        // Without a net or a ref this silences the rule everywhere, which is
        // switching the check off with extra steps.
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            r#"
[[waive]]
check = "si"
kind = "controlled_impedance"
reason = "the SI check is noisy"
until = "2030-01-01"
"#,
        );
        let err = WaiverSet::load(&p).unwrap_err().to_string();
        assert!(err.contains("needs `nets` or `refs`"), "{err}");
    }

    #[test]
    fn a_waiver_that_matched_nothing_is_reported_stale() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), GOOD);
        let mut set = WaiverSet::load_at(&p, RunDate::from_epoch_days(20_663)).unwrap();
        assert_eq!(set.stale().len(), 1, "nothing matched it yet");
        set.take_waiver(
            "si",
            "controlled_impedance",
            &["USB_DP".into()],
            &[],
            "USB_DP",
        );
        assert!(
            set.stale().is_empty(),
            "it matched, so it is earning its place"
        );
    }

    #[test]
    fn an_impossible_date_is_refused() {
        assert_eq!(
            parse_ymd_epoch_days("2026-02-30"),
            None,
            "February has no 30th"
        );
        assert_eq!(
            parse_ymd_epoch_days("2026-13-01"),
            None,
            "there is no month 13"
        );
        assert_eq!(
            parse_ymd_epoch_days("2025-02-29"),
            None,
            "2025 is not a leap year"
        );
        assert!(parse_ymd_epoch_days("2024-02-29").is_some(), "2024 is");
    }

    #[test]
    fn shared_calendar_matches_known_epochs() {
        assert_eq!(parse_ymd_epoch_days("1970-01-01"), Some(0));
        assert_eq!(parse_ymd_epoch_days("1970-01-02"), Some(1));
        assert_eq!(parse_ymd_epoch_days("1969-12-31"), Some(-1));
        assert_eq!(parse_ymd_epoch_days("2000-03-01"), Some(11017));
    }

    #[test]
    fn a_clock_behind_the_build_cannot_resurrect_a_lapsed_waiver() {
        // The fail-open direction. A container with no RTC, or a backdated job,
        // reads a clock from before this build existed; believing it would make
        // every expired waiver active again and silently reopen gates somebody
        // closed on purpose.
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            r#"
[[waive]]
check = "si"
kind = "controlled_impedance"
nets = ["USB_DP"]
reason = "expired on purpose"
until = "2026-01-01"
"#,
        );
        // A clock reading 2020 is rejected by RunDate instead of being clamped
        // into a believable date. Unknown dates expire every waiver.
        let mut set = WaiverSet::load_at(&p, RunDate::from_epoch_days(18_262)).unwrap();
        assert!(
            set.take_waiver("si", "controlled_impedance", &["USB_DP".into()], &[], "x")
                .is_none(),
            "an untrustworthy clock cannot cover any finding"
        );
        let mut unknown = WaiverSet::load_at(&p, RunDate::unknown()).unwrap();
        assert!(
            unknown
                .take_waiver("si", "controlled_impedance", &["USB_DP".into()], &[], "x")
                .is_none(),
            "an absent clock reading is fail-closed too"
        );
    }

    #[test]
    fn a_missing_waiver_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        let set = WaiverSet::discover(&board).unwrap();
        assert!(
            set.is_empty(),
            "most boards have no waivers, and that is fine"
        );
    }
}
