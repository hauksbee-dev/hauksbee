//! Connectivity lint: design-rule checks that read the netlist graph (nets,
//! components, pin functions) rather than copper geometry. These are the
//! "lint-class" checks that catch wiring intent errors a value/short sweep
//! misses: missing I2C pull-ups, floating active-control pins (enable / reset /
//! chip-select left on a single-pin net), and LED series-resistor current
//! sanity.
//!
//! Design priority is **low false-positive rate**. Every check is conservative:
//! it fires only when the structural evidence is unambiguous, and it carries the
//! exact nets / refs / pins so a finding can be chased to the source. Decoupling
//! is not judged here per IC (too noisy); the engine layer's
//! `rail_decoupling` check reports it as a per-rail ratio note.
//!
//! Works on any extraction path. Pin-function-aware checks (control-pin
//! floating) need `Pin.function` populated, which the KiCad netlist, newer
//! `.kicad_pcb`, and schematic paths provide; net-name-based checks (I2C
//! pull-ups, LED current) work even on pin-function-less inputs.

use crate::assembly::AssemblyState;
use crate::part_class::{self, connected_pads, ref_designator};
use crate::{Component, ExtractedBoard, Net, Pin};
use hauksbee_ir::pin_functions::{dedicated_i2c_pin, names_spi_line, PeripheralFunction};
use hauksbee_models::refdes::RefClass;
use std::fmt::Write as _;

mod polarity;
mod topology;

/// One lint finding, severity-tagged, with the evidence to reproduce it.
#[derive(Debug, Clone)]
pub struct LintFinding {
    pub check: LintCheck,
    pub severity: Severity,
    /// Human-readable one-line description.
    pub message: String,
    /// Reference designators implicated (e.g. the IC with the floating EN, or
    /// the resistor + LED of an over-driven indicator).
    pub refs: Vec<String>,
    /// Net(s) implicated by name.
    pub nets: Vec<String>,
}

/// The word every low-severity abstention message carries.
///
/// A check that declined to judge says so (`... abstained: ...`), and
/// [`LintFinding::is_abstention`] keys on it for the checks whose abstentions
/// share a kind with real findings.
pub const ABSTENTION_MARKER: &str = " abstained";

/// How the layout lint's unfilled-zone note opens. That note says connectivity
/// through a pour was not checked, so it is an abstention too.
pub const ZONES_NOT_FILLED_PREFIX: &str = "zones not filled";

/// The word a low-severity evidence note carries: a decoded value or computed
/// band reported for audit, with no verdict of its own. Any verdict drawn from
/// it is emitted as a separate finding.
pub const INFORMATIONAL_MARKER: &str = " (informational)";

/// Opens a part-identity finding that names a different value or part number.
///
/// Nothing else differs for the reference. It is a non-gating item
/// to look at before ordering: the run used the layout's part, the one that
/// gets built. Footprint or pin-map differences never carry it.
pub const CHECK_BEFORE_ORDERING: &str = "Check before ordering: ";

/// Opens the closing sentence of a finding that computed its own remedy.
///
/// The remedy is a resistance bound or a feed to use. Everything after the
/// marker is that remedy, and every surface's "what to do" and the verdict's
/// next step use it in place of the check kind's generic advice
/// ([`LintFinding::own_remedy`]).
pub const REMEDY_MARKER: &str = "Remedy: ";

impl LintFinding {
    /// The remedy this finding worked out for itself, when its message closes
    /// with one ([`REMEDY_MARKER`]); `None` leaves the check kind's advice.
    #[must_use]
    pub fn own_remedy(&self) -> Option<&str> {
        let (_, remedy) = self.message.rsplit_once(REMEDY_MARKER)?;
        Some(remedy.trim()).filter(|r| !r.is_empty())
    }

    /// Whether this finding asks the reader to do something: not an abstention
    /// and not a low-severity informational evidence note.
    #[must_use]
    pub fn is_actionable(&self) -> bool {
        let evidence_note =
            self.severity == Severity::Low && self.message.contains(INFORMATIONAL_MARKER);
        !(self.is_abstention() || evidence_note)
    }

    /// Whether this finding records what a check did NOT judge rather than a
    /// flaw it found: an MCU with no model, a strap divider it cannot evaluate,
    /// an operating envelope / decode whose input level is unknown, a rating
    /// whose current path runs into a part with no working model, or a
    /// pour with no saved fill that connectivity cannot be read through.
    ///
    /// An abstention tells the reader where coverage ends and must stay visible,
    /// but it is not something to act on: a clean board that merely lacks a
    /// declared rail level or a model must not read as having findings. The
    /// run-level coverage verdict (`invalid`) is what carries the missing
    /// coverage; this flag keeps the note out of the actionable tally.
    #[must_use]
    pub fn is_abstention(&self) -> bool {
        match self.check {
            LintCheck::UncheckedMcu => true,
            LintCheck::StrapPin => self.severity == Severity::Low,
            LintCheck::UnroutedPad => {
                self.severity == Severity::Low && self.message.starts_with(ZONES_NOT_FILLED_PREFIX)
            }
            LintCheck::OperatingEnvelope
            | LintCheck::DeviceDecode
            | LintCheck::DcOverstress
            | LintCheck::RequiredPinBias
            | LintCheck::SupplyCapacitance
            | LintCheck::HoldUpDroop
            | LintCheck::PullupIneffective => {
                self.severity == Severity::Low && self.message.contains(ABSTENTION_MARKER)
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintCheck {
    /// An I2C bus net (SDA/SCL) with no resistor to a power rail.
    MissingI2cPullup,
    /// An SD / microSD card bus net (CMD / DAT0..DAT3) with no resistor to a
    /// power rail. The SD physical-layer spec requires host-side pull-ups: CMD
    /// is driven open-drain during card identification and DAT0 carries the
    /// card's busy signalling, so a floating line hangs initialization in a
    /// way firmware cannot see. CMD/DAT0 report Medium; DAT1..DAT3 (required
    /// by the spec but conventionally omitted in 1-bit-only designs) report
    /// Low.
    MissingSdPullup,
    /// An active-control pin (enable / reset / chip-select / output-enable) on a
    /// net with no other connection - genuinely floating, no defined level.
    FloatingControlPin,
    /// An LED + series resistor whose computed forward current is outside the
    /// sane indicator band.
    LedCurrentSanity,
    /// Two push-pull output (or power-output) pins of different parts tied
    /// directly on one net with nothing to resolve the contention - both can
    /// drive the net to opposite levels and fight. A schematic-stage ERC check
    /// built from extracted pin electrical types.
    OutputContention,
    /// An MCU boot strapping pin (per the model db's per-part strap table) whose
    /// net cannot hold the level the part needs at the reset latch window: a
    /// free-running clock source on it, a pull to the wrong rail, or no defined
    /// level where one is required. Produced by the engine-layer strap lint.
    StrapPin,
    /// Two board-level functions are routed to different MCU pins, but those
    /// pins map to the same shared silicon resource instance inside the MCU (a
    /// PWM slice/channel, a QSPI pin group), so the MCU cannot serve both at
    /// once. Produced by `resource_conflict.rs`, reported through this same
    /// `NetLintReport` shape.
    McuResourceConflict,
    /// A passive reference designator disagrees with the passive footprint
    /// family, e.g. R1 on `Capacitor_SMD:C_0603`.
    DesignatorFootprintMismatch,
    /// A passive value is physically implausible for the stated package.
    ValuePackageSanity,
    /// A passive still has a placeholder / unset value (`R`, `C`, `L`, `?`,
    /// empty), so downstream physics cannot know the actual part.
    PlaceholderValue,
    /// A component that looks like an MCU (by part-number family) resolved to no
    /// device model, so the MCU-model-dependent checks; the boot strap-pin lint
    /// and the internal resource-conflict check, could not run on it. Without
    /// this note those checks are silently empty, which a bare "Looks healthy"
    /// verdict would misread as "checked and clean". Informational, never a
    /// `--strict` failure: an unmodelled part is a coverage gap, not a defect.
    UncheckedMcu,
    /// A configurable controller whose strap / divider resistors select a
    /// documented operating mode decodes (against the part's datasheet bands) to
    /// the WRONG mode, or to an internally-inconsistent configuration. Per-part,
    /// grows incrementally; seeded with the CYPD3177 USB-C PD sink. Produced by
    /// the engine-layer `device_decode` check.
    DeviceDecode,
    /// A model card's sourced recommended operating envelope is violated by
    /// the full derived DC range of the connected rail.
    OperatingEnvelope,
    /// A part's signal pin sits in a HIGHER voltage domain than the part's own
    /// supply rail: the pin's net is tied or pulled up to a rail above the
    /// supply, so current flows through the pin's protection clamp into the
    /// lower rail (back-powering an unpowered domain at power-down, and
    /// exceeding the VCC+0.5 V input abs-max whenever the pin is not
    /// explicitly higher-voltage-tolerant). Produced by the engine-layer
    /// `back_power` check.
    BackPower,
    /// An I2C bus whose pull-ups are mis-sized for its load: too strong (the
    /// effective pull-up demands more sink current than the I2C spec's 3 mA to
    /// reach VOL, so devices may never read a valid low) or, advisorily, too
    /// weak for the estimated bus capacitance to meet the rise-time budget.
    /// The presence of pull-ups is `MissingI2cPullup`; this is their sizing.
    /// Produced by the engine-layer `bus_loading` check.
    I2cBusLoading,
    /// A schematic-ERC fault read from pin names and pin electrical types where
    /// both are specific: two UART transmit pins driving one net (TX wired to
    /// TX), a `power_in` pin left unconnected, or (a non-gating note) an LED
    /// hung from an open-drain fault flag to ground (it lights when the flag is
    /// idle).
    /// Pins whose role is unknown (passive, generic GPIO) never fire. The
    /// message names the pins, why it matters, and the likely fix.
    PinFunction,
    /// A pad, or group of pads, that copper on the layout never joins to the
    /// rest of its net (KiCad's "unconnected items"). Also carries the
    /// one-line notes for stray track/via copper and for zones saved unfilled.
    UnroutedPad,
    /// A pin wired to a different set of pins on the layout than in its
    /// same-stem schematic: the PCB is out of date with the schematic.
    PcbSchematicMismatch,
    /// One rail set by more than one source: two regulator outputs tied, or
    /// named rails of different voltages joined by a series conductor. The
    /// sources fight, so the rail's voltage and the input currents upstream
    /// of it mean nothing. Produced by the engine-layer `rail_conflicts` check.
    RailConflict,
    /// A supply rail whose ICs are short of local decoupling: far fewer
    /// capacitors to ground on the rail than ICs drawing from it, or no bulk
    /// (>= 1 uF) capacitor on a rail feeding several ICs. A netlist carries no
    /// placement, so this is a ratio, never a proximity claim; it is a
    /// non-gating note. Produced by the engine-layer `rail_decoupling` check.
    MissingDecoupling,
    /// A CMOS logic input (by the part's model card or its schematic pin type)
    /// on no net, or on a net with no driver, pull resistor or other
    /// connection. A floating CMOS input reads random levels and can sit in
    /// its linear region drawing shoot-through current. An explicit
    /// no-connect flag does not excuse it unless the card records an internal
    /// pull on that input. Produced by the engine-layer `floating_inputs`
    /// check.
    FloatingInput,
    /// A part held past one of its card's ratings (a junction's or channel's
    /// current, a driver pin's current, a resistor's wattage, a package's
    /// junction temperature) at the board's solved DC operating point: the
    /// state it powers up into and rests in. Produced by the engine-layer
    /// `dc_stress` check, which runs the co-simulation's stress monitor over
    /// that one solved state.
    DcOverstress,
    /// A ground- or return-named net that nothing on the board joins to the
    /// main ground (no copper, resistor, ferrite, net tie or part spans them),
    /// with parts returning through it. A capacitor is not a join.
    GroundIsland,
    /// A connector whose every signal pin sits on a net touching only that pin,
    /// with at least one of those nets explicitly drawn: nothing reaches it.
    UnwiredConnector,
    /// A two-pin LED held at ground on its anode and at a positive rail on its
    /// cathode (directly or through one resistor each): reverse-biased in every
    /// state, it never lights.
    LedReversed,
    /// One instance of a repeated cell (eight or more identical parts sharing
    /// one dominant wiring signature) wired differently from its twins on two
    /// or more pads: a swapped pair or a mislabelled net in one copy. Produced
    /// by the engine-layer `siblings` check.
    SiblingMismatch,
    /// A BJT whose base is on the same node as a low-impedance driver (a
    /// supply rail, an analog-switch channel to a rail, a logic or MCU output)
    /// while its emitter is on a rail, with no series resistance: the junction
    /// conducts whatever the driver can supply. Produced by the engine-layer
    /// `node_drive` check.
    UnlimitedBaseDrive,
    /// A node whose only connections are capacitor plates and high-impedance
    /// inputs: nothing sets or drains its DC level. Produced by the
    /// engine-layer `node_drive` check.
    CapNoDcPath,
    /// A MOSFET gate or analog-switch select held only by MCU pins, connector
    /// pins and other gates, with no pull resistor: undefined while the MCU is
    /// in reset or the cable is out. Produced by the engine-layer `node_drive`
    /// check.
    FloatingGate,
    /// A MOSFET the board names only by class (no part number) that conducts,
    /// at one of its assumed corners, with its gate driven below the class's
    /// maximum threshold: whether it switches depends on which part is fitted.
    /// Produced by the engine-layer `dc_stress` check.
    GateUnderdrive,
    /// A linear regulator whose card says an open enable pin is off, with that
    /// pin left with nothing on the board that can take it to its on level:
    /// the rail never comes on. Produced by the engine-layer
    /// `regulator_enable` check.
    RegulatorHeldOff,
    /// A carded IC's power pads wired to nothing, or to the wrong side: every
    /// pad of a supply family on no net, every return pad on no net, a supply
    /// pad on ground, or a return pad on a supply rail the part's own supply
    /// does not sit above. Read from the card's pin roles alone
    /// (`hauksbee_models::pin_roles`), so it needs no supply-range row.
    /// Produced by the engine-layer `power_pins` check.
    PowerPinWiring,
    /// A connector pin whose own name says supply (`VBUS`, `VCC`, `+5V`, `VIN`)
    /// wired to a ground net, or a pin named `GND` wired to a positive supply
    /// rail: supply and ground exchanged at the connector. Produced by
    /// `polarity.rs` from pin names and net names alone.
    SupplyGroundSwapped,
    /// A unidirectional diode (TVS, Zener, Schottky, rectifier) between ground
    /// and a rail with its anode on the higher-potential net: forward-biased
    /// across the rail, it clamps the rail to a diode drop. Produced by
    /// `polarity.rs`.
    DiodeReversed,
    /// A polarised capacitor (CP footprint or symbol, `+` pin) with its +
    /// terminal on the lower-potential of two nets whose order the names fix:
    /// reverse-biased whenever the board is powered. Produced by
    /// `polarity.rs`.
    PolarisedCapReversed,
    /// A rail the analysis would otherwise power by its name alone whose only
    /// source is a part the board does not fit (its regulator, or the ferrite
    /// or 0 ohm link to a powered rail), with no connector, test point or
    /// battery to bring it in: it never comes up. Also the non-gating notes on
    /// a rail whose own source is unfitted but may come in from off the board,
    /// and on a run that powered no rail at all. Produced by the engine-layer
    /// `rail_basis` check.
    RailUnpowered,
    /// An active-high enable or active-low reset input of any other part
    /// that the board pulls to its off level with nothing on the net able to
    /// release it: the part is never enabled, or is held in reset for good.
    /// Produced by the engine-layer `control_held_off` check.
    ControlHeldOff,
    /// A part's `Datasheet` field names a file for a different device or logic
    /// family (a link left over from the part it was copied from). A model
    /// drafted from that file would describe the wrong part. Low severity and
    /// non-gating. Produced by the engine-layer `datasheet_field` check.
    DatasheetMismatch,
    /// A passive's fields disagree about the part: the value its manufacturer
    /// part number decodes to is not its Value field's, the MPN's case is not
    /// the footprint's, the footprint and symbol disagree about polarity, or
    /// the MPN's rated voltage is below the rail it sits on. Produced by the
    /// engine-layer `part_fields` check.
    PartFieldMismatch,
    /// A pull-up resistor that is present but cannot do its job: its rail never
    /// comes up, or it leaves the line below the receiver's guaranteed
    /// input-high level (VIH) because the rail is too low or the resistor too
    /// large for the input's leakage. When a limit the check needs is not
    /// stated, it says it abstained. Produced by the engine-layer
    /// `pullup_effect` check.
    PullupIneffective,
    /// A digital-audio bus (I2S, left-justified, TDM) wired against its own
    /// roles: two transmit pins on one data line, or a data line with no
    /// transmitter, a bit clock wired to a frame clock, a clock nothing or two
    /// parts drive, or a master clock a part requires and nothing connects.
    /// Fires only when every non-passive pin on the net has a known audio role.
    /// Produced by the engine-layer `audio_bus` check.
    AudioBusWiring,
    /// A part with every supply off (a rail an MCU-driven switch turned off)
    /// while a signal pin is pulled up to, or driven from, a rail that stays
    /// on: the pin's protection clamp conducts into the unpowered chip.
    /// Produced by the engine-layer `power_states` check.
    PowerDomainBackfeed,
    /// An I2C bus left unusable by a rail switched off: its pull-ups all on
    /// the gated rail while powered devices still use it, or an unpowered
    /// device clamping a bus the powered devices still use. Produced by the
    /// engine-layer `power_states` check.
    PowerDomainBusLockup,
    /// A non-strap pin whose card says it needs a pull-up, a pull-down or a
    /// defined idle level (an open-drain flag, a bootloader UART RX that must
    /// idle high) with that bias missing, of the opposite direction, or outside
    /// the datasheet's resistance range. Produced by the engine-layer
    /// `pin_bias` check.
    RequiredPinBias,
    /// A supply input or output with less capacitance to ground than its
    /// card's datasheet requires (a motor driver's bulk capacitor, a regulator
    /// or load switch's input and output capacitors). Produced by the
    /// engine-layer `supply_cap` check.
    SupplyCapacitance,
    /// A regulator fed from a rectified low-frequency source whose bulk
    /// capacitor lets the input sag, between charging peaks, below the least
    /// input the regulator needs. Produced by the engine-layer `holdup` check.
    HoldUpDroop,
    /// An open-drain or open-collector output (alert, interrupt, fault,
    /// power-good, data-out) on a net with no resistor path to a power rail and
    /// no receiving MCU pin the card says has an internal pull-up: the line
    /// can only be pulled low, so it floats when the part lets go. Produced by
    /// the engine-layer `open_drain_pullup` check.
    MissingOpenDrainPullup,
    /// A point-to-point signal net wired against its pins' directions: it
    /// joins only outputs (or only bidirectional or open-drain pins) with
    /// nothing to receive, or only inputs with nothing to drive them. Fires
    /// only when every non-passive pin on the net has a known direction.
    /// Produced by the engine-layer `signal_direction` check.
    SignalDirection,
    /// A programming or debug header whose pin order mates a standard adapter
    /// the wrong way round: the board's transmit facing the adapter's
    /// transmit, a SWD/JTAG/ISP signal on another signal's pin. Judged
    /// against the one header table in `hauksbee-ir`. Produced by the
    /// engine-layer `debug_header` check.
    DebugHeaderPinout,
    /// A two-wire bus crossed between a standard connector and the part behind
    /// it: a USB receptacle's `D+` on the controller's `D-` pad, a Qwiic or
    /// STEMMA QT port's `SDA` pin on the controller's `SCL` pad. The connector's
    /// pin roles come from the one table in `hauksbee-ir`. Produced by the
    /// engine-layer `bus_swap` check.
    BusLineSwap,
    /// A USB-C receptacle whose CC resistors are the wrong value or kind for
    /// the port's role: a sink pull-down that is not 5.1 kOhm, a source pull-up
    /// that is not a standard Rp, Rp on a port whose VBUS is an input or Rd on
    /// one that drives it, a CC pin on ground. Judged against USB Type-C R2.0
    /// Tables 4-24 and 4-25. Produced by the engine-layer `cc_termination`
    /// check.
    UsbCcTermination,
    /// A switch or push button that joins two different supply rails, or a
    /// rail and ground, with no series resistance: a short when pressed. Also
    /// the reset or strap line it serves with no pull resistor. Produced by
    /// the engine-layer `switch_short` check.
    SwitchShort,
    /// A net named for a peripheral function (SPI SCK/MOSI/MISO, I2C
    /// SDA/SCL, UART TX/RX, PWM) on an MCU pin whose card says the pin cannot
    /// carry it. Produced by the engine-layer `pin_mux` check.
    PinMuxCapability,
    /// A symbol pin whose name (a crystal in/out, or another named function)
    /// disagrees with the model card's pin map for that pin number. Produced
    /// by the engine-layer `symbol_card_pins` check.
    SymbolCardPinMismatch,
    /// An interrupt the firmware handles, such as an AVR timer input capture,
    /// that fires only from a pad the board leaves unconnected, so the handler
    /// can never run. Produced by the `firmware_intent` check, which runs only
    /// when a firmware image is given.
    FirmwareUnconnectedPad,
    /// A current-sense shunt (a resistor of at most 0.1 ohm with a measurement
    /// input on its nets) whose sense connection reaches the shunt pad only
    /// through copper that also carries the load current, when that shared
    /// copper's estimated resistance exceeds 1% of the shunt. Produced by the
    /// layout check in `kelvin_sense`.
    KelvinSense,
}

/// The phrase that marks a pull-up-presence finding as the "the resistor
/// exists but is do-not-populate" variant.
///
/// The plain-language renderer keys
/// its DNP explanation off this exact substring, so it is a shared constant
/// rather than two strings that can silently drift apart.
pub const DNP_PULLUP_MESSAGE_MARKER: &str = "do-not-populate";

/// The phrase that marks the policy-fitted variant of a DNP pull-up finding.
///
/// That is the variant where this run fits DNP parts, so the resistor exists in
/// the analysed board but not on the assembled one. The other DNP variant (the
/// default policy, where the resistor is already left out) does not say it. The
/// plain renderer keys its explanation off this substring; shared for the same
/// no-drift reason as [`DNP_PULLUP_MESSAGE_MARKER`].
pub const DNP_FITTED_NOTE_MARKER: &str = "this run fits DNP parts";

/// The phrase that marks an SD pull-up finding as the SPI-wired-socket
/// variant.
///
/// Its rationale differs from SD mode (CMD is the host's
/// push-pull DI there, not an open-drain identification line). The plain
/// renderer keys its SPI explanation off this substring; shared for the same
/// no-drift reason as [`DNP_PULLUP_MESSAGE_MARKER`].
pub const SPI_PULLUP_MESSAGE_MARKER: &str = "wired for SPI mode";

/// The phrase a finding's message ends on when the part's model card is a user file.
///
/// That is a `--models-dir` or user-directory card, and the text after the
/// phrase is its path. The plain renderer reads it back with
/// [`model_card_file_in`] so its fix names the file to edit; shared for the same
/// no-drift reason as [`DNP_PULLUP_MESSAGE_MARKER`].
pub const MODEL_CARD_MESSAGE_MARKER: &str = " Model card file: ";

/// The user-supplied model card file a finding message names, if it does.
#[must_use]
pub fn model_card_file_in(message: &str) -> Option<&str> {
    message
        .rsplit_once(MODEL_CARD_MESSAGE_MARKER)
        .map(|(_, file)| file.trim())
        .filter(|file| !file.is_empty())
}

/// The I2C speed class a bus is judged at (UM10204 Table 10).
///
/// A static lint cannot see the firmware's clock, so the class comes from what
/// the board says: a bus net named for its speed (`I2C_400K_SDA`, `SCL_FM+`).
/// A bus that names none is judged as standard mode and the finding says so,
/// because standard mode has the loosest rise-time budget and cannot fault a bus
/// that runs faster than the board claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum I2cMode {
    /// Standard mode, 100 kHz: 1000 ns rise budget, 3 mA sink.
    Standard,
    /// Fast mode, 400 kHz: 300 ns rise budget, 3 mA sink.
    Fast,
    /// Fast-mode plus, 1 MHz: 120 ns rise budget, 20 mA sink.
    FastPlus,
}

impl I2cMode {
    /// The class a net or part name states, `None` when it states none.
    #[must_use]
    pub fn from_hint(text: &str) -> Option<Self> {
        let upper = text.to_ascii_uppercase();
        let tokens: Vec<&str> = upper
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '+' || c == '.'))
            .filter(|t| !t.is_empty())
            .collect();
        let has = |t: &str| tokens.contains(&t);
        if [
            "FM+", "FMP", "FMPLUS", "FASTPLUS", "1M", "1MHZ", "1000K", "1000KHZ",
        ]
        .iter()
        .any(|t| has(t))
            || (has("FAST") && has("PLUS"))
        {
            Some(Self::FastPlus)
        } else if ["FAST", "400K", "400KHZ", "0.4M"].iter().any(|t| has(t)) {
            Some(Self::Fast)
        } else if ["STD", "STANDARD", "100K", "100KHZ"].iter().any(|t| has(t)) {
            Some(Self::Standard)
        } else {
            None
        }
    }

    /// The most current (A) a bus driver of this class sinks at VOL = 0.4 V.
    #[must_use]
    pub const fn sink_limit_a(self) -> f64 {
        match self {
            Self::Standard | Self::Fast => 3.0e-3,
            Self::FastPlus => 20.0e-3,
        }
    }

    /// The longest 30%..70% rise time (s) of this class.
    #[must_use]
    pub const fn rise_limit_s(self) -> f64 {
        match self {
            Self::Standard => 1000.0e-9,
            Self::Fast => 300.0e-9,
            Self::FastPlus => 120.0e-9,
        }
    }

    /// The class, with its clock, as a message names it.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Standard => "standard-mode (100 kHz)",
            Self::Fast => "fast-mode (400 kHz)",
            Self::FastPlus => "fast-mode plus (1 MHz)",
        }
    }
}

