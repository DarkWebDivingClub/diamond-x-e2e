//! BLAKE2b activates under a running node.
//!
//! Mission 03.4. Every scenario so far has run at level 0, where a Knots
//! chain produces only v1 headers and is behaviourally Bitcoin Core. This
//! is the first time the fork actually happens underneath `dln-node-knots`:
//! the chain switches to 164-byte BLAKE2b headers at a height chosen here,
//! while a node with an open channel is watching.
//!
//! What it has to show, in order:
//!
//!   * the switch really happened — v1 below the height, v2 at and above it,
//!     otherwise the scenario could pass by never activating at all;
//!   * the node keeps following the chain across it;
//!   * a channel opened before the switch is still there afterwards, with
//!     its confirmation count continuous rather than restarted;
//!   * a payment settles on the far side;
//!   * a node restarted against an already-activated chain comes back.
//!
//! Requires `dln-node-knots` built with the `blake2b` feature; without it
//! the node cannot parse a v2 header and step 6 is where it stops.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tracing::info;

use dln_e2e_test::bitcoind::BitcoindHarness;
use dln_e2e_test::dln_node_client::{DlnNode, SignerMode};
use dln_e2e_test::{relay, util};

/// Height at which BLAKE2b activates. Comfortably above coinbase maturity
/// so a channel can be funded and confirmed while the chain is still v1.
const ACTIVATION_HEIGHT: u64 = 150;

const CHANNEL_SATS: u64 = 2_000_000;
const PUSH_MSAT: u64 = 500_000;
const PAYMENT_MSAT: u64 = 100_000;

