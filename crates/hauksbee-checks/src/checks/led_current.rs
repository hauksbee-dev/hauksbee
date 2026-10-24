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
//! LED's current rating, the series resistor's wattage (its card's; a
//! footprint's standard wattage is an assumption about the part, judged only
//! past every chip resistor of the size) and the driving pin's current rating.
//!
//! A limit from a stand-in card (an LED guessed from its colour, the generic
//! LED's assumed corners) is never a failing finding ([`hauksbee_models::rating_basis`]): the DC
//! stress pass states it as a condition. One limit holds whatever indicator
//! LED is fitted: on a chip (0402 to 1206) or round 3/5 mm package the stand-in
//! card's pulsed peak current (`package_ceiling`: 100 mA at duty 1/10 for the
//! Wurth WL-SMCW family the generic card is drawn from, over three times its
//! 30 mA continuous rating). A steady current past that is past the continuous
//! rating of any LED in the package, so it is judged as a limit the board
//! states, like a resistor footprint's wattage.
//!
//! An LED whose card names forward-voltage corners is judged at every corner
//! and the resistor at its stated tolerance: a judged limit passed at every
//! corner of a rail-held string is high severity, at only some corners a
//! conditional medium one, and on a firmware- or logic-driven string medium,
//! since the driver decides when it is lit. A string whose ends are not both
//! held (an open-drain status pin, a transistor switch) is not judged.
//!
//! This is the one LED-current rule: the connectivity lint's old fixed-Vf
//! estimate is retired in its favour.
//!

use std::collections::BTreeMap;

use hauksbee_bind::binder::{resolve, role_nets};
use hauksbee_bind::digital::{output_roles, LogicLevels};
use hauksbee_bind::stress::resistor_power_from_footprint;
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_led, is_plain_resistor, Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport,
    Severity,
};
use hauksbee_ir::units::{format_amps, format_number, format_percent, format_volts, format_watts};
use hauksbee_models::rating_basis::{Limit, StandIn};
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
    power: Option<Wattage>,
}

/// A series resistor's wattage.
struct Wattage {
    rated: Rated,
    /// Where the rating came from (`card 'x'`, `its R_0603 footprint`).
    from: String,
    /// Read from the footprint's size alone: the standard rating of a chip
    /// resistor that size, an assumption about the fitted part.
    assumed: bool,
}

/// How far past a footprint's standard wattage a resistor must dissipate
/// before that assumed rating is judged. The footprint states the size, not
/// the part: a high-power or anti-surge chip of the same size is rated a few
/// times higher, so only a dissipation past every chip of the size is a
/// finding about the board. Below it the DC ratings pass states the
/// assumption at its solve.
const FOOTPRINT_MARGIN: f64 = 4.0;

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
    /// The package ceiling ([`package_ceiling`]) when the card's own current
    /// rating is a stand-in: amps and the package, in words.
    ceiling: Option<(f64, String)>,
}

/// One rating passed in one scenario.
struct Exceeded {
    /// What was seen, against what.
    clause: String,
    stand_in: Option<StandIn>,
}

/// The string judged at one LED card and one end of the tolerance span.
struct Scenario {
    amps: f64,
    /// The LED's forward drop at that current, and the card it is from.
    drop: f64,
    card: String,
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
        let cards: Vec<Led> = (models.iter())
            .filter_map(|model| Led::of(model, led))
            .collect();
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

