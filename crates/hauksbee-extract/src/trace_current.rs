//! Trace current-capacity check (IPC-2221 ampacity from layout geometry).
//!
//! The geometric DRC already reduces every copper track to a finite-width
//! segment on a known net. This module reuses that same parse to ask a different
//! question: **can the copper actually carry the current the design pushes
//! through it?** For each net it finds the *narrowest* routed track segment
//! (the bottleneck of a series conductor) and converts that width into a
//! current rating with the IPC-2221 external-layer formula, so a net carrying a
//! cited load current beyond its rating can be flagged.
//!
//! ## The honest reach of this check (read before trusting a result)
//!
//! 1. **Pour fidelity.** Hauksbee models copper as the discrete `(segment ...)`
//!    primitives KiCad writes. It does **not** rasterise filled `(zone ...)`
//!    pours. On real boards the high-current rails (motor supply, ground, bulk
//!    5 V) are almost always distributed as *copper pours*, and the only
//!    discrete segments left on those nets are the thin pad-entry / thermal-spoke
//!    stubs into the pour. Measuring "minimum segment width" on a poured net
//!    therefore reads a 0.25 mm stub and would scream "undersized" when the
//!    actual conductor is a centimetres-wide plane. That is a guaranteed false
//!    positive. So **any net that carries a copper zone is reported as
//!    `Poured` and never flagged** - its true cross-section is out of hauksbee's
//!    reach, and saying so is the honest answer, not a pass.
//!
//! 2. **Current attribution.** The netlist does not encode how much current a
//!    net carries. This module never invents a current: the caller supplies the
//!    attributed load current with a citation (a part's datasheet rating, a
//!    connector's spec). The geometry+IPC engine here is pure physics; the
//!    judgement of "this net carries N amps" stays with the caller and its
//!    citation, exactly as the regulator-derating analysis does.
//!
//! 3. **Width, not length.** IPC-2221 ampacity depends on width and copper
//!    weight, not length (length affects voltage drop, a separate concern). The
//!    bottleneck of a series trace is its narrowest segment, which is what this
//!    reports.
//!
//! The result is a check that *discriminates*: it fires on a genuinely
//! under-width discrete trace carrying a cited current, and it stays silent
//! (with an explicit `Poured` / `NoCurrentCited` reason) everywhere it cannot
//! see the real conductor. A check that cannot tell a thin pour-stub from a thin
//! signal trace would be worse than no check; this one refuses to guess.

use std::collections::{HashMap, HashSet};

use forge_sexpr::List;

/// Copper weight in ounces per square foot. 1 oz = 35 um = 1.378 mil thick.
pub const OZ_1: f64 = 1.0;
/// IPC-2221 external-layer constant `k` (internal layers use 0.024).
const K_EXTERNAL: f64 = 0.048;
/// IPC-2221 internal-layer constant `k`.
const K_INTERNAL: f64 = 0.024;
/// 1 oz copper finished thickness in mils.
const MIL_PER_OZ: f64 = 1.378;
/// mm per mil.
const MM_PER_MIL: f64 = 0.0254;

/// IPC-2221 current capacity (amps) of a trace of the given finished width
/// (mm) and copper weight (oz) for a target temperature rise `dt` (degrees C).
///
/// `I = k * dT^0.44 * A^0.725`, with A the cross-sectional area in mils^2 and
/// `k = 0.048` for external (outer) copper layers, `0.024` for internal. This
/// is the long-standard ampacity curve; it is a conservative ballpark, not a
/// thermal simulation, and is documented as such.
pub fn ipc2221_ampacity(width_mm: f64, oz: f64, dt_c: f64, external: bool) -> f64 {
    if width_mm <= 0.0 || oz <= 0.0 || dt_c <= 0.0 {
        return 0.0;
    }
    let thick_mil = MIL_PER_OZ * oz;
    let width_mil = width_mm / MM_PER_MIL;
    let area_mil2 = width_mil * thick_mil;
    let k = if external { K_EXTERNAL } else { K_INTERNAL };
    k * dt_c.powf(0.44) * area_mil2.powf(0.725)
}

