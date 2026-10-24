//! Design-rule / physics checks that run against a parsed or solved board.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! Each check is self-contained: it takes an
//! [`ExtractedBoard`] (and, where it needs
//! physics, builds and solves its own [`Circuit`](hauksbee_ir::Circuit)), and
//! returns a verdict plus the numbers behind it. They are kept separate from
//! the bind-time [`stress`](hauksbee_bind::stress) monitor: stress watches a running
//! co-simulation against datasheet ratings, whereas a check answers a specific
//! standards question about the design.
//!
//! - [`usb_c`]: the USB Type-C CC attach classifier. It attaches a generic
//!   source + cable model to a board's CC termination and classifies the result
//!   against the USB Type-C spec windows (Sink / `AudioAccessory` / ...). This is
//!   what re-derives the `RPi` 4 shared-CC-pulldown fault cold.
//! - [`straps`]: the boot strapping-pin lint. It reads each MCU's strap table
//!   from the model db and flags a strap net that cannot hold the level the part
//!   needs at reset (a free-running clock on it, or a pull to the wrong rail).
//! - [`mcu_coverage`]: flags a recognised MCU that has no authored device model,
//!   so the strap and resource-conflict checks above could not run on it,
//!   keeping a "Looks healthy" verdict from being printed over a recognised MCU
//!   the tool never modelled.
//! - [`converter`]: discrete switching-converter topology recovery (switch node
//!   / input rail / output rail / bulk caps) from the netlist + part kinds,
//!   shared by the ampacity and ripple checks.
//! - [`ampacity`]: IPC-2221 trace-ampacity, wired into `--si`. Attributes cited
//!   currents from the DB models and runs the
//!   [`audit_trace_currents`](hauksbee_extract::audit_trace_currents) engine.
//! - [`ripple`]: input bulk-capacitor ripple-current overstress on a buck.
//! - [`contention`]: the model-aware driver-contention lint. Two parts that BIND
//!   to push-pull digital outputs on one net are fighting, whatever the
//!   schematic's pin electrical types said. The extract-layer contention check
//!   reads pin types and treats a `bidirectional` MCU pad as a resolver, so a
//!   mis-mapped logic model driving an MCU GPIO net slipped past it.
//!   `transmit_contention_lint` adds the pads a card binds to a UART transmit
//!   function (a port pad whose `uart_tx` table or role token says TXD on a
//!   serial line) as drivers, so two TXDs on one line are caught.
//! - [`back_power`]: the cross-domain pull lint. A modelled part's signal pin
//!   tied or pulled up to a rail above the part's own supply conducts through
//!   the pin's protection clamp into the lower rail: back-powering at
//!   power-down, abs-max violation powered up.
//! - [`bus_loading`]: I2C pull-up SIZING (the presence check is the extract
//!   layer's `MissingI2cPullup`): an effective pull-up demanding more sink
//!   current than the speed class's driver rating (3 mA; 20 mA in fast-mode
//!   plus; lower when a device card states `iol_a`), or too weak for the
//!   estimated bus capacitance to meet the class's rise-time budget.
//! - [`pullup_effect`]: a pull-up that is present and cannot do its job: its
//!   rail never comes up, it leaves the line below the receiver's VIH(min), or
//!   it is too large for the receivers' stated input leakage.
//! - [`audio_bus`]: an I2S/TDM bus wired against its pad roles: two
//!   transmitters on one data line, a data line nothing transmits on, BCLK on
//!   the LRCLK pad, a clock nobody or two masters drive, a required MCLK
//!   unconnected.
//! - [`power_pins`]: a carded IC whose supply pads are all open, whose return
//!   pads are all open, whose supply sits on ground or whose ground sits on a
//!   rail, judged from the card's pin roles alone.
//! - [`power_states`]: what a board does with one rail switched off and the
//!   rest on: a part with every supply off whose signal pin a rail that stays
//!   on holds high (back-feed), and an I2C bus whose pull-ups die with the
//!   gated rail or that an unpowered device clamps (bus lock-up).
//! - [`regulator_cout`]: a linear regulator whose output carries less
//!   capacitance than its card's stability minimum (`cout_min_f`).
//! - [`pin_bias`]: a pin whose card requires a pull-up, a pull-down or a defined
//!   idle level (`[[models.pin_bias]]`, non-strap pins) that the board biases the
//!   wrong way, leaves open, or pulls with a resistor outside the datasheet range.
//! - [`supply_cap`]: a supply input or regulator/load-switch output whose card
//!   states a minimum capacitance (`[[models.supply_cap]]`) that the capacitors
//!   on the net, summed, do not reach.
//! - [`holdup`]: a regulator fed from a rectifier whose bulk capacitor lets the
//!   ripple valley fall below the regulator's minimum input voltage.
//! - [`regulator_enable`]: a linear regulator whose card says an open enable
//!   pin is off (`en_open = "off"`, or an internal pull toward off) with that
//!   pin left with nothing that can raise it: its rail never comes on. It also
//!   flags an enable whose only source is the device's own output (or a net
//!   downstream of it), which can never rise.
//! - [`control_held_off`]: an active-high enable or active-low reset the board
//!   pulls to its off level with nothing that can release it: the part is never
//!   enabled, or is held in reset and never runs.
//! - [`switch_short`]: a switch or push button closed between a sourced supply
//!   rail and ground, or between two sourced rails of different voltage, with
//!   nothing in series: a short when pressed.
//! - [`signal_direction`]: a signal net wired against its pins' directions:
//!   only outputs (or only inputs) on it, or a transmit pin facing a transmit
//!   pin, judged from the symbol pin types and the cards' pad roles.
//! - [`symbol_card_pins`]: a schematic symbol pin whose name states a different
//!   pin (crystal input against output, supply against ground, reset) than the
//!   card's role for the same pad number: the board is read two ways at once.
//! - [`debug_header`]: a header named as a standard programming or serial header
//!   (FTDI 6-pin, AVR ISP 6/10, ARM Cortex 10-pin) whose pins carry other
//!   signals than the standard pin order (the board's TX on the cable's TX pin,
//!   MOSI/MISO exchanged, supply on a ground pin). Pinouts: `hauksbee_ir::header_pinouts`.
//! - [`pin_mux`]: a bus wired to MCU pads that cannot carry it: SPI, I2C, UART
//!   or PWM where the card's `[models.pin_capability.<function>]` table lists
//!   other pads (the intent read from a partner part's pin and the net name).
//!   A UART partner that transmits needs a receive pad, and one that only
//!   listens needs a transmit pad; the message names the pads that would work.
//! - [`datasheet_field`]: a part's `Datasheet` field whose file name clearly
//!   names a different device or logic family, so a model drafted from it
//!   would describe the wrong part (a low-severity, non-gating note).
//! - [`crystal_caps`]: crystal load capacitors far outside the C1/C2 range the
//!   driving IC's card states (`xtal_cap_min_f`/`xtal_cap_max_f`), for a
//!   crystal whose value states no CL.
//! - [`logic_levels`]: a CMOS logic input left between its VCC-scaled VIL
//!   and VIH, where the datasheet guarantees no logic state.
//! - [`solved_levels`]: the supply-envelope, logic-level and analog-input
//!   judges on solved levels, one implementation for a CI run's spans and for
//!   the DC operating point `check` solves; the rail-knowledge tables judge
//!   only the nets no solve covers.
//! - [`rail_basis`]: a rail powered by its name alone whose only source is a
//!   part the board does not fit (with no connector or test point on it)
//!   never comes up; other name-powered rails are stated verdict conditions,
//!   and a run that powered no rail says it judged none.
//! - [`rail_conflicts`]: a rail more than one source sets the voltage of
//!   (two regulators tied, differently named rails joined), from the binder's
//!   rail-ownership pass.
//! - [`rail_decoupling`]: a supply rail with far fewer capacitors to ground
//!   than ICs drawing from it, or no bulk capacitor on a busy rail (a count,
//!   reported as a non-gating note).
//! - [`floating_inputs`]: a CMOS logic input left with no driver, pull or
//!   other connection, no-connect flag or not.
//! - [`siblings`]: one instance of a repeated cell (a part group of eight or
//!   more with one dominant wiring signature) wired differently from its
//!   twins on two or more pads, or a minority of transistor units whose
//!   base, collector and emitter are a terminal exchange of the majority's
//!   wiring: the swapped-pin outlier. Run from [`node_drive`] on its index.
//! - [`node_drive`]: three node-level DC faults read from pin roles: a BJT
//!   base driven straight from a rail, switch or output with its emitter on a
//!   rail; a node held only by capacitor plates and high-impedance inputs;
//!   and a MOSFET gate, analog-switch select or regulator/driver enable held
//!   only by MCU or connector pins with no pull.
//! - [`fet_orientation`]: a power FET wired so its body diode defeats it: a
//!   gate-driven high-side switch that cannot turn off, or a reverse-polarity
//!   FET with drain and source inverted.
//! - [`output_release`]: drops an output-contention finding when every driver
//!   on the net is a card-stated tri-state pad released by its own chip select.
//! - [`open_drain_pullup`]: a card-stated open-drain output on a net with no
//!   pull-up resistor and no receiving MCU pin with a stated internal pull-up.
//! - [`dc_stress`]: every card rating (junction, channel and driver-pin
//!   current, resistor wattage, junction temperature) judged at the board's
//!   solved DC operating point by the co-simulation's own stress monitor.
//! - [`led_current`]: an LED string's current from the levels its two ends are
//!   held at, with no solve, against the LED's own rating, an indicator
//!   package's ceiling, the series resistor's wattage and the driving pin's
//!   rating: the one LED-current rule.
//! - [`open_paths`]: the rating claims that pass would read as met whose
//!   current path runs into a part with no working model, as abstentions.
//! - [`rest_supply`]: whether the resting draw [`dc_stress`] judges is one the
//!   rails' supplies can give; parts over a rating only at an impossible draw
//!   are one finding about the supply or the model, not overstress findings.

