//! A long-running compact block filter client for the wallet and for the trade engine.
//!
//! The client follows the chain of block headers, tests the filter of every block for the watched
//! scripts, loads the blocks that match, and keeps the transactions that pay to a watched script
//! or spend a watched output. It sees confirmed transactions only. It sends transactions to the
//! peers and learns no result of that.
//!
//! The client connects only to the peers that it was given. When the connection to them is lost,
//! it keeps trying to connect again.
//!
//! The rules are specified in `docs/specifications/trade/chain-observation.md`.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bdk_kyoto::bip157::chain::{BlockHeaderChanges, ChainState};
use bdk_kyoto::bip157::{Builder, Event, Network, Package, ServiceFlags};
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
/// The pause before the client connects again after it lost all peers. It doubles with every
/// attempt in a row that does not reach the chain tip, up to [`MAX_RECONNECT_DELAY`].
const FIRST_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
/// A client that connects again starts this number of blocks below the height that it had
/// evaluated, so that it notices a reorganisation of the last blocks that happened in between.
const RECONNECT_DEPTH: u32 = 10;
/// How long the client waits for a peer to ask for a transaction that it announced.
const BROADCAST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a problem with the peers must last before the client reports it. The client has no
/// peer for a moment at every start and at every new connection.
const PEER_PROBLEM_GRACE: Duration = Duration::from_secs(5);
/// How often a caller that waits for the client looks at its state, also without an event.
const WAIT_TICK: Duration = Duration::from_millis(500);

/// A transaction that the client found in a block of the chain with the most work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmedTx {
    pub tx: Arc<Transaction>,
    pub height: u32,
    pub block_hash: BlockHash,
    /// The time in the header of the block, in seconds since the Unix epoch.
    pub block_time: u32,
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

/// A problem with the peers that the client reports while it keeps trying.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PeerProblem {
    /// No peer is connected.
    #[error("no peer of the Bitcoin network is reachable")]
    NoPeerReachable,
    /// A peer is connected, but it does not serve compact block filters.
    #[error("the connected peer does not serve compact block filters")]
    NoCompactFilters,
}

/// Why a request to the client failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
    #[error("the compact block filter client is not started yet")]
    NotStarted,
    #[error("the compact block filter client was shut down")]
    ShutDown,
    #[error("the compact block filter client has no connection at the moment")]
    NotConnected,
    #[error(transparent)]
    Peers(#[from] PeerProblem),
    /// The transaction was announced, but no peer asked for it. A peer does not ask for a
    /// transaction that it has already, or that it has rejected since the last block.
    #[error("no peer asked for the announced transaction within {} seconds", .0.as_secs())]
    NotRequested(Duration),
    #[error("the transactions are no package of one parent and one child: {0}")]
    InvalidPackage(String),
    #[error("the compact block filter client reported an error: {0}")]
    Node(String),
}

/// A request to evaluate earlier blocks again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rescan {
    /// The blocks above this height are evaluated again.
    from: u32,
    /// Whether the filters of the new evaluation have started to arrive.
    started: bool,
}

/// What the client knows about the chain: the block headers, the watched scripts and outputs, and
/// the transactions that it found. This part has no network access.
#[derive(Default)]
struct ChainIndex {
    /// The block after which the client follows the chain. `None`: the first block of the chain.
    base: Option<(u32, BlockHash)>,
    /// The block after which the running connection follows the chain.
    connection_start: u32,
    /// The blocks of the chain with the most work above the base, as far as they are known.
    headers: BTreeMap<u32, BlockHash>,
    /// The last block header of the chain with the most work.
    tip: Option<(u32, BlockHash)>,
    /// The height up to which all blocks are evaluated.
    synced_height: Option<u32>,
    /// Whether the running connection has reached the chain tip of its peers.
    reached_tip: bool,
    /// The watched scripts. The value is a height: the blocks above it are evaluated for the
    /// script.
    scripts: HashMap<ScriptBuf, u32>,
    /// Outputs that are watched although the client did not see the transaction that created
    /// them, with a height as for the scripts.
    outpoints: HashMap<OutPoint, u32>,
    txs: HashMap<Txid, ConfirmedTx>,
    /// The transaction that spends an output, for watched outputs.
    spends: HashMap<OutPoint, Txid>,
    /// The height of the last filter that was tested for the watched scripts.
    last_filter: Option<u32>,
    /// A new evaluation of earlier blocks that is not finished.
    rescan: Option<Rescan>,
}

impl ChainIndex {
    /// Whether `outpoint` is a watched output: named by the caller, or an output with a watched
    /// script of a transaction that the client found.
    fn is_watched_output(&self, outpoint: &OutPoint) -> bool {
        self.outpoints.contains_key(outpoint)
            || self.txs.get(&outpoint.txid).is_some_and(|found| {
                found
                    .tx
                    .output
                    .get(outpoint.vout as usize)
                    .is_some_and(|output| self.scripts.contains_key(&output.script_pubkey))
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
                .any(|output| self.scripts.contains_key(&output.script_pubkey));
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
                block_time: block.header.time,
            };
            if self.txs.insert(txid, confirmed.clone()) != Some(confirmed) {
                found.push(txid);
            }
        }
        found
    }

