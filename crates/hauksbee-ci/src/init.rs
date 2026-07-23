//! `hauksbee-ci init <board>`: scaffold a starter spec from a real board, so a
//! user's first CI spec is an edit and not a blank page.
//!
//! It loads and binds the board through the same extract+bind path the runner
//! uses, then reads the detected supplies (the binder's supply legs), the
//! detected MCU, and the board's rail-looking nets straight off the bound board.
//! Every generated line carries a short comment naming what it does, cribbed
//! from docs/ci/CI.md, so the file teaches its own format as the user edits it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use hauksbee_engine::{bind_board, is_ground, power_rail_voltage};
use hauksbee_models::ModelLibrary;

use crate::error::SpecError;
use crate::runner;

/// Scaffold `<board-stem>.toml` beside the board and return its path. Refuses to
/// overwrite an existing file: the point is a starting point, not a clobber of
/// hand-written work.
pub fn init(board: &Path) -> Result<PathBuf, SpecError> {
    let out = board.with_file_name(format!("{}.toml", board_stem(board)));
    if out.exists() {
        return Err(SpecError::Invalid(format!(
            "{} already exists; refusing to overwrite it. Move it aside (or delete it) to regenerate the starter spec.",
            out.display()
        )));
    }
    let spec = render_spec(board)?;
    std::fs::write(&out, spec)
        .map_err(|e| SpecError::Io(format!("writing {}: {e}", out.display())))?;
    Ok(out)
}

fn board_stem(board: &Path) -> String {
    board
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("board")
        .to_string()
}

