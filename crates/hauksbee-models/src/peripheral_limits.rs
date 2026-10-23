//! `[models.peripherals]`: the datasheet limits of an MCU's UART and ADC, as
//! data the co-simulation fidelity layer enforces.
//!
//! The emulators move UART bytes and ADC samples faithfully whatever the
//! firmware configured. A firmware whose baud divisor is 3.5 % off, or whose ADC
//! clock is ten times the datasheet window, still reads clean data in the
//! emulator and garbage on the bench. This table states what the silicon allows,
//! so a run can say where the firmware asked for more.
//!
//! Every block carries the URL (and hash) of the datasheet it was read from and
//! a `basis` naming the table, section and page of every value in it. A value the
//! datasheet does not print is left out, never estimated: every number here is
//! optional where a datasheet may not state it, and a consumer abstains on an
//! absent one. The names and the arithmetic come from
//! [`hauksbee_ir::peripheral_limits`], the one place they are spelled.
//!
//! The bus limits `max_sclk_hz` and `i2c_max_hz` are not tables: they are params
//! written as printed rows on the peripheral's own card (see
//! [`hauksbee_ir::peripheral_limits::BUS_LIMIT_PARAMS`]).

use serde::{Deserialize, Serialize};

use crate::check::positive_finite;
use hauksbee_ir::peripheral_limits::{
    adc_conversion_rate_hz, adc_max_source_ohms, uart_baud, uart_divisor, AdcConversionMode,
};

/// `[models.peripherals]`: one optional table per peripheral kind.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PeripheralLimits {
    /// `[models.peripherals.uart]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uart: Option<UartLimits>,
    /// `[models.peripherals.adc]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adc: Option<AdcLimits>,
}

impl PeripheralLimits {
    /// No peripheral table is declared.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.uart.is_none() && self.adc.is_none()
    }

    /// Every problem with the declared tables (empty when none is declared).
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(uart) = &self.uart {
            out.extend(uart.problems());
        }
        if let Some(adc) = &self.adc {
            out.extend(adc.problems());
        }
        out
    }
}

/// A document a block's values were read from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRef {
    /// The https URL it was downloaded from.
    pub url: String,
    /// SHA-256 of the file as read.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
}

// ── UART ──────────────────────────────────────────────────────────────────────

/// `[models.peripherals.uart]`: where the UART's clock comes from, how its baud
/// divisor turns into a rate, the highest rate, and how much rate error a
/// receiver tolerates.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UartLimits {
    /// URL of the datasheet (or reference manual) the block is read from.
    pub source: String,
    /// SHA-256 of that document as read, so the citation is reproducible.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_sha256: String,
    /// Further documents the block draws values from (a reference manual
    /// beside the datasheet).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_from: Vec<SourceRef>,
    /// The document revision and the table, section and page of the block as a
    /// whole.
    pub basis: String,
    /// `[[models.peripherals.uart.clock_sources]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clock_sources: Vec<UartClockSource>,
    /// `[models.peripherals.uart.baud]`.
    pub baud: UartBaudGenerator,
    /// `[[models.peripherals.uart.max_baud]]`: a highest rate the datasheet
    /// prints. Absent where it only follows from the divisor range.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub max_baud: Vec<UartMaxBaud>,
    /// `[[models.peripherals.uart.receiver_tolerance]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub receiver_tolerance: Vec<UartReceiverTolerance>,
}

/// The clock a UART instance counts its baud divisor from.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UartClockSource {
    /// The clock's datasheet name (`clk_peri`, `PCLK2`, the system clock).
    pub name: String,
    /// The UART instances on it (`USART1`); empty means every instance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instances: Vec<String>,
    /// The highest frequency the datasheet allows that clock, Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hz: Option<f64>,
    pub basis: String,
}

