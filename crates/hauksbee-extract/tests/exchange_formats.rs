//! Cross-format agreement for the two fab/assembly exchange readers.
//!
//! The strongest evidence an exchange reader is right is not that it parses:
//! it is that the SAME board, exported to the exchange format and read back,
//! agrees with the board hauksbee reads natively. KiCad 9 exports both ODB++
//! and IPC-2581, so for every fixture here the `.kicad_pcb` in
//! `crates/hauksbee-ci/examples/boards/` is ground truth with a known-correct
//! component, net and pad count (the KiCad reader is the oldest and
//! best-exercised path in the crate), and the two exchange readings are checked
//! against it — not just in counts, but in the net PARTITION over pins: two pins
//! that share a net in the KiCad reading must share one in the exchange reading,
//! and two that do not must not.
//!
//! ## Fixture provenance
//!
//! `fixtures/exchange/<board>.ipc2581.xml` and `<board>.odb.zip` were generated
//! on 2026-08-04 from this repo's own example boards with KiCad 9.0.3:
//!
//! ```text
//! kicad-cli pcb export ipc2581 -o <board>.ipc2581.xml <board>.kicad_pcb
//! kicad-cli pcb export odb --compression zip -o <board>.odb.zip <board>.kicad_pcb
//! ```
//!
//! `watchy.ipc2581.xml` is gzipped because the raw export is 2.5 MB; the test
//! inflates it. `watchy` is the interesting one: 82 parts and 312 pads over 84
//! nets, four copper layers, hierarchical net names KiCad escapes on export,
//! nameless mechanical pads, two test points placed twice, two pad-less artwork
//! placements (`REF**`, `G***`) and three `exclude_from_bom` footprints.
//!
//! `fixtures/exchange/thirdparty/` holds two REAL third-party IPC-2581 documents,
//! neither produced by KiCad, both MIT-licensed:
//!
//! * `ember-pcb.revB.xml` — revision B, from `akashlevy/ember-pcb` at commit
//!   825ca310 (`ember-pcb.ipc.xml`), a 25-layer FPGA interposer. It is a
//!   **stackup-only** export: real, valid, and carrying no placement.
//! * `testcase10-bom.revC.xml` — revision C, from `sjgallagher2/ipc2581` at
//!   commit befdfb02 (`examples/testcase10-Rev C data/testcase10-RevC-BOM.xml`),
//!   the BOM member of the IPC-2581 Consortium's published "testcase10" set,
//!   written by Allegro. Also carries no placement.
//!
//! Both are here because a reader is only as good as its refusals: these are the
//! files a user will really upload and be told "this is not the export I need".
//!
//! The one real third-party ODB++ job (a 2 MB Valor NPI 11.4 archive of a Mentor
//! PowerPCB design) is too large to commit and lives in the shared
//! `board-corpus/odbpp/`; its test skips when the corpus is absent, the same way
//! the Altium and gerber corpus sweeps do.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

use hauksbee_extract::{ipc2581, odbpp, Component, ExtractedBoard};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("exchange")
}

fn boards_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("hauksbee-ci")
        .join("examples")
        .join("boards")
}

