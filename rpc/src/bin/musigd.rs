use std::error::Error;
use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use bdk_wallet::bitcoin::Network;
use bmp_tracing::tracing::{info, warn};
use chain::client::ChainClient;
use clap::Parser;
use protocol::params::{EngineConfig, EngineConfigError};
use protocol::trade_model::{MemoryTradeStore, TradeModelStore};
use rpc::bmp_wallet_service::{BMPWalletServiceImpl, BmpWalletImpl, BmpWalletServer};
use rpc::daemon_service::{DaemonImpl, DaemonServer};
use rpc::server::{MusigImpl, MusigServer, NoTradeWallet, TradeWalletProvider};
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;

/// The longest time that `musigd` waits for the running requests after it was asked to end.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

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

    /// The seller's engine publishes the swap transaction on its own when this number of blocks,
    /// or fewer, remain before the buyer's warning transaction becomes valid, and the buyer has
    /// not answered the release. Without a value it is the release safety margin. It cannot be
    /// smaller than the release safety margin.
    #[arg(long, env = "BISQ_MUSIG_FORCED_CLOSE_MARGIN")]
    forced_close_margin: Option<u16>,
}

impl Cli {
    /// The configuration of the trade engine, from the options and the environment variables.
    fn engine_config(&self) -> Result<EngineConfig, EngineConfigError> {
        let config = EngineConfig::new(
            self.warning_lock_time,
            self.claim_lock_time,
            self.deposit_timeout,
            self.release_safety_margin,
        )?;
        match self.forced_close_margin {
            Some(margin) => config.with_forced_close_margin(margin),
            None => Ok(config),
        }
    }
}

/// Starts to listen for the signals that ask the process to end. The returned future resolves
/// with the name of the first signal that arrives: SIGTERM or SIGINT, which is Ctrl-C in a
/// terminal.
#[cfg(unix)]
fn termination_signal() -> std::io::Result<impl Future<Output = &'static str>> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = terminate.recv() => "SIGTERM",
            _ = interrupt.recv() => "SIGINT",
        }
    })
}

/// Starts to listen for Ctrl-C, the only request to end that this platform delivers.
#[cfg(not(unix))]
fn termination_signal() -> std::io::Result<impl Future<Output = &'static str>> {
    Ok(async {
        if tokio::signal::ctrl_c().await.is_err() {
            // Without a listener no request to end can arrive.
            std::future::pending::<()>().await;
        }
        "Ctrl-C"
    })
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
    // client serves the sync of the wallet, the trade service and all broadcasts. The first sync
    // of the wallet starts it, near the chain tip of the wallet: no trade exists before the
    // wallet is open, so earlier blocks are of no interest.
    let peers = chain::parse_peers(&cli.wallet_peers)?;
    let chain_client = (!peers.is_empty() && cli.wallet_dir.is_some())
        .then(|| ChainClient::new(cli.wallet_network, peers));

    // The bisq2-facing wallet service is optional: without --wallet-dir there is no BMPWallet to
    // serve.
    let bmp_wallet = if let Some(dir) = &cli.wallet_dir {
        // The wallet service knows the trades, so that it does not release a coin that a trade
        // has reserved.
        let mut service = BMPWalletServiceImpl::<ChainClient>::new(dir.clone(), cli.wallet_network)
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
                .with_chain_data_source(chain_client.clone())
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

    // The trade service uses the wallet of the wallet service. Without a wallet it refuses every
    // request that needs one.
    let trade_wallet: Arc<dyn TradeWalletProvider> = if let Some(service) = &bmp_wallet {
        Arc::clone(service) as _
    } else {
        warn!("No --wallet-dir given: the trade service has no wallet and cannot start a trade.");
        Arc::new(NoTradeWallet)
    };
    // The request to end the process in the regular way
    // (docs/specifications/trade/shutdown.md). The request `Shutdown` of the internal service
    // or a signal cancels it. Then the services end the streams of their subscriptions, and the
    // server stops to accept requests.
    let shutdown = CancellationToken::new();
    let mut musig =
        MusigImpl::new(trades, trade_wallet, engine_config).with_shutdown(shutdown.clone());
    if let Some(chain_client) = &chain_client {
        musig = musig.with_chain(Arc::new(chain_client.clone()));
    } else {
        warn!("The trade service has no access to the chain and cannot publish a deposit.");
    }

    let mut server = Server::builder()
        .add_service(MusigServer::new(musig))
        .add_service(DaemonServer::new(DaemonImpl {
            shutdown: shutdown.clone(),
        }));
    if let Some(service) = bmp_wallet {
        server = server.add_service(BmpWalletServer::new(BmpWalletImpl {
            wallet_service: service,
            shutdown: shutdown.clone(),
        }));
    }

    // The listener for the signals exists before the first request is served.
    let terminated = termination_signal()?;
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            let signal = terminated.await;
            info!(signal, "Got a signal to shut down.");
            shutdown.cancel();
        }
    });
    info!(port = cli.port, "Starting gRPC server.");
    let mut serving = pin!(server.serve_with_shutdown(addr, shutdown.clone().cancelled_owned()));
    tokio::select! {
        // The request to end is looked at first, because the server ends after it too.
        biased;
        () = shutdown.cancelled() => info!("Shutting down: no new request is accepted."),
        // Without a request to end, the server ends only with an error, for example a used port.
        result = &mut serving => return Ok(result?),
    }
    // From here on the server accepts no connection and no request, and the subscriptions have
    // ended. The server ends when the running requests have their responses. A request that is
    // still open at the limit is ended. Every change of a trade or of the wallet is stored when
    // it is made, so this loses nothing.
    let completed = tokio::time::timeout(SHUTDOWN_GRACE, serving).await.ok();
    if completed.transpose()?.is_none() {
        warn!(
            "Requests were still open {} seconds after the request to shut down. They are ended \
             now.",
            SHUTDOWN_GRACE.as_secs()
        );
    }
    if let Some(chain_client) = chain_client {
        // An error means that the client was never started, so there is nothing to end.
        let _ = chain_client.shutdown();
    }
    info!("Shut down.");

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
            "--forced-close-margin=5",
        ])
        .expect("valid command line");
        let config = cli.engine_config().expect("valid configuration");
        assert_eq!(config.warning_lock_time(), LockTime::from_height(20));
        assert_eq!(config.claim_lock_time(), LockTime::from_height(10));
        assert_eq!(config.deposit_timeout(), 12);
        assert_eq!(config.release_safety_margin(), 2);
        assert_eq!(config.forced_close_margin(), 5);

        // Without the option the forced close margin is the release safety margin.
        let cli = Cli::try_parse_from(["musigd", "--release-safety-margin=9"])
            .expect("valid command line");
        let config = cli.engine_config().expect("valid configuration");
        assert_eq!(config.forced_close_margin(), 9);

        let cli = Cli::try_parse_from([
            "musigd",
            "--release-safety-margin=9",
            "--forced-close-margin=8",
        ])
        .expect("valid command line");
        let error = cli
            .engine_config()
            .expect_err("smaller than the safety margin");
        assert!(
            error.0.contains("forced close margin of 8 blocks"),
            "{error}"
        );
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
