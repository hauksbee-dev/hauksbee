//! Spec-loading and validation errors, plus the "did you mean ...?" net-name
//! suggester. [`SpecError`] carries the IO / TOML / invalid / unknown-net cases a
//! spec can fail on, and [`near_matches`] ranks known net names by edit distance
//! so a typo'd net reference points the user at the real name instead of failing
//! blankly.

use std::fmt;

/// An error loading or validating a spec.
#[derive(Debug)]
pub enum SpecError {
    Io(String),
    Toml {
        file: String,
        message: String,
    },
    Invalid(String),
    /// One or more referenced nets do not exist on the board. Each entry is
    /// (net, context, near-matches).
    UnknownNets(Vec<(String, &'static str, Vec<String>)>),
    /// Several independent errors found in ONE pass. Validation collects every
    /// error it can rather than stopping at the first, so a spec author fixes
    /// one invocation's worth of findings, not one finding per invocation.
    Many(Vec<SpecError>),
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpecError::Io(m) => write!(f, "{m}"),
            SpecError::Toml { file, message } => {
                write!(f, "could not parse spec {file}: {message}")
            }
            SpecError::Invalid(m) => write!(f, "invalid spec: {m}"),
            SpecError::UnknownNets(items) => {
                writeln!(f, "spec references net(s) not found on the board:")?;
                for (net, ctx, suggestions) in items {
                    write!(f, "  '{net}' (in {ctx})")?;
                    if suggestions.is_empty() {
                        writeln!(f)?;
                    } else {
                        writeln!(f, "; did you mean: {}?", suggestions.join(", "))?;
                    }
                }
                Ok(())
            }
            SpecError::Many(errors) => {
                for (i, e) in errors.iter().enumerate() {
                    if i > 0 {
                        writeln!(f)?;
                    }
                    write!(f, "{e}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for SpecError {}

/// Return up to `limit` known names closest to `target`, ranked by edit
/// distance, preferring substring/case-insensitive matches. Only returns names
/// within a sensible distance so the suggestions are actually useful.
pub fn near_matches(target: &str, known: &[String], limit: usize) -> Vec<String> {
    let t_lower = target.to_ascii_lowercase();
    let mut scored: Vec<(usize, &String)> = known
        .iter()
        // Skip the empty/unnamed net (KiCad's "no net" bucket): suggesting it
        // produces a bare leading comma in the "did you mean: , +5V?" list and
        // is never a real net a user meant to reference.
        .filter(|name| !name.trim().is_empty())
        .map(|name| {
            let n_lower = name.to_ascii_lowercase();
            // Substring containment is a strong signal; give it a big bonus.
            let contains = n_lower.contains(&t_lower) || t_lower.contains(&n_lower);
            let dist = levenshtein(&t_lower, &n_lower);
            let score = if contains {
                dist.saturating_sub(3)
            } else {
                dist
            };
            (score, name)
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)));
    // Keep only reasonably-close suggestions: within half the target length,
    // or any substring match.
    let cutoff = (target.len() / 2).max(3);
    scored
        .into_iter()
        .filter(|(score, _)| *score <= cutoff)
        .take(limit)
        .map(|(_, name)| name.clone())
        .collect()
}

/// The closest option to `target` within edit distance 2, for a "did you
/// mean ...?" hint on a closed vocabulary (assertion kinds, supply kinds,
/// peripheral types). Distance 2 covers the real mistakes (a dropped letter, a
/// swapped pair, an added character) without ever suggesting something the
/// user plainly did not type; a full-list dump already follows in the error,
/// so a wild guess here would only mislead.
pub fn did_you_mean(target: &str, options: &[&str]) -> Option<String> {
    let t = target.to_ascii_lowercase();
    options
        .iter()
        .map(|o| (levenshtein(&t, &o.to_ascii_lowercase()), *o))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        // Distance 0 means the vocabulary DOES contain the word and the caller
        // rejected it anyway; that is a caller bug, not a typo, so no hint.
        .filter(|(d, _)| *d > 0)
        .map(|(_, o)| o.to_string())
}

/// Format the `did_you_mean` hint as the parenthetical the error messages
/// splice in: `" (did you mean 'voltage'?)"`, or empty when nothing is close.
pub fn did_you_mean_hint(target: &str, options: &[&str]) -> String {
    match did_you_mean(target, options) {
        Some(s) => format!(" (did you mean '{s}'?)"),
        None => String::new(),
    }
}

/// Width-cap every line of an error message that quotes file content (the
/// TOML parser's caret-annotated snippet). A machine-written input can be one
/// enormous line, and quoting it whole buries the actual error; anything past
/// the cap is elided with a marker. Lines within the cap pass through intact.
pub fn cap_context_width(msg: &str) -> String {
    const MAX: usize = 200;
    msg.lines()
        .map(|line| {
            if line.chars().count() <= MAX {
                line.to_string()
            } else {
                let head: String = line.chars().take(MAX).collect();
                format!("{head} ...(line truncated)")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Classic Levenshtein edit distance.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_close_net_names() {
        let known = vec![
            "ANALOG_VDD".to_string(),
            "+5V".to_string(),
            "GND".to_string(),
            "DIGITAL_VDD".to_string(),
        ];
        let s = near_matches("ANALOG_VDDD", &known, 3);
        assert_eq!(s.first().map(String::as_str), Some("ANALOG_VDD"));
    }

    #[test]
    fn empty_net_is_never_suggested() {
        // KiCad's unnamed "no net" bucket must not leak into the suggestion list
        // (it renders as a leading comma). A near-miss of a real net still wins.
        let known = vec![String::new(), "+5V".to_string(), "GND".to_string()];
        let s = near_matches("+5W", &known, 5);
        assert!(!s.iter().any(|n| n.is_empty()), "empty net leaked: {s:?}");
        assert_eq!(s.first().map(String::as_str), Some("+5V"));
    }

    #[test]
    fn no_wild_suggestions_for_garbage() {
        let known = vec!["ANALOG_VDD".to_string(), "+5V".to_string()];
        let s = near_matches("zzzzzzzzzz", &known, 3);
        assert!(s.is_empty(), "garbage should not match: {s:?}");
    }

    #[test]
    fn did_you_mean_catches_a_typo_within_two_edits() {
        let kinds = ["voltage", "uart", "toggle", "no_faults"];
        assert_eq!(did_you_mean("voltag", &kinds).as_deref(), Some("voltage"));
        assert_eq!(did_you_mean("volage", &kinds).as_deref(), Some("voltage"));
        assert_eq!(did_you_mean("tooggle", &kinds).as_deref(), Some("toggle"));
    }

    #[test]
    fn did_you_mean_stays_quiet_when_nothing_is_close() {
        let kinds = ["voltage", "uart", "toggle"];
        assert_eq!(did_you_mean("frobnicate", &kinds), None);
        // An exact vocabulary member is not a typo; no hint.
        assert_eq!(did_you_mean("voltage", &kinds), None);
    }
}