fn read_text(p: &Path) -> String {
    let bytes = std::fs::read(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    if p.extension().is_some_and(|e| e == "gz") {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&bytes[..])
            .read_to_end(&mut out)
            .unwrap_or_else(|e| panic!("inflate {}: {e}", p.display()));
        return String::from_utf8_lossy(&out).into_owned();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The KiCad reading of an example board: ground truth.
fn native(board: &str) -> ExtractedBoard {
    let p = boards_dir().join(format!("{board}.kicad_pcb"));
    ExtractedBoard::from_kicad_pcb(&read_text(&p)).expect("the KiCad reader is ground truth")
}

fn ipc(board: &str) -> ipc2581::Ipc2581Extraction {
    let mut p = fixtures().join(format!("{board}.ipc2581.xml"));
    if !p.exists() {
        p = fixtures().join(format!("{board}.ipc2581.xml.gz"));
    }
    ipc2581::extract(&read_text(&p)).expect("the IPC-2581 fixture reads")
}

fn odb(board: &str) -> odbpp::OdbExtraction {
    let p = fixtures().join(format!("{board}.odb.zip"));
    let bytes = std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    odbpp::from_odbpp_archive(&bytes).expect("the ODB++ fixture reads")
}

/// (refdes, pad) → net name, for every *netted* pin. Net NAMES are compared
/// directly here because both exporters preserve them; the partition check below
/// is the one that does not depend on that.
fn pin_nets(b: &ExtractedBoard) -> BTreeMap<(String, String), String> {
    let names: HashMap<i64, &str> = b.nets.iter().map(|n| (n.id, n.name.as_str())).collect();
    let mut out = BTreeMap::new();
    for c in &b.components {
        for p in &c.pins {
            if let Some(net) = p.net.and_then(|id| names.get(&id)) {
                out.insert((c.reference.clone(), p.number.clone()), (*net).to_string());
            }
        }
    }
    out
}

fn pad_count(b: &ExtractedBoard) -> usize {
    b.components.iter().map(|c| c.pins.len()).sum()
}

fn refdes(b: &ExtractedBoard) -> BTreeSet<&str> {
    b.components.iter().map(|c| c.reference.as_str()).collect()
}

fn net_names(b: &ExtractedBoard) -> BTreeSet<&str> {
    b.nets.iter().map(|n| n.name.as_str()).collect()
}

/// The net PARTITION over the pins the two readings share: every unordered pair
/// of shared pins must be same-net in both, or different-net in both. This is
/// the check that does not care what the nets are called, only what is wired to
/// what — the property a simulation actually depends on.
///
/// `expect_unshared` names the components whose pad NAMES a producer is known to
/// change on export (see [`ODB_RENAMED_PADS`]); a pin outside that list failing
/// to appear in both readings is a fault.
fn assert_same_partition(
    label: &str,
    a: &ExtractedBoard,
    b: &ExtractedBoard,
    expect_unshared: &[&str],
) {
    let (pa, pb) = (pin_nets(a), pin_nets(b));
    let unexpected: Vec<&(String, String)> = pa
        .keys()
        .chain(pb.keys())
        .filter(|k| !(pa.contains_key(*k) && pb.contains_key(*k)))
        .filter(|k| !expect_unshared.contains(&k.0.as_str()))
        .collect();
    assert!(
        unexpected.is_empty(),
        "{label}: {} netted pin(s) appear in only one reading and are not a known \
         exporter rename: {unexpected:?}",
        unexpected.len()
    );
    let shared: Vec<&(String, String)> = pa.keys().filter(|k| pb.contains_key(*k)).collect();
    let mut disagreements = Vec::new();
    for i in 0..shared.len() {
        for j in (i + 1)..shared.len() {
            let (x, y) = (shared[i], shared[j]);
            let together_a = pa[x] == pa[y];
            let together_b = pb[x] == pb[y];
            if together_a != together_b {
                disagreements.push(format!(
                    "{}.{} and {}.{}: {} in one reading ({} / {}), {} in the other",
                    x.0,
                    x.1,
                    y.0,
                    y.1,
                    if together_a { "same net" } else { "different nets" },
                    pa[x],
                    pa[y],
                    if together_b { "same net" } else { "different nets" },
                ));
            }
        }
    }
    assert!(
        disagreements.is_empty(),
        "{label}: {} pin pair(s) are wired differently: {}",
        disagreements.len(),
        disagreements[..disagreements.len().min(5)].join("; ")
    );
}

/// Components whose pads cannot be compared BY NAME, derived from the native
/// reading rather than hard-coded.
///
/// Two cases, both real on watchy:
///
/// * a pad with **no name** — a mechanical/NPTH hole or an unnumbered connector
///   shield. KiCad's exchange exporters will not write a nameless pad, so they
///   synthesise `PAD0`, `NPTH1`, and the name cannot round-trip.
/// * **two pads under one name** — a switch with two pads both called `1`. Any
///   `(refdes, pad name)` keyed comparison collapses them.
///
/// Pad COUNTS and the net partition over the *named* pads are still compared for
/// these components; only the name equality is skipped.
fn unkeyable_pads(b: &ExtractedBoard) -> BTreeSet<&str> {
    b.components
        .iter()
        .filter(|c| {
            let names: Vec<&str> = c.pins.iter().map(|p| p.number.as_str()).collect();
            let unique: BTreeSet<&str> = names.iter().copied().collect();
            names.iter().any(|n| n.is_empty()) || unique.len() != names.len()
        })
        .map(|c| c.reference.as_str())
        .collect()
}

fn pads_per_ref(b: &ExtractedBoard) -> BTreeMap<&str, usize> {
    b.components
        .iter()
        .map(|c| (c.reference.as_str(), c.pins.len()))
        .collect()
}

/// Pin-name set per component, so a reading that loses or invents a pad is
/// caught even where the net partition happens to survive.
fn pins_by_ref(b: &ExtractedBoard) -> BTreeMap<&str, BTreeSet<&str>> {
    b.components
        .iter()
        .map(|c: &Component| {
            (
                c.reference.as_str(),
                c.pins.iter().map(|p| p.number.as_str()).collect(),
            )
        })
        .collect()
}

/// Every board read three ways, with the NATIVE counts stated so a regression
/// that shifts all three readings together still fails.
const AGREEING: &[(&str, usize, usize, usize)] = &[
    // (board, components, nets, pads)
    ("blinky", 5, 5, 14),
    ("boot_gate", 3, 4, 10),
    ("power_resistor", 1, 2, 2),
    ("tolerance_divider", 2, 3, 4),
    ("watchy", 82, 84, 312),
];

/// The one place KiCad's exporters lose information that no reader can recover,
/// measured rather than assumed.
///
/// **`IPC_SPLIT_INSTANCES`** — where a board places two footprints under one
/// reference designator (watchy's TP4 and TP5 are test points placed twice), the
/// IPC-2581 exporter *renames* the second instance `TP4_1`, so the document
/// states 84 distinct designators for an 82-part board. Nothing in the file
/// marks `TP4_1` as TP4's second instance, and folding a `_<n>` suffix back
/// would corrupt a part genuinely designated `R1_1`, so the reading keeps both.
/// The ODB++ exporter writes `TP4` twice and the reading merges them, which is
/// why the ODB++ column agrees exactly and the IPC-2581 column does not.
///
/// **`ODB_RENAMED_PADS`** — KiCad de-duplicates footprints by pad geometry when
/// it writes package definitions, so blinky's 2-pin LED is given the axial
/// resistor's package. The IPC-2581 export still names the LED's own pads in its
/// `<PinRef>`s (`A`/`K`, which the reader detects as a wrong package reference),
/// but the ODB++ export takes the toeprint names from the borrowed package and
/// writes `1`/`2`. Nothing in the ODB++ job contradicts itself, so the rename is
/// undetectable there; the pads and their nets are right, only the names are not.
const IPC_SPLIT_INSTANCES: &[(&str, &str)] = &[("watchy", "TP4_1"), ("watchy", "TP5_1")];
const ODB_RENAMED_PADS: &[(&str, &str)] = &[("blinky", "D1")];

fn split_instances(board: &str) -> Vec<&'static str> {
    IPC_SPLIT_INSTANCES
        .iter()
        .filter(|(b, _)| *b == board)
        .map(|(_, r)| *r)
        .collect()
}

fn renamed_pads(board: &str) -> Vec<&'static str> {
    ODB_RENAMED_PADS
        .iter()
        .filter(|(b, _)| *b == board)
        .map(|(_, r)| *r)
        .collect()
}

