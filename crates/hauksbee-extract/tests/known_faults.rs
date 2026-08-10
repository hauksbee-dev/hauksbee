//! Known-fault validation: run hauksbee against open-hardware board revisions
//! that fixed a real, *documented* electrical fault between two public releases.
//! The revision history is ground truth: "hauksbee flags rev N for exactly the
//! thing rev N+1 fixed" is the strongest calibration the tool can have.
//!
//! These tests are corpus-gated (skipped, not failed, when the board corpus is
//! absent) like `drc_corpus.rs`, because they read the historical revision pairs
//! grouped as `{zswatch_devkit,watchy_history}/<version>/` under the corpus root.
//! Every board goes through the layout-tolerant resolver, so the hand-built and
//! fetched corpora both work; and every gate here prints the number of boards it
//! opened, because a gold row that opened none has validated nothing.
//!
//! Each gold pair below has a prior-art citation in
//! `docs/evidence/KNOWN_FAULTS_VALIDATION.md`. The point of encoding them as tests is
//! that the calibration is now CI-enforced: if a future change to the I2C
//! pull-up check stops flagging the faulty ZSWatch DevKit 1.2.0, or starts
//! flagging the fixed 1.2.1, the regression fails here immediately.

use std::path::PathBuf;

use hauksbee_extract::{ExtractedBoard, LintCheck};

/// The directory the board ids sit under, whichever corpus layout is on disk.
///
/// Corpus-gated skip; `HAUKSBEE_REQUIRE_CORPUS=1` turns absence into a hard
/// fail, so the gold-row calibration cannot vacuously green-out on a runner
/// that is supposed to have the corpus.
///
/// This hardcoded `<corpus>/famous`, which exists only in the hand-built layout.
/// A corpus produced by `scripts/fetch-corpus.sh` has the board ids at its root,
/// so the join named a directory that was not there and every gold row here
/// failed on board LOCATION rather than on a finding.
fn boards_root() -> Option<PathBuf> {
    match hauksbee_testkit::corpus_boards_root(env!("CARGO_MANIFEST_DIR")) {
        Some(p) => Some(p),
        None => {
            require_corpus("the board corpus (neither <corpus>/famous/ nor <corpus>/ resolved)");
            None
        }
    }
}

/// One gold-row board, by corpus-relative path, in whichever layout holds it.
///
/// The revision pairs are the awkward case: the hand-built corpus groups them as
/// `zswatch_devkit/v1.2.0/`, and the fetch writes the same grouping via the
/// manifest's `dest`. Going through the resolver keeps both live.
fn board(rel: &str) -> Option<PathBuf> {
    match hauksbee_testkit::corpus_board(env!("CARGO_MANIFEST_DIR"), rel) {
        Some(p) => Some(p),
        None => {
            require_corpus(rel);
            None
        }
    }
}

/// A required corpus file is missing: skip, unless HAUKSBEE_REQUIRE_CORPUS is set,
/// in which case fail loudly so the calibration is genuinely CI-enforced.
fn require_corpus(what: &str) {
    if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
        panic!("HAUKSBEE_REQUIRE_CORPUS set but required corpus path is missing: {what}");
    }
    eprintln!("corpus path absent ({what}); skipping (set HAUKSBEE_REQUIRE_CORPUS=1 to fail)");
}

fn lint_pcb(path: &PathBuf) -> hauksbee_extract::NetLintReport {
    let text = std::fs::read_to_string(path).expect("read board");
    ExtractedBoard::from_auto(&text)
        .expect("parse board")
        .net_lint()
}

fn i2c_findings(r: &hauksbee_extract::NetLintReport) -> usize {
    r.of_check(LintCheck::MissingI2cPullup).count()
}

/// Count medium-or-higher I2C pull-up findings (the on-board, high-confidence
/// ones; Low is the intentional header break-out and is not a fault).
fn i2c_medium_plus(r: &hauksbee_extract::NetLintReport) -> usize {
    use hauksbee_extract::Severity;
    r.of_check(LintCheck::MissingI2cPullup)
        .filter(|f| matches!(f.severity, Severity::Medium | Severity::High))
        .count()
}

