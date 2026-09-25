//! Spec-loading and validation errors, plus the "did you mean ...?" net-name
//! suggester. [`SpecError`] carries the IO / TOML / invalid / unknown-net cases a
//! spec can fail on, and [`near_matches`] ranks known net names by edit distance
//! so a typo'd net reference points the user at the real name instead of failing
//! blankly.

use std::fmt;

pub use hauksbee_engine::commands::common::cap_context_width;
use hauksbee_engine::reports::cosim::levenshtein;

/// An error loading or validating a spec.
#[derive(Debug)]
pub enum SpecError {
    Io(String),
    Toml {
        file: String,
        message: String,
    },
    Invalid(String),
    /// The solver deliberately refused or failed an analysis. Keep the typed
    /// source so the CLI can distinguish honesty refusals from malformed specs
    /// without parsing the message.
    Solver {
        context: &'static str,
        source: hauksbee_solve::SolveError,
    },
    /// One or more referenced nets do not exist on the board. Each entry is
    /// (net, context, near-matches).
    UnknownNets(Vec<(String, &'static str, Vec<String>)>),
    /// Several independent errors found in ONE pass. Validation collects every
    /// error it can rather than stopping at the first, so a spec author fixes
    /// one invocation's worth of findings, not one finding per invocation.
    Many(Vec<Self>),
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(m) => write!(f, "{m}"),
            Self::Toml { file, message } => {
                write!(f, "could not parse spec {file}: {message}")
            }
            Self::Invalid(m) => write!(f, "invalid spec: {m}"),
            Self::Solver { context, source } => {
                write!(f, "invalid spec: {context}: {source}")
            }
            Self::UnknownNets(items) => {
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
            Self::Many(errors) => {
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

impl std::error::Error for SpecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Solver { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl SpecError {
    /// Fold independently collected errors into one result: none is `Ok`, one
    /// comes back as itself, several as [`SpecError::Many`].
    pub fn from_many(mut errs: Vec<Self>) -> Result<(), Self> {
        match errs.len() {
            0 => Ok(()),
            1 => Err(errs.remove(0)),
            _ => Err(Self::Many(errs)),
        }
    }

    /// A `map_err` adapter: `.map_err(SpecError::invalid("reading BOM"))`
    /// yields `Invalid("reading BOM: <cause>")`.
    pub fn invalid<E: fmt::Display>(context: impl Into<String>) -> impl Fn(E) -> Self {
        let context = context.into();
        move |e| Self::Invalid(format!("{context}: {e}"))
    }

    /// The [`SpecError::Io`] twin of [`SpecError::invalid`].
    pub fn io<E: fmt::Display>(context: impl Into<String>) -> impl Fn(E) -> Self {
        let context = context.into();
        move |e| Self::Io(format!("{context}: {e}"))
    }

    /// CLI exit code for this pre-verdict error. A typed solver refusal means
    /// the requested analysis is invalid (3); other setup/spec errors remain 2.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Solver {
                source: hauksbee_solve::SolveError::Refused { .. },
                ..
            } => hauksbee_engine::result::EXIT_INVALID_FOR_ANALYSIS,
            Self::Many(errors) => errors.iter().map(Self::exit_code).max().unwrap_or(2),
            _ => 2,
        }
    }
}

/// Return up to `limit` known names closest to `target`, ranked by edit
/// distance, preferring substring/case-insensitive matches. Only returns names
/// within a sensible distance so the suggestions are actually useful.
#[must_use]
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

/// Suggestions for a component reference that is not on the board, ranked by
/// edit distance and capped.
///
/// [`near_matches`] is tuned for net names: long, wordy, and worth a generous
/// cutoff. Reference designators are the opposite. They are two or three
/// characters over a tiny alphabet, so half-the-target-length lets almost
/// anything through: `R99` is within three edits of both `D1` and `U1`, and
/// offering those as "did you mean" is noise wearing help's clothes.
///
/// So: within two edits, and the designator's first letter has to match. That
/// first letter is the component CLASS, the one character of a reference nobody
/// fat-fingers into a different one, and it is what makes `R99 -> D1` obviously
/// not a typo while `R_Shnt15301 -> R_Shunt15301` obviously is.
#[must_use]
pub fn near_refs(target: &str, known: &[String], limit: usize) -> Vec<String> {
    let class = designator_class(target);
    let t = target.to_ascii_lowercase();
    let mut scored: Vec<(usize, &String)> = known
        .iter()
        .filter(|name| !name.trim().is_empty())
        .filter(|name| designator_class(name) == class)
        .map(|name| (levenshtein(&t, &name.to_ascii_lowercase()), name))
        // Distance 0 means the caller rejected a reference the board HAS; that
        // is a caller bug, not a typo, and suggesting the same string back is
        // worse than saying nothing.
        .filter(|(d, _)| *d > 0 && *d <= 2)
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)));
    scored.dedup_by(|a, b| a.1 == b.1);
    scored
        .into_iter()
        .take(limit)
        .map(|(_, name)| name.clone())
        .collect()
}

