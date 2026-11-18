//! A long-running compact block filter client for the trade engine.
//!
//! The client follows the chain of block headers, tests the filter of every block for the watched
//! scripts, loads the blocks that match, and keeps the transactions that pay to a watched script
//! or spend a watched output. It sees confirmed transactions only. It sends transactions to the
//! peers and learns no result of that.
//!
//! The rules are specified in `docs/specifications/trade/chain-observation.md`.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bdk_kyoto::bip157::chain::{BlockHeaderChanges, ChainState};
use bdk_kyoto::bip157::{Builder, Event, Network, Package};
pub use bdk_kyoto::{HashCheckpoint, TrustedPeer};
use bdk_kyoto::{Info, Receiver, Requester, UnboundedReceiver, Warning};
use bdk_wallet::bitcoin::{Block, BlockHash, OutPoint, ScriptBuf, Transaction, Txid};
use tokio::sync::broadcast;

use crate::{ChainApi, ChainApiError};

/// How often the client asks a peer again for a block that it did not get.
const BLOCK_FETCH_ATTEMPTS: usize = 3;
/// The pause before the client asks again for a block.
const BLOCK_FETCH_RETRY_DELAY: Duration = Duration::from_secs(2);
/// The number of events that a slow subscriber can fall behind before it loses events.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// A transaction that the client found in a block of the chain with the most work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmedTx {
    pub tx: Arc<Transaction>,
    pub height: u32,
    pub block_hash: BlockHash,
}

/// What a subscriber of the client is told.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChainEvent {
    /// The client has evaluated all blocks up to this height.
    Synced { height: u32 },
    /// A transaction that pays to a watched script or spends a watched output was found in a
    /// block.
    Confirmed { txid: Txid, height: u32 },
    /// The blocks from this height on were replaced by other blocks. The client has forgotten the
    /// transactions that it found in them. It reports a transaction again if a new block
    /// contains it.
    Reorganized { from_height: u32 },
}

/// What the client knows about the chain: the tip, the watched scripts and outputs, and the
/// transactions that it found. This part has no network access.
#[derive(Default)]
struct ChainIndex {
    /// The last block header of the chain with the most work.
    tip: Option<(u32, BlockHash)>,
    /// The height up to which all blocks are evaluated.
    synced_height: Option<u32>,
    scripts: HashSet<ScriptBuf>,
    /// Outputs that are watched although the client did not see the transaction that created
    /// them.
    outpoints: HashSet<OutPoint>,
    txs: HashMap<Txid, ConfirmedTx>,
    /// The transaction that spends an output, for watched outputs.
    spends: HashMap<OutPoint, Txid>,
}

impl ChainIndex {
    /// Whether `outpoint` is a watched output: named by the caller, or an output with a watched
    /// script of a transaction that the client found.
    fn is_watched_output(&self, outpoint: &OutPoint) -> bool {
        self.outpoints.contains(outpoint)
            || self.txs.get(&outpoint.txid).is_some_and(|found| {
                found
                    .tx
                    .output
                    .get(outpoint.vout as usize)
                    .is_some_and(|output| self.scripts.contains(&output.script_pubkey))
            })
    }

    /// Evaluates a block of the chain with the most work. Returns the IDs of the transactions
    /// that are relevant and were not known in this block before.
    fn apply_block(&mut self, height: u32, block: &Block) -> Vec<Txid> {
        let block_hash = block.block_hash();
        let mut found = Vec::new();
        // The transactions of a block are in an order in which a transaction comes after the
        // transactions that it spends, so one pass finds chains inside the block.
        for tx in &block.txdata {
            let txid = tx.compute_txid();
            let spent = tx
                .input
                .iter()
                .map(|input| input.previous_output)
                .filter(|outpoint| self.is_watched_output(outpoint))
                .collect::<Vec<_>>();
            let pays_to_watched_script = tx
                .output
                .iter()
                .any(|output| self.scripts.contains(&output.script_pubkey));
            if spent.is_empty() && !pays_to_watched_script {
                continue;
            }
            for outpoint in spent {
                self.spends.insert(outpoint, txid);
            }
            let confirmed = ConfirmedTx {
                tx: Arc::new(tx.clone()),
                height,
                block_hash,
            };
            if self.txs.insert(txid, confirmed.clone()) != Some(confirmed) {
                found.push(txid);
            }
        }
        found
    }

