//! Shared harness for dln-node end-to-end scenarios.
//!
//! Exposed as a library so several scenario binaries can drive the same
//! infrastructure (bitcoind, relay, nodes) without duplicating it.

pub mod bitcoind;
pub mod dln_node_client;
pub mod lnrod_client;
pub mod process;
pub mod relay;
pub mod util;
