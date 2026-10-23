//! The names a pin goes by, read once.
//!
//! A schematic symbol names its pins (`XTAL1`, `~{RESET}`, `PB6/XTAL1`, `VDD`)
//! and a model card names the same pads in its own words (`pb6_xtal1`, `reset`,
//! `vcc`). Two surfaces that read the same pad must agree on what it is, and
//! the question "do these two names describe the same pin" is asked from more
//! than one place, so the answer is here: [`functions_named`] reads a pin name
//! into the set of [`PinFunction`]s it states.
//!
//! Only functions whose meaning does not depend on the viewpoint of the reader
//! are listed. A crystal input, the reset pin, a supply, ground, a bus data or
//! clock line or a debug port pin is the same pin from every side. `TX`/`RX`
//! and `MOSI`/`MISO` are not: a module symbol may name a pin by the signal it
//! sends or the signal it takes, so two names that differ there are not a
//! contradiction and are left unread. A name that states no listed function
//! (`PA4`, `GPIO7`, `EN`, `1`) reads as the empty set: it is not evidence of
//! anything and a caller must stay silent about it.
//!
//! A name states a function when one of its parts does: it is split on every
//! character that is not a letter or a digit, so `PB6/XTAL1`, `pb6_xtal1` and
//! `PB6(XTAL1)` all state [`PinFunction::Xtal1`], and `OSC_IN` is read as the
//! two parts `osc` and `in` joined. Supply, ground and reset parts may carry a
//! trailing index (`GND2`, `VDD1`); the crystal pins' own index is part of what
//! they are (`XTAL1` is not `XTAL2`).

use std::collections::BTreeSet;

/// A pin function that reads the same from either side of the pad.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PinFunction {
    /// The crystal or oscillator input (`XTAL1`, `OSC_IN`, `XI`).
    Xtal1,
    /// The crystal or oscillator output (`XTAL2`, `OSC_OUT`, `XO`).
    Xtal2,
    /// The reset pin (`RESET`, `NRST`, `MCLR`).
    Reset,
    /// A positive supply pin (`VCC`, `VDD`, `AVCC`).
    PositiveSupply,
    /// A ground pin (`GND`, `VSS`, `AGND`).
    Ground,
    /// An analog reference pin (`AREF`, `VREF`).
    AnalogReference,
    /// The I2C data line (`SDA`).
    I2cData,
    /// The I2C clock line (`SCL`).
    I2cClock,
    /// The debug port data line (`SWDIO`).
    SwdData,
    /// The debug port clock line (`SWCLK`).
    SwdClock,
}

impl PinFunction {
    /// Every function, for the drift tests and for a surface that lists them.
    pub const ALL: [Self; 10] = [
        Self::Xtal1,
        Self::Xtal2,
        Self::Reset,
        Self::PositiveSupply,
        Self::Ground,
        Self::AnalogReference,
        Self::I2cData,
        Self::I2cClock,
        Self::SwdData,
        Self::SwdClock,
    ];

    /// The words a reader sees for this function.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Xtal1 => "crystal input (XTAL1)",
            Self::Xtal2 => "crystal output (XTAL2)",
            Self::Reset => "reset",
            Self::PositiveSupply => "positive supply",
            Self::Ground => "ground",
            Self::AnalogReference => "analog reference",
            Self::I2cData => "I2C data (SDA)",
            Self::I2cClock => "I2C clock (SCL)",
            Self::SwdData => "SWD data (SWDIO)",
            Self::SwdClock => "SWD clock (SWCLK)",
        }
    }

    /// Whether this function is a supply or ground pin, whose swap is a short.
    #[must_use]
    pub const fn is_power(self) -> bool {
        matches!(self, Self::PositiveSupply | Self::Ground)
    }

    /// Whether two names for one pad can both be right: the same function, or
    /// a reference and a positive supply. A `VREF` pin is a supply input on a
    /// level translator or a ratiometric converter, and a datasheet may call the
    /// same pad `VCC` or `VREF`, so the pair states no contradiction.
    #[must_use]
    pub const fn agrees_with(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::PositiveSupply, Self::AnalogReference)
                | (Self::AnalogReference, Self::PositiveSupply)
        ) || self as u8 == other as u8
    }
}

