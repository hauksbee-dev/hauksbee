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
/// Finished thickness of 1 oz copper in mm (`MIL_PER_OZ * MM_PER_MIL`), used to
/// convert a stackup's declared per-layer thickness back into copper weight.
const MM_PER_OZ: f64 = MIL_PER_OZ * MM_PER_MIL;

/// Where a net's copper weight and layer side came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopperSource {
    /// Read from the board's own `(setup (stackup ...))` copper layers.
    Stackup,
    /// No stackup declared (or the bottleneck's layer is not in it): the stated
    /// 1 oz external default. Every message built from this is marked ASSUMED.
    Assumed,
}

/// Per-copper-layer finished weight and side, read from a board's declared
/// stackup.
///
/// Ampacity is not a per-board constant. IPC-2221 halves `k` (0.048 -> 0.024) for
/// internal copper, and inner layers are routinely half the weight of the outer
/// ones (0.5 oz inner against 1 oz outer is the common 4-layer build), so a
/// 0.5 oz inner trace rates at roughly 30% of the 1 oz external figure for the
/// same width. Rating every net as 1 oz external therefore over-states inner-layer
/// capacity by about 3x and lets genuinely undersized inner traces pass.
#[derive(Debug, Clone, Default)]
pub struct CopperWeights {
    /// Copper layer name (`F.Cu`, `In1.Cu`, ...) -> (weight in oz, is external).
    by_layer: HashMap<String, (f64, bool)>,
}

impl CopperWeights {
    /// Read the per-layer copper weights from a KiCad `(setup (stackup ...))`
    /// block. Returns an empty table when the board declares no stackup, which is
    /// how a caller distinguishes "measured" from "assumed".
    ///
    /// The outer layers are the first and last **copper** entries in declared
    /// top-to-bottom order; everything between them is internal. A stackup that
    /// declares a copper layer without a thickness contributes nothing, so that
    /// layer falls back to the assumed default rather than to a zero rating.
    pub fn from_root(root: &List) -> Self {
        let mut declared: Vec<(String, f64)> = Vec::new();
        if let Some(stackup) = root.find("setup").and_then(|s| s.find("stackup")) {
            for layer in stackup.find_all("layer") {
                let ty = layer.find_value("type").unwrap_or_default();
                if !ty.eq_ignore_ascii_case("copper") {
                    continue;
                }
                let Some(name) = layer.arg_value(0) else {
                    continue;
                };
                let thickness = layer.find_f64("thickness").unwrap_or(0.0);
                declared.push((name, thickness));
            }
        }
        let last = declared.len().saturating_sub(1);
        let mut by_layer = HashMap::new();
        for (i, (name, thickness)) in declared.iter().enumerate() {
            if *thickness <= 0.0 {
                continue;
            }
            let external = i == 0 || i == last;
            by_layer.insert(name.clone(), (thickness / MM_PER_OZ, external));
        }
        CopperWeights { by_layer }
    }

    /// Whether any per-layer copper weight was declared.
    pub fn is_empty(&self) -> bool {
        self.by_layer.is_empty()
    }

