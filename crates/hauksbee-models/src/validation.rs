//! Physical-range validation for model parameters.
//!
//! Used by the extraction pipeline to reject LLM-generated model entries whose
//! parameters are missing or out of physical bounds before they are saved to
//! the user database, so a hallucinated part value cannot silently enter the
//! model library. [`validate`] collects every violation at once rather than
//! stopping at the first.

use std::collections::HashSet;

use crate::check::{nonneg_finite, positive_finite, Problems};
use crate::schema::{
    AboveDomainBehavior, Characteristic, ComponentKind, CurrentProgramEquation,
    CurrentProgramSemantics, ModelEntry, OperatingEnvelope, ParamValue, PeripheralSpec, Pinout,
};
use crate::sensor_spec::{Bus, SensorSpec};
use hauksbee_ir::evidence::{ModelSourceTier, ModelUncertainty};
use thiserror::Error;

/// A validation error.
#[derive(Debug, Error)]
#[error("model '{id}': {message}")]
pub struct ValidationError {
    pub id: String,
    pub message: String,
}

/// Does `entry` declare `role` in `[models.pins]`? Exact match, as the
/// register-map and SPI-NOR rules spell their roles verbatim.
fn has_role(entry: &ModelEntry, role: &str) -> bool {
    entry.pins.values().any(|known| known == role)
}

/// [`has_role`], case-insensitively: the rules that read author-written role
/// lists (`must_not_float_roles`, the current-program role sets) accept any
/// casing.
fn has_role_ci(entry: &ModelEntry, role: &str) -> bool {
    entry
        .pins
        .values()
        .any(|known| known.eq_ignore_ascii_case(role))
}

/// The message every role rule shares: `what` '`role`' is not a role in `[models.pins]`.
fn not_a_role(what: impl std::fmt::Display, role: &str) -> String {
    format!("{what} '{role}' is not a role in [models.pins]")
}

/// A role several pads may share as one net inside: supply, ground, NC, exposed pad.
#[must_use]
pub fn repeatable_role(role: &str) -> bool {
    let role = role.to_ascii_lowercase();
    let base = role.trim_end_matches(|c: char| c.is_ascii_digit() || c == '_');
    let supply = ["vcc", "vdd", "vee", "vss"].contains(&base.get(..3).unwrap_or(base));
    let one_net = "vs v_plus v_minus nc dnc ep epad exposed_pad thermal_pad pad tab";
    supply || base.contains("gnd") || one_net.split(' ').any(|r| r == base)
}

/// A per-unit signal role on several pads (a lone electrode letter such as `"B"`
/// aliases a numbered pad): merged units, which the binder wires once.
fn merged_unit_roles(entry: &ModelEntry) -> Vec<String> {
    use ComponentKind::{BjtNpn, BjtPnp, Comparator, Digital, Opamp, ShiftRegister};
    let logic = &entry.logic;
    let units = match entry.kind {
        BjtNpn | BjtPnp => "collector_q1, base_q1, emitter_q1, collector_q2, ...",
        Opamp | Comparator => "out_a, in_minus_a, in_plus_a, out_b, ... for units _a to _d",
        _ => "a1, y1, a2, y2, ..., each listed in [models.logic]",
    };
    let per_unit = |role: &str| match entry.kind {
        Opamp | Comparator => !repeatable_role(role),
        BjtNpn | BjtPnp => role.split("_q").next() == Some("base"),
        Digital | ShiftRegister => logic.inputs.iter().chain(&logic.outputs).any(|r| r == role),
        _ => false,
    };
    let mut pads = std::collections::BTreeMap::<&str, Vec<&str>>::new();
    for (pad, role) in entry.pins.iter().filter(|(_, r)| per_unit(r)) {
        if pad.len() > 1 || !pad.chars().all(|c| c.is_ascii_alphabetic()) {
            pads.entry(role).or_default().push(pad);
        }
    }
    let merged = pads.into_iter().filter(|(_, p)| p.len() > 1);
    merged.map(|(role, pads)| format!("[models.pins] pads {} all carry '{role}': give each unit of a multi-unit part its own suffixed roles ({units}), or the binder wires one unit and floats the rest; only supply, ground, nc and exposed-pad roles repeat", pads.join(", "))).collect()
}

/// True when the generic digital binder has executable logic for this card.
#[must_use]
pub fn digital_has_behavior(entry: &ModelEntry) -> bool {
    entry.kind != ComponentKind::Digital
        || !entry.logic.is_empty()
        || !entry.behavioral.is_empty()
        || entry.peripheral.is_some()
        || entry.pins.values().any(|role| {
            role.strip_prefix('a')
                .is_some_and(|n| entry.pins.values().any(|other| other == &format!("y{n}")))
        })
}

/// The per-kind parameter contract the solver reads: which params must be
/// present, the physical bounds on the ones that are (by value, or by
/// magnitude for a quantity whose sign carries meaning), and which pairs must
/// be strictly ordered so the solver never sees an inverted band.
struct KindRules {
    required: &'static [&'static str],
    ranges: &'static [(&'static str, f64, f64)],
    magnitude: &'static [(&'static str, f64, f64)],
    ordered: &'static [(&'static str, &'static str)],
}

const NO_RULES: KindRules = KindRules {
    required: &[],
    ranges: &[],
    magnitude: &[],
    ordered: &[],
};

fn validate_vreg_capacitor_params(entry: &ModelEntry, p: &mut Problems) {
    if entry.kind != ComponentKind::Vreg || entry.behavioral.converter.is_some() {
        return;
    }
    let params = &entry.params;
    if params.0.contains_key("cout_min_f") {
        let ok = params.get_f64("cout_min_f").is_some_and(nonneg_finite);
        p.require(ok, || {
            "param 'cout_min_f' must be a non-negative finite number".into()
        });
    }
    let esr_keys = ["cout_esr_min_ohm", "cout_esr_max_ohm"];
    for key in esr_keys {
        if params.0.contains_key(key) {
            p.require(params.get_f64(key).is_some_and(positive_finite), || {
                format!("param '{key}' must be a positive finite number")
            });
        }
    }
    let (min, max) = (params.get_f64(esr_keys[0]), params.get_f64(esr_keys[1]));
    p.require(min.is_some() == max.is_some(), || {
        "vreg output-capacitor ESR requires both 'cout_esr_min_ohm' and 'cout_esr_max_ohm'".into()
    });
    if let (Some(min), Some(max)) = (min, max) {
        p.require(min <= max, || {
            format!(
                "param '{}' = {min} must be less than or equal to '{}' = {max}",
                esr_keys[0], esr_keys[1]
            )
        });
    }
}

/// A programmed-current or behavioural vreg (charger, pmic) may abstain on an
/// `iq_a` or `dropout_v` printed as no single value by an `unknown` source
/// record; the rows stay characteristics. The binder then stamps no Iq and
/// flags the rail's headroom as unverified, never assumed. A BJT's is/nf and a
/// switch's roff may abstain the same way: the binder defaults them.
fn abstains(entry: &ModelEntry, key: &str) -> bool {
    use ComponentKind::{AnalogSwitch, BjtNpn, BjtPnp};
    let defaulted = matches!(
        (entry.kind, key),
        (BjtNpn | BjtPnp, "is" | "nf") | (AnalogSwitch, "roff")
    );
    let behaves = entry.current_program.is_some() || !entry.behavioral.is_empty();
    (defaulted || behaves && matches!(key, "iq_a" | "dropout_v"))
        && entry.source.as_ref().is_some_and(|s| {
            s.uncertainty.iter().any(
                |u| matches!(u, ModelUncertainty::Unknown { parameter, .. } if parameter == key),
            )
        })
}

/// Whether a cited row prints `v`, in `key`'s unit, in any column (a row in
/// another SI unit, such as a time row for a resistance, never counts).
fn printed(entry: &ModelEntry, key: &str, v: f64) -> bool {
    let (unit, sb) = (
        crate::param_names::param_unit(key).unwrap_or(""),
        crate::units::scale_between,
    );
    let scale =
        |r: &Characteristic| sb(&r.unit, unit).or_else(|| sb(&r.unit, &r.unit).xor(Some(1.0)));
    let hit =
        |r, x: f64| scale(r).is_some_and(|s| x.abs().mul_add(s, -v.abs()).abs() <= 1e-9 * v.abs());
    let mut rows = entry.source.iter().flat_map(|s| &s.characteristic);
    rows.any(|r| [r.min, r.typ, r.max].iter().flatten().any(|x| hit(r, *x)))
}

// One table: each kind's required params and physical ranges, one arm per kind.
#[allow(clippy::too_many_lines)]
fn kind_rules(entry: &ModelEntry) -> KindRules {
    use ComponentKind::{
        AnalogSwitch, BjtNpn, BjtPnp, Comparator, Diode, Nmos, Opamp, Pmos, Timer, Vreg,
    };
    match entry.kind {
        Diode => KindRules {
            required: &["is", "n", "rs"],
            ranges: &[
                ("is", 1e-30, 1e-3),
                ("n", 0.5, 3.0),
                ("rs", 0.0, 1000.0),
                ("cjo", 0.0, 1e-6),
            ],
            ..NO_RULES
        },
        BjtNpn | BjtPnp => KindRules {
            required: &["is", "bf", "nf"],
            ranges: &[
                ("is", 1e-20, 1e-3),
                ("bf", 1.0, 2000.0),
                ("nf", 0.5, 3.0),
                ("vaf", 1.0, 500.0),
                ("rb", 0.0, 1e6),
                ("rc", 0.0, 1e6),
                ("re", 0.0, 1e6),
            ],
            ..NO_RULES
        },
        // kp = k'·(W/L) for the level-1 SPICE model. A discrete POWER MOSFET's
        // effective W/L is enormous, so kp legitimately runs into the hundreds
        // (db/mosfet.toml carries 200 for ipa045n10n3g); bound generously,
        // still catching a nonsense hallucination.
        Nmos | Pmos => KindRules {
            required: &["vto", "kp"],
            ranges: &[
                ("vto", -10.0, 10.0),
                ("kp", 1e-6, 1000.0),
                ("lambda", 0.0, 1.0),
            ],
            ..NO_RULES
        },
        Vreg if entry.params.get_bool("shunt").unwrap_or(false) => KindRules {
            required: &["vref", "zka_ohms", "ikmax_a"],
            ranges: &[("zka_ohms", 1e-6, 1e6), ("ikmax_a", 1e-9, 1000.0)],
            magnitude: &[("vref", 0.1, 5.0)],
            ..NO_RULES
        },
        // An ADJUSTABLE regulator has no output setpoint of its own: the BOARD's
        // divider sets the rail, so requiring a `vout` here would be a false
        // fact. With no `gnd` pin (LM317 class) it holds `vref` between OUT and
        // ADJ, whose terminal is its only quiescent path, so `iadj_a` stands in
        // for `iq_a`. A `gnd` pin makes it ground-referenced, the binder's same
        // test: `vref` is held on FB above GND, IQ returns through GND, and the
        // FB bias is usually tiny or unpublished, so `iq_a` is what it needs.
        Vreg if entry.behavioral.converter.is_none() && entry.params.get_f64("vref").is_some() => {
            KindRules {
                required: if has_role(entry, "gnd") {
                    &["vref", "dropout_v", "iq_a"]
                } else {
                    &["vref", "dropout_v", "iadj_a"]
                },
                ranges: &[
                    ("dropout_v", 0.0, 10.0),
                    ("iadj_a", 0.0, 1e-3),
                    ("iq_a", 0.0, 1.0),
                ],
                magnitude: &[("vref", 0.1, 5.0)],
                ..NO_RULES
            }
        }
        // A declarative converter owns its output, input draw and regulation
        // semantics; `bind_vreg` never reads the simple-LDO tuple on that path,
        // so requiring invented `vout/dropout_v/iq_a` there would be false facts.
        // `vout` is judged by MAGNITUDE: the 79xx family regulates BELOW ground
        // and is stamped as a DC source against ground, so the sign is the
        // whole meaning.
        Vreg if entry.behavioral.converter.is_none() => KindRules {
            required: &["vout", "dropout_v", "iq_a"],
            ranges: &[("dropout_v", 0.0, 10.0), ("iq_a", 0.0, 1.0)],
            magnitude: &[("vout", 0.5, 30.0)],
            ordered: &[],
        },
        // A printed swing binds the rail to the supply net; the fixed level is then unread.
        Opamp => KindRules {
            required: &["gain", "rail_lo|swing_lo", "rail_hi|swing_hi"],
            ranges: &[
                ("gain", 1.0, 1e9),
                ("rail_lo", -60.0, 60.0),
                ("rail_hi", -60.0, 60.0),
                ("swing_lo", 0.0, 5.0),
                ("swing_hi", 0.0, 5.0),
            ],
            ordered: &[("rail_lo", "rail_hi")],
            ..NO_RULES
        },
        Comparator if entry.params.get_bool("open_collector").unwrap_or(false) => KindRules {
            required: &["ron_ohms", "leak_s"],
            ranges: &[("ron_ohms", 1e-6, 1e9), ("leak_s", 0.0, 1.0)],
            ..NO_RULES
        },
        Comparator => KindRules {
            required: &["out_lo", "out_hi", "hysteresis"],
            ranges: &[
                ("hysteresis", 0.0, 5.0),
                ("out_lo", -60.0, 60.0),
                ("out_hi", -60.0, 60.0),
            ],
            ordered: &[("out_lo", "out_hi")],
            ..NO_RULES
        },
        // A 555's trip points are FRACTIONS of VCC (that is the part's defining
        // property), so they are bounded away from the rails rather than given
        // in volts, and `trigger_frac < threshold_frac` is the ordering that
        // makes the two comparators a hysteresis band at all. `latch_voff <
        // latch_von` is the same claim about the SR latch's own band: swapped,
        // the relay would open where it should close and the part would
        // free-run at the solver's timestep.
        Timer => KindRules {
            required: &[
                "threshold_frac",
                "trigger_frac",
                "divider_ohms",
                "rsat_ohms",
            ],
            ranges: &[
                ("threshold_frac", 0.05, 0.95),
                ("trigger_frac", 0.05, 0.95),
                ("divider_ohms", 100.0, 1e6),
                ("rsat_ohms", 0.1, 1000.0),
                ("comparator_gain", 10.0, 1e5),
                ("timing_error_frac", 0.0, 0.5),
            ],
            ordered: &[
                ("trigger_frac", "threshold_frac"),
                ("latch_voff", "latch_von"),
            ],
            ..NO_RULES
        },
        // ron/roff ranges overlap, so a swapped pair (ron=5000, roff=2000)
        // would model a switch that conducts MORE when open; the order rule
        // closes that the way the opamp/comparator rails are ordered.
        AnalogSwitch => KindRules {
            required: &["ron", "roff"],
            ranges: &[("ron", 0.01, 10_000.0), ("roff", 1e3, 1e12)],
            ordered: &[("ron", "roff")],
            ..NO_RULES
        },
        // Digital / MCU / connector / ignore / behavioural vreg: no mandatory
        // numeric params.
        _ => NO_RULES,
    }
}

