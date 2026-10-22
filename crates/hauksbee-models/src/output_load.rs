//! A logic output's level is its LIGHT-LOAD level; the heavy-load row is its
//! output resistance.
//!
//! Datasheets print VOH/VOL at several test currents: a light one (IOH =
//! -100 uA, typically VCC - 0.2 V) and heavy ones (IOH = -24 mA, 2.0 V at VCC
//! 3 V). The binder drives an output as `voh` behind `ro`
//! ([`crate::schema::ModelEntry`] params), so `voh` is the unloaded level and
//! the heavy row is the drop across `ro`. A card whose `voh`/`vol` is the
//! heavy row drives an unloaded output at the loaded level: a 74LVC244A on
//! 3.3 V read 2.0 V instead of about 3.1 V.
//!
//! [`light_load_levels`] repairs such a card at load time, the same way for
//! every digital card: the level row becomes the light-load level (the card's
//! own light-load row when it cites one, else the rail, an assumption it
//! states), `ro` is derived from the heavy row when the card states none, and
//! both, with the working, are recorded on the evidence block.
//! [`heavy_level_warnings`] names the slip so extraction writes the
//! light-load row in the first place.

use std::sync::LazyLock;

use hauksbee_ir::evidence::{ModelIntervalKind, ModelUncertainty};
use regex::Regex;

use crate::param_names::{cited_rows, quantity, row_value, symbol};
use crate::schema::{Characteristic, ComponentKind, ModelEntry, ParamValue};

/// The test current (A) above which a level row is a loaded level.
///
/// The light-load rows logic families print are 20 uA (HC/HCT) to 100 uA
/// (LVC/AHC/AVC); their drive rows start at 1 mA (LVC at 1.2 V) and run to
/// 64 mA (ABT). TTL-compatible outputs (parallel EEPROMs, RS-485 receivers)
/// print their one VOH row at -400 uA, a load the same way, so the line is drawn
/// between the two at 250 uA.
pub const HEAVY_TEST_A: f64 = 250e-6;

/// A level row's test current magnitude in A (`IOH = -24 mA`, `IO = 100 uA`).
#[must_use]
pub fn test_current_a(row: &Characteristic) -> Option<f64> {
    quantity(&row.conditions, r"(?i:I_?(?:OH|OL|O))", "A")
}

/// A level row's test supply in V (`VCC = 3 V`); `None` for a range.
fn test_supply_v(row: &Characteristic) -> Option<f64> {
    let text = &row.conditions;
    range(text)
        .is_none()
        .then(|| quantity(text, r"[AD]?V_?[A-Z]*(?:CC|DD)[A-Z]*", "V"))?
}

/// A printed supply range (`1.65 V to 3.6 V`).
fn range(text: &str) -> Option<(f64, f64)> {
    static RANGE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(\d*\.?\d+)\s*V?\s*(?:to|–|\.\.)\s*(\d*\.?\d+)\s*V").expect("a valid pattern")
    });
    let c = RANGE.captures(text)?;
    let (a, b) = (c[1].parse::<f64>().ok()?, c[2].parse::<f64>().ok()?);
    (a > 0.0 && b > a).then_some((a, b))
}

/// The drop a supply-relative level prints below its rail (`VCC - 0.2`).
fn rail_drop(text: &str) -> Option<f64> {
    static DROP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\bV_?[A-Z]*(?:CC|DD)[A-Z_]*\s*[-−–]\s*(\d*\.?\d+)\s*(m?V)?")
            .expect("a valid pattern")
    });
    let c = DROP.captures(text)?;
    let x: f64 = c[1].parse().ok()?;
    Some(if c.get(2).is_some_and(|u| u.as_str() == "mV") {
        x * 1e-3
    } else {
        x
    })
}

/// The heavy-load level row a `voh`/`vol` param is: `(row, level V, test
/// current A, test supply V)`.
fn heavy_row<'a>(entry: &'a ModelEntry, name: &str) -> Option<(&'a Characteristic, f64, f64, f64)> {
    let Some(ParamValue::Row(row)) = entry.params.0.get(name) else {
        return None;
    };
    let current = test_current_a(row).filter(|i| *i >= HEAVY_TEST_A)?;
    let level = row_value(name, row).ok()?;
    Some((row, level, current, test_supply_v(row)?))
}