    /// The weight (oz) and side for a copper layer, when the stackup declares it.
    pub fn get(&self, layer: &str) -> Option<(f64, bool)> {
        self.by_layer.get(layer).copied()
    }
}

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
    /// Copper layer the narrowest segment sits on, which decides its weight and
    /// whether IPC-2221's internal derating applies.
    pub min_trace_layer: Option<String>,
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
        self.min_trace_width_mm
            .map(|w| ipc2221_ampacity(w, oz, dt_c, external))
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
    // net id -> name, and the inverse (name -> id) so a track that cites its
    // net by NAME rather than numeric id (some KiCad generations) still resolves.
    let mut names: HashMap<i64, String> = HashMap::new();
    let mut by_name: HashMap<String, i64> = HashMap::new();
    for n in root.find_all("net") {
        if let (Some(id), Some(name)) = (n.arg_i64(0), n.arg_value(1)) {
            let name = crate::netname::unescape_net_name(&name);
            names.entry(id).or_insert(name.clone());
            by_name.entry(name).or_insert(id);
        }
    }

    let mut tally = WidthTally::default();
    let mut zone_count: HashMap<i64, usize> = HashMap::new();

    // Track segments (straight).
    for seg in root.find_all("segment") {
        let layer = seg.find_value("layer").unwrap_or_default();
        if !layer.ends_with(".Cu") {
            continue;
        }
        let Some(id) = net_id_of(seg, &by_name) else {
            continue;
        };
        let w = seg.find_f64("width").unwrap_or(0.0);
        if w <= 0.0 {
            continue;
        }
        tally.accumulate(id, w, &layer);
    }
    // Arc tracks carry a width too.
    for arc in root.find_all("arc") {
        let layer = arc.find_value("layer").unwrap_or_default();
        if !layer.ends_with(".Cu") {
            continue;
        }
        let Some(id) = net_id_of(arc, &by_name) else {
            continue;
        };
        let w = arc.find_f64("width").unwrap_or(0.0);
        if w <= 0.0 {
            continue;
        }
        tally.accumulate(id, w, &layer);
    }
    // Zones: a net that carries a filled pour has its real cross-section in the
    // pour, not the segments. Record the zone count so the net is marked Poured.
    for zone in root.find_all("zone") {
        let Some(id) = net_id_of(zone, &by_name) else {
            continue;
        };
        // Only copper-layer zones count (keepouts on edge cuts do not conduct).
        let on_copper = zone
            .find("layers")
            .map(|l| {
                (0..)
                    .map_while(|i| l.arg_value(i))
                    .any(|n| n.ends_with(".Cu"))
            })
            .unwrap_or(false)
            || zone
                .find_value("layer")
                .map(|l| l.ends_with(".Cu"))
                .unwrap_or(false);
        if !on_copper {
            continue;
        }
        *zone_count.entry(id).or_default() += 1;
        tally.seen.insert(id);
    }

    let mut out = Vec::new();
    for id in &tally.seen {
        let id = *id;
        let zc = zone_count.get(&id).copied().unwrap_or(0);
        let sc = tally.seg_count.get(&id).copied().unwrap_or(0);
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
            min_trace_width_mm: tally.min_w.get(&id).copied(),
            min_trace_layer: tally.min_layer.get(&id).cloned(),
            max_trace_width_mm: tally.max_w.get(&id).copied(),
            segment_count: sc,
            zone_count: zc,
        });
    }
    out.sort_by_key(|n| n.net_id);
    out
}

#[derive(Default)]
struct WidthTally {
    min_w: HashMap<i64, f64>,
    max_w: HashMap<i64, f64>,
    /// Layer of the narrowest segment seen so far, kept in step with `min_w`.
    min_layer: HashMap<i64, String>,
    seg_count: HashMap<i64, usize>,
    seen: HashSet<i64>,
}

impl WidthTally {
    fn accumulate(&mut self, id: i64, w: f64, layer: &str) {
        self.seen.insert(id);
        *self.seg_count.entry(id).or_default() += 1;
        let is_new_min = self.min_w.get(&id).is_none_or(|m| w < *m);
        self.min_w
            .entry(id)
            .and_modify(|m| *m = m.min(w))
            .or_insert(w);
        self.max_w
            .entry(id)
            .and_modify(|m| *m = m.max(w))
            .or_insert(w);
        if is_new_min {
            self.min_layer.insert(id, layer.to_string());
        }
    }
}

/// Resolve a primitive's `(net ...)` child to a net id. Handles the numeric
/// form `(net N)` / `(net N "name")` AND the name-only form `(net "NAME")` some
/// KiCad generations emit on tracks, resolving the latter through the file's own
/// `(net N "name")` declarations. Without the name fallback a name-only export
/// yields zero segments, so the IPC-2221 ampacity audit silently reports no
/// copper, a false all-clear on a safety check.
fn net_id_of(list: &List, by_name: &HashMap<String, i64>) -> Option<i64> {
    let net = list.find("net")?;
    if let Some(id) = net.arg_i64(0) {
        return Some(id);
    }
    // The table keys are unescaped display names, so a name-only reference
    // must be normalized the same way before lookup.
    by_name
        .get(&crate::netname::unescape_net_name(&net.arg_value(0)?))
        .copied()
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
    /// Copper layer the bottleneck sits on, when the layout named one.
    pub layer: Option<String>,
    /// Copper weight (oz) the rating used.
    pub oz: f64,
    /// Whether the rating used the external (0.048) or internal (0.024) constant.
    pub external: bool,
    /// Whether `oz` / `external` were declared by the board or assumed.
    pub copper_source: CopperSource,
}

