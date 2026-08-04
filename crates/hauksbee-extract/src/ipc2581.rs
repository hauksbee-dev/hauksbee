//! Extraction from IPC-2581 (DPMX) design-exchange XML.
//!
//! IPC-2581 is the open, vendor-neutral answer to ODB++: one XML document
//! carrying the stackup, the placement, the netlist and the BOM. Altium,
//! Cadence, Zuken, Mentor, DownStream and KiCad 8+ all export it, and unlike a
//! gerber set it is *electrical* — the netlist is stated, not reconstructed.
//!
//! ## What this reads
//!
//! ```text
//! <IPC-2581 revision="B"|"C">
//!   <Content><FunctionMode mode="…"/></Content>       what the file is FOR
//!   <Bom><BomItem …><RefDes name populate packageRef/>  values, DNP, package
//!         <Characteristics><Textual …/>
//!   <Ecad><CadData>
//!     <Layer name layerFunction side/>                 the stackup roles
//!     <Step name>
//!       <Package name><Pin number/>                    the pins a part has
//!       <Component refDes packageRef part layerRef>     placement
//!         <Location x y/><Xform rotation mirror/>
//!       <LogicalNet name><PinRef componentRef pin/>     the netlist
//!       <LayerFeature layerRef><Set net><Pad><PinRef/>  per-layer copper
//! ```
//!
//! ## What "the format" actually means in the wild
//!
//! The shape above is the schema's. Real exports diverge from it in ways that are
//! all schema-valid and all fatal to a reader that assumes one producer, so each
//! one is handled explicitly. Every item here was found by reading a real
//! document from a real tool, and each was the difference between a full netlist
//! and a board with nets and **zero connections**:
//!
//! | What varies | Who | Handled by |
//! |---|---|---|
//! | Revision `A`, `B`, `C`, or no `revision` attribute at all | Allegro 16.6 / Altium / KiCad / Zuken | nothing keys on it; it is only reported |
//! | Namespaced or bare element names | all / converters | matching on the **local name** |
//! | `<LogicalNetPin>` instead of `<PinRef>` inside `<LogicalNet>` | Zuken CR5000 | both element names accepted |
//! | `<PadStack net><LayerPad><PinRef/>` instead of `<LayerFeature><Set net><Pad>` | Altium | both containers accepted |
//! | Names prefixed with the step (`bd-sample:IC11`) | Zuken | [`strip_names`] |
//! | Names prefixed with a tag (`CMP:U1`, `NET:GND`) | KiCad | [`strip_prefix_tag`] |
//! | `{slash}`-escaped names | KiCad | [`crate::unescape_kicad_name`], gated on the producer |
//! | `<Pin name="1" number="0.0">` — `number` is an ORDINAL | Zuken | `name` preferred over `number` |
//! | `<Pin>` with neither `name` nor `number` | Allegro 16.6 | ignored rather than treated as a nameless pin |
//! | Placement on `<Xform x y>` rather than `<Location>` | converters | both read |
//! | `value` attribute on `<Component>` | converters | read, below the BOM's `Value` |
//! | `<LogicalNet>` at the document root, outside `<Ecad>` | converters | unkeyed connectivity is folded into the step |
//! | `layerFunction` of `PLANE` / `SIGNAL` / `MIXED`, not just `CONDUCTOR` | Allegro / Zuken | all counted as copper |
//! | Component side as `A-Component` / `Top Layer`, not `TOP` | Zuken / Altium | the `<Layer>` table's `side`, then name heuristics |
//!
//! ## Two sources of connectivity, and the disagreement between them
//!
//! `<LogicalNet>` is the netlist proper. `<LayerFeature><Set net="…">` is the
//! per-layer copper, whose `<Pad><PinRef/>` children *also* state which pin is
//! on which net. KiCad 9 writes only the second form (it emits no `LogicalNet`
//! at all); Allegro writes both. When both exist the `LogicalNet` view is used
//! and the copper view becomes a cross-check; when only the copper exists it is
//! the source, and pins that appear on two different nets across layers are a
//! genuine contradiction and are named in [`Ipc2581Stats::disagreements`].
//!
//! The other cross-check worth having is package-vs-placement. A component
//! names a `<Package>`, and the package declares its pins; the `<PinRef>`s name
//! pins too. The package's pins are used only when they ACCOUNT FOR every
//! referenced pin: a package that merely overlaps the netlist is a package for a
//! different part (KiCad 9 de-duplicates packages by pad geometry and so gives a
//! 2-pin LED the resistor's package, whose pins are `1`/`2` where the LED's are
//! `A`/`K`), and unioning the two grows the part phantom unconnected pads that
//! read downstream as open-pin findings. So the netlist's pins stand alone and the
//! mismatch is named.
//!
//! ## Deliberately unsupported
//!
//! * **Geometry.** `<Features>`, `<Polygon>`, `<Outline>`, `<Profile>`,
//!   padstack and primitive dictionaries are read past: hauksbee's IR holds no
//!   copper, so an IPC-2581 board reports clearance DRC as *not checked* rather
//!   than green. Pad *locations* are kept (they are on the pin).
//! * **Stackup and materials** (`<Stackup>`, `<StackupLayer>`, `<Spec>`,
//!   `<DielectricLayer>`), **drill/tooling** (`<DrillHole>`, `<Tool>`), **test**
//!   (`<Testpoint>`), **panel/fabrication** (`<Panel>`, `<Fabrication>`,
//!   `<Assembly>` function-mode sections beyond the BOM), **`<AvlHeader>` /
//!   approved-vendor data**, **`<Route>`/`<Phynet>` routing intent, and the
//!   logistics header beyond nothing at all.
//! * **Multi-step documents**: the first `<Step>` with components is read and
//!   the rest are named in [`Ipc2581Stats::steps`], not silently merged.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use crate::altium::VALUE_UNRESOLVED_KEY;
use crate::{Component, ExtractError, ExtractedBoard, Net, Pin};

/// Why a part read from an IPC-2581 document has no value.
///
/// IPC-2581 carries values in the `<Bom>` section, as a `<Textual>`
/// characteristic; a document exported without a BOM (a `FABRICATION` function
/// mode, or a tool that writes CadData only) has placement and netlist but no
/// values at all. `part` and `OEMDesignNumberRef` are device/library ids, not
/// values, and are never promoted to one.
pub const VALUE_UNRESOLVED_REASON: &str =
    "no value in the IPC-2581 document: its BOM carries no `Value` characteristic \
     for this part, and the part reference is a device id rather than a value";

/// Where the netlist came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetSource {
    /// `<LogicalNet>`: the netlist proper.
    LogicalNet,
    /// `<LayerFeature><Set net>`: the per-layer copper's pad references, which
    /// is all KiCad's exporter writes.
    LayerFeature,
}

impl NetSource {
    pub fn as_str(self) -> &'static str {
        match self {
            NetSource::LogicalNet => "LogicalNet",
            NetSource::LayerFeature => "LayerFeature/Set",
        }
    }
}

/// The honest accounting of an IPC-2581 read.
#[derive(Debug, Clone)]
pub struct Ipc2581Stats {
    /// The `revision` attribute on the root, verbatim ("B", "C", "").
    pub revision: String,
    /// `<Content><FunctionMode mode="…"/>`: what the producer says the file is
    /// for (`DESIGN`, `ASSEMBLY`, `FABRICATION`, `BOM`, `TEST`, …).
    pub function_mode: String,
    /// The producing tool from `<SoftwarePackage>`.
    pub producer: String,
    /// The step that was read.
    pub step: String,
    /// Every `<Step>` in the document.
    pub steps: Vec<String>,
    pub net_source: NetSource,
    /// `<Layer>` names whose `layerFunction` is `CONDUCTOR`.
    pub copper_layers: Vec<String>,
    /// `<Pad>` elements seen inside `<LayerFeature>`, per copper layer. The IR
    /// holds no geometry, so this is the accounting, not the pads themselves.
    pub pads_per_layer: Vec<(String, usize)>,
    /// Pins that ended up on a net.
    pub connected_pins: usize,
    /// Placements dropped as board artwork because they carry no pad at all.
    pub artwork: Vec<String>,
    /// True when the document has no `<Bom>` section, so no component has a
    /// value and no populate flag could be read: `dnp == false` everywhere means
    /// "the document did not say", not "the part is fitted".
    pub bom_absent: bool,
    /// Nets the document declares that touch no component pad at all.
    ///
    /// Not an error: a netlist may legitimately carry an unused net (Allegro's
    /// testcase3 declares exactly one, `Unused_04712D00`). But a net nothing is
    /// attached to is invisible in every downstream check, so it is counted and
    /// said out loud rather than passed along as if it were wired.
    pub nets_without_pads: Vec<String>,
    /// Cross-checks that did not agree, each a whole sentence.
    pub disagreements: Vec<String>,
}

