//! Bitcoin Knots backend at level 0.
//!
//! Mission 02.1: swap the regtest backend from Bitcoin Core to Bitcoin
//! Knots and show that a node works against it unchanged, while the chain
//! stays on v1 headers.
//!
//! "Level 0" means the Knots daemon runs without `-testactivationheight`,
//! so `Blake2bHeight` remains at `INT_MAX` and every block carries a v1
//! header — 80 bytes, SHA256d, bit 31 of `nVersion` clear. At v1 the chain
//! is behaviourally Core, which is what makes this step small: nothing in
//! the node or its dependency stack has to change.
//!
//! The node binary is `dln-node-knots`, even though at level 0 it is
//! identical to `dln-node`, so the binary split later missions rely on is
//! exercised from the start.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::json;
use tracing::info;

use dln_e2e_test::bitcoind::BitcoindHarness;
use dln_e2e_test::dln_node_client::{DlnNode, SignerMode};
use dln_e2e_test::{relay, util};

const KNOTS_NODE_DIR: &str = "/home/rene/git/dln-node-knots";

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
        std::env::var("OUTPUT_DIR").unwrap_or_else(|_| util::unique_tmp_dir("knots-backend")),
    );
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir)?;
    info!("Output directory: {}", output_dir.display());

    // ── Step 1: use the Knots build of the node ─────────────────────────
    if std::env::var("DLN_NODE_BINARY").is_err() {
        let binary = build_knots_node()?;
        info!("Step 1: using dln-node-knots at {binary}");
        std::env::set_var("DLN_NODE_BINARY", &binary);
    }

    // ── Step 2: Bitcoin Knots regtest ───────────────────────────────────
    info!("Step 2: Starting bitcoind (Bitcoin Knots, regtest, level 0)");
    let bitcoind = BitcoindHarness::start_knots().await;
    let miner_address = bitcoind.get_new_address().await;
    bitcoind.mine_blocks(110, &miner_address).await;

    let info = bitcoind
        .rpc("getblockchaininfo", json!([]))
        .await
        .map_err(|e| anyhow::anyhow!("getblockchaininfo failed: {e}"))?;
    let height = info["blocks"].as_u64().context("blocks missing")?;
    anyhow::ensure!(height == 110, "expected height 110, got {height}");
    info!("  chain height {height}");

    // ── Step 3: headers must still be v1 ────────────────────────────────
    // This is the scope guard: if Blake2B ever activates, headers become
    // 164 bytes with bit 31 set, and this mission has overrun.
    info!("Step 3: Verifying headers are v1");
    for h in [0u64, 1, height] {
        let hash = bitcoind
            .rpc("getblockhash", json!([h]))
            .await
            .map_err(|e| anyhow::anyhow!("getblockhash({h}) failed: {e}"))?;
        let hex = bitcoind
            .rpc("getblockheader", json!([hash, false]))
            .await
            .map_err(|e| anyhow::anyhow!("getblockheader failed: {e}"))?;
        let raw = hex::decode(hex.as_str().context("header not a string")?)?;
        anyhow::ensure!(
            raw.len() == 80,
            "block {h}: expected an 80-byte v1 header, got {} bytes",
            raw.len()
        );
        let version = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        anyhow::ensure!(
            version & 0x8000_0000 == 0,
            "block {h}: bit 31 of nVersion is set (0x{version:08x}) — this is a v2 header"
        );
    }
    info!("  blocks 0, 1 and {height}: 80 bytes, bit 31 clear");

    // ── Step 4: a node runs against it unchanged ────────────────────────
    // Bind the container handle: testcontainers stops the container when it
    // drops, so inlining this call would tear the relay down immediately.
    let (_relay_container, relay_url) = relay::start_relay().await;

    info!("Step 4: Starting a plain node against the Knots backend");
    let node = DlnNode::start_named(
        "alice",
        SignerMode::Plain,
        &bitcoind,
        &miner_address,
        &relay_url,
        &output_dir,
    )
    .await
    .context("node failed to start against Knots")?;
    info!("  node_id={}", node.node_id());

    // Starting at all proves chain sync — ldk-node will not come up without
    // reaching bitcoind. An address proves the wallet is live too.
    let address = node
        .new_onchain_address()
        .await
        .context("node could not generate an address")?;
    anyhow::ensure!(
        address.starts_with("bcrt1"),
        "expected a regtest address, got {address}"
    );
    info!("  wallet address {address}");

    Ok(())
}

/// Build `dln-node-knots` and return the binary path.
fn build_knots_node() -> Result<String> {
    let dir = PathBuf::from(KNOTS_NODE_DIR);
    anyhow::ensure!(dir.is_dir(), "{KNOTS_NODE_DIR} not found");

    let status = std::process::Command::new("cargo")
        .args(["build", "--bin", "dln-node"])
        .current_dir(&dir)
        .status()
        .context("failed to run cargo build for dln-node-knots")?;
    anyhow::ensure!(status.success(), "cargo build failed with {status}");

    let binary = dir.join("target/debug/dln-node");
    anyhow::ensure!(binary.exists(), "binary not found at {}", binary.display());
    Ok(binary.to_string_lossy().to_string())
}