/// How a divisor register becomes a baud rate:
/// `baud = f_clk / (oversampling * (register / 2^fraction_bits + divisor_offset))`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UartBaudGenerator {
    /// The datasheet's equation, as printed or paraphrased in one line.
    pub formula: String,
    /// Added to the scaled register (`UBRR + 1` is 1; `USARTDIV` is 0).
    pub divisor_offset: f64,
    /// Fraction bits at the bottom of the register (`USART_BRR` has 4).
    pub fraction_bits: u8,
    /// Width of the divisor register (read as one number, fraction included).
    pub register_bits: u8,
    /// The samples per bit the receiver can be set to (16 normal, 8 double
    /// speed): the `oversampling` factor of the formula.
    pub oversampling: Vec<u32>,
    pub basis: String,
}

/// A highest baud rate the datasheet states.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UartMaxBaud {
    /// The instances it applies to; empty means every instance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instances: Vec<String>,
    /// At this oversampling; absent when the datasheet does not tie it to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oversampling: Option<u32>,
    pub baud_hz: f64,
    /// What the figure is printed against (a clock frequency, a mode).
    pub condition: String,
    pub basis: String,
}

/// How far a receiver's baud may be off before frames are lost.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UartReceiverTolerance {
    /// Start, data, parity and stop bits of the frame the figure is printed for
    /// (one stop bit).
    pub frame_bits: u8,
    /// Samples per bit the figure is printed for.
    pub oversampling: u32,
    /// Whether the baud divisor has a non-zero fraction, when the datasheet
    /// prints different figures for each.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fractional_divisor: Option<bool>,
    /// Whether a set noise flag counts as an error, when the datasheet prints
    /// different figures for each.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise_flag_is_error: Option<bool>,
    /// The lowest (most negative) total rate error the receiver still accepts,
    /// in percent, and the highest.
    pub total_error_min_pct: f64,
    pub total_error_max_pct: f64,
    /// The datasheet's recommended limit on the receiver's own error, in
    /// percent either side, when it prints one apart from the total.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_receiver_error_pct: Option<f64>,
    pub basis: String,
}

impl UartLimits {
    /// The baud rate the generator makes from `f_clk_hz` at `oversampling` and
    /// divisor `register`. `None` when the card does not allow that oversampling
    /// or the register does not fit its width.
    #[must_use]
    pub fn baud_hz(&self, f_clk_hz: f64, oversampling: u32, register: u32) -> Option<f64> {
        let g = &self.baud;
        let fits = g.register_bits >= 32 || u64::from(register) < 1u64 << g.register_bits;
        if !g.oversampling.contains(&oversampling) || !fits {
            return None;
        }
        let divisor = uart_divisor(register, g.fraction_bits, g.divisor_offset);
        uart_baud(f_clk_hz, oversampling, divisor)
    }

    /// The tolerance row for a frame, preferring the row that names the
    /// fractional-divisor and noise-flag cases `fractional` and
    /// `noise_flag_is_error` ask for; a row that leaves a case unnamed matches
    /// either.
    #[must_use]
    pub fn tolerance(
        &self,
        frame_bits: u8,
        oversampling: u32,
        fractional: Option<bool>,
        noise_flag_is_error: Option<bool>,
    ) -> Option<&UartReceiverTolerance> {
        let agrees = |row: Option<bool>, asked: Option<bool>| {
            row.is_none() || asked.is_none() || row == asked
        };
        self.receiver_tolerance.iter().find(|t| {
            t.frame_bits == frame_bits
                && t.oversampling == oversampling
                && agrees(t.fractional_divisor, fractional)
                && agrees(t.noise_flag_is_error, noise_flag_is_error)
        })
    }

