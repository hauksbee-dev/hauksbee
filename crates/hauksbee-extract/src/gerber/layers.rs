//! Layer-role inference from fab filenames.
//!
//! A gerber job may ship exporter metadata (`.gbrjob`, Altium `.EXTREP` and
//! `.LDP`) stating what its files are. Where it does not, names are the only
//! clue. There is no single convention, so we recognise the common ones and
//! let an explicit mapping file override when a board is exotic:
//!
//!   - **KiCad long names**: `board-F_Cu.gbr`, `board-B_Cu.gbr`, inner
//!     `board-In1_Cu.gbr`.
//!   - **Protel / legacy extensions**: `.GTL` (top copper), `.GBL` (bottom),
//!     `.G1L`/`.G2L`/`.GP1`… (inner), `.GTP`/`.GBP` (paste), `.GTO`/`.GBO`
//!     (silk), `.GTS`/`.GBS` (mask), `.GKO`/`.GM1` (outline), `.TXT`/`.DRL`
//!     (drill), `.GBR`/`.GB?` ambiguous.
//!   - **Altium-ish**: `.GTL`/`.GBL` plus `Top Layer.gbr` etc.
//!   - **Generic words**: a filename containing `top`+`copper`, `bottom`+`cu`,
//!     `signal`, `inner`, etc.
//!
//! The classifier only needs to find the *copper* and *drill* roles for
//! electrical reconstruction; everything else (silk, mask, paste, courtyard,
//! fab, outline) is recognised so it can be deliberately ignored rather than
//! mis-read as copper.

use std::path::Path;

/// What a fab file is, for the purposes of electrical reconstruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerRole {
    /// Copper signal layer. `index` is the stack position: 0 = top (F.Cu),
    /// `n-1` = bottom (B.Cu), 1..n-1 = inner layers top-to-bottom. We don't
    /// always know the total layer count up front, so inner indices are
    /// assigned provisionally and re-ordered once the whole set is seen.
    Copper { index: usize, name: String },
    /// Excellon drill file (plated or non-plated; the file's own records say
    /// which). Gerber-format drills are also possible but rare.
    Drill,
    /// Board outline / edge cuts. Useful for bounds, not connectivity.
    Outline,
    /// Recognised but electrically irrelevant (silk, mask, paste, fab,
    /// courtyard, adhesive, assembly drawing, ...).
    Ignored,
    /// Could not be classified.
    Unknown,
}

impl LayerRole {
    pub fn is_copper(&self) -> bool {
        matches!(self, LayerRole::Copper { .. })
    }
}

/// Lowercased file *stem*-and-extension view used by the matchers.
struct Name<'a> {
    /// Full lowercased file name (`board-f_cu.gbr`).
    full: String,
    /// Lowercased extension without the dot (`gbr`, `gtl`, `drl`), if any.
    ext: Option<String>,
    /// Original (case-preserved) layer label we surface to the user.
    original: &'a str,
}

impl<'a> Name<'a> {
    fn of(path: &'a Path) -> Self {
        let file = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase());
        Name {
            full: file.to_ascii_lowercase(),
            ext,
            original: path.file_stem().and_then(|s| s.to_str()).unwrap_or(""),
        }
    }

    fn has(&self, needle: &str) -> bool {
        self.full.contains(needle)
    }

    fn ext_is(&self, e: &str) -> bool {
        self.ext.as_deref() == Some(e)
    }

    /// Does the name carry `cu` as a COPPER token?
    ///
    /// As a bare substring `cu` matches inside `circuit`, `accumulator`, `vcut`,
    /// `document` and `cube`, so any project so named supplied the copper token and a
    /// mechanical or documentation film became copper. Requiring `cu` to END a word
    /// fixed that and broke the other side: `cu` glued to a FOLLOWING role word is a
    /// real convention, and `-CuTop.gbr` / `-CuBottom.gbr` ship on thirteen corpus
    /// zips plus a loose directory (the crkbd corne boards, six switch-plate PCBs and
    /// the RoyalBlue54L antenna). Those went Unknown, which is dropped with no note,
    /// and the RoyalBlue54L directory failed outright with "no copper gerber layers
    /// found here".
    ///
    /// So: a whole word (`-F_Cu`, `_cu.gbr`), or ending one (`TopCu`), or abutting a
    /// side or stack token on its right (`CuTop`, `CuBottom`, `CuIn1`). Nothing else.
    fn has_cu_token(&self) -> bool {
        const AFTER: [&str; 8] = ["top", "bot", "bottom", "front", "back", "in", "l", "mid"];
        let hay = self.full.as_bytes();
        self.match_positions("cu").any(|i| {
            let j = i + 2;
            match hay.get(j) {
                // Word end: `-F_Cu.gbr`, `In1_Cu`, and `TopCu` where only the left
                // side is glued.
                None => true,
                Some(&b) if !b.is_ascii_alphabetic() => true,
                // Glued to a role word on the right.
                _ => {
                    let tail = &self.full[j..];
                    AFTER.iter().any(|t| {
                        tail.strip_prefix(*t).is_some_and(|rest| {
                            // `in`/`l` only count when a stack number follows, so
                            // `culminate` and `clone` are not copper. The role itself
                            // must end too: `CuTopography`, `CuTopcoat` and
                            // `CuBottomless` are project words, not layer tokens.
                            if *t == "in" || *t == "l" {
                                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                                digits > 0
                                    && rest
                                        .as_bytes()
                                        .get(digits)
                                        .is_none_or(|byte| !byte.is_ascii_alphabetic())
                            } else {
                                rest.as_bytes()
                                    .first()
                                    .is_none_or(|byte| !byte.is_ascii_alphabetic())
                            }
                        })
                    })
                }
            }
        })
    }

    /// Byte offsets where `needle` occurs. `None` of them when the needle is longer
    /// than the name, which `0..len - needle_len + 1` did NOT express: with
    /// `saturating_sub` the range became `0..1`, and indexing `hay[0..needle_len]`
    /// then panicked. A fab folder holding a file called `a` or `x.g` aborted
    /// extraction with a panic instead of an error, on the `border` test.
    fn match_positions<'n>(&'n self, needle: &'n str) -> impl Iterator<Item = usize> + 'n {
        let hay = self.full.as_bytes();
        let ned = needle.as_bytes();
        let upto = hay.len().checked_sub(ned.len()).map(|n| n + 1).unwrap_or(0);
        (0..upto).filter(move |&i| &hay[i..i + ned.len()] == ned)
    }

    /// `needle` present as a whole word, i.e. not run together with other
    /// letters. Substring matching is wrong for the bare role names: `top`
    /// appears inside `stopmask` and `bot` inside `robot`, and a mask film read
    /// as copper poisons the whole reconstruction. Digits count as separators so
    /// `1 - Top`, `L2-GND` and `top2` all match their role.
    fn has_word(&self, needle: &str) -> bool {
        let hay = self.full.as_bytes();
        let alpha = |b: u8| b.is_ascii_alphabetic();
        self.match_positions(needle).any(|i| {
            let before_ok = i == 0 || !alpha(hay[i - 1]);
            let j = i + needle.len();
            let after_ok = j >= hay.len() || !alpha(hay[j]);
            before_ok && after_ok
        })
    }
}

