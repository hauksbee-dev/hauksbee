//! Which pin of a standard bus connector carries which line, from the documents
//! that define it, and how a pin or net name states a bus line.
//!
//! A USB receptacle and a Qwiic or STEMMA QT port fix the role of their data
//! pins: the controller on the far side must land its own data pin on the same
//! line, and a board that crosses the pair (`D+` to `D-`, `SDA` to `SCL`) never
//! enumerates or never answers. The swapped-lines check judges a board against
//! this table; it is the one place those roles are written down, and each row
//! names the document it was taken from.
//!
//! Two lines of one bus are an opposite pair ([`BusLine::opposite`]). A name
//! states a line only when it states exactly one ([`bus_line_named`]): a name
//! that holds both `SDA` and `SCL`, or both `D+` and `D-`, states neither.

use std::collections::BTreeSet;

/// One data line of a two-wire bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BusLine {
    /// USB `D+` (`DP`).
    UsbDp,
    /// USB `D-` (`DM`).
    UsbDm,
    /// I2C data (`SDA`).
    I2cSda,
    /// I2C clock (`SCL`).
    I2cScl,
}

impl BusLine {
    /// Every line, for the drift tests.
    pub const ALL: [Self; 4] = [Self::UsbDp, Self::UsbDm, Self::I2cSda, Self::I2cScl];

    /// The other line of the same bus.
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::UsbDp => Self::UsbDm,
            Self::UsbDm => Self::UsbDp,
            Self::I2cSda => Self::I2cScl,
            Self::I2cScl => Self::I2cSda,
        }
    }

    /// The bus the line belongs to.
    #[must_use]
    pub const fn bus(self) -> &'static str {
        match self {
            Self::UsbDp | Self::UsbDm => "USB",
            Self::I2cSda | Self::I2cScl => "I2C",
        }
    }

    /// The line as a reader writes it.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::UsbDp => "D+",
            Self::UsbDm => "D-",
            Self::I2cSda => "SDA",
            Self::I2cScl => "SCL",
        }
    }
}

/// The document a connector's pin order was read from.
#[derive(Debug, Clone, Copy)]
pub struct ConnectorSource {
    /// Title and number of the document.
    pub document: &'static str,
    /// Where it was fetched from.
    pub url: &'static str,
    /// SHA-256 of the fetched file.
    pub sha256: &'static str,
    /// The page, table or section that states the order.
    pub location: &'static str,
}

const USB_C_SPEC: ConnectorSource = ConnectorSource {
    document: "USB Type-C Cable and Connector Specification, Release 2.0, August 2019",
    url: "https://www.usb.org/sites/default/files/documents/usb_type-c.zip",
    sha256: "87d15160bf8bd25129b27e0b7c5e261ed0b1806b3b0d3a7b5d8b1eb783dc9981",
    location: "section 3.2.3 Table 3-4 USB Type-C Receptacle Interface Pin Assignments (p. 68): A6 Dp1, A7 Dn1, B6 Dp2, B7 Dn2",
};

const ADAFRUIT_STEMMA: ConnectorSource = ConnectorSource {
    document: "Adafruit, Introducing Adafruit STEMMA and STEMMA QT (learn.adafruit.com guide, PDF export of 18 pages)",
    url: "https://cdn-learn.adafruit.com/downloads/pdf/introducing-adafruit-stemma-qt.pdf",
    sha256: "823bdf0ea9a2aff1d31eca0c97c846e1566de7abfd1c754fe4504b9f2b7e4ac2",
    location: "page 7, 'STEMMA 4-Pin I2C': STEMMA QT cables follow the SparkFun Qwiic convention, JST SH pin 1 GND, 2 V+, 3 SDA, 4 SCL",
};

const SEEED_GROVE: ConnectorSource = ConnectorSource {
    document: "Seeed Studio wiki, Grove System: 'Interface of Grove modules'",
    url: "https://wiki.seeedstudio.com/Grove_System/",
    sha256: "0bc6af72dfecb00acb8adf7f68f035f53f292dffb8d22d6d44cb99c0257a3d72",
    location: "'Interface of Grove modules': pin 1 yellow (SCL on I2C Grove connectors), pin 2 white (SDA), pin 3 red VCC, pin 4 black GND",
};