impl Ipc2581Stats {
    /// The whole-sentence notes a report must carry for this read: where the
    /// connectivity came from, what was not checked, and every cross-check that
    /// disagreed. The board-input normalizer copies these onto its own notes, so
    /// a disagreement the reader found actually reaches the user.
    pub fn notes(&self) -> Vec<String> {
        let mut out = vec![format!(
            "IPC-2581 input (revision {}, FunctionMode {}): the netlist was read \
             from the document's {} section, not reverse-engineered from copper. \
             Clearance DRC and trace-geometry SI need the original layout file and \
             were not run.",
            rev_or_unknown(&self.revision),
            mode_or_unknown(&self.function_mode),
            self.net_source.as_str()
        )];
        if !self.artwork.is_empty() {
            out.push(format!(
                "{} placement(s) have no pad and were read as board artwork \
                 rather than parts: {}.",
                self.artwork.len(),
                sample_list(&self.artwork)
            ));
        }
        if self.bom_absent {
            out.push(
                "This document carries no BOM, so no component has a value and \
                 no populate/do-not-populate flag could be read: every placed \
                 part has been treated as fitted."
                    .to_string(),
            );
        }
        if !self.nets_without_pads.is_empty() {
            out.push(format!(
                "{} net(s) are declared but touch no component pad, so nothing \
                 downstream can see them: {}.",
                self.nets_without_pads.len(),
                sample_list(&self.nets_without_pads)
            ));
        }
        out.extend(self.disagreements.clone());
        out
    }
}

/// An IPC-2581 read: the board plus its accounting.
#[derive(Debug)]
pub struct Ipc2581Extraction {
    pub board: ExtractedBoard,
    pub stats: Ipc2581Stats,
}

/// Content sniff: an XML document whose root element is `IPC-2581`. Matched on
/// the root element rather than the namespace URI because the un-namespaced form
/// real converters emit is still an IPC-2581 document; the namespace, when
/// present, is accepted as a second confirmation.
pub fn looks_like_ipc2581(bytes: &[u8]) -> bool {
    let window = &bytes[..bytes.len().min(4096)];
    let head = String::from_utf8_lossy(window);
    // The root element, allowing an XML declaration, comments, a BOM and any
    // namespace prefix in front of it.
    head.contains("<IPC-2581")
        || head.contains(":IPC-2581")
        || head.contains("http://webstds.ipc.org/2581")
}

/// The prefixes producers put on names for cross-section uniqueness. Stripping
/// only these, rather than everything before the first colon, keeps a net
/// genuinely called `A:B` intact.
const NAME_PREFIXES: &[&str] = &[
    "CMP", "NET", "PIN", "PKG", "LAYER", "BOARD", "REF", "DRILL", "BOM", "STEP", "PART", "PAD",
];

/// Strip a producer's uniqueness prefix, where the prefix may also be the STEP
/// NAME.
///
/// Zuken's CR5000 exporter prefixes every name with the step: `refDes=
/// "bd-sample:IC18"`, `componentRef="bd-sample:IC11"`, `name="bd-sample:RESET"`.
/// The connectivity is internally consistent either way, so the netlist still
/// resolved — but the reference designator reaching the IR was `bd-sample:IC18`,
/// which no model binder can recognise as an IC, and every net was called
/// `bd-sample:RESET`. The step name is only stripped when it is followed by a
/// colon and something else, so a step called `A` cannot eat a net named `A:B`
/// belonging to nothing.
fn strip_names(name: &str, step: &str) -> String {
    let once = strip_prefix_tag(name);
    if step.is_empty() {
        return once.to_string();
    }
    match once.split_once(':') {
        Some((head, rest)) if head == step && !rest.is_empty() => rest.to_string(),
        _ => once.to_string(),
    }
}

/// Strip a producer's `<TAG>:` uniqueness prefix from a name.
fn strip_prefix_tag(name: &str) -> &str {
    match name.split_once(':') {
        Some((tag, rest)) if NAME_PREFIXES.contains(&tag) && !rest.is_empty() => rest,
        _ => name,
    }
}

/// The attributes of an element as a name → unescaped-value map, keyed on the
/// attribute's LOCAL name so `xsi:type` style prefixes do not change a lookup.
fn attrs(e: &BytesStart<'_>) -> HashMap<String, String> {
    e.attributes()
        .flatten()
        .map(|a| {
            // quick-xml does not unescape attribute values, and IPC-2581 net
            // names really do contain `&amp;` (a `CLK&RST` bus name) — see the
            // same note in `eagle.rs`. The non-deprecated successor takes an
            // `XmlVersion` quick-xml does not export.
            #[allow(deprecated)]
            let value = a
                .unescape_value()
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned());
            let key = a.key.local_name();
            (
                String::from_utf8_lossy(key.as_ref()).into_owned(),
                value,
            )
        })
        .collect()
}

/// An element's name without its namespace prefix, so a namespaced and a bare
/// document match identically. Taken from the raw name rather than quick-xml's
/// `local_name` so the same helper serves start and end tags.
fn local(name: &[u8]) -> String {
    let name = String::from_utf8_lossy(name);
    match name.rsplit_once(':') {
        Some((_, rest)) => rest.to_string(),
        None => name.into_owned(),
    }
}

/// One `<Component>` as the document states it.
#[derive(Default, Clone)]
struct RawComponent {
    refdes: String,
    package_ref: String,
    part: String,
    layer_ref: String,
    /// A `value` attribute on `<Component>`: outside the schema, but written by
    /// real converters and the only value source in a document with no `<Bom>`.
    value: String,
    x: f64,
    y: f64,
    rotation: f64,
    mirrored: bool,
}

/// One `<BomItem><RefDes>` row.
#[derive(Default, Clone)]
struct BomRow {
    package_ref: String,
    populate: bool,
    layer_ref: String,
    /// The item's `<Characteristics><Textual>` pairs, shared by every RefDes on
    /// the item.
    characteristics: Vec<(String, String)>,
    oem_design_number: String,
}

/// Everything one pass over the document collected.
#[derive(Default)]
struct Doc {
    revision: String,
    function_mode: String,
    producer: String,
    units_mm: f64,
    steps: Vec<String>,
    /// Layer name → (layerFunction, side).
    layers: BTreeMap<String, (String, String)>,
    /// Step → packages: package name → declared pin numbers.
    packages: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    /// Step → components.
    components: BTreeMap<String, Vec<RawComponent>>,
    /// Step → `LogicalNet` name → (component, pin) pairs.
    logical_nets: BTreeMap<String, Vec<(String, Vec<(String, String)>)>>,
    /// Step → layer → net name → (component, pin) pairs from `<Pad><PinRef>`.
    layer_nets: BTreeMap<String, Vec<(String, String, Vec<(String, String)>)>>,
    /// Step → layer → `<Pad>` count.
    pads_per_layer: BTreeMap<String, BTreeMap<String, usize>>,
    /// Refdes → BOM row.
    bom: BTreeMap<String, BomRow>,
    /// Pad locations, keyed (step, component, pin), in the document's units.
    pad_positions: HashMap<(String, String, String), (f64, f64)>,
}

