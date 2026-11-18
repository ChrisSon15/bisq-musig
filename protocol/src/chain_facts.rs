//! What the trade engine is told about the chain, and what it reports back.
//!
//! The engine has no access to the chain. Its caller reads the facts about the outputs of a
//! trade from its chain access and passes them to [`TradeModel::observe_chain`]. The engine
//! derives the state of the trade and its reactions from the facts, so a fact that changes after
//! a reorganisation changes the result as well. The rules are specified in
//! `docs/specifications/trade/chain-observation.md`.
//!
//! [`TradeModel::observe_chain`]: crate::trade_model::TradeModel::observe_chain

use std::collections::BTreeMap;
use std::sync::Arc;

use bdk_wallet::bitcoin::{OutPoint, Transaction, Txid};

use crate::state::TradeState;

/// A confirmed transaction that spends a watched output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spend {
    pub tx: Arc<Transaction>,
    /// The number of confirmations of the transaction, at least 1.
    pub confirmations: u32,
}

/// The facts about the chain that concern one trade. The engine sees confirmed transactions only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChainFacts {
    /// The height of the last block of the chain with the most work.
    pub tip_height: u32,
    /// The number of confirmations of the deposit transaction, or 0.
    pub deposit_confirmations: u32,
    /// The confirmed transaction that spends an output, for the outputs that the trade watches
    /// ([`TradeModel::outputs_to_watch`](crate::trade_model::TradeModel::outputs_to_watch)).
    pub spends: BTreeMap<OutPoint, Spend>,
}

impl ChainFacts {
    /// Reads the facts for one trade from a chain access.
    ///
    /// - `confirmations` returns the number of confirmations of a transaction, or 0.
    /// - `spending_tx` returns the confirmed transaction that spends an output.
    pub fn collect(
        tip_height: u32,
        deposit_txid: &Txid,
        watched_outputs: impl IntoIterator<Item = OutPoint>,
        confirmations: impl Fn(&Txid) -> u32,
        spending_tx: impl Fn(&OutPoint) -> Option<Arc<Transaction>>,
    ) -> Self {
        let spends = watched_outputs
            .into_iter()
            .filter_map(|outpoint| {
                let tx = spending_tx(&outpoint)?;
                let confirmations = confirmations(&tx.compute_txid());
                (confirmations > 0).then_some((outpoint, Spend { tx, confirmations }))
            })
            .collect();
        Self {
            tip_height,
            deposit_confirmations: confirmations(deposit_txid),
            spends,
        }
    }
}

/// The transactions of a trade that a trader sends to the network.
#[expect(
    clippy::exhaustive_enums,
    reason = "the transactions of the protocol are a closed set"
)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TxKind {
    Deposit,
    DepositAbort,
    Swap,
    CustomPayout,
    Warning,
    Redirect,
    Claim,
    Penalty,
}

/// A transaction that the trader should have on the network at the moment. A broadcast has no
/// result, so the caller sends such a transaction again after every new block, until the engine
/// no longer names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxToSend {
    pub kind: TxKind,
    pub tx: Transaction,
}

/// What the engine reports to its caller after it has looked at the chain.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TradeEvent {
    /// The state of the trade changed. After a reorganisation the new state can be an earlier
    /// state of the trade.
    StateChanged { from: TradeState, to: TradeState },
    /// The peer took funds of the trader with a claim or a penalty transaction.
    Loss(String),
    /// Something happened on the chain that the engine does not expect from a peer with
    /// unmodified software. The engine changed no state for it.
    Unexpected(String),
}
