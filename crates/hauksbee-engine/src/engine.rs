//! [`HauksbeeEngine`]: the concrete engine behind `hauksbee-server`'s [`Engine`]
//! trait, the object that turns a bound board plus optional firmware into a
//! running co-simulation the server (and its many front-ends) can drive.
//!
//! It wraps a [`Scheduler`] over a [`BoundBoard`] and owns the board metadata
//! the wire protocol reports (net names, component kinds, MCU backends, the bind
//! report). Its main job at the boundary is translation: the protocol's
//! human-facing [`SolverControls`] are mapped onto the solver's
//! [`SolverOptions`]/[`Integration`]/[`StepControl`], and each protocol request
//! is serviced by stepping the scheduler and emitting a [`SimFrame`]. This is
//! where the "one engine, many front-ends" story is made real -- the server, the
//! TUI, and CI all talk to this one type rather than the solver directly.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use hauksbee_extract::ExtractedBoard;
use hauksbee_models::{Bus, ModelLibrary};
use hauksbee_server::engine::Engine;
use hauksbee_server::protocol::{
    BoardInfo, ChemistryConfig, FaultInfo, LiveRegisterMapSpec, PeripheralInfo, PowerSupplyConfig,
    ShortsDisclosure, SimFrame, SolverControls, SupplyState, UsbSpecConfig,
};
use hauksbee_solve::{Integration, SolverOptions, StepControl};

use crate::binder::{bind_board, BoundBoard};
use crate::peripherals::{CsProvenance, I2cBus, RegisterMapSensor, ResolvedCs, SpiBus};
use crate::power_supply::{Chemistry, PowerSupply, UsbSpec};
use crate::report::BindReport;
use crate::scheduler::Scheduler;

/// Analog chunk for boards whose MCU runs on an external emulator (Renode or
/// QEMU). See the note in [`HauksbeeEngine::from_bound`] for why the
/// scheduler's much finer default is wrong for those backends.
const EXTERNAL_BACKEND_CHUNK_S: f64 = 5e-3;

/// The live engine: a co-sim scheduler plus board metadata for the protocol.
pub struct HauksbeeEngine {
    sched: Scheduler,
    board_name: String,
    board_url: String,
    net_names: Vec<String>,
    component_kinds: HashMap<String, String>,
    mcu_backends: Vec<(String, String)>,
    controls: SolverControls,
    report_store: BindReport,
    /// What happened to the DRC's detected copper shorts on THIS engine, for
    /// the wire `BoardInfo` so the live-sim UI can disclose it (the report's
    /// co-sim block already does; the live surface must match). None when no
    /// shorts were detected (or the launch path ran no DRC).
    shorts: Option<ShortsDisclosure>,
}

impl HauksbeeEngine {
    /// Build an engine from a bound board and optional firmware.
    pub fn from_bound(
        bound: BoundBoard,
        firmware: Option<&Path>,
        board_url: &str,
    ) -> anyhow::Result<Self> {
        let board_name = bound.name.clone();
        let net_names = bound.net_names.clone();
        let component_kinds = bound.component_kinds.clone();
        let mcu_backends = bound
            .mcus
            .iter()
            .map(|m| (m.reference.clone(), m.backend.clone()))
            .collect();
        let report_store = bound.report.clone();
        let controls = SolverControls::default();
        let opts = controls_to_options(&controls);
        let mut sched = Scheduler::new(bound, firmware, opts)?;
        // Coarsen the analog chunk for external emulators, the way every
        // deliberate caller already does (the CLI co-sim report, the CI runner,
        // the proven QEMU integration tests). Renode and QEMU advance the guest
        // over a control socket, and one round-trip costs ~25 ms of wall time
        // whatever slice of guest time it buys, so the scheduler's 100 us
        // default (right for the in-process AVR core) spends 300 wall seconds
        // per simulated second on round-trips alone. The live sim was the one
        // caller that never coarsened it and inherited the default: measured at
        // 0.0008x realtime, an ESP32 needed over an hour of wall clock to reach
        // app_main, so "Drive it live" looked hung. 5 ms matches the value the
        // integration tests proved and sits inside the CI runner's 1..10 ms
        // clamp. A caller that knows better still overrides it afterwards.
        if sched.has_external_backend() {
            sched.chunk_s = EXTERNAL_BACKEND_CHUNK_S;
        }
        Ok(HauksbeeEngine {
            sched,
            board_name,
            board_url: board_url.to_string(),
            net_names,
            component_kinds,
            mcu_backends,
            controls,
            report_store,
            shorts: None,
        })
    }