/// A KiCad-style layer suffix stating outright which copper layer a film is:
/// `-F_Cu`, `-B_Cu`, `-In<n>_Cu`, with `.` accepted for `_`. Unambiguous, so it is
/// read before the non-copper word sweep, which cannot otherwise be talked out of
/// claiming a file whose project name happens to contain one of its words.
fn explicit_kicad_copper(n: &Name) -> Option<LayerRole> {
    let stem = n.original.to_ascii_lowercase();
    let token_suffix = |token: &str| {
        stem.strip_suffix(token).is_some_and(|prefix| {
            prefix
                .as_bytes()
                .last()
                .is_none_or(|byte| !byte.is_ascii_alphanumeric())
        })
    };
    if token_suffix("f_cu") || token_suffix("f.cu") {
        return Some(LayerRole::Copper {
            index: 0,
            name: top_label(n.original),
        });
    }
    if token_suffix("b_cu") || token_suffix("b.cu") {
        return Some(LayerRole::Copper {
            index: usize::MAX,
            name: bottom_label(n.original),
        });
    }
    for sep in ['_', '.'] {
        let tail = format!("{sep}cu");
        for pos in stem.match_indices("in").map(|(p, _)| p) {
            if pos > 0 && stem.as_bytes()[pos - 1].is_ascii_alphanumeric() {
                continue;
            }
            let rest = &stem[pos + 2..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            let Ok(k) = digits.parse::<usize>() else {
                continue;
            };
            if (1..=32).contains(&k) && rest[digits.len()..] == tail {
                return Some(LayerRole::Copper {
                    index: k,
                    name: n.original.to_string(),
                });
            }
        }
    }
    None
}

/// Extensions that are certainly NOT a plotted film: documents, data files and
/// job metadata that a fab folder ships alongside the gerbers. Used to refuse
/// copper classification for names that would otherwise match on a role word
/// alone; anything not listed is left to the role rules, so an exporter's
/// unusual film extension still reaches them.
fn is_definitely_not_a_film(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "csv"
            | "pos"
            | "pdf"
            | "xlsx"
            | "xls"
            | "doc"
            | "docx"
            | "json"
            | "xml"
            | "html"
            | "htm"
            | "md"
            | "zip"
            | "gz"
            | "7z"
            | "rar"
            | "png"
            | "jpg"
            | "jpeg"
            | "svg"
            | "dxf"
            | "step"
            | "stp"
            | "gbrjob"
            | "ipc"
            | "log"
            | "ini"
            | "toml"
            | "yaml"
            | "yml"
    )
}

/// Extensions that hold a plotted gerber film. A file with one of these that
/// survived every non-copper test is a candidate for the bare role-name rules.
fn is_gerber_film_ext(n: &Name) -> bool {
    matches!(
        n.ext.as_deref(),
        Some("gbr" | "ger" | "gdo" | "pho" | "grb" | "art" | "gerber")
    )
}

/// Classify a single fab file by its path. Inner-copper indices are
/// provisional (see [`assign_inner_indices`]).
pub fn classify(path: &Path) -> LayerRole {
    let n = Name::of(path);

    // ── Nothing with a non-film extension is a fab film, of any role ────────
    // Hoisted above the drill checks because a drill MAP is a drawing:
    // `corne-cherry-NPTH-drl_map.pdf` carries the `drl` token, matched a name-based
    // drill rule, and `read_to_string` on the PDF then failed the WHOLE extraction
    // with "stream did not contain valid UTF-8". Thirteen corpus zips ship one. None
    // of the extensions on this list is a drill or film extension, and returning
    // Unknown is what routes a `.csv`/`.pos` to the placement reader, so hoisting
    // costs nothing.
    if n.ext.as_deref().is_some_and(is_definitely_not_a_film) {
        return LayerRole::Unknown;
    }

    // ── Drill first: extension is the strongest signal ──────────────────────
    if n.ext_is("drl") || n.ext_is("txt") || n.ext_is("xln") || n.ext_is("nc") || n.ext_is("tap") {
        // `.txt` is sometimes a readme; require a drill-ish hint when ambiguous.
        // Altium exports the drill as `<board>-RoundHoles.TXT` /
        // `-RectHoles.TXT` / `-SlotHoles.TXT` (the Inkplate 6 set), so `holes`
        // is a drill hint too. A plain `README.TXT` has none of these tokens
        // and is still ignored.
        if n.ext_is("txt")
            && !(n.has("drill") || n.has("drl") || n.has("nc") || n.has("pth") || n.has("holes"))
        {
            return LayerRole::Ignored;
        }
        return LayerRole::Drill;
    }
    if n.has("drill") || n.has("-pth") || n.has("-npth") || n.has(".pth") || n.has(".npth") {
        return LayerRole::Drill;
    }
    // Allegro gerber-format drill film, e.g. `drill-1-6.art`: drilling drawn as
    // flashes on a gerber layer rather than as a separate Excellon file. The
    // `drill` substring above already catches the common names; this keeps the
    // `.art` ones routed to the drill role explicitly.
    if n.ext_is("art") && n.has("drill") {
        return LayerRole::Drill;
    }

    // ── Things that are explicitly NOT copper (check before generic copper) ──
    // Order matters: paste/mask/silk often also contain "top"/"bottom".
    // ── Outline by extension, before the word sweep ─────────────────────────
    // `.GKO`/`.GM1`/`.GML` are Altium's board-outline films, and `Mechanical_1` is
    // what Altium calls the layer they are plotted from, so the word sweep below
    // claimed `board-Mechanical_1.GM1` as Ignored and the outline was lost.
    if n.ext_is("gko") || n.ext_is("gm1") || n.ext_is("gml") {
        return LayerRole::Outline;
    }

    // ── An EXPLICIT copper suffix outranks the word sweep ───────────────────
    // The sweep below returns Ignored unconditionally on a raw substring, so a
    // project whose NAME contains a non-copper word lost its copper outright:
    // `mechanical-keyboard-F_Cu.gbr`, `MechanicalKeyboard-B_Cu.gbr`,
    // `mechanical_keyboard-In1_Cu.gbr`, `Documentation-F_Cu.gbr` and the
    // pre-existing `fabricator-F_Cu.gbr` all classified Ignored. Mechanical
    // keyboards are one of the largest open-hardware PCB categories and this
    // corpus is full of them. `F_Cu`/`B_Cu`/`In<n>_Cu` is not a word that might
    // appear in a project name; it is KiCad stating the layer, so it wins.
    if let Some(role) = explicit_kicad_copper(&n) {
        return role;
    }

    let non_copper = [
        ("paste", &["gtp", "gbp"][..]),
        ("mask", &["gts", "gbs"][..]),
        ("solder", &["gts", "gbs", "gtp", "gbp"][..]), // soldermask/solderpaste
        ("silk", &["gto", "gbo"][..]),
        ("legend", &["gto", "gbo"][..]),
        ("overlay", &["gto", "gbo"][..]),
        ("courtyard", &[][..]),
        ("fab", &[][..]),
        ("assembly", &[][..]),
        // The `Top.gbr`/`Bottom.gbr` exporters (DipTrace and friends) ship
        // `TopAssy.gbr` and `TopDimension.gbr` in the same folder. Those names
        // carry the same bare role word as the copper film, so the bare-name
        // copper rules below would read an assembly drawing as a copper layer
        // and reconstruct nets out of it. Exclude them by their OWN word first.
        ("assy", &[][..]),
        ("dimension", &[][..]),
        ("drawing", &[][..]),
        ("keepout", &[][..]),
        // Altium and Eagle both plot these beside the copper, and every name-based
        // copper rule below keys on WORDS: `MyCircuit_Mechanical_Layer_1.gbr` and
        // `Documentation Layer 1.gbr` came back as inner COPPER, which puts outline
        // and dimension lines on a layer every drill barrel stitches. That is a
        // large false merge, the very bug the negative-pour work exists to fix,
        // reintroduced by a filename, and it inflates the layer count that
        // blind-via span resolution reads.
        // Whole words, unlike the rest of this list. `MechanicalKeyboard-B_Cu.gbr`
        // glues the word to the next one, and the explicit-suffix rule above already
        // rescues the separated forms; requiring a word here means the raw substring
        // cannot reach past a project name either.
        ("mechanical", &[][..]),
        ("documentation", &[][..]),
        ("adhes", &["gma", "gba"][..]),
        ("glue", &[][..]),
    ];
    for (word, exts) in non_copper {
        let hit = if word == "mechanical" || word == "documentation" {
            n.has_word(word)
        } else {
            n.has(word)
        };
        if hit || exts.iter().any(|e| n.ext_is(e)) {
            return LayerRole::Ignored;
        }
    }

    // ── Outline / edge ──────────────────────────────────────────────────────
    if n.has("edge")
        || n.has("outline")
        || n.has("boardoutline")
        || n.has("-gko")
        || n.has("margin")
        || n.has_word("border")
        || n.ext_is("gko")
        || n.ext_is("gm1")
        || n.ext_is("gml")
    {
        return LayerRole::Outline;
    }

    // ── Copper by Protel/Altium extension ───────────────────────────────────
    if n.ext_is("gtl") {
        return LayerRole::Copper {
            index: 0,
            name: top_label(n.original),
        };
    }
    if n.ext_is("gbl") {
        return LayerRole::Copper {
            index: usize::MAX,
            name: bottom_label(n.original),
        };
    }
    // Inner copper: .G1L/.G2L… or .GP1/.GP2… or .G1/.G2…
    if let Some(idx) = protel_inner_index(&n) {
        return LayerRole::Copper {
            index: idx,
            name: n.original.to_string(),
        };
    }

    // ── Nothing below here can be copper unless the file is a plotted film ──
    // Every rule from here on keys on WORDS in the name, and the words a fab job
    // puts on its copper films appear just as readily on the documents beside
    // them: Altium's per-side pick-and-place is `Pick Place for <board> - Top
    // Layer.csv` and its per-layer prints are `<board>_Copper_Top.pdf`. The
    // directory scan claims copper before it looks for placement data, so a
    // matched CSV was swallowed as an empty copper film and the components never
    // bound. Refusing here rather than inside one rule covers the top, bottom,
    // inner, bare-role and `.art` rules alike.
    //
    // Stated as what a film is NOT, deliberately. An allowlist of film extensions
    // would drop the copper of any exporter using a name outside it
    // (`-Inner1.gbx`, `-signal_2.gb`), which is the same silent loss this
    // function has been fixed for twice; the thing actually being excluded is a
    // small, known set of non-films. Everything genuinely ambiguous (`.txt` for a
    // drill, extensionless plots) is already resolved above or left to the rules.
    if n.ext.as_deref().is_some_and(is_definitely_not_a_film) {
        return LayerRole::Unknown;
    }

    // ── Copper by KiCad/long name ───────────────────────────────────────────
    // `*-F_Cu.gbr`, `*-B_Cu.gbr`, `*-In1_Cu.gbr` (case-insensitive).
    // `cu` has to END a word. As a bare substring it matches inside `circuit`,
    // `accumulator`, `vcut`, `document` and `cube`, so any project name containing
    // one of those plus a role word read as copper. Requiring a word END keeps the
    // forms that occur (`-F_Cu`, `In1_Cu`, `TopCu`) and rejects the ones that do not.
    let has_copper = |n: &Name| n.has_cu_token() || n.has("copper");
    if n.has("f_cu")
        || n.has("f.cu")
        || (n.has("top") && has_copper(&n))
        || n.has("toplayer")
        || n.has("top layer")
    {
        return LayerRole::Copper {
            index: 0,
            name: top_label(n.original),
        };
    }
    if n.has("b_cu")
        || n.has("b.cu")
        || (n.has("bottom") && has_copper(&n))
        || n.has("bottomlayer")
        || n.has("bottom layer")
    {
        return LayerRole::Copper {
            index: usize::MAX,
            name: bottom_label(n.original),
        };
    }
    if let Some(idx) = kicad_inner_index(&n) {
        return LayerRole::Copper {
            index: idx,
            name: n.original.to_string(),
        };
    }

    // ── Bare role names on a plotted film ───────────────────────────────────
    // `Top.gbr` / `Bottom.gbr`, `1 - Top.gbr` / `2 - Bottom.gbr`,
    // `board-Front.gbr` / `board-Back.gbr`, `TOP.gbr` / `BOTTOM.gbr` with
    // `L2-GND.gbr` inners. DipTrace, Sprint Layout, PCB Elegance and several
    // house CAM scripts all plot copper this way, and it was the single most
    // common shape in a 60-job corpus of real fab folders: the KiCad and Protel
    // rules above matched none of them, so the whole job was refused with "no
    // copper gerber layers found here" while the copper sat right there.
    //
    // Safe only HERE, at the end: every mask / paste / silk / assembly /
    // dimension / outline film carrying the same role word has already been
    // claimed by the tests above, so what is left with a bare `top` is copper.
    if is_gerber_film_ext(&n) {
        if n.has_word("top") || n.has_word("front") {
            return LayerRole::Copper {
                index: 0,
                name: top_label(n.original),
            };
        }
        if n.has_word("bottom") || n.has_word("bot") || n.has_word("back") {
            return LayerRole::Copper {
                index: usize::MAX,
                name: bottom_label(n.original),
            };
        }
        // Inner films named by stack position: `L2-GND.gbr`, `l3.gbr`. L1 is the
        // top layer, so `L<n>` maps to inner index n-1.
        if let Some(idx) = bare_stack_index(&n) {
            return LayerRole::Copper {
                index: idx,
                name: n.original.to_string(),
            };
        }
    }

    // ── Allegro / Cadence `.art` exports (e.g. uConsole) ────────────────────
    // The copper films are named by role: `top.art`, `bottom.art`, and inner
    // plane layers `gnd02.art`, `pwr04.art`, `gnd05.art` where the number is the
    // stack position. The mask/silk/paste films (`solder_*`, `silk_*`,
    // `paste_*`) were already caught above; the assembly films (`adt`, `adb`)
    // and `art03` / `art_aper` are not copper.
    if n.ext_is("art") {
        let stem = &n.full;
        if stem.starts_with("top") {
            return LayerRole::Copper {
                index: 0,
                name: "TOP".to_string(),
            };
        }
        if stem.starts_with("bottom") || stem.starts_with("bot.") {
            return LayerRole::Copper {
                index: usize::MAX,
                name: "BOTTOM".to_string(),
            };
        }
        // Inner plane with an embedded stack number: gnd02, pwr04, gnd05, l3 …
        if stem.starts_with("gnd")
            || stem.starts_with("pwr")
            || stem.starts_with("pgnd")
            || stem.starts_with("power")
            || stem.starts_with("plane")
        {
            let digits: String = stem.chars().filter(|c| c.is_ascii_digit()).collect();
            if let Ok(k) = digits.parse::<usize>() {
                if k >= 1 {
                    return LayerRole::Copper {
                        index: k,
                        name: n.original.to_string(),
                    };
                }
            }
            // No number: still a copper plane, drop it after top, before bottom.
            return LayerRole::Copper {
                index: 1,
                name: n.original.to_string(),
            };
        }
    }

    LayerRole::Unknown
}