    /// Forgets everything that is known about the blocks at or above `height`.
    fn disconnect_from(&mut self, height: u32) {
        self.headers.split_off(&height);
        self.tip = self
            .headers
            .last_key_value()
            .map(|(height, hash)| (*height, *hash))
            .or(self.base);
        self.txs.retain(|_, found| found.height < height);
        let txs = &self.txs;
        self.spends.retain(|_, txid| txs.contains_key(txid));
        let below = height.checked_sub(1);
        self.synced_height = match self.synced_height {
            Some(synced) if synced >= height => below,
            other => other,
        };
        self.last_filter = match self.last_filter {
            Some(last) if last >= height => below,
            other => other,
        };
    }

    /// Records a block header of the chain with the most work. Returns the height from which
    /// the known blocks were replaced, if the header replaces a known block.
    ///
    /// A connection that starts below the known tip reports known headers again. A header that
    /// differs from the known one at its height shows a reorganisation that happened while the
    /// client was not connected.
    fn connect_header(&mut self, height: u32, block_hash: BlockHash) -> Option<u32> {
        let replaced = match self.headers.get(&height) {
            Some(known) if *known == block_hash => return None,
            Some(_) => {
                self.disconnect_from(height);
                Some(height)
            }
            None => None,
        };
        self.headers.insert(height, block_hash);
        if self.tip.is_none_or(|(tip_height, _)| height >= tip_height) {
            self.tip = Some((height, block_hash));
        }
        replaced
    }

    /// Records that the filter of the block at `height` is tested now.
    fn filter_arrived(&mut self, height: u32) {
        let continues = self.last_filter.is_some_and(|last| height == last + 1);
        // Filters arrive in the order of the chain. A filter that does not follow the last one
        // starts a new evaluation.
        if !continues
            && let Some(rescan) = &mut self.rescan
            && height <= rescan.from.saturating_add(1)
        {
            rescan.started = true;
        }
        self.last_filter = Some(height);
    }

    /// The height from which a script or output is evaluated that is watched from now on.
    fn evaluated_height(&self) -> u32 {
        self.last_filter
            .or(self.base.map(|(height, _)| height))
            .unwrap_or(0)
    }

    /// Watches scripts and outputs for all blocks above `from`. Returns `true` if blocks that
    /// were tested already must be evaluated again for that.
    fn watch_from(
        &mut self,
        scripts: impl IntoIterator<Item = ScriptBuf>,
        outpoints: impl IntoIterator<Item = OutPoint>,
        from: u32,
    ) -> bool {
        fn lower<K: std::hash::Hash + Eq>(map: &mut HashMap<K, u32>, key: K, from: u32) -> bool {
            match map.entry(key) {
                Entry::Vacant(entry) => {
                    entry.insert(from);
                    true
                }
                Entry::Occupied(mut entry) if *entry.get() > from => {
                    entry.insert(from);
                    true
                }
                Entry::Occupied(_) => false,
            }
        }
        let mut changed = false;
        for script in scripts {
            changed |= lower(&mut self.scripts, script, from);
        }
        for outpoint in outpoints {
            changed |= lower(&mut self.outpoints, outpoint, from);
        }
        changed && self.require_rescan(from)
    }

    /// Notes that the blocks above `from` must be evaluated again, if a block above `from` was
    /// tested already. Returns whether that is the case.
    fn require_rescan(&mut self, from: u32) -> bool {
        if self.last_filter.is_none_or(|last| from >= last) {
            // The filters of all blocks above `from` are still to come.
            return false;
        }
        let from = self.rescan.map_or(from, |rescan| rescan.from.min(from));
        self.rescan = Some(Rescan {
            from,
            started: false,
        });
        self.synced_height = self.synced_height.map(|synced| synced.min(from));
        true
    }