// ---------------------------------------------------------------------------
// GOLD ROW: ZSWatch Watch-DevKit-HW, "missing I2C pull-ups" (PR #158).
//
//   github.com/ZSWatch/Watch-DevKit-HW, CHANGELOG 1.2.1:
//     "Add missing pull-ups for I2C lines (#158)"
//
// Faulty 1.2.0: the RTC-side I2C bus (PCA9306 SDA2/SCL2 -> RV-8263 RTC) carries
// no pull-up. The PCA9306 is a bare pass-gate (no integrated pulls), so that
// translated bus genuinely needs its own resistors. hauksbee flags it MEDIUM.
// Fixed 1.2.1: R504/R505 (3k3) added; hauksbee goes clean. This is the strongest
// possible calibration: the tool flags exactly what the next revision fixed.
// ---------------------------------------------------------------------------

#[test]
fn zswatch_devkit_i2c_pullup_flagged_in_faulty_clean_in_fixed() {
    let Some(faulty) = board("zswatch_devkit/v1.2.0/ZSWatch-Watch-DevKit.kicad_pcb") else {
        return;
    };
    let Some(fixed) = board("zswatch_devkit/v1.2.1/ZSWatch-Watch-DevKit.kicad_pcb") else {
        return;
    };
    hauksbee_testkit::scanned("ZSWatch DevKit I2C gold row", 2);

    // Faulty 1.2.0: at least the two on-board RTC bus lines flag at medium+.
    let rf = lint_pcb(&faulty);
    let med_faulty = i2c_medium_plus(&rf);
    assert!(
        med_faulty >= 2,
        "faulty DevKit 1.2.0 should flag the missing RTC I2C pull-ups (>=2 medium), got {med_faulty}: {:?}",
        rf.of_check(LintCheck::MissingI2cPullup).map(|f| &f.message).collect::<Vec<_>>()
    );

    // Fixed 1.2.1: the pull-ups are present; ZERO I2C findings of any severity.
    let rfx = lint_pcb(&fixed);
    assert_eq!(
        i2c_findings(&rfx),
        0,
        "fixed DevKit 1.2.1 added R504/R505 and should be I2C-clean, got: {:?}",
        rfx.of_check(LintCheck::MissingI2cPullup)
            .map(|f| &f.message)
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Cross-check: the SHIPPED ZSWatch mainboard (corpus board, the FIXED design
// for this same PCA9306 + RV-8263 RTC topology) carries its pull-ups and is
// clean. This is the same circuit the FAMOUS_SWEEP C2 deep-dive analysed, where
// the pulls WERE present; here we assert hauksbee stays clean on it, so the
// faulty/fixed contrast above is a true discriminator and not a parser quirk.
// ---------------------------------------------------------------------------

#[test]
fn zswatch_mainboard_rtc_i2c_is_clean() {
    let Some(mainboard) = board("zswatch_mainboard/watch/ZSWatch-Watch.kicad_pcb") else {
        return;
    };
    hauksbee_testkit::scanned("ZSWatch mainboard RTC I2C cross-check", 1);
    let r = lint_pcb(&mainboard);
    assert_eq!(
        i2c_medium_plus(&r),
        0,
        "shipped ZSWatch mainboard should have no on-board missing-pull-up findings, got: {:?}",
        r.of_check(LintCheck::MissingI2cPullup)
            .map(|f| &f.message)
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// DOCUMENTED MISS (guard, not a gold row): the "control input driven only by a
// Hi-Z-capable MCU GPIO with no pull" fault. Two independently-cited instances:
//
//   * Watchy issue #14: e-paper RES# on a normal GPIO, no pull-up; fixed in
//     v2.0 by adding R20 (100K) to 3V3.
//   * ZSWatch DevKit issue #123: DISPLAY-EN (the NTS0104 OE) on GPIO P0.20,
//     no pull-up; fixed in 1.2.0 by adding R613/R614 (100K).
//
// hauksbee does NOT flag these, and the validation doc explains why this is the
// correct, honest behaviour: the identical structure (GPIO -> IC enable/reset,
// no pull) appears on DISPLAY-CS, DISPLAY-RST, VIB-EN and on the shipped
// mainboard / Reform / Watchy boost-EN, none of which are faults. A structural
// check cannot tell the one documented fault from the many benign twins without
// manufacturing false positives. This test pins that the FIXED revisions, which
// DO carry the pull, are clean, so if a future check ever tries to fire here it
// must at least not fire on the corrected designs.
// ---------------------------------------------------------------------------

#[test]
fn fixed_control_pull_revisions_are_lint_clean() {
    if boards_root().is_none() {
        return;
    }
    // Watchy v2.0 (R20 RES# pull-up present) and ZSWatch DevKit 1.2.0
    // (R613/R614 DISPLAY-EN pull present) must be clean of floating-control-pin
    // findings.
    let wanted = [
        "watchy_history/v2.0/Watchy.kicad_pcb",
        "zswatch_devkit/v1.2.0/ZSWatch-Watch-DevKit.kicad_pcb",
    ];
    let mut checked = 0usize;
    for rel in wanted {
        let Some(b) = board(rel) else { continue };
        checked += 1;
        let r = lint_pcb(&b);
        let floating = r.of_check(LintCheck::FloatingControlPin).count();
        assert_eq!(
            floating,
            0,
            "fixed revision {} should have no floating-control-pin findings, got {floating}",
            b.display()
        );
    }
    hauksbee_testkit::scanned("fixed-control-pull guard", checked);
}

// ---------------------------------------------------------------------------
// ODrive v2 GND <-> AGND overlap gold row. The upstream repo's v2 directory
// carries two layouts: Inverter45attempt.PcbDoc (an attempt that was never the
// shipped design) and the final Inverter.PcbDoc. The attempt overlaps its GND
// and AGND pours on F.Cu at x=153.2, y=183.9 (gap -1.0 mm), and the file's own
// rule set forbids it: its only ShortCircuit rule is the Altium default (scope
// All/All, ALLOWED=FALSE), so Altium's DRC would flag the identical overlap and
// no scoped allowance sanctions a deliberate ground join. The final layout is
// short-clean, which makes the pair a true discriminator: hauksbee flags the
// attempt for exactly the defect the final file does not have. The odrive_v2
// corpus entry is known_good = false for this adjudicated reason (the marker is
// per-directory and both files share one), so THIS test, not the fetched-sweep
// silence gate, owns both files' coverage.
// ---------------------------------------------------------------------------

#[test]
fn odrive_v2_attempt_ground_short_flagged_and_final_is_clean() {
    let Some(attempt) = board("odrive/v2/v2/Inverter45attempt.PcbDoc") else {
        return;
    };
    let Some(fixed) = board("odrive/v2/v2/Inverter.PcbDoc") else {
        return;
    };
    hauksbee_testkit::scanned("ODrive v2 ground-short gold row", 2);

    let bytes = std::fs::read(&attempt).expect("read attempt board");
    let report = hauksbee_extract::ExtractedBoard::altium_drc(&bytes).expect("drc runs");
    let shorts: Vec<_> = report.shorts().collect();
    assert_eq!(
        shorts.len(),
        1,
        "the attempt layout carries exactly the one GND/AGND overlap, got: {:?}",
        shorts
            .iter()
            .map(|s| format!("{} <-> {} on {}", s.net_a_name, s.net_b_name, s.layer))
            .collect::<Vec<_>>()
    );
    let s = shorts[0];
    let mut nets = [s.net_a_name.as_str(), s.net_b_name.as_str()];
    nets.sort_unstable();
    assert_eq!(
        nets,
        ["AGND", "GND"],
        "the attempt's short is the analog/digital ground overlap"
    );
    assert_eq!(s.layer, "F.Cu");

    let bytes = std::fs::read(&fixed).expect("read final board");
    let report = hauksbee_extract::ExtractedBoard::altium_drc(&bytes).expect("drc runs");
    assert_eq!(
        report.shorts().count(),
        0,
        "the final Inverter.PcbDoc is short-clean, so the contrast is real"
    );
}

// ---------------------------------------------------------------------------
// MWGEN-G1 pad-overlap gold row. An RF signal-generator design, 373 footprints
// on four layers, drawn in KiCad 6. Its reference-input corner puts an SMAJ48CA
// TVS in an SMA body (D503, 2.5 x 1.8 mm pads) on top of two SOT-23 diodes and two
// hand-solder 0603s, and its Laird BMI-S-205-F shield-can fence pad on J206 on
// top of a 0603 ferrite bead. Six pads of different nets overlap as a result.
//
// The ground truth is the same shape as the ODrive row's: the design's own rule
// set forbids the overlaps, and here a second tool agrees on every one. Both
// halves are recorded rather than described, so neither can rot into a
// hand-typed list nobody re-derives:
//
//   * `mwgen_g1_2fc77c90_kicad_9_0_3_shorts.json` is KiCad 9.0.3's own
//     `kicad-cli pcb drc` output on this exact revision, its six shorting_items
//     violations verbatim. This test derives the expected set FROM that file, so
//     the comparison is against a recording of the other tool, and CI needs no
//     KiCad installed. That pins hauksbee, not KiCad: a change in KiCad's own
//     reading is invisible until someone re-runs the recorded command.
//     `mwgen_g1_2fc77c90_oracle.md` holds the command and the input hash.
//   * MWGEN-G1.kicad_pro is read here too: shorting_items and clearance are
//     `error` and `drc_exclusions` is empty, so the design neither loosened the
//     rule that forbids these overlaps nor recorded an exclusion accepting an
//     instance of one. (Those severities are KiCad's defaults, not a deliberate
//     tightening; the same is true of the Altium default the ODrive row rests
//     on.)
//
// The gap magnitudes are hauksbee's own measurements, pinned as a regression
// guard: KiCad's JSON reports no gap for a shorting_items violation, so they are
// not part of the agreement.
// ---------------------------------------------------------------------------

/// One `shorting_items` violation as KiCad recorded it: the net pair and the two
/// pads, each with its owning footprint, pad number and centre.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RecordedShort {
    /// Sorted, so it can be compared with hauksbee's unordered pair.
    nets: [String; 2],
    layer: String,
    /// Sorted `"<REF> pad <n>"`, so a wrong pad on the right footprint fails.
    pads: [String; 2],
    /// The two pad centres, in file order, as micrometre integers (`Ord`, and
    /// exact: KiCad writes them to 6 decimal places at most).
    anchors_um: [(i64, i64); 2],
}

/// KiCad's recorded verdict on MWGEN-G1, parsed out of the committed report
/// rather than transcribed.
///
/// KiCad states each shorted item as `"Pad <n> [<net>] of <REF> on <LAYER>"`,
/// which carries the pad number, the net, the footprint and the layer, and gives
/// the pad centre in `items[].pos`. A violation whose two items disagree on the
/// layer would be a different kind of finding and panics here rather than being
/// averaged away.
fn kicad_recorded_shorts(oracle: &str) -> Vec<RecordedShort> {
    let json: serde_json::Value =
        serde_json::from_str(oracle).expect("the recorded KiCad report parses");
    let mut out = Vec::new();
    for v in json["violations"].as_array().expect("violations array") {
        assert_eq!(v["type"], "shorting_items", "the oracle holds only shorts");
        let mut nets = Vec::new();
        let mut pads = Vec::new();
        let mut layers = Vec::new();
        let mut anchors = Vec::new();
        for item in v["items"].as_array().expect("items array") {
            let d = item["description"].as_str().expect("item description");
            let pad_no = d
                .strip_prefix("Pad ")
                .and_then(|r| r.split_once(' '))
                .map(|(n, _)| n.to_string())
                .unwrap_or_else(|| panic!("no leading 'Pad <n>' in {d:?}"));
            let (net, rest) = d
                .split_once('[')
                .and_then(|(_, r)| r.split_once(']'))
                .unwrap_or_else(|| panic!("no [net] in {d:?}"));
            let rest = rest.trim_start_matches(" of ");
            let (reference, layer) = rest
                .split_once(" on ")
                .unwrap_or_else(|| panic!("no ' on <layer>' in {d:?}"));
            let um = |v: &serde_json::Value| (v.as_f64().expect("pos is a number") * 1e3) as i64;
            nets.push(net.to_string());
            pads.push(format!("{reference} pad {pad_no}"));
            layers.push(layer.to_string());
            anchors.push((um(&item["pos"]["x"]), um(&item["pos"]["y"])));
        }
        assert_eq!(nets.len(), 2, "a short is between exactly two items");
        assert_eq!(layers[0], layers[1], "both items on one layer");
        nets.sort();
        pads.sort();
        out.push(RecordedShort {
            nets: [nets[0].clone(), nets[1].clone()],
            layer: layers.remove(0),
            pads: [pads[0].clone(), pads[1].clone()],
            anchors_um: [anchors[0], anchors[1]],
        });
    }
    out.sort();
    out
}

#[test]
fn mwgen_g1_pad_overlap_shorts_match_kicads_own_drc() {
    let Some(pcb) = board("mwgen_g1/MWGEN-G1.kicad_pcb") else {
        return;
    };
    let Some(pro) = board("mwgen_g1/MWGEN-G1.kicad_pro") else {
        return;
    };
    hauksbee_testkit::scanned("MWGEN-G1 pad-overlap gold row", 1);

    let recorded = kicad_recorded_shorts(include_str!("mwgen_g1_2fc77c90_kicad_9_0_3_shorts.json"));
    assert_eq!(
        recorded.len(),
        6,
        "the recording is the six-short report this row was adjudicated on"
    );

    let text = std::fs::read_to_string(&pcb).expect("read MWGEN-G1 board");
    let report = hauksbee_extract::ExtractedBoard::drc(&text).expect("drc runs");

    // Key each finding by (sorted net pair, layer, sorted footprint pair). All
    // six keys are distinct on this board, so a map loses nothing and lets the
    // gap and the location be checked PER finding: a sorted list of gaps would
    // pass if two findings swapped their measurements.
    type Key = ([String; 2], String, [String; 2]);
    let mut got: std::collections::BTreeMap<Key, (f64, f64, f64)> = Default::default();
    for s in report.shorts() {
        let mut nets = [s.net_a_name.clone(), s.net_b_name.clone()];
        nets.sort();
        let mut owners = [s.item_a.owner.clone(), s.item_b.owner.clone()];
        owners.sort();
        let key = (nets, s.layer.clone(), owners);
        assert!(
            got.insert(key.clone(), (s.gap_mm, s.x, s.y)).is_none(),
            "two findings share the key {key:?}, so per-finding checks would alias"
        );
    }

    // KiCad's expected keys come from the recording. The footprint pair is
    // derived from its "<REF> pad <n>" strings, and the pad NUMBER is pinned
    // separately below, through the location check.
    let want: std::collections::BTreeSet<Key> = recorded
        .iter()
        .map(|r| {
            let mut owners = [
                r.pads[0].split(" pad ").next().unwrap().to_string(),
                r.pads[1].split(" pad ").next().unwrap().to_string(),
            ];
            owners.sort();
            (r.nets.clone(), r.layer.clone(), owners)
        })
        .collect();
    assert_eq!(
        got.keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        want,
        "hauksbee's shorts must be exactly the set KiCad 9.0.3 recorded"
    );

    // hauksbee's own measurement of each overlap, keyed so it cannot be satisfied
    // by the right numbers on the wrong findings. Negative throughout, because
    // every one is pad copper inside pad copper.
    let expected: &[(&str, &str, [&str; 2], f64)] = &[
        (
            "/Clock reference/Input Clocks/REFIN",
            "/Clock reference/Input Clocks/REFIN_GND",
            ["C205", "D503"],
            -0.015386,
        ),
        (
            "/Clock reference/Input Clocks/REFIN",
            "/Clock reference/Input Clocks/REFIN_GND",
            ["D503", "R204"],
            -0.015386,
        ),
        (
            "/Clock reference/Input Clocks/REFIN_GND",
            "Net-(D202-Pad3)",
            ["D202", "D503"],
            -0.059614,
        ),
        (
            "/Clock reference/Input Clocks/REFIN_GND",
            "Net-(D202-Pad3)",
            ["D203", "D503"],
            -0.059614,
        ),
        (
            "/Clock reference/REFOUT_SEC",
            "GND",
            ["D203", "D503"],
            -0.150001,
        ),
        ("GND", "Net-(FB204-Pad1)", ["FB204", "J206"], -0.000001),
    ];
    assert_eq!(expected.len(), got.len());
    for (net_a, net_b, owners, want_gap) in expected {
        let key = (
            [net_a.to_string(), net_b.to_string()],
            "F.Cu".to_string(),
            [owners[0].to_string(), owners[1].to_string()],
        );
        let (gap, _, _) = got
            .get(&key)
            .unwrap_or_else(|| panic!("no finding for {key:?}; got {:?}", got.keys()));
        assert!(
            *gap < 0.0 && (gap - want_gap).abs() < 1e-6,
            "{net_a} <-> {net_b} on {owners:?}: measured {gap} mm, expected {want_gap} mm"
        );
    }

    // And each contact point must sit on the pads KiCad named. The tolerance is
    // 2 mm, which is under half the 4 mm between D503's own two pads, so this
    // fails if a finding drifts to the other pad of the same footprint: the pad
    // number KiCad reports is pinned even though hauksbee's finding only names
    // the footprint.
    for r in &recorded {
        let mut owners = [
            r.pads[0].split(" pad ").next().unwrap().to_string(),
            r.pads[1].split(" pad ").next().unwrap().to_string(),
        ];
        owners.sort();
        let key = (r.nets.clone(), r.layer.clone(), owners);
        let (_, x, y) = got[&key];
        for (ax, ay) in r.anchors_um {
            let d = ((x - ax as f64 / 1e3).powi(2) + (y - ay as f64 / 1e3).powi(2)).sqrt();
            assert!(
                d < 2.0,
                "contact point ({x:.3}, {y:.3}) is {d:.3} mm from KiCad's pad anchor \
                 ({:.3}, {:.3}) for {:?}",
                ax as f64 / 1e3,
                ay as f64 / 1e3,
                r.pads
            );
        }
    }

    // The design's own rule set, so "forbidden by the file itself" is asserted
    // rather than asserted-in-prose.
    let project: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&pro).expect("read MWGEN-G1 project"))
            .expect("the project file parses");
    let settings = &project["board"]["design_settings"];
    assert_eq!(settings["rule_severities"]["shorting_items"], "error");
    assert_eq!(settings["rule_severities"]["clearance"], "error");
    assert_eq!(
        settings["drc_exclusions"]
            .as_array()
            .expect("drc_exclusions is a list")
            .len(),
        0,
        "no exclusion records an instance of this being accepted"
    );
}

// ---------------------------------------------------------------------------
// emonTx V3.4.5 pour-overlap row: one file, one net pair, two layers, opposite
// verdicts, and the fabrication output to settle which is which.
//
// GND and AGND are separate nets with no discrete part bridging them, and their
// same-rank pours overlap in the same 0.349 mm band on both copper layers where
// the finding sits (GND's outline runs along y=43.205 with fill below; AGND's
// runs along y=42.856 with fill above). Nothing in that geometry separates the
// layers, and reading the outline overlap as copper reported a short on both.
//
// The gerbers upstream ships beside the `.brd` separate them, and this test reads
// those gerbers rather than trusting the write-up: on F.Cu, where both pours hold
// isolate="0.3048", a point inside the AGND pour and a point inside the GND pour
// land in DIFFERENT filled contours (8.236 mm apart at their closest); on B.Cu,
// where the AGND pour carries isolate="0.00030625", the same two points land in
// the SAME contour. So the oracle is asserted, not described, and if a future
// fetch landed different fabrication data the test would say so instead of
// quietly agreeing with whatever the checker now returns.
//
// What the oracle settles is which layer has contact. That the isolate is the
// setting which PERMITTED it is an inference on top, argued in the evidence doc
// and deliberately not smuggled into this test's assertions.
//
// The emontx3 corpus entry is known_good = false because of the B.Cu contact.
// That is not a claim the board is defective; a silence gate needs boards the
// tool says nothing about, and here it correctly says something.
// ---------------------------------------------------------------------------

/// Which `G36`/`G37` region fill of a Gerber copper layer encloses a point.
///
/// Returns `(contour index, dark)`: the index identifies the fill, and `dark` is
/// the layer polarity in force when it was drawn (`%LPD*%` adds copper, `%LPC*%`
/// erases). Two points in the same dark contour are in one poured body; two
/// points in different dark contours are in two.
///
/// What this deliberately does NOT model, because the claims made from it are
/// scoped to match: strokes (`D01`), flashes (`D03`) and arc curvature are
/// ignored, so it cannot see a track or an annulus bridging two fills, and it
/// carries no net attribution. The write-up's separate, polarity-and-stroke-aware
/// raster experiments are where those questions are settled; this is the part
/// worth pinning in CI, because it is cheap, exact on the vector data, and is the
/// step the verdict turns on. As a guard against the polarity hole mattering
/// here, callers assert that no CLEAR contour encloses the probe points.
fn gerber_fill_contour(text: &str, x: f64, y: f64) -> Option<(usize, bool)> {
    // (ring, dark) per region fill, in file order.
    let mut contours: Vec<(Vec<(f64, f64)>, bool)> = Vec::new();
    let mut current: Option<Vec<(f64, f64)>> = None;
    let (mut cx, mut cy) = (0.0f64, 0.0f64);
    let mut dark = true;
    // %FSLAX34Y34%: six integer digits, four of them fractional, so a raw
    // coordinate is millimetres * 1e4.
    assert!(
        text.contains("%FSLAX34Y34*%") && text.contains("%MOMM*%"),
        "this reader assumes the Eagle export's own X34Y34 millimetre format"
    );
    let flush =
        |cur: &mut Option<Vec<(f64, f64)>>, out: &mut Vec<(Vec<(f64, f64)>, bool)>, dark: bool| {
            if let Some(pts) = cur.take() {
                if pts.len() > 2 {
                    out.push((pts, dark));
                }
            }
        };
    // Line by line, then statement by statement. Splitting the whole file on
    // `*` does not work: an extended command is `%...*%`, so its own `*` cuts
    // the following statement in half and 139 of this layer's 156 `G36`s arrive
    // glued to a stray `%` and get skipped as extended commands.
    for block in text.lines().flat_map(|line| line.split('*')).map(str::trim) {
        if block.starts_with("G04") {
            continue;
        }
        // Polarity is an extended command, so it has to be read BEFORE the
        // `%`-prefixed statements are skipped.
        if block.starts_with("%LPD") {
            dark = true;
            continue;
        }
        if block.starts_with("%LPC") {
            dark = false;
            continue;
        }
        if block.starts_with('%') {
            continue;
        }
        if block.ends_with("G36") {
            flush(&mut current, &mut contours, dark);
            current = Some(Vec::new());
            continue;
        }
        if block.ends_with("G37") {
            flush(&mut current, &mut contours, dark);
            continue;
        }
        let coord = |tag: char, fallback: f64| -> f64 {
            match block.split(tag).nth(1) {
                Some(rest) => {
                    let digits: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_digit() || *c == '-')
                        .collect();
                    digits.parse::<f64>().map(|v| v / 1e4).unwrap_or(fallback)
                }
                None => fallback,
            }
        };
        let op = if block.ends_with("D01") || block.ends_with("D1") {
            1
        } else if block.ends_with("D02") || block.ends_with("D2") {
            2
        } else {
            continue;
        };
        let (nx, ny) = (coord('X', cx), coord('Y', cy));
        if let Some(pts) = current.as_mut() {
            if op == 2 {
                // A move inside a region statement starts a new contour.
                if pts.len() > 2 {
                    contours.push((std::mem::take(pts), dark));
                } else {
                    pts.clear();
                }
                pts.push((nx, ny));
            } else {
                pts.push((nx, ny));
            }
        }
        (cx, cy) = (nx, ny);
    }
    flush(&mut current, &mut contours, dark);
    assert!(
        !contours.is_empty(),
        "no G36 region fills found; this is not the copper layer we think it is"
    );
    // Even-odd crossing count. Last enclosing contour wins, because a later
    // clear region erases what an earlier dark one laid down.
    let hit = |pts: &Vec<(f64, f64)>| {
        let mut inside = false;
        for i in 0..pts.len() {
            let (x1, y1) = pts[i];
            let (x2, y2) = pts[(i + 1) % pts.len()];
            if (y1 > y) != (y2 > y) && x < (x2 - x1) * (y - y1) / (y2 - y1) + x1 {
                inside = !inside;
            }
        }
        inside
    };
    contours
        .iter()
        .enumerate()
        .rfind(|(_, (pts, _))| hit(pts))
        .map(|(i, (_, dark))| (i, *dark))
}

#[test]
fn emontx_ground_pour_join_is_flagged_on_the_merged_layer_only() {
    let Some(brd) = board("emontx3/hardware/V3.4.5/emonTx V3.4.5.brd") else {
        return;
    };
    let gerber = |name: &str| {
        board(&format!(
            "emontx3/hardware/V3.4.5/GERBERS emonTx V3.4.5_2021-04-07\
             /CAMOutputs/GerberFiles/{name}"
        ))
    };
    let Some(top) = gerber("copper_top.gbr") else {
        return;
    };
    let Some(bottom) = gerber("copper_bottom.gbr") else {
        return;
    };
    hauksbee_testkit::scanned("emonTx V3.4.5 pour-overlap row", 1);

    // The oracle. Two probe points well inside each pour's own territory, away
    // from the seam: (60, 60) is inside the AGND outline on both layers,
    // (10, 20) is inside the GND outline on both.
    let (agnd, gnd) = ((60.0, 60.0), (10.0, 20.0));
    for (path, layer, want_same) in [(&top, "F.Cu", false), (&bottom, "B.Cu", true)] {
        let text = std::fs::read_to_string(path).expect("read gerber");
        let (a, a_dark) = gerber_fill_contour(&text, agnd.0, agnd.1)
            .unwrap_or_else(|| panic!("{layer}: no fill at the AGND probe point"));
        let (g, g_dark) = gerber_fill_contour(&text, gnd.0, gnd.1)
            .unwrap_or_else(|| panic!("{layer}: no fill at the GND probe point"));
        // Both layers carry clear-polarity regions (136 on F.Cu, 99 on B.Cu),
        // which is why polarity is read at all: a probe landing in one would mean
        // the enclosing fill had been erased there and the comparison below would
        // be meaningless. Neither probe does, and this asserts it.
        assert!(
            a_dark && g_dark,
            "{layer}: a probe point sits in a CLEAR region (AGND dark={a_dark}, \
             GND dark={g_dark}), so it is not in poured copper"
        );
        assert_eq!(
            a == g,
            want_same,
            "{layer}: AGND fill contour {a}, GND fill contour {g}; expected \
             {} copper body/bodies",
            if want_same { "one" } else { "two separate" }
        );
    }

    // The verdict, which must follow the oracle layer for layer.
    let text = std::fs::read_to_string(&brd).expect("read emonTx board");
    let report = hauksbee_extract::ExtractedBoard::drc(&text).expect("drc runs");
    let shorts: Vec<_> = report.shorts().collect();
    assert_eq!(
        shorts.len(),
        1,
        "only the layer whose fill actually merged, got: {:?}",
        shorts
            .iter()
            .map(|s| format!("{} <-> {} on {}", s.net_a_name, s.net_b_name, s.layer))
            .collect::<Vec<_>>()
    );
    let s = shorts[0];
    let mut nets = [s.net_a_name.as_str(), s.net_b_name.as_str()];
    nets.sort_unstable();
    assert_eq!(nets, ["AGND", "GND"]);
    assert_eq!(
        s.layer, "B.Cu",
        "copper_bottom.gbr is the layer with one merged contour"
    );
    assert!(
        s.item_a.owner.contains("isolate 0.00030625")
            || s.item_b.owner.contains("isolate 0.00030625"),
        "the finding names the zeroed isolate that let the pours meet, got {:?} / {:?}",
        s.item_a.owner,
        s.item_b.owner
    );
}
