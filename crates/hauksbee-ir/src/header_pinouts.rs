//! Standard programming and debug header pinouts, from the documents that
//! define them.
//!
//! A board that carries a debug or serial header mates with a programmer or a
//! USB-to-serial cable whose connector fixes what every pin is. A header wired
//! in another order does not work, and a swap of supply and ground can destroy
//! the adapter or the board. The check that judges a board's header reads the
//! pinout from here: this table is the one place a standard header's pin order
//! is written down, and each row names the document it was taken from.
//!
//! Pin numbers are the header's own pin numbers as the standard numbers them
//! (a two-row header counts across the rows: pins 1 and 2 are the first row).
//! Signals are named from the point of view of the board the header is on,
//! except [`HeaderSignal::AdapterTransmit`] and [`HeaderSignal::AdapterReceive`],
//! which are the adapter's own transmit and receive: the board's transmit
//! must land on [`HeaderSignal::AdapterReceive`].
//!
//! Only headers whose pin order a published document states are listed. There
//! is no entry for a "common 4-pin UART header": the order of those varies
//! from vendor to vendor and no document fixes it, so nothing can be judged
//! against one.

/// What a pin of a standard header carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeaderSignal {
    /// Ground.
    Ground,
    /// The target's supply, as a reference or as power to the adapter.
    Supply,
    /// The target's reset line.
    Reset,
    /// SPI MOSI (programming data into the target).
    Mosi,
    /// SPI MISO (programming data out of the target).
    Miso,
    /// SPI clock.
    Sck,
    /// Serial Wire Debug data (`SWDIO`, the JTAG `TMS` pin on this header).
    SwdData,
    /// Serial Wire Debug clock (`SWCLK`, the JTAG `TCK` pin on this header).
    SwdClock,
    /// Serial Wire Output (`SWO`, the JTAG `TDO` pin on this header).
    Swo,
    /// JTAG data in (`TDI`).
    Tdi,
    /// A mechanical key or a pin to leave unconnected.
    Unused,
    /// The adapter's `TXD` output: the board's receive pin goes here.
    AdapterTransmit,
    /// The adapter's `RXD` input: the board's transmit pin goes here.
    AdapterReceive,
    /// A serial handshake pin (`CTS#`, `RTS#`), optional on the board.
    Handshake,
}

impl HeaderSignal {
    /// The words a reader sees for this signal on a header pin.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ground => "GND",
            Self::Supply => "the target supply",
            Self::Reset => "RESET",
            Self::Mosi => "MOSI",
            Self::Miso => "MISO",
            Self::Sck => "SCK",
            Self::SwdData => "SWDIO",
            Self::SwdClock => "SWCLK",
            Self::Swo => "SWO/TDO",
            Self::Tdi => "TDI",
            Self::Unused => "a key or no-connect pin",
            Self::AdapterTransmit => "the adapter's TXD (the board's receive goes here)",
            Self::AdapterReceive => "the adapter's RXD (the board's transmit goes here)",
            Self::Handshake => "a handshake line (CTS#/RTS#)",
        }
    }
}

/// The document a pinout was read from.
#[derive(Debug, Clone, Copy)]
pub struct HeaderSource {
    /// Title and number of the document.
    pub document: &'static str,
    /// Where it was fetched from.
    pub url: &'static str,
    /// SHA-256 of the fetched file.
    pub sha256: &'static str,
    /// The figure or table that states the order.
    pub location: &'static str,
}

/// One standard header.
#[derive(Debug, Clone, Copy)]
pub struct StandardHeader {
    /// A stable id (`ftdi_6`).
    pub id: &'static str,
    /// What the header is called.
    pub name: &'static str,
    /// The number of pins.
    pub pin_count: usize,
    /// Each pin number and what it carries.
    pub pins: &'static [(&'static str, HeaderSignal)],
    /// Words whose presence in a part's value, symbol or footprint name says it
    /// is this header, each one a whole word of its own.
    pub name_words: &'static [&'static str],
    /// Where the order comes from.
    pub source: HeaderSource,
}

