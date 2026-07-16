//! The declarative AS-BUILT overlay: the physical delta between a board's
//! design files and the real, reworked board on the bench.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/asbuilt.md.
//!
//! A `.asbuilt.toml` file describes BOARD state only — solder-blob shorts,
//! fitted component values, cut traces, lifted pins, jumper wires: the rework
//! a technician performed on copper. It deliberately does NOT describe
//! harness/scenario state (firmware-programmed registers, injected drives,
//! held enable nets): that is per-run configuration and lives where harness
//! config lives (the CI spec, the co-sim call sites). The line is physical:
//! if undoing it needs a soldering iron, it belongs here; if undoing it needs
//! a reset button, it does not.
//!
//! The vocabulary (each an array-of-tables):
//!
//! - `[[replace]]` — a component value swap or retune: `ref` names the device
//!   (substring match on the bound device name), `set` carries the fitted
//!   values (`ohms`, `farads`, `von`, `voff`), optional `was` records the
//!   removed part's value and doubles as a match refinement (±1 % relative),
//!   so a rework note like "removed the 10 pF, fitted 5.8 nF" is executable.
//! - `[[cut]]` — severed traces: every device terminal of the named role
//!   (`base`/`collector`/`emitter`) on a net matching `net_glob` is moved to
//!   a fresh floating node (floating beats grounded for cut terminals;
//!   dev-plan 02 §6). `expect_matches` pins the severed-terminal count.
//! - `[[lift]]` — one lifted pin: like `cut` but for a single named device.
//! - `[[jumper]]` — a bodge wire: net `to` is merged onto net `from`.
//!
//! Validation is fail-loud in the house style (`hauksbee-ci`'s spec/error
//! conventions): unknown TOML keys are rejected (`deny_unknown_fields`),
//! unknown refs/nets name the offending line and suggest near matches, and
//! every entry must match exactly the number of devices/terminals it declares
//! (default 1) — a rework that silently applies to nothing or to twice as
//! much hardware as documented is a lie about the physical board.
//!
//! Application order is fixed and semantic: `replace` entries in file order,
//! then `cut`, then `lift`, then `jumper`. All are pure [`BoundBoard`]
//! mutations — no transient, no DC — preserving the prep-stays-solve-free
//! invariant `tarski_prep` established.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use toml::Spanned;

use hauksbee_ir::{Device, NodeId};

use crate::binder::BoundBoard;

/// First node id handed to floated (cut/lifted) terminals. Must clear the
/// binder's node namespace entirely — [`apply`](AsBuiltOverlay::apply) fails
/// loudly if any bound node reaches it. The value is the historical base
/// `tarski_prep::fault1_cut` used, kept so the overlay's result is
/// byte-identical to the proven imperative surgery.
pub const FLOAT_NODE_BASE: u32 = 800_000;

/// Everything that can go wrong loading or applying an overlay. Messages are
/// self-contained: they carry the overlay origin (path), the 1-based line of
/// the offending entry, and near-match suggestions where a name was unknown.
#[derive(Debug, thiserror::Error)]
pub enum AsBuiltError {
    #[error("as-built overlay {origin}: TOML parse error: {source}")]
    Parse {
        origin: String,
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("as-built overlay {origin}: reading file: {source}")]
    Io {
        origin: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "as-built overlay {origin}:{line}: [[replace]] ref \"{reference}\" matches no device{}{}",
        if *.filtered_out > 0 {
            format!(" ({filtered_out} name match(es) exist but none carry the declared `was` value)")
        } else {
            String::new()
        },
        render_suggestions(.suggestions)
    )]
    UnknownRef {
        origin: String,
        line: usize,
        reference: String,
        filtered_out: usize,
        suggestions: Vec<String>,
    },
    #[error(
        "as-built overlay {origin}:{line}: {what} \"{pattern}\" matches no net on the bound board{}",
        render_suggestions(.suggestions)
    )]
    UnknownNet {
        origin: String,
        line: usize,
        what: &'static str,
        pattern: String,
        suggestions: Vec<String>,
    },
    #[error(
        "as-built overlay {origin}:{line}: {entry} matched {got} but the overlay declares \
         expect_matches = {expected} — the file no longer describes this board; refusing to apply"
    )]
    MatchCount {
        origin: String,
        line: usize,
        entry: String,
        expected: usize,
        got: usize,
    },
    #[error(
        "as-built overlay {origin}:{line}: [[replace]] ref \"{reference}\": key `{key}` does not \
         apply to matched device \"{device}\" ({variant}) — ohms needs a resistor, farads a \
         capacitor, von/voff a vswitch"
    )]
    KeyVariantMismatch {
        origin: String,
        line: usize,
        reference: String,
        key: &'static str,
        device: String,
        variant: &'static str,
    },
    #[error("as-built overlay {origin}:{line}: [[replace]] ref \"{reference}\": `set` is empty — an entry that changes nothing documents nothing")]
    EmptySet {
        origin: String,
        line: usize,
        reference: String,
    },
    #[error(
        "as-built overlay {origin}: the bound board already uses node id {max_seen}, at or above \
         the floating-node base {FLOAT_NODE_BASE}; cut/lift cannot allocate collision-free \
         floating nodes"
    )]
    NodeSpace { origin: String, max_seen: u32 },
    #[error("as-built overlay {origin}:{line}: [[jumper]] from and to name the same net \"{net}\" — a jumper to itself is a no-op and therefore a documentation error")]
    JumperSelf {
        origin: String,
        line: usize,
        net: String,
    },
}

