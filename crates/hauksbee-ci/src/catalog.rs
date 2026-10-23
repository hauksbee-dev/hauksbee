//! The assertion catalogue: one table of every `kind` the spec loader accepts.
//!
//! The CLI (`hauksbee-ci explain`), the report's "why it fails" line, the server's
//! `GET /api/ci/catalog` and `GET /api/ci/schema`, and the app's check pickers all
//! read the same words. A test pins the table to the loader's closed vocabulary, so
//! a new kind cannot ship without an entry; another pins every field name to a real
//! `Assertion` field, and another runs every example through the spec validator, so
//! a documented example cannot rot. Entries are plain data: add a field to
//! [`Entry`] and every surface can show it.
//!
//! Version 2 adds what a click-to-assert UI needs: where each kind applies
//! (`offers`), typed fields with a recipe for a sensible pre-filled value
//! (`default_from`), and what else the spec must declare with the fix (`needs`).
//! The v1 keys stay in the JSON, derived from the same data.

use std::fmt::Write as _;
use std::sync::OnceLock;

use schemars::generate::SchemaSettings;
use serde_json::{json, Value};

use hauksbee_frontdoor_api::actions::WebAction;

use crate::spec::{Assertion, Spec};

/// Where an assertion kind can be offered. A pin selection reuses the `Net`
/// offers of the pin's net, so there is no separate pin target.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Net,
    Part,
    Board,
    Uart,
    Peripheral,
    /// A scope trace the user is looking at: "Assert this" on a probed net.
    Trace,
    /// A decoded bus transaction.
    Bus,
    /// A bench-view row: a reset count, a park site, a decoded baud.
    Bench,
}

impl Target {
    const fn name(self) -> &'static str {
        match self {
            Self::Net => "net",
            Self::Part => "part",
            Self::Board => "board",
            Self::Uart => "uart",
            Self::Peripheral => "peripheral",
            Self::Trace => "trace",
            Self::Bus => "bus",
            Self::Bench => "bench",
        }
    }
}

/// What else a spec must declare before the assertion can run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Need {
    Firmware,
    Ac,
    Scenario,
    Search,
    Trace,
    Supply,
    Peripheral,
    /// The bench record of a co-sim run (bus log, reset count, decoded baud).
    Bench,
}

impl Need {
    /// `(wire token, how a spec spells it, the fix in plain words)`.
    const fn copy(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Bench => ("bench", "bench data", "Needs the bench record of a firmware run (bus log, reset count, decoded baud). Runs do not record it yet, so the check is accepted into the spec but reported as not judged, never as a pass."),
            Self::Firmware => ("firmware", "firmware", "Load a firmware image (.hex or .elf); in a spec it is `firmware = \"fw.hex\"`."),
            Self::Ac => ("ac", "[ac]", "Add an `[ac]` block (`fstart`, `fstop`, `points`) so the small-signal sweep runs."),
            Self::Scenario => ("scenario", "[[scenario]]", "Add a `[[scenario]]` that steps a part's load, give it an `id` and name that id here."),
            Self::Search => ("search", "[search]", "Add a `[search]` block with a `[[search.sweep]]` of two or more values on the swept net."),
            Self::Trace => ("trace", "a trace file", "Capture the real board's waveform into a `trace.toml` and point `trace` at it."),
            Self::Supply => ("supply", "[[supply]] kind = \"battery\"", "Declare the supply with a `[[supply]]` block whose `kind` is `\"battery\"`."),
            Self::Peripheral => ("peripheral", "[[peripheral]]", "Declare the device with a `[[peripheral]]` (or `[[sensor]]`) block and use its `id`."),
        }
    }
}

impl Need {
    /// The button that meets this need inside the web app, when the app has one.
    /// The words in [`Self::copy`] spell the spec (`[[scenario]]`, `[ac]`) for a
    /// person editing TOML; this is what a person clicking through uses instead.
    /// `search`, `trace` and `peripheral` have no guided flow yet, so they offer
    /// no action and the app falls back to the spec text.
    fn action(self) -> Option<WebAction> {
        match self {
            Self::Firmware => Some(WebAction::add_firmware()),
            Self::Ac => Some(WebAction::add_ac_sweep()),
            Self::Scenario => Some(WebAction::add_scenario()),
            Self::Supply => Some(WebAction::add_supply(None, None)),
            Self::Search | Self::Trace | Self::Peripheral | Self::Bench => None,
        }
    }
}

