//! Derivation conventions: params a card's own cited rows (25 °C or unstated)
//! give by a fixed rule, so the extractor writes them instead of the model
//! restating them, or abstaining because it does not know the convention.
//!
//! [`crate::param_names::derivations`] folds these in. So `Reply::complete`
//! fills an omitted one with its working, the lint names it, and
//! `needless_abstention` refuses an identity-only card whose rows give a
//! required one. Each rule cites its datasheet basis. None runs without the
//! row it reads, and none overwrites a param the card already states.

use std::sync::LazyLock;

use regex::Regex;

use crate::param_names::{
    at_25, cited_rows, has_symbol, quantity, row_column, symbol, vf_points, Derived,
};
use crate::schema::{Characteristic, ComponentKind, ModelEntry, OperatingEnvelope};

/// Every convention-derived param for `entry`, unrounded, with its working.
pub fn conventions(entry: &ModelEntry) -> Vec<Derived> {
    use ComponentKind::{
        Adc, AnalogSwitch, Comparator, Digital, Diode, Opamp, ShiftRegister, Vreg,
    };
    let has = |k: &str| entry.params.0.contains_key(k);
    let mut out: Vec<Derived> = Vec::new();
    match entry.kind {
        Diode => out.extend(diode(entry)),
        // Op-amp output swing: a VOL/VOH row printed below 1 V is the output's
        // headroom from its own supply pin (a level at the rail, VS minus VOH),
        // what `swing_*` means. A level above 1 V is an absolute level, read
        // only at its test supply, so it is no headroom.
        Opamp => {
            for (name, rail, sym) in [
                ("swing_lo", "rail_lo", "VOL"),
                ("swing_hi", "rail_hi", "VOH"),
            ] {
                let headroom = named(entry, sym).filter_map(|r| cell(name, r));
                let first = headroom.map(|(v, how)| (v.abs(), how)).find(|c| c.0 < 1.0);
                out.extend(first.filter(|_| !has(rail)).map(|(v, how)| {
                    (
                        name,
                        v,
                        format!("headroom from its supply pin, printed below 1 V: {how}"),
                    )
                }));
            }
        }
        Comparator => {
            if entry.params.get_bool("open_collector") != Some(true) {
                out.extend(levels(entry));
            }
            out.extend(delay(entry).into_iter().chain(supply_current(entry)));
        }
        Digital | ShiftRegister | Adc => {
            out.extend(delay(entry).into_iter().chain(supply_current(entry)));
            out.extend(logic_levels(entry));
        }
        // A switching converter's dropout or supply row is an internal bias
        // regulator's (the `converter_dropout` plausibility rule).
        Vreg if entry.params.get_bool("shunt") != Some(true)
            && entry.behavioral.converter.is_none() =>
        {
            out.extend(regulator(entry));
        }
        AnalogSwitch => out.extend(switch(entry)),
        _ => {}
    }
    // A stated param, or its per-channel `<name>_<n>` set, is the card's own.
    let channel = |k: &str, name: &str| {
        k.strip_prefix(name)
            .and_then(|t| t.strip_prefix('_'))
            .is_some_and(|t| t.parse::<u32>().is_ok())
    };
    out.retain(|d| !has(d.0) && !entry.params.0.keys().any(|k| channel(k, d.0)));
    out
}

/// Rows at 25 °C (or unstated) naming any of `syms`: a symbol word
/// ([`symbol`], `VZ*` a prefix) or, written `~words_like_this`, a phrase in the
/// parameter text.
fn named<'a>(
    entry: &'a ModelEntry,
    syms: &'a str,
) -> impl Iterator<Item = &'a Characteristic> + 'a {
    let names = move |r: &Characteristic| {
        let text = r.parameter.to_lowercase();
        syms.split(' ').any(|s| {
            let phrase = |p: &str| text.contains(&p.replace('_', " "));
            s.strip_prefix('~').map_or_else(|| symbol(r, s), phrase)
        })
    };
    cited_rows(entry)
        .iter()
        .filter(move |r| at_25(r) && names(r))
}

/// `r` read in `unit`: column `col` (a guaranteed bound) else its typ; for a
/// nominal (`typ`), the typ, else the min/max midpoint, else its one printed
/// bound, a conservative stand-in named in the working.
fn read(r: &Characteristic, col: &str, unit: &str) -> Option<(f64, String)> {
    let scale = crate::units::scale_between(&r.unit, unit)?;
    let mid = r.min.zip(r.max).map(|(a, b)| f64::midpoint(a, b));
    let order = match col {
        "min" => [("min", r.min), ("typ", r.typ), ("", None)],
        "max" => [("max", r.max), ("typ", r.typ), ("", None)],
        _ => [
            ("typ", r.typ),
            ("min/max midpoint", mid),
            ("only printed bound", r.min.xor(r.max)),
        ],
    };
    let (col, x) = order.into_iter().find_map(|(c, x)| Some((c, x?)))?;
    let how = format!("'{}' {col} {x} {} ({})", r.parameter, r.unit, r.conditions);
    Some((x * scale, how))
}