fn render_suggestions(suggestions: &[String]) -> String {
    if suggestions.is_empty() {
        String::new()
    } else {
        format!(" — did you mean: {}?", suggestions.join(", "))
    }
}

/// The values a `[[replace]]` sets (or, as `was`, the values it removed).
/// Every key optional; unknown keys are a parse error.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetValues {
    pub ohms: Option<f64>,
    pub farads: Option<f64>,
    pub von: Option<f64>,
    pub voff: Option<f64>,
}

impl SetValues {
    fn is_empty(&self) -> bool {
        self.ohms.is_none() && self.farads.is_none() && self.von.is_none() && self.voff.is_none()
    }
    /// `was`-refinement: does `dev`'s current value match every declared key
    /// within ±1 % relative? (Keys that don't apply to the variant fail the
    /// match rather than erroring: `was` selects, `set` demands.)
    fn matches_current(&self, dev: &Device) -> bool {
        let near = |actual: f64, expected: f64| (actual - expected).abs() < 0.01 * expected.abs();
        let ok = |declared: Option<f64>, actual: Option<f64>| match declared {
            None => true,
            Some(exp) => actual.is_some_and(|act| near(act, exp)),
        };
        let (ohms, farads, von, voff) = match dev {
            Device::Resistor { ohms, .. } => (Some(*ohms), None, None, None),
            Device::Capacitor { farads, .. } => (None, Some(*farads), None, None),
            Device::VSwitch { von, voff, .. } => (None, None, Some(*von), Some(*voff)),
            _ => (None, None, None, None),
        };
        ok(self.ohms, ohms) && ok(self.farads, farads) && ok(self.von, von) && ok(self.voff, voff)
    }
}

/// One component swap/retune: the fitted `set` values land on every device
/// whose name contains `ref` (refined by `was` when present).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replace {
    #[serde(rename = "ref")]
    pub reference: Spanned<String>,
    #[serde(default)]
    pub was: Option<SetValues>,
    pub set: SetValues,
    /// Exact number of devices this entry must hit. Default 1: a rework note
    /// describes specific physical parts.
    #[serde(default)]
    pub expect_matches: Option<usize>,
    #[serde(default)]
    pub note: Option<String>,
}

/// Terminal role a `cut`/`lift` severs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Terminal {
    Base,
    Collector,
    Emitter,
}

impl Terminal {
    /// The mutable node this role names on `dev`, when the variant has it.
    fn node_mut<'d>(&self, dev: &'d mut Device) -> Option<&'d mut NodeId> {
        match (self, dev) {
            (Terminal::Base, Device::Bjt { b, .. }) => Some(b),
            (Terminal::Collector, Device::Bjt { c, .. }) => Some(c),
            (Terminal::Emitter, Device::Bjt { e, .. }) => Some(e),
            _ => None,
        }
    }
}

