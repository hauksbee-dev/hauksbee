//! The per-kind parameter-name vocabulary, and the warning-tier lint that flags
//! a name outside it as a probable typo.
//!
//! [`Params`](crate::schema::Params) is a free-form typed bag, deliberately: a
//! new device class must be expressible without a schema change, and
//! `validation.rs` bounds the values it recognises rather than the names. The
//! cost is that `rs_typ` where the binder reads `rs` is not wrong, it is
//! *absent*: the entry validates, resolves, binds, and silently runs on the
//! default series resistance. Nothing in the pipeline ever mentions the key
//! again.
//!
//! So this is a lint, not a schema. It names the vocabulary each kind's
//! consumers actually read and reports anything else as a probable typo, adding
//! the nearest known name when one is close enough to be worth naming (see
//! `nearest`, which stays silent rather than guess). It stays a WARNING and
//! never gates an exit code, because an unknown name is not provably wrong: a
//! genuine extension, or a param a future consumer will read, is a legitimate
//! entry that must still lint clean enough to ship.
//!
//! ## What counts as known
//!
//! Three sources, and a name from any of them passes:
//!
//! 1. The per-kind table below. It is curated, not derived: the starting point
//!    is the names the binder reads (`hauksbee-engine`'s `*_model_from` /
//!    `bind_*` functions), plus the concrete keys the datasheet-extraction
//!    prompt asks an author for (`datasheet::required_params_for_kind`). The
//!    second source matters, because several shipped params (`tpd_s`, `bits`,
//!    `supply_pin`) are read by no code at all and are documentation for the
//!    next author: warning about those would train people to ignore the lint.
//!    Where that prompt names a *quantity* rather than a key (it asks a passive
//!    for "its tolerance" and a self-resonant frequency without fixing a spelling)
//!    nothing is added here, because inventing a key name would be asserting an
//!    interface that does not exist. Such a name warns, which is the correct
//!    outcome: no consumer reads it.
//! 2. Any identifier the entry's own `[models.behavioral]` block references. A
//!    `Law::expr` and a `Transition::guard` bind "the param keys verbatim", so a
//!    model that computes `v_vplus / tie_ohms` has *defined* `tie_ohms` for
//!    itself. This is what keeps the lint sound on expression-driven models
//!    instead of second-guessing them.
//! 3. `<stem>_from_ref`, when `<stem>` is known AND the entry has a behavioral
//!    block: the binder rewrites that family into `<stem> = ohms(Rxx)` at bind
//!    time, so the suffix form is the same parameter reached from the board. The
//!    behavioral condition is not decoration, it is where the rewrite lives:
//!    `resolve_from_ref_params` sits past an early `continue` for an entry with
//!    no behavioral block, so `vout_from_ref` on a plain stamp-path vreg is read
//!    by nobody and has to warn.
//!
//! ## Where it lives / further reading
//!
//! Consumed by `hauksbee models lint` (`hauksbee-engine/src/commands/models.rs`).
//! Rationale: `docs/how-and-why/hauksbee-models/schema.md`.

use crate::schema::{Characteristic, ComponentKind, ModelEntry};
use hauksbee_ir::units::{format_percent, format_volts};

/// Names any kind may carry: resolve-report annotations, the explicit
/// identity-without-behavior state, and the free-text `warning` the binder
/// surfaces for an entry that models only part of its part.
/// `bind_opamp` and `bind_analog_switch` both read it, and a warning carrying
/// [`hauksbee_ir::evidence::Assumption::PARTIAL_MODEL_MARKER`] also becomes an
/// assumption on the evidence map, so it reaches `--plain` and `--json` and not
/// only the bind report.
const UNIVERSAL: &[&str] = &[
    "warning",
    "identity_only",
    "unlocked_by",
    "must_not_float_roles",
    "float_tolerated_roles",
    "float_tolerated_basis",
    "auto_bind",
    "auto_bind_family",
    "auto_bind_pin_summary",
    // Read by the board checks: the open-drain sink rating of an I2C pin
    // (`bus_loading`), the high-level input leakage of a logic input
    // (`pullup_effect`), and the audio-bus facts of an I2S/TDM part
    // (`audio_bus`).
    "iol_a",
    "iih_a",
    "i2s_clock_role",
    "mclk_required",
    "dout_hiz",
];

/// The SPICE junction params shared by the diode and the MOSFET body diode.
///
/// Deliberately only what `diode_model_from` reads. `db/diodes.toml`'s clamp
/// entries carry `vt_clamp` / `rd_clamp`, but those are referenced by their own
/// behavioral law expressions and are exempted through that route: listing them
/// here would let a plain stamp-path diode carry an unread `rd_clamp` in
/// silence, which is the exact failure this lint exists to catch.
const DIODE: &[&str] = &[
    "is", "n", "rs", "cjo", "vj", "m", "tt", "bv", "ibv", "xti", "eg",
];

/// Gummel-Poon params, plus `pair_count` for a dual-transistor package.
const BJT: &[&str] = &[
    "is",
    "bf",
    "br",
    "vaf",
    "var",
    "nf",
    "nr",
    "rb",
    "re",
    "rc",
    "irb",
    "rbm",
    "cje",
    "cjc",
    "tf",
    "tr",
    "ikf",
    "ikr",
    "ise",
    "ne",
    "isc",
    "nc",
    "xti",
    "eg",
    "pair_count",
];

/// Level-1 MOSFET params, the overlap/junction capacitances, and the
/// datasheet-Rds(on) split (`rd`/`rs`). `is` is the body diode's saturation
/// current, shared with [`DIODE`].
const MOSFET: &[&str] = &[
    "vto", "kp", "lambda", "gamma", "phi", "w_over_l", "n_sub", "cgs", "cgd", "is", "cbd", "cbs",
    "pb", "mj", "rd", "rs",
];

/// The converter and digital vocabularies, ordered so each kind's names are one
/// contiguous slice (see [`known_param_names`]): the converter names (resolution,
/// reference, output impedance, bus wiring), then `bits`, then the logic-level and
/// supply-accounting names every digital kind shares. `supply_pin`/`gnd_pin` name
/// pin roles rather than carrying physics.
///
/// An `adc` takes the converter names PLUS the digital ones, and that is not a
/// convenience union. `bind_component` routes `ComponentKind::Adc` through
/// `bind_digital`, which reads `voh`/`vol`/`vih`/`vil` (via
/// `LogicLevels::from_params`) and `supply_static_ua` exactly as it does for a
/// `digital` entry. Listing only the converter names made the lint report those as
/// "not read for this kind, so it is silently ignored", which is the opposite of
/// true: the engine reads them and the LINT was the thing that did not know. An
/// entry author who believed the warning would strip a bus level the solver wanted.
static CONVERTER_AND_DIGITAL: [&str; 25] = [
    "gain",
    "rout",
    "vref_int",
    "vref_ext",
    "lsb_size_v",
    "i2c_addr",
    "scl_pin",
    "sda_pin",
    "bits",
    "supply_pin",
    "gnd_pin",
    "vih",
    "vil",
    "voh",
    "vol",
    "ro",
    "ro_strong",
    "tpd_s",
    "supply_static_ua",
    "supply_cpd_pf",
    // Digital kinds only (past `ADC_TO`): the outputs only sink.
    "open_collector",
    // A digital part's enable input (a motor or gate driver's ENABLE): the
    // internal pull the datasheet prints, and the state it states for an open
    // pin, with its quote. The floating-enable check reads them.
    "en_pull_down_ohms",
    "en_pull_up_ohms",
    "en_open",
    "en_open_basis",
];

// Where each kind's slice of `CONVERTER_AND_DIGITAL` starts or ends; pinned by
// `converter_and_digital_splits_fall_on_their_named_boundaries`.
const SHIFT_REGISTER_FROM: usize = 8; // `bits`
const DIGITAL_FROM: usize = 9; // `supply_pin`
const DAC_TO: usize = 11; // through `gnd_pin`
const ADC_TO: usize = 19; // everything before `supply_cpd_pf`

/// A vocabulary slice plus the bus-limit names, which are spelled once in
/// `hauksbee_ir::peripheral_limits` and so are joined here rather than retyped.
fn with_bus_limits(base: &[&'static str]) -> Vec<&'static str> {
    base.iter()
        .copied()
        .chain(hauksbee_ir::peripheral_limits::BUS_LIMIT_PARAMS)
        .collect()
}

/// The digital vocabulary plus the datasheet bus-clock limits
/// (`max_sclk_hz`, `i2c_max_hz`) a peripheral card carries.
static DIGITAL_WITH_BUS_LIMITS: std::sync::LazyLock<Vec<&'static str>> =
    std::sync::LazyLock::new(|| with_bus_limits(&CONVERTER_AND_DIGITAL[DIGITAL_FROM..]));

/// The ADC vocabulary plus the same bus-clock limits (an I2C or SPI ADC).
static ADC_WITH_BUS_LIMITS: std::sync::LazyLock<Vec<&'static str>> =
    std::sync::LazyLock::new(|| with_bus_limits(&CONVERTER_AND_DIGITAL[..ADC_TO]));

/// A timer's names. The first half are datasheet quantities; the rest are the
/// numerical parameters of the composite stamp (see db/timer.toml), on the card
/// so the paired ngspice deck pins them where a reader can find them.
const TIMER: &[&str] = &[
    "threshold_frac",
    "trigger_frac",
    "divider_ohms",
    "rsat_ohms",
    "vol_v",
    "voh_drop_v",
    "reset_threshold_v",
    "timing_error_frac",
    "out_ohms",
    "sense_ohms",
    "comparator_gain",
    "latch_von",
    "latch_voff",
    "latch_hold",
];

/// The parameter names this kind's consumers read, beyond `UNIVERSAL`.
///
/// A kind with an empty slice carries no numeric physics at all (a connector
/// models pin continuity; `Ignore` models nothing), so every param on it is
/// unread by construction and the lint says so.
#[must_use]
pub fn known_param_names(kind: ComponentKind) -> &'static [&'static str] {
    match kind {
        ComponentKind::Passive => &[
            "ohms",
            "series_ohms",
            "value_override",
            "esr",
            "esl",
            // A crystal's specified load capacitance, shunt capacitance and
            // motional capacitance (the crystal load-capacitor check reads them).
            "cl_f",
            "c0_f",
            "c1_f",
        ],
        ComponentKind::Diode => DIODE,
        ComponentKind::BjtNpn | ComponentKind::BjtPnp => BJT,
        ComponentKind::Nmos | ComponentKind::Pmos => MOSFET,
        ComponentKind::Vreg => &[
            "vout",
            // The adjustable (ADJ-referenced) trio: the rail is the board's,
            // not the card's, so there is no `vout` on such an entry.
            "vref",
            "iadj_a",
            // The feedback pin's own current INTO the pin (signed, as the
            // datasheet prints it) on a ground-referenced adjustable part: it
            // flows on through the divider's top resistor and shifts the rail
            // by `ifb_a * Rtop`.
            "ifb_a",
            "dropout_v",
            "iq_a",
            "cout_min_f",
            "cout_recommended_f",
            "cout_recommended_type",
            "cout_esr_min_ohm",
            "cout_esr_max_ohm",
            "battery_chemistry",
            // A power-path charger's charge port (`bat`), beside its `out`
            // system rail: the float, the IN-to-BAT headroom, and the input
            // current limit that also caps the charge current.
            "vbat_reg_v",
            "bat_dropout_v",
            "input_limit_a",
            "vout_setpoint_low",
            "vout_setpoint_high",
            "shunt",
            "iref_a",
            "zka_ohms",
            "iktest_a",
            "ikmax_a",
            "ikmin_a",
            // A simple regulator's enable or shutdown pin (`en`/`shdn` role):
            // guaranteed-on and guaranteed-off levels against the part's GND,
            // which level turns the output on, the internal pull an open pin
            // follows, and the active output discharge while it is off.
            "en_on_v",
            "en_off_v",
            "en_on_level",
            "en_pull_down_ohms",
            "en_pull_up_ohms",
            "en_pull_down_a",
            "en_pull_up_a",
            "en_discharge_ohms",
            // The state the datasheet states for an open pin when it prints no
            // pull value (`"off"` or `"on"`).
            "en_open",
            // The datasheet's own words (with page) for that open-pin state,
            // quoted by the board check that finds the pin left open.
            "en_open_basis",
            // The supply current from IN while the enable holds the part off
            // (the datasheet's I_SHDN); the card's `iq_a` is the on-state figure.
            "i_shdn_a",
        ],
        // `swing_*`: output-swing headrooms from the V-/V+ PINS, which bind the
        // rails to the supply nets (`rail_*` then only a fallback).
        ComponentKind::Opamp => &[
            "gain", "pole_hz", "slew", "rail_lo", "rail_hi", "swing_lo", "swing_hi",
        ],
        // `out_lo`/`out_hi`/`hysteresis` are the push-pull stamp's (`swing_*`
        // put its levels on the supply nets, as for an op-amp); the last four
        // the open-collector stage's, whose levels come from the board instead.
        ComponentKind::Comparator => &[
            "out_lo",
            "out_hi",
            "swing_lo",
            "swing_hi",
            "hysteresis",
            "tpd_s",
            "supply_static_ua",
            "open_collector",
            "gain",
            "ron_ohms",
            "leak_s",
        ],
        ComponentKind::Timer => TIMER,
        ComponentKind::AnalogSwitch => &[
            "ron",
            "ron_s0",
            "ron_s1",
            "roff",
            "vth",
            // A load switch's enable pin (`ctrl`): the on level, the internal
            // pull or the open-pin state the datasheet prints, with its quote.
            "en_on_level",
            "en_pull_down_ohms",
            "en_pull_up_ohms",
            "en_pull_down_a",
            "en_pull_up_a",
            "en_open",
            "en_open_basis",
        ],
        ComponentKind::Digital => &DIGITAL_WITH_BUS_LIMITS[..],
        // A shift register is the digital vocabulary plus its stage count.
        ComponentKind::ShiftRegister => &CONVERTER_AND_DIGITAL[SHIFT_REGISTER_FROM..],
        ComponentKind::Dac => &CONVERTER_AND_DIGITAL[..DAC_TO],
        ComponentKind::Adc => &ADC_WITH_BUS_LIMITS[..],
        ComponentKind::Mcu => &[
            "backend",
            "module",
            "max_adc_source_impedance_ohms",
            "internal_clock",
            "default_clock_hz",
            // The datasheet's recommended range for each external crystal load
            // capacitor (C1, C2), read by the crystal load-cap check.
            "xtal_cap_min_f",
            "xtal_cap_max_f",
            // The datasheet's minimum resistance of the reset input's
            // permanent internal pull-up, read by the held-off check to
            // judge an external pull-down against it.
            "reset_pull_up_ohms",
        ],
        ComponentKind::Connector | ComponentKind::Ignore => &[],
    }
}

/// What each param without a unit suffix means, as `name unit meaning` (`-` is
/// dimensionless). A suffixed name (`iq_a`, `tpd_s`, `cout_min_f`) carries its
/// unit already, except `leak_s`: siemens, not seconds. The prompt shows these
/// to the author and [`source_warnings`] checks cited rows against the units.
const GLOSSARY: &str = "is A saturation current (a fitted law constant, not a leakage row); \
    n - emission coefficient; rs ohm series resistance; cjo F zero-bias junction capacitance; \
    vj V junction potential; m - grading coefficient; tt s transit time; \
    bv V reverse breakdown voltage; ibv A current at bv; eg eV bandgap; \
    xti - saturation-current temperature exponent; bf - forward beta (hFE); br - reverse beta; \
    nf - forward emission coefficient; nr - reverse emission coefficient; \
    vaf V forward Early voltage; var V reverse Early voltage; rb ohm base resistance; \
    irb A current where the base resistance falls halfway to rbm; rbm ohm high-current base resistance; \
    re ohm emitter resistance; rc ohm collector resistance; cje F zero-bias B-E capacitance; \
    cjc F zero-bias B-C capacitance; tf s forward transit time; tr s reverse transit time; \
    ikf A forward beta roll-off current; ise A B-E leakage saturation current; \
    ne - B-E leakage emission coefficient; vto V gate threshold; kp A/V^2 transconductance; \
    lambda 1/V channel-length modulation; cgs F gate-source capacitance; \
    cgd F gate-drain capacitance; cbd F drain-body capacitance; rd ohm drain resistance; \
    vout V fixed output setpoint; vref V reference voltage; gain - open-loop gain (V/V); \
    pole_hz Hz dominant pole; slew V/us slew rate; rail_lo V output rail (low); \
    rail_hi V output rail (high); swing_lo V headroom from V-; swing_hi V headroom from V+; \
    out_lo V output low level; out_hi V output high level; hysteresis V input hysteresis; \
    ron_ohms ohm output on-resistance (VOL / IOL); leak_s S output-off conductance (IOH / VOH); \
    ron ohm switch on-resistance; roff ohm switch off-resistance; vth V control threshold; \
    vih V input high threshold; vil V input low threshold; voh V output high level; \
    vol V output low level; ro ohm output resistance; \
    en_on_level - the enable-pin level (high or low) that turns a regulator on; \
    en_open - the state (off, on or undefined) the datasheet states for an open enable pin; \
    en_open_basis - the datasheet's words and page for the open enable pin's state; \
    cout_recommended_type - the output-capacitor type a recommended value names (\"solid tantalum\")";

fn glossary() -> impl Iterator<Item = [&'static str; 3]> {
    GLOSSARY.split(';').filter_map(|e| {
        let mut it = e.trim().splitn(3, ' ');
        Some([it.next()?, it.next()?, it.next()?])
    })
}

/// The unit `name` is documented in: its glossary entry, else its suffix.
#[must_use]
pub fn param_unit(name: &str) -> Option<&'static str> {
    glossary()
        .find(|[n, ..]| *n == name)
        .map(|[_, unit, _]| unit)
        .or_else(|| crate::units::suffix_unit(name).map(|(_, base)| base))
}

