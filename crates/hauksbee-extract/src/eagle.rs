//! Extraction from Eagle `.brd` board files (XML, Eagle 6+). Covers the
//! large legacy ecosystem: Arduino, Adafruit, SparkFun designs.
//!
//! Connectivity in a `.brd` is explicit: `<signals><signal name="GND">
//! <contactref element="C1" pad="1"/>...`. Component placement comes from
//! `<elements>`, pad offsets from `<packages>`.

use crate::{Component, ExtractError, ExtractedBoard, Net, Pin};
use hauksbee_ir::eprintln;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;

/// One XML element's attributes as a map, with entities unescaped.
///
/// quick-xml does NOT unescape attribute values, so an `&amp;`/`&lt;`/`&#38;` in
/// an Eagle net name, value or reference would otherwise be stored literally and
/// never match the same net's spelling elsewhere. Falls back to the raw bytes
/// only on a decode error.
///
/// quick-xml deprecates `unescape_value` in favour of `normalized_value`, which
/// takes an `XmlVersion` the crate does not export, so the replacement is not
/// callable from outside quick-xml.
pub fn xml_attrs(e: &quick_xml::events::BytesStart) -> HashMap<String, String> {
    e.attributes()
        .flatten()
        .map(|a| {
            #[allow(deprecated)]
            let value = a.unescape_value().map_or_else(
                |_| String::from_utf8_lossy(&a.value).into_owned(),
                std::borrow::Cow::into_owned,
            );
            (String::from_utf8_lossy(a.key.as_ref()).into_owned(), value)
        })
        .collect()
}

