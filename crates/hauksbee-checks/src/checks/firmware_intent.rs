//! What the firmware means to use, against what the board connects.
//!
//! A firmware image says which hardware it uses before it runs: the interrupt
//! vector table of an AVR names every interrupt the program handles. A handler
//! for the Timer/Counter1 capture event is written because the program times an
//! external signal, and that event fires only from the part's one capture pad
//! (`ICP1`). If that pad is connected to nothing on the board, the handler can
//! never run and the measurement the program exists to make never happens, a
//! fault no static check of the board alone can see, because nothing on the
//! board says the capture was wanted.
//!
//! The check is given the vectors each part's image handles ([`FirmwareIntent`],
//! read by the engine from the ELF or HEX) and the card's
//! `[models.pin_capability.timer_capture]` table, which names the pad. It speaks
//! only when the firmware handles a capture vector and every pad the card lists
//! for that capture input is unconnected: a pad on a net with no other pin, or no
//! net at all. A capture pad wired to anything is not judged (what it is wired to
//! is the other checks' business). It is Medium: the board may be a test fixture
//! the capture input is clipped onto, but the firmware as built does nothing
//! with it.

use std::collections::{BTreeMap, BTreeSet};

use hauksbee_extract::{
    Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity,
};
use hauksbee_models::ModelLibrary;

/// The interrupt vectors each AVR part's firmware handles, by simavr part name
/// (`atmega328p`), as the vector names of the part's datasheet
/// (`TIMER1_CAPT`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FirmwareIntent {
    by_part: BTreeMap<String, BTreeSet<String>>,
}

impl FirmwareIntent {
    /// Record that firmware for `part` handles `vectors`.
    pub fn insert<'a>(&mut self, part: &str, vectors: impl IntoIterator<Item = &'a str>) {
        self.by_part
            .entry(part.to_owned())
            .or_default()
            .extend(vectors.into_iter().map(str::to_owned));
    }

    /// The vectors handled for `part`.
    fn vectors(&self, part: &str) -> Option<&BTreeSet<String>> {
        self.by_part.get(part)
    }

    /// Whether nothing is recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_part.is_empty()
    }
}

/// The simavr parts (`atmega328p`) of the processors on `board`, which is what
/// the engine needs to know to read a firmware image's vector table for each.
#[must_use]
pub fn simavr_parts(board: &ExtractedBoard, lib: &ModelLibrary) -> BTreeSet<String> {
    super::resolved_components(board, lib)
        .filter_map(|(_, model)| {
            model
                .params
                .get_str("backend")
                .and_then(|b| b.strip_prefix("simavr:"))
                .map(str::to_owned)
        })
        .collect()
}

/// The capture pin a capture vector fires from: `TIMER1_CAPT` is `ICP1`.
fn capture_pin_of(vector: &str) -> Option<String> {
    let n = vector.strip_prefix("TIMER")?.strip_suffix("_CAPT")?;
    (!n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).then(|| format!("ICP{n}"))
}

/// Whether the pad description `text` names `label` as a whole word
/// (`D8 = ICP1 (PB0)` names `ICP1`, `ICP10` does not).
fn names(text: &str, label: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| w.eq_ignore_ascii_case(label))
}

/// Whether pad `number` of `mcu` shares a net with a pin of another part.
fn pad_connected(board: &ExtractedBoard, mcu: &Component, number: &str) -> bool {
    let Some(net) = mcu
        .pins
        .iter()
        .find(|p| p.number.trim() == number)
        .and_then(|p| p.net)
        .filter(|n| *n != 0)
    else {
        return false;
    };
    board
        .components
        .iter()
        .filter(|c| c.reference != mcu.reference)
        .any(|c| c.pins.iter().any(|p| p.net == Some(net)))
}

