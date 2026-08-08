//! Copper short / clearance detection (geometric DRC).
//!
//! Hauksbee simulates from a real layout, so two pieces of copper that touch
//! when they belong to different nets are an *electrical* fact the simulation
//! must know about: a solder bridge, an overlapping pad, a pour eating into a
//! track. This module finds those from geometry alone and hands them to the
//! engine, which can then merge the shorted nets and show the consequence.
//!
//! ## What is checked
//!
//! Every conductive primitive on a copper layer is reduced to one of three
//! shapes, all in board millimetres:
//!   - **Capsule** (a "stadium"): a track segment of finite width, or an arc
//!     sampled into a short capsule chain. Distance to anything else is the
//!     segment-to-segment distance minus both half-widths.
//!   - **Disc**: a round pad, or a via / through-hole pad annular ring. Spans
//!     the layers it touches (vias and `*.Cu` pads sit on every copper layer).
//!   - **Polygon**: a rectangular / rounded / oval / custom pad outline, or a
//!     filled zone area. Distance uses closed-polygon edge distance, and a
//!     point-in-polygon test catches full containment.
//!
//! Primitives are bucketed per copper layer and indexed in an [`rstar`]
//! R*-tree, so each one is only distance-tested against neighbours whose
//! bounding boxes are within the clearance window. That keeps an 85 MB board
//! (hundreds of thousands of primitives) to a few seconds instead of the
//! O(n²) blow-up a naive all-pairs sweep would cost.
//!
//! ## Overlap vs clearance
//!
//! For a candidate pair on different nets we compute the signed gap (copper
//! edge to copper edge, already accounting for widths):
//!   - gap `<= 0` → the copper actually intersects: a **short**
//!     ([`ViolationKind::Short`]).
//!   - `0 < gap < clearance` → they do not touch but sit closer than the
//!     design rule allows: a **clearance violation**
//!     ([`ViolationKind::Clearance`]), a near-short risk, lower severity.
//!
//! Clearance is read from the board's design rules when present, else a sane
//! default ([`DEFAULT_CLEARANCE_MM`]).

use std::collections::HashMap;

use forge_sexpr::{Document, List};
use rstar::{RTree, RTreeObject, AABB};
use serde::{Deserialize, Serialize};

/// Default copper-to-copper clearance (mm) when the board states no rule.
pub const DEFAULT_CLEARANCE_MM: f64 = 0.2;

/// Tolerance band (mm) below the clearance rule inside which a gap is treated as
/// *at* the rule, not a violation.
///
/// A gap reported as `clearance - epsilon` is overwhelmingly a routing-to-rule
/// artifact: KiCad lets the router lay copper *at* the design rule, and the nm
/// grid plus our capsule/arc flattening (chord error a few microns, see
/// `ARC_SEGMENTS`) leaves the measured gap a hair under the nominal rule. Those
/// boundary gaps generated 137 spurious clearance notes on bms-c1 and 66 on the
/// PD-sink board, drowning real findings. So a gap within this many microns of
/// the rule is not a violation; only a gap genuinely *under* (rule - tolerance)
/// is. Shorts (gap <= 0, actual copper overlap) are unaffected: this only
/// raises the floor for the soft clearance band, never for true intersections.
/// 5 um is well under any real copper clearance (the tightest fab rules are
/// ~75 um) yet above the geometry's own rounding noise.
pub const CLEARANCE_TOLERANCE_MM: f64 = 0.005;

/// Gaps at or below this (mm) are copper *touching*: a short, not a clearance.
///
/// `gap <= 0.0` was the whole test, and it is not enough. The gap comes out of
/// `shape_gap`, which subtracts and square-roots f64 coordinates in millimetres;
/// on a 300 mm board that arithmetic carries an absolute error around 1e-13 mm,
/// so two edges that meet exactly can measure a hair *positive*. A real corpus
/// board produced a 9.77e-15 mm gap between different nets, which is touching
/// copper by any physical reading, and the bare `> 0.0` test filed it as a
/// clearance note instead of a short. Under-reporting a short is the worst
/// failure this detector has: it is the one finding a board cannot ship with.
///
/// 1e-9 mm (one picometre) is the band. It sits four orders above the f64 noise
/// floor described above, and three orders BELOW KiCad's nanometre coordinate
/// grid, which is the finest gap a KiCad file can even express. So it cannot
/// swallow a gap any real design intended: the smallest representable non-zero
/// gap, 1e-6 mm, is a thousand times wider than this band.
pub const SHORT_TOUCH_EPS_MM: f64 = 1e-9;

/// Whether a measured gap means the copper is in contact.
///
/// Negative is overlap, zero is abutment, and anything inside
/// [`SHORT_TOUCH_EPS_MM`] is one of those two measured through floating point.
pub fn is_touching(gap_mm: f64) -> bool {
    gap_mm <= SHORT_TOUCH_EPS_MM
}

/// Arcs are flattened into this many straight capsule links. Eight keeps the
/// chord error under a few microns for typical track-radius arcs while staying
/// cheap.
const ARC_SEGMENTS: usize = 8;

/// One KiCad net class's clearance-relevant DRC values.
#[derive(Debug, Clone, PartialEq)]
pub struct NetClassRule {
    pub name: String,
    pub clearance_mm: f64,
    pub diff_pair_gap_mm: Option<f64>,
}

/// Clearance resolver shared by the KiCad and Eagle DRC paths (KiCad fills
/// classes from the `.kicad_pro`; Eagle fills classes and the pair matrix
/// from `<classes>`).
///
/// The project file can assign nets to classes explicitly or by wildcard
/// pattern. Once resolved to concrete net names, the pair rule is KiCad's
/// conservative max(class A clearance, class B clearance), except the two
/// halves of a same-class differential pair may use that class's
/// `diff_pair_gap`.
#[derive(Debug, Clone, PartialEq)]
pub struct ClearanceRules {
    pub default_clearance_mm: f64,
    classes: HashMap<String, NetClassRule>,
    net_classes: HashMap<String, String>,
    /// Explicit clearance for a concrete (class, class) pair, keyed with the
    /// lexically smaller name first. Eagle's net-class model carries a pair
    /// matrix (class N declares `<clearance class="M" value="V"/>` entries):
    /// an explicit entry wins over the max-of-two-classes fallback in
    /// [`Self::effective_clearance`] (it may relax a pair below the classes'
    /// own rules); pairs WITHOUT an entry fall through to that max rule,
    /// matching Eagle's larger-value-wins behaviour for nets of different
    /// classes.
    class_pair_clearances: HashMap<(String, String), f64>,
}

impl ClearanceRules {
    pub fn new(default_clearance_mm: f64) -> Self {
        let default = if default_clearance_mm > 0.0 {
            default_clearance_mm
        } else {
            DEFAULT_CLEARANCE_MM
        };
        Self {
            default_clearance_mm: default,
            classes: HashMap::new(),
            net_classes: HashMap::new(),
            class_pair_clearances: HashMap::new(),
        }
    }

    pub fn add_class(&mut self, rule: NetClassRule) {
        // Keep a class that carries ANY usable rule, an explicit clearance OR a
        // diff-pair gap. A KiCad class routinely leaves `clearance` at 0 ("inherit
        // board default") while still defining a `diff_pair_gap`; dropping it
        // wholesale (a bare `clearance_mm > 0.0` gate) discards the gap AND makes
        // `assign_net` reject its nets, so a diff pair routed at its own gap gets
        // checked against the wider board default and falsely flagged. A class
        // with clearance 0 is retained and resolves to the board default for
        // spacing (see `clearance_for_net`) while still contributing its gap.
        if rule.clearance_mm > 0.0 || rule.diff_pair_gap_mm.is_some_and(|g| g > 0.0) {
            self.classes.insert(rule.name.clone(), rule);
        }
    }

    pub fn assign_net(&mut self, net: &str, class: &str) {
        if self.classes.contains_key(class) || class == "Default" {
            self.net_classes.insert(net.to_string(), class.to_string());
        }
    }

    pub fn clearance_for_net(&self, net: &str) -> f64 {
        self.net_classes
            .get(net)
            .and_then(|class| self.classes.get(class))
            // A class clearance of 0 means "inherit the board default", not a
            // literal zero-clearance rule, resolve it to the default so a
            // diff-pair-only class does not report a 0 mm spacing requirement.
            .map(|r| {
                if r.clearance_mm > 0.0 {
                    r.clearance_mm
                } else {
                    self.default_clearance_mm
                }
            })
            .unwrap_or(self.default_clearance_mm)
    }

    /// Record an explicit clearance for a concrete class pair (either order;
    /// `a == b` pins the class's own same-class rule). Pair entries take
    /// precedence over the max-of-two-classes fallback in
    /// [`Self::effective_clearance`]. A duplicate insert keeps the stricter
    /// value.
    pub fn add_class_pair_clearance(&mut self, class_a: &str, class_b: &str, clearance_mm: f64) {
        if clearance_mm <= 0.0 {
            return;
        }
        let key = if class_a <= class_b {
            (class_a.to_string(), class_b.to_string())
        } else {
            (class_b.to_string(), class_a.to_string())
        };
        let entry = self.class_pair_clearances.entry(key).or_insert(0.0);
        *entry = entry.max(clearance_mm);
    }

    pub fn effective_clearance(&self, net_a: &str, net_b: &str) -> f64 {
        if let Some(gap) = self.diff_pair_gap(net_a, net_b) {
            return gap;
        }
        if let Some(pair) = self.class_pair_clearance(net_a, net_b) {
            return pair;
        }
        self.clearance_for_net(net_a)
            .max(self.clearance_for_net(net_b))
    }

    /// The explicit pair-matrix clearance for the two nets' classes, when both
    /// nets are class-assigned and the pair has an entry.
    fn class_pair_clearance(&self, net_a: &str, net_b: &str) -> Option<f64> {
        let class_a = self.net_classes.get(net_a)?;
        let class_b = self.net_classes.get(net_b)?;
        let key = if class_a <= class_b {
            (class_a.clone(), class_b.clone())
        } else {
            (class_b.clone(), class_a.clone())
        };
        self.class_pair_clearances.get(&key).copied()
    }

    fn max_clearance(&self) -> f64 {
        self.classes
            .values()
            .map(|r| r.clearance_mm)
            .chain(self.class_pair_clearances.values().copied())
            .fold(self.default_clearance_mm, f64::max)
    }

    fn diff_pair_gap(&self, net_a: &str, net_b: &str) -> Option<f64> {
        let class_a = self.net_classes.get(net_a)?;
        let class_b = self.net_classes.get(net_b)?;
        if class_a != class_b {
            return None;
        }
        if !diff_pair_halves(net_a, net_b) {
            return None;
        }
        self.classes
            .get(class_a)?
            .diff_pair_gap_mm
            .filter(|g| *g > 0.0)
    }
}

impl Default for ClearanceRules {
    fn default() -> Self {
        Self::new(DEFAULT_CLEARANCE_MM)
    }
}

fn diff_pair_halves(a: &str, b: &str) -> bool {
    let strip = |s: &str| -> Option<String> {
        let leaf = s.rsplit('/').next().unwrap_or(s);
        if let Some(base) = leaf.strip_suffix('+') {
            return Some(base.to_string());
        }
        if let Some(base) = leaf.strip_suffix('-') {
            return Some(base.to_string());
        }
        let upper = leaf.to_ascii_uppercase();
        for suffix in ["_P", "_N"] {
            if upper.ends_with(suffix) {
                return Some(leaf[..leaf.len() - suffix.len()].to_string());
            }
        }
        None
    };
    let polarity = |s: &str| -> Option<char> {
        let leaf = s.rsplit('/').next().unwrap_or(s);
        if leaf.ends_with('+') {
            Some('+')
        } else if leaf.ends_with('-') {
            Some('-')
        } else {
            let upper = leaf.to_ascii_uppercase();
            if upper.ends_with("_P") {
                Some('+')
            } else if upper.ends_with("_N") {
                Some('-')
            } else {
                None
            }
        }
    };
    strip(a).zip(strip(b)).is_some_and(|(ba, bb)| ba == bb)
        && polarity(a)
            .zip(polarity(b))
            .is_some_and(|(pa, pb)| pa != pb)
}

/// Parse a KiCad `.kicad_pro` JSON file into concrete clearance rules for the
/// given board net names.
pub fn clearance_rules_from_kicad_pro<'a>(
    text: &str,
    net_names: impl IntoIterator<Item = &'a str>,
) -> Option<ClearanceRules> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let settings = v.get("net_settings")?;
    let classes = settings.get("classes")?.as_array()?;
    let mut default_clearance = DEFAULT_CLEARANCE_MM;
    let mut rules = ClearanceRules::default();
    for class in classes {
        // Skip a nameless/malformed class entry rather than aborting the whole
        // parse. A bare `?` here propagates None out of the function, so one bad
        // object (e.g. in a hand-edited .kicad_pro) silently discards EVERY
        // class, default clearance, and diff-pair gap, dropping DRC to the bare
        // default everywhere, a KiCad 10 board keeps its clearances only here.
        // The sibling assignment/pattern loops already skip bad entries; match
        // that.
        let Some(name) = class
            .get("name")
            .and_then(|x| x.as_str())
            .map(str::to_string)
        else {
            continue;
        };
        let clearance = class
            .get("clearance")
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0);
        let diff_gap = class
            .get("diff_pair_gap")
            .and_then(|x| x.as_f64())
            .filter(|g| *g > 0.0);
        if name == "Default" && clearance > 0.0 {
            default_clearance = clearance;
        }
        rules.add_class(NetClassRule {
            name,
            clearance_mm: clearance,
            diff_pair_gap_mm: diff_gap,
        });
    }
    rules.default_clearance_mm = default_clearance;

    if let Some(assignments) = settings
        .get("netclass_assignments")
        .and_then(|x| x.as_object())
    {
        for (net, class) in assignments {
            if let Some(class) = class.as_str() {
                rules.assign_net(net, class);
            } else if let Some(classes) = class.as_array() {
                if let Some(class) = classes.first().and_then(|x| x.as_str()) {
                    rules.assign_net(net, class);
                }
            }
        }
    }

    let nets: Vec<&str> = net_names.into_iter().collect();
    if let Some(patterns) = settings.get("netclass_patterns").and_then(|x| x.as_array()) {
        for pat in patterns {
            let Some(class) = pat.get("netclass").and_then(|x| x.as_str()) else {
                continue;
            };
            let Some(pattern) = pat.get("pattern").and_then(|x| x.as_str()) else {
                continue;
            };
            for net in &nets {
                if !rules.net_classes.contains_key(*net)
                    && kicad_netclass_pattern_matches(pattern, net)
                {
                    rules.assign_net(net, class);
                }
            }
        }
    }

    Some(rules)
}

fn kicad_netclass_pattern_matches(pattern: &str, net: &str) -> bool {
    // Iterative two-pointer glob: on a mismatch, back up to just past the most
    // recent `*` and let it absorb one more net character. Each retry advances
    // that star's anchor, so matching is O(len(pattern) · len(net)), no
    // exponential backtracking on `*`-heavy patterns from a crafted .kicad_pro
    // (a recursive `inner(&p[1..], n) || inner(p, &n[1..])` does backtrack).
    fn inner(p: &[char], n: &[char]) -> bool {
        let (mut pi, mut ni) = (0usize, 0usize);
        // Resume point: (pattern index after the last `*`, net index the next
        // retry should restart from).
        let mut star: Option<(usize, usize)> = None;
        while ni < n.len() {
            // How many (pattern, net) chars the token at p[pi] consumes
            // against n[ni]; None on a mismatch.
            let step: Option<(usize, usize)> = if pi >= p.len() {
                None
            } else {
                match p[pi] {
                    '*' => {
                        star = Some((pi + 1, ni));
                        pi += 1;
                        continue;
                    }
                    '?' => Some((1, 1)),
                    '[' => match p[pi..].iter().position(|c| *c == ']') {
                        Some(end) => {
                            class_matches(&p[pi + 1..pi + end], n[ni]).then_some((end + 1, 1))
                        }
                        // Unterminated class: `[` is a literal.
                        None => (p[pi] == n[ni]).then_some((1, 1)),
                    },
                    c => (c == n[ni]).then_some((1, 1)),
                }
            };
            match (step, star) {
                (Some((dp, dn)), _) => {
                    pi += dp;
                    ni += dn;
                }
                (None, Some((sp, sn))) => {
                    pi = sp;
                    ni = sn + 1;
                    star = Some((sp, sn + 1));
                }
                (None, None) => return false,
            }
        }
        // Net exhausted: the rest of the pattern must be all `*`.
        p[pi..].iter().all(|c| *c == '*')
    }
    fn class_matches(class: &[char], c: char) -> bool {
        let mut i = 0;
        while i < class.len() {
            if i + 2 < class.len() && class[i + 1] == '-' {
                if class[i] <= c && c <= class[i + 2] {
                    return true;
                }
                i += 3;
            } else {
                if class[i] == c {
                    return true;
                }
                i += 1;
            }
        }
        false
    }
    inner(
        &pattern.chars().collect::<Vec<_>>(),
        &net.chars().collect::<Vec<_>>(),
    )
}

/// Whether a finding is a true short or only a clearance violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViolationKind {
    /// Copper from two nets physically overlaps (gap <= 0): an electrical short.
    Short,
    /// Copper from two nets is closer than the design clearance but not
    /// touching: a near-short manufacturing risk.
    Clearance,
}

impl ViolationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ViolationKind::Short => "short",
            ViolationKind::Clearance => "clearance",
        }
    }
}

/// What kind of copper primitive was involved in a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemKind {
    Track,
    Arc,
    Via,
    Pad,
    Zone,
    /// Directly drawn copper (an Eagle `<rectangle>` / `<circle>` on a copper
    /// layer): exact copper like a track, NOT a pour fill. Kept distinct from
    /// [`ItemKind::Zone`] because the sweep's Zone-Pad overlap suppression
    /// (the KiCad antipad-carve rule) must not swallow a pad sitting on
    /// drawn copper.
    Graphic,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Track => "track",
            ItemKind::Arc => "arc",
            ItemKind::Via => "via",
            ItemKind::Pad => "pad",
            ItemKind::Zone => "zone",
            ItemKind::Graphic => "graphic",
        }
    }
}

/// One copper primitive involved in a finding (the human-facing description).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub kind: ItemKind,
    /// Net id this copper belongs to.
    pub net: i64,
    /// Owning component reference for pads (e.g. "U3"), empty otherwise.
    pub owner: String,
}

/// One short / clearance finding between two different nets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrcFinding {
    pub kind: ViolationKind,
    /// The two nets involved (ids), lower id first for stable ordering.
    pub net_a: i64,
    pub net_b: i64,
    /// Net names, matching `net_a` / `net_b`.
    pub net_a_name: String,
    pub net_b_name: String,
    /// Copper layer the violation is on (e.g. "F.Cu").
    pub layer: String,
    /// Location (mm): the point of closest approach between the two
    /// primitives, i.e. the midpoint of the shortest copper edge-to-edge span
    /// (the contact point when the copper overlaps; a point of the contained
    /// copper for full containment).
    pub x: f64,
    pub y: f64,
    /// Signed copper-edge gap (mm). <= 0 for an overlap, the penetration depth
    /// as a negative number; positive for a clearance violation.
    pub gap_mm: f64,
    /// Effective clearance rule for this net pair (mm).
    pub required_clearance_mm: f64,
    /// The two primitives that came closest.
    pub item_a: Item,
    pub item_b: Item,
}

/// The full DRC report for a board.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DrcReport {
    /// Clearance rule used (mm).
    pub clearance_mm: f64,
    /// Every finding, shorts and clearance violations together.
    pub findings: Vec<DrcFinding>,
    /// Number of copper primitives indexed (diagnostics / perf reporting).
    pub primitive_count: usize,
    /// Set when the board's `.kicad_pcb` format version is newer than the
    /// newest one with exact KiCad DRC parity. KiCad 10 name-only nets and
    /// keyhole antipads are handled, but remaining finding parity is still
    /// unvalidated; surfaces print this caveat and CI gates do not fail on those
    /// results. `None` on a validated version (no behaviour change).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_warning: Option<String>,
}

/// The newest `.kicad_pcb` format version hauksbee's copper extraction is
/// validated against for exact finding parity. KiCad 10 is `20260206`; its
/// name-only nets and keyhole-antipad fill contours are handled and checked
/// against kicad-cli 10.0.5, but the complete finding set does not yet match the
/// native DRC exactly. So `>= 20260000` remains explicitly unvalidated.
pub const FIRST_UNVALIDATED_PCB_VERSION: u32 = 20260000;

/// The `(version N)` format token from a `.kicad_pcb`, if present.
pub fn kicad_pcb_format_version(text: &str) -> Option<u32> {
    let head: String = text.chars().take(512).collect();
    let i = head.find("(version")?;
    head[i + "(version".len()..]
        .trim_start()
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
}

/// A caveat string when the board's format version is newer than hauksbee's
/// validated range, else `None`.
pub fn unvalidated_version_warning(text: &str) -> Option<String> {
    let v = kicad_pcb_format_version(text)?;
    if v < FIRST_UNVALIDATED_PCB_VERSION {
        return None;
    }
    Some(format!(
        "board format {v} is newer than hauksbee's exact KiCad DRC parity range. KiCad 10 \
         name-only nets and keyhole antipads are handled, but remaining findings are UNVALIDATED; \
         cross-check with KiCad 10's own DRC."
    ))
}

