//! In-memory representation of a Board-as-Code program.
//!
//! A [`Program`] is a list of named [`Block`]s (function definitions, one per
//! repeated cluster the decompiler found) plus a `main` body of [`Stmt`]s. The
//! body declares nets, instantiates blocks, and places singleton components.
//!
//! Every concrete component is a [`Comp`]: identity (`lib_id`, `value`),
//! placement (`at`, `rot`), an optional [`Space`] distance field for the
//! re-layout placer, and its [`Pad`]s with per-pad net assignments. A [`Block`]
//! defines the *shared* slot layout (part type + pad shapes) once; each
//! instantiation supplies the concrete per-instance components.

/// A whole board expressed as code.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    /// KiCad format version to emit (`board version N`).
    pub version: i64,
    /// Block (function) definitions, one per repeated cluster. Ordered for
    /// determinism.
    pub blocks: Vec<Block>,
    /// The `main` body: net declarations, block instantiations and singletons.
    pub body: Vec<Stmt>,
    /// The board outline as a rectangle `(min_x, min_y, max_x, max_y)` in mm.
    ///
    /// Populated from the source board's `Edge.Cuts` geometry on decompile, or
    /// from a `board size W H` / `board outline ...` statement. The re-layout
    /// placer keeps every component (courtyard included) inside this rectangle.
    /// `None` means "unconstrained" (the placer falls back to an auto box).
    pub outline: Option<Outline>,
}

/// A rectangular board outline, in board (mm) coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Outline {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Outline {
    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }
    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }
    pub fn center(&self) -> (f64, f64) {
        ((self.min_x + self.max_x) * 0.5, (self.min_y + self.max_y) * 0.5)
    }
}

/// Which board edge a component is pinned to (deliverable 2: position
/// constraints). The placer holds the component against that edge and only
/// relaxes its free coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub fn parse(s: &str) -> Option<Edge> {
        match s {
            "left" => Some(Edge::Left),
            "right" => Some(Edge::Right),
            "top" => Some(Edge::Top),
            "bottom" => Some(Edge::Bottom),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Edge::Left => "left",
            Edge::Right => "right",
            Edge::Top => "top",
            Edge::Bottom => "bottom",
        }
    }
}

/// A reusable block: the slot layout shared by every instance of a cluster.
///
/// The block records, per slot, the expected part type and pad shapes so the
/// code reads as a real function ("a synapse is a BCM857BS plus a switch plus
/// six resistors"). The concrete per-instance components live in the
/// [`Stmt::Instance`] that calls the block, so anomalies (a wrong-value part)
/// remain visible at the call site rather than being smoothed into the
/// template.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub name: String,
    /// Per-slot template: `(lib_id, value)` by majority vote, for documentation
    /// and editor affordance. Length == number of slots.
    pub slots: Vec<SlotSpec>,
    /// How many times this block is instantiated (for the doc comment).
    pub instances: usize,
}

/// One slot in a block template.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotSpec {
    pub lib_id: String,
    pub value: String,
    pub pad_count: usize,
}

/// A statement in the `main` body.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// `net <name>` — declare a net. Declaration order fixes the emitted net id
    /// table; pads referencing an undeclared net auto-declare it.
    Net(String),
    /// `space fn <block> <dist>` — a clearance distance field applied to every
    /// instance of a whole block/function (deliverable 3).
    BlockSpace { block: String, dist: f64 },
    /// `pin <ref> edge <left|right|top|bottom>` - hold a component against a
    /// board edge during re-layout. The placer fixes the edge-normal coordinate
    /// and relaxes only the along-edge coordinate.
    Pin { reference: String, edge: Edge },
    /// `lock <ref>` - never move this component during re-layout. Its exact
    /// coordinates are preserved and it acts as a fixed keep-out for everything
    /// else.
    Lock { reference: String },
    /// An instantiation of a block: the concrete components for one instance.
    Instance(Instance),
    /// A singleton component placed inline (not part of any repeated cluster).
    Single(Comp),
}

/// One instantiation of a [`Block`].
#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    /// Block name being instantiated.
    pub block: String,
    /// The concrete components, slot-aligned to the block's `slots`. A `None`
    /// marks a slot with no component in this instance (a missing part).
    pub comps: Vec<Option<Comp>>,
}

/// A concrete component with full geometry and connectivity.
#[derive(Debug, Clone, PartialEq)]
pub struct Comp {
    pub reference: String,
    pub lib_id: String,
    pub value: String,
    pub layer: String,
    /// World placement `(x, y)` in mm.
    pub at: (f64, f64),
    /// Rotation in degrees.
    pub rot: f64,
    /// Optional clearance distance field (mm) kept around this component by the
    /// re-layout placer.
    pub space: Option<Space>,
    pub pads: Vec<Pad>,
}

/// A clearance distance field: keep at least `dist` mm clear around the owner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Space {
    pub dist: f64,
}

/// A pad with geometry and net.
#[derive(Debug, Clone, PartialEq)]
pub struct Pad {
    /// Pad number/name as it appears in the footprint.
    pub number: String,
    pub kind: String,
    pub shape: String,
    /// Local offset from the footprint origin `(x, y)`.
    pub at: (f64, f64),
    pub size: (f64, f64),
    pub drill: Option<f64>,
    pub layers: Vec<String>,
    /// Net name; `None` for an unconnected pad.
    pub net: Option<String>,
}

impl Comp {
    /// All net names referenced by this component's pads, in pad order, deduped.
    pub fn nets(&self) -> Vec<&str> {
        let mut out = Vec::new();
        for p in &self.pads {
            if let Some(n) = &p.net {
                if !out.contains(&n.as_str()) {
                    out.push(n.as_str());
                }
            }
        }
        out
    }
}

impl Program {
    /// Iterate every concrete component in deterministic body order.
    pub fn comps(&self) -> impl Iterator<Item = &Comp> {
        self.body.iter().flat_map(|s| -> Box<dyn Iterator<Item = &Comp>> {
            match s {
                Stmt::Instance(inst) => Box::new(inst.comps.iter().flatten()),
                Stmt::Single(c) => Box::new(std::iter::once(c)),
                _ => Box::new(std::iter::empty()),
            }
        })
    }

    /// Mutable access to every concrete component in body order.
    pub fn comps_mut(&mut self) -> impl Iterator<Item = &mut Comp> {
        self.body.iter_mut().flat_map(|s| -> Box<dyn Iterator<Item = &mut Comp>> {
            match s {
                Stmt::Instance(inst) => Box::new(inst.comps.iter_mut().flatten()),
                Stmt::Single(c) => Box::new(std::iter::once(c)),
                _ => Box::new(std::iter::empty()),
            }
        })
    }

    /// Find a component by reference designator (mutable).
    pub fn comp_mut(&mut self, reference: &str) -> Option<&mut Comp> {
        self.comps_mut().find(|c| c.reference == reference)
    }
}