impl TraceCurrentFinding {
    /// The copper basis in words, marked ASSUMED (and naming the upload that
    /// would settle it) whenever the board did not declare the layer.
    pub fn describe_copper(&self) -> String {
        let side = if self.external {
            "external"
        } else {
            "internal"
        };
        match self.copper_source {
            CopperSource::Stackup => match &self.layer {
                Some(l) => format!("{} {side} {l}, per the board stackup", describe_oz(self.oz)),
                None => format!("{} {side}, per the board stackup", describe_oz(self.oz)),
            },
            CopperSource::Assumed => format!(
                "ASSUMED {} {side} - the layout declares no copper weight for {}, so upload a \
                 stackup declaration or fab drawing to rate the real copper",
                describe_oz(self.oz),
                self.layer.as_deref().unwrap_or("this layer"),
            ),
        }
    }
}

/// Capacity-only row for a power-looking net when no current attribution exists.
#[derive(Debug, Clone)]
pub struct TraceCapacityRow {
    pub net_id: i64,
    pub net: String,
    pub kind: CopperKind,
    pub min_width_mm: Option<f64>,
    pub segment_count: usize,
    pub zone_count: usize,
    pub capacity_10c_a: f64,
    pub capacity_20c_a: f64,
    /// Copper weight (oz) the capacities were computed at.
    pub oz: f64,
    /// Whether the internal (0.024) or external (0.048) constant was used.
    pub external: bool,
    /// Whether `oz` / `external` came from the board or were assumed.
    pub copper_source: CopperSource,
}

/// Audit parameters (copper weight, target rise, layer side). The `oz` /
/// `external` pair is only the **fallback** for a board that declares no
/// stackup; when `copper` holds a declared per-layer table, each net is rated on
/// the layer its bottleneck actually sits on.
#[derive(Debug, Clone)]
pub struct TraceAudit {
    /// Fallback copper weight (oz) when the layer is not in the declared stackup.
    pub oz: f64,
    pub dt_c: f64,
    /// Fallback layer side when the layer is not in the declared stackup.
    pub external: bool,
    /// Per-layer copper weights read from the board. Empty means undeclared, and
    /// every rating built from the fallback is then marked ASSUMED.
    pub copper: CopperWeights,
    /// A net must miss its rating by more than this factor before it is flagged,
    /// to keep rounding-level near-misses out (1.0 = flag any shortfall).
    pub margin: f64,
}

impl Default for TraceAudit {
    fn default() -> Self {
        TraceAudit {
            oz: OZ_1,
            dt_c: 10.0,
            external: true,
            copper: CopperWeights::default(),
            margin: 1.0,
        }
    }
}

impl TraceAudit {
    /// The default audit, with per-layer copper weights read from `root`.
    pub fn from_root(root: &List) -> Self {
        TraceAudit {
            copper: CopperWeights::from_root(root),
            ..Default::default()
        }
    }

    /// The default audit, with per-layer copper weights read from raw
    /// `.kicad_pcb` text. Non-KiCad or unparseable text yields the plain default,
    /// whose ratings are then marked ASSUMED.
    pub fn from_pcb_text(text: &str) -> Self {
        if !text.contains("(kicad_pcb") {
            return Self::default();
        }
        match forge_sexpr::parse(text)
            .ok()
            .and_then(|d| d.root().map(CopperWeights::from_root))
        {
            Some(copper) => TraceAudit {
                copper,
                ..Default::default()
            },
            None => Self::default(),
        }
    }

    /// The copper weight and side to rate a trace on `layer` against, plus where
    /// those numbers came from. An undeclared layer falls back to the stated
    /// default rather than refusing to rate the net, but the source records that
    /// so the message can say ASSUMED.
    pub fn copper_for(&self, layer: Option<&str>) -> (f64, bool, CopperSource) {
        match layer.and_then(|l| self.copper.get(l)) {
            Some((oz, external)) => (oz, external, CopperSource::Stackup),
            None => (self.oz, self.external, CopperSource::Assumed),
        }
    }
}