/// Inverse of [`ipc2221_ampacity`]: the minimum finished width (mm) needed to
/// carry `amps` at temperature rise `dt_c`. Useful for "you need at least X mm".
pub fn ipc2221_min_width_mm(amps: f64, oz: f64, dt_c: f64, external: bool) -> f64 {
    if amps <= 0.0 || oz <= 0.0 || dt_c <= 0.0 {
        return 0.0;
    }
    let k = if external { K_EXTERNAL } else { K_INTERNAL };
    // area_mil2 = (I / (k * dT^0.44))^(1/0.725)
    let area_mil2 = (amps / (k * dt_c.powf(0.44))).powf(1.0 / 0.725);
    let thick_mil = MIL_PER_OZ * oz;
    let width_mil = area_mil2 / thick_mil;
    width_mil * MM_PER_MIL
}

/// Whether a net's copper was routed as discrete traces, or as a pour, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopperKind {
    /// Only discrete `(segment)`/`(arc)` tracks: the min width is meaningful.
    Traces,
    /// The net carries at least one filled `(zone)` pour: the min discrete
    /// segment width is NOT the real conductor cross-section. Out of reach.
    Poured,
    /// No copper at all (a schematic-only net, or an unrouted net).
    None,
}

/// Per-net copper geometry summary extracted from a `.kicad_pcb`.
#[derive(Debug, Clone)]
pub struct NetCopper {
    pub net_id: i64,
    pub name: String,
    pub kind: CopperKind,
    /// Narrowest discrete track segment on the net (mm), if any tracks exist.
    pub min_trace_width_mm: Option<f64>,
    /// Widest discrete track segment on the net (mm).
    pub max_trace_width_mm: Option<f64>,
    /// Number of discrete track segments on the net.
    pub segment_count: usize,
    /// Number of filled zones on the net.
    pub zone_count: usize,
}

impl NetCopper {
    /// IPC-2221 ampacity of the *bottleneck* (narrowest) discrete trace, at the
    /// given temperature rise / copper weight. `None` when the net has no
    /// discrete tracks (e.g. pure pour or no copper). Reported only for
    /// `Traces` nets in the flagging path, but exposed for any net for probing.
    pub fn bottleneck_ampacity(&self, oz: f64, dt_c: f64, external: bool) -> Option<f64> {
        self.min_trace_width_mm.map(|w| ipc2221_ampacity(w, oz, dt_c, external))
    }
}

/// Parse `.kicad_pcb` text into per-net copper geometry. Returns an empty vector
/// when the text is not a KiCad layout or does not parse, so callers outside the
/// extract crate (which has the s-expr parser) can run the ampacity check from
/// raw text without depending on the parser themselves.
pub fn net_copper_from_text(text: &str) -> Vec<NetCopper> {
    if !text.contains("(kicad_pcb") {
        return Vec::new();
    }
    let Ok(doc) = forge_sexpr::parse(text) else {
        return Vec::new();
    };
    match doc.root() {
        Some(root) => net_copper_from_root(root),
        None => Vec::new(),
    }
}