impl DrcReport {
    /// Only the true overlaps (electrical shorts).
    pub fn shorts(&self) -> impl Iterator<Item = &DrcFinding> {
        self.findings
            .iter()
            .filter(|f| f.kind == ViolationKind::Short)
    }

    /// Only the clearance (near-short) violations.
    pub fn clearance_violations(&self) -> impl Iterator<Item = &DrcFinding> {
        self.findings
            .iter()
            .filter(|f| f.kind == ViolationKind::Clearance)
    }

    pub fn short_count(&self) -> usize {
        self.shorts().count()
    }

    pub fn is_clean(&self) -> bool {
        self.short_count() == 0
    }

    /// Distinct unordered net pairs that are shorted together, as (id, id) with
    /// the lower id first. The engine uses these to merge nets.
    pub fn shorted_net_pairs(&self) -> Vec<(i64, i64)> {
        let mut pairs: Vec<(i64, i64)> = self
            .shorts()
            .map(|f| (f.net_a.min(f.net_b), f.net_a.max(f.net_b)))
            .collect();
        pairs.sort_unstable();
        pairs.dedup();
        pairs
    }
}

// ── Geometry primitives ──────────────────────────────────────────────────────

/// A capsule: a line segment with a radius (half copper width). Tracks and arc
/// links are capsules; a disc is a degenerate capsule with `a == b`.
#[derive(Debug, Clone, Copy)]
struct Capsule {
    ax: f64,
    ay: f64,
    bx: f64,
    by: f64,
    r: f64,
}

/// A primitive's solid shape, all coordinates in board mm.
#[derive(Debug, Clone)]
enum Shape {
    /// Track / arc-link / disc (via, round pad).
    Capsule(Capsule),
    /// Closed polygon outline (rect/oval/custom pad, zone fill). The points are
    /// the vertices in order; `r` is an extra inflation radius (0 for zones,
    /// the corner radius for a roundrect treated as a polygon + radius).
    Polygon { pts: Vec<(f64, f64)>, r: f64 },
}

/// One indexed copper primitive on a single layer.
#[derive(Debug, Clone)]
struct Primitive {
    shape: Shape,
    net: i64,
    kind: ItemKind,
    owner: String,
    /// Axis-aligned bounds (minx, miny, maxx, maxy), already inflated by the
    /// primitive's own radius so a box-overlap query within `clearance` of two
    /// boxes is a superset of the real close pairs.
    bounds: [f64; 4],
    /// Index back into the per-layer primitive vector (set after build).
    idx: usize,
}

impl Primitive {
    fn item(&self) -> Item {
        Item {
            kind: self.kind,
            net: self.net,
            owner: self.owner.clone(),
        }
    }
}

/// An R*-tree leaf: a primitive's bounding box plus its index. We keep the
/// heavy shape data out of the tree and look it up by index, so the tree nodes
/// stay small.
#[derive(Debug, Clone)]
struct Leaf {
    bounds: [f64; 4],
    idx: usize,
}

impl RTreeObject for Leaf {
    type Envelope = AABB<[f64; 2]>;
    fn envelope(&self) -> Self::Envelope {
        AABB::from_corners(
            [self.bounds[0], self.bounds[1]],
            [self.bounds[2], self.bounds[3]],
        )
    }
}

// ── Distance helpers (all in mm) ─────────────────────────────────────────────

/// Closest point on segment AB to point P, with the squared distance.
fn point_seg_closest(px: f64, py: f64, ax: f64, ay: f64, bx: f64, by: f64) -> ((f64, f64), f64) {
    let dx = bx - ax;
    let dy = by - ay;
    let len2 = dx * dx + dy * dy;
    if len2 <= f64::EPSILON {
        let ex = px - ax;
        let ey = py - ay;
        return ((ax, ay), ex * ex + ey * ey);
    }
    let t = (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0);
    let cx = ax + t * dx;
    let cy = ay + t * dy;
    let ex = px - cx;
    let ey = py - cy;
    ((cx, cy), ex * ex + ey * ey)
}

/// Minimum distance between two segments (centerlines) plus the closest point
/// pair (on AB, on CD). 0 with the crossing point (both) when they cross.
fn seg_seg_closest(
    a1: (f64, f64),
    a2: (f64, f64),
    b1: (f64, f64),
    b2: (f64, f64),
) -> (f64, (f64, f64), (f64, f64)) {
    if segments_intersect(a1, a2, b1, b2) {
        let d1x = a2.0 - a1.0;
        let d1y = a2.1 - a1.1;
        let d2x = b2.0 - b1.0;
        let d2y = b2.1 - b1.1;
        let denom = d1x * d2y - d1y * d2x;
        if denom.abs() > 1e-12 {
            let t = (((b1.0 - a1.0) * d2y - (b1.1 - a1.1) * d2x) / denom).clamp(0.0, 1.0);
            let p = (a1.0 + t * d1x, a1.1 + t * d1y);
            return (0.0, p, p);
        }
        // Colinear touch: the endpoint candidates below find a zero-distance
        // pair.
    }
    let mut best = (f64::INFINITY, a1, b1);
    let (p, d2) = point_seg_closest(b1.0, b1.1, a1.0, a1.1, a2.0, a2.1);
    if d2 < best.0 {
        best = (d2, p, b1);
    }
    let (p, d2) = point_seg_closest(b2.0, b2.1, a1.0, a1.1, a2.0, a2.1);
    if d2 < best.0 {
        best = (d2, p, b2);
    }
    let (p, d2) = point_seg_closest(a1.0, a1.1, b1.0, b1.1, b2.0, b2.1);
    if d2 < best.0 {
        best = (d2, a1, p);
    }
    let (p, d2) = point_seg_closest(a2.0, a2.1, b1.0, b1.1, b2.0, b2.1);
    if d2 < best.0 {
        best = (d2, a2, p);
    }
    (best.0.sqrt(), best.1, best.2)
}

/// Orientation sign of the triplet (p, q, r): >0 ccw, <0 cw, 0 colinear.
fn orient(p: (f64, f64), q: (f64, f64), r: (f64, f64)) -> f64 {
    (q.0 - p.0) * (r.1 - p.1) - (q.1 - p.1) * (r.0 - p.0)
}

fn on_seg(p: (f64, f64), q: (f64, f64), r: (f64, f64)) -> bool {
    q.0 <= p.0.max(r.0) && q.0 >= p.0.min(r.0) && q.1 <= p.1.max(r.1) && q.1 >= p.1.min(r.1)
}

/// Proper / improper segment intersection test.
fn segments_intersect(p1: (f64, f64), p2: (f64, f64), p3: (f64, f64), p4: (f64, f64)) -> bool {
    let d1 = orient(p3, p4, p1);
    let d2 = orient(p3, p4, p2);
    let d3 = orient(p1, p2, p3);
    let d4 = orient(p1, p2, p4);
    if ((d1 > 0.0) != (d2 > 0.0)) && ((d3 > 0.0) != (d4 > 0.0)) {
        return true;
    }
    (d1 == 0.0 && on_seg(p3, p1, p4))
        || (d2 == 0.0 && on_seg(p3, p2, p4))
        || (d3 == 0.0 && on_seg(p1, p3, p2))
        || (d4 == 0.0 && on_seg(p1, p4, p2))
}

