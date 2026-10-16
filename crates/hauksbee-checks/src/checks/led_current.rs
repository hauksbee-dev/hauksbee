//! An LED string's current, judged from the levels its two ends are held at.
//!
//! An LED in series with a resistor between two held nets carries a current
//! the string alone decides: `I·(R + Rs + Ro) + n·Vt·ln(1 + I/Is) = Va − Vk`,
//! with the forward drop from the LED's own card (its `is`, `n` and `rs`) and
//! the ends' levels from the board's resolved rails (a regulator's output, a
//! battery's chemistry, a named rail's ±5 % band, ground) or a logic or MCU
//! output driving one end (lit at its supply level through its strong-corner
//! output resistance, or sunk at 0 V). The resistor may sit on either side of
//! the LED.
//!
//! The DC ratings pass ([`super::dc_stress`]) judges the same ratings at the
//! solved operating point, but a board whose operating point does not solve,
//! or whose rail solves dead, or whose GPIO sits low at rest, never lights the
//! LED there. This check needs no solve: it judges the current against the
//! LED's current rating, the series resistor's wattage (its card's, else its
//! footprint's) and the driving pin's current rating.
//!
//! A limit from a stand-in card (an LED guessed from its colour, the generic
//! LED's assumed corners) is a stated condition, never a failing finding
//! ([`rating_basis`]). An LED whose card names forward-voltage corners is judged
//! at every corner and the resistor at its stated tolerance: a judged limit
//! passed at every corner of a rail-held string is high severity, at only some
//! corners a conditional medium one, and on a firmware- or logic-driven string
//! medium, since the driver decides when it is lit. A string whose ends are
//! not both held (an open-drain status pin, a transistor switch) is not judged.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.

use std::collections::BTreeMap;

use hauksbee_bind::binder::{resolve, role_nets};
use hauksbee_bind::digital::{output_roles, LogicLevels};
use hauksbee_bind::stress::resistor_power_from_footprint;
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_led, is_plain_resistor, Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport,
    Severity,
};
use hauksbee_models::rating_basis::{self, Limit, StandIn};
use hauksbee_models::{ComponentKind, ModelEntry, ModelLibrary, Resolution};

use super::envelope::{role_net_id, RailResolver};
use super::BoardStatics;

/// Every LED string on `board` whose current passes a rating.
#[must_use]
pub fn led_current_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let strings = led_strings(board, lib).into_iter();
    NetLintReport {
        findings: strings.map(|judged| judged.finding).collect(),
    }
}

/// One judged string: its finding and the parts the string itself is made of
/// (the LED and its resistors, not the part driving it).
pub(crate) struct JudgedString {
    pub(crate) finding: LintFinding,
    pub(crate) string: Vec<String>,
}

/// [`led_current_lint`] with each finding's string parts.
pub(crate) fn led_strings(board: &ExtractedBoard, lib: &ModelLibrary) -> Vec<JudgedString> {
    let statics = BoardStatics::of(board, lib);
    let solved = BTreeMap::new();
    let ctx = Ctx {
        board,
        lib,
        statics: &statics,
        rails: RailResolver::new(board, &statics, &solved),
        members: members(board),
    };
    let leds = (board.components.iter()).filter(|c| is_led(c) && c.pins.len() == 2);
    leds.filter_map(|led| ctx.judge(led)).collect()
}

/// The fitted parts' pads on each net: `(component index, pad)`.
fn members(board: &ExtractedBoard) -> BTreeMap<i64, Vec<(usize, String)>> {
    let mut by_net: BTreeMap<i64, Vec<(usize, String)>> = BTreeMap::new();
    for (index, comp) in board.components.iter().enumerate() {
        if !AssemblyState::of(comp).is_present() {
            continue;
        }
        for pin in &comp.pins {
            if let Some(net) = pin.net.filter(|&id| id > 0) {
                let pad = (index, pin.number.clone());
                by_net.entry(net).or_default().push(pad);
            }
        }
    }
    by_net
}

