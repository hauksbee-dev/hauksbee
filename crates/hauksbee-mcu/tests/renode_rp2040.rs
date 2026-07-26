//! Integration smoke for the RP2040 Renode config (05-cosim-fidelity §5.4).
//!
//! Boots the `rp2040` platform in REAL Renode and asserts that GPIO output
//! diffing works: a hand-assembled Cortex-M0(+) image enables an output on the
//! SIO and drives it high, and the backend's standard ODR-poll (pointed at SIO
//! `GPIO_OUT`, offset 0x10) synthesises the resulting `on_pin_change` edge.
//!
//! # Why this is (honestly) skip-gated
//!
//! The Renode build installed on this machine (portable **v1.16.1**) ships **no
//! rp2040 platform**, `platforms/cpus/` carries only `picosoc`/`litex_picorv32`
//! (unrelated RISC-V soft cores). Per 05 §5.4's gate, the smoke checks for the
//! platform `.repl` first and SKIPS LOUDLY with the reason when it is absent,
//! rather than failing. When run against a Renode that carries the platform, it
//! becomes a real boot test and confirms (or corrects) the two things the config
//! could not verify offline: the SIO peripheral's name and whether Renode's SIO
//! model reads `GPIO_OUT` back as the driven value.
//!
//! # Cortex-M0(+) legality
//!
//! RP2040 is Cortex-M0+, which executes the Thumb(-1) subset. The hand-assembled
//! image below uses only `LDR (literal)` (T1), `STR (immediate)` (T1), and `B`
//! (T2, unconditional), all present on M0/M0+; the same instruction family the
//! `renode_adc_injection.rs` M3 fixture uses. No Thumb-2-only encoding appears.

#![cfg(feature = "renode")]

use hauksbee_mcu::renode::{find_renode, is_available};
use hauksbee_mcu::{Mcu, RenodeBackend, RenodeConfig};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The GPIO the firmware drives high. Bit 25 is the Raspberry Pi Pico's onboard
/// LED, a natural choice, and well inside the 0..29 range.
const DRIVEN_GPIO: u8 = 25;

/// RP2040 SIO GPIO registers (datasheet §2.3.1.7), absolute addresses.
const SIO_GPIO_OUT: u32 = 0xD000_0010;
const SIO_GPIO_OE: u32 = 0xD000_0020;

/// SRAM base the image is loaded at (RP2040 SRAM is 264 KiB from 0x2000_0000).
const LOAD_ADDR: u32 = 0x2000_0000;

/// Build a tiny Cortex-M0(+) ELF32 that enables SIO output on `DRIVEN_GPIO` and
/// drives it high forever. Layout at `LOAD_ADDR`:
///
/// ```text
///   +0x00  initial SP    = 0x2000_2000  (unused: the loop needs no stack)
///   +0x04  reset vector  = LOAD_ADDR + 0x08 + 1  (Thumb bit set)
///   +0x08  4802  ldr r0, [pc, #8]    ; r0 = SIO_GPIO_OE   (literal @ +0x14)
///   +0x0A  4903  ldr r1, [pc, #12]   ; r1 = 1<<DRIVEN_GPIO (literal @ +0x18)
///   +0x0C  6001  str r1, [r0]        ; GPIO_OE |= mask  (enable the output)
///   +0x0E  4803  ldr r0, [pc, #12]   ; r0 = SIO_GPIO_OUT  (literal @ +0x1C)
///   +0x10  6001  str r1, [r0]        ; loop: GPIO_OUT = mask (bit high)
///   +0x12  E7FD  b   loop            ; (-6 from PC → back to +0x10)
///   +0x14  0xD000_0020               ; SIO_GPIO_OE
///   +0x18  0x0200_0000               ; 1 << 25
///   +0x1C  0xD000_0010               ; SIO_GPIO_OUT
/// ```
fn build_gpio_drive_elf() -> Vec<u8> {
    let mask: u32 = 1 << DRIVEN_GPIO;

    let mut payload: Vec<u8> = Vec::new();
    let w = |v: u32| v.to_le_bytes();
    let h = |v: u16| v.to_le_bytes();
    payload.extend_from_slice(&w(0x2000_2000)); // initial SP
    payload.extend_from_slice(&w(LOAD_ADDR + 0x08 + 1)); // reset (Thumb)
    payload.extend_from_slice(&h(0x4802)); // ldr r0, [pc, #8]   -> SIO_GPIO_OE
    payload.extend_from_slice(&h(0x4903)); // ldr r1, [pc, #12]  -> mask
    payload.extend_from_slice(&h(0x6001)); // str r1, [r0]       (GPIO_OE = mask)
    payload.extend_from_slice(&h(0x4803)); // ldr r0, [pc, #12]  -> SIO_GPIO_OUT
    payload.extend_from_slice(&h(0x6001)); // loop: str r1, [r0] (GPIO_OUT = mask)
    payload.extend_from_slice(&h(0xE7FD)); // b loop
    payload.extend_from_slice(&w(SIO_GPIO_OE));
    payload.extend_from_slice(&w(mask));
    payload.extend_from_slice(&w(SIO_GPIO_OUT));
    assert_eq!(payload.len(), 0x20);

    // ELF32 header (52 bytes) + one PT_LOAD program header (32 bytes), same
    // wrapper as the ADC fixture (a symbol-less loadable image).
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

fn write_elf_to_temp() -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "hauksbee-renode-rp2040-fixture-{}-{}.elf",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&p, build_gpio_drive_elf()).expect("write fixture ELF");
    p
}

