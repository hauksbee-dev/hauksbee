//! `hauksbee serve [--port N]`: the local web front door (upload-and-report).

/// `hauksbee serve [--port N]`: the local web front door (upload-and-report).
pub fn run(port: u16) -> anyhow::Result<()> {
    use std::sync::Arc;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let addr = format!("127.0.0.1:{}", port);
        // Inject the engine's analysis as the server's analyzer callback, so the
        // server crate needs no dependency on the engine/extract crates. The
        // firmware-aware callback handles both the board-only path (firmware ==
        // None -> analyze_json) and the firmware co-sim path.
        let analyze: hauksbee_server::frontdoor::FirmwareAnalyzer = Arc::new(
            |name: &str, contents: &str, fw: Option<(&str, &[u8])>| match fw {
                Some((fw_name, fw_bytes)) => {
                    crate::analyze_with_firmware_json(name, contents, fw_name, fw_bytes)
                }
                None => crate::analyze_json(name, contents),
            },
        );
        hauksbee_server::frontdoor::serve_with_firmware(&addr, analyze).await
    })
}
