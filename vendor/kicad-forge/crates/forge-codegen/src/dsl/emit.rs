//! Build an executable [`Program`] from a parsed board and render it to text.
//!
//! The grouping follows the repeat-detection [`Analysis`]: each multi-instance
//! cluster becomes a [`Block`] (function), each cluster instance becomes a
//! [`Stmt::Instance`], and everything else is a [`Stmt::Single`]. Pad geometry
//! and net assignments are read straight from the [`Pcb`] so the program is a
//! faithful, executable description of the board's connectivity.

use crate::cluster::Cluster;
use crate::dsl::model::{Block, Comp, Instance, Outline, Pad, Program, SlotSpec, Stmt};
use crate::{decompile_analysis, Netlist};
use forge_model::Pcb;
use std::collections::BTreeSet;
use std::fmt::Write as _;

/// Build a [`Program`] from a board, grouped by repeat detection.
pub fn program_from_board(pcb: &Pcb) -> Program {
    let (nl, analysis) = decompile_analysis(pcb);
    let comps = read_comps(pcb);
    let version = match pcb.version() {
        0 => 20241229,
        v => v,
    };

    let mut blocks = Vec::new();
    let mut body: Vec<Stmt> = Vec::new();
    let mut placed = vec![false; comps.len()];

    // Declare every net up front, in first-seen order over the netlist's
    // components, so the emitted net table is stable.
    let mut declared: BTreeSet<String> = BTreeSet::new();
    let mut net_decls: Vec<String> = Vec::new();
    for c in &comps {
        for p in &c.pads {
            if let Some(n) = &p.net {
                if declared.insert(n.clone()) {
                    net_decls.push(n.clone());
                }
            }
        }
    }
    for n in net_decls {
        body.push(Stmt::Net(n));
    }

    // One block per multi-instance cluster.
    for c in &analysis.clusters {
        let block = build_block(&nl, c);
        for inst in &c.instances {
            let comps_by_slot: Vec<Option<Comp>> = inst
                .comps_by_slot
                .iter()
                .map(|slot| {
                    slot.map(|ci| {
                        placed[ci] = true;
                        comps[ci].clone()
                    })
                })
                .collect();
            // Extra components in this instance (beyond the template) are placed
            // as singletons so nothing is lost.
            let mut extras: Vec<Comp> = Vec::new();
            for &ci in &inst.extra_comps {
                if !placed[ci] {
                    placed[ci] = true;
                    extras.push(comps[ci].clone());
                }
            }
            body.push(Stmt::Instance(Instance {
                block: block.name.clone(),
                comps: comps_by_slot,
            }));
            for e in extras {
                body.push(Stmt::Single(e));
            }
        }
        blocks.push(block);
    }

    // Singletons + anything the analysis didn't reach.
    for c in &analysis.singletons {
        for inst in &c.instances {
            for ci in inst.comps_by_slot.iter().flatten() {
                if !placed[*ci] {
                    placed[*ci] = true;
                    body.push(Stmt::Single(comps[*ci].clone()));
                }
            }
            for &ci in &inst.extra_comps {
                if !placed[ci] {
                    placed[ci] = true;
                    body.push(Stmt::Single(comps[ci].clone()));
                }
            }
        }
    }
    for (ci, done) in placed.iter().enumerate() {
        if !done {
            body.push(Stmt::Single(comps[ci].clone()));
        }
    }

    Program {
        version,
        blocks,
        body,
        outline: read_outline(pcb),
    }
}