    /// Every problem with the block.
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        let at = "[models.peripherals.uart]";
        source_problems(
            at,
            &self.source,
            &self.source_sha256,
            &self.also_from,
            &self.basis,
            &mut out,
        );
        for (i, c) in self.clock_sources.iter().enumerate() {
            let at = format!("{at} clock_sources[{i}]");
            cite(&at, &c.basis, &mut out);
            if c.name.trim().is_empty() {
                out.push(format!("{at} needs the clock's name"));
            }
            optional_positive(&at, "max_hz", c.max_hz, &mut out);
        }
        let g = &self.baud;
        let at_baud = format!("{at}.baud");
        cite(&at_baud, &g.basis, &mut out);
        if g.formula.trim().is_empty() {
            out.push(format!("{at_baud} needs the datasheet's formula"));
        }
        if !g.divisor_offset.is_finite() || g.divisor_offset < 0.0 {
            out.push(format!(
                "{at_baud} divisor_offset must be finite and not negative"
            ));
        }
        if g.register_bits == 0 || g.register_bits > 32 {
            out.push(format!("{at_baud} register_bits must be 1 to 32"));
        }
        if g.fraction_bits >= g.register_bits {
            out.push(format!(
                "{at_baud} fraction_bits must be fewer than register_bits"
            ));
        }
        if g.oversampling.is_empty() || g.oversampling.contains(&0) {
            out.push(format!(
                "{at_baud} oversampling must list the non-zero samples-per-bit options"
            ));
        }
        for (i, m) in self.max_baud.iter().enumerate() {
            let at = format!("{at} max_baud[{i}]");
            cite(&at, &m.basis, &mut out);
            if !positive_finite(m.baud_hz) {
                out.push(format!("{at} baud_hz must be positive"));
            }
            if m.condition.trim().is_empty() {
                out.push(format!(
                    "{at} needs the condition the figure is printed against"
                ));
            }
        }
        for (i, t) in self.receiver_tolerance.iter().enumerate() {
            let at = format!("{at} receiver_tolerance[{i}]");
            cite(&at, &t.basis, &mut out);
            if t.frame_bits == 0 || t.oversampling == 0 {
                out.push(format!("{at} needs frame_bits and oversampling"));
            }
            let ordered = t.total_error_min_pct.is_finite()
                && t.total_error_max_pct.is_finite()
                && t.total_error_min_pct <= 0.0
                && t.total_error_max_pct >= 0.0;
            if !ordered {
                out.push(format!(
                    "{at} total_error_min_pct must be at most 0 and total_error_max_pct at least 0"
                ));
            }
            optional_positive(
                &at,
                "recommended_receiver_error_pct",
                t.recommended_receiver_error_pct,
                &mut out,
            );
        }
        out
    }
}

// ── ADC ───────────────────────────────────────────────────────────────────────

/// `[models.peripherals.adc]`: the ADC's resolution, clock window, conversion
/// timing, sampling front end and accuracy.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdcLimits {
    /// URL of the datasheet (or reference manual) the block is read from.
    pub source: String,
    /// SHA-256 of that document as read.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_sha256: String,
    /// Further documents the block draws values from (a reference manual
    /// beside the datasheet).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_from: Vec<SourceRef>,
    /// The document revision and the table, section and page of the block as a
    /// whole.
    pub basis: String,
    /// The resolutions the converter can run at, bits.
    pub resolution_bits: Vec<u8>,
    /// `[models.peripherals.adc.clock]`.
    pub clock: AdcClock,
    /// `[[models.peripherals.adc.conversions]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conversions: Vec<AdcConversion>,
    /// `[[models.peripherals.adc.max_sample_rate]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub max_sample_rate: Vec<AdcSampleRate>,
    /// `[models.peripherals.adc.input]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<AdcInput>,
    /// `[[models.peripherals.adc.accuracy]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accuracy: Vec<AdcAccuracy>,
}

/// The clock the conversion counts its cycles in.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdcClock {
    /// What it is derived from (`system clock through ADPS`, `PCLK2 through ADCPRE`).
    pub source: String,
    /// The divisors from that clock the prescaler offers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prescalers: Vec<u32>,
    /// The datasheet's permitted ADC clock range, Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_hz: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hz: Option<f64>,
    /// The narrower range that gives full resolution, Hz, where the datasheet
    /// separates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_resolution_min_hz: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_resolution_max_hz: Option<f64>,
    /// A clock the datasheet requires to be exactly this, Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_hz: Option<f64>,
    pub basis: String,
}

