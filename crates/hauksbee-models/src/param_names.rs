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
    "auto_bind",
    "auto_bind_family",
    "auto_bind_pin_summary",
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
static CONVERTER_AND_DIGITAL: [&str; 19] = [
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
    "tpd_s",
    "supply_static_ua",
    "supply_cpd_pf",
];

// Where each kind's slice of `CONVERTER_AND_DIGITAL` starts or ends; pinned by
// `converter_and_digital_splits_fall_on_their_named_boundaries`.
const SHIFT_REGISTER_FROM: usize = 8; // `bits`
const DIGITAL_FROM: usize = 9; // `supply_pin`
const DAC_TO: usize = 11; // through `gnd_pin`
const ADC_TO: usize = 18; // everything before `supply_cpd_pf`

/// The parameter names this kind's consumers read, beyond `UNIVERSAL`.
///
/// A kind with an empty slice carries no numeric physics at all (a connector
/// models pin continuity; `Ignore` models nothing), so every param on it is
/// unread by construction and the lint says so.
#[must_use]
pub fn known_param_names(kind: ComponentKind) -> &'static [&'static str] {
    match kind {
        ComponentKind::Passive => &["ohms", "series_ohms", "value_override", "esr", "esl"],
        ComponentKind::Diode => DIODE,
        ComponentKind::BjtNpn | ComponentKind::BjtPnp => BJT,
        ComponentKind::Nmos | ComponentKind::Pmos => MOSFET,
        ComponentKind::Vreg => &[
            "vout",
            // The adjustable (ADJ-referenced) trio: the rail is the board's,
            // not the card's, so there is no `vout` on such an entry.
            "vref",
            "iadj_a",
            "dropout_v",
            "iq_a",
            "cout_min_f",
            "cout_esr_min_ohm",
            "cout_esr_max_ohm",
            "battery_chemistry",
            "vout_setpoint_low",
            "vout_setpoint_high",
            "shunt",
            "iref_a",
            "zka_ohms",
            "iktest_a",
            "ikmax_a",
            "ikmin_a",
        ],
        // `swing_*`: output-swing headrooms from the V-/V+ PINS, which bind the
        // rails to the supply nets (`rail_*` then only a fallback).
        ComponentKind::Opamp => &[
            "gain", "pole_hz", "slew", "rail_lo", "rail_hi", "swing_lo", "swing_hi",
        ],
        // `out_lo`/`out_hi`/`hysteresis` are the push-pull stamp's; the last four
        // the open-collector stage's, whose levels come from the board instead.
        ComponentKind::Comparator => &[
            "out_lo",
            "out_hi",
            "hysteresis",
            "tpd_s",
            "supply_static_ua",
            "open_collector",
            "gain",
            "ron_ohms",
            "leak_s",
        ],
        // The first half are datasheet quantities; the rest are the numerical
        // parameters of the composite stamp (see db/timer.toml), on the card
        // so the paired ngspice deck pins them where a reader can find them.
        ComponentKind::Timer => &[
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
        ],
        ComponentKind::AnalogSwitch => &["ron", "roff", "vth"],
        ComponentKind::Digital => &CONVERTER_AND_DIGITAL[DIGITAL_FROM..],
        // A shift register is the digital vocabulary plus its stage count.
        ComponentKind::ShiftRegister => &CONVERTER_AND_DIGITAL[SHIFT_REGISTER_FROM..],
        ComponentKind::Dac => &CONVERTER_AND_DIGITAL[..DAC_TO],
        ComponentKind::Adc => &CONVERTER_AND_DIGITAL[..ADC_TO],
        ComponentKind::Mcu => &["backend", "module", "max_adc_source_impedance_ohms"],
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
    vol V output low level; ro ohm output resistance";

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
    const MIN: &[&str] = &["vih", "voh", "out_hi", "rail_hi"];
    const MAX: &[&str] = &["vil", "vol", "out_lo", "rail_lo", "vol_v", "voh_drop_v"];
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
/// (its glossary or suffix unit), or none for a unitless param.
fn row_scale(name: &str, unit: &str) -> Result<f64, String> {
    let want = param_unit(name).filter(|u| *u != "-");
    let scale = match want {
        _ if unit.trim() == want.unwrap_or("") => Some(1.0),
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
                     row printing both, never a one-sided bound"
                ));
            }
        }
    }
    out.extend(row_slips(entry, rows));
    out.extend(law_warning(entry, rows));
    out.extend(gain_warning(entry, rows));
    out.extend(channel_slips(entry, rows));
    derived.retain(|d| !entry.params.0.contains_key(d.0));
    for (n, v, how) in derived {
        out.push(format!(
            "param '{n}' is omitted but its cited row gives {n} = {v:e}, {how}"
        ));
    }
    out
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

