//! Which peripheral interrupt vectors an AVR firmware image uses.
//!
//! An AVR's interrupt vector table sits at flash address 0: one slot per
//! interrupt, each a `jmp` (parts with more than 8 KiB of flash) or an `rjmp`
//! (smaller parts) to the handler. A toolchain fills the slots of interrupts
//! the program does not handle with a jump to one shared default routine
//! (`__bad_interrupt` in avr-libc), so a slot that leads anywhere else belongs to
//! an interrupt the firmware handles. Reading the table says which peripherals
//! the firmware means to use, for example that it handles the Timer/Counter1
//! capture event, without running it.
//!
//! The vector order is the part's own, from its datasheet's "Reset and Interrupt
//! Vectors" table; [`table_for`] holds the parts it has been read for. A part not
//! listed gets no answer, never a guess.

/// One part's interrupt vector table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorTable {
    /// Bytes per slot: 4 for a `jmp` table, 2 for an `rjmp` table.
    pub slot_bytes: usize,
    /// The vector names in datasheet order, the reset vector first.
    pub names: &'static [&'static str],
}

/// The 26 vectors of the `ATmega48`/88/168/328 family, in datasheet order
/// (DS40002061B Tables 12-1, 12-2, 12-4, 12-6: pp. 66, 68, 71, 74).
const MEGA_X8: [&str; 26] = [
    "RESET",
    "INT0",
    "INT1",
    "PCINT0",
    "PCINT1",
    "PCINT2",
    "WDT",
    "TIMER2_COMPA",
    "TIMER2_COMPB",
    "TIMER2_OVF",
    "TIMER1_CAPT",
    "TIMER1_COMPA",
    "TIMER1_COMPB",
    "TIMER1_OVF",
    "TIMER0_COMPA",
    "TIMER0_COMPB",
    "TIMER0_OVF",
    "SPI_STC",
    "USART_RX",
    "USART_UDRE",
    "USART_TX",
    "ADC",
    "EE_READY",
    "ANALOG_COMP",
    "TWI",
    "SPM_READY",
];

/// The vector table of the simavr part `part` (`"atmega328p"`), when it has
/// been read from the part's datasheet.
#[must_use]
pub fn table_for(part: &str) -> Option<VectorTable> {
    let slot_bytes = match part {
        // 1-word vectors: the 4 KiB and 8 KiB parts.
        "atmega48" | "atmega48p" | "atmega88" | "atmega88p" => 2,
        // 2-word vectors: the 16 KiB and 32 KiB parts.
        "atmega168" | "atmega168p" | "atmega328" | "atmega328p" => 4,
        _ => return None,
    };
    Some(VectorTable {
        slot_bytes,
        names: &MEGA_X8,
    })
}

/// Where one slot leads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// A `jmp`/`rjmp` to this byte address.
    Jump(i64),
    /// `reti`: the interrupt is acknowledged and nothing is done.
    Reti,
    /// Erased flash (0xFFFF) or no bytes: nothing programmed.
    Empty,
    /// Some other code, which is the handler itself.
    Inline,
}

