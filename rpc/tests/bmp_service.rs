//! Integration-test home of the `musigd` daemon (formerly the `rpc/src/bin/musigd.rs` binary).
//!
//! The full gRPC server — the `MuSig` trade-protocol service, the wallet service, and the two BMP
//! services — is built by [`spawn_musigd`] and backed by an external `bitcoind`, so the daemon
//! can be started as a test case rather than as a standalone process.
//!
//! The entry point is [`run_musigd_server`] — an `#[ignore]`d, long-running server on a fixed port
//! (taken from the `MUSIGD_PORT` env var, default `50051`). It serves until the process is killed,
//! replacing `cargo run --bin musigd -- --port <PORT>` for e.g. the Java integration test client.
//! Run it with, for example:
//!
//!   ```sh
//!   RPC_URL=http://127.0.0.1:18443 RPC_PASS=<pass> MUSIGD_PORT=50051 \
//!       cargo test -p rpc --test bmp_service -- --ignored run_musigd_server --nocapture
//!   ```
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bdk_bitcoind_rpc::bitcoincore_rpc::{Auth, Client as BitcoinCoreClient};
use bdk_electrum::BdkElectrumClient;
use bdk_electrum::electrum_client::Client as ElectrumClient;
use bdk_wallet::bitcoin::Amount;
use protocol::protocol_musig_adaptor::{BMPContext, BMPProtocol, ProtocolRole, Round1Parameter};
use rpc::pb::bmp_protocol::bmp_protocol_service_server::{
    BmpProtocolService, BmpProtocolServiceServer,
};
use rpc::pb::bmp_protocol::{self, InitializeRequest, InitializeResponse, Role};
use rpc::pb::convert::TryProtoInto as _;
use rpc::server::{MusigImpl, MusigServer, WalletImpl, WalletServer};
use rpc::wallet::WalletServiceImpl;
use tokio::net::TcpListener;
use tokio::task::{self, JoinHandle};
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;
use tonic::{Request, Response, Result, Status, transport};
use tracing::info;
use wallet::protocol_wallet_api::MemWallet;

/// Create a fresh in-memory wallet, fund it from the given bitcoind RPC client, and sync until
/// the funds confirm. Previously abstracted behind the `chain::ChainFunding` trait; inlined here
/// as this integration test is its only caller.
fn funded_mem_wallet(
    client: BdkElectrumClient<ElectrumClient>,
    rpc: &BitcoinCoreClient,
) -> anyhow::Result<MemWallet> {
    use bdk_bitcoind_rpc::bitcoincore_rpc::RpcApi as _;

    const MAX_RETRIES: u32 = 20;
    const RETRY_DELAY_MS: u64 = 500;

    let mut wallet = MemWallet::new(client)?;
    let address = wallet.reveal_next_address();
    rpc.send_to_address(
        &address,
        Amount::from_btc(10f64).unwrap(),
        None,
        None,
        None,
        None,
        None,
        None,
    )?;
    rpc.generate_to_address(1, &address)?;

    for attempt in 0..MAX_RETRIES {
        wallet.sync()?;
        if wallet.balance() > Amount::from_sat(0) {
            info!("Wallet funded after {attempt} retries");
            return Ok(wallet);
        }
        if attempt < MAX_RETRIES - 1 {
            std::thread::sleep(Duration::from_millis(RETRY_DELAY_MS));
        }
    }
    anyhow::bail!("Wallet failed to sync funded balance after {MAX_RETRIES} attempts")
}

pub struct BmpServiceImpl {
    // Each trade protocol is stored against a unique ID.
    active_protocols: Mutex<HashMap<String, BMPProtocol>>,
    bitcoin_rpc_client: Arc<BitcoinCoreClient>,
    electrum_url: String,
}

impl BmpServiceImpl {
    pub fn new(client: Arc<BitcoinCoreClient>, electrum_url: String) -> Self {
        Self {
            active_protocols: Mutex::new(HashMap::new()),
            bitcoin_rpc_client: client,
            electrum_url,
        }
    }
}