/// Severed traces: float every `terminal` of every device sitting on a net
/// matching `net_glob` (`*` wildcards).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cut {
    pub net_glob: Spanned<String>,
    pub terminal: Terminal,
    /// Exact number of terminals this cut must sever (default 1).
    #[serde(default)]
    pub expect_matches: Option<usize>,
    #[serde(default)]
    pub note: Option<String>,
}

/// One lifted pin: float `terminal` of the single device named by `ref`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lift {
    #[serde(rename = "ref")]
    pub reference: Spanned<String>,
    pub terminal: Terminal,
    #[serde(default)]
    pub note: Option<String>,
}

/// A bodge wire: net `to` is merged onto net `from` (every terminal on `to`
/// moves to `from`'s node; both names then resolve to it).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Jumper {
    pub from: Spanned<String>,
    pub to: Spanned<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// The `[board]` provenance block, in the `trace.toml` house style: what
/// physical board this delta describes and where the record comes from.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardMeta {
    /// The design file this overlay reworks (documentation; not resolved).
    pub netlist: Option<String>,
    pub provenance: Option<String>,
    pub date: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AsBuiltDoc {
    #[serde(default)]
    board: Option<BoardMeta>,
    #[serde(default, rename = "replace")]
    replaces: Vec<Replace>,
    #[serde(default, rename = "cut")]
    cuts: Vec<Cut>,
    #[serde(default, rename = "lift")]
    lifts: Vec<Lift>,
    #[serde(default, rename = "jumper")]
    jumpers: Vec<Jumper>,
}

/// A parsed overlay plus the source it came from (kept so semantic errors can
/// name the offending line, which the `Spanned` byte offsets index into).
#[derive(Debug, Clone)]
pub struct AsBuiltOverlay {
    doc: AsBuiltDoc,
    src: String,
    origin: String,
}

/// What an [`AsBuiltOverlay::apply`] did, one human line per entry, for the
/// CLI to print (the overlay equivalent of the bind report).
#[derive(Debug, Clone, Default)]
pub struct AsBuiltReport {
    pub lines: Vec<String>,
}

impl AsBuiltOverlay {
    /// Parse an overlay from TOML text. `origin` labels errors (a path, or a
    /// symbolic name for embedded overlays).
    pub fn parse(src: &str, origin: &str) -> Result<Self, AsBuiltError> {
        let doc: AsBuiltDoc = toml::from_str(src).map_err(|e| AsBuiltError::Parse {
            origin: origin.to_string(),
            source: Box::new(e),
        })?;
        Ok(Self {
            doc,
            src: src.to_string(),
            origin: origin.to_string(),
        })
    }

    /// Load an overlay from a file.
    pub fn load(path: &Path) -> Result<Self, AsBuiltError> {
        let origin = path.display().to_string();
        let src = std::fs::read_to_string(path).map_err(|e| AsBuiltError::Io {
            origin: origin.clone(),
            source: e,
        })?;
        Self::parse(&src, &origin)
    }

    /// The `[board]` provenance block, if the overlay carries one.
    pub fn board(&self) -> Option<&BoardMeta> {
        self.doc.board.as_ref()
    }

    /// 1-based line of a spanned field in the source text.
    fn line_of<T>(&self, spanned: &Spanned<T>) -> usize {
        let start = spanned.span().start.min(self.src.len());
        1 + self.src[..start].bytes().filter(|&b| b == b'\n').count()
    }

    /// Apply every entry to `bound`, in the fixed order replace → cut → lift
    /// → jumper. Fail-loud: the first entry that does not describe this board
    /// (unknown ref/net, match-count drift, key/variant mismatch) aborts with
    /// a line-numbered error and the board must be considered poisoned.
    pub fn apply(&self, bound: &mut BoundBoard) -> Result<AsBuiltReport, AsBuiltError> {
        let mut report = AsBuiltReport::default();
        for rep in &self.doc.replaces {
            self.apply_replace(rep, bound, &mut report)?;
        }
        // Floating-node allocation is shared across all cuts and lifts, in
        // file order, so ids are deterministic and collision-free.
        let mut next_float = self.check_node_space(bound)?;
        for cut in &self.doc.cuts {
            self.apply_cut(cut, bound, &mut next_float, &mut report)?;
        }
        for lift in &self.doc.lifts {
            self.apply_lift(lift, bound, &mut next_float, &mut report)?;
        }
        for jumper in &self.doc.jumpers {
            self.apply_jumper(jumper, bound, &mut report)?;
        }
        Ok(report)
    }

