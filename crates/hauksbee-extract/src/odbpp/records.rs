//! The ODB++ line-record grammar: `matrix`, `eda/data`, the component layers'
//! `components` file, `netlists/*/netlist`, and `layers/*/features`.
//!
//! Every ODB++ data file is line-oriented ASCII. Four conventions run through
//! all of them and are handled once, here:
//!
//! * A line beginning `#` is a comment. Producers use comments as *record
//!   separators* (`# CMP 3`), so a comment must never terminate a record's
//!   continuation lines; only the next record type does.
//! * A trailing `;<key>=<value>,...` on a record carries feature attributes,
//!   keyed by the `@n <name>` table at the top of the same file.
//! * `UNITS=MM` or `UNITS=INCH` at the top of the file sets the length unit for
//!   every coordinate in it. Different files in one job may legitimately
//!   disagree, so units are read PER FILE, with `misc/info`'s `UNITS` as the
//!   job-wide default for a file that declares none (which the spec allows and
//!   real producers use).
//! * The block-structured files (`matrix`, `stephdr`, `tools`) use
//!   `TYPE {\n KEY=VALUE\n }` blocks.

/// A `NAME { KEY=VALUE ... }` block from `matrix`, `tools` or `stephdr`.
#[derive(Debug, Clone)]
pub(crate) struct Block {
    pub(crate) kind: String,
    pub(crate) fields: Vec<(String, String)>,
}

impl Block {
    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }
}

/// Parse the `NAME { ... }` blocks of a block-structured ODB++ file.
pub(crate) fn parse_blocks(text: &str) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut current: Option<Block> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_suffix('{') {
            current = Some(Block {
                kind: name.trim().to_ascii_uppercase(),
                fields: Vec::new(),
            });
            continue;
        }
        if line == "}" {
            if let Some(b) = current.take() {
                out.push(b);
            }
            continue;
        }
        if let (Some(block), Some((k, v))) = (current.as_mut(), line.split_once('=')) {
            block
                .fields
                .push((k.trim().to_ascii_uppercase(), v.trim().to_string()));
        }
    }
    out
}

/// One layer row of `matrix/matrix`.
#[derive(Debug, Clone)]
pub(crate) struct MatrixLayer {
    pub(crate) name: String,
    /// `SIGNAL`, `POWER_GROUND`, `MIXED`, `DRILL`, `COMPONENT`, `SOLDER_MASK`…
    pub(crate) layer_type: String,
    /// `BOARD` or `MISC`.
    pub(crate) context: String,
}

impl MatrixLayer {
    /// A layer that carries etched copper *on the board*. `MIXED` and
    /// `POWER_GROUND` are copper as much as `SIGNAL` is; counting only `SIGNAL`
    /// loses a plane layer's geometry on every four-layer board. The `MISC`
    /// context is excluded: a documentation or coupon layer can carry a copper
    /// TYPE without being part of the stackup.
    pub(crate) fn is_copper(&self) -> bool {
        matches!(
            self.layer_type.as_str(),
            "SIGNAL" | "POWER_GROUND" | "MIXED"
        ) && self.context != "MISC"
    }

    pub(crate) fn is_drill(&self) -> bool {
        self.layer_type == "DRILL"
    }

    pub(crate) fn is_component(&self) -> bool {
        self.layer_type == "COMPONENT"
    }
}

/// The matrix: the job's step names and layer rows.
#[derive(Debug, Default)]
pub(crate) struct Matrix {
    pub(crate) steps: Vec<String>,
    pub(crate) layers: Vec<MatrixLayer>,
}

pub(crate) fn parse_matrix(text: &str) -> Matrix {
    let mut m = Matrix::default();
    for block in parse_blocks(text) {
        match block.kind.as_str() {
            "STEP" => {
                if let Some(name) = block.get("NAME") {
                    m.steps.push(name.to_ascii_lowercase());
                }
            }
            "LAYER" => {
                let Some(name) = block.get("NAME") else {
                    continue;
                };
                m.layers.push(MatrixLayer {
                    name: name.to_ascii_lowercase(),
                    layer_type: block.get("TYPE").unwrap_or("").to_ascii_uppercase(),
                    context: block.get("CONTEXT").unwrap_or("").to_ascii_uppercase(),
                });
            }
            _ => {}
        }
    }
    m
}

