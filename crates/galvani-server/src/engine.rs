//! The engine boundary. The server streams whatever an [`Engine`] produces;
//! the real circuit engine plugs in behind this trait, and a lightweight
//! demo engine exists so the UI stack can run before full integration.

use crate::protocol::{BoardInfo, PowerSupplyConfig, SimFrame, SolverControls};

pub trait Engine: Send + 'static {
    fn board_info(&self) -> BoardInfo;
    /// Advance simulation by `dt` seconds and produce a frame.
    fn step(&mut self, dt: f64) -> SimFrame;
    fn reset(&mut self);
    fn set_controls(&mut self, controls: SolverControls);
    fn controls(&self) -> SolverControls;
    /// Write bytes to an MCU's serial input.
    fn serial(&mut self, mcu: &str, data: &[u8]);
    /// Drive a bound input source.
    fn set_input(&mut self, source: &str, value: f64);
    /// Configure the power supply on a supply net (Feature 1). Default no-op
    /// for engines without configurable supplies.
    fn set_power_supply(&mut self, _net: &str, _supply: PowerSupplyConfig) {}
}

/// Demo engine: an emulated AVR running real firmware with a synthetic
/// analog environment. Proves the server/MCU/frontend stack end to end.
pub struct McuDemoEngine {
    mcu: Box<dyn galvani_mcu::Mcu + Send>,
    name: String,
    board_url: String,
    sim_time: f64,
    controls: SolverControls,
    uart_rx: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    led_state: std::sync::Arc<std::sync::Mutex<bool>>,
    adc_volts: f64,
}

impl McuDemoEngine {
    pub fn new(
        firmware_hex: &std::path::Path,
        name: &str,
        board_url: &str,
    ) -> anyhow::Result<Self> {
        use galvani_mcu::Mcu;
        let mut mcu = galvani_mcu::AvrMcu::atmega328p_16mhz()?;
        mcu.load_firmware(firmware_hex)?;
        let uart_rx: std::sync::Arc<std::sync::Mutex<Vec<u8>>> = Default::default();
        let sink = uart_rx.clone();
        mcu.on_uart(Box::new(move |b| sink.lock().unwrap().push(b)));
        let led_state: std::sync::Arc<std::sync::Mutex<bool>> = Default::default();
        let led = led_state.clone();
        mcu.on_pin_change(Box::new(move |pin, high| {
            if pin.port == 'B' && pin.bit == 5 {
                *led.lock().unwrap() = high;
            }
        }));
        Ok(McuDemoEngine {
            mcu: Box::new(mcu),
            name: name.to_string(),
            board_url: board_url.to_string(),
            sim_time: 0.0,
            controls: SolverControls::default(),
            uart_rx,
            led_state,
            adc_volts: 2.5,
        })
    }
}

impl Engine for McuDemoEngine {
    fn board_info(&self) -> BoardInfo {
        BoardInfo {
            name: self.name.clone(),
            board_url: self.board_url.clone(),
            num_components: 1,
            num_nets: 2,
            nets: vec!["D13_LED".into(), "A0".into()],
            component_kinds: [("U1".to_string(), "mcu".to_string())].into(),
            mcus: vec![("U1".into(), "simavr:atmega328p".into())],
            power_supplies: Default::default(),
        }
    }

    fn step(&mut self, dt: f64) -> SimFrame {
        let micros = (dt * 1e6) as u64;
        self.mcu.set_analog_in(0, self.adc_volts);
        let _ = self.mcu.run_micros(micros);
        self.sim_time += dt;
        let led = *self.led_state.lock().unwrap();
        let uart: Vec<u8> = std::mem::take(&mut *self.uart_rx.lock().unwrap());
        SimFrame {
            t: self.sim_time,
            realtime_factor: 1.0,
            net_voltages: [
                ("D13_LED".to_string(), if led { 5.0 } else { 0.0 }),
                ("A0".to_string(), self.adc_volts),
            ]
            .into(),
            component_states: [(
                "U1".to_string(),
                [("running".to_string(), 1.0)].into_iter().collect(),
            )]
            .into(),
            uart: if uart.is_empty() {
                Default::default()
            } else {
                [("U1".to_string(), uart)].into()
            },
            net_currents: Default::default(),
            faults: Default::default(),
            supply_states: Default::default(),
        }
    }

    fn reset(&mut self) {
        self.sim_time = 0.0;
    }

    fn set_controls(&mut self, controls: SolverControls) {
        self.controls = controls;
    }

    fn controls(&self) -> SolverControls {
        self.controls.clone()
    }

    fn serial(&mut self, _mcu: &str, data: &[u8]) {
        self.mcu.uart_write(data);
    }

    fn set_input(&mut self, source: &str, value: f64) {
        if source == "A0" {
            self.adc_volts = value;
        }
    }
}