    /// The block after which a new connection follows the chain: some blocks below the evaluated
    /// height, and not below the base. An unfinished new evaluation of earlier blocks needs no
    /// extra rule, because the evaluated height is not above the height of such an evaluation.
    fn connection_start_point(&self) -> Option<(u32, BlockHash)> {
        let Some(synced_height) = self.synced_height else {
            return self.base;
        };
        let wanted = synced_height.saturating_sub(RECONNECT_DEPTH);
        let base_height = self.base.map_or(0, |(height, _)| height);
        if wanted <= base_height {
            return self.base;
        }
        self.headers
            .get(&wanted)
            .map(|hash| (wanted, *hash))
            .or(self.base)
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

    /// Whether all blocks up to the chain tip of the peers are evaluated for all watched scripts.
    fn is_synced(&self) -> bool {
        self.reached_tip
            && self.rescan.is_none()
            && self
                .tip
                .is_some_and(|(height, _)| self.synced_height == Some(height))
    }
}

/// The state of the chain at a moment at which the client had evaluated all blocks up to the
/// chain tip.
#[derive(Clone, Debug)]
pub struct ChainView {
    /// The block after which the client follows the chain. `None`: the first block of the chain.
    pub base: Option<(u32, BlockHash)>,
    /// The last block of the chain with the most work.
    pub tip: (u32, BlockHash),
    headers: BTreeMap<u32, BlockHash>,
    /// The relevant transactions that the client found, in the order of their blocks.
    pub txs: Vec<ConfirmedTx>,
}

impl ChainView {
    /// The block of the chain with the most work at `height`, if the client knows it: the base
    /// and the blocks above it.
    pub fn block_hash(&self, height: u32) -> Option<BlockHash> {
        self.headers.get(&height).copied().or_else(|| {
            self.base
                .filter(|(base_height, _)| *base_height == height)
                .map(|(_, hash)| hash)
        })
    }

    /// The height of the base, or 0.
    pub fn base_height(&self) -> u32 {
        self.base.map_or(0, |(height, _)| height)
    }

    /// The blocks above `height`, in the order of the chain.
    pub fn blocks_above(&self, height: u32) -> impl Iterator<Item = (u32, BlockHash)> + '_ {
        self.headers
            .range(height.saturating_add(1)..)
            .map(|(height, hash)| (*height, *hash))
    }
}

/// The future of a broadcast.
pub type BroadcastFuture = Pin<Box<dyn Future<Output = Result<Txid, ClientError>> + Send>>;

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

    /// Watches outputs with their scripts for all blocks above the block `start`, which the
    /// caller knows to be a block of the chain with the most work. Blocks that were evaluated
    /// already are evaluated again for these outputs, and the chain does not count as synced
    /// until that is done ([`Self::synced_height`]). A trade that was stored before a restart is
    /// watched in this way from the block at which its deposit transaction was signed.
    fn watch_outputs_from(&self, outputs: Vec<(OutPoint, ScriptBuf)>, start: (u32, BlockHash));

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

    /// A problem with the peers that lasts, if there is one. While it lasts, the chain is not
    /// observed and nothing can be sent.
    fn peer_problem(&self) -> Option<PeerProblem>;

    /// Subscribes to the events of the chain.
    fn subscribe(&self) -> broadcast::Receiver<ChainEvent>;

    /// Sends a transaction to the network. A success means that the transaction was sent, not
    /// that a node accepted it. The error [`ClientError::NotRequested`] means that the
    /// transaction was announced and no peer asked for it, which is the case for a transaction
    /// that the peer has already.
    fn broadcast(&self, tx: Transaction) -> BroadcastFuture;
}

struct Shared {
    network: Network,
    peers: Vec<TrustedPeer>,
    index: Mutex<ChainIndex>,
    /// The current problem with the peers, and since when it lasts.
    peer_problem: Mutex<Option<(PeerProblem, Instant)>>,
    events: broadcast::Sender<ChainEvent>,
    /// The handle of the running connection.
    requester: Mutex<Option<Requester>>,
    started: AtomicBool,
    shut_down: AtomicBool,
    /// Set when the connection is ended on purpose, to start a new one at once.
    reconnect_now: AtomicBool,
}

impl Shared {
    fn send(&self, event: ChainEvent) {
        // An error means that nobody is subscribed.
        let _ = self.events.send(event);
    }

    fn set_problem(&self, problem: PeerProblem) {
        let mut current = self.peer_problem.lock().unwrap();
        match *current {
            Some((known, _)) if known == problem => {}
            // A peer without compact block filters also drops the connection. The more specific
            // problem stays until the client has evaluated filters.
            Some((PeerProblem::NoCompactFilters, _)) => {}
            _ => *current = Some((problem, Instant::now())),
        }
    }

    /// Ends `problem`, if it is the current problem.
    fn clear_problem(&self, problem: PeerProblem) {
        let mut current = self.peer_problem.lock().unwrap();
        if current.is_some_and(|(known, _)| known == problem) {
            *current = None;
        }
    }

    fn clear_all_problems(&self) {
        *self.peer_problem.lock().unwrap() = None;
    }

    fn lasting_problem(&self) -> Option<PeerProblem> {
        self.peer_problem
            .lock()
            .unwrap()
            .filter(|(_, since)| since.elapsed() >= PEER_PROBLEM_GRACE)
            .map(|(problem, _)| problem)
    }

    /// Ends the running connection. The client connects again at once, at a start point that
    /// fits the current state of the index.
    fn reconnect(&self) {
        self.reconnect_now.store(true, Ordering::Release);
        if let Some(requester) = self.requester.lock().unwrap().as_ref() {
            let _ = requester.shutdown();
        }
    }