struct Ctx<'a> {
    board: &'a ExtractedBoard,
    lib: &'a ModelLibrary,
    statics: &'a BoardStatics<'a>,
    rails: RailResolver<'a>,
    members: BTreeMap<i64, Vec<(usize, String)>>,
}

/// A rating, and the stand-in card it came from when it is not the part's own.
type Rated = (f64, Option<StandIn>);

/// A series resistor of the string.
struct Series {
    reference: String,
    ohms: f64,
    tolerance: Option<f64>,
    /// Its wattage, and where the rating came from.
    power: Option<(Rated, String)>,
}

/// A part driving one end of the string.
struct Driver {
    reference: String,
    ro: f64,
    ro_stated: bool,
    pin_limit: Option<Rated>,
}

/// One end of the string: the net and the level span it is held at when lit.
struct End {
    net: String,
    min_v: f64,
    max_v: f64,
    driver: Option<Driver>,
}

/// The string: its LED, resistors and two ends.
struct LedString<'a> {
    led: &'a Component,
    series: Vec<Series>,
    anode: End,
    cathode: End,
}

/// The LED's forward model at one card.
struct Led {
    card: String,
    is: f64,
    n: f64,
    rs: f64,
    max_a: Option<Rated>,
}

/// One rating passed in one scenario.
struct Exceeded {
    /// The part whose rating it is.
    part: String,
    /// What was seen, against what.
    clause: String,
    /// The rating, in words ("25.0 mA current").
    rating: String,
    stand_in: Option<StandIn>,
}

/// The string judged at one LED card and one end of the tolerance span.
struct Scenario {
    amps: f64,
    corner: String,
    exceeded: Vec<Exceeded>,
}

impl Scenario {
    fn judged(&self) -> bool {
        self.exceeded.iter().any(|e| e.stand_in.is_none())
    }
}