    /// Forgets everything that was found in blocks at or above `height`.
    fn disconnect_from(&mut self, height: u32) {
        self.txs.retain(|_, found| found.height < height);
        let txs = &self.txs;
        self.spends.retain(|_, txid| txs.contains_key(txid));
        self.synced_height = match self.synced_height {
            Some(synced) if synced >= height => height.checked_sub(1),
            other => other,
        };
    }

    /// The number of confirmations of a transaction that the client found, or 0.
    fn confirmations(&self, txid: &Txid) -> u32 {
        match (self.txs.get(txid), self.tip) {
            (Some(found), Some((tip_height, _))) if tip_height >= found.height => {
                tip_height - found.height + 1
            }
            _ => 0,
        }
    }
}

/// A problem with the peers that the client reports while it keeps trying.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerProblem {
    /// No peer is connected.
    NoPeerReachable,
    /// A peer is connected, but it does not serve compact block filters.
    NoCompactFilters,
}

/// The future of a broadcast.
pub type BroadcastFuture = Pin<Box<dyn Future<Output = Result<Txid, ChainApiError>> + Send>>;

/// The access to the chain that the trade engine and its service need: watching scripts and
/// outputs, reading what was found, events, and sending transactions.
///
/// [`ChainClient`] implements it with compact block filters. A test can implement it without a
/// network.
pub trait ChainAccess: Send + Sync {
    /// Watches a script: every transaction with an output to it, and every transaction that
    /// spends such an output of a transaction that was found.
    fn watch_script(&self, script: ScriptBuf);

    /// Watches an output that is known to the caller, with its script.
    fn watch_output(&self, outpoint: OutPoint, script: ScriptBuf);

    /// The height of the last block of the chain with the most work, if the chain is known.
    fn tip_height(&self) -> Option<u32>;

    /// The height up to which all blocks are evaluated.
    fn synced_height(&self) -> Option<u32>;

    /// A relevant transaction that was found in a block.
    fn transaction(&self, txid: &Txid) -> Option<ConfirmedTx>;

    /// The number of confirmations of a transaction that was found, or 0.
    fn confirmations(&self, txid: &Txid) -> u32;

    /// The confirmed transaction that spends a watched output.
    fn spending_tx(&self, outpoint: &OutPoint) -> Option<ConfirmedTx>;

    /// Subscribes to the events of the chain.
    fn subscribe(&self) -> broadcast::Receiver<ChainEvent>;

    /// Sends a transaction to the network. A success means that the transaction was sent, not
    /// that a node accepted it.
    fn broadcast(&self, tx: Transaction) -> BroadcastFuture;
}

struct Shared {
    index: Mutex<ChainIndex>,
    peer_problem: Mutex<Option<PeerProblem>>,
    events: broadcast::Sender<ChainEvent>,
    /// Set when the client was started.
    requester: OnceLock<Requester>,
}

/// The error of a request that needs the running client.
#[derive(Debug, thiserror::Error)]
#[error("the compact block filter client is not started yet")]
pub struct NotStarted;

/// The long-running compact block filter client. Cloning gives another handle to the same
/// client.
///
/// A client is created without network access, so that scripts can be watched and events can be
/// subscribed to at once. [`Self::start`] connects it to the peers.
#[derive(Clone)]
pub struct ChainClient {
    network: Network,
    peers: Vec<TrustedPeer>,
    shared: Arc<Shared>,
}