// ── eda/data and the component layers ─────────────────────────────────────────

/// A package (footprint) definition from `eda/data`. ODB++ references packages
/// by their *ordinal* in the file, and the same name legitimately appears more
/// than once with different outlines, so the index is the identity.
#[derive(Debug, Clone, Default)]
pub(crate) struct Package {
    pub(crate) name: String,
    /// Pin names in declaration order. A component's toeprint index is an index
    /// into this list, which is what lets a toeprint's own name be cross-checked
    /// against the package it claims to be an instance of.
    pub(crate) pins: Vec<String>,
}

/// What a `TOP` record's net field said.
///
/// A plain `usize` collapsed three different situations into one — "on no net",
/// "on net 77 which this job never declares", and "the field was not a number" —
/// and the last two are faults a reader must report rather than quietly turn
/// into a floating pad.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NetRef {
    /// Ordinal 0: ODB++'s `$NONE$`, a pad deliberately on no net.
    None,
    /// A net ordinal into `eda/data`'s `NET` sequence. Not yet known to be in
    /// range: the table is not available at parse time.
    Num(usize),
    /// The field was not a number. Kept verbatim so the report can quote it.
    Unparseable(String),
}

/// One pad of a placed component ("toeprint" in ODB++ terms).
#[derive(Debug, Clone)]
pub(crate) struct Toeprint {
    /// Index into the owning package's pin list.
    pub(crate) pin_index: usize,
    pub(crate) x_mm: f64,
    pub(crate) y_mm: f64,
    /// The net this pad claims, as the record stated it.
    pub(crate) net: NetRef,
    /// The pad name as the placement records it.
    pub(crate) name: String,
}

/// One placed component, before reference-designator normalization.
#[derive(Debug, Clone)]
pub(crate) struct RawComponent {
    pub(crate) pkg_index: Option<usize>,
    pub(crate) x_mm: f64,
    pub(crate) y_mm: f64,
    pub(crate) rotation: f64,
    /// ODB++ `M` mirror flag: the component is placed on the far side.
    pub(crate) mirrored: bool,
    pub(crate) refdes: String,
    /// The `part_name` field: a manufacturer part number from most tools, a
    /// `<library>_<footprint>` string from KiCad.
    pub(crate) part: String,
    /// `PRP` properties, verbatim.
    pub(crate) props: Vec<(String, String)>,
    pub(crate) toeprints: Vec<Toeprint>,
    /// Which component layer the record came from, when it came from one
    /// (`comp_+_top` → false, `comp_+_bot` → true). `None` for `eda/data`,
    /// whose CMP records carry only the mirror flag.
    pub(crate) layer_is_bottom: Option<bool>,
    /// The `.no_pop` feature attribute: this part is not assembled.
    pub(crate) no_pop: bool,
}

/// What one `eda/data` or `components` file yielded.
#[derive(Debug, Default)]
pub(crate) struct EdaData {
    /// Net names indexed by ODB++ net ordinal. Index 0 is `$NONE$`.
    pub(crate) nets: Vec<String>,
    pub(crate) packages: Vec<Package>,
    pub(crate) components: Vec<RawComponent>,
    /// The `LYR` line: which layers the net/feature-id references point into.
    pub(crate) layers: Vec<String>,
}