/// Whether two sets of functions read from one pad contradict each other: both
/// state something and no function of one agrees with a function of the other.
#[must_use]
pub fn contradict(a: &BTreeSet<PinFunction>, b: &BTreeSet<PinFunction>) -> bool {
    !a.is_empty() && !b.is_empty() && !a.iter().any(|x| b.iter().any(|y| x.agrees_with(*y)))
}

/// A spelling of a function: the lower-case part, whether a trailing index is
/// part of the spelling's meaning (`false`: `gnd2` is `gnd`), and the function.
struct Spelling {
    part: &'static str,
    indexed: bool,
    function: PinFunction,
}

const fn spell(part: &'static str, indexed: bool, function: PinFunction) -> Spelling {
    Spelling {
        part,
        indexed,
        function,
    }
}

/// Every spelling [`functions_named`] recognises. One table, so a spelling is
/// added in one place. A part is matched whole, never as a prefix: `vddio` is
/// listed because `vdd` does not match it.
const SPELLINGS: &[Spelling] = &[
    spell("xtal1", true, PinFunction::Xtal1),
    spell("xtalin", true, PinFunction::Xtal1),
    spell("xtali", true, PinFunction::Xtal1),
    spell("osc1", true, PinFunction::Xtal1),
    spell("oscin", true, PinFunction::Xtal1),
    spell("osci", true, PinFunction::Xtal1),
    spell("xi", true, PinFunction::Xtal1),
    spell("xin", true, PinFunction::Xtal1),
    spell("xtal2", true, PinFunction::Xtal2),
    spell("xtalout", true, PinFunction::Xtal2),
    spell("xtalo", true, PinFunction::Xtal2),
    spell("osc2", true, PinFunction::Xtal2),
    spell("oscout", true, PinFunction::Xtal2),
    spell("osco", true, PinFunction::Xtal2),
    spell("xo", true, PinFunction::Xtal2),
    spell("xout", true, PinFunction::Xtal2),
    spell("reset", false, PinFunction::Reset),
    spell("resetn", false, PinFunction::Reset),
    spell("nreset", false, PinFunction::Reset),
    spell("rst", false, PinFunction::Reset),
    spell("rstn", false, PinFunction::Reset),
    spell("nrst", false, PinFunction::Reset),
    spell("mclr", false, PinFunction::Reset),
    spell("nmclr", false, PinFunction::Reset),
    spell("vcc", false, PinFunction::PositiveSupply),
    spell("vdd", false, PinFunction::PositiveSupply),
    spell("avcc", false, PinFunction::PositiveSupply),
    spell("avdd", false, PinFunction::PositiveSupply),
    spell("vcca", false, PinFunction::PositiveSupply),
    spell("vdda", false, PinFunction::PositiveSupply),
    spell("vccio", false, PinFunction::PositiveSupply),
    spell("vddio", false, PinFunction::PositiveSupply),
    spell("vbat", false, PinFunction::PositiveSupply),
    spell("gnd", false, PinFunction::Ground),
    spell("agnd", false, PinFunction::Ground),
    spell("dgnd", false, PinFunction::Ground),
    spell("pgnd", false, PinFunction::Ground),
    spell("gnda", false, PinFunction::Ground),
    spell("vss", false, PinFunction::Ground),
    spell("vssa", false, PinFunction::Ground),
    spell("avss", false, PinFunction::Ground),
    spell("vssio", false, PinFunction::Ground),
    spell("ground", false, PinFunction::Ground),
    spell("aref", false, PinFunction::AnalogReference),
    spell("avref", false, PinFunction::AnalogReference),
    spell("vref", false, PinFunction::AnalogReference),
    spell("sda", false, PinFunction::I2cData),
    spell("scl", false, PinFunction::I2cClock),
    spell("swdio", false, PinFunction::SwdData),
    spell("swdat", false, PinFunction::SwdData),
    spell("swclk", false, PinFunction::SwdClock),
    spell("swdck", false, PinFunction::SwdClock),
];

