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

/// A token that only numbers the port in front of the signal name (`s1`,
/// `port2`, `ch0`, `slot3`): `s1_bclk` is the bit clock of serial port 1.
/// A bare letter-and-digit like `p5` is not one: `p5_sck` is a GPIO.
fn is_leading_port(token: &str) -> bool {
    let digits = token.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    let word = &token[..token.len() - digits.len()];
    !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
        && matches!(word, "s" | "port" | "ch" | "slot" | "sai" | "i2s")
}

/// A token that only numbers or letters the port after the signal name
/// (`bclk_s1`, `lrclk_2`, `sck_a` of an STM32 SAI sub-block, `ws_p1`).
fn is_trailing_port(token: &str) -> bool {
    let digits = token.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    let word = &token[..token.len() - digits.len()];
    let numbered = !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit());
    (numbered && word.len() <= 1)
        || (numbered && matches!(word, "port" | "ch" | "slot"))
        || matches!(token, "a" | "b" | "c" | "d")
}

/// The name table of the audio vocabulary: `name` is the signal's name with
/// the bus words and port numbers already set aside.
fn table(name: &str) -> Option<AudioRole> {
    Some(match name {
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

/// The role the tokens name, read as is first and then with the port number
/// or letter that may lead or trail the signal name set aside, so `sdin1`,
/// `bclk_s1`, `lrclk_2`, `s1_bclk` and `sai1_sck_a` are read like `sdin`,
/// `bclk`, `lrclk`, `bclk` and `sck`.
fn from_tokens(tokens: &[&str]) -> Option<AudioRole> {
    let mut kept: Vec<&str> = tokens.iter().copied().filter(|t| !is_noise(t)).collect();
    if let Some(role) = table(&kept.concat()) {
        return Some(role);
    }
    while kept.first().is_some_and(|t| is_leading_port(t)) {
        kept.remove(0);
    }
    while kept.len() > 1 && kept.last().is_some_and(|t| is_trailing_port(t)) {
        kept.pop();
        if let Some(role) = table(&kept.concat()) {
            return Some(role);
        }
    }
    // A port number run onto the name (`sdin1`, `bclk2`).
    let joined = kept.concat();
    let stem = joined.trim_end_matches(|c: char| c.is_ascii_digit());
    // At most two digits: `sdin1` is port 1, `ws2812` is an LED.
    if stem.len() < joined.len() && joined.len() - stem.len() <= 2 {
        return table(stem);
    }
    table(&joined)
}

fn tokens_of(text: &str) -> Vec<String> {
    text.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// `role` as an audio pad, or `None` for any other role.
#[must_use]
pub fn audio_role(role: &str) -> Option<AudioRole> {
    let tokens = tokens_of(role);
    from_tokens(&tokens.iter().map(String::as_str).collect::<Vec<_>>())
}

/// The audio-bus signal a NET's name plainly states, or `None`.
///
/// A net name is looser than a card role: it carries a sheet path, a
/// direction suffix and project words. A name is read only when it is clearly
/// about the bus: an unambiguous clock name (`BCLK`, `LRCK`, `MCLK`) or any
/// bus signal name beside a bus word (`I2S_WS`, `CODEC_SDATA`, `SAI1_FS_A`),
/// so `SD`, `DATA`, `WS` or `CLK` alone never says anything. Auto-generated
/// names (`Net-(U1-BCLK)`) are the tool's, not the author's, and say nothing.
#[must_use]
pub fn net_signal(net: &str) -> Option<AudioRole> {
    let lower = net.trim().to_ascii_lowercase();
    if lower.contains("net-(") || lower.starts_with("unconnected-") {
        return None;
    }
    let owned = tokens_of(&lower);
    let tokens: Vec<&str> = owned.iter().map(String::as_str).collect();
    let context = |t: &&str| {
        ["i2s", "sai", "pcm", "tdm", "aud", "audio", "codec", "dac", "adc"]
            .iter()
            .any(|b| t.strip_prefix(b).is_some_and(|r| r.chars().all(|c| c.is_ascii_digit())))
    };
    let has_bus_word = tokens.iter().any(context);
    let bare: Vec<&str> = tokens.iter().copied().filter(|t| !context(t)).collect();
    let mut role = from_tokens(&bare);
    if role.is_none() {
        // A direction suffix (`bclk_out`) after a clock name.
        if let Some((last, head)) = bare.split_last() {
            if matches!(*last, "in" | "out" | "tx" | "rx") {
                role = from_tokens(head).filter(|r| r.is_clock());
            }
        }
    }
    let role = role?;
    let unambiguous = bare.iter().any(|t| {
        let stem = t.trim_end_matches(|c: char| c.is_ascii_digit());
        matches!(stem, "bclk" | "bck" | "lrclk" | "lrck" | "mclk")
    });
    (unambiguous || has_bus_word).then_some(role)
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
            // A port number or letter on the signal name.
            ("BCLK_S1", AudioRole::BitClock),
            ("LRCLK_2", AudioRole::FrameClock),
            ("SDIN1", AudioRole::DataIn),
            ("sdout2", AudioRole::DataOut),
            ("bclk1", AudioRole::BitClock),
            ("s1_bclk", AudioRole::BitClock),
            ("sai1_sck_a", AudioRole::BitClock),
            ("SAI1_FS_B", AudioRole::FrameClock),
            ("mclk_a", AudioRole::MasterClock),
            ("i2s2_ws_p1", AudioRole::FrameClock),
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
            // A GPIO with a letter-and-digit port is not a port suffix.
            "p5_sck",
            "ws2812",
            "pb5_sck_1",
            "gpio18_bclk_alt",
        ] {
            assert_eq!(audio_role(role), None, "{role}");
        }
    }

    #[test]
    fn a_net_name_states_a_signal_only_when_it_is_plainly_about_the_bus() {
        for (net, want) in [
            ("I2S_BCLK", Some(AudioRole::BitClock)),
            ("/audio/I2S_LRCLK", Some(AudioRole::FrameClock)),
            ("BCLK", Some(AudioRole::BitClock)),
            ("MCLK", Some(AudioRole::MasterClock)),
            ("CODEC_MCLK_OUT", Some(AudioRole::MasterClock)),
            ("I2S_WS", Some(AudioRole::FrameClock)),
            ("SAI1_FS_A", Some(AudioRole::FrameClock)),
            ("I2S_SDATA", Some(AudioRole::Data)),
            ("I2S0_DOUT", Some(AudioRole::DataOut)),
            // Too ambiguous alone, or the tool's own naming.
            ("WS", None),
            ("SD", None),
            ("DATA", None),
            ("CLK", None),
            ("SCK", None),
            ("Net-(U1-BCLK)", None),
            ("unconnected-(U1-LRCLK)", None),
            ("+3V3", None),
        ] {
            assert_eq!(net_signal(net), want, "{net}");
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