    /// Convenience: extract + bind + build in one call.
    pub fn from_board_file(
        board_text: &str,
        firmware: Option<&Path>,
        board_url: &str,
    ) -> anyhow::Result<Self> {
        let board = ExtractedBoard::from_auto(board_text)?;
        let lib = ModelLibrary::builtin();
        let bound = bind_board(&board, &lib);
        HauksbeeEngine::from_bound(bound, firmware, board_url)
    }

    /// The bind report for this engine's board.
    pub fn report(&self) -> &BindReport {
        &self.report_store
    }

    /// Direct access to the scheduler (headless stats, tests).
    pub fn scheduler(&self) -> &Scheduler {
        &self.sched
    }
    /// Arm the LIVE-session behaviour: once the strict-abort streak trips,
    /// a multi-chunk step returns early instead of grinding the rest of the
    /// frame's chunks; the session is about to end on the failure either
    /// way, and the sooner the step returns the sooner the session can say
    /// so and take Reset. Only the serving surfaces call this: headless,
    /// CI and report co-sims keep the complete march, because their
    /// failed-window record over the WHOLE requested span is the product.
    pub fn arm_live_abort(&mut self) {
        self.sched.stop_when_dead = true;
    }

    pub fn scheduler_mut(&mut self) -> &mut Scheduler {
        &mut self.sched
    }

    /// Force an output SPIKE net's voltage after each analog solve (until
    /// cleared); the hook the firmware-driven Tarski inference uses to drive the
    /// 10 SPIKE nets from the EXACT feedforward decomposition (the monolith does
    /// not converge). Returns false if the net does not exist.
    pub fn force_net_voltage(&mut self, net: &str, volts: f64) -> bool {
        self.sched.force_net_voltage(net, volts)
    }

    /// Force a net HIGH while `t_start <= sim_time < t_end`, else LOW, used to
    /// rate-code an output SPIKE net (HIGH for a sim-time fraction proportional
    /// to its decomposed spike count). Returns false if the net is absent.
    pub fn force_net_voltage_windowed(
        &mut self,
        net: &str,
        high_volts: f64,
        low_volts: f64,
        t_start: f64,
        t_end: f64,
    ) -> bool {
        self.sched
            .force_net_voltage_windowed(net, high_volts, low_volts, t_start, t_end)
    }

    /// Current sim time (s).
    pub fn sim_time(&self) -> f64 {
        self.sched.sim_time()
    }

    /// Clear all forced net-voltage overrides.
    pub fn clear_forced_voltages(&mut self) {
        self.sched.clear_forced_voltages();
    }

    /// What-if: short two nets together (the solder-bridge scenario). Bridges
    /// them with a small resistance and raises a `short` fault. Returns whether
    /// the bridge was applied (both nets exist and were not already bridged).
    pub fn short_nets(&mut self, net_a: &str, net_b: &str) -> bool {
        self.sched.short_nets(net_a, net_b)
    }

    /// Apply every copper short a geometric DRC report detected, bridging each
    /// shorted net pair so the simulation shows the consequences. Returns the
    /// number of bridges stamped.
    pub fn apply_drc_shorts(&mut self, report: &hauksbee_extract::DrcReport) -> usize {
        self.sched.apply_drc_shorts(report)
    }

    /// Apply the measured copper topology, but emit a short fault only for
    /// contacts that lack board-local physical authorization. A companion
    /// schematic may add context without authorizing a location.
    pub fn apply_drc_shorts_with_qualification(
        &mut self,
        report: &hauksbee_extract::DrcReport,
        qualification: Option<&hauksbee_extract::DrcTieQualification>,
    ) -> usize {
        self.sched
            .apply_drc_shorts_with_qualification(report, qualification)
    }