    /// A resistor's wattage: its own card's, else the one its footprint
    /// package states (which the board states, so it is judged), else a
    /// stand-in card's.
    fn resistor_power(&self, resistor: &Component) -> Option<Wattage> {
        let model = AssemblyState::of(resistor)
            .fitted()
            .and_then(|part| resolve(self.lib, part).model);
        let card = model.and_then(|model| {
            let watts = model.ratings.max_power_w?;
            let stand_in = model.ratings.basis.stand_in_for(Limit::Power).cloned();
            Some(Wattage {
                rated: (watts, stand_in),
                from: format!("card '{}'", model.id),
                assumed: false,
            })
        });
        if card.as_ref().is_some_and(|c| c.rated.1.is_none()) {
            return card;
        }
        let footprint = &resistor.footprint;
        let package = footprint.rsplit(':').next().unwrap_or(footprint);
        (resistor_power_from_footprint(footprint).watts)
            .map(|watts| Wattage {
                rated: (watts, None),
                from: format!("its {package} footprint"),
                assumed: true,
            })
            .or(card)
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
        let mut found =
            (self.statics.parts(self.board)).filter(|(comp, model)| drives(comp, model));
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

/// Footprint words of a mid- or high-power LED package, which no indicator
/// ceiling bounds.
const POWER_PACKAGES: [&str; 12] = [
    "5050", "3535", "2835", "3528", "5730", "3030", "CREE", "LUXEON", "OSLON", "POWER", "PLCC4",
    "WS2812",
];

/// Footprint words of a chip (0402 to 1206, inch and metric codes) or round
/// 3/5 mm through-hole indicator LED package.
const INDICATOR_PACKAGES: [&str; 16] = [
    "0402", "0603", "0805", "1206", "1005", "1608", "2012", "3216", "D3.0MM", "D5.0MM", "LED3MM",
    "LED5MM", "LED-3MM", "LED-5MM", "LED_3MM", "LED_5MM",
];

/// The steady current no indicator LED on `led`'s package is rated to carry:
/// the stand-in `model`'s pulsed peak (`max_surge_current_a`), when its
/// continuous rating is a stand-in's and the footprint names a chip (0402 to
/// 1206) or round 3/5 mm indicator package. `None` for any other package (a
/// mid- or high-power LED may carry hundreds of milliamps) or an own card,
/// whose own rating is judged instead.
///
/// Source: the generic LED card's family, Wurth Elektronik WL-SMCW 0603
/// (150060RS75000 and siblings, rev 002.009, Absolute Maximum Ratings): IF
/// 30 mA continuous, IF peak 100 mA at duty 1/10 and 1 kHz, every colour.
fn package_ceiling(led: &Component, model: &ModelEntry) -> Option<(f64, String)> {
    model.ratings.basis.stand_in_for(Limit::Current)?;
    let peak = model.ratings.max_surge_current_a.filter(|a| *a > 0.0)?;
    let footprint = led.footprint.rsplit(':').next().unwrap_or(&led.footprint);
    let upper = footprint.to_ascii_uppercase();
    if POWER_PACKAGES.iter().any(|t| upper.contains(t)) {
        return None;
    }
    let indicator = (INDICATOR_PACKAGES.iter()).any(|t| upper.contains(t));
    indicator.then(|| (peak, footprint.to_string()))
}

impl Led {
    fn of(model: &ModelEntry, led: &Component) -> Option<Self> {
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
            ceiling: package_ceiling(led, model),
        })
    }

    fn nvt(&self) -> f64 {
        self.n * hauksbee_ir::thermal_voltage_c(hauksbee_ir::Circuit::new().temp_c)
    }

    /// The LED's forward drop carrying `amps`.
    fn drop(&self, amps: f64) -> f64 {
        amps.mul_add(self.rs, self.nvt() * (amps / self.is).ln_1p())
    }

