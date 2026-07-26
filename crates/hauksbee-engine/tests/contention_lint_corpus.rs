//! Zero-false-positive calibration for the model-aware driver-contention lint.
//!
//! The project ships a check only if it is provably SILENT on boards known to be
//! good. This sweeps every CAD file in the board corpus (the famous schematic /
//! layout set, the KiCad demos, the Eagle and Altium cross-validation boards, and
//! the Arduino-class reference designs) and asserts the contention lint finds
//! nothing. If a future change to the check or to the model db makes it fire on a
//! known-good board, this goes red before the false positive ships.
//!
//! ## What "non-vacuous" means for this corpus
//!
//! An early version of this sweep demanded that the corpus present modelled
//! PUSH-PULL output pins, and failed its own guard with "417 boards scanned, 0
//! driver pins". The instrumented answer to that zero: the corpus genuinely
//! carries almost no parts that resolve to the digital model db. Its 74-series
//! population is 74LS parts (not modelled) and 3-state buffers (74HC125,
//! 74HC365), and the modelled 74HC125s correctly present all four outputs as
//! tri-stateable, contributing zero push-pull pins BY DESIGN. So this corpus can
//! prove the classifier engages (parts bind, outputs are enumerated, the
//! tri-state exclusion applies) but cannot organically present a push-pull pin.
//! The guards below assert what the corpus can actually supply, and the
//! injected-fight test proves end-to-end firing on a real extraction, so the
//! sweep cannot silently decay into "ran but exercised nothing".
//!
//! Corpus-gated: skipped when the board-corpus symlink is absent, unless
//! `HAUKSBEE_REQUIRE_CORPUS=1` is set, which turns absence into a hard failure so
//! the calibration cannot vacuously green-out in CI.

use std::path::{Path, PathBuf};

use hauksbee_engine::checks::contention::{contention_lint, scan};
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

fn corpus_root() -> Option<PathBuf> {
    let p = hauksbee_testkit::corpus_dir(env!("CARGO_MANIFEST_DIR")).unwrap_or_default();
    if p.exists() {
        return Some(p);
    }
    if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
        panic!(
            "HAUKSBEE_REQUIRE_CORPUS set but board-corpus is missing: {}",
            p.display()
        );
    }
    eprintln!("board-corpus absent; skipping contention-lint calibration");
    None
}

fn load(p: &PathBuf) -> Option<ExtractedBoard> {
    if p.extension().and_then(|e| e.to_str()) == Some("kicad_sch") {
        return ExtractedBoard::from_kicad_schematic_path(p).ok();
    }
    ExtractedBoard::from_auto(&std::fs::read_to_string(p).ok()?).ok()
}

/// Walk the corpus and yield every loadable CAD file.
fn corpus_boards(root: &Path) -> Vec<(PathBuf, ExtractedBoard)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
            if !matches!(ext, "kicad_pcb" | "kicad_sch" | "net" | "brd") {
                continue;
            }
            // Files the extractor cannot handle are skipped, not failed: this
            // test calibrates the lint, it does not police the readers.
            let Some(board) = load(&p) else { continue };
            out.push((p, board));
        }
    }
    out
}

