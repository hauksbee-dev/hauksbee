//! One integration-test binary for the crate; each module was a
//! separate test file (and separate link step) before.

#[path = "frontdoor_http.rs"]
mod frontdoor_http;
#[path = "packaged_asset_sync.rs"]
mod packaged_asset_sync;
#[path = "wire_contract.rs"]
mod wire_contract;
#[path = "ws_roundtrip.rs"]
mod ws_roundtrip;
