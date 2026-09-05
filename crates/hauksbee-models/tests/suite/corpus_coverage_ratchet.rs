//! The coverage ratchet: bind rates across the board corpus may go up, never down.
//!
//! Every other test in this suite checks one entry against one datasheet. This
//! one checks the thing those entries exist for: the fraction of a real board the
//! library can actually describe. It is the only test here that would notice a
//! match rule quietly narrowing under an unrelated edit, because a narrowed rule
//! still resolves its own hand-written fixture while binding nothing on a board.
//!
//! WHAT IT MEASURES, precisely, because the number is only meaningful if that is
//! clear. It reads the `(value, footprint)` pair of every component on the board
//! straight out of the board file and asks the library to resolve it. That is
//! exactly the pair a match rule sees, so this measures THE MATCH RULES. It is
//! not the same population as `hauksbee run --report`, which counts non-ignored
//! parts after the binder has had its say and adds the engine's own fallbacks on
//! top; the numbers here are lower and are not comparable to that report's.
//! Reimplementing the extractor was the alternative and it would measure the
//! extractor's bugs as well as the library's.
//!
//! WHY THE NUMBERS LOOK LOW: because they exclude the engine's own fallbacks,
//! which is the point. A plain `10k` resistor or `100nF` capacitor has no library
//! entry and does not need one; the engine resolves it from the value string
//! itself (`r_fallback`, `c_fallback`), and those parts are the majority of any
//! board. So the corpus-wide figure here is around a quarter, while
//! `hauksbee run --report` on the same corpus says roughly three quarters of
//! non-ignored parts resolve. Both are true and they count different things.
//! This one is the number that moves when a match rule changes, which is what a
//! ratchet on this crate should watch.
//!
//! HOW THE FLOORS WERE SET: by measuring, then writing the number down, with the
//! measured value in the comment beside each so drift is visible. They are
//! floors, not targets. Raising coverage should make these pass with room, and
//! the right response is to raise the floor in the same commit.
//!
//! WHY PER-BOARD AND NOT ONE CORPUS TOTAL: an aggregate hides compensation. A
//! change that broke every Olimex rule while resolving a hundred MNT Reform
//! passives would leave a corpus total flat and this test green, which is the
//! exact failure a ratchet exists to catch. Per-board floors localise it.
//!
//! Without a corpus this SKIPS LOUDLY rather than passing quietly, and under
//! `HAUKSBEE_REQUIRE_CORPUS=1` (which CI sets) absence is a failure. A green tick
//! next to a test that verified nothing is the vacuous pass the product refuses
//! to emit for boards, and it is no better here.

use std::path::{Path, PathBuf};

use hauksbee_models::{ComponentQuery, ModelLibrary};

/// One board's floor: where it lives, and the minimum it must bind.
struct Floor {
    /// Relative path under the corpus root. Several candidates when the two
    /// corpus layouts pin different upstream revisions of the same board.
    rels: &'static [&'static str],
    /// Minimum components the library must resolve. Measured, then recorded.
    min_bound: usize,
    /// Components the board had when the floor was measured, so a floor that
    /// passes only because the parser now sees fewer parts is visible.
    total_when_measured: usize,
}