/// Parse the per-net copper geometry (track widths + zone presence) from a
/// KiCad `.kicad_pcb` document root. Mirrors `drc::collect_primitives` but keeps
/// only what an ampacity question needs.
pub fn net_copper_from_root(root: &List) -> Vec<NetCopper> {
    // net id -> name
    let mut names: HashMap<i64, String> = HashMap::new();
    for n in root.find_all("net") {
        if let (Some(id), Some(name)) = (n.arg_i64(0), n.arg_value(1)) {
            names.entry(id).or_insert(name);
        }
    }

    let mut min_w: HashMap<i64, f64> = HashMap::new();
    let mut max_w: HashMap<i64, f64> = HashMap::new();
    let mut seg_count: HashMap<i64, usize> = HashMap::new();
    let mut zone_count: HashMap<i64, usize> = HashMap::new();
    let mut seen: HashSet<i64> = HashSet::new();

    // Track segments (straight).
    for seg in root.find_all("segment") {
        let layer = seg.find_value("layer").unwrap_or_default();
        if !layer.ends_with(".Cu") {
            continue;
        }
        let Some(id) = net_id_of(seg) else { continue };
        let w = seg.find_f64("width").unwrap_or(0.0);
        if w <= 0.0 {
            continue;
        }
        accumulate(&mut min_w, &mut max_w, &mut seg_count, &mut seen, id, w);
    }
    // Arc tracks carry a width too.
    for arc in root.find_all("arc") {
        let layer = arc.find_value("layer").unwrap_or_default();
        if !layer.ends_with(".Cu") {
            continue;
        }
        let Some(id) = net_id_of(arc) else { continue };
        let w = arc.find_f64("width").unwrap_or(0.0);
        if w <= 0.0 {
            continue;
        }
        accumulate(&mut min_w, &mut max_w, &mut seg_count, &mut seen, id, w);
    }
    // Zones: a net that carries a filled pour has its real cross-section in the
    // pour, not the segments. Record the zone count so the net is marked Poured.
    for zone in root.find_all("zone") {
        let Some(id) = net_id_of(zone) else { continue };
        // Only copper-layer zones count (keepouts on edge cuts do not conduct).
        let on_copper = zone
            .find("layers")
            .map(|l| (0..).map_while(|i| l.arg_value(i)).any(|n| n.ends_with(".Cu")))
            .unwrap_or(false)
            || zone.find_value("layer").map(|l| l.ends_with(".Cu")).unwrap_or(false);
        if !on_copper {
            continue;
        }
        *zone_count.entry(id).or_default() += 1;
        seen.insert(id);
    }

    let mut out = Vec::new();
    for id in seen {
        let zc = zone_count.get(&id).copied().unwrap_or(0);
        let sc = seg_count.get(&id).copied().unwrap_or(0);
        let kind = if zc > 0 {
            CopperKind::Poured
        } else if sc > 0 {
            CopperKind::Traces
        } else {
            CopperKind::None
        };
        out.push(NetCopper {
            net_id: id,
            name: names.get(&id).cloned().unwrap_or_default(),
            kind,
            min_trace_width_mm: min_w.get(&id).copied(),
            max_trace_width_mm: max_w.get(&id).copied(),
            segment_count: sc,
            zone_count: zc,
        });
    }
    out.sort_by_key(|n| n.net_id);
    out
}

fn accumulate(
    min_w: &mut HashMap<i64, f64>,
    max_w: &mut HashMap<i64, f64>,
    seg_count: &mut HashMap<i64, usize>,
    seen: &mut HashSet<i64>,
    id: i64,
    w: f64,
) {
    seen.insert(id);
    *seg_count.entry(id).or_default() += 1;
    min_w.entry(id).and_modify(|m| *m = m.min(w)).or_insert(w);
    max_w.entry(id).and_modify(|m| *m = m.max(w)).or_insert(w);
}

/// Resolve a `(net N "name")` or `(net N)` child of a primitive to its id.
fn net_id_of(list: &List) -> Option<i64> {
    let net = list.find("net")?;
    net.arg_i64(0)
}

/// A trace-current finding: a discrete-trace net whose narrowest segment cannot
/// carry the cited load current.
#[derive(Debug, Clone)]
pub struct TraceCurrentFinding {
    pub net_id: i64,
    pub net: String,
    /// The narrowest segment width found (mm) - the bottleneck.
    pub min_width_mm: f64,
    /// IPC-2221 ampacity of that width at the audit's temperature rise.
    pub ampacity_a: f64,
    /// The cited load current the caller attributed to the net (A).
    pub cited_current_a: f64,
    /// The minimum width that WOULD carry the cited current (mm), for the fix.
    pub required_width_mm: f64,
    /// Free-text citation for the attributed current (datasheet / connector).
    pub citation: String,
}

/// Audit parameters (copper weight, target rise, layer side). Defaults are the
/// defensible external-1oz-10C ballpark; callers can tighten them.
#[derive(Debug, Clone, Copy)]
pub struct TraceAudit {
    pub oz: f64,
    pub dt_c: f64,
    pub external: bool,
    /// A net must miss its rating by more than this factor before it is flagged,
    /// to keep rounding-level near-misses out (1.0 = flag any shortfall).
    pub margin: f64,
}

