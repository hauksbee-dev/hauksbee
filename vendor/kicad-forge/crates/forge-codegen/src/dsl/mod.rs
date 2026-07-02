//! Board-as-Code: an executable, round-trippable DSL for a PCB.
//!
//! The decompiler in the sibling modules produces a *readable* program
//! ([`crate::decompile`]) and proves structural coverage by rebuilding a board
//! from the in-memory [`Analysis`] ([`crate::rebuild`]). Neither path is
//! *executable from text*: the readable program drops net assignments, and the
//! rebuild reads the analysis struct rather than parsing source.
//!
//! This module closes that loop. It defines a small, line-oriented DSL that
//! carries everything needed to re-emit a board with full connectivity:
//!
//! * `board version N` header,
//! * `fn <name>(...) { comp ... }` blocks grouping a repeated cluster's
//!   components (so an editor sees the structure the decompiler found),
//! * `fn main { ... }` that declares nets and instantiates the blocks plus the
//!   singletons, each component carrying its concrete pads and per-pad nets.
//!
//! The DSL is:
//!
//! * **executable**: [`Program::parse`] reads text into a [`Program`] and
//!   [`Program::build`] interprets it into a [`forge_model::Pcb`];
//! * **round-trippable on connectivity**: board -> [`from_board`] -> text ->
//!   [`Program::parse`] -> [`Program::build`] -> board' preserves the component
//!   set and net connectivity (checked with [`crate::semantics`] /
//!   [`crate::compare`]); byte-exactness is explicitly *not* a goal here;
//! * **editable**: changing a `val "..."`, a `pad N net X`, or a `space`
//!   distance field in the text and rebuilding is reflected in the board.
//!
//! Pad geometry (kind, shape, local offset, size, drill, layers) is preserved
//! so the rebuilt board carries enough to route. Distance fields (`space`)
//! drive the logical re-layout placer in [`crate::layout`].

mod build;
mod emit;
mod model;
mod parse;

pub use model::{Block, Comp, Edge, Instance, Outline, Pad, Program, Space, Stmt};

use forge_model::Pcb;

/// Decompile a board directly into executable Board-as-Code text.
///
/// Deterministic: identical board in, identical text out. The function grouping
/// follows the repeat-detection [`crate::Analysis`]; every component appears
/// exactly once (inside a block instantiation or as a singleton), each carrying
/// its concrete pads and nets.
pub fn to_code(pcb: &Pcb) -> String {
    let prog = from_board(pcb);
    prog.emit()
}

/// Build a [`Program`] from a parsed board, grouped by the decompiler's
/// repeat-detection analysis.
pub fn from_board(pcb: &Pcb) -> Program {
    emit::program_from_board(pcb)
}
