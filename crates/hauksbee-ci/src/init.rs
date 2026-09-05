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

/// Scaffold `<board-stem>.toml` into the CURRENT DIRECTORY and return its
/// path. The old default (beside the board) put the spec where no CI tooling
/// looks; writing where the user is standing means `hauksbee-ci init
/// hardware/board.kicad_pcb` run from `ci/` lands the spec in `ci/`, which is
/// exactly where the pre-commit hook and the action's auto-detect search.
/// The generated `board = "..."` path is computed relative to the output
/// directory, so the spec is valid wherever it lands. Refuses to overwrite an
/// existing file: the point is a starting point, not a clobber of
/// hand-written work.
pub fn init(board: &Path) -> Result<PathBuf, SpecError> {
    init_to(board, None)
}

/// [`init`] with an explicit destination (`--out`): a path ending in `.toml` is
/// the spec file itself, and anything else is a directory that gets
/// `<board-stem>.toml` inside it (created if it does not exist yet). `None`
/// means the current directory.
///
/// The suffix decides, not the filesystem. `--out ci` on a repo that has no
/// `ci/` yet is the common first command, and the guidance printed right after
/// says specs are discovered in `ci/`; resolving that to a FILE named `ci`
/// would scaffold a spec no tool ever finds, which is a gate that silently
/// checks nothing.
pub fn init_to(board: &Path, out: Option<&Path>) -> Result<PathBuf, SpecError> {
    let out = match out {
        None => {
            let cwd = std::env::current_dir()
                .map_err(|e| SpecError::Io(format!("reading the current directory: {e}")))?;
            cwd.join(format!("{}.toml", board_stem(board)))
        }
        Some(p) => {
            // A `.toml` suffix names the spec file; every other spelling is a
            // directory to write `<board-stem>.toml` into, whether or not it
            // exists yet.
            if p.is_dir() || !names_a_spec_file(p) {
                std::fs::create_dir_all(p)
                    .map_err(|e| SpecError::Io(format!("creating {}: {e}", p.display())))?;
                p.join(format!("{}.toml", board_stem(board)))
            } else {
                if let Some(parent) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        SpecError::Io(format!("creating {}: {e}", parent.display()))
                    })?;
                }
                p.to_path_buf()
            }
        }
    };
    if out.exists() {
        return Err(SpecError::Invalid(format!(
            "{} already exists; refusing to overwrite it. Move it aside (or delete it) to regenerate the starter spec.",
            out.display()
        )));
    }
    let spec_dir = out
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let run_hint = if out.is_relative() {
        out.clone()
    } else {
        std::env::current_dir()
            .ok()
            .and_then(|cwd| relative_path(&cwd, &out))
            .unwrap_or_else(|| out.clone())
    };
    let spec = render_spec_at_with_run_hint(board, &spec_dir, &run_hint.display().to_string())?;
    std::fs::write(&out, spec)
        .map_err(|e| SpecError::Io(format!("writing {}: {e}", out.display())))?;
    Ok(out)
}

/// The USB profile scaffolded for a detected USB rail: a BC1.2 / USB-C 1.5 A
/// port, the commonest thing a hobby board is plugged into. Generous enough
/// that the starter spec stays green on a board that behaves, tight enough that
/// a real inrush shows as droop.
const USB_PROFILE: &str = "5v1.5a";

/// The bench-PSU current limit scaffolded for a detected rail with no better
/// story. A bench supply is the honest default for "something outside the board
/// feeds this net": it holds `volts` until the board asks for more than this,
/// then folds back, which is what makes a rail assertion on the net falsifiable.
const BENCH_LIMIT_A: f64 = 2.0;

/// Does this detected rail read as a USB port, so the scaffold can model the
/// actual source instead of a generic bench supply? Both halves must agree: the
/// name says USB and the detected voltage is the 5 V a USB port supplies. A
/// `VBUS_3V3` level-shifter net is not a port.
fn is_usb_port_rail(net: &str, volts: f64) -> bool {
    let n = net.to_ascii_uppercase();
    (n.contains("VBUS") || n.contains("USB")) && (volts - 5.0).abs() < 0.1
}

/// Is this `--out` value the spec file itself rather than a directory to put
/// the spec in? A `.toml` extension (any case) says file; everything else,
/// including a bare name like `ci` and an explicit `ci/`, says directory.
fn names_a_spec_file(p: &Path) -> bool {
    p.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("toml"))
}

