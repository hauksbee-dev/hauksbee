//! Shared helpers for the engine crate's integration-test binary.

use std::path::PathBuf;
use std::process::{Command, Output};

use hauksbee_engine::binder::BoundBoard;
use hauksbee_engine::HauksbeeEngine;
use hauksbee_extract::{Component, Net, Pin};
use hauksbee_frontdoor_api::engine::Engine;
use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};

/// The compiled `hauksbee` binary (Cargo sets this for the engine crate's tests).
pub const fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

/// The compiled `hauksbee` binary as an owned path.
pub fn hauksbee_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hauksbee"))
}

/// Workspace-relative example boards, resolved from this crate's manifest dir.
pub fn board(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

/// `rel` under the repository's `testdata/`, canonicalized, or `None` when this
/// checkout does not carry it.
///
/// Firmware images and demo ELFs are BUILT (`make -C testdata/firmware/...`),
/// not committed, so every co-sim test has to decide whether to run or skip
/// from the same existence check.
pub fn testdata(rel: &str) -> Option<PathBuf> {
    let p = board("../../testdata").join(rel);
    p.exists().then(|| p.canonicalize().unwrap_or(p))
}

/// Step `engine` through `n` frames of `frame_dt` seconds and return the last
/// voltage it reported on the "FLAG" net (0 V if it reported none).
///
/// Every sensor co-sim reads its firmware's verdict off that one net, so the
/// stepping loop is the same whichever transport (in-process, QEMU, Renode) and
/// whichever bus (I2C, SPI) is under test.
pub fn last_flag(engine: &mut HauksbeeEngine, frame_dt: f64, n: usize) -> f64 {
    let mut last = 0.0;
    for _ in 0..n {
        if let Some(&v) = engine.step(frame_dt).net_voltages.get("FLAG") {
            last = v;
        }
    }
    last
}

/// Boot `fw` on the `testdata/boards/{demo}.kicad_pcb` MCP3008 SPI-ADC demo
/// with 5 ms co-sim chunks (each one is an emulator control round-trip).
pub fn spi_adc_engine(demo: &str, fw: &std::path::Path) -> HauksbeeEngine {
    let path = board(&format!("../../testdata/boards/{demo}.kicad_pcb"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("read SPI ADC board from {}", path.display()));
    let parsed = hauksbee_extract::ExtractedBoard::from_auto(&text).expect("parse SPI board");
    let bound =
        hauksbee_engine::binder::bind_board(&parsed, &hauksbee_models::ModelLibrary::builtin());
    let mut engine =
        HauksbeeEngine::from_bound(bound, Some(fw), &format!("/boards/{demo}.kicad_pcb"))
            .expect("build SPI ADC engine");
    engine.scheduler_mut().chunk_s = 5e-3;
    engine
}

/// Run the SPI-ADC demo for `ms` of virtual time with the MCP3008 channel-0
/// voltage fixed at `ch0_volts`, and return the final "FLAG" net voltage.
///
/// The MCP3008 has a 3.3 V Vref. The firmware's threshold is counts >= 512,
/// i.e. Vin >= 1.65 V: below it FLAG is LOW, at or above it FLAG is HIGH.
pub fn spi_adc_flag(demo: &str, fw: &std::path::Path, ch0_volts: f64, ms: u32) -> f64 {
    let mut engine = spi_adc_engine(demo, fw);
    let mut adc = hauksbee_engine::Mcp3008::new(3.3);
    adc.set_channel(0, ch0_volts);
    let bus = hauksbee_engine::SpiBus::new("U2", Box::new(adc));
    engine
        .scheduler_mut()
        .attach_spi_bus(std::sync::Arc::new(std::sync::Mutex::new(bus)), None);
    let n = usize::try_from(ms / 5).expect("frame count fits"); // 5 ms frames
    last_flag(&mut engine, 5e-3, n)
}

/// Why a FLAG stuck LOW at every input voltage points at the transport, not the
/// threshold logic: the modelled MCP3008 never answered, so the firmware read
/// 0x000 from every SPI byte (MISO undriven) and `counts == 0` always. Both
/// backends install an `on_spi` bridge, but QEMU only surfaces transactions
/// that firmware submits through the RAM-mailbox bus contract; firmware driving
/// the real (host-invisible) SPI2 controller never reaches the model.
fn spi_adc_stuck_low_hint(backend: &str) -> String {
    let transport = if backend.eq_ignore_ascii_case("qemu") {
        "Espressif QEMU never surfaces transfers on the real SPI2 controller to the host, \
         so only firmware that submits through the RAM-mailbox bus contract (or a QEMU \
         SPI hook) reaches the model: see hauksbee-mcu/src/qemu/mod.rs"
            .to_string()
    } else {
        format!(
            "check that the {backend} SPI bridge in hauksbee-mcu/src/{}/mod.rs routes this \
             controller to the attached bus",
            backend.to_lowercase()
        )
    };
    format!(
        "A FLAG stuck LOW at every input means the modelled MCP3008 never answered \
         (counts == 0 always) rather than the threshold being wrong; {transport}."
    )
}

/// 0.8 V is below the 1.65 V threshold, so FLAG must read LOW.
pub fn assert_spi_adc_flag_low_below_threshold(flag_at: impl Fn(f64) -> f64) {
    let v = flag_at(0.8);
    eprintln!("FLAG at 0.8 V input: {v:.3} V");
    assert!(
        v < 1.0,
        "at 0.8 V ADC input (below threshold), FLAG net should be LOW; got {v:.3} V. \
         FLAG HIGH here means the firmware read a count >= 512 for a 0.8 V input, so \
         its threshold logic or the modelled ADC's conversion is wrong (an ADC that \
         never answers leaves FLAG LOW and does not fail this check)."
    );
}

/// 2.5 V is above the 1.65 V threshold, so FLAG must read HIGH. `backend` names
/// the emulator whose `on_spi` bridge carries the firmware's SPI transfers.
pub fn assert_spi_adc_flag_high_above_threshold(backend: &str, flag_at: impl Fn(f64) -> f64) {
    let v = flag_at(2.5);
    eprintln!("FLAG at 2.5 V input: {v:.3} V");
    assert!(
        v > 2.5,
        "at 2.5 V ADC input (above threshold), FLAG net should be HIGH (~3.3 V); got \
         {v:.3} V. {}",
        spi_adc_stuck_low_hint(backend)
    );
}

/// Sweep the MCP3008 channel voltage across the 1.65 V (Vref/2) threshold:
/// with a working SPI bridge FLAG is LOW below it and HIGH at or above it.
/// With a no-op bridge FLAG is always LOW (the ADC reads 0 every time).
pub fn assert_spi_adc_flag_follows_sweep(backend: &str, flag_at: impl Fn(f64) -> f64) {
    let sweep: &[(f64, bool)] = &[
        (0.5, false),  // well below threshold
        (1.0, false),  // below threshold
        (1.64, false), // just below threshold
        (1.65, true),  // at threshold (counts == 512)
        (2.0, true),   // above threshold
        (3.0, true),   // near Vref
    ];
    let mut failures = Vec::new();
    for &(volts, expect_high) in sweep {
        let v = flag_at(volts);
        let got_high = v > 2.0;
        eprintln!(
            "  {volts:.2} V -> FLAG {v:.3} V ({})",
            if got_high { "HIGH" } else { "LOW" }
        );
        if got_high != expect_high {
            failures.push((volts, expect_high, v));
        }
    }
    assert!(
        failures.is_empty(),
        "FLAG did not follow the ADC voltage sweep on {backend}: {failures:?}. {}",
        spi_adc_stuck_low_hint(backend)
    );
}

/// An I2C bus "U2" carrying one LM75 at its default address, reading `temp_c`.
pub fn lm75_bus(temp_c: f64) -> std::sync::Arc<std::sync::Mutex<hauksbee_engine::I2cBus>> {
    use hauksbee_engine::{I2cBus, Lm75};
    std::sync::Arc::new(std::sync::Mutex::new(
        I2cBus::new("U2").with_slave(Box::new(Lm75::new(Lm75::DEFAULT_ADDR, temp_c))),
    ))
}

/// Boot `fw` on the KiCad `board` text with 5 ms co-sim chunks (each one is an
/// emulator control round-trip) and an [`lm75_bus`] at `temp_c` attached.
pub fn lm75_engine(
    board: &str,
    fw: &std::path::Path,
    temp_c: f64,
) -> (
    HauksbeeEngine,
    std::sync::Arc<std::sync::Mutex<hauksbee_engine::I2cBus>>,
) {
    let mut engine = HauksbeeEngine::from_board_file(board, Some(fw), "/ci")
        .expect("build I2C thermostat engine");
    engine.scheduler_mut().chunk_s = 5e-3;
    let bus = lm75_bus(temp_c);
    engine.scheduler_mut().attach_i2c_bus(bus.clone());
    (engine, bus)
}

/// Sweep the LM75 across the firmware's 30 C threshold: FLAG (read HIGH above
/// `high_above` volts) must be HIGH exactly at the temperatures >= 30 C.
pub fn assert_flag_follows_lm75_sweep(mcu: &str, high_above: f64, flag_at: impl Fn(f64) -> f64) {
    eprintln!("{mcu} I2C thermostat sweep:");
    for t in [10.0_f64, 25.0, 29.0, 31.0, 35.0, 50.0, 28.0, 15.0] {
        let v = flag_at(t);
        let (high, expect) = (v > high_above, t >= 30.0);
        let level = |h: bool| if h { "HIGH" } else { "LOW" };
        eprintln!("  {t:.1} C -> FLAG = {v:.3} V ({})", level(high));
        assert_eq!(
            high,
            expect,
            "{mcu} at {t} C: expected FLAG {}, got {v:.3} V",
            level(expect)
        );
    }
}

/// One engine, LM75 at 20 C for `frames` 5 ms frames (FLAG LOW), then raised to
/// 40 C for as many again (FLAG HIGH): fails when `backend`'s I2C bridge serves
/// bytes cached from boot instead of fresh ones on every transaction.
pub fn assert_lm75_change_is_live(
    board: &str,
    fw: &std::path::Path,
    frames: usize,
    backend: &str,
) -> HauksbeeEngine {
    use hauksbee_engine::Lm75;
    let (mut engine, bus) = lm75_engine(board, fw, 20.0);
    let ms = frames * 5;
    let cold = last_flag(&mut engine, 5e-3, frames);
    assert!(
        cold < 1.0,
        "after {ms} ms at 20 C, FLAG should be LOW; got {cold:.3} V"
    );
    if let Some(lm75) = bus.lock().unwrap().slave_mut_t::<Lm75>(Lm75::DEFAULT_ADDR) {
        lm75.set_temp_c(40.0);
    }
    let hot = last_flag(&mut engine, 5e-3, frames);
    assert!(
        hot > 2.0,
        "after raising to 40 C, FLAG should go HIGH; got {hot:.3} V. \
         This fails if the {backend} I2C bridge does not deliver updated bytes."
    );
    engine
}

/// A TP4054 in its 400 mA constant-current operating phase, programmed by the
/// published 1.66 kOhm application value: the nets and parts of a `kicad_pcb`,
/// VBAT unrouted. Unlike a regulator rating or current limit, this is an
/// explicit operating-current assertion.
pub const TP4054_CHARGER: &str = r#"
  (net 1 "VBAT")
  (net 2 "VIN")
  (net 3 "GND")
  (net 4 "PROG")
  (footprint "Package_TO_SOT_SMD:SOT-23-5" (layer "F.Cu") (at 0 0)
    (property "Reference" "U1")
    (property "Value" "TP4054")
    (pad "1" smd rect (at 0 3) (size 1 1) (layers "F.Cu"))
    (pad "2" smd rect (at 0 2) (size 1 1) (layers "F.Cu") (net 3 "GND"))
    (pad "3" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "VBAT"))
    (pad "4" smd rect (at 0 -2) (size 1 1) (layers "F.Cu") (net 2 "VIN"))
    (pad "5" smd rect (at 2 0) (size 1 1) (layers "F.Cu") (net 4 "PROG")))
  (footprint "Resistor_SMD:R_0603" (layer "F.Cu") (at 4 0)
    (property "Reference" "R1")
    (property "Value" "1.66k")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 4 "PROG"))
    (pad "2" smd rect (at 2 0) (size 1 1) (layers "F.Cu") (net 3 "GND")))"#;