    /// Record an already-computed shorts outcome for the wire `BoardInfo`
    /// (used when the caller bridged the shorts itself, e.g. `--apply-shorts`,
    /// and only the disclosure is left to do).
    pub fn set_shorts_disclosure(&mut self, disclosure: ShortsDisclosure) {
        self.shorts = Some(disclosure);
    }

    /// Run the geometric DRC verdict this engine was launched with through the
    /// same bridge-or-refuse policy the web co-sim uses, and RECORD the outcome
    /// for the wire `BoardInfo`: validated shorts are bridged into the circuit
    /// (so the live rails reflect the board as built); an unvalidated layout
    /// version (`version_warning`) leaves them un-bridged, with the reason
    /// disclosed instead of silently simulating the idealised board.
    pub fn apply_and_disclose_drc_shorts(&mut self, report: &hauksbee_extract::DrcReport) {
        self.apply_and_disclose_drc_shorts_with_qualification(report, None);
    }

    pub fn apply_and_disclose_drc_shorts_with_qualification(
        &mut self,
        report: &hauksbee_extract::DrcReport,
        qualification: Option<&hauksbee_extract::DrcTieQualification>,
    ) {
        let detected = report
            .shorts()
            .filter(|finding| {
                qualification.is_none_or(|qualified| qualified.tie_for(finding).is_none())
            })
            .count();
        if detected == 0 {
            self.shorts = None;
            return;
        }
        let bridged = if report.version_warning.is_none() {
            self.apply_drc_shorts_with_qualification(report, qualification)
        } else {
            0
        };
        self.shorts = Some(ShortsDisclosure {
            detected,
            bridged,
            unapplied_reason: if bridged == 0 {
                Some(report.version_warning.clone().unwrap_or_else(|| {
                    "the shorted nets could not be bridged into the live circuit".to_string()
                }))
            } else {
                None
            },
        });
    }

    /// Convenience: extract + bind + build, then run geometric DRC on the same
    /// board text and apply every detected short before simulating. This is the
    /// "detect shorts from geometry, then simulate what the board does with them
    /// present" path. Returns the engine and the DRC report.
    pub fn from_board_file_with_drc_shorts(
        board_text: &str,
        firmware: Option<&Path>,
        board_url: &str,
    ) -> anyhow::Result<(Self, hauksbee_extract::DrcReport)> {
        let mut engine = Self::from_board_file(board_text, firmware, board_url)?;
        let report = ExtractedBoard::drc(board_text)?;
        engine.apply_drc_shorts(&report);
        Ok((engine, report))
    }
}

impl Engine for HauksbeeEngine {
    fn board_info(&self) -> BoardInfo {
        BoardInfo {
            name: self.board_name.clone(),
            board_url: self.board_url.clone(),
            num_components: self.component_kinds.len(),
            num_nets: self.net_names.len(),
            nets: self.net_names.clone(),
            component_kinds: self.component_kinds.clone(),
            mcus: self.mcu_backends.clone(),
            power_supplies: self
                .sched
                .supplies
                .iter()
                .map(|s| (s.net_name.clone(), supply_to_config(&s.supply)))
                .collect(),
            peripherals: self
                .sched
                .peripheral_infos()
                .into_iter()
                .map(|(id, kind)| PeripheralInfo { id, kind })
                .collect(),
            // Live controls travel through typed peripherals. The engine does
            // not expose arbitrary internal V/I sources until a model declares
            // a safe range and purpose for them.
            input_sources: Vec::new(),
            shorts: self.shorts.clone(),
        }
    }

