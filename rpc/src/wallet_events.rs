//! The events of the wallet for the caller of the wallet service: what changed in the wallet
//! since the caller looked at it last, so that the caller does not have to ask at intervals.
//!
//! An event is a hint. The state of the wallet is what the other requests of the wallet service
//! return. An event with the whole state opens every stream, and follows when a stream has lost
//! events.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bdk_wallet::Balance;
use bdk_wallet::bitcoin::Txid;
use futures_util::stream::{self, BoxStream, StreamExt as _};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use wallet::bmp_wallet::BMPWallet;

/// One change of the wallet. `height` is the height of the last block that the wallet has seen,
/// after the change.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WalletChange {
    /// The state of the wallet, as the first event of a stream, when the wallet is opened, and
    /// after a stream has lost events. The caller reads the transactions and the outputs of the
    /// wallet again.
    State {
        height: u32,
        available: Balance,
        locked: Balance,
    },
    /// The wallet has seen a new last block of the chain.
    Tip { height: u32 },
    /// The balance changed: the part that a new transaction can spend, and the part of the
    /// outputs that are reserved.
    Balance {
        height: u32,
        available: Balance,
        locked: Balance,
    },
    /// A transaction of the wallet is new, or the block that contains it changed.
    /// `block_height` is `None` while the transaction is unconfirmed.
    Transaction {
        height: u32,
        txid: Txid,
        block_height: Option<u32>,
    },
    /// A transaction is no longer valid for the wallet: another transaction spent its coins, or
    /// it was forgotten.
    TransactionRemoved { height: u32, txid: Txid },
}

/// What the events are computed from: the state of the wallet at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WalletSnapshot {
    height: u32,
    available: Balance,
    locked: Balance,
    /// The valid transactions of the wallet with the heights of their blocks.
    transactions: HashMap<Txid, Option<u32>>,
}

impl WalletSnapshot {
    pub(crate) fn of(wallet: &BMPWallet) -> Self {
        Self {
            height: wallet.latest_checkpoint().height(),
            available: wallet.available_balance(),
            locked: wallet.locked_balance(),
            transactions: wallet.transaction_block_heights(),
        }
    }

    fn state(&self) -> WalletChange {
        WalletChange::State {
            height: self.height,
            available: self.available.clone(),
            locked: self.locked.clone(),
        }
    }

    /// The events that lead from the state `earlier` to this state.
    fn changes_since(&self, earlier: &Self) -> Vec<WalletChange> {
        let height = self.height;
        let mut changes = Vec::new();
        if height != earlier.height {
            changes.push(WalletChange::Tip { height });
        }
        let mut transactions = self
            .transactions
            .iter()
            .filter(|(txid, block_height)| earlier.transactions.get(*txid) != Some(*block_height))
            .map(|(txid, block_height)| WalletChange::Transaction {
                height,
                txid: *txid,
                block_height: *block_height,
            })
            .collect::<Vec<_>>();
        transactions.extend(
            earlier
                .transactions
                .keys()
                .filter(|txid| !self.transactions.contains_key(*txid))
                .map(|txid| WalletChange::TransactionRemoved {
                    height,
                    txid: *txid,
                }),
        );
        // A fixed order, so that two subscribers and two runs see the same sequence.
        transactions.sort_by_key(|change| match change {
            WalletChange::Transaction { txid, .. }
            | WalletChange::TransactionRemoved { txid, .. } => *txid,
            _ => unreachable!("only events of transactions are collected here"),
        });
        changes.extend(transactions);
        if (&self.available, &self.locked) != (&earlier.available, &earlier.locked) {
            changes.push(WalletChange::Balance {
                height,
                available: self.available.clone(),
                locked: self.locked.clone(),
            });
        }
        changes
    }
}

/// The number of events that a slow subscriber can fall behind before it loses events.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Publishes the changes of a wallet to the subscribers of its events.
pub(crate) struct WalletEvents {
    /// The state that the last published event led to. `None` while no wallet is open.
    snapshot: Arc<Mutex<Option<WalletSnapshot>>>,
    events: broadcast::Sender<WalletChange>,
}

impl Default for WalletEvents {
    fn default() -> Self {
        Self {
            snapshot: Arc::default(),
            events: broadcast::channel(EVENT_CHANNEL_CAPACITY).0,
        }
    }
}