/// Conversion timing for one start mode at one resolution. Exactly one of
/// `total_cycles` (a fixed time) and `cycles_after_sample` (the datasheet adds
/// it to a sample time) is set.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdcConversion {
    pub mode: Option<AdcConversionMode>,
    pub resolution_bits: u8,
    /// The sample times the converter offers, ADC clock cycles; empty when the
    /// datasheet does not state the sample time on its own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sample_cycles: Vec<f64>,
    /// The conversion's whole duration, ADC clock cycles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cycles: Option<f64>,
    /// The cycles after the sample phase (successive approximation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycles_after_sample: Option<f64>,
    pub basis: String,
}

impl AdcConversion {
    /// The conversion's duration in ADC clock cycles for a given sample time.
    #[must_use]
    pub fn cycles(&self, sample_cycles: f64) -> Option<f64> {
        self.total_cycles
            .or_else(|| self.cycles_after_sample.map(|after| sample_cycles + after))
    }
}

/// The highest sample rate the datasheet states for a resolution.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdcSampleRate {
    /// Absent where the datasheet states the rate for reduced resolution
    /// without naming a bit count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_bits: Option<u8>,
    pub max_hz: f64,
    pub condition: String,
    pub basis: String,
}

/// The sampling front end: the capacitor the input charges, and the resistance
/// in front of it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdcInput {
    /// The sample-and-hold capacitor, F.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_cap_f: Option<f64>,
    /// The resistance between the pin and that capacitor, ohms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_series_ohms_min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_series_ohms_max: Option<f64>,
    /// The highest source resistance the datasheet allows or recommends, ohms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_source_ohms: Option<f64>,
    /// `[[models.peripherals.adc.input.max_source_by_sample_time]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub max_source_by_sample_time: Vec<AdcSourceLimit>,
    pub basis: String,
}

/// The highest source resistance at one sample time, where the datasheet
/// tabulates it. `max_ohms` is absent where the table prints "NA".
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdcSourceLimit {
    pub sample_cycles: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ohms: Option<f64>,
    /// The ADC clock the row is printed at, Hz.
    pub adc_clock_hz: f64,
    pub basis: String,
}

/// The conversion error the datasheet states, either as an absolute error in
/// LSB or as an effective number of bits.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdcAccuracy {
    pub resolution_bits: u8,
    /// The ADC clock the figure is printed at, Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adc_clock_hz: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abs_error_lsb: Option<f64>,
    /// The column the error is printed in (`typ` or `max`), so a typical figure
    /// is never read as a guaranteed limit. Required with `abs_error_lsb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enob_bits: Option<f64>,
    pub condition: String,
    pub basis: String,
}

impl AdcLimits {
    /// The sample rate limit for `resolution_bits`, Hz, from the stated rows.
    #[must_use]
    pub fn max_rate_hz(&self, resolution_bits: u8) -> Option<f64> {
        self.max_sample_rate
            .iter()
            .find(|r| r.resolution_bits == Some(resolution_bits))
            .map(|r| r.max_hz)
    }

    /// The back-to-back conversion rate at an ADC clock and sample time, Hz,
    /// for a start mode and resolution this card states.
    #[must_use]
    pub fn conversion_rate_hz(
        &self,
        mode: AdcConversionMode,
        resolution_bits: u8,
        f_adc_hz: f64,
        sample_cycles: f64,
    ) -> Option<f64> {
        let row = self
            .conversions
            .iter()
            .find(|c| c.mode == Some(mode) && c.resolution_bits == resolution_bits)?;
        adc_conversion_rate_hz(f_adc_hz, row.cycles(sample_cycles)?)
    }

    /// The highest source resistance that settles to a quarter LSB in
    /// `sample_cycles` of an ADC clock at `f_adc_hz`, from the card's sampling
    /// capacitor and switch resistance (the datasheet's RC form). `None` when the
    /// card does not state the capacitor.
    #[must_use]
    pub fn settled_source_ohms(&self, sample_cycles: f64, f_adc_hz: f64) -> Option<f64> {
        let input = self.input.as_ref()?;
        let bits = *self.resolution_bits.iter().max()?;
        adc_max_source_ohms(
            sample_cycles / f_adc_hz,
            input.sample_cap_f?,
            input.input_series_ohms_max.unwrap_or(0.0),
            bits,
        )
    }

