//! Builder API for constructing KiCad PCB files from scratch.
//!
//! `PcbBuilder::new(version)` → add layers, nets, footprints → `.build() -> Pcb`
//!
//! The emitted skeleton matches the minimal structure required for KiCad 9+ to
//! open the file. Real corpus files were used to determine required sections.

use forge_sexpr::{Document, List, Sexpr, Token};

use crate::pcb::{fmt_f64, Pcb};

/// Accumulated data for building a net entry.
pub struct NetBuilder {
    pub id: i64,
    pub name: String,
}

/// Accumulated data for a layer entry.
pub struct LayerBuilder {
    pub id: i64,
    pub name: String,
    pub kind: String,
}

/// Accumulated data for building a footprint.
pub struct FootprintBuilder {
    pub lib_id: String,
    pub reference: String,
    pub value: String,
    pub at: (f64, f64, f64),
    pub layer: String,
    pub(crate) pads: Vec<PadSpec>,
}

pub(crate) struct PadSpec {
    pub number: String,
    pub kind: String,
    pub shape: String,
    pub at: (f64, f64, f64),
    pub size: (f64, f64),
    pub drill: Option<f64>,
    pub layers: Vec<String>,
    pub net: Option<(i64, String)>,
}

/// Builder for a complete KiCad PCB document.
pub struct PcbBuilder {
    version: i64,
    layers: Vec<LayerBuilder>,
    nets: Vec<NetBuilder>,
    footprints: Vec<FootprintBuilder>,
    segments: Vec<SegSpec>,
    vias: Vec<ViaSpec>,
    gr_lines: Vec<GrLineSpec>,
}

struct SegSpec {
    start: (f64, f64),
    end: (f64, f64),
    width: f64,
    layer: String,
    net: Option<i64>,
}

struct GrLineSpec {
    start: (f64, f64),
    end: (f64, f64),
    width: f64,
    layer: String,
}

struct ViaSpec {
    at: (f64, f64),
    size: f64,
    drill: f64,
    layers: Vec<String>,
    net: Option<i64>,
}

impl PcbBuilder {
    pub fn new(version: i64) -> Self {
        PcbBuilder {
            version,
            layers: Vec::new(),
            nets: Vec::new(),
            footprints: Vec::new(),
            segments: Vec::new(),
            vias: Vec::new(),
            gr_lines: Vec::new(),
        }
    }

    /// Add a standard 2-layer setup (F.Cu / B.Cu + common silkscreen/fab
    /// layers). Convenience helper; you can also call `add_layer` directly.
    pub fn standard_2layer_layers(mut self) -> Self {
        let defs: &[(i64, &str, &str)] = &[
            (0,  "F.Cu",       "signal"),
            (31, "B.Cu",       "signal"),
            (36, "B.SilkS",    "user"),
            (37, "F.SilkS",    "user"),
            (38, "B.Mask",     "user"),
            (39, "F.Mask",     "user"),
            (44, "Edge.Cuts",  "user"),
            (48, "B.Fab",      "user"),
            (49, "F.Fab",      "user"),
        ];
        for (id, name, kind) in defs {
            self.layers.push(LayerBuilder {
                id: *id,
                name: name.to_string(),
                kind: kind.to_string(),
            });
        }
        self
    }

    pub fn add_layer(mut self, id: i64, name: &str, kind: &str) -> Self {
        self.layers.push(LayerBuilder { id, name: name.to_string(), kind: kind.to_string() });
        self
    }

    pub fn add_net(mut self, id: i64, name: &str) -> Self {
        self.nets.push(NetBuilder { id, name: name.to_string() });
        self
    }

    pub fn add_footprint(mut self, fp: FootprintBuilder) -> Self {
        self.footprints.push(fp);
        self
    }

    pub fn add_segment(mut self, start: (f64, f64), end: (f64, f64), width: f64, layer: &str, net: Option<i64>) -> Self {
        self.segments.push(SegSpec { start, end, width, layer: layer.to_string(), net });
        self
    }

    pub fn add_via(mut self, at: (f64, f64), size: f64, drill: f64, layers: Vec<String>, net: Option<i64>) -> Self {
        self.vias.push(ViaSpec { at, size, drill, layers, net });
        self
    }

    /// Add a graphic line (e.g. a board-outline segment on `Edge.Cuts`).
    /// Unlike `add_segment` (a copper track), this emits a `(gr_line ...)` node,
    /// which is how KiCad and downstream tools represent non-copper geometry.
    pub fn add_gr_line(mut self, start: (f64, f64), end: (f64, f64), width: f64, layer: &str) -> Self {
        self.gr_lines.push(GrLineSpec { start, end, width, layer: layer.to_string() });
        self
    }

