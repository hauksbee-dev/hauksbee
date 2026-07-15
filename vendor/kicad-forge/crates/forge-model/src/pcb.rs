//! Typed views over a KiCad PCB CST.
//!
//! Version compatibility notes discovered from real corpus files:
//!
//! v5 (20171130, stormduino): `(module lib:name ...)` with `(tstamp ...)`,
//!   `(fp_text reference R1 ...)` for reference/value, pads use bare atoms for
//!   layers (`*.Cu` not `"*.Cu"`), net refs are `(net N name)` where name may
//!   be unquoted atom or quoted string.
//!
//! v9 (20241229, microwave): `(footprint "lib:name" ...)` with `(uuid ...)`,
//!   `(property "Reference" "R1" ...)` for reference/value, all strings quoted.
//!
//! v10 (20260206, pic_programmer): same as v9 but net in pads/segments/vias is
//!   `(net "NetName")` - just the name string, no ID number.
//!
//! Pad `at` rotation in kicad_pcb footprints: in kicad_pcb the pad `at`
//! rotation IS absolute (not relative to the footprint), meaning the total
//! visual rotation is just the pad's own rotation. However for computing
//! absolute XY position we still need to rotate the pad's XY offset by the
//! footprint rotation.

use forge_sexpr::{parse, Document, List, Sexpr, Token};
use std::f64::consts::PI;

use crate::Error;

/// A parsed KiCad PCB file. Owns the underlying CST document.
pub struct Pcb {
    pub(crate) doc: Document,
}

impl Pcb {
    /// Parse a `.kicad_pcb` file, verifying the root node is `kicad_pcb`.
    pub fn parse(text: &str) -> Result<Pcb, Error> {
        let doc = parse(text)?;
        let root_name = doc
            .root()
            .and_then(|l| l.name())
            .unwrap_or("")
            .to_string();
        if root_name != "kicad_pcb" {
            return Err(Error::NotPcb(root_name));
        }
        Ok(Pcb { doc })
    }

    /// Emit the document. Byte-identical to the source if unmodified.
    pub fn emit(&self) -> String {
        self.doc.emit()
    }

    /// Emit with KiCad-style pretty-printing.
    pub fn emit_pretty(&self) -> String {
        self.doc.emit_pretty()
    }

    fn root(&self) -> &List {
        self.doc.root().expect("already verified root exists")
    }

    fn root_mut(&mut self) -> &mut List {
        self.doc.root_mut().expect("already verified root exists")
    }

    /// KiCad format version token (e.g. 20241229).
    pub fn version(&self) -> i64 {
        self.root().find_i64("version").unwrap_or(0)
    }

    /// Top-level `(net N "name")` declarations.
    pub fn nets(&self) -> Vec<Net> {
        self.root()
            .find_all("net")
            .filter_map(Net::from_list)
            .collect()
    }

