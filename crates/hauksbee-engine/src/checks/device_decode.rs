//! Device-decode check class: configurable controllers whose strap / divider
//! resistors *select a documented operating mode*, decoded against the part's
//! datasheet bands.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! A whole class of parts (USB-PD sink controllers, programmable LDOs, address-
//! strapped peripherals, mode-pin codecs) read an analog resistor-divider voltage
//! on a configuration pin and decode it against a published table of voltage
//! bands. The board author picks resistor values to land the pin in the band for
//! the mode they want. If the chosen values miss the band, the part silently
//! enters the *wrong* mode. A value/short sweep cannot see this: every resistor
//! is in spec and every net is connected; the fault is that the divider decodes
//! to a band the author did not intend.
//!
//! ## Honest scope
//!
//! This is a **per-part** check that **grows incrementally**. There is no generic
//! "decode any config pin" engine, because each part has its own pins, its own
//! band table, and its own consistency rules (e.g. the CYPD3177's VBUS_MIN >
//! VBUS_MAX override). Each supported part is a hand-written decoder seeded from
//! its datasheet. Today exactly one part is seeded: the Cypress / Infineon
//! **CYPD3177** (EZ-PD BCR) USB-C PD sink controller. Adding a second part means
//! adding a second decoder; this is by design, not a stub.
//!
//! ## Zero-false-positive discipline (binding)
//!
//! The check fires ONLY when:
//!   - the part is positively identified (value/MPN contains the part token), AND
//!   - the configuration divider on the pin resolves to *known* resistor values
//!     (a parseable pull-up to the reference rail and a parseable pull-down to
//!     ground), so the pin voltage is computable.
//! If the divider cannot be resolved to concrete resistor values, the check stays
//! SILENT. A check that cannot resolve the divider does not fire. (See
//! `docs/checks/DEVICE_DECODE.md` and `docs/about/LIMITATIONS.md`.)
//!
//! ## CYPD3177 seed (the hunt this reproduces)
//!
//! `docs/hunts/pd-sink-trigger.md` CANDIDATE 4. The board's rotary VBUS voltage
//! selector mis-codes its top two detents because it keeps a permanent 10k
//! pull-down across the pin and switches an *additional parallel* leg in, which
//! cannot reproduce the datasheet's intended single-pull-down-per-detent codes.
//! Detent 4 ("15 V") decodes to the 12 V band; detent 5 ("20 V") decodes to the
//! 19 V band. Compounded by a hard-wired VBUS_MIN = 19 V, which (datasheet Note 1)
//! overrides VBUS_MAX whenever VBUS_MIN > VBUS_MAX, defeating the selector.
//!
//! ### What this check resolves automatically vs what it cannot
//!
//! Resolves automatically from the netlist:
//!   - VBUS_MAX / VBUS_MIN pins (by pin function name, or by net name fallback).
//!   - The fixed divider on each pin: pull-up resistor to the 3.3 V reference and
//!     the permanent pull-down resistor to ground.
//!   - Switch-selectable extra pull-down legs *when* the selector is a multi-pad
//!     component (e.g. a rotary SW) whose common pad sits on the config net and
//!     whose other pads each reach ground directly or through one resistor: each
//!     such pad is decoded as one detent.
//!   - The Note-1 consistency check (VBUS_MIN band > VBUS_MAX band).
//!
//! Cannot resolve (honest limits):
//!   - It does not know the *intended* voltage label silk-screened next to each
//!     detent. It flags a detent only when the decoded band is internally
//!     inconsistent with the part's own reachable range, OR via the Note-1 check.
//!     The "detent N is labelled 15 V but codes 12 V" mismatch is proven in the
//!     unit tests against the hunt's hand-derived numbers; on a real board it is
//!     reported as the set of distinct bands the selector can reach, so a reviewer
//!     sees that "20 V" is unreachable.
//!   - Selectors wired through parts that do not bind as a multi-pad switch on the
//!     config net are not enumerated; the static (permanent) divider is still
//!     decoded and the Note-1 check still runs.

use hauksbee_extract::{
    Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity,
};
use hauksbee_models::value::parse_value;
use hauksbee_models::ModelLibrary;

/// Reference rail for the CYPD3177 config dividers (VDDD = 3.3 V).
const VDDD: f64 = 3.3;

/// A decoded PD request voltage from a VBUS_MAX / VBUS_MIN divider, in volts.
/// `Faulty` is the catch-all for a voltage above every named band (should not
/// happen for an in-range divider, kept for totality).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdVolts {
    V5,
    V9,
    V12,
    V15,
    V19,
    V20,
}

impl PdVolts {
    /// Nominal volts for ordering / display.
    pub fn volts(self) -> u32 {
        match self {
            PdVolts::V5 => 5,
            PdVolts::V9 => 9,
            PdVolts::V12 => 12,
            PdVolts::V15 => 15,
            PdVolts::V19 => 19,
            PdVolts::V20 => 20,
        }
    }
}