#[test]
fn the_native_reading_has_the_counts_the_agreement_tests_assert() {
    for (board, comps, nets, pads) in AGREEING {
        let b = native(board);
        assert_eq!(b.components.len(), *comps, "{board}: components");
        assert_eq!(b.nets.len(), *nets, "{board}: nets");
        assert_eq!(pad_count(&b), *pads, "{board}: pads");
    }
}

#[test]
fn ipc2581_agrees_with_the_native_reading_of_the_same_board() {
    for (board, comps, nets, pads) in AGREEING {
        let truth = native(board);
        let out = ipc(board);
        let got = &out.board;
        let split = split_instances(board);
        assert_eq!(
            got.components.len(),
            *comps + split.len(),
            "{board} IPC-2581: components (native {comps} plus {} exporter-split \
             instance(s) {split:?})",
            split.len()
        );
        assert_eq!(got.nets.len(), *nets, "{board} IPC-2581: nets");
        assert_eq!(pad_count(got), *pads, "{board} IPC-2581: pads");
        assert_eq!(net_names(got), net_names(&truth), "{board} IPC-2581: net names");
        let mut extra = refdes(got);
        for r in &split {
            assert!(extra.remove(r), "{board}: {r} should be an extra designator");
        }
        assert_eq!(extra, refdes(&truth), "{board} IPC-2581: designators");
        assert_eq!(out.stats.revision, "C");

        let mut exceptions = unkeyable_pads(&truth);
        exceptions.extend(split.iter().copied());
        // A split instance's pads are counted under the renamed designator, so
        // the per-component count is compared only for the parts the exporter
        // left alone.
        let (gp, tp) = (pads_per_ref(got), pads_per_ref(&truth));
        for (r, want) in &tp {
            if split.contains(r) || split.iter().any(|s| s.starts_with(&format!("{r}_"))) {
                continue;
            }
            assert_eq!(
                gp.get(r),
                Some(want),
                "{board} IPC-2581: {r} pad count"
            );
        }
        let names = pins_by_ref(got);
        let want_names = pins_by_ref(&truth);
        for (r, want) in &want_names {
            if exceptions.contains(r) || split.iter().any(|s| s.starts_with(&format!("{r}_"))) {
                continue;
            }
            assert_eq!(
                names.get(r),
                Some(want),
                "{board} IPC-2581: {r} pad names"
            );
        }
        let exceptions: Vec<&str> = exceptions.into_iter().collect();
        assert_same_partition(&format!("{board} IPC-2581"), &truth, got, &exceptions);
    }
}