    /// All `(footprint ...)` or `(module ...)` children.
    pub fn footprints(&self) -> Vec<Footprint<'_>> {
        self.root()
            .lists()
            .filter(|l| matches!(l.name(), Some("footprint") | Some("module")))
            .map(|l| Footprint { list: l })
            .collect()
    }

    /// All `(segment ...)` children.
    pub fn segments(&self) -> Vec<Segment<'_>> {
        self.root()
            .find_all("segment")
            .map(|l| Segment { list: l })
            .collect()
    }

    /// All `(via ...)` children.
    pub fn vias(&self) -> Vec<Via<'_>> {
        self.root()
            .find_all("via")
            .map(|l| Via { list: l })
            .collect()
    }

    /// All `(zone ...)` children.
    pub fn zones(&self) -> Vec<Zone<'_>> {
        self.root()
            .find_all("zone")
            .map(|l| Zone { list: l })
            .collect()
    }

    /// All top-level `(arc ...)` children (track arcs).
    pub fn arcs(&self) -> Vec<TrackArc<'_>> {
        self.root()
            .find_all("arc")
            .map(|l| TrackArc { list: l })
            .collect()
    }

    /// Layer definitions.
    pub fn layers(&self) -> Vec<Layer> {
        self.root()
            .find("layers")
            .map(|l| l.lists().filter_map(Layer::from_list).collect())
            .unwrap_or_default()
    }

    /// The `(general ...)` block.
    pub fn general(&self) -> General<'_> {
        // Unwrap-safe: every well-formed PCB has general; fall back to a
        // synthetic empty list if somehow missing.
        let list = self.root().find("general");
        General { list }
    }

    /// All `(gr_line ...)` children.
    pub fn gr_lines(&self) -> Vec<GrLine<'_>> {
        self.root()
            .find_all("gr_line")
            .map(|l| GrLine { list: l })
            .collect()
    }

    /// All `(gr_text ...)` children.
    pub fn gr_texts(&self) -> Vec<GrText<'_>> {
        self.root()
            .find_all("gr_text")
            .map(|l| GrText { list: l })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Mutation API
    // -----------------------------------------------------------------------

    /// Get a mutable view of the footprint identified by its `Reference`.
    pub fn footprint_mut(&mut self, reference: &str) -> Result<FootprintMut<'_>, Error> {
        let root = self.root_mut();
        let list = root
            .children
            .iter_mut()
            .filter_map(|c| match c {
                Sexpr::List(l) if matches!(l.name(), Some("footprint") | Some("module")) => Some(l),
                _ => None,
            })
            .find(|l| {
                let fp = Footprint { list: l };
                fp.reference().as_deref() == Some(reference)
            })
            .ok_or_else(|| Error::FootprintNotFound(reference.to_string()))?;
        Ok(FootprintMut { list })
    }

    /// Add a routed segment. The new node is appended to the root list.
    pub fn add_segment(&mut self, start: (f64, f64), end: (f64, f64), width: f64, layer: &str, net: Option<i64>) {
        let mut args = vec![
            Sexpr::list("start", vec![Sexpr::atom(fmt_f64(start.0)), Sexpr::atom(fmt_f64(start.1))]),
            Sexpr::list("end",   vec![Sexpr::atom(fmt_f64(end.0)),   Sexpr::atom(fmt_f64(end.1))]),
            Sexpr::list("width", vec![Sexpr::atom(fmt_f64(width))]),
            Sexpr::list("layer", vec![Sexpr::Token(Token::string(layer))]),
        ];
        if let Some(n) = net {
            args.push(Sexpr::list("net", vec![Sexpr::atom(n.to_string())]));
        }
        let node = Sexpr::list("segment", args);
        self.root_mut().push(node);
    }

    /// Add a via. `layers` should be `["F.Cu", "B.Cu"]` for standard vias.
    pub fn add_via(&mut self, at: (f64, f64), size: f64, drill: f64, layers: &[&str], net: Option<i64>) {
        let layers_nodes: Vec<Sexpr> = layers.iter().map(|l| Sexpr::Token(Token::string(l))).collect();
        let mut args = vec![
            Sexpr::list("at",    vec![Sexpr::atom(fmt_f64(at.0)), Sexpr::atom(fmt_f64(at.1))]),
            Sexpr::list("size",  vec![Sexpr::atom(fmt_f64(size))]),
            Sexpr::list("drill", vec![Sexpr::atom(fmt_f64(drill))]),
            Sexpr::list("layers", layers_nodes),
        ];
        if let Some(n) = net {
            args.push(Sexpr::list("net", vec![Sexpr::atom(n.to_string())]));
        }
        let node = Sexpr::list("via", args);
        self.root_mut().push(node);
    }

    /// Renumber a net: updates `(net N "name")` declaration and all pad/
    /// segment/via `(net N ...)` references with numeric IDs.
    pub fn renumber_net(&mut self, old_id: i64, new_id: i64) {
        let root = self.root_mut();
        for child in root.children.iter_mut() {
            if let Sexpr::List(l) = child {
                renumber_net_in_list(l, old_id, new_id);
            }
        }
    }
}

