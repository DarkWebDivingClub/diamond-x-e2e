use dln_e2e_test::{bitcoind, dln_node_client, lnrod_client, relay, util};

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::info;

use dln_e2e_test::bitcoind::BitcoindHarness;
use dln_e2e_test::dln_node_client::DlnNode;
use dln_e2e_test::lnrod_client::LnrodNode;
use dln_e2e_test::lnrod_client::admin_api::payment::PaymentStatus;

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
        std::env::var("OUTPUT_DIR")
            .unwrap_or_else(|_| util::unique_tmp_dir("system-test")),
    );
    // Always start with a clean output directory.
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir)?;
    info!("Output directory: {}", output_dir.display());

    // ── Step 1: Infrastructure ──────────────────────────────────────
    info!("Step 1: Starting infrastructure");

    let bitcoind = BitcoindHarness::start().await;
    let miner_address = bitcoind.get_new_address().await;
    bitcoind.mine_blocks(110, &miner_address).await;
    info!("  bitcoind ready, mined 110 blocks");

    let (_relay_container, relay_url) = relay::start_relay().await;
    info!("  strfry relay ready at {relay_url}");

    // ── Step 2: Start dln-node (child process) ─────────────────
    info!("Step 2: Starting dln-node");

    let mut ldk_node = DlnNode::start(&bitcoind, &miner_address, &relay_url, &output_dir)
        .await
        .context("failed to start dln-node")?;
    info!("  dln-node node_id={}", ldk_node.node_id());

    // ── Step 3: Start lnrod (child process) ─────────────────────────
    info!("Step 3: Starting lnrod");

    let lnrod_data_dir = output_dir.join("lnrod-data");
    std::fs::create_dir_all(&lnrod_data_dir)?;

    let mut lnrod = LnrodNode::start(
        &bitcoind.rpc_url_with_auth(),
        &output_dir,
        &lnrod_data_dir,
        &output_dir,
        &relay_url,
    )
    .await
    .context("failed to start lnrod")?;
    let lnrod_node_id = lnrod.node_id_hex().await?;
    info!("  lnrod node_id={lnrod_node_id}");

    // ── Step 4: Open channel (dln-node -> lnrod) ──────────────
    // dln-node opens the channel to lnrod. This matches the reference test pattern
    // (LdkService is the opener) and avoids channel type negotiation issues between
    // lnrod's LDK version and ldk-node 0.7.
    info!("Step 4: Opening channel dln-node -> lnrod");

    // Fund lnrod with on-chain BTC. This is needed for anchor channel fee bumping:
    // LDK's router won't route through an anchor channel unless the node has
    // confirmed UTXOs available for fee bumping.
    let lnrod_info = lnrod
        .client
        .node_info(lnrod_client::admin_api::Void {})
        .await?
        .into_inner();
    let lnrod_fund_addr = lnrod_info.shutdown_address;
    if !lnrod_fund_addr.is_empty() {
        bitcoind.send_to_address(&lnrod_fund_addr, 0.01).await;
        bitcoind.mine_blocks(1, &miner_address).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        info!("  funded lnrod anchor reserve at {lnrod_fund_addr}");
    }

    let lnrod_addr = format!("127.0.0.1:{}", lnrod.ln_port);
    ldk_node
        .open_channel(&lnrod_node_id, &lnrod_addr, 2_000_000, Some(500_000))
        .await
        .context("open_channel failed")?;
    info!("  channel open initiated (2M sat, push 500k sat)");

    // Mine and wait for channel to become active on both sides.
    bitcoind.mine_blocks(6, &miner_address).await;

    util::wait_until(
        "channel active",
        Duration::from_secs(60),
        Duration::from_millis(500),
        || {
            let mut lnrod_cl = lnrod.client.clone();
            let lnrod_nid = lnrod_node_id.clone();
            let miner_addr = miner_address.clone();
            let btc = &bitcoind;
            let ldk = &ldk_node;
            async move {
                // Check lnrod channels.
                let channels = match lnrod_cl
                    .channel_list(lnrod_client::admin_api::Void {})
                    .await
                {
                    Ok(resp) => resp.into_inner().channels,
                    Err(_) => return false,
                };
                let lnrod_active = channels.iter().any(|c| c.is_active && !c.is_pending);

                // Check dln-node channels via NCC.
                let ldk_ready = ldk.has_ready_channel_with(&lnrod_nid).await;

                if !lnrod_active || !ldk_ready {
                    // Mine another block to help confirmations along.
                    let _ = btc.mine_blocks(1, &miner_addr).await;
                }

                lnrod_active && ldk_ready
            }
        },
    )
    .await;
    info!("  channel active on both sides");

    // Allow gossip (channel_announcement + channel_update) to propagate between
    // the two LDK implementations. Mine additional blocks and wait so that both
    // nodes have each other's channels in their network graphs.
    info!("  waiting for gossip propagation...");
    bitcoind.mine_blocks(6, &miner_address).await;
    tokio::time::sleep(Duration::from_secs(5)).await;

    // ── Step 5: dln-node pays lnrod ───────────────────────────
    // Do the dln-node → lnrod direction first, since ldk-node's
    // internal router handles first_hops reliably. This also exercises
    // lnrod's invoice creation.
    info!("Step 5: dln-node -> lnrod payment (10M msat)");

    let lnrod_invoice = lnrod
        .invoice_new(10_000_000)
        .await
        .context("lnrod invoice_new failed")?;
    info!(
        "  lnrod invoice: {}...{}",
        &lnrod_invoice[..20],
        &lnrod_invoice[lnrod_invoice.len() - 10..]
    );

    ldk_node
        .pay_invoice(&lnrod_invoice)
        .await
        .context("ldk pay_invoice failed")?;

    // Wait for payment to appear as succeeded on lnrod side (inbound).
    util::wait_until(
        "ldk->lnrod payment succeeded",
        Duration::from_secs(30),
        Duration::from_millis(200),
        || {
            let mut cl = lnrod.client.clone();
            async move {
                let payments = match cl
                    .payment_list(lnrod_client::admin_api::Void {})
                    .await
                {
                    Ok(resp) => resp.into_inner().payments,
                    Err(_) => return false,
                };
                payments
                    .iter()
                    .any(|p| !p.is_outbound && p.status() == PaymentStatus::Succeeded)
            }
        },
    )
    .await;
    info!("  payment succeeded");

    // ── Step 6: lnrod pays dln-node (keysend) ─────────────────
    // Use keysend (spontaneous payment) instead of invoice-based payment
    // because lnrod uses LDK 0.1.8 while dln-node uses LDK 0.2.2.
    // The invoice feature bits from 0.2.2 are unrecognized by 0.1.8's
    // router, causing RouteNotFound. Keysend avoids invoice features entirely.
    info!("Step 6: lnrod -> dln-node keysend (5M msat)");

    let ldk_node_id_bytes = hex::decode(ldk_node.node_id())
        .context("ldk node_id hex decode")?;

    // Retry keysend — the route may not be available immediately.
    util::wait_until(
        "lnrod->ldk keysend succeeded",
        Duration::from_secs(60),
        Duration::from_secs(2),
        || {
            let mut cl = lnrod.client.clone();
            let dest = ldk_node_id_bytes.clone();
            async move {
                // Try to send (may fail with RouteNotFound).
                let _ = cl
                    .payment_keysend(lnrod_client::admin_api::PaymentKeysendRequest {
                        node_id: dest,
                        value_msat: 5_000_000,
                    })
                    .await;

                // Check if any outbound payment succeeded.
                let payments = match cl
                    .payment_list(lnrod_client::admin_api::Void {})
                    .await
                {
                    Ok(resp) => resp.into_inner().payments,
                    Err(_) => return false,
                };
                payments
                    .iter()
                    .any(|p| p.is_outbound && p.status() == PaymentStatus::Succeeded)
            }
        },
    )
    .await;
    info!("  keysend succeeded");

    // ── Step 7: Close channel cooperatively ─────────────────────────
    info!("Step 7: Closing channel cooperatively");

    // Close from the dln-node side (the opener) via NCC.
    let ldk_channels = ldk_node.list_channels().await?;
    anyhow::ensure!(!ldk_channels.is_empty(), "no channels to close");
    let channel_id = ldk_channels[0]["id"]
        .as_str()
        .context("channel id missing from list_channels response")?;

    ldk_node
        .close_channel(channel_id, false)
        .await
        .context("close_channel failed")?;
    info!("  close initiated");

    // Mine blocks for close confirmation.
    bitcoind.mine_blocks(6, &miner_address).await;

    util::wait_until(
        "channel closed on both sides",
        Duration::from_secs(60),
        Duration::from_millis(500),
        || {
            let mut cl = lnrod.client.clone();
            let lnrod_nid = lnrod_node_id.clone();
            let miner_addr = miner_address.clone();
            let btc = &bitcoind;
            let ldk = &ldk_node;
            async move {
                let lnrod_channels = match cl
                    .channel_list(lnrod_client::admin_api::Void {})
                    .await
                {
                    Ok(resp) => resp.into_inner().channels,
                    Err(_) => return false,
                };
                let lnrod_closed = lnrod_channels.is_empty();
                let ldk_closed = !ldk.has_channel_with(&lnrod_nid).await;

                if !lnrod_closed || !ldk_closed {
                    let _ = btc.mine_blocks(1, &miner_addr).await;
                }

                lnrod_closed && ldk_closed
            }
        },
    )
    .await;
    info!("  channel closed on both sides");

    // ── Step 8: Verify ──────────────────────────────────────────────
    info!("Step 8: Final verification");

    let ldk_balance = ldk_node.get_balance_msat().await?;
    info!("  dln-node balance: {} msat", ldk_balance);
    anyhow::ensure!(
        ldk_balance > 0,
        "dln-node on-chain balance should be > 0"
    );

    ldk_node.child.check_alive()?;
    info!("  dln-node still running");

    if let Some(ref mut signer) = ldk_node.signer_child {
        signer.check_alive()?;
        info!("  alice eln-signer still running");
    }

    lnrod.child.check_alive()?;
    info!("  lnrod still running");

    if let Some(ref mut proxy) = lnrod.proxy_child {
        proxy.check_alive()?;
        info!("  vls-nostr-proxy still running");
    }

    if let Some(ref mut signer) = lnrod.signer_child {
        signer.check_alive()?;
        info!("  bob eln-signer still running");
    }

    // Cleanup.
    ldk_node.child.kill().await;
    if let Some(ref mut signer) = ldk_node.signer_child {
        signer.kill().await;
    }
    if let Some(ref mut proxy) = lnrod.proxy_child {
        proxy.kill().await;
    }
    if let Some(ref mut signer) = lnrod.signer_child {
        signer.kill().await;
    }
    lnrod.child.kill().await;

    if std::env::var("KEEP_OUTPUT").is_err() {
        let _ = std::fs::remove_dir_all(&output_dir);
    }

    info!("All steps passed");
    Ok(())
}