#[test]
fn odbpp_agrees_with_the_native_reading_of_the_same_board() {
    for (board, comps, nets, pads) in AGREEING {
        let truth = native(board);
        let out = odb(board);
        let got = &out.board;
        let renamed = renamed_pads(board);
        assert_eq!(got.components.len(), *comps, "{board} ODB++: components");
        assert_eq!(got.nets.len(), *nets, "{board} ODB++: nets");
        assert_eq!(pad_count(got), *pads, "{board} ODB++: pads");
        assert_eq!(refdes(got), refdes(&truth), "{board} ODB++: designators");
        assert_eq!(net_names(got), net_names(&truth), "{board} ODB++: net names");
        assert_eq!(out.stats.pads, *pads, "{board} ODB++: pad accounting");
        // Pads per component: counts always, names except where a pad cannot be
        // keyed by name or the exporter renamed it.
        assert_eq!(
            pads_per_ref(got),
            pads_per_ref(&truth),
            "{board} ODB++: pads per component"
        );
        let mut exceptions = unkeyable_pads(&truth);
        exceptions.extend(renamed.iter().copied());
        let (gp, tp) = (pins_by_ref(got), pins_by_ref(&truth));
        for (r, want) in &tp {
            if exceptions.contains(r) {
                continue;
            }
            let have = gp.get(r).unwrap_or_else(|| panic!("{board}: {r} is missing"));
            assert_eq!(have, want, "{board} ODB++: {r} pad names");
        }
        let exceptions: Vec<&str> = exceptions.into_iter().collect();
        assert_same_partition(&format!("{board} ODB++"), &truth, got, &exceptions);
    }
}

#[test]
fn the_two_exchange_readings_of_one_board_agree_with_each_other() {
    for (board, ..) in AGREEING {
        let a = ipc(board).board;
        let b = odb(board).board;
        let mut exceptions = renamed_pads(board);
        exceptions.extend(split_instances(board));
        let mut extra = refdes(&a);
        for r in split_instances(board) {
            extra.remove(r);
        }
        assert_eq!(extra, refdes(&b), "{board}: designators");
        assert_eq!(net_names(&a), net_names(&b), "{board}: net names");
        assert_same_partition(
            &format!("{board} IPC-2581 vs ODB++"),
            &a,
            &b,
            &exceptions,
        );
    }
}

#[test]
fn a_renamed_pad_still_lands_on_the_right_net() {
    // blinky's D1 reads as A/K natively and in IPC-2581 but as 1/2 in ODB++.
    // Whatever the pads are called, the LED's anode must be on LED_A and its
    // cathode on GND: what the names cost is readability, not correctness.
    let o = odb("blinky").board;
    let d1 = o.component("D1").expect("D1");
    let by_net: BTreeSet<&str> = d1
        .pins
        .iter()
        .filter_map(|p| p.net.and_then(|id| o.net(id)).map(|n| n.name.as_str()))
        .collect();
    assert_eq!(
        by_net,
        ["GND", "LED_A"].into_iter().collect::<BTreeSet<_>>(),
        "the renamed pads are still on the right two nets"
    );
    let t = native("blinky");
    let td1 = t.component("D1").expect("D1");
    let t_by_net: BTreeSet<&str> = td1
        .pins
        .iter()
        .filter_map(|p| p.net.and_then(|id| t.net(id)).map(|n| n.name.as_str()))
        .collect();
    assert_eq!(by_net, t_by_net);
    // And the IPC-2581 reading keeps the real names AND says the package was wrong.
    let i = ipc("blinky");
    let names: BTreeSet<&str> = i
        .board
        .component("D1")
        .expect("D1")
        .pins
        .iter()
        .map(|p| p.number.as_str())
        .collect();
    assert_eq!(names, ["A", "K"].into_iter().collect::<BTreeSet<_>>());
    assert!(
        i.stats
            .disagreements
            .iter()
            .any(|d| d.contains("package reference is wrong") && d.contains("D1")),
        "the borrowed package must be reported: {:?}",
        i.stats.disagreements
    );
}