/// A diode is/n/rs on too few VF points ([`underfit`]), on max rows only, or
/// over 10% off a cited VF row; or a bf outside every hFE row.
fn law_warning(entry: &ModelEntry, rows: &[Characteristic]) -> Option<String> {
    if matches!(entry.kind, ComponentKind::BjtNpn | ComponentKind::BjtPnp) {
        let bf = entry.params.get_f64("bf")?;
        let hfe: Vec<_> = rows.iter().filter(|r| symbol(r, "HFE")).collect();
        let off = |r: &&Characteristic| !(r.min.unwrap_or(bf)..=r.max.unwrap_or(bf)).contains(&bf);
        let off = !hfe.is_empty() && hfe.iter().all(off);
        return off.then(|| format!("bf = {bf} is outside the printed min..max of every hFE row"));
    }
    let law = diode_law(entry).filter(|_| entry.kind == ComponentKind::Diode)?;
    let points = vf_points(rows)?;
    let (miss, i, v) = worst_miss(law, &points);
    let at = diode_vf(law, i);
    let max_only = !points.is_empty() && rows.iter().all(|r| !symbol(r, "VF") || r.typ.is_none());
    let max_only = max_only.then(|| "diode is/n/rs are a max-only fit: a max VF is a guaranteed bound, not typical behaviour; fit typ VF rows or points read off the typical IF-VF curve".into());
    underfit(entry).or(max_only).or_else(|| (miss > 0.1).then(|| format!("diode is/n/rs give VF = {at:.3} V at IF = {i} A where a cited VF row prints {v} V, over 10% off")))
}

/// A diode card's is/n/rs resting on cited VF points at under two distinct
/// currents a decade apart: three unknowns underdetermined, refused on save.
pub(crate) fn underfit(entry: &ModelEntry) -> Option<String> {
    let law = diode_law(entry).filter(|_| entry.kind == ComponentKind::Diode);
    let points = vf_points(cited_rows(entry)).filter(|p| !p.is_empty() && law.is_some())?;
    let (count, span) = (points.len(), if_span(&points));
    (span < 10.0).then(|| format!("diode is/n/rs rest on {count} cited VF point(s) spanning {span:.1}× in IF: three unknowns need VF points at two, best three, distinct currents a decade or more apart; cite typ VF rows or points read off the typical IF-VF curve"))
}

/// The highest over the lowest current among `points` (0 when there are none).
fn if_span(points: &[(f64, f64)]) -> f64 {
    let currents = points.iter().map(|p| p.0);
    currents.clone().fold(0.0, f64::max) / currents.fold(f64::INFINITY, f64::min)
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
/// `VBRR` in "V(BR)R breakdown").
fn symbol(row: &Characteristic, sym: &str) -> bool {
    let text = row.parameter.replace(['(', ')'], "");
    let mut words = text.split(|c: char| !c.is_alphanumeric());
    words.any(|w| w.eq_ignore_ascii_case(sym))
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

fn cited_rows(entry: &ModelEntry) -> &[Characteristic] {
    entry.source.as_ref().map_or(&[], |s| &s.characteristic)
}

/// Whether a row is at 25 °C or states no temperature.
fn at_25(row: &Characteristic) -> bool {
    static CELSIUS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"([-+]?\d*\.?\d+)\s*°\s*C").expect("a valid pattern")
    });
    let mut temperatures = CELSIUS.captures_iter(&row.conditions);
    temperatures.all(|t| t[1].parse() == Ok(25))
}

