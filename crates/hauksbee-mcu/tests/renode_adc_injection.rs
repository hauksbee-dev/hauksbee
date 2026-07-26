//! Regression fixture for Renode ADC injection (05-cosim-fidelity §5.1).
//!
//! `Mcu::set_analog_in` used to be a documented no-op on the Renode backend:
//! the scheduler pushed modeled ADC voltages every chunk and they were dropped
//! on the floor for every non-AVR part. This fixture FAILS on that behaviour
//! and passes with the Monitor/RAM injection path:
//!
//!   1. The test configures an `AdcChannelMap` that delivers channel 0's count
//!      into an SRAM result word (`0x2000_4000`) via `sysbus WriteDoubleWord`,
//!      the same Monitor TCP channel the backend's ODR diffing rides.
//!   2. The firmware is a REAL Cortex-M3 program executed by Renode: a
//!      hand-assembled Thumb loop (built byte-by-byte below, because this repo
//!      cannot assume an ARM cross-toolchain) that first configures every
//!      GPIOC pin as an output (CRL/CRH = 0x33333333, GP push-pull 50 MHz,
//!      required since the direction-aware ODR poll only reports
//!      configured-output pins as driven, exactly like real hardware only
//!      drives configured outputs), then reads the ADC result word and copies
//!      it to GPIOC ODR forever:
//!
//!      ```text
//!      ldr r0, =0x20004000   ; ADC result word (the injection target)
//!      ldr r1, =0x4001100C   ; GPIOC ODR
//!      ldr r2, =0x40011000   ; GPIOC CRL
//!      ldr r3, =0x33333333   ; all pins: GP push-pull output, 50 MHz
//!      str r3, [r2]          ; CRL: pins 0-7 output
//!      str r3, [r2, #4]      ; CRH: pins 8-15 output
//!      loop: ldr r2, [r0]
//!            str r2, [r1]
//!            b loop
//!      ```
//!
//!   3. The test then observes the injected count coming back OUT of the guest
//!      through the standard `on_pin_change` ODR poll, i.e. the count is
//!      firmware-visible in the strongest sense: firmware instructions read it
//!      and act on it, and the co-sim sees the consequence on pins.
//!
//! Before the fix: no injection happens, the result word stays 0, the firmware
//! copies 0 to ODR, no edges fire, the reconstructed word is 0 → FAIL.
//!
//! Skips gracefully when Renode is not installed (same pattern as
//! `renode_stm32.rs`), printing the reason.

#![cfg(feature = "renode")]

use hauksbee_mcu::renode::is_available;
use hauksbee_mcu::{AdcChannelMap, AdcInject, Mcu, RenodeBackend, RenodeConfig};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// SRAM word the injected count is written to (STM32F103 SRAM, well clear of
/// anything the tiny firmware uses; it has no stack and no data).
const ADC_RESULT_WORD: u32 = 0x2000_4000;