/// A coordinate: a finite number, or `None`.
///
/// `parse::<f64>()` accepts `inf`, `NaN` and an overflowing exponent (`1e400`),
/// and the crate refuses a non-finite coordinate rather than let it into the IR
/// (see `pcb::reject_non_finite_geometry` and `eagle::non_finite_coord` for the
/// same rule on the native formats): a distance compared against NaN is silently
/// false, which turns a clearance check into a meaningless pass. ODB++ producers
/// cannot write such a file, so nothing real is lost.
fn coord(raw: &str) -> Option<f64> {
    raw.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// The multiplier from a `UNITS=` value to millimetres.
pub(crate) fn unit_scale(value: &str) -> f64 {
    if value.trim().eq_ignore_ascii_case("INCH") {
        25.4
    } else {
        1.0
    }
}

/// This file's own `UNITS=` line, if it declares one.
///
/// `None` matters: ODB++ makes `misc/info`'s `UNITS` the JOB default, and a
/// producer that declares `UNITS=INCH` there and omits it per file is entitled
/// to. Defaulting to millimetres here instead of asking the caller for the job
/// default made every coordinate in such a job 25.4× too small, silently.
pub(crate) fn declared_units(text: &str) -> Option<f64> {
    text.lines()
        .take(40)
        .filter_map(|l| l.trim().strip_prefix("UNITS="))
        .map(unit_scale)
        .next()
}

/// Split a record line into its body and its feature-attribute tail.
///
/// The tail is `;<attributes>;<system attributes>` — Valor NPI writes
/// `CMP ... ;0=0.001000,1=2;ID=61442` — so the attributes stop at the SECOND
/// semicolon. Taking everything after the first would fold `ID=61442` into the
/// attribute list and make its index lookups garbage.
fn split_attrs(line: &str) -> (&str, Option<&str>) {
    match line.split_once(';') {
        Some((body, tail)) => {
            let attrs = tail.split(';').next().unwrap_or("").trim();
            (body.trim_end(), (!attrs.is_empty()).then_some(attrs))
        }
        None => (line, None),
    }
}

/// The `@n <name>` attribute-name table at the top of a features/components
/// file, so a `;0=1` tail can be resolved to `.comp_mount_type=1`.
fn attr_names(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix('@') {
            if let Some((idx, name)) = rest.split_once(' ') {
                if let Ok(i) = idx.trim().parse::<usize>() {
                    if out.len() <= i {
                        out.resize(i + 1, String::new());
                    }
                    out[i] = name.trim().to_string();
                }
            }
        }
    }
    out
}

/// True when the `;attr` tail sets `.no_pop`. `.no_pop` is a boolean attribute,
/// so its mere presence (`;3` or `;3=1`) is the assertion.
fn attrs_say_no_pop(attrs: Option<&str>, names: &[String]) -> bool {
    let Some(attrs) = attrs else { return false };
    attrs.split(',').any(|tok| {
        let key = tok.split('=').next().unwrap_or("").trim();
        key.parse::<usize>()
            .ok()
            .and_then(|i| names.get(i))
            .is_some_and(|n| n == ".no_pop")
    })
}

/// Parse `eda/data`: net names, package definitions and (when the producer puts
/// them there) the placed components.
/// `job_units` is the job default from `misc/info`, used when the file itself
/// declares no `UNITS=` line.
pub(crate) fn parse_eda_data(text: &str, job_units: f64) -> EdaData {
    let scale = declared_units(text).unwrap_or(job_units);
    let names = attr_names(text);
    let mut out = EdaData::default();
    // Which multi-line record we are inside. A `#` comment does not end one.
    enum In {
        None,
        Pkg,
        Cmp,
    }
    let mut state = In::None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (body, attrs) = split_attrs(line);
        let mut tok = body.split_whitespace();
        let Some(kind) = tok.next() else { continue };
        match kind {
            "LYR" => out.layers = tok.map(|s| s.to_ascii_lowercase()).collect(),
            "NET" => {
                state = In::None;
                out.nets.push(tok.next().unwrap_or("").to_string());
            }
            "PKG" => {
                state = In::Pkg;
                out.packages.push(Package {
                    name: tok.next().unwrap_or("").to_string(),
                    pins: Vec::new(),
                });
            }
            "PIN" => {
                if matches!(state, In::Pkg) {
                    if let Some(pkg) = out.packages.last_mut() {
                        pkg.pins.push(tok.next().unwrap_or("").to_string());
                    }
                }
            }
            "CMP" => {
                state = In::Cmp;
                if let Some(c) = parse_cmp(body, attrs, &names, scale, None) {
                    out.components.push(c);
                }
            }
            "PRP" => {
                if matches!(state, In::Cmp) {
                    if let (Some(c), Some(p)) = (out.components.last_mut(), parse_prp(body)) {
                        c.props.push(p);
                    }
                }
            }
            "TOP" => {
                if matches!(state, In::Cmp) {
                    if let (Some(c), Some(t)) = (out.components.last_mut(), parse_top(body, scale))
                    {
                        c.toeprints.push(t);
                    }
                }
            }
            // `SNT`/`FID` are the net → feature-id back-references, and `FTR`/
            // `TXT` the feature records. Deliberately dropped: hauksbee's IR
            // holds connectivity, not per-feature provenance.
            _ => {}
        }
    }
    out
}

