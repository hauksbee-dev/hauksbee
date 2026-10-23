//! Required pin bias: the pull-up, pull-down or defined idle level a card says
//! a (non-strap) pin needs.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! Some pins are only specified with a bias on them. An open-drain flag or
//! interface output pulls low and cannot pull high, so without a pull-up it
//! never reads high; a ROM-bootloader UART receive line that floats reads noise
//! as start bits. The card says so per pin (`[[models.pin_bias]]`: the level,
//! whether a resistor is required or any defined level will do, the resistance
//! range the datasheet allows, and the table it is read from) and this check
//! reads the net that pin sits on:
//!
//!   - no bias where one is required: nothing on the net sets a level, or (for
//!     a required pull) only a part that holds a level some of the time does;
//!   - the opposite bias: a pull-down where the pin needs a pull-up, the pin
//!     tied to the wrong supply or ground, or a divider whose opposing pull wins;
//!   - the right pull outside the datasheet's resistance range at a tolerance
//!     corner.
//!
//! A boot strap is not judged here: a pin the card lists under `straps` belongs
//! to the strap lint (`straps.rs`). A pull on the far side of one series
//! resistor counts as the pull. A pull whose value does not read as a
//! resistance makes the range part abstain, naming the part.
//!
//! ## What it never claims
//!
//! A pin whose net is held only by an MCU pad is Medium, not High, and says the
//! firmware may enable an internal pull; a net some other part drives is silent
//! for an idle-level requirement, because that part, not this card, decides the
//! level; and a divider is judged only against the rail's midpoint, since the
//! card carries no thresholds.

use hauksbee_extract::{
    Component, ExtractedBoard, LintCheck, LintFinding, Net, NetLintReport, Pin, Severity,
    ABSTENTION_MARKER, REMEDY_MARKER,
};
use hauksbee_ir::units::format_engineering;
use hauksbee_models::netclass::may_be_supply;
use hauksbee_models::{BiasLevel, BiasNeed, ModelEntry, ModelLibrary, PinBias};
use std::collections::{BTreeMap, BTreeSet};

use super::envelope::role_net_id;
use super::floating_inputs::{Classifier, Hold, Weak, WeakKind};
use super::straps::{is_assembled_resistor, pulls_on, Pull};
use super::{is_ground_name, is_unconnected_net};

/// Ohms in the unit a datasheet prints them in.
fn ohms(r: f64) -> String {
    format_engineering(r, "Ω")
}

/// What the net of a biased pin does for the required level.
enum Verdict {
    /// The requirement is met, or a part we cannot judge decides the level.
    Fine,
    /// Nothing the card accepts sets the level.
    Missing { severity: Severity, why: String },
    /// The bias runs the wrong way.
    Opposite { why: String },
    /// The right pull is outside the datasheet's range.
    OutOfRange { why: String },
    /// The check could not read one side.
    Abstain { why: String },
}

/// The parallel combination of resistances.
fn parallel(values: impl Iterator<Item = f64>) -> f64 {
    1.0 / values.map(|r| 1.0 / r.max(1e-6)).sum::<f64>()
}

/// The pulls on the far side of one assembled series resistor whose other pad
/// is on `net`, excluding pulls that resistor itself makes.
fn pulls_through_series(board: &ExtractedBoard, net: i64) -> Vec<Pull> {
    let mut found = Vec::new();
    for (res, _) in board
        .net_members(net)
        .into_iter()
        .filter(|(c, _)| is_assembled_resistor(c))
    {
        let sides: Vec<i64> = res.pins.iter().filter_map(|p| p.net).collect();
        if sides.len() != 2 {
            continue;
        }
        let Some(&far) = sides.iter().find(|id| **id != net) else {
            continue;
        };
        let Some(far_net) = board.net(far) else {
            continue;
        };
        if is_ground_name(&far_net.name) || may_be_supply(&far_net.name) {
            continue; // a pull, not a hop
        }
        found.extend(
            pulls_on(board, far)
                .into_iter()
                .filter(|p| p.reference != res.reference),
        );
    }
    found
}