/// Does a net NAME read as a power net even though the rail detector could
/// not price it (so `power_rail_voltage` returned `None`)? Used only to keep
/// the scaffold's boot/toggle examples off nets like `VSUP_UNLIMITED` or
/// `PWR_EN_RAIL`; a false positive here just means the example falls back to
/// a placeholder, so the heuristic can afford to be broad.
fn looks_like_power_name(net: &str) -> bool {
    let n = net.to_ascii_uppercase();
    [
        "VSUP", "VDD", "VCC", "VBUS", "VBAT", "BAT+", "PWR", "POWER", "SUPPLY", "VIN", "RAIL",
    ]
    .iter()
    .any(|tag| n.contains(tag))
}

fn board_stem(board: &Path) -> String {
    board
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("board")
        .to_string()
}

/// Render the starter spec's TOML text for `board`, with the `board = "..."`
/// reference written relative to the board's own directory (the historical
/// beside-the-board shape). Kept for callers that place the file themselves.
pub fn render_spec(board: &Path) -> Result<String, SpecError> {
    let dir = board
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    render_spec_at(board, &dir)
}

/// The path string the generated spec's `board = "..."` line carries: relative
/// from `spec_dir` to the board when both resolve, absolute otherwise. A spec
/// whose board reference is wrong on arrival would fail its very first run,
/// the opposite of "your first spec is an edit".
fn board_reference(board: &Path, spec_dir: &Path) -> String {
    let board_abs = std::fs::canonicalize(board).unwrap_or_else(|_| board.to_path_buf());
    let dir_abs = std::fs::canonicalize(spec_dir).unwrap_or_else(|_| spec_dir.to_path_buf());
    match relative_path(&dir_abs, &board_abs) {
        Some(rel) => rel.display().to_string(),
        None => board_abs.display().to_string(),
    }
}

/// `to` expressed relative to the directory `from` (both absolute), walking up
/// with `..` where needed. `None` when they share no root (different drives).
fn relative_path(from: &Path, to: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let from: Vec<Component> = from.components().collect();
    let to: Vec<Component> = to.components().collect();
    // Different prefixes (Windows drives) cannot be related; same-root paths
    // always share at least the root component.
    let common = from
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a == b)
        .count();
    if common == 0 {
        return None;
    }
    let mut rel = PathBuf::new();
    for _ in common..from.len() {
        rel.push("..");
    }
    for c in &to[common..] {
        rel.push(c.as_os_str());
    }
    if rel.as_os_str().is_empty() {
        rel.push(".");
    }
    Some(rel)
}

/// Render the starter spec's TOML text for `board` as it should read when the
/// file lives in `spec_dir`. Split out from [`init_to`] so it can be exercised
/// without touching disk.
pub fn render_spec_at(board: &Path, spec_dir: &Path) -> Result<String, SpecError> {
    render_spec_at_with_run_hint(board, spec_dir, &format!("{}.toml", board_stem(board)))
}

