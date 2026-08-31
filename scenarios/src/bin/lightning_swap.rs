//! An atomic cross-chain swap over Lightning.
//!
//! Mission 02.4. Alice holds Knots-chain value and wants Core-chain value;
//! Bob the reverse. Both legs use one payment hash, so neither party can
//! take their side without enabling the other to take theirs.
//!
//! Bob generates the secret. He is therefore the only party who can move
//! first, and doing so publishes the preimage — which is what lets Alice
//! take her side. Alice never learns the secret from the test: she reads it
//! back from her own payment, exactly as she would in production.
//!
//! Happy path only. Neither party abandons, and no CLTV ordering is
//! asserted, so this shows atomicity under cooperation rather than under
//! attack. Timeouts are a follow-on mission.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use sha2::{Digest, Sha256};
use tracing::info;

use dln_e2e_harness::bitcoind::BitcoindHarness;
use dln_e2e_harness::dln_node_client::{DlnNode, SignerMode};
use dln_e2e_harness::{relay, util};

const CHANNEL_SATS: u64 = 2_000_000;
const PUSH_MSAT: u64 = 500_000;

/// Distinct per chain, so a balance assertion cannot pass by coincidence.
const CORE_LEG_MSAT: u64 = 120_000;
const KNOTS_LEG_MSAT: u64 = 250_000;

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
        std::env::var("OUTPUT_DIR").unwrap_or_else(|_| util::unique_tmp_dir("lightning-swap")),
    );
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir)?;
    info!("Output directory: {}", output_dir.display());

    let knots_binary = util::build_knots_node()?;

    // ── Step 1: the 02.3 topology ───────────────────────────────────────
    info!("Step 1: Two chains, four nodes, a channel on each");
    let core = BitcoindHarness::start().await;
    let knots = BitcoindHarness::start_knots().await;
    let core_miner = core.get_new_address().await;
    let knots_miner = knots.get_new_address().await;
    core.mine_blocks(110, &core_miner).await;
    knots.mine_blocks(110, &knots_miner).await;

    let (_relay_container, relay_url) = relay::start_relay().await;

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

    // Bob pays on Core, Alice pays on Knots, so each funds their own leg.
    let alice_core_id = alice_core.node_id();
    let bob_knots_id = bob_knots.node_id();
    bob_core
        .open_channel(&alice_core_id, &format!("127.0.0.1:{}", alice_core.ln_port),
                      CHANNEL_SATS, Some(PUSH_MSAT))
        .await.context("core channel open failed")?;
    alice_knots
        .open_channel(&bob_knots_id, &format!("127.0.0.1:{}", bob_knots.ln_port),
                      CHANNEL_SATS, Some(PUSH_MSAT))
        .await.context("knots channel open failed")?;

    wait_for(Duration::from_secs(180), "both channels ready", || async {
        core.mine_blocks(1, &core_miner).await;
        knots.mine_blocks(1, &knots_miner).await;
        Ok(bob_core.has_ready_channel_with(&alice_core_id).await
            && alice_knots.has_ready_channel_with(&bob_knots_id).await)
    })
    .await?;
    info!("  both channels ready");

    // ── Step 2: one hash, two invoices ──────────────────────────────────
    info!("Step 2: bob generates the secret; both legs use its hash");
    let secret_bytes = Keys::generate().secret_key().to_secret_bytes();
    let secret = hex::encode(secret_bytes);
    let payment_hash = hex::encode(Sha256::digest(secret_bytes));

    let alice_before = alice_core.get_balance_msat().await?;
    let bob_before = bob_knots.get_balance_msat().await?;

    let alice_invoice = alice_core
        .make_hold_invoice(CORE_LEG_MSAT, &payment_hash, "swap: alice receives core")
        .await
        .context("alice-core make_hold_invoice failed")?;
    let bob_invoice = bob_knots
        .make_hold_invoice(KNOTS_LEG_MSAT, &payment_hash, "swap: bob receives knots")
        .await
        .context("bob-knots make_hold_invoice failed")?;

    anyhow::ensure!(
        alice_invoice != bob_invoice,
        "both legs produced the same invoice"
    );
    for (who, inv) in [("alice", &alice_invoice), ("bob", &bob_invoice)] {
        anyhow::ensure!(
            invoice_payment_hash(inv)? == payment_hash,
            "{who}'s invoice does not carry the agreed payment hash"
        );
    }
    info!("  both invoices carry {payment_hash}");

    // ── Steps 3-5: fund both legs, then settle in order ─────────────────
    // Everything must overlap. Neither payment can complete until its payee
    // settles, and bob must not settle until alice's leg is also funded —
    // otherwise he takes his side while she has nothing held.
    info!("Step 3: funding both legs");
    let started = Instant::now();

    // Bob's Core payment cannot complete until alice settles the Core leg,
    // and she cannot do that until she has seen the preimage — which only
    // appears when bob settles the Knots leg. So alice's settle must run
    // inside this concurrent block, not after it: awaiting bob's payment
    // first would deadlock.
    let bob_pays_core = bob_core.pay_invoice(&alice_invoice);

    let alice_flow = async {
        // Completes when bob settles, returning the preimage he revealed.
        let observed = alice_knots
            .pay_invoice(&bob_invoice)
            .await
            .context("alice-knots pay_invoice failed")?;
        anyhow::ensure!(
            observed == secret,
            "alice observed {observed}, expected {secret}"
        );
        info!(
            "Step 5: alice observed the preimage from her own payment after {:.1}s",
            started.elapsed().as_secs_f32()
        );

        // Settle with the observed value, never the test's own copy. This is
        // what releases bob's Core payment.
        alice_core
            .settle_hold_invoice(&observed)
            .await
            .context("alice-core settle_hold_invoice failed")?;
        Ok::<String, anyhow::Error>(observed)
    };

    let bob_settles = async {
        wait_for(Duration::from_secs(120), "both legs held", || async {
            Ok(alice_core.lookup_invoice(&payment_hash).await.is_ok()
                && bob_knots.lookup_invoice(&payment_hash).await.is_ok())
        })
        .await?;
        tokio::time::sleep(Duration::from_secs(3)).await;
        info!("Step 4: bob settles the knots leg, revealing the secret");
        bob_knots.settle_hold_invoice(&secret).await
    };

    let (core_payment, alice_result, settled) =
        tokio::join!(bob_pays_core, alice_flow, bob_settles);
    settled.context("bob-knots settle_hold_invoice failed")?;
    let observed = alice_result?;
    let core_preimage = core_payment.context("bob-core pay_invoice failed")?;
    anyhow::ensure!(
        core_preimage == observed,
        "the core leg settled with a different preimage"
    );

    // ── Step 6: both parties ended up with what they wanted ─────────────
    info!("Step 6: verifying balances on both chains");
    wait_for(Duration::from_secs(60), "balances to settle", || async {
        Ok(alice_core.get_balance_msat().await? >= alice_before + CORE_LEG_MSAT
            && bob_knots.get_balance_msat().await? >= bob_before + KNOTS_LEG_MSAT)
    })
    .await?;

    let alice_after = alice_core.get_balance_msat().await?;
    let bob_after = bob_knots.get_balance_msat().await?;
    info!("  alice on core:  {alice_before} -> {alice_after} msat");
    info!("  bob on knots:   {bob_before} -> {bob_after} msat");

    Ok(())
}

/// Extract the payment hash from a BOLT11 invoice's tagged fields.
fn invoice_payment_hash(invoice: &str) -> Result<String> {
    use lightning_invoice::Bolt11Invoice;
    use std::str::FromStr;
    let parsed = Bolt11Invoice::from_str(invoice)
        .map_err(|e| anyhow::anyhow!("invoice did not parse: {e:?}"))?;
    Ok(parsed.payment_hash().to_string())
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