pub mod ampacity;
pub mod analog_inputs;
pub mod audio_bus;
pub mod back_power;
pub mod boot;
pub mod bus_loading;
pub(crate) mod card_trust;
pub mod contention;
pub mod control_held_off;
pub mod converter;
pub mod crystal_caps;
pub mod datasheet_field;
pub mod dc_stress;
pub mod debug_header;
pub mod device_decode;
pub mod envelope;
pub mod fet_orientation;
pub mod firmware_intent;
pub mod float_parity;
pub mod floating_inputs;
pub mod holdup;
pub mod indicators;
pub mod led_current;
pub mod logic_levels;
pub mod mcu_coverage;
pub mod model_controls;
pub mod node_drive;
pub mod open_drain_pullup;
pub mod open_paths;
pub mod output_release;
pub mod pin_bias;
pub mod pin_mux;
pub mod power_pins;
pub mod power_states;
pub mod pullup_effect;
pub mod rail_basis;
pub mod rail_conflicts;
pub mod rail_decoupling;
pub mod regulator_cout;
pub mod regulator_enable;
pub(crate) mod reset_state;
pub mod rest_supply;
pub mod ripple;
pub mod siblings;
pub mod signal_direction;
pub mod sim_parity;
pub mod solved_levels;
pub mod straps;
pub mod supply_cap;
pub mod switch_short;
pub mod symbol_card_pins;
#[cfg(test)]
pub(crate) mod test_support;
pub mod usb_c;

