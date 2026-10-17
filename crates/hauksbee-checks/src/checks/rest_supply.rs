//! Whether the resting state the stress pass judges is one the board's
//! supplies can give.
//!
//! Every rail the binder sources is an ideal voltage source unless a spec
//! declares the real supply, so the DC solve holds it at its level whatever
//! the board pulls. A board that rests with many copies of one cell each
//! driving a transistor base or a switch channel from that rail then computes
//! amperes per cell and tens of amperes in total. No bench supply, USB port or
//! dev-board pin gives that: the rail sags long before, and the current each
//! part really carries is its share of what the supply can give. Reporting
//! the ideal figure as each part's overstress is a solve artefact presented as
//! a board fault.
//!
//! So a settled state whose draw on a rail exceeds that rail's capability is
//! judged again with the rail held to it (a bench supply in constant-current
//! mode, [`PowerSupply::Bench`]). A part still over a rating there is a fault
//! at any supply and stays one: one LED or diode with no series resistor is
//! the usual case. A part over a rating only at the ideal rail's draw is not
//! reported as overstressed; all such parts become one finding that names the
//! rail, the draw and the capability, and says what to check.
//!
//! The capability is the supply's declared current limit (a bench supply's
//! setting, a USB port's spec, a battery protector's trip) or, for a rail no
//! spec describes, [`UNDECLARED_SUPPLY_LIMIT_A`].

use std::collections::BTreeMap;

use hauksbee_bind::binder::BoundBoard;
use hauksbee_bind::{FaultKind, PowerSupply};
use hauksbee_extract::{LintCheck, LintFinding, Severity, REMEDY_MARKER};
use hauksbee_ir::units::format_amps;
use hauksbee_solve::Workspace;

/// The current assumed available from a rail no spec gives a supply for, A.
///
/// The highest "Type-C Current" a USB-C source advertises at 5 V without
/// Power Delivery (3.0 A, the binder's [`hauksbee_bind::UsbSpec::V5_3A`]), and
/// a common bench-supply setting. A board resting above it
/// on an undeclared supply is far more likely mis-modelled than real; a
/// `[[supply]]` with its real `current_limit_a` replaces it.
pub const UNDECLARED_SUPPLY_LIMIT_A: f64 = 3.0;

/// How every finding of [`implausible_rest_finding`] opens, so a surface can
/// tell it from a part overstress.
pub const OPENING: &str = "At rest the solve draws ";

/// One rail whose resting draw exceeds what its supply can give.
#[derive(Debug, Clone, PartialEq)]
pub struct Overload {
    /// The rail's net name.
    pub net: String,
    /// The current the solve sources into it, A.
    pub draw_a: f64,
    /// What its supply can give, A.
    pub limit_a: f64,
    /// Whether `limit_a` is the supply's declared limit, not the default.
    pub declared: bool,
}

/// What `supply` can source, A, and whether that is its declared limit.
#[must_use]
pub const fn capability(supply: &PowerSupply) -> (f64, bool) {
    match supply {
        PowerSupply::Bench {
            current_limit_a, ..
        } => (*current_limit_a, true),
        PowerSupply::Usb { spec } => (spec.current_limit_a(), true),
        PowerSupply::Battery {
            protection: Some(p),
            ..
        } => (p.trip_a, true),
        _ => (UNDECLARED_SUPPLY_LIMIT_A, false),
    }
}

/// Every supply leg of `bound` sourcing more than its capability in the
/// settled state `(ws, x)`.
#[must_use]
pub fn overloads(bound: &BoundBoard, ws: &Workspace, x: &[f64]) -> Vec<Overload> {
    (bound.supplies.iter())
        .filter_map(|leg| {
            let branch = ws
                .layout
                .branch(leg.vsource)
                .and_then(|i| x.get(i).copied())?;
            // A Vsource's branch current flows p to n inside it, so the
            // current delivered into a positive rail is its negative; a
            // negative rail delivers the other way.
            let draw = -branch * leg.supply.nominal_volts().signum();
            let (limit, declared) = capability(&leg.supply);
            (draw.is_finite() && limit > 0.0 && draw > limit).then(|| Overload {
                net: leg.net_name.clone(),
                draw_a: draw,
                limit_a: limit,
                declared,
            })
        })
        .collect()
}

