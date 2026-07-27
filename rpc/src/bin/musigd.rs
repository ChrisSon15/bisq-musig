use std::error::Error;
use std::sync::Arc;

use bdk_bitcoind_rpc::bitcoincore_rpc::{Auth, Client as BitcoinCoreClient};
use bmp_tracing::tracing::info;
use clap::Parser;
use rpc::server::{MusigImpl, MusigServer, WalletImpl, WalletServer};
use rpc::wallet::WalletServiceImpl;
use tonic::transport::Server;

#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
#[expect(clippy::doc_markdown, reason = "doc comments are used verbatim by Clap and not intended to be markdown")]
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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli: Cli = Cli::parse();
    bmp_tracing::init("info");
    // Create RPC client. (No connection is made at this point.)
    let rpc_client = {
        let auth = if let (Some(user), Some(pass)) = (&cli.bitcoin_rpc_user, &cli.bitcoin_rpc_pass) {
            Auth::UserPass(user.clone(), pass.clone())
        } else {
            Auth::None
        };
        BitcoinCoreClient::new(&cli.bitcoin_rpc_url, auth)?
    };

    let addr = format!("127.0.0.1:{}", cli.port).parse()?;
    let musig = MusigImpl::default();
    let wallet = WalletImpl {
        wallet_service: Arc::new(WalletServiceImpl::new()),
    };
    wallet.wallet_service.clone().spawn_connection(Arc::new(rpc_client));

    info!(port = cli.port, "Starting gRPC server.");
    Server::builder()
        .add_service(MusigServer::new(musig))
        .add_service(WalletServer::new(wallet))
        .serve(addr)
        .await?;

    Ok(())
}
