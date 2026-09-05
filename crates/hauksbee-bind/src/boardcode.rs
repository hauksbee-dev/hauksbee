//! Board-as-Code compile / decompile: the text half of the edit -> simulate
//! loop.
//!
//! [`decompile_board_to_code`] is the CODE side: a `.kicad_pcb` becomes
//! editable Board-as-Code text via `forge_codegen::to_code`.
//! [`code_to_board_text`] is the recompile: text becomes a valid `.kicad_pcb`
//! (`Program::parse` -> `Program::build` -> `Pcb::emit`). [`load_code`] reads a
//! `.board` file or a directory holding exactly one. The simulate half
//! (`check_code`, `CheckReport`) lives upstairs in `hauksbee_engine::boardcode`
//! because it drives the co-simulation engine; [`board_input`](crate::board_input)
//! needs only this half to accept `.board` files as an input format.

use std::path::Path;

use forge_codegen::dsl::{Comp, Pad, Stmt};
use forge_codegen::{to_code, Program};
use forge_model::Pcb;
use hauksbee_extract::ExtractedBoard;

/// Decompile a parsed-or-raw `.kicad_pcb` text into editable Board-as-Code.
pub fn decompile_board_to_code(kicad_pcb_text: &str) -> anyhow::Result<String> {
    let extracted = ExtractedBoard::from_kicad_pcb(kicad_pcb_text)?;
    ensure_boardcode_serializable(&extracted)?;
    let pcb = Pcb::parse(kicad_pcb_text).map_err(|e| anyhow::anyhow!("parsing board: {e:?}"))?;
    Ok(to_code(&pcb))
}

/// Decompile *any* extracted board text into editable Board-as-Code.
///
/// `decompile_board_to_code` only accepts a `.kicad_pcb` (it goes straight
/// through `Pcb::parse`, which rejects a `.net`/IPC/Eagle file). This bridge
/// handles every text format the extractor understands: a `.kicad_pcb` keeps
/// the geometry-rich layout path (so blocks/clusters and pad geometry survive),
/// and everything else (KiCad `.net`, IPC-D-356, Eagle `.brd`, KiCad `.sch`)
/// goes through [`ExtractedBoard::from_auto`] + [`program_from_extracted`].
///
/// A `.net` carries pin *functions* (`pinfunction "A"`/`"K"`) that a layout
/// lacks. The Board-as-Code `pad` line has no role slot (it is a geometry +
/// net record), so the function is not re-emitted verbatim; what survives is
/// the pad *number*, which the binder's pin-role rule table maps back to a role
///. For a netlist with a generic diode the standard `1->K, 2->A`
/// numbering binds the part directly, with a guess-warning naming the rule.
pub fn decompile_any_to_code(board_text: &str) -> anyhow::Result<String> {
    let head: String = board_text.chars().take(512).collect();
    if head.contains("(kicad_pcb") {
        // Layout: keep the cluster-aware geometry decompiler.
        return decompile_board_to_code(board_text);
    }
    // Netlist / IPC / Eagle / schematic: extract, then emit flat code.
    let board = ExtractedBoard::from_auto(board_text)?;
    Ok(program_from_extracted(&board)?.emit())
}

