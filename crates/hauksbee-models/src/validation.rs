//! Physical-range validation for model parameters.
//!
//! Used by the extraction pipeline to reject LLM-generated model entries whose
//! parameters are missing or out of physical bounds before they are saved to
//! the user database, so a hallucinated part value cannot silently enter the
//! model library. [`validate`] collects every violation at once rather than
//! stopping at the first.

use std::collections::{BTreeMap, HashSet};

use crate::check::{nonneg_finite, positive_finite, Problems};
use crate::derating::temp_span;
use crate::schema::{
    AboveDomainBehavior, Characteristic, ComponentKind, CurrentProgramEquation,
    CurrentProgramSemantics, ModelEntry, OperatingEnvelope, OutputLevel, ParamValue, Params, PeripheralSpec,
    Pinout,
};
use crate::sensor_spec::{Bus, SensorSpec};
use hauksbee_ir::evidence::ModelUncertainty;
use hauksbee_ir::units::format_amps;
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
    // A datasheet RECOMMENDATION ("22 µF tantalum will ensure stability") is
    // a sufficient value, not the stated floor `cout_min_f` records.
    if params.0.contains_key("cout_recommended_f") {
        let rec = params
            .get_f64("cout_recommended_f")
            .filter(|f| positive_finite(*f));
        p.require(rec.is_some(), || {
            "param 'cout_recommended_f' must be a positive finite number".into()
        });
        if let (Some(rec), Some(min)) = (rec, params.get_f64("cout_min_f")) {
            p.require(min <= rec, || {
                format!(
                    "param 'cout_min_f' = {min} must be less than or equal to \
                     'cout_recommended_f' = {rec}"
                )
            });
        }
    }
    if params.0.contains_key("cout_recommended_type") {
        p.require(
            params.0.contains_key("cout_recommended_f")
                && params
                    .get_str("cout_recommended_type")
                    .is_some_and(|t| !t.trim().is_empty()),
            || {
                "param 'cout_recommended_type' must be a non-empty string beside \
                 'cout_recommended_f'"
                    .into()
            },
        );
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

/// Which of `keys` the card's `[models.params]` states, comma-separated.
fn stated_params<'a>(entry: &ModelEntry, keys: impl Iterator<Item = &'a &'a str>) -> String {
    let stated: Vec<&str> = keys
        .copied()
        .filter(|k| entry.params.0.contains_key(*k))
        .collect();
    stated.join(", ")
}

/// The enable-pin fields of a simple (ideal-source) regulator, read by the
/// binder's enable gate. Thresholds come as a pair ordered for the polarity
/// (guaranteed off on the far side of guaranteed on, so the gap between them is
/// the band the gate holds its state across), at most one internal pull, and
/// only on a card whose pin map names the `en` or `shdn` pin they describe.
fn validate_vreg_enable_params(entry: &ModelEntry, p: &mut Problems) {
    const KEYS: [&str; 8] = [
        "en_on_v",
        "en_off_v",
        "en_on_level",
        "en_pull_down_ohms",
        "en_pull_up_ohms",
        "en_pull_down_a",
        "en_pull_up_a",
        "en_discharge_ohms",
    ];
    let params = &entry.params;
    // Named in the errors: a fixer told only "en_*" searched for that prefix,
    // missed `i_shdn_a`, and re-ran the checker three times.
    let stated = stated_params(
        entry,
        KEYS.iter().chain(&["i_shdn_a", "en_open", "en_open_basis"]),
    );
    if entry.kind != ComponentKind::Vreg || stated.is_empty() {
        return;
    }
    p.require(has_role(entry, "en") || has_role(entry, "shdn"), || {
        format!("enable params ({stated}) need an 'en' or 'shdn' role in [models.pins]")
    });
    p.require(entry.behavioral.converter.is_none(), || {
        format!(
            "enable params ({stated}) are the simple regulator's; a converter states \
             [models.behavioral.converter.enable] instead: remove them from [models.params]"
        )
    });
    let level = params.get_str("en_on_level");
    p.require(
        !params.0.contains_key("en_on_level") || matches!(level, Some("high" | "low")),
        || "param 'en_on_level' must be \"high\" or \"low\"".into(),
    );
    let (on, off) = (params.get_f64("en_on_v"), params.get_f64("en_off_v"));
    p.require(on.is_some() == off.is_some(), || {
        "vreg enable thresholds need both 'en_on_v' and 'en_off_v'".into()
    });
    for (key, v) in [("en_on_v", on), ("en_off_v", off)] {
        if let Some(v) = v {
            p.require(v.is_finite() && v.abs() <= 60.0, || {
                format!("param '{key}' = {v} must be a finite voltage within 60 V")
            });
        }
    }
    if let (Some(on), Some(off)) = (on, off) {
        let low = level == Some("low");
        p.require(if low { on <= off } else { off <= on }, || {
            format!(
                "param 'en_off_v' = {off} must sit on the off side of 'en_on_v' = {on} \
                 for an output that turns on {}",
                if low { "low" } else { "high" }
            )
        });
    }
    // The shutdown supply current is what IN draws with the part off, so it
    // sits below the on-state `iq_a` (the binder draws the difference when on).
    if params.0.contains_key("i_shdn_a") {
        let iq = params.get_f64("iq_a");
        p.require(
            params
                .get_f64("i_shdn_a")
                .is_some_and(|i| i.is_finite() && i >= 0.0 && iq.is_some_and(|q| i < q)),
            || "param 'i_shdn_a' must be finite, at least 0 and below 'iq_a'".into(),
        );
    }
    let pulls = KEYS[3..7]
        .iter()
        .filter(|k| params.0.contains_key(**k))
        .count();
    p.require(pulls <= 1, || {
        "a vreg enable pin has at most one internal pull ('en_pull_*')".into()
    });
    validate_en_open(params, level, pulls, p);
    for key in &KEYS[3..] {
        if params.0.contains_key(*key) {
            p.require(params.get_f64(key).is_some_and(positive_finite), || {
                format!("param '{key}' must be a positive finite number")
            });
        }
    }
}