/// Whether some part other than `comp` that is not a resistor or capacitor is
/// on the net.
fn other_active_part(board: &ExtractedBoard, comp: &Component, net: i64) -> bool {
    board.net_members(net).into_iter().any(|(c, _)| {
        c.reference != comp.reference
            && !c.dnp
            && !is_assembled_resistor(c)
            && !hauksbee_extract::is_capacitor(c)
    })
}

/// The parts behind a may-float net, for the message.
fn holders(weak: &[Weak]) -> String {
    weak.iter()
        .map(|w| w.label.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The parallel resistance of `pulls`, or `None` when any value is unreadable.
fn combined(pulls: &[&Pull]) -> Option<f64> {
    pulls
        .iter()
        .map(|p| p.ohms)
        .collect::<Option<Vec<f64>>>()
        .map(|v| parallel(v.into_iter()))
}

/// Judge the net of `pin` against `row`.
fn judge(
    board: &ExtractedBoard,
    classify: &Classifier<'_>,
    (comp, pin): (&Component, &Pin),
    row: &PinBias,
) -> Verdict {
    let want_high = row.level == BiasLevel::High;
    let word = row.level.pull_word();
    let alone = |id: i64| {
        board
            .net_members(id)
            .iter()
            .all(|(c, _)| c.reference == comp.reference)
    };
    if row.open_ok
        && pin.net.filter(|id| *id != 0).is_none_or(|id| {
            alone(id) || board.net(id).is_some_and(|n| is_unconnected_net(&n.name))
        })
    {
        return Verdict::Fine; // an unused flag: nothing is wired to read it
    }
    let Some(net) = pin.net.filter(|id| *id != 0).and_then(|id| board.net(id)) else {
        return Verdict::Missing {
            severity: Severity::High,
            why: "the pin is not connected to anything".to_string(),
        };
    };
    if is_unconnected_net(&net.name) {
        return Verdict::Missing {
            severity: Severity::High,
            why: "the pin is left unconnected".to_string(),
        };
    }
    // Tied straight to a supply or to ground.
    let ground = is_ground_name(&net.name);
    if ground || may_be_supply(&net.name) {
        return if ground == want_high {
            Verdict::Opposite {
                why: format!(
                    "the pin is tied directly to '{}', the opposite of the {} level it must idle at",
                    net.name,
                    if want_high { "high" } else { "low" }
                ),
            }
        } else {
            Verdict::Fine
        };
    }

    let direct = pulls_on(board, net.id);
    let series = pulls_through_series(board, net.id);
    let (right_direct, wrong_direct): (Vec<&Pull>, Vec<&Pull>) =
        direct.iter().partition(|p| p.up == want_high);
    let right_series = series.iter().filter(|p| p.up == want_high).count();

    if right_direct.is_empty() && right_series == 0 {
        return without_right_pull(board, classify, (comp, pin), row, net, wrong_direct.first());
    }

    if let Some(v) = divider(word, &right_direct, &wrong_direct) {
        return v;
    }
    if (row.min_ohm.is_none() && row.max_ohm.is_none()) || right_direct.is_empty() {
        return Verdict::Fine; // no range stated, or a series path whose value is not judged
    }
    in_range(row, word, &right_direct)
}

/// The net has no pull of the required direction: the opposite pull, nothing
/// at all, or only parts that hold a level some of the time.
fn without_right_pull(
    board: &ExtractedBoard,
    classify: &Classifier<'_>,
    (comp, pin): (&Component, &Pin),
    row: &PinBias,
    net: &Net,
    wrong: Option<&&Pull>,
) -> Verdict {
    let want_high = row.level == BiasLevel::High;
    let word = row.level.pull_word();
    if let Some(w) = wrong {
        // Only the opposite pull. Another part driving the net decides the
        // level for an idle requirement; a required pull is still absent.
        if row.needs == BiasNeed::Idle && other_active_part(board, comp, net.id) {
            return Verdict::Fine;
        }
        let dir = if w.up { "pull-up" } else { "pull-down" };
        return Verdict::Opposite {
            why: format!(
                "the only resistor on the net is {} {} ({dir}), which holds the pin {} where the datasheet needs it {}",
                w.reference,
                w.value.trim(),
                if w.up { "high" } else { "low" },
                if want_high { "high" } else { "low" },
            ),
        };
    }
    match classify.hold(comp, pin) {
        Hold::Defined => Verdict::Fine,
        Hold::Floats => Verdict::Missing {
            severity: Severity::High,
            why: format!(
                "net '{}' has no {word} resistor and no part that sets its level",
                net.name
            ),
        },
        Hold::MayFloat(weak) => {
            let on_gpio = weak.iter().any(|w| w.kind == WeakKind::Gpio);
            let extra = if on_gpio {
                " (firmware may enable the MCU pad's internal pull, which is not a board fact this check can see, so confirm it)"
            } else {
                ""
            };
            Verdict::Missing {
                severity: Severity::Medium,
                why: format!(
                    "net '{}' has no {word} resistor, and is held only by {}, none of which sets a level whenever that part lets go{extra}",
                    net.name,
                    holders(&weak)
                ),
            }
        }
    }
}

/// A pull of the right direction exists. An opposing pull makes a divider,
/// judged at the rail's midpoint; `None` when there is no opposing pull or it
/// does not win.
fn divider(word: &str, right: &[&Pull], wrong: &[&Pull]) -> Option<Verdict> {
    if wrong.is_empty() || right.is_empty() {
        return None;
    }
    let (Some(r_right), Some(r_wrong)) = (combined(right), combined(wrong)) else {
        return Some(Verdict::Abstain {
            why: "a pull resistor's value does not read as a resistance, so the divider on the pin cannot be judged".to_string(),
        });
    };
    // Fraction of the rail at the pin if the right pull points at the rail.
    let towards_right = r_wrong / (r_right + r_wrong);
    (towards_right < 0.5).then(|| Verdict::Opposite {
        why: format!(
            "the {word} ({}) is overpowered by an opposing pull ({}), so the pin sits nearer the wrong level",
            ohms(r_right),
            ohms(r_wrong)
        ),
    })
}

/// The right pull against the datasheet's resistance range, at tolerance corners.
fn in_range(row: &PinBias, word: &str, right_direct: &[&Pull]) -> Verdict {
    if let Some(p) = right_direct.iter().find(|p| p.ohms.is_none()) {
        return Verdict::Abstain {
            why: format!(
                "{} value '{}' is not a resistance, so the {word} cannot be checked against the datasheet range",
                p.reference,
                p.value.trim()
            ),
        };
    }
    // The strongest and weakest the parallel pulls can be at tolerance corners.
    let corner = |sign: f64| {
        parallel(
            right_direct
                .iter()
                .map(|p| p.ohms.unwrap_or(f64::MAX) * (sign * p.tol).mul_add(1.0, 1.0)),
        )
    };
    let (weakest, strongest) = (corner(1.0), corner(-1.0));
    let list = right_direct
        .iter()
        .map(|p| format!("{} {}", p.reference, p.value.trim()))
        .collect::<Vec<_>>()
        .join(", ");
    if let Some(max) = row.max_ohm.filter(|m| weakest > *m) {
        return Verdict::OutOfRange {
            why: format!(
                "the {word} ({list}) is {} at its weakest tolerance corner, above the datasheet's {} maximum: too weak to hold the level",
                ohms(weakest),
                ohms(max)
            ),
        };
    }
    if let Some(min) = row.min_ohm.filter(|m| strongest < *m) {
        return Verdict::OutOfRange {
            why: format!(
                "the {word} ({list}) is {} at its strongest tolerance corner, below the datasheet's {} minimum: it loads the pin's driver",
                ohms(strongest),
                ohms(min)
            ),
        };
    }
    Verdict::Fine
}

/// The rail a pull on this part goes to, named for the remedy.
fn rail_name(board: &ExtractedBoard, comp: &Component, model: &ModelEntry) -> String {
    ["vcc", "vdd", "vio", "vddio", "vccio"]
        .iter()
        .find_map(|r| role_net_id(comp, model, r))
        .and_then(|id| board.net(id))
        .map_or_else(
            || "the part's I/O supply".to_string(),
            |n| format!("'{}'", n.name),
        )
}

/// The remedy sentence: what to add and its value range.
fn remedy(
    board: &ExtractedBoard,
    (comp, model): (&Component, &ModelEntry),
    row: &PinBias,
    net: &str,
) -> String {
    let target = if row.level == BiasLevel::High {
        rail_name(board, comp, model)
    } else {
        "ground".to_string()
    };
    let range = match (row.min_ohm, row.max_ohm) {
        (Some(lo), Some(hi)) => format!(" of {} to {}", ohms(lo), ohms(hi)),
        (None, Some(hi)) => format!(" of at most {}", ohms(hi)),
        (Some(lo), None) => format!(" of at least {}", ohms(lo)),
        (None, None) => String::new(),
    };
    format!(
        "{REMEDY_MARKER}Add (or correct) a {}{range} from '{net}' to {target}.",
        row.level.pull_word()
    )
}

/// One finding per fitted part pin whose card requires a bias the net lacks.
#[must_use]
pub fn pin_bias_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    let models: BTreeMap<&str, ModelEntry> = super::resolved_components(board, lib)
        .filter(|(_, m)| !m.pin_bias.is_empty())
        .map(|(c, m)| (c.reference.as_str(), m))
        .collect();
    let senses = BTreeSet::new();
    let classify = Classifier {
        board,
        models: &models,
        senses: &senses,
        strict: true,
    };
    for comp in &board.components {
        let Some(model) = models.get(comp.reference.as_str()) else {
            continue;
        };
        for row in &model.pin_bias {
            if model.straps.iter().any(|s| s.role == row.role) {
                continue; // the strap lint owns it
            }
            for (pad, _) in model
                .pins
                .iter()
                .filter(|(_, role)| role.eq_ignore_ascii_case(&row.role))
            {
                let Some(pin) = comp.pins.iter().find(|p| p.number == *pad) else {
                    continue;
                };
                let verdict = match judge(board, &classify, (comp, pin), row) {
                    // A function most boards never use: only a bias the wrong way
                    // round is evidence, never a pin left open.
                    Verdict::Missing { .. } if row.opposed_only => Verdict::Fine,
                    v => v,
                };
                if let Some(f) = finding(board, (comp, model), (pad, pin), row, &verdict) {
                    report.findings.push(f);
                }
            }
        }
    }
    report
}

/// The finding for `verdict`, or `None` when the pin is fine.
fn finding(
    board: &ExtractedBoard,
    (comp, model): (&Component, &ModelEntry),
    (pad, pin): (&str, &Pin),
    row: &PinBias,
    verdict: &Verdict,
) -> Option<LintFinding> {
    let net = pin
        .net
        .and_then(|id| board.net(id))
        .map_or("(unconnected)", |n| n.name.as_str());
    let level = if row.level == BiasLevel::High {
        "high"
    } else {
        "low"
    };
    let head = format!(
        "{} ({}) pin {pad} ({}) must idle {level} (card '{}': {}). ",
        comp.reference,
        comp.value.trim(),
        row.role,
        model.id,
        row.basis.trim()
    );
    let note = if row.note.trim().is_empty() {
        String::new()
    } else {
        format!(" {}", row.note.trim())
    };
    let fix = remedy(board, (comp, model), row, net);
    let (severity, message) = match verdict {
        Verdict::Fine => return None,
        Verdict::Missing { severity, why } => {
            (*severity, format!("{head}It has no bias: {why}.{note} {fix}"))
        }
        Verdict::Opposite { why } => (
            // A bias that only matters when the board uses the function (a ROM
            // bootloader's UART) is a Medium warning, not a gating fault.
            if row.opposed_only {
                Severity::Medium
            } else {
                Severity::High
            },
            format!("{head}It is biased the wrong way: {why}.{note} {fix}"),
        ),
        Verdict::OutOfRange { why } => (
            Severity::Medium,
            format!("{head}Its bias is out of range: {why}.{note} {fix}"),
        ),
        Verdict::Abstain { why } => (
            Severity::Low,
            format!(
                "{head}The required-bias check{ABSTENTION_MARKER} on net '{net}': {why}. {REMEDY_MARKER}Give the resistor a numeric value so the datasheet range can be checked."
            ),
        ),
    };
    Some(LintFinding {
        check: LintCheck::RequiredPinBias,
        severity,
        message,
        refs: vec![comp.reference.clone()],
        nets: vec![net.to_string()],
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;

    /// A test device whose open-drain `nfault` pin needs a 1k..100k pull-up and
    /// whose `rx` pin must idle high (a bootloader receive line); pad 1 vcc,
    /// 2 gnd, 3 nfault, 4 rx, 5 out (no bias row).
    const CARD: &str = r#"
[[models]]
id = "bias_test_driver"
kind = "digital"
description = "pin bias fixture"
[models.match]
value_re = "^BIASTEST$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "vcc"
"2" = "gnd"
"3" = "nfault"
"4" = "rx"
"5" = "out"
"6" = "flag"
"7" = "brx"
[[models.pin_bias]]
role = "brx"
level = "high"
needs = "idle"
opposed_only = true
basis = "fixture app note Table 2, p.4: the ROM bootloader RX pin idles high; a low level is a break"
[[models.pin_bias]]
role = "nfault"
level = "high"
needs = "pull"
min_ohm = 1000.0
max_ohm = 100000.0
basis = "fixture datasheet Table 1, p.3: nFAULT open-drain output, requires an external pull-up"
note = "An open-drain output cannot pull itself high."
[[models.pin_bias]]
role = "rx"
level = "high"
needs = "idle"
basis = "fixture datasheet section 2, p.9: RX must idle high"
[[models.pin_bias]]
role = "flag"
level = "high"
needs = "pull"
open_ok = true
basis = "fixture datasheet Table 1, p.3: FLAG open-drain output"
"#;

    fn library() -> ModelLibrary {
        let dir = hauksbee_testkit::fresh_scratch_dir("pin_bias", "card");
        std::fs::write(dir.join("models.toml"), CARD).unwrap();
        let mut lib = ModelLibrary::builtin();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "test card must load: {errors:?}");
        lib
    }

    /// Nets: 1 GND, 2 +3V3, 3 FAULT, 4 RXD, 5 OUT, 6 `FAULT_MCU` (behind a series R).
    fn fixture(extra: Vec<Component>) -> ExtractedBoard {
        let mut parts = vec![part(
            "U1",
            "BIASTEST",
            &[("1", 2), ("2", 1), ("3", 3), ("4", 4), ("5", 5), ("6", 7)],
        )];
        parts.extend(extra);
        board(
            &[
                (1, "GND"),
                (2, "+3V3"),
                (3, "FAULT"),
                (4, "RXD"),
                (5, "OUT"),
                (6, "FAULT_MCU"),
                (7, "FLAG"),
            ],
            parts,
        )
    }

    fn lint(extra: Vec<Component>) -> Vec<LintFinding> {
        pin_bias_lint(&fixture(extra), &library()).findings
    }

    fn r(reference: &str, value: &str, a: i64, b: i64) -> Component {
        part(reference, value, &[("1", a), ("2", b)])
    }

    fn on_net<'a>(f: &'a [LintFinding], net: &str) -> Vec<&'a LintFinding> {
        f.iter().filter(|f| f.nets == [net]).collect()
    }

    #[test]
    fn a_required_pull_up_that_is_missing_is_flagged_with_the_fix() {
        let f = lint(vec![r("R2", "10k", 4, 2)]);
        let nfault = on_net(&f, "FAULT");
        assert_eq!(nfault.len(), 1, "{f:?}");
        assert_eq!(nfault[0].check, LintCheck::RequiredPinBias);
        assert_eq!(nfault[0].severity, Severity::High);
        let m = &nfault[0].message;
        for s in [
            "nfault",
            "no bias",
            "pull-up",
            "'+3V3'",
            "Table 1, p.3",
            "bias_test_driver",
        ] {
            assert!(m.contains(s), "{s:?} in {m}");
        }
        assert!(nfault[0].own_remedy().is_some());
    }

    #[test]
    fn a_pull_down_where_a_pull_up_is_required_is_the_opposite_bias() {
        let f = lint(vec![r("R1", "10k", 3, 1), r("R2", "10k", 4, 2)]);
        let nfault = on_net(&f, "FAULT");
        assert_eq!(nfault.len(), 1, "{f:?}");
        assert_eq!(nfault[0].severity, Severity::High);
        assert!(
            nfault[0].message.contains("wrong way"),
            "{}",
            nfault[0].message
        );
        assert!(
            nfault[0].message.contains("R1 10k"),
            "{}",
            nfault[0].message
        );
    }

    #[test]
    fn a_correct_pull_up_in_range_is_silent() {
        let f = lint(vec![r("R1", "10k", 3, 2), r("R2", "10k", 4, 2)]);
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn a_pull_up_outside_the_datasheet_range_is_flagged_either_way() {
        let weak = lint(vec![r("R1", "1M", 3, 2), r("R2", "10k", 4, 2)]);
        assert_eq!(weak.len(), 1, "{weak:?}");
        assert_eq!(weak[0].severity, Severity::Medium);
        assert!(weak[0].message.contains("too weak"), "{}", weak[0].message);
        let strong = lint(vec![r("R1", "100", 3, 2), r("R2", "10k", 4, 2)]);
        assert_eq!(strong.len(), 1, "{strong:?}");
        assert!(
            strong[0].message.contains("loads the pin's driver"),
            "{}",
            strong[0].message
        );
    }

    #[test]
    fn a_pull_up_whose_tolerance_corner_crosses_the_range_is_flagged() {
        // 100k 5% is up to 105k: its weakest corner is above the 100k maximum.
        let f = lint(vec![r("R1", "100k 5%", 3, 2), r("R2", "10k", 4, 2)]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].message.contains("too weak"), "{}", f[0].message);
    }

    #[test]
    fn a_pull_up_behind_one_series_resistor_counts() {
        // FAULT -- 1k -- FAULT_MCU, with the pull-up on the MCU side.
        let f = lint(vec![
            r("R3", "1k", 3, 6),
            r("R1", "10k", 6, 2),
            r("R2", "10k", 4, 2),
        ]);
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn an_idle_requirement_on_a_floating_net_is_flagged() {
        let f = lint(vec![r("R1", "10k", 3, 2)]);
        let rx = on_net(&f, "RXD");
        assert_eq!(rx.len(), 1, "{f:?}");
        assert!(
            rx[0].message.contains("must idle high"),
            "{}",
            rx[0].message
        );
    }

    #[test]
    fn an_idle_requirement_with_only_a_pull_down_is_flagged() {
        let f = lint(vec![r("R1", "10k", 3, 2), r("R2", "10k", 4, 1)]);
        let rx = on_net(&f, "RXD");
        assert_eq!(rx.len(), 1, "{f:?}");
        assert!(rx[0].message.contains("wrong way"), "{}", rx[0].message);
    }

    #[test]
    fn a_pin_tied_to_ground_where_a_pull_up_is_needed_is_flagged() {
        let b = board(
            &[(1, "GND"), (2, "+3V3"), (4, "RXD")],
            vec![
                part("U1", "BIASTEST", &[("1", 2), ("2", 1), ("3", 1), ("4", 4)]),
                r("R2", "10k", 4, 2),
            ],
        );
        let f = pin_bias_lint(&b, &library()).findings;
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(
            f[0].message.contains("tied directly to 'GND'"),
            "{}",
            f[0].message
        );
    }

    #[test]
    fn a_dominating_opposite_pull_in_a_divider_is_flagged() {
        let f = lint(vec![
            r("R1", "100k", 3, 2),
            r("R3", "1k", 3, 1),
            r("R2", "10k", 4, 2),
        ]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].message.contains("overpowered"), "{}", f[0].message);
    }

    #[test]
    fn an_unreadable_pull_value_abstains_rather_than_passes() {
        let f = lint(vec![r("R1", "TBD", 3, 2), r("R2", "10k", 4, 2)]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].severity, Severity::Low);
        assert!(f[0].is_abstention(), "{}", f[0].message);
    }

    #[test]
    fn an_open_ok_flag_is_silent_unconnected_and_flagged_once_something_reads_it() {
        let both = [r("R1", "10k", 3, 2), r("R2", "10k", 4, 2)];
        assert!(lint(both.to_vec()).is_empty());
        let mut with_reader = both.to_vec();
        with_reader.push(part("J1", "CONN", &[("1", 7)]));
        let f = lint(with_reader);
        let flag = on_net(&f, "FLAG");
        assert_eq!(flag.len(), 1, "{f:?}");
        assert!(flag[0].message.contains("pull-up"), "{}", flag[0].message);
    }

    #[test]
    fn a_dnp_pull_up_is_not_a_pull_up() {
        let mut dnp = r("R1", "10k", 3, 2);
        dnp.dnp = true;
        let f = lint(vec![dnp, r("R2", "10k", 4, 2)]);
        assert_eq!(on_net(&f, "FAULT").len(), 1, "{f:?}");
    }

    /// A bootloader receive line (`opposed_only`): an ordinary port pin when the
    /// bootloader is unused, so a floating or unbiased pin is fine, but a pull
    /// toward the break level is a flaw.
    #[test]
    fn a_bootloader_rx_pin_is_flagged_only_when_pulled_the_wrong_way() {
        let run = |pull: Option<(&str, i64)>| {
            let mut parts = vec![part("U1", "BIASTEST", &[("1", 2), ("2", 1), ("7", 8)])];
            if let Some((value, rail)) = pull {
                parts.push(r("R1", value, 8, rail));
            }
            let b = board(&[(1, "GND"), (2, "+3V3"), (8, "BOOT_RX")], parts);
            pin_bias_lint(&b, &library()).findings
        };
        // Floating: the port pin is simply unused. Silent.
        assert!(run(None).is_empty());
        // The correct idle level, a pull-up: silent.
        assert!(run(Some(("10k", 2))).is_empty());
        // A pull-down holds the line in break so the bootloader never starts.
        let down = run(Some(("10k", 1)));
        assert_eq!(down.len(), 1, "{down:?}");
        assert_eq!(down[0].check, LintCheck::RequiredPinBias);
        assert_eq!(down[0].severity, Severity::Medium);
        assert!(down[0].message.contains("wrong way"), "{}", down[0].message);
        assert!(down[0].message.contains("p.4"), "{}", down[0].message);
    }

    /// The shipped STM32F103C8 card: PA10 (pad 31) is the system-memory
    /// bootloader's USART1 RX, so a pull-down there is flagged and a pull-up
    /// or nothing is not.
    #[test]
    fn the_shipped_stm32_bootloader_rx_rejects_a_pull_down() {
        let lib = ModelLibrary::builtin();
        let run = |pull_to: Option<i64>| {
            let mut parts = vec![part("U1", "STM32F103C8", &[("31", 3)])];
            if let Some(rail) = pull_to {
                parts.push(part("R1", "10k", &[("1", 3), ("2", rail)]));
            }
            let b = board(&[(1, "GND"), (2, "+3V3"), (3, "USART_RX")], parts);
            pin_bias_lint(&b, &lib).findings
        };
        let down = run(Some(1));
        assert_eq!(down.len(), 1, "{down:?}");
        assert!(down[0].message.contains("wrong way"), "{}", down[0].message);
        assert!(run(Some(2)).is_empty());
        assert!(run(None).is_empty());
    }

    /// The shipped AP22815 card (pad 3 is the open-drain FLG output) needs a
    /// pull-up once anything reads the flag: a pull-down is the wrong way
    /// round, a pull-up to the rail is silent, and an unused flag is silent.
    #[test]
    fn the_shipped_load_switch_flag_needs_its_pull_up() {
        let lib = ModelLibrary::builtin();
        let run = |pull_to: Option<i64>| {
            let mut parts = vec![part(
                "U1",
                "AP22815",
                &[("1", 3), ("2", 1), ("3", 4), ("4", 5), ("5", 2)],
            )];
            if let Some(rail) = pull_to {
                parts.push(part("R1", "10k", &[("1", 4), ("2", rail)]));
            }
            let b = board(
                &[(1, "GND"), (2, "+5V"), (3, "+3V3"), (4, "FLG_N"), (5, "EN")],
                parts,
            );
            pin_bias_lint(&b, &lib).findings
        };
        let wrong = run(Some(1));
        assert_eq!(wrong.len(), 1, "{wrong:?}");
        assert!(
            wrong[0].message.contains("pull-down"),
            "{}",
            wrong[0].message
        );
        assert!(run(Some(3)).is_empty());
        // Nothing else on the flag net: unused, so not required.
        assert!(run(None).is_empty());
    }
}
