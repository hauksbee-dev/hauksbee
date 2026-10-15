//! The chip-size ladder, defined once.
//!
//! Three places used to restate it: the stress monitor's resistor ratings (by
//! imperial code, and by the metric-only code), the thermal monitor's size-derived
//! theta_JA, and the BOM-value parser's size codes that leak into a value field.
//! `docs/checks/THERMAL.md` renders its chip tables from this list (`tests/suite/docs_generated.rs`).

/// One chip body: its two size codes, the standard resistor rating it carries and
/// its still-air junction-to-ambient figure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChipSize {
    /// The imperial code (`0402`).
    pub imperial: &'static str,
    /// The metric code (`1005`), the digit run before `Metric` in a `KiCad` name.
    pub metric: &'static str,
    /// The standard resistor rating (W).
    pub watts: f64,
    /// Still-air theta_JA (C/W); `None` where no figure has been derived.
    pub theta_ja_c_per_w: Option<f64>,
}

const fn chip(
    imperial: &'static str,
    metric: &'static str,
    watts: f64,
    theta_ja_c_per_w: Option<f64>,
) -> ChipSize {
    ChipSize {
        imperial,
        metric,
        watts,
        theta_ja_c_per_w,
    }
}

/// Every chip size recognised, smallest body first. The order is load-bearing:
/// `KiCad` pairs the imperial code with its metric twin, and 01005's metric code
/// IS 0402 (`R_01005_0402Metric`), so a substring search must meet 01005 before
/// 0402; and a smaller body is the hotter, so a lookup that stops at the first hit
/// errs on the safe side.
pub const CHIP_SIZES: &[ChipSize] = &[
    chip("01005", "0402", 1.0 / 32.0, Some(1200.0)),
    chip("0201", "0603", 1.0 / 20.0, Some(900.0)),
    chip("0402", "1005", 1.0 / 16.0, Some(600.0)),
    chip("0603", "1608", 1.0 / 10.0, Some(400.0)),
    chip("0805", "2012", 1.0 / 8.0, Some(300.0)),
    chip("1206", "3216", 1.0 / 4.0, Some(220.0)),
    chip("1210", "3225", 1.0 / 2.0, Some(160.0)),
    chip("2010", "5025", 3.0 / 4.0, Some(120.0)),
    chip("1812", "4532", 1.0, None),
    chip("2512", "6332", 1.0, Some(80.0)),
    chip("2220", "5750", 1.0, None),
];

/// The first size, smallest body first, whose imperial code `wanted` accepts.
pub fn find_imperial(wanted: impl Fn(&str) -> bool) -> Option<&'static ChipSize> {
    CHIP_SIZES.iter().find(|c| wanted(c.imperial))
}

/// The size with this metric code.
#[must_use]
pub fn by_metric(code: &str) -> Option<&'static ChipSize> {
    CHIP_SIZES.iter().find(|c| c.metric == code)
}

/// Whether `code` is an imperial chip-size code.
#[must_use]
pub fn is_imperial_code(code: &str) -> bool {
    CHIP_SIZES.iter().any(|c| c.imperial == code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_and_a_chip_precedes_its_metric_twin() {
        for (i, a) in CHIP_SIZES.iter().enumerate() {
            for b in &CHIP_SIZES[i + 1..] {
                assert_ne!(a.imperial, b.imperial);
                assert_ne!(a.metric, b.metric);
            }
        }
        let pos = |code: &str| CHIP_SIZES.iter().position(|c| c.imperial == code);
        assert!(pos("01005") < pos("0402"));
        assert!(pos("0201") < pos("0603"));
    }
}