/// The column table: the printed column (`min`, `typ` or `max`) a row param
/// reads.
///
/// A param that IS a guaranteed limit reads its worst case: an input threshold
/// or output level (VIH/VOH min, VIL/VOL max), a headroom or dropout (max), and
/// any name carrying a `min` or `max` word. Everything else reads its nominal,
/// the typ.
#[must_use]
pub fn row_column(name: &str) -> &'static str {
    // A regulator enable's guaranteed-on level reads like a VIH (the min of its
    // row) and its guaranteed-off level like a VIL (the max), for the on-high
    // pins every card with such rows has.
    const MIN: &[&str] = &["vih", "voh", "out_hi", "rail_hi", "en_on_v"];
    const MAX: &[&str] = &[
        "vil",
        "vol",
        "out_lo",
        "rail_lo",
        "vol_v",
        "voh_drop_v",
        "en_off_v",
    ];
    const MAX_HEADROOM: &[&str] = &["dropout_v", "swing_lo", "swing_hi"];
    let has = |word| name.split('_').any(|w| w == word);
    if MIN.contains(&name) || has("min") {
        "min"
    } else if MAX.contains(&name) || MAX_HEADROOM.contains(&name) || has("max") {
        "max"
    } else {
        "typ"
    }
}

/// The factor from a row's printed `unit` to the base unit of param `name`
/// (its glossary or suffix unit), or none for a unitless param (`-` or empty).
fn row_scale(name: &str, unit: &str) -> Result<f64, String> {
    let want = param_unit(name).filter(|u| *u != "-");
    let scale = match want {
        _ if unit.trim().trim_matches('-') == want.unwrap_or("") => Some(1.0),
        Some(want) => crate::units::scale_between(unit, want),
        None => None,
    };
    scale.ok_or_else(|| {
        let want = want.map_or_else(|| "no unit".to_string(), |u| format!("{u}, by its name"));
        format!(
            "unit '{unit}' does not fit '{name}', which is read in {want}: keep a rail-relative \
             or other such row as a characteristic in its printed unit, never drop it"
        )
    })
}

/// The value the simulator reads from `row` as param `name`: the column
/// [`row_column`] picks, converted from the printed unit to the one its name
/// reads (`_ua` is micro-amperes).
///
/// # Errors
///
/// Fails when the row's printed unit cannot convert to the unit `name` reads, or
/// when the row prints neither the column `name` reads nor its fallback.
pub fn row_value(name: &str, row: &Characteristic) -> Result<f64, String> {
    let suffix = crate::units::suffix_unit(name).map_or(1.0, |(scale, _)| scale);
    let scale = row_scale(name, &row.unit)? / suffix;
    let midpoint = row.min.zip(row.max).map(|(a, b)| f64::midpoint(a, b));
    // A limit falls back to the typ; a nominal to the midpoint, never one bound.
    let (column, else_) = match row_column(name) {
        "min" => (row.min.or(row.typ), "the typ"),
        "max" => (row.max.or(row.typ), "the typ"),
        _ => (row.typ.or(midpoint), "the min/max midpoint"),
    };
    column.map(|v| v * scale).ok_or_else(|| {
        let reads = row_column(name);
        format!(
            "'{name}' reads the {reads} column, else {else_}, and the row prints neither: \
             keep the row as a characteristic, never drop it"
        )
    })
}

/// `row` as param `name`'s characteristic record, in the base unit of that
/// param, so its derived interval sits beside the declared ones.
pub(crate) fn row_characteristic(name: &str, row: &Characteristic) -> Option<Characteristic> {
    let base = row_scale(name, &row.unit).ok()?;
    let scaled = |v: Option<f64>| v.map(|v| v * base);
    Some(Characteristic {
        parameter: name.to_string(),
        min: scaled(row.min),
        typ: scaled(row.typ),
        max: scaled(row.max),
        unit: param_unit(name).unwrap_or_default().to_string(),
        ..row.clone()
    })
}

/// `name = meaning (unit)` for each glossary param `kind` reads, for the prompt.
#[must_use]
pub fn param_meanings(kind: ComponentKind) -> String {
    let names = known_param_names(kind);
    let lines: Vec<String> = glossary()
        .filter(|[n, ..]| names.contains(n))
        .map(|[n, unit, meaning]| format!("{n} = {meaning} ({unit})"))
        .collect();
    lines.join("; ")
}

/// Params the binder reads only in the other output form of this entry.
fn form_ignored(entry: &ModelEntry) -> &'static [&'static str] {
    match (entry.kind, entry.params.get_bool("open_collector")) {
        (ComponentKind::Comparator, Some(true)) => &["out_lo", "out_hi", "hysteresis"],
        (ComponentKind::Comparator, _) => &["ron_ohms", "leak_s"],
        _ => &[],
    }
}

/// Warnings that tie `[models.params]` to the card's own characteristic rows.
///
/// Rows are matched by value: a param equal to a row printed in another base
/// unit than the param's documented one (a current stored as a conductance),
/// and a nominal equal to the min or max of a row that prints no typ (unless
/// another row prints it as a typ).
///
/// The unit check only looks below 1e-3, where a copied current or capacitance
/// lives and a round volt or ohm value cannot collide.
#[must_use]
pub fn source_warnings(entry: &ModelEntry) -> Vec<String> {
    let rows = cited_rows(entry);
    let (mut out, mut derived) = (Vec::new(), derivations(entry));
    for (name, value) in &entry.params.0 {
        let (Some(v), Some(unit)) = (value.as_f64(), param_unit(name)) else {
            continue;
        };
        if v == 0.0
            || unit.contains('/')
            || crate::units::si_unit(unit).is_none_or(|(s, _)| s.to_bits() != 1.0_f64.to_bits())
        {
            continue;
        }
        let hits: Vec<_> = rows
            .iter()
            .filter(|row| !row.unit.contains('/'))
            .filter_map(|row| {
                let (scale, base) = crate::units::si_unit(&row.unit)?;
                let columns = [("min", row.min), ("typ", row.typ), ("max", row.max)];
                let column = columns.into_iter().find_map(|(column, x)| {
                    x.filter(|x| (x * scale - v).abs() <= 1e-9 * v.abs())
                        .map(|_| column)
                })?;
                Some((row, column, base))
            })
            .collect();
        let printed_typ = hits
            .iter()
            .any(|&(_, column, base)| column == "typ" && base == unit);
        for (row, column, base) in hits {
            let row_name = &row.parameter;
            if base != unit && v.abs() < 1e-3 {
                out.push(format!(
                    "param '{name}' is documented in {unit} but equals the {column} of \
                     characteristic '{row_name}' ({}): derive it in {unit}, do not copy the row",
                    row.unit
                ));
            } else if base == unit
                && !printed_typ
                && row.typ.is_none()
                // A bound param (VOL max) equalling the column it reads is its meaning.
                && row_column(name) != column
                && !derived.iter().any(|d| d.0 == name && (d.1 - v).abs() <= 1e-3 * v.abs())
            {
                out.push(format!(
                    "param '{name}' equals the {column} of characteristic '{row_name}', which \
                     prints no typ: a nominal is the typ, or the midpoint (min + max) / 2 of a \
                     row printing both, never a one-sided bound. If you derived the value from \
                     printed rows, start the row's basis with \"derived:\" and say how; if the \
                     datasheet gives no nominal, drop the param and add an `unknown` record"
                ));
            }
        }
    }
    out.extend(row_slips(entry, rows));
    out.extend(stated_abstentions(entry));
    out.extend(crate::output_load::unrepaired_level_warnings(entry));
    out.extend(law_warning(entry, rows).or_else(|| ron_miss(entry)));
    out.extend(gain_warning(entry, rows));
    out.extend(channel_slips(entry, rows));
    // The rows the completion step reads as ratings (`implied_rating`): the symbol
    // AND the key's unit, so a `PD_SCK` pulse width or a `mW/C` derating slope is none.
    let rating = |r: &&Characteristic| crate::derating::implied_rating(r).is_some();
    out.extend(rows.iter().filter(|r| r.rating.is_none()).filter(rating).map(|r| format!("characteristic '{}' prints a rating: tag it rating = \"<key>\" (max_current_a, max_power_w, theta_ja_c_per_w) with its temperature, package or mounting, so Hauksbee reads the row for the scenario's temperature", r.parameter)));
    derived.retain(|d| !entry.params.0.contains_key(d.0));
    for (n, v, how) in derived {
        out.push(format!(
            "param '{n}' is omitted but its cited row gives {n} = {v:e}, {how}"
        ));
    }
    out
}

