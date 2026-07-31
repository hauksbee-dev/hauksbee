//! Real SPI chip-select framing (05 §2, gate `cosim_spi_cs_frames_transactions`
//! in 08-validation-and-test-campaign.md §2).
//!
//! The pre-05-§2 co-sim framed SPI transactions on the CHUNK BOUNDARY: it reset
//! every slave's command state machine once per analog chunk, standing in for a
//! real chip-select edge that simavr's SPI IRQ never surfaces. That heuristic was
//! wrong in two documented ways, and these tests pin both of them.
//!
//! The fix frames transactions on the REAL CS edges. The scheduler drives the
//! byte/edge application paths that this test exercises directly:
//!   - a CS falling edge  -> `SpiBus::cs_assert`   (begin a transaction),
//!   - a byte transfer     -> `SpiBus::transfer`    (one MOSI/MISO exchange),
//!   - a CS rising edge    -> `SpiBus::cs_deassert` (end the transaction),
//! interleaved in cycle order by the `on_pin_change` + `on_spi` closures. When a
//! bus has a resolved CS pin (or a backend that surfaces CS itself), it
//! `frames_itself()` and the scheduler SKIPS the chunk-boundary deselect, so a
//! boundary-spanning transaction is no longer truncated.

use hauksbee_engine::{Mcp3008, Spi25Eeprom, SpiBus, SpiFramingMode, SpiSlave};

/// Decode the common 3-byte MCP3008 single-ended read against a bus, framing the
/// transaction with real CS edges.
fn framed_mcp3008_read(bus: &mut SpiBus, channel: u8) -> u16 {
    bus.cs_assert(); // CS falling edge: begin
    let _b0 = bus.transfer(0x01); // start bit
    let hi = bus.transfer(channel << 4); // SGL=0 here is fine; channel in bits 6..4
    let lo = bus.transfer(0x00);
    bus.cs_deassert(); // CS rising edge: end
    (((hi & 0x03) as u16) << 8) | lo as u16
}

/// FAILURE MODE (a): two CS-framed transactions inside ONE chunk must be framed
/// SEPARATELY. A chunk-boundary heuristic resets only once per chunk, so a
/// second transaction's bytes append to the first slave's sequence counter and
/// decode against stale state. Real CS edges separate them.
#[test]
fn two_transactions_one_chunk_are_framed_separately() {
    let mut adc = Mcp3008::new(5.0);
    adc.set_channel(3, 2.5); // ~512 counts (half scale)
    adc.set_channel(1, 1.25); // ~256 counts (quarter scale)
    let mut bus = SpiBus::new("U_ADC", Box::new(adc));

    // Two transactions, both inside the SAME chunk, each bounded by its own CS
    // falling/rising edge. With real framing the second read starts fresh.
    let first = framed_mcp3008_read(&mut bus, 3);
    let second = framed_mcp3008_read(&mut bus, 1);

    assert!(
        (first as i32 - 512).abs() <= 2,
        "first transaction reads channel 3 (~512), got {first}"
    );
    assert!(
        (second as i32 - 256).abs() <= 2,
        "second transaction, framed separately, reads channel 1 (~256), got {second}"
    );

    // Contrast: the OLD heuristic never reset between the two transactions (no
    // per-CS-edge deselect, only one reset at the chunk boundary). Replaying the
    // same six bytes with NO intervening frame keeps the sequence counter running
    // past byte 2, so the second "transaction" is misdecoded, the bug this fixes.
    let mut merged_adc = Mcp3008::new(5.0);
    merged_adc.set_channel(3, 2.5);
    merged_adc.set_channel(1, 1.25);
    let mut merged = SpiBus::new("U_ADC", Box::new(merged_adc));
    let _ = merged.transfer(0x01);
    let _ = merged.transfer(3 << 4);
    let _ = merged.transfer(0x00);
    // No cs_deassert/cs_assert here: the six bytes run as one merged stream.
    let mhi = merged.transfer(0x01); // seq 3 -> treated as a low-byte, not a start
    let mlo = merged.transfer(1 << 4);
    let merged_second = (((mhi & 0x03) as u16) << 8) | mlo as u16;
    assert_ne!(
        merged_second, second,
        "the merged (unframed) path must MISDECODE the second read; that is the \
         two-transactions-per-chunk bug the CS framing fixes"
    );
}

