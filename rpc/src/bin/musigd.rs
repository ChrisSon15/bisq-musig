use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bdk_wallet::bitcoin::Network;
use bmp_tracing::tracing::{info, warn};
use chain::CBFScanner;
use chain::client::{ChainClient, HashCheckpoint};
use clap::Parser;
use protocol::params::{EngineConfig, EngineConfigError};
use protocol::trade_model::{MemoryTradeStore, TradeModelStore};
use rpc::bmp_wallet_service::{
    BMPWalletServiceImpl, BmpWalletImpl, BmpWalletServer, BmpWalletService as _,
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

    /// Directory holding the BMP wallet database. When set, the bisq2-facing wallet.Wallet
    /// service is served from a BMPWallet in this directory. The wallet itself is opened (or
    /// created) by the client through the OpenOrCreateWallet RPC, which carries the wallet
    /// password, so no password is taken on the command line.
    #[arg(long)]
    wallet_dir: Option<PathBuf>,

    /// Bitcoin network the BMP wallet operates on.
    #[arg(long, default_value = "regtest")]
    wallet_network: Network,

    /// Peer (host:port) of the Bitcoin network, on the network of the wallet. Repeatable. The
    /// wallet syncs through the peers with compact block filters, and the wallet and the trade
    /// service send their transactions to them. With no peer given the wallet does not sync and
    /// nothing can be sent, but every non-chain operation still works.
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
    let addr = format!("127.0.0.1:{}", cli.port).parse()?;

    // The bisq2-facing wallet service is optional: without --wallet-dir there is no BMPWallet to
    // serve.
    let trades: Arc<dyn TradeModelStore> = Arc::new(MemoryTradeStore::default());
    // All access to the chain goes through peers of the Bitcoin network (D2). One long-running
    // client serves the trade service and the broadcasts of the wallet.
    let peers = chain::parse_peers(&cli.wallet_peers)?;
    let chain_client = (!peers.is_empty() && cli.wallet_dir.is_some())
        .then(|| ChainClient::new(cli.wallet_network, peers));

    // The bisq2-facing wallet service is optional: without --wallet-dir there is no BMPWallet to
    // serve.
    let bmp_wallet = if let Some(dir) = &cli.wallet_dir {
        // The wallet service knows the trades, so that it does not release a coin that a trade
        // has reserved.
        let mut service = BMPWalletServiceImpl::new(dir.clone(), cli.wallet_network)
            .with_trade_store(Arc::clone(&trades));
        if let Some(secs) = cli.wallet_poll_secs {
            service = service.with_poll_period(Duration::from_secs(secs));
        }
        if let Some(chain_client) = &chain_client {
            info!(
                peer_count = cli.wallet_peers.len(),
                "Syncing the BMP wallet over compact block filters once it has been opened."
            );
            service = service
                .with_chain_data_source(CBFScanner::from_hostnames(&cli.wallet_peers)?)
                .with_broadcaster(Arc::new(chain_client.clone()));
        } else {
            warn!("No --wallet-peer given: the BMP wallet will not sync with the chain.");
        }

        let service = Arc::new(service);
        Arc::clone(&service).spawn_connection();
        Some(service)
    } else {
        None
    };

    // The chain client starts when the wallet has synced: near the chain tip of the wallet,
    // because no trade exists before the wallet is open, so earlier blocks are of no interest.
    if let (Some(chain_client), Some(service)) = (&chain_client, &bmp_wallet) {
        let (chain_client, service) = (chain_client.clone(), Arc::clone(service));
        tokio::spawn(async move {
            while !service.is_ready() {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            let start = service.chain_start_point().await;
            info!(?start, "Starting the chain client of the trade service.");
            chain_client.start(start.map(|(height, hash)| HashCheckpoint::new(height, hash)));
        });
    }

    // The trade service uses the wallet of the wallet service. Without a wallet it refuses every
    // request that needs one.
    let trade_wallet: Arc<dyn TradeWalletProvider> = if let Some(service) = &bmp_wallet {
        Arc::clone(service) as _
    } else {
        warn!("No --wallet-dir given: the trade service has no wallet and cannot start a trade.");
        Arc::new(NoTradeWallet)
    };
    let mut musig = MusigImpl::new(trades, trade_wallet, engine_config);
    if let Some(chain_client) = chain_client {
        musig = musig.with_chain(Arc::new(chain_client));
    } else {
        warn!("The trade service has no access to the chain and cannot publish a deposit.");
    }

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
