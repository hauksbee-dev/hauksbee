//! galvani-engine: the integration heart.
//!
//! This crate turns an [`ExtractedBoard`](galvani_extract::ExtractedBoard)
//! plus a [`ModelLibrary`](galvani_models::ModelLibrary) into a *live*
//! co-simulation that couples three domains:
//!
//! 1. **Analog** — the MNA transient solver in `galvani-solve`, fed a
//!    [`Circuit`](galvani_ir::Circuit) the [`binder`] builds by resolving
//!    every component to a model and stamping it as IR devices.
//! 2. **MCU** — emulated microcontroller cores from `galvani-mcu`, coupled at
//!    the pin level: GPIO output edges drive analog nets, analog node voltages
//!    are injected into ADC channels, UART passes through.
//! 3. **Digital** — behavioral ICs (shift registers, gates) handled by the
//!    [`digital`] event layer, NOT solved in MNA. Their inputs sample net
//!    voltages against `vih`/`vil`; their outputs drive nets as Thevenin
//!    sources stamped into the circuit.
//!
//! The [`scheduler::Scheduler`] steps all three in lockstep chunks
//! (generalizing the Tarski-Emulator pattern), and [`engine::GalvaniEngine`]
//! exposes the whole thing behind `galvani-server`'s `Engine` trait.

pub mod binder;
pub mod digital;
pub mod drivers;
pub mod engine;
pub mod report;
pub mod scheduler;

pub use binder::{bind_board, BoundBoard};
pub use engine::GalvaniEngine;
pub use report::{BindOutcome, BindReport, BindRow};