/// Copper joining every pad of `crates/hauksbee-ci/examples/boards/boot_gate.kicad_pcb`
/// to its net, clear of the other nets. That example ships unrouted, which now
/// blocks ordering on its own; a test about its shorts that needs the rest of
/// the board to pass routes it with [`routed_boot_gate`].
const BOOT_GATE_ROUTES: &str = "
  (segment (start 103 100) (end 110 100) (width 0.25) (layer F.Cu) (net 3))
  (segment (start 111 100) (end 111 95) (width 0.25) (layer F.Cu) (net 4))
  (segment (start 111 95) (end 114 95) (width 0.25) (layer F.Cu) (net 4))
  (segment (start 114 95) (end 114 100) (width 0.25) (layer F.Cu) (net 4))
  (segment (start 97 101) (end 97 108) (width 0.25) (layer F.Cu) (net 1))
  (segment (start 97 108) (end 112 108) (width 0.25) (layer F.Cu) (net 1))
  (segment (start 112 108) (end 112 100) (width 0.25) (layer F.Cu) (net 1))
  (segment (start 103 102) (end 103 108) (width 0.25) (layer F.Cu) (net 1))
  (segment (start 97 100) (end 95 100) (width 0.25) (layer F.Cu) (net 2))
  (segment (start 95 100) (end 95 92) (width 0.25) (layer F.Cu) (net 2))
  (segment (start 95 92) (end 105 92) (width 0.25) (layer F.Cu) (net 2))
  (via (at 105 92) (size 0.6) (drill 0.3) (layers F.Cu B.Cu) (net 2))
  (segment (start 105 92) (end 106 92) (width 0.25) (layer B.Cu) (net 2))
  (segment (start 106 92) (end 112 92) (width 0.25) (layer B.Cu) (net 2))
  (segment (start 112 92) (end 112 100) (width 0.25) (layer B.Cu) (net 2))
  (segment (start 103 101) (end 106 101) (width 0.25) (layer F.Cu) (net 2))
  (via (at 106 101) (size 0.6) (drill 0.3) (layers F.Cu B.Cu) (net 2))
  (segment (start 106 101) (end 106 92) (width 0.25) (layer B.Cu) (net 2))