/// An `unknown` record abstaining (`ABSTAIN: ...`) on a param the card states.
///
/// The two contradict: the evidence says the value cannot be known while the
/// simulator reads one. Measured: a PMOS draft kept the abstention its first,
/// failed attempt wrote beside the vto and kp its passing attempt fitted.
fn stated_abstentions(entry: &ModelEntry) -> Vec<String> {
    let records = entry.source.iter().flat_map(|s| &s.uncertainty);
    let abstains = records.filter_map(|u| match u {
        hauksbee_ir::evidence::ModelUncertainty::Unknown { parameter, reason }
            if reason.trim_start().starts_with("ABSTAIN") =>
        {
            Some(parameter)
        }
        _ => None,
    });
    let stated = abstains.filter(|name| entry.params.0.contains_key(name.as_str()));
    stated
        .map(|name| {
            format!(
                "[[models.source.uncertainty]] abstains on '{name}' while [models.params] states \
                 it: keep the param with its derivation and drop the abstention, or drop the \
                 param and keep the abstention, never both"
            )
        })
        .collect()
}

/// A param or current-program constant grossly off every row printing it. A
/// row prints `name` when its unit converts to the param's and any symbol in
/// its text (a run with no lowercase letter) is the name, its stem (less the
/// unit suffix) or, for a one-letter stem, that letter subscripted (`k_volts`
/// is KISET's K). Off: every column is a decade slip of 10^3 or more (over
/// 10^2.5), or the value is outside min..max and 3x or more past the nearer
/// bound. One agreeing row clears it.
fn row_slips(entry: &ModelEntry, rows: &[Characteristic]) -> Vec<String> {
    let program = toml::Table::try_from(&entry.current_program).unwrap_or_default();
    let named = entry.params.0.iter().map(|(k, v)| (k, v.as_f64()));
    let apart = |a: f64, b: f64| (a / b).abs().max((b / a).abs());
    let mut out = Vec::new();
    for (name, v) in named.chain(program.iter().map(|(k, v)| (k, v.as_float()))) {
        let (Some(v), Some(unit)) = (v.filter(|v| *v != 0.0), param_unit(name)) else {
            continue;
        };
        let words: Vec<&str> = name.split('_').collect();
        let stem =
            words[..words.len() - usize::from(crate::units::suffix_unit(name).is_some())].concat();
        let slip = |row: &Characteristic| {
            let symbols = row.parameter.split(|c: char| !c.is_alphanumeric());
            let symbols = symbols.filter(|s| !s.is_empty() && !s.chars().any(char::is_lowercase));
            let cited = symbols
                .map(str::to_lowercase)
                .any(|s| s == *name || s == stem || (stem.len() == 1 && s.starts_with(&stem)));
            let scale = crate::units::scale_between(&row.unit, unit).filter(|_| cited)?;
            let cols = [("typ", row.typ), ("min", row.min), ("max", row.max)];
            let cols = cols.map(|(c, x)| (c, x.unwrap_or(0.0)));
            let &(column, x) = cols.iter().find(|c| c.1 != 0.0)?;
            let decade = cols
                .iter()
                .all(|c| c.1 == 0.0 || apart(c.1 * scale, v) >= 316.0);
            let beyond = row.min.zip(row.max).is_some_and(|(a, b)| {
                let (lo, hi) = (a.min(b) * scale, a.max(b) * scale);
                (v < lo || v > hi) && apart(v, lo).min(apart(v, hi)) >= 3.0
            });
            let (p, ru, ratio) = (&row.parameter, &row.unit, apart(x * scale, v));
            let base = format!("{:.11e}", x * scale).parse().unwrap_or(x * scale);
            let why = ["beyond its printed min..max", "a power-of-ten slip"][usize::from(decade)];
            let message = format!("{name} = {v} but characteristic '{p}' prints {column} {x} {ru} = {base} {unit}: {ratio:.0}× apart, {why}; check the unit");
            Some((decade || beyond).then_some(message))
        };
        let slips = rows.iter().filter_map(slip).collect::<Option<Vec<_>>>();
        out.extend(slips.and_then(|s| s.into_iter().next()));
    }
    out
}

/// A diode is/n/rs over 10% off any VF row on the card (typ, else max), or
/// fitted to max rows alone without its basis saying so; or a bf outside every
/// hFE row.
/// Gain groups: a card its id or orderable codes pin to one (a row whose
/// parameter words end a code, `hFE group B` for `…B`) answers to that group's
/// rows; one pinning none, over hFE rows printed per group, must split.
fn law_warning(entry: &ModelEntry, rows: &[Characteristic]) -> Option<String> {
    if matches!(entry.kind, ComponentKind::BjtNpn | ComponentKind::BjtPnp) {
        let bf = entry.params.get_f64("bf")?;
        let hfe: Vec<_> = rows.iter().filter(|r| symbol(r, "HFE")).collect();
        let codes = entry.variant.iter().flat_map(|v| &v.orderable);
        let codes: Vec<String> = codes.chain([&entry.id]).map(|c| c.to_uppercase()).collect();
        let pins = |r: &&Characteristic| {
            let mut words = r.parameter.split(|c: char| !c.is_alphanumeric());
            words.any(|w| !w.is_empty() && codes.iter().any(|c| c.ends_with(&w.to_uppercase())))
        };
        let own: Vec<_> = hfe.iter().copied().filter(pins).collect();
        let conditions: std::collections::BTreeSet<_> = hfe.iter().map(|r| &r.conditions).collect();
        if own.is_empty() && entry.variant.is_none() && conditions.len() < hfe.len() {
            return Some(format!("hFE is printed per gain group and '{}' pins none: emit one [[models]] per group as orderable variants (orderable_re = its suffix), each bf its group's printed hFE row", entry.id));
        }
        let whose = ["its gain group's hFE row", "every hFE row"][usize::from(own.is_empty())];
        let hfe = if own.is_empty() { hfe } else { own };
        let off = |r: &&Characteristic| !(r.min.unwrap_or(bf)..=r.max.unwrap_or(bf)).contains(&bf);
        let off = !hfe.is_empty() && hfe.iter().all(off);
        return off.then(|| format!("bf = {bf} is outside the printed min..max of {whose}"));
    }
    let law = diode_law(entry).filter(|_| entry.kind == ComponentKind::Diode)?;
    let (miss, i, v) = worst_miss(law, &vf_points(rows));
    let vf: Vec<_> = rows.iter().filter(|r| symbol(r, "VF")).collect();
    let mut bases = entry.params.rows().map(|r| r.1).chain(vf.iter().copied());
    let hedged = bases.any(|r| r.basis.contains("guaranteed maxima"));
    let max_only = !vf.is_empty() && !hedged && vf.iter().all(|r| r.typ.is_none());
    let max_only = max_only.then(|| "diode is/n/rs are fitted to max VF rows alone: say so in their basis (law fitted to guaranteed maxima, a conservative stand-in for typical)".into());
    let at = diode_vf(law, i);
    (miss > 0.1).then(|| format!( "diode is/n/rs give VF = {} at IF = {i} A where a VF row on the card prints {v} V, over 10% off", format_volts(at))).or(max_only)
}

/// A `gain` 10 dB (3.16×) or more off every printed column of every cited
/// AOL/AVOL row, each converted to V/V by [`crate::units::gain_ratio`] (dB,
/// V/mV, V/µV): a unit slip `pole_hz = GBW / gain` hides by moving with it.
fn gain_warning(entry: &ModelEntry, rows: &[Characteristic]) -> Option<String> {
    let gain = entry.params.get_f64("gain").filter(|g| *g > 0.0)?;
    let aol = rows.iter().filter(|r| AOL.split(' ').any(|s| symbol(r, s)));
    let ratio = |r: &Characteristic, x: f64| crate::units::gain_ratio(x, &r.unit);
    let columns = aol.flat_map(|r| [r.typ, r.min, r.max].map(|x| Some((r, x?, ratio(r, x?)?))));
    let read: Vec<_> = columns.flatten().collect();
    let off = read.iter().all(|c| (c.2 / gain).max(gain / c.2) >= 3.16);
    let (r, x, v) = read.first().filter(|_| off)?;
    Some(format!("gain = {gain} V/V but cited '{}' prints {x} {} = {v:.4e} V/V: convert AOL to V/V (dB: 10^(dB/20); V/mV: ×1e3; V/µV: ×1e6), then pole_hz = GBW / gain", r.parameter, r.unit))
}

/// Open-loop gain symbols: the rows a `gain` is read from.
const AOL: &str = "AOL AVOL AVD AVO";

/// Channels a row's own label (its parameter and basis text) names: `IN1`,
/// `VOUT2`, `EN_1`, `CH2`, `channel B` (hex A to D, mod 9, read as 1 to 4),
/// never those its bias conditions mention.
fn channels(row: &Characteristic) -> Vec<u32> {
    static CHANNEL: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        let re = r"\b(?:(?i:channel|ch)\s*|(?:V?IN|V?OUT|EN|ILIM|FLT|FAULT)_?)([1-8A-D])\b";
        regex::Regex::new(re).expect("a valid pattern")
    });
    let label = format!("{} {}", row.parameter, row.basis);
    let index = |c: regex::Captures| u32::from_str_radix(&c[1], 16).ok().map(|d| d % 9);
    CHANNEL.captures_iter(&label).filter_map(index).collect()
}

/// A channel-indexed param (`ron_1`, `ilim2_a`) whose own row param, or every
/// cited row printing its value, is labelled for other channels only. One row
/// labelled for its channel clears it.
fn channel_slips(entry: &ModelEntry, rows: &[Characteristic]) -> Vec<String> {
    let slip = |name: &String| {
        let num = |w: &str| w.trim_start_matches(char::is_alphabetic).parse().ok();
        let k: u32 = name.split('_').find_map(num)?;
        let v = entry.params.get_f64(name).filter(|v| *v != 0.0)?;
        let prints = |r: &&Characteristic| {
            let scale = crate::units::si_unit(&r.unit).map_or(1.0, |s| s.0);
            let near = |x: &f64| (x * scale - v).abs() <= 1e-6 * v.abs();
            [r.min, r.typ, r.max].iter().flatten().any(near)
        };
        let own = entry.params.rows().find(|r| r.0 == name).map(|r| r.1);
        let named = rows.iter().filter(prints).chain(own);
        let named: Vec<_> = named.map(|r| (r, channels(r))).collect();
        let clear = !(1..=8).contains(&k) || named.iter().any(|c| c.1.contains(&k));
        let (r, c) = named.iter().find(|c| !c.1.is_empty()).filter(|_| !clear)?;
        Some(format!("param '{name}' is channel {k}'s but its row '{}' ({}) is labelled for channel {}: attribute each per-channel row by the channel its own label names", r.parameter, r.basis, c[0]))
    };
    entry.params.0.keys().filter_map(slip).collect()
}

/// Whether a row's parameter text has the word `sym` (`HFE` in "hFE gain",
/// `VBRR` in "V(BR)R breakdown"); `sym*` is any word it starts (`VZ*`: `VZnom`).
pub(crate) fn symbol(row: &Characteristic, sym: &str) -> bool {
    has_symbol(&row.parameter, sym)
}

/// [`symbol`] over any text.
pub(crate) fn has_symbol(text: &str, sym: &str) -> bool {
    let text = text.replace(['(', ')'], "");
    let (sym, prefix) = sym.strip_suffix('*').map_or((sym, false), |s| (s, true));
    let starts = |w: &str| {
        w.get(..sym.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(sym))
    };
    let mut words = text.split(|c: char| !c.is_alphanumeric());
    words.any(|w| {
        if prefix {
            starts(w)
        } else {
            w.eq_ignore_ascii_case(sym)
        }
    })
}

/// A diode its description names Schottky: it stores no charge, so trr gives no tt.
pub(crate) fn schottky(entry: &ModelEntry) -> bool {
    entry.kind == ComponentKind::Diode && entry.description.to_lowercase().contains("schottky")
}

/// A BJT `is` putting `nf·VT·ln(IC/is)` within 3% of a cited VBE(sat) row (a
/// "sat" symbol) at its IC and of no forward-active VBE row: fitted to saturation.
pub(crate) fn is_from_sat(entry: &ModelEntry) -> Option<String> {
    let bjt = matches!(entry.kind, ComponentKind::BjtNpn | ComponentKind::BjtPnp);
    let is = entry.params.get_f64("is").filter(|_| bjt)?;
    let vt = entry.params.get_f64("nf").unwrap_or(1.0) * hauksbee_ir::thermal_voltage_c(27.0);
    let miss = |r: &Characteristic| {
        let v = r.typ.or(r.max).or(r.min)?.abs() * crate::units::scale_between(&r.unit, "V")?;
        Some((vt * (quantity(&r.conditions, "I_?C", "A")? / is).ln() - v).abs() / v)
    };
    let vbe = |r: &&Characteristic| ["VBE", "VBEon", "VBEsat"].iter().any(|s| symbol(r, s));
    let fitted = cited_rows(entry)
        .iter()
        .filter(vbe)
        .filter(|r| miss(r).is_some_and(|m| m <= 0.03));
    let (sat, on): (Vec<_>, Vec<_>) =
        fitted.partition(|r| r.parameter.to_lowercase().contains("sat"));
    let row = sat.first().filter(|_| on.is_empty())?;
    Some(format!("is = {is:e} reproduces the saturation row '{}' ({}): fit is only to a forward-active VBE(on) row or curve at the lowest IC, else omit it with an `unknown` record", row.parameter, row.conditions))
}