/// An explicit layer-role mapping file: the escape hatch for exotic jobs whose
/// file names defeat the conventions. Format is one `filename = role` per line
/// (`#` comments allowed), where role is `copper:<index>` (0 = top, larger =
/// deeper, `bottom` for the bottom layer), `drill`, `outline`, or `ignore`:
///
/// ```text
/// top.art    = copper:0
/// gnd02.art  = copper:1
/// pwr04.art  = copper:2
/// bottom.art = copper:bottom
/// drill-1-6.art = drill
/// ```
pub fn parse_mapping(text: &str) -> std::collections::HashMap<String, LayerRole> {
    let mut map = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, role)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim().to_string();
        let role = role.trim().to_ascii_lowercase();
        let parsed = if let Some(idx) = role.strip_prefix("copper:") {
            let idx = idx.trim();
            if idx == "bottom" {
                LayerRole::Copper {
                    index: usize::MAX,
                    name: name.clone(),
                }
            } else if let Ok(k) = idx.parse::<usize>() {
                LayerRole::Copper {
                    index: k,
                    name: name.clone(),
                }
            } else {
                continue;
            }
        } else {
            match role.as_str() {
                "drill" => LayerRole::Drill,
                "outline" => LayerRole::Outline,
                "ignore" => LayerRole::Ignored,
                _ => continue,
            }
        };
        map.insert(name, parsed);
    }
    map
}

/// One copper-layer token from Altium's Layer Pairs Export (`.LDP`) file.
///
/// Altium writes the conventional plot extensions rather than numeric X2
/// positions: `gtl,g1,g2,gbl` means physical layers 1 through 4. Keeping the
/// tokens typed lets the caller resolve `gbl` only after it has seen the whole
/// declaration, instead of guessing the board depth from a filename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LdpLayer {
    Top,
    Inner(u32),
    Bottom,
}

/// What one `.LDP` row says about a drill file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LdpDrillRole {
    /// `Some(true)` for a plated set, `Some(false)` for non-plated, and `None`
    /// when the set name did not state either.
    pub plated: Option<bool>,
    /// Ordered copper layers reached by the drill set.
    pub layers: Vec<LdpLayer>,
}