/// Validate a [`ModelEntry`], checking that required params are present and
/// within physical bounds. Returns `Ok(())` on success, or a list of violations.
///
/// # Errors
///
/// Returns every [`ValidationError`] found, not just the first.
// The entry validator: one flat walk, one rule per line, in the order the card's tables appear.
#[allow(clippy::too_many_lines)]
pub fn validate(entry: &ModelEntry) -> Result<(), Vec<ValidationError>> {
    let mut p = Problems::default();
    let params = &entry.params;
    let mut supplies = vec![0.0];
    supplies.extend(entry.ratings.max_voltage_v);

    for envelope in &entry.envelope {
        p.require(!envelope.basis().trim().is_empty(), || {
            "operating envelope requires a non-empty basis naming the datasheet table and row"
                .into()
        });
        match envelope {
            OperatingEnvelope::SupplyRange {
                pin,
                min_v,
                max_v,
                abs_max_v,
                ..
            } => {
                supplies.extend([*min_v, *max_v, abs_max_v.unwrap_or(0.0)]);
                p.require(has_role_ci(entry, pin), || {
                    not_a_role("operating envelope pin", pin)
                });
                let ordered = min_v.is_finite() && max_v.is_finite() && min_v < max_v;
                p.require(ordered, || {
                    format!(
                        "operating envelope requires finite min_v < max_v, got {min_v} and {max_v}"
                    )
                });
                p.require(
                    !abs_max_v.is_some_and(|v| !v.is_finite() || v < *max_v),
                    || format!("operating envelope abs_max_v must be finite and at least max_v {max_v}"),
                );
            }
            OperatingEnvelope::RailOrder { lower, upper, .. } => {
                for (field, role) in [("lower", lower), ("upper", upper)] {
                    p.require(has_role_ci(entry, role), || {
                        not_a_role(format!("operating envelope {field} role"), role)
                    });
                }
                p.require(!lower.eq_ignore_ascii_case(upper), || {
                    "operating envelope rail_order lower and upper roles must differ".into()
                });
            }
            OperatingEnvelope::InputCommonMode {
                supply,
                inputs,
                headroom_v,
                ..
            } => {
                for role in std::iter::once(supply).chain(inputs) {
                    p.require(has_role_ci(entry, role), || {
                        format!("operating envelope role '{role}' is not a role in [models.pins]")
                    });
                }
                p.require(inputs.len() >= 2 && headroom_v.is_finite() && *headroom_v >= 0.0, || {
                    format!("operating envelope input_common_mode needs at least two input roles and a finite non-negative headroom_v, got {} and {headroom_v}", inputs.len())
                });
            }
        }
    }

    // A pin map is a stated fact even on an identity-only card.
    p.0.extend(merged_unit_roles(entry));
    check_pinout(entry, &mut p);

    // An identity-only card is useful provenance, but it is deliberately not a
    // simulation model. Keep that state explicit and machine-checkable instead
    // of letting an empty `digital` card count as bound coverage. The engine
    // leaves these parts OPEN while retaining the winning model id/source.
    let identity_only = params.get_bool("identity_only").unwrap_or(false);
    p.require(
        !params.0.contains_key("identity_only") || params.get_bool("identity_only").is_some(),
        || "params.identity_only must be a boolean".into(),
    );
    if !identity_only && !digital_has_behavior(entry) {
        p.push("digital model has no executable behavior; add [models.logic] (for example, the builtin 74hc14 card declares inputs, outputs, and [models.logic.comb]) or set params.identity_only = true with warning and unlocked_by so binding leaves the part open");
    }
    if identity_only {
        for key in ["warning", "unlocked_by"] {
            p.require(
                params.get_str(key).is_some_and(|v| !v.trim().is_empty()),
                || format!("identity-only model requires non-empty params.{key} so reports state both the limitation and what would unlock behavior"),
            );
        }
        let behaves = !entry.logic.is_empty()
            || !entry.behavioral.is_empty()
            || entry.current_program.is_some()
            || entry.peripheral.is_some()
            || entry.peripheral_power.is_some();
        p.require(!behaves, || "identity-only model cannot also declare logic, behavioral physics, current_program, a firmware peripheral, or peripheral power; remove identity_only only after that behavior is validated".into());
        let required = kind_rules(entry).required;
        let abstention =
            crate::param_names::needless_abstention(entry, required, |k| abstains(entry, k));
        p.0.extend(abstention);
    }

    if let Some(peripheral) = &entry.peripheral {
        check_peripheral(entry, peripheral, &mut p);
    }

    if let Some(power) = &entry.peripheral_power {
        p.require(entry.peripheral.is_some(), || {
            "peripheral_power requires a [models.peripheral] protocol model".into()
        });
        for (field, role) in [
            ("supply_role", &power.supply_role),
            ("return_role", &power.return_role),
        ] {
            if role.trim().is_empty() {
                p.push(format!("peripheral_power {field} must not be empty"));
            } else {
                p.require(has_role(entry, role), || {
                    format!("peripheral_power {field} '{role}' is not a role in [models.pins]")
                });
            }
        }
        p.require(positive_finite(power.power_on_threshold_v), || {
            "peripheral_power power_on_threshold_v must be finite and positive".into()
        });
        for (field, value) in [
            ("idle_a", Some(power.idle_a)),
            ("read_a", Some(power.read_a)),
            ("write_a", Some(power.write_a)),
            ("low_power_a", power.low_power_a),
        ] {
            if let Some(v) = value {
                p.require(nonneg_finite(v), || {
                    format!("peripheral_power {field} must be finite and non-negative")
                });
            }
        }
    }

    // A behavior capability is an API key consumed by coverage requirements,
    // not a marketing sentence: stable vocabulary, no duplicates, never both
    // implemented and missing.
    let capability_ok = |v: &str| {
        !v.is_empty()
            && v.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    for (list, capabilities) in [
        ("implements", &entry.coverage.implements),
        ("missing", &entry.coverage.missing),
    ] {
        let mut seen = HashSet::new();
        for capability in capabilities {
            if !capability_ok(capability) {
                p.push(format!(
                    "coverage capability '{capability}' must use lowercase ASCII letters, digits, and underscores"
                ));
            } else if !seen.insert(capability.as_str()) {
                p.push(format!("coverage.{list} repeats '{capability}'"));
            } else if list == "missing" && entry.coverage.implements.contains(capability) {
                p.push(format!(
                    "coverage capability '{capability}' cannot be both implemented and missing"
                ));
            }
        }
    }

    if params.0.contains_key("must_not_float_roles") {
        match params.get_str("must_not_float_roles") {
            None => p.push(
                "params.must_not_float_roles must be a comma-separated string of [models.pins] roles",
            ),
            Some(raw) => {
                let mut seen = HashSet::new();
                let mut any = false;
                for role in raw.split(',').map(str::trim).filter(|r| !r.is_empty()) {
                    any = true;
                    if seen.insert(role.to_ascii_lowercase()) {
                        p.require(has_role_ci(entry, role), || {
                            format!("params.must_not_float_roles entry '{role}' is not a role in [models.pins]")
                        });
                    } else {
                        p.push(format!("params.must_not_float_roles repeats role '{role}'"));
                    }
                }
                p.require(any, || "params.must_not_float_roles names no roles".into());
            }
        }
    }

    // Identity-only entries name the intended part class but intentionally do
    // not satisfy its solver requirements or carry a complete pin map: fake
    // constants merely to pass lint would recreate the false coverage the
    // state exists to prevent.
    if !identity_only {
        let rules = kind_rules(entry);
        for key in rules.required {
            let given = |k| params.get_f64(k).is_some() || abstains(entry, k);
            p.require(key.split('|').any(given), || {
                format!("missing required param '{}'", key.replace('|', "' or '"))
            });
        }
        // A bound is never a value: a plain number sitting on a nonzero one (a
        // "placeholder at the floor") on a sourced card is refused unless a cited
        // row prints it (a hand-written, unsourced card claims no datasheet value).
        let plain = |&(key, lo, hi): &(&'static str, f64, f64)| {
            Some((key, lo, hi, params.0.get(key)?.as_f64()?)).filter(|_| entry.source.is_some())
        };
        for (key, lo, hi, v) in rules.ranges.iter().chain(rules.magnitude).filter_map(plain) {
            let x = if lo >= 0.0 { v.abs() } else { v };
            let at = |&(b, _): &(f64, _)| b != 0.0 && (x - b).abs() <= 1e-9 * b.abs();
            let bound = [(lo, "floor"), (hi, "ceiling")].into_iter().find(at);
            if let Some((b, side)) = bound.filter(|_| !printed(entry, key, v)) {
                p.push(format!("param '{key}' = {v:e} sits at the checker {side} {b:e}: a bound is not a datasheet value; cite a printed row that gives it, or omit it (an `unknown` record where the kind allows one)"));
            }
        }
        for (key, lo, hi) in rules.ranges {
            if let Some(value) = params.0.get(*key) {
                match params.get_f64(key) {
                    Some(v) => p.require(v.is_finite() && v >= *lo && v <= *hi, || {
                        format!("param '{key}' = {v} is outside physical range [{lo}, {hi}]")
                    }),
                    // A row's own problem is reported with the rows below.
                    None if matches!(value, ParamValue::Row(_)) => {}
                    None => p.push(format!("param '{key}' must be numeric")),
                }
            }
        }
        for (key, lo, hi) in rules.magnitude {
            if let Some(v) = params.get_f64(key) {
                p.require(v.is_finite() && v.abs() >= *lo && v.abs() <= *hi, || {
                    format!(
                        "param '{key}' = {v} is outside physical range (magnitude {lo} to {hi}, either sign)"
                    )
                });
            }
        }
        // A regulator entry is either fixed (`vout`) or ADJ-referenced (`vref`).
        // Carrying both would leave one of the two silently unread by the binder.
        let both = params.get_f64("vout").is_some() && params.get_f64("vref").is_some();
        p.require(!both, || {
            "a vreg entry carries either 'vout' (fixed) or 'vref' (adjustable), not both"
                .to_string()
        });
        // A fixed output's accuracy row spans at most ±5% (line, load, temperature),
        // so bounds wider than ±7% (max/min > 1.15) are the family's selectable outputs.
        if let Some(ParamValue::Row(r)) = params
            .0
            .get("vout")
            .filter(|_| entry.kind == ComponentKind::Vreg)
        {
            if let Some((a, b)) = r.min.zip(r.max) {
                let (lo, hi) = (a.abs().min(b.abs()), a.abs().max(b.abs()));
                p.require(a * b > 0.0 && hi <= 1.15 * lo, || format!("param 'vout' spans {a} to {b}: a fixed-output regulator's vout is the ordered variant's nominal output (its output-voltage-accuracy row's min/max at most), never the family's selectable output range"));
            }
        }
        validate_vreg_capacitor_params(entry, &mut p);
        // Logic levels on any kind: a Schmitt VT+/VT- swap inverts every input.
        let levels = [("vil", "vih"), ("vol", "voh")];
        for (lo, hi) in rules.ordered.iter().chain(&levels) {
            if let (Some(a), Some(b)) = (params.get_f64(lo), params.get_f64(hi)) {
                p.require(a < b, || {
                    format!("param '{lo}' = {a} must be strictly less than '{hi}' = {b}")
                });
            }
        }
        // With a swing published, an op-amp rail at ground, a stated supply or
        // half of one (split supply) is the supply copied, not a swing limit.
        let near = |v: f64, s: f64| (v.abs() - s.abs()).abs() < 1e-3;
        let at_supply = |v: &f64| supplies.iter().any(|s| near(*v, *s) || near(*v, s / 2.0));
        for side in ["lo", "hi"] {
            let get = |k: &str| params.get_f64(&format!("{k}_{side}"));
            let swing = get("swing").is_some_and(|s| s > 0.0) && entry.kind == ComponentKind::Opamp;
            if let Some(v) = get("rail").filter(|v| swing && at_supply(v)) {
                p.push(format!("param 'rail_{side}' = {v} is a supply level, not the output's swing limit: with swing_{side} printed, omit it or give the stated test supply minus that swing"));
            }
        }
        if crate::param_names::schottky(entry) && params.get_f64("tt").is_some_and(|t| t > 0.0) {
            p.push("a Schottky stores no minority charge and its printed trr is capacitive recovery: omit tt, never derive it from trr");
        }
        p.0.extend(crate::param_names::is_from_sat(entry));
        // Abstaining on params never makes a resolved pin table unknown.
        let unresolved = |role: &String| role.starts_with("unknown_pad_");
        p.require(entry.pins.is_empty() || !entry.pins.values().all(unresolved), || "every [models.pins] pad is unknown_pad_ on a card carrying params: name each pad the pin table resolves; unknown_pad_ is only for a pad the datasheet leaves unresolved".into());
        check_required_pins(entry, &mut p);
        check_behavioral_roles(entry, &mut p);
    }
    // No consumer reads a rating from [models.params]: the binder takes it from [models.ratings].
    for u in crate::param_names::unknown_params(entry) {
        let rating = |l: &str| l.split_whitespace().next() == Some(u.name.as_str());
        if crate::schema::Ratings::KEY_GUIDE.lines().any(rating) {
            p.push(format!(
                "param '{0}' is a rating, read only from [models.ratings]: move {0} there",
                u.name
            ));
        }
    }

    // Absolute-maximum ratings gate the stress/destruction faults, and the
    // monitor computes `if limit > 0.0 { value/limit } else { 0.0 }`: a NaN
    // or non-positive rating silently disables the fault (thermal.rs leaves
    // the thermal resistances unfloored the same way).
    let r = &entry.ratings;
    for (name, rating) in [
        ("max_current_a", r.max_current_a),
        ("max_surge_current_a", r.max_surge_current_a),
        ("max_power_w", r.max_power_w),
        ("max_voltage_v", r.max_voltage_v),
        ("max_pin_current_a", r.max_pin_current_a),
        ("max_ripple_current_a", r.max_ripple_current_a),
        ("max_junction_temp_c", r.max_junction_temp_c),
        ("theta_ja_c_per_w", r.theta_ja_c_per_w),
        ("theta_jc_c_per_w", r.theta_jc_c_per_w),
    ] {
        if let Some(v) = rating {
            p.require(positive_finite(v), || {
                format!("rating '{name}' = {v} must be a positive finite number")
            });
        }
    }

    if let Some(program) = &entry.current_program {
        check_current_program(entry, program, &mut p);
    }
    // A param written as its printed row: the row's own shape, then a unit and
    // column the simulator can read.
    for (key, row) in params.rows() {
        let mut named = row.clone();
        named.parameter = key.into();
        let read = crate::param_names::row_value(key, row).err();
        let problems = named.problems().into_iter().chain(read);
        p.0.extend(problems.map(|m| format!("param '{key}': {m}")));
    }
    if let Some(source) = &entry.source {
        p.0.extend(
            source
                .problems()
                .into_iter()
                .map(|m| format!("[models.source]: {m}")),
        );
        // No semiconductor or IC row reaches a farad: a printed pF/nF number
        // stored under `F` unconverted. A passive (a supercapacitor) may.
        for (i, row) in source.characteristic.iter().enumerate() {
            let farads = crate::units::si_unit(&row.unit).filter(|(_, base)| *base == "F");
            let big = [row.min, row.typ, row.max].into_iter().flatten();
            let big = big.filter(|v| farads.is_some_and(|(s, _)| (v * s).abs() >= 1.0));
            if let Some(v) = big
                .max_by(f64::total_cmp)
                .filter(|_| entry.kind != ComponentKind::Passive)
            {
                p.push(format!(
                    "[models.source]: characteristic {i} ({}): {v} {} is a farad or more, not a \
                     semiconductor capacitance: convert the printed pF/nF value",
                    row.parameter, row.unit
                ));
            }
        }
    }

    if p.0.is_empty() {
        Ok(())
    } else {
        Err(p
            .0
            .into_iter()
            .map(|message| ValidationError {
                id: entry.id.clone(),
                message,
            })
            .collect())
    }
}

// One arm per peripheral kind, each a flat list of its rules.
#[allow(clippy::too_many_lines)]
fn check_peripheral(entry: &ModelEntry, peripheral: &PeripheralSpec, p: &mut Problems) {
    match peripheral {
        PeripheralSpec::I2cEeprom {
            address,
            size_bytes,
            page_size,
            word_address_bytes,
        } => {
            p.require(*address <= 0x7f, || {
                format!("peripheral I2C address 0x{address:02x} is not a 7-bit address")
            });
            p.require(*size_bytes > 0, || {
                "peripheral I2C EEPROM size_bytes must be positive".into()
            });
            p.require(page_size.is_power_of_two() && page_size <= size_bytes, || {
                format!("peripheral I2C EEPROM page_size {page_size} must be a power of two no larger than size_bytes {size_bytes}")
            });
            p.require(matches!(word_address_bytes, 1 | 2), || {
                format!(
                    "peripheral I2C EEPROM word_address_bytes must be 1 or 2, got {word_address_bytes}"
                )
            });
        }
        PeripheralSpec::SpiNorFlash {
            size_bytes,
            page_size,
            sector_size,
            jedec_id,
            spi_mode,
            cs_role,
            clk_role,
            mosi_role,
            miso_role,
        } => {
            for (name, value) in [
                ("size_bytes", *size_bytes),
                ("page_size", *page_size),
                ("sector_size", *sector_size),
            ] {
                p.require(value > 0, || {
                    format!("peripheral SPI NOR {name} must be positive")
                });
            }
            let regions_ok = page_size.is_power_of_two()
                && sector_size.is_power_of_two()
                && (*size_bytes == 0 || (page_size <= size_bytes && sector_size <= size_bytes));
            p.require(regions_ok, || "peripheral SPI NOR page_size and sector_size must be power-of-two regions no larger than the array".into());
            p.require(jedec_id.len() == 3, || {
                format!(
                    "peripheral SPI NOR jedec_id must contain exactly 3 bytes, got {}",
                    jedec_id.len()
                )
            });
            p.require(*spi_mode <= 3, || {
                format!("peripheral SPI NOR spi_mode must be 0..3, got {spi_mode}")
            });
            for (name, role) in [
                ("cs_role", cs_role),
                ("clk_role", clk_role),
                ("mosi_role", mosi_role),
                ("miso_role", miso_role),
            ] {
                p.require(has_role(entry, role), || {
                    format!("peripheral SPI NOR {name} '{role}' is not a role in [models.pins]")
                });
            }
        }
        PeripheralSpec::RegisterMap {
            spec_toml,
            controller,
            scl_role,
            sda_role,
            cs_role,
            clk_role,
            mosi_role,
            miso_role,
            required_high_roles,
            required_low_roles,
            address_select_role,
            address_when_low,
            address_when_high,
        } => {
            let sensor = match SensorSpec::from_toml(spec_toml) {
                Ok(spec) => spec,
                Err(error) => {
                    p.push(format!(
                        "peripheral register-map spec_toml must be a valid [sensor] document: {error}"
                    ));
                    return;
                }
            };
            p.require(
                !controller
                    .as_ref()
                    .is_some_and(|name| name.trim().is_empty()),
                || "peripheral register-map controller must not be empty".into(),
            );
            p.require(sensor.sensor.source.is_none(), || {
                "peripheral register-map spec_toml must not carry [sensor.source]; \
                 the card's [models.source] is its provenance"
                    .into()
            });
            // The spec's [sensor.pins] supplies roles too, so it must agree with
            // the card's pad map wherever both name a pad.
            for (pad, role) in &sensor.sensor.pins {
                if let Some(card) = entry.pins.get(pad).filter(|card| *card != role) {
                    p.push(format!("pad '{pad}' is '{card}' in [models.pins] but '{role}' in the register-map [sensor.pins]"));
                }
            }
            let has_role = |entry: &ModelEntry, role: &str| {
                has_role(entry, role) || sensor.sensor.pins.values().any(|known| known == role)
            };
            let roles: &[(&str, &String)] = match sensor.sensor.bus {
                Bus::I2c => &[("scl_role", scl_role), ("sda_role", sda_role)],
                Bus::Spi => &[
                    ("cs_role", cs_role),
                    ("clk_role", clk_role),
                    ("mosi_role", mosi_role),
                    ("miso_role", miso_role),
                ],
            };
            for (name, role) in roles {
                p.require(!role.trim().is_empty() && has_role(entry, role), || {
                    not_a_role(format!("peripheral register-map {name}"), role)
                });
            }
            for (level, role) in required_high_roles
                .iter()
                .map(|role| ("high", role))
                .chain(required_low_roles.iter().map(|role| ("low", role)))
            {
                p.require(!role.trim().is_empty() && has_role(entry, role), || {
                    format!("peripheral register-map required-{level} role '{role}' is not a role in [models.pins]")
                });
            }
            for role in required_high_roles {
                p.require(!required_low_roles.contains(role), || {
                    format!("peripheral register-map role '{role}' cannot be required both high and low")
                });
            }

            // The address strap is an all-or-none contract: the binder refuses
            // a floating strap instead of choosing a plausible address.
            match (address_select_role, address_when_low, address_when_high) {
                (None, None, None) => {}
                (Some(_), Some(_), Some(_)) if !sensor.sensor.address_selects.is_empty() => p.push(
                    "peripheral register-map address_select_role and the spec's [[sensor.address_select]] \
                     both choose the address; keep one",
                ),
                (Some(role), Some(low), Some(high)) => {
                    p.require(sensor.sensor.bus == Bus::I2c, || {
                        "peripheral register-map address selection is only valid for I2C".into()
                    });
                    p.require(!role.trim().is_empty() && has_role(entry, role), || {
                        format!("peripheral register-map address-select role '{role}' is not a role in [models.pins]")
                    });
                    p.require(*low <= 0x7f && *high <= 0x7f && low != high, || {
                        format!("peripheral register-map address strap must select two distinct 7-bit I2C addresses, got 0x{low:02x}/0x{high:02x}")
                    });
                }
                _ => p.push("peripheral register-map address selection requires address_select_role, address_when_low, and address_when_high together"),
            }
        }
    }
}

/// Board-programmed current is solver-facing physics just like `params`:
/// malformed constants silently turn a real rail current into zero/NaN, and
/// confusing a normal-operating ceiling with a device-level safety threshold
/// makes the part promise operation in a region the datasheet never specified.
fn check_current_program(
    entry: &ModelEntry,
    program: &crate::schema::CurrentProgram,
    p: &mut Problems,
) {
    let role_ok = |role: &str| !role.trim().is_empty() && has_role_ci(entry, role.trim());
    p.require(role_ok(&program.pin), || {
        not_a_role("current_program.pin", &program.pin)
    });

    let regulated = program.semantics == CurrentProgramSemantics::RegulatedCurrent;
    for (field, roles) in [
        ("current_in_roles", &program.current_in_roles),
        ("current_out_roles", &program.current_out_roles),
    ] {
        p.require(!regulated || !roles.is_empty(), || {
            format!("current_program regulated_current requires non-empty {field}")
        });
        let mut seen = HashSet::new();
        for role in roles {
            if !role_ok(role) {
                p.push(not_a_role(format!("current_program.{field} entry"), role));
            } else if !seen.insert(role.trim().to_ascii_lowercase()) {
                p.push(format!("current_program.{field} repeats role '{role}'"));
            }
        }
    }
    for input in &program.current_in_roles {
        let both = program
            .current_out_roles
            .iter()
            .any(|output| output.eq_ignore_ascii_case(input));
        p.require(!both, || {
            format!("current_program role '{input}' appears in both current_in_roles and current_out_roles")
        });
    }

    let limit = program.max_operating_current_a;
    p.require(!regulated || limit.is_some(), || "current_program regulated_current requires max_operating_current_a so an undersized programming resistor cannot imply operation beyond the sourced domain".into());
    p.require(program.above_domain != AboveDomainBehavior::Saturate || limit.is_some(), || "current_program above_domain = saturate requires max_operating_current_a as the sourced saturation value".into());
    if let Some(limit) = limit {
        p.positive("current_program.max_operating_current_a", limit);
        // The programmed quantity is the part's rail/load current; a generic
        // per-pin source/sink limit bounds the PROG pin itself, not that rail.
        if let Some(device_limit) = entry.ratings.max_current_a.filter(|v| positive_finite(*v)) {
            p.require(!(limit.is_finite() && limit > device_limit), || {
                format!("current_program.max_operating_current_a = {limit} A exceeds ratings.max_current_a = {device_limit} A")
            });
        }
    }

    // Every numeric constant of the equation, named as the TOML names it.
    let mut valid = true;
    let fields = toml::Table::try_from(&program.equation).unwrap_or_default();
    for (name, v) in fields.iter().filter_map(|(k, v)| Some((k, v.as_float()?))) {
        p.positive(format!("current_program.{name}"), v);
        valid &= positive_finite(v);
    }
    match &program.equation {
        CurrentProgramEquation::PiecewiseInverseResistance {
            low_k_volts,
            transition_current_a,
            high_numerator_a,
            resistance_scale_ohms,
            high_offset,
        } if valid => {
            let transition_ohms = low_k_volts / transition_current_a;
            let high_at_transition =
                high_numerator_a / (transition_ohms / resistance_scale_ohms + high_offset);
            let gap = (high_at_transition - transition_current_a).abs() / transition_current_a;
            p.require(gap <= 0.01, || {
                format!("current_program piecewise branches are not continuous at {transition_current_a} A (high branch gives {high_at_transition} A)")
            });
        }
        CurrentProgramEquation::SenseScaledResistance {
            sense_roles,
            sense_far_roles,
            ..
        } => {
            p.require(!sense_roles.is_empty(), || {
                "current_program.sense_roles must name at least one role".into()
            });
            let mut seen = HashSet::new();
            for role in sense_roles {
                p.require(role_ok(role), || {
                    not_a_role("current_program.sense_roles entry", role)
                });
                p.require(seen.insert(role.trim().to_ascii_lowercase()), || {
                    format!("current_program.sense_roles repeats role '{role}'")
                });
            }
            p.require(sense_far_roles.len() == sense_roles.len(), || {
                format!(
                    "current_program.sense_far_roles has {} entries but sense_roles has {}",
                    sense_far_roles.len(),
                    sense_roles.len()
                )
            });
            for role in sense_far_roles {
                p.require(
                    role.eq_ignore_ascii_case("ground") || has_role_ci(entry, role.trim()),
                    || format!("current_program.sense_far_roles entry '{role}' is neither 'ground' nor a role in [models.pins]"),
                );
            }
        }
        _ => {}
    }
    check_program_scale(entry, program, p, valid);
}

/// The unit slip a current-program constant is checked for.
const K_SI: &str = "the constant is in volts (A·Ω), and a law printed with I in mA and R \
    in kΩ keeps its printed constant (mA·kΩ = V): check it for a 1000x slip";

/// A programming pin develops its reference (0.1 V to a few V) across the
/// resistor, so under 10 Ω it would sink tens of mA: a load shunt, not a
/// program resistor. A constant divided by 1000 (a law printed in mA and kΩ,
/// wrongly converted) moves a typical 1 to 10 kΩ full-scale resistor to 1 to 10 Ω.
const MIN_PROGRAM_OHMS: f64 = 10.0;

/// The law against the physical resistor range, and against every cited
/// charge or limit current row printed at a stated programming resistor
/// (`R<pin> = 10 kΩ`): the law must land within 25% of the printed columns.
fn check_program_scale(
    entry: &ModelEntry,
    program: &crate::schema::CurrentProgram,
    p: &mut Problems,
    valid: bool,
) {
    let full = program
        .max_operating_current_a
        .or(entry.ratings.max_current_a);
    let short = match &program.equation {
        CurrentProgramEquation::SenseScaledResistance {
            program_bias_a: bias,
            program_full_scale_v: volts,
            ..
        } => (volts / bias < MIN_PROGRAM_OHMS)
            .then(|| format!("program_full_scale_v / program_bias_a = {} Ω", volts / bias)),
        _ => full.zip(program.equation_current_a(MIN_PROGRAM_OHMS)).and_then(|(full, i)| {
            (i < full).then(|| format!("the law gives {i:.4} A at {MIN_PROGRAM_OHMS} Ω, short of the {full} A full scale"))
        }),
    };
    if let Some(short) = short.filter(|_| valid) {
        p.push(format!("current_program: {short}, so full scale needs a programming resistor under {MIN_PROGRAM_OHMS} Ω, which no programming pin takes; {K_SI}"));
    }
    let resistor = format!("(?i:R_?{})", regex::escape(program.pin.trim()));
    for row in entry.source.iter().flat_map(|s| &s.characteristic) {
        let name = row.parameter.to_ascii_lowercase();
        let programmed = ["charg", "chrg", "chg", "reg", "lim"]
            .iter()
            .any(|w| name.contains(w))
            && !["pre", "trickle", "term", "pin", "leak"]
                .iter()
                .any(|w| name.contains(w));
        let scale = crate::units::scale_between(&row.unit, "A").filter(|_| programmed && valid);
        let ohms = crate::param_names::quantity(&row.conditions, &resistor, "ohm");
        let law = ohms.and_then(|r| Some((r, program.equation_current_a(r)?)));
        let Some((scale, (ohms, law))) = scale.zip(law) else {
            continue;
        };
        let printed = [row.min, row.typ, row.max].into_iter().flatten();
        let printed = printed.map(|v| (v * scale).abs());
        let (lo, hi) = printed.fold((f64::INFINITY, 0.0_f64), |(lo, hi), v| {
            (lo.min(v), hi.max(v))
        });
        p.require((lo / 1.25..=hi * 1.25).contains(&law), || {
            let (what, at) = (&row.parameter, &row.conditions);
            format!("current_program: the law gives {law:.4} A at {ohms} Ω, but characteristic '{what}' prints {lo}..{hi} A there ({at}); {K_SI}")
        });
    }
}

/// A state-controlled series path or a profiled load is solver-facing
/// connectivity: both ends must name roles the entry actually maps from
/// physical pins. A typo here otherwise validates, then the runtime silently
/// skips the path and the part that was supposed to close a rail stays open.
fn check_behavioral_roles(entry: &ModelEntry, p: &mut Problems) {
    let declared: HashSet<String> = entry
        .pins
        .values()
        .map(|role| role.to_ascii_lowercase())
        .collect();
    let mut check = |field: String, role: &str| {
        p.require(declared.contains(&role.to_ascii_lowercase()), || {
            format!("behavioral.{field} role '{role}' is not present in [models.pins]")
        });
    };
    for (index, path) in entry.behavioral.series_paths.iter().enumerate() {
        check(format!("series_paths[{index}].a"), &path.a);
        check(format!("series_paths[{index}].b"), &path.b);
    }
    for (index, load) in entry.behavioral.profiled_loads.iter().enumerate() {
        check(
            format!("profiled_loads[{index}].supply_pin"),
            &load.supply_pin,
        );
        if let Some(role) = &load.return_pin {
            check(format!("profiled_loads[{index}].return_pin"), role);
        }
    }
}

/// When an entry supplies an explicit `[models.pins]` map, verify it carries the
/// signal roles the binder needs for that kind. A role typo (`"1" = "anmode"`)
/// otherwise passes every check above, then binds the part OPEN at run time with
/// a misleading "pin not connected" message; the exact trap a first-time part
/// author hits. This checks only that each REQUIRED role is PRESENT (under any
/// binder-accepted alias / channel suffix); it never flags EXTRA pins, so a
/// legitimately-declared power/NC pin (an op-amp's `vcc`/`vee`) is fine. An
/// empty pins map is the footprint/pin-rules inference path and is left alone.
fn check_required_pins(entry: &ModelEntry, p: &mut Problems) {
    // A register map's own `[sensor.pins]` declares the roles it binds.
    let sensor_pins = || match &entry.peripheral {
        Some(PeripheralSpec::RegisterMap { spec_toml, .. }) => {
            SensorSpec::from_toml(spec_toml).is_ok_and(|spec| !spec.sensor.pins.is_empty())
        }
        _ => false,
    };
    if entry.kind == ComponentKind::Digital && entry.pins.is_empty() && !sensor_pins() {
        p.push("digital model has no [models.pins]; without declared roles it can bind cleanly while driving and observing nothing");
        return;
    }
    p.0.extend(missing_pin_roles(entry));
}

/// Why a card's pin map leaves it OPEN at bind time: one line per role the
/// binder's path for its kind reads (`bind_vreg`, `bind_adj_vreg`, the device
/// binders) that the map does not declare.
///
/// `models resolve` reports the same lines, so a card that lints clean is not
/// left open for its pin map.
#[must_use]
pub fn missing_pin_roles(entry: &ModelEntry) -> Vec<String> {
    // A behavioral part (converter/FSM/DAC power IC) references its pins from
    // the [models.behavioral] block by arbitrary datasheet names, NOT through
    // the simple analog binder, so the canonical anchor roles do not apply;
    // except a vreg, which `bind_vreg` binds by role unless a converter owns it.
    let conv = entry.behavioral.converter.as_ref();
    let vreg = entry.kind == ComponentKind::Vreg;
    if entry.pins.is_empty() || (!entry.behavioral.is_empty() && !vreg) {
        return Vec::new();
    }
    let conv_roles = conv.map(|c| [[c.out_pin.as_str()], [c.in_pin.as_str()]]);
    // Each inner slice is one required role; the model satisfies it by mapping
    // some pin to ANY name in the slice, after normalization. Names are the
    // binder's accepted aliases. analog_switch is deliberately EXCLUDED: it
    // binds SPST (`in_out_a`/`in_out_b`) and SPDT (`com`/`s0`/`s1`) forms with
    // too varied a vocabulary to anchor-check without false positives.
    let conv_required: Vec<&[&str]> = conv_roles.iter().flatten().map(|r| &r[..]).collect();
    let required: &[&[&str]] = match entry.kind {
        ComponentKind::Vreg if conv.is_some() => &conv_required,
        ComponentKind::Diode => &[&["anode", "a", "p"], &["cathode", "k", "n"]],
        ComponentKind::BjtNpn | ComponentKind::BjtPnp => {
            &[&["collector", "c"], &["base", "b"], &["emitter", "e"]]
        }
        ComponentKind::Nmos | ComponentKind::Pmos => {
            &[&["drain", "d"], &["gate", "g"], &["source", "s"]]
        }
        // A set-point regulator binds its input by the exact `in` role (a
        // datasheet's VDD/VIN name leaves it OPEN); a shunt reference has none.
        // A fixed one returns through `gnd`; an adjustable one may through ADJ alone.
        ComponentKind::Vreg if entry.params.get_bool("shunt").unwrap_or(false) => &[&["out"]],
        ComponentKind::Vreg if entry.params.get_f64("vref").is_some() => &[&["in"], &["out"]],
        ComponentKind::Vreg if entry.params.get_f64("vout").is_some() => {
            &[&["in"], &["out"], &["gnd"]]
        }
        ComponentKind::Vreg => &[&["out"]],
        ComponentKind::Opamp | ComponentKind::Comparator => &[
            &["out"],
            &["in_plus", "inp", "in+"],
            &["in_minus", "inn", "in-"],
        ],
        _ => return Vec::new(),
    };
    let declared: HashSet<String> = entry.pins.values().map(|role| fold_role(role)).collect();
    let missing = required
        .iter()
        .filter(|f| !f.iter().any(|n| declared.contains(&fold_role(n))));
    let why = |f: &&[&str]| {
        let (role, kind, names) = (f[0], entry.kind, f.join(" / "));
        let adj = (role == "gnd" && declared.contains("adj")).then_some(". A fixed-output part's ADJ/GND pin is its ground: map it 'gnd'; 'adj' is for an adjustable card (vref, no vout)");
        format!("[models.pins] declares no '{role}' pin (a {kind:?} needs it); the part would bind OPEN. Accepted role names: {names}{}", adj.unwrap_or_default())
    };
    missing.map(why).collect()
}

/// A role folded onto its base: lowercase, less a trailing channel suffix
/// (`_a`..`_d` or `_q<N>`), then a trailing digit run + underscore. This folds
/// the binder's channel variants onto the base role: `out_1`->`out`,
/// `d1`/`d2`->`d` (dual MOSFET), `collector_q2`->`collector`.
fn fold_role(role: &str) -> String {
    let mut r = role.to_ascii_lowercase();
    if let Some(base) = ["_a", "_b", "_c", "_d"]
        .iter()
        .find_map(|sfx| r.strip_suffix(sfx))
    {
        r = base.to_string();
    }
    if let Some(idx) = r.rfind("_q") {
        if idx + 2 < r.len() && r[idx + 2..].bytes().all(|c| c.is_ascii_digit()) {
            r.truncate(idx);
        }
    }
    r.trim_end_matches(|c: char| c.is_ascii_digit())
        .trim_end_matches('_')
        .to_string()
}

/// A role or printed pin name as a [`fold_role`] key less an active-low `_n`
/// and a rail `V` prefix: `VIN`, `IN` and the role `in` agree.
fn pin_key(name: &str) -> String {
    let folded = fold_role(name);
    let base = folded.strip_suffix("_n").unwrap_or(&folded);
    let rest = base.strip_prefix('v').filter(|r| r.len() > 1);
    let rail = rest.filter(|r| r.starts_with(|c: char| c.is_ascii_alphabetic()));
    rail.unwrap_or(base).to_string()
}

/// The keys of a printed pin name: each word ("NC/GND") and the whole name ("N.C.").
fn pin_name_keys(name: &str) -> Vec<String> {
    let whole: String = name
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    let words = name.split(|c: char| !c.is_alphanumeric() && c != '_');
    let words = words.chain([whole.as_str()]).filter(|w| !w.is_empty());
    words.map(pin_key).collect()
}

/// `[models.pins]` against the pin-table column of the card's own package
/// (`[models.variant] package`, else the one package the description names,
/// else a lone column): the same pads, and no pad whose role is the name that
/// column prints on another pad, the mark of a map read off another package.
///
/// An extracted card keys every pad by its number or ball id: the binder reads a
/// card's pins by the board footprint's pad numbers, so a lead letter binds nothing.
fn check_pinout(entry: &ModelEntry, p: &mut Problems) {
    let source = entry.source.as_ref();
    let plain = |s: &str| s.to_lowercase().replace(|c: char| !c.is_alphanumeric(), "");
    let pad = |k: &&String| {
        let k = plain(k);
        k.contains(char::is_numeric) || k.ends_with("pad") || ["ep", "tab", "dap"].contains(&&*k)
    };
    let lettered: Vec<_> = entry.pins.keys().filter(|k| !pad(k)).cloned().collect();
    if source.is_some_and(|s| s.tier == ModelSourceTier::DatasheetDerived) && !lettered.is_empty() {
        p.push(format!("[models.pins] keys {} are lead letters or pin names, not pad numbers: a card binds by the footprint's pad numbers, so they bind nothing. Key each pad by its number (ball id on a BGA) from the pin-configuration table, the package drawing, or the numbers printed beside the letters; if the datasheet prints none, omit [models.pins] and list \"pad_numbers\" in [models.coverage] missing rather than guess", lettered.join(", ")));
    }
    let pinouts = source.map_or(&[][..], |s| s.pinout.as_slice());
    if pinouts.is_empty() || entry.pins.is_empty() {
        return;
    }
    let description = plain(&entry.description);
    let declared = entry.variant.as_ref().and_then(|v| v.package.as_deref());
    let named: Vec<&Pinout> = pinouts
        .iter()
        .filter(|o| match (declared, plain(&o.package)) {
            (Some(package), own) => own == plain(package),
            (None, own) => pinouts.len() == 1 || description.contains(&own),
        })
        .collect();
    // A description naming "SOT-23-5" also contains "SOT-23": the longer is meant.
    let within = |o: &Pinout, b: &Pinout| {
        b.package.len() > o.package.len() && plain(&b.package).contains(&plain(&o.package))
    };
    let named: Vec<&Pinout> = named
        .iter()
        .copied()
        .filter(|o| !named.iter().any(|b| within(o, b)))
        .collect();
    let [own] = named[..] else {
        let all: Vec<&str> = pinouts.iter().map(|o| o.package.as_str()).collect();
        p.push(format!("[[models.source.pinout]] records {}, but neither [models.variant] package nor the description names exactly one of them: name the one package [models.pins] is for", all.join(", ")));
        return;
    };
    let package = &own.package;
    if !own.pins.keys().eq(entry.pins.keys()) {
        let pads = |m: &std::collections::BTreeMap<String, String>| {
            m.keys().map(String::as_str).collect::<Vec<_>>().join(", ")
        };
        p.push(format!("[models.pins] pads ({}) are not the '{package}' pinout's pads ({}): map exactly the declared package's pads", pads(&entry.pins), pads(&own.pins)));
    }
    for (pad, role) in &entry.pins {
        let key = pin_key(role);
        let prints = |name: &str| pin_name_keys(name).contains(&key);
        let Some(here) = own.pins.get(pad).filter(|name| !prints(name)) else {
            continue;
        };
        if role.starts_with("unknown_pad_") {
            p.push(format!("[models.pins] pad {pad} = '{role}', but the '{package}' pinout prints '{here}' on it: name each pad the pin table resolves; unknown_pad_ is only for a pad the datasheet leaves unresolved"));
        }
        if let Some((other, name)) = own.pins.iter().find(|(q, n)| *q != pad && prints(n)) {
            p.push(format!("[models.pins] pad {pad} = '{role}', but the '{package}' pinout prints '{here}' on pad {pad} and '{name}' on pad {other}: read [models.pins] from the declared package's own column and figure; a figure labelled with another package is not this card's"));
        }
    }
}

// ── Kind vocabulary help ──────────────────────────────────────────────────────

/// Every TOML spelling [`ComponentKind`] accepts, in declaration order. Kept
/// in lockstep with the enum by `kind_vocabulary_and_suggestions` below.
pub const KIND_NAMES: &[&str] = &[
    "passive",
    "diode",
    "bjt_npn",
    "bjt_pnp",
    "nmos",
    "pmos",
    "vreg",
    "opamp",
    "comparator",
    "timer",
    "analog_switch",
    "digital",
    "dac",
    "adc",
    "shift_register",
    "mcu",
    "connector",
    "ignore",
];

/// Every bound [`validate`] holds `kind`'s params to, across the variants a param
/// selects, so the extraction prompt quotes the checker, not a drifting copy.
#[must_use]
pub fn param_bounds(kind: ComponentKind) -> Vec<String> {
    let num = |v: f64| match v.abs() {
        a if a == 0.0 || (1e-3..1e5).contains(&a) => v.to_string(),
        _ => format!("{v:e}"),
    };
    let mut lines = Vec::new();
    for probe in [None, Some("vref"), Some("shunt"), Some("open_collector")] {
        let mut entry = ModelEntry {
            kind,
            ..ModelEntry::default()
        };
        match probe {
            Some("vref") => entry.params.set_f64("vref", 1.0),
            Some(flag) => entry.params.set_bool(flag, true),
            None => {}
        }
        let rules = kind_rules(&entry);
        let by_value = rules.ranges.iter().map(|range| (range, ""));
        for (&(key, lo, hi), how) in by_value.chain(rules.magnitude.iter().map(|m| (m, "|x| "))) {
            let line = format!("{key}: {how}{} to {}", num(lo), num(hi));
            if !lines.contains(&line) {
                lines.push(line);
            }
        }
    }
    lines
}

/// A did-you-mean for a kind name the schema rejected.
///
/// Common industry synonyms map directly (an author typing `ldo` should be told
/// `vreg`, not left to guess which of seventeen names we meant); anything else
/// falls back to nearest-edit-distance over [`KIND_NAMES`].
#[must_use]
pub fn kind_suggestion(unknown: &str) -> Option<&'static str> {
    let lower = unknown.trim().to_ascii_lowercase();
    let alias = match lower.as_str() {
        // Regulators and power ICs of every flavour model as `vreg` (the
        // behavioural block carries what the base kind cannot).
        "ldo" | "regulator" | "buck" | "boost" | "buck_boost" | "smps" | "dcdc" | "dc_dc"
        | "pmic" | "charger" => Some("vreg"),
        "npn" | "bjt" | "transistor" => Some("bjt_npn"),
        "pnp" => Some("bjt_pnp"),
        "mosfet" | "fet" | "nfet" | "n_mosfet" | "nmosfet" => Some("nmos"),
        "pfet" | "p_mosfet" | "pmosfet" => Some("pmos"),
        "op_amp" | "operational_amplifier" | "amplifier" => Some("opamp"),
        "resistor" | "capacitor" | "inductor" | "res" | "cap" | "ferrite" | "crystal" => {
            Some("passive")
        }
        "led" | "zener" | "schottky" | "rectifier" | "tvs" => Some("diode"),
        "switch" | "mux" | "multiplexer" => Some("analog_switch"),
        "microcontroller" | "micro" | "soc" => Some("mcu"),
        "header" | "jack" | "socket" | "plug" => Some("connector"),
        "logic" | "gate" | "flip_flop" | "latch" => Some("digital"),
        _ => None,
    };
    alias.or_else(|| {
        KIND_NAMES
            .iter()
            .map(|k| (hauksbee_ir::levenshtein(&lower, k), *k))
            .filter(|(d, _)| *d <= 2)
            .min_by_key(|(d, _)| *d)
            .map(|(_, k)| k)
    })
}