impl StandardHeader {
    /// What pin `number` carries on this header.
    #[must_use]
    pub fn signal_at(&self, number: &str) -> Option<HeaderSignal> {
        self.pins
            .iter()
            .find(|(n, _)| *n == number)
            .map(|(_, s)| *s)
    }

    /// The pin numbers that carry `signal`.
    #[must_use]
    pub fn pins_of(&self, signal: HeaderSignal) -> Vec<&'static str> {
        self.pins
            .iter()
            .filter(|(_, s)| *s == signal)
            .map(|(n, _)| *n)
            .collect()
    }
}

use HeaderSignal as S;

const FTDI_TTL_232R: HeaderSource = HeaderSource {
    document: "FTDI TTL-232R TTL to USB Serial Converter Range of Cables, datasheet v2.01, FT_000054",
    url: "https://www.cl.cam.ac.uk/teaching/0910/P31/docs/usbseriallead.pdf",
    sha256: "7f2040db6cbd121261a6d242308b58822b1bc7e20bef324a3fc7c7225ddf9679",
    location: "section 4.1 Figure 4.1 (6 way header pin out) and section 4.2 Table 4.1 (cable signal descriptions)",
};

const AVR042: HeaderSource = HeaderSource {
    document: "Atmel AVR042: AVR Hardware Design Considerations, Atmel-2521R-09/2016",
    url: "https://ww1.microchip.com/downloads/en/DeviceDoc/atmel-2521-avr-hardware-design-considerations_applicationnote_avr042.pdf",
    sha256: "4b35d4f7b80c6056e2cad84a293767845e366fa7a59830f425b34b01b9d2facf",
    location: "section 4.1 Figure 4-1 Connections for 6/10 Pin ISP Headers (p. 8)",
};

const ATMEL_ICE: HeaderSource = HeaderSource {
    document: "Atmel-ICE User Guide, Atmel-42330C-10/2016",
    url: "https://ww1.microchip.com/downloads/en/DeviceDoc/Atmel-42330-Atmel-ICE_UserGuide.pdf",
    sha256: "e3fc67fea09cc891b3497f337a9f00be08dae591fa91892b13e9ee0605f57cf0",
    location: "section 4.2.2.1 Figure 4-2 and Table 4-1 (p. 25), section 4.2.5 Table 4-4 SWD pin mapping, SAM port column (p. 29)",
};

/// Every standard header the board checks know, one place.
pub const STANDARD_HEADERS: &[StandardHeader] = &[
    StandardHeader {
        id: "ftdi_6",
        name: "FTDI 6-pin serial header (TTL-232R)",
        pin_count: 6,
        pins: &[
            ("1", S::Ground),
            ("2", S::Handshake),
            ("3", S::Supply),
            ("4", S::AdapterTransmit),
            ("5", S::AdapterReceive),
            ("6", S::Handshake),
        ],
        name_words: &["ftdi", "ttl232r", "ttl232"],
        source: FTDI_TTL_232R,
    },
    StandardHeader {
        id: "avr_isp_6",
        name: "AVR 6-pin ISP header",
        pin_count: 6,
        pins: &[
            ("1", S::Miso),
            ("2", S::Supply),
            ("3", S::Sck),
            ("4", S::Mosi),
            ("5", S::Reset),
            ("6", S::Ground),
        ],
        name_words: &["isp", "avrisp"],
        source: AVR042,
    },
    StandardHeader {
        id: "avr_isp_10",
        name: "AVR 10-pin ISP header",
        pin_count: 10,
        pins: &[
            ("1", S::Mosi),
            ("2", S::Supply),
            ("3", S::Unused),
            ("4", S::Ground),
            ("5", S::Reset),
            ("6", S::Ground),
            ("7", S::Sck),
            ("8", S::Ground),
            ("9", S::Miso),
            ("10", S::Ground),
        ],
        name_words: &["isp", "avrisp"],
        source: AVR042,
    },
    StandardHeader {
        id: "arm_cortex_swd_10",
        name: "ARM Cortex debug 10-pin header (SWD/JTAG)",
        pin_count: 10,
        pins: &[
            ("1", S::Supply),
            ("2", S::SwdData),
            ("3", S::Ground),
            ("4", S::SwdClock),
            ("5", S::Ground),
            ("6", S::Swo),
            ("7", S::Unused),
            ("8", S::Tdi),
            ("9", S::Ground),
            ("10", S::Reset),
        ],
        name_words: &["swd", "jtag", "cortex"],
        source: ATMEL_ICE,
    },
];