/// The magnitude of `sym = <number> <unit>` in `text`, in `unit` (`IR = 10 mA`).
pub(crate) fn quantity(text: &str, sym: &str, unit: &str) -> Option<f64> {
    let re =
        regex::Regex::new(&format!(r"\b{sym}\s*=\s*([-+]?\d*\.?\d+)\s*([a-zA-ZµμΩ]*)")).ok()?;
    let c = re.captures(text)?;
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
            if let Some((bv, how, r)) = first("VBR VBRR BV VZ", "V") {
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
    let round = |v: f64| format!("{v:.4e}").parse().unwrap_or(v);
    let out = out
        .into_iter()
        .map(|(n, v, how)| (n, round(v), format!("derived: {n} = {how}")));
    out.collect()
}

fn diode_law(entry: &ModelEntry) -> Option<[f64; 3]> {
    let p = &entry.params;
    Some([p.get_f64("is")?, p.get_f64("n")?, p.get_f64("rs")?])
}

/// VF of `[is, n, rs]` at `i` amperes and the solver's nominal 27 °C.
fn diode_vf([is, n, rs]: [f64; 3], i: f64) -> f64 {
    (n * hauksbee_ir::thermal_voltage_c(27.0)).mul_add((i / is).ln_1p(), i * rs)
}

fn worst_miss(law: [f64; 3], points: &[(f64, f64)]) -> (f64, f64, f64) {
    let miss = |&(i, v): &(f64, f64)| ((diode_vf(law, i) - v).abs() / v, i, v);
    points
        .iter()
        .map(miss)
        .fold((0.0, 0.0, 0.0), |a, b| if b.0 > a.0 { b } else { a })
}

/// Cited `(IF, VF)` in A and V: VF rows at the IF their conditions name, at 25 °C
/// or unstated; their typs alone where those span a decade, else each row's typ,
/// else max (the column a curated card fits without typ).
fn vf_points(rows: &[Characteristic]) -> Option<Vec<(f64, f64)>> {
    let current = regex::Regex::new(r"\bI_?F\s*[=≈:]?\s*(\d*\.?\d+)\s*([µμu]?A|mA)\b").ok()?;
    let point = |row: &Characteristic, column: Option<f64>| {
        let c = current.captures(&row.conditions).filter(|_| at_25(row))?;
        let i = c[1].parse::<f64>().ok()? * crate::units::scale_between(&c[2], "A")?;
        let v = column? * crate::units::scale_between(&row.unit, "V")?;
        (i > 0.0).then_some((i, v))
    };
    let vf = rows.iter().filter(|r| symbol(r, "VF"));
    let typ: Vec<_> = vf.clone().filter_map(|r| point(r, r.typ)).collect();
    let all = vf.filter_map(|r| point(r, r.typ.or(r.max))).collect();
    Some(if if_span(&typ) >= 10.0 { typ } else { all })
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
/// coefficient by Frisch-Waugh (3+ points over two decades, else 0), then
/// `VF - IF·rs` gives n and is. `None` unless the points span a decade (as
/// [`underfit`] asks), and the law is physical and within 10%.
fn diode_fit(points: &[(f64, f64)]) -> Option<[f64; 3]> {
    let (i, v): (Vec<f64>, Vec<f64>) = points.iter().copied().unzip();
    let ln: Vec<f64> = i.iter().map(|x| x.ln()).collect();
    let residual = |y: &[f64]| -> Vec<f64> {
        let (a, b) = line(&ln, y);
        y.iter().zip(&ln).map(|(y, x)| y - a * x - b).collect()
    };
    let (ri, rv) = (residual(&i), residual(&v));
    let fw =
        ri.iter().zip(&rv).map(|(a, b)| a * b).sum::<f64>() / ri.iter().map(|a| a * a).sum::<f64>();
    let spans = points.len() >= 3 && if_span(points) >= 100.0;
    let rs = if spans { fw.max(0.0) } else { 0.0 };
    let drop: Vec<f64> = v.iter().zip(&i).map(|(v, i)| v - i * rs).collect();
    let (a, b) = line(&ln, &drop);
    let law = [(-b / a).exp(), a / hauksbee_ir::thermal_voltage_c(27.0), rs];
    let physical = (1e-30..=1e-3).contains(&law[0]) && (0.5..=3.0).contains(&law[1]);
    (if_span(points) >= 10.0 && physical && worst_miss(law, points).0 <= 0.1).then_some(law)
}

/// The fit an executable diode card must take: its law is absent or over 10% off
/// its own VF rows, and the fit is not.
pub(crate) fn diode_refit(entry: &ModelEntry) -> Option<[f64; 3]> {
    let identity = entry.params.get_bool("identity_only") == Some(true);
    let diode = entry.kind == ComponentKind::Diode && !identity;
    let points = vf_points(&entry.source.as_ref().filter(|_| diode)?.characteristic)?;
    let (fit, fits) = (diode_fit(&points)?, |law| worst_miss(law, &points).0 <= 0.1);
    (!diode_law(entry).is_some_and(fits)).then_some(fit)
}

/// [`diode_refit`] as the is/n/rs completion writes over the card's, with the working.
pub(crate) fn diode_refit_params(entry: &ModelEntry) -> Vec<Derived> {
    let (Some(fit), Some(points)) = (diode_refit(entry), vf_points(cited_rows(entry))) else {
        return Vec::new();
    };
    let off = diode_law(entry).map_or_else(
        || "absent".into(),
        |law| format!("{:.0}% off", 100.0 * worst_miss(law, &points).0),
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
    let fits = entry.kind == Diode && vf_points(rows).and_then(|p| diode_fit(&p)).is_some();
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
        match need(key).and_then(cite) {
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
        let row = |name: &str, cols: &str, unit: &str| {
            format!("[models.source]\ntier = \"datasheet-derived\"\n[[models.source.characteristic]]\nparameter = \"{name}\"\n{cols}\nunit = \"{unit}\"\nconditions = \"c\"\nbasis = \"b\"\n")
        };
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
            let source = row(name, &cols.replace(", ", "\n"), unit);
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
        let row = |name: &str, cols: String, conditions: String| {
            format!("[[models.source.characteristic]]\nparameter = \"{name}\"\n{cols}\nunit = \"V\"\nconditions = \"{conditions}\"\nbasis = \"b\"\n")
        };
        let vf = |ma: f64| {
            row(
                "VF forward voltage",
                format!("typ = {}", diode_vf(law, ma / 1e3)),
                format!("IF = {ma} mA, TA = 25 \u{b0}C"),
            )
        };
        let source =
            |rows: String| format!("[models.source]\ntier = \"datasheet-derived\"\n{rows}");
        let rows = source([1.0, 10.0, 100.0, 1000.0].map(vf).concat());
        let drafted = entry("diode", "is = 1e-8\nn = 1.2\nrs = 0.5", &rows);
        assert!(source_warnings(&drafted).is_empty() && diode_refit(&drafted).is_none());
        let off = entry("diode", "is = 1e-14\nn = 1.0\nrs = 0.0", &rows);
        let fit = diode_refit(&off).expect("the rows fit their own law");
        assert!(
            fit.iter().zip(law).all(|(a, b)| (a / b - 1.0).abs() < 1e-3),
            "{fit:?}"
        );
        assert!(source_warnings(&off)[0].ends_with("over 10% off"));
        let hfe = source(row(
            "hFE DC current gain",
            "min = 110\nmax = 220".into(),
            "c".into(),
        ));
        let bjt = |bf: f64| {
            source_warnings(&entry(
                "bjt_npn",
                &format!("is = 1e-14\nbf = {bf}\nnf = 1.0"),
                &hfe,
            ))
        };
        assert!(
            bjt(300.0)[0].contains("bf = 300 is outside the printed min..max")
                && bjt(200.0).is_empty()
        );
    }

    /// A source block of `+`-joined rows, each `parameter ; columns ; unit ; conditions`.
    fn cited(rows: &str) -> String {
        let row = |spec: &str| {
            let [p, cols, unit, conditions] = spec.split(" ; ").collect::<Vec<_>>()[..] else {
                panic!("{spec}")
            };
            let (cols, head) = (cols.replace(", ", "\n"), "[[models.source.characteristic]]");
            let tail = format!("unit = \"{unit}\"\nconditions = \"{conditions}\"\nbasis = \"b\"\n");
            format!("{head}\nparameter = \"{p}\"\n{cols}\n{tail}")
        };
        let rows: String = rows.split(" + ").map(row).collect();
        format!("[models.source]\ntier = \"datasheet-derived\"\n{rows}")
    }

    /// Each flagged beside a legal control: a diode law on one VF point, under a
    /// decade or max rows only (the first also refused on save); a gain off its
    /// AOL row once dB, V/mV or V/V is converted; a channel's param on another
    /// channel's row. Case: kind | params | rows | warning (empty: clean).
    #[test]
    #[rustfmt::skip]
    fn thin_diode_fits_gain_unit_slips_and_swapped_channel_rows_are_flagged() {
        let vf = |col: &str, ma: f64| format!("VF ; {col} = {} ; V ; IF = {ma} mA", diode_vf([1e-9, 1.8, 0.2], ma / 1e3));
        let (law, ron) = ("is = 1e-9\nn = 1.8\nrs = 0.2", "RON IN1 to OUT1 ; typ = 90 ; mΩ ; VIN2 = 5 V");
        let ilim = "ILIM channel A ; typ = 1.5 ; A ; - + ILIM channel B ; typ = 2 ; A ; -";
        for case in [
            format!("diode | {law} | {} | rest on 1 cited VF point(s) spanning 1.0×", vf("max", 1000.0)),
            format!("diode | {law} | {} + {} | rest on 2 cited VF point(s) spanning 5.0×", vf("typ", 1.0), vf("typ", 5.0)),
            format!("diode | {law} | {} | a max-only fit", [1.0, 10.0, 100.0].map(|i| vf("max", i)).join(" + ")),
            format!("diode | {law} | {} + {} | ", vf("max", 1.0), [1.0, 10.0, 100.0].map(|i| vf("typ", i)).join(" + ")),
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
            assert_eq!(underfit(&e).is_some(), want.starts_with("rest on"), "{case}");
        }
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
            "diode | VF ; max = 1 ; V ; IF = 10 mA + IR ; max = 5 ; uA ; VR = 50 V | ",
            "bjt_pnp | hFE ; min = 100 ; - ; IC = -2 mA + VBE(on) ; max = -0.75 ; V ; IC = -2 mA | 'VBE(on)', 'hFE'",
            "bjt_npn | hFE ; min = 100 ; - ; IC = 2 mA + VCE(sat) ; max = 0.2 ; V ; IC = 10 mA | ",
            "bjt_npn | hFE ; min = 100 ; - ; IC = 2 mA + VBE sat ; max = 1.2 ; V ; IC = 500 mA | ",
            "nmos | VGS(th) ; min = 1, max = 2.5 ; V ; ID = 250 uA + RDS(on) ; typ = 40 ; mΩ ; VGS = 10 V | 'RDS(on)', 'VGS(th)'",
            "pmos | VGS(th) ; min = 1, max = 2.5 ; V ; ID = 1 mA + RDS(on) ; max = 90 ; mΩ ; VGS = 4.5 V, TJ = 125 °C | ",
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
}
