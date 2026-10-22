//! The typed peripheral configuration snapshot, per shipped descriptor.
//!
//! Each test feeds a register image into the part's real `[[soc.decode]]`
//! blocks and checks the numbers a person would compute by hand from the
//! reference manual (the working is in a comment on every case). The blocks'
//! manual citations live in the descriptors; these tests prove the expressions
//! implement what those comments say. A register the image leaves out reads as
//! unreadable, so the "unknown stays unknown" cases need no special support.

#![cfg(feature = "renode")]

use hauksbee_mcu::periph_snapshot::{Decoded, PeripheralConfig};
use hauksbee_mcu::RenodeConfig;
use std::collections::BTreeMap;

fn decode(config: &RenodeConfig, regs: &[(u32, u32)]) -> PeripheralConfig {
    let mut image: BTreeMap<u32, u32> = regs.iter().copied().collect();
    PeripheralConfig::decode(&config.decode, &mut image)
}

#[track_caller]
fn close(got: &Decoded<f64>, want: f64) {
    let v = got
        .known()
        .unwrap_or_else(|| panic!("expected {want}, got {got}"));
    assert!(
        (v - want).abs() <= want.abs() * 1e-9,
        "expected {want}, got {v}"
    );
}

fn uart(p: &PeripheralConfig, name: &str) -> hauksbee_mcu::periph_snapshot::UartConfig {
    p.uarts()
        .into_iter()
        .find(|u| u.name == name)
        .unwrap_or_else(|| panic!("no uart block {name}"))
}

fn spi(p: &PeripheralConfig, name: &str) -> hauksbee_mcu::periph_snapshot::SpiConfig {
    p.spis()
        .into_iter()
        .find(|u| u.name == name)
        .unwrap_or_else(|| panic!("no spi block {name}"))
}

fn timer(p: &PeripheralConfig, name: &str) -> hauksbee_mcu::periph_snapshot::TimerConfig {
    p.timers()
        .into_iter()
        .find(|u| u.name == name)
        .unwrap_or_else(|| panic!("no timer block {name}"))
}

// ── STM32F103 (RM0008 Rev 20) ────────────────────────────────────────

/// HSE 8 MHz x PLLMUL 9 = 72 MHz (the Blue Pill bring-up): PLLSRC = 1 (bit 16),
/// PLLXTPRE = 0, PLLMUL = 7 (bits 21:18, x9), SW = SWS = 2 (PLL), HPRE /1,
/// PPRE1 /2 (100 at bits 10:8), PPRE2 /1. PCLK1 = 36 MHz, PCLK2 = 72 MHz.
/// USART1 on PCLK2: BRR 0x271 = 625 gives 72e6 / 625 = 115200 baud.
#[test]
fn f103_pll_72mhz_clock_and_usart1() {
    let cfgr = (1 << 16) | (7 << 18) | (4 << 8) | (2 << 2) | 2;
    let p = decode(
        &RenodeConfig::stm32f103(),
        &[
            (0x4002_1000, 0x0303_0083),
            (0x4002_1004, cfgr),
            (0x4001_3808, 0x271),
            (0x4001_380C, 0),
            (0x4001_3810, 0),
        ],
    );
    let clock = p.clock().expect("clock block");
    close(&clock.core_hz, 72e6);
    let u = uart(&p, "usart1");
    close(&u.baud, 72e6 / 625.0);
    close(&u.data_bits, 8.0);
    close(&u.stop_bits, 1.0);
}

// ── STM32F4 (RM0090 Rev 22) ──────────────────────────────────────────