    fn check_node_space(&self, bound: &BoundBoard) -> Result<u32, AsBuiltError> {
        if self.doc.cuts.is_empty() && self.doc.lifts.is_empty() {
            return Ok(FLOAT_NODE_BASE);
        }
        let max_seen = bound
            .circuit
            .devices
            .iter()
            .flat_map(|d| d.nodes())
            .chain(bound.net_nodes.values().copied())
            .map(|n| n.0)
            .max()
            .unwrap_or(0);
        if max_seen >= FLOAT_NODE_BASE {
            return Err(AsBuiltError::NodeSpace {
                origin: self.origin.clone(),
                max_seen,
            });
        }
        Ok(FLOAT_NODE_BASE)
    }

    fn apply_replace(
        &self,
        rep: &Replace,
        bound: &mut BoundBoard,
        report: &mut AsBuiltReport,
    ) -> Result<(), AsBuiltError> {
        let line = self.line_of(&rep.reference);
        let reference = rep.reference.get_ref().as_str();
        if rep.set.is_empty() {
            return Err(AsBuiltError::EmptySet {
                origin: self.origin.clone(),
                line,
                reference: reference.to_string(),
            });
        }
        let mut name_matches = 0usize;
        let mut hits: Vec<usize> = Vec::new();
        for (i, dev) in bound.circuit.devices.iter().enumerate() {
            if !dev.name().contains(reference) {
                continue;
            }
            name_matches += 1;
            if rep.was.as_ref().is_none_or(|w| w.matches_current(dev)) {
                hits.push(i);
            }
        }
        if hits.is_empty() {
            let known: Vec<String> = bound
                .circuit
                .devices
                .iter()
                .map(|d| d.name().to_string())
                .collect();
            return Err(AsBuiltError::UnknownRef {
                origin: self.origin.clone(),
                line,
                reference: reference.to_string(),
                filtered_out: name_matches,
                suggestions: near_matches(reference, &known, 3),
            });
        }
        let expected = rep.expect_matches.unwrap_or(1);
        if hits.len() != expected {
            return Err(AsBuiltError::MatchCount {
                origin: self.origin.clone(),
                line,
                entry: format!("[[replace]] ref \"{reference}\" matched {} device(s)", hits.len()),
                expected,
                got: hits.len(),
            });
        }
        for &i in &hits {
            let dev = &mut bound.circuit.devices[i];
            let mismatch = |key: &'static str, dev: &Device| AsBuiltError::KeyVariantMismatch {
                origin: self.origin.clone(),
                line,
                reference: reference.to_string(),
                key,
                device: dev.name().to_string(),
                variant: variant_name(dev),
            };
            if let Some(v) = rep.set.ohms {
                match dev {
                    Device::Resistor { ohms, .. } => *ohms = v,
                    other => return Err(mismatch("ohms", other)),
                }
            }
            if let Some(v) = rep.set.farads {
                match dev {
                    Device::Capacitor { farads, .. } => *farads = v,
                    other => return Err(mismatch("farads", other)),
                }
            }
            if let Some(v) = rep.set.von {
                match dev {
                    Device::VSwitch { von, .. } => *von = v,
                    other => return Err(mismatch("von", other)),
                }
            }
            if let Some(v) = rep.set.voff {
                match dev {
                    Device::VSwitch { voff, .. } => *voff = v,
                    other => return Err(mismatch("voff", other)),
                }
            }
        }
        report.lines.push(format!(
            "replace {reference}: {} device(s) set {}",
            hits.len(),
            describe_set(&rep.set)
        ));
        Ok(())
    }

    fn apply_cut(
        &self,
        cut: &Cut,
        bound: &mut BoundBoard,
        next_float: &mut u32,
        report: &mut AsBuiltReport,
    ) -> Result<(), AsBuiltError> {
        let line = self.line_of(&cut.net_glob);
        let pattern = cut.net_glob.get_ref().as_str();
        let matched_nodes: std::collections::HashSet<NodeId> = bound
            .net_nodes
            .iter()
            .filter(|(name, _)| glob_match(pattern, name))
            .map(|(_, &nid)| nid)
            .collect();
        if matched_nodes.is_empty() {
            let known: Vec<String> = bound.net_nodes.keys().cloned().collect();
            return Err(AsBuiltError::UnknownNet {
                origin: self.origin.clone(),
                line,
                what: "[[cut]] net_glob",
                pattern: pattern.to_string(),
                suggestions: near_matches(&pattern.replace('*', ""), &known, 3),
            });
        }
        let mut severed = 0usize;
        for dev in bound.circuit.devices.iter_mut() {
            if let Some(node) = cut.terminal.node_mut(dev) {
                if matched_nodes.contains(node) {
                    *node = NodeId(*next_float);
                    *next_float += 1;
                    severed += 1;
                }
            }
        }
        let expected = cut.expect_matches.unwrap_or(1);
        if severed != expected {
            return Err(AsBuiltError::MatchCount {
                origin: self.origin.clone(),
                line,
                entry: format!("[[cut]] net_glob \"{pattern}\" severed {severed} terminal(s)"),
                expected,
                got: severed,
            });
        }
        report.lines.push(format!(
            "cut {pattern}: {severed} {:?} terminal(s) floated across {} net(s)",
            cut.terminal,
            matched_nodes.len()
        ));
        Ok(())
    }

    fn apply_lift(
        &self,
        lift: &Lift,
        bound: &mut BoundBoard,
        next_float: &mut u32,
        report: &mut AsBuiltReport,
    ) -> Result<(), AsBuiltError> {
        let line = self.line_of(&lift.reference);
        let reference = lift.reference.get_ref().as_str();
        let mut lifted = 0usize;
        let mut name_matches = 0usize;
        for dev in bound.circuit.devices.iter_mut() {
            if !dev.name().contains(reference) {
                continue;
            }
            name_matches += 1;
            if let Some(node) = lift.terminal.node_mut(dev) {
                *node = NodeId(*next_float);
                *next_float += 1;
                lifted += 1;
            }
        }
        if name_matches == 0 {
            let known: Vec<String> = bound
                .circuit
                .devices
                .iter()
                .map(|d| d.name().to_string())
                .collect();
            return Err(AsBuiltError::UnknownRef {
                origin: self.origin.clone(),
                line,
                reference: reference.to_string(),
                filtered_out: 0,
                suggestions: near_matches(reference, &known, 3),
            });
        }
        if lifted != 1 {
            return Err(AsBuiltError::MatchCount {
                origin: self.origin.clone(),
                line,
                entry: format!(
                    "[[lift]] ref \"{reference}\" ({:?}) lifted {lifted} terminal(s)",
                    lift.terminal
                ),
                expected: 1,
                got: lifted,
            });
        }
        report
            .lines
            .push(format!("lift {reference}: {:?} pin floated", lift.terminal));
        Ok(())
    }

    fn apply_jumper(
        &self,
        jumper: &Jumper,
        bound: &mut BoundBoard,
        report: &mut AsBuiltReport,
    ) -> Result<(), AsBuiltError> {
        let resolve = |net: &Spanned<String>, what: &'static str| {
            let name = net.get_ref().as_str();
            bound.net_nodes.get(name).copied().ok_or_else(|| {
                let known: Vec<String> = bound.net_nodes.keys().cloned().collect();
                AsBuiltError::UnknownNet {
                    origin: self.origin.clone(),
                    line: self.line_of(net),
                    what,
                    pattern: name.to_string(),
                    suggestions: near_matches(name, &known, 3),
                }
            })
        };
        let from = resolve(&jumper.from, "[[jumper]] from")?;
        let to = resolve(&jumper.to, "[[jumper]] to")?;
        if from == to {
            return Err(AsBuiltError::JumperSelf {
                origin: self.origin.clone(),
                line: self.line_of(&jumper.to),
                net: jumper.to.get_ref().clone(),
            });
        }
        for dev in bound.circuit.devices.iter_mut() {
            dev.map_nodes(&mut |n| if n == to { from } else { n });
        }
        for nid in bound.net_nodes.values_mut() {
            if *nid == to {
                *nid = from;
            }
        }
        // The circuit's device terminals are remapped above, but the binder also
        // cached raw NodeIds on the event-driven layer — the MCUs' role/ADC/GPIO
        // node maps, the supply legs, the DAC output drivers, and any behavioral
        // device's node map. Left unremapped they point at the orphaned `to`
        // node, so ADC injection, GPIO drive, rail stamping, and DAC output all
        // read/write the wrong (now-floating) node after a bodge-wire. (R8 #6)
        let remap = |n: NodeId| if n == to { from } else { n };
        for mcu in bound.mcus.iter_mut() {
            for n in mcu.role_nets.values_mut() {
                *n = remap(*n);
            }
            for n in mcu.adc_nets.values_mut() {
                *n = remap(*n);
            }
            for drv in mcu.gpio_drivers.values_mut() {
                drv.net = remap(drv.net);
            }
        }
        for dac in bound.dacs.iter_mut() {
            for drv in dac.vout_drivers.iter_mut().flatten() {
                drv.net = remap(drv.net);
            }
        }
        for leg in bound.supplies.iter_mut() {
            leg.net = remap(leg.net);
        }
        for b in bound.behavioral.iter_mut() {
            b.remap_node(&remap);
        }
        report.lines.push(format!(
            "jumper {} -> {}: nets merged",
            jumper.to.get_ref(),
            jumper.from.get_ref()
        ));
        Ok(())
    }
}