use hauksbee_bind::binder::BoardFacts;
use hauksbee_bind::variant_sweep::CornerPlan;
use hauksbee_extract::{ExtractedBoard, LintCheck, LintFinding, NetLintReport, SiReport};
use hauksbee_models::ModelLibrary;

/// Net-name classification: the one table (`hauksbee_models::netclass`).
pub(crate) use hauksbee_models::netclass::{is_ground as is_ground_name, leaf as norm};

/// The fractional tolerance a part states ("10k 1%", "±5%", or a `Tolerance`
/// property), or `None` when it states none: an unstated tolerance is never
/// invented, so a corner judged on it says it was judged at nominal values.
pub(crate) fn stated_tolerance(c: &hauksbee_extract::Component) -> Option<f64> {
    let property = (c.properties.iter())
        .filter(|(k, _)| k.eq_ignore_ascii_case("tolerance"))
        .map(|(_, v)| v.as_str());
    property
        .chain(c.value.split_whitespace())
        .find_map(|t| {
            let pct = t
                .trim()
                .trim_start_matches(['±', '+', '-'])
                .strip_suffix('%')?;
            pct.parse::<f64>().ok()
        })
        .filter(|pct| pct.is_finite() && *pct >= 0.0)
        .map(|pct| pct / 100.0)
}

/// A card's numeric param, unless it lies outside its physical range (a unit
/// slip), in which case it is read as not stated
/// ([`hauksbee_models::ranges::quarantine`]).
pub(crate) fn plausible_param(model: &hauksbee_models::ModelEntry, key: &str) -> Option<f64> {
    let value = model.params.get_f64(key)?;
    let slip = hauksbee_models::ranges::quarantine(model)
        .is_some_and(|q| q.found.iter().any(|f| f.param && f.key == key));
    (!slip).then_some(value)
}

/// `KiCad`'s placeholder net for a pin that is on no net at all.
pub(crate) fn is_unconnected_net(name: &str) -> bool {
    name.trim_start_matches('/').starts_with("unconnected-")
}

