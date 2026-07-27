use std::collections::BTreeMap;
use std::sync::Arc;

use bdk_kyoto::bip157::{Builder, Network};
use bdk_kyoto::{
    BuilderExt as _, Info, Receiver, ScanType, TrustedPeer, UnboundedReceiver, Update, Warning,
};
use bdk_wallet::Wallet;
use bdk_wallet::bitcoin::{Transaction, Txid};
use bdk_wallet::chain::DescriptorId;
use bdk_wallet::chain::spk_client::{FullScanRequest, FullScanResponse};
use tokio::select;
/// Minimal abstraction over blockchain interaction for broadcasting transactions.
pub trait ChainApi: Send + Sync {
    fn transaction_broadcast(&self, tx: &Transaction) -> anyhow::Result<Txid>;
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
    ) -> anyhow::Result<FullScanResponse<K>>;
}

pub struct CBFScanner {
    pub peers: Vec<TrustedPeer>,
}

impl CBFScanner {
    pub const fn new(peers: Vec<TrustedPeer>) -> Self {
        Self { peers }
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
        peers: Vec<TrustedPeer>,
        wallets: Vec<(&Wallet, ScanType)>,
    ) -> anyhow::Result<BTreeMap<DescriptorId, Update>> {
        let client = Builder::new(network)
            .add_peers(peers)
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