/// Parse Altium's `Layer Pairs Export File` (`.LDP`).
///
/// Each useful row is a pipe-delimited record such as:
///
/// ```text
/// LayersSetName=Top_Bot_Plated_Thru_Holes|DrillFile=board-PTH.txt|DrillLayers=gtl,g1,g2,gbl
/// ```
///
/// The returned key is a lower-cased basename because Altium commonly writes
/// the manifest name in lower case while the ZIP member itself preserves mixed
/// case. Incomplete or unfamiliar rows are not promoted to authority.
pub fn parse_ldp(text: &str) -> Vec<(String, LdpDrillRole)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut set_name: Option<&str> = None;
        let mut drill_file: Option<&str> = None;
        let mut drill_layers: Option<&str> = None;
        for field in line.split('|') {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            match key.trim().to_ascii_lowercase().as_str() {
                "layerssetname" => set_name = Some(value.trim()),
                "drillfile" => drill_file = Some(value.trim()),
                "drilllayers" => drill_layers = Some(value.trim()),
                _ => {}
            }
        }
        let (Some(file), Some(layer_text)) = (drill_file, drill_layers) else {
            continue;
        };
        let file = file.rsplit(['/', '\\']).next().unwrap_or(file).trim();
        if file.is_empty() {
            continue;
        }
        let mut parsed_layers = Vec::new();
        let mut valid = true;
        for token in layer_text.split(',').map(|s| s.trim().to_ascii_lowercase()) {
            let layer = match token.as_str() {
                "gtl" | "top" => Some(LdpLayer::Top),
                "gbl" | "bottom" | "bot" => Some(LdpLayer::Bottom),
                _ => {
                    let number = token
                        .strip_prefix("gp")
                        .or_else(|| token.strip_prefix('g'))
                        .and_then(|s| s.strip_suffix('l').or(Some(s)))
                        .and_then(|s| s.parse::<u32>().ok());
                    number.filter(|n| *n >= 1).map(LdpLayer::Inner)
                }
            };
            match layer {
                Some(layer) => parsed_layers.push(layer),
                None => {
                    valid = false;
                    break;
                }
            }
        }
        if !valid || parsed_layers.len() < 2 {
            continue;
        }
        let compact_name: String = set_name
            .unwrap_or("")
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        let plated = if compact_name.contains("nonplated") {
            Some(false)
        } else if compact_name.contains("plated") {
            Some(true)
        } else {
            None
        };
        out.push((
            file.to_ascii_lowercase(),
            LdpDrillRole {
                plated,
                layers: parsed_layers,
            },
        ));
    }
    out
}

/// A layer role declared by an Altium Gerber Extension Report (`.EXTREP`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtRepRole {
    Copper { index: usize },
    Drill,
    Outline,
    Ignored,
}

/// Parsed `.EXTREP` authority. An extension is usable only when every row for
/// it agrees; reports that assign the same extension to several roles (for
/// example Altium's named-output mode where every film is `.gbr`) retain the
/// extension in `contested` and deliberately supply no role for it.
#[derive(Debug, Clone, Default)]
pub struct ExtRepMetadata {
    pub roles: std::collections::HashMap<String, ExtRepRole>,
    pub contested: std::collections::BTreeSet<String>,
}

/// Parse the data rows of an Altium Gerber Extension Report (`.EXTREP`).
pub fn parse_extrep(text: &str) -> ExtRepMetadata {
    let mut out = ExtRepMetadata::default();
    for line in text.lines().map(str::trim) {
        if !line.starts_with('.') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let Some(extension) = fields.next() else {
            continue;
        };
        let extension = extension
            .trim_start_matches('.')
            .trim()
            .to_ascii_lowercase();
        if extension.is_empty()
            || !extension
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            continue;
        }
        let description = fields.collect::<Vec<_>>().join(" ").to_ascii_lowercase();
        let role = if description.contains("top layer") {
            Some(ExtRepRole::Copper { index: 0 })
        } else if description.contains("bottom layer") {
            Some(ExtRepRole::Copper { index: usize::MAX })
        } else if description.contains("mid-layer") || description.contains("mid layer") {
            let number: String = description
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit())
                .collect();
            number
                .parse::<usize>()
                .ok()
                .filter(|n| *n >= 1)
                .map(|index| ExtRepRole::Copper { index })
        } else if description.contains("profile")
            || description == "board"
            || description.contains("outline")
        {
            Some(ExtRepRole::Outline)
        } else if description.contains("drill") {
            Some(ExtRepRole::Drill)
        } else if description.contains("overlay")
            || description.contains("legend")
            || description.contains("paste")
            || description.contains("solder")
            || description.contains("mask")
            || description.contains("mechanical")
        {
            Some(ExtRepRole::Ignored)
        } else {
            None
        };
        let Some(role) = role else {
            continue;
        };
        if out.contested.contains(&extension) {
            continue;
        }
        match out.roles.get(&extension) {
            Some(existing) if *existing != role => {
                out.roles.remove(&extension);
                out.contested.insert(extension);
            }
            _ => {
                out.roles.insert(extension, role);
            }
        }
    }
    out
}

/// What a `.gbrjob` job file says one of its files is.
///
/// The job file is the exporter's own manifest (Ucamco Gerber Job File spec;
/// KiCad and Altium both write one): `FilesAttributes[]` lists every film with
/// its `Path` and `FileFunction`, including the copper films' PHYSICAL layer
/// number (`Copper,L3,Inr`). That is the authoritative answer to the two
/// questions filename inference can only guess at: which files are copper, and
/// where each sits in the stack. Filename inference stays as the fallback for
/// jobs that ship no `.gbrjob`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GbrJobRole {
    /// A copper film with its declared layer number and side.
    ///
    /// The NUMBER is not blindly trusted as a physical stack position: real
    /// exporters get it wrong (KiCad 9 writes its internal layer IDs for
    /// inner films, so a four-layer board's manifest reads L1, L5, L7, L4).
    /// What holds even then is the ORDER: the side tags are correct and the
    /// inner numbers increase with depth. The caller therefore ranks films by
    /// (side, number) and treats the numbers as physical only when they are
    /// contiguous `1..=n`.
    Copper { layer: u32, side: GbrJobSide },
    /// A drill/rout file, with the manifest's own plating declaration:
    /// `Plated,...` is `true`, `NonPlated,...` is `false`. Collapsing both to
    /// one role would throw the declaration away and leave plating to the
    /// filename inference, which can promote a manifest-declared mechanical
    /// file to plated (fabricating stitches) merely because a sibling is
    /// named NPTH.
    Drill { plated: bool },
    /// The board outline (`Profile`).
    Outline,
    /// Recognised and electrically irrelevant (mask, silk, paste, ...).
    Ignored,
}

/// The side tag of a `Copper,L<n>,<side>` manifest entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GbrJobSide {
    Top,
    /// `Inr`, or a missing/unrecognised side field.
    Inner,
    Bottom,
}

/// Parse a `.gbrjob` file's `FilesAttributes` into basename -> role.
///
/// Only the fields the classifier needs are read. A malformed job file (or one
/// with no `FilesAttributes`) yields an empty map, which callers treat as "no
/// job file": the filename fallback then classifies everything, so a broken
/// manifest degrades to today's behavior instead of dropping the job.
pub fn parse_gbrjob(text: &str) -> std::collections::HashMap<String, GbrJobRole> {
    let mut out = std::collections::HashMap::new();
    // Entries are keyed by basename (matching how the job's files are
    // collected), so two manifest paths that differ only by directory can
    // collide. If their roles DISAGREE, the name identifies neither file and
    // is dropped entirely, the same discipline the layer-name tokens follow:
    // applying either role to both files would reconstruct artwork as copper
    // or vanish real copper, on nothing better than map insertion order.
    let mut contested = std::collections::HashSet::new();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return out;
    };
    let Some(files) = v.get("FilesAttributes").and_then(|f| f.as_array()) else {
        return out;
    };
    for f in files {
        let Some(path) = f.get("Path").and_then(|p| p.as_str()) else {
            continue;
        };
        // Paths may carry sub-directories; the classifier keys by basename,
        // matching how the job's files were collected.
        let base = path.rsplit(['/', '\\']).next().unwrap_or(path).to_string();
        let Some(func) = f.get("FileFunction").and_then(|p| p.as_str()) else {
            continue;
        };
        let mut fields = func.split(',').map(|s| s.trim());
        let head = fields.next().unwrap_or("").to_ascii_uppercase();
        let role = match head.as_str() {
            "COPPER" => {
                // `Copper,L<n>,<side>`: the declared layer number + side.
                let Some(n) = fields.next().and_then(|l| {
                    let digits: String = l
                        .strip_prefix(['L', 'l'])?
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect();
                    digits.parse::<u32>().ok()
                }) else {
                    continue;
                };
                if n < 1 {
                    continue;
                }
                let side = match fields.next().unwrap_or("").to_ascii_uppercase().as_str() {
                    "TOP" => GbrJobSide::Top,
                    "BOT" | "BOTTOM" => GbrJobSide::Bottom,
                    _ => GbrJobSide::Inner,
                };
                GbrJobRole::Copper { layer: n, side }
            }
            "PLATED" => GbrJobRole::Drill { plated: true },
            "NONPLATED" => GbrJobRole::Drill { plated: false },
            "PROFILE" => GbrJobRole::Outline,
            // Everything else the job names is by definition not copper and
            // not drilling: mask, paste, legend, drawings. Marking it Ignored
            // stops the bare-role-name fallback from reading, say, a
            // `Top.gbr` assembly drawing as top copper.
            _ => GbrJobRole::Ignored,
        };
        if contested.contains(&base) {
            continue;
        }
        match out.get(&base) {
            Some(existing) if *existing != role => {
                out.remove(&base);
                contested.insert(base);
            }
            _ => {
                out.insert(base, role);
            }
        }
    }
    out
}