/// Decode a config-pin voltage (in millivolts) against the EZ-PD BCR datasheet
/// **Table 2** VBUS_MAX bands (Infineon 002-25383), VDDD = 3.3 V reference.
///
/// Verbatim bands (mV):
///   5 V: 0..=248, 9 V: 249..=786, 12 V: 787..=1347, 15 V: 1348..=1920,
///   19 V: 1921..=2778, 20 V: >= 2779.
///
/// VBUS_MIN uses the same band structure, so this one decoder serves both pins.
pub fn decode_table2(mv: i64) -> PdVolts {
    match mv {
        i64::MIN..=248 => PdVolts::V5,
        249..=786 => PdVolts::V9,
        787..=1347 => PdVolts::V12,
        1348..=1920 => PdVolts::V15,
        1921..=2778 => PdVolts::V19,
        _ => PdVolts::V20,
    }
}

/// Config-pin voltage of a single-pull-up / single-pull-down divider, in mV:
/// `Vpin = VDDD * Rpd / (Rpu + Rpd)`. `r_pd` is the *effective* pull-down (the
/// permanent pull-down in parallel with any switched leg). An `r_pd` of 0 (a
/// detent that grounds the pin directly) gives 0 mV; an open pull-down is modelled
/// by the caller passing only the permanent resistor.
pub fn divider_mv(r_pu: f64, r_pd: f64) -> i64 {
    if r_pu <= 0.0 && r_pd <= 0.0 {
        return 0;
    }
    let v = VDDD * r_pd / (r_pu + r_pd);
    (v * 1000.0).round() as i64
}

/// Two resistors in parallel (both finite, > 0). Either being 0 ohm shorts to 0.
fn parallel(a: f64, b: f64) -> f64 {
    if a <= 0.0 || b <= 0.0 {
        return 0.0;
    }
    (a * b) / (a + b)
}

/// Does this component's value / MPN positively identify it as a CYPD3177
/// (EZ-PD BCR)? Robust to layout-only extraction: we key on the value string
/// directly rather than depending on a model-DB entry existing.
fn is_cypd3177(c: &Component) -> bool {
    let v = c.value.to_ascii_uppercase().replace(['-', ' ', '_'], "");
    let l = c.lib_id.to_ascii_uppercase().replace(['-', ' ', '_'], "");
    v.contains("CYPD3177") || l.contains("CYPD3177") || v.contains("EZPDBCR")
}

/// A plain two-terminal, assembled resistor (ref R*, not RV/RT/RN/RP/RM), with a
/// parseable ohm value. Mirrors the strap-lint resistor test plus a value parse.
fn resistor_ohms(c: &Component) -> Option<f64> {
    if c.dnp {
        return None;
    }
    let r = c.reference.to_ascii_uppercase();
    let lib = c.lib_id.to_ascii_lowercase();
    let is_r_ref = r.starts_with('R')
        && !r.starts_with("RV")
        && !r.starts_with("RT")
        && !r.starts_with("RN")
        && !r.starts_with("RP")
        && !r.starts_with("RM");
    let connected = c.pins.iter().filter(|p| p.net.is_some()).count();
    if !is_r_ref || connected != 2 || lib.contains("ferrite") || lib.contains("inductor") {
        return None;
    }
    parse_value(&c.value)
        .map(|p| p.si)
        .filter(|o| o.is_finite() && *o >= 0.0)
}

// ── Net-name helpers (local copies; extract versions are private) ──

fn norm(name: &str) -> String {
    let n = name.trim();
    let leaf = n.rsplit('/').next().unwrap_or(n);
    leaf.trim().to_ascii_uppercase()
}

fn is_ground_name(name: &str) -> bool {
    let n = norm(name);
    matches!(
        n.as_str(),
        "GND" | "GNDA" | "GNDD" | "AGND" | "DGND" | "PGND" | "VSS" | "GNDIO" | "0"
    ) || n.starts_with("GND")
}

/// Is this the 3.3 V reference rail (VDDD)? The CYPD3177 dividers reference VDDD,
/// which the board exposes as +3.3V / +3V3 / VDDD.
fn is_vddd_name(name: &str) -> bool {
    let n = norm(name);
    matches!(
        n.as_str(),
        "+3V3" | "3V3" | "+3.3V" | "3.3V" | "VDDD" | "VCC3V3" | "VDD3V3" | "VDD3P3" | "+3V3A"
    ) || n.contains("3V3")
        || n.contains("3.3V")
        || n == "VDDD"
}

fn is_unconnected_net(name: &str) -> bool {
    name.trim_start_matches('/').starts_with("unconnected-")
}