impl LintCheck {
    /// Every variant, for exhaustiveness-style tests in downstream crates
    /// (e.g. "every check has a plain-language template"). Grows with the
    /// enum; a variant added here but not to a consumer's handling shows up
    /// as a test failure instead of a silent gap.
    pub const ALL: [Self; 56] = [
        Self::MissingI2cPullup,
        Self::MissingSdPullup,
        Self::FloatingControlPin,
        Self::LedCurrentSanity,
        Self::OutputContention,
        Self::StrapPin,
        Self::McuResourceConflict,
        Self::DesignatorFootprintMismatch,
        Self::ValuePackageSanity,
        Self::PlaceholderValue,
        Self::UncheckedMcu,
        Self::DeviceDecode,
        Self::OperatingEnvelope,
        Self::BackPower,
        Self::I2cBusLoading,
        Self::PinFunction,
        Self::UnroutedPad,
        Self::PcbSchematicMismatch,
        Self::RailConflict,
        Self::MissingDecoupling,
        Self::FloatingInput,
        Self::DcOverstress,
        Self::GroundIsland,
        Self::UnwiredConnector,
        Self::LedReversed,
        Self::SiblingMismatch,
        Self::UnlimitedBaseDrive,
        Self::CapNoDcPath,
        Self::FloatingGate,
        Self::GateUnderdrive,
        Self::RegulatorHeldOff,
        Self::PowerPinWiring,
        Self::SupplyGroundSwapped,
        Self::DiodeReversed,
        Self::PolarisedCapReversed,
        Self::RailUnpowered,
        Self::ControlHeldOff,
        Self::DatasheetMismatch,
        Self::PartFieldMismatch,
        Self::PullupIneffective,
        Self::AudioBusWiring,
        Self::PowerDomainBackfeed,
        Self::PowerDomainBusLockup,
        Self::RequiredPinBias,
        Self::SupplyCapacitance,
        Self::HoldUpDroop,
        Self::MissingOpenDrainPullup,
        Self::SignalDirection,
        Self::DebugHeaderPinout,
        Self::BusLineSwap,
        Self::UsbCcTermination,
        Self::SwitchShort,
        Self::PinMuxCapability,
        Self::SymbolCardPinMismatch,
        Self::FirmwareUnconnectedPad,
        Self::KelvinSense,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingI2cPullup => "missing_i2c_pullup",
            Self::MissingSdPullup => "missing_sd_pullup",
            Self::FloatingControlPin => "floating_control_pin",
            Self::LedCurrentSanity => "led_current_sanity",
            Self::OutputContention => "output_contention",
            Self::StrapPin => "strap_pin",
            Self::McuResourceConflict => "mcu_resource_conflict",
            Self::DesignatorFootprintMismatch => "designator_footprint_mismatch",
            Self::ValuePackageSanity => "value_package_sanity",
            Self::PlaceholderValue => "placeholder_value",
            Self::UncheckedMcu => "unchecked_mcu",
            Self::DeviceDecode => "device_decode",
            Self::OperatingEnvelope => "operating_envelope",
            Self::BackPower => "back_power",
            Self::I2cBusLoading => "i2c_bus_loading",
            Self::PinFunction => "pin_function",
            Self::UnroutedPad => "unrouted_pad",
            Self::PcbSchematicMismatch => "pcb_schematic_mismatch",
            Self::RailConflict => "rail_conflict",
            Self::MissingDecoupling => "missing_decoupling",
            Self::FloatingInput => "floating_input",
            Self::DcOverstress => "dc_overstress",
            Self::GroundIsland => "ground_island",
            Self::UnwiredConnector => "unwired_connector",
            Self::LedReversed => "led_reversed",
            Self::SiblingMismatch => "sibling_mismatch",
            Self::UnlimitedBaseDrive => "unlimited_base_drive",
            Self::CapNoDcPath => "cap_no_dc_path",
            Self::FloatingGate => "floating_gate",
            Self::GateUnderdrive => "gate_underdrive",
            Self::RegulatorHeldOff => "regulator_held_off",
            Self::PowerPinWiring => "power_pin_wiring",
            Self::SupplyGroundSwapped => "supply_ground_swapped",
            Self::DiodeReversed => "diode_reversed",
            Self::PolarisedCapReversed => "polarised_cap_reversed",
            Self::RailUnpowered => "rail_unpowered",
            Self::ControlHeldOff => "control_held_off",
            Self::DatasheetMismatch => "datasheet_mismatch",
            Self::PartFieldMismatch => "part_field_mismatch",
            Self::PullupIneffective => "pullup_ineffective",
            Self::AudioBusWiring => "audio_bus_wiring",
            Self::PowerDomainBackfeed => "power_domain_backfeed",
            Self::PowerDomainBusLockup => "power_domain_bus_lockup",
            Self::RequiredPinBias => "required_pin_bias",
            Self::SupplyCapacitance => "supply_capacitance",
            Self::HoldUpDroop => "hold_up_droop",
            Self::MissingOpenDrainPullup => "missing_open_drain_pullup",
            Self::SignalDirection => "signal_direction",
            Self::DebugHeaderPinout => "debug_header_pinout",
            Self::BusLineSwap => "bus_line_swap",
            Self::UsbCcTermination => "usb_cc_termination",
            Self::SwitchShort => "switch_short",
            Self::PinMuxCapability => "pin_mux_capability",
            Self::SymbolCardPinMismatch => "symbol_card_pin_mismatch",
            Self::FirmwareUnconnectedPad => "firmware_unconnected_pad",
            Self::KelvinSense => "kelvin_sense",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Would ship a functional failure.
    High,
    /// Degrades margin / robustness; works in the nominal case.
    Medium,
    /// Ugly practice, unlikely to bite.
    Low,
}

impl Severity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

/// The full lint report.
#[derive(Debug, Clone, Default)]
pub struct NetLintReport {
    pub findings: Vec<LintFinding>,
}

impl NetLintReport {
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
    #[must_use]
    pub const fn count(&self) -> usize {
        self.findings.len()
    }
    pub fn of_check(&self, c: LintCheck) -> impl Iterator<Item = &LintFinding> {
        self.findings.iter().filter(move |f| f.check == c)
    }
}

impl ExtractedBoard {
    /// Run the connectivity lint-class checks.
    #[must_use]
    pub fn net_lint(&self) -> NetLintReport {
        let mut report = NetLintReport::default();
        let _members = MemberIndexScope::new(self);
        check_i2c_pullups(self, &mut report);
        check_sd_pullups(self, &mut report);
        check_floating_control_pins(self, &mut report);
        check_output_contention(self, &mut report);
        check_series_contention(self, &mut report);
        check_pin_functions(self, &mut report);
        check_design_file_qc(self, &mut report);
        topology::check_ground_islands(self, &mut report);
        topology::check_unwired_connectors(self, &mut report);
        topology::check_reversed_leds(self, &mut report);
        polarity::check_connector_supply_pins(self, &mut report);
        polarity::check_diodes_across_rails(self, &mut report);
        polarity::check_polarised_capacitors(self, &mut report);
        report
    }
}

// ---------------------------------------------------------------------------
// Net classification: one table for the lint, the SI checks, the binder and
// every board check (`hauksbee_models::netclass`).
// ---------------------------------------------------------------------------

pub use hauksbee_models::netclass::{is_ground, is_positive_supply, is_supply, leaf as norm};
pub use polarity::capacitor_polarity_marks;

/// Is this component a plain two-terminal resistor (the kind that can be a
/// pull-up)?
///
/// Delegates to `crate::part_class::classify_two_terminal`, which answers from
/// the model DB's declared passive class through the assembly witness and only
/// falls back to the designator / `lib_id` strings when no better evidence
/// exists. A bare designator-prefix test loses a real resistor numbered into an
/// `RN` range and accepts a capacitor labelled `R5` as a pull-up, farads read as
/// ohms. Shared with the SI checks so the two surfaces cannot disagree.
#[must_use]
pub fn is_resistor(c: &Component) -> bool {
    part_class::classify_two_terminal(c).is_resistor()
}

/// A resistor NETWORK / array (RN/RP/RM): several resistor elements in one
/// package, a standard way to pull up an I2C bus (a 4-element array pulls SDA/SCL
/// and two more signals). Not a plain two-terminal resistor, so `is_resistor`
/// excludes it, but it IS a legitimate bus pull-up and an active-device
/// miscount, so the I2C check must recognise it.
fn is_resistor_array(c: &Component) -> bool {
    let r = ref_designator(&c.reference);
    if (r.starts_with("RN") || r.starts_with("RP") || r.starts_with("RM")) && connected_pads(c) >= 3
    {
        return true;
    }
    // RA/RZ are common array designators outside KiCad's RN convention.
    if (r.starts_with("RA") || r.starts_with("RZ")) && connected_pads(c) >= 3 {
        return true;
    }
    // A plain R-designated part on an array footprint is the same physical
    // thing; without this, its `4x0402` grid read as a pin array and the part
    // classified connector-like, silencing whole nets. Vendor array footprints
    // (Bourns CAY16/CAT16, Panasonic EXB) carry no "r_array" token.
    let fp = c.footprint.to_ascii_lowercase();
    r.starts_with('R')
        && connected_pads(c) >= 3
        && (fp.contains("r_array")
            || fp.contains("cay16")
            || fp.contains("cat16")
            || fp.contains("exb"))
}

/// Is this an I2C level translator that provides its own bus pull-ups, so an
/// external pull-up on the connected bus is optional rather than missing?
///
/// Open-drain auto-direction translators (NXP `NTS010x`, TI `TXS010x`) and
/// buffered repeaters (PCA9517/PCA9617, LTC4311) integrate ~10 kOhm pull-ups to
/// each Vcc. A bare pass-gate switch (PCA9306, PCA9509 pass type) does NOT, so
/// it is deliberately excluded, as is the push-pull `TXB010x`: it has no pull-ups
/// and "should not be used in applications such as I2C" (TXB0101 SCES639F 6.3.4).
fn has_integrated_i2c_pullups(c: &Component) -> bool {
    const FAMILIES: [&str; 6] = [
        "NTS010", "TXS010", "PCA9517", "PCA9617", "LTC4311", "NVT2010",
    ];
    let v = c.value.to_ascii_uppercase();
    FAMILIES.iter().any(|f| v.contains(f))
}

/// Is this an LED? Either the record's own identity ([`is_led_identity`]) or
/// the looser hints a name carries (ref `LED*`, a `D*` with an LED symbol,
/// "led" anywhere in the library id or value).
///
/// This is the one LED test: the binder's generic-LED guess and the indicator
/// tracer both start from [`is_led_identity`], so a part the binder models as
/// an LED is always one the checks trace as an LED.
#[must_use]
pub fn is_led(c: &Component) -> bool {
    let lib = c.lib_id.to_ascii_lowercase();
    let val = c.value.to_ascii_lowercase();
    let r = ref_designator(&c.reference);
    is_led_identity(c)
        || lib.contains("led")
        || val.contains("led")
        || (r.starts_with("LED"))
        || (r.starts_with('D') && lib.contains(":led"))
}

/// Does the record itself call this part an LED?
///
/// It does when it carries an LED symbol (`Device:LED`, `LED_Small`, a symbol
/// in an `LED` library), an LED package (`LED_SMD:`, `LED_THT:`,
/// `CHIPLED_0805`, `LEDs:LED_0603`, `PCM_LED_SMD_*`, `LED-1206`, `LED5MM`) or
/// the value exactly `LED`.
///
/// Pad count, designator and wavelength are the caller's to judge; `OLED`
/// modules and multi-die `LED3535` packages are not named by this test.
#[must_use]
pub fn is_led_identity(c: &Component) -> bool {
    // A footprint word that is the LED package itself: `LED`, `LEDS`,
    // `CHIPLED`, `LED5MM`; never `OLED` or a multi-die `LED3535`.
    let package_word = |word: &str| {
        let word = word.strip_prefix("CHIP").unwrap_or(word);
        let rest = word.strip_prefix("LED").unwrap_or("-");
        matches!(rest, "" | "S")
            || rest
                .strip_suffix("MM")
                .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
    };
    let by_package = c
        .footprint
        .to_ascii_uppercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(package_word);
    let (library, symbol) = c.lib_id.split_once(':').unwrap_or(("", &c.lib_id));
    let symbol = symbol.to_ascii_uppercase();
    let by_symbol = symbol == "LED"
        || symbol.starts_with("LED_")
        || ["LED", "LEDS"].contains(&library.to_ascii_uppercase().as_str());
    by_symbol || by_package || c.value.trim().eq_ignore_ascii_case("LED")
}

/// True if the component looks like a bare connector / header / test point /
/// mounting hole.
///
/// These are things whose pins legitimately dangle or break a bus out to an
/// external module that carries its own terminations. Checks the reference
/// prefix, the library/symbol id, AND the footprint / package name, because
/// boards label headers with non-obvious refs (Adafruit's Arduino headers are
/// `IOH`/`IOL`/`AD`/`POWER`, Eagle package `1X10`).
#[must_use]
pub fn is_connector_like(c: &Component) -> bool {
    // A resistor NETWORK (RN/RP/RM) is a passive part, never a connector, even
    // though its footprint name carries a grid dimension ("R_Array_..._4x0603")
    // that `is_pin_array_package` reads as a pin array. Without this a resistor-
    // array I2C pull-up was classified a header (exits_to_connector) and its
    // pull-up credit skipped.
    if is_resistor_array(c) {
        return false;
    }
    let r = c.reference.to_ascii_uppercase();
    let lib = c.lib_id.to_ascii_lowercase();
    let fp = c.footprint.to_ascii_lowercase();
    // A chip that sits in a socket, or whose designator reads as a switch
    // (`SW1` on an analog-switch IC), is still the chip: the footprint's
    // "socket" and the designator's "SW" are not evidence it is a connector.
    let chip = || is_socketed_chip(c) || (r.starts_with("SW") && crate::is_active_ic(c));
    // Reference-prefix conventions.
    if r.starts_with('J')
        || r.starts_with("CN")
        || r.starts_with("CON")
        || r.starts_with("TP")
        || r.starts_with("MH")
        || r.starts_with("MK")
        || (r.starts_with("SW") && !chip()) // a button is not a bus terminator
        || r == "POWER"
        || r.starts_with("IOH")
        || r.starts_with("IOL")
        || r.starts_with("UEXT")
        || r.starts_with("EXT")
    {
        return true;
    }
    // P-prefixed pin headers (P1, PWR) but not transistors etc - P alone is
    // rare as a connector ref; keep it but guard against e.g. "PHY".
    if (r.starts_with('P') && r.len() <= 3 && r.chars().skip(1).all(|c| c.is_ascii_digit()))
        || r == "PWR"
    {
        return true;
    }
    // Library / symbol id.
    if lib.contains("connector")
        || lib.contains("conn:")
        || lib.contains("mountinghole")
        || lib.contains("testpoint")
        || lib.contains("header")
        || lib.contains("pinhead")
    {
        return true;
    }
    // Value string naming a connector / module interface (board-to-board, SoM,
    // SODIMM, FPC). Adapter / interposer boards label these with `U` refs.
    let val = c.value.to_ascii_lowercase();
    if val.contains("connector")
        || val
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .any(|word| word == "som")
        || val.contains("sodimm")
        || val.contains("header")
        || (val.contains("socket") && !chip())
    {
        return true;
    }
    // Footprint / package geometry: pin rows, headers, sockets, board-to-board
    // and edge connectors (Hirose DF-series, SODIMM, FPC/FFC, card edge).
    fp.contains("header")
        || fp.contains("connector")
        || fp.contains("uext")
        || fp.contains("pinhead")
        || (fp.contains("socket") && !chip())
        || fp.contains("sodimm")
        || fp.contains("df40")
        || fp.contains("df12")
        || fp.contains("df9")
        || fp.contains("b2b")
        || fp.contains("fpc")
        || fp.contains("ffc")
        || fp.contains("edge")
        // Pin rows (`_1x04`, `_2x20`) come through the grid parser, which tells
        // them from an IC body (`LGA-12_2x2mm`); a bare `_2x` substring did not.
        || is_pin_array_package(&c.footprint)
        || is_module_socket(c)
}

/// Whether a connector-like part's pad `p` is a driver rather than a bare contact.
///
/// A plug or pin header holds no level of its own, but a plug-in module, a
/// system-on-module or a dev board drives its outputs, though its footprint
/// reads as a board-to-board connector. A pad counts as a driver when the symbol
/// types it `output`, `bidirectional`, `tri_state`, `open_collector` or
/// `open_emitter`, or when the part's value or symbol says it is a module (not
/// a pad typed `input` or as a supply input). Passive pads of a header are not.
#[must_use]
pub fn drives_a_level(c: &Component, p: &Pin) -> bool {
    let kind = p.kind.to_ascii_lowercase();
    let kind = kind.split('+').next().unwrap_or_default().trim();
    if matches!(
        kind,
        "output" | "bidirectional" | "tri_state" | "open_collector" | "open_emitter"
    ) {
        return true;
    }
    if matches!(kind, "input" | "power_in") {
        return false;
    }
    let text = format!("{} {}", c.value, c.lib_id).to_ascii_lowercase();
    let mut words = text.split(|ch: char| !ch.is_ascii_alphanumeric());
    words.any(|w| matches!(w, "module" | "som" | "devkit" | "compute"))
        || text.contains("dev board")
        || text.contains("dev_board")
        || text.contains("development board")
}

/// An IC that plugs into a socket (a DIP or PLCC part in a socket footprint) or
/// otherwise reads as a connector by its footprint alone: the record itself says
/// it is a chip. Decided by the symbol, value, part number and pin roles, not by
/// the footprint name: an active IC (see [`crate::is_active_ic`]) on a footprint
/// that lands an IC package, or one with supply and ground pins and a part-number
/// value, and in neither case a pin row, a connector symbol or a module-socket
/// footprint.
fn is_socketed_chip(c: &Component) -> bool {
    if !crate::is_active_ic(c) || is_pin_array_package(&c.footprint) || is_module_socket(c) {
        return false;
    }
    let lib = c.lib_id.to_ascii_lowercase();
    if ["connector", "conn:", "header", "pinhead", "pinsocket"]
        .iter()
        .any(|k| lib.contains(k))
    {
        return false;
    }
    // A value that names a part family (`24Cxx`) or counts pins (`PIC_18_PINS`)
    // describes the socket's target, not a chip sitting in it.
    let value = c.value.to_ascii_lowercase();
    if value.contains("xx")
        || value
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .any(|w| w == "pin" || w == "pins")
    {
        return false;
    }
    let pads: std::collections::HashSet<&str> = c.pins.iter().map(|p| p.number.as_str()).collect();
    if hauksbee_models::refdes::lands_an_ic_package(&c.footprint, pads.len()) {
        return true;
    }
    let named = |names: &[&str]| {
        c.pins.iter().any(|p| {
            let f = p.function.to_ascii_uppercase();
            names.iter().any(|n| f.starts_with(n))
        })
    };
    let part_number = c.value.chars().any(|ch| ch.is_ascii_digit())
        && c.value.chars().any(|ch| ch.is_ascii_alphabetic())
        && c.value.chars().filter(char::is_ascii_alphanumeric).count() >= 4;
    part_number
        && named(&["VCC", "VDD", "VIN", "V+", "AVDD", "DVDD", "VBAT"])
        && named(&["GND", "VSS", "AGND", "V-"])
}

/// Fewest distinct pads a layout footprint needs before its row shape alone
/// marks it a module socket. Two-row IC packages stop well short of this (the
/// largest TSOP-II has 86 pins).
const MODULE_SOCKET_MIN_PADS: usize = 100;

/// Distance between pad centres, across the rows, that still counts as one
/// row: generous for placement rounding, well under any pad pitch.
const MODULE_SOCKET_ROW_TOLERANCE_MM: f64 = 0.2;

/// A layout footprint with 100 or more pads nearly all on at most four parallel
/// lines: a SODIMM, board-to-board or mezzanine socket that a plug-in module
/// mates with.
///
/// A carrier board often names the socket after the module it takes (a bare
/// part number in the value, a vendor connector number in the footprint), so
/// no word in the file says "connector". The pad geometry does: QFP, QFN, BGA
/// and LGA packages spread their pins over four sides or a grid, and no IC
/// package puts this many pins in two rows. The rows are found along the pad
/// cloud's own principal axis, so a socket placed at any angle reads the same.
/// Schematic pin positions are not pad geometry (a big symbol is often drawn
/// as two pin columns), so only parts placed on a copper layer qualify.
#[must_use]
pub fn is_module_socket(c: &Component) -> bool {
    if c.layer.is_empty() || c.pins.len() < MODULE_SOCKET_MIN_PADS {
        return false;
    }
    let placed: Vec<(&str, (f64, f64))> = (c.pins.iter())
        .filter_map(|p| p.position.map(|at| (p.number.as_str(), at)))
        .collect();
    let distinct: std::collections::HashSet<&str> = placed.iter().map(|(n, _)| *n).collect();
    if distinct.len() < MODULE_SOCKET_MIN_PADS {
        return false;
    }
    #[allow(clippy::cast_precision_loss, reason = "a pad count is far below 2^52")]
    let n = placed.len() as f64;
    let mean_x = placed.iter().map(|(_, (x, _))| x).sum::<f64>() / n;
    let mean_y = placed.iter().map(|(_, (_, y))| y).sum::<f64>() / n;
    let (mut sxx, mut syy, mut sxy) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (_, (x, y)) in &placed {
        let (dx, dy) = (x - mean_x, y - mean_y);
        sxx = dx.mul_add(dx, sxx);
        syy = dy.mul_add(dy, syy);
        sxy = dx.mul_add(dy, sxy);
    }
    // Angle of the principal (long) axis; distances across the rows are
    // projections onto the perpendicular.
    let theta = 0.5 * (2.0 * sxy).atan2(sxx - syy);
    let (sin, cos) = theta.sin_cos();
    let mut across: Vec<f64> = (placed.iter())
        .map(|(_, (x, y))| cos.mul_add(y - mean_y, -sin * (x - mean_x)))
        .collect();
    across.sort_by(f64::total_cmp);
    let mut rows: Vec<usize> = vec![1];
    for pair in across.windows(2) {
        if pair[1] - pair[0] > MODULE_SOCKET_ROW_TOLERANCE_MM {
            rows.push(1);
        } else if let Some(last) = rows.last_mut() {
            *last += 1;
        }
    }
    rows.sort_unstable_by(|a, b| b.cmp(a));
    // Shield tabs and mounting pads may sit off the rows; 95% must be on them.
    let on_rows: usize = rows.iter().take(4).sum();
    on_rows * 20 >= placed.len() * 19
}

/// Eagle/KiCad pin-row package names like "1X10", "2X05", "`1X10_OVALWAVE`".
///
/// A pin grid is INTEGER counts with no unit. A body DIMENSION ("7x7mm",
/// "3.9x4.9mm") is a package size, not a pin grid, and KiCad appends one to
/// essentially every SMD IC footprint ("LQFP-48_7x7mm", "QFN-48-1EP_7x7mm"), so
/// accepting any `<digit>X<digit>` makes those ICs read as connector-like and
/// silently suppresses the floating-control / I2C-pull-up / output-contention
/// lints on them. Decimal and `mm`-suffixed tokens are rejected, matching
/// [`crate::gerber::connect`]'s `grid_hint`.
fn is_pin_array_package(fp: &str) -> bool {
    let bytes: Vec<char> = fp.to_ascii_uppercase().chars().collect();
    // IPC-7351 land-pattern names give pitch then body ("QFN50P400X400X100").
    if bytes
        .windows(3)
        .any(|w| w[0].is_ascii_digit() && w[1] == 'P' && w[2].is_ascii_digit())
    {
        return false;
    }
    for i in 0..bytes.len() {
        if bytes[i] != 'X' || i == 0 {
            continue;
        }
        // The numeric token immediately before / after the 'X' (digits and '.').
        let mut a = i;
        while a > 0 && (bytes[a - 1].is_ascii_digit() || bytes[a - 1] == '.') {
            a -= 1;
        }
        let mut b = i + 1;
        while b < bytes.len() && (bytes[b].is_ascii_digit() || bytes[b] == '.') {
            b += 1;
        }
        let left: &[char] = &bytes[a..i];
        let right: &[char] = &bytes[i + 1..b];
        // A digit must sit immediately on each side of the 'X'.
        if !left.last().is_some_and(char::is_ascii_digit)
            || !right.first().is_some_and(char::is_ascii_digit)
        {
            continue;
        }
        // Body dimension, not a pin grid: a decimal ("3.9x4.9") or a trailing "mm"
        // unit ("7x7mm"). A real pin grid is integer counts with no unit.
        let is_mm = bytes.get(b) == Some(&'M') && bytes.get(b + 1) == Some(&'M');
        if left.contains(&'.') || right.contains(&'.') || is_mm {
            continue;
        }
        return true;
    }
    false
}

/// (component, pin) indices of every net's members, for the board a
/// [`ExtractedBoard::net_lint`] run is on (keyed by its address): the checks ask
/// for members net by net, and scanning every component per ask was quadratic
/// in the part count on a large board.
type MemberIndex = (usize, std::collections::HashMap<i64, Vec<(usize, usize)>>);

std::thread_local! {
    static MEMBER_INDEX: std::cell::RefCell<Option<MemberIndex>> =
        const { std::cell::RefCell::new(None) };
}

/// Installs the member index for one lint run; removes it when dropped.
struct MemberIndexScope;

impl MemberIndexScope {
    fn new(board: &ExtractedBoard) -> Self {
        let mut by_net: std::collections::HashMap<i64, Vec<(usize, usize)>> =
            std::collections::HashMap::new();
        for (ci, c) in board.components.iter().enumerate() {
            for (pi, p) in c.pins.iter().enumerate() {
                if let Some(net) = p.net {
                    by_net.entry(net).or_default().push((ci, pi));
                }
            }
        }
        let key = std::ptr::from_ref(board) as usize;
        MEMBER_INDEX.with(|m| *m.borrow_mut() = Some((key, by_net)));
        Self
    }
}

impl Drop for MemberIndexScope {
    fn drop(&mut self) {
        MEMBER_INDEX.with(|m| *m.borrow_mut() = None);
    }
}

/// All (component, pin) members of a net id, in component then pin order.
fn members(board: &ExtractedBoard, net_id: i64) -> Vec<(&Component, &Pin)> {
    let key = std::ptr::from_ref(board) as usize;
    let indexed = MEMBER_INDEX.with(|m| {
        let m = m.borrow();
        let (_, by_net) = m.as_ref().filter(|(k, _)| *k == key)?;
        Some(by_net.get(&net_id).cloned().unwrap_or_default())
    });
    indexed.map_or_else(
        || board.net_members(net_id),
        |at| {
            let pick = |(ci, pi): (usize, usize)| {
                let c = &board.components[ci];
                (c, &c.pins[pi])
            };
            at.into_iter().map(pick).collect()
        },
    )
}

/// Is this component a capacitor (decoupling, bulk or crystal load)?
///
/// Its presence to ground makes a net read as a local supply rail. Delegates
/// to the same evidence ladder as `is_resistor`, so the capacitor and
/// resistor questions stay consistent about any one part.
#[must_use]
pub fn is_capacitor(c: &Component) -> bool {
    part_class::classify_two_terminal(c).is_capacitor()
}

/// A passive value that names no value: blank, `?`, or the bare letter `R`/`C`/`L`.
///
/// `TBD` is not one: shipped boards use it on optional parts, so gating on it would
/// fail known-good designs. The binder still leaves such a part open and says so.
#[must_use]
pub fn is_placeholder_value(value: &str) -> bool {
    ["", "?", "R", "C", "L"]
        .iter()
        .any(|p| value.trim().eq_ignore_ascii_case(p))
}

/// The passive ('R', 'C' or 'L') a designator claims, from the one prefix
/// table (`hauksbee_models::refdes`): `LED1`, `LS1`, `CR1` and `RN1` claim none.
fn passive_prefix(reference: &str) -> Option<char> {
    match RefClass::of(reference) {
        RefClass::Resistor => Some('R'),
        RefClass::Capacitor => Some('C'),
        RefClass::Inductor => Some('L'),
        _ => None,
    }
}

/// Whether a capacitor-designated part carries a zero-resistance value (`0`,
/// `0R`, `0 ohm`) and nothing in its record says it is a deliberate link: not a
/// resistor symbol or footprint, not a jumper or net tie. Such a position is a
/// wire where a capacitor was meant, or a link option nobody marked.
fn is_zero_ohm_in_capacitor_position(c: &Component) -> bool {
    let Some(v) = hauksbee_models::value::parse_value(&c.value) else {
        return false;
    };
    let ohmic = v.unit.as_deref().is_none_or(|u| {
        let u = u.to_ascii_lowercase();
        u == "r" || u == "ohm" || u == "ohms" || u == "\u{3a9}"
    });
    if !ohmic || v.si > 0.0 || crate::dnp::is_jumper_or_net_tie(c) {
        return false;
    }
    !is_deliberate_zero_ohm_part(c)
}

/// Whether a C-designated part's own symbol and package say it is a resistor.
///
/// A project's library may call its parts anything (`myproj:R_0603`,
/// `project_lib:Resistor`, `R_Small_US`, `Resistors_SMD:R_0805`, Eagle
/// `rcl:R0603`, IPC `RESC1608X55N`), so the answer comes from the name's tokens
/// ([`name_passive_class`]), not from one library's spelling.
///
/// A capacitor-class symbol is never a deliberate link, even on a resistor
/// footprint: that pairing is a mismatch in the record itself. Otherwise a
/// resistor-class symbol or footprint is the designer saying "0 ohm option".
fn is_deliberate_zero_ohm_part(c: &Component) -> bool {
    match (
        name_passive_class(&c.lib_id),
        name_passive_class(&c.footprint),
    ) {
        (Some('C'), _) => false,
        (Some('R'), _) | (_, Some('R')) => true,
        _ => false,
    }
}

/// `'R'` or `'C'` when a symbol or footprint name says resistor or capacitor.
///
/// Reads the leaf after the last `:` or `/`, split into tokens on anything
/// that is not a letter or digit. A token is resistor-class when it is `r`,
/// `res`, `resistor(s)`, an `r`/`res` followed by digits (`R0603`, `RES0402`),
/// or an IPC-7351 resistor family (`RESC1608X55N`, `RESMELF`, `RESAD`). It is
/// capacitor-class on the same pattern with `c`, `cap`, `capacitor(s)`, `cp`,
/// `cpol`, an IPC `CAPC`/`CAPAE`/`CAPM` family, or a body word (`radial`,
/// `disc`, `elec`, `polarized`, `tantalum`). Whole tokens only, so `Reset` and
/// `Resonator` are neither. A name carrying both kinds, or neither, answers
/// `None`; when the leaf is silent the library part (`Resistor_SMD:`,
/// `Capacitors_SMD:`) is read for the plain words only.
fn name_passive_class(name: &str) -> Option<char> {
    let lower = name.to_ascii_lowercase();
    let (lib, leaf) = lower
        .rsplit_once([':', '/'])
        .unwrap_or(("", lower.as_str()));
    let tokens_class = |text: &str, plain_words_only: bool| {
        let (mut r, mut c) = (false, false);
        for t in text.split(|ch: char| !ch.is_ascii_alphanumeric()) {
            match token_passive_class(t, plain_words_only) {
                Some('R') => r = true,
                Some('C') => c = true,
                _ => {}
            }
        }
        match (r, c) {
            (true, false) => Some('R'),
            (false, true) => Some('C'),
            _ => None,
        }
    };
    tokens_class(leaf, false).or_else(|| tokens_class(lib, true))
}

fn token_passive_class(token: &str, plain_words_only: bool) -> Option<char> {
    let digits_after = |prefix: &str| {
        token
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(|ch: char| ch.is_ascii_digit()))
    };
    if matches!(token, "res" | "resistor" | "resistors") {
        return Some('R');
    }
    if matches!(token, "cap" | "caps" | "capacitor" | "capacitors") {
        return Some('C');
    }
    if plain_words_only {
        return None;
    }
    if token == "r" || digits_after("r") || digits_after("res") {
        return Some('R');
    }
    if ["resc", "resm", "resad", "resistor"]
        .iter()
        .any(|family| token.starts_with(family))
    {
        return Some('R');
    }
    if matches!(token, "c" | "cp" | "cpol")
        || digits_after("c")
        || digits_after("cap")
        || [
            "capc",
            "capae",
            "capap",
            "capm",
            "capad",
            "capprd",
            "capacitor",
        ]
        .iter()
        .any(|family| token.starts_with(family))
        || matches!(
            token,
            "radial" | "disc" | "elec" | "electrolytic" | "polarized" | "tantalum"
        )
    {
        return Some('C');
    }
    None
}