/// Hold each overloaded rail of `bound` to its capability. Returns the
/// supplies to put back with [`release`].
pub fn hold_to_capability(bound: &mut BoundBoard, over: &[Overload]) -> Vec<(usize, PowerSupply)> {
    let BoundBoard {
        circuit, supplies, ..
    } = bound;
    let mut saved = Vec::new();
    for (k, leg) in supplies.iter_mut().enumerate() {
        let Some(o) = over.iter().find(|o| o.net == leg.net_name) else {
            continue;
        };
        saved.push((k, leg.supply.clone()));
        let volts = leg.supply.nominal_volts();
        leg.reconfigure(
            circuit,
            PowerSupply::Bench {
                volts,
                current_limit_a: o.limit_a,
            },
        );
    }
    saved
}

/// Put back the supplies [`hold_to_capability`] replaced.
pub fn release(bound: &mut BoundBoard, saved: Vec<(usize, PowerSupply)>) {
    let BoundBoard {
        circuit, supplies, ..
    } = bound;
    for (k, supply) in saved {
        if let Some(leg) = supplies.get_mut(k) {
            leg.reconfigure(circuit, supply);
        }
    }
}

/// Keep the worst draw seen on each rail across the judged states.
pub fn note_overloads(into: &mut Vec<Overload>, seen: Vec<Overload>) {
    for o in seen {
        if let Some(k) = into.iter().position(|k| k.net == o.net) {
            if o.draw_a > into[k].draw_a {
                into[k] = o;
            }
        } else {
            into.push(o);
        }
    }
}

/// How many parts the finding quotes before "and N more".
const SHOWN: usize = 3;

impl Overload {
    /// Whether the draw is a near-short: so far past what the supply gives
    /// ([`hauksbee_bind::is_far_past_rating`], the rule every stress figure
    /// follows) that the ideal rail and the solver's floors set the figure,
    /// not the board.
    #[must_use]
    pub fn is_near_short(&self) -> bool {
        hauksbee_bind::is_far_past_rating(FaultKind::Overcurrent, self.draw_a, self.limit_a)
    }
}