#[test]
fn values_and_footprints_survive_the_round_trip() {
    // The IR's `value` is what model binding keys on, so a value lost in
    // translation is a board that cannot be simulated.
    let truth = native("blinky");
    for reading in [ipc("blinky").board, odb("blinky").board] {
        for t in &truth.components {
            let got = reading
                .component(&t.reference)
                .unwrap_or_else(|| panic!("{} is missing", t.reference));
            assert_eq!(
                got.value, t.value,
                "{}: value must survive the export",
                t.reference
            );
            assert!(
                !got.footprint.is_empty(),
                "{}: the footprint must survive",
                t.reference
            );
        }
    }
}

#[test]
fn ipc2581_recovers_populate_flags_and_odbpp_cannot() {
    // KiCad's IPC-2581 exporter writes `populate="false"` for the three
    // `exclude_from_bom` footprints on watchy; its ODB++ exporter writes no
    // populate flag at all, so the ODB++ reading cannot know. That asymmetry is
    // a fact about the exporters and is asserted rather than papered over.
    let i = ipc("watchy").board;
    let o = odb("watchy").board;
    let mut ipc_dnp: Vec<&str> = i
        .components
        .iter()
        .filter(|c| c.dnp)
        .map(|c| c.reference.as_str())
        .collect();
    ipc_dnp.sort_unstable();
    assert_eq!(
        ipc_dnp,
        vec!["TP4", "TP5"],
        "IPC-2581 carries the populate flag. Only two of watchy's three \
         `exclude_from_bom` footprints show up: the third is the display, which \
         is pad-less artwork and dropped. The exporter's split instances \
         (TP4_1/TP5_1) are absent from the BOM, so their flag is lost with them"
    );
    assert_eq!(
        o.components.iter().filter(|c| c.dnp).count(),
        0,
        "KiCad's ODB++ export carries no populate flag, so the ODB++ reading \
         must not invent one"
    );
    // KiCad drives `populate` from `exclude_from_bom`, which is NOT the same as
    // its own `dnp` flag: the native reading marks none of these three DNP.
    let truth = native("watchy");
    assert_eq!(
        truth.components.iter().filter(|c| c.dnp).count(),
        0,
        "the native reading keys on KiCad's `dnp` attribute, which watchy does \
         not set on any footprint"
    );
}

#[test]
fn board_artwork_is_dropped_and_named_rather_than_counted_as_a_part() {
    // watchy's two pad-less placements are the sqfmi duck silkscreen (`G***`)
    // and the e-paper display's outline (`REF**`, its only bottom-side
    // placement). The native reader drops both as artwork; the exchange readers
    // must agree, and must say which ones went.
    let truth = native("watchy");
    assert!(
        truth.component("G***").is_none() && truth.component("REF**").is_none(),
        "the native reader drops pad-less placements"
    );
    let o = odb("watchy");
    let i = ipc("watchy");
    for (label, artwork) in [("ODB++", &o.stats.artwork), ("IPC-2581", &i.stats.artwork)] {
        let mut got = artwork.clone();
        got.sort();
        assert_eq!(
            got,
            vec!["G***".to_string(), "REF**".to_string()],
            "{label}: the dropped artwork must be named"
        );
    }
    assert!(o.board.component("G***").is_none());
    assert!(i.board.component("G***").is_none());
    // Every remaining part is on a side both readings agree about.
    for side in ["F.Cu", "B.Cu"] {
        let want = truth.components.iter().filter(|c| c.layer == side).count();
        for (label, b) in [("IPC-2581", &i.board), ("ODB++", &o.board)] {
            let got = b.components.iter().filter(|c| c.layer == side).count();
            // The IPC-2581 reading carries the two exporter-split instances.
            let allowance = if label == "IPC-2581" && side == "F.Cu" {
                split_instances("watchy").len()
            } else {
                0
            };
            assert_eq!(got, want + allowance, "{label}: components on {side}");
        }
    }
}

