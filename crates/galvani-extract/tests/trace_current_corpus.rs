//! Corpus-gated trace-current sweep on the LumenPnP motherboard (the round-3
//! motor-driver target). This pins the honest result behind the IPC-2221
//! trace-current check: the high-current motor supply is a copper POUR (out of
//! the discrete-segment check's reach, correctly skipped), and the discrete
//! TMC2226 coil traces that ARE routed are adequately sized for the cited coil
//! current, so the check fires nothing. If a future change made the check flag a
//! poured rail's thin pad-stub, or miscompute the coil-trace ampacity, this
//! test would catch it.
//!
//! Skipped when the corpus is absent; `GALVANI_REQUIRE_CORPUS=1` makes absence a
//! hard fail so it cannot vacuously pass on a runner that should have the corpus.

use std::collections::HashMap;
use std::path::PathBuf;

use galvani_extract::{
    audit_trace_currents, net_copper_from_root, CopperKind, TraceAudit,
};

fn corpus_famous() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../board-corpus/famous");
    if p.exists() {
        Some(p)
    } else if std::env::var("GALVANI_REQUIRE_CORPUS").is_ok() {
        panic!("GALVANI_REQUIRE_CORPUS set but board-corpus/famous is absent");
    } else {
        None
    }
}

#[test]
fn lumenpnp_motor_supply_is_poured_and_coil_traces_are_adequately_sized() {
    let Some(famous) = corpus_famous() else {
        eprintln!("corpus absent; skipping LumenPnP trace-current sweep");
        return;
    };
    let path = famous.join("lumenpnp/mobo/mobo.kicad_pcb");
    let Ok(text) = std::fs::read_to_string(&path) else {
        if std::env::var("GALVANI_REQUIRE_CORPUS").is_ok() {
            panic!("LumenPnP mobo missing under required corpus");
        }
        eprintln!("LumenPnP mobo missing; skipping");
        return;
    };
    let doc = forge_sexpr::parse(&text).expect("kicad_pcb parses");
    let copper = net_copper_from_root(doc.root().expect("root"));

    // The motor supply VDC distributes via copper pours: it must be classified
    // Poured (13 zones on the real board), so the discrete-width check never
    // mistakes its thin pad-entry stubs for the conductor.
    let vdc = copper
        .iter()
        .find(|n| n.name == "VDC")
        .expect("VDC net present");
    assert_eq!(
        vdc.kind,
        CopperKind::Poured,
        "VDC (motor supply, 6x TMC2226) is poured; its true cross-section is the \
         plane, not its 0.25 mm stubs"
    );
    assert!(vdc.zone_count >= 1, "VDC carries at least one pour zone");

    // The per-motor coil phase nets ARE routed as discrete traces; the board
    // routes them at 1.5 mm (IPC-2221 external 1 oz, 10 C ~ 3.2 A), comfortably
    // above the TMC2226's 1.4 A RMS coil rating. Confirm a representative coil
    // net is Traces and adequately wide.
    let coil = copper
        .iter()
        .find(|n| n.name.ends_with("/A1") && n.kind == CopperKind::Traces)
        .expect("a motor coil A1 trace net present");
    let min_w = coil.min_trace_width_mm.expect("coil net has tracks");
    assert!(
        min_w >= 1.0,
        "coil trace {} min width {min_w} mm should be a real power trace, not a stub",
        coil.name
    );

    // The audit, given the TMC2226's cited 1.4 A RMS coil current on EVERY
    // trace-routed coil net, fires nothing: the traces carry it with margin, and
    // the poured rails are out of scope. This is the honest negative.
    let mut cited: HashMap<String, (f64, String)> = HashMap::new();
    for nc in &copper {
        // Attribute the coil current to the routed coil phase nets only.
        if nc.kind == CopperKind::Traces
            && (nc.name.ends_with("/A1")
                || nc.name.ends_with("/A2")
                || nc.name.ends_with("/B1")
                || nc.name.ends_with("/B2"))
        {
            cited.insert(
                nc.name.clone(),
                (1.4, "TMC2226 1.4 A RMS coil (datasheet)".to_string()),
            );
        }
    }
    assert!(!cited.is_empty(), "found coil nets to attribute current to");
    let findings = audit_trace_currents(&copper, &cited, &TraceAudit::default());
    assert!(
        findings.is_empty(),
        "LumenPnP coil traces (1.5 mm) carry the 1.4 A coil current with margin; \
         the trace-current audit must be silent. Unexpected: {findings:?}"
    );
}