/// HSE 8 MHz, PLLM 8, PLLN 336, PLLP /2 = 168 MHz; PPRE1 /4 (101 at 12:10) =
/// 42 MHz, PPRE2 /2 (100 at 15:13) = 84 MHz; timers x2 on both.
/// USART2 (PCLK1 42 MHz), OVER8 = 0, BRR = 0x16D3 (mantissa 365, fraction 3):
/// the manual's BRR is USARTDIV in 12.4 fixed point, so baud = 42e6 / 5843.
/// 2 stop bits (CR2 STOP = 10), 9 data bits (CR1 M = 1).
/// SPI2 (PCLK1): BR = 2 gives /8 = 5.25 MHz, CPOL = 1, CPHA = 1.
/// I2C1 fast mode (F/S bit 15), DUTY = 1 (bit 14), CCR = 4: PCLK1 / (25 x 4)
/// = 420 kHz.
#[test]
fn f4_pll_168mhz_and_peripherals() {
    let pllcfgr = (1 << 22) | 8 | (336 << 6);
    let cfgr = (5 << 10) | (4 << 13) | (2 << 2) | 2;
    let p = decode(
        &RenodeConfig::stm32f4_discovery(),
        &[
            (0x4002_3800, 0x0303_0083),
            (0x4002_3804, pllcfgr),
            (0x4002_3808, cfgr),
            (0x4000_4408, 0x16D3),
            (0x4000_440C, 1 << 12),
            (0x4000_4410, 2 << 12),
            (0x4000_3800, (2 << 3) | 0b11),
            (0x4000_541C, (1 << 15) | (1 << 14) | 4),
            (0x4000_0028, 41_999),
            (0x4000_002C, 999),
        ],
    );
    close(&p.clock().expect("clock").core_hz, 168e6);
    let u = uart(&p, "usart2");
    close(&u.baud, 42e6 / f64::from(0x16D3));
    close(&u.data_bits, 9.0);
    close(&u.stop_bits, 2.0);
    let s = spi(&p, "spi2");
    close(&s.sck_hz, 42e6 / 8.0);
    close(&s.cpol, 1.0);
    close(&s.cpha, 1.0);
    close(
        &p.i2cs()
            .iter()
            .find(|i| i.name == "i2c1")
            .expect("i2c1")
            .scl_hz,
        420e3,
    );
    // TIM2 on the x2 APB1 timer clock: 84 MHz / 42000 = 2 kHz, 1000 ticks = 0.5 s.
    let t = timer(&p, "tim2");
    close(&t.tick_hz, 2000.0);
    close(&t.period_s, 0.5);
}

// ── STM32F072 (RM0091 Rev 10) ────────────────────────────────────────

const F072_CR: u32 = 0x4002_1000;
const F072_CFGR: u32 = 0x4002_1004;
const F072_CFGR2: u32 = 0x4002_102C;
const F072_CFGR3: u32 = 0x4002_1030;

/// HSE/PREDIV (PLLSRC = 10) x6 = 48 MHz with HSE 8 MHz and PREDIV /1; SW = SWS
/// = PLL; HPRE /1; PPRE /2 (100): PCLK = 24 MHz, timer clock 48 MHz.
fn f072_regs(cfgr3: u32) -> Vec<(u32, u32)> {
    let cfgr = (2 << 15) | (4 << 18) | (4 << 8) | (2 << 2) | 2;
    vec![
        (F072_CR, 0x0303_0083),
        (F072_CFGR, cfgr),
        (F072_CFGR2, 0),
        (F072_CFGR3, cfgr3),
    ]
}