/// If a TOML deserialization error is an unknown [`ComponentKind`] variant, the
/// note to append: the did-you-mean, or the full vocabulary.
///
/// Detected from the error text (serde owns the wording); the `bjt_npn` probe
/// keeps it from firing on some other enum's unknown-variant error.
#[must_use]
pub fn kind_error_note(err_text: &str) -> Option<String> {
    if !err_text.contains("unknown variant") || !err_text.contains("bjt_npn") {
        return None;
    }
    let unknown = err_text.split('`').nth(1)?;
    let Some(s) = kind_suggestion(unknown) else {
        return Some(format!(
            "unknown kind '{unknown}'; valid kinds: {}",
            KIND_NAMES.join(", ")
        ));
    };
    Some(format!("unknown kind '{unknown}': did you mean '{s}'?"))
}

const SOURCE_KEYS: &str = "tier validation references uncertainty characteristic pinout";
const RECORD_KINDS: &str = " ([[...source.uncertainty]] status = \"interval\" bounds one \
    parameter by printed min AND max as low/high; [[...source.characteristic]] keeps a \
    printed row's min/typ/max columns as printed)";

/// Every missing and unknown key of every `[models.source]` / `[sensor.source]`
/// record.
///
/// Read from the raw TOML so one lint run names them all: a parse stops at the
/// first, and an uncertainty record drops an unknown key silently, so a `typ`
/// written there would otherwise vanish.
pub fn source_shape_findings(root: &toml::Value) -> Vec<String> {
    let models = root.get("models").and_then(toml::Value::as_array);
    let sources = models.into_iter().flatten().enumerate();
    let sources = sources
        .filter_map(|(i, m)| Some((format!("models[{i}].source"), m.get("source")?)))
        .chain(
            root.get("sensor")
                .and_then(|s| s.get("source"))
                .map(|s| ("sensor.source".into(), s)),
        );
    let source_keys: Vec<&str> = SOURCE_KEYS.split_whitespace().collect();
    let mut out = Vec::new();
    for (path, table) in sources.filter_map(|(p, s)| Some((p, s.as_table()?))) {
        for (key, value) in table {
            if !source_keys.contains(&key.as_str()) {
                out.push(format!(
                    "{path}: unknown key '{key}'{}",
                    key_hint(key, &source_keys)
                ));
                continue;
            }
            let records = value.as_array().filter(|_| key != "references");
            for (i, fields) in records.into_iter().flatten().enumerate() {
                let Some(fields) = fields.as_table() else {
                    continue;
                };
                let status = fields.get("status").and_then(toml::Value::as_str);
                let (required, optional) = match (key.as_str(), status) {
                    ("characteristic", _) => ("parameter unit conditions basis", "min typ max"),
                    ("pinout", _) => ("package pins", ""),
                    (_, Some("interval")) => ("status parameter low high unit kind basis", ""),
                    (_, Some("unknown")) => ("status parameter reason", ""),
                    _ => ("status", "parameter low high unit kind basis reason"),
                };
                let required: Vec<&str> = required.split_whitespace().collect();
                let at = format!("{path}.{key}[{i}]");
                for missing in required.iter().filter(|k| !fields.contains_key(**k)) {
                    out.push(format!("{at}: missing required key '{missing}'"));
                }
                let known: Vec<&str> = required
                    .iter()
                    .copied()
                    .chain(optional.split_whitespace())
                    .collect();
                for extra in fields.keys().filter(|k| !known.contains(&k.as_str())) {
                    out.push(format!(
                        "{at}: unknown key '{extra}'{}",
                        key_hint(extra, &known)
                    ));
                }
            }
        }
    }
    out
}