fn top_label(orig: &str) -> String {
    if orig.is_empty() {
        "F.Cu".to_string()
    } else {
        orig.to_string()
    }
}
fn bottom_label(orig: &str) -> String {
    if orig.is_empty() {
        "B.Cu".to_string()
    } else {
        orig.to_string()
    }
}

/// Protel inner-copper extension: `.g1l`/`.g2l`…, `.gp1`/`.gp2`…, `.g1`/`.g2`…
/// Returns the 1-based inner position mapped to a provisional stack index.
fn protel_inner_index(n: &Name) -> Option<usize> {
    let ext = n.ext.as_deref()?;
    // gNl  (g1l, g2l, ... g14l)
    if let Some(rest) = ext.strip_prefix('g') {
        if let Some(digits) = rest.strip_suffix('l') {
            if let Ok(k) = digits.parse::<usize>() {
                if k >= 1 {
                    return Some(k); // provisional: inner k sits at stack pos k
                }
            }
        }
        // gpN (internal plane N)
        if let Some(digits) = rest.strip_prefix('p') {
            if let Ok(k) = digits.parse::<usize>() {
                return Some(k);
            }
        }
        // gN (g1, g2 ... bare)
        if let Ok(k) = rest.parse::<usize>() {
            if k >= 1 {
                return Some(k);
            }
        }
    }
    None
}

/// KiCad inner-copper name: `*-In1_Cu.gbr`, `*-In2_Cu.gbr`, … or
/// `inner1`, `signal2`, `Copper_Signal_1`, etc.
fn kicad_inner_index(n: &Name) -> Option<usize> {
    /// The layer index that butts straight up against a marker.
    fn index_at(tail: &str) -> Option<usize> {
        let d: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        d.parse().ok()
    }
    /// The layer index one separator after a marker. Altium 24 plots its inner
    /// copper as `<board>_Copper_Signal_1.gbr`; requiring the digit to butt
    /// straight up against `signal` matched `signal1` and missed `signal_1`, so
    /// the inner layers of every Altium four-layer job fell through to `Unknown`
    /// and their copper was silently discarded. A single `-`, `_` or space is the
    /// separator every exporter uses; `.` is not, because that is the extension
    /// boundary.
    fn index_after_sep(tail: &str) -> Option<usize> {
        match tail.as_bytes().first() {
            Some(b'-' | b'_' | b' ') => index_at(&tail[1..]),
            _ => None,
        }
    }
    // A copper layer, and for the SEPARATED form a plausible stack position:
    // `board-Inner 2024-05-01.gbr` must not report inner layer 2024. The bound is
    // not applied to a butt-up digit, where the number always IS a stack position:
    // bounding it there discards the copper of a >32-layer stackup outright, which
    // is worse than handing `assign_inner_indices` a number to re-rank.
    // `copper` does not contain the substring `cu`, so testing only for `cu` left
    // `<board>_Copper_Layer_1.gbr` classifying Unknown and its copper discarded:
    // the same silent loss this function was fixed for on `_Copper_Signal_1`, one
    // marker over. The top and bottom rules already use both spellings.
    let has_copper = n.has_cu_token() || n.has("copper");
    let accept = |marker: &str, k: usize, bounded: bool| -> Option<usize> {
        (k >= 1
            && (!bounded || k <= 32)
            && (has_copper
                || marker == "signal"
                || marker == "inner"
                // `plane` alone names no material: `Peelable Plane 1.gbr` and
                // `Carbon Plane 1.gbr` are films, not copper. It counts only when the
                // name also states a copper ROLE, which is what an internal plane
                // does. `paste` staying Ignored was not evidence the marker was
                // bounded; it survived only because `paste` is enumerated.
                || (marker == "plane"
                    && (n.has("internal")
                        || n.has("inner")
                        || n.has("gnd")
                        || n.has("ground")
                        || n.has("pwr")
                        || n.has("power")
                        || n.has("vcc")
                        || n.has("vdd")))))
        .then_some(k)
    };
    // Scan EVERY occurrence of a marker, not just the first: a project name that
    // itself contains one (e.g. "mainboard-In2_Cu", "arduino-In1_Cu") puts a
    // non-digit-tailed "in" ahead of the real `In<k>_Cu` token. `find` stopped at
    // that first match and gave up, silently dropping the inner copper layer.
    //
    // A BUTT-UP digit wins over a separated one, everywhere, before any separated
    // match is considered. `Main_2-In1_Cu.gbr` otherwise reads its own project
    // name: the "in" inside "main" is followed by `_2`, which the separator form
    // accepts, and the film came back as inner layer 2 instead of 1. The separated
    // form additionally requires the marker to start a token, so an "in" buried in
    // a word cannot claim a number that follows the word.
    for pass in 0..2 {
        // `plane` is here because an INTERNAL PLANE is copper and is exactly the film
        // drawn negatively: `Internal Plane 1.gbr` and
        // `<board>_Internal_Plane_1.gbr` both classified Unknown, so their copper was
        // discarded outright, on the one construct this reader was extended for.
        for marker in ["in", "inner", "signal", "layer", "plane"] {
            for (pos, _) in n.full.match_indices(marker) {
                let tail = &n.full[pos + marker.len()..];
                // A marker BURIED in a word may not claim a layer index, in
                // either form. `main2-In1_Cu.gbr` otherwise read its own
                // project name: the "in" inside "main" butts straight against
                // `2`, so the butt-up pass took it and reported inner layer 2.
                // Worse than a wrong label, since two films of one job can then
                // collide on an index and `assign_inner_indices` densifies them
                // into the wrong stack order, which is what the blind-via layer
                // pair resolution keys on.
                // `in` is the one marker short enough to hide inside ordinary words:
                // `main`, `pin`, `origin`, `austin`. `main2-In1_Cu.gbr` read its own
                // project name that way, the "in" inside "main" butting straight
                // against `2`, and reported inner layer 2. Worse than a wrong label,
                // since two films of one job then collide on an index and
                // `assign_inner_indices` densifies them into the wrong stack order,
                // which is what blind-via layer-pair resolution keys on. So `in` has
                // to start a token in EITHER form.
                //
                // The longer markers must NOT carry that requirement in the butt-up
                // form. They glue to a preceding word only in names where the digit
                // IS the layer index, and demanding a token start there discarded
                // `board-InnerLayer1_Cu.gbr`, `board_MidLayer1_Cu.gbr` and
                // `board-CopperLayer2_Cu.gbr`, which classified as copper before:
                // the same silent loss this function has been fixed for twice
                // already. The separated form keeps the requirement for all of them,
                // that being where a stray number after a word is the real hazard.
                let at_token_start = pos == 0 || !n.full.as_bytes()[pos - 1].is_ascii_alphabetic();
                // `plane` needs a token start in BOTH forms, like `in`: without it
                // any project ending "...plane<digit>" had its TOP film reindexed as
                // inner 1 (`Backplane1-Top.gbr`, `Airplane1-Top.gbr`), two films could
                // then collide on an index, and `assign_inner_indices` densified them
                // into the wrong stack order, which is what blind-via span resolution
                // reads.
                let k = if pass == 0 {
                    (at_token_start || !matches!(marker, "in" | "plane"))
                        .then(|| index_at(tail))
                        .flatten()
                } else if at_token_start {
                    index_after_sep(tail)
                } else {
                    None
                };
                if let Some(k) = k.and_then(|k| accept(marker, k, pass == 1)) {
                    return Some(k);
                }
            }
        }
    }
    None
}

