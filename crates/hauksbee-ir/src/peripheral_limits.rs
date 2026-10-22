//! The one vocabulary for datasheet peripheral limits (UART, ADC, bus clocks),
//! and the arithmetic every consumer shares.
//!
//! A model card states what a peripheral can do (`[models.peripherals.uart]`,
//! `[models.peripherals.adc]`, the `max_sclk_hz` and `i2c_max_hz` bus-limit
//! params). The card schema in `hauksbee-models`, the card validator, the
//! co-simulation fidelity layer that enforces the limits and the documents all
//! name those things with the words defined here, and compute the baud and
//! conversion arithmetic with the functions here, so a formula or a key is
//! spelled once.
//!
//! Every function is pure and returns `None` or a non-finite-free value for
//! an input the formula cannot take (a zero divisor, a negative time).

use serde::{Deserialize, Serialize};

/// The `[models.peripherals]` table name on a model card.
pub const PERIPHERALS_TABLE: &str = "peripherals";

/// `[models.peripherals.uart]`.
pub const UART_TABLE: &str = "uart";

/// `[models.peripherals.adc]`.
pub const ADC_TABLE: &str = "adc";

/// The param holding the highest SPI clock a peripheral card's datasheet
/// allows, in hertz. Written as a printed row (`unit = "Hz"`, `max = ...`) so it
/// carries its test conditions and its datasheet basis.
pub const MAX_SCLK_HZ_PARAM: &str = "max_sclk_hz";

/// The param holding the highest I2C SCL clock a peripheral card's datasheet
/// allows (its fastest bus mode), in hertz, written as a printed row like
/// [`MAX_SCLK_HZ_PARAM`].
pub const I2C_MAX_HZ_PARAM: &str = "i2c_max_hz";

/// The bus-limit params, in the order a document lists them.
pub const BUS_LIMIT_PARAMS: [&str; 2] = [MAX_SCLK_HZ_PARAM, I2C_MAX_HZ_PARAM];

/// How a conversion starts, which sets how many ADC cycles it takes and when the
/// input is sampled (`[[models.peripherals.adc.conversions]] mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdcConversionMode {
    /// A conversion started by software, or the next one in free-running mode.
    Normal,
    /// The first conversion after the ADC is switched on, which initialises the
    /// analogue circuitry.
    FirstAfterEnable,
    /// A conversion started by a hardware trigger event.
    AutoTriggered,
}

impl AdcConversionMode {
    /// Every mode, in the order a document lists them.
    pub const ALL: [Self; 3] = [Self::Normal, Self::FirstAfterEnable, Self::AutoTriggered];

    /// The wire word the card spells it with.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::FirstAfterEnable => "first_after_enable",
            Self::AutoTriggered => "auto_triggered",
        }
    }
}

/// The divisor a baud-rate register value stands for.
///
/// It is `register / 2^fraction_bits + offset`. An AVR `UBRR` has no fraction and
/// an offset of 1 (`UBRR + 1`); an STM32 `USART_BRR` has four fraction bits and no offset (`USARTDIV`); an RP2040
/// `UARTIBRD`/`UARTFBRD` pair read as one number has six fraction bits.
#[must_use]
pub fn uart_divisor(register: u32, fraction_bits: u8, offset: f64) -> f64 {
    f64::from(register) / 2f64.powi(i32::from(fraction_bits)) + offset
}

/// The baud rate an asynchronous UART runs at: `f_clk / (oversampling * divisor)`.
/// `None` for a zero oversampling, a non-positive divisor or a non-positive clock.
#[must_use]
pub fn uart_baud(f_clk_hz: f64, oversampling: u32, divisor: f64) -> Option<f64> {
    let samples = f64::from(oversampling);
    (f_clk_hz > 0.0 && samples > 0.0 && divisor > 0.0).then(|| f_clk_hz / (samples * divisor))
}

/// The error of `actual` against `target`, in percent: `(actual - target) / target`.
/// `None` for a non-positive target.
#[must_use]
pub fn baud_error_pct(actual_hz: f64, target_hz: f64) -> Option<f64> {
    (target_hz > 0.0).then(|| (actual_hz - target_hz) / target_hz * 100.0)
}