/// The net id a U pin carries for a given pin-function role (e.g. "VBUS_MAX"),
/// or, failing a function match, by the net's own name. Returns (net_id, name).
fn pin_net_for_role<'a>(
    board: &'a ExtractedBoard,
    comp: &'a Component,
    role: &str,
) -> Option<(i64, &'a str)> {
    // Prefer the pin function (the schematic / newer .kicad_pcb populate it).
    if let Some(p) = comp
        .pins
        .iter()
        .find(|p| p.function.eq_ignore_ascii_case(role) && p.net.is_some())
    {
        if let Some(id) = p.net {
            if let Some(net) = board.net(id) {
                return Some((id, &net.name));
            }
        }
    }
    // Fallback: a net whose (leaf) name names the role, attached to this part.
    let want = role.to_ascii_uppercase();
    for p in &comp.pins {
        let Some(id) = p.net else { continue };
        let Some(net) = board.net(id) else { continue };
        let n = norm(&net.name);
        if n == want || n.contains(&want) {
            return Some((id, &net.name));
        }
    }
    None
}

/// The resolved fixed divider on a config net: the pull-up resistor to VDDD and
/// the permanent pull-down resistor to ground. Either may be absent. Switched
/// legs are handled separately.
struct FixedDivider {
    r_pu: Option<(String, f64)>, // (ref, ohms) to VDDD
    r_pd: Option<(String, f64)>, // (ref, ohms) to GND
}

/// Walk a config net's members and resolve the fixed pull-up (to VDDD) and
/// permanent pull-down (to GND) resistors. If multiple pull-downs are directly
/// present, they are combined in parallel; likewise pull-ups.
fn resolve_fixed_divider(board: &ExtractedBoard, net_id: i64) -> FixedDivider {
    let mut pu: Option<(String, f64)> = None;
    let mut pd: Option<(String, f64)> = None;
    for (c, _p) in board.net_members(net_id) {
        let Some(ohms) = resistor_ohms(c) else {
            continue;
        };
        for op in &c.pins {
            let Some(oid) = op.net else { continue };
            if oid == net_id {
                continue;
            }
            let Some(on) = board.net(oid) else { continue };
            if is_ground_name(&on.name) {
                pd = Some(match pd {
                    None => (c.reference.clone(), ohms),
                    Some((r, o)) => (format!("{r}||{}", c.reference), parallel(o, ohms)),
                });
            } else if is_vddd_name(&on.name) {
                pu = Some(match pu {
                    None => (c.reference.clone(), ohms),
                    Some((r, o)) => (format!("{r}||{}", c.reference), parallel(o, ohms)),
                });
            }
        }
    }
    FixedDivider { r_pu: pu, r_pd: pd }
}

/// One enumerated rotary detent: its switched extra pull-down (None = open leg),
/// and the resulting decode.
struct Detent {
    /// Effective extra pull-down (ohms) the selector adds in parallel with the
    /// permanent pull-down. `Some(0.0)` = grounds the pin directly; `None` = open.
    extra_pd: Option<f64>,
    label: String,
}

/// Enumerate the selectable detents of a rotary / slide selector whose *common*
/// pad sits on `net_id`. We look for a multi-pad switch-like component (>= 3
/// connected pads, ref SW*/S*, or footprint naming a switch) that touches
/// `net_id`, then for each of its *other* pads decode the leg: a pad on ground is
/// a direct-ground detent (0 ohm); a pad reaching ground through exactly one
/// resistor contributes that resistor as the extra pull-down; an unconnected /
/// dangling pad is an open detent.
fn enumerate_detents(board: &ExtractedBoard, net_id: i64) -> Vec<Detent> {
    let mut detents = Vec::new();
    // Find a switch component with a pad on this net.
    for (sw, _p) in board.net_members(net_id) {
        if !is_selector_switch(sw) {
            continue;
        }
        for pad in &sw.pins {
            let Some(pid) = pad.net else {
                // a dangling NC pad on the selector is an open detent
                detents.push(Detent {
                    extra_pd: None,
                    label: format!("{}.{}", sw.reference, pad.number),
                });
                continue;
            };
            if pid == net_id {
                continue; // the common pad
            }
            let Some(pn) = board.net(pid) else { continue };
            if is_unconnected_net(&pn.name) {
                detents.push(Detent {
                    extra_pd: None,
                    label: format!("{}.{} (open)", sw.reference, pad.number),
                });
                continue;
            }
            if is_ground_name(&pn.name) {
                detents.push(Detent {
                    extra_pd: Some(0.0),
                    label: format!("{}.{} (GND)", sw.reference, pad.number),
                });
                continue;
            }
            // A leg node: look for a single resistor from this node to ground.
            if let Some((rref, ohms)) = leg_resistor_to_ground(board, pid) {
                detents.push(Detent {
                    extra_pd: Some(ohms),
                    label: format!("{}.{} ({rref})", sw.reference, pad.number),
                });
            }
            // else: leg node we cannot resolve to a clean pull-down -> skip it
            // (zero-false-positive: we do not invent a value).
        }
        // Only the first selector on the net is enumerated.
        break;
    }
    detents
}