    /// Consume the builder and produce a [`Pcb`].
    pub fn build(self) -> Pcb {
        let root = build_kicad_pcb(self);
        let doc = Document::new(vec![Sexpr::List(root)], "\n");
        Pcb { doc }
    }
}

fn build_kicad_pcb(b: PcbBuilder) -> List {
    let mut children: Vec<Sexpr> = vec![Sexpr::Token(Token::atom("kicad_pcb"))];

    // (version N)
    children.push(Sexpr::list("version", vec![Sexpr::atom(b.version.to_string())]));
    // (generator "pcbnew")
    children.push(Sexpr::list("generator", vec![Sexpr::Token(Token::string("pcbnew"))]));

    // (general (thickness 1.6))
    let general = Sexpr::list("general", vec![
        Sexpr::list("thickness", vec![Sexpr::atom("1.6")]),
    ]);
    children.push(general);

    // (paper "A4")
    children.push(Sexpr::list("paper", vec![Sexpr::Token(Token::string("A4"))]));

    // (layers ...)
    let mut layer_children: Vec<Sexpr> = vec![Sexpr::Token(Token::atom("layers"))];
    for l in &b.layers {
        let lc = vec![
            Sexpr::atom(l.id.to_string()),
            Sexpr::Token(Token::string(&l.name)),
            Sexpr::atom(l.kind.clone()),
        ];
        // For v5-style: ensure the children have leading spaces.
        let node = Sexpr::List(List::new(lc));
        layer_children.push(node);
    }
    children.push(Sexpr::List(List::new(layer_children)));

    // (setup (pad_to_mask_clearance 0))
    let setup = Sexpr::list("setup", vec![
        Sexpr::list("pad_to_mask_clearance", vec![Sexpr::atom("0")]),
    ]);
    children.push(setup);

    // net declarations
    // Always include net 0 "".
    let has_net0 = b.nets.iter().any(|n| n.id == 0);
    if !has_net0 {
        children.push(Sexpr::list("net", vec![
            Sexpr::atom("0"),
            Sexpr::Token(Token::string("")),
        ]));
    }
    for net in &b.nets {
        children.push(Sexpr::list("net", vec![
            Sexpr::atom(net.id.to_string()),
            Sexpr::Token(Token::string(&net.name)),
        ]));
    }

    // footprints
    for fp in b.footprints {
        children.push(Sexpr::List(build_footprint(fp)));
    }

    // segments
    for seg in b.segments {
        children.push(build_segment_node(seg));
    }

    // vias
    for via in b.vias {
        children.push(build_via_node(via));
    }

    // graphic lines (board outline etc.)
    for gl in b.gr_lines {
        children.push(build_gr_line_node(gl));
    }

    List::new(children)
}

fn build_gr_line_node(gl: GrLineSpec) -> Sexpr {
    Sexpr::list("gr_line", vec![
        Sexpr::list("start", vec![Sexpr::atom(fmt_f64(gl.start.0)), Sexpr::atom(fmt_f64(gl.start.1))]),
        Sexpr::list("end", vec![Sexpr::atom(fmt_f64(gl.end.0)), Sexpr::atom(fmt_f64(gl.end.1))]),
        Sexpr::list("layer", vec![Sexpr::Token(Token::string(&gl.layer))]),
        Sexpr::list("width", vec![Sexpr::atom(fmt_f64(gl.width))]),
    ])
}

fn build_footprint(fp: FootprintBuilder) -> List {
    let mut children: Vec<Sexpr> = vec![Sexpr::Token(Token::atom("footprint"))];
    children.push(Sexpr::Token(Token::string(&fp.lib_id)));
    children.push(Sexpr::list("layer", vec![Sexpr::Token(Token::string(&fp.layer))]));
    children.push(Sexpr::list("at", vec![
        Sexpr::atom(fmt_f64(fp.at.0)),
        Sexpr::atom(fmt_f64(fp.at.1)),
        Sexpr::atom(fmt_f64(fp.at.2)),
    ]));

    // Reference property
    children.push(build_property("Reference", &fp.reference));
    // Value property
    children.push(build_property("Value", &fp.value));

    // Pads
    for pad in fp.pads {
        children.push(Sexpr::List(build_pad(pad)));
    }

    List::new(children)
}