/// Whether `model`'s pin map fits `comp`'s footprint, judged as the binder
/// judges it ([`hauksbee_bind::package_fit::misfit`]).
///
/// A card the binder refuses for this footprint leaves the part unmodelled in
/// simulation; a check that read pin roles off the same card would judge a
/// different circuit (an output pin on the input rail, ground on the output),
/// so every check takes a card only where this holds.
pub(crate) fn card_fits(
    board: &ExtractedBoard,
    comp: &hauksbee_extract::Component,
    model: &hauksbee_models::ModelEntry,
) -> bool {
    let net = |id: Option<i64>| {
        let name = &board.net(id?)?.name;
        Some((
            hauksbee_models::netclass::is_canonical_ground(name),
            name.clone(),
        ))
    };
    hauksbee_bind::package_fit::misfit(comp, model, &net).is_none()
}

/// Every assembled component paired with the model that resolved it.
///
/// The three-state assembly contract, asked once: only a Present record can be
/// a resolvable part, so DNP and identity-refused records abstain rather than
/// contribute a model-derived finding. Nor does a card whose pin map does not
/// fit the footprint ([`card_fits`]).
pub(crate) fn resolved_components<'a>(
    board: &'a ExtractedBoard,
    lib: &'a ModelLibrary,
) -> impl Iterator<Item = (&'a hauksbee_extract::Component, hauksbee_models::ModelEntry)> + 'a {
    BoardStatics::resolve_all(board, lib).map(|(_, comp, model)| (comp, model))
}

/// What a board and the model library alone decide, asked of them once.
///
/// A run-evidence pass asks the same questions of the same board many times (a
/// model per part, the regulator and battery rails), and an ensemble asks them
/// again for every member. None of them reads a member's supply values, enables
/// or seed: those arrive separately, as the `solved` levels each pass takes. So
/// the answers are kept here and shared by every member that runs on this exact
/// board. A member whose board differs (a toleranced value, a pinned variant)
/// must build its own; [`Self::describes`] says whether a board is the one these
/// were taken from.
pub struct BoardStatics<'l> {
    lib: &'l ModelLibrary,
    fingerprint: u64,
    /// Each fitted part's model, and the other things the library decides.
    facts: BoardFacts,
    rails: std::sync::OnceLock<envelope::Rails>,
}

impl<'l> BoardStatics<'l> {
    /// The statics of `board` against `lib`.
    #[must_use]
    pub fn of(board: &ExtractedBoard, lib: &'l ModelLibrary) -> Self {
        Self::with_facts(board, lib, BoardFacts::derived(board, lib, None))
    }

    /// The statics of `board`, given its `facts`.
    #[must_use]
    pub fn with_facts(board: &ExtractedBoard, lib: &'l ModelLibrary, facts: BoardFacts) -> Self {
        Self {
            lib,
            fingerprint: fingerprint(board),
            facts,
            rails: std::sync::OnceLock::new(),
        }
    }

    /// What the library decides about the board, for the binder to take.
    #[must_use]
    pub const fn facts(&self) -> &BoardFacts {
        &self.facts
    }

    fn resolve_all<'a>(
        board: &'a ExtractedBoard,
        lib: &'a ModelLibrary,
    ) -> impl Iterator<
        Item = (
            usize,
            &'a hauksbee_extract::Component,
            hauksbee_models::ModelEntry,
        ),
    > + 'a {
        let each = board.components.iter().enumerate();
        each.filter_map(move |(i, comp)| {
            let part = hauksbee_extract::assembly::AssemblyState::of(comp).fitted()?;
            let model = hauksbee_bind::binder::resolve(lib, part).model?;
            card_fits(board, comp, &model).then_some((i, comp, model))
        })
    }

    /// The board's regulator and battery rails, found on first ask.
    pub(crate) fn rails(&self, board: &ExtractedBoard) -> &envelope::Rails {
        self.rails.get_or_init(|| envelope::Rails::of(board, self))
    }

    /// Whether `board` is the board these statics were taken from.
    #[must_use]
    pub fn describes(&self, board: &ExtractedBoard) -> bool {
        fingerprint(board) == self.fingerprint
    }

    /// [`resolved_components`] of the board these statics describe.
    pub(crate) fn parts<'a>(
        &'a self,
        board: &'a ExtractedBoard,
    ) -> impl Iterator<
        Item = (
            &'a hauksbee_extract::Component,
            &'a hauksbee_models::ModelEntry,
        ),
    > + 'a {
        self.facts
            .models()
            .map(|(i, model)| (&board.components[i], model))
            .filter(|(comp, model)| card_fits(board, comp, model))
    }
}