#[test]
fn contention_lint_is_silent_on_the_known_good_corpus() {
    let Some(root) = corpus_root() else { return };
    let lib = ModelLibrary::builtin();

    let mut scanned = 0usize;
    // Boards on which at least one part bound to a digital model and presented
    // output roles to the classifier. Silence over boards where nothing ever
    // reached the classification proves nothing.
    let mut engaged_boards = 0usize;
    let mut roles_presented = 0usize;
    let mut pushpull_pins = 0usize;
    let mut offenders: Vec<String> = Vec::new();

    for (p, board) in corpus_boards(&root) {
        scanned += 1;
        let s = scan(&board, &lib);
        if s.output_roles_presented() > 0 {
            engaged_boards += 1;
            roles_presented += s.output_roles_presented();
            pushpull_pins += s.pushpull_driver_pins();
            // The evidence trail: what the check saw, part by part, so a zero
            // in the totals is attributable instead of mysterious.
            eprintln!("engaged: {}", p.display());
            for part in &s.parts {
                eprintln!(
                    "  {} '{}' -> {}: {} output role(s), {} tri-stateable, \
                     {} push-pull pin(s) on eligible nets \
                     (pads: {} absent, {} unrouted, {} on ground/unconnected)",
                    part.reference,
                    part.value,
                    part.model_id,
                    part.output_roles,
                    part.tristateable_roles,
                    part.pushpull_pins,
                    part.pads_absent,
                    part.pads_unrouted,
                    part.pads_on_excluded_nets,
                );
            }
        }
        for f in contention_lint(&board, &lib).findings {
            offenders.push(format!("{}: {}", p.display(), f.message));
        }
    }

    eprintln!(
        "contention-lint calibration: {scanned} boards scanned, {engaged_boards} engaging the \
         classifier ({roles_presented} output roles presented, {pushpull_pins} push-pull driver \
         pins), {} finding(s)",
        offenders.len()
    );
    assert!(
        scanned >= 50,
        "expected a substantial corpus sweep, scanned only {scanned} boards"
    );
    // Anti-vacuity: the corpus must actually exercise the classifier. The
    // numbers are set from the measured corpus (see the module docs): the
    // 74HC125 boards alone bind multiple parts and present 4 output roles
    // each. If a db or resolver regression stops digital parts binding, or a
    // classifier regression stops enumerating outputs, this fails loudly
    // instead of letting the zero-finding assertion pass over an inert check.
    assert!(
        engaged_boards >= 2 && roles_presented >= 16,
        "the sweep must exercise the check, not just run it: only {engaged_boards} board(s) \
         engaged the classifier with {roles_presented} output role(s) presented"
    );
    assert!(
        offenders.is_empty(),
        "the contention lint must be silent on known-good boards, but fired on {} net(s):\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

/// End-to-end firing proof on a REAL extraction: take a known-good corpus board,
/// confirm the lint is silent on it, then inject two modelled 74HC08s whose `y1`
/// outputs (pad 3) short onto one of the board's real signal nets, and require
/// exactly one High finding naming that net and both injected parts. This is the
/// half of non-vacuity the corpus cannot supply organically (it owns no modelled
/// push-pull parts): it proves the full path from a real board's nets and pad
/// numbering through classification to the finding, not just the synthetic unit
/// fixtures.
#[test]
fn contention_lint_fires_on_a_real_board_with_an_injected_fight() {
    let Some(root) = corpus_root() else { return };
    let lib = ModelLibrary::builtin();

    let path = root.join("kicad-demos-src/demos/pic_programmer/pic_programmer.kicad_sch");
    let mut board =
        ExtractedBoard::from_kicad_schematic_path(&path).expect("pic_programmer schematic loads");
    assert!(
        contention_lint(&board, &lib).findings.is_empty(),
        "pic_programmer is a known-good board and must start silent"
    );

    // Pick a real signal net to stage the fight on: named, not ground, not a
    // KiCad unconnected placeholder.
    let net = board
        .nets
        .iter()
        .find(|n| {
            let name = n.name.trim_start_matches('/');
            !name.is_empty()
                && !name.starts_with("unconnected-")
                && !name.to_ascii_uppercase().contains("GND")
        })
        .expect("pic_programmer has a named signal net")
        .clone();

    let template = board.components[0].clone();
    let inject = |reference: &str| {
        let mut c = template.clone();
        c.reference = reference.to_string();
        c.value = "74HC08".to_string();
        c.lib_id = "74xx:74HC08".to_string();
        c.dnp = false;
        c.properties.clear();
        c.pins = vec![hauksbee_extract::Pin {
            number: "3".to_string(), // y1, push-pull, no output enable
            net: Some(net.id),
            function: String::new(),
            kind: String::new(),
            position: None,
        }];
        c
    };
    let (u1, u2) = (inject("U901"), inject("U902"));
    board.components.push(u1);
    board.components.push(u2);

    let findings = contention_lint(&board, &lib).findings;
    assert_eq!(
        findings.len(),
        1,
        "exactly one finding for the injected fight: {findings:?}"
    );
    let f = &findings[0];
    assert_eq!(f.nets, vec![net.name.clone()]);
    assert!(f.refs.contains(&"U901".to_string()) && f.refs.contains(&"U902".to_string()));
}