/// Parse a component-layer `components` file (`steps/<step>/layers/comp_+_top/
/// components`). Same CMP/PRP/TOP grammar as `eda/data`, and the layer fixes
/// the board side.
pub(crate) fn parse_components_file(
    text: &str,
    is_bottom: bool,
    job_units: f64,
) -> Vec<RawComponent> {
    let scale = declared_units(text).unwrap_or(job_units);
    let names = attr_names(text);
    let mut out: Vec<RawComponent> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (body, attrs) = split_attrs(line);
        match body.split_whitespace().next() {
            Some("CMP") => {
                if let Some(c) = parse_cmp(body, attrs, &names, scale, Some(is_bottom)) {
                    out.push(c);
                }
            }
            Some("PRP") => {
                if let (Some(c), Some(p)) = (out.last_mut(), parse_prp(body)) {
                    c.props.push(p);
                }
            }
            Some("TOP") => {
                if let (Some(c), Some(t)) = (out.last_mut(), parse_top(body, scale)) {
                    c.toeprints.push(t);
                }
            }
            _ => {}
        }
    }
    out
}

/// `CMP <pkg_ref> <x> <y> <rot> <mirror> <comp_name> <part_name>`.
///
/// `part_name` is the last field and real exports put spaces in it
/// ("47uH 0.5A"), so it takes the whole remainder rather than one token.
fn parse_cmp(
    body: &str,
    attrs: Option<&str>,
    attr_names: &[String],
    scale: f64,
    layer_is_bottom: Option<bool>,
) -> Option<RawComponent> {
    let mut it = body.split_whitespace();
    it.next()?; // CMP
    let pkg_index = it.next()?.parse::<usize>().ok();
    let x = coord(it.next()?)? * scale;
    let y = coord(it.next()?)? * scale;
    let rotation = it.next()?.parse::<f64>().unwrap_or(0.0);
    let mirrored = it.next()?.eq_ignore_ascii_case("M");
    let refdes = it.next()?.to_string();
    let part = it.collect::<Vec<_>>().join(" ");
    Some(RawComponent {
        pkg_index,
        x_mm: x,
        y_mm: y,
        rotation,
        mirrored,
        refdes,
        part,
        props: Vec::new(),
        toeprints: Vec::new(),
        layer_is_bottom,
        no_pop: attrs_say_no_pop(attrs, attr_names),
    })
}

/// `PRP <name> '<value>'`. The value is single-quoted and may hold spaces.
fn parse_prp(body: &str) -> Option<(String, String)> {
    let rest = body.strip_prefix("PRP")?.trim_start();
    let (name, tail) = rest.split_once(char::is_whitespace)?;
    let tail = tail.trim();
    let value = match (tail.find('\''), tail.rfind('\'')) {
        (Some(a), Some(b)) if b > a => tail[a + 1..b].to_string(),
        _ => tail.to_string(),
    };
    Some((name.to_string(), value))
}

/// `TOP <pin_index> <x> <y> <rot> <mirror> <net> <subnet> <name>`.
fn parse_top(body: &str, scale: f64) -> Option<Toeprint> {
    let t: Vec<&str> = body.split_whitespace().collect();
    if t.len() < 8 {
        return None;
    }
    Some(Toeprint {
        pin_index: t[1].parse::<usize>().ok()?,
        x_mm: coord(t[2])? * scale,
        y_mm: coord(t[3])? * scale,
        net: match t[6].parse::<usize>() {
            Ok(0) => NetRef::None,
            Ok(n) => NetRef::Num(n),
            Err(_) => NetRef::Unparseable(t[6].to_string()),
        },
        // The toeprint name is optional in the grammar; when it is absent the
        // package pin name is the only name there is, and the caller falls back
        // to it rather than inventing "".
        name: t.get(8).copied().unwrap_or("").to_string(),
    })
}

// ── netlists/<name>/netlist ───────────────────────────────────────────────────