";

/// The boot-gate example `text` with [`BOOT_GATE_ROUTES`] added.
pub fn routed_boot_gate(text: &str) -> String {
    let end = text.trim_end().strip_suffix(')').expect("a closed board");
    format!("{end}{BOOT_GATE_ROUTES})\n")
}

/// A declared 2-layer stackup: 1 oz outer copper, so an ampacity verdict rests
/// on the board's own copper weight rather than an assumption.
pub const DECLARED_STACKUP: &str = r#"
  (setup (stackup
    (layer "F.Cu" (type "copper") (thickness 0.035))
    (layer "dielectric 1" (type "core") (thickness 1.51) (epsilon_r 4.5))
    (layer "B.Cu" (type "copper") (thickness 0.035))
  ))"#;

/// Run the compiled binary and wait for it to exit.
pub fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("hauksbee binary runs")
}

/// [`run`] against a board: the `run` subcommand, that board's path, then
/// `args`. Every CLI test that analyses a board spells this same prefix.
pub fn run_board(board: &std::path::Path, args: &[&str]) -> Output {
    run(&[&["run", board.to_str().unwrap()], args].concat())
}

/// Decode a captured `Output`'s stdout as UTF-8.
pub fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// Decode a captured `Output`'s stderr as UTF-8.
pub fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Assert the exit code and that stderr contains every needle.
pub fn expect_err(out: &Output, code: i32, needles: &[&str]) {
    let err = stderr(out);
    assert_eq!(out.status.code(), Some(code), "stderr: {err}");
    for n in needles {
        assert!(err.contains(n), "stderr lacks {n:?}: {err}");
    }
}