/// The receptacle pads of a USB Type-C connector that carry a USB 2.0 line.
///
/// A receptacle repeats the pair on both sides of the connector (the plug's
/// orientation picks which); the two sides are bussed together on the board.
pub const USB_C_RECEPTACLE_LINES: &[(&str, BusLine)] = &[
    ("A6", BusLine::UsbDp),
    ("B6", BusLine::UsbDp),
    ("A7", BusLine::UsbDm),
    ("B7", BusLine::UsbDm),
];

/// The document behind [`USB_C_RECEPTACLE_LINES`].
pub const USB_C_SOURCE: ConnectorSource = USB_C_SPEC;

/// The line a USB Type-C receptacle pad carries, if it carries one.
#[must_use]
pub fn usb_c_line_at(pad: &str) -> Option<BusLine> {
    let pad = pad.trim();
    USB_C_RECEPTACLE_LINES
        .iter()
        .find(|(p, _)| p.eq_ignore_ascii_case(pad))
        .map(|(_, l)| *l)
}

/// A named I2C connector whose pin order a vendor document fixes.
#[derive(Debug, Clone, Copy)]
pub struct BusConnector {
    /// A stable id (`qwiic`).
    pub id: &'static str,
    /// What the connector is called.
    pub name: &'static str,
    /// The numbered pads the connector's signal pins use; a footprint's shield
    /// and mounting pads are not counted.
    pub pin_count: usize,
    /// The pins that carry a bus line.
    pub pins: &'static [(&'static str, BusLine)],
    /// Name alternatives, each a list of whole words that must all appear in a
    /// part's value, symbol or footprint name.
    pub names: &'static [&'static [&'static str]],
    /// The name is shared with connectors that carry other buses (a Grove
    /// socket may be I2C, UART or analogue), so a verdict needs both lines
    /// to be seen crossed.
    pub shared_name: bool,
    /// Where the order comes from.
    pub source: ConnectorSource,
}

impl BusConnector {
    /// The line pin `number` carries.
    #[must_use]
    pub fn line_at(&self, number: &str) -> Option<BusLine> {
        self.pins
            .iter()
            .find(|(n, _)| *n == number.trim())
            .map(|(_, l)| *l)
    }
}

/// Every named bus connector the checks know.
pub const BUS_CONNECTORS: &[BusConnector] = &[
    BusConnector {
        id: "qwiic",
        name: "SparkFun Qwiic (JST SH 4-pin)",
        pin_count: 4,
        pins: &[("3", BusLine::I2cSda), ("4", BusLine::I2cScl)],
        names: &[&["qwiic"]],
        shared_name: false,
        source: ADAFRUIT_STEMMA,
    },
    BusConnector {
        id: "stemma_qt",
        name: "Adafruit STEMMA QT (JST SH 4-pin)",
        pin_count: 4,
        pins: &[("3", BusLine::I2cSda), ("4", BusLine::I2cScl)],
        names: &[&["stemma", "qt"], &["stemmaqt"]],
        shared_name: false,
        source: ADAFRUIT_STEMMA,
    },
    BusConnector {
        id: "grove_i2c",
        name: "Seeed Grove I2C (4-pin)",
        pin_count: 4,
        pins: &[("1", BusLine::I2cScl), ("2", BusLine::I2cSda)],
        names: &[&["grove"]],
        shared_name: true,
        source: SEEED_GROVE,
    },
];

/// The connectors of `pin_count` numbered pads that the lower-case words `words`
/// name.
pub fn bus_connectors_named(
    pin_count: usize,
    words: &[String],
) -> impl Iterator<Item = &'static BusConnector> + '_ {
    BUS_CONNECTORS.iter().filter(move |c| {
        c.pin_count == pin_count
            && c.names
                .iter()
                .any(|all| all.iter().all(|w| words.iter().any(|x| x == w)))
    })
}