/// Read the board outline from the source board's `Edge.Cuts` geometry.
///
/// The decompiler keeps connectivity, not full geometry, so the outline is
/// captured as the axis-aligned bounding box of every `Edge.Cuts` `gr_line`.
/// That is exactly what the re-layout placer needs (it keeps parts inside the
/// box), and it round-trips through `board outline X0 Y0 X1 Y1`. Returns `None`
/// when the board has no edge geometry (the placer then auto-sizes a box).
fn read_outline(pcb: &Pcb) -> Option<Outline> {
    let mut min = (f64::MAX, f64::MAX);
    let mut max = (f64::MIN, f64::MIN);
    let mut any = false;
    for gl in pcb.gr_lines() {
        if gl.layer() != "Edge.Cuts" {
            continue;
        }
        for (x, y) in [gl.start(), gl.end()] {
            min.0 = min.0.min(x);
            min.1 = min.1.min(y);
            max.0 = max.0.max(x);
            max.1 = max.1.max(y);
            any = true;
        }
    }
    if any && max.0 > min.0 && max.1 > min.1 {
        Some(Outline {
            min_x: min.0,
            min_y: min.1,
            max_x: max.0,
            max_y: max.1,
        })
    } else {
        None
    }
}

/// Read every footprint with full pad geometry into a flat [`Comp`] vector,
/// indexed identically to [`Netlist::from_pcb`] (footprint order).
fn read_comps(pcb: &Pcb) -> Vec<Comp> {
    let mut out = Vec::new();
    for fp in pcb.footprints() {
        let (x, y, rot) = fp.at();
        let mut pads = Vec::new();
        for pad in fp.pads() {
            let net = pad
                .net()
                .and_then(|(_, name)| if name.is_empty() { None } else { Some(name) });
            let (px, py, _) = pad.at();
            pads.push(Pad {
                number: pad.number(),
                kind: pad_kind_str(&pad),
                shape: {
                    let s = pad.shape();
                    if s.is_empty() {
                        "rect".to_string()
                    } else {
                        s
                    }
                },
                at: (px, py),
                size: {
                    let (w, h) = pad.size();
                    if w == 0.0 && h == 0.0 {
                        (1.0, 1.0)
                    } else {
                        (w, h)
                    }
                },
                drill: pad.drill(),
                layers: {
                    let l = pad.layers();
                    if l.is_empty() {
                        vec!["F.Cu".to_string()]
                    } else {
                        l
                    }
                },
                net,
            });
        }
        out.push(Comp {
            reference: fp.reference().unwrap_or_default(),
            lib_id: fp.lib_id(),
            value: fp.value().unwrap_or_default(),
            layer: {
                let l = fp.layer();
                if l.is_empty() {
                    "F.Cu".to_string()
                } else {
                    l
                }
            },
            at: (x, y),
            rot,
            space: None,
            pads,
        });
    }
    out
}

fn pad_kind_str(pad: &forge_model::Pad) -> String {
    use forge_model::PadKind;
    match pad.kind() {
        PadKind::Smd => "smd",
        PadKind::ThruHole => "thru_hole",
        PadKind::NpThruHole => "np_thru_hole",
        PadKind::Connect => "connect",
        PadKind::Unknown => "smd",
    }
    .to_string()
}