/// FAILURE MODE (b): a transaction that SPANS a chunk boundary must NOT be reset
/// mid-way. On a bus that frames itself (resolved CS pin), the scheduler skips the
/// chunk-boundary deselect, so the command state machine survives the boundary and
/// the reply completes intact. This is exactly where a chunk-boundary heuristic
/// truncates instead.
#[test]
fn transaction_spanning_a_chunk_boundary_is_not_reset() {
    // Seed an EEPROM with two bytes at address 0.
    let mut eeprom = Spi25Eeprom::new(256);
    // Write "OK" at 0x0000 via a framed WREN + WRITE transaction.
    {
        // WREN in its own transaction.
        eeprom.select();
        assert_eq!(eeprom.transfer(0x06), 0xFF); // WREN
        eeprom.deselect();
        // WRITE "OK" at 0x0000.
        eeprom.select();
        eeprom.transfer(0x02); // WRITE
        eeprom.transfer(0x00);
        eeprom.transfer(0x00);
        eeprom.transfer(b'O');
        eeprom.transfer(b'K');
        eeprom.deselect();
    }

    let mut bus = SpiBus::new("U_EE", Box::new(eeprom));
    // Resolve a CS pin: the bus now frames itself, so the scheduler must NOT
    // deselect it at the chunk boundary.
    bus.set_cs_pin(Some(('B', 2)));
    assert!(
        bus.frames_itself(),
        "a resolved CS pin means the bus frames itself"
    );
    assert_eq!(bus.framing_mode(), SpiFramingMode::Exact);

    // Begin a READ transaction and clock the first two bytes (instruction + high
    // address). This is the state a chunk boundary can fall into.
    bus.cs_assert();
    bus.transfer(0x03); // READ
    bus.transfer(0x00); // addr hi

    // --- CHUNK BOUNDARY falls here ---
    // The scheduler's step 3c skips `slave_deselect` for a bus that frames itself
    // (05 §2, failure mode b). So we do NOT deselect: the transaction survives.
    assert!(
        !simulate_chunk_boundary_deselect(&mut bus),
        "a self-framing bus is skipped by the chunk-boundary heuristic"
    );

    // Continue the SAME transaction in the next chunk: low address byte, then read.
    bus.transfer(0x00); // addr lo -> address 0x0000 latched
    let o = bus.transfer(0x00);
    let k = bus.transfer(0x00);
    bus.cs_deassert();
    assert_eq!(
        [o, k],
        [b'O', b'K'],
        "the boundary-spanning READ returns 'OK' intact"
    );

    // Contrast: a HEURISTIC bus (no CS pin) is NOT skipped, so the boundary
    // deselect truncates the transaction and the read is corrupted.
    let mut eeprom2 = Spi25Eeprom::new(256);
    eeprom2.select();
    eeprom2.transfer(0x06);
    eeprom2.deselect();
    eeprom2.select();
    eeprom2.transfer(0x02);
    eeprom2.transfer(0x00);
    eeprom2.transfer(0x00);
    eeprom2.transfer(b'O');
    eeprom2.transfer(b'K');
    eeprom2.deselect();
    let mut heuristic = SpiBus::new("U_EE", Box::new(eeprom2));
    assert!(!heuristic.frames_itself());
    assert_eq!(heuristic.framing_mode(), SpiFramingMode::Heuristic);
    heuristic.transfer(0x03); // READ
    heuristic.transfer(0x00); // addr hi
                              // Boundary: the heuristic path DOES deselect, truncating the transaction.
    assert!(
        simulate_chunk_boundary_deselect(&mut heuristic),
        "a heuristic bus is deselected at the chunk boundary"
    );
    heuristic.transfer(0x00); // addr lo, but the command was reset: re-interpreted
    let bogus_o = heuristic.transfer(0x00);
    assert_ne!(
        bogus_o, b'O',
        "the heuristic boundary reset truncates the READ, the bug the fix removes"
    );
}

/// FRAMING MODE COVERAGE: the tier reported per slave drives the co-sim JSON
/// `spi_framing` field, and (critically) `frames_itself` is the exact condition
/// the scheduler's step 3c gates the chunk-boundary deselect on.
#[test]
fn framing_mode_reflects_the_cs_source() {
    // No CS resolved: heuristic.
    let mut bus = SpiBus::new("U", Box::new(Mcp3008::new(5.0)));
    assert_eq!(bus.framing_mode(), SpiFramingMode::Heuristic);
    assert!(!bus.frames_itself());
    assert_eq!(bus.framing_mode().as_str(), "heuristic");

    // Resolved CS pin: exact.
    bus.set_cs_pin(Some(('D', 4)));
    assert_eq!(bus.framing_mode(), SpiFramingMode::Exact);
    assert!(bus.frames_itself());
    assert_eq!(bus.framing_mode().as_str(), "exact");

    // A backend that surfaces CS itself wins even without a resolved pin.
    let mut backend_bus = SpiBus::new("U2", Box::new(Mcp3008::new(5.0)));
    backend_bus.note_backend_deselect();
    assert_eq!(backend_bus.framing_mode(), SpiFramingMode::Backend);
    assert!(backend_bus.frames_itself());
    assert_eq!(backend_bus.framing_mode().as_str(), "backend");
}

/// Model the scheduler's step 3c decision for one bus: it deselects (and would
/// have warned) ONLY when the bus does not frame itself. Returns whether a
/// deselect was applied, so the tests can assert the skip/apply split.
fn simulate_chunk_boundary_deselect(bus: &mut SpiBus) -> bool {
    if bus.frames_itself() {
        return false;
    }
    bus.slave_deselect();
    true
}
