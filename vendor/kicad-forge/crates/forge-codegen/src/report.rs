//! Text rendering of an [`Anomaly`].

use crate::cluster::Anomaly;

pub fn render_anomaly(a: &Anomaly) -> String {
    match a {
        Anomaly::ValueMismatch {
            slot,
            reference,
            expected,
            found,
        } => format!(
            "slot {} ({}): value expected '{}' but found '{}'",
            slot, reference, expected, found
        ),
        Anomaly::LibIdMismatch {
            slot,
            reference,
            expected,
            found,
        } => format!(
            "slot {} ({}): lib_id expected '{}' but found '{}'",
            slot, reference, expected, found
        ),
        Anomaly::MissingComponent {
            slot,
            expected_lib_id,
            expected_value,
        } => format!(
            "slot {}: MISSING {} = {}",
            slot, expected_lib_id, expected_value
        ),
        Anomaly::ExtraComponent {
            reference,
            lib_id,
            value,
        } => format!("EXTRA {} = {} ({})", reference, value, lib_id),
    }
}