/// Assert a captured run succeeded, surfacing stderr on failure.
pub fn assert_ok(o: &Output) {
    assert!(o.status.success(), "{}", stderr(o));
}

/// Decode and concatenate a captured run's stdout and stderr, in that order.
pub fn combined(o: &Output) -> String {
    format!("{}{}", stdout(o), stderr(o))
}

/// Read and parse a JSON artifact (SARIF, report) a run wrote to `path`.
pub fn read_json(path: impl AsRef<std::path::Path>) -> serde_json::Value {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{} was written: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{} is JSON: {e}", path.display()))
}

/// The repo root, two levels up from this crate's manifest dir.
pub fn repo_root() -> PathBuf {
    hauksbee_testkit::repo_root(env!("CARGO_MANIFEST_DIR"))
}

/// Return a device without changing the caller's insertion order or `DeviceId`.
pub fn resistor(name: String, a: NodeId, b: NodeId, ohms: f64) -> Device {
    Device::resistor(name, a, b, ohms)
}

pub fn capacitor(name: String, a: NodeId, b: NodeId, farads: f64) -> Device {
    Device::capacitor(name, a, b, farads)
}

pub const fn vsource(name: String, p: NodeId, n: NodeId, kind: SourceKind) -> Device {
    Device::Vsource { name, p, n, kind }
}

