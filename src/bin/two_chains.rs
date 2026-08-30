//! Two chains, four nodes.
//!
//! Mission 02.3. A cross-chain swap needs both chains live in one process,
//! and because a Lightning node serves exactly one chain, each party needs
//! an instance per chain. That is two bitcoinds and four nodes.
//!
//! Core runs one chain with `dln-node`; Knots runs the other with
//! `dln-node-knots`, at level 0 — no `-testactivationheight`, so only v1
//! headers appear. One relay serves all four nodes: it is a control plane,
//! not part of any payment path.
//!
//! Channels are funded by whoever will pay on that chain in 02.4:
//! `bob-core → alice-core` and `alice-knots → bob-knots`.
//!
//! No swap here. This stops once both chains have a working channel.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;
use tracing::info;

use dln_e2e_test::bitcoind::BitcoindHarness;
use dln_e2e_test::dln_node_client::{DlnNode, SignerMode};
use dln_e2e_test::{relay, util};

const CHANNEL_SATS: u64 = 2_000_000;
const PUSH_MSAT: u64 = 500_000;

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
        std::env::var("OUTPUT_DIR").unwrap_or_else(|_| util::unique_tmp_dir("two-chains")),
    );
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir)?;
    info!("Output directory: {}", output_dir.display());

    let knots_binary = util::build_knots_node()?;

    // ── Step 1: two chains ──────────────────────────────────────────────
    info!("Step 1: Starting both chains");
    let core = BitcoindHarness::start().await;
    let knots = BitcoindHarness::start_knots().await;
    let core_miner = core.get_new_address().await;
    let knots_miner = knots.get_new_address().await;
    core.mine_blocks(110, &core_miner).await;
    knots.mine_blocks(110, &knots_miner).await;

    anyhow::ensure!(
        core.rpc_port() != knots.rpc_port(),
        "both harnesses bound the same RPC port"
    );
    info!(
        "  core on :{}, knots on :{}",
        core.rpc_port(),
        knots.rpc_port()
    );

    // ── Step 2: the chains are independent ──────────────────────────────
    // Mine only on Core and confirm Knots does not move. Shared genesis
    // means nothing at the protocol level distinguishes these chains, so
    // this is worth asserting rather than assuming.
    info!("Step 2: Asserting chain independence");
    let knots_before = height(&knots).await?;
    core.mine_blocks(5, &core_miner).await;
    let core_h = height(&core).await?;
    let knots_after = height(&knots).await?;
    anyhow::ensure!(core_h == 115, "core height {core_h}, expected 115");
    anyhow::ensure!(
        knots_after == knots_before,
        "mining on core moved knots: {knots_before} -> {knots_after}"
    );
    info!("  core {core_h}, knots {knots_after} — independent");

    // ── Step 3: four nodes, one relay ───────────────────────────────────
    let (_relay_container, relay_url) = relay::start_relay().await;

    info!("Step 3: Starting four nodes");
    let alice_core = DlnNode::start_on(
        "alice-core", SignerMode::Plain, None,
        &core, &core_miner, &relay_url, &output_dir,
    ).await.context("alice-core failed to start")?;
    let bob_core = DlnNode::start_on(
        "bob-core", SignerMode::Plain, None,
        &core, &core_miner, &relay_url, &output_dir,
    ).await.context("bob-core failed to start")?;
    let alice_knots = DlnNode::start_on(
        "alice-knots", SignerMode::Plain, Some(&knots_binary),
        &knots, &knots_miner, &relay_url, &output_dir,
    ).await.context("alice-knots failed to start")?;
    let bob_knots = DlnNode::start_on(
        "bob-knots", SignerMode::Plain, Some(&knots_binary),
        &knots, &knots_miner, &relay_url, &output_dir,
    ).await.context("bob-knots failed to start")?;

    let ids = [
        alice_core.node_id(), bob_core.node_id(),
        alice_knots.node_id(), bob_knots.node_id(),
    ];
    let mut unique = ids.clone().to_vec();
    unique.sort();
    unique.dedup();
    anyhow::ensure!(
        unique.len() == 4,
        "expected four distinct node_ids, got {}: {ids:?}",
        unique.len()
    );
    info!("  four distinct node_ids");

    // ── Step 4: a channel on each chain ─────────────────────────────────
    // Funded by whoever pays on that chain in 02.4.
    info!("Step 4: Opening a channel on each chain");
    let alice_core_id = alice_core.node_id();
    bob_core
        .open_channel(
            &alice_core_id,
            &format!("127.0.0.1:{}", alice_core.ln_port),
            CHANNEL_SATS,
            Some(PUSH_MSAT),
        )
        .await
        .context("core channel open failed")?;

    let bob_knots_id = bob_knots.node_id();
    alice_knots
        .open_channel(
            &bob_knots_id,
            &format!("127.0.0.1:{}", bob_knots.ln_port),
            CHANNEL_SATS,
            Some(PUSH_MSAT),
        )
        .await
        .context("knots channel open failed")?;

    wait_for(Duration::from_secs(180), "both channels ready", || async {
        core.mine_blocks(1, &core_miner).await;
        knots.mine_blocks(1, &knots_miner).await;
        Ok(bob_core.has_ready_channel_with(&alice_core_id).await
            && alice_core.has_ready_channel_with(&bob_core.node_id()).await
            && alice_knots.has_ready_channel_with(&bob_knots_id).await
            && bob_knots.has_ready_channel_with(&alice_knots.node_id()).await)
    })
    .await?;
    info!("  both channels ready");

    // ── Step 5: no cross-chain visibility ───────────────────────────────
    // Each node should see exactly its own chain's channel.
    info!("Step 5: Asserting no cross-chain channel visibility");
    anyhow::ensure!(
        !alice_core.has_channel_with(&bob_knots_id).await,
        "a Core node sees a Knots channel"
    );
    anyhow::ensure!(
        !alice_knots.has_channel_with(&bob_core.node_id()).await,
        "a Knots node sees a Core channel"
    );
    info!("  no cross-chain visibility");

    Ok(())
}

async fn height(h: &BitcoindHarness) -> Result<u64> {
    let info = h
        .rpc("getblockchaininfo", json!([]))
        .await
        .map_err(|e| anyhow::anyhow!("getblockchaininfo failed: {e}"))?;
    info["blocks"].as_u64().context("blocks missing")
}


async fn wait_for<F, Fut>(timeout: Duration, what: &str, mut check: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if check().await? {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