#[test]
fn the_readers_report_what_they_read_and_what_they_dropped() {
    let o = odb("watchy");
    assert_eq!(o.stats.step, "pcb");
    assert_eq!(o.stats.placement_source, odbpp::PlacementSource::ComponentLayers);
    assert!(
        o.stats.producer.contains("KiCad"),
        "the producing tool is recorded: {}",
        o.stats.producer
    );
    // Four copper layers plus the two drill layers watchy's matrix declares.
    let copper: Vec<&str> = o
        .stats
        .layers
        .iter()
        .filter(|l| l.layer_type != "DRILL")
        .map(|l| l.name.as_str())
        .collect();
    assert_eq!(copper, vec!["f.cu", "in1.cu", "in2.cu", "b.cu"]);
    assert!(
        o.stats.copper_features() > 1000,
        "the copper geometry is counted even though the IR cannot hold it: {}",
        o.stats.copper_features()
    );
    assert!(o.stats.drills > 0, "drills are counted: {}", o.stats.drills);
    assert_eq!(o.stats.netlist_nets, Some(84), "the CAD netlist agrees on 84 nets");
    assert!(
        o.stats.disagreements.is_empty(),
        "a KiCad-written job must be self-consistent: {:?}",
        o.stats.disagreements
    );

    let i = ipc("watchy");
    assert_eq!(i.stats.revision, "C");
    assert_eq!(i.stats.function_mode, "ASSEMBLY");
    assert_eq!(i.stats.net_source, ipc2581::NetSource::LayerFeature);
    assert_eq!(i.stats.copper_layers.len(), 4);
    assert!(
        i.stats.pads_per_layer.iter().map(|(_, n)| *n).sum::<usize>() > 0,
        "pads are accounted per layer"
    );
}

#[test]
fn both_formats_are_detected_by_content_through_the_registry() {
    use hauksbee_extract::reader::Registry;
    let registry = Registry::builtin();

    let xml = read_text(&fixtures().join("blinky.ipc2581.xml"));
    let claimed = registry
        .detect(xml.as_bytes(), None)
        .expect("IPC-2581 is claimed");
    assert_eq!(claimed.name(), "ipc-2581");
    assert_eq!(
        registry
            .read(xml.as_bytes(), None)
            .expect("and reads")
            .components
            .len(),
        5
    );

    let zip = std::fs::read(fixtures().join("blinky.odb.zip")).expect("read zip");
    let claimed = registry.detect(&zip, None).expect("ODB++ is claimed");
    assert_eq!(claimed.name(), "odb++");
    assert!(claimed.is_binary(), "an archive is binary input");
    // The byte entry point (the web upload path) reaches it too.
    let board = ExtractedBoard::from_auto_bytes(&zip)
        .expect("a binary reader claims the archive")
        .expect("and reads it");
    assert_eq!(board.components.len(), 5);

    // And neither claims the other's file, nor any other format's.
    for other in [
        &read_text(&boards_dir().join("blinky.kicad_pcb")).into_bytes()[..],
        b"<eagle version=\"9.0\"><drawing/></eagle>",
        b"317GND              R1    -1    D0472PA00X+019000Y+029450X0945Y0945R180S0\n",
    ] {
        let name = registry.detect(other, None).map(|r| r.name().to_string());
        assert!(
            name.as_deref() != Some("ipc-2581") && name.as_deref() != Some("odb++"),
            "the exchange readers must not claim {name:?} input"
        );
    }
}

// ── Real third-party documents ────────────────────────────────────────────────

#[test]
fn a_real_stackup_only_revision_b_export_refuses_and_says_why() {
    // github.com/akashlevy/ember-pcb @ 825ca310, ember-pcb.ipc.xml (MIT).
    let text = read_text(&fixtures().join("thirdparty").join("ember-pcb.revB.xml"));
    assert!(
        ipc2581::looks_like_ipc2581(text.as_bytes()),
        "it IS an IPC-2581 document and must be recognised as one"
    );
    let err = ipc2581::extract(&text).expect_err("but it carries no design");
    let msg = err.to_string();
    assert!(msg.contains("revision B"), "the revision is named: {msg}");
    assert!(msg.contains("places no components"), "got: {msg}");
    assert!(
        msg.contains("stackup of 25 layer(s)"),
        "what it DOES carry is named: {msg}"
    );
    assert!(
        msg.contains("CadData"),
        "and what was missing is named: {msg}"
    );
}