pub(crate) fn cited_rows(entry: &ModelEntry) -> &[Characteristic] {
    entry.source.as_ref().map_or(&[], |s| &s.characteristic)
}

/// Whether a row is at 25 °C or states no temperature (`25 °C`, `25 ℃`, `25C`).
pub(crate) fn at_25(row: &Characteristic) -> bool {
    static CELSIUS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"([-+]?\d*\.?\d+)\s*(?:°\s*C|℃|C\b)").expect("a valid pattern")
    });
    let mut temperatures = CELSIUS.captures_iter(&row.conditions);
    temperatures.all(|t| t[1].parse() == Ok(25))
}

/// The magnitude of `sym = <number> <unit>` in `text`, in `unit` (`IR = 10 mA`);
/// `≈`, `:` or a space may stand for the `=` (`IF ≈ 10 mA`, `IF 10 mA`), and a
/// Unicode minus is read as the magnitude it negates (`VGS = −2.5 V`).
pub(crate) fn quantity(text: &str, sym: &str, unit: &str) -> Option<f64> {
    let re = format!(r"\b{sym}[\s=≈:−]+([-+]?\d*\.?\d+)\s*([a-zA-ZµμΩ]*)");
    let c = regex::Regex::new(&re).ok()?.captures(text)?;
    let scale = crate::units::scale_between(&c[2], unit).or_else(|| c[2].is_empty().then_some(1.0));
    Some(c[1].parse::<f64>().ok()?.abs() * scale?)
}

/// A param a cited row fixes by a standard relation: its name, value and working.
pub(crate) type Derived = (&'static str, f64, String);

/// Every param a cited row (25 °C or unstated) fixes by a standard relation, on
/// the card or not: BJT tf = 1/(2π·fT), cjc/cje from Cob/Cib; MOSFET cgd = Crss,
/// cgs = Ciss - Crss; diode cjo from CT, bv/ibv from V(BR) or VZ and its test
/// current, tt from trr = tt·ln(1 + IF/IR) (storage time; never a Schottky's). A
/// row (a typ-printing one first) reads its typ, else its min/max midpoint, else
/// its one printed bound, named in the working. A capacitance printed at reverse
/// bias V (the lowest printed) is taken to zero bias by C·(1 + V/vj)^m at the
/// junction's vj/m: the card's diode vj/m, else the binder's defaults. Nothing is
/// derived without its row.
pub(crate) fn derivations(entry: &ModelEntry) -> Vec<Derived> {
    let rows = cited_rows(entry);
    let find = |syms: &str, unit: &str| -> Vec<(f64, String, &Characteristic)> {
        let cited = |r: &&Characteristic| at_25(r) && syms.split(' ').any(|s| symbol(r, s));
        rows.iter()
            .filter(cited)
            .filter_map(|r| {
                let (col, x) = match (r.typ, r.min, r.max) {
                    (Some(t), ..) => ("typ", t),
                    (_, Some(a), Some(b)) => ("midpoint", f64::midpoint(a, b)),
                    (_, Some(a), _) => ("min", a),
                    (_, _, b) => ("max", b?),
                };
                let v = x.abs() * crate::units::scale_between(&r.unit, unit)?;
                let how = format!("'{}' {col} {x} {} ({})", r.parameter, r.unit, r.conditions);
                Some((v, how, r))
            })
            .collect()
    };
    // A row printing a typ is read before one printing only a bound (fT typ, not min).
    let first = |s, u| find(s, u).into_iter().min_by_key(|d| d.2.typ.is_none());
    let zero_bias = |name, syms, bias, (vj, m): (f64, f64)| {
        let at = |(c, how, r): (f64, String, &Characteristic)| {
            let v = quantity(&r.conditions, bias, "V");
            let at = v.map_or_else(
                || "no bias printed, read as zero bias".into(),
                |v| format!("taken from {v} V reverse bias by C·(1 + V/{vj})^{m}"),
            );
            let c0 = c * (1.0 + v.unwrap_or(0.0) / vj).powf(m);
            (v.unwrap_or(f64::INFINITY), c0, format!("{how}, {at}"))
        };
        let found = find(syms, "F").into_iter().map(at);
        let lowest = found.min_by(|a, b| a.0.total_cmp(&b.0));
        lowest.map(|(_, c0, how)| (name, c0, how))
    };
    let mut out: Vec<Derived> = Vec::new();
    match entry.kind {
        ComponentKind::BjtNpn | ComponentKind::BjtPnp => {
            let (d, tau) = (hauksbee_ir::BjtModel::default(), std::f64::consts::TAU);
            let ft = first("FT", "Hz")
                .map(|(f, w, _)| ("tf", (tau * f).recip(), format!("1/(2π·fT), {w}")));
            let cjc = zero_bias("cjc", "COB COBO CCB CCBO CC", "V_?CB", (d.vjc, d.mjc));
            let cje = zero_bias("cje", "CIB CIBO CEB CEBO CE", "V_?[EB]{2}", (d.vje, d.mje));
            out.extend([ft, cjc, cje].into_iter().flatten());
        }
        ComponentKind::Nmos | ComponentKind::Pmos => {
            if let Some((crss, how, _)) = first("CRSS", "F") {
                let ciss = find("CISS", "F").into_iter().find(|c| c.0 > crss);
                let cgs =
                    ciss.map(|(c, i, _)| ("cgs", c - crss, format!("Ciss - Crss, {i}; {how}")));
                out.extend(cgs);
                out.push(("cgd", crss, format!("Crss at its printed bias, {how}")));
            }
        }
        ComponentKind::Diode => {
            let (p, d) = (&entry.params, hauksbee_ir::DiodeModel::default());
            let junction = |k, default| p.get_f64(k).unwrap_or(default);
            let vj_m = (junction("vj", d.vj), junction("m", d.m));
            out.extend(zero_bias("cjo", "CT CJ CD CTOT", "V_?R", vj_m));
            if let Some((bv, how, r)) = first("VBR VBRR BV VZ*", "V") {
                let ibv = quantity(&r.conditions, "I_?(?:R|Z|ZT|T|BR)", "A");
                out.extend(ibv.map(|i| ("ibv", i, format!("the test current of {how}"))));
                out.push(("bv", bv, format!("V(BR) or VZ, {how}")));
            }
            if let Some((trr, how, r)) = first("TRR", "s").filter(|_| !schottky(entry)) {
                let current = |sym| quantity(&r.conditions, sym, "A").filter(|i| *i > 0.0);
                let ratio = current("I_?F").zip(current("I_?R")).map(|(f, r)| f / r);
                let assumed = ["", " (no IF/IR printed: IF = IR)"][usize::from(ratio.is_none())];
                let tt = trr / ratio.unwrap_or(1.0).ln_1p();
                out.push(("tt", tt, format!("trr / ln(1 + IF/IR){assumed}, {how}")));
            }
        }
        _ => {}
    }
    out.extend(crate::conventions::conventions(entry));
    let round = |v: f64| format!("{v:.4e}").parse().unwrap_or(v);
    let derived = |(n, v, how): Derived| (n, round(v), format!("derived: {n} = {how}"));
    out.into_iter().map(derived).collect()
}

fn diode_law(entry: &ModelEntry) -> Option<[f64; 3]> {
    let p = &entry.params;
    Some([p.get_f64("is")?, p.get_f64("n")?, p.get_f64("rs")?])
}

/// VF of `[is, n, rs]` at `i` amperes and the solver's nominal 27 °C.
pub(crate) fn diode_vf([is, n, rs]: [f64; 3], i: f64) -> f64 {
    (n * hauksbee_ir::thermal_voltage_c(27.0)).mul_add((i / is).ln_1p(), i * rs)
}

fn worst_miss(law: [f64; 3], points: &[Point]) -> (f64, f64, f64) {
    let miss = |&(i, v, _): &Point| ((diode_vf(law, i) - v).abs() / v, i, v);
    let worse = |a: (f64, f64, f64), b: (f64, f64, f64)| if b.0 > a.0 { b } else { a };
    points.iter().map(miss).fold((0.0, 0.0, 0.0), worse)
}

/// A printed row as a law checks it: `(bias, value, row)` in base units.
pub(crate) type Point<'a> = (f64, f64, &'a Characteristic);

/// Cited `(IF, VF)` in A and V, as [`at_bias`] reads VF rows.
pub(crate) fn vf_points(rows: &[Characteristic]) -> Vec<Point<'_>> {
    at_bias(rows, ["VF", "I_?F", "A", "V"])
}

/// Each `sym` row at 25 °C or unstated, with the positive `bias` its conditions
/// print in `of` (`I_?F` in A) and its typ, else max (the column a curated card
/// fits without typ), in `unit`. A law answers to every such row.
fn at_bias<'a>(rows: &'a [Characteristic], [sym, bias, of, unit]: [&str; 4]) -> Vec<Point<'a>> {
    let point = |r: &'a Characteristic| {
        let x = quantity(&r.conditions, bias, of).filter(|x| *x > 0.0 && at_25(r))?;
        let v = r.typ.or(r.max)? * crate::units::scale_between(&r.unit, unit)?;
        symbol(r, sym).then_some((x, v, r))
    };
    rows.iter().filter_map(point).collect()
}

/// A MOSFET law off an RDS(on) row (cited, or a param's own) at 25 °C or unstated
/// and the VGS it prints: small-VDS `1/(kp·w_over_l·(|VGS| - |vto|)) + rd + rs`
/// over 10% off its typ, or above a max-only row, a guaranteed bound. Lint warns
/// and a save refuses it, as a diode law off its VF rows.
pub(crate) fn ron_miss(entry: &ModelEntry) -> Option<String> {
    let p = &entry.params;
    let mos = matches!(entry.kind, ComponentKind::Nmos | ComponentKind::Pmos);
    let kp = p.get_f64("kp").filter(|_| mos)? * p.get_f64("w_over_l").unwrap_or(1.0);
    let vto = p.get_f64("vto")?.abs();
    let ohmic = p.get_f64("rd").unwrap_or(0.0) + p.get_f64("rs").unwrap_or(0.0);
    let own: Vec<_> = p.rows().map(|r| r.1.clone()).collect();
    let rows = [cited_rows(entry), &own].concat();
    at_bias(&rows, ["RDSon", "V_?GS", "V", "ohm"]).into_iter().find_map(|(vgs, v, r)| {
        let law = if vgs > vto { (kp * (vgs - vto)).recip() + ohmic } else { f64::INFINITY };
        let (x, typ) = (r.typ.or(r.max)?, r.typ.is_some());
        let [column, why] = if typ { ["typ", "over 10% off"] } else { ["max", "above that guaranteed bound"] };
        let off = if typ { (law - v).abs() > 0.1 * v } else { law > 1.001 * v };
        off.then(|| format!("kp/vto/rd/rs give RDS(on) = {:.4} {u} at |VGS| = {vgs} V where row '{}' ({}) prints {column} {x} {u}, {why}: the law 1/(kp*(|VGS| - |vto|)) + rd + rs must give every RDS(on) row; with rows at two or more VGS, kp comes from their difference and the residue at the higher VGS splits evenly into rd and rs", law * x / v, r.parameter, r.conditions, u = r.unit))
    })
}