/// Render a copper weight the way a fab drawing does: `1 oz`, `0.5 oz`.
pub fn describe_oz(oz: f64) -> String {
    if (oz - oz.round()).abs() < 0.01 {
        format!("{:.0} oz", oz.round())
    } else {
        format!("{oz:.2} oz")
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
        let Some(min_w) = nc.min_trace_width_mm else {
            continue;
        };
        // Rate the bottleneck on the copper it is actually built in: an inner
        // 0.5 oz trace carries roughly a third of what the same width carries as
        // 1 oz outer copper.
        let (oz, external, copper_source) = audit.copper_for(nc.min_trace_layer.as_deref());
        let ampacity = ipc2221_ampacity(min_w, oz, audit.dt_c, external);
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
                required_width_mm: ipc2221_min_width_mm(*current, oz, audit.dt_c, external),
                citation: citation.clone(),
                layer: nc.min_trace_layer.clone(),
                oz,
                external,
                copper_source,
            });
        }
    }
    findings.sort_by(|a, b| a.net_id.cmp(&b.net_id));
    findings
}

/// Capacity-only report for power-looking nets. This never produces pass/fail:
/// without a cited current the honest output is "this routed bottleneck can
/// carry roughly N amps", or "poured net: out of reach".
pub fn trace_capacity_report(copper: &[NetCopper], audit: &TraceAudit) -> Vec<TraceCapacityRow> {
    let mut rows = Vec::new();
    for nc in copper {
        if !power_like_net(&nc.name) {
            continue;
        }
        let (oz, external, copper_source) = audit.copper_for(nc.min_trace_layer.as_deref());
        let (capacity_10c_a, capacity_20c_a) = if nc.kind == CopperKind::Traces {
            (
                nc.min_trace_width_mm
                    .map(|w| ipc2221_ampacity(w, oz, 10.0, external))
                    .unwrap_or(f64::NAN),
                nc.min_trace_width_mm
                    .map(|w| ipc2221_ampacity(w, oz, 20.0, external))
                    .unwrap_or(f64::NAN),
            )
        } else {
            (f64::NAN, f64::NAN)
        };
        rows.push(TraceCapacityRow {
            net_id: nc.net_id,
            net: nc.name.clone(),
            kind: nc.kind,
            min_width_mm: nc.min_trace_width_mm,
            segment_count: nc.segment_count,
            zone_count: nc.zone_count,
            capacity_10c_a,
            capacity_20c_a,
            oz,
            external,
            copper_source,
        });
    }
    rows.sort_by(|a, b| a.net_id.cmp(&b.net_id).then(a.net.cmp(&b.net)));
    rows
}

fn power_like_net(name: &str) -> bool {
    let n = name
        .rsplit('/')
        .next()
        .unwrap_or(name)
        .trim()
        .to_ascii_uppercase();
    n.starts_with('+')
        || matches!(
            n.as_str(),
            "GND"
                | "GNDA"
                | "GNDD"
                | "PGND"
                | "VBUS"
                | "VBAT"
                | "VBATT"
                | "BATT"
                | "VIN"
                | "VOUT"
                | "VSYS"
                | "VCC"
                | "VDD"
        )
        || n.contains("BATT")
        || n.contains("VBUS")
        || n.contains("VMOT")
        || n.contains("VCC")
        || n.contains("VDD")
        || n.contains("POWER")
        || n.contains("PWR")
}

/// The copper basis of a capacity row in table-width form: `0.5 oz int`,
/// `1 oz ext (assumed)`.
fn compact_copper(r: &TraceCapacityRow) -> String {
    let side = if r.external { "ext" } else { "int" };
    let assumed = if r.copper_source == CopperSource::Assumed {
        " (assumed)"
    } else {
        ""
    };
    format!("{} {side}{assumed}", describe_oz(r.oz))
}

