use serde_json::{json, Value};
use std::time::Duration;
use testcontainers::{
    core::IntoContainerPort,
    runners::AsyncRunner,
    ContainerAsync, GenericImage,
};
use testcontainers::ImageExt;

const RPC_USER: &str = "rpcuser";
const RPC_PASS: &str = "rpcpass";
const RPC_PORT: u16 = 18443;

pub struct BitcoindHarness {
    _container: ContainerAsync<GenericImage>,
    rpc_url: String,
    rpc_port: u16,
    client: reqwest::Client,
}

/// Bitcoin Core regtest image.
const CORE_IMAGE: (&str, &str) = ("ruimarinho/bitcoin-core", "latest");

/// Bitcoin Knots regtest image, built by `docker/knots/build.sh`.
const KNOTS_IMAGE: (&str, &str) = ("dwdc/bitcoin-knots", "29");

/// Coinbase headline for an activating regtest chain. Arbitrary, but every
/// party has to agree on it — it is consensus-critical on a real chain.
const ACTIVATION_HEADLINE: &str = "dwdc regtest blake2b activation";

impl BitcoindHarness {
    /// Start a Bitcoin Core regtest node.
    pub async fn start() -> Self {
        Self::start_with(CORE_IMAGE.0, CORE_IMAGE.1, &[]).await
    }

    /// Start a Bitcoin Knots regtest node at level 0 — no
    /// `-testactivationheight`, so `Blake2bHeight` stays at `INT_MAX` and
    /// the chain produces only v1 headers (80 bytes, SHA256d).
    ///
    /// `-blake2b_headline` is mandatory: the node refuses to start without
    /// it. Its value is consensus-critical on a real chain but arbitrary on
    /// a regtest chain that never activates.
    pub async fn start_knots() -> Self {
        Self::start_with(
            KNOTS_IMAGE.0,
            KNOTS_IMAGE.1,
            &["-blake2b_headline=dwdc regtest level 0"],
        )
        .await
    }

    /// Start a Bitcoin Knots regtest node that activates BLAKE2b at
    /// `height`, so blocks below it carry v1 headers and blocks at and
    /// above it carry v2 headers.
    ///
    /// The headline is not decoration here. Consensus requires the coinbase
    /// of the activation block to contain it, so a node started with a
    /// different one rejects the chain this one builds. The internal miner
    /// puts it there, which is why `generatetoaddress` is enough.
    pub async fn start_knots_activating_at(height: u64) -> Self {
        Self::start_with(
            KNOTS_IMAGE.0,
            KNOTS_IMAGE.1,
            &[
                &format!("-testactivationheight=blake2b@{height}"),
                &format!("-blake2b_headline={ACTIVATION_HEADLINE}"),
            ],
        )
        .await
    }

    /// Start whichever backend `BITCOIND_IMPL` selects — `core` (default)
    /// or `knots`.
    ///
    /// Lets one scenario run against either chain without duplicating it.
    /// Pair with `DLN_NODE_BINARY` to use the matching node build.
    pub async fn start_from_env() -> Self {
        match std::env::var("BITCOIND_IMPL").unwrap_or_else(|_| "core".to_string()).as_str() {
            "knots" => {
                tracing::info!("BITCOIND_IMPL=knots — using the Bitcoin Knots backend");
                Self::start_knots().await
            }
            "core" => Self::start().await,
            other => panic!("unknown BITCOIND_IMPL {other:?}, expected core or knots"),
        }
    }

    /// Start a regtest node from an arbitrary image, with extra daemon args
    /// appended to the standard set.
    pub async fn start_with(image: &str, tag: &str, extra_args: &[&str]) -> Self {
        let mut cmd = vec![
            "-regtest=1".to_string(),
            "-server=1".to_string(),
            "-txindex=1".to_string(),
            "-printtoconsole".to_string(),
            "-fallbackfee=0.0002".to_string(),
            "-rpcbind=0.0.0.0".to_string(),
            "-rpcallowip=0.0.0.0/0".to_string(),
            format!("-rpcuser={RPC_USER}"),
            format!("-rpcpassword={RPC_PASS}"),
        ];
        cmd.extend(extra_args.iter().map(|a| a.to_string()));

        let container = GenericImage::new(image, tag)
            .with_exposed_port(RPC_PORT.tcp())
            .with_cmd(cmd)
            .start()
            .await
            .unwrap_or_else(|e| panic!("Failed to start {image}:{tag} container: {e}"));

        let host_port = container
            .get_host_port_ipv4(RPC_PORT)
            .await
            .expect("Failed to get mapped bitcoind RPC port");

        let rpc_url = format!("http://localhost:{host_port}");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        let harness = Self {
            _container: container,
            rpc_url,
            rpc_port: host_port,
            client,
        };

        harness.wait_until_ready().await;
        harness.create_wallet("testwallet").await;

        harness
    }

    pub async fn create_wallet(&self, wallet: &str) {
        let _ = self
            .rpc_call("createwallet", json!([wallet]))
            .await
            .expect("createwallet RPC should succeed");
    }

    pub async fn get_new_address(&self) -> String {
        self.rpc_call("getnewaddress", json!([]))
            .await
            .expect("getnewaddress RPC should succeed")
            .as_str()
            .expect("getnewaddress result should be a string")
            .to_string()
    }

    pub async fn mine_blocks(&self, blocks: u64, address: &str) {
        let _ = self
            .rpc_call("generatetoaddress", json!([blocks, address]))
            .await
            .expect("generatetoaddress RPC should succeed");
    }

    pub async fn send_to_address(&self, address: &str, amount_btc: f64) -> String {
        self.rpc_call("sendtoaddress", json!([address, amount_btc]))
            .await
            .expect("sendtoaddress RPC should succeed")
            .as_str()
            .expect("sendtoaddress result should be a txid string")
            .to_string()
    }

    pub fn rpc_host(&self) -> &'static str {
        "127.0.0.1"
    }

    pub fn rpc_port(&self) -> u16 {
        self.rpc_port
    }

    pub fn rpc_user(&self) -> &'static str {
        RPC_USER
    }

    pub fn rpc_password(&self) -> &'static str {
        RPC_PASS
    }

    /// Full RPC URL including the http:// scheme.
    pub fn rpc_url_with_auth(&self) -> String {
        format!(
            "http://{}:{}@127.0.0.1:{}/wallet/testwallet",
            RPC_USER, RPC_PASS, self.rpc_port
        )
    }

    async fn wait_until_ready(&self) {
        for _ in 0..60u32 {
            let ready = self.rpc_call("getblockchaininfo", json!([])).await.is_ok();
            if ready {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        panic!("bitcoind RPC did not become ready in time");
    }

    /// Issue an arbitrary JSON-RPC call, for assertions the typed helpers
    /// above do not cover.
    pub async fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        self.rpc_call(method, params).await
    }

    async fn rpc_call(&self, method: &str, params: Value) -> Result<Value, String> {
        let body = json!({
            "jsonrpc": "1.0",
            "id": "test",
            "method": method,
            "params": params,
        });

        let response = self
            .client
            .post(&self.rpc_url)
            .basic_auth(RPC_USER, Some(RPC_PASS))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("RPC request failed for {method}: {e}"))?;

        let status = response.status();
        let json: Value = response
            .json()
            .await
            .map_err(|e| format!("RPC response JSON decode failed for {method}: {e}"))?;

        if !status.is_success() {
            return Err(format!("RPC HTTP status error for {method}: {status}, body: {json}"));
        }

        if !json["error"].is_null() {
            return Err(format!("RPC returned error for {method}: {}", json["error"]));
        }

        Ok(json["result"].clone())
    }
}