/// A light-load level: its value at the test supply, the working, and, for a
/// level that follows the rail, the supply range it holds over.
type Light = (f64, String, Option<(f64, f64)>);

/// The light-load level the card cites for `sym` at supply `vcc`.
///
/// A supply-relative row (`VCC - 0.2`) holds over the range it prints, else
/// the card's supply range; an absolute one is that level at `vcc` only.
fn cited_light(entry: &ModelEntry, sym: &str, vcc: f64) -> Option<Light> {
    let light =
        |r: &&Characteristic| symbol(r, sym) && test_current_a(r).is_some_and(|i| i < HEAVY_TEST_A);
    cited_rows(entry).iter().filter(light).find_map(|r| {
        let text = format!("{} {} {}", r.parameter, r.conditions, r.basis);
        if let Some(d) = rail_drop(&text).filter(|_| sym == "VOH") {
            let how = format!(
                "'{}' ({}): VCC - {d} V at VCC {vcc} V",
                r.parameter, r.conditions
            );
            let span = range(&r.conditions).or_else(|| supply_range(entry));
            return Some((vcc - d, how, span));
        }
        let at = test_supply_v(r).is_none_or(|v| (v - vcc).abs() <= 0.02 * vcc);
        let column = if sym == "VOH" {
            r.min.or(r.typ)
        } else {
            r.max.or(r.typ)
        };
        let scale = crate::units::scale_between(&r.unit, "V")?;
        let v = column.filter(|_| at)? * scale;
        Some((
            v,
            format!("'{}' ({}): {v} V", r.parameter, r.conditions),
            None,
        ))
    })
}

/// The card's own supply range on its `vcc`/`vdd` pin.
fn supply_range(entry: &ModelEntry) -> Option<(f64, f64)> {
    entry.envelope.iter().find_map(|e| match e {
        crate::schema::OperatingEnvelope::SupplyRange {
            pin, min_v, max_v, ..
        } if ["vcc", "vdd"].iter().any(|r| pin.eq_ignore_ascii_case(r)) => Some((*min_v, *max_v)),
        _ => None,
    })
}

/// The light-load level for a card that cites none: the rail, an assumption
/// stated in the working. "The rail" holds at every supply, so a high level's
/// table spans them all.
fn assumed_light(entry: &ModelEntry, sym: &str, high: bool, vcc: f64) -> Light {
    let (v, at) = if high {
        (vcc, "its rail")
    } else {
        (0.0, "ground")
    };
    let how = format!(
        "ASSUMED {at} ({v} V): the card cites no light-load {sym} row, and a CMOS output \
         with no load sits at the rail it switches to"
    );
    let span = high.then(|| supply_range(entry).unwrap_or((0.0, 100.0)));
    (v, how, span)
}

/// `ro` from each repaired level's drop, when the card states none: the larger
/// (weaker) side, since the binder drives high and low through one `ro`.
fn derive_ro(entry: &mut ModelEntry, ro: &[(f64, String)]) -> Option<ModelUncertainty> {
    let worst = ro.iter().map(|r| r.0).fold(0.0_f64, f64::max);
    let worst = (worst * 10.0).round() / 10.0;
    if worst <= 0.0 || entry.params.0.contains_key("ro") {
        return None;
    }
    entry
        .params
        .0
        .insert("ro".to_string(), ParamValue::Float(worst));
    let low = ro.iter().map(|r| r.0).fold(f64::INFINITY, f64::min);
    let working: Vec<_> = ro.iter().map(|r| r.1.as_str()).collect();
    let basis = format!(
        "ro = (unloaded level - loaded level) / |test current|, the larger side kept: {}",
        working.join("; ")
    );
    ModelUncertainty::interval_with_kind(
        "ro",
        (low * 10.0).round() / 10.0,
        worst,
        "ohm",
        ModelIntervalKind::EstimatedRange,
        basis,
    )
    .ok()
}

