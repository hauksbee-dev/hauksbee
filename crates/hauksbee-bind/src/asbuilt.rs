//! The declarative AS-BUILT overlay: the physical delta between a board's
//! design files and the real, reworked board on the bench.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/asbuilt.md.
//!
//! A `.asbuilt.toml` file describes BOARD state only, solder-blob shorts,
//! fitted component values, cut traces, lifted pins, jumper wires, parts
//! wired on: the rework
//! a technician performed on copper. It deliberately does NOT describe
//! harness/scenario state (firmware-programmed registers, injected drives,
//! held enable nets): that is per-run configuration and lives where harness
//! config lives (the CI spec, the co-sim call sites). The line is physical:
//! if undoing it needs a soldering iron, it belongs here; if undoing it needs
//! a reset button, it does not.
//!
//! The vocabulary (each an array-of-tables):
//!
//! - `[[replace]]`, a component value swap or retune: `ref` names the device
//!   (substring match on the bound device name), `set` carries the fitted
//!   values (`ohms`, `farads`, `von`, `voff`), optional `was` records the
//!   removed part's value and doubles as a match refinement (±1 % relative),
//!   so a rework note like "removed the 10 pF, fitted 5.8 nF" is executable.
//! - `[[cut]]`, severed traces: every device terminal of the named role
//!   (`base`/`collector`/`emitter`) on a net matching `net_glob` is moved to
//!   a fresh floating node (floating beats grounded for cut terminals;
//!   `expect_matches` pins the severed-terminal count.
//! - `[[lift]]`, one lifted pin: like `cut` but for a single named device.
//! - `[[jumper]]`, a bodge wire: net `to` is merged onto net `from`.
//! - `[[fit]]`, a part wired ON: a new two-terminal device (`ohms` or
//!   `farads`) soldered between two named nets, under a fresh reference that
//!   must not collide with a fabricated part. This is the additive half of
//!   rework (pull resistors onto floating control lines, a bulk cap across a
//!   rail) that `replace` cannot express.
//!
//! Validation is fail-loud in the house style (`hauksbee-ci`'s spec/error
//! conventions): unknown TOML keys are rejected (`deny_unknown_fields`),
//! unknown refs/nets name the offending line and suggest near matches, and
//! every entry must match exactly the number of devices/terminals it declares
//! (default 1), a rework that silently applies to nothing or to twice as
//! much hardware as documented is a lie about the physical board.
//!
//! Application order is fixed and semantic: `replace` entries in file order,
//! then `cut`, then `lift`, then `jumper`, then `fit` (a rework part is
//! soldered onto the board the earlier steps produced). All are pure [`BoundBoard`]
//! mutations, no transient, no DC, preserving the prep-stays-solve-free
//! invariant the earlier hand-written preparation code established.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use serde::Deserialize;
use toml::Spanned;

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{Component, ExtractedBoard, Net, Pin};
use hauksbee_ir::{levenshtein, Device, NodeId};
use hauksbee_models::value::parse_value;
use hauksbee_models::ModelLibrary;

use crate::binder::BoundBoard;

/// First node id handed to floated (cut/lifted) terminals.
///
/// Must clear the binder's node namespace entirely,
/// [`apply`](AsBuiltOverlay::apply) fails loudly if any bound node reaches
/// it. The value is the historical base the earlier hand-written surgery used,
/// kept so the overlay's result is byte-identical to the proven imperative
/// surgery.
pub const FLOAT_NODE_BASE: u32 = 800_000;

/// Everything that can go wrong loading or applying an overlay.
///
/// Messages are self-contained: they carry the overlay origin (path), the
/// 1-based line of the offending entry, and near-match suggestions where a
/// name was unknown.
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
         expect_matches = {expected}; the file no longer describes this board; refusing to apply"
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
         apply to matched device \"{device}\" ({variant}): ohms needs a resistor, farads a \
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
    #[error("as-built overlay {origin}:{line}: [[replace]] ref \"{reference}\": `set` is empty; an entry that changes nothing documents nothing")]
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
    #[error("as-built overlay {origin}:{line}: [[jumper]] from and to name the same net \"{net}\"; a jumper to itself is a no-op and therefore a documentation error")]
    JumperSelf {
        origin: String,
        line: usize,
        net: String,
    },
    #[error(
        "as-built overlay {origin}:{line}: [[fit]] ref \"{reference}\" collides with an existing \
         device name; a rework part needs a fresh reference so the record cannot shadow a \
         fabricated one"
    )]
    FitRefCollision {
        origin: String,
        line: usize,
        reference: String,
    },
    #[error(
        "as-built overlay {origin}:{line}: [[fit]] ref \"{reference}\" must declare exactly one \
         of `ohms` or `farads`; the value is what says which part was soldered on"
    )]
    FitValue {
        origin: String,
        line: usize,
        reference: String,
    },
    #[error("as-built overlay {origin}:{line}: [[fit]] from and to name the same net \"{net}\"; a part across one net is a no-op and therefore a documentation error")]
    FitSelf {
        origin: String,
        line: usize,
        net: String,
    },
}