impl WalletEvents {
    /// Compares the wallet with the state that was published last, and publishes what changed.
    /// The caller holds the lock of the wallet, so the events of two changes keep their order.
    pub(crate) fn publish_changes(&self, wallet: &BMPWallet) {
        let snapshot = WalletSnapshot::of(wallet);
        let mut published = self.snapshot.lock().unwrap();
        let changes = match &*published {
            Some(earlier) => snapshot.changes_since(earlier),
            // The wallet was opened.
            None => vec![snapshot.state()],
        };
        *published = Some(snapshot);
        for change in changes {
            // An error means that nobody is subscribed.
            let _ = self.events.send(change);
        }
    }

    /// The events of the wallet from now on. The stream starts with the state of the wallet, if
    /// a wallet is open; otherwise the state follows when the wallet is opened.
    pub(crate) fn subscribe(&self) -> BoxStream<'static, WalletChange> {
        // Subscribed before the state is read, so that no change is lost between the two.
        let events = self.events.subscribe();
        let first = self
            .snapshot
            .lock()
            .unwrap()
            .as_ref()
            .map(WalletSnapshot::state);
        let snapshot = Arc::clone(&self.snapshot);
        let later = stream::unfold((events, snapshot), |(mut events, snapshot)| async move {
            loop {
                match events.recv().await {
                    Ok(change) => return Some((change, (events, snapshot))),
                    // The subscriber lost events. It gets the state in their place.
                    Err(RecvError::Lagged(_)) => {
                        let state = snapshot.lock().unwrap().as_ref().map(WalletSnapshot::state);
                        if let Some(state) = state {
                            return Some((state, (events, snapshot)));
                        }
                    }
                    Err(RecvError::Closed) => return None,
                }
            }
        });
        stream::iter(first).chain(later).boxed()
    }
}

#[cfg(test)]
mod tests {
    use bdk_wallet::bitcoin::Amount;
    use bdk_wallet::bitcoin::hashes::Hash as _;

    use super::*;

    fn balance(confirmed: u64) -> Balance {
        Balance {
            confirmed: Amount::from_sat(confirmed),
            ..Balance::default()
        }
    }

    fn txid(byte: u8) -> Txid {
        Txid::from_byte_array([byte; 32])
    }

    fn snapshot(height: u32, confirmed: u64, txs: &[(u8, Option<u32>)]) -> WalletSnapshot {
        WalletSnapshot {
            height,
            available: balance(confirmed),
            locked: balance(0),
            transactions: txs
                .iter()
                .map(|(byte, block_height)| (txid(*byte), *block_height))
                .collect(),
        }
    }

    #[test]
    fn no_change_gives_no_event() {
        let state = snapshot(10, 5_000, &[(1, Some(9)), (2, None)]);
        assert_eq!(state.changes_since(&state), vec![]);
    }

    /// A new block that confirms one transaction, a new unconfirmed transaction, a transaction
    /// that is no longer valid, and the balance that follows: one event for each change, the
    /// tip first, the transactions in the order of their IDs, the balance last.
    #[test]
    fn every_change_gives_one_event() {
        let earlier = snapshot(10, 5_000, &[(1, Some(9)), (2, None), (3, None)]);
        let later = snapshot(11, 7_000, &[(1, Some(9)), (2, Some(11)), (4, None)]);
        assert_eq!(
            later.changes_since(&earlier),
            vec![
                WalletChange::Tip { height: 11 },
                WalletChange::Transaction {
                    height: 11,
                    txid: txid(2),
                    block_height: Some(11),
                },
                WalletChange::TransactionRemoved {
                    height: 11,
                    txid: txid(3),
                },
                WalletChange::Transaction {
                    height: 11,
                    txid: txid(4),
                    block_height: None,
                },
                WalletChange::Balance {
                    height: 11,
                    available: balance(7_000),
                    locked: balance(0),
                },
            ]
        );
    }

    /// A reorganisation moves a transaction to another block, or back to unconfirmed.
    #[test]
    fn a_changed_block_of_a_transaction_is_an_event() {
        let earlier = snapshot(11, 5_000, &[(1, Some(10)), (2, Some(11))]);
        let later = snapshot(11, 5_000, &[(1, Some(11)), (2, None)]);
        assert_eq!(
            later.changes_since(&earlier),
            vec![
                WalletChange::Transaction {
                    height: 11,
                    txid: txid(1),
                    block_height: Some(11),
                },
                WalletChange::Transaction {
                    height: 11,
                    txid: txid(2),
                    block_height: None,
                },
            ]
        );
    }
}
