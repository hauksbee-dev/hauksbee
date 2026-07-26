//! Installed-footprint-library discovery and copy-through ("dressing").
//!
//! A hauksbee-emitted board names every footprint by its full library id
//! (`Package_DIP:DIP-28_W7.62mm`), but the minimal emitter writes only what the
//! DSL carries: name, layer, position, pads, nets. KiCad's library-parity DRC
//! (`lib_footprint_mismatch`) then flags every footprint on every board, and
//! `kicad-cli pcb render` shows bare pads: no courtyard, no fabrication or
//! silkscreen graphics, no reference designator text, no 3D model.
//!
//! The fix: when the installed library footprint can be FOUND, copy through
//! everything the minimal emitter drops. The library `.kicad_mod` is parsed and
//! its CST is used as the emitted footprint node, with the board data (position,
//! rotation, reference, value, pad nets) patched in. That is strictly better
//! than synthesising the missing pieces, because the library file is the very
//! thing the DRC compares against, and its `(model ...)` offset is authored
//! against the same footprint origin (so no origin guessing for rotated DIPs).
//!
//! ## Discovery
//!
//! `HAUKSBEE_FOOTPRINT_DIR` overrides everything: a path list (`:`-separated on
//! Unix) of directories containing `<Lib>.pretty/` folders; an empty value or
//! `none`/`off` disables copy-through entirely. Without the override, discovery
//! reads the KiCad global `fp-lib-table` (macOS
//! `~/Library/Preferences/kicad/<ver>/`, Linux `~/.config/kicad/<ver>/`,
//! newest version first), honours `KICAD*_FOOTPRINT_DIR` environment variables,
//! and falls back to the stock install locations (macOS
//! `/Applications/KiCad/KiCad.app/Contents/SharedSupport/footprints`, Linux
//! `/usr/share/kicad/footprints` and `/usr/local/share/kicad/footprints`).
//!
//! ## Graceful degradation
//!
//! Every failure (no KiCad installed, unknown library nickname, missing
//! footprint file, or a library whose pads do not match the board's) falls back
//! to the minimal emission that exists today, per footprint. A fresh clone with
//! no KiCad produces byte-for-byte what it produced before this module existed.
//!
//! ## Rotation
//!
//! The kicad_pcb coordinate contract (documented on `forge_model::Pad`
//! `absolute_pos`): child-item X/Y offsets are footprint-local and KiCad rotates
//! them at load time, while `at` ANGLE fields on pads and text are stored as the
//! absolute angle (footprint rotation already added). So graphics coordinates
//! are copied through untouched, and the footprint rotation is ADDED to the
//! angle field of every pad, property, and fp_text: exactly the transform
//! KiCad's own writer applies, and the same frame the pad emitter already uses
//! (local offsets passed through verbatim).

use std::collections::HashMap;
use std::path::PathBuf;

use forge_model::fmt_f64;
use forge_sexpr::{quote, List, Sexpr, Token};

use crate::dsl::Comp;

/// Positional tolerance (mm) when matching a DSL pad to a library pad. A board
/// decompiled from a real layout carries the library offsets verbatim (six
/// decimal places max), so genuine matches are exact; anything beyond this is
/// an intentional edit and the library must not clobber it.
const PAD_POS_TOL: f64 = 0.01;

/// Size tolerance (mm) for the same guard.
const PAD_SIZE_TOL: f64 = 0.01;

/// A resolver for installed KiCad footprint libraries.
///
/// Construct with [`FootprintLib::discover`] (environment + system discovery),
/// [`FootprintLib::with_roots`] (explicit directories; used by tests), or
/// [`FootprintLib::disabled`] (never resolves; minimal emission everywhere).
pub struct FootprintLib {
    /// Directories that contain `<Lib>.pretty/` folders.
    roots: Vec<PathBuf>,
    /// Library nickname -> `.pretty` directory, from the global fp-lib-table.
    nicknames: HashMap<String, PathBuf>,
    /// Resolution cache: full lib id -> parsed footprint node (None = not found,
    /// cached so a 137-part board stats each missing library once, not 137x).
    cache: HashMap<String, Option<List>>,
}

impl FootprintLib {
    /// A resolver that never finds anything (pure minimal emission).
    pub fn disabled() -> FootprintLib {
        FootprintLib {
            roots: Vec::new(),
            nicknames: HashMap::new(),
            cache: HashMap::new(),
        }
    }