/// A hash of everything about `board`'s parts that a resolution reads.
fn fingerprint(board: &ExtractedBoard) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for c in &board.components {
        (
            &c.reference,
            &c.value,
            &c.lib_id,
            &c.footprint,
            &c.properties,
            c.dnp,
        )
            .hash(&mut h);
        c.pins.iter().for_each(|p| (&p.number, p.net).hash(&mut h));
    }
    h.finish()
}

/// The full engine-level lint.
///
/// It is the connectivity net-lint plus the model-aware
/// checks, strap pins, MCU resource conflicts, the unmodelled-MCU coverage
/// note, configured-device decode faults (e.g. a CYPD3177 PD-sink divider), and
/// model-aware driver contention.
/// Kept as one function so every surface (`--lint`, `--check`, the JSON
/// aggregate, TUI, the web front door) runs the identical set and no caller can
/// reopen the "Looks healthy" hole by forgetting one: splice `device_decode` in
/// on the `--lint` path alone and every other surface returns a false PASS on
/// those faults.
#[must_use]
pub fn engine_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    engine_lint_swept(board, lib).0
}

/// [`engine_lint`] judged at every assumed corner of the board's class-only
/// parts (a generic MOSFET symbol with no part number binds a corner set).
///
/// The unpinned run judges each set at its stand-in; each further run of the
/// [`CornerPlan`] pins the corners and adds the findings the stand-in run did
/// not raise, naming the corners that raised them. So the result passes only
/// if every corner does. Returns the plan, which the caller marks on its bind
/// report so the evidence says the result holds across the corners, not at one.
/// The sweep is bounded by the plan (exhaustive for a few sets, else joint),
/// so its cost is a small constant number of extra lint passes.
#[must_use]
pub fn engine_lint_swept(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
) -> (NetLintReport, Option<CornerPlan>) {
    let mut report = engine_lint_at(board, lib);
    let plan = CornerPlan::of(board, lib);
    for run in plan.iter().flat_map(|p| &p.runs) {
        let mut pinned = board.clone();
        hauksbee_bind::variant_sweep::pin(&mut pinned, run);
        let corners: Vec<String> = run.iter().map(|(r, id)| format!("{r} = {id}")).collect();
        for mut finding in engine_lint_at(&pinned, lib).findings {
            let seen = report.findings.iter().any(|f| {
                f.check == finding.check && f.refs == finding.refs && f.nets == finding.nets
            });
            if seen {
                continue;
            }
            let named = run
                .iter()
                .all(|(_, id)| finding.message.contains(id.as_str()));
            if !named {
                finding.message = format!(
                    "{} (at the assumed corners {})",
                    finding.message,
                    corners.join(", ")
                );
            }
            report.findings.push(finding);
        }
    }
    (report, plan)
}

/// The lint's progress, in percent of its own time: each tick below is the share, in
/// percent, of the lint's wall time spent in the passes just run (they sum to
/// 100). Measured by timestamping every tick over `stickhub` tiled 12 and 30
/// times and a private 6,884-part netlist; the passes' shares agreed to within
/// a few points across them. The first tick also carries a start-up cost that
/// does not grow with the board, and the last covers the model-control pass.
/// The layout checks that follow the lint are not here: they are one pass that
/// `layout_lint` credits whole (see `progress::layout_begins`).
/// The bar only needs the shape (a handful of passes carry most of the time),
/// so a pass that later speeds up or slows down moves the bar a little off the
/// clock and nothing else.
fn lint_tick(percent: u64) {
    hauksbee_ir::progress::lint_advance(percent);
}

/// An LED string's current judged with no solve (a GPIO-lit string, or a
/// board whose operating point does not solve). A string the DC stress
/// pass already overstresses is its finding, not a second one.
fn add_unsolved_led_findings(
    report: &mut NetLintReport,
    board: &ExtractedBoard,
    lib: &ModelLibrary,
) {
    for judged in led_current::led_strings(board, lib) {
        let reported = report.findings.iter().any(|f| {
            f.check == LintCheck::DcOverstress && f.refs.iter().any(|r| judged.string.contains(r))
        });
        if !reported {
            report.findings.push(judged.finding);
        }
    }
}