/// Render the starter spec's TOML text for `board`. Split out from [`init`] so
/// it can be exercised without touching disk.
pub fn render_spec(board: &Path) -> Result<String, SpecError> {
    let extracted = runner::load_board(board)?;
    // Same layered library as a run (builtin → packs → user model dirs), so
    // the scaffold detects the MCU/supplies a run would actually bind.
    let lib = ModelLibrary::builtin_with_user_dirs(&[]);
    let bound = bind_board(&extracted, &lib);

    // The board's own detected supplies: the binder stamps one supply leg per
    // rail it found, at that rail's nominal voltage. Reuse them verbatim so the
    // scaffold powers exactly what a run would.
    let mut supplies: Vec<(String, f64)> = bound
        .supplies
        .iter()
        .map(|leg| (leg.net_name.clone(), leg.supply.nominal_volts()))
        .collect();
    supplies.sort_by(|a, b| a.0.cmp(&b.0));

    // Rail-looking nets, from the shared rail-name helper rather than a fresh
    // heuristic: every non-ground net the binder recognises as a supply rail,
    // deduped and ordered. These become commented voltage assertions.
    let mut rails: Vec<(String, f64)> = Vec::new();
    for net in &extracted.nets {
        if is_ground(&net.name) {
            continue;
        }
        if let Some(v) = power_rail_voltage(&net.name) {
            if !rails.iter().any(|(n, _)| n == &net.name) {
                rails.push((net.name.clone(), v));
            }
        }
    }
    rails.sort_by(|a, b| a.0.cmp(&b.0));

    // Reference rail voltage the boot-coverage "driven high" threshold keys off:
    // the highest detected supply or rail, falling back to 3.3 V.
    let vref = supplies
        .iter()
        .chain(rails.iter())
        .map(|(_, v)| *v)
        .fold(0.0_f64, f64::max);
    let vref = if vref > 0.0 { vref } else { 3.3 };
    let boot_level = round1(vref * 0.7); // a logic-high threshold, one decimal

    // A concrete control net for the boot-coverage assertion: the first
    // non-rail, non-ground signal net (what the firmware is most likely to
    // drive). None such -> a named placeholder the user replaces.
    let boot_net = extracted
        .nets
        .iter()
        .map(|n| n.name.as_str())
        .find(|n| !n.is_empty() && !is_ground(n) && power_rail_voltage(n).is_none())
        .map(str::to_string);

    // Detected MCU (first, if any). The binder's backend string is
    // "<backend>:<kind>"; the spec's `mcu` hint wants just the kind.
    let mcu_backend = bound.mcus.first().map(|m| m.backend.clone());
    let mcu_kind = mcu_backend
        .as_deref()
        .map(|b| b.rsplit(':').next().unwrap_or(b).to_string());

    // Can the detected MCU's backend actually satisfy a boot-coverage assertion?
    // The gate is pin drive DIRECTION: a backend that reports it can tell a
    // held-LOW control net from an undriven one, so its boot-coverage diagnosis
    // is trustworthy. The in-process AVR backend always reports it (DDR hooks),
    // and a `renode:` part does once its SoC descriptor maps every GPIO port's
    // direction register (stm32f103 CRL/CRH, stm32f4 MODER, nrf52840 DIR — see
    // db/mcu/*.soc.toml). The `qemu:` ESP32 family and unmapped Renode parts
    // cannot; on those a scaffolded boot-coverage assertion can go RED with a
    // misleading diagnosis on a net the firmware actually drives LOW (or via an
    // unmodelled peripheral bus), so the assertion is emitted commented-out with
    // an honest note naming the gap and the user opts in deliberately.
    let boot_coverage_supported = mcu_backend
        .as_deref()
        .map_or(true, hauksbee_engine::scheduler::backend_reports_drive_direction);

    let stem = board_stem(board);
    let board_file = board
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("board.kicad_pcb");

    let mut s = String::new();
    let _ = writeln!(s, "# hauksbee-ci starter spec, generated by `hauksbee-ci init`.");
    let _ = writeln!(s, "# Every line is commented with what it does. Uncomment and tune, then run:");
    let _ = writeln!(s, "#   hauksbee-ci run {stem}.toml");
    let _ = writeln!(s, "# The board, MCU, supplies and rails below were detected from the board.");
    let _ = writeln!(
        s,
        "# Full assertion catalog (docs/ci/CI.md): voltage, uart, toggle, no_faults,\n\
         #   max_current, max_temp, rail_window, protection_trip, boot-coverage,\n\
         #   phase_margin, ac_gain, peripheral, hwtrace."
    );
    let _ = writeln!(s);
    let _ = writeln!(s, "name = \"{stem} power-up\"        # label shown in reports");
    let _ = writeln!(s, "board = \"{board_file}\"          # the design file this spec checks");

    // MCU + firmware placeholder.
    match &mcu_kind {
        Some(kind) => {
            let _ = writeln!(
                s,
                "mcu = \"{kind}\"                # detected MCU (informational; the binder auto-detects)"
            );
        }
        None => {
            let _ = writeln!(
                s,
                "# mcu = \"atmega328p\"          # no MCU detected (informational note only; the binder detects the MCU from the board)"
            );
        }
    }
    let _ = writeln!(
        s,
        "# firmware = \"firmware/build/app.elf\"   # ELF/hex to boot on the MCU (co-sim your firmware)"
    );
    let _ = writeln!(s, "duration_ms = 200               # simulated time to run");
    let _ = writeln!(s);

    // Supplies (enabled): one leg per detected rail.
    let _ = writeln!(s, "# Supplies: power the rails the board expects. kind is one of");
    let _ = writeln!(s, "# ideal | bench | wall | usb | battery (ideal = a stiff rail at `volts`).");
    if supplies.is_empty() {
        let _ = writeln!(s, "# No supply rail was detected; add one the board is fed from:");
        let _ = writeln!(s, "# [[supply]]");
        let _ = writeln!(s, "# net = \"+5V\"");
        let _ = writeln!(s, "# kind = \"ideal\"");
        let _ = writeln!(s, "# volts = 5.0");
    } else {
        for (net, v) in &supplies {
            let _ = writeln!(s, "[[supply]]");
            let _ = writeln!(s, "net = \"{net}\"                   # detected supply rail");
            let _ = writeln!(s, "kind = \"ideal\"");
            let _ = writeln!(s, "volts = {}", fmt1(*v));
        }
    }
    let _ = writeln!(s);

    // Assertions.
    let _ = writeln!(s, "# Assertions: at least one must hold for the build to go green.");
    let _ = writeln!(s);
    let _ = writeln!(s, "# no_faults: the stress monitor raised no over-current / over-voltage /");
    let _ = writeln!(s, "# over-power / reverse-bias / over-temperature fault across the run.");
    let _ = writeln!(s, "[[assert]]");
    let _ = writeln!(s, "kind = \"no_faults\"");
    let _ = writeln!(s);
    let _ = writeln!(s, "# boot-coverage: a control net (a gate / enable / reset / chip-select) the");
    let _ = writeln!(s, "# firmware must actively drive to a defined level within a deadline of reset,");
    let _ = writeln!(s, "# with no stress fault during the boot window before it does.");
    // Always scaffold boot-coverage COMMENTED-OUT: it asserts on what the
    // *firmware* does, and `firmware = ...` is itself commented above, so the
    // starter spec has no image to boot. Left live it would go RED out of the
    // box on every board (the control net is never driven / the MCU never runs),
    // which is exactly the false-red first-run the persona panel hit. So the
    // starter is GREEN on `no_faults` alone; the user opts into boot-coverage
    // deliberately, after wiring up their firmware. The `cc` prefix stays a
    // variable so the two `[[assert]]` blocks below read the same as the other
    // assertion sections.
    let cc = "# ";
    let _ = writeln!(s, "# NOTE: left commented-out. It boots your firmware and checks the control net,");
    let _ = writeln!(s, "#   so it only means something once `firmware = ...` above points at a real");
    let _ = writeln!(s, "#   ELF/hex. Uncomment both the firmware line and this block together.");
    if !boot_coverage_supported {
        let backend = mcu_backend.as_deref().unwrap_or("");
        let _ = writeln!(s, "#   Also note this board's MCU runs on the `{backend}` backend. It co-sims");
        let _ = writeln!(s, "#   GPIO and UART (docs/cosim/MCU.md), but its platform has no verified direction-");
        let _ = writeln!(s, "#   register map, so it cannot report pin drive DIRECTION and cannot");
        let _ = writeln!(s, "#   distinguish a held-LOW pin from an undriven one. On it, watch only a net");
        let _ = writeln!(s, "#   driven by plain GPIO to a defined HIGH level. AVR boards and the");
        let _ = writeln!(s, "#   direction-mapped Renode parts (STM32F103/F4, nRF52840) can watch a");
        let _ = writeln!(s, "#   held-LOW net too.");
    }
    let _ = writeln!(s, "{cc}[[assert]]");
    let _ = writeln!(s, "{cc}kind = \"boot-coverage\"");
    match &boot_net {
        Some(net) => {
            let _ = writeln!(
                s,
                "{cc}net = \"{net}\"                  # control net to watch (edit to your gate/enable/reset/CS)"
            );
        }
        None => {
            let _ = writeln!(
                s,
                "{cc}net = \"CONTROL_NET\"            # no signal net detected; set this to a real control net"
            );
        }
    }
    let _ = writeln!(s, "{cc}min = {}                    # driven level (V) the firmware must reach", fmt1(boot_level));
    let _ = writeln!(s, "{cc}deadline_ms = 100.0             # by this long after reset");
    let _ = writeln!(s);

    // Commented voltage assertions on the rails.
    let _ = writeln!(s, "# voltage: a rail stays within bounds (min = worst dip, max = worst rise).");
    let _ = writeln!(s, "# Uncomment the rails you want gated and tune the tolerance.");
    if rails.is_empty() {
        let _ = writeln!(s, "# [[assert]]");
        let _ = writeln!(s, "# kind = \"voltage\"");
        let _ = writeln!(s, "# net = \"+5V\"");
        let _ = writeln!(s, "# min = 4.75");
        let _ = writeln!(s, "# max = 5.25");
    } else {
        for (net, v) in &rails {
            let _ = writeln!(s, "# [[assert]]");
            let _ = writeln!(s, "# kind = \"voltage\"");
            let _ = writeln!(s, "# net = \"{net}\"                  # rail detected at ~{} V", fmt1(*v));
            let _ = writeln!(s, "# min = {}", fmt2(v * 0.95));
            let _ = writeln!(s, "# max = {}", fmt2(v * 1.05));
            let _ = writeln!(s, "# after_ms = 50                  # only sample once it has settled");
        }
    }

    // Transient / brownout on-ramp: scaffold a [[profile]] + [[scenario]] +
    // rail_window whenever a supply rail was detected. This is the ONLY place a
    // user discovers dynamic analysis from `hauksbee run` — there is no `run
    // --transient` flag; brownout/inrush lives here in the spec. Commented so
    // the starter stays GREEN on no_faults alone; the user opts in and tunes the
    // load profile to their board. See docs/checks/TRANSIENTS.md.
    if let Some((rail, _)) = supplies.first() {
        let _ = writeln!(s);
        let _ = writeln!(s, "# rail_window: does the rail stay up under a dynamic load step (a WiFi");
        let _ = writeln!(s, "# burst, a motor kick, an inrush)? This is the transient/brownout check —");
        let _ = writeln!(s, "# there is NO `run --transient` flag; it lives here. A [[profile]] shapes");
        let _ = writeln!(s, "# the load current, a [[scenario]] attaches it to a supply net, and the");
        let _ = writeln!(s, "# rail_window assert bounds the rail while it runs. See docs/checks/TRANSIENTS.md.");
        let _ = writeln!(s, "# [[profile]]");
        let _ = writeln!(s, "# id = \"load_step\"");
        let _ = writeln!(s, "# [[profile.segment]]");
        let _ = writeln!(s, "# level_a = 0.05                 # baseline current (A)");
        let _ = writeln!(s, "# rise_s = 0.001");
        let _ = writeln!(s, "# duration_s = 0.0");
        let _ = writeln!(s, "# [[profile.segment]]");
        let _ = writeln!(s, "# level_a = 0.5                  # the step / burst current (A) — tune to your load");
        let _ = writeln!(s, "# rise_s = 0.0005");
        let _ = writeln!(s, "# duration_s = 0.010");
        let _ = writeln!(s, "# period_s = 0.100");
        let _ = writeln!(s, "# idle_a = 0.05");
        let _ = writeln!(s, "#");
        let _ = writeln!(s, "# [[scenario]]");
        let _ = writeln!(s, "# id = \"step\"");
        let _ = writeln!(s, "# profile = \"load_step\"");
        let _ = writeln!(s, "# supply_net = \"{rail}\"          # detected rail the load hangs off");
        let _ = writeln!(s, "# start_ms = 1.0");
        let _ = writeln!(s, "#");
        let _ = writeln!(s, "# [[assert]]");
        let _ = writeln!(s, "# kind = \"rail_window\"");
        let _ = writeln!(s, "# scenario = \"step\"");
        let _ = writeln!(s, "# net = \"{rail}\"");
        let _ = writeln!(s, "# min = {}                    # the rail must never sag below this (V)", fmt2(vref * 0.9));
    }

    // Firmware-behaviour assertions — the tool's headline pitch ("assert the UART
    // says hello, assert the LED blinks"). Only meaningful once `firmware = ...`
    // above points at a real image, so scaffold them COMMENTED and only when an
    // MCU was detected; uncomment them together with the firmware line.
    if mcu_backend.is_some() {
        let _ = writeln!(s);
        let _ = writeln!(s, "# uart: the firmware's serial output contains a string or matches a regex.");
        let _ = writeln!(s, "#   (needs `firmware = ...` above — the tool boots the image and reads the UART.)");
        let _ = writeln!(s, "# [[assert]]");
        let _ = writeln!(s, "# kind = \"uart\"");
        let _ = writeln!(s, "# contains = \"hello\"             # a boot banner / heartbeat your firmware prints");
        let _ = writeln!(s);
        let _ = writeln!(s, "# toggle: a net toggles at an expected rate — a blink / clock / PWM check.");
        let toggle_net = boot_net.as_deref().unwrap_or("LED");
        let _ = writeln!(s, "# [[assert]]");
        let _ = writeln!(s, "# kind = \"toggle\"");
        let _ = writeln!(s, "# net = \"{toggle_net}\"                  # the blinking / clocked net (edit to yours)");
        let _ = writeln!(s, "# freq_hz = 1.0                  # expected toggle rate (Hz)");
        let _ = writeln!(s, "# tolerance = 0.2                # +/-20%");
    }

    Ok(s)
}

/// Round to one decimal place.
fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Format a voltage with one decimal place ("5.0", "3.3"), so TOML always reads
/// as a float rather than an int the user might mistake for a count.
fn fmt1(v: f64) -> String {
    format!("{v:.1}")
}

/// Format a voltage bound with two decimals ("4.75", "5.25").
fn fmt2(v: f64) -> String {
    format!("{v:.2}")
}
