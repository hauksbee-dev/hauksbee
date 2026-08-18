//! Compatibility re-exports for the shared front-door wire protocol.
//!
//! The message types themselves moved to `hauksbee-frontdoor-api::protocol`
//! so the compute engine can speak the protocol without linking this server
//! crate; the glob re-export keeps every existing
//! `hauksbee_server::protocol::*` path compiling unchanged. New code should
//! import from the api crate directly; this module exists for the paths that
//! predate the split.

pub use hauksbee_frontdoor_api::protocol::*;
