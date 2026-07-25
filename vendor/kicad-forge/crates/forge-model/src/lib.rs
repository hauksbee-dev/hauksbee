//! Typed views over the forge-sexpr CST for KiCad PCB and schematic files.
//!
//! Design principle: every accessor reads from the underlying [`List`] on
//! demand; mutations edit the CST in place; unknown fields are preserved
//! losslessly because only the parts we care about are touched.
//!
//! Supports KiCad versions 5 through 10 (version tokens 20171130..20260206).

mod builder;
mod error;
mod pcb;
mod schematic;

pub use builder::{FootprintBuilder, LayerBuilder, NetBuilder, PcbBuilder};
pub use error::Error;
pub use pcb::{
    fmt_f64, Footprint, FootprintMut, General, GrLine, GrText, Layer, Net, Pad, PadKind, Pcb,
    Segment, TrackArc, Via, Zone,
};
pub use schematic::{Schematic, SchematicSheet, SchematicSymbol};