/// Build the tiny Cortex-M3 firmware as a loadable ELF32 image, in memory.
///
/// Layout at 0x0800_0000 (the STM32F103 flash base, matching the repo's
/// `stm32_blinky` linker script):
///   +0x00  initial SP        = 0x2000_2000 (unused: the loop needs no stack)
///   +0x04  reset vector      = 0x0800_0009 (code below, Thumb bit set)
///   +0x08  4804      ldr r0, [pc, #16]   ; = 0x20004000  (literal at +0x1C)
///   +0x0A  4905      ldr r1, [pc, #20]   ; = 0x4001100C  (literal at +0x20)
///   +0x0C  4A05      ldr r2, [pc, #20]   ; = 0x40011000  (literal at +0x24)
///   +0x0E  4B06      ldr r3, [pc, #24]   ; = 0x33333333  (literal at +0x28)
///   +0x10  6013      str r3, [r2]        ; CRL: pins 0-7 GP output
///   +0x12  6053      str r3, [r2, #4]    ; CRH: pins 8-15 GP output
///   +0x14  6802      ldr r2, [r0]        ; loop:
///   +0x16  600A      str r2, [r1]
///   +0x18  E7FC      b   loop            ; (-8 from PC)
///   +0x1A  BF00      nop                 ; literal-pool alignment
///   +0x1C  0x2000_4000
///   +0x20  0x4001_100C
///   +0x24  0x4001_1000
///   +0x28  0x3333_3333
fn build_adc_copy_elf() -> Vec<u8> {
    const LOAD_ADDR: u32 = 0x0800_0000;
    const GPIOC_ODR: u32 = 0x4001_100C;
    const GPIOC_CRL: u32 = 0x4001_1000;

    let mut payload: Vec<u8> = Vec::new();
    let w = |v: u32| v.to_le_bytes();
    let h = |v: u16| v.to_le_bytes();
    payload.extend_from_slice(&w(0x2000_2000)); // initial SP
    payload.extend_from_slice(&w(LOAD_ADDR + 0x08 + 1)); // reset (Thumb)
    payload.extend_from_slice(&h(0x4804)); // ldr r0, [pc, #16]
    payload.extend_from_slice(&h(0x4905)); // ldr r1, [pc, #20]
    payload.extend_from_slice(&h(0x4A05)); // ldr r2, [pc, #20]
    payload.extend_from_slice(&h(0x4B06)); // ldr r3, [pc, #24]
    payload.extend_from_slice(&h(0x6013)); // str r3, [r2]
    payload.extend_from_slice(&h(0x6053)); // str r3, [r2, #4]
    payload.extend_from_slice(&h(0x6802)); // ldr r2, [r0]
    payload.extend_from_slice(&h(0x600A)); // str r2, [r1]
    payload.extend_from_slice(&h(0xE7FC)); // b loop
    payload.extend_from_slice(&h(0xBF00)); // nop (align pool)
    payload.extend_from_slice(&w(ADC_RESULT_WORD));
    payload.extend_from_slice(&w(GPIOC_ODR));
    payload.extend_from_slice(&w(GPIOC_CRL));
    payload.extend_from_slice(&w(0x3333_3333));
    assert_eq!(payload.len(), 0x2C);

    // ELF32 header (52 bytes) + one PT_LOAD program header (32 bytes).
    let e_entry: u32 = LOAD_ADDR + 0x08 + 1;
    let phoff: u32 = 52;
    let payload_off: u32 = 52 + 32;
    let mut elf: Vec<u8> = Vec::new();
    elf.extend_from_slice(&[0x7F, b'E', b'L', b'F', 1, 1, 1, 0]); // ELF32, LE
    elf.extend_from_slice(&[0; 8]); // EI_PAD
    elf.extend_from_slice(&2u16.to_le_bytes()); // e_type = EXEC
    elf.extend_from_slice(&40u16.to_le_bytes()); // e_machine = EM_ARM
    elf.extend_from_slice(&1u32.to_le_bytes()); // e_version
    elf.extend_from_slice(&e_entry.to_le_bytes());
    elf.extend_from_slice(&phoff.to_le_bytes()); // e_phoff
    elf.extend_from_slice(&0u32.to_le_bytes()); // e_shoff
    elf.extend_from_slice(&0x0500_0000u32.to_le_bytes()); // e_flags: EABI v5
    elf.extend_from_slice(&52u16.to_le_bytes()); // e_ehsize
    elf.extend_from_slice(&32u16.to_le_bytes()); // e_phentsize
    elf.extend_from_slice(&1u16.to_le_bytes()); // e_phnum
    elf.extend_from_slice(&40u16.to_le_bytes()); // e_shentsize
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shnum
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shstrndx
    assert_eq!(elf.len(), 52);
    // PT_LOAD
    elf.extend_from_slice(&1u32.to_le_bytes()); // p_type
    elf.extend_from_slice(&payload_off.to_le_bytes()); // p_offset
    elf.extend_from_slice(&LOAD_ADDR.to_le_bytes()); // p_vaddr
    elf.extend_from_slice(&LOAD_ADDR.to_le_bytes()); // p_paddr
    elf.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // p_filesz
    elf.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // p_memsz
    elf.extend_from_slice(&5u32.to_le_bytes()); // p_flags = R+X
    elf.extend_from_slice(&4u32.to_le_bytes()); // p_align
    assert_eq!(elf.len(), 84);
    elf.extend_from_slice(&payload);
    elf
}