fn word(head: &[u8], at: usize) -> Option<u16> {
    let b = head.get(at..at + 2)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn decode_slot(head: &[u8], at: usize) -> Slot {
    let Some(w0) = word(head, at) else {
        return Slot::Empty;
    };
    if w0 == 0xFFFF {
        return Slot::Empty;
    }
    if w0 == 0x9518 {
        return Slot::Reti;
    }
    // RJMP k: 1100 kkkk kkkk kkkk, a signed word offset from the next word.
    if w0 & 0xF000 == 0xC000 {
        let mut k = i64::from(w0 & 0x0FFF);
        if k >= 0x800 {
            k -= 0x1000;
        }
        let here = i64::try_from(at).unwrap_or(0) / 2;
        return Slot::Jump((here + 1 + k) * 2);
    }
    // JMP k: 1001 010k kkkk 110k, then the low 16 bits of the word address.
    if w0 & 0xFE0E == 0x940C {
        if let Some(w1) = word(head, at + 2) {
            let high = (u32::from(w0 >> 4) & 0x1F) << 17 | (u32::from(w0) & 1) << 16;
            return Slot::Jump(i64::from(high | u32::from(w1)) * 2);
        }
    }
    Slot::Inline
}

/// The names of the vectors the firmware whose first flash bytes are `head`
/// handles, for the part `part`, in table order. The reset vector is not one.
///
/// `None` when the part's table is unknown, or when the image gives no way to
/// tell a handled vector from an unhandled one: fewer than two non-reset slots
/// share a target (so there is no shared default routine to compare against).
#[must_use]
pub fn used_vectors(part: &str, head: &[u8]) -> Option<Vec<&'static str>> {
    let table = table_for(part)?;
    let slots: Vec<Slot> = (1..table.names.len())
        .map(|i| decode_slot(head, i * table.slot_bytes))
        .collect();
    // The default routine: the jump target most slots share.
    let mut counts: Vec<(i64, usize)> = Vec::new();
    for slot in &slots {
        if let Slot::Jump(target) = slot {
            match counts.iter_mut().find(|(t, _)| t == target) {
                Some((_, n)) => *n += 1,
                None => counts.push((*target, 1)),
            }
        }
    }
    let &(default, shared) = counts.iter().max_by_key(|(_, n)| *n)?;
    if shared < 2 {
        return None;
    }
    Some(
        slots
            .iter()
            .zip(&table.names[1..])
            .filter(|(slot, _)| match slot {
                Slot::Jump(target) => *target != default,
                Slot::Inline => true,
                Slot::Reti | Slot::Empty => false,
            })
            .map(|(_, name)| *name)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `jmp` table: every vector to `default`, then `handled` (vector number,
    /// 1-based, reset being 1) to its own target. Word addresses.
    fn jmp_table(len: usize, default: u32, handled: &[(usize, u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in 1..=len {
            let target = handled
                .iter()
                .find(|(v, _)| *v == n)
                .map_or(default, |(_, t)| *t);
            let w0 =
                0x940C | (((target >> 16) & 1) as u16) | ((((target >> 17) & 0x1F) as u16) << 4);
            out.extend_from_slice(&w0.to_le_bytes());
            out.extend_from_slice(&((target & 0xFFFF) as u16).to_le_bytes());
        }
        out
    }

    #[test]
    fn a_vector_that_leads_away_from_the_shared_default_is_used() {
        // Vector 11 is TIMER1_CAPT on the ATmega328P.
        let head = jmp_table(26, 0x0070, &[(1, 0x0080), (11, 0x0123), (17, 0x0150)]);
        let used = used_vectors("atmega328p", &head).unwrap();
        assert_eq!(used, vec!["TIMER1_CAPT", "TIMER0_OVF"]);
    }

    #[test]
    fn a_table_with_every_vector_on_the_default_uses_none() {
        let head = jmp_table(26, 0x0070, &[(1, 0x0080)]);
        assert_eq!(
            used_vectors("atmega328p", &head).unwrap(),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn rjmp_tables_decode_relative_offsets() {
        // ATmega88: 1-word slots. Slot n at word n; default routine at word 40.
        let mut head = Vec::new();
        for n in 0u16..26 {
            let target: u16 = if n == 10 { 50 } else { 40 };
            let k = (target - n - 1) & 0x0FFF;
            head.extend_from_slice(&(0xC000 | k).to_le_bytes());
        }
        assert_eq!(
            used_vectors("atmega88", &head).unwrap(),
            vec!["TIMER1_CAPT"]
        );
    }

    #[test]
    fn a_reti_slot_and_an_unknown_part_say_nothing() {
        let mut head = jmp_table(26, 0x0070, &[(1, 0x0080)]);
        // Vector 11 (offset 40) replaced by RETI.
        head[40..42].copy_from_slice(&0x9518u16.to_le_bytes());
        assert!(used_vectors("atmega328p", &head).unwrap().is_empty());
        assert!(used_vectors("atmega2560", &head).is_none());
    }

    #[test]
    fn an_image_without_a_shared_default_is_not_guessed_at() {
        let head = jmp_table(1, 0x0080, &[]);
        assert!(used_vectors("atmega328p", &head).is_none());
    }
}
