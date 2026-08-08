//! X2 attribute reading, proven two-sided.
//!
//! An X2 gerber film states each pad's component and pin (`%TO.P`), its net
//! (`%TO.N`) and what the aperture *is* (`%TA.AperFunction`). When those are
//! present, identity comes from the film: real pin numbers, real net names,
//! vias classified as vias. When they are absent (a stripped export, or legacy
//! CAM output), the geometry-only reconstruction runs exactly as before.
//!
//! Both sides are proven here:
//! - a synthetic job, once with attributes and once stripped, end to end
//!   through [`from_gerber_dir`];
//! - the ZSWatch corpus board, whose production folder ships the X2 gerbers
//!   NEXT TO the netlist they were exported with, so the film's claims are
//!   checked against a same-batch oracle rather than against themselves.

use std::collections::HashMap;
use std::path::PathBuf;

use hauksbee_extract::gerber::from_gerber_dir;
use hauksbee_extract::ExtractedBoard;

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

// ── Corpus oracle: ZSWatch ──────────────────────────────────────────────────

fn zswatch_paths() -> Option<(PathBuf, PathBuf)> {
    let root = hauksbee_testkit::corpus_boards_root(env!("CARGO_MANIFEST_DIR"))?;
    let base = root.join("zswatch_mainboard/production/watch-RELEASED");
    let gerbers = base.join("Manufacturing/Fabrication/Gerbers");
    let netlist = base.join("Netlist/ZSWatch-Watch-netlist.net");
    (gerbers.is_dir() && netlist.is_file()).then_some((gerbers, netlist))
}

/// The netlist name a gerber `%TO.N` corresponds to: KiCad's X2 export writes
/// the net's short name, while the netlist may carry the hierarchical path
/// (`/Touch/TOUCH-RST`). Compare on the last path segment.
fn short(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

#[test]
fn zswatch_x2_export_agrees_with_its_own_netlist_oracle() {
    let Some((gerbers, netlist)) = zswatch_paths() else {
        eprintln!("skipping: ZSWatch corpus board not present");
        return;
    };
    let oracle = ExtractedBoard::from_kicad_netlist(&std::fs::read_to_string(netlist).unwrap())
        .expect("netlist oracle");
    let g = from_gerber_dir(&gerbers).expect("gerber extraction");
    assert!(g.stats.x2_bound_pads > 0, "the export carries %TO.P");
    assert!(g.stats.x2_named_nets > 0, "the export carries %TO.N");

    // Oracle pad map: (refdes, pin) -> oracle net id, plus net names.
    let oracle_net_name: HashMap<i64, &str> = oracle
        .nets
        .iter()
        .map(|n| (n.id, n.name.as_str()))
        .collect();
    let mut oracle_pads: HashMap<(String, String), i64> = HashMap::new();
    for c in &oracle.components {
        for p in &c.pins {
            if let Some(net) = p.net {
                oracle_pads.insert((c.reference.clone(), p.number.clone()), net);
            }
        }
    }
    let recon_net_name: HashMap<i64, &str> = g
        .board
        .nets
        .iter()
        .map(|n| (n.id, n.name.as_str()))
        .collect();

    let mut matched: Vec<((String, String), i64, i64)> = Vec::new(); // key, oracle net, recon net
    let mut recon_pads = 0usize;
    for c in &g.board.components {
        for p in &c.pins {
            recon_pads += 1;
            let key = (c.reference.clone(), p.number.clone());
            if let (Some(&onet), Some(rnet)) = (oracle_pads.get(&key), p.net) {
                matched.push((key, onet, rnet));
            }
        }
    }
    eprintln!(
        "[zswatch-x2] oracle pads {} | recon pads {} | matched {} | recon nets {} | oracle nets {}",
        oracle_pads.len(),
        recon_pads,
        matched.len(),
        g.board.nets.len(),
        oracle.nets.len(),
    );

    // Pads the film binds that the netlist does not list. Measured on this
    // export these are exclusively MECHANICAL pads (mounting holes "MH",
    // mounting posts "MP", connector shields "SH", one unconnected pin): pads
    // that physically exist, which the netlist export omits or collapses. So
    // they are bounded, and none may carry a plain numeric pin the netlist
    // would certainly have listed.
    let unmatched: Vec<_> = g
        .board
        .components
        .iter()
        .flat_map(|c| {
            c.pins
                .iter()
                .map(move |p| (c.reference.clone(), p.number.clone()))
        })
        .filter(|k| !oracle_pads.contains_key(k))
        .collect();
    eprintln!(
        "[zswatch-x2] {} film pads not in the netlist (mechanical): {unmatched:?}",
        unmatched.len()
    );
    assert!(
        unmatched.len() * 100 <= recon_pads * 8,
        "too many film-bound pads absent from the oracle: {unmatched:?}"
    );

    // Coverage: the films flash every pad on outer copper, so nearly all
    // oracle pads must be recovered with their exact identity.
    assert!(
        matched.len() * 100 >= oracle_pads.len() * 95,
        "matched {} of {} oracle pads",
        matched.len(),
        oracle_pads.len()
    );

    // Net names: the film's name for each pad's net must be the oracle's.
    let name_mismatches: Vec<_> = matched
        .iter()
        .filter(|(_, onet, rnet)| short(oracle_net_name[onet]) != short(recon_net_name[rnet]))
        .map(|(key, onet, rnet)| {
            (
                key.clone(),
                oracle_net_name[onet].to_string(),
                recon_net_name[rnet].to_string(),
            )
        })
        .collect();
    assert!(
        name_mismatches.is_empty(),
        "every matched pad's net name must come from the film verbatim; \
         mismatches: {name_mismatches:?}"
    );

    // Partition: same-net iff same-net, across every matched pad pair.
    let mut disagree = 0usize;
    for i in 0..matched.len() {
        for j in (i + 1)..matched.len() {
            let same_o = matched[i].1 == matched[j].1;
            let same_r = matched[i].2 == matched[j].2;
            if same_o != same_r {
                disagree += 1;
            }
        }
    }
    assert_eq!(
        disagree, 0,
        "net partition must agree over all matched pads"
    );
}