/// Locate an rp2040 platform `.repl` inside the installed Renode, if any. The
/// platforms live beside the `renode` binary (`<root>/platforms/...`).
fn rp2040_platform_present() -> bool {
    let Ok(bin) = find_renode() else {
        return false;
    };
    let Some(root) = bin.parent() else {
        return false;
    };
    let candidates = [
        root.join("platforms/cpus/rp2040.repl"),
        root.join("platforms/boards/raspberry_pi_pico.repl"),
        root.join("platforms/boards/rpi_pico.repl"),
    ];
    candidates.iter().any(|p| p.exists())
}

#[test]
fn rp2040_gpio_output_diffing_works() {
    if !is_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }
    if !rp2040_platform_present() {
        eprintln!(
            "SKIP: the installed Renode ships no rp2040 platform \
             (checked platforms/cpus/rp2040.repl and the Pico board repls). \
             This is the honest skip-gate of 05 §5.4: the RP2040 RenodeConfig is \
             shipped and unit-tested, but a real boot needs a Renode build that \
             carries the rp2040 platform description."
        );
        return;
    }

    let mut config = RenodeConfig::rp2040();
    // The hand-built ELF has no symbols, so Renode's Cortex-M cannot find the
    // vector table on its own; point it at the loaded table so SP/PC load from
    // it at start, exactly like the ADC fixture does for the M3.
    config
        .post_load_setup
        .push(format!("{{cpu}} VectorTableOffset 0x{LOAD_ADDR:08X}"));
    let mut mcu = RenodeBackend::new(config).expect("spawn Renode rp2040");

    let elf = write_elf_to_temp();
    mcu.load_firmware(&elf)
        .expect("load hand-assembled M0 fixture ELF");

    let levels: Arc<Mutex<HashMap<(char, u8), bool>>> = Arc::new(Mutex::new(HashMap::new()));
    let sink = levels.clone();
    mcu.on_pin_change(Box::new(move |pin, high, _cycle| {
        sink.lock().unwrap().insert((pin.port, pin.bit), high);
    }));

    // Let the firmware run a few chunks; it drives DRIVEN_GPIO high on the first
    // loop iteration, so the ODR-poll must observe the 0 -> 1 edge.
    for _ in 0..4 {
        mcu.run_micros(20_000).expect("run chunk");
    }

    let driven_high = levels
        .lock()
        .unwrap()
        .get(&('0', DRIVEN_GPIO))
        .copied()
        .unwrap_or(false);
    assert!(
        driven_high,
        "firmware drives GPIO{DRIVEN_GPIO} high via SIO GPIO_OUT; the ODR-poll of \
         SIO GPIO_OUT (offset 0x10) must synthesise a rising edge on ('0', {DRIVEN_GPIO}). \
         A miss here means either the SIO peripheral name or the GPIO_OUT read-back \
         assumption in RenodeConfig::rp2040() needs correcting against this platform."
    );

    std::fs::remove_file(&elf).ok();
}
