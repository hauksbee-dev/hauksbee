//! The digital-audio vocabulary of a card's `[models.pins]` roles.
//!
//! A card names each pad's role as a free string, and the audio-bus pads (I2S,
//! left-justified, TDM, PCM) are the ones the audio-bus check needs to find:
//! which pad is the bit clock, which the frame clock, which carries data in or
//! out, which takes the master clock. This is the one table that says it.
//!
//! What counts is deliberately narrow, because a check that reads it must never
//! call a legal board wrong:
//!
//! * A role is an audio role only when, once the bus noise words (`i2s`, `i2s1`,
//!   `sai`, `pcm`, `tdm`, `aud`, `audio`, digits) are set aside, what is left is
//!   exactly one name of the table below. `pb5_sck` (a GPIO with an alternate
//!   function), `ws2812_din` and `i2s_bclk_alt_gpio` are not audio roles: a
//!   multiplexed pad can be anything, so it is never held to an audio rule.
//! * A part is held to the audio vocabulary only when its card names both a bit
//!   clock and a frame clock. A part with a bare `sck` is as often SPI.
//! * A bare `sd`, `data` or `sdio` pad is a data pad whose direction the name
//!   does not say; `sdin`, `din`, `sdi`, `dacdat` are inputs and `sdout`,
//!   `dout`, `sdo`, `adcdat` are outputs.

use std::collections::BTreeMap;

/// What an audio pad is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioRole {
    /// The bit clock (`bclk`, `bck`, `sck`, `sclk`).
    BitClock,
    /// The frame clock or word select (`lrclk`, `lrck`, `ws`, `fs`, `fsync`).
    FrameClock,
    /// The master clock (`mclk`, `mck`, `sysclk`).
    MasterClock,
    /// A data input (`sdin`, `din`, `sdi`, `dacdat`).
    DataIn,
    /// A data output (`sdout`, `dout`, `sdo`, `adcdat`).
    DataOut,
    /// A data pad whose direction the name does not state (`sd`, `data`).
    Data,
}

impl AudioRole {
    /// A clock of the bus, as against a data line.
    #[must_use]
    pub const fn is_clock(self) -> bool {
        matches!(self, Self::BitClock | Self::FrameClock | Self::MasterClock)
    }

    /// A data line of the bus.
    #[must_use]
    pub const fn is_data(self) -> bool {
        matches!(self, Self::DataIn | Self::DataOut | Self::Data)
    }

    /// The pad as a message names it.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::BitClock => "bit clock (BCLK)",
            Self::FrameClock => "frame clock (LRCLK/WS)",
            Self::MasterClock => "master clock (MCLK)",
            Self::DataIn => "data input",
            Self::DataOut => "data output",
            Self::Data => "data",
        }
    }
}

/// Whether `token` is a bus noise word: it names the bus or a channel, not the
/// pad's function.
fn is_noise(token: &str) -> bool {
    let bus = ["i2s", "sai", "pcm", "tdm", "aud", "audio"];
    token.chars().all(|c| c.is_ascii_digit())
        || bus.iter().any(|b| {
            token
                .strip_prefix(b)
                .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit()))
        })
}

/// `role` as an audio pad, or `None` for any other role.
#[must_use]
pub fn audio_role(role: &str) -> Option<AudioRole> {
    let lower = role.trim().to_ascii_lowercase();
    let kept: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty() && !is_noise(t))
        .collect();
    let name = kept.concat();
    Some(match name.as_str() {
        "bclk" | "bck" | "sck" | "sclk" | "bitclk" | "bitclock" => AudioRole::BitClock,
        "lrclk" | "lrck" | "lrc" | "ws" | "fs" | "fsync" | "wclk" | "wsel" | "lrclock" => {
            AudioRole::FrameClock
        }
        "mclk" | "mck" | "sysclk" => AudioRole::MasterClock,
        "sdin" | "din" | "sdi" | "dacdat" | "dacdata" | "datain" | "dacin" => AudioRole::DataIn,
        "sdout" | "dout" | "sdo" | "adcdat" | "adcdata" | "dataout" | "adcout" => {
            AudioRole::DataOut
        }
        "sd" | "sdata" | "data" | "sdio" => AudioRole::Data,
        _ => return None,
    })
}

/// The audio pads of a card's pad-to-role map, or `None` when the card is not
/// an audio part (it names no bit clock, or no frame clock).
#[must_use]
pub fn audio_pads(pins: &BTreeMap<String, String>) -> Option<BTreeMap<String, AudioRole>> {
    let roles: BTreeMap<String, AudioRole> = pins
        .iter()
        .filter_map(|(pad, role)| Some((pad.clone(), audio_role(role)?)))
        .collect();
    let has = |wanted: AudioRole| roles.values().any(|r| *r == wanted);
    (has(AudioRole::BitClock) && has(AudioRole::FrameClock)).then_some(roles)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_names_are_read_after_the_bus_noise_words() {
        for (role, want) in [
            ("bclk", AudioRole::BitClock),
            ("I2S_BCLK", AudioRole::BitClock),
            ("i2s1_sck", AudioRole::BitClock),
            ("lrclk", AudioRole::FrameClock),
            ("i2s0_ws", AudioRole::FrameClock),
            ("fs", AudioRole::FrameClock),
            ("mclk", AudioRole::MasterClock),
            ("sd_in", AudioRole::DataIn),
            ("sdin", AudioRole::DataIn),
            ("dacdat", AudioRole::DataIn),
            ("sdout", AudioRole::DataOut),
            ("i2s_dout", AudioRole::DataOut),
            ("adcdat", AudioRole::DataOut),
            ("data", AudioRole::Data),
        ] {
            assert_eq!(audio_role(role), Some(want), "{role}");
        }
        for role in [
            "pb5_sck",
            "ws2812_din",
            "gpio18",
            "i2s_bclk_gpio",
            "vdd",
            "scl",
            "mosi",
            "clk",
        ] {
            assert_eq!(audio_role(role), None, "{role}");
        }
    }

    #[test]
    fn a_part_is_audio_only_when_it_names_both_clocks() {
        let pins = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        let codec = pins(&[("1", "bclk"), ("2", "lrclk"), ("3", "dout"), ("4", "vdd")]);
        let found = audio_pads(&codec).expect("a codec");
        assert_eq!(found.len(), 3);
        // A SPI part has a bare sck and no frame clock.
        let spi = pins(&[("1", "sck"), ("2", "sdo"), ("3", "cs")]);
        assert!(audio_pads(&spi).is_none());
    }
}
