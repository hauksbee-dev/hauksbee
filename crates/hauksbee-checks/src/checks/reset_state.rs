//! What a net rests at while the MCU driving it is held in reset.
//!
//! An MCU pad is high impedance while the part is in reset (and until firmware
//! configures it), unless the MCU card says the pad rests at something else
//! (`[models.pin_capability.reset_mode]`: an internal pull that is on out of
//! reset, a pad that drives a level). So a net whose only holders are MCU pads
//! has no level in the reset window: it floats there, whatever the firmware
//! drives later. This is the one place that judgement is made. It is the
//! static form of the "reset-state pin modes" step of
//! `docs/design/checks-from-simulation.md`: the per-pad mode lives on the card
//! and this module reads it; a floating-node detector run over a reset-window
//! solve can take its reset-window drive set from [`pad_reset_mode`] the same
//! way, and `enable_floats_in_reset` (in `control_held_off`) is its first
//! consumer.
//!
//! The judgement is conservative. A net counts as floating in reset only when
//! every part that holds it is an MCU pad shown to be high impedance there; a
//! rail, a pull resistor, a driver that is not an MCU pad, a switch, a header
//! or any part the classifier cannot show weak is something that may set a
//! level, and the net is left to the checks that know that part.

use std::collections::BTreeMap;

use hauksbee_extract::{Component, Pin};
use hauksbee_models::{ModelEntry, ResetPadMode};

use super::floating_inputs::{Classifier, Hold, Weak, WeakKind};

/// What `pad` of the MCU `reference` rests at in reset, from its card:
/// high impedance unless the card lists the pad.
pub(super) fn pad_reset_mode(
    models: &BTreeMap<&str, ModelEntry>,
    reference: &str,
    pad: &str,
) -> ResetPadMode {
    models
        .get(reference)
        .map_or_else(ResetPadMode::default, |m| m.reset_pad_mode(pad))
}

/// How the net on one pad rests while its MCU holders are in reset.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ResetRest {
    /// Something sets a level in reset: a rail, a pull, or a driver that is
    /// not an MCU pad.
    Set,
    /// Held only by MCU pads, at least one of which the card says is pulled or
    /// driven in reset: the net rests at that pad's level, not floating.
    McuPulled {
        /// The pads, as the classifier names them.
        pads: Vec<Weak>,
        /// The level they hold the net at (`true` high), or `None` when the
        /// pads pull or drive it both ways. A driven pad outranks a pulled one.
        high: Option<bool>,
    },
    /// Nothing is on the net at all.
    Open,
    /// Held only by MCU pads that are high impedance in reset: the net floats.
    /// The pads, as the classifier names them.
    McuHiZ(Vec<Weak>),
    /// Weak holders of other kinds (a switch, a header, a jumper, a transistor)
    /// share the net with, or replace, the MCU pads: they may set a level in
    /// the reset window, and this module does not judge which.
    Mixed,
}

/// The level a set of pulled or driven pad modes holds a net at: the driven
/// pads' level when any pad drives, else the pulls'; `None` when they disagree.
fn rest_level(modes: &[ResetPadMode]) -> Option<bool> {
    let level =
        |up: ResetPadMode, down: ResetPadMode| match (modes.contains(&up), modes.contains(&down)) {
            (true, false) => Some(Some(true)),
            (false, true) => Some(Some(false)),
            (true, true) => Some(None),
            (false, false) => None,
        };
    level(ResetPadMode::DrivesHigh, ResetPadMode::DrivesLow)
        .or_else(|| level(ResetPadMode::PullUp, ResetPadMode::PullDown))
        .flatten()
}

/// How the net on `pin` of `comp` rests while the MCUs that hold it are in
/// reset.
pub(super) fn reset_rest(classify: &Classifier<'_>, comp: &Component, pin: &Pin) -> ResetRest {
    match classify.hold(comp, pin) {
        Hold::Defined => ResetRest::Set,
        Hold::Floats => ResetRest::Open,
        Hold::MayFloat(weak) => {
            let all_mcu = weak
                .iter()
                .all(|w| matches!(w.kind, WeakKind::Gpio | WeakKind::McuOutput));
            if !all_mcu {
                return ResetRest::Mixed;
            }
            let modes: Vec<ResetPadMode> = weak
                .iter()
                .filter_map(|w| w.pad.as_ref())
                .map(|(reference, pad)| pad_reset_mode(classify.models, reference, pad))
                .filter(|m| !m.is_hi_z())
                .collect();
            if modes.is_empty() {
                return ResetRest::McuHiZ(weak);
            }
            ResetRest::McuPulled {
                high: rest_level(&modes),
                pads: weak,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::test_support::{board, part};
    use hauksbee_models::ModelLibrary;
    use std::collections::BTreeSet;

    /// An MCU whose pad 2 is pulled up in reset and whose pad 1 is not listed
    /// (high impedance).
    const CARD: &str = r#"
[[models]]
id = "rest_mcu"
kind = "mcu"
description = "reset mode fixture"
[models.match]
value_re = "^RESTMCU$"
[models.pins]
"1" = "gpio_a"
"2" = "gpio_b"
"3" = "vdd"
[models.pin_capability.reset_mode]
basis = "fixture"
[models.pin_capability.reset_mode.pads]
"2" = "pull_up"
"#;

    /// The rest of net `/NET` seen from pad 1 of U2 when the MCU's `pad` is on it
    /// and `extra` parts are added.
    fn rest(pad: &str, extra: Vec<Component>) -> ResetRest {
        let dir = hauksbee_testkit::fresh_scratch_dir("reset_state", "card");
        std::fs::write(dir.join("models.toml"), CARD).unwrap();
        let mut lib = ModelLibrary::builtin();
        assert!(lib.load_user_dir(&dir).is_empty(), "test card must load");
        let mut parts = vec![
            part("U1", "RESTMCU", &[(pad, 3), ("3", 2)]),
            part("U2", "SomeIC", &[("1", 3), ("2", 1)]),
        ];
        parts.extend(extra);
        let b = board(&[(1, "GND"), (2, "+3V3"), (3, "/NET")], parts);
        let models: BTreeMap<&str, ModelEntry> = crate::checks::resolved_components(&b, &lib)
            .map(|(c, m)| (c.reference.as_str(), m))
            .collect();
        let senses = BTreeSet::new();
        let classify = Classifier {
            board: &b,
            models: &models,
            senses: &senses,
            strict: true,
            open_jumpers: true,
        };
        let u2 = b.components.iter().find(|c| c.reference == "U2").unwrap();
        reset_rest(&classify, u2, &u2.pins[0])
    }

    #[test]
    fn a_pad_the_card_does_not_list_floats_in_reset_and_a_listed_pull_does_not() {
        assert!(matches!(rest("1", vec![]), ResetRest::McuHiZ(w) if w.len() == 1));
        assert!(
            matches!(rest("2", vec![]), ResetRest::McuPulled { high: Some(true), ref pads } if pads.len() == 1),
            "pulled up in reset"
        );
    }

    #[test]
    fn a_resistor_to_a_rail_or_a_header_changes_the_verdict() {
        let pull = part("R1", "100k", &[("1", 3), ("2", 1)]);
        assert_eq!(rest("1", vec![pull]), ResetRest::Set);
        let mut header = part("J1", "Conn", &[("1", 3), ("2", 1)]);
        header.footprint = "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical".into();
        assert_eq!(rest("1", vec![header]), ResetRest::Mixed);
    }
}
