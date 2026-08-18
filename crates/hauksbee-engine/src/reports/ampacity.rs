//! `--ampacity`: IPC-2221 capacity-only report. No current is fabricated here,
//! without a per-net current spec this tells the user the bottleneck capacity and
//! explicitly asks for a current before pass/fail. JSON remains unsupported;
//! plain mode keeps the same auditable rows under a short prose explanation.

/// Print the trace-capacity report and return. `altium_present` boards carry no
/// routed-copper geometry through this path yet, so they report an empty table.
pub fn emit(
    text: &str,
    altium_present: bool,
    evidence: &crate::evidence::BoardEvidence,
) -> anyhow::Result<()> {
    emit_quiet(text, altium_present, evidence, &[], false, false)
}

pub(crate) fn emit_quiet(
    text: &str,
    altium_present: bool,
    evidence: &crate::evidence::BoardEvidence,
    board_net_names: &[String],
    plain: bool,
    quiet: bool,
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
    if plain {
        println!(
            "Plain-language ampacity: this estimates how much current each recognized power rail's narrowest routed copper can carry; it is not a pass/fail result without a supplied load current."
        );
    }
    print!(
        "{}",
        hauksbee_extract::render_trace_capacity_report_with_context(&rows, board_net_names)
    );
    print!("{}", super::render_evidence_appendix(evidence, quiet));
    Ok(())
}
