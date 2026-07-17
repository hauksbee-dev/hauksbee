//! Engine-level proof of per-controller SPI routing.
//!
//! These tests verify that `attach_spi_bus_on("spi2", ...)` and
//! `attach_spi_bus_on("spi3", ...)` route to DIFFERENT slaves -- cross-talk
//! would return the wrong slave's response. The tests exercise the real
//! per-controller routing in the scheduler, not a stub: the scheduler's
//! `spi_controller_map` is populated and the per-controller lookup returns the
//! correct bus with no cross-talk.
//!
//! No MCU, no Renode, no firmware required: these tests verify the dispatcher
//! contract in isolation. The Renode integration (firmware driving two SPI buses
//! to different sensors on spi2/spi3) is gated on `#[cfg(feature = "renode")]`
//! in `spi_sensor_cosim_renode.rs` and requires a real Renode install.

use std::sync::{Arc, Mutex};

use hauksbee_engine::peripherals::SpiBus;
use hauksbee_engine::peripherals::SpiSlave;

// ─────────────────────────────────────────────────────────────────────────────
// Minimal SpiSlave implementation for tests: returns a fixed WHO_AM_I byte.
// ─────────────────────────────────────────────────────────────────────────────

struct WhoAmISlave {
    who_am_i: u8,
    /// True when we have received the WHO_AM_I command byte (0x75) and the
    /// next transfer should return the identity byte.
    pending: bool,
}

impl WhoAmISlave {
    fn new(who_am_i: u8) -> Self {
        WhoAmISlave { who_am_i, pending: false }
    }
}

impl SpiSlave for WhoAmISlave {
    fn transfer(&mut self, mosi: u8) -> u8 {
        if self.pending {
            self.pending = false;
            self.who_am_i
        } else if mosi == 0x75 {
            // WHO_AM_I read command (ICM-42605 style: 0x75 | 0x80 for read)
            self.pending = true;
            0xFF
        } else if mosi == (0x75 | 0x80) {
            // Alternate: combined read-register byte
            self.pending = true;
            0xFF
        } else {
            0xFF
        }
    }

    fn deselect(&mut self) {
        self.pending = false;
    }

    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// Build a minimal Scheduler with no MCUs (firmware-less). The circuit is
/// empty (no board) so binding and analog-solving are no-ops. We only need the
/// scheduler to hold the SPI bus map and the chunk-boundary deselect loop.
fn empty_scheduler() -> hauksbee_engine::scheduler::Scheduler {
    use hauksbee_engine::bind_board;
    use hauksbee_extract::ExtractedBoard;
    use hauksbee_models::ModelLibrary;
    use hauksbee_solve::SolverOptions;

    // A minimal KiCad netlist with no components: just the GND net.
    let netlist = r#"(export (version "E") (nets (net (code "0") (name "GND"))))"#;
    let board = ExtractedBoard::from_auto(netlist).expect("minimal board");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);
    hauksbee_engine::scheduler::Scheduler::new(bound, None, SolverOptions::default())
        .expect("scheduler from empty board")
}

#[test]
fn per_controller_spi_routing_no_crosstalk() {
    // Sensor A on spi2: WHO_AM_I = 0xAA (ICM-42605 clone stand-in)
    // Sensor B on spi3: WHO_AM_I = 0xBB (BMP280 stand-in)
    let bus_aa = Arc::new(Mutex::new(SpiBus::new(
        "ICM",
        Box::new(WhoAmISlave::new(0xAA)),
    )));
    let bus_bb = Arc::new(Mutex::new(SpiBus::new(
        "BMP",
        Box::new(WhoAmISlave::new(0xBB)),
    )));

    let mut sched = empty_scheduler();
    sched.attach_spi_bus_on("spi2", bus_aa.clone(), None, None);
    sched.attach_spi_bus_on("spi3", bus_bb.clone(), None, None);

    // Verify the controller map lookup: spi2 -> bus_aa, spi3 -> bus_bb.
    // Arc::ptr_eq checks physical identity, not just value equality, so a
    // cross-talk defect (spi3 returning bus_aa) would fail here.
    let got_spi2 = sched.spi_bus_for_controller("spi2").expect("spi2 registered");
    let got_spi3 = sched.spi_bus_for_controller("spi3").expect("spi3 registered");

    assert!(
        Arc::ptr_eq(got_spi2, &bus_aa),
        "spi2 should resolve to bus_aa (ICM); got a different Arc"
    );
    assert!(
        Arc::ptr_eq(got_spi3, &bus_bb),
        "spi3 should resolve to bus_bb (BMP); got a different Arc"
    );
    assert!(
        !Arc::ptr_eq(got_spi2, got_spi3),
        "spi2 and spi3 must NOT resolve to the same bus (cross-talk)"
    );

    // Drive the WHO_AM_I read sequence on each bus and verify the response.
    // Command byte 0x75 puts the slave into "pending" state; the following
    // 0x00 dummy byte returns the identity value.
    let miso_aa_1 = bus_aa.lock().unwrap().transfer(0x75);  // send command
    let miso_aa_2 = bus_aa.lock().unwrap().transfer(0x00);  // read response
    assert_eq!(miso_aa_1, 0xFF, "command byte should return 0xFF (bus lines idle)");
    assert_eq!(miso_aa_2, 0xAA, "spi2 slave (ICM) WHO_AM_I should be 0xAA");

    let miso_bb_1 = bus_bb.lock().unwrap().transfer(0x75);
    let miso_bb_2 = bus_bb.lock().unwrap().transfer(0x00);
    assert_eq!(miso_bb_1, 0xFF);
    assert_eq!(miso_bb_2, 0xBB, "spi3 slave (BMP) WHO_AM_I should be 0xBB");

    // Cross-talk check: driving spi2's bus gives 0xAA, NOT 0xBB.
    bus_aa.lock().unwrap().slave_deselect();
    bus_aa.lock().unwrap().transfer(0x75);
    let cross = bus_aa.lock().unwrap().transfer(0x00);
    assert_ne!(cross, 0xBB, "spi2 bus returned spi3's 0xBB -- cross-talk detected!");
    assert_eq!(cross, 0xAA, "spi2 should still return 0xAA after deselect+re-read");

    // spi_buses() must contain both (for chunk-boundary deselects).
    assert_eq!(sched.spi_buses().len(), 2, "both buses must be in spi_buses()");
}

#[test]
fn attach_spi_bus_on_coexists_with_legacy_attach_spi_bus() {
    // A bus attached via the legacy path (no controller name) and another via
    // the explicit-controller path must both appear in spi_buses() so the
    // chunk-boundary deselect loop reaches both.
    let legacy_bus = Arc::new(Mutex::new(SpiBus::new(
        "LEGACY",
        Box::new(WhoAmISlave::new(0x11)),
    )));
    let named_bus = Arc::new(Mutex::new(SpiBus::new(
        "NAMED",
        Box::new(WhoAmISlave::new(0x22)),
    )));

    let mut sched = empty_scheduler();
    sched.attach_spi_bus(legacy_bus.clone(), None, None);
    sched.attach_spi_bus_on("spi2", named_bus.clone(), None, None);

    // Both show up in the flat spi_buses() slice.
    assert_eq!(sched.spi_buses().len(), 2);

    // The named one is findable by controller name.
    let found = sched.spi_bus_for_controller("spi2").expect("spi2 registered");
    assert!(Arc::ptr_eq(found, &named_bus));

    // The legacy one is NOT findable by any controller name (no key inserted).
    assert!(sched.spi_bus_for_controller("spi1").is_none(), "legacy bus has no controller key");
}