impl ChainClient {
    /// Creates a client for `network` that will connect to `peers`. It does nothing until
    /// [`Self::start`] is called.
    pub fn new(network: Network, peers: Vec<TrustedPeer>) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            network,
            peers,
            shared: Arc::new(Shared {
                index: Mutex::new(ChainIndex::default()),
                peer_problem: Mutex::new(None),
                events,
                requester: OnceLock::new(),
            }),
        }
    }

    /// Creates a client and starts it at once. See [`Self::start`].
    pub fn started(
        network: Network,
        peers: Vec<TrustedPeer>,
        start: Option<HashCheckpoint>,
    ) -> Self {
        let client = Self::new(network, peers);
        client.start(start);
        client
    }

    /// Starts the client on the current Tokio runtime. It connects to the peers and follows the
    /// chain strictly after `start`, or from the first block of the chain if `start` is `None`.
    /// A second call does nothing.
    ///
    /// # Panics
    /// If it is called outside of a Tokio runtime.
    pub fn start(&self, start: Option<HashCheckpoint>) {
        if self.is_started() {
            return;
        }
        let mut builder = Builder::new(self.network).add_peers(self.peers.iter().cloned());
        if let Some(start) = start {
            builder = builder.chain_state(ChainState::Checkpoint(start));
        }
        let (node, client) = builder.build();
        if self.shared.requester.set(client.requester.clone()).is_err() {
            // Another caller started the client in the meantime.
            return;
        }
        tokio::spawn(async move {
            if let Err(error) = node.run().await {
                tracing::error!(%error, "The compact block filter client stopped.");
            }
        });
        tokio::spawn(log_and_track_peers(
            client.info_rx,
            client.warn_rx,
            Arc::clone(&self.shared),
        ));
        tokio::spawn(process_events(
            client.event_rx,
            client.requester,
            Arc::clone(&self.shared),
        ));
    }

    /// Whether [`Self::start`] was called.
    pub fn is_started(&self) -> bool {
        self.shared.requester.get().is_some()
    }

    fn requester(&self) -> Result<&Requester, ChainApiError> {
        self.shared
            .requester
            .get()
            .ok_or_else(|| ChainApiError::backend(NotStarted))
    }

    /// Watches a script: every transaction with an output to it, and every transaction that
    /// spends such an output of a transaction that the client found.
    ///
    /// Only blocks that are evaluated after this call are tested for the script. Use
    /// [`Self::rescan_from`] to evaluate earlier blocks again.
    pub fn watch_script(&self, script: ScriptBuf) {
        self.shared.index.lock().unwrap().scripts.insert(script);
    }

    /// Watches an output that is known to the caller, with its script. The client reports the
    /// transaction that spends it, also if it never saw the transaction that created it.
    pub fn watch_output(&self, outpoint: OutPoint, script: ScriptBuf) {
        let mut index = self.shared.index.lock().unwrap();
        index.outpoints.insert(outpoint);
        index.scripts.insert(script);
    }

    /// Evaluates the blocks above `height` again, for scripts that were added after the client
    /// had passed them, or after a start. The client is not synced until it reports
    /// [`ChainEvent::Synced`] again.
    pub fn rescan_from(&self, height: u32) -> Result<(), ChainApiError> {
        {
            let mut index = self.shared.index.lock().unwrap();
            if index.synced_height.is_some_and(|synced| synced > height) {
                index.synced_height = Some(height);
            }
        }
        self.requester()?
            .rescan_from(height)
            .map_err(ChainApiError::backend)
    }

    /// The height of the last block header of the chain with the most work.
    pub fn tip_height(&self) -> Option<u32> {
        self.shared
            .index
            .lock()
            .unwrap()
            .tip
            .map(|(height, _)| height)
    }

    /// The height up to which the client has evaluated all blocks. A transaction that the client
    /// does not know is not confirmed at or below this height, if its script or its inputs were
    /// watched while the blocks were evaluated.
    pub fn synced_height(&self) -> Option<u32> {
        self.shared.index.lock().unwrap().synced_height
    }

    /// A relevant transaction that the client found in a block.
    pub fn transaction(&self, txid: &Txid) -> Option<ConfirmedTx> {
        self.shared.index.lock().unwrap().txs.get(txid).cloned()
    }

    /// The number of confirmations of a transaction that the client found, or 0 if it did not
    /// find the transaction.
    pub fn confirmations(&self, txid: &Txid) -> u32 {
        self.shared.index.lock().unwrap().confirmations(txid)
    }

    /// The confirmed transaction that spends a watched output.
    pub fn spending_tx(&self, outpoint: &OutPoint) -> Option<ConfirmedTx> {
        let index = self.shared.index.lock().unwrap();
        index
            .spends
            .get(outpoint)
            .and_then(|txid| index.txs.get(txid))
            .cloned()
    }

    /// A problem with the peers, if there is one at the moment. The client keeps trying.
    pub fn peer_problem(&self) -> Option<PeerProblem> {
        self.shared.peer_problem.lock().unwrap().clone()
    }

    /// Subscribes to the events of the client. A subscriber that falls too far behind loses
    /// events and should then read the state of the client again.
    pub fn subscribe(&self) -> broadcast::Receiver<ChainEvent> {
        self.shared.events.subscribe()
    }

    /// Waits until the client has evaluated all blocks up to the height `height`.
    pub async fn wait_until_synced_to(&self, height: u32) -> Result<(), ChainApiError> {
        let mut events = self.subscribe();
        loop {
            if self.synced_height().is_some_and(|synced| synced >= height) {
                return Ok(());
            }
            match events.recv().await {
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(error @ broadcast::error::RecvError::Closed) => {
                    return Err(ChainApiError::backend(error));
                }
            }
        }
    }

    /// Sends a transaction to the peers. This returns when the transaction was sent to at least
    /// one peer. It does not tell whether a node accepted the transaction.
    pub async fn broadcast(&self, tx: Transaction) -> Result<Txid, ChainApiError> {
        let txid = tx.compute_txid();
        self.requester()?
            .submit_package(tx)
            .await
            .map_err(ChainApiError::backend)?;
        Ok(txid)
    }

    /// Sends a parent transaction together with a child that spends one of its outputs, so that
    /// the fee of the child also pays for the parent. Returns the ID of the child.
    pub async fn broadcast_package(
        &self,
        parent: Transaction,
        child: Transaction,
    ) -> Result<Txid, ChainApiError> {
        let txid = child.compute_txid();
        let package = Package::new_one_parent_one_child(parent, child).map_err(|error| {
            ChainApiError::backend(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                error.to_string(),
            ))
        })?;
        self.requester()?
            .submit_package(package)
            .await
            .map_err(ChainApiError::backend)?;
        Ok(txid)
    }

    /// Stops the client.
    pub fn shutdown(&self) -> Result<(), ChainApiError> {
        self.requester()?.shutdown().map_err(ChainApiError::backend)
    }
}

