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

use crate::schema::{ComponentKind, ModelEntry};

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
    pole_hz Hz dominant pole; slew V/s slew rate; rail_lo V output rail (low); \
    rail_hi V output rail (high); swing_lo V headroom from V-; swing_hi V headroom from V+; \
    out_lo V output low level; out_hi V output high level; hysteresis V input hysteresis; \
    ron_ohms ohm output on-resistance (VOL / IOL); leak_s S output-off conductance (IOH / VOH); \
    ron ohm switch on-resistance; roff ohm switch off-resistance; vth V control threshold; \
    vih V input high threshold; vil V input low threshold; voh V output high level; \
    vol V output low level; ro ohm output resistance";

/// Params that ARE a printed bound (an input threshold, an output level), so
/// equalling a min-only or max-only row is their meaning, not a mistake.
const BOUND_PARAMS: &[&str] = &[
    "vih", "vil", "voh", "vol", "out_lo", "out_hi", "rail_lo", "rail_hi", "swing_lo", "swing_hi",
];

fn glossary() -> impl Iterator<Item = [&'static str; 3]> {
    GLOSSARY.split(';').filter_map(|e| {
        let mut it = e.trim().splitn(3, ' ');
        Some([it.next()?, it.next()?, it.next()?])
    })
}

/// The unit `name` is documented in: its glossary entry, else its suffix.
pub fn param_unit(name: &str) -> Option<&'static str> {
    glossary()
        .find(|[n, ..]| *n == name)
        .map(|[_, unit, _]| unit)
        .or_else(|| crate::units::suffix_unit(name).map(|(_, base)| base))
}

/// `name = meaning (unit)` for each glossary param `kind` reads, for the prompt.
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

/// Warnings that tie `[models.params]` to the card's own characteristic rows,
/// matched by value: a param equal to a row printed in another base unit than
/// the param's documented one (a current stored as a conductance), and a
/// nominal equal to the min or max of a row that prints no typ (unless another
/// row prints it as a typ). The unit check only looks below 1e-3, where a copied
/// current or capacitance lives and a round volt or ohm value cannot collide.
pub fn source_warnings(entry: &ModelEntry) -> Vec<String> {
    let rows = entry
        .source
        .as_ref()
        .map_or(&[][..], |s| &s.characteristic[..]);
    let mut out = Vec::new();
    for (name, value) in &entry.params.0 {
        let (Some(v), Some(unit)) = (value.as_f64(), param_unit(name)) else {
            continue;
        };
        if v == 0.0
            || unit.contains('/')
            || crate::units::si_unit(unit).is_none_or(|(s, _)| s != 1.0)
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
                && !BOUND_PARAMS.contains(&name.as_str())
            {
                out.push(format!(
                    "param '{name}' equals the {column} of characteristic '{row_name}', which \
                     prints no typ: a nominal is the typ, or the midpoint (min + max) / 2 of a \
                     row printing both, never a one-sided bound"
                ));
            }
        }
    }
    out
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
    pub fn message(&self) -> String {
        match &self.suggestion {
            Some(s) => format!(
                "param '{}' is not read for this kind or output form; did you mean '{}'? \
                 (unread params are silently ignored, so the default applies)",
                self.name, s
            ),
            None => format!(
                "param '{}' is not read for this kind or output form, so it is silently ignored; \
                 remove it or check the spelling",
                self.name
            ),
        }
    }
}