    fn step(&mut self, dt: f64) -> SimFrame {
        // Measure the step's real cost so the frame reports the DELIVERED
        // sim-per-wall ratio. The server's sim loop overwrites this with a
        // rolling-window measurement for the stream; the per-step value keeps
        // headless and embedded callers honest with zero extra plumbing.
        let step_started = std::time::Instant::now();
        let result = self.sched.step(dt);
        let step_wall = step_started.elapsed().as_secs_f64();
        let mut component_states = self.sched.mcu_states();
        component_states.extend(self.sched.digital_states());
        component_states.extend(self.sched.peripheral_states());
        // Fold per-component stress (0..1) into the component-state maps so the
        // UI can heat-map parts approaching their ratings.
        for (reference, stress) in self.sched.stress_states() {
            component_states
                .entry(reference)
                .or_default()
                .insert("stress".to_string(), stress);
        }
        let faults = self
            .sched
            .drain_faults()
            .into_iter()
            .map(|f| FaultInfo {
                component: f.component,
                kind: f.kind.as_str().to_string(),
                value: f.value,
                limit: f.limit,
                t: f.t,
                destroyed: f.destroyed,
            })
            .collect();
        let supply_states = self
            .sched
            .supply_states()
            .into_iter()
            .map(|(net, (kind, current_a, soc))| {
                (
                    net,
                    SupplyState {
                        kind,
                        current_a,
                        soc,
                    },
                )
            })
            .collect();
        SimFrame {
            t: result.sim_time,
            realtime_factor: if step_wall > 0.0 { dt / step_wall } else { 0.0 },
            requested_factor: 0.0,
            rate_limited: false,
            // Scope honesty: nets whose MCU drive this backend cannot observe
            // (levels-only backends, tri-stated driver) are flagged so the UI
            // can say "static level, not a measured drive" instead of
            // presenting the passive network's idle voltage as a measurement.
            unobserved_drive_nets: self.sched.unobserved_drive_nets(),
            net_voltages: self.sched.net_voltages(),
            // The chunk's envelope, not just the instant above. Without it a
            // strobe narrower than the chunk is gone before the frame is
            // serialised, and no client can tell a flat net from a fast one.
            net_v_extremes: self.sched.frame_v_extremes().clone(),
            component_states,
            uart: result.uart,
            net_currents: Default::default(),
            faults,
            supply_states,
        }
    }

    fn reset(&mut self) {
        // Restart the sim clock AND drop every run-accumulated diagnostic (failed
        // chunks/windows, the consecutive-failure streak, the sub-µs clock carry,
        // net stats, per-frame peaks). Zeroing only `sim_time` here once let the
        // previous run's failure surface and clock carry bleed into the next run.
        self.sched.reset_run_state();
    }

    fn set_controls(&mut self, controls: SolverControls) {
        self.controls = controls.clone();
        self.sched.opts = controls_to_options(&controls);
        // The stress monitor's junction-temperature estimate sits on its own
        // ambient, not the solver's temperature: without this, the UI slider
        // changes device physics but never the over-temperature checks.
        self.sched.set_ambient_c(controls.temperature_c);
        self.sched
            .set_destructive_faults(controls.destructive_faults);
        if controls.fixed_dt > 0.0 {
            self.sched.chunk_s = controls.fixed_dt;
        }
    }

    fn controls(&self) -> SolverControls {
        self.controls.clone()
    }

    fn serial(&mut self, mcu: &str, data: &[u8]) {
        self.sched.serial(mcu, data);
    }

    fn set_input(&mut self, source: &str, value: f64) {
        self.sched.set_input(source, value);
    }

    fn set_power_supply(&mut self, net: &str, supply: PowerSupplyConfig) {
        self.sched.set_power_supply(net, config_to_supply(supply));
    }

    fn set_peripheral(&mut self, id: &str, value: f64) -> bool {
        self.sched.set_peripheral(id, value)
    }

