//! Physical-range validation for model parameters. Used by the extraction
//! pipeline to reject LLM-generated model entries whose parameters are missing or
//! out of physical bounds before they are saved to the user database, so a
//! hallucinated part value cannot silently enter the model library. [`validate`]
//! collects every violation at once rather than stopping at the first.

use std::collections::HashSet;

use crate::check::{nonneg_finite, positive_finite, Problems};
use crate::schema::{
    AboveDomainBehavior, ComponentKind, CurrentProgramEquation, CurrentProgramSemantics,
    ModelEntry, OperatingEnvelope, PeripheralSpec,
};
use crate::sensor_spec::{Bus, SensorSpec};
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

fn kind_rules(entry: &ModelEntry) -> KindRules {
    use ComponentKind::*;
    match entry.kind {
        Diode => KindRules {
            required: &["is", "n", "rs"],
            ranges: &[
                ("is", 1e-20, 1e-3),
                ("n", 0.5, 3.0),
                ("rs", 0.0, 1000.0),
                ("cjo", 0.0, 1e-6),
            ],
            ..NO_RULES
        },
        BjtNpn | BjtPnp => KindRules {
            required: &["is", "bf", "nf", "vaf"],
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
        Opamp => KindRules {
            required: &["gain", "rail_lo", "rail_hi"],
            ranges: &[
                ("gain", 1.0, 1e9),
                ("rail_lo", -60.0, 60.0),
                ("rail_hi", -60.0, 60.0),
            ],
            ordered: &[("rail_lo", "rail_hi")],
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
pub fn validate(entry: &ModelEntry) -> Result<(), Vec<ValidationError>> {
    let mut p = Problems::default();
    let params = &entry.params;

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
                p.require(has_role_ci(entry, pin), || {
                    format!("operating envelope pin '{pin}' is not a role in [models.pins]")
                });
                p.require(
                    min_v.is_finite() && max_v.is_finite() && min_v < max_v,
                    || {
                        format!(
                        "operating envelope requires finite min_v < max_v, got {min_v} and {max_v}"
                    )
                    },
                );
                p.require(
                    !abs_max_v.is_some_and(|v| !v.is_finite() || v < *max_v),
                    || format!("operating envelope abs_max_v must be finite and at least max_v {max_v}"),
                );
            }
            OperatingEnvelope::RailOrder { lower, upper, .. } => {
                for (field, role) in [("lower", lower), ("upper", upper)] {
                    p.require(has_role_ci(entry, role), || {
                        format!(
                            "operating envelope {field} role '{role}' is not a role in [models.pins]"
                        )
                    });
                }
                p.require(!lower.eq_ignore_ascii_case(upper), || {
                    "operating envelope rail_order lower and upper roles must differ".into()
                });
            }
        }
    }

    // An identity-only card is useful provenance, but it is deliberately not a
    // simulation model. Keep that state explicit and machine-checkable instead
    // of letting an empty `digital` card count as bound coverage. The engine
    // leaves these parts OPEN while retaining the winning model id/source.
    let identity_only = params.get_bool("identity_only").unwrap_or(false);
    p.require(
        !params.0.contains_key("identity_only") || params.get_bool("identity_only").is_some(),
        || "params.identity_only must be a boolean".into(),
    );
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
        p.require(entry.coverage.implements.is_empty(), || {
            "identity-only model cannot declare coverage.implements; it has no executable behavior"
                .into()
        });
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
                    if !seen.insert(role.to_ascii_lowercase()) {
                        p.push(format!("params.must_not_float_roles repeats role '{role}'"));
                    } else {
                        p.require(has_role_ci(entry, role), || {
                            format!("params.must_not_float_roles entry '{role}' is not a role in [models.pins]")
                        });
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
            p.require(params.get_f64(key).is_some(), || {
                format!("missing required param '{key}'")
            });
        }
        for (key, lo, hi) in rules.ranges {
            if let Some(v) = params.get_f64(key) {
                p.require(v.is_finite() && v >= *lo && v <= *hi, || {
                    format!("param '{key}' = {v} is outside physical range [{lo}, {hi}]")
                });
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
        for (lo, hi) in rules.ordered {
            if let (Some(a), Some(b)) = (params.get_f64(lo), params.get_f64(hi)) {
                p.require(a < b, || {
                    format!("param '{lo}' = {a} must be strictly less than '{hi}' = {b}")
                });
            }
        }
        check_required_pins(entry, &mut p);
        check_behavioral_roles(entry, &mut p);
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
                    format!(
                        "peripheral register-map {name} '{role}' is not a role in [models.pins]"
                    )
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
        format!(
            "current_program.pin '{}' is not a role in [models.pins]",
            program.pin
        )
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
                p.push(format!(
                    "current_program.{field} entry '{role}' is not a role in [models.pins]"
                ));
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

    let positive = |p: &mut Problems, name: &str, v: f64| {
        p.positive(format!("current_program.{name}"), v);
        positive_finite(v)
    };
    match &program.equation {
        CurrentProgramEquation::InverseResistance { k_volts } => {
            positive(p, "k_volts", *k_volts);
        }
        CurrentProgramEquation::PowerLawResistance {
            coefficient_a,
            resistance_scale_ohms,
            exponent,
        } => {
            for (name, v) in [
                ("coefficient_a", *coefficient_a),
                ("resistance_scale_ohms", *resistance_scale_ohms),
                ("exponent", *exponent),
            ] {
                positive(p, name, v);
            }
        }
        CurrentProgramEquation::PiecewiseInverseResistance {
            low_k_volts,
            transition_current_a,
            high_numerator_a,
            resistance_scale_ohms,
            high_offset,
        } => {
            let mut valid = true;
            for (name, v) in [
                ("low_k_volts", *low_k_volts),
                ("transition_current_a", *transition_current_a),
                ("high_numerator_a", *high_numerator_a),
                ("resistance_scale_ohms", *resistance_scale_ohms),
                ("high_offset", *high_offset),
            ] {
                valid &= positive(p, name, v);
            }
            if valid {
                let transition_ohms = low_k_volts / transition_current_a;
                let high_at_transition =
                    high_numerator_a / (transition_ohms / resistance_scale_ohms + high_offset);
                let gap = (high_at_transition - transition_current_a).abs() / transition_current_a;
                p.require(gap <= 0.01, || {
                    format!("current_program piecewise branches are not continuous at {transition_current_a} A (high branch gives {high_at_transition} A)")
                });
            }
        }
        CurrentProgramEquation::SenseScaledResistance {
            sense_roles,
            sense_far_roles,
            program_bias_a,
            program_full_scale_v,
            sense_full_scale_v,
        } => {
            for (name, v) in [
                ("program_bias_a", *program_bias_a),
                ("program_full_scale_v", *program_full_scale_v),
                ("sense_full_scale_v", *sense_full_scale_v),
            ] {
                positive(p, name, v);
            }
            p.require(!sense_roles.is_empty(), || {
                "current_program.sense_roles must name at least one role".into()
            });
            let mut seen = HashSet::new();
            for role in sense_roles {
                p.require(role_ok(role), || {
                    format!(
                        "current_program.sense_roles entry '{role}' is not a role in [models.pins]"
                    )
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
    if entry.kind == ComponentKind::Digital && entry.pins.is_empty() {
        p.push("digital model has no [models.pins]; without declared roles it can bind cleanly while driving and observing nothing");
        return;
    }
    // A behavioral part (converter/FSM/DAC power IC) references its pins from
    // the [models.behavioral] block by arbitrary datasheet names, NOT through
    // the simple analog binder, so the canonical anchor roles do not apply.
    if entry.pins.is_empty() || !entry.behavioral.is_empty() {
        return;
    }
    // Each inner slice is one required role; the model satisfies it by mapping
    // some pin to ANY name in the slice, after normalization. Names are the
    // binder's accepted aliases. analog_switch is deliberately EXCLUDED: it
    // binds SPST (`in_out_a`/`in_out_b`) and SPDT (`com`/`s0`/`s1`) forms with
    // too varied a vocabulary to anchor-check without false positives.
    let required: &[&[&str]] = match entry.kind {
        ComponentKind::Diode => &[&["anode", "a", "p"], &["cathode", "k", "n"]],
        ComponentKind::BjtNpn | ComponentKind::BjtPnp => {
            &[&["collector", "c"], &["base", "b"], &["emitter", "e"]]
        }
        ComponentKind::Nmos | ComponentKind::Pmos => {
            &[&["drain", "d"], &["gate", "g"], &["source", "s"]]
        }
        ComponentKind::Vreg => &[&["out"]],
        ComponentKind::Opamp | ComponentKind::Comparator => &[
            &["out"],
            &["in_plus", "inp", "in+"],
            &["in_minus", "inn", "in-"],
        ],
        _ => return,
    };

    // Normalize each declared role: lowercase, strip a trailing channel suffix
    // (`_a`..`_d` or `_q<N>`), then a trailing digit run + underscore. This
    // folds the binder's channel variants onto the base role: `out_1`->`out`,
    // `d1`/`d2`->`d` (dual MOSFET), `collector_q2`->`collector`.
    let normalize = |role: &str| -> String {
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
    };
    let declared: HashSet<String> = entry.pins.values().map(|role| normalize(role)).collect();

    for family in required {
        p.require(family.iter().any(|name| declared.contains(*name)), || {
            format!(
                "[models.pins] declares no '{}' pin (a {:?} needs it); the part would bind OPEN. Accepted role names: {}",
                family[0],
                entry.kind,
                family.join(" / ")
            )
        });
    }
}

// ── Kind vocabulary help ──────────────────────────────────────────────────────

/// Every TOML spelling [`ComponentKind`] accepts, in declaration order. Kept
/// in lockstep with the enum by `kind_names_cover_the_enum` below.
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
    "analog_switch",
    "digital",
    "dac",
    "adc",
    "shift_register",
    "mcu",
    "connector",
    "ignore",
];

/// A did-you-mean for a kind name the schema rejected. Common industry
/// synonyms map directly (an author typing `ldo` should be told `vreg`, not
/// left to guess which of seventeen names we meant); anything else falls back
/// to nearest-edit-distance over [`KIND_NAMES`].
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

/// If a TOML deserialization error is an unknown [`ComponentKind`] variant,
/// the note to append: the did-you-mean, or the full vocabulary. Detected
/// from the error text (serde owns the wording); the `bjt_npn` probe keeps it
/// from firing on some other enum's unknown-variant error.
pub fn kind_error_note(err_text: &str) -> Option<String> {
    if !err_text.contains("unknown variant") || !err_text.contains("bjt_npn") {
        return None;
    }
    let unknown = err_text.split('`').nth(1)?;
    Some(match kind_suggestion(unknown) {
        Some(s) => format!("unknown kind '{unknown}': did you mean '{s}'?"),
        None => format!(
            "unknown kind '{unknown}'; valid kinds: {}",
            KIND_NAMES.join(", ")
        ),
    })
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
        e.pins = roles
            .iter()
            .map(|(p, r)| (p.to_string(), r.to_string()))
            .collect();
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

    #[test]
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

    fn i2c_eeprom() -> Option<PeripheralSpec> {
        Some(PeripheralSpec::I2cEeprom {
            address: 0x50,
            size_bytes: 128,
            page_size: 8,
            word_address_bytes: 1,
        })
    }

    #[test]
    fn peripheral_power_requires_protocol_connected_roles_and_physical_values() {
        let mut e = entry(ComponentKind::Digital, &[]);
        pins(
            &mut e,
            &[("1", "scl"), ("2", "gnd"), ("3", "sda"), ("4", "vcc")],
        );
        e.peripheral = i2c_eeprom();
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
        e.peripheral = i2c_eeprom();

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
    }

    #[test]
    fn typoed_pin_role_is_caught_but_extra_pins_and_channel_suffixes_pass() {
        let mut d = diode();
        pins(&mut d, &[("1", "anmode"), ("2", "cathode")]);
        rejects(&d, "anode");

        let mut ok = diode();
        pins(&mut ok, &[("1", "a"), ("2", "k"), ("3", "case")]);
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
    }

    #[test]
    fn kind_vocabulary_and_suggestions() {
        #[derive(Debug, serde::Deserialize)]
        struct Probe {
            #[allow(dead_code)]
            kind: ComponentKind,
        }
        for name in KIND_NAMES {
            assert!(
                toml::from_str::<Probe>(&format!("kind = \"{name}\"")).is_ok(),
                "{name}"
            );
        }
        // Lockstep with the enum: serde's own "expected one of" list, which
        // enumerates every variant, must be exactly KIND_NAMES.
        let err = toml::from_str::<Probe>("kind = \"__not_a_kind__\"")
            .expect_err("sentinel kind must not deserialize")
            .to_string();
        let listed: Vec<&str> = err
            .split("expected one of ")
            .nth(1)
            .expect("serde names the variants")
            .split(',')
            .map(|t| t.trim().trim_matches(|c| c == '`' || c == '.'))
            .filter(|t| !t.is_empty())
            .collect();
        assert_eq!(listed, KIND_NAMES, "KIND_NAMES drifted from ComponentKind");
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