#[tonic::async_trait]
impl BmpProtocolService for BmpServiceImpl {
    async fn initialize(
        &self,
        request: Request<InitializeRequest>,
    ) -> Result<Response<InitializeResponse>> {
        let req = request.into_inner();
        info!("Received initialize request: {req:?}");

        let client = BdkElectrumClient::new(ElectrumClient::new(&self.electrum_url).unwrap());
        let client2 = BdkElectrumClient::new(ElectrumClient::new(&self.electrum_url).unwrap());

        let mock_wallet = funded_mem_wallet(client, self.bitcoin_rpc_client.as_ref())
            .map_err(|e: anyhow::Error| Status::internal(e.to_string()))?;

        info!(
            "mock_wallet initialized balance: {:?}",
            mock_wallet.balance()
        );

        let chain = Box::new(testenv::Testchain::new(client2));

        let role =
            Role::try_from(req.role).map_err(|_| Status::invalid_argument("Unrecognised role"))?;
        let role = match role {
            Role::Seller => ProtocolRole::Seller,
            Role::Buyer => ProtocolRole::Buyer,
        };

        let context = BMPContext::new(
            chain,
            Box::new(mock_wallet),
            role,
            Amount::from_sat(req.seller_amount_sats),
            Amount::from_sat(req.buyer_amount_sats),
        ).map_err(|e| Status::internal(e.to_string()))?;

        let protocol = BMPProtocol::new(context).map_err(|e| Status::internal(e.to_string()))?;

        let trade_id = &req.trade_id;
        if trade_id.is_empty() {
            return Err(Status::invalid_argument("Trade ID must not be empty"));
        }
        self.active_protocols
            .lock()
            .unwrap()
            .insert(trade_id.clone(), protocol);

        Ok(Response::new(InitializeResponse {
            trade_id: trade_id.clone(),
        }))
    }

    async fn execute_round1(
        &self,
        request: Request<bmp_protocol::Round1Request>,
    ) -> Result<Response<bmp_protocol::Round1Response>> {
        let req = request.into_inner();
        let trade_id = req.trade_id;

        let mut protocols = self.active_protocols.lock().unwrap();
        let protocol = protocols
            .get_mut(&trade_id)
            .ok_or_else(|| Status::not_found(format!("Trade not found: {trade_id}")))?;

        let round1_result = protocol
            .round1()
            .map_err(|e| Status::aborted(e.to_string()))?;

        drop(protocols);
        Ok(Response::new(round1_result.try_into()?))
    }

    async fn execute_round2(
        &self,
        request: Request<bmp_protocol::Round2Request>,
    ) -> Result<Response<bmp_protocol::Round2Response>> {
        let req = request.into_inner();
        let trade_id = req.trade_id;

        let mut protocols = self.active_protocols.lock().unwrap();
        let protocol = protocols
            .get_mut(&trade_id)
            .ok_or_else(|| Status::not_found(format!("Trade not found: {trade_id}")))?;

        let peer_round1_params: Round1Parameter =
            req.peer_round1_response.unwrap().try_proto_into()?;

        let round2_result = protocol
            .round2(peer_round1_params)
            .map_err(|e| Status::aborted(e.to_string()))?;

        drop(protocols);
        Ok(Response::new(round2_result.try_into()?))
    }

    async fn execute_round3(
        &self,
        request: Request<bmp_protocol::Round3Request>,
    ) -> Result<Response<bmp_protocol::Round3Response>> {
        let req = request.into_inner();
        let trade_id = req.trade_id;

        let mut protocols = self.active_protocols.lock().unwrap();
        let protocol = protocols
            .get_mut(&trade_id)
            .ok_or_else(|| Status::not_found(format!("Trade not found: {trade_id}")))?;

        let peer_round2_params = req.peer_round2_response.unwrap().try_proto_into()?;

        let round3_result = protocol
            .round3(peer_round2_params)
            .map_err(|e| Status::aborted(e.to_string()))?;

        drop(protocols);
        Ok(Response::new(round3_result.try_into()?))
    }