/// Parse the document once into [`Doc`].
#[allow(clippy::too_many_lines)]
fn scan(text: &str) -> Result<Doc, ExtractError> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut doc = Doc {
        units_mm: 1.0,
        ..Doc::default()
    };
    // Where we are. IPC-2581 nests deeply but the elements we want are
    // unambiguous by name, so a handful of "current" cursors is enough.
    let mut step = String::new();
    let mut in_bom_item: Option<(Vec<String>, BomRow)> = None;
    let mut cur_package: Option<String> = None;
    let mut cur_component: Option<RawComponent> = None;
    let mut cur_logical_net: Option<(String, Vec<(String, String)>)> = None;
    let mut cur_layer_feature: Option<String> = None;
    let mut cur_set: Option<(String, Vec<(String, String)>)> = None;
    let mut cur_pad: Option<(f64, f64)> = None;
    let mut saw_root = false;
    let mut buf = Vec::new();

    loop {
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(|e| ExtractError::Xml(format!("IPC-2581: {e}")))?;
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(ev, Event::Empty(_));
                let name = local(e.name().as_ref());
                let a = attrs(e);
                match name.as_str() {
                    "IPC-2581" => {
                        saw_root = true;
                        doc.revision = a.get("revision").cloned().unwrap_or_default();
                    }
                    "FunctionMode" => {
                        doc.function_mode = a.get("mode").cloned().unwrap_or_default();
                    }
                    "SoftwarePackage" => {
                        let n = a.get("name").cloned().unwrap_or_default();
                        let r = a.get("revision").cloned().unwrap_or_default();
                        doc.producer = format!("{n} {r}").trim().to_string();
                    }
                    "CadHeader" => {
                        if let Some(u) = a.get("units") {
                            doc.units_mm = unit_scale(u);
                        }
                    }
                    "Layer" => {
                        if let Some(n) = a.get("name") {
                            doc.layers.insert(
                                strip_names(n, &step).to_string(),
                                (
                                    a.get("layerFunction").cloned().unwrap_or_default(),
                                    a.get("side").cloned().unwrap_or_default(),
                                ),
                            );
                        }
                    }
                    // `<Step>` inside `<CadData>` is the design step; a
                    // `<StepRef>` elsewhere is only a pointer and has its own
                    // element name, so this never picks one up.
                    "Step" => {
                        step = a
                            .get("name")
                            .map(|n| strip_names(n, &step).to_string())
                            .unwrap_or_default();
                        if !doc.steps.contains(&step) {
                            doc.steps.push(step.clone());
                        }
                    }
                    "Package" => {
                        let n = a
                            .get("name")
                            .map(|n| strip_names(n, &step).to_string())
                            .unwrap_or_default();
                        doc.packages
                            .entry(step.clone())
                            .or_default()
                            .entry(n.clone())
                            .or_default();
                        cur_package = (!empty).then_some(n);
                    }
                    "Pin" => {
                        // A `<Pin>` inside `<Package>` declares a pin; the
                        // element name is reused nowhere else we read.
                        //
                        // `name` is preferred over `number` because producers
                        // disagree about which one holds the pin's identity.
                        // KiCad and Allegro write `<Pin number="7"/>`; Zuken
                        // writes `<Pin name="1" type="THRU" number="0.0"/>`,
                        // where `number` is the pin's ORDINAL in the package and
                        // `name` is what the netlist references. Reading
                        // `number` on a Zuken package gave every part pins
                        // called "0.0", "1.0", "2.0" — names no `<PinRef>` could
                        // ever match, so every package looked wrong.
                        let pin = a
                            .get("name")
                            .or_else(|| a.get("number"))
                            // A `<Pin>` with neither names nothing. Allegro's
                            // rev-A exporter writes exactly that for some
                            // packages, and keeping the empty string made every
                            // such package "declare" a list of nameless pins that
                            // no `<PinRef>` could match — which then reported 41
                            // of 42 components as having the wrong package.
                            .filter(|v| !v.trim().is_empty());
                        if let (Some(pkg), Some(num)) = (cur_package.as_ref(), pin) {
                            if let Some(pins) = doc
                                .packages
                                .get_mut(&step)
                                .and_then(|m| m.get_mut(pkg.as_str()))
                            {
                                pins.push(strip_names(num, &step).to_string());
                            }
                        }
                    }
                    "Component" => {
                        let c = RawComponent {
                            refdes: a
                                .get("refDes")
                                .map(|v| strip_names(v, &step).to_string())
                                .unwrap_or_default(),
                            package_ref: a
                                .get("packageRef")
                                .map(|v| strip_names(v, &step).to_string())
                                .unwrap_or_default(),
                            part: a
                                .get("part")
                                .map(|v| strip_names(v, &step).to_string())
                                .unwrap_or_default(),
                            layer_ref: a
                                .get("layerRef")
                                .map(|v| strip_names(v, &step).to_string())
                                .unwrap_or_default(),
                            // Not in the schema, but converters really do write
                            // it, and a document with no `<Bom>` has no other
                            // place a value could come from.
                            value: a.get("value").cloned().unwrap_or_default(),
                            ..RawComponent::default()
                        };
                        if empty {
                            doc.components.entry(step.clone()).or_default().push(c);
                        } else {
                            cur_component = Some(c);
                        }
                    }
                    "Location" => {
                        let x = num(&a, "x");
                        let y = num(&a, "y");
                        if let Some(c) = cur_component.as_mut() {
                            c.x = x;
                            c.y = y;
                        } else if cur_pad.is_some() {
                            cur_pad = Some((x, y));
                        }
                    }
                    "Xform" => {
                        if let Some(c) = cur_component.as_mut() {
                            c.rotation = num(&a, "rotation");
                            c.mirrored = a
                                .get("mirror")
                                .is_some_and(|v| v.eq_ignore_ascii_case("true"));
                            // Some producers put the placement on the `<Xform>`
                            // rather than in a sibling `<Location>`; taking it
                            // only from `<Location>` left every part at (0, 0).
                            if a.contains_key("x") || a.contains_key("y") {
                                c.x = num(&a, "x");
                                c.y = num(&a, "y");
                            }
                        }
                    }
                    "LogicalNet" => {
                        let n = a
                            .get("name")
                            .map(|v| strip_names(v, &step).to_string())
                            .unwrap_or_default();
                        if empty {
                            doc.logical_nets
                                .entry(step.clone())
                                .or_default()
                                .push((n, Vec::new()));
                        } else {
                            cur_logical_net = Some((n, Vec::new()));
                        }
                    }
                    // Both of these are cleared on `Empty` as well as on `End`: a
                    // self-closing `<Set/>` has no `End` event, so leaving the
                    // cursor alive attributed the NEXT element's `<PinRef>`s to a
                    // set that had already finished, and they were then discarded
                    // when the stale set was flushed.
                    "LayerFeature" => {
                        cur_layer_feature = (!empty).then(|| {
                            a.get("layerRef")
                                .map(|v| strip_names(v, &step).to_string())
                                .unwrap_or_default()
                        });
                    }
                    // `<Set net>` is the schema's net-bearing copper container.
                    // `<PadStack net>` is Altium's: it writes
                    // `<Step><PadStack net="+5V"><LayerPad><PinRef/></LayerPad>`
                    // and no `<Set>` carries connectivity at all, so matching
                    // only `<Set>` read a 27-component Altium board as having 15
                    // nets and zero connections.
                    "Set" | "PadStack" => {
                        // A container without a net is geometry with no
                        // connectivity (silkscreen, mask, an unnetted padstack);
                        // it is still walked so nested pads are counted.
                        let n = a.get("net").map(|v| strip_names(v, &step).to_string());
                        cur_set = (!empty).then(|| (n.unwrap_or_default(), Vec::new()));
                    }
                    // `<LayerPad>` is the per-layer pad of an Altium `<PadStack>`,
                    // the same role `<Pad>` plays inside a `<Set>`.
                    "Pad" | "LayerPad" => {
                        cur_pad = Some((f64::NAN, f64::NAN));
                        if let Some(layer) = cur_layer_feature.as_ref() {
                            *doc.pads_per_layer
                                .entry(step.clone())
                                .or_default()
                                .entry(layer.clone())
                                .or_default() += 1;
                        }
                        if empty {
                            cur_pad = None;
                        }
                    }
                    // `LogicalNetPin` is Zuken CR5000's element name for the same
                    // thing `PinRef` is: `<LogicalNet name="RESET">
                    // <LogicalNetPin pin="13" componentRef="IC11"/></LogicalNet>`.
                    // Both are schema-valid, and matching only `PinRef` read
                    // every Zuken document as a board with 1549 declared nets and
                    // not one connection — placement and netlist both present,
                    // and the reader silently joined nothing to anything.
                    "PinRef" | "LogicalNetPin" => {
                        let comp = a
                            .get("componentRef")
                            .map(|v| strip_names(v, &step).to_string())
                            .unwrap_or_default();
                        let pin = a
                            .get("pin")
                            .map(|v| strip_names(v, &step).to_string())
                            .unwrap_or_default();
                        if comp.is_empty() {
                            continue;
                        }
                        if let Some((x, y)) = cur_pad {
                            if x.is_finite() && y.is_finite() {
                                doc.pad_positions
                                    .insert((step.clone(), comp.clone(), pin.clone()), (x, y));
                            }
                        }
                        if let Some((_, pins)) = cur_logical_net.as_mut() {
                            pins.push((comp, pin));
                        } else if let Some((_, pins)) = cur_set.as_mut() {
                            pins.push((comp, pin));
                        }
                    }
                    "BomItem" => {
                        in_bom_item = Some((
                            Vec::new(),
                            BomRow {
                                populate: true,
                                oem_design_number: a
                                    .get("OEMDesignNumberRef")
                                    .map(|v| strip_names(v, &step).to_string())
                                    .unwrap_or_default(),
                                ..BomRow::default()
                            },
                        ));
                    }
                    "RefDes" => {
                        if let (Some((refs, row)), Some(n)) = (in_bom_item.as_mut(), a.get("name")) {
                            refs.push(strip_names(n, &step).to_string());
                            // A BomItem groups identical parts, so package /
                            // layer / populate are per-RefDes; the last one
                            // wins only for the shared fields, and per-refdes
                            // values are recorded as they are seen.
                            row.package_ref = a
                                .get("packageRef")
                                .map(|v| strip_names(v, &step).to_string())
                                .unwrap_or_else(|| row.package_ref.clone());
                            row.layer_ref = a
                                .get("layerRef")
                                .map(|v| strip_names(v, &step).to_string())
                                .unwrap_or_else(|| row.layer_ref.clone());
                            row.populate = a
                                .get("populate")
                                .map(|v| !v.eq_ignore_ascii_case("false"))
                                .unwrap_or(true);
                            let mut per = row.clone();
                            per.characteristics = Vec::new();
                            doc.bom.insert(strip_names(n, &step).to_string(), per);
                        }
                    }
                    "Textual" => {
                        if let Some((_, row)) = in_bom_item.as_mut() {
                            if let (Some(k), Some(v)) = (
                                a.get("textualCharacteristicName"),
                                a.get("textualCharacteristicValue"),
                            ) {
                                row.characteristics.push((k.clone(), v.clone()));
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::End(ref e) => match local(e.name().as_ref()).as_str() {
                "Package" => cur_package = None,
                "Component" => {
                    if let Some(c) = cur_component.take() {
                        doc.components.entry(step.clone()).or_default().push(c);
                    }
                }
                "LogicalNet" => {
                    if let Some(n) = cur_logical_net.take() {
                        doc.logical_nets.entry(step.clone()).or_default().push(n);
                    }
                }
                "Set" | "PadStack" => {
                    if let Some((net, pins)) = cur_set.take() {
                        if !net.is_empty() {
                            doc.layer_nets.entry(step.clone()).or_default().push((
                                cur_layer_feature.clone().unwrap_or_default(),
                                net,
                                pins,
                            ));
                        }
                    }
                }
                "LayerFeature" => cur_layer_feature = None,
                "Pad" | "LayerPad" => cur_pad = None,
                "BomItem" => {
                    if let Some((refs, row)) = in_bom_item.take() {
                        for r in refs {
                            if let Some(entry) = doc.bom.get_mut(&r) {
                                entry.characteristics = row.characteristics.clone();
                                if entry.oem_design_number.is_empty() {
                                    entry.oem_design_number = row.oem_design_number.clone();
                                }
                            }
                        }
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    if !saw_root {
        return Err(ExtractError::WrongRoot {
            expected: "IPC-2581",
            found: None,
        });
    }
    // `<Bom>` precedes `<Ecad>`, so its reference designators were stripped
    // before any step name was known. Re-key them now that the steps are, or a
    // Zuken-style `bd-sample:R32` in the BOM would never match the `R32` the
    // placement resolved to and every value would be lost.
    if !doc.steps.is_empty() {
        doc.bom = std::mem::take(&mut doc.bom)
            .into_iter()
            .map(|(k, v)| {
                let stripped = doc
                    .steps
                    .iter()
                    .map(|s| strip_names(&k, s))
                    .find(|s| *s != k)
                    .unwrap_or_else(|| k.clone());
                (stripped, v)
            })
            .collect();
    }
    Ok(doc)
}

/// A numeric attribute, or 0.0.
///
/// Non-finite values are dropped rather than adopted: `parse::<f64>()` accepts
/// `inf`, `NaN` and an overflowing exponent, and a distance compared against NaN
/// is silently false, which turns a clearance or length check into a meaningless
/// pass. The native readers refuse such a file outright
/// (`pcb::reject_non_finite_geometry`); an IPC-2581 coordinate only ever reaches
/// a pad position, so falling back to 0.0 keeps the same guarantee — no
/// non-finite number enters the IR — without discarding a whole document over one
/// bad attribute.
fn num(a: &HashMap<String, String>, key: &str) -> f64 {
    a.get(key)
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(0.0)
}

/// IPC-2581 `units` values, as a multiplier to millimetres.
fn unit_scale(units: &str) -> f64 {
    match units.to_ascii_uppercase().as_str() {
        "INCH" | "INCHES" => 25.4,
        "MICRON" | "MICRONS" | "MICROMETER" => 0.001,
        // MILLIMETER is the common case and the fallback: a producer that omits
        // or misspells the attribute is far more likely to mean mm than inches,
        // and guessing inches would inflate every coordinate 25-fold.
        _ => 1.0,
    }
}

/// Extract the connectivity model from an IPC-2581 document.
pub fn extract(text: &str) -> Result<Ipc2581Extraction, ExtractError> {
    let doc = scan(text)?;
    let mut disagreements: Vec<String> = Vec::new();
    // KiCad escapes every name it writes into an exchange format
    // ([`crate::unescape_kicad_name`]). Escaped names are used INTERNALLY (the
    // pin → net map and the package lookups all key on what the document says)
    // and un-escaped only on the way into the IR, so nothing depends on the
    // order the two forms are compared in.
    let kicad = doc.producer.to_ascii_uppercase().contains("KICAD");
    let unescape = |s: &str| -> String {
        if kicad {
            crate::unescape_kicad_name(s)
        } else {
            s.to_string()
        }
    };

    // The step to read: the first that has components, else the first at all.
    let step = doc
        .steps
        .iter()
        .find(|s| doc.components.get(*s).is_some_and(|c| !c.is_empty()))
        .or_else(|| doc.steps.first())
        .cloned()
        .unwrap_or_default();
    let raw_components = doc.components.get(&step).cloned().unwrap_or_default();

    if raw_components.is_empty() {
        return Err(ExtractError::Ipc2581(no_cad_data_message(&doc)));
    }
    if doc.steps.len() > 1 {
        disagreements.push(format!(
            "this document has {} steps ({}); step '{step}' was read and the others \
             were not",
            doc.steps.len(),
            doc.steps.join(", ")
        ));
    }

    // ── Connectivity ─────────────────────────────────────────────────────────
    //
    // Connectivity is looked up under the step it belongs to, and then under the
    // UNKEYED bucket. The schema puts `<LogicalNet>` inside `<Step>`, but real
    // documents put it at the root, outside `<Ecad>` altogether; those nets were
    // filed under the empty step and, since the components were filed under a
    // real one, the two never met — a fully-specified 10-part board read as
    // having nets and no connections at all.
    fn for_step<T: Clone>(m: &BTreeMap<String, Vec<T>>, step: &str) -> Vec<T> {
        let mut v = m.get(step).cloned().unwrap_or_default();
        if !step.is_empty() {
            v.extend(m.get("").cloned().unwrap_or_default());
        }
        v
    }
    let logical = for_step(&doc.logical_nets, &step);
    let layer_sets = for_step(&doc.layer_nets, &step);
    let net_source = if logical.iter().any(|(_, pins)| !pins.is_empty()) {
        NetSource::LogicalNet
    } else {
        NetSource::LayerFeature
    };

    // (component, pin) → net name, plus first-appearance net order.
    let mut net_order: Vec<String> = Vec::new();
    let mut pin_net: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut contradictions: BTreeSet<String> = BTreeSet::new();
    let note_net = |name: &str, order: &mut Vec<String>| {
        if !order.iter().any(|n| n == name) {
            order.push(name.to_string());
        }
    };
    match net_source {
        NetSource::LogicalNet => {
            for (name, pins) in &logical {
                note_net(name, &mut net_order);
                for (comp, pin) in pins {
                    assign(&mut pin_net, &mut contradictions, comp, pin, name);
                }
            }
            // The copper view is now the check, not a second source.
            let mut copper_only = 0usize;
            let mut mismatched: BTreeSet<String> = BTreeSet::new();
            for (_, net, pins) in &layer_sets {
                for (comp, pin) in pins {
                    match pin_net.get(&(comp.clone(), pin.clone())) {
                        Some(have) if have != net => {
                            mismatched.insert(format!("{comp}.{pin} ({have} vs {net})"));
                        }
                        None => copper_only += 1,
                        _ => {}
                    }
                }
            }
            if !mismatched.is_empty() {
                let v: Vec<String> = mismatched.into_iter().collect();
                disagreements.push(format!(
                    "{} pin(s) are on a different net in the copper (LayerFeature) \
                     than in the netlist (LogicalNet); the netlist was used: {}",
                    v.len(),
                    sample_list(&v)
                ));
            }
            if copper_only > 0 {
                disagreements.push(format!(
                    "{copper_only} pad reference(s) in the copper name a pin the \
                     netlist does not place on any net"
                ));
            }
        }
        NetSource::LayerFeature => {
            for (_, net, pins) in &layer_sets {
                note_net(net, &mut net_order);
                for (comp, pin) in pins {
                    assign(&mut pin_net, &mut contradictions, comp, pin, net);
                }
            }
            if !logical.is_empty() {
                disagreements.push(format!(
                    "this document declares {} LogicalNet element(s) but none of \
                     them names a pin, so connectivity was taken from the copper \
                     (LayerFeature/Set) instead",
                    logical.len()
                ));
            }
        }
    }
    if !contradictions.is_empty() {
        let v: Vec<String> = contradictions.into_iter().collect();
        disagreements.push(format!(
            "{} pin(s) are placed on two different nets by the same source, which \
             cannot both be true; the first was kept: {}",
            v.len(),
            sample_list(&v)
        ));
    }

    if net_order.is_empty() {
        return Err(ExtractError::Ipc2581(format!(
            "this IPC-2581 document (revision {}, FunctionMode {}) places {} \
             component(s) but declares no nets: it has no <LogicalNet> with pin \
             references and no <LayerFeature><Set net=\"…\"> copper. hauksbee \
             will not report on a board it cannot wire up. Re-export with the \
             DESIGN or ASSEMBLY function mode, which carries the netlist",
            rev_or_unknown(&doc.revision),
            mode_or_unknown(&doc.function_mode),
            raw_components.len(),
        )));
    }

    let net_ids: HashMap<&str, i64> = net_order
        .iter()
        .enumerate()
        .map(|(i, n)| (n.as_str(), i as i64 + 1))
        .collect();
    let nets: Vec<Net> = net_order
        .iter()
        .map(|n| Net {
            id: net_ids[n.as_str()],
            name: unescape(n),
        })
        .collect();

    // ── Components ───────────────────────────────────────────────────────────
    // Pins referenced per component, so a wrong package assignment can be
    // detected against the pins the connectivity actually names.
    let mut referenced: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (comp, pin) in pin_net.keys() {
        referenced
            .entry(comp.as_str())
            .or_default()
            .insert(pin.as_str());
    }
    let packages = doc.packages.get(&step).cloned().unwrap_or_default();

    let mut wrong_packages: Vec<String> = Vec::new();
    let mut components: Vec<Component> = Vec::with_capacity(raw_components.len());
    let mut connected_pins = 0usize;
    let mut orphan_refs: BTreeSet<String> = BTreeSet::new();
    let placed: HashSet<&str> = raw_components.iter().map(|c| c.refdes.as_str()).collect();
    // Both connectivity sources are searched for a dangling component, not just
    // the one that won: a copper `PinRef` naming a component the document never
    // places is a broken reference whichever view it came from.
    let all_refs = logical
        .iter()
        .flat_map(|(_, pins)| pins.iter())
        .chain(layer_sets.iter().flat_map(|(_, _, pins)| pins.iter()));
    for (comp, _) in all_refs {
        if !placed.contains(comp.as_str()) {
            orphan_refs.insert(comp.clone());
        }
    }
    if !orphan_refs.is_empty() {
        let v: Vec<String> = orphan_refs.into_iter().collect();
        disagreements.push(format!(
            "{} net reference(s) name a component the document never places, so \
             those connections have nothing to attach to: {}",
            v.len(),
            sample_list(&v)
        ));
    }

    for (idx, c) in raw_components.iter().enumerate() {
        let declared: Vec<String> = packages.get(&c.package_ref).cloned().unwrap_or_default();
        let refd = referenced.get(c.refdes.as_str());
        // The package a component names must ACCOUNT FOR every pin the netlist
        // puts on that component: `<Package>` is the statement of what pins the
        // part has, so a referenced pin the package does not declare means the
        // package reference is for a different part.
        //
        // Requiring containment rather than mere overlap matters. KiCad
        // de-duplicates packages by pad geometry, so a 2-pin LED gets the axial
        // resistor's package: with an overlap test, a part whose pins are `1` and
        // `K` against a package declaring `1`,`2`,`3` "fitted" on the strength of
        // `1` alone and was emitted with FOUR pads — `2` and `3` fabricated and
        // unconnected, which downstream reads as two phantom open-pin findings
        // and a wrong pin count for binding. So: the package's pins are used only
        // when they are a superset of the referenced ones, and otherwise the
        // netlist's pins stand alone and the mismatch is named.
        let unaccounted: Vec<&str> = match refd {
            Some(r) => r
                .iter()
                .filter(|p| !declared.iter().any(|d| d == *p))
                .copied()
                .collect(),
            None => Vec::new(),
        };
        let package_fits = declared.is_empty() || unaccounted.is_empty();
        if !package_fits {
            wrong_packages.push(format!(
                "{} (package {} declares {}, netlist also uses {})",
                c.refdes,
                c.package_ref,
                sample_list(&declared),
                sample_list(&unaccounted.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
            ));
        }
        let mut numbers: Vec<String> = if package_fits { declared.clone() } else { Vec::new() };
        if let Some(r) = refd {
            for pin in r {
                if !numbers.iter().any(|n| n == pin) {
                    numbers.push((*pin).to_string());
                }
            }
        }

        let pins: Vec<Pin> = numbers
            .iter()
            .map(|number| {
                let net = pin_net
                    .get(&(c.refdes.clone(), number.clone()))
                    .and_then(|n| net_ids.get(n.as_str()).copied());
                if net.is_some() {
                    connected_pins += 1;
                }
                let position = doc
                    .pad_positions
                    .get(&(step.clone(), c.refdes.clone(), number.clone()))
                    .map(|(x, y)| (x * doc.units_mm, y * doc.units_mm));
                Pin {
                    number: unescape(number),
                    net,
                    function: String::new(),
                    kind: String::new(),
                    position,
                }
            })
            .collect();

        let bom = doc.bom.get(&c.refdes);
        let mut properties: Vec<(String, String)> = Vec::new();
        // The `value` attribute on `<Component>` when the producer wrote one; the
        // BOM's `Value` characteristic overrides it below, being the schema's own
        // place for it.
        let mut value = c.value.clone();
        if let Some(b) = bom {
            for (k, v) in &b.characteristics {
                if v.is_empty() {
                    continue;
                }
                if k.eq_ignore_ascii_case("value") {
                    value = v.clone();
                } else {
                    properties.push((k.clone(), v.clone()));
                }
            }
        }
        if value.is_empty() {
            // `part` and `OEMDesignNumberRef` are library/device identifiers, not
            // component values (KiCad appends the value to the part name, Allegro
            // puts a device type there); promoting either would fabricate values.
            properties.push((
                VALUE_UNRESOLVED_KEY.to_string(),
                VALUE_UNRESOLVED_REASON.to_string(),
            ));
        }

        // The BOM's packageRef is the real footprint where CadData's has been
        // de-duplicated by geometry, so it wins when present.
        let footprint = bom
            .map(|b| b.package_ref.clone())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| c.package_ref.clone());
        let layer_ref = if c.layer_ref.is_empty() {
            bom.map(|b| b.layer_ref.clone()).unwrap_or_default()
        } else {
            c.layer_ref.clone()
        };

        // Kept verbatim, and merged (not renamed) when the document places two
        // instances under one designator; see
        // [`crate::merge_duplicate_references`].
        let reference = if c.refdes.is_empty() {
            format!("UNK{idx}")
        } else {
            unescape(&c.refdes)
        };

        components.push(Component {
            reference,
            value,
            lib_id: c.part.clone(),
            footprint,
            position: Some((c.x * doc.units_mm, c.y * doc.units_mm, c.rotation)),
            layer: side_of(&layer_ref, c.mirrored, &doc.layers).to_string(),
            properties,
            dnp: bom.is_some_and(|b| !b.populate),
            pins,
        });
    }
    // A placement with no pad is board artwork, not a part (see the same rule in
    // `pcb.rs` and [`crate::odbpp`]): dropped, and named in the stats.
    let artwork: Vec<String> = components
        .iter()
        .filter(|c| c.pins.is_empty())
        .map(|c| c.reference.clone())
        .collect();
    let components = crate::merge_duplicate_references(
        components.into_iter().filter(|c| !c.pins.is_empty()).collect(),
    );

    if !wrong_packages.is_empty() {
        disagreements.push(format!(
            "{} component(s) name a package whose declared pins are entirely \
             different from the pins the netlist puts on them, so the package \
             reference is wrong and only the netlist's pins were used: {}",
            wrong_packages.len(),
            sample_list(&wrong_packages)
        ));
    }
    if connected_pins == 0 {
        return Err(ExtractError::Ipc2581(format!(
            "this IPC-2581 document declares {} net(s) and places {} component(s), \
             but not one net reference resolves to a placed component's pin, so the \
             board has no connectivity to check",
            nets.len(),
            components.len()
        )));
    }

    // `PLANE` is copper as much as `CONDUCTOR` is (Allegro names its power and
    // ground layers `PLANE`, Zuken names all of them `SIGNAL`), so counting only
    // `CONDUCTOR` under-reported the stackup of every multi-layer board that has
    // planes: testcase10 reads 44 layers, 8 of them copper, of which 4 are planes.
    let copper_layers: Vec<String> = doc
        .layers
        .iter()
        .filter(|(_, (func, _))| {
            ["CONDUCTOR", "PLANE", "SIGNAL", "MIXED"]
                .iter()
                .any(|k| func.eq_ignore_ascii_case(k))
        })
        .map(|(n, _)| n.clone())
        .collect();
    let pads_per_layer: Vec<(String, usize)> = doc
        .pads_per_layer
        .get(&step)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), *v)).collect())
        .unwrap_or_default();

    // Nets nothing is attached to. Computed from the finished board rather than
    // from the document, so it catches a net lost to any of the resolution steps
    // above as well as one the document really declares unused.
    let attached: HashSet<i64> = components
        .iter()
        .flat_map(|c| c.pins.iter())
        .filter_map(|p| p.net)
        .collect();
    let nets_without_pads: Vec<String> = nets
        .iter()
        .filter(|n| !attached.contains(&n.id))
        .map(|n| n.name.clone())
        .collect();

    Ok(Ipc2581Extraction {
        board: ExtractedBoard {
            // IPC-2581 step names carry a `BOARD:` prefix from KiCad and the
            // design name from Allegro; either is a better board name than the
            // file stem, and the prefix is already stripped.
            name: unescape(&step),
            nets,
            components,
        },
        stats: Ipc2581Stats {
            revision: doc.revision,
            function_mode: doc.function_mode,
            producer: doc.producer,
            step,
            steps: doc.steps,
            net_source,
            copper_layers,
            pads_per_layer,
            connected_pins,
            artwork,
            bom_absent: doc.bom.is_empty(),
            nets_without_pads,
            disagreements,
        },
    })
}

fn assign(
    pin_net: &mut BTreeMap<(String, String), String>,
    contradictions: &mut BTreeSet<String>,
    comp: &str,
    pin: &str,
    net: &str,
) {
    let key = (comp.to_string(), pin.to_string());
    match pin_net.get(&key) {
        Some(have) if have != net => {
            contradictions.insert(format!("{comp}.{pin} ({have} and {net})"));
        }
        Some(_) => {}
        None => {
            pin_net.insert(key, net.to_string());
        }
    }
}

/// Which side a component is on. The `<Layer>` table's `side` is authoritative
/// when the layer is declared; otherwise the layer name and the mirror flag are.
fn side_of(layer_ref: &str, mirrored: bool, layers: &BTreeMap<String, (String, String)>) -> &'static str {
    if let Some((_, side)) = layers.get(layer_ref) {
        match side.to_ascii_uppercase().as_str() {
            "TOP" => return "F.Cu",
            "BOTTOM" => return "B.Cu",
            _ => {}
        }
    }
    let l = layer_ref.to_ascii_uppercase();
    if l.contains("BOT") || l.starts_with("B.") {
        return "B.Cu";
    }
    if l.contains("TOP") || l.starts_with("F.") {
        return "F.Cu";
    }
    if mirrored {
        "B.Cu"
    } else {
        "F.Cu"
    }
}

/// The refusal for an IPC-2581 file that is a real, valid document but is not a
/// design: a BOM-only, fabrication-only or stackup-only export. It names the
/// function mode, what the file DOES carry, and what to re-export.
fn no_cad_data_message(doc: &Doc) -> String {
    let mut has: Vec<String> = Vec::new();
    if !doc.bom.is_empty() {
        has.push(format!(
            "a BOM with {} reference designator(s)",
            doc.bom.len()
        ));
    }
    if !doc.layers.is_empty() {
        has.push(format!("a stackup of {} layer(s)", doc.layers.len()));
    }
    let net_pins: usize = doc
        .logical_nets
        .values()
        .flatten()
        .map(|(_, p)| p.len())
        .sum();
    if net_pins > 0 {
        has.push(format!("{net_pins} net pin reference(s)"));
    }
    if !doc.steps.is_empty() {
        has.push(format!("step(s) {}", doc.steps.join(", ")));
    }
    format!(
        "this IPC-2581 document (revision {}, FunctionMode {}) places no \
         components: it carries {} but no <Ecad><CadData><Step><Component> \
         placement. hauksbee needs the design/assembly export, which carries the \
         placement and netlist, not the {}-only one",
        rev_or_unknown(&doc.revision),
        mode_or_unknown(&doc.function_mode),
        if has.is_empty() {
            "nothing hauksbee can use".to_string()
        } else {
            has.join(", ")
        },
        if doc.function_mode.is_empty() {
            "partial".to_string()
        } else {
            doc.function_mode.to_ascii_lowercase()
        }
    )
}

fn rev_or_unknown(rev: &str) -> String {
    if rev.is_empty() {
        "unstated".to_string()
    } else {
        rev.to_string()
    }
}

fn mode_or_unknown(mode: &str) -> String {
    if mode.is_empty() {
        "unstated".to_string()
    } else {
        mode.to_string()
    }
}

fn sample_list(items: &[String]) -> String {
    const MAX: usize = 6;
    if items.len() <= MAX {
        return items.join(", ");
    }
    format!("{}, and {} more", items[..MAX].join(", "), items.len() - MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A revision-C document in KiCad's shape: namespaced, prefixed names, no
    /// `LogicalNet` (connectivity in the copper), a BOM carrying values and DNP.
    fn kicad_shaped() -> String {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<IPC-2581 revision="C" xmlns="http://webstds.ipc.org/2581">
  <Content roleRef="Owner"><FunctionMode mode="ASSEMBLY"/><StepRef name="BOARD:div"/></Content>
  <HistoryRecord number="1"><FileRevision fileRevisionId="1">
    <SoftwarePackage name="KiCad" revision="9.0.3" vendor="KiCad EDA"/>
  </FileRevision></HistoryRecord>
  <Bom name="BOM:div">
    <BomItem OEMDesignNumberRef="REF:R_10k" quantity="1" pinCount="2">
      <RefDes name="CMP:R1" packageRef="PKG:R_0603" populate="true" layerRef="LAYER:F.Cu"/>
      <Characteristics category="ELECTRICAL">
        <Textual definitionSource="KICAD" textualCharacteristicName="Value" textualCharacteristicValue="10k"/>
      </Characteristics>
    </BomItem>
    <BomItem OEMDesignNumberRef="REF:LED_RED" quantity="1" pinCount="2">
      <RefDes name="CMP:D1" packageRef="PKG:LED_D5.0mm" populate="false" layerRef="LAYER:B.Cu"/>
      <Characteristics category="ELECTRICAL">
        <Textual definitionSource="KICAD" textualCharacteristicName="Value" textualCharacteristicValue="RED"/>
      </Characteristics>
    </BomItem>
  </Bom>
  <Ecad name="Design">
    <CadHeader units="MILLIMETER"/>
    <CadData>
      <Layer name="LAYER:F.Cu" layerFunction="CONDUCTOR" polarity="POSITIVE" side="TOP"/>
      <Layer name="LAYER:B.Cu" layerFunction="CONDUCTOR" polarity="POSITIVE" side="BOTTOM"/>
      <Step name="BOARD:div" type="BOARD">
        <Package name="KI:R_0603_1" type="OTHER">
          <Pin number="PIN:1"><Location x="-0.8" y="0.0"/></Pin>
          <Pin number="PIN:2"><Location x="0.8" y="0.0"/></Pin>
        </Package>
        <Component refDes="CMP:R1" packageRef="KI:R_0603_1" part="REF:R_10k" layerRef="LAYER:F.Cu">
          <Location x="1.0" y="2.0"/><Xform rotation="90.0"/>
        </Component>
        <Component refDes="CMP:D1" packageRef="KI:R_0603_1" part="REF:LED_RED" layerRef="LAYER:B.Cu">
          <Location x="5.0" y="2.0"/>
        </Component>
        <LayerFeature layerRef="LAYER:F.Cu">
          <Set net="NET:VIN">
            <Pad padstackDefRef="P1"><Location x="0.2" y="2.0"/><PinRef componentRef="CMP:R1" pin="PIN:1"/></Pad>
          </Set>
          <Set net="NET:MID">
            <Pad padstackDefRef="P1"><Location x="1.8" y="2.0"/><PinRef componentRef="CMP:R1" pin="PIN:2"/></Pad>
            <Pad padstackDefRef="P1"><Location x="4.2" y="2.0"/><PinRef componentRef="CMP:D1" pin="PIN:A"/></Pad>
          </Set>
          <Set net="NET:GND">
            <Pad padstackDefRef="P1"><Location x="5.8" y="2.0"/><PinRef componentRef="CMP:D1" pin="PIN:K"/></Pad>
          </Set>
        </LayerFeature>
      </Step>
    </CadData>
  </Ecad>
</IPC-2581>
"#
        .to_string()
    }

    #[test]
    fn reads_a_kicad_shaped_revision_c_document() {
        assert!(looks_like_ipc2581(kicad_shaped().as_bytes()));
        let out = extract(&kicad_shaped()).expect("extract");
        assert_eq!(out.stats.revision, "C");
        assert_eq!(out.stats.function_mode, "ASSEMBLY");
        assert_eq!(out.stats.producer, "KiCad 9.0.3");
        assert_eq!(out.stats.net_source, NetSource::LayerFeature);
        assert_eq!(out.board.name, "div", "the BOARD: prefix is stripped");
        assert_eq!(out.board.nets.len(), 3);
        assert_eq!(out.board.components.len(), 2);

        let r1 = out.board.component("R1").expect("R1");
        assert_eq!(r1.value, "10k", "the BOM characteristic is the value");
        assert_eq!(r1.footprint, "R_0603", "the BOM's packageRef wins");
        assert_eq!(r1.layer, "F.Cu");
        assert_eq!(r1.position, Some((1.0, 2.0, 90.0)));
        assert_eq!(r1.pins.len(), 2);
        assert_eq!(r1.pins[0].number, "1", "the PIN: prefix is stripped");
        assert_eq!(r1.pins[0].position, Some((0.2, 2.0)), "pad location kept");

        let d1 = out.board.component("D1").expect("D1");
        assert!(d1.dnp, "populate=\"false\" is DNP");
        assert_eq!(d1.layer, "B.Cu");
        // The package it names declares 1/2 and its netlist pins are A/K: the
        // package is wrong, and the part must not grow to four pads.
        assert_eq!(d1.pins.len(), 2, "no phantom pads from the wrong package");
        let mut names: Vec<&str> = d1.pins.iter().map(|p| p.number.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["A", "K"]);
        assert!(
            out.stats
                .disagreements
                .iter()
                .any(|d| d.contains("package reference is wrong") && d.contains("D1")),
            "the wrong package must be reported: {:?}",
            out.stats.disagreements
        );

        // Connectivity: MID joins R1.2 and D1.A.
        let mid = out.board.net_by_name("MID").expect("MID");
        assert_eq!(out.board.net_members(mid.id).len(), 2);
        assert_eq!(out.stats.connected_pins, 4);
        assert_eq!(out.stats.copper_layers, vec!["B.Cu", "F.Cu"]);
    }

    #[test]
    fn an_unnamespaced_revision_b_document_with_logical_nets_reads_the_same_way() {
        // Bare element names, no prefixes, INCH units, `LogicalNet` as the
        // netlist: the Allegro/converter shape.
        let xml = r#"<IPC-2581 revision="B">
  <Content><FunctionMode mode="DESIGN"/></Content>
  <Ecad><CadHeader units="INCH"/><CadData>
    <Layer name="TOP" layerFunction="CONDUCTOR" side="TOP"/>
    <Step name="board1">
      <Package name="SMR0603"><Pin number="1"/><Pin number="2"/></Package>
      <Component refDes="R1" packageRef="SMR0603" part="RES-10K" layerRef="TOP">
        <Location x="1.0" y="0.0"/>
      </Component>
      <Component refDes="R2" packageRef="SMR0603" part="RES-10K" layerRef="BOTTOM">
        <Location x="2.0" y="0.0"/>
      </Component>
      <LogicalNet name="VCC"><PinRef componentRef="R1" pin="1"/></LogicalNet>
      <LogicalNet name="MID">
        <PinRef componentRef="R1" pin="2"/><PinRef componentRef="R2" pin="1"/>
      </LogicalNet>
      <LogicalNet name="GND"><PinRef componentRef="R2" pin="2"/></LogicalNet>
    </Step>
  </CadData></Ecad>
</IPC-2581>"#;
        let out = extract(xml).expect("extract");
        assert_eq!(out.stats.revision, "B");
        assert_eq!(out.stats.net_source, NetSource::LogicalNet);
        assert_eq!(out.board.nets.len(), 3);
        assert_eq!(out.board.components.len(), 2);
        let r1 = out.board.component("R1").expect("R1");
        assert_eq!(r1.pins.len(), 2);
        assert_eq!(
            r1.position,
            Some((25.4, 0.0, 0.0)),
            "INCH units convert to mm"
        );
        assert_eq!(out.board.component("R2").expect("R2").layer, "B.Cu");
        let mid = out.board.net_by_name("MID").expect("MID");
        assert_eq!(out.board.net_members(mid.id).len(), 2);
        // No value anywhere: honestly unresolved rather than "RES-10K".
        assert_eq!(r1.value, "");
        assert!(r1
            .properties
            .iter()
            .any(|(k, _)| k == VALUE_UNRESOLVED_KEY));
        assert_eq!(r1.lib_id, "RES-10K");
    }

    #[test]
    fn copper_that_contradicts_the_netlist_is_reported_and_the_netlist_wins() {
        let xml = r#"<IPC-2581 revision="C">
  <Ecad><CadData><Step name="s">
    <Package name="P"><Pin number="1"/><Pin number="2"/></Package>
    <Component refDes="R1" packageRef="P" layerRef="TOP"><Location x="0" y="0"/></Component>
    <Component refDes="R2" packageRef="P" layerRef="TOP"><Location x="1" y="0"/></Component>
    <LogicalNet name="A"><PinRef componentRef="R1" pin="1"/><PinRef componentRef="R2" pin="1"/></LogicalNet>
    <LogicalNet name="B"><PinRef componentRef="R1" pin="2"/><PinRef componentRef="R2" pin="2"/></LogicalNet>
    <LayerFeature layerRef="TOP">
      <Set net="B"><Pad><PinRef componentRef="R1" pin="1"/></Pad></Set>
      <Set net="A"><Pad><PinRef componentRef="R9" pin="1"/></Pad></Set>
    </LayerFeature>
  </Step></CadData></Ecad>
</IPC-2581>"#;
        let out = extract(xml).expect("extract");
        let joined = out.stats.disagreements.join(" | ");
        assert!(
            joined.contains("different net in the copper") && joined.contains("R1.1 (A vs B)"),
            "got: {joined}"
        );
        assert!(
            joined.contains("name a pin the netlist does not place"),
            "got: {joined}"
        );
        assert!(
            joined.contains("name a component the document never places") && joined.contains("R9"),
            "got: {joined}"
        );
        // The netlist won: R1.1 is on A.
        let a = out.board.net_by_name("A").expect("A");
        assert!(out
            .board
            .net_members(a.id)
            .iter()
            .any(|(c, p)| c.reference == "R1" && p.number == "1"));
    }

    #[test]
    fn a_bom_only_document_refuses_and_names_what_it_had() {
        let xml = r#"<IPC-2581 revision="C" xmlns="http://webstds.ipc.org/2581">
  <Content roleRef="Owner"><FunctionMode mode="BOM"/><StepRef name="tc10"/></Content>
  <Ecad name="x"><Bom name="BOM_tc10"><BomItem OEMDesignNumberRef="BGA" quantity="1" pinCount="576">
    <RefDes name="U1" packageRef="BGA" populate="true" layerRef="TOP"/>
    <RefDes name="U2" packageRef="BGA" populate="true" layerRef="TOP"/>
  </BomItem></Bom></Ecad>
</IPC-2581>"#;
        let err = extract(xml).expect_err("a BOM-only export must refuse");
        let msg = err.to_string();
        assert!(msg.contains("places no components"), "got: {msg}");
        assert!(msg.contains("FunctionMode BOM"), "got: {msg}");
        assert!(msg.contains("2 reference designator"), "got: {msg}");
        assert!(msg.contains("bom-only"), "names the fix: {msg}");
    }

    #[test]
    fn a_stackup_only_document_refuses_and_names_the_layers() {
        let xml = r#"<IPC-2581 revision="B" xmlns="http://webstds.ipc.org/2581">
  <Content><StepRef name="ipcFile"/></Content>
  <Ecad><CadData>
    <Layer name="TOP" layerFunction="CONDUCTOR" side="TOP"/>
    <Layer name="BOTTOM" layerFunction="CONDUCTOR" side="BOTTOM"/>
    <Step name="ipcFile"><LayerFeature layerRef="TOP"><Set/></LayerFeature></Step>
  </CadData></Ecad>
</IPC-2581>"#;
        let err = extract(xml).expect_err("a stackup-only export must refuse");
        let msg = err.to_string();
        assert!(msg.contains("places no components"), "got: {msg}");
        assert!(msg.contains("2 layer(s)"), "got: {msg}");
        assert!(msg.contains("revision B"), "got: {msg}");
    }

    #[test]
    fn placement_with_no_netlist_refuses_rather_than_returning_a_half_board() {
        let xml = r#"<IPC-2581 revision="C">
  <Content><FunctionMode mode="ASSEMBLY"/></Content>
  <Ecad><CadData><Step name="s">
    <Package name="P"><Pin number="1"/></Package>
    <Component refDes="R1" packageRef="P" layerRef="TOP"><Location x="0" y="0"/></Component>
  </Step></CadData></Ecad>
</IPC-2581>"#;
        let err = extract(xml).expect_err("placement without nets must refuse");
        let msg = err.to_string();
        assert!(msg.contains("declares no nets"), "got: {msg}");
        assert!(msg.contains("no <LogicalNet>"), "got: {msg}");
        assert!(msg.contains("1 component(s)"), "got: {msg}");
    }

    #[test]
    fn the_root_element_gates_the_sniff_and_the_read() {
        assert!(!looks_like_ipc2581(b"<eagle version=\"9.0\"><drawing/></eagle>"));
        assert!(!looks_like_ipc2581(b"(kicad_pcb (version 20240108))"));
        // The namespace alone is enough (a document rewritten without the root
        // in the first 4 KiB still declares it).
        assert!(looks_like_ipc2581(
            b"<!-- comment --><ns:IPC-2581 xmlns:ns=\"http://webstds.ipc.org/2581\">"
        ));
        let err = extract("<eagle/>").expect_err("a non-IPC-2581 root must not read");
        assert!(err.to_string().contains("not a IPC-2581 file"), "got: {err}");
    }

    #[test]
    fn a_non_finite_coordinate_never_reaches_the_ir() {
        let xml = r#"<IPC-2581 revision="C">
  <Ecad><CadHeader units="MILLIMETER"/><CadData><Step name="s">
    <Package name="P"><Pin number="1"/><Pin number="2"/></Package>
    <Component refDes="R1" packageRef="P" layerRef="TOP">
      <Location x="NaN" y="1e400"/><Xform rotation="inf"/>
    </Component>
    <LogicalNet name="A"><PinRef componentRef="R1" pin="1"/></LogicalNet>
    <LogicalNet name="B"><PinRef componentRef="R1" pin="2"/></LogicalNet>
  </Step></CadData></Ecad>
</IPC-2581>"#;
        let out = extract(xml).expect("the document still reads");
        let (x, y, r) = out.board.component("R1").expect("R1").position.expect("pos");
        assert!(
            x.is_finite() && y.is_finite() && r.is_finite(),
            "position {x},{y},{r} is not finite"
        );
    }

    #[test]
    fn zuken_and_altium_element_names_are_read_as_connectivity() {
        // Two producer shapes that carried full netlists the reader used to see
        // nothing in. Kept as unit tests as well as corpus tests so the shapes
        // stay covered without the multi-megabyte files.
        let zuken = r#"<IPC-2581 xmlns="http://webstds.ipc.org/2581">
  <Content><FunctionMode mode="DESIGN" level="1"/></Content>
  <Ecad><CadHeader units="MILLIMETER"/><CadData>
    <Layer name="A-Component" side="TOP" context="BOARD" layerFunction="COMPONENT"/>
    <Layer name="B-Component" side="BOTTOM" context="BOARD" layerFunction="COMPONENT"/>
    <Layer name="Conductive1" side="TOP" context="BOARD" layerFunction="SIGNAL"/>
    <Step name="bd">
      <Package name="bd:CC-CHP-2125"><Pin name="1" type="THRU" number="0.0"/><Pin name="2" type="THRU" number="1.0"/></Package>
      <Component part="ECJ" refDes="bd:C20" layerRef="A-Component" packageRef="bd:CC-CHP-2125"/>
      <Component part="LM218D" refDes="bd:IC11" layerRef="B-Component" packageRef="bd:CC-CHP-2125"/>
      <LogicalNet name="bd:RESET" netClass="SIGNAL">
        <LogicalNetPin pin="1" componentRef="bd:IC11"/>
        <LogicalNetPin pin="1" componentRef="bd:C20"/>
      </LogicalNet>
      <LogicalNet name="bd:GND"><LogicalNetPin pin="2" componentRef="bd:C20"/></LogicalNet>
    </Step>
  </CadData></Ecad>
</IPC-2581>"#;
        let out = extract(zuken).expect("the Zuken shape reads");
        assert_eq!(out.stats.net_source, NetSource::LogicalNet);
        assert_eq!(out.board.nets.len(), 2);
        // The step prefix is gone from both designators and net names.
        let c20 = out.board.component("C20").expect("C20");
        assert_eq!(out.board.net_by_name("RESET").map(|n| n.name.as_str()), Some("RESET"));
        // The pin's `name` is its identity; the `number` ordinal must not become one.
        let mut pins: Vec<&str> = c20.pins.iter().map(|p| p.number.as_str()).collect();
        pins.sort_unstable();
        assert_eq!(pins, vec!["1", "2"]);
        // The `<Layer>` table resolves `A-`/`B-Component` to the right sides.
        assert_eq!(c20.layer, "F.Cu");
        assert_eq!(out.board.component("IC11").expect("IC11").layer, "B.Cu");
        let reset = out.board.net_by_name("RESET").expect("RESET");
        assert_eq!(out.board.net_members(reset.id).len(), 2);
        assert_eq!(out.stats.copper_layers, vec!["Conductive1"]);

        let altium = r#"<IPC-2581 revision="B" xmlns="http://webstds.ipc.org/2581">
  <Content roleRef="Owner"><FunctionMode mode="USERDEF" level="1"/></Content>
  <Ecad><CadData>
    <Layer name="Top Layer" layerFunction="CONDUCTOR" side="TOP"/>
    <Step name="SWITCH BOARD">
      <Package name="0603"><Pin number="1"/><Pin number="2"/></Package>
      <Component refDes="R5" packageRef="0603" layerRef="Top Layer"/>
      <PadStack net="+5V">
        <LayerPad layerRef="Top Layer"><Location x="1.0" y="2.0"/><PinRef componentRef="R5" pin="1"/></LayerPad>
      </PadStack>
      <PadStack net="GND">
        <LayerPad layerRef="Top Layer"><Location x="3.0" y="2.0"/><PinRef componentRef="R5" pin="2"/></LayerPad>
      </PadStack>
    </Step>
  </CadData></Ecad>
</IPC-2581>"#;
        let out = extract(altium).expect("the Altium shape reads");
        assert_eq!(out.stats.net_source, NetSource::LayerFeature);
        assert_eq!(out.board.nets.len(), 2);
        let r5 = out.board.component("R5").expect("R5");
        assert_eq!(r5.pins.len(), 2);
        assert_eq!(r5.pins[0].position, Some((1.0, 2.0)), "the LayerPad location");
        assert_eq!(out.stats.connected_pins, 2);
    }

    #[test]
    fn connectivity_declared_outside_the_step_still_reaches_its_components() {
        // A real document puts `<LogicalNet>` at the root, outside `<Ecad>`; the
        // components are inside a step, so the two were filed under different
        // keys and never met.
        let xml = r#"<IPC-2581 revision="C">
  <LogicalNet name="VIN"><PinRef componentRef="C1" pin="1"/><PinRef componentRef="U1" pin="1"/></LogicalNet>
  <LogicalNet name="GND"><PinRef componentRef="C1" pin="2"/><PinRef componentRef="U1" pin="2"/></LogicalNet>
  <Ecad><CadData><Step name="LED_POWER_BOARD">
    <Package name="C_0805"><Pin number="1"/><Pin number="2"/></Package>
    <Component refDes="C1" packageRef="C_0805" layerRef="TOP_COPPER" value="10uF">
      <Xform x="12.0" y="8.0" rotation="0"/>
    </Component>
    <Component refDes="U1" packageRef="C_0805" layerRef="TOP_COPPER" value="AP2112K-3.3"/>
  </Step></CadData></Ecad>
</IPC-2581>"#;
        let out = extract(xml).expect("reads");
        assert_eq!(out.board.nets.len(), 2);
        assert_eq!(out.stats.connected_pins, 4);
        let c1 = out.board.component("C1").expect("C1");
        // The `value` attribute on `<Component>` is outside the schema but real.
        assert_eq!(c1.value, "10uF");
        // And its placement is on the `<Xform>`, not in a sibling `<Location>`.
        assert_eq!(c1.position, Some((12.0, 8.0, 0.0)));
        assert!(out.stats.bom_absent, "no BOM here, and the note must say so");
        assert!(out
            .stats
            .notes()
            .iter()
            .any(|n| n.contains("carries no BOM")));
    }

    #[test]
    fn a_package_that_only_partly_matches_does_not_fabricate_pads() {
        // Overlap is not enough: a 3-pin package borrowed by a part whose netlist
        // pins are `1` and `K` "fitted" on the strength of `1`, and the part came
        // out with four pads, two of them invented and unconnected.
        let xml = r#"<IPC-2581 revision="C">
  <Ecad><CadData><Step name="s">
    <Package name="SOT23"><Pin number="1"/><Pin number="2"/><Pin number="3"/></Package>
    <Component refDes="D1" packageRef="SOT23" layerRef="TOP"><Location x="0" y="0"/></Component>
    <LogicalNet name="A"><PinRef componentRef="D1" pin="1"/></LogicalNet>
    <LogicalNet name="B"><PinRef componentRef="D1" pin="K"/></LogicalNet>
  </Step></CadData></Ecad>
</IPC-2581>"#;
        let out = extract(xml).expect("reads");
        let d1 = out.board.component("D1").expect("D1");
        let mut pins: Vec<&str> = d1.pins.iter().map(|p| p.number.as_str()).collect();
        pins.sort_unstable();
        assert_eq!(pins, vec!["1", "K"], "no pads 2 and 3 invented");
        assert!(
            out.stats
                .disagreements
                .iter()
                .any(|d| d.contains("package reference is wrong")
                    && d.contains("declares 1, 2, 3")
                    && d.contains("netlist also uses K")),
            "the mismatch must name both sides: {:?}",
            out.stats.disagreements
        );
    }

    #[test]
    fn known_prefixes_are_stripped_and_a_colon_in_a_real_name_is_not() {
        assert_eq!(strip_prefix_tag("CMP:U1"), "U1");
        assert_eq!(strip_prefix_tag("NET:/CPU/CLK"), "/CPU/CLK");
        assert_eq!(strip_prefix_tag("U1"), "U1");
        assert_eq!(
            strip_prefix_tag("BUS:DATA"),
            "BUS:DATA",
            "an unknown tag is part of the name"
        );
        assert_eq!(strip_prefix_tag("PIN:"), "PIN:", "an empty rest is not a prefix");
    }
}