pub fn render_trace_capacity_report(rows: &[TraceCapacityRow]) -> String {
    let mut out = String::new();
    out.push_str("ampacity: IPC-2221 capacity only; supply a current for pass/fail.\n");
    if rows.is_empty() {
        out.push_str("no power-like routed nets found.\n");
        return out;
    }
    // The copper basis decides every number in the table, so it is stated above
    // it rather than left for the reader to assume.
    if rows
        .iter()
        .any(|r| r.copper_source == CopperSource::Assumed)
    {
        out.push_str(
            "copper weight ASSUMED 1 oz external: the layout declares no stackup, so upload a \
             stackup declaration or fab drawing to rate the real copper.\n",
        );
    } else {
        out.push_str("copper weight and layer side read from the board stackup.\n");
    }
    // Column widths follow the content (with sane caps), so headers stay
    // aligned and cells are never chopped mid-word to fit a fixed grid.
    let cells: Vec<(String, String, String, String, String)> = rows
        .iter()
        .map(|r| {
            let (width, cap, note) = match r.kind {
                CopperKind::Traces => (
                    r.min_width_mm
                        .map(|w| format!("{w:.3} mm"))
                        .unwrap_or_else(|| "-".to_string()),
                    if r.capacity_10c_a.is_finite() {
                        format!("{:.2} A", r.capacity_10c_a)
                    } else {
                        "-".to_string()
                    },
                    if r.capacity_20c_a.is_finite() {
                        format!("20C {:.2} A, {}", r.capacity_20c_a, compact_copper(r))
                    } else {
                        "-".to_string()
                    },
                ),
                CopperKind::Poured => (
                    r.min_width_mm
                        .map(|w| format!("{w:.3} mm"))
                        .unwrap_or_else(|| "-".to_string()),
                    "-".to_string(),
                    format!("poured ({} zone)", r.zone_count),
                ),
                CopperKind::None => ("-".to_string(), "-".to_string(), "no copper".to_string()),
            };
            let kind = match r.kind {
                CopperKind::Traces => "traces",
                CopperKind::Poured => "poured",
                CopperKind::None => "none",
            };
            (truncate(&r.net, 40), kind.to_string(), width, cap, note)
        })
        .collect();
    let headers = ("Net", "Copper", "Min width", "Cap @10C", "Note");
    let w0 = cells
        .iter()
        .map(|c| c.0.chars().count())
        .chain([headers.0.len()])
        .max()
        .unwrap();
    let w1 = cells
        .iter()
        .map(|c| c.1.chars().count())
        .chain([headers.1.len()])
        .max()
        .unwrap();
    let w2 = cells
        .iter()
        .map(|c| c.2.chars().count())
        .chain([headers.2.len()])
        .max()
        .unwrap();
    let w3 = cells
        .iter()
        .map(|c| c.3.chars().count())
        .chain([headers.3.len()])
        .max()
        .unwrap();
    let w4 = cells
        .iter()
        .map(|c| c.4.chars().count())
        .chain([headers.4.len()])
        .max()
        .unwrap();
    let pad = |s: &str, w: usize| {
        let len = s.chars().count();
        format!("{}{}", s, " ".repeat(w - len))
    };
    let rule = |l: &str, m: &str, r: &str| {
        format!(
            "{l}\u{2500}{}\u{2500}{m}\u{2500}{}\u{2500}{m}\u{2500}{}\u{2500}{m}\u{2500}{}\u{2500}{m}\u{2500}{}\u{2500}{r}\n",
            "\u{2500}".repeat(w0),
            "\u{2500}".repeat(w1),
            "\u{2500}".repeat(w2),
            "\u{2500}".repeat(w3),
            "\u{2500}".repeat(w4),
        )
    };
    out.push_str(&rule("\u{250c}", "\u{252c}", "\u{2510}"));
    out.push_str(&format!(
        "\u{2502} {} \u{2502} {} \u{2502} {} \u{2502} {} \u{2502} {} \u{2502}\n",
        pad(headers.0, w0),
        pad(headers.1, w1),
        pad(headers.2, w2),
        pad(headers.3, w3),
        pad(headers.4, w4),
    ));
    out.push_str(&rule("\u{251c}", "\u{253c}", "\u{2524}"));
    for c in &cells {
        out.push_str(&format!(
            "\u{2502} {} \u{2502} {} \u{2502} {} \u{2502} {} \u{2502} {} \u{2502}\n",
            pad(&c.0, w0),
            pad(&c.1, w1),
            pad(&c.2, w2),
            pad(&c.3, w3),
            pad(&c.4, w4),
        ));
    }
    out.push_str(&rule("\u{2514}", "\u{2534}", "\u{2518}"));
    out
}

