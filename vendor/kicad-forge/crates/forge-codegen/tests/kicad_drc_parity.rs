//! Real-KiCad proof for the library copy-through (cold-drive defects 9/10).
//!
//! Builds a board whose components use STOCK KiCad footprints with pad
//! geometry read from the installed library itself, emits it twice (library
//! copy-through disabled and enabled), and runs `kicad-cli pcb drc` on both.
//! The minimal emission draws one `lib_footprint_mismatch` per footprint; the
//! dressed emission must draw none.
//!
//! Self-skipping: when `kicad-cli` or the stock libraries are absent (CI, a
//! fresh clone with no KiCad) the test reports why and passes vacuously. The
//! hermetic equivalents live in `src/fplib.rs` against a fixture library.

use forge_codegen::dsl::{Comp, Pad, Program, Stmt};
use forge_codegen::FootprintLib;
use std::path::PathBuf;
use std::process::Command;

fn kicad_cli() -> Option<PathBuf> {
    // PATH first, then the standard macOS app bundle.
    if let Ok(out) = Command::new("kicad-cli").arg("version").output() {
        if out.status.success() {
            return Some(PathBuf::from("kicad-cli"));
        }
    }
    let mac = PathBuf::from("/Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli");
    if mac.is_file() {
        return Some(mac);
    }
    None
}

/// Read pad geometry for a stock footprint from the discovered library, so the
/// board's pads match the library exactly (as a decompiled real board would).
fn comp_from_library(
    lib: &mut FootprintLib,
    reference: &str,
    lib_id: &str,
    value: &str,
    at: (f64, f64),
    rot: f64,
    nets: &[&str],
) -> Option<Comp> {
    let fp = lib.resolve(lib_id)?;
    let mut pads = Vec::new();
    for (i, pad) in fp.find_all("pad").enumerate() {
        let number = pad.arg_value(0)?;
        let kind = pad.arg_value(1)?;
        let shape = pad.arg_value(2)?;
        let atl = pad.find("at")?;
        let szl = pad.find("size")?;
        pads.push(Pad {
            number,
            kind,
            shape,
            at: (atl.arg_f64(0)?, atl.arg_f64(1)?),
            size: (szl.arg_f64(0)?, szl.arg_f64(1)?),
            drill: pad.find("drill").and_then(|d| d.arg_f64(0)),
            layers: pad
                .find("layers")
                .map(|l| {
                    l.children
                        .iter()
                        .skip(1)
                        .filter_map(|c| c.as_token())
                        .map(|t| t.value())
                        .collect()
                })
                .unwrap_or_default(),
            net: Some(nets[i % nets.len()].to_string()),
        });
    }
    if pads.is_empty() {
        return None;
    }
    Some(Comp {
        reference: reference.to_string(),
        lib_id: lib_id.to_string(),
        value: value.to_string(),
        layer: "F.Cu".to_string(),
        at,
        rot,
        space: None,
        pads,
    })
}

fn drc_mismatch_count(cli: &PathBuf, board_text: &str, dir: &PathBuf, name: &str) -> usize {
    let board = dir.join(format!("{name}.kicad_pcb"));
    let report = dir.join(format!("{name}.drc.json"));
    std::fs::write(&board, board_text).expect("write board");
    let out = Command::new(cli)
        .args([
            "pcb",
            "drc",
            "--format",
            "json",
            "-o",
            report.to_str().unwrap(),
            board.to_str().unwrap(),
        ])
        .output()
        .expect("kicad-cli drc runs");
    // kicad-cli exits nonzero when violations exist; only a missing report is
    // a real failure.
    let json = std::fs::read_to_string(&report).unwrap_or_else(|e| {
        panic!(
            "no DRC report for {name}: {e}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    json.matches("lib_footprint_mismatch").count()
}

#[test]
fn stock_footprints_pass_kicad_library_parity_drc_when_dressed() {
    let Some(cli) = kicad_cli() else {
        eprintln!("SKIP: kicad-cli not installed; hermetic fixture tests cover this path");
        return;
    };
    let mut lib = FootprintLib::discover();

    let comps = [
        comp_from_library(
            &mut lib,
            "U1",
            "Package_DIP:DIP-28_W7.62mm",
            "ATmega328P",
            (100.0, 80.0),
            0.0,
            &["N1", "N2", "N3", "N4"],
        ),
        // Rotated part: proves angle handling against the real DRC.
        comp_from_library(
            &mut lib,
            "R1",
            "Resistor_SMD:R_0603_1608Metric",
            "10k",
            (120.0, 85.0),
            90.0,
            &["N1", "N2"],
        ),
        comp_from_library(
            &mut lib,
            "C1",
            "Capacitor_SMD:C_0603_1608Metric",
            "100n",
            (126.0, 85.0),
            0.0,
            &["N3", "N4"],
        ),
    ];
    if comps.iter().any(|c| c.is_none()) {
        eprintln!("SKIP: stock KiCad footprint libraries not discoverable on this machine");
        return;
    }
    let mut body: Vec<Stmt> = ["N1", "N2", "N3", "N4"]
        .iter()
        .map(|n| Stmt::Net(n.to_string()))
        .collect();
    body.extend(comps.into_iter().flatten().map(Stmt::Single));
    let prog = Program {
        version: 20241229,
        blocks: Vec::new(),
        body,
        outline: None,
    };

    let dir = std::env::temp_dir().join(format!("hauksbee_drc_parity_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");

    let minimal = prog
        .build_with_library(&mut FootprintLib::disabled())
        .emit();
    let dressed = prog.build_with_library(&mut lib).emit();

    let before = drc_mismatch_count(&cli, &minimal, &dir, "minimal");
    let after = drc_mismatch_count(&cli, &dressed, &dir, "dressed");
    eprintln!("lib_footprint_mismatch: minimal={before}, dressed={after}");

    assert!(
        before >= 3,
        "the minimal emission should mismatch every footprint (got {before})"
    );
    assert_eq!(
        after, 0,
        "the dressed emission must pass KiCad's library-parity check"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