/// `r` as param `name`: the column [`row_column`] gives it, in its unit.
fn cell(name: &str, r: &Characteristic) -> Option<(f64, String)> {
    let (scale, unit) = crate::units::suffix_unit(name)
        .or_else(|| crate::param_names::param_unit(name).map(|u| (1.0, u)))?;
    read(r, row_column(name), unit).map(|(v, how)| (v / scale, how))
}

/// A row for an idle mode (shutdown, standby, sleep, off), not the operating one.
fn idle(r: &Characteristic) -> bool {
    let text = r.parameter.to_lowercase();
    let mut words = text.split(|c: char| !c.is_alphanumeric());
    words.any(|w| {
        matches!(
            w,
            "shutdown" | "standby" | "sleep" | "off" | "disabled" | "powerdown"
        )
    })
}

/// The positive single supply a row's conditions print (VCC, VDD, VS, V+).
fn supply(r: &Characteristic) -> Option<f64> {
    quantity(&r.conditions, r"(?:V_?(?:CC|DD|S)|V\+)", "V").filter(|v| *v > 0.0)
}

/// A comparator's push-pull output levels: `out_lo` is the guaranteed VOL
/// (max), `out_hi` the guaranteed VOH (min). A VOH printed below 1 V is
/// headroom from the supply, so `out_hi` is the printed supply minus it.
fn levels(entry: &ModelEntry) -> Vec<Derived> {
    let lo = named(entry, "VOL").find_map(|r| cell("out_lo", r));
    let hi = named(entry, "VOH").find_map(|r| {
        if let Some((v, how)) = cell("out_hi", r).filter(|c| c.0 >= 1.0) {
            return Some((v, how));
        }
        // Headroom: its largest guaranteed value, the lowest guaranteed level.
        let (v, how) = read(r, "max", "V").filter(|c| c.0 < 1.0)?;
        let s = supply(r).filter(|s| *s > v)?;
        Some((s - v, format!("{s} V supply minus its headroom, {how}")))
    });
    let lo = lo.map(|(v, how)| ("out_lo", v, format!("VOL, {how}")));
    lo.into_iter()
        .chain(hi.map(|(v, how)| ("out_hi", v, format!("VOH, {how}"))))
        .collect()
}

/// The overdrive a comparator delay row is printed at (`VOD = 100 mV`,
/// `20 mV overdrive`), in V; zero when it prints none.
fn overdrive(text: &str) -> f64 {
    static OD: LazyLock<Regex> = LazyLock::new(|| {
        let n = r"(\d*\.?\d+)[\s-]*([mµu]?V)";
        Regex::new(&format!(
            r"(?i)(?:\bV_?OD\b|overdrive)\s*[=:≈]?\s*{n}|{n}[\s-]*(?:of\s+)?overdrive"
        ))
        .expect("a valid pattern")
    });
    let Some(c) = OD.captures(text) else {
        return 0.0;
    };
    let (x, unit) = c
        .get(1)
        .zip(c.get(2))
        .or_else(|| c.get(3).zip(c.get(4)))
        .unzip();
    let x: f64 = x.and_then(|m| m.as_str().parse().ok()).unwrap_or(0.0);
    x * unit
        .and_then(|u| crate::units::scale_between(u.as_str(), "V"))
        .unwrap_or(1.0)
}

/// `tpd_s`: a tPD row, else the larger of tPLH/tPHL printed under the same
/// conditions (the slower edge bounds the delay). A comparator prints delay per
/// input overdrive; the row at the largest is the part's intrinsic delay, the
/// one a small overdrive only lengthens (rows naming none count as the least).
fn delay(entry: &ModelEntry) -> Option<Derived> {
    let syms = "TPD TPLH TPHL TRESP ~propagation_delay ~response_time";
    let rows: Vec<_> = named(entry, syms)
        .filter_map(|r| Some((r, cell("tpd_s", r)?)))
        .collect();
    let od = |r: &Characteristic| overdrive(&format!("{} {}", r.parameter, r.conditions));
    let top = rows
        .iter()
        .map(|(r, _)| od(r))
        .fold(f64::NEG_INFINITY, f64::max);
    let at: Vec<_> = rows.iter().filter(|(r, _)| od(r) >= top).collect();
    let first = &at.first()?.0.conditions;
    let same = at.iter().filter(|(r, _)| &r.conditions == first);
    let (_, (v, how)) = same.max_by(|a, b| a.1 .0.total_cmp(&b.1 .0))?;
    Some((
        "tpd_s",
        *v,
        format!("propagation delay, the slower edge at the largest printed overdrive: {how}"),
    ))
}

