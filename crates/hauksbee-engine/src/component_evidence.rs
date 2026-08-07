//! Pin-level evidence for claims about one physical component.
//!
//! Extractors deliberately preserve ambiguous component records so
//! connectivity and board-level DRC remain available. Whether a record may be
//! trusted at all is the three-state assembled-component contract
//! ([`hauksbee_extract::assembly::AssemblyState`]); this module answers the
//! next question down: given a part that IS present, what nets do its logical
//! terminals actually sit on? Repeated physical pads must coalesce into one
//! logical terminal without accepting contradictory connectivity, and a
//! model-declared pin role must map to exactly one net or be refused.

use std::collections::BTreeMap;

use hauksbee_extract::assembly::FittedComponent;
use hauksbee_extract::Component;
use hauksbee_models::ModelEntry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PinEvidenceError {
    ConflictingNets {
        pad: String,
        first: i64,
        second: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RoleEvidenceError {
    Pins(PinEvidenceError),
    MissingRole(String),
    UnconnectedRole(String),
    ConflictingRoleNets {
        role: String,
        first: i64,
        second: i64,
    },
}

pub(crate) fn logical_pin_nets(
    component: &Component,
) -> Result<BTreeMap<String, Option<i64>>, PinEvidenceError> {
    let mut logical = BTreeMap::new();
    for pin in &component.pins {
        let pad = pin.number.trim();
        if pad.is_empty() {
            continue;
        }
        let observed = pin.net.filter(|net| *net != 0);
        match logical.get_mut(pad) {
            None => {
                logical.insert(pad.to_string(), observed);
            }
            Some(known @ None) if observed.is_some() => *known = observed,
            Some(Some(first)) => {
                if let Some(second) = observed {
                    if *first != second {
                        return Err(PinEvidenceError::ConflictingNets {
                            pad: pad.to_string(),
                            first: *first,
                            second,
                        });
                    }
                }
            }
            Some(None) => {}
        }
    }
    Ok(logical)
}

/// The net a model-declared pin role sits on, for a part that has already
/// answered the three-state assembled-component question: taking the
/// [`FittedComponent`] witness (the only source of a bindable model) means an
/// identity-refused or DNP-absent record cannot reach this at all.
pub(crate) fn role_net(
    part: FittedComponent<'_>,
    model: &ModelEntry,
    role: &str,
) -> Result<i64, RoleEvidenceError> {
    let logical = logical_pin_nets(&part).map_err(RoleEvidenceError::Pins)?;
    let pads: Vec<_> = model
        .pins
        .iter()
        .filter(|(_, mapped_role)| mapped_role.eq_ignore_ascii_case(role))
        .map(|(pad, _)| pad.as_str())
        .collect();
    if pads.is_empty() {
        return Err(RoleEvidenceError::MissingRole(role.to_string()));
    }

    let mut resolved = None;
    for pad in pads {
        let net = logical
            .get(pad)
            .copied()
            .flatten()
            .ok_or_else(|| RoleEvidenceError::UnconnectedRole(role.to_string()))?;
        match resolved {
            None => resolved = Some(net),
            Some(first) if first != net => {
                return Err(RoleEvidenceError::ConflictingRoleNets {
                    role: role.to_string(),
                    first,
                    second: net,
                });
            }
            Some(_) => {}
        }
    }
    resolved.ok_or_else(|| RoleEvidenceError::UnconnectedRole(role.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::Pin;

    fn component(pins: &[(&str, Option<i64>)]) -> Component {
        Component {
            reference: "U1".into(),
            value: "TEST".into(),
            lib_id: String::new(),
            footprint: String::new(),
            position: None,
            layer: String::new(),
            properties: Vec::new(),
            dnp: false,
            pins: pins
                .iter()
                .map(|(number, net)| Pin {
                    number: (*number).into(),
                    net: *net,
                    function: String::new(),
                    kind: String::new(),
                    position: None,
                })
                .collect(),
        }
    }

    #[test]
    fn repeated_physical_pads_coalesce_into_one_logical_terminal() {
        let c = component(&[("1", Some(7)), ("1", Some(7)), ("2", Some(3))]);
        let pins = logical_pin_nets(&c).unwrap();
        assert_eq!(pins.len(), 2);
        assert_eq!(pins.get("1"), Some(&Some(7)));
        assert_eq!(pins.get("2"), Some(&Some(3)));
    }

    #[test]
    fn unknown_repeated_pad_is_enriched_by_known_connectivity_in_any_order() {
        for pins in [
            [("1", None), ("1", Some(0)), ("1", Some(9))],
            [("1", Some(9)), ("1", Some(0)), ("1", None)],
        ] {
            let logical = logical_pin_nets(&component(&pins)).unwrap();
            assert_eq!(logical.get("1"), Some(&Some(9)));
        }
    }

    #[test]
    fn conflicting_repeated_pad_connectivity_is_refused() {
        let err = logical_pin_nets(&component(&[("1", Some(7)), ("1", Some(9))])).unwrap_err();
        assert_eq!(
            err,
            PinEvidenceError::ConflictingNets {
                pad: "1".into(),
                first: 7,
                second: 9,
            }
        );
    }
}