#[test]
fn f072_pll_48mhz_usart_spi_timers() {
    let mut regs = f072_regs(0);
    regs.extend([
        // USART1 on PCLK (USART1SW = 00), OVER8 = 0: BRR 208 gives 24e6 / 208.
        (0x4001_380C, 0xD0), // BRR at +0x0C
        (0x4001_3800, 0),    // CR1
        (0x4001_3804, 0),    // CR2
        // USART2: OVER8 = 1, BRR = 26 << 4 | 0 gives 24e6 / (8 x 26).
        (0x4000_440C, 0x1A0),
        (0x4000_4400, 1 << 15),
        (0x4000_4404, 0b11 << 12),
        // SPI1: BR = 3 (/16), CPOL = 1.
        (0x4001_3000, (3 << 3) | 0b10),
        // TIM2 (32-bit): PSC 2399, ARR 9999 on 48 MHz.
        (0x4000_0028, 2399),
        (0x4000_002C, 9999),
        // TIM1: PSC 47, ARR 999, RCR 4 on 48 MHz.
        (0x4001_2C28, 47),
        (0x4001_2C2C, 999),
        (0x4001_2C30, 4),
    ]);
    let p = decode(&RenodeConfig::stm32f072(), &regs);
    close(&p.clock().expect("clock").core_hz, 48e6);
    let u1 = uart(&p, "usart1");
    close(&u1.baud, 24e6 / 208.0);
    close(&u1.data_bits, 8.0);
    close(&u1.stop_bits, 1.0);
    let u2 = uart(&p, "usart2");
    close(&u2.baud, 24e6 / 208.0);
    close(&u2.stop_bits, 1.5);
    let s = spi(&p, "spi1");
    close(&s.sck_hz, 24e6 / 16.0);
    close(&s.cpol, 1.0);
    close(&s.cpha, 0.0);
    // 48 MHz / 2400 = 20 kHz; 10000 ticks = 0.5 s.
    let t2 = timer(&p, "tim2");
    close(&t2.tick_hz, 20_000.0);
    close(&t2.period_s, 0.5);
    // 1 MHz tick, (999 + 1) x (4 + 1) ticks = 5 ms.
    let t1 = timer(&p, "tim1");
    close(&t1.tick_hz, 1e6);
    close(&t1.period_s, 0.005);
}

/// USART1SW selects the kernel clock (RM0091 p.133): 01 SYSCLK, 10 LSE, 11 HSI.
#[test]
fn f072_usart1_follows_its_clock_switch() {
    for (sw, fck) in [(0u32, 24e6), (1, 48e6), (2, 32768.0), (3, 8e6)] {
        let mut regs = f072_regs(sw);
        regs.extend([(0x4001_380C, 0x100), (0x4001_3800, 0)]);
        regs.push((0x4001_3804, 0));
        let p = decode(&RenodeConfig::stm32f072(), &regs);
        close(&uart(&p, "usart1").baud, fck / 256.0);
    }
}

/// USART2SW (RCC_CFGR3 17:16, RM0091 p.133, "available only on STM32F07x and
/// STM32F09x"): 00 PCLK, 01 SYSCLK, 10 LSE, 11 HSI, independent of USART1SW.
/// With PCLK 24 MHz, SYSCLK 48 MHz, HSI 8 MHz and BRR 0x100 (256) the baud is
/// the selected clock / 256. USART3 has no switch on the F072 (USART3SW is
/// F09x only), so it stays on PCLK whatever bits 19:18 hold.
#[test]
fn f072_usart2_follows_its_clock_switch() {
    for (sw, fck) in [(0u32, 24e6), (1, 48e6), (2, 32768.0), (3, 8e6)] {
        // USART1SW stays 00 (PCLK) while USART2SW moves.
        let mut regs = f072_regs(sw << 16);
        regs.extend([(0x4000_440C, 0x100), (0x4000_4400, 0), (0x4000_4404, 0)]);
        regs.extend([(0x4001_380C, 0x100), (0x4001_3800, 0), (0x4001_3804, 0)]);
        regs.extend([(0x4000_480C, 0x100), (0x4000_4800, 0), (0x4000_4804, 0)]);
        let p = decode(&RenodeConfig::stm32f072(), &regs);
        close(&uart(&p, "usart2").baud, fck / 256.0);
        close(&uart(&p, "usart1").baud, 24e6 / 256.0);
        close(&uart(&p, "usart3").baud, 24e6 / 256.0);
    }
}