    /// The current through the LED and `ohms` in series across `volts`.
    fn current(&self, volts: f64, ohms: f64) -> f64 {
        let nvt = self.nvt();
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

impl LedString<'_> {
    const fn ends(&self) -> [&End; 2] {
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
            let Some(Wattage {
                rated: (rating, stand_in),
                from,
                assumed,
            }) = &s.power
            else {
                continue;
            };
            let dissipated = current * current * r;
            if *assumed && dissipated > FOOTPRINT_MARGIN * rating {
                out.push(Exceeded {
                    clause: format!(
                        "{} dissipates {}, {:.0} times the {} a standard chip resistor the \
                         size of {from} is rated for (assumed from its size: the board does not \
                         state the part's own rating), past any chip resistor that size",
                        s.reference,
                        format_watts(dissipated),
                        dissipated / rating,
                        format_watts(*rating),
                    ),
                    stand_in: None,
                });
            } else if !*assumed && dissipated > *rating {
                out.push(Exceeded {
                    clause: format!(
                        "{} dissipates {}, over its {} rating (from {from})",
                        s.reference,
                        format_watts(dissipated),
                        format_watts(*rating)
                    ),
                    stand_in: stand_in.clone(),
                });
            }
        }
        let reference = &self.led.reference;
        if let Some((ceiling, package)) = (led.ceiling.as_ref()).filter(|(a, _)| current > *a) {
            out.push(Exceeded {
                clause: format!(
                    "{reference} carries {} steadily, over even the {} pulsed peak (duty 1/10) \
                     of an indicator LED, past the continuous rating of any LED in its {package} \
                     package (the generic limit; card '{}' gives no part's own rating)",
                    format_amps(current),
                    format_amps(*ceiling),
                    led.card,
                ),
                stand_in: None,
            });
        }
        if let Some((rating, stand_in)) = (led.max_a.as_ref()).filter(|(a, _)| current > *a) {
            out.push(Exceeded {
                clause: format!(
                    "{reference} carries {}, over its {} current rating",
                    format_amps(current),
                    format_amps(*rating)
                ),
                stand_in: stand_in.clone(),
            });
        }
        for d in self.drivers() {
            if let Some((rating, stand_in)) = (d.pin_limit.as_ref()).filter(|(a, _)| current > *a) {
                out.push(Exceeded {
                    clause: format!(
                        "{}'s pin passes {}, over its {} pin rating",
                        d.reference,
                        format_amps(current),
                        format_amps(*rating)
                    ),
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
                    drop: card.drop(current),
                    card: card.card.clone(),
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
        // 2 % steps up to a thousand times the fitted value.
        for _ in 0..350 {
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
                format!("'{}' at {}", end.net, format_volts(end.max_v))
            } else {
                format!(
                    "'{}' at {} to {}",
                    end.net,
                    format_volts(end.min_v),
                    format_volts(end.max_v)
                )
            };
        };
        let ro = if d.ro_stated {
            format!("its card's {} Ω output resistance", format_number(d.ro, 2))
        } else {
            format!(
                "an assumed {} Ω output resistance (its card states none)",
                format_number(d.ro, 2)
            )
        };
        let level = if sources {
            format!(
                "high ({} to {})",
                format_volts(end.min_v),
                format_volts(end.max_v)
            )
        } else {
            "low".to_string()
        };
        format!(
            "'{}' when {} drives it {level} through {ro}",
            end.net, d.reference
        )
    }

    /// What the string carries at `worst`, in words.
    fn subject(&self, worst: &Scenario) -> String {
        let resistors: Vec<String> = (self.series.iter())
            .map(|s| {
                let tolerance = s.tolerance.map_or_else(
                    || "no tolerance stated".to_string(),
                    |t| format!("±{}", format_percent(t * 100.0, 0)),
                );
                format!(
                    "{} ({} Ω, {tolerance})",
                    s.reference,
                    format_number(s.ohms, 2)
                )
            })
            .collect();
        let corner = if worst.corner.is_empty() {
            String::new()
        } else {
            format!(" (at {})", worst.corner)
        };
        format!(
            "{} lit from {} through {} to {} carries up to {}{corner}, its forward drop \
             {} there (card '{}')",
            self.led.reference,
            Self::held(&self.anode, true),
            resistors.join(" and "),
            Self::held(&self.cathode, false),
            format_amps(worst.amps),
            format_volts(worst.drop),
            worst.card
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
        let why = if at.is_empty() {
            " (CONDITIONAL: only at the high end of its rail and resistor tolerances)".to_string()
        } else {
            format!(
                " (CONDITIONAL: only at {}; the board does not say which part is fitted, so \
                 the fitted part decides whether it holds)",
                at.join(" / ")
            )
        };
        (Severity::Medium, why)
    }

    /// The finding for a string that passes a rating in any scenario.
    fn finding(&self, cards: &[Led]) -> Option<LintFinding> {
        let scenarios = self.scenarios(cards);
        let by_amps = |x: &&Scenario, y: &&Scenario| x.amps.total_cmp(&y.amps);
        let held: Vec<&Scenario> = scenarios.iter().filter(|s| s.judged()).collect();
        let remedy = self.safe_ohms(cards).map_or_else(String::new, |r| {
            format!(
                "; a series resistance of at least {} Ω keeps every rating",
                format_number(r, 2)
            )
        });
        // Only a judged limit is a finding here: a stand-in card's limit
        // passed is the condition the DC stress pass states.
        let worst = held.iter().copied().max_by(by_amps)?;
        let clauses: Vec<&str> = (worst.exceeded.iter())
            .filter(|e| e.stand_in.is_none())
            .map(|e| e.clause.as_str())
            .collect();
        let (severity, qualifier) = self.severity(&held, scenarios.len());
        let subject = self.subject(worst);
        let message = format!("{subject}: {}{qualifier}{remedy}", clauses.join("; "));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::test_support::{board, part};

    const NETS: [(i64, &str); 6] = [
        (1, "GND"),
        (2, "+3V3"),
        (3, "/MID"),
        (4, "+5V"),
        (5, "/GPIO"),
        (6, "+12V"),
    ];

    fn with_footprint(mut c: Component, footprint: &str) -> Component {
        c.footprint = footprint.into();
        c
    }

    fn led(value: &str, anode: i64, cathode: i64) -> Component {
        let pads = [("A", anode), ("K", cathode)];
        with_footprint(part("D1", value, &pads), "LED_SMD:LED_0603_1608Metric")
    }

    fn resistor(value: &str, a: i64, b: i64) -> Component {
        let pads = [("1", a), ("2", b)];
        with_footprint(part("R1", value, &pads), "Resistor_SMD:R_0603_1608Metric")
    }

    fn lint(parts: Vec<Component>) -> Vec<LintFinding> {
        let report = led_current_lint(&board(&NETS, parts), &ModelLibrary::builtin());
        (report.findings.into_iter())
            .filter(|f| f.check == LintCheck::LedCurrentSanity)
            .collect()
    }

    /// Faulty: a red 0603 LED from +3V3 with its cathode resistor cut to
    /// 1 ohm carries well over 100 mA, past the indicator package's ceiling
    /// at every end of the rail's band, though its colour card is a stand-in.
    #[test]
    fn a_one_ohm_cathode_resistor_overdrives_a_red_indicator() {
        let found = lint(vec![led("RED", 2, 3), resistor("1", 3, 1)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(f.severity, Severity::High, "{}", f.message);
        assert!(f.refs.contains(&"D1".to_string()) && f.refs.contains(&"R1".to_string()));
        assert!(f.nets.contains(&"+3V3".to_string()), "{:?}", f.nets);
        for words in [
            "pulsed peak",
            "100 mA",
            "'led_red'",
            "forward drop",
            "at least",
        ] {
            assert!(f.message.contains(words), "{words}: {}", f.message);
        }
    }

    /// Legal controls: the same red LED through the board's 120 ohm, and a
    /// generic LED through 330 ohm or 1k from +3V3 or +5V, are silent.
    #[test]
    fn typical_indicator_resistors_are_silent() {
        assert!(lint(vec![led("RED", 2, 3), resistor("120", 3, 1)]).is_empty());
        for rail in [2, 4] {
            for ohms in ["330", "1k"] {
                let found = lint(vec![resistor(ohms, rail, 3), led("LED", 3, 1)]);
                assert!(found.is_empty(), "{ohms} on net {rail}: {found:#?}");
            }
        }
    }

    /// A generic LED (colour unknown) from +5V through 1 ohm: past the
    /// package ceiling at the low forward-voltage corner at least, so a
    /// gating finding naming the LED and its resistor.
    #[test]
    fn a_one_ohm_resistor_on_a_generic_led_is_gating() {
        let found = lint(vec![resistor("1", 4, 3), led("LED", 3, 1)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            matches!(found[0].severity, Severity::High | Severity::Medium),
            "{}",
            found[0].message
        );
        assert!(
            found[0].message.contains("pulsed peak"),
            "{}",
            found[0].message
        );
    }

    /// A stand-in card's own 25 mA passed, but nothing the board or the
    /// package states: no finding here (the DC stress pass states that
    /// condition at its solve).
    #[test]
    fn a_stand_in_rating_alone_is_not_a_finding() {
        // About 30 mA at the red corner: over 25 mA, under the 100 mA ceiling,
        // and 0.09 W in a 1206's 0.25 W.
        let series = with_footprint(
            part("R1", "100", &[("1", 4), ("2", 3)]),
            "Resistor_SMD:R_1206_3216Metric",
        );
        let found = lint(vec![series, led("LED", 3, 1)]);
        assert!(found.is_empty(), "{found:#?}");
    }

    /// A DNP option resistor on the anode net is not part of the string: the
    /// fitted 1 ohm resistor is, and the string it makes is judged.
    #[test]
    fn a_dnp_resistor_beside_the_string_does_not_hide_it() {
        let series = with_footprint(
            part("R1", "1", &[("1", 4), ("2", 3)]),
            "Resistor_SMD:R_0402_1005Metric",
        );
        let mut option = with_footprint(
            part("R0", "DNP", &[("1", 3), ("2", 1)]),
            "Resistor_SMD:R_0402_1005Metric",
        );
        // What the DNP policy records for a "DNP" value in the real pipeline.
        option.dnp = true;
        let found = lint(vec![series, option, led("LED", 3, 1)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].refs.contains(&"R1".to_string()));
        assert!(!found[0].refs.contains(&"R0".to_string()));
    }

    /// A footprint states a resistor's size, not its own rating: 68 ohm in
    /// 0402 from +5V dissipates about 0.17 W, under three times the 1/16 W of
    /// a standard 0402, which a high-power 0402 may carry. Not a finding.
    #[test]
    fn a_footprint_wattage_passed_inside_the_margin_is_not_a_finding() {
        let series = with_footprint(
            part("R1", "68", &[("1", 4), ("2", 3)]),
            "Resistor_SMD:R_0402_1005Metric",
        );
        let found = lint(vec![series, led("LED", 3, 1)]);
        assert!(found.is_empty(), "{found:#?}");
    }

    /// 200 ohm in 0402 from +12V carries about 50 mA (inside the indicator
    /// ceiling) and dissipates about 0.5 W, eight times a standard 0402's
    /// rating: past any chip resistor that size, stated as an assumption.
    #[test]
    fn a_resistor_far_past_its_footprint_wattage_is_judged_as_an_assumption() {
        let series = with_footprint(
            part("R1", "200", &[("1", 6), ("2", 3)]),
            "Resistor_SMD:R_0402_1005Metric",
        );
        let found = lint(vec![series, led("LED", 3, 1)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        let message = &found[0].message;
        assert!(
            message.contains("R1 dissipates") && message.contains("assumed from its size"),
            "{message}"
        );
        assert!(!message.contains("pulsed peak"), "{message}");
    }

    /// An MCU pin lighting the LED through 1 ohm passes the pin's own 40 mA
    /// rating (the `ATmega328P` card); through 330 ohm it does not.
    #[test]
    fn a_gpio_lit_led_is_judged_against_the_pin_rating() {
        let mcu = |r: &str| {
            let u1 = part("U1", "ATmega328P", &[("7", 4), ("8", 1), ("19", 5)]);
            vec![u1, resistor(r, 5, 3), led("RED", 3, 1)]
        };
        let found = lint(mcu("1"));
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(f.severity, Severity::Medium, "{}", f.message);
        assert!(f.message.contains("U1's pin passes"), "{}", f.message);
        assert!(f.refs.contains(&"U1".to_string()));
        assert!(lint(mcu("330")).is_empty());
    }
}
