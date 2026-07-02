//! Pretty text rendering of an [`Analysis`].

use crate::cluster::{Analysis, Anomaly, Cluster};
use crate::netlist::Netlist;
use std::fmt::Write;

/// Render a human-readable summary of the analysis.
pub fn render_report(nl: &Netlist, analysis: &Analysis) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "=== Board decompilation report ===");
    let _ = writeln!(s, "total components: {}", analysis.total_comps);
    let _ = writeln!(
        s,
        "multi-instance clusters: {}  | singleton blocks: {}",
        analysis.clusters.len(),
        analysis.singletons.len()
    );
    let _ = writeln!(
        s,
        "cluster coverage: {:.1}% of components",
        analysis.cluster_coverage() * 100.0
    );
    let _ = writeln!(s);

    let _ = writeln!(s, "--- Clusters (size >= 2) ---");
    for c in &analysis.clusters {
        render_cluster(&mut s, nl, c);
    }

    if !analysis.singletons.is_empty() {
        let _ = writeln!(s, "--- Singleton blocks ({}) ---", analysis.singletons.len());
        // Show only the largest few singletons by component count to avoid noise.
        for c in analysis.singletons.iter().take(15) {
            let refs: Vec<&str> = c.instances[0]
                .comps_by_slot
                .iter()
                .flatten()
                .map(|&ci| nl.comps[ci].reference.as_str())
                .collect();
            let _ = writeln!(
                s,
                "  {} ({} comp): {}",
                c.name,
                c.template.len(),
                preview(&refs)
            );
        }
        if analysis.singletons.len() > 15 {
            let _ = writeln!(s, "  ... and {} more", analysis.singletons.len() - 15);
        }
        let _ = writeln!(s);
    }

    s
}

fn render_cluster(s: &mut String, nl: &Netlist, c: &Cluster) {
    let _ = writeln!(
        s,
        "* {}  x{}  ({} components/instance, {} anomalies)",
        c.name,
        c.size(),
        c.template.len(),
        c.anomaly_count()
    );
    // Roles summary.
    let _ = writeln!(s, "    roles:");
    for tr in &c.template {
        let val = if tr.value.is_empty() {
            String::new()
        } else {
            format!(" = {}", tr.value)
        };
        let _ = writeln!(s, "      [{}] {}{}", tr.slot, tr.lib_id, val);
    }
    // Anomalous instances.
    let anomalous: Vec<_> = c
        .instances
        .iter()
        .filter(|i| !i.anomalies.is_empty())
        .collect();
    if !anomalous.is_empty() {
        let _ = writeln!(s, "    ANOMALIES:");
        for inst in anomalous {
            let label = instance_label(nl, inst.block_index, inst);
            for a in &inst.anomalies {
                let _ = writeln!(s, "      [{}] {}", label, render_anomaly(a));
            }
        }
    }
    let _ = writeln!(s);
}

fn instance_label(nl: &Netlist, _block_index: usize, inst: &crate::cluster::Instance) -> String {
    // Use the first present reference as the instance label.
    inst.comps_by_slot
        .iter()
        .flatten()
        .chain(inst.extra_comps.iter())
        .next()
        .map(|&ci| nl.comps[ci].reference.clone())
        .unwrap_or_else(|| "?".to_string())
}

pub fn render_anomaly(a: &Anomaly) -> String {
    match a {
        Anomaly::ValueMismatch {
            slot,
            reference,
            expected,
            found,
        } => format!(
            "slot {} ({}): value expected '{}' but found '{}'",
            slot, reference, expected, found
        ),
        Anomaly::LibIdMismatch {
            slot,
            reference,
            expected,
            found,
        } => format!(
            "slot {} ({}): lib_id expected '{}' but found '{}'",
            slot, reference, expected, found
        ),
        Anomaly::MissingComponent {
            slot,
            expected_lib_id,
            expected_value,
        } => format!(
            "slot {}: MISSING {} = {}",
            slot, expected_lib_id, expected_value
        ),
        Anomaly::ExtraComponent {
            reference,
            lib_id,
            value,
        } => format!("EXTRA {} = {} ({})", reference, value, lib_id),
    }
}

fn preview(refs: &[&str]) -> String {
    let shown: Vec<&str> = refs.iter().take(8).copied().collect();
    if refs.len() > 8 {
        format!("{}, ... (+{})", shown.join(", "), refs.len() - 8)
    } else {
        shown.join(", ")
    }
}