fn variant_name(dev: &Device) -> &'static str {
    match dev {
        Device::Resistor { .. } => "resistor",
        Device::Capacitor { .. } => "capacitor",
        Device::Inductor { .. } => "inductor",
        Device::Vsource { .. } => "vsource",
        Device::Isource { .. } => "isource",
        Device::Diode { .. } => "diode",
        Device::Bjt { .. } => "bjt",
        Device::Mosfet { .. } => "mosfet",
        Device::VSwitch { .. } => "vswitch",
        _ => "other",
    }
}

fn describe_set(set: &SetValues) -> String {
    let mut parts = Vec::new();
    if let Some(v) = set.ohms {
        parts.push(format!("ohms={v}"));
    }
    if let Some(v) = set.farads {
        parts.push(format!("farads={v}"));
    }
    if let Some(v) = set.von {
        parts.push(format!("von={v}"));
    }
    if let Some(v) = set.voff {
        parts.push(format!("voff={v}"));
    }
    parts.join(", ")
}

/// `*`-wildcard glob match (no other metacharacters). Anchored both ends.
fn glob_match(pattern: &str, text: &str) -> bool {
    fn inner(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'*') => (0..=t.len()).any(|skip| inner(&p[1..], &t[skip..])),
            Some(&c) => t.first() == Some(&c) && inner(&p[1..], &t[1..]),
        }
    }
    inner(pattern.as_bytes(), text.as_bytes())
}