    /// Every problem with the block.
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        let at = "[models.peripherals.adc]";
        source_problems(
            at,
            &self.source,
            &self.source_sha256,
            &self.also_from,
            &self.basis,
            &mut out,
        );
        if self.resolution_bits.is_empty() || self.resolution_bits.contains(&0) {
            out.push(format!(
                "{at} resolution_bits must list the resolutions in bits"
            ));
        }
        self.clock_problems(at, &mut out);
        self.timing_problems(at, &mut out);
        self.input_problems(at, &mut out);
        self.accuracy_problems(at, &mut out);
        out
    }

    /// Problems with the ADC clock window.
    fn clock_problems(&self, at: &str, out: &mut Vec<String>) {
        let c = &self.clock;
        let at_clock = format!("{at}.clock");
        cite(&at_clock, &c.basis, out);
        if c.source.trim().is_empty() {
            out.push(format!(
                "{at_clock} source must say what the ADC clock is derived from"
            ));
        }
        if c.prescalers.contains(&0) {
            out.push(format!("{at_clock} prescalers must be non-zero divisors"));
        }
        for (name, v) in [
            ("min_hz", c.min_hz),
            ("max_hz", c.max_hz),
            ("full_resolution_min_hz", c.full_resolution_min_hz),
            ("full_resolution_max_hz", c.full_resolution_max_hz),
            ("required_hz", c.required_hz),
        ] {
            optional_positive(&at_clock, name, v, out);
        }
        for (lo, hi, what) in [
            (c.min_hz, c.max_hz, "min_hz and max_hz"),
            (
                c.full_resolution_min_hz,
                c.full_resolution_max_hz,
                "full_resolution_min_hz and full_resolution_max_hz",
            ),
        ] {
            if lo.zip(hi).is_some_and(|(lo, hi)| lo > hi) {
                out.push(format!("{at_clock} {what} are out of order"));
            }
        }
    }

    /// Problems with the conversion and sample-rate rows.
    fn timing_problems(&self, at: &str, out: &mut Vec<String>) {
        for (i, v) in self.conversions.iter().enumerate() {
            let at = format!("{at} conversions[{i}]");
            cite(&at, &v.basis, out);
            if v.mode.is_none() {
                out.push(format!("{at} needs a mode"));
            }
            if !self.resolution_bits.contains(&v.resolution_bits) {
                out.push(format!(
                    "{at} resolution_bits is not one of the ADC's resolutions"
                ));
            }
            match (v.total_cycles, v.cycles_after_sample) {
                (Some(t), None) if positive_finite(t) => {}
                (None, Some(a)) if positive_finite(a) && !v.sample_cycles.is_empty() => {}
                _ => out.push(format!(
                    "{at} needs total_cycles, or cycles_after_sample beside the sample_cycles it is added to (not both)"
                )),
            }
            if v.sample_cycles.iter().any(|s| !positive_finite(*s)) {
                out.push(format!("{at} sample_cycles must be positive"));
            }
        }
        for (i, r) in self.max_sample_rate.iter().enumerate() {
            let at = format!("{at} max_sample_rate[{i}]");
            cite(&at, &r.basis, out);
            if !positive_finite(r.max_hz) {
                out.push(format!("{at} max_hz must be positive"));
            }
            if r.condition.trim().is_empty() {
                out.push(format!(
                    "{at} needs the condition the rate is printed against"
                ));
            }
            if r.resolution_bits
                .is_some_and(|b| !self.resolution_bits.contains(&b))
            {
                out.push(format!(
                    "{at} resolution_bits is not one of the ADC's resolutions"
                ));
            }
        }
    }

    /// Problems with the input-circuit table.
    fn input_problems(&self, at: &str, out: &mut Vec<String>) {
        if let Some(input) = &self.input {
            let at = format!("{at}.input");
            cite(&at, &input.basis, out);
            for (name, v) in [
                ("sample_cap_f", input.sample_cap_f),
                ("input_series_ohms_min", input.input_series_ohms_min),
                ("input_series_ohms_max", input.input_series_ohms_max),
                ("max_source_ohms", input.max_source_ohms),
            ] {
                optional_positive(&at, name, v, out);
            }
            if input
                .input_series_ohms_min
                .zip(input.input_series_ohms_max)
                .is_some_and(|(lo, hi)| lo > hi)
            {
                out.push(format!(
                    "{at} input_series_ohms_min exceeds input_series_ohms_max"
                ));
            }
            for (i, s) in input.max_source_by_sample_time.iter().enumerate() {
                let at = format!("{at} max_source_by_sample_time[{i}]");
                cite(&at, &s.basis, out);
                if !positive_finite(s.sample_cycles) || !positive_finite(s.adc_clock_hz) {
                    out.push(format!("{at} needs sample_cycles and adc_clock_hz"));
                }
                optional_positive(&at, "max_ohms", s.max_ohms, out);
            }
        }
    }

    /// Problems with the accuracy rows.
    fn accuracy_problems(&self, at: &str, out: &mut Vec<String>) {
        for (i, a) in self.accuracy.iter().enumerate() {
            let at = format!("{at} accuracy[{i}]");
            cite(&at, &a.basis, out);
            if a.abs_error_lsb.is_none() && a.enob_bits.is_none() {
                out.push(format!("{at} needs abs_error_lsb or enob_bits"));
            }
            if a.abs_error_lsb.is_some() && !matches!(a.column.as_deref(), Some("typ" | "max")) {
                out.push(format!(
                    "{at} abs_error_lsb needs column = \"typ\" or \"max\""
                ));
            }
            optional_positive(&at, "abs_error_lsb", a.abs_error_lsb, out);
            optional_positive(&at, "enob_bits", a.enob_bits, out);
            optional_positive(&at, "adc_clock_hz", a.adc_clock_hz, out);
            if a.condition.trim().is_empty() {
                out.push(format!(
                    "{at} needs the condition the figure is printed against"
                ));
            }
        }
    }
}

