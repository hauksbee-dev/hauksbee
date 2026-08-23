//! The server's lightweight demo engine.
//!
//! The transport-independent [`Engine`] boundary itself lives in
//! `hauksbee-frontdoor-api` (the leaf crate both this server and the compute
//! engine depend on, so neither has to depend on the other) and is
//! re-exported here so `hauksbee_server::engine::Engine` keeps resolving for
//! existing callers. What remains locally is the demo implementation: an
//! emulated AVR proving the server/MCU/frontend stack without a real board.

#[cfg(feature = "avr")]
use crate::protocol::{BoardInfo, SimFrame, SolverControls};
pub use hauksbee_frontdoor_api::engine::Engine;

/// Demo engine: an emulated AVR running real firmware with a synthetic
/// analog environment. Proves the server/MCU/frontend stack end to end.
///
/// Feature-gated on `avr`: this is the only place the server crate touches the
/// simavr-backed [`hauksbee_mcu::AvrMcu`]. Keeping it behind the gate is what
/// lets the GPL-free release shape (`--no-default-features --features
/// renode,qemu`) build the server crate without linking GPL-3.0 libsimavr.
#[cfg(feature = "avr")]
pub struct McuDemoEngine {
    mcu: Box<dyn hauksbee_mcu::Mcu + Send>,
    name: String,
    board_url: String,
    /// Kept so `reset()` can reboot from a fresh firmware load, not just zero the
    /// clock, which would leak the MCU core, UART buffer and LED state across it.
    firmware_path: std::path::PathBuf,
    sim_time: f64,
    controls: SolverControls,
    uart_rx: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    led_state: std::sync::Arc<std::sync::Mutex<bool>>,
    adc_volts: f64,
}

#[cfg(feature = "avr")]
impl McuDemoEngine {
    pub fn new(
        firmware_hex: &std::path::Path,
        name: &str,
        board_url: &str,
    ) -> anyhow::Result<Self> {
        let uart_rx: std::sync::Arc<std::sync::Mutex<Vec<u8>>> = Default::default();
        let led_state: std::sync::Arc<std::sync::Mutex<bool>> = Default::default();
        let mcu = Self::build_mcu(firmware_hex, &uart_rx, &led_state)?;
        Ok(McuDemoEngine {
            mcu,
            name: name.to_string(),
            board_url: board_url.to_string(),
            firmware_path: firmware_hex.to_path_buf(),
            sim_time: 0.0,
            controls: SolverControls::default(),
            uart_rx,
            led_state,
            adc_volts: 2.5,
        })
    }

    /// Build a fresh AvrMcu with the demo callbacks wired to the given shared
    /// UART/LED state. Shared by `new` and `reset` so a reboot re-registers the
    /// same callbacks against the same Arcs.
    fn build_mcu(
        firmware_hex: &std::path::Path,
        uart_rx: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        led_state: &std::sync::Arc<std::sync::Mutex<bool>>,
    ) -> anyhow::Result<Box<dyn hauksbee_mcu::Mcu + Send>> {
        use hauksbee_mcu::Mcu;
        // Guard the firmware path before simavr sees it: a missing file segfaults
        // the native loader (exit 139) instead of erroring.
        hauksbee_mcu::validate_firmware_path(firmware_hex)?;
        let mut mcu = hauksbee_mcu::AvrMcu::atmega328p_16mhz()?;
        mcu.load_firmware(firmware_hex)?;
        let sink = uart_rx.clone();
        mcu.on_uart(Box::new(move |b| sink.lock().unwrap().push(b)));
        let led = led_state.clone();
        mcu.on_pin_change(Box::new(move |pin, high, _cycle| {
            if pin.port == 'B' && pin.bit == 5 {
                *led.lock().unwrap() = high;
            }
        }));
        Ok(Box::new(mcu))
    }
}

#[cfg(feature = "avr")]
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
            peripherals: Default::default(),
            input_sources: vec![crate::protocol::InputSourceInfo {
                id: "A0".into(),
                kind: "voltage".into(),
                min: 0.0,
                max: 5.0,
                initial: 2.5,
                unit: "V".into(),
            }],
            shorts: None,
        }
    }

    fn step(&mut self, dt: f64) -> SimFrame {
        let micros = (dt * 1e6) as u64;
        // Measure what this step actually cost so the reported factor is the
        // delivered sim-per-wall ratio, never an asserted 1.0. The sim loop
        // overwrites it with a rolling-window measurement for the stream;
        // this per-step value keeps direct embedders honest too.
        let step_started = std::time::Instant::now();
        self.mcu.set_analog_in(0, self.adc_volts);
        let _ = self.mcu.run_micros(micros);
        let step_wall = step_started.elapsed().as_secs_f64();
        self.sim_time += dt;
        let led = *self.led_state.lock().unwrap();
        let uart: Vec<u8> = std::mem::take(&mut *self.uart_rx.lock().unwrap());
        SimFrame {
            t: self.sim_time,
            realtime_factor: if step_wall > 0.0 { dt / step_wall } else { 0.0 },
            requested_factor: 0.0,
            rate_limited: false,
            unobserved_drive_nets: Vec::new(),
            net_voltages: [
                ("D13_LED".to_string(), if led { 5.0 } else { 0.0 }),
                ("A0".to_string(), self.adc_volts),
            ]
            .into(),
            // The demo engine's synthetic nets hold their level for the whole
            // step, so there is no envelope to report. Empty is the honest
            // answer here, not a placeholder.
            net_v_extremes: Default::default(),
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
        // A real reboot, not just a clock rewind: rebuild the MCU core from a
        // fresh firmware load and clear the buffered UART / LED state, so the
        // post-reset frames reflect a fresh boot instead of leaking mid-stream
        // execution across the reset.
        if let Ok(mcu) = Self::build_mcu(&self.firmware_path, &self.uart_rx, &self.led_state) {
            self.mcu = mcu;
        }
        self.uart_rx.lock().unwrap().clear();
        *self.led_state.lock().unwrap() = false;
        self.adc_volts = 2.5;
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