/// Repair a digital card whose `voh`/`vol` is printed at a heavy test current.
///
/// See the module docs. Returns the evidence: each repaired level's heavy row
/// (as a characteristic, so it stays on the card) and an interval record
/// stating the working and any assumption.
pub fn light_load_levels(entry: &mut ModelEntry) -> (Vec<Characteristic>, Vec<ModelUncertainty>) {
    let (mut rows, mut records) = (Vec::new(), Vec::new());
    // An identity-only card is never driven, so its levels stay as written.
    let identity = entry.params.get_bool("identity_only") == Some(true);
    if identity
        || !matches!(
            entry.kind,
            ComponentKind::Digital | ComponentKind::ShiftRegister
        )
    {
        return (rows, records);
    }
    let mut ro: Vec<(f64, String)> = Vec::new();
    for (name, sym, high) in [("voh", "VOH", true), ("vol", "VOL", false)] {
        let Some((row, heavy, current, vcc)) = heavy_row(entry, name) else {
            continue;
        };
        let row = row.clone();
        let (light, how, span) =
            cited_light(entry, sym, vcc).unwrap_or_else(|| assumed_light(entry, sym, high, vcc));
        let drop = (light - heavy).abs();
        let r = drop / current;
        ro.push((
            r,
            format!(
                "{sym} {light} V unloaded vs {heavy} V at {} mA: {r:.1} ohm",
                current * 1e3
            ),
        ));
        entry
            .params
            .0
            .insert(name.to_string(), ParamValue::Float(light));
        // A supply-relative high level follows the rail over the range it is printed for.
        if let Some((lo, hi)) = span.filter(|_| high && !entry.levels.contains_key(name)) {
            let d = vcc - light;
            entry
                .levels
                .insert(name.to_string(), vec![[lo, lo - d], [hi, hi - d]]);
        }
        let basis = format!(
            "the card's {sym} row is printed at a heavy test current ({}, '{}'), a loaded \
             level; {name} is the light-load level, {how}, and the output resistance carries \
             the drop to {heavy} V at that load",
            row.conditions, row.basis
        );
        let (low, high_v) = (light.min(heavy), light.max(heavy));
        let record = ModelUncertainty::interval_with_kind(
            name,
            low,
            high_v,
            "V",
            ModelIntervalKind::EstimatedRange,
            basis,
        );
        records.extend(record.ok());
        rows.extend(crate::param_names::row_characteristic(name, &row));
    }
    records.extend(derive_ro(entry, &ro));
    (rows, records)
}

/// Warnings for a `voh`/`vol` taken at a heavy test current.
///
/// That is a row param, a number equal to a cited heavy-load row's guaranteed
/// column, or a number on a card whose cited rows for that level are ALL heavy-load
/// ones, so the number can only have come from one of them (a rail-relative
/// `0.8 x VDDIO min, IOH = 1 mA` row prints no column to equal).
///
/// A card with no `voh` is an open-drain (or input-only) part, whose `vol` is the
/// pull-down level at its rated sink current: the level it holds under that load,
/// not an unloaded one, so the cited-rows rule leaves it alone.
#[must_use]
pub fn heavy_level_warnings(entry: &ModelEntry) -> Vec<String> {
    // The cards [`light_load_levels`] repairs: an identity-only card is never driven.
    let identity = entry.params.get_bool("identity_only") == Some(true);
    if identity
        || !matches!(
            entry.kind,
            ComponentKind::Digital | ComponentKind::ShiftRegister
        )
    {
        return Vec::new();
    }
    let mut out = Vec::new();
    let pushes_high = entry.params.0.contains_key("voh");
    for (name, sym) in [("voh", "VOH"), ("vol", "VOL")] {
        let heavy = |r: &Characteristic| test_current_a(r).filter(|i| *i >= HEAVY_TEST_A);
        let rows: Vec<&Characteristic> = cited_rows(entry)
            .iter()
            .filter(|r| symbol(r, sym))
            .collect();
        let current = match entry.params.0.get(name) {
            Some(ParamValue::Row(row)) => heavy(row),
            Some(value) => value.as_f64().and_then(|v| {
                let equal = rows.iter().find_map(|r| {
                    let x = row_value(name, r).ok()?;
                    heavy(r).filter(|_| (x - v).abs() <= 1e-9 * v.abs().max(1.0))
                });
                let only_heavy = || {
                    let all_heavy = rows.iter().all(|r| heavy(r).is_some());
                    let first = rows.iter().find_map(|r| heavy(r));
                    first.filter(|_| all_heavy && pushes_high && !entry.params.0.contains_key("ro"))
                };
                equal.or_else(only_heavy)
            }),
            None => None,
        };
        if let Some(i) = current {
            out.push(format!(
                "param '{name}' is the {sym} row at a heavy test current ({:.3} mA), a loaded \
                 level: state the light-load row (|IO| about 100 uA, often VCC - 0.2 V, as a \
                 [models.levels] {name} table when it follows VCC) as {name}, and derive ro = \
                 |{sym}_light - {sym}_heavy| / |IO| from the heavy row",
                i * 1e3
            ));
        }
    }
    out
}

