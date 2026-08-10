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

impl DeclaredNetTie {
    /// True when this declaration covers a copper contact between these two
    /// nets, in either order. Net names are compared exactly: Eagle net names
    /// are case-sensitive and `GND` is not `gnd`.
    pub fn covers(&self, net_a: &str, net_b: &str) -> bool {
        (self.net == net_a && self.tied_net == net_b)
            || (self.net == net_b && self.tied_net == net_a)
    }

    /// The declaration in one clause, naming the symbols and the net, for a
    /// report line. `AGND7 wired to SUPPLY6 in net GND`.
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
/// slice. Eagle writes `<settings>`, `<grid>` and the full `<layers>` table
/// first: on emonTx V3.4.5 the root is at byte 75 but `<schematic>` is at 10856,
/// so a 4 KB head slice rejected a perfectly good schematic and silently
/// disabled this whole pathway on the very board it exists for. Only `<eagle>`
/// is checked near the top, where it genuinely is. The scan is one linear pass
/// and cheap next to parsing the file.
pub fn looks_like_eagle_schematic(text: &str) -> bool {
    let head: String = text.chars().take(1024).collect();
    head.contains("<eagle") && text.contains("<schematic")
}

/// Every deliberate net tie the schematic declares, in file order.
///
/// Returns an empty vector for a schematic that declares none, which is the
/// common case and is not an error: most boards keep their supply symbols on
/// their own nets. An XML error IS surfaced, because a companion input the user
/// explicitly supplied must not be silently half-read.
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

/// One `<segment>` of one `<net>`: which net it belongs to and which of its
/// `<pinref>`s name a part this file defines as a supply symbol.
struct Segment {
    net: String,
    supply_parts: Vec<String>,
}

struct Parsed {
    /// Part name -> the net name its supply symbol imposes. Only supply parts
    /// appear here, so a lookup miss means "an ordinary component".
    part_supply: HashMap<String, String>,
    segments: Vec<Segment>,
}

/// A single streaming pass, in the order Eagle writes the file: `<libraries>`
/// (symbols, then devicesets) precede `<parts>`, which precede `<sheets>`. That
/// order is what makes one pass enough, and it is the order Eagle's DTD fixes.
fn parse(text: &str) -> Result<Parsed, ExtractError> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);

    // (library, symbol) -> the supply net name, from the symbol's `sup` pin.
    let mut supply_symbols: HashMap<(String, String), String> = HashMap::new();
    // (library, deviceset) -> the supply net name, resolved through its gates.
    let mut supply_devicesets: HashMap<(String, String), String> = HashMap::new();
    let mut part_supply: HashMap<String, String> = HashMap::new();
    let mut segments: Vec<Segment> = Vec::new();

    let mut cur_library = String::new();
    let mut cur_symbol: Option<String> = None;
    let mut cur_deviceset: Option<String> = None;
    let mut cur_net: Option<String> = None;
    let mut cur_segment: Option<Segment> = None;
    let mut saw_eagle_root = false;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let a = attrs(&e);
                match e.name().as_ref() {
                    b"eagle" => saw_eagle_root = true,
                    b"library" => {
                        cur_library = a.get("name").cloned().unwrap_or_default();
                    }
                    b"symbol" => cur_symbol = a.get("name").cloned(),
                    b"pin" => {
                        // The supply marker. `direction="sup"` is Eagle's own
                        // flag for a supply pin; the pin's NAME is the net.
                        // Keying on the library name (`supply1`) instead would
                        // be a naming heuristic and would miss a hand-drawn
                        // supply symbol in a project library.
                        if let (Some(symbol), Some("sup"), Some(name)) = (
                            cur_symbol.as_ref(),
                            a.get("direction").map(String::as_str),
                            a.get("name"),
                        ) {
                            supply_symbols
                                .insert((cur_library.clone(), symbol.clone()), name.clone());
                        }
                    }
                    b"deviceset" => cur_deviceset = a.get("name").cloned(),
                    b"gate" => {
                        if let (Some(deviceset), Some(symbol)) =
                            (cur_deviceset.as_ref(), a.get("symbol"))
                        {
                            if let Some(supply) =
                                supply_symbols.get(&(cur_library.clone(), symbol.clone()))
                            {
                                // A multi-gate deviceset is not a supply symbol;
                                // a supply symbol has exactly one gate. Taking
                                // the first supply gate is therefore not a
                                // choice, and `or_insert` keeps it stable if a
                                // pathological file offers two.
                                supply_devicesets
                                    .entry((cur_library.clone(), deviceset.clone()))
                                    .or_insert_with(|| supply.clone());
                            }
                        }
                    }
                    b"part" => {
                        let (Some(name), Some(library), Some(deviceset)) =
                            (a.get("name"), a.get("library"), a.get("deviceset"))
                        else {
                            continue;
                        };
                        if let Some(supply) =
                            supply_devicesets.get(&(library.clone(), deviceset.clone()))
                        {
                            part_supply.insert(name.clone(), supply.clone());
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
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"symbol" => cur_symbol = None,
                b"deviceset" => cur_deviceset = None,
                b"library" => cur_library.clear(),
                b"net" => cur_net = None,
                b"segment" => {
                    if let Some(segment) = cur_segment.take() {
                        if !segment.supply_parts.is_empty() {
                            segments.push(segment);
                        }
                    }
                }
                _ => {}
            },
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

    if !saw_eagle_root {
        return Err(ExtractError::WrongRoot {
            expected: "eagle schematic",
            found: None,
        });
    }
    Ok(Parsed {
        part_supply,
        segments,
    })
}

/// Attribute map with entities unescaped, mirroring `eagle.rs`: quick-xml hands
/// back raw attribute bytes, so an `&amp;` in a net name would otherwise reach a
/// report literally and never match the `.brd`'s own spelling of that net.
fn attrs(e: &quick_xml::events::BytesStart) -> HashMap<String, String> {
    e.attributes()
        .flatten()
        .map(|a| {
            // quick-xml deprecates this in favour of `normalized_value`, which
            // takes an `XmlVersion` the crate does not export. Same call and
            // same reasoning as `eagle.rs`.
            #[allow(deprecated)]
            let value = a
                .unescape_value()
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned());
            (String::from_utf8_lossy(a.key.as_ref()).into_owned(), value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_foreign_supply_symbol_on_a_net_declares_a_tie() {
        // The emonTx construct, distilled: an AGND supply symbol wired to a GND
        // supply symbol inside one segment of net GND.
        let ties = declared_net_ties(&schematic(
            r#"<net name="GND" class="0">
<segment>
<pinref part="SUPPLY6" gate="GND" pin="GND"/>
<pinref part="AGND7" gate="VR1" pin="AGND"/>
<junction x="-31.75" y="365.76"/>
</segment>
</net>"#,
        ))
        .expect("parses");
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
    fn supply_symbols_on_their_own_nets_declare_nothing() {
        // The overwhelmingly common case, and the false-negative guard: a board
        // whose grounds are separate must produce no exemption whatsoever.
        let ties = declared_net_ties(&schematic(
            r#"<net name="GND" class="0">
<segment><pinref part="SUPPLY6" gate="GND" pin="GND"/><pinref part="R1" gate="G$1" pin="1"/></segment>
</net>
<net name="AGND" class="0">
<segment><pinref part="AGND7" gate="VR1" pin="AGND"/><pinref part="R1" gate="G$1" pin="2"/></segment>
</net>"#,
        ))
        .expect("parses");
        assert!(ties.is_empty(), "no tie is declared, got {ties:?}");
    }

    #[test]
    fn a_tie_in_one_segment_does_not_travel_to_another_net() {
        // Two nets each carrying their own supply symbol, plus a third net that
        // ties nothing: the declaration is per segment, so only the segment
        // that actually holds the foreign symbol declares anything.
        let ties = declared_net_ties(&schematic(
            r#"<net name="AGND" class="0">
<segment><pinref part="AGND7" gate="VR1" pin="AGND"/><pinref part="SUPPLY6" gate="GND" pin="GND"/></segment>
</net>
<net name="GND" class="0">
<segment><pinref part="R1" gate="G$1" pin="1"/></segment>
</net>"#,
        ))
        .expect("parses");
        assert_eq!(ties.len(), 1);
        // Declared from AGND's side this time: net AGND, foreign symbol SUPPLY6.
        assert_eq!(ties[0].net, "AGND");
        assert_eq!(ties[0].tied_net, "GND");
        assert_eq!(ties[0].symbol, "SUPPLY6");
        assert_eq!(ties[0].tied_to, ["AGND7"]);
    }

    #[test]
    fn an_ordinary_component_is_not_a_supply_symbol() {
        // R1's pins carry no `direction="sup"`, so two nets bridged by a
        // resistor symbol are NOT declared tied. A resistor really joining two
        // nets is a component, and the DRC must keep reporting copper that
        // bypasses it.
        let ties = declared_net_ties(&schematic(
            r#"<net name="GND" class="0">
<segment><pinref part="R1" gate="G$1" pin="1"/><pinref part="R1" gate="G$1" pin="2"/></segment>
</net>"#,
        ))
        .expect("parses");
        assert!(ties.is_empty(), "got {ties:?}");
    }

    #[test]
    fn a_supply_symbol_alone_on_a_named_net_still_declares_the_tie() {
        // The label form: net named by a <label>, not by a second supply
        // symbol, so there is nothing to report as "wired to".
        let ties = declared_net_ties(&schematic(
            r#"<net name="VBUS" class="0">
<segment><pinref part="SUPPLY6" gate="GND" pin="GND"/><label x="0" y="0" size="1.778" layer="95"/></segment>
</net>"#,
        ))
        .expect("parses");
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
        let preamble: String = (0..400)
            .map(|n| {
                format!(
                    "<layer number=\"{n}\" name=\"L{n}\" color=\"4\" fill=\"1\" \
                     visible=\"yes\" active=\"yes\"/>\n"
                )
            })
            .collect();
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
        let ties = declared_net_ties(
            &schematic(
                r#"<net name="GND" class="0">
<segment><pinref part="SUPPLY6" gate="GND" pin="GND"/><pinref part="AGND7" gate="VR1" pin="AGND"/></segment>
</net>"#,
            )
            .replace("<schematic>", &format!("{preamble}<schematic>")),
        )
        .expect("parses");
        assert_eq!(ties.len(), 1);
        assert_eq!(ties[0].describe(), "AGND7 wired to SUPPLY6 in net GND");
    }

    #[test]
    fn malformed_xml_is_an_error_not_an_empty_answer() {
        // A companion the user explicitly supplied must never be silently
        // half-read: "no ties declared" and "could not read it" are different
        // claims, and only one of them is safe to downgrade a short on.
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
}
