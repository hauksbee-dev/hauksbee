//! Extraction from ASCII Protel board exports (`Protel_Advanced_PCB`).
//!
//! Altium's `.PcbDoc` exists in two on-disk forms. Altium Designer saves the
//! binary OLE2 container ([`crate::altium`]); EasyEDA and several converters
//! instead export the ASCII form: one pipe-delimited record per line,
//! `|RECORD=Board|KIND=Protel_Advanced_PCB|...`. A majority of the `.pcbdoc`
//! files found in the wild (21 of 30 surveyed) are this ASCII form, so
//! rejecting it with "not a valid OLE2 file" locked out most real uploads.
//!
//! The records carry the same properties the binary streams do, just spelled
//! out: `|RECORD=Net|ID=0|NAME=EN`, `|RECORD=Component|ID=4|SOURCEDESIGNATOR=R5
//! |PATTERN=R0603|LAYER=TOP|...`, `|RECORD=Pad|COMPONENT=4|NET=27|NAME=1|...`.
//! Component values live in `|RECORD=Text|COMMENT=True` records whose
//! `WIDESTRING` field is the string as comma-separated Unicode code points
//! (`49,107,937` = "1kΩ"). This module reads nets, components, pads and
//! comment texts into the same [`ExtractedBoard`] shape every other extractor
//! feeds, so the whole downstream pipeline (bind, lint, report) works
//! unchanged. Copper geometry (tracks/polygons) is not read; like a netlist,
//! an ASCII-Protel board gets connectivity checks but no clearance DRC.

use crate::altium::{
    canonical_component_identities, is_copper_layer, layer_id_from_name, parse_len_mm,
    side_from_layer_name, value_from_description, ComponentIdentityInput, VALUE_UNRESOLVED_KEY,
    VALUE_UNRESOLVED_REASON,
};
use crate::{Component, ExtractError, ExtractedBoard, Net, Pin};
use std::collections::HashMap;

/// Content sniff: pipe-delimited `|RECORD=` text that declares itself a
/// `Protel_Advanced_PCB` board. The KIND gate keeps ASCII exports of OTHER
/// Protel documents (schematics etc.) out of this reader; those fall through
/// to the unrecognized-format message instead of a garbled parse.
pub(crate) fn looks_like_protel_ascii(bytes: &[u8]) -> bool {
    let head = leading_text(bytes);
    head.starts_with("|RECORD=") && head.contains("KIND=Protel_Advanced_PCB")
}

/// Content sniff for the error path: ANY leading pipe-delimited `|RECORD=`
/// text, whether or not it is a board this crate can read.
pub(crate) fn looks_like_pipe_records(bytes: &[u8]) -> bool {
    leading_text(bytes).starts_with("|RECORD=")
}

/// The first ~2 KiB as text, BOM and leading whitespace stripped.
fn leading_text(bytes: &[u8]) -> String {
    let window = &bytes[..bytes.len().min(2048)];
    String::from_utf8_lossy(window)
        .trim_start_matches('\u{feff}')
        .trim_start()
        .to_string()
}

/// One record line as an uppercased-key field map.
fn fields(line: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for tok in line.split('|') {
        if let Some((k, v)) = tok.split_once('=') {
            map.insert(k.trim().to_ascii_uppercase(), v.trim().to_string());
        }
    }
    map
}

/// Decode a `WIDESTRING` field: comma-separated decimal Unicode code points.
fn decode_widestring(s: &str) -> String {
    s.split(',')
        .filter_map(|t| t.trim().parse::<u32>().ok())
        .filter_map(char::from_u32)
        .collect()
}

