//! Declared net ties read from an Eagle `.sch` (XML, Eagle 6+).
//!
//! This is deliberately NOT a schematic extractor. An Eagle `.brd` already
//! carries the whole netlist, so nothing here derives connectivity. It answers
//! one question the `.brd` cannot: **did the designer declare that two named
//! nets are joined on purpose?**
//!
//! Eagle expresses that with supply symbols. A supply symbol is a library
//! symbol whose single pin carries `direction="sup"`, and the PIN'S NAME is the
//! net the symbol imposes (`GND`, `AGND`, `+5V`). Dropping an `AGND` supply
//! symbol onto a segment of net `GND` is how a star ground is drawn: it says,
//! in the schematic, that these two nets meet here and nowhere else. The board
//! then routes them together and the copper DRC, handed only the `.brd`, sees a
//! short between two differently named nets.
//!
//! The construct is exact and local, so it cannot be mistaken for an accident:
//! a supply symbol is a deliberate placement, and the tie is only claimed when
//! the symbol's own supply name differs from the name of the net it sits on.
//! Two nets that merely touch in copper produce nothing here, which is the
//! point: this narrows what is reported, and it must never widen it.
//!
//! ```text
//! <symbol name="AGND">           <pin name="AGND" ... direction="sup"/>
//! <deviceset name="AGND">        <gate name="VR1" symbol="AGND"/>
//! <part name="AGND7" library="supply1" deviceset="AGND"/>
//! <net name="GND"><segment>
//!   <pinref part="SUPPLY6" gate="GND" pin="GND"/>   <- agrees with the net
//!   <pinref part="AGND7"   gate="VR1" pin="AGND"/>  <- declares the tie
//! </segment></net>
//! ```

use crate::ExtractError;
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::eagle::xml_attrs as attrs;
use std::collections::HashMap;

/// One deliberate tie between two named nets, as the schematic declares it.
///
/// `net` is the net the segment belongs to and `tied_net` the net the foreign
/// supply symbol imposes. The pair is unordered for matching purposes; both
/// spellings are kept because the report quotes the schematic's own wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredNetTie {
    /// The net whose segment carries the tie (`GND` in the example above).
    pub net: String,
    /// The net the foreign supply symbol imposes (`AGND`).
    pub tied_net: String,
    /// The foreign supply symbol's part name (`AGND7`).
    pub symbol: String,
    /// Supply symbols in the same segment that agree with `net` (`SUPPLY6`),
    /// i.e. what the foreign symbol is wired to. Empty when the segment names
    /// its net by a label rather than a second supply symbol.
    pub tied_to: Vec<String>,
}

/// Physical schematic part identity used to prove a `.brd`/`.sch` pair belongs
/// to the same design before any declaration can lower severity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SchematicPartIdentity {
    pub reference: String,
    pub value: String,
}

/// One physical package pad's net incidence in the schematic.
///
/// Eagle schematic pins are translated through the selected device's `<connect>` map so `pin`
/// uses the same package-pad vocabulary as the companion `.brd`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SchematicPinNetIdentity {
    pub reference: String,
    pub pin: String,
    pub net: String,
}

impl DeclaredNetTie {
    /// True when this declaration covers a copper contact between these two
    /// nets, in either order. Net names are compared exactly: Eagle net names
    /// are case-sensitive and `GND` is not `gnd`.
    #[must_use]
    pub fn covers(&self, net_a: &str, net_b: &str) -> bool {
        (self.net == net_a && self.tied_net == net_b)
            || (self.net == net_b && self.tied_net == net_a)
    }

    /// The declaration in one clause, naming the symbols and the net, for a
    /// report line. `AGND7 wired to SUPPLY6 in net GND`.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.tied_to.is_empty() {
            format!("{} placed on net {}", self.symbol, self.net)
        } else {
            format!(
                "{} wired to {} in net {}",
                self.symbol,
                self.tied_to.join("/"),
                self.net
            )
        }
    }
}

/// True for text that is an Eagle XML schematic rather than a board.
///
/// Both share the `<eagle>` root, so the discriminator is the `<schematic>`
/// element, which a `.brd` never carries (it has `<board>` instead).
///
/// `<schematic>` is deliberately searched over the WHOLE document, not a head
/// slice: Eagle writes `<settings>`, `<grid>` and the full `<layers>` table
/// first, so `<schematic>` can sit ten kilobytes in and a head slice would
/// reject a perfectly good schematic. Only `<eagle>` is checked near the top,
/// where it genuinely is; the scan is one linear pass, cheap next to parsing.
#[must_use]
pub fn looks_like_eagle_schematic(text: &str) -> bool {
    parse(text).is_ok()
}

/// Every deliberate net tie the schematic declares, in file order.
///
/// Returns an empty vector for a schematic that declares none, which is the
/// common case and is not an error: most boards keep their supply symbols on
/// their own nets. An XML error IS surfaced, because a companion input the user
/// explicitly supplied must not be silently half-read.
///
/// # Errors
///
/// Returns [`ExtractError::Xml`] when the schematic is not well-formed XML.
pub fn declared_net_ties(text: &str) -> Result<Vec<DeclaredNetTie>, ExtractError> {
    let parsed = parse(text)?;
    let mut out = Vec::new();
    for segment in &parsed.segments {
        // Split the segment's supply symbols into those that agree with the
        // net's name and those that impose a different one. Only the latter
        // declare anything; the former are what they are wired to.
        let mut agreeing = Vec::new();
        let mut foreign = Vec::new();
        for part in &segment.supply_parts {
            let Some(supply) = parsed.part_supply.get(part) else {
                continue;
            };
            if *supply == segment.net {
                agreeing.push(part.clone());
            } else {
                foreign.push((part.clone(), supply.clone()));
            }
        }
        for (symbol, tied_net) in foreign {
            out.push(DeclaredNetTie {
                net: segment.net.clone(),
                tied_net,
                symbol,
                tied_to: agreeing.clone(),
            });
        }
    }
    Ok(out)
}

/// The schematic's physical parts (reference and value), sorted and deduplicated.
///
/// # Errors
///
/// Returns [`ExtractError::Xml`] when the schematic is not well-formed XML.
pub fn schematic_part_identities(text: &str) -> Result<Vec<SchematicPartIdentity>, ExtractError> {
    let mut parts = parse(text)?.physical_parts;
    parts.sort();
    parts.dedup();
    Ok(parts)
}