/// Word length is M1:M0 (RM0091 p.744): 01 = 9 bits, 10 = 7 bits, and the
/// reserved 11 stays unknown instead of being guessed.
#[test]
fn f072_word_length_encodings() {
    for (cr1, want) in [
        (0u32, Some(8.0)),
        (1 << 12, Some(9.0)),
        (1 << 28, Some(7.0)),
        ((1 << 12) | (1 << 28), None),
    ] {
        let mut regs = f072_regs(0);
        regs.extend([(0x4000_440C, 0xD0), (0x4000_4400, cr1), (0x4000_4404, 0)]);
        let p = decode(&RenodeConfig::stm32f072(), &regs);
        let bits = uart(&p, "usart2").data_bits;
        match want {
            Some(w) => close(&bits, w),
            None => assert!(bits.is_unknown(), "M = 11 is reserved: {bits}"),
        }
    }
}

/// A register the backend could not read poisons only the values built on it.
#[test]
fn f072_unreadable_register_only_poisons_its_dependants() {
    let mut regs = f072_regs(0);
    regs.retain(|(a, _)| *a != F072_CFGR3);
    regs.extend([(0x4000_4800, 0), (0x4000_4804, 0), (0x4000_480C, 0xD0)]);
    regs.extend([(0x4000_440C, 0xD0), (0x4000_4400, 0), (0x4000_4404, 0)]);
    regs.extend([(0x4001_380C, 0xD0), (0x4001_3800, 0), (0x4001_3804, 0)]);
    let p = decode(&RenodeConfig::stm32f072(), &regs);
    // USART1 and USART2 need CFGR3 for their kernel clocks; USART3 does not.
    assert!(uart(&p, "usart1").baud.is_unknown());
    assert!(uart(&p, "usart2").baud.is_unknown());
    close(&uart(&p, "usart3").baud, 24e6 / 208.0);
    close(&p.clock().expect("clock").core_hz, 48e6);
}

/// The reset state: HSI 8 MHz is SYSCLK, so a zeroed BRR (the divider is zero)
/// is Unknown rather than infinite.
#[test]
fn f072_reset_state_and_zero_divider() {
    let regs = vec![
        (F072_CR, 0x83),
        (F072_CFGR, 0),
        (F072_CFGR2, 0),
        (F072_CFGR3, 0),
        (0x4000_440C, 0),
        (0x4000_4400, 0),
        (0x4000_4404, 0),
    ];
    let p = decode(&RenodeConfig::stm32f072(), &regs);
    close(&p.clock().expect("clock").core_hz, 8e6);
    assert!(uart(&p, "usart2").baud.is_unknown());
}

// ── STM32L0 (RM0377 Rev 10) ──────────────────────────────────────────

const L0_CR: u32 = 0x4002_1000;
const L0_ICSCR: u32 = 0x4002_1004;
const L0_CFGR: u32 = 0x4002_100C;
const L0_CCIPR: u32 = 0x4002_104C;

/// PLL from HSE 8 MHz (PLLSRC bit 16), PLLMUL code 1 = x4, PLLDIV code 01 = /2:
/// 8 x 4 / 2 = 16 MHz. SW = SWS = 3 (PLL); HPRE /1; PPRE1 /1; PPRE2 /4 (101 at
/// 13:11): PCLK2 = 4 MHz, its timers 8 MHz (x2 rule, p.172).
fn l0_regs(ccipr: u32) -> Vec<(u32, u32)> {
    let cfgr = (1 << 16) | (1 << 18) | (1 << 22) | (5 << 11) | (3 << 2) | 3;
    vec![
        (L0_CR, 0x0000_0083),
        (L0_ICSCR, 5 << 13),
        (L0_CFGR, cfgr),
        (L0_CCIPR, ccipr),
    ]
}