/// The open-drain pull-up and FET-orientation checks, appended to `report`. The
/// I2C and SD checks name their own buses by net, so a net they reported is not
/// reported again for an open-drain output; a backwards power FET the extract
/// layer's direct-neighbour rule already named is not named twice.
fn extend_with_wiring_direction(
    report: &mut NetLintReport,
    board: &ExtractedBoard,
    lib: &ModelLibrary,
) {
    for finding in open_drain_pullup::open_drain_pullup_lint(board, lib).findings {
        let covered = report.findings.iter().any(|r| {
            matches!(
                r.check,
                LintCheck::MissingI2cPullup | LintCheck::MissingSdPullup
            ) && r.nets.iter().any(|n| finding.nets.contains(n))
        });
        if !covered {
            report.findings.push(finding);
        }
    }
    for finding in fet_orientation::fet_orientation_lint(board, lib).findings {
        let seen = report.findings.iter().any(|r| {
            r.check == finding.check
                && r.refs == finding.refs
                && r.message.contains("body diode")
                && r.message.contains("always-on FET") == finding.message.contains("always-on FET")
        });
        if !seen {
            report.findings.push(finding);
        }
    }
}

/// The pull-up and digital-audio checks, appended to `report`. A pull-up is
/// judged against the receivers on its line and the rails the rail model says
/// come up. `back_power` already reports a pull-up that is above its
/// receiver's supply, and `contention` a pair of outputs, so what either has
/// reported on a net is not reported again here.
fn extend_with_pullup_and_audio(
    report: &mut NetLintReport,
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    bases: &hauksbee_bind::rail_basis::RailBases,
) {
    let never_up: std::collections::HashSet<String> = bases
        .rails
        .iter()
        .filter(|r| r.never_up())
        .flat_map(|r| r.nets.iter().cloned())
        .collect();
    let reported = |report: &NetLintReport, check: LintCheck, f: &LintFinding| {
        report
            .findings
            .iter()
            .any(|r| r.check == check && f.nets.iter().any(|n| r.nets.contains(n)))
    };
    let pullups: Vec<_> = pullup_effect::pullup_effect_lint(board, lib, &never_up)
        .findings
        .into_iter()
        .filter(|f| !reported(report, LintCheck::BackPower, f))
        .collect();
    report.findings.extend(pullups);
    let audio: Vec<_> = audio_bus::audio_bus_lint(board, lib)
        .findings
        .into_iter()
        .filter(|f| !reported(report, LintCheck::OutputContention, f))
        .collect();
    report.findings.extend(audio);
}

/// The power-state findings, less what `back_power` already says of the same
/// part and net: a pull-up above the part's supply is its fault in every
/// state, not a second finding about the switched one.
fn power_state_findings(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    report: &NetLintReport,
) -> Vec<LintFinding> {
    let mut found = power_states::power_state_lint(board, lib).findings;
    found.retain(|f| {
        !report.findings.iter().any(|r| {
            r.check == LintCheck::BackPower
                && r.refs.iter().any(|x| f.refs.first() == Some(x))
                && f.nets.iter().any(|n| r.nets.contains(n))
        })
    });
    found
}

/// The wiring checks that read the nets and the cards together: a switch
/// across a supply, a net wired against its pins' directions, a symbol that
/// disagrees with its card, a debug header in the wrong order, and a bus on a
/// pad that cannot carry it. The direction check gives way to the findings
/// already in `report` on the same nets.
fn extend_with_wiring_checks(
    report: &mut NetLintReport,
    board: &ExtractedBoard,
    lib: &ModelLibrary,
) {
    report
        .findings
        .extend(switch_short::switch_short_lint(board, lib).findings);
    // A net the contention, floating-input or UART checks already report is
    // theirs: the direction check says it again only for the nets they skip.
    let direction: Vec<_> = signal_direction::signal_direction_lint(board, lib)
        .findings
        .into_iter()
        .filter(|f| {
            !report.findings.iter().any(|r| {
                matches!(
                    r.check,
                    LintCheck::OutputContention
                        | LintCheck::FloatingInput
                        | LintCheck::FloatingControlPin
                        | LintCheck::PinFunction
                ) && f.nets.iter().any(|n| r.nets.contains(n))
            })
        })
        .collect();
    report.findings.extend(direction);
    report
        .findings
        .extend(symbol_card_pins::symbol_card_pin_lint(board, lib).findings);
    report
        .findings
        .extend(debug_header::debug_header_lint(board, lib).findings);
    // A transmit pad facing another transmitter is a contention finding already
    // (it names the pads that can receive), not a second one about the pad.
    let mux: Vec<_> = pin_mux::pin_mux_lint(board, lib)
        .findings
        .into_iter()
        .filter(|f| !(f.message.contains("UART") && reported_on_nets(report, f)))
        .collect();
    report.findings.extend(mux);
}

