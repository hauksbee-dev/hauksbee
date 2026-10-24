//! What an ATmega48/88/168/328's registers say about pull-ups and sleep.
//!
//! Pure decoding of register bytes the simavr backend reads out of the data
//! space, kept apart from the emulator so it is tested without a core. All
//! addresses are DATA-space addresses (I/O address + 0x20) from the Register
//! Summary of Microchip DS40002061B (ATmega48A/PA/88A/PA/168A/PA/328/P).

use crate::traits::{PinId, SleepMode};

/// `SMCR`, the Sleep Mode Control Register: 0x33 (0x53), section 10.11.1.
pub const SMCR_ADDRESS: u16 = 0x53;

/// `MCUCR`, the MCU Control Register: 0x35 (0x55), section 10.11.2.
pub const MCUCR_ADDRESS: u16 = 0x55;

/// `MCUCR` bit 4: PUD, "Pull-up Disable".
///
/// Set, it disables the pull-up function for every pin of every port (section 14.2.1, Table 14-1: a pin with
/// `DDxn = 0`, `PORTxn = 1` is an input with pull-up only while PUD is 0).
pub const MCUCR_PUD: u8 = 1 << 4;

/// The data-space addresses of `(DDRx, PORTx)` for a port letter.
///
/// The three ports the family has: `DDRB` 0x04 (0x24) with `PORTB` 0x05 (0x25),
/// `DDRC` 0x07 (0x27) with `PORTC` 0x08 (0x28), `DDRD` 0x0A (0x2A) with `PORTD`
/// 0x0B (0x2B); Register Summary, "Port B/C/D Data Direction Register" rows.
#[must_use]
pub const fn port_registers(port: char) -> Option<(u16, u16)> {
    match port {
        'B' => Some((0x24, 0x25)),
        'C' => Some((0x27, 0x28)),
        'D' => Some((0x2A, 0x2B)),
        _ => None,
    }
}

/// Whether `name` (a simavr part name such as `"atmega328p"`) is a member of
/// the family whose register map this module encodes.
#[must_use]
pub fn is_family_member(name: &str) -> bool {
    ["atmega48", "atmega88", "atmega168", "atmega328"]
        .iter()
        .any(|p| name.starts_with(p))
}

/// The sleep mode `SMCR` selects.
///
/// SM2:0 are bits 3:1, Table 10-2 ("Sleep Mode Select"): 000 Idle, 001 ADC Noise Reduction, 010 Power-down, 011
/// Power-save, 110 Standby, 111 External Standby, 100 and 101 reserved.
#[must_use]
pub fn sleep_mode_from_smcr(smcr: u8) -> SleepMode {
    match (smcr >> 1) & 0b111 {
        0b000 => SleepMode::Idle,
        0b001 => SleepMode::AdcNoiseReduction,
        0b010 => SleepMode::PowerDown,
        0b011 => SleepMode::PowerSave,
        0b110 => SleepMode::Standby,
        0b111 => SleepMode::ExtendedStandby,
        other => SleepMode::Other(format!("reserved (SM2:0 = {other:03b})")),
    }
}

/// The pins whose internal pull-up is on: `DDxn = 0` and `PORTxn = 1` with PUD
/// clear (Table 14-1). `ports` carries `(port letter, DDRx, PORTx)`.
///
/// PC6 is left out: unless the RSTDISBL fuse is programmed it is the RESET
/// pin, `PORTC6` does nothing, and the reset pull-up is a separate resistor
/// (`RRST`) the card states separately.
#[must_use]
pub fn pull_ups_from_registers(mcucr: u8, ports: &[(char, u8, u8)]) -> Vec<PinId> {
    if mcucr & MCUCR_PUD != 0 {
        return Vec::new();
    }
    let mut pins = Vec::new();
    for &(port, ddr, data) in ports {
        let on = data & !ddr;
        for bit in 0u8..8 {
            if on & (1 << bit) != 0 && !(port == 'C' && bit == 6) {
                pins.push(PinId { port, bit });
            }
        }
    }
    pins
}

#[cfg(test)]
mod tests {
    use super::{pull_ups_from_registers, sleep_mode_from_smcr, MCUCR_PUD};
    use crate::traits::{PinId, SleepMode};

    #[test]
    fn smcr_decodes_the_sleep_mode_select_table() {
        assert_eq!(sleep_mode_from_smcr(0b0000_0001), SleepMode::Idle);
        assert_eq!(
            sleep_mode_from_smcr(0b0000_0011),
            SleepMode::AdcNoiseReduction
        );
        assert_eq!(sleep_mode_from_smcr(0b0000_0101), SleepMode::PowerDown);
        assert_eq!(sleep_mode_from_smcr(0b0000_0111), SleepMode::PowerSave);
        assert_eq!(sleep_mode_from_smcr(0b0000_1101), SleepMode::Standby);
        assert_eq!(
            sleep_mode_from_smcr(0b0000_1111),
            SleepMode::ExtendedStandby
        );
        assert!(matches!(
            sleep_mode_from_smcr(0b0000_1001),
            SleepMode::Other(_)
        ));
        assert!(SleepMode::PowerDown.is_deep());
        assert!(!SleepMode::Idle.is_deep());
    }

    #[test]
    fn a_pull_up_needs_input_direction_port_high_and_pud_clear() {
        let d = |ddr, port| vec![('D', ddr, port)];
        let pd = |bit| PinId { port: 'D', bit };
        // PD2, PD3 inputs with PORT = 1.
        assert_eq!(
            pull_ups_from_registers(0, &d(0x00, 0b0000_1100)),
            vec![pd(2), pd(3)]
        );
        // PD3 an output driving high: no pull-up.
        assert_eq!(
            pull_ups_from_registers(0, &d(0b0000_1000, 0b0000_1100)),
            vec![pd(2)]
        );
        // PORT = 0: tri-state input.
        assert!(pull_ups_from_registers(0, &d(0, 0)).is_empty());
        // PUD set: every pull-up is off.
        assert!(pull_ups_from_registers(MCUCR_PUD, &d(0, 0xFF)).is_empty());
    }

    #[test]
    fn pc6_is_the_reset_pin_not_a_gpio_pull_up() {
        let pins = pull_ups_from_registers(0, &[('C', 0, 0b0100_0001)]);
        assert_eq!(pins, vec![PinId { port: 'C', bit: 0 }]);
    }
}