    fn attach_peripheral(
        &mut self,
        spec: hauksbee_server::protocol::LivePeripheralSpec,
    ) -> Result<(), String> {
        use crate::peripherals::controls::{Pushbutton, Stimulus, StimulusKind, ToggleSwitch};
        use hauksbee_ir::{NodeId, SourceKind};

        if spec.id.trim().is_empty() || spec.id.len() > 96 {
            return Err("live peripheral id must be 1..=96 characters".into());
        }
        if self
            .sched
            .peripheral_infos()
            .iter()
            .any(|(id, _)| id == &spec.id)
        {
            return Err(format!(
                "a live peripheral named '{}' already exists",
                spec.id
            ));
        }
        let net = self
            .sched
            .net_nodes
            .get(&spec.net)
            .copied()
            .ok_or_else(|| format!("board net '{}' does not exist", spec.net))?;
        let to = match spec.to.as_deref() {
            None | Some("") | Some("GND") | Some("gnd") | Some("0") => NodeId::GROUND,
            Some(name) => self
                .sched
                .net_nodes
                .get(name)
                .copied()
                .ok_or_else(|| format!("board net '{name}' does not exist"))?,
        };
        match spec.kind.as_str() {
            "stimulus" => {
                let offset = spec.offset.unwrap_or(0.0);
                if !offset.is_finite() {
                    return Err("stimulus offset must be finite".into());
                }
                let stimulus = Stimulus::voltage(
                    self.sched.circuit_mut(),
                    &spec.id,
                    net,
                    StimulusKind::Wave(SourceKind::Dc(offset)),
                );
                self.sched.attach_peripheral(Box::new(stimulus));
            }
            "pushbutton" => {
                let bounce_ms = spec.bounce_ms.unwrap_or(5.0);
                if !bounce_ms.is_finite() || bounce_ms < 0.0 {
                    return Err("pushbutton bounce_ms must be finite and non-negative".into());
                }
                let button =
                    Pushbutton::new(self.sched.circuit_mut(), &spec.id, net, to, bounce_ms);
                self.sched.attach_peripheral(Box::new(button));
                if spec.initial.unwrap_or(0.0) >= 0.5 {
                    self.sched.set_peripheral(&spec.id, 1.0);
                }
            }
            "toggle" => {
                let closed = spec.initial.unwrap_or(0.0) >= 0.5;
                let toggle = ToggleSwitch::new(self.sched.circuit_mut(), &spec.id, net, to, closed);
                self.sched.attach_peripheral(Box::new(toggle));
            }
            other => {
                return Err(format!(
                    "live peripheral type '{other}' is not supported (expected stimulus|pushbutton|toggle)"
                ));
            }
        }
        Ok(())
    }

    fn attach_register_map(&mut self, spec: LiveRegisterMapSpec) -> Result<(), String> {
        if spec.id.trim().is_empty() || spec.id.len() > 96 {
            return Err("live register-map id must be 1..=96 characters".into());
        }
        if spec.spec_toml.is_empty() || spec.spec_toml.len() > 1_048_576 {
            return Err("live register-map spec must be 1..=1048576 bytes".into());
        }
        if spec.inputs.len() > 256 {
            return Err("live register-map spec has more than 256 input overrides".into());
        }
        if spec
            .controller
            .as_ref()
            .is_some_and(|name| name.trim().is_empty() || name.len() > 96)
        {
            return Err("live register-map controller must be 1..=96 characters".into());
        }
        if self
            .sched
            .peripheral_infos()
            .iter()
            .any(|(id, _)| id == &spec.id)
        {
            return Err(format!(
                "a live peripheral named '{}' already exists",
                spec.id
            ));
        }

        let mut sensor = RegisterMapSensor::from_toml(&spec.spec_toml)
            .map_err(|error| format!("register-map spec refused: {error}"))?;
        for (name, value) in &spec.inputs {
            if !value.is_finite() {
                return Err(format!("register-map input '{name}' must be finite"));
            }
            if sensor.input(name).is_none() {
                return Err(format!(
                    "register-map input '{name}' is not declared by the exact spec"
                ));
            }
            sensor.set_input(name, *value);
        }

        match sensor.bus() {
            Bus::I2c => {
                if spec.controller.is_some() || spec.cs_net.is_some() {
                    return Err(
                        "an I2C register-map device must not set controller or cs_net".into(),
                    );
                }
                let bus = Arc::new(Mutex::new(
                    I2cBus::new(&spec.id).with_slave(Box::new(sensor)),
                ));
                self.sched.attach_i2c_bus(bus);
            }
            Bus::Spi => {
                let resolved_cs = if let Some(net_name) = spec.cs_net.as_deref() {
                    let net = self
                        .sched
                        .net_nodes
                        .get(net_name)
                        .copied()
                        .ok_or_else(|| format!("board net '{net_name}' does not exist"))?;
                    let pin = self.sched.pin_driving_node(net).ok_or_else(|| {
                        format!(
                            "SPI chip-select net '{net_name}' is not driven by a modeled MCU pin"
                        )
                    })?;
                    Some(ResolvedCs {
                        pin,
                        net: Some(net),
                        provenance: CsProvenance::SpecDeclared,
                    })
                } else {
                    None
                };
                let bus = Arc::new(Mutex::new(SpiBus::new(&spec.id, Box::new(sensor))));
                if let Some(controller) = spec.controller.as_deref() {
                    self.sched.attach_spi_bus_on(controller, bus, resolved_cs);
                } else {
                    self.sched.attach_spi_bus(bus, resolved_cs);
                }
            }
        }
        Ok(())
    }