/// Cap a cell at `max` characters, marking the cut with an ellipsis instead of
/// a silent mid-word chop.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('\u{2026}');
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_only_net_reference_still_resolves_copper() {
        // A track that cites its net by NAME rather than numeric id (some KiCad
        // generations) must still be measured, otherwise the IPC-2221 ampacity
        // audit silently sees zero copper on the net, a false all-clear.
        let pcb = r#"(kicad_pcb
          (net 0 "")
          (net 1 "GND")
          (segment (start 0 0) (end 1 0) (width 0.25) (layer "F.Cu") (net "GND"))
        )"#;
        let nets = net_copper_from_text(pcb);
        let gnd = nets
            .iter()
            .find(|n| n.name == "GND")
            .expect("GND net should be measured from its name-only track");
        assert_eq!(gnd.kind, CopperKind::Traces);
        assert!((gnd.min_trace_width_mm.unwrap() - 0.25).abs() < 1e-9);
    }

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
            assert!(
                (back - amps).abs() < 1e-6,
                "roundtrip {amps} -> {w} -> {back}"
            );
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

    #[test]
    fn nonfinite_20c_capacity_renders_as_dash() {
        // Bug-hunt #11: a Traces net whose width could not be measured carries a
        // NaN capacity (the 10C column already guards this). The 20C column must
        // fall back to "-" too, never emit a nonsensical "20C NaN A".
        let row = TraceCapacityRow {
            net_id: 1,
            net: "MOTOR".to_string(),
            kind: CopperKind::Traces,
            min_width_mm: None,
            segment_count: 3,
            zone_count: 0,
            capacity_10c_a: f64::NAN,
            capacity_20c_a: f64::NAN,
        };
        let out = render_trace_capacity_report(&[row]);
        assert!(
            !out.contains("NaN"),
            "report leaked a NaN into the table:\n{out}"
        );
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
        cited.insert(
            "MOTOR".to_string(),
            (2.0, "TMC2226 2.0 A RMS coil".to_string()),
        );
        let f = audit_trace_currents(&copper, &cited, &TraceAudit::default());
        assert_eq!(f.len(), 1, "the undersized motor trace must fire");
        let fd = &f[0];
        assert_eq!(fd.net, "MOTOR");
        assert!((fd.min_width_mm - 0.25).abs() < 1e-9);
        assert!(
            fd.ampacity_a < 1.0,
            "0.25mm ampacity {} should be <1A",
            fd.ampacity_a
        );
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
        cited.insert(
            "VDC".to_string(),
            (8.0, "6x TMC2226 motor supply".to_string()),
        );
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
        assert!(
            f.is_empty(),
            "an adequately-sized trace must not fire: {f:?}"
        );
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

    #[test]
    fn capacity_report_includes_power_like_trace_nets_without_claiming_pass_fail() {
        let body = r#"
          (net 1 "+BATT")
          (net 2 "SDA")
          (segment (start 0 0) (end 10 0) (width 0.5) (layer "F.Cu") (net 1))
          (segment (start 0 2) (end 10 2) (width 0.15) (layer "F.Cu") (net 2))
        "#;
        let copper = pcb(body);
        let report = trace_capacity_report(&copper, &TraceAudit::default());
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].net, "+BATT");
        assert!(report[0].capacity_10c_a > 1.0);
        let rendered = render_trace_capacity_report(&report);
        assert!(rendered.contains("capacity only"));
        assert!(rendered.contains("supply a current"));
        assert!(!rendered.contains("SDA"));
    }

    #[test]
    fn capacity_report_marks_poured_power_nets_out_of_reach() {
        let body = r#"
          (net 1 "VBUS")
          (segment (start 0 0) (end 1 0) (width 0.2) (layer "F.Cu") (net 1))
          (zone (net 1) (net_name "VBUS") (layers "F.Cu")
            (filled_polygon (layer "F.Cu") (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10))))
        "#;
        let copper = pcb(body);
        let report = trace_capacity_report(&copper, &TraceAudit::default());
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].kind, CopperKind::Poured);
        assert!(report[0].capacity_10c_a.is_nan());
        assert!(render_trace_capacity_report(&report).contains("poured"));
    }
}