#[test]
fn l0_pll_16mhz_usart_spi_timers() {
    let mut regs = l0_regs(0);
    regs.extend([
        // USART2 on APB1 (16 MHz), BRR 139: 16e6 / 139 = 115107.9.
        (0x4000_440C, 139),
        (0x4000_4400, 0),
        (0x4000_4404, 0),
        // USART1 on APB2 (4 MHz), BRR 417: 4e6 / 417 = 9592.
        (0x4001_380C, 417),
        (0x4001_3800, 1 << 12),
        (0x4001_3804, 0b10 << 12),
        // SPI2 (APB1 16 MHz): BR = 7 gives /256.
        (0x4000_3800, 7 << 3),
        // TIM21 on APB2 timers (8 MHz): PSC 7999, ARR 999 gives 1 kHz, 1 s.
        (0x4001_0828, 7999),
        (0x4001_082C, 999),
    ]);
    let p = decode(&RenodeConfig::stm32l0(), &regs);
    close(&p.clock().expect("clock").core_hz, 16e6);
    close(&uart(&p, "usart2").baud, 16e6 / 139.0);
    let u1 = uart(&p, "usart1");
    close(&u1.baud, 4e6 / 417.0);
    close(&u1.data_bits, 9.0);
    close(&u1.stop_bits, 2.0);
    close(&spi(&p, "spi2").sck_hz, 62_500.0);
    let t = timer(&p, "tim21");
    close(&t.tick_hz, 1000.0);
    close(&t.period_s, 1.0);
}

/// USART2SEL (RCC_CCIPR 3:2, p.209): 01 SYSCLK, 10 HSI16, 11 LSE.
#[test]
fn l0_usart2_follows_ccipr() {
    for (sel, fck) in [(0u32, 16e6), (1, 16e6), (2, 16e6), (3, 32768.0)] {
        let mut regs = l0_regs(sel << 2);
        regs.extend([(0x4000_440C, 64), (0x4000_4400, 0), (0x4000_4404, 0)]);
        let p = decode(&RenodeConfig::stm32l0(), &regs);
        close(&uart(&p, "usart2").baud, fck / 64.0);
    }
}

/// HSI16DIVEN (RCC_CR bit 3, RM0377 p.182) divides HSI16 by 4 before the HSI16
/// node of Figure 17 (p.171), so the divided clock is what the SYSCLK mux, the
/// PLLSRC mux and the USART kernel mux see.
const L0_HSI16DIVEN: u32 = 1 << 3;

/// SYSCLK straight from HSI16 (SW = SWS = 01): 16 MHz, or 4 MHz with the divider.
#[test]
fn l0_hsi16diven_divides_sysclk() {
    for (cr, want) in [(0x83u32, 16e6), (0x83 | L0_HSI16DIVEN, 4e6)] {
        let regs = vec![
            (L0_CR, cr),
            (L0_ICSCR, 5 << 13),
            (L0_CFGR, (1 << 2) | 1),
            (L0_CCIPR, 0),
        ];
        let p = decode(&RenodeConfig::stm32l0(), &regs);
        close(&p.clock().expect("clock").core_hz, want);
    }
}

/// PLL fed by HSI16 (PLLSRC = 0), x4 (code 1 at 21:18) and /2 (code 01 at
/// 23:22), SW = SWS = 3: 16 x 4 / 2 = 32 MHz undivided; with HSI16DIVEN the
/// PLL input is 4 MHz, so 4 x 4 / 2 = 8 MHz.
#[test]
fn l0_hsi16diven_divides_the_pll_input() {
    let cfgr = (1 << 18) | (1 << 22) | (3 << 2) | 3;
    for (cr, want) in [(0x83u32, 32e6), (0x83 | L0_HSI16DIVEN, 8e6)] {
        let regs = vec![
            (L0_CR, cr),
            (L0_ICSCR, 5 << 13),
            (L0_CFGR, cfgr),
            (L0_CCIPR, 0),
        ];
        let p = decode(&RenodeConfig::stm32l0(), &regs);
        close(&p.clock().expect("clock").core_hz, want);
    }
}