/// A word the `D+` / `D-` spelling is read from, with its trailing digits
/// dropped (`dp1`, `usbdm`).
fn usb_word(word: &str) -> Option<BusLine> {
    let bare = word.trim_end_matches(|c: char| c.is_ascii_digit());
    match bare {
        "dp" | "dplus" | "usbdp" | "udp" => Some(BusLine::UsbDp),
        "dm" | "dminus" | "usbdm" | "udm" => Some(BusLine::UsbDm),
        _ => None,
    }
}

/// The USB lines `text` spells with a sign: `D+`, `USB_D-`, `UD+`.
fn signed_usb_lines(lower: &str) -> BTreeSet<BusLine> {
    let mut found = BTreeSet::new();
    let bytes = lower.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        let line = match c {
            b'+' => BusLine::UsbDp,
            b'-' => BusLine::UsbDm,
            _ => continue,
        };
        if i == 0 || bytes[i - 1] != b'd' {
            continue;
        }
        // A `D` that is a word of its own, or the tail of `UD` / `USBD`.
        let before = &lower[..i - 1];
        let starts_word = |s: &str| {
            s.chars()
                .next_back()
                .is_none_or(|p| !p.is_ascii_alphanumeric())
        };
        let prefix = before
            .strip_suffix("usb")
            .or_else(|| before.strip_suffix('u'))
            .unwrap_or(before);
        if starts_word(before) || starts_word(prefix) {
            found.insert(line);
        }
    }
    found
}