/// The rate conversions complete at, back to back: `f_adc / conversion_cycles`.
/// `None` for a non-positive clock or cycle count.
#[must_use]
pub fn adc_conversion_rate_hz(f_adc_hz: f64, conversion_cycles: f64) -> Option<f64> {
    (f_adc_hz > 0.0 && conversion_cycles > 0.0).then(|| f_adc_hz / conversion_cycles)
}

/// The fraction of a step an RC input path has settled to after `t_s` seconds:
/// `1 - exp(-t / (r * c))`. `None` for a negative time or a non-positive RC.
#[must_use]
pub fn rc_settled_fraction(r_ohms: f64, c_farads: f64, t_s: f64) -> Option<f64> {
    let tau = r_ohms * c_farads;
    (t_s >= 0.0 && tau > 0.0).then(|| 1.0 - (-t_s / tau).exp())
}

/// The highest source resistance that lets a sample-and-hold capacitor settle.
///
/// Settling is to within a quarter LSB of an `n_bits` converter in `sample_time_s`:
/// `t / (C * ln(2^(n + 2))) - r_switch`. This is the form ST prints as Equation 1
/// in the STM32F103 datasheet (DS5319, section 5.3.18); a value at or below zero
/// means no external source resistance settles in that time, and is `None`.
#[must_use]
pub fn adc_max_source_ohms(
    sample_time_s: f64,
    sample_cap_f: f64,
    switch_ohms: f64,
    n_bits: u8,
) -> Option<f64> {
    let denom = sample_cap_f * f64::from(u32::from(n_bits) + 2) * std::f64::consts::LN_2;
    let r = sample_time_s / denom - switch_ohms;
    (sample_time_s > 0.0 && denom > 0.0 && r > 0.0).then_some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avr_baud_formula_matches_the_published_example() {
        // Microchip DS40002061B Table 20-7: 16 MHz, UBRR = 8, U2X = 0 is 115.2k at -3.5 %.
        let divisor = uart_divisor(8, 0, 1.0);
        let baud = uart_baud(16e6, 16, divisor).unwrap();
        let err = baud_error_pct(baud, 115_200.0).unwrap();
        assert!((err - -3.55).abs() < 0.01, "{err}");
    }

    #[test]
    fn fractional_divisors_scale_by_their_fraction_bits() {
        // STM32 RM0008 Table 192: 72 MHz, USARTDIV 39.0625 (BRR 0x271) is 115.2k.
        let divisor = uart_divisor(0x271, 4, 0.0);
        assert!((divisor - 39.0625).abs() < 1e-12);
        let baud = uart_baud(72e6, 16, divisor).unwrap();
        assert!((baud - 115_200.0).abs() < 1e-6);
    }

    #[test]
    fn degenerate_inputs_are_none() {
        assert_eq!(uart_baud(16e6, 0, 9.0), None);
        assert_eq!(uart_baud(16e6, 16, 0.0), None);
        assert_eq!(baud_error_pct(1.0, 0.0), None);
        assert_eq!(adc_conversion_rate_hz(0.0, 13.0), None);
        assert_eq!(rc_settled_fraction(1e3, 0.0, 1e-6), None);
        assert_eq!(rc_settled_fraction(1e3, 1e-12, -1.0), None);
    }

    #[test]
    fn rc_settling_follows_the_exponential() {
        // One time constant settles to 1 - 1/e.
        let f = rc_settled_fraction(10e3, 14e-12, 10e3 * 14e-12).unwrap();
        assert!((f - (1.0 - (-1f64).exp())).abs() < 1e-12);
    }

    #[test]
    fn source_resistance_matches_the_stm32_datasheet_equation() {
        // DS5319 Table 48 (fADC = 14 MHz, CADC = 8 pF, RADC = 1 kOhm, 12 bits):
        // 1.5 cycles -> 0.4 kOhm, 13.5 -> 11.4 kOhm.
        let r = |cycles: f64| adc_max_source_ohms(cycles / 14e6, 8e-12, 1e3, 12).unwrap();
        assert!((r(1.5) - 400.0).abs() < 50.0, "{}", r(1.5));
        assert!((r(13.5) - 11_400.0).abs() < 100.0, "{}", r(13.5));
        assert_eq!(adc_max_source_ohms(0.0, 8e-12, 1e3, 12), None);
    }

    #[test]
    fn mode_words_are_the_serde_words() {
        for mode in AdcConversionMode::ALL {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, format!("\"{}\"", mode.as_str()));
        }
    }
}