/// USART2SEL = 10 (HSI16, p.209) takes the divided HSI16: with HSI16DIVEN a
/// BRR of 64 gives 4e6 / 64 instead of 16e6 / 64. The PLL setting of the
/// surrounding image (HSE-fed, 16 MHz) does not matter for this kernel clock.
#[test]
fn l0_usart_hsi16_kernel_clock_is_divided() {
    for (cr, fck) in [(0x83u32, 16e6), (0x83 | L0_HSI16DIVEN, 4e6)] {
        let mut regs = l0_regs(0b10 << 2);
        regs.retain(|(a, _)| *a != L0_CR);
        regs.push((L0_CR, cr));
        regs.extend([(0x4000_440C, 64), (0x4000_4400, 0), (0x4000_4404, 0)]);
        let p = decode(&RenodeConfig::stm32l0(), &regs);
        close(&uart(&p, "usart2").baud, fck / 64.0);
    }
}

/// MSI (reset) clock: ICSCR MSIRANGE = 5 is 2.097152 MHz (p.184).
#[test]
fn l0_reset_state_is_msi() {
    let regs = vec![
        (L0_CR, 0x300),
        (L0_ICSCR, 5 << 13),
        (L0_CFGR, 0),
        (L0_CCIPR, 0),
    ];
    let p = decode(&RenodeConfig::stm32l0(), &regs);
    close(&p.clock().expect("clock").core_hz, 2_097_152.0);
}

// ── STM32G4 (RM0440 Rev 9) ───────────────────────────────────────────

const G4_CR: u32 = 0x4002_1000;
const G4_CFGR: u32 = 0x4002_1008;
const G4_PLLCFGR: u32 = 0x4002_100C;
const G4_CCIPR: u32 = 0x4002_1088;

/// HSE 8 MHz, PLLSRC = 11, PLLM code 1 (/2), PLLN 42, PLLR code 00 (/2):
/// 8 / 2 x 42 / 2 = 84 MHz. SW = SWS = 3 (PLL). PPRE1 /2: PCLK1 = 42 MHz,
/// timers 84 MHz.
fn g4_regs(ccipr: u32) -> Vec<(u32, u32)> {
    let cfgr = (4 << 8) | (3 << 2) | 3;
    let pllcfgr = 3 | (1 << 4) | (42 << 8);
    vec![
        (G4_CR, 0x0303_0063),
        (G4_CFGR, cfgr),
        (G4_PLLCFGR, pllcfgr),
        (G4_CCIPR, ccipr),
    ]
}

#[test]
fn g4_pll_84mhz_usart_prescaler_spi_timers() {
    let mut regs = g4_regs(0);
    regs.extend([
        // USART2 on PCLK1 42 MHz, BRR 365: 42e6 / 365.
        (0x4000_440C, 365),
        (0x4000_4400, 0),
        (0x4000_4404, 0),
        (0x4000_442C, 0),
        // USART3 with PRESCALER = 1 (/2): 21e6 / 365.
        (0x4000_480C, 365),
        (0x4000_4800, 0),
        (0x4000_4804, 0),
        (0x4000_482C, 1),
        // USART1 with the out-of-range PRESCALER 12, treated as /256 (p.1772).
        (0x4001_380C, 164),
        (0x4001_3800, 0),
        (0x4001_3804, 0),
        (0x4001_382C, 12),
        // SPI1 on PCLK2 84 MHz, BR = 2 (/8).
        (0x4001_3000, 2 << 3),
        // TIM8: 84 MHz / 84 = 1 MHz, ARR 999, REP 0: 1 ms.
        (0x4001_3428, 83),
        (0x4001_342C, 999),
        (0x4001_3430, 0),
        // TIM2 (32-bit ARR): PSC 41999 on 84 MHz is 2 kHz, ARR 0xFFFF_FFFF.
        (0x4000_0028, 41_999),
        (0x4000_002C, 0xFFFF_FFFF),
    ]);
    let p = decode(&RenodeConfig::stm32g4(), &regs);
    close(&p.clock().expect("clock").core_hz, 84e6);
    close(&uart(&p, "usart2").baud, 42e6 / 365.0);
    close(&uart(&p, "usart3").baud, 21e6 / 365.0);
    close(&uart(&p, "usart1").baud, 84e6 / 256.0 / 164.0);
    close(&spi(&p, "spi1").sck_hz, 10.5e6);
    let t8 = timer(&p, "tim8");
    close(&t8.tick_hz, 1e6);
    close(&t8.period_s, 0.001);
    let t2 = timer(&p, "tim2");
    close(&t2.tick_hz, 2000.0);
    close(&t2.period_s, 4_294_967_296.0 / 2000.0);
}