// ── shared checks ─────────────────────────────────────────────────────────────

/// A `basis` that names where in a document a value was read: it says a table,
/// section, figure, equation or page, and carries a number.
#[must_use]
pub fn cites_location(basis: &str) -> bool {
    let lower = basis.to_ascii_lowercase();
    ["table", "section", "figure", "equation", "p.", "page"]
        .iter()
        .any(|w| lower.contains(w))
        && basis.chars().any(|c| c.is_ascii_digit())
}

fn cite(at: &str, basis: &str, out: &mut Vec<String>) {
    if !cites_location(basis) {
        out.push(format!(
            "{at} needs a basis naming the datasheet table, section and page the values were read from"
        ));
    }
}

fn source_problems(
    at: &str,
    source: &str,
    sha256: &str,
    also_from: &[SourceRef],
    basis: &str,
    out: &mut Vec<String>,
) {
    let hex = |h: &str| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit());
    if !source.starts_with("https://") {
        out.push(format!(
            "{at} source must be the https URL of the datasheet it was read from"
        ));
    }
    if !sha256.is_empty() && !hex(sha256) {
        out.push(format!("{at} source_sha256 must be 64 hex digits"));
    }
    for d in also_from {
        if !d.url.starts_with("https://") || (!d.sha256.is_empty() && !hex(&d.sha256)) {
            out.push(format!(
                "{at} also_from needs an https url and a 64-hex sha256"
            ));
        }
    }
    cite(at, basis, out);
}

fn optional_positive(at: &str, name: &str, v: Option<f64>, out: &mut Vec<String>) {
    if v.is_some_and(|v| !positive_finite(v)) {
        out.push(format!("{at} {name} must be finite and positive"));
    }
}

#[cfg(test)]
#[path = "peripheral_limits_tests.rs"]
mod tests;