/// Nearest known names to `target` by edit distance, for did-you-mean output.
/// Mirrors `hauksbee-ci/src/error.rs::near_matches` (that crate depends on
/// this one, so the helper cannot be imported without inverting the edge).
fn near_matches(target: &str, known: &[String], limit: usize) -> Vec<String> {
    let mut scored: BTreeMap<(usize, &str), ()> = BTreeMap::new();
    for k in known {
        let d = levenshtein(target, k);
        // Only offer plausible neighbours: within half the target's length.
        if d <= target.len().div_ceil(2).max(2) {
            scored.insert((d, k.as_str()), ());
        }
    }
    scored.keys().take(limit).map(|(_, k)| k.to_string()).collect()
}

fn levenshtein(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_star_and_literals() {
        assert!(glob_match("Net-(ANALOG_SWITCH*05-A)", "Net-(ANALOG_SWITCH1205-A)"));
        assert!(!glob_match("Net-(ANALOG_SWITCH*05-A)", "Net-(ANALOG_SWITCH1205-B)"));
        assert!(!glob_match("Net-(ANALOG_SWITCH*05-A)", "Net-(ANALOG_SWITCH1206-A)"));
        assert!(glob_match("abc", "abc"));
        assert!(!glob_match("abc", "abcd"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
    }

    #[test]
    fn unknown_toml_key_is_rejected() {
        let err = AsBuiltOverlay::parse(
            "[[replace]]\nref = \"R1\"\nset = { ohms = 1.0 }\nbogus = 3\n",
            "test.asbuilt.toml",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bogus"), "parse error must name the unknown key: {msg}");
    }

    #[test]
    fn unknown_set_key_is_rejected() {
        let err = AsBuiltOverlay::parse(
            "[[replace]]\nref = \"R1\"\nset = { henries = 1.0 }\n",
            "test.asbuilt.toml",
        )
        .unwrap_err();
        assert!(err.to_string().contains("henries"));
    }

    #[test]
    fn line_numbers_point_at_the_entry() {
        let overlay = AsBuiltOverlay::parse(
            "# header\n\n[[replace]]\nref = \"NOPE\"\nset = { ohms = 1.0 }\n",
            "test.asbuilt.toml",
        )
        .unwrap();
        assert_eq!(overlay.line_of(&overlay.doc.replaces[0].reference), 4);
    }

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("same", "same"), 0);
    }

    /// Round-8 #6: an as-built `[[jumper]]` merging net `N` (NodeId 2) onto
    /// `BUS` (NodeId 1) must remap not only the circuit devices and net_nodes
    /// but also the cached NodeIds the binder stashed on the event-driven layer
    /// — the MCU's role/ADC/GPIO node maps and the DAC output drivers. Left
    /// stale they point at the orphaned node.
    #[test]
    fn jumper_remaps_cached_mcu_and_dac_node_ids() {
        use crate::binder::{BoundBoard, DacBinding, McuBinding};
        use crate::drivers::PinDriver;
        use crate::report::BindReport;
        use hauksbee_ir::{Circuit, DeviceId, NodeId};
        use std::collections::HashMap;

        let bus = NodeId(1);
        let n = NodeId(2);
        let drv = |net: NodeId| PinDriver {
            vsource: DeviceId(0),
            net,
            enabled: true,
            roff: 1e9,
            resistor: DeviceId(1),
            ron: 25.0,
        };

        let mut role_nets = HashMap::new();
        role_nets.insert("adc0".to_string(), n);
        let mut adc_nets = HashMap::new();
        adc_nets.insert(0u8, n);
        let mut gpio_drivers = HashMap::new();
        gpio_drivers.insert(('A', 5u8), drv(n));
        let mcu = McuBinding {
            reference: "U1".to_string(),
            backend: String::new(),
            requested_part: String::new(),
            pad_roles: HashMap::new(),
            role_nets,
            gpio_drivers,
            adc_nets,
            module: false,
        };
        let dac = DacBinding {
            reference: "U9".to_string(),
            address: 0x60,
            vref: 3.3,
            gain: 1,
            vout_drivers: [Some(drv(n)), None, None, None],
        };

        let mut net_nodes = HashMap::new();
        net_nodes.insert("BUS".to_string(), bus);
        net_nodes.insert("N".to_string(), n);

        let mut bound = BoundBoard {
            name: String::new(),
            circuit: Circuit::new(),
            net_nodes,
            net_names: Vec::new(),
            digital: Vec::new(),
            mcus: vec![mcu],
            component_kinds: HashMap::new(),
            input_sources: HashMap::new(),
            supplies: Vec::new(),
            behavioral: Vec::new(),
            device_meta: Vec::new(),
            dacs: vec![dac],
            report: BindReport::default(),
        };

        let overlay =
            AsBuiltOverlay::parse("[[jumper]]\nfrom = \"BUS\"\nto = \"N\"\n", "t.toml").unwrap();
        overlay.apply(&mut bound).expect("jumper applies");

        // Every cached NodeId that was N must now be BUS.
        let m = &bound.mcus[0];
        assert_eq!(m.role_nets["adc0"], bus, "role_nets remapped");
        assert_eq!(m.adc_nets[&0], bus, "adc_nets remapped");
        assert_eq!(m.gpio_drivers[&('A', 5)].net, bus, "gpio driver net remapped");
        assert_eq!(
            bound.dacs[0].vout_drivers[0].as_ref().unwrap().net,
            bus,
            "DAC output driver net remapped"
        );
    }
}