impl Ctx<'_> {
    fn judge(&self, led: &Component) -> Option<JudgedString> {
        let part = AssemblyState::of(led).fitted()?;
        let res = resolve(self.lib, part);
        let model = res.model.as_ref()?;
        if model.kind != ComponentKind::Diode {
            return None;
        }
        let roles = role_nets(led, model, self.lib.pin_rules());
        let (anode, cathode) = (*roles.get("anode")?, *roles.get("cathode")?);
        if anode == cathode {
            return None;
        }
        let cards = self.cards(led, &res)?;
        let (anode_series, anode_end) = self.side(led, anode);
        let (cathode_series, cathode_end) = self.side(led, cathode);
        let series: Vec<Series> = anode_series.into_iter().chain(cathode_series).collect();
        if series.is_empty() || anode_end == cathode_end {
            return None;
        }
        let string = LedString {
            led,
            series,
            anode: self.end(anode_end, true)?,
            cathode: self.end(cathode_end, false)?,
        };
        if string.anode.driver.is_some() && string.cathode.driver.is_some() {
            return None;
        }
        let finding = string.finding(&cards)?;
        let mut parts = vec![led.reference.clone()];
        parts.extend(string.series.iter().map(|s| s.reference.clone()));
        Some(JudgedString {
            finding,
            string: parts,
        })
    }

    /// The LED's card, or each forward-voltage corner when its card is one of
    /// a corner set the board does not decide.
    fn cards(&self, led: &Component, res: &Resolution) -> Option<Vec<Led>> {
        use hauksbee_models::variants::SWEEP_PROPERTY;
        let open = (res.selection.as_ref()).filter(|s| s.corners && s.determined.is_none());
        let models: Vec<ModelEntry> = match open {
            Some(selection) => (selection.survivors())
                .filter_map(|member| {
                    let mut pinned = led.clone();
                    let pin = (SWEEP_PROPERTY.to_string(), member.id.clone());
                    pinned.properties.push(pin);
                    let part = AssemblyState::of(&pinned).fitted()?;
                    resolve(self.lib, part).model
                })
                .collect(),
            None => vec![res.model.clone()?],
        };
        let cards: Vec<Led> = models.iter().filter_map(Led::of).collect();
        (!cards.is_empty() && cards.len() == models.len()).then_some(cards)
    }

    /// The series resistor on `net` (when the LED's only neighbour there is a
    /// plain resistor of a readable value) and the net the string goes on to.
    fn side(&self, led: &Component, net: i64) -> (Option<Series>, i64) {
        let Some(on) = self.members.get(&net) else {
            return (None, net);
        };
        let others: Vec<&(usize, String)> = (on.iter())
            .filter(|(index, _)| self.board.components[*index].reference != led.reference)
            .collect();
        let [(index, pad)] = others.as_slice() else {
            return (None, net);
        };
        let resistor = &self.board.components[*index];
        let far = (resistor.pins.iter())
            .filter(|pin| pin.number != *pad)
            .find_map(|pin| pin.net.filter(|&id| id > 0 && id != net));
        let plain = is_plain_resistor(resistor) && resistor.pins.len() == 2;
        let (Some(far), true, Some(ohms)) = (far, plain, ohms(&resistor.value)) else {
            return (None, net);
        };
        let series = Series {
            reference: resistor.reference.clone(),
            ohms,
            tolerance: super::stated_tolerance(resistor),
            power: self.resistor_power(resistor),
        };
        (Some(series), far)
    }

    /// A resistor's wattage: its card's, else its footprint package's.
    fn resistor_power(&self, resistor: &Component) -> Option<(Rated, String)> {
        let model = AssemblyState::of(resistor)
            .fitted()
            .and_then(|part| resolve(self.lib, part).model);
        if let Some(model) = model {
            if let Some(watts) = model.ratings.max_power_w {
                let stand_in = model.ratings.basis.stand_in_for(Limit::Power).cloned();
                return Some(((watts, stand_in), format!("card '{}'", model.id)));
            }
        }
        let watts = resistor_power_from_footprint(&resistor.footprint).watts?;
        let footprint = &resistor.footprint;
        let package = footprint.rsplit(':').next().unwrap_or(footprint);
        Some(((watts, None), format!("its {package} footprint")))
    }

    /// The level `net` is held at while the LED is lit: a rail's span, or a
    /// driver's supply (`sources`: it drives the anode) or 0 V (it sinks).
    fn end(&self, net: i64, sources: bool) -> Option<End> {
        let name = self.board.net(net)?.name.clone();
        let corners = self.rails.corners(net);
        if let Some((min_v, max_v)) = span(&corners) {
            return Some(End {
                net: name,
                min_v,
                max_v,
                driver: None,
            });
        }
        let (comp, model) = self.driver_on(net)?;
        let (min_v, max_v) = if sources {
            self.supply(comp, model)?
        } else {
            (0.0, 0.0)
        };
        let ratings = &model.ratings;
        let pin_limit = (ratings.max_pin_current_a).map(|amps| {
            let stand_in = ratings.basis.stand_in_for(Limit::PinCurrent).cloned();
            (amps, stand_in)
        });
        let driver = Driver {
            reference: comp.reference.clone(),
            ro: LogicLevels::from_params(model).ro_strong,
            ro_stated: ["ro", "ro_strong"]
                .iter()
                .any(|key| model.params.get_f64(key).is_some()),
            pin_limit,
        };
        Some(End {
            net: name,
            min_v,
            max_v,
            driver: Some(driver),
        })
    }

    /// The one logic or MCU part driving `net`: an MCU's pad there (a GPIO's
    /// direction is firmware state) or a logic part's output pad.
    fn driver_on(&self, net: i64) -> Option<(&Component, &ModelEntry)> {
        let drives = |comp: &Component, model: &ModelEntry| {
            let on = |pad: &str| (comp.pins.iter()).any(|p| p.number == pad && p.net == Some(net));
            match model.kind {
                ComponentKind::Mcu => comp.pins.iter().any(|p| p.net == Some(net)),
                ComponentKind::Digital | ComponentKind::ShiftRegister => {
                    let outputs = output_roles(model);
                    (model.pins.iter()).any(|(pad, role)| outputs.contains(role) && on(pad))
                }
                _ => false,
            }
        };
        let mut found = (self.statics.parts(self.board)).filter(|(comp, model)| drives(comp, model));
        let first = found.next()?;
        found.next().is_none().then_some(first)
    }

    /// The span of a driver's I/O supply rail: its card's `vdd`/`vcc`/`vddio`
    /// pads' nets, as resolved.
    fn supply(&self, comp: &Component, model: &ModelEntry) -> Option<(f64, f64)> {
        let levels: Vec<f64> = (["vdd", "vcc", "vddio", "iovdd"].iter())
            .filter_map(|role| role_net_id(comp, model, role))
            .flat_map(|net| self.rails.corners(net))
            .filter(|volts| *volts > 0.0)
            .collect();
        span(&levels)
    }
}

