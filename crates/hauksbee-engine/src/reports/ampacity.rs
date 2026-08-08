//! `--ampacity`: IPC-2221 capacity-only report. No current is fabricated here,
//! without a per-net current spec this tells the user the bottleneck capacity and
//! explicitly asks for a current before pass/fail. Text-only (there is no JSON /
//! plain variant of this report).

/// Print the trace-capacity report and return. `altium_present` boards carry no
/// routed-copper geometry through this path yet, so they report an empty table.
pub fn emit(
    text: &str,
    altium_present: bool,
    evidence: &crate::evidence::BoardEvidence,
) -> anyhow::Result<()> {
    let rows = if altium_present {
        Vec::new()
    } else {
        let doc = forge_sexpr::parse(text)?;
        let root = doc.root();
        let copper = root
            .map(hauksbee_extract::net_copper_from_root)
            .unwrap_or_default();
        // Rate each net on its own layer's declared copper, not a blanket 1 oz.
        let audit = root
            .map(hauksbee_extract::TraceAudit::from_root)
            .unwrap_or_default();
        hauksbee_extract::trace_capacity_report(&copper, &audit)
    };
    print!("{}", hauksbee_extract::render_trace_capacity_report(&rows));
    print!("{}", evidence.render_plain());
    Ok(())
}
