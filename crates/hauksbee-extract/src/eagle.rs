//! Extraction from Eagle `.brd` board files (XML, Eagle 6+). Covers the
//! large legacy ecosystem: Arduino, Adafruit, SparkFun designs.
//!
//! Connectivity in a `.brd` is explicit: `<signals><signal name="GND">
//! <contactref element="C1" pad="1"/>...`. Component placement comes from
//! `<elements>`, pad offsets from `<packages>`.

use crate::{Component, ExtractError, ExtractedBoard, Net, Pin};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;

pub fn extract(text: &str) -> Result<ExtractedBoard, ExtractError> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);

    // (library name, package name) -> [(pad name, dx, dy)]. Eagle namespaces
    // packages per <library>: two embedded libraries may each define a package
    // named e.g. "0805" with different pads. Keying by bare package name merged
    // them, so an element resolved to the concatenation of both libraries' pads
    // and was emitted with doubled/mixed pins.
    let mut packages: HashMap<(String, String), Vec<(String, f64, f64)>> = HashMap::new();
    let mut cur_library = String::new();
    let mut cur_package: Option<String> = None;
    // elements
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
    }
    let mut elements: Vec<El> = Vec::new();
    // signals
    let mut signals: Vec<(String, Vec<(String, String)>)> = Vec::new();
    let mut saw_eagle_root = false;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let attrs = |e: &quick_xml::events::BytesStart| -> HashMap<String, String> {
                    e.attributes()
                        .flatten()
                        .map(|a| {
                            (
                                String::from_utf8_lossy(a.key.as_ref()).into_owned(),
                                String::from_utf8_lossy(&a.value).into_owned(),
                            )
                        })
                        .collect()
                };
                match e.name().as_ref() {
                    b"eagle" => saw_eagle_root = true,
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
                        let rot = a.get("rot").map(String::as_str).unwrap_or("R0");
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
                            // 'M' (mirror) in either order — "MR90", "SMR90".
                            // `starts_with('M')` missed the spin-prefixed
                            // "SMR90" form, disagreeing with drc.rs (contains).
                            mirrored: rot.contains('M'),
                            // Eagle marks a do-not-populate / assembly-variant
                            // part with populate="no" on the element. Without
                            // this every Eagle part read as populated, unlike
                            // the KiCad readers which thread the analogous field.
                            dnp: a
                                .get("populate")
                                .map(|p| p.eq_ignore_ascii_case("no"))
                                .unwrap_or(false),
                        });
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
            Ok(Event::End(e)) if e.name().as_ref() == b"library" => cur_library.clear(),
            Ok(Event::Eof) => break,
            Err(err) => {
                return Err(ExtractError::Xml(err.to_string()));
            }
            _ => {}
        }
        buf.clear();
    }

    if !saw_eagle_root {
        return Err(ExtractError::WrongRoot {
            expected: "eagle",
            found: None,
        });
    }

    let nets: Vec<Net> = signals
        .iter()
        .enumerate()
        .map(|(i, (name, _))| Net {
            id: i as i64 + 1,
            name: name.clone(),
        })
        .collect();

    // (element, pad) -> net id
    let mut pad_net: HashMap<(String, String), i64> = HashMap::new();
    for (i, (_, refs)) in signals.iter().enumerate() {
        for (el, pad) in refs {
            pad_net.insert((el.clone(), pad.clone()), i as i64 + 1);
        }
    }

    // `package` is required on an Eagle <element>; when it is absent the lookup
    // below misses and the part lands with zero pins (its own connectivity
    // silently lost, though the nets it touched survive via <contactref>). Only
    // reachable with schema-invalid XML, so surface it rather than fail — a
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
            // and then rotates by `-deg` — mirroring flips the sense of
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
                            let abs = (el.x + dx * cos - dy * sin, el.y + dx * sin + dy * cos);
                            Pin {
                                number: pname.clone(),
                                net: pad_net.get(&(el.name.clone(), pname.clone())).copied(),
                                function: String::new(),
                                kind: String::new(),
                                position: Some(abs),
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
                properties: Vec::new(),
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

fn num(attrs: &HashMap<String, String>, key: &str) -> Option<f64> {
    attrs.get(key)?.parse().ok()
}