/// Every parameter name on `entry` that no consumer of its kind reads.
///
/// Empty for a clean entry. See the module docs for the three ways a name
/// counts as known.
pub fn unknown_params(entry: &ModelEntry) -> Vec<UnknownParam> {
    let vocab = known_param_names(entry.kind);
    let referenced = behavioral_identifiers(entry);

    let ignored = form_ignored(entry);
    let known = |name: &str| -> bool {
        (vocab.contains(&name) && !ignored.contains(&name))
            || UNIVERSAL.contains(&name)
            || referenced.contains(name)
    };

    // `<stem>_from_ref` is the board-programmable form of `<stem>`, but ONLY on
    // the behavioral path: `resolve_from_ref_params` runs in the binder after an
    // early `continue` for an entry with an empty behavioral block, so a
    // `vout_from_ref` on a plain stamp-path vreg is never rewritten and never
    // read. Exempting it there would hide exactly the silently-ignored param this
    // lint is for, so the suffix only counts when the entry has a behavioral
    // block to be rewritten for.
    let from_ref_is_resolved = !entry.behavioral.is_empty();

    entry
        .params
        .0
        .keys()
        .map(|name| {
            let stem = if from_ref_is_resolved {
                name.strip_suffix("_from_ref").unwrap_or(name)
            } else {
                name.as_str()
            };
            (name, stem)
        })
        .filter(|(_, stem)| !known(stem))
        .map(|(name, stem)| UnknownParam {
            name: name.clone(),
            // Correct on the stem: `voutt_from_ref` should point at `vout`, not
            // be compared whole against a vocabulary that has no suffixed names.
            suggestion: nearest(stem.strip_suffix("_from_ref").unwrap_or(stem), vocab),
        })
        .collect()
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
    let mut out = std::collections::BTreeSet::new();
    let mut harvest = |src: &str| {
        for ident in src.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            // An identifier cannot start with a digit; that filters the numeric
            // literals out without parsing them.
            if ident.is_empty() || ident.starts_with(|c: char| c.is_ascii_digit()) {
                continue;
            }
            if RUNTIME_OWNED.contains(&ident)
                || ident.starts_with("state_")
                || EXPR_BUILTINS.contains(&ident)
            {
                continue;
            }
            out.insert(ident.to_string());
        }
    };
    for law in &entry.behavioral.laws {
        harvest(&law.expr);
    }
    if let Some(fsm) = &entry.behavioral.fsm {
        for t in &fsm.transitions {
            harvest(&t.guard);
        }
    }
    out
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
            [Some("S"), Some("s"), Some("F"), Some("A"), Some("V/s")]
        );
        assert!(param_meanings(ComponentKind::Diode)
            .contains("cjo = zero-bias junction capacitance (F)"));

        let row = |cols: &str, unit: &str| {
            format!("[models.source]\ntier = \"datasheet-derived\"\n[[models.source.characteristic]]\nparameter = \"ROW\"\n{cols}\nunit = \"{unit}\"\nconditions = \"c\"\nbasis = \"b\"\n")
        };
        let (leak, leak_ok) = (format!("{oc}1e-10"), format!("{oc}2e-11"));
        for (kind, params, source, want) in [
            (
                "comparator",
                &*leak,
                row("max = 0.1", "nA"),
                "documented in S",
            ),
            ("comparator", &leak_ok, row("max = 0.1", "nA"), ""),
            (
                "vreg",
                "dropout_v = 0.5",
                row("max = 0.5", "V"),
                "prints no typ",
            ),
            (
                "vreg",
                "dropout_v = 0.5",
                row("min = 0.5\nmax = 0.9", "V"),
                "no typ",
            ),
            (
                "vreg",
                "dropout_v = 0.3",
                row("typ = 0.3\nmax = 0.5", "V"),
                "",
            ),
            (
                "vreg",
                "dropout_v = 0.3",
                row("min = 0.1\nmax = 0.5", "V"),
                "",
            ),
            ("digital", "voh = 2.4", row("min = 2.4", "V"), ""),
            ("diode", "rs = 1.0", row("max = 1.0", "V"), ""),
        ] {
            let found = source_warnings(&entry(kind, params, &source));
            let ok = found.len() == usize::from(!want.is_empty());
            assert!(
                ok && found.iter().all(|w| w.contains(want)),
                "{params}: {found:?}"
            );
        }
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