/// The library device name of every physical part whose value is blank.
///
/// Returned as (reference, name), sorted. With no value set, the deviceset/device a part
/// was placed from (`ATTINY85`) is the only identity the design states.
///
/// # Errors
///
/// Returns [`ExtractError::Xml`] when the schematic is not well-formed XML.
pub fn schematic_blank_value_device_names(
    text: &str,
) -> Result<Vec<(String, String)>, ExtractError> {
    let mut names = parse(text)?.blank_value_device_names;
    names.sort();
    Ok(names)
}

/// The manufacturer part number embedded in each physical part's library
/// device name, for parts that have one: `(reference, part number)`, sorted.
///
/// `SparkFun` style libraries put the part number in the device
/// (`MOSFET_PCH` / `-DMG2305UX-7`) or across the deviceset and device
/// (`V_REG_AP2112` + `K-3.3V`) and a rating string in the value
/// (`20V/4.2A/52mΩ/1.4W`), which names no part. Each is only a candidate:
/// the binder keeps it only where it identifies the part better than the
/// value did.
///
/// # Errors
///
/// Returns [`ExtractError::Xml`] when the schematic is not well-formed XML.
pub fn schematic_device_part_numbers(text: &str) -> Result<Vec<(String, String)>, ExtractError> {
    let mut found = parse(text)?.device_part_numbers;
    found.sort();
    Ok(found)
}

/// The part number an Eagle device name spells, if any.
///
/// The device (package variant) alone when it is one, else the full name with its leading class words
/// (`MOSFET`, `V_REG`) dropped. `None` when neither looks like a part number
/// (a value code, a package, a class word, a tolerance).
#[must_use]
pub fn embedded_part_number(deviceset: &str, device: &str, technology: &str) -> Option<String> {
    let device = device.trim_matches(|c: char| c == '-' || c == '_');
    if looks_like_part_number(device) {
        return Some(device.to_string());
    }
    // The deviceset carries the number's head (`V_REG_AP2112` + `K-3.3V`)
    // only when the device continues it: a device that is a package or a
    // value code (`DFN8`, `3.3V`) appended to it spells nothing.
    let continues = device.is_empty()
        || (device.starts_with(|c: char| c.is_ascii_alphabetic()) && !is_shape_code(device));
    if !continues {
        return None;
    }
    let full = eagle_device_name(deviceset, device, technology);
    // Drop leading letter-only words: the class the library files the part
    // under, never part of the number.
    let mut rest = full.as_str();
    while let Some(i) = rest.find(['-', '_']) {
        if !rest[..i].chars().all(|c| c.is_ascii_alphabetic()) {
            break;
        }
        rest = &rest[i + 1..];
    }
    let rest = rest.trim_matches(|c: char| c == '-' || c == '_');
    looks_like_part_number(rest).then(|| rest.to_string())
}

/// Package words that, followed by a pin count or size, name a shape.
const SHAPE_WORDS: [&str; 26] = [
    "SOT", "SOD", "SOIC", "SOP", "SSOP", "TSSOP", "MSOP", "QFN", "DFN", "QFP", "TQFP", "LQFP",
    "BGA", "TO", "DPAK", "SMA", "SMB", "SMC", "EIA", "DO", "SMD", "PTH", "DIP", "CAP", "RES",
    "LED",
];

/// Imperial chip sizes, which name a footprint whatever word they follow.
const SIZE_CODES: [&str; 8] = [
    "0201", "0402", "0603", "0805", "1206", "1210", "2010", "2512",
];

/// Whether the first word of `text` is a package or size code (`SOT23`,
/// `DFN8`, `EIA3216`, `0603`, `BLUE0603`) rather than a part's name.
fn is_shape_code(text: &str) -> bool {
    let first = first_word(text);
    let word: String = first
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect();
    let tail = &first[word.len()..];
    let shape =
        SHAPE_WORDS.contains(&word.as_str()) && tail.starts_with(|c: char| c.is_ascii_digit());
    shape || SIZE_CODES.contains(&tail)
}

fn first_word(text: &str) -> String {
    text.split(['-', '_'])
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase()
}

/// Whether `text` reads as a manufacturer part number: one token of letters,
/// digits and `-` `.` `/` `+` `#`, whose first word mixes letters and digits
/// and starts with a letter (or is a JEDEC `1N`..`4N` number), and is not a
/// package or size code.
fn looks_like_part_number(text: &str) -> bool {
    let allowed = |c: char| c.is_ascii_alphanumeric() || "-./+#_".contains(c);
    if text.len() < 4 || !text.chars().all(allowed) {
        return false;
    }
    let first = first_word(text);
    let letters = first.chars().any(|c| c.is_ascii_alphabetic());
    let digits = first.chars().any(|c| c.is_ascii_digit());
    let jedec = matches!(first.as_bytes(), [b'1'..=b'4', b'N', b'0'..=b'9', ..]);
    let lead = first.starts_with(|c: char| c.is_ascii_alphabetic()) || jedec;
    letters && digits && lead && !is_shape_code(text)
}

/// Eagle's full device name: `*` in the deviceset name takes the technology and
/// `?` the device (package variant); with no `?` the device name is appended.
fn eagle_device_name(deviceset: &str, device: &str, technology: &str) -> String {
    let name = deviceset.replace('*', technology);
    if name.contains('?') {
        name.replace('?', device)
    } else {
        name + device
    }
}

/// Every package-pad net incidence in the schematic, sorted and deduplicated.
///
/// # Errors
///
/// Returns [`ExtractError::Xml`] when the schematic is not well-formed XML.
pub fn schematic_pin_net_identities(
    text: &str,
) -> Result<Vec<SchematicPinNetIdentity>, ExtractError> {
    let mut incidences = parse(text)?.physical_pin_nets;
    incidences.sort();
    incidences.dedup();
    Ok(incidences)
}

/// Every physical part's package-pad pin names, as (reference, pad, pin name),
/// sorted.
///
/// An Eagle `.brd` carries pads but no pin names; the schematic's
/// `<connect>` map is where "pad 25 is IO0" lives. Eagle's `@n` suffix for
/// repeated pins ("GND@2") is dropped.
///
/// # Errors
///
/// Returns [`ExtractError::Xml`] when the schematic is not well-formed XML.
pub fn schematic_pad_names(text: &str) -> Result<Vec<(String, String, String)>, ExtractError> {
    let mut out: Vec<_> = schematic_pad_pins(text)?
        .into_iter()
        .map(|(part, pad, name, _)| (part, pad, name))
        .collect();
    out.dedup();
    Ok(out)
}

