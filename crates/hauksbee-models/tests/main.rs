//! One integration-test binary for the crate; each module was a
//! separate test file (and separate link step) before.

#[path = "extract_sandbox.rs"]
mod extract_sandbox;
#[path = "pin_roles.rs"]
mod pin_roles;
#[path = "suite/main.rs"]
mod suite;
