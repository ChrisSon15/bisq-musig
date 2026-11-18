pub mod client;

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

/// A peer address that cannot be used.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid peer address {peer:?}: {reason}; expected host:port")]
pub struct PeerAddressError {
    pub peer: String,
    pub reason: String,
}

/// Parses a peer address of the form `host:port`. The host is an IP address (IPv6 in brackets),
/// a host name, or `localhost`.
pub fn parse_peer(peer: &str) -> Result<TrustedPeer, PeerAddressError> {
    let peer = peer.trim();
    let invalid = |reason: String| PeerAddressError {
        peer: peer.to_owned(),
        reason,
    };
    // IP literal with port (v4 or bracketed v6): no hostname handling needed
    if let Ok(addr) = peer.parse::<SocketAddr>() {
        return Ok(TrustedPeer::from_socket_addr(addr));
    }
    let (host, port) = peer
        .rsplit_once(':')
        .ok_or_else(|| invalid("the port is missing".to_owned()))?;
    if host.is_empty() {
        return Err(invalid("the host is missing".to_owned()));
    }
    let port: u16 = port
        .parse()
        .map_err(|e| invalid(format!("the port is no number below 65536 ({e})")))?;
    if host.eq_ignore_ascii_case("localhost") {
        Ok(TrustedPeer::from_socket_addr(SocketAddr::new(
            Ipv4Addr::LOCALHOST.into(),
            port,
        )))
    } else {
        Ok(TrustedPeer::from_hostname(host, port))
    }
}

/// Parses a list of peer addresses. See [`parse_peer`].
pub fn parse_peers(peers: &[String]) -> Result<Vec<TrustedPeer>, PeerAddressError> {
    peers.iter().map(|peer| parse_peer(peer)).collect()
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
    pub fn from_hostnames(hostnames: &[String]) -> Result<Self, PeerAddressError> {
        Ok(Self::new(parse_peers(hostnames)?))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// F22: a peer address that cannot be used gives an error, not a panic.
    #[test]
    fn f22_peer_addresses_are_parsed_without_panic() {
        for valid in [
            "127.0.0.1:18444",
            "[::1]:18444",
            "localhost:18444",
            "LocalHost:18444",
            "node.example.org:8333",
            " 127.0.0.1:18444 ",
        ] {
            assert!(parse_peer(valid).is_ok(), "{valid}");
        }
        for (invalid, reason) in [
            ("127.0.0.1", "the port is missing"),
            ("node.example.org", "the port is missing"),
            ("", "the port is missing"),
            (":8333", "the host is missing"),
            ("node.example.org:port", "the port is no number"),
            ("node.example.org:70000", "the port is no number"),
        ] {
            let error = parse_peer(invalid).expect_err(invalid);
            assert!(error.reason.contains(reason), "{invalid}: {error}");
        }
        let peers = ["127.0.0.1:18444".to_owned(), "nonsense".to_owned()];
        assert_eq!(
            parse_peers(&peers).expect_err("one invalid peer").peer,
            "nonsense"
        );
        assert!(CBFScanner::from_hostnames(&peers[..1]).is_ok());
    }
}