/// Minimal runtime layers over `nets`, whose order is the net-name order.
pub fn bound_board(name: &str, circuit: Circuit, nets: &[(&str, NodeId)]) -> BoundBoard {
    BoundBoard {
        name: name.to_string(),
        circuit,
        net_nodes: nets.iter().map(|&(n, id)| (n.to_string(), id)).collect(),
        net_names: nets.iter().map(|&(n, _)| n.to_string()).collect(),
        ..BoundBoard::default()
    }
}

/// A board whose every chunk solve fails: two ideal voltage sources drive
/// `n1` to 5 V and 3 V at once with no series impedance between them. That
/// MNA system is structurally singular with no DC operating point, so the
/// transient's DC solve fails regardless of warm-start or gmin stepping (gmin
/// regularizes floating nodes, not conflicting hard sources). No firmware or
/// MCU is needed: the scheduler still runs the analog solve each chunk.
pub fn impossible_board() -> BoundBoard {
    let mut c = Circuit::new();
    let n1 = c.node("n1");
    c.add(vsource(
        "V1".into(),
        n1,
        NodeId::GROUND,
        SourceKind::Dc(5.0),
    ));
    c.add(vsource(
        "V2".into(),
        n1,
        NodeId::GROUND,
        SourceKind::Dc(3.0),
    ));
    bound_board("impossible", c, &[("n1", n1)])
}

/// Unpositioned fixture pin; PCB-only and passive kinds remain distinct.
pub fn pin(number: &str, net: Option<i64>, function: &str, kind: &str) -> Pin {
    Pin {
        number: number.to_string(),
        net,
        function: function.to_string(),
        kind: kind.to_string(),
        position: None,
    }
}

