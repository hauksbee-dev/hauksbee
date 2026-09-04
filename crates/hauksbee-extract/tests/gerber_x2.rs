//! X2 attribute reading, proven two-sided.
//!
//! An X2 gerber film states each pad's component and pin (`%TO.P`), its net
//! (`%TO.N`) and what the aperture *is* (`%TA.AperFunction`). When those are
//! present, identity comes from the film: real pin numbers, real net names,
//! vias classified as vias. When they are absent (a stripped export, or legacy
//! CAM output), the geometry-only reconstruction runs exactly as before.
//!
//! A synthetic job is read twice, once with attributes and once stripped, end
//! to end through [`from_gerber_dir`].

use std::collections::HashMap;
use std::path::PathBuf;

use hauksbee_extract::gerber::from_gerber_dir;

/// A two-component film: R1 (two pads, nets VCC/SIG), C7 (two pads, SIG/GND),
/// a stitching via on VCC sitting INSIDE C7's footprint window, and a track.
/// The via is the classic trap: geometrically it looks like a third C7 pad.
const X2_FILM: &str = "\
%FSLAX46Y46*%
%MOMM*%
%TF.FileFunction,Copper,L1,Top*%
%TA.AperFunction,SMDPad,CuDef*%
%ADD10C,1.000000*%
%TD*%
%TA.AperFunction,ViaPad*%
%ADD11C,0.600000*%
%TD*%
%TA.AperFunction,Conductor*%
%ADD12C,0.250000*%
%TD*%
D10*
%TO.P,R1,2*%
%TO.N,SIG*%
X2000000Y0D03*
%TO.P,R1,1*%
%TO.N,VCC*%
X0Y0D03*
%TO.P,C7,1*%
%TO.N,SIG*%
X6000000Y0D03*
%TO.P,C7,2*%
%TO.N,GND*%
X8000000Y0D03*
%TD*%
D11*
%TO.N,VCC*%
X7000000Y1000000D03*
%TD*%
D12*
%TO.N,SIG*%
X2000000Y0D02*
X6000000Y0D01*
%TD*%
M02*
";

/// The same film with every X2 attribute line removed. Byte-identical drawing
/// commands; only the identity is gone.
fn stripped(film: &str) -> String {
    film.lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with("%TA") || t.starts_with("%TO") || t.starts_with("%TD"))
        })
        .map(|l| format!("{l}\n"))
        .collect()
}

fn job_dir(tag: &str, film: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hauksbee_x2_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("board-F_Cu.gbr"), film).unwrap();
    dir
}

#[test]
fn x2_job_binds_pins_nets_and_vias_from_the_film() {
    let dir = job_dir("attr", X2_FILM);
    let g = from_gerber_dir(&dir).expect("extract");
    let _ = std::fs::remove_dir_all(&dir);

    // No P&P file at all: both components exist purely from film identity.
    assert_eq!(g.stats.x2_film_components, 2);
    assert_eq!(g.stats.x2_bound_pads, 4);
    let by_ref: HashMap<&str, &hauksbee_extract::Component> = g
        .board
        .components
        .iter()
        .map(|c| (c.reference.as_str(), c))
        .collect();
    let r1 = by_ref["R1"];
    let c7 = by_ref["C7"];
    // Film pin numbers, not claim order (R1's pads were flashed 2 then 1).
    assert_eq!(
        r1.pins
            .iter()
            .map(|p| p.number.as_str())
            .collect::<Vec<_>>(),
        vec!["1", "2"]
    );
    // The via inside C7's window is NOT a third pad: the film called it a via.
    assert_eq!(c7.pins.len(), 2, "ViaPad must not inflate C7's pad count");

    // Net names come from the film, and the pad-to-net binding is exact.
    let net_name: HashMap<i64, &str> = g
        .board
        .nets
        .iter()
        .map(|n| (n.id, n.name.as_str()))
        .collect();
    let pin_net = |c: &hauksbee_extract::Component, num: &str| {
        net_name[&c
            .pins
            .iter()
            .find(|p| p.number == num)
            .unwrap()
            .net
            .unwrap()]
            .to_string()
    };
    assert_eq!(pin_net(r1, "1"), "VCC");
    assert_eq!(pin_net(r1, "2"), "SIG");
    assert_eq!(pin_net(c7, "1"), "SIG");
    assert_eq!(pin_net(c7, "2"), "GND");
    // R1-1 and the via share %TO.N,VCC with no copper joining them: the film
    // says one conductor, so they are one net.
    assert!(
        g.board.nets.iter().any(|n| n.name == "VCC"),
        "VCC must exist as one film-named net"
    );
}

#[test]
fn the_same_job_stripped_reproduces_the_geometry_only_reconstruction() {
    let dir = job_dir("bare", &stripped(X2_FILM));
    let g = from_gerber_dir(&dir).expect("extract");
    let _ = std::fs::remove_dir_all(&dir);

    // No X2, no P&P: no components can be bound, no net can be named, and the
    // via is indistinguishable from a pad (counted among the flashes). This is
    // exactly the pre-X2 behavior, preserved.
    assert_eq!(g.stats.x2_film_components, 0);
    assert_eq!(g.stats.x2_bound_pads, 0);
    assert_eq!(g.stats.x2_named_nets, 0);
    assert!(g.board.components.is_empty());
    assert!(g.board.nets.iter().all(|n| n.name.starts_with("NET_")));
    assert_eq!(
        g.stats.total_flashes, 5,
        "stripped of its attribute, the via is just another flash"
    );
    // Geometry unions only what touches: R1-2 -- track -- C7-1 is one net;
    // R1-1, C7-2 and the via are three more.
    assert_eq!(g.stats.n_nets, 4);
}