fn build_property(key: &str, value: &str) -> Sexpr {
    Sexpr::list("property", vec![
        Sexpr::Token(Token::string(key)),
        Sexpr::Token(Token::string(value)),
    ])
}

fn build_pad(p: PadSpec) -> List {
    let mut children: Vec<Sexpr> = vec![Sexpr::Token(Token::atom("pad"))];
    children.push(Sexpr::Token(Token::value_token(&p.number)));
    children.push(Sexpr::atom(p.kind));
    children.push(Sexpr::atom(p.shape));
    children.push(Sexpr::list("at", vec![
        Sexpr::atom(fmt_f64(p.at.0)),
        Sexpr::atom(fmt_f64(p.at.1)),
        Sexpr::atom(fmt_f64(p.at.2)),
    ]));
    children.push(Sexpr::list("size", vec![
        Sexpr::atom(fmt_f64(p.size.0)),
        Sexpr::atom(fmt_f64(p.size.1)),
    ]));
    if let Some(d) = p.drill {
        children.push(Sexpr::list("drill", vec![Sexpr::atom(fmt_f64(d))]));
    }
    let layer_tokens: Vec<Sexpr> = p.layers.iter().map(|l| Sexpr::Token(Token::string(l))).collect();
    children.push(Sexpr::list("layers", layer_tokens));
    if let Some((id, name)) = p.net {
        children.push(Sexpr::list("net", vec![
            Sexpr::atom(id.to_string()),
            Sexpr::Token(Token::string(&name)),
        ]));
    }
    List::new(children)
}

fn build_segment_node(s: SegSpec) -> Sexpr {
    let mut args = vec![
        Sexpr::list("start", vec![Sexpr::atom(fmt_f64(s.start.0)), Sexpr::atom(fmt_f64(s.start.1))]),
        Sexpr::list("end",   vec![Sexpr::atom(fmt_f64(s.end.0)),   Sexpr::atom(fmt_f64(s.end.1))]),
        Sexpr::list("width", vec![Sexpr::atom(fmt_f64(s.width))]),
        Sexpr::list("layer", vec![Sexpr::Token(Token::string(&s.layer))]),
    ];
    if let Some(n) = s.net {
        args.push(Sexpr::list("net", vec![Sexpr::atom(n.to_string())]));
    }
    Sexpr::list("segment", args)
}

fn build_via_node(v: ViaSpec) -> Sexpr {
    let layer_tokens: Vec<Sexpr> = v.layers.iter().map(|l| Sexpr::Token(Token::string(l))).collect();
    let mut args = vec![
        Sexpr::list("at",    vec![Sexpr::atom(fmt_f64(v.at.0)), Sexpr::atom(fmt_f64(v.at.1))]),
        Sexpr::list("size",  vec![Sexpr::atom(fmt_f64(v.size))]),
        Sexpr::list("drill", vec![Sexpr::atom(fmt_f64(v.drill))]),
        Sexpr::list("layers", layer_tokens),
    ];
    if let Some(n) = v.net {
        args.push(Sexpr::list("net", vec![Sexpr::atom(n.to_string())]));
    }
    Sexpr::list("via", args)
}

// ---------------------------------------------------------------------------
// FootprintBuilder public API
// ---------------------------------------------------------------------------

impl FootprintBuilder {
    pub fn new(lib_id: &str, reference: &str, value: &str) -> Self {
        FootprintBuilder {
            lib_id: lib_id.to_string(),
            reference: reference.to_string(),
            value: value.to_string(),
            at: (0.0, 0.0, 0.0),
            layer: "F.Cu".to_string(),
            pads: Vec::new(),
        }
    }

    pub fn at(mut self, x: f64, y: f64, rot: f64) -> Self {
        self.at = (x, y, rot);
        self
    }

    pub fn layer(mut self, layer: &str) -> Self {
        self.layer = layer.to_string();
        self
    }

    pub fn add_pad(
        mut self,
        number: &str,
        kind: &str,
        shape: &str,
        at: (f64, f64),
        size: (f64, f64),
        drill: Option<f64>,
        layers: Vec<&str>,
        net: Option<(i64, &str)>,
    ) -> Self {
        self.pads.push(PadSpec {
            number: number.to_string(),
            kind: kind.to_string(),
            shape: shape.to_string(),
            at: (at.0, at.1, 0.0),
            size,
            drill,
            layers: layers.into_iter().map(|s| s.to_string()).collect(),
            net: net.map(|(id, name)| (id, name.to_string())),
        });
        self
    }
}