/// Build an editable Board-as-Code [`Program`] directly from an
/// [`ExtractedBoard`].
///
/// This is the bridge that lets an extracted board (KiCad netlist, IPC-356,
/// Eagle) become editable code without a `.kicad_pcb` round-trip. Net ids are
/// resolved to names; each component becomes a singleton [`Stmt::Single`] so the
/// program is flat and every pad-net is explicit and editable. Pad geometry the
/// extractor does not carry is filled with sane SMD defaults (connectivity, not
/// geometry, is what matters for the simulate loop).
pub fn program_from_extracted(board: &ExtractedBoard) -> anyhow::Result<Program> {
    ensure_boardcode_serializable(board)?;
    let mut id_to_name = std::collections::HashMap::new();
    for n in &board.nets {
        id_to_name.insert(n.id, n.name.clone());
    }

    let mut body: Vec<Stmt> = Vec::new();
    // Declare nets in board order for a stable table.
    for n in &board.nets {
        if !n.name.is_empty() {
            body.push(Stmt::Net(n.name.clone()));
        }
    }

    for c in &board.components {
        let (x, y, rot) = c.position.unwrap_or((0.0, 0.0, 0.0));
        let pads = c
            .pins
            .iter()
            .map(|p| {
                let net = p
                    .net
                    .and_then(|id| id_to_name.get(&id).cloned())
                    .filter(|s| !s.is_empty());
                let at = p.position.unwrap_or((0.0, 0.0));
                Pad {
                    number: p.number.clone(),
                    kind: "smd".to_string(),
                    shape: "rect".to_string(),
                    at,
                    size: (1.0, 1.0),
                    drill: None,
                    layers: vec!["F.Cu".to_string()],
                    net,
                }
            })
            .collect();
        body.push(Stmt::Single(Comp {
            reference: c.reference.clone(),
            // Board-as-Code recompiles to a `.kicad_pcb`, where a component's
            // identity is its FOOTPRINT (the extractor sets `footprint = lib_id`
            // of the footprint block). A netlist carries both a symbol lib_id
            // ("Device:D") and a footprint ("Diode_SMD:D_SOD-323"); the footprint
            // is what survives the round-trip and what the binder + pin-rule
            // table match on, so prefer it. Fall back to the symbol lib_id only
            // when no footprint is known.
            lib_id: if !c.footprint.is_empty() {
                c.footprint.clone()
            } else {
                c.lib_id.clone()
            },
            value: c.value.clone(),
            layer: if c.layer.is_empty() {
                "F.Cu".to_string()
            } else {
                c.layer.clone()
            },
            at: (x, y),
            rot,
            space: None,
            pads,
        }));
    }

    Ok(Program {
        version: 20241229,
        blocks: Vec::new(),
        body,
        outline: None,
    })
}

fn ensure_boardcode_serializable(board: &ExtractedBoard) -> anyhow::Result<()> {
    // The three-state assembled-component contract: the Board-as-Code DSL can
    // only express Present, so the two absent states must refuse loudly here.
    // Silently emitting either one would let recompilation turn an unknown or
    // unpopulated part into an ordinary fitted component.
    for c in &board.components {
        match hauksbee_extract::assembly::AssemblyState::of(c) {
            hauksbee_extract::assembly::AssemblyState::Present(_) => {}
            hauksbee_extract::assembly::AssemblyState::DnpAbsent(_) => {
                anyhow::bail!(
                    "cannot convert {} to Board-as-Code: its DNP state is not representable and recompilation would fit the part",
                    c.reference
                );
            }
            hauksbee_extract::assembly::AssemblyState::IdentityUnknown(refusal) => {
                anyhow::bail!(
                    "cannot convert {} to Board-as-Code without losing ambiguous identity evidence: {}",
                    c.reference,
                    refusal.reason()
                );
            }
        }
    }
    Ok(())
}

/// Recompile Board-as-Code text into a `.kicad_pcb` string.
///
/// Parses the DSL, interprets it into a [`Pcb`], and emits KiCad s-expression
/// text. Errors carry the offending line number from the DSL parser.
pub fn code_to_board_text(code: &str) -> anyhow::Result<String> {
    let prog = Program::parse(code).map_err(|e| anyhow::anyhow!("board code: {e}"))?;
    let pcb = prog.build();
    Ok(pcb.emit())
}

/// Load Board-as-Code from a path that is either a `.board` file or a directory
/// containing exactly one `.board` file.
pub fn load_code(path: &Path) -> anyhow::Result<String> {
    if path.is_dir() {
        let mut found = None;
        for entry in std::fs::read_dir(path)? {
            let p = entry?.path();
            if p.extension().map(|e| e == "board").unwrap_or(false) {
                if found.is_some() {
                    anyhow::bail!(
                        "{} contains more than one .board file; pass the file directly",
                        path.display()
                    );
                }
                found = Some(p);
            }
        }
        let f =
            found.ok_or_else(|| anyhow::anyhow!("no .board file found in {}", path.display()))?;
        Ok(std::fs::read_to_string(f)?)
    } else {
        std::fs::read_to_string(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                // Suggestion adapts to what is actually runnable: the bundled
                // example only exists inside a source checkout; a bare binary
                // gets told how to MAKE a .board from any board file instead.
                let checkout = std::path::Path::new("examples/board-as-code/blinky.board");
                let suggestion = if checkout.exists() {
                    "hauksbee check-code examples/board-as-code/blinky.board".to_string()
                } else {
                    "hauksbee to-code <your .kicad_pcb> --out my.board   # create one from any board"
                        .to_string()
                };
                anyhow::anyhow!(
                    "no board-as-code file at '{}'. Check the path, or:\n  {suggestion}",
                    path.display()
                )
            } else {
                anyhow::anyhow!("reading '{}': {e}", path.display())
            }
        })
    }
}