/// `supply_static_ua`: the quiescent supply-current row (ICC, IDD, IQ): an
/// operating row, else a standby one; never a shutdown or sleep row, nor one
/// measured reading, writing or at a running clock (the `static_supply`
/// plausibility rule holds a drafted value to the same).
fn supply_current(entry: &ModelEntry) -> Option<Derived> {
    use crate::plausibility::{dynamic, shutdown, standby};
    let syms = "ICC* IDD* IQ ISUPPLY ISB ~supply_current ~quiescent_current ~standby_current";
    let quiet: Vec<_> = named(entry, syms)
        .filter(|r| !dynamic(r) && !shutdown(r))
        .collect();
    let operating = quiet.iter().filter(|r| !standby(r));
    let ranked = operating.chain(quiet.iter().filter(|r| standby(r)));
    let row = ranked.copied().find_map(|r| cell("supply_static_ua", r))?;
    Some((
        "supply_static_ua",
        row.0,
        format!("quiescent supply current, {}", row.1),
    ))
}

/// A logic level printed as a fraction of, or an offset from, a supply
/// (`0.7 × VDDIO`, `70 % VDD`, `VDD − 0.4 V`): that fraction or offset applied
/// to the supply's value its text prints (`VDDIO = 1.8 V`), else, for the main
/// supply, the one the card's stated levels are read at (the `one_corner`
/// plausibility rule), else a cited row for that supply (typ, else the
/// min/max midpoint).
fn relative_level(
    entry: &ModelEntry,
    text: &str,
    percent_of: Option<f64>,
) -> Option<(f64, String)> {
    use crate::plausibility::{FRACTION, RAIL};
    static OFFSET: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(r"\b{RAIL}\s*([-+−])\s*(\d*\.?\d+)\s*(m?V)?")).expect("a valid pattern")
    });
    static NAME: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(&format!(r"\b{RAIL}\b")).expect("a valid pattern"));
    let value = |rail: &str| {
        let printed = quantity(text, rail, "V");
        let main = matches!(rail, "VDD" | "VCC" | "VS");
        let corner = || {
            main.then(|| crate::plausibility::levels_corner(entry))
                .flatten()
        };
        let row = || {
            named(entry, rail)
                .find_map(|r| read(r, "typ", "V"))
                .map(|c| c.0)
        };
        printed.or_else(corner).or_else(row).filter(|v| *v > 0.0)
    };
    if let Some(pct) = percent_of {
        let rail = NAME.captures(text)?[1].to_string();
        let s = value(&rail)?;
        return Some((pct / 100.0 * s, format!("{pct} % of {rail} = {s} V")));
    }
    if let Some(c) = FRACTION.captures(text) {
        let f: f64 = c[1].parse().ok()?;
        let f = if c.get(2).is_some() { f / 100.0 } else { f };
        let s = value(&c[3]).filter(|_| (0.0..=1.0).contains(&f))?;
        return Some((f * s, format!("{} × {} = {s} V", &c[1], &c[3])));
    }
    let c = OFFSET.captures(text)?;
    let x: f64 = c[3].parse::<f64>().ok()?
        * if c.get(4).is_some_and(|u| u.as_str() == "mV") {
            1e-3
        } else {
            1.0
        };
    let s = value(&c[1])?;
    let v = if &c[2] == "+" { s + x } else { s - x };
    Some((v, format!("{} {} {x} V at {} = {s} V", &c[1], &c[2], &c[1])))
}