/// Write the firmware ELF to a unique temp path.
fn write_elf_to_temp() -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "hauksbee-renode-adc-fixture-{}-{}.elf",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&p, build_adc_copy_elf()).expect("write fixture ELF");
    p
}

/// Reconstruct the 16-bit ODR word of one port from accumulated pin levels.
fn word_of(levels: &HashMap<(char, u8), bool>, port: char) -> u32 {
    let mut w = 0u32;
    for (&(p, bit), &high) in levels {
        if p == port && high {
            w |= 1 << bit;
        }
    }
    w
}

/// Expected count for `volts` against the map below (3.3 V, 12-bit).
fn expected_count(volts: f64) -> u32 {
    ((volts / 3.3).clamp(0.0, 1.0) * 4095.0).round() as u32
}

#[test]
fn renode_adc_injection_reaches_firmware() {
    if !is_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }

    let mut config = RenodeConfig::stm32f103().with_adc_channel(AdcChannelMap {
        channel: 0,
        inject: AdcInject::MemoryWord(ADC_RESULT_WORD),
        full_scale_volts: 3.3,
        max_count: 4095,
    });
    // The hand-built ELF carries no section headers/symbols, so Renode's
    // CortexM cannot locate the vector table on its own (a toolchain ELF like
    // blinky.elf needs no such help). Point it at the table explicitly; the
    // CPU then loads SP/PC from it at start, exactly like real boot.
    config
        .post_load_setup
        .push("{cpu} VectorTableOffset 0x08000000".to_string());
    let mut mcu = RenodeBackend::new(config).expect("spawn Renode STM32F103");

    let elf = write_elf_to_temp();
    mcu.load_firmware(&elf).expect("load hand-assembled fixture ELF");

    // Accumulate firmware-driven pin levels from the standard ODR-poll edges.
    let levels: Arc<Mutex<HashMap<(char, u8), bool>>> = Arc::new(Mutex::new(HashMap::new()));
    let sink = levels.clone();
    mcu.on_pin_change(Box::new(move |pin, high, _cycle| {
        sink.lock().unwrap().insert((pin.port, pin.bit), high);
    }));

    // Inject 2.0 V on channel 0 and let the firmware run. The scheduler's
    // contract is one set_analog_in per chunk, so mirror that here.
    let v1 = 2.0;
    for _ in 0..4 {
        mcu.set_analog_in(0, v1);
        mcu.run_micros(20_000).expect("run chunk");
    }
    let got1 = word_of(&levels.lock().unwrap(), 'C');
    assert_eq!(
        got1,
        expected_count(v1),
        "firmware should copy the injected ADC count (2.0 V → 0x{:03X}) to GPIOC ODR; \
         got 0x{got1:03X}. A zero word here is the pre-fix no-op set_analog_in.",
        expected_count(v1)
    );

    // The injection is live: a new voltage must be visible on the next chunks.
    let v2 = 0.4;
    for _ in 0..4 {
        mcu.set_analog_in(0, v2);
        mcu.run_micros(20_000).expect("run chunk");
    }
    let got2 = word_of(&levels.lock().unwrap(), 'C');
    assert_eq!(
        got2,
        expected_count(v2),
        "changing the injected voltage (0.4 V → 0x{:03X}) must update the \
         firmware-visible count; got 0x{got2:03X}",
        expected_count(v2)
    );

    // An unmapped channel is a LOUD drop (stderr warning), never a panic and
    // never a fake write.
    mcu.set_analog_in(5, 1.0);

    std::fs::remove_file(&elf).ok();
}