/// `part` without its trailing digits, when something is left.
fn without_index(part: &str) -> &str {
    let stripped = part.trim_end_matches(|c: char| c.is_ascii_digit());
    if stripped.is_empty() {
        part
    } else {
        stripped
    }
}

/// The function a single lower-case part states, if it states one.
fn function_of_part(part: &str) -> Option<PinFunction> {
    let bare = without_index(part);
    SPELLINGS
        .iter()
        .find(|s| s.part == part || (!s.indexed && s.part == bare))
        .map(|s| s.function)
}

/// The functions the pin name `name` states: empty when it states none.
///
/// ```
/// use hauksbee_ir::pin_functions::{functions_named, PinFunction};
/// assert!(functions_named("PB6/XTAL1").contains(&PinFunction::Xtal1));
/// assert!(functions_named("OSC_OUT").contains(&PinFunction::Xtal2));
/// assert!(functions_named("PA4").is_empty());
/// ```
#[must_use]
pub fn functions_named(name: &str) -> BTreeSet<PinFunction> {
    let lower = name.to_ascii_lowercase();
    let parts: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|p| !p.is_empty())
        .collect();
    let mut found = BTreeSet::new();
    for (i, part) in parts.iter().enumerate() {
        found.extend(function_of_part(part));
        // `OSC_IN` is two parts spelling one word.
        if let Some(next) = parts.get(i + 1) {
            found.extend(function_of_part(&format!("{part}{next}")));
        }
    }
    found
}

/// A peripheral function a pad can be wired to inside an MCU: the keys of a
/// card's `[models.pin_capability.<key>]` tables.
///
/// These are directional where the peripheral is: `UartTx` is the MCU's own
/// transmit pin. [`peripheral_functions_named`] reads only the names whose
/// meaning is the same from either side of the wire; a UART's direction is
/// decided by the other end, not by what a net is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PeripheralFunction {
    /// SPI clock.
    SpiSck,
    /// SPI controller-out data.
    SpiMosi,
    /// SPI controller-in data.
    SpiMiso,
    /// I2C data.
    I2cSda,
    /// I2C clock.
    I2cScl,
    /// UART transmit.
    UartTx,
    /// UART receive.
    UartRx,
    /// A timer's PWM output.
    Pwm,
}

impl PeripheralFunction {
    /// Every function, for the drift tests and for a surface that lists them.
    pub const ALL: [Self; 8] = [
        Self::SpiSck,
        Self::SpiMosi,
        Self::SpiMiso,
        Self::I2cSda,
        Self::I2cScl,
        Self::UartTx,
        Self::UartRx,
        Self::Pwm,
    ];

    /// The table key on a card (`spi_sck`).
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::SpiSck => "spi_sck",
            Self::SpiMosi => "spi_mosi",
            Self::SpiMiso => "spi_miso",
            Self::I2cSda => "i2c_sda",
            Self::I2cScl => "i2c_scl",
            Self::UartTx => "uart_tx",
            Self::UartRx => "uart_rx",
            Self::Pwm => "pwm",
        }
    }

    /// The words a reader sees for this function.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::SpiSck => "SPI clock (SCK)",
            Self::SpiMosi => "SPI data out (MOSI)",
            Self::SpiMiso => "SPI data in (MISO)",
            Self::I2cSda => "I2C data (SDA)",
            Self::I2cScl => "I2C clock (SCL)",
            Self::UartTx => "UART transmit (TX)",
            Self::UartRx => "UART receive (RX)",
            Self::Pwm => "hardware PWM output",
        }
    }
}