/// A digital input or output level: a supply-relative one ([`relative_level`])
/// from a row or `unknown` record quoting it; with none of the four levels on
/// the card, a printed absolute row's guaranteed column (VIH/VOH min, VIL/VOL
/// max).
fn logic_levels(entry: &ModelEntry) -> Vec<Derived> {
    let names = [
        ("vih", "VIH"),
        ("vil", "VIL"),
        ("voh", "VOH"),
        ("vol", "VOL"),
    ];
    let bare = names.iter().all(|(n, _)| !entry.params.0.contains_key(*n));
    let unknowns = entry
        .source
        .iter()
        .flat_map(|s| &s.uncertainty)
        .filter_map(|u| match u {
            hauksbee_ir::evidence::ModelUncertainty::Unknown { parameter, reason } => {
                Some((parameter, reason))
            }
            hauksbee_ir::evidence::ModelUncertainty::Interval { .. } => None,
        });
    let unknowns: Vec<_> = unknowns.collect();
    let mut out = Vec::new();
    for (name, sym) in names {
        let row_level = |r: &Characteristic| {
            let text = format!("{} {} {}", r.parameter, r.conditions, r.basis);
            let column = match row_column(name) {
                "min" => r.min.or(r.typ),
                "max" => r.max.or(r.typ),
                _ => r.typ,
            };
            let percent = (r.unit.trim() == "%").then_some(column);
            let percent = match percent {
                Some(p) => Some(p?),
                None if r.min.or(r.typ).or(r.max).is_some() => return None,
                None => None,
            };
            relative_level(entry, &text, percent)
                .map(|(v, how)| (v, format!("'{}', {how}", r.parameter)))
        };
        let quoted = unknowns
            .iter()
            .filter(|u| has_symbol(u.0, sym))
            .find_map(|(p, why)| {
                relative_level(entry, why, None)
                    .map(|(v, how)| (v, format!("unknown record '{p}', {how}")))
            });
        let level = named(entry, sym).find_map(row_level).or(quoted);
        // A row recording its own "0.7 VDD" coefficient as volts is no level.
        let absolute = || {
            named(entry, sym)
                .filter(|r| bare && crate::plausibility::fraction_as_volts(r).is_none())
                .find_map(|r| cell(name, r))
        };
        out.extend(
            level
                .or_else(absolute)
                .map(|(v, how)| (name, v, format!("{sym}, {how}"))),
        );
    }
    out
}