    async fn execute_round4(
        &self,
        request: Request<bmp_protocol::Round4Request>,
    ) -> Result<Response<bmp_protocol::Round4Response>> {
        let req = request.into_inner();
        let trade_id = req.trade_id;

        let mut protocols = self.active_protocols.lock().unwrap();
        let protocol = protocols
            .get_mut(&trade_id)
            .ok_or_else(|| Status::not_found(format!("Trade not found: {trade_id}")))?;

        let peer_round3_params = req.peer_round3_response.unwrap().try_proto_into()?;

        let round4_result = protocol
            .round4(peer_round3_params)
            .map_err(|e| Status::aborted(e.to_string()))?;

        drop(protocols);
        Ok(Response::new(round4_result.try_into()?))
    }

    async fn execute_round5(
        &self,
        request: Request<bmp_protocol::Round5Request>,
    ) -> Result<Response<bmp_protocol::ExecuteRound5Response>> {
        let req = request.into_inner();
        let trade_id = req.trade_id;

        let mut protocols = self.active_protocols.lock().unwrap();
        let protocol = protocols
            .get_mut(&trade_id)
            .ok_or_else(|| Status::not_found(format!("Trade not found: {trade_id}")))?;

        let peer_round4_params = req.peer_round4_response.unwrap().try_proto_into()?;

        info!("Round 5 params {:#?}", peer_round4_params.deposit_tx_signed.clone().extract_tx().unwrap());
        protocol
            .round5(peer_round4_params)
            .map_err(|e| Status::aborted(e.to_string()))?;

        drop(protocols);
        Ok(Response::new(bmp_protocol::ExecuteRound5Response {}))
    }
}

/// Builds and spawns the full `musigd` gRPC server, mirroring the former `musigd` binary's
/// `main`, but on a caller-supplied listener (instead of a fixed port) so it can run inside a
/// test harness.
fn spawn_musigd(
    listener: TcpListener,
    client: Arc<BitcoinCoreClient>,
    electrum_url: String,
) -> JoinHandle<Result<(), transport::Error>> {
    let musig = MusigImpl::default();
    let wallet = WalletImpl {
        wallet_service: Arc::new(WalletServiceImpl::new()),
    };

    wallet
        .wallet_service
        .clone()
        .spawn_connection(client.clone());

    let bmp_protocol_impl = BmpServiceImpl::new(client, electrum_url);

    let incoming = TcpIncoming::from(listener);
    let handle = task::spawn(async move {
        Server::builder()
            .add_service(MusigServer::new(musig))
            .add_service(WalletServer::new(wallet))
            .add_service(BmpProtocolServiceServer::new(bmp_protocol_impl))
            .serve_with_incoming(incoming)
            .await
    });
    handle
}

/// Long-running server on a fixed port, the test-case replacement for the old `musigd` binary.
///
/// Ignored by default so it doesn't block the test suite; run it explicitly when you need a live
/// server (e.g. for the Java integration test client). The port is read from the `MUSIGD_PORT`
/// env var, defaulting to `50051`. Serves until the process is killed.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "long-running server; start manually for the Java integration test client"]
async fn run_musigd_server() -> Result<()> {
    bmp_tracing::init("info");

    let port: u16 = std::env::var("MUSIGD_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(50051);

    let rpc_url = std::env::var("RPC_URL").ok();
    let electrum_url = std::env::var("ELECTRUM_URL").ok();
    let rpc_pass = Some("bitcoin");
    let rpc_user = Some("bitcoin");

    // Create RPC client
    let rpc_client = rpc_url
        .as_ref()
        .map(|rpc_url| {
            info!(rpc_url, "Connecting to external Bitcoin Core RPC");

            let auth = if let (Some(user), Some(pass)) = (rpc_user, rpc_pass) {
                Auth::UserPass(user.to_owned(), pass.to_owned())
            } else {
                let home = std::env::var_os("HOME")
                    .map(std::path::PathBuf::from)
                    .expect("Can't determine home directory for cookie-file fallback; set RPC_PASS");
                Auth::CookieFile(home.join(".bitcoin").join(".cookie"))
            };

            BitcoinCoreClient::new(rpc_url, auth)
                .expect("Failed to construct Bitcoin Core RPC client")
        })
        .expect("RPC_URL must be set");

    let listener = TcpListener::bind(("127.0.0.1", port)).await?;

    info!(port, "Starting musigd gRPC server.");
    let _ = spawn_musigd(
        listener,
        Arc::new(rpc_client),
        electrum_url.unwrap(),
    ).await;

    Ok(())
}