fn build_block(nl: &Netlist, c: &Cluster) -> Block {
    let _ = nl;
    let slots = c
        .template
        .iter()
        .map(|tr| SlotSpec {
            lib_id: tr.lib_id.clone(),
            value: tr.value.clone(),
            pad_count: tr.pad_count,
        })
        .collect();
    Block {
        name: c.name.clone(),
        slots,
        instances: c.instances.len(),
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

impl Program {
    /// Render the program to Board-as-Code text.
    pub fn emit(&self) -> String {
        let mut s = String::new();
        let ncomps = self.comps().count();
        let _ = writeln!(s, "# Board-as-Code (hauksbee board DSL v1)");
        let _ = writeln!(
            s,
            "# {ncomps} components, {} block(s), {} net(s).",
            self.blocks.len(),
            self.body
                .iter()
                .filter(|st| matches!(st, Stmt::Net(_)))
                .count()
        );
        let _ = writeln!(s, "board version {}", self.version);
        if let Some(o) = &self.outline {
            let _ = writeln!(
                s,
                "board outline {} {} {} {}",
                fnum(o.min_x),
                fnum(o.min_y),
                fnum(o.max_x),
                fnum(o.max_y)
            );
        }
        let _ = writeln!(s);

        for b in &self.blocks {
            emit_block(&mut s, b);
        }

        let _ = writeln!(s, "fn main {{");
        // Nets first.
        for st in &self.body {
            if let Stmt::Net(n) = st {
                let _ = writeln!(s, "    net {}", quote(n));
            }
        }
        // Block-level space fields.
        for st in &self.body {
            if let Stmt::BlockSpace { block, dist } = st {
                let _ = writeln!(s, "    space fn {} {}", block, fnum(*dist));
            }
        }
        // Placement constraints (pin / lock).
        for st in &self.body {
            match st {
                Stmt::Pin { reference, edge } => {
                    let _ = writeln!(s, "    pin {} edge {}", reference, edge.as_str());
                }
                Stmt::Lock { reference } => {
                    let _ = writeln!(s, "    lock {}", reference);
                }
                _ => {}
            }
        }
        let _ = writeln!(s);
        // Instances and singletons in body order.
        for st in &self.body {
            match st {
                Stmt::Instance(inst) => emit_instance(&mut s, inst),
                Stmt::Single(c) => emit_comp(&mut s, c, 1),
                _ => {}
            }
        }
        let _ = writeln!(s, "}}");
        s
    }
}

fn emit_block(s: &mut String, b: &Block) {
    let _ = writeln!(
        s,
        "# block {}: {} slot(s), {} instance(s)",
        b.name,
        b.slots.len(),
        b.instances
    );
    let _ = writeln!(s, "fn {} {{", b.name);
    for (i, slot) in b.slots.iter().enumerate() {
        let _ = writeln!(
            s,
            "    slot {} lib {} val {} pads {}",
            i,
            quote(&slot.lib_id),
            quote(&slot.value),
            slot.pad_count
        );
    }
    let _ = writeln!(s, "}}");
    let _ = writeln!(s);
}

fn emit_instance(s: &mut String, inst: &Instance) {
    let _ = writeln!(s, "    instance {} {{", inst.block);
    for (slot, comp) in inst.comps.iter().enumerate() {
        match comp {
            Some(c) => emit_comp(s, c, 2),
            None => {
                let _ = writeln!(s, "        # slot {slot}: missing");
            }
        }
    }
    let _ = writeln!(s, "    }}");
}

fn emit_comp(s: &mut String, c: &Comp, indent: usize) {
    let pad = "    ".repeat(indent);
    let _ = writeln!(
        s,
        "{pad}comp {} lib {} val {} layer {} at {} {} rot {} {{",
        c.reference,
        quote(&c.lib_id),
        quote(&c.value),
        quote(&c.layer),
        fnum(c.at.0),
        fnum(c.at.1),
        fnum(c.rot),
    );
    if let Some(sp) = c.space {
        let _ = writeln!(s, "{pad}    space {}", fnum(sp.dist));
    }
    for p in &c.pads {
        emit_pad(s, p, indent + 1);
    }
    let _ = writeln!(s, "{pad}}}");
}

fn emit_pad(s: &mut String, p: &Pad, indent: usize) {
    let pad = "    ".repeat(indent);
    let net = match &p.net {
        Some(n) => format!("net {}", quote(n)),
        None => "nonet".to_string(),
    };
    let drill = match p.drill {
        Some(d) => format!(" drill {}", fnum(d)),
        None => String::new(),
    };
    let _ = writeln!(
        s,
        "{pad}pad {} {} {} at {} {} size {} {}{} layers [{}] {}",
        quote(&p.number),
        p.kind,
        p.shape,
        fnum(p.at.0),
        fnum(p.at.1),
        fnum(p.size.0),
        fnum(p.size.1),
        drill,
        p.layers.join(" "),
        net,
    );
}

/// Quote a string token. Always quoted so values with spaces survive.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Format a float compactly but losslessly enough for connectivity round-trip.
fn fnum(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        // Up to 6 decimals, trimmed.
        let mut s = format!("{:.6}", v);
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
        s
    }
}
