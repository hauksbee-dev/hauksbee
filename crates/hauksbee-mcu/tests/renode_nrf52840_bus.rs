//! Live-Renode verification for the nRF52840 bus controllers (U3 finding 2).
//!
//! `nrf52840.soc.toml` used to ship `controllers = []` for both buses, so a
//! bound I2C/SPI sensor was silently never exercised (`install_i2c_bridge`
//! returns Ok on an empty list; `on_spi` returns silently). The descriptor now
//! names the controllers the stock Renode 1.16.1 `nrf52840.repl` actually
//! models, `twi0`/`twi1` (I2C.NRF52840_I2C) and `spi2` (NRF52840_SPI), and
//! this test proves, against the LIVE Renode install, that:
//!
//!   1. the descriptor's controller names exist on the booted platform (a
//!      typo'd name would make the bridge registration fail), and
//!   2. the Hauksbee bridge peripherals actually REGISTER on them: the C# I2C
//!      bridge is loaded and attached at a slave address on every configured
//!      TWI controller, and the SPI bridge attaches on `spi2`. Registration is
//!      the step Renode validates the container type and name on, so passing
//!      it is the "this controller can host engine slaves" proof; the same
//!      bar the shipped STM32F103 controllers were held to when their bridge
//!      first landed.
//!
//! Honest scope: this verifies controller existence + bridge registration. An
//! end-to-end firmware round-trip (an nRF ELF reading a bound sensor through
//! its own TWI driver) needs an nRF bus firmware fixture the repo does not yet
//! carry; Renode models the pre-EasyDMA TWI/SPI interfaces here, so
//! TWIM/SPIM-only firmware would still miss the model. The coverage machinery
//! (unexercised-bus warnings) stays live for anything beyond these
//! controllers.
//!
//! Skips gracefully when Renode is not installed, like every renode_* test.

#![cfg(feature = "renode")]

use hauksbee_mcu::renode::is_available;
use hauksbee_mcu::{Mcu, RenodeBackend, RenodeConfig};

#[test]
fn nrf52840_descriptor_names_real_controllers_and_bridges_register() {
    if !is_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }

    let config = RenodeConfig::nrf52840();
    assert_eq!(
        config.i2c_controllers,
        vec!["twi0".to_string(), "twi1".to_string()],
        "descriptor must carry the live platform's TWI controllers"
    );
    assert_eq!(
        config.spi_controllers,
        vec!["spi2".to_string()],
        "descriptor must carry the live platform's SPI controller"
    );

    let mut mcu = RenodeBackend::new(config).expect("spawn Renode nRF52840");

    // Coverage hooks: with real controllers configured, the backend must
    // report the buses as modeled (the unexercised-bus warning must NOT fire
    // for this platform any more).
    assert!(mcu.i2c_bus_modeled(), "nRF52840 now models I2C (twi0/twi1)");
    assert!(mcu.spi_bus_modeled(None), "nRF52840 now models SPI (spi2)");
    assert!(mcu.spi_bus_modeled(Some("spi2")));

    // I2C: registering the bridge at 0x48 on BOTH twi0 and twi1 exercises
    // Renode's name lookup and container-type check for each controller.
    // `on_i2c` panics loudly on any registration failure.
    mcu.set_i2c_slave_addresses(&[0x48]);
    mcu.on_i2c(Box::new(|_ev| None));

    // SPI: registering the bridge on spi2 (NullRegistrationPoint attach).
    // `on_spi` routes to the first configured controller and panics on
    // failure; the named-controller path must accept "spi2" too.
    mcu.on_spi(Box::new(|_ev| 0xFF));
    mcu.on_spi_controller("spi2", Box::new(|_ev| 0xFF));

    // And the drop path still tells the truth: no ADC map is shipped for this
    // platform (the live repl models no ADC/SAADC), so an injection must be
    // recorded as dropped, not silently swallowed.
    assert!(mcu.adc_dropped_channels().is_empty());
    mcu.set_analog_in(0, 1.5);
    assert_eq!(
        mcu.adc_dropped_channels(),
        vec![0u8],
        "an unmapped ADC injection must be recorded as dropped"
    );
}