/// A slide / rotary / DIP selector: a switch-class component (ref SW*/S*, or a
/// footprint/lib naming a switch) with >= 3 connected pads (a 2-pad part is a
/// plain on/off button, not a multi-position selector).
fn is_selector_switch(c: &Component) -> bool {
    let r = c.reference.to_ascii_uppercase();
    let lib = c.lib_id.to_ascii_lowercase();
    let fp = c.footprint.to_ascii_lowercase();
    let connected = c.pins.iter().filter(|p| p.net.is_some()).count();
    if connected < 3 {
        return false;
    }
    r.starts_with("SW")
        || (r.starts_with('S') && r.len() <= 3 && r.chars().skip(1).all(|c| c.is_ascii_digit()))
        || lib.contains("switch")
        || lib.contains("rotary")
        || fp.contains("switch")
        || fp.contains("rotary")
}

/// A single assembled resistor with one pad on `node` and the other on ground.
fn leg_resistor_to_ground(board: &ExtractedBoard, node: i64) -> Option<(String, f64)> {
    for (c, _p) in board.net_members(node) {
        let Some(ohms) = resistor_ohms(c) else {
            continue;
        };
        for op in &c.pins {
            let Some(oid) = op.net else { continue };
            if oid == node {
                continue;
            }
            if let Some(on) = board.net(oid) {
                if is_ground_name(&on.name) {
                    return Some((c.reference.clone(), ohms));
                }
            }
        }
    }
    None
}

/// Run the device-decode check class over a board. Currently seeded with the
/// CYPD3177; each `is_cypd3177` part is decoded independently.
pub fn device_decode_lint(board: &ExtractedBoard, _lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    for comp in &board.components {
        if is_cypd3177(comp) {
            check_cypd3177(board, comp, &mut report);
        }
    }
    report
}