    /// The strict-abort streak, surfaced to the live server: once
    /// [`crate::scheduler::STRICT_CONSECUTIVE_FAILED_ABORT`] chunks in a row
    /// have failed every rescue rung, no further stepping will produce a real
    /// answer, and the sim loop must end the session with this reason instead
    /// of grinding the dead solve forever. Same threshold the headless
    /// `--strict` / CI abort uses: one rule for "this run is unrecoverable".
    /// A `reset()` clears the streak (see `reset_run_state`), so a relaunch
    /// or reset starts clean.
    fn analog_failure(&self) -> Option<String> {
        if !self.sched.analog_abort_tripped() {
            return None;
        }
        // The most recent failed-window reason is the current story (it
        // carries an MCU-refused-to-advance failure too, which trips the
        // same streak); `last_solve_error` only remembers the latest ANALOG
        // refusal and can be stale across an MCU death.
        let reason = self
            .sched
            .failed_window_reasons()
            .last()
            .map(String::as_str)
            .or_else(|| self.sched.last_solve_error())
            .unwrap_or("the march did not advance");
        Some(format!(
            "the co-simulation failed {} chunks in a row and cannot recover: {reason}",
            crate::scheduler::STRICT_CONSECUTIVE_FAILED_ABORT
        ))
    }

    /// One analog chunk, for a board on an external emulator: a step smaller
    /// than that still pays a full control-socket round-trip and simply buys
    /// less guest time for it. In-process cores (the AVR backend) report no
    /// floor, so their pacing is unchanged.
    fn min_step_dt(&self) -> f64 {
        if self.sched.has_external_backend() {
            self.sched.chunk_s
        } else {
            0.0
        }
    }
}

/// Map a wire [`PowerSupplyConfig`] onto the engine's behavioral [`PowerSupply`].
pub fn config_to_supply(c: PowerSupplyConfig) -> PowerSupply {
    match c {
        PowerSupplyConfig::Ideal { volts } => PowerSupply::Ideal { volts },
        PowerSupplyConfig::Bench {
            volts,
            current_limit_a,
        } => PowerSupply::Bench {
            volts,
            current_limit_a,
        },
        PowerSupplyConfig::Wall {
            volts,
            r_out_ohms,
            ripple_vpp,
            ripple_hz,
        } => PowerSupply::Wall {
            volts,
            r_out_ohms,
            ripple_vpp,
            ripple_hz,
        },
        PowerSupplyConfig::Usb { spec } => PowerSupply::Usb {
            spec: match spec {
                UsbSpecConfig::V5_0_5a => UsbSpec::V5_0_5A,
                UsbSpecConfig::V5_1_5a => UsbSpec::V5_1_5A,
                UsbSpecConfig::V5_3a => UsbSpec::V5_3A,
            },
        },
        PowerSupplyConfig::Battery {
            chemistry,
            cells,
            capacity_mah,
            soc,
            r_internal_ohms,
        } => PowerSupply::Battery {
            chemistry: match chemistry {
                ChemistryConfig::LiIon => Chemistry::LiIon,
                ChemistryConfig::Alkaline => Chemistry::Alkaline,
                ChemistryConfig::NiMh => Chemistry::NiMh,
                ChemistryConfig::LiFePo4 => Chemistry::LiFePO4,
            },
            cells,
            capacity_mah,
            soc,
            r_internal_ohms,
            // The wire protocol does not (yet) carry BMS protection settings;
            // protection is configured by the scenario layer (hauksbee-ci).
            protection: None,
        },
    }
}

