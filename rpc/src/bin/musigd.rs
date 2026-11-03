use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bdk_bitcoind_rpc::bitcoincore_rpc::{Auth, Client as BitcoinCoreClient};
use bdk_wallet::bitcoin::Network;
use bmp_tracing::tracing::{info, warn};
use chain::CBFScanner;
use clap::Parser;
use rpc::bmp_wallet_service::{
    BMPWalletServiceImpl, BitcoinCoreChainApi, BmpWalletImpl, BmpWalletServer,
};
use rpc::server::{MusigImpl, MusigServer, WalletImpl, WalletServer};
use rpc::wallet::WalletServiceImpl;
use tonic::transport::Server;

#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
#[expect(
    clippy::doc_markdown,
    reason = "doc comments are used verbatim by Clap and not intended to be markdown"
)]
struct Cli {
    /// The port of the MuSig daemon
    #[arg(short, long, default_value_t = 50051)]
    port: u16,

    /// Bitcoin Core RPC URL.
    #[arg(long, default_value = "http://localhost:18443")]
    bitcoin_rpc_url: String,

    /// Bitcoin Core RPC username
    #[arg(long)]
    bitcoin_rpc_user: Option<String>,

    /// Bitcoin Core RPC password
    #[arg(long)]
    bitcoin_rpc_pass: Option<String>,

    /// Directory holding the BMP wallet database. When set, the bisq2-facing wallet.Wallet
    /// service is served from a BMPWallet in this directory. The wallet itself is opened (or
    /// created) by the client through the OpenOrCreateWallet RPC, which carries the wallet
    /// password, so no password is taken on the command line.
    #[arg(long)]
    wallet_dir: Option<PathBuf>,

    /// Bitcoin network the BMP wallet operates on.
    #[arg(long, default_value = "regtest")]
    wallet_network: Network,

    /// Peer (host:port) for compact-block-filter syncing of the BMP wallet. Repeatable. With no
    /// peer given the wallet does not sync, but every non-chain operation still works.
    #[arg(long = "wallet-peer")]
    wallet_peers: Vec<String>,

    /// Seconds between BMP wallet re-syncs. Integration tests lower this so they don't have to
    /// wait out the default.
    #[arg(long)]
    wallet_poll_secs: Option<u64>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli: Cli = Cli::parse();
    bmp_tracing::init("info");
    // Create RPC client. (No connection is made at this point.)
    let rpc_client = {
        let auth = if let (Some(user), Some(pass)) = (&cli.bitcoin_rpc_user, &cli.bitcoin_rpc_pass)
        {
            Auth::UserPass(user.clone(), pass.clone())
        } else {
            Auth::None
        };
        Arc::new(BitcoinCoreClient::new(&cli.bitcoin_rpc_url, auth)?)
    };

    let addr = format!("127.0.0.1:{}", cli.port).parse()?;
    let musig = MusigImpl::default();
    let wallet = WalletImpl {
        wallet_service: Arc::new(WalletServiceImpl::new()),
    };
    wallet
        .wallet_service
        .clone()
        .spawn_connection(Arc::clone(&rpc_client));

    let mut server = Server::builder()
        .add_service(MusigServer::new(musig))
        .add_service(WalletServer::new(wallet));

    // The bisq2-facing wallet service is optional: without --wallet-dir there is no BMPWallet to
    // serve, and musigd behaves exactly as before.
    let bmp_wallet = if let Some(dir) = &cli.wallet_dir {
        let mut service = BMPWalletServiceImpl::new(dir.clone(), cli.wallet_network)
            .with_broadcaster(Arc::new(BitcoinCoreChainApi::new(Arc::clone(&rpc_client))));

        if let Some(secs) = cli.wallet_poll_secs {
            service = service.with_poll_period(Duration::from_secs(secs));
        }

        let peers = &cli.wallet_peers;
        if peers.is_empty() {
            warn!("No --wallet-peer given: the BMP wallet will not sync with the chain.");
        } else {
            info!(
                peer_count = peers.len(),
                "Syncing the BMP wallet over compact block filters once it has been opened."
            );
            service = service.with_chain_data_source(CBFScanner::from_hostnames(peers.clone()));
        }

        let service = Arc::new(service);
        Arc::clone(&service).spawn_connection(Arc::clone(&rpc_client));
        Some(service)
    } else {
        None
    };

    if let Some(service) = bmp_wallet {
        server = server.add_service(BmpWalletServer::new(BmpWalletImpl {
            wallet_service: service,
        }));
    }

    info!(port = cli.port, "Starting gRPC server.");
    server.serve(addr).await?;

    Ok(())
}