/// What a MOSFET card's level-1 law rests on, as uncertainty records its
/// evidence carries.
///
/// - `vto`: a cited VGS(th) row (25 °C or unstated) printing min and max but no
///   typ is the threshold's declared range, so `vto` inside it is recorded as
///   that specification-limits interval, its basis saying where in the range
///   `vto` sits (the midpoint when no typ is printed). A nominal read from a
///   min/max-only row is never a silent typical value.
/// - `rds_on`: the law `1/(kp·w_over_l·(|VGS| - |vto|)) + rd + rs` against every
///   cited RDS(on) row (typ, else max), as the signed percentage error at each
///   VGS: the fit error of kp, rd and rs, an estimated range, never a bound.
///
/// Empty for any other kind, and for a card without the rows or params.
#[must_use]
pub fn mosfet_law_evidence(entry: &ModelEntry) -> Vec<hauksbee_ir::evidence::ModelUncertainty> {
    use hauksbee_ir::evidence::{ModelIntervalKind, ModelUncertainty};
    let p = &entry.params;
    let mos = matches!(entry.kind, ComponentKind::Nmos | ComponentKind::Pmos);
    let Some(vto) = p.get_f64("vto").filter(|_| mos) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let own: Vec<_> = p.rows().map(|r| r.1.clone()).collect();
    let rows = [cited_rows(entry), &own].concat();
    let threshold = |r: &&Characteristic| {
        (symbol(r, "VGSth") || symbol(r, "VTH")) && at_25(r) && r.typ.is_none()
    };
    // A `vto` written as the row itself already declares that row's interval.
    let as_row = matches!(p.0.get("vto"), Some(crate::schema::ParamValue::Row(_)));
    for row in rows.iter().filter(threshold).filter(|_| !as_row) {
        let Some(((a, b), scale)) = row
            .min
            .zip(row.max)
            .zip(crate::units::scale_between(&row.unit, "V"))
        else {
            continue;
        };
        let (low, high) = (a.min(b) * scale, a.max(b) * scale);
        // Compared by magnitude: a PMOS sheet may print VGS(th) unsigned.
        let (lo, hi, v) = (
            low.abs().min(high.abs()),
            low.abs().max(high.abs()),
            vto.abs(),
        );
        if !(lo - 1e-9..=hi + 1e-9).contains(&v) {
            continue;
        }
        let mid = f64::midpoint(lo, hi);
        let place = if (v - mid).abs() <= 1e-6 * mid.max(1e-9) {
            "is the min/max midpoint, no typ printed".to_string()
        } else {
            format!("is {v} V by magnitude inside it, no typ printed")
        };
        let basis = format!(
            "{} ({}), min and max columns: the threshold's declared range; vto = {vto} V {place}",
            row.parameter, row.basis
        );
        let record = ModelUncertainty::interval("vto", low, high, "V", basis);
        out.extend(record.ok());
        break;
    }
    let (Some(kp), true) = (p.get_f64("kp"), vto.is_finite()) else {
        return out;
    };
    let kp = kp * p.get_f64("w_over_l").unwrap_or(1.0);
    let ohmic = p.get_f64("rd").unwrap_or(0.0) + p.get_f64("rs").unwrap_or(0.0);
    let points = at_bias(&rows, ["RDSon", "V_?GS", "V", "ohm"]);
    let typ_at = |vgs: f64| {
        points
            .iter()
            .any(|(x, _, r)| r.typ.is_some() && (x - vgs).abs() < 1e-9)
    };
    let mut errors: Vec<(f64, f64, &str)> = Vec::new();
    for &(vgs, printed, row) in &points {
        let over = vgs - vto.abs();
        // A sheet printing typ and max at one VGS as two rows: the law answers the typ.
        let column = if row.typ.is_some() { "typ" } else { "max" };
        if over <= 0.0 || kp <= 0.0 || (column == "max" && typ_at(vgs)) {
            continue;
        }
        let law = (kp * over).recip() + ohmic;
        errors.push((vgs, 100.0 * (law - printed) / printed, column));
    }
    if errors.is_empty() {
        return out;
    }
    errors.sort_by(|a, b| b.0.total_cmp(&a.0));
    let (low, high) = errors
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(l, h), e| {
            (l.min(e.1), h.max(e.1))
        });
    let points: Vec<String> = errors
        .iter()
        .map(|(vgs, e, column)| format!("|VGS| {vgs} V {e:+.1}% vs {column}"))
        .collect();
    let basis = format!(
        "level-1 law 1/(kp*(|VGS| - |vto|)) + rd + rs (kp {kp}, vto {vto} V, rd + rs {ohmic} ohm) \
         against the cited RDS(on) rows: {}; the fit error at the printed rows, not a bound",
        points.join(", ")
    );
    let round = |v: f64| (v * 10.0).round() / 10.0;
    let record = ModelUncertainty::interval_with_kind(
        "rds_on",
        round(low),
        round(high),
        "%",
        ModelIntervalKind::EstimatedRange,
        basis,
    );
    out.extend(record.ok());
    out
}

/// Slope and intercept of `y` on `x`, by least squares.
fn line(x: &[f64], y: &[f64]) -> (f64, f64) {
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.iter().map(|_| 1.0).sum::<f64>();
    let (mx, my) = (mean(x), mean(y));
    let sxy: f64 = x.iter().zip(y).map(|(a, b)| (a - mx) * (b - my)).sum();
    let slope = sxy / x.iter().map(|a| (a - mx).powi(2)).sum::<f64>();
    (slope, my - slope * mx)
}

/// `VF = n·VT·ln(IF/is) + IF·rs` is linear in `ln IF` and `IF`: `rs` is the `IF`
/// coefficient by Frisch-Waugh (3+ points over two decades, ln 100 = 4.6, else 0),
/// then `VF - IF·rs` gives n and is. `None` unless physical and within 10%.
pub(crate) fn diode_fit(points: &[Point]) -> Option<[f64; 3]> {
    let (i, v): (Vec<f64>, Vec<f64>) = points.iter().map(|p| (p.0, p.1)).unzip();
    let ln: Vec<f64> = i.iter().map(|x| x.ln()).collect();
    let residual = |y: &[f64]| -> Vec<f64> {
        let (a, b) = line(&ln, y);
        y.iter().zip(&ln).map(|(y, x)| y - a * x - b).collect()
    };
    let (ri, rv) = (residual(&i), residual(&v));
    let fw =
        ri.iter().zip(&rv).map(|(a, b)| a * b).sum::<f64>() / ri.iter().map(|a| a * a).sum::<f64>();
    let spans = points.len() >= 3 && ln.iter().any(|a| ln.iter().any(|b| a - b >= 4.6));
    let rs = if spans { fw.max(0.0) } else { 0.0 };
    let drop: Vec<f64> = v.iter().zip(&i).map(|(v, i)| v - i * rs).collect();
    let (a, b) = line(&ln, &drop);
    let law = [(-b / a).exp(), a / hauksbee_ir::thermal_voltage_c(27.0), rs];
    let physical = (1e-30..=1e-3).contains(&law[0]) && (0.5..=3.0).contains(&law[1]);
    let main = (points.len() >= 2 && physical && worst_miss(law, points).0 <= 0.1).then_some(law);
    main.or_else(|| ideal_diode_fit(points))
}

/// The ideal-junction law (n = 1) through the lowest and highest printed `(IF, VF)`
/// when the free fit is unphysical, as two maxima over one decade give: the junction
/// slope `VT·ln(IF)` is the ideal one and the extra drop is series resistance, the
/// least assumption that reproduces both rows. `None` unless physical and within 10%.
fn ideal_diode_fit(points: &[Point]) -> Option<[f64; 3]> {
    let by_current = |a: &&Point, b: &&Point| a.0.total_cmp(&b.0);
    let (lo, hi) = (
        points.iter().min_by(by_current)?,
        points.iter().max_by(by_current)?,
    );
    let vt = hauksbee_ir::thermal_voltage_c(27.0);
    let rs = (hi.1 - lo.1 - vt * (hi.0 / lo.0).ln()) / (hi.0 - lo.0);
    let is = lo.0 * (-lo.0.mul_add(-rs, lo.1) / vt).exp();
    let law = [is, 1.0, rs];
    let physical = hi.0 > lo.0 && rs >= 0.0 && (1e-30..=1e-3).contains(&is);
    (physical && worst_miss(law, points).0 <= 0.1).then_some(law)
}

/// The fit an executable diode card must take: its law is absent or over 10% off
/// its own VF rows, and the fit is not.
pub(crate) fn diode_refit(entry: &ModelEntry) -> Option<[f64; 3]> {
    let identity = entry.params.get_bool("identity_only") == Some(true);
    let points = vf_points(cited_rows(entry));
    let fit = diode_fit(&points).filter(|_| entry.kind == ComponentKind::Diode && !identity)?;
    let fits = |law| worst_miss(law, &points).0 <= 0.1;
    (!diode_law(entry).is_some_and(fits)).then_some(fit)
}

/// [`diode_refit`] as the is/n/rs completion writes over the card's, with the working.
pub(crate) fn diode_refit_params(entry: &ModelEntry) -> Vec<Derived> {
    let (Some(fit), points) = (diode_refit(entry), vf_points(cited_rows(entry))) else {
        return Vec::new();
    };
    let miss = diode_law(entry).map(|law| worst_miss(law, &points).0);
    let off = miss.map_or_else(
        || "absent".into(),
        |m| format!("{} off", format_percent(100.0 * m, 0)),
    );
    let count = points.len();
    let how =
        format!("fitted: is/n/rs to the cited VF rows ({count} points), the card's law was {off}");
    let round = |v: f64| format!("{v:.4e}").parse().unwrap_or(v);
    let fitted = ["is", "n", "rs"].into_iter().zip(fit);
    fitted
        .map(|(name, v)| (name, round(v), how.clone()))
        .collect()
}

/// Why an identity-only card's own cited rows (25 °C or unstated, any printed
/// column) cover every `required` param its kind's checker asks for, or `None`.
/// A required param no row covers leaves the abstention legal unless
/// `may_abstain` lets an `unknown` record stand for it. Covering rows: VF rows
/// that fit a diode's is/n/rs; hFE for bf and VBE(on)-at-IC for is/nf (nf = 1);
/// VGS(th) for vto and RDS(on)-at-VGS or gFS-at-ID for kp; AOL for gain, VOL/VOH
/// for an op-amp's or comparator's low/high output, VHYS for hysteresis.
pub(crate) fn needless_abstention(
    entry: &ModelEntry,
    required: &[&str],
    may_abstain: impl Fn(&str) -> bool,
) -> Option<String> {
    use ComponentKind::{BjtNpn, BjtPnp, Comparator, Diode, Nmos, Opamp, Pmos};
    let rows = cited_rows(entry);
    // One VF row fixes `is` at the ideal n = 1, rs = 0 (`conventions`).
    let points = vf_points(rows);
    let fits = entry.kind == Diode && (points.len() == 1 || diode_fit(&points).is_some());
    // A required param a convention derives from the cited rows is covered too.
    let derived = derivations(entry);
    let derive = |key: &str| {
        let d = derived.iter().find(|d| key.split('|').any(|k| k == d.0))?;
        Some(format!("({})", d.2))
    };
    // Each need is `|`-separated alternatives `SYMBOLS@bias:unit`, the bias a
    // quantity the row's conditions must print.
    let need = |key: &str| match (entry.kind, key) {
        (Diode, _) if fits => Some("VF"),
        (BjtNpn | BjtPnp, "bf") => Some("HFE"),
        (BjtNpn | BjtPnp, "is" | "nf") => Some("VBE VBEon@I_?C:A"),
        (Nmos | Pmos, "vto") => Some("VGSth VTH"),
        (Nmos | Pmos, "kp") => Some("RDSon@V_?GS:V|GFS@I_?D:A"),
        (Opamp, "gain") => Some(AOL),
        (Opamp | Comparator, k) if k.ends_with("_lo") => Some("VOL"),
        (Opamp | Comparator, k) if k.ends_with("_hi") => Some("VOH"),
        (Comparator, "hysteresis") => Some("VHYS VHYST HYS"),
        _ => None,
    };
    let cite = |need: &str| {
        let found = need.split('|').find_map(|alt| {
            let (syms, bias) = alt.split_once('@').unwrap_or((alt, ""));
            let biased = |r: &Characteristic| {
                let b = bias.split_once(':');
                b.is_none_or(|(b, unit)| quantity(&r.conditions, b, unit).is_some())
            };
            let printed = |r: &Characteristic| r.typ.or(r.min).or(r.max).is_some();
            let sat = |r: &Characteristic| {
                syms.starts_with("VBE") && r.parameter.to_lowercase().contains("sat")
            };
            let named = |r: &Characteristic| syms.split(' ').any(|s| symbol(r, s)) && !sat(r);
            rows.iter()
                .find(|r| at_25(r) && printed(r) && biased(r) && named(r))
        });
        found.map(|r| format!("'{}'", r.parameter))
    };
    let mut cited = Vec::new();
    for key in required {
        match need(key).and_then(cite).or_else(|| derive(key)) {
            Some(row) => cited.push(row),
            None if key.split('|').any(&may_abstain) => {}
            None => return None,
        }
    }
    cited.sort();
    cited.dedup();
    let (cited, kind) = (cited.join(", "), serde_json::to_string(&entry.kind).ok()?);
    if cited.is_empty() {
        return None;
    }
    Some(format!("identity-only, but cited rows {cited} suffice for kind {kind} (typ-only or max-only rows are usable; Hauksbee fits and derives from them): write the model, naming any other missing fact in an `unknown` record"))
}

/// One parameter name outside the entry's vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownParam {
    /// The name as written in the TOML.
    pub name: String,
    /// The nearest known name, when one is close enough to be worth naming.
    pub suggestion: Option<String>,
}

impl UnknownParam {
    /// The warning line, phrased as a suspicion rather than a verdict: the lint
    /// cannot tell a typo from an extension, and saying "unused" would be a
    /// claim about the future.
    #[must_use]
    pub fn message(&self) -> String {
        let name = &self.name;
        let Some(s) = &self.suggestion else {
            return format!(
                "param '{name}' is not read for this kind or output form, so it is silently \
                 ignored; remove it or check the spelling"
            );
        };
        format!(
            "param '{name}' is not read for this kind or output form; did you mean '{s}'? \
             (unread params are silently ignored, so the default applies)"
        )
    }
}