fn render_suggestions(suggestions: &[String]) -> String {
    if suggestions.is_empty() {
        String::new()
    } else {
        format!("; did you mean: {}?", suggestions.join(", "))
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
    const fn is_empty(&self) -> bool {
        self.ohms.is_none() && self.farads.is_none() && self.von.is_none() && self.voff.is_none()
    }
    /// `was`-refinement: does `dev`'s current value match every declared key
    /// within ±1 % relative? (Keys that don't apply to the variant fail the
    /// match rather than erroring: `was` selects, `set` demands.)
    fn matches_current(&self, dev: &Device) -> bool {
        let near = |actual: f64, expected: f64| (actual - expected).abs() < 0.01 * expected.abs();
        let ok = |declared: Option<f64>, actual: Option<f64>| {
            declared.is_none_or(|exp| actual.is_some_and(|act| near(act, exp)))
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
    const fn node_mut(self, dev: &mut Device) -> Option<&mut NodeId> {
        match (self, dev) {
            (Self::Base, Device::Bjt { b, .. }) => Some(b),
            (Self::Collector, Device::Bjt { c, .. }) => Some(c),
            (Self::Emitter, Device::Bjt { e, .. }) => Some(e),
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

/// A part wired ON during rework: a new two-terminal device soldered between
/// two existing nets (a pull resistor onto a floating control line, a bulk
/// capacitor across a rail).
///
/// Exactly one of `ohms` / `farads` names the part; `ref` is the rework
/// record's own designator and must not collide with a fabricated device, so
/// the overlay can never silently shadow the board. Nets are named as on the
/// board; a fresh node from a `[[cut]]` cannot be a terminal (it has no name
/// by construction; when a rework wires into a cut, record the wire side with
/// a `[[jumper]]` onto a real net instead).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fit {
    #[serde(rename = "ref")]
    pub reference: Spanned<String>,
    pub from: Spanned<String>,
    pub to: Spanned<String>,
    #[serde(default)]
    pub ohms: Option<f64>,
    #[serde(default)]
    pub farads: Option<f64>,
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
    #[serde(default, rename = "fit")]
    fits: Vec<Fit>,
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
    /// Entries [`AsBuiltOverlay::apply_to_board`] could not carry onto the
    /// static board, so a static finding on the parts and nets they touch is
    /// the design's, not the reworked board's. Always empty from
    /// [`AsBuiltOverlay::apply`].
    pub unapplied: Vec<UnappliedEntry>,
}

/// One overlay entry the static board does not carry, with what it touches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnappliedEntry {
    /// The entry as the overlay names it (`replace NEURON_SWITCH601`).
    pub entry: String,
    /// Why the static board cannot carry it.
    pub why: String,
    /// The board parts the entry reworks.
    pub refs: Vec<String>,
    /// The board nets the entry reworks.
    pub nets: Vec<String>,
}

impl UnappliedEntry {
    /// Whether a finding on `refs` and `nets` names a part or net this entry
    /// reworks, so the finding cannot be read as the reworked board's.
    #[must_use]
    pub fn touches(&self, refs: &[String], nets: &[String]) -> bool {
        refs.iter().any(|r| self.refs.contains(r)) || nets.iter().any(|n| self.nets.contains(n))
    }
}

impl AsBuiltOverlay {
    /// Parse an overlay from TOML text. `origin` labels errors (a path, or a
    /// symbolic name for embedded overlays).
    ///
    /// # Errors
    ///
    /// Returns [`AsBuiltError::Parse`] when `src` is not a valid overlay TOML document.
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
    ///
    /// # Errors
    ///
    /// Returns [`AsBuiltError::Io`] when the file cannot be read, or
    /// [`AsBuiltError::Parse`] when its contents are not a valid overlay.
    pub fn load(path: &Path) -> Result<Self, AsBuiltError> {
        let origin = path.display().to_string();
        let src = std::fs::read_to_string(path).map_err(|e| AsBuiltError::Io {
            origin: origin.clone(),
            source: e,
        })?;
        Self::parse(&src, &origin)
    }

    /// The `[board]` provenance block, if the overlay carries one.
    #[must_use]
    pub const fn board(&self) -> Option<&BoardMeta> {
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
    ///
    /// # Errors
    ///
    /// Returns the first entry that does not describe this board: an unknown
    /// reference or net, a match count that drifted, a key/variant mismatch, or a
    /// node space too full to allocate floating nodes for cuts and lifts.
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
        // Fits come last: a rework part is soldered onto the board the earlier
        // steps produced, so its nets resolve AFTER any jumper merges.
        for fit in &self.doc.fits {
            self.apply_fit(fit, bound, &mut report)?;
        }
        Ok(report)
    }

    fn apply_fit(
        &self,
        fit: &Fit,
        bound: &mut BoundBoard,
        report: &mut AsBuiltReport,
    ) -> Result<(), AsBuiltError> {
        let reference = fit.reference.get_ref().as_str();
        if bound.circuit.devices.iter().any(|d| d.name() == reference) {
            return Err(AsBuiltError::FitRefCollision {
                origin: self.origin.clone(),
                line: self.line_of(&fit.reference),
                reference: reference.to_string(),
            });
        }
        let a = self.resolve_net(bound, &fit.from, "[[fit]] from")?;
        let b = self.resolve_net(bound, &fit.to, "[[fit]] to")?;
        if a == b {
            return Err(AsBuiltError::FitSelf {
                origin: self.origin.clone(),
                line: self.line_of(&fit.to),
                net: fit.to.get_ref().clone(),
            });
        }
        let (device, described) = match (fit.ohms, fit.farads) {
            (Some(ohms), None) => (
                Device::resistor(reference.to_string(), a, b, ohms),
                format!("{ohms} ohm resistor"),
            ),
            (None, Some(farads)) => (
                Device::capacitor(reference.to_string(), a, b, farads),
                format!("{farads} F capacitor"),
            ),
            _ => {
                return Err(AsBuiltError::FitValue {
                    origin: self.origin.clone(),
                    line: self.line_of(&fit.reference),
                    reference: reference.to_string(),
                })
            }
        };
        bound.circuit.devices.push(device);
        report.lines.push(format!(
            "fit {reference}: {described} between {} and {}{}",
            fit.from.get_ref(),
            fit.to.get_ref(),
            fit.note
                .as_deref()
                .map(|n| format!(" ({n})"))
                .unwrap_or_default()
        ));
        Ok(())
    }

    fn check_node_space(&self, bound: &BoundBoard) -> Result<u32, AsBuiltError> {
        if self.doc.cuts.is_empty() && self.doc.lifts.is_empty() {
            return Ok(FLOAT_NODE_BASE);
        }
        let max_seen = bound
            .circuit
            .devices
            .iter()
            .flat_map(hauksbee_ir::Device::nodes)
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
                entry: format!(
                    "[[replace]] ref \"{reference}\" matched {} device(s)",
                    hits.len()
                ),
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
        for dev in &mut bound.circuit.devices {
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
        for dev in &mut bound.circuit.devices {
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

    /// Resolve an overlay-named net to its bound node, or refuse with the
    /// overlay line, what the net was for, and the nearest real net names.
    fn resolve_net(
        &self,
        bound: &BoundBoard,
        net: &Spanned<String>,
        what: &'static str,
    ) -> Result<NodeId, AsBuiltError> {
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
    }

    fn apply_jumper(
        &self,
        jumper: &Jumper,
        bound: &mut BoundBoard,
        report: &mut AsBuiltReport,
    ) -> Result<(), AsBuiltError> {
        let from = self.resolve_net(bound, &jumper.from, "[[jumper]] from")?;
        let to = self.resolve_net(bound, &jumper.to, "[[jumper]] to")?;
        if from == to {
            return Err(AsBuiltError::JumperSelf {
                origin: self.origin.clone(),
                line: self.line_of(&jumper.to),
                net: jumper.to.get_ref().clone(),
            });
        }
        for dev in &mut bound.circuit.devices {
            dev.map_nodes(&mut |n| if n == to { from } else { n });
        }
        for nid in bound.net_nodes.values_mut() {
            if *nid == to {
                *nid = from;
            }
        }
        // The circuit's device terminals are remapped above, but the binder also
        // cached raw NodeIds on the event-driven layer; the MCUs' role/ADC/GPIO
        // node maps, the supply legs, the DAC output drivers, and any behavioral
        // device's node map. Left unremapped they point at the orphaned `to`
        // node, so ADC injection, GPIO drive, rail stamping, and DAC output all
        // read/write the wrong (now-floating) node after a bodge-wire. (R8 #6)
        let remap = |n: NodeId| if n == to { from } else { n };
        for mcu in &mut bound.mcus {
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
        for dac in &mut bound.dacs {
            for drv in dac.vout_drivers.iter_mut().flatten() {
                drv.net = remap(drv.net);
            }
            for n in &mut dac.bus_nodes {
                *n = remap(*n);
            }
        }
        for p in &mut bound.peripherals {
            for n in &mut p.bus_nodes {
                *n = remap(*n);
            }
        }
        for leg in &mut bound.supplies {
            leg.net = remap(leg.net);
        }
        for b in &mut bound.behavioral {
            b.remap_node(remap);
        }
        // Digital components cache raw NodeIds the same way: `roles` maps each
        // wired pin to its net node (read at tick to SAMPLE input levels via
        // `node_v`, and read by the 595/165 chain builders to find SER/QH), and
        // each `PinDriver.net` is the output node the driver stamps onto. Left
        // unremapped after a bodge wire, an input role samples the orphaned
        // (now floating ~0 V) `to` node instead of the merged net, so a buffer
        // or shift-register reads a stale LOW regardless of the real signal.
        for d in &mut bound.digital {
            for n in d.roles.values_mut() {
                *n = remap(*n);
            }
            for drv in d.drivers.values_mut() {
                drv.net = remap(drv.net);
            }
        }
        report.lines.push(format!(
            "jumper {} -> {}: nets merged",
            jumper.to.get_ref(),
            jumper.from.get_ref()
        ));
        Ok(())
    }
}

/// The overlay on an [`ExtractedBoard`], so the static checks judge the
/// reworked board: fitted resistor/capacitor values, severed transistor
/// terminals (cut and lift), bodge-wire net merges and wired-on parts. Switch
/// trip points are circuit behaviour only the simulated board carries; they,
/// and any entry the static board cannot carry, are listed in
/// [`AsBuiltReport::unapplied`] with the parts and nets they touch.
impl AsBuiltOverlay {
    /// Apply the overlay to `board`, in the same order as
    /// [`apply`](Self::apply) (replace, cut, lift, jumper, fit), so the static
    /// checks judge the reworked board and not the design.
    /// [`apply`](Self::apply) is the authority on what each entry does: a cut
    /// or lift is applied to the board bound with `lib` by that same code, and
    /// every terminal it floats is traced back through the part's pin roles
    /// (the binder's own [`role_pads`](crate::binder::role_pads)) to the pads
    /// it severs, which move to a fresh one-pad net. An entry the static board
    /// cannot carry is left as designed and listed in the report's
    /// [`unapplied`](AsBuiltReport::unapplied), never dropped.
    ///
    /// # Errors
    ///
    /// Returns the first `[[cut]]` or `[[lift]]` entry that does not describe
    /// the bound board (as [`apply`](Self::apply)), or the first `[[jumper]]`
    /// or `[[fit]]` entry that names a net the board lacks, joins a net to
    /// itself, reuses a part reference, or lacks exactly one of `ohms`/`farads`.
    pub fn apply_to_board(
        &self,
        board: &mut ExtractedBoard,
        lib: &ModelLibrary,
    ) -> Result<AsBuiltReport, AsBuiltError> {
        let mut report = AsBuiltReport::default();
        // Found on the design board, as the simulation binds it before the
        // overlay: a value swap moves no terminal, so the order is the same.
        let floated = self.floated_terminals(board, lib)?;
        for rep in &self.doc.replaces {
            Self::replace_on_board(rep, board, &mut report);
        }
        sever_on_board(board, lib, floated, &mut report);
        for jumper in &self.doc.jumpers {
            self.jumper_on_board(jumper, board, &mut report)?;
        }
        for fit in &self.doc.fits {
            self.fit_on_board(fit, board, &mut report)?;
        }
        Ok(report)
    }

    /// Every terminal the overlay's cuts and lifts float, read off the board
    /// bound with `lib` after [`apply`](Self::apply)'s own cut and lift steps
    /// ran on it, grouped by entry in application order.
    fn floated_terminals(
        &self,
        board: &ExtractedBoard,
        lib: &ModelLibrary,
    ) -> Result<Floated, AsBuiltError> {
        let mut floated = Floated::default();
        if self.doc.cuts.is_empty() && self.doc.lifts.is_empty() {
            return Ok(floated);
        }
        let mut bound = crate::binder::bind_board(board, lib);
        let mut next_float = self.check_node_space(&bound)?;
        let mut scratch = AsBuiltReport::default();
        for cut in &self.doc.cuts {
            let before = bjt_terminals(&bound);
            self.apply_cut(cut, &mut bound, &mut next_float, &mut scratch)?;
            let entry = format!("cut {}", cut.net_glob.get_ref());
            floated.entries.push((entry, "cut", moved(&before, &bound)));
        }
        for lift in &self.doc.lifts {
            let before = bjt_terminals(&bound);
            self.apply_lift(lift, &mut bound, &mut next_float, &mut scratch)?;
            let entry = format!("lift {} {:?}", lift.reference.get_ref(), lift.terminal);
            floated
                .entries
                .push((entry, "lift", moved(&before, &bound)));
        }
        floated.net_nodes = bound.net_nodes;
        Ok(floated)
    }

    fn replace_on_board(rep: &Replace, board: &mut ExtractedBoard, report: &mut AsBuiltReport) {
        let reference = rep.reference.get_ref().as_str();
        let named: Vec<String> = (board.components.iter())
            .filter(|c| c.reference.contains(reference))
            .map(|c| c.reference.clone())
            .collect();
        let unapplied = |why: String| UnappliedEntry {
            entry: format!("replace {reference}"),
            why,
            refs: named.clone(),
            nets: Vec::new(),
        };
        if rep.set.von.is_some() || rep.set.voff.is_some() {
            let trip = SetValues {
                von: rep.set.von,
                voff: rep.set.voff,
                ..SetValues::default()
            };
            let why = format!(
                "{} is a switch trip point, which only the simulated circuit carries",
                describe_set(&trip)
            );
            report.lines.push(format!(
                "replace {reference}: {} not represented in static checks (simulated circuit only)",
                describe_set(&trip)
            ));
            report.unapplied.push(unapplied(why));
        }
        let Some(value) = rep.set.ohms.or(rep.set.farads) else {
            return;
        };
        let was = rep.was.as_ref().and_then(|w| w.ohms.or(w.farads));
        let near = |actual: f64, expected: f64| (actual - expected).abs() < 0.01 * expected.abs();
        let hits: Vec<usize> = board
            .components
            .iter()
            .enumerate()
            .filter(|(_, c)| c.reference.contains(reference))
            .filter(|(_, c)| {
                let current = parse_value(&c.value).map(|p| p.si);
                was.is_none_or(|w| current.is_some_and(|cur| near(cur, w)))
            })
            .map(|(i, _)| i)
            .collect();
        // The simulated board is the authority on what the entry hits; a static
        // board that disagrees on the count is left as designed, and says so.
        let expected = rep.expect_matches.unwrap_or(1);
        if hits.len() != expected {
            report.lines.push(format!(
                "replace {reference}: matched {} static part(s), expected {expected}; left as designed",
                hits.len()
            ));
            report.unapplied.push(unapplied(format!(
                "the board's part values match {} part(s) where the entry expects {expected}, \
                 so the static board keeps the designed value",
                hits.len()
            )));
            return;
        }
        for &i in &hits {
            board.components[i].value = format!("{value}");
        }
        report.lines.push(format!(
            "replace {reference}: {} part(s) set {}",
            hits.len(),
            describe_set(&rep.set)
        ));
    }

    fn net_id_on_board(
        &self,
        board: &ExtractedBoard,
        net: &Spanned<String>,
        what: &'static str,
    ) -> Result<i64, AsBuiltError> {
        let name = net.get_ref().as_str();
        board
            .nets
            .iter()
            .find(|n| n.name == name)
            .map(|n| n.id)
            .ok_or_else(|| {
                let known: Vec<String> = board.nets.iter().map(|n| n.name.clone()).collect();
                AsBuiltError::UnknownNet {
                    origin: self.origin.clone(),
                    line: self.line_of(net),
                    what,
                    pattern: name.to_string(),
                    suggestions: near_matches(name, &known, 3),
                }
            })
    }

    fn jumper_on_board(
        &self,
        jumper: &Jumper,
        board: &mut ExtractedBoard,
        report: &mut AsBuiltReport,
    ) -> Result<(), AsBuiltError> {
        let from = self.net_id_on_board(board, &jumper.from, "[[jumper]] from")?;
        let to = self.net_id_on_board(board, &jumper.to, "[[jumper]] to")?;
        if from == to {
            return Err(AsBuiltError::JumperSelf {
                origin: self.origin.clone(),
                line: self.line_of(&jumper.to),
                net: jumper.to.get_ref().clone(),
            });
        }
        for pin in board.components.iter_mut().flat_map(|c| &mut c.pins) {
            if pin.net == Some(to) {
                pin.net = Some(from);
            }
        }
        board.nets.retain(|n| n.id != to);
        report.lines.push(format!(
            "jumper {} -> {}: nets merged",
            jumper.to.get_ref(),
            jumper.from.get_ref()
        ));
        Ok(())
    }

    fn fit_on_board(
        &self,
        fit: &Fit,
        board: &mut ExtractedBoard,
        report: &mut AsBuiltReport,
    ) -> Result<(), AsBuiltError> {
        let reference = fit.reference.get_ref().as_str();
        let line = self.line_of(&fit.reference);
        let origin = self.origin.clone();
        if board.components.iter().any(|c| c.reference == reference) {
            return Err(AsBuiltError::FitRefCollision {
                origin,
                line,
                reference: reference.to_string(),
            });
        }
        let a = self.net_id_on_board(board, &fit.from, "[[fit]] from")?;
        let b = self.net_id_on_board(board, &fit.to, "[[fit]] to")?;
        if a == b {
            return Err(AsBuiltError::FitSelf {
                origin,
                line: self.line_of(&fit.to),
                net: fit.to.get_ref().clone(),
            });
        }
        let (lib_id, value) = match (fit.ohms, fit.farads) {
            (Some(ohms), None) => ("Device:R", ohms),
            (None, Some(farads)) => ("Device:C", farads),
            _ => {
                return Err(AsBuiltError::FitValue {
                    origin,
                    line,
                    reference: reference.to_string(),
                })
            }
        };
        let pin = |number: &str, net: i64| Pin {
            number: number.to_string(),
            net: Some(net),
            function: String::new(),
            kind: "passive".to_string(),
            position: None,
        };
        board.components.push(Component {
            reference: reference.to_string(),
            value: format!("{value}"),
            lib_id: lib_id.to_string(),
            pins: vec![pin("1", a), pin("2", b)],
            ..Component::default()
        });
        report.lines.push(format!(
            "fit {reference}: {value} between {} and {}",
            fit.from.get_ref(),
            fit.to.get_ref()
        ));
        Ok(())
    }
}

/// The terminals an overlay's cuts and lifts float on the bound board.
#[derive(Default)]
struct Floated {
    /// Per entry, in application order: its name, its op ("cut" or "lift")
    /// and every terminal it floated.
    entries: Vec<(String, &'static str, Vec<FloatedTerminal>)>,
    /// The bound board's net names to nodes (cuts and lifts move terminals,
    /// never nets).
    net_nodes: HashMap<String, NodeId>,
}

/// One device terminal a cut or lift floated, and the node it left.
struct FloatedTerminal {
    device: String,
    terminal: Terminal,
    was: NodeId,
}

const TERMINALS: [Terminal; 3] = [Terminal::Base, Terminal::Collector, Terminal::Emitter];

impl Terminal {
    /// The binder's role name for this terminal (a dual part's unit suffix,
    /// `_q2`, follows it).
    const fn role(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Collector => "collector",
            Self::Emitter => "emitter",
        }
    }
}

/// Every transistor's terminal nodes, in [`TERMINALS`] order, by device index.
fn bjt_terminals(bound: &BoundBoard) -> Vec<Option<[NodeId; 3]>> {
    (bound.circuit.devices.iter())
        .map(|d| match d {
            Device::Bjt { b, c, e, .. } => Some([*b, *c, *e]),
            _ => None,
        })
        .collect()
}

/// The terminals whose node differs from `before`: what one entry floated.
fn moved(before: &[Option<[NodeId; 3]>], bound: &BoundBoard) -> Vec<FloatedTerminal> {
    let after = bjt_terminals(bound);
    let mut out = Vec::new();
    for ((dev, old), new) in bound.circuit.devices.iter().zip(before).zip(&after) {
        let (Some(old), Some(new)) = (old, new) else {
            continue;
        };
        for ((terminal, was), now) in TERMINALS.iter().zip(old).zip(new) {
            if was != now {
                out.push(FloatedTerminal {
                    device: dev.name().to_string(),
                    terminal: *terminal,
                    was: *was,
                });
            }
        }
    }
    out
}

/// The board part a stamped transistor came from, and its unit suffix: the
/// binder names a single transistor after its reference and each unit of a
/// multi-transistor package `<reference>_q<n>`.
fn owner(by_ref: &HashMap<&str, usize>, device: &str) -> Option<(usize, String)> {
    if let Some(&i) = by_ref.get(device) {
        return Some((i, String::new()));
    }
    let (reference, unit) = device.rsplit_once("_q")?;
    let numbered = !unit.is_empty() && unit.bytes().all(|b| b.is_ascii_digit());
    let &i = by_ref.get(reference).filter(|_| numbered)?;
    Some((i, format!("_q{unit}")))
}

/// The pins of `comp` that make up `terminal` (with unit `suffix`) on one of
/// `was_nets`, read through the part's pin roles exactly as the binder bound
/// them. Empty when the part resolves to no model or the role to no pad there.
fn terminal_pins(
    comp: &Component,
    lib: &ModelLibrary,
    terminal: Terminal,
    suffix: &str,
    was_nets: &[i64],
) -> Vec<usize> {
    let Some(part) = AssemblyState::of(comp).fitted() else {
        return Vec::new();
    };
    let resolution = crate::binder::resolve(lib, part);
    let Some(model) = resolution.model.as_ref() else {
        return Vec::new();
    };
    let role = format!("{}{suffix}", terminal.role());
    let pads = crate::binder::role_pads(comp, model, lib.pin_rules(), &role);
    (comp.pins.iter().enumerate())
        .filter(|(_, p)| pads.contains(&p.number) && p.net.is_some_and(|n| was_nets.contains(&n)))
        .map(|(i, _)| i)
        .collect()
}

/// "a, b, c and 87 more": a long list kept to one line.
fn first_few(items: &[String]) -> String {
    const SHOWN: usize = 3;
    if items.len() <= SHOWN {
        return items.join(", ");
    }
    format!(
        "{} and {} more",
        items[..SHOWN].join(", "),
        items.len() - SHOWN
    )
}

/// Move every pad a floated terminal maps to onto a fresh one-pad net (the
/// static twin of the simulation's fresh floating node), and list each entry
/// whose terminals could not all be traced to a pad as unapplied.
fn sever_on_board(
    board: &mut ExtractedBoard,
    lib: &ModelLibrary,
    floated: Floated,
    report: &mut AsBuiltReport,
) {
    let mut next_id = board.nets.iter().map(|n| n.id).max().unwrap_or(0) + 1;
    for (entry, op, terminals) in floated.entries {
        let by_ref: HashMap<&str, usize> = (board.components.iter().enumerate())
            .map(|(i, c)| (c.reference.as_str(), i))
            .collect();
        let mut cuts: Vec<(usize, usize)> = Vec::new();
        let mut untraced = UnappliedEntry {
            entry: entry.clone(),
            ..UnappliedEntry::default()
        };
        let mut missed: Vec<String> = Vec::new();
        for t in &terminals {
            let was_nets: Vec<i64> = (board.nets.iter())
                .filter(|n| floated.net_nodes.get(&n.name) == Some(&t.was))
                .map(|n| n.id)
                .collect();
            let found = owner(&by_ref, &t.device).map(|(ci, suffix)| {
                let pins =
                    terminal_pins(&board.components[ci], lib, t.terminal, &suffix, &was_nets);
                (ci, pins)
            });
            match found {
                Some((ci, pins)) if !pins.is_empty() => {
                    cuts.extend(pins.into_iter().map(|pi| (ci, pi)));
                }
                other => {
                    let part = other.map_or_else(
                        || t.device.clone(),
                        |(ci, _)| board.components[ci].reference.clone(),
                    );
                    missed.push(format!("{} {}", t.device, t.terminal.role()));
                    untraced.refs.push(part);
                    untraced.nets.extend(
                        (board.nets.iter())
                            .filter(|n| was_nets.contains(&n.id))
                            .map(|n| n.name.clone()),
                    );
                }
            }
        }
        let mut severed: Vec<String> = Vec::new();
        for (ci, pi) in cuts {
            let comp = &mut board.components[ci];
            let pin = &mut comp.pins[pi];
            let name = format!("asbuilt-{op}-({}-Pad{})", comp.reference, pin.number);
            severed.push(format!("{} pad {}", comp.reference, pin.number));
            pin.net = Some(next_id);
            board.nets.push(Net::new(next_id, name));
            next_id += 1;
        }
        if !severed.is_empty() {
            report.lines.push(format!(
                "{entry}: {} pad(s) severed onto their own nets ({})",
                severed.len(),
                first_few(&severed)
            ));
        }
        if !missed.is_empty() {
            report.lines.push(format!(
                "{entry}: {} terminal(s) not traced to a pad, left as designed ({})",
                missed.len(),
                first_few(&missed)
            ));
            untraced.why = format!(
                "the simulated board floats {} but no pad of the part could be traced to \
                 that terminal through its pin roles",
                first_few(&missed)
            );
            untraced.refs.sort();
            untraced.refs.dedup();
            untraced.nets.sort();
            untraced.nets.dedup();
            report.unapplied.push(untraced);
        }
    }
}

const fn variant_name(dev: &Device) -> &'static str {
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
    let mut scored: BTreeSet<(usize, &str)> = BTreeSet::new();
    for k in known {
        let d = levenshtein(target, k);
        // Only offer plausible neighbours: within half the target's length.
        if d <= target.len().div_ceil(2).max(2) {
            scored.insert((d, k.as_str()));
        }
    }
    scored
        .iter()
        .take(limit)
        .map(|(_, k)| k.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_star_and_literals() {
        assert!(glob_match(
            "Net-(ANALOG_SWITCH*05-A)",
            "Net-(ANALOG_SWITCH1205-A)"
        ));
        assert!(!glob_match(
            "Net-(ANALOG_SWITCH*05-A)",
            "Net-(ANALOG_SWITCH1205-B)"
        ));
        assert!(!glob_match(
            "Net-(ANALOG_SWITCH*05-A)",
            "Net-(ANALOG_SWITCH1206-A)"
        ));
        assert!(glob_match("abc", "abc"));
        assert!(!glob_match("abc", "abcd"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
    }

    /// A minimal bound board for overlay unit tests: an empty circuit plus
    /// the named nets, mirroring the runner's own test literal.
    fn test_board(nets: &[(&str, u32)]) -> BoundBoard {
        BoundBoard {
            name: "asbuilt-test".to_string(),
            net_nodes: nets
                .iter()
                .map(|(n, id)| (n.to_string(), NodeId(*id)))
                .collect(),
            net_names: nets.iter().map(|(n, _)| n.to_string()).collect(),
            ..BoundBoard::default()
        }
    }

    #[test]
    fn fit_requires_exactly_one_value_kind() {
        // Both keys, and neither key, are documentation errors at parse-time
        // shape level (the apply step raises FitValue; parsing accepts the
        // struct, so drive apply with a tiny bound board).
        let overlay = AsBuiltOverlay::parse(
            "[[fit]]\nref = \"R_PULL\"\nfrom = \"A\"\nto = \"B\"\n",
            "test.asbuilt.toml",
        )
        .unwrap();
        let mut bound = test_board(&[("A", 1), ("B", 2)]);
        let err = overlay.apply(&mut bound).unwrap_err().to_string();
        assert!(err.contains("exactly one of `ohms` or `farads`"), "{err}");
    }

    #[test]
    fn fit_adds_the_part_and_fails_loud_on_collision_and_self() {
        let overlay = AsBuiltOverlay::parse(
            "[[fit]]\nref = \"R_PULL_OE\"\nfrom = \"OE_N\"\nto = \"+5V\"\nohms = 10000.0\nnote = \"rework: pull-up wired on\"\n",
            "test.asbuilt.toml",
        )
        .unwrap();
        let mut bound = test_board(&[("OE_N", 1), ("+5V", 2)]);
        let report = overlay.apply(&mut bound).unwrap();
        assert_eq!(bound.circuit.devices.len(), 1);
        match &bound.circuit.devices[0] {
            Device::Resistor {
                name, a, b, ohms, ..
            } => {
                assert_eq!(name, "R_PULL_OE");
                assert_eq!((a.0, b.0), (1, 2));
                assert!((*ohms - 10_000.0).abs() < 1e-9, "{ohms}");
            }
            other => panic!("expected the fitted resistor, got {other:?}"),
        }
        assert!(
            report
                .lines
                .iter()
                .any(|l| l.contains("R_PULL_OE") && l.contains("pull-up wired on")),
            "{report:?}"
        );

        // Re-applying the same fit onto the result collides with the part it
        // just added: the record cannot silently shadow a device.
        let err = overlay.apply(&mut bound).unwrap_err().to_string();
        assert!(
            err.contains("collides with an existing device name"),
            "{err}"
        );

        // And a fit across one net is a documentation error.
        let self_fit = AsBuiltOverlay::parse(
            "[[fit]]\nref = \"R_X\"\nfrom = \"OE_N\"\nto = \"OE_N\"\nohms = 1000.0\n",
            "test.asbuilt.toml",
        )
        .unwrap();
        let mut bound2 = test_board(&[("OE_N", 1)]);
        let err = self_fit.apply(&mut bound2).unwrap_err().to_string();
        assert!(err.contains("name the same net"), "{err}");
    }

    #[test]
    fn unknown_toml_key_is_rejected() {
        let err = AsBuiltOverlay::parse(
            "[[replace]]\nref = \"R1\"\nset = { ohms = 1.0 }\nbogus = 3\n",
            "test.asbuilt.toml",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bogus"),
            "parse error must name the unknown key: {msg}"
        );
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

    /// Round-8 #6: an as-built `[[jumper]]` merging net `N` (`NodeId` 2) onto
    /// `BUS` (`NodeId` 1) must remap not only the circuit devices and `net_nodes`
    /// but also the cached `NodeIds` the binder stashed on the event-driven layer;
    /// the MCU's role/ADC/GPIO node maps and the DAC output drivers. Left
    /// stale they point at the orphaned node.
    #[test]
    fn jumper_remaps_cached_mcu_and_dac_node_ids() {
        use crate::binder::{BoundBoard, DacBinding, McuBinding};
        use crate::digital::{DigitalComponent, LogicLevels};
        use crate::drivers::PinDriver;
        use hauksbee_ir::{DeviceId, NodeId};
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
            supply: None,
        };

        let mcu = McuBinding {
            reference: "U1".to_string(),
            backend: String::new(),
            requested_part: String::new(),
            clock: crate::mcu_clock::McuClock::default(),
            internal_clock: None,
            pad_roles: HashMap::new(),
            role_nets: HashMap::from([("adc0".to_string(), n)]),
            gpio_drivers: HashMap::from([(('A', 5u8), drv(n))]),
            adc_nets: HashMap::from([(0u8, n)]),
            adc_pin: HashMap::new(),
            module: false,
            max_supply_v: None,
            supply_stand_in: None,
            min_supply_v: None,
            supply_floors: HashMap::new(),
            electrical: crate::mcu_electrical::McuElectrical::default(),
        };
        let dac = DacBinding {
            reference: "U9".to_string(),
            address: 0x60,
            vref: 3.3,
            gain: 1,
            vout_drivers: [Some(drv(n)), None, None, None],
            bus_nodes: Vec::new(),
        };

        // A digital buffer whose input role `a0` and output driver `y0` both
        // sit on the net being merged away: after the jumper its cached role
        // NodeId and driver net must follow to BUS or it samples/drives the
        // orphaned floating node.
        let digital = DigitalComponent {
            reference: "U7".to_string(),
            levels: LogicLevels {
                ro: 25.0,
                ..LogicLevels::from_params(&hauksbee_models::ModelEntry::default())
            },
            roles: HashMap::from([("a0".to_string(), n)]),
            drivers: HashMap::from([("y0".to_string(), drv(n))]),
            logic: None,
            supply: None,
            rails: crate::digital::RailLevels::default(),
        };

        let mut bound = BoundBoard {
            net_nodes: HashMap::from([("BUS".to_string(), bus), ("N".to_string(), n)]),
            digital: vec![digital],
            mcus: vec![mcu],
            dacs: vec![dac],
            ..BoundBoard::default()
        };

        let overlay =
            AsBuiltOverlay::parse("[[jumper]]\nfrom = \"BUS\"\nto = \"N\"\n", "t.toml").unwrap();
        overlay.apply(&mut bound).expect("jumper applies");

        // Every cached NodeId that was N must now be BUS.
        let m = &bound.mcus[0];
        assert_eq!(m.role_nets["adc0"], bus, "role_nets remapped");
        assert_eq!(m.adc_nets[&0], bus, "adc_nets remapped");
        assert_eq!(
            m.gpio_drivers[&('A', 5)].net,
            bus,
            "gpio driver net remapped"
        );
        assert_eq!(
            bound.dacs[0].vout_drivers[0].as_ref().unwrap().net,
            bus,
            "DAC output driver net remapped"
        );
        let d = &bound.digital[0];
        assert_eq!(d.roles["a0"], bus, "digital input role NodeId remapped");
        assert_eq!(
            d.drivers["y0"].net, bus,
            "digital output driver net remapped"
        );
    }

    fn static_part(reference: &str, value: &str, nets: [i64; 2]) -> Component {
        let pin = |number: &str, net: i64| Pin {
            number: number.to_string(),
            net: Some(net),
            function: String::new(),
            kind: "passive".to_string(),
            position: None,
        };
        Component {
            reference: reference.to_string(),
            value: value.to_string(),
            lib_id: "Device:R".to_string(),
            pins: vec![pin("1", nets[0]), pin("2", nets[1])],
            ..Component::default()
        }
    }

    #[test]
    fn overlay_applies_replace_jumper_and_fit_to_the_static_board() {
        let lib = ModelLibrary::builtin();
        let net = |id, name: &str| hauksbee_extract::Net {
            id,
            name: name.to_string(),
        };
        let mut board = ExtractedBoard {
            name: "static".into(),
            nets: vec![net(1, "A"), net(2, "B"), net(3, "C")],
            components: vec![
                static_part("R1", "1k", [1, 2]),
                static_part("R2", "10k", [2, 3]),
            ],
        };
        let overlay = AsBuiltOverlay::parse(
            r#"
[[replace]]
ref = "R1"
was = { ohms = 1000.0 }
set = { ohms = 0.001 }
[[replace]]
ref = "ANALOG_SWITCH1"
set = { von = 3.6, voff = 3.4 }
[[jumper]]
from = "A"
to = "C"
[[fit]]
ref = "R_fit"
from = "A"
to = "B"
ohms = 4700.0
"#,
            "unit",
        )
        .unwrap();
        let report = overlay.apply_to_board(&mut board, &lib).unwrap();
        assert_eq!(board.components[0].value, "0.001");
        assert_eq!(board.components[1].pins[1].net, Some(1), "C merged onto A");
        assert!(board.nets.iter().all(|n| n.name != "C"));
        let fitted = board.components.last().unwrap();
        assert_eq!(
            (fitted.reference.as_str(), fitted.value.as_str()),
            ("R_fit", "4700")
        );
        assert!(report
            .lines
            .iter()
            .any(|l| l.contains("ANALOG_SWITCH1") && l.contains("not represented")));
        // The trip point the static board cannot carry is listed, not dropped.
        assert_eq!(report.unapplied.len(), 1, "{:?}", report.unapplied);
        assert_eq!(report.unapplied[0].entry, "replace ANALOG_SWITCH1");
        // A `was` that disagrees with the board leaves the part as designed.
        let mut other = board.clone();
        other.components[1].value = "10k".into();
        let stale = AsBuiltOverlay::parse(
            "[[replace]]\nref = \"R2\"\nwas = { ohms = 1.0 }\nset = { ohms = 5.0 }\n",
            "unit",
        )
        .unwrap();
        let report = stale.apply_to_board(&mut other, &lib).unwrap();
        assert_eq!(other.components[1].value, "10k");
        // ...and lists the entry as unapplied, naming the part it would rework.
        assert_eq!(report.unapplied.len(), 1, "{:?}", report.unapplied);
        assert_eq!(report.unapplied[0].refs, ["R2"]);
        assert!(report.unapplied[0].touches(&["R2".into()], &[]));
        assert!(!report.unapplied[0].touches(&["R1".into()], &["A".into()]));
        // A jumper onto a net the board lacks is a hard error.
        let bad =
            AsBuiltOverlay::parse("[[jumper]]\nfrom = \"A\"\nto = \"NOPE\"\n", "unit").unwrap();
        assert!(bad.apply_to_board(&mut other, &lib).is_err());
    }

    /// A dual transistor (BCM847BS, SOT-363) wired like one Tarski inhibitory
    /// cell: unit 2's base on the switch net, unit 1 diode-connected (base and
    /// collector on one net), both emitters on ground.
    fn dual_bjt_board() -> ExtractedBoard {
        ExtractedBoard::from_kicad_netlist(
            r#"(export (version "E")
  (components
    (comp (ref "Q1") (value "BCM847BS,135") (footprint "Package_TO_SOT_SMD:SOT-363_SC-70-6"))
    (comp (ref "R1") (value "10k") (footprint "Resistor_SMD:R_0402_1005Metric"))
    (comp (ref "R2") (value "1k") (footprint "Resistor_SMD:R_0402_1005Metric")))
  (nets
    (net (code "1") (name "GND")
      (node (ref "Q1") (pin "1") (pinfunction "E1") (pintype "passive"))
      (node (ref "Q1") (pin "4") (pinfunction "E2") (pintype "passive")))
    (net (code "2") (name "SW_A")
      (node (ref "Q1") (pin "5") (pinfunction "B2") (pintype "passive"))
      (node (ref "R1") (pin "1") (pintype "passive")))
    (net (code "3") (name "B1")
      (node (ref "Q1") (pin "2") (pinfunction "B1") (pintype "passive"))
      (node (ref "Q1") (pin "6") (pinfunction "C1") (pintype "passive"))
      (node (ref "R2") (pin "1") (pintype "passive")))
    (net (code "4") (name "C2")
      (node (ref "Q1") (pin "3") (pinfunction "C2") (pintype "passive")))
    (net (code "5") (name "+5V")
      (node (ref "R1") (pin "2") (pintype "passive"))
      (node (ref "R2") (pin "2") (pintype "passive")))))"#,
        )
        .expect("fixture netlist parses")
    }

    fn net_of(board: &ExtractedBoard, reference: &str, pad: &str) -> String {
        let pin = (board.components.iter())
            .find(|c| c.reference == reference)
            .and_then(|c| c.pins.iter().find(|p| p.number == pad))
            .expect("pad exists");
        let id = pin.net.expect("pad is connected");
        board.net(id).expect("net exists").name.clone()
    }

    #[test]
    fn a_cut_severs_the_base_pad_the_simulation_floats_on_the_static_board() {
        let lib = ModelLibrary::builtin();
        let mut board = dual_bjt_board();
        let overlay = AsBuiltOverlay::parse(
            "[[cut]]\nnet_glob = \"SW_*\"\nterminal = \"base\"\n",
            "unit",
        )
        .unwrap();
        // The simulated board floats exactly one base...
        let mut bound = crate::binder::bind_board(&board, &lib);
        overlay
            .apply(&mut bound)
            .expect("the cut describes the bound board");
        let report = overlay.apply_to_board(&mut board, &lib).unwrap();
        // ...and the static board severs that base's pad, and only it.
        assert!(report.unapplied.is_empty(), "{:?}", report.unapplied);
        assert_eq!(net_of(&board, "Q1", "5"), "asbuilt-cut-(Q1-Pad5)");
        assert_eq!(net_of(&board, "R1", "1"), "SW_A", "the switch side stays");
        assert_eq!(net_of(&board, "Q1", "2"), "B1");
        assert!(
            report.lines.iter().any(|l| l.contains("Q1 pad 5")),
            "{:?}",
            report.lines
        );
        // A cut that matches a different number of terminals than declared is
        // refused here exactly as on the simulated board.
        let drifted = AsBuiltOverlay::parse(
            "[[cut]]\nnet_glob = \"SW_*\"\nterminal = \"base\"\nexpect_matches = 2\n",
            "unit",
        )
        .unwrap();
        let err = drifted
            .apply_to_board(&mut dual_bjt_board(), &lib)
            .unwrap_err();
        assert!(matches!(err, AsBuiltError::MatchCount { .. }), "{err}");
    }

    #[test]
    fn a_lift_severs_one_terminal_by_its_pin_role_not_by_its_net() {
        // Unit 1's base and collector share net B1: only the pin role tells
        // the base pad (2) from the collector pad (6).
        let lib = ModelLibrary::builtin();
        let mut board = dual_bjt_board();
        let overlay =
            AsBuiltOverlay::parse("[[lift]]\nref = \"Q1_q1\"\nterminal = \"base\"\n", "unit")
                .unwrap();
        let report = overlay.apply_to_board(&mut board, &lib).unwrap();
        assert!(report.unapplied.is_empty(), "{:?}", report.unapplied);
        assert_eq!(net_of(&board, "Q1", "2"), "asbuilt-lift-(Q1-Pad2)");
        assert_eq!(net_of(&board, "Q1", "6"), "B1", "the collector stays");
        assert_eq!(net_of(&board, "Q1", "5"), "SW_A", "the other unit stays");
    }

    #[test]
    fn a_terminal_with_no_traceable_pad_is_listed_unapplied() {
        let by_ref = HashMap::from([("Q1", 0usize)]);
        assert_eq!(owner(&by_ref, "Q1"), Some((0, String::new())));
        assert_eq!(owner(&by_ref, "Q1_q2"), Some((0, "_q2".to_string())));
        assert_eq!(owner(&by_ref, "Q1_qx"), None);
        assert_eq!(owner(&by_ref, "Q7"), None);
        // A floated terminal whose device is no board part is not dropped.
        let mut board = dual_bjt_board();
        let floated = Floated {
            entries: vec![(
                "lift X9 Base".to_string(),
                "lift",
                vec![FloatedTerminal {
                    device: "X9".to_string(),
                    terminal: Terminal::Base,
                    was: NodeId(1),
                }],
            )],
            net_nodes: HashMap::new(),
        };
        let mut report = AsBuiltReport::default();
        sever_on_board(&mut board, &ModelLibrary::builtin(), floated, &mut report);
        assert_eq!(report.unapplied.len(), 1, "{:?}", report.unapplied);
        assert_eq!(report.unapplied[0].refs, ["X9"]);
        assert!(report.unapplied[0].why.contains("X9 base"));
    }
}