    /// Explicit search roots (directories containing `<Lib>.pretty/`).
    pub fn with_roots(roots: Vec<PathBuf>) -> FootprintLib {
        FootprintLib {
            roots,
            nicknames: HashMap::new(),
            cache: HashMap::new(),
        }
    }

    /// Environment + system discovery.
    ///
    /// `HAUKSBEE_FOOTPRINT_DIR` (path list) overrides everything; empty /
    /// `none` / `off` disables. Otherwise: global fp-lib-table nicknames,
    /// `KICAD*_FOOTPRINT_DIR` environment variables, stock install locations.
    pub fn discover() -> FootprintLib {
        match std::env::var("HAUKSBEE_FOOTPRINT_DIR") {
            Ok(v) => {
                let v = v.trim().to_string();
                if v.is_empty() || v == "none" || v == "off" {
                    return FootprintLib::disabled();
                }
                FootprintLib::with_roots(std::env::split_paths(&v).collect())
            }
            Err(_) => FootprintLib::system(),
        }
    }

    /// System discovery: fp-lib-table + env vars + stock locations.
    fn system() -> FootprintLib {
        let mut roots: Vec<PathBuf> = Vec::new();

        // KICAD9_FOOTPRINT_DIR / KICAD_FOOTPRINT_DIR / etc, if they point at
        // real directories.
        for (key, val) in std::env::vars() {
            if is_kicad_footprint_dir_var(&key) {
                let p = PathBuf::from(&val);
                if p.is_dir() {
                    roots.push(p);
                }
            }
        }

        // Stock install locations (macOS app bundle, Linux distro paths).
        for cand in [
            "/Applications/KiCad/KiCad.app/Contents/SharedSupport/footprints",
            "/Applications/KiCad.app/Contents/SharedSupport/footprints",
            "/usr/share/kicad/footprints",
            "/usr/local/share/kicad/footprints",
        ] {
            let p = PathBuf::from(cand);
            if p.is_dir() && !roots.contains(&p) {
                roots.push(p);
            }
        }

        // Global fp-lib-table: nickname -> .pretty dir, with ${VAR} substitution.
        // Environment wins; an unset KICAD*_FOOTPRINT_DIR falls back to the
        // first stock root (that is exactly what KiCad's internal default is).
        let fallback = roots.first().cloned();
        let lookup = |var: &str| -> Option<String> {
            if let Ok(v) = std::env::var(var) {
                return Some(v);
            }
            if is_kicad_footprint_dir_var(var) {
                return fallback.as_ref().map(|p| p.display().to_string());
            }
            None
        };
        let mut nicknames = HashMap::new();
        for table in global_fp_lib_tables() {
            if let Ok(text) = std::fs::read_to_string(&table) {
                for (nick, dir) in parse_fp_lib_table(&text, &lookup) {
                    // Newest KiCad version first; first entry wins.
                    nicknames.entry(nick).or_insert(dir);
                }
            }
        }

        FootprintLib {
            roots,
            nicknames,
            cache: HashMap::new(),
        }
    }

    /// Resolve a full library id (`"Package_DIP:DIP-28_W7.62mm"`) to the parsed
    /// `(footprint ...)` node of the installed `.kicad_mod`, or None.
    pub fn resolve(&mut self, lib_id: &str) -> Option<&List> {
        if !self.cache.contains_key(lib_id) {
            let loaded = self.load(lib_id);
            self.cache.insert(lib_id.to_string(), loaded);
        }
        self.cache.get(lib_id).and_then(|o| o.as_ref())
    }

    fn load(&self, lib_id: &str) -> Option<List> {
        let (nick, name) = lib_id.split_once(':')?;
        if nick.is_empty() || name.is_empty() {
            return None;
        }
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(dir) = self.nicknames.get(nick) {
            candidates.push(dir.clone());
        }
        for root in &self.roots {
            candidates.push(root.join(format!("{nick}.pretty")));
        }
        for dir in candidates {
            let path = dir.join(format!("{name}.kicad_mod"));
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(doc) = forge_sexpr::parse(&text) else {
                continue;
            };
            let Some(root) = doc.root() else { continue };
            // v6+ `.kicad_mod` only; a v5 `(module ...)` uses fp_text fields we
            // would have to rewrite structurally, so it stays on minimal
            // emission rather than risking a half-translated node.
            if root.name() != Some("footprint") {
                continue;
            }
            // Clone materialises owned text, so the node safely outlives `doc`.
            return Some(root.clone());
        }
        None
    }
}