/// Every parameter name on `entry` that no consumer of its kind reads.
///
/// Empty for a clean entry. See the module docs for the three ways a name
/// counts as known.
#[must_use]
pub fn unknown_params(entry: &ModelEntry) -> Vec<UnknownParam> {
    let vocab = known_param_names(entry.kind);
    let referenced = behavioral_identifiers(entry);

    let ignored = form_ignored(entry);
    let known = |name: &str| -> bool {
        (vocab.contains(&name) && !ignored.contains(&name))
            || UNIVERSAL.contains(&name)
            || referenced.contains(name)
    };

    // `<stem>_from_ref` counts only on the behavioral path (module docs, point 3),
    // but a suggestion is always for the stem: `voutt_from_ref` points at `vout`.
    let behavioral = !entry.behavioral.is_empty();
    let unknown = entry.params.0.keys().filter(|name| {
        let stem = name.strip_suffix("_from_ref").filter(|_| behavioral);
        !known(stem.unwrap_or(name))
    });
    let suggest = |name: &str| nearest(name.strip_suffix("_from_ref").unwrap_or(name), vocab);
    let unknown = unknown.map(|name| UnknownParam {
        name: name.clone(),
        suggestion: suggest(name),
    });
    unknown.collect()
}

/// The nearest name in `vocab` to `name`, or `None` when nothing is close.
///
/// Two signals, strongest first.
///
/// A known name that is a `_`-boundary prefix of what was written is almost
/// always the answer: the commonest real mistake is a datasheet-column suffix
/// glued onto the right stem (`rs_typ`, `cjo_max`, `gain_db`), and edit distance
/// handles that badly because each suffix character counts against it. The
/// longest such prefix wins, so `vref_int_v` suggests `vref_int` and not `vref`.
///
/// Otherwise edit distance, with a cutoff that scales with length. Names of
/// three characters or fewer get NO distance-based suggestion at all: the SPICE
/// vocabulary is full of two-letter names one edit apart (`rd`/`rs`/`re`,
/// `is`/`ise`, `vj`/`vf`), so a nearest match there is a coin flip presented as
/// help, and the warning already names the offending key. Longer names tolerate
/// the couple of characters a real slip costs.
fn nearest(name: &str, vocab: &[&str]) -> Option<String> {
    let prefix = vocab
        .iter()
        .filter(|k| name.strip_prefix(**k).is_some_and(|r| r.starts_with('_')))
        .max_by_key(|k| k.len());
    if let Some(k) = prefix {
        return Some((*k).to_string());
    }

    let cutoff = match name.chars().count() {
        0..=3 => return None,
        4..=6 => 2,
        _ => 3,
    };
    vocab
        .iter()
        .map(|k| (hauksbee_ir::levenshtein(name, k), *k))
        .filter(|(d, _)| *d <= cutoff)
        // Ties go to the alphabetically first name, so the suggestion is stable
        // across runs rather than dependent on table order.
        .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)))
        .map(|(_, k)| k.to_string())
}

/// Names the expression runtime binds ITSELF, after the params, so a param of
/// the same name is silently overwritten and can never be read.
///
/// Appearing in an expression therefore does NOT make one of these a parameter
/// read, and exempting them would hide the worst version of the bug this lint
/// looks for: a param that is not merely unused but actively clobbered.
/// `state_<name>` is prefix-matched, since the runtime derives one per FSM state.
const RUNTIME_OWNED: &[&str] = &["t", "temperature_c", "t_in_state", "state"];

/// The sandbox's builtin function names. A call in an expression is not a
/// parameter read either.
const EXPR_BUILTINS: &[&str] = &["if", "min", "max", "abs", "floor", "ceil", "round"];