/// Measured against the corpus at the revisions pinned in corpus.toml.
const FLOORS: &[Floor] = &[
    // Olimex ESP32-EVB, the board this batch was aimed at: BC817-40, WPM2015-3,
    // 1N5822, MCP73833, the DG306 terminal blocks, the UEXT header, the microSD
    // socket, the not-assembled positions and the mounting holes all land here.
    Floor {
        rels: &["famous/olimex_esp32/HARDWARE/REV-L/ESP32-EVB_Rev_L.kicad_pcb"],
        min_bound: 60,   // measured 63
        total_when_measured: 170,
    },
    // Olimex RP2040-PICO-PC, the second board sharing that part list.
    Floor {
        rels: &[
            "famous/olimex_rp2040_pico_pc/HARDWARE/RP2040-PICO-PC hardware revision D/RP2040-PICO-PC_rev_D.kicad_pcb",
            "famous/olimex_rp2040_pico_pc/HARDWARE/RP2040-PICO-PC hardware revision C/RP2040-PICO-PC_rev_C.kicad_pcb",
        ],
        min_bound: 24,   // measured 25
        total_when_measured: 72,
    },
    // MNT Reform motherboard: the PMV50E complementary pair, the PMZ550UNE, the
    // traced ferrite beads and the IMC1210 inductor.
    Floor {
        rels: &[
            "famous/mnt_reform/reform2-motherboard30-pcb/reform2-motherboard30.kicad_pcb",
            "famous/mnt_reform/reform2-motherboard25-pcb/reform2-motherboard25.kicad_pcb",
        ],
        min_bound: 69,   // measured 72
        total_when_measured: 529,
    },
    // MNT Reform keyboard: 79 Cherry ML keyswitches, which is what the switch
    // rules were widened for.
    Floor {
        rels: &[
            "famous/mnt_reform/historic-reform1/reform-keyboard-pcb/mntcomp-keyboard.kicad_pcb",
        ],
        min_bound: 90,   // measured 94, of which 79 are Cherry ML keyswitches
        total_when_measured: 239,
    },
    // Watchy, the board the older entries were built against. Included so the
    // ratchet also guards what already worked.
    Floor {
        rels: &["famous/watchy/Watchy.kicad_pcb"],
        min_bound: 21,   // measured 22
        total_when_measured: 86,
    },
];

/// The corpus-wide floor, as a percentage of all components on every board.
///
/// Weaker than the per-board floors because it cannot localise a regression, but
/// it catches a broad narrowing that happens to miss all five named boards.
const CORPUS_WIDE_FLOOR_PCT: f64 = 22.5; // measured 23.7% (3791 of 16022)

// ─── the board reader ────────────────────────────────────────────────────────
//
// Deliberately small and deliberately not the extractor. All a match rule sees
// is a value string, a footprint string and (on a schematic) an MPN, so all this
// needs to recover is those. It handles both KiCad footprint text forms, because
// the corpus mixes them: `(property "Value" "...")` on KiCad 8 boards and
// `(fp_text value ...)` on KiCad 6/7 and legacy `(module ...)` boards.

/// Every `(footprint_lib, value)` pair on a `.kicad_pcb`.
fn kicad_components(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut starts: Vec<(usize, String)> = Vec::new();
    for tag in ["(footprint \"", "(module "] {
        let mut from = 0;
        while let Some(i) = text[from..].find(tag) {
            let at = from + i;
            let rest = &text[at + tag.len()..];
            let lib = if tag.ends_with('"') {
                rest.split('"').next().unwrap_or("").to_string()
            } else {
                rest.split_whitespace().next().unwrap_or("").to_string()
            };
            starts.push((at, lib));
            from = at + tag.len();
        }
    }
    starts.sort_by_key(|(at, _)| *at);

    for (n, (at, lib)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map_or(text.len(), |(a, _)| *a);
        let chunk = &text[*at..end];
        let value = field(chunk, "(property \"Value\" \"")
            .or_else(|| field(chunk, "(fp_text value \""))
            .or_else(|| bare_fp_text_value(chunk))
            .unwrap_or_default();
        out.push((lib.clone(), value));
    }
    out
}

fn field(chunk: &str, prefix: &str) -> Option<String> {
    let i = chunk.find(prefix)? + prefix.len();
    Some(chunk[i..].split('"').next()?.to_string())
}

/// Legacy `(fp_text value ML (at ...))`: unquoted, whitespace-terminated.
fn bare_fp_text_value(chunk: &str) -> Option<String> {
    let i = chunk.find("(fp_text value ")? + "(fp_text value ".len();
    let rest = &chunk[i..];
    let tok = rest.split_whitespace().next()?;
    (!tok.starts_with('(')).then(|| tok.trim_end_matches(')').to_string())
}

/// Every `(footprint_lib, value)` pair on an Eagle `.brd`.
fn eagle_components(text: &str) -> Vec<(String, String)> {
    // <element name="U1" library="..." package="SOT23-5" value="MCP73831"/>
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find("<element ") {
        let at = from + i;
        let end = text[at..].find('>').map_or(text.len(), |e| at + e);
        let tag = &text[at..end];
        let pkg = attr(tag, "package").unwrap_or_default();
        let val = attr(tag, "value").unwrap_or_default();
        out.push((pkg, val));
        from = end.max(at + 1);
    }
    out
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let pat = format!("{name}=\"");
    let i = tag.find(&pat)? + pat.len();
    Some(tag[i..].split('"').next()?.to_string())
}