/// Flag every capture interrupt the firmware handles whose capture pad the board
/// leaves unconnected.
#[must_use]
pub fn firmware_intent_lint(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    intent: &FirmwareIntent,
) -> NetLintReport {
    let mut report = NetLintReport::default();
    if intent.is_empty() {
        return report;
    }
    for (mcu, model) in super::resolved_components(board, lib) {
        let Some(part) = model
            .params
            .get_str("backend")
            .and_then(|b| b.strip_prefix("simavr:"))
        else {
            continue;
        };
        let (Some(vectors), Some(table)) = (
            intent.vectors(part),
            model.pin_capability.timer_capture.as_ref(),
        ) else {
            continue;
        };
        for vector in vectors {
            let Some(label) = capture_pin_of(vector) else {
                continue;
            };
            let pads: Vec<&String> = table
                .pads
                .iter()
                .filter(|(_, what)| names(what, &label))
                .map(|(pad, _)| pad)
                .collect();
            if pads.iter().any(|pad| pad_connected(board, mcu, pad)) {
                continue;
            }
            let where_ = if pads.is_empty() {
                format!(
                    "the card '{}' lists no pad of this package for {label}",
                    model.id
                )
            } else {
                format!(
                    "{label} is pad {} of the package ({}), and that pad is connected to nothing",
                    pads.iter()
                        .map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join(" / "),
                    pads.iter()
                        .filter_map(|p| model.pins.get(*p))
                        .map(|r| r.to_ascii_uppercase())
                        .collect::<Vec<_>>()
                        .join(" / "),
                )
            };
            report.findings.push(LintFinding {
                check: LintCheck::FirmwareUnconnectedPad,
                severity: Severity::Medium,
                message: format!(
                    "{} ({}) runs firmware that handles the {vector} interrupt, which fires only \
                     from the capture input {label}, but {where_}: the handler can never run. \
                     Wire the signal the firmware times to that pad, or take the capture out of \
                     the firmware if the board measures it another way",
                    mcu.reference, mcu.value,
                ),
                refs: vec![mcu.reference.clone()],
                nets: Vec::new(),
            });
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board as build, part};
    use super::*;

    fn board(capture_net: i64, partner: bool) -> ExtractedBoard {
        // ATmega328P, 28-pin: pad 14 is PB0 / ICP1.
        let mut parts = vec![part(
            "U1",
            "ATmega328P",
            &[("7", 1), ("8", 2), ("14", capture_net)],
        )];
        if partner {
            parts.push(part("J1", "Conn_01x02", &[("1", 3), ("2", 2)]));
        }
        build(&[(1, "VCC"), (2, "GND"), (3, "SENSE")], parts)
    }

    fn intent(vectors: &[&str]) -> FirmwareIntent {
        let mut i = FirmwareIntent::default();
        i.insert("atmega328p", vectors.iter().copied());
        i
    }

    fn run(b: &ExtractedBoard, i: &FirmwareIntent) -> Vec<LintFinding> {
        firmware_intent_lint(b, &ModelLibrary::builtin(), i).findings
    }

    #[test]
    fn a_capture_handler_with_the_capture_pad_unconnected_is_medium() {
        let found = run(&board(0, false), &intent(&["TIMER1_CAPT"]));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].check, LintCheck::FirmwareUnconnectedPad);
        assert_eq!(found[0].severity, Severity::Medium);
        assert!(
            found[0].message.contains("TIMER1_CAPT"),
            "{}",
            found[0].message
        );
        assert!(found[0].message.contains("ICP1") && found[0].message.contains("pad 14"));
    }

    #[test]
    fn a_capture_pad_on_a_net_with_no_other_pin_is_unconnected() {
        let b = board(3, false);
        assert_eq!(run(&b, &intent(&["TIMER1_CAPT"])).len(), 1);
    }

    #[test]
    fn a_capture_pad_wired_to_a_signal_is_silent() {
        let b = board(3, true);
        assert!(run(&b, &intent(&["TIMER1_CAPT"])).is_empty());
    }

    #[test]
    fn firmware_that_does_not_handle_the_capture_vector_is_silent() {
        assert!(run(&board(0, false), &intent(&["TIMER0_OVF", "USART_RX"])).is_empty());
        assert!(run(&board(0, false), &FirmwareIntent::default()).is_empty());
    }

    #[test]
    fn firmware_for_another_part_is_not_judged_against_this_one() {
        let mut other = FirmwareIntent::default();
        other.insert("atmega168", ["TIMER1_CAPT"]);
        assert!(run(&board(0, false), &other).is_empty());
    }
}
