use std::sync::Arc;

use bdk_wallet::chain::keychain_txout::KeychainTxOutIndex;
use bdk_wallet::chain::{BlockId, ConfirmationBlockTime, IndexedTxGraph, TxUpdate};
use bdk_wallet::{KeychainKind, PersistedWallet, Update, Wallet};
use chain::client::{ChainClient, ChainView, HashCheckpoint};

use crate::error::ChainDataSourceError;
use crate::persisted::BMPWalletPersister;

#[trait_variant::make(Send)]
pub trait ChainDataSource {
    async fn sync(
        &self,
        _persister: Vec<&mut PersistedWallet<impl BMPWalletPersister>>,
    ) -> Result<(), ChainDataSourceError>;
}

/// A client that is started by a wallet sync begins this number of known blocks below the chain
/// tip of the wallet, so that it notices a reorganisation of the last blocks of the wallet.
const START_DEPTH: usize = 10;

/// The wallet sync through the long-running compact block filter client, which the trade engine
/// uses as well. The first sync starts the client.
///
/// A sync registers the scripts and the outputs of the wallets with the client, waits until the
/// client has evaluated all blocks up to the chain tip for them, and applies the blocks and the
/// transactions that the client found to the wallets.
impl ChainDataSource for ChainClient {
    async fn sync(
        &self,
        mut wallets: Vec<&mut PersistedWallet<impl BMPWalletPersister>>,
    ) -> Result<(), ChainDataSourceError> {
        let Some(first) = wallets.first() else {
            return Ok(());
        };
        if first.network() != self.network() {
            return Err(ChainDataSourceError::NetworkMismatch {
                wallet: first.network(),
                peers: self.network(),
            });
        }
        if !self.is_started() {
            // The scripts are watched before the client tests its first filter, so that the
            // client needs to load the filters only once.
            let starts = wallets
                .iter()
                .map(|wallet| start_point(wallet))
                .collect::<Vec<_>>();
            for (wallet, start) in wallets.iter().zip(&starts) {
                watch_wallet(self, wallet, start.height);
            }
            let start = starts.into_iter().min_by_key(|start| start.height);
            self.start(
                start
                    .filter(|start| start.height > 0)
                    .map(|start| HashCheckpoint::new(start.height, start.hash)),
            );
        }

        // The block of each wallet above which the wallet needs the chain: the highest block of
        // the wallet that is part of the chain with the most work. It is found in the first
        // round and does not rise in later rounds, in which the wallets know more blocks.
        let mut floors: Vec<Option<BlockId>> = vec![None; wallets.len()];
        loop {
            let view = self.synced_view().await?;
            let mut evaluating = false;
            for (wallet, floor) in wallets.iter().zip(&mut floors) {
                let agreement = agreement_point(wallet, &view);
                let floor = *floor.insert(match *floor {
                    Some(floor) if floor.height <= agreement.height => floor,
                    _ => agreement,
                });
                if floor.height < view.base_height() {
                    // For example a key that was imported with an early start height.
                    self.lower_base(HashCheckpoint::new(floor.height, floor.hash));
                }
                evaluating |= watch_wallet(self, wallet, floor.height);
            }
            if evaluating {
                // The client evaluates earlier blocks again for scripts that are new to it.
                continue;
            }

            let mut more_scripts = false;
            for (wallet, floor) in wallets.iter_mut().zip(&floors) {
                let floor = floor.expect("set above for every wallet");
                let update = wallet_update(wallet, &view, floor.height);
                wallet.apply_update(update)?;
                // A payment to one of the last scripts of a wallet makes the wallet derive
                // further scripts. They can have payments in the blocks that are evaluated
                // already.
                more_scripts |= watch_wallet(self, wallet, floor.height);
            }
            if !more_scripts {
                return Ok(());
            }
        }
    }
}

/// The block at which a client should start for this wallet: [`START_DEPTH`] known blocks below
/// the chain tip of the wallet, or the lowest block that the wallet knows.
fn start_point(wallet: &Wallet) -> BlockId {
    wallet
        .checkpoints()
        .take(START_DEPTH + 1)
        .last()
        .map_or_else(
            || wallet.latest_checkpoint().block_id(),
            |checkpoint| checkpoint.block_id(),
        )
}

/// The highest block of the wallet that is part of the chain of `view`.
///
/// A block of the wallet at or below the base of the view cannot be compared. It counts as part
/// of the chain, as the start point of the client did.
fn agreement_point(wallet: &Wallet, view: &ChainView) -> BlockId {
    let mut lowest = wallet.latest_checkpoint().block_id();
    for checkpoint in wallet.checkpoints() {
        lowest = checkpoint.block_id();
        if lowest.height > view.tip.0 {
            continue;
        }
        match view.block_hash(lowest.height) {
            Some(hash) if hash == lowest.hash => return lowest,
            Some(_) => {}
            None => return lowest,
        }
    }
    // Not even the first block of the chain agrees: the wallet is given the chain of the view
    // above its lowest block.
    lowest
}

/// Watches all scripts that the wallet can recognise, and all outputs that it knows, for the
/// blocks above `from`. Returns `true` if the client must evaluate earlier blocks again for
/// that.
fn watch_wallet(client: &ChainClient, wallet: &Wallet, from: u32) -> bool {
    let index: &KeychainTxOutIndex<KeychainKind> = wallet.spk_index();
    client.watch_from(
        index.inner().all_spks().values().cloned(),
        index.outpoints().iter().map(|(_, outpoint)| *outpoint),
        from,
    )
}

/// The update for a wallet: the blocks of the view above the highest block of the wallet that is
/// part of the chain, and the transactions above the height `floor` that are relevant for the
/// wallet.
fn wallet_update(wallet: &Wallet, view: &ChainView, floor: u32) -> Update {
    let mut graph = IndexedTxGraph::<ConfirmationBlockTime, _>::new(wallet.spk_index().clone());
    let _ = graph.batch_insert_relevant(view.txs.iter().filter(|found| found.height > floor).map(
        |found| {
            let anchor = ConfirmationBlockTime {
                block_id: BlockId {
                    height: found.height,
                    hash: found.block_hash,
                },
                confirmation_time: u64::from(found.block_time),
            };
            (Arc::clone(&found.tx), [anchor])
        },
    ));

    // A block of the view replaces a different block of the wallet at its height, together with
    // all blocks of the wallet above it.
    let agreement = agreement_point(wallet, view);
    let mut chain = wallet.latest_checkpoint();
    for (height, hash) in view.blocks_above(agreement.height) {
        chain = chain.insert(BlockId { height, hash });
    }

    Update {
        last_active_indices: graph.index.last_used_indices(),
        tx_update: TxUpdate::from(graph.graph().clone()),
        chain: Some(chain),
    }
}