fn footprint_family(footprint: &str) -> Option<char> {
    let fp = footprint.to_ascii_lowercase();
    if fp.contains("capacitor") || fp.contains(":c_") || fp.contains("/c_") {
        Some('C')
    } else if fp.contains("resistor") || fp.contains(":r_") || fp.contains("/r_") {
        Some('R')
    } else if fp.contains("inductor") || fp.contains(":l_") || fp.contains("/l_") {
        Some('L')
    } else {
        None
    }
}

fn package_code(footprint: &str) -> Option<&'static str> {
    let fp = footprint.to_ascii_lowercase();
    [
        "0201", "0402", "0603", "0805", "1206", "1210", "1812", "2010", "2512",
    ]
    .into_iter()
    .find(|code| fp.contains(code))
}

/// The passive a family letter names, for a sentence.
const fn passive_word(family: char) -> &'static str {
    match family {
        'R' => "resistor",
        'C' => "capacitor",
        _ => "inductor",
    }
}

/// A designator on another passive's footprint, and how much it matters.
///
/// Two-terminal R, C and L chips of one EIA size share one IPC chip land
/// within tenths of a millimetre, so an `L5` on `R_0603` fits and solders:
/// that is a library naming note. A land with no chip size code (a radial, a
/// power-inductor body, a named package) or a part that is not two pads may
/// really not fit, which stays a warning.
fn designator_footprint_finding(c: &Component, prefix: char, family: char) -> LintFinding {
    let pads: std::collections::BTreeSet<&str> = c.pins.iter().map(|p| p.number.as_str()).collect();
    let shared_chip = package_code(&c.footprint).filter(|_| pads.len() == 2);
    let (severity, message) = shared_chip.map_or_else(
        || {
            (
                Severity::Medium,
                format!(
                    "{} is a {prefix} designator but uses {family} footprint '{}'",
                    c.reference, c.footprint
                ),
            )
        },
        |size| {
            (
                Severity::Low,
                format!(
                    "{} is a {prefix} designator on a {family} chip land '{}'. Same {size} chip \
                     size, so the part fits; the land is the generic {} variant. Rename the \
                     footprint to the {} land if the BOM tooling keys on it",
                    c.reference,
                    c.footprint,
                    passive_word(family),
                    passive_word(prefix)
                ),
            )
        },
    );
    LintFinding {
        check: LintCheck::DesignatorFootprintMismatch,
        severity,
        message,
        refs: vec![c.reference.clone()],
        nets: Vec::new(),
    }
}

fn parse_capacitance_uf(value: &str) -> Option<f64> {
    let s = value.trim().replace(['µ', 'μ'], "u");
    if s.is_empty() {
        return None;
    }
    // Parse only the LEADING number+unit token. Collecting every digit in the
    // whole string mis-reads values that carry a voltage rating or a package
    // suffix: "10uF 25V" or "10u_0402" would otherwise concatenate to 1025 / 100402
    // uF and false-positive the package-ceiling check (a zero-false-positive
    // violation). Stop the number at the first non-numeric char, then take only
    // the immediately-following letters as the unit, ignoring any trailing text.
    let mut chars = s.chars().peekable();
    let mut num = String::new();
    while let Some(&ch) = chars.peek() {
        if ch.is_ascii_digit() || ch == '.' {
            num.push(ch);
            chars.next();
        } else {
            break;
        }
    }
    let mut unit = String::new();
    while let Some(&ch) = chars.peek() {
        if ch.is_ascii_alphabetic() {
            unit.push(ch.to_ascii_lowercase());
            chars.next();
        } else {
            break;
        }
    }
    // R-style decimal: a digit run AFTER a BARE prefix letter is the fractional
    // part ("4u7" = 4.7 uF, "1n5" = 1.5 nF), mirroring parse_ohms' "4K7" handling.
    // This only applies when the unit is the multiplier letter standing in for the
    // decimal point, i.e. the 'F' is ABSENT. When the unit already spells out the
    // Farad ("1uF25V", "10nF50V"), the value is complete and the trailing digits
    // are an attached voltage rating, not a fraction: eating them turned "1uF25V"
    // into 1.25 uF and false-flagged a valid 0201/0402 cap on the package ceiling.
    // (Canonical hauksbee_models::value::parse_value("1uF25V") already returns 1 uF.)
    let frac: String = if unit.contains('f') {
        String::new()
    } else {
        let mut f = String::new();
        while let Some(&ch) = chars.peek() {
            if ch.is_ascii_digit() {
                f.push(ch);
                chars.next();
            } else {
                break;
            }
        }
        f
    };
    let n: f64 = if frac.is_empty() {
        num.parse().ok()?
    } else {
        format!("{}.{}", num.trim_end_matches('.'), frac)
            .parse()
            .ok()?
    };
    if unit.starts_with("pf") || unit == "p" {
        Some(n / 1_000_000.0)
    } else if unit.starts_with("nf") || unit == "n" {
        Some(n / 1000.0)
    } else if unit.starts_with("uf") || unit.starts_with('u') {
        Some(n)
    } else if unit.starts_with("mf") || unit == "m" {
        Some(n * 1000.0)
    } else {
        None
    }
}

fn mlcc_ceiling_uf(package: &str) -> Option<f64> {
    match package {
        "0201" => Some(1.0),
        "0402" => Some(22.0),
        "0603" => Some(100.0),
        "0805" => Some(220.0),
        _ => None,
    }
}

fn check_design_file_qc(board: &ExtractedBoard, report: &mut NetLintReport) {
    for c in &board.components {
        // The three-state contract: an unassembled part has no value to QC,
        // and an identity-refused record's value is not evidence of one.
        if !crate::assembly::AssemblyState::of(c).is_present() {
            continue;
        }
        let Some(prefix) = passive_prefix(&c.reference) else {
            continue;
        };
        let value_trimmed = c.value.trim();
        // A solder jumper / solder bridge / net tie has no electrical value BY
        // DESIGN, and its reference can look passive (the Arduino Uno's
        // RESET-EN solder jumper starts with 'R'). Demanding a value be "set"
        // on such a part is a guaranteed false positive, so the link-part class
        // the DNP policy already recognises is exempt from the placeholder
        // check. (Verified fire before this guard: Arduino Uno R3 RESET-EN,
        // library "jumper", package "SJ", value "".)
        // A manufacturer part number names the part outright, so a generic
        // symbol value beside it ("L" on a ferrite bead ordered by MPN) is not
        // a value left unset: the BOM and the model lookup go by the MPN.
        let has_mpn = c
            .properties
            .iter()
            .any(|(k, v)| crate::bom::is_mpn_property_key(k) && !v.trim().is_empty());
        // A value the INPUT cannot carry is not a placeholder the designer left:
        // the run states it once as reduced scope (the parts' values are unknown,
        // supply a BOM) rather than blocking the order on it.
        if is_placeholder_value(value_trimmed)
            && !c.value_not_carried()
            && !has_mpn
            && !crate::dnp::is_jumper_or_net_tie(c)
            && !crate::dnp::unbought(c)
        {
            report.findings.push(LintFinding {
                check: LintCheck::PlaceholderValue,
                severity: Severity::Medium,
                message: format!(
                    "{} has placeholder value '{}'; set the actual {} value before BOM/simulation",
                    c.reference, c.value, prefix
                ),
                refs: vec![c.reference.clone()],
                nets: Vec::new(),
            });
        }

        if let Some(family) = footprint_family(&c.footprint) {
            if family != prefix {
                report
                    .findings
                    .push(designator_footprint_finding(c, prefix, family));
            }
        }

        if prefix == 'C' && is_zero_ohm_in_capacitor_position(c) && !crate::dnp::unbought(c) {
            report.findings.push(LintFinding {
                check: LintCheck::ValuePackageSanity,
                severity: Severity::Medium,
                message: format!(
                    "{} is a capacitor position fitted with a 0 ohm link (value '{}'): a link \
                     ties its two nets together at DC, so it blocks nothing and filters nothing. \
                     {REMEDY_MARKER}Fit the capacitor the position needs (AC-coupling, decoupling \
                     and filter positions need one). If a 0 ohm link is intended, mark the \
                     position do-not-populate or draw it as a resistor.",
                    c.reference, c.value
                ),
                refs: vec![c.reference.clone()],
                nets: Vec::new(),
            });
        }

        if prefix == 'C' {
            if let (Some(pkg), Some(uf)) =
                (package_code(&c.footprint), parse_capacitance_uf(&c.value))
            {
                if let Some(max_uf) = mlcc_ceiling_uf(pkg) {
                    if uf > max_uf {
                        report.findings.push(LintFinding {
                            check: LintCheck::ValuePackageSanity,
                            severity: Severity::Medium,
                            message: format!(
                                "{} value {} is implausible in {} (conservative ceiling {:.0}uF)",
                                c.reference, c.value, pkg, max_uf
                            ),
                            refs: vec![c.reference.clone()],
                            nets: Vec::new(),
                        });
                    }
                }
            }
        }
    }
}

/// A net is *rail-like* if its name is a recognised rail, OR it structurally
/// behaves like a local power node: it carries a bypass capacitor to ground
/// (the unmistakable signature of a supply) and is fed through a ferrite /
/// inductor or a regulator. This catches filtered / switched local rails that
/// CAD auto-names `Net-(C5-Pad2)` so a pull-up to them is not mis-flagged as
/// missing. A bus signal never carries a cap-to-ground, so this does not
/// misclassify SDA/SCL themselves.
fn net_is_raillike(board: &ExtractedBoard, net_id: i64) -> bool {
    if board.net(net_id).is_some_and(|n| is_supply(&n.name)) {
        return true;
    }
    let mem = members(board, net_id);
    // A bypass cap to ground on this net?
    mem.iter().any(|(c, _)| {
        is_capacitor(c)
            && c.pins.iter().any(|op| {
                op.net
                    .filter(|id| *id != net_id)
                    .and_then(|id| board.net(id))
                    .is_some_and(|on| is_ground(&on.name))
            })
    })
}

/// KiCad emits one placeholder net per deliberately-unconnected pad, named
/// `unconnected-(REF-PIN-PadN)`. A pin on such a net is an explicit no-connect,
/// never a fault.
#[must_use]
pub fn is_unconnected_net(name: &str) -> bool {
    name.trim_start_matches('/').starts_with("unconnected-")
}

/// The pin's electrical type marks it as a deliberate no-connect.
#[must_use]
pub fn pin_is_no_connect(p: &Pin) -> bool {
    let k = p.kind.to_ascii_lowercase();
    k.contains("no_connect") || k == "nc" || k == "unconnected"
}

// ---------------------------------------------------------------------------
// Check 1: missing I2C pull-ups.
// ---------------------------------------------------------------------------

/// The I2C line a net name states, if any.
///
/// A net is an I2C data/clock line if its (leaf) name is exactly SDA/SCL or a
/// recognised decorated form (`I2C_SDA`, SDA1, ASDA, `SDA_3V3`, ...). We require the
/// SDA/SCL token to stand as its own word so we do not match "USDA" sub-strings
/// or unrelated nets.
#[must_use]
pub fn i2c_role(name: &str) -> Option<&'static str> {
    let n = norm(name);
    // Split on common separators and look for an exact SDA/SCL token.
    let toks: Vec<&str> = n.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    let has = |needle: &str| {
        toks.iter().any(|t| {
            // exact, or token like "SDA1"/"SDA2" (the bus index suffix), or
            // "ASDA"/"ASCL" (Watchy's alt I2C). Strip a single leading 'A' and a
            // trailing digit before comparing.
            let t = t.strip_prefix('A').unwrap_or(t);
            let t = t.trim_end_matches(|c: char| c.is_ascii_digit());
            t == needle
        })
    };
    if has("SDA") {
        Some("SDA")
    } else if has("SCL") {
        Some("SCL")
    } else {
        None
    }
}

/// What a scan of one net's assembled members found by way of a pull-up: a
/// resistor (or array element) with one pad on the net and another on a
/// rail-like, non-ground net.
#[derive(Default)]
struct PullupScan {
    /// A fitted pull-up exists.
    fitted: bool,
    /// A pull-up exists in the layout but is DNP, present only because the
    /// default fit policy assumes DNP parts get placed: as assembled the line
    /// still floats, so the finding names the part instead of clearing.
    dnp_ref: Option<String>,
    /// A pull-up exists in the layout but is do-not-populate and left out of the
    /// assembled board (the default policy): it must not count as a pull-up, and
    /// the finding names it so a reader sees why the line is judged bare.
    absent_ref: Option<String>,
    /// A resistor sits where a pull-up would but its VALUE cannot be parsed (an
    /// MPN or blank field, routine in Altium/EAGLE extraction): the net can be
    /// judged in neither direction.
    unjudgeable: bool,
}

/// Scan `net_id` for a pull-up. `min_ohms` excludes series dampers (22-100 R is
/// routine on SDIO) when the check cares; ground is excluded so a card-detect
/// pull-down or a split-ground stitching cap is never credited. A resistor
/// ARRAY is credited if ANY element reaches a rail, without pairing the element
/// to this net's pad: a stated over-credit kept for consistency.
fn pullup_scan(board: &ExtractedBoard, net_id: i64, min_ohms: Option<f64>) -> PullupScan {
    let mut scan = PullupScan::default();
    for (c, _) in members(board, net_id) {
        let state = AssemblyState::of(c);
        let absent = matches!(state, AssemblyState::DnpAbsent(_));
        if !(state.is_present() || absent) || !(is_resistor(c) || is_resistor_array(c)) {
            continue;
        }
        let parsed = parse_ohms(&c.value);
        if let Some(min) = min_ohms {
            if parsed.is_some_and(|ohms| ohms < min) {
                continue;
            }
        }
        for oid in c.pins.iter().filter_map(|p| p.net).filter(|&o| o != net_id) {
            let far_is_ground = board.net(oid).is_some_and(|n| is_ground(&n.name));
            if far_is_ground || !net_is_raillike(board, oid) {
                continue;
            }
            if absent {
                scan.absent_ref.get_or_insert_with(|| c.reference.clone());
            } else if min_ohms.is_some() && parsed.is_none() {
                scan.unjudgeable = true;
            } else if crate::dnp::fitted_from_dnp_policy(c) {
                scan.dnp_ref.get_or_insert_with(|| c.reference.clone());
            } else {
                scan.fitted = true;
            }
        }
    }
    scan
}

/// A transistor that sources current into `net_id` from a supply: a
/// constant-current or switched pull-up.
///
/// Its collector (or drain) is on the line, its emitter (or source) sits on a
/// non-ground rail directly or through a resistor (the emitter resistor that
/// sets a current source's current), and its base (or gate) is not tied to that
/// emitter, which would hold it off. Pin roles come from the source's pin
/// functions, so a transistor without named pins is never credited, and an
/// emitter on another bus line (an NMOS level shifter) is not a supply.
fn is_transistor_pullup(board: &ExtractedBoard, c: &Component, net_id: i64) -> bool {
    if connected_pads(c) != 3 {
        return false;
    }
    let role_net = |roles: &[&str]| {
        (c.pins.iter())
            .filter(|p| {
                roles
                    .iter()
                    .any(|r| p.function.trim().eq_ignore_ascii_case(r))
            })
            .find_map(|p| p.net)
    };
    let (Some(out), Some(supply), Some(control)) = (
        role_net(&["C", "COLLECTOR", "D", "DRAIN"]),
        role_net(&["E", "EMITTER", "S", "SOURCE"]),
        role_net(&["B", "BASE", "G", "GATE"]),
    ) else {
        return false;
    };
    if out != net_id || supply == net_id || control == supply {
        return false;
    }
    let is_supply = |id: i64| {
        id != net_id
            && board
                .net(id)
                .is_some_and(|n| !is_ground(&n.name) && i2c_role(&n.name).is_none())
            && net_is_raillike(board, id)
    };
    is_supply(supply)
        || members(board, supply).iter().any(|(r, _)| {
            AssemblyState::of(r).is_present()
                && is_resistor(r)
                && (r.pins.iter())
                    .filter_map(|p| p.net)
                    .any(|o| o != supply && is_supply(o))
        })
}

/// Deduped, present-parts-only references of a net's members: a double-listed
/// pad must not repeat a reference, and a skipped DNP part is not implicated.
fn present_refs(mem: &[(&Component, &Pin)]) -> Vec<String> {
    let mut refs: Vec<String> = Vec::new();
    for (c, _) in mem {
        if AssemblyState::of(c).is_present() && !refs.contains(&c.reference) {
            refs.push(c.reference.clone());
        }
    }
    refs
}