/// A bare stack-position film name: `L2-GND.gbr`, `l3.gbr`, `layer4.gbr`.
/// `L1` is the top layer by convention, so `L<k>` is inner index `k - 1`.
/// Returns `None` for `L1` (the top rule already claimed it) and for anything
/// past a plausible stack depth, so a project called `L500` cannot invent 500
/// layers of copper.
fn bare_stack_index(n: &Name) -> Option<usize> {
    for marker in ["l", "layer"] {
        for (pos, _) in n.full.match_indices(marker) {
            // Must start the name or follow a separator, else the `l` inside
            // `flrp` or `plain` matches.
            if pos > 0 && n.full.as_bytes()[pos - 1].is_ascii_alphanumeric() {
                continue;
            }
            let tail = &n.full[pos + marker.len()..];
            let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                continue;
            }
            if let Ok(k) = digits.parse::<usize>() {
                if (2..=32).contains(&k) {
                    return Some(k - 1);
                }
            }
        }
    }
    None
}

/// Once every copper file is classified, resolve the provisional indices into a
/// dense top-to-bottom ordering. Top stays 0; bottom (the `usize::MAX`
/// sentinel) becomes the last index; inner layers keep their relative order.
///
/// Input/output: a list of `(role, original_index_into_caller_vec)` for the
/// copper files only. Returns the same list with `Copper.index` rewritten to a
/// dense `0..n` stack position.
pub fn assign_inner_indices(mut coppers: Vec<(LayerRole, usize)>) -> Vec<(LayerRole, usize)> {
    let n = coppers.len();
    if n == 0 {
        return coppers;
    }
    // Stable sort by provisional index (top=0, inner=1..,, bottom=MAX last).
    coppers.sort_by_key(|(role, _)| match role {
        LayerRole::Copper { index, .. } => *index,
        _ => usize::MAX,
    });
    for (dense, (role, _)) in coppers.iter_mut().enumerate() {
        if let LayerRole::Copper { index, .. } = role {
            *index = dense;
        }
    }
    coppers
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn role(name: &str) -> LayerRole {
        classify(&PathBuf::from(name))
    }

    #[test]
    fn kicad_names() {
        assert!(matches!(
            role("board-F_Cu.gbr"),
            LayerRole::Copper { index: 0, .. }
        ));
        assert!(matches!(
            role("board-B_Cu.gbr"),
            LayerRole::Copper {
                index: usize::MAX,
                ..
            }
        ));
        assert!(matches!(
            role("board-In1_Cu.gbr"),
            LayerRole::Copper { index: 1, .. }
        ));
        assert!(matches!(
            role("board-In2_Cu.gbr"),
            LayerRole::Copper { index: 2, .. }
        ));
        assert_eq!(role("board-F_Mask.gbr"), LayerRole::Ignored);
        assert_eq!(role("board-F_Silkscreen.gbr"), LayerRole::Ignored);
        assert_eq!(role("board-F_Paste.gbr"), LayerRole::Ignored);
    }

    #[test]
    fn altium_inner_signal_films_are_copper() {
        // Altium 24 plots inner copper as `<board>_Copper_Signal_1.gbr`, with a
        // separator between the word and the layer index. Requiring the digit
        // to butt straight up against `signal` matched none of them, so both
        // inner films of every Altium four-layer job classified Unknown and
        // their copper never reached the reconstruction: a real four-layer
        // board came back as two layers.
        assert!(matches!(
            role("ARDEP_Mainboard_Copper_Signal_1.gbr"),
            LayerRole::Copper { index: 1, .. }
        ));
        assert!(matches!(
            role("ARDEP_Mainboard_Copper_Signal_2.gbr"),
            LayerRole::Copper { index: 2, .. }
        ));
        assert!(matches!(
            role("ARDEP_Mainboard_Copper_Signal_Top.gbr"),
            LayerRole::Copper { index: 0, .. }
        ));
        assert!(matches!(
            role("ARDEP_Mainboard_Copper_Signal_Bot.gbr"),
            LayerRole::Copper {
                index: usize::MAX,
                ..
            }
        ));
        // The separator is one character of `-`, `_` or space, never a run and
        // never a dot: `signal.1` is a stem/extension boundary, not an index.
        assert!(matches!(
            role("board Inner 3 Cu.gbr"),
            LayerRole::Copper { index: 3, .. }
        ));
        assert!(matches!(
            role("board-signal-2.gbr"),
            LayerRole::Copper { index: 2, .. }
        ));
        // And only a plotted FILM can be copper. The signal/inner markers need
        // no `cu` token, so without an extension gate a placement CSV or a
        // drawing whose name happens to carry one became a copper layer; the
        // directory scan claims copper before it looks for placement data, so
        // the CSV was swallowed as an empty copper film and no component bound.
        assert!(!role("signal_1.csv").is_copper());
        assert!(!role("inner_2.pdf").is_copper());
        assert!(!role("board-Inner1.xlsx").is_copper());
        // A butt-up digit wins over a separated one, everywhere, and a marker
        // buried inside a word cannot claim a number that follows the word.
        // `Main_2-In1_Cu.gbr` otherwise read its own project name: the "in" inside
        // "main" is followed by `_2`, so the film came back as inner layer 2.
        assert!(matches!(
            role("Main_2-In1_Cu.gbr"),
            LayerRole::Copper { index: 1, .. }
        ));
        assert!(matches!(
            role("Pin_3-In1_Cu.gbr"),
            LayerRole::Copper { index: 1, .. }
        ));
        assert!(matches!(
            role("Origin_4-In2_Cu.gbr"),
            LayerRole::Copper { index: 2, .. }
        ));
        assert!(matches!(
            role("Austin 1-In2_Cu.gbr"),
            LayerRole::Copper { index: 2, .. }
        ));
        assert!(!role("board-Inner 2024-05-01.gbr").is_copper());
        // The bound is on the SEPARATED form only. A butt-up digit always IS a
        // stack position, and bounding it there discarded the copper of a
        // >32-layer stackup outright.
        assert!(matches!(
            role("board-in40_cu.gbr"),
            LayerRole::Copper { index: 40, .. }
        ));
        // "copper" spelled out is the copper token too. `cu` is not a substring of
        // it, so a name using the long spelling with the `layer` or `in` marker
        // classified Unknown and its copper was discarded.
        assert!(matches!(
            role("ARDEP_Mainboard_Copper_Layer_1.gbr"),
            LayerRole::Copper { index: 1, .. }
        ));
        assert!(matches!(
            role("board_copper_layer_2.gbr"),
            LayerRole::Copper { index: 2, .. }
        ));
        // A marker buried in a word may not claim an index in EITHER form. The
        // butt-up pass had no token-start gate, so `main2-In1_Cu.gbr` read the "in"
        // inside "main" against the `2` right after it. Two films of one job then
        // collide on an index and `assign_inner_indices` densifies them into the
        // wrong stack order, which is what blind-via layer-pair resolution reads.
        assert!(matches!(
            role("main2-In1_Cu.gbr"),
            LayerRole::Copper { index: 1, .. }
        ));
        assert!(matches!(
            role("origin3-In1_Cu.gbr"),
            LayerRole::Copper { index: 1, .. }
        ));
        assert!(matches!(
            role("pin2-In10_Cu.gbr"),
            LayerRole::Copper { index: 10, .. }
        ));
        // The longer markers keep the butt-up form ungated: they glue to a
        // preceding word only where the digit IS the layer index, and requiring a
        // token start there discarded copper these names classified before.
        for (name, want) in [
            ("board-InnerLayer1_Cu.gbr", 1usize),
            ("board_MidLayer1_Cu.gbr", 1),
            ("board-CopperLayer2_Cu.gbr", 2),
            ("board-CopperInner1.gbr", 1),
        ] {
            match role(name) {
                LayerRole::Copper { index, .. } => assert_eq!(index, want, "{name}"),
                other => panic!("{name} should be copper, got {other:?}"),
            }
        }
        // A name-based copper rule must not read a MECHANICAL or DOCUMENTATION film
        // as copper. `cu` was a substring test, so any project name containing
        // `circuit`, `accumulator`, `vcut`, `document` or `cube` supplied the copper
        // token, and a film with `Layer <n>` in its name became inner copper. That
        // puts outline and dimension lines on a layer every drill barrel stitches,
        // which is a large false merge, and it inflates the layer count blind-via
        // span resolution reads.
        for name in [
            "MyCircuit_Mechanical_Layer_1.gbr",
            "Documentation Layer 1.gbr",
            "Accumulator_Mechanical_Layer_3.gbr",
            "board-Vcut_Layer_2.gbr",
            "MyCircuit_Component_Layer_1.gbr",
            "MyCircuit-Profile_Layer_1.gbr",
            "MyCircuit_Top_Layer_Drawing.gbr",
        ] {
            assert!(!role(name).is_copper(), "{name} is not a copper film");
        }
        // `cu` is the copper token as a whole word, ending one, OR abutting a side or
        // stack token on its RIGHT. `-CuTop.gbr` / `-CuBottom.gbr` is a real
        // convention: thirteen corpus zips and a loose directory ship it, and
        // requiring `cu` to end a word turned both films of those boards Unknown,
        // which is dropped with no note. `cutop_named_corpus_boards_still_reconstruct`
        // reads two of those boards end to end.
        assert!(role("TopCu.gbr").is_copper());
        assert!(role("board-F_Cu.gbr").is_copper());
        assert!(matches!(
            role("corne-cherry-CuTop.gbr"),
            LayerRole::Copper { index: 0, .. }
        ));
        assert!(matches!(
            role("corne-cherry-CuBottom.gbr"),
            LayerRole::Copper {
                index: usize::MAX,
                ..
            }
        ));
        assert!(matches!(
            role("RoyalBlue54L-NFC-Antenna-CuTop.gbr"),
            LayerRole::Copper { index: 0, .. }
        ));
        for name in [
            "board-CuTopography.gbr",
            "board-CuTopcoat.gbr",
            "board-CuBottomless.gbr",
            "board-CuFrontier.gbr",
            "board-CuMidpoint.gbr",
            "board-CuInvention.gbr",
            "board-CuLayer.gbr",
        ] {
            assert!(
                !role(name).is_copper(),
                "a word merely beginning with a copper role is not a copper token: {name}"
            );
        }
        // An EXPLICIT copper suffix outranks the non-copper word sweep, which returns
        // Ignored on a raw substring and so discarded the copper of any project whose
        // NAME carried one of its words. Mechanical keyboards are one of the largest
        // open-hardware PCB categories and this corpus is full of them.
        for (name, want) in [
            ("mechanical-keyboard-F_Cu.gbr", 0usize),
            ("MechanicalKeyboard-B_Cu.gbr", usize::MAX),
            ("mechanical_keyboard-In1_Cu.gbr", 1),
            ("Documentation-F_Cu.gbr", 0),
            ("fabricator-F_Cu.gbr", 0),
        ] {
            match role(name) {
                LayerRole::Copper { index, .. } => assert_eq!(index, want, "{name}"),
                other => panic!("{name} should be copper, got {other:?}"),
            }
        }
        for name in [
            "Pin2_Cu-Mechanical_1.gbr",
            "Spin2_Cu-Documentation_1.gbr",
            "project-in2_cu-mechanical.gbr",
        ] {
            assert!(
                !role(name).is_copper(),
                "an in<n>_cu substring in the project name must not override the film role: {name}"
            );
        }
        // `.GM1` is Altium's board-outline film and `Mechanical_1` is the layer it is
        // plotted from, so the word sweep claimed it and the outline was lost.
        assert_eq!(role("board-Mechanical_1.GM1"), LayerRole::Outline);
        // `plane` counts only where the name states a copper ROLE, and only at a token
        // start. Otherwise any project ending "...plane<digit>" had its TOP film
        // reindexed as inner 1, which corrupts the stack order blind-via span
        // resolution reads.
        for name in ["Peelable Plane 1.gbr", "Carbon Plane 1.gbr"] {
            assert!(!role(name).is_copper(), "{name} is a film, not copper");
        }
        for name in [
            "Backplane1-Top.gbr",
            "Airplane1-Top.gbr",
            "Mainplane1-Top.gbr",
        ] {
            assert!(
                matches!(role(name), LayerRole::Copper { index: 0, .. }),
                "{name} is a TOP film, not inner 1"
            );
        }
        assert!(matches!(
            role("Backplane2-Bottom.gbr"),
            LayerRole::Copper {
                index: usize::MAX,
                ..
            }
        ));
        assert!(matches!(
            role("GND Plane 2.gbr"),
            LayerRole::Copper { index: 2, .. }
        ));
        // A drill MAP is a drawing. It carries the `drl` token, matched a name-based
        // drill rule, and reading the PDF as text failed the whole extraction.
        assert!(!matches!(
            role("corne-cherry-NPTH-drl_map.pdf"),
            LayerRole::Drill
        ));
        // And a name shorter than a marker must not panic. `hay.len() - needle.len()`
        // under-flowed into `0..1`, and indexing then panicked, so a fab folder with a
        // one-character filename aborted extraction instead of returning an error.
        for name in ["a", "1", "ab", "abc", "x.g", ""] {
            let _ = role(name);
        }
        // An INTERNAL PLANE is copper, and is exactly the film drawn negatively.
        // Both of these classified Unknown, so their copper was discarded outright.
        for (name, want) in [
            ("Internal Plane 1.gbr", 1usize),
            ("ARDEP_Mainboard_Internal_Plane_1.gbr", 1),
            ("Internal Plane 2.gbr", 2),
        ] {
            match role(name) {
                LayerRole::Copper { index, .. } => assert_eq!(index, want, "{name}"),
                other => panic!("{name} should be copper, got {other:?}"),
            }
        }
        assert_eq!(role("board-Paste_Plane.gbr"), LayerRole::Ignored);

        // And no name-based copper rule may claim a file that is plainly not a
        // film. Altium's per-side pick-and-place and its per-layer prints both
        // carry the words the top and bottom rules key on, and the directory scan
        // claims copper before it looks for placement data, so a matched CSV was
        // swallowed as an empty copper film and no component bound.
        for name in [
            "Pick Place for ARDEP - Top Layer.csv",
            "ARDEP_Mainboard_Copper_Top.csv",
            "ARDEP_Mainboard_Copper_Top.pdf",
            "ARDEP_Mainboard-Top Layer.pdf",
            "top layer bom.xlsx",
            "ARDEP_Mainboard_Copper_Bottom.csv",
        ] {
            assert!(!role(name).is_copper(), "{name} is not a copper film");
        }
    }

    #[test]
    fn inner_copper_survives_a_project_name_containing_the_marker() {
        // Round-27: a project name whose text contains "in" (or inner/signal/
        // layer) put a non-digit-tailed marker ahead of the real `In<k>_Cu`
        // token. `find` stopped at that first match and dropped the inner copper
        // layer to Unknown, silently vanishing it from reconstruction. Every
        // occurrence must be scanned so the genuine layer index is recovered.
        assert!(
            matches!(
                role("mainboard-In1_Cu.gbr"),
                LayerRole::Copper { index: 1, .. }
            ),
            "the 'in' inside 'mainboard' must not shadow the real In1 token"
        );
        assert!(matches!(
            role("mainboard-In2_Cu.gbr"),
            LayerRole::Copper { index: 2, .. }
        ));
        assert!(matches!(
            role("arduino-In1_Cu.gbr"),
            LayerRole::Copper { index: 1, .. }
        ));
        // A name with the marker but no real inner token stays non-copper.
        assert_ne!(
            role("arduino-F_Silkscreen.gbr"),
            LayerRole::Copper {
                index: 1,
                name: String::new()
            }
        );
        assert_eq!(role("board-Edge_Cuts.gbr"), LayerRole::Outline);
        assert_eq!(role("board-PTH.drl"), LayerRole::Drill);
        assert_eq!(role("board-NPTH.drl"), LayerRole::Drill);
    }

    #[test]
    fn protel_extensions() {
        assert!(matches!(
            role("design.GTL"),
            LayerRole::Copper { index: 0, .. }
        ));
        assert!(matches!(
            role("design.gbl"),
            LayerRole::Copper {
                index: usize::MAX,
                ..
            }
        ));
        assert!(matches!(
            role("design.G1L"),
            LayerRole::Copper { index: 1, .. }
        ));
        assert_eq!(role("design.GTS"), LayerRole::Ignored);
        assert_eq!(role("design.GTO"), LayerRole::Ignored);
        assert_eq!(role("design.GKO"), LayerRole::Outline);
        assert_eq!(role("design.TXT"), LayerRole::Ignored); // bare .txt = readme
        assert_eq!(role("design-drill.txt"), LayerRole::Drill);
        assert_eq!(role("design.drl"), LayerRole::Drill);
    }

    #[test]
    fn generic_words() {
        assert!(matches!(
            role("TopLayer.gbr"),
            LayerRole::Copper { index: 0, .. }
        ));
        assert!(matches!(
            role("Bottom Copper.gbr"),
            LayerRole::Copper {
                index: usize::MAX,
                ..
            }
        ));
    }

    #[test]
    fn allegro_art_names() {
        assert!(matches!(
            role("top.art"),
            LayerRole::Copper { index: 0, .. }
        ));
        assert!(matches!(
            role("bottom.art"),
            LayerRole::Copper {
                index: usize::MAX,
                ..
            }
        ));
        assert!(matches!(
            role("gnd02.art"),
            LayerRole::Copper { index: 2, .. }
        ));
        assert!(matches!(
            role("pwr04.art"),
            LayerRole::Copper { index: 4, .. }
        ));
        assert_eq!(role("drill-1-6.art"), LayerRole::Drill);
        assert_eq!(role("silk_top.art"), LayerRole::Ignored);
        assert_eq!(role("solder_bot.art"), LayerRole::Ignored);
        assert_eq!(role("paste_top.art"), LayerRole::Ignored);
    }

    #[test]
    fn altium_ldp_names_drill_plating_and_physical_layers() {
        let rows = parse_ldp(
            "Layer Pairs Export File for PCB: demo.PcbDoc\n\
             LayersSetName=Top_Bot_Plated_Thru_Holes|DrillFile=FAB/Board-PTH.TXT|DrillLayers=gtl,g1,g2,gbl\n\
             LayersSetName=Top_Bot_NonPlated_Thru_Holes|DrillFile=Board-NPTH.txt|DrillLayers=gtl,g1,g2,gbl\n\
             LayersSetName=broken|DrillFile=bad.txt|DrillLayers=gtl,unknown\n",
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "board-pth.txt");
        assert_eq!(rows[0].1.plated, Some(true));
        assert_eq!(
            rows[0].1.layers,
            vec![
                LdpLayer::Top,
                LdpLayer::Inner(1),
                LdpLayer::Inner(2),
                LdpLayer::Bottom
            ]
        );
        assert_eq!(rows[1].0, "board-npth.txt");
        assert_eq!(rows[1].1.plated, Some(false));
    }

    #[test]
    fn extrep_uses_unique_extensions_and_contests_reused_ones() {
        let report = parse_extrep(
            "Layer Extension     Layer Description\n\
             .GTL                Top Layer\n\
             .G1                 Mid-Layer 1\n\
             .GBL                Bottom Layer\n\
             .GKO                Profile\n\
             .GBR                Top Layer\n\
             .gbr                Top Overlay\n",
        );
        assert_eq!(
            report.roles.get("gtl"),
            Some(&ExtRepRole::Copper { index: 0 })
        );
        assert_eq!(
            report.roles.get("g1"),
            Some(&ExtRepRole::Copper { index: 1 })
        );
        assert_eq!(report.roles.get("gko"), Some(&ExtRepRole::Outline));
        assert!(!report.roles.contains_key("gbr"));
        assert!(report.contested.contains("gbr"));
    }

    #[test]
    fn gbrjob_files_attributes_parse_to_roles() {
        let text = r#"{
  "Header": {"GenerationSoftware": {"Vendor": "KiCad"}},
  "FilesAttributes": [
    {"Path": "brd-F_Cu.gbr", "FileFunction": "Copper,L1,Top", "FilePolarity": "Positive"},
    {"Path": "sub/inner_gnd.gbr", "FileFunction": "Copper,L2,Inr"},
    {"Path": "brd-B_Cu.gbr", "FileFunction": "Copper,L4,Bot"},
    {"Path": "brd-PTH.drl", "FileFunction": "Plated,1,4,PTH"},
    {"Path": "brd-NPTH.drl", "FileFunction": "NonPlated,1,4,NPTH"},
    {"Path": "brd-Edge_Cuts.gbr", "FileFunction": "Profile,NP"},
    {"Path": "brd-F_Mask.gbr", "FileFunction": "SolderMask,Top"}
  ]
}"#;
        let m = parse_gbrjob(text);
        assert_eq!(
            m.get("brd-F_Cu.gbr"),
            Some(&GbrJobRole::Copper {
                layer: 1,
                side: GbrJobSide::Top
            })
        );
        // Sub-directory paths key by basename, like the file walk does.
        assert_eq!(
            m.get("inner_gnd.gbr"),
            Some(&GbrJobRole::Copper {
                layer: 2,
                side: GbrJobSide::Inner
            })
        );
        assert_eq!(
            m.get("brd-B_Cu.gbr"),
            Some(&GbrJobRole::Copper {
                layer: 4,
                side: GbrJobSide::Bottom
            })
        );
        assert_eq!(
            m.get("brd-PTH.drl"),
            Some(&GbrJobRole::Drill { plated: true })
        );
        assert_eq!(
            m.get("brd-NPTH.drl"),
            Some(&GbrJobRole::Drill { plated: false })
        );
        assert_eq!(m.get("brd-Edge_Cuts.gbr"), Some(&GbrJobRole::Outline));
        assert_eq!(m.get("brd-F_Mask.gbr"), Some(&GbrJobRole::Ignored));
        // Garbage degrades to an empty map, never a panic or a wrong role.
        assert!(parse_gbrjob("not json").is_empty());
        assert!(parse_gbrjob("{}").is_empty());
    }

    #[test]
    fn gbrjob_basename_collisions_with_conflicting_roles_name_neither() {
        // Entries are keyed by basename; two sub-directory paths that collide
        // with DIFFERENT roles identify nothing (applying either role to both
        // files would read artwork as copper or vanish real copper on map
        // order). Agreeing duplicates keep their shared role.
        let text = r#"{"FilesAttributes": [
    {"Path": "copper/layer.gbr", "FileFunction": "Copper,L1,Top"},
    {"Path": "documentation/layer.gbr", "FileFunction": "AssemblyDrawing,Top"},
    {"Path": "a/mask.gbr", "FileFunction": "SolderMask,Top"},
    {"Path": "b/mask.gbr", "FileFunction": "SolderMask,Top"}
  ]}"#;
        let m = parse_gbrjob(text);
        assert_eq!(
            m.get("layer.gbr"),
            None,
            "a contested basename names neither"
        );
        assert_eq!(m.get("mask.gbr"), Some(&GbrJobRole::Ignored));
    }

    #[test]
    fn mapping_file() {
        let text = "\
# explicit overrides\n\
weird_top.gbr = copper:0\n\
weird_in1.gbr = copper:1\n\
weird_bot.gbr = copper:bottom\n\
holes.txt = drill\n\
edge.gbr = outline\n";
        let m = parse_mapping(text);
        assert!(matches!(
            m.get("weird_top.gbr"),
            Some(LayerRole::Copper { index: 0, .. })
        ));
        assert!(matches!(
            m.get("weird_in1.gbr"),
            Some(LayerRole::Copper { index: 1, .. })
        ));
        assert!(matches!(
            m.get("weird_bot.gbr"),
            Some(LayerRole::Copper {
                index: usize::MAX,
                ..
            })
        ));
        assert_eq!(m.get("holes.txt"), Some(&LayerRole::Drill));
        assert_eq!(m.get("edge.gbr"), Some(&LayerRole::Outline));
    }

    #[test]
    fn dense_reorder() {
        let coppers = vec![
            (
                LayerRole::Copper {
                    index: usize::MAX,
                    name: "B".into(),
                },
                0,
            ),
            (
                LayerRole::Copper {
                    index: 0,
                    name: "F".into(),
                },
                1,
            ),
            (
                LayerRole::Copper {
                    index: 2,
                    name: "In2".into(),
                },
                2,
            ),
            (
                LayerRole::Copper {
                    index: 1,
                    name: "In1".into(),
                },
                3,
            ),
        ];
        let out = assign_inner_indices(coppers);
        // Order: F(0), In1(1), In2(2), B(3)
        let idxs: Vec<usize> = out
            .iter()
            .map(|(r, _)| match r {
                LayerRole::Copper { index, .. } => *index,
                _ => 999,
            })
            .collect();
        assert_eq!(idxs, vec![0, 1, 2, 3]);
        // The original-vec back-references follow the names.
        let names: Vec<&str> = out
            .iter()
            .map(|(r, _)| match r {
                LayerRole::Copper { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(names, vec!["F", "In1", "In2", "B"]);
    }
}