/// The peripheral functions the net or pin name `name` states.
///
/// Only functions that read the same from either side of the wire (SPI, I2C and
/// PWM words) are read. `TX`/`RX` are not read here: whose transmit a name means is
/// the writer's viewpoint, so a UART is decided by [`names_a_uart`] and the
/// part on the other end.
///
/// ```
/// use hauksbee_ir::pin_functions::{peripheral_functions_named, PeripheralFunction};
/// assert!(peripheral_functions_named("SPI_SCK").contains(&PeripheralFunction::SpiSck));
/// assert!(peripheral_functions_named("FAN_PWM").contains(&PeripheralFunction::Pwm));
/// assert!(peripheral_functions_named("CLK").is_empty());
/// ```
#[must_use]
pub fn peripheral_functions_named(name: &str) -> BTreeSet<PeripheralFunction> {
    const WORDS: &[(&str, PeripheralFunction)] = &[
        ("sck", PeripheralFunction::SpiSck),
        ("sclk", PeripheralFunction::SpiSck),
        ("mosi", PeripheralFunction::SpiMosi),
        ("copi", PeripheralFunction::SpiMosi),
        ("miso", PeripheralFunction::SpiMiso),
        ("cipo", PeripheralFunction::SpiMiso),
        ("sda", PeripheralFunction::I2cSda),
        ("scl", PeripheralFunction::I2cScl),
        ("pwm", PeripheralFunction::Pwm),
    ];
    let lower = name.to_ascii_lowercase();
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter_map(|w| WORDS.iter().find(|(k, _)| *k == w).map(|(_, f)| *f))
        .collect()
}

/// Whether `name` names a serial port by a whole word (`UART`, `USART`, `TXD`,
/// `RXD`, `TX`, `RX`), in whatever notation. It says a UART is meant, not which
/// way: see [`peripheral_functions_named`].
///
/// ```
/// use hauksbee_ir::pin_functions::names_a_uart;
/// assert!(names_a_uart("DEBUG_UART_TX") && names_a_uart("RXD1"));
/// assert!(!names_a_uart("TXEN") && !names_a_uart("MOSI"));
/// ```
#[must_use]
pub fn names_a_uart(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| {
        let bare = w.trim_end_matches(|c: char| c.is_ascii_digit());
        matches!(bare, "uart" | "usart" | "tx" | "rx" | "txd" | "rxd")
    })
}

/// The direction of one pin-function token as a UART line.
///
/// `Some(true)` is transmit (`TX`, `TXD`, `TXD0`, `U0TXD`, `UART1_TX`,
/// `USART2_TX`), `Some(false)` receive, `None` anything else. A bus prefix other
/// than a UART one (`EMAC_TXD0`, `ETH_TXD`, `RMII_TXD1`) is a different
/// interface and reads as `None`.
///
/// The direction is the pad's own: a pin named `TXD` on a controller sends. A
/// pin of a peripheral part may name the line from either side, so a caller
/// reads that part's direction from its electrical type, not from this.
///
/// ```
/// use hauksbee_ir::pin_functions::uart_token_direction;
/// assert_eq!(uart_token_direction("USART1_TX"), Some(true));
/// assert_eq!(uart_token_direction("U0RXD"), Some(false));
/// assert_eq!(uart_token_direction("RMII_TXD0"), None);
/// ```
#[must_use]
pub fn uart_token_direction(token: &str) -> Option<bool> {
    let t = token.to_ascii_uppercase();
    let t = ["LPUART", "USART", "UART", "U"]
        .iter()
        .find_map(|p| t.strip_prefix(p))
        .map_or(t.as_str(), |rest| {
            rest.trim_start_matches(|c: char| c.is_ascii_digit())
                .trim_start_matches('_')
        });
    match t.trim_end_matches(|c: char| c.is_ascii_digit()) {
        "TX" | "TXD" => Some(true),
        "RX" | "RXD" => Some(false),
        _ => None,
    }
}

