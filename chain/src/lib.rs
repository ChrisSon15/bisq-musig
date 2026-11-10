use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use bdk_kyoto::bip157::{Builder, Network};
use bdk_kyoto::{
    BuilderExt as _, Info, Receiver, ScanType, TrustedPeer, UnboundedReceiver, Update, Warning,
};
use bdk_wallet::Wallet;
use bdk_wallet::bitcoin::{Transaction, Txid};
use bdk_wallet::chain::DescriptorId;
use bdk_wallet::chain::spk_client::{FullScanRequest, FullScanResponse};
use thiserror::Error;
use tokio::select;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SyncError {
    #[error("failed to build compact-filter client: {0}")]
    Build(#[from] bdk_kyoto::builder::BuilderError),
    #[error("compact-filter client stopped before producing updates: {0}")]
    Updates(#[from] bdk_kyoto::UpdateError),
    #[error("failed to shut down compact-filter client: {0}")]
    Shutdown(#[from] bdk_kyoto::ClientError),
}

#[derive(Debug, Error)]
#[error(transparent)]
#[non_exhaustive]
pub enum ChainApiError {
    Backend(Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl ChainApiError {
    pub fn backend<E>(error: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Backend(Box::new(error))
    }
}

#[derive(Debug, Error)]
#[error(transparent)]
#[non_exhaustive]
pub enum FullScanError {
    Backend(Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl FullScanError {
    pub fn backend<E>(error: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Backend(Box::new(error))
    }
}

/// Minimal abstraction over blockchain interaction for broadcasting transactions.
pub trait ChainApi: Send + Sync {
    fn transaction_broadcast(&self, tx: &Transaction) -> Result<Txid, ChainApiError>;
}

/// Abstraction over the read-side of a chain backend: pre-populating a transaction cache and
/// performing a full keychain scan. Wallet-level sync routines compose these primitives instead
/// of taking a hard dependency on a specific chain client (e.g. `BdkElectrumClient`).
pub trait ChainScanner {
    /// Insert transactions into the backend's transaction cache so it will not re-fetch them.
    /// Typically used to pre-populate the cache from an existing `TxGraph` before a scan.
    fn populate_tx_cache(&self, txs: impl IntoIterator<Item = impl Into<Arc<Transaction>>>);

    /// Full scan the keychain scripts described by `request` against the chain backend and return
    /// updates suitable for applying to a `bdk_wallet` data structure.
    fn full_scan<K: Ord + Clone>(
        &self,
        request: impl Into<FullScanRequest<K>>,
        stop_gap: usize,
        batch_size: usize,
        fetch_prev_txouts: bool,
    ) -> Result<FullScanResponse<K>, FullScanError>;
}

pub struct CBFScanner {
    pub peers: Vec<TrustedPeer>,
}

impl CBFScanner {
    #[expect(
        clippy::missing_const_for_fn,
        reason = "a const CBFScanner isn't useful, as a \
        non-empty peer list can't be built at compile time; keeps the body free to grow"
    )]
    pub fn new(peers: Vec<TrustedPeer>) -> Self {
        Self { peers }
    }

    /// Builds a scanner from plain `host:port` peer addresses, so callers don't need to depend
    /// on `bdk_kyoto` just to name a peer.
    pub fn from_hostnames(hostnames: Vec<String>) -> Self {
        Self::new(
            hostnames
                .iter()
                .map(|peer| {
                    let peer = peer.trim();

                    // IP literal with port (v4 or bracketed v6): no hostname handling needed
                    if let Ok(addr) = peer.parse::<SocketAddr>() {
                        return TrustedPeer::from_socket_addr(addr);
                    }

                    let (host, port) = peer
                        .rsplit_once(':')
                        .unwrap_or_else(|| panic!("Missing host or port in peer {peer:?}"));
                    let port: u16 = port
                        .parse()
                        .unwrap_or_else(|e| panic!("Invalid port in peer {peer:?}: {e}"));

                    if host.eq_ignore_ascii_case("localhost") {
                        TrustedPeer::from_socket_addr(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port))
                    } else {
                        TrustedPeer::from_hostname(host, port)
                    }
                })
                .collect(),
        )
    }

    async fn traces(
        mut info_subscriber: Receiver<Info>,
        mut warning_subscriber: UnboundedReceiver<Warning>,
    ) {
        loop {
            select! {
                info = info_subscriber.recv() => {
                    if let Some(info) = info {
                        match info {
                            Info::Progress(p) => {
                                tracing::info!("chain height: {}, filter download progress: {}%", p.chain_height(), p.percentage_complete());
                            },
                            Info::BlockReceived(b) => {
                                tracing::info!("downloaded block: {b}");
                            },
                            _ => (),
                        }
                    } else {
                        break;
                    }
                }
                warn = warning_subscriber.recv() => {
                    if let Some(warn) = warn {
                        tracing::warn!("{warn}");
                    } else {
                        break;
                    }
                }
            }
        }
    }

    pub async fn sync_cbf(
        &self,
        network: Network,
        wallets: Vec<(&Wallet, ScanType)>,
    ) -> Result<BTreeMap<DescriptorId, Update>, SyncError> {
        let client = Builder::new(network)
            .add_peers(self.peers.iter().cloned())
            .build_with_wallets(wallets)?;

        let (client, logging, mut update_subscriber) = client.subscribe();

        tokio::task::spawn(async move {
            Self::traces(logging.info_subscriber, logging.warning_subscriber).await;
        });
        let client = client.start();
        let requester = client.requester();
        // Updates are grouped with the `DescriptorId` of the public, external descriptor.
        let updates = update_subscriber
            .updates()
            .await?
            .collect::<BTreeMap<_, _>>();

        requester.shutdown()?;
        Ok(updates)
    }
}