/// The datasheet's stated open-pin state (`en_open`) and its quote
/// (`en_open_basis`), checked against the internal pull the card states.
fn validate_en_open(params: &Params, level: Option<&str>, pulls: usize, p: &mut Problems) {
    // The datasheet's stated open-pin state, for a card that prints no pull
    // value: it may not contradict a pull the card does state (an open pin
    // follows its pull, so a pull toward the off level cannot read as on).
    if params.0.contains_key("en_open") {
        let open = params.get_str("en_open");
        p.require(matches!(open, Some("off" | "on")), || {
            "param 'en_open' must be \"off\" or \"on\"".into()
        });
        let has = |k: &str| params.0.contains_key(k);
        let pull_up = if has("en_pull_up_ohms") || has("en_pull_up_a") {
            Some(true)
        } else if has("en_pull_down_ohms") || has("en_pull_down_a") {
            Some(false)
        } else {
            None
        };
        if let (Some(up), Some(open @ ("off" | "on"))) = (pull_up, open) {
            // A pull-up holds an on-high pin on; a pull-down an on-low pin on.
            let pull_holds_on = up == (level != Some("low"));
            p.require(pull_holds_on == (open == "on"), || {
                format!(
                    "param 'en_open' = \"{open}\" contradicts the card's internal pull-{}",
                    if up { "up" } else { "down" }
                )
            });
        }
    }
    // The quote of the open-pin state backs a stated state: `en_open` or an
    // internal pull, never nothing.
    if params.0.contains_key("en_open_basis") {
        p.require(
            params
                .get_str("en_open_basis")
                .is_some_and(|q| !q.trim().is_empty()),
            || "param 'en_open_basis' must be the datasheet's words, a non-empty string".into(),
        );
        p.require(params.0.contains_key("en_open") || pulls == 1, || {
            "param 'en_open_basis' quotes the open-pin state, so the card needs 'en_open' \
                 or an internal pull ('en_pull_*') for it to back"
                .into()
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

/// Whether an identity-only card's own cited rows give every param its kind
/// requires, or a convention derives it, so the abstention is refused.
pub(crate) fn needless_identity_only(entry: &ModelEntry) -> bool {
    let required = kind_rules(entry).required;
    crate::param_names::needless_abstention(entry, required, |k| abstains(entry, k)).is_some()
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
                ("swing_lo", 0.0, 5.0),
                ("swing_hi", 0.0, 5.0),
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
            ranges: &[
                ("ron", 0.01, 10_000.0),
                ("ron_s0", 0.01, 10_000.0),
                ("ron_s1", 0.01, 10_000.0),
                ("roff", 1e3, 1e12),
            ],
            ordered: &[("ron", "roff"), ("ron_s0", "roff"), ("ron_s1", "roff")],
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

    let mut rows = HashSet::new();
    for envelope in &entry.envelope {
        p.require(!envelope.basis().trim().is_empty(), || {
            "operating envelope requires a non-empty basis naming the datasheet table and row"
                .into()
        });
        let (text, span) = (envelope.conditions(), temp_span(envelope.conditions()));
        let row = format!("{:?} {span:?}", envelope.channel());
        p.require(text.is_empty() || (span.is_some() && rows.insert(row)), || {
            format!("operating envelope conditions '{text}' must name a temperature (TA = 25 °C), once per channel")
        });
        match envelope {
            OperatingEnvelope::SupplyRange {
                pin,
                min_v,
                max_v,
                abs_max_v,
                abs_max_basis,
                ..
            } => {
                p.require(
                    abs_max_basis.trim().is_empty() || abs_max_v.is_some(),
                    || "operating envelope abs_max_basis needs the abs_max_v it cites".into(),
                );
                supplies.extend([*min_v, *max_v, abs_max_v.unwrap_or(0.0)]);
                p.require(has_role_ci(entry, pin), || {
                    not_a_role("operating envelope pin", pin)
                });
                // A sense or measurement input's range is a measurement window,
                // not a rail the board supplies: reading it as a supply range
                // lets a shunt's full scale pose as the part's own supply.
                p.require(!crate::pin_roles::is_sense_input_role(pin), || {
                    format!(
                        "operating envelope supply_range on '{pin}', a sense or measurement \
                         input: a supply_range is for a pin the board supplies. Rate the \
                         input's ground-referenced maximum with an input_tolerance row, or \
                         leave it unrated"
                    )
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
            OperatingEnvelope::InputTolerance {
                pins,
                max_v,
                abs_max_v,
                abs_max_basis,
                ..
            } => {
                p.require(!pins.is_empty(), || {
                    "operating envelope input_tolerance needs at least one input role in pins"
                        .into()
                });
                for role in pins {
                    p.require(has_role_ci(entry, role), || {
                        not_a_role("operating envelope input_tolerance pin", role)
                    });
                    // A driven output (or a bidirectional port, which the logic
                    // block declares as an output too) is rated VCC + 0.5 V
                    // whatever its high-impedance row says.
                    let driven = entry
                        .logic
                        .outputs
                        .iter()
                        .any(|out| out.eq_ignore_ascii_case(role));
                    p.require(!driven, || {
                        format!("operating envelope input_tolerance pin '{role}' is a logic output; only an input in every mode takes a supply-independent rating")
                    });
                }
                p.require(max_v.is_finite() && *max_v > 0.0, || {
                    format!("operating envelope input_tolerance max_v must be a finite positive voltage, got {max_v}")
                });
                p.require(
                    !abs_max_v.is_some_and(|v| !v.is_finite() || v < *max_v),
                    || format!("operating envelope input_tolerance abs_max_v must be finite and at least max_v {max_v}"),
                );
                p.require(
                    abs_max_basis.trim().is_empty() || abs_max_v.is_some(),
                    || "operating envelope abs_max_basis needs the abs_max_v it cites".into(),
                );
            }
        }
    }

    // A mode-qualifying strap names the boot-select strap whose alternate mode
    // it qualifies; a dangling or non-selecting reference would silently turn
    // the float check off.
    for strap in &entry.straps {
        if let Some(sel) = strap.only_with.as_deref() {
            let ok =
                sel != strap.role && entry.straps.iter().any(|s| s.role == sel && s.boot_select);
            p.require(ok, || {
                format!(
                    "strap '{}' only_with '{sel}' must name another boot_select strap on this card",
                    strap.role
                )
            });
        }
    }

    check_pin_capability(entry, &mut p);

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
        p.push("digital model has no executable behavior; add [models.logic] (for example, the builtin 74hc14 card declares inputs, outputs, and [models.logic.comb]); for a driver or power IC whose outputs are not logic, a supply-draw law (params supply_current_a and supply_on_v, and a [[models.behavioral.laws]] current law from the supply role to gnd); or set params.identity_only = true with warning and unlocked_by so binding leaves the part open");
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

    for (key, table) in &entry.levels {
        let ok = ["vih", "vil", "voh", "vol"].contains(&key.as_str()) && table.len() >= 2;
        p.require(ok && table.windows(2).all(|w| w[0][0] < w[1][0]), || {
            format!("[models.levels] '{key}' must be vih, vil, voh or vol, with two or more ascending [vcc, volts] points")
        });
    }
    for out in &entry.supply_outputs {
        let r = &out.role;
        p.require(entry.pins.values().any(|v| v == r), || {
            format!("[[models.supply_outputs]] role '{r}' is not a role in [models.pins]")
        });
        let ordered = out.min.is_finite()
            && out.max.is_finite()
            && out.min <= out.nominal
            && out.nominal <= out.max;
        p.require(ordered, || {
            format!("[[models.supply_outputs]] '{r}' needs finite min <= nominal <= max")
        });
        p.require(!out.basis.trim().is_empty(), || {
            format!("[[models.supply_outputs]] '{r}' needs a basis citing its source path")
        });
    }
    for d in &entry.output_drive {
        let name = &d.setting;
        let sane = d.vdd.is_finite()
            && d.vdd > 0.0
            && !d.points.is_empty()
            && d.points.iter().all(|pt| {
                pt.current_a.is_finite()
                    && pt.current_a > 0.0
                    && pt.volts.is_finite()
                    && (0.0..=d.vdd).contains(&pt.volts)
            });
        p.require(sane, || {
            format!("[[models.output_drive]] '{name}' needs a positive vdd and points with positive currents and volts inside 0..vdd")
        });
        p.require(!d.basis.trim().is_empty(), || {
            format!("[[models.output_drive]] '{name}' needs a basis citing the datasheet table and page")
        });
        let covered = |level| d.disconnected.contains(&level) || d.fit(level).is_some();
        p.require(covered(OutputLevel::High) && covered(OutputLevel::Low), || {
            format!("[[models.output_drive]] '{name}' needs a usable point or a disconnected entry for both high and low")
        });
    }
    if !entry.output_drive.is_empty() {
        let selectors: Vec<u32> = entry.output_drive.iter().filter_map(|d| d.selector).collect();
        let unique = selectors.iter().collect::<std::collections::BTreeSet<_>>().len() == selectors.len();
        p.require(unique, || "[[models.output_drive]] selectors must be unique".to_string());
        p.require(
            selectors.is_empty() || entry.drive_select.is_some(),
            || "[[models.output_drive]] selectors need a [models.drive_select] saying where the field lives".to_string(),
        );
        p.require(
            entry.output_drive.iter().filter(|d| d.reset_default).count() <= 1,
            || "[[models.output_drive]] at most one setting is reset_default".to_string(),
        );
    }
    if let Some(sel) = &entry.drive_select {
        p.require(
            !sel.port_bases.is_empty() && sel.pins_per_register > 0 && (1..=32).contains(&sel.width) && sel.lsb + sel.width <= 32 && !sel.basis.trim().is_empty(),
            || "[models.drive_select] needs port_bases, pins_per_register > 0, a width in 1..=32 inside the word, and a basis".to_string(),
        );
    }
    if let Some(r) = &entry.reset {
        let finite = |v: Option<f64>| v.is_none_or(|v| v.is_finite() && v >= 0.0);
        p.require(
            finite(r.assert_below_v) && finite(r.release_above_v) && finite(r.pin_vil_v) && finite(r.pin_vih_v) && finite(r.release_delay_s),
            || "[models.reset] thresholds must be finite and non-negative".to_string(),
        );
        p.require(
            r.assert_below_v.zip(r.release_above_v).is_none_or(|(a, b)| a <= b)
                && r.pin_vil_v.zip(r.pin_vih_v).is_none_or(|(a, b)| a < b),
            || "[models.reset] needs assert_below_v <= release_above_v and pin_vil_v < pin_vih_v".to_string(),
        );
        p.require(!r.basis.trim().is_empty(), || {
            "[models.reset] needs a basis citing the datasheet tables and pages".to_string()
        });
        if r.assert_below_v.is_some() || r.release_above_v.is_some() || r.release_delay_s.is_some() {
            let ok = r
                .supply_pin
                .as_ref()
                .is_some_and(|role| entry.pins.values().any(|v| v == role));
            p.require(ok, || {
                "[models.reset] supply thresholds need supply_pin naming a role in [models.pins]".to_string()
            });
        }
    }
    for opt in &entry.optional_supplies {
        let r = &opt.role;
        p.require(entry.pins.values().any(|v| v == r), || {
            format!("[[models.optional_supplies]] role '{r}' is not a role in [models.pins]")
        });
        p.require(!opt.basis.trim().is_empty(), || {
            format!("[[models.optional_supplies]] '{r}' needs a basis citing the datasheet")
        });
        if let Some(input) = &opt.regulated_from {
            p.require(entry.pins.values().any(|v| v == input), || {
                format!(
                    "[[models.optional_supplies]] '{r}' regulated_from '{input}' is not a role in [models.pins]"
                )
            });
        }
    }
    for (rail, roles) in &entry.level_rails {
        let unmapped = std::iter::once(rail)
            .chain(roles)
            .find(|r| !entry.pins.values().any(|v| v == *r));
        p.require(unmapped.is_none(), || format!("[models.level_rails] '{rail}' names role '{}', which no pad in [models.pins] carries", unmapped.map_or("", String::as_str)));
    }
    for key in ["must_not_float_roles", "float_tolerated_roles"] {
        if !params.0.contains_key(key) {
            continue;
        }
        let Some(raw) = params.get_str(key) else {
            p.push(format!(
                "params.{key} must be a comma-separated string of [models.pins] roles"
            ));
            continue;
        };
        let mut seen = HashSet::new();
        let mut any = false;
        for role in raw.split(',').map(str::trim).filter(|r| !r.is_empty()) {
            any = true;
            if seen.insert(role.to_ascii_lowercase()) {
                p.require(has_role_ci(entry, role), || {
                    format!("params.{key} entry '{role}' is not a role in [models.pins]")
                });
            } else {
                p.push(format!("params.{key} repeats role '{role}'"));
            }
        }
        p.require(any, || format!("params.{key} names no roles"));
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
        validate_vreg_enable_params(entry, &mut p);
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
        if crate::schema::Ratings::keys().any(|k| k == u.name) {
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
    // Every numeric rating key (the table is the serialised struct, so a new key
    // is covered; `polarized` is the one flag).
    let stated = toml::Value::try_from(&entry.ratings).ok();
    let stated = || {
        (stated.as_ref())
            .and_then(toml::Value::as_table)
            .into_iter()
            .flatten()
    };
    for (name, v) in stated().filter_map(|(k, v)| Some((k, v.as_float()?))) {
        p.require(positive_finite(v), || {
            format!("rating '{name}' = {v} must be a positive finite number")
        });
    }
    for (key, _) in stated().filter(|(k, _)| !rating_applies(entry.kind, k)) {
        let valid = rating_keys(entry.kind);
        p.push(format!(
            "rating '{key}' does not apply to a {} card; its rating keys are: {}",
            serde_json::to_value(entry.kind)
                .ok()
                .as_ref()
                .and_then(|v| v.as_str())
                .unwrap_or("this"),
            valid.join(", ")
        ));
    }

    // A unit-suffixed field outside the range its meaning allows is a unit slip
    // (`cout_min_f = 1.0` for 1 uF), the one table `ranges::ROWS` holds.
    p.0.extend(
        crate::ranges::problems(entry)
            .iter()
            .map(crate::ranges::Implausible::message),
    );

    if let Some(program) = &entry.current_program {
        check_current_program(entry, program, &mut p);
    }
    for table in &entry.resistor_bins {
        check_resistor_bins(entry, table, &mut p);
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
            (source.problems().into_iter())
                .chain(crate::derating::tag_problems(entry))
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

/// A resistor table selects params at bind time, so it is held to what a
/// param is held to, plus the table's own shape: every row is exactly one of a
/// nominal or an open-ended range, the nominal windows cannot overlap (two rows
/// claiming one resistor is an ambiguous strap), and a row sets only a name the
/// kind's consumers read, never one `[models.params]` already fixes.
fn check_resistor_bins(entry: &ModelEntry, table: &crate::schema::ResistorBins, p: &mut Problems) {
    let pin = table.pin.trim();
    p.require(!pin.is_empty() && has_role_ci(entry, pin), || {
        not_a_role("resistor_bins.pin", &table.pin)
    });
    let tol = table.tolerance;
    p.require(tol.is_finite() && tol > 0.0 && tol < 0.5, || {
        format!("resistor_bins '{pin}' tolerance = {tol} must lie in (0, 0.5)")
    });
    p.require(!table.bins.is_empty(), || {
        format!("resistor_bins '{pin}' needs at least one [[models.resistor_bins.bin]] row")
    });
    let known = crate::param_names::known_param_names(entry.kind);
    let mut nominals = Vec::new();
    for bin in &table.bins {
        let shapes = [bin.ohms, bin.above_ohms, bin.below_ohms];
        let given: Vec<f64> = shapes.into_iter().flatten().collect();
        p.require(given.len() == 1 && given.iter().all(|r| positive_finite(*r)), || {
            format!("resistor_bins '{pin}' row {} must give exactly one positive ohms, above_ohms or below_ohms", bin.label())
        });
        nominals.extend(bin.ohms.filter(|r| positive_finite(*r)));
        for (name, value) in &bin.params {
            p.require(value.is_finite(), || {
                format!(
                    "resistor_bins '{pin}' row {} param '{name}' = {value} is not finite",
                    bin.label()
                )
            });
            p.require(known.contains(&name.as_str()), || {
                format!(
                    "resistor_bins '{pin}' row {} sets '{name}', which no {:?} consumer reads",
                    bin.label(),
                    entry.kind
                )
            });
            p.require(!entry.params.0.contains_key(name), || {
                format!("resistor_bins '{pin}' row {} sets '{name}', which [models.params] already fixes: the board's strap and the card cannot both decide it", bin.label())
            });
        }
    }
    nominals.sort_by(f64::total_cmp);
    for pair in nominals.windows(2) {
        let (lo, hi) = (pair[0], pair[1]);
        p.require(lo * (1.0 + tol) < hi * (1.0 - tol), || {
            format!("resistor_bins '{pin}' rows {lo:.0} and {hi:.0} ohm overlap at tolerance {tol}: one resistor would select both")
        });
    }
}

/// The datasheet's allowed programming-resistance bounds must be positive and
/// ordered, or the out-of-range report would fire on every board or on none.
fn check_program_ohm_range(program: &crate::schema::CurrentProgram, p: &mut Problems) {
    for (name, ohms) in [
        ("min_program_ohms", program.min_program_ohms),
        ("max_program_ohms", program.max_program_ohms),
    ] {
        if let Some(ohms) = ohms {
            p.positive(format!("current_program.{name}"), ohms);
        }
    }
    if let (Some(min), Some(max)) = (program.min_program_ohms, program.max_program_ohms) {
        p.require(min < max, || {
            format!(
                "current_program.min_program_ohms = {min} is not below max_program_ohms = {max}"
            )
        });
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

    check_program_ohm_range(program, p);

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
            (i < full).then(|| format!( "the law gives {} at {MIN_PROGRAM_OHMS} Ω, short of the {full} A full scale", format_amps(i)))
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
            format!( "current_program: the law gives {} at {ohms} Ω, but characteristic '{what}' prints {lo}..{hi} A there ({at}); {K_SI}", format_amps(law))
        });
    }
}

/// A state-controlled series path or a profiled load is solver-facing
/// connectivity: both ends must name roles the entry actually maps from
/// physical pins. A typo here otherwise validates, then the runtime silently
/// skips the path and the part that was supposed to close a rail stays open.
fn check_behavioral_roles(entry: &ModelEntry, p: &mut Problems) {
    let declared = entry.pins.values().map(|role| role.to_ascii_lowercase());
    let declared: HashSet<String> = declared.collect();
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
    // Without `ctrl` an SPDT binds one throw or none, whatever its select is called.
    let spdt = ["com", "s0", "s1", "ctrl"].map(|r| has_role(entry, r));
    let switch = entry.kind == ComponentKind::AnalogSwitch;
    p.require(!switch || spdt[..3].contains(&false) || spdt[3], || {
        "analog_switch maps com/s0/s1 but no 'ctrl': map the select/enable pin (SEL, EN, IN) \
         as 'ctrl', with s0 the throw joined to com while ctrl is LOW and s1 while it is HIGH \
         (so an active-low select's selected input is s0)"
            .into()
    });
}

/// The fix for a switching regulator drafted as a simple one: its pads hold a
/// switch node but no regulated output, which only a converter block binds.
pub const SWITCHER_FIX: &str = ". This pin map has a switch node (SW/PH/LX), so the part is a switching regulator: its regulated rail is past the inductor, on no pad of the IC. Keep the switch node 'sw' and add [models.behavioral.converter] with topology (\"buck\", \"boost\" or \"buck_boost\"), out_pin = \"sw\", in_pin = \"in\", vout_setpoint (the fixed output, or for an adjustable part the datasheet's typical-application output as a seed for a board without a divider, never the reference) and, for an adjustable part, [models.behavioral.converter.feedback] with pin = \"fb\" and vref_v = the feedback reference; drop vout/vref/dropout_v from [models.params]";

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
    // An identity-only switching converter names its switch node 'sw' and no
    // 'out': a non-synchronous boost has no output pad, and the switch node is
    // never one (`check_pinout`).
    let identity = entry.params.get_bool("identity_only").unwrap_or(false);
    if vreg && conv.is_none() && identity && entry.pins.values().any(|r| switch_node(r)) {
        return Vec::new();
    }
    let conv_required: Vec<&[&str]> = conv_roles.iter().flatten().map(|r| &r[..]).collect();
    let required: &[&[&str]] = match entry.kind {
        ComponentKind::Vreg if conv.is_some() => &conv_required,
        ComponentKind::Diode => &[
            &["anode", "a", "p", "common"],
            &["cathode", "k", "n", "common"],
        ],
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
    // A switching regulator mapped as a simple one: its rail is past the
    // inductor, so the way out is the converter block, never an invented pad.
    let switcher = vreg && conv.is_none() && entry.pins.values().any(|r| switch_node(r));
    let why = |f: &&[&str]| {
        let (role, kind, names) = (f[0], entry.kind, f.join(" / "));
        let adj = (role == "gnd" && declared.contains("adj")).then_some(". A fixed-output part's ADJ/GND pin is its ground: map it 'gnd'; 'adj' is for an adjustable card (vref, no vout)");
        let sw = (role == "out" && switcher).then_some(SWITCHER_FIX);
        format!("[models.pins] declares no '{role}' pin (a {kind:?} needs it); the part would bind OPEN. Accepted role names: {names}{}", adj.or(sw).unwrap_or_default())
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

/// `[[models.source.pinout]]`, one column per package the sheet prints.
fn pinouts(entry: &ModelEntry) -> &[Pinout] {
    entry.source.as_ref().map_or(&[], |s| &s.pinout)
}

/// The package standard a card's sheet, printing lead letters alone, numbers its
/// pads from (kept on a compiled card): each such pad binds as an assumption.
#[must_use]
pub fn pin_standard(entry: &ModelEntry) -> Option<&str> {
    let declared = pinouts(entry).iter().find_map(|o| o.standard.as_deref());
    declared.or(entry.pin_standard.as_deref())
}

/// The pin-table column of the card's own package: `[models.variant] package`,
/// else the one package the description names, else a lone column. `Ok(None)`
/// with no column; `Err` naming the columns when none or several are the card's.
fn own_pinout(entry: &ModelEntry) -> Result<Option<&Pinout>, String> {
    let pinouts = pinouts(entry);
    if pinouts.is_empty() {
        return Ok(None);
    }
    let plain = |s: &str| s.to_lowercase().replace(|c: char| !c.is_alphanumeric(), "");
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
    let outer = |o: &&Pinout| !named.iter().any(|b| within(o, b));
    let named: Vec<&Pinout> = named.iter().copied().filter(outer).collect();
    let [own] = named[..] else {
        let all: Vec<&str> = pinouts.iter().map(|o| o.package.as_str()).collect();
        return Err(format!("[[models.source.pinout]] records {}, but neither [models.variant] package nor the description names exactly one of them: name the one package [models.pins] is for", all.join(", ")));
    };
    Ok(Some(own))
}

/// How a sheet whose packages number their pads differently is carded.
///
/// The prompts (P4, the pins section) and [`draft_pin_map_problems`] quote it.
/// Measured: one 8-pin PDIP/SOIC map written for an EEPROM whose SOT-23-6
/// numbers the same signals differently, so a SOT-23 board bound wrong pads.
pub const PACKAGE_SPLIT_RULE: &str = "when the part number names no package and its packages \
    number their pads differently, write one [[models]] card per numbering, each with its own \
    [models.pins], its own [[models.source.pinout]] column, the package in its description and a \
    [models.match] footprint_re naming that package's footprints (e.g. '(?i)SOT-?23' or \
    '(?i)(SOIC|SO|DIP)-?8'); packages sharing one numbering share one card, and none takes \
    another's map";

/// Whether two pin-table columns number their pads differently: another pad
/// set, or a pad both print under different names.
fn numbering_differs(a: &Pinout, b: &Pinout) -> bool {
    let plain = |s: &str| s.to_lowercase().replace(|c: char| !c.is_alphanumeric(), "");
    a.pins.len() != b.pins.len()
        || a.pins.iter().any(|(pad, name)| {
            b.pins
                .get(pad)
                .is_none_or(|other| plain(other) != plain(name))
        })
}

/// The lead numbering a package standard fixes, for a sheet printing only lead
/// letters on a drawn view.
///
/// The one table the P7 prompt rule quotes and [`draft_pin_map_problems`]
/// cites, keyed by [`crate::package::package_body`] name.
pub const LEAD_STANDARDS: &[(&str, &str)] = &[
    (
        "SOT-23",
        "JEDEC TO-236 top view: 1 lower-left, 2 lower-right, 3 top",
    ),
    (
        "SOT-223",
        "JEDEC TO-261 top view, leads down: 1, 2, 3 left to right, 4 the tab",
    ),
    (
        "SOT-89",
        "JEDEC TO-243 top view, leads down: 1, 2, 3 left to right, the tab joined to 2",
    ),
    (
        "SOT-323",
        "JEITA SC-70 (Nexperia SOT323 outline) top view: 1 lower-left, 2 lower-right, 3 top",
    ),
    (
        "SOT-416",
        "JEITA SC-75 (Nexperia SOT416 outline) top view: 1 lower-left, 2 lower-right, 3 top",
    ),
    (
        "SOT-523",
        "the SC-75 body (Nexperia SOT416 outline) top view: 1 lower-left, 2 lower-right, 3 top",
    ),
];

/// The package body `package` spells and its [`LEAD_STANDARDS`] numbering.
///
/// The three-lead bodies smaller than SOT-23 (SOT-323, SOT-416, SOT-523) have
/// no row of their own in the package-body table, which reads them as the
/// catch-all `SOT`; for those the spelling itself names the standard.
#[must_use]
pub fn lead_standard(package: &str) -> Option<(&'static str, &'static str)> {
    let body = crate::package::package_body(package).map(|b| b.name);
    let squash = |s: &str| {
        let kept = s.chars().filter(char::is_ascii_alphanumeric);
        kept.map(|c| c.to_ascii_uppercase()).collect::<String>()
    };
    let spelled = squash(package.rsplit(':').next().unwrap_or(package));
    let small = |name: &str| matches!(body, None | Some("SOT")) && spelled.contains(&squash(name));
    LEAD_STANDARDS
        .iter()
        .copied()
        .find(|(name, _)| body == Some(*name) || small(name))
}

/// The P7 table as prompt lines.
#[must_use]
pub fn lead_standards_guide() -> String {
    let lines = LEAD_STANDARDS
        .iter()
        .map(|(p, s)| format!("        {p}: {s}"));
    lines.collect::<Vec<_>>().join("\n")
}

/// The package a card's `[models.pins]` is read for.
///
/// That is `[models.variant] package`, else its own `[[models.source.pinout]]` column's package. Kept on
/// a compiled card so the binder can hold it against the footprint.
#[must_use]
pub fn pin_package(entry: &ModelEntry) -> Option<&str> {
    let declared = entry.variant.as_ref().and_then(|v| v.package.as_deref());
    let own = || own_pinout(entry).ok().flatten().map(|o| o.package.as_str());
    declared.or_else(own).or(entry.pin_package.as_deref())
}

/// Printed names of a switching converter's switch node (`SW`, `LX`, `PH`, `L1`).
fn switch_node(name: &str) -> bool {
    pin_name_keys(name)
        .iter()
        .any(|k| matches!(k.as_str(), "sw" | "lx" | "ph" | "phase" | "l"))
}

/// A printed feedback input (`FB`, `VFB`, `ADJ`) that names no output too
/// (`FB/VOUT` is a fixed part's output): it senses the output through a
/// divider and is never the regulated output itself.
fn feedback_node(name: &str) -> bool {
    let keys = pin_name_keys(name);
    let output = |k: &String| matches!(k.as_str(), "out" | "output" | "vo" | "os" | "sense");
    keys.iter().any(|k| matches!(k.as_str(), "fb" | "adj")) && !keys.iter().any(output)
}

/// `[models.pin_capability]`: an MCU card's structured capability tables.
///
/// The table is a complete statement, so it is held to the same pad map the
/// card's pins use: every listed pad must exist there, must not be a supply or
/// ground pad, and a channel is named for each; the basis cites the datasheet.
fn check_pin_capability(entry: &ModelEntry, p: &mut Problems) {
    let Some(adc) = &entry.pin_capability.adc else {
        return;
    };
    p.require(entry.kind == ComponentKind::Mcu, || {
        "[models.pin_capability] belongs on an mcu card".into()
    });
    p.require(!adc.basis.trim().is_empty(), || {
        "[models.pin_capability.adc] needs a basis citing the datasheet table and page".into()
    });
    for (pad, channel) in &adc.pads {
        let Some(role) = entry.pins.get(pad) else {
            p.push(format!(
                "[models.pin_capability.adc] pad '{pad}' is not a pad of [models.pins]"
            ));
            continue;
        };
        p.require(!channel.trim().is_empty(), || {
            format!("[models.pin_capability.adc] pad '{pad}' needs its ADC channel name")
        });
        let r = role.trim().to_ascii_lowercase();
        let supply = crate::pin_roles::power_pin(&r, entry.kind).is_some();
        p.require(!supply, || {
            format!(
                "[models.pin_capability.adc] pad '{pad}' is the supply or ground role '{role}', not an ADC input"
            )
        });
    }
}

/// `[models.pins]` against the pin-table column of the card's own package
/// (see [`own_pinout`]): the same pads, and no pad whose role is the name that
/// column prints on another pad, the mark of a map read off another package.
fn check_pinout(entry: &ModelEntry, p: &mut Problems) {
    let pinouts = pinouts(entry);
    let numbered = |m: &BTreeMap<String, String>| m.keys().all(|k| k.parse::<u32>().is_ok());
    for o in pinouts.iter().filter(|o| o.standard.is_some()) {
        let (package, standard) = (&o.package, o.standard.as_deref().unwrap_or_default());
        let view = standard.to_lowercase();
        p.require(numbered(&o.pins) && numbered(&entry.pins), || format!("[[models.source.pinout]] '{package}' numbers its pads from standard '{standard}', so it and [models.pins] are keyed by pad number: a card keyed by lead letters carries no standard"));
        p.require(view.contains("top view") || view.contains("bottom view"), || format!("[[models.source.pinout]] '{package}' standard '{standard}' must name the view (\"top view\" or \"bottom view\") its numbering is read from: a pad numbered from the wrong view is a mirrored pinout that binds cleanly"));
        p.require(!entry.coverage.missing.iter().any(|m| m == "pad_numbers"), || format!("[[models.source.pinout]] '{package}' numbers the pads from standard '{standard}', so `pad_numbers` is not missing from [models.coverage]"));
    }
    check_package_fit(entry, p);
    if entry.pins.is_empty() {
        return;
    }
    let own = match own_pinout(entry) {
        Ok(Some(own)) => own,
        Ok(None) => return,
        Err(why) => return p.push(why),
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
        let printed = own.pins.get(pad);
        let regulated =
            entry.kind == ComponentKind::Vreg && matches!(key.as_str(), "out" | "output");
        if let Some(name) = printed.filter(|name| regulated && switch_node(name)) {
            p.push(format!("[models.pins] pad {pad} = '{role}', but the '{package}' pinout prints '{name}' on it: that is the converter's switch node, never its regulated output. Map it 'sw'; the regulated rail is past the inductor (and a boost's past its diode), which is no pad of the IC"));
        }
        if let Some(name) = printed.filter(|name| regulated && feedback_node(name)) {
            p.push(format!("[models.pins] pad {pad} = '{role}', but the '{package}' pinout prints '{name}' on it: that is the feedback input, which senses the output through a divider, never the regulated output. Map it 'fb' (or 'adj')"));
        }
        let prints = |name: &str| pin_name_keys(name).contains(&key);
        let Some(here) = printed.filter(|name| !prints(name)) else {
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

/// Package names a card spells for itself: `[models.variant] package`, the
/// footprint rule's alternatives, and the digit-bearing words of its id and
/// description (`MSOP-10`, `sot23`), where the package table knows them.
fn card_packages(entry: &ModelEntry) -> Vec<String> {
    let variant = entry.variant.as_ref().and_then(|v| v.package.clone());
    let footprints = (entry.r#match.footprint_re.iter()).flat_map(|f| f.split('|'));
    let words = format!("{} {}", entry.description, entry.id.replace('_', " "));
    let words = words.split(|c: char| !c.is_alphanumeric() && c != '-');
    let named = words.filter(|w| w.contains(|c: char| c.is_ascii_digit()));
    let all = variant.into_iter().chain(footprints.map(str::to_owned));
    let all = all.chain(named.map(str::to_owned));
    all.filter(|name| crate::package::package_body(name).is_some())
        .collect()
}

/// A warning when the pin-table column `[models.pins]` is read from is for another package.
///
/// It names a package body that none of the card's own package names spell: a sheet
/// printing several order codes has one column per code, and a neighbour's
/// (the CH340K's ESSOP10 for the CH340X's MSOP-10) swaps pads silently.
/// Silent when either side's package is unknown.
#[must_use]
pub fn pinout_package_warning(entry: &ModelEntry) -> Option<String> {
    let column = &own_pinout(entry).ok().flatten()?.package;
    let card = card_packages(entry);
    let other = |a: &str, b: &str| crate::package::bodies_differ(a, b) == Some(true);
    let foreign = |part: &str| card.iter().all(|k| other(part, k));
    (!card.is_empty() && column.split('/').all(foreign)).then(|| {
        format!("[[models.source.pinout]] column '{column}' is a different package from the card's {}: a multi-variant sheet prints one column or drawing per order code, so read the pads from the column or drawing for this order code's package", card.join(", "))
    })
}

/// A numbered pin map against the pin count its declared package spells: no
/// pad past it (one more for an exposed pad), and an own column recording at
/// least that many pads. A map that does not fit was read for another package.
fn check_package_fit(entry: &ModelEntry, p: &mut Problems) {
    let Some(package) = pin_package(entry) else {
        return;
    };
    let Some(count) = crate::package::pin_count(package) else {
        return;
    };
    let pads = |m: &BTreeMap<String, String>| -> Vec<u32> {
        m.keys().filter_map(|k| k.parse().ok()).collect()
    };
    let past: Vec<String> = pads(&entry.pins)
        .into_iter()
        .filter(|&n| n == 0 || n > count + 1)
        .map(|n| n.to_string())
        .collect();
    p.require(past.is_empty(), || format!("[models.pins] maps pads {} but a {package} has {count} pins (and at most one exposed pad): the map was read for another package's column", past.join(", ")));
    let own = own_pinout(entry).ok().flatten();
    let short = own
        .map(|o| (o.pins.len(), pads(&o.pins).len()))
        .filter(|&(all, n)| all == n && n < count as usize);
    if let Some((n, _)) = short {
        p.push(format!("[[models.source.pinout]] '{package}' records {n} pads but a {package} has {count}: record every pad the column prints (NC included), or name the package this column is really for"));
    }
}

/// A warning when the card leaves many pads `unknown_pad_`.
///
/// A supply or ground pin among them goes unchecked, so the card cannot be
/// trusted for supply checks. Advisory (the
/// lint, the draft and the binder say it); the threshold is a quarter of the
/// mapped pads and at least two.
#[must_use]
pub fn unknown_pads_warning(entry: &ModelEntry) -> Option<String> {
    let unknown = entry
        .pins
        .values()
        .filter(|r| r.starts_with("unknown_pad_"))
        .count();
    let total = entry.pins.len();
    (unknown >= 2 && unknown * 4 >= total).then(|| {
        format!("{unknown} of {total} pads in [models.pins] are unknown_pad_: any supply or ground pin among them goes unchecked, so this card cannot be trusted for supply checks")
    })
}

/// The rating categories nearly every datasheet of a kind prints.
///
/// Each is `(key, what it is)`: a draft that has neither the key nor an
/// `unknown` record for it most likely skipped the row. Measured: two tiers
/// drafting one pack FET driver both dropped its printed TJ maximum.
#[must_use]
pub const fn expected_ratings(kind: ComponentKind) -> &'static [(&'static str, &'static str)] {
    use ComponentKind::{
        Adc, AnalogSwitch, BjtNpn, BjtPnp, Comparator, Dac, Digital, Diode, Mcu, Nmos, Opamp, Pmos,
        ShiftRegister, Timer, Vreg,
    };
    const VOLTAGE: (&str, &str) = (
        "max_voltage_v",
        "absolute-maximum supply or blocking voltage",
    );
    const TJ: (&str, &str) = ("max_junction_temp_c", "junction-temperature maximum");
    const CURRENT: (&str, &str) = ("max_current_a", "rated continuous (output) current");
    match kind {
        Diode | BjtNpn | BjtPnp | Nmos | Pmos | Vreg => &[VOLTAGE, TJ, CURRENT],
        Opamp | Comparator | AnalogSwitch | Digital | ShiftRegister | Dac | Adc | Timer => {
            &[VOLTAGE, TJ]
        }
        Mcu => &[VOLTAGE],
        _ => &[],
    }
}

/// Whether a pin role names a power or drive output rather than a signal.
///
/// A switch node or a half-bridge, motor-phase or gate-drive output (`sw`,
/// `phase`, `ho`/`lo`, `out1`, `out_a`, `drv`) carries load current; a plain
/// `out` is a signal output unless the part also has a supply input.
fn is_output_stage_role(role: &str) -> bool {
    let r = role.to_ascii_lowercase();
    let stem = r.trim_end_matches(|c: char| c.is_ascii_digit() || c == '_');
    if matches!(
        stem,
        "sw" | "lx" | "phase" | "ph" | "ho" | "lo" | "gh" | "gl" | "drv" | "drive"
    ) {
        return true;
    }
    r.strip_prefix("out").is_some_and(|rest| {
        let rest = rest.trim_start_matches('_');
        !rest.is_empty()
            && rest.len() <= 2
            && rest.chars().all(|c| c.is_ascii_alphanumeric())
            && rest != "en"
    })
}

/// Whether a digital-kind card describes a power or driver output stage.
///
/// Decided from the card's role data, never its name: no executable logic (its
/// outputs are not logic levels), and either a switch, bridge or gate-drive
/// output role, or a supply input role beside a plain `out` (a load switch).
fn has_output_stage(entry: &ModelEntry) -> bool {
    if entry.kind != ComponentKind::Digital || !entry.logic.is_empty() {
        return false;
    }
    let roles: Vec<String> = entry
        .pins
        .values()
        .map(|r| r.to_ascii_lowercase())
        .collect();
    let has = |names: &[&str]| roles.iter().any(|r| names.contains(&r.as_str()));
    roles.iter().any(|r| is_output_stage_role(r)) || (has(&["out", "vout"]) && has(&["in", "vin"]))
}

/// [`expected_ratings`] for this card: its kind's categories, plus the rated
/// output current when the card is a power or driver output stage.
///
/// Measured: a motor driver's 3.5 A output rating was kept as an untagged
/// characteristic row because the digital kind did not expect a current.
#[must_use]
pub fn expected_ratings_for(entry: &ModelEntry) -> Vec<(&'static str, &'static str)> {
    let mut expected = expected_ratings(entry.kind).to_vec();
    if has_output_stage(entry) {
        expected.push(("max_current_a", "rated continuous (output) current"));
    }
    expected
}

/// Whether `[models.ratings]` key `key` means anything on a `kind` card.
///
/// A capacitor's ripple, a MOSFET's gate bias, a regulator's minimum load and an
/// IC's per-pin limit are each read only for their own class.
#[must_use]
pub fn rating_applies(kind: ComponentKind, key: &str) -> bool {
    use ComponentKind::{BjtNpn, BjtPnp, Connector, Diode, Nmos, Passive, Pmos, Vreg};
    match key {
        "polarized" | "max_ripple_current_a" => kind == Passive,
        "rds_on_vgs" | "vgs_th_max" | "vgs_max" => matches!(kind, Nmos | Pmos),
        "min_current_a" => kind == Vreg,
        "max_pin_current_a" => !matches!(
            kind,
            Diode | BjtNpn | BjtPnp | Nmos | Pmos | Passive | Connector
        ),
        _ => true,
    }
}

/// How a drafted card's `value_re` / `mpn_re` is anchored: the extraction prompt
/// states it and [`without_end_anchor`] enforces it, so the two cannot disagree.
pub const MATCH_PATTERN_RULE: &str = "Anchor the pattern at the START only, never the end: \
    a board value or orderable code carries a suffix (package, reel, grade, a description), \
    so `^PART$` binds nothing but the bare name.";

/// `pattern` without the end anchor [`MATCH_PATTERN_RULE`] forbids, or `None` when it
/// ends with none (an escaped `\$` is a literal dollar, not an anchor).
#[must_use]
pub fn without_end_anchor(pattern: &str) -> Option<&str> {
    let bare = pattern.strip_suffix('$')?;
    let slashes = bare.bytes().rev().take_while(|b| *b == b'\\').count();
    if slashes % 2 != 0 {
        return None;
    }
    // `^PART|$`: the end anchor was a whole alternative ("or the name ends here"),
    // and the `|` it leaves behind would be an empty alternative matching every value.
    let alternative = bare
        .strip_suffix('|')
        .filter(|head| head.bytes().rev().take_while(|b| *b == b'\\').count() % 2 == 0);
    Some(alternative.unwrap_or(bare))
}

/// The rating keys that apply to `kind` ([`rating_applies`]): the ones validation
/// accepts, and the ones the prompt may offer.
#[must_use]
pub fn rating_keys(kind: ComponentKind) -> Vec<&'static str> {
    crate::schema::Ratings::keys()
        .filter(|k| rating_applies(kind, k))
        .collect()
}

/// The `KEY_GUIDE` lines whose key applies to `kind` ([`rating_applies`]).
#[must_use]
pub fn rating_guide(kind: ComponentKind) -> String {
    let lines = crate::schema::Ratings::KEY_GUIDE.lines();
    let applies = |l: &&str| {
        l.split_whitespace()
            .next()
            .is_some_and(|k| rating_applies(kind, k))
    };
    lines.filter(applies).collect::<Vec<_>>().join("\n")
}

/// Advisory: each expected rating category ([`expected_ratings`]) left unstated.
///
/// That is, the card neither states (the key, or a row tagged `rating = "<key>"`) nor declares
/// absent (an `unknown` uncertainty record whose `parameter` is the key).
#[must_use]
pub fn missing_rating_warnings(entry: &ModelEntry) -> Vec<String> {
    let stated = toml::Value::try_from(&entry.ratings).ok();
    let stated = stated.as_ref().and_then(toml::Value::as_table);
    let source = entry.source.as_ref();
    let tagged = |key: &str| {
        source.is_some_and(|s| {
            s.characteristic
                .iter()
                .any(|r| r.rating.as_deref() == Some(key))
        })
    };
    let declared = |key: &str| {
        source.is_some_and(|s| {
            s.uncertainty.iter().any(
                |u| matches!(u, ModelUncertainty::Unknown { parameter, .. } if parameter == key),
            )
        })
    };
    expected_ratings_for(entry)
        .iter()
        .filter(|(key, _)| {
            !(stated.is_some_and(|t| t.contains_key(*key)) || tagged(key) || declared(key))
        })
        .map(|(key, what)| {
            format!(
                "no {key} ({what}): read it from the Absolute Maximum Ratings, Recommended \
                 Operating Conditions or Thermal tables; if the datasheet prints none, add \
                 [[models.source.uncertainty]] status = \"unknown\", parameter = \"{key}\", \
                 reason naming the tables checked"
            )
        })
        .collect()
}

/// An adjustable converter's `vout_setpoint` written as its own reference.
///
/// `vout_setpoint == feedback.vref_v` is how a FIXED-output card says its FB
/// pin senses VOUT directly (the checks read it so), so on an adjustable part
/// it claims a fixed output at the reference voltage. The output of an
/// adjustable part is the board divider's; the seed is the datasheet's
/// typical-application output. A card naming itself fixed may keep it.
#[must_use]
pub fn adjustable_seed_problem(entry: &ModelEntry) -> Option<String> {
    let c = entry.behavioral.converter.as_ref()?;
    let fb = c.feedback.as_ref()?;
    let description = entry.description.to_ascii_lowercase();
    let fixed = description.contains("fixed") && !description.contains("adjustable");
    let equal = (c.vout_setpoint - fb.vref_v).abs() <= 1e-6 * fb.vref_v.abs().max(1.0);
    (equal && !fixed).then(|| {
        format!(
            "converter vout_setpoint = {} equals feedback vref_v, which declares a FIXED {} V \
             output whose FB pin senses VOUT directly. An adjustable part's output is set by the \
             board's feedback divider, which Hauksbee reads: set vout_setpoint to the output of \
             the datasheet's typical application (cite it) as the seed for a board without one, \
             never the reference. Only a fixed-output version keeps vout_setpoint = vref_v, and \
             its description says 'fixed'",
            c.vout_setpoint, c.vout_setpoint
        )
    })
}

/// What a machine draft must carry beyond [`validate`].
///
/// A numbered pin map of three or more pads on an IC or discrete records the package it was read
/// for, as that package's `[[models.source.pinout]]` column, so the checker can
/// hold the map against it and the binder against the footprint.
///
/// A map keyed only by lead letters (G/S/D, B/C/E) on a package whose standard
/// fixes the lead numbering ([`LEAD_STANDARDS`]) is numbered by it (prompt rule
/// P7): a footprint numbers its pads, so letters bind nothing. Measured: a
/// SOT-223 MOSFET drafted with `"G"`, `"S"`, `"D"` keys under a drawn top view.
#[must_use]
pub fn draft_pin_map_problems(entry: &ModelEntry) -> Vec<String> {
    use ComponentKind::{Connector, Ignore, Passive};
    let lettered = entry.pins.len() >= 3
        && entry
            .pins
            .keys()
            .all(|k| !k.chars().any(|c| c.is_ascii_digit()));
    let standard = pin_package(entry).and_then(lead_standard);
    if let Some((package, standard)) = standard.filter(|_| lettered) {
        let keys: Vec<&str> = entry.pins.keys().map(String::as_str).collect();
        return vec![format!("[models.pins] is keyed by lead letters ({}) on a {package} part: a board footprint numbers its pads, so letters bind nothing. Number the pins by the package standard, {standard}, read against the datasheet's drawn view, and record `standard = \"{standard}\"` in the [[models.source.pinout]] column (its pins as printed), per P7", keys.join(", "))];
    }
    let numbered = entry
        .pins
        .keys()
        .filter(|k| k.parse::<u32>().is_ok())
        .count();
    let exempt = matches!(entry.kind, Passive | Connector | Ignore) || numbered < 3;
    if exempt {
        return Vec::new();
    }
    let Ok(Some(own)) = own_pinout(entry) else {
        return vec!["[models.pins] numbers its pads but records no [[models.source.pinout]] column it is read from: add the column for the package this map is for (package = as printed, pins = the names it prints), so the map can be checked against that package and the board's footprint".into()];
    };
    let package = &own.package;
    let gated = entry.r#match.footprint_re.is_some()
        || entry.variant.as_ref().is_some_and(|v| v.package.is_some());
    let renumbered = pinouts(entry)
        .iter()
        .filter(|o| !std::ptr::eq(*o, own) && numbering_differs(o, own));
    let others: Vec<&str> = renumbered.map(|o| o.package.as_str()).collect();
    if !gated && !others.is_empty() {
        return vec![format!(
            "[models.pins] is read from '{package}', but the column(s) {} number their pads \
             differently, and this card names no package a board could be held to: {PACKAGE_SPLIT_RULE}",
            others.join(", ")
        )];
    }
    let unread: Vec<&str> = (own.pins.iter())
        .filter(|(_, name)| name.starts_with("unknown_pad_"))
        .map(|(pad, _)| pad.as_str())
        .collect();
    let why = format!("[[models.source.pinout]] '{package}' records pads {} as unknown_pad_: a column records the names its pin table prints for this package; a pad left unread means the map was not read from this package's column", unread.join(", "));
    (!unread.is_empty()).then_some(why).into_iter().collect()
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
    let distance = |k: &&'static str| (hauksbee_ir::levenshtein(&lower, k), *k);
    let near = KIND_NAMES.iter().map(distance).filter(|n| n.0 <= 2);
    alias.or_else(|| near.min_by_key(|n| n.0).map(|n| n.1))
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
                    ("characteristic", _) => (
                        "parameter unit conditions basis",
                        "min typ max rating package mounting pins",
                    ),
                    ("pinout", _) => ("package pins", "standard"),
                    (_, Some("interval")) => ("status parameter low high unit kind basis", ""),
                    (_, Some("unknown")) => ("status parameter reason", ""),
                    _ => ("status", "parameter low high unit kind basis reason"),
                };
                let required: Vec<&str> = required.split_whitespace().collect();
                let at = format!("{path}.{key}[{i}]");
                for missing in required.iter().filter(|k| !fields.contains_key(**k)) {
                    out.push(format!("{at}: missing required key '{missing}'"));
                }
                let known = required.iter().copied().chain(optional.split_whitespace());
                let known: Vec<&str> = known.collect();
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

/// The keys that belong under `[models.params]` and nowhere else.
const PARAMS_ONLY_KEYS: [&str; 3] = ["identity_only", "warning", "unlocked_by"];

/// Where `identity_only`, `warning` or `unlocked_by` sit outside `[models.params]`.
///
/// Read from the raw TOML because the schema drops such a key at the entry level
/// without a word, and names it only as "unknown field" inside another table, so
/// the card is refused for lacking the very key it carries.
#[must_use]
pub fn misplaced_param_keys(root: &toml::Value) -> Vec<String> {
    let models = root.get("models").and_then(toml::Value::as_array);
    let mut out = Vec::new();
    for entry in models
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_table)
    {
        let id = entry.get("id").and_then(toml::Value::as_str).unwrap_or("?");
        let tables = entry.iter().filter(|(k, _)| *k != "params");
        let tables = tables.filter_map(|(k, v)| Some((format!("[models.{k}]"), v.as_table()?)));
        let places = std::iter::once(("[[models]] itself".to_string(), entry)).chain(tables);
        for (place, table) in places {
            for key in PARAMS_ONLY_KEYS.iter().filter(|k| table.contains_key(**k)) {
                out.push(format!(
                    "model '{id}': `{key}` is set directly in {place}, but it is a param and only \
                     counts under [models.params]: move the line `{key} = ...` below the \
                     `[models.params]` header"
                ));
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

    /// A digital card with no logic and a bridge, switch-node or load-switch
    /// output expects a rated current; a signal-output or logic card does not.
    #[test]
    fn output_stage_digital_cards_expect_a_current_rating() {
        let warns_current = |e: &ModelEntry| {
            missing_rating_warnings(e)
                .iter()
                .any(|w| w.starts_with("no max_current_a"))
        };
        let mut bridge = entry(ComponentKind::Digital, &[]);
        pins(
            &mut bridge,
            &[("1", "gnd"), ("2", "vm"), ("3", "out1"), ("4", "out2")],
        );
        assert!(warns_current(&bridge));
        let mut switch = entry(ComponentKind::Digital, &[]);
        pins(&mut switch, &[("1", "in"), ("2", "out"), ("3", "gnd")]);
        assert!(warns_current(&switch));
        let mut detector = entry(ComponentKind::Digital, &[]);
        pins(&mut detector, &[("1", "vcc"), ("2", "out"), ("3", "gnd")]);
        assert!(!warns_current(&detector));
        let mut stated = bridge.clone();
        stated.ratings.max_current_a = Some(3.5);
        assert!(!warns_current(&stated));
    }

    /// A simple regulator card whose pad 3 carries `role`, with extra params.
    fn en(role: &str, params: &[(&str, f64)], level: Option<&str>) -> ModelEntry {
        let mut p = vec![("vout", 3.3), ("dropout_v", 0.3), ("iq_a", 1e-4)];
        p.extend_from_slice(params);
        let mut e = entry(ComponentKind::Vreg, &p);
        pins(
            &mut e,
            &[("1", "in"), ("2", "gnd"), ("3", role), ("5", "out")],
        );
        if let Some(level) = level {
            e.params.set_str("en_on_level", level);
        }
        e
    }

    /// The simple regulator's enable fields: paired, ordered for the polarity,
    /// one pull at most, and only on a card that names the pin.
    #[test]
    fn vreg_enable_fields_are_paired_ordered_and_need_the_pin() {
        let levels = [("en_on_v", 1.5), ("en_off_v", 0.4)];
        accepts(&en("en", &levels, None));
        accepts(&en("en", &levels, Some("high")));
        accepts(&en(
            "shdn",
            &[("en_on_v", 0.4), ("en_off_v", 1.5)],
            Some("low"),
        ));
        accepts(&en(
            "en",
            &[("en_pull_down_ohms", 3e6), ("en_discharge_ohms", 60.0)],
            None,
        ));
        accepts(&en("en", &[("i_shdn_a", 1e-7)], None));
        accepts(&en("en", &[("i_shdn_a", 0.0)], None));
        rejects(&en("en", &[("i_shdn_a", 1e-4)], None), "below 'iq_a'");
        rejects(&en("en", &[("i_shdn_a", 2e-4)], None), "below 'iq_a'");
        rejects(&en("en", &[("i_shdn_a", -1e-7)], None), "below 'iq_a'");
        rejects(
            &en("nc", &[("i_shdn_a", 1e-7)], None),
            "'en' or 'shdn' role",
        );
        rejects(&en("nc", &levels, None), "'en' or 'shdn' role");
        rejects(&en("en", &[("en_on_v", 1.5)], None), "need both");
        rejects(
            &en("en", &[("en_on_v", 0.4), ("en_off_v", 1.5)], None),
            "off side",
        );
        rejects(&en("shdn", &levels, Some("low")), "off side");
        rejects(&en("en", &levels, Some("rising")), "\"high\" or \"low\"");
        rejects(
            &en("en", &[("en_on_v", 90.0), ("en_off_v", 0.4)], None),
            "within 60 V",
        );
        rejects(
            &en(
                "en",
                &[("en_pull_down_ohms", 3e6), ("en_pull_up_a", 1e-6)],
                None,
            ),
            "at most one internal pull",
        );
        rejects(
            &en("en", &[("en_discharge_ohms", 0.0)], None),
            "positive finite",
        );
    }

    /// The stated open-pin state: a word, on a card that names the pin, and
    /// never against a pull the card also states; its quote backs a stated state.
    #[test]
    fn vreg_enable_open_state_is_a_word_its_pull_allows() {
        let open = |role: &str, params: &[(&str, f64)], level: Option<&str>, state: &str| {
            let mut e = en(role, params, level);
            e.params.set_str("en_open", state);
            e
        };
        accepts(&open("en", &[], None, "off"));
        accepts(&open("en", &[], None, "on"));
        accepts(&open("shdn", &[], Some("low"), "off"));
        accepts(&open("en", &[("en_pull_down_ohms", 3e6)], None, "off"));
        accepts(&open("en", &[("en_pull_up_a", 1e-6)], None, "on"));
        accepts(&open(
            "shdn",
            &[("en_pull_down_a", 1e-6)],
            Some("low"),
            "on",
        ));
        rejects(&open("en", &[], None, "floating"), "\"off\" or \"on\"");
        rejects(&open("nc", &[], None, "off"), "'en' or 'shdn' role");
        rejects(
            &open("en", &[("en_pull_up_a", 1e-6)], None, "off"),
            "contradicts the card's internal pull-up",
        );
        rejects(
            &open("en", &[("en_pull_down_ohms", 3e6)], None, "on"),
            "contradicts the card's internal pull-down",
        );
        rejects(
            &open("shdn", &[("en_pull_down_a", 1e-6)], Some("low"), "off"),
            "contradicts the card's internal pull-down",
        );
        // The datasheet's words for the open state back a stated state.
        let quoted = |mut e: ModelEntry, quote: &str| {
            e.params.set_str("en_open_basis", quote);
            e
        };
        accepts(&quoted(
            open("en", &[], None, "off"),
            "p.3: logic low or open = shutdown",
        ));
        accepts(&quoted(
            en("en", &[("en_pull_down_ohms", 3e6)], None),
            "p.4: RPD 3 MOhm",
        ));
        rejects(
            &quoted(en("en", &[], None), "p.3: open = off"),
            "needs 'en_open'",
        );
        rejects(
            &quoted(open("en", &[], None, "off"), "  "),
            "non-empty string",
        );
        rejects(
            &quoted(open("nc", &[], None, "off"), "p.3"),
            "'en' or 'shdn' role",
        );
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
        let mut levels = entry(ComponentKind::Digital, &[]);
        levels
            .levels
            .insert("vih".into(), vec![[4.5, 3.15], [2.0, 1.5]]);
        rejects(&levels, "ascending [vcc, volts] points");
        let mut rails = entry(ComponentKind::Digital, &[]);
        pins(&mut rails, &roles("1:vcca 3:a1 5:dir 6:vccb"));
        rails
            .level_rails
            .insert("vcca".into(), vec!["a1".into(), "dir".into()]);
        let rail_errors = |e: &ModelEntry| {
            validate(e)
                .err()
                .into_iter()
                .flatten()
                .filter(|x| x.message.contains("level_rails"))
                .count()
        };
        assert_eq!(rail_errors(&rails), 0);
        rails.level_rails.insert("vccb".into(), vec!["b1".into()]);
        rejects(&rails, "names role 'b1'");
    }

    /// A buck mapped as a linear regulator is told the converter way out,
    /// never to invent an `out` pad; a linear one missing `out` is not.
    #[test]
    fn a_switcher_missing_out_is_pointed_at_the_converter_block() {
        let params = [("vout", 3.3), ("dropout_v", 0.3), ("iq_a", 1e-3)];
        let mut buck = entry(ComponentKind::Vreg, &params);
        pins(
            &mut buck,
            &[("1", "in"), ("2", "sw"), ("3", "gnd"), ("4", "fb")],
        );
        rejects(&buck, "add [models.behavioral.converter] with topology");
        let mut linear = entry(ComponentKind::Vreg, &params);
        pins(&mut linear, &[("1", "in"), ("2", "gnd")]);
        assert!(!missing_pin_roles(&linear).concat().contains("converter"));
        rejects(&linear, "declares no 'out' pin");
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
        let per_path = |r1| entry(AnalogSwitch, &[("ron", 5.0), ("ron_s0", 0.25), ("ron_s1", r1), ("roff", 1e9)]);
        accepts(&per_path(1.3));
        rejects(&per_path(50_000.0), "ron_s1");

        rejects(
            &entry(
                Comparator,
                &[("out_lo", 3.3), ("out_hi", 0.0), ("hysteresis", 0.05)],
            ),
            "out_lo",
        );
    }

    /// A recommended output capacitance: positive, never below a stated
    /// minimum, and a named type only beside it.
    #[test]
    fn recommended_output_capacitance() {
        use ComponentKind::Vreg;
        let vreg = |extra: &[(&str, f64)]| {
            let mut p = vec![("vout", 3.3), ("dropout_v", 0.3), ("iq_a", 1e-3)];
            p.extend_from_slice(extra);
            entry(Vreg, &p)
        };
        for bad in [0.0, -1e-6, f64::NAN] {
            rejects(&vreg(&[("cout_recommended_f", bad)]), "cout_recommended_f");
        }
        accepts(&vreg(&[("cout_recommended_f", 22e-6)]));
        accepts(&vreg(&[
            ("cout_min_f", 1.5e-6),
            ("cout_recommended_f", 2.2e-6),
        ]));
        rejects(
            &vreg(&[("cout_min_f", 4.7e-6), ("cout_recommended_f", 1e-6)]),
            "less than or equal",
        );
        let mut typed = vreg(&[("cout_recommended_f", 22e-6)]);
        typed
            .params
            .set_str("cout_recommended_type", "solid tantalum");
        accepts(&typed);
        let mut stray_type = vreg(&[]);
        stray_type
            .params
            .set_str("cout_recommended_type", "solid tantalum");
        rejects(&stray_type, "beside 'cout_recommended_f'");
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
        // A temperature row must name its temperature, once per channel.
        let row = |at: &str| {
            format!("pin = \"vcc\"\nmin_v = 2.7\nmax_v = 3.6\nbasis = \"R\"\nconditions = \"{at}\"")
        };
        accepts(&fixture(&row("TA = −40 °C to 85 °C")));
        rejects(&fixture(&row("VCC = 3.3 V")), "must name a temperature");
        let twice = row("TA = 25 °C") + "\n[[envelope]]\nkind = \"supply_range\"\n";
        rejects(&fixture(&(twice + &row("TA=25°C"))), "once per channel");
    }

    #[test]
    fn a_sense_input_is_never_rated_as_a_supply() {
        let fixture = |pin: &str| -> ModelEntry {
            toml::from_str(&format!(
                "id = \"e\"\nkind = \"digital\"\n[params]\nidentity_only = true\nwarning = \"w\"\n\
                 unlocked_by = \"u\"\n[pins]\n\"1\" = \"vcc\"\n\"2\" = \"{pin}\"\n\
                 [[envelope]]\nkind = \"supply_range\"\npin = \"{pin}\"\nmin_v = -0.3\n\
                 max_v = 0.3\nabs_max_v = 0.5\nbasis = \"Absolute Maximum Ratings, sense row\""
            ))
            .unwrap()
        };
        for pin in ["isense", "vsense_a", "sns", "imon", "ain0"] {
            rejects(&fixture(pin), "sense or measurement input");
        }
        // An amplifier's differential input keeps its ground-referenced rating.
        accepts(&fixture("in_plus"));
    }

    #[test]
    fn input_tolerance_rates_inputs_only_with_an_ordered_ceiling() {
        let fixture = |body: &str| -> ModelEntry {
            toml::from_str(&format!(
                "id = \"t\"\nkind = \"digital\"\n[pins]\n\"1\" = \"vcc\"\n\"2\" = \"a\"\n\
                 \"3\" = \"y\"\n\"4\" = \"gnd\"\n[logic]\ninputs = [\"a\"]\noutputs = [\"y\"]\n\
                 [logic.comb]\ny = \"a\"\n[[envelope]]\nkind = \"input_tolerance\"\n{body}"
            ))
            .unwrap()
        };
        let ok = "pins = [\"a\"]\nmax_v = 5.5\nabs_max_v = 6.5\nbasis = \"ROC, VI row\"";
        accepts(&fixture(ok));
        rejects(&fixture(&ok.replace("[\"a\"]", "[\"y\"]")), "logic output");
        rejects(&fixture(&ok.replace("[\"a\"]", "[\"b\"]")), "b");
        rejects(&fixture(&ok.replace("[\"a\"]", "[]")), "at least one");
        rejects(&fixture(&ok.replace("6.5", "5.0")), "abs_max_v");
        rejects(
            &fixture(&ok.replace("max_v = 5.5", "max_v = -1.0")),
            "max_v",
        );
        rejects(
            &fixture("pins = [\"a\"]\nmax_v = 5.5\nbasis = \"R\"\nabs_max_basis = \"L\""),
            "abs_max_basis",
        );
        rejects(&fixture("pins = [\"a\"]\nmax_v = 5.5"), "basis");
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
        e.params.set_str("must_not_float_roles", "hold_n");
        e.params.set_str("float_tolerated_roles", "wp_n");
        accepts(&e);
        e.params.set_str("float_tolerated_roles", "nope");
        rejects(&e, "nope");
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

        // An SPDT's select must be `ctrl`, whatever the datasheet calls it.
        let mut sw = entry(ComponentKind::AnalogSwitch, &[("ron", 5.0), ("roff", 1e9)]);
        pins(&mut sw, &[("1", "com"), ("2", "s0"), ("3", "s1")]);
        sw.pins.insert("4".into(), "en_n".into());
        rejects(&sw, "s0 the throw joined to com while ctrl is LOW");
        sw.pins.insert("4".into(), "ctrl".into());
        accepts(&sw);
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
    fn params_only_keys_set_at_the_wrong_level_are_named_with_where_they_belong() {
        let root = |src: &str| toml::from_str::<toml::Value>(src).unwrap();
        let ok = "[[models]]\nid = \"a\"\n[models.params]\nidentity_only = true\nwarning = \"w\"";
        assert!(misplaced_param_keys(&root(ok)).is_empty());
        let entry = "[[models]]\nid = \"a\"\nidentity_only = true\n[models.params]\nx = 1";
        let found = misplaced_param_keys(&root(entry));
        assert_eq!(found.len(), 1);
        for needle in [
            "model 'a'",
            "`identity_only`",
            "[[models]] itself",
            "[models.params]",
        ] {
            assert!(found[0].contains(needle), "{needle}: {found:?}");
        }
        let nested =
            "[[models]]\nid = \"b\"\n[models.source]\nwarning = \"w\"\nunlocked_by = \"u\"";
        let found = misplaced_param_keys(&root(nested));
        assert!(
            found.len() == 2 && found[0].contains("[models.source]"),
            "{found:?}"
        );
    }

    /// Every shipped card passes the raw-TOML shape lint the way `models lint` runs it.
    #[test]
    fn shipped_cards_pass_the_raw_shape_lint() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("db");
        let mut checked = 0;
        for file in std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()) {
            if file.extension().is_none_or(|e| e != "toml") {
                continue;
            }
            let root: toml::Value =
                toml::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
            let found = [source_shape_findings(&root), misplaced_param_keys(&root)].concat();
            assert!(found.is_empty(), "{}: {found:?}", file.display());
            checked += 1;
        }
        assert!(checked > 5, "the db directory was not scanned");
    }

    /// No shipped card reads its pads from a column for another package body.
    #[test]
    fn shipped_cards_read_pads_from_their_own_package() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("db");
        let (mut flagged, mut compared) = (Vec::new(), 0);
        for file in std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()) {
            if file.extension().is_none_or(|e| e != "toml") {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            let Ok(db) = toml::from_str::<crate::schema::DbFile>(&text) else {
                continue;
            };
            for entry in &db.models {
                let column = own_pinout(entry).ok().flatten();
                compared += usize::from(column.is_some() && !card_packages(entry).is_empty());
                if let Some(why) = pinout_package_warning(entry) {
                    flagged.push(format!("{} {}: {why}", file.display(), entry.id));
                }
            }
        }
        assert!(flagged.is_empty(), "{}", flagged.join("\n"));
        assert!(compared >= 10, "only {compared} cards had both packages");
    }

    /// The CH340X card read its pads from the CH340K's ESSOP10 column: a column
    /// for another package body than the card's must warn, a matching or
    /// unrecognised one must not.
    #[test]
    fn a_pinout_column_for_another_package_than_the_card_warns() {
        let card = |description: &str, column: &str| -> ModelEntry {
            let text = LDO_FIXTURE.replace("LDO, SOT-23-5 package", description);
            let text = text.replacen(
                "package = \"SOT-23-5\"",
                &format!("package = \"{column}\""),
                1,
            );
            let cut = text.find("[[source.pinout]]\npackage = \"DFN").unwrap();
            toml::from_str(&text[..cut]).unwrap()
        };
        let warns =
            |description: &str, column: &str| pinout_package_warning(&card(description, column));
        assert!(warns("LDO, SOT-223", "SOT-23-5").is_some());
        let faulty = warns("CH340X bridge, MSOP-10", "ESSOP10").expect("ESSOP10 is not MSOP-10");
        assert!(
            faulty.contains("column or drawing for this order code's package"),
            "{faulty}"
        );
        for (description, column) in [
            ("CH340X bridge, MSOP-10", "MSOP-10 (CH340X)"),
            ("bridge, MSOP-10", "VSSOP-10 (DGS)"),
            ("LDO, SOT-23-5", "SOT-23-5 (DBV)"),
            ("LDO, SOT-23 package", "SOT-23-5"),
            ("LDO, SOIC-8", "SOIC / SOP-8"),
            ("LDO, QFN-16", "QFN-16 EP"),
            // An unknown package on either side never warns.
            ("bridge, MSOP-10", "CH340K body"),
            ("bridge, MSOP-10", "TI DGS"),
            ("an LDO", "ESSOP10"),
        ] {
            assert!(
                warns(description, column).is_none(),
                "{description} vs {column}"
            );
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

    /// The package a map is for bounds it: no pad past the package's count
    /// (one exposed pad allowed), a numbered column records every pad; and a
    /// converter's switch node is never its regulated output.
    #[test]
    #[rustfmt::skip]
    fn a_pin_map_fits_its_package_and_a_switch_node_is_never_the_output() {
        let card = |edits: &[(&str, &str)]| -> ModelEntry {
            let text = edits.iter().fold(LDO_FIXTURE.to_string(), |s, (a, b)| s.replacen(a, b, 1));
            toml::from_str(&text).unwrap()
        };
        assert_eq!(pin_package(&card(&[])), Some("SOT-23-5"));
        let sot_pins = "pins = { \"1\" = \"VIN\", \"2\" = \"GND\", \"3\" = \"EN\", \"4\" = \"NC\", \"5\" = \"VOUT\" }";
        let identity = "[params]\nidentity_only = true\nwarning = \"w\"\nunlocked_by = \"u\"";
        for (edits, needle) in [
            // A pad a SOT-23-5 does not have: read off a bigger package.
            (vec![("\"5\" = \"out\"", "\"5\" = \"out\"\n\"8\" = \"nc\"")], "maps pads 8 but a SOT-23-5 has 5 pins"),
            // A numbered column short of the package's pads.
            (vec![("\"4\" = \"NC\", ", ""), ("\"4\" = \"nc\"\n", "")], "records 4 pads but a SOT-23-5 has 5"),
            // The column prints SW where the map says out.
            (vec![("\"5\" = \"VOUT\" }", "\"5\" = \"SW\" }")], "that is the converter's switch node, never its regulated output"),
            (vec![("\"5\" = \"VOUT\" }", "\"5\" = \"LX\" }")], "Map it 'sw'"),
            // Nor is the feedback input the output.
            (vec![("\"5\" = \"VOUT\" }", "\"5\" = \"VFB\" }")], "that is the feedback input"),
        ] {
            rejects(&card(&edits), needle);
        }
        // An identity-only boost names its switch node 'sw' and needs no 'out'
        // pad: its regulated rail is past the diode, no pad of the IC.
        let boost = card(&[("\"5\" = \"out\"", "\"5\" = \"sw\""), ("\"5\" = \"VOUT\" }", "\"5\" = \"SW\" }"), ("[params]", identity)]);
        accepts(&boost);
        // A fixed part's FB/VOUT pin is its output.
        accepts(&card(&[("\"5\" = \"VOUT\" }", "\"5\" = \"FB/VOUT\" }")]));
        // A draft must say which package its map is for, read in full.
        // Its DFN-4 column numbers the pads differently, so the card is held to
        // its SOT-23-5 footprints (PACKAGE_SPLIT_RULE) before it stands.
        assert!(draft_pin_map_problems(&card(&[]))[0].contains(PACKAGE_SPLIT_RULE));
        let gate = ("[source]", "[match]\nfootprint_re = \"(?i)SOT-?23\"\n[source]");
        assert!(draft_pin_map_problems(&card(&[gate])).is_empty());
        let no_column = LDO_FIXTURE[..LDO_FIXTURE.find("[[source.pinout]]").unwrap()].replace("SOT-23-5 package", "");
        let no_column: ModelEntry = toml::from_str(&no_column).unwrap();
        assert!(draft_pin_map_problems(&no_column)[0].contains("records no [[models.source.pinout]] column"));
        let unread = card(&[gate, (sot_pins, &sot_pins.replace("\"EN\"", "\"unknown_pad_3\"").replace("\"NC\"", "\"unknown_pad_4\""))]);
        assert!(draft_pin_map_problems(&unread)[0].contains("records pads 3, 4 as unknown_pad_"));
        // Many unknown pads: advisory, naming why it matters.
        let vague = card(&[("\"3\" = \"en\"", "\"3\" = \"unknown_pad_3\""), ("\"4\" = \"nc\"", "\"4\" = \"unknown_pad_4\"")]);
        assert!(unknown_pads_warning(&vague).is_some_and(|w| w.contains("2 of 5 pads") && w.contains("supply checks")));
        assert!(unknown_pads_warning(&card(&[])).is_none());
    }

    fn drafted(toml: &str) -> ModelEntry {
        let db: crate::schema::DbFile = toml::from_str(toml).expect("a db file");
        db.models.into_iter().next().expect("an entry")
    }

    /// Round 3: a SOT-223 MOSFET drafted with lead letters under a drawn top
    /// view (ZXMP10A17G, "Pin Out - Top View" D/G/S). A package with no
    /// standard in the table keeps its letters (P7).
    #[test]
    fn letter_keyed_pins_on_a_standard_package_are_numbered() {
        let card = |package: &str| {
            drafted(&format!("[[models]]\nid = \"x\"\nkind = \"pmos\"\ndescription = \"P-channel MOSFET in {package}\"\n[models.pins]\n\"G\" = \"gate\"\n\"S\" = \"source\"\n\"D\" = \"drain\"\n[models.source]\ntier = \"datasheet-derived\"\n[[models.source.pinout]]\npackage = \"{package}\"\npins = {{ \"G\" = \"G\", \"S\" = \"S\", \"D\" = \"D\" }}\n"))
        };
        let problems = draft_pin_map_problems(&card("SOT223 (Type DN)"));
        assert!(
            problems[0].contains("lead letters (D, G, S)") && problems[0].contains("JEDEC TO-261"),
            "{problems:?}"
        );
        assert!(draft_pin_map_problems(&card("TO-247")).is_empty());
        // DMG1013T: a lettered SOT523 top view is numbered by the SC-75 outline.
        let sot523 = draft_pin_map_problems(&card("SOT523"));
        assert!(
            sot523[0].contains("lead letters (D, G, S)") && sot523[0].contains("SC-75"),
            "{sot523:?}"
        );
        assert_eq!(lead_standard("SOT-23").map(|s| s.0), Some("SOT-23"));
        assert_eq!(lead_standard("SOT-23-6"), None);
        assert_eq!(lead_standard("SOT523").map(|s| s.0), Some("SOT-523"));
        assert_eq!(
            lead_standard("Package_TO_SOT_SMD:SOT-416").map(|s| s.0),
            Some("SOT-416")
        );
        assert_eq!(lead_standard("SOT-323").map(|s| s.0), Some("SOT-323"));
        assert_eq!(lead_standard("SOT-363"), None);
        assert_eq!(lead_standard("SOT-353"), None);
    }

    /// Round 3: one 8-pin PDIP/SOIC map for an EEPROM whose SOT-23-6 numbers
    /// the same signals differently (24AA025UID Table 2-1). Gated by its
    /// footprint, the card holds only boards of its own package.
    #[test]
    fn one_map_for_several_numberings_is_refused() {
        let card = |gate: &str| {
            drafted(&format!("[[models]]\nid = \"x\"\nkind = \"digital\"\ndescription = \"EEPROM in PDIP/SOIC\"\n[models.match]\nvalue_re = '(?i)^24AA025UID'\n{gate}\n[models.pins]\n\"1\" = \"a0\"\n\"2\" = \"a1\"\n\"3\" = \"a2\"\n\"4\" = \"vss\"\n\"5\" = \"sda\"\n\"6\" = \"scl\"\n\"7\" = \"wp\"\n\"8\" = \"vcc\"\n[models.source]\ntier = \"datasheet-derived\"\n[[models.source.pinout]]\npackage = \"PDIP/SOIC\"\npins = {{ \"1\" = \"A0\", \"2\" = \"A1\", \"3\" = \"A2\", \"4\" = \"VSS\", \"5\" = \"SDA\", \"6\" = \"SCL\", \"7\" = \"WP\", \"8\" = \"VCC\" }}\n[[models.source.pinout]]\npackage = \"SOT-23-6\"\npins = {{ \"1\" = \"SCL\", \"2\" = \"VSS\", \"3\" = \"SDA\", \"4\" = \"A1\", \"5\" = \"A0\", \"6\" = \"VCC\" }}\n"))
        };
        let problems = draft_pin_map_problems(&card(""));
        assert!(
            problems[0].contains("SOT-23-6") && problems[0].contains("footprint_re"),
            "{problems:?}"
        );
        assert!(draft_pin_map_problems(&card("footprint_re = '(?i)(SOIC|SO|DIP)-?8'")).is_empty());
    }

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
        let (sot, standard) = ("package = \"SOT-23-5\"", |s: &str| format!("package = \"SOT-23-5\"\nstandard = \"{s}\""));
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
            (vec![(sot, standard("JEDEC MO-178 top view: 1 lower-left"))], ""),
            (vec![(sot, standard("JEDEC MO-178: 1 lower-left"))], "must name the view (\"top view\" or"),
            (vec![("package = \"DFN-4\"", "package = \"DFN-4\"\nstandard = \"bottom view\"".into())], "a card keyed by lead letters carries no standard"),
            (vec![(sot, standard("top view")), ("[source]", "[coverage]\nmissing = [\"pad_numbers\"]\n[source]".into())], "`pad_numbers` is not missing"),
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

    /// A strap table selects one row per resistance (a printed nominal within
    /// its window, or an open-ended range), nothing between rows, and refuses
    /// rows that overlap, unread params, and params the card already fixes.
    #[test]
    #[rustfmt::skip]
    fn a_strap_table_selects_one_row_and_refuses_ambiguity() {
        let table = |rows: &str| -> ModelEntry {
            toml::from_str(&format!("{LDO_FIXTURE}\n[[resistor_bins]]\npin = \"en\"\ntolerance = 0.05\n{rows}")).unwrap()
        };
        let row = |shape: &str, params: &str| format!("[[resistor_bins.bin]]\n{shape}\nparams = {{ {params} }}\n");
        let good = table(&[
            row("ohms = 10000.0", "vbat_reg_v = 4.2"),
            row("ohms = 20000.0", "vbat_reg_v = 4.35, input_limit_a = 0.5"),
            "[[resistor_bins.bin]]\nabove_ohms = 100000.0\nnote = \"disabled\"\n".into(),
        ].concat());
        accepts(&good);
        let bins = &good.resistor_bins[0];
        let pick = |ohms: f64| bins.select(ohms).map(crate::schema::ResistorBin::label);
        assert_eq!(pick(10_400.0).as_deref(), Some("10000 ohm"));
        assert_eq!(pick(19_100.0).as_deref(), Some("20000 ohm"));
        assert_eq!(pick(150_000.0).as_deref(), Some("> 100000 ohm"));
        assert_eq!(pick(15_000.0), None, "between rows selects nothing");
        assert_eq!(pick(0.0), None);
        assert_eq!(pick(f64::NAN), None);
        for (rows, needle) in [
            (row("ohms = 10000.0", "vbat_reg_v = 4.2") + &row("ohms = 10500.0", "vbat_reg_v = 4.1"), "overlap at tolerance"),
            (row("ohms = 10000.0", "frobnicate = 1.0"), "which no Vreg consumer reads"),
            (row("ohms = 10000.0", "vout = 5.0"), "already fixes"),
            (row("ohms = 10000.0\nbelow_ohms = 500.0", "vbat_reg_v = 4.2"), "exactly one positive"),
        ] {
            rejects(&table(&rows), needle);
        }
        let off_pin = toml::from_str(&format!("{LDO_FIXTURE}\n[[resistor_bins]]\npin = \"strap\"\ntolerance = 0.05\n{}", row("ohms = 1.0", "vbat_reg_v = 4.2"))).unwrap();
        rejects(&off_pin, "resistor_bins.pin");
    }
}