/// A `netlists/<name>/netlist` file: the CAD netlist ODB++ ships alongside the
/// EDA data, as `name → number of net points`.
#[derive(Debug, Default)]
pub(crate) struct CadNetlist {
    /// Net name → how many netlist points reference it.
    pub(crate) points_per_net: std::collections::BTreeMap<String, usize>,
}

pub(crate) fn parse_netlist(text: &str) -> CadNetlist {
    let mut names: std::collections::BTreeMap<usize, String> = Default::default();
    let mut counts: std::collections::BTreeMap<usize, usize> = Default::default();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('H') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('$') {
            // `$<net_number> <net_name>`
            if let Some((num, name)) = rest.split_once(char::is_whitespace) {
                if let Ok(n) = num.trim().parse::<usize>() {
                    names.insert(n, name.trim().to_string());
                }
            }
            continue;
        }
        // A net point: `<net_number> <via_type> <x> <y> ...`
        if let Some(first) = line.split_whitespace().next() {
            if let Ok(n) = first.parse::<usize>() {
                *counts.entry(n).or_default() += 1;
            }
        }
    }
    let mut out = CadNetlist::default();
    for (num, name) in names {
        out.points_per_net
            .insert(name, counts.get(&num).copied().unwrap_or(0));
    }
    out
}

// ── layers/<layer>/features ───────────────────────────────────────────────────

/// What one layer's `features` file holds, counted by primitive kind. ODB++
/// carries far more per-feature detail than hauksbee's IR has anywhere to put
/// (see the module docs on what is deliberately dropped), so the geometry is
/// summarized rather than materialized.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct FeatureCounts {
    pub(crate) lines: usize,
    pub(crate) pads: usize,
    pub(crate) arcs: usize,
    pub(crate) surfaces: usize,
    /// Text and barcode features: counted so the `F` header adds up, but not
    /// broken out, because the IR has nowhere to put either.
    pub(crate) other: usize,
    /// Record types this reader does not recognise at all. Their presence means a
    /// total that falls short of the `F` header is this reader's gap rather than
    /// the file's error, so the mismatch is not reported as one — a disagreement
    /// list that cries wolf is a disagreement list nobody reads.
    pub(crate) unknown_records: usize,
    /// The `F <n>` header's claimed feature count, when present.
    pub(crate) declared: Option<usize>,
}

impl FeatureCounts {
    pub(crate) fn total(&self) -> usize {
        self.lines + self.pads + self.arcs + self.surfaces + self.other
    }
}

pub(crate) fn parse_features(text: &str) -> FeatureCounts {
    let mut c = FeatureCounts::default();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('#') {
            // The header comment block; `F <n>` sits outside it.
            let _ = rest;
            continue;
        }
        let mut it = line.split_whitespace();
        match it.next() {
            Some("F") => c.declared = it.next().and_then(|v| v.parse::<usize>().ok()),
            Some("L") => c.lines += 1,
            Some("P") => c.pads += 1,
            Some("A") => c.arcs += 1,
            // A surface is `S ... OB/OS/OE ... SE`; count the opening record.
            Some("S") => c.surfaces += 1,
            // `T` (text) and `B` (barcode) are features too, and counted, but
            // not broken out: the IR has nowhere to put either.
            Some("T") | Some("B") => c.other += 1,
            // A surface body, a symbol/attribute-table line, or a record type
            // this reader does not model. Tracked so the `F` header check can
            // tell "the header is wrong" from "there is a record kind here I do
            // not count", and cry wolf only in the first case.
            Some(tok) if !SURFACE_BODY.contains(&tok) => {
                if tok.len() <= 3 && tok.chars().all(|ch| ch.is_ascii_uppercase()) {
                    c.unknown_records += 1;
                }
            }
            _ => {}
        }
    }
    c
}