/// The pin kind (KiCad electrical-type vocabulary) an Eagle pin `direction`
/// states. Eagle's default when the attribute is absent is `io`, but an
/// absent attribute is no evidence, so it maps to an empty (unknown) kind.
fn kind_of_direction(direction: &str) -> &'static str {
    match direction {
        "in" => "input",
        "out" => "output",
        "io" => "bidirectional",
        "oc" => "open_collector",
        "hiz" => "tri_state",
        "pas" => "passive",
        "pwr" | "sup" => "power_in",
        "nc" => "no_connect",
        _ => "",
    }
}

/// Every physical part's package-pad pins, sorted.
///
/// Each entry is (reference, pad, pin name, pin kind). The kind comes from the symbol pin's stated
/// `direction` (`kind_of_direction`); empty when the library states none.
///
/// # Errors
///
/// Returns [`ExtractError::Xml`] when the schematic is not well-formed XML.
pub fn schematic_pad_pins(
    text: &str,
) -> Result<Vec<(String, String, String, String)>, ExtractError> {
    let parsed = parse(text)?;
    let mut out = Vec::new();
    for (part, (library, deviceset, device)) in &parsed.part_devices {
        for ((l, d, v, gate, pin), pads) in &parsed.device_pin_maps {
            if (l, d, v) != (library, deviceset, device) {
                continue;
            }
            let kind = parsed
                .gate_symbols
                .get(&(l.clone(), d.clone(), gate.clone()))
                .and_then(|symbol| {
                    parsed
                        .pin_directions
                        .get(&(l.clone(), symbol.clone(), pin.clone()))
                })
                .map_or("", |dir| kind_of_direction(dir));
            let name = pin.split('@').next().unwrap_or(pin);
            out.extend(pads.iter().map(|pad| {
                (
                    part.clone(),
                    pad.clone(),
                    name.to_string(),
                    kind.to_string(),
                )
            }));
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// One `<segment>` of one `<net>`: which net it belongs to and which of its
/// `<pinref>`s name a part this file defines as a supply symbol.
struct Segment {
    net: String,
    supply_parts: Vec<String>,
}

/// Pins seen inside one `<symbol>`, so "is this a supply symbol?" can be decided
/// once the symbol closes rather than pin by pin.
///
/// The single-pin requirement is load-bearing, and getting it wrong is how this
/// module silences real shorts. Eagle libraries routinely mark an ordinary
/// component's power pins `direction="sup"` (an SD socket, a radio module), and
/// registering those as supply symbols would make the part "declare" a tie
/// between ground and every net it touches, attaching false intent context to a
/// genuine rail-to-ground short. A real Eagle supply symbol is a bare marker:
/// one pin, and that pin is `sup`.
#[derive(Default)]
struct SymbolPins {
    total: usize,
    sup_names: Vec<String>,
}

impl SymbolPins {
    /// The net this symbol imposes, or `None` when it is an ordinary component.
    fn supply_net(&self) -> Option<&str> {
        if self.total == 1 && self.sup_names.len() == 1 {
            let name = self.sup_names[0].as_str();
            // An unnamed pin names no net.
            return (!name.trim().is_empty()).then_some(name);
        }
        None
    }
}

/// One `<deviceset>` being scanned, resolved at its closing tag.
struct DevicesetScan {
    name: String,
    gate_symbols: Vec<String>,
    /// True once any `<device>` names a package: then this is a physical part,
    /// not a supply marker, whatever its symbol looks like.
    any_device_has_package: bool,
}

/// Eagle library identity. Embedded libraries can share their display name;
/// the URN is the stable identity carried by both `<library>` and `<part>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LibraryId {
    name: String,
    urn: Option<String>,
}

struct Parsed {
    /// Part name -> the net name its supply symbol imposes. Only supply parts
    /// appear here, so a lookup miss means "an ordinary component".
    part_supply: HashMap<String, String>,
    segments: Vec<Segment>,
    physical_parts: Vec<SchematicPartIdentity>,
    physical_pin_nets: Vec<SchematicPinNetIdentity>,
    part_devices: HashMap<String, (LibraryId, String, String)>,
    /// Part name -> its library device name, for physical parts with no value.
    blank_value_device_names: Vec<(String, String)>,
    /// Part name -> the part number its library device name embeds.
    device_part_numbers: Vec<(String, String)>,
    device_pin_maps: DevicePinMaps,
    /// (library, symbol, pin) -> the pin's Eagle `direction`, when stated.
    pin_directions: HashMap<(LibraryId, String, String), String>,
    /// (library, deviceset, gate) -> the symbol that gate places.
    gate_symbols: HashMap<(LibraryId, String, String), String>,
}

/// (library, deviceset, device, gate, schematic pin) -> package pad(s).
type DevicePinMaps = HashMap<(LibraryId, String, String, String, String), Vec<String>>;

/// A single streaming pass, in the order Eagle writes the file: `<libraries>`
/// (symbols, then devicesets) precede `<parts>`, which precede `<sheets>`. That
/// order is what makes one pass enough, and it is the order Eagle's DTD fixes.
#[allow(clippy::too_many_lines, reason = "one streaming pass over the XML")]
fn parse(text: &str) -> Result<Parsed, ExtractError> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);

    // (library, symbol) -> the supply net name, for symbols that pass BOTH tests
    // in `SymbolPins` below.
    let mut supply_symbols: HashMap<(LibraryId, String), String> = HashMap::new();
    // (library, deviceset) -> the supply net name, resolved at `</deviceset>`.
    let mut supply_devicesets: HashMap<(LibraryId, String), String> = HashMap::new();
    let mut libraries_by_name: HashMap<String, Vec<LibraryId>> = HashMap::new();
    let mut part_supply: HashMap<String, String> = HashMap::new();
    let mut segments: Vec<Segment> = Vec::new();
    let mut physical_parts = Vec::new();
    let mut physical_pin_nets = Vec::new();
    let mut device_pin_maps: DevicePinMaps = HashMap::new();
    // Physical part -> its selected library/deviceset/device mapping.
    let mut physical_part_devices: HashMap<String, (LibraryId, String, String)> = HashMap::new();
    let mut blank_value_device_names = Vec::new();
    let mut device_part_numbers = Vec::new();

    let mut cur_library: Option<LibraryId> = None;
    let mut cur_symbol: Option<String> = None;
    let mut pin_directions: HashMap<(LibraryId, String, String), String> = HashMap::new();
    let mut gate_symbol_of: HashMap<(LibraryId, String, String), String> = HashMap::new();
    let mut cur_symbol_pins = SymbolPins::default();
    let mut cur_deviceset: Option<DevicesetScan> = None;
    let mut cur_device: Option<String> = None;
    let mut cur_net: Option<String> = None;
    let mut cur_segment: Option<Segment> = None;
    let mut saw_document_element = false;
    let mut saw_eagle_root = false;
    let mut in_eagle = false;
    let mut in_schematic = false;
    let mut saw_schematic = false;
    let mut closed_schematic = false;
    let mut invalid_structure = false;
    let mut element_path: Vec<Vec<u8>> = Vec::new();

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(event @ (Event::Start(_) | Event::Empty(_))) => {
                let (e, is_empty) = match &event {
                    Event::Start(e) => (e, false),
                    Event::Empty(e) => (e, true),
                    _ => unreachable!(),
                };
                let a = attrs(e);
                let name = e.name().as_ref().to_vec();
                if element_path.is_empty() {
                    if saw_document_element {
                        invalid_structure = true;
                    } else {
                        saw_document_element = true;
                        saw_eagle_root = name == b"eagle";
                    }
                }
                if name == b"eagle" && element_path.is_empty() && saw_eagle_root {
                    in_eagle = !is_empty;
                    if !is_empty {
                        element_path.push(name);
                    }
                    buf.clear();
                    continue;
                }
                if name == b"schematic" {
                    let is_real_schematic = in_eagle
                        && element_path.len() == 2
                        && element_path[0].as_slice() == b"eagle"
                        && element_path[1].as_slice() == b"drawing";
                    if is_real_schematic && !saw_schematic {
                        saw_schematic = true;
                        in_schematic = !is_empty;
                        closed_schematic = is_empty;
                    } else {
                        invalid_structure = true;
                    }
                    if !is_empty {
                        element_path.push(name);
                    }
                    buf.clear();
                    continue;
                }
                if !in_schematic {
                    if !is_empty {
                        element_path.push(name);
                    }
                    buf.clear();
                    continue;
                }
                if !is_empty {
                    element_path.push(name);
                }
                match e.name().as_ref() {
                    b"library" => {
                        let library = LibraryId {
                            name: a.get("name").cloned().unwrap_or_default(),
                            urn: a.get("urn").cloned(),
                        };
                        let identities = libraries_by_name.entry(library.name.clone()).or_default();
                        if !identities.contains(&library) {
                            identities.push(library.clone());
                        }
                        cur_library = Some(library);
                    }
                    b"symbol" => {
                        cur_symbol = a.get("name").cloned();
                        cur_symbol_pins = SymbolPins::default();
                    }
                    b"pin" => {
                        // Count EVERY pin, and remember the name of the `sup`
                        // one. The decision is deferred to `</symbol>` because it
                        // depends on the total: see `SymbolPins::supply_net`.
                        if let (Some(library), Some(symbol), Some(pin), Some(dir)) = (
                            cur_library.as_ref(),
                            cur_symbol.as_ref(),
                            a.get("name"),
                            a.get("direction"),
                        ) {
                            pin_directions.insert(
                                (library.clone(), symbol.clone(), pin.clone()),
                                dir.clone(),
                            );
                        }
                        if cur_symbol.is_some() {
                            cur_symbol_pins.total += 1;
                            if a.get("direction").map(String::as_str) == Some("sup") {
                                cur_symbol_pins
                                    .sup_names
                                    .push(a.get("name").cloned().unwrap_or_default());
                            }
                        }
                    }
                    b"deviceset" => {
                        cur_deviceset = a.get("name").cloned().map(|name| DevicesetScan {
                            name,
                            gate_symbols: Vec::new(),
                            any_device_has_package: false,
                        });
                    }
                    b"gate" => {
                        if let (Some(deviceset), Some(symbol)) =
                            (cur_deviceset.as_mut(), a.get("symbol"))
                        {
                            deviceset.gate_symbols.push(symbol.clone());
                            if let (Some(library), Some(gate)) =
                                (cur_library.as_ref(), a.get("name"))
                            {
                                gate_symbol_of.insert(
                                    (library.clone(), deviceset.name.clone(), gate.clone()),
                                    symbol.clone(),
                                );
                            }
                        }
                    }
                    b"device" => {
                        // A supply symbol is a schematic-only marker: it has no
                        // physical part behind it. Eagle writes a real component's
                        // variants as `<device name=".." package="..">`, and a
                        // supply symbol's as a bare `<device name="">` with no
                        // package at all. This is the second, independent test
                        // that keeps a connector or IC out of the supply set.
                        if let Some(deviceset) = cur_deviceset.as_mut() {
                            if a.get("package").is_some_and(|p| !p.trim().is_empty()) {
                                deviceset.any_device_has_package = true;
                            }
                        }
                        cur_device = Some(a.get("name").cloned().unwrap_or_default());
                    }
                    b"connect" => {
                        if let (
                            Some(library),
                            Some(deviceset),
                            Some(device),
                            Some(gate),
                            Some(pin),
                            Some(pads),
                        ) = (
                            cur_library.clone(),
                            cur_deviceset.as_ref(),
                            cur_device.as_ref(),
                            a.get("gate"),
                            a.get("pin"),
                            a.get("pad"),
                        ) {
                            let entry = device_pin_maps
                                .entry((
                                    library,
                                    deviceset.name.clone(),
                                    device.clone(),
                                    gate.clone(),
                                    pin.clone(),
                                ))
                                .or_default();
                            entry.extend(pads.split_whitespace().map(str::to_string));
                        }
                    }
                    b"part" => {
                        let (Some(name), Some(library), Some(deviceset)) =
                            (a.get("name"), a.get("library"), a.get("deviceset"))
                        else {
                            continue;
                        };
                        let library_id = a.get("library_urn").map_or_else(
                            || {
                                let candidates = libraries_by_name.get(library)?;
                                (candidates.len() == 1).then(|| candidates[0].clone())
                            },
                            |urn| {
                                Some(LibraryId {
                                    name: library.clone(),
                                    urn: Some(urn.clone()),
                                })
                            },
                        );
                        if let Some(supply) = library_id.clone().and_then(|library_id| {
                            supply_devicesets
                                .get(&(library_id, deviceset.clone()))
                                .cloned()
                        }) {
                            part_supply.insert(name.clone(), supply.clone());
                        } else {
                            let value = a.get("value").cloned().unwrap_or_default();
                            let device = a.get("device").map_or("", String::as_str);
                            let technology = a.get("technology").map_or("", String::as_str);
                            if let Some(number) =
                                embedded_part_number(deviceset, device, technology)
                            {
                                device_part_numbers.push((name.clone(), number));
                            }
                            if value.trim().is_empty() {
                                blank_value_device_names.push((
                                    name.clone(),
                                    eagle_device_name(deviceset, device, technology),
                                ));
                            }
                            physical_parts.push(SchematicPartIdentity {
                                reference: name.clone(),
                                value,
                            });
                            if let Some(library_id) = library_id {
                                physical_part_devices.insert(
                                    name.clone(),
                                    (
                                        library_id,
                                        deviceset.clone(),
                                        a.get("device").cloned().unwrap_or_default(),
                                    ),
                                );
                            }
                        }
                    }
                    b"net" => cur_net = a.get("name").cloned(),
                    b"segment" => {
                        if let Some(net) = cur_net.clone() {
                            cur_segment = Some(Segment {
                                net,
                                supply_parts: Vec::new(),
                            });
                        }
                    }
                    b"pinref" => {
                        if let (Some(segment), Some(part)) = (cur_segment.as_mut(), a.get("part")) {
                            if part_supply.contains_key(part) {
                                segment.supply_parts.push(part.clone());
                            } else if let (Some(gate), Some(pin), Some(net)) =
                                (a.get("gate"), a.get("pin"), cur_net.as_ref())
                            {
                                if let Some((library, deviceset, device)) =
                                    physical_part_devices.get(part)
                                {
                                    if let Some(pads) = device_pin_maps.get(&(
                                        library.clone(),
                                        deviceset.clone(),
                                        device.clone(),
                                        gate.clone(),
                                        pin.clone(),
                                    )) {
                                        physical_pin_nets.extend(pads.iter().map(|pad| {
                                            SchematicPinNetIdentity {
                                                reference: part.clone(),
                                                pin: pad.clone(),
                                                net: net.clone(),
                                            }
                                        }));
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
                if is_empty && e.name().as_ref() == b"device" {
                    cur_device = None;
                }
            }
            Ok(Event::End(e)) => {
                if e.name().as_ref() == b"schematic" {
                    if in_schematic
                        && element_path.len() == 3
                        && element_path[0].as_slice() == b"eagle"
                        && element_path[1].as_slice() == b"drawing"
                        && element_path[2].as_slice() == b"schematic"
                    {
                        in_schematic = false;
                        closed_schematic = true;
                    } else {
                        invalid_structure = true;
                    }
                    element_path.pop();
                    buf.clear();
                    continue;
                }
                if e.name().as_ref() == b"eagle" {
                    in_eagle = false;
                    element_path.pop();
                    buf.clear();
                    continue;
                }
                if !in_schematic {
                    element_path.pop();
                    buf.clear();
                    continue;
                }
                match e.name().as_ref() {
                    b"symbol" => {
                        if let (Some(library), Some(symbol), Some(net)) = (
                            cur_library.clone(),
                            cur_symbol.take(),
                            cur_symbol_pins.supply_net(),
                        ) {
                            supply_symbols.insert((library, symbol), net.to_string());
                        }
                        cur_symbol_pins = SymbolPins::default();
                    }
                    b"deviceset" => {
                        if let Some(deviceset) = cur_deviceset.take() {
                            // Exactly one gate, no packaged device, and that gate's
                            // symbol passed the pin test. All three, or nothing.
                            if !deviceset.any_device_has_package
                                && deviceset.gate_symbols.len() == 1
                            {
                                if let Some(library) = cur_library.clone() {
                                    let key = (library.clone(), deviceset.gate_symbols[0].clone());
                                    if let Some(supply) = supply_symbols.get(&key) {
                                        supply_devicesets
                                            .insert((library, deviceset.name), supply.clone());
                                    }
                                }
                            }
                        }
                    }
                    b"device" => cur_device = None,
                    b"library" => cur_library = None,
                    b"net" => cur_net = None,
                    b"segment" => {
                        if let Some(segment) = cur_segment.take() {
                            if !segment.supply_parts.is_empty() {
                                segments.push(segment);
                            }
                        }
                    }
                    _ => {}
                }
                element_path.pop();
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(ExtractError::Xml(format!(
                    "Eagle schematic is malformed: {e}"
                )))
            }
            _ => {}
        }
        buf.clear();
    }

    if !saw_eagle_root
        || !saw_schematic
        || in_eagle
        || in_schematic
        || !closed_schematic
        || invalid_structure
        || !element_path.is_empty()
    {
        return Err(ExtractError::WrongRoot {
            expected: "eagle schematic",
            found: None,
        });
    }
    Ok(Parsed {
        part_supply,
        segments,
        physical_parts,
        physical_pin_nets,
        part_devices: physical_part_devices,
        blank_value_device_names,
        device_part_numbers,
        device_pin_maps,
        pin_directions,
        gate_symbols: gate_symbol_of,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    /// A minimal Eagle 6 schematic: two supply symbols in one library, a parts
    /// list, and whatever nets the caller supplies.
    fn schematic(nets: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE eagle SYSTEM "eagle.dtd">
<eagle version="6.6.0">
<drawing>
<schematic>
<libraries>
<library name="supply1">
<symbols>
<symbol name="GND">
<pin name="GND" x="0" y="2.54" visible="off" length="short" direction="sup" rot="R270"/>
</symbol>
<symbol name="AGND">
<pin name="AGND" x="0" y="2.54" visible="off" length="short" direction="sup" rot="R270"/>
</symbol>
<symbol name="R">
<pin name="1" x="-5" y="0" visible="off" length="short"/>
<pin name="2" x="5" y="0" visible="off" length="short"/>
</symbol>
</symbols>
<devicesets>
<deviceset name="GND" prefix="SUPPLY">
<gates><gate name="GND" symbol="GND" x="0" y="0"/></gates>
</deviceset>
<deviceset name="AGND" prefix="AGND">
<gates><gate name="VR1" symbol="AGND" x="0" y="0"/></gates>
</deviceset>
<deviceset name="R" prefix="R">
<gates><gate name="G$1" symbol="R" x="0" y="0"/></gates>
</deviceset>
</devicesets>
</library>
</libraries>
<parts>
<part name="SUPPLY6" library="supply1" deviceset="GND" device=""/>
<part name="AGND7" library="supply1" deviceset="AGND" device=""/>
<part name="R1" library="supply1" deviceset="R" device=""/>
</parts>
<sheets>
<sheet>
<nets>
{nets}
</nets>
</sheet>
</sheets>
</schematic>
</drawing>
</eagle>
"#
        )
    }

    /// The ties [`declared_net_ties`] reads from `text`, which must parse.
    fn ties_in(text: &str) -> Vec<DeclaredNetTie> {
        declared_net_ties(text).expect("parses")
    }

    #[test]
    fn a_foreign_supply_symbol_on_a_net_declares_a_tie() {
        // The emonTx construct, distilled: an AGND supply symbol wired to a GND
        // supply symbol inside one segment of net GND.
        let ties = ties_in(&schematic(
            r#"<net name="GND" class="0">
<segment>
<pinref part="SUPPLY6" gate="GND" pin="GND"/>
<pinref part="AGND7" gate="VR1" pin="AGND"/>
<junction x="-31.75" y="365.76"/>
</segment>
</net>"#,
        ));
        assert_eq!(ties.len(), 1, "one declaration, got {ties:?}");
        assert_eq!(ties[0].net, "GND");
        assert_eq!(ties[0].tied_net, "AGND");
        assert_eq!(ties[0].symbol, "AGND7");
        assert_eq!(ties[0].tied_to, ["SUPPLY6"]);
        assert_eq!(ties[0].describe(), "AGND7 wired to SUPPLY6 in net GND");
        assert!(ties[0].covers("GND", "AGND"), "matches in file order");
        assert!(ties[0].covers("AGND", "GND"), "and reversed");
        assert!(!ties[0].covers("GND", "+5V"), "and nothing else");
    }

    #[test]
    fn a_tie_in_one_segment_does_not_travel_to_another_net() {
        // Two nets each carrying their own supply symbol, plus a third net that
        // ties nothing: the declaration is per segment, so only the segment
        // that actually holds the foreign symbol declares anything.
        let ties = ties_in(&schematic(
            r#"<net name="AGND" class="0">
<segment><pinref part="AGND7" gate="VR1" pin="AGND"/><pinref part="SUPPLY6" gate="GND" pin="GND"/></segment>
</net>
<net name="GND" class="0">
<segment><pinref part="R1" gate="G$1" pin="1"/></segment>
</net>"#,
        ));
        assert_eq!(ties.len(), 1);
        // Declared from AGND's side this time: net AGND, foreign symbol SUPPLY6.
        assert_eq!(ties[0].net, "AGND");
        assert_eq!(ties[0].tied_net, "GND");
        assert_eq!(ties[0].symbol, "SUPPLY6");
        assert_eq!(ties[0].tied_to, ["AGND7"]);
    }

    /// A schematic whose library also holds a REAL multi-pin component that marks
    /// its power pins `direction="sup"`, which is what ordinary Eagle libraries
    /// do. `nets` is spliced in as written.
    fn schematic_with_powered_component(nets: &str) -> String {
        // Shaped after the `SD-MMC` symbol in margay_logger/Hardware/Margay.sch
        // (13 pins, 4 of them `sup`) and `XBEE` in emonTx V3.2.sch (20 pins, 2
        // `sup`): a physical part whose supply pins carry the same marker a
        // supply symbol uses.
        let component = r#"
<symbol name="SD-MMC">
<pin name="DAT2" x="-12.7" y="0" length="short"/>
<pin name="VDD" x="-12.7" y="-7.62" length="short" direction="sup"/>
<pin name="VSS1" x="-12.7" y="-10.16" length="short" direction="sup"/>
<pin name="VSS2" x="-12.7" y="-12.7" length="short" direction="sup"/>
<pin name="GND" x="-12.7" y="-17.78" length="short" direction="sup"/>
</symbol>
"#;
        let deviceset = r#"
<deviceset name="SDMMC" prefix="J">
<gates><gate name="G$1" symbol="SD-MMC" x="0" y="0"/></gates>
<devices>
<device name="WR-CRD" package="SD_WURTH_WR-CRD">
<technologies><technology name=""/></technologies>
</device>
</devices>
</deviceset>
"#;
        schematic(nets)
            .replace("</symbols>", &format!("{component}</symbols>"))
            .replace("</devicesets>", &format!("{deviceset}</devicesets>"))
            .replace(
                "</parts>",
                "<part name=\"J1\" library=\"supply1\" deviceset=\"SDMMC\" device=\"WR-CRD\"/>\n</parts>",
            )
    }

    #[test]
    fn schematics_without_a_supply_symbol_joining_two_nets_declare_nothing() {
        let packaged_testpoint = schematic(
            r#"<net name="GND" class="0">
<segment><pinref part="TP1" gate="G$1" pin="AGND"/><pinref part="SUPPLY6" gate="GND" pin="GND"/></segment>
</net>"#,
        )
        .replace(
            "</devicesets>",
            r#"<deviceset name="TESTPOINT" prefix="TP">
<gates><gate name="G$1" symbol="AGND" x="0" y="0"/></gates>
<devices><device name="" package="TP_PAD"><technologies><technology name=""/></technologies></device></devices>
</deviceset>
</devicesets>"#,
        )
        .replace(
            "</parts>",
            "<part name=\"TP1\" library=\"supply1\" deviceset=\"TESTPOINT\" device=\"\"/>\n</parts>",
        );
        let cases = [
            // The overwhelmingly common case, and the false-negative guard: a
            // board whose grounds are separate must produce no exemption.
            (
                "supply symbols on their own nets",
                schematic(
                    r#"<net name="GND" class="0">
<segment><pinref part="SUPPLY6" gate="GND" pin="GND"/><pinref part="R1" gate="G$1" pin="1"/></segment>
</net>
<net name="AGND" class="0">
<segment><pinref part="AGND7" gate="VR1" pin="AGND"/><pinref part="R1" gate="G$1" pin="2"/></segment>
</net>"#,
                ),
            ),
            // THE false-negative guard on the recogniser itself. An SD socket
            // sitting on a signal net must not "declare" that signal tied to GND:
            // doing so would attach false intent context to a genuine
            // rail-to-ground short. Measured on the real files this fixture is
            // modelled on, the earlier any-`sup`-pin rule invented 6 ties on
            // Margay and 19 on emonTx V3.2, including 3.3V/GND. J1's own GND pin
            // sits on a signal net beside only an ordinary component.
            (
                "an SD socket is a component, not a supply symbol",
                schematic_with_powered_component(
                    r#"<net name="MISO" class="0">
<segment><pinref part="J1" gate="G$1" pin="GND"/><pinref part="R1" gate="G$1" pin="1"/></segment>
</net>"#,
                ),
            ),
            // Even a ONE-pin symbol whose single pin is `sup` is not a supply
            // marker if a physical package sits behind it: a supply symbol is
            // schematic-only. Eagle writes a real part's variants as
            // `<device name=".." package="..">` and a supply symbol's as a bare
            // `<device name="">`, which is the discriminator used here.
            ("a packaged testpoint declares nothing", packaged_testpoint),
            // R1's pins carry no `direction="sup"`, so two nets bridged by a
            // resistor symbol are NOT declared tied. A resistor really joining
            // two nets is a component, and the DRC must keep reporting copper
            // that bypasses it.
            (
                "an ordinary component is not a supply symbol",
                schematic(
                    r#"<net name="GND" class="0">
<segment><pinref part="R1" gate="G$1" pin="1"/><pinref part="R1" gate="G$1" pin="2"/></segment>
</net>"#,
                ),
            ),
        ];
        for (why, text) in cases {
            let ties = ties_in(&text);
            assert!(ties.is_empty(), "{why}: {ties:?}");
        }
    }

    #[test]
    fn a_real_supply_symbol_still_declares_beside_a_powered_component() {
        // And the recogniser is not merely off: with the SD socket present in the
        // same library, the genuine AGND-on-GND declaration is still found.
        let ties = ties_in(&schematic_with_powered_component(
            r#"<net name="GND" class="0">
<segment>
<pinref part="SUPPLY6" gate="GND" pin="GND"/>
<pinref part="AGND7" gate="VR1" pin="AGND"/>
<pinref part="J1" gate="G$1" pin="GND"/>
</segment>
</net>"#,
        ));
        assert_eq!(ties.len(), 1, "got {ties:?}");
        assert_eq!(ties[0].describe(), "AGND7 wired to SUPPLY6 in net GND");
        assert_eq!(
            ties[0].tied_to,
            ["SUPPLY6"],
            "J1 is not a supply symbol, so it is not listed as what the tie joins"
        );
    }

    #[test]
    fn a_supply_symbol_alone_on_a_named_net_still_declares_the_tie() {
        // The label form: net named by a <label>, not by a second supply
        // symbol, so there is nothing to report as "wired to".
        let ties = ties_in(&schematic(
            r#"<net name="VBUS" class="0">
<segment><pinref part="SUPPLY6" gate="GND" pin="GND"/><label x="0" y="0" size="1.778" layer="95"/></segment>
</net>"#,
        ));
        assert_eq!(ties.len(), 1);
        assert_eq!(ties[0].tied_to, Vec::<String>::new());
        assert_eq!(ties[0].describe(), "SUPPLY6 placed on net VBUS");
    }

    #[test]
    fn a_board_is_not_a_schematic() {
        assert!(!looks_like_eagle_schematic(
            r#"<?xml version="1.0"?><eagle version="6.6.0"><drawing><board><signals/></board></drawing></eagle>"#
        ));
        assert!(looks_like_eagle_schematic(&schematic("")));
    }

    #[test]
    fn a_schematic_is_recognised_behind_a_long_settings_and_layers_preamble() {
        // Regression. Eagle writes `<settings>`, `<grid>` and the whole
        // `<layers>` table before `<schematic>`: on the real emonTx V3.4.5 file
        // the root element is at byte 75 and `<schematic>` at 10856. A head-slice
        // check for `<schematic>` therefore rejected a valid schematic, which
        // silently disabled the entire declared-tie pathway on the board it was
        // written for, while every small fixture kept passing.
        let preamble = (0..400).fold(String::new(), |mut s, n| {
            let _ = writeln!(
                s,
                "<layer number=\"{n}\" name=\"L{n}\" color=\"4\" fill=\"1\" \
                 visible=\"yes\" active=\"yes\"/>"
            );
            s
        });
        assert!(
            preamble.len() > 8192,
            "the preamble must exceed any plausible head slice, is {}",
            preamble.len()
        );
        let text = schematic("").replace("<schematic>", &format!("{preamble}<schematic>"));
        assert!(
            text.find("<schematic").expect("present") > 8192,
            "the fixture must put <schematic> past the slice"
        );
        assert!(looks_like_eagle_schematic(&text));

        // And the parse still finds the parts and nets behind that preamble.
        let ties = ties_in(
            &schematic(
                r#"<net name="GND" class="0">
<segment><pinref part="SUPPLY6" gate="GND" pin="GND"/><pinref part="AGND7" gate="VR1" pin="AGND"/></segment>
</net>"#,
            )
            .replace("<schematic>", &format!("{preamble}<schematic>")));
        assert_eq!(ties.len(), 1);
        assert_eq!(ties[0].describe(), "AGND7 wired to SUPPLY6 in net GND");
    }

    #[test]
    fn malformed_xml_is_an_error_not_an_empty_answer() {
        // A companion the user explicitly supplied must never be silently
        // half-read: "no ties declared" and "could not read it" are different
        // claims. Neither one authorizes a board location or downgrades a short.
        let err = declared_net_ties("<eagle><drawing><schematic><parts><part name=")
            .expect_err("a truncated attribute is an error");
        assert!(
            format!("{err}").contains("malformed"),
            "unhelpful error: {err}"
        );

        // And a file that is XML but not an Eagle document at all.
        let err = declared_net_ties("<?xml version=\"1.0\"?><kicad_sch/>")
            .expect_err("a non-Eagle root is an error");
        assert!(
            format!("{err}").contains("eagle schematic"),
            "unhelpful error: {err}"
        );
    }

    #[test]
    fn a_schematic_tag_that_is_not_the_drawing_schematic_is_refused() {
        for text in [
            // A `<schematic>` inside a comment is text, not an element.
            r#"<?xml version="1.0"?><eagle><drawing><board><!-- <schematic> --></board></drawing></eagle>"#,
            // A lookalike element name sharing the prefix.
            r#"<?xml version="1.0"?><eagle><drawing><schematic_backup/></drawing></eagle>"#,
            // A real `<schematic>` element outside the eagle/drawing path.
            r#"<?xml version="1.0"?>
<eagle><compatibility><schematic><libraries/><parts/><sheets/></schematic></compatibility></eagle>"#,
        ] {
            assert!(!looks_like_eagle_schematic(text), "{text}");
            assert!(declared_net_ties(text).is_err(), "{text}");
        }
    }

    #[test]
    fn only_nodes_inside_the_real_schematic_are_read() {
        let decoy = r#"<?xml version="1.0"?>
<eagle><drawing>
  <board>
    <libraries><library name="decoy"><symbols><symbol name="GND"><pin name="GND" direction="sup"/></symbol></symbols><devicesets><deviceset name="GND"><gates><gate name="G" symbol="GND"/></gates></deviceset></devicesets></library></libraries>
    <parts><part name="P1" library="decoy" deviceset="GND"/></parts>
    <net name="VBUS"><segment><pinref part="P1"/></segment></net>
  </board>
  <schematic><libraries/><parts/><sheets/></schematic>
</drawing></eagle>"#;
        assert!(looks_like_eagle_schematic(decoy));
        assert!(
            declared_net_ties(decoy)
                .expect("the real schematic parses")
                .is_empty(),
            "board-side decoy nodes are outside the schematic and cannot declare a tie"
        );
    }

    #[test]
    fn a_part_uses_library_urn_when_same_named_libraries_collide() {
        let text = r#"<?xml version="1.0"?>
<eagle><drawing><schematic>
<libraries>
  <library name="shared" urn="urn:adsk.eagle:library:1">
    <symbols><symbol name="MARK"><pin name="GND" direction="sup"/></symbol></symbols>
    <devicesets><deviceset name="MARK"><gates><gate name="G" symbol="MARK"/></gates></deviceset></devicesets>
  </library>
  <library name="shared" urn="urn:adsk.eagle:library:2">
    <symbols><symbol name="MARK"><pin name="1"/><pin name="2"/></symbol></symbols>
    <devicesets><deviceset name="MARK"><gates><gate name="G" symbol="MARK"/></gates><devices><device name="" package="R0603"/></devices></deviceset></devicesets>
  </library>
</libraries>
<parts><part name="R1" library="shared" library_urn="urn:adsk.eagle:library:2" deviceset="MARK" device=""/></parts>
<sheets><sheet><nets><net name="VBUS"><segment><pinref part="R1" gate="G" pin="1"/></segment></net></nets></sheet></sheets>
</schematic></drawing></eagle>"#;
        assert!(
            declared_net_ties(text)
                .expect("managed-library schematic parses")
                .is_empty(),
            "the ordinary packaged part in library URN 2 must not inherit URN 1's supply symbol"
        );
    }

    /// Pad names come from each part's own device variant's `<connect>` map:
    /// `@n` duplicates collapse to the pin name, a multi-pad pin names every
    /// pad, and a sibling variant of the same deviceset does not leak in.
    #[test]
    fn pad_names_come_from_the_parts_own_device_connects() {
        let text = r#"<?xml version="1.0"?>
<eagle><drawing><schematic>
<libraries><library name="mods">
  <symbols><symbol name="M"><pin name="GND@1"/><pin name="GND@2"/><pin name="IO2"/></symbol></symbols>
  <devicesets><deviceset name="WROOM"><gates><gate name="G$1" symbol="M"/></gates><devices>
    <device name="SKINNY" package="P38"><connects>
      <connect gate="G$1" pin="GND@1" pad="1"/><connect gate="G$1" pin="GND@2" pad="38 39"/>
      <connect gate="G$1" pin="IO2" pad="24"/></connects></device>
    <device name="WIDE" package="P40"><connects>
      <connect gate="G$1" pin="IO2" pad="99"/></connects></device>
  </devices></deviceset></devicesets>
</library></libraries>
<parts><part name="X2" library="mods" deviceset="WROOM" device="SKINNY"/></parts>
<sheets/>
</schematic></drawing></eagle>"#;
        let names = schematic_pad_names(text).expect("parses");
        let own = |pad: &str, name: &str| ("X2".to_string(), pad.to_string(), name.to_string());
        assert_eq!(
            names,
            [
                own("1", "GND"),
                own("24", "IO2"),
                own("38", "GND"),
                own("39", "GND")
            ]
        );
        assert!(
            schematic_pad_names("<eagle><drawing><board/></drawing></eagle>")
                .map_or(true, |v| v.is_empty())
        );
    }

    /// Each pad's electrical kind is its symbol pin's library `direction`,
    /// reached through the gate the pad's `<connect>` names: a transceiver's
    /// `TXD` declared `in` is an input whatever its name suggests, and a pin
    /// with no direction stays unknown rather than guessed.
    #[test]
    fn pad_kinds_come_from_the_symbol_pin_direction() {
        let text = r#"<?xml version="1.0"?>
<eagle><drawing><schematic>
<libraries><library name="ifc">
  <symbols><symbol name="XCVR"><pin name="TXD" direction="in"/><pin name="RXD" direction="out"/>
    <pin name="VCC" direction="pwr"/><pin name="FAULT" direction="oc"/><pin name="X"/></symbol></symbols>
  <devicesets><deviceset name="XCVR"><gates><gate name="A" symbol="XCVR"/></gates><devices>
    <device name="" package="SO8"><connects>
      <connect gate="A" pin="TXD" pad="1"/><connect gate="A" pin="RXD" pad="4"/>
      <connect gate="A" pin="VCC" pad="3"/><connect gate="A" pin="FAULT" pad="5"/>
      <connect gate="A" pin="X" pad="8"/></connects></device>
  </devices></deviceset></devicesets>
</library></libraries>
<parts><part name="U7" library="ifc" deviceset="XCVR" device=""/></parts>
<sheets/>
</schematic></drawing></eagle>"#;
        let pins = schematic_pad_pins(text).expect("parses");
        let kind = |pad: &str| {
            pins.iter()
                .find(|p| p.0 == "U7" && p.1 == pad)
                .map(|p| p.3.clone())
                .expect("pad present")
        };
        assert_eq!(kind("1"), "input");
        assert_eq!(kind("4"), "output");
        assert_eq!(kind("3"), "power_in");
        assert_eq!(kind("5"), "open_collector");
        assert_eq!(kind("8"), "", "an undeclared direction is not guessed");
    }
}