/// Extract the connectivity model from ASCII Protel text.
pub fn extract(text: &str) -> Result<ExtractedBoard, ExtractError> {
    // File-side ids are explicit (`ID=` on nets/components, `NET=`/`COMPONENT=`
    // back-references on pads/texts), so collect keyed maps first.
    let mut net_names: HashMap<i64, String> = HashMap::new();
    struct Comp {
        id: i64,
        refdes: String,
        pattern: String,
        library: String,
        description: String,
        source_hierarchical_path: String,
        source_unique_id: String,
        layer_name: String,
        x_mm: f64,
        y_mm: f64,
        rotation: f64,
    }
    let mut comps: Vec<Comp> = Vec::new();
    struct Pad {
        component: Option<i64>,
        net: Option<i64>,
        name: String,
        layer: String,
        x_mm: f64,
        y_mm: f64,
    }
    let mut pads: Vec<Pad> = Vec::new();
    let mut comments: HashMap<i64, String> = HashMap::new();

    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        let m = fields(line);
        let id_of = |key: &str| m.get(key).and_then(|v| v.parse::<i64>().ok());
        match m.get("RECORD").map(String::as_str) {
            Some("Net") => {
                if let (Some(id), Some(name)) = (id_of("ID"), m.get("NAME")) {
                    net_names.insert(id, name.clone());
                }
            }
            Some("Component") => {
                let Some(id) = id_of("ID") else { continue };
                comps.push(Comp {
                    id,
                    refdes: m.get("SOURCEDESIGNATOR").cloned().unwrap_or_default(),
                    pattern: m.get("PATTERN").cloned().unwrap_or_default(),
                    library: m.get("SOURCEFOOTPRINTLIBRARY").cloned().unwrap_or_default(),
                    description: m.get("SOURCEDESCRIPTION").cloned().unwrap_or_default(),
                    source_hierarchical_path: m
                        .get("SOURCEHIERARCHICALPATH")
                        .cloned()
                        .unwrap_or_default(),
                    source_unique_id: m
                        .get("UNIQUEID")
                        .or_else(|| m.get("SOURCEUNIQUEID"))
                        .cloned()
                        .unwrap_or_default(),
                    layer_name: m.get("LAYER").cloned().unwrap_or_default(),
                    x_mm: m.get("X").and_then(|v| parse_len_mm(v)).unwrap_or(0.0),
                    y_mm: m.get("Y").and_then(|v| parse_len_mm(v)).unwrap_or(0.0),
                    rotation: m
                        .get("ROTATION")
                        .and_then(|v| v.parse::<f64>().ok())
                        .unwrap_or(0.0),
                });
            }
            Some("Pad") => {
                pads.push(Pad {
                    component: id_of("COMPONENT").filter(|&v| v >= 0),
                    net: id_of("NET").filter(|&v| v >= 0),
                    name: m.get("NAME").cloned().unwrap_or_default(),
                    layer: m.get("LAYER").cloned().unwrap_or_default(),
                    x_mm: m.get("X").and_then(|v| parse_len_mm(v)).unwrap_or(0.0),
                    y_mm: m.get("Y").and_then(|v| parse_len_mm(v)).unwrap_or(0.0),
                });
            }
            Some("Text") => {
                // Comment texts carry the value; designator texts (the refdes
                // label) must never be mistaken for one.
                let is_comment = m
                    .get("COMMENT")
                    .is_some_and(|v| v.eq_ignore_ascii_case("true"));
                if !is_comment {
                    continue;
                }
                let Some(comp) = id_of("COMPONENT") else {
                    continue;
                };
                let txt = m.get("WIDESTRING").map(|w| decode_widestring(w));
                let txt = match txt {
                    Some(t) if !t.is_empty() => t,
                    _ => m.get("TEXT").cloned().unwrap_or_default(),
                };
                // ".Comment" / ".Designator" placeholders mean the displayed
                // string is field-bound, not a literal value.
                if !txt.is_empty() && !txt.starts_with('.') {
                    comments.entry(comp).or_insert(txt);
                }
            }
            _ => {}
        }
    }

    // Net ids: file id + 1, keeping hauksbee's id 0 = "no net" convention.
    let mut net_ids: Vec<i64> = net_names.keys().copied().collect();
    net_ids.sort_unstable();
    let nets: Vec<Net> = net_ids
        .iter()
        .map(|&id| Net {
            id: id + 1,
            name: net_names[&id].clone(),
        })
        .collect();
    let net_id = |file_id: Option<i64>| -> Option<i64> {
        file_id
            .filter(|id| net_names.contains_key(id))
            .map(|id| id + 1)
    };

    // Group pads by owning component, copper pads only (same reasoning as the
    // binary path: a paste/mask/mechanical pad record is not a pin, and
    // counting one turns a 2-pad passive into a phantom 3-pad array).
    let mut pads_by_comp: HashMap<i64, Vec<&Pad>> = HashMap::new();
    for p in &pads {
        if !is_copper_layer(layer_id_from_name(&p.layer)) {
            continue;
        }
        if let Some(c) = p.component {
            pads_by_comp.entry(c).or_default().push(p);
        }
    }

    let identity_inputs: Vec<ComponentIdentityInput> = comps
        .iter()
        .map(|component| ComponentIdentityInput {
            source_designator: component.refdes.clone(),
            source_hierarchical_path: component.source_hierarchical_path.clone(),
            source_unique_id: component.source_unique_id.clone(),
            record_key: component.id.to_string(),
            pins: pads_by_comp
                .get(&component.id)
                .into_iter()
                .flat_map(|pads| pads.iter())
                .map(|pad| (pad.name.clone(), pad.net))
                .collect(),
        })
        .collect();
    let identities = canonical_component_identities(&identity_inputs);

    let mut components: Vec<Component> = Vec::with_capacity(comps.len());
    for (index, c) in comps.iter().enumerate() {
        let identity = &identities[index];
        let reference = identity.reference.clone();

        let pins: Vec<Pin> = pads_by_comp
            .get(&c.id)
            .map(|ps| {
                ps.iter()
                    .map(|p| Pin {
                        number: p.name.clone(),
                        net: net_id(p.net),
                        function: String::new(),
                        kind: String::new(),
                        position: Some((p.x_mm, p.y_mm)),
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Value resolution matches the binary path: comment text, else the
        // SOURCEDESCRIPTION parse, else honestly unresolved with the reason.
        let mut properties: Vec<(String, String)> = Vec::new();
        properties.extend(identity.properties.clone());
        let mut value = comments.get(&c.id).cloned().unwrap_or_default();
        if value.is_empty() && !c.description.is_empty() {
            let d = value_from_description(&c.description);
            if let Some(v) = d.value {
                value = v;
            }
            if let Some(v) = d.voltage {
                properties.push(("voltage_rating".to_string(), v));
            }
            if let Some(p) = d.power {
                properties.push(("power_rating".to_string(), p));
            }
        }
        if value.is_empty() {
            properties.push((
                VALUE_UNRESOLVED_KEY.to_string(),
                VALUE_UNRESOLVED_REASON.to_string(),
            ));
        }

        let lib_id = if c.library.is_empty() {
            c.pattern.clone()
        } else {
            format!("{}:{}", c.library, c.pattern)
        };
        components.push(Component {
            reference,
            value,
            lib_id,
            footprint: c.pattern.clone(),
            position: Some((c.x_mm, c.y_mm, c.rotation)),
            layer: side_from_layer_name(&c.layer_name).to_string(),
            properties,
            dnp: false,
            pins,
        });
    }

    if nets.is_empty() && components.is_empty() {
        return Err(ExtractError::Altium(
            "ASCII Protel file carries no net or component records".into(),
        ));
    }

    let components = crate::merge_duplicate_references(components);

    Ok(ExtractedBoard {
        name: String::new(),
        nets,
        components,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A widestring for "1kΩ": '1'=49, 'k'=107, 'Ω'=937.
    const KILOHM: &str = "49,107,937";

    fn sample() -> String {
        [
            "|RECORD=Board|KIND=Protel_Advanced_PCB|VERSION=5.00",
            "|RECORD=Net|ID=0|NAME=VCC",
            "|RECORD=Net|ID=1|NAME=GND",
            "|RECORD=Component|ID=0|LAYER=TOP|X=100mil|Y=200mil|ROTATION=90|PATTERN=R0603|SOURCEDESIGNATOR=R1|SOURCEFOOTPRINTLIBRARY=Std",
            "|RECORD=Component|ID=1|LAYER=BOTTOM|X=0mil|Y=0mil|ROTATION=0|PATTERN=LED0603|SOURCEDESIGNATOR=LED1",
            "|RECORD=Pad|COMPONENT=0|NET=0|LAYER=TOP|NAME=1|X=100mil|Y=200mil",
            "|RECORD=Pad|COMPONENT=0|NET=1|LAYER=TOP|NAME=2|X=160mil|Y=200mil",
            // A paste-layer pad record on the same component: not a pin.
            "|RECORD=Pad|COMPONENT=0|LAYER=TOPPASTE|NAME=P|X=130mil|Y=200mil",
            "|RECORD=Pad|COMPONENT=1|NET=1|LAYER=MULTILAYER|NAME=1|X=0mil|Y=0mil",
            &format!("|RECORD=Text|COMPONENT=0|COMMENT=True|LAYER=TOP|WIDESTRING={KILOHM}"),
            "|RECORD=Text|COMPONENT=0|DESIGNATOR=True|LAYER=TOP|WIDESTRING=82,49",
        ]
        .join("\r\n")
    }

    #[test]
    fn extracts_nets_components_pads_and_comment_values() {
        let board = extract(&sample()).expect("extract");
        assert_eq!(board.nets.len(), 2);
        assert!(board.net_by_name("VCC").is_some());
        let r1 = board.component("R1").expect("R1");
        assert_eq!(r1.value, "1kΩ", "comment widestring is the value");
        assert_eq!(r1.footprint, "R0603");
        assert_eq!(r1.layer, "F.Cu");
        assert_eq!(
            r1.pins.len(),
            2,
            "the paste-layer pad record must not become a pin"
        );
        // Connectivity: R1.2 and LED1.1 share GND.
        let gnd = board.net_by_name("GND").unwrap();
        assert_eq!(board.net_members(gnd.id).len(), 2);
        // The LED has no comment and no description: honestly unresolved.
        let led = board.component("LED1").unwrap();
        assert_eq!(led.value, "");
        assert!(led
            .properties
            .iter()
            .any(|(k, v)| k == VALUE_UNRESOLVED_KEY && v == VALUE_UNRESOLVED_REASON));
    }

    #[test]
    fn designator_text_is_never_the_value() {
        // Drop the comment record: the DESIGNATOR text ("R1") must not become
        // the value the way the binary-path flag mixup fabricated values.
        let text: String = sample()
            .lines()
            .filter(|l| !l.contains("COMMENT=True"))
            .collect::<Vec<_>>()
            .join("\n");
        let board = extract(&text).expect("extract");
        let r1 = board.component("R1").unwrap();
        assert_eq!(r1.value, "", "designator text must not be read as a value");
    }

    #[test]
    fn duplicate_placements_merge_instead_of_inventing_part_references() {
        let text = [
            "|RECORD=Board|KIND=Protel_Advanced_PCB|VERSION=5.00",
            "|RECORD=Net|ID=0|NAME=A",
            "|RECORD=Net|ID=1|NAME=B",
            "|RECORD=Component|ID=0|LAYER=TOP|PATTERN=TESTPOINT|SOURCEDESIGNATOR=TP1",
            "|RECORD=Component|ID=1|LAYER=BOTTOM|PATTERN=TESTPOINT|SOURCEDESIGNATOR=TP1",
            "|RECORD=Pad|COMPONENT=0|NET=0|LAYER=TOP|NAME=1|X=0mil|Y=0mil",
            "|RECORD=Pad|COMPONENT=1|NET=0|LAYER=MULTILAYER|NAME=1|X=0mil|Y=0mil",
            "|RECORD=Pad|COMPONENT=1|NET=1|LAYER=MULTILAYER|NAME=2|X=100mil|Y=0mil",
        ]
        .join("\n");

        let board = extract(&text).expect("extract");
        assert_eq!(board.components.len(), 1);
        let tp1 = board.component("TP1").expect("unsuffixed TP1");
        assert_eq!(tp1.pins.len(), 3, "physical pad records are preserved");
        assert_eq!(
            tp1.pins
                .iter()
                .map(|pin| pin.number.as_str())
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from(["1", "2"]),
            "electrically there are two unique pin numbers"
        );
        assert!(tp1
            .properties
            .iter()
            .any(|(key, _)| key == "reference_ambiguous"));
        assert!(board.component("TP1_2").is_none());
    }

    #[test]
    fn ascii_hierarchical_channels_use_the_shared_full_path_identity() {
        let text = [
            "|RECORD=Board|KIND=Protel_Advanced_PCB|VERSION=5.00",
            "|RECORD=Net|ID=0|NAME=A",
            "|RECORD=Net|ID=1|NAME=B",
            "|RECORD=Component|ID=7|LAYER=TOP|PATTERN=R0402|SOURCEDESIGNATOR=R1|SOURCEHIERARCHICALPATH=\\A\\BANK",
            "|RECORD=Component|ID=8|LAYER=TOP|PATTERN=R0402|SOURCEDESIGNATOR=R1|SOURCEHIERARCHICALPATH=\\B\\BANK",
            "|RECORD=Pad|COMPONENT=7|NET=0|LAYER=TOP|NAME=1|X=0mil|Y=0mil",
            "|RECORD=Pad|COMPONENT=8|NET=1|LAYER=TOP|NAME=1|X=100mil|Y=0mil",
        ]
        .join("\n");

        let board = extract(&text).expect("extract");
        assert_eq!(board.components.len(), 2);
        assert!(board.component("R1@A/BANK").is_some());
        assert!(board.component("R1@B/BANK").is_some());
    }

    #[test]
    fn numeric_designators_get_per_record_identities_without_hitting_real_unk_names() {
        let text = [
            "|RECORD=Board|KIND=Protel_Advanced_PCB|VERSION=5.00",
            "|RECORD=Net|ID=0|NAME=A",
            "|RECORD=Component|ID=7|LAYER=TOP|PATTERN=PAD|SOURCEDESIGNATOR=123",
            "|RECORD=Component|ID=8|LAYER=TOP|PATTERN=PAD|SOURCEDESIGNATOR=123",
            "|RECORD=Component|ID=9|LAYER=TOP|PATTERN=PAD|SOURCEDESIGNATOR=UNK123",
            "|RECORD=Pad|COMPONENT=7|NET=0|LAYER=TOP|NAME=1|X=0mil|Y=0mil",
            "|RECORD=Pad|COMPONENT=8|NET=0|LAYER=TOP|NAME=2|X=100mil|Y=0mil",
            "|RECORD=Pad|COMPONENT=9|NET=0|LAYER=TOP|NAME=3|X=200mil|Y=0mil",
        ]
        .join("\n");

        let board = extract(&text).expect("extract");
        assert_eq!(board.components.len(), 3);
        assert!(
            board.component("UNK123").is_some(),
            "genuine name preserved"
        );
        let synthetic: Vec<_> = board
            .components
            .iter()
            .filter(|component| {
                component
                    .properties
                    .iter()
                    .any(|(key, _)| key == "reference_unresolved")
            })
            .collect();
        assert_eq!(synthetic.len(), 2);
        assert_ne!(synthetic[0].reference, synthetic[1].reference);
        assert!(synthetic
            .iter()
            .all(|component| component.reference != "UNK123"));
    }

    #[test]
    fn a_later_split_placement_value_clears_the_unresolved_marker() {
        let text = [
            "|RECORD=Board|KIND=Protel_Advanced_PCB|VERSION=5.00",
            "|RECORD=Net|ID=0|NAME=A",
            "|RECORD=Component|ID=0|LAYER=TOP|PATTERN=R0603|SOURCEDESIGNATOR=R1",
            "|RECORD=Component|ID=1|LAYER=BOTTOM|PATTERN=R0603|SOURCEDESIGNATOR=R1",
            "|RECORD=Pad|COMPONENT=0|NET=0|LAYER=TOP|NAME=1|X=0mil|Y=0mil",
            "|RECORD=Pad|COMPONENT=1|NET=0|LAYER=MULTILAYER|NAME=1|X=0mil|Y=0mil",
            "|RECORD=Pad|COMPONENT=1|NET=0|LAYER=MULTILAYER|NAME=2|X=100mil|Y=0mil",
            "|RECORD=Text|COMPONENT=1|COMMENT=True|TEXT=10k",
        ]
        .join("\n");

        let board = extract(&text).expect("extract");
        let r1 = board.component("R1").expect("merged R1");
        assert_eq!(r1.value, "10k");
        assert!(
            !r1.properties
                .iter()
                .any(|(key, _)| key == VALUE_UNRESOLVED_KEY),
            "a recovered value and an unresolved marker cannot both be true: {:?}",
            r1.properties
        );
    }

    #[test]
    fn sniffs_require_the_board_kind() {
        assert!(looks_like_protel_ascii(sample().as_bytes()));
        assert!(!looks_like_protel_ascii(
            b"|RECORD=Sheet|KIND=Protel_Schematic"
        ));
        assert!(looks_like_pipe_records(b"|RECORD=Sheet|KIND=Whatever"));
        assert!(!looks_like_pipe_records(b"(kicad_pcb)"));
    }
}