/// Whether `report` already holds a contention, UART-name or direction finding
/// on a net of `finding`.
fn reported_on_nets(report: &NetLintReport, finding: &LintFinding) -> bool {
    report.findings.iter().any(|r| {
        matches!(
            r.check,
            LintCheck::OutputContention | LintCheck::PinFunction | LintCheck::SignalDirection
        ) && finding.nets.iter().any(|n| r.nets.contains(n))
    })
}

/// A serial line with two transmitters: the pin-type check and the UART-name
/// check may already have said so on the same net, and are not repeated.
fn extend_with_transmit_contention(
    report: &mut NetLintReport,
    board: &ExtractedBoard,
    lib: &ModelLibrary,
) {
    let transmit: Vec<_> = contention::transmit_contention_lint(board, lib)
        .findings
        .into_iter()
        .filter(|f| !reported_on_nets(report, f))
        .collect();
    report.findings.extend(transmit);
}

/// One [`engine_lint`] pass over `board` as it stands.
fn engine_lint_at(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = board.net_lint();
    lint_tick(5);
    report
        .findings
        .extend(straps::strap_lint(board, lib).findings);
    lint_tick(13);
    report.findings.extend(resources_lint(board, lib).findings);
    report
        .findings
        .extend(device_decode::device_decode_lint(board, lib).findings);
    // One binding serves the checks that need the bound board: the rail
    // conflicts read its report, the DC stress pass consumes it and solves the
    // board's operating point once. Binding a large board is not cheap, so it
    // is done once here, not once per check. The levels that solve gave judge
    // the supply envelope, the logic levels and the analog inputs below; a net
    // the solve does not cover is judged on the rail knowledge the board states.
    let bound = hauksbee_bind::binder::bind_board(board, lib);
    let conflicts = rail_conflicts::rail_conflict_findings(&bound.report).findings;
    let bases = hauksbee_bind::rail_basis::rail_bases(board, lib, &bound);
    let dc = dc_stress::dc_stress_at_corners(board, lib, bound);
    let statics = BoardStatics::of(board, lib);
    let rails = |net: &str| hauksbee_bind::binder::power_rail_voltage(net);
    let judged = dc.levels.lint(board, &statics, &rails);
    report.findings.extend(judged.envelope);
    lint_tick(12);
    for findings in [
        contention::contention_lint(board, lib).findings,
        back_power::back_power_lint(board, lib).findings,
        datasheet_field::datasheet_field_lint(board).findings,
        bus_loading::bus_loading_lint(board, lib).findings,
        regulator_cout::regulator_cout_lint(board, lib).findings,
        pin_bias::pin_bias_lint(board, lib).findings,
        supply_cap::supply_cap_lint(board, lib).findings,
        holdup::holdup_lint(board, lib).findings,
        power_pins::power_pin_lint(board, lib).findings,
    ] {
        report.findings.extend(findings);
    }
    extend_with_transmit_contention(&mut report, board, lib);
    report
        .findings
        .extend(power_state_findings(board, lib, &report));
    // A regulator held off by its open enable pin is one finding: the
    // extract layer's generic floating-control note on the same pin and net
    // says less (no rail, no datasheet state) and gives way to it.
    let held_off = regulator_enable::regulator_enable_lint(board, lib).findings;
    report.findings.retain(|f| {
        f.check != LintCheck::FloatingControlPin
            || !held_off
                .iter()
                .any(|h| h.refs == f.refs && f.nets.iter().all(|n| h.nets.contains(n)))
    });
    // The same fault on any other part's enable or reset input: a pull to the
    // off level that nothing on the net can release. The two checks split a
    // regulator's enable pad between them; a part and net one has reported is
    // never reported again by the other.
    let control: Vec<_> = control_held_off::control_held_off_lint(board, lib)
        .findings
        .into_iter()
        .filter(|c| {
            !held_off
                .iter()
                .any(|h| h.refs == c.refs && c.nets.iter().all(|n| h.nets.contains(n)))
        })
        .collect();
    report.findings.extend(held_off);
    report.findings.extend(control);
    lint_tick(4);
    lint_tick(16);
    report.findings.extend(conflicts);
    // A rail powered by its name alone whose only source is unfitted never
    // comes up; a run that powered no rail judged no level.
    report
        .findings
        .extend(rail_basis::rail_basis_findings(&bases).findings);
    extend_with_pullup_and_audio(&mut report, board, lib, &bases);
    report
        .findings
        .extend(rail_decoupling::decoupling_lint(board, lib).findings);
    report
        .findings
        .extend(floating_inputs::floating_input_lint(board, lib).findings);
    report
        .findings
        .extend(node_drive::node_drive_lint(board, lib).findings);
    extend_with_wiring_direction(&mut report, board, lib);
    extend_with_wiring_checks(&mut report, board, lib);
    report.findings.extend(dc.report.findings);
    lint_tick(14);
    add_unsolved_led_findings(&mut report, board, lib);
    // The comparator half of the analog-input check and the logic-level check
    // read the levels the DC solve gave. A net it does not cover (nothing in
    // the netlist sets it, or its rail is dead) falls back to the rail table:
    // a divider tap is then an abstention, and a logic input is judged only
    // where it is tied to a named rail (3.3 V into a 5 V CMOS input). The ADC
    // source-impedance half is pure topology and runs either way.
    lint_tick(12);
    report.findings.extend(judged.analog);
    lint_tick(10);
    report.findings.extend(judged.logic);
    lint_tick(12);
    for finding in model_controls::model_control_lint(board, lib).findings {
        // Schematic/netlist extraction can already carry the pin function and
        // raise the same structural finding. The model-aware path exists for
        // PCB-only inputs whose pads have numbers/nets but no symbol function.
        if !report.findings.iter().any(|existing| {
            existing.check == finding.check
                && existing.refs == finding.refs
                && existing.nets == finding.nets
        }) {
            report.findings.push(finding);
        }
    }
    // What the floating-node detector finds that no hand check above did: a
    // net nothing holds whose level the solve shows moving with leakage alone.
    let promoted = float_parity::float_promoted(board, lib, &report.findings);
    report.findings.extend(promoted);
    lint_tick(2);
    // Last, so every check's finding is seen: what rests only on a drafted
    // card's doubtful field is a note to check the card, not a board fault.
    card_trust::cap_doubted_cards(&mut report, board, lib);
    // A card that states an output goes high-impedance when deselected (a
    // shared SPI MISO line) takes the pin-type contention finding away.
    output_release::drop_released_contention(&mut report, board, lib);
    report
}