/// The one finding for the parts over a rating only at a draw their supply
/// cannot give.
///
/// `parts` maps each to its first clause at the ideal rail. `unsolved` says the
/// re-judge with the rails held did not solve, so no part could be cleared or
/// kept there.
#[must_use]
pub fn implausible_rest_finding(
    over: &[Overload],
    parts: &BTreeMap<String, String>,
    unsolved: bool,
) -> Option<LintFinding> {
    if over.is_empty() || parts.is_empty() {
        return None;
    }
    let rails: Vec<String> = (over.iter())
        .map(|o| {
            let limit = if o.declared {
                format!("its supply's declared {} limit", format_amps(o.limit_a))
            } else {
                format!(
                    "the {} assumed for a supply no spec declares",
                    format_amps(o.limit_a)
                )
            };
            if o.is_near_short() {
                format!(
                    "far more than {limit} from {} (a near-short: {})",
                    o.net,
                    hauksbee_bind::far_past_reason(FaultKind::Overcurrent, o.draw_a)
                )
            } else {
                format!(
                    "{} from {}, more than {limit}",
                    format_amps(o.draw_a),
                    o.net
                )
            }
        })
        .collect();
    // On a near-short the draw is the solver's figure, and so are the
    // per-part figures it implies.
    let near_short = over.iter().any(Overload::is_near_short);
    let shown: Vec<String> = (parts.iter().take(SHOWN))
        .map(|(part, clause)| {
            if near_short {
                part.clone()
            } else {
                format!("{part}: {clause}")
            }
        })
        .collect();
    let more = parts.len().saturating_sub(SHOWN);
    let more = if more > 0 {
        format!("; and {more} more")
    } else {
        String::new()
    };
    let verdict = if unsolved {
        "The board did not solve with the rail held there, so these figures are unconfirmed either way"
    } else {
        "Held to what the supply can give, each of them is within its ratings"
    };
    let nets: Vec<String> = over.iter().map(|o| o.net.clone()).collect();
    Some(LintFinding {
        check: LintCheck::DcOverstress,
        severity: Severity::Medium,
        message: format!(
            "{OPENING}{}, so the rail would sag long before that, and the overstress the ideal \
             rail computes on {} part(s) is not what the board sees ({}{more}). {verdict}. \
             Either a model or binding is wrong (a switch's resting state, a pin map, a part \
             that is not fitted), or the board crowbars the rail through these parts. \
             {REMEDY_MARKER}Declare the real supply with its current limit ([[supply]] with \
             current_limit_a, or the app's supply setting), then trace the path from {} into \
             the first part named and check it against the datasheet",
            rails.join("; "),
            parts.len(),
            shown.join("; "),
            nets.join(" and "),
        ),
        refs: parts.keys().cloned().collect(),
        nets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_bind::UsbSpec;

    #[test]
    fn a_declared_limit_is_the_capability_and_an_ideal_rail_gets_the_default() {
        let bench = PowerSupply::Bench {
            volts: 12.0,
            current_limit_a: 0.5,
        };
        let near = |(a, declared): (f64, bool), want: f64| (a - want).abs() < 1e-12 && declared;
        assert!(near(capability(&bench), 0.5));
        let usb = PowerSupply::Usb {
            spec: UsbSpec::V5_0_5A,
        };
        assert!(near(capability(&usb), 0.5));
        let (ideal, declared) = capability(&PowerSupply::Ideal { volts: 5.0 });
        assert!((ideal - UNDECLARED_SUPPLY_LIMIT_A).abs() < 1e-12 && !declared);
    }

    /// Faulty: a draw past the bound no supply reaches is a near-short, and
    /// its exact figure (the solver's floor, not the board's) is not printed;
    /// so is a 350 A draw on a 3 A supply, under that bound but over 100x
    /// what the supply gives, with the true reason for each. Legal control: a
    /// plausible 21.2 A is printed as before.
    #[test]
    fn a_near_short_draw_is_not_printed_as_a_figure() {
        let parts: BTreeMap<String, String> =
            [("R1".to_string(), "R1 dissipates 24950 mW".to_string())].into();
        let short = [Overload {
            net: "+5V".into(),
            draw_a: 4995.0,
            limit_a: 3.0,
            declared: false,
        }];
        let f = implausible_rest_finding(&short, &parts, false).expect("a finding");
        assert!(f.message.starts_with(OPENING), "{}", f.message);
        assert!(
            f.message.contains("far more than the 3 A assumed")
                && f.message.contains("past 500 A")
                && f.message.contains("not physical"),
            "{}",
            f.message
        );
        assert!(
            !f.message.contains("4995") && !f.message.contains("24950"),
            "{}",
            f.message
        );
        let ideal = [Overload {
            draw_a: 350.0,
            ..short[0].clone()
        }];
        let f = implausible_rest_finding(&ideal, &parts, false).expect("a finding");
        assert!(
            f.message.contains("a near-short: over 100x") && !f.message.contains("350"),
            "{}",
            f.message
        );
        let real = [Overload {
            draw_a: 21.2,
            ..short[0].clone()
        }];
        let f = implausible_rest_finding(&real, &parts, false).expect("a finding");
        assert!(f.message.contains("21.2 A") && f.message.contains("24950 mW"));
    }

    #[test]
    fn the_finding_names_the_rail_the_draw_the_limit_and_its_own_remedy() {
        let over = [Overload {
            net: "+5P".into(),
            draw_a: 21.2,
            limit_a: 3.0,
            declared: false,
        }];
        let parts: BTreeMap<String, String> = (0..5)
            .map(|k| {
                (
                    format!("Q{k}"),
                    format!("Q{k} carries 235.2 mA, over its 100.0 mA rating"),
                )
            })
            .collect();
        let f = implausible_rest_finding(&over, &parts, false).expect("a finding");
        assert_eq!(f.severity, Severity::Medium);
        assert!(f.message.starts_with(OPENING), "{}", f.message);
        for want in [
            "21.2 A from +5P",
            "3 A assumed",
            "5 part(s)",
            "and 2 more",
            "within its ratings",
        ] {
            assert!(f.message.contains(want), "{want}: {}", f.message);
        }
        assert_eq!(f.refs.len(), 5);
        assert!(f
            .own_remedy()
            .is_some_and(|r| r.contains("current_limit_a")));
        assert!(implausible_rest_finding(&over, &BTreeMap::new(), false).is_none());
        assert!(implausible_rest_finding(&[], &parts, false).is_none());
    }
}