fn render_spec_at_with_run_hint(
    board: &Path,
    spec_dir: &Path,
    run_hint: &str,
) -> Result<String, SpecError> {
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

    // Rails the binder found by name but could not put a voltage to. Same
    // detector the run uses, so the scaffold asks about exactly the nets the
    // report would later warn about.
    let unpowered = runner::unpowered_supply_nets(&extracted, &bound, &[]);

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
    // drive). Power-ish NAMES the rail detector does not price (VSUP_*,
    // VBAT_*, PWR_*) are excluded too: a scaffolded boot/toggle example on a
    // power net reads as nonsense on arrival and teaches the wrong shape.
    // None such -> a named placeholder the user replaces.
    let boot_net = extracted
        .nets
        .iter()
        .map(|n| n.name.as_str())
        .find(|n| {
            !n.is_empty()
                && !is_ground(n)
                && power_rail_voltage(n).is_none()
                && !looks_like_power_name(n)
        })
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
    // direction register (stm32f103 CRL/CRH, stm32f4 MODER, nrf52840 DIR, see
    // db/mcu/*.soc.toml). The `qemu:` ESP32 family and unmapped Renode parts
    // cannot; on those a scaffolded boot-coverage assertion can go RED with a
    // misleading diagnosis on a net the firmware actually drives LOW (or via an
    // unmodelled peripheral bus), so the assertion is emitted commented-out with
    // an honest note naming the gap and the user opts in deliberately.
    let boot_coverage_supported = mcu_backend.as_deref().map_or(
        true,
        hauksbee_engine::scheduler::backend_reports_drive_direction,
    );

    let stem = board_stem(board);
    let board_file = board_reference(board, spec_dir);
    let docs = hauksbee_ir::docs_url("docs/ci/CI.md");

    let mut s = format!(
        "# hauksbee-ci starter spec, generated by `hauksbee-ci init`.\n\
         # Every line is commented with what it does. Uncomment and tune, then run:\n\
         #   hauksbee-ci run {run_hint}\n\
         # The board, MCU, supplies and rails below were detected from the board.\n\
         # Full assertion catalog ({docs}): voltage, uart, toggle, no_faults,\n\
         #   max_current, max_temp, rail_window, protection_trip, boot_coverage,\n\
         #   phase_margin, ac_gain, peripheral, hwtrace, model_coverage.\n\
         \n\
         name = \"{stem} power-up\"        # label shown in reports\n\
         board = \"{board_file}\"          # the design file this spec checks\n\
         # bom = \"hardware/bom.csv\"        # exact purchased-part identity (optional)\n\
         # bom_columns = [\"reference=Customer Reference\"] # confirm an ambiguous header\n\
         # placement = \"hardware/positions.csv\" # assembly position/side cross-check (optional)\n\
         # variant = \"hardware/prototype.variant.toml\" # explicit fit/no-fit assembly (optional)\n"
    );

    // MCU + firmware placeholder. The informational value is what the BOARD
    // says (the part value), not the coarse backend family it collapses to:
    // printing "atmega328p" for an RP2040 board because of a family fallback
    // would teach the user a falsehood on line three of their first spec.
    // When the modelled core differs from the requested part, both are named.
    let requested = bound
        .mcus
        .first()
        .map(|m| m.requested_part.as_str())
        .filter(|r| !r.is_empty());
    match (&mcu_kind, requested) {
        (Some(kind), Some(requested)) if !requested.eq_ignore_ascii_case(kind) => {
            let _ = writeln!(s, "mcu = \"{requested}\"                # board's MCU (informational); co-simmed on the {kind} core");
        }
        (Some(kind), _) => {
            let _ = writeln!(s, "mcu = \"{kind}\"                # detected MCU (informational; the binder auto-detects)");
        }
        (None, _) => {
            s.push_str("# mcu = \"atmega328p\"          # no MCU detected (informational note only; the binder detects the MCU from the board)\n");
        }
    }
    s.push_str(
        "# firmware = \"firmware/build/app.elf\"   # ELF/hex to boot on the MCU (co-sim your firmware)\n\
         duration_ms = 200               # simulated time to run\n\
         \n\
         # Supplies: power the rails the board expects. kind is one of\n\
         # ideal | bench | wall | usb | battery.\n\
         # Each detected rail is scaffolded as a behavioral source (a bench PSU with a\n\
         # current limit, or a USB port with its droop), never as kind = \"ideal\", so\n\
         # that the rail assertions further down are gates rather than decoration: an\n\
         # ideal source pins its net to `volts` whatever the board draws, so a voltage\n\
         # or rail_window check on that net cannot fail for a board reason and its green\n\
         # vouches for nothing. A bench PSU folds back at its limit and a USB port sags\n\
         # across its cable, so the rail can move and the check can fire.\n",
    );
    // Supplies (enabled): one leg per detected rail, modelled BEHAVIORALLY.
    if supplies.is_empty() {
        s.push_str(
            "# No supply rail was detected; add one the board is fed from:\n\
             # [[supply]]\n# net = \"+5V\"\n# kind = \"bench\"\n# volts = 5.0\n# current_limit_a = 2.0\n",
        );
    }
    for (net, v) in &supplies {
        let _ = writeln!(
            s,
            "[[supply]]\nnet = \"{net}\"                   # detected supply rail"
        );
        if is_usb_port_rail(net, *v) {
            let _ = writeln!(
                s,
                "kind = \"usb\"                     # the name and the 5 V say this rail is a USB port\n\
                 usb = \"{USB_PROFILE}\"               # what the port negotiates: 5v0.5a | 5v1.5a | 5v3a"
            );
        } else {
            let _ = writeln!(
                s,
                "kind = \"bench\"\nvolts = {}\n\
                 current_limit_a = {BENCH_LIMIT_A:.1}            # what the source delivers before it folds back; set yours",
                fmt1(*v)
            );
        }
    }
    // Rails whose name says "supply" and nothing else. Nobody can read a voltage
    // off ANALOG_VDD or bare VDD, so the binder refuses to invent one and the net
    // sits at 0 V. Leaving that for the user to discover means their first run
    // solves a board with a rail dead and reports whatever that implies. Only the
    // person who drew the schematic knows the number, so the scaffold asks rather
    // than guessing, and puts the question where they are already editing.
    if !unpowered.is_empty() {
        s.push_str(
            "\n# These nets name a supply but not a voltage, so nothing can work out\n\
             # what to feed them and they will sit at 0 V. Fill in the voltage and\n\
             # uncomment each one, or every analog result is solved around a dead rail.\n",
        );
        for net in &unpowered {
            let _ = writeln!(
                s,
                "# [[supply]]\n# net = \"{net}\"\n# kind = \"ideal\"\n\
                 # volts =                     # what does this rail run at?"
            );
        }
    }

    // Assertions. no_faults is the one live gate. boot_coverage is always
    // scaffolded COMMENTED-OUT: it asserts on what the *firmware* does, and
    // `firmware = ...` is itself commented above, so the starter spec has no
    // image to boot. Left live it would go RED out of the box on every board
    // (the control net is never driven / the MCU never runs), handing the user
    // a false red on their very first run. So the starter is GREEN on
    // no_faults alone; the user opts into boot-coverage deliberately, after
    // wiring up their firmware.
    s.push_str(
        "\n# Assertions: at least one must hold for the build to go green.\n\
         \n\
         # no_faults: the stress monitor raised no over-current / over-voltage /\n\
         # over-power / reverse-bias / over-temperature fault across the run.\n\
         [[assert]]\nkind = \"no_faults\"\n\
         \n\
         # boot_coverage: a control net (a gate / enable / reset / chip-select) the\n\
         # firmware must actively drive to a defined level within a deadline of reset,\n\
         # with no stress fault during the boot window before it does.\n\
         # NOTE: left commented-out. It boots your firmware and checks the control net,\n\
         #   so it only means something once `firmware = ...` above points at a real\n\
         #   ELF/hex. Uncomment both the firmware line and this block together.\n",
    );
    if !boot_coverage_supported {
        let _ = writeln!(
            s,
            "#   Also note this board's MCU runs on the `{}` backend. It co-sims\n\
             #   GPIO and UART ({}), but its platform has no verified direction-\n\
             #   register map, so it cannot report pin drive DIRECTION and cannot\n\
             #   distinguish a held-LOW pin from an undriven one. On it, watch only a net\n\
             #   driven by plain GPIO to a defined HIGH level. AVR boards and the\n\
             #   direction-mapped Renode parts (STM32F103/F4, nRF52840) can watch a\n\
             #   held-LOW net too.",
            mcu_backend.as_deref().unwrap_or(""),
            hauksbee_ir::docs_url("docs/cosim/MCU.md")
        );
    }
    let boot_net_line = match &boot_net {
        Some(net) => format!("# net = \"{net}\"                  # control net to watch (edit to your gate/enable/reset/CS)"),
        None => "# net = \"CONTROL_NET\"            # no signal net detected; set this to a real control net".to_string(),
    };
    let _ = writeln!(
        s,
        "# [[assert]]\n# kind = \"boot_coverage\"\n{boot_net_line}\n\
         # min = {}                    # driven level (V) the firmware must reach\n\
         # deadline_ms = 100.0             # by this long after reset\n",
        fmt1(boot_level)
    );

    // Commented voltage assertions on the rails. A rail the BOARD derives (a
    // regulator output, whatever hangs off the input rail) is the strongest gate
    // the scaffold can offer, because every part between the source and that net
    // is under test, so those come first and are named as such. The rails fed
    // straight from a declared supply leg come after; they gate too, but only
    // because the supply above is behavioral.
    let (derived, source_fed): (Vec<_>, Vec<_>) = rails
        .iter()
        .partition(|(net, _)| !supplies.iter().any(|(s, _)| s == net));
    s.push_str(
        "# voltage: a rail stays within bounds (min = worst dip, max = worst rise).\n\
         # Uncomment the rails you want gated and tune the tolerance.\n",
    );
    if rails.is_empty() {
        s.push_str(
            "# [[assert]]\n# kind = \"voltage\"\n# net = \"+5V\"\n# min = 4.75\n# max = 5.25\n",
        );
    }
    if !derived.is_empty() {
        s.push_str(
            "# These rails the board makes for itself, so a check on them measures the\n\
             # regulator, the filtering and the load between the input and the net:\n",
        );
    }
    let noted = (derived.iter().map(|r| (r, "derived by the board")))
        .chain(source_fed.iter().map(|r| (r, "fed by the supply above")));
    for ((net, v), note) in noted {
        let _ = writeln!(
            s,
            "# [[assert]]\n# kind = \"voltage\"\n\
             # net = \"{net}\"                  # rail detected at ~{} V, {note}\n\
             # min = {}\n# max = {}\n\
             # after_ms = 50                  # only sample once it has settled",
            fmt1(*v),
            fmt2(v * 0.95),
            fmt2(v * 1.05)
        );
    }

    // Transient / brownout on-ramp: scaffold a [[profile]] + [[scenario]] +
    // rail_window whenever a supply rail was detected. This is the ONLY place a
    // user discovers dynamic analysis from `hauksbee run`; there is no `run
    // --transient` flag; brownout/inrush lives here in the spec. Commented so
    // the starter stays GREEN on no_faults alone; the user opts in and tunes the
    // load profile to their board. See docs/checks/TRANSIENTS.md. `part` is
    // required by the scenario loader (the load attaches to a component); it
    // is scaffolded with the detected MCU so uncommenting the block as written
    // always parses. The floor is derived from THIS rail's own detected
    // voltage (-5%), never from the board-wide reference: a 4.5 V floor
    // scaffolded onto a 1.1 V core rail is wrong on arrival.
    if let Some((rail, rail_v)) = supplies.first() {
        let scenario_part = bound
            .mcus
            .first()
            .map(|m| m.reference.as_str())
            .unwrap_or("U1");
        let _ = writeln!(
            s,
            "\n# rail_window: does the rail stay up under a dynamic load step (a WiFi\n\
             # burst, a motor kick, an inrush)? This is the transient/brownout check:\n\
             # there is NO `run --transient` flag; it lives here. A [[profile]] shapes\n\
             # the load current, a [[scenario]] attaches it to a supply net, and the\n\
             # rail_window assert bounds the rail while it runs. See {}.\n\
             # [[profile]]\n# id = \"load_step\"\n# [[profile.segment]]\n\
             # level_a = 0.05                 # baseline current (A)\n\
             # rise_s = 0.001\n# duration_s = 0.0\n# [[profile.segment]]\n\
             # level_a = 0.5                  # the step / burst current (A); tune to your load\n\
             # rise_s = 0.0005\n# duration_s = 0.010\n# period_s = 0.100\n# idle_a = 0.05\n#\n\
             # [[scenario]]\n# id = \"step\"\n\
             # part = \"{scenario_part}\"                   # the component drawing the load\n\
             # profile = \"load_step\"\n\
             # supply_net = \"{rail}\"          # detected rail the load hangs off\n\
             # start_ms = 1.0\n#\n\
             # [[assert]]\n# kind = \"rail_window\"\n# scenario = \"step\"\n# net = \"{rail}\"\n\
             # min = {}                    # 5% below the rail's detected {} V; tune to your budget",
            hauksbee_ir::docs_url("docs/checks/TRANSIENTS.md"),
            fmt2(rail_v * 0.95),
            fmt1(*rail_v)
        );
    }

    // Firmware-behaviour assertions; the tool's headline pitch ("assert the UART
    // says hello, assert the LED blinks"). Only meaningful once `firmware = ...`
    // above points at a real image, so scaffold them COMMENTED and only when an
    // MCU was detected; uncomment them together with the firmware line.
    if mcu_backend.is_some() {
        let _ = writeln!(
            s,
            "\n# uart: the firmware's serial output contains a string or matches a regex.\n\
             #   (needs `firmware = ...` above; the tool boots the image and reads the UART.)\n\
             # [[assert]]\n# kind = \"uart\"\n\
             # contains = \"hello\"             # a boot banner / heartbeat your firmware prints\n\
             \n\
             # toggle: a net toggles at an expected rate, a blink / clock / PWM check.\n\
             # [[assert]]\n# kind = \"toggle\"\n\
             # net = \"{}\"                  # the blinking / clocked net (edit to yours)\n\
             # freq_hz = 1.0                  # expected toggle rate (Hz)\n\
             # tolerance = 0.2                # +/-20%",
            boot_net.as_deref().unwrap_or("LED")
        );
    }

    Ok(s)
}

#[cfg(test)]
mod relative_path_tests {
    use super::relative_path;
    use std::path::{Path, PathBuf};

    #[test]
    fn sibling_parent_and_nested_shapes_all_resolve() {
        let rel = |from: &str, to: &str| relative_path(Path::new(from), Path::new(to));
        assert_eq!(
            rel("/repo/ci", "/repo/hardware/board.kicad_pcb"),
            Some(PathBuf::from("../hardware/board.kicad_pcb"))
        );
        assert_eq!(
            rel("/repo", "/repo/hardware/board.kicad_pcb"),
            Some(PathBuf::from("hardware/board.kicad_pcb"))
        );
        assert_eq!(
            rel("/repo/a/b", "/repo/board.kicad_pcb"),
            Some(PathBuf::from("../../board.kicad_pcb"))
        );
        // Same directory: the bare file name.
        assert_eq!(
            rel("/repo", "/repo/board.kicad_pcb"),
            Some(PathBuf::from("board.kicad_pcb"))
        );
    }
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
