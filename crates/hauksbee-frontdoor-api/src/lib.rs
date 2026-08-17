//! Leaf contracts shared by the hauksbee analysis engine and web front door.
//!
//! This crate deliberately contains no server, solver, extraction, or MCU
//! implementation. It defines only the engine boundary, the live wire protocol,
//! and the callbacks/data handed from an embedding application to a front door.

pub mod engine;
pub mod frontdoor;
pub mod protocol;