/// `KICAD_FOOTPRINT_DIR`, `KICAD9_FOOTPRINT_DIR`, ... (any version digits).
fn is_kicad_footprint_dir_var(key: &str) -> bool {
    let Some(rest) = key.strip_prefix("KICAD") else {
        return false;
    };
    let Some(ver) = rest.strip_suffix("_FOOTPRINT_DIR") else {
        return false;
    };
    ver.is_empty() || ver.chars().all(|c| c.is_ascii_digit())
}

/// Candidate global fp-lib-table files, newest KiCad version first.
fn global_fp_lib_tables() -> Vec<PathBuf> {
    let mut out: Vec<(f64, PathBuf)> = Vec::new();
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    for base in [
        home.join("Library/Preferences/kicad"), // macOS
        home.join(".config/kicad"),             // Linux
    ] {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for e in entries.flatten() {
            let dir = e.path();
            let table = dir.join("fp-lib-table");
            if !table.is_file() {
                continue;
            }
            // Version dirs are "9.0", "10.0", ...; sort numerically so 10 > 9.
            let ver = dir
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.parse::<f64>().ok())
                .unwrap_or(0.0);
            out.push((ver, table));
        }
    }
    out.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    out.into_iter().map(|(_, p)| p).collect()
}

/// Parse a KiCad `fp-lib-table`, returning `(nickname, .pretty dir)` pairs.
///
/// Only `(type "KiCad")` rows resolve (other backends are not `.kicad_mod`
/// directories). `${VAR}` references substitute through `lookup`; a row with an
/// unresolvable variable or a missing directory is skipped, never an error.
fn parse_fp_lib_table(
    text: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(doc) = forge_sexpr::parse(text) else {
        return out;
    };
    let Some(root) = doc.root() else { return out };
    if root.name() != Some("fp_lib_table") {
        return out;
    }
    for lib in root.find_all("lib") {
        let Some(nick) = lib.find_value("name") else {
            continue;
        };
        let ty = lib.find_value("type").unwrap_or_default();
        if !ty.eq_ignore_ascii_case("kicad") {
            continue;
        }
        let Some(uri) = lib.find_value("uri") else {
            continue;
        };
        let Some(path) = substitute_uri(&uri, lookup) else {
            continue;
        };
        if path.is_dir() {
            out.push((nick, path));
        }
    }
    out
}

/// Substitute `${VAR}` references in a library uri. Returns None if any
/// variable cannot be resolved (e.g. `${KIPRJMOD}`: we have no project scope).
fn substitute_uri(uri: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let mut out = String::with_capacity(uri.len());
    let mut rest = uri;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}')?;
        let var = &after[..end];
        out.push_str(&lookup(var)?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(PathBuf::from(out))
}

// ---------------------------------------------------------------------------
// Dressing: library node + board data -> emitted footprint node
// ---------------------------------------------------------------------------