/// Decode a CYPD3177's VBUS_MAX (with its rotary selector, if resolvable) and
/// VBUS_MIN dividers, and flag the documented failure modes.
fn check_cypd3177(board: &ExtractedBoard, u: &Component, report: &mut NetLintReport) {
    // ── VBUS_MAX ────────────────────────────────────────────────────────────
    let max = pin_net_for_role(board, u, "VBUS_MAX");
    let min = pin_net_for_role(board, u, "VBUS_MIN");

    // Resolve VBUS_MAX's reachable bands (permanent divider + each detent).
    let mut max_band: Option<PdVolts> = None; // the TOP reachable band (open detent)
    let mut reachable: Vec<(PdVolts, String)> = Vec::new();
    if let Some((max_id, max_name)) = max {
        let fd = resolve_fixed_divider(board, max_id);
        // Need at least a pull-up and the permanent pull-down to compute anything.
        if let (Some((_pur, rpu)), Some((_pdr, rpd))) = (&fd.r_pu, &fd.r_pd) {
            let detents = enumerate_detents(board, max_id);
            if detents.is_empty() {
                // No selector resolvable: decode the static divider alone.
                let mv = divider_mv(*rpu, *rpd);
                let b = decode_table2(mv);
                max_band = Some(b);
                reachable.push((b, format!("static {mv} mV")));
            } else {
                for d in &detents {
                    let eff_pd = match d.extra_pd {
                        Some(extra) => parallel(*rpd, extra), // permanent || switched leg
                        None => *rpd,                         // open leg: permanent only
                    };
                    let mv = divider_mv(*rpu, eff_pd);
                    let b = decode_table2(mv);
                    reachable.push((b, format!("{} -> {mv} mV", d.label)));
                }
                // The "open" detent (only the permanent divider) is the top of
                // the dial; use it as the nominal max for the Note-1 comparison.
                let open_mv = divider_mv(*rpu, *rpd);
                max_band = Some(decode_table2(open_mv));

                // Flag a SELECTOR that cannot reach the part's top bands: the
                // headline capability is unreachable. This only applies to a real
                // multi-detent selector, a plain fixed divider decodes to exactly
                // ONE band by construction, so it can NEVER reach both 15 V and
                // 20 V, and firing the "selector cannot reach" finding on it was
                // a false positive on every correct fixed-voltage sink.
                let reaches_20 = reachable.iter().any(|(b, _)| *b == PdVolts::V20);
                let reaches_15 = reachable.iter().any(|(b, _)| *b == PdVolts::V15);
                if !reachable.is_empty() && (!reaches_20 || !reaches_15) {
                    let bands: Vec<String> = reachable
                        .iter()
                        .map(|(b, why)| format!("{}V [{why}]", b.volts()))
                        .collect();
                    let mut missing = Vec::new();
                    if !reaches_15 {
                        missing.push("15V");
                    }
                    if !reaches_20 {
                        missing.push("20V");
                    }
                    report.findings.push(LintFinding {
                        check: LintCheck::DeviceDecode,
                        severity: Severity::Medium,
                        message: format!(
                            "{} VBUS_MAX selector on net '{max_name}' decodes (Table 2) to {{{}}}, but cannot reach {{{}}}: the divider's permanent pull-down plus switched-parallel legs cannot reproduce the datasheet's per-detent codes, so the top setting(s) are unreachable",
                            u.reference,
                            bands.join(", "),
                            missing.join(", "),
                        ),
                        refs: vec![u.reference.clone()],
                        nets: vec![max_name.to_string()],
                    });
                }
            }
        }
    }

    // ── VBUS_MIN and Note-1 consistency ─────────────────────────────────────
    if let Some((min_id, min_name)) = min {
        let fd = resolve_fixed_divider(board, min_id);
        if let (Some((_pur, rpu)), Some((_pdr, rpd))) = (&fd.r_pu, &fd.r_pd) {
            let mv = divider_mv(*rpu, *rpd);
            let min_band = decode_table2(mv);
            // Note 1: if VBUS_MIN > VBUS_MAX, VBUS_MAX is used as both min and
            // max, so for every reachable detent whose VBUS_MAX band is BELOW the
            // hard-wired VBUS_MIN, the selector is silently defeated (the part
            // requests VBUS_MAX, not the labelled higher voltage). We fire when at
            // least one reachable detent is clamped, comparing against the lowest
            // reachable VBUS_MAX band so we catch the defeat even when the top
            // (open) detent happens to equal VBUS_MIN.
            let clamped = reachable
                .iter()
                .filter(|(b, _)| min_band.volts() > b.volts())
                .count();
            if max_band.is_some() && clamped > 0 {
                let top = max_band.map(|b| b.volts()).unwrap_or(0);
                report.findings.push(LintFinding {
                    check: LintCheck::DeviceDecode,
                    severity: Severity::High,
                    message: format!(
                        "{} VBUS_MIN net '{min_name}' decodes (Table 2) to {}V ({mv} mV), GREATER than {clamped} of the VBUS_MAX selector's reachable detents (top band {top}V): per EZ-PD BCR datasheet Note 1 (VBUS_MIN > VBUS_MAX), VBUS_MAX is used as both minimum and maximum, so the hard-wired VBUS_MIN silently overrides and defeats those selector positions",
                        u.reference,
                        min_band.volts(),
                    ),
                    refs: vec![u.reference.clone()],
                    nets: vec![min_name.to_string()],
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 1. Pure Table-2 decode against the band edges ───────────────────────
    #[test]
    fn table2_band_edges_decode() {
        let cases: [(i64, PdVolts); 11] = [
            (0, PdVolts::V5),
            (248, PdVolts::V5),
            (249, PdVolts::V9),
            (786, PdVolts::V9),
            (787, PdVolts::V12),
            (1347, PdVolts::V12),
            (1348, PdVolts::V15),
            (1920, PdVolts::V15),
            (1921, PdVolts::V19),
            (2778, PdVolts::V19),
            (2779, PdVolts::V20),
        ];
        for (mv, want) in cases {
            assert_eq!(decode_table2(mv), want, "decode {mv} mV");
        }
    }

    // ── 2. The five hand-derived detent voltages decode as the hunt states ──
    //    proving detent 4 ("15 V") mis-decodes to 12 V and detent 5 ("20 V")
    //    mis-decodes to 19 V.
    #[test]
    fn hunt_detent_voltages_decode() {
        let cases: [(i64, PdVolts, &str); 5] = [
            (0, PdVolts::V5, "detent1 5V"),
            (499, PdVolts::V9, "detent2 9V"),
            (908, PdVolts::V12, "detent3 12V"),
            (1315, PdVolts::V12, "detent4 labelled 15V -> 12V WRONG"),
            (2185, PdVolts::V19, "detent5 labelled 20V -> 19V WRONG"),
        ];
        for (mv, want, why) in cases {
            assert_eq!(decode_table2(mv), want, "{why}");
        }
    }

    // ── 3. Divider-voltage computation matches the hunt's hand math (~2 mV) ──
    #[test]
    fn divider_voltage_matches_hand_math() {
        let rpu = 5100.0;
        let r12 = 10_000.0;
        // detent5 open: permanent R12 alone -> 2185 mV
        assert!((divider_mv(rpu, r12) - 2185).abs() <= 2, "open leg");
        // detent4: R12 || 5.1k = 3400 -> 1315 mV
        assert!(
            (divider_mv(rpu, parallel(r12, 5100.0)) - 1315).abs() <= 2,
            "5.1k leg"
        );
        // detent3: R12 || 2.4k -> 908 mV
        assert!(
            (divider_mv(rpu, parallel(r12, 2400.0)) - 908).abs() <= 2,
            "2.4k leg"
        );
        // detent2: R12 || 1k -> 499 mV
        assert!(
            (divider_mv(rpu, parallel(r12, 1000.0)) - 499).abs() <= 2,
            "1k leg"
        );
        // detent1: direct GND (0 ohm) -> 0 mV
        assert_eq!(divider_mv(rpu, parallel(r12, 0.0)), 0, "GND-direct leg");
    }

    // ── 4a. Synthetic board: VBUS_MIN hard-wired to 19V band while VBUS_MAX
    //        tops out below it -> the Note-1 override finding must fire. ──────
    fn cypd_board(min_pd: &str, max_extra_leg_pd: &str) -> String {
        // U1 CYPD3177: pad 1 = VBUS_MIN, pad 2 = VBUS_MAX.
        // VBUS_MAX net 21: R11 5.1k pull-up to +3.3V (net 5), R12 10k pull-down to
        //   GND (net 1), plus a slide switch SW1 whose common (pad 6) is on net 21
        //   and whose pad 4 reaches GND via R15 (`max_extra_leg_pd`), pad 5 open.
        // VBUS_MIN net 20: R9 5.1k pull-up to +3.3V, R10 `min_pd` pull-down to GND.
        format!(
            r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 5 "+3.3V")
  (net 20 "Net-(U1-VBUS_MIN)")
  (net 21 "VBUS_max")
  (net 24 "Net-(R15-Pad1)")
  (net 31 "unconnected-(SW1-Pad5)")
  (module Package_DFN_QFN:QFN24 (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value CYPD3177-24LQ (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 20 "Net-(U1-VBUS_MIN)") (pinfunction "VBUS_MIN"))
    (pad 2 smd rect (at 0 1) (net 21 "VBUS_max") (pinfunction "VBUS_MAX"))
  )
  (module R (layer F.Cu) (at 110 100)
    (fp_text reference R9 (at 0 0) (layer F.SilkS))
    (fp_text value 5k1 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 5 "+3.3V"))
    (pad 2 smd rect (at 2 0) (net 20 "Net-(U1-VBUS_MIN)"))
  )
  (module R (layer F.Cu) (at 110 102)
    (fp_text reference R10 (at 0 0) (layer F.SilkS))
    (fp_text value {min_pd} (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 20 "Net-(U1-VBUS_MIN)"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )
  (module R (layer F.Cu) (at 110 104)
    (fp_text reference R11 (at 0 0) (layer F.SilkS))
    (fp_text value 5k1 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 5 "+3.3V"))
    (pad 2 smd rect (at 2 0) (net 21 "VBUS_max"))
  )
  (module R (layer F.Cu) (at 110 106)
    (fp_text reference R12 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 21 "VBUS_max"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )
  (module R (layer F.Cu) (at 110 108)
    (fp_text reference R15 (at 0 0) (layer F.SilkS))
    (fp_text value {max_extra_leg_pd} (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 24 "Net-(R15-Pad1)"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )
  (module SW (layer F.Cu) (at 120 100)
    (fp_text reference SW1 (at 0 0) (layer F.SilkS))
    (fp_text value SS-10-15SPE (at 0 2) (layer F.Fab))
    (pad 4 smd rect (at 0 0) (net 24 "Net-(R15-Pad1)"))
    (pad 5 smd rect (at 1 0) (net 31 "unconnected-(SW1-Pad5)"))
    (pad 6 smd rect (at 2 0) (net 21 "VBUS_max"))
  )
)"#
        )
    }

    fn decode_findings(text: &str) -> NetLintReport {
        let board = ExtractedBoard::from_kicad_pcb(text).expect("parse synthetic board");
        let lib = ModelLibrary::builtin();
        device_decode_lint(&board, &lib)
    }

    #[test]
    fn device_decode_findings_flow_through_engine_lint() {
        // Chokepoint fix: device_decode must run INSIDE engine_lint so
        // --check/--json/TUI/frontdoor surface these faults, not only --lint.
        let text = cypd_board("10k", "5k1");
        let board = ExtractedBoard::from_kicad_pcb(&text).expect("parse synthetic board");
        let lib = ModelLibrary::builtin();
        assert!(
            device_decode_lint(&board, &lib)
                .of_check(LintCheck::DeviceDecode)
                .next()
                .is_some(),
            "fixture should produce a device_decode finding"
        );
        let full = crate::checks::engine_lint(&board, &lib);
        assert!(
            full.of_check(LintCheck::DeviceDecode).next().is_some(),
            "engine_lint must include device_decode findings via the chokepoint"
        );
    }

    #[test]
    fn note1_override_fires_when_min_exceeds_max() {
        // VBUS_MIN = 10k pull-down -> 2185 mV -> 19V band.
        // VBUS_MAX: permanent 10k, switched leg R15 = 5.1k. Open detent -> 2185 mV
        //   (19V) too -- but with R12 permanent the dial tops at the 19V band, and
        //   its lower detents (5.1k leg -> 1315 mV = 12V) sit below VBUS_MIN. The
        //   top reachable band equals 19V, and the 15V/20V finding plus the Note-1
        //   finding both apply. We assert the Note-1 (High) finding fires.
        let r = decode_findings(&cypd_board("10k", "5k1"));
        let note1: Vec<_> = r
            .of_check(LintCheck::DeviceDecode)
            .filter(|f| matches!(f.severity, Severity::High))
            .collect();
        assert_eq!(note1.len(), 1, "exactly one Note-1 override finding");
        assert!(note1[0].message.contains("Note 1"));
        assert!(note1[0].message.contains("VBUS_MIN"));
        assert!(note1[0].nets.iter().any(|n| n.contains("VBUS_MIN")));
    }

    #[test]
    fn unreachable_top_band_fires_medium() {
        // The selector cannot reach 20V (no detent grounds the pull-up / opens the
        // pull-down to give >= 2779 mV), so the Medium "unreachable top band"
        // finding fires regardless of the Note-1 case.
        let r = decode_findings(&cypd_board("10k", "5k1"));
        let med: Vec<_> = r
            .of_check(LintCheck::DeviceDecode)
            .filter(|f| matches!(f.severity, Severity::Medium))
            .collect();
        assert_eq!(med.len(), 1, "exactly one unreachable-top-band finding");
        assert!(med[0].message.contains("VBUS_MAX"));
        assert!(med[0].message.contains("20V"));
    }

    #[test]
    fn fixed_voltage_sink_without_a_selector_is_silent() {
        // R13: a CYPD3177 whose VBUS_MAX is a plain fixed pull-up + pull-down
        // with NO selector switch decodes to exactly ONE band and can never
        // reach both 15 V and 20 V by construction. The "selector cannot reach"
        // reachability gate must NOT fire on it; it applies only to a real
        // multi-detent selector. (This is the primary/correct EZ-PD BCR use.)
        let text = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 5 "+3.3V")
  (net 21 "VBUS_max")
  (module Package_DFN_QFN:QFN24 (layer F.Cu) (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value CYPD3177-24LQ (at 0 2) (layer F.Fab))
    (pad 2 smd rect (at 0 1) (net 21 "VBUS_max") (pinfunction "VBUS_MAX"))
  )
  (module R (layer F.Cu) (at 110 104)
    (fp_text reference R11 (at 0 0) (layer F.SilkS))
    (fp_text value 5k1 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 5 "+3.3V"))
    (pad 2 smd rect (at 2 0) (net 21 "VBUS_max"))
  )
  (module R (layer F.Cu) (at 110 106)
    (fp_text reference R12 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 21 "VBUS_max"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )
)"#;
        assert_eq!(
            decode_findings(text).of_check(LintCheck::DeviceDecode).count(),
            0,
            "a fixed-voltage (no-selector) CYPD3177 must not trip the selector-reachability finding"
        );
    }

    // ── 4b. Clean config: VBUS_MIN low (5V band) and VBUS_MAX able to reach
    //        both 15V and 20V -> the check is SILENT (zero false positive). ───
    #[test]
    fn clean_config_is_silent() {
        // VBUS_MIN pull-down 100k -> 3.3*100/105.1 = 3140 mV -> 20V band? No: we
        // want VBUS_MIN LOW. Use a large pull-UP-dominant divider instead: make
        // VBUS_MIN decode to the 5V band by a tiny pull-down (0R -> 0 mV).
        // And make VBUS_MAX reach 15V and 20V: a switch leg that opens the
        // pull-down (giving a high node) plus a direct-ground detent. We model a
        // single-pull-down-per-detent correct design: NO permanent R12 (so the
        // open detent gives the full 3.3V = 20V), and a leg that yields the 15V
        // band.
        let text = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 5 "+3.3V")
  (net 20 "Net-(U1-VBUS_MIN)")
  (net 21 "VBUS_max")
  (net 24 "Net-(R15-Pad1)")
  (net 25 "Net-(R16-Pad1)")
  (module Package_DFN_QFN:QFN24 (layer F.Cu) (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value CYPD3177-24LQ (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 20 "Net-(U1-VBUS_MIN)") (pinfunction "VBUS_MIN"))
    (pad 2 smd rect (at 0 1) (net 21 "VBUS_max") (pinfunction "VBUS_MAX"))
  )
  (module R (layer F.Cu) (at 110 100)
    (fp_text reference R9 (at 0 0) (layer F.SilkS))
    (fp_text value 100k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 5 "+3.3V"))
    (pad 2 smd rect (at 2 0) (net 20 "Net-(U1-VBUS_MIN)"))
  )
  (module R (layer F.Cu) (at 110 102)
    (fp_text reference R10 (at 0 0) (layer F.SilkS))
    (fp_text value 1k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 20 "Net-(U1-VBUS_MIN)"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )
  (module R (layer F.Cu) (at 110 104)
    (fp_text reference R11 (at 0 0) (layer F.SilkS))
    (fp_text value 5k1 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 5 "+3.3V"))
    (pad 2 smd rect (at 2 0) (net 21 "VBUS_max"))
  )
  (module R (layer F.Cu) (at 110 106)
    (fp_text reference R15 (at 0 0) (layer F.SilkS))
    (fp_text value 5k1 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 24 "Net-(R15-Pad1)"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )
  (module R (layer F.Cu) (at 110 110)
    (fp_text reference R16 (at 0 0) (layer F.SilkS))
    (fp_text value 1k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 25 "Net-(R16-Pad1)"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )
  (module SW (layer F.Cu) (at 120 100)
    (fp_text reference SW1 (at 0 0) (layer F.SilkS))
    (fp_text value ROTARY (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 1 "GND"))
    (pad 3 smd rect (at 1 0) (net 25 "Net-(R16-Pad1)"))
    (pad 4 smd rect (at 2 0) (net 24 "Net-(R15-Pad1)"))
    (pad 6 smd rect (at 3 0) (net 21 "VBUS_max"))
  )
)"#;
        // Here VBUS_MAX has NO permanent pull-down (no R12). resolve_fixed_divider
        // finds a pull-up (R11) but NO pull-down -> the check cannot compute the
        // static divider, so per zero-false-positive discipline it stays silent on
        // VBUS_MAX. VBUS_MIN resolves (100k/1k -> ~33 mV = 5V band) but with no
        // VBUS_MAX band to compare, Note-1 cannot fire. Net result: silent.
        let r = decode_findings(text);
        assert_eq!(
            r.of_check(LintCheck::DeviceDecode).count(),
            0,
            "a config the check cannot fault (or cannot fully resolve) stays silent"
        );
    }

    // ── Zero-false-positive: a non-CYPD board is never touched. ──────────────
    #[test]
    fn non_cypd_part_is_ignored() {
        let text = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 5 "+3.3V")
  (net 21 "VBUS_max")
  (module QFN (layer F.Cu) (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value STM32F103 (at 0 2) (layer F.Fab))
    (pad 2 smd rect (at 0 1) (net 21 "VBUS_max") (pinfunction "VBUS_MAX"))
  )
)"#;
        assert_eq!(
            decode_findings(text)
                .of_check(LintCheck::DeviceDecode)
                .count(),
            0
        );
    }

    // ── DNP-awareness (the real pd-sink board's actual state) ────────────────
    // On the INGBZGMBH board the permanent VBUS_MAX pull-down (R12) and the
    // VBUS_MIN pull-up (R9) are marked Do-Not-Populate in BOTH the schematic and
    // the .kicad_pcb (`(attr smd exclude_from_bom dnp)`). With R12 unpopulated the
    // VBUS_MAX divider has no static pull-down, so it cannot be computed as a
    // fixed divider and the check stays SILENT. This is correct: a check that
    // ignored DNP and fired here would be a false positive. (See the report: this
    // also means the hunt doc's "permanent R12 always present" premise does not
    // match the as-built files.)
    #[test]
    fn dnp_permanent_pulldown_makes_check_silent() {
        // Same as the faulting board, but R12 (VBUS_MAX permanent pull-down) and
        // R9 (VBUS_MIN pull-up) carry the KiCad DNP attribute.
        let text = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 5 "+3.3V")
  (net 20 "Net-(U1-VBUS_MIN)")
  (net 21 "VBUS_max")
  (net 24 "Net-(R15-Pad1)")
  (net 31 "unconnected-(SW1-Pad5)")
  (module Package_DFN_QFN:QFN24 (layer F.Cu) (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value CYPD3177-24LQ (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 20 "Net-(U1-VBUS_MIN)") (pinfunction "VBUS_MIN"))
    (pad 2 smd rect (at 0 1) (net 21 "VBUS_max") (pinfunction "VBUS_MAX"))
  )
  (module R (layer F.Cu) (at 110 100)
    (fp_text reference R9 (at 0 0) (layer F.SilkS))
    (fp_text value 5k1 (at 0 2) (layer F.Fab))
    (attr smd exclude_from_bom dnp)
    (pad 1 smd rect (at 0 0) (net 5 "+3.3V"))
    (pad 2 smd rect (at 2 0) (net 20 "Net-(U1-VBUS_MIN)"))
  )
  (module R (layer F.Cu) (at 110 102)
    (fp_text reference R10 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 20 "Net-(U1-VBUS_MIN)"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )
  (module R (layer F.Cu) (at 110 104)
    (fp_text reference R11 (at 0 0) (layer F.SilkS))
    (fp_text value 5k1 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 5 "+3.3V"))
    (pad 2 smd rect (at 2 0) (net 21 "VBUS_max"))
  )
  (module R (layer F.Cu) (at 110 106)
    (fp_text reference R12 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (attr smd exclude_from_bom dnp)
    (pad 1 smd rect (at 0 0) (net 21 "VBUS_max"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )
  (module R (layer F.Cu) (at 110 108)
    (fp_text reference R15 (at 0 0) (layer F.SilkS))
    (fp_text value 5k1 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 24 "Net-(R15-Pad1)"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )
  (module SW (layer F.Cu) (at 120 100)
    (fp_text reference SW1 (at 0 0) (layer F.SilkS))
    (fp_text value SS-10-15SPE (at 0 2) (layer F.Fab))
    (pad 4 smd rect (at 0 0) (net 24 "Net-(R15-Pad1)"))
    (pad 5 smd rect (at 1 0) (net 31 "unconnected-(SW1-Pad5)"))
    (pad 6 smd rect (at 2 0) (net 21 "VBUS_max"))
  )
)"#;
        assert_eq!(
            decode_findings(text).of_check(LintCheck::DeviceDecode).count(),
            0,
            "with the permanent pull-down DNP the static divider is uncomputable; the check must stay silent"
        );
    }
}