/// The lowest and highest of `levels`, when there are any.
fn span(levels: &[f64]) -> Option<(f64, f64)> {
    let min_v = levels.iter().copied().fold(f64::INFINITY, f64::min);
    let max_v = levels.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (!levels.is_empty()).then_some((min_v, max_v))
}

impl Led {
    fn of(model: &ModelEntry) -> Option<Self> {
        let params = &model.params;
        let (is, n) = (params.get_f64("is")?, params.get_f64("n")?);
        let max_a = (model.ratings.max_current_a).map(|amps| {
            let stand_in = model.ratings.basis.stand_in_for(Limit::Current).cloned();
            (amps, stand_in)
        });
        (is > 0.0 && n > 0.0).then(|| Self {
            card: model.id.clone(),
            is,
            n,
            rs: params.get_f64("rs").unwrap_or(0.0).max(0.0),
            max_a,
        })
    }

    /// The current through the LED and `ohms` in series across `volts`.
    fn current(&self, volts: f64, ohms: f64) -> f64 {
        let nvt = self.n * hauksbee_ir::thermal_voltage_c(hauksbee_ir::Circuit::new().temp_c);
        let total = ohms + self.rs;
        let excess = |amps: f64| amps.mul_add(total, nvt * (amps / self.is).ln_1p()) - volts;
        if volts <= 0.0 {
            return 0.0;
        }
        // The drop rises with the current, so bisect between none and the
        // current the resistance alone would pass.
        let (mut lo, mut hi) = (0.0, volts / total.max(1e-3));
        for _ in 0..200 {
            let mid = 0.5 * (lo + hi);
            if excess(mid) > 0.0 {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        0.5 * (lo + hi)
    }
}

/// A resistor value in ohms (no unit, or Ω).
fn ohms(value: &str) -> Option<f64> {
    hauksbee_models::value::parse_value(value)
        .filter(|p| matches!(p.unit.as_deref(), None | Some("Ω")))
        .map(|p| p.si)
        .filter(|r| r.is_finite() && *r >= 0.0)
}

fn amps(amps: f64) -> String {
    if amps >= 1.0 {
        format!("{amps:.2} A")
    } else {
        format!("{:.1} mA", amps * 1e3)
    }
}

fn watts(watts: f64) -> String {
    if watts >= 1.0 {
        format!("{watts:.2} W")
    } else {
        format!("{:.0} mW", watts * 1e3)
    }
}

/// `v` with no trailing zeros.
fn trim(v: f64) -> String {
    let s = if v >= 100.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    };
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

impl LedString<'_> {
    fn ends(&self) -> [&End; 2] {
        [&self.anode, &self.cathode]
    }

    fn drivers(&self) -> impl Iterator<Item = &Driver> {
        self.ends().into_iter().filter_map(|e| e.driver.as_ref())
    }

    /// The drivers' output resistance in the loop.
    fn ro(&self) -> f64 {
        self.drivers().map(|d| d.ro).sum()
    }

    /// Every rating the string passes carrying `current` through `led` with
    /// each resistor at `ohms`.
    fn exceeded(&self, led: &Led, current: f64, ohms: &[f64]) -> Vec<Exceeded> {
        let mut out = Vec::new();
        for (s, r) in self.series.iter().zip(ohms) {
            let Some(((rating, stand_in), from)) = &s.power else {
                continue;
            };
            let dissipated = current * current * r;
            if dissipated > *rating {
                out.push(Exceeded {
                    part: s.reference.clone(),
                    clause: format!(
                        "{} dissipates {}, over its {} rating (from {from})",
                        s.reference,
                        watts(dissipated),
                        watts(*rating)
                    ),
                    rating: format!("{} power", watts(*rating)),
                    stand_in: stand_in.clone(),
                });
            }
        }
        let reference = &self.led.reference;
        if let Some((rating, stand_in)) = (led.max_a.as_ref()).filter(|(a, _)| current > *a) {
            out.push(Exceeded {
                part: reference.clone(),
                clause: format!(
                    "{reference} carries {}, over its {} current rating",
                    amps(current),
                    amps(*rating)
                ),
                rating: format!("{} current", amps(*rating)),
                stand_in: stand_in.clone(),
            });
        }
        for d in self.drivers() {
            if let Some((rating, stand_in)) = (d.pin_limit.as_ref()).filter(|(a, _)| current > *a) {
                out.push(Exceeded {
                    part: d.reference.clone(),
                    clause: format!(
                        "{}'s pin passes {}, over its {} pin rating",
                        d.reference,
                        amps(current),
                        amps(*rating)
                    ),
                    rating: format!("{} pin current", amps(*rating)),
                    stand_in: stand_in.clone(),
                });
            }
        }
        out
    }

    /// The string judged at each LED card, at the high end of the drive
    /// (highest level across it, lowest resistance) and the low end.
    fn scenarios(&self, cards: &[Led]) -> Vec<Scenario> {
        let mut out = Vec::new();
        for card in cards {
            for high in [true, false] {
                let ohms: Vec<f64> = (self.series.iter())
                    .map(|s| {
                        let t = s.tolerance.unwrap_or(0.0);
                        s.ohms * if high { 1.0 - t } else { 1.0 + t }
                    })
                    .collect();
                let volts = if high {
                    self.anode.max_v - self.cathode.min_v
                } else {
                    self.anode.min_v - self.cathode.max_v
                };
                let current = card.current(volts, ohms.iter().sum::<f64>() + self.ro());
                let corner = if cards.len() > 1 {
                    format!("{} as {}", self.led.reference, card.card)
                } else {
                    String::new()
                };
                out.push(Scenario {
                    amps: current,
                    corner,
                    exceeded: self.exceeded(card, current, &ohms),
                });
            }
        }
        out
    }

    /// The smallest total series resistance (in 2 % steps up from the fitted
    /// one) at which every card keeps every rating at the high end of the drive.
    fn safe_ohms(&self, cards: &[Led]) -> Option<f64> {
        let fitted: f64 = self.series.iter().map(|s| s.ohms).sum();
        let tolerance = (self.series.iter())
            .filter_map(|s| s.tolerance)
            .fold(0.0, f64::max);
        let volts = self.anode.max_v - self.cathode.min_v;
        // Each resistor keeps its share of the total (the first takes it all
        // when the fitted ones sum to nothing).
        let share = |s: &Series, first: bool| {
            if fitted > 0.0 {
                s.ohms / fitted
            } else if first {
                1.0
            } else {
                0.0
            }
        };
        let mut total = fitted.max(1.0);
        while total < 1.0e7 {
            let low = total * (1.0 - tolerance);
            let ohms: Vec<f64> = (self.series.iter().enumerate())
                .map(|(i, s)| low * share(s, i == 0))
                .collect();
            let ok = cards.iter().all(|card| {
                let current = card.current(volts, low + self.ro());
                self.exceeded(card, current, &ohms).is_empty()
            });
            if ok {
                return Some(total);
            }
            total *= 1.02;
        }
        None
    }

    /// How one end is held, in words.
    fn held(end: &End, sources: bool) -> String {
        let Some(d) = &end.driver else {
            return if (end.max_v - end.min_v).abs() < 1e-9 {
                format!("'{}' at {:.2} V", end.net, end.max_v)
            } else {
                format!("'{}' at {:.2} V to {:.2} V", end.net, end.min_v, end.max_v)
            };
        };
        let ro = if d.ro_stated {
            format!("its card's {} Ω output resistance", trim(d.ro))
        } else {
            format!(
                "an assumed {} Ω output resistance (its card states none)",
                trim(d.ro)
            )
        };
        let level = if sources {
            format!("high ({:.2} V to {:.2} V)", end.min_v, end.max_v)
        } else {
            "low".to_string()
        };
        format!("'{}' when {} drives it {level} through {ro}", end.net, d.reference)
    }

    /// What the string carries at `worst`, in words.
    fn subject(&self, worst: &Scenario) -> String {
        let resistors: Vec<String> = (self.series.iter())
            .map(|s| {
                let tolerance = s.tolerance.map_or_else(
                    || "no tolerance stated".to_string(),
                    |t| format!("±{:.0} %", t * 100.0),
                );
                format!("{} ({} Ω, {tolerance})", s.reference, trim(s.ohms))
            })
            .collect();
        let corner = if worst.corner.is_empty() {
            String::new()
        } else {
            format!(" (at {})", worst.corner)
        };
        format!(
            "{} lit from {} through {} to {} carries up to {}{corner}",
            self.led.reference,
            Self::held(&self.anode, true),
            resistors.join(" and "),
            Self::held(&self.cathode, false),
            amps(worst.amps),
        )
    }

    /// The severity of a judged exceedance, and the words that qualify it.
    fn severity(&self, held: &[&Scenario], all: usize) -> (Severity, String) {
        if self.drivers().next().is_some() {
            let why = " (whenever its driver lights it: the firmware or logic decides when)";
            return (Severity::Medium, why.to_string());
        }
        if held.len() == all {
            return (Severity::High, String::new());
        }
        let mut at: Vec<&str> = (held.iter())
            .map(|s| s.corner.as_str())
            .filter(|c| !c.is_empty())
            .collect();
        at.dedup();
        let at = if at.is_empty() {
            "the high end of its rail and resistor tolerance".to_string()
        } else {
            at.join(" / ")
        };
        let why = format!(
            " (CONDITIONAL: only at {at}; the board does not say which part is fitted, so the \
             fitted part decides whether it holds)"
        );
        (Severity::Medium, why)
    }

    /// The finding for a string that passes a rating in any scenario.
    fn finding(&self, cards: &[Led]) -> Option<LintFinding> {
        let scenarios = self.scenarios(cards);
        let by_amps = |x: &&Scenario, y: &&Scenario| x.amps.total_cmp(&y.amps);
        let held: Vec<&Scenario> = scenarios.iter().filter(|s| s.judged()).collect();
        let remedy = self.safe_ohms(cards).map_or_else(String::new, |r| {
            format!("; a series resistance of at least {} Ω keeps every rating", trim(r))
        });
        let (severity, message) = if let Some(worst) = held.iter().max_by(by_amps) {
            let clauses: Vec<&str> = (worst.exceeded.iter())
                .filter(|e| e.stand_in.is_none())
                .map(|e| e.clause.as_str())
                .collect();
            let (severity, qualifier) = self.severity(&held, scenarios.len());
            let subject = self.subject(worst);
            (severity, format!("{subject}: {}{qualifier}{remedy}", clauses.join("; ")))
        } else {
            // Only a stand-in card's limit is passed: the stated condition.
            let worst = (scenarios.iter())
                .filter(|s| !s.exceeded.is_empty())
                .max_by(by_amps)?;
            let e = worst.exceeded.first()?;
            let subject = format!("{}, and {}", self.subject(worst), e.clause);
            let condition = rating_basis::condition(&e.part, e.stand_in.as_ref()?, &subject, &e.rating);
            (Severity::Low, format!("{condition}{remedy}"))
        };
        let mut refs = vec![self.led.reference.clone()];
        refs.extend(self.series.iter().map(|s| s.reference.clone()));
        refs.extend(self.drivers().map(|d| d.reference.clone()));
        let mut nets = vec![self.anode.net.clone(), self.cathode.net.clone()];
        nets.dedup();
        Some(LintFinding {
            check: LintCheck::LedCurrentSanity,
            severity,
            message,
            refs,
            nets,
        })
    }
}