/// USART1SEL (RCC_CCIPR 1:0, p.324): 00 PCLK2, 01 SYSCLK, 10 HSI16, 11 LSE.
#[test]
fn g4_usart1_follows_ccipr() {
    for (sel, fck) in [(0u32, 84e6), (1, 84e6), (2, 16e6), (3, 32768.0)] {
        let mut regs = g4_regs(sel);
        regs.extend([
            (0x4001_380C, 64),
            (0x4001_3800, 0),
            (0x4001_3804, 0),
            (0x4001_382C, 0),
        ]);
        let p = decode(&RenodeConfig::stm32g4(), &regs);
        close(&uart(&p, "usart1").baud, fck / 64.0);
    }
}

/// Every Renode STM32 descriptor that decodes a clock also decodes its UART
/// block names uniquely, so a typed view never silently picks the wrong one.
#[test]
fn block_names_are_unique_per_kind() {
    let parts = [
        RenodeConfig::stm32f103(),
        RenodeConfig::stm32f4_discovery(),
        RenodeConfig::stm32f072(),
        RenodeConfig::stm32l0(),
        RenodeConfig::stm32g4(),
    ];
    for cfg in &parts {
        let mut seen = std::collections::BTreeSet::new();
        for b in &cfg.decode {
            assert!(
                seen.insert((format!("{:?}", b.kind), b.name.clone())),
                "{:?} block {} declared twice",
                b.kind,
                b.name
            );
        }
    }
}

// ── RP2040 XOSC startup (RP2040 datasheet s2.16.3, s2.16.7) ──────────────

/// STARTUP.DELAY (bits 13:0) counts 256 crystal cycles and STARTUP.X4 (bit 20)
/// "multiplies the startup_delay by 4". At the reference design's 12 MHz one
/// count is 256 / 12 = 21.333 us: the pico-sdk's DELAY = 47 is 1002.7 us, the
/// hardware reset value 0xC4 (196) is 4181.3 us, and X4 quadruples either.
#[test]
fn rp2040_xosc_startup_includes_x4() {
    let config = RenodeConfig::rp2040();
    let xosc = config
        .clock_ready
        .iter()
        .find(|e| e.name == "xosc")
        .expect("xosc readiness entry");
    let scale = xosc.startup_scale.expect("programmable startup delay");
    let x4 = 1u32 << 20;
    let near = |got: f64, want: f64| {
        assert!((got - want).abs() < 1e-6, "expected {want}, got {got}");
    };
    near(scale.startup_us(47), 47.0 * 256.0 / 12.0);
    near(scale.startup_us(0xC4), 196.0 * 256.0 / 12.0);
    near(scale.startup_us(47 | x4), 4.0 * 47.0 * 256.0 / 12.0);
    near(scale.startup_us(0xC4 | x4), 4.0 * 196.0 * 256.0 / 12.0);
    // Bits outside DELAY and X4 (bit 14 and above, other than X4) change nothing.
    near(scale.startup_us(47 | (1 << 14)), 47.0 * 256.0 / 12.0);
}
