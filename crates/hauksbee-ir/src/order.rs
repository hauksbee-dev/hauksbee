//! Canonical ordering and precision for everything that reaches output or a
//! hash.
//!
//! A board describes the same circuit whatever order its file lists the
//! components, nets and footprints in, so the report must not depend on that
//! order either: not in a pair's direction ("GND to AREF"), not in the last
//! digits of a computed gap, not in the `group` key a waiver is filed under,
//! and not in the order of a list. This module is the one place the rules
//! live; producers call it at the point a list or a number leaves them.

use std::cmp::Ordering;

/// Decimal places a millimetre length keeps once it is computed (1 nm).
///
/// Board files state coordinates to a micron at best, and geometry done in
/// floating point differs in the 12th digit with the order the operands met
/// in. Rounding to a nanometre removes that noise without discarding any
/// precision the input had.
pub const MM_DECIMALS: i32 = 6;

/// `value` rounded to [`MM_DECIMALS`] places, with no negative zero.
#[must_use]
pub fn quantize_mm(value: f64) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let scale = 10f64.powi(MM_DECIMALS);
    let rounded = (value * scale).round() / scale;
    if rounded == 0.0 {
        0.0
    } else {
        rounded
    }
}

/// Natural sort key for a reference designator: alpha prefix (case-folded),
/// then the numeric part as a NUMBER (so R2 sorts before R10), then any
/// remaining suffix.
///
/// Shared by every user-facing table that lists parts.
#[must_use]
pub fn natural_ref_key(reference: &str) -> (String, u64, String) {
    let prefix: String = reference
        .chars()
        .take_while(|c| !c.is_ascii_digit())
        .collect();
    let rest = &reference[prefix.len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let suffix = rest[digits.len()..].to_string();
    (
        prefix.to_ascii_uppercase(),
        digits.parse().unwrap_or(0),
        suffix,
    )
}

/// Total order on designators: natural first, the exact text as the tie-break,
/// so two spellings that fold together still sort the same way every run.
#[must_use]
pub fn cmp_refs(a: &str, b: &str) -> Ordering {
    natural_ref_key(a)
        .cmp(&natural_ref_key(b))
        .then_with(|| a.cmp(b))
}

/// `refs` in canonical order ([`cmp_refs`]).
#[must_use]
pub fn sorted_refs(refs: &[String]) -> Vec<String> {
    let mut out = refs.to_vec();
    out.sort_by(|a, b| cmp_refs(a, b));
    out
}

/// Whether the pair (`name_a`, `id_a`) comes before (`name_b`, `id_b`) in the
/// canonical direction for a pair of nets: by name, the id only breaking a tie
/// between two nets that share one. A pair of nets is the same pair however the
/// file numbered them, so the finding that names it must read the same way.
#[must_use]
pub fn net_pair_in_order(name_a: &str, id_a: i64, name_b: &str, id_b: i64) -> bool {
    (name_a, id_a) <= (name_b, id_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantizing_removes_the_noise_a_different_operand_order_leaves() {
        assert_eq!(
            quantize_mm(0.194_500_000_000_000_5),
            quantize_mm(0.194_500_000_000_000_45)
        );
        assert!((quantize_mm(0.195_000_4) - 0.195).abs() < 1e-12);
    }

    #[test]
    fn quantizing_never_yields_negative_zero() {
        assert!(quantize_mm(-1e-9).is_sign_positive());
        assert!(quantize_mm(-0.0).is_sign_positive());
        assert!(quantize_mm(f64::NAN).is_nan());
    }

    #[test]
    fn designators_sort_naturally_and_totally() {
        let mut v: Vec<String> = ["R10", "R2", "C1", "U$1", "r2", "R2"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        v.sort_by(|a, b| cmp_refs(a, b));
        assert_eq!(v, ["C1", "R2", "R2", "r2", "R10", "U$1"]);
    }

    #[test]
    fn a_net_pair_reads_the_same_whichever_way_it_was_numbered() {
        assert!(net_pair_in_order("AREF", 9, "GND", 1));
        assert!(!net_pair_in_order("GND", 1, "AREF", 9));
        assert!(net_pair_in_order("A", 1, "A", 2));
    }
}