/// (bound, total) for one board, resolving each component against the library.
fn bind_rate(board: &Path) -> (usize, usize) {
    let text = std::fs::read_to_string(board).unwrap_or_default();
    let comps = if board.extension().is_some_and(|e| e == "brd") {
        eagle_components(&text)
    } else {
        kicad_components(&text)
    };

    let lib = ModelLibrary::builtin();
    let mut bound = 0;
    for (footprint, value) in &comps {
        let q = ComponentQuery {
            value: (!value.is_empty()).then(|| value.clone()),
            footprint: (!footprint.is_empty()).then(|| footprint.clone()),
            ..Default::default()
        };
        if lib.resolve(&q).model.is_some() {
            bound += 1;
        }
    }
    (bound, comps.len())
}

fn corpus_board(rels: &[&str]) -> Option<PathBuf> {
    hauksbee_testkit::corpus_board_any(env!("CARGO_MANIFEST_DIR"), rels)
}

#[test]
fn corpus_bind_rates_do_not_regress() {
    let mut ran = 0;
    let mut failures = Vec::new();

    for floor in FLOORS {
        let Some(board) = corpus_board(floor.rels) else {
            let msg = format!(
                "corpus_bind_rates_do_not_regress: {} not found via the board \
                 corpus. Get it with: scripts/fetch-corpus.sh",
                floor.rels[0]
            );
            assert!(!hauksbee_testkit::require_assets(), "{msg}");
            eprintln!("note: {msg}");
            continue;
        };
        ran += 1;

        let (bound, total) = bind_rate(&board);

        if bound < floor.min_bound {
            failures.push(format!(
                "{}: bound {bound} of {total}, floor is {}. A match rule has \
                 narrowed or an entry has gone. Find which part stopped binding \
                 with:\n      hauksbee models resolve '{}' | grep UNRESOLVED",
                floor.rels[0],
                floor.min_bound,
                board.display()
            ));
        }

        // A floor that passes because the board now reports far fewer components
        // is not a pass, it is the same regression in a different hat. 10% of
        // slack absorbs an upstream revision bump.
        if total * 10 < floor.total_when_measured * 9 {
            failures.push(format!(
                "{}: now reports {total} components where the floor was measured \
                 against {}. The bind count is not comparable; re-measure rather \
                 than trusting it.",
                floor.rels[0], floor.total_when_measured
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "corpus bind-rate regression:\n    {}",
        failures.join("\n    ")
    );
    assert!(
        ran > 0 || !hauksbee_testkit::require_assets(),
        "HAUKSBEE_REQUIRE_CORPUS is set but no corpus board was found"
    );
}

#[test]
fn the_corpus_wide_rate_holds_its_floor() {
    let Some(root) = hauksbee_testkit::corpus_boards_root(env!("CARGO_MANIFEST_DIR")) else {
        let msg = "the_corpus_wide_rate_holds_its_floor: no board corpus. \
                   Get it with: scripts/fetch-corpus.sh";
        assert!(!hauksbee_testkit::require_assets(), "{msg}");
        eprintln!("note: {msg}");
        return;
    };

    let mut boards = Vec::new();
    collect(&root, &mut boards);
    assert!(
        !boards.is_empty(),
        "the corpus directory exists but holds no .kicad_pcb or .brd: a sweep \
         that walks nothing must not be reported as evidence"
    );

    let (mut bound, mut total) = (0usize, 0usize);
    for b in &boards {
        let (nb, nt) = bind_rate(b);
        bound += nb;
        total += nt;
    }

    assert!(total > 0, "no components read from {} boards", boards.len());
    let pct = 100.0 * bound as f64 / total as f64;
    assert!(
        pct >= CORPUS_WIDE_FLOOR_PCT,
        "corpus-wide bind rate {pct:.1}% ({bound} of {total} across {} boards) \
         has fallen below the {CORPUS_WIDE_FLOOR_PCT}% floor",
        boards.len()
    );
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            collect(&p, out);
        } else if p
            .extension()
            .is_some_and(|x| x == "kicad_pcb" || x == "brd")
        {
            out.push(p);
        }
    }
}