/// The UART direction a card's pad role states.
///
/// Roles such as `pd1_txd`, `pa9_usart1_tx` and `gpio1_u0txd` give `Some(true)`
/// for transmit and `Some(false)` for receive. `None` when the role names no
/// UART line or names both directions.
///
/// ```
/// use hauksbee_ir::pin_functions::uart_role_direction;
/// assert_eq!(uart_role_direction("pd1_txd"), Some(true));
/// assert_eq!(uart_role_direction("pa10_usart1_rx"), Some(false));
/// assert_eq!(uart_role_direction("pb3_mosi_oc2a"), None);
/// ```
#[must_use]
pub fn uart_role_direction(role: &str) -> Option<bool> {
    const NOT_A_UART: [&str; 12] = [
        "en", "clk", "clock", "er", "dv", "crs", "rmii", "mii", "eth", "emac", "ctl", "sync",
    ];
    let lower = role.to_ascii_lowercase();
    let parts: Vec<&str> = lower.split('_').collect();
    let mut dirs = parts.iter().enumerate().filter_map(|(i, part)| {
        let mut neighbours = [i.checked_sub(1), Some(i + 1)]
            .into_iter()
            .flatten()
            .filter_map(|j| parts.get(j));
        if neighbours.any(|n| NOT_A_UART.contains(n)) {
            return None;
        }
        uart_token_direction(part)
    });
    let first = dirs.next()?;
    dirs.all(|d| d == first).then_some(first)
}

