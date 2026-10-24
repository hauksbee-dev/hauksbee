//! A crystal's start-up time: how long after its oscillator is enabled the
//! crystal's oscillation is large and steady enough to clock a core.
//!
//! Firmware either polls a hardware ready flag (STM32 `HSERDY`, which the
//! oscillator raises once it sees a usable clock) or trusts a fixed delay (the
//! RP2040's `XOSC_STARTUP.DELAY` counter, an AVR's start-up fuses, a busy
//! wait). A fixed delay shorter than the crystal's real start-up hands the core
//! a clock that is not there yet; the co-sim reproduces that, and it needs the
//! start-up time to do so.
//!
//! The time comes from the crystal's own card (`startup_s`, read from its
//! datasheet). A crystal with no card, or a card that does not state it, takes
//! a default for its class, marked assumed, so every finding built on it says
//! so and names the field that would settle it.

use crate::schema::ModelEntry;

/// The card param holding a crystal's start-up time, in seconds.
pub const STARTUP_PARAM: &str = "startup_s";

/// Below this a crystal is a low-frequency tuning fork (32.768 kHz watch
/// crystals); at or above it an AT-cut MHz crystal.
const TUNING_FORK_MAX_HZ: f64 = 1.0e6;

/// Assumed start-up of an MHz crystal with no stated value: 1 ms. RP2040
/// datasheet (build 2023-06), s2.16.3 "Startup Delay" (p.217): the oscillation
/// takes "of the order of a few milliseconds", and "the 1ms default is
/// sufficient for the RP2040 reference design ... which runs the XOSC at
/// 12MHz". The reference-design crystal is an ordinary 3.2 x 2.5 mm SMD part,
/// so the vendor-proven figure is the default rather than a worst case: a
/// default that failed the vendor's own reference design would be a false
/// alarm on every board copying it.
const MHZ_DEFAULT_US: u64 = 1_000;
const MHZ_DEFAULT_BASIS: &str = "assumed 1 ms for an MHz crystal with no stated start-up \
     time (RP2040 datasheet s2.16.3 p.217: 1 ms suffices for its 12 MHz reference-design \
     crystal); state the crystal's part number, or `startup_s` on its card, to settle it";

/// Assumed start-up of a 32.768 kHz tuning-fork crystal: 1 s. `ATmega328P` data
/// sheet DS40002061B, Timer/Counter2 asynchronous operation (pp.160-161): the
/// 32.768 kHz oscillator "might take as long as one second to stabilize".
const KHZ_DEFAULT_US: u64 = 1_000_000;
const KHZ_DEFAULT_BASIS: &str = "assumed 1 s for a 32.768 kHz tuning-fork crystal with no \
     stated start-up time (ATmega328P DS40002061B pp.160-161: the 32.768 kHz oscillator \
     may take as long as one second to stabilize); state `startup_s` on its card to settle it";

/// A crystal's start-up time and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrystalStartup {
    /// Board reference of the crystal (`Y1`).
    pub reference: String,
    /// Start-up time, enable to usable oscillation, in microseconds.
    pub startup_us: u64,
    /// True when no card states it and the class default stands in.
    pub assumed: bool,
    /// The card id and its citation, or the default's basis.
    pub basis: String,
}

impl CrystalStartup {
    /// The start-up time in words: "10 ms (card `abm8_x`)" or "1 ms (assumed ...)".
    #[must_use]
    pub fn describe(&self) -> String {
        format!("{} ({})", format_us(self.startup_us), self.basis)
    }
}

/// `us` as a short human duration: "850 us", "10 ms", "1.5 s".
#[must_use]
pub fn format_us(us: u64) -> String {
    // Durations here are far below 2^52 us, so the float view is exact enough
    // for a two-significant-figure label.
    #[allow(clippy::cast_precision_loss)]
    let v = us as f64;
    if us >= 1_000_000 {
        format!("{} s", trim(v / 1e6))
    } else if us >= 1_000 {
        format!("{} ms", trim(v / 1e3))
    } else {
        format!("{us} us")
    }
}

fn trim(v: f64) -> String {
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The start-up time of crystal `reference`.
///
/// Its card's `startup_s` when the card states one, else the default for its
/// class by frequency (`hz`, from its value or card; an unknown frequency is
/// taken as an MHz core crystal).
#[must_use]
pub fn crystal_startup(
    reference: &str,
    card: Option<&ModelEntry>,
    hz: Option<f64>,
) -> CrystalStartup {
    if let Some((card, s)) = card.and_then(|c| {
        c.params
            .get_f64(STARTUP_PARAM)
            .filter(|s| s.is_finite() && *s > 0.0)
            .map(|s| (c, s))
    }) {
        // Validated to 1 us .. 10 s (ranges.rs), well inside u64.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let startup_us = (s * 1e6).round() as u64;
        return CrystalStartup {
            reference: reference.to_string(),
            startup_us,
            assumed: false,
            basis: format!("card {}", card.id),
        };
    }
    let fork = hz.is_some_and(|hz| hz > 0.0 && hz < TUNING_FORK_MAX_HZ);
    let (startup_us, basis) = if fork {
        (KHZ_DEFAULT_US, KHZ_DEFAULT_BASIS)
    } else {
        (MHZ_DEFAULT_US, MHZ_DEFAULT_BASIS)
    };
    CrystalStartup {
        reference: reference.to_string(),
        startup_us,
        assumed: true,
        basis: basis.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(startup: Option<f64>) -> ModelEntry {
        let extra = startup.map_or_else(String::new, |s| format!("startup_s = {s:e}\n"));
        let toml = format!(
            "id = \"xtal_test\"\nkind = \"passive\"\npassive_class = \"crystal\"\n\
             description = \"test crystal\"\n[match]\nmpn_re = \"^X$\"\n[params]\n\
             cl_f = 10.0e-12\n{extra}[pins]\n\"1\" = \"a\"\n\"2\" = \"b\"\n"
        );
        toml::from_str(&toml).expect("test card parses")
    }

    #[test]
    fn a_card_value_wins_and_is_not_assumed() {
        let s = crystal_startup("Y1", Some(&card(Some(10e-3))), Some(12e6));
        assert_eq!(s.startup_us, 10_000);
        assert!(!s.assumed);
        assert!(s.basis.contains("xtal_test"), "{}", s.basis);
        assert_eq!(format_us(s.startup_us), "10 ms");
    }

    #[test]
    fn a_card_without_the_field_takes_the_class_default() {
        let mhz = crystal_startup("Y1", Some(&card(None)), Some(12e6));
        assert_eq!((mhz.startup_us, mhz.assumed), (1_000, true));
        assert!(mhz.basis.contains("startup_s"), "{}", mhz.basis);
        let fork = crystal_startup("Y2", None, Some(32_768.0));
        assert_eq!((fork.startup_us, fork.assumed), (1_000_000, true));
        assert_eq!(format_us(fork.startup_us), "1 s");
        let unknown = crystal_startup("Y3", None, None);
        assert_eq!(unknown.startup_us, 1_000);
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(format_us(850), "850 us");
        assert_eq!(format_us(1_003), "1.003 ms");
        assert_eq!(format_us(64_200), "64.2 ms");
        assert_eq!(format_us(1_500_000), "1.5 s");
    }
}