/// Where a pre-filled value comes from. Serialized as `{"source": ..}`; the UI
/// resolves it against the live frame, the measurement probe or the part's card.
#[derive(Clone, Copy, Debug)]
pub enum DefaultFrom {
    /// The measured value times `1 + pct / 100` (`-5` is 5 % under it).
    Measured(f64),
    /// The measured peak times `factor`.
    MeasuredPeak(f64),
    /// The named `Ratings` key of the bound card times `derate`, else `fallback`.
    Rating {
        key: &'static str,
        derate: f64,
        fallback: &'static Self,
    },
    /// VIL and VIH from the cards of the parts the net drives.
    CardLogicLevels,
    /// The first line the firmware printed.
    FirstUartLine,
    /// A fixed number.
    Constant(f64),
    /// A named datum of the run's bench view (`reset_count`, `uart_baud`,
    /// `park_symbol`, `bus_addr`...), read from the observation the user is
    /// looking at; empty until a surface has that observation.
    Bench(&'static str),
}

impl DefaultFrom {
    fn to_json(self) -> Value {
        match self {
            Self::Measured(pct) => json!({ "source": "measured", "pct": pct }),
            Self::MeasuredPeak(factor) => json!({ "source": "measured_peak", "factor": factor }),
            Self::Rating {
                key,
                derate,
                fallback,
            } => json!({
                "source": "rating", "key": key, "derate": derate, "fallback": fallback.to_json(),
            }),
            Self::CardLogicLevels => json!({ "source": "card_logic_levels" }),
            Self::FirstUartLine => json!({ "source": "first_uart_line" }),
            Self::Constant(value) => json!({ "source": "constant", "value": value }),
            Self::Bench(key) => json!({ "source": "bench", "key": key }),
        }
    }
}

/// One field of an assertion: its name, what it means, the value used when absent,
/// and how an editor can pre-fill it.
pub struct Field {
    pub name: &'static str,
    pub what: &'static str,
    /// What happens when the field is left out (prose; empty for a required field).
    pub default: &'static str,
    pub from: Option<DefaultFrom>,
}

impl Field {
    const fn fill(self, from: DefaultFrom) -> Self {
        Self {
            from: Some(from),
            ..self
        }
    }
}

const fn req(name: &'static str, what: &'static str) -> Field {
    Field {
        name,
        what,
        default: "",
        from: None,
    }
}
const fn opt(name: &'static str, what: &'static str, default: &'static str) -> Field {
    Field {
        name,
        what,
        default,
        from: None,
    }
}

/// Everything known about one assertion kind.
pub struct Entry {
    pub kind: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
    /// Fields the loader refuses to run without (one of a pair is noted in `what`).
    pub required: &'static [Field],
    pub optional: &'static [Field],
    /// What else the spec must declare, e.g. firmware, `[ac]`, `[[scenario]]`.
    pub needs: &'static [Need],
    /// Valid TOML that passes `hauksbee-ci check`: the `[[assert]]` plus any block it needs.
    pub example: &'static str,
    /// The usual physical or spec cause when the assertion fails.
    pub fails_because: &'static str,
}

impl Entry {
    /// Required fields first, then optional ones.
    pub fn fields(&self) -> impl Iterator<Item = &Field> {
        self.required.iter().chain(self.optional)
    }
}

const NAME: Field = opt(
    "name",
    "the label shown in reports",
    "built from the other fields",
);

/// Where each kind is offered, in the offer's own words. `{net}`, `{ref}` and `{id}`
/// are the picked object. An offer names no placeholder for a value ("..."): the
/// check editor that opens asks for it.
#[rustfmt::skip]
static OFFERS: &[(&str, Target, &str)] = &[
    ("voltage", Target::Net, "Keep {net} within a voltage range"),
    ("rail_window", Target::Net, "Hold {net} through a load step"),
    ("no_faults", Target::Board, "Nothing is over-stressed anywhere"),
    ("max_current", Target::Part, "Set a current limit for {ref}"),
    ("max_temp", Target::Part, "Set a temperature limit for {ref}"),
    ("uart", Target::Uart, "Firmware prints an expected message"),
    ("toggle", Target::Net, "{net} toggles at the expected rate"),
    ("pulse_width", Target::Net, "The first pulse on {net} is the expected width"),
    ("logic_level", Target::Net, "{net} reaches a valid logic level"),
    ("boot_coverage", Target::Net, "Firmware drives {net} to its level soon after reset"),
    ("protection_trip", Target::Net, "The supply on {net} trips its protection (or not)"),
    ("peripheral", Target::Peripheral, "{id} ends in the right state"),
    ("phase_margin", Target::Net, "The loop at {net} has enough phase margin"),
    ("ac_gain", Target::Net, "Gain at {net} is as expected"),
    ("model_coverage", Target::Board, "Enough of the board is modelled"),
    ("hwtrace", Target::Net, "{net} matches a captured trace"),
    ("fires", Target::Part, "{ref} trips at its threshold"),
    ("increases", Target::Part, "{ref} fires more as a swept net rises"),
    ("decreases", Target::Part, "{ref} fires less as a swept net rises"),
    // Offers from what a user is looking at: a scope trace, a decoded bus
    // transaction, a bench row, a UART line. "Assert this" and "Assert what I
    // expected" both open the same editor, pre-filled from the observation.
    ("toggle", Target::Trace, "Assert that {net} keeps toggling like this"),
    ("voltage", Target::Trace, "Assert that {net} stays within this range"),
    ("pulse_width", Target::Trace, "Assert that the first pulse on {net} is this wide"),
    ("bus", Target::Bus, "Assert that this device keeps answering on the bus"),
    ("resets", Target::Bench, "Assert that the MCU resets no more than this"),
    ("reaches", Target::Bench, "Assert that the firmware reaches this function"),
    ("uart_baud", Target::Uart, "Assert that the UART runs at this baud rate"),
];

/// The offers for `kind`: `(target, words)` pairs.
pub fn offers_for(kind: &str) -> impl Iterator<Item = (Target, &'static str)> + '_ {
    OFFERS
        .iter()
        .filter(move |(k, _, _)| *k == kind)
        .map(|&(_, t, w)| (t, w))
}

/// Every kind, in the order the CLI and the app list them.
// A data table, a few lines per kind; rustfmt would wrap every field onto its own line.
#[rustfmt::skip]
pub static CATALOG: &[Entry] = &[
    Entry { kind: "voltage", title: "A net stays within a voltage range", summary: "A net's worst dip (min) and worst peak (max) stay inside your bounds, optionally only after it settles.",
        required: &[req("net", "the net to watch"), req("min", "lowest allowed volts (or give max)").fill(DefaultFrom::Measured(-5.0))],
        optional: &[opt("max", "highest allowed volts", "no ceiling").fill(DefaultFrom::Measured(5.0)), opt("after_ms", "only judge from this time on", "the whole run"), NAME],
        needs: &[],
        example: "[[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = 4.75\nafter_ms = 50\n",
        fails_because: "the rail left its window; check the supply feeding this net and the load pulling it down." },
    Entry { kind: "rail_window", title: "A rail survives a load step", summary: "During a [[scenario]] load step, a rail stays within bounds, dips only briefly and recovers in time.",
        required: &[req("net", "the rail to watch"), req("scenario", "the [[scenario]] id to judge over"), req("min", "brownout floor in volts (or max, or dip_below with for_max_ms)").fill(DefaultFrom::Measured(-5.0))],
        optional: &[opt("max", "ceiling in volts", "none").fill(DefaultFrom::Measured(5.0)), opt("dip_below", "a level it may sit under only briefly", "none"), opt("for_max_ms", "how long it may sit under dip_below", "none"), opt("recover_to", "level it must return to", "none"), opt("recover_within_ms", "by this long", "none"), opt("spike_above", "overvoltage level", "none"), opt("spike_for_max_ms", "how long above spike_above", "none"), opt("settle_to", "level to settle back to after a spike", "none"), opt("settle_within_ms", "by this long", "none")],
        needs: &[Need::Scenario],
        example: "[[scenario]]\nid = \"load_step\"\npart = \"U1\"\nprofile = \"esp32_boot_wifi\"\nsupply_net = \"+5V\"\nstart_ms = 1.0\n\n[[assert]]\nkind = \"rail_window\"\nnet = \"+5V\"\nscenario = \"load_step\"\nmin = 3.0\n",
        fails_because: "the rail dipped/spiked or recovered/settled outside the window; check the scenario's load step and the decoupling on this net." },
    Entry { kind: "no_faults", title: "Nothing is over-stressed", summary: "The stress monitor raised no over-current, over-voltage, over-power, reverse-bias or over-temperature fault anywhere on the board.",
        required: &[],
        optional: &[NAME],
        needs: &[],
        example: "[[assert]]\nkind = \"no_faults\"\n",
        fails_because: "the stress monitor tripped; the named component exceeded a rating (over-current / -voltage / -power / -temp / reverse-bias); check its part value and supply." },
    Entry { kind: "max_current", title: "A part's current stays below a limit", summary: "A component's peak through-current stays under a ceiling (tracked for resistors and diodes; no_faults covers the rest).",
        required: &[req("ref", "the component, e.g. R1"), req("amps", "the ceiling in amps").fill(DefaultFrom::Rating { key: "max_current_a", derate: 0.8, fallback: &DefaultFrom::MeasuredPeak(2.0) })],
        optional: &[NAME],
        needs: &[],
        example: "[[assert]]\nkind = \"max_current\"\nref = \"R1\"\namps = 0.02\n",
        fails_because: "the part drew more than its limit; check its load and the override value." },
    Entry { kind: "max_temp", title: "A part stays below a temperature", summary: "A component's steady-state junction temperature (ambient + power x thermal resistance) stays under a ceiling.",
        required: &[req("ref", "the component, e.g. U1")],
        optional: &[opt("celsius", "your ceiling; omit to use the part's own datasheet Tj(max), which needs a real model", "the model's Tj(max)"), NAME],
        needs: &[],
        example: "[[assert]]\nkind = \"max_temp\"\nref = \"U1\"\ncelsius = 125\n",
        fails_because: "the junction ran hotter than the limit; check dissipation and the `ambient_c` assumption." },
    Entry { kind: "uart", title: "Firmware prints a string", summary: "The firmware's serial output contains a string, or matches a regex.",
        required: &[req("contains", "text that must appear (or give matches)").fill(DefaultFrom::FirstUartLine)],
        optional: &[opt("matches", "a regex instead of contains", "none"), opt("mcu", "which MCU's UART to read", "all MCUs"), NAME],
        needs: &[Need::Firmware],
        example: "[[assert]]\nkind = \"uart\"\ncontains = \"hauksbee-demo v1\"\n",
        fails_because: "the expected UART text never appeared; check the firmware image and baud, and that the MCU booted." },
    Entry { kind: "toggle", title: "A net toggles at a rate", summary: "A net changes at an expected frequency (a blink check), or at least a minimum number of times.",
        required: &[req("net", "the net to watch"), req("freq_hz", "expected frequency (or give min_toggles, not both)").fill(DefaultFrom::Measured(0.0))],
        optional: &[opt("min_toggles", "a count instead of a frequency", "none"), opt("tolerance", "fraction of freq_hz, in (0, 1]", "0.25").fill(DefaultFrom::Constant(0.1)), NAME],
        needs: &[Need::Firmware],
        example: "[[assert]]\nkind = \"toggle\"\nnet = \"D13\"\nfreq_hz = 5.0\ntolerance = 0.4\n",
        fails_because: "the net toggled the wrong number of times; check the firmware's drive rate and the deadline window." },
    Entry { kind: "pulse_width", title: "The first pulse is a set width", summary: "The width in milliseconds of the first complete pulse on a net; refuses if no pulse completes.",
        required: &[req("net", "the net to watch")],
        optional: &[opt("level", "high or low pulse", "high"), opt("after_ms", "ignore everything before this", "0"), opt("min", "shortest allowed width, ms", "none").fill(DefaultFrom::Measured(-10.0)), opt("max", "longest allowed width, ms", "none").fill(DefaultFrom::Measured(10.0)), NAME],
        needs: &[],
        example: "[[assert]]\nkind = \"pulse_width\"\nnet = \"D13\"\nlevel = \"high\"\nmin = 10.15\nmax = 11.44\n",
        fails_because: "the pulse is wider or narrower than the bound, or no complete pulse fit in the run; check the timing component or lengthen duration_ms." },
    Entry { kind: "logic_level", title: "A net reaches a valid logic level", summary: "A net reaches a valid high (or low) at every input it drives; the level is read from those parts' own cards.",
        required: &[req("net", "the net to watch")],
        optional: &[opt("level", "high or low", "high"), opt("min_width_ms", "hold the level this long", "no hold"), opt("min_toggles", "in at least this many pulses", "1"), opt("min", "a level in volts to use instead of the cards' VIH", "from the cards").fill(DefaultFrom::CardLogicLevels), opt("max", "the same for a low", "from the cards").fill(DefaultFrom::CardLogicLevels), NAME],
        needs: &[],
        example: "[[assert]]\nkind = \"logic_level\"\nnet = \"D13\"\nmin_width_ms = 0.001\nmin_toggles = 3\n",
        fails_because: "the net never reached a level its inputs accept, or held it too briefly; check the driver, the pull-ups and the supply the receiving parts run from." },
    Entry { kind: "boot_coverage", title: "Firmware drives a control net in time", summary: "A gate, enable, reset or chip-select net that the firmware must actively drive to a level within a deadline of reset.",
        required: &[req("net", "the control net"), req("min", "the level in volts it must reach").fill(DefaultFrom::Measured(-10.0)), req("deadline_ms", "by this long after reset")],
        optional: &[opt("hold_ms", "how long the level must hold", "through the deadline"), NAME],
        needs: &[Need::Firmware],
        example: "[[assert]]\nkind = \"boot_coverage\"\nnet = \"D13\"\nmin = 3.0\ndeadline_ms = 20.0\n",
        fails_because: "the firmware never drove the control net in time; check `firmware = ...` points at the right image and the net is a GPIO the firmware actually drives." },
    Entry { kind: "protection_trip", title: "A supply's protection trips (or not)", summary: "A battery or e-fuse protection cutoff fired, or must not have fired, within a scenario window.",
        required: &[req("supply_net", "the supply's net"), req("expect_trip", "true if it should trip, false if it must not")],
        optional: &[opt("scenario", "limit to one [[scenario]] window", "the whole run"), NAME],
        needs: &[Need::Supply],
        example: "[[supply]]\nnet = \"+5V\"\nkind = \"battery\"\nchemistry = \"liion\"\ncells = 1\ncapacity_mah = 400\nprotection_trip_a = 1.0\nprotection_delay_ms = 2.0\n\n[[assert]]\nkind = \"protection_trip\"\nsupply_net = \"+5V\"\nexpect_trip = true\n",
        fails_because: "the supply's protection did/did not latch as asserted; check the supply's limits and the load that triggers it." },
    Entry { kind: "peripheral", title: "An attached device ends in a state", summary: "A bus device, sensor or waveform sink's end-of-run state, read by id and field (or bytes).",
        required: &[req("id", "the [[peripheral]] or [[sensor]] id"), req("field", "the field to read, with min and/or max (or give bytes)")],
        optional: &[opt("bytes", "expected memory contents, instead of field", "none"), opt("min", "lowest allowed value", "none"), opt("max", "highest allowed value", "none"), NAME],
        needs: &[Need::Peripheral],
        example: "[[peripheral]]\nid = \"VCD\"\ntype = \"vcd_sink\"\nnets = [\"D13\"]\n\n[[assert]]\nkind = \"peripheral\"\nid = \"VCD\"\nfield = \"transitions\"\nmin = 15\nmax = 25\n",
        fails_because: "the device's end state is outside the bound; check what the firmware sent it and that the bus is wired to the right pins." },
    Entry { kind: "phase_margin", title: "A loop has enough phase margin", summary: "The feedback loop's phase margin, in degrees, at the net you name, from a small-signal [ac] sweep.",
        required: &[req("net", "the loop break or output net"), req("min", "lowest margin in degrees, e.g. 45 (or max)")],
        optional: &[opt("max", "highest margin in degrees", "none"), NAME],
        needs: &[Need::Ac],
        example: "[ac]\nfstart = 10\nfstop = 1e6\npoints = 20\n\n[[assert]]\nkind = \"phase_margin\"\nnet = \"+5V\"\nmin = 45\n",
        fails_because: "the loop's phase margin missed the bound; check the compensation network and the `[ac]` sweep range." },
    Entry { kind: "ac_gain", title: "Gain at a frequency is in a band", summary: "A net's small-signal gain, in dB, at a frequency, from the [ac] sweep.",
        required: &[req("net", "the net to read"), req("min", "lowest gain in dB (or max)")],
        optional: &[opt("max", "highest gain in dB", "none"), opt("freq_hz", "the frequency to read at", "the sweep's reference point"), NAME],
        needs: &[Need::Ac],
        example: "[ac]\nfstart = 10\nfstop = 1e6\npoints = 20\n\n[[assert]]\nkind = \"ac_gain\"\nnet = \"+5V\"\nfreq_hz = 1000\nmin = -3\n",
        fails_because: "the small-signal gain missed the band; check the AC stimulus net and the sweep points." },
    Entry { kind: "model_coverage", title: "Enough of the board is modelled", summary: "How much of the board bound to a real device model, so a part that loses its model fails the build instead of simulating a hole.",
        required: &[req("min_critical", "fraction of active ICs that must bind, 0..1 (or min_resolved / max_active_unresolved)")],
        optional: &[opt("min_resolved", "fraction of all parts that must bind", "none"), opt("max_active_unresolved", "most unresolved parts allowed on connected nets", "none"), NAME],
        needs: &[],
        example: "[[assert]]\nkind = \"model_coverage\"\nmin_critical = 1.0\nmax_active_unresolved = 0\n",
        fails_because: "an active part has no model, so its answer is not simulated; the failure names the parts, write or extract a model for each." },
    Entry { kind: "hwtrace", title: "Simulation matches a captured trace", summary: "The simulated waveform's features (period, rise time, overshoot) agree with a scope trace you captured on the real board.",
        required: &[req("trace", "path to the trace.toml, relative to the spec")],
        optional: &[NAME],
        needs: &[Need::Trace],
        example: "[[assert]]\nkind = \"hwtrace\"\ntrace = \"trace.toml\"\n",
        fails_because: "the simulated waveform disagrees with the capture beyond tolerance; check the part values on that net and that the capture was taken at the same load." },
    Entry { kind: "fires", title: "A comparator trips at a level", summary: "A comparator's output rises (fires) as many times as you expect, optionally first tripping at a given input level.",
        required: &[req("ref", "the comparator, e.g. U1")],
        optional: &[opt("target", "the input level in volts it must first trip at", "any"), opt("tolerance", "fraction of target, in (0, 1]", "0.05"), opt("min_toggles", "how many times it must fire", "1"), opt("reset_below", "the input must fall below this after each firing", "none"), NAME],
        needs: &[],
        example: "[[assert]]\nkind = \"fires\"\nref = \"U1\"\ntarget = 0.387\ntolerance = 0.1\n",
        fails_because: "the comparator tripped at a different level, too often or not at all; the report ranks the parts that set the trip point and solves a repair value." },
    Entry { kind: "increases", title: "A quantity rises as you sweep a net", summary: "A comparator's firing rate (or a net's average level) must rise each step along one swept net; a flat step fails too.",
        required: &[req("along", "a [[search.sweep]] net with two or more values"), req("ref", "the comparator, or give net (exactly one of the two)")],
        optional: &[opt("net", "a net whose mean level is judged, instead of ref", "none"), opt("after_ms", "ignore everything before this", "0"), NAME],
        needs: &[Need::Search],
        example: "[search]\n[[search.sweep]]\nnet = \"+5V\"\nvolts = [4.5, 5.0]\n\n[[assert]]\nkind = \"increases\"\nref = \"U1\"\nalong = \"+5V\"\nafter_ms = 0.05\n",
        fails_because: "the response is flat or inverted between two neighbouring sweep points, which is a topology finding rather than a part tolerance; the report names the pair." },
    Entry { kind: "decreases", title: "A quantity falls as you sweep a net", summary: "The mirror of increases: a comparator's firing rate (or a net's average level) must fall each step along one swept net.",
        required: &[req("along", "a [[search.sweep]] net with two or more values"), req("ref", "the comparator, or give net (exactly one of the two)")],
        optional: &[opt("net", "a net whose mean level is judged, instead of ref", "none"), opt("after_ms", "ignore everything before this", "0"), NAME],
        needs: &[Need::Search],
        example: "[search]\n[[search.sweep]]\nnet = \"+5V\"\nvolts = [4.5, 5.0]\n\n[[assert]]\nkind = \"decreases\"\nref = \"U1\"\nalong = \"+5V\"\nafter_ms = 0.05\n",
        fails_because: "the response is flat or rising between two neighbouring sweep points, which is a topology finding rather than a part tolerance; the report names the pair." },
    Entry { kind: "bus", title: "A bus device answers", summary: "The firmware talks to an I2C address (it ACKs, at least a number of times) or sends an SPI frame containing given bytes.",
        required: &[req("protocol", "i2c or spi")],
        optional: &[opt("addr", "the 7-bit I2C address, 0..127", "none").fill(DefaultFrom::Bench("bus_addr")), opt("acked", "true if the device must ACK, false if it must not", "true"), opt("bytes", "SPI: hex bytes the frame must contain", "none").fill(DefaultFrom::Bench("bus_bytes")), opt("min_transactions", "at least this many transactions", "1"), opt("bus", "which bus, e.g. I2C1", "any bus"), opt("mcu", "which MCU's bus to read", "all MCUs"), NAME],
        needs: &[Need::Firmware, Need::Bench],
        example: "[[assert]]\nkind = \"bus\"\nprotocol = \"i2c\"\naddr = 0x48\nmin_transactions = 2\n",
        fails_because: "no transaction like it was seen on the bus; check the address, the pull-ups on SDA and SCL, and that the firmware initialises the peripheral." },
    Entry { kind: "resets", title: "The MCU does not keep resetting", summary: "The MCU resets at most a number of times during the run, optionally counting only one cause (a watchdog, a brown-out).",
        required: &[req("max_resets", "the most resets allowed (0 for a board that must never reset)").fill(DefaultFrom::Bench("reset_count"))],
        optional: &[opt("cause", "count only this cause: power_on, brown_out, watchdog, software or pin", "every cause"), opt("mcu", "which MCU", "all MCUs"), NAME],
        needs: &[Need::Firmware, Need::Bench],
        example: "[[assert]]\nkind = \"resets\"\nmax_resets = 0\ncause = \"brown_out\"\n",
        fails_because: "the MCU reset more often than allowed; the cause says why (a sagging rail for brown_out, a missed refresh for watchdog)." },
    Entry { kind: "reaches", title: "Firmware reaches a function", summary: "The firmware executes a named function (within a deadline), or is not parked in one at the end of the run.",
        required: &[req("symbol", "the function or label the firmware must reach (or give not_parked_in)").fill(DefaultFrom::Bench("entry_symbol"))],
        optional: &[opt("not_parked_in", "instead of symbol: the firmware must not end the run inside this function", "none").fill(DefaultFrom::Bench("park_symbol")), opt("deadline_ms", "must reach it by this long after reset", "the whole run"), opt("mcu", "which MCU", "all MCUs"), NAME],
        needs: &[Need::Firmware],
        example: "[[assert]]\nkind = \"reaches\"\nsymbol = \"main\"\n",
        fails_because: "the program counter never got to the symbol; check the boot path (clock, reset line, boot pins) and that the image is the right one." },
    Entry { kind: "uart_baud", title: "The UART runs at a baud rate", summary: "The serial port the firmware set up runs at the expected baud rate, within a tolerance.",
        required: &[req("baud", "the expected baud rate").fill(DefaultFrom::Bench("uart_baud"))],
        optional: &[opt("tolerance", "fraction of baud, in (0, 1]", "0.03").fill(DefaultFrom::Constant(0.03)), opt("mcu", "which MCU's UART", "all MCUs"), NAME],
        needs: &[Need::Firmware, Need::Bench],
        example: "[[assert]]\nkind = \"uart_baud\"\nbaud = 115200\n",
        fails_because: "the UART divisor gives a different rate than expected, usually a wrong clock tree: the baud is derived from the peripheral clock, so check the PLL and the crystal." },
];

/// The entry for `kind`, folding the `boot-coverage` alias onto `boot_coverage`.
#[must_use]
pub fn entry(kind: &str) -> Option<&'static Entry> {
    let kind = if kind == "boot-coverage" {
        "boot_coverage"
    } else {
        kind
    };
    CATALOG.iter().find(|e| e.kind == kind)
}

/// The `type` the loader's own JSON schema gives an `Assertion` property
/// (`number`, `string`, `boolean`, `integer`), so the catalogue cannot disagree with it.
fn field_type(name: &str) -> &'static str {
    static TYPES: OnceLock<Value> = OnceLock::new();
    let schema = TYPES.get_or_init(|| {
        let root = SchemaSettings::draft07()
            .into_generator()
            .into_root_schema_for::<Assertion>();
        serde_json::to_value(root).unwrap_or(Value::Null)
    });
    let ty = &schema["properties"][name]["type"];
    let ty = ty.as_array().map_or(ty.as_str(), |all| {
        all.iter().filter_map(Value::as_str).find(|t| *t != "null")
    });
    match ty {
        Some("number") => "number",
        Some("integer") => "integer",
        Some("boolean") => "boolean",
        _ => "string",
    }
}

