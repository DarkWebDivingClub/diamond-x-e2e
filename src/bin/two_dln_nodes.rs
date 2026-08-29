//! Two plain dln-node instances over a Bitcoin Core regtest network.
//!
//! No VLS at all: both nodes take the `SignerMode::Plain` path, so ldk-node
//! uses its own KeysManager and BDK wallet, seeded from its storage dir. A
//! Nostr relay is still required because the harness drives the nodes over
//! NWC/NCC — that is the control plane, not the signing path.
//!
//! Scenario: alice opens a channel to bob, bob issues an invoice, alice pays
//! it, and the balance change is asserted on both sides.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::info;

use dln_e2e_test::bitcoind::BitcoindHarness;
use dln_e2e_test::dln_node_client::{DlnNode, SignerMode};
use dln_e2e_test::{relay, util};

const CHANNEL_SATS: u64 = 2_000_000;
const PUSH_MSAT: u64 = 500_000;
const PAYMENT_MSAT: u64 = 100_000;

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
        std::env::var("OUTPUT_DIR").unwrap_or_else(|_| util::unique_tmp_dir("two-dln-nodes")),
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

    // ── Step 3: Two nodes, both with embedded signers ───────────────────
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
    info!("  alice node_id={}", alice.node_id());

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
    info!("  bob node_id={}", bob.node_id());

    anyhow::ensure!(
        alice.signer_child.is_none() && bob.signer_child.is_none(),
        "expected no external signer process in plain mode"
    );
    // Fail fast and clearly if the two nodes report the same identity, rather
    // than timing out later waiting for a channel that can never open.
    anyhow::ensure!(
        alice.node_id() != bob.node_id(),
        "alice and bob report the same node_id ({}) — node addressing is wrong",
        alice.node_id()
    );

    // ── Step 5: alice opens a channel to bob ────────────────────────────
    let bob_id = bob.node_id();
    let bob_addr = format!("127.0.0.1:{}", bob.ln_port);
    info!("Step 5: alice -> bob channel, {CHANNEL_SATS} sats");
    alice
        .open_channel(&bob_id, &bob_addr, CHANNEL_SATS, Some(PUSH_MSAT))
        .await
        .context("open_channel failed")?;

    // Confirm the funding transaction.
    info!("Step 6: Mining to confirm the channel");
    wait_for(Duration::from_secs(120), "channel ready on both sides", || {
        let (alice, bob, bob_id, bitcoind, miner_address) =
            (&alice, &bob, &bob_id, &bitcoind, &miner_address);
        async move {
            bitcoind.mine_blocks(1, miner_address).await;
            let alice_id = alice.node_id();
            Ok(alice.has_ready_channel_with(bob_id).await
                && bob.has_ready_channel_with(&alice_id).await)
        }
    })
    .await?;

    // ── Step 7: bob invoices, alice pays ────────────────────────────────
    info!("Step 7: bob issues an invoice for {PAYMENT_MSAT} msat");
    let bob_before = bob.get_balance_msat().await?;
    let invoice = bob
        .make_invoice(PAYMENT_MSAT, "two-dln-nodes e2e")
        .await
        .context("bob make_invoice failed")?;

    info!("Step 8: alice pays it");
    alice
        .pay_invoice(&invoice)
        .await
        .context("alice pay_invoice failed")?;

    wait_for(Duration::from_secs(60), "bob balance to increase", || {
        let (bob, bob_before) = (&bob, bob_before);
        async move { Ok(bob.get_balance_msat().await? >= bob_before + PAYMENT_MSAT) }
    })
    .await?;

    let bob_after = bob.get_balance_msat().await?;
    info!("bob balance {bob_before} -> {bob_after} msat");
    Ok(())
}

/// Poll `check` until it returns true or the deadline passes.
async fn wait_for<F, Fut>(timeout: Duration, what: &str, mut check: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if check().await? {
            return Ok(());
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