/// Layout wiring checks on a `KiCad` layout (empty for anything else).
///
/// Reports pads copper never joins to their net and, when `board_path` has a
/// same-stem `.kicad_sch` beside it, pins the layout wires differently from
/// that schematic.
#[must_use]
pub fn layout_lint(
    board: &ExtractedBoard,
    text: Option<&str>,
    board_path: Option<&std::path::Path>,
) -> NetLintReport {
    let Some(text) = text.filter(|t| t.contains("(kicad_pcb")) else {
        hauksbee_ir::progress::layout_ends();
        return NetLintReport::default();
    };
    // One pass over the layout with nothing inside it to tick: the bar's share
    // for it is credited half before and half after.
    hauksbee_ir::progress::layout_begins();
    let schematic = board_path.and_then(ExtractedBoard::companion_schematic);
    let report = board.layout_lint(text, schematic.as_ref());
    hauksbee_ir::progress::layout_ends();
    report
}

/// The full engine-level signal-integrity report.
///
/// It is the extract-layer SI checks
/// (`board.si_checks`) PLUS the model-aware engine-layer checks whose attribution
/// needs the bound DB models, trace ampacity (IPC-2221) and input-cap ripple.
/// Kept as one chokepoint so every SI surface (`--si`, `--check`, the JSON
/// aggregate, TUI, the web front door) runs the identical set. The
/// ampacity/ripple checks appended on the dedicated `--si` path alone would
/// leave `--check`, the combined `--json`, and the web report returning a
/// false "looks healthy" over an under-width power trace or an over-ripple input
/// cap that `--si` flags; the SI twin of the `engine_lint` hole above.
/// `geo_text` is the raw layout text (None for Altium, whose geometry is not yet
/// threaded into the text-based SI checks).
#[must_use]
pub fn engine_si(board: &ExtractedBoard, lib: &ModelLibrary, geo_text: Option<&str>) -> SiReport {
    let cards = crystal_caps::crystal_cards(board, lib);
    let mut report = board.si_checks_with(geo_text, &cards);
    ampacity::append_ampacity(board, lib, geo_text, &mut report);
    ripple::append_ripple(board, lib, &mut report);
    crystal_caps::append_crystal_caps(board, lib, &cards, &mut report);
    report
}

/// The `--resources` view: the MCU internal resource-conflict check PLUS the
/// unchecked-strap-bearing-MCU coverage note.
///
/// A named chokepoint so neither the
/// `--resources` path nor `engine_lint` can drop the coverage note by forgetting
/// to append it.
#[must_use]
pub fn resources_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = board.resource_conflicts();
    report
        .findings
        .extend(mcu_coverage::mcu_coverage_lint(board, lib).findings);
    report
}