/// Whether the pin name `name` states a sensor's analogue output.
///
/// A whole word does: `AOUT`, `ANALOG_OUT`, `VTEMP`, `TEMP_OUT`, `SENSOR_OUT`,
/// `VIOUT`. A bare `OUT` or `VOUT` is not read, since a regulator's output
/// carries the same name.
///
/// ```
/// use hauksbee_ir::pin_functions::names_an_analog_output;
/// assert!(names_an_analog_output("AOUT") && names_an_analog_output("ANALOG_OUT"));
/// assert!(!names_an_analog_output("VOUT") && !names_an_analog_output("OUT"));
/// ```
#[must_use]
pub fn names_an_analog_output(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    words.iter().any(|w| {
        matches!(
            *w,
            "aout" | "analogout" | "vtemp" | "tempout" | "sensorout" | "viout"
        )
    }) || words
        .windows(2)
        .any(|w| matches!((w[0], w[1]), ("analog" | "temp" | "sensor", "out")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uart_directions_are_read_from_tokens_and_roles() {
        for (token, want) in [
            ("TX", Some(true)),
            ("txd0", Some(true)),
            ("U0TXD", Some(true)),
            ("LPUART1_TX", Some(true)),
            ("RXD", Some(false)),
            ("UART2_RX", Some(false)),
            ("TXEN", None),
            ("RMII_TXD0", None),
            ("MOSI", None),
        ] {
            assert_eq!(uart_token_direction(token), want, "{token}");
        }
        assert_eq!(uart_role_direction("p01_txd"), Some(true));
        assert_eq!(uart_role_direction("gpio1_u0txd"), Some(true));
        assert_eq!(uart_role_direction("pa10_usart1_rx"), Some(false));
        assert_eq!(uart_role_direction("pd0_rxd_txd"), None);
        assert_eq!(uart_role_direction("pb5_sck"), None);
        // Ethernet and RMII lines are another interface.
        assert_eq!(uart_role_direction("pb11_eth_tx_en"), None);
        assert_eq!(uart_role_direction("pb12_rmii_txd0"), None);
        assert_eq!(uart_role_direction("gpio5_rx_dv"), None);
    }

    #[test]
    fn an_analog_output_is_named_by_whole_word_and_not_by_a_bare_out() {
        for yes in ["AOUT", "ANALOG_OUT", "VTEMP", "Temp_Out", "VIOUT"] {
            assert!(names_an_analog_output(yes), "{yes}");
        }
        for no in ["OUT", "VOUT", "OUT1", "TEMPERATURE", "NOUT", "OUTA"] {
            assert!(!names_an_analog_output(no), "{no}");
        }
    }

    #[test]
    fn a_reference_and_a_supply_do_not_contradict_but_a_supply_and_ground_do() {
        let set = |n: &str| functions_named(n);
        assert!(!contradict(&set("VREF1"), &set("VCC")));
        assert!(!contradict(&set("VDD"), &set("AREF")));
        assert!(contradict(&set("VCC"), &set("GND")));
        assert!(contradict(&set("XTAL1"), &set("XTAL2")));
        // A name that states nothing contradicts nothing.
        assert!(!contradict(&set("PA4"), &set("GND")));
    }

    #[test]
    fn peripheral_names_are_read_by_whole_word() {
        use PeripheralFunction as P;
        let named = |n: &str| {
            peripheral_functions_named(n)
                .into_iter()
                .collect::<Vec<_>>()
        };
        assert_eq!(named("SPI1_MOSI"), [P::SpiMosi]);
        assert_eq!(named("~{SCK}"), [P::SpiSck]);
        assert_eq!(named("I2C_SDA"), [P::I2cSda]);
        assert_eq!(named("/LED_PWM"), [P::Pwm]);
        assert!(named("CLK").is_empty() && named("SDATA").is_empty() && named("TX").is_empty());
        assert!(names_a_uart("UART0_RX") && names_a_uart("TXD") && !names_a_uart("RXEN"));
        let mut keys: Vec<_> = P::ALL.iter().map(|f| f.key()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), P::ALL.len());
        assert!(P::ALL.iter().all(|f| !f.label().is_empty()));
    }

    fn named(name: &str) -> Vec<PinFunction> {
        functions_named(name).into_iter().collect()
    }

    #[test]
    fn a_name_states_the_function_it_spells_in_any_notation() {
        for (name, want) in [
            ("XTAL1", PinFunction::Xtal1),
            ("PB6/XTAL1", PinFunction::Xtal1),
            ("pb6_xtal1", PinFunction::Xtal1),
            ("PB6(XTAL1/TOSC1)", PinFunction::Xtal1),
            ("OSC_IN", PinFunction::Xtal1),
            ("OSC1", PinFunction::Xtal1),
            ("XTAL2", PinFunction::Xtal2),
            ("pb7_xtal2", PinFunction::Xtal2),
            ("OSC_OUT", PinFunction::Xtal2),
            ("XO", PinFunction::Xtal2),
            ("~{RESET}", PinFunction::Reset),
            ("NRST", PinFunction::Reset),
            ("pc6_reset", PinFunction::Reset),
            ("RESET#", PinFunction::Reset),
            ("VCC", PinFunction::PositiveSupply),
            ("VDD1", PinFunction::PositiveSupply),
            ("AVCC", PinFunction::PositiveSupply),
            ("GND", PinFunction::Ground),
            ("gnd2", PinFunction::Ground),
            ("VSSA", PinFunction::Ground),
            ("AREF", PinFunction::AnalogReference),
            ("pc4_adc4_sda", PinFunction::I2cData),
            ("PC5/SCL", PinFunction::I2cClock),
            ("SWDIO", PinFunction::SwdData),
            ("SWCLK", PinFunction::SwdClock),
        ] {
            assert!(
                functions_named(name).contains(&want),
                "{name}: {:?}",
                named(name)
            );
        }
    }

    #[test]
    fn a_name_that_states_no_listed_function_reads_as_none() {
        for name in [
            "", "PA4", "GPIO7", "EN", "1", "EP", "pd0_rxd", "MOSI", "TX", "OSC32_IN", "XTAL",
            "VIN", "VOUT", "SDATA", "SCLK",
        ] {
            assert!(
                functions_named(name).is_empty(),
                "{name}: {:?}",
                named(name)
            );
        }
    }

    #[test]
    fn the_crystal_index_is_part_of_the_name_but_a_supply_index_is_not() {
        assert_eq!(named("XTAL1"), [PinFunction::Xtal1]);
        assert_eq!(named("XTAL2"), [PinFunction::Xtal2]);
        assert_eq!(named("GND3"), [PinFunction::Ground]);
        assert!(functions_named("XTAL3").is_empty());
    }

    #[test]
    fn every_function_has_a_spelling_and_a_label() {
        for f in PinFunction::ALL {
            assert!(SPELLINGS.iter().any(|s| s.function == f), "{f:?}");
            assert!(!f.label().is_empty());
        }
        let mut parts: Vec<&str> = SPELLINGS.iter().map(|s| s.part).collect();
        let n = parts.len();
        parts.sort_unstable();
        parts.dedup();
        assert_eq!(parts.len(), n, "a spelling is listed twice");
        assert!(PinFunction::Ground.is_power() && !PinFunction::Reset.is_power());
    }
}