/// The one bus line the pin or net name `text` states, if it states exactly one.
///
/// ```
/// use hauksbee_ir::connector_pinouts::{bus_line_named, BusLine};
/// assert_eq!(bus_line_named("D+"), Some(BusLine::UsbDp));
/// assert_eq!(bus_line_named("PA11 (USB_DM)"), Some(BusLine::UsbDm));
/// assert_eq!(bus_line_named("PC5(ADC5/SCL)"), Some(BusLine::I2cScl));
/// assert_eq!(bus_line_named("SDA/SCL"), None);
/// assert_eq!(bus_line_named("PB3"), None);
/// ```
#[must_use]
pub fn bus_line_named(text: &str) -> Option<BusLine> {
    let lower = text.to_ascii_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    // A DisplayPort or SDRAM name also spells `dp` / `dm`; those carry a lane,
    // aux or data-mask word or a polarity letter beside it.
    let display = words
        .iter()
        .any(|w| matches!(*w, "aux" | "hpd" | "lane" | "ml" | "tx" | "rx"))
        || words.windows(2).any(|p| {
            // `DP0_P`, `DM2_N`: a numbered lane with a polarity word after it.
            matches!(p[1], "p" | "n")
                && (p[0].starts_with("dp") || p[0].starts_with("dm"))
                && p[0].ends_with(|c: char| c.is_ascii_digit())
        });
    let mut found = signed_usb_lines(&lower);
    if !display {
        for (i, word) in words.iter().enumerate() {
            found.extend(usb_word(word));
            // `D_PLUS`, `UD_P`: two words spelling one.
            if let Some(next) = words.get(i + 1) {
                let joined = format!("{word}{next}");
                found.extend(usb_word(&joined));
                if *word == "ud" || *word == "d" {
                    match *next {
                        "p" | "plus" => {
                            found.insert(BusLine::UsbDp);
                        }
                        "n" | "m" | "minus" => {
                            found.insert(BusLine::UsbDm);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    for word in &words {
        match word.trim_end_matches(|c: char| c.is_ascii_digit()) {
            "sda" => {
                found.insert(BusLine::I2cSda);
            }
            "scl" => {
                found.insert(BusLine::I2cScl);
            }
            _ => {}
        }
    }
    let mut it = found.into_iter();
    let first = it.next()?;
    it.next().is_none().then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_connector_rows_cite_a_hashed_source_and_list_their_pins_once() {
        for c in BUS_CONNECTORS {
            let mut pins: Vec<_> = c.pins.iter().map(|(n, _)| *n).collect();
            pins.sort_unstable();
            pins.dedup();
            assert_eq!(pins.len(), c.pins.len(), "{}", c.id);
            assert!(c.pins.len() <= c.pin_count, "{}", c.id);
            assert_eq!(c.source.sha256.len(), 64, "{}", c.id);
            assert!(c.source.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(c.source.url.starts_with("https://"), "{}", c.id);
            assert!(!c.names.is_empty() && c.names.iter().all(|a| !a.is_empty()));
            assert!(c.names.iter().flat_map(|a| a.iter()).all(|w| w
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())));
        }
        assert_eq!(USB_C_SOURCE.sha256.len(), 64);
    }

    #[test]
    fn the_pinouts_are_the_documents() {
        let qwiic = &BUS_CONNECTORS[0];
        assert_eq!(qwiic.line_at("3"), Some(BusLine::I2cSda));
        assert_eq!(qwiic.line_at("4"), Some(BusLine::I2cScl));
        assert_eq!(qwiic.line_at("1"), None, "pin 1 is ground");
        let grove = BUS_CONNECTORS.iter().find(|c| c.id == "grove_i2c").unwrap();
        assert_eq!(grove.line_at("1"), Some(BusLine::I2cScl));
        assert_eq!(grove.line_at("2"), Some(BusLine::I2cSda));
        assert_eq!(usb_c_line_at("A6"), Some(BusLine::UsbDp));
        assert_eq!(usb_c_line_at("b7"), Some(BusLine::UsbDm));
        assert_eq!(usb_c_line_at("A5"), None, "A5 is CC1");
    }

    #[test]
    fn connectors_are_found_by_whole_words_and_pad_count() {
        let w = |s: &str| {
            s.split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<String>>()
        };
        let ids = |n: usize, s: &str| -> Vec<&'static str> {
            bus_connectors_named(n, &w(s)).map(|c| c.id).collect()
        };
        assert_eq!(ids(4, "qwiic"), ["qwiic"]);
        assert_eq!(ids(4, "stemma qt"), ["stemma_qt"]);
        assert_eq!(ids(4, "stemmaqt"), ["stemma_qt"]);
        assert!(
            ids(4, "stemma").is_empty(),
            "JST PH STEMMA is another order"
        );
        assert!(ids(5, "qwiic").is_empty());
        assert_eq!(ids(4, "grove"), ["grove_i2c"]);
    }

    #[test]
    fn a_name_states_one_line_or_none() {
        use BusLine::{I2cScl, I2cSda, UsbDm, UsbDp};
        for (name, want) in [
            ("D+", Some(UsbDp)),
            ("D-", Some(UsbDm)),
            ("USB_D+", Some(UsbDp)),
            ("USBD-", Some(UsbDm)),
            ("UD+", Some(UsbDp)),
            ("UD-", Some(UsbDm)),
            ("DP", Some(UsbDp)),
            ("DM1", Some(UsbDm)),
            ("usb_dp", Some(UsbDp)),
            ("pa12_usb_dp", Some(UsbDp)),
            ("d_plus", Some(UsbDp)),
            ("d_minus", Some(UsbDm)),
            ("ud_p", Some(UsbDp)),
            ("ud_n", Some(UsbDm)),
            ("usbdp_dn1", Some(UsbDp)),
            ("pc4_adc4_sda", Some(I2cSda)),
            ("PC5(ADC5/SCL)", Some(I2cScl)),
            ("SDA1", Some(I2cSda)),
            ("SDA/SCL", None),
            ("DP_AUX", None),
            ("DP0_TX", None),
            ("DP0_P", None),
            ("PB3", None),
            ("MENUD+", None),
            ("", None),
        ] {
            assert_eq!(bus_line_named(name), want, "{name}");
        }
    }

    #[test]
    fn the_opposite_of_a_line_is_its_pair() {
        for l in BusLine::ALL {
            assert_ne!(l.opposite(), l);
            assert_eq!(l.opposite().opposite(), l);
            assert_eq!(l.opposite().bus(), l.bus());
        }
    }
}