/// A linear regulator's or charger's `iq_a` and `dropout_v`.
///
/// `iq_a`: the ground or supply current of the operating mode. A charger's is
/// the row printed while charging, the mode the simulated part runs in; a
/// regulator's the no-load row (its quiescent current by definition), else
/// the lightest printed load.
///
/// `dropout_v`: the dropout row of the card's own output option at the rated
/// output current, else the highest printed load (dropout grows with load, so
/// that bounds every lighter one).
/// With no dropout row, the pass element's on-resistance times the current
/// ceiling: in dropout the pass FET is fully on, so VIN − VOUT = I·RDS(on).
fn regulator(entry: &ModelEntry) -> Vec<Derived> {
    let load =
        |r: &Characteristic| quantity(&r.conditions, "I_?(?:OUT|O|L|LOAD)", "A").unwrap_or(0.0);
    let mut out = Vec::new();
    let iq_syms = "IQ IGND IG ICC IDD ISUPPLY ~quiescent ~ground_current ~supply_current";
    let iq: Vec<_> = named(entry, iq_syms)
        .filter(|r| !idle(r))
        .filter_map(|r| Some((r, cell("iq_a", r)?)))
        .collect();
    let charging = |r: &Characteristic| {
        let text = format!("{} {}", r.parameter, r.conditions).to_lowercase();
        text.contains("charg") && !text.contains("discharg")
    };
    let charge = iq.iter().find(|(r, _)| charging(r));
    let lightest = iq
        .iter()
        .rev()
        .min_by(|a, b| load(a.0).total_cmp(&load(b.0)));
    out.extend(
        charge
            .map(|(_, (v, how))| ("iq_a", *v, format!("supply current while charging, {how}")))
            .or_else(|| {
                lightest.map(|(_, (v, how))| {
                    (
                        "iq_a",
                        *v,
                        format!("quiescent current at the lightest printed load, {how}"),
                    )
                })
            }),
    );
    let rows: Vec<_> = named(entry, "VDO VDROP VDROPOUT ~dropout")
        .filter_map(|r| Some((r, cell("dropout_v", r)?)))
        .collect();
    let vout = entry.params.get_f64("vout").map(f64::abs);
    let own = |r: &Characteristic| {
        let printed = quantity(&r.conditions, "V_?(?:OUT|O)", "V");
        printed
            .zip(vout)
            .is_some_and(|(a, b)| (a - b).abs() <= 0.02 * b)
    };
    let mine = rows.iter().any(|(r, _)| own(r));
    let pool = rows.iter().filter(|(r, _)| !mine || own(r));
    // The rated output current's row, else the heaviest printed load's.
    let rated = |r: &Characteristic| {
        entry
            .ratings
            .max_current_a
            .is_some_and(|i| (load(r) - i).abs() <= 0.02 * i)
    };
    let key = |r: &Characteristic| (rated(r), load(r));
    let heaviest = pool.rev().max_by(|a, b| {
        key(a.0)
            .partial_cmp(&key(b.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if let Some((_, (v, how))) = heaviest {
        out.push((
            "dropout_v",
            *v,
            format!("dropout at the rated, else the highest printed, load: {how}"),
        ));
        return out;
    }
    let ceiling = entry
        .current_program
        .as_ref()
        .and_then(|c| c.max_operating_current_a);
    let ceiling = ceiling.map(|i| (i, "current_program.max_operating_current_a"));
    let ceiling = ceiling.or_else(|| {
        entry
            .ratings
            .max_current_a
            .map(|i| (i, "ratings.max_current_a"))
    });
    let ron = named(entry, "RDSon RON ~on-resistance").find_map(|r| read(r, "max", "ohm"));
    if let (Some((r, how)), Some((i, from))) = (ron, ceiling) {
        let why =
            format!("pass element fully on: RDS(on) × current ceiling, {how} × {i} A ({from})");
        out.push(("dropout_v", r * i, why));
    }
    out
}

/// `ratings.max_junction_temp_c` when the card omits it: the highest maximum a
/// row naming the junction temperature (`TJ`, "junction temperature") prints in
/// °C. A part is rated to the hottest junction its datasheet allows anywhere
/// (the key's definition), so a lower operating-range row does not cap it.
pub fn junction_limit(entry: &ModelEntry) -> Option<(f64, String)> {
    if entry.ratings.max_junction_temp_c.is_some() {
        return None;
    }
    let named = |r: &Characteristic| {
        let flat: String = (r.parameter.to_lowercase().chars())
            .filter(|c| c.is_alphanumeric())
            .collect();
        let celsius = matches!(r.unit.replace(['°', ' ', '℃'], "").as_str(), "C" | "");
        celsius
            && !flat.contains("storage")
            && !flat.contains("stg")
            && (flat.contains("junctiontemp") || symbol(r, "TJ") || symbol(r, "TJmax"))
    };
    let rows = cited_rows(entry).iter().filter(|r| named(r));
    let best = rows
        .filter_map(|r| Some((r.max?, r)))
        .max_by(|a, b| a.0.total_cmp(&b.0))?;
    let (v, r) = best;
    (30.0..=400.0).contains(&v).then(|| {
        let how = format!(
            "the hottest printed junction temperature, '{}' max {v} ({})",
            r.parameter, r.conditions
        );
        (v, how)
    })
}

/// A load or analog switch's ron, roff and vth.
///
/// roff: an off switch passing leakage I at voltage V across it is V/I; V is
/// the voltage the leakage row is tested at, else the highest recommended
/// supply, else the rated voltage, and I its maximum (the guaranteed bound,
/// which gives the lowest, conservative roff). A switch with no off-state
/// leakage row uses its shutdown-current row, the same path to its input.
///
/// vth: a printed control threshold (typ), else the control input's VIH
/// minimum: between VIL max and VIH min the state is unspecified, and VIH min
/// is the level the datasheet guarantees switches the part.
fn switch(entry: &ModelEntry) -> Vec<Derived> {
    let mut out = Vec::new();
    out.extend(
        named(entry, "RON RDSon")
            .find_map(|r| cell("ron", r))
            .map(|(v, how)| ("ron", v, how)),
    );
    let control = |r: &Characteristic| {
        let text = r.parameter.to_lowercase();
        ["enable", "control", "input leak", "logic"]
            .iter()
            .any(|w| text.contains(w))
            || ["EN", "CTRL", "SEL", "IEN", "IIN"]
                .iter()
                .any(|s| symbol(r, s))
    };
    let leak = "IOFF ILEAK ILKG IOUToff IDoff ISoff INOoff INCoff ICOMoff ~off-state ~off_leakage";
    let rows = || named(entry, leak).filter(|r| !control(r));
    let shutdown = || named(entry, "ISD ISHDN ~shutdown_current").filter(|r| !control(r));
    let current = rows()
        .chain(shutdown())
        .find_map(|r| Some((r, read(r, "max", "A")?)));
    let supply = entry.envelope.iter().filter_map(|e| match e {
        OperatingEnvelope::SupplyRange { max_v, .. } => Some(*max_v),
        _ => None,
    });
    let supply = supply
        .reduce(f64::max)
        .map(|v| (v, "the highest recommended supply"));
    let rated = entry
        .ratings
        .max_voltage_v
        .map(|v| (v, "the rated voltage"));
    if let Some((r, (i, how))) = current.filter(|c| c.1 .0 > 0.0) {
        let tested = quantity(&r.conditions, "V_?(?:IN|I|S|D|COM|NO|NC|BUS|DS)", "V")
            .map(|v| (v, "its test voltage"));
        if let Some((v, at)) = tested.or(supply).or(rated) {
            let roff = v / i;
            let why = format!("off-state leakage: {v} V ({at}) / {how}");
            out.extend((1e3..=1e12).contains(&roff).then_some(("roff", roff, why)));
        }
    }
    let threshold = named(
        entry,
        "VTH VIT VENth VONth ~en_threshold ~enable_threshold ~on_threshold",
    )
    .filter(|r| r.typ.is_some())
    .find_map(|r| read(r, "typ", "V"))
    .or_else(|| named(entry, "VIH VENH VIHEN").find_map(|r| read(r, "min", "V")));
    out.extend(threshold.map(|(v, how)| ("vth", v, format!("control threshold, {how}"))));
    out
}

/// A diode's bv when it prints no breakdown row, and a law through one VF row.
///
/// bv: the rated reverse voltage (VRRM, VRWM or VR) is the highest the part is
/// guaranteed to block, so breakdown sits at or above it; ibv is the reverse
/// current printed at that voltage, the point the law then passes through.
///
/// is: one VF row cannot separate n from is, so n is the ideal-diode 1 the
/// binder applies to a card without one (or the card's own n, fitted to a
/// curve) and rs 0 (or the card's), and is = IF / (exp((VF − IF·rs)/(n·VT)) − 1)
/// puts the law through the printed point at the solver's nominal 27 °C.
fn diode(entry: &ModelEntry) -> Vec<Derived> {
    let mut out = Vec::new();
    if named(entry, "VBR VBRR BV VZ*").next().is_none() {
        let bv = named(entry, "VRRM VRWM VR VRM").find_map(|r| cell("bv", r));
        if let Some((bv, how)) = bv.filter(|b| b.0 > 0.0) {
            let at = |r: &&Characteristic| {
                quantity(&r.conditions, "V_?R", "V").is_some_and(|v| (v - bv).abs() <= 0.01 * bv)
            };
            let ibv = named(entry, "IR IRM IRRM")
                .filter(at)
                .find_map(|r| cell("ibv", r));
            out.extend(ibv.map(|(i, w)| ("ibv", i, format!("the reverse current at bv, {w}"))));
            out.push((
                "bv",
                bv,
                format!("no breakdown row: the rated reverse voltage, {how}"),
            ));
        }
    }
    let p = &entry.params;
    if let ([(i, v, r)], false) = (&vf_points(cited_rows(entry))[..], p.0.contains_key("is")) {
        let (n, rs) = (
            p.get_f64("n").unwrap_or(1.0),
            p.get_f64("rs").unwrap_or(0.0),
        );
        let vt = n * hauksbee_ir::thermal_voltage_c(27.0);
        let is = i / ((v - i * rs) / vt).exp_m1();
        if (1e-30..=1e-3).contains(&is) {
            let how = format!("one VF row, '{}' {v} V at IF = {i} A: is = IF / (exp((VF - IF·rs)/(n·VT)) - 1) at n = {n}, rs = {rs}", r.parameter);
            out.push(("is", is, how.clone()));
            out.push((
                "n",
                n,
                format!("{how} (n = 1, the ideal diode, with no curve to fit)"),
            ));
            out.push((
                "rs",
                rs,
                format!("{how} (rs = 0: one point fixes no slope)"),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture card: `kind`, its params, and `+`-joined rows `parameter ;
    /// columns ; unit ; conditions`, then any extra TOML.
    fn card(kind: &str, params: &str, rows: &str, extra: &str) -> ModelEntry {
        let row = |spec: &str| {
            let [p, cols, unit, cond] = spec.split(" ; ").collect::<Vec<_>>()[..] else {
                panic!("{spec}")
            };
            // `-`: a row printing no number.
            let cols = if cols == "-" {
                String::new()
            } else {
                cols.replace(", ", "\n")
            };
            format!("[[models.source.characteristic]]\nparameter = \"{p}\"\n{cols}\nunit = \"{unit}\"\nconditions = \"{cond}\"\nbasis = \"b\"\n")
        };
        let rows: String = rows
            .split(" + ")
            .filter(|r| !r.is_empty())
            .map(row)
            .collect();
        let src = format!("[[models]]\nid = \"xq100\"\nkind = \"{kind}\"\ndescription = \"t\"\n[models.params]\n{params}\n{extra}\n[models.source]\ntier = \"datasheet-derived\"\n{rows}");
        let db: crate::schema::DbFile = toml::from_str(&src).expect("fixture parses");
        db.models.into_iter().next().expect("one entry")
    }

    fn got(e: &ModelEntry) -> String {
        let all = conventions(e)
            .into_iter()
            .map(|(n, v, _)| format!("{n} = {v:.4e}"));
        all.collect::<Vec<_>>().join(", ")
    }

    /// Each rule from synthetic rows, beside the row it must not read: another
    /// overdrive, a hot row, a shutdown row, a stated param. Case: kind |
    /// params | rows | derived.
    #[test]
    #[rustfmt::skip]
    fn each_convention_reads_its_row_and_only_its_row() {
        let mut bad = Vec::new();
        for case in [
            // Diode: VZ-prefixed symbols are handled by `derivations`; VR fallback here.
            "diode |  | VRRM peak reverse ; max = 40 ; V ; - + IR ; max = 0.5 ; mA ; VR = 40 V | ibv = 5.0000e-4, bv = 4.0000e1",
            "diode |  | V(BR) ; min = 45 ; V ; IR = 1 mA + VRRM ; max = 40 ; V ; - | ",
            "diode |  | VF forward ; max = 1 ; V ; IF = 10 mA | is = 1.6186e-19, n = 1.0000e0, rs = 0.0000e0",
            "diode | n = 2.0 | VF forward ; max = 1 ; V ; IF = 10 mA | is = 4.0232e-11, rs = 0.0000e0",
            "diode |  | VF ; max = 1 ; V ; IF = 10 mA + VF ; max = 1.1 ; V ; IF = 100 mA | ",
            // Op-amp swing from headroom rows, never an absolute level.
            "opamp |  | VOH swing from V+ ; typ = 10, max = 25 ; mV ; RL = 10 kΩ + VOL ; max = 20 ; mV ; RL = 10 kΩ | swing_lo = 2.0000e-2, swing_hi = 2.5000e-2",
            "opamp |  | VOH ; min = 4.8 ; V ; VS = 5 V | ",
            "opamp | rail_hi = 4.8 | VOH ; max = 25 ; mV ; - | ",
            // Comparator: levels, delay at the largest overdrive, supply current.
            "comparator |  | VOL ; max = 0.4 ; V ; ISINK = 4 mA + VOH ; min = 4.6 ; V ; VCC = 5 V | out_lo = 4.0000e-1, out_hi = 4.6000e0",
            "comparator |  | VOH high level ; max = 0.2 ; V ; VCC = 3.3 V | out_hi = 3.1000e0",
            "comparator |  | tPD ; typ = 450 ; ns ; VOD = 5 mV + tPD ; typ = 80 ; ns ; VOD = 100 mV + tPD ; typ = 60 ; ns ; VOD = 100 mV, TA = 125 °C | tpd_s = 8.0000e-8",
            "comparator |  | tPLH ; typ = 40 ; ns ; 20 mV overdrive + tPHL ; typ = 55 ; ns ; 20 mV overdrive | tpd_s = 5.5000e-8",
            "comparator | open_collector = true | VOL ; max = 0.4 ; V ; - + ICC ; typ = 0.8, max = 1.5 ; mA ; - | supply_static_ua = 8.0000e2",
            "digital |  | ICC shutdown ; max = 1 ; uA ; - + ICC ; max = 20 ; uA ; VCC = 5.5 V | supply_static_ua = 2.0000e1",
            // Round-3 drafts: the static draw is never a bus-active or write row (PCF8563, 24AA025UID).
            "digital |  | IDD supply current ; max = 200 ; uA ; Interface active; fSCL = 100 kHz + IDD supply current ; typ = 250, max = 500 ; nA ; Interface inactive (fSCL = 0 Hz); VDD = 3.0 V | supply_static_ua = 2.5000e-1",
            "digital |  | ICC write, operating current ; typ = 0.1, max = 3 ; mA ; VCC = 5.5 V, SCL = 400 kHz + ICC read, operating current ; max = 1 ; mA ; VCC = 5.5 V + ICCS, standby current ; typ = 0.01, max = 1 ; uA ; VCC = 5.5 V | supply_static_ua = 1.0000e-2",
            // A ratiometric level scales by the supply the card's own levels are read at (CY8CMBR3108).
            "digital | voh = 2.4 | VOH, output voltage HIGH level ; min = 2.4 ; V ; IOH = −4 mA at 3 V VDD + VDD, chip supply voltage ; min = 1.71, typ = 1.8, max = 1.89 ; V ; VCC shorted to VDD | vih = 2.1000e0 | [[models.source.uncertainty]]\nstatus = \"unknown\"\nparameter = \"vih_xres\"\nreason = \"Table 15 gives VIH_XRES as 0.7 × VDD\"",
            // A row recording its own 0.7 VDD coefficient as volts is no level (PCF8563).
            "digital |  | VIH high-level input voltage ; min = 0.7, max = 5.5 ; V ; Lower limit 0.7 VDD | ",
            // A buck's dropout row is its internal VCC LDO's (MAX17760).
            "vreg |  | VCC dropout voltage ; max = 0.27 ; V ; VIN = 4.5V, IVCC = 5mA |  | [models.behavioral.converter]\ntopology = \"buck\"\nout_pin = \"sw\"\nin_pin = \"in\"\nvout_setpoint = 5.0",
            // Regulator: no-load iq, charging iq, dropout at the highest load of its own option.
            "vreg | vout = 3.3 | IQ ; typ = 300 ; uA ; IOUT = 100 mA + IQ ; typ = 50 ; uA ; IOUT = 0 mA + IQ shutdown ; max = 1 ; uA ; - | iq_a = 5.0000e-5",
            "vreg |  | ICC standby ; typ = 50 ; uA ; - + ICC charge mode ; typ = 500 ; uA ; charging | iq_a = 5.0000e-4",
            "vreg | vout = 3.3 | VDO ; max = 150 ; mV ; IOUT = 100 mA, VOUT = 3.3 V + VDO ; max = 400 ; mV ; IOUT = 300 mA, VOUT = 3.3 V + VDO ; max = 600 ; mV ; IOUT = 300 mA, VOUT = 1.8 V | dropout_v = 4.0000e-1",
            "vreg | vout = 4.2 | RDS(on) pass ; typ = 0.4, max = 0.6 ; Ω ; - | ",
            "vreg | vout = 4.2 | RDS(on) pass ; typ = 0.4, max = 0.6 ; Ω ; - | dropout_v = 3.0000e-1 | [models.ratings]\nmax_current_a = 0.5",
            // Load switch: roff from leakage at its test voltage, vth from VIH min.
            "analog_switch |  | RON ; typ = 80 ; mΩ ; VIN = 5 V + IOFF off-state leakage ; max = 1 ; uA ; VIN = 5.5 V + EN input leakage ; max = 0.1 ; uA ; - + VIH EN ; min = 1.5 ; V ; - | ron = 8.0000e-2, roff = 5.5000e6, vth = 1.5000e0",
            "analog_switch |  | VEN threshold ; typ = 1.1 ; V ; rising + VIH ; min = 1.5 ; V ; - | vth = 1.1000e0",
            "analog_switch |  | ISD shutdown current ; max = 2 ; uA ; - | roff = 3.0000e6 | [models.ratings]\nmax_voltage_v = 6.0",
            // Digital: levels as a fraction of a supply, from a row or an unknown record.
            "digital |  | VIH ; min = 70 ; % ; of VDDIO, VDDIO = 1.8 V + VIL ; - ; V ; 0.2 × VDDIO, VDDIO = 1.8 V + VOL ; max = 0.4 ; V ; IOL = 3 mA | vih = 1.2600e0, vil = 3.6000e-1, vol = 4.0000e-1",
            "digital |  | VDDIO supply ; min = 1.2, typ = 1.8, max = 3.6 ; V ; - | voh = 1.6000e0 | [[models.source.uncertainty]]\nstatus = \"unknown\"\nparameter = \"VOH\"\nreason = \"printed as VDDIO - 0.2 V\"",
            "digital |  | VIH ; min = 2 ; V ; VCC = 5 V + VOL ; max = 0.1 ; V ; - | vih = 2.0000e0, vol = 1.0000e-1",
            "digital | vih = 2.0 | VOL ; max = 0.1 ; V ; - | ",
        ] {
            let [kind, params, rows, want, ref extra @ ..] = case.split(" | ").collect::<Vec<_>>()[..] else { panic!("{case}") };
            let e = card(kind, params, rows, extra.first().copied().unwrap_or(""));
            if got(&e) != want {
                bad.push(format!("{case}\n    got: {}", got(&e)));
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    /// The junction limit is the hottest maximum a junction-temperature row
    /// prints, never a storage row, a stated one, or a row without a max.
    #[test]
    fn junction_limit_is_the_hottest_printed_junction_maximum() {
        let rows = "Junction temperature TJ ; max = 150 ; °C ; absolute maximum + Operating junction temperature ; min = -40, max = 125 ; °C ; - + Storage temperature ; max = 175 ; °C ; -";
        let e = card("opamp", "", rows, "");
        assert_eq!(junction_limit(&e).map(|j| j.0), Some(150.0));
        let stated = card(
            "opamp",
            "",
            rows,
            "[models.ratings]\nmax_junction_temp_c = 125.0",
        );
        assert!(junction_limit(&stated).is_none());
        let none = card(
            "opamp",
            "",
            "TJ ; typ = 25 ; °C ; - + Storage temperature ; max = 175 ; °C ; -",
            "",
        );
        assert!(junction_limit(&none).is_none());
    }

    #[test]
    fn overdrive_reads_either_order_in_volts() {
        let cases = [
            ("VOD = 5 mV", 5e-3),
            ("100-mV overdrive", 0.1),
            ("Overdrive: 20 mV", 0.02),
            ("RL = 5 kΩ", 0.0),
        ];
        for (text, want) in cases {
            assert!((overdrive(text) - want).abs() < 1e-12, "{text}");
        }
    }
}
