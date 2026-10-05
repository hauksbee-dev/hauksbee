//! Findings that repeat across parts: one cell instantiated ninety times says
//! the same thing ninety times, so the human reports print it once.
//!
//! Two findings repeat when they differ only in which part they name. The
//! designator is masked out of the message and the rest must match exactly,
//! numbers included, so a finding that reads differently for one part (another
//! current, another rating) never joins a group that hides the difference.
//! Machine output keeps every finding; it carries a group key instead.

/// The message with its one implicated part masked.
///
/// The message comes back unchanged when it does not name exactly one part. A designator counts only as a whole
/// token (`IC10006` in `IC10006_q2` is the part; the `R1` in `R10` is not), so
/// the sub-element suffix a check appends to a part stays in the shape.
#[must_use]
pub fn repeat_shape(message: &str, refs: &[String]) -> String {
    let [part] = refs else {
        return message.to_string();
    };
    if part.is_empty() {
        return message.to_string();
    }
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    let mut before: Option<char> = None;
    while let Some(at) = rest.find(part.as_str()) {
        let (head, tail) = rest.split_at(at);
        let prev = head.chars().next_back().or(before);
        let after = &tail[part.len()..];
        let whole = !prev.is_some_and(char::is_alphanumeric)
            && !after.chars().next().is_some_and(char::is_alphanumeric);
        out.push_str(head);
        if whole {
            out.push_str("<part>");
        } else {
            out.push_str(part);
        }
        before = part.chars().next_back();
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Items in report order, merged where `key` returns the same `Some` value.
/// The group sits where its first member sat; an item with no key stands alone.
pub fn group_repeats<'a, T>(
    items: impl IntoIterator<Item = &'a T>,
    key: impl Fn(&T) -> Option<String>,
) -> Vec<Vec<&'a T>> {
    let mut groups: Vec<Vec<&'a T>> = Vec::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for item in items {
        match key(item) {
            Some(k) => {
                if let Some(&at) = seen.get(&k) {
                    groups[at].push(item);
                } else {
                    seen.insert(k, groups.len());
                    groups.push(vec![item]);
                }
            }
            None => groups.push(vec![item]),
        }
    }
    groups
}

/// `"90 parts like IC10006, IC10106, IC10206 and 87 more"`: the lead of a
/// grouped line, naming the count and a few of the parts.
#[must_use]
pub fn describe_parts(parts: &[&str]) -> String {
    const SHOWN: usize = 3;
    let n = parts.len();
    let shown = parts
        .iter()
        .take(SHOWN)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if n > SHOWN {
        format!("{n} parts like {shown} and {} more", n - SHOWN)
    } else {
        format!("{n} parts like {shown}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(r: &str) -> Vec<String> {
        vec![r.to_string()]
    }

    #[test]
    fn the_part_and_its_sub_elements_are_masked() {
        let m = "IC10006 is overstressed: IC10006_q2 carries 235.2 mA";
        assert_eq!(
            repeat_shape(m, &refs("IC10006")),
            "<part> is overstressed: <part>_q2 carries 235.2 mA"
        );
    }

    #[test]
    fn a_longer_designator_is_not_a_match() {
        let m = "R1 sits beside R10 and XR1";
        assert_eq!(
            repeat_shape(m, &refs("R1")),
            "<part> sits beside R10 and XR1"
        );
    }

    #[test]
    fn two_parts_or_none_leave_the_message_alone() {
        let m = "R1 and R2 fight";
        assert_eq!(repeat_shape(m, &["R1".into(), "R2".into()]), m);
        assert_eq!(repeat_shape(m, &[]), m);
    }

    #[test]
    fn different_numbers_do_not_group() {
        let a = repeat_shape("Q1 carries 235 mA", &refs("Q1"));
        let b = repeat_shape("Q2 carries 236 mA", &refs("Q2"));
        let c = repeat_shape("Q3 carries 235 mA", &refs("Q3"));
        assert_ne!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn groups_keep_report_order_and_leave_unkeyed_items_alone() {
        let items = ["a1", "b1", "a2", "x", "a3"];
        let groups = group_repeats(&items, |s| {
            s.starts_with(['a', 'b']).then(|| s[..1].to_string())
        });
        let shown: Vec<Vec<&str>> = groups
            .iter()
            .map(|g| g.iter().map(|s| **s).collect())
            .collect();
        assert_eq!(shown, vec![vec!["a1", "a2", "a3"], vec!["b1"], vec!["x"]]);
    }

    #[test]
    fn the_lead_names_the_count_and_a_few_parts() {
        assert_eq!(describe_parts(&["A", "B"]), "2 parts like A, B");
        assert_eq!(
            describe_parts(&["A", "B", "C", "D", "E"]),
            "5 parts like A, B, C and 2 more"
        );
    }
}