/// Map the engine's [`PowerSupply`] back to a wire [`PowerSupplyConfig`].
pub fn supply_to_config(s: &PowerSupply) -> PowerSupplyConfig {
    match s {
        PowerSupply::Ideal { volts } => PowerSupplyConfig::Ideal { volts: *volts },
        PowerSupply::Bench {
            volts,
            current_limit_a,
        } => PowerSupplyConfig::Bench {
            volts: *volts,
            current_limit_a: *current_limit_a,
        },
        PowerSupply::Wall {
            volts,
            r_out_ohms,
            ripple_vpp,
            ripple_hz,
        } => PowerSupplyConfig::Wall {
            volts: *volts,
            r_out_ohms: *r_out_ohms,
            ripple_vpp: *ripple_vpp,
            ripple_hz: *ripple_hz,
        },
        PowerSupply::Usb { spec } => PowerSupplyConfig::Usb {
            spec: match spec {
                UsbSpec::V5_0_5A => UsbSpecConfig::V5_0_5a,
                UsbSpec::V5_1_5A => UsbSpecConfig::V5_1_5a,
                UsbSpec::V5_3A => UsbSpecConfig::V5_3a,
            },
        },
        PowerSupply::Battery {
            chemistry,
            cells,
            capacity_mah,
            soc,
            r_internal_ohms,
            ..
        } => PowerSupplyConfig::Battery {
            chemistry: match chemistry {
                Chemistry::LiIon => ChemistryConfig::LiIon,
                Chemistry::Alkaline => ChemistryConfig::Alkaline,
                Chemistry::NiMh => ChemistryConfig::NiMh,
                Chemistry::LiFePO4 => ChemistryConfig::LiFePo4,
            },
            cells: *cells,
            capacity_mah: *capacity_mah,
            soc: *soc,
            r_internal_ohms: *r_internal_ohms,
        },
    }
}

/// Map the UI's [`SolverControls`] onto solver [`SolverOptions`].
pub fn controls_to_options(c: &SolverControls) -> SolverOptions {
    let integration = match c.integration.as_str() {
        "gear2" => Integration::Gear2,
        "be" | "backward_euler" => Integration::BackwardEuler,
        _ => Integration::Trapezoidal,
    };
    let step = if c.fixed_dt > 0.0 {
        StepControl::Fixed { dt: c.fixed_dt }
    } else {
        // Adaptive within the chunk; seed from a sensible default.
        StepControl::Adaptive {
            dt_initial: 1e-7,
            dt_min: 1e-12,
            dt_max: 1e-4,
        }
    };
    // granularity 1.0 = full physics / tight tolerances; lower loosens them.
    let g = c.granularity.clamp(0.01, 1.0);
    let scale = (1.0 / g).clamp(1.0, 1e3);
    let mut effects = hauksbee_solve::DeviceEffects::default();
    effects.junction_caps = c.junction_caps;
    effects.series_resistance = c.parasitics || effects.series_resistance;

    // Start from defaults so new solver knobs (partitioning, ...) keep their
    // sensible values, then override what the UI controls.
    let mut opts = SolverOptions::default();
    opts.integration = integration;
    opts.step = step;
    opts.reltol = 1e-3 * scale;
    opts.vntol = 1e-6 * scale;
    opts.abstol = 1e-12 * scale;
    opts.chgtol = 1e-14 * scale;
    opts.temperature_c = c.temperature_c;
    opts.effects = effects;
    opts.granularity = g;
    opts
}