impl ChainAccess for ChainClient {
    fn watch_script(&self, script: ScriptBuf) {
        Self::watch_script(self, script);
    }

    fn watch_output(&self, outpoint: OutPoint, script: ScriptBuf) {
        Self::watch_output(self, outpoint, script);
    }

    fn tip_height(&self) -> Option<u32> {
        Self::tip_height(self)
    }

    fn synced_height(&self) -> Option<u32> {
        Self::synced_height(self)
    }

    fn transaction(&self, txid: &Txid) -> Option<ConfirmedTx> {
        Self::transaction(self, txid)
    }

    fn confirmations(&self, txid: &Txid) -> u32 {
        Self::confirmations(self, txid)
    }

    fn spending_tx(&self, outpoint: &OutPoint) -> Option<ConfirmedTx> {
        Self::spending_tx(self, outpoint)
    }

    fn subscribe(&self) -> broadcast::Receiver<ChainEvent> {
        Self::subscribe(self)
    }

    fn broadcast(&self, tx: Transaction) -> BroadcastFuture {
        let client = self.clone();
        Box::pin(async move { Self::broadcast(&client, tx).await })
    }
}

impl ChainApi for ChainClient {
    /// Sends the transaction to the peers. This must be called from a thread that may block, for
    /// example inside `tokio::task::block_in_place`.
    fn transaction_broadcast(&self, tx: &Transaction) -> Result<Txid, ChainApiError> {
        tokio::runtime::Handle::current().block_on(self.broadcast(tx.clone()))
    }
}

/// Writes the messages of the client to the log and keeps the last problem with the peers.
async fn log_and_track_peers(
    mut info_rx: Receiver<Info>,
    mut warn_rx: UnboundedReceiver<Warning>,
    shared: Arc<Shared>,
) {
    let set_problem = |problem: Option<PeerProblem>| {
        *shared.peer_problem.lock().unwrap() = problem;
    };
    loop {
        tokio::select! {
            info = info_rx.recv() => match info {
                Some(Info::ConnectionsMet | Info::SuccessfulHandshake) => set_problem(None),
                Some(Info::Progress(progress)) => tracing::debug!(
                    chain_height = progress.chain_height(),
                    percent = progress.percentage_complete(),
                    "Filter download progress."
                ),
                Some(info) => tracing::debug!("{info}"),
                None => break,
            },
            warning = warn_rx.recv() => match warning {
                Some(Warning::NeedConnections { connected: 0, .. }) => {
                    tracing::warn!("No peer of the Bitcoin network is reachable.");
                    set_problem(Some(PeerProblem::NoPeerReachable));
                }
                Some(Warning::NoCompactFilters) => {
                    tracing::warn!("The connected peer does not serve compact block filters.");
                    set_problem(Some(PeerProblem::NoCompactFilters));
                }
                Some(warning) => tracing::warn!("{warning}"),
                None => break,
            },
        }
    }
}