/// The key an unknown one probably meant, and for a column or interval word
/// used in the wrong record, the one-line difference between the two records.
fn key_hint(key: &str, known: &[&str]) -> String {
    let swap = match key {
        "low" => "min",
        "high" => "max",
        "min" => "low",
        "max" => "high",
        _ => "",
    };
    let swap = known.contains(&swap).then_some(swap);
    let near = swap.or_else(|| {
        let distance = |k: &str| hauksbee_ir::levenshtein(key, k);
        let near = known.iter().copied().filter(|k| distance(k) <= 2);
        near.min_by_key(|k| distance(k))
    });
    let near = near.map_or(String::new(), |k| format!("; did you mean '{k}'?"));
    let explain = ["interval", "typ"].contains(&key);
    format!("{near}{}", if explain { RECORD_KINDS } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        AboveDomainBehavior, ComponentKind, CurrentProgramEquation, ModelEntry, Params,
        PeripheralPower, PeripheralSpec,
    };

    fn entry(kind: ComponentKind, params: &[(&str, f64)]) -> ModelEntry {
        let mut p = Params::default();
        for (k, v) in params {
            p.set_f64(*k, *v);
        }
        ModelEntry {
            id: "t".into(),
            kind,
            params: p,
            ..Default::default()
        }
    }

    fn diode() -> ModelEntry {
        entry(
            ComponentKind::Diode,
            &[("is", 1e-14), ("n", 1.5), ("rs", 1.0)],
        )
    }

    fn pins(e: &mut ModelEntry, roles: &[(&str, &str)]) {
        e.pins = roles.iter().map(|&(p, r)| (p.into(), r.into())).collect();
    }

    /// The entry fails, and one error names `needle` (so the right rule fired).
    fn rejects(e: &ModelEntry, needle: &str) {
        let errs = validate(e).expect_err(needle);
        assert!(
            errs.iter().any(|x| x.message.contains(needle)),
            "expected an error naming {needle:?}: {errs:?}"
        );
    }

    fn accepts(e: &ModelEntry) {
        assert!(validate(e).is_ok(), "{:?}", validate(e));
    }

    /// Refused: a merged unit role, a swapped Schmitt pair, a rail at a supply.
    #[test]
    fn merged_units_swapped_levels_and_supply_rails_are_refused() {
        fn roles(s: &str) -> Vec<(&str, &str)> {
            s.split(' ').filter_map(|r| r.split_once(':')).collect()
        }
        let mut e = entry(
            ComponentKind::Opamp,
            &[("gain", 1e5), ("swing_lo", 0.02), ("swing_hi", 1.5)],
        );
        let dual = "4:vee 8:vcc 1:out_a 2:in_minus_a 3:in_plus_a 5:in_plus_b 6:in_minus_b 7:out_b";
        pins(&mut e, &roles(dual));
        accepts(&e);
        pins(&mut e, &roles(&dual.replace("7:out_b", "7:out_a")));
        rejects(&e, "pads 1, 7 all carry 'out_a': give each unit");
        pins(&mut e, &roles(dual));
        e.params.set_f64("rail_hi", 5.0);
        e.ratings.max_voltage_v = Some(10.0);
        rejects(&e, "'rail_hi' = 5 is a supply level");
        e.params.set_f64("rail_hi", 3.5);
        accepts(&e);
        let mut q = entry(
            ComponentKind::BjtNpn,
            &[("is", 1e-14), ("bf", 200.0), ("nf", 1.0)],
        );
        pins(
            &mut q,
            &roles("1:emitter_q1 2:base 3:collector_q2 4:emitter_q2 5:base 6:collector_q1"),
        );
        rejects(&q, "pads 2, 5 all carry 'base'");
        rejects(
            &entry(ComponentKind::Digital, &[("vil", 2.5), ("vih", 1.5)]),
            "'vil' = 2.5 must be strictly less",
        );
    }

    #[test]
    #[rustfmt::skip]
    fn param_ranges_and_orderings() {
        use ComponentKind::*;
        accepts(&diode());
        rejects(&entry(Diode, &[("is", 1.0), ("n", 1.5), ("rs", 1.0)]), "is");
        let missing = entry(Diode, &[("is", 1e-14)]);
        rejects(&missing, "'n'");
        rejects(&missing, "'rs'");
        rejects(
            &entry(
                BjtNpn,
                &[("is", 1e-14), ("bf", 9999.0), ("nf", 1.0), ("vaf", 80.0)],
            ),
            "bf",
        );

        rejects(
            &entry(Opamp, &[("gain", 1e5), ("rail_lo", 5.0), ("rail_hi", 0.0)]),
            "rail_lo",
        );
        accepts(&entry(
            Opamp,
            &[("gain", 1e5), ("rail_lo", 0.0), ("rail_hi", 5.0)],
        ));

        for kp in [4.5, 30.0, 200.0] {
            accepts(&entry(Nmos, &[("vto", 2.0), ("kp", kp)]));
        }
        rejects(&entry(Nmos, &[("vto", 2.0), ("kp", 5000.0)]), "kp");

        rejects(
            &entry(
                Vreg,
                &[("vout", f64::NAN), ("dropout_v", 0.3), ("iq_a", 1e-3)],
            ),
            "vout",
        );
        let vreg = |extra: &[(&str, f64)]| {
            let mut p = vec![("vout", 3.3), ("dropout_v", 0.3), ("iq_a", 1e-3)];
            p.extend_from_slice(extra);
            entry(Vreg, &p)
        };
        for bad in [-1e-6, f64::NAN, f64::INFINITY] {
            rejects(&vreg(&[("cout_min_f", bad)]), "cout_min_f");
        }
        for (params, error) in [
            (
                vec![("cout_esr_min_ohm", 0.0), ("cout_esr_max_ohm", 1.0)],
                "cout_esr_min_ohm",
            ),
            (vec![("cout_esr_min_ohm", 0.1)], "requires both"),
            (
                vec![("cout_esr_min_ohm", 1.0), ("cout_esr_max_ohm", 0.1)],
                "less than or equal",
            ),
        ] {
            rejects(&vreg(&params), error);
        }
        for cout in [0.0, 1e-6] {
            accepts(&vreg(&[("cout_min_f", cout)]));
            for (min, max) in [(0.01, 1.0), (0.01, 0.01)] {
                accepts(&vreg(&[
                    ("cout_min_f", cout),
                    ("cout_esr_min_ohm", min),
                    ("cout_esr_max_ohm", max),
                ]));
            }
        }
        let mut wrong_type = vreg(&[]);
        wrong_type.params.set_str("cout_min_f", "1u");
        rejects(&wrong_type, "non-negative finite number");
        // Adjustable: a `gnd` pin (ground-referenced) needs `iq_a`; without one
        // (LM317 class) the ADJ current `iadj_a` is the quiescent path.
        for (gnd, have, missing) in [(true, "iq_a", "iadj_a"), (false, "iadj_a", "iq_a")] {
            let adj = |key| {
                let mut e = entry(Vreg, &[("vref", 0.8), ("dropout_v", 0.3), (key, 5e-5)]);
                pins(&mut e, &[("1", "in"), ("2", "out"), ("3", "fb"), ("4", if gnd { "gnd" } else { "nc" })]);
                e
            };
            accepts(&adj(have));
            rejects(&adj(missing), &format!("missing required param '{have}'"));
        }
        let mut vdd = vreg(&[]);
        pins(&mut vdd, &[("1", "vdd"), ("2", "out"), ("3", "gnd")]);
        rejects(&vdd, "declares no 'in' pin");
        // A behavioral pin without a converter still binds through `bind_vreg`.
        let mut sink = vreg(&[]);
        pins(&mut sink, &[("1", "in"), ("2", "gate"), ("3", "gnd")]);
        sink.behavioral.pins.insert("gate".into(), crate::behavioral::BehavioralPin::default());
        rejects(&sink, "declares no 'out' pin");

        rejects(
            &entry(AnalogSwitch, &[("ron", 5000.0), ("roff", 2000.0)]),
            "ron",
        );
        accepts(&entry(AnalogSwitch, &[("ron", 5.0), ("roff", 1e9)]));

        rejects(
            &entry(
                Comparator,
                &[("out_lo", 3.3), ("out_hi", 0.0), ("hysteresis", 0.05)],
            ),
            "out_lo",
        );
    }

    #[test]
    fn ratings_must_be_positive_and_finite() {
        let mut e = diode();
        e.ratings.max_current_a = Some(f64::NAN);
        rejects(&e, "max_current_a");
        let mut e = diode();
        e.ratings.max_voltage_v = Some(-75.0);
        rejects(&e, "max_voltage_v");
        let mut e = diode();
        e.ratings.theta_ja_c_per_w = Some(-50.0);
        rejects(&e, "theta_ja_c_per_w");
        let mut e = diode();
        e.ratings.theta_jc_c_per_w = Some(f64::NAN);
        rejects(&e, "theta_jc_c_per_w");

        let mut e = diode();
        e.ratings.max_current_a = Some(1.0);
        e.ratings.max_voltage_v = Some(75.0);
        e.ratings.theta_ja_c_per_w = Some(62.0);
        accepts(&e);
    }

    #[test]
    fn malformed_operating_envelopes_are_rejected() {
        let fixture = |body: &str| -> ModelEntry {
            toml::from_str(&format!(
                "id = \"e\"\nkind = \"digital\"\n[params]\nidentity_only = true\nwarning = \"w\"\n\
                 unlocked_by = \"u\"\n[pins]\n\"1\" = \"vcc\"\n[[envelope]]\nkind = \"supply_range\"\n{body}"
            ))
            .unwrap()
        };
        accepts(&fixture(
            "pin = \"vcc\"\nmin_v = 2.7\nmax_v = 3.6\nbasis = \"ROC, VCC row\"",
        ));
        rejects(&fixture("pin = \"vcc\"\nmin_v = 2.7\nmax_v = 3.6"), "basis");
        rejects(
            &fixture("pin = \"missing\"\nmin_v = 2.7\nmax_v = 3.6\nbasis = \"ROC\""),
            "missing",
        );
        rejects(
            &fixture("pin = \"vcc\"\nmin_v = 3.6\nmax_v = 2.7\nbasis = \"ROC\""),
            "min_v",
        );
    }

    #[test]
    fn identity_only_requires_an_explicit_unlock_and_no_behavior() {
        let mut e = diode();
        e.params = Params::default();
        e.params.set_bool("identity_only", true);
        e.params.set_str("warning", "pin identity only");
        rejects(&e, "unlocked_by");
        e.params
            .set_str("unlocked_by", "validated diode I-V parameters");
        accepts(&e);
        e.logic.inputs.push("a".into());
        rejects(&e, "cannot also declare");
    }

    #[test]
    fn behaviorless_digital_card_must_be_identity_only() {
        rejects(&entry(ComponentKind::Digital, &[]), "identity_only = true");
    }

    #[test]
    fn must_not_float_roles_must_be_pins() {
        let mut e = diode();
        pins(
            &mut e,
            &[
                ("1", "wp_n"),
                ("2", "hold_n"),
                ("3", "anode"),
                ("4", "cathode"),
            ],
        );
        e.params.set_str("must_not_float_roles", "wp_n, hold_n");
        accepts(&e);
        e.params.set_str("must_not_float_roles", "wp_n, missing");
        rejects(&e, "missing");
    }

    const fn i2c_eeprom() -> PeripheralSpec {
        PeripheralSpec::I2cEeprom {
            address: 0x50,
            size_bytes: 128,
            page_size: 8,
            word_address_bytes: 1,
        }
    }

    #[test]
    fn peripheral_power_requires_protocol_connected_roles_and_physical_values() {
        let mut e = entry(ComponentKind::Digital, &[]);
        pins(
            &mut e,
            &[("1", "scl"), ("2", "gnd"), ("3", "sda"), ("4", "vcc")],
        );
        e.peripheral = Some(i2c_eeprom());
        e.peripheral_power = Some(PeripheralPower {
            supply_role: "vcc".into(),
            return_role: "gnd".into(),
            power_on_threshold_v: 1.7,
            idle_a: 6e-6,
            read_a: 1e-3,
            write_a: 3e-3,
            low_power_a: Some(1e-6),
        });
        accepts(&e);

        e.peripheral = None;
        rejects(&e, "requires a [models.peripheral]");
        e.peripheral = Some(i2c_eeprom());

        e.peripheral_power.as_mut().unwrap().supply_role = "missing".into();
        rejects(&e, "not a role");
        e.peripheral_power.as_mut().unwrap().supply_role = "vcc".into();

        e.peripheral_power.as_mut().unwrap().write_a = -1.0;
        rejects(&e, "write_a");
    }

    #[test]
    fn register_map_peripheral_requires_valid_spec_and_board_roles() {
        let spec = "[sensor]\nname = \"WHOAMI\"\nbus = \"i2c\"\ni2c_address = 0x18\n\
                    [[sensor.register]]\naddr = 0x00\nconst = [0x13]\n[sensor.protocol]\nstyle = \"i2c_pointer\"\n";
        let mut e = entry(ComponentKind::Digital, &[]);
        pins(
            &mut e,
            &[
                ("1", "scl"),
                ("2", "gnd"),
                ("3", "sda"),
                ("4", "vcc"),
                ("5", "sdo"),
                ("6", "csb"),
            ],
        );
        let make = |spec_toml: &str, sda_role: &str, address_when_high: Option<u8>| {
            Some(PeripheralSpec::RegisterMap {
                spec_toml: spec_toml.into(),
                controller: None,
                scl_role: "scl".into(),
                sda_role: sda_role.into(),
                cs_role: "cs".into(),
                clk_role: "sck".into(),
                mosi_role: "mosi".into(),
                miso_role: "miso".into(),
                required_high_roles: vec!["csb".into()],
                required_low_roles: vec![],
                address_select_role: Some("sdo".into()),
                address_when_low: Some(0x18),
                address_when_high,
            })
        };
        e.peripheral = make(spec, "sda", Some(0x19));
        accepts(&e);
        e.peripheral = make(spec, "sda", None);
        rejects(&e, "requires address_select_role");
        e.peripheral = make(
            "[sensor]\nname = \"broken\"\nbus = \"i2c\"",
            "sda",
            Some(0x19),
        );
        rejects(&e, "register-map");
        e.peripheral = make(spec, "missing_sda", Some(0x19));
        rejects(&e, "missing_sda");
        let sourced = format!("{spec}[sensor.source]\ntier = \"datasheet-derived\"\n");
        e.peripheral = make(&sourced, "sda", Some(0x19));
        rejects(&e, "must not carry [sensor.source]");
        let pinned = spec.replace(
            "[sensor.protocol]",
            "[sensor.pins]\n1 = \"scl\"\n3 = \"sda\"\n7 = \"a0\"\n[sensor.protocol]",
        );
        e.peripheral = make(&pinned, "sda", Some(0x19));
        accepts(&e);
        let swapped = pinned.replace("1 = \"scl\"\n3 = \"sda\"", "1 = \"sda\"\n3 = \"scl\"");
        e.peripheral = make(&swapped, "sda", Some(0x19));
        rejects(&e, "'sda' in the register-map [sensor.pins]");
        let table = "[[sensor.address_select]]\naddress = 0x18\nstraps = { a0 = \"low\" }\n";
        e.peripheral = make(&format!("{pinned}{table}"), "sda", Some(0x19));
        rejects(&e, "keep one");
    }

    #[test]
    fn typoed_pin_role_is_caught_but_extra_pins_and_channel_suffixes_pass() {
        let mut d = diode();
        pins(&mut d, &[("1", "anmode"), ("2", "cathode")]);
        rejects(&d, "anode");

        let mut ok = diode();
        pins(&mut ok, &[("1", "a"), ("2", "k"), ("3", "case")]);
        accepts(&ok);
        pins(&mut ok, &[("A", "anode"), ("K", "cathode")]); // letters bind by board pin names only
        accepts(&ok);
        accepts(&diode());

        let mut op = entry(
            ComponentKind::Opamp,
            &[("gain", 1e5), ("rail_lo", 0.0), ("rail_hi", 12.0)],
        );
        pins(
            &mut op,
            &[("1", "out_a"), ("2", "in_minus_a"), ("3", "in_plus_a")],
        );
        accepts(&op);
    }

    fn programmed_vreg() -> ModelEntry {
        toml::from_str(
            r#"
id = "programmed_vreg"
kind = "vreg"
[params]
vout = 4.2
dropout_v = 0.3
iq_a = 0.001
[pins]
"1" = "in"
"2" = "gnd"
"3" = "prog"
"4" = "out"
"5" = "sense_a"
"6" = "sense_b"
[ratings]
max_current_a = 0.8
[current_program]
pin = "prog"
semantics = "regulated_current"
current_in_roles = ["in"]
current_out_roles = ["out"]
max_operating_current_a = 0.4
equation = "piecewise_inverse_resistance"
low_k_volts = 1000.0
transition_current_a = 0.15
high_numerator_a = 1.2
resistance_scale_ohms = 1000.0
high_offset = 1.3333333333333333
"#,
        )
        .unwrap()
    }

    fn with_program(f: impl FnOnce(&mut crate::schema::CurrentProgram)) -> ModelEntry {
        let mut e = programmed_vreg();
        f(e.current_program.as_mut().unwrap());
        e
    }

    #[test]
    fn source_shape_names_every_missing_and_misplaced_key() {
        let card = |source: &str| -> toml::Value {
            toml::from_str(&format!(
                "[[models]]\nid = \"x\"\n[models.source]\ntier = \"datasheet-derived\"\n{source}"
            ))
            .unwrap()
        };
        let row =
            "parameter = \"VF\"\nunit = \"V\"\nconditions = \"IF = 1 mA\"\nbasis = \"Table 1\"";
        let good = format!("[[models.source.characteristic]]\n{row}\nmax = 0.4\n[[models.source.uncertainty]]\nstatus = \"unknown\"\nparameter = \"model\"\nreason = \"none\"");
        assert_eq!(source_shape_findings(&card(&good)), Vec::<String>::new());
        let bad = "[[models.source.characteristic]]\nparameter = \"VF\"\nlow = 0.3\n\
                   [[models.source.uncertainty]]\nstatus = \"interval\"\nparameter = \"is\"\nlow = 1.0\nhigh = 2.0\ntyp = 1.5\nunit = \"A\"\nkind = \"specification-limits\"\nbasis = \"b\"\n\
                   [[models.source.interval]]\nparameter = \"vdd\"";
        let found = source_shape_findings(&card(bad)).join("\n");
        for needle in [
            "characteristic[0]: missing required key 'unit'",
            "characteristic[0]: missing required key 'basis'",
            "characteristic[0]: unknown key 'low'; did you mean 'min'?",
            "uncertainty[0]: unknown key 'typ' ([[...source.uncertainty]]",
            "models[0].source: unknown key 'interval' (",
        ] {
            assert!(found.contains(needle), "{needle}\n{found}");
        }
    }

    #[test]
    fn charger_iq_abstains_by_record_and_capacitance_rows_stay_below_a_farad() {
        let source = |extra: &str| -> Option<crate::schema::ModelSourceSpec> {
            Some(toml::from_str(&format!("tier = \"datasheet-derived\"\n{extra}")).expect("parses"))
        };
        let unknown =
            "[[uncertainty]]\nstatus = \"unknown\"\nparameter = \"iq_a\"\nreason = \"max only\"";
        let mut e = programmed_vreg();
        e.params.0.remove("iq_a");
        rejects(&e, "missing required param 'iq_a'");
        e.source = source(unknown);
        accepts(&e);
        let mut plain = entry(ComponentKind::Vreg, &[("vout", 3.3), ("dropout_v", 0.3)]);
        plain.source = source(unknown);
        rejects(&plain, "missing required param 'iq_a'");

        let row = |value: &str| {
            let text = format!("[[characteristic]]\nparameter = \"CT\"\ntyp = {value}\nconditions = \"VR = 0 V\"\nbasis = \"Table 1\"");
            source(&text)
        };
        let mut d = diode();
        d.source = row("2.0\nunit = \"pF\"");
        accepts(&d);
        d.source = row("2.0\nunit = \"F\"");
        rejects(&d, "a farad or more");
    }

    #[test]
    fn current_program_equations_and_operating_limits_are_validated() {
        accepts(&programmed_vreg());
        accepts(&with_program(|p| p.max_operating_current_a = Some(0.1)));
        rejects(
            &with_program(|p| p.pin = "not_a_pin".into()),
            "current_program.pin",
        );
        rejects(
            &with_program(|p| p.current_out_roles.clear()),
            "current_out_roles",
        );
        rejects(
            &with_program(|p| p.current_out_roles = vec!["IN".into()]),
            "both current_in_roles and current_out_roles",
        );
        rejects(
            &with_program(|p| p.max_operating_current_a = Some(0.0)),
            "max_operating_current_a",
        );
        rejects(
            &with_program(|p| p.max_operating_current_a = None),
            "requires max_operating_current_a",
        );
        rejects(
            &with_program(|p| {
                p.max_operating_current_a = None;
                p.above_domain = AboveDomainBehavior::Saturate;
            }),
            "above_domain = saturate",
        );
        rejects(
            &with_program(|p| p.max_operating_current_a = Some(0.9)),
            "ratings.max_current_a",
        );

        let mut pin_limit = programmed_vreg();
        pin_limit.ratings.max_current_a = None;
        pin_limit.ratings.max_pin_current_a = Some(0.3);
        accepts(&pin_limit);

        let sense = |sense_b: &str, far: Vec<&str>, bias: f64| {
            with_program(|p| {
                p.equation = CurrentProgramEquation::SenseScaledResistance {
                    sense_roles: vec!["sense_a".into(), sense_b.into()],
                    sense_far_roles: far.into_iter().map(String::from).collect(),
                    program_bias_a: bias,
                    program_full_scale_v: 1.0,
                    sense_full_scale_v: 0.05,
                }
            })
        };
        accepts(&sense("sense_b", vec!["in", "ground"], 50e-6));
        rejects(
            &sense("not_a_pin", vec!["in", "ground"], 50e-6),
            "sense_roles",
        );
        rejects(
            &sense("SENSE_A", vec!["in", "ground"], 50e-6),
            "repeats role",
        );
        rejects(&sense("sense_b", vec!["in"], 50e-6), "sense_far_roles");
        rejects(
            &sense("sense_b", vec!["not_a_pin", "ground"], 50e-6),
            "not_a_pin",
        );
        rejects(
            &sense("sense_b", vec!["in", "ground"], 0.0),
            "program_bias_a",
        );

        rejects(
            &with_program(|p| {
                p.equation = CurrentProgramEquation::PiecewiseInverseResistance {
                    low_k_volts: 1000.0,
                    transition_current_a: 0.15,
                    high_numerator_a: 1.2,
                    resistance_scale_ohms: 1000.0,
                    high_offset: 3.0,
                }
            }),
            "continuous",
        );
        rejects(
            &with_program(|p| {
                p.equation = CurrentProgramEquation::InverseResistance { k_volts: f64::NAN }
            }),
            "k_volts",
        );

        // I(mA) = 1000 / R(kΩ) is k_volts = 1000: the 1000x slip, a correct
        // constant, and a constant a cited RPROG row contradicts.
        let row = "[[characteristic]]\nparameter = \"ICHG charge current\"\ntyp = 100\n\
                   unit = \"mA\"\nconditions = \"RPROG = 10 kΩ\"\nbasis = \"Table 1, typ\"";
        for (k_volts, needle) in [
            (1.0, Some("1000x slip")),
            (1000.0, None),
            (2000.0, Some("prints 0.1..0.1 A")),
        ] {
            let mut e = with_program(|p| {
                p.equation = CurrentProgramEquation::InverseResistance { k_volts }
            });
            e.source = toml::from_str(&format!("tier = \"datasheet-derived\"\n{row}")).ok();
            needle.map_or_else(|| accepts(&e), |needle| rejects(&e, needle));
        }
    }

    /// A regulator whose sheet prints a five-pin and a four-pin package column.
    const LDO_FIXTURE: &str = r#"
id = "ldo_fixture"
kind = "vreg"
description = "LDO, SOT-23-5 package"
[params]
vout = 3.3
dropout_v = 0.3
iq_a = 0.0001
[pins]
"1" = "in"
"2" = "gnd"
"3" = "en"
"4" = "nc"
"5" = "out"
[source]
tier = "datasheet-derived"
[[source.pinout]]
package = "SOT-23-5"
pins = { "1" = "VIN", "2" = "GND", "3" = "EN", "4" = "NC", "5" = "VOUT" }
[[source.pinout]]
package = "DFN-4"
pins = { "1" = "VOUT", "2" = "GND", "3" = "EN", "4" = "VIN", "EP" = "GND" }
"#;

    /// Also: a vout row wider than an accuracy band (either sign), a rating
    /// written as a param, and a pin table left wholly `unknown_pad_`.
    #[test]
    #[rustfmt::skip]
    fn pins_match_the_pinout_and_vout_rows_and_ratings_sit_where_read() {
        let card = |edits: &[(&str, &str)]| -> ModelEntry {
            let text = edits
                .iter()
                .fold(LDO_FIXTURE.to_string(), |s, (a, b)| s.replace(a, b));
            toml::from_str(&text).unwrap()
        };
        accepts(&card(&[]));
        let dfn_dropped = LDO_FIXTURE
            .find("[[source.pinout]]\npackage = \"DFN")
            .unwrap();
        accepts(&card(&[
            (&LDO_FIXTURE[dfn_dropped..], ""),
            ("SOT-23-5 package", ""),
        ]));
        let row = |v: &str| format!("vout = {{ {v}, unit = \"V\", conditions = \"c\", basis = \"b\" }}");
        let (vout, pinouts) = ("vout = 3.3", &LDO_FIXTURE[LDO_FIXTURE.find("[[source.pinout]]").unwrap()..]);
        let unknown = [("\"in\"", "\"unknown_pad_1\""), ("\"gnd\"", "\"unknown_pad_2\""), ("\"en\"", "\"unknown_pad_3\""), ("\"nc\"", "\"unknown_pad_4\""), ("\"out\"", "\"unknown_pad_5\"")];
        let identity = "[params]\nidentity_only = true\nwarning = \"w\"\nunlocked_by = \"u\"";
        // Case: edits | the refusal's text, or "" for a card that must pass.
        for (edits, needle) in [
            (vec![(vout, row("min = 3.2, typ = 3.3, max = 3.4"))], ""),
            (vec![(vout, row("min = -5.2, max = -4.8"))], ""),
            (vec![(vout, row("min = 0.8, max = 3.3"))], "never the family's selectable output range"),
            (vec![(vout, row("min = -12, max = -5"))], "param 'vout' spans -12 to -5"),
            (vec![(vout, row("min = -3.3, max = 3.3"))], "param 'vout' spans -3.3 to 3.3"),
            (vec![("iq_a = 0.0001", "iq_a = 0.0001\nmax_current_a = 0.5".into())], "move max_current_a there"),
            (vec![(unknown[0].0, unknown[0].1.into())], "pinout prints 'VIN' on it: name each pad"),
            (vec![("\"2\" = \"gnd\"", "\"2\" = \"adj\"".into())], "ADJ/GND pin is its ground: map it 'gnd'"),
            (vec![("\"1\" = \"in\"", "\"VIN\" = \"in\"".into())], "keys VIN are lead letters or pin names"),
            (vec![(pinouts, String::new()), ("\"4\" = \"nc\"", "\"EP\" = \"nc\"\n\"A4\" = \"nc\"".into())], ""),
        ] {
            let e = card(&edits.iter().map(|(a, b)| (*a, b.as_str())).collect::<Vec<_>>());
            if needle.is_empty() { accepts(&e) } else { rejects(&e, needle) }
        }
        let wholly_unknown = [&unknown[..], &[(pinouts, "")]].concat();
        rejects(&card(&wholly_unknown), "every [models.pins] pad is unknown_pad_");
        accepts(&card(&[&wholly_unknown[..], &[("[params]", identity)]].concat()));
        for (edits, needle) in [
            // VOUT taken from the other package's drawing, VIN from elsewhere.
            (
                &[
                    ("\"1\" = \"in\"", "\"1\" = \"out\""),
                    ("\"5\" = \"out\"", "\"5\" = \"in\""),
                ][..],
                "the 'SOT-23-5' pinout prints 'VIN' on pad 1 and 'VOUT' on pad 5",
            ),
            (&[("SOT-23-5 package", "")], "names exactly one"),
            (
                &[("SOT-23-5 package", "DFN-4 package")],
                "not the 'DFN-4' pinout's pads",
            ),
        ] {
            rejects(&card(edits), needle);
        }
    }

    #[test]
    fn kind_vocabulary_and_suggestions() {
        // serde's unknown-variant error lists every variant in order, so this
        // pins both the order and the completeness of KIND_NAMES.
        let err = serde_json::from_value::<ComponentKind>("__not_a_kind__".into()).unwrap_err();
        let listed: Vec<String> = KIND_NAMES.iter().map(|k| format!("`{k}`")).collect();
        let expected = format!("expected one of {}", listed.join(", "));
        assert!(err.to_string().contains(&expected), "{err}");
        assert_eq!(kind_suggestion("LDO"), Some("vreg"));
        assert_eq!(kind_suggestion("npn"), Some("bjt_npn"));
        assert_eq!(kind_suggestion("led"), Some("diode"));
        assert_eq!(kind_suggestion("pasive"), Some("passive"));
        assert_eq!(kind_suggestion("opamps"), Some("opamp"));
        assert_eq!(kind_suggestion("zzzzzz"), None);

        let note = kind_error_note("unknown variant `ldo`, expected one of `passive`, `bjt_npn`");
        assert!(note.unwrap().contains("vreg"));
        assert_eq!(kind_error_note("some other error"), None);
    }
}
