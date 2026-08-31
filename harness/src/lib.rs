//! Shared infrastructure for driving `dln-node` end-to-end scenarios.
//!
//! Bitcoin Core and Bitcoin Knots regtest backends, a Nostr relay, and a
//! client that speaks the node's NWC and NCC control planes. The scenarios
//! themselves live elsewhere — in this repository for the exchange, and in
//! `dln-node-e2e` and `dln-node-knots-e2e` for the node.
//!
//! It is a library so the three suites drive the same infrastructure rather
//! than three drifting copies of it.

pub mod bitcoind;
pub mod dln_node_client;
pub mod process;
pub mod relay;
pub mod util;