/// Recursively update numeric net IDs in a list tree.
fn renumber_net_in_list(list: &mut List, old_id: i64, new_id: i64) {
    if list.name() == Some("net") {
        // Could be top-level `(net N "name")` or inline `(net N name)`.
        if let Some(Sexpr::Token(t)) = list.children.get_mut(1) {
            if t.as_i64() == Some(old_id) {
                t.raw = new_id.to_string().into();
                return; // Don't recurse into net children.
            }
        }
    }
    for child in list.children.iter_mut() {
        if let Sexpr::List(l) = child {
            renumber_net_in_list(l, old_id, new_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Net
// ---------------------------------------------------------------------------

/// A top-level net declaration: `(net N "name")`.
#[derive(Debug, Clone)]
pub struct Net {
    pub id: i64,
    pub name: String,
}

impl Net {
    fn from_list(l: &List) -> Option<Self> {
        let id = l.arg_i64(0)?;
        let name = l.arg_value(1).unwrap_or_default();
        Some(Net { id, name })
    }
}

// ---------------------------------------------------------------------------
// Layer
// ---------------------------------------------------------------------------

/// A layer entry from the `(layers ...)` block.
#[derive(Debug, Clone)]
pub struct Layer {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub canonical_name: Option<String>,
}

impl Layer {
    fn from_list(l: &List) -> Option<Self> {
        // Layer entries look like `(0 "F.Cu" signal)` or `(0 F.Cu signal)`.
        // The first child (children[0]) is the numeric id (acts as the "name"
        // position in the list).  arg(N) = children[N+1], so:
        //   id   = children[0] = name()
        //   name = children[1] = arg(0)
        //   kind = children[2] = arg(1)
        //   canonical = children[3] = arg(2)
        let id_str = l.name()?;
        let id: i64 = id_str.parse().ok()?;
        let name = l.arg_value(0)?;
        let kind = l.arg_value(1).unwrap_or_default();
        let canonical_name = l.arg_value(2);
        Some(Layer { id, name, kind, canonical_name })
    }
}

// ---------------------------------------------------------------------------
// General
// ---------------------------------------------------------------------------

/// View of the `(general ...)` block.
pub struct General<'a> {
    list: Option<&'a List>,
}

impl General<'_> {
    pub fn thickness(&self) -> Option<f64> {
        self.list.and_then(|l| l.find_f64("thickness"))
    }
}

// ---------------------------------------------------------------------------
// Footprint
// ---------------------------------------------------------------------------

/// A borrowed view of a `(footprint ...)` or `(module ...)` list.
pub struct Footprint<'a> {
    pub(crate) list: &'a List,
}

impl<'a> Footprint<'a> {
    /// Library ID string, e.g. `"Resistor_SMD:R_0402"`. First string argument.
    pub fn lib_id(&self) -> String {
        self.list.arg_value(0).unwrap_or_default()
    }

    /// Position and rotation `(at x y rot?)`.
    pub fn at(&self) -> (f64, f64, f64) {
        let at = match self.list.find("at") {
            Some(l) => l,
            None => return (0.0, 0.0, 0.0),
        };
        let x = at.arg_f64(0).unwrap_or(0.0);
        let y = at.arg_f64(1).unwrap_or(0.0);
        let rot = at.arg_f64(2).unwrap_or(0.0);
        (x, y, rot)
    }

    /// Layer the footprint is on.
    pub fn layer(&self) -> String {
        self.list.find_value("layer").unwrap_or_default()
    }

    /// Reference designator. Works for v5 (`fp_text reference`) and v6+
    /// (`property "Reference"`).
    pub fn reference(&self) -> Option<String> {
        fp_text_or_property(self.list, "reference", "Reference")
    }

    /// Value field.
    pub fn value(&self) -> Option<String> {
        fp_text_or_property(self.list, "value", "Value")
    }

    /// UUID (v8+ `(uuid ...)`) or legacy timestamp (`(tstamp ...)`).
    pub fn uuid(&self) -> Option<String> {
        self.list
            .find_value("uuid")
            .or_else(|| self.list.find_value("tstamp"))
    }

    /// All `(pad ...)` children.
    pub fn pads(&self) -> Vec<Pad<'_>> {
        self.list
            .find_all("pad")
            .map(|l| Pad { list: l })
            .collect()
    }

    /// All `(property "key" "value" ...)` children.
    pub fn properties(&self) -> Vec<(String, String)> {
        self.list
            .find_all("property")
            .filter_map(|l| {
                let key = l.arg_value(0)?;
                let val = l.arg_value(1).unwrap_or_default();
                Some((key, val))
            })
            .collect()
    }
}

