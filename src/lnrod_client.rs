use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use tonic::transport::Channel;

use crate::process::ManagedChild;
use crate::util;

pub mod admin_api {
    tonic::include_proto!("admin_api");
}

use admin_api::admin_client::AdminClient;
use admin_api::{
    ChannelCloseRequest, ChannelNewRequest, InvoiceNewRequest, PaymentKeysendRequest,
    PaymentSendRequest, PeerConnectRequest, PingRequest, Void,
};

/// Wrapper around an lnrod child process and its gRPC admin client.
pub struct LnrodNode {
    pub child: ManagedChild,
    pub proxy_child: Option<ManagedChild>,
    pub signer_child: Option<ManagedChild>,
    pub client: AdminClient<Channel>,
    pub ln_port: u16,
    pub rpc_port: u16,
}

impl LnrodNode {
    /// Build (if needed) and start an lnrod node, returning the wrapper once gRPC is ready.
    ///
    /// Uses vls-nostr-proxy and eln-signer as the signing transport.
    /// The proxy connects to lnrod's VLS gRPC port and bridges to Nostr.
    /// The signer subscribes to Nostr and runs an embedded VLS handler.
    pub async fn start(
        bitcoind_rpc_url: &str,
        log_dir: &Path,
        data_dir: &Path,
        output_dir: &Path,
        relay_url: &str,
    ) -> Result<Self> {
        let ln_port = util::free_port();
        let rpc_port = util::free_port();
        let vls_port = util::free_port();

        let binary = lnrod_binary()?;

        let ln_port_s = ln_port.to_string();
        let rpc_port_s = rpc_port.to_string();
        let vls_port_s = vls_port.to_string();
        let data_dir_s = data_dir.to_string_lossy().to_string();

        let args: Vec<&str> = vec![
            "--regtest",
            "--signer", "vls-grpc",
            "--lnport", &ln_port_s,
            "--rpcport", &rpc_port_s,
            "--vlsport", &vls_port_s,
            "--bitcoin", bitcoind_rpc_url,
            "--datadir", &data_dir_s,
        ];

        // lnrod blocks waiting for the signer transport to connect, so spawn it first.
        let child = ManagedChild::spawn("lnrod", &binary, &args, log_dir)?;

        // Generate Nostr keypairs for proxy and signer
        let proxy_keys = Keys::generate();
        let signer_keys = Keys::generate();
        let proxy_pubkey = proxy_keys.public_key().to_hex();
        let signer_pubkey = signer_keys.public_key().to_hex();
        let proxy_nsec = proxy_keys.secret_key().to_secret_hex();
        let signer_nsec = signer_keys.secret_key().to_secret_hex();

        tracing::info!("nostr proxy pubkey: {}", proxy_pubkey);
        tracing::info!("nostr signer pubkey: {}", signer_pubkey);

        // Spawn eln-signer first (it subscribes and waits for requests)
        let signer_binary = eln_signer_binary()?;
        let signer_args: Vec<&str> = vec![
            "--relay", relay_url,
            "--proxy-pubkey", &proxy_pubkey,
            "--nsec", &signer_nsec,
            "--network", "regtest",
            "--integration-test",
            "--policy-filter", "policy-commitment-htlc-routing-balance:warn",
            "--policy-filter", "policy-routing-cltv-delta:warn",
        ];
        let signer_child = ManagedChild::spawn(
            "eln-signer",
            &signer_binary,
            &signer_args,
            log_dir,
        )?;

        // Give signer time to connect to relay and subscribe
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Spawn vls-nostr-proxy (connects to lnrod's VLS gRPC port and to relay)
        let proxy_binary = nostr_proxy_binary()?;
        let proxy_connect = format!("http://127.0.0.1:{vls_port}");
        let proxy_args: Vec<&str> = vec![
            "--connect", &proxy_connect,
            "--relay", relay_url,
            "--signer-pubkey", &signer_pubkey,
            "--nsec", &proxy_nsec,
            "--network", "regtest",
        ];
        let proxy_child = ManagedChild::spawn(
            "vls-nostr-proxy",
            &proxy_binary,
            &proxy_args,
            log_dir,
        )?;

        // Once proxy connects to lnrod and signer completes init via Nostr,
        // lnrod starts the admin gRPC server.
        let endpoint = format!("http://127.0.0.1:{rpc_port}");
        let client = wait_for_grpc(&endpoint).await
            .context("lnrod gRPC did not become ready")?;

        Ok(Self {
            child,
            proxy_child: Some(proxy_child),
            signer_child: Some(signer_child),
            client,
            ln_port,
            rpc_port,
        })
    }

    /// Get this node's public key (node_id) as raw bytes.
    pub async fn node_id_bytes(&mut self) -> Result<Vec<u8>> {
        let info = self.client.node_info(Void {}).await?.into_inner();
        Ok(info.node_id)
    }

    /// Get this node's public key as hex string.
    pub async fn node_id_hex(&mut self) -> Result<String> {
        Ok(hex::encode(self.node_id_bytes().await?))
    }

    /// Connect to a peer.
    pub async fn peer_connect(&mut self, node_id: &[u8], address: &str) -> Result<()> {
        self.client
            .peer_connect(PeerConnectRequest {
                node_id: node_id.to_vec(),
                address: address.to_string(),
            })
            .await?;
        Ok(())
    }

    /// Open a new channel.
    pub async fn channel_new(
        &mut self,
        node_id: &[u8],
        value_sat: u64,
        push_msat: u64,
    ) -> Result<()> {
        self.client
            .channel_new(ChannelNewRequest {
                node_id: node_id.to_vec(),
                is_public: true,
                value_sat,
                push_msat,
            })
            .await?;
        Ok(())
    }

    /// List channels, returning the raw proto messages.
    pub async fn channel_list(&mut self) -> Result<Vec<admin_api::Channel>> {
        let reply = self.client.channel_list(Void {}).await?.into_inner();
        Ok(reply.channels)
    }

    /// Close a channel.
    pub async fn channel_close(&mut self, channel_id: &[u8], force: bool) -> Result<()> {
        self.client
            .channel_close(ChannelCloseRequest {
                channel_id: channel_id.to_vec(),
                is_force: force,
            })
            .await?;
        Ok(())
    }

    /// Create a new invoice.
    pub async fn invoice_new(&mut self, value_msat: u64) -> Result<String> {
        let reply = self
            .client
            .invoice_new(InvoiceNewRequest { value_msat })
            .await?
            .into_inner();
        Ok(reply.invoice)
    }

    /// Send a payment by bolt11 invoice string.
    pub async fn payment_send(&mut self, invoice: &str) -> Result<()> {
        self.client
            .payment_send(PaymentSendRequest {
                invoice: invoice.to_string(),
            })
            .await?;
        Ok(())
    }

    /// Send a keysend (spontaneous) payment to a node.
    pub async fn payment_keysend(&mut self, node_id: &[u8], value_msat: u64) -> Result<()> {
        self.client
            .payment_keysend(PaymentKeysendRequest {
                node_id: node_id.to_vec(),
                value_msat,
            })
            .await?;
        Ok(())
    }

    /// List all payments.
    pub async fn payment_list(&mut self) -> Result<Vec<admin_api::Payment>> {
        let reply = self.client.payment_list(Void {}).await?.into_inner();
        Ok(reply.payments)
    }

    /// Ping the node.
    pub async fn ping(&mut self) -> Result<String> {
        let reply = self
            .client
            .ping(PingRequest {
                message: "hello".to_string(),
            })
            .await?
            .into_inner();
        Ok(reply.message)
    }
}

/// Determine the lnrod binary path: LNROD_BINARY env var, or build via cargo.
fn lnrod_binary() -> Result<String> {
    if let Ok(path) = std::env::var("LNROD_BINARY") {
        tracing::info!("Using pre-built lnrod binary: {path}");
        return Ok(path);
    }

    // Default: build lnrod from the sibling VLS repo.
    let lnrod_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("system-test parent")
        .join("validating-lightning-signer/lnrod");

    tracing::info!("Building lnrod from {}", lnrod_dir.display());
    let status = std::process::Command::new("cargo")
        .args(["build", "--bin", "lnrod"])
        .current_dir(&lnrod_dir)
        .status()
        .context("failed to run cargo build for lnrod")?;

    anyhow::ensure!(status.success(), "cargo build lnrod failed with {status}");

    // lnrod is in its own target dir since it's excluded from the workspace.
    let binary = lnrod_dir.join("target/debug/lnrod");
    anyhow::ensure!(binary.exists(), "lnrod binary not found at {}", binary.display());
    Ok(binary.to_string_lossy().to_string())
}

/// Determine the vls-nostr-proxy binary path.
fn nostr_proxy_binary() -> Result<String> {
    if let Ok(path) = std::env::var("VLS_NOSTR_PROXY_BINARY") {
        tracing::info!("Using pre-built vls-nostr-proxy binary: {path}");
        return Ok(path);
    }

    let transport_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("system-test parent")
        .join("vls-nostr-transport");

    tracing::info!("Building vls-nostr-proxy from {}", transport_dir.display());
    let status = std::process::Command::new("cargo")
        .args(["build", "--bin", "vls-nostr-proxy"])
        .current_dir(&transport_dir)
        .status()
        .context("failed to run cargo build for vls-nostr-proxy")?;

    anyhow::ensure!(status.success(), "cargo build vls-nostr-proxy failed with {status}");

    let binary = transport_dir.join("target/debug/vls-nostr-proxy");
    anyhow::ensure!(binary.exists(), "vls-nostr-proxy binary not found at {}", binary.display());
    Ok(binary.to_string_lossy().to_string())
}

/// Determine the eln-signer binary path.
fn eln_signer_binary() -> Result<String> {
    if let Ok(path) = std::env::var("ELN_SIGNER_BINARY") {
        tracing::info!("Using pre-built eln-signer binary: {path}");
        return Ok(path);
    }

    let transport_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("system-test parent")
        .join("eln-signer");

    tracing::info!("Building eln-signer from {}", transport_dir.display());
    let status = std::process::Command::new("cargo")
        .args(["build", "--bin", "eln-signer"])
        .current_dir(&transport_dir)
        .status()
        .context("failed to run cargo build for eln-signer")?;

    anyhow::ensure!(status.success(), "cargo build eln-signer failed with {status}");

    let binary = transport_dir.join("target/debug/eln-signer");
    anyhow::ensure!(binary.exists(), "eln-signer binary not found at {}", binary.display());
    Ok(binary.to_string_lossy().to_string())
}

/// Retry connecting to lnrod's gRPC endpoint until a Ping succeeds.
async fn wait_for_grpc(endpoint: &str) -> Result<AdminClient<Channel>> {
    let timeout = Duration::from_secs(60);
    let start = tokio::time::Instant::now();

    loop {
        match AdminClient::connect(endpoint.to_string()).await {
            Ok(mut client) => {
                match client
                    .ping(PingRequest {
                        message: "ready?".to_string(),
                    })
                    .await
                {
                    Ok(_) => return Ok(client),
                    Err(e) => {
                        tracing::debug!("gRPC ping failed: {e}");
                    }
                }
            }
            Err(e) => {
                tracing::debug!("gRPC connect failed: {e}");
            }
        }

        if start.elapsed() > timeout {
            anyhow::bail!("gRPC endpoint {endpoint} did not become ready in {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
