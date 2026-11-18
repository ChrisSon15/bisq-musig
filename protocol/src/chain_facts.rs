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

/// A point in time of a trade at which a rule of the engine changes. Time is counted in blocks,
/// so a deadline is a height of the chain: the rule changes when the last block of the chain has
/// this height.
#[expect(
    clippy::exhaustive_enums,
    reason = "the deadlines follow from the rules of the protocol, a closed set"
)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeadlineKind {
    /// The deposit transaction is unconfirmed. At this height the engine sends the deposit abort
    /// transaction.
    DepositTimeout,
    /// From this height on a warning transaction is valid: the request to publish the warning
    /// is accepted, and a warning transaction of either trader can be in the next block.
    WarningLockTime,
    /// The last height at which the seller's release is accepted. Later, the time before the
    /// buyer's warning becomes valid is shorter than the release safety margin.
    LastRelease,
    /// The seller has released, and the buyer has not answered. At this height the seller's
    /// engine publishes the swap transaction on its own.
    ForcedClose,
    /// A warning transaction is confirmed and its escrow is unspent. From this height on the
    /// claim of the trader who published the warning is valid, and his engine sends it. The
    /// redirect or the penalty transaction of the other trader must be confirmed at this height
    /// at the latest.
    ClaimLockTime,
}

/// A deadline of a trade that is in force at the moment.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Deadline {
    pub kind: DeadlineKind,
    pub height: u32,
}

/// Something that the engine cannot do on its own, and that the user must do so that the trade
/// ends or his funds are safe.
#[expect(
    clippy::exhaustive_enums,
    reason = "the actions follow from the rules of the protocol, a closed set"
)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum UserAction {
    /// The trade is terminated, so no step with the peer is possible any more. The trader gets
    /// his funds through the dispute path only: he must publish his warning transaction when the
    /// warning lock time has passed, unless the peer publishes his own first.
    PublishWarning,
    /// The seller's release is not accepted any more (the deadline `LastRelease` has passed),
    /// so the trade cannot end with a release. The traders must agree on a custom payout, or a
    /// trader must publish his warning transaction when the warning lock time has passed.
    StartDispute,
    /// The trade is closed cooperatively and both payouts are unspent. The peer can still
    /// publish his warning transaction, and the engine answers it only while the program runs.
    /// The user must spend his payout, or end the program in the regular way, which moves the
    /// payout to the seed wallet.
    SpendOrDrainPayout,
}

/// The parts of a trade that its caller shows to the user: every event of a trade reports them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TradeStatus {
    pub state: TradeState,
    /// Whether nothing can change for the trade any more.
    pub is_final: bool,
    /// The reason, if the trade was terminated because the peer did not follow the protocol.
    pub termination_reason: Option<String>,
    pub deadlines: Vec<Deadline>,
    pub user_actions: Vec<UserAction>,
}

/// A confirmed transaction of a trade.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ConfirmedTradeTx {
    /// [`TxKind::DepositAbort`] stands for every transaction that spent an input of the deposit
    /// transaction in its place, whoever sent it.
    pub kind: TxKind,
    pub txid: Txid,
    pub confirmations: u32,
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
    /// The number of confirmations of a transaction of the trade changed. It is reported from
    /// the first confirmation until the transaction counts as confirmed, and with 0 when a
    /// reorganisation removed the transaction.
    TxConfirmations(ConfirmedTradeTx),
    /// A deadline, a required action of the user or the final mark of the trade changed, and the
    /// state did not. [`TradeModel::status`](crate::trade_model::TradeModel::status) has the new
    /// values.
    StatusChanged,
}