/// Get the value of an fp_text field (v5) or property (v6+).
fn fp_text_or_property(list: &List, fp_text_kind: &str, property_name: &str) -> Option<String> {
    // Try v6+ property first.
    for l in list.find_all("property") {
        if l.arg_value(0).as_deref() == Some(property_name) {
            return l.arg_value(1);
        }
    }
    // Fall back to v5 fp_text.
    for l in list.find_all("fp_text") {
        if l.arg_value(0).as_deref() == Some(fp_text_kind) {
            return l.arg_value(1);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// FootprintMut
// ---------------------------------------------------------------------------

/// Mutable view of a footprint list for CST-editing operations.
pub struct FootprintMut<'a> {
    pub(crate) list: &'a mut List,
}

impl FootprintMut<'_> {
    /// Move the footprint: update (or insert) the `(at x y rot)` child.
    pub fn set_at(&mut self, x: f64, y: f64, rot: f64) {
        let args = vec![
            Sexpr::atom(fmt_f64(x)),
            Sexpr::atom(fmt_f64(y)),
            Sexpr::atom(fmt_f64(rot)),
        ];
        if let Some(at) = self.list.find_mut("at") {
            at.children.truncate(1); // keep the "at" keyword
            at.children.extend(args.into_iter().map(|a| {
                let mut tok = a;
                if let Sexpr::Token(ref mut t) = tok {
                    t.leading = " ".into();
                }
                tok
            }));
        } else {
            let node = Sexpr::list("at", args);
            self.list.push(node);
        }
    }

    /// Update the reference designator.
    pub fn set_reference(&mut self, reference: &str) {
        // Try v6+ property first.
        for l in self.list.children.iter_mut() {
            if let Sexpr::List(l) = l {
                if l.name() == Some("property")
                    && l.arg_value(0).as_deref() == Some("Reference")
                {
                    if let Some(Sexpr::Token(t)) = l.children.get_mut(2) {
                        t.raw = forge_sexpr::quote(reference).into();
                        return;
                    }
                }
            }
        }
        // Fall back to v5 fp_text.
        for l in self.list.children.iter_mut() {
            if let Sexpr::List(l) = l {
                if l.name() == Some("fp_text")
                    && l.arg_value(0).as_deref() == Some("reference")
                {
                    if let Some(Sexpr::Token(t)) = l.children.get_mut(2) {
                        t.raw = forge_sexpr::quote(reference).into();
                        return;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pad
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PadKind {
    Smd,
    ThruHole,
    NpThruHole,
    Connect,
    Unknown,
}

impl PadKind {
    fn from_str(s: &str) -> Self {
        match s {
            "smd" => PadKind::Smd,
            "thru_hole" => PadKind::ThruHole,
            "np_thru_hole" => PadKind::NpThruHole,
            "connect" => PadKind::Connect,
            _ => PadKind::Unknown,
        }
    }
}

/// A borrowed view of a `(pad ...)` list.
pub struct Pad<'a> {
    pub(crate) list: &'a List,
}

impl<'a> Pad<'a> {
    /// Pad number string, e.g. `"1"`.
    pub fn number(&self) -> String {
        self.list.arg_value(0).unwrap_or_default()
    }

    /// Pad kind: smd, thru_hole, np_thru_hole, connect.
    pub fn kind(&self) -> PadKind {
        self.list
            .arg_value(1)
            .as_deref()
            .map(PadKind::from_str)
            .unwrap_or(PadKind::Unknown)
    }

    /// Pad shape string, e.g. `"circle"`, `"rect"`, `"oval"`.
    pub fn shape(&self) -> String {
        self.list.arg_value(2).unwrap_or_default()
    }

    /// Local position and rotation `(at x y rot?)`.
    pub fn at(&self) -> (f64, f64, f64) {
        let at = match self.list.find("at") {
            Some(l) => l,
            None => return (0.0, 0.0, 0.0),
        };
        let x = at.arg_f64(0).unwrap_or(0.0);
        let y = at.arg_f64(1).unwrap_or(0.0);
        let rot = at.arg_f64(2).unwrap_or(0.0);
        (x, y, rot)
    }

    /// Pad size `(size w h)`.
    pub fn size(&self) -> (f64, f64) {
        let sz = match self.list.find("size") {
            Some(l) => l,
            None => return (0.0, 0.0),
        };
        (sz.arg_f64(0).unwrap_or(0.0), sz.arg_f64(1).unwrap_or(0.0))
    }

    /// Net assignment. Returns `(id, name)` for numeric-ID files (v5–v9) or
    /// `(0, name)` for v10 string-only nets.
    pub fn net(&self) -> Option<(i64, String)> {
        let net = self.list.find("net")?;
        // v10: (net "GND")
        if net.arg_i64(0).is_none() {
            let name = net.arg_value(0)?;
            return Some((0, name));
        }
        // v5–v9: (net N name) or (net N "name")
        let id = net.arg_i64(0)?;
        let name = net.arg_value(1).unwrap_or_default();
        Some((id, name))
    }

    /// Layer list for this pad.
    pub fn layers(&self) -> Vec<String> {
        self.list
            .find("layers")
            .map(|l| {
                l.children
                    .iter()
                    .skip(1)
                    .filter_map(|c| c.as_token())
                    .map(|t| t.value())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Drill diameter if present `(drill d)`.
    pub fn drill(&self) -> Option<f64> {
        self.list.find_f64("drill")
    }

    /// Absolute position of this pad, accounting for footprint placement and
    /// rotation.
    ///
    /// KiCad PCB rule: the pad `at` (x, y) offset is defined in footprint-
    /// local space. To get world coordinates we rotate the offset by the
    /// footprint rotation angle, then add the footprint origin.
    ///
    /// The pad `at` rotation field in a kicad_pcb file is absolute (not
    /// relative to the footprint), so it does not affect XY position.
    pub fn absolute_pos(&self, fp: &Footprint) -> (f64, f64) {
        let (fx, fy, frot) = fp.at();
        let (px, py, _) = self.at();
        let angle = frot * PI / 180.0;
        // KiCad's board frame is y-DOWN with CCW-positive rotation, so the
        // world offset is (px·cos + py·sin, −px·sin + py·cos) — NOT the textbook
        // y-up matrix. The y-up form (px·cos − py·sin, px·sin + py·cos) mirrors
        // X for any non-0/180° footprint (e.g. frot=90 sends a pad at (0, py) to
        // −py instead of +py). This matches hauksbee-extract's pcb/drc/si pad
        // transforms, which document the same y-down derivation.
        let rx = px * angle.cos() + py * angle.sin();
        let ry = -px * angle.sin() + py * angle.cos();
        (fx + rx, fy + ry)
    }
}

// ---------------------------------------------------------------------------
// Segment
// ---------------------------------------------------------------------------

/// A routed track segment.
pub struct Segment<'a> {
    pub(crate) list: &'a List,
}

impl<'a> Segment<'a> {
    pub fn start(&self) -> (f64, f64) {
        xy(self.list.find("start"))
    }

    pub fn end(&self) -> (f64, f64) {
        xy(self.list.find("end"))
    }

    pub fn width(&self) -> f64 {
        self.list.find_f64("width").unwrap_or(0.0)
    }

    pub fn layer(&self) -> String {
        self.list.find_value("layer").unwrap_or_default()
    }

    /// Net ID (v5–v9) or 0 (v10 where net is a string).
    pub fn net_id(&self) -> i64 {
        net_id(self.list)
    }

    /// Net name (all versions).
    pub fn net_name(&self) -> Option<String> {
        net_name(self.list)
    }
}

// ---------------------------------------------------------------------------
// Via
// ---------------------------------------------------------------------------

/// A via.
pub struct Via<'a> {
    pub(crate) list: &'a List,
}

impl<'a> Via<'a> {
    pub fn at(&self) -> (f64, f64) {
        xy(self.list.find("at"))
    }

    pub fn size(&self) -> f64 {
        self.list.find_f64("size").unwrap_or(0.0)
    }

    pub fn drill(&self) -> f64 {
        self.list.find_f64("drill").unwrap_or(0.0)
    }

    pub fn layers(&self) -> Vec<String> {
        self.list
            .find("layers")
            .map(|l| {
                l.children
                    .iter()
                    .skip(1)
                    .filter_map(|c| c.as_token())
                    .map(|t| t.value())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Net ID or 0 for v10 string-net format.
    pub fn net_id(&self) -> i64 {
        net_id(self.list)
    }

    pub fn net_name(&self) -> Option<String> {
        net_name(self.list)
    }
}

// ---------------------------------------------------------------------------
// Zone
// ---------------------------------------------------------------------------

/// A copper zone/fill.
pub struct Zone<'a> {
    pub(crate) list: &'a List,
}

impl<'a> Zone<'a> {
    pub fn net_id(&self) -> i64 {
        net_id(self.list)
    }

    pub fn net_name(&self) -> Option<String> {
        net_name(self.list)
    }

    pub fn layer(&self) -> String {
        self.list.find_value("layer").unwrap_or_default()
    }

    /// Outline polygon points from the first `(polygon (pts ...))` child.
    pub fn outline_pts(&self) -> Vec<(f64, f64)> {
        self.list
            .find("polygon")
            .and_then(|p| p.find("pts"))
            .map(pts_list)
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// TrackArc
// ---------------------------------------------------------------------------

/// A top-level arc track.
pub struct TrackArc<'a> {
    pub(crate) list: &'a List,
}

impl<'a> TrackArc<'a> {
    pub fn start(&self) -> (f64, f64) {
        xy(self.list.find("start"))
    }

    pub fn mid(&self) -> (f64, f64) {
        xy(self.list.find("mid"))
    }

    pub fn end(&self) -> (f64, f64) {
        xy(self.list.find("end"))
    }

    pub fn width(&self) -> f64 {
        self.list.find_f64("width").unwrap_or(0.0)
    }

    pub fn layer(&self) -> String {
        self.list.find_value("layer").unwrap_or_default()
    }

    pub fn net_id(&self) -> i64 {
        net_id(self.list)
    }
}

// ---------------------------------------------------------------------------
// GrLine / GrText
// ---------------------------------------------------------------------------

/// A graphical line on a non-copper layer.
pub struct GrLine<'a> {
    pub(crate) list: &'a List,
}

impl<'a> GrLine<'a> {
    pub fn start(&self) -> (f64, f64) {
        xy(self.list.find("start"))
    }

    pub fn end(&self) -> (f64, f64) {
        xy(self.list.find("end"))
    }

    pub fn layer(&self) -> String {
        self.list.find_value("layer").unwrap_or_default()
    }
}

/// A graphical text element.
pub struct GrText<'a> {
    pub(crate) list: &'a List,
}

impl<'a> GrText<'a> {
    pub fn text(&self) -> String {
        self.list.arg_value(0).unwrap_or_default()
    }

    pub fn at(&self) -> (f64, f64) {
        xy(self.list.find("at"))
    }

    pub fn layer(&self) -> String {
        self.list.find_value("layer").unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract (x, y) from an `(at ...)` or `(start ...)` etc. list.
fn xy(list: Option<&List>) -> (f64, f64) {
    let l = match list {
        Some(l) => l,
        None => return (0.0, 0.0),
    };
    (l.arg_f64(0).unwrap_or(0.0), l.arg_f64(1).unwrap_or(0.0))
}

/// Extract points from `(pts (xy x y) (xy x y) ...)`.
fn pts_list(pts: &List) -> Vec<(f64, f64)> {
    pts.find_all("xy")
        .map(|l| (l.arg_f64(0).unwrap_or(0.0), l.arg_f64(1).unwrap_or(0.0)))
        .collect()
}

/// Extract a numeric net ID from a list containing `(net N ...)`.
/// Returns 0 for v10 where the net is `(net "name")`.
fn net_id(list: &List) -> i64 {
    list.find("net").and_then(|n| n.arg_i64(0)).unwrap_or(0)
}

/// Extract a net name from `(net N name)` (v5–v9) or `(net "name")` (v10).
fn net_name(list: &List) -> Option<String> {
    let net = list.find("net")?;
    // v10: first arg is a string name
    if net.arg_i64(0).is_none() {
        return net.arg_value(0);
    }
    // v5–v9: second arg is the name
    net.arg_value(1)
}

/// Format an f64 for KiCad output: up to 6 decimal places, no trailing zeros.
pub(crate) fn fmt_f64(v: f64) -> String {
    // Check for integer value.
    if v.fract() == 0.0 && v.abs() < 1e12 {
        return format!("{}", v as i64);
    }
    // Up to 6 decimal places, trim trailing zeros.
    let s = format!("{:.6}", v);
    let trimmed = s.trim_end_matches('0');
    let trimmed = trimmed.trim_end_matches('.');
    trimmed.to_string()
}

#[cfg(test)]
mod pad_transform_tests {
    use super::Pcb;

    // A pad offset (0, 5) inside a footprint placed at (10, 20) rotated 90°
    // (KiCad y-down, CCW-positive) lands at world (15, 20): the +Y local offset
    // rotates onto +X. The old y-up matrix wrongly produced (5, 20) — a mirror.
    #[test]
    fn rotated_pad_absolute_pos_uses_kicad_y_down_frame() {
        let text = r#"(kicad_pcb
  (footprint "R_test" (at 10 20 90)
    (pad "1" smd rect (at 0 5) (size 1 1))
  )
)"#;
        let pcb = Pcb::parse(text).expect("parse");
        let fp = pcb.footprints().into_iter().next().expect("one footprint");
        let pad = fp.pads().into_iter().next().expect("one pad");
        let (x, y) = pad.absolute_pos(&fp);
        assert!((x - 15.0).abs() < 1e-9, "x = {x}, expected 15.0 (y-down rotation)");
        assert!((y - 20.0).abs() < 1e-9, "y = {y}, expected 20.0");

        // 270° sends +Y local onto −X: world (5, 20).
        let text270 = text.replace("(at 10 20 90)", "(at 10 20 270)");
        let pcb = Pcb::parse(&text270).expect("parse");
        let fp = pcb.footprints().into_iter().next().unwrap();
        let pad = fp.pads().into_iter().next().unwrap();
        let (x, _y) = pad.absolute_pos(&fp);
        assert!((x - 5.0).abs() < 1e-9, "x = {x}, expected 5.0 at 270°");
    }
}