/// [`heavy_level_warnings`] for the card as it binds.
///
/// [`light_load_levels`] has first turned every heavy row it can into the
/// light-load level behind `ro`, so only a level the loader cannot repair (no
/// stated supply, a bare scalar taken from a heavy row) is a finding.
#[must_use]
pub fn unrepaired_level_warnings(entry: &ModelEntry) -> Vec<String> {
    let mut bound = entry.clone();
    light_load_levels(&mut bound);
    heavy_level_warnings(&bound)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(params: &str, extra: &str) -> ModelEntry {
        let src = format!(
            "[[models]]\nid = \"t\"\nkind = \"digital\"\ndescription = \"t\"\n[models.params]\n{params}\n{extra}\n"
        );
        let db: crate::schema::DbFile = toml::from_str(&src).expect("fixture parses");
        db.models.into_iter().next().expect("one entry")
    }

    /// TI SN74LVC244A (SCAS414AG) 5.5, -40 to 125 °C: VOH 2.0 V min and VOL
    /// 0.8 V max at VCC 3 V, |IO| 24 mA; VOH VCC - 0.3 V and VOL 0.3 V at
    /// 100 uA over 1.65 V to 3.6 V.
    const HEAVY: &str = "voh = { min = 2.0, unit = \"V\", conditions = \"VCC = 3 V, IOH = -24 mA\", basis = \"5.5 VOH\" }\n\
        vol = { max = 0.8, unit = \"V\", conditions = \"VCC = 3 V, IOL = 24 mA\", basis = \"5.5 VOL\" }";

    #[test]
    fn a_heavy_load_level_becomes_the_light_load_level_and_its_resistance() {
        let light = "[models.source]\ntier = \"datasheet-derived\"\n\
            [[models.source.characteristic]]\nparameter = \"VOH\"\nunit = \"V\"\n\
            conditions = \"VCC = 1.65 V to 3.6 V, IOH = -100 uA\"\nbasis = \"5.5 VOH, VCC - 0.3\"\n\
            [[models.source.characteristic]]\nparameter = \"VOL\"\nmax = 0.3\nunit = \"V\"\n\
            conditions = \"VCC = 1.65 V to 3.6 V, IOL = 100 uA\"\nbasis = \"5.5 VOL\"";
        let mut entry = card(HEAVY, light);
        assert!(
            heavy_level_warnings(&entry).len() == 2,
            "{:?}",
            heavy_level_warnings(&entry)
        );
        let (rows, records) = light_load_levels(&mut entry);
        let p = &entry.params;
        assert!((p.get_f64("voh").unwrap() - 2.7).abs() < 1e-9);
        assert!((p.get_f64("vol").unwrap() - 0.3).abs() < 1e-9);
        // (2.7 - 2.0) / 24 mA = 29.2 ohm against (0.8 - 0.3) / 24 mA = 20.8 ohm.
        assert!((p.get_f64("ro").unwrap() - 29.2).abs() < 1e-9);
        let table = &entry.levels["voh"];
        assert!((schema_level(table, 3.3) - 3.0).abs() < 1e-9);
        assert_eq!(rows.len(), 2);
        assert_eq!(records.len(), 3);
        assert!(heavy_level_warnings(&entry).is_empty());
    }

    #[test]
    fn with_no_light_row_the_rail_is_stated_as_an_assumption() {
        let mut entry = card(HEAVY, "");
        let (_, records) = light_load_levels(&mut entry);
        assert!((entry.params.get_f64("voh").unwrap() - 3.0).abs() < 1e-9);
        assert!(entry.params.get_f64("vol").unwrap().abs() < 1e-9);
        // (3.0 - 2.0) / 24 mA = 41.7 ohm.
        assert!((entry.params.get_f64("ro").unwrap() - 41.7).abs() < 1e-9);
        let ModelUncertainty::Interval { basis, kind, .. } = &records[0] else {
            panic!("an interval: {records:?}");
        };
        assert_eq!(*kind, ModelIntervalKind::EstimatedRange);
        assert!(basis.contains("ASSUMED its rail"), "{basis}");
    }

    #[test]
    fn a_light_load_or_stated_card_is_left_alone() {
        let light = "voh = { min = 3.1, unit = \"V\", conditions = \"VCC = 3.3 V, IOH = -100 uA\", basis = \"VOH\" }\nro = 25.0";
        let mut entry = card(light, "");
        let before = format!("{:?}", entry.params);
        let (rows, records) = light_load_levels(&mut entry);
        assert!(rows.is_empty() && records.is_empty());
        assert_eq!(format!("{:?}", entry.params), before);
        let mut nmos = card(HEAVY, "");
        nmos.kind = ComponentKind::Nmos;
        assert!(light_load_levels(&mut nmos).1.is_empty());
    }

    fn schema_level(table: &[[f64; 2]], vcc: f64) -> f64 {
        crate::schema::level_at(table, vcc)
    }

    #[test]
    fn a_scalar_level_whose_only_cited_rows_are_heavy_is_flagged() {
        let heavy_row = "[models.source]\ntier = \"datasheet-derived\"\n\
            [[models.source.characteristic]]\nparameter = \"VOH\"\nunit = \"V\"\n\
            conditions = \"VDDIO = 1.62 V, IOH = -1 mA\"\nbasis = \"0.8 VDDIO min\"\n\
            [[models.source.characteristic]]\nparameter = \"VOL\"\nunit = \"V\"\n\
            conditions = \"VDDIO = 1.62 V, IOL = 3 mA\"\nbasis = \"0.2 VDDIO max\"";
        let warned = |params: &str| heavy_level_warnings(&card(params, heavy_row)).len();
        // Neither number equals a printed column, yet each can only have come from a heavy row.
        assert_eq!(warned("voh = 2.64\nvol = 0.66"), 2);
        // A stated ro means the heavy row was already turned into a resistance.
        assert_eq!(warned("voh = 2.64\nvol = 0.66\nro = 660.0"), 0);
        // An open-drain output has no voh: its vol is the level at the rated sink current.
        assert_eq!(warned("vol = 0.4"), 0);
    }

    /// The unloaded high level of every builtin push-pull digital card sits at its
    /// rail, and the lint finds nothing left to say about any of them.
    ///
    /// Read as cards load: a row printed at a heavy test current has been turned into
    /// the light-load level behind `ro`. The level is held to the card's
    /// `[models.levels]` table where it has one (every point of it); else to a
    /// supply the part runs on: the ends of its supply range and the standard rails
    /// inside it (a scalar printed at a family's 4.5 V test supply is right there),
    /// or, for a TTL-level part that states no range, 4.5 V and 5 V. Open-drain cards
    /// (no `voh`) and identity-only ones, which are never driven, are exempt.
    ///
    /// `UNRESOLVED` lists the cards whose datasheet could not be read to re-state
    /// the level at its true test current. It is a ratchet: a card that stops
    /// offending must be taken off the list.
    #[test]
    fn every_builtin_digital_card_drives_its_rail_unloaded() {
        const RAILS: [f64; 6] = [1.2, 1.8, 2.5, 3.3, 4.5, 5.0];
        const WITHIN_V: f64 = 0.3;
        const UNRESOLVED: [(&str, &str); 14] = [
            ("bma423", "datasheet download returns 404; the 0.8 VDDIO level has no stated test current"),
            ("cp2102n_qfn28", "manufacturer download blocked; level table not re-checked against the PDF"),
            ("max3237e", "manufacturer download blocked; level table not re-checked against the PDF"),
            ("bmp581", "Table 17 prints 80 % VDDIO with no test current; the drive-strength tables are images"),
            ("lis3dh", "prints 0.9 VDDIO min with no test current"),
            ("mcp23017_spdip_soic_ssop28", "the datasheet prints only the heavy-current rows (VOL 0.6 V at 8 mA, VOH VDD - 0.7 V at -3 mA) and no light-load row to re-state"),
            ("mcp23017_qfn28", "the datasheet prints only the heavy-current rows (VOL 0.6 V at 8 mA, VOH VDD - 0.7 V at -3 mA) and no light-load row to re-state"),
            ("mcp23s17_spdip_soic_ssop28", "the datasheet prints only the heavy-current rows (VOL 0.6 V at 8 mA, VOH VDD - 0.7 V at -3 mA) and no light-load row to re-state"),
            ("mcp23s17_qfn28", "the datasheet prints only the heavy-current rows (VOL 0.6 V at 8 mA, VOH VDD - 0.7 V at -3 mA) and no light-load row to re-state"),
            ("mcp23008_pdip_soic18", "the datasheet prints only the heavy-current rows (VOL 0.6 V at 8 mA, VOH VDD - 0.7 V at -3 mA) and no light-load row to re-state"),
            ("mcp23008_ssop20", "the datasheet prints only the heavy-current rows (VOL 0.6 V at 8 mA, VOH VDD - 0.7 V at -3 mA) and no light-load row to re-state"),
            ("mcp23008_qfn20", "the datasheet prints only the heavy-current rows (VOL 0.6 V at 8 mA, VOH VDD - 0.7 V at -3 mA) and no light-load row to re-state"),
            ("pca9685_tssop28", "the datasheet prints the totem-pole VOH only at -10 mA (Table 14) and no light-load row to re-state"),
            ("pca9685_hvqfn28", "the datasheet prints the totem-pole VOH only at -10 mA (Table 14) and no light-load row to re-state"),
        ];
        let (mut checked, mut offenders) = (0, Vec::<(String, String)>::new());
        for (name, src) in crate::BUILTIN_TOML_FILES {
            let db: crate::schema::DbFile = toml::from_str(src).expect(name);
            for mut entry in db.models {
                let driven = matches!(
                    entry.kind,
                    ComponentKind::Digital | ComponentKind::ShiftRegister
                );
                let exempt = entry.params.get_bool("identity_only") == Some(true)
                    || !entry.params.0.contains_key("voh");
                if !driven || exempt {
                    continue;
                }
                light_load_levels(&mut entry);
                offenders.extend(
                    heavy_level_warnings(&entry)
                        .into_iter()
                        .map(|w| (entry.id.clone(), format!("{name}: {}: lint: {w}", entry.id))),
                );
                checked += 1;
                let Some(voh) = entry.params.get_f64("voh") else {
                    offenders.push((
                        entry.id.clone(),
                        format!("{name}: {}: voh is not a number", entry.id),
                    ));
                    continue;
                };
                // Within `WITHIN_V` below a supply, and never above it.
                let at =
                    |vcc: f64, level: f64| level <= vcc + 1e-9 && vcc - level <= WITHIN_V + 1e-9;
                if let Some(table) = entry.levels.get("voh") {
                    for p in table.iter().filter(|p| !at(p[0], p[1])) {
                        offenders.push((
                            entry.id.clone(),
                            format!(
                                "{name}: {}: unloaded voh {} V on a {} V rail",
                                entry.id, p[1], p[0]
                            ),
                        ));
                    }
                    continue;
                }
                let mut supplies: Vec<f64> = Vec::new();
                for e in &entry.envelope {
                    if let crate::schema::OperatingEnvelope::SupplyRange {
                        pin, min_v, max_v, ..
                    } = e
                    {
                        if pin.starts_with("vcc") || pin.starts_with("vdd") {
                            supplies.extend([*min_v, *max_v]);
                            supplies.extend(RAILS.iter().filter(|r| (*min_v..=*max_v).contains(r)));
                        }
                    }
                }
                if supplies.is_empty() {
                    supplies = vec![4.5, 5.0];
                }
                if !supplies.iter().any(|&vcc| at(vcc, voh)) {
                    offenders.push((
                        entry.id.clone(),
                        format!(
                            "{name}: {}: unloaded voh {voh} V is not within {WITHIN_V} V of any of \
                             its supplies {supplies:?}",
                            entry.id
                        ),
                    ));
                }
            }
        }
        assert!(checked > 40, "only {checked} cards checked");
        let stale: Vec<&str> = UNRESOLVED
            .iter()
            .filter(|(id, _)| !offenders.iter().any(|(o, _)| o == id))
            .map(|(id, _)| *id)
            .collect();
        assert!(
            stale.is_empty(),
            "no longer offending, remove from UNRESOLVED: {stale:?}"
        );
        let new: Vec<&str> = offenders
            .iter()
            .filter(|(id, _)| !UNRESOLVED.iter().any(|(u, _)| u == id))
            .map(|(_, w)| w.as_str())
            .collect();
        assert!(
            new.is_empty(),
            "push-pull digital cards driving a loaded level unloaded:\n  {}",
            new.join("\n  ")
        );
    }
}
