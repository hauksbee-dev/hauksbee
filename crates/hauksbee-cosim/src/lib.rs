//! hauksbee-cosim: the co-simulation layer of the engine.
//!
//! Given a [`BoundBoard`](hauksbee_bind::BoundBoard) from `hauksbee-bind`,
//! this crate runs it as a *live* co-simulation that couples three domains:
//!
//! 1. **Analog**; the MNA transient solver in `hauksbee-solve`, fed the
//!    [`Circuit`](hauksbee_ir::Circuit) the binder built.
//! 2. **MCU**, emulated microcontroller cores from `hauksbee-mcu`, coupled at
//!    the pin level: GPIO output edges drive analog nets, analog node voltages
//!    are injected into ADC channels, UART passes through.
//! 3. **Digital**, behavioral ICs (shift registers, gates) handled by the
//!    `digital` event layer, NOT solved in MNA.
//!
//! The [`scheduler::Scheduler`] steps all three in lockstep chunks
//! (generalizing the Tarski-Emulator pattern); [`peripherals`] and
//! [`responders`] are the sensors, actuators and bit-banged bus slaves that
//! hang off the MCU pins; and [`engine::HauksbeeEngine`] exposes the whole
//! thing behind `hauksbee-frontdoor-api`'s `Engine` trait.
//!
//! The `avr`, `renode` and `qemu` features forward to `hauksbee-mcu` exactly
//! as `hauksbee-engine` does; nothing in this crate links an emulator itself.

pub mod engine;
pub mod error_budget;
pub mod peripherals;
pub mod responders;
pub mod scheduler;

pub use engine::HauksbeeEngine;
pub use peripherals::{
    controls::{Encoder, Potentiometer, Pushbutton, Stimulus, StimulusKind, ToggleSwitch},
    i2c::{Eeprom24c, I2cBus, I2cSlave, Lm75},
    load::DynamicLoad,
    sink::VcdSink,
    spi::{
        CsProvenance, Mcp3008, ResolvedCs, Spi25Eeprom, SpiBus, SpiFramingMode, SpiNorFlash,
        SpiSlave,
    },
    Peripheral, PeripheralSet, RegisterMapSensor, TickCtx, TimelineEvent,
};
pub use responders::{
    BitBangSpiPins, BitBangSpiResponder, InputResponder, ResponderRegistry, SoftI2cResponder,
};

// The device layer's names, so `crate::FaultEvent`-style paths inside this
// crate resolve the same way they did when it was one crate with the binder.
pub use hauksbee_bind::{
    bind_board, bind_board_with, BehavioralDevice, BoundBoard, CustomBehavior, CustomRegistry,
    FaultEvent, FaultKind, PowerSupply, StressMonitor, SupplyLeg,
};
