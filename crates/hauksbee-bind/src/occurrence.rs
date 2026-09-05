//! Evidence subject naming for components that occur more than once.
//!
//! A reference designator normally names one part, so it can be the evidence
//! subject as-is. When the same designator occurs several times (a multi-unit
//! part, or a board that reuses a refdes), each occurrence gets a stable,
//! content-derived subject so evidence maps never merge two parts into one.
//! Shared by the evidence spine and the co-sim scheduler, which is why it
//! lives at this layer rather than in `hauksbee_engine::evidence`.

use std::collections::BTreeMap;

pub const OCCURRENCE_PREFIX: &str = "@hkb-occurrence:";

pub fn component_occurrence_subject(
    reference: &str,
    total_occurrences: usize,
    ordinal: usize,
) -> String {
    let reference = reference.trim();
    if !reference.is_empty() && total_occurrences <= 1 && !reference.starts_with(OCCURRENCE_PREFIX)
    {
        return reference.to_string();
    }
    let encoded = reference
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    format!("{OCCURRENCE_PREFIX}{encoded}:{ordinal}")
}

pub fn component_occurrence_subjects_for_references<'a>(
    references: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let references: Vec<_> = references.into_iter().collect();
    let mut totals = BTreeMap::<String, usize>::new();
    for reference in &references {
        *totals.entry(reference.trim().to_string()).or_default() += 1;
    }
    let mut seen = BTreeMap::<String, usize>::new();
    references
        .into_iter()
        .map(|reference| {
            let reference = reference.trim();
            let ordinal = seen.entry(reference.to_string()).or_default();
            *ordinal += 1;
            component_occurrence_subject(
                reference,
                totals.get(reference).copied().unwrap_or_default(),
                *ordinal,
            )
        })
        .collect()
}
