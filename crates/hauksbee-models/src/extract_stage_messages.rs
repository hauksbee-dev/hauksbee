//! The words of the datasheet stage of a batch.
//!
//! The end-tally lines that say how long getting the datasheets took and which
//! parts were given up on. Shown by the CLI and by anything else that runs a
//! batch.

use std::time::Duration;

fn parts(n: usize) -> String {
    format!("{n} part{}", if n == 1 { "" } else { "s" })
}

/// A duration to the second, as `45 s` or `3 min 12 s`.
#[must_use]
pub fn took(d: Duration) -> String {
    let secs = d.as_secs() + u64::from(d.subsec_millis() >= 500);
    if secs < 60 {
        format!("{secs} s")
    } else {
        format!("{} min {:02} s", secs / 60, secs % 60)
    }
}

/// The tally line on the stage that fetches datasheets outside the agents'
/// places: how many parts it covered, how long it ran (to the last one), and how
/// many datasheets it found.
#[must_use]
pub fn stage_line(covered: usize, found: usize, ran: Duration) -> String {
    format!(
        "datasheets: {} fetched alongside the drafting in {}; {found} found",
        parts(covered),
        took(ran)
    )
}

/// The tally line on parts nothing could be fetched for: they never took an
/// agent's place and are listed for the person to supply by hand. `None` when
/// there are none.
#[must_use]
pub fn given_up_line(given_up: usize) -> Option<String> {
    (given_up > 0).then(|| {
        format!(
            "given up for no datasheet: {} (no agent was started for them; supply them by hand, \
             listed below)",
            parts(given_up)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_in_seconds_then_minutes() {
        assert_eq!(took(Duration::from_millis(400)), "0 s");
        assert_eq!(took(Duration::from_secs(45)), "45 s");
        assert_eq!(took(Duration::from_millis(59_600)), "1 min 00 s");
        assert_eq!(took(Duration::from_secs(192)), "3 min 12 s");
    }

    #[test]
    fn the_tally_lines_say_what_the_stage_did() {
        assert_eq!(
            stage_line(14, 11, Duration::from_secs(192)),
            "datasheets: 14 parts fetched alongside the drafting in 3 min 12 s; 11 found"
        );
        assert!(stage_line(1, 1, Duration::from_secs(3)).contains("1 part fetched"));
        assert_eq!(given_up_line(0), None);
        let line = given_up_line(3).unwrap();
        assert!(
            line.starts_with("given up for no datasheet: 3 parts"),
            "{line}"
        );
        assert!(given_up_line(1).unwrap().contains(": 1 part ("));
    }
}