/// A DNP part that still marks the far end of a bus: a plug-in module socket
/// (by pad geometry) or a connector / header. Test points, mounting holes and
/// switches carry no bus end, and a DNP resistor or IC is simply absent.
fn is_dnp_bus_end(c: &Component) -> bool {
    if is_module_socket(c) {
        return true;
    }
    let r = c.reference.to_ascii_uppercase();
    is_connector_like(c) && !is_test_point_or_hole(c) && !r.starts_with("SW")
}

/// What the members of a bus say about where it ends.
struct BusEnds {
    via_module_socket: bool,
    exits_to_connector: bool,
    /// The socket or header that ends the bus is marked DNP.
    via_dnp_end: bool,
    active_devices: usize,
}

/// The severity and wording for a bus with no pull-up, or `None` when the
/// topology is too ambiguous to judge.
///
/// Two regimes:
///  - The bus exits to a header / connector with no on-board pull. This is
///    the *intentional* "pull-ups live on the attached module" pattern
///    (every Arduino/Feather breaks I2C out to pins this way). Low / known.
///  - The bus is entirely on-board (a master and at least one on-board
///    peripheral) yet has no pull-up anywhere. This is the higher-
///    confidence "genuinely missing" case. Medium.
///
/// The regime is decided BEFORE the DNP variant so the DNP arm can only fire
/// where the absent-part case would, at the same severity: a topology too
/// ambiguous to judge stays unjudged whether or not the pull-up that isn't
/// fitted happens to exist in the layout, and a simulation-policy flag must
/// not decide whether CI gates. A socket or header marked DNP keeps the regime
/// of its fitted twin (the DNP flag is the fab's build instruction, not a
/// design change), so the note stays Low rather than escalating, and says so.
const fn pullup_regime(ends: &BusEnds) -> Option<(Severity, &'static str)> {
    let dnp = ends.via_dnp_end;
    if ends.via_module_socket {
        Some((
            Severity::Low,
            if dnp {
                "bus reaches a plug-in module socket marked DNP (the module is fitted \
                 later); assumed pulled up on the module, which this file does not show"
            } else {
                "bus reaches a plug-in module socket; assumed pulled up on the module, \
                 which this file does not show"
            },
        ))
    } else if ends.exits_to_connector {
        Some((
            Severity::Low,
            if dnp {
                "bus breaks out to a header marked DNP; pull-ups conventionally on the \
                 attached module"
            } else {
                "bus breaks out to a header; pull-ups conventionally on the attached module"
            },
        ))
    } else if ends.active_devices >= 2 {
        Some((
            Severity::Medium,
            "on-board master and peripheral, no pull-up present",
        ))
    } else {
        None
    }
}

/// The message of a missing-pull-up finding when the line has a resistor that
/// is do-not-populate, or `None` when it has none.
///
/// `absent` is a resistor the assembled board leaves out (the default policy);
/// `policy_fitted` is one this run fits only because the fit policy assumes DNP
/// parts get placed. Neither counts as a pull-up on the built board, so the
/// finding names the part: a reader who sees the resistor drawn on the line
/// must not be told it is missing. `note` is the check's own reason the line
/// needs one. `subject` reads "I2C SDA" or "SD card CMD".
fn dnp_pullup_message(
    subject: &str,
    net: &str,
    absent: Option<&str>,
    policy_fitted: Option<&str>,
    note: &str,
) -> Option<String> {
    if let Some(part) = absent {
        return Some(format!(
            "{subject} net '{net}' has no fitted pull-up to a rail: {part} is the only \
             pull-up and is marked {DNP_PULLUP_MESSAGE_MARKER}, so it is not counted and as \
             built the line floats ({note})"
        ));
    }
    policy_fitted.map(|part| {
        format!(
            "{subject} net '{net}' relies on {part} for its pull-up, but {part} is marked \
             {DNP_PULLUP_MESSAGE_MARKER}: as assembled, the line floats \
             ({DNP_FITTED_NOTE_MARKER}, so other results may look fine)"
        )
    })
}