/// Follows the events of the node: header changes, filters and the end of a filter scan.
async fn process_events(
    mut event_rx: UnboundedReceiver<Event>,
    requester: Requester,
    shared: Arc<Shared>,
) {
    // The blocks whose filter matched a watched script, with their height.
    let mut matched_blocks: Vec<(u32, BlockHash)> = Vec::new();
    let send = |event: ChainEvent| {
        // An error means that nobody is subscribed.
        let _ = shared.events.send(event);
    };
    while let Some(event) = event_rx.recv().await {
        match event {
            Event::ChainUpdate(BlockHeaderChanges::Connected(header)) => {
                shared.index.lock().unwrap().tip = Some((header.height, header.block_hash()));
            }
            Event::ChainUpdate(BlockHeaderChanges::Reorganized {
                accepted,
                reorganized,
            }) => {
                let Some(from_height) = reorganized.iter().map(|header| header.height).min() else {
                    continue;
                };
                {
                    let mut index = shared.index.lock().unwrap();
                    index.disconnect_from(from_height);
                    index.tip = accepted
                        .iter()
                        .max_by_key(|header| header.height)
                        .map(|header| (header.height, header.block_hash()));
                }
                matched_blocks.retain(|(height, _)| *height < from_height);
                tracing::warn!(from_height, "The chain was reorganised.");
                send(ChainEvent::Reorganized { from_height });
            }
            Event::ChainUpdate(BlockHeaderChanges::ForkAdded(_)) => {}
            Event::IndexedFilter(filter) => {
                let matches = {
                    let index = shared.index.lock().unwrap();
                    // A filter test with no script reports a match, so it is skipped.
                    !index.scripts.is_empty() && filter.contains_any(index.scripts.iter())
                };
                if matches {
                    matched_blocks.push((filter.height(), filter.block_hash()));
                }
            }
            Event::FiltersSynced(update) => {
                // The blocks are evaluated in the order of the chain, so that a transaction is
                // known before the transaction that spends it.
                matched_blocks.sort_unstable();
                matched_blocks.dedup();
                let mut missing = Vec::new();
                for (height, block_hash) in std::mem::take(&mut matched_blocks) {
                    let Some(block) = fetch_block(&requester, block_hash).await else {
                        missing.push((height, block_hash));
                        continue;
                    };
                    let found = shared.index.lock().unwrap().apply_block(height, &block);
                    for txid in found {
                        send(ChainEvent::Confirmed { txid, height });
                    }
                }
                // A block that could not be loaded is asked for again after the next scan. The
                // client does not report a height as synced that lies above such a block.
                let synced_height = missing
                    .iter()
                    .map(|(height, _)| height.saturating_sub(1))
                    .min()
                    .unwrap_or(update.tip.height);
                matched_blocks = missing;
                {
                    let mut index = shared.index.lock().unwrap();
                    index.tip = Some((update.tip.height, update.tip.hash));
                    index.synced_height = Some(synced_height);
                }
                send(ChainEvent::Synced {
                    height: synced_height,
                });
            }
        }
    }
    tracing::info!("The event stream of the compact block filter client ended.");
}

