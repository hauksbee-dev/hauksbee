//! hauksbee-checks: the static-check layer of the engine.
//!
//! Every check here reads a board plus the models it binds to and returns
//! findings without running a simulation: [`checks`] is the netlist / boot /
//! strap / contention / converter / USB-C lint family, [`decoupling`] applies
//! capacitor parasitics, and [`dcpath`] classifies how a net is DC-defined.
//! They depend on `hauksbee-bind` for the binder and device models and on
//! nothing above it, so the check suite and the co-simulation layer build in
//! parallel and an edit to one never rebuilds the other.

pub mod checks;
pub mod dcpath;
pub mod decoupling;

pub use checks::usb_c::{
    classify_attach, classify_board, extract_sink_termination, usb_c_report, Attach, Cable,
    CcResult, CcThresholds, PinState, Rp, SinkTermination, UsbcLevel, UsbcReport,
};
pub use decoupling::{apply_parasitics, CapClass, EsrEsl};