/// Even-odd point-in-polygon test.
fn point_in_polygon(px: f64, py: f64, poly: &[(f64, f64)]) -> bool {
    let n = poly.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        if ((yi > py) != (yj > py)) && (px < (xj - xi) * (py - yi) / (yj - yi) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Minimum boundary-to-boundary distance between two polygons (0 if their
/// edges cross) plus the closest point pair (on `a`, on `b`). Containment is
/// handled by the caller via point-in-polygon.
fn poly_poly_closest(a: &[(f64, f64)], b: &[(f64, f64)]) -> (f64, (f64, f64), (f64, f64)) {
    let na = a.len();
    let nb = b.len();
    let mut best = (f64::INFINITY, (0.0, 0.0), (0.0, 0.0));
    if na == 0 || nb == 0 {
        return best;
    }
    if na < 2 || nb < 2 {
        // Degenerate: fall back to nearest a-vertex against b's boundary.
        for &pa in a {
            let mut jb = nb - 1;
            for ib in 0..nb {
                let (q, d2) = point_seg_closest(pa.0, pa.1, b[jb].0, b[jb].1, b[ib].0, b[ib].1);
                if d2.sqrt() < best.0 {
                    best = (d2.sqrt(), pa, q);
                }
                jb = ib;
            }
        }
        return best;
    }
    let mut ja = na - 1;
    for ia in 0..na {
        let a1 = a[ja];
        let a2 = a[ia];
        let mut jb = nb - 1;
        for ib in 0..nb {
            let cand = seg_seg_closest(a1, a2, b[jb], b[ib]);
            if cand.0 < best.0 {
                best = cand;
            }
            jb = ib;
        }
        ja = ia;
    }
    best
}

/// The reported violation location for a closest centerline/boundary point
/// pair `pa`/`pb` carrying copper radii `ra`/`rb`: the midpoint of the copper
/// edge-to-edge span along the closest-approach line. For a positive gap that
/// is the middle of the air gap; for an overlap it lands inside the shared
/// copper (the contact point); clamped between `pa` and `pb` so deep
/// penetrations stay on the copper.
fn closest_approach_point(pa: (f64, f64), pb: (f64, f64), ra: f64, rb: f64) -> (f64, f64) {
    let dx = pb.0 - pa.0;
    let dy = pb.1 - pa.1;
    let d = (dx * dx + dy * dy).sqrt();
    if d <= f64::EPSILON {
        return pa;
    }
    let t = ((ra + d - rb) / (2.0 * d)).clamp(0.0, 1.0);
    (pa.0 + dx * t, pa.1 + dy * t)
}

/// Signed copper-edge gap between two primitives. Negative means they overlap
/// (the magnitude is roughly the penetration), and the returned `(x, y)` is
/// the point of closest approach (see [`closest_approach_point`]; for full
/// containment, a point of the contained copper).
fn shape_gap(a: &Shape, b: &Shape) -> (f64, (f64, f64)) {
    match (a, b) {
        (Shape::Capsule(ca), Shape::Capsule(cb)) => {
            let (d, pa, pb) = seg_seg_closest(
                (ca.ax, ca.ay),
                (ca.bx, ca.by),
                (cb.ax, cb.ay),
                (cb.bx, cb.by),
            );
            (d - ca.r - cb.r, closest_approach_point(pa, pb, ca.r, cb.r))
        }
        (Shape::Capsule(c), Shape::Polygon { pts, r })
        | (Shape::Polygon { pts, r }, Shape::Capsule(c)) => {
            // True centerline-to-boundary distance: the capsule segment against
            // every polygon edge (0 if it crosses the boundary). This catches a
            // track passing straight through a pad even when neither endpoint is
            // inside and no vertex is near.
            let seg_a = (c.ax, c.ay);
            let seg_b = (c.bx, c.by);
            // Containment: either capsule endpoint inside the polygon (the
            // track terminates within the pad / pour copper). Fully engulfed:
            // a hard overlap regardless of edge distance, located at the
            // contained endpoint.
            if point_in_polygon(c.ax, c.ay, pts) {
                return (-(c.r + r).max(0.0) - 1e-6, seg_a);
            }
            if point_in_polygon(c.bx, c.by, pts) {
                return (-(c.r + r).max(0.0) - 1e-6, seg_b);
            }
            let mut best = (f64::INFINITY, seg_a, seg_a);
            let n = pts.len();
            if n >= 2 {
                let mut j = n - 1;
                for i in 0..n {
                    let cand = seg_seg_closest(seg_a, seg_b, pts[j], pts[i]);
                    if cand.0 < best.0 {
                        best = cand;
                    }
                    j = i;
                }
            } else if n == 1 {
                let (p, d2) = point_seg_closest(pts[0].0, pts[0].1, c.ax, c.ay, c.bx, c.by);
                best = (d2.sqrt(), p, pts[0]);
            }
            let (d, pc, pp) = best;
            (d - c.r - r, closest_approach_point(pc, pp, c.r, *r))
        }
        (Shape::Polygon { pts: pa, r: ra }, Shape::Polygon { pts: pb, r: rb }) => {
            let (d, qa, qb) = poly_poly_closest(pa, pb);
            let edge = d - ra - rb;
            // Containment either way is a hard overlap, located at a vertex of
            // the contained outline.
            if pa.first().is_some_and(|&(x, y)| point_in_polygon(x, y, pb)) {
                return (edge.min(0.0) - 1e-6, pa[0]);
            }
            if pb.first().is_some_and(|&(x, y)| point_in_polygon(x, y, pa)) {
                return (edge.min(0.0) - 1e-6, pb[0]);
            }
            (edge, closest_approach_point(qa, qb, *ra, *rb))
        }
    }
}

// ── Net resolution (mirrors pcb.rs, but standalone) ──────────────────────────

/// A net id table that handles all three KiCad encodings the same way `pcb.rs`
/// does: declared `(net N "name")`, name-only `(net "name")`, and the older
/// `(net N)` + separate `(net_name "name")` on zones.
#[derive(Default)]
struct NetResolver {
    by_id: HashMap<i64, String>,
    by_name: HashMap<String, i64>,
    next_synthetic: i64,
}

impl NetResolver {
    fn from_root(root: &List) -> Self {
        let mut r = NetResolver::default();
        for n in root.find_all("net") {
            match (n.arg_i64(0), n.arg_value(0), n.arg_value(1)) {
                (Some(id), _, name) => r.declare(id, name.unwrap_or_default()),
                (None, Some(name), _) => {
                    r.id_of(&name);
                }
                _ => {}
            }
        }
        r
    }

    fn declare(&mut self, id: i64, name: String) {
        // Same normalization as pcb.rs's NetTable: file-syntax escapes end
        // here, so DRC findings name nets the way the schematic shows them.
        let name = crate::netname::unescape_net_name(&name);
        self.by_name.entry(name.clone()).or_insert(id);
        self.by_id.entry(id).or_insert(name);
        self.next_synthetic = self.next_synthetic.max(id + 1);
    }

    fn id_of(&mut self, name: &str) -> i64 {
        let name = crate::netname::unescape_net_name(name);
        if let Some(&id) = self.by_name.get(&name) {
            return id;
        }
        let id = self.next_synthetic.max(1);
        self.next_synthetic = id + 1;
        self.declare(id, name);
        id
    }

    fn name_of(&self, id: i64) -> String {
        self.by_id.get(&id).cloned().unwrap_or_default()
    }

    /// True for nets that carry no real connectivity: KiCad's auto-generated
    /// `unconnected-(...)` placeholders (one per floating pad) and the empty
    /// net 0. Copper on these cannot form an electrical short, so the sweep
    /// skips them the same way it skips net 0.
    fn is_no_net(&self, id: i64) -> bool {
        id == 0 || self.name_of(id).starts_with("unconnected-")
    }

    /// Resolve a `(net ...)` child of a list to an id, handling the numeric,
    /// name-only, and numeric-with-sibling-`net_name` forms.
    fn net_ref(&mut self, list: &List) -> Option<i64> {
        let net = list.find("net")?;
        // Numeric id form.
        if let Some(id) = net
            .arg(0)
            .filter(|t| !t.is_string())
            .and_then(|t| t.as_i64())
        {
            // Some zones carry both `(net N)` and `(net_name "X")`; declare the
            // name so reporting has it.
            if let Some(name) = list.find_value("net_name") {
                self.declare(id, name);
            }
            return Some(id);
        }
        // Name-only form.
        let name = net.arg_value(0)?;
        Some(self.id_of(&name))
    }
}

// ── Extraction ───────────────────────────────────────────────────────────────

/// A whole filled-zone polygon kept aside for containment tests (a different-net
/// primitive sitting fully inside the pour is a short even if it never crosses
/// the boundary). The boundary itself is indexed as edge capsules so the R-tree
/// prunes the distance sweep; this side-table only serves point-in-polygon.
#[derive(Debug, Clone)]
struct ZonePoly {
    pts: Vec<(f64, f64)>,
    net: i64,
    bounds: [f64; 4],
    /// True when this came from a real `filled_polygon` (the actual copper, with
    /// antipads / thermal reliefs). Outline-only zones (no computed fill, common
    /// in pre-2017 boards) set this false: their drawn boundary is kept for
    /// clearance checks, but the containment short-test is skipped because the
    /// solid outline would falsely engulf every pad of other nets.
    filled: bool,
}

/// Per-layer accumulator of copper primitives.
#[derive(Default)]
struct LayerBuckets {
    /// layer name -> primitives on that layer (indexed for the distance sweep).
    by_layer: HashMap<String, Vec<Primitive>>,
    /// layer name -> full zone polygons (for containment only).
    zones: HashMap<String, Vec<ZonePoly>>,
}

impl LayerBuckets {
    fn push(&mut self, layer: &str, prim: Primitive) {
        self.by_layer
            .entry(layer.to_string())
            .or_default()
            .push(prim);
    }

    /// Add a zone polygon: its boundary edges become indexed capsules (radius 0)
    /// so distance/clearance to it is pruned by the R-tree, and the whole
    /// polygon is stashed for the containment pass. `filled` marks whether this
    /// is real fill copper (eligible for the containment short-test) or only a
    /// drawn outline.
    fn push_zone(&mut self, layer: &str, pts: Vec<(f64, f64)>, net: i64, filled: bool) {
        self.push_zone_opts(layer, pts, net, filled, true)
    }

    /// As [`Self::push_zone`], but `edges` controls whether the boundary edges
    /// are pushed as indexed capsules. A pour whose true fill (with antipads /
    /// thermal reliefs) is not modelled cannot be short-tested against same-layer
    /// foreign copper without manufacturing false shorts where a via legitimately
    /// passes through an antipad void; for such pours pass `edges = false` so the
    /// outline contributes nothing to the sweep. This is the Altium split-plane
    /// case (the real fill lives in `Regions6`, which the extractor does not
    /// parse), and is the principled analogue of the Eagle `filled = false` rule.
    fn push_zone_opts(
        &mut self,
        layer: &str,
        pts: Vec<(f64, f64)>,
        net: i64,
        filled: bool,
        edges: bool,
    ) {
        if pts.len() < 3 {
            return;
        }
        if edges {
            let n = pts.len();
            let mut j = n - 1;
            for i in 0..n {
                let cap = Capsule {
                    ax: pts[j].0,
                    ay: pts[j].1,
                    bx: pts[i].0,
                    by: pts[i].1,
                    r: 0.0,
                };
                self.push(
                    layer,
                    make_prim(Shape::Capsule(cap), net, ItemKind::Zone, String::new()),
                );
                j = i;
            }
        }
        let bounds = polygon_bounds(&pts);
        self.zones
            .entry(layer.to_string())
            .or_default()
            .push(ZonePoly {
                pts,
                net,
                bounds,
                filled,
            });
    }
}

/// Bounding box (minx, miny, maxx, maxy) of a point list.
fn polygon_bounds(pts: &[(f64, f64)]) -> [f64; 4] {
    let mut b = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for &(x, y) in pts {
        b[0] = b[0].min(x);
        b[1] = b[1].min(y);
        b[2] = b[2].max(x);
        b[3] = b[3].max(y);
    }
    b
}

/// Read `(start x y)` / `(end x y)` style coordinate children.
fn xy_pair(list: &List, name: &str) -> Option<(f64, f64)> {
    let l = list.find(name)?;
    Some((l.arg_f64(0)?, l.arg_f64(1)?))
}

/// Inflated bounds of a shape: (minx, miny, maxx, maxy).
fn shape_bounds(shape: &Shape) -> [f64; 4] {
    match shape {
        Shape::Capsule(c) => {
            let minx = c.ax.min(c.bx) - c.r;
            let miny = c.ay.min(c.by) - c.r;
            let maxx = c.ax.max(c.bx) + c.r;
            let maxy = c.ay.max(c.by) + c.r;
            [minx, miny, maxx, maxy]
        }
        Shape::Polygon { pts, r } => {
            let mut minx = f64::INFINITY;
            let mut miny = f64::INFINITY;
            let mut maxx = f64::NEG_INFINITY;
            let mut maxy = f64::NEG_INFINITY;
            for &(x, y) in pts {
                minx = minx.min(x);
                miny = miny.min(y);
                maxx = maxx.max(x);
                maxy = maxy.max(y);
            }
            [minx - r, miny - r, maxx + r, maxy + r]
        }
    }
}

/// A representative interior point of a shape (its centroid-ish point), used
/// for the zone containment test.
fn representative_point(shape: &Shape) -> (f64, f64) {
    match shape {
        Shape::Capsule(c) => ((c.ax + c.bx) / 2.0, (c.ay + c.by) / 2.0),
        Shape::Polygon { pts, .. } => {
            let n = pts.len().max(1) as f64;
            let sx: f64 = pts.iter().map(|p| p.0).sum();
            let sy: f64 = pts.iter().map(|p| p.1).sum();
            (sx / n, sy / n)
        }
    }
}

fn make_prim(shape: Shape, net: i64, kind: ItemKind, owner: String) -> Primitive {
    let bounds = shape_bounds(&shape);
    Primitive {
        shape,
        net,
        kind,
        owner,
        bounds,
        idx: 0,
    }
}

/// Expand a `*.Cu` / `F&B.Cu` layer token to the concrete copper layers it
/// occupies, given the set of copper layers the board declares.
fn expand_layers(token: &str, copper_layers: &[String]) -> Vec<String> {
    if token == "*.Cu" || token.eq_ignore_ascii_case("F&B.Cu") {
        copper_layers.to_vec()
    } else if token.ends_with(".Cu") {
        vec![token.to_string()]
    } else {
        Vec::new()
    }
}

/// Expand a via's named end layers to the inclusive span of copper layers its
/// barrel passes through, using the board's declared (stackup-ordered) copper
/// list. `["F.Cu", "B.Cu"]` on a 4-layer board becomes F/In1/In2/B; a buried
/// `["In1.Cu", "In2.Cu"]` fills its inner span too. Names not present in the
/// declared stack keep the list verbatim (nothing to interpolate against).
fn via_layer_span(names: &[String], copper_layers: &[String]) -> Vec<String> {
    let idx: Vec<usize> = names
        .iter()
        .filter_map(|n| copper_layers.iter().position(|c| c == n))
        .collect();
    match (idx.iter().min(), idx.iter().max()) {
        (Some(&lo), Some(&hi)) => copper_layers[lo..=hi].to_vec(),
        _ => names.to_vec(),
    }
}

/// Collect the copper layer names the board declares (from `(layers ...)`),
/// falling back to the canonical two-layer stack.
fn copper_layers_of(root: &List) -> Vec<String> {
    let mut layers = Vec::new();
    if let Some(decl) = root.find("layers") {
        for l in decl.lists() {
            // `(0 "F.Cu" signal)`: the name is arg 0 (a string).
            if let Some(name) = l.arg_value(0) {
                if name.ends_with(".Cu") {
                    layers.push(name);
                }
            }
            // `(layer "F.Cu" ...)` style is also possible; name in arg 0 too.
        }
    }
    if layers.is_empty() {
        layers = vec!["F.Cu".to_string(), "B.Cu".to_string()];
    }
    layers
}

/// Build the per-layer copper primitives for a parsed board.
fn collect_primitives(root: &List, nets: &mut NetResolver) -> LayerBuckets {
    let mut buckets = LayerBuckets::default();
    let copper_layers = copper_layers_of(root);

    // ── Track segments ──────────────────────────────────────────────────────
    for seg in root.find_all("segment") {
        let (Some(start), Some(end)) = (xy_pair(seg, "start"), xy_pair(seg, "end")) else {
            continue;
        };
        let width = seg.find_f64("width").unwrap_or(0.0);
        let layer = seg.find_value("layer").unwrap_or_default();
        if !layer.ends_with(".Cu") {
            continue;
        }
        let Some(net) = nets.net_ref(seg) else {
            continue;
        };
        let cap = Capsule {
            ax: start.0,
            ay: start.1,
            bx: end.0,
            by: end.1,
            r: width / 2.0,
        };
        buckets.push(
            &layer,
            make_prim(Shape::Capsule(cap), net, ItemKind::Track, String::new()),
        );
    }

    // ── Arc tracks (KiCad 7+ `(arc (start)(mid)(end)(width)(layer)(net))`) ───
    for arc in root.find_all("arc") {
        let (Some(start), Some(end)) = (xy_pair(arc, "start"), xy_pair(arc, "end")) else {
            continue;
        };
        let mid = xy_pair(arc, "mid");
        let width = arc.find_f64("width").unwrap_or(0.0);
        let layer = arc.find_value("layer").unwrap_or_default();
        if !layer.ends_with(".Cu") {
            continue;
        }
        let Some(net) = nets.net_ref(arc) else {
            continue;
        };
        for cap in flatten_arc(start, mid, end, width) {
            buckets.push(
                &layer,
                make_prim(Shape::Capsule(cap), net, ItemKind::Arc, String::new()),
            );
        }
    }

    // ── Vias (multi-layer discs) ────────────────────────────────────────────
    for via in root.find_all("via") {
        let Some(at) = xy_pair(via, "at") else {
            continue;
        };
        let size = via.find_f64("size").unwrap_or(0.0);
        let Some(net) = nets.net_ref(via) else {
            continue;
        };
        let layer_token = via
            .find("layers")
            .and_then(|l| {
                // `(layers "F.Cu" "B.Cu")` names only the via's END layers; the
                // barrel physically passes through every copper layer between
                // them. Expand the named span across the board's ordered
                // copper stack so inner-layer copper (In1.Cu, ...) is tested
                // too, mirroring how pads reach all layers via expand_layers.
                let names: Vec<String> = (0..)
                    .map_while(|i| l.arg_value(i))
                    .filter(|n| n.ends_with(".Cu"))
                    .collect();
                if names.is_empty() {
                    None
                } else {
                    Some(via_layer_span(&names, &copper_layers))
                }
            })
            .unwrap_or_else(|| copper_layers.clone());
        let disc = Capsule {
            ax: at.0,
            ay: at.1,
            bx: at.0,
            by: at.1,
            r: size / 2.0,
        };
        for layer in &layer_token {
            buckets.push(
                layer,
                make_prim(Shape::Capsule(disc), net, ItemKind::Via, String::new()),
            );
        }
    }

    // ── Filled zones (copper pours) ─────────────────────────────────────────
    for zone in root.find_all("zone") {
        let Some(net) = nets.net_ref(zone) else {
            continue;
        };
        // A zone can fill several layers; each `(filled_polygon (layer ...))`
        // is the actual copper. Fall back to the zone's own `(layer ...)` and
        // its drawn `(polygon ...)` outline when no fill is present.
        let mut any_fill = false;
        for fp in zone.find_all("filled_polygon") {
            let layer = fp
                .find_value("layer")
                .or_else(|| zone.find_value("layer"))
                .unwrap_or_default();
            if !layer.ends_with(".Cu") {
                continue;
            }
            if let Some(pts) = read_pts(fp) {
                if pts.len() >= 3 {
                    any_fill = true;
                    buckets.push_zone(&layer, pts, net, true);
                }
            }
        }
        if !any_fill {
            // No fill computed in the file (common pre-2017): keep the drawn
            // outline for clearance checks but mark it unfilled so the
            // containment short-test skips it (the solid outline would falsely
            // engulf every other-net pad inside the pour, since antipads and
            // thermal reliefs are not represented).
            //
            // A zone may span SEVERAL layers via `(layers "F.Cu" "B.Cu")` (or a
            // `*.Cu` wildcard); reading only the single `(layer ...)` dropped the
            // outline of every multi-layer unfilled zone. Expand the declared
            // layer set and keep the outline on each copper layer it occupies.
            if let Some(poly) = zone.find("polygon").and_then(read_pts) {
                let mut layers: Vec<String> = Vec::new();
                if let Some(decl) = zone.find("layers") {
                    for t in (0..).map_while(|i| decl.arg_value(i)) {
                        layers.extend(expand_layers(&t, &copper_layers));
                    }
                }
                if layers.is_empty() {
                    if let Some(single) = zone.find_value("layer") {
                        layers.extend(expand_layers(&single, &copper_layers));
                    }
                }
                layers.sort();
                layers.dedup();
                for layer in layers {
                    buckets.push_zone(&layer, poly.clone(), net, false);
                }
            }
        }
    }

    // ── Pads (inside footprints) ────────────────────────────────────────────
    for fp in root.find_all("footprint").chain(root.find_all("module")) {
        let owner = footprint_reference(fp);
        let (fx, fy, frot) = at_of(fp);
        let rot_rad = frot.to_radians();
        let (fsin, fcos) = rot_rad.sin_cos();
        for pad in fp.find_all("pad") {
            let Some(net) = nets.net_ref(pad) else {
                continue;
            };
            collect_pad(
                pad,
                net,
                &owner,
                (fx, fy),
                (fsin, fcos),
                &copper_layers,
                &mut buckets,
            );
        }
    }

    buckets
}

/// One pad → a primitive on each copper layer it touches.
fn collect_pad(
    pad: &List,
    net: i64,
    owner: &str,
    forigin: (f64, f64),
    frot: (f64, f64),
    copper_layers: &[String],
    buckets: &mut LayerBuckets,
) {
    let (fsin, fcos) = frot;
    let (fx, fy) = forigin;
    // Pad-local placement.
    let (px, py, prot) = at_of(pad);
    // Pad shape token is the 3rd positional arg: (pad "1" smd roundrect ...).
    let shape_tok = pad.arg_value(2).unwrap_or_default();
    let size = pad.find("size");
    let (sx, sy) = match size {
        Some(s) => (s.arg_f64(0).unwrap_or(0.0), s.arg_f64(1).unwrap_or(0.0)),
        None => (0.0, 0.0),
    };

    // World transform for a pad-frame offset (KiCad y-down, footprint rotation
    // counter-clockwise: matches pcb.rs's pad placement).
    let to_world = |ox: f64, oy: f64| -> (f64, f64) {
        (
            fx + px * fcos + py * fsin + ox,
            fy - px * fsin + py * fcos + oy,
        )
    };
    let pad_origin = to_world(0.0, 0.0);

    // A through-hole pad can carry `(drill ... (offset x y))`: the pad's `(at)`
    // is the HOLE position and the copper shape is displaced by the offset,
    // rotated with the pad. Castellated / edge-solder module footprints (e.g.
    // the OpenMower xESC2-mini) use this to hang half the copper past the hole;
    // ignoring it draws the copper up to half a pad away from where KiCad
    // filled the surrounding zone, which manufactured 114 phantom clearance
    // warnings on a board KiCad's own DRC scores clean.
    let (offx, offy) = pad
        .find("drill")
        .and_then(|d| d.find("offset"))
        .and_then(|o| Some((o.arg_f64(0)?, o.arg_f64(1)?)))
        .unwrap_or((0.0, 0.0));

    // Rotate a pad-local outline offset into the world frame. KiCad writes the
    // pad's `(at x y rot)` rotation as the pad outline's *absolute* board-frame
    // orientation (the footprint rotation is already folded into it), so the
    // outline is rotated by `prot` alone (NOT composed with the footprint
    // rotation again, which the position transform already applied). The y-down
    // form matches `to_world`: (lx cos + ly sin, -lx sin + ly cos). The drill
    // offset is added in the pad-local frame so it rotates with the outline.
    let (psin, pcos) = prot.to_radians().sin_cos();
    let outline_to_world = |lx: f64, ly: f64| -> (f64, f64) {
        let lx = lx + offx;
        let ly = ly + offy;
        let wx = lx * pcos + ly * psin;
        let wy = -lx * psin + ly * pcos;
        (pad_origin.0 + wx, pad_origin.1 + wy)
    };

    // Layers this pad sits on. No `(layers ...)` list at all → assume every
    // copper layer (through-hole style). A list that names only non-copper
    // layers (e.g. `(layers "F.Mask")`) means the pad carries NO copper: it
    // must stay off the copper buckets entirely, not fall back to all of them.
    let layers: Vec<String> = match pad.find("layers") {
        Some(l) => {
            let mut out = Vec::new();
            for t in (0..).map_while(|i| l.arg_value(i)) {
                out.extend(expand_layers(&t, copper_layers));
            }
            out
        }
        None => copper_layers.to_vec(),
    };

    let shapes: Vec<Shape> = match shape_tok.as_str() {
        "circle" => vec![{
            let r = sx.max(sy) / 2.0;
            // Through outline_to_world so a drill offset displaces the disc too.
            let c = outline_to_world(0.0, 0.0);
            Shape::Capsule(Capsule {
                ax: c.0,
                ay: c.1,
                bx: c.0,
                by: c.1,
                r,
            })
        }],
        "oval" => vec![{
            // A stadium: a capsule whose segment runs along the longer axis,
            // radius = half the shorter dimension.
            let (long, short, along_x) = if sx >= sy {
                (sx, sy, true)
            } else {
                (sy, sx, false)
            };
            let half = (long - short).max(0.0) / 2.0;
            let (a, b) = if along_x {
                (outline_to_world(-half, 0.0), outline_to_world(half, 0.0))
            } else {
                (outline_to_world(0.0, -half), outline_to_world(0.0, half))
            };
            Shape::Capsule(Capsule {
                ax: a.0,
                ay: a.1,
                bx: b.0,
                by: b.1,
                r: short / 2.0,
            })
        }],
        // Trapezoid: `(rect_delta dx dy)` skews the rectangle. KiCad's
        // outline (legacy `PAD::BuildPadPolygon`): `size` is the average of
        // the two parallel edges and the delta is the full difference, so the
        // wide edge extends BEYOND the size box, so a bounding rectangle both
        // understates the wide edge and overstates the narrow one.
        "trapezoid" => vec![trapezoid_polygon(pad, sx, sy, &outline_to_world)],
        // Custom pad: the copper is the anchor pad shape UNION every drawn
        // primitive. Stamping only the first polygon (the old behaviour)
        // dropped the anchor disc and every further primitive, silently
        // un-checking real copper.
        "custom" => custom_pad_shapes(pad, sx, sy, &outline_to_world),
        // rect, roundrect and anything else: a rectangle. For roundrect we
        // keep a corner radius as inflation so the rounded copper is not
        // overstated.
        _ => vec![{
            let min_side = sx.min(sy);
            let rr = if shape_tok == "roundrect" {
                let ratio = pad.find_f64("roundrect_rratio").unwrap_or(0.0);
                ratio * min_side
            } else {
                0.0
            };
            // KiCad stores chamfered pads as roundrect with `(chamfer_ratio N)`
            // + `(chamfer <corners...>)`: each named corner gets a straight
            // 45-degree cut of size ratio * min(w, h), while the un-named
            // corners keep the roundrect radius. Treating such a pad as a full
            // rectangle manufactures false shorts against copper legitimately
            // routed through the notch (the Kailh-socket pads on PolyKybd
            // slice 0.77 mm off both bottom corners and a 45-degree track
            // passes through; KiCad 9 DRC reports no short there).
            let chamfered = chamfered_corners(pad);
            let ch = if chamfered.iter().any(|&c| c) {
                (pad.find_f64("chamfer_ratio").unwrap_or(0.0) * min_side).max(0.0)
            } else {
                0.0
            };
            if ch > 0.0 {
                chamfered_rect_polygon(sx, sy, rr, ch, chamfered, &outline_to_world)
            } else {
                // Inset the rectangle by the corner radius and carry `r` so the
                // rounded outline is represented as inset-poly + radius.
                rect_polygon(
                    (sx - 2.0 * rr).max(0.0),
                    (sy - 2.0 * rr).max(0.0),
                    &outline_to_world,
                    rr,
                )
            }
        }],
    };

    for layer in &layers {
        for shape in &shapes {
            buckets.push(
                layer,
                make_prim(shape.clone(), net, ItemKind::Pad, owner.to_string()),
            );
        }
    }
}

/// A rectangle of size (w, h) centred on the pad origin, built via the world
/// transform, carrying inflation radius `r`.
fn rect_polygon(w: f64, h: f64, to_world: &dyn Fn(f64, f64) -> (f64, f64), r: f64) -> Shape {
    let hw = w / 2.0;
    let hh = h / 2.0;
    let pts = vec![
        to_world(-hw, -hh),
        to_world(hw, -hh),
        to_world(hw, hh),
        to_world(-hw, hh),
    ];
    Shape::Polygon { pts, r }
}

/// Which corners a pad's `(chamfer <corners...>)` list names, in the fixed
/// order [top_left, top_right, bottom_right, bottom_left] (pad-local frame,
/// y-down, matching KiCad's file coordinates: "top" is negative y).
fn chamfered_corners(pad: &List) -> [bool; 4] {
    let mut out = [false; 4];
    if let Some(ch) = pad.find("chamfer") {
        for t in (0..).map_while(|i| ch.arg_value(i)) {
            match t.as_str() {
                "top_left" => out[0] = true,
                "top_right" => out[1] = true,
                "bottom_right" => out[2] = true,
                "bottom_left" => out[3] = true,
                _ => {}
            }
        }
    }
    out
}

/// A chamfered (round)rect pad outline of size (w, h), corner radius `rr` on
/// the non-chamfered corners and a straight 45-degree cut of size `ch` on each
/// corner flagged in `chamfered` ([top_left, top_right, bottom_right,
/// bottom_left], pad-local y-down frame), built via the world transform.
///
/// Same representation strategy as `rect_polygon` for a roundrect: the
/// returned polygon is the outline deflated by `rr`, carried with inflation
/// radius `rr` so the rounded corners are exact. The chamfer cut line is
/// therefore also deflated: its axis intercepts sit (sqrt(2) - 1) * rr inside
/// the true ones, so the inflated cut edge lands exactly on KiCad's. The only
/// approximation is at the cut's two endpoints, which inflate to radius-`rr`
/// arcs instead of sharp vertices, understating the copper there by at most
/// ~0.09 * rr (zero whenever rr is 0, the common chamfered-pad case).
fn chamfered_rect_polygon(
    w: f64,
    h: f64,
    rr: f64,
    ch: f64,
    chamfered: [bool; 4],
    to_world: &dyn Fn(f64, f64) -> (f64, f64),
) -> Shape {
    let hw = w / 2.0;
    let hh = h / 2.0;
    let cut = ch + (std::f64::consts::SQRT_2 - 1.0) * rr;
    // Corner order matches rect_polygon: TL, TR, BR, BL. `(cx, cy)` is the
    // corner's quadrant sign; `vert_first` is whether the outline traversal
    // enters the corner along the vertical pad edge (so the vertex on that
    // edge comes first).
    let corners = [
        (-1.0, -1.0, true),
        (1.0, -1.0, false),
        (1.0, 1.0, true),
        (-1.0, 1.0, false),
    ];
    let mut local: Vec<(f64, f64)> = Vec::with_capacity(8);
    for (i, &(cx, cy, vert_first)) in corners.iter().enumerate() {
        if chamfered[i] {
            let on_vert = (cx * (hw - rr).max(0.0), cy * (hh - cut).max(0.0));
            let on_horz = (cx * (hw - cut).max(0.0), cy * (hh - rr).max(0.0));
            if vert_first {
                local.push(on_vert);
                local.push(on_horz);
            } else {
                local.push(on_horz);
                local.push(on_vert);
            }
        } else {
            local.push((cx * (hw - rr).max(0.0), cy * (hh - rr).max(0.0)));
        }
    }
    // A maximal chamfer (ratio 0.5 on adjacent corners of a square-ish pad)
    // can make consecutive vertices coincide; drop the duplicates.
    let mut pts: Vec<(f64, f64)> = Vec::with_capacity(local.len());
    for &(x, y) in &local {
        if pts
            .last()
            .is_none_or(|&(px, py)| (px - x).abs() > 1e-9 || (py - y).abs() > 1e-9)
        {
            pts.push((x, y));
        }
    }
    if pts.len() > 1 {
        let first = pts[0];
        let last = *pts.last().unwrap();
        if (first.0 - last.0).abs() <= 1e-9 && (first.1 - last.1).abs() <= 1e-9 {
            pts.pop();
        }
    }
    let pts = pts.into_iter().map(|(x, y)| to_world(x, y)).collect();
    Shape::Polygon { pts, r: rr }
}

/// A KiCad trapezoid pad outline. `(rect_delta dx dy)` on a `(size sx sy)`
/// pad gives the four pad-local corners (KiCad's legacy `PAD::BuildPadPolygon`,
/// with `delta = rect_delta / 2`, y-down pad frame):
///
/// ```text
///   (-sx/2 - dy/2,  sy/2 + dx/2)   (-sx/2 + dy/2, -sy/2 - dx/2)
///   ( sx/2 - dy/2, -sy/2 + dx/2)   ( sx/2 + dy/2,  sy/2 - dx/2)
/// ```
///
/// so one parallel edge is `size + delta` long and the other `size - delta`.
/// A maximal delta collapses one edge to a point (a triangle); coincident
/// consecutive vertices are dropped.
fn trapezoid_polygon(
    pad: &List,
    sx: f64,
    sy: f64,
    to_world: &dyn Fn(f64, f64) -> (f64, f64),
) -> Shape {
    let (dx, dy) = pad
        .find("rect_delta")
        .and_then(|d| Some((d.arg_f64(0)?, d.arg_f64(1)?)))
        .unwrap_or((0.0, 0.0));
    let hw = sx / 2.0;
    let hh = sy / 2.0;
    // Clamp the delta the way KiCad does (|dx| <= sy, |dy| <= sx): past that
    // the parallel edge lengths (size ± delta) go negative and the quad turns
    // into a self-intersecting bowtie, whose edge distances are garbage. At
    // the clamp boundary one edge collapses to a point (a triangle), which
    // the vertex dedup below handles.
    let ddx = (dx / 2.0).clamp(-hh, hh);
    let ddy = (dy / 2.0).clamp(-hw, hw);
    let local = [
        (-hw - ddy, hh + ddx),
        (-hw + ddy, -hh - ddx),
        (hw - ddy, -hh + ddx),
        (hw + ddy, hh - ddx),
    ];
    let mut pts: Vec<(f64, f64)> = Vec::with_capacity(4);
    for (x, y) in local {
        if pts
            .last()
            .is_none_or(|&(px, py)| (px - x).abs() > 1e-9 || (py - y).abs() > 1e-9)
        {
            pts.push((x, y));
        }
    }
    if pts.len() > 1 {
        let first = pts[0];
        let last = *pts.last().unwrap();
        if (first.0 - last.0).abs() <= 1e-9 && (first.1 - last.1).abs() <= 1e-9 {
            pts.pop();
        }
    }
    if pts.len() < 3 {
        // A pathological delta (both edges collapsed) leaves no area to
        // stamp; fall back to the size box rather than a degenerate polygon.
        return rect_polygon(sx, sy, to_world, 0.0);
    }
    let pts = pts.into_iter().map(|(x, y)| to_world(x, y)).collect();
    Shape::Polygon { pts, r: 0.0 }
}

/// All solid copper shapes of a KiCad custom pad, transformed to world
/// coordinates: the anchor pad shape (`(options (anchor circle|rect))` over
/// `(size sx sy)`, circle by default) plus every drawn primitive inside
/// `(primitives ...)`: all `gr_poly`/`poly` outlines (not just the first),
/// stroked lines, arcs, circles and rectangles.
fn custom_pad_shapes(
    pad: &List,
    sx: f64,
    sy: f64,
    to_world: &dyn Fn(f64, f64) -> (f64, f64),
) -> Vec<Shape> {
    let mut out = Vec::new();

    // Anchor: the base pad shape the primitives are unioned onto. KiCad only
    // writes `circle` or `rect`; the non-rect branch models the anchor over
    // `size` as a stadium along the longer axis, which is exactly the disc
    // when sx == sy (the circle-anchor case) and never over-claims the
    // circumscribed disc if a non-square size ever appears.
    let anchor = pad
        .find("options")
        .and_then(|o| o.find_value("anchor"))
        .unwrap_or_else(|| "circle".to_string());
    if anchor == "rect" {
        out.push(rect_polygon(sx, sy, to_world, 0.0));
    } else {
        let (long, short, along_x) = if sx >= sy {
            (sx, sy, true)
        } else {
            (sy, sx, false)
        };
        let half = (long - short).max(0.0) / 2.0;
        let (a, b) = if along_x {
            (to_world(-half, 0.0), to_world(half, 0.0))
        } else {
            (to_world(0.0, -half), to_world(0.0, half))
        };
        out.push(Shape::Capsule(Capsule {
            ax: a.0,
            ay: a.1,
            bx: b.0,
            by: b.1,
            r: short / 2.0,
        }));
    }

    let Some(prims) = pad.find("primitives") else {
        return out;
    };
    // KiCad writes an explicit fill token when a circle/rect primitive is
    // filled (`(fill yes)` classically, `(fill solid)`/`(fill none)` in newer
    // formats) and omits it or writes `none` for outline-only strokes, so an
    // absent token means unfilled, exactly as the writer meant it.
    let filled = |l: &List| -> bool {
        matches!(
            l.find_value("fill").as_deref(),
            Some("yes") | Some("true") | Some("solid")
        )
    };
    for prim in prims.lists() {
        // Width comes from the classic bare `(width w)` or the newer
        // `(stroke (width w))` form KiCad uses on board graphics. A zero
        // width stamps the centerline as zero-width copper: the conservative
        // minimum (KiCad's editor rejects zero-width strokes, so this only
        // arises in hand-edited files, and inventing a default stroke width
        // would over-claim copper).
        let width = prim
            .find_f64("width")
            .or_else(|| prim.find("stroke").and_then(|s| s.find_f64("width")))
            .unwrap_or(0.0);
        let r = width / 2.0;
        match prim.name() {
            Some("gr_poly") | Some("poly") => {
                let Some(pts_list) = prim.find("pts") else {
                    continue;
                };
                let pts: Vec<(f64, f64)> = pts_list
                    .find_all("xy")
                    .filter_map(|p| Some(to_world(p.arg_f64(0)?, p.arg_f64(1)?)))
                    .collect();
                if pts.len() >= 3 {
                    // Custom-pad polygon primitives are always filled; a
                    // nonzero width strokes the outline, inflating it by r.
                    out.push(Shape::Polygon { pts, r });
                }
            }
            Some("gr_line") => {
                if let (Some(a), Some(b)) = (xy_pair(prim, "start"), xy_pair(prim, "end")) {
                    let a = to_world(a.0, a.1);
                    let b = to_world(b.0, b.1);
                    out.push(Shape::Capsule(Capsule {
                        ax: a.0,
                        ay: a.1,
                        bx: b.0,
                        by: b.1,
                        r,
                    }));
                }
            }
            Some("gr_arc") => {
                if let (Some(a), Some(b)) = (xy_pair(prim, "start"), xy_pair(prim, "end")) {
                    let a = to_world(a.0, a.1);
                    let b = to_world(b.0, b.1);
                    let mid = xy_pair(prim, "mid").map(|m| to_world(m.0, m.1));
                    // Covering flattening (not the lossy board-arc chain): a
                    // pad-primitive arc is exact copper, and chord sag would
                    // hide a grazing short on the stroke.
                    for cap in covering_arc_from_3(a, mid, b, width) {
                        out.push(Shape::Capsule(cap));
                    }
                }
            }
            Some("gr_circle") => {
                let (Some(c), Some(e)) = (xy_pair(prim, "center"), xy_pair(prim, "end")) else {
                    continue;
                };
                let radius = (e.0 - c.0).hypot(e.1 - c.1);
                let c = to_world(c.0, c.1);
                if filled(prim) {
                    out.push(Shape::Capsule(Capsule {
                        ax: c.0,
                        ay: c.1,
                        bx: c.0,
                        by: c.1,
                        r: radius + r,
                    }));
                } else {
                    // Stroke-only ring: the interior is NOT copper. Flatten the
                    // circumference into capsule links of the stroke radius so
                    // copper legitimately inside the ring stays silent.
                    out.extend(
                        ring_capsules(c.0, c.1, radius, r)
                            .into_iter()
                            .map(Shape::Capsule),
                    );
                }
            }
            Some("gr_rect") => {
                let (Some(a), Some(b)) = (xy_pair(prim, "start"), xy_pair(prim, "end")) else {
                    continue;
                };
                let corners = [(a.0, a.1), (b.0, a.1), (b.0, b.1), (a.0, b.1)];
                if filled(prim) {
                    let pts = corners.into_iter().map(|(x, y)| to_world(x, y)).collect();
                    out.push(Shape::Polygon { pts, r });
                } else {
                    for i in 0..4 {
                        let p = to_world(corners[i].0, corners[i].1);
                        let q = to_world(corners[(i + 1) % 4].0, corners[(i + 1) % 4].1);
                        out.push(Shape::Capsule(Capsule {
                            ax: p.0,
                            ay: p.1,
                            bx: q.0,
                            by: q.1,
                            r,
                        }));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Flatten a full circle of the given centre/radius into a chain of capsule
/// links of radius `r` (an annulus / stroked ring, not a solid disc), covering
/// the true annulus. See [`covering_arc_capsules`] for the covering scheme.
fn ring_capsules(cx: f64, cy: f64, radius: f64, r: f64) -> Vec<Capsule> {
    covering_arc_capsules(cx, cy, radius, 0.0, std::f64::consts::TAU, r)
}

/// Maximum chord sagitta permitted when flattening a circle or covering arc
/// (mm). The covering scheme in [`covering_arc_capsules`] converts this into a
/// copper overstatement of at most the same amount, kept strictly under
/// [`CLEARANCE_TOLERANCE_MM`]. Precisely what that buys: a gap routed AT the
/// rule (or over it) still measures above `rule - CLEARANCE_TOLERANCE_MM`, so
/// routing-to-rule copper is never flagged; the overstatement only narrows
/// the forgiveness band, so a gap already 2.5-5 µm under the rule (a true
/// sub-rule gap the tolerance would otherwise forgive) can now be reported.
/// That reports true violations early, never invents one on rule-compliant
/// copper.
const RING_SAGITTA_MM: f64 = 0.0025;

/// Flatten the circular arc of `sweep` radians starting at angle `a0` on the
/// circle (`cx`, `cy`, `radius`) into capsule links of stroke radius `r` that
/// COVER the true arc band.
///
/// Chord flattening alone puts the capsule midlines INSIDE the true circle by
/// the sagitta `s`, shrinking both copper edges of the stroke and hiding
/// grazing shorts (missing a short is this module's worst failure). Instead
/// the segment count is chosen so `s <= RING_SAGITTA_MM`, the chain vertices
/// sit at `radius + s/2` (splitting the sag symmetrically about the true
/// circle), and the capsule radius is inflated by `s/2`. The capsule surface
/// then tracks the true band `[radius - r, radius + r]` to within `s` on both
/// edges: overstatement is at most `s` (at vertices), and the only
/// under-cover is the O(s²/radius) mid-chord residue on the outer edge,
/// nanometres at this tolerance. `s` is at most [`RING_SAGITTA_MM`], under
/// the [`CLEARANCE_TOLERANCE_MM`] finding band.
/// The bias is deliberately FN-averse: a true air gap smaller than the
/// overstatement (a couple of microns) can read as a touch and be reported a
/// short; the alternative (chords under the true circle) silently drops real
/// grazing shorts, the worse failure.
fn covering_arc_capsules(
    cx: f64,
    cy: f64,
    radius: f64,
    a0: f64,
    sweep: f64,
    r: f64,
) -> Vec<Capsule> {
    let n = covering_segments(radius, sweep);
    let s = radius * (1.0 - (sweep.abs() / (2.0 * n as f64)).cos());
    let mid_radius = radius + s / 2.0;
    let at = |ang: f64| (cx + mid_radius * ang.cos(), cy + mid_radius * ang.sin());
    let mut caps = Vec::with_capacity(n);
    let mut prev = at(a0);
    for i in 1..=n {
        let p = at(a0 + sweep * i as f64 / n as f64);
        caps.push(Capsule {
            ax: prev.0,
            ay: prev.1,
            bx: p.0,
            by: p.1,
            r: r + s / 2.0,
        });
        prev = p;
    }
    caps
}

/// Segment count that keeps the chord sagitta `radius * (1 - cos(sweep/2n))`
/// at or below [`RING_SAGITTA_MM`], floored at [`ARC_SEGMENTS`] per half-turn
/// (minimum 1).
fn covering_segments(radius: f64, sweep: f64) -> usize {
    let turns = sweep.abs() / std::f64::consts::PI;
    let floor = ((ARC_SEGMENTS as f64 * turns).ceil() as usize).max(1);
    if radius <= RING_SAGITTA_MM {
        return floor;
    }
    let half_angle = (1.0 - RING_SAGITTA_MM / radius).acos();
    if half_angle <= 0.0 {
        return floor;
    }
    ((sweep.abs() / (2.0 * half_angle)).ceil() as usize).max(floor)
}

/// Read a `(pts (xy ..)(xy ..))` child into world-frame coordinates (no
/// transform; zones / filled polygons are already in board frame).
fn read_pts(list: &List) -> Option<Vec<(f64, f64)>> {
    let pts = list.find("pts")?;
    let out: Vec<(f64, f64)> = pts
        .find_all("xy")
        .filter_map(|p| Some((p.arg_f64(0)?, p.arg_f64(1)?)))
        .collect();
    (!out.is_empty()).then_some(out)
}

/// Flatten an arc (start, optional mid, end) of the given width into a chain of
/// capsule links. Without a mid point we approximate with the chord.
fn flatten_arc(
    start: (f64, f64),
    mid: Option<(f64, f64)>,
    end: (f64, f64),
    width: f64,
) -> Vec<Capsule> {
    let r = width / 2.0;
    let Some(mid) = mid else {
        return vec![Capsule {
            ax: start.0,
            ay: start.1,
            bx: end.0,
            by: end.1,
            r,
        }];
    };
    // Circumcircle of the three points.
    let Some((cx, cy, radius)) = circle_from_3(start, mid, end) else {
        return vec![Capsule {
            ax: start.0,
            ay: start.1,
            bx: end.0,
            by: end.1,
            r,
        }];
    };
    let ang = |p: (f64, f64)| (p.1 - cy).atan2(p.0 - cx);
    let a0 = ang(start);
    let am = ang(mid);
    let a1 = ang(end);
    // Sweep direction: go start->mid->end the short way through mid.
    // Normalise so the arc passes through mid.
    let sweep = arc_sweep_through(a0, a1, am);
    let mut caps = Vec::with_capacity(ARC_SEGMENTS);
    let mut prev = start;
    for i in 1..=ARC_SEGMENTS {
        let t = i as f64 / ARC_SEGMENTS as f64;
        let a = a0 + sweep * t;
        let p = (cx + radius * a.cos(), cy + radius * a.sin());
        caps.push(Capsule {
            ax: prev.0,
            ay: prev.1,
            bx: p.0,
            by: p.1,
            r,
        });
        prev = p;
    }
    caps
}

/// As [`flatten_arc`], but the chain COVERS the true stroke band (see
/// [`covering_arc_capsules`]). Used for pad-primitive arcs, which are exact
/// copper: the lossy board-arc chain's chord sag (up to `~0.02 * radius`)
/// could hide a grazing short on the stroke.
fn covering_arc_from_3(
    start: (f64, f64),
    mid: Option<(f64, f64)>,
    end: (f64, f64),
    width: f64,
) -> Vec<Capsule> {
    let r = width / 2.0;
    let chord = vec![Capsule {
        ax: start.0,
        ay: start.1,
        bx: end.0,
        by: end.1,
        r,
    }];
    let Some(mid) = mid else {
        return chord;
    };
    let Some((cx, cy, radius)) = circle_from_3(start, mid, end) else {
        return chord;
    };
    let ang = |p: (f64, f64)| (p.1 - cy).atan2(p.0 - cx);
    let a0 = ang(start);
    let sweep = arc_sweep_through(a0, ang(end), ang(mid));
    covering_arc_capsules(cx, cy, radius, a0, sweep, r)
}

/// Total signed CCW sweep from angle `from` to angle `to` that passes through
/// `via` (radians): the positive CCW sweep when `via` lies on it, otherwise
/// the complementary negative sweep.
fn arc_sweep_through(from: f64, to: f64, via: f64) -> f64 {
    let norm = |x: f64| {
        let mut v = x;
        while v <= -std::f64::consts::PI {
            v += std::f64::consts::TAU;
        }
        while v > std::f64::consts::PI {
            v -= std::f64::consts::TAU;
        }
        v
    };
    let mut s = norm(to - from);
    if s < 0.0 {
        s += std::f64::consts::TAU;
    }
    let mut m = norm(via - from);
    if m < 0.0 {
        m += std::f64::consts::TAU;
    }
    if m <= s {
        s
    } else {
        s - std::f64::consts::TAU
    }
}

/// Circumcircle (centre, radius) of three points, or None if colinear.
fn circle_from_3(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> Option<(f64, f64, f64)> {
    let d = 2.0 * (a.0 * (b.1 - c.1) + b.0 * (c.1 - a.1) + c.0 * (a.1 - b.1));
    if d.abs() < 1e-12 {
        return None;
    }
    let a2 = a.0 * a.0 + a.1 * a.1;
    let b2 = b.0 * b.0 + b.1 * b.1;
    let c2 = c.0 * c.0 + c.1 * c.1;
    let ux = (a2 * (b.1 - c.1) + b2 * (c.1 - a.1) + c2 * (a.1 - b.1)) / d;
    let uy = (a2 * (c.0 - b.0) + b2 * (a.0 - c.0) + c2 * (b.0 - a.0)) / d;
    let r = ((a.0 - ux).powi(2) + (a.1 - uy).powi(2)).sqrt();
    Some((ux, uy, r))
}

/// `(at x y [rot])` reader.
fn at_of(list: &List) -> (f64, f64, f64) {
    match list.find("at") {
        Some(at) => (
            at.arg_f64(0).unwrap_or(0.0),
            at.arg_f64(1).unwrap_or(0.0),
            at.arg_f64(2).unwrap_or(0.0),
        ),
        None => (0.0, 0.0, 0.0),
    }
}

/// Reference designator of a footprint (property "Reference" or fp_text).
fn footprint_reference(fp: &List) -> String {
    for prop in fp.find_all("property") {
        if prop.arg_value(0).as_deref() == Some("Reference") {
            if let Some(v) = prop.arg_value(1) {
                return v;
            }
        }
    }
    for t in fp.find_all("fp_text") {
        if t.arg_value(0).as_deref() == Some("reference") {
            if let Some(v) = t.arg_value(1) {
                return v;
            }
        }
    }
    String::new()
}

fn footprint_property(fp: &List, key: &str) -> String {
    let legacy_key = key.to_ascii_lowercase();
    fp.find_all("property")
        .find_map(|prop| {
            (prop.arg_value(0).as_deref() == Some(key))
                .then(|| prop.arg_value(1))
                .flatten()
        })
        .or_else(|| {
            fp.find_all("fp_text").find_map(|text| {
                (text.arg_value(0).as_deref() == Some(legacy_key.as_str()))
                    .then(|| text.arg_value(1))
                    .flatten()
            })
        })
        .unwrap_or_default()
}

/// One electrically independent net-tie group.
///
/// KiCad permits several groups in one footprint. Keeping the group as the
/// unit of exemption is load-bearing: flattening all nets by owner would waive
/// a real collision between two otherwise independent groups.
struct NetTieGroup {
    owner: String,
    nets: std::collections::HashSet<i64>,
    geometry_by_layer: std::collections::HashMap<String, Vec<(i64, Shape)>>,
}

/// Explicit local copper-link groups. The owner remains part of every group so
/// a legal A/B contact at NT1 never waives another A/B collision elsewhere.
#[derive(Default)]
struct NetTieOwners {
    groups: Vec<NetTieGroup>,
}

impl NetTieOwners {
    fn insert(&mut self, owner: String, nets: impl IntoIterator<Item = i64>) {
        if owner.is_empty() {
            return;
        }
        let nets: std::collections::HashSet<i64> = nets.into_iter().collect();
        if nets.len() < 2 {
            return;
        }
        self.groups.push(NetTieGroup {
            owner,
            nets,
            geometry_by_layer: std::collections::HashMap::new(),
        });
    }

    fn capture_geometry(&mut self, buckets: &LayerBuckets) {
        for group in &mut self.groups {
            for (layer, prims) in &buckets.by_layer {
                for prim in prims {
                    if prim.owner == group.owner && group.nets.contains(&prim.net) {
                        group
                            .geometry_by_layer
                            .entry(layer.clone())
                            .or_default()
                            .push((prim.net, prim.shape.clone()));
                    }
                }
            }
        }
    }

    fn point_shape((x, y): (f64, f64)) -> Shape {
        Shape::Capsule(Capsule {
            ax: x,
            ay: y,
            bx: x,
            by: y,
            r: 0.0,
        })
    }

    fn touches_geometry_at(geometry: &[(i64, Shape)], net: Option<i64>, point: (f64, f64)) -> bool {
        let point = Self::point_shape(point);
        geometry.iter().any(|(shape_net, shape)| {
            net.is_none_or(|wanted| wanted == *shape_net) && {
                let (gap, _) = shape_gap(shape, &point);
                is_touching(gap)
            }
        })
    }

    fn owned_or_endpoint_attached(
        group: &NetTieGroup,
        primitive: &Primitive,
        geometry: &[(i64, Shape)],
    ) -> bool {
        if primitive.owner == group.owner {
            return true;
        }
        let Shape::Capsule(capsule) = &primitive.shape else {
            return false;
        };
        [(capsule.ax, capsule.ay), (capsule.bx, capsule.by)]
            .into_iter()
            .any(|point| Self::touches_geometry_at(geometry, Some(primitive.net), point))
    }

    fn exempts(&self, layer: &str, a: &Primitive, b: &Primitive, contact: (f64, f64)) -> bool {
        self.groups.iter().any(|group| {
            group.nets.contains(&a.net)
                && group.nets.contains(&b.net)
                && group.geometry_by_layer.get(layer).is_some_and(|geometry| {
                    Self::owned_or_endpoint_attached(group, a, geometry)
                        && Self::owned_or_endpoint_attached(group, b, geometry)
                        && Self::touches_geometry_at(geometry, None, contact)
                })
        })
    }
}

/// Structured closed-pad pairs retained by older EAGLE-to-KiCad imports.
/// Requiring both the `TIED` footprint token and `Closed(a-b)` value keeps this
/// a declared legacy semantic rather than a generic name heuristic.
fn legacy_kicad_closed_pad_pairs(value: &str, footprint: &str) -> Vec<(String, String)> {
    let footprint_leaf = footprint.rsplit([':', '/']).next().unwrap_or(footprint);
    if !footprint_leaf
        .split(['_', '-'])
        .any(|token| token.eq_ignore_ascii_case("tied"))
    {
        return Vec::new();
    }

    let lower_value = value.to_ascii_lowercase();
    let mut pairs = Vec::new();
    let mut cursor = 0;
    while let Some(relative_start) = lower_value[cursor..].find("closed(") {
        let pair_start = cursor + relative_start + "closed(".len();
        let Some(relative_end) = lower_value[pair_start..].find(')') else {
            break;
        };
        let pair_end = pair_start + relative_end;
        if let Some((a, b)) = value[pair_start..pair_end].split_once('-') {
            let (a, b) = (a.trim(), b.trim());
            if !a.is_empty() && !b.is_empty() {
                pairs.push((a.to_string(), b.to_string()));
            }
        }
        cursor = pair_end + 1;
    }
    pairs
}

/// Insert the native KiCad net-tie groups for one footprint. Group strings are
/// comma-separated pad numbers, and separate string arguments are separate
/// electrical groups. A legacy `(attr net_tie)` with no group list means one
/// group containing all pads. The narrow two-field 0R convention is retained
/// only for old boards that predate native metadata.
fn insert_kicad_net_tie_groups(
    out: &mut NetTieOwners,
    fp: &List,
    owner: String,
    nets: &mut NetResolver,
) {
    let pad_nets: std::collections::HashMap<String, i64> = fp
        .find_all("pad")
        .filter_map(|pad| Some((pad.arg_value(0)?, nets.net_ref(pad)?)))
        .collect();

    let group_specs: Vec<String> = fp
        .find("net_tie_pad_groups")
        .into_iter()
        .flat_map(|groups| (0..).map_while(|i| groups.arg_value(i)))
        .collect();
    if !group_specs.is_empty() {
        for spec in group_specs {
            out.insert(
                owner.clone(),
                spec.split(',')
                    .filter_map(|pad| pad_nets.get(pad.trim()).copied()),
            );
        }
        return;
    }

    let native_attr = fp.find_all("attr").any(|attr| {
        (0..)
            .map_while(|i| attr.arg_value(i))
            .any(|value| value == "net_tie")
    });
    let value = footprint_property(fp, "Value");
    let footprint = fp.arg_value(0).unwrap_or_default();
    if native_attr {
        out.insert(owner, pad_nets.into_values());
        return;
    }

    // Some KiCad boards imported from older EAGLE libraries predate native
    // net-tie metadata but retain two independent, structured declarations:
    // a footprint token `TIED` and a value such as
    // `Closed(1-2)/Opened(2-3)`. Preserve only the explicitly closed pair; in
    // particular, never flatten pad 3 into the 1/2 tie group.
    let closed_pairs = legacy_kicad_closed_pad_pairs(&value, &footprint);
    if !closed_pairs.is_empty() {
        for (a, b) in closed_pairs {
            out.insert(
                owner.clone(),
                [pad_nets.get(&a).copied(), pad_nets.get(&b).copied()]
                    .into_iter()
                    .flatten(),
            );
        }
        return;
    }

    let legacy_zero_ohm = crate::dnp::is_explicit_zero_ohm_copper_link_fields(&value, &footprint);
    if legacy_zero_ohm {
        out.insert(owner, pad_nets.into_values());
    }
}

// ── Board clearance rule ─────────────────────────────────────────────────────

/// Read the board's design-rule copper clearance (mm), else the default. KiCad
/// stores it in `(setup (rules (min_clearance N)))` (modern) or a default
/// net-class `(clearance N)`. We take the smallest credible rule we can find in
/// `(setup ...)`, since DRC should use the tightest rule. Zone `connect_pads`
/// clearances are intentionally ignored (they are not the trace rule).
fn board_clearance(root: &List) -> f64 {
    let Some(setup) = root.find("setup") else {
        return DEFAULT_CLEARANCE_MM;
    };
    // Modern: (setup (rules (min_clearance N) ...)).
    if let Some(rules) = setup.find("rules") {
        if let Some(c) = rules.find_f64("min_clearance") {
            if c > 0.0 {
                return c;
            }
        }
    }
    // Direct (setup (min_clearance N)) or (setup (clearance N)).
    for key in ["min_clearance", "clearance", "trace_clearance"] {
        if let Some(c) = setup.find_f64(key) {
            if c > 0.0 {
                return c;
            }
        }
    }
    DEFAULT_CLEARANCE_MM
}

// ── Top-level DRC ────────────────────────────────────────────────────────────

/// Run geometric short / clearance detection on a parsed `.kicad_pcb` document.
/// `clearance_override` forces the clearance rule (mm) when `Some`.
pub fn run_drc(doc: &Document, clearance_override: Option<f64>) -> DrcReport {
    let rules = clearance_override.map(ClearanceRules::new);
    run_drc_with_clearance_rules(doc, rules)
}

/// Run geometric short / clearance detection with project-derived per-net
/// clearance rules.
pub fn run_drc_with_clearance_rules(doc: &Document, rules: Option<ClearanceRules>) -> DrcReport {
    let Some(root) = doc.root() else {
        return DrcReport::default();
    };
    if root.name() != Some("kicad_pcb") {
        return DrcReport::default();
    }
    let mut nets = NetResolver::from_root(root);
    let rules = rules.unwrap_or_else(|| ClearanceRules::new(board_clearance(root)));
    let buckets = collect_primitives(root, &mut nets);

    // Nets that carry no real connectivity (net 0 and KiCad's per-pad
    // `unconnected-(...)` placeholders): copper on them is never a short.
    let no_net: std::collections::HashSet<i64> = buckets
        .by_layer
        .values()
        .flat_map(|v| v.iter().map(|p| p.net))
        .chain(buckets.zones.values().flat_map(|v| v.iter().map(|z| z.net)))
        .filter(|&id| nets.is_no_net(id))
        .collect();

    let mut net_ties = NetTieOwners::default();
    for fp in root.find_all("footprint").chain(root.find_all("module")) {
        insert_kicad_net_tie_groups(&mut net_ties, fp, footprint_reference(fp), &mut nets);
    }
    net_ties.capture_geometry(&buckets);
    sweep_buckets(buckets, &rules, &no_net, &net_ties, |id| nets.name_of(id))
}

/// The geometry-source-agnostic core of the DRC: take per-layer copper buckets
/// (already in board mm), a clearance rule, the set of net ids that carry no
/// connectivity (never a short), and a way to resolve a net id to its name, and
/// run the R-tree pruned edge / clearance sweep plus the zone containment pass.
///
/// Both the KiCad path ([`run_drc`]) and the Eagle path
/// ([`eagle_drc::run`]) build [`LayerBuckets`] from their own geometry and hand
/// them here, so there is exactly one detection / classification engine.
///
/// `net_ties` contains only explicitly recognised net-tie/jumper footprints,
/// keyed by owner. An exemption therefore applies only to copper touching that
/// particular component's geometry. Merely sharing any ordinary component can
/// never waive a net pair globally.
fn sweep_buckets(
    buckets: LayerBuckets,
    rules: &ClearanceRules,
    no_net: &std::collections::HashSet<i64>,
    net_ties: &NetTieOwners,
    name_of: impl Fn(i64) -> String,
) -> DrcReport {
    let max_clearance = rules.max_clearance();
    let mut report = DrcReport {
        clearance_mm: rules.default_clearance_mm,
        findings: Vec::new(),
        primitive_count: buckets.by_layer.values().map(Vec::len).sum(),
        version_warning: None,
    };

    // De-dup findings on the same net pair + layer + rounded location, so a
    // pour overlapping a long track does not emit thousands of near-identical
    // rows.
    let mut seen: std::collections::HashSet<(i64, i64, String, i64, i64)> =
        std::collections::HashSet::new();

    // Record one finding, de-duplicating on net pair + layer + ~0.25 mm cell so
    // a pour running alongside a long track does not emit thousands of rows.
    let mut record = |kind: ViolationKind,
                      pa: i64,
                      pb: i64,
                      item_a: Item,
                      item_b: Item,
                      layer: &str,
                      cx: f64,
                      cy: f64,
                      gap: f64,
                      required_clearance_mm: f64| {
        let (na, nb) = (pa.min(pb), pa.max(pb));
        let key = (
            na,
            nb,
            layer.to_string(),
            (cx * 4.0).round() as i64,
            (cy * 4.0).round() as i64,
        );
        if !seen.insert(key) {
            return;
        }
        let (item_a, item_b) = if pa <= pb {
            (item_a, item_b)
        } else {
            (item_b, item_a)
        };
        report.findings.push(DrcFinding {
            kind,
            net_a: na,
            net_b: nb,
            net_a_name: name_of(na),
            net_b_name: name_of(nb),
            layer: layer.to_string(),
            x: cx,
            y: cy,
            gap_mm: gap,
            required_clearance_mm,
            item_a,
            item_b,
        });
    };

    let empty_zones: Vec<ZonePoly> = Vec::new();
    for (layer, prims_ref) in &buckets.by_layer {
        let zones = buckets.zones.get(layer).unwrap_or(&empty_zones);
        // Index primitives, recording their position for shape lookup.
        let mut prims = prims_ref.clone();
        for (i, p) in prims.iter_mut().enumerate() {
            p.idx = i;
        }
        let leaves: Vec<Leaf> = prims
            .iter()
            .map(|p| Leaf {
                bounds: p.bounds,
                idx: p.idx,
            })
            .collect();
        let tree = RTree::bulk_load(leaves);

        // ── Edge / clearance sweep (R-tree pruned) ──────────────────────────
        for p in &prims {
            // Query window: this primitive's bounds inflated by the clearance.
            let query = AABB::from_corners(
                [p.bounds[0] - max_clearance, p.bounds[1] - max_clearance],
                [p.bounds[2] + max_clearance, p.bounds[3] + max_clearance],
            );
            for leaf in tree.locate_in_envelope_intersecting(query) {
                let q = &prims[leaf.idx];
                // Each unordered pair once; skip same-net.
                if q.idx <= p.idx || q.net == p.net {
                    continue;
                }
                // Net 0 and `unconnected-(...)` copper carry no connectivity, so
                // they cannot form an electrical short.
                if no_net.contains(&p.net) || no_net.contains(&q.net) {
                    continue;
                }
                let name_p = name_of(p.net);
                let name_q = name_of(q.net);
                let clearance = rules.effective_clearance(&name_p, &name_q);
                let (gap, (cx, cy)) = shape_gap(&p.shape, &q.shape);
                if gap >= clearance {
                    continue;
                }
                // A deliberate net tie is a local footprint property, not a
                // global relationship between its two nets. The collision point
                // must land on that explicit tie's own copper geometry.
                if net_ties.exempts(layer, p, q, (cx, cy)) {
                    continue;
                }
                // A zone boundary edge "overlapping" a different-net pad (gap <= 0)
                // is almost always the antipad carve, not a short: KiCad always
                // clears a pad out of a different-net pour, and KiCad-10 (format
                // 20260206) fills represent that antipad with keyhole slits whose
                // boundary edges run through the pad interior, producing spurious
                // negative gaps (a 1668-short false-positive epidemic on one ESC,
                // none on the same board's tracks/vias). A real pour incursion is a
                // Track / Via / Arc crossing the boundary, which is still caught
                // (the BMS REG1_3V3 and FPV-Drone shorts are Track/Via<->Zone). So
                // a Zone<->Pad *overlap* is suppressed here; a positive sub-clearance
                // Zone<->Pad gap is still kept as a normal clearance note.
                let zone_pad = (p.kind == ItemKind::Zone && q.kind == ItemKind::Pad)
                    || (p.kind == ItemKind::Pad && q.kind == ItemKind::Zone);
                if zone_pad && is_touching(gap) {
                    continue;
                }
                // Copper in contact is always a short; see SHORT_TOUCH_EPS_MM for
                // why "in contact" is a band and not `<= 0.0`. A gap clear of that
                // band is a clearance violation only when it falls more than the
                // tolerance below the rule; a gap sitting at (or a few microns
                // under) the rule is routing-to-rule, not a defect, and is
                // dropped to kill the boundary-note noise.
                let kind = if is_touching(gap) {
                    ViolationKind::Short
                } else if gap < clearance - CLEARANCE_TOLERANCE_MM {
                    ViolationKind::Clearance
                } else {
                    continue;
                };
                record(
                    kind,
                    p.net,
                    q.net,
                    p.item(),
                    q.item(),
                    layer,
                    cx,
                    cy,
                    gap,
                    clearance,
                );
            }
        }

        // ── Zone containment pass ───────────────────────────────────────────
        // A primitive sitting fully inside a different-net pour (without ever
        // crossing the indexed boundary edges) is still a short. Test each
        // non-zone primitive's representative point against every opposite-net
        // zone whose bounding box contains it.
        if !zones.is_empty() {
            for p in &prims {
                // Skip zones (a pour is not "contained" in another) and no-net
                // copper. Pads participate: the filled contour is the source of
                // truth, so a pad inside solid different-net fill is a short.
                // KiCad 10 keyhole antipads encode their void as an inner loop
                // joined to the outer contour by a doubled-back slit; even-odd
                // containment excludes that inner loop and keeps the valid pad
                // silent.
                if p.kind == ItemKind::Zone || no_net.contains(&p.net) {
                    continue;
                }
                let (rx, ry) = representative_point(&p.shape);
                for z in zones {
                    if !z.filled || z.net == p.net || no_net.contains(&z.net) {
                        continue;
                    }
                    if rx < z.bounds[0] || rx > z.bounds[2] || ry < z.bounds[1] || ry > z.bounds[3]
                    {
                        continue;
                    }
                    if point_in_polygon(rx, ry, &z.pts) {
                        record(
                            ViolationKind::Short,
                            p.net,
                            z.net,
                            p.item(),
                            Item {
                                kind: ItemKind::Zone,
                                net: z.net,
                                owner: String::new(),
                            },
                            layer,
                            rx,
                            ry,
                            -max_clearance,
                            max_clearance,
                        );
                    }
                }
            }
        }
    }

    // Stable order: shorts first, then by net pair and layer.
    report.findings.sort_by(|a, b| {
        (a.kind == ViolationKind::Clearance)
            .cmp(&(b.kind == ViolationKind::Clearance))
            .then(a.net_a.cmp(&b.net_a))
            .then(a.net_b.cmp(&b.net_b))
            .then(a.layer.cmp(&b.layer))
    });
    report
}

/// Convenience: parse `.kicad_pcb` text and run DRC with the default clearance
/// rule (or the board's own rule when present).
pub fn drc_from_text(text: &str) -> Result<DrcReport, forge_sexpr::ParseError> {
    let doc = forge_sexpr::parse(text)?;
    let mut report = run_drc(&doc, None);
    report.version_warning = unvalidated_version_warning(text);
    Ok(report)
}

/// Like [`drc_from_text`] but with an explicit clearance rule (mm). KiCad 10
/// (format 20260206) keeps the design-rule clearance in the sibling `.kicad_pro`
/// (`net_settings.classes[].clearance`), not in the `.kicad_pcb` `(setup)` block,
/// so `board_clearance` would otherwise fall back to [`DEFAULT_CLEARANCE_MM`]. The
/// caller (which knows the board path) reads the project's Default-class clearance
/// and passes it here. `None` keeps the board/`DEFAULT_CLEARANCE_MM` behaviour.
pub fn drc_from_text_with_clearance(
    text: &str,
    clearance_override: Option<f64>,
) -> Result<DrcReport, forge_sexpr::ParseError> {
    drc_from_text_with_clearance_rules(text, clearance_override.map(ClearanceRules::new))
}

/// Like [`drc_from_text_with_clearance`] but with concrete per-net rules.
pub fn drc_from_text_with_clearance_rules(
    text: &str,
    rules: Option<ClearanceRules>,
) -> Result<DrcReport, forge_sexpr::ParseError> {
    let doc = forge_sexpr::parse(text)?;
    let mut report = run_drc_with_clearance_rules(&doc, rules);
    report.version_warning = unvalidated_version_warning(text);
    Ok(report)
}

/// Convenience: run the geometric DRC on Eagle `.brd` XML text, using the
/// board's own design-rule clearance (falling back to [`DEFAULT_CLEARANCE_MM`]).
pub fn eagle_drc_from_text(text: &str) -> DrcReport {
    eagle_drc::run(text, None)
}

/// Convenience: run the geometric DRC on Altium `.PcbDoc` raw bytes, using the
/// default clearance rule.
pub fn altium_drc_from_bytes(bytes: &[u8]) -> Result<DrcReport, crate::ExtractError> {
    altium_drc::run(bytes, None)
}

// ── Eagle .brd geometry → the same engine ────────────────────────────────────

/// Eagle `.brd` geometry extraction. The connectivity extractor in `eagle.rs`
/// reads pad nets; this reads *copper geometry per net* (wires, vias, pads,
/// smds, polygons, rectangles, circles) and feeds it to `sweep_buckets`, the
/// exact same short / clearance engine the KiCad path uses. The geometry is kept
/// in Eagle's native frame (millimetres, y-up); the DRC is self-consistent so
/// the y orientation never matters, only relative positions do.
///
/// Layer model: Eagle copper layers are numbered, 1 = Top, 16 = Bottom, 2..15
/// inner. A mirrored element (`rot="MR<deg>"`) is flipped onto the opposite side,
/// so its smds and the side-specific copper swap 1↔16.
pub mod eagle_drc {
    use super::{
        is_touching, make_prim, point_in_polygon, poly_poly_closest, sweep_buckets, Capsule,
        ClearanceRules, DrcFinding, DrcReport, Item, ItemKind, LayerBuckets, NetClassRule,
        NetTieOwners, Shape, ViolationKind, ARC_SEGMENTS, DEFAULT_CLEARANCE_MM,
    };
    use quick_xml::events::Event;
    use quick_xml::Reader;
    use std::collections::HashMap;

    type Attrs = HashMap<String, String>;

    /// A `<polygon>` (signal pour) being streamed: which signal owns it, its
    /// pour settings, and the vertex ring as (x, y, curve-to-next-degrees).
    struct PartialPoly {
        signal: usize,
        width: f64,
        layer: i64,
        isolate: f64,
        rank: i64,
        thermals: bool,
        orphans: bool,
        cutout: bool,
        verts: Vec<(f64, f64, f64)>,
    }

    /// One Eagle net class: its clearance matrix rows
    /// (`<clearance class="M" value="V"/>`, self entry included). Values are
    /// mm; a 0 / absent entry defers to the class fallback rules (see the
    /// rules construction in [`run`]).
    #[derive(Default)]
    struct EagleClass {
        clearances: Vec<(i64, f64)>,
    }

    fn attrs_of(e: &quick_xml::events::BytesStart) -> Attrs {
        e.attributes()
            .flatten()
            .map(|a| {
                // Unescape XML entities in the value (quick-xml leaves them raw),
                // consistent with the eagle.rs reader; fall back to raw bytes on
                // a decode error.
                // quick-xml deprecates this in favour of `normalized_value`, which takes an
                // `XmlVersion` the crate does not export, so the replacement is not
                // callable from outside quick-xml. Staying on the deprecated call
                // until upstream makes the successor reachable.
                #[allow(deprecated)]
                let value = a
                    .unescape_value()
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned());
                (String::from_utf8_lossy(a.key.as_ref()).into_owned(), value)
            })
            .collect()
    }

    fn num(a: &Attrs, k: &str) -> Option<f64> {
        a.get(k)?.parse().ok()
    }

    /// Parse an Eagle length value that may carry a unit suffix (`"6mil"`,
    /// `"0.15mm"`, or a bare number already in mm). Design-rule clearances are
    /// written in mil; raw geometry coordinates are bare mm.
    fn parse_len_mm(s: &str) -> Option<f64> {
        let s = s.trim();
        if let Some(v) = s.strip_suffix("mil") {
            return v.trim().parse::<f64>().ok().map(|m| m * 0.0254);
        }
        if let Some(v) = s.strip_suffix("mm") {
            return v.trim().parse::<f64>().ok();
        }
        if let Some(v) = s.strip_suffix("mic") {
            return v.trim().parse::<f64>().ok().map(|m| m * 0.001);
        }
        if let Some(v) = s.strip_suffix("inch") {
            return v.trim().parse::<f64>().ok().map(|m| m * 25.4);
        }
        s.parse::<f64>().ok()
    }

    /// A copper layer number is real copper (1 = Top, 16 = Bottom, 2..15 inner).
    fn is_copper_layer(n: i64) -> bool {
        (1..=16).contains(&n)
    }

    /// Canonical layer name for reporting, matching the connectivity extractor's
    /// `F.Cu` / `B.Cu` convention for the two outer layers.
    fn layer_name(n: i64) -> String {
        match n {
            1 => "F.Cu".to_string(),
            16 => "B.Cu".to_string(),
            other => format!("In{other}.Cu"),
        }
    }

    /// Mirror a copper layer number across the board (Top↔Bottom). Inner layers
    /// reflect about the stack centre; for the two-layer boards in scope this is
    /// just 1↔16, but the general form keeps multilayer honest.
    fn mirror_layer(n: i64) -> i64 {
        if (1..=16).contains(&n) {
            17 - n
        } else {
            n
        }
    }

    /// One pad / smd shape inside a package definition, in package-local mm.
    /// `name` is the pad name, used to map the placed copper to its net.
    #[derive(Clone)]
    enum PkgItem {
        /// Through-hole pad: present on every copper layer. `shape` is Eagle's
        /// (round / square / octagon / long / offset); `diameter` is the pad
        /// copper diameter, `drill` the hole, `rot_deg` the pad's own rotation.
        Pad {
            name: String,
            x: f64,
            y: f64,
            diameter: f64,
            drill: f64,
            shape: String,
            rot_deg: f64,
        },
        /// Surface-mount pad: a single layer (1 by default, flipped by mirror),
        /// `dx`/`dy` rectangle with `roundness` (0..100 %) and `rot_deg`.
        Smd {
            name: String,
            x: f64,
            y: f64,
            dx: f64,
            dy: f64,
            roundness: f64,
            rot_deg: f64,
            layer: i64,
        },
    }

    impl PkgItem {
        fn name(&self) -> &str {
            match self {
                PkgItem::Pad { name, .. } | PkgItem::Smd { name, .. } => name,
            }
        }
    }

    /// A placed component instance.
    struct Element {
        name: String,
        library: String,
        package: String,
        value: String,
        x: f64,
        y: f64,
        rot_deg: f64,
        mirrored: bool,
    }

    /// Board-level copper geometry attached to a net (signal), read straight from
    /// `<signal>` children: wires, vias, polygons, plus board-level rectangles /
    /// circles on copper layers.
    enum SignalGeom {
        Wire {
            x1: f64,
            y1: f64,
            x2: f64,
            y2: f64,
            width: f64,
            curve: f64,
            layer: i64,
        },
        Via {
            x: f64,
            y: f64,
            diameter: Option<f64>,
            drill: f64,
            shape: String,
        },
        Polygon {
            width: f64,
            layer: i64,
            /// Antipad gap the pour keeps around foreign copper (mm). Eagle
            /// applies max(isolate, design-rule / class clearance); 0 means
            /// "rules only".
            isolate: f64,
            /// Pour priority. Overlapping same-rank pours of different
            /// signals are an Eagle DRC error (both get poured: a real short);
            /// with differing ranks the higher-numbered pour yields.
            rank: i64,
            /// Thermal-relief spokes on same-net pads (copper removal only).
            thermals: bool,
            /// Keep unconnected fill pockets (`orphans="on"`); off removes
            /// copper, never adds it.
            orphans: bool,
            /// `pour="cutout"`: the polygon carves other pours and is not
            /// copper itself.
            cutout: bool,
            verts: Vec<(f64, f64, f64)>, // (x, y, curve-to-next in deg)
        },
        Rect {
            x1: f64,
            y1: f64,
            x2: f64,
            y2: f64,
            rot_deg: f64,
            layer: i64,
        },
        Circle {
            x: f64,
            y: f64,
            radius: f64,
            width: f64,
            layer: i64,
        },
    }

    /// Everything parsed from a `.brd`: package defs, placements, per-net copper,
    /// and the board's design-rule clearance.
    struct Parsed {
        // Keyed by (library, package): Eagle namespaces packages per <library>,
        // so same-named packages in different libraries must not merge.
        packages: HashMap<(String, String), Vec<PkgItem>>,
        elements: Vec<Element>,
        /// signal index -> (name, geometry)
        signals: Vec<(String, Vec<SignalGeom>)>,
        /// signal index -> net-class number (`<signal class="N">`, default 0).
        signal_classes: Vec<i64>,
        /// Net classes declared in `<classes>`, by class number.
        classes: HashMap<i64, EagleClass>,
        clearance_mm: Option<f64>,
        via_restring: ViaRestring,
        pad_elongation_long_pct: f64,
        pad_elongation_offset_pct: f64,
        saw_eagle: bool,
    }

    impl Default for Parsed {
        fn default() -> Self {
            Self {
                packages: HashMap::new(),
                elements: Vec::new(),
                signals: Vec::new(),
                signal_classes: Vec::new(),
                classes: HashMap::new(),
                clearance_mm: None,
                via_restring: ViaRestring::default(),
                pad_elongation_long_pct: 100.0,
                pad_elongation_offset_pct: 100.0,
                saw_eagle: false,
            }
        }
    }

    /// Via outer-diameter rule, used when a via gives no explicit `diameter`.
    /// Eagle computes the annular ring as `max(rvViaOuter * drill, rlMinViaOuter)`
    /// clamped to `rlMaxViaOuter`, and the outer diameter is `drill + 2*ring`.
    struct ViaRestring {
        rv: f64,
        min_mm: f64,
        max_mm: f64,
    }

    impl Default for ViaRestring {
        fn default() -> Self {
            // Eagle's stock defaults.
            ViaRestring {
                rv: 0.25,
                min_mm: 0.2032,
                max_mm: 0.508,
            }
        }
    }

    impl ViaRestring {
        fn outer_diameter(&self, drill: f64) -> f64 {
            let ring = (self.rv * drill).clamp(self.min_mm, self.max_mm);
            drill + 2.0 * ring
        }
    }

    /// Parse `rot`/`rot`-style strings: `R90`, `MR270`, `M90`, `SR0`. Returns
    /// `(degrees, mirrored)`.
    fn parse_rot(s: &str) -> (f64, bool) {
        let mirrored = s.contains('M');
        let deg = s
            .trim_start_matches(['M', 'S', 'R'])
            .parse::<f64>()
            .unwrap_or(0.0);
        (deg, mirrored)
    }

    /// Stream the XML once, collecting packages, elements, signals and rules.
    fn parse(text: &str) -> Parsed {
        let mut reader = Reader::from_str(text);
        reader.config_mut().trim_text(true);
        let mut out = Parsed::default();

        // Parser state.
        let mut cur_library = String::new();
        let mut cur_package: Option<String> = None;
        let mut cur_signal: Option<usize> = None;
        // The net class currently being read (its `<clearance>` rows follow).
        let mut cur_class: Option<i64> = None;
        // The polygon currently being read (signal index, partial Polygon).
        let mut cur_poly: Option<PartialPoly> = None;
        // Pending raw param values, resolved after the stream.
        let mut params: HashMap<String, String> = HashMap::new();
        // Depth nesting so we know when a polygon's vertices end.
        let mut buf = Vec::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                    let name = e.name().as_ref().to_vec();
                    let a = attrs_of(&e);
                    match name.as_slice() {
                        b"eagle" => out.saw_eagle = true,
                        b"param" => {
                            if let (Some(k), Some(v)) = (a.get("name"), a.get("value")) {
                                params.insert(k.clone(), v.clone());
                            }
                        }
                        b"library" => {
                            cur_library = a.get("name").cloned().unwrap_or_default();
                        }
                        b"package" => {
                            cur_package = a.get("name").cloned();
                            if let Some(n) = &cur_package {
                                out.packages
                                    .entry((cur_library.clone(), n.clone()))
                                    .or_default();
                            }
                        }
                        b"pad" => {
                            if let Some(pkg) = &cur_package {
                                if let (Some(x), Some(y)) = (num(&a, "x"), num(&a, "y")) {
                                    let drill = num(&a, "drill").unwrap_or(0.0);
                                    let diameter = num(&a, "diameter").unwrap_or(0.0);
                                    let (rot_deg, _) =
                                        parse_rot(a.get("rot").map(String::as_str).unwrap_or("R0"));
                                    out.packages
                                        .entry((cur_library.clone(), pkg.clone()))
                                        .or_default()
                                        .push(PkgItem::Pad {
                                            name: a.get("name").cloned().unwrap_or_default(),
                                            x,
                                            y,
                                            diameter,
                                            drill,
                                            shape: a
                                                .get("shape")
                                                .cloned()
                                                .unwrap_or_else(|| "round".to_string()),
                                            rot_deg,
                                        });
                                }
                            }
                        }
                        b"smd" => {
                            if let Some(pkg) = &cur_package {
                                if let (Some(x), Some(y), Some(dx), Some(dy)) =
                                    (num(&a, "x"), num(&a, "y"), num(&a, "dx"), num(&a, "dy"))
                                {
                                    let (rot_deg, _) =
                                        parse_rot(a.get("rot").map(String::as_str).unwrap_or("R0"));
                                    let layer = num(&a, "layer").map(|v| v as i64).unwrap_or(1);
                                    out.packages
                                        .entry((cur_library.clone(), pkg.clone()))
                                        .or_default()
                                        .push(PkgItem::Smd {
                                            name: a.get("name").cloned().unwrap_or_default(),
                                            x,
                                            y,
                                            dx,
                                            dy,
                                            roundness: num(&a, "roundness").unwrap_or(0.0),
                                            rot_deg,
                                            layer,
                                        });
                                }
                            }
                        }
                        b"element" => {
                            let (rot_deg, mirrored) =
                                parse_rot(a.get("rot").map(String::as_str).unwrap_or("R0"));
                            out.elements.push(Element {
                                name: a.get("name").cloned().unwrap_or_default(),
                                library: a.get("library").cloned().unwrap_or_default(),
                                package: a.get("package").cloned().unwrap_or_default(),
                                value: a.get("value").cloned().unwrap_or_default(),
                                x: num(&a, "x").unwrap_or(0.0),
                                y: num(&a, "y").unwrap_or(0.0),
                                rot_deg,
                                mirrored,
                            });
                        }
                        b"class" => {
                            if let Some(n) = num(&a, "number").map(|v| v as i64) {
                                cur_class = Some(n);
                                out.classes.entry(n).or_default();
                            }
                        }
                        b"clearance" => {
                            if let Some(n) = cur_class {
                                if let (Some(other), Some(v)) = (
                                    num(&a, "class").map(|v| v as i64),
                                    a.get("value").and_then(|s| parse_len_mm(s)),
                                ) {
                                    out.classes
                                        .entry(n)
                                        .or_default()
                                        .clearances
                                        .push((other, v));
                                }
                            }
                        }
                        b"signal" => {
                            out.signals
                                .push((a.get("name").cloned().unwrap_or_default(), Vec::new()));
                            out.signal_classes
                                .push(num(&a, "class").map(|v| v as i64).unwrap_or(0));
                            cur_signal = Some(out.signals.len() - 1);
                        }
                        b"wire" => {
                            if let Some(si) = cur_signal {
                                if let (Some(x1), Some(y1), Some(x2), Some(y2)) =
                                    (num(&a, "x1"), num(&a, "y1"), num(&a, "x2"), num(&a, "y2"))
                                {
                                    let layer = num(&a, "layer").map(|v| v as i64).unwrap_or(0);
                                    if is_copper_layer(layer) {
                                        out.signals[si].1.push(SignalGeom::Wire {
                                            x1,
                                            y1,
                                            x2,
                                            y2,
                                            width: num(&a, "width").unwrap_or(0.0),
                                            curve: num(&a, "curve").unwrap_or(0.0),
                                            layer,
                                        });
                                    }
                                }
                            }
                        }
                        b"via" => {
                            if let Some(si) = cur_signal {
                                if let (Some(x), Some(y)) = (num(&a, "x"), num(&a, "y")) {
                                    out.signals[si].1.push(SignalGeom::Via {
                                        x,
                                        y,
                                        diameter: num(&a, "diameter"),
                                        drill: num(&a, "drill").unwrap_or(0.0),
                                        shape: a
                                            .get("shape")
                                            .cloned()
                                            .unwrap_or_else(|| "round".to_string()),
                                    });
                                }
                            }
                        }
                        b"polygon" => {
                            if let Some(si) = cur_signal {
                                let layer = num(&a, "layer").map(|v| v as i64).unwrap_or(0);
                                if is_copper_layer(layer) {
                                    cur_poly = Some(PartialPoly {
                                        signal: si,
                                        width: num(&a, "width").unwrap_or(0.0),
                                        layer,
                                        isolate: num(&a, "isolate").unwrap_or(0.0),
                                        // Board signal polygons carry rank
                                        // 1..6 and behave as rank 1 when the
                                        // attribute is absent (Eagle's editor
                                        // floor), so an attribute-less pour
                                        // and an explicit rank="1" pour are
                                        // the SAME rank; clamp keeps
                                        // hand-written out-of-range values in
                                        // the board range too.
                                        rank: num(&a, "rank")
                                            .map(|v| (v as i64).clamp(1, 6))
                                            .unwrap_or(1),
                                        thermals: a.get("thermals").map(String::as_str)
                                            != Some("off"),
                                        orphans: a.get("orphans").map(String::as_str) == Some("on"),
                                        cutout: a.get("pour").map(String::as_str) == Some("cutout"),
                                        verts: Vec::new(),
                                    });
                                }
                            }
                        }
                        b"vertex" => {
                            if let Some(poly) = cur_poly.as_mut() {
                                if let (Some(x), Some(y)) = (num(&a, "x"), num(&a, "y")) {
                                    poly.verts.push((x, y, num(&a, "curve").unwrap_or(0.0)));
                                }
                            }
                        }
                        b"rectangle" => {
                            if let Some(si) = cur_signal {
                                let layer = num(&a, "layer").map(|v| v as i64).unwrap_or(0);
                                if is_copper_layer(layer) {
                                    if let (Some(x1), Some(y1), Some(x2), Some(y2)) =
                                        (num(&a, "x1"), num(&a, "y1"), num(&a, "x2"), num(&a, "y2"))
                                    {
                                        let (rot_deg, _) = parse_rot(
                                            a.get("rot").map(String::as_str).unwrap_or("R0"),
                                        );
                                        out.signals[si].1.push(SignalGeom::Rect {
                                            x1,
                                            y1,
                                            x2,
                                            y2,
                                            rot_deg,
                                            layer,
                                        });
                                    }
                                }
                            }
                        }
                        b"circle" => {
                            if let Some(si) = cur_signal {
                                let layer = num(&a, "layer").map(|v| v as i64).unwrap_or(0);
                                if is_copper_layer(layer) {
                                    if let (Some(x), Some(y), Some(r)) =
                                        (num(&a, "x"), num(&a, "y"), num(&a, "radius"))
                                    {
                                        out.signals[si].1.push(SignalGeom::Circle {
                                            x,
                                            y,
                                            radius: r,
                                            width: num(&a, "width").unwrap_or(0.0),
                                            layer,
                                        });
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::End(e)) => match e.name().as_ref() {
                    b"library" => cur_library.clear(),
                    b"package" => cur_package = None,
                    b"signal" => cur_signal = None,
                    b"class" => cur_class = None,
                    b"polygon" => {
                        if let Some(poly) = cur_poly.take() {
                            if poly.verts.len() >= 3 {
                                out.signals[poly.signal].1.push(SignalGeom::Polygon {
                                    width: poly.width,
                                    layer: poly.layer,
                                    isolate: poly.isolate,
                                    rank: poly.rank,
                                    thermals: poly.thermals,
                                    orphans: poly.orphans,
                                    cutout: poly.cutout,
                                    verts: poly.verts,
                                });
                            }
                        }
                    }
                    _ => {}
                },
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
            buf.clear();
        }

        // Resolve the design-rule clearance: take the tightest of the spacing
        // rules that actually gate copper-to-copper (wire/pad/via/smd), so we do
        // not manufacture noise by using a looser rule than the board allows.
        let dr_keys = [
            "mdWireWire",
            "mdWirePad",
            "mdWireVia",
            "mdPadPad",
            "mdPadVia",
            "mdViaVia",
            "mdSmdPad",
            "mdSmdVia",
            "mdSmdSmd",
        ];
        let mut tightest: Option<f64> = None;
        for k in dr_keys {
            if let Some(v) = params.get(k).and_then(|s| parse_len_mm(s)) {
                if v > 0.0 {
                    tightest = Some(tightest.map_or(v, |t: f64| t.min(v)));
                }
            }
        }
        out.clearance_mm = tightest;

        // Via restring rule (for vias with no explicit diameter).
        let mut vr = ViaRestring::default();
        if let Some(v) = params.get("rvViaOuter").and_then(|s| s.parse::<f64>().ok()) {
            vr.rv = v;
        }
        if let Some(v) = params.get("rlMinViaOuter").and_then(|s| parse_len_mm(s)) {
            vr.min_mm = v;
        }
        if let Some(v) = params.get("rlMaxViaOuter").and_then(|s| parse_len_mm(s)) {
            vr.max_mm = v;
        }
        out.via_restring = vr;

        let valid_percent = |key: &str| {
            params
                .get(key)
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v >= 0.0)
        };
        if let Some(v) = valid_percent("psElongationLong") {
            out.pad_elongation_long_pct = v;
        }
        if let Some(v) = valid_percent("psElongationOffset") {
            out.pad_elongation_offset_pct = v;
        }

        out
    }

    /// Flatten an Eagle `curve` arc (chord endpoints + signed included angle in
    /// degrees) into a chain of capsule links of the given radius, using the
    /// standard [`ARC_SEGMENTS`] board-wire density.
    fn flatten_curve(x1: f64, y1: f64, x2: f64, y2: f64, curve_deg: f64, r: f64) -> Vec<Capsule> {
        flatten_curve_n(x1, y1, x2, y2, curve_deg, r, ARC_SEGMENTS)
    }

    /// As [`flatten_curve`], with an explicit segment count. Pour outlines use
    /// a sagitta-bounded count (see [`super::covering_segments`]) so the
    /// same-rank overlap test is not fooled by chord sag on curved edges.
    fn flatten_curve_n(
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        curve_deg: f64,
        r: f64,
        n: usize,
    ) -> Vec<Capsule> {
        if curve_deg.abs() < 1e-6 {
            return vec![Capsule {
                ax: x1,
                ay: y1,
                bx: x2,
                by: y2,
                r,
            }];
        }
        // The included angle is `curve_deg`; the chord subtends it. Centre lies
        // off the chord midpoint by the sagitta direction.
        let theta = curve_deg.to_radians();
        let mx = (x1 + x2) / 2.0;
        let my = (y1 + y2) / 2.0;
        let dx = x2 - x1;
        let dy = y2 - y1;
        let chord = (dx * dx + dy * dy).sqrt();
        if chord < 1e-9 {
            return vec![Capsule {
                ax: x1,
                ay: y1,
                bx: x2,
                by: y2,
                r,
            }];
        }
        let radius = (chord / 2.0) / (theta / 2.0).sin().abs();
        // Distance from chord midpoint to centre.
        let h = (radius * radius - (chord / 2.0).powi(2)).max(0.0).sqrt();
        // Unit normal to the chord. The centre sits a distance `h` along the
        // normal from the midpoint, but on which side depends on the sign of the
        // included angle AND on whether the arc is the major or minor one. Rather
        // than reason through every quadrant (and get the parallel diff-pair arcs
        // crossing, which manufactures false shorts), pick the side that actually
        // lands the sweep on the stated endpoint.
        let nx = -dy / chord;
        let ny = dx / chord;
        let pick = |s: f64| -> (f64, f64, f64, f64) {
            let cx = mx + s * h * nx;
            let cy = my + s * h * ny;
            let a0 = (y1 - cy).atan2(x1 - cx);
            let ex = cx + radius * (a0 + theta).cos();
            let ey = cy + radius * (a0 + theta).sin();
            let err = (ex - x2).hypot(ey - y2);
            (err, cx, cy, a0)
        };
        let (cx, cy, a0) = {
            let plus = pick(1.0);
            let minus = pick(-1.0);
            let best = if plus.0 <= minus.0 { plus } else { minus };
            (best.1, best.2, best.3)
        };
        let n = n.max(1);
        let mut caps = Vec::with_capacity(n);
        let mut prev = (x1, y1);
        for i in 1..=n {
            let t = i as f64 / n as f64;
            let a = a0 + theta * t;
            let p = (cx + radius * a.cos(), cy + radius * a.sin());
            caps.push(Capsule {
                ax: prev.0,
                ay: prev.1,
                bx: p.0,
                by: p.1,
                r,
            });
            prev = p;
        }
        caps
    }

    /// Build a regular-octagon polygon of "diameter" `d` (flat-to-flat across the
    /// bounding box) centred at `(cx, cy)`, rotated by `rot` radians.
    fn octagon(cx: f64, cy: f64, d: f64, rot: f64) -> Vec<(f64, f64)> {
        let r = d / 2.0;
        // Octagon vertices at 22.5° offsets, circumradius chosen so the flats
        // sit at ±r (matching Eagle's flat-to-flat pad size).
        let circum = r / (std::f64::consts::FRAC_PI_8).cos();
        let (rs, rc) = rot.sin_cos();
        (0..8)
            .map(|i| {
                let a = std::f64::consts::FRAC_PI_8 + i as f64 * std::f64::consts::FRAC_PI_4;
                let (lx, ly) = (circum * a.cos(), circum * a.sin());
                (cx + lx * rc - ly * rs, cy + lx * rs + ly * rc)
            })
            .collect()
    }

    /// Axis-aligned-then-rotated rectangle centred at `(cx, cy)` of size `w`×`h`.
    fn rect_pts(cx: f64, cy: f64, w: f64, h: f64, rot: f64) -> Vec<(f64, f64)> {
        let hw = w / 2.0;
        let hh = h / 2.0;
        let (rs, rc) = rot.sin_cos();
        [(-hw, -hh), (hw, -hh), (hw, hh), (-hw, hh)]
            .into_iter()
            .map(|(lx, ly)| (cx + lx * rc - ly * rs, cy + lx * rs + ly * rc))
            .collect()
    }

    /// Place a package item into world coordinates and push the resulting copper
    /// primitive(s) onto the right layer(s).
    fn place_pkg_item(
        item: &PkgItem,
        el: &Element,
        net: i64,
        copper_layers: &[i64],
        long_elongation_pct: f64,
        offset_elongation_pct: f64,
        buckets: &mut LayerBuckets,
    ) {
        // Element transform: local (lx, ly) → world. Eagle is y-up, rotation CCW.
        // A mirrored element (`MR<deg>`) is reflected about the Y axis (negate
        // local X) and then rotated CLOCKWISE by `deg` (mirroring flips the sense
        // of rotation). Equivalently: flip-X, then rotate by `-deg`.
        //
        // The earlier form used flip-Y with `+deg`. That is *identical* to
        // flip-X/`-deg` only when the rotation is a non-trivial multiple that
        // absorbs the sign (e.g. MR90), which is why it tested clean on the QT
        // Py's `MR90` SOIC8. But for `MR0` / `MR180` (no rotation to absorb the
        // sign) flip-Y and flip-X diverge, and flip-Y placed pads on the wrong
        // side of the package origin. That manufactured false shorts on the
        // SparkFun RP2040 Thing Plus, whose `MR0` micro-SD socket J6 then dropped
        // its SCLK/VCC/GND pads ~23 mm away, on top of the V_USB / EN bottom
        // traces. With flip-X/`-deg`, J6's SCLK pad lands at (10.51, 49.05),
        // exactly on its own SPI_SCK1 net copper (verified), and the QT Py `MR90`
        // placement is unchanged (the two transforms coincide there).
        let eff_rot = if el.mirrored { -el.rot_deg } else { el.rot_deg };
        let (esin, ecos) = eff_rot.to_radians().sin_cos();
        let to_world = |lx: f64, ly: f64| -> (f64, f64) {
            let mx = if el.mirrored { -lx } else { lx };
            (el.x + mx * ecos - ly * esin, el.y + mx * esin + ly * ecos)
        };
        match item {
            PkgItem::Pad {
                x,
                y,
                diameter,
                drill,
                shape,
                rot_deg,
                ..
            } => {
                let (cx, cy) = to_world(*x, *y);
                // Pads in the corpus all carry an explicit copper diameter; if one
                // is absent, fall back to a 0.2 mm annular ring over the drill.
                let d = if *diameter > 0.0 {
                    *diameter
                } else {
                    drill + 2.0 * 0.2032
                };
                // Transform the pad's local +X direction through the same
                // flip-X then rotate(-element) matrix as its position. Under a
                // mirror, angle p becomes 180-p before the element rotation.
                // The 180-degree term is invisible for symmetric long pads but
                // load-bearing for asymmetric `shape="offset"` pads.
                let pad_rot = if el.mirrored {
                    (180.0 - el.rot_deg - rot_deg).to_radians()
                } else {
                    (el.rot_deg + rot_deg).to_radians()
                };
                let shape = match shape.as_str() {
                    "square" => Shape::Polygon {
                        pts: rect_pts(cx, cy, d, d, pad_rot),
                        r: 0.0,
                    },
                    "octagon" => Shape::Polygon {
                        pts: octagon(cx, cy, d, pad_rot),
                        r: 0.0,
                    },
                    "long" => {
                        // EAGLE's `psElongationLong` is the percentage added to
                        // the circular diameter. The capsule segment contributes
                        // that added length; its radius contributes the base `d`.
                        let half = d * long_elongation_pct / 200.0;
                        let (rs, rc) = pad_rot.sin_cos();
                        let ax = cx - half * rc;
                        let ay = cy - half * rs;
                        let bx = cx + half * rc;
                        let by = cy + half * rs;
                        Shape::Capsule(Capsule {
                            ax,
                            ay,
                            bx,
                            by,
                            r: d / 2.0,
                        })
                    }
                    "offset" => {
                        // Offset: like "long" but the hole sits at one end, so the
                        // copper extends a full diameter to one side.
                        let (rs, rc) = pad_rot.sin_cos();
                        let extension = d * offset_elongation_pct / 100.0;
                        let bx = cx + extension * rc;
                        let by = cy + extension * rs;
                        Shape::Capsule(Capsule {
                            ax: cx,
                            ay: cy,
                            bx,
                            by,
                            r: d / 2.0,
                        })
                    }
                    // round (default): a disc.
                    _ => Shape::Capsule(Capsule {
                        ax: cx,
                        ay: cy,
                        bx: cx,
                        by: cy,
                        r: d / 2.0,
                    }),
                };
                for layer in copper_layers {
                    buckets.push(
                        &layer_name(*layer),
                        make_prim(shape.clone(), net, ItemKind::Pad, el.name.clone()),
                    );
                }
            }
            PkgItem::Smd {
                x,
                y,
                dx,
                dy,
                roundness,
                rot_deg,
                layer,
                ..
            } => {
                let (cx, cy) = to_world(*x, *y);
                // The smd's drawn layer (1 by default) flips with a mirrored
                // element.
                let layer = if el.mirrored {
                    mirror_layer(*layer)
                } else {
                    *layer
                };
                if !is_copper_layer(layer) {
                    return;
                }
                let smd_rot = if el.mirrored {
                    (180.0 - el.rot_deg - rot_deg).to_radians()
                } else {
                    (el.rot_deg + rot_deg).to_radians()
                };
                // Roundness is a percentage of the shorter side used as the
                // corner radius; carry it as an inflation radius on an inset rect
                // so the rounded copper is not overstated (same trick as KiCad
                // roundrect).
                let rr = (roundness / 100.0) * dx.min(*dy) / 2.0;
                let shape = Shape::Polygon {
                    pts: rect_pts(
                        cx,
                        cy,
                        (dx - 2.0 * rr).max(0.0),
                        (dy - 2.0 * rr).max(0.0),
                        smd_rot,
                    ),
                    r: rr,
                };
                buckets.push(
                    &layer_name(layer),
                    make_prim(shape, net, ItemKind::Pad, el.name.clone()),
                );
            }
        }
    }

    /// Run the geometric DRC on Eagle `.brd` text, feeding the shared engine.
    /// `clearance_override` forces the clearance rule (mm) when `Some`; otherwise
    /// the board's own design-rule clearance is used, falling back to
    /// [`DEFAULT_CLEARANCE_MM`] only when the board states none.
    pub fn run(text: &str, clearance_override: Option<f64>) -> DrcReport {
        let parsed = parse(text);
        if !parsed.saw_eagle {
            return DrcReport::default();
        }
        let clearance = clearance_override
            .or(parsed.clearance_mm)
            .unwrap_or(DEFAULT_CLEARANCE_MM);

        // Net ids: 1-based signal index, matching `eagle.rs`. Name lookup table.
        let names: Vec<String> = parsed.signals.iter().map(|(n, _)| n.clone()).collect();
        let name_of =
            |id: i64| -> String { names.get((id - 1) as usize).cloned().unwrap_or_default() };

        // The two-layer copper stack these boards use; a TH pad / via sits on all
        // of them. (Inner layers would be added here for a multilayer board, but
        // the famous corpus is all two-layer.)
        let copper_layers: Vec<i64> = vec![1, 16];

        let mut buckets = LayerBuckets::default();

        // Signal pours, kept aside for the pour-to-pour rank check (they are
        // NOT stamped as solid copper; see the Polygon arm below).
        struct Pour {
            net: i64,
            layer: i64,
            rank: i64,
            width: f64,
            isolate: f64,
            thermals: bool,
            orphans: bool,
            pts: Vec<(f64, f64)>,
        }
        let mut pours: Vec<Pour> = Vec::new();

        // ── Board-level signal geometry ──────────────────────────────────────
        for (si, (_, geoms)) in parsed.signals.iter().enumerate() {
            let net = si as i64 + 1;
            for g in geoms {
                match g {
                    SignalGeom::Wire {
                        x1,
                        y1,
                        x2,
                        y2,
                        width,
                        curve,
                        layer,
                    } => {
                        let r = width / 2.0;
                        for cap in flatten_curve(*x1, *y1, *x2, *y2, *curve, r) {
                            buckets.push(
                                &layer_name(*layer),
                                make_prim(Shape::Capsule(cap), net, ItemKind::Track, String::new()),
                            );
                        }
                    }
                    SignalGeom::Via {
                        x,
                        y,
                        diameter,
                        drill,
                        shape,
                    } => {
                        let d =
                            diameter.unwrap_or_else(|| parsed.via_restring.outer_diameter(*drill));
                        // A via spans every copper layer (`extent="1-16"`).
                        let prim_shape = if shape == "octagon" {
                            Shape::Polygon {
                                pts: octagon(*x, *y, d, 0.0),
                                r: 0.0,
                            }
                        } else {
                            Shape::Capsule(Capsule {
                                ax: *x,
                                ay: *y,
                                bx: *x,
                                by: *y,
                                r: d / 2.0,
                            })
                        };
                        for layer in &copper_layers {
                            buckets.push(
                                &layer_name(*layer),
                                make_prim(prim_shape.clone(), net, ItemKind::Via, String::new()),
                            );
                        }
                    }
                    SignalGeom::Polygon {
                        width,
                        layer,
                        isolate,
                        rank,
                        thermals,
                        orphans,
                        cutout,
                        verts,
                    } => {
                        // Signal polygon (copper pour). The `.brd` stores the
                        // pour's requested outline and its pour settings
                        // (`isolate`, `rank`, `thermals`, `orphans`, all
                        // parsed above); only the COMPUTED fill polygon is
                        // absent, because Eagle re-derives it on every
                        // ratsnest / CAM run. That derivation is what makes
                        // the fill safe to leave un-stamped against ordinary
                        // copper: Eagle carves max(isolate, applicable
                        // design-rule / net-class clearance) around every
                        // foreign-net wire, pad and via (an `isolate` below
                        // the rules distance is ignored), thermal spokes only
                        // remove same-net copper, and orphan removal only
                        // deletes fill pockets. Every setting keeps or widens
                        // gaps, so a fill Eagle derives from these settings
                        // cannot short or crowd foreign copper in the same
                        // file (pour-to-copper pairs are therefore left
                        // unchecked, not checked-and-clean), while treating the drawn
                        // outline as solid copper would turn every legitimate
                        // crossing track and every isolated foreign pad into a
                        // false short. The one construct the settings CANNOT
                        // make safe is two overlapping same-rank pours of
                        // different signals: Eagle pours both and flags the
                        // overlap as a DRC error, so the rank check below does
                        // the same.
                        if !cutout {
                            pours.push(Pour {
                                net,
                                layer: *layer,
                                rank: *rank,
                                width: *width,
                                isolate: *isolate,
                                thermals: *thermals,
                                orphans: *orphans,
                                pts: flatten_polygon(verts),
                            });
                        }
                    }
                    SignalGeom::Rect {
                        x1,
                        y1,
                        x2,
                        y2,
                        rot_deg,
                        layer,
                    } => {
                        let cx = (x1 + x2) / 2.0;
                        let cy = (y1 + y2) / 2.0;
                        let w = (x2 - x1).abs();
                        let h = (y2 - y1).abs();
                        let pts = rect_pts(cx, cy, w, h, rot_deg.to_radians());
                        buckets.push(
                            &layer_name(*layer),
                            make_prim(
                                Shape::Polygon { pts, r: 0.0 },
                                net,
                                ItemKind::Graphic,
                                String::new(),
                            ),
                        );
                    }
                    SignalGeom::Circle {
                        x,
                        y,
                        radius,
                        width,
                        layer,
                    } => {
                        // A drawn circle on copper. With a nonzero stroke width
                        // the copper is ONLY the annulus of that stroke at
                        // `radius`; the interior is bare board, and stamping a
                        // solid disc manufactures phantom shorts against copper
                        // legitimately routed through the hole. Eagle renders a
                        // zero-width circle as a filled disc, so only that case
                        // is solid. The chain is sagitta-bounded and inflated
                        // to COVER the true annulus (see `ring_capsules`), so
                        // a grazing short on the ring edge is not lost to
                        // chord flattening.
                        if *width > 0.0 {
                            for cap in super::ring_capsules(*x, *y, *radius, width / 2.0) {
                                buckets.push(
                                    &layer_name(*layer),
                                    make_prim(
                                        Shape::Capsule(cap),
                                        net,
                                        ItemKind::Graphic,
                                        String::new(),
                                    ),
                                );
                            }
                        } else {
                            buckets.push(
                                &layer_name(*layer),
                                make_prim(
                                    Shape::Capsule(Capsule {
                                        ax: *x,
                                        ay: *y,
                                        bx: *x,
                                        by: *y,
                                        r: *radius,
                                    }),
                                    net,
                                    ItemKind::Graphic,
                                    String::new(),
                                ),
                            );
                        }
                    }
                }
            }
        }

        // ── Placed package copper (pads + smds) ──────────────────────────────
        // (element, pad-name) → net, from contactrefs. We re-read those here via
        // the connectivity extractor's mapping so the geometry carries nets.
        let pad_net = pad_net_map(text, &parsed);
        let mut net_ties = NetTieOwners::default();
        for el in &parsed.elements {
            if crate::dnp::is_eagle_copper_link_fields(&el.value, &el.library, &el.package) {
                net_ties.insert(
                    el.name.clone(),
                    pad_net
                        .iter()
                        .filter_map(|((owner, _), net)| (owner == &el.name).then_some(*net)),
                );
            }
        }
        for el in &parsed.elements {
            let Some(items) = parsed
                .packages
                .get(&(el.library.clone(), el.package.clone()))
            else {
                continue;
            };
            for item in items {
                let net = pad_net
                    .get(&(el.name.clone(), item.name().to_string()))
                    .copied()
                    .unwrap_or(0);
                place_pkg_item(
                    item,
                    el,
                    net,
                    &copper_layers,
                    parsed.pad_elongation_long_pct,
                    parsed.pad_elongation_offset_pct,
                    &mut buckets,
                );
            }
        }
        net_ties.capture_geometry(&buckets);

        // Net 0 (no contactref → unconnected pad) carries no connectivity, so it
        // is never a short, exactly like KiCad's net 0.
        let no_net: std::collections::HashSet<i64> = std::iter::once(0).collect();

        // ── Net-class clearances ─────────────────────────────────────────────
        // Eagle's `<classes>` block is a clearance matrix: class N declares
        // `<clearance class="M" value="V"/>` rows (its own number included).
        // Eagle's rule set is "the larger value applies" throughout: a class
        // value below the design-rule clearance is ignored (so every stored
        // value is floored at the design rules), and when nets of DIFFERENT
        // classes meet with no explicit matrix cell (absent or 0), the larger
        // of the two classes' own clearances governs, which is exactly the
        // max-of-two-classes fallback in `effective_clearance`, so only
        // explicit non-zero matrix cells are registered as pair overrides.
        //
        // "The design rules" here is this path's single clearance: the
        // TIGHTEST copper-gating md* value (resolved above). The Eagle engine
        // deliberately models one clearance rather than the per-item-kind md*
        // matrix (mdWireWire vs mdPadPad, ...), the documented
        // no-manufactured-noise choice this DRC has always made; a class or
        // pair value is therefore floored at that single rule, not at the
        // item-kind-specific one. Modelling the full md* matrix would be a
        // separate feature touching every finding, not a net-class concern.
        let mut rules = ClearanceRules::new(clearance);
        let class_key = |n: i64| format!("class-{n}");
        for (number, class) in &parsed.classes {
            let self_clearance = class
                .clearances
                .iter()
                .find(|(other, _)| other == number)
                .map(|(_, v)| *v)
                .unwrap_or(0.0);
            rules.add_class(NetClassRule {
                name: class_key(*number),
                clearance_mm: self_clearance.max(clearance),
                diff_pair_gap_mm: None,
            });
            for (other, value) in &class.clearances {
                if other != number && *value > 0.0 {
                    rules.add_class_pair_clearance(
                        &class_key(*number),
                        &class_key(*other),
                        value.max(clearance),
                    );
                }
            }
        }
        for (si, (name, _)) in parsed.signals.iter().enumerate() {
            let class = parsed.signal_classes.get(si).copied().unwrap_or(0);
            if parsed.classes.contains_key(&class) {
                rules.assign_net(name, &class_key(class));
            }
        }

        let mut report = sweep_buckets(buckets, &rules, &no_net, &net_ties, &name_of);

        // ── Pour-to-pour rank arbitration ────────────────────────────────────
        // Two overlapping pours of different signals with the SAME rank have no
        // arbitration: Eagle pours both (a physical short on the fabricated
        // board) and its own DRC reports the overlap. Differing ranks are
        // arbitrated (the higher-numbered pour carves around the lower), so
        // they stay silent.
        for (i, a) in pours.iter().enumerate() {
            for b in pours.iter().skip(i + 1) {
                if a.layer != b.layer
                    || a.net == b.net
                    || a.rank != b.rank
                    || no_net.contains(&a.net)
                    || no_net.contains(&b.net)
                {
                    continue;
                }
                // Overlap means the vertex rings themselves cross or one
                // contains the other. The rings are deliberately NOT inflated
                // by the pours' `width` strokes: whether Eagle's boundary
                // stroke extends past the vertex ring is renderer detail, and
                // inflating would invent shorts for near-but-disjoint pours.
                // Requiring true ring overlap keeps the short claim airtight
                // under either reading (Eagle's own DRC flags the polygon
                // overlap, not a stroke graze).
                let (gap, qa, _) = poly_poly_closest(&a.pts, &b.pts);
                let contained = a
                    .pts
                    .first()
                    .is_some_and(|&(x, y)| point_in_polygon(x, y, &b.pts))
                    || b.pts
                        .first()
                        .is_some_and(|&(x, y)| point_in_polygon(x, y, &a.pts));
                if !contained && !is_touching(gap) {
                    continue;
                }
                let (x, y) = if is_touching(gap) {
                    qa
                } else if a
                    .pts
                    .first()
                    .is_some_and(|&(px, py)| point_in_polygon(px, py, &b.pts))
                {
                    a.pts[0]
                } else {
                    b.pts[0]
                };
                let pour_item = |p: &Pour| Item {
                    kind: ItemKind::Zone,
                    net: p.net,
                    owner: format!(
                        "pour(rank {}, width {} mm, isolate {} mm, thermals {}, orphans {})",
                        p.rank,
                        p.width,
                        p.isolate,
                        if p.thermals { "on" } else { "off" },
                        if p.orphans { "on" } else { "off" },
                    ),
                };
                let (na, nb) = (a.net.min(b.net), a.net.max(b.net));
                let (item_a, item_b) = if a.net <= b.net {
                    (pour_item(a), pour_item(b))
                } else {
                    (pour_item(b), pour_item(a))
                };
                report.findings.push(DrcFinding {
                    kind: ViolationKind::Short,
                    net_a: na,
                    net_b: nb,
                    net_a_name: name_of(na),
                    net_b_name: name_of(nb),
                    layer: layer_name(a.layer),
                    x,
                    y,
                    gap_mm: gap.min(0.0),
                    required_clearance_mm: rules
                        .effective_clearance(&name_of(a.net), &name_of(b.net)),
                    item_a,
                    item_b,
                });
            }
        }
        report.findings.sort_by(|a, b| {
            (a.kind == ViolationKind::Clearance)
                .cmp(&(b.kind == ViolationKind::Clearance))
                .then(a.net_a.cmp(&b.net_a))
                .then(a.net_b.cmp(&b.net_b))
                .then(a.layer.cmp(&b.layer))
        });
        report
    }

    /// Flatten a polygon's vertex ring, expanding any per-vertex `curve` (the arc
    /// from this vertex to the next) into intermediate points. Curves are
    /// expanded at sagitta-bounded density (points ON the true arc, chord sag
    /// at most [`super::RING_SAGITTA_MM`]) rather than the coarse board-wire
    /// [`ARC_SEGMENTS`]: the ring feeds the same-rank pour overlap test, where
    /// a coarse chord sagging inward (or bulging outward, for the opposite
    /// curve sign) by `~0.02 * radius` would miss or invent an overlap.
    fn flatten_polygon(verts: &[(f64, f64, f64)]) -> Vec<(f64, f64)> {
        let n = verts.len();
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let (x1, y1, curve) = verts[i];
            let (x2, y2, _) = verts[(i + 1) % n];
            out.push((x1, y1));
            if curve.abs() > 1e-6 {
                let theta = curve.to_radians().abs();
                let chord = (x2 - x1).hypot(y2 - y1);
                let segments = if (theta / 2.0).sin().abs() > 1e-12 {
                    let radius = (chord / 2.0) / (theta / 2.0).sin();
                    super::covering_segments(radius, theta)
                } else {
                    ARC_SEGMENTS
                };
                for cap in flatten_curve_n(x1, y1, x2, y2, curve, 0.0, segments) {
                    out.push((cap.bx, cap.by));
                }
                // The last pushed point coincides with the next vertex; drop it
                // so we do not duplicate (the loop pushes (x2,y2) next iteration).
                out.pop();
            }
        }
        out
    }

    /// (element, pad-name) → net id, read from `<contactref>` inside `<signal>`.
    fn pad_net_map(text: &str, parsed: &Parsed) -> HashMap<(String, String), i64> {
        let mut reader = Reader::from_str(text);
        reader.config_mut().trim_text(true);
        let mut map = HashMap::new();
        let mut cur_signal: Option<usize> = None;
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                    let a = attrs_of(&e);
                    match e.name().as_ref() {
                        b"signal" => {
                            let name = a.get("name").cloned().unwrap_or_default();
                            cur_signal = parsed.signals.iter().position(|(n, _)| *n == name);
                        }
                        b"contactref" => {
                            if let (Some(si), Some(el), Some(pad)) =
                                (cur_signal, a.get("element"), a.get("pad"))
                            {
                                map.insert((el.clone(), pad.clone()), si as i64 + 1);
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::End(e)) if e.name().as_ref() == b"signal" => cur_signal = None,
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
            buf.clear();
        }
        map
    }
}

// ── Altium .PcbDoc geometry → the same engine ─────────────────────────────────

/// Altium `.PcbDoc` copper-geometry extraction for the DRC. The connectivity
/// extractor in `altium.rs` reads pad nets; this reads *copper geometry per net*
/// (tracks, arcs, vias, pads, polygon outlines) straight from the binary record
/// streams and feeds it to `sweep_buckets`, the exact same short / clearance
/// engine the KiCad and Eagle paths use.
///
/// Geometry is kept in Altium's native frame (millimetres, y-up); the DRC is
/// self-consistent so the y orientation never matters, only relative positions.
///
/// Record layouts ported from KiCad's `altium_parser_pcb.cpp` (`ATRACK6`,
/// `AARC6`, `AVIA6`, `APAD6`); see `altium.rs` for the field-by-field citation.
pub mod altium_drc {
    use super::{
        chamfered_rect_polygon, make_prim, sweep_buckets, Capsule, ClearanceRules, DrcReport,
        ItemKind, LayerBuckets, NetTieOwners, Shape, ARC_SEGMENTS, DEFAULT_CLEARANCE_MM,
    };
    use crate::altium::{self, is_copper_layer, layer_name, parse_pads, MM_PER_UNIT, NONE_U16};
    use crate::ExtractError;
    use std::collections::{HashMap, HashSet};

    /// Map an Altium primitive's net field to the hauksbee net id (index + 1, so
    /// id 0 stays the "no net" bucket), or 0 when unattached / out of range.
    fn net_id(field: u16, n_nets: usize) -> i64 {
        if field == NONE_U16 || (field as usize) >= n_nets {
            0
        } else {
            field as i64 + 1
        }
    }

    /// The copper layers a through-hole (multi-layer) primitive occupies: front,
    /// back, and every inner layer the board declares. For a two-layer board this
    /// is just F.Cu + B.Cu.
    fn multi_layers(copper: &HashSet<String>) -> Vec<String> {
        let mut v: Vec<String> = copper.iter().cloned().collect();
        if v.is_empty() {
            v = vec!["F.Cu".to_string(), "B.Cu".to_string()];
        }
        v
    }

    pub fn run(bytes: &[u8], clearance_override: Option<f64>) -> Result<DrcReport, ExtractError> {
        let mut doc = altium::PcbDoc::open(bytes)?;
        let clearance = clearance_override.unwrap_or(DEFAULT_CLEARANCE_MM);

        // Net names (for the report); a primitive's net field indexes this list.
        let net_names: Vec<String> = doc
            .data("Nets")
            .map(|b| altium::parse_net_names(&b))
            .unwrap_or_default();
        let n_nets = net_names.len();
        let name_of = |id: i64| -> String {
            if id == 0 {
                String::new()
            } else {
                net_names
                    .get((id - 1) as usize)
                    .cloned()
                    .unwrap_or_default()
            }
        };

        // Canonical component identity and native Component Type by index, so
        // geometry ownership is channel-aware (same identity path as
        // extraction, split placements included) and only Altium's explicit
        // `Net Tie` / `Net Tie (In BOM)` types can receive a local exemption.
        let component_data = doc.data("Components");
        let pad_data = doc.data("Pads");
        let comp_identities: Vec<altium::DrcComponentIdentity> = component_data
            .map(|components| {
                altium::parse_drc_component_identities(&components, pad_data.as_deref())
            })
            .unwrap_or_default();
        let owner_of = |idx: u16| -> String {
            if idx == NONE_U16 {
                String::new()
            } else {
                comp_identities
                    .get(idx as usize)
                    .map(|identity| identity.reference.clone())
                    .unwrap_or_default()
            }
        };
        let explicit_tie_owners: HashSet<String> = comp_identities
            .iter()
            .filter(|identity| identity.is_net_tie)
            .map(|identity| identity.reference.clone())
            .collect();

        let mut buckets = LayerBuckets::default();
        let mut copper: HashSet<String> = HashSet::new();
        copper.insert("F.Cu".to_string());
        copper.insert("B.Cu".to_string());

        // ── Tracks ──────────────────────────────────────────────────────────
        if let Some(b) = doc.data("Tracks") {
            for t in parse_tracks(&b) {
                if !is_copper_layer(t.layer) {
                    continue;
                }
                let layer = layer_name(t.layer);
                copper.insert(layer.clone());
                let net = net_id(t.net, n_nets);
                buckets.push(
                    &layer,
                    make_prim(
                        Shape::Capsule(Capsule {
                            ax: t.x1,
                            ay: t.y1,
                            bx: t.x2,
                            by: t.y2,
                            r: t.width / 2.0,
                        }),
                        net,
                        ItemKind::Track,
                        owner_of(t.component),
                    ),
                );
            }
        }

        // ── Arcs ────────────────────────────────────────────────────────────
        if let Some(b) = doc.data("Arcs") {
            for a in parse_arcs(&b) {
                if !is_copper_layer(a.layer) {
                    continue;
                }
                let layer = layer_name(a.layer);
                copper.insert(layer.clone());
                let net = net_id(a.net, n_nets);
                let owner = owner_of(a.component);
                for cap in flatten_altium_arc(&a) {
                    buckets.push(
                        &layer,
                        make_prim(Shape::Capsule(cap), net, ItemKind::Arc, owner.clone()),
                    );
                }
            }
        }

        // ── Vias (through-hole: present on every copper layer) ──────────────
        if let Some(b) = doc.data("Vias") {
            let layers = multi_layers(&copper);
            for v in parse_vias(&b) {
                let net = net_id(v.net, n_nets);
                for layer in &layers {
                    buckets.push(
                        layer,
                        make_prim(
                            Shape::Capsule(Capsule {
                                ax: v.x,
                                ay: v.y,
                                bx: v.x,
                                by: v.y,
                                r: v.diameter / 2.0,
                            }),
                            net,
                            ItemKind::Via,
                            String::new(),
                        ),
                    );
                }
            }
        }

        // ── Pads ────────────────────────────────────────────────────────────
        let mut net_ties = NetTieOwners::default();
        let mut net_tie_nets: HashMap<String, HashSet<i64>> = HashMap::new();
        if let Some(b) = doc.data("Pads") {
            let pads = parse_pads(&b);
            let pad_geo = parse_pad_geometry(&b);
            let multi = multi_layers(&copper);
            for (p, g) in pads.iter().zip(pad_geo.iter()) {
                let net = net_id(p.net, n_nets);
                let owner = owner_of(p.component);
                if net != 0 && explicit_tie_owners.contains(&owner) {
                    net_tie_nets.entry(owner.clone()).or_default().insert(net);
                }
                // Through-hole pads sit on the multi-layer slot; SMD pads on one
                // copper side.
                let layers: Vec<String> = if p.layer == crate::altium::ALTIUM_MULTI_LAYER {
                    multi.clone()
                } else if is_copper_layer(p.layer) {
                    vec![layer_name(p.layer)]
                } else {
                    continue;
                };
                let shape = pad_shape(p.x_mm, p.y_mm, g);
                for layer in &layers {
                    copper.insert(layer.clone());
                    buckets.push(
                        layer,
                        make_prim(shape.clone(), net, ItemKind::Pad, owner.clone()),
                    );
                }
            }
        }
        for (owner, nets) in net_tie_nets {
            net_ties.insert(owner, nets);
        }
        net_ties.capture_geometry(&buckets);

        // ── Polygon (copper-pour) outlines ──────────────────────────────────
        // Altium stores the requested outline; the filled copper with its
        // antipads lives in Regions6 which we do not model. Push the outline as a
        // containment-only, edge-less zone (see `push_zone_opts`) so a foreign
        // via passing through a split-plane antipad does not read as a short.
        if let Some(b) = doc.data("Polygons") {
            for poly in parse_polygons(&b) {
                if !is_copper_layer(poly.layer) || poly.pts.len() < 3 {
                    continue;
                }
                let layer = layer_name(poly.layer);
                copper.insert(layer.clone());
                let net = net_id(poly.net, n_nets);
                buckets.push_zone_opts(&layer, poly.pts, net, false, false);
            }
        }

        // Net 0 carries no connectivity, so it is never a short (KiCad net 0).
        let no_net: HashSet<i64> = std::iter::once(0).collect();

        let rules = ClearanceRules::new(clearance);
        Ok(sweep_buckets(buckets, &rules, &no_net, &net_ties, name_of))
    }

    // ── Binary record parsers (fixed layout, little-endian) ───────────────────

    fn u8_at(b: &[u8], o: usize) -> u8 {
        b.get(o).copied().unwrap_or(0)
    }
    fn u16_at(b: &[u8], o: usize) -> u16 {
        if o + 2 <= b.len() {
            u16::from_le_bytes([b[o], b[o + 1]])
        } else {
            0
        }
    }
    fn u32_at(b: &[u8], o: usize) -> u32 {
        if o + 4 <= b.len() {
            u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
        } else {
            0
        }
    }
    fn coord_mm(b: &[u8], o: usize) -> f64 {
        (u32_at(b, o) as i32) as f64 * MM_PER_UNIT
    }
    fn f64_at(b: &[u8], o: usize) -> f64 {
        if o + 8 <= b.len() {
            let mut a = [0u8; 8];
            a.copy_from_slice(&b[o..o + 8]);
            f64::from_le_bytes(a)
        } else {
            0.0
        }
    }

    pub(crate) struct Track {
        pub layer: u8,
        pub net: u16,
        pub component: u16,
        pub x1: f64,
        pub y1: f64,
        pub x2: f64,
        pub y2: f64,
        pub width: f64,
    }

    /// TRACKS6: 1-byte marker (4) then one sub-record.
    fn parse_tracks(b: &[u8]) -> Vec<Track> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < b.len() {
            if u8_at(b, pos) != 4 {
                break;
            }
            pos += 1;
            let len = u32_at(b, pos) as usize;
            pos += 4;
            let s = pos;
            out.push(Track {
                layer: u8_at(b, s),
                net: u16_at(b, s + 3),
                component: u16_at(b, s + 7),
                x1: coord_mm(b, s + 13),
                y1: coord_mm(b, s + 17),
                x2: coord_mm(b, s + 21),
                y2: coord_mm(b, s + 25),
                width: coord_mm(b, s + 29),
            });
            pos = (s + len).min(b.len());
        }
        out
    }

    pub(crate) struct Arc {
        pub layer: u8,
        pub net: u16,
        pub component: u16,
        pub cx: f64,
        pub cy: f64,
        pub radius: f64,
        pub start_deg: f64,
        pub end_deg: f64,
        pub width: f64,
    }

    /// ARCS6: 1-byte marker (1) then one sub-record.
    fn parse_arcs(b: &[u8]) -> Vec<Arc> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < b.len() {
            if u8_at(b, pos) != 1 {
                break;
            }
            pos += 1;
            let len = u32_at(b, pos) as usize;
            pos += 4;
            let s = pos;
            out.push(Arc {
                layer: u8_at(b, s),
                net: u16_at(b, s + 3),
                component: u16_at(b, s + 7),
                cx: coord_mm(b, s + 13),
                cy: coord_mm(b, s + 17),
                radius: coord_mm(b, s + 21),
                start_deg: f64_at(b, s + 25),
                end_deg: f64_at(b, s + 33),
                width: coord_mm(b, s + 41),
            });
            pos = (s + len).min(b.len());
        }
        out
    }

    /// Flatten an Altium arc (centre, radius, start/end angle in degrees) into a
    /// chain of capsule links of half-width radius.
    fn flatten_altium_arc(a: &Arc) -> Vec<Capsule> {
        let r = a.width / 2.0;
        let mut sweep = a.end_deg - a.start_deg;
        // Altium arcs go counter-clockwise from start to end; normalise to a
        // positive sweep.
        while sweep <= 0.0 {
            sweep += 360.0;
        }
        while sweep > 360.0 {
            sweep -= 360.0;
        }
        let start = a.start_deg.to_radians();
        let sweep = sweep.to_radians();
        let mut caps = Vec::with_capacity(ARC_SEGMENTS);
        let mut prev = (a.cx + a.radius * start.cos(), a.cy + a.radius * start.sin());
        for i in 1..=ARC_SEGMENTS {
            let t = i as f64 / ARC_SEGMENTS as f64;
            let ang = start + sweep * t;
            let p = (a.cx + a.radius * ang.cos(), a.cy + a.radius * ang.sin());
            caps.push(Capsule {
                ax: prev.0,
                ay: prev.1,
                bx: p.0,
                by: p.1,
                r,
            });
            prev = p;
        }
        caps
    }

    pub(crate) struct Via {
        pub net: u16,
        pub x: f64,
        pub y: f64,
        pub diameter: f64,
    }

    /// VIAS6: 1-byte marker (3) then one sub-record. Vias carry no component.
    fn parse_vias(b: &[u8]) -> Vec<Via> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < b.len() {
            if u8_at(b, pos) != 3 {
                break;
            }
            pos += 1;
            let len = u32_at(b, pos) as usize;
            pos += 4;
            let s = pos;
            out.push(Via {
                net: u16_at(b, s + 3),
                x: coord_mm(b, s + 13),
                y: coord_mm(b, s + 17),
                diameter: coord_mm(b, s + 21),
            });
            pos = (s + len).min(b.len());
        }
        out
    }

    /// Per-pad geometry (size + shape) read from sub-record 5, parallel to the
    /// connectivity `parse_pads`.
    pub(crate) struct PadGeo {
        pub size_x: f64,
        pub size_y: f64,
        pub shape: u8,
        pub rotation: f64,
    }

    fn parse_pad_geometry(b: &[u8]) -> Vec<PadGeo> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < b.len() {
            if u8_at(b, pos) != 2 {
                break;
            }
            pos += 1;
            // sub1 (name)..sub4 skipped
            for _ in 0..4 {
                let len = u32_at(b, pos) as usize;
                pos += 4 + len;
            }
            // sub5 geometry
            let sr5 = u32_at(b, pos) as usize;
            pos += 4;
            let s = pos;
            out.push(PadGeo {
                size_x: coord_mm(b, s + 21),
                size_y: coord_mm(b, s + 25),
                shape: u8_at(b, s + 49),
                rotation: f64_at(b, s + 52),
            });
            pos = s + sr5;
            // sub6 stack skipped
            let sr6 = u32_at(b, pos) as usize;
            pos += 4 + sr6;
        }
        out
    }

    /// Octagon corner cut, as a fraction of the pad's shorter side. Ported
    /// from KiCad's Altium importer (`altium_pcb.cpp`,
    /// `ALTIUM_PAD_SHAPE::OCTAGONAL` → chamfered rect, ratio 0.25, all
    /// corners), the same reference implementation the record layouts in this
    /// module are ported from. A regular octagon (ratio ≈ 0.293 on a square
    /// pad) would cut MORE copper, so 0.25 is the conservative reading if the
    /// two ever disagree.
    const OCTAGON_CHAMFER_RATIO: f64 = 0.25;

    /// Build the solid copper shape for a pad. Shape codes: 1 = circle/oval,
    /// 2 = rectangle, 3 = octagon (KiCad `ALTIUM_PAD_SHAPE`). Rectangles are
    /// exact; the octagon is the rectangle with each corner cut at 45° by
    /// [`OCTAGON_CHAMFER_RATIO`] of the shorter side, so copper legitimately
    /// routed past a cut corner is not a phantom short.
    fn pad_shape(cx: f64, cy: f64, g: &PadGeo) -> Shape {
        let rot = g.rotation.to_radians();
        let (w, h) = (g.size_x, g.size_y);
        match g.shape {
            1 if (w - h).abs() < 1e-6 => Shape::Capsule(Capsule {
                ax: cx,
                ay: cy,
                bx: cx,
                by: cy,
                r: w.max(h) / 2.0,
            }),
            1 => {
                // Oval / stadium: a capsule along the long axis.
                let (long, short) = if w >= h { (w, h) } else { (h, w) };
                let half = (long - short) / 2.0;
                let along = if w >= h {
                    rot
                } else {
                    rot + std::f64::consts::FRAC_PI_2
                };
                let (rs, rc) = along.sin_cos();
                Shape::Capsule(Capsule {
                    ax: cx - half * rc,
                    ay: cy - half * rs,
                    bx: cx + half * rc,
                    by: cy + half * rs,
                    r: short / 2.0,
                })
            }
            3 => {
                // Octagon: the rectangle with 45° corner cuts of
                // OCTAGON_CHAMFER_RATIO * min(w, h), built by the same
                // chamfered-rect constructor the KiCad path uses.
                let c = OCTAGON_CHAMFER_RATIO * w.min(h);
                let (rs, rc) = rot.sin_cos();
                let to_world = |lx: f64, ly: f64| (cx + lx * rc - ly * rs, cy + lx * rs + ly * rc);
                chamfered_rect_polygon(w, h, 0.0, c, [true; 4], &to_world)
            }
            _ => {
                // Rectangle (and unknown codes, conservatively): the rectangle.
                let hw = w / 2.0;
                let hh = h / 2.0;
                let (rs, rc) = rot.sin_cos();
                let pts = [(-hw, -hh), (hw, -hh), (hw, hh), (-hw, hh)]
                    .into_iter()
                    .map(|(lx, ly)| (cx + lx * rc - ly * rs, cy + lx * rs + ly * rc))
                    .collect();
                Shape::Polygon { pts, r: 0.0 }
            }
        }
    }

    pub(crate) struct Polygon {
        pub layer: u8,
        pub net: u16,
        pub pts: Vec<(f64, f64)>,
    }

    /// POLYGONS6: properties records; the outline is VX<i>/VY<i> vertices.
    fn parse_polygons(b: &[u8]) -> Vec<Polygon> {
        let mut out = Vec::new();
        for m in altium::properties_records(b) {
            let layer =
                altium::layer_id_from_name(m.get("LAYER").map(String::as_str).unwrap_or(""));
            let net = m
                .get("NET")
                .and_then(|s| s.trim().parse::<i64>().ok())
                .map(|v| if v < 0 { NONE_U16 } else { v as u16 })
                .unwrap_or(NONE_U16);
            let mut pts = Vec::new();
            let mut i = 0;
            loop {
                let vx = m.get(&format!("VX{i}"));
                let vy = m.get(&format!("VY{i}"));
                match (vx, vy) {
                    (Some(x), Some(y)) => {
                        if let (Some(x), Some(y)) =
                            (altium::parse_len_mm(x), altium::parse_len_mm(y))
                        {
                            pts.push((x, y));
                        }
                        i += 1;
                    }
                    _ => break,
                }
            }
            out.push(Polygon { layer, net, pts });
        }
        out
    }
}

#[cfg(test)]
mod netclass_glob_tests {
    use super::kicad_netclass_pattern_matches as m;

    #[test]
    fn glob_semantics_are_preserved() {
        // Literals and `?`.
        assert!(m("abc", "abc"));
        assert!(!m("abc", "abx"));
        assert!(!m("abc", "abcd"));
        assert!(m("/USB/USB_D?", "/USB/USB_D+"));
        assert!(!m("/USB/USB_D?", "/USB/USB_D"));
        // `*` at edges and interior.
        assert!(m("*", ""));
        assert!(m("*", "anything"));
        assert!(m("*abc*", "abc"));
        assert!(m("*abc*", "xxabcyy"));
        assert!(!m("*abc*", "xxabyy"));
        assert!(m("a*b*c", "aXXbYYc"));
        assert!(m("a*b*c", "abc"));
        assert!(!m("a*b*c", "aXXbYY"));
        assert!(m("a*", "a"));
        assert!(!m("a*", "b"));
        // Character classes (ranges + literals) and the unterminated-`[`
        // literal fallback.
        assert!(m("/DDR/ddr-a[0-9]", "/DDR/ddr-a7"));
        assert!(!m("/DDR/ddr-a[0-9]", "/DDR/ddr-ax"));
        assert!(m("[abc]x", "bx"));
        assert!(!m("[abc]x", "dx"));
        assert!(m("a[", "a["));
        assert!(!m("a[", "ab"));
    }

    #[test]
    fn pathological_star_pattern_terminates() {
        // A recursive matcher is exponential here (catastrophic
        // backtracking on `*`-heavy patterns from a crafted .kicad_pro); the
        // two-pointer matcher is O(pattern · net). Correctness only: it
        // terminates and returns false.
        let pattern = "*a".repeat(30);
        let net = format!("{}b", "a".repeat(500));
        assert!(!m(&pattern, &net));
        // And the matching counterpart still matches.
        assert!(m(&pattern, &"a".repeat(500)));
    }
}

#[cfg(test)]
mod version_warning_tests {
    use super::*;

    #[test]
    fn parses_format_version_from_header() {
        assert_eq!(
            kicad_pcb_format_version("(kicad_pcb (version 20260206) (generator pcbnew)"),
            Some(20260206)
        );
        assert_eq!(
            kicad_pcb_format_version("(kicad_pcb\n\t(version 20221018)\n"),
            Some(20221018)
        );
        assert_eq!(kicad_pcb_format_version("(eagle><drawing>"), None);
    }

    #[test]
    fn warns_only_on_unvalidated_kicad10_plus() {
        // KiCad 10 (20260206) → warn.
        assert!(unvalidated_version_warning("(kicad_pcb (version 20260206)").is_some());
        // KiCad 9 (20241229) and KiCad 7 (20221018) → no warning.
        assert!(unvalidated_version_warning("(kicad_pcb (version 20241229)").is_none());
        assert!(unvalidated_version_warning("(kicad_pcb (version 20221018)").is_none());
    }

    #[test]
    fn drc_report_carries_the_warning_for_kicad10() {
        // A minimal v20260206 board → the report flags the version even with no copper.
        let r = drc_from_text_with_clearance_rules(
            "(kicad_pcb (version 20260206) (generator x))",
            None,
        )
        .expect("parses");
        assert!(
            r.version_warning.is_some(),
            "KiCad 10 board must carry the caveat"
        );
        let r2 = drc_from_text_with_clearance_rules(
            "(kicad_pcb (version 20221018) (generator x))",
            None,
        )
        .expect("parses");
        assert!(r2.version_warning.is_none(), "validated board must not");
    }
}