/// Blocks mined past activation before the assertions run.
const BLOCKS_PAST_ACTIVATION: u64 = 5;

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
        std::env::var("OUTPUT_DIR").unwrap_or_else(|_| util::unique_tmp_dir("activation")),
    );
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir)?;
    info!("Output directory: {}", output_dir.display());

    // ── Step 1: a node that can read v2 headers ─────────────────────────
    // Without the feature the node parses the first 80 bytes of a 164-byte
    // header and computes the wrong hash, so this scenario must not be able
    // to pass by accident with a level-0 build.
    if std::env::var("DLN_NODE_BINARY").is_err() {
        std::env::set_var("KNOTS_FEATURES", "blake2b");
        let binary = util::build_knots_node()?;
        info!("Step 1: using dln-node-knots (blake2b) at {binary}");
        std::env::set_var("DLN_NODE_BINARY", &binary);
    }

    // ── Step 2: a chain that will activate ──────────────────────────────
    info!("Step 2: Starting Knots, BLAKE2b activating at height {ACTIVATION_HEIGHT}");
    let bitcoind = BitcoindHarness::start_knots_activating_at(ACTIVATION_HEIGHT).await;
    let miner = bitcoind.get_new_address().await;
    bitcoind.mine_blocks(110, &miner).await;
    let height = chain_height(&bitcoind).await?;
    anyhow::ensure!(height == 110, "expected height 110, got {height}");
    anyhow::ensure!(
        height < ACTIVATION_HEIGHT,
        "mined past activation before the nodes were up"
    );
    info!("  chain height {height}, still below activation");

    // ── Step 3: two nodes and a channel, all under v1 ───────────────────
    let (_relay_container, relay_url) = relay::start_relay().await;

    info!("Step 3: Starting alice and bob");
    let alice = DlnNode::start_named(
        "alice", SignerMode::Plain, &bitcoind, &miner, &relay_url, &output_dir,
    )
    .await
    .context("failed to start alice")?;
    let bob = DlnNode::start_named(
        "bob", SignerMode::Plain, &bitcoind, &miner, &relay_url, &output_dir,
    )
    .await
    .context("failed to start bob")?;
    let bob_id = bob.node_id();
    anyhow::ensure!(alice.node_id() != bob_id, "alice and bob share a node_id");
    info!("  alice={} bob={bob_id}", alice.node_id());

    info!("Step 4: alice opens a channel to bob, {CHANNEL_SATS} sats");
    alice
        .open_channel(&bob_id, &format!("127.0.0.1:{}", bob.ln_port), CHANNEL_SATS, Some(PUSH_MSAT))
        .await
        .context("open_channel failed")?;
    wait_for(Duration::from_secs(180), "channel ready under v1 headers", || {
        let (alice, bob, bob_id, bitcoind, miner) = (&alice, &bob, &bob_id, &bitcoind, &miner);
        async move {
            bitcoind.mine_blocks(1, miner).await;
            Ok(alice.has_ready_channel_with(bob_id).await
                && bob.has_ready_channel_with(&alice.node_id()).await)
        }
    })
    .await?;

    let before = channel_with(&alice, &bob_id).await?;
    let funding_txid = before["funding_txid"]
        .as_str()
        .context("channel has no funding_txid")?
        .to_string();
    let funding_height_before = funding_height(&alice, &bob_id).await?;
    let height_before = chain_height(&bitcoind).await?;
    anyhow::ensure!(
        height_before < ACTIVATION_HEIGHT,
        "channel confirmed at {height_before}, at or past activation — nothing left to cross"
    );
    info!(
        "  funding {funding_txid} in block {funding_height_before}, chain at {height_before}"
    );

    // ── Step 5: the last v1 block ───────────────────────────────────────
    info!("Step 5: Mining to {}, the last v1 block", ACTIVATION_HEIGHT - 1);
    bitcoind
        .mine_blocks(ACTIVATION_HEIGHT - 1 - height_before, &miner)
        .await;
    let height = chain_height(&bitcoind).await?;
    anyhow::ensure!(height == ACTIVATION_HEIGHT - 1, "expected {}, got {height}", ACTIVATION_HEIGHT - 1);
    assert_header(&bitcoind, height, 1).await?;
    assert_header(&bitcoind, height_before, 1).await?;
    info!("  heights {height_before} and {height}: v1, 80 bytes");

    // ── Step 6: cross it ────────────────────────────────────────────────
    info!("Step 6: Mining across activation");
    bitcoind.mine_blocks(1 + BLOCKS_PAST_ACTIVATION, &miner).await;
    let tip = chain_height(&bitcoind).await?;
    anyhow::ensure!(
        tip == ACTIVATION_HEIGHT + BLOCKS_PAST_ACTIVATION,
        "expected tip {}, got {tip}",
        ACTIVATION_HEIGHT + BLOCKS_PAST_ACTIVATION
    );

    // The switch is real, or the rest of this proves nothing.
    assert_header(&bitcoind, ACTIVATION_HEIGHT - 1, 1).await?;
    assert_header(&bitcoind, ACTIVATION_HEIGHT, 2).await?;
    assert_header(&bitcoind, tip, 2).await?;
    info!("  height {} is v1, {ACTIVATION_HEIGHT} and {tip} are v2", ACTIVATION_HEIGHT - 1);

    // ── Step 7: the nodes follow it ─────────────────────────────────────
    info!("Step 7: Waiting for both nodes to reach height {tip}");
    wait_for(Duration::from_secs(180), "both nodes at the post-activation tip", || {
        let (alice, bob) = (&alice, &bob);
        async move {
            Ok(u64::from(alice.block_height().await?) >= tip
                && u64::from(bob.block_height().await?) >= tip)
        }
    })
    .await
    .context("a node stopped following the chain across activation")?;
    info!("  alice and bob both at {tip}");

    // ── Step 8: the channel survived, and kept counting ─────────────────
    info!("Step 8: Checking the channel across the switch");
    let after = channel_with(&alice, &bob_id).await?;
    anyhow::ensure!(
        after["funding_txid"].as_str() == Some(funding_txid.as_str()),
        "funding txid changed across activation: {} -> {}",
        funding_txid,
        after["funding_txid"]
    );
    anyhow::ensure!(
        after["state"].as_str() == Some("active"),
        "channel is {:?} after activation, expected active",
        after["state"]
    );

    // The specific failure this is looking for: a count that restarts.
    //
    // Comparing raw confirmation counts would only measure the node's lag
    // behind bitcoind at the two sampling moments. What has to hold is that
    // the node still believes the funding transaction is in the same block,
    // so compare the height its own count implies.
    let funding_height_after = funding_height(&alice, &bob_id).await?;
    anyhow::ensure!(
        funding_height_after == funding_height_before,
        "the node stopped counting from the same block across activation: funding was in \
         block {funding_height_before}, now its confirmations imply {funding_height_after}"
    );
    let confirmations_after = after["confirmations"].as_u64().unwrap_or(0);
    anyhow::ensure!(
        confirmations_after > 0,
        "channel reports no confirmations after activation"
    );
    info!(
        "  funding still {funding_txid} in block {funding_height_after}, \
         {confirmations_after} confirmations, channel active"
    );

    // ── Step 9: value moves on the far side ─────────────────────────────
    info!("Step 9: bob invoices {PAYMENT_MSAT} msat, alice pays it under v2 headers");
    let bob_before = bob.get_balance_msat().await?;
    let invoice = bob
        .make_invoice(PAYMENT_MSAT, "activation e2e")
        .await
        .context("bob make_invoice failed")?;
    alice.pay_invoice(&invoice).await.context("alice pay_invoice failed")?;
    wait_for(Duration::from_secs(60), "bob's balance to increase", || {
        let bob = &bob;
        async move { Ok(bob.get_balance_msat().await? > bob_before) }
    })
    .await?;
    info!("  paid after activation");

    // ── Step 10: restart against an activated chain ─────────────────────
    // Starting fresh is a different path from following the switch live:
    // the node reads back a chain that is already past activation.
    info!("Step 10: Starting a third node against the activated chain");
    bitcoind.mine_blocks(2, &miner).await;
    let tip = chain_height(&bitcoind).await?;
    let carol = DlnNode::start_named(
        "carol", SignerMode::Plain, &bitcoind, &miner, &relay_url, &output_dir,
    )
    .await
    .context("a node could not start against an already-activated chain")?;
    wait_for(Duration::from_secs(120), "carol to reach the tip", || {
        let carol = &carol;
        async move { Ok(u64::from(carol.block_height().await?) >= tip) }
    })
    .await?;
    info!("  carol synced to {tip} from cold");

    Ok(())
}