/// Whether an element `<attribute>` marks the part not fitted: the
/// assembly-variant `POPULATE` attribute set false (`0`/`no`/`false`), or a
/// `DNP` / `NOPOP` / `DO_NOT_POPULATE` attribute set true. Eagle libraries
/// and CAM flows use these alongside the element's own `populate="no"`.
fn attribute_marks_dnp(name: &str, value: &str) -> bool {
    let v = value.trim().to_ascii_lowercase();
    let falsy = matches!(v.as_str(), "0" | "no" | "false" | "n");
    let truthy = matches!(v.as_str(), "1" | "yes" | "true" | "y" | "dnp");
    match name.trim().to_ascii_uppercase().as_str() {
        "POPULATE" => falsy,
        "DNP" | "NOPOP" | "DO_NOT_POPULATE" | "DONOTPOPULATE" => truthy,
        _ => false,
    }
}
/// An element `<attribute>` worth keeping as a component property: one with a
/// non-empty value that is not the `NAME`/`VALUE` text placeholder (those
/// repeat the element's own reference and value). Library attributes such as
/// `MPN`, `PART_NUMBER` or `MANUFACTURER-PN` carry the part's identity when the
/// value field holds a rating string or a stale part number; dropping them left
/// the binder only the value to resolve against.
fn element_property(name: &str, value: &str) -> Option<(String, String)> {
    let value = value.trim();
    let placeholder = name.eq_ignore_ascii_case("NAME") || name.eq_ignore_ascii_case("VALUE");
    (!value.is_empty() && !placeholder).then(|| (name.to_string(), value.to_string()))
}
#[allow(clippy::too_many_lines, reason = "one streaming pass over the XML")]
pub fn extract(text: &str) -> Result<ExtractedBoard, ExtractError> {
    type Pads = Vec<(String, f64, f64)>;
    struct El {
        name: String,
        library: String,
        package: String,
        value: String,
        x: f64,
        y: f64,
        rot_deg: f64,
        mirrored: bool,
        dnp: bool,
        properties: Vec<(String, String)>,
    }
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);

    // (library name, package name) -> [(pad name, dx, dy)]. Eagle namespaces
    // packages per <library>: two embedded libraries may each define a package
    // named e.g. "0805" with different pads. Keying by bare package name merged
    // them, so an element resolved to the concatenation of both libraries' pads
    // and was emitted with doubled/mixed pins.
    let mut packages: HashMap<(String, String), Pads> = HashMap::new();
    let mut cur_library = String::new();
    let mut cur_package: Option<String> = None;
    // elements
    let mut elements: Vec<El> = Vec::new();
    // signals
    let mut signals: Vec<(String, Vec<(String, String)>)> = Vec::new();
    let mut saw_eagle_root = false;
    // Inside an `<element>`: its `<attribute>` children belong to the
    // element just pushed.
    let mut in_element = false;

    let mut guard = XmlGuard::default();
    let mut saw_board = false;
    let mut saw_schematic = false;
    let mut buf = Vec::new();
    loop {
        // Its own block: the event borrows `buf`, which is cleared below.
        {
            let event = reader.read_event_into(&mut buf);
            if let Ok(seen) = &event {
                guard.observe(seen)?;
            }
            match event {
                Ok(Event::Start(e) | Event::Empty(e)) => {
                    let attrs = xml_attrs;
                    match e.name().as_ref() {
                        b"eagle" => saw_eagle_root = true,
                        b"board" => saw_board = true,
                        b"schematic" => saw_schematic = true,
                        b"library" => {
                            cur_library = attrs(&e).get("name").cloned().unwrap_or_default();
                        }
                        b"package" => {
                            let a = attrs(&e);
                            cur_package = a.get("name").cloned();
                            if let Some(name) = &cur_package {
                                packages
                                    .entry((cur_library.clone(), name.clone()))
                                    .or_default();
                            }
                        }
                        b"pad" | b"smd" => {
                            if let Some(pkg) = &cur_package {
                                let a = attrs(&e);
                                if let Some(bad) = non_finite_coord(&a) {
                                    return Err(corrupt_coord("pad", &bad.0, &bad.1));
                                }
                                let (Some(name), Some(x), Some(y)) =
                                    (a.get("name"), num(&a, "x"), num(&a, "y"))
                                else {
                                    continue;
                                };
                                packages
                                    .entry((cur_library.clone(), pkg.clone()))
                                    .or_default()
                                    .push((name.clone(), x, y));
                            }
                        }
                        b"element" => {
                            let a = attrs(&e);
                            if let Some(bad) = non_finite_coord(&a) {
                                return Err(corrupt_coord("element", &bad.0, &bad.1));
                            }
                            let rot = a.get("rot").map_or("R0", String::as_str);
                            in_element = true;
                            elements.push(El {
                                name: a.get("name").cloned().unwrap_or_default(),
                                library: a.get("library").cloned().unwrap_or_default(),
                                package: a.get("package").cloned().unwrap_or_default(),
                                value: a.get("value").cloned().unwrap_or_default(),
                                x: num(&a, "x").unwrap_or(0.0),
                                y: num(&a, "y").unwrap_or(0.0),
                                rot_deg: rot
                                    .trim_start_matches(['M', 'S', 'R'])
                                    .parse()
                                    .unwrap_or(0.0),
                                // Eagle can prefix the rotation with 'S' (spin) and
                                // 'M' (mirror) in either order, "MR90", "SMR90".
                                // `starts_with('M')` missed the spin-prefixed
                                // "SMR90" form, disagreeing with drc.rs (contains).
                                mirrored: rot.contains('M'),
                                // Eagle marks a do-not-populate / assembly-variant
                                // part with populate="no" on the element. Without
                                // this every Eagle part read as populated, unlike
                                // the KiCad readers which thread the analogous field.
                                dnp: a
                                    .get("populate")
                                    .is_some_and(|p| p.eq_ignore_ascii_case("no")),
                                properties: Vec::new(),
                            });
                        }
                        b"attribute" if in_element => {
                            let a = attrs(&e);
                            if let (Some(el), Some(name), Some(value)) =
                                (elements.last_mut(), a.get("name"), a.get("value"))
                            {
                                el.dnp |= attribute_marks_dnp(name, value);
                                if let Some(prop) = element_property(name, value) {
                                    el.properties.push(prop);
                                }
                            }
                        }
                        b"signal" => {
                            let a = attrs(&e);
                            signals.push((a.get("name").cloned().unwrap_or_default(), Vec::new()));
                        }
                        b"contactref" => {
                            let a = attrs(&e);
                            if let (Some(sig), Some(el), Some(pad)) =
                                (signals.last_mut(), a.get("element"), a.get("pad"))
                            {
                                sig.1.push((el.clone(), pad.clone()));
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::End(e)) if e.name().as_ref() == b"package" => cur_package = None,
                Ok(Event::End(e)) if matches!(e.name().as_ref(), b"element" | b"elements") => {
                    in_element = false;
                }
                Ok(Event::End(e)) if e.name().as_ref() == b"library" => cur_library.clear(),
                Ok(Event::Eof) => break,
                Err(err) => return Err(xml_error(&err)),
                _ => {}
            }
        }
        buf.clear();
    }

    if !saw_eagle_root {
        return Err(ExtractError::WrongRoot {
            expected: "eagle",
            found: None,
        });
    }
    // A file cut short parses cleanly up to the cut, so without this its nets
    // and parts would be the first half of a board, read as the whole one.
    guard.finish()?;
    if saw_schematic && !saw_board {
        return Err(eagle_schematic_alone());
    }

    let nets: Vec<Net> = signals
        .iter()
        .zip(1_i64..)
        .map(|((name, _), id)| Net::new(id, name.clone()))
        .collect();

    // (element, pad) -> net id
    let mut pad_net: HashMap<(String, String), i64> = HashMap::new();
    for ((_, refs), id) in signals.iter().zip(1_i64..) {
        for (el, pad) in refs {
            pad_net.insert((el.clone(), pad.clone()), id);
        }
    }

    // `package` is required on an Eagle <element>; when it is absent the lookup
    // below misses and the part lands with zero pins (its own connectivity
    // silently lost, though the nets it touched survive via <contactref>). Only
    // reachable with schema-invalid XML, so surface it rather than fail, a
    // silent zero-pin component is the confusing symptom.
    let missing_pkg = elements.iter().filter(|e| e.package.is_empty()).count();
    if missing_pkg > 0 {
        eprintln!(
            "hauksbee: {missing_pkg} Eagle element(s) have no `package` attribute \
             (schema-invalid); their pins could not be placed"
        );
    }

    let components = elements
        .into_iter()
        .map(|el| {
            // A mirrored element (`MR<deg>`) reflects about Y (negate local X)
            // and then rotates by `-deg`, mirroring flips the sense of
            // rotation. Rotating by `+deg` here diverged from the
            // corpus-validated placement in drc.rs (place_pkg_item) for any
            // `MR<deg>` whose deg is not a multiple of 180 (e.g. MR90 put pads
            // on the wrong side of the origin); the two forms coincide only
            // when the rotation absorbs the sign.
            let eff_rot = if el.mirrored { -el.rot_deg } else { el.rot_deg };
            let (sin, cos) = eff_rot.to_radians().sin_cos();
            let pins = packages
                .get(&(el.library.clone(), el.package.clone()))
                .map(|pads| {
                    pads.iter()
                        .map(|(pname, dx, dy)| {
                            let dx = if el.mirrored { -dx } else { *dx };
                            // Eagle rotation is counter-clockwise, y up.
                            let abs = (
                                dx.mul_add(cos, el.x) - dy * sin,
                                dx.mul_add(sin, el.y) + dy * cos,
                            );
                            Pin {
                                number: pname.clone(),
                                net: pad_net.get(&(el.name.clone(), pname.clone())).copied(),
                                position: Some(abs),
                                ..Default::default()
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            Component {
                reference: el.name,
                value: el.value,
                lib_id: format!("{}:{}", el.library, el.package),
                footprint: el.package,
                position: Some((el.x, el.y, el.rot_deg)),
                layer: if el.mirrored { "B.Cu" } else { "F.Cu" }.to_string(),
                properties: el.properties,
                dnp: el.dnp,
                pins,
            }
        })
        .collect();

    Ok(ExtractedBoard {
        name: String::new(),
        nets,
        components,
    })
}

/// Deepest element nesting an Eagle file is read at. Eagle itself writes about
/// a dozen levels (`eagle > drawing > board > libraries > library > packages >
/// package > smd`), so anything past this is a damaged or hostile file.
const MAX_XML_DEPTH: usize = 64;

/// Longest single attribute value an Eagle file is read with. Eagle writes
/// values of a few hundred bytes at most; a megabyte attribute is not a board.
const MAX_ATTR_BYTES: usize = 64 * 1024;

/// Watches an Eagle XML event stream for what a streaming parse otherwise
/// accepts without a word: nesting deep enough to exhaust a reader, an
/// attribute too large to be a board's, an entity declaration that could pull
/// in another file or expand without bound, and a document that ends before
/// its root element closes.
///
/// Each is refused with a sentence naming the cause, instead of the file
/// reading as a board with nothing in it ("parsed, but is empty") or, for a
/// cut-short file, as a board with only the part that survived.
#[derive(Default)]
struct XmlGuard {
    open: Vec<String>,
}

impl XmlGuard {
    /// Account for one event, refusing the file when it breaks a limit.
    fn observe(&mut self, event: &Event<'_>) -> Result<(), ExtractError> {
        match event {
            Event::Start(e) => {
                check_attribute_sizes(e)?;
                if self.open.len() >= MAX_XML_DEPTH {
                    return Err(ExtractError::Corrupt(format!(
                        "this Eagle file nests XML elements more than {MAX_XML_DEPTH} levels \
                         deep (Eagle itself writes about a dozen), so hauksbee refused to read \
                         it: runaway nesting is how a damaged or hostile file exhausts a \
                         parser. Re-save the board from Eagle and retry; if you did not expect \
                         this file, do not trust it"
                    )));
                }
                self.open
                    .push(String::from_utf8_lossy(e.name().as_ref()).into_owned());
            }
            Event::Empty(e) => check_attribute_sizes(e)?,
            Event::End(_) => {
                self.open.pop();
            }
            Event::DocType(d) if d.windows(b"<!ENTITY".len()).any(|w| w == b"<!ENTITY") => {
                return Err(ExtractError::Corrupt(xml_entities_message()));
            }
            _ => {}
        }
        Ok(())
    }

    /// At the end of the document: refuse it when elements are still open.
    fn finish(&self) -> Result<(), ExtractError> {
        self.open.last().map_or(Ok(()), |last| {
            Err(truncated(&format!(
                "the XML ends while {} element(s) are still open, the innermost <{last}>",
                self.open.len()
            )))
        })
    }
}

/// The refusal for an XML document that declares entities. Shared with the
/// format sniffer, which meets the same declarations in a file whose `<eagle>`
/// root sits past the sniff window.
#[must_use]
pub fn xml_entities_message() -> String {
    "this file declares XML entities in its DOCTYPE (<!ENTITY ...>). Eagle never writes \
     these, and an entity can pull in another file from this machine or expand into \
     gigabytes, so hauksbee refused to read the file rather than resolve them. Re-save the \
     board from Eagle (which drops the declaration) and retry; if you did not expect this \
     file, do not trust it"
        .to_string()
}

fn check_attribute_sizes(e: &quick_xml::events::BytesStart) -> Result<(), ExtractError> {
    e.attributes()
        .flatten()
        .find(|a| a.value.len() > MAX_ATTR_BYTES)
        .map_or(Ok(()), |a| {
            Err(ExtractError::Corrupt(format!(
                "this Eagle file has an attribute ({}=\"...\") of {} KiB, past the {} KiB \
                 hauksbee reads (Eagle writes values of a few hundred bytes at most), so it \
                 was refused for safety, not parsed. Re-save the board from Eagle and retry; \
                 if you did not expect this file, do not trust it",
                String::from_utf8_lossy(a.key.as_ref()),
                a.value.len() / 1024,
                MAX_ATTR_BYTES / 1024,
            )))
        })
}

/// The refusal for an Eagle file whose XML ends early. Reading what survives
/// would give a board with some of its nets and parts and no sign of the rest.
fn truncated(how: &str) -> ExtractError {
    ExtractError::Corrupt(format!(
        "this Eagle file is truncated: {how}. It was probably cut short by an \
         interrupted save, copy or download, and hauksbee will not report on the part that \
         survived as if it were the whole board. Re-save or re-copy the file from Eagle and \
         retry"
    ))
}

/// A quick-xml error as the user should hear it: a syntax error is the input
/// ending inside a tag, comment or quoted value (the parser only meets these at
/// the end of the input), which is a cut-short file; anything else keeps the
/// parser's own wording.
fn xml_error(err: &quick_xml::Error) -> ExtractError {
    match err {
        quick_xml::Error::Syntax(inner) => truncated(&format!("the XML ends mid-way ({inner})")),
        other => ExtractError::Xml(other.to_string()),
    }
}

/// The refusal for an Eagle schematic handed over as the board.
fn eagle_schematic_alone() -> ExtractError {
    ExtractError::Corrupt(format!(
        "this is an Eagle schematic (.sch), not a board: it holds the circuit but no \
         placement or copper, so there is nothing here to check for ordering. Pass the \
         design's board file (.brd) instead; its .sch is read as well when it sits beside the \
         .brd with the same name, or can be named with --schematic. See {}",
        hauksbee_ir::docs_url("docs/ingest/EAGLE.md")
    ))
}

/// A coordinate is only usable if it is a finite number. Rust's `f64` parser
/// accepts "NaN", "inf" and an exponent that overflows to infinity, and any of
/// those poisons every later distance comparison: a pad at NaN is neither near
/// nor far from anything, so the clearance check reports a clean board. Treat
/// them as absent here and refuse at the call site.
fn num(attrs: &HashMap<String, String>, key: &str) -> Option<f64> {
    attrs
        .get(key)?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

/// Coordinate attributes on Eagle geometry elements.
const COORD_ATTRS: &[&str] = &[
    "x", "y", "x1", "y1", "x2", "y2", "dx", "dy", "drill", "diameter",
];

/// The first coordinate attribute present but not a finite number, as
/// (attribute, raw value).
fn non_finite_coord(attrs: &HashMap<String, String>) -> Option<(String, String)> {
    for key in COORD_ATTRS {
        if let Some(raw) = attrs.get(*key) {
            if raw.parse::<f64>().is_ok_and(|v| !v.is_finite()) {
                return Some(((*key).to_string(), raw.clone()));
            }
        }
    }
    None
}

fn corrupt_coord(tag: &str, attr: &str, raw: &str) -> ExtractError {
    ExtractError::Corrupt(format!(
        "board geometry is corrupt: an Eagle <{tag}> has {attr}=\"{raw}\", which is not \
         a finite number. Distances cannot be compared against it, so a clearance check \
         would report a meaningless pass. Re-save the board from Eagle, or fix that \
         {attr} value by hand"
    ))
}

/// True for a pre-Eagle-6 `.brd` / `.sch`, which is a BINARY format this
/// reader does not parse.
///
/// Eagle moved to XML in version 6. Every earlier drawing opens with a 24-byte
/// drawing record whose first byte is the tag `0x10` and whose second byte is
/// the format era: `0x80` for the Eagle 3.x layout and `0x00` for 4.x/5.x.
/// Those TWO bytes are the whole magic, which is also how KiCad's binary Eagle
/// importer keys the two branches of its reader.
///
/// Byte 2 is NOT part of the magic. It is a per-era number that genuinely
/// varies (`0x64`, `0x30`, `0x31`, `0x6a`, `0x72` across the surveyed drawings),
/// so pinning it would recognise one corpus and miss real Eagle files, the wrong
/// way round for a check whose job is to name a format hauksbee cannot read.
/// Byte 3 is zero in all 76 surveyed files and is kept as the one cheap guard
/// against a two-byte coincidence.
///
/// No text format can begin with a `0x10` control byte, and no container
/// hauksbee reads (OLE2 `D0 CF 11 E0`, zip `PK`, gzip `1F 8B`) starts this way,
/// so this cannot take a file away from a reader that would have read it.
///
/// This is a NEGATIVE detector: it exists so the refusal can name the format
/// and the one action that unlocks the file, instead of reciting the whole
/// accepted-format list at someone holding a board hauksbee reads happily
/// after a re-save. The `.brd` and `.sch` forms share the header, so the
/// message must not claim to know which of the two this is.
#[must_use]
pub fn looks_like_eagle_binary(bytes: &[u8]) -> bool {
    matches!(bytes, [0x10, 0x00 | 0x80, _, 0x00, ..])
}

/// What to tell someone who dropped a pre-Eagle-6 binary drawing: what the file
/// is, and the input that unlocks it.
///
/// It names Eagle and Fusion because those two certainly write the XML form, and
/// then stops naming tools: whether any given third-party importer reads the
/// pre-6 binary format is not something hauksbee can check, and a refusal that
/// asserts it would be trading one wrong instruction for another. The
/// requirement is stated instead of the vendor, so the sentence stays true as
/// the tooling moves.
#[must_use]
pub fn eagle_binary_message() -> String {
    format!(
        "this is an Eagle drawing in the pre-Eagle-6 BINARY format, which hauksbee does \
         not read. Eagle 6 moved the .brd and .sch formats to XML, and the XML form is \
         what hauksbee reads: open this file once in Eagle 6 or later, or in Fusion 360 \
         Electronics, re-save it, and retry with the re-saved file. Anything that opens \
         the pre-6 binary format and writes Eagle XML or a KiCad board will do; failing \
         that, the design's gerbers are the other way in. See {}",
        hauksbee_ir::docs_url("docs/ingest/EAGLE.md")
    )
}

#[cfg(test)]
mod tests {
    use super::{eagle_binary_message, extract, looks_like_eagle_binary};

    #[test]
    fn pre_eagle_6_binary_headers_are_recognised_and_xml_is_not() {
        // The first six bytes of real pre-v6 drawings, and the point of the
        // table is byte 2: it is 0x64 across the whole Mutable Instruments
        // corpus and something else on every one of KiCad's regression boards,
        // so a detector that treats it as a fixed stamp passes this crate's own
        // corpus and refuses to recognise files from anywhere else.
        for (head, what) in [
            (
                [0x10, 0x80, 0x64, 0x00, 0x21, 0x13],
                "MI braids_v50.brd, era 3",
            ),
            (
                [0x10, 0x00, 0x64, 0x00, 0xed, 0x0d],
                "MI volts_v01.brd, era 4/5",
            ),
            (
                [0x10, 0x00, 0x72, 0x00, 0xaa, 0x07],
                "KiCad blink1_b1a.brd, era 4/5",
            ),
            (
                [0x10, 0x80, 0x72, 0x00, 0x88, 0x07],
                "KiCad blink1_v1a.brd, era 3",
            ),
            (
                [0x10, 0x80, 0x6a, 0x00, 0x35, 0x10],
                "KiCad boomchak.brd, era 3",
            ),
            (
                [0x10, 0x80, 0x31, 0x00, 0xa6, 0x05],
                "KiCad brenner57e.brd, era 3",
            ),
            (
                [0x10, 0x80, 0x30, 0x00, 0xc3, 0x01],
                "KiCad rocketgps.brd, era 3",
            ),
            (
                [0x10, 0x00, 0x6a, 0x00, 0x9a, 0x1b],
                "KiCad turnemoff.brd, era 4/5",
            ),
        ] {
            assert!(looks_like_eagle_binary(&head), "missed {what}: {head:02x?}");
        }
        // The XML form stays with the reader that parses it.
        assert!(!looks_like_eagle_binary(b"<?xml version=\"1.0\"?><eagle>"));
        assert!(!looks_like_eagle_binary(b"<eagle version=\"9.6.2\">"));
        // A truncated header, another binary container, and a third era byte
        // that is not followed by the zero all 76 real files carry.
        assert!(!looks_like_eagle_binary(&[0x10, 0x80, 0x64]));
        assert!(!looks_like_eagle_binary(&[0x10, 0x40, 0x64, 0x00]));
        assert!(!looks_like_eagle_binary(&[0x10, 0x80, 0x64, 0x01]));
        assert!(!looks_like_eagle_binary(&[0xd0, 0xcf, 0x11, 0xe0]));
        assert!(!looks_like_eagle_binary(b"PK\x03\x04"));
        assert!(!looks_like_eagle_binary(b""));
        // The message names the format and the action that unlocks the file.
        let msg = eagle_binary_message();
        assert!(msg.contains("pre-Eagle-6"), "got: {msg}");
        assert!(msg.contains("re-save"), "got: {msg}");
    }

    #[test]
    fn attribute_xml_entities_are_unescaped() {
        // R13: quick-xml leaves attribute values escaped. A net name carrying an
        // entity (`&amp;`) must decode to '&', not the literal "&amp;".
        let xml = r#"<eagle><drawing><board><signals>
            <signal name="VBUS&amp;5"/>
            <signal name="A&lt;B"/>
        </signals></board></drawing></eagle>"#;
        let board = extract(xml).unwrap();
        let names: Vec<&str> = board.nets.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"VBUS&5"), "entity decoded: {names:?}");
        assert!(names.contains(&"A<B"), "entity decoded: {names:?}");
    }

    /// An assembly-variant attribute on the element (`POPULATE=0`, `DNP=1`)
    /// marks the part not fitted just as `populate="no"` does; an attribute
    /// that says the part is populated, or an unrelated one, leaves it fitted.
    #[test]
    fn element_attributes_mark_parts_not_fitted() {
        let el = |name: &str, attr: &str| {
            format!(
                r#"<element name="{name}" library="l" package="P" value="v" x="0" y="0">{attr}</element>"#
            )
        };
        let xml = format!(
            r#"<eagle><drawing><board><libraries><library name="l"><packages><package name="P">
<smd name="1" x="0" y="0" dx="1" dy="1" layer="1"/></package></packages></library></libraries>
<elements>{}{}{}{}{}</elements><signals/></board></drawing></eagle>"#,
            el("U1", r#"<attribute name="POPULATE" value="0"/>"#),
            el("U2", r#"<attribute name="POPULATE" value="1"/>"#),
            el("U3", r#"<attribute name="DNP" value="yes"/>"#),
            el("U4", r#"<attribute name="MPN" value="0"/>"#),
            el("U5", ""),
        );
        let board = extract(&xml).unwrap();
        let dnp = |r: &str| {
            board
                .components
                .iter()
                .find(|c| c.reference == r)
                .map(|c| c.dnp)
                .expect("element present")
        };
        assert!(dnp("U1") && dnp("U3"));
        assert!(!dnp("U2") && !dnp("U4") && !dnp("U5"));
    }

    /// Element attributes with a value reach the component's properties, so a
    /// part number held in an `MPN`/`PART_NUMBER` attribute is visible to the
    /// binder; the `NAME`/`VALUE` placeholders and empty attributes are not
    /// copied.
    #[test]
    fn element_attributes_become_component_properties() {
        let xml = r#"<eagle><drawing><board><libraries><library name="l"><packages><package name="P">
<smd name="1" x="0" y="0" dx="1" dy="1" layer="1"/></package></packages></library></libraries>
<elements><element name="Q1" library="l" package="P" value="20V/4.2A" x="0" y="0">
<attribute name="NAME" x="0" y="0" layer="25"/>
<attribute name="VALUE" value="20V/4.2A" x="0" y="0" layer="27"/>
<attribute name="PART_NUMBER" value=" PMV48XP "/>
<attribute name="PROD_ID" value="TRANS-1234"/>
<attribute name="SPICEPREFIX" value=""/>
</element></elements><signals/></board></drawing></eagle>"#;
        let board = extract(xml).unwrap();
        assert_eq!(
            board.components[0].properties,
            vec![
                ("PART_NUMBER".to_string(), "PMV48XP".to_string()),
                ("PROD_ID".to_string(), "TRANS-1234".to_string()),
            ]
        );
    }
}