/// Build the emitted footprint node for `comp` from the installed library
/// footprint `lib_fp`, or None when the library cannot be trusted for this
/// component (the caller then falls back to minimal emission).
///
/// The library CST is copied verbatim (courtyard, fab and silk graphics,
/// `attr`, `descr`, `tags`, `(model ...)` with its origin-correct offset) and
/// the board data is patched in: full lib id, placement `(at x y rot)`,
/// Reference/Value property values, pad nets, and the footprint rotation added
/// to every pad/property/fp_text angle (see the module docs for why angles are
/// the only rotated field).
pub(crate) fn dress_footprint(
    lib_fp: &List,
    comp: &Comp,
    net_id: &HashMap<String, i64>,
) -> Option<List> {
    // Back-side placement would need a full layer flip (graphics layers, y
    // mirror); refuse and keep minimal emission rather than emit a wrong body.
    if comp.layer != "F.Cu" {
        return None;
    }

    // Guard: every DSL pad must match a library pad on number, kind, shape,
    // local position and size. A decompiled board matches exactly; a hand-
    // edited pad must win over the library, so any mismatch refuses dressing.
    let lib_pads: Vec<&List> = lib_fp.find_all("pad").collect();
    let mut pad_net: Vec<Option<(i64, &str)>> = vec![None; lib_pads.len()];
    let mut consumed = vec![false; lib_pads.len()];
    for p in &comp.pads {
        let mut matched = false;
        for (i, lp) in lib_pads.iter().enumerate() {
            if consumed[i] {
                continue;
            }
            if lp.arg_value(0).as_deref() != Some(p.number.as_str())
                || lp.arg_value(1).as_deref() != Some(p.kind.as_str())
                || lp.arg_value(2).as_deref() != Some(p.shape.as_str())
            {
                continue;
            }
            let lat = lp
                .find("at")
                .map(|l| (l.arg_f64(0).unwrap_or(0.0), l.arg_f64(1).unwrap_or(0.0)));
            let lsz = lp
                .find("size")
                .map(|l| (l.arg_f64(0).unwrap_or(0.0), l.arg_f64(1).unwrap_or(0.0)));
            let (Some(lat), Some(lsz)) = (lat, lsz) else {
                continue;
            };
            if (lat.0 - p.at.0).abs() > PAD_POS_TOL
                || (lat.1 - p.at.1).abs() > PAD_POS_TOL
                || (lsz.0 - p.size.0).abs() > PAD_SIZE_TOL
                || (lsz.1 - p.size.1).abs() > PAD_SIZE_TOL
            {
                continue;
            }
            consumed[i] = true;
            if let Some(net) = &p.net {
                pad_net[i] = net_id.get(net).map(|id| (*id, net.as_str()));
            }
            matched = true;
            break;
        }
        if !matched {
            return None;
        }
    }

    let mut fp = lib_fp.clone();

    // Footprint name: the library file stores the bare name; a board stores the
    // full "Lib:Name" id. Preserve the token's leading trivia.
    match fp.children.get_mut(1) {
        Some(Sexpr::Token(t)) => t.raw = quote(&comp.lib_id).into(),
        _ => return None,
    }

    // Library-file-only headers do not belong in a board footprint.
    fp.children.retain(|c| {
        !matches!(
            c,
            Sexpr::List(l) if matches!(
                l.name(),
                Some("version") | Some("generator") | Some("generator_version")
            )
        )
    });

    // Placement: insert `(at x y [rot])` right after the `(layer ...)` node
    // (or after the name when the library carries no layer).
    let at_idx = fp
        .children
        .iter()
        .position(|c| matches!(c, Sexpr::List(l) if l.name() == Some("layer")))
        .map(|i| i + 1)
        .unwrap_or(2);
    let mut at_children = vec![
        Sexpr::Token(Token::atom("at")),
        tok(" ", fmt_f64(comp.at.0)),
        tok(" ", fmt_f64(comp.at.1)),
    ];
    let rot = normalize_angle(comp.rot);
    if rot != 0.0 {
        at_children.push(tok(" ", fmt_f64(rot)));
    }
    let mut at_list = List::new(at_children);
    at_list.leading = "\n\t".into();
    fp.children.insert(at_idx, Sexpr::List(at_list));

    // Patch board data into the copied children.
    let mut pad_ordinal = 0usize;
    for child in fp.children.iter_mut() {
        let Sexpr::List(l) = child else { continue };
        match l.name() {
            Some("property") => {
                let key = l.arg_value(0);
                let new = match key.as_deref() {
                    Some("Reference") => Some(comp.reference.as_str()),
                    Some("Value") => Some(comp.value.as_str()),
                    _ => None,
                };
                if let Some(v) = new {
                    if let Some(Sexpr::Token(t)) = l.children.get_mut(2) {
                        t.raw = quote(v).into();
                    }
                }
                add_rotation_to_at_angle(l, rot);
            }
            Some("fp_text") => add_rotation_to_at_angle(l, rot),
            Some("pad") => {
                add_rotation_to_at_angle(l, rot);
                if let Some(Some((id, name))) = pad_net.get(pad_ordinal) {
                    let mut net = List::new(vec![
                        Sexpr::Token(Token::atom("net")),
                        tok(" ", id.to_string()),
                        tok(" ", quote(name)),
                    ]);
                    net.leading = "\n\t\t".into();
                    l.push(Sexpr::List(net));
                }
                pad_ordinal += 1;
            }
            _ => {}
        }
    }

    Some(fp)
}