/// The standard header with id `id`.
#[must_use]
pub fn header(id: &str) -> Option<&'static StandardHeader> {
    STANDARD_HEADERS.iter().find(|h| h.id == id)
}

/// The standard headers with `pin_count` pins that a whole word of `words` names.
pub fn headers_named(
    pin_count: usize,
    words: &[String],
) -> impl Iterator<Item = &'static StandardHeader> + '_ {
    STANDARD_HEADERS.iter().filter(move |h| {
        h.pin_count == pin_count && h.name_words.iter().any(|w| words.iter().any(|x| x == w))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_header_lists_each_of_its_pins_once_and_cites_a_hashed_source() {
        for h in STANDARD_HEADERS {
            assert_eq!(h.pins.len(), h.pin_count, "{}", h.id);
            for n in 1..=h.pin_count {
                let want = n.to_string();
                assert_eq!(
                    h.pins.iter().filter(|(p, _)| *p == want).count(),
                    1,
                    "{} pin {n}",
                    h.id
                );
            }
            assert_eq!(h.source.sha256.len(), 64, "{}", h.id);
            assert!(h.source.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(h.source.url.starts_with("https://"), "{}", h.id);
            assert!(!h.source.location.is_empty() && !h.name_words.is_empty());
            assert!(h
                .name_words
                .iter()
                .all(|w| w.chars().all(char::is_alphanumeric)));
        }
        let mut ids: Vec<_> = STANDARD_HEADERS.iter().map(|h| h.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), STANDARD_HEADERS.len());
    }

    #[test]
    fn the_pinouts_are_the_documents() {
        let ftdi = header("ftdi_6").unwrap();
        assert_eq!(ftdi.signal_at("4"), Some(HeaderSignal::AdapterTransmit));
        assert_eq!(ftdi.signal_at("5"), Some(HeaderSignal::AdapterReceive));
        assert_eq!(ftdi.pins_of(HeaderSignal::Ground), ["1"]);
        let isp = header("avr_isp_6").unwrap();
        assert_eq!(isp.signal_at("4"), Some(HeaderSignal::Mosi));
        assert_eq!(isp.signal_at("1"), Some(HeaderSignal::Miso));
        let swd = header("arm_cortex_swd_10").unwrap();
        assert_eq!(swd.pins_of(HeaderSignal::Ground), ["3", "5", "9"]);
        assert_eq!(swd.signal_at("10"), Some(HeaderSignal::Reset));
        assert_eq!(
            header("avr_isp_10").unwrap().pins_of(HeaderSignal::Sck),
            ["7"]
        );
        assert!(header("uart_4").is_none());
    }

    #[test]
    fn headers_are_found_by_whole_word_and_pin_count() {
        let words = |s: &str| vec![s.to_string()];
        let got: Vec<_> = headers_named(6, &words("ftdi")).map(|h| h.id).collect();
        assert_eq!(got, ["ftdi_6"]);
        let got: Vec<_> = headers_named(10, &words("isp")).map(|h| h.id).collect();
        assert_eq!(got, ["avr_isp_10"]);
        assert_eq!(headers_named(4, &words("ftdi")).count(), 0);
        assert_eq!(headers_named(6, &words("icsp")).count(), 0);
    }
}
