//! Contract and evidence integration tests bundled to avoid repeated linking.

#[path = "evidence_spine_ci.rs"]
mod evidence_spine_ci;
#[path = "firmware_guard.rs"]
mod firmware_guard;
#[path = "firmware_input_ci.rs"]
mod firmware_input_ci;
#[path = "floating_net_verdict.rs"]
mod floating_net_verdict;
#[path = "manifest_cli_contract.rs"]
mod manifest_cli_contract;
#[path = "multiunit_keying.rs"]
mod multiunit_keying;
#[path = "packaged_asset_sync.rs"]
mod packaged_asset_sync;
#[path = "unpowered_rail_is_declared.rs"]
mod unpowered_rail_is_declared;