/// [`pin`] with no function and no kind: just a number on a net, the common
/// case for a fixture that only cares about connectivity.
pub fn netted_pin(number: &str, net: i64) -> Pin {
    pin(number, Some(net), "", "")
}

/// [`pin`] with no function, kind "passive": a resistor/capacitor-style pad.
pub fn passive_pin(number: &str, net: i64) -> Pin {
    pin(number, Some(net), "", "passive")
}

pub fn component(
    reference: String,
    value: String,
    lib_id: String,
    footprint: String,
    pins: Vec<Pin>,
) -> Component {
    Component {
        reference,
        value,
        lib_id,
        footprint,
        pins,
        ..Default::default()
    }
}

pub const fn net(id: i64, name: String) -> Net {
    Net { id, name }
}

/// A two-pin 0402 resistor-shaped [`component`], the shape both the
/// zero-ohm-jumper and zero-ohm-link fixtures build their board from: no
/// `lib_id`, footprint fixed at the real 0402 link package.
pub fn r0402(reference: &str, value: &str, pins: Vec<Pin>) -> Component {
    component(
        reference.to_string(),
        value.to_string(),
        String::new(),
        "Resistor_SMD:R_0402_1005Metric".to_string(),
        pins,
    )
}

/// The RP2040 hardware design guide's 12 MHz crystal circuit for a KiCad board
/// text: `(pads, parts)`, the XIN/XOUT pads (20 and 21) for the QFN-56 module
/// and the crystal, 15 pF load capacitors and 1 kOhm series resistor on XOUT.
///
/// The caller declares the four nets (`gnd`, `xin`, `xout`, `xtal_b`, named
/// "GND", "XIN", "XOUT" and "`XTAL_B`"). The Pico SDK's start-up code enables the
/// crystal oscillator and waits for it to report stable, which on silicon never
/// happens with nothing fitted, so firmware booted on a co-sim board only
/// starts when this circuit (or an oscillator) is there.
pub fn rp2040_crystal(gnd: u32, xin: u32, xout: u32, xtal_b: u32) -> (String, String) {
    let pads = format!(
        "(pad 20 smd rect (at -3 7) (net {xin} \"XIN\"))\n    \
         (pad 21 smd rect (at -3 8) (net {xout} \"XOUT\"))"
    );
    let parts = format!(
        r#"
  (module Crystal:Crystal_SMD_3225-4Pin (layer F.Cu)
    (at 105 105)
    (fp_text reference Y1 (at 0 0) (layer F.SilkS))
    (fp_text value 12MHz (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net {xin} "XIN"))
    (pad 2 smd rect (at 1 0) (net {gnd} "GND"))
    (pad 3 smd rect (at 2 0) (net {xtal_b} "XTAL_B"))
    (pad 4 smd rect (at 3 0) (net {gnd} "GND"))
  )
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 105 110)
    (fp_text reference R20 (at 0 0) (layer F.SilkS))
    (fp_text value 1k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net {xout} "XOUT"))
    (pad 2 smd rect (at 2 0) (net {xtal_b} "XTAL_B"))
  )
  (module Capacitor_SMD:C_0603_1608Metric (layer F.Cu)
    (at 105 115)
    (fp_text reference C20 (at 0 0) (layer F.SilkS))
    (fp_text value 15pF (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net {xin} "XIN"))
    (pad 2 smd rect (at 2 0) (net {gnd} "GND"))
  )
  (module Capacitor_SMD:C_0603_1608Metric (layer F.Cu)
    (at 105 120)
    (fp_text reference C21 (at 0 0) (layer F.SilkS))
    (fp_text value 15pF (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net {xtal_b} "XTAL_B"))
    (pad 2 smd rect (at 2 0) (net {gnd} "GND"))
  )
"#
    );
    (pads, parts)
}