/// Every identifier in the entry's behavioral expressions that could plausibly
/// be a parameter read: the law expressions and the FSM transition guards bind
/// "the param keys verbatim", so a name used there is defined by the entry
/// itself.
///
/// Two classes are excluded rather than harvested, because a name the runtime
/// owns is not a param read and exempting it produces a false negative:
///
/// * [`RUNTIME_OWNED`] (`t`, `t_in_state`, `state_<name>`) is bound AFTER the
///   params in `hauksbee-engine`'s expression context, so a param called `t` is
///   overwritten by simulation time on every evaluation. That is worse than
///   unused, and it must warn.
/// * [`EXPR_BUILTINS`] are functions.
///
/// Pin references (`v_<role>`) ARE still harvested. They are bound before the
/// params, so a param sharing a pin's name would shadow the voltage rather than
/// be lost, which is a different defect from the one this lint reports, and
/// warning on it here would be guessing at the author's intent.
fn behavioral_identifiers(entry: &ModelEntry) -> std::collections::BTreeSet<String> {
    let behavioral = &entry.behavioral;
    let guards = behavioral.fsm.iter().flat_map(|f| &f.transitions);
    let sources = behavioral.laws.iter().map(|law| &law.expr);
    let sources = sources.chain(guards.map(|t| &t.guard));
    let idents = sources.flat_map(|s| s.split(|c: char| !(c.is_alphanumeric() || c == '_')));
    // An identifier cannot start with a digit; that filters the numeric literals
    // out without parsing them.
    let literal = |i: &str| i.is_empty() || i.starts_with(|c: char| c.is_ascii_digit());
    let owned = |i: &str| RUNTIME_OWNED.contains(&i) || i.starts_with("state_");
    let runtime = |i: &&str| literal(i) || owned(i) || EXPR_BUILTINS.contains(i);
    idents.filter(|i| !runtime(i)).map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ModelEntry;

    fn entry(kind: &str, params: &str, extra: &str) -> ModelEntry {
        let src = format!(
            "[[models]]\nid = \"t\"\nkind = \"{kind}\"\ndescription = \"t\"\n[models.params]\n{params}\n{extra}\n"
        );
        let db: crate::schema::DbFile = toml::from_str(&src).expect("fixture parses");
        db.models.into_iter().next().expect("one entry")
    }

    fn law(expr: &str) -> String {
        format!(
            "[[models.behavioral.laws]]\nname = \"l\"\nkind = \"current\"\na = \"vplus\"\nb = \"gnd\"\nexpr = \"{expr}\"\n"
        )
    }

    /// A PMOS whose sheet prints VGS(th) min/max only and three RDS(on) typ rows
    /// (Diodes DMG1013T, DS31784 Rev. 7-2): its midpoint vto carries the
    /// declared threshold range, its law carries the fit error at each row, and
    /// an abstention left beside the stated vto and kp is flagged.
    #[test]
    fn a_mosfet_law_states_its_threshold_range_and_fit_error() {
        use hauksbee_ir::evidence::{ModelIntervalKind, ModelUncertainty};
        let row = |typ: f64, vgs: f64, id: f64| {
            format!(
                "[[models.source.characteristic]]\nparameter = \"RDS(ON) Static Drain-Source On-Resistance\"\n\
                 typ = {typ}\nunit = \"ohm\"\nconditions = \"VGS = {vgs} V, ID = {id} mA; TA = +25°C\"\n\
                 basis = \"Electrical Characteristics, RDS(ON), typ column\"\n"
            )
        };
        let source = format!(
            "[models.source]\ntier = \"datasheet-derived\"\nvalidation = \"physical-bounds-only\"\n\
             [[models.source.uncertainty]]\nstatus = \"unknown\"\nparameter = \"vto\"\n\
             reason = \"ABSTAIN: VGS(TH) has min/max but no typical value\"\n\
             [[models.source.characteristic]]\nparameter = \"VGS(TH) Gate Threshold Voltage\"\n\
             min = -0.5\nmax = -1.0\nunit = \"V\"\nconditions = \"VDS = VGS, ID = -250 μA\"\n\
             basis = \"Electrical Characteristics, VGS(TH), min/max columns\"\n{}{}{}",
            row(0.5, -4.5, -350.0),
            row(0.7, -2.5, -300.0),
            row(1.0, -1.8, -150.0)
        );
        let card = entry(
            "pmos",
            "vto = -0.75\nkp = 1.25\nrd = 0.15\nrs = 0.15",
            &source,
        );
        let records = mosfet_law_evidence(&card);
        let [ModelUncertainty::Interval {
            parameter,
            low,
            high,
            kind,
            basis,
            ..
        }, ModelUncertainty::Interval {
            parameter: law,
            low: lo,
            high: hi,
            kind: law_kind,
            basis: law_basis,
            ..
        }] = &records[..]
        else {
            panic!("two intervals: {records:?}");
        };
        assert_eq!((parameter.as_str(), *low, *high), ("vto", -1.0, -0.5));
        assert_eq!(*kind, ModelIntervalKind::SpecificationLimits);
        assert!(basis.contains("midpoint, no typ printed"), "{basis}");
        // 1/(1.25*(4.5-0.75)) + 0.3 = 0.5133 (+2.7%); at 2.5 V 0.7571 (+8.2%);
        // at 1.8 V 1.0619 (+6.2%).
        assert_eq!((law.as_str(), *lo, *hi), ("rds_on", 2.7, 8.2));
        assert_eq!(*law_kind, ModelIntervalKind::EstimatedRange);
        assert!(
            law_basis.contains("|VGS| 1.8 V +6.2% vs typ"),
            "{law_basis}"
        );
        let warned = source_warnings(&card);
        assert!(
            warned
                .iter()
                .any(|w| w.contains("abstains on 'vto' while [models.params] states")),
            "{warned:?}"
        );
        // An NMOS with no threshold or RDS(on) rows states nothing it cannot cite.
        assert!(mosfet_law_evidence(&entry("nmos", "vto = 1\nkp = 0.1", "")).is_empty());
    }

    #[test]
    fn typos_are_reported_with_a_suggestion_only_when_it_is_safe() {
        assert!(
            unknown_params(&entry("diode", "is = 1e-9\nn = 1.8\nrs = 6.0\nm = 0.5", "")).is_empty()
        );

        let found = unknown_params(&entry("diode", "cjo_typ = 4e-12", ""));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "cjo_typ");
        assert_eq!(found[0].suggestion.as_deref(), Some("cjo"));
        assert!(found[0].message().contains("cjo"));

        // No near neighbour, and a short unknown name (`vf` is one edit from `vj`,
        // a different physical quantity): warn, but do not guess.
        for (kind, p) in [
            ("analog_switch", "thermal_pad_area_mm2 = 12.0"),
            ("diode", "vf = 2.0"),
        ] {
            let found = unknown_params(&entry(kind, p, ""));
            assert_eq!(found.len(), 1, "{p}");
            assert_eq!(found[0].suggestion, None, "{found:?}");
        }
    }

    #[test]
    fn law_expressions_define_their_own_params_but_not_runtime_names() {
        let e = entry("digital", "tie_ohms = 100.0", &law("v_vplus / tie_ohms"));
        assert!(unknown_params(&e).is_empty(), "{:?}", unknown_params(&e));

        let e = entry(
            "digital",
            "t = 1.0\nstate_on = 2.0\nmin = 3.0",
            &law("min(t, state_on)"),
        );
        let names: Vec<String> = unknown_params(&e).into_iter().map(|u| u.name).collect();
        for expected in ["t", "state_on", "min"] {
            assert!(names.iter().any(|n| n == expected), "{expected}: {names:?}");
        }
    }

    /// `<stem>_from_ref` follows its stem only where the binder rewrites it: on
    /// a behavioral entry. On a plain stamp-path entry nothing reads it.
    #[test]
    fn the_from_ref_suffix_follows_its_stem_only_on_a_behavioral_entry() {
        let extra = "[[models.behavioral.laws]]\nname = \"l\"\nkind = \"current\"\na = \"out\"\nb = \"gnd\"\nexpr = \"0.0\"\n";
        assert!(unknown_params(&entry("vreg", "vout_from_ref = \"R1\"", extra)).is_empty());
        for (params, extra) in [
            ("voutt_from_ref = \"R1\"", extra),
            ("vout_from_ref = \"R1\"", ""),
        ] {
            let found = unknown_params(&entry("vreg", params, extra));
            assert_eq!(found.len(), 1, "{params}: {found:?}");
            assert_eq!(found[0].suggestion.as_deref(), Some("vout"), "{found:?}");
        }
    }

    #[test]
    fn output_form_units_and_cited_rows_are_cross_checked() {
        let oc = "open_collector = true\nron_ohms = 30.0\nleak_s = ";
        for (params, unread) in [
            ("out_lo = 0.0\nout_hi = 5.0\nhysteresis = 0.0", None),
            ("out_lo = 0.0\nout_hi = 5.0\nleak_s = 1e-9", Some("leak_s")),
            (
                &format!("{oc}2e-11\nhysteresis = 0.001"),
                Some("hysteresis"),
            ),
        ] {
            let found = unknown_params(&entry("comparator", params, ""));
            assert_eq!(found.first().map(|u| u.name.as_str()), unread, "{params}");
        }
        assert_eq!(
            ["leak_s", "tpd_s", "cjo", "iq_a", "slew"].map(param_unit),
            [Some("S"), Some("s"), Some("F"), Some("A"), Some("V/us")]
        );
        assert!(param_meanings(ComponentKind::Diode)
            .contains("cjo = zero-bias junction capacitance (F)"));

        let kp = "[models.current_program]\npin = \"p\"\nsemantics = \"regulated_current\"\nequation = \"inverse_resistance\"\nk_volts = ";
        for case in [
            "comparator | OC1e-10 | max = 0.1 | nA | documented in S",
            "comparator | OC2e-11 | max = 0.1 | nA | ",
            "vreg | vout = 0.5 | max = 0.5 | V | prints no typ",
            "vreg | dropout_v = 0.5 | min = 0.5, max = 0.9 | V | no typ",
            "vreg | dropout_v = 0.5 | max = 0.5 | V | ",
            "vreg | dropout_v = 0.3 | typ = 0.3, max = 0.5 | V | ",
            "vreg | dropout_v = 0.3 | min = 0.1, max = 0.5 | V | ",
            "digital | voh = 2.4 | min = 2.4 | V | ",
            "diode | rs = 1.0 | max = 1.0 | V | ",
            "vreg | KP0.54 | KISET k: min = 490, typ = 540, max = 590 | AΩ | = 540 V: 1000× apart",
            "vreg | KP0.54 | ISET current factor KISET: typ = 540 | AΩ | = 540 V: 1000× apart",
            "vreg | iq_a = 6.5e-5 | ISD shutdown IQSD: typ = 0.05 | uA | ",
            "vreg | KP540 | KISET k: min = 490, typ = 540, max = 590 | AΩ | ",
            "vreg | KP0.52 | KISET k: typ = 520 | mA·kΩ | typ 520 mA·kΩ = 520 V: 1000× apart",
            "vreg | vout = 0.0033 | VOUT out: typ = 3.3 | V | 1000× apart, a power-of-ten slip",
            "vreg | vout = 0.0033 | VOUT out: typ = 3.3 | ppm/°C | ",
            "vreg | vout = 0.0033 | VIN in: typ = 3.3 | V | ",
            "vreg | iq_a = 6.5e-6 | IQ q: min = 50, max = 90 | uA | 50 uA = 0.00005 A: 8× apart, beyond",
            "vreg | iq_a = 6.5e-5 | IQ q: min = 50, max = 90 | uA | ",
            "diode | rs = 1.9 | RS series: typ = 0.8, max = 2.5 | Ω | ",
        ] {
            let [kind, params, cols, unit, want] = case.split(" | ").collect::<Vec<_>>()[..] else {
                panic!("{case}")
            };
            let (name, cols) = cols.split_once(": ").unwrap_or(("ROW", cols));
            let source = cited(&format!("{name} ; {cols} ; {unit} ; c"));
            let params = params.replace("OC", oc).replace("KP", kp);
            let found = source_warnings(&entry(kind, &params, &source));
            let all = found.iter().all(|w| w.contains(want));
            assert!(
                all && found.len() == usize::from(!want.is_empty()),
                "{case}: {found:?}"
            );
        }
    }

    /// VF rows cut from a known law refit to it; a bf outside hFE warns.
    #[test]
    fn a_drafted_law_is_checked_against_its_cited_rows() {
        let law = [1e-8, 1.2, 0.5];
        #[rustfmt::skip]
        let vf = |ma: f64| format!("VF forward ; typ = {} ; V ; IF = {ma} mA, TA = 25 °C", diode_vf(law, ma / 1e3));
        let rows = cited(&[1.0, 10.0, 100.0, 1000.0].map(vf).join(" + "));
        let drafted = entry("diode", "is = 1e-8\nn = 1.2\nrs = 0.5", &rows);
        assert!(source_warnings(&drafted).is_empty() && diode_refit(&drafted).is_none());
        let off = entry("diode", "is = 1e-14\nn = 1.0\nrs = 0.0", &rows);
        let fit = diode_refit(&off).expect("the rows fit their own law");
        assert!(
            fit.iter().zip(law).all(|(a, b)| (a / b - 1.0).abs() < 1e-3),
            "{fit:?}"
        );
        assert!(source_warnings(&off)[0].ends_with("over 10% off"));
        let hfe =
            |(g, a, b): (&str, u32, u32)| format!("hFE {g} ; min = {a}, max = {b} ; - ; IC = 2 mA");
        let one = cited(&hfe(("DC current gain", 110, 220)));
        #[rustfmt::skip]
        let groups = cited(&[("group A", 110, 220), ("group B", 200, 450), ("group C", 420, 800)].map(hfe).join(" + "));
        #[rustfmt::skip]
        let cases = [
            // (case, id, a variant member, rows, bf) -> the warning, or none
            ("one row, inside", "t", false, &one, 200.0, ""),
            ("one row, outside", "t", false, &one, 300.0, "bf = 300 is outside the printed min..max of every hFE row"),
            ("group B, inside its row", "xyz847b", true, &groups, 300.0, ""),
            ("group B, in group A's row only", "xyz847b", true, &groups, 150.0, "bf = 150 is outside the printed min..max of its gain group's hFE row"),
            ("a member no row names, outside every group", "xyz847q", true, &groups, 900.0, "of every hFE row"),
            ("no group pinned over per-group rows", "xyz847", false, &groups, 300.0, "hFE is printed per gain group and 'xyz847' pins none"),
        ];
        for (case, id, member, rows, bf, want) in cases {
            let mut e = entry("bjt_npn", &format!("is = 1e-14\nbf = {bf}\nnf = 1.0"), rows);
            (e.id, e.variant) = (id.into(), member.then(Default::default));
            let found = source_warnings(&e);
            assert!(
                found.len() == usize::from(!want.is_empty())
                    && found.iter().all(|w| w.contains(want)),
                "{case}: {found:?}"
            );
        }
    }

    /// A source block of `+`-joined rows, each `parameter ; columns ; unit ;
    /// conditions`, then `; basis` if not `b`.
    fn cited(rows: &str) -> String {
        let row = |spec: &str| {
            let [p, cols, unit, conditions, ref basis @ ..] =
                spec.split(" ; ").collect::<Vec<_>>()[..]
            else {
                panic!("{spec}")
            };
            let basis = basis.first().unwrap_or(&"b");
            let (cols, head) = (cols.replace(", ", "\n"), "[[models.source.characteristic]]");
            let tail =
                format!("unit = \"{unit}\"\nconditions = \"{conditions}\"\nbasis = \"{basis}\"\n");
            format!("{head}\nparameter = \"{p}\"\n{cols}\n{tail}")
        };
        let rows: String = rows.split(" + ").map(row).collect();
        format!("[models.source]\ntier = \"datasheet-derived\"\n{rows}")
    }

    /// Each flagged beside a legal control: a diode law over 10% off a printed VF
    /// row (typ, else max), or on max rows alone with no hedge in a basis; a MOSFET
    /// law fitted to one of two RDS(on) typ rows, or above a max-only one; a gain off its
    /// AOL row once dB, V/mV or V/V is converted; a channel's param on another
    /// channel's row. Case: kind | params | rows | warning (empty: clean).
    #[test]
    #[rustfmt::skip]
    fn diode_fits_gain_unit_slips_and_swapped_channel_rows_are_flagged() {
        let vf = |col: &str, ma: f64| format!("VF ; {col} = {} ; V ; IF = {ma} mA", diode_vf([1e-9, 1.8, 0.2], ma / 1e3));
        let (law, ron) = ("is = 1e-9\nn = 1.8\nrs = 0.2", "RON IN1 to OUT1 ; typ = 90 ; mΩ ; VIN2 = 5 V");
        let hedge = "Table 2, max; law fitted to guaranteed maxima, a conservative stand-in for typical";
        let ilim = "ILIM channel A ; typ = 1.5 ; A ; - + ILIM channel B ; typ = 2 ; A ; -";
        let rds = "RDS(on) ; typ = 38.57 ; mΩ ; VGS = -4.5 V, ID = -2.8 A + RDS(on) ; typ = 76.67 ; mΩ ; VGS = −2.5 V, ID = -2 A";
        for case in [
            format!("diode | {law} | {} ; {hedge} + {} | ", vf("max", 1000.0), vf("max", 3000.0)),
            format!("diode | {law} | {} + {} | fitted to max VF rows alone", vf("max", 1000.0), vf("max", 3000.0)),
            format!("diode | {law} | {} + {} + VF ; max = {} ; V ; IF = 1 A | IF = 1 A where a VF row", vf("typ", 10.0), vf("typ", 100.0), 1.2 * diode_vf([1e-9, 1.8, 0.2], 1.0)),
            format!("diode | is = 2.6e-4\nn = 2.0\nrs = 0.2 | {} ; {hedge} + {} ; {hedge} | over 10% off", vf("max", 1000.0), vf("max", 3000.0)),
            format!("diode | {law} | {} + {} | ", vf("max", 1.0), [1.0, 10.0, 100.0].map(|i| vf("typ", i)).join(" + ")),
            format!("pmos | vto = -1\nkp = 7.4077 | {rds} | at |VGS| = 2.5 V where row 'RDS(on)' (VGS = −2.5 V, ID = -2 A) prints typ 76.67 mΩ, over 10% off"),
            format!("pmos | vto = -1\nkp = 10\nrd = 0.005\nrs = 0.005 | {rds} | "),
            "pmos | vto = -1\nkp = 7.4077 | RDS(on) ; max = 50 ; mΩ ; VGS = -4.5 V | ".into(),
            "nmos | vto = 1\nkp = 5 | RDS(on) ; max = 50 ; mΩ ; VGS = 4.5 V | prints max 50 mΩ, above that guaranteed bound".into(),
            "opamp | gain = 1e8 | AOL ; typ = 100 ; dB ; RL = 2 kΩ | prints 100 dB = 1.0000e5 V/V".into(),
            "opamp | gain = 1e5 | AOL ; min = 90, typ = 100 ; dB ; RL = 2 kΩ | ".into(),
            "opamp | gain = 2e8 | AVOL ; min = 50, typ = 200 ; V/mV ; - | prints 200 V/mV = 2.0000e5 V/V".into(),
            "opamp | gain = 6e4 | AVOL ; min = 50, typ = 200 ; V/mV ; - | ".into(),
            "comparator | gain = 2e2 | AVD gain ; typ = 2e5 ; V/V ; - | convert AOL to V/V".into(),
            "comparator | gain = 2e5 | AVD gain ; typ = 200 ; V/uV ; - + AOL ; typ = 2e5 ; V/V ; - | ".into(),
            format!("analog_switch | ron_1 = 0.12 | {ron} + RON IN2 to OUT2 ; typ = 120 ; mΩ ; VIN1 = 5 V | channel 1's but its row 'RON IN2 to OUT2' (b) is labelled for channel 2:"),
            format!("analog_switch | ron_1 = 0.09 | {ron} + RON IN2 to OUT2 ; typ = 120 ; mΩ ; VIN1 = 5 V | "),
            format!("analog_switch | ilim_2_a = 1.5 | {ilim} | labelled for channel 1:"),
            format!("analog_switch | ilim_2_a = 2.0 | {ilim} | "),
            "analog_switch | ron_2_ohms = { typ = 0.1, unit = \"ohm\", conditions = \"VIN2 = 5 V\", basis = \"OUT1 row\" } | - ; typ = 1 ; V ; - | row '' (OUT1 row)".into(),
        ] {
            let [kind, params, rows, want] = case.split(" | ").collect::<Vec<_>>()[..] else { panic!("{case}") };
            let e = entry(kind, params, &cited(rows));
            let found = source_warnings(&e);
            assert!(found.len() == usize::from(!want.is_empty()) && found.iter().all(|w| w.contains(want)), "{case}: {found:?}");
        }
    }

    /// Two VF maxima that no physical free fit passes through take the ideal
    /// junction plus a series resistance; two that no non-negative resistance
    /// reproduces (a flatter rise than the junction gives) take no law.
    #[test]
    fn two_vf_maxima_fit_an_ideal_junction_with_series_resistance() {
        let fit = |rows: &str| {
            let e = entry("diode", "", &cited(rows));
            diode_fit(&vf_points(cited_rows(&e)))
        };
        let [is, n, rs] =
            fit("VF ; max = 0.45 ; V ; IF = 1 A + VF ; max = 0.75 ; V ; IF = 3 A").expect("fits");
        assert!((n - 1.0).abs() < 1e-9 && rs > 0.0 && is > 0.0);
        assert!((diode_vf([is, n, rs], 1.0) - 0.45).abs() < 1e-3);
        assert!((diode_vf([is, n, rs], 3.0) - 0.75).abs() < 1e-3);
        assert!(fit("VF ; max = 0.30 ; V ; IF = 1 A + VF ; max = 0.31 ; V ; IF = 3 A").is_none());
    }

    /// An identity-only card whose own cited rows cover every param its checker
    /// requires is refused by name (a missing optional GBW row changes nothing);
    /// one lacking a row for a required param, or a digital part, may abstain.
    #[test]
    fn identity_only_is_refused_where_the_cited_rows_suffice() {
        let law = [1e-9, 1.8, 0.2];
        let vf = |ma: f64| format!("VF ; max = {} ; V ; IF = {ma} mA", diode_vf(law, ma / 1e3));
        let diode = format!("diode | {} | 'VF'", [1.0, 10.0, 100.0].map(vf).join(" + "));
        for case in [
            diode.as_str(),
            "diode | VF ; max = 1 ; V ; IF = 10 mA + IR ; max = 5 ; uA ; VR = 50 V | 'VF'",
            "analog_switch | RON ; typ = 80 ; mΩ ; - + IOFF off-state leakage ; max = 1 ; uA ; VIN = 5.5 V | (derived: roff = off-state leakage: 5.5 V (its test voltage) / 'IOFF off-state leakage' max 1 uA (VIN = 5.5 V)), (derived: ron = 'RON' typ 80 mΩ (-))",
            "analog_switch | RON ; typ = 80 ; mΩ ; - | ",
            "bjt_pnp | hFE ; min = 100 ; - ; IC = -2 mA + VBE(on) ; max = -0.75 ; V ; IC = -2 mA | 'VBE(on)', 'hFE'",
            "bjt_npn | hFE ; min = 100 ; - ; IC = 2 mA + VCE(sat) ; max = 0.2 ; V ; IC = 10 mA | ",
            "bjt_npn | hFE ; min = 100 ; - ; IC = 2 mA + VBE sat ; max = 1.2 ; V ; IC = 500 mA | ",
            "nmos | VGS(th) ; min = 1, max = 2.5 ; V ; ID = 250 uA + RDS(on) ; typ = 40 ; mΩ ; VGS = 10 V | 'RDS(on)', 'VGS(th)'",
            "pmos | VGS(th) ; min = 1, max = 2.5 ; V ; ID = 1 mA + RDS(on) ; max = 90 ; mΩ ; VGS = 4.5 V, TJ=125 C | ",
            "opamp | AOL ; typ = 110 ; dB ; RL = 10 kΩ + VOH ; min = 4.8, typ = 4.9 ; V ; RL = 10 kΩ + VOL ; max = 0.1 ; V ; RL = 10 kΩ | 'AOL', 'VOH', 'VOL'",
            "opamp | AOL ; typ = 110 ; dB ; RL = 10 kΩ + VOH ; typ = 4.9 ; V ; RL = 10 kΩ | ",
            "comparator | VOH ; min = 4.9 ; V ; IO = 4 mA + VOL ; max = 0.4 ; V ; IO = 4 mA | ",
            "comparator | VOH ; min = 4.9 ; V ; - + VOL ; max = 0.4 ; V ; - + VHYS ; typ = 3 ; mV ; - | 'VHYS', 'VOH', 'VOL'",
            "comparator | VOL ; max = 0.4 ; V ; ISINK = 4 mA + IOH leakage ; max = 1 ; uA ; VO = 5 V | ",
            "digital | VIH ; min = 2 ; V ; VCC = 5 V | ",
        ] {
            let [kind, rows, want] = case.split(" | ").collect::<Vec<_>>()[..] else {
                panic!("{case}")
            };
            let card = "identity_only = true\nwarning = \"w\"\nunlocked_by = \"u\"";
            let got = crate::validation::validate(&entry(kind, card, &cited(rows)));
            let named = format!("rows {want} suffice for kind \"{kind}\"");
            let refused = got.as_ref().is_err_and(|e| e.iter().any(|e| e.message.contains(&named)));
            assert!(if want.is_empty() { got.is_ok() } else { refused }, "{case}: {got:?}");
        }
        // An uncovered required param counts as covered only where an `unknown` may stand.
        let half = entry(
            "opamp",
            "",
            &cited("AOL ; typ = 110 ; dB ; - + VOH ; typ = 4.9 ; V ; -"),
        );
        let required = ["gain", "rail_lo|swing_lo", "rail_hi|swing_hi"];
        assert!(needless_abstention(&half, &required, |k| k == "swing_lo").is_some());
        assert!(needless_abstention(&half, &required, |_| false).is_none());
    }

    /// Refused by name, each beside a legal control: a plain value on a nonzero
    /// checker bound that no cited row prints (an `unknown` record stands for a
    /// defaulted param), a BJT `is` fitted to VBE(sat), and a Schottky's tt,
    /// which its trr no longer derives. Case: kind | params | rows | unknowns | description | error.
    #[test]
    #[rustfmt::skip]
    fn bound_placeholders_saturation_fits_and_schottky_tt_are_refused() {
        let (on, sat) = ("VBE(on) ; typ = 660 ; mV ; IC = 2 mA", "VBE(sat) ; max = 0.9 ; V ; IC = 50 mA");
        let (ron, trr) = ("RON ; typ = 0.1 ; ohm ; VIN = 5 V", "trr ; max = 5 ; ns ; IF = 10 mA, IR = 10 mA");
        for case in [
            format!("bjt_npn | is = 1e-20, bf = 200, nf = 1.0 | {on} | - | t | 'is' = 1e-20 sits at the checker floor 1e-20"),
            format!("bjt_npn | bf = 200 | {on} | is nf | t | "),
            format!("analog_switch | ron = 0.1, roff = 1000.0 | {ron} | - | t | 'roff' = 1e3 sits at the checker floor"),
            "analog_switch | ron = 0.1, roff = 1000.0 | ROFF off ; typ = 1 ; kΩ ; VIN = 5 V | - | t | ".into(),
            format!("analog_switch | ron = 0.1, roff = 1000.0 | {ron} + tf ; typ = 1000 ; µs ; CL = 1 µF | - | t | checker floor"),
            format!("analog_switch | ron = 0.1 | {ron} | roff | t | "),
            format!("diode | is = 1e-14, n = 1.5, rs = 0.0 | {trr} | - | t | "),
            format!("bjt_npn | is = 3.87e-17, bf = 200, nf = 1.0 | {sat} | - | t | reproduces the saturation row 'VBE(sat)'"),
            format!("bjt_npn | is = 1.66e-14, bf = 200, nf = 1.0 | {on} + {sat} | - | t | "),
            format!("diode | is = 1e-8, n = 1.05, rs = 0.5, tt = 7.2e-9 | {trr} | - | t | "),
            format!("diode | is = 1e-8, n = 1.05, rs = 0.5, tt = 7.2e-9 | {trr} | - | SBD_FIXTURE Schottky | a Schottky stores no"),
            format!("diode | is = 1e-8, n = 1.05, rs = 0.5 | {trr} | - | SBD_FIXTURE Schottky | "),
        ] {
            let [kind, params, rows, unknown, desc, want] = case.split(" | ").collect::<Vec<_>>()[..] else { panic!("{case}") };
            let record = |p| format!("[[models.source.uncertainty]]\nstatus = \"unknown\"\nparameter = \"{p}\"\nreason = \"r\"\n");
            let unknown: String = unknown.split_whitespace().filter(|p| *p != "-").map(record).collect();
            let mut e = entry(kind, &params.replace(", ", "\n"), &(cited(rows) + &unknown));
            e.description = desc.into();
            let tt = derivations(&e).iter().any(|d| d.0 == "tt");
            assert_eq!(tt, kind == "diode" && !schottky(&e), "{case}");
            let got = crate::validation::validate(&e);
            let named = got.as_ref().is_err_and(|e| e.len() == 1 && e[0].message.contains(want));
            assert!(if want.is_empty() { got.is_ok() } else { named }, "{case}: {got:?}");
        }
    }

    /// Each standard relation fills its param from synthetic cited rows only.
    #[test]
    fn cited_rows_derive_the_spice_params_they_fix() {
        for case in [
            "bjt_npn | fT gain-bandwidth ; typ = 250 ; MHz ; VCE = 5 V | tf = 6.3662e-10",
            "bjt_pnp | fT ; min = 100 ; MHz ; IC = -10 mA | tf = 1.5915e-9",
            "bjt_npn | fT ; min = 100 ; MHz ; IC = 10 mA + fT ; typ = 250 ; MHz ; IC = 10 mA | tf = 6.3662e-10",
            "bjt_pnp | Cob output capacitance ; max = 4 ; pF ; VCB = -5 V, f = 1 MHz | cjc = 7.8339e-12",
            "bjt_npn | Ce emitter capacitance ; typ = 10 ; pF ; VEB = 0.5 V | cje = 1.1836e-11",
            "nmos | Ciss ; typ = 630 ; pF ; VDS = 15 V + Crss ; typ = 50 ; pF ; VDS = 15 V | cgs = 5.8e-10, cgd = 5e-11",
            "diode | CT ; typ = 1 ; pF ; VR = 10 V + CT total ; typ = 2 ; pF ; VR = 3 V | cjo = 4e-12",
            "diode | V(BR)R breakdown ; min = 100 ; V ; IR = 100 µA | ibv = 1e-4, bv = 1e2",
            "diode | VZ zener ; min = 3.1, typ = 3.3, max = 3.5 ; V ; IZT = 76 mA | ibv = 7.6e-2, bv = 3.3e0",
            "diode | trr recovery ; max = 4 ; ns ; IF = 10 mA, IR = 1 mA | tt = 1.6681e-9",
            "diode | trr ; max = 4 ; ns ; RL = 100 Ω | tt = 5.7708e-9",
            "diode | CT ; typ = 4 ; pF ; VR = 0 V, TA = 85 °C | ",
        ] {
            let [kind, rows, want] = case.split(" | ").collect::<Vec<_>>()[..] else {
                panic!("{case}")
            };
            let rows = cited(rows);
            let e = entry(kind, "", &rows);
            let got = derivations(&e).into_iter().map(|d| format!("{} = {:e}", d.0, d.1));
            let got: Vec<String> = got.collect();
            assert_eq!(got.join(", "), want, "{case}");
            // Omitted, the lint names it; written as derived, nothing warns.
            let warned = source_warnings(&e).iter().filter(|w| w.contains("is omitted")).count();
            assert_eq!(warned, got.len(), "{case}");
            let written = got.join("\n").replace("e0", "");
            assert!(source_warnings(&entry(kind, &written, &rows)).is_empty(), "{case}");
        }
    }

    #[test]
    fn a_row_param_reads_its_column_in_the_unit_its_name_names() {
        for case in [
            "iq_a | typ = 65, max = 90, unit = \"uA\" | 6.500000e-5",
            "supply_static_ua | typ = 65, max = 90, unit = \"mA\" | 6.500000e4",
            "vout | min = 3.2, max = 3.4, unit = \"V\" | 3.300000e0",
            "dropout_v | typ = 150, max = 250, unit = \"mV\" | 2.500000e-1",
            "voh | min = 2.4, typ = 3, unit = \"V\" | 2.400000e0",
            "vol | typ = 0.1, unit = \"V\" | 1.000000e-1",
            "gain | typ = 1e5, unit = \"\" | 1.000000e5",
            "vout | min = 3.2, unit = \"V\" | reads the typ column, else the min/max midpoint",
            "vol | min = 0.1, unit = \"V\" | reads the max column, else the typ",
            "iq_a | typ = 5, unit = \"mV/\u{b0}C\" | which is read in A, by its name",
            "gain | typ = 5, unit = \"V\" | which is read in no unit",
        ] {
            let [name, cols, want] = case.splitn(3, " | ").collect::<Vec<_>>()[..] else {
                panic!("{case}")
            };
            let src = cols.replace(", ", "\n") + "\nconditions = \"c\"\nbasis = \"b\"";
            let row: Characteristic = toml::from_str(&src).expect("row parses");
            let got = row_value(name, &row).map_or_else(|e| e, |v| format!("{v:.6e}"));
            assert!(got.contains(want), "{case}: {got}");
        }
        let columns = ["vih", "i_max_a", "rail_lo", "iq_a"].map(row_column);
        assert_eq!(columns, ["min", "max", "max", "typ"]);
    }

    #[test]
    fn converter_and_digital_splits_fall_on_their_named_boundaries() {
        let names = known_param_names;
        assert_eq!(names(ComponentKind::Digital)[0], "supply_pin");
        assert_eq!(names(ComponentKind::ShiftRegister)[0], "bits");
        assert_eq!(names(ComponentKind::Dac).last(), Some(&"gnd_pin"));
        assert!(!names(ComponentKind::Dac).contains(&"vih"));
        assert!(names(ComponentKind::Adc).contains(&"vih"));
        assert!(!names(ComponentKind::Adc).contains(&"supply_cpd_pf"));
    }

    #[test]
    fn every_shipped_db_param_name_is_known() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("db");
        let mut offenders: Vec<String> = Vec::new();
        let mut files = 0usize;
        for f in std::fs::read_dir(&dir).expect("db/ is readable") {
            let path = f.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("db file is readable");
            let Ok(db) = toml::from_str::<crate::schema::DbFile>(&text) else {
                continue;
            };
            files += 1;
            for e in &db.models {
                for u in unknown_params(e) {
                    offenders.push(format!(
                        "{}: model '{}': {}",
                        path.display(),
                        e.id,
                        u.message()
                    ));
                }
            }
        }
        assert!(files > 0, "no [[models]] db file was read from {dir:?}");
        assert!(offenders.is_empty(), "{}", offenders.join("\n"));
    }

    /// A row is an untagged rating by its symbol and the key's unit: a `PD_SCK` pulse
    /// width (us) and a `PD` derating slope (mW/°C) are not power limits, a `PD` in
    /// mW is.
    #[test]
    fn a_rating_lint_reads_the_unit_not_the_pin_name() {
        let warns = |row: &str| {
            let rows = cited(row);
            let e = entry("digital", "supply_static_ua = 1.0", &rows);
            source_warnings(&e)
                .iter()
                .any(|w| w.contains("prints a rating"))
        };
        assert!(!warns("PD_SCK input high time ; max = 50 ; us ; VBG = 1 V"));
        assert!(!warns(
            "PD derate above 25 °C ; typ = 2 ; mW/°C ; TA > 25 °C"
        ));
        assert!(warns("PD power dissipation ; max = 250 ; mW ; TA = 25 °C"));
    }
}
