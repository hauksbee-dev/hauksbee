//! [`GalvaniEngine`]: the real engine behind `galvani-server`'s [`Engine`]
//! trait. Wraps a [`Scheduler`] over a [`BoundBoard`] and translates the wire
//! [`SolverControls`] onto [`SolverOptions`].

use std::collections::HashMap;
use std::path::Path;

use galvani_extract::ExtractedBoard;
use galvani_models::ModelLibrary;
use galvani_server::engine::Engine;
use galvani_server::protocol::{
    BoardInfo, ChemistryConfig, FaultInfo, PowerSupplyConfig, SimFrame, SolverControls,
    SupplyState, UsbSpecConfig,
};
use galvani_solve::{Integration, SolverOptions, StepControl};

use crate::binder::{bind_board, BoundBoard};
use crate::power_supply::{Chemistry, PowerSupply, UsbSpec};
use crate::report::BindReport;
use crate::scheduler::Scheduler;

/// The live engine: a co-sim scheduler plus board metadata for the protocol.
pub struct GalvaniEngine {
    sched: Scheduler,
    board_name: String,
    board_url: String,
    net_names: Vec<String>,
    component_kinds: HashMap<String, String>,
    mcu_backends: Vec<(String, String)>,
    controls: SolverControls,
    report_store: BindReport,
}

impl GalvaniEngine {
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
        let sched = Scheduler::new(bound, firmware, opts)?;
        Ok(GalvaniEngine {
            sched,
            board_name,
            board_url: board_url.to_string(),
            net_names,
            component_kinds,
            mcu_backends,
            controls,
            report_store,
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
        GalvaniEngine::from_bound(bound, firmware, board_url)
    }

    /// The bind report for this engine's board.
    pub fn report(&self) -> &BindReport {
        &self.report_store
    }

    /// Direct access to the scheduler (headless stats, tests).
    pub fn scheduler(&self) -> &Scheduler {
        &self.sched
    }
    pub fn scheduler_mut(&mut self) -> &mut Scheduler {
        &mut self.sched
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
    pub fn apply_drc_shorts(&mut self, report: &galvani_extract::DrcReport) -> usize {
        self.sched.apply_drc_shorts(report)
    }

    /// Convenience: extract + bind + build, then run geometric DRC on the same
    /// board text and apply every detected short before simulating. This is the
    /// "detect shorts from geometry, then simulate what the board does with them
    /// present" path. Returns the engine and the DRC report.
    pub fn from_board_file_with_drc_shorts(
        board_text: &str,
        firmware: Option<&Path>,
        board_url: &str,
    ) -> anyhow::Result<(Self, galvani_extract::DrcReport)> {
        let mut engine = Self::from_board_file(board_text, firmware, board_url)?;
        let report = ExtractedBoard::drc(board_text)?;
        engine.apply_drc_shorts(&report);
        Ok((engine, report))
    }
}

impl Engine for GalvaniEngine {
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
        }
    }

    fn step(&mut self, dt: f64) -> SimFrame {
        let result = self.sched.step(dt);
        let mut component_states = self.sched.mcu_states();
        component_states.extend(self.sched.digital_states());
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
            realtime_factor: 1.0,
            net_voltages: self.sched.net_voltages(),
            component_states,
            uart: result.uart,
            net_currents: Default::default(),
            faults,
            supply_states,
        }
    }

    fn reset(&mut self) {
        self.sched.sim_time = 0.0;
        for st in self.sched.stats.values_mut() {
            *st = Default::default();
        }
    }

    fn set_controls(&mut self, controls: SolverControls) {
        self.controls = controls.clone();
        self.sched.opts = controls_to_options(&controls);
        self.sched.set_destructive_faults(controls.destructive_faults);
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
    let mut effects = galvani_solve::DeviceEffects::default();
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