/// The unit a field's number is in, when it has one.
fn unit(kind: &str, name: &str) -> Option<&'static str> {
    match name {
        "amps" => Some("A"),
        "celsius" => Some("C"),
        "freq_hz" => Some("Hz"),
        "baud" => Some("baud"),
        "dip_below" | "recover_to" | "spike_above" | "settle_to" | "reset_below" | "target" => {
            Some("V")
        }
        n if n.ends_with("_ms") => Some("ms"),
        "min" | "max" => match kind {
            "voltage" | "rail_window" | "logic_level" | "boot_coverage" => Some("V"),
            "pulse_width" => Some("ms"),
            "phase_margin" => Some("deg"),
            "ac_gain" => Some("dB"),
            _ => None,
        },
        _ => None,
    }
}

/// `after_ms` becomes `After ms`; `ref` reads as `Part`.
fn label(name: &str) -> String {
    let words = match name {
        "ref" => "part".to_string(),
        other => other.replace('_', " "),
    };
    let mut chars = words.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

fn fields_json(kind: &str, fields: &[Field], required: bool) -> Vec<Value> {
    fields
        .iter()
        .map(|f| {
            json!({
                // v1 keys.
                "name": f.name, "what": f.what, "default": f.default,
                // v2 keys.
                "key": f.name, "label": label(f.name), "unit": unit(kind, f.name),
                "type": field_type(f.name), "required": required, "help": f.what,
                "if_absent": (!f.default.is_empty()).then_some(f.default),
                "default_from": f.from.map(DefaultFrom::to_json),
            })
        })
        .collect()
}

fn entry_json(e: &Entry) -> Value {
    let mut offer = serde_json::Map::new();
    let mut targets = Vec::new();
    for (target, words) in offers_for(e.kind) {
        targets.push(target.name());
        offer.insert(target.name().to_string(), json!(words));
    }
    let fields: Vec<Value> = fields_json(e.kind, e.required, true)
        .into_iter()
        .chain(fields_json(e.kind, e.optional, false))
        .collect();
    json!({
        "kind": e.kind, "title": e.title, "summary": e.summary,
        "required": fields_json(e.kind, e.required, true),
        "optional": fields_json(e.kind, e.optional, false),
        "needs": e.needs.iter().map(|n| n.copy().1).collect::<Vec<_>>(),
        "example": e.example, "fails_because": e.fails_because,
        "targets": targets, "offer": offer, "fields": fields,
        "prerequisites": e.needs.iter().map(|n| {
            let (token, spelled, fix) = n.copy();
            // `action` is additive: the id and button words the web app uses
            // instead of the spec text in `fix`; absent when it has no flow.
            json!({ "need": token, "label": spelled, "fix": fix, "action": n.action() })
        }).collect::<Vec<_>>(),
    })
}

/// Read the text of one `key=value` the command line gave as the TOML value
/// `key` takes in a `kind` assertion.
///
/// The catalogue is the only place that knows which fields a kind has and what
/// type each one is, so `hauksbee-ci add` asks here instead of keeping a second
/// table: a number is a number, a count is a whole number (`0x48` is accepted,
/// since an I2C address is written that way), a flag is `true` or `false`, and
/// everything else is text.
///
/// # Errors
///
/// A sentence naming the kind's real fields (with a did-you-mean) when `key`
/// is not one of them, or saying what the field wants when `raw` is not that.
pub fn parse_field_value(kind: &str, key: &str, raw: &str) -> Result<toml::Value, String> {
    let e = entry(kind).ok_or_else(|| format!("unknown assertion kind '{kind}'"))?;
    let names: Vec<&str> = e.fields().map(|f| f.name).collect();
    if !names.contains(&key) {
        return Err(format!(
            "a `{kind}` assertion has no field `{key}`{}; its fields: {}",
            crate::error::did_you_mean_hint(key, &names),
            names.join(", ")
        ));
    }
    let wants = |what: &str| format!("`{key}` wants {what}, got `{raw}`");
    match field_type(key) {
        "number" => raw
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .map(toml::Value::Float)
            .ok_or_else(|| wants("a number")),
        "integer" => {
            let parsed = raw.strip_prefix("0x").map_or_else(
                || raw.parse::<i64>().ok(),
                |hex| i64::from_str_radix(hex, 16).ok(),
            );
            parsed
                .map(toml::Value::Integer)
                .ok_or_else(|| wants("a whole number"))
        }
        "boolean" => match raw {
            "true" => Ok(toml::Value::Boolean(true)),
            "false" => Ok(toml::Value::Boolean(false)),
            _ => Err(wants("true or false")),
        },
        _ => Ok(toml::Value::String(raw.to_string())),
    }
}

/// The fields a reading of `value` fills in for `kind`.
///
/// These follow the catalogue's own `default_from` rules: a field that is the
/// measured value plus or minus a margin, and the margin a rate is judged
/// within. The app resolves the same rules against its live reading, so "take
/// it from the run" gives the same numbers on every surface.
#[must_use]
pub fn measured_fields(kind: &str, value: f64) -> Vec<(&'static str, toml::Value)> {
    let round4 = |v: f64| format!("{v:.3e}").parse::<f64>().unwrap_or(v);
    entry(kind).map_or_else(Vec::new, |e| {
        e.fields()
            .filter_map(|f| match f.from? {
                DefaultFrom::Measured(pct) => Some((
                    f.name,
                    toml::Value::Float(round4(value * (1.0 + pct / 100.0))),
                )),
                DefaultFrom::Constant(c) => Some((f.name, toml::Value::Float(c))),
                _ => None,
            })
            .collect()
    })
}

/// The whole catalogue as the JSON the server and the app read.
#[must_use]
pub fn to_json() -> Value {
    let kinds: Vec<Value> = CATALOG.iter().map(entry_json).collect();
    json!({ "ok": true, "version": 2, "kinds": kinds })
}

/// The editor's JSON Schema for a spec, carrying the catalogue's words.
///
/// Every assertion property gets `x-hauksbee-doc` and the root gets
/// `x-hauksbee-glossary`, so hover help in an editor is the same prose as the CLI's.
#[must_use]
pub fn editor_schema() -> Value {
    let root = SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<Spec>();
    let mut schema = serde_json::to_value(root).unwrap_or(Value::Null);
    if let Some(props) = schema
        .pointer_mut("/definitions/Assertion/properties")
        .and_then(Value::as_object_mut)
    {
        for (name, prop) in props.iter_mut() {
            let mut by_kind = serde_json::Map::new();
            for e in CATALOG {
                let doc = if name == "kind" {
                    json!({ "title": e.title, "summary": e.summary, "fails_because": e.fails_because, "example": e.example })
                } else if let Some((f, required)) = e
                    .required
                    .iter()
                    .map(|f| (f, true))
                    .chain(e.optional.iter().map(|f| (f, false)))
                    .find(|(f, _)| f.name == name)
                {
                    json!({ "what": f.what, "default": f.default, "required": required, "unit": unit(e.kind, name) })
                } else {
                    continue;
                };
                by_kind.insert(e.kind.to_string(), doc);
            }
            if let Some(prop) = prop.as_object_mut() {
                prop.insert("x-hauksbee-doc".into(), json!({ "by_kind": by_kind }));
            }
        }
    }
    schema["x-hauksbee-glossary"] =
        serde_json::to_value(hauksbee_ir::glossary::all()).unwrap_or(Value::Null);
    schema
}

/// The one-line-per-kind list `hauksbee-ci explain` prints.
#[must_use]
pub fn render_list() -> String {
    let mut out =
        String::from("assertion kinds (hauksbee-ci explain <kind> for fields and an example):\n");
    for e in CATALOG {
        let _ = writeln!(out, "  {:<16} {}", e.kind, e.title);
    }
    out
}

/// One entry, in full: summary, fields, what else the spec needs, an example, why it fails.
#[must_use]
pub fn render_entry(e: &Entry) -> String {
    let mut out = format!("{}: {}\n{}\n", e.kind, e.title, e.summary);
    let section = |out: &mut String, head: &str, fields: &[Field]| {
        if fields.is_empty() {
            return;
        }
        let _ = writeln!(out, "\n{head}:");
        for f in fields {
            let default = if f.default.is_empty() {
                String::new()
            } else {
                format!(" (default: {})", f.default)
            };
            let _ = writeln!(out, "  {:<18} {}{default}", f.name, f.what);
        }
    };
    section(&mut out, "required", e.required);
    section(&mut out, "optional", e.optional);
    if !e.needs.is_empty() {
        let needs: Vec<&str> = e.needs.iter().map(|n| n.copy().1).collect();
        let _ = writeln!(out, "\nalso needs: {}", needs.join(", "));
    }
    out.push_str("\nexample:\n");
    for line in e.example.lines() {
        let _ = writeln!(out, "  {line}");
    }
    let _ = writeln!(out, "\nwhen it fails: {}", e.fails_because);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::{check_spec, CheckOptions};
    use crate::spec::ASSERTION_KINDS;

    /// The table and the loader's closed vocabulary list exactly the same kinds
    /// (one entry each), and the loader's own `match` knows every one of them (so
    /// the list itself cannot drift from the code that enforces it).
    #[test]
    fn the_table_and_the_loader_agree_on_every_kind() {
        let mut table: Vec<&str> = CATALOG.iter().map(|e| e.kind).collect();
        let mut loader: Vec<&str> = ASSERTION_KINDS.to_vec();
        table.sort_unstable();
        loader.sort_unstable();
        assert_eq!(table, loader);
        assert_eq!(loader.len(), 23, "a new kind needs a catalogue entry");
        for e in CATALOG {
            let a = Assertion {
                kind: e.kind.to_string(),
                ..Default::default()
            };
            let msg = a
                .validate()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(!msg.contains("unknown assertion kind"), "{}: {msg}", e.kind);
        }
    }

    /// Every field the catalogue names is a real `Assertion` field, and every kind
    /// is offered somewhere and its prerequisites carry a fix.
    #[test]
    fn every_catalogue_field_is_a_real_assertion_field() {
        let real = crate::spec::struct_fields::<Assertion>();
        for e in CATALOG {
            for f in e.fields() {
                assert!(
                    real.iter().any(|r| r == f.name),
                    "{}: `{}` is not an Assertion field",
                    e.kind,
                    f.name
                );
            }
            assert!(offers_for(e.kind).next().is_some(), "{}: no offer", e.kind);
            for n in e.needs {
                assert!(!n.copy().2.is_empty());
            }
        }
        assert!(OFFERS.iter().all(|(k, _, _)| entry(k).is_some()));
    }

    /// Every example is a spec the validator accepts, and its required fields are
    /// the ones it actually sets.
    #[test]
    fn every_example_passes_spec_validation() {
        let dir = tempfile::tempdir().unwrap();
        for e in CATALOG {
            // The firmware key is the one thing `needs` names that lives outside the example.
            let firmware = if e.needs.contains(&Need::Firmware) {
                "firmware = \"fw.hex\"\n"
            } else {
                ""
            };
            let spec = format!(
                "board = \"b.kicad_pcb\"\n{firmware}duration_ms = 100\n\n{}",
                e.example
            );
            let path = dir.path().join(format!("{}.toml", e.kind));
            std::fs::write(&path, &spec).unwrap();
            let diags = check_spec(
                &path,
                &CheckOptions {
                    no_board: true,
                    ..Default::default()
                },
            );
            assert!(
                diags.is_empty(),
                "{}: {:?}\n{spec}",
                e.kind,
                diags.iter().map(|d| &d.message).collect::<Vec<_>>()
            );
            for f in e.required {
                assert!(
                    e.example.contains(&format!("{} = ", f.name)),
                    "{}: example omits required `{}`",
                    e.kind,
                    f.name
                );
            }
            assert!(!e.fails_because.is_empty() && !e.title.is_empty());
        }
    }

    #[test]
    fn explain_prints_required_fields_and_an_example() {
        let text = render_entry(entry("rail_window").unwrap());
        assert!(
            text.contains("required:")
                && text.contains("scenario")
                && text.contains("kind = \"rail_window\""),
            "{text}"
        );
        assert!(entry("boot-coverage").is_some() && entry("nope").is_none());
        assert_eq!(to_json()["kinds"].as_array().unwrap().len(), CATALOG.len());
    }

    /// An offer is shown to the user as written: a placeholder for a value
    /// ("... V") reads as a sentence cut short, so the offer names what is set
    /// and the editor that opens asks for the number.
    #[test]
    fn no_offer_holds_a_placeholder_for_a_value() {
        for (kind, _, words) in OFFERS {
            for placeholder in ["...", "\u{2026}"] {
                assert!(!words.contains(placeholder), "{kind}: {words:?}");
            }
            assert!(
                words
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_uppercase() || c == '{'),
                "{kind}: {words:?} does not read as a sentence"
            );
        }
        let wording = |kind| offers_for(kind).next().map(|(_, w)| w);
        assert_eq!(
            wording("voltage"),
            Some("Keep {net} within a voltage range")
        );
        assert_eq!(
            wording("max_current"),
            Some("Set a current limit for {ref}")
        );
        assert_eq!(
            wording("max_temp"),
            Some("Set a temperature limit for {ref}")
        );
        assert_eq!(
            wording("toggle"),
            Some("{net} toggles at the expected rate")
        );
    }

    /// The v2 keys a click-to-assert UI reads, next to the v1 keys it replaces.
    #[test]
    fn the_json_carries_offers_typed_fields_and_prerequisites() {
        let all = to_json();
        assert_eq!(all["version"], 2);
        let v = all["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["kind"] == "voltage")
            .unwrap();
        assert_eq!(v["targets"], json!(["net", "trace"]));
        assert!(v["offer"]["net"].as_str().unwrap().contains("{net}"));
        let min = v["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == "min")
            .unwrap();
        assert_eq!(min["type"], "number");
        assert_eq!(min["unit"], "V");
        assert_eq!(min["required"], true);
        assert_eq!(min["default_from"]["source"], "measured");
        assert_eq!(
            v["summary"].as_str(),
            Some(entry("voltage").unwrap().summary)
        );
        let t = all["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["kind"] == "phase_margin")
            .unwrap();
        assert_eq!(t["needs"], json!(["[ac]"]));
        assert_eq!(t["prerequisites"][0]["need"], "ac");
        // The web app gets a button id beside the spec text.
        assert_eq!(t["prerequisites"][0]["action"]["id"], "add_ac_sweep");
        assert!(t["prerequisites"][0]["action"]["label"].is_string());
    }

    /// A need with a guided flow carries its action; one without says null, so
    /// the app knows to show the spec text instead of an empty button.
    #[test]
    fn needs_offer_an_action_only_where_the_app_has_a_flow() {
        let with = [Need::Firmware, Need::Ac, Need::Scenario, Need::Supply];
        let without = [Need::Search, Need::Trace, Need::Peripheral, Need::Bench];
        assert!(with.iter().all(|n| n.action().is_some()));
        assert!(without.iter().all(|n| n.action().is_none()));
    }

    /// The four observation kinds are in the table, offered from the surface a
    /// person sees the observation on, and name the bench need only where a run
    /// cannot yet judge them.
    #[test]
    fn the_observation_kinds_are_offered_where_the_observation_is_seen() {
        let targets =
            |k: &str| -> Vec<&'static str> { offers_for(k).map(|(t, _)| t.name()).collect() };
        assert!(targets("toggle").contains(&"trace"));
        assert!(targets("voltage").contains(&"trace"));
        assert!(targets("pulse_width").contains(&"trace"));
        assert_eq!(targets("bus"), ["bus"]);
        assert_eq!(targets("resets"), ["bench"]);
        assert_eq!(targets("reaches"), ["bench"]);
        assert!(targets("uart_baud").contains(&"uart"));
        for kind in ["bus", "resets", "uart_baud"] {
            assert!(entry(kind).unwrap().needs.contains(&Need::Bench), "{kind}");
        }
        // A reach of the entry symbol is judged from the execution evidence a run records today.
        assert!(!entry("reaches").unwrap().needs.contains(&Need::Bench));
        let all = to_json();
        let baud = all["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["kind"] == "uart_baud")
            .unwrap();
        assert_eq!(baud["fields"][0]["default_from"]["source"], "bench");
        assert_eq!(baud["fields"][0]["unit"], "baud");
        assert_eq!(baud["fields"][0]["type"], "integer");
        assert_eq!(baud["prerequisites"][1]["need"], "bench");
    }

    #[test]
    fn the_editor_schema_documents_each_property_per_kind() {
        let s = editor_schema();
        let kind = &s["definitions"]["Assertion"]["properties"]["kind"];
        assert!(kind["x-hauksbee-doc"]["by_kind"]["voltage"]["summary"].is_string());
        let min = &s["definitions"]["Assertion"]["properties"]["min"];
        assert_eq!(min["x-hauksbee-doc"]["by_kind"]["ac_gain"]["unit"], "dB");
        assert!(s["x-hauksbee-glossary"]
            .as_array()
            .is_some_and(|g| !g.is_empty()));
    }
}