/// Loads a block from a peer, with a few attempts.
async fn fetch_block(requester: &Requester, block_hash: BlockHash) -> Option<Block> {
    for attempt in 1..=BLOCK_FETCH_ATTEMPTS {
        match requester.get_block(block_hash).await {
            Ok(indexed_block) => return Some(indexed_block.block),
            Err(error) => {
                tracing::warn!(%block_hash, attempt, %error, "Could not load a block.");
                tokio::time::sleep(BLOCK_FETCH_RETRY_DELAY).await;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use bdk_wallet::bitcoin::block::{Header, Version as BlockVersion};
    use bdk_wallet::bitcoin::hashes::Hash as _;
    use bdk_wallet::bitcoin::{
        Amount, CompactTarget, Sequence, TxIn, TxMerkleNode, TxOut, Witness, absolute, transaction,
    };

    use super::*;

    fn script(byte: u8) -> ScriptBuf {
        ScriptBuf::from_bytes(vec![0x51, 0x20, byte])
    }

    fn tx(inputs: &[OutPoint], outputs: &[ScriptBuf]) -> Transaction {
        Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: inputs
                .iter()
                .map(|outpoint| TxIn {
                    previous_output: *outpoint,
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                })
                .collect(),
            output: outputs
                .iter()
                .map(|script_pubkey| TxOut {
                    value: Amount::from_sat(1_000),
                    script_pubkey: script_pubkey.clone(),
                })
                .collect(),
        }
    }

    fn block(nonce: u32, txdata: Vec<Transaction>) -> Block {
        Block {
            header: Header {
                version: BlockVersion::TWO,
                prev_blockhash: BlockHash::all_zeros(),
                merkle_root: TxMerkleNode::all_zeros(),
                time: 0,
                bits: CompactTarget::from_consensus(0),
                nonce,
            },
            txdata,
        }
    }

    fn other_outpoint(byte: u8) -> OutPoint {
        OutPoint::new(Txid::from_byte_array([byte; 32]), 0)
    }

    /// A transaction is relevant if it pays to a watched script, or if it spends a watched
    /// output. A spend is found also when the spending transaction is in the same block, and
    /// when the output was named by the caller and its transaction was never seen.
    #[test]
    fn finds_payments_and_spends() {
        let mut index = ChainIndex::default();
        index.scripts.insert(script(1));
        let named_output = other_outpoint(9);
        index.outpoints.insert(named_output);

        let unrelated = tx(&[other_outpoint(2)], &[script(7)]);
        let funding = tx(&[other_outpoint(3)], &[script(7), script(1)]);
        let funded_output = OutPoint::new(funding.compute_txid(), 1);
        let spend_in_same_block = tx(&[funded_output], &[script(8)]);
        let spend_of_other_output = tx(&[OutPoint::new(funding.compute_txid(), 0)], &[script(8)]);
        let first = block(
            1,
            vec![
                unrelated.clone(),
                funding.clone(),
                spend_in_same_block.clone(),
                spend_of_other_output,
            ],
        );
        let found = index.apply_block(10, &first);
        assert_eq!(
            found,
            vec![funding.compute_txid(), spend_in_same_block.compute_txid()]
        );
        assert_eq!(
            index.spends.get(&funded_output),
            Some(&spend_in_same_block.compute_txid())
        );
        assert!(!index.txs.contains_key(&unrelated.compute_txid()));
        // The same block again reports nothing new.
        assert_eq!(index.apply_block(10, &first), vec![]);

        let spend_of_named_output = tx(&[named_output], &[script(8)]);
        let second = block(2, vec![spend_of_named_output.clone()]);
        assert_eq!(
            index.apply_block(11, &second),
            vec![spend_of_named_output.compute_txid()]
        );
        assert_eq!(
            index.spends.get(&named_output),
            Some(&spend_of_named_output.compute_txid())
        );

        // Confirmations are counted from the tip.
        index.tip = Some((12, BlockHash::all_zeros()));
        assert_eq!(index.confirmations(&funding.compute_txid()), 3);
        assert_eq!(
            index.confirmations(&spend_of_named_output.compute_txid()),
            2
        );
        assert_eq!(index.confirmations(&unrelated.compute_txid()), 0);
    }

    /// A reorganisation removes what was found in the replaced blocks, and nothing else.
    #[test]
    fn reorganisation_forgets_the_replaced_blocks() {
        let mut index = ChainIndex::default();
        index.scripts.insert(script(1));
        let funding = tx(&[other_outpoint(3)], &[script(1)]);
        let funded_output = OutPoint::new(funding.compute_txid(), 0);
        let spend = tx(&[funded_output], &[script(8)]);
        index.apply_block(10, &block(1, vec![funding.clone()]));
        index.apply_block(11, &block(2, vec![spend.clone()]));
        index.tip = Some((11, BlockHash::all_zeros()));
        index.synced_height = Some(11);

        index.disconnect_from(11);
        assert_eq!(index.synced_height, Some(10));
        assert!(index.txs.contains_key(&funding.compute_txid()));
        assert!(!index.txs.contains_key(&spend.compute_txid()));
        assert_eq!(index.spends.get(&funded_output), None);

        // The spend is found again in a block of the new chain, at another height.
        index.tip = Some((12, BlockHash::all_zeros()));
        assert_eq!(
            index.apply_block(12, &block(3, vec![spend.clone()])),
            vec![spend.compute_txid()]
        );
        assert_eq!(index.confirmations(&spend.compute_txid()), 1);

        index.disconnect_from(0);
        assert!(index.txs.is_empty() && index.spends.is_empty());
        assert_eq!(index.synced_height, None);
    }
}
