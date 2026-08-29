//! On-chain payment between two plain dln-node instances over Bitcoin Core
//! regtest.
//!
//! Complements `two_dln_nodes`, which covers the Lightning path. Here nothing
//! is routed: alice spends from her BDK wallet directly to an address bob's
//! wallet generated, and the transaction is verified in bitcoind rather than
//! inferred from a balance change.
//!
//! Both nodes take the `SignerMode::Plain` path, so ldk-node uses its own
//! KeysManager and BDK wallet with no VLS in the signing path.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use tracing::info;

use dln_e2e_test::bitcoind::BitcoindHarness;
use dln_e2e_test::dln_node_client::{DlnNode, SignerMode};
use dln_e2e_test::{relay, util};

/// Amount alice sends to bob on chain.
const SEND_SATS: u64 = 250_000;
/// Confirmations to wait for before asserting.
const CONFIRMATIONS: u64 = 3;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run_scenario().await {
        Ok(()) => {
            println!("\n=== PASS ===");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("\n=== FAIL ===\n{e:?}");
            std::process::exit(1);
        }
    }
}

async fn run_scenario() -> Result<()> {
    let output_dir = PathBuf::from(
        std::env::var("OUTPUT_DIR").unwrap_or_else(|_| util::unique_tmp_dir("onchain-payment")),
    );
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir)?;
    info!("Output directory: {}", output_dir.display());

    // ── Step 1: Bitcoin Core regtest ────────────────────────────────────
    info!("Step 1: Starting bitcoind (regtest) and mining to maturity");
    let bitcoind = BitcoindHarness::start_from_env().await;
    let miner_address = bitcoind.get_new_address().await;
    bitcoind.mine_blocks(110, &miner_address).await;

    // ── Step 2: Nostr relay (control plane only) ────────────────────────
    info!("Step 2: Starting Nostr relay (NWC/NCC control plane)");
    let (_relay_container, relay_url) = relay::start_relay().await;

    // ── Step 3: Two plain nodes ─────────────────────────────────────────
    info!("Step 3: Starting alice (plain, no VLS)");
    let alice = DlnNode::start_named(
        "alice",
        SignerMode::Plain,
        &bitcoind,
        &miner_address,
        &relay_url,
        &output_dir,
    )
    .await
    .context("failed to start alice")?;

    info!("Step 4: Starting bob (plain, no VLS)");
    let bob = DlnNode::start_named(
        "bob",
        SignerMode::Plain,
        &bitcoind,
        &miner_address,
        &relay_url,
        &output_dir,
    )
    .await
    .context("failed to start bob")?;

    anyhow::ensure!(
        alice.node_id() != bob.node_id(),
        "alice and bob report the same node_id ({}) — node addressing is wrong",
        alice.node_id()
    );

    // ── Step 5: bob supplies a receiving address ────────────────────────
    info!("Step 5: bob generates an on-chain address");
    let bob_address = bob
        .new_onchain_address()
        .await
        .context("bob make_new_address failed")?;
    info!("  bob address: {bob_address}");

    // ── Step 6: alice pays it on chain ──────────────────────────────────
    info!("Step 6: alice sends {SEND_SATS} sats on chain");
    let txid = alice
        .pay_onchain(&bob_address, SEND_SATS)
        .await
        .context("alice pay_onchain failed")?;
    info!("  txid: {txid}");

    // ── Step 7: confirm and verify against bitcoind ─────────────────────
    info!("Step 7: Mining {CONFIRMATIONS} blocks and verifying the transaction");
    bitcoind.mine_blocks(CONFIRMATIONS, &miner_address).await;

    let tx = bitcoind
        .rpc("getrawtransaction", json!([txid, true]))
        .await
        .map_err(|e| anyhow::anyhow!("getrawtransaction failed: {e}"))?;

    let confirmations = tx["confirmations"].as_u64().unwrap_or(0);
    anyhow::ensure!(
        confirmations >= CONFIRMATIONS,
        "expected >= {CONFIRMATIONS} confirmations, got {confirmations}"
    );

    // The transaction must actually pay bob's address the requested amount,
    // not merely exist.
    let paid_to_bob = tx["vout"]
        .as_array()
        .context("vout missing")?
        .iter()
        .find(|out| out["scriptPubKey"]["address"].as_str() == Some(bob_address.as_str()))
        .context("no output pays bob's address")?;

    let paid_sats = (paid_to_bob["value"].as_f64().context("value missing")? * 1e8).round() as u64;
    anyhow::ensure!(
        paid_sats == SEND_SATS,
        "expected {SEND_SATS} sats to bob, found {paid_sats}"
    );

    info!("confirmed: {paid_sats} sats to {bob_address} in {txid} ({confirmations} confs)");

    // Give the nodes a moment to see the confirmation before teardown.
    tokio::time::sleep(Duration::from_secs(2)).await;
    Ok(())
}
