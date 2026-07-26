//! Emit readable Rust-like source that rebuilds the board.
//!
//! The output is *not* required to compile via rustc. It is required to be
//! deterministic and human-readable: a `fn block_<name>(...)` per cluster, a
//! `fn main()` that calls each block per instance, anomalous instances inlined
//! with `// ANOMALY:` comments, and singletons emitted inline.

use crate::cluster::{Analysis, Cluster, Instance};
use crate::netlist::Netlist;
use crate::report::render_anomaly;
use std::collections::HashSet;
use std::fmt::Write;

/// Generate the full decompiled program text.
pub fn emit_program(nl: &Netlist, analysis: &Analysis) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "// Auto-decompiled board: {} components.",
        analysis.total_comps
    );
    let _ = writeln!(
        s,
        "// {} repeated block(s), {} singleton(s). Coverage {:.1}%.",
        analysis.clusters.len(),
        analysis.singletons.len(),
        analysis.cluster_coverage() * 100.0
    );
    let _ = writeln!(s, "use forge_model::{{FootprintBuilder, PcbBuilder}};");
    let _ = writeln!(s);

    // Block functions for each multi-instance cluster.
    for c in &analysis.clusters {
        emit_block_fn(&mut s, c);
    }

    // main: instance calls + singletons.
    let _ = writeln!(s, "fn main() {{");
    let _ = writeln!(
        s,
        "    let mut b = PcbBuilder::new(20241229).standard_2layer_layers();"
    );
    let _ = writeln!(s);

    for c in &analysis.clusters {
        let _ = writeln!(s, "    // --- {} (x{}) ---", c.name, c.size());
        for inst in &c.instances {
            emit_instance_call(&mut s, nl, c, inst);
        }
        let _ = writeln!(s);
    }

    if !analysis.singletons.is_empty() {
        let _ = writeln!(s, "    // --- singletons (non-repeating) ---");
        for c in &analysis.singletons {
            emit_singleton(&mut s, nl, c);
        }
    }

    let _ = writeln!(s, "    let _pcb = b.build();");
    let _ = writeln!(s, "}}");
    s
}

fn emit_block_fn(s: &mut String, c: &Cluster) {
    let n = c.template.len();
    let _ = writeln!(
        s,
        "/// {} component{}, instanced {} time{}.",
        n,
        plural(n),
        c.size(),
        plural(c.size())
    );
    let _ = writeln!(
        s,
        "fn {}(b: &mut PcbBuilder, at: (f64, f64), rot: f64, refs: [&str; {}]) {{",
        c.name, n
    );
    for tr in &c.template {
        let _ = writeln!(
            s,
            "    // slot {}: {} = {}",
            tr.slot,
            tr.lib_id,
            dq(&tr.value)
        );
        let _ = writeln!(
            s,
            "    b.add_footprint_at(refs[{}], {}, {}, at, rot); // pads: {}",
            tr.slot,
            sl(&tr.lib_id),
            sl(&tr.value),
            tr.pad_count
        );
    }
    let _ = writeln!(s, "}}");
    let _ = writeln!(s);
}

fn emit_instance_call(s: &mut String, nl: &Netlist, c: &Cluster, inst: &Instance) {
    let refs: Vec<String> = inst
        .comps_by_slot
        .iter()
        .map(|c| match c {
            Some(ci) => format!("\"{}\"", nl.comps[*ci].reference),
            None => "\"<missing>\"".to_string(),
        })
        .collect();

    let (at, rot) = match inst.placement {
        Some(p) => (
            format!("({:.4}, {:.4})", p.dx, p.dy),
            format!("{:.1}", p.rot),
        ),
        None => ("(0.0, 0.0)".to_string(), "0.0".to_string()),
    };

    if inst.anomalies.is_empty() && inst.placement.is_some() {
        let _ = writeln!(
            s,
            "    {}(&mut b, {}, {}, [{}]);",
            c.name,
            at,
            rot,
            refs.join(", ")
        );
    } else {
        // Anomalous or non-rigid: inline with explanatory comments.
        for a in &inst.anomalies {
            let _ = writeln!(s, "    // ANOMALY: {}", render_anomaly(a));
        }
        if inst.placement.is_none() {
            let _ = writeln!(
                s,
                "    // ANOMALY: geometry does not match cluster rigidly; placed inline."
            );
        }
        let _ = writeln!(s, "    {{");
        for (slot, comp) in inst.comps_by_slot.iter().enumerate() {
            match comp {
                Some(ci) => {
                    let cc = &nl.comps[*ci];
                    let (x, y, r) = cc.at;
                    let _ = writeln!(
                        s,
                        "        b.add_footprint_xy(\"{}\", {}, {}, ({:.4}, {:.4}), {:.1});",
                        cc.reference,
                        sl(&cc.lib_id),
                        sl(&cc.value),
                        x,
                        y,
                        r
                    );
                }
                None => {
                    let tr = &c.template[slot];
                    let _ = writeln!(
                        s,
                        "        // slot {} MISSING: expected {} = {}",
                        slot,
                        tr.lib_id,
                        dq(&tr.value)
                    );
                }
            }
        }
        for &ci in &inst.extra_comps {
            let cc = &nl.comps[ci];
            let (x, y, r) = cc.at;
            let _ = writeln!(
                s,
                "        b.add_footprint_xy(\"{}\", {}, {}, ({:.4}, {:.4}), {:.1}); // EXTRA",
                cc.reference,
                sl(&cc.lib_id),
                sl(&cc.value),
                x,
                y,
                r
            );
        }
        let _ = writeln!(s, "    }}");
    }
}

fn emit_singleton(s: &mut String, nl: &Netlist, c: &Cluster) {
    let inst = &c.instances[0];
    let mut comps: Vec<usize> = inst.comps_by_slot.iter().flatten().copied().collect();
    comps.extend(inst.extra_comps.iter().copied());
    // Dedup just in case.
    let mut seen = HashSet::new();
    let _ = writeln!(s, "    {{ // {} ({} comp)", c.name, comps.len());
    for ci in comps {
        if !seen.insert(ci) {
            continue;
        }
        let cc = &nl.comps[ci];
        let (x, y, r) = cc.at;
        let _ = writeln!(
            s,
            "        b.add_footprint_xy(\"{}\", {}, {}, ({:.4}, {:.4}), {:.1});",
            cc.reference,
            sl(&cc.lib_id),
            sl(&cc.value),
            x,
            y,
            r
        );
    }
    let _ = writeln!(s, "    }}");
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// String literal (quoted, escaped minimally).
fn sl(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Double-quote for comments (no escaping concerns).
fn dq(s: &str) -> String {
    format!("\"{}\"", s)
}