/// The continuation records inside a surface (`S`) or an outline: not features.
const SURFACE_BODY: &[&str] = &["OB", "OS", "OC", "OE", "SE", "CT", "CE"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_rows_classify_copper_drill_and_component_layers() {
        let m = parse_matrix(
            "STEP {\n    COL=1\n    NAME=PCB\n}\n\n\
             LAYER {\n    ROW=1\n    CONTEXT=BOARD\n    TYPE=COMPONENT\n    NAME=COMP_+_TOP\n}\n\
             LAYER {\n    ROW=2\n    CONTEXT=BOARD\n    TYPE=SIGNAL\n    NAME=F.CU\n}\n\
             LAYER {\n    ROW=3\n    CONTEXT=BOARD\n    TYPE=POWER_GROUND\n    NAME=IN1.CU\n}\n\
             LAYER {\n    ROW=4\n    CONTEXT=BOARD\n    TYPE=DRILL\n    NAME=DRILL_1\n}\n\
             LAYER {\n    ROW=5\n    CONTEXT=MISC\n    TYPE=DOCUMENT\n    NAME=EDGE.CUTS\n}\n",
        );
        assert_eq!(m.steps, vec!["pcb".to_string()]);
        let copper: Vec<&str> = m
            .layers
            .iter()
            .filter(|l| l.is_copper())
            .map(|l| l.name.as_str())
            .collect();
        assert_eq!(
            copper,
            vec!["f.cu", "in1.cu"],
            "a POWER_GROUND plane is copper too"
        );
        assert_eq!(m.layers.iter().filter(|l| l.is_drill()).count(), 1);
        assert_eq!(m.layers.iter().filter(|l| l.is_component()).count(), 1);
        assert_eq!(m.layers[4].context, "MISC");
    }

    #[test]
    fn cmp_part_name_keeps_its_spaces() {
        let c = parse_cmp(
            "CMP 1 75.19 -106.35 90.0 N L2 Inductor_SMD_L_NR-40xx 47uH 0.5A",
            None,
            &[],
            1.0,
            Some(false),
        )
        .expect("CMP parses");
        assert_eq!(c.refdes, "L2");
        assert_eq!(
            c.part, "Inductor_SMD_L_NR-40xx 47uH 0.5A",
            "part_name is the remainder, not one token"
        );
        assert_eq!(c.rotation, 90.0);
        assert!(!c.mirrored);
    }

    #[test]
    fn attributes_stop_at_the_system_attribute_semicolon() {
        // Valor NPI: `;<attrs>;<sysattrs>`. Folding `ID=` into the attribute
        // list makes every index lookup wrong.
        let (body, attrs) = split_attrs("CMP 0 -3.495 0.44 0.0 N MTG1 MTG188NP ;1=2;ID=61442");
        assert_eq!(body, "CMP 0 -3.495 0.44 0.0 N MTG1 MTG188NP");
        assert_eq!(attrs, Some("1=2"));
        assert!(attrs_say_no_pop(attrs, &[String::new(), ".no_pop".into()]));
        // `PKG ... ;;ID=` has an empty attribute list.
        let (body, attrs) = split_attrs("PKG MH_188NP 0 -0.13 -0.13 0.13 0.13;;ID=61431");
        assert_eq!(body, "PKG MH_188NP 0 -0.13 -0.13 0.13 0.13");
        assert_eq!(attrs, None);
    }

    #[test]
    fn inch_units_are_converted_to_mm() {
        let eda = parse_eda_data(
            "UNITS=INCH\nNET $NONE$\nNET GND\nPKG P 1 0 0 1 1;\nPIN 1 T 0 0 0 E T\n\
             CMP 0 1.0 2.0 0 N R1 R_0603\nTOP 0 1.0 2.0 0 N 1 0 1\n",
            1.0,
        );
        let c = &eda.components[0];
        assert!((c.x_mm - 25.4).abs() < 1e-9, "1 inch is 25.4 mm");
        assert!((c.toeprints[0].y_mm - 50.8).abs() < 1e-9);
    }

    #[test]
    fn a_comment_between_records_does_not_end_the_component() {
        // Producers separate records with `# CMP 1` comment lines; treating a
        // comment as a terminator drops every PRP/TOP after the first blank.
        let comps = parse_components_file(
            "UNITS=MM\n@0 .no_pop\n# CMP 0\nCMP 0 1.0 2.0 0 N R1 Res ;0\n\
             PRP Value '10k'\n#\nTOP 0 1.0 2.0 0 N 3 0 1\nTOP 1 3.0 2.0 0 N 4 0 2\n",
            false,
            1.0,
        );
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].toeprints.len(), 2);
        assert_eq!(comps[0].props, vec![("Value".into(), "10k".into())]);
        assert!(comps[0].no_pop, ".no_pop attribute is honoured");
        assert_eq!(comps[0].toeprints[1].net, NetRef::Num(4));
    }

    #[test]
    fn a_net_field_that_is_not_a_number_is_kept_rather_than_read_as_no_net() {
        // `unwrap_or(0)` turned a malformed net field into ODB++'s "no net",
        // which reports a connected pad as floating with nothing said.
        let comps = parse_components_file(
            "UNITS=MM\nCMP 0 0 0 0 N R1 R ;\nTOP 0 0 0 0.0 N GND 0 1\n\
             TOP 1 1 0 0.0 N 0 0 2\nTOP 2 2 0 0.0 N 7 0 3\n",
            false,
            1.0,
        );
        let t = &comps[0].toeprints;
        assert_eq!(t[0].net, NetRef::Unparseable("GND".to_string()));
        assert_eq!(t[1].net, NetRef::None, "ordinal 0 is $NONE$");
        assert_eq!(t[2].net, NetRef::Num(7));
    }

    #[test]
    fn a_file_with_no_units_line_takes_the_job_default() {
        // ODB++ makes `misc/info`'s UNITS the job default. Assuming millimetres
        // for a file that declares nothing made an INCH job 25.4x too small.
        assert_eq!(declared_units("UNITS=INCH\nF 0\n"), Some(25.4));
        assert_eq!(declared_units("UNITS=MM\nF 0\n"), Some(1.0));
        assert_eq!(declared_units("ID=61435\nF 0\n"), None);
        let comps = parse_components_file("CMP 0 1.0 0 0 N R1 R ;\n", false, 25.4);
        assert!((comps[0].x_mm - 25.4).abs() < 1e-9, "job default applied");
    }

    #[test]
    fn netlist_points_are_counted_by_net_name() {
        let n = parse_netlist(
            "H optimize n staggered n\n$1 +5V\n$2 GND\n#\n#Netlist points\n#\n\
             1 0.0 -7.20 6.60 B e c staggered 0 0 0\n1 0 -7.06 -2.91 T 1.0 1.0 e c\n\
             2 0.0 7.69 0.39 B e c staggered 0 0 0\n",
        );
        assert_eq!(n.points_per_net.get("+5V"), Some(&2));
        assert_eq!(n.points_per_net.get("GND"), Some(&1));
    }

    #[test]
    fn feature_kinds_are_counted_and_the_declared_total_kept() {
        let c = parse_features(
            "UNITS=MM\n#\n#Num Features\n#\nF 4\n#\n#Layer features\n#\n\
             L 0 0 1 1 0 P 0 \nP 0 0 1 P 0 8 0.0 ;0=0\nA 0 0 1 1 2 2 0 P 0 Y\n\
             S P 0 \nOB 0 0 I\nOE\nSE\n",
        );
        assert_eq!(c.declared, Some(4));
        assert_eq!(
            (c.lines, c.pads, c.arcs, c.surfaces),
            (1, 1, 1, 1),
            "the surface's OB/OE/SE body must not be counted as features"
        );
        assert_eq!(c.total(), 4);
        assert_eq!(c.unknown_records, 0, "every record here is recognised");
    }

    #[test]
    fn text_features_count_and_an_unrecognised_record_suppresses_the_header_check() {
        // A copper layer carrying a `T` (text) feature adds up against its `F`
        // header only if text is counted; and a record type this reader does not
        // model must mark the count as incomplete rather than let the caller
        // report the header as wrong.
        let c = parse_features("UNITS=MM\nF 2\n#\nP 0 0 1 P 0 8 0.0 \nT 0 0 0 1 1 0 P 0 hi\n");
        assert_eq!(c.total(), 2, "text is a feature");
        assert_eq!(c.unknown_records, 0);
        assert_eq!(c.declared, Some(2));

        let c = parse_features("UNITS=MM\nF 3\n#\nP 0 0 1 P 0 8 0.0 \nXYZ 1 2 3\n");
        assert_eq!(c.total(), 1);
        assert!(
            c.unknown_records > 0,
            "an unmodelled record must be tracked so the F check stays quiet"
        );
    }
}