/// Assert a block's header format, from the raw serialization rather than
/// from the field the RPC reports, so the two are cross-checked.
async fn assert_header(bitcoind: &BitcoindHarness, height: u64, want: u64) -> Result<()> {
    let hash = bitcoind
        .rpc("getblockhash", json!([height]))
        .await
        .map_err(|e| anyhow::anyhow!("getblockhash({height}) failed: {e}"))?;

    let verbose = bitcoind
        .rpc("getblockheader", json!([hash]))
        .await
        .map_err(|e| anyhow::anyhow!("getblockheader({height}) failed: {e}"))?;
    let headerv = verbose["headerv"].as_u64().context("no headerv in the result")?;
    anyhow::ensure!(headerv == want, "height {height}: headerv is {headerv}, expected {want}");

    let hex = bitcoind
        .rpc("getblockheader", json!([hash, false]))
        .await
        .map_err(|e| anyhow::anyhow!("getblockheader({height}, false) failed: {e}"))?;
    let raw = hex::decode(hex.as_str().context("header not a string")?)?;
    let (want_len, want_bit31) = if want == 2 { (164, true) } else { (80, false) };
    anyhow::ensure!(
        raw.len() == want_len,
        "height {height}: header is {} bytes, expected {want_len}",
        raw.len()
    );
    let version = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
    anyhow::ensure!(
        (version & 0x8000_0000 != 0) == want_bit31,
        "height {height}: nVersion is 0x{version:08x}, which disagrees with headerv {headerv}"
    );
    Ok(())
}

/// The block the node believes the funding transaction is in, derived from
/// its own height and its own confirmation count.
///
/// Both come from the node, so this is independent of how far behind
/// bitcoind it happens to be. The height is read either side of the channel
/// and the sample retried if a block landed in between, so the two numbers
/// always describe the same moment.
async fn funding_height(node: &DlnNode, peer: &str) -> Result<u64> {
    for _ in 0..10 {
        let before = u64::from(node.block_height().await?);
        let channel = channel_with(node, peer).await?;
        let after = u64::from(node.block_height().await?);
        if before != after {
            continue;
        }
        let confirmations = channel["confirmations"]
            .as_u64()
            .context("channel has no confirmations — is the node build current?")?;
        anyhow::ensure!(confirmations > 0, "channel funding is unconfirmed");
        return Ok(before - confirmations + 1);
    }
    anyhow::bail!("could not sample height and confirmations together")
}

async fn chain_height(bitcoind: &BitcoindHarness) -> Result<u64> {
    let info = bitcoind
        .rpc("getblockchaininfo", json!([]))
        .await
        .map_err(|e| anyhow::anyhow!("getblockchaininfo failed: {e}"))?;
    info["blocks"].as_u64().context("blocks missing")
}

async fn channel_with(node: &DlnNode, peer: &str) -> Result<Value> {
    node.list_channels()
        .await?
        .into_iter()
        .find(|c| c["peer_pubkey"].as_str() == Some(peer))
        .with_context(|| format!("no channel with {peer}"))
}

async fn wait_for<F, Fut>(timeout: Duration, label: &str, mut f: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if f().await? {
            return Ok(());
        }
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "timed out waiting for {label}");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