/// Add the footprint rotation to the angle field of this item's `(at x y ang)`.
///
/// In a kicad_pcb, pad and text `at` angles are stored as absolute angles
/// (footprint rotation included) while X/Y stay footprint-local; a `.kicad_mod`
/// stores the same fields relative to an unrotated footprint. So placement at
/// rotation R means: angle' = angle + R, X/Y untouched.
fn add_rotation_to_at_angle(item: &mut List, rot: f64) {
    if rot == 0.0 {
        return;
    }
    let Some(at) = item.find_mut("at") else {
        return;
    };
    // children: ["at", x, y, angle?]
    match at.children.get_mut(3) {
        Some(Sexpr::Token(t)) => {
            let a = t.as_f64().unwrap_or(0.0);
            t.raw = fmt_f64(normalize_angle(a + rot)).into();
        }
        _ => at.children.push(tok(" ", fmt_f64(rot))),
    }
}

fn normalize_angle(a: f64) -> f64 {
    let mut a = a % 360.0;
    if a < 0.0 {
        a += 360.0;
    }
    a
}

fn tok(leading: &str, raw: impl Into<String>) -> Sexpr {
    Sexpr::Token(Token {
        leading: leading.into(),
        raw: raw.into().into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::Program;
    use std::path::PathBuf;

    fn fixture_roots() -> Vec<PathBuf> {
        vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/fplib")]
    }

    /// A program whose pads exactly match the fixture library footprint.
    fn matching_program(rot: f64) -> Program {
        let code = format!(
            r#"# Board-as-Code (hauksbee board DSL v1)
board version 20241229

fn main {{
    net "A"
    net "B"
    comp R1 lib "Fixture_Lib:R_TEST" val "10k" layer "F.Cu" at 100 50 rot {rot} {{
        pad "1" smd roundrect at -0.825 0 size 0.8 0.95 layers [F.Cu F.Paste F.Mask] net "A"
        pad "2" smd roundrect at 0.825 0 size 0.8 0.95 layers [F.Cu F.Paste F.Mask] net "B"
    }}
}}
"#
        );
        Program::parse(&code).expect("fixture program parses")
    }

    #[test]
    fn dressed_footprint_copies_graphics_model_and_refdes_from_library() {
        let prog = matching_program(0.0);
        let mut lib = FootprintLib::with_roots(fixture_roots());
        let text = prog.build_with_library(&mut lib).emit();

        // Full lib id survives, placement is the board's.
        assert!(
            text.contains(r#"(footprint "Fixture_Lib:R_TEST""#),
            "{text}"
        );
        assert!(
            text.contains("(at 100 50)"),
            "board placement present: {text}"
        );
        // Courtyard, silk and fab graphics copied through verbatim.
        assert!(
            text.contains(r#"(layer "F.CrtYd")"#),
            "courtyard copied: {text}"
        );
        assert!(text.contains(r#"(layer "F.SilkS")"#), "silk copied: {text}");
        assert!(text.contains(r#"(layer "F.Fab")"#), "fab copied: {text}");
        // The 3D model block is verbatim (origin authored against this very
        // footprint, so no origin correction is ever needed).
        assert!(
            text.contains(r#"(model "${KICAD9_3DMODEL_DIR}/Resistor_SMD.3dshapes/R_TEST.wrl""#),
            "model copied: {text}"
        );
        // Reference designator text at the library's authored position.
        assert!(
            text.contains(r#"(property "Reference" "R1""#),
            "refdes value patched: {text}"
        );
        assert!(
            text.contains("(at 0 -1.43 0)"),
            "library refdes position kept: {text}"
        );
        // Board nets injected into the copied pads; library pad detail kept.
        assert!(
            text.contains(r#"(net 1 "A")"#) && text.contains(r#"(net 2 "B")"#),
            "{text}"
        );
        assert!(
            text.contains("(roundrect_rratio 0.25)"),
            "library pad detail kept: {text}"
        );
        // Library-file headers must not leak into a board footprint.
        assert!(!text.contains("kicad-footprint-generator"), "{text}");
    }

    #[test]
    fn rotation_is_added_to_pad_and_text_angles_but_not_coordinates() {
        let prog = matching_program(90.0);
        let mut lib = FootprintLib::with_roots(fixture_roots());
        let text = prog.build_with_library(&mut lib).emit();

        assert!(text.contains("(at 100 50 90)"), "rotated placement: {text}");
        // Pad 1 is authored at angle 90 in the library; 90 + 90 = 180. Pad 2
        // has no authored angle; it gains the footprint's 90. X/Y stay local.
        assert!(
            text.contains("(at -0.825 0 180)"),
            "pad 1 angle summed: {text}"
        );
        assert!(
            text.contains("(at 0.825 0 90)"),
            "pad 2 angle added: {text}"
        );
        // Text angles are absolute in a board file too.
        assert!(
            text.contains("(at 0 -1.43 90)"),
            "refdes angle rotated: {text}"
        );
        // Graphics coordinates are footprint-local: untouched.
        assert!(
            text.contains("(start -1.48 -0.73)"),
            "courtyard untouched: {text}"
        );
    }

    #[test]
    fn missing_library_degrades_to_todays_minimal_emission() {
        let prog = matching_program(0.0);
        // Disabled resolver = a machine with no KiCad installed.
        let minimal = prog
            .build_with_library(&mut FootprintLib::disabled())
            .emit();
        assert!(
            !minimal.contains("model"),
            "no invented model block: {minimal}"
        );
        assert!(
            !minimal.contains("F.CrtYd"),
            "no invented graphics: {minimal}"
        );
        assert!(
            minimal.contains(r#"(net 1 "A")"#),
            "connectivity intact: {minimal}"
        );

        // An unknown lib id with a real resolver takes the same path.
        let mut lib = FootprintLib::with_roots(fixture_roots());
        let code = r#"# Board-as-Code (hauksbee board DSL v1)
board version 20241229

fn main {
    net "A"
    comp U9 lib "No_Such_Lib:NOPE" val "x" layer "F.Cu" at 0 0 rot 0 {
        pad "1" smd rect at 0 0 size 1 1 layers [F.Cu] net "A"
    }
}
"#;
        let prog = Program::parse(code).expect("parses");
        let text = prog.build_with_library(&mut lib).emit();
        assert!(text.contains(r#"(footprint "No_Such_Lib:NOPE""#), "{text}");
        assert!(!text.contains("model"), "unknown lib stays minimal: {text}");
    }

    #[test]
    fn edited_pads_refuse_library_dressing() {
        // Same lib id, but the pads have been moved by hand: the library file
        // no longer describes this part, so copy-through must refuse rather
        // than clobber the edit.
        let code = r#"# Board-as-Code (hauksbee board DSL v1)
board version 20241229

fn main {
    net "A"
    net "B"
    comp R1 lib "Fixture_Lib:R_TEST" val "10k" layer "F.Cu" at 0 0 rot 0 {
        pad "1" smd roundrect at -2 0 size 0.8 0.95 layers [F.Cu] net "A"
        pad "2" smd roundrect at 2 0 size 0.8 0.95 layers [F.Cu] net "B"
    }
}
"#;
        let prog = Program::parse(code).expect("parses");
        let mut lib = FootprintLib::with_roots(fixture_roots());
        let text = prog.build_with_library(&mut lib).emit();
        assert!(
            !text.contains("model"),
            "edited pads keep minimal emission: {text}"
        );
        assert!(
            text.contains("(at -2 0"),
            "the edited pad geometry wins: {text}"
        );
    }

    #[test]
    fn dressing_is_deterministic_and_idempotent() {
        let prog = matching_program(90.0);
        let a = prog
            .build_with_library(&mut FootprintLib::with_roots(fixture_roots()))
            .emit();
        let b = prog
            .build_with_library(&mut FootprintLib::with_roots(fixture_roots()))
            .emit();
        assert_eq!(a, b, "identical input must emit identical bytes");
        // The emitted board still parses as a valid PCB.
        forge_model::Pcb::parse(&a).expect("dressed board re-parses");
    }

    #[test]
    fn fp_lib_table_substitutes_variables_and_skips_unresolved() {
        let table = r#"(fp_lib_table
  (version 7)
  (lib (name "Fixture_Lib")(type "KiCad")(uri "${TEST_FP_DIR}/Fixture_Lib.pretty")(options "")(descr ""))
  (lib (name "Broken")(type "KiCad")(uri "${KIPRJMOD}/x.pretty")(options "")(descr ""))
  (lib (name "NotKicad")(type "Eagle")(uri "${TEST_FP_DIR}/Fixture_Lib.pretty")(options "")(descr ""))
)"#;
        let root = fixture_roots().remove(0);
        let lookup = |var: &str| -> Option<String> {
            (var == "TEST_FP_DIR").then(|| root.display().to_string())
        };
        let rows = parse_fp_lib_table(table, &lookup);
        assert_eq!(rows.len(), 1, "only the resolvable KiCad row: {rows:?}");
        assert_eq!(rows[0].0, "Fixture_Lib");
        assert!(rows[0].1.ends_with("Fixture_Lib.pretty"));
    }
}