/// The I2C line the symbol pins of a net state when its own name does not: a
/// present, non-connector part's pin that is only `SDA` or `SCL`
/// ([`dedicated_i2c_pin`]). Pins that disagree (one `SDA`, one `SCL`) state
/// nothing. Returns the line and the pin that says so (`U2 pin 1 'SDA'`).
fn i2c_role_from_pins(mem: &[(&Component, &Pin)]) -> Option<(&'static str, String)> {
    let mut found: Option<(&'static str, String)> = None;
    for (c, p) in mem {
        if !AssemblyState::of(c).is_present() || is_connector_like(c) {
            continue;
        }
        let role = match dedicated_i2c_pin(&p.function) {
            Some(PeripheralFunction::I2cSda) => "SDA",
            Some(PeripheralFunction::I2cScl) => "SCL",
            _ => continue,
        };
        match &found {
            Some((r, _)) if *r != role => return None,
            Some(_) => {}
            None => {
                found = Some((
                    role,
                    format!("{} pin {} '{}'", c.reference, p.number, p.function.trim()),
                ));
            }
        }
    }
    found
}

fn check_i2c_pullups(board: &ExtractedBoard, report: &mut NetLintReport) {
    for net in &board.nets {
        if net.id == 0 || is_unconnected_net(&net.name) {
            continue;
        }
        let mem = members(board, net.id);
        // The net's own name first; a net named for something else (`SENSOR_D`,
        // `Net-(R4-Pad1)`) is still the bus when the pins on it say so.
        let (role, by_pins) = match i2c_role(&net.name) {
            Some(role) => (role, None),
            None => match i2c_role_from_pins(&mem) {
                Some((role, pin)) => (role, Some(pin)),
                None => continue,
            },
        };
        report.findings.extend(i2c_net_finding(
            board,
            net,
            &mem,
            role,
            by_pins.as_deref(),
            &|_, p| !p.function.trim().is_empty(),
        ));
    }
}

/// The missing-pull-up finding for a net a model card says is an I2C line.
///
/// `role` is `"SDA"` or `"SCL"` and `evidence` names the card pad that says so
/// (`U2 pin 1 'SDA'`). It judges the net exactly as the name- and pin-named buses are judged, and
/// is silent on a net that is not a bus, has a pull-up, or has a part or end
/// it cannot read. `readable` says whether a part's pin on the bus can be read
/// (it has a symbol pin name, or a card for the pad).
#[must_use]
pub fn i2c_pullup_finding_for_net(
    board: &ExtractedBoard,
    net_id: i64,
    role: &str,
    evidence: &str,
    readable: &dyn Fn(&Component, &Pin) -> bool,
) -> Option<LintFinding> {
    let net = board.net(net_id)?;
    if net.id == 0 || is_unconnected_net(&net.name) {
        return None;
    }
    let role = if role.eq_ignore_ascii_case("SCL") {
        "SCL"
    } else {
        "SDA"
    };
    i2c_net_finding(
        board,
        net,
        &members(board, net_id),
        role,
        Some(evidence),
        readable,
    )
}

/// What the members of a net say about the bus on it.
struct BusSurvey {
    has_pullup: bool,
    ends: BusEnds,
    /// A part on the bus cannot be read: its identity is in dispute, or its pin
    /// on the bus has no name.
    unread: bool,
}

/// Survey the members of `net_id` for the pull-up they supply and where the bus ends.
///
/// `fitted` is whether a fitted resistor already pulls the line up.
fn survey_bus(
    board: &ExtractedBoard,
    net_id: i64,
    mem: &[(&Component, &Pin)],
    fitted: bool,
    readable: &dyn Fn(&Component, &Pin) -> bool,
) -> BusSurvey {
    let mut has_pullup = fitted;
    // Dedup active devices by reference: an IPC-356 both-sided through-hole
    // access record lists a device's bus pad twice, so a raw per-entry count
    // turned a single-device (ambiguous → skip) bus into `active_devices == 2`
    // and fired a false "on-board master and peripheral, no pull-up" finding.
    let mut active_refs: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut exits_to_connector = false;
    let mut via_module_socket = false;
    let mut via_dnp_end = false;
    // Pin-named buses only: a part on the bus that cannot be read (its
    // identity is in dispute, or its pin on the bus has no name).
    let mut unread = false;

    for (c, p) in mem {
        // Only assembled, identity-trusted parts count: an absent device
        // must not inflate the bus. A socket or header marked DNP is the
        // exception: it stands for the module or cable that mates with it
        // later, so it is still the bus's far end.
        match AssemblyState::of(c) {
            AssemblyState::Present(_) => {}
            AssemblyState::DnpAbsent(_) => {
                if is_dnp_bus_end(c) {
                    exits_to_connector = true;
                    via_module_socket |= is_module_socket(c);
                    via_dnp_end = true;
                }
                continue;
            }
            AssemblyState::IdentityUnknown(_) => {
                unread = true;
                continue;
            }
        }
        // A level translator with integrated pull-ups (NTS010x / TXS010x /
        // PCA9517-class auto-direction parts) supplies the bus
        // pull-up itself: an external resistor is optional, not missing. A
        // pure pass-gate (PCA9306) does NOT, so it is excluded from this.
        if has_integrated_i2c_pullups(c) {
            has_pullup = true;
        }
        if is_connector_like(c) {
            exits_to_connector = true;
            via_module_socket |= is_module_socket(c);
            continue;
        }
        // A transistor current source (or switched pull-up) is the pull-up,
        // not a device on the bus.
        if is_transistor_pullup(board, c, net_id) {
            has_pullup |= !crate::dnp::fitted_from_dnp_policy(c);
            continue;
        }
        // An IC / sensor pin on the bus means the bus is used on-board. A
        // resistor ARRAY is a passive pull-up, not an active device, exclude
        // it so it does not inflate `active_devices` and manufacture the
        // two-device "missing pull-up" case.
        let r = c.reference.to_ascii_uppercase();
        if r.starts_with('U') || (c.pins.len() > 2 && !is_resistor(c) && !is_resistor_array(c)) {
            active_refs.insert(c.reference.as_str());
            unread |= !readable(c, p);
        }
    }
    BusSurvey {
        has_pullup,
        ends: BusEnds {
            via_module_socket,
            exits_to_connector,
            via_dnp_end,
            active_devices: active_refs.len(),
        },
        unread,
    }
}

/// One net's missing-pull-up finding, when it is the I2C line `role`.
///
/// `by_pins` is the evidence when the net's name does not say so.
fn i2c_net_finding(
    board: &ExtractedBoard,
    net: &Net,
    mem: &[(&Component, &Pin)],
    role: &str,
    by_pins: Option<&str>,
    readable: &dyn Fn(&Component, &Pin) -> bool,
) -> Option<LintFinding> {
    {
        // A pin that spells an I2C line proves nothing on a net called for SPI
        // or a clock: a display's serial pins are often named `SCL` and `SDA`.
        if by_pins.is_some() && names_spi_line(&net.name) {
            return None;
        }
        // A real two-wire bus has at least two members (master + peripheral, or
        // a peripheral + the pull). A single-member "SDA" is a stub / NC pad.
        if mem.len() < 2 {
            return None;
        }
        let scan = pullup_scan(board, net.id, None);
        let survey = survey_bus(board, net.id, mem, scan.fitted, readable);
        if survey.has_pullup {
            return None;
        }
        let BusSurvey { ends, unread, .. } = survey;
        let active_devices = ends.active_devices;
        let regime = if let Some(pin) = by_pins {
            // A bus found by its pins alone is judged only where nothing about
            // it is unknown: a connector or socket (the pull may be off-board)
            // and a part that cannot be read abstain.
            if ends.exits_to_connector || unread || active_devices < 2 {
                return None;
            }
            Some((
                Severity::High,
                format!(
                    "on-board master and peripheral, no pull-up present; the net is the bus \
                     because of {pin}"
                ),
            ))
        } else {
            pullup_regime(&ends).map(|(sev, note)| (sev, note.to_string()))
        };
        // A pull-up that is do-not-populate is not a pull-up on the built board.
        // Where the regime would otherwise abstain (a single on-board device
        // and no header), its presence in the layout is the evidence that the
        // designer meant the line to be pulled up, so it is reported at Low.
        let Some((sev, note)) = regime.or_else(|| {
            scan.absent_ref.is_some().then_some((
                Severity::Low,
                "only one device sits on the bus, so the pull-up may be fitted elsewhere"
                    .to_string(),
            ))
        }) else {
            // A single on-board device with no header and no pull: ambiguous,
            // skip rather than risk a false positive.
            return None;
        };

        if let Some(message) = dnp_pullup_message(
            &format!("I2C {role}"),
            &net.name,
            scan.absent_ref.as_deref(),
            scan.dnp_ref.as_deref(),
            &note,
        ) {
            return Some(LintFinding {
                check: LintCheck::MissingI2cPullup,
                severity: sev,
                message,
                refs: present_refs(mem),
                nets: vec![net.name.clone()],
            });
        }

        let remedy = if by_pins.is_some() {
            format!(
                " {REMEDY_MARKER}Add a resistor (2.2 to 10 kilohms, by bus speed) from '{}' to \
                 the rail the devices run from.",
                net.name
            )
        } else {
            String::new()
        };
        Some(LintFinding {
            check: LintCheck::MissingI2cPullup,
            severity: sev,
            message: format!(
                "I2C {role} net '{}' has no pull-up to a rail: {note}{remedy}",
                net.name
            ),
            refs: mem.iter().map(|(c, _)| c.reference.clone()).collect(),
            nets: vec![net.name.clone()],
        })
    }
}

// ---------------------------------------------------------------------------
// Check 1b: missing SD-card bus pull-ups.
// ---------------------------------------------------------------------------

/// A net is an SD/eMMC card bus line if its name carries a card-bus context
/// token (SD, USD, MICROSD, TF, SDIO, SDMMC, SDCARD, MMC, EMMC) *and* names
/// one of the lines the SD physical-layer spec requires a host pull-up on:
/// CMD, or DAT0..DAT7 in any of the common spellings (`sd_dat0`, `sd_data_0`,
/// `sd_d2`, `emmc_d7`, ...). CLK is push-pull driven and never requires one,
/// so it is not matched. The context requirement means a bare "CMD" net (a
/// motor driver command line, a modem AT channel) can never fire this check.
///
/// The context token is searched in the FULL net name including its
/// hierarchical sheet path, because KiCad puts the context there for local
/// labels (`/uSD Connector/CMD`); the role token is taken from the leaf only,
/// so a sheet merely *named* after the card cannot turn an unrelated leaf
/// signal into a bus line.
///
/// Name context alone is deliberately not enough to fire the check: SDRAM buses
/// are conventionally named `SD_D0..SD_D15` too, so `check_sd_pullups` also
/// requires an actual card socket on the net.
fn sd_role(name: &str) -> Option<&'static str> {
    const DAT: [&str; 8] = [
        "DAT0", "DAT1", "DAT2", "DAT3", "DAT4", "DAT5", "DAT6", "DAT7",
    ];
    // Context: tokens of the whole name, sheet path included. A bus-index
    // suffix ("SDMMC1", "SDIO2") still names the same controller.
    let full = name.trim().to_ascii_uppercase();
    let ctx = full
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .any(|t| {
            let t = t.trim_end_matches(|c: char| c.is_ascii_digit());
            matches!(
                t,
                "SD" | "USD"
                    | "MICROSD"
                    | "TF"
                    | "SDIO"
                    | "SDMMC"
                    | "SDCARD"
                    | "MMC"
                    | "EMMC"
                    | "USDHC"
                    | "SDHC"
            )
        });
    if !ctx {
        return None;
    }
    // Role: tokens of the leaf only.
    let n = norm(name);
    let toks: Vec<&str> = n
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    if toks.contains(&"CMD") {
        return Some("CMD");
    }
    let dat_index = |t: &str| -> Option<usize> {
        for stem in ["DATA", "DAT", "D"] {
            if let Some(rest) = t.strip_prefix(stem) {
                if rest.len() == 1 && rest.chars().all(|c| c.is_ascii_digit()) {
                    let i = rest.parse::<usize>().ok()?;
                    if i <= 7 {
                        return Some(i);
                    }
                }
            }
        }
        None
    };
    for (i, t) in toks.iter().enumerate() {
        // Combined token: "DAT0" / "DATA2" / "D3".
        if let Some(idx) = dat_index(t) {
            return Some(DAT[idx]);
        }
        // Split spelling: a "DAT"/"DATA" token followed by a bare digit token
        // ("sd_data_0" tokenizes to SD, DATA, 0).
        if matches!(*t, "DAT" | "DATA") {
            if let Some(next) = toks.get(i + 1) {
                if next.len() == 1 {
                    if let Ok(idx) = next.parse::<usize>() {
                        if idx <= 7 {
                            return Some(DAT[idx]);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Does this component's NAME identify it as the card socket? Checked against
/// the footprint, library id, and value, which is where real designs carry
/// it: KiCad's official card sockets live in `Connector_Card`, vendor
/// footprints are named things like `microSD_HC_Molex_47219-2001`, Hirose
/// sockets ship under their bare `DM3AT`/`DM3D` part numbers, and the
/// JLCPCB/EasyEDA default microSD part is the `TF-01A`/`TF-01B` family.
fn is_card_socket_by_name(c: &Component) -> bool {
    let hay = format!("{} {} {}", c.footprint, c.lib_id, c.value).to_ascii_lowercase();
    [
        "microsd",
        "micro_sd",
        "micro-sd",
        "micro sd",
        "sdcard",
        "sd_card",
        "sd-card",
        "sd card",
        "tf_card",
        "tf-card",
        "tfcard",
        "tf-01",
        "tf_01",
        "tf01",
        "memorycard",
        "memory_card",
        "connector_card",
        "sd_socket",
        "usd_",
        "emmc_socket",
        "dm3at",
        "dm3bt",
        "dm3d",
        "dm1aa",
    ]
    .iter()
    .any(|p| hay.contains(p))
}

/// Does this connector behave like the card socket structurally, whatever its
/// footprint is called? This backstop exists because socket naming in the
/// wild is unbounded (bare vendor part numbers, house libraries), and a
/// name-whitelist alone silently exempts every board it misses. The bar is
/// deliberately higher than "touches several card-bus nets", because a pin
/// header, FPC, mezzanine, or SODIMM that breaks the whole bus out to
/// another board ALSO touches several; classifying one of those as the
/// socket would both fire on socket-less boards and disable the
/// bus-leaves-the-board escape. Structural recognition therefore requires
/// all three of: CMD among the roles (an SDRAM bus has DAT-like lines but
/// never CMD), at least three distinct roles, and a package that is not a
/// header / pin array / board-to-board carrier.
fn is_card_socket_structurally(board: &ExtractedBoard, c: &Component) -> bool {
    let fp = format!("{} {}", c.footprint, c.lib_id).to_ascii_lowercase();
    if is_pin_array_package(&c.footprint)
        || [
            "header",
            "pinhead",
            "fpc",
            "ffc",
            "b2b",
            "board-to-board",
            "sodimm",
            "dimm",
            "edge",
            "df40",
            "df12",
            "fh12",
            "fh34",
            "jst",
            "slimstack",
            "mezz",
            "m.2",
            "m2_",
            "minipcie",
            "sim",
        ]
        .iter()
        .any(|p| fp.contains(p))
    {
        return false;
    }
    // Shape gate that needs no name knowledge at all: a microSD/SD/eMMC
    // socket has 8-13 connected pads. A 24-pin flex, a 30-pin mezzanine, or a
    // SODIMM carrying the bus off-board is far wider, whatever its footprint
    // is called; those must default to "the bus exits", not to "socket".
    if connected_pads(c) > 16 {
        return false;
    }
    let mut roles = std::collections::HashSet::new();
    for p in &c.pins {
        let Some(net) = p.net.and_then(|id| board.net(id)) else {
            continue;
        };
        if let Some(role) = sd_role(&net.name) {
            roles.insert(role);
        } else {
            // CLK counts toward the structural signature (it is excluded from
            // sd_role only because it needs no pull-up).
            let leaf = norm(&net.name);
            let full = net.name.to_ascii_uppercase();
            if (leaf.ends_with("CLK") || leaf.ends_with("CLOCK") || leaf.ends_with("CK"))
                && (full.contains("SD") || full.contains("MMC") || full.contains("TF"))
            {
                roles.insert("CLK");
            }
        }
    }
    roles.contains("CMD") && roles.len() >= 3
}

/// A test point or mounting hole is a single-point attachment, not a bus
/// leaving the board; it must not count as "the bus exits to a connector".
#[must_use]
pub fn is_test_point_or_hole(c: &Component) -> bool {
    let r = c.reference.to_ascii_uppercase();
    let lib = c.lib_id.to_ascii_lowercase();
    r.starts_with("TP")
        || r.starts_with("MH")
        || r.starts_with("MK")
        || lib.contains("testpoint")
        || lib.contains("mountinghole")
}

/// A soldered-down eMMC (the "card" as a BGA on this same board). Matched on
/// the value only, against the unambiguous vendor prefixes; the JEDEC pull-up
/// requirement applies to it exactly as to a removable card.
fn is_emmc_device(c: &Component) -> bool {
    let v = c.value.to_ascii_uppercase();
    v.contains("EMMC")
        || [
            "KLM", "THGBM", "THGAF", "THGAM", "MTFC", "H26M", "SDINB", "SDINA",
        ]
        .iter()
        .any(|p| v.starts_with(p))
}

/// SD-bus level translators / port expanders that integrate their own
/// pull-ups (`TXS0206x`, the dedicated SD-card translator, and TXS02612), and
/// pass-gate card muxes through which a host-side pull-up conducts (FSA2567,
/// NVT4857, TS3A27518, NLSX/PI4ULS-class shifters). Either way, a net behind
/// one of these is beyond single-net static reasoning: skip rather than
/// report a pull-up "missing" that the switch supplies or forwards.
fn is_sd_bus_switch(c: &Component) -> bool {
    let v = c.value.to_ascii_uppercase();
    [
        "TXS0206",
        "TXS0208",
        "TXS02612",
        "FSA2567",
        "NVT4857",
        "TS3A27518",
        "NLSX",
        "PI4ULS",
    ]
    .iter()
    .any(|p| v.contains(p))
}

/// ESD/TVS protection arrays: multi-pin parts that sit ON the bus next to a
/// socket without being a host. Counting one as "on-board host silicon"
/// defeats the no-host breakout escape on exactly the board type where a
/// protection array is guaranteed to be present.
fn is_protection_part(c: &Component) -> bool {
    let v = c.value.to_ascii_uppercase();
    v.contains("ESD")
        || v.contains("TVS")
        || [
            "TPD", "PESD", "USBLC", "SRV05", "IP42", "SP05", "PRTR", "SMF", "NUP", "RCLAMP",
            "CM12", "CM14",
        ]
        .iter()
        .any(|p| v.starts_with(p))
}

/// Does this component qualify as on-board host silicon for a card bus?
/// Protection arrays and the card device itself (a down eMMC) do not: an
/// eMMC module or an ESD-protected passive adapter has no host on board, and
/// its pull-ups legitimately live on the carrier.
fn is_host_silicon(c: &Component) -> bool {
    if is_protection_part(c) || is_emmc_device(c) {
        return false;
    }
    let r = c.reference.to_ascii_uppercase();
    r.starts_with('U') || (c.pins.len() > 2 && !is_resistor(c) && !is_resistor_array(c))
}

/// The net name carries an SPI signal-role word (`SPI`, `MOSI`, `MISO`,
/// `COPI`, `CIPO`, `PICO`, `POCI`, optionally with a bus index), as a
/// dual-named SD line like `SPI_CIPO1/SDIO_DAT0` does. `SDI`/`SDO` are left
/// out: they read as SD-card words too. A hierarchical KiCad name is judged by
/// its leaf, so a sheet named for an SPI flash does not count.
fn names_spi_role(name: &str) -> bool {
    let leaf = if name.starts_with('/') {
        name.rsplit('/').next().unwrap_or(name)
    } else {
        name
    };
    let upper = leaf.to_ascii_uppercase();
    (upper.split(|c: char| !c.is_ascii_alphanumeric())).any(|word| {
        let word = word.trim_end_matches(|c: char| c.is_ascii_digit());
        matches!(
            word,
            "SPI" | "MOSI" | "MISO" | "COPI" | "CIPO" | "PICO" | "POCI"
        )
    })
}

#[allow(clippy::too_many_lines, reason = "one pass over SD socket nets")]
fn check_sd_pullups(board: &ExtractedBoard, report: &mut NetLintReport) {
    for net in &board.nets {
        if net.id == 0 || is_unconnected_net(&net.name) {
            continue;
        }
        let Some(role) = sd_role(&net.name) else {
            continue;
        };
        let mem = members(board, net.id);
        // A single-member SD net is a stub / NC pad, not a bus.
        if mem.len() < 2 {
            continue;
        }

        // Sub-1 kΩ values are series dampers, not pull-ups.
        let scan_for_pullup = |net_id: i64| pullup_scan(board, net_id, Some(1_000.0));
        let PullupScan {
            fitted: mut has_pullup,
            dnp_ref: mut dnp_pullup_ref,
            absent_ref: mut absent_pullup_ref,
            mut unjudgeable,
        } = scan_for_pullup(net.id);
        let mut has_socket = false;
        let mut socket_parts: Vec<&Component> = Vec::new();
        let mut has_emmc = false;
        let mut behind_bus_switch = false;
        let mut exits_to_other_connector = false;
        let mut active_refs: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (c, _p) in &mem {
            // Only assembled, identity-trusted parts participate in the
            // topology decision.
            if !AssemblyState::of(c).is_present() {
                continue;
            }
            if is_test_point_or_hole(c) {
                // A single-point attachment, not the bus leaving the board.
                continue;
            }
            if is_connector_like(c) {
                // A card socket is the positive evidence a card bus exists; a
                // second, non-socket connector means the bus leaves the board.
                if is_card_socket_by_name(c) || is_card_socket_structurally(board, c) {
                    has_socket = true;
                    socket_parts.push(*c);
                } else {
                    exits_to_other_connector = true;
                }
                continue;
            }
            if is_emmc_device(c) {
                // A soldered-down eMMC is the card; JEDEC places the same
                // pull-up requirement on its bus.
                has_emmc = true;
            }
            if is_sd_bus_switch(c) || has_integrated_i2c_pullups(c) {
                // A pass-gate mux, or a TXS-class translator whose every
                // I/O carries its own ~10 k pull-up (they get dropped onto
                // SD/SDIO lines too): either way, a discrete resistor is not
                // the whole story on this net.
                behind_bus_switch = true;
            }
            if is_host_silicon(c) {
                active_refs.insert(c.reference.as_str());
            }
            // One-hop traversal through a series element: SD buses routinely
            // interpose a small damper or a ferrite between host and socket. The
            // far half-net legitimately carries the pull-up, the host, or the
            // connector the bus exits through, so all three kinds of evidence
            // cross the hop; socket evidence deliberately does not, so only the
            // socket-side net reports and a split bus cannot double-report one
            // defect. A two-terminal resistor below 1 kΩ, or an FB/L-referenced
            // two-pin part that is not an LED, qualifies as the series element.
            // A hop into ground or a NAMED rail is refused, since everything
            // touches ground and an unrelated bleeder would count as this line's
            // pull-up; the refusal deliberately does not use the structural
            // bypass-cap rail test, a host pin with an EMI cap to ground being a
            // signal node that must still be searched. An unparseable resistor
            // value also qualifies for the hop: refusing both the hop and the
            // credit would fail toward silence twice on one unknown.
            let two_pin_series = c.pins.len() == 2
                && ((is_resistor(c) && parse_ohms(&c.value).is_none_or(|o| o < 1_000.0))
                    || matches!(
                        RefClass::of(&c.reference),
                        RefClass::FerriteBead | RefClass::Inductor
                    ));
            if two_pin_series {
                for op in &c.pins {
                    match op.net {
                        Some(oid) if oid != net.id => {
                            let far_named_ground_or_rail = board
                                .net(oid)
                                .is_some_and(|n| is_ground(&n.name) || is_supply(&n.name));
                            if far_named_ground_or_rail {
                                continue;
                            }
                            let far = scan_for_pullup(oid);
                            has_pullup |= far.fitted;
                            unjudgeable |= far.unjudgeable;
                            if dnp_pullup_ref.is_none() {
                                dnp_pullup_ref = far.dnp_ref;
                            }
                            if absent_pullup_ref.is_none() {
                                absent_pullup_ref = far.absent_ref;
                            }
                            for (fc, _fp) in members(board, oid) {
                                if !AssemblyState::of(fc).is_present() || is_test_point_or_hole(fc)
                                {
                                    continue;
                                }
                                if is_connector_like(fc) {
                                    // Mirror the near-side classification: a
                                    // non-socket connector one damper away is
                                    // still the bus leaving the board.
                                    if !(is_card_socket_by_name(fc)
                                        || is_card_socket_structurally(board, fc))
                                    {
                                        exits_to_other_connector = true;
                                    }
                                    continue;
                                }
                                if is_sd_bus_switch(fc) || has_integrated_i2c_pullups(fc) {
                                    behind_bus_switch = true;
                                }
                                if is_host_silicon(fc) {
                                    active_refs.insert(fc.reference.as_str());
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        if has_pullup {
            continue;
        }
        // A resistor with an unreadable value sits exactly where a pull-up
        // would: the net cannot be judged, in either direction.
        if unjudgeable {
            continue;
        }
        // A pass-gate mux or integrated-pull-up translator on the net puts
        // the bias topology beyond single-net static reasoning (the switch
        // forwards a host-side pull-up, or supplies its own): abstain rather
        // than report a pull-up missing that the switch provides.
        if behind_bus_switch {
            continue;
        }
        // No card socket and no soldered-down eMMC on the net: the name alone
        // is not evidence. SDRAM buses are named SD_D0.. too, and an MCU that
        // routes SD lines to a plain header may rely on the attached module's
        // pull-ups.
        if !has_socket && !has_emmc {
            continue;
        }
        // The bus also reaches another connector: an adapter/breakout that
        // passes it through to a host board, where the pull-ups legitimately
        // live. Ambiguous, skip. (An eMMC module is exactly this shape too:
        // the eMMC does not count as host silicon, so it is also rescued by
        // the no-host escape below.)
        if exits_to_other_connector {
            continue;
        }
        // Card evidence but no on-board host silicon at all: same
        // adapter/module story, whatever the bus is called.
        if active_refs.is_empty() {
            continue;
        }

        let refs = present_refs(&mem);

        // SPI-mode heuristic: a socket wired for SPI leaves DAT1/DAT2 out of
        // the host's hands, and in SPI mode the CMD pin is the host's
        // push-pull DI, so the SD-mode open-drain rationale would be false
        // for that board. The test is whether HOST SILICON reaches DAT1/DAT2,
        // not whether those nets exist: an SPI design that dutifully parks
        // DAT1/DAT2 on pull-ups (what the spec asks for) must not be promoted
        // to SD-mode severity for doing the right thing. The spec still
        // recommends the pull-ups, so this softens severity and wording
        // rather than going silent.
        let spi_named = has_socket && names_spi_role(&net.name);
        let spi_suspect = has_socket
            && socket_parts.iter().all(|s| {
                !s.pins.iter().any(|p| {
                    let Some(nid) = p.net else { return false };
                    let is_dat12 = board
                        .net(nid)
                        .and_then(|n| sd_role(&n.name))
                        .is_some_and(|r| r == "DAT1" || r == "DAT2");
                    is_dat12
                        && members(board, nid).iter().any(|(mc, _)| {
                            AssemblyState::of(mc).is_present() && is_host_silicon(mc)
                        })
                })
            });

        // CMD and DAT0 are exercised by every card in every bus mode; a
        // missing pull-up there stalls identification or busy detection.
        // DAT1..DAT7 are spec-required but conventionally omitted on buses
        // run narrower than full width, so they report Low rather than
        // Medium.
        let (sev, why) = if spi_suspect {
            (
                Severity::Low,
                "this socket appears wired for SPI mode (DAT1/DAT2 unused); the SD spec \
                 still recommends the pull-up (in SPI mode CMD is the host's DI and DAT0 \
                 the card's DO, which floats while the card is deselected)",
            )
        } else if spi_named {
            // The net is named for an SPI role too: the design runs (or can
            // run) the card in SPI mode, where CMD is the host's push-pull DI
            // and the SD-mode open-drain rationale does not hold.
            (
                Severity::Low,
                "this socket appears wired for SPI mode (the net is named for an SPI role); \
                 the SD spec still recommends the pull-up (in SPI mode CMD is the host's DI \
                 and DAT0 the card's DO, which floats while the card is deselected)",
            )
        } else {
            match role {
                "CMD" => (
                    Severity::Medium,
                    "CMD is driven open-drain during card identification; without a pull-up \
                     initialization can hang before firmware sees a card at all",
                ),
                "DAT0" => (
                    Severity::Medium,
                    "DAT0 carries the card's response and busy signalling; it floats whenever \
                     the card is not driving it",
                ),
                _ => (
                    Severity::Low,
                    "required by the SD/eMMC spec, though conventionally omitted when the bus \
                     runs narrower than its full width; MCU internal pull-ups, if enabled, only \
                     cover the window after firmware configures them",
                ),
            }
        };

        // A pull-up that exists in the layout but is DNP, whether left out of
        // the analysis or present in simulation only because the fit policy
        // assumes it will be placed, is its own finding AT THE SAME SEVERITY
        // the absent-part case earns: the board as assembled floats either
        // way, and a policy flag that exists as a simulation convenience must
        // not decide whether CI gates on a fab-real defect. The finding names
        // the part, so a reader who sees a resistor drawn on the line is not
        // told it is missing.
        if let Some(message) = dnp_pullup_message(
            &format!("SD card {role}"),
            &net.name,
            absent_pullup_ref.as_deref(),
            dnp_pullup_ref.as_deref(),
            why,
        ) {
            report.findings.push(LintFinding {
                check: LintCheck::MissingSdPullup,
                severity: sev,
                message,
                refs,
                nets: vec![net.name.clone()],
            });
            continue;
        }

        report.findings.push(LintFinding {
            check: LintCheck::MissingSdPullup,
            severity: sev,
            message: format!(
                "SD card {role} net '{}' has no pull-up to a rail: {why}",
                net.name
            ),
            refs,
            nets: vec![net.name.clone()],
        });
    }
}

// ---------------------------------------------------------------------------
// Check 2: floating active-control pins.
// ---------------------------------------------------------------------------

/// Does this pin function name denote a *dedicated* active-control input whose
/// level must be defined at power-up (a chip enable, reset, shutdown, ...)?
///
/// Deliberately strict to avoid matching multiplexed GPIO signal names that
/// merely contain "EN"/"CS" as a substring (e.g. an Ethernet RMII `TX_EN`
/// strobe, a `GPIO21/EMAC_TX_EN` mux pin, a `SENSE` line, an `OPEN` flag). We
/// match only when the function name *is* a recognised control pin, after
/// stripping active-low decoration. A name carrying a peripheral/mux prefix
/// (containing '/', or a known bus/peripheral keyword) is treated as a signal,
/// not a control pin.
#[must_use]
pub fn control_role(function: &str) -> Option<&'static str> {
    // A multiplexed / signal name (Ethernet, SPI, GPIO mux, etc.) is not a
    // dedicated control pin even if a sub-token looks like "EN".
    const SIGNAL_KEYWORDS: [&str; 12] = [
        "GPIO", "EMAC", "RMII", "TX_EN", "RX_EN", "CLKEN", "VSPI", "HSPI", "UART", "PWM", "SENSE",
        "OPEN",
    ];
    let f = function.trim().to_ascii_uppercase();
    if f.is_empty() {
        return None;
    }
    if SIGNAL_KEYWORDS.iter().any(|k| f.contains(k)) {
        return None;
    }
    // A '/'-bearing name is a mux alias list ("D2/A1/SCL"); not a control pin.
    if f.contains('/') && !f.starts_with('/') {
        return None;
    }
    // Strip active-low decoration: ~{RST}, /RESET, RST#, nRST, RST_N, RESETN.
    let core: String = f
        .trim_start_matches('~')
        .trim_start_matches('/')
        .replace(['~', '{', '}', '/', '#'], "");
    // Strip a leading 'N' (nRST) and trailing '_N' active-low suffix.
    let core = core.trim_start_matches('N').to_string();
    let core = core.trim_end_matches("_N").to_string();
    let c = core.trim();

    // Exact / canonical matches only. Bare trailing-N active-low reset forms
    // (RSTN / RESETN) are listed explicitly rather than stripped: a blanket
    // trailing-'N' strip would maim the no-N control names (EN -> E, SHDN -> SHD),
    // so the doc comment's "RESETN" promise is kept with dedicated arms instead.
    match c {
        // CHIP_PU / CHIP_EN: Espressif's name for the chip enable.
        "EN" | "ENABLE" | "CE" | "CEN" | "SHDN" | "SHUTDOWN" | "NSHDN" | "CHIP_PU" | "CHIP_EN" => {
            Some("enable")
        }
        "RST" | "RESET" | "RSTN" | "RESETN" | "MR" | "RESE" => Some("reset"), // RESE = RESET after _N strip
        "CS" | "SS" | "NCS" | "NSS" | "CSB" | "CSN" | "SSN" => Some("chip-select"),
        "OE" | "NOE" => Some("output-enable"),
        "BOOT" | "BOOT0" | "BOOT1" => Some("boot-mode strap"),
        _ => {
            // Allow a trailing rail-name suffix on EN/RST: "EN_3V3", "RST_MCU".
            let head = c.split('_').next().unwrap_or(c);
            match head {
                "EN" => Some("enable"),
                "RST" | "RESET" | "RSTN" | "RESETN" => Some("reset"),
                _ => None,
            }
        }
    }
}

/// A part whose control pins the floating-control lint judges.
///
/// An IC (`U`, `IC`, `MCU`) or a transistor (`Q`). The floating-inputs board check reads
/// the same predicate so the two never report one pin twice.
#[must_use]
pub fn is_logic_or_switch_ref(c: &Component) -> bool {
    matches!(
        RefClass::of(&c.reference),
        RefClass::ActiveIc | RefClass::Transistor
    )
}

fn check_floating_control_pins(board: &ExtractedBoard, report: &mut NetLintReport) {
    // Fitted logic/active parts only: an unassembled IC has no pin to float, a
    // refused record's functions are not evidence, and "EN" means nothing on a
    // connector or passive.
    let active = |c: &&Component| {
        AssemblyState::of(c).is_present() && !is_connector_like(c) && is_logic_or_switch_ref(c)
    };
    for c in board.components.iter().filter(active) {
        for p in &c.pins {
            // A deliberate no-connect (pintype or `unconnected-(...)` net) is the
            // designer's choice. Floating == a *named* net touching only this pin;
            // a None net is usually an unrouted pad the layout will fill, too
            // noisy on PCB-only inputs.
            let net = p.net.and_then(|id| board.net(id));
            let (Some(role), false, Some(n)) =
                (control_role(&p.function), pin_is_no_connect(p), net)
            else {
                continue;
            };
            if is_unconnected_net(&n.name) || members(board, n.id).len() != 1 {
                continue;
            }
            report.findings.push(LintFinding {
                check: LintCheck::FloatingControlPin,
                severity: Severity::High,
                message: format!(
                    "{} pin {} ({}) is a floating {role} input: net '{}' touches only this pin (no driver or pull)",
                    c.reference, p.number, p.function, n.name
                ),
                refs: vec![c.reference.clone()],
                nets: vec![n.name.clone()],
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Check 3 (retired): LED current. The one LED-current rule is
// `hauksbee_checks::checks::led_current`, which reads the LED's own card and
// the rails' resolved levels; the fixed-Vf estimate that lived here is gone.
// ---------------------------------------------------------------------------

/// Parse a resistor value string ("330", "1k", "4k7", "1.2K", "0R") to ohms via
/// the single canonical parser in `hauksbee-models`.
///
/// The canonical parser handles µ/Ω/ohm-sign glyphs, MEG/GIG, milli-`m`, the
/// R/K/M-decimal form, "/footprint" qualifiers, chip-size codes and trailing
/// tolerance annotations in one tested place; a hand-rolled copy drifts from it
/// (lowercase-`m` read as MEG is a 1e9 error). Accept only an ohmic magnitude
/// (no unit, or an explicit Ω) so a stray farad/volt value still reads as "not
/// a resistor".
pub fn parse_ohms(v: &str) -> Option<f64> {
    hauksbee_models::value::parse_value(v)
        .filter(|p| matches!(p.unit.as_deref(), None | Some("Ω")))
        .map(|p| p.si)
}

// ---------------------------------------------------------------------------
// Check 4: output-vs-output contention (schematic-stage ERC).
// ---------------------------------------------------------------------------
//
// Two distinct parts each driving the same net with a *push-pull* output, with
// nothing on the net that can resolve the fight (no series resistor, no
// tri-state/open-drain/bidirectional pin, no input that reframes the net as a
// driven-input), is a wiring error: at any instant the two outputs can command
// opposite levels and source/sink destructive cross-current.
//
// This is built purely from the KiCad pin electrical type (`Pin.kind`), which
// only the schematic / netlist extraction paths populate. It is calibrated to be
// SILENT on the full known-good schematic corpus; the one raw fire there is an
// open-drain interrupt line whose pins a symbol author typed `output`, excluded
// by the open-drain-name and tiebreaker rules below. The sibling "undriven
// input" check is deliberately absent: on the same corpus it could not be
// calibrated to zero false positives.

/// A push-pull driver type. `tri_state` / `open_collector` / `open_emitter` are
/// deliberately NOT here: those are wired-OR safe and resolve contention.
fn is_pushpull_output(kind: &str) -> bool {
    matches!(
        kind.trim().to_ascii_lowercase().as_str(),
        "output" | "power_out"
    )
}

/// A member that can legitimately resolve two outputs sharing a net: a passive
/// (series R / mux), a tri-state / open-drain / open-emitter pin, or an input/
/// bidirectional pin (which reframes the net as a driven input the symbol author
/// modelled as also-output, the common IRQ / shared-IO case). Its presence means
/// "do not fire" - we only flag a net whose ONLY drivers are bare push-pull
/// outputs of different parts.
fn resolves_contention(kind: &str) -> bool {
    matches!(
        kind.trim().to_ascii_lowercase().as_str(),
        "passive" | "tri_state" | "open_collector" | "open_emitter" | "input" | "bidirectional"
    )
}

/// A net whose name marks it as an open-drain / wired-OR line (interrupt, alert,
/// ready/busy, NMI). Symbol authors routinely type these pins `output` though
/// they are open-drain pulled high; two such pins on one net is the intended
/// wired-OR, not contention. Excluded by name so the check stays at zero false
/// positives.
fn is_wired_or_name(name: &str) -> bool {
    const KEYS: [&str; 11] = [
        "IRQ", "INT", "ALERT", "RDY", "READY", "BUSY", "NMI", "PG", "PGOOD", "FAULT", "NFLT",
    ];
    // Whole-WORD match: split on non-alphanumerics and compare each token, not the
    // raw string. A bare `contains("INT")` fired inside SETPOINT / PRINT / MIDPOINT
    // (and `contains("PG")` mid-word), silently suppressing the output-contention
    // check on unrelated push-pull signal nets. Each token is compared as-is (so
    // NMI/NFLT/PGOOD/PG match) and with an active-low leading 'N' stripped
    // (NINT/NIRQ) and a trailing digit index stripped (IRQ2, INT_0 -> INT).
    let n = norm(name);
    n.split(|c: char| !c.is_ascii_alphanumeric()).any(|tok| {
        let base = tok.trim_end_matches(|c: char| c.is_ascii_digit());
        KEYS.contains(&base) || base.strip_prefix('N').is_some_and(|s| KEYS.contains(&s))
    })
}

fn check_output_contention(board: &ExtractedBoard, report: &mut NetLintReport) {
    for net in &board.nets {
        // Wired-OR / interrupt nets carry multiple open-drain "outputs" by design.
        if net.id == 0
            || is_unconnected_net(&net.name)
            || is_ground(&net.name)
            || is_wired_or_name(&net.name)
        {
            continue;
        }
        // An absent part is neither a driver (no false fight from a DNP option)
        // nor a resolver (a DNP series R must not excuse a real one).
        let mem: Vec<_> = members(board, net.id)
            .into_iter()
            .filter(|(c, _)| AssemblyState::of(c).is_present())
            .collect();
        // A connector pin is an off-board interface, never an on-board driver.
        let drivers: Vec<_> = mem
            .iter()
            .filter(|(c, p)| is_pushpull_output(&p.kind) && !is_connector_like(c))
            .collect();
        // Any resolver on the net (series R, tri-state, input, connector) means
        // the contention is or can be resolved: do not fire.
        let resolved = mem.iter().any(|(c, p)| {
            resolves_contention(&p.kind)
                || c.reference.to_ascii_uppercase().starts_with('R')
                || is_connector_like(c)
        });
        // Need >= 2 *distinct* driver parts (two pins of one chip on a net is an
        // internal short the symbol expresses, not an inter-part fight). Ordered,
        // so the JSON `refs` array is byte-reproducible.
        let distinct: std::collections::BTreeSet<String> =
            drivers.iter().map(|(c, _)| c.reference.clone()).collect();
        if distinct.len() < 2 || resolved {
            continue;
        }
        // Two power_out pins tied (a rail-vs-rail short) are High; two signal
        // outputs Medium.
        let power_out = drivers
            .iter()
            .filter(|(_, p)| p.kind.eq_ignore_ascii_case("power_out"));
        let drv_desc: Vec<String> = drivers
            .iter()
            .map(|(c, p)| format!("{}.{} ({})", c.reference, p.number, p.kind))
            .collect();
        report.findings.push(LintFinding {
            check: LintCheck::OutputContention,
            severity: if power_out.count() >= 2 { Severity::High } else { Severity::Medium },
            message: format!(
                "net '{}' is driven by {} push-pull outputs with no series resistor or tri-state to resolve them: {}",
                net.name,
                drivers.len(),
                hauksbee_ir::vocabulary::names_summary(&drv_desc)
            ),
            refs: distinct.into_iter().collect(),
            nets: vec![net.name.clone()],
        });
    }
}

/// Series resistance above which two outputs facing each other are left alone:
/// the tie is too weak to read as a wiring fault.
const SERIES_CONTENTION_MAX_OHMS: f64 = 10_000.0;

/// Two push-pull outputs facing each other through a resistor (or a chain of
/// resistors whose joints hold nothing else), e.g. a UART TX wired to the other
/// side's TX through a 330 ohm series resistor. The two ends are different
/// nets, so [`check_output_contention`] never sees them together, yet when the
/// outputs disagree the resistor alone limits the current between them.
///
/// Silent where the shape is legitimate: a summing node or an R-2R ladder (the
/// resistors' far ends are not both driven by an output, and a ladder joint holds
/// more than two resistor pins), a pin typed tri-state, open-drain or
/// bidirectional on either end, a connector on either end (an off-board input
/// may be there), and resistances above [`SERIES_CONTENTION_MAX_OHMS`].
fn check_series_contention(board: &ExtractedBoard, report: &mut NetLintReport) {
    let present = |c: &Component| AssemblyState::of(c).is_present();
    let plain_r = |c: &Component| {
        present(c) && c.pins.len() == 2 && is_resistor(c) && parse_ohms(&c.value).is_some()
    };
    let signal = |id: i64| {
        id != 0
            && board
                .net(id)
                .is_some_and(|n| !is_unconnected_net(&n.name) && !is_ground(&n.name))
    };
    // A joint of a series chain: exactly two pins, both plain resistors.
    let interior = |id: i64| {
        let m: Vec<_> = members(board, id)
            .into_iter()
            .filter(|(c, _)| present(c))
            .collect();
        m.len() == 2 && m.iter().all(|(c, _)| plain_r(c)) && m[0].0.reference != m[1].0.reference
    };
    let mut visited: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for r in board.components.iter().filter(|c| plain_r(c)) {
        if visited.contains(&r.reference) {
            continue;
        }
        let mut chain = vec![r.reference.clone()];
        let mut ohms = parse_ohms(&r.value).unwrap_or(0.0);
        let mut ends = Vec::new();
        for start in r.pins.iter().filter_map(|p| p.net) {
            let (mut net, mut cur) = (start, r);
            for _ in 0..8 {
                if !interior(net) {
                    break;
                }
                let Some((o, _)) = members(board, net)
                    .into_iter()
                    .find(|(c, _)| c.reference != cur.reference && present(c))
                else {
                    break;
                };
                chain.push(o.reference.clone());
                ohms += parse_ohms(&o.value).unwrap_or(0.0);
                let Some(far) = o.pins.iter().filter_map(|p| p.net).find(|n| *n != net) else {
                    break;
                };
                (net, cur) = (far, o);
            }
            ends.push(net);
        }
        visited.extend(chain.iter().cloned());
        let [a, b] = ends[..] else { continue };
        if a == b || !signal(a) || !signal(b) || ohms > SERIES_CONTENTION_MAX_OHMS {
            continue;
        }
        let name = |id: i64| board.net(id).map(|n| n.name.clone()).unwrap_or_default();
        if is_wired_or_name(&name(a)) || is_wired_or_name(&name(b)) {
            continue;
        }
        let (Some(da), Some(db)) = (
            series_side(board, a, &chain, &present),
            series_side(board, b, &chain, &present),
        ) else {
            continue;
        };
        report.findings.extend(series_contention_finding(
            board,
            &chain,
            ohms,
            (a, b),
            (da.as_slice(), db.as_slice()),
        ));
    }
}

/// A driving pin and the part it belongs to.
type Driver<'a> = (&'a Component, &'a Pin);

/// A pin that drives its net push-pull: a typed output, or a dedicated UART
/// transmit pad (an MCU pad is usually typed `bidirectional`).
fn drives_pushpull(c: &Component, p: &Pin) -> bool {
    is_pushpull_output(&p.kind) || (uart_role(p).is_some_and(|r| r.0) && !is_bus_transceiver(c))
}

/// The push-pull drivers on net `id` outside the series `chain`, or `None` when
/// there are none or the net holds a connector or a pin that can release or
/// share the line (which resolves the fight).
fn series_side<'a>(
    board: &'a ExtractedBoard,
    id: i64,
    chain: &[String],
    present: &dyn Fn(&Component) -> bool,
) -> Option<Vec<Driver<'a>>> {
    let mem: Vec<_> = members(board, id)
        .into_iter()
        .filter(|(c, _)| present(c) && !chain.contains(&c.reference))
        .collect();
    let resolved = mem.iter().any(|(c, p)| {
        is_connector_like(c)
            || (!drives_pushpull(c, p)
                && matches!(
                    p.kind.trim().to_ascii_lowercase().as_str(),
                    "tri_state" | "open_collector" | "open_emitter" | "bidirectional"
                ))
    });
    let found: Vec<_> = mem
        .into_iter()
        .filter(|(c, p)| drives_pushpull(c, p))
        .collect();
    (!resolved && !found.is_empty()).then_some(found)
}

/// The finding for the drivers `da` and `db` of nets `a` and `b` joined by the
/// series `chain` of `ohms`; `None` when one part drives both ends.
fn series_contention_finding(
    board: &ExtractedBoard,
    chain: &[String],
    ohms: f64,
    (a, b): (i64, i64),
    (da, db): (&[Driver<'_>], &[Driver<'_>]),
) -> Option<LintFinding> {
    let name = |id: i64| board.net(id).map(|n| n.name.clone()).unwrap_or_default();
    let refs_a: std::collections::BTreeSet<&str> =
        da.iter().map(|(c, _)| c.reference.as_str()).collect();
    let refs_b: std::collections::BTreeSet<&str> =
        db.iter().map(|(c, _)| c.reference.as_str()).collect();
    if refs_a == refs_b && refs_a.len() == 1 {
        return None;
    }
    // The swing each side can drive, from the highest named rail its
    // driving parts are powered from.
    let swing = da.iter().chain(db).filter_map(|(c, _)| {
        c.pins
            .iter()
            .filter(|p| p.kind.eq_ignore_ascii_case("power_in"))
            .filter_map(|p| p.net.and_then(|id| board.net(id)))
            .filter_map(|n| hauksbee_models::netclass::rail_voltage(&n.name))
            .max_by(f64::total_cmp)
    });
    let volts = swing.max_by(f64::total_cmp);
    let drv_desc: Vec<String> = da
        .iter()
        .chain(db)
        .map(|(c, p)| format!("{}.{} ({})", c.reference, p.number, p.function))
        .collect();
    let chain_text = if chain.len() == 1 {
        format!("series resistor {}", chain[0])
    } else {
        format!("series resistors {}", chain.join(" + "))
    };
    let current = volts.map_or_else(
        || "its current is the supply voltage divided by that resistance".to_string(),
        |v| {
            format!(
                "up to {v:.1} V / {ohms:.0} ohm = {}",
                hauksbee_ir::units::format_amps(v / ohms.max(1.0))
            )
        },
    );
    let mut refs: Vec<String> = refs_a
        .iter()
        .chain(&refs_b)
        .map(|s| (*s).to_string())
        .collect();
    refs.sort();
    refs.dedup();
    Some(LintFinding {
            check: LintCheck::OutputContention,
            severity: Severity::Medium,
            message: format!(
                "'{}' and '{}' are joined only by {chain_text} ({ohms:.0} ohm) and each side is driven by a push-pull output: {}. Two outputs facing each other through a resistor still fight: when they drive opposite levels the resistor alone limits the current between them, {current}. If this is a UART, TX must go to the other side's RX; otherwise drive the line from one side only",
                name(a),
                name(b),
                hauksbee_ir::vocabulary::names_summary(&drv_desc)
            ),
            refs,
            nets: vec![name(a), name(b)],
        })
}

/// UART direction of one pin-function token, read once in
/// [`hauksbee_ir::pin_functions::uart_token_direction`].
fn uart_dir(token: &str) -> Option<bool> {
    hauksbee_ir::pin_functions::uart_token_direction(token)
}

/// Function-name tokens, split on the separators symbol libraries use for
/// multiplexed names (`U0TXD/GPIO43/CLK_OUT1`, `PA9 (USART1_TX)`).
fn function_tokens(function: &str) -> impl Iterator<Item = &str> {
    function
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
}

/// The pin's UART role, and whether the pin is a dedicated UART pin (its whole
/// name is that one token). A pin offering both a TX and an RX alternate is
/// ambiguous and has no role; a pin typed `input` is never a transmitter.
fn uart_role(p: &Pin) -> Option<(bool, bool)> {
    let dirs: Vec<bool> = function_tokens(&p.function).filter_map(uart_dir).collect();
    let tx = *dirs.first()?;
    if dirs.iter().any(|d| *d != tx) || (tx && p.kind.eq_ignore_ascii_case("input")) {
        return None;
    }
    Some((tx, function_tokens(&p.function).count() == 1))
}

/// The UART role of a pin by its function name, for the engine-layer checks
/// that share this reading: `Some((transmit, dedicated))`, see `uart_role`.
#[must_use]
pub fn uart_pin_role(p: &Pin) -> Option<(bool, bool)> {
    uart_role(p)
}

/// A bus transceiver (a part with a `CANH`/`CANL` bus pin): its `TXD` is
/// the input the controller transmits into and its `RXD` an output, whatever
/// direction an untyped symbol gives them, so its pins carry no UART role.
fn is_bus_transceiver(c: &Component) -> bool {
    c.pins.iter().any(|p| {
        function_tokens(&p.function)
            .any(|t| matches!(t.to_ascii_uppercase().as_str(), "CANH" | "CANL"))
    })
}

/// An active-low fault flag (`FLT`, `~{FAULT}`, `nFLT`) that is not typed as
/// a push-pull output. Such a pin only sinks, and it sinks on the fault, so an
/// LED from it to ground is dark on the fault (a note: a design choice, see
/// [`check_pin_functions`]). A power-good flag is excluded: an LED fed by its
/// pull-up lights on "good", which is the intended reading.
fn is_fault_flag(p: &Pin) -> bool {
    !matches!(
        p.kind.to_ascii_lowercase().as_str(),
        "output" | "tri_state" | "power_out"
    ) && function_tokens(&p.function).any(|t| {
        let t = t.to_ascii_uppercase();
        let t = t.strip_prefix('N').filter(|r| r.len() > 1).unwrap_or(&t);
        matches!(t, "FLT" | "FAULT")
    })
}

/// Pin-name/pin-type ERC ([`LintCheck::PinFunction`]). Each rule fires only on
/// pins whose role is specific; passive and generic pins stay silent.
fn check_pin_functions(board: &ExtractedBoard, report: &mut NetLintReport) {
    let fitted = |c: &Component| AssemblyState::of(c).is_present() && !is_connector_like(c);
    let mut push = |severity, message: String, refs: Vec<String>, nets: Vec<String>| {
        report.findings.push(LintFinding {
            check: LintCheck::PinFunction,
            severity,
            message,
            refs,
            nets,
        });
    };
    let signal = |n: &&Net| n.id != 0 && !is_unconnected_net(&n.name) && !is_ground(&n.name);
    for net in board.nets.iter().filter(signal) {
        let roles: Vec<_> = members(board, net.id)
            .into_iter()
            .filter_map(|(c, p)| {
                let role = uart_role(p).filter(|_| fitted(c) && !is_bus_transceiver(c))?;
                Some((c, p, role))
            })
            .collect();
        let parts: std::collections::BTreeSet<String> =
            roles.iter().map(|r| r.0.reference.clone()).collect();
        if parts.len() >= 2 && roles.iter().all(|r| r.2 .0) && roles.iter().any(|r| r.2 .1) {
            let pins: Vec<String> = roles
                .iter()
                .map(|(c, p, _)| format!("{}.{} ({})", c.reference, p.number, p.function))
                .collect();
            push(Severity::High, format!(
                "net '{}' joins {} UART transmit pins and no receive pin: {}. Two transmitters drive the same wire and neither hears the other, so the link cannot work (TX must go to the other side's RX); swap the TX/RX pair between these parts",
                net.name, pins.len(), hauksbee_ir::vocabulary::names_summary(&pins)), parts.into_iter().collect(), vec![net.name.clone()]);
        }
    }
    for led in board
        .components
        .iter()
        .filter(|c| fitted(c) && is_led(c) && c.pins.len() == 2)
    {
        let (Some(anode), Some(cathode)) =
            (named(led, ["A", "ANODE"]), named(led, ["K", "CATHODE"]))
        else {
            continue;
        };
        let grounded = |id: i64| board.net(id).is_some_and(|n| is_ground(&n.name));
        let returns = grounded(cathode)
            || members(board, cathode).iter().any(|(r, _)| {
                r.reference != led.reference
                    && is_resistor(r)
                    && r.pins.iter().filter_map(|q| q.net).any(grounded)
            });
        let flag = members(board, anode)
            .into_iter()
            .find(|(c, p)| c.reference != led.reference && fitted(c) && is_fault_flag(p));
        // A note, never a gate: the LED runs on the flag's pull-up current and
        // is off whenever the pin sinks, so nothing is overstressed, and "lit
        // while all is well" is the same reading a power-good lamp gives on
        // purpose. What it can harm is the idle level a logic input on the
        // line sees, which depends on the pull-up and the reader's threshold:
        // the level checks judge that where the parts are modelled.
        if let (true, Some((c, p))) = (returns, flag) {
            let net = board.net(anode).map(|n| n.name.clone()).unwrap_or_default();
            push(Severity::Low, format!(
                "{} has its anode on {}.{} ({}), an active-low open-drain fault flag, and its cathode returns to ground: it runs on the flag's pull-up current, so it is lit while there is no fault and dark on a fault, and nothing is overstressed. If it is meant to show the fault, move it between a supply rail and the pin (cathode to the pin). As wired it also holds the idle level of '{net}' near its forward voltage plus its resistor's drop, below the pull-up rail: check that any logic input reading this line still sees a valid high",
                led.reference, c.reference, p.number, p.function), vec![led.reference.clone(), c.reference.clone()], vec![net]);
        }
    }
    for q in board.components.iter().filter(|c| fitted(c)) {
        let pin = |names| named(q, names);
        let (Some(g), Some(s), Some(d)) = (
            pin(["G", "GATE"]),
            pin(["S", "SOURCE"]),
            pin(["D", "DRAIN"]),
        ) else {
            continue;
        };
        let entry = |id| {
            members(board, id)
                .iter()
                .any(|(c, _)| is_connector_like(c) && !is_test_point_or_hole(c))
        };
        if s != d && entry(s) && !entry(d) && static_gate(board, q, g, [s, d]) {
            let name = |id| board.net(id).map(|n| n.name.clone()).unwrap_or_default();
            push(Severity::Medium, format!(
                "{} is an always-on FET (its gate is biased only by passives), as used for reverse-polarity protection, but its source is on the connector net '{}' and its drain on '{}': the body diode then points from the load back to the input and a reversed supply is not blocked; swap source and drain so the drain faces the connector",
                q.reference, name(s), name(d)), vec![q.reference.clone()], vec![name(s), name(d)]);
        }
    }
}

/// The net of `c`'s first pin whose function is one of `names`. A pin with no
/// function (an Eagle board, whose package pads are named `G`/`D`/`S`) is read
/// by its pad name.
fn named(c: &Component, names: [&str; 2]) -> Option<i64> {
    let hit = |p: &&Pin| {
        let name = if p.function.is_empty() {
            &p.number
        } else {
            &p.function
        };
        names.iter().any(|n| name.eq_ignore_ascii_case(n))
    };
    c.pins.iter().find(hit)?.net
}

/// True when the gate net `g` of `q`, and every net one passive further out, holds
/// only 2-pin resistors, capacitors and diodes, all ending within two hops on
/// ground or on `q`'s own source/drain: nothing drives the gate, so the FET
/// sits permanently on.
fn static_gate(board: &ExtractedBoard, q: &Component, g: i64, ends: [i64; 2]) -> bool {
    let passive = |c: &Component| {
        let r = ref_designator(&c.reference);
        let tail = r.trim_start_matches(['C', 'D', 'Z']);
        c.pins.len() == 2
            && (is_resistor(c)
                || (tail.len() == r.len() - 1 && tail.starts_with(|d: char| d.is_ascii_digit())))
    };
    let stop = |id: i64| ends.contains(&id) || board.net(id).is_some_and(|n| is_ground(&n.name));
    let mut frontier = vec![(g, q.reference.as_str())];
    for depth in 0..2 {
        let mut next = Vec::new();
        for (net, from) in frontier {
            let others: Vec<_> = members(board, net)
                .into_iter()
                .filter(|(c, _)| c.reference != from && c.reference != q.reference)
                .collect();
            if others.is_empty() && depth == 0 || others.iter().any(|(c, _)| !passive(c)) {
                return false;
            }
            for (c, p) in others {
                next.extend(
                    c.pins
                        .iter()
                        .filter(|o| o.number != p.number)
                        .filter_map(|o| o.net)
                        .filter(|id| *id != g && !stop(*id))
                        .map(|id| (id, c.reference.as_str())),
                );
            }
        }
        frontier = next;
    }
    frontier.is_empty()
}

/// Render a lint report as a Unicode table.
#[must_use]
pub fn render_netlint(report: &NetLintReport) -> String {
    let mut out = String::new();
    if report.findings.is_empty() {
        out.push_str("net-lint: no findings.\n");
        return out;
    }
    let groups = crate::repeat::group_repeats(&report.findings, |f| {
        let [_] = f.refs.as_slice() else { return None };
        Some(format!(
            "{}/{}/{}",
            f.check.as_str(),
            f.severity.as_str(),
            crate::repeat::repeat_shape(&f.message, &f.refs)
        ))
    });
    let _ = write!(out, "net-lint: {} finding(s)", report.findings.len());
    if groups.len() < report.findings.len() {
        let _ = write!(
            out,
            " (repeats grouped into {} lines; --json lists each)",
            groups.len()
        );
    }
    out.push('\n');
    for group in groups {
        let first = group[0];
        let (severity, check) = (first.severity.as_str(), first.check.as_str());
        if group.len() == 1 {
            let _ = writeln!(out, "  [{severity}] {check} - {}", first.message);
        } else {
            let parts: Vec<&str> = group.iter().map(|f| f.refs[0].as_str()).collect();
            let lead = crate::repeat::describe_parts(&parts);
            let _ = writeln!(out, "  [{severity}] {check} - {lead}: {}", first.message);
        }
    }
    out
}

#[cfg(test)]
mod repeat_render_tests {
    use super::*;

    fn overstress(part: &str, ma: &str) -> LintFinding {
        LintFinding {
            check: LintCheck::DcOverstress,
            severity: Severity::High,
            message: format!("{part} is overstressed: {part}_s0 passes {ma} mA"),
            refs: vec![part.to_string()],
            nets: Vec::new(),
        }
    }

    #[test]
    fn a_finding_repeated_across_parts_is_one_line_with_a_count_and_a_few_refs() {
        let report = NetLintReport {
            findings: vec![
                overstress("IC10006", "234"),
                overstress("IC10106", "234"),
                overstress("IC10206", "234"),
                overstress("IC10306", "234"),
                overstress("IC10406", "50"),
            ],
        };
        let text = render_netlint(&report);
        assert!(
            text.contains("5 finding(s) (repeats grouped into 2 lines"),
            "{text}"
        );
        assert!(
            text.contains(
                "[high] dc_overstress - 4 parts like IC10006, IC10106, IC10206 and 1 more: IC10006 is overstressed: IC10006_s0 passes 234 mA"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "[high] dc_overstress - IC10406 is overstressed: IC10406_s0 passes 50 mA"
            ),
            "{text}"
        );
    }

    #[test]
    fn distinct_findings_render_as_before() {
        let report = NetLintReport {
            findings: vec![overstress("IC1", "234"), overstress("IC2", "50")],
        };
        let text = render_netlint(&report);
        assert!(text.contains("2 finding(s)\n"), "{text}");
        assert!(!text.contains("parts like"), "{text}");
    }
}

#[cfg(test)]
mod pin_array_tests {
    use super::is_pin_array_package;

    #[test]
    fn body_dimension_footprints_are_not_pin_arrays() {
        // KiCad appends a body dimension ("7x7mm", "3.9x4.9mm") to nearly
        // every SMD IC footprint. Reading any `<digit>x<digit>` as a pin
        // grid makes those ICs "connector-like", silently suppressing the
        // floating-control / I2C-pull-up / output-contention lints on real
        // ICs. A body dimension (decimal, or an "mm" unit) is NOT a pin grid.
        for fp in [
            "MCU_ST:LQFP-48_7x7mm",
            "Package_QFP:LQFP-64_10x10mm",
            "Package_SO:SOIC-8_3.9x4.9mm",
            "Package_DFN_QFN:QFN-48-1EP_7x7mm",
            "lib:QFN50P400X400X100-25N-D",
        ] {
            assert!(
                !is_pin_array_package(fp),
                "{fp} is a body size, not a pin grid"
            );
        }
        // A genuine pin grid (integer counts, no unit) still reads as one.
        for fp in ["PinHeader_1x10", "Connector_2x05", "1X10", "2X20_P2.54mm"] {
            assert!(is_pin_array_package(fp), "{fp} is a real pin grid");
        }
    }
}

#[cfg(test)]
mod i2c_pullup_dedup_tests {
    use crate::testutil::{board, part, pin};
    use crate::{LintCheck, Pin};

    fn sda_pin() -> Pin {
        let mut p = pin("5", Some(1));
        p.function = "SDA".into();
        p
    }

    #[test]
    fn single_device_with_a_double_listed_pad_does_not_fire_missing_pullup() {
        // A single on-board I2C device (ambiguous → deliberate skip) whose SDA
        // pad is double-listed (IPC-356 both-sided access record) counted as TWO
        // active devices, escalating the skip into a false "on-board master and
        // peripheral, no pull-up" MissingI2cPullup finding. Deduping by reference,
        // one device stays one and the check must stay silent.
        // The same pad listed twice.
        let board = board(
            &[(1, "SDA")],
            vec![part("U1", "SENSOR", "", "", vec![sda_pin(), sda_pin()])],
        );
        let report = board.net_lint();
        assert_eq!(
            report.of_check(LintCheck::MissingI2cPullup).count(),
            0,
            "a single device (its pad merely double-listed) must not fire a missing-pullup finding"
        );
    }
}

/// A bus whose nets are not named for it: the symbol pins on them say what it is.
#[cfg(test)]
mod i2c_pin_named_bus_tests {
    use crate::testutil::{board, part, pin};
    use crate::{Component, ExtractedBoard, LintCheck, LintFinding, Pin, Severity};

    const DATA: i64 = 1;
    const CLOCK: i64 = 2;
    const RAIL: i64 = 3;

    fn named(number: &str, net: i64, function: &str) -> Pin {
        let mut p = pin(number, Some(net));
        p.function = function.into();
        p
    }

    /// A controller on both lines; its pads are port names, not bus names.
    fn controller() -> Component {
        part(
            "U1",
            "MCU",
            "",
            "QFN-40",
            vec![named("5", DATA, "PB7"), named("6", CLOCK, "PB6")],
        )
    }

    /// A peripheral whose pins carry the given names on the two lines.
    fn peripheral(data: &str, clock: &str) -> Component {
        part(
            "U2",
            "SENSOR",
            "",
            "SOIC-8",
            vec![named("1", DATA, data), named("2", CLOCK, clock)],
        )
    }

    fn pull(reference: &str, net: i64) -> Component {
        part(
            reference,
            "4.7k",
            "",
            "R_0603",
            vec![pin("1", Some(RAIL)), pin("2", Some(net))],
        )
    }

    fn run(parts: Vec<Component>) -> Vec<LintFinding> {
        let b: ExtractedBoard = board(
            &[(DATA, "SENS_D"), (CLOCK, "SENS_C"), (RAIL, "+3V3")],
            parts,
        );
        b.net_lint()
            .of_check(LintCheck::MissingI2cPullup)
            .cloned()
            .collect()
    }

    #[test]
    fn pins_named_sda_and_scl_make_an_unnamed_net_a_bus_with_no_pull_up() {
        let found = run(vec![controller(), peripheral("SDA", "SCL")]);
        assert_eq!(found.len(), 2, "{found:#?}");
        assert!(found.iter().all(|f| f.severity == Severity::High));
        let data = found.iter().find(|f| f.nets == ["SENS_D"]).unwrap();
        assert!(
            data.message.contains("I2C SDA net 'SENS_D'"),
            "{}",
            data.message
        );
        assert!(data.message.contains("U2 pin 1 'SDA'"), "{}", data.message);
        assert!(data.own_remedy().is_some(), "{}", data.message);
        assert!(found
            .iter()
            .any(|f| f.message.contains("I2C SCL net 'SENS_C'")));
    }

    #[test]
    fn the_bus_words_around_the_line_name_do_not_matter() {
        for (data, clock) in [
            ("I2C_SDA", "I2C_SCL"),
            ("TWI_SDA", "TWI_SCL"),
            ("SDA0", "SCL0"),
        ] {
            let found = run(vec![controller(), peripheral(data, clock)]);
            assert_eq!(found.len(), 2, "{data}: {found:#?}");
        }
    }

    #[test]
    fn a_pull_up_resistor_keeps_a_pin_named_bus_silent() {
        let found = run(vec![
            controller(),
            peripheral("SDA", "SCL"),
            pull("R1", DATA),
            pull("R2", CLOCK),
        ]);
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn a_resistor_network_pull_up_keeps_a_pin_named_bus_silent() {
        let network = part(
            "RN1",
            "4k7",
            "",
            "R_Array_Convex_4x0603",
            vec![
                pin("1", Some(RAIL)),
                pin("2", Some(DATA)),
                pin("3", Some(RAIL)),
                pin("4", Some(CLOCK)),
            ],
        );
        let found = run(vec![controller(), peripheral("SDA", "SCL"), network]);
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn a_connector_on_the_bus_abstains() {
        let header = part(
            "J1",
            "Conn_01x04",
            "Connector:PinHeader",
            "PinHeader_1x04_P2.54mm",
            vec![pin("1", Some(DATA)), pin("2", Some(CLOCK))],
        );
        let found = run(vec![controller(), peripheral("SDA", "SCL"), header]);
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn a_part_that_cannot_be_read_abstains() {
        // The controller's pins carry no names at all.
        let mut blind = controller();
        blind.pins.iter_mut().for_each(|p| p.function.clear());
        let found = run(vec![blind, peripheral("SDA", "SCL")]);
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn spi_and_multiplexed_pin_names_do_not_make_a_bus() {
        for (data, clock) in [
            ("SDO", "SCK"),
            ("SDI", "SCLK"),
            ("MISO", "MOSI"),
            ("SDA/SDI", "SCL/SCLK"),
            ("PB6/SDA", "PB7/SCL"),
        ] {
            let found = run(vec![controller(), peripheral(data, clock)]);
            assert!(found.is_empty(), "{data} {clock}: {found:#?}");
        }
    }

    #[test]
    fn a_net_whose_pins_name_both_lines_is_not_judged() {
        let both = part(
            "U3",
            "SENSOR",
            "",
            "SOIC-8",
            vec![named("1", DATA, "SCL"), named("2", CLOCK, "SDA")],
        );
        let found = run(vec![controller(), peripheral("SDA", "SCL"), both]);
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn a_net_called_for_spi_or_a_clock_is_not_an_i2c_bus() {
        // A display whose serial pins are named SCL/SDA, wired as SPI.
        let b: ExtractedBoard = board(
            &[
                (DATA, "DISPLAY-MOSI"),
                (CLOCK, "DISPLAY-CLK"),
                (RAIL, "+3V3"),
            ],
            vec![controller(), peripheral("SDA", "SCL")],
        );
        let found: Vec<_> = b
            .net_lint()
            .of_check(LintCheck::MissingI2cPullup)
            .cloned()
            .collect();
        assert!(
            !found.iter().any(|f| f.nets == ["DISPLAY-CLK"]),
            "{found:#?}"
        );
    }

    #[test]
    fn one_device_alone_is_not_a_bus() {
        let found = run(vec![peripheral("SDA", "SCL")]);
        assert!(found.is_empty(), "{found:#?}");
    }
}

#[cfg(test)]
mod i2c_pullup_source_tests {
    use crate::testutil::{board, part, pin};
    use crate::{Component, ExtractedBoard, LintCheck, Pin, Severity};

    fn named(number: &str, net: i64, function: &str) -> Pin {
        let mut p = pin(number, Some(net));
        p.function = function.into();
        p
    }

    /// An MCU and an expander on SDA, a supply rail with its bypass cap, and
    /// `transistor` as the only other part on the line.
    fn bus_with(transistor: Component) -> ExtractedBoard {
        board(
            &[
                (1, "EXP_SDA"),
                (2, "I2C_BIAS"),
                (3, "Net-(Q1-Pad2)"),
                (4, "VCC_MCU"),
                (5, "GND"),
            ],
            vec![
                part("U1", "MCU", "", "QFN-40", vec![named("5", 1, "SDA")]),
                part("U2", "TCA9535", "", "WQFN-24", vec![named("20", 1, "SDA")]),
                part(
                    "R1",
                    "100",
                    "",
                    "R_0603",
                    vec![pin("1", Some(4)), pin("2", Some(3))],
                ),
                part(
                    "C1",
                    "10u",
                    "",
                    "C_0805",
                    vec![pin("1", Some(4)), pin("2", Some(5))],
                ),
                transistor,
            ],
        )
    }

    fn i2c_findings(b: &ExtractedBoard) -> Vec<Severity> {
        let report = b.net_lint();
        report
            .of_check(LintCheck::MissingI2cPullup)
            .map(|f| f.severity)
            .collect()
    }

    /// A PNP whose emitter reaches the rail through its current-setting
    /// resistor and whose collector is on the line is a constant-current
    /// pull-up: the bus is pulled up, and the PNP is not a second device.
    #[test]
    fn pnp_current_source_is_the_pullup() {
        let q = part(
            "Q1",
            "BC807",
            "",
            "SOT-23",
            vec![named("1", 2, "B"), named("2", 3, "E"), named("3", 1, "C")],
        );
        assert_eq!(i2c_findings(&bus_with(q)), Vec::<Severity>::new());
    }

    /// The same transistor with its emitter on ground pulls the line DOWN, and
    /// one without named pins proves nothing: both still leave an on-board
    /// master and peripheral with no pull-up.
    #[test]
    fn grounded_or_unnamed_transistor_is_not_a_pullup() {
        let grounded = part(
            "Q1",
            "BC817",
            "",
            "SOT-23",
            vec![named("1", 2, "B"), named("2", 5, "E"), named("3", 1, "C")],
        );
        assert_eq!(i2c_findings(&bus_with(grounded)), vec![Severity::Medium]);
        let unnamed = part(
            "Q1",
            "BC807",
            "",
            "SOT-23",
            vec![pin("1", Some(2)), pin("2", Some(3)), pin("3", Some(1))],
        );
        assert_eq!(i2c_findings(&bus_with(unnamed)), vec![Severity::Medium]);
    }

    /// Pads of one footprint at `points`, numbered from 1, placed on F.Cu.
    fn placed(reference: &str, points: &[(f64, f64)]) -> Component {
        let mut c = part(reference, "PART", "", "FP", Vec::new());
        c.layer = "F.Cu".into();
        for (i, at) in points.iter().enumerate() {
            let mut p = pin(&(i + 1).to_string(), None);
            p.position = Some(*at);
            c.pins.push(p);
        }
        c
    }

    /// 200 pads in two rows, rotated 30 degrees, plus two shield tabs off the
    /// rows: the shape of a SODIMM socket.
    fn two_row_socket_points() -> Vec<(f64, f64)> {
        let (sin, cos) = 30.0_f64.to_radians().sin_cos();
        let mut pts = Vec::new();
        for i in 0..100 {
            let along = f64::from(i) * 0.6;
            for across in [-4.1, 4.1] {
                pts.push((along * cos - across * sin, along * sin + across * cos));
            }
        }
        pts.push((-3.0, 12.0));
        pts.push((63.0, 12.0));
        pts
    }

    #[test]
    fn module_socket_is_read_from_pad_rows_not_from_names() {
        assert!(super::is_module_socket(&placed(
            "U1",
            &two_row_socket_points()
        )));
        assert!(super::is_connector_like(&placed(
            "U1",
            &two_row_socket_points()
        )));
        // A 15 x 15 BGA and a 144-pin QFP have as many pins, spread over a
        // grid or four sides.
        let bga: Vec<(f64, f64)> = (0..225)
            .map(|i| (f64::from(i % 15) * 0.8, f64::from(i / 15) * 0.8))
            .collect();
        assert!(!super::is_module_socket(&placed("U1", &bga)));
        let mut qfp = Vec::new();
        for i in 0..36 {
            let t = f64::from(i).mul_add(0.5, -8.75);
            qfp.extend([(t, -11.0), (t, 11.0), (-11.0, t), (11.0, t)]);
        }
        assert!(!super::is_module_socket(&placed("U1", &qfp)));
        // Schematic pin positions are a drawing, not pads.
        let mut symbol = placed("U1", &two_row_socket_points());
        symbol.layer.clear();
        assert!(!super::is_module_socket(&symbol));
    }

    /// A DAC on a bus that also reaches a plug-in module socket named only by
    /// a part number: the module is assumed to carry the pull-ups (a stated
    /// note, not a gating warning). The same bus to a BGA processor soldered
    /// on the board stays a missing pull-up.
    #[test]
    fn bus_to_module_socket_is_a_stated_assumption_not_a_fault() {
        let with_host = |mut host: Component| {
            host.pins[0].net = Some(1);
            board(
                &[(1, "DAC_SDA")],
                vec![
                    host,
                    part("U2", "WM8960", "", "QFN-32", vec![named("20", 1, "SDA")]),
                ],
            )
        };
        let socket = with_host(placed("U1", &two_row_socket_points()));
        let report = socket.net_lint();
        let found: Vec<_> = report.of_check(LintCheck::MissingI2cPullup).collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].severity, Severity::Low);
        assert!(
            found[0].message.contains("module socket"),
            "{}",
            found[0].message
        );
        let bga: Vec<(f64, f64)> = (0..225)
            .map(|i| (f64::from(i % 15) * 0.8, f64::from(i / 15) * 0.8))
            .collect();
        assert_eq!(
            i2c_findings(&with_host(placed("U1", &bga))),
            vec![Severity::Medium]
        );
    }

    // ---- A DNP footprint that stands for a mated module (repro spec 1) ----

    /// A peripheral on SDA and SCL, plus `far_end` joining the same two nets
    /// through its pads 1 and 2, plus `extra` parts.
    fn two_wire(mut far_end: Component, extra: Vec<Component>) -> ExtractedBoard {
        far_end.pins[0].net = Some(1);
        far_end.pins[1].net = Some(2);
        let mut parts = vec![
            part(
                "U2",
                "PERIPHERAL",
                "",
                "SOIC-8",
                vec![named("1", 1, "SDA"), named("2", 2, "SCL")],
            ),
            far_end,
        ];
        parts.extend(extra);
        board(&[(1, "SDA"), (2, "SCL"), (3, "+3V3")], parts)
    }

    fn pullups() -> Vec<Component> {
        ["R1", "R2"]
            .iter()
            .zip([1, 2])
            .map(|(r, net)| {
                part(
                    r,
                    "4.7k",
                    "",
                    "R_0603",
                    vec![pin("1", Some(3)), pin("2", Some(net))],
                )
            })
            .collect()
    }

    fn socket(dnp: bool) -> Component {
        let mut c = placed("U1", &two_row_socket_points());
        c.dnp = dnp;
        c
    }

    fn header(dnp: bool) -> Component {
        let mut c = part(
            "J1",
            "Conn_01x04",
            "Connector:PinHeader",
            "PinHeader_1x04_P2.54mm",
            vec![
                pin("1", None),
                pin("2", None),
                pin("3", None),
                pin("4", None),
            ],
        );
        c.dnp = dnp;
        c
    }

    /// (a) the module socket is fitted: the Low "assumed pulled up" note, on
    /// both lines.
    #[test]
    fn fitted_module_socket_is_a_low_note() {
        let found = i2c_findings(&two_wire(socket(false), Vec::new()));
        assert_eq!(found, vec![Severity::Low, Severity::Low]);
    }

    /// (b) the same socket marked DNP (the fab does not solder the module) is
    /// still the bus's far end: same Low note, never silent, never escalated by
    /// the build flag, and the message says the socket is DNP.
    #[test]
    fn dnp_module_socket_still_counts_as_a_bus_end() {
        let board = two_wire(socket(true), Vec::new());
        let report = board.net_lint();
        let found: Vec<_> = report.of_check(LintCheck::MissingI2cPullup).collect();
        assert_eq!(found.len(), 2, "SDA and SCL");
        for f in found {
            assert_eq!(f.severity, Severity::Low);
            assert!(f.message.contains("module socket"), "{}", f.message);
            assert!(f.message.contains("DNP"), "{}", f.message);
            assert!(f.refs.contains(&"U1".to_string()), "{:?}", f.refs);
        }
    }

    /// (c) DNP socket plus pull-ups on the bus: nothing to report.
    #[test]
    fn dnp_module_socket_with_pullups_is_quiet() {
        assert_eq!(
            i2c_findings(&two_wire(socket(true), pullups())),
            Vec::<Severity>::new()
        );
    }

    /// (d) a plain DNP header instead of the big socket: the header regime,
    /// Low, and visible.
    #[test]
    fn dnp_header_counts_as_a_bus_end() {
        let board = two_wire(header(true), Vec::new());
        let report = board.net_lint();
        let found: Vec<_> = report.of_check(LintCheck::MissingI2cPullup).collect();
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|f| f.severity == Severity::Low));
        assert!(found[0].message.contains("header marked DNP"));
        // Fitted, the same header gives the unmarked Low note.
        assert_eq!(
            i2c_findings(&two_wire(header(false), Vec::new())),
            vec![Severity::Low, Severity::Low]
        );
    }

    /// A DNP part with no bus role stays out of the count: a DNP peripheral IC
    /// or test point beside a single live device does not make a second end,
    /// and a DNP socket does not turn a two-device bus into anything else.
    #[test]
    fn dnp_part_without_a_bus_role_stays_out_of_the_count() {
        let mut ic = part(
            "U3",
            "SPARE",
            "",
            "SOIC-8",
            vec![named("1", 1, "SDA"), named("2", 2, "SCL")],
        );
        ic.dnp = true;
        assert_eq!(
            i2c_findings(&two_wire(ic, Vec::new())),
            Vec::<Severity>::new(),
            "one live device plus a DNP IC is an ambiguous bus, not a missing pull-up"
        );
        let mut tp = part(
            "TP1",
            "TestPoint",
            "TestPoint:TestPoint_Pad",
            "TestPoint_Pad_D1.0mm",
            vec![pin("1", None), pin("2", None)],
        );
        tp.dnp = true;
        assert_eq!(
            i2c_findings(&two_wire(tp, Vec::new())),
            Vec::<Severity>::new()
        );
    }

    /// A live master on the two lines, so the far end of `two_wire` is a second
    /// active device and the bus is a judged on-board bus.
    fn master() -> Component {
        part(
            "U1",
            "MCU",
            "",
            "QFN-40",
            vec![named("1", 1, "SDA"), named("2", 2, "SCL")],
        )
    }

    /// The `pullups()` pair with the references in `dnp` marked do-not-populate.
    fn pullups_dnp(dnp: &[&str]) -> Vec<Component> {
        let mut rs = pullups();
        for r in &mut rs {
            r.dnp = dnp.contains(&r.reference.as_str());
        }
        rs
    }

    /// The `missing_i2c_pullup` findings as (net, severity, message).
    fn i2c_detail(b: &ExtractedBoard) -> Vec<(String, Severity, String)> {
        b.net_lint()
            .of_check(LintCheck::MissingI2cPullup)
            .map(|f| (f.nets[0].clone(), f.severity, f.message.clone()))
            .collect()
    }

    /// A line whose only pull-up is do-not-populate has no pull-up on the built
    /// board: it is reported, at the severity a missing pull-up earns, and the
    /// message names the DNP resistor instead of reading as a bare omission.
    #[test]
    fn a_dnp_only_pullup_is_not_a_pullup_and_the_finding_says_so() {
        let b = two_wire(master(), pullups_dnp(&["R1", "R2"]));
        let found = i2c_detail(&b);
        assert_eq!(found.len(), 2, "SDA and SCL: {found:?}");
        for (net, sev, msg) in &found {
            assert_eq!(*sev, Severity::Medium, "{net}: {msg}");
            assert!(msg.contains("do-not-populate"), "{msg}");
            let part = if net == "SDA" { "R1" } else { "R2" };
            assert!(msg.contains(part), "{msg}");
            assert!(
                !msg.contains(crate::DNP_FITTED_NOTE_MARKER),
                "default policy leaves the part out; it does not fit it: {msg}"
            );
        }
        // The same board with the resistors fitted is clean.
        assert!(i2c_findings(&two_wire(master(), pullups_dnp(&[]))).is_empty());
    }

    /// One fitted pull-up and one DNP: only the line left without a fitted
    /// pull-up is reported.
    #[test]
    fn a_dnp_pullup_beside_a_fitted_one_leaves_only_the_bare_line() {
        let b = two_wire(master(), pullups_dnp(&["R2"]));
        let found = i2c_detail(&b);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, "SCL");
        assert!(found[0].2.contains("R2"), "{}", found[0].2);
    }

    /// A single live device with a DNP pull-up used to be silent as an
    /// "ambiguous" bus. The pull-up drawn in the layout shows the line was meant
    /// to be pulled up, so it is a Low finding naming the part.
    #[test]
    fn a_single_device_with_only_a_dnp_pullup_is_a_low_finding() {
        let mut ic = part(
            "U3",
            "SPARE",
            "",
            "SOIC-8",
            vec![named("1", 1, "SDA"), named("2", 2, "SCL")],
        );
        ic.dnp = true;
        let found = i2c_detail(&two_wire(ic.clone(), pullups_dnp(&["R1", "R2"])));
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|f| f.1 == Severity::Low), "{found:?}");
        assert!(found.iter().all(|f| f.2.contains("do-not-populate")));
        // With no pull-up drawn at all the bus stays the ambiguous, silent case.
        assert!(i2c_findings(&two_wire(ic, Vec::new())).is_empty());
    }

    /// A DNP pull-up on a bus that ends at a module socket stays the Low note,
    /// and the note now names the unfitted resistor.
    #[test]
    fn a_dnp_pullup_on_a_module_socket_bus_keeps_the_low_regime() {
        let found = i2c_detail(&two_wire(socket(false), pullups_dnp(&["R1", "R2"])));
        assert_eq!(found.len(), 2, "{found:?}");
        for (_, sev, msg) in &found {
            assert_eq!(*sev, Severity::Low);
            assert!(msg.contains("do-not-populate") && msg.contains("module socket"));
        }
    }
}

#[cfg(test)]
mod sd_pullup_tests {
    use crate::testutil::{board, pin};
    use crate::{Component, ExtractedBoard, LintCheck, Pin, Severity};

    fn part(reference: &str, value: &str, footprint: &str, pins: Vec<Pin>) -> Component {
        crate::testutil::part(reference, value, "", footprint, pins)
    }

    /// Host MCU + card socket, no pull-up: CMD fires Medium, DAT1 fires Low.
    #[test]
    fn host_and_socket_without_pullup_fires() {
        let board = board(
            &[(1, "sd_cmd"), (2, "sd_data_1")],
            vec![
                part(
                    "U1",
                    "STM32H533VET6",
                    "LQFP-100",
                    vec![pin("1", Some(1)), pin("2", Some(2))],
                ),
                part(
                    "J3",
                    "0472192001",
                    "microSD_socket",
                    vec![pin("3", Some(1)), pin("5", Some(2))],
                ),
            ],
        );
        let report = board.net_lint();
        let f: Vec<_> = report.of_check(LintCheck::MissingSdPullup).collect();
        assert_eq!(f.len(), 2, "CMD and DAT1 both lack pull-ups: {f:?}");
        let cmd = f
            .iter()
            .find(|x| x.nets == ["sd_cmd"])
            .expect("cmd finding");
        assert_eq!(cmd.severity, Severity::Medium);
        let dat1 = f
            .iter()
            .find(|x| x.nets == ["sd_data_1"])
            .expect("dat1 finding");
        assert_eq!(dat1.severity, Severity::Low);
    }

    /// The same bus with a 10k to a rail is clean, and the pulled net alone
    /// goes quiet (per-net check).
    #[test]
    fn pullup_to_rail_clears_the_net() {
        let board = board(
            &[(1, "sd_cmd"), (3, "+3V3")],
            vec![
                part("U1", "MCU", "LQFP-100", vec![pin("1", Some(1))]),
                part("J3", "SOCKET", "microSD_socket", vec![pin("3", Some(1))]),
                part(
                    "R11",
                    "10k",
                    "R_0402",
                    vec![pin("1", Some(1)), pin("2", Some(3))],
                ),
            ],
        );
        let report = board.net_lint();
        assert_eq!(report.of_check(LintCheck::MissingSdPullup).count(), 0);
    }

    /// Socket wired straight to a header (no on-board host): an adapter
    /// board, where the pull-ups live on the host. Must stay silent.
    #[test]
    fn breakout_without_host_is_silent() {
        let board = board(
            &[(1, "sd_cmd")],
            vec![
                part("J1", "HEADER", "PinHeader_1x08", vec![pin("1", Some(1))]),
                part("J2", "SOCKET", "microSD_socket", vec![pin("3", Some(1))]),
            ],
        );
        let report = board.net_lint();
        assert_eq!(report.of_check(LintCheck::MissingSdPullup).count(), 0);
    }

    /// An SDRAM bus is conventionally named `SD_D0..SD_D15` but has no card
    /// socket, only the MCU and the SDRAM chip. The check must stay silent:
    /// the net NAME is not evidence a card exists.
    #[test]
    fn sdram_bus_named_sd_d0_is_silent() {
        let board = board(
            &[(1, "SD_D0"), (2, "SD_D3")],
            vec![
                part(
                    "U1",
                    "STM32F429ZIT6",
                    "LQFP-144",
                    vec![pin("1", Some(1)), pin("2", Some(2))],
                ),
                part(
                    "U2",
                    "MT48LC16M16A2",
                    "TSOP-II-54",
                    vec![pin("3", Some(1)), pin("4", Some(2))],
                ),
            ],
        );
        let report = board.net_lint();
        assert_eq!(
            report.of_check(LintCheck::MissingSdPullup).count(),
            0,
            "an SDRAM bus named SD_D0.. must not fire the SD card check"
        );
    }

    /// Hierarchical KiCad names put the card context in the sheet path and
    /// only the role in the leaf; the check must still see the bus.
    #[test]
    fn hierarchical_sheet_name_still_fires() {
        let board = board(
            &[(1, "/uSD Connector/CMD")],
            vec![
                part("U1", "MCU", "LQFP-100", vec![pin("1", Some(1))]),
                part("J3", "SOCKET", "microSD_socket", vec![pin("3", Some(1))]),
            ],
        );
        let report = board.net_lint();
        assert_eq!(report.of_check(LintCheck::MissingSdPullup).count(), 1);
    }

    /// A DNP pull-up must not silently clear the finding, on EITHER dnp
    /// policy. Under `--honour-dnp` the part is absent and the plain Medium
    /// finding fires; under the fit-except-links policy the part is
    /// simulation-fitted, and the check must say the assembled board still
    /// floats instead of going green (the policy marks such parts with
    /// `DNP_FITTED_KEY` exactly so presence-class lints can tell).
    #[test]
    fn dnp_pullup_does_not_silently_clear() {
        use crate::dnp::DnpPolicy;
        let make = || {
            let mut r = part(
                "R11",
                "10k",
                "R_0402",
                vec![pin("1", Some(1)), pin("2", Some(3))],
            );
            r.dnp = true;
            board(
                &[(1, "sd_cmd"), (3, "+3V3"), (4, "sd_data_1")],
                vec![
                    // The host reaches DAT1 too, so the socket reads as
                    // SD-mode wiring (the SPI heuristic would otherwise
                    // soften the severity).
                    part(
                        "U1",
                        "MCU",
                        "LQFP-100",
                        vec![pin("1", Some(1)), pin("2", Some(4))],
                    ),
                    part(
                        "J3",
                        "SOCKET",
                        "microSD_socket",
                        vec![pin("3", Some(1)), pin("5", Some(4))],
                    ),
                    r,
                ],
            )
        };

        let cmd_findings = |board: &ExtractedBoard| -> Vec<(Severity, String)> {
            board
                .net_lint()
                .of_check(LintCheck::MissingSdPullup)
                .filter(|x| x.nets == ["sd_cmd"])
                .map(|x| (x.severity, x.message.clone()))
                .collect()
        };
        let mut honoured = make();
        honoured
            .apply_dnp_policy(DnpPolicy::Honour, &[], &[])
            .unwrap();
        let f = cmd_findings(&honoured);
        assert_eq!(f.len(), 1, "honoured DNP: the line has no pull-up at all");
        assert_eq!(f[0].0, Severity::Medium);
        // The message names the resistor that is drawn but not fitted, and does
        // not claim this run fits it (the default policy leaves it out).
        assert!(
            f[0].1.contains("R11") && f[0].1.contains("do-not-populate"),
            "the finding names the unfitted part: {}",
            f[0].1
        );
        assert!(
            !f[0].1.contains(crate::DNP_FITTED_NOTE_MARKER),
            "{}",
            f[0].1
        );

        let mut fitted = make();
        fitted
            .apply_dnp_policy(DnpPolicy::FitExceptLinks, &[], &[])
            .unwrap();
        let f = cmd_findings(&fitted);
        assert_eq!(
            f.len(),
            1,
            "policy-fitted DNP pull-up must surface, not silently satisfy the check"
        );
        // Same severity the absent-part case earns: a simulation-policy flag
        // must not decide whether CI gates on a fab-real defect.
        assert_eq!(f[0].0, Severity::Medium);
        assert!(
            f[0].1.contains("R11") && f[0].1.contains("do-not-populate"),
            "the finding names the unfitted part: {}",
            f[0].1
        );

        // A part the user explicitly fits is a build decision, not a policy
        // assumption: genuinely clean.
        let mut named = make();
        named
            .apply_dnp_policy(DnpPolicy::FitExceptLinks, &["R11".to_string()], &[])
            .unwrap();
        assert_eq!(cmd_findings(&named).len(), 0);
    }

    /// How many `missing_sd_pullup` findings the lint raises on `board`.
    fn sd_pullup_findings(board: &ExtractedBoard) -> usize {
        board
            .net_lint()
            .of_check(LintCheck::MissingSdPullup)
            .count()
    }

    /// SD buses routinely interpose a small series damper between host and
    /// socket; a pull-up on the far side of it is a real pull-up.
    #[test]
    fn pullup_across_series_damper_clears() {
        let board = board(
            &[(1, "sd_cmd"), (2, "sd_cmd_mcu"), (3, "+3V3")],
            vec![
                part("J3", "SOCKET", "microSD_socket", vec![pin("3", Some(1))]),
                part(
                    "R5",
                    "33",
                    "R_0402",
                    vec![pin("1", Some(1)), pin("2", Some(2))],
                ),
                part("U1", "MCU", "LQFP-100", vec![pin("1", Some(2))]),
                part(
                    "R11",
                    "10k",
                    "R_0402",
                    vec![pin("1", Some(2)), pin("2", Some(3))],
                ),
                // An ESD array on the socket-side net so the active-part
                // guard is not what saves this board.
                part(
                    "D2",
                    "TPD4E05U06",
                    "SOT-23-6",
                    vec![
                        pin("1", Some(1)),
                        pin("2", None),
                        pin("3", None),
                        pin("4", None),
                    ],
                ),
            ],
        );
        assert_eq!(
            sd_pullup_findings(&board),
            0,
            "the 10k behind the 33R damper is the pull-up"
        );
    }

    /// A pass-gate card mux (FSA2567) or a TXS-class translator with integrated
    /// pull-ups on the bus puts the bias topology beyond one-net reasoning: a
    /// discrete resistor is not the whole story, so abstain.
    #[test]
    fn bus_switch_or_integrated_pullup_translator_abstains() {
        for (reference, value, package) in
            [("U7", "FSA2567", "QFN-16"), ("U9", "TXS0108E", "TSSOP-20")]
        {
            let between = vec![pin("2", Some(1)), pin("3", None), pin("4", None)];
            let board = board(
                &[(1, "sd_cmd")],
                vec![
                    part("U1", "MCU", "LQFP-100", vec![pin("1", Some(1))]),
                    part(reference, value, package, between),
                    part("J3", "SOCKET", "microSD_socket", vec![pin("3", Some(1))]),
                ],
            );
            assert_eq!(sd_pullup_findings(&board), 0, "{value}");
        }
    }

    /// A soldered-down eMMC is the card; the JEDEC pull-up requirement is the
    /// same, socket or no socket.
    #[test]
    fn down_emmc_without_pullup_fires() {
        let board = board(
            &[(1, "EMMC_CMD")],
            vec![
                part("U1", "iMX6ULL", "BGA-289", vec![pin("A1", Some(1))]),
                part("U8", "KLM8G1GETF-B041", "BGA-153", vec![pin("M5", Some(1))]),
            ],
        );
        let f: Vec<_> = {
            let r = board.net_lint();
            r.of_check(LintCheck::MissingSdPullup)
                .map(|f| f.severity)
                .collect()
        };
        assert_eq!(f, vec![Severity::Medium]);
    }

    /// A socket wired for SPI mode (DAT1/DAT2 unused) still deserves the
    /// advisory, but at note severity and without the SD-mode open-drain
    /// rationale, which would be false for that board.
    #[test]
    fn spi_wired_socket_downgrades_to_note() {
        let board = board(
            &[(1, "/uSD/CMD"), (2, "/uSD/DAT0")],
            vec![
                part(
                    "U1",
                    "SAMD21",
                    "TQFP-48",
                    vec![pin("1", Some(1)), pin("2", Some(2))],
                ),
                part(
                    "J3",
                    "SOCKET",
                    "microSD_socket",
                    vec![pin("3", Some(1)), pin("7", Some(2))],
                ),
            ],
        );
        let report = board.net_lint();
        let f: Vec<_> = report.of_check(LintCheck::MissingSdPullup).collect();
        assert_eq!(f.len(), 2);
        assert!(f.iter().all(|x| x.severity == Severity::Low), "{f:?}");
        assert!(
            f.iter()
                .all(|x| x.message.contains(super::SPI_PULLUP_MESSAGE_MARKER)),
            "the plain renderer keys on the marker, so the message must carry it: {f:?}"
        );
    }

    /// A socket whose lines are named for both SPI and SDIO roles
    /// (`SPI_CIPO1/SDIO_DAT0`) is run in SPI mode, even though the host also
    /// reaches DAT1/DAT2: CMD and DAT0 get the note, not the SD-mode warning.
    /// The same socket named only for SDIO keeps the warning.
    #[test]
    fn spi_named_sd_lines_downgrade_to_note() {
        let socket_board = |names: [&'static str; 4]| {
            board(
                &[(1, names[0]), (2, names[1]), (3, names[2]), (4, names[3])],
                vec![
                    part(
                        "U1",
                        "RP2040",
                        "QFN-56",
                        vec![
                            pin("1", Some(1)),
                            pin("2", Some(2)),
                            pin("3", Some(3)),
                            pin("4", Some(4)),
                        ],
                    ),
                    part(
                        "J6",
                        "SOCKET",
                        "microSD_socket",
                        vec![
                            pin("3", Some(1)),
                            pin("7", Some(2)),
                            pin("8", Some(3)),
                            pin("1", Some(4)),
                        ],
                    ),
                ],
            )
        };
        let cmd_dat0 = |b: &ExtractedBoard| -> Vec<(Severity, bool)> {
            let report = b.net_lint();
            report
                .of_check(LintCheck::MissingSdPullup)
                .filter(|f| f.message.contains("CMD") || f.message.contains("DAT0 net"))
                .map(|f| {
                    (
                        f.severity,
                        f.message.contains(super::SPI_PULLUP_MESSAGE_MARKER),
                    )
                })
                .collect()
        };
        let spi = socket_board([
            "SPI_COPI1/SDIO_CMD",
            "SPI_CIPO1/SDIO_DAT0",
            "SDIO_DAT1",
            "SDIO_DAT2",
        ]);
        assert_eq!(
            cmd_dat0(&spi),
            vec![(Severity::Low, true), (Severity::Low, true)]
        );
        let sdio = socket_board(["SDIO_CMD", "SDIO_DAT0", "SDIO_DAT1", "SDIO_DAT2"]);
        assert_eq!(
            cmd_dat0(&sdio),
            vec![(Severity::Medium, false), (Severity::Medium, false)]
        );
    }

    /// A DAT3 pull-DOWN (the card-detect convention) to ground must never be
    /// credited as the pull-up, even when a split-ground stitching cap makes
    /// ground look rail-like.
    #[test]
    fn ground_pulldown_is_not_credited() {
        let board = board(
            &[(1, "sd_cmd"), (8, "GND"), (9, "AGND")],
            vec![
                part("U1", "MCU", "LQFP-100", vec![pin("1", Some(1))]),
                part("J3", "SOCKET", "microSD_socket", vec![pin("3", Some(1))]),
                part(
                    "R9",
                    "100k",
                    "R_0402",
                    vec![pin("1", Some(1)), pin("2", Some(8))],
                ),
                part(
                    "C9",
                    "100n",
                    "C_0402",
                    vec![pin("1", Some(8)), pin("2", Some(9))],
                ),
            ],
        );
        assert_eq!(sd_pullup_findings(&board), 1);
    }
}

#[cfg(test)]
mod parse_ohms_tests {
    use super::parse_ohms;

    #[test]
    fn leading_r_sub_ohm_notation_parses() {
        // "R47" = 0.47 Ω (the leading-R sub-1-ohm marking). The
        // empty integer part must not fail the parse: a failure here propagates
        // via `?` and aborts the whole rail-resistor search, silently skipping
        // the LED-current check on a near-dead-short.
        assert_eq!(parse_ohms("R47"), Some(0.47));
        assert_eq!(parse_ohms("r47"), Some(0.47));
        // The other leading/trailing-R spellings.
        assert_eq!(parse_ohms("4R7"), Some(4.7));
        assert_eq!(parse_ohms("47R"), Some(47.0));
        assert_eq!(parse_ohms("0R"), Some(0.0));
        assert_eq!(parse_ohms("4K7"), Some(4700.0));
        assert_eq!(parse_ohms("330"), Some(330.0));
    }

    #[test]
    fn spice_meg_multiplier_parses() {
        // "10MEG" landed on the single 'M' and misparsed to None, dropping
        // a 10 MΩ resistor from the pull-up / LED-current analyses. MEG/GIG must
        // be matched before the single-letter scan.
        assert_eq!(parse_ohms("10MEG"), Some(1e7));
        assert_eq!(parse_ohms("2GIG"), Some(2e9));
        // 4M7 single-letter decimal notation is still 4.7 MΩ.
        assert_eq!(parse_ohms("4M7"), Some(4.7e6));
    }

    #[test]
    fn parse_ohms_matches_the_canonical_parser() {
        // The "/footprint" qualifier (Olimex "2.2k/R0603") must
        // be tolerated, net-lint dropped it and returned None, silently
        // disabling the LED-current check for that resistor.
        assert_eq!(parse_ohms("2.2k/R0603"), Some(2200.0));
        assert_eq!(parse_ohms("330R/R0603"), Some(330.0));
        // Lowercase 'm' is milli, not mega.
        assert_eq!(parse_ohms("2m2"), Some(0.0022));
        // Inline tolerance annotations are tolerated.
        assert_eq!(parse_ohms("10k 1%"), Some(10_000.0));
    }
}

#[cfg(test)]
mod mirror_and_pad_tests {
    use super::{connected_pads, is_capacitor, passive_prefix, ref_designator};
    use crate::testutil::{part, pin};
    use crate::Component;

    fn comp(reference: &str, footprint: &str, pads: &[(&str, Option<i64>)]) -> Component {
        let pins = pads.iter().map(|(num, net)| pin(num, *net)).collect();
        part(reference, "", "", footprint, pins)
    }

    #[test]
    fn split_keyboard_mirror_prefix_is_stripped_before_classifying() {
        // Si.rs strips the split-keyboard mirror `r` prefix (rC2 -> C2), but
        // netlint's classifiers uppercased the raw reference, so a mirrored-half
        // decoupling cap `rC2` read as an `R` designator: is_capacitor missed it
        // and check_design_file_qc raised a bogus DesignatorFootprintMismatch
        // ("rC2 is an R designator but uses a C footprint") on a clean corpus board.
        assert_eq!(ref_designator("rC2"), "C2");
        assert_eq!(ref_designator("R5"), "R5"); // genuine R untouched
        assert_eq!(ref_designator("rR3"), "R3");

        // The prefix (used by the designator/footprint QC) now agrees with the
        // footprint family for a mirrored cap.
        assert_eq!(passive_prefix("rC2"), Some('C'));
        assert_eq!(passive_prefix("rR3"), Some('R'));
        assert_eq!(passive_prefix("R5"), Some('R'));

        // is_capacitor recognises the mirrored cap.
        let cap = comp(
            "rC2",
            "Capacitor_SMD:C_0402_1005Metric",
            &[("1", Some(1)), ("2", Some(2))],
        );
        assert!(
            is_capacitor(&cap),
            "mirrored-half cap must classify as a capacitor"
        );
    }

    #[test]
    fn connected_pads_dedups_repeated_pad_numbers() {
        // Some extractors (IPC-356 top+bottom access records, Eagle .brd
        // per-contact listing, both-sided through-hole pads) list a pad more than
        // once. Counting raw net-carrying pin ENTRIES made a two-terminal part
        // look like 3+ terminals, so is_resistor/is_capacitor (which gate on
        // `== 2`) silently dropped it. Dedup by distinct pad number, like si.rs.
        let r = comp(
            "R1",
            "Resistor_SMD:R_0402",
            &[
                ("1", Some(5)),
                ("1", Some(5)),
                ("2", Some(6)),
                ("2", Some(6)),
            ],
        );
        assert_eq!(
            connected_pads(&r),
            2,
            "four entries over two pads = two pads"
        );
    }
}

#[cfg(test)]
mod wired_or_tests {
    use super::is_wired_or_name;

    #[test]
    fn wired_or_names_match_whole_words_not_substrings() {
        // Substring matching suppressed the output-contention ERC on any
        // net whose name merely CONTAINED "INT"/"PG"/etc. Genuine open-drain lines
        // must still be recognised; unrelated push-pull signal nets must not be.
        // Recognised (whole-word, incl. active-low N-prefix and digit index):
        for name in [
            "IRQ",
            "EDP_IRQ",
            "SENSOR_INT",
            "INT1",
            "NINT",
            "NIRQ",
            "PG",
            "PGOOD",
            "PG_3V3",
            "ALERT_N",
            "NMI",
            "NFLT",
            "CPU_RDY",
            "BUSY0",
        ] {
            assert!(
                is_wired_or_name(name),
                "{name} is a wired-OR/open-drain line"
            );
        }
        // NOT recognised, "INT"/"PG" only appear mid-word, so these push-pull
        // signal nets must stay in the contention check:
        for name in [
            "SETPOINT_DAC",
            "PRINT_HEAD",
            "MIDPOINT",
            "INTERNAL_CLK",
            "SPGND_SENSE",
            "SPRING_A",
        ] {
            assert!(
                !is_wired_or_name(name),
                "{name} must not be treated as wired-OR"
            );
        }
    }
}

#[cfg(test)]
mod passive_prefix_tests {
    use super::passive_prefix;

    #[test]
    fn led_speaker_ldr_lcd_are_not_inductors() {
        // The 'L' arm classified every L-prefixed designator as an inductor,
        // so an LED/LS/LDR/LCD with a blank value fired a false "set the actual L
        // value" placeholder finding (and an LDR on a resistor footprint fired a
        // false designator/footprint mismatch). Only real inductors are 'L'.
        assert_eq!(passive_prefix("LED1"), None);
        assert_eq!(passive_prefix("LS1"), None);
        assert_eq!(passive_prefix("LDR1"), None);
        assert_eq!(passive_prefix("LCD1"), None);
        // Genuine inductors still classify.
        assert_eq!(passive_prefix("L1"), Some('L'));
        assert_eq!(passive_prefix("L23"), Some('L'));
        // The sibling R/C arms are unchanged.
        assert_eq!(passive_prefix("R5"), Some('R'));
        assert_eq!(passive_prefix("C10"), Some('C'));
        assert_eq!(passive_prefix("RN1"), None);
    }
}

#[cfg(test)]
mod rail_and_cap_tests {
    use super::parse_capacitance_uf;

    #[test]
    fn r_style_capacitance_keeps_the_fractional_part() {
        // "4u7" = 4.7 uF (unit letter as the decimal point); the trailing
        // digit was dropped, under-reporting the value.
        assert_eq!(parse_capacitance_uf("4u7"), Some(4.7));
        assert_eq!(parse_capacitance_uf("1u5"), Some(1.5));
        assert_eq!(parse_capacitance_uf("2n2"), Some(2.2 / 1000.0));
        // Explicit-decimal and unit-suffixed forms unchanged.
        assert_eq!(parse_capacitance_uf("4.7uF"), Some(4.7));
        assert_eq!(parse_capacitance_uf("10u"), Some(10.0));
        assert_eq!(parse_capacitance_uf("100nF"), Some(0.1));
    }

    #[test]
    fn attached_voltage_rating_is_not_read_as_a_capacitance_fraction() {
        // The R-style-decimal frac loop ran even when the unit already
        // spelled out the Farad ("uF"), so "1uF25V" ate the "25" rating and
        // became 1.25 uF, false-flagging a valid 0201 (1 uF ceiling) cap on the
        // package-ceiling check. When the 'F' is present the value is complete
        // and trailing digits are a rating, not a fraction.
        assert_eq!(parse_capacitance_uf("1uF25V"), Some(1.0));
        assert_eq!(parse_capacitance_uf("10uF50V"), Some(10.0));
        assert_eq!(parse_capacitance_uf("100nF16V"), Some(0.1));
        // Bare-prefix R-style decimals still fold the trailing digits in.
        assert_eq!(parse_capacitance_uf("4u7"), Some(4.7));
    }
}

#[cfg(test)]
mod placeholder_jumper_exemption_tests {
    use crate::testutil::board;
    use crate::{Component, LintCheck};

    fn comp(reference: &str, value: &str, lib_id: &str, footprint: &str) -> Component {
        crate::testutil::part(reference, value, lib_id, footprint, Vec::new())
    }

    fn placeholder_count(components: Vec<Component>) -> usize {
        board(&[], components)
            .net_lint()
            .of_check(LintCheck::PlaceholderValue)
            .count()
    }

    #[test]
    fn solder_jumpers_with_empty_value_are_not_placeholders() {
        // The verified corpus false fire: Arduino Uno R3 RESET-EN, an Eagle
        // solder jumper (library "jumper", package "SJ") whose value is ""
        // BY DESIGN. Its 'R'-leading reference made passive_prefix read it as
        // a resistor, so the placeholder-value lint demanded a value be set.
        // The link-part class must be exempt.
        assert_eq!(
            placeholder_count(vec![comp("RESET-EN", "", "jumper:SJ", "SJ")]),
            0,
            "an Eagle SJ solder jumper is not a placeholder-valued resistor"
        );
        // KiCad conventions, with deliberately passive-looking references so
        // the exemption (not the reference prefix) is what protects them.
        assert_eq!(
            placeholder_count(vec![
                comp(
                    "R100",
                    "",
                    "Jumper:SolderJumper_2_P1.3mm_Open",
                    "Jumper:SolderJumper_2_P1.3mm_Open_RoundedPad1.0x1.5mm"
                ),
                comp("L7", "", "Device:Net-Tie_2", "NetTie:NetTie-2_SMD_Pad0.5mm"),
            ]),
            0,
            "KiCad solder jumpers / net ties are not placeholder-valued passives"
        );
    }

    #[test]
    fn unbought_parts_with_unset_values_are_not_placeholders() {
        // A DNP part, a DNP-policy-fitted one, and a part excluded from the
        // BOM are never purchased, so an unset value on them is not a defect.
        let mut dnp = comp("R1", "", "Device:R", "Resistor_SMD:R_0603_1608Metric");
        dnp.dnp = true;
        let mut fitted = comp("R2", "", "Device:R", "Resistor_SMD:R_0603_1608Metric");
        fitted
            .properties
            .push((crate::dnp::DNP_FITTED_KEY.to_string(), String::new()));
        let mut no_bom = comp("C3", "?", "Device:C", "Capacitor_SMD:C_0402_1005Metric");
        no_bom
            .properties
            .push((crate::dnp::EXCLUDED_FROM_BOM_KEY.to_string(), String::new()));
        assert_eq!(placeholder_count(vec![dnp, fitted, no_bom]), 0);
    }

    #[test]
    fn genuine_unset_passives_still_fire() {
        // The exemption must not blunt the real check: a plain resistor /
        // capacitor with an empty or bare-letter value is still a defect.
        assert_eq!(
            placeholder_count(vec![
                comp("R1", "", "Device:R", "Resistor_SMD:R_0603_1608Metric"),
                comp("C2", "?", "Device:C", "Capacitor_SMD:C_0402_1005Metric"),
            ]),
            2,
            "real unset passives must keep firing"
        );
    }
}

#[cfg(test)]
mod socketed_chip_tests {
    use super::is_connector_like;
    use crate::testutil::{part, pin};
    use crate::Component;

    fn chip(reference: &str, value: &str, lib_id: &str, footprint: &str, pads: usize) -> Component {
        let pins = (1..=pads)
            .map(|n| pin(&n.to_string(), Some(i64::try_from(n).unwrap())))
            .collect();
        part(reference, value, lib_id, footprint, pins)
    }

    /// A DIP chip plugged into a socket footprint, and an analog-switch IC
    /// drawn as `SW1`, are chips: their footprint's "socket" and their
    /// designator's "SW" are not evidence of a connector.
    #[test]
    fn a_chip_in_a_socket_or_under_a_switch_designator_is_not_a_connector() {
        let socketed = chip(
            "U3",
            "ATtiny85",
            "MCU_Microchip_ATtiny:ATtiny85-20P",
            "Package_DIP:DIP-8_W7.62mm_Socket",
            8,
        );
        assert!(!is_connector_like(&socketed), "socketed DIP chip");
        let analog_switch = chip(
            "SW1",
            "TS3A5017",
            "Analog_Switch:TS3A5017",
            "Package_SO:SOIC-16_3.9x9.9mm_P1.27mm",
            16,
        );
        assert!(!is_connector_like(&analog_switch), "SW1 analog-switch IC");
        let socket_value = chip(
            "IC2",
            "74HC595 socket",
            "74xx:74HC595",
            "Package_DIP:DIP-16_W7.62mm",
            16,
        );
        assert!(
            !is_connector_like(&socket_value),
            "chip with socket in its value"
        );
    }

    /// A programmer's target socket drawn as `U5` with a value that names the
    /// family or counts pins is the socket, not a chip in it.
    #[test]
    fn a_target_socket_valued_by_family_or_pin_count_stays_a_socket() {
        let eeprom = chip(
            "U1",
            "24Cxx",
            "Device:Foo",
            "Package_DIP:DIP-8_W7.62mm_Socket_LongPads",
            8,
        );
        assert!(is_connector_like(&eeprom), "24Cxx target socket");
        let pic = chip(
            "U5",
            "PIC_18_PINS",
            "Device:Foo",
            "Package_DIP:DIP-18_W7.62mm_Socket_LongPads",
            18,
        );
        assert!(is_connector_like(&pic), "PIC_18_PINS target socket");
    }

    /// The same words on a part that is a socket, a button or a header still
    /// read as connectors.
    #[test]
    fn real_sockets_buttons_and_headers_stay_connector_like() {
        let socket = chip(
            "XU1",
            "DIP-8 socket",
            "Connector_Generic:Conn_02x04_Odd_Even",
            "Package_DIP:DIP-8_W7.62mm_Socket",
            8,
        );
        assert!(is_connector_like(&socket), "an empty IC socket");
        let header = chip(
            "P1",
            "Conn_01x08",
            "Connector_Generic:Conn_01x08",
            "Connector_PinSocket_2.54mm:PinSocket_1x08_P2.54mm_Vertical",
            8,
        );
        assert!(is_connector_like(&header), "a pin socket");
        let button = chip(
            "SW2",
            "SW_Push",
            "Switch:SW_Push",
            "Button_Switch_SMD:SW_SPST_TL3342",
            2,
        );
        assert!(is_connector_like(&button), "a push button");
        let dip_switch = chip(
            "SW3",
            "SW_DIP_x04",
            "Switch:SW_DIP_x04",
            "Button_Switch_THT:SW_DIP_SPSTx04_Slide_9.78x12.34mm_W7.62mm_P2.54mm",
            8,
        );
        assert!(is_connector_like(&dip_switch), "a DIP switch");
    }
}

#[cfg(test)]
mod zero_ohm_capacitor_position_tests {
    use crate::testutil::{board, part};
    use crate::{Component, LintCheck};

    fn comp(reference: &str, value: &str, lib_id: &str, footprint: &str) -> Component {
        part(reference, value, lib_id, footprint, Vec::new())
    }

    fn zero_ohm_findings(components: Vec<Component>) -> Vec<String> {
        board(&[], components)
            .net_lint()
            .of_check(LintCheck::ValuePackageSanity)
            .map(|f| f.message.clone())
            .collect()
    }

    const CAP_FP: &str = "Capacitor_SMD:C_0402_1005Metric";

    #[test]
    fn a_capacitor_position_fitted_with_a_zero_ohm_value_is_flagged() {
        for value in ["0", "0R", "0 ohm", "0\u{3a9}", "0.0"] {
            let found = zero_ohm_findings(vec![comp("C40", value, "Device:C", CAP_FP)]);
            assert_eq!(found.len(), 1, "{value}: {found:?}");
            assert!(
                found[0].contains("C40") && found[0].contains("0 ohm link"),
                "{found:?}"
            );
        }
    }

    #[test]
    fn a_resistor_class_symbol_or_footprint_under_any_library_naming_is_silent() {
        // (symbol, footprint): the project's own naming, none of it KiCad's stock
        // `Device:R` / `Resistor_SMD:`. Either side naming a resistor is enough.
        let forms = [
            ("myproj:R_0603", "myproj:R0603_HandSolder"),
            ("project_lib:Resistor", "project_lib:0603"),
            ("Device:RES", "Footprints:Part"),
            ("Device:R_Small_US", "Footprints:Part"),
            ("Device:R_US", "Resistors_SMD:R_0805"),
            ("proj:Part", "0603_RES"),
            ("proj:Part", "RES_0603"),
            ("rcl:R-EU_R0603", "rcl:R0603"),
            ("proj:Part", "RESC1608X55N"),
            ("proj:Part", "RESMELF3516X150N"),
            ("proj:Part", "Resistors_SMD:0805"),
            ("proj:Part", "proj:R1206W"),
            (
                "proj:Part",
                "Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm_P10.16mm",
            ),
            ("Resistors:Part", "proj:Part"),
        ];
        for (n, (lib_id, footprint)) in forms.into_iter().enumerate() {
            let found = zero_ohm_findings(vec![comp(&format!("C{n}"), "0R", lib_id, footprint)]);
            assert!(found.is_empty(), "{lib_id} / {footprint}: {found:?}");
        }
    }

    #[test]
    fn a_capacitor_class_symbol_or_footprint_carrying_zero_ohms_is_flagged() {
        // Real capacitor naming, stock or project-specific, with nothing marking a link.
        let forms = [
            ("Device:C", "Capacitor_SMD:C_0603_1608Metric"),
            ("Device:C_Small", "myproj:C_0603"),
            ("Device:CP", "Capacitor_THT:CP_Radial_D5.0mm_P2.00mm"),
            (
                "Device:C_Polarized",
                "Capacitor_THT:C_Disc_D3.0mm_W1.6mm_P2.50mm",
            ),
            ("myproj:Cap", "CAPC1608X55N"),
            ("rcl:C-EU", "rcl:C0603"),
            ("proj:Part", "CAPAE530X610N"),
            ("proj:Part", "0603_CAP"),
            // Not resistor-named: `Reset` and `Resonator` merely start with `res`.
            ("proj:Reset_Link", "proj:Part"),
            ("proj:Part", "Crystal:Resonator_SMD_0603"),
            ("proj:Part", "Footprints:Part"),
        ];
        for (n, (lib_id, footprint)) in forms.into_iter().enumerate() {
            let found = zero_ohm_findings(vec![comp(&format!("C{n}"), "0", lib_id, footprint)]);
            assert_eq!(found.len(), 1, "{lib_id} / {footprint}: {found:?}");
        }
    }

    #[test]
    fn a_capacitor_symbol_on_a_resistor_footprint_is_flagged() {
        // The symbol says capacitor, the land says resistor: the record disagrees
        // with itself, so the 0 ohm value is not a deliberate link.
        for footprint in [
            "Resistor_SMD:R_0402_1005Metric",
            "myproj:R0603_HandSolder",
            "RESC1608X55N",
        ] {
            let found = zero_ohm_findings(vec![comp("C9", "0R", "Device:C", footprint)]);
            assert_eq!(found.len(), 1, "{footprint}: {found:?}");
        }
        // The same land under a resistor symbol is the deliberate option.
        assert!(zero_ohm_findings(vec![comp(
            "C9",
            "0R",
            "Device:R",
            "myproj:R0603_HandSolder"
        )])
        .is_empty());
    }

    #[test]
    fn real_capacitors_and_deliberate_link_options_are_silent() {
        // A real capacitor value is silent.
        assert!(zero_ohm_findings(vec![comp("C1", "100n", "Device:C", CAP_FP)]).is_empty());
        // A zero-ohm option the designer marked do-not-populate is silent.
        let mut dnp = comp("C2", "0", "Device:C", CAP_FP);
        dnp.dnp = true;
        assert!(zero_ohm_findings(vec![dnp]).is_empty());
        // A zero-ohm option drawn as a resistor in a C-designated slot is deliberate.
        let resistor = comp("C3", "0R", "Device:R", "Resistor_SMD:R_0402_1005Metric");
        assert!(zero_ohm_findings(vec![resistor]).is_empty());
        // The same in an Eagle library: a resistor package under a C designator.
        let eagle = comp("C5", "0R", "resistor:R0603", "R0603");
        assert!(zero_ohm_findings(vec![eagle]).is_empty());
        // A solder jumper or net tie is a link by class.
        let jumper = comp(
            "C4",
            "0",
            "Jumper:SolderJumper_2_Open",
            "Jumper:SolderJumper_2_P1.3mm_Open",
        );
        assert!(zero_ohm_findings(vec![jumper]).is_empty());
        // A zero-ohm resistor in a resistor position is the ordinary link.
        assert!(zero_ohm_findings(vec![comp(
            "R5",
            "0",
            "Device:R",
            "Resistor_SMD:R_0402_1005Metric"
        )])
        .is_empty());
    }
}
