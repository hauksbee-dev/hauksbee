//! Spec errors with human-friendly messages, including "did you mean ...?"
//! suggestions for unknown net names.

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
                        writeln!(f, " — did you mean: {}?", suggestions.join(", "))?;
                    }
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
    fn no_wild_suggestions_for_garbage() {
        let known = vec!["ANALOG_VDD".to_string(), "+5V".to_string()];
        let s = near_matches("zzzzzzzzzz", &known, 3);
        assert!(s.is_empty(), "garbage should not match: {s:?}");
    }
}