/// The component-class letter of a reference designator, lowercased: `r` of
/// `R99` and of `R_Shunt15301`, `u` of `U1`. `None` for anything that does not
/// start with a letter.
fn designator_class(r: &str) -> Option<char> {
    r.trim()
        .chars()
        .next()
        .filter(char::is_ascii_alphabetic)
        .map(|c| c.to_ascii_lowercase())
}

/// The `; did you mean: a, b?` clause for a suggestion list, or empty when
/// there is nothing worth suggesting. One formatter so every call site's
/// wording (and the parser in `check` that splits it back out) agrees.
#[must_use]
pub fn suggestion_clause(suggestions: &[String]) -> String {
    if suggestions.is_empty() {
        String::new()
    } else {
        format!("; did you mean: {}?", suggestions.join(", "))
    }
}

/// The closest option to `target` within edit distance 2, for a "did you
/// mean ...?" hint on a closed vocabulary (assertion kinds, supply kinds,
/// peripheral types). Distance 2 covers the real mistakes (a dropped letter, a
/// swapped pair, an added character) without ever suggesting something the
/// user plainly did not type; a full-list dump already follows in the error,
/// so a wild guess here would only mislead.
#[must_use]
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
#[must_use]
pub fn did_you_mean_hint(target: &str, options: &[&str]) -> String {
    did_you_mean(target, options).map_or_else(String::new, |s| format!(" (did you mean '{s}'?)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(std::string::ToString::to_string).collect()
    }

    #[test]
    fn shared_diagnostics_preserve_unicode_and_boundaries() {
        assert_eq!(cap_context_width(""), "");
        assert_eq!(cap_context_width("µ\r\n\r\nΩ\n"), "µ\n\nΩ");
        for width in [199, 200] {
            let line = "界".repeat(width);
            assert_eq!(cap_context_width(&line), line);
        }
        let capped = "界".repeat(200);
        assert_eq!(
            cap_context_width(&format!("{capped}x\nshort")),
            format!("{capped} ...(line truncated)\nshort")
        );
        assert_eq!(levenshtein("", "µΩ"), 2);
        assert_eq!(levenshtein("µΩ", ""), 2);
        assert_eq!(did_you_mean("é", &["e"]).as_deref(), Some("e"));
    }

    #[test]
    fn solver_refusal_exit_code_uses_variant_not_message() {
        let refusal = SpecError::Solver {
            context: "AC analysis",
            source: hauksbee_solve::SolveError::Refused {
                message: "soundness could not be established".into(),
            },
        };
        assert_eq!(
            refusal.exit_code(),
            hauksbee_engine::result::EXIT_INVALID_FOR_ANALYSIS
        );
        assert_eq!(
            SpecError::Invalid("user text says refused".into()).exit_code(),
            2
        );
    }

    #[test]
    fn net_suggestions_are_close_named_and_never_the_empty_net() {
        let known = names(&["", "ANALOG_VDD", "+5V", "GND", "DIGITAL_VDD"]);
        assert_eq!(
            near_matches("ANALOG_VDDD", &known, 3)
                .first()
                .map(String::as_str),
            Some("ANALOG_VDD")
        );
        // KiCad's unnamed "no net" bucket must not leak (it renders as a bare comma).
        let s = near_matches("+5W", &known, 5);
        assert!(
            !s.iter().any(std::string::String::is_empty),
            "empty net leaked: {s:?}"
        );
        assert_eq!(s.first().map(String::as_str), Some("+5V"));
        assert!(
            near_matches("zzzzzzzzzz", &known, 3).is_empty(),
            "garbage must not match"
        );
    }

    #[test]
    fn ref_suggestions_stay_inside_the_designator_family() {
        // A net-name cutoff would offer D1 and U1 for R99; neither is a plausible typo of an R.
        let known = names(&["D1", "U1", "R1", "R9", "C7", "R_Shunt15301"]);
        let s = near_refs("R99", &known, 3);
        assert!(!s.iter().any(|r| r == "D1" || r == "U1"), "{s:?}");
        assert_eq!(
            s.first().map(String::as_str),
            Some("R9"),
            "closest first: {s:?}"
        );
        assert!(
            near_refs("Q3", &known, 3).is_empty(),
            "no family on the board, no suggestion"
        );
        assert_eq!(
            near_refs("R_Shunt15302", &known, 3),
            names(&["R_Shunt15301"])
        );
    }

    #[test]
    fn did_you_mean_catches_a_typo_within_two_edits_and_stays_quiet_otherwise() {
        let kinds = ["voltage", "uart", "toggle", "no_faults"];
        assert_eq!(did_you_mean("voltag", &kinds).as_deref(), Some("voltage"));
        assert_eq!(did_you_mean("frobnicate", &kinds), None);
        // An exact vocabulary member is not a typo.
        assert_eq!(did_you_mean("voltage", &kinds), None);
    }
}
