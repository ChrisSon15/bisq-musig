use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bdk_bitcoind_rpc::bitcoincore_rpc::{Auth, Client as BitcoinCoreClient};
use bdk_wallet::bitcoin::Network;
use bmp_tracing::tracing::{info, warn};
use chain::CBFScanner;
use clap::Parser;
use protocol::params::{EngineConfig, EngineConfigError};
use protocol::trade_model::MemoryTradeStore;
use rpc::bmp_wallet_service::{
    BMPWalletServiceImpl, BitcoinCoreChainApi, BmpWalletImpl, BmpWalletServer,
};
use rpc::server::{MusigImpl, MusigServer, NoTradeWallet, TradeWalletProvider};
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

    /// Relative lock time of the warning transactions of a trade, in blocks after the
    /// confirmation of the deposit transaction. Both traders must use the same value.
    #[arg(
        long,
        env = "BISQ_MUSIG_WARNING_LOCK_TIME",
        default_value_t = EngineConfig::DEFAULT_WARNING_LOCK_TIME
    )]
    warning_lock_time: u16,

    /// Relative lock time of the claim transactions of a trade, in blocks after the confirmation
    /// of the warning transaction. Both traders must use the same value.
    #[arg(
        long,
        env = "BISQ_MUSIG_CLAIM_LOCK_TIME",
        default_value_t = EngineConfig::DEFAULT_CLAIM_LOCK_TIME
    )]
    claim_lock_time: u16,

    /// Number of blocks without a confirmed deposit transaction after which a trader aborts the
    /// deposit.
    #[arg(
        long,
        env = "BISQ_MUSIG_DEPOSIT_TIMEOUT",
        default_value_t = EngineConfig::DEFAULT_DEPOSIT_TIMEOUT
    )]
    deposit_timeout: u16,

    /// The seller does not release the payout when fewer blocks than this remain before the
    /// buyer's warning transaction becomes valid.
    #[arg(
        long,
        env = "BISQ_MUSIG_RELEASE_SAFETY_MARGIN",
        default_value_t = EngineConfig::DEFAULT_RELEASE_SAFETY_MARGIN
    )]
    release_safety_margin: u16,
}

impl Cli {
    /// The configuration of the trade engine, from the options and the environment variables.
    fn engine_config(&self) -> Result<EngineConfig, EngineConfigError> {
        EngineConfig::new(
            self.warning_lock_time,
            self.claim_lock_time,
            self.deposit_timeout,
            self.release_safety_margin,
        )
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli: Cli = Cli::parse();
    bmp_tracing::init("info");
    // The lock times are part of every trade, so a configuration outside the limits stops the
    // start of the daemon.
    let engine_config = cli.engine_config()?;
    info!("Configuration of the trade engine: {engine_config}.");
    // Create RPC client. (No connection is made at this point.) Only the broadcaster of the BMP
    // wallet uses it.
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

    // The bisq2-facing wallet service is optional: without --wallet-dir there is no BMPWallet to
    // serve.
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
            service = service.with_chain_data_source(CBFScanner::from_hostnames(&peers.clone()));
        }

        let service = Arc::new(service);
        Arc::clone(&service).spawn_connection();
        Some(service)
    } else {
        None
    };

    // The trade service uses the wallet of the wallet service. Without a wallet it refuses every
    // request that needs one.
    let trade_wallet: Arc<dyn TradeWalletProvider> = if let Some(service) = &bmp_wallet {
        Arc::clone(service) as _
    } else {
        warn!("No --wallet-dir given: the trade service has no wallet and cannot start a trade.");
        Arc::new(NoTradeWallet)
    };
    let musig = MusigImpl::new(
        Arc::new(MemoryTradeStore::default()),
        trade_wallet,
        engine_config,
    );

    let mut server = Server::builder().add_service(MusigServer::new(musig));
    if let Some(service) = bmp_wallet {
        server = server.add_service(BmpWalletServer::new(BmpWalletImpl {
            wallet_service: service,
        }));
    }

    info!(port = cli.port, "Starting gRPC server.");
    server.serve(addr).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use bdk_wallet::bitcoin::relative::LockTime;

    use super::*;

    /// Without options and environment variables the engine gets its default configuration.
    #[test]
    fn engine_config_has_defaults() {
        let cli = Cli::try_parse_from(["musigd"]).expect("valid command line");
        assert_eq!(cli.engine_config(), Ok(EngineConfig::default()));
    }

    #[test]
    fn engine_config_is_taken_from_the_options() {
        let cli = Cli::try_parse_from([
            "musigd",
            "--warning-lock-time=20",
            "--claim-lock-time=10",
            "--deposit-timeout=12",
            "--release-safety-margin=2",
        ])
        .expect("valid command line");
        let config = cli.engine_config().expect("valid configuration");
        assert_eq!(config.warning_lock_time(), LockTime::from_height(20));
        assert_eq!(config.claim_lock_time(), LockTime::from_height(10));
        assert_eq!(config.deposit_timeout(), 12);
        assert_eq!(config.release_safety_margin(), 2);
    }

    /// A lock time outside the limits of the specification is no valid configuration.
    #[test]
    fn engine_config_outside_the_limits_is_rejected() {
        let cli =
            Cli::try_parse_from(["musigd", "--warning-lock-time=9"]).expect("valid command line");
        let error = cli.engine_config().expect_err("9 is not larger than 3 + 6");
        assert!(error.0.contains("warning lock time of 9 blocks"), "{error}");
    }
}