#[test]
fn a_real_allegro_bom_only_revision_c_export_refuses_and_says_why() {
    // github.com/sjgallagher2/ipc2581 @ befdfb02, the IPC-2581 Consortium's
    // testcase10 BOM member (MIT).
    let text = read_text(&fixtures().join("thirdparty").join("testcase10-bom.revC.xml"));
    assert!(ipc2581::looks_like_ipc2581(text.as_bytes()));
    let err = ipc2581::extract(&text).expect_err("a BOM is not a design");
    let msg = err.to_string();
    assert!(msg.contains("revision C"), "got: {msg}");
    assert!(msg.contains("FunctionMode BOM"), "got: {msg}");
    assert!(
        msg.contains("BOM with 56 reference designator(s)"),
        "the BOM it does carry is counted: {msg}"
    );
    assert!(msg.contains("bom-only"), "and the fix is named: {msg}");
}

/// The real Valor NPI job in the shared corpus: a Mentor PowerPCB design with
/// 813 components and 643 nets, in INCH units, with the placement on the
/// component layers and `;attrs;sysattrs` tails. There is no native reading of
/// this board to compare against, so what is asserted is INTERNAL consistency:
/// the job's own CAD netlist must agree with its EDA data, every pad must name a
/// net the EDA data declares, and the reader must find the whole placement.
#[test]
fn the_real_valor_npi_job_reads_and_is_internally_consistent() {
    let Some(root) = hauksbee_testkit::corpus_dir(env!("CARGO_MANIFEST_DIR")) else {
        eprintln!("board-corpus not present; skipping the real ODB++ job");
        return;
    };
    let job = root.join("odbpp").join("valor-npi-sample-design.tgz");
    if !job.is_file() {
        assert!(
            std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_err(),
            "HAUKSBEE_REQUIRE_CORPUS set but {} is absent",
            job.display()
        );
        eprintln!("{} not present; skipping", job.display());
        return;
    }
    let bytes = std::fs::read(&job).expect("read the corpus job");
    assert!(
        odbpp::looks_like_odbpp_archive(&bytes),
        "a real .tgz job must be sniffed as ODB++"
    );
    let out = odbpp::from_odbpp_archive(&bytes).expect("the real job reads");

    assert_eq!(out.board.components.len(), 813, "440 top + 373 bottom");
    assert_eq!(out.board.nets.len(), 643, "644 NET records less $NONE$");
    assert_eq!(out.stats.pads, 2811, "1921 top + 890 bottom toeprints");
    assert_eq!(out.stats.step, "step");
    assert_eq!(
        out.stats.placement_source,
        odbpp::PlacementSource::ComponentLayers
    );
    assert!(
        out.stats.producer.contains("PADS-POWERPCB"),
        "the producing tool is recorded: {}",
        out.stats.producer
    );
    assert_eq!(out.stats.netlist_nets, Some(643));
    // INCH units: the board is inches across, so a reading that forgot to scale
    // would put everything inside a 10 mm square.
    let xs: Vec<f64> = out
        .board
        .components
        .iter()
        .filter_map(|c| c.position.map(|(x, _, _)| x))
        .collect();
    let span = xs.iter().cloned().fold(f64::MIN, f64::max)
        - xs.iter().cloned().fold(f64::MAX, f64::min);
    assert!(
        span > 100.0,
        "INCH coordinates must be scaled to mm; the placement spans only {span:.1} mm"
    );
    // Every pad's net must be a net the job declares.
    let ids: BTreeSet<i64> = out.board.nets.iter().map(|n| n.id).collect();
    for c in &out.board.components {
        for p in &c.pins {
            if let Some(id) = p.net {
                assert!(
                    ids.contains(&id),
                    "{}.{} is on undeclared net {id}",
                    c.reference,
                    p.number
                );
            }
        }
    }
    // The lint the whole pipeline runs must not find a dangling net id.
    assert!(
        out.board.lint().undeclared_nets.is_empty(),
        "a real job must produce a lint-clean net graph"
    );
    assert!(
        out.stats.disagreements.is_empty(),
        "this job is self-consistent; any disagreement is a reader bug: {:?}",
        out.stats.disagreements
    );
}
