//! Physical-range validation for model parameters. Used by the extraction
//! pipeline to reject LLM-generated model entries whose parameters are missing or
//! out of physical bounds before they are saved to the user database, so a
//! hallucinated part value cannot silently enter the model library. [`validate`]
//! collects every violation at once rather than stopping at the first.

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

/// Validate a [`ModelEntry`], checking that required params are present and
/// within physical bounds.
///
/// Returns `Ok(())` on success, or a list of violations.
pub fn validate(entry: &ModelEntry) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    let role_exists = |role: &str| {
        entry
            .pins
            .values()
            .any(|known| known.eq_ignore_ascii_case(role))
    };
    for envelope in &entry.envelope {
        if envelope.basis().trim().is_empty() {
            errors.push(ValidationError {
                id: entry.id.clone(),
                message: "operating envelope requires a non-empty basis naming the datasheet table and row"
                    .to_string(),
            });
        }
        match envelope {
            OperatingEnvelope::SupplyRange {
                pin,
                min_v,
                max_v,
                abs_max_v,
                ..
            } => {
                if !role_exists(pin) {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: format!(
                            "operating envelope pin '{pin}' is not a role in [models.pins]"
                        ),
                    });
                }
                if !min_v.is_finite() || !max_v.is_finite() || min_v >= max_v {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: format!(
                            "operating envelope requires finite min_v < max_v, got {min_v} and {max_v}"
                        ),
                    });
                }
                if abs_max_v.is_some_and(|value| !value.is_finite() || value < *max_v) {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: format!(
                            "operating envelope abs_max_v must be finite and at least max_v {max_v}"
                        ),
                    });
                }
            }
            OperatingEnvelope::RailOrder { lower, upper, .. } => {
                for (field, role) in [("lower", lower), ("upper", upper)] {
                    if !role_exists(role) {
                        errors.push(ValidationError {
                            id: entry.id.clone(),
                            message: format!(
                                "operating envelope {field} role '{role}' is not a role in [models.pins]"
                            ),
                        });
                    }
                }
                if lower.eq_ignore_ascii_case(upper) {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: "operating envelope rail_order lower and upper roles must differ"
                            .to_string(),
                    });
                }
            }
        }
    }

    // An identity-only card is useful provenance, but it is deliberately not a
    // simulation model. Keep that state explicit and machine-checkable instead
    // of letting an empty `digital` card count as bound coverage. The engine
    // leaves these parts OPEN while retaining the winning model id/source.
    let identity_only = entry.params.get_bool("identity_only").unwrap_or(false);
    if entry.params.0.contains_key("identity_only")
        && entry.params.get_bool("identity_only").is_none()
    {
        errors.push(ValidationError {
            id: entry.id.clone(),
            message: "params.identity_only must be a boolean".to_string(),
        });
    }
    if identity_only {
        for key in ["warning", "unlocked_by"] {
            if entry
                .params
                .get_str(key)
                .is_none_or(|value| value.trim().is_empty())
            {
                errors.push(ValidationError {
                    id: entry.id.clone(),
                    message: format!(
                        "identity-only model requires non-empty params.{key} so reports state both the limitation and what would unlock behavior"
                    ),
                });
            }
        }
        if !entry.logic.is_empty()
            || !entry.behavioral.is_empty()
            || entry.current_program.is_some()
            || entry.peripheral.is_some()
            || entry.peripheral_power.is_some()
        {
            errors.push(ValidationError {
                id: entry.id.clone(),
                message: "identity-only model cannot also declare logic, behavioral physics, current_program, a firmware peripheral, or peripheral power; remove identity_only only after that behavior is validated"
                    .to_string(),
            });
        }
        if !entry.coverage.implements.is_empty() {
            errors.push(ValidationError {
                id: entry.id.clone(),
                message: "identity-only model cannot declare coverage.implements; it has no executable behavior"
                    .to_string(),
            });
        }
    }

    if let Some(peripheral) = &entry.peripheral {
        match peripheral {
            PeripheralSpec::I2cEeprom {
                address,
                size_bytes,
                page_size,
                word_address_bytes,
            } => {
                if *address > 0x7f {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: format!(
                            "peripheral I2C address 0x{address:02x} is not a 7-bit address"
                        ),
                    });
                }
                if *size_bytes == 0 {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: "peripheral I2C EEPROM size_bytes must be positive".into(),
                    });
                }
                if !page_size.is_power_of_two() || *page_size > *size_bytes {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: format!(
                            "peripheral I2C EEPROM page_size {page_size} must be a power of two no larger than size_bytes {size_bytes}"
                        ),
                    });
                }
                if !matches!(word_address_bytes, 1 | 2) {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: format!(
                            "peripheral I2C EEPROM word_address_bytes must be 1 or 2, got {word_address_bytes}"
                        ),
                    });
                }
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
                    if value == 0 {
                        errors.push(ValidationError {
                            id: entry.id.clone(),
                            message: format!("peripheral SPI NOR {name} must be positive"),
                        });
                    }
                }
                if !page_size.is_power_of_two()
                    || !sector_size.is_power_of_two()
                    || (*size_bytes > 0 && (*page_size > *size_bytes || *sector_size > *size_bytes))
                {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: "peripheral SPI NOR page_size and sector_size must be power-of-two regions no larger than the array".into(),
                    });
                }
                if jedec_id.len() != 3 {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: format!(
                            "peripheral SPI NOR jedec_id must contain exactly 3 bytes, got {}",
                            jedec_id.len()
                        ),
                    });
                }
                if *spi_mode > 3 {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: format!(
                            "peripheral SPI NOR spi_mode must be 0..3, got {spi_mode}"
                        ),
                    });
                }
                for (name, role) in [
                    ("cs_role", cs_role),
                    ("clk_role", clk_role),
                    ("mosi_role", mosi_role),
                    ("miso_role", miso_role),
                ] {
                    if !entry.pins.values().any(|pin_role| pin_role == role) {
                        errors.push(ValidationError {
                            id: entry.id.clone(),
                            message: format!(
                                "peripheral SPI NOR {name} '{role}' is not a role in [models.pins]"
                            ),
                        });
                    }
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
            } => match SensorSpec::from_toml(spec_toml) {
                Err(error) => errors.push(ValidationError {
                    id: entry.id.clone(),
                    message: format!(
                        "peripheral register-map spec_toml must be a valid [sensor] document: {error}"
                    ),
                }),
                Ok(sensor) => {
                    if controller.as_ref().is_some_and(|name| name.trim().is_empty()) {
                        errors.push(ValidationError {
                            id: entry.id.clone(),
                            message: "peripheral register-map controller must not be empty".into(),
                        });
                    }
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
                        if role.trim().is_empty()
                            || !entry.pins.values().any(|pin_role| pin_role == *role)
                        {
                            errors.push(ValidationError {
                                id: entry.id.clone(),
                                message: format!(
                                    "peripheral register-map {name} '{role}' is not a role in [models.pins]"
                                ),
                            });
                        }
                    }
                    for (level, role) in required_high_roles
                        .iter()
                        .map(|role| ("high", role))
                        .chain(required_low_roles.iter().map(|role| ("low", role)))
                    {
                        if role.trim().is_empty()
                            || !entry.pins.values().any(|pin_role| pin_role == role)
                        {
                            errors.push(ValidationError {
                                id: entry.id.clone(),
                                message: format!(
                                    "peripheral register-map required-{level} role '{role}' is not a role in [models.pins]"
                                ),
                            });
                        }
                    }
                    for role in required_high_roles {
                        if required_low_roles.contains(role) {
                            errors.push(ValidationError {
                                id: entry.id.clone(),
                                message: format!(
                                    "peripheral register-map role '{role}' cannot be required both high and low"
                                ),
                            });
                        }
                    }

                    let address_fields = [
                        address_select_role.is_some(),
                        address_when_low.is_some(),
                        address_when_high.is_some(),
                    ];
                    if address_fields.iter().any(|present| *present)
                        && !address_fields.iter().all(|present| *present)
                    {
                        errors.push(ValidationError {
                            id: entry.id.clone(),
                            message: "peripheral register-map address selection requires address_select_role, address_when_low, and address_when_high together".into(),
                        });
                    } else if let (Some(role), Some(low), Some(high)) =
                        (address_select_role, address_when_low, address_when_high)
                    {
                        if sensor.sensor.bus != Bus::I2c {
                            errors.push(ValidationError {
                                id: entry.id.clone(),
                                message: "peripheral register-map address selection is only valid for I2C"
                                    .into(),
                            });
                        }
                        if role.trim().is_empty()
                            || !entry.pins.values().any(|pin_role| pin_role == role)
                        {
                            errors.push(ValidationError {
                                id: entry.id.clone(),
                                message: format!(
                                    "peripheral register-map address-select role '{role}' is not a role in [models.pins]"
                                ),
                            });
                        }
                        if *low > 0x7f || *high > 0x7f || low == high {
                            errors.push(ValidationError {
                                id: entry.id.clone(),
                                message: format!(
                                    "peripheral register-map address strap must select two distinct 7-bit I2C addresses, got 0x{low:02x}/0x{high:02x}"
                                ),
                            });
                        }
                    }
                }
            },
        }
    }

    if let Some(power) = &entry.peripheral_power {
        if entry.peripheral.is_none() {
            errors.push(ValidationError {
                id: entry.id.clone(),
                message: "peripheral_power requires a [models.peripheral] protocol model".into(),
            });
        }
        for (field, role) in [
            ("supply_role", power.supply_role.as_str()),
            ("return_role", power.return_role.as_str()),
        ] {
            if role.trim().is_empty() {
                errors.push(ValidationError {
                    id: entry.id.clone(),
                    message: format!("peripheral_power {field} must not be empty"),
                });
            } else if !entry.pins.values().any(|pin_role| pin_role == role) {
                errors.push(ValidationError {
                    id: entry.id.clone(),
                    message: format!(
                        "peripheral_power {field} '{role}' is not a role in [models.pins]"
                    ),
                });
            }
        }
        if !power.power_on_threshold_v.is_finite() || power.power_on_threshold_v <= 0.0 {
            errors.push(ValidationError {
                id: entry.id.clone(),
                message: "peripheral_power power_on_threshold_v must be finite and positive".into(),
            });
        }
        for (field, value) in [
            ("idle_a", power.idle_a),
            ("read_a", power.read_a),
            ("write_a", power.write_a),
        ] {
            if !value.is_finite() || value < 0.0 {
                errors.push(ValidationError {
                    id: entry.id.clone(),
                    message: format!("peripheral_power {field} must be finite and non-negative"),
                });
            }
        }
        if power
            .low_power_a
            .is_some_and(|value| !value.is_finite() || value < 0.0)
        {
            errors.push(ValidationError {
                id: entry.id.clone(),
                message: "peripheral_power low_power_a must be finite and non-negative".into(),
            });
        }
    }

    // A behavior capability is an API key consumed by coverage requirements,
    // not a marketing sentence. Keep the vocabulary stable enough for exact
    // matching, reject duplicates, and prevent the same capability being both
    // implemented and missing.
    let valid_capability = |value: &str| {
        !value.is_empty()
            && value
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
    };
    let mut implemented = std::collections::HashSet::new();
    for capability in &entry.coverage.implements {
        if !valid_capability(capability) {
            errors.push(ValidationError {
                id: entry.id.clone(),
                message: format!(
                    "coverage capability '{capability}' must use lowercase ASCII letters, digits, and underscores"
                ),
            });
        } else if !implemented.insert(capability.as_str()) {
            errors.push(ValidationError {
                id: entry.id.clone(),
                message: format!("coverage.implements repeats '{capability}'"),
            });
        }
    }
    let mut missing = std::collections::HashSet::new();
    for capability in &entry.coverage.missing {
        if !valid_capability(capability) {
            errors.push(ValidationError {
                id: entry.id.clone(),
                message: format!(
                    "coverage capability '{capability}' must use lowercase ASCII letters, digits, and underscores"
                ),
            });
        } else if !missing.insert(capability.as_str()) {
            errors.push(ValidationError {
                id: entry.id.clone(),
                message: format!("coverage.missing repeats '{capability}'"),
            });
        } else if implemented.contains(capability.as_str()) {
            errors.push(ValidationError {
                id: entry.id.clone(),
                message: format!(
                    "coverage capability '{capability}' cannot be both implemented and missing"
                ),
            });
        }
    }

    if entry.params.0.contains_key("must_not_float_roles") {
        match entry.params.get_str("must_not_float_roles") {
            None => errors.push(ValidationError {
                id: entry.id.clone(),
                message: "params.must_not_float_roles must be a comma-separated string of [models.pins] roles"
                    .to_string(),
            }),
            Some(raw) => {
                let roles: Vec<&str> = raw
                    .split(',')
                    .map(str::trim)
                    .filter(|role| !role.is_empty())
                    .collect();
                if roles.is_empty() {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: "params.must_not_float_roles names no roles".to_string(),
                    });
                }
                let mut seen = std::collections::HashSet::new();
                for role in roles {
                    let normalized = role.to_ascii_lowercase();
                    if !seen.insert(normalized) {
                        errors.push(ValidationError {
                            id: entry.id.clone(),
                            message: format!(
                                "params.must_not_float_roles repeats role '{role}'"
                            ),
                        });
                    } else if !entry
                        .pins
                        .values()
                        .any(|known| known.eq_ignore_ascii_case(role))
                    {
                        errors.push(ValidationError {
                            id: entry.id.clone(),
                            message: format!(
                                "params.must_not_float_roles entry '{role}' is not a role in [models.pins]"
                            ),
                        });
                    }
                }
            }
        }
    }

    macro_rules! require_f64 {
        ($key:expr) => {
            if entry.params.get_f64($key).is_none() {
                errors.push(ValidationError {
                    id: entry.id.clone(),
                    message: format!("missing required param '{}'", $key),
                });
            }
        };
    }

    macro_rules! check_range {
        ($key:expr, $min:expr, $max:expr) => {
            if let Some(v) = entry.params.get_f64($key) {
                // A non-finite value (NaN / ±inf) slips through `v < min || v > max`
                // because every IEEE comparison against NaN is false, NaN is
                // neither below-min nor above-max, so it must be rejected up front
                // or a `nan`/`inf` TOML literal defeats the whole physical-bounds
                // gate and propagates into the solver.
                if !v.is_finite() || v < $min || v > $max {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: format!(
                            "param '{}' = {} is outside physical range [{}, {}]",
                            $key, v, $min, $max
                        ),
                    });
                }
            }
        };
    }

    /// Like [`check_range`], but on the MAGNITUDE, so a rail below ground is
    /// judged by how big it is rather than rejected for its sign. NaN and the
    /// infinities are still refused, for the same reason as above.
    macro_rules! check_signed_range {
        ($key:expr, $min:expr, $max:expr) => {
            if let Some(v) = entry.params.get_f64($key) {
                if !v.is_finite() || v.abs() < $min || v.abs() > $max {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: format!(
                            "param '{}' = {} is outside physical range (magnitude {} to {}, \
                             either sign)",
                            $key, v, $min, $max
                        ),
                    });
                }
            }
        };
    }

    // Require `$lo` < `$hi` when both are present. A swapped/degenerate pair
    // (e.g. an opamp with rail_lo=5, rail_hi=0) is otherwise accepted as valid
    // and gives the solver an empty/inverted saturation band, silently pinning
    // the output. Only checked when both parse; the require_f64! calls report a
    // missing member on their own.
    macro_rules! check_order {
        ($lo:expr, $hi:expr) => {
            if let (Some(lo), Some(hi)) = (entry.params.get_f64($lo), entry.params.get_f64($hi)) {
                if lo >= hi {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: format!(
                            "param '{}' = {} must be strictly less than '{}' = {}",
                            $lo, lo, $hi, hi
                        ),
                    });
                }
            }
        };
    }

    // Identity-only entries name the intended part class, but intentionally do
    // not satisfy that class's solver requirements. Requiring fake vout/gain/
    // diode constants merely to pass lint would recreate the false coverage
    // this state exists to prevent.
    if !identity_only {
        match entry.kind {
            ComponentKind::Diode => {
                require_f64!("is");
                require_f64!("n");
                require_f64!("rs");
                check_range!("is", 1e-20, 1e-3);
                check_range!("n", 0.5, 3.0);
                check_range!("rs", 0.0, 1000.0);
                check_range!("cjo", 0.0, 1e-6);
            }
            ComponentKind::BjtNpn | ComponentKind::BjtPnp => {
                require_f64!("is");
                require_f64!("bf");
                require_f64!("nf");
                require_f64!("vaf");
                check_range!("is", 1e-20, 1e-3);
                check_range!("bf", 1.0, 2000.0);
                check_range!("nf", 0.5, 3.0);
                check_range!("vaf", 1.0, 500.0);
                check_range!("rb", 0.0, 1e6);
                check_range!("rc", 0.0, 1e6);
                check_range!("re", 0.0, 1e6);
            }
            ComponentKind::Nmos | ComponentKind::Pmos => {
                require_f64!("vto");
                require_f64!("kp");
                check_range!("vto", -10.0, 10.0);
                // kp = k'·(W/L) for the level-1 SPICE model. For a discrete POWER
                // MOSFET the effective W/L is enormous, so kp legitimately runs into
                // the tens or hundreds (the repo's own datasheet-cited db/mosfet.toml
                // has kp up to 200 for ipa045n10n3g). A 1.0 A/V² ceiling false-flagged
                // 6 of 8 shipped models and rejected any correctly-extracted power
                // FET. Bound generously, still catches a nonsense hallucination.
                check_range!("kp", 1e-6, 1000.0);
                check_range!("lambda", 0.0, 1.0);
            }
            ComponentKind::Vreg => {
                // A declarative converter owns its output, input draw and
                // regulation semantics; `bind_vreg` deliberately does not read
                // the simple-LDO `vout/dropout_v/iq_a` tuple in that path.
                // Requiring authors to invent those unused numbers made a valid
                // adjustable buck impossible to lint without false facts.
                if entry.behavioral.converter.is_none() {
                    require_f64!("vout");
                    require_f64!("dropout_v");
                    require_f64!("iq_a");
                    // Magnitude, not value: a negative rail is a real regulator.
                    // The 79xx family and every dual-supply analog board regulate
                    // BELOW ground, and `vout` is stamped as a DC source against
                    // ground, so the sign carries the whole meaning.
                    check_signed_range!("vout", 0.5, 30.0);
                    check_range!("dropout_v", 0.0, 10.0);
                    check_range!("iq_a", 0.0, 1.0);
                }
            }
            ComponentKind::Opamp => {
                require_f64!("gain");
                require_f64!("rail_lo");
                require_f64!("rail_hi");
                check_range!("gain", 1.0, 1e9);
                check_range!("rail_lo", -60.0, 60.0);
                check_range!("rail_hi", -60.0, 60.0);
                check_order!("rail_lo", "rail_hi");
            }
            ComponentKind::Comparator => {
                require_f64!("out_lo");
                require_f64!("out_hi");
                require_f64!("hysteresis");
                check_range!("hysteresis", 0.0, 5.0);
                check_range!("out_lo", -60.0, 60.0);
                check_range!("out_hi", -60.0, 60.0);
                check_order!("out_lo", "out_hi");
            }
            ComponentKind::AnalogSwitch => {
                require_f64!("ron");
                require_f64!("roff");
                check_range!("ron", 0.01, 10_000.0);
                check_range!("roff", 1e3, 1e12);
                // On-resistance must be far below off-resistance; the two ranges
                // overlap ([0.01,1e4] vs [1e3,1e12]), so a swapped/degenerate pair
                // (ron=5000, roff=2000) is representable and would model a switch that
                // conducts MORE when open, an inverted transmission gate the solver
                // routes the wrong way. Same hazard the R35 opamp/comparator order
                // checks close.
                check_order!("ron", "roff");
            }
            // Digital / MCU / connector / ignore: no mandatory numeric params
            _ => {}
        }
    }

    // Identity-only cards deliberately stamp no circuit or firmware behavior,
    // so requiring a complete pin map here pressures authors to fabricate one
    // merely to record an exact identity. The board-observed pins remain in
    // `models coverage` / `models prepare` inventory.json, and the engine keeps
    // the part OPEN until an executable card supplies the roles it needs.
    if !identity_only {
        check_required_pins(entry, &mut errors);
        check_behavioral_series_path_roles(entry, &mut errors);
    }

    // Absolute-maximum ratings gate the engine's stress/destruction faults. A
    // NaN, negative, or zero rating passes every kind-specific check above (which
    // only look at `params`, never `ratings`), then silently disables the fault:
    // the stress monitor computes `if limit > 0.0 { value/limit } else { 0.0 }`,
    // so a NaN (NaN>0 is false) or non-positive limit yields frac 0 and the
    // Overcurrent/Overvoltage/Overpower check never trips, an unprotected part
    // that validated clean. Reject any present rating that is not positive-finite.
    for (name, rating) in [
        ("max_current_a", entry.ratings.max_current_a),
        ("max_surge_current_a", entry.ratings.max_surge_current_a),
        ("max_power_w", entry.ratings.max_power_w),
        ("max_voltage_v", entry.ratings.max_voltage_v),
        ("max_pin_current_a", entry.ratings.max_pin_current_a),
        ("max_ripple_current_a", entry.ratings.max_ripple_current_a),
        ("max_junction_temp_c", entry.ratings.max_junction_temp_c),
        // The thermal resistances are solver-facing and UNFLOORED: thermal.rs
        // computes `Tj = ambient + power.max(0)*theta_ja`, so a negative/NaN
        // theta drives Tj at or below ambient and the Overtemperature fault never
        // trips (frac.max(0) = 0). Gate them like the other ratings (R52 missed
        // these two).
        ("theta_ja_c_per_w", entry.ratings.theta_ja_c_per_w),
        ("theta_jc_c_per_w", entry.ratings.theta_jc_c_per_w),
    ] {
        if let Some(v) = rating {
            if !v.is_finite() || v <= 0.0 {
                errors.push(ValidationError {
                    id: entry.id.clone(),
                    message: format!("rating '{name}' = {v} must be a positive finite number"),
                });
            }
        }
    }

    // Board-programmed current is solver-facing physics just like `params`:
    // malformed constants can silently turn a real rail current into zero/NaN,
    // while confusing a normal-operating ceiling with a device-level safety
    // threshold makes the part promise operation in a region the datasheet does
    // not specify as normal.
    if let Some(program) = &entry.current_program {
        let role_exists = !program.pin.trim().is_empty()
            && entry
                .pins
                .values()
                .any(|role| role.eq_ignore_ascii_case(program.pin.trim()));
        if !role_exists {
            errors.push(ValidationError {
                id: entry.id.clone(),
                message: format!(
                    "current_program.pin '{}' is not a role in [models.pins]",
                    program.pin
                ),
            });
        }

        for (field, roles) in [
            ("current_in_roles", &program.current_in_roles),
            ("current_out_roles", &program.current_out_roles),
        ] {
            if program.semantics == CurrentProgramSemantics::RegulatedCurrent && roles.is_empty() {
                errors.push(ValidationError {
                    id: entry.id.clone(),
                    message: format!(
                        "current_program regulated_current requires non-empty {field}"
                    ),
                });
            }
            let mut seen = std::collections::HashSet::new();
            for role in roles {
                let normalized = role.trim().to_ascii_lowercase();
                if normalized.is_empty()
                    || !entry
                        .pins
                        .values()
                        .any(|known| known.eq_ignore_ascii_case(role.trim()))
                {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: format!(
                            "current_program.{field} entry '{role}' is not a role in [models.pins]"
                        ),
                    });
                } else if !seen.insert(normalized) {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: format!("current_program.{field} repeats role '{role}'"),
                    });
                }
            }
        }
        for input in &program.current_in_roles {
            if program
                .current_out_roles
                .iter()
                .any(|output| output.eq_ignore_ascii_case(input))
            {
                errors.push(ValidationError {
                    id: entry.id.clone(),
                    message: format!(
                        "current_program role '{input}' appears in both current_in_roles and current_out_roles"
                    ),
                });
            }
        }

        if program.semantics == CurrentProgramSemantics::RegulatedCurrent
            && program.max_operating_current_a.is_none()
        {
            errors.push(ValidationError {
                id: entry.id.clone(),
                message: "current_program regulated_current requires max_operating_current_a so an undersized programming resistor cannot imply operation beyond the sourced domain".into(),
            });
        }
        if program.above_domain == AboveDomainBehavior::Saturate
            && program.max_operating_current_a.is_none()
        {
            errors.push(ValidationError {
                id: entry.id.clone(),
                message: "current_program above_domain = saturate requires max_operating_current_a as the sourced saturation value"
                    .into(),
            });
        }

        if let Some(limit) = program.max_operating_current_a {
            if !limit.is_finite() || limit <= 0.0 {
                errors.push(ValidationError {
                    id: entry.id.clone(),
                    message: format!(
                        "current_program.max_operating_current_a = {limit} must be a positive finite number"
                    ),
                });
            }
            // The programmed quantity is the part's rail/load current. A
            // generic per-pin source/sink limit applies to the PROG/control pin
            // itself and is not a bound on that independently controlled rail.
            if let Some(device_limit) = entry
                .ratings
                .max_current_a
                .filter(|value| value.is_finite() && *value > 0.0)
            {
                if limit.is_finite() && limit > device_limit {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: format!(
                            "current_program.max_operating_current_a = {limit} A exceeds ratings.max_current_a = {device_limit} A"
                        ),
                    });
                }
            }
        }

        let mut check_positive = |name: &str, value: f64| {
            if !value.is_finite() || value <= 0.0 {
                errors.push(ValidationError {
                    id: entry.id.clone(),
                    message: format!(
                        "current_program.{name} = {value} must be a positive finite number"
                    ),
                });
                false
            } else {
                true
            }
        };

        match &program.equation {
            CurrentProgramEquation::InverseResistance { k_volts } => {
                check_positive("k_volts", *k_volts);
            }
            CurrentProgramEquation::PowerLawResistance {
                coefficient_a,
                resistance_scale_ohms,
                exponent,
            } => {
                for (name, value) in [
                    ("coefficient_a", *coefficient_a),
                    ("resistance_scale_ohms", *resistance_scale_ohms),
                    ("exponent", *exponent),
                ] {
                    check_positive(name, value);
                }
            }
            CurrentProgramEquation::PiecewiseInverseResistance {
                low_k_volts,
                transition_current_a,
                high_numerator_a,
                resistance_scale_ohms,
                high_offset,
            } => {
                let constants_valid = [
                    ("low_k_volts", *low_k_volts),
                    ("transition_current_a", *transition_current_a),
                    ("high_numerator_a", *high_numerator_a),
                    ("resistance_scale_ohms", *resistance_scale_ohms),
                    ("high_offset", *high_offset),
                ]
                .into_iter()
                .all(|(name, value)| check_positive(name, value));

                if constants_valid {
                    let transition_resistance_ohms = *low_k_volts / *transition_current_a;
                    let high_at_transition = *high_numerator_a
                        / (transition_resistance_ohms / *resistance_scale_ohms + *high_offset);
                    let relative_gap =
                        (high_at_transition - *transition_current_a).abs() / *transition_current_a;
                    if relative_gap > 0.01 {
                        errors.push(ValidationError {
                            id: entry.id.clone(),
                            message: format!(
                                "current_program piecewise branches are not continuous at {transition_current_a} A (high branch gives {high_at_transition} A)"
                            ),
                        });
                    }
                }
            }
            CurrentProgramEquation::SenseScaledResistance {
                sense_roles,
                sense_far_roles,
                program_bias_a,
                program_full_scale_v,
                sense_full_scale_v,
            } => {
                for (name, value) in [
                    ("program_bias_a", *program_bias_a),
                    ("program_full_scale_v", *program_full_scale_v),
                    ("sense_full_scale_v", *sense_full_scale_v),
                ] {
                    check_positive(name, value);
                }
                if sense_roles.is_empty() {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: "current_program.sense_roles must name at least one role"
                            .to_string(),
                    });
                }
                let mut normalized_roles = std::collections::HashSet::new();
                for role in sense_roles {
                    if role.trim().is_empty()
                        || !entry
                            .pins
                            .values()
                            .any(|known| known.eq_ignore_ascii_case(role.trim()))
                    {
                        errors.push(ValidationError {
                            id: entry.id.clone(),
                            message: format!(
                                "current_program.sense_roles entry '{role}' is not a role in [models.pins]"
                            ),
                        });
                    }
                    if !normalized_roles.insert(role.trim().to_ascii_lowercase()) {
                        errors.push(ValidationError {
                            id: entry.id.clone(),
                            message: format!("current_program.sense_roles repeats role '{role}'"),
                        });
                    }
                }
                if sense_far_roles.len() != sense_roles.len() {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: format!(
                            "current_program.sense_far_roles has {} entries but sense_roles has {}",
                            sense_far_roles.len(),
                            sense_roles.len()
                        ),
                    });
                }
                for role in sense_far_roles {
                    if !role.eq_ignore_ascii_case("ground")
                        && !entry
                            .pins
                            .values()
                            .any(|known| known.eq_ignore_ascii_case(role.trim()))
                    {
                        errors.push(ValidationError {
                            id: entry.id.clone(),
                            message: format!(
                                "current_program.sense_far_roles entry '{role}' is neither 'ground' nor a role in [models.pins]"
                            ),
                        });
                    }
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// A state-controlled series path is solver-facing connectivity: both ends
/// must name roles the entry actually maps from physical pins. A typo here
/// otherwise validates, then the runtime silently skips the path and the part
/// that was supposed to close a rail remains open.
fn check_behavioral_series_path_roles(entry: &ModelEntry, errors: &mut Vec<ValidationError>) {
    let declared = entry
        .pins
        .values()
        .map(|role| role.to_ascii_lowercase())
        .collect::<std::collections::HashSet<_>>();
    for (index, path) in entry.behavioral.series_paths.iter().enumerate() {
        for (end, role) in [("a", &path.a), ("b", &path.b)] {
            if !declared.contains(&role.to_ascii_lowercase()) {
                errors.push(ValidationError {
                    id: entry.id.clone(),
                    message: format!(
                        "behavioral.series_paths[{index}].{end} role '{role}' is not present in [models.pins]"
                    ),
                });
            }
        }
    }
    for (index, load) in entry.behavioral.profiled_loads.iter().enumerate() {
        if !declared.contains(&load.supply_pin.to_ascii_lowercase()) {
            errors.push(ValidationError {
                id: entry.id.clone(),
                message: format!(
                    "behavioral.profiled_loads[{index}].supply_pin role '{}' is not present in [models.pins]",
                    load.supply_pin
                ),
            });
        }
        if let Some(role) = &load.return_pin {
            if !declared.contains(&role.to_ascii_lowercase()) {
                errors.push(ValidationError {
                    id: entry.id.clone(),
                    message: format!(
                        "behavioral.profiled_loads[{index}].return_pin role '{role}' is not present in [models.pins]"
                    ),
                });
            }
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
fn check_required_pins(entry: &ModelEntry, errors: &mut Vec<ValidationError>) {
    if entry.kind == ComponentKind::Digital && entry.pins.is_empty() {
        errors.push(ValidationError {
            id: entry.id.clone(),
            message: "digital model has no [models.pins]; without declared roles it can bind cleanly while driving and observing nothing".to_string(),
        });
        return;
    }
    if entry.pins.is_empty() {
        return;
    }
    // A behavioral part (converter/FSM/DAC power IC) references its pins from the
    // [models.behavioral] block by arbitrary datasheet names (e.g. an LTC4020's
    // `bat`/`pvin`), NOT through the simple analog binder, so the canonical
    // anchor roles do not apply. Leave it alone.
    if !entry.behavioral.is_empty() {
        return;
    }
    // Each inner slice is one required role; the model satisfies it by mapping
    // some pin to ANY name in the slice, after normalization. Names are the
    // binder's accepted aliases (see bind_diode/bjt/mosfet/vreg/opamp/comparator
    // in hauksbee-engine). analog_switch is deliberately EXCLUDED: it binds
    // SPST (`in_out_a`/`in_out_b`) and SPDT (`com`/`s0`/`s1`) forms with too
    // varied a vocabulary to anchor-check without false positives.
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
        // Kinds whose pin vocabulary is open or handled elsewhere (analog_switch,
        // digital, mcu, dac, adc, shift_register, connector, passive, ignore).
        _ => return,
    };

    // Normalize each declared role: lowercase, strip a trailing channel suffix
    // (`_a`..`_d` or `_q<N>`), then a trailing digit run + underscore. This
    // folds the binder's channel variants onto the base role: `out_1`->`out`,
    // `d1`/`d2`->`d` (dual MOSFET), `collector_q2`->`collector`.
    let normalize = |role: &str| -> String {
        let mut r = role.to_ascii_lowercase();
        for sfx in ["_a", "_b", "_c", "_d"] {
            if let Some(base) = r.strip_suffix(sfx) {
                r = base.to_string();
                break;
            }
        }
        if let Some(idx) = r.rfind("_q") {
            if idx + 2 < r.len() && r[idx + 2..].chars().all(|c| c.is_ascii_digit()) {
                r = r[..idx].to_string();
            }
        }
        r = r.trim_end_matches(|c: char| c.is_ascii_digit()).to_string();
        r.trim_end_matches('_').to_string()
    };
    let declared: std::collections::HashSet<String> =
        entry.pins.values().map(|role| normalize(role)).collect();

    for role_family in required {
        if !role_family.iter().any(|name| declared.contains(*name)) {
            errors.push(ValidationError {
                id: entry.id.clone(),
                message: format!(
                    "[models.pins] declares no '{}' pin (a {:?} needs it); \
                     the part would bind OPEN. Accepted role names: {}",
                    role_family[0],
                    entry.kind,
                    role_family.join(" / ")
                ),
            });
        }
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
        | "pmic" | "charger" => "vreg",
        "npn" | "bjt" | "transistor" => "bjt_npn",
        "pnp" => "bjt_pnp",
        "mosfet" | "fet" | "nfet" | "n_mosfet" | "nmosfet" => "nmos",
        "pfet" | "p_mosfet" | "pmosfet" => "pmos",
        "op_amp" | "operational_amplifier" | "amplifier" => "opamp",
        "resistor" | "capacitor" | "inductor" | "res" | "cap" | "ferrite" | "crystal" => "passive",
        "led" | "zener" | "schottky" | "rectifier" | "tvs" => "diode",
        "switch" | "mux" | "multiplexer" => "analog_switch",
        "microcontroller" | "micro" | "soc" => "mcu",
        "header" | "jack" | "socket" | "plug" => "connector",
        "logic" | "gate" | "flip_flop" | "latch" => "digital",
        _ => "",
    };
    if !alias.is_empty() {
        return Some(alias);
    }
    KIND_NAMES
        .iter()
        .map(|k| (levenshtein(&lower, k), *k))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| k)
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
    match kind_suggestion(unknown) {
        Some(s) => Some(format!("unknown kind '{unknown}': did you mean '{s}'?")),
        None => Some(format!(
            "unknown kind '{unknown}'; valid kinds: {}",
            KIND_NAMES.join(", ")
        )),
    }
}

/// Iterative Levenshtein edit distance (short vocabulary strings).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        AboveDomainBehavior, ComponentKind, CurrentProgramEquation, ModelEntry, Params,
        PeripheralPower, PeripheralSpec,
    };
    use std::collections::BTreeMap;

    fn entry(kind: ComponentKind, params: &[(&str, f64)]) -> ModelEntry {
        let mut p = Params::default();
        for (k, v) in params {
            p.set_f64(*k, *v);
        }
        ModelEntry {
            id: "t".into(),
            kind,
            description: String::new(),
            r#match: Default::default(),
            params: p,
            pins: BTreeMap::new(),
            envelope: Default::default(),
            ratings: Default::default(),
            straps: Vec::new(),
            behavioral: Default::default(),
            logic: Default::default(),
            current_program: None,
            peripheral: None,
            peripheral_power: None,
            coverage: Default::default(),
            passive_class: None,
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
        #[derive(serde::Deserialize)]
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