    /// Asks the running connection to evaluate the blocks above `from` again.
    fn request_rescan(&self, from: u32) {
        let connection_start = self.index.lock().unwrap().connection_start;
        if from < connection_start {
            // A connection cannot go below its start point.
            self.reconnect();
        } else if let Some(requester) = self.requester.lock().unwrap().as_ref() {
            // The connection ignores the request while it loads headers. The client then asks
            // again when the connection has reached the chain tip.
            let _ = requester.rescan_from(from);
        }
    }
}

/// The long-running compact block filter client. Cloning gives another handle to the same
/// client.
///
/// A client is created without network access, so that scripts can be watched and events can be
/// subscribed to at once. [`Self::start`] connects it to the peers.
#[derive(Clone)]
pub struct ChainClient {
    shared: Arc<Shared>,
}

impl ChainClient {
    /// Creates a client for `network` that will connect to `peers`, and to no other peer. It
    /// does nothing until [`Self::start`] is called.
    pub fn new(network: Network, peers: Vec<TrustedPeer>) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            shared: Arc::new(Shared {
                network,
                peers,
                index: Mutex::new(ChainIndex::default()),
                peer_problem: Mutex::new(None),
                events,
                requester: Mutex::new(None),
                started: AtomicBool::new(false),
                shut_down: AtomicBool::new(false),
                reconnect_now: AtomicBool::new(false),
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
        if self.shared.started.swap(true, Ordering::AcqRel) {
            return;
        }
        {
            let mut index = self.shared.index.lock().unwrap();
            index.base = start.map(|start| (start.height, start.hash));
        }
        tokio::spawn(run_connections(Arc::clone(&self.shared)));
    }

    /// Whether [`Self::start`] was called.
    pub fn is_started(&self) -> bool {
        self.shared.started.load(Ordering::Acquire)
    }

    /// The network of the client.
    pub fn network(&self) -> Network {
        self.shared.network
    }

    /// The block after which the client follows the chain. `None` before the start, and for a
    /// client that follows the chain from its first block.
    pub fn base(&self) -> Option<HashCheckpoint> {
        self.shared
            .index
            .lock()
            .unwrap()
            .base
            .map(|(height, hash)| HashCheckpoint::new(height, hash))
    }

    /// Allows the client to follow the chain from `base` on, if it follows the chain from a
    /// higher block so far. The caller must know that `base` is a block of the chain with the
    /// most work.
    ///
    /// The client evaluates the additional blocks when a script is watched from a height below
    /// its old base ([`Self::watch_from`]).
    pub fn lower_base(&self, base: HashCheckpoint) {
        let mut index = self.shared.index.lock().unwrap();
        if index.base.is_some_and(|(height, _)| height > base.height) {
            index.base = Some((base.height, base.hash));
        }
    }

    /// Watches a script: every transaction with an output to it, and every transaction that
    /// spends such an output of a transaction that the client found.
    ///
    /// Only blocks that are evaluated after this call are tested for the script. Use
    /// [`Self::watch_from`] to evaluate earlier blocks as well.
    pub fn watch_script(&self, script: ScriptBuf) {
        let mut index = self.shared.index.lock().unwrap();
        let from = index.evaluated_height();
        index.scripts.entry(script).or_insert(from);
    }

    /// Watches an output that is known to the caller, with its script. The client reports the
    /// transaction that spends it, also if it never saw the transaction that created it.
    pub fn watch_output(&self, outpoint: OutPoint, script: ScriptBuf) {
        let mut index = self.shared.index.lock().unwrap();
        let from = index.evaluated_height();
        index.outpoints.entry(outpoint).or_insert(from);
        index.scripts.entry(script).or_insert(from);
    }

    /// Watches scripts and outputs for all blocks above the height `from`, which must not lie
    /// below the base of the client. The script of every output must be among the scripts.
    ///
    /// Returns `true` if the client must evaluate earlier blocks again for that. It is then not
    /// synced until that is done.
    pub fn watch_from(
        &self,
        scripts: impl IntoIterator<Item = ScriptBuf>,
        outpoints: impl IntoIterator<Item = OutPoint>,
        from: u32,
    ) -> bool {
        let rescan = self
            .shared
            .index
            .lock()
            .unwrap()
            .watch_from(scripts, outpoints, from);
        if rescan {
            self.shared.request_rescan(from);
        }
        rescan
    }

    /// Evaluates the blocks above `height` again for all watched scripts and outputs, for
    /// example for a script that was watched after the client had passed these blocks. The
    /// client is not synced until that is done.
    pub fn rescan_from(&self, height: u32) -> Result<(), ChainApiError> {
        if !self.is_started() {
            return Err(ChainApiError::backend(ClientError::NotStarted));
        }
        let rescan = {
            let mut index = self.shared.index.lock().unwrap();
            for from in index.scripts.values_mut() {
                *from = (*from).min(height);
            }
            for from in index.outpoints.values_mut() {
                *from = (*from).min(height);
            }
            index.require_rescan(height)
        };
        if rescan {
            self.shared.request_rescan(height);
        }
        Ok(())
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

    /// A problem with the peers that has lasted for some seconds, if there is one. The client
    /// keeps trying.
    pub fn peer_problem(&self) -> Option<PeerProblem> {
        self.shared.lasting_problem()
    }

    /// Subscribes to the events of the client. A subscriber that falls too far behind loses
    /// events and should then read the state of the client again.
    pub fn subscribe(&self) -> broadcast::Receiver<ChainEvent> {
        self.shared.events.subscribe()
    }

    /// The state of the chain, if the client has evaluated all blocks up to the chain tip of its
    /// peers for all watched scripts.
    pub fn view(&self) -> Option<ChainView> {
        let index = self.shared.index.lock().unwrap();
        if !index.is_synced() {
            return None;
        }
        let mut txs = index.txs.values().cloned().collect::<Vec<_>>();
        txs.sort_by_key(|found| found.height);
        Some(ChainView {
            base: index.base,
            tip: index.tip?,
            headers: index.headers.clone(),
            txs,
        })
    }

    /// Waits until the client has evaluated all blocks up to the chain tip of its peers for all
    /// watched scripts, and returns the state of the chain at that moment.
    ///
    /// # Errors
    /// If the client is not started or was shut down, or if a problem with the peers lasts.
    pub async fn synced_view(&self) -> Result<ChainView, ClientError> {
        let mut events = self.subscribe();
        loop {
            if self.shared.shut_down.load(Ordering::Acquire) {
                return Err(ClientError::ShutDown);
            }
            if !self.is_started() {
                return Err(ClientError::NotStarted);
            }
            if let Some(view) = self.view() {
                return Ok(view);
            }
            if let Some(problem) = self.peer_problem() {
                return Err(problem.into());
            }
            // The state is read again after every event, and after a pause without an event.
            let _ = tokio::time::timeout(WAIT_TICK, events.recv()).await;
        }
    }

    /// Waits until the client has evaluated all blocks up to the height `height`.
    pub async fn wait_until_synced_to(&self, height: u32) -> Result<(), ChainApiError> {
        let mut events = self.subscribe();
        loop {
            if self.shared.shut_down.load(Ordering::Acquire) {
                return Err(ChainApiError::backend(ClientError::ShutDown));
            }
            if self.synced_height().is_some_and(|synced| synced >= height) {
                return Ok(());
            }
            let _ = tokio::time::timeout(WAIT_TICK, events.recv()).await;
        }
    }

    async fn submit(&self, package: Package) -> Result<(), ClientError> {
        if !self.is_started() {
            return Err(ClientError::NotStarted);
        }
        let requester = self
            .shared
            .requester
            .lock()
            .unwrap()
            .clone()
            .ok_or(ClientError::NotConnected)?;
        // The request ends when a peer has asked for the transaction. A peer that has the
        // transaction already never asks, so the wait has a limit.
        match tokio::time::timeout(BROADCAST_TIMEOUT, requester.submit_package(package)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(ClientError::Node(error.to_string())),
            Err(_) => Err(ClientError::NotRequested(BROADCAST_TIMEOUT)),
        }
    }

    /// Sends a transaction to the peers. This returns when the transaction was sent to at least
    /// one peer. It does not tell whether a node accepted the transaction.
    ///
    /// # Errors
    /// If the client has no connection, or if no peer asked for the transaction in time
    /// ([`ClientError::NotRequested`]).
    pub async fn broadcast(&self, tx: Transaction) -> Result<Txid, ClientError> {
        let txid = tx.compute_txid();
        self.submit(tx.into()).await?;
        Ok(txid)
    }

    /// Sends a parent transaction together with a child that spends one of its outputs, so that
    /// the fee of the child also pays for the parent. Returns the ID of the child.
    pub async fn broadcast_package(
        &self,
        parent: Transaction,
        child: Transaction,
    ) -> Result<Txid, ClientError> {
        let txid = child.compute_txid();
        let package = Package::new_one_parent_one_child(parent, child)
            .map_err(|error| ClientError::InvalidPackage(error.to_string()))?;
        self.submit(package).await?;
        Ok(txid)
    }

    /// Stops the client for good.
    pub fn shutdown(&self) -> Result<(), ChainApiError> {
        if !self.is_started() {
            return Err(ChainApiError::backend(ClientError::NotStarted));
        }
        self.shared.shut_down.store(true, Ordering::Release);
        if let Some(requester) = self.shared.requester.lock().unwrap().as_ref() {
            // An error means that the connection has ended already.
            let _ = requester.shutdown();
        }
        Ok(())
    }
}

impl ChainAccess for ChainClient {
    fn watch_script(&self, script: ScriptBuf) {
        Self::watch_script(self, script);
    }

    fn watch_output(&self, outpoint: OutPoint, script: ScriptBuf) {
        Self::watch_output(self, outpoint, script);
    }

    fn watch_outputs_from(&self, outputs: Vec<(OutPoint, ScriptBuf)>, start: (u32, BlockHash)) {
        let (height, hash) = start;
        // The client follows the chain from this block on, if it started at a later one.
        self.lower_base(HashCheckpoint::new(height, hash));
        let (outpoints, scripts): (Vec<_>, Vec<_>) = outputs.into_iter().unzip();
        self.watch_from(scripts, outpoints, height);
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

    fn peer_problem(&self) -> Option<PeerProblem> {
        Self::peer_problem(self)
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
        tokio::runtime::Handle::current()
            .block_on(self.broadcast(tx.clone()))
            .map_err(ChainApiError::backend)
    }
}

/// Keeps a connection to the peers for the lifetime of the client. The library ends a connection
/// when it has no peer left to try. The client then starts a new one after a pause.
async fn run_connections(shared: Arc<Shared>) {
    let mut delay = FIRST_RECONNECT_DELAY;
    while !shared.shut_down.load(Ordering::Acquire) {
        let start = {
            let mut index = shared.index.lock().unwrap();
            let start = index.connection_start_point();
            index.connection_start = start.map_or(0, |(height, _)| height);
            index.reached_tip = false;
            // The first filter of the new connection starts a new evaluation.
            index.last_filter = None;
            start
        };
        // Only the given peers: the library would otherwise look for other peers through DNS
        // when the given peers fail.
        // Blocks are loaded with their witness data: the trade engine reads a key share from the
        // signature of a transaction, and it tells a claim from a penalty by the witness.
        let mut builder = Builder::new(shared.network)
            .add_peers(shared.peers.iter().cloned())
            .whitelist_only()
            .fetch_witness_data();
        if let Some((height, hash)) = start {
            builder =
                builder.chain_state(ChainState::Checkpoint(HashCheckpoint::new(height, hash)));
        }
        let (node, client) = builder.build();
        *shared.requester.lock().unwrap() = Some(client.requester.clone());
        // A shutdown that came while no connection existed.
        if shared.shut_down.load(Ordering::Acquire) {
            break;
        }
        tracing::debug!(?start, "Connecting the compact block filter client.");
        let mut node_task = tokio::spawn(node.run());
        let peers_task = tokio::spawn(log_and_track_peers(
            client.info_rx,
            client.warn_rx,
            client.requester.clone(),
            Arc::clone(&shared),
        ));
        // The connection is over when the library has ended it. Its event channel can stay open
        // after that, so the end of the events is not the signal.
        let node_result = tokio::select! {
            result = &mut node_task => result,
            () = process_events(client.event_rx, &client.requester, &shared) => node_task.await,
        };
        *shared.requester.lock().unwrap() = None;
        let reached_tip = {
            let mut index = shared.index.lock().unwrap();
            std::mem::take(&mut index.reached_tip)
        };
        match node_result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(%error, "The compact block filter client lost its peers.");
                shared.set_problem(PeerProblem::NoPeerReachable);
            }
            Err(error) => tracing::error!(%error, "The compact block filter client failed."),
        }
        peers_task.abort();
        if shared.shut_down.load(Ordering::Acquire) {
            break;
        }
        if shared.reconnect_now.swap(false, Ordering::AcqRel) {
            continue;
        }
        if reached_tip {
            delay = FIRST_RECONNECT_DELAY;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(MAX_RECONNECT_DELAY);
    }
    *shared.requester.lock().unwrap() = None;
    tracing::info!("The compact block filter client was shut down.");
}

/// Writes the messages of a connection to the log and keeps the problem with the peers.
async fn log_and_track_peers(
    mut info_rx: Receiver<Info>,
    mut warn_rx: UnboundedReceiver<Warning>,
    requester: Requester,
    shared: Arc<Shared>,
) {
    loop {
        tokio::select! {
            info = info_rx.recv() => match info {
                Some(Info::ConnectionsMet | Info::SuccessfulHandshake) => {
                    shared.clear_problem(PeerProblem::NoPeerReachable);
                    // The library checks only later whether a peer serves compact block filters,
                    // and it does not report that for a peer that drops the connection first.
                    if let Ok(peers) = requester.peer_info().await
                        && !peers.is_empty()
                        && !peers
                            .iter()
                            .any(|(_, services)| services.has(ServiceFlags::COMPACT_FILTERS))
                    {
                        tracing::warn!("The connected peer does not serve compact block filters.");
                        shared.set_problem(PeerProblem::NoCompactFilters);
                    }
                }
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
                    // This is reported before every attempt to connect, so it is not logged.
                    shared.set_problem(PeerProblem::NoPeerReachable);
                }
                Some(Warning::NoCompactFilters) => {
                    tracing::warn!("The connected peer does not serve compact block filters.");
                    shared.set_problem(PeerProblem::NoCompactFilters);
                }
                Some(warning) => tracing::warn!("{warning}"),
                None => break,
            },
        }
    }
}

/// Follows the events of a connection until it ends: header changes, filters and the end of a
/// filter scan.
async fn process_events(
    mut event_rx: UnboundedReceiver<Event>,
    requester: &Requester,
    shared: &Shared,
) {
    // The blocks whose filter matched a watched script, with their height.
    let mut matched_blocks: Vec<(u32, BlockHash)> = Vec::new();
    while let Some(event) = event_rx.recv().await {
        match event {
            Event::ChainUpdate(BlockHeaderChanges::Connected(header)) => {
                let replaced_from = shared
                    .index
                    .lock()
                    .unwrap()
                    .connect_header(header.height, header.block_hash());
                if let Some(from_height) = replaced_from {
                    matched_blocks.retain(|(height, _)| *height < from_height);
                    tracing::warn!(
                        from_height,
                        "The chain was reorganised while the client was not connected."
                    );
                    shared.send(ChainEvent::Reorganized { from_height });
                }
            }
            Event::ChainUpdate(BlockHeaderChanges::Reorganized {
                mut accepted,
                reorganized,
            }) => {
                let Some(from_height) = reorganized.iter().map(|header| header.height).min() else {
                    continue;
                };
                accepted.sort_by_key(|header| header.height);
                {
                    let mut index = shared.index.lock().unwrap();
                    index.disconnect_from(from_height);
                    for header in &accepted {
                        index.connect_header(header.height, header.block_hash());
                    }
                }
                matched_blocks.retain(|(height, _)| *height < from_height);
                tracing::warn!(from_height, "The chain was reorganised.");
                shared.send(ChainEvent::Reorganized { from_height });
            }
            Event::ChainUpdate(BlockHeaderChanges::ForkAdded(_)) => {}
            Event::IndexedFilter(filter) => {
                let matches = {
                    let mut index = shared.index.lock().unwrap();
                    index.filter_arrived(filter.height());
                    // A filter test with no script reports a match, so it is skipped.
                    !index.scripts.is_empty() && filter.contains_any(index.scripts.keys())
                };
                if matches {
                    matched_blocks.push((filter.height(), filter.block_hash()));
                }
            }
            Event::FiltersSynced(update) => {
                let matched = std::mem::take(&mut matched_blocks);
                filters_synced(update.tip, matched, requester, shared).await;
            }
        }
    }
    tracing::debug!("A connection of the compact block filter client ended.");
}

/// Evaluates the blocks whose filters matched, when the connection has tested the filters of all
/// blocks up to the chain tip `tip`, and records up to which height the client is synced now.
async fn filters_synced(
    tip: HashCheckpoint,
    mut matched_blocks: Vec<(u32, BlockHash)>,
    requester: &Requester,
    shared: &Shared,
) {
    // The blocks are evaluated in the order of the chain, so that a transaction is known before
    // the transaction that spends it.
    matched_blocks.sort_unstable();
    matched_blocks.dedup();
    let mut lowest_missing = None;
    for (height, block_hash) in matched_blocks {
        let Some(block) = fetch_block(requester, block_hash).await else {
            lowest_missing.get_or_insert(height);
            continue;
        };
        let found = shared.index.lock().unwrap().apply_block(height, &block);
        for txid in found {
            shared.send(ChainEvent::Confirmed { txid, height });
        }
    }
    let (synced_height, rescan_from, replaced_from) = {
        let mut index = shared.index.lock().unwrap();
        let replaced_from = index.connect_header(tip.height, tip.hash);
        match index.rescan {
            // The filters of the new evaluation did not arrive: the connection ignored the
            // request, or it finished an earlier scan first. The request is sent again, unless
            // no block above its height exists any more.
            Some(rescan) if !rescan.started && rescan.from < tip.height => {}
            _ => index.rescan = None,
        }
        // A block that could not be loaded is evaluated again. The client does not report a
        // height as synced that lies above such a block.
        if let Some(lowest) = lowest_missing {
            let from = lowest.saturating_sub(1);
            index.rescan = Some(Rescan {
                from: index.rescan.map_or(from, |rescan| rescan.from.min(from)),
                started: false,
            });
        }
        let rescan_from = index.rescan.map(|rescan| rescan.from);
        let synced_height = rescan_from.map_or(tip.height, |from| from.min(tip.height));
        index.synced_height = Some(synced_height);
        index.reached_tip = true;
        (synced_height, rescan_from, replaced_from)
    };
    if let Some(from_height) = replaced_from {
        shared.send(ChainEvent::Reorganized { from_height });
    }
    shared.clear_all_problems();
    shared.send(ChainEvent::Synced {
        height: synced_height,
    });
    if let Some(from) = rescan_from {
        shared.request_rescan(from);
    }
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
        index.scripts.insert(script(1), 0);
        let named_output = other_outpoint(9);
        index.outpoints.insert(named_output, 0);

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
        index.scripts.insert(script(1), 0);
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

    fn hash(number: u32) -> BlockHash {
        let mut bytes = [0; 32];
        bytes[..4].copy_from_slice(&number.to_le_bytes());
        BlockHash::from_byte_array(bytes)
    }

    /// A connection that starts below the known tip reports known headers again. They change
    /// nothing. A header that differs from the known one shows a reorganisation that happened
    /// while the client was not connected: the client forgets the replaced blocks.
    #[test]
    fn known_headers_change_nothing_and_other_headers_replace_blocks() {
        let mut index = ChainIndex {
            base: Some((9, hash(9))),
            ..ChainIndex::default()
        };
        index.scripts.insert(script(1), 9);
        for height in 10..=12 {
            assert_eq!(index.connect_header(height, hash(height)), None);
        }
        let funding = tx(&[other_outpoint(3)], &[script(1)]);
        index.apply_block(11, &block(1, vec![funding.clone()]));
        index.synced_height = Some(12);
        index.last_filter = Some(12);
        assert_eq!(index.tip, Some((12, hash(12))));

        // The same headers again, from a new connection.
        for height in 10..=12 {
            assert_eq!(index.connect_header(height, hash(height)), None);
        }
        assert_eq!(index.tip, Some((12, hash(12))));
        assert_eq!(index.synced_height, Some(12));
        assert_eq!(index.confirmations(&funding.compute_txid()), 2);

        // Another block at height 11.
        assert_eq!(index.connect_header(11, hash(111)), Some(11));
        assert_eq!(index.tip, Some((11, hash(111))));
        assert_eq!(index.synced_height, Some(10));
        assert_eq!(index.last_filter, Some(10));
        assert_eq!(index.headers.get(&12), None);
        assert!(index.txs.is_empty());
    }

    /// A script that is watched from a height below the tested blocks needs a new evaluation,
    /// and the client is not synced until the filters of that evaluation have arrived.
    #[test]
    fn watching_from_an_earlier_height_needs_a_new_evaluation() {
        let mut index = ChainIndex {
            base: Some((9, hash(9))),
            reached_tip: true,
            ..ChainIndex::default()
        };
        for height in 10..=20 {
            index.connect_header(height, hash(height));
            index.filter_arrived(height);
        }
        index.synced_height = Some(20);
        assert!(index.is_synced());

        // From the tip on: the filters of all later blocks are still to come.
        assert!(!index.watch_from([script(1)], [], 20));
        assert!(index.is_synced());
        // The same script again from the same height or a higher one changes nothing.
        assert!(!index.watch_from([script(1)], [], 20));
        assert!(!index.watch_from([script(1)], [], 25));

        // From height 15: the blocks 16 to 20 were tested without the script.
        assert!(index.watch_from([script(2)], [other_outpoint(1)], 15));
        assert_eq!(
            index.rescan,
            Some(Rescan {
                from: 15,
                started: false
            })
        );
        assert_eq!(index.synced_height, Some(15));
        assert!(!index.is_synced());
        // A known script from a lower height extends the new evaluation.
        assert!(index.watch_from([script(1)], [], 12));
        assert_eq!(index.rescan.map(|rescan| rescan.from), Some(12));

        // A filter that continues the old scan does not start the new evaluation.
        index.connect_header(21, hash(21));
        index.filter_arrived(21);
        assert_eq!(index.rescan.map(|rescan| rescan.started), Some(false));
        // The first filter above height 12 does.
        index.filter_arrived(13);
        assert_eq!(index.rescan.map(|rescan| rescan.started), Some(true));
        assert_eq!(index.scripts.get(&script(1)), Some(&12));
        assert_eq!(index.scripts.get(&script(2)), Some(&15));
    }

    /// A new connection starts some blocks below the evaluated height, and never below the base.
    #[test]
    fn a_new_connection_starts_below_the_evaluated_height() {
        let mut index = ChainIndex {
            base: Some((100, hash(100))),
            ..ChainIndex::default()
        };
        // Nothing is evaluated yet.
        assert_eq!(index.connection_start_point(), Some((100, hash(100))));
        for height in 101..=130 {
            index.connect_header(height, hash(height));
            index.filter_arrived(height);
        }
        index.synced_height = Some(130);
        assert_eq!(index.connection_start_point(), Some((120, hash(120))));

        // A new evaluation from height 125 lowers the evaluated height to 125.
        assert!(index.watch_from([script(1)], [], 125));
        assert_eq!(index.connection_start_point(), Some((115, hash(115))));

        // Near the base, and with a base that was lowered to a block whose header is unknown.
        assert!(index.watch_from([script(2)], [], 105));
        assert_eq!(index.connection_start_point(), Some((100, hash(100))));
        index.base = Some((50, hash(50)));
        assert!(index.watch_from([script(3)], [], 70));
        assert_eq!(index.connection_start_point(), Some((50, hash(50))));

        // A client that follows the chain from its first block.
        let from_genesis = ChainIndex::default();
        assert_eq!(from_genesis.connection_start_point(), None);
    }
}