impl Default for TraceAudit {
    fn default() -> Self {
        TraceAudit { oz: 1.0, dt_c: 10.0, external: true, margin: 1.0 }
    }
}

/// Given the per-net copper geometry and a map of `net name -> (cited current,
/// citation)`, flag every **trace-routed** net whose bottleneck cannot carry the
/// cited current. Poured nets are skipped (their true cross-section is out of
/// reach); nets with no cited current are skipped (no fabricated load).
pub fn audit_trace_currents(
    copper: &[NetCopper],
    cited: &HashMap<String, (f64, String)>,
    audit: &TraceAudit,
) -> Vec<TraceCurrentFinding> {
    let mut findings = Vec::new();
    for nc in copper {
        let Some((current, citation)) = cited.get(&nc.name) else {
            continue;
        };
        // Only discrete-trace nets are in reach. Poured nets are honestly
        // out of scope (see module docs).
        if nc.kind != CopperKind::Traces {
            continue;
        }
        let Some(min_w) = nc.min_trace_width_mm else { continue };
        let ampacity = ipc2221_ampacity(min_w, audit.oz, audit.dt_c, audit.external);
        if ampacity <= 0.0 {
            continue;
        }
        if *current > ampacity * audit.margin {
            findings.push(TraceCurrentFinding {
                net_id: nc.net_id,
                net: nc.name.clone(),
                min_width_mm: min_w,
                ampacity_a: ampacity,
                cited_current_a: *current,
                required_width_mm: ipc2221_min_width_mm(*current, audit.oz, audit.dt_c, audit.external),
                citation: citation.clone(),
            });
        }
    }
    findings.sort_by(|a, b| a.net_id.cmp(&b.net_id));
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hand-checked against standard trace-width calculators (1 oz external):
    // 0.25 mm (~10 mil) @ 10 C rise ~ 0.88 A; 1.0 mm @ 10 C ~ 2.39 A.
    #[test]
    fn ampacity_matches_hand_values() {
        let a = ipc2221_ampacity(0.25, 1.0, 10.0, true);
        assert!((a - 0.88).abs() < 0.03, "0.25mm@10C = {a}");
        let b = ipc2221_ampacity(1.0, 1.0, 10.0, true);
        assert!((b - 2.39).abs() < 0.05, "1.0mm@10C = {b}");
        // 20 C rise is the looser common target: 0.25 mm ~ 1.19 A.
        let c = ipc2221_ampacity(0.25, 1.0, 20.0, true);
        assert!((c - 1.19).abs() < 0.04, "0.25mm@20C = {c}");
    }

    #[test]
    fn min_width_is_inverse_of_ampacity() {
        for amps in [0.5, 1.0, 2.0, 4.0] {
            let w = ipc2221_min_width_mm(amps, 1.0, 10.0, true);
            let back = ipc2221_ampacity(w, 1.0, 10.0, true);
            assert!((back - amps).abs() < 1e-6, "roundtrip {amps} -> {w} -> {back}");
        }
    }

    #[test]
    fn internal_layers_carry_less() {
        let ext = ipc2221_ampacity(1.0, 1.0, 10.0, true);
        let int = ipc2221_ampacity(1.0, 1.0, 10.0, false);
        assert!(int < ext, "internal {int} should be below external {ext}");
        // k ratio is 0.024/0.048 = 0.5 exactly.
        assert!((int / ext - 0.5).abs() < 1e-9);
    }

    fn pcb(body: &str) -> Vec<NetCopper> {
        let text = format!("(kicad_pcb (net 0 \"\") {body})");
        let doc = forge_sexpr::parse(&text).unwrap();
        net_copper_from_root(doc.root().unwrap())
    }

    #[test]
    fn narrowest_segment_is_the_bottleneck() {
        // Net 1 routed with a wide 2.0 mm trunk but one 0.25 mm choke segment.
        let body = r#"
          (net 1 "MOTOR")
          (segment (start 0 0) (end 10 0) (width 2.0) (layer "F.Cu") (net 1))
          (segment (start 10 0) (end 11 0) (width 0.25) (layer "F.Cu") (net 1))
          (segment (start 11 0) (end 20 0) (width 2.0) (layer "F.Cu") (net 1))
        "#;
        let c = pcb(body);
        let m = c.iter().find(|n| n.net_id == 1).unwrap();
        assert_eq!(m.kind, CopperKind::Traces);
        assert_eq!(m.min_trace_width_mm, Some(0.25));
        assert_eq!(m.max_trace_width_mm, Some(2.0));
        assert_eq!(m.segment_count, 3);
    }

    #[test]
    fn true_positive_undersized_motor_trace_is_flagged() {
        // A 0.25 mm trace (~0.88 A @ 10 C) carrying a cited 2.0 A motor coil.
        let body = r#"
          (net 1 "MOTOR")
          (segment (start 0 0) (end 10 0) (width 0.25) (layer "F.Cu") (net 1))
        "#;
        let copper = pcb(body);
        let mut cited = HashMap::new();
        cited.insert("MOTOR".to_string(), (2.0, "TMC2226 2.0 A RMS coil".to_string()));
        let f = audit_trace_currents(&copper, &cited, &TraceAudit::default());
        assert_eq!(f.len(), 1, "the undersized motor trace must fire");
        let fd = &f[0];
        assert_eq!(fd.net, "MOTOR");
        assert!((fd.min_width_mm - 0.25).abs() < 1e-9);
        assert!(fd.ampacity_a < 1.0, "0.25mm ampacity {} should be <1A", fd.ampacity_a);
        assert!(fd.required_width_mm > 0.25, "fix needs a wider trace");
    }

    #[test]
    fn poured_net_is_never_flagged_even_with_thin_stub() {
        // The killer false-positive shape: a net with a copper POUR plus a thin
        // 0.25 mm pad-entry stub, carrying real motor current. The min segment
        // width is 0.25 mm but the real conductor is the pour. Must NOT fire.
        let body = r#"
          (net 1 "VDC")
          (segment (start 0 0) (end 1 0) (width 0.25) (layer "F.Cu") (net 1))
          (zone (net 1) (net_name "VDC") (layers "F.Cu")
            (filled_polygon (layer "F.Cu") (pts (xy 0 0) (xy 50 0) (xy 50 50) (xy 0 50))))
        "#;
        let copper = pcb(body);
        let vdc = copper.iter().find(|n| n.name == "VDC").unwrap();
        assert_eq!(vdc.kind, CopperKind::Poured, "VDC carries a zone -> Poured");
        let mut cited = HashMap::new();
        cited.insert("VDC".to_string(), (8.0, "6x TMC2226 motor supply".to_string()));
        let f = audit_trace_currents(&copper, &cited, &TraceAudit::default());
        assert!(f.is_empty(), "a poured net must never be flagged: {f:?}");
    }

    #[test]
    fn adequately_sized_trace_is_not_flagged() {
        // 3.0 mm trace (~5.3 A @ 10 C) carrying a cited 2.0 A: comfortable.
        let body = r#"
          (net 1 "VMOT")
          (segment (start 0 0) (end 10 0) (width 3.0) (layer "F.Cu") (net 1))
        "#;
        let copper = pcb(body);
        let mut cited = HashMap::new();
        cited.insert("VMOT".to_string(), (2.0, "one stepper".to_string()));
        let f = audit_trace_currents(&copper, &cited, &TraceAudit::default());
        assert!(f.is_empty(), "an adequately-sized trace must not fire: {f:?}");
    }

    #[test]
    fn no_cited_current_means_no_finding() {
        // Even a hair-thin trace is silent if no current is attributed to it
        // (the module never invents a load).
        let body = r#"
          (net 1 "SIGNAL")
          (segment (start 0 0) (end 10 0) (width 0.1) (layer "F.Cu") (net 1))
        "#;
        let copper = pcb(body);
        let cited = HashMap::new();
        let f = audit_trace_currents(&copper, &cited, &TraceAudit::default());
        assert!(f.is_empty());
    }
}
